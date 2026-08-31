use std::collections::BTreeMap;
use std::hint::black_box;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::timing::current_millis;
use crate::app::update;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(windows, target_os = "macos")))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;

const STATE_SCHEMA: u32 = 1;
const CALIBRATION_VERSION: u32 = 1;
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ControlPlan {
    pub(super) cpu_hundredths: u16,
    pub(super) cores: u16,
    pub(super) memory: MemoryPlan,
    pub(super) priority: Priority,
    #[serde(default)]
    pub(super) crash_risk_accepted: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "bytes")]
pub(super) enum MemoryPlan {
    None,
    Headroom(u64),
    Absolute(u64),
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Priority {
    Normal,
    BelowNormal,
    Low,
}

#[derive(Clone)]
struct BuiltinProfile {
    name: &'static str,
    label: &'static str,
    reference_single: f64,
    reference_multi: f64,
    plan: ControlPlan,
}

fn builtin_profiles() -> Vec<BuiltinProfile> {
    vec![
        builtin(
            "iphone-16",
            "iPhone 16",
            [1.00, 1.00],
            control(75, 6, 6 * GIB, Priority::Normal),
        ),
        builtin(
            "iphone-11",
            "iPhone 11",
            [0.43, 0.30],
            control(45, 4, 4 * GIB, Priority::Normal),
        ),
        builtin(
            "pixel-6a",
            "Pixel 6a",
            [0.36, 0.37],
            control(50, 4, 4 * GIB, Priority::Normal),
        ),
        builtin(
            "galaxy-a14",
            "Galaxy A14",
            [0.12, 0.17],
            control(25, 4, 2 * GIB, Priority::BelowNormal),
        ),
        builtin(
            "iphone-6s",
            "iPhone 6s",
            [0.19, 0.12],
            control(18, 2, 1536 * MIB, Priority::BelowNormal),
        ),
        builtin(
            "redmi-2-2015",
            "Redmi 2 (2015)",
            [0.05, 0.04],
            control(8, 2, 768 * MIB, Priority::Low),
        ),
    ]
}

fn builtin(
    name: &'static str,
    label: &'static str,
    reference: [f64; 2],
    plan: ControlPlan,
) -> BuiltinProfile {
    BuiltinProfile {
        name,
        label,
        reference_single: reference[0],
        reference_multi: reference[1],
        plan,
    }
}

fn control(cpu_percent: u16, cores: u16, headroom: u64, priority: Priority) -> ControlPlan {
    ControlPlan {
        cpu_hundredths: cpu_percent * 100,
        cores,
        memory: MemoryPlan::Headroom(headroom),
        priority,
        crash_risk_accepted: true,
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BenchmarkSample {
    pub(super) single: f64,
    pub(super) multi: f64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Calibration {
    version: u32,
    key: String,
    created_unix_ms: u64,
    baseline: BenchmarkSample,
    profiles: BTreeMap<String, BenchmarkSample>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum DesiredProfile {
    #[default]
    Off,
    Builtin {
        name: String,
    },
    Advanced {
        name: Option<String>,
        plan: ControlPlan,
    },
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TransitionPhase {
    #[default]
    Off,
    Inactive,
    Applying,
    Active,
    CleanupPending,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AppliedReadback {
    pub(super) cpu_hundredths: Option<u16>,
    pub(super) affinity_mask: Option<u64>,
    pub(super) memory_cap_bytes: Option<u64>,
    pub(super) priority: Option<Priority>,
    pub(super) assigned_processes: u32,
    pub(super) neutral: bool,
}

#[derive(Clone, Copy)]
pub(super) struct OriginalControls {
    pub(super) affinity_mask: Option<u64>,
    pub(super) priority_class: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Enrollment {
    pid: u32,
    start_identity: String,
    backend_locator: String,
    generation: u64,
    current_commit_bytes: u64,
    #[serde(default)]
    original_affinity_mask: Option<u64>,
    #[serde(default)]
    original_priority_class: Option<u32>,
    residual_membership: bool,
    readback: AppliedReadback,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredState {
    schema_version: u32,
    installation_id: String,
    generation: u64,
    desired: DesiredProfile,
    phase: TransitionPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    calibration: Option<Calibration>,
    #[serde(default)]
    saved_profiles: BTreeMap<String, ControlPlan>,
    #[serde(default)]
    enrollments: Vec<Enrollment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

impl Default for StoredState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA,
            installation_id: format!("{:x}-{:x}", current_millis(), std::process::id()),
            generation: 0,
            desired: DesiredProfile::Off,
            phase: TransitionPhase::Off,
            calibration: None,
            saved_profiles: BTreeMap::new(),
            enrollments: Vec::new(),
            last_error: None,
        }
    }
}

struct Inner {
    state: StoredState,
    load_error: Option<String>,
}

pub(crate) struct Manager {
    path: Option<PathBuf>,
    backend: platform::BackendState,
    inner: Mutex<Inner>,
}

impl Manager {
    pub(crate) fn load() -> Arc<Self> {
        let path = update::user_data_dir()
            .map(|root| root.join("performance-profile.json"))
            .ok();
        let (state, load_error) = match path.as_ref().filter(|path| path.exists()) {
            Some(path) => match std::fs::read(path)
                .with_context(|| format!("Failed to read {}", path.display()))
                .and_then(|bytes| serde_json::from_slice::<StoredState>(&bytes).map_err(Into::into))
                .and_then(|state| {
                    (state.schema_version == STATE_SCHEMA)
                        .then_some(state)
                        .context("Unsupported performance-profile state version")
                }) {
                Ok(state) => (state, None),
                Err(error) => (StoredState::default(), Some(format!("{error:#}"))),
            },
            None => (StoredState::default(), None),
        };
        let manager = Arc::new(Self {
            path,
            backend: platform::BackendState::default(),
            inner: Mutex::new(Inner { state, load_error }),
        });
        if let Err(error) = manager.reconcile_startup() {
            eprintln!("[renium] performance-profile recovery failed: {error:#}");
        }
        manager
    }

    pub(crate) fn command_with_enrollments(
        &self,
        parameters: &Value,
        pids: &[u32],
    ) -> Result<Value> {
        let action = parameters
            .get("action")
            .and_then(Value::as_str)
            .context("Performance profile action is required")?;
        let identities = pids
            .iter()
            .copied()
            .map(|pid| {
                platform::process_identity(pid)
                    .with_context(|| format!("Could not identify Studio process {pid}"))
                    .map(|identity| (pid, identity))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_loaded(&inner)?;
        self.prune_exited(&mut inner.state)?;
        match action {
            "list" => self.list(&inner.state),
            "show" => Ok(self.status(&inner.state)),
            "calibrate" => {
                self.calibrate(&mut inner.state)?;
                Ok(self.status(&inner.state))
            }
            "use" => {
                let name = parameters
                    .get("name")
                    .and_then(Value::as_str)
                    .context("Profile name is required")?;
                self.use_profile(&mut inner.state, name)?;
                if self.add_enrollments(&mut inner.state, identities.iter())? {
                    self.apply_all(&mut inner.state)?;
                }
                Ok(self.status(&inner.state))
            }
            "advanced" => {
                self.use_advanced(&mut inner.state, parameters)?;
                if self.add_enrollments(&mut inner.state, identities.iter())? {
                    self.apply_all(&mut inner.state)?;
                }
                Ok(self.status(&inner.state))
            }
            "off" => {
                self.turn_off(&mut inner.state)?;
                Ok(self.status(&inner.state))
            }
            _ => bail!("Unknown performance profile action '{action}'"),
        }
    }

    pub(crate) fn enroll(&self, pid: u32) -> Result<()> {
        self.enroll_many(&[pid])
    }

    pub(crate) fn enroll_many(&self, pids: &[u32]) -> Result<()> {
        let identities = pids
            .iter()
            .copied()
            .map(|pid| {
                platform::process_identity(pid)
                    .with_context(|| format!("Could not identify Studio process {pid}"))
                    .map(|identity| (pid, identity))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_loaded(&inner)?;
        self.prune_exited(&mut inner.state)?;
        if matches!(inner.state.desired, DesiredProfile::Off) {
            return Ok(());
        }
        let added = self.add_enrollments(&mut inner.state, identities.iter())?;
        if !added {
            return Ok(());
        }
        inner.state.phase = TransitionPhase::Applying;
        self.save(&inner.state)?;
        if !self.calibration_is_current(&inner.state)
            && matches!(inner.state.desired, DesiredProfile::Builtin { .. })
        {
            inner.state.phase = TransitionPhase::Inactive;
            inner.state.last_error = Some("Calibration is stale; run `rbx pf cal`".to_string());
            self.save(&inner.state)?;
            return Ok(());
        }
        self.apply_all(&mut inner.state)
    }

    fn add_enrollments<'a>(
        &self,
        state: &mut StoredState,
        identities: impl IntoIterator<Item = &'a (u32, String)>,
    ) -> Result<bool> {
        let mut added = false;
        for (pid, identity) in identities {
            if state
                .enrollments
                .iter()
                .any(|entry| entry.pid == *pid && entry.start_identity == *identity)
            {
                continue;
            }
            let locator = platform::backend_locator(&state.installation_id, identity);
            let generation = state.generation;
            state.enrollments.push(Enrollment {
                pid: *pid,
                start_identity: identity.clone(),
                backend_locator: locator,
                generation,
                current_commit_bytes: platform::process_tree_commit(*pid).unwrap_or(0),
                original_affinity_mask: platform::process_allowed_affinity(*pid, Some(identity))?,
                original_priority_class: platform::process_priority_class(*pid, Some(identity))?,
                residual_membership: false,
                readback: AppliedReadback::default(),
                error: None,
            });
            added = true;
        }
        Ok(added)
    }

    fn ensure_loaded(&self, inner: &Inner) -> Result<()> {
        if let Some(error) = &inner.load_error {
            bail!("Performance profiles are unavailable because saved state is invalid: {error}");
        }
        if self.path.is_none() {
            bail!("Renium's user data directory is unavailable");
        }
        Ok(())
    }

    fn reconcile_startup(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.load_error.is_some() || self.path.is_none() {
            return Ok(());
        }
        self.prune_exited(&mut inner.state)?;
        if matches!(inner.state.desired, DesiredProfile::Off)
            || matches!(
                inner.state.phase,
                TransitionPhase::CleanupPending | TransitionPhase::Off
            )
        {
            return self.turn_off(&mut inner.state);
        }
        if matches!(inner.state.desired, DesiredProfile::Builtin { .. })
            && !self.calibration_is_current(&inner.state)
        {
            self.neutralize_all(&mut inner.state)?;
            inner.state.phase = TransitionPhase::Inactive;
            inner.state.last_error = Some("Calibration is stale; run `rbx pf cal`".to_string());
            return self.save(&inner.state);
        }
        self.apply_all(&mut inner.state)
    }

    fn list(&self, state: &StoredState) -> Result<Value> {
        let mut profiles = Vec::new();
        for profile in builtin_profiles() {
            let availability = self.profile_availability(state, &profile);
            let achieved = state
                .calibration
                .as_ref()
                .and_then(|calibration| calibration.profiles.get(profile.name));
            profiles.push(json!({
                "name": profile.name,
                "label": profile.label,
                "approximate": true,
                "available": availability.is_none(),
                "reason": availability,
                "cpuPercent": f64::from(profile.plan.cpu_hundredths) / 100.0,
                "cores": profile.plan.cores,
                "memoryHeadroomBytes": match profile.plan.memory { MemoryPlan::Headroom(bytes) => Some(bytes), _ => None },
                "referenceSingle": profile.reference_single,
                "referenceMulti": profile.reference_multi,
                "singleThreadEnforced": false,
                "achievedSingleRatio": achieved.map(|sample| sample.single),
                "achievedMultiRatio": achieved.map(|sample| sample.multi),
            }));
        }
        for name in state.saved_profiles.keys() {
            profiles.push(
                json!({ "name": name, "saved": true, "available": platform::supports_profiles() }),
            );
        }
        Ok(json!({
            "profiles": profiles,
            "backend": platform::backend_name(),
            "calibrated": self.calibration_is_current(state),
            "note": "Device names are approximate performance references, not hardware emulation"
        }))
    }

    fn profile_availability(
        &self,
        state: &StoredState,
        profile: &BuiltinProfile,
    ) -> Option<String> {
        if !platform::supports_profiles() {
            return Some(platform::unsupported_reason().to_string());
        }
        if platform::logical_processors() < usize::from(profile.plan.cores) {
            return Some(format!(
                "Host has fewer than {} logical processors",
                profile.plan.cores
            ));
        }
        if !self.calibration_is_current(state) {
            return Some("Calibration required".to_string());
        }
        let Some(calibration) = state.calibration.as_ref() else {
            return Some("Calibration required".to_string());
        };
        let Some(achieved) = calibration.profiles.get(profile.name) else {
            return Some("Profile was not measured by the current calibration".to_string());
        };
        if achieved.single <= 0.0 || achieved.multi <= 0.0 {
            return Some("Host cannot enforce this profile's processor count".to_string());
        }
        if achieved.multi > 1.10 {
            return Some(
                "Aggregate control plan exceeded native host performance during calibration"
                    .to_string(),
            );
        }
        None
    }

    fn calibration_is_current(&self, state: &StoredState) -> bool {
        state.calibration.as_ref().is_some_and(|calibration| {
            calibration.version == CALIBRATION_VERSION
                && calibration.key == platform::calibration_key()
        })
    }

    fn calibrate(&self, state: &mut StoredState) -> Result<()> {
        if !platform::supports_profiles() {
            bail!("{}", platform::unsupported_reason());
        }
        let had_profile = !matches!(state.desired, DesiredProfile::Off);
        if had_profile {
            state.phase = TransitionPhase::Applying;
            self.save(state)?;
            self.neutralize_all(state)?;
        }
        let plans = builtin_profiles()
            .into_iter()
            .map(|profile| (profile.name.to_string(), profile.plan))
            .collect::<BTreeMap<_, _>>();
        let result = platform::calibrate(&plans);
        match result {
            Ok((baseline, profiles)) => {
                state.calibration = Some(Calibration {
                    version: CALIBRATION_VERSION,
                    key: platform::calibration_key(),
                    created_unix_ms: now_ms(),
                    baseline,
                    profiles,
                });
                state.last_error = None;
                if had_profile {
                    self.apply_all(state)?;
                } else {
                    state.phase = TransitionPhase::Off;
                    self.save(state)?;
                }
                Ok(())
            }
            Err(error) => {
                state.last_error = Some(format!("Calibration failed: {error:#}"));
                if had_profile {
                    let _ = self.apply_all(state);
                }
                self.save(state)?;
                Err(error)
            }
        }
    }

    fn use_profile(&self, state: &mut StoredState, name: &str) -> Result<()> {
        let normalized = name.trim().to_ascii_lowercase();
        let desired = if builtin_profiles()
            .iter()
            .any(|profile| profile.name == normalized)
        {
            if !self.calibration_is_current(state) {
                self.calibrate(state)?;
            }
            let profile = builtin_profiles()
                .into_iter()
                .find(|profile| profile.name == normalized)
                .expect("profile was found");
            if let Some(reason) = self.profile_availability(state, &profile) {
                bail!("Profile '{}' is unavailable: {reason}", profile.name);
            }
            DesiredProfile::Builtin { name: normalized }
        } else if let Some(plan) = state.saved_profiles.get(&normalized).cloned() {
            DesiredProfile::Advanced {
                name: Some(normalized),
                plan,
            }
        } else {
            bail!("Unknown performance profile '{name}'; run `rbx pf ls`");
        };
        state.generation = state.generation.saturating_add(1);
        state.desired = desired;
        state.phase = TransitionPhase::Applying;
        state.last_error = None;
        self.save(state)?;
        self.apply_all(state)
    }

    fn use_advanced(&self, state: &mut StoredState, parameters: &Value) -> Result<()> {
        if !platform::supports_profiles() {
            bail!("{}", platform::unsupported_reason());
        }
        let cpu_percent = parse_number(parameters, "cpu")?.unwrap_or(100.0);
        if !(0.01..=100.0).contains(&cpu_percent) {
            bail!("cpu must be from 0.01 to 100");
        }
        let cores = parse_integer(parameters, "cores")?
            .unwrap_or_else(|| platform::logical_processors() as u64);
        if cores == 0
            || cores > platform::logical_processors() as u64
            || cores > u64::from(u16::MAX)
        {
            bail!("cores must be from 1 to {}", platform::logical_processors());
        }
        let headroom = parameters
            .get("headroom")
            .and_then(Value::as_str)
            .map(parse_bytes)
            .transpose()?;
        let absolute = parameters
            .get("mem")
            .and_then(Value::as_str)
            .map(parse_bytes)
            .transpose()?;
        if headroom.is_some() && absolute.is_some() {
            bail!("Use headroom or mem, not both");
        }
        let risk = parameters.get("risk").and_then(Value::as_str) == Some("crash");
        if absolute.is_some() && !risk {
            bail!("Absolute memory caps can crash Studio; add risk=crash");
        }
        let priority = match parameters
            .get("prio")
            .and_then(Value::as_str)
            .unwrap_or("normal")
        {
            "normal" => Priority::Normal,
            "below" | "below-normal" => Priority::BelowNormal,
            "low" => Priority::Low,
            value => bail!("prio must be normal, below, or low; got '{value}'"),
        };
        let plan = ControlPlan {
            cpu_hundredths: (cpu_percent * 100.0).round() as u16,
            cores: cores as u16,
            memory: absolute
                .map(MemoryPlan::Absolute)
                .or_else(|| headroom.map(MemoryPlan::Headroom))
                .unwrap_or(MemoryPlan::None),
            priority,
            crash_risk_accepted: risk,
        };
        let save_name = parameters
            .get("save")
            .and_then(Value::as_str)
            .map(normalize_saved_name)
            .transpose()?;
        if let Some(name) = &save_name {
            state.saved_profiles.insert(name.clone(), plan.clone());
        }
        state.generation = state.generation.saturating_add(1);
        state.desired = DesiredProfile::Advanced {
            name: save_name,
            plan,
        };
        state.phase = TransitionPhase::Applying;
        state.last_error = None;
        self.save(state)?;
        self.apply_all(state)
    }

    fn desired_plan(&self, state: &StoredState) -> Result<Option<ControlPlan>> {
        match &state.desired {
            DesiredProfile::Off => Ok(None),
            DesiredProfile::Builtin { name } => builtin_profiles()
                .into_iter()
                .find(|profile| profile.name == name)
                .map(|profile| Some(profile.plan))
                .with_context(|| format!("Saved profile '{name}' no longer exists")),
            DesiredProfile::Advanced { plan, .. } => Ok(Some(plan.clone())),
        }
    }

    fn apply_all(&self, state: &mut StoredState) -> Result<()> {
        if let Err(error) = self.apply_all_inner(state) {
            state.phase = TransitionPhase::Inactive;
            state.last_error = Some(format!("{error:#}"));
            self.save(state)?;
            return Err(error);
        }
        Ok(())
    }

    fn apply_all_inner(&self, state: &mut StoredState) -> Result<()> {
        let Some(plan) = self.desired_plan(state)? else {
            return self.turn_off(state);
        };
        state.phase = TransitionPhase::Applying;
        self.save(state)?;

        let mut materialized = Vec::new();
        let mut current_studio_commit = 0u64;
        let mut proposed_studio_commit = 0u64;
        for entry in &state.enrollments {
            if !platform::identity_matches(entry.pid, &entry.start_identity) {
                continue;
            }
            platform::preflight(entry.pid, &entry.start_identity, &entry.backend_locator)?;
            let current = platform::process_tree_commit(entry.pid)?;
            let cap = materialize_memory_cap(
                &plan.memory,
                current,
                entry.generation == state.generation,
                entry.readback.memory_cap_bytes,
            );
            if cap.is_some_and(|cap| cap < current) {
                bail!(
                    "Memory cap is below Studio's current {} byte commit; close work or raise the cap",
                    current
                );
            }
            current_studio_commit = current_studio_commit.saturating_add(current);
            proposed_studio_commit =
                proposed_studio_commit.saturating_add(cap.map_or(current, |cap| cap.max(current)));
            materialized.push((
                entry.pid,
                entry.start_identity.clone(),
                entry.original_affinity_mask,
                entry.original_priority_class,
                current,
                cap,
            ));
        }
        if let Some(commit) = platform::commit_info() {
            let reserve = (commit.limit / 10).max(2 * GIB);
            let projected = projected_commit(
                commit.total,
                current_studio_commit,
                proposed_studio_commit,
                reserve,
            );
            if projected > commit.limit {
                bail!(
                    "Profile memory limits need {} more bytes than the protected system commit budget",
                    projected - commit.limit
                );
            }
        }

        for (pid, identity, original_affinity_mask, original_priority_class, current, cap) in
            materialized
        {
            let index = state
                .enrollments
                .iter()
                .position(|entry| entry.pid == pid && entry.start_identity == identity)
                .expect("materialized enrollment exists");
            let locator = state.enrollments[index].backend_locator.clone();
            let applied = platform::apply(
                &self.backend,
                pid,
                &identity,
                &locator,
                &plan,
                OriginalControls {
                    affinity_mask: original_affinity_mask,
                    priority_class: original_priority_class,
                },
                cap,
            );
            match applied {
                Ok(readback) => {
                    let entry = &mut state.enrollments[index];
                    entry.generation = state.generation;
                    entry.current_commit_bytes = current;
                    entry.residual_membership = false;
                    entry.readback = readback;
                    entry.error = None;
                    self.save(state)?;
                }
                Err(error) => {
                    if let Ok(readback) = platform::neutralize(
                        &self.backend,
                        pid,
                        &identity,
                        &locator,
                        OriginalControls {
                            affinity_mask: original_affinity_mask,
                            priority_class: original_priority_class,
                        },
                    ) {
                        let entry = &mut state.enrollments[index];
                        entry.residual_membership = platform::membership_is_irreversible()
                            && readback.assigned_processes > 0;
                        entry.readback = readback;
                    }
                    state.enrollments[index].error = Some(format!("{error:#}"));
                    state.last_error = Some(format!(
                        "Could not apply profile to Studio {pid}: {error:#}"
                    ));
                    self.save(state)?;
                    return Err(error);
                }
            }
        }
        state.phase = if state.enrollments.is_empty() {
            TransitionPhase::Inactive
        } else {
            TransitionPhase::Active
        };
        state.last_error = None;
        self.save(state)
    }

    fn neutralize_all(&self, state: &mut StoredState) -> Result<()> {
        let mut failure = None;
        for index in 0..state.enrollments.len() {
            let entry = &state.enrollments[index];
            if !platform::identity_matches(entry.pid, &entry.start_identity) {
                continue;
            }
            match platform::neutralize(
                &self.backend,
                entry.pid,
                &entry.start_identity,
                &entry.backend_locator,
                OriginalControls {
                    affinity_mask: entry.original_affinity_mask,
                    priority_class: entry.original_priority_class,
                },
            ) {
                Ok(readback) => {
                    let entry = &mut state.enrollments[index];
                    entry.readback = readback;
                    entry.residual_membership = platform::membership_is_irreversible()
                        && entry.readback.assigned_processes > 0;
                    entry.error = None;
                }
                Err(error) => {
                    let message = format!("Could not neutralize Studio {}: {error:#}", entry.pid);
                    state.enrollments[index].error = Some(message.clone());
                    failure.get_or_insert(message);
                }
            }
            self.save(state)?;
        }
        if let Some(message) = failure {
            bail!(message);
        }
        Ok(())
    }

    fn turn_off(&self, state: &mut StoredState) -> Result<()> {
        state.desired = DesiredProfile::Off;
        state.phase = TransitionPhase::CleanupPending;
        self.save(state)?;
        if let Err(error) = self.neutralize_all(state) {
            state.last_error = Some(format!("{error:#}"));
            self.save(state)?;
            return Err(error);
        }
        let released = state
            .enrollments
            .iter()
            .filter(|entry| !entry.residual_membership)
            .map(|entry| entry.backend_locator.clone())
            .collect::<Vec<_>>();
        state.enrollments.retain(|entry| entry.residual_membership);
        for locator in released {
            self.backend.release(&locator);
        }
        state.phase = TransitionPhase::Off;
        state.last_error = None;
        self.save(state)
    }

    fn prune_exited(&self, state: &mut StoredState) -> Result<()> {
        let before = state.enrollments.len();
        let mut released = Vec::new();
        state.enrollments.retain(|entry| {
            let alive = platform::identity_matches(entry.pid, &entry.start_identity);
            if !alive {
                released.push(entry.backend_locator.clone());
            }
            alive
        });
        for locator in released {
            self.backend.release(&locator);
        }
        if state.enrollments.len() != before {
            self.save(state)?;
        }
        Ok(())
    }

    fn status(&self, state: &StoredState) -> Value {
        let selected = match &state.desired {
            DesiredProfile::Off => Value::Null,
            DesiredProfile::Builtin { name } => Value::String(name.clone()),
            DesiredProfile::Advanced { name, .. } => name.as_ref().map_or_else(
                || Value::String("advanced".to_string()),
                |name| Value::String(name.clone()),
            ),
        };
        let studios = state
            .enrollments
            .iter()
            .map(|entry| {
                let mut status = serde_json::Map::from_iter([
                    ("pid".to_string(), json!(entry.pid)),
                    ("neutral".to_string(), json!(entry.readback.neutral)),
                ]);
                if entry.readback.assigned_processes > 0 {
                    status.insert(
                        "processes".to_string(),
                        json!(entry.readback.assigned_processes),
                    );
                }
                if !entry.readback.neutral {
                    status.insert(
                        "cpuPercent".to_string(),
                        json!(
                            entry
                                .readback
                                .cpu_hundredths
                                .map(|value| f64::from(value) / 100.0)
                        ),
                    );
                    status.insert(
                        "cores".to_string(),
                        json!(entry.readback.affinity_mask.map(u64::count_ones)),
                    );
                    status.insert(
                        "memoryCapBytes".to_string(),
                        json!(entry.readback.memory_cap_bytes),
                    );
                    status.insert("priority".to_string(), json!(entry.readback.priority));
                }
                if entry.residual_membership {
                    status.insert("residualMembership".to_string(), Value::Bool(true));
                }
                if let Some(error) = &entry.error {
                    status.insert("error".to_string(), Value::String(error.clone()));
                }
                Value::Object(status)
            })
            .collect::<Vec<_>>();
        json!({
            "selected": selected,
            "phase": phase_name(state.phase),
            "backend": platform::backend_name(),
            "calibrated": self.calibration_is_current(state),
            "memoryCapsCanCrashStudio": state.enrollments.iter().any(|entry| entry.readback.memory_cap_bytes.is_some()),
            "studios": studios,
            "error": state.last_error,
        })
    }

    fn save(&self, state: &StoredState) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("Renium user data directory is unavailable")?;
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        update::install_bytes(path, &bytes)
            .with_context(|| format!("Failed to save {}", path.display()))
    }
}

fn phase_name(phase: TransitionPhase) -> &'static str {
    match phase {
        TransitionPhase::Off => "off",
        TransitionPhase::Inactive => "inactive",
        TransitionPhase::Applying => "applying",
        TransitionPhase::Active => "active",
        TransitionPhase::CleanupPending => "cleanup-pending",
    }
}

fn parse_number(parameters: &Value, name: &str) -> Result<Option<f64>> {
    parameters
        .get(name)
        .and_then(Value::as_str)
        .map(|value| {
            value
                .parse::<f64>()
                .with_context(|| format!("{name} must be a number"))
        })
        .transpose()
}

fn parse_integer(parameters: &Value, name: &str) -> Result<Option<u64>> {
    parameters
        .get(name)
        .and_then(Value::as_str)
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("{name} must be an integer"))
        })
        .transpose()
}

fn parse_bytes(value: &str) -> Result<u64> {
    let value = value.trim().to_ascii_lowercase();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number = value[..split]
        .parse::<f64>()
        .context("Memory value must begin with a number")?;
    let multiplier = match value[split..].trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        suffix => bail!("Unknown memory suffix '{suffix}'"),
    };
    let bytes = number * multiplier;
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        bail!("Memory value is out of range");
    }
    Ok(bytes.round() as u64)
}

fn normalize_saved_name(value: &str) -> Result<String> {
    let name = value.trim().to_ascii_lowercase();
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("save must use 1-48 letters, numbers, dashes, or underscores");
    }
    if builtin_profiles()
        .iter()
        .any(|profile| profile.name == name)
    {
        bail!("save cannot replace built-in profile '{name}'");
    }
    Ok(name)
}

fn now_ms() -> u64 {
    current_millis().min(u128::from(u64::MAX)) as u64
}

fn projected_commit(
    current_total: u64,
    current_studio: u64,
    proposed_studio: u64,
    reserve: u64,
) -> u64 {
    current_total
        .saturating_sub(current_studio)
        .saturating_add(proposed_studio)
        .saturating_add(reserve)
}

fn materialize_memory_cap(
    plan: &MemoryPlan,
    current: u64,
    same_generation: bool,
    prior: Option<u64>,
) -> Option<u64> {
    match plan {
        MemoryPlan::None => None,
        MemoryPlan::Headroom(_) if same_generation && prior.is_some() => prior,
        MemoryPlan::Headroom(bytes) => Some(current.saturating_add(*bytes)),
        MemoryPlan::Absolute(bytes) => Some(*bytes),
    }
}

pub(crate) fn run_worker() -> Result<()> {
    let mut gate = [0u8; 1];
    std::io::stdin()
        .read_exact(&mut gate)
        .context("Performance worker was not started by the Renium daemon")?;
    let sample = benchmark();
    println!("{}", serde_json::to_string(&sample)?);
    Ok(())
}

pub(crate) fn run_holder(pid: u32, identity: &str, locator: &str) -> Result<()> {
    platform::run_holder(pid, identity, locator)
}

fn benchmark() -> BenchmarkSample {
    const SAMPLE_TIME: Duration = Duration::from_millis(500);
    let single = benchmark_lane(SAMPLE_TIME);
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(32);
    let gate = Arc::new(std::sync::Barrier::new(threads));
    let workers = (0..threads)
        .map(|_| {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.wait();
                benchmark_lane(SAMPLE_TIME)
            })
        })
        .collect::<Vec<_>>();
    let multi = workers
        .into_iter()
        .filter_map(|worker| worker.join().ok())
        .sum();
    BenchmarkSample { single, multi }
}

fn benchmark_lane(duration: Duration) -> f64 {
    let started = Instant::now();
    let mut iterations = 0u64;
    let mut value = 0x9e37_79b9_7f4a_7c15u64;
    while started.elapsed() < duration {
        for _ in 0..4096 {
            value ^= value << 7;
            value ^= value >> 9;
            value = value.wrapping_mul(0xd6e8_feb8_6659_fd93);
        }
        iterations += 4096;
    }
    black_box(value);
    iterations as f64 / started.elapsed().as_secs_f64()
}

#[derive(Clone, Copy)]
pub(super) struct CommitInfo {
    pub(super) total: u64,
    pub(super) limit: u64,
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryPlan, materialize_memory_cap, normalize_saved_name, parse_bytes, projected_commit,
    };

    #[test]
    fn parses_compact_memory_values() {
        assert_eq!(parse_bytes("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("768m").unwrap(), 768 * 1024 * 1024);
    }

    #[test]
    fn validates_saved_profile_names() {
        assert_eq!(normalize_saved_name("Low-End").unwrap(), "low-end");
        assert!(normalize_saved_name("iphone-11").is_err());
        assert!(normalize_saved_name("bad name").is_err());
    }

    #[test]
    fn commit_projection_replaces_current_studio_usage() {
        assert_eq!(projected_commit(10_000, 3_000, 5_000, 1_000), 13_000);
    }

    #[test]
    fn commit_projection_cannot_underflow() {
        assert_eq!(projected_commit(1_000, 2_000, 500, 250), 750);
    }

    #[test]
    fn headroom_cap_is_stable_within_one_profile_generation() {
        let plan = MemoryPlan::Headroom(50);
        assert_eq!(materialize_memory_cap(&plan, 100, false, None), Some(150));
        assert_eq!(
            materialize_memory_cap(&plan, 80, true, Some(150)),
            Some(150)
        );
        assert_eq!(
            materialize_memory_cap(&plan, 80, false, Some(150)),
            Some(130)
        );
    }
}
