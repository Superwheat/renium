use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Action, Reply, Status};
use crate::app::{timing::current_millis, update};
use crate::system::files::atomic_write_file;

const RETRY_INTERVAL: Duration = Duration::from_secs(3);
const SLOW_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const QUICK_ATTEMPTS: u32 = 2;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Setting {
    revision: String,
    mode: Action,
}

impl Setting {
    fn enabled(&self) -> bool {
        self.mode == Action::Unlock
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Process {
    pid: u32,
    identity: String,
    revision: String,
    #[serde(flatten)]
    status: Status,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    revision: String,
    mode: Action,
    updated: u128,
    processes: Vec<Process>,
}

#[derive(Clone)]
struct AttemptState {
    revision: String,
    failures: u32,
    last_attempt: Instant,
    permanent: bool,
}

fn directory() -> Result<PathBuf> {
    Ok(update::user_data_dir()?.join("studio-fps").join("global"))
}

fn setting_from_value(value: Value) -> Result<Setting> {
    let mode: Action = serde_json::from_value(value)?;
    anyhow::ensure!(
        matches!(mode, Action::Default | Action::Unlock),
        "studioFpsMode must be default or unlock"
    );
    Ok(Setting {
        revision: serde_json::to_string(&mode)?,
        mode,
    })
}

fn read_setting() -> Result<Setting> {
    setting_from_value(crate::project::config::user_studio_fps_mode()?)
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

fn start_worker(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let lock = open_lock(&dir.join("worker.lock"))?;
    if lock.try_lock().is_ok() {
        lock.unlock()?;
        super::background_command()?
            .arg("fps-global-worker")
            .spawn()
            .context("Could not start global Studio FPS control")?;
    }
    Ok(())
}

fn should_resume(setting: &Setting, previous: Option<&Snapshot>) -> bool {
    if setting.enabled() {
        return true;
    }
    previous.is_some_and(|snapshot| {
        snapshot.mode == Action::Unlock
            || snapshot
                .processes
                .iter()
                .any(|process| process.status.unlocked || process.status.hook_installed)
    })
}

pub(crate) fn resume() -> Result<()> {
    if !cfg!(windows) || super::helpers_disabled() {
        return Ok(());
    }
    let dir = directory()?;
    let setting = read_setting()?;
    let previous = fs::read(dir.join("status.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok());
    if should_resume(&setting, previous.as_ref()) {
        start_worker(&dir)?;
    }
    Ok(())
}

pub(crate) fn command(action: Action) -> Result<Value> {
    anyhow::ensure!(cfg!(windows), "Studio FPS control is supported on Windows");
    if super::helpers_disabled() {
        bail!("Studio FPS helpers are disabled by RENIUM_NO_FPS_HELPERS");
    }
    let dir = directory()?;
    fs::create_dir_all(&dir)?;
    let lock = open_lock(&dir.join("command.lock"))?;
    lock.try_lock()
        .context("Another global FPS command is in progress; retry when it finishes")?;
    if action != Action::Status {
        crate::project::config::set_user_studio_fps_mode(json!(action))?;
    }
    let setting = read_setting()?;
    let started = current_millis();
    start_worker(&dir)?;
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Ok(bytes) = fs::read(dir.join("status.json"))
            && let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes)
            && snapshot.revision == setting.revision
            && snapshot.updated >= started
        {
            let errors: Vec<_> = snapshot
                .processes
                .iter()
                .filter_map(|process| {
                    process
                        .status
                        .error
                        .as_ref()
                        .map(|error| format!("PID {}: {error}", process.pid))
                })
                .collect();
            if action != Action::Status && !errors.is_empty() {
                bail!(
                    "Global Studio FPS mode was saved, but some windows could not apply it: {}. Check `rbx fps status --global`",
                    errors.join("; ")
                );
            }
            return Ok(json!({
                "scope": "global",
                "mode": snapshot.mode,
                "ok": errors.is_empty(),
                "processes": snapshot.processes,
            }));
        }
        if Instant::now() >= deadline {
            bail!(
                "Global Studio FPS did not acknowledge the setting; check `rbx fps status --global`"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn worker_running(dir: &Path) -> bool {
    open_lock(&dir.join("worker.lock")).is_ok_and(|lock| lock.try_lock().is_err())
}

fn refresh_in(root: &Path, process: &mut Process) -> bool {
    let dir = super::state_dir_in(root, process.pid, &process.identity);
    if let Ok(bytes) = fs::read(dir.join("status.json"))
        && let Ok(reply) = serde_json::from_slice::<Reply>(&bytes)
    {
        process.status = reply.status;
    }
    worker_running(&dir) || (process.status.mode == "default" && !process.status.unlocked)
}

fn refresh(process: &mut Process) -> bool {
    let Ok(root) = super::state_root() else {
        return false;
    };
    refresh_in(&root, process)
}

fn needs_apply(process: &Process, setting: &Setting, running: bool) -> bool {
    process.revision != setting.revision || !running
}

fn is_permanent_apply_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("renderschedulerstate")
        || lower.contains("expected 1 studio")
        || lower.contains("native fpsunlock unavailable")
        || lower.contains("vtable")
        || lower.contains("is not roblox studio")
        || lower.contains("disabled by renium_no_fps_helpers")
        || lower.contains("supported on windows")
}

fn should_skip_retry(state: Option<&AttemptState>, revision: &str, now: Instant) -> bool {
    let Some(state) = state else {
        return false;
    };
    if state.revision != revision {
        return false;
    }
    let interval = if state.failures < QUICK_ATTEMPTS {
        RETRY_INTERVAL
    } else {
        SLOW_RETRY_INTERVAL
    };
    state.permanent || now.saturating_duration_since(state.last_attempt) < interval
}

fn step_global_tick<R, A>(
    setting: &Setting,
    live: &BTreeMap<u32, String>,
    processes: &mut BTreeMap<u32, Process>,
    attempted: &mut HashMap<u32, AttemptState>,
    now: Instant,
    mut refresh_fn: R,
    apply_fn: A,
) where
    R: FnMut(&mut Process) -> bool,
    A: Fn(u32, Action) -> Result<Status> + Sync,
{
    processes.retain(|pid, process| live.get(pid) == Some(&process.identity));
    attempted.retain(|pid, _| processes.contains_key(pid));
    if setting.enabled() {
        for (pid, identity) in live {
            processes.entry(*pid).or_insert_with(|| Process {
                pid: *pid,
                identity: identity.clone(),
                revision: String::new(),
                status: Status::default(),
            });
        }
    }
    let mut to_apply: Vec<u32> = Vec::new();
    for (&pid, process) in processes.iter_mut() {
        let running = process.revision == setting.revision && refresh_fn(process);
        if !needs_apply(process, setting, running) {
            continue;
        }
        if should_skip_retry(attempted.get(&pid), &setting.revision, now) {
            continue;
        }
        to_apply.push(pid);
    }
    let results: Vec<(u32, Result<Status>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = to_apply
            .into_iter()
            .map(|pid| {
                let mode = setting.mode;
                let apply_ref = &apply_fn;
                scope.spawn(move || (pid, apply_ref(pid, mode)))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    for (pid, outcome) in results {
        let Some(process) = processes.get_mut(&pid) else {
            continue;
        };
        match outcome {
            Ok(status) => {
                process.status = status;
                process.revision.clone_from(&setting.revision);
                attempted.remove(&pid);
            }
            Err(error) => {
                let text = format!("{error:#}");
                let permanent = is_permanent_apply_error(&text);
                let prev = attempted
                    .get(&pid)
                    .filter(|state| state.revision == setting.revision);
                let failures = prev.map_or(1, |state| state.failures.saturating_add(1));
                attempted.insert(
                    pid,
                    AttemptState {
                        revision: setting.revision.clone(),
                        failures,
                        last_attempt: now,
                        permanent,
                    },
                );
                process.status.error = Some(text);
            }
        }
    }
}

pub(crate) fn run_worker() -> Result<()> {
    let dir = directory()?;
    fs::create_dir_all(&dir)?;
    let lock = open_lock(&dir.join("worker.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let previous = fs::read(dir.join("status.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok())
        .unwrap_or_default();
    let mut processes: BTreeMap<u32, Process> = previous
        .processes
        .into_iter()
        .map(|process| (process.pid, process))
        .collect();
    let mut attempted: HashMap<u32, AttemptState> = HashMap::new();
    loop {
        let setting = read_setting()?;
        let live: BTreeMap<_, _> = crate::studio::diagnosis::studio_process_ids()
            .into_iter()
            .filter_map(|pid| {
                super::validate_studio(pid)
                    .ok()
                    .map(|identity| (pid, identity))
            })
            .collect();
        step_global_tick(
            &setting,
            &live,
            &mut processes,
            &mut attempted,
            Instant::now(),
            refresh,
            |pid, mode| {
                let value = super::command(pid, mode)?;
                Ok(serde_json::from_value(value)?)
            },
        );
        let idle = !setting.enabled()
            && processes.values().all(|process| {
                process.status.error.is_none()
                    && process.status.mode == "default"
                    && !process.status.unlocked
                    && !process.status.hook_installed
            });
        let snapshot = Snapshot {
            revision: setting.revision,
            mode: setting.mode,
            updated: current_millis(),
            processes: processes.values().cloned().collect(),
        };
        atomic_write_file(&dir.join("status.json"), &serde_json::to_vec(&snapshot)?)?;
        if idle {
            let command_lock = open_lock(&dir.join("command.lock"))?;
            if command_lock.try_lock().is_ok() && read_setting()?.revision == snapshot.revision {
                super::prune_stale_state_dirs();
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn global_fps_modes_adopt_new_processes_and_respect_local_overrides_via_refresh() {
        let root = crate::tests::support::temp_dir("fps-global-refresh");
        let unlock = Setting {
            revision: "\"unlock\"".into(),
            mode: Action::Unlock,
        };
        let default_setting = Setting {
            revision: "\"default\"".into(),
            mode: Action::Default,
        };
        let mut processes = BTreeMap::new();
        let mut attempted = HashMap::new();
        let live = BTreeMap::from([(101_u32, "ident-101".to_string())]);
        let apply_count = AtomicUsize::new(0);

        let now = Instant::now();
        step_global_tick(
            &unlock,
            &live,
            &mut processes,
            &mut attempted,
            now,
            |p| refresh_in(&root, p),
            |pid, action| {
                apply_count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(pid, 101);
                assert_eq!(action, Action::Unlock);
                Ok(Status {
                    mode: "unlock".into(),
                    unlocked: true,
                    hook_installed: true,
                    ..Status::default()
                })
            },
        );
        assert_eq!(apply_count.load(Ordering::Relaxed), 1);
        assert!(processes[&101].status.unlocked);

        let pdir = super::super::state_dir_in(&root, 101, "ident-101");
        fs::create_dir_all(&pdir).unwrap();
        atomic_write_file(
            &pdir.join("status.json"),
            &serde_json::to_vec(&Reply {
                request: "local-def".into(),
                status: Status {
                    mode: "default".into(),
                    unlocked: false,
                    hook_installed: false,
                    ..Status::default()
                },
            })
            .unwrap(),
        )
        .unwrap();

        step_global_tick(
            &unlock,
            &live,
            &mut processes,
            &mut attempted,
            now + Duration::from_secs(5),
            |p| refresh_in(&root, p),
            |_, _| panic!("local default override must not be overwritten by global unlock"),
        );
        assert_eq!(processes[&101].status.mode, "default");
        assert!(!processes[&101].status.unlocked);

        let live2 = BTreeMap::from([
            (101_u32, "ident-101".to_string()),
            (202_u32, "ident-202".to_string()),
        ]);
        step_global_tick(
            &unlock,
            &live2,
            &mut processes,
            &mut attempted,
            now + Duration::from_secs(6),
            |p| refresh_in(&root, p),
            |pid, action| {
                apply_count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(pid, 202);
                assert_eq!(action, Action::Unlock);
                Ok(Status {
                    mode: "unlock".into(),
                    unlocked: true,
                    hook_installed: true,
                    ..Status::default()
                })
            },
        );
        assert_eq!(apply_count.load(Ordering::Relaxed), 2);
        assert!(processes[&202].status.unlocked);

        step_global_tick(
            &default_setting,
            &live2,
            &mut processes,
            &mut attempted,
            now + Duration::from_secs(7),
            |p| refresh_in(&root, p),
            |_, action| {
                apply_count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(action, Action::Default);
                Ok(Status {
                    mode: "default".into(),
                    unlocked: false,
                    hook_installed: false,
                    ..Status::default()
                })
            },
        );
        assert_eq!(apply_count.load(Ordering::Relaxed), 4);
        assert!(!processes[&202].status.unlocked);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn global_worker_stops_retrying_unsupported_build_and_slows_transient_retries() {
        let unlock = Setting {
            revision: "\"unlock\"".into(),
            mode: Action::Unlock,
        };
        let live = BTreeMap::from([
            (10_u32, "unsupported".to_string()),
            (20_u32, "transient".to_string()),
        ]);
        let mut processes = BTreeMap::new();
        let mut attempted = HashMap::new();
        let calls_10 = AtomicUsize::new(0);
        let calls_20 = AtomicUsize::new(0);
        let t0 = Instant::now();

        for seconds in [0_u64, 4, 8, 12, 16, 20, 62, 66, 70] {
            step_global_tick(
                &unlock,
                &live,
                &mut processes,
                &mut attempted,
                t0 + Duration::from_secs(seconds),
                |_| false,
                |pid, _| {
                    if pid == 10 {
                        calls_10.fetch_add(1, Ordering::Relaxed);
                        bail!("RenderSchedulerState vtable resolved 0 shouldSkipRender candidates");
                    } else {
                        calls_20.fetch_add(1, Ordering::Relaxed);
                        bail!("temporary open error");
                    }
                },
            );
        }
        assert_eq!(
            calls_10.load(Ordering::Relaxed),
            1,
            "unsupported Studio build must not be re-scanned in a loop"
        );
        assert_eq!(
            calls_20.load(Ordering::Relaxed),
            QUICK_ATTEMPTS as usize + 1,
            "after the quick attempts a transient error is retried once a minute"
        );
    }

    #[test]
    fn resume_starts_worker_when_setting_was_hand_edited_to_default() {
        let default_setting = Setting {
            revision: "\"default\"".into(),
            mode: Action::Default,
        };
        let unlock_setting = Setting {
            revision: "\"unlock\"".into(),
            mode: Action::Unlock,
        };
        assert!(should_resume(&unlock_setting, None));
        assert!(!should_resume(&default_setting, None));
        let clean_default_snap = Snapshot {
            revision: "\"default\"".into(),
            mode: Action::Default,
            updated: 1,
            processes: vec![],
        };
        assert!(!should_resume(&default_setting, Some(&clean_default_snap)));
        let stale_unlock_snap = Snapshot {
            revision: "\"unlock\"".into(),
            mode: Action::Unlock,
            updated: 1,
            processes: vec![Process {
                pid: 42,
                identity: "id".into(),
                revision: "\"unlock\"".into(),
                status: Status {
                    mode: "unlock".into(),
                    unlocked: true,
                    hook_installed: true,
                    ..Status::default()
                },
            }],
        };
        assert!(should_resume(&default_setting, Some(&stale_unlock_snap)));
    }

    #[test]
    fn global_fps_setting_validates_default_and_unlock_only() {
        assert!(!setting_from_value(json!("default")).unwrap().enabled());
        assert!(setting_from_value(json!("unlock")).unwrap().enabled());
        for invalid in [json!(true), json!("status"), json!("off"), json!("auto")] {
            assert!(setting_from_value(invalid).is_err());
        }
    }

    #[test]
    fn global_fps_needs_no_project_but_targeted_fps_does() {
        for global in [true, false] {
            let request = crate::automation::Request {
                v: crate::automation::PROTOCOL_VERSION,
                id: 1,
                op: crate::automation::op::STUDIO_FPS,
                cx: None,
                p: json!({"global": global, "action": "status"}),
            };
            assert_eq!(request.validate().is_ok(), global);
        }
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "fps", "unlock", "--global"])
                .is_ok()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "fps", "default", "--server"])
                .is_ok()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "fps", "unlock", "--global", "--pid", "1"])
                .is_err()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "fps", "unlock", "--global", "--player", "1"])
                .is_err()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "fps", "unlock", "--global", "--server"])
                .is_err()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "fps", "unlock", "--player", "1", "--server"])
                .is_err()
        );
    }
}
