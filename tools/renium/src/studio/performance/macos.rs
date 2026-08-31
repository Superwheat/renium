use std::collections::BTreeMap;
use std::ffi::{c_int, c_void};

use anyhow::{Result, bail};

use super::{AppliedReadback, BenchmarkSample, CommitInfo, ControlPlan, OriginalControls};

#[derive(Default)]
pub(super) struct BackendState;

impl BackendState {
    pub(super) fn release(&self, _locator: &str) {}
}

const PROC_PIDTBSDINFO: c_int = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcBsdInfo {
    flags: u32,
    status: u32,
    xstatus: u32,
    pid: u32,
    ppid: u32,
    uid: u32,
    gid: u32,
    ruid: u32,
    rgid: u32,
    svuid: u32,
    svgid: u32,
    rfu_1: u32,
    comm: [u8; 16],
    name: [u8; 32],
    nfiles: u32,
    pgid: u32,
    pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    nice: i32,
    start_tvsec: u64,
    start_tvusec: u64,
}

unsafe extern "C" {
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffer_size: c_int,
    ) -> c_int;
}

pub(super) fn backend_name() -> &'static str {
    "macos-report-only"
}

pub(super) fn supports_profiles() -> bool {
    false
}

pub(super) fn unsupported_reason() -> &'static str {
    "macOS does not expose a standard ordinary-user CPU quota that Renium can apply and reliably restore"
}

pub(super) fn calibration_key() -> String {
    format!("macos-{}", std::env::consts::ARCH)
}

pub(super) fn logical_processors() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

pub(super) fn process_identity(pid: u32) -> Option<String> {
    let mut info = ProcBsdInfo {
        flags: 0,
        status: 0,
        xstatus: 0,
        pid: 0,
        ppid: 0,
        uid: 0,
        gid: 0,
        ruid: 0,
        rgid: 0,
        svuid: 0,
        svgid: 0,
        rfu_1: 0,
        comm: [0; 16],
        name: [0; 32],
        nfiles: 0,
        pgid: 0,
        pjobc: 0,
        e_tdev: 0,
        e_tpgid: 0,
        nice: 0,
        start_tvsec: 0,
        start_tvusec: 0,
    };
    // SAFETY: proc_pidinfo writes at most the provided ProcBsdInfo-sized buffer.
    let read = unsafe {
        proc_pidinfo(
            i32::try_from(pid).ok()?,
            PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut ProcBsdInfo).cast(),
            i32::try_from(std::mem::size_of::<ProcBsdInfo>()).ok()?,
        )
    };
    if read != std::mem::size_of::<ProcBsdInfo>() as i32 {
        return None;
    }
    Some(format!("{}.{:06}", info.start_tvsec, info.start_tvusec))
}

pub(super) fn identity_matches(pid: u32, identity: &str) -> bool {
    process_identity(pid).as_deref() == Some(identity)
}

pub(super) fn backend_locator(_installation_id: &str, identity: &str) -> String {
    format!("macos-{identity}")
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
    _original: OriginalControls,
    _memory_cap: Option<u64>,
) -> Result<AppliedReadback> {
    bail!("{}", unsupported_reason())
}

pub(super) fn neutralize(
    _backend: &BackendState,
    _pid: u32,
    _identity: &str,
    _locator: &str,
    _original: OriginalControls,
) -> Result<AppliedReadback> {
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
