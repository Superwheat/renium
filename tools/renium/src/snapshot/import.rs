use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde_json::{Map, Value, json};

use crate::app::profile;
use crate::app::timing::{elapsed_ms, log_timing, log_timing_ms, verbose_timing_logs};
use crate::bytecode::acquire_settings_file_lock;
use crate::editor::paths::{project_script_file_names_in_layout, script_file_names};
use crate::project::sourcemap::{
    SourcemapNode, build_service_sourcemap_from_state, finalize_project_sourcemap_temp,
    load_existing_sourcemap_root, sourcemap_root_is_current,
};
use crate::rbx::encode::settings_root_indices;
use crate::roblox::schema::{MATERIAL_SERVICE_CLASS, USE_2022_MATERIALS_PROPERTY};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::{
    PulledDigest, PulledRows, SettingsBytecode, child_indices_for_instance,
    decode_settings_bytecode_holding, encode_service_settings_binary, pull_changed_document_rows,
    pull_changed_values, pulled_digest_from_service_state, settings_bytecode_from_service_state,
    write_fresh_service_settings_binary_file,
};
use crate::settings::equivalence::{
    SettingsAlignment, align_pulled_settings_bytes_to_reference,
    align_pulled_settings_documents_to_reference, viewport_camera_lacks_enum_types,
};
use crate::settings::tree::editor_service_root_index;
use crate::snapshot::codec::parse_source_range_batch;
use crate::snapshot::export::{
    LARGE_SERVICE_DETERMINISTIC_FETCH_MIN_INSTANCES, exported_parts_to_service_state,
    fetch_json_payload, log_chunk_fetch_metrics, merge_chunk_fetch_metrics,
};
use crate::snapshot::types::{ExportedSnapshotParts, ServiceState, SnapshotInstance};
use crate::studio::bridge::{BridgeServer, ChunkFetchMetrics, SourceBatchMap};
use crate::system::LockRecover;
use crate::system::files::{
    OnDrop, path_key, sanitize_name, service_settings_path, unique_child_stem,
    write_bytes_if_changed_in_existing_dir,
};
use sha2::{Digest, Sha256};

struct DirectImportTask {
    service: String,
    parts: Box<ExportedSnapshotParts>,
}

#[derive(Default)]
struct DirectImportTaskQueueState {
    services: VecDeque<DirectImportTask>,
    active_workers: usize,
    closed: bool,
}

struct DirectImportTaskQueue {
    state: Mutex<DirectImportTaskQueueState>,
    ready: Condvar,
    worker_gate: Condvar,
}

impl DirectImportTaskQueue {
    fn new(active_workers: usize) -> Self {
        Self {
            state: Mutex::new(DirectImportTaskQueueState {
                active_workers: active_workers.max(1),
                ..Default::default()
            }),
            ready: Condvar::new(),
            worker_gate: Condvar::new(),
        }
    }

    fn enqueue_service(&self, task: DirectImportTask) -> bool {
        let mut state = self.state.lock_recover();
        if state.closed {
            return false;
        }
        state.services.push_back(task);
        drop(state);
        self.ready.notify_one();
        true
    }

    fn receive(&self, worker_index: usize) -> Option<DirectImportTask> {
        let mut state = self.state.lock_recover();
        loop {
            if worker_index >= state.active_workers && !state.closed {
                state = self
                    .worker_gate
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if let Some(task) = state.services.pop_front() {
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
        let mut state = self.state.lock_recover();
        state.active_workers = state.active_workers.max(active_workers);
        drop(state);
        self.worker_gate.notify_all();
        self.ready.notify_all();
    }

    fn close(&self) {
        let mut state = self.state.lock_recover();
        state.closed = true;
        drop(state);
        self.worker_gate.notify_all();
        self.ready.notify_all();
    }
}

pub(crate) enum SourcemapWriterMessage {
    Service(String, SourcemapNode),
    Finish,
}

pub(crate) struct SourcemapWriter {
    sender: mpsc::Sender<SourcemapWriterMessage>,
    handle: Option<thread::JoinHandle<Result<()>>>,
}

fn update_sourcemap_service_node(
    service_nodes: &mut HashMap<String, SourcemapNode>,
    service: String,
    node: SourcemapNode,
) -> bool {
    if service_nodes.get(&service) == Some(&node) {
        return false;
    }
    service_nodes.insert(service, node);
    true
}

impl SourcemapWriter {
    pub(crate) fn start(
        project_root: PathBuf,
        durable: bool,
        ready: Option<ImportReadyGate>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<SourcemapWriterMessage>();
        let handle = thread::spawn(move || -> Result<()> {
            if let Some(ready) = &ready {
                ready()?;
            }
            let existing_root = load_existing_sourcemap_root(&project_root)?;
            let mut wrote_update = existing_root
                .as_ref()
                .is_none_or(|root| !sourcemap_root_is_current(&project_root, root));
            let mut service_nodes = existing_root
                .map(|root| {
                    root.children
                        .into_iter()
                        .map(|node| (node.name.clone(), node))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            while let Ok(message) = receiver.recv() {
                let mut pending_finish = false;
                match message {
                    SourcemapWriterMessage::Service(service, node) => {
                        wrote_update |=
                            update_sourcemap_service_node(&mut service_nodes, service, node);
                    }
                    SourcemapWriterMessage::Finish => pending_finish = true,
                }

                while let Ok(pending) = receiver.try_recv() {
                    match pending {
                        SourcemapWriterMessage::Service(service, node) => {
                            wrote_update |=
                                update_sourcemap_service_node(&mut service_nodes, service, node);
                        }
                        SourcemapWriterMessage::Finish => {
                            pending_finish = true;
                            break;
                        }
                    }
                }

                if pending_finish {
                    if wrote_update {
                        finalize_project_sourcemap_temp(&project_root, &service_nodes, durable)?;
                    }
                    return Ok(());
                }
            }
            if wrote_update {
                finalize_project_sourcemap_temp(&project_root, &service_nodes, durable)?;
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

/// Blocks until the import destination is fully prepared; see
/// `ExportProjectStage::copy_gate`.
pub(crate) type ImportReadyGate = Arc<dyn Fn() -> Result<()> + Send + Sync>;

/// Where a direct import writes, and whose existing script files decide the
/// names and forms of the imported scripts (the written tree itself when
/// `layout_src_root` is `None`).
#[derive(Clone)]
pub(crate) struct DirectImportRoots {
    pub(crate) project_root: PathBuf,
    pub(crate) src_dir: PathBuf,
    pub(crate) layout_src_root: Option<PathBuf>,
}

#[derive(Clone)]
struct DirectImportWorker {
    queue: Arc<DirectImportTaskQueue>,
    ready: Option<ImportReadyGate>,
    roots: DirectImportRoots,
    first_error: Arc<Mutex<Option<String>>>,
    service_nodes: Arc<Mutex<HashMap<String, SourcemapNode>>>,
    pending_tasks: Arc<AtomicUsize>,
    pending_signal: Arc<(Mutex<()>, Condvar)>,
    sourcemap_sender: Option<mpsc::Sender<SourcemapWriterMessage>>,
    run_started: Instant,
}

impl DirectImportWorker {
    fn record_error(&self, error: String) {
        let mut slot = self.first_error.lock_recover();
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
        let src_root = self.roots.project_root.join(&self.roots.src_dir);
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
                // Conversion above is CPU-only; wait for the destination only
                // before reading or writing it.
                if let Some(ready) = &self.ready {
                    let _waiting = profile::span(&format!("{service}: wait for the stage"));
                    ready()?;
                }
                let node = import_service_state_with_sourcemap(
                    &state,
                    &self.roots.project_root,
                    &src_root,
                    self.roots.layout_src_root.as_deref(),
                    &service,
                );
                // Freeing a large captured service takes a few hundred
                // milliseconds that nothing after it waits for.
                rayon::spawn(move || drop(state));
                node
            });
        match result {
            Ok(node) => {
                if let Some(sender) = &self.sourcemap_sender {
                    let _ = sender.send(SourcemapWriterMessage::Service(
                        service.clone(),
                        node.clone(),
                    ));
                }
                self.service_nodes
                    .lock_recover()
                    .insert(service.clone(), node);
            }
            Err(error) => {
                self.log_service_span(&service, started_ms, started, true);
                return Err(error).with_context(|| service);
            }
        }
        self.log_service_span(&service, started_ms, started, false);
        Ok(())
    }

    fn run_task(&self, task: DirectImportTask) -> Result<()> {
        self.import_service(task.service, *task.parts)
    }

    fn run(self, worker_index: usize) {
        while let Some(task) = self.queue.receive(worker_index) {
            let _pending_guard = OnDrop::new(|| {
                let _guard = self.pending_signal.0.lock_recover();
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
        roots: DirectImportRoots,
        active_worker_count: usize,
        drain_worker_count: usize,
        sourcemap_sender: Option<mpsc::Sender<SourcemapWriterMessage>>,
        run_started: Instant,
        ready: Option<ImportReadyGate>,
    ) -> Result<Self> {
        let worker_count = drain_worker_count.max(active_worker_count);
        let queue = Arc::new(DirectImportTaskQueue::new(active_worker_count));
        let first_error = Arc::new(Mutex::new(None::<String>));
        let service_nodes = Arc::new(Mutex::new(HashMap::<String, SourcemapNode>::new()));
        let pending_tasks = Arc::new(AtomicUsize::new(0));
        let pending_signal = Arc::new((Mutex::new(()), Condvar::new()));

        let worker = DirectImportWorker {
            queue: Arc::clone(&queue),
            ready,
            roots,
            first_error: Arc::clone(&first_error),
            service_nodes: Arc::clone(&service_nodes),
            pending_tasks: Arc::clone(&pending_tasks),
            pending_signal: Arc::clone(&pending_signal),
            sourcemap_sender,
            run_started,
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
        if queue.enqueue_service(DirectImportTask {
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
        let slot = self.first_error.lock_recover();
        if let Some(message) = slot.as_ref() {
            bail!("Direct import failed: {message}");
        }
        Ok(())
    }

    fn activate_all_workers(&self) {
        if let Some(queue) = self.queue.as_ref() {
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
        let mut pending_guard = self.pending_signal.0.lock_recover();
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
        let nodes = std::mem::take(&mut *self.service_nodes.lock_recover());
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
    layout_src_root: Option<&Path>,
    service: &str,
) -> Result<SourcemapNode> {
    let import_started = Instant::now();
    let write_tree_started = Instant::now();
    let node = import_service_tree(state, project_root, src_root, layout_src_root, service)?;
    log_timing(&format!("{service}: write src tree"), write_tree_started);
    log_timing(&format!("{service}: import service total"), import_started);
    Ok(node)
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

pub(crate) fn direct_import_export_order(services: &[String]) -> Vec<String> {
    let mut ranked: Vec<(usize, String, f64)> = services
        .iter()
        .enumerate()
        .map(|(index, service)| (index, service.clone(), cold_service_export_score(service)))
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

pub(crate) fn resolve_source_worker_count(
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
    soft_target.min(effective_cap)
}

pub(crate) fn fetch_script_sources(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    script_count: usize,
    source_worker_count: usize,
    export_id: Option<&str>,
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
                            "exportId": export_id,
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
                                "exportId": export_id,
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

pub(crate) fn merge_script_sources(
    instances: &mut [SnapshotInstance],
    source_map: &SourceBatchMap,
) {
    for instance in instances {
        if !matches!(
            instance.class_name.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        ) {
            instance.source_key = None;
            continue;
        }
        let source = instance
            .instance_index
            .and_then(|index| source_map.by_index.get(&index))
            .or_else(|| {
                instance
                    .source_key
                    .as_deref()
                    .and_then(|key| source_map.by_key.get(key))
            })
            .or_else(|| {
                instance
                    .instance_id
                    .as_deref()
                    .and_then(|id| source_map.by_key.get(&format!("id:{id}")))
            })
            .or_else(|| {
                instance
                    .instance_index
                    .and_then(|index| source_map.by_key.get(&format!("id:{index:x}")))
            })
            .or_else(|| {
                instance
                    .debug_id
                    .as_deref()
                    .and_then(|id| source_map.by_key.get(&format!("debug:{id}")))
            })
            .or_else(|| source_map.by_key.get(&instance.path));
        if let Some(source) = source {
            instance
                .properties
                .insert("Source".to_string(), Value::String(source.clone()));
        } else if instance.source_key.is_some() {
            instance
                .properties
                .entry("Source".to_string())
                .or_insert_with(|| Value::String(EXTERNAL_SOURCE_MARKER.to_string()));
        }
        instance.source_key = None;
    }
}

fn direct_import_cpu_cap() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .clamp(2, 16)
}

pub(crate) fn resolve_direct_import_workers() -> usize {
    4.min(direct_import_cpu_cap())
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
        let source_in_subtree = compute_source_in_subtree(&instances, &children_by_index);
        return Ok(ServiceState {
            instances,
            native_properties_by_instance: None,
            children_by_index,
            source_in_subtree,
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
    let source_in_subtree = compute_source_in_subtree(&instances, &children_by_index);

    Ok(ServiceState {
        instances,
        native_properties_by_instance: None,
        children_by_index,
        source_in_subtree,
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

fn compute_source_in_subtree(
    instances: &[SnapshotInstance],
    children_by_index: &[Vec<usize>],
) -> Vec<bool> {
    let mut source_flags = vec![false; instances.len()];
    let mut states = vec![0u8; instances.len()];
    let mut stack = Vec::new();
    for root in 0..instances.len() {
        if states[root] == 2 {
            continue;
        }
        states[root] = 1;
        stack.push((
            root,
            0usize,
            script_file_names(&instances[root].class_name).is_some(),
        ));
        while let Some((index, next_child, has_source)) = stack.last_mut() {
            let children = children_by_index.get(*index).map_or(&[][..], Vec::as_slice);
            if let Some(&child) = children.get(*next_child) {
                *next_child += 1;
                if child >= instances.len() {
                    continue;
                }
                match states[child] {
                    0 => {
                        states[child] = 1;
                        let child_source =
                            script_file_names(&instances[child].class_name).is_some();
                        stack.push((child, 0, child_source));
                    }
                    1 => *has_source |= script_file_names(&instances[child].class_name).is_some(),
                    _ => *has_source |= source_flags[child],
                }
                continue;
            }
            let (index, _, has_source) = stack.pop().expect("metric stack unexpectedly empty");
            source_flags[index] = has_source;
            states[index] = 2;
            if let Some((_, _, parent_source)) = stack.last_mut() {
                *parent_source |= has_source;
            }
        }
    }
    source_flags
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

fn derive_parent_path(path: &str) -> Option<String> {
    let last_dot = path.rfind('.')?;
    if last_dot == 0 {
        return None;
    }
    Some(path[..last_dot].to_string())
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

/// Scripts keep the file names and forms the project already has. The files
/// are looked for under `src_root`, or under `layout_src_root` when the import
/// writes into a stage that carries no copy of them.
fn import_service_tree(
    state: &ServiceState,
    project_root: &Path,
    src_root: &Path,
    layout_src_root: Option<&Path>,
    service: &str,
) -> Result<SourcemapNode> {
    let service_dir = src_root.join(sanitize_name(service));
    let layout_dir = layout_src_root.map_or_else(
        || service_dir.clone(),
        |root| root.join(sanitize_name(service)),
    );
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
        let emitting = profile::span(&format!("{service}: write script files"));
        let expected_batch = emit_children_indices(
            state,
            root_children,
            &service_dir,
            &layout_dir,
            &SourceOutput::Files {
                fresh: fresh_service_dir,
            },
            &visited,
        )?;
        expected_batch.merge_into(&expected_paths);
        drop(emitting);

        match settings_task.join() {
            Ok(result) => result?,
            Err(_) => bail!("{service}: settings write worker panicked"),
        }

        log_timing_ms(
            &format!("{service}: expected-path tracking"),
            expected_path_tracking_ms(&expected_paths),
        );

        if cleanup_required {
            let _cleaning = profile::span(&format!("{service}: remove files Studio dropped"));
            let cleanup_handle =
                spawn_cleanup_service_dir(service_dir.clone(), Arc::clone(&expected_paths));
            join_cleanup_handle(service, cleanup_handle)?;
        }

        let _mapping = profile::span(&format!("{service}: build sourcemap"));
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
            let mut files = expected_paths.files.lock_recover();
            for path in self.files {
                files.insert(path);
            }
        }
        if !self.dirs.is_empty() {
            let mut dirs = expected_paths.dirs.lock_recover();
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

pub(crate) fn is_import_stage_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".renium-import"))
    else {
        return false;
    };
    let Some((service, nonce)) = body.rsplit_once('.') else {
        return false;
    };
    let Some((pid, sequence)) = nonce.split_once('-') else {
        return false;
    };
    !service.is_empty()
        && !pid.is_empty()
        && !sequence.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
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
    // A cloned project can retain its store without an empty script folder.
    // Fresh scripts do not imply a fresh store or disposable instance IDs.
    let fresh_stage = fresh_stage && !settings_path.exists();
    if let Some(parent) = settings_path.parent() {
        fs::create_dir_all(parent)?;
    }
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
        // The file holds this state's own bytes, so the next pull that
        // captures the same state leaves it alone without decoding it.
        if let Ok(observed) = pulled_digest_from_service_state(state)
            && !observed.viewport_camera_lacks_enum_types
            && let Ok(bytes) = fs::read(&settings_path)
        {
            remember_pull(
                Sha256::digest(&bytes).into(),
                observed.digest,
                Some(observed.rows),
            );
        }
    } else {
        let lock_started = Instant::now();
        let _lock = acquire_settings_file_lock(&settings_path)?;
        log_timing(
            &format!("{service}: acquire settings file lock"),
            lock_started,
        );
        let preservation_started = Instant::now();
        let preserved_state =
            state_with_preserved_material_service_settings(service, state, &settings_path)?;
        log_timing(
            &format!("{service}: preserve material settings"),
            preservation_started,
        );
        let state_to_write = preserved_state.as_ref().unwrap_or(state);
        // A store whose file and observed state are the ones the last pull
        // found unchanged is unchanged again, and nothing is decoded for it.
        // One whose file holds an earlier state's own document, where only
        // values changed since, is compared in the changed instances alone.
        // For any other the captured state becomes a document while the
        // stored file is read, and the store is encoded once, after the two
        // are aligned.
        let alignment_started = Instant::now();
        let stored_file = (|| -> Result<Option<StoredFile>> {
            let _reading = profile::span(&format!("{service}: read stored file"));
            let bytes = match fs::read(&settings_path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to read {}", settings_path.display()));
                }
            };
            let digest = <[u8; 32]>::from(Sha256::digest(&bytes));
            let remembered = remembered_pull(&digest);
            Ok(Some((bytes, digest, remembered)))
        })();
        // A digest of the captured state says whether it is the state a pull
        // left this file alone with before. While nothing changed, no document
        // of the captured store is built.
        let pulled = matches!(&stored_file, Ok(Some(_)))
            .then(|| pulled_digest_from_service_state(state_to_write).ok())
            .flatten();
        let unchanged_again = matches!(
            (&stored_file, &pulled),
            (Ok(Some((_, _, Some(remembered)))), Some(pulled)) if remembered.digest == pulled.digest
        );
        // Where values changed since a pull left the file as a state's own
        // document, and where no pull remembers the file, the instances that
        // differ are compared alone. With the answer come the rows the file
        // goes on to hold, where it stays.
        let changed_values = match (&stored_file, &pulled) {
            (Ok(Some((bytes, _, remembered))), Some(pulled))
                if !unchanged_again && !pull_aligns_whole_documents() =>
            {
                let _comparing = profile::span(&format!("{service}: changed values"));
                // What the pull remembers of the file says which instances
                // changed where Studio still numbers them as it did; a file
                // it remembers nothing of, or a place Studio opened again,
                // is read for it.
                remembered
                    .as_ref()
                    .and_then(|remembered| remembered.rows.clone())
                    .and_then(|before| {
                        pull_changed_values(state_to_write, &before, &pulled.rows, bytes)
                            .ok()
                            .flatten()
                            .map(|alignment| (alignment, Some(before)))
                    })
                    .or_else(|| {
                        pull_changed_document_rows(state_to_write, &pulled.rows, bytes)
                            .ok()
                            .flatten()
                            .map(|(alignment, same)| (alignment, same.then(|| pulled.rows.clone())))
                    })
            }
            _ => None,
        };
        let stored_digest = match &stored_file {
            Ok(Some((_, digest, _))) => Some(*digest),
            _ => None,
        };
        let (stored, observed) = if unchanged_again || changed_values.is_some() {
            (Ok(None), Ok(None))
        } else {
            rayon::join(
                || -> Result<Option<StoredSettings>> {
                    let Some((bytes, digest, _)) = stored_file? else {
                        return Ok(None);
                    };
                    Ok(Some(StoredSettings {
                        digest,
                        document: decode_stored_settings(&settings_path, &bytes)?,
                    }))
                },
                || {
                    let _converting = profile::span(&format!("{service}: captured store document"));
                    Ok::<_, anyhow::Error>(Some(settings_bytecode_from_service_state(
                        state_to_write,
                    )?))
                },
            )
        };
        let aligned = (|| -> Result<Option<Vec<u8>>> {
            if unchanged_again {
                profile::record(
                    &format!("{service}: unchanged since the last pull"),
                    alignment_started,
                );
                return Ok(None);
            }
            if let (Some((alignment, rows)), Some(stored), Some(pulled)) =
                (changed_values, stored_digest, &pulled)
            {
                return Ok(match alignment {
                    SettingsAlignment::Equivalent => {
                        remember_pull(stored, pulled.digest, rows);
                        None
                    }
                    SettingsAlignment::Changed(written) => {
                        forget_pull(&stored);
                        remember_pull(
                            Sha256::digest(&written).into(),
                            pulled.digest,
                            Some(pulled.rows.clone()),
                        );
                        Some(written)
                    }
                });
            }
            #[cfg(test)]
            WHOLLY_ALIGNED.lock_recover().push(settings_path.clone());
            // A captured state the decoder would refuse goes the way it went
            // before: its own bytes are written or compared as they are.
            let Ok(Some(observed)) = observed else {
                return pull_from_bytes(state_to_write, &settings_path);
            };
            Ok(match stored? {
                Some(stored) => {
                    let reference = stored.document;
                    let answer_follows_camera = viewport_camera_lacks_enum_types(&reference);
                    let pulled_alignment = align_pulled_settings_documents_to_reference(
                        reference,
                        observed,
                        &|| encode_service_settings_binary(state_to_write),
                        alignment_started,
                    )
                    .with_context(|| format!("Failed to align {}", settings_path.display()))?;
                    // The rows are those of the file only where it holds the
                    // state's document as the state itself encodes it.
                    let rows = |pulled: &PulledDigest| {
                        (pulled_alignment.stored_as_observed
                            && pulled.rows.encodes_as_its_document())
                        .then(|| pulled.rows.clone())
                    };
                    match pulled_alignment.alignment {
                        SettingsAlignment::Equivalent => {
                            if !answer_follows_camera && let Some(pulled) = &pulled {
                                remember_pull(stored.digest, pulled.digest, rows(pulled));
                            }
                            None
                        }
                        SettingsAlignment::Changed(aligned) => {
                            forget_pull(&stored.digest);
                            // A file that holds the captured state's own
                            // document is left alone by a pull of that state.
                            if !answer_follows_camera
                                && let Some(pulled) = &pulled
                                && let Some(rows) = rows(pulled)
                            {
                                remember_pull(
                                    Sha256::digest(&aligned).into(),
                                    pulled.digest,
                                    Some(rows),
                                );
                            }
                            Some(aligned)
                        }
                    }
                }
                // A store that is not there yet gets the captured state's own bytes.
                None => Some(encode_service_settings_binary(state_to_write)?),
            })
        })();
        if cfg!(debug_assertions) {
            assert_pull_matches_the_byte_path(state_to_write, &settings_path, &aligned);
        }
        let aligned = aligned?;
        log_timing(
            &format!("{service}: align settings file"),
            alignment_started,
        );
        let publish_started = Instant::now();
        if let Some(aligned) = aligned {
            write_bytes_if_changed_in_existing_dir(&settings_path, &aligned)?;
        }
        log_timing(
            &format!("{service}: publish settings file"),
            publish_started,
        );
    }
    log_timing(&format!("{service}: write settings file"), started);
    Ok(())
}

/// The stores whose pull aligned whole documents, for the tests of the pulls
/// that need not.
#[cfg(test)]
static WHOLLY_ALIGNED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A stored file's bytes, their digest, and what a pull remembers of them.
type StoredFile = (Vec<u8>, [u8; 32], Option<RememberedPull>);

/// A store file as a pull read it: the digest of its bytes and its document.
struct StoredSettings {
    digest: [u8; 32],
    document: SettingsBytecode,
}

fn decode_stored_settings(settings_path: &Path, bytes: &[u8]) -> Result<SettingsBytecode> {
    decode_settings_bytecode_holding(bytes)
        .with_context(|| format!("Failed to align {}", settings_path.display()))
}

/// For a stored file a pull found unchanged, by the digest of its bytes: the
/// digest of the observed store it was compared with. The alignment answers
/// from those two alone, wherever the file lies (a pull works on a staged
/// copy of the project), so the same two again are unchanged again.
fn remembered_pulls() -> &'static Mutex<RememberedPulls> {
    static PULLS: OnceLock<Mutex<RememberedPulls>> = OnceLock::new();
    PULLS.get_or_init(Mutex::default)
}

type RememberedPulls = HashMap<[u8; 32], RememberedPull>;

#[derive(Clone)]
struct RememberedPull {
    digest: [u8; 32],
    /// The digest's parts for each instance of a state whose own document the
    /// file holds: a later state is compared with them instance by instance.
    rows: Option<Arc<PulledRows>>,
}

const MAX_REMEMBERED_PULLS: usize = 1_024;
/// 32 bytes are kept for each instance.
const MAX_REMEMBERED_ROWS: usize = 4_000_000;

fn remember_pull(stored: [u8; 32], observed: [u8; 32], rows: Option<Arc<PulledRows>>) {
    remember_pull_within(
        &mut remembered_pulls().lock_recover(),
        (stored, observed),
        rows,
        (MAX_REMEMBERED_PULLS, MAX_REMEMBERED_ROWS),
    );
}

/// Remembers the pull of one file, within `(pulls, rows)` at most: past the
/// first every pull is forgotten, past the second the rows of every other
/// file, and rows that alone are past it are not kept.
fn remember_pull_within(
    pulls: &mut RememberedPulls,
    (stored, observed): ([u8; 32], [u8; 32]),
    rows: Option<Arc<PulledRows>>,
    (most_pulls, most_rows): (usize, usize),
) {
    if pulls.len() >= most_pulls && !pulls.contains_key(&stored) {
        pulls.clear();
    }
    let rows = rows.filter(|rows| rows.len() <= most_rows);
    let held = pulls
        .iter()
        .filter(|(file, _)| **file != stored)
        .filter_map(|(_, pull)| pull.rows.as_ref())
        .map(|rows| rows.len())
        .sum::<usize>();
    if rows
        .as_ref()
        .is_some_and(|rows| held + rows.len() > most_rows)
    {
        for pull in pulls.values_mut() {
            pull.rows = None;
        }
    }
    pulls.insert(
        stored,
        RememberedPull {
            digest: observed,
            rows,
        },
    );
}

/// What a pull remembers of a file, read and let go at once: a caller that
/// goes on to digest a state does so on the pool, where a thread that waits
/// takes up another service's work, which must find the memory free.
fn remembered_pull(stored: &[u8; 32]) -> Option<RememberedPull> {
    remembered_pulls().lock_recover().get(stored).cloned()
}

/// A file a pull replaced is not read again.
fn forget_pull(stored: &[u8; 32]) {
    remembered_pulls().lock_recover().remove(stored);
}

/// `RENIUM_PULL_WHOLE_DOCUMENTS` makes every changed store take the whole
/// alignment, to measure it against the comparison of changed instances.
fn pull_aligns_whole_documents() -> bool {
    static WHOLE: OnceLock<bool> = OnceLock::new();
    *WHOLE.get_or_init(|| std::env::var_os("RENIUM_PULL_WHOLE_DOCUMENTS").is_some())
}

/// The pull as it worked before it aligned documents: the captured state is
/// encoded, and its bytes are aligned with the stored file's. A captured
/// state the decoder refuses still takes this way, and every pull of a debug
/// build is checked against it.
fn pull_from_bytes(state: &ServiceState, settings_path: &Path) -> Result<Option<Vec<u8>>> {
    let observed = encode_service_settings_binary(state)?;
    Ok(match fs::read(settings_path) {
        Ok(reference) => {
            match align_pulled_settings_bytes_to_reference(&reference, &observed)
                .with_context(|| format!("Failed to align {}", settings_path.display()))?
            {
                SettingsAlignment::Equivalent => None,
                SettingsAlignment::Changed(aligned) => Some(aligned),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(observed),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read {}", settings_path.display()));
        }
    })
}

/// What the pull is about to write, or that it fails, is what
/// `pull_from_bytes` gives.
fn assert_pull_matches_the_byte_path(
    state: &ServiceState,
    settings_path: &Path,
    aligned: &Result<Option<Vec<u8>>>,
) {
    let expected = pull_from_bytes(state, settings_path);
    let same = match (&expected, aligned) {
        (Ok(expected), Ok(aligned)) => expected == aligned,
        (Err(_), Err(_)) => true,
        _ => false,
    };
    assert!(
        same,
        "{}: the pull from documents differs from the pull from bytes (bytes: {}, documents: {})",
        settings_path.display(),
        expected
            .as_ref()
            .map_or_else(|error| format!("{error:#}"), |_| "ok".into()),
        aligned
            .as_ref()
            .map_or_else(|error| format!("{error:#}"), |_| "ok".into()),
    );
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
        let guard = expected_paths.files.lock_recover();
        guard.clone()
    };
    let expected_dirs = {
        let guard = expected_paths.dirs.lock_recover();
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
    remove_stale_import_paths(service_dir, &stale_files, &stale_dirs)?;
    log_timing(
        &format!("cleanup delete {}", service_dir.display()),
        delete_started,
    );

    Ok(())
}

pub(crate) fn remove_stale_import_paths(
    service_dir: &Path,
    stale_files: &[PathBuf],
    stale_dirs: &[PathBuf],
) -> Result<()> {
    if stale_files.is_empty() && stale_dirs.is_empty() {
        return Ok(());
    }

    let mut ordered_dirs = stale_dirs.to_vec();
    ordered_dirs.sort_by_key(|path| path.components().count());
    let mut root_dirs: Vec<PathBuf> = Vec::new();
    for dir in ordered_dirs {
        let relative = dir.strip_prefix(service_dir).with_context(|| {
            format!(
                "Stale import directory escaped service root: {}",
                dir.display()
            )
        })?;
        if relative.as_os_str().is_empty() {
            bail!("Refusing to delete import service root {}", dir.display());
        }
        if !root_dirs.iter().any(|root| dir.starts_with(root)) {
            root_dirs.push(dir);
        }
    }

    for file in stale_files {
        let relative = file.strip_prefix(service_dir).with_context(|| {
            format!("Stale import file escaped service root: {}", file.display())
        })?;
        if relative.as_os_str().is_empty() {
            bail!("Refusing to delete import service root {}", file.display());
        }
    }

    for dir in &root_dirs {
        if dir.exists() {
            fs::remove_dir_all(dir).with_context(|| {
                format!("Failed to delete stale import directory {}", dir.display())
            })?;
        }
    }

    for file in stale_files {
        if root_dirs.iter().any(|root| file.starts_with(root)) || !file.exists() {
            continue;
        }
        fs::remove_file(file)
            .with_context(|| format!("Failed to delete stale import file {}", file.display()))?;
    }
    Ok(())
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

enum SourceOutput {
    Files { fresh: bool },
    Memory(Mutex<BTreeMap<PathBuf, Option<Vec<u8>>>>),
}

impl SourceOutput {
    fn directory(&self, path: &Path) -> Result<()> {
        match self {
            Self::Files { .. } => fs::create_dir_all(path)
                .with_context(|| format!("Failed to create {}", path.display())),
            Self::Memory(entries) => {
                let mut entries = entries.lock_recover();
                let entry = entries.entry(path.to_owned()).or_insert(None);
                anyhow::ensure!(
                    entry.is_none(),
                    "Projected directory collides with a file: {}",
                    path.display()
                );
                Ok(())
            }
        }
    }

    fn source(&self, path: &Path, source: &str) -> Result<()> {
        match self {
            Self::Files { fresh } => write_script_source_file(path, source, *fresh),
            Self::Memory(entries) => {
                // Verification cannot manufacture an empty script for missing Source.
                anyhow::ensure!(
                    source != EXTERNAL_SOURCE_MARKER,
                    "Missing fetched Source for {}",
                    path.display()
                );
                let previous = entries
                    .lock_recover()
                    .insert(path.to_owned(), Some(source.as_bytes().to_vec()));
                anyhow::ensure!(
                    previous.is_none(),
                    "Projected source path already exists: {}",
                    path.display()
                );
                Ok(())
            }
        }
    }
}

/// The store of a captured state as a comparison with the project reads it:
/// the project's own file where a pull would leave that file alone for this
/// state (its own document, or one the state differs from by nothing a pull
/// counts as a change), so that nothing is encoded or decoded to find the
/// two alike, and the state's encoding anywhere else. A pull keeps the
/// stored viewport camera, and so does this.
fn captured_store_bytes(state: &ServiceState, stored: &Path) -> Result<Vec<u8>> {
    if let Ok(bytes) = fs::read(stored) {
        let remembered = remembered_pull(&Sha256::digest(&bytes).into());
        if remembered.is_some_and(|remembered| {
            remembered.rows.is_some()
                && pulled_digest_from_service_state(state)
                    .is_ok_and(|pulled| pulled.digest == remembered.digest)
        }) {
            return Ok(bytes);
        }
    }
    encode_service_settings_binary(state)
}

/// Project the same settings, script names and directories as a fresh import,
/// without touching disk or constructing the full editor sourcemap. Existing
/// script files are looked for under `layout_src_root` (or `src_root`).
pub(crate) fn service_projection_in_memory(
    state: &ServiceState,
    src_root: &Path,
    layout_src_root: Option<&Path>,
    service: &str,
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let service_dir = src_root.join(sanitize_name(service));
    let layout_dir = layout_src_root.map_or_else(
        || service_dir.clone(),
        |root| root.join(sanitize_name(service)),
    );
    let settings_path = service_settings_path(&service_dir);
    let output = SourceOutput::Memory(Mutex::new(BTreeMap::new()));
    output.directory(&service_dir)?;
    let (settings, sources) = rayon::join(
        || captured_store_bytes(state, &service_settings_path(&layout_dir)),
        || {
            let visited = (0..state.instances.len())
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>();
            mark_visited(&visited, state.service_root_index);
            emit_children_indices(
                state,
                child_indices_for_instance(state, state.service_root_index),
                &service_dir,
                &layout_dir,
                &output,
                &visited,
            )
        },
    );
    sources?;
    let SourceOutput::Memory(entries) = output else {
        unreachable!()
    };
    let mut entries = entries.into_inner().unwrap_or_else(PoisonError::into_inner);
    entries.insert(settings_path, Some(settings?));
    Ok(entries)
}

fn emit_node_index(
    state: &ServiceState,
    index: usize,
    parent_dir: &Path,
    layout_dir: &Path,
    fs_stem: &str,
    output: &SourceOutput,
    visited: &[AtomicBool],
) -> Result<ExpectedPathBatch> {
    if !mark_visited(visited, index) {
        return Ok(ExpectedPathBatch::default());
    }

    let instance = &state.instances[index];
    let child_indices = child_indices_for_instance(state, index);
    let has_children = !child_indices.is_empty();
    let class_name = instance.class_name.as_str();
    let mut expected = ExpectedPathBatch::default();

    if let Some(names) = project_script_file_names_in_layout(
        parent_dir,
        layout_dir,
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

        if names.directory {
            let dir_path = parent_dir.join(fs_stem);
            output.directory(&dir_path)?;
            expected.track_dir(&dir_path);
            let source_path = dir_path.join(names.source_file_name);
            output.source(&source_path, source)?;
            expected.track_file(&source_path);

            let child_expected = emit_children_indices(
                state,
                child_indices,
                &dir_path,
                &layout_dir.join(fs_stem),
                output,
                visited,
            )?;
            expected.extend(child_expected);
            return Ok(expected);
        }

        let script_path = parent_dir.join(format!("{fs_stem}{}", names.leaf_suffix));
        output.source(&script_path, source)?;
        expected.track_file(&script_path);

        return Ok(expected);
    }

    if !state.source_in_subtree.get(index).copied().unwrap_or(false) {
        return Ok(expected);
    }

    let dir_path = parent_dir.join(fs_stem);
    output.directory(&dir_path)?;
    expected.track_dir(&dir_path);

    let child_expected = emit_children_indices(
        state,
        child_indices,
        &dir_path,
        &layout_dir.join(fs_stem),
        output,
        visited,
    )?;
    expected.extend(child_expected);

    Ok(expected)
}

fn emit_children_indices(
    state: &ServiceState,
    child_indices: &[usize],
    dir_path: &Path,
    layout_dir: &Path,
    output: &SourceOutput,
    visited: &[AtomicBool],
) -> Result<ExpectedPathBatch> {
    let named_children = name_child_indices(state, child_indices);

    const PARALLEL_CHILD_THRESHOLD: usize = 8;
    if named_children.len() >= PARALLEL_CHILD_THRESHOLD && rayon::current_num_threads() > 1 {
        let built = named_children
            .par_iter()
            .map(|(child_index, child_stem)| {
                emit_node_index(
                    state,
                    *child_index,
                    dir_path,
                    layout_dir,
                    child_stem,
                    output,
                    visited,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let mut expected = ExpectedPathBatch::default();
        for child_expected in built {
            expected.extend(child_expected);
        }
        Ok(expected)
    } else {
        let mut expected = ExpectedPathBatch::default();
        for (child_index, child_stem) in named_children {
            let child_expected = emit_node_index(
                state,
                child_index,
                dir_path,
                layout_dir,
                &child_stem,
                output,
                visited,
            )?;
            expected.extend(child_expected);
        }
        Ok(expected)
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
    if source == EXTERNAL_SOURCE_MARKER {
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

#[cfg(test)]
mod projection_tests {
    use super::*;

    /// A store a first pull wrote is remembered with the state it was written
    /// from: pulling that state again decodes nothing and changes nothing.
    #[test]
    fn a_freshly_written_store_is_left_alone_by_the_next_pull_of_its_state() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-fresh-store-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "fresh-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let mut instances = vec![SnapshotInstance {
            name: "ReplicatedStorage".into(),
            class_name: "ReplicatedStorage".into(),
            instance_index: Some(1),
            ..Default::default()
        }];
        for index in 0..40 {
            instances.push(SnapshotInstance {
                name: format!("Value{index}"),
                class_name: "NumberValue".into(),
                instance_index: Some(index + 2),
                parent_index: Some(1),
                properties: Map::from_iter([("Value".into(), json!(index))]),
                ..Default::default()
            });
        }
        let state = build_service_state_from_instances(
            "ReplicatedStorage",
            None,
            instances,
            HashMap::new(),
            true,
        )?;
        import_service_tree(&state, &root, &source, None, "ReplicatedStorage")?;
        let store = service_settings_path(&source.join("ReplicatedStorage"));
        let written = fs::read(&store)?;
        let digest = <[u8; 32]>::from(Sha256::digest(&written));
        assert_eq!(
            remembered_pull(&digest).map(|pull| pull.digest),
            Some(pulled_digest_from_service_state(&state)?.digest)
        );
        import_service_tree(&state, &root, &source, None, "ReplicatedStorage")?;
        assert!(fs::read(&store)? == written);
        Ok(())
    }

    fn workspace_state(camera: Map<String, Value>, transparency: f64) -> Result<ServiceState> {
        workspace_state_of(camera, transparency, 40)
    }

    fn workspace_state_of(
        camera: Map<String, Value>,
        transparency: f64,
        parts: usize,
    ) -> Result<ServiceState> {
        let mut instances = vec![
            SnapshotInstance {
                name: "Workspace".into(),
                class_name: "Workspace".into(),
                instance_index: Some(1),
                properties: Map::from_iter([(
                    "CurrentCamera".into(),
                    json!({"_type":"Ref","instanceIndex":2}),
                )]),
                ..Default::default()
            },
            SnapshotInstance {
                name: "Camera".into(),
                class_name: "Camera".into(),
                instance_index: Some(2),
                parent_index: Some(1),
                properties: camera,
                ..Default::default()
            },
        ];
        for index in 0..parts {
            instances.push(SnapshotInstance {
                name: format!("Part{index}"),
                class_name: "Part".into(),
                instance_index: Some(index + 3),
                parent_index: Some(1),
                properties: Map::from_iter([(
                    "Transparency".into(),
                    json!(if index == 7 { transparency } else { 0.5 }),
                )]),
                ..Default::default()
            });
        }
        build_service_state_from_instances("Workspace", None, instances, HashMap::new(), true)
    }

    /// A pull after a value changed in Studio compares the changed instance
    /// alone: no whole documents are aligned, the stored camera stays, and
    /// the store it writes is remembered like one a first pull wrote.
    #[test]
    fn a_changed_value_is_pulled_without_aligning_the_whole_store() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-changed-value-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "changed-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let camera = |field_of_view: f64| {
            Map::from_iter([("FieldOfView".to_string(), json!(field_of_view))])
        };
        let store = service_settings_path(&source.join("Workspace"));
        import_service_tree(
            &workspace_state(camera(70.0), 0.5)?,
            &root,
            &source,
            None,
            "Workspace",
        )?;
        let first = fs::read(&store)?;
        for (round, transparency) in [0.25, 0.75, 0.75].into_iter().enumerate() {
            let before = fs::read(&store)?;
            let state = workspace_state(camera(40.0 + round as f64), transparency)?;
            import_service_tree(&state, &root, &source, None, "Workspace")?;
            let written = fs::read(&store)?;
            assert_eq!(written != before, round < 2, "round {round}");
            let document = crate::settings::bytecode::decode_settings_bytecode(&written)?;
            assert_eq!(
                document.instances[9].properties["Transparency"],
                json!(transparency)
            );
            assert_eq!(document.instances[1].properties["FieldOfView"], json!(70.0));
            let remembered = remembered_pull(&Sha256::digest(&written).into())
                .expect("the written store is remembered");
            assert!(remembered.digest == pulled_digest_from_service_state(&state)?.digest);
            assert!(remembered.rows.is_some());
        }
        assert!(first != fs::read(&store)?);
        assert!(!WHOLLY_ALIGNED.lock_recover().contains(&store));
        Ok(())
    }

    /// A Workspace as one Studio session holds it: the session numbers the
    /// instances and gives each its UniqueId.
    fn session_state(
        session: usize,
        field_of_view: f64,
        transparency: f64,
    ) -> Result<ServiceState> {
        let unique_id = |index: usize| json!({"_type": "UniqueId", "value": format!("{session:024x}{index:08x}")});
        let mut instances = vec![
            SnapshotInstance {
                name: "Workspace".into(),
                class_name: "Workspace".into(),
                instance_index: Some(1),
                properties: Map::from_iter([(
                    "CurrentCamera".into(),
                    json!({"_type":"Ref","instanceIndex":2}),
                )]),
                ..Default::default()
            },
            SnapshotInstance {
                name: "Camera".into(),
                class_name: "Camera".into(),
                instance_index: Some(2),
                parent_index: Some(1),
                debug_id: Some(format!("0_{}", session * 1_000 + 2)),
                properties: Map::from_iter([
                    ("FieldOfView".to_string(), json!(field_of_view)),
                    ("UniqueId".to_string(), unique_id(2)),
                ]),
                ..Default::default()
            },
        ];
        for index in 0..24 {
            instances.push(SnapshotInstance {
                // Half of them are namesakes of one another.
                name: if index % 2 == 0 {
                    "Part".into()
                } else {
                    format!("Part{index}")
                },
                class_name: "Part".into(),
                instance_index: Some(index + 3),
                parent_index: Some(1),
                debug_id: Some(format!("0_{}", session * 1_000 + index + 3)),
                properties: Map::from_iter([
                    (
                        "Transparency".to_string(),
                        json!(if index == 7 { transparency } else { 0.5 }),
                    ),
                    ("UniqueId".to_string(), unique_id(index + 3)),
                ]),
                ..Default::default()
            });
        }
        build_service_state_from_instances("Workspace", None, instances, HashMap::new(), true)
    }

    /// A place Studio opened again carries other ids and other UniqueIds than
    /// its store. The pull reads the instances that differ and no whole
    /// document: the store keeps the ids it holds its instances under and
    /// its camera, takes the new UniqueIds and values, and is the stored file
    /// to a comparison with the project from then on.
    #[test]
    fn a_place_studio_opened_again_is_pulled_without_aligning_the_whole_store() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-reopened-place-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "reopened-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let store = service_settings_path(&source.join("Workspace"));
        import_service_tree(
            &session_state(1, 70.0, 0.5)?,
            &root,
            &source,
            None,
            "Workspace",
        )?;
        let first = crate::settings::bytecode::decode_settings_bytecode(&fs::read(&store)?)?;
        let ids = |document: &SettingsBytecode| {
            document
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
                .collect::<Vec<_>>()
        };
        // The same place in a second session, its camera moved; then a value
        // changes in that session; then nothing does.
        for (round, transparency) in [0.5, 0.125, 0.125].into_iter().enumerate() {
            let before = fs::read(&store)?;
            let state = session_state(2, 40.0, transparency)?;
            import_service_tree(&state, &root, &source, None, "Workspace")?;
            let written = fs::read(&store)?;
            assert_eq!(written != before, round < 2, "round {round}");
            let document = crate::settings::bytecode::decode_settings_bytecode(&written)?;
            assert_eq!(ids(&document), ids(&first), "round {round}");
            assert_eq!(
                document.instances[1].properties,
                first.instances[1].properties
            );
            assert_eq!(
                document.instances[9].properties["Transparency"],
                json!(transparency)
            );
            assert_eq!(
                document.instances[9].properties["UniqueId"]["value"],
                json!(format!("{:024x}{:08x}", 2, 10))
            );
            let projected = service_projection_in_memory(&state, &source, None, "Workspace")?
                .remove(&store)
                .flatten()
                .expect("the projection holds the store");
            assert!(projected == written, "round {round}");
        }
        assert!(!WHOLLY_ALIGNED.lock_recover().contains(&store));
        Ok(())
    }

    /// A Workspace whose parts stand under a folder and point at one
    /// another, under ids of Studio's and without UniqueIds: nothing tells
    /// its instances apart but where they stand. With `new`, one more part
    /// stands before the folder.
    fn grouped_state(new: bool) -> Result<ServiceState> {
        let shift = usize::from(new);
        let mut instances = vec![
            SnapshotInstance {
                name: "Workspace".into(),
                class_name: "Workspace".into(),
                instance_index: Some(1),
                properties: Map::from_iter([(
                    "CurrentCamera".into(),
                    json!({"_type":"Ref","instanceIndex":2}),
                )]),
                ..Default::default()
            },
            SnapshotInstance {
                name: "Camera".into(),
                class_name: "Camera".into(),
                instance_index: Some(2),
                parent_index: Some(1),
                debug_id: Some("0_902".into()),
                properties: Map::from_iter([("FieldOfView".to_string(), json!(70.0))]),
                ..Default::default()
            },
        ];
        if new {
            instances.push(SnapshotInstance {
                name: "New".into(),
                class_name: "Part".into(),
                instance_index: Some(3),
                parent_index: Some(1),
                debug_id: Some("0_990".into()),
                ..Default::default()
            });
        }
        let folder = 3 + shift;
        instances.push(SnapshotInstance {
            name: "Group".into(),
            class_name: "Folder".into(),
            instance_index: Some(folder),
            parent_index: Some(1),
            debug_id: Some("0_903".into()),
            ..Default::default()
        });
        for index in 0..12 {
            instances.push(SnapshotInstance {
                name: format!("Part{index}"),
                class_name: "Part".into(),
                instance_index: Some(folder + 1 + index),
                parent_index: Some(folder),
                debug_id: Some(format!("0_{}", 910 + index)),
                properties: Map::from_iter([
                    ("Transparency".to_string(), json!(0.5)),
                    (
                        "Target".to_string(),
                        json!({"_type":"Ref","instanceIndex": folder + 1 + (index + 1) % 12}),
                    ),
                ]),
                ..Default::default()
            });
        }
        build_service_state_from_instances("Workspace", None, instances, HashMap::new(), true)
    }

    /// A new instance is pulled without aligning the whole store: every
    /// instance past it keeps the id it is stored under, whatever place it
    /// and the instances it points at moved to.
    #[test]
    fn a_new_instance_is_pulled_without_aligning_the_whole_store() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-new-instance-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "grown-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let store = service_settings_path(&source.join("Workspace"));
        import_service_tree(&grouped_state(false)?, &root, &source, None, "Workspace")?;
        let first = crate::settings::bytecode::decode_settings_bytecode(&fs::read(&store)?)?;
        import_service_tree(&grouped_state(true)?, &root, &source, None, "Workspace")?;
        let grown = crate::settings::bytecode::decode_settings_bytecode(&fs::read(&store)?)?;
        assert_eq!(grown.instances.len(), first.instances.len() + 1);
        assert_eq!(grown.instances[2].name, "New");
        for (index, stored) in first.instances.iter().enumerate() {
            let now = &grown.instances[index + usize::from(index >= 2)];
            assert_eq!(
                (&now.settings_id, &now.name),
                (&stored.settings_id, &stored.name)
            );
        }
        assert_eq!(
            grown.instances[4].properties["Target"],
            json!({"_type":"Ref","instanceIndex":6})
        );
        // And gone again, the store is the first one.
        import_service_tree(&grouped_state(false)?, &root, &source, None, "Workspace")?;
        assert!(
            crate::settings::bytecode::decode_settings_bytecode(&fs::read(&store)?)?.instances
                == first.instances
        );
        assert!(!WHOLLY_ALIGNED.lock_recover().contains(&store));
        Ok(())
    }

    /// A capture compared with the project is the project's own file while
    /// Studio holds what a pull left there, whatever its camera does, and the
    /// state's encoding once a value changed.
    #[test]
    fn a_capture_of_what_a_pull_left_is_the_stored_file() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-captured-store-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "captured-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let camera = |field_of_view: f64| {
            Map::from_iter([("FieldOfView".to_string(), json!(field_of_view))])
        };
        // Values of this test's own: what a pull remembers goes by the
        // bytes of the store, which another test's store must not share.
        let pulled = workspace_state(camera(71.0), 0.375)?;
        import_service_tree(&pulled, &root, &source, None, "Workspace")?;
        let store = service_settings_path(&source.join("Workspace"));
        let stored = fs::read(&store)?;
        let settings = |state: &ServiceState| -> Result<Vec<u8>> {
            Ok(
                service_projection_in_memory(state, &source, None, "Workspace")?
                    .remove(&store)
                    .flatten()
                    .expect("the projection holds the store"),
            )
        };
        let moved = workspace_state(camera(41.0), 0.375)?;
        assert!(encode_service_settings_binary(&moved)? != stored);
        assert!(settings(&moved)? == stored);
        let changed = workspace_state(camera(41.0), 0.625)?;
        assert!(settings(&changed)? == encode_service_settings_binary(&changed)?);
        fs::write(&store, encode_service_settings_binary(&changed)?)?;
        assert!(settings(&moved)? == encode_service_settings_binary(&moved)?);
        Ok(())
    }

    /// Captures of many services are compared at once, each from a thread of
    /// its own and each digesting its state on the shared pool: none of them
    /// may wait for another while it holds what the pulls remember.
    #[test]
    fn captures_of_many_services_are_compared_at_once() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-captured-stores-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "captured-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let services = (0..8)
            .map(|index| {
                let source = root.join(format!("src{index}"));
                let state = workspace_state_of(
                    Map::from_iter([("FieldOfView".to_string(), json!(50.0))]),
                    0.0625 + index as f64 / 64.0,
                    6_000,
                )?;
                import_service_tree(&state, &root, &source, None, "Workspace")?;
                Ok((Arc::new(state), source))
            })
            .collect::<Result<Vec<_>>>()?;
        // More comparisons wait for the pool than it has threads, so a
        // thread that waits inside one digest takes up the next comparison.
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(2)
                .build()
                .unwrap(),
        );
        let (done, finished) = mpsc::channel();
        let waiting = services.len() * 2;
        for (state, source) in services.into_iter().cycle().take(waiting) {
            let (done, pool) = (done.clone(), pool.clone());
            thread::spawn(move || {
                let store = service_settings_path(&source.join("Workspace"));
                let alike = (0..10).all(|_| {
                    pool.install(|| captured_store_bytes(&state, &store)).ok()
                        == fs::read(&store).ok()
                });
                let _ = done.send(alike);
            });
        }
        for _ in 0..waiting {
            assert_eq!(
                finished.recv_timeout(std::time::Duration::from_secs(90)),
                Ok(true),
                "the comparisons wait for one another"
            );
        }
        Ok(())
    }

    /// What the pulls remember is bound: past the bound on rows the other
    /// files' rows go and their digests stay, rows past it alone are not
    /// kept, and past the bound on pulls everything goes.
    #[test]
    fn what_the_pulls_remember_is_bound() -> Result<()> {
        let rows = |parts: usize| -> Result<Arc<PulledRows>> {
            let state = workspace_state_of(Map::new(), 0.5, parts)?;
            Ok(pulled_digest_from_service_state(&state)?.rows)
        };
        let (small, large) = (rows(8)?, rows(28)?);
        assert_eq!((small.len(), large.len()), (10, 30));
        let mut pulls = RememberedPulls::new();
        let file = |number: u8| [number; 32];
        let held = |pulls: &RememberedPulls, number: u8| {
            pulls.get(&file(number)).map(|pull| pull.rows.is_some())
        };
        remember_pull_within(&mut pulls, (file(1), file(9)), Some(small.clone()), (3, 30));
        remember_pull_within(&mut pulls, (file(2), file(9)), Some(small.clone()), (3, 30));
        assert_eq!((held(&pulls, 1), held(&pulls, 2)), (Some(true), Some(true)));
        // The same file again counts once.
        remember_pull_within(&mut pulls, (file(2), file(8)), Some(small.clone()), (3, 30));
        assert_eq!((held(&pulls, 1), held(&pulls, 2)), (Some(true), Some(true)));
        assert!(pulls[&file(2)].digest == file(8));
        remember_pull_within(&mut pulls, (file(3), file(9)), Some(large.clone()), (3, 30));
        assert_eq!(
            (held(&pulls, 1), held(&pulls, 2), held(&pulls, 3)),
            (Some(false), Some(false), Some(true))
        );
        remember_pull_within(&mut pulls, (file(3), file(9)), Some(large), (3, 29));
        assert_eq!(held(&pulls, 3), Some(false));
        remember_pull_within(&mut pulls, (file(4), file(9)), None, (3, 30));
        assert_eq!(pulls.len(), 1);
        assert_eq!(held(&pulls, 4), Some(false));
        Ok(())
    }

    /// A label whose text is scaled and not wrapped is written wrapped by
    /// the encoder of documents alone, so a pull after a change elsewhere
    /// aligns the whole store, and answers what a pull from bytes answers.
    #[test]
    fn a_store_the_two_encoders_write_apart_is_aligned_whole() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-forced-property-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "forced-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let state = |transparency: f64| -> Result<ServiceState> {
            let mut state = workspace_state_of(
                Map::from_iter([("FieldOfView".to_string(), json!(33.0))]),
                transparency,
                12,
            )?;
            let label = &mut state.instances[5];
            label.class_name = "TextLabel".into();
            label.properties.insert("TextScaled".into(), json!(true));
            label.properties.insert("TextWrapped".into(), json!(false));
            Ok(state)
        };
        let store = service_settings_path(&source.join("Workspace"));
        import_service_tree(&state(0.125)?, &root, &source, None, "Workspace")?;
        assert!(
            !pulled_digest_from_service_state(&state(0.125)?)?
                .rows
                .encodes_as_its_document()
        );
        import_service_tree(&state(0.875)?, &root, &source, None, "Workspace")?;
        let bytes = fs::read(&store)?;
        // The file is not the state's own encoding, so no rows go with it.
        assert!(
            remembered_pull(&Sha256::digest(&bytes).into()).is_none_or(|pull| pull.rows.is_none())
        );
        let written = crate::settings::bytecode::decode_settings_bytecode(&bytes)?;
        assert_eq!(written.instances[5].properties["TextWrapped"], json!(true));
        assert_eq!(
            written.instances[9].properties["Transparency"],
            json!(0.875)
        );
        assert!(WHOLLY_ALIGNED.lock_recover().contains(&store));
        Ok(())
    }

    /// Studio's viewport moved to another camera of a store no pull
    /// remembers: the root names it, so the whole store is aligned and the
    /// file is rewritten, as a pull from bytes rewrites it.
    #[test]
    fn a_viewport_that_moved_to_another_camera_is_aligned_whole() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-viewport-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "viewport-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let state = |current: usize, field_of_view: f64| {
            let camera = |name: &str, index: usize, field_of_view: f64| SnapshotInstance {
                name: name.into(),
                class_name: "Camera".into(),
                instance_index: Some(index),
                parent_index: Some(1),
                properties: Map::from_iter([("FieldOfView".into(), json!(field_of_view))]),
                ..Default::default()
            };
            build_service_state_from_instances(
                "Workspace",
                None,
                vec![
                    SnapshotInstance {
                        name: "Workspace".into(),
                        class_name: "Workspace".into(),
                        instance_index: Some(1),
                        properties: Map::from_iter([(
                            "CurrentCamera".into(),
                            json!({"_type":"Ref","instanceIndex":current}),
                        )]),
                        ..Default::default()
                    },
                    camera("Camera", 2, 73.0),
                    camera("Other", 3, field_of_view),
                ],
                HashMap::new(),
                true,
            )
        };
        import_service_tree(&state(2, 53.0)?, &root, &source, None, "Workspace")?;
        let store = service_settings_path(&source.join("Workspace"));
        let stored = fs::read(&store)?;
        forget_pull(&Sha256::digest(&stored).into());
        import_service_tree(&state(3, 47.0)?, &root, &source, None, "Workspace")?;
        assert!(fs::read(&store)? != stored);
        assert!(WHOLLY_ALIGNED.lock_recover().contains(&store));
        Ok(())
    }

    /// A camera past what a store may hold fails its pull the way it failed
    /// before a digest could answer for the state.
    #[test]
    fn a_camera_past_the_store_limit_fails_the_pull() -> Result<()> {
        let parent = std::env::temp_dir().join("renium-camera-limit-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "limit-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let small = workspace_state(Map::from_iter([("FieldOfView".into(), json!(70))]), 0.5)?;
        let huge = workspace_state(
            (0..5)
                .map(|index| {
                    (
                        format!("Huge{index}"),
                        json!(format!("{index}{}", "a".repeat(30 << 20))),
                    )
                })
                .chain([("FieldOfView".into(), json!(70))])
                .collect(),
            0.5,
        )?;
        assert!(encode_service_settings_binary(&huge).is_err());
        import_service_tree(&small, &root, &source, None, "Workspace")?;
        assert!(import_service_tree(&huge, &root, &source, None, "Workspace").is_err());
        Ok(())
    }

    #[test]
    fn memory_projection_matches_fresh_import_bytes_and_paths() -> Result<()> {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/projection-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "projection-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let mut instances = vec![SnapshotInstance {
            name: "ReplicatedStorage".into(),
            class_name: "ReplicatedStorage".into(),
            instance_index: Some(1),
            ..Default::default()
        }];
        // Duplicate sanitized names, all script classes and RunContext variants,
        // child-bearing scripts, no-source subtrees, references and native values.
        for index in 0..24 {
            let id = instances.len() + 1;
            instances.push(SnapshotInstance {
                name: if index % 2 == 0 { "same?" } else { "same*" }.into(),
                class_name: ["ModuleScript", "Script", "LocalScript"][index % 3].into(),
                instance_index: Some(id),
                parent_index: Some(1),
                properties: Map::from_iter([
                    (
                        "Source".into(),
                        json!(format!("-- {index}\r\nreturn {index}\r\n")),
                    ),
                    (
                        "RunContext".into(),
                        json!(["Legacy", "Client", "Plugin"][index % 3]),
                    ),
                ]),
                attributes: Map::from_iter([("enabled".into(), json!(false))]),
                ..Default::default()
            });
            if index % 2 == 0 {
                instances.push(SnapshotInstance {
                    name: "Not a script".into(),
                    class_name: "ObjectValue".into(),
                    instance_index: Some(id + 1),
                    parent_index: Some(id),
                    ..Default::default()
                });
            }
        }
        let mut state = build_service_state_from_instances(
            "ReplicatedStorage",
            None,
            instances,
            HashMap::new(),
            true,
        )?;
        let mut native = vec![Vec::new(); state.instances.len()];
        for (index, instance) in state.instances.iter().enumerate() {
            if instance.class_name == "ObjectValue" {
                native[index].push(crate::snapshot::types::NativeSettingsProperty {
                    name: "Value".into(),
                    value: crate::snapshot::types::NativeSettingsValue::Ref(1),
                });
            }
        }
        state.native_properties_by_instance = Some(native);
        let projected = service_projection_in_memory(&state, &source, None, "ReplicatedStorage")?;
        assert!(!source.exists(), "Memory projection wrote to disk");
        import_service_tree(&state, &root, &source, None, "ReplicatedStorage")?;
        let observed = walkdir::WalkDir::new(source.join("ReplicatedStorage"))
            .into_iter()
            .map(|entry| {
                let entry = entry?;
                let value = if entry.file_type().is_dir() {
                    None
                } else {
                    Some(fs::read(entry.path())?)
                };
                Ok((entry.into_path(), value))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        assert_eq!(projected, observed);
        let collision = SourceOutput::Memory(Mutex::new(BTreeMap::new()));
        let file = source.join("collision.luau");
        collision.source(&file, "return true")?;
        assert!(collision.directory(&file).is_err());
        assert!(collision.source(&file, "return false").is_err());
        state.instances[1]
            .properties
            .insert("Source".into(), json!(EXTERNAL_SOURCE_MARKER));
        assert!(
            service_projection_in_memory(&state, &source, None, "ReplicatedStorage")
                .unwrap_err()
                .to_string()
                .contains("Missing fetched Source")
        );
        Ok(())
    }

    fn packages_state() -> Result<ServiceState> {
        let module = |id: usize, name: &str, parent: usize, source: &str| SnapshotInstance {
            name: name.into(),
            class_name: "ModuleScript".into(),
            instance_index: Some(id),
            parent_index: Some(parent),
            properties: Map::from_iter([("Source".into(), json!(source))]),
            ..Default::default()
        };
        let folder = |id: usize, name: &str, parent: usize| SnapshotInstance {
            name: name.into(),
            class_name: "Folder".into(),
            instance_index: Some(id),
            parent_index: Some(parent),
            ..Default::default()
        };
        build_service_state_from_instances(
            "ReplicatedStorage",
            None,
            vec![
                SnapshotInstance {
                    name: "ReplicatedStorage".into(),
                    class_name: "ReplicatedStorage".into(),
                    instance_index: Some(1),
                    ..Default::default()
                },
                folder(2, "Packages", 1),
                module(
                    3,
                    "Promise",
                    2,
                    "return require(script.Parent._Index[\"evaera_promise@4.0.0\"][\"promise\"])",
                ),
                folder(4, "_Index", 2),
                folder(5, "evaera_promise@4.0.0", 4),
                module(6, "promise", 5, "return {}"),
                module(7, "Fresh", 2, "return 1"),
            ],
            HashMap::new(),
            true,
        )
    }

    fn write_packages_layout(service_dir: &Path) -> Result<(PathBuf, PathBuf)> {
        let packages = service_dir.join("Packages");
        let promise = packages.join("Promise.lua");
        let init = packages
            .join("_Index")
            .join("evaera_promise@4.0.0")
            .join("promise")
            .join("init.lua");
        fs::create_dir_all(init.parent().unwrap())?;
        fs::write(&promise, "return nil")?;
        fs::write(&init, "return nil")?;
        Ok((promise, init))
    }

    #[test]
    fn import_keeps_existing_script_files_and_forms() -> Result<()> {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/projection-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "layout-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        let service_dir = source.join("ReplicatedStorage");
        let (promise, init) = write_packages_layout(&service_dir)?;
        let state = packages_state()?;
        import_service_tree(&state, &root, &source, None, "ReplicatedStorage")?;
        assert_eq!(
            fs::read_to_string(&promise)?,
            "return require(script.Parent._Index[\"evaera_promise@4.0.0\"][\"promise\"])"
        );
        assert_eq!(fs::read_to_string(&init)?, "return {}");
        assert!(!promise.with_extension("luau").exists());
        assert!(!init.parent().unwrap().with_extension("luau").exists());
        assert!(service_dir.join("Packages").join("Fresh.luau").is_file());
        Ok(())
    }

    #[test]
    fn memory_projection_follows_the_layout_of_another_root() -> Result<()> {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/projection-tests");
        fs::create_dir_all(&parent)?;
        let root = crate::system::files::create_unique_directory(&parent, "layout-memory-")?;
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let project_source = root.join("project").join("src");
        write_packages_layout(&project_source.join("ReplicatedStorage"))?;
        let stage_source = root.join("stage").join("src");
        let state = packages_state()?;
        let projected = service_projection_in_memory(
            &state,
            &stage_source,
            Some(&project_source),
            "ReplicatedStorage",
        )?;
        assert!(!stage_source.exists(), "Memory projection wrote to disk");
        let packages = stage_source.join("ReplicatedStorage").join("Packages");
        let promise_dir = packages
            .join("_Index")
            .join("evaera_promise@4.0.0")
            .join("promise");
        assert_eq!(
            projected.get(&packages.join("Promise.lua")),
            Some(&Some(
                b"return require(script.Parent._Index[\"evaera_promise@4.0.0\"][\"promise\"])"
                    .to_vec()
            ))
        );
        assert_eq!(projected.get(&promise_dir), Some(&None));
        assert_eq!(
            projected.get(&promise_dir.join("init.lua")),
            Some(&Some(b"return {}".to_vec()))
        );
        assert_eq!(
            projected.get(&packages.join("Fresh.luau")),
            Some(&Some(b"return 1".to_vec()))
        );
        assert!(!projected.contains_key(&promise_dir.with_extension("luau")));
        let unaligned =
            service_projection_in_memory(&state, &stage_source, None, "ReplicatedStorage")?;
        assert!(unaligned.contains_key(&packages.join("Promise.luau")));
        assert!(unaligned.contains_key(&promise_dir.with_extension("luau")));
        Ok(())
    }
}
