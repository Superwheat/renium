use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::{output, update};
use crate::automation::{commands::daemon_result, op};
use crate::system::files::atomic_write_file;

pub(crate) mod global;

#[cfg(windows)]
#[path = "audio_windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "audio_macos.rs"]
mod platform;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    #[default]
    Status,
    Off,
    Mute,
    Unmute,
    Auto,
}

#[derive(Args)]
pub(crate) struct AudioArgs {
    #[arg(value_enum, default_value = "status")]
    action: Action,
    #[arg(long, conflicts_with_all = ["pid", "player"], help = "Remember the mode for all current and future Studio windows")]
    global: bool,
    #[arg(long, help = "Target an exact local Studio process")]
    pid: Option<u32>,
    #[arg(
        long,
        conflicts_with = "pid",
        help = "Target a test client instead of the Edit window"
    )]
    player: Option<String>,
}

#[derive(Args)]
pub(crate) struct WorkerArgs {
    #[arg(long)]
    pid: u32,
    #[arg(long)]
    identity: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    pub(super) mode: String,
    pub(super) focused: bool,
    pub(super) sessions: usize,
    pub(super) muted_sessions: usize,
    pub(super) pending_restores: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Request {
    id: String,
    action: Action,
}

#[derive(Deserialize, Serialize)]
struct Reply {
    request: String,
    status: Status,
}

pub(crate) fn run(args: AudioArgs, project: Option<&std::path::Path>) -> Result<()> {
    let value = if args.global {
        global::command(args.action)?
    } else if let Some(pid) = args.pid {
        command(pid, args.action)?
    } else {
        daemon_result(
            op::STUDIO_AUDIO,
            project,
            json!({ "action": args.action, "player": args.player }),
            false,
            None,
        )?
    };
    output::print_json_output(&value, false)
}

fn state_dir(pid: u32, identity: &str) -> Result<PathBuf> {
    let key = crate::system::files::sha256_hex(identity.as_bytes());
    Ok(update::user_data_dir()?
        .join("studio-audio")
        .join(format!("{pid}-{key}")))
}

fn background_command() -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    Ok(command)
}

fn validate_studio(pid: u32) -> Result<String> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        #[cfg(windows)]
        let executable = crate::studio::input::process_executable_path(pid)?;
        #[cfg(target_os = "macos")]
        let executable = crate::studio::native::serializer::process_executable_path(pid)?;
        let name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        anyhow::ensure!(
            matches!(
                name,
                "RobloxStudioBeta.exe" | "RobloxStudio.exe" | "RobloxStudio" | "RobloxStudio.bin"
            ),
            "PID {pid} is not Roblox Studio"
        );
        identity(pid).context("Studio exited before audio control started")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = pid;
        bail!("Studio audio control is supported on Windows and macOS")
    }
}

pub(crate) fn command(pid: u32, action: Action) -> Result<Value> {
    let identity = validate_studio(pid)?;
    let dir = state_dir(pid, &identity)?;
    fs::create_dir_all(&dir)?;
    let command_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("command.lock"))?;
    command_lock
        .try_lock()
        .context("Another audio command is in progress for this Studio; retry when it finishes")?;
    let id = format!(
        "{}-{}",
        std::process::id(),
        crate::app::timing::current_millis()
    );
    atomic_write_file(
        &dir.join("request.json"),
        &serde_json::to_vec(&Request {
            id: id.clone(),
            action,
        })?,
    )?;
    let mut worker = background_command()?;
    worker.args([
        "audio-worker",
        "--pid",
        &pid.to_string(),
        "--identity",
        &identity,
    ]);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("worker.lock"))?;
    if lock.try_lock().is_ok() {
        lock.unlock()?;
        worker
            .spawn()
            .context("Could not start Studio audio control")?;
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(bytes) = fs::read(dir.join("status.json"))
            && let Ok(reply) = serde_json::from_slice::<Reply>(&bytes)
            && reply.request == id
        {
            if let Some(error) = reply.status.error {
                bail!("Studio audio: {error}");
            }
            let mut value = serde_json::to_value(reply.status)?;
            value["pid"] = json!(pid);
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!(
                "Studio audio did not acknowledge the command; run `rbx audio status` before retrying"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn run_worker(args: WorkerArgs) -> Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        anyhow::ensure!(
            validate_studio(args.pid)? == args.identity,
            "Studio process identity changed before audio control started"
        );
        let dir = state_dir(args.pid, &args.identity)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("worker.lock"))?;
        if lock.try_lock().is_err() {
            return Ok(());
        }
        let mut backend = match platform::Backend::new(args.pid) {
            Ok(backend) => backend,
            Err(error) => {
                let request: Request =
                    serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
                let reply = Reply {
                    request: request.id,
                    status: Status {
                        error: Some(format!("{error:#}")),
                        ..Status::default()
                    },
                };
                atomic_write_file(&dir.join("status.json"), &serde_json::to_vec(&reply)?)?;
                return Ok(());
            }
        };
        let mut last_request = String::new();
        let mut mode = Action::Off;
        let mut last_bytes = Vec::new();
        loop {
            #[cfg(windows)]
            let exited = !backend.is_running();
            #[cfg(not(windows))]
            let exited = false;
            if exited || identity(args.pid).as_deref() != Some(args.identity.as_str()) {
                backend.step(Action::Off)?;
                return Ok(());
            }
            let request: Request = serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
            let action = if request.id != last_request {
                last_request = request.id;
                if request.action != Action::Status {
                    mode = request.action;
                }
                mode
            } else {
                mode
            };
            let mut status = match backend.step(action) {
                Ok(status) => status,
                Err(error) => Status {
                    error: Some(format!("{error:#}")),
                    ..Status::default()
                },
            };
            if mode == Action::Unmute {
                mode = Action::Off;
            }
            status.mode = match mode {
                Action::Auto => "auto",
                Action::Mute => "mute",
                _ => "off",
            }
            .to_string();
            let idle = mode == Action::Off && status.pending_restores == 0;
            let bytes = serde_json::to_vec(&Reply {
                request: last_request.clone(),
                status,
            })?;
            if bytes != last_bytes {
                atomic_write_file(&dir.join("status.json"), &bytes)?;
                last_bytes = bytes;
            }
            if idle {
                let command_lock = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(dir.join("command.lock"))?;
                if command_lock.try_lock().is_ok() {
                    let latest: Request =
                        serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
                    if latest.id == last_request {
                        drop(backend);
                        lock.unlock()?;
                        return Ok(());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = args;
        bail!("Studio audio control is supported on Windows and macOS")
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn identity(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        update::process_start_identity(pid)
    }
    #[cfg(target_os = "macos")]
    {
        crate::studio::performance::audio_process_identity(pid)
    }
}

#[cfg(any(windows, test))]
#[derive(Default)]
pub(super) struct MuteOwnership {
    changed: bool,
}

#[cfg(any(windows, test))]
impl MuteOwnership {
    pub(super) fn desired(&self, current: bool, mute: bool, unmute: bool) -> Option<bool> {
        if unmute {
            current.then_some(false)
        } else if mute {
            (!current).then_some(true)
        } else if self.changed && current {
            Some(false)
        } else {
            None
        }
    }

    pub(super) fn accepted(&mut self, mute: bool, changed: bool) {
        self.changed = mute && (self.changed || changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_restores_only_owned_mutes() {
        let mut state = MuteOwnership::default();
        assert_eq!(state.desired(true, true, false), None);
        state.accepted(true, false);
        assert_eq!(state.desired(true, false, false), None);
        assert_eq!(state.desired(false, true, false), Some(true));
        state.accepted(true, true);
        assert_eq!(state.desired(true, false, false), Some(false));
        assert_eq!(state.desired(false, false, false), None);
        state.accepted(false, false);
        assert_eq!(state.desired(true, false, false), None);
        assert_eq!(state.desired(true, false, true), Some(false));
    }
}
