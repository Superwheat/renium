use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows_sys::Win32::System::ProcessStatus::{
    GetPerformanceInfo, GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS,
    PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemServices::{
    JOB_OBJECT_ASSIGN_PROCESS, JOB_OBJECT_QUERY, JOB_OBJECT_SET_ATTRIBUTES,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, GetProcessAffinityMask, GetProcessTimes,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE, PROCESS_VM_READ, SetPriorityClass, SetProcessAffinityMask,
    WaitForSingleObject,
};

use super::{
    AppliedReadback, BenchmarkSample, CommitInfo, ControlPlan, OriginalControls, Priority,
};

const PROCESS_JOB_RIGHTS: u32 =
    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
const JOB_RIGHTS: u32 = JOB_OBJECT_ASSIGN_PROCESS | JOB_OBJECT_QUERY | JOB_OBJECT_SET_ATTRIBUTES;
const SYNCHRONIZE_RIGHT: u32 = 0x0010_0000;

struct OwnedHandle(HANDLE);

// SAFETY: Win32 kernel handles are process-wide. Ownership still moves with
// OwnedHandle, and every shared access is serialized by BackendState::jobs.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE, context: &str) -> Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(last_error(context));
        }
        Ok(Self(handle))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: OwnedHandle is constructed from a valid, uniquely owned Win32 handle.
        unsafe { CloseHandle(self.0) };
    }
}

#[derive(Default)]
pub(super) struct BackendState {
    jobs: Mutex<HashMap<String, OwnedHandle>>,
}

impl BackendState {
    fn with_existing_job<T>(
        &self,
        locator: &str,
        run: impl FnOnce(&OwnedHandle) -> Result<T>,
    ) -> Result<Option<T>> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if !jobs.contains_key(locator) {
            let Some(job) = open_job(locator)? else {
                return Ok(None);
            };
            jobs.insert(locator.to_string(), job);
        }
        run(jobs.get(locator).expect("job was inserted")).map(Some)
    }

    fn ensure_holder(&self, pid: u32, identity: &str, locator: &str) -> Result<()> {
        if self.with_existing_job(locator, |_| Ok(()))?.is_some() {
            return Ok(());
        }
        start_holder(pid, identity, locator)?;
        self.with_existing_job(locator, |_| Ok(()))?
            .context("Performance holder did not publish its Job Object")
    }

    pub(super) fn release(&self, locator: &str) {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(locator);
    }
}

pub(super) fn backend_name() -> &'static str {
    "windows-job-object"
}

pub(super) fn supports_profiles() -> bool {
    true
}

pub(super) fn unsupported_reason() -> &'static str {
    "Windows Job Object controls are unavailable"
}

pub(super) fn calibration_key() -> String {
    let processor = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".to_string());
    let mut power = "unknown".to_string();
    // SAFETY: SYSTEM_POWER_STATUS is fully initialized and passed to the documented API.
    let status = unsafe {
        let mut status: SYSTEM_POWER_STATUS = zeroed();
        if GetSystemPowerStatus(&mut status) != 0 {
            power = format!(
                "ac{}-battery{}-saver{}",
                status.ACLineStatus, status.BatteryFlag, status.SystemStatusFlag
            );
        }
        status
    };
    let _ = status;
    format!(
        "windows-{}-{}-{}-{}",
        std::env::consts::ARCH,
        logical_processors(),
        processor,
        power
    )
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

pub(super) fn backend_locator(installation_id: &str, identity: &str) -> String {
    let installation = sanitize(installation_id);
    let identity = sanitize(identity);
    format!("Local\\Renium.Performance.{installation}.{identity}")
}

pub(super) fn preflight(pid: u32, identity: &str, locator: &str) -> Result<()> {
    if !identity_matches(pid, identity) {
        bail!("Studio PID {pid} was replaced before profile preflight");
    }
    let job = create_job(Some(locator))?;
    for member in process_tree(pid)? {
        let process = match open_process(member, PROCESS_JOB_RIGHTS) {
            Ok(process) => process,
            Err(_) if member != pid && process_identity(member).is_none() => continue,
            Err(error) => return Err(error),
        };
        if member == pid {
            ensure_process_identity(&process, identity)?;
        }
        ensure_job_queryable(&process, &job).with_context(|| {
            format!(
                "Could not inspect process {member} in Studio {pid}'s process tree ({identity})"
            )
        })?;
    }
    Ok(())
}

pub(super) fn process_tree_commit(pid: u32) -> Result<u64> {
    let mut total = 0u64;
    for member in process_tree(pid)? {
        let process = match open_process(
            member,
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION,
        ) {
            Ok(process) => process,
            Err(_) if member != pid && process_identity(member).is_none() => continue,
            Err(error) => return Err(error),
        };
        // SAFETY: PROCESS_MEMORY_COUNTERS_EX is initialized and the API receives its exact size.
        let private = unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS_EX = zeroed();
            counters.cb = u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>())?;
            if GetProcessMemoryInfo(
                process.0,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            ) == 0
            {
                if member != pid && process_identity(member).is_none() {
                    continue;
                }
                return Err(last_error("GetProcessMemoryInfo failed"));
            }
            counters.PrivateUsage as u64
        };
        total = total.saturating_add(private);
    }
    Ok(total)
}

pub(super) fn process_allowed_affinity(
    pid: u32,
    expected_identity: Option<&str>,
) -> Result<Option<u64>> {
    let process = open_process(
        pid,
        PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    if let Some(identity) = expected_identity {
        ensure_process_identity(&process, identity)?;
    }
    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    // SAFETY: process is valid and both masks point to initialized writable storage.
    if unsafe { GetProcessAffinityMask(process.0, &mut process_mask, &mut system_mask) } == 0 {
        return Err(last_error("GetProcessAffinityMask failed"));
    }
    Ok(Some((process_mask & system_mask) as u64))
}

pub(super) fn process_priority_class(
    pid: u32,
    expected_identity: Option<&str>,
) -> Result<Option<u32>> {
    let process = open_process(
        pid,
        PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    if let Some(identity) = expected_identity {
        ensure_process_identity(&process, identity)?;
    }
    // SAFETY: process is a valid process handle with query rights.
    let priority = unsafe { GetPriorityClass(process.0) };
    if priority == 0 {
        return Err(last_error("GetPriorityClass failed"));
    }
    Ok(Some(priority))
}

pub(super) fn commit_info() -> Option<CommitInfo> {
    // SAFETY: PERFORMANCE_INFORMATION is initialized and passed with its exact size.
    unsafe {
        let mut information: PERFORMANCE_INFORMATION = zeroed();
        information.cb = u32::try_from(size_of::<PERFORMANCE_INFORMATION>()).ok()?;
        if GetPerformanceInfo(&mut information, information.cb) == 0 {
            return None;
        }
        Some(CommitInfo {
            total: (information.CommitTotal as u64).saturating_mul(information.PageSize as u64),
            limit: (information.CommitLimit as u64).saturating_mul(information.PageSize as u64),
        })
    }
}

pub(super) fn calibrate(
    plans: &BTreeMap<String, ControlPlan>,
) -> Result<(BenchmarkSample, BTreeMap<String, BenchmarkSample>)> {
    let first = run_worker(None)?;
    let second = run_worker(None)?;
    validate_baseline(&first, &second)?;
    let baseline = BenchmarkSample {
        single: (first.single + second.single) / 2.0,
        multi: (first.multi + second.multi) / 2.0,
    };
    let mut samples = BTreeMap::new();
    for (name, plan) in plans {
        if usize::from(plan.cores) > logical_processors() {
            samples.insert(name.clone(), BenchmarkSample::default());
            continue;
        }
        let raw = run_worker(Some(plan))?;
        samples.insert(
            name.clone(),
            BenchmarkSample {
                single: raw.single / baseline.single,
                multi: raw.multi / baseline.multi,
            },
        );
    }
    Ok((baseline, samples))
}

pub(super) fn apply(
    backend: &BackendState,
    pid: u32,
    identity: &str,
    locator: &str,
    plan: &ControlPlan,
    original: OriginalControls,
    memory_cap: Option<u64>,
) -> Result<AppliedReadback> {
    if !identity_matches(pid, identity) {
        bail!("Studio PID {pid} was replaced before profile application");
    }
    backend.ensure_holder(pid, identity, locator)?;
    backend
        .with_existing_job(locator, |job| {
            apply_to_job(job, pid, identity, plan, original, memory_cap)
        })?
        .context("Performance holder Job Object disappeared")
}

fn apply_to_job(
    job: &OwnedHandle,
    pid: u32,
    identity: &str,
    plan: &ControlPlan,
    original: OriginalControls,
    memory_cap: Option<u64>,
) -> Result<AppliedReadback> {
    let tree = process_tree(pid)?;
    let mut process_handles = Vec::with_capacity(tree.len());
    for member in tree {
        let process = match open_process(member, PROCESS_JOB_RIGHTS) {
            Ok(process) => process,
            Err(_) if member != pid && process_identity(member).is_none() => continue,
            Err(error) => return Err(error),
        };
        ensure_job_queryable(&process, job)?;
        process_handles.push((member, process));
    }
    for (member, process) in &process_handles {
        let mut already = 0;
        // SAFETY: process and job are valid handles and already points to writable BOOL storage.
        if unsafe { IsProcessInJob(process.0, job.0, &mut already) } == 0 {
            return Err(last_error("IsProcessInJob failed"));
        }
        if already == 0 {
            // SAFETY: both handles are valid and opened with the rights required by the API.
            if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
                if *member != pid && process_identity(*member).is_none() {
                    continue;
                }
                return Err(last_error(&format!(
                    "Could not assign process {member} to performance job"
                )));
            }
        }
    }
    let assigned = process_handles
        .iter()
        .map(|(member, _)| *member)
        .collect::<HashSet<_>>();
    for member in process_tree(pid)? {
        if assigned.contains(&member) {
            continue;
        }
        let process = match open_process(member, PROCESS_JOB_RIGHTS) {
            Ok(process) => process,
            Err(_) if process_identity(member).is_none() => continue,
            Err(error) => return Err(error),
        };
        ensure_job_queryable(&process, job)?;
        let mut already = 0;
        // SAFETY: process and job are valid handles and already is writable BOOL storage.
        if unsafe { IsProcessInJob(process.0, job.0, &mut already) } == 0 {
            return Err(last_error("IsProcessInJob failed after Studio tree rescan"));
        }
        if already == 0 {
            // SAFETY: both handles are valid and opened with the rights required by the API.
            if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0
                && process_identity(member).is_some()
            {
                return Err(last_error(&format!(
                    "Could not assign newly created process {member} to performance job"
                )));
            }
        }
    }
    apply_job_controls(job, plan, memory_cap)?;
    let expected_affinity =
        apply_process_controls(pid, Some(identity), plan, original.affinity_mask)?;
    let mut readback = query_job(job)?;
    read_process_controls(pid, identity, &mut readback)?;
    verify_readback(plan, expected_affinity, memory_cap, &readback)?;
    if !identity_matches(pid, identity) {
        let _ = neutralize_job(job);
        bail!("Studio PID {pid} was replaced during profile application");
    }
    Ok(readback)
}

pub(super) fn neutralize(
    backend: &BackendState,
    pid: u32,
    identity: &str,
    locator: &str,
    original: OriginalControls,
) -> Result<AppliedReadback> {
    if !identity_matches(pid, identity) {
        return Ok(AppliedReadback {
            neutral: true,
            ..AppliedReadback::default()
        });
    }
    let Some(readback) = backend.with_existing_job(locator, |job| {
        neutralize_job(job)?;
        restore_process_controls(
            pid,
            identity,
            original.affinity_mask,
            original.priority_class,
        )?;
        let mut readback = query_job(job)?;
        read_process_controls(pid, identity, &mut readback)?;
        if !readback.neutral {
            bail!("Windows did not clear every Renium job limit");
        }
        Ok(readback)
    })?
    else {
        return Ok(AppliedReadback {
            neutral: true,
            ..AppliedReadback::default()
        });
    };
    Ok(readback)
}

pub(super) fn membership_is_irreversible() -> bool {
    true
}

pub(super) fn run_holder(pid: u32, identity: &str, locator: &str) -> Result<()> {
    if !identity_matches(pid, identity) {
        bail!("Studio PID {pid} was replaced before the performance holder started");
    }
    let process = open_process(pid, SYNCHRONIZE_RIGHT | PROCESS_QUERY_LIMITED_INFORMATION)?;
    ensure_process_identity(&process, identity)?;
    let _job = create_job(Some(locator))?;
    println!("ready");
    std::io::stdout().flush()?;
    // SAFETY: process is a valid process handle. INFINITE waits only in this isolated holder.
    let result = unsafe { WaitForSingleObject(process.0, u32::MAX) };
    if result != WAIT_OBJECT_0 {
        bail!("Could not wait for Studio process {pid} (Windows result {result})");
    }
    Ok(())
}

fn start_holder(pid: u32, identity: &str, locator: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let executable = std::env::current_exe().context("Could not locate rbx for profile holder")?;
    let mut child = Command::new(executable)
        .arg("performance-holder")
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--identity")
        .arg(identity)
        .arg("--locator")
        .arg(locator)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("Could not start the performance holder")?;
    let stdout = child
        .stdout
        .take()
        .context("Performance holder stdout was unavailable")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ready = String::new();
        let result = BufReader::new(stdout).read_line(&mut ready).map(|_| ready);
        let _ = sender.send(result);
    });
    let ready = match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Performance holder did not start within 3 seconds");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Performance holder stopped before reporting readiness");
        }
    };
    if ready.trim() != "ready" {
        let status = child.try_wait()?;
        bail!("Performance holder did not start (status {status:?})");
    }
    Ok(())
}

fn run_worker(plan: Option<&ControlPlan>) -> Result<BenchmarkSample> {
    let executable = std::env::current_exe().context("Could not locate rbx for calibration")?;
    let mut child = Command::new(executable)
        .arg("performance-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start the performance calibration worker")?;
    let job = if let Some(plan) = plan {
        let job = create_job(None)?;
        let process = open_process(child.id(), PROCESS_JOB_RIGHTS)?;
        // SAFETY: both handles are valid and opened with the rights required by the API.
        if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
            return Err(last_error(
                "Could not assign the calibration worker to its job",
            ));
        }
        apply_job_controls(&job, plan, None)?;
        let original_affinity = process_allowed_affinity(child.id(), None)?;
        apply_process_controls(child.id(), None, plan, original_affinity)?;
        Some(job)
    } else {
        None
    };
    child
        .stdin
        .take()
        .context("Calibration worker stdin was unavailable")?
        .write_all(b"\n")?;
    let output = child.wait_with_output()?;
    drop(job);
    if !output.status.success() {
        bail!(
            "Calibration worker failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("Calibration worker returned invalid data")
}

fn validate_baseline(left: &BenchmarkSample, right: &BenchmarkSample) -> Result<()> {
    let variance = |a: f64, b: f64| (a - b).abs() / a.max(b).max(1.0);
    if variance(left.single, right.single) > 0.20 || variance(left.multi, right.multi) > 0.20 {
        bail!("Host load changed too much during calibration; retry when the machine is steadier");
    }
    Ok(())
}

fn apply_job_controls(
    job: &OwnedHandle,
    plan: &ControlPlan,
    memory_cap: Option<u64>,
) -> Result<()> {
    let cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
        ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
            CpuRate: u32::from(plan.cpu_hundredths),
        },
    };
    set_job_information(job, JobObjectCpuRateControlInformation, &cpu)?;

    // SAFETY: all-zero is a valid initial value for JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    if let Some(memory_cap) = memory_cap {
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit =
            usize::try_from(memory_cap).context("Memory cap exceeds this platform")?;
    }
    set_job_information(job, JobObjectExtendedLimitInformation, &limits)
}

fn apply_process_controls(
    pid: u32,
    expected_identity: Option<&str>,
    plan: &ControlPlan,
    original_affinity_mask: Option<u64>,
) -> Result<u64> {
    let process = open_process(
        pid,
        PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION,
    )?;
    if let Some(identity) = expected_identity {
        ensure_process_identity(&process, identity)?;
    }
    let allowed = original_affinity_mask
        .and_then(|mask| usize::try_from(mask).ok())
        .context("Studio's original processor affinity is unavailable")?;
    let affinity = first_set_bits(allowed, usize::from(plan.cores));
    if affinity.count_ones() < u32::from(plan.cores) {
        bail!(
            "Studio's allowed processor mask has fewer than {} processors",
            plan.cores
        );
    }
    // SAFETY: process is valid and affinity is a nonzero subset of its original mask.
    if unsafe { SetProcessAffinityMask(process.0, affinity) } == 0 {
        return Err(last_error("SetProcessAffinityMask failed"));
    }
    // SAFETY: process is valid and priority_class returns a documented class.
    if unsafe { SetPriorityClass(process.0, priority_class(plan.priority)) } == 0 {
        return Err(last_error("SetPriorityClass failed"));
    }
    Ok(affinity as u64)
}

fn restore_process_controls(
    pid: u32,
    identity: &str,
    original_affinity_mask: Option<u64>,
    original_priority_class: Option<u32>,
) -> Result<()> {
    let process = open_process(
        pid,
        PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION,
    )?;
    ensure_process_identity(&process, identity)?;
    if let Some(mask) = original_affinity_mask.and_then(|mask| usize::try_from(mask).ok()) {
        // SAFETY: process is valid and mask was captured from this same process identity.
        if unsafe { SetProcessAffinityMask(process.0, mask) } == 0 {
            return Err(last_error("Could not restore Studio processor affinity"));
        }
    }
    if let Some(priority) = original_priority_class {
        // SAFETY: process is valid and priority was captured from this same process identity.
        if unsafe { SetPriorityClass(process.0, priority) } == 0 {
            return Err(last_error("Could not restore Studio priority"));
        }
    }
    Ok(())
}

fn read_process_controls(pid: u32, identity: &str, readback: &mut AppliedReadback) -> Result<()> {
    let process = open_process(
        pid,
        PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    ensure_process_identity(&process, identity)?;
    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    // SAFETY: process is valid and both masks point to initialized writable storage.
    if unsafe { GetProcessAffinityMask(process.0, &mut process_mask, &mut system_mask) } == 0 {
        return Err(last_error("GetProcessAffinityMask failed"));
    }
    // SAFETY: process is a valid process handle with query rights.
    let priority = unsafe { GetPriorityClass(process.0) };
    if priority == 0 {
        return Err(last_error("GetPriorityClass failed"));
    }
    readback.affinity_mask = Some((process_mask & system_mask) as u64);
    readback.priority = priority_from_class(priority);
    Ok(())
}

fn priority_class(priority: Priority) -> u32 {
    match priority {
        Priority::Normal => NORMAL_PRIORITY_CLASS,
        Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Low => IDLE_PRIORITY_CLASS,
    }
}

fn priority_from_class(priority: u32) -> Option<Priority> {
    match priority {
        NORMAL_PRIORITY_CLASS => Some(Priority::Normal),
        BELOW_NORMAL_PRIORITY_CLASS => Some(Priority::BelowNormal),
        IDLE_PRIORITY_CLASS => Some(Priority::Low),
        _ => None,
    }
}

fn neutralize_job(job: &OwnedHandle) -> Result<()> {
    let current_cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION =
        query_job_information(job, JobObjectCpuRateControlInformation)?;
    if current_cpu.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE != 0 {
        let cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: 0,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 0 },
        };
        set_job_information(job, JobObjectCpuRateControlInformation, &cpu)?;
    }
    let current_limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
        query_job_information(job, JobObjectExtendedLimitInformation)?;
    if current_limits.BasicLimitInformation.LimitFlags != 0 {
        // SAFETY: all-zero disables every extended job limit.
        let limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        set_job_information(job, JobObjectExtendedLimitInformation, &limits)?;
    }
    Ok(())
}

fn query_job(job: &OwnedHandle) -> Result<AppliedReadback> {
    let cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION =
        query_job_information(job, JobObjectCpuRateControlInformation)?;
    let limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
        query_job_information(job, JobObjectExtendedLimitInformation)?;
    let accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
        query_job_information(job, JobObjectBasicAccountingInformation)?;
    let cpu_enabled = cpu.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE != 0;
    let flags = limits.BasicLimitInformation.LimitFlags;
    // SAFETY: ControlFlags says the active union field is CpuRate.
    let cpu_rate = unsafe { cpu.Anonymous.CpuRate as u16 };
    Ok(AppliedReadback {
        cpu_hundredths: cpu_enabled.then_some(cpu_rate),
        affinity_mask: None,
        memory_cap_bytes: (flags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0)
            .then_some(limits.JobMemoryLimit as u64),
        priority: None,
        assigned_processes: accounting.ActiveProcesses,
        neutral: !cpu_enabled && flags == 0,
    })
}

fn verify_readback(
    plan: &ControlPlan,
    expected_affinity: u64,
    memory_cap: Option<u64>,
    readback: &AppliedReadback,
) -> Result<()> {
    if readback.cpu_hundredths != Some(plan.cpu_hundredths) {
        bail!("Windows CPU cap readback did not match the requested cap");
    }
    if readback.affinity_mask != Some(expected_affinity) || readback.priority != Some(plan.priority)
    {
        bail!("Windows affinity or priority readback did not match the requested limits");
    }
    if readback.memory_cap_bytes != memory_cap {
        bail!("Windows memory cap readback did not match the requested cap");
    }
    Ok(())
}

fn set_job_information<T>(job: &OwnedHandle, class: i32, value: &T) -> Result<()> {
    // SAFETY: value points to the structure required by class and its exact byte length is supplied.
    if unsafe {
        SetInformationJobObject(
            job.0,
            class,
            (value as *const T).cast::<c_void>(),
            u32::try_from(size_of::<T>())?,
        )
    } == 0
    {
        return Err(last_error("SetInformationJobObject failed"));
    }
    Ok(())
}

fn query_job_information<T>(job: &OwnedHandle, class: i32) -> Result<T> {
    // SAFETY: all queried Job Object information structures used here permit zero initialization.
    let mut value: T = unsafe { zeroed() };
    // SAFETY: value points to writable T storage matching class and the exact byte length is supplied.
    if unsafe {
        QueryInformationJobObject(
            job.0,
            class,
            (&mut value as *mut T).cast::<c_void>(),
            u32::try_from(size_of::<T>())?,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("QueryInformationJobObject failed"));
    }
    Ok(value)
}

fn create_job(name: Option<&str>) -> Result<OwnedHandle> {
    let wide = name.map(wide_null);
    // SAFETY: the optional name is a valid NUL-terminated UTF-16 string for this call.
    let handle = unsafe {
        CreateJobObjectW(
            std::ptr::null(),
            wide.as_ref().map_or(std::ptr::null(), |name| name.as_ptr()),
        )
    };
    OwnedHandle::new(handle, "CreateJobObjectW failed")
}

fn open_job(name: &str) -> Result<Option<OwnedHandle>> {
    let wide = wide_null(name);
    // SAFETY: wide is a valid NUL-terminated UTF-16 string.
    let handle = unsafe { OpenJobObjectW(JOB_RIGHTS, 0, wide.as_ptr()) };
    if handle.is_null() {
        // SAFETY: GetLastError reads thread-local Win32 error state.
        if unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        return Err(last_error("OpenJobObjectW failed"));
    }
    Ok(Some(OwnedHandle(handle)))
}

fn open_process(pid: u32, rights: u32) -> Result<OwnedHandle> {
    // SAFETY: OpenProcess accepts any PID and returns a checked handle.
    let handle = unsafe { OpenProcess(rights, 0, pid) };
    OwnedHandle::new(handle, &format!("OpenProcess({pid}) failed"))
}

fn ensure_process_identity(process: &OwnedHandle, expected: &str) -> Result<()> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut creation = zero;
    let mut exit = zero;
    let mut kernel = zero;
    let mut user = zero;
    // SAFETY: process has query rights and every FILETIME points to writable storage.
    if unsafe { GetProcessTimes(process.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0
    {
        return Err(last_error("GetProcessTimes failed"));
    }
    let actual = ((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
        .to_string();
    if actual != expected {
        bail!("Studio process identity changed during profile operation");
    }
    Ok(())
}

fn ensure_job_queryable(process: &OwnedHandle, job: &OwnedHandle) -> Result<()> {
    let mut ours = 0;
    // SAFETY: both handles are valid and ours is writable.
    if unsafe { IsProcessInJob(process.0, job.0, &mut ours) } == 0 {
        return Err(last_error("IsProcessInJob(Renium) failed"));
    }
    Ok(())
}

fn process_tree(root: u32) -> Result<Vec<u32>> {
    // SAFETY: CreateToolhelp32Snapshot returns a checked snapshot handle.
    let snapshot = OwnedHandle::new(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) },
        "Could not snapshot Windows processes",
    )?;
    // SAFETY: all-zero is a valid initial PROCESSENTRY32W before dwSize is set.
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = u32::try_from(size_of::<PROCESSENTRY32W>())?;
    let mut children = HashMap::<u32, Vec<u32>>::new();
    // SAFETY: snapshot and entry are valid for process enumeration.
    let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while has_entry {
        children
            .entry(entry.th32ParentProcessID)
            .or_default()
            .push(entry.th32ProcessID);
        // SAFETY: snapshot and entry remain valid for the next enumeration call.
        has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    let mut result = Vec::new();
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(pid) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        result.push(pid);
        if let Some(found) = children.get(&pid) {
            pending.extend(found.iter().copied());
        }
    }
    Ok(result)
}

fn first_set_bits(mask: usize, count: usize) -> usize {
    let mut out = 0usize;
    let mut remaining = count;
    for bit in 0..usize::BITS {
        let value = 1usize << bit;
        if mask & value != 0 {
            out |= value;
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
    }
    out
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn wide_null(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_error(context: &str) -> anyhow::Error {
    // SAFETY: GetLastError reads thread-local Win32 error state.
    let code = unsafe { GetLastError() };
    anyhow!("{context} (Windows error {code})")
}

#[cfg(test)]
mod tests {
    use super::first_set_bits;

    #[test]
    fn affinity_selects_only_allowed_processors() {
        assert_eq!(first_set_bits(0b10110, 2), 0b00110);
        assert_eq!(first_set_bits(0b10110, 8), 0b10110);
    }
}
