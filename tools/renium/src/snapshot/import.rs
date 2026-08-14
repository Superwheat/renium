use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde_json::{Map, Value, json};

use crate::app::build::{
    GIT_HASH as BUILD_GIT_HASH, TIMESTAMP_UNIX as BUILD_TIMESTAMP_UNIX, VERSION as BUILD_VERSION,
};
use crate::app::output::emit_global_output;
use crate::app::timing::{
    current_millis, elapsed_ms, log_timing, log_timing_ms, verbose_timing_logs,
};
use crate::bytecode::acquire_settings_file_lock;
use crate::cli::args::{ImportServiceArgs, ImportSnapshotsArgs};
use crate::editor::paths::{project_script_file_names, script_file_names};
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::project::sourcemap::{
    SourcemapNode, build_service_sourcemap_from_state, finalize_project_sourcemap_temp,
    load_existing_sourcemap_root, path_to_sourcemap_relative,
    write_project_sourcemap_from_service_nodes, write_project_sourcemap_with_updates,
};
use crate::rbx::encode::settings_root_indices;
use crate::roblox::schema::{MATERIAL_SERVICE_CLASS, USE_2022_MATERIALS_PROPERTY};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::bytecode::{
    SettingsBytecode, child_indices_for_instance, write_fresh_service_settings_binary_file,
    write_service_settings_binary_file,
};
use crate::settings::tree::editor_service_root_index;
use crate::snapshot::codec::parse_source_range_batch;
use crate::snapshot::export::{
    BRIDGE_PROTOCOL_VERSION, ExportProjectStage, LARGE_SERVICE_DETERMINISTIC_FETCH_MIN_INSTANCES,
    adaptive_tune_estimated_total_ms, collect_publish_hashes, exported_parts_to_service_state,
    fetch_json_payload, log_chunk_fetch_metrics, merge_chunk_fetch_metrics,
    publish_operation_paths,
};
use crate::snapshot::types::{
    AdaptiveTuneCache, AdaptiveTuneEntry, ExportedSnapshotParts, ServiceState, SnapshotInstance,
    SnapshotManifest,
};
use crate::studio::bridge::{BridgeServer, ChunkFetchMetrics, SourceBatchMap};
use crate::system::files::{
    OnDrop, canonical_path, path_key, read_json_file, resolve_existing_project_root, sanitize_name,
    service_settings_path, unique_child_stem, write_bytes_if_changed_in_existing_dir,
};

enum DirectImportTask {
    Service {
        service: String,
        parts: Box<ExportedSnapshotParts>,
    },
    Subtree(DirectImportSubtreeTask),
}

#[derive(Default)]
struct DirectImportPhase {
    core_complete: AtomicBool,
    state: Mutex<()>,
    ready: Condvar,
}

impl DirectImportPhase {
    fn is_core_complete(&self) -> bool {
        self.core_complete.load(Ordering::Acquire)
    }

    fn wait_for_core(&self) {
        if self.is_core_complete() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.is_core_complete() {
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn complete_core(&self) {
        if !self.core_complete.swap(true, Ordering::AcqRel) {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            drop(state);
            self.ready.notify_all();
        }
    }
}

#[derive(Default)]
struct DirectImportTaskQueueState {
    services: VecDeque<DirectImportTask>,
    subtrees: VecDeque<DirectImportTask>,
    prefer_subtree: bool,
    early_service_started: bool,
    active_workers: usize,
    closed: bool,
}

struct DirectImportTaskQueue {
    state: Mutex<DirectImportTaskQueueState>,
    ready: Condvar,
    worker_gate: Condvar,
    phase: Arc<DirectImportPhase>,
}

impl DirectImportTaskQueue {
    fn new(active_workers: usize, phase: Arc<DirectImportPhase>) -> Self {
        Self {
            state: Mutex::new(DirectImportTaskQueueState {
                active_workers,
                ..Default::default()
            }),
            ready: Condvar::new(),
            worker_gate: Condvar::new(),
            phase,
        }
    }

    fn enqueue_service(&self, task: DirectImportTask) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return false;
        }
        state.services.push_back(task);
        drop(state);
        self.ready.notify_one();
        true
    }

    fn enqueue_subtree(&self, task: DirectImportTask) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return false;
        }
        state.subtrees.push_back(task);
        drop(state);
        self.ready.notify_one();
        true
    }

    fn receive(&self, worker_index: usize) -> Option<DirectImportTask> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if worker_index >= state.active_workers && !state.closed {
                state = self
                    .worker_gate
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if !self.phase.is_core_complete() && worker_index == 0 {
                if !state.early_service_started {
                    if let Some(task) = state.services.pop_front() {
                        state.early_service_started = true;
                        return Some(task);
                    }
                } else if let Some(task) = state.subtrees.pop_front() {
                    return Some(task);
                }
                if state.closed {
                    return None;
                }
                state = self
                    .ready
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if !state.services.is_empty() && !state.subtrees.is_empty() {
                state.prefer_subtree = !state.prefer_subtree;
                let task = if state.prefer_subtree {
                    state.subtrees.pop_front()
                } else {
                    state.services.pop_front()
                };
                return task;
            }
            if let Some(task) = state.services.pop_front() {
                return Some(task);
            }
            if let Some(task) = state.subtrees.pop_front() {
                return Some(task);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn activate_workers(&self, active_workers: usize) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.active_workers = state.active_workers.max(active_workers);
        drop(state);
        self.worker_gate.notify_all();
        self.ready.notify_all();
    }

    fn complete_core_phase(&self) {
        self.phase.complete_core();
        self.worker_gate.notify_all();
        self.ready.notify_all();
    }

    fn close(&self) {
        self.complete_core_phase();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.worker_gate.notify_all();
        self.ready.notify_all();
    }
}

struct DirectImportSubtreeTask {
    shared: Arc<SplitDirectImportState>,
    items: Vec<DirectImportSubtreeItem>,
}

struct DirectImportSubtreeItem {
    index: usize,
    parent_dir: PathBuf,
    fs_stem: String,
    output_slot: usize,
    parent_assembly: Arc<SplitNodeAssembly>,
}

struct SplitNodeAssembly {
    name: String,
    class_name: String,
    file_paths: Vec<String>,
    child_slots: Vec<usize>,
    remaining_children: AtomicUsize,
    output_slot: Option<usize>,
    parent: Option<Arc<SplitNodeAssembly>>,
}

struct SplitDirectImportState {
    service: String,
    service_dir: PathBuf,
    final_service_dir: PathBuf,
    fresh_stage: bool,
    cleanup_required: bool,
    project_root: PathBuf,
    state: Arc<ServiceState>,
    expected_paths: Arc<ImportPathSets>,
    settings_write: Mutex<Option<thread::JoinHandle<Result<()>>>>,
    visited: Vec<AtomicBool>,
    slots: Mutex<Vec<Option<SourcemapNode>>>,
    queued_tasks: AtomicUsize,
    completed_tasks: AtomicUsize,
    total_task_tenths_ms: AtomicU64,
    max_task_tenths_ms: AtomicU64,
    failed: AtomicBool,
    started: Instant,
}

enum SplitImportDecision {
    Queued(Arc<SplitDirectImportState>),
    Inline(ServiceState),
}

struct SplitNodeShell {
    name: String,
    class_name: String,
    file_paths: Vec<String>,
    dir_path: PathBuf,
}

pub(crate) enum SourcemapWriterMessage {
    Service(String, SourcemapNode),
    Finish,
}

pub(crate) struct SourcemapWriter {
    sender: mpsc::Sender<SourcemapWriterMessage>,
    handle: Option<thread::JoinHandle<Result<()>>>,
}

impl SourcemapWriter {
    pub(crate) fn start(project_root: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel::<SourcemapWriterMessage>();
        let handle = thread::spawn(move || -> Result<()> {
            let mut service_nodes = load_existing_sourcemap_root(&project_root)?
                .map(|root| {
                    root.children
                        .into_iter()
                        .map(|node| (node.name.clone(), node))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            let mut wrote_update = false;
            while let Ok(message) = receiver.recv() {
                let mut pending_finish = false;
                match message {
                    SourcemapWriterMessage::Service(service, node) => {
                        wrote_update = true;
                        service_nodes.insert(service, node);
                    }
                    SourcemapWriterMessage::Finish => pending_finish = true,
                }

                while let Ok(pending) = receiver.try_recv() {
                    match pending {
                        SourcemapWriterMessage::Service(service, node) => {
                            wrote_update = true;
                            service_nodes.insert(service, node);
                        }
                        SourcemapWriterMessage::Finish => {
                            pending_finish = true;
                            break;
                        }
                    }
                }

                if pending_finish {
                    if wrote_update {
                        finalize_project_sourcemap_temp(&project_root, &service_nodes)?;
                    }
                    return Ok(());
                }
            }
            if wrote_update {
                finalize_project_sourcemap_temp(&project_root, &service_nodes)?;
            }
            Ok(())
        });

        Self {
            sender,
            handle: Some(handle),
        }
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<SourcemapWriterMessage> {
        self.sender.clone()
    }

    pub(crate) fn request_finish(&self) {
        let _ = self.sender.send(SourcemapWriterMessage::Finish);
    }

    pub(crate) fn join(mut self) -> Result<()> {
        if let Some(handle) = self.handle.take() {
            match handle.join() {
                Ok(result) => result,
                Err(_) => bail!("Sourcemap writer panicked"),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for SourcemapWriter {
    fn drop(&mut self) {
        let _ = self.sender.send(SourcemapWriterMessage::Finish);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) struct DirectImportDispatcher {
    queue: Option<Arc<DirectImportTaskQueue>>,
    workers: Vec<thread::JoinHandle<()>>,
    first_error: Arc<Mutex<Option<String>>>,
    service_nodes: Arc<Mutex<HashMap<String, SourcemapNode>>>,
    pending_tasks: Arc<AtomicUsize>,
    pending_signal: Arc<(Mutex<()>, Condvar)>,
}

#[derive(Clone)]
struct DirectImportWorker {
    queue: Arc<DirectImportTaskQueue>,
    project_root: PathBuf,
    src_dir: PathBuf,
    first_error: Arc<Mutex<Option<String>>>,
    service_nodes: Arc<Mutex<HashMap<String, SourcemapNode>>>,
    pending_tasks: Arc<AtomicUsize>,
    pending_signal: Arc<(Mutex<()>, Condvar)>,
    sourcemap_sender: Option<mpsc::Sender<SourcemapWriterMessage>>,
    run_started: Instant,
    early_pool: Arc<rayon::ThreadPool>,
}

impl DirectImportWorker {
    fn record_error(&self, error: String) {
        let mut slot = self
            .first_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(error);
        }
    }

    fn log_service_span(&self, service: &str, started_ms: f64, started: Instant, failed: bool) {
        log_timing(&format!("{service}: direct import worker total"), started);
        if failed || verbose_timing_logs() {
            println!(
                "[renium] service import span: service={}, start_ms={:.1}, end_ms={:.1}, duration_ms={:.1}",
                service,
                started_ms,
                elapsed_ms(self.run_started),
                elapsed_ms(started)
            );
        }
    }

    fn import_service(&self, service: String, parts: ExportedSnapshotParts) -> Result<()> {
        let src_root = self.project_root.join(&self.src_dir);
        let started = Instant::now();
        let started_ms = elapsed_ms(self.run_started);
        if verbose_timing_logs() {
            println!("[renium] {service}: direct import worker start");
        }
        let result = fs::create_dir_all(&src_root)
            .with_context(|| format!("Failed to create {}", src_root.display()))
            .and_then(|_| {
                let build_started = Instant::now();
                let state = exported_parts_to_service_state(&service, parts)?;
                log_timing(&format!("{service}: build service state"), build_started);
                Ok(state)
            })
            .and_then(|state| {
                match maybe_enqueue_split_import_tasks(
                    self.queue.as_ref(),
                    &self.pending_tasks,
                    &self.project_root,
                    &src_root,
                    &service,
                    state,
                )? {
                    SplitImportDecision::Queued(shared) => {
                        if verbose_timing_logs() {
                            println!(
                                "[renium] {}: queued {} subtree import tasks",
                                service,
                                shared.queued_tasks.load(Ordering::Acquire)
                            );
                        }
                        Ok(None)
                    }
                    SplitImportDecision::Inline(state) => {
                        self.queue.phase.wait_for_core();
                        let import_started = Instant::now();
                        let node = import_service_state_with_sourcemap(
                            &state,
                            &self.project_root,
                            &src_root,
                            &service,
                        )?;
                        log_timing(
                            &format!("{service}: import + sourcemap build"),
                            import_started,
                        );
                        Ok(Some(node))
                    }
                }
            });
        match result {
            Ok(Some(node)) => {
                if let Some(sender) = &self.sourcemap_sender {
                    let _ = sender.send(SourcemapWriterMessage::Service(
                        service.clone(),
                        node.clone(),
                    ));
                }
                self.service_nodes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(service.clone(), node);
            }
            Ok(None) => {}
            Err(error) => {
                self.log_service_span(&service, started_ms, started, true);
                return Err(error).with_context(|| service);
            }
        }
        self.log_service_span(&service, started_ms, started, false);
        Ok(())
    }

    fn import_subtree(&self, task: DirectImportSubtreeTask) -> Result<()> {
        let started = Instant::now();
        let shared = Arc::clone(&task.shared);
        let service = shared.service.clone();
        let run = || {
            process_split_subtree_task(
                task,
                &self.service_nodes,
                self.sourcemap_sender.as_ref(),
                started,
            )
        };
        let result = if self.queue.phase.is_core_complete() {
            run()
        } else {
            self.early_pool.install(run)
        };
        result.with_context(|| service)
    }

    fn run_task(&self, task: DirectImportTask) -> Result<()> {
        match task {
            DirectImportTask::Service { service, parts } => self.import_service(service, *parts),
            DirectImportTask::Subtree(task) => self.import_subtree(task),
        }
    }

    fn run(self, worker_index: usize) {
        while let Some(task) = self.queue.receive(worker_index) {
            let _pending_guard = OnDrop::new(|| {
                let _guard = self
                    .pending_signal
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                self.pending_tasks.fetch_sub(1, Ordering::AcqRel);
                self.pending_signal.1.notify_all();
            });
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_task(task)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.record_error(format!("{error:#}")),
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    self.record_error(format!("import worker panicked: {message}"));
                }
            }
        }
    }
}

impl DirectImportDispatcher {
    pub(crate) fn start(
        project_root: PathBuf,
        src_dir: PathBuf,
        active_worker_count: usize,
        drain_worker_count: usize,
        sourcemap_sender: Option<mpsc::Sender<SourcemapWriterMessage>>,
        run_started: Instant,
    ) -> Result<Self> {
        let worker_count = drain_worker_count.max(active_worker_count);
        let phase = Arc::new(DirectImportPhase::default());
        let queue = Arc::new(DirectImportTaskQueue::new(
            active_worker_count,
            Arc::clone(&phase),
        ));
        let early_pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .thread_name(|index| format!("renium-early-import-{index}"))
                .build()
                .context("Failed to create early import worker pool")?,
        );
        let first_error = Arc::new(Mutex::new(None::<String>));
        let service_nodes = Arc::new(Mutex::new(HashMap::<String, SourcemapNode>::new()));
        let pending_tasks = Arc::new(AtomicUsize::new(0));
        let pending_signal = Arc::new((Mutex::new(()), Condvar::new()));

        let worker = DirectImportWorker {
            queue: Arc::clone(&queue),
            project_root,
            src_dir,
            first_error: Arc::clone(&first_error),
            service_nodes: Arc::clone(&service_nodes),
            pending_tasks: Arc::clone(&pending_tasks),
            pending_signal: Arc::clone(&pending_signal),
            sourcemap_sender,
            run_started,
            early_pool,
        };
        let mut workers = Vec::with_capacity(worker_count);
        for worker_index in 0..worker_count {
            let worker = worker.clone();
            workers.push(thread::spawn(move || worker.run(worker_index)));
        }

        Ok(Self {
            queue: Some(queue),
            workers,
            first_error,
            service_nodes,
            pending_tasks,
            pending_signal,
        })
    }

    pub(crate) fn enqueue_parts(&self, service: &str, parts: ExportedSnapshotParts) -> Result<()> {
        self.check_error()?;
        let queue = self
            .queue
            .as_ref()
            .with_context(|| "Direct import dispatcher is closed")?;
        self.pending_tasks.fetch_add(1, Ordering::AcqRel);
        if queue.enqueue_service(DirectImportTask::Service {
            service: service.to_string(),
            parts: Box::new(parts),
        }) {
            Ok(())
        } else {
            self.pending_tasks.fetch_sub(1, Ordering::AcqRel);
            bail!("Failed to queue direct import task: dispatcher is closed")
        }
    }

    pub(crate) fn check_error(&self) -> Result<()> {
        let slot = self
            .first_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(message) = slot.as_ref() {
            bail!("Direct import failed: {message}");
        }
        Ok(())
    }

    fn activate_all_workers(&self) {
        if let Some(queue) = self.queue.as_ref() {
            queue.complete_core_phase();
            queue.activate_workers(self.workers.len());
        }
    }

    pub(crate) fn activate_workers(&self, active_workers: usize) {
        if let Some(queue) = self.queue.as_ref() {
            queue.activate_workers(active_workers.min(self.workers.len()));
        }
    }

    pub(crate) fn finish(mut self) -> Result<HashMap<String, SourcemapNode>> {
        self.activate_all_workers();
        let pending_started = Instant::now();
        let mut pending_guard = self
            .pending_signal
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while self.pending_tasks.load(Ordering::Acquire) > 0 {
            self.check_error()?;
            pending_guard = self
                .pending_signal
                .1
                .wait(pending_guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(pending_guard);
        log_timing("direct import pending wait", pending_started);
        let join_started = Instant::now();
        if let Some(queue) = self.queue.take() {
            queue.close();
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        log_timing("direct import worker join", join_started);
        self.check_error()?;
        let nodes = std::mem::take(
            &mut *self
                .service_nodes
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        Ok(nodes)
    }
}

impl Drop for DirectImportDispatcher {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.take() {
            queue.close();
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

pub(crate) fn import_service_state_with_sourcemap(
    state: &ServiceState,
    project_root: &Path,
    src_root: &Path,
    service: &str,
) -> Result<SourcemapNode> {
    let import_started = Instant::now();
    let write_tree_started = Instant::now();
    let node = import_service_tree(state, project_root, src_root, service)?;
    log_timing(&format!("{service}: write src tree"), write_tree_started);
    log_timing(&format!("{service}: import service total"), import_started);
    Ok(node)
}

pub(crate) fn import_snapshots(args: ImportSnapshotsArgs) -> Result<()> {
    let snapshot_dir = args.snapshot_dir.clone();
    let services = args.services.clone();
    let changed_paths = import_snapshots_inner(args, true)?;
    emit_global_output(
        &json!({
            "ok": true,
            "snapshotDir": snapshot_dir,
            "services": parse_services(&services)?,
            "changedPaths": changed_paths,
        }),
        "Imported snapshots",
    )
}

fn import_snapshots_inner(
    mut args: ImportSnapshotsArgs,
    allow_project_stage: bool,
) -> Result<Vec<PathBuf>> {
    if allow_project_stage {
        apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    }
    let project_root = resolve_existing_project_root(&args.project_root)?;
    let services = parse_services(&args.services)?;
    if allow_project_stage
        && !args.no_project_write
        && config::try_load_project(None, Some(&project_root))?
            .is_some_and(|loaded| loaded.root == project_root)
    {
        let stage = ExportProjectStage::create(&project_root, &args.src_dir, &services)?;
        args.project_root.clone_from(&stage.import_project_root);
        args.src_dir.clone_from(&stage.import_src_dir);
        import_snapshots_inner(args, false)?;
        stage.finish_projection()?;
        return stage.publish(&project_root).map(|paths| {
            paths
                .into_iter()
                .map(|path| project_root.join(path))
                .collect()
        });
    }
    config::refresh_script_naming(&project_root)?;
    let snapshot_dir = canonical_path(&args.snapshot_dir).with_context(|| {
        format!(
            "Failed to resolve snapshot directory: {}",
            args.snapshot_dir.display()
        )
    })?;
    config::validate_relative_portable_path(&args.src_dir, "srcDir")?;
    let src_root = project_root.join(&args.src_dir);
    fs::create_dir_all(&src_root)
        .with_context(|| format!("Failed to create {}", src_root.display()))?;
    let tracked_paths = services
        .iter()
        .map(|service| args.src_dir.join(sanitize_name(service)))
        .chain(std::iter::once(PathBuf::from("sourcemap.json")))
        .collect::<Vec<_>>();
    let before = collect_publish_hashes(&project_root, &tracked_paths)?;

    let thread_count = resolve_thread_count(args.threads, services.len());
    println!(
        "[renium] import-snapshots start: version={}, git={}, build_ts={}, protocol={}, services={}, threads={}",
        BUILD_VERSION,
        BUILD_GIT_HASH,
        BUILD_TIMESTAMP_UNIX,
        BRIDGE_PROTOCOL_VERSION,
        services.len(),
        thread_count
    );
    let mut sourcemap_nodes: HashMap<String, SourcemapNode> = HashMap::new();

    if thread_count <= 1 || services.len() <= 1 {
        for service in &services {
            println!("[renium] {service}: loading snapshot");
            let state = load_service_state(&snapshot_dir, service)?;
            println!("[renium] {service}: writing src tree");
            let node =
                import_service_state_with_sourcemap(&state, &project_root, &src_root, service)?;
            sourcemap_nodes.insert(service.clone(), node);
            println!("[renium] {service}: done");
        }
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .build()
            .context("Failed to build thread pool")?;
        let shared_nodes = Mutex::new(HashMap::<String, SourcemapNode>::new());
        pool.install(|| -> Result<()> {
            services.par_iter().try_for_each(|service| -> Result<()> {
                println!("[renium] {service}: loading snapshot");
                let state = load_service_state(&snapshot_dir, service)?;
                println!("[renium] {service}: writing src tree");
                let node =
                    import_service_state_with_sourcemap(&state, &project_root, &src_root, service)?;
                {
                    let mut nodes = shared_nodes.lock().unwrap_or_else(PoisonError::into_inner);
                    nodes.insert(service.clone(), node);
                }
                println!("[renium] {service}: done");
                Ok(())
            })
        })?;
        let drained_nodes = shared_nodes
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        sourcemap_nodes.extend(drained_nodes);
    }

    finish_import_sourcemap(&project_root, sourcemap_nodes, args.no_project_write)?;

    println!("[renium] import-snapshots done");
    let after = collect_publish_hashes(&project_root, &tracked_paths)?;
    Ok(publish_operation_paths(&before, &after)
        .into_iter()
        .map(|path| project_root.join(path))
        .collect())
}

pub(crate) fn import_service(args: ImportServiceArgs) -> Result<()> {
    import_service_inner(args, true)
}

fn import_service_inner(mut args: ImportServiceArgs, allow_project_stage: bool) -> Result<()> {
    if allow_project_stage {
        apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    }
    let project_root = resolve_existing_project_root(&args.project_root)?;
    let service = parse_single_service(&args.service)?;
    if allow_project_stage
        && !args.no_project_write
        && config::try_load_project(None, Some(&project_root))?
            .is_some_and(|loaded| loaded.root == project_root)
    {
        let stage = ExportProjectStage::create(
            &project_root,
            &args.src_dir,
            std::slice::from_ref(&service),
        )?;
        args.project_root.clone_from(&stage.import_project_root);
        args.src_dir.clone_from(&stage.import_src_dir);
        import_service_inner(args, false)?;
        stage.finish_projection()?;
        stage.publish(&project_root)?;
        return Ok(());
    }
    config::refresh_script_naming(&project_root)?;
    config::validate_relative_portable_path(&args.src_dir, "srcDir")?;
    let src_root = project_root.join(&args.src_dir);
    fs::create_dir_all(&src_root)
        .with_context(|| format!("Failed to create {}", src_root.display()))?;

    println!(
        "[renium] import-service start: version={BUILD_VERSION}, git={BUILD_GIT_HASH}, build_ts={BUILD_TIMESTAMP_UNIX}, protocol={BRIDGE_PROTOCOL_VERSION}, service={service}"
    );

    let payload = read_snapshot_payload(args.snapshot_file.as_deref())?;
    let manifest: SnapshotManifest =
        serde_json::from_str(&payload).context("Invalid snapshot JSON payload")?;

    let state = service_state_from_manifest(&service, manifest)?;
    let node = import_service_state_with_sourcemap(&state, &project_root, &src_root, &service)?;
    let mut sourcemap_nodes = HashMap::new();
    sourcemap_nodes.insert(service.clone(), node);

    finish_import_sourcemap(&project_root, sourcemap_nodes, args.no_project_write)?;

    println!("[renium] import-service done: {service}");
    Ok(())
}

fn syncback_project_adapters_if_configured(project_root: &Path) -> Result<usize> {
    let Some(loaded) = config::try_load_project(None, Some(project_root))? else {
        return Ok(0);
    };
    if loaded.root != project_root {
        return Ok(0);
    }
    if !loaded
        .project
        .adapters
        .iter()
        .any(|adapter| adapter.direction != config::AdapterDirection::ToProject)
    {
        return Ok(0);
    }
    let source_root = loaded.root.join(&loaded.project.source_root);
    let adapters = loaded
        .project
        .adapters
        .iter()
        .filter(|adapter| {
            adapter.direction != config::AdapterDirection::ToProject
                && adapter.target.segments().first().is_some_and(|service| {
                    service_settings_path(&source_root.join(service)).is_file()
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if adapters.is_empty() {
        return Ok(0);
    }
    let mut project = loaded.project.clone();
    project.adapters = adapters;
    let loaded = config::LoadedProject {
        path: loaded.path,
        root: loaded.root,
        project,
    };
    let started = Instant::now();
    let changed = config::syncback_project_adapters(&loaded, false)?;
    log_timing_ms("adapter syncback", elapsed_ms(started));
    Ok(changed)
}

fn finish_import_sourcemap(
    project_root: &Path,
    nodes: HashMap<String, SourcemapNode>,
    update_existing: bool,
) -> Result<()> {
    if update_existing {
        write_project_sourcemap_with_updates(project_root, nodes)
    } else {
        write_project_sourcemap_from_service_nodes(project_root, &nodes)
    }?;
    syncback_project_adapters_if_configured(project_root)?;
    Ok(())
}

pub(crate) fn parse_services(raw: &str) -> Result<Vec<String>> {
    if raw.trim().is_empty() {
        return Ok(DEFAULT_SYNC_SERVICES
            .iter()
            .map(|s| (*s).to_string())
            .collect());
    }

    let mut out = Vec::new();
    for token in raw.split(',') {
        let service = token.trim();
        if service.is_empty() {
            continue;
        }
        if !DEFAULT_SYNC_SERVICES.contains(&service) {
            bail!("Unsupported service: {service}");
        }
        if !out.iter().any(|v| v == service) {
            out.push(service.to_string());
        }
    }
    if out.is_empty() {
        bail!("No valid services provided");
    }
    Ok(out)
}

fn adaptive_tune_service_score(tune: Option<&AdaptiveTuneEntry>) -> f64 {
    let Some(tune) = tune else {
        return 0.0;
    };

    let estimated_total_ms = adaptive_tune_estimated_total_ms(tune)
        .or(tune.wave_ms)
        .unwrap_or(0.0)
        .max(0.0);
    let payload_mb = tune.payload_bytes as f64 / (1024.0 * 1024.0);
    estimated_total_ms
        + payload_mb * 2.0
        + tune.instance_count as f64 / 10_000.0
        + tune.items_fetched as f64 / 20_000.0
}

fn cold_service_export_score(service: &str) -> f64 {
    match service {
        "ServerStorage" => 1_000.0,
        "Workspace" => 600.0,
        "ReplicatedStorage" => 450.0,
        "ServerScriptService" => 180.0,
        "ReplicatedFirst" => 120.0,
        "StarterPlayer" => 90.0,
        "StarterGui" => 80.0,
        "MaterialService" => 70.0,
        "Lighting" => 60.0,
        "StarterPack" => 50.0,
        "Players" => 40.0,
        _ => 0.0,
    }
}

pub(crate) fn direct_import_export_order(
    services: &[String],
    adaptive_tune_cache: &AdaptiveTuneCache,
) -> Vec<String> {
    let mut ranked: Vec<(usize, String, f64)> = services
        .iter()
        .enumerate()
        .map(|(index, service)| {
            let tune_score = adaptive_tune_service_score(adaptive_tune_cache.services.get(service));
            let score = if tune_score > 0.0 {
                tune_score
            } else {
                cold_service_export_score(service)
            };
            (index, service.clone(), score)
        })
        .collect();

    ranked.sort_by(|left, right| {
        right
            .2
            .partial_cmp(&left.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });

    ranked.into_iter().map(|(_, service, _)| service).collect()
}

fn parse_single_service(raw: &str) -> Result<String> {
    let parsed = parse_services(raw)?;
    if parsed.len() != 1 {
        bail!("Expected exactly one service, got {}", parsed.len());
    }
    Ok(parsed[0].clone())
}

fn resolve_thread_count(requested: usize, service_count: usize) -> usize {
    if service_count <= 1 {
        return 1;
    }
    if requested > 0 {
        return requested.min(service_count);
    }
    std::thread::available_parallelism().map_or(1, |value| value.get().min(service_count))
}

pub(crate) fn resolve_source_worker_count(
    requested: usize,
    channel_count: usize,
    script_count: usize,
    instance_count: usize,
) -> usize {
    if script_count <= 1 {
        return 1;
    }

    let channel_count = channel_count.max(1);
    let large_service_cap = if instance_count >= LARGE_SERVICE_DETERMINISTIC_FETCH_MIN_INSTANCES {
        1
    } else if instance_count >= 10_000 {
        2
    } else {
        channel_count
    };
    let soft_target = channel_count.min(large_service_cap);
    let hard_cap = channel_count.saturating_mul(2).min(64);
    let cpu_cap = std::thread::available_parallelism()
        .map_or(8, |v| v.get().saturating_mul(2))
        .max(4);
    let effective_cap = hard_cap.min(cpu_cap).min(script_count);

    if requested > 0 {
        return requested.min(effective_cap);
    }

    soft_target.min(effective_cap)
}

pub(crate) fn fetch_script_sources(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    script_count: usize,
    source_worker_count: usize,
) -> Result<SourceBatchMap> {
    const SOURCE_BATCH_SIZE: usize = 128;
    let mut source_map = SourceBatchMap::default();
    let source_batches: Vec<(usize, usize)> = (1..=script_count)
        .step_by(SOURCE_BATCH_SIZE)
        .map(|start_index| {
            let batch_len = (script_count - start_index + 1).min(SOURCE_BATCH_SIZE);
            (start_index, batch_len)
        })
        .collect();
    if script_count <= 1 || source_worker_count <= 1 {
        let mut loaded_scripts = 0usize;
        let mut metrics = ChunkFetchMetrics::default();
        for (start_index, batch_len) in &source_batches {
            if verbose_timing_logs() {
                println!(
                    "[renium] {service}: script {}/{}",
                    loaded_scripts + 1,
                    script_count
                );
            }
            let (payload, batch_metrics) =
                fetch_json_payload(chunk_size, |chunk_start, max_len| {
                    bridge.call_chunk(
                        "getSourceRangeBatchCompactChunk",
                        json!({
                            "service": service,
                            "startIndex": start_index,
                            "maxCount": batch_len,
                            "chunkStart": chunk_start,
                            "maxLen": max_len,
                        }),
                    )
                })?;
            merge_chunk_fetch_metrics(&mut metrics, batch_metrics);
            let fetched = parse_source_range_batch(payload)
                .with_context(|| format!("Invalid source range payload for {service}"))?;
            loaded_scripts += *batch_len;
            source_map.by_index.extend(fetched.by_index);
            source_map.by_key.extend(fetched.by_key);
        }
        log_chunk_fetch_metrics(&format!("{service}: source payloads"), metrics);
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(source_worker_count)
            .build()
            .context("Failed to create script source worker pool")?;
        let fetched = pool.install(|| {
            source_batches
                .par_iter()
                .enumerate()
                .map(|(index, (start_index, batch_len))| -> Result<(SourceBatchMap, ChunkFetchMetrics)> {
                    let progress_scripts = ((index + 1) * SOURCE_BATCH_SIZE).min(script_count);
                    if verbose_timing_logs()
                        && (index == 0 || progress_scripts == script_count || (index + 1) % 2 == 0)
                    {
                        println!("[renium] {service}: script {progress_scripts}/{script_count}");
                    }
                    let (payload, metrics) = fetch_json_payload(chunk_size, |chunk_start, max_len| {
                        bridge.call_chunk(
                            "getSourceRangeBatchCompactChunk",
                            json!({
                                "service": service,
                                "startIndex": start_index,
                                "maxCount": batch_len,
                                "chunkStart": chunk_start,
                                "maxLen": max_len,
                            }),
                        )
                    })?;
                    Ok((
                        parse_source_range_batch(payload).context("Invalid source range payload")?,
                        metrics,
                    ))
                })
                .collect::<Result<Vec<_>>>()
        })?;

        let mut metrics = ChunkFetchMetrics::default();
        for (batch_sources, batch_metrics) in fetched {
            merge_chunk_fetch_metrics(&mut metrics, batch_metrics);
            source_map.by_index.extend(batch_sources.by_index);
            source_map.by_key.extend(batch_sources.by_key);
        }
        log_chunk_fetch_metrics(&format!("{service}: source payloads"), metrics);
    }

    Ok(source_map)
}

fn direct_import_cpu_cap() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .clamp(2, 16)
}

pub(crate) fn resolve_direct_import_workers(requested: usize) -> usize {
    let cpu_cap = direct_import_cpu_cap();
    if requested > 0 {
        return requested.min(cpu_cap);
    }
    4.min(cpu_cap)
}

pub(crate) fn resolve_direct_import_drain_workers(
    requested: usize,
    active_workers: usize,
) -> usize {
    if requested > 0 {
        return active_workers;
    }
    active_workers.max(8.min(direct_import_cpu_cap()))
}

pub(crate) fn load_service_state(snapshot_dir: &Path, service: &str) -> Result<ServiceState> {
    let snapshot_path = snapshot_dir.join(format!("{service}.json"));
    let manifest: SnapshotManifest = read_json_file(&snapshot_path)
        .with_context(|| format!("Failed to read snapshot: {}", snapshot_path.display()))?;

    service_state_from_manifest(service, manifest)
}

fn service_state_from_manifest(service: &str, manifest: SnapshotManifest) -> Result<ServiceState> {
    if manifest.instances.is_empty() {
        bail!("Snapshot has no instances for service {service}");
    }

    build_service_state_from_instances(
        service,
        None,
        manifest.instances,
        normalize_class_defaults(manifest.class_defaults),
        false,
    )
}

pub(crate) fn build_service_state_from_instances(
    service: &str,
    root_path_from_manifest: Option<&str>,
    mut instances: Vec<SnapshotInstance>,
    class_defaults_by_class: HashMap<String, Map<String, Value>>,
    properties_default_elided: bool,
) -> Result<ServiceState> {
    let instance_count = instances.len();
    let can_use_dense_index_topology = properties_default_elided
        && instances.iter().enumerate().all(|(index, instance)| {
            instance.instance_index == Some(index + 1)
                && instance
                    .parent_index
                    .is_none_or(|parent_index| parent_index > 0 && parent_index <= instances.len())
        });
    if can_use_dense_index_topology {
        let service_root_index = instances
            .iter()
            .position(|instance| instance.instance_index == Some(1))
            .with_context(|| format!("Snapshot missing root service instance: {service}"))?;
        let children_by_index = build_children_by_index_from_dense_parent_indices(&instances);
        let (source_in_subtree, script_count_in_subtree, subtree_sizes) =
            compute_subtree_metrics(&instances, &children_by_index);
        return Ok(ServiceState {
            instances,
            native_properties_by_instance: None,
            children_by_index,
            source_in_subtree,
            script_count_in_subtree,
            subtree_sizes,
            service_root_index,
            class_defaults_by_class,
            properties_default_elided,
            dense_index_topology: true,
        });
    }
    let mut children_by_parent_index: HashMap<usize, Vec<usize>> =
        HashMap::with_capacity(instance_count);
    let mut children_by_parent_instance_id: HashMap<String, Vec<usize>> =
        HashMap::with_capacity(instance_count);
    let mut children_by_parent_debug: HashMap<String, Vec<usize>> =
        HashMap::with_capacity(instance_count);
    let mut index_by_instance_index: HashMap<usize, usize> = HashMap::with_capacity(instance_count);
    let mut index_by_instance_id: HashMap<String, usize> = HashMap::with_capacity(instance_count);

    for (index, instance) in instances.iter().enumerate() {
        if let Some(instance_index) = instance.instance_index.filter(|value| *value > 0) {
            index_by_instance_index
                .entry(instance_index)
                .or_insert(index);
        }

        if let Some(instance_id) = instance.instance_id.as_deref().filter(|s| !s.is_empty()) {
            index_by_instance_id
                .entry(instance_id.to_string())
                .or_insert(index);
        }

        if let Some(parent_index) = instance.parent_index.filter(|value| *value > 0) {
            children_by_parent_index
                .entry(parent_index)
                .or_default()
                .push(index);
        }

        if let Some(parent_instance_id) = instance
            .parent_instance_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            children_by_parent_instance_id
                .entry(parent_instance_id.to_string())
                .or_default()
                .push(index);
        }

        if let Some(parent_debug_id) = instance
            .parent_debug_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            children_by_parent_debug
                .entry(parent_debug_id.to_string())
                .or_default()
                .push(index);
        }
    }

    let service_root_index = find_service_root_index(
        service,
        root_path_from_manifest,
        &instances,
        &index_by_instance_index,
        &index_by_instance_id,
    )
    .with_context(|| {
        format!(
            "Snapshot missing root service instance: {service} (manifest root: {})",
            root_path_from_manifest.unwrap_or("n/a")
        )
    })?;

    let can_use_index_topology = properties_default_elided && !children_by_parent_index.is_empty();
    let needs_path_rebuild = !can_use_index_topology
        && instances
            .iter()
            .any(|instance| instance.path.is_empty() || instance.path_segments.is_empty());
    if needs_path_rebuild
        && (!children_by_parent_index.is_empty() || !children_by_parent_instance_id.is_empty())
    {
        rebuild_instance_paths_from_ids(
            service,
            service_root_index,
            &children_by_parent_index,
            &children_by_parent_instance_id,
            &mut instances,
        );
    }

    let mut children_by_parent_path: HashMap<String, Vec<usize>> = HashMap::new();

    if !can_use_index_topology {
        for (index, instance) in instances.iter().enumerate() {
            let parent_path = instance
                .parent_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(std::string::ToString::to_string)
                .or_else(|| derive_parent_path(&instance.path));

            if let Some(parent_path) = parent_path {
                children_by_parent_path
                    .entry(parent_path)
                    .or_default()
                    .push(index);
            }
        }
    }

    let children_by_index = build_children_by_index(
        &instances,
        &children_by_parent_index,
        &children_by_parent_instance_id,
        &children_by_parent_path,
        &children_by_parent_debug,
    );
    let (source_in_subtree, script_count_in_subtree, subtree_sizes) =
        compute_subtree_metrics(&instances, &children_by_index);

    Ok(ServiceState {
        instances,
        native_properties_by_instance: None,
        children_by_index,
        source_in_subtree,
        script_count_in_subtree,
        subtree_sizes,
        service_root_index,
        class_defaults_by_class,
        properties_default_elided,
        dense_index_topology: false,
    })
}

fn build_children_by_index_from_dense_parent_indices(
    instances: &[SnapshotInstance],
) -> Vec<Vec<usize>> {
    let mut children_by_index = vec![Vec::new(); instances.len()];
    for (index, instance) in instances.iter().enumerate() {
        let Some(parent_index) = instance.parent_index else {
            continue;
        };
        if parent_index == 0 || parent_index > instances.len() {
            continue;
        }
        children_by_index[parent_index - 1].push(index);
    }
    children_by_index
}

fn build_children_by_index(
    instances: &[SnapshotInstance],
    children_by_parent_index: &HashMap<usize, Vec<usize>>,
    children_by_parent_instance_id: &HashMap<String, Vec<usize>>,
    children_by_parent_path: &HashMap<String, Vec<usize>>,
    children_by_parent_debug: &HashMap<String, Vec<usize>>,
) -> Vec<Vec<usize>> {
    (0..instances.len())
        .map(|parent_index| {
            resolve_child_indices_for_instance(
                instances,
                parent_index,
                children_by_parent_index,
                children_by_parent_instance_id,
                children_by_parent_path,
                children_by_parent_debug,
            )
        })
        .collect()
}

fn resolve_child_indices_for_instance(
    instances: &[SnapshotInstance],
    parent_index: usize,
    children_by_parent_index: &HashMap<usize, Vec<usize>>,
    children_by_parent_instance_id: &HashMap<String, Vec<usize>>,
    children_by_parent_path: &HashMap<String, Vec<usize>>,
    children_by_parent_debug: &HashMap<String, Vec<usize>>,
) -> Vec<usize> {
    let Some(instance) = instances.get(parent_index) else {
        return Vec::new();
    };

    let raw_children = instance
        .instance_index
        .filter(|value| *value > 0)
        .and_then(|instance_index| children_by_parent_index.get(&instance_index))
        .or_else(|| {
            instance
                .instance_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|instance_id| children_by_parent_instance_id.get(instance_id))
        })
        .or_else(|| {
            instance
                .debug_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|debug_id| children_by_parent_debug.get(debug_id))
        })
        .or_else(|| children_by_parent_path.get(&instance.path));

    let Some(raw_children) = raw_children else {
        return Vec::new();
    };

    let mut deduped: Vec<usize> = Vec::with_capacity(raw_children.len());
    let mut seen_child_indices: HashSet<usize> = HashSet::with_capacity(raw_children.len());
    for child_index in raw_children {
        if *child_index >= instances.len() {
            continue;
        }
        if seen_child_indices.insert(*child_index) {
            deduped.push(*child_index);
        }
    }

    deduped
}

fn compute_subtree_metrics(
    instances: &[SnapshotInstance],
    children_by_index: &[Vec<usize>],
) -> (Vec<bool>, Vec<usize>, Vec<usize>) {
    struct Frame {
        index: usize,
        next_child: usize,
        has_source: bool,
        script_count: usize,
        subtree_size: usize,
    }

    impl Frame {
        fn new(index: usize, instances: &[SnapshotInstance]) -> Self {
            let has_source = script_file_names(&instances[index].class_name).is_some();
            Self {
                index,
                next_child: 0,
                has_source,
                script_count: usize::from(has_source),
                subtree_size: 1,
            }
        }

        fn add(&mut self, has_source: bool, script_count: usize, subtree_size: usize) {
            self.has_source |= has_source;
            self.script_count = self.script_count.saturating_add(script_count);
            self.subtree_size = self.subtree_size.saturating_add(subtree_size);
        }
    }

    let mut source_flags = vec![false; instances.len()];
    let mut script_counts = vec![0; instances.len()];
    let mut subtree_sizes = vec![0; instances.len()];
    let mut states = vec![0u8; instances.len()];
    let mut stack = Vec::new();

    for root in 0..instances.len() {
        if states[root] == 2 {
            continue;
        }
        states[root] = 1;
        stack.push(Frame::new(root, instances));

        while let Some(frame) = stack.last_mut() {
            let children = children_by_index
                .get(frame.index)
                .map_or(&[][..], Vec::as_slice);
            if let Some(&child) = children.get(frame.next_child) {
                frame.next_child += 1;
                if child >= instances.len() {
                    continue;
                }
                match states[child] {
                    0 => {
                        states[child] = 1;
                        stack.push(Frame::new(child, instances));
                    }
                    1 => {
                        let has_source = script_file_names(&instances[child].class_name).is_some();
                        frame.add(has_source, usize::from(has_source), 1);
                    }
                    _ => frame.add(
                        source_flags[child],
                        script_counts[child],
                        subtree_sizes[child],
                    ),
                }
                continue;
            }

            let frame = stack.pop().expect("metric stack unexpectedly empty");
            source_flags[frame.index] = frame.has_source;
            script_counts[frame.index] = frame.script_count;
            subtree_sizes[frame.index] = frame.subtree_size;
            states[frame.index] = 2;
            if let Some(parent) = stack.last_mut() {
                parent.add(frame.has_source, frame.script_count, frame.subtree_size);
            }
        }
    }

    (source_flags, script_counts, subtree_sizes)
}

fn find_service_root_index(
    service: &str,
    root_path_from_manifest: Option<&str>,
    instances: &[SnapshotInstance],
    index_by_instance_index: &HashMap<usize, usize>,
    index_by_instance_id: &HashMap<String, usize>,
) -> Option<usize> {
    if let Some(index) = index_by_instance_index.get(&1) {
        return Some(*index);
    }

    if let Some(index) = index_by_instance_id.get("1") {
        return Some(*index);
    }

    let mut root_candidates: Vec<String> = Vec::new();
    if let Some(root_path) = root_path_from_manifest {
        root_candidates.push(root_path.to_string());
    }

    let game_candidate = format!("game.{service}");
    if !root_candidates.iter().any(|x| x == &game_candidate) {
        root_candidates.push(game_candidate);
    }
    if !root_candidates.iter().any(|x| x == service) {
        root_candidates.push(service.to_string());
    }

    if let Some(index) = root_candidates.iter().find_map(|candidate| {
        instances
            .iter()
            .position(|instance| instance.path == *candidate)
    }) {
        return Some(index);
    }

    instances
        .iter()
        .position(|instance| {
            instance.parent_index.is_none()
                && instance
                    .parent_instance_id
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
                && (instance.name == service || instance.path == service)
        })
        .or_else(|| {
            instances.iter().position(|instance| {
                instance.parent_index.is_none()
                    && instance
                        .parent_instance_id
                        .as_deref()
                        .unwrap_or("")
                        .is_empty()
            })
        })
}

fn rebuild_instance_paths_from_ids(
    service: &str,
    root_index: usize,
    children_by_parent_index: &HashMap<usize, Vec<usize>>,
    children_by_parent_instance_id: &HashMap<String, Vec<usize>>,
    instances: &mut [SnapshotInstance],
) {
    if root_index >= instances.len() {
        return;
    }

    let root_name = if instances[root_index].name.is_empty() {
        service.to_string()
    } else {
        instances[root_index].name.clone()
    };
    let mut stack: Vec<(usize, Vec<String>)> = vec![(root_index, vec![root_name])];
    let mut visited = vec![false; instances.len()];

    while let Some((index, proposed_segments)) = stack.pop() {
        if index >= instances.len() || visited[index] {
            continue;
        }
        visited[index] = true;

        let effective_segments = if instances[index].path_segments.is_empty() {
            proposed_segments
        } else {
            instances[index].path_segments.clone()
        };
        let path = effective_segments.join(".");

        if instances[index].path_segments.is_empty() {
            instances[index]
                .path_segments
                .clone_from(&effective_segments);
        }
        if instances[index].path.is_empty() {
            instances[index].path.clone_from(&path);
        }
        if index != root_index
            && instances[index]
                .parent_path
                .as_deref()
                .unwrap_or("")
                .is_empty()
        {
            instances[index].parent_path = (effective_segments.len() > 1)
                .then(|| effective_segments[..effective_segments.len() - 1].join("."));
        }

        let child_indices = instances[index]
            .instance_index
            .filter(|value| *value > 0)
            .and_then(|instance_index| children_by_parent_index.get(&instance_index))
            .or_else(|| {
                instances[index]
                    .instance_id
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .and_then(|instance_id| children_by_parent_instance_id.get(instance_id))
            });
        let Some(child_indices) = child_indices else {
            continue;
        };

        for child_index in child_indices.iter().rev() {
            if *child_index >= instances.len() {
                continue;
            }
            let child_name = if instances[*child_index].name.is_empty() {
                instances[*child_index].class_name.to_string()
            } else {
                instances[*child_index].name.clone()
            };
            let mut child_segments = effective_segments.clone();
            child_segments.push(child_name);
            stack.push((*child_index, child_segments));
        }
    }
}

pub(crate) fn normalize_class_defaults(raw: Value) -> HashMap<String, Map<String, Value>> {
    let mut out = HashMap::new();
    let Value::Object(class_defaults) = raw else {
        return out;
    };

    for (class_name, value) in class_defaults {
        let Value::Object(properties) = value else {
            continue;
        };
        if !properties.is_empty() {
            out.insert(class_name, properties);
        }
    }
    out
}

fn read_snapshot_payload(snapshot_file: Option<&Path>) -> Result<String> {
    if let Some(path) = snapshot_file {
        return fs::read_to_string(path)
            .with_context(|| format!("Failed to read snapshot payload: {}", path.display()));
    }

    let mut payload = String::new();
    io::stdin()
        .read_to_string(&mut payload)
        .context("Failed to read snapshot payload from stdin")?;
    if payload.trim().is_empty() {
        bail!("Snapshot payload is empty");
    }
    Ok(payload)
}

fn derive_parent_path(path: &str) -> Option<String> {
    let last_dot = path.rfind('.')?;
    if last_dot == 0 {
        return None;
    }
    Some(path[..last_dot].to_string())
}

const DIRECT_IMPORT_SUBTREE_SPLIT_MIN_INSTANCES: usize = 4_000;
const DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN: usize = 2;
const DIRECT_IMPORT_RECURSIVE_SPLIT_TARGET: usize = 4_000;
const DIRECT_IMPORT_SUBTREE_GROUP_TARGET_INSTANCES: usize = 4_000;
const DIRECT_IMPORT_SUBTREE_GROUP_MAX_ITEMS: usize = 8;
const DIRECT_IMPORT_SUBTREE_SPLIT_MIN_SCRIPT_FILES: usize = 128;
const DIRECT_IMPORT_RECURSIVE_SPLIT_TARGET_SCRIPT_FILES: usize = 48;
const DIRECT_IMPORT_SUBTREE_GROUP_TARGET_SCRIPT_FILES: usize = 48;

struct DirectImportTuning {
    split_min_instances: usize,
    recursive_split_target: usize,
    group_target_instances: usize,
    group_max_items: usize,
    split_min_script_files: usize,
    recursive_split_target_script_files: usize,
    group_target_script_files: usize,
}

fn direct_import_tuning(instance_count: usize, script_count: usize) -> DirectImportTuning {
    if instance_count >= 60_000 {
        DirectImportTuning {
            split_min_instances: 2_000,
            recursive_split_target: 2_000,
            group_target_instances: 1_200,
            group_max_items: 4,
            split_min_script_files: 64,
            recursive_split_target_script_files: 24,
            group_target_script_files: 16,
        }
    } else {
        DirectImportTuning {
            split_min_instances: DIRECT_IMPORT_SUBTREE_SPLIT_MIN_INSTANCES,
            recursive_split_target: DIRECT_IMPORT_RECURSIVE_SPLIT_TARGET,
            group_target_instances: if instance_count >= 25_000 {
                2_000
            } else {
                DIRECT_IMPORT_SUBTREE_GROUP_TARGET_INSTANCES
            },
            group_max_items: DIRECT_IMPORT_SUBTREE_GROUP_MAX_ITEMS,
            split_min_script_files: DIRECT_IMPORT_SUBTREE_SPLIT_MIN_SCRIPT_FILES,
            recursive_split_target_script_files: if instance_count >= 25_000 {
                32
            } else if script_count >= DIRECT_IMPORT_SUBTREE_SPLIT_MIN_SCRIPT_FILES {
                24
            } else {
                DIRECT_IMPORT_RECURSIVE_SPLIT_TARGET_SCRIPT_FILES
            },
            group_target_script_files: if instance_count >= 25_000
                || script_count >= DIRECT_IMPORT_SUBTREE_SPLIT_MIN_SCRIPT_FILES
            {
                24
            } else {
                DIRECT_IMPORT_SUBTREE_GROUP_TARGET_SCRIPT_FILES
            },
        }
    }
}

fn name_child_indices(state: &ServiceState, child_indices: &[usize]) -> Vec<(usize, String)> {
    let mut used_stem_keys = HashSet::new();
    let mut next_suffix_by_base = HashMap::new();
    let mut named_children = Vec::with_capacity(child_indices.len());
    for child_index in child_indices {
        let child = &state.instances[*child_index];
        let child_stem =
            unique_child_stem(&child.name, &mut used_stem_keys, &mut next_suffix_by_base);
        named_children.push((*child_index, child_stem));
    }
    named_children
}

fn maybe_enqueue_split_import_tasks(
    sender: &DirectImportTaskQueue,
    pending_tasks: &AtomicUsize,
    project_root: &Path,
    src_root: &Path,
    service: &str,
    state: ServiceState,
) -> Result<SplitImportDecision> {
    let split_decision_started = Instant::now();
    let service_instance_count = state.instances.len();
    let service_script_count = state
        .script_count_in_subtree
        .get(state.service_root_index)
        .copied()
        .unwrap_or(0);
    let tuning = direct_import_tuning(service_instance_count, service_script_count);
    let root_child_lookup_started = Instant::now();
    let root_children = child_indices_for_instance(&state, state.service_root_index);
    log_timing(
        &format!("{service}: split root child lookup"),
        root_child_lookup_started,
    );
    if (service_instance_count < tuning.split_min_instances
        && service_script_count < tuning.split_min_script_files)
        || root_children.len() < DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN
    {
        log_timing(
            &format!("{service}: split decision"),
            split_decision_started,
        );
        return Ok(SplitImportDecision::Inline(state));
    }

    let name_children_started = Instant::now();
    let named_children = name_child_indices(&state, root_children);
    log_timing(
        &format!("{service}: split name child indices"),
        name_children_started,
    );
    if named_children.len() < DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN {
        log_timing(
            &format!("{service}: split decision"),
            split_decision_started,
        );
        return Ok(SplitImportDecision::Inline(state));
    }

    let final_service_dir = src_root.join(sanitize_name(service));
    let (service_dir, cleanup_required, fresh_stage) =
        prepare_split_import_service_dir(&final_service_dir)?;
    let expected_paths = Arc::new(ImportPathSets::default());
    track_expected_dir(&expected_paths, &service_dir);
    let split_state_setup_started = Instant::now();
    let shared_state = Arc::new(state);
    let settings_state = Arc::clone(&shared_state);
    let settings_dir = service_dir.clone();
    let settings_expected_paths = Arc::clone(&expected_paths);
    let settings_service = service.to_string();
    let settings_write = thread::spawn(move || {
        write_service_settings_file(
            &settings_service,
            &settings_state,
            &settings_dir,
            &settings_expected_paths,
            fresh_stage,
        )
    });

    let visited = (0..shared_state.instances.len())
        .map(|_| AtomicBool::new(false))
        .collect::<Vec<_>>();
    mark_visited(&visited, shared_state.service_root_index);

    let shared = Arc::new(SplitDirectImportState {
        service: service.to_string(),
        service_dir: service_dir.clone(),
        final_service_dir,
        fresh_stage,
        cleanup_required,
        project_root: project_root.to_path_buf(),
        state: shared_state,
        expected_paths,
        settings_write: Mutex::new(Some(settings_write)),
        visited,
        slots: Mutex::new(Vec::new()),
        queued_tasks: AtomicUsize::new(0),
        completed_tasks: AtomicUsize::new(0),
        total_task_tenths_ms: AtomicU64::new(0),
        max_task_tenths_ms: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        started: Instant::now(),
    });
    log_timing(
        &format!("{service}: split state setup"),
        split_state_setup_started,
    );

    let root_child_slots = allocate_split_slots(&shared, named_children.len());
    let root_assembly = Arc::new(SplitNodeAssembly {
        name: service.to_string(),
        class_name: service.to_string(),
        file_paths: Vec::new(),
        child_slots: root_child_slots.clone(),
        remaining_children: AtomicUsize::new(root_child_slots.len()),
        output_slot: None,
        parent: None,
    });

    let split_task_planning_started = Instant::now();
    SplitImportPlanner {
        shared: &shared,
        sender,
        pending_tasks,
    }
    .plan_children(
        &service_dir,
        named_children,
        root_child_slots,
        &root_assembly,
    )?;
    log_timing(
        &format!("{service}: split task planning"),
        split_task_planning_started,
    );
    log_timing(
        &format!("{service}: split decision"),
        split_decision_started,
    );

    Ok(SplitImportDecision::Queued(shared))
}

fn allocate_split_slots(shared: &SplitDirectImportState, count: usize) -> Vec<usize> {
    let mut slots = shared.slots.lock().unwrap_or_else(PoisonError::into_inner);
    let start = slots.len();
    slots.resize(start + count, None);
    drop(slots);
    (start..start + count).collect()
}

struct SplitImportPlanner<'a> {
    shared: &'a Arc<SplitDirectImportState>,
    sender: &'a DirectImportTaskQueue,
    pending_tasks: &'a AtomicUsize,
}

impl SplitImportPlanner<'_> {
    fn tuning(&self) -> DirectImportTuning {
        direct_import_tuning(
            self.shared.state.instances.len(),
            self.shared
                .state
                .script_count_in_subtree
                .get(self.shared.state.service_root_index)
                .copied()
                .unwrap_or(0),
        )
    }

    fn queue_subtrees(&self, items: Vec<DirectImportSubtreeItem>) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        self.shared.queued_tasks.fetch_add(1, Ordering::AcqRel);
        self.pending_tasks.fetch_add(1, Ordering::AcqRel);
        let queue_send_started = Instant::now();
        let queued =
            self.sender
                .enqueue_subtree(DirectImportTask::Subtree(DirectImportSubtreeTask {
                    shared: Arc::clone(self.shared),
                    items,
                }));
        log_timing(
            &format!("{}: split queue send", self.shared.service),
            queue_send_started,
        );
        if queued {
            Ok(())
        } else {
            self.shared.queued_tasks.fetch_sub(1, Ordering::AcqRel);
            self.pending_tasks.fetch_sub(1, Ordering::AcqRel);
            bail!("Failed to queue subtree import task: dispatcher is closed")
        }
    }

    fn plan_children(
        &self,
        parent_dir: &Path,
        named_children: Vec<(usize, String)>,
        child_slots: Vec<usize>,
        parent_assembly: &Arc<SplitNodeAssembly>,
    ) -> Result<()> {
        let tuning = self.tuning();
        let mut group_items = Vec::<DirectImportSubtreeItem>::new();
        let mut group_instances = 0usize;
        let mut group_script_files = 0usize;

        let flush_group = |items: &mut Vec<DirectImportSubtreeItem>,
                           instances: &mut usize,
                           script_files: &mut usize|
         -> Result<()> {
            if items.is_empty() {
                return Ok(());
            }
            self.queue_subtrees(std::mem::take(items))?;
            *instances = 0;
            *script_files = 0;
            Ok(())
        };

        for ((child_index, child_stem), child_slot) in named_children.into_iter().zip(child_slots) {
            let child_indices = child_indices_for_instance(&self.shared.state, child_index);
            let subtree_size = self
                .shared
                .state
                .subtree_sizes
                .get(child_index)
                .copied()
                .unwrap_or(1);
            let subtree_script_files = self
                .shared
                .state
                .script_count_in_subtree
                .get(child_index)
                .copied()
                .unwrap_or(0);
            let should_recurse = (subtree_size > tuning.recursive_split_target
                || subtree_script_files > tuning.recursive_split_target_script_files)
                && child_indices.len() >= DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN;

            if should_recurse {
                flush_group(
                    &mut group_items,
                    &mut group_instances,
                    &mut group_script_files,
                )?;
                self.plan_node(
                    parent_dir,
                    child_index,
                    child_stem,
                    child_slot,
                    Arc::clone(parent_assembly),
                )?;
                continue;
            }

            if !group_items.is_empty()
                && (group_items.len() >= tuning.group_max_items
                    || group_instances.saturating_add(subtree_size) > tuning.group_target_instances
                    || group_script_files.saturating_add(subtree_script_files)
                        > tuning.group_target_script_files)
            {
                flush_group(
                    &mut group_items,
                    &mut group_instances,
                    &mut group_script_files,
                )?;
            }

            group_instances = group_instances.saturating_add(subtree_size);
            group_script_files = group_script_files.saturating_add(subtree_script_files);
            group_items.push(DirectImportSubtreeItem {
                index: child_index,
                parent_dir: parent_dir.to_path_buf(),
                fs_stem: child_stem,
                output_slot: child_slot,
                parent_assembly: Arc::clone(parent_assembly),
            });
        }

        flush_group(
            &mut group_items,
            &mut group_instances,
            &mut group_script_files,
        )
    }

    fn plan_node(
        &self,
        parent_dir: &Path,
        index: usize,
        fs_stem: String,
        output_slot: usize,
        parent_assembly: Arc<SplitNodeAssembly>,
    ) -> Result<()> {
        let tuning = self.tuning();
        let child_indices = child_indices_for_instance(&self.shared.state, index);
        let subtree_size = self
            .shared
            .state
            .subtree_sizes
            .get(index)
            .copied()
            .unwrap_or(1);
        let subtree_script_files = self
            .shared
            .state
            .script_count_in_subtree
            .get(index)
            .copied()
            .unwrap_or(0);
        let has_source = self
            .shared
            .state
            .source_in_subtree
            .get(index)
            .copied()
            .unwrap_or(false);
        if (subtree_size <= tuning.recursive_split_target
            && subtree_script_files <= tuning.recursive_split_target_script_files)
            || child_indices.len() < DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN
            || !has_source
        {
            self.queue_subtrees(vec![DirectImportSubtreeItem {
                index,
                parent_dir: parent_dir.to_path_buf(),
                fs_stem,
                output_slot,
                parent_assembly,
            }])?;
            return Ok(());
        }

        let Some(shell) = self.emit_node_shell(index, parent_dir, &fs_stem)? else {
            bail!("Failed to create split shell for {}", self.shared.service);
        };

        let named_children = name_child_indices(&self.shared.state, child_indices);
        let child_slots = allocate_split_slots(self.shared, named_children.len());
        let assembly = Arc::new(SplitNodeAssembly {
            name: shell.name,
            class_name: shell.class_name,
            file_paths: shell.file_paths,
            child_slots: child_slots.clone(),
            remaining_children: AtomicUsize::new(child_slots.len()),
            output_slot: Some(output_slot),
            parent: Some(parent_assembly),
        });

        self.plan_children(&shell.dir_path, named_children, child_slots, &assembly)
    }
}

fn record_split_task_timing(shared: &SplitDirectImportState, started: Instant) {
    let tenths_ms = (elapsed_ms(started) * 10.0).round().max(0.0) as u64;
    shared.completed_tasks.fetch_add(1, Ordering::AcqRel);
    shared
        .total_task_tenths_ms
        .fetch_add(tenths_ms, Ordering::AcqRel);

    let mut current = shared.max_task_tenths_ms.load(Ordering::Acquire);
    while tenths_ms > current {
        match shared.max_task_tenths_ms.compare_exchange_weak(
            current,
            tenths_ms,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

impl SplitImportPlanner<'_> {
    fn emit_node_shell(
        &self,
        index: usize,
        parent_dir: &Path,
        fs_stem: &str,
    ) -> Result<Option<SplitNodeShell>> {
        if !mark_visited(&self.shared.visited, index) {
            return Ok(None);
        }

        let instance = &self.shared.state.instances[index];
        let class_name = instance.class_name.as_str();

        if let Some((source_file_name, _leaf_suffix)) =
            project_script_file_names(parent_dir, fs_stem, true, class_name, &instance.properties)
        {
            let dir_path = parent_dir.join(fs_stem);
            fs::create_dir_all(&dir_path)
                .with_context(|| format!("Failed to create {}", dir_path.display()))?;
            track_expected_dir(&self.shared.expected_paths, &dir_path);

            let source = instance
                .properties
                .get("Source")
                .and_then(Value::as_str)
                .unwrap_or("");
            let source_path = dir_path.join(source_file_name);
            write_import_source_file(&source_path, source.as_bytes(), self.shared.fresh_stage)?;
            track_expected_file(&self.shared.expected_paths, &source_path);

            return Ok(Some(SplitNodeShell {
                name: fs_stem.to_string(),
                class_name: class_name.to_string(),
                file_paths: vec![path_to_sourcemap_relative(
                    &self.shared.project_root,
                    &source_path,
                )],
                dir_path,
            }));
        }

        if !self
            .shared
            .state
            .source_in_subtree
            .get(index)
            .copied()
            .unwrap_or(false)
        {
            return Ok(None);
        }

        let dir_path = parent_dir.join(fs_stem);
        fs::create_dir_all(&dir_path)
            .with_context(|| format!("Failed to create {}", dir_path.display()))?;
        track_expected_dir(&self.shared.expected_paths, &dir_path);

        Ok(Some(SplitNodeShell {
            name: fs_stem.to_string(),
            class_name: "Folder".to_string(),
            file_paths: Vec::new(),
            dir_path,
        }))
    }
}

fn complete_split_slot(
    shared: &SplitDirectImportState,
    output_slot: usize,
    node: Option<SourcemapNode>,
    parent_assembly: &Arc<SplitNodeAssembly>,
    service_nodes: &Mutex<HashMap<String, SourcemapNode>>,
    sourcemap_sender: Option<&mpsc::Sender<SourcemapWriterMessage>>,
) -> Result<()> {
    {
        let mut slots = shared.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if output_slot < slots.len() {
            slots[output_slot] = node;
        }
    }

    if parent_assembly
        .remaining_children
        .fetch_sub(1, Ordering::AcqRel)
        == 1
    {
        complete_split_assembly(shared, parent_assembly, service_nodes, sourcemap_sender)?;
    }

    Ok(())
}

fn complete_split_assembly(
    shared: &SplitDirectImportState,
    assembly: &Arc<SplitNodeAssembly>,
    service_nodes: &Mutex<HashMap<String, SourcemapNode>>,
    sourcemap_sender: Option<&mpsc::Sender<SourcemapWriterMessage>>,
) -> Result<()> {
    if shared.failed.load(Ordering::Acquire) {
        return Ok(());
    }

    let children = {
        let slots = shared.slots.lock().unwrap_or_else(PoisonError::into_inner);
        assembly
            .child_slots
            .iter()
            .filter_map(|slot| slots.get(*slot).and_then(std::clone::Clone::clone))
            .collect::<Vec<_>>()
    };

    let node = SourcemapNode {
        name: assembly.name.clone(),
        class_name: assembly.class_name.clone(),
        file_paths: assembly.file_paths.clone(),
        children,
    };

    if let Some(output_slot) = assembly.output_slot {
        if let Some(parent) = assembly.parent.as_ref() {
            complete_split_slot(
                shared,
                output_slot,
                Some(node),
                parent,
                service_nodes,
                sourcemap_sender,
            )?;
        }
    } else {
        let settings_write = {
            let mut guard = shared
                .settings_write
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard.take()
        };
        if let Some(handle) = settings_write {
            match handle.join() {
                Ok(result) => result?,
                Err(_) => bail!("{}: settings write worker panicked", shared.service),
            }
        }

        log_timing_ms(
            &format!("{}: expected-path tracking", shared.service),
            expected_path_tracking_ms(&shared.expected_paths),
        );

        if shared.fresh_stage {
            fs::rename(&shared.service_dir, &shared.final_service_dir).with_context(|| {
                format!(
                    "Failed to publish staged service {} to {}",
                    shared.service_dir.display(),
                    shared.final_service_dir.display()
                )
            })?;
        } else if shared.cleanup_required {
            let cleanup_handle = spawn_cleanup_service_dir(
                shared.service_dir.clone(),
                Arc::clone(&shared.expected_paths),
            );
            join_cleanup_handle(&shared.service, cleanup_handle)?;
        }

        let node = build_service_sourcemap_from_state(
            &shared.state,
            &shared.project_root,
            &shared.final_service_dir,
        );

        if let Some(sender) = sourcemap_sender {
            let _ = sender.send(SourcemapWriterMessage::Service(
                shared.service.clone(),
                node.clone(),
            ));
        }

        {
            let mut nodes = service_nodes.lock().unwrap_or_else(PoisonError::into_inner);
            nodes.insert(shared.service.clone(), node);
        }
        let completed_tasks = shared.completed_tasks.load(Ordering::Acquire);
        if completed_tasks > 0 && verbose_timing_logs() {
            let total_ms = shared.total_task_tenths_ms.load(Ordering::Acquire) as f64 / 10.0;
            let max_ms = shared.max_task_tenths_ms.load(Ordering::Acquire) as f64 / 10.0;
            println!(
                "[renium] {}: subtree import tasks done -> tasks={}, avg_ms={:.1}, max_ms={:.1}",
                shared.service,
                completed_tasks,
                total_ms / completed_tasks as f64,
                max_ms
            );
        }
        log_timing(
            &format!("{}: direct import split total", shared.service),
            shared.started,
        );
    }

    Ok(())
}

fn process_split_subtree_task(
    task: DirectImportSubtreeTask,
    service_nodes: &Mutex<HashMap<String, SourcemapNode>>,
    sourcemap_sender: Option<&mpsc::Sender<SourcemapWriterMessage>>,
    started: Instant,
) -> Result<()> {
    let shared = task.shared;
    let mut completed_slots = Vec::with_capacity(task.items.len());
    let mut local_expected = ExpectedPathBatch::default();
    for item in task.items {
        let node = emit_node_index(
            &shared.state,
            item.index,
            &shared.project_root,
            &item.parent_dir,
            &item.fs_stem,
            shared.fresh_stage,
            &shared.visited,
        );

        match node {
            Ok((node, expected)) => {
                local_expected.extend(expected);
                completed_slots.push((item.output_slot, node, item.parent_assembly));
            }
            Err(err) => {
                record_split_task_timing(&shared, started);
                shared.failed.store(true, Ordering::Release);
                return Err(err);
            }
        }
    }

    record_split_task_timing(&shared, started);
    local_expected.merge_into(&shared.expected_paths);
    for (output_slot, node, parent_assembly) in completed_slots {
        complete_split_slot(
            &shared,
            output_slot,
            node,
            &parent_assembly,
            service_nodes,
            sourcemap_sender,
        )?;
    }
    Ok(())
}

fn import_service_tree(
    state: &ServiceState,
    project_root: &Path,
    src_root: &Path,
    service: &str,
) -> Result<SourcemapNode> {
    let service_dir = src_root.join(sanitize_name(service));
    let cleanup_required = ensure_import_service_dir(&service_dir)?;
    let fresh_service_dir = !cleanup_required;
    let expected_paths = Arc::new(ImportPathSets::default());
    track_expected_dir(&expected_paths, &service_dir);
    thread::scope(|scope| -> Result<SourcemapNode> {
        let settings_task = scope.spawn(|| {
            write_service_settings_file(
                service,
                state,
                &service_dir,
                &expected_paths,
                fresh_service_dir,
            )
        });

        let visited = (0..state.instances.len())
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>();
        mark_visited(&visited, state.service_root_index);

        let root_children = child_indices_for_instance(state, state.service_root_index);
        let (_, expected_batch) = emit_children_indices(
            state,
            root_children,
            project_root,
            &service_dir,
            fresh_service_dir,
            &visited,
        )?;
        expected_batch.merge_into(&expected_paths);

        match settings_task.join() {
            Ok(result) => result?,
            Err(_) => bail!("{service}: settings write worker panicked"),
        }

        log_timing_ms(
            &format!("{service}: expected-path tracking"),
            expected_path_tracking_ms(&expected_paths),
        );

        if cleanup_required {
            let cleanup_handle =
                spawn_cleanup_service_dir(service_dir.clone(), Arc::clone(&expected_paths));
            join_cleanup_handle(service, cleanup_handle)?;
        }

        Ok(build_service_sourcemap_from_state(
            state,
            project_root,
            &service_dir,
        ))
    })
}

#[derive(Default)]
struct ImportPathSets {
    files: Mutex<HashSet<String>>,
    dirs: Mutex<HashSet<String>>,
    tracking_tenths_ms: AtomicU64,
}

#[derive(Default)]
struct ExpectedPathBatch {
    files: Vec<String>,
    dirs: Vec<String>,
}

impl ExpectedPathBatch {
    fn track_file(&mut self, path: &Path) {
        self.files.push(path_key(path));
    }

    fn track_dir(&mut self, path: &Path) {
        self.dirs.push(path_key(path));
    }

    fn extend(&mut self, mut other: ExpectedPathBatch) {
        self.files.append(&mut other.files);
        self.dirs.append(&mut other.dirs);
    }

    fn merge_into(self, expected_paths: &ImportPathSets) {
        let started = Instant::now();
        if !self.files.is_empty() {
            let mut files = expected_paths
                .files
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for path in self.files {
                files.insert(path);
            }
        }
        if !self.dirs.is_empty() {
            let mut dirs = expected_paths
                .dirs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for path in self.dirs {
                dirs.insert(path);
            }
        }
        let tenths_ms = (elapsed_ms(started) * 10.0).round().max(0.0) as u64;
        expected_paths
            .tracking_tenths_ms
            .fetch_add(tenths_ms, Ordering::Relaxed);
    }
}

fn expected_path_tracking_ms(expected_paths: &ImportPathSets) -> f64 {
    expected_paths.tracking_tenths_ms.load(Ordering::Relaxed) as f64 / 10.0
}

fn ensure_import_service_dir(service_dir: &Path) -> Result<bool> {
    if let Some(parent) = service_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    match fs::create_dir(service_dir) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && service_dir.is_dir() => {
            Ok(true)
        }
        Err(error) => {
            Err(error).with_context(|| format!("Failed to create {}", service_dir.display()))
        }
    }
}

fn prepare_split_import_service_dir(final_service_dir: &Path) -> Result<(PathBuf, bool, bool)> {
    match fs::metadata(final_service_dir) {
        Ok(metadata) if metadata.is_dir() => {
            return Ok((final_service_dir.to_path_buf(), true, false));
        }
        Ok(_) => bail!(
            "Import service path is not a directory: {}",
            final_service_dir.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to stat {}", final_service_dir.display()));
        }
    }
    let parent = final_service_dir
        .parent()
        .with_context(|| "Import service path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let service_name = final_service_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("service");
    for _ in 0..32 {
        let sequence = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stage = parent.join(format!(
            ".{service_name}.{}-{sequence}.renium-import",
            std::process::id()
        ));
        match fs::create_dir(&stage) {
            Ok(()) => return Ok((stage, false, true)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to create {}", stage.display()));
            }
        }
    }
    bail!(
        "Failed to allocate a staging directory for {}",
        final_service_dir.display()
    )
}

fn track_expected_file(expected_paths: &ImportPathSets, path: &Path) {
    let mut batch = ExpectedPathBatch::default();
    batch.track_file(path);
    batch.merge_into(expected_paths);
}

fn track_expected_dir(expected_paths: &ImportPathSets, path: &Path) {
    let mut batch = ExpectedPathBatch::default();
    batch.track_dir(path);
    batch.merge_into(expected_paths);
}

fn write_service_settings_file(
    service: &str,
    state: &ServiceState,
    service_dir: &Path,
    expected_paths: &ImportPathSets,
    fresh_stage: bool,
) -> Result<()> {
    let settings_path = service_settings_path(service_dir);
    track_expected_file(expected_paths, &settings_path);
    if !fresh_stage {
        track_expected_file(
            expected_paths,
            &PathBuf::from(format!("{}.lock", settings_path.display())),
        );
    }
    let started = Instant::now();
    if fresh_stage {
        write_fresh_service_settings_binary_file(&settings_path, state)?;
    } else {
        let _lock = acquire_settings_file_lock(&settings_path)?;
        let preserved_state =
            state_with_preserved_material_service_settings(service, state, &settings_path)?;
        let state_to_write = preserved_state.as_ref().unwrap_or(state);
        write_service_settings_binary_file(&settings_path, state_to_write)?;
    }
    log_timing(&format!("{service}: write settings file"), started);
    Ok(())
}

pub(crate) fn state_with_preserved_material_service_settings(
    service: &str,
    state: &ServiceState,
    settings_path: &Path,
) -> Result<Option<ServiceState>> {
    if service != MATERIAL_SERVICE_CLASS || !settings_path.exists() {
        return Ok(None);
    }
    let Some(root) = state.instances.get(state.service_root_index) else {
        return Ok(None);
    };
    if root.properties.contains_key(USE_2022_MATERIALS_PROPERTY) {
        return Ok(None);
    }

    let existing = SettingsBytecode::read_file(settings_path)
        .with_context(|| format!("Failed to read {}", settings_path.display()))?;
    let Some(existing_root_index) = editor_service_root_index(&existing, MATERIAL_SERVICE_CLASS)
        .or_else(|| settings_root_indices(&existing).into_iter().next())
    else {
        return Ok(None);
    };
    let Some(value) = existing.instances[existing_root_index]
        .properties
        .get(USE_2022_MATERIALS_PROPERTY)
        .cloned()
    else {
        return Ok(None);
    };

    let mut preserved = state.clone();
    if let Some(root) = preserved.instances.get_mut(preserved.service_root_index) {
        root.properties
            .insert(USE_2022_MATERIALS_PROPERTY.to_string(), value);
        Ok(Some(preserved))
    } else {
        Ok(None)
    }
}

fn spawn_cleanup_service_dir(
    service_dir: PathBuf,
    expected_paths: Arc<ImportPathSets>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let cleanup_started = Instant::now();
        cleanup_service_dir(&service_dir, &expected_paths)?;
        log_timing(
            &format!("cleanup {}", service_dir.display()),
            cleanup_started,
        );
        Ok(())
    })
}

fn join_cleanup_handle(service: &str, handle: thread::JoinHandle<Result<()>>) -> Result<()> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("{service}: cleanup failed")),
        Err(_) => bail!("{service}: cleanup worker panicked"),
    }
}

fn cleanup_service_dir(service_dir: &Path, expected_paths: &ImportPathSets) -> Result<()> {
    if !service_dir.exists() {
        return Ok(());
    }

    let expected_files = {
        let guard = expected_paths
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    };
    let expected_dirs = {
        let guard = expected_paths
            .dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    };

    let mut stale_files = Vec::new();
    let mut stale_dirs = Vec::new();
    let scan_started = Instant::now();
    collect_stale_paths(
        service_dir,
        &expected_files,
        &expected_dirs,
        &mut stale_files,
        &mut stale_dirs,
    )?;
    log_timing(
        &format!("cleanup scan {}", service_dir.display()),
        scan_started,
    );

    let delete_started = Instant::now();
    let backup = quarantine_stale_import_paths(service_dir, &stale_files, &stale_dirs)?;
    if let Some(backup) = backup {
        println!(
            "[renium] moved {} stale file(s) and {} stale directorie(s) to {}",
            stale_files.len(),
            stale_dirs.len(),
            backup.display()
        );
    }
    log_timing(
        &format!("cleanup quarantine {}", service_dir.display()),
        delete_started,
    );

    Ok(())
}

pub(crate) fn quarantine_stale_import_paths(
    service_dir: &Path,
    stale_files: &[PathBuf],
    stale_dirs: &[PathBuf],
) -> Result<Option<PathBuf>> {
    if stale_files.is_empty() && stale_dirs.is_empty() {
        return Ok(None);
    }
    let src_root = service_dir
        .parent()
        .context("Import service directory has no src parent")?;
    let project_root = src_root
        .parent()
        .context("Import src directory has no project parent")?;
    let service_name = service_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("service");
    let backup_root = project_root
        .join(".renium")
        .join("import-backups")
        .join(format!("{}-{}", current_millis(), std::process::id()))
        .join(service_name);

    let mut ordered_dirs = stale_dirs.to_vec();
    ordered_dirs.sort_by_key(|path| path.components().count());
    let mut root_dirs: Vec<PathBuf> = Vec::new();
    for dir in ordered_dirs {
        if !root_dirs.iter().any(|root| dir.starts_with(root)) {
            root_dirs.push(dir);
        }
    }

    for dir in &root_dirs {
        if !dir.exists() {
            continue;
        }
        let relative = dir.strip_prefix(service_dir).with_context(|| {
            format!(
                "Stale import directory escaped service root: {}",
                dir.display()
            )
        })?;
        let destination = backup_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::rename(dir, &destination).with_context(|| {
            format!(
                "Failed to move stale import directory {} to {}",
                dir.display(),
                destination.display()
            )
        })?;
    }

    for file in stale_files {
        if root_dirs.iter().any(|root| file.starts_with(root)) || !file.exists() {
            continue;
        }
        let relative = file.strip_prefix(service_dir).with_context(|| {
            format!("Stale import file escaped service root: {}", file.display())
        })?;
        let destination = backup_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::rename(file, &destination).with_context(|| {
            format!(
                "Failed to move stale import file {} to {}",
                file.display(),
                destination.display()
            )
        })?;
    }
    Ok(Some(backup_root))
}

fn collect_stale_paths(
    current_dir: &Path,
    expected_files: &HashSet<String>,
    expected_dirs: &HashSet<String>,
    stale_files: &mut Vec<PathBuf>,
    stale_dirs: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries = fs::read_dir(current_dir)
        .with_context(|| format!("Failed to read {}", current_dir.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("Failed to iterate {}", current_dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to stat {}", path.display()))?;
        if file_type.is_dir() {
            collect_stale_paths(
                &path,
                expected_files,
                expected_dirs,
                stale_files,
                stale_dirs,
            )?;
            if !expected_dirs.contains(&path_key(&path)) {
                stale_dirs.push(path);
            }
            continue;
        }
        if file_type.is_file() && !expected_files.contains(&path_key(&path)) {
            stale_files.push(path);
        }
    }
    Ok(())
}

fn emit_node_index(
    state: &ServiceState,
    index: usize,
    project_root: &Path,
    parent_dir: &Path,
    fs_stem: &str,
    fresh_stage: bool,
    visited: &[AtomicBool],
) -> Result<(Option<SourcemapNode>, ExpectedPathBatch)> {
    if !mark_visited(visited, index) {
        return Ok((None, ExpectedPathBatch::default()));
    }

    let instance = &state.instances[index];
    let child_indices = child_indices_for_instance(state, index);
    let has_children = !child_indices.is_empty();
    let class_name = instance.class_name.as_str();
    let mut expected = ExpectedPathBatch::default();

    if let Some((source_file_name, leaf_suffix)) = project_script_file_names(
        parent_dir,
        fs_stem,
        has_children,
        class_name,
        &instance.properties,
    ) {
        let source = instance
            .properties
            .get("Source")
            .and_then(Value::as_str)
            .unwrap_or("");

        if has_children {
            let dir_path = parent_dir.join(fs_stem);
            fs::create_dir_all(&dir_path)
                .with_context(|| format!("Failed to create {}", dir_path.display()))?;
            expected.track_dir(&dir_path);
            let source_path = dir_path.join(source_file_name);
            write_script_source_file(&source_path, source, fresh_stage)?;
            expected.track_file(&source_path);

            let (children, child_expected) = emit_children_indices(
                state,
                child_indices,
                project_root,
                &dir_path,
                fresh_stage,
                visited,
            )?;
            expected.extend(child_expected);
            return Ok((
                Some(SourcemapNode {
                    name: fs_stem.to_string(),
                    class_name: class_name.to_string(),
                    file_paths: vec![path_to_sourcemap_relative(project_root, &source_path)],
                    children,
                }),
                expected,
            ));
        }

        let script_path = parent_dir.join(format!("{fs_stem}{leaf_suffix}"));
        write_script_source_file(&script_path, source, fresh_stage)?;
        expected.track_file(&script_path);

        return Ok((
            Some(SourcemapNode {
                name: fs_stem.to_string(),
                class_name: class_name.to_string(),
                file_paths: vec![path_to_sourcemap_relative(project_root, &script_path)],
                children: Vec::new(),
            }),
            expected,
        ));
    }

    if !state.source_in_subtree.get(index).copied().unwrap_or(false) {
        return Ok((None, expected));
    }

    let dir_path = parent_dir.join(fs_stem);
    fs::create_dir_all(&dir_path)
        .with_context(|| format!("Failed to create {}", dir_path.display()))?;
    expected.track_dir(&dir_path);

    let (children, child_expected) = emit_children_indices(
        state,
        child_indices,
        project_root,
        &dir_path,
        fresh_stage,
        visited,
    )?;
    expected.extend(child_expected);

    if children.is_empty() {
        return Ok((None, expected));
    }

    Ok((
        Some(SourcemapNode {
            name: fs_stem.to_string(),
            class_name: "Folder".to_string(),
            file_paths: Vec::new(),
            children,
        }),
        expected,
    ))
}

fn emit_children_indices(
    state: &ServiceState,
    child_indices: &[usize],
    project_root: &Path,
    dir_path: &Path,
    fresh_stage: bool,
    visited: &[AtomicBool],
) -> Result<(Vec<SourcemapNode>, ExpectedPathBatch)> {
    let named_children = name_child_indices(state, child_indices);

    const PARALLEL_CHILD_THRESHOLD: usize = 8;
    if named_children.len() >= PARALLEL_CHILD_THRESHOLD && rayon::current_num_threads() > 1 {
        let built = named_children
            .par_iter()
            .map(|(child_index, child_stem)| {
                emit_node_index(
                    state,
                    *child_index,
                    project_root,
                    dir_path,
                    child_stem,
                    fresh_stage,
                    visited,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let mut nodes = Vec::with_capacity(named_children.len());
        let mut expected = ExpectedPathBatch::default();
        for (node, child_expected) in built {
            if let Some(node) = node {
                nodes.push(node);
            }
            expected.extend(child_expected);
        }
        Ok((nodes, expected))
    } else {
        let mut built = Vec::with_capacity(named_children.len());
        let mut expected = ExpectedPathBatch::default();
        for (child_index, child_stem) in named_children {
            let (node, child_expected) = emit_node_index(
                state,
                child_index,
                project_root,
                dir_path,
                &child_stem,
                fresh_stage,
                visited,
            )?;
            expected.extend(child_expected);
            if let Some(node) = node {
                built.push(node);
            }
        }
        Ok((built, expected))
    }
}

fn mark_visited(visited: &[AtomicBool], index: usize) -> bool {
    if index >= visited.len() {
        return false;
    }
    !visited[index].swap(true, Ordering::AcqRel)
}

fn write_import_source_file(source_path: &Path, content: &[u8], fresh_stage: bool) -> Result<()> {
    if !fresh_stage {
        return write_bytes_if_changed_in_existing_dir(source_path, content);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(source_path)
        .with_context(|| format!("Failed to create {}", source_path.display()))?;
    file.write_all(content)
        .with_context(|| format!("Failed to write {}", source_path.display()))
}

fn write_script_source_file(source_path: &Path, source: &str, fresh_stage: bool) -> Result<()> {
    if source == "__SOURCE_EXTERNAL__" {
        if source_path.exists() {
            println!(
                "[renium] warning: missing fetched Source for {}; keeping the existing file",
                source_path.display()
            );
            return Ok(());
        }
        println!(
            "[renium] warning: missing fetched Source for {}; writing an empty script",
            source_path.display()
        );
        return write_import_source_file(source_path, b"", fresh_stage);
    }
    write_import_source_file(source_path, source.as_bytes(), fresh_stage)
}
