#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

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
