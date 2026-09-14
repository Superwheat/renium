#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::suppress_package_notices;

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

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
