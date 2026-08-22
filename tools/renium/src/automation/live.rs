use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

use super::BoundContext;
use super::context as bound_context;
use super::runtime::{
    acknowledge_pulled_changes, automation_failure_ref, automation_pull_args, automation_push_args,
};
use crate::app::output::ensure_plugin_api_ok;
use crate::editor::sync::push_editor_changes_with_warm_bridge;
use crate::project::config;
use crate::snapshot::export::{
    PublishEntryState, PublishedProjectChanges, export_snapshots_with_warm_bridge,
};
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::files::{atomic_write_file, fnv1a};
use crate::system::watch::FileWatcher;

const EVENT_DEBOUNCE: Duration = Duration::from_millis(10);
const RESCAN_RETRY: Duration = Duration::from_millis(500);
const MAX_PUSH_RETRY_DELAY: Duration = Duration::from_secs(5);
const STUDIO_POLL_INTERVAL: Duration = Duration::from_millis(500);
const STATE_VERSION: u8 = 1;
const STATE_FILE: &str = "live-watch-state.rmp.zst";
const STATE_JOURNAL_FILE: &str = "live-watch-state.journal";
const STATE_JOURNAL_COMPACT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    directory: bool,
    length: u64,
    hash: u64,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    running: bool,
    pull_changes: bool,
    paused: bool,
    pending_paths: Vec<String>,
    pushes: u64,
    pulls: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct Control {
    stop: AtomicBool,
    retry: AtomicBool,
    retry_pull: AtomicBool,
    reset: AtomicBool,
    pull_changes: AtomicBool,
    file_pause_count: AtomicU64,
    generation: AtomicU64,
    file_changes: Mutex<FileChanges>,
    sync_active: Mutex<bool>,
    sync_idle: Condvar,
    reset_serial: Mutex<()>,
    reset_state: Mutex<ResetState>,
    reset_event: Condvar,
    root: PathBuf,
    status: Mutex<Status>,
    finished: Mutex<bool>,
    finished_event: Condvar,
}

#[derive(Default)]
struct FileChanges {
    queued: BTreeSet<PathBuf>,
    settled: BTreeSet<PathBuf>,
}

#[derive(Default)]
struct ResetState {
    requested: u64,
    completed: u64,
    error: Option<String>,
    rebase: Option<Rebase>,
}

enum Rebase {
    Captured(CapturedState),
    Published(PublishedProjectChanges),
}

pub(crate) struct CapturedState {
    full: bool,
    scopes: BTreeSet<PathBuf>,
    entries: BTreeMap<PathBuf, Option<FileStamp>>,
}

impl Control {
    fn new(root: PathBuf, pull_changes: bool, files_paused: bool) -> Self {
        Self {
            stop: AtomicBool::new(false),
            retry: AtomicBool::new(false),
            retry_pull: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            pull_changes: AtomicBool::new(pull_changes),
            file_pause_count: AtomicU64::new(u64::from(files_paused)),
            generation: AtomicU64::new(0),
            file_changes: Mutex::new(FileChanges::default()),
            sync_active: Mutex::new(false),
            sync_idle: Condvar::new(),
            reset_serial: Mutex::new(()),
            reset_state: Mutex::new(ResetState::default()),
            reset_event: Condvar::new(),
            root,
            status: Mutex::new(Status {
                running: true,
                pull_changes,
                paused: files_paused,
                ..Status::default()
            }),
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
    }

    fn rebase_then_resume(&self, rebase: Rebase) -> Result<()> {
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
            state.rebase = Some(rebase);
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

    fn take_rebase(&self) -> Option<(u64, Rebase)> {
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
    }

    fn pause(&self) {
        self.file_pause_count.fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .paused = true;
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
        Some(SyncActivity { control: self })
    }

    fn snapshot(&self) -> Value {
        json!(
            self.status
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        )
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
    }

    fn fail(&self, error: String) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .error = Some(error);
    }

    fn clear_error(&self) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .error = None;
    }

    fn finish(&self) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running = false;
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
        self.control.sync_idle.notify_all();
    }
}

#[derive(Default)]
pub(crate) struct Manager {
    sessions: Mutex<HashMap<u64, Arc<Control>>>,
}

impl Manager {
    fn control(&self, context_id: u64) -> Option<Arc<Control>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&context_id)
            .cloned()
    }

    pub(crate) fn start(
        &self,
        context: BoundContext,
        bridge: Arc<BridgeServer>,
        pull_changes: Option<bool>,
        files_paused: bool,
        reset_files_paused: bool,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(control) = sessions.get(&context.id).cloned() {
            let running = control
                .status
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .running;
            if running && !control.stop.load(Ordering::Acquire) {
                if let Some(pull_changes) = pull_changes {
                    control.set_pull_changes(pull_changes);
                }
                if reset_files_paused {
                    control.replace_pause(files_paused);
                } else if files_paused && control.file_pause_count.load(Ordering::Acquire) == 0 {
                    control.pause();
                }
                return Ok(control.snapshot());
            }
            drop(sessions);
            control.wait_finished();
            sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            if sessions
                .get(&context.id)
                .is_some_and(|current| Arc::ptr_eq(current, &control))
            {
                sessions.remove(&context.id);
            }
        }
        let project = open_watch_project(&context)?;
        let current = scan(&project)?;
        let (mut baseline, state_error, state_missing) = match load_state(&project) {
            Ok(Some(baseline)) => (baseline, None, false),
            Ok(None) => (current.clone(), None, true),
            Err(error) => (current.clone(), Some(error), true),
        };
        let saved_entries = baseline.len();
        baseline.retain(|path, _| relevant(&project, path));
        let state_pruned = baseline.len() != saved_entries;
        let control = Arc::new(Control::new(
            PathBuf::from(&context.root),
            pull_changes.unwrap_or(true),
            files_paused,
        ));
        if let Some(error) = state_error {
            control.fail(format!("Live sync ignored invalid saved state: {error:#}"));
        }
        if (state_missing || state_pruned)
            && let Err(error) = save_state(&project, &baseline)
        {
            control.fail(format!(
                "Live sync could not save its initial state: {error:#}"
            ));
        }
        let context_id = context.id;
        let worker_control = Arc::clone(&control);
        thread::Builder::new()
            .name(format!("renium-live-{context_id}"))
            .spawn(move || {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(context, bridge, project, baseline, current, &worker_control)
                })) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => worker_control.fail(format!("{error:#}")),
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|message| (*message).to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        worker_control.fail(format!("Live sync watcher panicked: {message}"));
                    }
                }
                worker_control.finish();
            })
            .context("Failed to start live sync watcher")?;
        sessions.insert(context_id, Arc::clone(&control));
        Ok(control.snapshot())
    }

    pub(crate) fn status(&self, context_id: u64) -> Value {
        self.control(context_id)
            .as_deref()
            .map_or_else(|| json!({ "running": false }), |control| control.snapshot())
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

    pub(crate) fn reconcile_then_resume(
        &self,
        context_id: u64,
        published: PublishedProjectChanges,
    ) -> Result<Value> {
        if let Some(control) = self.control(context_id) {
            control.rebase_then_resume(Rebase::Published(published))?;
        }
        Ok(self.status(context_id))
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
        let control = self.control(context_id);
        if let Some(control) = control {
            control.stop.store(true, Ordering::Release);
            control.wait_finished();
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            if sessions
                .get(&context_id)
                .is_some_and(|current| Arc::ptr_eq(current, &control))
            {
                sessions.remove(&context_id);
            }
            control.snapshot()
        } else {
            json!({ "running": false })
        }
    }

    pub(crate) fn cancel(&self, context_id: u64) {
        self.stop(context_id);
    }
}

struct WatchProject {
    watcher: FileWatcher,
    root: PathBuf,
    roots: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    full_push: BTreeSet<PathBuf>,
    project_path: PathBuf,
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
        project_path: loaded.path,
    })
}

#[derive(Serialize, Deserialize)]
struct PersistedState {
    version: u8,
    project: String,
    files: BTreeMap<String, FileStamp>,
}

#[derive(Serialize, Deserialize)]
struct PersistedChange {
    path: String,
    stamp: Option<FileStamp>,
}

fn state_path(project: &WatchProject) -> PathBuf {
    project.root.join(".renium").join(STATE_FILE)
}

fn state_journal_path(project: &WatchProject) -> PathBuf {
    project.root.join(".renium").join(STATE_JOURNAL_FILE)
}

fn save_state(project: &WatchProject, baseline: &BTreeMap<PathBuf, FileStamp>) -> Result<()> {
    let state = PersistedState {
        version: STATE_VERSION,
        project: project.project_path.to_string_lossy().into_owned(),
        files: baseline
            .iter()
            .map(|(path, stamp)| (path.to_string_lossy().into_owned(), *stamp))
            .collect(),
    };
    let encoded = rmp_serde::to_vec(&state).context("Failed to encode live sync state")?;
    let compressed = zstd::stream::encode_all(encoded.as_slice(), 1)
        .context("Failed to compress live sync state")?;
    atomic_write_file(&state_path(project), &compressed)?;
    match fs::remove_file(state_journal_path(project)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Failed to reset live sync state journal"),
    }
    Ok(())
}

fn append_state_changes(
    project: &WatchProject,
    baseline: &BTreeMap<PathBuf, FileStamp>,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    let paths = paths.into_iter().collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(());
    }
    let journal_path = state_journal_path(project);
    if let Some(parent) = journal_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut journal = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&journal_path)
        .with_context(|| format!("Failed to open {}", journal_path.display()))?;
    for path in paths {
        let encoded = rmp_serde::to_vec(&PersistedChange {
            path: path.to_string_lossy().into_owned(),
            stamp: baseline.get(&path).copied(),
        })
        .context("Failed to encode live sync state change")?;
        let length = u32::try_from(encoded.len()).context("Live sync state change is too large")?;
        journal
            .write_all(&length.to_le_bytes())
            .and_then(|()| journal.write_all(&encoded))
            .with_context(|| format!("Failed to write {}", journal_path.display()))?;
    }
    journal
        .flush()
        .with_context(|| format!("Failed to flush {}", journal_path.display()))?;
    if journal.metadata()?.len() >= STATE_JOURNAL_COMPACT_BYTES {
        drop(journal);
        save_state(project, baseline)?;
    }
    Ok(())
}

fn load_state(project: &WatchProject) -> Result<Option<BTreeMap<PathBuf, FileStamp>>> {
    let path = state_path(project);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    let decoded = zstd::stream::decode_all(bytes.as_slice())
        .with_context(|| format!("Failed to decompress {}", path.display()))?;
    let state: PersistedState = rmp_serde::from_slice(&decoded)
        .with_context(|| format!("Failed to decode {}", path.display()))?;
    if state.version != STATE_VERSION || state.project != project.project_path.to_string_lossy() {
        return Ok(None);
    }
    let mut baseline = state
        .files
        .into_iter()
        .map(|(path, stamp)| (PathBuf::from(path), stamp))
        .collect::<BTreeMap<_, _>>();
    let journal_path = state_journal_path(project);
    let journal = match fs::read(&journal_path) {
        Ok(journal) => journal,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(baseline)),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read {}", journal_path.display()));
        }
    };
    let mut offset = 0;
    while offset + 4 <= journal.len() {
        let record_start = offset;
        let length = u32::from_le_bytes([
            journal[offset],
            journal[offset + 1],
            journal[offset + 2],
            journal[offset + 3],
        ]) as usize;
        offset += 4;
        let Some(end) = offset
            .checked_add(length)
            .filter(|end| *end <= journal.len())
        else {
            offset = record_start;
            break;
        };
        let change: PersistedChange = rmp_serde::from_slice(&journal[offset..end])
            .with_context(|| format!("Failed to decode {}", journal_path.display()))?;
        let path = PathBuf::from(change.path);
        if let Some(stamp) = change.stamp {
            baseline.insert(path, stamp);
        } else {
            baseline.remove(&path);
        }
        offset = end;
    }
    if offset < journal.len() {
        OpenOptions::new()
            .write(true)
            .open(&journal_path)
            .and_then(|file| file.set_len(offset as u64))
            .with_context(|| format!("Failed to repair {}", journal_path.display()))?;
    }
    Ok(Some(baseline))
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
        relative.components().any(|component| {
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
        let bytes = match fs::read(path) {
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

fn push(context: &BoundContext, bridge: &BridgeServer, paths: Option<&[PathBuf]>) -> Result<()> {
    let mut parameters = json!({ "verifySources": true });
    if let Some(paths) = paths {
        parameters["changedPaths"] = json!(paths);
    }
    let _selection = bound_context::select(context);
    bridge.clear_runtime_pins();
    push_editor_changes_with_warm_bridge(
        automation_push_args(context, &parameters, false)?,
        bridge,
    )?;
    Ok(())
}

fn pull_studio_changes(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<Option<PublishedProjectChanges>> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Live sync context has no Studio runtime")?;
    let _gate = bridge.acquire_request_gate();
    let _selection = bound_context::select(context);
    bridge.clear_runtime_pins();
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": true }),
        BridgeTarget::Main,
        runtime_id,
        Some(Duration::from_secs(1)),
    )?;
    ensure_plugin_api_ok(&state)?;
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
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    let published = export_snapshots_with_warm_bridge(
        automation_pull_args(context, &parameters, true)?,
        bridge,
        &info,
        0.0,
        false,
    )?;
    acknowledge_pulled_changes(bridge, &services, seq, &state_runtime_id)?;
    Ok(Some(published))
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
        .collect::<BTreeSet<_>>();
    for path in candidates {
        let relative = path.strip_prefix(&project.root).unwrap_or(&path);
        if let Some(expected) = published.expected.get(relative)
            && published_state_matches(
                &project.root,
                relative,
                expected.as_ref(),
                current.get(&path),
            )
        {
            if let Some(stamp) = current.get(&path) {
                baseline.insert(path.clone(), *stamp);
            } else {
                baseline.remove(&path);
            }
            pending.remove(&path);
        } else if baseline.get(&path) != current.get(&path) {
            pending.insert(path);
        } else {
            pending.remove(&path);
        }
    }
}

fn run(
    context: BoundContext,
    bridge: Arc<BridgeServer>,
    mut project: WatchProject,
    mut baseline: BTreeMap<PathBuf, FileStamp>,
    current: BTreeMap<PathBuf, FileStamp>,
    control: &Control,
) -> Result<()> {
    let mut pending = BTreeSet::new();
    let mut blocked = BTreeMap::new();
    let mut push_ready = queue_scan_changes(&baseline, &current, &mut pending, &mut blocked);
    let mut last_event = if push_ready {
        Instant::now() - EVENT_DEBOUNCE
    } else {
        Instant::now()
    };
    let mut last_studio_poll = Instant::now();
    let mut last_rescan = Instant::now() - RESCAN_RETRY;
    let mut push_retry_delay = Duration::ZERO;
    let mut pull_retry_delay = Duration::ZERO;
    let mut pull_ready = true;
    let mut rescan_pending = false;
    control.update_pending(&pending);

    while !control.stop.load(Ordering::Acquire) && bridge.alive.load(Ordering::Acquire) {
        let receive_timeout = if push_ready {
            EVENT_DEBOUNCE
                .max(push_retry_delay)
                .saturating_sub(last_event.elapsed())
                .min(Duration::from_millis(50))
        } else {
            Duration::from_millis(50)
        };
        match project.watcher.receiver().recv_timeout(receive_timeout) {
            Ok(Ok(event)) => {
                match queue_changed(&project, &baseline, &mut pending, &mut blocked, event.paths) {
                    Ok(changed) => {
                        if changed {
                            unblock_full_push(&project, &mut blocked);
                            last_event = Instant::now();
                            push_retry_delay = Duration::ZERO;
                            push_ready = true;
                            control.update_pending(&pending);
                        }
                    }
                    Err(error) => {
                        control.fail(format!(
                            "Project watcher could not read a changed file: {error:#}"
                        ));
                        rescan_pending = true;
                    }
                }
            }
            Ok(Err(error)) => {
                control.fail(format!("Project watcher failed: {error}"));
                rescan_pending = true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("Project watcher stopped")
            }
        }

        if project.watcher.take_overflowed() {
            rescan_pending = true;
        }
        if rescan_pending && last_rescan.elapsed() >= RESCAN_RETRY {
            last_rescan = Instant::now();
            match scan(&project) {
                Ok(current) => {
                    if queue_scan_changes(&baseline, &current, &mut pending, &mut blocked) {
                        unblock_full_push(&project, &mut blocked);
                        push_ready = true;
                        last_event = Instant::now();
                        push_retry_delay = Duration::ZERO;
                        control.update_pending(&pending);
                    }
                    if blocked.is_empty() {
                        control.clear_error();
                    }
                    rescan_pending = false;
                }
                Err(error) => {
                    control.fail(format!("Project watcher rescan failed: {error:#}"));
                }
            }
        }
        if control.reset.swap(false, Ordering::AcqRel) {
            let (reset_sequence, rebase) = control
                .take_rebase()
                .context("Live sync rebase request was missing")?;
            match open_watch_project(&context)
                .and_then(|current| scan(&current).map(|snapshot| (current, snapshot)))
            {
                Ok((current, snapshot)) => {
                    project = current;
                    match rebase {
                        Rebase::Captured(captured) => {
                            if captured.full {
                                baseline = captured
                                    .entries
                                    .into_iter()
                                    .filter(|(path, _)| relevant(&project, path))
                                    .filter_map(|(path, stamp)| stamp.map(|stamp| (path, stamp)))
                                    .collect();
                            } else {
                                let candidates = baseline
                                    .keys()
                                    .chain(snapshot.keys())
                                    .filter(|path| {
                                        captured.scopes.iter().any(|scope| path.starts_with(scope))
                                    })
                                    .cloned()
                                    .collect::<BTreeSet<_>>();
                                for path in candidates {
                                    let expected =
                                        captured.entries.get(&path).copied().unwrap_or(None);
                                    if !relevant(&project, &path)
                                        || snapshot.get(&path).copied() != expected
                                    {
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
                            queue_scan_changes(&baseline, &snapshot, &mut pending, &mut blocked);
                        }
                        Rebase::Published(published) => {
                            blocked.clear();
                            reconcile_published_changes(
                                &project,
                                &mut baseline,
                                &snapshot,
                                &mut pending,
                                &published,
                            );
                        }
                    }
                    push_ready = !pending.is_empty();
                    push_retry_delay = Duration::ZERO;
                    control.update_pending(&pending);
                    let reset_error = save_state(&project, &baseline)
                        .err()
                        .map(|error| format!("Live sync could not save its state: {error:#}"));
                    if let Some(error) = &reset_error {
                        control.fail(error.clone());
                    } else {
                        control.clear_error();
                    }
                    control.release_pause();
                    control.complete_reset(reset_sequence, reset_error);
                }
                Err(error) => {
                    let message = format!("Live sync could not refresh the project: {error:#}");
                    control.fail(message.clone());
                    control.release_pause();
                    control.complete_reset(reset_sequence, Some(message));
                    return Err(error);
                }
            }
        }
        let (queued, settled) = {
            let mut changes = control
                .file_changes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (
                std::mem::take(&mut changes.queued),
                std::mem::take(&mut changes.settled),
            )
        };
        if !queued.is_empty() {
            let mut accepted = false;
            for path in queued {
                if relevant(&project, &path) {
                    blocked.remove(&path);
                    pending.insert(path);
                    accepted = true;
                }
            }
            if accepted {
                unblock_full_push(&project, &mut blocked);
                push_ready = true;
                last_event = Instant::now() - EVENT_DEBOUNCE;
                push_retry_delay = Duration::ZERO;
            }
            control.update_pending(&pending);
        }
        if !settled.is_empty() {
            let mut settle_failed = false;
            let mut persisted = Vec::new();
            for path in settled {
                if !relevant(&project, &path) {
                    continue;
                }
                match stamp(&path) {
                    Ok(Some(current)) => {
                        baseline.insert(path.clone(), current);
                        pending.remove(&path);
                        blocked.remove(&path);
                        persisted.push(path);
                    }
                    Ok(None) => {
                        baseline.remove(&path);
                        pending.remove(&path);
                        blocked.remove(&path);
                        persisted.push(path);
                    }
                    Err(error) => {
                        control
                            .file_changes
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .settled
                            .insert(path);
                        control.fail(format!(
                            "Live sync could not settle a written file: {error:#}"
                        ));
                        settle_failed = true;
                    }
                }
            }
            push_ready = pending.iter().any(|path| !blocked.contains_key(path));
            control.update_pending(&pending);
            if settle_failed {
                thread::sleep(RESCAN_RETRY);
                continue;
            }
            match append_state_changes(&project, &baseline, persisted) {
                Ok(()) if blocked.is_empty() => control.clear_error(),
                Ok(()) => {}
                Err(error) => {
                    control.fail(format!("Live sync could not save its state: {error:#}"));
                }
            }
        }
        if control.retry.swap(false, Ordering::AcqRel) {
            blocked.clear();
            push_retry_delay = Duration::ZERO;
            pull_retry_delay = Duration::ZERO;
            push_ready = true;
            pull_ready = true;
        }
        if control.retry_pull.swap(false, Ordering::AcqRel) {
            pull_ready = true;
        }

        if push_ready
            && !rescan_pending
            && control.file_pause_count.load(Ordering::Acquire) == 0
            && !pending.is_empty()
            && pending.iter().any(|path| !blocked.contains_key(path))
            && last_event.elapsed() >= EVENT_DEBOUNCE.max(push_retry_delay)
        {
            let paths = pending
                .iter()
                .filter(|path| !blocked.contains_key(*path))
                .cloned()
                .collect::<Vec<_>>();
            let refresh_project = paths.iter().any(|path| project.full_push.contains(path));
            let mut refreshed_project = None;
            let captured = match if refresh_project {
                open_watch_project(&context).and_then(|current| {
                    scan(&current).map(|captured| {
                        refreshed_project = Some(current);
                        captured
                            .into_iter()
                            .map(|(path, stamp)| (path, Some(stamp)))
                            .collect()
                    })
                })
            } else {
                capture_pending(&project, &baseline, &paths)
            } {
                Ok(captured) => captured,
                Err(error) => {
                    control.fail(format!("Live sync could not read pending files: {error:#}"));
                    for path in &paths {
                        blocked.insert(path.clone(), stamp(path).unwrap_or(None));
                    }
                    push_ready = false;
                    rescan_pending = true;
                    continue;
                }
            };
            if let Some(current) = refreshed_project.take() {
                project = current;
            }
            let generation = control.generation.load(Ordering::Acquire);
            let Some(_activity) = control.begin_sync(generation) else {
                continue;
            };
            let _gate = bridge.acquire_request_gate();
            if control.generation.load(Ordering::Acquire) != generation
                || control.file_pause_count.load(Ordering::Acquire) > 0
            {
                continue;
            }
            let selected_paths = (!refresh_project).then_some(paths.as_slice());
            let result = match push(&context, &bridge, selected_paths) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let failure = automation_failure_ref(&error);
                    if failure.0.rt == 1 {
                        thread::sleep(Duration::from_millis(100));
                        push(&context, &bridge, selected_paths)
                    } else {
                        Err(error)
                    }
                }
            };
            match result {
                Ok(()) => {
                    let mut persisted = Vec::new();
                    if refresh_project {
                        let current = scan(&project)?;
                        baseline = captured
                            .into_iter()
                            .filter_map(|(path, stamp)| stamp.map(|stamp| (path, stamp)))
                            .collect();
                        blocked.clear();
                        pending.clear();
                        queue_scan_changes(&baseline, &current, &mut pending, &mut blocked);
                    } else {
                        for (path, captured_stamp) in &captured {
                            match stamp(path) {
                                Ok(current) if *captured_stamp == current => {
                                    if let Some(current) = current {
                                        baseline.insert(path.clone(), current);
                                    } else {
                                        baseline.remove(path);
                                    }
                                    pending.remove(path);
                                    blocked.remove(path);
                                    persisted.push(path.clone());
                                }
                                Ok(_) => {
                                    pending.insert(path.clone());
                                    blocked.remove(path);
                                }
                                Err(error) => {
                                    control.fail(format!(
                                        "Live sync could not verify a pushed file: {error:#}"
                                    ));
                                    rescan_pending = true;
                                }
                            }
                        }
                    }
                    let mut status = control
                        .status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    status.pushes = status.pushes.saturating_add(1);
                    if blocked.is_empty() {
                        status.error = None;
                    }
                    drop(status);
                    if refresh_project {
                        if let Err(error) = save_state(&project, &baseline) {
                            control.fail(format!("Live sync could not save its state: {error:#}"));
                        }
                    } else if let Err(error) = append_state_changes(&project, &baseline, persisted)
                    {
                        control.fail(format!("Live sync could not save its state: {error:#}"));
                    }
                    control.update_pending(&pending);
                    push_ready = pending.iter().any(|path| !blocked.contains_key(path));
                    push_retry_delay = Duration::ZERO;
                    last_event = Instant::now();
                }
                Err(error) => {
                    let failure = automation_failure_ref(&error);
                    if failure.0.c == "no_studio" {
                        return Err(error);
                    }
                    let retry = failure.0.rt == 1;
                    control.fail(failure.0.m);
                    push_ready = retry;
                    if push_ready {
                        push_retry_delay = if push_retry_delay.is_zero() {
                            Duration::from_millis(250)
                        } else {
                            (push_retry_delay * 2).min(MAX_PUSH_RETRY_DELAY)
                        };
                        last_event = Instant::now();
                    } else {
                        for path in &paths {
                            blocked
                                .insert(path.clone(), captured.get(path).copied().unwrap_or(None));
                        }
                    }
                }
            }
        }

        if pull_ready
            && control.pull_changes.load(Ordering::Acquire)
            && control.file_pause_count.load(Ordering::Acquire) == 0
            && pending.is_empty()
            && last_studio_poll.elapsed() >= STUDIO_POLL_INTERVAL.max(pull_retry_delay)
        {
            last_studio_poll = Instant::now();
            let generation = control.generation.load(Ordering::Acquire);
            let Some(_activity) = control.begin_sync(generation) else {
                continue;
            };
            let result = match pull_studio_changes(&context, &bridge) {
                Err(error) => {
                    if automation_failure_ref(&error).0.rt == 1 {
                        thread::sleep(Duration::from_millis(100));
                        pull_studio_changes(&context, &bridge)
                    } else {
                        Err(error)
                    }
                }
                result => result,
            };
            match result {
                Ok(Some(published)) => {
                    let current = scan(&project)?;
                    reconcile_published_changes(
                        &project,
                        &mut baseline,
                        &current,
                        &mut pending,
                        &published,
                    );
                    let mut status = control
                        .status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    status.pulls = status.pulls.saturating_add(1);
                    status.error = None;
                    drop(status);
                    control.update_pending(&pending);
                    push_ready = !pending.is_empty();
                    pull_retry_delay = Duration::ZERO;
                    if let Err(error) = save_state(&project, &baseline) {
                        control.fail(format!("Live sync could not save its state: {error:#}"));
                    }
                }
                Ok(None) => {
                    if !pull_retry_delay.is_zero() && blocked.is_empty() {
                        control.clear_error();
                    }
                    pull_retry_delay = Duration::ZERO;
                }
                Err(error) => {
                    let failure = automation_failure_ref(&error);
                    if failure.0.c == "no_studio" {
                        return Err(error);
                    }
                    let retry = failure.0.rt == 1;
                    control.fail(format!("Studio live sync failed: {}", failure.0.m));
                    pull_ready = retry;
                    if retry {
                        pull_retry_delay = if pull_retry_delay.is_zero() {
                            Duration::from_millis(250)
                        } else {
                            (pull_retry_delay * 2).min(MAX_PUSH_RETRY_DELAY)
                        };
                    }
                }
            }
        }
    }
    Ok(())
}
