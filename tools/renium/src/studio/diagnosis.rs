use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

#[cfg(any(windows, target_os = "macos"))]
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
/// The process marker sits in a Studio log's first lines.
const LOG_HEAD_BYTES: u64 = 8 * 1024;

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
        // A reused PID must not match the log of an older Studio, and a log
        // created long after this process started belongs to a later one.
        if started_unix.is_some_and(|started| created + 300 < started || created > started + 120) {
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
/// The startup part of a Studio log: every state the diagnosis reads (place
/// open, hang, sign-in) is logged in the first megabyte, and a long session's
/// log runs to tens of megabytes.
const LOG_STARTUP_BYTES: u64 = 1024 * 1024;

fn studio_log_startup(pid: u32, started_unix: Option<u64>) -> Option<String> {
    let file = std::fs::File::open(studio_log_for_process(pid, started_unix)?).ok()?;
    let mut head = Vec::new();
    file.take(LOG_STARTUP_BYTES).read_to_end(&mut head).ok()?;
    Some(String::from_utf8_lossy(&head).into_owned())
}

pub(crate) fn studio_open_failure(pid: u32, started_unix: Option<u64>) -> Option<String> {
    open_failure_in_log(&studio_log_startup(pid, started_unix)?)
}

/// A Studio that opened the Roblox sign-in dialog instead of its place: the
/// account's Studio session ended (Studio clears its cookies when a network
/// failure interrupts the sign-in check), and only the user can sign in again.
pub(crate) fn studio_signed_out(pid: u32, started_unix: Option<u64>) -> bool {
    studio_log_startup(pid, started_unix).is_some_and(|text| signed_out_in_log(&text))
}

fn signed_out_in_log(text: &str) -> bool {
    text.lines()
        .any(|line| line.contains("[FLog::StudioKeyEvents] show login dialog"))
}

/// A Studio whose own hang monitor escalated a main-thread hang before it
/// opened a place, read from its log. Studio 0.741 hangs windows opened while
/// another Studio window is running, usually before the automatic sign-in
/// finishes: the first window keeps the sign-in mutex locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartupHang {
    pub(crate) instances_at_launch: u32,
    pub(crate) signed_in: bool,
}

pub(crate) fn studio_startup_hang(pid: u32, started_unix: Option<u64>) -> Option<StartupHang> {
    startup_hang_in_log(&studio_log_startup(pid, started_unix)?)
}

fn startup_hang_in_log(text: &str) -> Option<StartupHang> {
    let mut hang = None;
    let mut instances_at_launch = 1;
    let mut signed_in = false;
    for line in text.lines() {
        if let Some((_, count)) = line.split_once("Running instance count at launch ") {
            instances_at_launch = count.trim().parse().unwrap_or(1);
        } else if line.contains("[FLog::LoginController] Login got Standalone DM ready") {
            signed_in = true;
        } else if line.contains("[FLog::StudioHangMonitor] Hang Detected") {
            hang = Some(StartupHang {
                instances_at_launch,
                signed_in,
            });
        } else if line.contains("[telemetryLog] State: OpenPlaceSuccess") {
            hang = None;
        }
    }
    hang
}

/// The macOS permission prompts on screen that concern Studio.
#[derive(Default)]
struct PermissionPrompts {
    /// The question of each prompt that names Studio.
    named: Vec<String>,
    /// Prompts whose text could not be read.
    unread: usize,
    /// Whether they stayed unread because Accessibility is off for the app
    /// that started Renium.
    accessibility_off: bool,
}

/// macOS asks before Studio may read the Documents folder, where its plugins
/// and settings live, and asks again when Studio's signature changes: after
/// every Studio update, once Renium has prepared the new version. A Studio
/// that needs the answer opens no place until someone gives it.
#[cfg(target_os = "macos")]
fn studio_permission_prompts() -> PermissionPrompts {
    let mut prompts = sort_system_prompts(crate::studio::input::system_prompts());
    prompts.accessibility_off =
        prompts.unread > 0 && !crate::studio::input::accessibility_granted();
    prompts
}

#[cfg(not(target_os = "macos"))]
fn studio_permission_prompts() -> PermissionPrompts {
    PermissionPrompts::default()
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn sort_system_prompts(prompts: Vec<Option<String>>) -> PermissionPrompts {
    let mut sorted = PermissionPrompts::default();
    for prompt in prompts {
        let Some(text) = prompt else {
            sorted.unread += 1;
            continue;
        };
        if let Some(question) = text.lines().next()
            && asked_by_studio(question)
        {
            sorted.named.push(question.to_string());
        }
    }
    sorted
}

/// Whether a prompt's question is asked on Studio's behalf: macOS opens the
/// question with the name of the app that asks, in quotes.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn asked_by_studio(question: &str) -> bool {
    let name = question
        .trim_start_matches(|character: char| !character.is_alphanumeric())
        .to_lowercase();
    name.starts_with("robloxstudio") || name.starts_with("roblox studio")
}

/// How long a Studio's request to open its place must have stayed pending
/// before a permission prompt on screen counts as what holds it. A launch
/// that proceeds logs the start of the open within a second of asking.
const PROMPT_HOLD_SECONDS: u64 = 20;

/// The second of a Studio's life at which it asked to open its place, when
/// the open never began after that: the state a macOS permission prompt
/// holds it in, read from its log.
fn studio_place_open_pending(pid: u32, started_unix: Option<u64>) -> Option<u64> {
    place_open_pending_in_log(&studio_log_startup(pid, started_unix)?)
}

/// A Studio log line carries the seconds since Studio started as its second
/// field.
fn place_open_pending_in_log(text: &str) -> Option<u64> {
    let mut requested = None;
    for line in text.lines() {
        if line.contains("[FLog::PlaceManager] Start to open place") {
            requested = line
                .split(',')
                .nth(1)
                .and_then(|seconds| seconds.split('.').next())
                .and_then(|seconds| seconds.trim().parse::<u64>().ok());
        } else if line.contains("[FLog::StudioKeyEvents] open place (")
            || line.contains("[telemetryLog] State: OpenPlace")
        {
            requested = None;
        }
    }
    requested
}

/// Whether a place open requested `requested` seconds into the life of a
/// Studio that is now `age` seconds old has been pending for the hold period.
fn pending_for_the_hold_period(age: Option<u64>, requested: Option<u64>) -> bool {
    matches!(
        (age, requested),
        (Some(age), Some(requested)) if age.saturating_sub(requested) >= PROMPT_HOLD_SECONDS
    )
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

/// How long a Studio process may take to open its place and connect the
/// plugin before its silence counts as a problem.
const STARTING_GRACE_SECONDS: u64 = 180;

/// What the daemon can see of Studio: its processes, the installed plugin,
/// the connected clients, and the project's bound place.
struct Observation<'a> {
    studio_running: bool,
    plugin: Option<&'a (String, Option<u64>)>,
    earliest_start: Option<u64>,
    clients_connected: bool,
    open_places: &'a [String],
    bound: Option<&'a str>,
    open_failures: &'a [(u32, String)],
    startup_hangs: &'a [(u32, StartupHang)],
    signed_out: &'a [u32],
    starting: &'a [(u32, u64)],
    prompts: &'a PermissionPrompts,
    prompt_held: &'a [u32],
}

/// The lead of every sentence that quotes a macOS prompt; the scenario
/// runner stops a run on it.
const PROMPT_LEAD: &str = "macOS is asking on this Mac's screen:";

fn held_verdict(pid: u32, question: &str) -> String {
    format!(
        "Studio (pid {pid}) asked to open its place and has not begun, while {PROMPT_LEAD} {question} Only the user can answer that prompt: ask them to, then run the command again. macOS asks again after each Studio update."
    )
}

/// What a prompt on screen may mean for a Studio that has not connected,
/// appended to a verdict that names another cause.
fn prompt_note(prompts: &PermissionPrompts) -> String {
    match prompts.named.first() {
        Some(question) => format!(
            " {PROMPT_LEAD} {question} A Studio that needs that answer does not connect until the user gives it."
        ),
        None if prompts.unread > 0 => format!(
            " macOS is showing a prompt on this Mac's screen that Renium could not read{}; if it asks on RobloxStudio's behalf, a Studio that needs the answer does not connect until the user gives it.",
            if prompts.accessibility_off {
                " (Accessibility is off for the app that started Renium)"
            } else {
                ""
            }
        ),
        None => String::new(),
    }
}

fn verdict_text(observation: &Observation<'_>) -> String {
    let note = prompt_note(observation.prompts);
    if let Some(failure) = failure_verdict(observation) {
        return format!("{failure}{note}");
    }
    if let (Some(pid), Some(question)) = (
        observation.prompt_held.first(),
        observation.prompts.named.first(),
    ) {
        return held_verdict(*pid, question);
    }
    format!("{}{note}", connection_verdict(observation))
}

/// A Studio that is not running, or one that reported its own failure.
fn failure_verdict(observation: &Observation<'_>) -> Option<String> {
    if !observation.studio_running {
        return Some(
            "Studio is not running; open the place with `rbx so FILE` or `rbx ro`.".to_string(),
        );
    }
    if let Some((pid, reason)) = observation.open_failures.first() {
        return Some(format!(
            "Studio (pid {pid}) could not open its place: {}. It is left at its start page; `rbx sx --terminate` closes it, then check that the signed-in Studio account can edit the place and run `rbx so FILE` or `rbx ro` again.",
            reason.trim_end_matches('.')
        ));
    }
    if let Some(pid) = observation.signed_out.first() {
        return Some(format!(
            "Studio (pid {pid}) is signed out and shows the Roblox sign-in dialog instead of its place; sign in to Studio, then run the command again."
        ));
    }
    let (pid, hang) = observation.startup_hangs.first()?;
    let stage = if hang.signed_in {
        "after signing in at launch"
    } else {
        "while signing in at launch"
    };
    let advice = if hang.instances_at_launch > 1 {
        "Studio 0.741 hangs windows opened while another Studio window is running; close this window and open the place again, and if it hangs again close every Studio window first or open the place from the running Studio with File > Open"
    } else {
        "close it and open the place again"
    };
    Some(format!(
        "Studio (pid {pid}) hung {stage} (its own hang monitor reported it) and will not open its place or connect; {advice}."
    ))
}

/// Why a running Studio that reported no failure serves no command of this
/// project.
fn connection_verdict(observation: &Observation<'_>) -> String {
    let Observation {
        plugin,
        earliest_start,
        clients_connected,
        open_places,
        bound,
        starting,
        ..
    } = *observation;
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
    if let Some((pid, elapsed)) = starting.first() {
        return format!(
            "Studio (pid {pid}) started {elapsed}s ago and has not connected yet; Studio commands wait for it, and `-w SECONDS` extends the wait."
        );
    }
    if !clients_connected {
        return "Studio is running with the plugin installed, but no Renium plugin has connected. In Studio, check the Renium panel's connect setting and that the plugin is enabled in the Plugins Manager.".to_string();
    }
    if open_places.is_empty() {
        return "Only Play session clients are connected; an Edit session is needed.".to_string();
    }
    match bound {
        Some(bound) => format!(
            "Studio has {} open, but this project is bound to {bound}; `rbx ro` opens it, or run from its project.",
            open_places.join(", ")
        ),
        None => format!(
            "Studio has {} open, but no connected Studio matches this project; `rbx ro` reopens the project's place, or pass --place.",
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
    let startup_hangs = processes
        .iter()
        .filter(|process| !connected_pids.contains(&u64::from(process.pid)))
        .filter_map(|process| {
            studio_startup_hang(process.pid, process.started_unix).map(|hang| (process.pid, hang))
        })
        .collect::<Vec<_>>();
    let signed_out = processes
        .iter()
        .filter(|process| !connected_pids.contains(&u64::from(process.pid)))
        .filter(|process| studio_signed_out(process.pid, process.started_unix))
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    let unconnected = processes
        .iter()
        .filter(|process| !connected_pids.contains(&u64::from(process.pid)))
        .filter(|process| !open_failures.iter().any(|(pid, _)| *pid == process.pid))
        .filter(|process| !startup_hangs.iter().any(|(pid, _)| *pid == process.pid))
        .filter(|process| !signed_out.contains(&process.pid))
        .collect::<Vec<_>>();
    let prompts = if unconnected.is_empty() {
        PermissionPrompts::default()
    } else {
        studio_permission_prompts()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs());
    let prompt_held = if prompts.named.is_empty() {
        Vec::new()
    } else {
        unconnected
            .iter()
            .filter(|process| {
                pending_for_the_hold_period(
                    process
                        .started_unix
                        .map(|started| now.saturating_sub(started)),
                    studio_place_open_pending(process.pid, process.started_unix),
                )
            })
            .map(|process| process.pid)
            .collect::<Vec<_>>()
    };
    let starting = unconnected
        .iter()
        .filter(|process| !prompt_held.contains(&process.pid))
        .filter_map(|process| {
            let elapsed = now.saturating_sub(process.started_unix?);
            (elapsed < STARTING_GRACE_SECONDS).then_some((process.pid, elapsed))
        })
        .collect::<Vec<_>>();
    let verdict = verdict_text(&Observation {
        studio_running: !processes.is_empty(),
        plugin: plugin.as_ref(),
        earliest_start,
        clients_connected: !clients.is_empty(),
        open_places: &open_places,
        bound: bound.as_deref(),
        open_failures: &open_failures,
        startup_hangs: &startup_hangs,
        signed_out: &signed_out,
        starting: &starting,
        prompts: &prompts,
        prompt_held: &prompt_held,
    });
    json!({
        "studioProcesses": processes.iter().map(|process| json!({
            "pid": process.pid,
            "startedUnix": process.started_unix,
            "title": process.title,
        })).collect::<Vec<_>>(),
        "plugin": plugin.as_ref().map(|(path, modified)| json!({"path": path, "modifiedUnix": modified})),
        "openPlaces": open_places,
        "openFailures": open_failures.iter().map(|(pid, reason)| json!({"pid": pid, "reason": reason})).collect::<Vec<_>>(),
        "startupHangs": startup_hangs.iter().map(|(pid, hang)| json!({"pid": pid, "instancesAtLaunch": hang.instances_at_launch, "signedIn": hang.signed_in})).collect::<Vec<_>>(),
        "signedOut": signed_out,
        "permissionPrompts": prompts.named,
        "heldByPrompt": prompt_held,
        "starting": starting.iter().map(|(pid, elapsed)| json!({"pid": pid, "elapsedSeconds": elapsed})).collect::<Vec<_>>(),
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

/// Whether a diagnosis reports a Studio at the Roblox sign-in dialog: waiting
/// for it to connect is pointless until the user signs in.
pub(crate) fn is_signed_out(diagnosis: &Value) -> bool {
    diagnosis["signedOut"]
        .as_array()
        .is_some_and(|pids| !pids.is_empty())
}

/// Whether a diagnosis reports this Studio as held by a macOS permission
/// prompt: it asked to open its place and waits for the user's answer.
pub(crate) fn is_held_by_prompt(diagnosis: &Value, pid: u32) -> bool {
    diagnosis["heldByPrompt"].as_array().is_some_and(|pids| {
        pids.iter()
            .any(|held| held.as_u64() == Some(u64::from(pid)))
    })
}

/// The verdict for this Studio when a diagnosis reports it as held by a
/// macOS permission prompt.
pub(crate) fn held_by_prompt_verdict(diagnosis: &Value, pid: u32) -> Option<String> {
    let question = diagnosis["permissionPrompts"]
        .as_array()?
        .first()?
        .as_str()?;
    is_held_by_prompt(diagnosis, pid).then(|| held_verdict(pid, question))
}

/// Whether a `status` wait may still end with a connected Studio: none is at
/// the sign-in dialog, and one is still opening its place or, for a project
/// that has matched no runtime yet (`unmatched`), may still connect.
pub(crate) fn worth_waiting(diagnosis: &Value, clients: &[Value], unmatched: bool) -> bool {
    !is_signed_out(diagnosis)
        && (is_starting(diagnosis) || unmatched && may_still_connect(diagnosis, clients))
}

/// Whether a diagnosis reports a Studio that is still opening its place.
pub(crate) fn is_starting(diagnosis: &Value) -> bool {
    diagnosis["starting"]
        .as_array()
        .is_some_and(|starting| !starting.is_empty())
}

/// Whether a Studio process the project has not matched yet may still
/// connect: one runs whose Edit session is not among `clients` and that no
/// macOS permission prompt holds, and none failed to open its place or hung
/// at launch. A `status -w` keeps waiting for it (a
/// daemon restart or a fresh launch connects its plugin a few seconds later).
/// When every running Studio has its Edit session connected, nothing is left
/// to wait for. A Play client does not cover its process: it can share the
/// process of an Edit session that has not reconnected yet.
pub(crate) fn may_still_connect(diagnosis: &Value, clients: &[Value]) -> bool {
    let non_empty = |key: &str| {
        diagnosis[key]
            .as_array()
            .is_some_and(|values| !values.is_empty())
    };
    let unconnected = diagnosis["studioProcesses"]
        .as_array()
        .is_some_and(|processes| {
            processes.iter().any(|process| {
                let held = process["pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .is_some_and(|pid| is_held_by_prompt(diagnosis, pid));
                !held
                    && !clients.iter().any(|client| {
                        client["role"] == "edit"
                            && client["pid"].as_u64() == process["pid"].as_u64()
                    })
            })
        });
    unconnected
        && !non_empty("openFailures")
        && !non_empty("startupHangs")
        && !non_empty("signedOut")
}

#[cfg(test)]
mod wait_tests {
    use serde_json::json;

    use super::may_still_connect;

    #[test]
    fn a_running_studio_without_a_failure_may_still_connect() {
        assert!(may_still_connect(
            &json!({
                "studioProcesses": [{ "pid": 1 }],
                "openFailures": [],
                "startupHangs": [],
            }),
            &[]
        ));
        assert!(!may_still_connect(
            &json!({
                "studioProcesses": [],
                "openFailures": [],
                "startupHangs": [],
            }),
            &[]
        ));
        assert!(!may_still_connect(
            &json!({
                "studioProcesses": [{ "pid": 1 }],
                "openFailures": [{ "pid": 1, "reason": "no access" }],
                "startupHangs": [],
            }),
            &[]
        ));
    }

    #[test]
    fn studios_that_are_all_connected_leave_nothing_to_wait_for() {
        let diagnosis = json!({
            "studioProcesses": [{ "pid": 1 }, { "pid": 2 }],
            "openFailures": [],
            "startupHangs": [],
        });
        let client = |pid: u32| json!({ "pid": pid, "role": "edit" });
        assert!(!may_still_connect(&diagnosis, &[client(1), client(2)]));
        assert!(may_still_connect(&diagnosis, &[client(1)]));
        assert!(may_still_connect(&diagnosis, &[json!({ "role": "edit" })]));
        let play = json!({ "pid": 2, "role": "play-server" });
        assert!(may_still_connect(&diagnosis, &[client(1), play]));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Observation, PermissionPrompts, StartupHang, elapsed_seconds, open_failure_in_log,
        startup_hang_in_log, verdict_text,
    };

    static NO_PROMPTS: PermissionPrompts = PermissionPrompts {
        named: Vec::new(),
        unread: 0,
        accessibility_off: false,
    };

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

    fn observation<'a>(
        plugin: Option<&'a (String, Option<u64>)>,
        clients_connected: bool,
    ) -> Observation<'a> {
        Observation {
            studio_running: true,
            plugin,
            earliest_start: Some(10),
            clients_connected,
            open_places: &[],
            bound: None,
            open_failures: &[],
            startup_hangs: &[],
            signed_out: &[],
            starting: &[],
            prompts: &NO_PROMPTS,
            prompt_held: &[],
        }
    }

    #[test]
    fn verdict_names_the_blocking_condition() {
        assert!(
            verdict_text(&Observation {
                studio_running: false,
                earliest_start: None,
                ..observation(None, false)
            })
            .contains("not running")
        );
        assert!(verdict_text(&observation(Some(&plugin(None)), false)).contains("rbx setup"));
        assert!(
            verdict_text(&observation(Some(&plugin(Some(20))), false)).contains("restart Studio")
        );
        assert!(
            verdict_text(&observation(Some(&plugin(Some(5))), false))
                .contains("no Renium plugin has connected")
        );
        assert!(verdict_text(&observation(Some(&plugin(Some(5))), true)).contains("Edit session"));
        let open = ["Other".to_string()];
        let mismatch = verdict_text(&Observation {
            open_places: &open,
            bound: Some("E:/place.rbxl"),
            ..observation(Some(&plugin(Some(20))), true)
        });
        assert!(mismatch.contains("Other") && mismatch.contains("E:/place.rbxl"));
        let refused = verdict_text(&Observation {
            open_failures: &[(4700, "User is not authorized to access Asset.".to_string())],
            ..observation(Some(&plugin(Some(5))), false)
        });
        assert!(refused.contains("4700") && refused.contains("not authorized"));
    }

    #[test]
    fn a_studio_at_the_sign_in_dialog_is_reported_signed_out_and_not_waited_for() {
        let log = "2026-11-02T09:56:58.340Z,1.3,b63c,6 [FLog::StudioTimingLog] Authenticated : NO
                   2026-11-02T09:56:58.401Z,1.4,b63c,6,Info [FLog::StudioKeyEvents] show login dialog [start]
";
        assert!(super::signed_out_in_log(log));
        assert!(!super::signed_out_in_log(
            "2026-11-02T09:56:58.340Z,1.3,b63c,6 [FLog::StudioTimingLog] Authenticated : NO
"
        ));
        let verdict = verdict_text(&Observation {
            studio_running: true,
            plugin: None,
            earliest_start: None,
            clients_connected: false,
            open_places: &[],
            bound: None,
            open_failures: &[],
            startup_hangs: &[],
            signed_out: &[4711],
            starting: &[(4711, 3)],
            prompts: &NO_PROMPTS,
            prompt_held: &[],
        });
        assert!(
            verdict.contains("4711") && verdict.contains("signed out"),
            "{verdict}"
        );
        let diagnosis =
            serde_json::json!({"signedOut": [4711], "studioProcesses": [{"pid": 4711}]});
        assert!(super::is_signed_out(&diagnosis));
        assert!(!super::may_still_connect(&diagnosis, &[]));
    }

    #[test]
    fn a_prompt_counts_when_studio_asks_and_its_question_is_what_gets_quoted() {
        let question =
            "\u{201c}RobloxStudio\u{201d} would like to access files in your Documents folder.";
        let prompts = super::sort_system_prompts(vec![
            Some(format!(
                "{question}\nRoblox Studio needs access to your Documents folder to store settings, plugins, and other data."
            )),
            Some(
                "\u{201c}Terminal\u{201d} wants access to control \u{201c}RobloxStudio\u{201d}."
                    .to_string(),
            ),
            None,
        ]);
        assert_eq!(prompts.named, [question.to_string()]);
        assert_eq!(prompts.unread, 1);
        assert!(super::asked_by_studio(
            "\u{201e}Roblox Studio\u{201c} m\u{f6}chte auf Dateien zugreifen."
        ));
        assert!(!super::asked_by_studio(
            "\u{201c}Terminal\u{201d} would like to access files in your Desktop folder."
        ));
    }

    #[test]
    fn a_studio_held_by_a_macos_permission_prompt_is_named_after_its_own_failures() {
        let question =
            "\u{201c}RobloxStudio\u{201d} would like to access files in your Documents folder.";
        let prompts = super::sort_system_prompts(vec![Some(question.to_string())]);
        let open = ["Other".to_string()];
        let held = verdict_text(&Observation {
            open_places: &open,
            prompts: &prompts,
            prompt_held: &[4711],
            ..observation(Some(&plugin(None)), true)
        });
        assert!(
            held.contains("4711") && held.contains(question) && !held.contains("rbx setup"),
            "{held}"
        );
        assert!(!held.contains("Allow"), "{held}");
        assert_eq!(held.matches(super::PROMPT_LEAD).count(), 1, "{held}");

        let hang = [(
            4712,
            StartupHang {
                instances_at_launch: 1,
                signed_in: true,
            },
        )];
        let hung = verdict_text(&Observation {
            startup_hangs: &hang,
            prompts: &prompts,
            prompt_held: &[4711],
            ..observation(Some(&plugin(Some(5))), false)
        });
        assert!(
            hung.contains("Studio (pid 4712) hung") && hung.contains(question),
            "{hung}"
        );
        let signed_out = verdict_text(&Observation {
            signed_out: &[4713],
            prompts: &prompts,
            prompt_held: &[4711],
            ..observation(Some(&plugin(Some(5))), false)
        });
        assert!(
            signed_out.contains("4713") && signed_out.contains(question),
            "{signed_out}"
        );

        let diagnosis = serde_json::json!({
            "heldByPrompt": [4711],
            "permissionPrompts": [question],
        });
        assert_eq!(
            super::held_by_prompt_verdict(&diagnosis, 4711),
            Some(super::held_verdict(4711, question))
        );
        assert_eq!(super::held_by_prompt_verdict(&diagnosis, 4712), None);
    }

    #[test]
    fn a_prompt_that_holds_no_studio_is_a_note_on_every_other_verdict() {
        let question =
            "\u{201c}RobloxStudio\u{201d} would like to access files in your Documents folder.";
        let prompts = super::sort_system_prompts(vec![Some(question.to_string())]);
        let open = ["Other".to_string()];
        let starting = verdict_text(&Observation {
            starting: &[(4711, 5)],
            prompts: &prompts,
            ..observation(Some(&plugin(Some(5))), false)
        });
        assert!(
            starting.contains("has not connected yet") && starting.contains(question),
            "{starting}"
        );
        let elsewhere = verdict_text(&Observation {
            open_places: &open,
            prompts: &prompts,
            ..observation(Some(&plugin(Some(5))), true)
        });
        assert!(
            elsewhere.contains("Other") && elsewhere.contains(question),
            "{elsewhere}"
        );
        let missing_plugin = verdict_text(&Observation {
            prompts: &prompts,
            ..observation(Some(&plugin(None)), false)
        });
        assert!(
            missing_plugin.contains("rbx setup") && missing_plugin.contains(question),
            "{missing_plugin}"
        );

        let unread = super::sort_system_prompts(vec![None]);
        let unread_note = verdict_text(&Observation {
            open_places: &open,
            prompts: &unread,
            ..observation(Some(&plugin(Some(5))), true)
        });
        assert!(
            unread_note.contains("could not read") && !unread_note.contains("Accessibility"),
            "{unread_note}"
        );
        let no_access = PermissionPrompts {
            accessibility_off: true,
            ..super::sort_system_prompts(vec![None])
        };
        let no_access_note = verdict_text(&Observation {
            prompts: &no_access,
            ..observation(Some(&plugin(Some(5))), false)
        });
        assert!(
            no_access_note.contains("Accessibility is off"),
            "{no_access_note}"
        );
        let quiet = verdict_text(&observation(Some(&plugin(Some(5))), false));
        assert!(!quiet.contains("macOS"), "{quiet}");
    }

    #[test]
    fn a_held_studio_ends_the_wait_and_another_starting_studio_keeps_it() {
        let held_only = serde_json::json!({
            "studioProcesses": [{"pid": 4711}],
            "heldByPrompt": [4711],
            "starting": [],
        });
        assert!(super::is_held_by_prompt(&held_only, 4711));
        assert!(!super::is_held_by_prompt(&held_only, 4712));
        assert!(!super::may_still_connect(&held_only, &[]));
        assert!(!super::worth_waiting(&held_only, &[], true));

        let with_another = serde_json::json!({
            "studioProcesses": [{"pid": 4711}, {"pid": 4712}],
            "heldByPrompt": [4711],
            "starting": [{"pid": 4712, "elapsedSeconds": 3}],
        });
        assert!(super::worth_waiting(&with_another, &[], false));
        let aged = serde_json::json!({
            "studioProcesses": [{"pid": 4711}, {"pid": 4712}],
            "heldByPrompt": [4711],
            "starting": [],
        });
        assert!(super::worth_waiting(&aged, &[], true));
        assert!(!super::worth_waiting(&aged, &[], false));
        let connected = serde_json::json!({ "pid": 4712, "role": "edit" });
        assert!(!super::worth_waiting(&aged, &[connected], true));
        let signed_out = serde_json::json!({
            "studioProcesses": [{"pid": 4712}],
            "signedOut": [4712],
            "starting": [{"pid": 4713, "elapsedSeconds": 3}],
        });
        assert!(!super::worth_waiting(&signed_out, &[], true));
    }

    #[test]
    fn a_place_open_that_was_asked_for_and_never_began_is_read_from_the_log() {
        let asked = "2026-11-11T20:48:57.160Z,2.160966,f70bdd80,6,Info [FLog::StudioKeyEvents] enter LifecycleManager UserSession scope\n\
            2026-11-11T20:48:57.162Z,2.162519,f70bdd80,6,Info [FLog::PlaceManager] Start to open place with traceId: e86f2f5f\n\
            2026-11-11T20:53:07.535Z,252.535339,6c39b000,6 [DFLog::NamedMutex] NamedMutex(0x1, name:InstallerInfo.log)\n";
        assert_eq!(super::place_open_pending_in_log(asked), Some(2));
        assert_eq!(
            super::place_open_pending_in_log(
                "2026-11-11T20:48:56.972Z,1.972898,f70bdd80,6,Info [FLog::StudioKeyEvents] login [end][success]\n"
            ),
            None
        );
        let began = format!(
            "{asked}2026-11-11T20:27:20.398Z,3.398547,f70bdd80,6,Info [FLog::StudioKeyEvents] open place (identifier = /p/a.rbxl) [start]\n"
        );
        assert_eq!(super::place_open_pending_in_log(&began), None);
        let state = format!(
            "{asked}2026-11-11T20:27:20.526Z,3.526665,6b903000,6 [telemetryLog] State: OpenPlaceInitialization\n"
        );
        assert_eq!(super::place_open_pending_in_log(&state), None);
    }

    #[test]
    fn only_a_request_that_stayed_pending_for_the_hold_period_counts_as_held() {
        assert!(super::pending_for_the_hold_period(Some(27), Some(2)));
        assert!(!super::pending_for_the_hold_period(Some(15), Some(2)));
        assert!(!super::pending_for_the_hold_period(
            Some(3_600),
            Some(3_595)
        ));
        assert!(!super::pending_for_the_hold_period(Some(3_600), None));
        assert!(!super::pending_for_the_hold_period(None, Some(2)));
    }

    #[test]
    fn a_studio_that_just_launched_is_reported_as_starting() {
        let open = ["Other".to_string()];
        let starting = verdict_text(&Observation {
            open_places: &open,
            bound: Some("published place 2 in game 1"),
            starting: &[(4711, 12)],
            ..observation(Some(&plugin(Some(5))), true)
        });
        assert!(starting.contains("4711") && starting.contains("12s"));
        assert!(!starting.contains("Other"));
        let unconnected = verdict_text(&Observation {
            starting: &[(4711, 12)],
            ..observation(Some(&plugin(Some(5))), false)
        });
        assert!(unconnected.contains("has not connected yet"));
        assert!(super::is_starting(
            &serde_json::json!({"starting": [{"pid": 4711}]})
        ));
        assert!(!super::is_starting(&serde_json::json!({"starting": []})));
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
        let hung = "x [FLog::SystemCheck] Running instance count at launch 2\nx [FLog::StudioKeyEvents] login (automatic) [start]\nx,Warning [FLog::StudioHangMonitor] Hang In Progress. HangId: 1\nx,Warning [FLog::StudioHangMonitor] Hang Detected. HangId: 1\n";
        assert_eq!(
            startup_hang_in_log(hung),
            Some(StartupHang {
                instances_at_launch: 2,
                signed_in: false
            })
        );
        let signed_in = "x [FLog::LoginController] Login got Standalone DM ready to enter User scope\nx,Warning [FLog::StudioHangMonitor] Hang Detected. HangId: 2\n";
        assert_eq!(
            startup_hang_in_log(signed_in),
            Some(StartupHang {
                instances_at_launch: 1,
                signed_in: true
            })
        );
        let stall_while_loading = "x,Warning [FLog::StudioHangMonitor] Hang Detected. HangId: 3\nx [telemetryLog] State: OpenPlaceSuccess\n";
        assert_eq!(startup_hang_in_log(stall_while_loading), None);
        assert_eq!(
            startup_hang_in_log(
                "x,Warning [FLog::StudioHangMonitor] Hang In Progress. HangId: 4\n"
            ),
            None
        );
    }
}
