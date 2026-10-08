//! The server and client processes of Studio multi-client tests, recognised by
//! their `-task StartServer|StartClient` command line, and the ones whose Edit
//! window is gone.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::studio::bridge::{BRIDGE_ROLE_EDIT, BRIDGE_ROLE_PLAY_CLIENT, BRIDGE_ROLE_PLAY_SERVER};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StudioProcess {
    pub(crate) pid: u32,
    pub(crate) parent: u32,
    pub(crate) studio: bool,
    pub(crate) test_role: Option<&'static str>,
    pub(crate) parent_reused: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Orphan {
    pub(crate) pid: u32,
    pub(crate) role: String,
    pub(crate) client: Option<Value>,
}

impl Orphan {
    pub(crate) fn runtime_id(&self) -> Option<&str> {
        self.client.as_ref()?.get("runtimeId")?.as_str()
    }

    pub(crate) fn summary(&self) -> Value {
        let mut summary = Map::new();
        summary.insert("pid".into(), json!(self.pid));
        summary.insert("role".into(), json!(self.role));
        if let Some(client) = &self.client {
            for key in ["playerName", "placeName", "placeId"] {
                if let Some(value) = client
                    .get(key)
                    .filter(|value| !value.is_null() && value.as_str() != Some(""))
                {
                    summary.insert(key.into(), value.clone());
                }
            }
        }
        summary.insert("connected".into(), json!(self.client.is_some()));
        Value::Object(summary)
    }
}

pub(crate) fn test_role_from_command_line(command_line: &str) -> Option<&'static str> {
    let mut tokens = command_line
        .split_whitespace()
        .map(|token| token.trim_matches('"'));
    while let Some(token) = tokens.next() {
        if token.eq_ignore_ascii_case("-task") || token.eq_ignore_ascii_case("--task") {
            return match tokens.next() {
                Some(task) if task.eq_ignore_ascii_case("StartServer") => {
                    Some(BRIDGE_ROLE_PLAY_SERVER)
                }
                Some(task) if task.eq_ignore_ascii_case("StartClient") => {
                    Some(BRIDGE_ROLE_PLAY_CLIENT)
                }
                _ => None,
            };
        }
    }
    None
}

#[cfg(windows)]
pub(crate) fn studio_process_table() -> Vec<StudioProcess> {
    let Ok(entries) = crate::studio::performance::process_entries() else {
        return Vec::new();
    };
    let started = |pid| {
        crate::app::update::process_start_identity(pid).and_then(|ticks| ticks.parse::<u64>().ok())
    };
    entries
        .into_iter()
        .map(|entry| {
            let test_role = if entry.studio {
                crate::studio::performance::process_command_line(entry.pid)
                    .as_deref()
                    .and_then(test_role_from_command_line)
            } else {
                None
            };
            // Windows keeps the id of an exited parent, and a later process can
            // reuse it; a parent younger than its child is such a reuse.
            let parent_reused = test_role.is_some()
                && matches!(
                    (started(entry.parent), started(entry.pid)),
                    (Some(parent), Some(own)) if parent > own
                );
            StudioProcess {
                pid: entry.pid,
                parent: entry.parent,
                studio: entry.studio,
                test_role,
                parent_reused,
            }
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn studio_process_table() -> Vec<StudioProcess> {
    let run = |args: &[&str]| {
        std::process::Command::new("ps")
            .args(args)
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let Some(listing) = run(&["-axo", "pid=,ppid=,comm="]) else {
        return Vec::new();
    };
    let mut processes = parse_ps_processes(&listing);
    let studio = processes
        .iter()
        .filter(|process| process.studio)
        .map(|process| process.pid.to_string())
        .collect::<Vec<_>>();
    if !studio.is_empty()
        && let Some(commands) = run(&["-ww", "-o", "pid=,args=", "-p", &studio.join(",")])
    {
        apply_ps_command_lines(&mut processes, &commands);
    }
    processes
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn studio_process_table() -> Vec<StudioProcess> {
    Vec::new()
}

fn split_pid(line: &str) -> Option<(u32, &str)> {
    let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
    Some((pid.parse().ok()?, rest.trim_start()))
}

/// `ps -o pid=,ppid=,comm=` lines; comm is the executable path, as in
/// `studio::diagnosis`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_ps_processes(text: &str) -> Vec<StudioProcess> {
    text.lines()
        .filter_map(|line| {
            let (pid, rest) = split_pid(line)?;
            let (parent, command) = match split_pid(rest) {
                Some(found) => found,
                None => (rest.trim().parse().ok()?, ""),
            };
            Some(StudioProcess {
                pid,
                parent,
                studio: matches!(
                    command.trim_end().rsplit('/').next(),
                    Some("RobloxStudio" | "RobloxStudio.bin")
                ),
                test_role: None,
                parent_reused: false,
            })
        })
        .collect()
}

/// `ps -o pid=,args=` lines for the Studio processes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn apply_ps_command_lines(processes: &mut [StudioProcess], text: &str) {
    let roles = text
        .lines()
        .filter_map(split_pid)
        .map(|(pid, args)| (pid, test_role_from_command_line(args)))
        .collect::<HashMap<_, _>>();
    for process in processes.iter_mut().filter(|process| process.studio) {
        process.test_role = roles.get(&process.pid).copied().flatten();
    }
}

/// Studio test processes started from `root`, a Studio Edit window.
pub(crate) fn test_descendants(processes: &[StudioProcess], root: u32) -> Vec<u32> {
    studio_descendants(processes, root)
        .into_iter()
        .filter(|process| process.test_role.is_some())
        .map(|process| process.pid)
        .collect()
}

pub(crate) fn studio_descendants(processes: &[StudioProcess], root: u32) -> Vec<&StudioProcess> {
    let mut children = HashMap::<u32, Vec<&StudioProcess>>::new();
    for process in processes {
        if process.pid != process.parent {
            children.entry(process.parent).or_default().push(process);
        }
    }
    let mut result = Vec::new();
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(pid) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        for child in children.get(&pid).into_iter().flatten() {
            if child.studio {
                result.push(*child);
            }
            pending.push(child.pid);
        }
    }
    result
}

// Walks up through live processes; a test process belongs to whichever Studio
// that is not itself a test process it reaches first. Reaching a missing or
// reused parent means the Edit window that launched it has exited; macOS hands
// such children to launchd, which leads to the same answer.
fn launching_edit_is_gone(by_pid: &HashMap<u32, &StudioProcess>, process: &StudioProcess) -> bool {
    let mut current = process;
    for _ in 0..32 {
        if current.parent_reused {
            return true;
        }
        match by_pid.get(&current.parent).copied() {
            None => return true,
            Some(ancestor) if ancestor.studio && ancestor.test_role.is_none() => return false,
            Some(ancestor) if ancestor.pid == ancestor.parent => return true,
            Some(ancestor) => current = ancestor,
        }
    }
    false
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Test processes whose launching Edit window has exited and whose launching
/// Edit runtime is not connected. Edit windows are never included.
pub(crate) fn orphan_test_processes(processes: &[StudioProcess], clients: &[Value]) -> Vec<Orphan> {
    let by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let edits = clients
        .iter()
        .filter(|client| client["role"] == BRIDGE_ROLE_EDIT);
    let edit_runtimes = edits
        .clone()
        .filter_map(|client| string_field(client, "runtimeId"))
        .collect::<HashSet<_>>();
    let edit_pids = edits
        .filter_map(|client| client.get("pid").and_then(Value::as_u64))
        .collect::<HashSet<_>>();
    let mut orphans = Vec::new();
    for process in processes {
        let Some(test_role) = process.test_role.filter(|_| process.studio) else {
            continue;
        };
        if edit_pids.contains(&u64::from(process.pid)) || !launching_edit_is_gone(&by_pid, process)
        {
            continue;
        }
        let bridged = clients
            .iter()
            .filter(|client| {
                client.get("pid").and_then(Value::as_u64) == Some(u64::from(process.pid))
                    && matches!(
                        client["role"].as_str(),
                        Some(BRIDGE_ROLE_PLAY_SERVER | BRIDGE_ROLE_PLAY_CLIENT)
                    )
            })
            .collect::<Vec<_>>();
        if bridged.iter().any(|client| {
            string_field(client, "launchEditRuntimeId").is_some_and(|id| edit_runtimes.contains(id))
        }) {
            continue;
        }
        let client = bridged.first().map(|client| (*client).clone());
        orphans.push(Orphan {
            pid: process.pid,
            role: client
                .as_ref()
                .and_then(|client| string_field(client, "role"))
                .unwrap_or(test_role)
                .to_string(),
            client,
        });
    }
    orphans.sort_by_key(|orphan| orphan.pid);
    orphans
}

/// Whether a test runtime belongs to the same place as an Edit runtime: the
/// same published place, or for local files the same file name.
pub(crate) fn same_place(client: &Value, edit: &Value) -> bool {
    let id = |value: &Value, key: &str| value.get(key).and_then(Value::as_i64).filter(|id| *id > 0);
    match id(edit, "placeId") {
        Some(place) => {
            id(client, "placeId") == Some(place)
                && match (id(client, "gameId"), id(edit, "gameId")) {
                    (Some(client_game), Some(edit_game)) => client_game == edit_game,
                    _ => true,
                }
        }
        None => {
            id(client, "placeId").is_none()
                && string_field(client, "placeName").is_some_and(|name| {
                    string_field(edit, "placeName")
                        .is_some_and(|edit_name| edit_name.eq_ignore_ascii_case(name))
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent: u32, test_role: Option<&'static str>) -> StudioProcess {
        StudioProcess {
            pid,
            parent,
            studio: true,
            test_role,
            parent_reused: false,
        }
    }

    fn other(pid: u32, parent: u32) -> StudioProcess {
        StudioProcess {
            pid,
            parent,
            studio: false,
            test_role: None,
            parent_reused: false,
        }
    }

    #[test]
    fn command_lines_name_the_test_role() {
        assert_eq!(
            test_role_from_command_line(
                r#""C:\Roblox\Versions\v\RobloxStudioBeta.exe" -task StartServer -studiotestservicemode -placeId 1"#
            ),
            Some(BRIDGE_ROLE_PLAY_SERVER)
        );
        assert_eq!(
            test_role_from_command_line(
                "RobloxStudioBeta.exe -task startclient -studiotestservicemode"
            ),
            Some(BRIDGE_ROLE_PLAY_CLIENT)
        );
        for edit in [
            r#""C:\Roblox\RobloxStudioBeta.exe" -task EditFile -localPlaceFile "E:\a b\x.rbxl""#,
            "RobloxStudioBeta.exe",
            "RobloxStudioBeta.exe -task",
            r#"RobloxStudioBeta.exe -localPlaceFile "E:\StartServer.rbxl""#,
        ] {
            assert_eq!(test_role_from_command_line(edit), None, "{edit}");
        }
    }

    #[test]
    fn mac_process_lines_keep_paths_and_roles() {
        let mut table = parse_ps_processes(
            "  1     0 /sbin/launchd\n\
             500     1 /Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio\n\
             501   500 /Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio\n\
             502   501 /Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio\n\
             503     1 /usr/bin/grep\n\
             504     1 /Applications/My Tools/RobloxStudio Notes\n",
        );
        apply_ps_command_lines(
            &mut table,
            "500 /Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio roblox-studio:1+launchmode:edit\n\
             501 /Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio -task StartServer -studiotestservicemode\n\
             502 /Applications/RobloxStudio.app/Contents/MacOS/RobloxStudio -task StartClient\n\
             503 /usr/bin/grep -task StartServer\n",
        );
        assert_eq!(
            table,
            vec![
                other(1, 0),
                process(500, 1, None),
                process(501, 500, Some(BRIDGE_ROLE_PLAY_SERVER)),
                process(502, 501, Some(BRIDGE_ROLE_PLAY_CLIENT)),
                other(503, 1),
                other(504, 1),
            ]
        );
    }

    #[test]
    fn descendants_list_only_test_processes_of_that_window() {
        let processes = [
            other(4, 0),
            process(10, 4, None),
            process(11, 10, Some(BRIDGE_ROLE_PLAY_SERVER)),
            process(12, 11, Some(BRIDGE_ROLE_PLAY_CLIENT)),
            other(13, 10),
            process(14, 13, Some(BRIDGE_ROLE_PLAY_CLIENT)),
            process(15, 10, None),
            process(20, 4, None),
            process(21, 20, Some(BRIDGE_ROLE_PLAY_SERVER)),
        ];
        let mut found = test_descendants(&processes, 10);
        found.sort_unstable();
        assert_eq!(found, [11, 12, 14]);
        assert_eq!(studio_descendants(&processes, 10).len(), 4);
        assert_eq!(test_descendants(&processes, 20), [21]);
    }

    fn client(role: &str, runtime: &str, pid: u32, launch_edit: &str) -> Value {
        json!({
            "role": role,
            "runtimeId": runtime,
            "pid": pid,
            "launchEditRuntimeId": launch_edit,
            "placeId": 7,
            "gameId": 3,
            "placeName": "DTE",
            "playerName": if role == BRIDGE_ROLE_PLAY_CLIENT { "Player1" } else { "" },
        })
    }

    #[test]
    fn orphans_are_test_processes_of_an_exited_edit_window() {
        let processes = [
            other(4, 0),
            // A restarted Edit window: the old one (pid 10) has exited.
            process(30, 4, None),
            process(11, 10, Some(BRIDGE_ROLE_PLAY_SERVER)),
            process(12, 11, Some(BRIDGE_ROLE_PLAY_CLIENT)),
            // A live window's own test, and a test whose window lost only its plugin.
            process(20, 4, None),
            process(21, 20, Some(BRIDGE_ROLE_PLAY_SERVER)),
            process(22, 21, Some(BRIDGE_ROLE_PLAY_CLIENT)),
            // A kicked client whose server and window are both gone, without a bridge.
            process(40, 39, Some(BRIDGE_ROLE_PLAY_CLIENT)),
        ];
        let clients = [
            client(BRIDGE_ROLE_EDIT, "edit-new", 30, ""),
            client(BRIDGE_ROLE_PLAY_SERVER, "server-old", 11, "edit-old"),
            client(BRIDGE_ROLE_PLAY_CLIENT, "client-old", 12, "edit-old"),
            client(BRIDGE_ROLE_PLAY_SERVER, "server-live", 21, "edit-reloaded"),
            client(BRIDGE_ROLE_PLAY_CLIENT, "client-live", 22, "edit-reloaded"),
        ];
        let orphans = orphan_test_processes(&processes, &clients);
        assert_eq!(
            orphans.iter().map(|orphan| orphan.pid).collect::<Vec<_>>(),
            [11, 12, 40]
        );
        assert_eq!(orphans[0].runtime_id(), Some("server-old"));
        assert_eq!(orphans[1].role, BRIDGE_ROLE_PLAY_CLIENT);
        assert_eq!(
            orphans[2].summary(),
            json!({"pid": 40, "role": "play-client", "connected": false})
        );
        assert_eq!(
            orphans[1].summary(),
            json!({"pid": 12, "role": "play-client", "playerName": "Player1", "placeName": "DTE", "placeId": 7, "connected": true})
        );
    }

    #[test]
    fn a_connected_edit_runtime_or_edit_process_keeps_its_tests() {
        let processes = [
            process(11, 10, Some(BRIDGE_ROLE_PLAY_SERVER)),
            process(12, 11, Some(BRIDGE_ROLE_PLAY_CLIENT)),
            process(50, 1, Some(BRIDGE_ROLE_PLAY_SERVER)),
        ];
        let clients = [
            client(BRIDGE_ROLE_EDIT, "edit-old", 99, ""),
            client(BRIDGE_ROLE_PLAY_SERVER, "server-old", 11, "edit-old"),
            client(BRIDGE_ROLE_PLAY_CLIENT, "client-old", 12, "edit-old"),
            client(BRIDGE_ROLE_EDIT, "edit-in-50", 50, ""),
        ];
        assert!(orphan_test_processes(&processes, &clients).is_empty());
        let unknown_parent = [process(60, 60, Some(BRIDGE_ROLE_PLAY_SERVER))];
        assert_eq!(orphan_test_processes(&unknown_parent, &[]).len(), 1);
        let edit_without_role = [process(70, 1, None), other(1, 0)];
        assert!(orphan_test_processes(&edit_without_role, &[]).is_empty());
    }

    #[test]
    fn a_reused_parent_id_does_not_adopt_a_test_of_an_exited_window() {
        // The exited Edit window's id 10 now belongs to a WebView2 helper of a
        // newer Studio window.
        let mut server = process(11, 10, Some(BRIDGE_ROLE_PLAY_SERVER));
        server.parent_reused = true;
        let processes = [
            process(30, 4, None),
            other(10, 30),
            server,
            process(12, 11, Some(BRIDGE_ROLE_PLAY_CLIENT)),
        ];
        let orphans = orphan_test_processes(&processes, &[]);
        assert_eq!(
            orphans.iter().map(|orphan| orphan.pid).collect::<Vec<_>>(),
            [11, 12]
        );
        let mut adopted = processes.clone();
        adopted[2].parent_reused = false;
        assert!(orphan_test_processes(&adopted, &[]).is_empty());
    }

    #[test]
    fn same_place_matches_published_ids_or_local_file_names() {
        let edit = json!({"placeId": 7, "gameId": 3, "placeName": "DTE"});
        assert!(same_place(&json!({"placeId": 7, "gameId": 3}), &edit));
        assert!(same_place(&json!({"placeId": 7}), &edit));
        assert!(!same_place(&json!({"placeId": 7, "gameId": 4}), &edit));
        assert!(!same_place(
            &json!({"placeId": 8, "placeName": "DTE"}),
            &edit
        ));
        let local = json!({"placeId": 0, "placeName": "scratch.rbxl"});
        assert!(same_place(
            &json!({"placeId": 0, "placeName": "Scratch.rbxl"}),
            &local
        ));
        assert!(!same_place(
            &json!({"placeId": 0, "placeName": "Place1"}),
            &local
        ));
        assert!(!same_place(
            &json!({"placeId": 7, "placeName": "scratch.rbxl"}),
            &local
        ));
        assert!(!same_place(&json!({}), &local));
    }

    #[cfg(windows)]
    #[test]
    fn reads_a_live_process_command_line() {
        let command_line = crate::studio::performance::process_command_line(std::process::id())
            .expect("own command line");
        let executable = std::env::current_exe().unwrap();
        let name = executable
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(command_line.contains(&name), "{command_line}");
        assert!(
            crate::studio::performance::process_entries()
                .unwrap()
                .iter()
                .any(|entry| entry.pid == std::process::id() && !entry.studio)
        );
    }
}
