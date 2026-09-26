use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use crate::app::setup::{PLUGIN_ASSET_NAME, roblox_plugins_dir};

pub(crate) struct StudioProcess {
    pub(crate) pid: u32,
    pub(crate) started_unix: Option<u64>,
    pub(crate) title: Option<String>,
}

#[cfg(windows)]
fn process_list(titles: bool) -> Vec<StudioProcess> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let mut processes = Vec::new();
    // SAFETY: CreateToolhelp32Snapshot returns a handle that is checked below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return processes;
    }
    // SAFETY: all-zero is a valid initial PROCESSENTRY32W before dwSize is set.
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: snapshot and entry are valid for process enumeration.
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while has_entry {
        let length = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
        if name.eq_ignore_ascii_case("RobloxStudioBeta.exe")
            || name.eq_ignore_ascii_case("RobloxStudio.exe")
        {
            let pid = entry.th32ProcessID;
            // SAFETY: OpenProcess returns a new handle or null; it is closed below.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            let mut started_unix = None;
            if !handle.is_null() {
                let mut creation = FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                };
                let mut other = [
                    FILETIME {
                        dwLowDateTime: 0,
                        dwHighDateTime: 0,
                    },
                    FILETIME {
                        dwLowDateTime: 0,
                        dwHighDateTime: 0,
                    },
                    FILETIME {
                        dwLowDateTime: 0,
                        dwHighDateTime: 0,
                    },
                ];
                // SAFETY: the handle is valid and every out-pointer refers to a live FILETIME.
                if unsafe {
                    GetProcessTimes(
                        handle,
                        &mut creation,
                        &mut other[0],
                        &mut other[1],
                        &mut other[2],
                    )
                } != 0
                {
                    let ticks = (u64::from(creation.dwHighDateTime) << 32)
                        | u64::from(creation.dwLowDateTime);
                    started_unix = ticks
                        .checked_sub(116_444_736_000_000_000)
                        .map(|since_epoch| since_epoch / 10_000_000);
                }
                // SAFETY: the handle came from OpenProcess above.
                unsafe { CloseHandle(handle) };
            }
            processes.push(StudioProcess {
                pid,
                started_unix,
                title: titles
                    .then(|| crate::studio::input::studio_window_title(pid).ok())
                    .flatten(),
            });
        }
        // SAFETY: snapshot and entry remain valid for the next enumeration call.
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: the snapshot handle is owned by this function.
    unsafe { CloseHandle(snapshot) };
    processes
}

#[cfg(target_os = "macos")]
fn process_list(titles: bool) -> Vec<StudioProcess> {
    use std::time::SystemTime;

    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,etime=,comm="])
        .output()
    else {
        return Vec::new();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let mut processes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(elapsed)) = (
            parts.next().and_then(|value| value.parse::<u32>().ok()),
            parts.next().and_then(elapsed_seconds),
        ) else {
            continue;
        };
        let command = parts.collect::<Vec<_>>().join(" ");
        if matches!(
            command.rsplit('/').next(),
            Some("RobloxStudio" | "RobloxStudio.bin")
        ) {
            processes.push(StudioProcess {
                pid,
                started_unix: Some(now.saturating_sub(elapsed)),
                title: titles
                    .then(|| crate::studio::input::studio_window_title(pid).ok())
                    .flatten(),
            });
        }
    }
    processes
}

#[cfg(not(any(windows, target_os = "macos")))]
fn process_list(_titles: bool) -> Vec<StudioProcess> {
    Vec::new()
}

pub(crate) fn studio_processes() -> Vec<StudioProcess> {
    process_list(true)
}

pub(crate) fn studio_process_started_unix(pid: u32) -> Option<u64> {
    process_list(false)
        .into_iter()
        .find(|process| process.pid == pid)
        .and_then(|process| process.started_unix)
}

pub(crate) fn studio_process_ids() -> Vec<u32> {
    process_list(false)
        .into_iter()
        .map(|process| process.pid)
        .collect()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn elapsed_seconds(text: &str) -> Option<u64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut seconds = 0u64;
    for part in clock.split(':') {
        seconds = seconds
            .checked_mul(60)?
            .checked_add(part.parse::<u64>().ok()?)?;
    }
    Some(days * 86_400 + seconds)
}

pub(crate) fn studio_log_directory() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Roblox").join("logs"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Logs")
                .join("Roblox")
        })
    } else {
        None
    }
}

// Studio names its process near the top of each log, for example
// "Constructing UIThreadNotifier for process '4700' ...".
const LOG_HEAD_BYTES: u64 = 256 * 1024;

pub(crate) fn studio_log_for_process(pid: u32, started_unix: Option<u64>) -> Option<PathBuf> {
    let marker = format!("for process '{pid}'");
    let mut newest: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(studio_log_directory()?).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.contains("Studio") || !name.ends_with(".log") {
            continue;
        }
        let created = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.created().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_secs());
        // A reused PID must not match the log of an older Studio.
        if started_unix.is_some_and(|started| created + 300 < started) {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut head = Vec::new();
        if file.take(LOG_HEAD_BYTES).read_to_end(&mut head).is_err()
            || !String::from_utf8_lossy(&head).contains(&marker)
        {
            continue;
        }
        if newest.as_ref().is_none_or(|(time, _)| created >= *time) {
            newest = Some((created, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Why the latest place open of this Studio failed, read from its log, or None
/// when it opened a place or has not finished trying.
pub(crate) fn studio_open_failure(pid: u32, started_unix: Option<u64>) -> Option<String> {
    let bytes = std::fs::read(studio_log_for_process(pid, started_unix)?).ok()?;
    open_failure_in_log(&String::from_utf8_lossy(&bytes))
}

fn open_failure_in_log(text: &str) -> Option<String> {
    let mut failure = None;
    let mut awaiting_message = false;
    for line in text.lines() {
        if line.contains("[telemetryLog] State: OpenPlaceSuccess") {
            failure = None;
            awaiting_message = false;
        } else if line.contains("[telemetryLog] State: OpenPlaceFailure") {
            failure = Some("Studio could not open the place".to_string());
            awaiting_message = true;
        } else if awaiting_message
            && let Some((_, message)) = line.split_once("[telemetryLog] ErrorMessage:")
        {
            let message = message.trim();
            if !message.is_empty() {
                failure = Some(message.to_string());
            }
            awaiting_message = false;
        }
    }
    failure
}

fn plugin_file() -> Option<(String, Option<u64>)> {
    let path = roblox_plugins_dir().ok()?.join(PLUGIN_ASSET_NAME);
    let modified = std::fs::metadata(&path).ok().and_then(|metadata| {
        metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|value| value.as_secs())
    });
    Some((path.display().to_string(), modified))
}

fn bound_target(project_root: Option<&Path>) -> Option<String> {
    let target = crate::automation::live::saved_studio_target_for_root(project_root?).ok()??;
    match (target.game_id, target.place_id, target.file) {
        (Some(game), Some(place), _) => Some(format!("published place {place} in game {game}")),
        (_, _, Some(file)) => Some(file.display().to_string()),
        _ => None,
    }
}

fn verdict_text(
    studio_running: bool,
    plugin: Option<&(String, Option<u64>)>,
    earliest_start: Option<u64>,
    clients_connected: bool,
    open_places: &[String],
    bound: Option<&str>,
    open_failures: &[(u32, String)],
) -> String {
    if !studio_running {
        return "Studio is not running; open the place with `rbx so FILE` or `rbx ro`.".to_string();
    }
    if let Some((pid, reason)) = open_failures.first() {
        return format!(
            "Studio (pid {pid}) could not open its place: {}. It is left at its start page; close it, and check that the signed-in Studio account can edit the place.",
            reason.trim_end_matches('.')
        );
    }
    if let Some((path, None)) = plugin {
        return format!(
            "The Renium Studio plugin is missing from {path}; run `rbx setup` to install it, then restart Studio."
        );
    }
    if let (Some((_, Some(modified))), Some(started)) = (plugin, earliest_start)
        && *modified > started
        && !clients_connected
    {
        return "The Renium plugin file changed after Studio started; restart Studio to load it."
            .to_string();
    }
    if !clients_connected {
        return "Studio is running with the plugin installed, but no Renium plugin has connected. In Studio, check the Renium panel's connect setting and that the plugin is enabled in the Plugins Manager.".to_string();
    }
    if open_places.is_empty() {
        return "Only Play session clients are connected; an Edit session is needed.".to_string();
    }
    match bound {
        Some(bound) => format!(
            "Studio has {} open, but this project is bound to {bound}; open that place or run from its project.",
            open_places.join(", ")
        ),
        None => format!(
            "Studio has {} open, but no connected Studio matches this project; open the project's place or pass --place.",
            open_places.join(", ")
        ),
    }
}

pub(crate) fn diagnose(clients: &[Value], project_root: Option<&Path>) -> Value {
    let processes = studio_processes();
    let plugin = plugin_file();
    let earliest_start = processes
        .iter()
        .filter_map(|process| process.started_unix)
        .min();
    let bound = bound_target(project_root);
    let open_places = clients
        .iter()
        .filter(|client| client["role"] == "edit")
        .filter_map(|client| client["placeName"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let connected_pids = clients
        .iter()
        .filter_map(|client| client["pid"].as_u64())
        .collect::<Vec<_>>();
    let open_failures = processes
        .iter()
        .filter(|process| !connected_pids.contains(&u64::from(process.pid)))
        .filter_map(|process| {
            studio_open_failure(process.pid, process.started_unix)
                .map(|reason| (process.pid, reason))
        })
        .collect::<Vec<_>>();
    let verdict = verdict_text(
        !processes.is_empty(),
        plugin.as_ref(),
        earliest_start,
        !clients.is_empty(),
        &open_places,
        bound.as_deref(),
        &open_failures,
    );
    json!({
        "studioProcesses": processes.iter().map(|process| json!({
            "pid": process.pid,
            "startedUnix": process.started_unix,
            "title": process.title,
        })).collect::<Vec<_>>(),
        "plugin": plugin.as_ref().map(|(path, modified)| json!({"path": path, "modifiedUnix": modified})),
        "openPlaces": open_places,
        "openFailures": open_failures.iter().map(|(pid, reason)| json!({"pid": pid, "reason": reason})).collect::<Vec<_>>(),
        "boundTarget": bound,
        "verdict": verdict,
    })
}

pub(crate) fn verdict(clients: &[Value], project_root: Option<&Path>) -> String {
    diagnose(clients, project_root)["verdict"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{elapsed_seconds, open_failure_in_log, verdict_text};

    #[test]
    fn elapsed_seconds_reads_ps_etime() {
        assert_eq!(elapsed_seconds("05"), Some(5));
        assert_eq!(elapsed_seconds("01:05"), Some(65));
        assert_eq!(elapsed_seconds("02:00:01"), Some(7_201));
        assert_eq!(elapsed_seconds("02-14:51:04"), Some(226_264));
        assert_eq!(elapsed_seconds("x"), None);
    }

    fn plugin(modified: Option<u64>) -> (String, Option<u64>) {
        ("plugins/Renium.rbxm".to_string(), modified)
    }

    #[test]
    fn verdict_names_the_blocking_condition() {
        assert!(verdict_text(false, None, None, false, &[], None, &[]).contains("not running"));
        assert!(
            verdict_text(true, Some(&plugin(None)), Some(10), false, &[], None, &[])
                .contains("rbx setup")
        );
        assert!(
            verdict_text(
                true,
                Some(&plugin(Some(20))),
                Some(10),
                false,
                &[],
                None,
                &[]
            )
            .contains("restart Studio")
        );
        assert!(
            verdict_text(
                true,
                Some(&plugin(Some(5))),
                Some(10),
                false,
                &[],
                None,
                &[]
            )
            .contains("no Renium plugin has connected")
        );
        assert!(
            verdict_text(true, Some(&plugin(Some(5))), Some(10), true, &[], None, &[])
                .contains("Edit session")
        );
        let open = ["Other".to_string()];
        let mismatch = verdict_text(
            true,
            Some(&plugin(Some(20))),
            Some(10),
            true,
            &open,
            Some("E:/place.rbxl"),
            &[],
        );
        assert!(mismatch.contains("Other") && mismatch.contains("E:/place.rbxl"));
        let refused = verdict_text(
            true,
            Some(&plugin(Some(5))),
            Some(10),
            false,
            &[],
            None,
            &[(4700, "User is not authorized to access Asset.".to_string())],
        );
        assert!(refused.contains("4700") && refused.contains("not authorized"));
    }

    #[test]
    fn the_latest_open_attempt_decides_the_failure() {
        let failed = "a,b,c,6 [telemetryLog] State: OpenPlaceLoadDataModel\r\n\
            a,b,c,6 [telemetryLog] State: OpenPlaceFailure\r\n\
            a,b,c,6 [telemetryLog] ErrorType: DataModelLoadingFailure\r\n\
            a,b,c,6 [telemetryLog] ErrorMessage: User is not authorized to access Asset.\r\n";
        assert_eq!(
            open_failure_in_log(failed).as_deref(),
            Some("User is not authorized to access Asset.")
        );
        let recovered = format!("{failed}a,b,c,6 [telemetryLog] State: OpenPlaceSuccess\n");
        assert_eq!(open_failure_in_log(&recovered), None);
        assert_eq!(
            open_failure_in_log("x [telemetryLog] State: OpenPlaceFailure\n").as_deref(),
            Some("Studio could not open the place")
        );
        assert_eq!(
            open_failure_in_log("x [telemetryLog] State: PlaceIdle\n"),
            None
        );
    }
}
