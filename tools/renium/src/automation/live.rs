use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

use super::context as bound_context;
use super::reconcile::{
    AppliedEditorChanges, BaselineSide, Coordinator, PairConfiguration, PairMode, PairSetup,
    push_project_delta, studio_change_guard_from_state,
};
use super::runtime::{
    acknowledge_pulled_changes, automation_failure_ref, automation_pull_args, automation_push_args,
};
use super::{BoundContext, StudioReopenTarget};
use crate::app::output::{ensure_plugin_api_ok, log_global};
use crate::app::timing::elapsed_ms;
use crate::editor::sync::{StudioChangeGuard, StudioChangedBeforePush};
use crate::project::config;
use crate::snapshot::export::{
    PublishEntryState, PublishedProjectChanges, export_snapshots_with_warm_bridge,
};
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::files::{OnDrop, atomic_write_file, fnv1a};
use crate::system::watch::FileWatcher;

const EVENT_DEBOUNCE: Duration = Duration::from_millis(10);
const SETTLE_QUIET_PERIOD: Duration = Duration::from_millis(100);
const STUDIO_PULL_SETTLE_LIMIT: Duration = Duration::from_secs(2);
const RESCAN_RETRY: Duration = Duration::from_millis(500);
const MAX_PUSH_RETRY_DELAY: Duration = Duration::from_secs(5);
const ENABLED_FILE: &str = "live-watch-state.enabled";
const ENABLED_MARKER_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnabledMarker {
    version: u8,
    target: String,
}

pub(super) fn log_live_timing(label: &str, started: Instant) {
    log_global(
        4,
        format_args!("[renium] live {label}: {:.1}ms", elapsed_ms(started)),
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    directory: bool,
    length: u64,
    hash: u64,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    running: bool,
    mode: String,
    pull_changes: bool,
    paused: bool,
    syncing: bool,
    pending_paths: Vec<String>,
    pushes: u64,
    pulls: u64,
    resolution_required: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    auto_desynced_packages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_desynced_at_push: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginLiveStatus {
    running: bool,
    paused: bool,
    read_only: bool,
    resolution_required: bool,
    error: Option<String>,
}

impl Control {
    fn plugin_live_status(&self) -> PluginLiveStatus {
        let status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        PluginLiveStatus {
            running: status.running,
            paused: status.paused,
            read_only: status.mode == "verify",
            resolution_required: status.resolution_required,
            error: status.error.clone(),
        }
    }
}

fn report_plugin_live_status(bridge: &BridgeServer, runtime_id: &str, status: &PluginLiveStatus) {
    if let Err(error) = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "compact": true, "liveSyncStatus": status }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(1)),
    ) {
        log_global(
            5,
            format_args!("[renium] live status display update failed: {error:#}"),
        );
    }
}

#[derive(Default)]
struct LivePushResult {
    accepted: BTreeMap<PathBuf, Option<PublishEntryState>>,
    auto_desynced_packages: Vec<String>,
}

fn auto_desynced_packages(summary: &serde_json::Map<String, Value>) -> Vec<String> {
    summary
        .get("autoDesyncedPackages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn record_successful_push(status: &mut Status, auto_desynced_packages: Vec<String>) {
    status.pushes = status.pushes.saturating_add(1);
    if !auto_desynced_packages.is_empty() {
        status.auto_desynced_packages = auto_desynced_packages;
        status.auto_desynced_at_push = Some(status.pushes);
    }
}

struct Control {
    stop: AtomicBool,
    retry: AtomicBool,
    retry_pull: AtomicBool,
    reset: AtomicBool,
    pull_changes: AtomicBool,
    writes_enabled: AtomicBool,
    file_pause_count: AtomicU64,
    generation: AtomicU64,
    settle_activity: AtomicU64,
    file_changes: Mutex<FileChanges>,
    sync_active: Mutex<bool>,
    sync_idle: Condvar,
    reset_serial: Mutex<()>,
    reset_state: Mutex<ResetState>,
    reset_event: Condvar,
    root: PathBuf,
    status: Mutex<Status>,
    plugin_state: Mutex<Value>,
    studio_wait_error: Mutex<Option<String>>,
    finished: Mutex<bool>,
    finished_event: Condvar,
}

#[derive(Default)]
struct FileChanges {
    notified: BTreeSet<PathBuf>,
    queued: BTreeSet<PathBuf>,
    settled: BTreeSet<PathBuf>,
}

#[derive(Default)]
struct ResetState {
    requested: u64,
    completed: u64,
    error: Option<String>,
    rebase: Option<RebaseRequest>,
}

enum Rebase {
    Captured(CapturedState),
    Published(PublishedProjectChanges),
}

struct RebaseRequest {
    value: Rebase,
    resume: bool,
}

pub(crate) struct CapturedState {
    full: bool,
    scopes: BTreeSet<PathBuf>,
    entries: BTreeMap<PathBuf, Option<FileStamp>>,
}

impl Control {
    fn new(
        root: PathBuf,
        pull_changes: bool,
        files_paused: bool,
        mode: PairMode,
        resolution_required: bool,
    ) -> Self {
        Self {
            stop: AtomicBool::new(false),
            retry: AtomicBool::new(false),
            retry_pull: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            pull_changes: AtomicBool::new(pull_changes),
            writes_enabled: AtomicBool::new(mode.writes()),
            file_pause_count: AtomicU64::new(u64::from(files_paused)),
            generation: AtomicU64::new(0),
            settle_activity: AtomicU64::new(0),
            file_changes: Mutex::new(FileChanges::default()),
            sync_active: Mutex::new(false),
            sync_idle: Condvar::new(),
            reset_serial: Mutex::new(()),
            reset_state: Mutex::new(ResetState::default()),
            reset_event: Condvar::new(),
            root,
            status: Mutex::new(Status {
                running: true,
                mode: mode.name().to_string(),
                pull_changes,
                paused: files_paused,
                resolution_required,
                ..Status::default()
            }),
            plugin_state: Mutex::new(json!({})),
            studio_wait_error: Mutex::new(None),
            finished: Mutex::new(false),
            finished_event: Condvar::new(),
        }
    }

    fn settle(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let paths = paths
            .into_iter()
            .map(|path| absolute(path, &self.root))
            .collect::<Vec<_>>();
        let mut changes = self
            .file_changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for path in paths {
            changes.notified.remove(&path);
            changes.queued.remove(&path);
            changes.settled.insert(path);
        }
        drop(changes);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn queue(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let paths = paths
            .into_iter()
            .map(|path| absolute(path, &self.root))
            .collect::<Vec<_>>();
        let mut changes = self
            .file_changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for path in &paths {
            changes.notified.remove(path);
            changes.settled.remove(path);
            changes.queued.insert(path.clone());
        }
        drop(changes);
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        let mut pending = status
            .pending_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        pending.extend(paths.iter().map(|path| {
            path.strip_prefix(&self.root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        }));
        status.pending_paths = pending.into_iter().collect();
        drop(status);
        self.notify_sync_state();
    }

    fn notify_files(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut changes = self
            .file_changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        changes
            .notified
            .extend(paths.into_iter().map(|path| absolute(path, &self.root)));
        drop(changes);
        self.notify_sync_state();
    }

    fn rebase_then_resume(&self, rebase: Rebase) -> Result<()> {
        self.rebase(rebase, true)
    }

    fn rebase_without_resume(&self, rebase: Rebase) -> Result<()> {
        self.rebase(rebase, false)
    }

    fn rebase(&self, rebase: Rebase, resume: bool) -> Result<()> {
        let _serial = self
            .reset_serial
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *self
            .file_changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = FileChanges::default();
        let sequence = {
            let mut state = self
                .reset_state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.requested = state.requested.saturating_add(1);
            state.error = None;
            state.rebase = Some(RebaseRequest {
                value: rebase,
                resume,
            });
            state.requested
        };
        self.reset.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while state.completed < sequence {
            if *self.finished.lock().unwrap_or_else(PoisonError::into_inner) {
                self.release_pause();
                anyhow::bail!("Live sync watcher stopped before refreshing its project state");
            }
            state = self
                .reset_event
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if let Some(error) = state.error.take() {
            anyhow::bail!(error);
        }
        Ok(())
    }

    fn take_rebase(&self) -> Option<(u64, RebaseRequest)> {
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let sequence = state.requested;
        state.rebase.take().map(|rebase| (sequence, rebase))
    }

    fn complete_reset(&self, sequence: u64, error: Option<String>) {
        let mut state = self
            .reset_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.completed = sequence;
        state.error = error;
        drop(state);
        self.reset_event.notify_all();
    }

    fn retry(&self) {
        self.retry.store(true, Ordering::Release);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .error = None;
        self.notify_sync_state();
    }

    fn set_pull_changes(&self, pull_changes: bool) {
        let changed = self.pull_changes.swap(pull_changes, Ordering::AcqRel) != pull_changes;
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pull_changes = pull_changes;
        if changed && pull_changes {
            self.retry_pull.store(true, Ordering::Release);
        }
        if changed {
            self.notify_sync_state();
        }
    }

    fn set_mode(&self, mode: PairMode) {
        self.writes_enabled.store(mode.writes(), Ordering::Release);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .mode = mode.name().to_string();
    }

    fn set_resolution_required(&self, required: bool) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if status.resolution_required != required {
            status.resolution_required = required;
            drop(status);
            self.notify_sync_state();
        }
    }

    fn pause(&self) {
        self.file_pause_count.fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paused = true;
        self.notify_sync_state();
        let mut active = self
            .sync_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *active {
            active = self
                .sync_idle
                .wait(active)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn replace_pause(&self, paused: bool) {
        self.file_pause_count.store(1, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paused = true;
        self.notify_sync_state();
        let mut active = self
            .sync_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *active {
            active = self
                .sync_idle
                .wait(active)
                .unwrap_or_else(PoisonError::into_inner);
        }
        self.file_pause_count
            .store(u64::from(paused), Ordering::Release);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paused = paused;
        self.notify_sync_state();
    }

    fn resume(&self, paths: impl IntoIterator<Item = PathBuf>) {
        self.settle(paths);
        self.release_pause();
    }

    fn release_pause(&self) {
        let previous = self
            .file_pause_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(count.saturating_sub(1))
            })
            .unwrap_or(0);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paused = previous > 1;
        self.notify_sync_state();
    }

    fn begin_sync(&self, generation: u64) -> Option<SyncActivity<'_>> {
        if self.file_pause_count.load(Ordering::Acquire) > 0 {
            return None;
        }
        let mut active = self
            .sync_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.file_pause_count.load(Ordering::Acquire) > 0
            || self.generation.load(Ordering::Acquire) != generation
        {
            return None;
        }
        *active = true;
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .syncing = true;
        Some(SyncActivity { control: self })
    }

    fn wait_settled(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut activity = self.settle_activity.load(Ordering::Acquire);
        let mut quiet_since = Instant::now();
        let mut active = self
            .sync_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            let status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
            let plugin = self
                .plugin_state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let plugin_pending = plugin["pendingChanges"].as_u64().unwrap_or(0) > 0
                || plugin["dirtyServices"]
                    .as_array()
                    .is_some_and(|services| !services.is_empty());
            let settled =
                !*active && !status.paused && status.pending_paths.is_empty() && !plugin_pending;
            let cannot_progress = !status.running
                || status.paused
                || status.error.is_some()
                || status.resolution_required
                || (plugin_pending && !self.pull_changes.load(Ordering::Acquire));
            drop(plugin);
            drop(status);
            if settled {
                let current_activity = self.settle_activity.load(Ordering::Acquire);
                if current_activity != activity {
                    activity = current_activity;
                    quiet_since = Instant::now();
                } else if quiet_since.elapsed() >= SETTLE_QUIET_PERIOD {
                    return true;
                }
            }
            if cannot_progress {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let wait_for = if settled {
                remaining.min(SETTLE_QUIET_PERIOD.saturating_sub(quiet_since.elapsed()))
            } else {
                remaining.min(SETTLE_QUIET_PERIOD)
            };
            let (next, _) = self
                .sync_idle
                .wait_timeout(active, wait_for)
                .unwrap_or_else(PoisonError::into_inner);
            active = next;
            let current_activity = self.settle_activity.load(Ordering::Acquire);
            if current_activity != activity {
                activity = current_activity;
                quiet_since = Instant::now();
            }
        }
    }

    fn notify_sync_state(&self) {
        self.settle_activity.fetch_add(1, Ordering::Release);
        self.sync_idle.notify_all();
    }

    fn snapshot(&self) -> Value {
        json!(
            self.status
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        )
    }

    fn set_plugin_state(&self, state: Value) {
        *self
            .plugin_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = state;
        self.notify_sync_state();
    }

    fn plugin_snapshot(&self) -> Value {
        self.plugin_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update_pending(&self, pending: &BTreeSet<PathBuf>) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_paths = pending
            .iter()
            .map(|path| {
                path.strip_prefix(&self.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        self.notify_sync_state();
    }

    fn fail(&self, error: String) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .error = Some(error);
        self.notify_sync_state();
    }

    fn clear_error(&self) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .error = None;
        self.notify_sync_state();
    }

    fn fail_studio_wait(&self, message: String) {
        *self
            .studio_wait_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.clone());
        self.fail(message);
    }

    fn clear_studio_wait_error(&self) {
        let expected = self
            .studio_wait_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if expected
            .as_ref()
            .is_some_and(|error| status.error.as_ref() == Some(error))
        {
            status.error = None;
        }
    }

    fn finish(&self) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running = false;
        self.notify_sync_state();
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.finished_event.notify_all();
        self.reset_event.notify_all();
    }

    fn wait_finished(&self) {
        let mut finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        while !*finished {
            finished = self
                .finished_event
                .wait(finished)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn stop_and_wait(&self, bridge: &BridgeServer, runtime_id: Option<&str>) {
        self.stop.store(true, Ordering::Release);
        if let Some(runtime_id) = runtime_id
            && let Err(error) = bridge.call_for_runtime_with_timeout(
                "cancelStudioChangeWait",
                json!({}),
                BridgeTarget::Edit,
                runtime_id,
                Some(Duration::from_secs(1)),
            )
        {
            log_global(
                5,
                format_args!("[renium] cancel live change wait: {error:#}"),
            );
        }
        self.wait_finished();
    }
}

struct SyncActivity<'a> {
    control: &'a Control,
}

impl Drop for SyncActivity<'_> {
    fn drop(&mut self) {
        *self
            .control
            .sync_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
        self.control
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .syncing = false;
        self.control.notify_sync_state();
    }
}

#[derive(Default)]
pub(crate) struct Manager {
    sessions: Mutex<HashMap<String, Session>>,
    aliases: Mutex<HashMap<u64, SessionAlias>>,
    starts: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    transitions: Arc<Mutex<()>>,
    next_session: AtomicU64,
    coordinator: Arc<Coordinator>,
}

#[derive(Clone, PartialEq, Eq)]
struct SessionAlias {
    key: String,
    session_id: u64,
}

struct Session {
    id: u64,
    control: Arc<Control>,
    bridge: Arc<BridgeServer>,
    runtime_id: Option<String>,
}

pub(crate) struct StartResult {
    pub(crate) status: Value,
    created: Option<SessionAlias>,
}

impl Manager {
    pub(crate) fn saved_studio_target(
        &self,
        context: &BoundContext,
    ) -> Result<Option<StudioReopenTarget>> {
        saved_studio_target_for_root(Path::new(&context.root))
    }

    pub(crate) fn saved_local_file(&self, context: &BoundContext) -> Result<Option<PathBuf>> {
        self.coordinator.saved_local_file(context)
    }

    pub(crate) fn transition_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.transitions)
    }

    pub(crate) fn ensure_target_available(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<()> {
        if let Some(owner) = self.coordinator.target_owner(context, bridge)? {
            bail!(
                "This Studio place is already owned by {owner}; stop that project's Live Sync first"
            );
        }
        Ok(())
    }

    fn start_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut starts = self.starts.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            starts
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }
    fn session_alias(&self, context_id: u64) -> Option<SessionAlias> {
        self.aliases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&context_id)
            .cloned()
    }

    fn session_key(&self, context_id: u64) -> Option<String> {
        let alias = self.session_alias(context_id)?;
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&alias.key)
            .filter(|session| session.id == alias.session_id)
            .map(|_| alias.key)
    }

    fn control(&self, context_id: u64) -> Option<Arc<Control>> {
        let alias = self.session_alias(context_id)?;
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&alias.key)
            .filter(|session| session.id == alias.session_id)
            .map(|session| Arc::clone(&session.control))
    }

    pub(crate) fn attach(&self, context: &BoundContext, bridge: &BridgeServer) -> Result<bool> {
        crate::plugins::verify_place_lease(context.place_id, context.resource_lease.as_ref())?;
        let key = self.coordinator.pair_key(context, bridge)?;
        let alias = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .and_then(|session| {
                let running = session
                    .control
                    .status
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .running;
                (running
                    && !session.control.stop.load(Ordering::Acquire)
                    && session.runtime_id == context.runtime_id)
                    .then(|| SessionAlias {
                        key: key.clone(),
                        session_id: session.id,
                    })
            });
        if let Some(alias) = alias {
            self.aliases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(context.id, alias);
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn start(
        &self,
        context: BoundContext,
        bridge: Arc<BridgeServer>,
        pull_changes: Option<bool>,
        files_paused: bool,
        reset_files_paused: bool,
        configuration: PairConfiguration,
    ) -> Result<StartResult> {
        let phase = Instant::now();
        let setup = self.coordinator.prepare(&context, &bridge, configuration)?;
        log_live_timing("startup pair preparation", phase);
        let pair_key = setup.key.clone();
        let mut release_owner = OnDrop::new(|| self.coordinator.release_target(&pair_key));
        let result = self.start_prepared(
            context,
            bridge,
            pull_changes,
            files_paused,
            reset_files_paused,
            setup,
        );
        if result.is_ok() {
            release_owner.disarm();
        }
        result
    }

    fn start_prepared(
        &self,
        context: BoundContext,
        bridge: Arc<BridgeServer>,
        pull_changes: Option<bool>,
        files_paused: bool,
        reset_files_paused: bool,
        mut setup: PairSetup,
    ) -> Result<StartResult> {
        let total_started = Instant::now();
        log_global(
            5,
            format_args!(
                "[renium] live start prepared: cx={} pair={} reconcile={}",
                context.id, setup.key, setup.requires_reconcile
            ),
        );
        let start_lock = self.start_lock(&setup.key);
        let _start = start_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some((session_id, control, runtime_id)) =
            sessions.get(&setup.key).map(|session| {
                (
                    session.id,
                    Arc::clone(&session.control),
                    session.runtime_id.clone(),
                )
            })
        {
            let running = control
                .status
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .running;
            if running
                && !control.stop.load(Ordering::Acquire)
                && runtime_id == context.runtime_id
                && !setup.requires_reconcile
            {
                control.set_mode(setup.mode);
                control.set_resolution_required(setup.resolution_required);
                if let Some(pull_changes) = pull_changes {
                    control.set_pull_changes(pull_changes);
                }
                if reset_files_paused {
                    control.replace_pause(files_paused);
                } else if files_paused && control.file_pause_count.load(Ordering::Acquire) == 0 {
                    control.pause();
                }
                drop(sessions);
                self.aliases
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        context.id,
                        SessionAlias {
                            key: setup.key,
                            session_id,
                        },
                    );
                return Ok(StartResult {
                    status: control.snapshot(),
                    created: None,
                });
            }
            drop(sessions);
            control.stop_and_wait(&bridge, runtime_id.as_deref());
            sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            if sessions.get(&setup.key).is_some_and(|current| {
                current.id == session_id && Arc::ptr_eq(&current.control, &control)
            }) {
                sessions.remove(&setup.key);
            }
            drop(sessions);
            self.aliases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|_, alias| alias.key != setup.key || alias.session_id != session_id);
            sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        }
        drop(sessions);
        if let Some(owner) = self.coordinator.claim_setup_target(&setup) {
            bail!(
                "This Studio place is already owned by {owner}; stop that project's Live Sync first"
            );
        }
        let phase = Instant::now();
        self.coordinator.reconcile(&context, &bridge, &mut setup)?;
        log_live_timing("startup reconcile", phase);
        let phase = Instant::now();
        let project = open_watch_project(&context)?;
        log_live_timing("startup watcher open", phase);
        let phase = Instant::now();
        let current = scan(&project)?;
        log_live_timing("startup filesystem scan", phase);
        let phase = Instant::now();
        let baseline = current.clone();
        log_live_timing("startup baseline clone", phase);
        let control = Arc::new(Control::new(
            PathBuf::from(&context.root),
            pull_changes.unwrap_or(true),
            files_paused,
            setup.mode,
            setup.resolution_required,
        ));
        if let Some(error) = setup.error {
            control.fail(error);
        }
        let session_id = self.next_session.fetch_add(1, Ordering::Relaxed) + 1;
        let context_id = context.id;
        let runtime_id = context.runtime_id.clone();
        let session_bridge = Arc::clone(&bridge);
        let pair_key = setup.key.clone();
        let worker_control = Arc::clone(&control);
        let coordinator = Arc::clone(&self.coordinator);
        let owner_coordinator = Arc::clone(&coordinator);
        let owner_key = pair_key.clone();
        let report_bridge = Arc::clone(&session_bridge);
        let report_runtime = runtime_id.clone();
        let (start_sender, start_receiver) = mpsc::sync_channel(0);
        thread::Builder::new()
            .name(format!("renium-live-{context_id}"))
            .spawn(move || {
                if start_receiver.recv().is_err() {
                    owner_coordinator.release_target(&owner_key);
                    worker_control.finish();
                    return;
                }
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(Worker {
                        context,
                        bridge,
                        project,
                        baseline,
                        current,
                        control: worker_control.clone(),
                        coordinator,
                        pair_key,
                    })
                })) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        log_global(5, format_args!("[renium] live worker failed: {error:#}"));
                        worker_control.fail(format!("{error:#}"));
                    }
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|message| (*message).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        worker_control.fail(format!("Live sync watcher panicked: {message}"));
                    }
                }
                worker_control
                    .status
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .running = false;
                if let Some(runtime_id) = report_runtime.as_deref() {
                    report_plugin_live_status(
                        &report_bridge,
                        runtime_id,
                        &worker_control.plugin_live_status(),
                    );
                }
                owner_coordinator.release_target(&owner_key);
                worker_control.finish();
            })
            .context("Failed to start live sync watcher")?;
        let session_key = setup.key;
        let session_alias = SessionAlias {
            key: session_key.clone(),
            session_id,
        };
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                session_key.clone(),
                Session {
                    id: session_id,
                    control: Arc::clone(&control),
                    bridge: session_bridge,
                    runtime_id,
                },
            );
        self.aliases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(context_id, session_alias.clone());
        if start_sender.send(()).is_err() {
            self.stop_session(&session_alias);
            bail!("Live sync watcher stopped before startup");
        }
        log_live_timing("startup total", total_started);
        Ok(StartResult {
            status: control.snapshot(),
            created: Some(session_alias),
        })
    }

    pub(crate) fn update_runtime_settings(
        &self,
        context: BoundContext,
        bridge: Arc<BridgeServer>,
        configuration: PairConfiguration,
    ) -> Result<Option<Value>> {
        let Some(control) = self.control(context.id) else {
            return Ok(None);
        };
        let setup = self.coordinator.prepare(&context, &bridge, configuration)?;
        log_global(
            5,
            format_args!(
                "[renium] live runtime settings: cx={} reconcile={}",
                context.id, setup.requires_reconcile
            ),
        );
        if !setup.requires_reconcile {
            return Ok(None);
        }
        let pull_changes = control.pull_changes.load(Ordering::Acquire);
        let files_paused = control.file_pause_count.load(Ordering::Acquire) > 0;
        self.start_prepared(
            context,
            bridge,
            Some(pull_changes),
            files_paused,
            true,
            setup,
        )
        .map(|started| Some(started.status))
    }

    pub(crate) fn rollback_start(&self, started: &StartResult) {
        if let Some(alias) = &started.created {
            self.stop_session(alias);
        }
    }

    pub(crate) fn status(&self, context_id: u64) -> Value {
        self.control(context_id)
            .as_deref()
            .map_or_else(|| json!({ "running": false }), |control| control.snapshot())
    }

    pub(crate) fn plugin_status(&self, context_id: u64) -> Value {
        self.control(context_id)
            .as_deref()
            .map_or_else(|| json!({}), Control::plugin_snapshot)
    }

    pub(crate) fn set_plugin_status(&self, context_id: u64, state: Value) {
        if let Some(control) = self.control(context_id) {
            control.set_plugin_state(state);
        }
    }

    pub(crate) fn baseline_files(
        &self,
        context: &BoundContext,
        paths: &[PathBuf],
    ) -> Result<Value> {
        let key = self
            .session_key(context.id)
            .context("Live sync has no reconciliation context")?;
        Ok(json!(
            self.coordinator.baseline_files(context, &key, paths)?
        ))
    }

    pub(crate) fn wait_settled(&self, context_id: u64, timeout: Duration) -> bool {
        self.control(context_id)
            .as_deref()
            .is_some_and(|control| control.wait_settled(timeout))
    }

    pub(crate) fn set_enabled(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        enabled: bool,
    ) -> Result<()> {
        let path = enabled_path(context);
        if enabled {
            let marker = EnabledMarker {
                version: ENABLED_MARKER_VERSION,
                target: self.coordinator.target_key(context, bridge)?,
            };
            atomic_write_file(&path, &serde_json::to_vec(&marker)?)
        } else {
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("Failed to remove {}", path.display()))
                }
            }
        }
    }

    pub(crate) fn enabled(&self, context: &BoundContext, bridge: &BridgeServer) -> Result<bool> {
        let bytes = match fs::read(enabled_path(context)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("Failed to read persisted Live Sync state"),
        };
        Ok(enabled_marker_matches(
            &bytes,
            &self.coordinator.target_key(context, bridge)?,
        ))
    }

    pub(crate) fn retry(&self, context_id: u64) -> Value {
        if let Some(control) = self.control(context_id) {
            control.retry();
        }
        self.status(context_id)
    }

    pub(crate) fn capture(
        &self,
        context: &BoundContext,
        paths: Option<&[PathBuf]>,
    ) -> Result<Option<CapturedState>> {
        if self.control(context.id).is_none() {
            return Ok(None);
        }
        let project = open_watch_project(context)?;
        let (full, scopes, entries) = if let Some(paths) = paths {
            let scopes = paths
                .iter()
                .cloned()
                .map(|path| absolute(path, &project.root))
                .collect::<BTreeSet<_>>();
            let paths = scopes.iter().cloned().collect::<Vec<_>>();
            let entries = capture_pending(&project, &BTreeMap::new(), &paths)?;
            (false, scopes, entries)
        } else {
            (
                true,
                BTreeSet::new(),
                scan(&project)?
                    .into_iter()
                    .map(|(path, stamp)| (path, Some(stamp)))
                    .collect(),
            )
        };
        Ok(Some(CapturedState {
            full,
            scopes,
            entries,
        }))
    }

    pub(crate) fn discard(&self, context: &BoundContext) -> Result<Value> {
        if let Some(control) = self.control(context.id) {
            control.pause();
            let captured = match self.capture(context, None) {
                Ok(Some(captured)) => captured,
                Ok(None) => {
                    control.release_pause();
                    return Ok(self.status(context.id));
                }
                Err(error) => {
                    control.release_pause();
                    return Err(error);
                }
            };
            control.rebase_then_resume(Rebase::Captured(captured))?;
        }
        Ok(self.status(context.id))
    }

    pub(crate) fn settle(&self, context_id: u64, paths: impl IntoIterator<Item = PathBuf>) {
        if let Some(control) = self.control(context_id) {
            control.settle(paths);
        }
    }

    pub(crate) fn queue(&self, context_id: u64, paths: impl IntoIterator<Item = PathBuf>) -> Value {
        if let Some(control) = self.control(context_id) {
            control.queue(paths);
        }
        self.status(context_id)
    }

    pub(crate) fn notify_files(
        &self,
        context_id: u64,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Value {
        if let Some(control) = self.control(context_id) {
            control.notify_files(paths);
        }
        self.status(context_id)
    }

    pub(crate) fn rebase_then_resume(
        &self,
        context_id: u64,
        captured: CapturedState,
    ) -> Result<Value> {
        if let Some(control) = self.control(context_id) {
            control.rebase_then_resume(Rebase::Captured(captured))?;
        }
        Ok(self.status(context_id))
    }

    pub(crate) fn acknowledge_then_resume(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        captured: CapturedState,
        side: BaselineSide,
    ) -> Result<Value> {
        self.acknowledge_then_resume_inner(context, bridge, captured, side, false)
    }

    pub(crate) fn acknowledge_then_resume_with_gate_held(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        captured: CapturedState,
        side: BaselineSide,
    ) -> Result<Value> {
        self.acknowledge_then_resume_inner(context, bridge, captured, side, true)
    }

    fn acknowledge_then_resume_inner(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        captured: CapturedState,
        side: BaselineSide,
        request_gate_held: bool,
    ) -> Result<Value> {
        let full = captured.full;
        let paths = captured.scopes.iter().cloned().collect::<Vec<_>>();
        if let Some(control) = self.control(context.id) {
            if let Err(error) = control.rebase_without_resume(Rebase::Captured(captured)) {
                control.set_mode(PairMode::Verify);
                control.fail(format!(
                    "Live sync could not rebase its file watcher: {error:#}"
                ));
                control.release_pause();
                return Err(error);
            }
            let result = (|| {
                if full {
                    log_global(
                        5,
                        format_args!("[renium] reconcile reason: full acknowledgment"),
                    );
                    if !request_gate_held {
                        bail!("Full push acknowledgment requires the bridge request gate");
                    }
                    let key = self
                        .session_key(context.id)
                        .context("Live sync has no reconciliation context")?;
                    self.coordinator
                        .record_full_editor_push_with_gate_held(context, &key, bridge)?;
                } else if let Some(key) = self.session_key(context.id) {
                    self.coordinator
                        .advance_baseline(context, &key, &paths, side)?;
                }
                Ok(())
            })();
            if let Err(error) = &result {
                control.set_mode(PairMode::Verify);
                control.fail(format!(
                    "Live sync could not record the shared state: {error:#}"
                ));
            }
            control.release_pause();
            result?;
        }
        Ok(self.status(context.id))
    }

    pub(crate) fn reconcile_then_resume(
        &self,
        context: &BoundContext,
        published: PublishedProjectChanges,
    ) -> Result<Value> {
        let paths = published.changed_roots.clone();
        if let Some(control) = self.control(context.id) {
            if let Err(error) = control.rebase_without_resume(Rebase::Published(published)) {
                control.set_mode(PairMode::Verify);
                control.fail(format!(
                    "Live sync could not rebase its file watcher: {error:#}"
                ));
                control.release_pause();
                return Err(error);
            }
            let result = if let Some(key) = self.session_key(context.id) {
                self.coordinator
                    .advance_baseline(context, &key, &paths, BaselineSide::Studio)
            } else {
                Ok(())
            };
            if let Err(error) = &result {
                control.set_mode(PairMode::Verify);
                control.fail(format!(
                    "Live sync could not record the shared state: {error:#}"
                ));
            }
            control.release_pause();
            result?;
        }
        Ok(self.status(context.id))
    }

    pub(crate) fn pause(&self, context_id: u64) -> Value {
        if let Some(control) = self.control(context_id) {
            control.pause();
        }
        self.status(context_id)
    }

    pub(crate) fn resume(
        &self,
        context_id: u64,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Value {
        if let Some(control) = self.control(context_id) {
            control.resume(paths);
        }
        self.status(context_id)
    }

    pub(crate) fn stop(&self, context_id: u64) -> Value {
        let Some(alias) = self.session_alias(context_id) else {
            return json!({ "running": false });
        };
        self.stop_session(&alias)
    }

    fn stop_session(&self, alias: &SessionAlias) -> Value {
        let session = {
            self.sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&alias.key)
                .filter(|session| session.id == alias.session_id)
                .map(|session| {
                    (
                        Arc::clone(&session.control),
                        Arc::clone(&session.bridge),
                        session.runtime_id.clone(),
                    )
                })
        };
        if let Some((control, bridge, runtime_id)) = session {
            control.stop_and_wait(&bridge, runtime_id.as_deref());
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            if sessions.get(&alias.key).is_some_and(|current| {
                current.id == alias.session_id && Arc::ptr_eq(&current.control, &control)
            }) {
                sessions.remove(&alias.key);
            }
            drop(sessions);
            self.aliases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|_, current| current != alias);
            let mut stopped = control.snapshot();
            if let Some(object) = stopped.as_object_mut()
                && let Some(error) = object.remove("error")
            {
                object.insert("previousError".to_string(), error);
            }
            stopped
        } else {
            self.aliases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|_, current| current != alias);
            json!({ "running": false })
        }
    }

    pub(crate) fn cancel(&self, context_id: u64) {
        let mut aliases = self.aliases.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(alias) = aliases.remove(&context_id) else {
            return;
        };
        let still_shared = aliases.values().any(|current| current == &alias);
        drop(aliases);
        if !still_shared {
            self.stop_session(&alias);
        }
    }
}

fn enabled_path(context: &BoundContext) -> PathBuf {
    Path::new(&context.root).join(".renium").join(ENABLED_FILE)
}

pub(crate) fn saved_studio_target_for_root(root: &Path) -> Result<Option<StudioReopenTarget>> {
    let path = root.join(".renium").join(ENABLED_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to read persisted Live Sync state at {}",
                    path.display()
                )
            });
        }
    };
    Ok(saved_studio_target(&bytes))
}

fn enabled_marker_matches(bytes: &[u8], target: &str) -> bool {
    serde_json::from_slice::<EnabledMarker>(bytes)
        .is_ok_and(|marker| marker.version == ENABLED_MARKER_VERSION && marker.target == target)
}

fn saved_studio_target(bytes: &[u8]) -> Option<StudioReopenTarget> {
    let marker = serde_json::from_slice::<EnabledMarker>(bytes).ok()?;
    if marker.version != ENABLED_MARKER_VERSION {
        return None;
    }
    if let Some(ids) = marker.target.strip_prefix("published:") {
        let (game_id, place_id) = ids.split_once(':')?;
        let game_id = game_id.parse::<i64>().ok().filter(|id| *id > 0)?;
        let place_id = place_id.parse::<i64>().ok().filter(|id| *id > 0)?;
        return Some(StudioReopenTarget {
            file: None,
            game_id: Some(game_id),
            place_id: Some(place_id),
        });
    }
    marker
        .target
        .strip_prefix("local-file:")
        .filter(|path| !path.is_empty())
        .map(|path| StudioReopenTarget {
            file: Some(PathBuf::from(path)),
            game_id: None,
            place_id: None,
        })
}

struct WatchProject {
    watcher: FileWatcher,
    root: PathBuf,
    roots: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    full_push: BTreeSet<PathBuf>,
}

fn open_watch_project(context: &BoundContext) -> Result<WatchProject> {
    let loaded = config::load_project(Some(Path::new(&context.project)), None)?;
    crate::project::version_control::ensure_renium_local_state_ignored(&loaded.root)?;
    let inputs = config::project_watch_inputs(&loaded)?;
    let roots = inputs
        .directories
        .into_iter()
        .map(|path| absolute(path, Path::new(&context.root)))
        .collect::<BTreeSet<_>>();
    let files = inputs
        .files
        .into_iter()
        .map(|path| absolute(path, Path::new(&context.root)))
        .collect::<BTreeSet<_>>();
    let full_push = inputs
        .full_push
        .into_iter()
        .map(|path| absolute(path, Path::new(&context.root)))
        .collect::<BTreeSet<_>>();
    let mut watcher = FileWatcher::new(4096)?;
    watcher.set_inputs(&files, &roots)?;
    Ok(WatchProject {
        watcher,
        root: PathBuf::from(&context.root),
        roots,
        files,
        full_push,
    })
}

fn absolute(path: PathBuf, root: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn ignored_under(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        relative
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.ends_with(".renium.lock")
                    || name.ends_with(".rbxl.lock")
                    || name.ends_with(".rbxlx.lock")
            })
            || relative.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| matches!(name, ".git" | ".renium"))
            })
    })
}

fn relevant(project: &WatchProject, path: &Path) -> bool {
    project.files.contains(path)
        || project
            .roots
            .iter()
            .any(|root| path == root || path.starts_with(root) && !ignored_under(path, root))
}

#[cfg(windows)]
fn transient_file_read_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(not(windows))]
fn transient_file_read_error(_error: &std::io::Error) -> bool {
    false
}

fn read_stamp_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut retries = 0;
    loop {
        match fs::read(path) {
            Err(error) if retries < 20 && transient_file_read_error(&error) => {
                retries += 1;
                thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

fn stamp(path: &Path) -> Result<Option<FileStamp>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    let directory = metadata.is_dir();
    let (length, hash) = if directory {
        (0, 0)
    } else {
        let bytes = match read_stamp_bytes(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()));
            }
        };
        (
            u64::try_from(bytes.len()).context("File is too large to fingerprint")?,
            fnv1a(&bytes),
        )
    };
    Ok(Some(FileStamp {
        directory,
        length,
        hash,
    }))
}

fn scan(project: &WatchProject) -> Result<BTreeMap<PathBuf, FileStamp>> {
    let mut stamps = BTreeMap::new();
    for path in &project.files {
        if let Some(value) = stamp(path)? {
            stamps.insert(path.clone(), value);
        }
    }
    for root in &project.roots {
        if root.is_file() {
            if let Some(value) = stamp(root)? {
                stamps.insert(root.clone(), value);
            }
            continue;
        }
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| !ignored_under(entry.path(), root))
        {
            let entry = entry.with_context(|| format!("Failed to scan {}", root.display()))?;
            if let Some(value) = stamp(entry.path())? {
                stamps.insert(entry.into_path(), value);
            }
        }
    }
    Ok(stamps)
}

fn capture_pending(
    project: &WatchProject,
    baseline: &BTreeMap<PathBuf, FileStamp>,
    paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Option<FileStamp>>> {
    let mut captured = paths
        .iter()
        .map(|path| Ok((path.clone(), stamp(path)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    for path in paths {
        let directory = captured
            .get(path)
            .and_then(|stamp| *stamp)
            .is_some_and(|stamp| stamp.directory)
            || baseline.get(path).is_some_and(|stamp| stamp.directory);
        if !directory {
            continue;
        }
        for descendant in baseline
            .keys()
            .filter(|candidate| candidate.starts_with(path))
        {
            captured.entry(descendant.clone()).or_insert(None);
        }
        if !path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(path)
            .into_iter()
            .filter_entry(|entry| !ignored_under(entry.path(), path))
        {
            let entry = entry.with_context(|| format!("Failed to scan {}", path.display()))?;
            let entry_path = entry.into_path();
            if relevant(project, &entry_path)
                && let Some(stamp) = stamp(&entry_path)?
            {
                captured.insert(entry_path, Some(stamp));
            }
        }
    }
    Ok(captured)
}

fn queue_changed(
    project: &WatchProject,
    baseline: &BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<bool> {
    let mut changed = false;
    for path in paths {
        let path = absolute(path, &project.root);
        if !relevant(project, &path) {
            continue;
        }
        let current = stamp(&path)?;
        if baseline.get(&path).copied() != current {
            let added = pending.insert(path.clone());
            let edited_after_failure = blocked
                .get(&path)
                .is_some_and(|attempted| *attempted != current);
            if edited_after_failure {
                blocked.remove(&path);
            }
            changed |= added || edited_after_failure;
        } else {
            changed |= pending.remove(&path);
            blocked.remove(&path);
        }
    }
    Ok(changed)
}

fn queue_scan_changes(
    baseline: &BTreeMap<PathBuf, FileStamp>,
    current: &BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
) -> bool {
    let mut changed = false;
    for path in baseline.keys().chain(current.keys()) {
        let current_stamp = current.get(path).copied();
        if baseline.get(path).copied() != current_stamp {
            let added = pending.insert(path.clone());
            let edited_after_failure = blocked
                .get(path)
                .is_some_and(|attempted| *attempted != current_stamp);
            if edited_after_failure {
                blocked.remove(path);
            }
            changed |= added || edited_after_failure;
        } else {
            changed |= pending.remove(path);
            blocked.remove(path);
        }
    }
    changed
}

fn unblock_full_push(project: &WatchProject, blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>) {
    blocked.retain(|path, _| !project.full_push.contains(path));
}

fn push_full(
    context: &BoundContext,
    bridge: &BridgeServer,
    guard: Option<&StudioChangeGuard>,
) -> Result<LivePushResult> {
    let parameters = json!({ "verifySources": true });
    let _selection = bound_context::select(context);
    bridge.clear_runtime_pins();
    if let Some(runtime_id) = context.runtime_id.as_deref() {
        bridge.pin_runtime(BridgeTarget::Main, runtime_id);
        bridge.pin_runtime(BridgeTarget::Edit, runtime_id);
    }
    let services = super::reconcile::sync_services();
    let summary = push_project_delta(
        context,
        bridge,
        &services,
        automation_push_args(context, &parameters, false)?,
        guard,
    )?;
    if summary.get("skippedByReview").and_then(Value::as_bool) == Some(true) {
        bail!("Studio changes are waiting for review");
    }
    Ok(LivePushResult {
        accepted: BTreeMap::new(),
        auto_desynced_packages: auto_desynced_packages(&summary),
    })
}

struct PulledStudioChanges {
    published: PublishedProjectChanges,
    services: Vec<String>,
    seq: u64,
    runtime_id: String,
}

fn pull_studio_changes(
    context: &BoundContext,
    bridge: &BridgeServer,
    control: &Control,
    pending_state: Option<Value>,
) -> Result<Option<PulledStudioChanges>> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Live sync context has no Studio runtime")?;
    let state = if let Some(state) = pending_state {
        state
    } else {
        read_live_studio_change_state(bridge, runtime_id)?
    };
    let state = settle_studio_change_state(bridge, runtime_id, state)?;
    // Reconciliation may already have acknowledged this queued notification.
    // Publish the fresh empty state too, or --wait retains the old dirty cache
    // until the next long-poll response despite all data already being synced.
    control.set_plugin_state(state.clone());
    if state["twoWaySyncEnabled"].as_bool() == Some(false) {
        return Ok(None);
    }
    let services = state["dirtyServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if services.is_empty() {
        return Ok(None);
    }
    let seq = state["seq"]
        .as_u64()
        .context("Studio change state did not include seq")?;
    let state_runtime_id = state["runtimeId"]
        .as_str()
        .context("Studio change state did not include runtimeId")?
        .to_string();
    let parameters = json!({
        "services": &services,
        "importMode": "staged",
    });
    let _gate = bridge.acquire_request_gate();
    let _selection = bound_context::select(context);
    bridge.clear_runtime_pins();
    bridge.pin_runtime(BridgeTarget::Main, runtime_id);
    bridge.pin_runtime(BridgeTarget::Edit, runtime_id);
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let published = export_snapshots_with_warm_bridge(
        automation_pull_args(context, &parameters, true)?,
        bridge,
        &info,
        0.0,
        false,
        state["referencePathsMayChange"].as_bool().unwrap_or(false),
    )?;
    Ok(Some(PulledStudioChanges {
        published,
        services,
        seq,
        runtime_id: state_runtime_id,
    }))
}

fn read_live_studio_change_state(bridge: &BridgeServer, runtime_id: &str) -> Result<Value> {
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": true }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(1)),
    )?;
    ensure_plugin_api_ok(&state)?;
    Ok(state)
}

fn settle_studio_change_state(
    bridge: &BridgeServer,
    runtime_id: &str,
    mut state: Value,
) -> Result<Value> {
    ensure_plugin_api_ok(&state)?;
    let deadline = Instant::now() + STUDIO_PULL_SETTLE_LIMIT;
    loop {
        thread::sleep(SETTLE_QUIET_PERIOD);
        let next = read_live_studio_change_state(bridge, runtime_id)?;
        let revision = studio_change_revision(&state);
        let settled = revision.is_some() && revision == studio_change_revision(&next);
        state = next;
        if settled || Instant::now() >= deadline {
            return Ok(state);
        }
    }
}

fn studio_change_revision(state: &Value) -> Option<(&str, u64)> {
    Some((state["runtimeId"].as_str()?, state["seq"].as_u64()?))
}

struct StudioPushState {
    pending: bool,
    guard: StudioChangeGuard,
}

fn studio_push_state(context: &BoundContext, bridge: &BridgeServer) -> Result<StudioPushState> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Live sync context has no Studio runtime")?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": true, "includeGenerations": true }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    let guard = studio_change_guard_from_state(context, &state)?;
    Ok(StudioPushState {
        pending: state["dirtyServices"]
            .as_array()
            .is_some_and(|services| !services.is_empty()),
        guard,
    })
}

enum StudioEvent {
    State(Value),
    Error(anyhow::Error),
}

fn wait_for_studio_event(context: &BoundContext, bridge: &BridgeServer) -> Result<Value> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Live sync context has no Studio runtime")?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "start": true,
            "waitSeconds": 25,
            "compact": true,
        }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(27)),
    )?;
    ensure_plugin_api_ok(&state)?;
    Ok(state)
}

fn start_studio_event_waiter(
    context: BoundContext,
    bridge: Arc<BridgeServer>,
    control: Arc<Control>,
) -> Result<(Receiver<StudioEvent>, SyncSender<()>)> {
    let (event_sender, events) = mpsc::channel();
    let (resume, resume_receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(format!("renium-studio-events-{}", context.id))
        .spawn(move || {
            while !control.stop.load(Ordering::Acquire) && bridge.alive.load(Ordering::Acquire) {
                let event = match wait_for_studio_event(&context, &bridge) {
                    Ok(state) => StudioEvent::State(state),
                    Err(error) => StudioEvent::Error(error),
                };
                if event_sender.send(event).is_err() {
                    break;
                }
                loop {
                    if control.stop.load(Ordering::Acquire) || !bridge.alive.load(Ordering::Acquire)
                    {
                        return;
                    }
                    match resume_receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(()) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
        })
        .context("Failed to start Studio event waiter")?;
    Ok((events, resume))
}

#[derive(Default)]
struct StudioEventState {
    wait_paused: bool,
    resume_at: Option<Instant>,
    pull_ready: bool,
    retry_delay: Duration,
    pending_payload: Option<Value>,
}

fn poll_studio_event(
    control: &Control,
    events: &Receiver<StudioEvent>,
    resume: &SyncSender<()>,
    state: &mut StudioEventState,
) -> Result<()> {
    if state
        .resume_at
        .is_some_and(|resume_at| Instant::now() >= resume_at)
    {
        let _ = resume.try_send(());
        state.wait_paused = false;
        state.resume_at = None;
    }
    if state.wait_paused {
        return Ok(());
    }
    match events.try_recv() {
        Ok(StudioEvent::State(payload)) => {
            control.set_plugin_state(payload.clone());
            control.clear_studio_wait_error();
            let has_changes = payload["dirtyServices"]
                .as_array()
                .is_some_and(|services| !services.is_empty());
            let enabled = payload["twoWaySyncEnabled"].as_bool() != Some(false);
            if has_changes && enabled {
                state.pending_payload = Some(payload);
                state.wait_paused = true;
                state.pull_ready = true;
                state.retry_delay = Duration::ZERO;
                return Ok(());
            }
            state.pending_payload = None;
            if enabled {
                let _ = resume.try_send(());
            } else {
                state.wait_paused = true;
                state.resume_at = Some(Instant::now() + RESCAN_RETRY);
            }
        }
        Ok(StudioEvent::Error(error)) => {
            let failure = automation_failure_ref(&error);
            if failure.0.c == "no_studio" {
                return Err(error);
            }
            control.fail_studio_wait(format!("Studio live sync failed: {}", failure.0.m));
            state.wait_paused = true;
            state.retry_delay = next_retry_delay(state.retry_delay);
            state.resume_at = Some(Instant::now() + state.retry_delay);
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => bail!("Studio change waiter stopped"),
    }
    Ok(())
}

fn next_retry_delay(current: Duration) -> Duration {
    if current.is_zero() {
        Duration::from_millis(250)
    } else {
        (current * 2).min(MAX_PUSH_RETRY_DELAY)
    }
}

fn published_state_matches(
    root: &Path,
    relative: &Path,
    expected: Option<&PublishEntryState>,
    current: Option<&FileStamp>,
) -> bool {
    match (expected, current) {
        (None, None) => true,
        (Some(PublishEntryState::Directory), Some(current)) => current.directory,
        (Some(PublishEntryState::File { length, hash, .. }), Some(current)) => {
            !current.directory && current.length == *length && current.hash == *hash
        }
        (Some(PublishEntryState::Symlink(target)), Some(_)) => {
            fs::read_link(root.join(relative)).is_ok_and(|actual| actual == *target)
        }
        _ => false,
    }
}

fn published_state_label(state: Option<&PublishEntryState>) -> String {
    match state {
        None => "missing".to_string(),
        Some(PublishEntryState::Directory) => "directory".to_string(),
        Some(PublishEntryState::File { length, hash, .. }) => {
            format!("file({length},{hash:016x})")
        }
        Some(PublishEntryState::Symlink(target)) => format!("symlink({})", target.display()),
    }
}

fn file_stamp_label(state: Option<&FileStamp>) -> String {
    match state {
        None => "missing".to_string(),
        Some(state) if state.directory => "directory".to_string(),
        Some(state) => format!("file({},{:016x})", state.length, state.hash),
    }
}

fn record_accepted_stamp(
    root: &Path,
    path: &Path,
    expected: Option<&PublishEntryState>,
    current: Option<&FileStamp>,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
) {
    let matches = published_state_matches(root, path, expected, current);
    // Advance to the bytes actually accepted, even if the editor has since
    // changed back to the old baseline. A later event must not cancel that edit.
    let accepted = match expected {
        Some(PublishEntryState::Directory) => Some(FileStamp {
            directory: true,
            length: 0,
            hash: 0,
        }),
        Some(PublishEntryState::File { length, hash, .. }) => Some(FileStamp {
            directory: false,
            length: *length,
            hash: *hash,
        }),
        Some(PublishEntryState::Symlink(_)) if matches => current.copied(),
        _ => None,
    };
    if let Some(accepted) = accepted {
        baseline.insert(path.to_path_buf(), accepted);
    } else {
        baseline.remove(path);
    }
    if matches {
        pending.remove(path);
    } else {
        pending.insert(path.to_path_buf());
    }
}

fn reconcile_published_changes(
    project: &WatchProject,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    current: &BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    published: &PublishedProjectChanges,
) {
    let candidates = baseline
        .keys()
        .chain(current.keys())
        .cloned()
        .chain(
            published
                .expected
                .keys()
                .map(|path| project.root.join(path)),
        )
        .collect::<BTreeSet<_>>();
    for path in candidates {
        let relative = path.strip_prefix(&project.root).unwrap_or(&path);
        if let Some(expected) = published.expected.get(relative) {
            record_accepted_stamp(
                &project.root,
                &path,
                expected.as_ref(),
                current.get(&path),
                baseline,
                pending,
            );
        } else if baseline.get(&path) != current.get(&path) {
            log_global(
                5,
                format_args!(
                    "[renium] live publish mismatch: {} expected={} current={}",
                    relative.display(),
                    published_state_label(
                        published.expected.get(relative).and_then(Option::as_ref)
                    ),
                    file_stamp_label(current.get(&path))
                ),
            );
            pending.insert(path);
        } else {
            pending.remove(&path);
        }
    }
}

struct Worker {
    context: BoundContext,
    bridge: Arc<BridgeServer>,
    project: WatchProject,
    baseline: BTreeMap<PathBuf, FileStamp>,
    current: BTreeMap<PathBuf, FileStamp>,
    control: Arc<Control>,
    coordinator: Arc<Coordinator>,
    pair_key: String,
}

#[derive(Default)]
struct ProjectEventOutcome {
    changed: bool,
    rescan: bool,
}

fn receive_project_event(
    project: &WatchProject,
    baseline: &BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
    control: &Control,
    timeout: Duration,
) -> Result<ProjectEventOutcome> {
    let mut outcome = ProjectEventOutcome::default();
    match project.watcher.receiver().recv_timeout(timeout) {
        Ok(Ok(_)) if control.file_pause_count.load(Ordering::Acquire) > 0 => {
            outcome.rescan = true;
        }
        Ok(Ok(event)) => {
            let had_blocked_changes = !blocked.is_empty();
            match queue_changed(project, baseline, pending, blocked, event.paths) {
                Ok(changed) => {
                    outcome.changed = changed;
                    if changed {
                        unblock_full_push(project, blocked);
                        control.update_pending(pending);
                    }
                }
                Err(error) => {
                    control.fail(format!(
                        "Project watcher could not read a changed file: {error:#}"
                    ));
                    outcome.rescan = true;
                }
            }
            if had_blocked_changes && blocked.is_empty() {
                control.clear_error();
            }
        }
        Ok(Err(error)) => {
            control.fail(format!("Project watcher failed: {error}"));
            outcome.rescan = true;
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            bail!("Project watcher stopped")
        }
    }
    Ok(outcome)
}

fn rescan_project_if_due(
    project: &WatchProject,
    baseline: &BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
    control: &Control,
    last_rescan: &mut Instant,
    rescan_pending: &mut bool,
) -> bool {
    if !*rescan_pending
        || control.file_pause_count.load(Ordering::Acquire) > 0
        || last_rescan.elapsed() < RESCAN_RETRY
    {
        return false;
    }
    *last_rescan = Instant::now();
    match scan(project) {
        Ok(current) => {
            let had_blocked_changes = !blocked.is_empty();
            let changed = queue_scan_changes(baseline, &current, pending, blocked);
            if changed {
                unblock_full_push(project, blocked);
                control.update_pending(pending);
            }
            if had_blocked_changes && blocked.is_empty() {
                control.clear_error();
            }
            *rescan_pending = false;
            changed
        }
        Err(error) => {
            control.fail(format!("Project watcher rescan failed: {error:#}"));
            false
        }
    }
}

fn apply_captured_rebase(
    project: &WatchProject,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    snapshot: &BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
    captured: CapturedState,
) {
    if captured.full {
        *baseline = captured
            .entries
            .into_iter()
            .filter(|(path, _)| relevant(project, path))
            .filter_map(|(path, stamp)| stamp.map(|stamp| (path, stamp)))
            .collect();
    } else {
        let candidates = baseline
            .keys()
            .chain(snapshot.keys())
            .filter(|path| captured.scopes.iter().any(|scope| path.starts_with(scope)))
            .cloned()
            .collect::<BTreeSet<_>>();
        for path in candidates {
            let expected = captured.entries.get(&path).copied().unwrap_or(None);
            if !relevant(project, &path) || snapshot.get(&path).copied() != expected {
                continue;
            }
            if let Some(stamp) = expected {
                baseline.insert(path, stamp);
            } else {
                baseline.remove(&path);
            }
        }
    }
    blocked.clear();
    pending.clear();
    queue_scan_changes(baseline, snapshot, pending, blocked);
}

fn apply_rebase_request(
    context: &BoundContext,
    project: &mut WatchProject,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
    control: &Control,
) -> Result<Option<bool>> {
    if !control.reset.swap(false, Ordering::AcqRel) {
        return Ok(None);
    }
    let (reset_sequence, rebase) = control
        .take_rebase()
        .context("Live sync rebase request was missing")?;
    let RebaseRequest { value, resume } = rebase;
    let refreshed = open_watch_project(context)
        .and_then(|current| scan(&current).map(|snapshot| (current, snapshot)));
    match refreshed {
        Ok((current, snapshot)) => {
            *project = current;
            match value {
                Rebase::Captured(captured) => {
                    apply_captured_rebase(project, baseline, &snapshot, pending, blocked, captured);
                }
                Rebase::Published(published) => {
                    blocked.clear();
                    reconcile_published_changes(project, baseline, &snapshot, pending, &published);
                }
            }
            control.update_pending(pending);
            control.clear_error();
            if resume {
                control.release_pause();
            }
            control.complete_reset(reset_sequence, None);
            Ok(Some(!pending.is_empty()))
        }
        Err(error) => {
            let message = format!("Live sync could not refresh the project: {error:#}");
            control.fail(message.clone());
            if resume {
                control.release_pause();
            }
            control.complete_reset(reset_sequence, Some(message));
            Err(error)
        }
    }
}

#[derive(Default)]
struct FileControlOutcome {
    push_ready: Option<bool>,
    push_immediately: bool,
    retry_later: bool,
}

fn record_current_stamp(
    path: &Path,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
) -> Result<()> {
    record_stamp(path, stamp(path)?, baseline, pending, blocked);
    Ok(())
}

fn record_stamp(
    path: &Path,
    current: Option<FileStamp>,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
) {
    if let Some(current) = current {
        baseline.insert(path.to_path_buf(), current);
    } else {
        baseline.remove(path);
    }
    pending.remove(path);
    blocked.remove(path);
}

fn apply_file_control_changes(
    project: &WatchProject,
    baseline: &mut BTreeMap<PathBuf, FileStamp>,
    pending: &mut BTreeSet<PathBuf>,
    blocked: &mut BTreeMap<PathBuf, Option<FileStamp>>,
    control: &Control,
) -> Result<FileControlOutcome> {
    let (notified, queued, settled) = {
        let mut changes = control
            .file_changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (
            std::mem::take(&mut changes.notified),
            std::mem::take(&mut changes.queued),
            std::mem::take(&mut changes.settled),
        )
    };
    let mut outcome = FileControlOutcome::default();
    if !notified.is_empty() && queue_changed(project, baseline, pending, blocked, notified)? {
        unblock_full_push(project, blocked);
        outcome.push_ready = Some(true);
        outcome.push_immediately = true;
        control.update_pending(pending);
    }
    if !queued.is_empty() {
        let mut accepted = false;
        for path in queued {
            if relevant(project, &path) {
                blocked.remove(&path);
                pending.insert(path);
                accepted = true;
            }
        }
        if accepted {
            unblock_full_push(project, blocked);
            outcome.push_ready = Some(true);
            outcome.push_immediately = true;
        }
        control.update_pending(pending);
    }
    if settled.is_empty() {
        return Ok(outcome);
    }
    for path in settled {
        if !relevant(project, &path) {
            continue;
        }
        if let Err(error) = record_current_stamp(&path, baseline, pending, blocked) {
            control
                .file_changes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .settled
                .insert(path);
            control.fail(format!(
                "Live sync could not settle a written file: {error:#}"
            ));
            outcome.retry_later = true;
        }
    }
    outcome.push_ready = Some(pending.iter().any(|path| !blocked.contains_key(path)));
    control.update_pending(pending);
    Ok(outcome)
}

struct PreparedPush {
    paths: Vec<PathBuf>,
    refresh_project: bool,
    captured: BTreeMap<PathBuf, Option<FileStamp>>,
    guard: StudioChangeGuard,
}

struct LiveLoop {
    context: BoundContext,
    bridge: Arc<BridgeServer>,
    project: WatchProject,
    baseline: BTreeMap<PathBuf, FileStamp>,
    control: Arc<Control>,
    coordinator: Arc<Coordinator>,
    pair_key: String,
    pending: BTreeSet<PathBuf>,
    blocked: BTreeMap<PathBuf, Option<FileStamp>>,
    push_ready: bool,
    last_event: Instant,
    studio_events: Receiver<StudioEvent>,
    studio_resume: SyncSender<()>,
    studio: StudioEventState,
    last_pull_attempt: Instant,
    last_rescan: Instant,
    push_retry_delay: Duration,
    rescan_pending: bool,
}

impl LiveLoop {
    fn new(worker: Worker) -> Result<Self> {
        let Worker {
            context,
            bridge,
            project,
            baseline,
            current,
            control,
            coordinator,
            pair_key,
        } = worker;
        let mut pending = BTreeSet::new();
        let mut blocked = BTreeMap::new();
        let push_ready = queue_scan_changes(&baseline, &current, &mut pending, &mut blocked);
        let last_event = if push_ready {
            Instant::now() - EVENT_DEBOUNCE
        } else {
            Instant::now()
        };
        let (studio_events, studio_resume) =
            start_studio_event_waiter(context.clone(), Arc::clone(&bridge), Arc::clone(&control))?;
        control.update_pending(&pending);
        Ok(Self {
            context,
            bridge,
            project,
            baseline,
            control,
            coordinator,
            pair_key,
            pending,
            blocked,
            push_ready,
            last_event,
            studio_events,
            studio_resume,
            studio: StudioEventState::default(),
            last_pull_attempt: Instant::now(),
            last_rescan: Instant::now() - RESCAN_RETRY,
            push_retry_delay: Duration::ZERO,
            rescan_pending: false,
        })
    }

    fn running(&self) -> bool {
        !self.control.stop.load(Ordering::Acquire) && self.bridge.alive.load(Ordering::Acquire)
    }

    fn receive_timeout(&self) -> Duration {
        if !self.push_ready {
            return Duration::from_millis(50);
        }
        EVENT_DEBOUNCE
            .max(self.push_retry_delay)
            .saturating_sub(self.last_event.elapsed())
            .min(Duration::from_millis(50))
    }

    fn process_events(&mut self) -> Result<bool> {
        poll_studio_event(
            &self.control,
            &self.studio_events,
            &self.studio_resume,
            &mut self.studio,
        )?;
        let receive_timeout = self.receive_timeout();
        let project_event = receive_project_event(
            &self.project,
            &self.baseline,
            &mut self.pending,
            &mut self.blocked,
            &self.control,
            receive_timeout,
        )?;
        if project_event.changed {
            self.last_event = Instant::now();
            self.push_retry_delay = Duration::ZERO;
            self.push_ready = true;
        }
        self.rescan_pending |= project_event.rescan || self.project.watcher.take_overflowed();
        if rescan_project_if_due(
            &self.project,
            &self.baseline,
            &mut self.pending,
            &mut self.blocked,
            &self.control,
            &mut self.last_rescan,
            &mut self.rescan_pending,
        ) {
            self.push_ready = true;
            self.last_event = Instant::now();
            self.push_retry_delay = Duration::ZERO;
        }
        if let Some(ready) = apply_rebase_request(
            &self.context,
            &mut self.project,
            &mut self.baseline,
            &mut self.pending,
            &mut self.blocked,
            &self.control,
        )? {
            self.push_ready = ready;
            self.push_retry_delay = Duration::ZERO;
        }
        let file_control = apply_file_control_changes(
            &self.project,
            &mut self.baseline,
            &mut self.pending,
            &mut self.blocked,
            &self.control,
        )?;
        if let Some(ready) = file_control.push_ready {
            self.push_ready = ready;
        }
        if file_control.push_immediately {
            self.last_event = Instant::now() - EVENT_DEBOUNCE;
            self.push_retry_delay = Duration::ZERO;
        }
        self.apply_retry_requests();
        Ok(file_control.retry_later)
    }

    fn apply_retry_requests(&mut self) {
        if self.control.retry.swap(false, Ordering::AcqRel) {
            self.blocked.clear();
            self.push_retry_delay = Duration::ZERO;
            self.studio.retry_delay = Duration::ZERO;
            self.push_ready = true;
            self.studio.pull_ready = true;
            if self.studio.wait_paused {
                self.studio.resume_at = None;
            }
        }
        if self.control.retry_pull.swap(false, Ordering::AcqRel) {
            self.studio.pull_ready = true;
        }
    }

    fn push_due(&self) -> bool {
        self.push_ready
            && self.control.writes_enabled.load(Ordering::Acquire)
            && !self.rescan_pending
            && self.control.file_pause_count.load(Ordering::Acquire) == 0
            && !self.pending.is_empty()
            && self
                .pending
                .iter()
                .any(|path| !self.blocked.contains_key(path))
            && self.last_event.elapsed() >= EVENT_DEBOUNCE.max(self.push_retry_delay)
    }

    fn push_guard(&mut self) -> Result<Option<StudioChangeGuard>> {
        match studio_push_state(&self.context, &self.bridge) {
            Ok(state) if state.pending => self.reconcile_concurrent_changes(),
            Ok(state) => Ok(Some(state.guard)),
            Err(error) if automation_failure_ref(&error).0.c == "no_studio" => Err(error),
            Err(error) => {
                self.control.fail(format!(
                    "Live sync could not inspect Studio changes: {error:#}"
                ));
                self.push_ready = false;
                self.rescan_pending = true;
                Ok(None)
            }
        }
    }

    fn reconcile_concurrent_changes(&mut self) -> Result<Option<StudioChangeGuard>> {
        log_global(
            5,
            format_args!("[renium] reconcile reason: concurrent editor and Studio changes"),
        );
        let generation = self.control.generation.load(Ordering::Acquire);
        let Some(_activity) = self.control.begin_sync(generation) else {
            return Ok(None);
        };
        match self
            .coordinator
            .reconcile_current(&self.context, &self.bridge)
        {
            Ok(setup) => {
                self.control.set_mode(setup.mode);
                self.control
                    .set_resolution_required(setup.resolution_required);
                if let Some(error) = setup.error {
                    self.control.fail(error);
                } else {
                    self.control.clear_error();
                }
                self.baseline = scan(&self.project)?;
                self.pending.clear();
                self.blocked.clear();
                self.control.update_pending(&self.pending);
                self.push_ready = false;
                self.studio.pending_payload = None;
                self.studio.pull_ready = true;
                self.last_event = Instant::now();
            }
            Err(error) if automation_failure_ref(&error).0.c == "no_studio" => return Err(error),
            Err(error) => {
                self.control.set_mode(PairMode::Verify);
                self.control.fail(format!(
                    "Live sync could not reconcile concurrent changes: {error:#}"
                ));
                self.push_ready = false;
                self.rescan_pending = true;
            }
        }
        Ok(None)
    }

    fn pending_push_paths(&self) -> Vec<PathBuf> {
        self.pending
            .iter()
            .filter(|path| !self.blocked.contains_key(*path))
            .cloned()
            .collect()
    }

    fn prepare_push(&mut self, guard: StudioChangeGuard) -> Result<PreparedPush> {
        let paths = self.pending_push_paths();
        let refresh_project = paths
            .iter()
            .any(|path| self.project.full_push.contains(path));
        let captured = if refresh_project {
            let project = open_watch_project(&self.context)?;
            let captured = scan(&project)?
                .into_iter()
                .map(|(path, stamp)| (path, Some(stamp)))
                .collect();
            self.project = project;
            captured
        } else {
            capture_pending(&self.project, &self.baseline, &paths)?
        };
        Ok(PreparedPush {
            paths,
            refresh_project,
            captured,
            guard,
        })
    }

    fn block_unreadable_push(&mut self, paths: &[PathBuf], error: &anyhow::Error) {
        self.control
            .fail(format!("Live sync could not read pending files: {error:#}"));
        for path in paths {
            self.blocked
                .insert(path.clone(), stamp(path).unwrap_or(None));
        }
        self.push_ready = false;
        self.rescan_pending = true;
    }

    fn execute_push(&self, push: &PreparedPush) -> Option<Result<LivePushResult>> {
        let generation = self.control.generation.load(Ordering::Acquire);
        let _activity = self.control.begin_sync(generation)?;
        let gate_started = Instant::now();
        let gate = self.bridge.acquire_request_gate();
        log_global(
            5,
            format_args!(
                "[renium] live push request gate: {:.1}ms",
                gate_started.elapsed().as_secs_f64() * 1000.0
            ),
        );
        if self.control.generation.load(Ordering::Acquire) != generation
            || self.control.file_pause_count.load(Ordering::Acquire) > 0
        {
            return None;
        }
        let push_started = Instant::now();
        let push_changes = || {
            if push.refresh_project {
                let validation_paths = push.captured.keys().cloned().collect::<Vec<_>>();
                self.coordinator
                    .validate_editor_changes(&self.context, &self.pair_key, &validation_paths)
                    .and_then(|()| push_full(&self.context, &self.bridge, Some(&push.guard)))
            } else {
                self.coordinator
                    .push_editor_changes(
                        &self.context,
                        &self.pair_key,
                        &self.bridge,
                        &push.paths,
                        Some(&push.guard),
                    )
                    .map(
                        |AppliedEditorChanges { accepted, summary }| LivePushResult {
                            accepted,
                            auto_desynced_packages: auto_desynced_packages(&summary),
                        },
                    )
            }
        };
        let result = match push_changes() {
            Err(error) if automation_failure_ref(&error).0.rt == 1 => {
                thread::sleep(Duration::from_millis(100));
                push_changes()
            }
            result => result,
        };
        drop(gate);
        log_global(
            5,
            format_args!(
                "[renium] live push execution: {:.1}ms",
                push_started.elapsed().as_secs_f64() * 1000.0
            ),
        );
        Some(result)
    }

    fn record_full_push(&mut self, push: &PreparedPush) -> Result<bool> {
        log_global(
            5,
            format_args!("[renium] reconcile reason: full project push"),
        );
        match self
            .coordinator
            .reconcile_current(&self.context, &self.bridge)
        {
            Ok(setup) => {
                self.control.set_mode(setup.mode);
                self.control
                    .set_resolution_required(setup.resolution_required);
                if let Some(error) = setup.error {
                    self.control.fail(error);
                    self.push_ready = false;
                    return Ok(false);
                }
            }
            Err(error) if automation_failure_ref(&error).0.c == "no_studio" => return Err(error),
            Err(error) => {
                self.control.set_mode(PairMode::Verify);
                self.control.fail(format!(
                    "Live sync applied files in Studio but could not record the shared state: {error:#}"
                ));
                self.push_ready = false;
                return Ok(false);
            }
        }
        let current = scan(&self.project)?;
        self.baseline = push
            .captured
            .iter()
            .filter_map(|(path, stamp)| stamp.map(|stamp| (path.clone(), stamp)))
            .collect();
        self.blocked.clear();
        self.pending.clear();
        queue_scan_changes(
            &self.baseline,
            &current,
            &mut self.pending,
            &mut self.blocked,
        );
        Ok(true)
    }

    fn record_incremental_push(
        &mut self,
        push: &PreparedPush,
        accepted: BTreeMap<PathBuf, Option<PublishEntryState>>,
    ) {
        for (path, captured_stamp) in &push.captured {
            if accepted.contains_key(path) {
                continue;
            }
            match stamp(path) {
                Ok(current) if *captured_stamp == current => {
                    record_stamp(
                        path,
                        current,
                        &mut self.baseline,
                        &mut self.pending,
                        &mut self.blocked,
                    );
                }
                Ok(_) => {
                    self.pending.insert(path.clone());
                    self.blocked.remove(path);
                }
                Err(error) => {
                    self.control.fail(format!(
                        "Live sync could not verify a pushed file: {error:#}"
                    ));
                    self.rescan_pending = true;
                }
            }
        }
        for (path, expected) in accepted {
            let current = stamp(&path);
            record_accepted_stamp(
                &self.project.root,
                &path,
                expected.as_ref(),
                current.as_ref().ok().and_then(|value| value.as_ref()),
                &mut self.baseline,
                &mut self.pending,
            );
            self.blocked.remove(&path);
            if let Err(error) = current {
                self.pending.insert(path);
                self.control.fail(format!(
                    "Live sync could not verify an accepted file: {error:#}"
                ));
                self.rescan_pending = true;
            }
        }
    }

    fn record_push_success(&mut self, push: &PreparedPush, result: LivePushResult) -> Result<bool> {
        let LivePushResult {
            accepted,
            auto_desynced_packages,
        } = result;
        if push.refresh_project {
            if !self.record_full_push(push)? {
                return Ok(true);
            }
        } else {
            self.record_incremental_push(push, accepted);
        }
        let mut status = self
            .control
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        record_successful_push(&mut status, auto_desynced_packages);
        if self.blocked.is_empty() {
            status.error = None;
        }
        drop(status);
        self.control.update_pending(&self.pending);
        self.push_ready = self
            .pending
            .iter()
            .any(|path| !self.blocked.contains_key(path));
        self.push_retry_delay = Duration::ZERO;
        self.last_event = Instant::now();
        Ok(false)
    }

    fn record_push_failure(&mut self, push: &PreparedPush, error: anyhow::Error) -> Result<bool> {
        if error.is::<StudioChangedBeforePush>() {
            self.push_ready = true;
            self.push_retry_delay = Duration::ZERO;
            self.last_event = Instant::now() - EVENT_DEBOUNCE;
            return Ok(true);
        }
        let failure = automation_failure_ref(&error);
        if failure.0.c == "no_studio" {
            return Err(error);
        }
        let retry = failure.0.rt == 1;
        self.control.fail(failure.0.m);
        self.push_ready = retry;
        if retry {
            self.push_retry_delay = next_retry_delay(self.push_retry_delay);
            self.last_event = Instant::now();
        } else {
            for path in &push.paths {
                self.blocked.insert(
                    path.clone(),
                    push.captured.get(path).copied().unwrap_or(None),
                );
            }
        }
        Ok(false)
    }

    fn maybe_push(&mut self) -> Result<bool> {
        if !self.push_due() {
            return Ok(false);
        }
        let Some(guard) = self.push_guard()? else {
            return Ok(true);
        };
        let paths = self.pending_push_paths();
        let push = match self.prepare_push(guard) {
            Ok(push) => push,
            Err(error) => {
                self.block_unreadable_push(&paths, &error);
                return Ok(true);
            }
        };
        let Some(result) = self.execute_push(&push) else {
            return Ok(true);
        };
        match result {
            Ok(generated_paths) => self.record_push_success(&push, generated_paths),
            Err(error) => self.record_push_failure(&push, error),
        }
    }

    fn pull_due(&self) -> bool {
        self.studio.pull_ready
            && self.control.writes_enabled.load(Ordering::Acquire)
            && self.control.pull_changes.load(Ordering::Acquire)
            && self.control.file_pause_count.load(Ordering::Acquire) == 0
            && self.pending.is_empty()
            && self.last_pull_attempt.elapsed() >= self.studio.retry_delay
    }

    fn pull_studio(&mut self) -> Result<Option<PulledStudioChanges>> {
        let payload = self.studio.pending_payload.take();
        match pull_studio_changes(&self.context, &self.bridge, &self.control, payload) {
            Err(error) if automation_failure_ref(&error).0.rt == 1 => {
                thread::sleep(Duration::from_millis(100));
                pull_studio_changes(&self.context, &self.bridge, &self.control, None)
            }
            result => result,
        }
    }

    fn finish_studio_wait(&mut self) {
        self.studio.pull_ready = false;
        self.studio.wait_paused = false;
        let _ = self.studio_resume.try_send(());
    }

    fn acknowledge_pull(&self, pulled: &PulledStudioChanges) -> Result<()> {
        if let Err(error) = self.coordinator.advance_baseline(
            &self.context,
            &self.pair_key,
            &pulled.published.changed_roots,
            BaselineSide::Studio,
        ) {
            self.control.set_mode(PairMode::Verify);
            self.control.fail(format!(
                "Live sync saved Studio changes but could not record the shared state: {error:#}"
            ));
            return Ok(());
        }
        match acknowledge_pulled_changes(
            &self.bridge,
            &pulled.services,
            pulled.seq,
            &pulled.runtime_id,
        ) {
            Ok(state) => {
                self.control.set_plugin_state(state);
                if let Err(error) = self.coordinator.record_studio_checkpoint(
                    &self.context,
                    &self.pair_key,
                    &self.control.plugin_snapshot(),
                ) {
                    log_global(
                        5,
                        format_args!("[renium] Studio checkpoint update failed: {error:#}"),
                    );
                }
                log_global(
                    5,
                    format_args!("[renium] live pull acknowledged seq {}", pulled.seq),
                );
                Ok(())
            }
            Err(error) if automation_failure_ref(&error).0.c == "no_studio" => Err(error),
            Err(error) => {
                self.control.fail(format!(
                    "Live sync saved Studio changes but could not acknowledge them: {error:#}"
                ));
                Ok(())
            }
        }
    }

    fn record_pull(&mut self, pulled: PulledStudioChanges) -> Result<()> {
        let current = scan(&self.project)?;
        log_global(
            5,
            format_args!(
                "[renium] live pull publish: roots={} expected={}",
                pulled.published.changed_roots.len(),
                pulled.published.expected.len()
            ),
        );
        reconcile_published_changes(
            &self.project,
            &mut self.baseline,
            &current,
            &mut self.pending,
            &pulled.published,
        );
        log_global(
            5,
            format_args!(
                "[renium] live pull pending after publish: {}",
                self.pending.len()
            ),
        );
        let mut status = self
            .control
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        status.pulls = status.pulls.saturating_add(1);
        status.error = None;
        drop(status);
        self.control.update_pending(&self.pending);
        self.push_ready = !self.pending.is_empty();
        self.studio.retry_delay = Duration::ZERO;
        self.acknowledge_pull(&pulled)?;
        self.finish_studio_wait();
        Ok(())
    }

    fn record_empty_pull(&mut self) {
        if !self.studio.retry_delay.is_zero() && self.blocked.is_empty() {
            self.control.clear_error();
        }
        self.studio.retry_delay = Duration::ZERO;
        self.finish_studio_wait();
    }

    fn record_pull_failure(&mut self, error: anyhow::Error) -> Result<()> {
        log_global(5, format_args!("[renium] live pull failed: {error:#}"));
        let failure = automation_failure_ref(&error);
        if failure.0.c == "no_studio" {
            return Err(error);
        }
        let retry = failure.0.rt == 1;
        self.control
            .fail(format!("Studio live sync failed: {}", failure.0.m));
        self.studio.pull_ready = retry;
        if retry {
            self.studio.retry_delay = next_retry_delay(self.studio.retry_delay);
        }
        Ok(())
    }

    fn maybe_pull(&mut self) -> Result<bool> {
        if !self.pull_due() {
            return Ok(false);
        }
        let current = scan(&self.project)?;
        if queue_scan_changes(
            &self.baseline,
            &current,
            &mut self.pending,
            &mut self.blocked,
        ) && !self.pending.is_empty()
        {
            self.control.update_pending(&self.pending);
            self.push_ready = true;
            self.last_event = Instant::now();
            return Ok(true);
        }
        self.last_pull_attempt = Instant::now();
        let generation = self.control.generation.load(Ordering::Acquire);
        let control = Arc::clone(&self.control);
        let Some(_activity) = control.begin_sync(generation) else {
            return Ok(true);
        };
        match self.pull_studio() {
            Ok(Some(pulled)) => self.record_pull(pulled)?,
            Ok(None) => self.record_empty_pull(),
            Err(error) => self.record_pull_failure(error)?,
        }
        Ok(false)
    }

    fn run(mut self) -> Result<()> {
        let mut reported_status = None;
        while self.running() {
            if self.push_ready || self.studio.pull_ready {
                crate::plugins::verify_place_lease(
                    self.context.place_id,
                    self.context.resource_lease.as_ref(),
                )?;
            }
            let status = self.control.plugin_live_status();
            if reported_status.as_ref() != Some(&status) {
                if let Some(runtime_id) = self.context.runtime_id.as_deref() {
                    report_plugin_live_status(&self.bridge, runtime_id, &status);
                }
                reported_status = Some(status);
            }
            if self.process_events()? {
                thread::sleep(RESCAN_RETRY);
                continue;
            }
            if self.maybe_push()? {
                continue;
            }
            self.maybe_pull()?;
        }
        Ok(())
    }
}

fn run(worker: Worker) -> Result<()> {
    LiveLoop::new(worker)?.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_control() -> Arc<Control> {
        Arc::new(Control::new(
            PathBuf::from("test-project"),
            true,
            false,
            PairMode::Reconcile,
            false,
        ))
    }

    #[test]
    fn plugin_status_reports_failure_recovery_and_stop_without_per_edit_updates() {
        let control = test_control();
        let healthy = control.plugin_live_status();
        control.status.lock().unwrap().pulls += 1;
        assert_eq!(control.plugin_live_status(), healthy);
        control.fail("Export failed".to_string());
        assert_eq!(
            control.plugin_live_status().error.as_deref(),
            Some("Export failed")
        );
        control.clear_error();
        assert_eq!(control.plugin_live_status(), healthy);
        control.finish();
        assert!(!control.plugin_live_status().running);
    }

    #[test]
    fn studio_pull_settles_only_on_the_same_runtime_revision() {
        let initial = json!({ "runtimeId": "runtime-a", "seq": 10 });
        let same = json!({ "runtimeId": "runtime-a", "seq": 10 });
        let advanced = json!({ "runtimeId": "runtime-a", "seq": 11 });
        let replaced = json!({ "runtimeId": "runtime-b", "seq": 10 });

        assert_eq!(
            studio_change_revision(&initial),
            studio_change_revision(&same)
        );
        assert_ne!(
            studio_change_revision(&initial),
            studio_change_revision(&advanced)
        );
        assert_ne!(
            studio_change_revision(&initial),
            studio_change_revision(&replaced)
        );
    }

    #[test]
    fn persisted_live_sync_only_matches_its_studio_target() {
        let marker = serde_json::to_vec(&EnabledMarker {
            version: ENABLED_MARKER_VERSION,
            target: "local-file:e:/downloads/TestPlace.rbxl".to_string(),
        })
        .unwrap();

        assert!(enabled_marker_matches(
            &marker,
            "local-file:e:/downloads/TestPlace.rbxl"
        ));
        assert!(!enabled_marker_matches(&marker, "published:123:456"));
        assert!(!enabled_marker_matches(
            b"1",
            "local-file:e:/downloads/TestPlace.rbxl"
        ));
    }

    #[test]
    fn persisted_live_sync_restores_its_exact_studio_target() {
        let published = serde_json::to_vec(&EnabledMarker {
            version: ENABLED_MARKER_VERSION,
            target: "published:123:456".to_string(),
        })
        .unwrap();
        assert_eq!(
            saved_studio_target(&published),
            Some(StudioReopenTarget {
                file: None,
                game_id: Some(123),
                place_id: Some(456),
            })
        );

        let local = serde_json::to_vec(&EnabledMarker {
            version: ENABLED_MARKER_VERSION,
            target: "local-file:e:/downloads/TestPlace.rbxl".to_string(),
        })
        .unwrap();
        assert_eq!(
            saved_studio_target(&local),
            Some(StudioReopenTarget {
                file: Some(PathBuf::from("e:/downloads/TestPlace.rbxl")),
                game_id: None,
                place_id: None,
            })
        );
        assert_eq!(saved_studio_target(b"{}"), None);
    }

    #[test]
    fn live_status_dates_the_last_auto_desync_event() {
        let mut status = Status::default();
        record_successful_push(&mut status, vec!["ReplicatedStorage.Package".to_string()]);
        record_successful_push(&mut status, Vec::new());

        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["pushes"], 2);
        assert_eq!(value["autoDesyncedAtPush"], 1);
        assert_eq!(
            value["autoDesyncedPackages"],
            json!(["ReplicatedStorage.Package"])
        );
    }

    #[test]
    fn acknowledged_intermediate_state_does_not_erase_a_later_revert() {
        let root = PathBuf::from("test-project");
        let path = root.join("src/changed.luau");
        let old = FileStamp {
            directory: false,
            length: 8,
            hash: fnv1a(b"return 1"),
        };
        let sent = FileStamp {
            directory: false,
            length: 8,
            hash: fnv1a(b"return 2"),
        };
        for (before, accepted) in [
            (Some(old), Some(sent)),
            (None, Some(sent)),
            (Some(old), None),
        ] {
            let mut baseline = before
                .map(|value| (path.clone(), value))
                .into_iter()
                .collect();
            let current = before
                .map(|value| (path.clone(), value))
                .into_iter()
                .collect();
            let mut pending = BTreeSet::from([path.clone()]);
            let expected = accepted.map(|value| PublishEntryState::File {
                sha256: String::new(),
                length: value.length,
                hash: value.hash,
            });
            record_accepted_stamp(
                &root,
                &path,
                expected.as_ref(),
                before.as_ref(),
                &mut baseline,
                &mut pending,
            );
            assert!(baseline.get(&path).copied() == accepted);
            queue_scan_changes(&baseline, &current, &mut pending, &mut BTreeMap::new());
            assert_eq!(pending, BTreeSet::from([path.clone()]));
            record_accepted_stamp(
                &root,
                &path,
                expected.as_ref(),
                accepted.as_ref(),
                &mut baseline,
                &mut pending,
            );
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn settled_wait_observes_an_event_quiet_period() {
        let control = test_control();
        control.set_plugin_state(json!({ "pendingChanges": 1 }));
        let (notified, notification) = mpsc::sync_channel(1);
        let notifier = Arc::clone(&control);
        let event = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let notified_at = Instant::now();
            notifier.set_plugin_state(json!({}));
            notified
                .send(notified_at)
                .expect("waiter should receive the notification time");
        });

        assert!(control.wait_settled(Duration::from_secs(1)));
        let settled_at = Instant::now();
        let notified_at = notification
            .recv()
            .expect("event thread should report its notification time");
        assert!(settled_at.duration_since(notified_at) >= SETTLE_QUIET_PERIOD);
        event.join().expect("event thread should finish");
    }

    #[test]
    fn settled_wait_tracks_pending_work_without_a_project_scan() {
        let control = test_control();
        control.queue([PathBuf::from("test-project/src/changed.luau")]);
        let finisher = Arc::clone(&control);
        let event = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            finisher.update_pending(&BTreeSet::new());
        });

        assert!(control.wait_settled(Duration::from_secs(1)));
        event.join().expect("event thread should finish");
    }

    #[test]
    fn editor_save_notification_queues_only_a_real_file_change() {
        let root = crate::tests::support::temp_dir("live-save-notification");
        let source_root = root.join("src");
        fs::create_dir_all(&source_root).unwrap();
        let source = source_root.join("Module.luau");
        fs::write(&source, "return 1\n").unwrap();

        let project = WatchProject {
            watcher: FileWatcher::new(8).unwrap(),
            root: root.clone(),
            roots: BTreeSet::from([source_root]),
            files: BTreeSet::new(),
            full_push: BTreeSet::new(),
        };
        let mut baseline = BTreeMap::from([(source.clone(), stamp(&source).unwrap().unwrap())]);
        let mut pending = BTreeSet::new();
        let mut blocked = BTreeMap::new();
        let control = Control::new(root.clone(), true, false, PairMode::Reconcile, false);

        control.notify_files([source.clone()]);
        let unchanged = apply_file_control_changes(
            &project,
            &mut baseline,
            &mut pending,
            &mut blocked,
            &control,
        )
        .unwrap();
        assert!(unchanged.push_ready.is_none());
        assert!(pending.is_empty());

        fs::write(&source, "return 2\n").unwrap();
        control.notify_files([source.clone()]);
        let changed = apply_file_control_changes(
            &project,
            &mut baseline,
            &mut pending,
            &mut blocked,
            &control,
        )
        .unwrap();
        assert_eq!(changed.push_ready, Some(true));
        assert!(changed.push_immediately);
        assert_eq!(pending, BTreeSet::from([source]));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settled_wait_returns_immediately_when_progress_is_blocked() {
        let control = test_control();
        control.fail("blocked".to_string());

        let started = Instant::now();
        assert!(!control.wait_settled(Duration::from_secs(1)));
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
