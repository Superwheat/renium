use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde_json::{Map, Value, json};

use crate::app::timing::{elapsed_ms, log_timing, log_timing_ms, verbose_timing_logs};
use crate::bytecode::acquire_settings_file_lock;
use crate::editor::paths::{project_script_file_names, script_file_names};
use crate::project::sourcemap::{
    SourcemapNode, build_service_sourcemap_from_state, finalize_project_sourcemap_temp,
    load_existing_sourcemap_root, sourcemap_root_is_current,
};
use crate::rbx::encode::settings_root_indices;
use crate::roblox::schema::{MATERIAL_SERVICE_CLASS, USE_2022_MATERIALS_PROPERTY};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::{
    SettingsBytecode, child_indices_for_instance, encode_service_settings_binary,
    write_fresh_service_settings_binary_file,
};
use crate::settings::equivalence::{SettingsAlignment, align_settings_bytes_to_reference};
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
    pub(crate) fn start(project_root: PathBuf, durable: bool) -> Self {
        let (sender, receiver) = mpsc::channel::<SourcemapWriterMessage>();
        let handle = thread::spawn(move || -> Result<()> {
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
                import_service_state_with_sourcemap(&state, &self.project_root, &src_root, &service)
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
        project_root: PathBuf,
        src_dir: PathBuf,
        active_worker_count: usize,
        drain_worker_count: usize,
        sourcemap_sender: Option<mpsc::Sender<SourcemapWriterMessage>>,
        run_started: Instant,
    ) -> Result<Self> {
        let worker_count = drain_worker_count.max(active_worker_count);
        let queue = Arc::new(DirectImportTaskQueue::new(active_worker_count));
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
    service: &str,
) -> Result<SourcemapNode> {
    let import_started = Instant::now();
    let write_tree_started = Instant::now();
    let node = import_service_tree(state, project_root, src_root, service)?;
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
        let expected_batch = emit_children_indices(
            state,
            root_children,
            &service_dir,
            &SourceOutput::Files {
                fresh: fresh_service_dir,
            },
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
        let observed = encode_service_settings_binary(state_to_write)?;
        let alignment_started = Instant::now();
        let aligned = match fs::read(&settings_path) {
            Ok(reference) => match align_settings_bytes_to_reference(&reference, &observed)
                .with_context(|| format!("Failed to align {}", settings_path.display()))?
            {
                SettingsAlignment::Equivalent => None,
                SettingsAlignment::Changed(aligned) => Some(aligned),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(observed),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read {}", settings_path.display()));
            }
        };
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

/// Project the same settings, script names and directories as a fresh import,
/// without touching disk or constructing the full editor sourcemap.
pub(crate) fn service_projection_in_memory(
    state: &ServiceState,
    src_root: &Path,
    service: &str,
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let service_dir = src_root.join(sanitize_name(service));
    let settings_path = service_settings_path(&service_dir);
    let output = SourceOutput::Memory(Mutex::new(BTreeMap::new()));
    output.directory(&service_dir)?;
    let (settings, sources) = rayon::join(
        || encode_service_settings_binary(state),
        || {
            let visited = (0..state.instances.len())
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>();
            mark_visited(&visited, state.service_root_index);
            emit_children_indices(
                state,
                child_indices_for_instance(state, state.service_root_index),
                &service_dir,
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
            output.directory(&dir_path)?;
            expected.track_dir(&dir_path);
            let source_path = dir_path.join(source_file_name);
            output.source(&source_path, source)?;
            expected.track_file(&source_path);

            let child_expected =
                emit_children_indices(state, child_indices, &dir_path, output, visited)?;
            expected.extend(child_expected);
            return Ok(expected);
        }

        let script_path = parent_dir.join(format!("{fs_stem}{leaf_suffix}"));
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

    let child_expected = emit_children_indices(state, child_indices, &dir_path, output, visited)?;
    expected.extend(child_expected);

    Ok(expected)
}

fn emit_children_indices(
    state: &ServiceState,
    child_indices: &[usize],
    dir_path: &Path,
    output: &SourceOutput,
    visited: &[AtomicBool],
) -> Result<ExpectedPathBatch> {
    let named_children = name_child_indices(state, child_indices);

    const PARALLEL_CHILD_THRESHOLD: usize = 8;
    if named_children.len() >= PARALLEL_CHILD_THRESHOLD && rayon::current_num_threads() > 1 {
        let built = named_children
            .par_iter()
            .map(|(child_index, child_stem)| {
                emit_node_index(state, *child_index, dir_path, child_stem, output, visited)
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
            let child_expected =
                emit_node_index(state, child_index, dir_path, &child_stem, output, visited)?;
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
    use crate::project::config;
    use std::time::Duration;

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
        let projected = service_projection_in_memory(&state, &source, "ReplicatedStorage")?;
        assert!(!source.exists(), "Memory projection wrote to disk");
        import_service_tree(&state, &root, &source, "ReplicatedStorage")?;
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
            service_projection_in_memory(&state, &source, "ReplicatedStorage")
                .unwrap_err()
                .to_string()
                .contains("Missing fetched Source")
        );
        Ok(())
    }
}
