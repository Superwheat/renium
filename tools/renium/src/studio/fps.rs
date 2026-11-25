use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
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
#[path = "fps_windows.rs"]
mod platform;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    #[default]
    Status,
    Unlock,
    Default,
}

#[derive(Args)]
pub(crate) struct FpsArgs {
    #[arg(value_enum, default_value = "status")]
    action: Action,
    #[arg(
        long,
        conflicts_with_all = ["pid", "player", "server"],
        help = "Remember the mode for all current and future Studio windows"
    )]
    global: bool,
    #[arg(
        long,
        conflicts_with_all = ["player", "server"],
        help = "Target an exact local Studio process"
    )]
    pid: Option<u32>,
    #[arg(
        long,
        conflicts_with_all = ["pid", "server"],
        help = "Target a test client instead of the Edit window"
    )]
    player: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["pid", "player"],
        help = "Target the local test server instead of the Edit window"
    )]
    server: bool,
}

#[derive(Args)]
pub(crate) struct WorkerArgs {
    #[arg(long)]
    pid: u32,
    #[arg(long)]
    identity: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    pub(super) mode: String,
    pub(super) unlocked: bool,
    pub(super) focused: bool,
    pub(super) hook_installed: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub(super) throttle_count: i32,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) render_checks: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(super) unthrottled_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
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

#[cfg(any(windows, test))]
pub(super) trait FpsBackend {
    fn is_running(&self) -> bool;
    fn step(&mut self, action: Action) -> Result<Status>;
}

pub(crate) fn run(args: FpsArgs, project: Option<&std::path::Path>) -> Result<()> {
    let value = if args.global {
        global::command(args.action)?
    } else if let Some(pid) = args.pid {
        command(pid, args.action)?
    } else {
        daemon_result(
            op::STUDIO_FPS,
            project,
            json!({
                "action": args.action,
                "player": args.player,
                "server": args.server,
            }),
            false,
            None,
        )?
    };
    output::print_json_output(&value, false)
}

fn state_root() -> Result<PathBuf> {
    Ok(update::user_data_dir()?.join("studio-fps"))
}

fn state_dir_in(root: &Path, pid: u32, identity: &str) -> PathBuf {
    let key = crate::system::files::sha256_hex(identity.as_bytes());
    root.join(format!("{pid}-{key}"))
}

fn state_dir(pid: u32, identity: &str) -> Result<PathBuf> {
    Ok(state_dir_in(&state_root()?, pid, identity))
}

#[cfg(any(windows, test))]
fn prune_stale_state_dirs_in(root: &Path, current_identity: impl Fn(u32) -> Option<String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == "global" {
            continue;
        }
        let Some((pid_str, hash)) = name.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let still_matches = current_identity(pid)
            .is_some_and(|ident| crate::system::files::sha256_hex(ident.as_bytes()) == hash);
        if still_matches {
            continue;
        }
        let unlocked = |file: &str| {
            let p = path.join(file);
            !p.exists()
                || OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&p)
                    .is_ok_and(|f| f.try_lock().is_ok())
        };
        if unlocked("worker.lock") && unlocked("command.lock") {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

pub(super) fn prune_stale_state_dirs() {
    #[cfg(windows)]
    if let Ok(root) = state_root() {
        prune_stale_state_dirs_in(&root, identity);
    }
}

pub(crate) fn helpers_disabled() -> bool {
    std::env::var_os("RENIUM_NO_FPS_HELPERS").is_some()
}

fn background_command() -> Result<Command> {
    #[cfg(windows)]
    crate::studio::audio::keep_standard_handles_private();
    let mut command = Command::new(std::env::current_exe()?);
    command
        .current_dir(update::user_data_dir()?)
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
    #[cfg(windows)]
    {
        let executable = crate::studio::input::process_executable_path(pid)?;
        let name = executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        anyhow::ensure!(
            name.eq_ignore_ascii_case("RobloxStudioBeta.exe"),
            "PID {pid} is not Roblox Studio"
        );
        identity(pid).context("Studio exited before FPS control started")
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        bail!("Studio FPS control is supported on Windows")
    }
}

pub(crate) fn command(pid: u32, action: Action) -> Result<Value> {
    if helpers_disabled() {
        bail!("Studio FPS helpers are disabled by RENIUM_NO_FPS_HELPERS");
    }
    let identity = validate_studio(pid)?;
    prune_stale_state_dirs();
    let dir = state_dir(pid, &identity)?;
    fs::create_dir_all(&dir)?;
    let command_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("command.lock"))?;
    let lock_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match command_lock.try_lock() {
            Ok(()) => break,
            Err(_) if Instant::now() < lock_deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(error).context(
                    "Another FPS command is in progress for this Studio; retry when it finishes",
                );
            }
        }
    }
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
        "fps-worker",
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
            .context("Could not start Studio FPS control")?;
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(bytes) = fs::read(dir.join("status.json"))
            && let Ok(reply) = serde_json::from_slice::<Reply>(&bytes)
            && reply.request == id
        {
            if let Some(error) = reply.status.error {
                bail!("Studio FPS: {error}");
            }
            let mut value = serde_json::to_value(reply.status)?;
            value["pid"] = json!(pid);
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!(
                "Studio FPS did not acknowledge the command; run `rbx fps status` before retrying"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(any(windows, test))]
struct WorkerState {
    last_request: String,
    mode: Action,
    last_bytes: Vec<u8>,
}

#[cfg(any(windows, test))]
impl WorkerState {
    fn new() -> Self {
        Self {
            last_request: String::new(),
            mode: Action::Default,
            last_bytes: Vec::new(),
        }
    }

    fn step_once(
        &mut self,
        dir: &Path,
        backend: &mut dyn FpsBackend,
        still_same_process: bool,
    ) -> Result<bool> {
        if !backend.is_running() || !still_same_process {
            if self.mode == Action::Unlock {
                let _ = backend.step(Action::Default);
            }
            return Ok(true);
        }
        let request: Request = serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
        let is_new_request = request.id != self.last_request;
        let action = if is_new_request {
            self.last_request = request.id;
            if request.action != Action::Status {
                self.mode = request.action;
            }
            request.action
        } else {
            self.mode
        };
        let mut status = match backend.step(action) {
            Ok(status) => status,
            Err(error) => {
                if action == Action::Unlock {
                    self.mode = Action::Default;
                }
                Status {
                    error: Some(format!("{error:#}")),
                    ..Status::default()
                }
            }
        };
        status.mode = match self.mode {
            Action::Unlock => "unlock",
            _ => "default",
        }
        .to_string();
        let idle = self.mode != Action::Unlock && !status.unlocked;
        let bytes = serde_json::to_vec(&Reply {
            request: self.last_request.clone(),
            status,
        })?;
        if bytes != self.last_bytes {
            atomic_write_file(&dir.join("status.json"), &bytes)?;
            self.last_bytes = bytes;
        }
        if idle {
            let command_lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(dir.join("command.lock"))?;
            if command_lock.try_lock().is_ok() {
                let latest: Request = serde_json::from_slice(&fs::read(dir.join("request.json"))?)?;
                if latest.id == self.last_request {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

pub(crate) fn run_worker(args: WorkerArgs) -> Result<()> {
    #[cfg(windows)]
    {
        anyhow::ensure!(
            validate_studio(args.pid)? == args.identity,
            "Studio process identity changed before FPS control started"
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
                        mode: "default".into(),
                        error: Some(format!("{error:#}")),
                        ..Status::default()
                    },
                };
                atomic_write_file(&dir.join("status.json"), &serde_json::to_vec(&reply)?)?;
                return Ok(());
            }
        };
        let mut state = WorkerState::new();
        loop {
            let same = identity(args.pid).as_deref() == Some(args.identity.as_str());
            if state.step_once(&dir, &mut backend, same)? {
                drop(backend);
                drop(lock);
                prune_stale_state_dirs();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        bail!("Studio FPS control is supported on Windows")
    }
}

#[cfg(windows)]
fn identity(pid: u32) -> Option<String> {
    update::process_start_identity(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBackend {
        running: bool,
        unlocked: bool,
        hook_installed: bool,
        fail_next: Option<String>,
        calls: Vec<Action>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                running: true,
                unlocked: false,
                hook_installed: false,
                fail_next: None,
                calls: Vec::new(),
            }
        }
    }

    impl FpsBackend for FakeBackend {
        fn is_running(&self) -> bool {
            self.running
        }

        fn step(&mut self, action: Action) -> Result<Status> {
            self.calls.push(action);
            if let Some(error) = self.fail_next.take() {
                bail!("{error}");
            }
            match action {
                Action::Unlock => {
                    self.unlocked = true;
                    self.hook_installed = true;
                }
                Action::Default => {
                    self.unlocked = false;
                    self.hook_installed = false;
                }
                Action::Status => {}
            }
            Ok(Status {
                unlocked: self.unlocked,
                hook_installed: self.hook_installed,
                ..Status::default()
            })
        }
    }

    fn write_req(dir: &Path, id: &str, action: Action) {
        atomic_write_file(
            &dir.join("request.json"),
            &serde_json::to_vec(&Request {
                id: id.into(),
                action,
            })
            .unwrap(),
        )
        .unwrap();
    }

    fn read_reply(dir: &Path) -> Reply {
        serde_json::from_slice(&fs::read(dir.join("status.json")).unwrap()).unwrap()
    }

    #[test]
    fn worker_state_machine_handles_unlock_status_default_error_and_idle_exit() {
        let dir = crate::tests::support::temp_dir("fps-worker-sm");
        let mut backend = FakeBackend::new();
        let mut worker = WorkerState::new();

        write_req(&dir, "r1", Action::Status);
        assert!(worker.step_once(&dir, &mut backend, true).unwrap());
        let r1 = read_reply(&dir);
        assert_eq!(r1.request, "r1");
        assert_eq!(r1.status.mode, "default");
        assert!(!r1.status.unlocked);
        assert_eq!(backend.calls, vec![Action::Status]);

        let mut worker = WorkerState::new();
        write_req(&dir, "r2", Action::Unlock);
        assert!(!worker.step_once(&dir, &mut backend, true).unwrap());
        let r2 = read_reply(&dir);
        assert_eq!(r2.status.mode, "unlock");
        assert!(r2.status.unlocked);
        assert!(!worker.step_once(&dir, &mut backend, true).unwrap());

        write_req(&dir, "r3", Action::Status);
        assert!(!worker.step_once(&dir, &mut backend, true).unwrap());
        let r3 = read_reply(&dir);
        assert_eq!(r3.request, "r3");
        assert_eq!(r3.status.mode, "unlock");
        assert!(r3.status.unlocked);

        write_req(&dir, "r4", Action::Default);
        assert!(worker.step_once(&dir, &mut backend, true).unwrap());
        let r4 = read_reply(&dir);
        assert_eq!(r4.request, "r4");
        assert_eq!(r4.status.mode, "default");
        assert!(!r4.status.unlocked);
        assert!(!r4.status.hook_installed);

        let mut expired = FakeBackend::new();
        expired.hook_installed = true;
        expired.unlocked = false;
        let mut worker = WorkerState::new();
        write_req(&dir, "r5", Action::Status);
        assert!(worker.step_once(&dir, &mut expired, true).unwrap());
        assert_eq!(expired.calls, vec![Action::Status]);
        let r5 = read_reply(&dir);
        assert!(r5.status.hook_installed && !r5.status.unlocked);

        let mut failing = FakeBackend::new();
        failing.fail_next = Some("probe refused".into());
        let mut worker = WorkerState::new();
        write_req(&dir, "r6", Action::Unlock);
        assert!(worker.step_once(&dir, &mut failing, true).unwrap());
        let r6 = read_reply(&dir);
        assert_eq!(r6.status.mode, "default");
        assert!(
            r6.status
                .error
                .as_deref()
                .unwrap()
                .contains("probe refused")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_stale_state_dirs_removes_exited_and_keeps_live_and_global() {
        let root = crate::tests::support::temp_dir("fps-prune");
        let live_dir = state_dir_in(&root, 100, "live-ident");
        let dead_dir = state_dir_in(&root, 200, "dead-ident");
        let reused_pid_dir = state_dir_in(&root, 100, "old-ident");
        let global_dir = root.join("global");
        for d in [&live_dir, &dead_dir, &reused_pid_dir, &global_dir] {
            fs::create_dir_all(d).unwrap();
        }
        prune_stale_state_dirs_in(&root, |pid| (pid == 100).then(|| "live-ident".to_string()));
        assert!(live_dir.exists());
        assert!(global_dir.exists());
        assert!(!dead_dir.exists());
        assert!(!reused_pid_dir.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
