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

#[cfg(all(test, any(windows, target_os = "macos")))]
#[test]
#[ignore = "Read-only native comparison; requires explicit owned fixture PID and title"]
fn native_snapshot_reads_match_approved_reads_and_reject_wrong_classes() -> Result<()> {
    use std::time::Instant;
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = std::env::var("RENIUM_INSPECT_FIXTURE_TITLE")?;
    anyhow::ensure!(
        title.starts_with("Renium"),
        "Expected an owned Renium fixture"
    );
    prepare_context(pid, &title)?;
    let mut paired = Vec::new();
    for _ in 0..3 {
        for class in ["Workspace", "MaterialService", "StarterPlayer"] {
            for property in crate::editor::native_roots::capture_properties(class) {
                let path = [class.into()];
                let started = Instant::now();
                let mut approved =
                    prepare_property(pid, &title, &path, &[1], property, Duration::from_secs(2))?;
                let before = approved.read()?;
                let separate_us = started.elapsed().as_micros();
                let started = Instant::now();
                let after = read_property(
                    pid,
                    &title,
                    &path,
                    &[1],
                    class,
                    property,
                    Duration::from_secs(2),
                )?;
                paired.push((separate_us, started.elapsed().as_micros()));
                assert_eq!(before, after, "{class}.{property}");
                let error = read_property(
                    pid,
                    &title,
                    &path,
                    &[1],
                    "Folder",
                    property,
                    Duration::from_secs(2),
                )
                .unwrap_err();
                assert!(error.to_string().contains("changed class"), "{error:#}");
            }
        }
    }
    println!("paired separate/atomic native read microseconds: {paired:?}");
    Ok(())
}
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
