#[cfg(any(target_os = "macos", test))]
mod arm64_functions;
#[cfg(any(windows, target_os = "macos", test))]
mod functions;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{AudioOutputGate, keep_selection_across_undo, suppress_package_notices};

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

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn register_history_if_available(
    pid: u32,
    title: &str,
    token: &str,
    terrain: bool,
) -> anyhow::Result<bool> {
    match register_history(pid, title, token) {
        Ok(()) => Ok(true),
        Err(error) => {
            let Some(unavailable) = error.downcast_ref::<HistoryHookUnavailable>() else {
                return Err(error);
            };
            if terrain {
                anyhow::bail!(
                    "Terrain sync needs the Studio history hook, which this Studio build does not support yet: {unavailable}"
                );
            }
            let mut warned = crate::system::LockRecover::lock_recover(&HISTORY_HOOK_WARNED);
            let first = !warned.contains(&pid);
            if first {
                warned.push(pid);
                crate::app::output::log_global(
                    2,
                    format_args!(
                        "[renium] warning: Studio history hook unavailable on this Studio build; Terrain sync is disabled until Renium is updated ({unavailable})"
                    ),
                );
            }
            Ok(false)
        }
    }
}
