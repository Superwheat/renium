#[cfg(any(target_os = "macos", test))]
mod arm64_functions;
#[cfg(any(target_os = "macos", test))]
mod arm64_playtest;
#[cfg(any(target_os = "macos", test))]
mod arm64_routing;
#[cfg(any(windows, target_os = "macos", test))]
mod functions;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{
    NativeMacTestClient, NativeTestJoin, TestClientRoute, TestJoinTarget, prepare_test_client,
    process_arguments, start_destination_server, test_client_configuration,
};
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) mod probes;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::NativePlayCaption;
#[cfg(windows)]
pub(crate) use windows::NativePlayJoin;
#[cfg(windows)]
pub(crate) use windows::NativePlayLoad;
#[cfg(windows)]
pub(crate) use windows::NativeTeleportDispatch;
#[cfg(windows)]
pub(crate) use windows::NativeTestJoin;
#[cfg(windows)]
pub(crate) use windows::notify_teleport_gui;
#[cfg(windows)]
pub(crate) use windows::{AudioOutputGate, keep_selection_across_undo, suppress_package_notices};
#[cfg(windows)]
pub(crate) use windows::{
    prepare_test_client, rejoin_test_client, start_destination_server, test_client_configuration,
};

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

#[test]
fn terrain_observation_retains_one_subscription_and_validates_model_ownership() {
    let name = if cfg!(windows) {
        "renium-terrain-observation-test.exe"
    } else {
        "renium-terrain-observation-test"
    };
    assert!(
        std::process::Command::new(std::path::Path::new(env!("OUT_DIR")).join(name))
            .status()
            .expect("run native Terrain lifecycle regression")
            .success()
    );
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn validate_function_arguments(
    class: &str,
    name: &str,
    arguments: &[serde_json::Value],
) -> Result<()> {
    functions::input(class, name, arguments).map(|_| ())
}

// Windows binds native work to a document window; macOS discovers the DataModel
// by its internal name. Never substitute game.Name for a Windows window title.
#[cfg(windows)]
pub(crate) fn target_name(pid: u32, _place_name: &str) -> Result<String> {
    crate::studio::input::studio_window_title(pid)
}

#[cfg(target_os = "macos")]
pub(crate) fn target_name(_pid: u32, place_name: &str) -> Result<String> {
    Ok(place_name.to_owned())
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum PackageAction {
    Desync,
    Restore,
    Publish,
    Update,
}

pub(crate) struct PackageTarget {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) expected_version: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PackageActionResult {
    pub(crate) action: &'static str,
    pub(crate) changed: bool,
    pub(crate) path: String,
    pub(crate) status: String,
    pub(crate) version: i64,
}

pub(crate) struct StudioActionOutcome {
    pub(crate) window_title: String,
    pub(crate) found: u32,
}

/// Runs one of Studio's own menu commands by its QAction object name on the
/// UI thread of the Studio window titled `studio_title`.
pub(crate) fn trigger_studio_action(
    pid: u32,
    studio_title: &str,
    action: &str,
) -> Result<StudioActionOutcome> {
    platform_trigger_studio_action(pid, studio_title, action)
}

/// Hidden diagnostic: runs one package action against a Studio process from
/// this CLI process, without the daemon, naming the step that failed.
pub(crate) fn native_package_command(args: crate::cli::NativePackageArgs) -> anyhow::Result<()> {
    if args.action == "history-register" {
        let title = target_name(args.pid, "")?;
        let result = register_history_if_available(args.pid, &title, "renium-diagnostic", false);
        println!(
            "{}",
            match result {
                Ok(hooked) => serde_json::json!({ "ok": true, "hooked": hooked }),
                Err(error) => serde_json::json!({ "ok": false, "error": format!("{error:#}") }),
            }
        );
        return Ok(());
    }
    if args.action == "history" {
        let title = target_name(args.pid, "")?;
        match history_diagnostic(args.pid, &title) {
            Ok(result) => println!("{result}"),
            Err(error) => println!(
                "{}",
                serde_json::json!({ "ok": false, "error": format!("{error:#}") })
            ),
        }
        return Ok(());
    }
    let action = match args.action.as_str() {
        "status" | "desync" => PackageAction::Desync,
        "restore" => PackageAction::Restore,
        other => anyhow::bail!("unknown package action {other}"),
    };
    let title = target_name(args.pid, "")?;
    let target = PackageTarget {
        path_segments: args.path.split('.').map(str::to_owned).collect(),
        path_ordinals: Vec::new(),
        expected_version: args.version,
    };
    let result = if args.action == "status" {
        platform_package_status(args.pid, &title, &target)
    } else {
        run_package_action(
            args.pid,
            &title,
            &target,
            action,
            Duration::from_secs_f64(args.timeout_seconds),
        )
    };
    match result {
        Ok(result) => println!("{}", serde_json::to_string(&result)?),
        Err(error) => println!(
            "{}",
            serde_json::json!({ "ok": false, "error": format!("{error:#}") })
        ),
    }
    Ok(())
}

pub(crate) fn run_package_action(
    pid: u32,
    studio_title: &str,
    target: &PackageTarget,
    action: PackageAction,
    timeout: Duration,
) -> Result<PackageActionResult> {
    platform_package_action(pid, studio_title, target, action, timeout)
}

#[cfg(target_os = "macos")]
pub(crate) use macos::*;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(any(windows, target_os = "macos"))]
fn terrain_payload(
    bindings: &[u8],
    token: &str,
    fingerprint: &[u8; 64],
    smooth: Option<&[u8]>,
    physics: Option<&[u8]>,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        bindings.len() == 176 && !token.is_empty() && token.len() <= 256,
        "Invalid Terrain transaction binding"
    );
    let flags = if smooth.is_none() && physics.is_none() {
        4
    } else {
        usize::from(smooth.is_some()) | (usize::from(physics.is_some()) << 1)
    };
    let smooth = smooth.unwrap_or_default();
    let physics = physics.unwrap_or_default();
    anyhow::ensure!(
        256 + token.len() + smooth.len() + physics.len() <= 128 * 1024 * 1024,
        "Terrain transfer exceeds 128 MiB"
    );
    let mut payload = bindings.to_vec();
    payload.extend_from_slice(fingerprint);
    for size in [token.len(), smooth.len(), physics.len(), flags] {
        payload.extend_from_slice(&(size as u32).to_le_bytes());
    }
    payload.extend_from_slice(token.as_bytes());
    payload.extend_from_slice(smooth);
    payload.extend_from_slice(physics);
    Ok(payload)
}

#[derive(Debug)]
pub(crate) struct HistoryHookUnavailable(pub(crate) String);

impl std::fmt::Display for HistoryHookUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HistoryHookUnavailable {}

#[cfg(any(windows, target_os = "macos"))]
static HISTORY_HOOK_WARNED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
pub(crate) fn native_capture_verified(pid: u32) -> bool {
    target_name(pid, "")
        .and_then(|title| probes::run(pid, &title, probes::NativeOperation::Capture))
        .is_ok()
}

pub(crate) fn register_history_if_available(
    pid: u32,
    title: &str,
    token: &str,
    terrain: bool,
) -> anyhow::Result<bool> {
    let result = probes::run(pid, title, probes::NativeOperation::History)
        .map_err(|error| anyhow::Error::new(HistoryHookUnavailable(format!("{error:#}"))))
        .and_then(|_| register_history(pid, title, token));
    match result {
        Ok(()) => Ok(true),
        Err(error) => {
            let Some(unavailable) = error.downcast_ref::<HistoryHookUnavailable>() else {
                return Err(error);
            };
            if terrain {
                anyhow::bail!(
                    "Terrain sync needs the Studio history hook; its structural probe failed: {unavailable}"
                );
            }
            let mut warned = crate::system::LockRecover::lock_recover(&HISTORY_HOOK_WARNED);
            let first = !warned.contains(&pid);
            if first {
                warned.push(pid);
                crate::app::output::log_global(
                    2,
                    format_args!(
                        "[renium] warning: Studio history hook unavailable; using explicit rollback where supported ({unavailable})"
                    ),
                );
            }
            Ok(false)
        }
    }
}
