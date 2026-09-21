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

#[derive(Default, Deserialize, Serialize)]
struct Setting {
    revision: String,
    mode: Action,
}

impl Setting {
    fn enabled(&self) -> bool {
        matches!(self.mode, Action::Auto | Action::Mute)
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

fn directory() -> Result<PathBuf> {
    Ok(update::user_data_dir()?.join("studio-audio").join("global"))
}

fn setting_from_value(value: Value) -> Result<Setting> {
    let mode: Action = serde_json::from_value(value)?;
    anyhow::ensure!(
        matches!(mode, Action::Off | Action::Mute | Action::Auto),
        "studioAudioMode must be off, mute, or auto"
    );
    Ok(Setting {
        revision: serde_json::to_string(&mode)?,
        mode,
    })
}

fn read_setting() -> Result<Setting> {
    setting_from_value(crate::project::config::user_studio_audio_mode()?)
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
            .arg("audio-global-worker")
            .spawn()
            .context("Could not start global Studio audio control")?;
    }
    Ok(())
}

pub(crate) fn resume() -> Result<()> {
    let dir = directory()?;
    if read_setting()?.enabled() {
        start_worker(&dir)?;
    }
    Ok(())
}

pub(crate) fn command(action: Action) -> Result<Value> {
    anyhow::ensure!(
        cfg!(any(windows, target_os = "macos")),
        "Studio audio control is supported on Windows and macOS"
    );
    let dir = directory()?;
    fs::create_dir_all(&dir)?;
    let lock = open_lock(&dir.join("command.lock"))?;
    lock.try_lock()
        .context("Another global audio command is in progress; retry when it finishes")?;
    if action != Action::Status {
        crate::project::config::set_user_studio_audio_mode(json!(if action == Action::Unmute {
            Action::Off
        } else {
            action
        }))?;
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
                    "Global Studio audio mode was saved, but some windows could not apply it: {}. Check `rbx audio status --global`",
                    errors.join("; ")
                );
            }
            return Ok(json!({
                "scope": "global", "mode": snapshot.mode,
                "ok": errors.is_empty(), "processes": snapshot.processes,
            }));
        }
        if Instant::now() >= deadline {
            bail!(
                "Global Studio audio did not acknowledge the setting; check `rbx audio status --global`"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn worker_running(dir: &Path) -> bool {
    open_lock(&dir.join("worker.lock")).is_ok_and(|lock| lock.try_lock().is_err())
}

fn refresh(process: &mut Process) -> bool {
    let Ok(dir) = super::state_dir(process.pid, &process.identity) else {
        return false;
    };
    if let Ok(bytes) = fs::read(dir.join("status.json"))
        && let Ok(reply) = serde_json::from_slice::<Reply>(&bytes)
    {
        process.status = reply.status;
    }
    worker_running(&dir) || (process.status.mode == "off" && process.status.pending_restores == 0)
}

fn needs_apply(process: &Process, setting: &Setting, running: bool) -> bool {
    process.revision != setting.revision || !running
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
    let mut attempted = HashMap::new();
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
        processes.retain(|pid, process| live.get(pid) == Some(&process.identity));
        attempted.retain(|pid, _| processes.contains_key(pid));
        if setting.enabled() {
            for (pid, identity) in live {
                processes.entry(pid).or_insert_with(|| Process {
                    pid,
                    identity,
                    revision: String::new(),
                    status: Status::default(),
                });
            }
        }
        std::thread::scope(|scope| {
            for process in processes.values_mut() {
                let running = process.revision == setting.revision && refresh(process);
                if !needs_apply(process, &setting, running) {
                    continue;
                }
                if attempted.get(&process.pid).is_some_and(
                    |(revision, time): &(String, Instant)| {
                        revision == &setting.revision && time.elapsed() < Duration::from_secs(3)
                    },
                ) {
                    continue;
                }
                attempted.insert(process.pid, (setting.revision.clone(), Instant::now()));
                let setting = &setting;
                scope.spawn(move || match super::command(process.pid, setting.mode) {
                    Ok(value) => match serde_json::from_value(value) {
                        Ok(status) => {
                            process.status = status;
                            process.revision.clone_from(&setting.revision);
                        }
                        Err(error) => process.status.error = Some(error.to_string()),
                    },
                    Err(error) => process.status.error = Some(format!("{error:#}")),
                });
            }
        });
        let idle = !setting.enabled()
            && processes.values().all(|process| {
                process.status.error.is_none()
                    && process.status.mode == "off"
                    && process.status.pending_restores == 0
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
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_modes_adopt_new_processes_and_preserve_local_overrides_until_changed() {
        let mut process = Process {
            pid: 1,
            identity: "first".into(),
            revision: String::new(),
            status: Status::default(),
        };
        let mut setting = Setting {
            revision: "enabled".into(),
            mode: Action::Auto,
        };
        assert!(needs_apply(&process, &setting, false));
        process.revision.clone_from(&setting.revision);
        assert!(!needs_apply(&process, &setting, true));
        process.status.mode = "off".into();
        assert!(!needs_apply(&process, &setting, true));
        assert!(needs_apply(&process, &setting, false));
        setting.revision = "disabled".into();
        setting.mode = Action::Off;
        assert!(needs_apply(&process, &setting, true));
        assert!(!setting.enabled());
        setting.mode = Action::Mute;
        assert!(setting.enabled());
    }

    #[test]
    fn global_setting_survives_restart_without_enabling_by_default() {
        assert!(!setting_from_value(json!("off")).unwrap().enabled());
        let first = setting_from_value(json!("auto")).unwrap();
        let restarted = setting_from_value(json!("auto")).unwrap();
        assert_eq!(first.revision, restarted.revision);
        assert_ne!(
            first.revision,
            setting_from_value(json!("mute")).unwrap().revision
        );
        for invalid in [json!(true), json!("unmute"), json!("status"), json!("yes")] {
            assert!(setting_from_value(invalid).is_err());
        }
    }

    #[test]
    fn global_audio_needs_no_project_but_targeted_audio_does() {
        for global in [true, false] {
            let request = crate::automation::Request {
                v: crate::automation::PROTOCOL_VERSION,
                id: 1,
                op: crate::automation::op::STUDIO_AUDIO,
                cx: None,
                p: json!({"global":global,"action":"status"}),
            };
            assert_eq!(request.validate().is_ok(), global);
        }
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "audio", "auto", "--global"])
                .is_ok()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "audio", "auto", "--global", "--pid", "1"])
                .is_err()
        );
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "audio", "auto", "--global", "--player", "1"])
                .is_err()
        );
    }
}
