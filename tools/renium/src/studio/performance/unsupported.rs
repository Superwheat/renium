use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::{AppliedReadback, BenchmarkSample, CommitInfo, ControlPlan, OriginalControls};

#[derive(Default)]
pub(super) struct BackendState;

impl BackendState {
    pub(super) fn release(&self, _locator: &str) {}
}

pub(super) fn backend_name() -> &'static str {
    "unsupported"
}

pub(super) fn supports_profiles() -> bool {
    false
}

pub(super) fn unsupported_reason() -> &'static str {
    "Roblox Studio performance profiles are supported on Windows; this platform has no safe equivalent controls"
}

pub(super) fn calibration_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub(super) fn logical_processors() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

pub(super) fn process_identity(pid: u32) -> Option<String> {
    crate::app::update::process_start_identity(pid)
}

pub(super) fn identity_matches(pid: u32, identity: &str) -> bool {
    process_identity(pid).as_deref() == Some(identity)
}

pub(super) fn backend_locator(_installation_id: &str, identity: &str) -> String {
    format!("unsupported-{identity}")
}

pub(super) fn preflight(_pid: u32, _identity: &str, _locator: &str) -> Result<()> {
    bail!("{}", unsupported_reason())
}

pub(super) fn process_tree_commit(_pid: u32) -> Result<u64> {
    Ok(0)
}

pub(super) fn process_allowed_affinity(
    _pid: u32,
    _expected_identity: Option<&str>,
) -> Result<Option<u64>> {
    Ok(None)
}

pub(super) fn process_priority_class(
    _pid: u32,
    _expected_identity: Option<&str>,
) -> Result<Option<u32>> {
    Ok(None)
}

pub(super) fn commit_info() -> Option<CommitInfo> {
    None
}

pub(super) fn calibrate(
    _plans: &BTreeMap<String, ControlPlan>,
) -> Result<(BenchmarkSample, BTreeMap<String, BenchmarkSample>)> {
    bail!("{}", unsupported_reason())
}

pub(super) fn apply(
    _backend: &BackendState,
    _pid: u32,
    _identity: &str,
    _locator: &str,
    _plan: &ControlPlan,
    original: OriginalControls,
    _memory_cap: Option<u64>,
) -> Result<AppliedReadback> {
    original.discard();
    bail!("{}", unsupported_reason())
}

pub(super) fn neutralize(
    _backend: &BackendState,
    _pid: u32,
    _identity: &str,
    _locator: &str,
    original: OriginalControls,
) -> Result<AppliedReadback> {
    original.discard();
    Ok(AppliedReadback {
        neutral: true,
        ..AppliedReadback::default()
    })
}

pub(super) fn membership_is_irreversible() -> bool {
    false
}

pub(super) fn run_holder(_pid: u32, _identity: &str, _locator: &str) -> Result<()> {
    bail!("{}", unsupported_reason())
}
