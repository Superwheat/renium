//! `rbx mcp`: Renium as one MCP tool over stdio. An agent passes `rbx`
//! arguments as a list, so no shell, quoting or shell choice stands between
//! it and the command; the reply is the command's own output.
use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::system::LockRecover;

pub(crate) mod register;

/// The newest first; a client asking for one of these gets it back.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// One command may run this long; then it is stopped and the call fails.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// A process the command started (the daemon, Studio) can inherit its output
/// pipes and keep them open; after the command exits its output is read for
/// this long at most.
const OUTPUT_DRAIN: Duration = Duration::from_secs(1);
const INVALID_PARAMS: i64 = -32602;

#[derive(Args)]
pub(crate) struct McpArgs {
    #[arg(
        long,
        conflicts_with = "uninstall",
        help = "Register this rbx as the `renium` MCP server with the agents installed here"
    )]
    pub(crate) install: bool,
    #[arg(long, help = "Remove the `renium` MCP server from those agents")]
    pub(crate) uninstall: bool,
}

pub(crate) fn run(args: McpArgs) -> Result<()> {
    if args.install || args.uninstall {
        return register::command(args.uninstall);
    }
    let server = Server::new(
        std::env::current_exe().context("Cannot locate the Renium executable")?,
        std::env::current_dir().context("Cannot read the working directory")?,
    )?;
    if io::stdin().is_terminal() {
        eprintln!(
            "rbx mcp speaks MCP on stdin and stdout for an agent; `rbx mcp --install` registers it"
        );
    }
    #[cfg(windows)]
    crate::studio::audio::keep_standard_handles_private();
    server.serve(io::stdin().lock())
}

fn tool() -> Value {
    let arguments = json!({"type": "array", "items": {"type": "string"}});
    json!({
        "name": "rbx",
        "description": "Renium's `rbx` command for Roblox Studio. Pass the arguments as a list; read RENIUM.md in the project for the commands.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Arguments after rbx, e.g. [\"bs\",\"Workspace\",\"Lobby\",\"-p\",\"Anchored=true\",\"--push\"]",
                },
                "steps": {
                    "type": "array",
                    "items": arguments,
                    "description": "Instead of args: argument lists run in order, stopping at the first failure",
                },
                "stdin": {
                    "type": "string",
                    "description": "Text fed to each command's stdin, for --batch - and -j -",
                },
                "cwd": {
                    "type": "string",
                    "description": "Project folder; default: the server's working directory",
                },
            },
        },
    })
}

struct Server {
    program: PathBuf,
    base: PathBuf,
    calls: Mutex<HashMap<String, Arc<AtomicBool>>>,
    #[cfg(windows)]
    job: CommandJob,
}

/// Where the answer to a message goes: to stdout as a line of its own, or
/// into the one array that answers a JSON-RPC batch.
#[derive(Clone)]
enum Outlet {
    Single,
    Batch(Arc<Batch>),
}

impl Outlet {
    fn send(&self, message: Value) {
        match self {
            Self::Single => send(&message),
            Self::Batch(batch) => batch.answers.lock_recover().push(message),
        }
    }
}

/// The answers to one batch. Every message of the batch holds on to it until
/// it is answered, so the array goes out when the last of them lets go: after
/// the slowest tool call, and not at all for a batch of notifications.
#[derive(Default)]
struct Batch {
    answers: Mutex<Vec<Value>>,
}

impl Drop for Batch {
    fn drop(&mut self) {
        let answers = std::mem::take(&mut *self.answers.lock_recover());
        if !answers.is_empty() {
            send(&Value::Array(answers));
        }
    }
}

/// Ends the running commands with the server when the server is killed and
/// cannot stop them itself: each command joins a job object that only this
/// process holds, and Windows terminates what is in such a job when its last
/// handle closes.
#[cfg(windows)]
struct CommandJob(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a job handle may be used from any thread.
#[cfg(windows)]
unsafe impl Send for CommandJob {}
// SAFETY: as above; the calls made on it take no exclusive access.
#[cfg(windows)]
unsafe impl Sync for CommandJob {}

#[cfg(windows)]
impl CommandJob {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };

        // SAFETY: both arguments are optional; null asks for an unnamed job
        // with default security.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(job);
        // SAFETY: the structure is plain data for which all zeroes means no limits.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // Only the command itself stays in the job: the daemon or the Studio
        // it starts must outlive the server.
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        // SAFETY: the pointer and the size describe `limits`, which outlives the call.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of_val(&limits) as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// A command that has already ended cannot join, and needs no one to end it.
    fn adopt(&self, child: &Child) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: both handles are open for the duration of the call.
        unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) };
    }
}

#[cfg(windows)]
impl Drop for CommandJob {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[derive(Deserialize)]
struct CallArguments {
    args: Option<Vec<String>>,
    steps: Option<Vec<Vec<String>>>,
    stdin: Option<String>,
    cwd: Option<PathBuf>,
}

struct Step {
    text: String,
    failed: bool,
}

enum Ending {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    Lost(io::Error),
}

impl Server {
    fn new(program: PathBuf, base: PathBuf) -> Result<Self> {
        Ok(Self {
            program,
            base,
            calls: Mutex::new(HashMap::new()),
            #[cfg(windows)]
            job: CommandJob::new().context("Could not create the job for the commands")?,
        })
    }

    fn serve(self, mut input: impl BufRead) -> Result<()> {
        let server = Arc::new(self);
        let mut calls: Vec<JoinHandle<()>> = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if input
                .read_until(b'\n', &mut line)
                .context("Could not read stdin")?
                == 0
            {
                break;
            }
            calls.retain(|call| !call.is_finished());
            calls.extend(server.receive(line.trim_ascii()));
        }
        // The end of stdin is the client leaving: nobody reads an answer any
        // more, so the running commands are stopped instead of waited for.
        for cancelled in server.calls.lock_recover().values() {
            cancelled.store(true, Ordering::Relaxed);
        }
        for call in calls {
            let _ = call.join();
        }
        Ok(())
    }

    /// Answers one line: a message, or a batch of them, which protocol
    /// 2025-03-26 allows. Tool calls run on their own threads, which are
    /// returned, so a slow command never holds up pings or other calls.
    fn receive(self: &Arc<Self>, line: &[u8]) -> Vec<JoinHandle<()>> {
        if line.is_empty() {
            return Vec::new();
        }
        match serde_json::from_slice(line) {
            Ok(Value::Array(messages)) if !messages.is_empty() => {
                let outlet = Outlet::Batch(Arc::default());
                messages
                    .iter()
                    .filter_map(|message| self.answer(message, outlet.clone()))
                    .collect()
            }
            Ok(message) => self.answer(&message, Outlet::Single).into_iter().collect(),
            Err(error) => {
                send(&reply(
                    &Value::Null,
                    Err((-32700, format!("Parse error: {error}"))),
                ));
                Vec::new()
            }
        }
    }

    fn answer(self: &Arc<Self>, message: &Value, outlet: Outlet) -> Option<JoinHandle<()>> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            if message.get("result").is_none() && message.get("error").is_none() {
                let id = message.get("id").unwrap_or(&Value::Null);
                outlet.send(reply(id, Err((-32600, "Invalid request".to_string()))));
            }
            return None;
        };
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(id) = message.get("id").cloned() else {
            self.notification(method, &params);
            return None;
        };
        let result = match method {
            "initialize" => Ok(initialize_result(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": [tool()]})),
            "tools/call" => return Some(self.spawn_call(id, params, outlet)),
            _ => Err((-32601, format!("Method not found: {method}"))),
        };
        outlet.send(reply(&id, result));
        None
    }

    fn notification(&self, method: &str, params: &Value) {
        if method == "notifications/cancelled"
            && let Some(id) = params.get("requestId")
            && let Some(cancelled) = self.calls.lock_recover().get(&id.to_string())
        {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn spawn_call(self: &Arc<Self>, id: Value, params: Value, outlet: Outlet) -> JoinHandle<()> {
        let key = id.to_string();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.calls
            .lock_recover()
            .insert(key.clone(), Arc::clone(&cancelled));
        let server = Arc::clone(self);
        thread::spawn(move || {
            let result = server.call(&params, &cancelled);
            server.calls.lock_recover().remove(&key);
            if !cancelled.load(Ordering::Relaxed) {
                outlet.send(reply(&id, result));
            }
        })
    }

    fn call(&self, params: &Value, cancelled: &AtomicBool) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        if name != "rbx" {
            return Err((INVALID_PARAMS, format!("Unknown tool: {name}")));
        }
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let arguments: CallArguments = serde_json::from_value(arguments)
            .map_err(|error| (INVALID_PARAMS, format!("Invalid arguments: {error}")))?;
        let steps = match (arguments.args, arguments.steps) {
            (Some(args), None) => vec![args],
            (None, Some(steps)) if !steps.is_empty() => steps,
            _ => {
                return Err((
                    INVALID_PARAMS,
                    "Pass exactly one of args (a list of strings) or steps (a list of such lists)"
                        .to_string(),
                ));
            }
        };
        let cwd = match arguments.cwd {
            Some(folder) => self.base.join(folder),
            None => self.base.clone(),
        };
        if !cwd.is_dir() {
            return Ok(tool_result(
                vec![format!("cwd {} is not a folder", cwd.display())],
                true,
            ));
        }
        let mut texts = Vec::new();
        for args in &steps {
            let step = self.run_step(args, arguments.stdin.as_deref(), &cwd, cancelled);
            texts.push(step.text);
            if step.failed {
                return Ok(tool_result(texts, true));
            }
        }
        Ok(tool_result(texts, false))
    }

    fn run_step(
        &self,
        args: &[String],
        input: Option<&str>,
        cwd: &Path,
        cancelled: &AtomicBool,
    ) -> Step {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(cwd)
            .env("RENIUM_AGENT_CLI", "1")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Step {
                    text: format!("Could not start {}: {error}", self.program.display()),
                    failed: true,
                };
            }
        };
        #[cfg(windows)]
        self.job.adopt(&child);
        if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
            let input = input.to_owned();
            thread::spawn(move || {
                let _ = pipe.write_all(input.as_bytes());
            });
        }
        let stdout = Capture::start(child.stdout.take());
        let stderr = Capture::start(child.stderr.take());
        let ending = wait(&mut child, cancelled);
        let drained = Instant::now() + OUTPUT_DRAIN;
        let output = [stdout.finish(drained), stderr.finish(drained)];
        let mut text = output
            .iter()
            .map(|part| part.trim_end())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let note = match ending {
            Ending::Exited(status) if status.success() => {
                return Step {
                    text,
                    failed: false,
                };
            }
            Ending::Exited(_) if !text.is_empty() => None,
            Ending::Exited(status) => Some(match status.code() {
                Some(code) => format!("rbx exited with code {code}"),
                None => "rbx was stopped by a signal".to_string(),
            }),
            Ending::TimedOut => Some(format!(
                "rbx did not finish within {} minutes and was stopped",
                COMMAND_TIMEOUT.as_secs() / 60
            )),
            Ending::Cancelled => None,
            Ending::Lost(error) => Some(format!("Could not wait for rbx: {error}")),
        };
        if let Some(note) = note {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&note);
        }
        Step { text, failed: true }
    }
}

fn wait(child: &mut Child, cancelled: &AtomicBool) -> Ending {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut pause = Duration::from_millis(1);
    let ending = loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ending::Exited(status),
            Ok(None) if cancelled.load(Ordering::Relaxed) => break Ending::Cancelled,
            Ok(None) if Instant::now() >= deadline => break Ending::TimedOut,
            Ok(None) => {}
            Err(error) => break Ending::Lost(error),
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    };
    let _ = child.kill();
    let _ = child.wait();
    ending
}

struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl Capture {
    fn start(pipe: Option<impl Read + Send + 'static>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (finished, done) = mpsc::channel();
        if let Some(mut pipe) = pipe {
            let sink = Arc::clone(&bytes);
            thread::spawn(move || {
                let mut chunk = [0; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => sink.lock_recover().extend_from_slice(&chunk[..read]),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = finished.send(());
            });
        }
        Self { bytes, done }
    }

    fn finish(self, deadline: Instant) -> String {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let bytes = std::mem::take(&mut *self.bytes.lock_recover());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|version| PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "renium", "version": crate::app::build::VERSION},
    })
}

fn tool_result(texts: Vec<String>, failed: bool) -> Value {
    let content = texts
        .into_iter()
        .map(|text| json!({"type": "text", "text": text}))
        .collect::<Vec<_>>();
    json!({"content": content, "isError": failed})
}

fn reply(id: &Value, result: Result<Value, (i64, String)>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    }
}

/// stdout carries only protocol messages, one per line.
fn send(message: &Value) {
    let mut line = message.to_string();
    line.push('\n');
    let mut stdout = io::stdout().lock();
    let _ = stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_stops_its_commands_when_stdin_ends() {
        let (program, args) = if cfg!(windows) {
            ("ping", json!(["-n", "60", "127.0.0.1"]))
        } else {
            ("sleep", json!(["60"]))
        };
        let server = Server::new(PathBuf::from(program), std::env::current_dir().unwrap()).unwrap();
        let call = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "rbx", "arguments": {"args": args}},
        });
        let (done, served) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(server.serve(io::Cursor::new(format!("{call}\n"))));
        });
        served
            .recv_timeout(Duration::from_secs(20))
            .expect("the server returns once its input has ended")
            .unwrap();
    }
}
