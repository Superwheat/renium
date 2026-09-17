use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use crate::app::setup::{PLUGIN_ASSET_NAME, roblox_plugins_dir};

pub(crate) struct StudioProcess {
    pub(crate) pid: u32,
    pub(crate) started_unix: Option<u64>,
    pub(crate) title: Option<String>,
}

#[cfg(windows)]
pub(crate) fn studio_processes() -> Vec<StudioProcess> {
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
        if String::from_utf16_lossy(&entry.szExeFile[..length])
            .eq_ignore_ascii_case("RobloxStudioBeta.exe")
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
                title: crate::studio::input::studio_window_title(pid).ok(),
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
pub(crate) fn studio_processes() -> Vec<StudioProcess> {
    use std::time::SystemTime;

    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,etimes=,comm="])
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
            parts.next().and_then(|value| value.parse::<u64>().ok()),
        ) else {
            continue;
        };
        let command = parts.collect::<Vec<_>>().join(" ");
        if command.contains("RobloxStudio") && !command.contains("Helper") {
            processes.push(StudioProcess {
                pid,
                started_unix: Some(now.saturating_sub(elapsed)),
                title: crate::studio::input::studio_window_title(pid).ok(),
            });
        }
    }
    processes
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn studio_processes() -> Vec<StudioProcess> {
    Vec::new()
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
) -> String {
    if !studio_running {
        return "Studio is not running; open the place with `rbx so FILE` or `rbx ro`.".to_string();
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
    let verdict = verdict_text(
        !processes.is_empty(),
        plugin.as_ref(),
        earliest_start,
        !clients.is_empty(),
        &open_places,
        bound.as_deref(),
    );
    json!({
        "studioProcesses": processes.iter().map(|process| json!({
            "pid": process.pid,
            "startedUnix": process.started_unix,
            "title": process.title,
        })).collect::<Vec<_>>(),
        "plugin": plugin.as_ref().map(|(path, modified)| json!({"path": path, "modifiedUnix": modified})),
        "openPlaces": open_places,
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
    use super::verdict_text;

    fn plugin(modified: Option<u64>) -> (String, Option<u64>) {
        ("plugins/Renium.rbxm".to_string(), modified)
    }

    #[test]
    fn verdict_names_the_blocking_condition() {
        assert!(verdict_text(false, None, None, false, &[], None).contains("not running"));
        assert!(
            verdict_text(true, Some(&plugin(None)), Some(10), false, &[], None)
                .contains("rbx setup")
        );
        assert!(
            verdict_text(true, Some(&plugin(Some(20))), Some(10), false, &[], None)
                .contains("restart Studio")
        );
        assert!(
            verdict_text(true, Some(&plugin(Some(5))), Some(10), false, &[], None)
                .contains("no Renium plugin has connected")
        );
        assert!(
            verdict_text(true, Some(&plugin(Some(5))), Some(10), true, &[], None)
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
        );
        assert!(mismatch.contains("Other") && mismatch.contains("E:/place.rbxl"));
    }
}
