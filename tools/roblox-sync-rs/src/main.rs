use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use rayon::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_with_config};
use walkdir::WalkDir;

const DEFAULT_SERVICES: [&str; 11] = [
    "Workspace",
    "Players",
    "Lighting",
    "MaterialService",
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer",
];

type PropertyCandidateMap = HashMap<String, Vec<String>>;

const DEFAULT_PROPERTY_CANDIDATES: [&str; 90] = [
    "Archivable",
    "Enabled",
    "RunContext",
    "Disabled",
    "LinkedSource",
    "Value",
    "Name",
    "ClassName",
    "Parent",
    "Part0",
    "Part1",
    "Attachment0",
    "Attachment1",
    "AutoLocalize",
    "RootLocalizationTable",
    "BackgroundColor3",
    "BackgroundTransparency",
    "BorderColor3",
    "BorderSizePixel",
    "Position",
    "Size",
    "AnchorPoint",
    "Rotation",
    "Visible",
    "Text",
    "TextColor3",
    "TextSize",
    "TextScaled",
    "FontFace",
    "Image",
    "ImageColor3",
    "ImageTransparency",
    "Color",
    "Transparency",
    "ZIndex",
    "LayoutOrder",
    "Active",
    "Selectable",
    "CanvasSize",
    "ScrollBarThickness",
    "AutomaticCanvasSize",
    "RichText",
    "LineHeight",
    "MaxVisibleGraphemes",
    "SliceCenter",
    "ScaleType",
    "TileSize",
    "Padding",
    "CellPadding",
    "CellSize",
    "FillDirection",
    "SortOrder",
    "HorizontalAlignment",
    "VerticalAlignment",
    "ApplyStrokeMode",
    "Thickness",
    "Color3",
    "Material",
    "BrickColor",
    "CanCollide",
    "CanQuery",
    "CanTouch",
    "Massless",
    "Anchored",
    "CastShadow",
    "CFrame",
    "Orientation",
    "AssemblyLinearVelocity",
    "AssemblyAngularVelocity",
    "Shape",
    "Reflectance",
    "TopSurface",
    "BottomSurface",
    "LeftSurface",
    "RightSurface",
    "FrontSurface",
    "BackSurface",
    "LightInfluence",
    "Brightness",
    "ClockTime",
    "FogColor",
    "FogEnd",
    "FogStart",
    "GeographicLatitude",
    "GlobalShadows",
    "EnvironmentDiffuseScale",
    "EnvironmentSpecularScale",
    "Ambient",
    "OutdoorAmbient",
    "Technology",
];

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn log_timing(label: &str, started: Instant) {
    println!(
        "[roblox-sync-rs] timing: {label} took {:.1}ms",
        elapsed_ms(started)
    );
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "High-performance Roblox snapshot importer and project JSON generator"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    ExportSnapshots(ExportSnapshotsArgs),
    ImportSnapshots(ImportSnapshotsArgs),
    ImportService(ImportServiceArgs),
    GenerateSourcemap(GenerateSourcemapArgs),
}

#[derive(Parser, Debug)]
struct ExportSnapshotsArgs {
    #[arg(long, value_name = "PATH", default_value = ".")]
    project_root: PathBuf,
    #[arg(long, value_name = "PATH", default_value = "snapshots")]
    snapshot_dir: PathBuf,
    #[arg(long, value_name = "SERVICES", default_value = "")]
    services: String,
    #[arg(long, default_value_t = 262144)]
    chunk_size: usize,
    #[arg(long, default_value_t = 0)]
    snapshot_instance_chunk_size: usize,
    #[arg(long, default_value_t = 8.0)]
    bridge_wait_seconds: f64,
    #[arg(long, default_value = "127.0.0.1")]
    bridge_host: String,
    #[arg(long, default_value = "8781,8782,8783,8784")]
    bridge_ports: String,
    #[arg(long, action = ArgAction::SetTrue, default_value_t = false)]
    run_import: bool,
    #[arg(long, action = ArgAction::SetTrue, default_value_t = false)]
    no_run_import: bool,
    #[arg(long, default_value = "direct")]
    import_mode: String,
    #[arg(long, action = ArgAction::SetTrue, default_value_t = false)]
    no_update_editor_icons: bool,
    #[arg(long, default_value_t = 20.0)]
    ws_wait_seconds: f64,
    #[arg(long, default_value = "ws")]
    transport: String,
    #[arg(long, default_value = "Roblox_Studio")]
    server: String,
    #[arg(long, value_name = "PATH", default_value = "")]
    config: String,
    #[arg(long, default_value_t = 0)]
    source_workers: usize,
    #[arg(long, default_value_t = 0)]
    instance_workers: usize,
    #[arg(long, default_value_t = 0)]
    import_workers: usize,
    #[arg(long, action = ArgAction::SetTrue)]
    adaptive_throttle: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    no_adaptive_throttle: bool,
    #[arg(long, action = ArgAction::SetTrue, default_value_t = false)]
    export_all_properties: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    no_export_all_properties: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    compact_meta_json: bool,
    #[arg(long, value_name = "PATH", default_value = "")]
    import_cli: String,
}

#[derive(Parser, Debug)]
struct ImportSnapshotsArgs {
    #[arg(long, value_name = "PATH")]
    snapshot_dir: PathBuf,
    #[arg(long, value_name = "PATH")]
    project_root: PathBuf,
    #[arg(long, value_name = "SERVICES", default_value = "")]
    services: String,
    #[arg(long, action = ArgAction::SetTrue)]
    no_project_write: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    compact_meta_json: bool,
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

#[derive(Parser, Debug)]
struct ImportServiceArgs {
    #[arg(long, value_name = "PATH")]
    project_root: PathBuf,
    #[arg(long, value_name = "SERVICE")]
    service: String,
    #[arg(long, value_name = "PATH")]
    snapshot_file: Option<PathBuf>,
    #[arg(long, action = ArgAction::SetTrue)]
    no_project_write: bool,
    #[arg(long, action = ArgAction::SetTrue)]
    compact_meta_json: bool,
}

#[derive(Parser, Debug)]
struct GenerateSourcemapArgs {
    #[arg(long, value_name = "PATH", default_value = ".")]
    project_root: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct SnapshotManifest {
    #[serde(default)]
    instances: Vec<SnapshotInstance>,
    #[serde(default, rename = "instanceChunks")]
    instance_chunks: Vec<InstanceChunkEntry>,
    #[serde(default)]
    services: Vec<SnapshotServiceRef>,
    #[serde(default, rename = "classDefaults")]
    class_defaults: Value,
}

#[derive(Debug, Default, Deserialize)]
struct SnapshotServiceRef {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum InstanceChunkEntry {
    FileName(String),
    Entry { file: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SnapshotInstance {
    path: String,
    #[serde(default, rename = "pathSegments")]
    path_segments: Vec<String>,
    name: String,
    #[serde(rename = "className")]
    class_name: String,
    #[serde(default)]
    properties: Map<String, Value>,
    #[serde(default, rename = "sourceKey", skip_serializing_if = "Option::is_none")]
    source_key: Option<String>,
    #[serde(default, rename = "parentPath")]
    parent_path: Option<String>,
    #[serde(default)]
    attributes: Map<String, Value>,
    #[serde(default, rename = "debugId")]
    debug_id: Option<String>,
    #[serde(default, rename = "parentDebugId")]
    parent_debug_id: Option<String>,
    #[serde(default, rename = "instanceId")]
    instance_id: Option<String>,
    #[serde(default, rename = "parentInstanceId")]
    parent_instance_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ServiceState {
    instances: Vec<SnapshotInstance>,
    children_by_parent_instance_id: HashMap<String, Vec<usize>>,
    children_by_parent_path: HashMap<String, Vec<usize>>,
    children_by_parent_debug: HashMap<String, Vec<usize>>,
    index_by_instance_id: HashMap<String, usize>,
    index_by_debug_id: HashMap<String, usize>,
    index_by_path: HashMap<String, usize>,
    index_by_path_segments: HashMap<String, usize>,
    rojo_ref_ids_by_index: Vec<Option<String>>,
    service_root_index: usize,
    class_defaults_by_class: HashMap<String, Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct BridgeChunk {
    #[serde(default, rename = "nextStart")]
    next_start: usize,
    #[serde(default)]
    total: usize,
    #[serde(default)]
    chunk: String,
}

struct ExportedSnapshotParts {
    service_name: String,
    service_class: String,
    service_path: String,
    generated_at: i64,
    script_count: usize,
    class_defaults: Value,
    instances: Vec<SnapshotInstance>,
    adaptive_tune: Option<AdaptiveTuneEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AdaptiveTuneCache {
    #[serde(default)]
    services: HashMap<String, AdaptiveTuneEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdaptiveTuneEntry {
    #[serde(rename = "batchSize")]
    batch_size: usize,
    workers: usize,
    #[serde(default, rename = "instanceCount")]
    instance_count: usize,
    #[serde(default, rename = "frameMs")]
    frame_ms: Option<f64>,
    #[serde(default, rename = "updatedAtUnix")]
    updated_at_unix: i64,
}

struct InstanceFetchResult {
    instances: Vec<SnapshotInstance>,
    tune: Option<AdaptiveTuneEntry>,
}

struct BridgeSocket {
    port: u16,
    socket: WebSocket<TcpStream>,
}

struct BridgeServer {
    sockets: Vec<Arc<Mutex<BridgeSocket>>>,
    next_id: std::sync::atomic::AtomicU64,
    preferred_index: std::sync::atomic::AtomicUsize,
}

impl BridgeServer {
    fn listen(host: &str, ports: &[u16], wait_seconds: f64) -> Result<Self> {
        let bind_host = if host.trim().is_empty() {
            "127.0.0.1"
        } else {
            host
        };
        let mut listeners: Vec<(u16, TcpListener)> = Vec::new();
        for port in ports {
            let listener = TcpListener::bind((bind_host, *port))
                .with_context(|| format!("Failed to bind bridge server on {bind_host}:{port}"))?;
            listener.set_nonblocking(true).with_context(|| {
                format!("Failed to set nonblocking listener {bind_host}:{port}")
            })?;
            println!("[roblox-sync-rs] bridge listening on {bind_host}:{port}");
            listeners.push((*port, listener));
        }

        let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds.max(1.0));
        let mut sockets: Vec<BridgeSocket> = Vec::new();
        let mut seen_ports: HashSet<u16> = HashSet::new();

        while Instant::now() < deadline {
            for (port, listener) in &listeners {
                if seen_ports.contains(port) {
                    continue;
                }
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_nodelay(true);
                        let socket_config = WebSocketConfig::default()
                            .read_buffer_size(256 * 1024)
                            .write_buffer_size(32 * 1024)
                            .max_write_buffer_size(4 * 1024 * 1024)
                            .max_message_size(Some(256 * 1024 * 1024))
                            .max_frame_size(Some(32 * 1024 * 1024))
                            .accept_unmasked_frames(false);
                        let mut socket = accept_with_config(stream, Some(socket_config))
                            .with_context(|| {
                                format!("WebSocket handshake failed on {bind_host}:{port}")
                            })?;
                        let _ = socket
                            .get_mut()
                            .set_read_timeout(Some(Duration::from_secs(30)));
                        let _ = socket
                            .get_mut()
                            .set_write_timeout(Some(Duration::from_secs(30)));
                        println!(
                            "[roblox-sync-rs] bridge channel connected on {bind_host}:{port} from {addr}"
                        );
                        sockets.push(BridgeSocket {
                            port: *port,
                            socket,
                        });
                        seen_ports.insert(*port);
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) => {
                        println!(
                            "[roblox-sync-rs] warning: listener accept failed on {bind_host}:{port}: {err}"
                        );
                    }
                }
            }
            if seen_ports.len() == listeners.len() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        if sockets.is_empty() {
            bail!(
                "No plugin bridge channels connected within {:.1}s on {:?}",
                wait_seconds.max(1.0),
                ports
            );
        }
        if seen_ports.len() != listeners.len() {
            let missing_ports: Vec<u16> = listeners
                .iter()
                .map(|(port, _)| *port)
                .filter(|port| !seen_ports.contains(port))
                .collect();
            bail!(
                "Only {}/{} plugin bridge channels connected within {:.1}s. Missing ports: {:?}",
                seen_ports.len(),
                listeners.len(),
                wait_seconds.max(1.0),
                missing_ports
            );
        }

        sockets.sort_by_key(|s| s.port);
        Ok(Self {
            sockets: sockets
                .into_iter()
                .map(|socket| Arc::new(Mutex::new(socket)))
                .collect(),
            next_id: std::sync::atomic::AtomicU64::new(1),
            preferred_index: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        if self.sockets.is_empty() {
            bail!("No active bridge sockets");
        }

        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let total = self.sockets.len();
        let start = self
            .preferred_index
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % total;

        let mut last_error: Option<String> = None;
        for _ in 0..64 {
            for offset in 0..total {
                let index = (start + offset) % total;
                let socket_lock = &self.sockets[index];
                let mut socket = match socket_lock.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => continue,
                };
                match Self::call_on_socket(&mut socket, id, method, &params) {
                    Ok(result) => {
                        return Ok(result);
                    }
                    Err(err) => {
                        last_error = Some(format!("port {}: {err}", socket.port));
                    }
                }
            }
            thread::yield_now();
        }

        for offset in 0..total {
            let index = (start + offset) % total;
            let socket_lock = &self.sockets[index];
            let mut socket = match socket_lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match Self::call_on_socket(&mut socket, id, method, &params) {
                Ok(result) => {
                    return Ok(result);
                }
                Err(err) => {
                    last_error = Some(format!("port {}: {err}", socket.port));
                }
            }
        }

        bail!(
            "Bridge call failed for {method}: {}",
            last_error.unwrap_or_else(|| "no active channels".to_string())
        )
    }

    fn channel_count(&self) -> usize {
        self.sockets.len()
    }

    fn call_on_socket(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
    ) -> Result<Value> {
        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        bridge_socket
            .socket
            .send(Message::Text(payload.to_string().into()))
            .with_context(|| {
                format!(
                    "Bridge send failed for method {method} on port {}",
                    bridge_socket.port
                )
            })?;

        loop {
            let message = bridge_socket.socket.read().with_context(|| {
                format!(
                    "Bridge read failed for method {method} on port {}",
                    bridge_socket.port
                )
            })?;
            match message {
                Message::Text(text) => {
                    let parsed: Value = serde_json::from_str(&text)
                        .with_context(|| format!("Invalid bridge JSON message: {text}"))?;
                    let msg_id = parsed.get("id").and_then(Value::as_u64);
                    if msg_id != Some(id) {
                        continue;
                    }
                    let ok = parsed.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    if ok {
                        return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
                    }
                    let err = parsed
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("bridge error");
                    bail!("Bridge method {method} failed: {err}");
                }
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                    continue;
                }
                Message::Close(frame) => {
                    bail!("Bridge closed while waiting for {method}: {frame:?}");
                }
            }
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::ExportSnapshots(args) => export_snapshots(args),
        Commands::ImportSnapshots(args) => import_snapshots(args),
        Commands::ImportService(args) => import_service(args),
        Commands::GenerateSourcemap(args) => generate_sourcemap_command(args),
    }
}

fn generate_sourcemap_command(args: GenerateSourcemapArgs) -> Result<()> {
    let project_root = args.project_root.canonicalize().with_context(|| {
        format!(
            "Failed to resolve project root: {}",
            args.project_root.display()
        )
    })?;
    generate_project_sourcemap(&project_root)
}

fn adaptive_tune_cache_path(project_root: &Path) -> PathBuf {
    project_root.join(".roblox-sync-adaptive.json")
}

fn load_adaptive_tune_cache(project_root: &Path) -> AdaptiveTuneCache {
    let path = adaptive_tune_cache_path(project_root);
    let Ok(cache) = read_json_file::<AdaptiveTuneCache>(&path) else {
        return AdaptiveTuneCache::default();
    };
    cache
}

fn write_adaptive_tune_cache(project_root: &Path, cache: &AdaptiveTuneCache) {
    let path = adaptive_tune_cache_path(project_root);
    if let Err(err) = write_json_file(&path, cache, true) {
        println!(
            "[roblox-sync-rs] warning: failed to write adaptive tuning cache {}: {err:#}",
            path.display()
        );
    }
}

enum DirectImportTask {
    Service {
        service: String,
        parts: ExportedSnapshotParts,
    },
    Subtree(DirectImportSubtreeTask),
    Shutdown,
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
    project_root: PathBuf,
    compact_meta_json: bool,
    state: Arc<ServiceState>,
    expected_paths: Arc<ImportPathSets>,
    visited: Arc<Vec<AtomicBool>>,
    slots: Mutex<Vec<Option<SourcemapNode>>>,
    queued_tasks: AtomicUsize,
    completed_tasks: AtomicUsize,
    total_task_tenths_ms: AtomicU64,
    max_task_tenths_ms: AtomicU64,
    failed: AtomicBool,
    started: Instant,
}

struct SplitNodeShell {
    name: String,
    class_name: String,
    file_paths: Vec<String>,
    dir_path: PathBuf,
}

enum SourcemapWriterMessage {
    Service(String, SourcemapNode),
    Finish,
}

struct SourcemapWriter {
    sender: mpsc::Sender<SourcemapWriterMessage>,
    handle: Option<thread::JoinHandle<Result<()>>>,
}

impl SourcemapWriter {
    fn start(project_root: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel::<SourcemapWriterMessage>();
        let handle = thread::spawn(move || -> Result<()> {
            let mut service_nodes = HashMap::<String, SourcemapNode>::new();
            while let Ok(message) = receiver.recv() {
                let mut pending_finish = false;
                match message {
                    SourcemapWriterMessage::Service(service, node) => {
                        service_nodes.insert(service, node);
                    }
                    SourcemapWriterMessage::Finish => pending_finish = true,
                }

                while let Ok(pending) = receiver.try_recv() {
                    match pending {
                        SourcemapWriterMessage::Service(service, node) => {
                            service_nodes.insert(service, node);
                        }
                        SourcemapWriterMessage::Finish => {
                            pending_finish = true;
                            break;
                        }
                    }
                }

                if pending_finish {
                    finalize_project_sourcemap_temp(&project_root, &service_nodes)?;
                    return Ok(());
                }

                write_project_sourcemap_temp_from_service_nodes(&project_root, &service_nodes)?;
            }
            finalize_project_sourcemap_temp(&project_root, &service_nodes)
        });

        Self {
            sender,
            handle: Some(handle),
        }
    }

    fn sender(&self) -> mpsc::Sender<SourcemapWriterMessage> {
        self.sender.clone()
    }

    fn finish(mut self) -> Result<()> {
        let _ = self.sender.send(SourcemapWriterMessage::Finish);
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

struct DirectImportDispatcher {
    sender: Option<mpsc::SyncSender<DirectImportTask>>,
    workers: Vec<thread::JoinHandle<()>>,
    first_error: Arc<Mutex<Option<String>>>,
    service_nodes: Arc<Mutex<HashMap<String, SourcemapNode>>>,
    pending_tasks: Arc<AtomicUsize>,
    worker_count: usize,
}

impl DirectImportDispatcher {
    fn start(
        project_root: PathBuf,
        compact_meta_json: bool,
        worker_count: usize,
        sourcemap_sender: Option<mpsc::Sender<SourcemapWriterMessage>>,
    ) -> Self {
        let queue_capacity = worker_count.max(1).saturating_mul(64).clamp(32, 1024);
        let (sender, receiver) = mpsc::sync_channel::<DirectImportTask>(queue_capacity);
        let shared_receiver = Arc::new(Mutex::new(receiver));
        let first_error = Arc::new(Mutex::new(None::<String>));
        let service_nodes = Arc::new(Mutex::new(HashMap::<String, SourcemapNode>::new()));
        let pending_tasks = Arc::new(AtomicUsize::new(0));
        let worker_count = worker_count.max(1);

        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let recv_clone = Arc::clone(&shared_receiver);
            let sender_clone = sender.clone();
            let root_clone = project_root.clone();
            let error_clone = Arc::clone(&first_error);
            let service_nodes_clone = Arc::clone(&service_nodes);
            let pending_tasks_clone = Arc::clone(&pending_tasks);
            let sourcemap_sender_clone = sourcemap_sender.clone();
            workers.push(thread::spawn(move || {
                loop {
                    let task = {
                        let guard = match recv_clone.lock() {
                            Ok(lock) => lock,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        guard.recv()
                    };
                    let Ok(task) = task else {
                        break;
                    };
                    match task {
                        DirectImportTask::Service { service, parts } => {
                            let src_root = root_clone.join("src");
                            let worker_started = Instant::now();
                            println!("[roblox-sync-rs] {}: direct import worker start", service);
                            let import_result = fs::create_dir_all(&src_root)
                                .with_context(|| format!("Failed to create {}", src_root.display()))
                                .and_then(|_| {
                                    let build_started = Instant::now();
                                    let state = exported_parts_to_service_state(&service, parts)?;
                                    log_timing(
                                        &format!("{}: build service state", service),
                                        build_started,
                                    );
                                    Ok(state)
                                })
                                .and_then(|state| {
                                    let maybe_shared = maybe_enqueue_split_import_tasks(
                                        &sender_clone,
                                        &pending_tasks_clone,
                                        &root_clone,
                                        &src_root,
                                        &service,
                                        compact_meta_json,
                                        &state,
                                    )?;
                                    if let Some(shared) = maybe_shared {
                                        println!(
                                            "[roblox-sync-rs] {}: queued {} subtree import tasks",
                                            service,
                                            shared.queued_tasks.load(Ordering::Acquire)
                                        );
                                        Ok(None)
                                    } else {
                                        let import_started = Instant::now();
                                        let node = import_service_state_with_sourcemap(
                                            &state,
                                            &root_clone,
                                            &src_root,
                                            &service,
                                            compact_meta_json,
                                        )?;
                                        log_timing(
                                            &format!("{}: import + sourcemap build", service),
                                            import_started,
                                        );
                                        Ok(Some(node))
                                    }
                                });

                            match import_result {
                                Ok(Some(node)) => {
                                    if let Some(sender) = sourcemap_sender_clone.as_ref() {
                                        let _ = sender.send(SourcemapWriterMessage::Service(
                                            service.clone(),
                                            node.clone(),
                                        ));
                                    }
                                    let mut nodes = match service_nodes_clone.lock() {
                                        Ok(lock) => lock,
                                        Err(poisoned) => poisoned.into_inner(),
                                    };
                                    nodes.insert(service.clone(), node);
                                    log_timing(
                                        &format!("{}: direct import worker total", service),
                                        worker_started,
                                    );
                                }
                                Ok(None) => {
                                    log_timing(
                                        &format!("{}: direct import worker total", service),
                                        worker_started,
                                    );
                                }
                                Err(err) => {
                                    let mut slot = match error_clone.lock() {
                                        Ok(lock) => lock,
                                        Err(poisoned) => poisoned.into_inner(),
                                    };
                                    if slot.is_none() {
                                        *slot = Some(format!("{}: {}", service, err));
                                    }
                                    log_timing(
                                        &format!("{}: direct import worker total", service),
                                        worker_started,
                                    );
                                }
                            }
                        }
                        DirectImportTask::Subtree(task) => {
                            let worker_started = Instant::now();
                            let shared = Arc::clone(&task.shared);
                            let service = shared.service.clone();
                            let result = process_split_subtree_task(
                                task,
                                &service_nodes_clone,
                                sourcemap_sender_clone.as_ref(),
                                worker_started,
                            );
                            if let Err(err) = result {
                                let mut slot = match error_clone.lock() {
                                    Ok(lock) => lock,
                                    Err(poisoned) => poisoned.into_inner(),
                                };
                                if slot.is_none() {
                                    *slot = Some(format!("{}: {}", service, err));
                                }
                            }
                        }
                        DirectImportTask::Shutdown => break,
                    }
                    pending_tasks_clone.fetch_sub(1, Ordering::AcqRel);
                }
            }));
        }

        Self {
            sender: Some(sender),
            workers,
            first_error,
            service_nodes,
            pending_tasks,
            worker_count,
        }
    }

    fn enqueue_parts(&self, service: &str, parts: ExportedSnapshotParts) -> Result<()> {
        self.check_error()?;
        let sender = self
            .sender
            .as_ref()
            .with_context(|| "Direct import dispatcher is closed")?;
        self.pending_tasks.fetch_add(1, Ordering::AcqRel);
        sender
            .send(DirectImportTask::Service {
                service: service.to_string(),
                parts,
            })
            .map_err(|err| {
                self.pending_tasks.fetch_sub(1, Ordering::AcqRel);
                anyhow::anyhow!("Failed to queue direct import task: {err}")
            })
    }

    fn check_error(&self) -> Result<()> {
        let slot = match self.first_error.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(message) = slot.as_ref() {
            bail!("Direct import failed: {message}");
        }
        Ok(())
    }

    fn finish(mut self) -> Result<HashMap<String, SourcemapNode>> {
        while self.pending_tasks.load(Ordering::Acquire) > 0 {
            self.check_error()?;
            thread::sleep(Duration::from_millis(5));
        }
        if let Some(sender) = self.sender.take() {
            for _ in 0..self.worker_count {
                let _ = sender.send(DirectImportTask::Shutdown);
            }
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        self.check_error()?;
        let nodes = match self.service_nodes.lock() {
            Ok(lock) => lock.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        Ok(nodes)
    }
}

impl Drop for DirectImportDispatcher {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            for _ in 0..self.worker_count {
                let _ = sender.send(DirectImportTask::Shutdown);
            }
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn export_snapshots(args: ExportSnapshotsArgs) -> Result<()> {
    let total_started = Instant::now();
    if args.transport != "ws" {
        bail!("Only --transport ws is supported in rust exporter");
    }

    let project_root = args.project_root.canonicalize().with_context(|| {
        format!(
            "Failed to resolve project root: {}",
            args.project_root.display()
        )
    })?;
    let services = parse_services(&args.services)?;
    let snapshot_dir = if args.snapshot_dir.is_absolute() {
        args.snapshot_dir.clone()
    } else {
        project_root.join(&args.snapshot_dir)
    };
    fs::create_dir_all(&snapshot_dir)
        .with_context(|| format!("Failed to create snapshot dir {}", snapshot_dir.display()))?;

    let ports = parse_bridge_ports(&args.bridge_ports)?;
    println!(
        "[roblox-sync-rs] export start: services={}, chunk_size={}, import_mode={}",
        services.len(),
        args.chunk_size,
        args.import_mode
    );
    let bridge = BridgeServer::listen(&args.bridge_host, &ports, args.bridge_wait_seconds)?;
    if args.no_export_all_properties {
        println!(
            "[roblox-sync-rs] ignoring --no-export-all-properties: full property export is enforced"
        );
    }
    if !args.export_all_properties {
        println!(
            "[roblox-sync-rs] full property export is enabled by default; keeping exportAllProperties=true"
        );
    }
    let export_all_properties = true;
    let _ = bridge.call(
        "setExportOptions",
        json!({
            "exportAllProperties": export_all_properties,
            "preSerializeOnPrepare": true
        }),
    );
    let property_candidates_by_class =
        load_rbx_dom_property_candidates(&project_root)?.unwrap_or_default();
    if !property_candidates_by_class.is_empty() {
        match bridge.call(
            "configurePropertyCandidates",
            json!({ "classes": property_candidate_map_to_value(&property_candidates_by_class) }),
        ) {
            Ok(result) => {
                let class_count = result
                    .get("classCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let property_count = result
                    .get("propertyCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                println!(
                    "[roblox-sync-rs] configured property candidates from rbx-dom: classes={}, properties={}",
                    class_count, property_count
                );
            }
            Err(err) => {
                println!(
                    "[roblox-sync-rs] warning: failed to configure rbx-dom property candidates: {}",
                    err
                );
            }
        }
    } else {
        println!(
            "[roblox-sync-rs] warning: rbx-dom property database not found, falling back to built-in property candidates"
        );
    }

    let run_import = if args.no_run_import {
        false
    } else if args.run_import {
        true
    } else {
        true
    };
    let adaptive_instance_batches = if args.no_adaptive_throttle {
        false
    } else if args.adaptive_throttle {
        true
    } else {
        true
    };
    println!(
        "[roblox-sync-rs] adaptive instance batching: {}",
        if adaptive_instance_batches {
            "enabled"
        } else {
            "disabled"
        }
    );
    let direct_import_mode = run_import && args.import_mode == "direct";
    let compact_meta_json = args.compact_meta_json;
    let effective_chunk = args.chunk_size.max(256);
    let source_workers = args.source_workers;
    let instance_workers = args.instance_workers;
    let import_workers = args.import_workers;
    let mut adaptive_tune_cache = load_adaptive_tune_cache(&project_root);
    let sourcemap_writer = if direct_import_mode {
        Some(SourcemapWriter::start(project_root.clone()))
    } else {
        None
    };

    let mut direct_import_dispatcher = if direct_import_mode {
        let resolved_import_workers = resolve_direct_import_workers(import_workers);
        println!(
            "[roblox-sync-rs] direct import workers: {}",
            resolved_import_workers
        );
        Some(DirectImportDispatcher::start(
            project_root.clone(),
            compact_meta_json,
            resolved_import_workers,
            sourcemap_writer.as_ref().map(SourcemapWriter::sender),
        ))
    } else {
        None
    };

    let export_services = if direct_import_mode {
        direct_import_export_order(&services)
    } else {
        services.clone()
    };
    if direct_import_mode && export_services != services {
        println!(
            "[roblox-sync-rs] direct import export order: {}",
            export_services.join(",")
        );
    }

    for service in &export_services {
        if let Some(dispatcher) = direct_import_dispatcher.as_ref() {
            dispatcher.check_error()?;
        }
        println!("[roblox-sync-rs] exporting {service}");
        let parts = export_single_service_parts(
            &bridge,
            service,
            effective_chunk,
            adaptive_instance_batches,
            source_workers,
            instance_workers,
            adaptive_tune_cache.services.get(service).cloned(),
            &property_candidates_by_class,
        )?;
        if let Some(tune) = parts.adaptive_tune.clone() {
            adaptive_tune_cache
                .services
                .insert(service.to_string(), tune);
            write_adaptive_tune_cache(&project_root, &adaptive_tune_cache);
        }

        if direct_import_mode {
            if let Some(dispatcher) = direct_import_dispatcher.as_ref() {
                dispatcher.enqueue_parts(service, parts)?;
            }
        } else {
            let snapshot = exported_parts_to_snapshot(parts);
            write_snapshot_file(&snapshot_dir, service, &snapshot)?;
        }
    }

    if run_import {
        if direct_import_mode {
            let mut sourcemap_nodes = HashMap::new();
            if let Some(dispatcher) = direct_import_dispatcher.take() {
                let drain_started = Instant::now();
                sourcemap_nodes = dispatcher.finish()?;
                log_timing("direct import dispatcher drain", drain_started);
            }
            let project_started = Instant::now();
            write_generated_project(&project_root, &services, compact_meta_json)?;
            log_timing("write generated project", project_started);
            if let Some(writer) = sourcemap_writer {
                let sourcemap_started = Instant::now();
                if let Err(err) = writer.finish() {
                    println!("[roblox-sync-rs] warning: {err}");
                }
                log_timing("sourcemap finalize", sourcemap_started);
            } else if let Err(err) = {
                let sourcemap_started = Instant::now();
                let result =
                    write_project_sourcemap_from_service_nodes(&project_root, &sourcemap_nodes);
                log_timing("sourcemap finalize", sourcemap_started);
                result
            } {
                println!("[roblox-sync-rs] warning: {err}");
            }
        } else {
            let import_args = ImportSnapshotsArgs {
                snapshot_dir: snapshot_dir.clone(),
                project_root: project_root.clone(),
                services: services.join(","),
                no_project_write: false,
                compact_meta_json,
                threads: 0,
            };
            import_snapshots(import_args)?;
        }
    }

    log_timing("full export-snapshots run", total_started);
    println!("[roblox-sync-rs] export done");
    Ok(())
}

fn parse_bridge_ports(raw: &str) -> Result<Vec<u16>> {
    let mut out = Vec::new();
    for token in raw.split(',') {
        let text = token.trim();
        if text.is_empty() {
            continue;
        }
        let value: u16 = text
            .parse()
            .with_context(|| format!("Invalid bridge port: {text}"))?;
        if value == 0 {
            bail!("Invalid bridge port: {text}");
        }
        if !out.contains(&value) {
            out.push(value);
        }
    }
    if out.is_empty() {
        bail!("No bridge ports configured");
    }
    if out.len() > 4 {
        bail!(
            "Only 4 bridge ports are supported; got {} in {:?}",
            out.len(),
            out
        );
    }
    Ok(out)
}

fn load_rbx_dom_property_candidates(project_root: &Path) -> Result<Option<PropertyCandidateMap>> {
    let database_path = project_root.join("_external/rbx-dom/rbx_dom_lua/src/database.json");
    if !database_path.exists() {
        return Ok(None);
    }

    let database_value: Value = read_json_file(&database_path)?;
    let classes = database_value
        .get("Classes")
        .and_then(Value::as_object)
        .with_context(|| format!("Missing Classes object in {}", database_path.display()))?;

    let mut by_class: PropertyCandidateMap = HashMap::new();
    let mut memo: HashMap<String, Vec<String>> = HashMap::new();
    let mut visiting: HashSet<String> = HashSet::new();

    for class_name in classes.keys() {
        let property_names =
            collect_rbx_dom_properties_for_class(class_name, classes, &mut memo, &mut visiting);
        if property_names.is_empty() {
            continue;
        }
        by_class.insert(class_name.clone(), property_names);
    }

    if by_class.is_empty() {
        return Ok(None);
    }
    Ok(Some(by_class))
}

fn property_candidate_map_to_value(by_class: &PropertyCandidateMap) -> Value {
    let mut out = Map::new();
    for (class_name, property_names) in by_class {
        out.insert(
            class_name.clone(),
            Value::Array(property_names.iter().cloned().map(Value::String).collect()),
        );
    }
    Value::Object(out)
}

fn parse_property_candidate_map(value: Option<&Value>) -> Result<PropertyCandidateMap> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let Some(classes) = value.as_object() else {
        bail!("propertyCandidatesByClass must be an object");
    };

    let mut out = HashMap::with_capacity(classes.len());
    for (class_name, property_names_value) in classes {
        let property_names = property_names_value
            .as_array()
            .with_context(|| format!("propertyCandidatesByClass.{class_name} must be an array"))?
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !property_names.is_empty() {
            out.insert(class_name.clone(), property_names);
        }
    }
    Ok(out)
}

fn collect_rbx_dom_properties_for_class(
    class_name: &str,
    classes: &Map<String, Value>,
    memo: &mut HashMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
) -> Vec<String> {
    if let Some(cached) = memo.get(class_name) {
        return cached.clone();
    }
    if !visiting.insert(class_name.to_string()) {
        return Vec::new();
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(class_value) = classes.get(class_name).and_then(Value::as_object) {
        if let Some(superclass) = class_value
            .get("Superclass")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let inherited =
                collect_rbx_dom_properties_for_class(superclass, classes, memo, visiting);
            for inherited_name in inherited {
                let key = inherited_name.to_ascii_lowercase();
                if seen.insert(key) {
                    ordered.push(inherited_name);
                }
            }
        }

        if let Some(properties) = class_value.get("Properties").and_then(Value::as_object) {
            for (property_name, property_value) in properties {
                if property_name.eq_ignore_ascii_case("source")
                    || property_name.eq_ignore_ascii_case("robloxlocked")
                {
                    continue;
                }
                if !is_readable_script_property(property_value) {
                    continue;
                }
                if !is_serialized_property(property_value) {
                    continue;
                }
                if !is_supported_property_data_type(property_value) {
                    continue;
                }

                let key = property_name.to_ascii_lowercase();
                if seen.insert(key) {
                    ordered.push(property_name.clone());
                }
            }
        }
    }

    ordered.sort();
    visiting.remove(class_name);
    memo.insert(class_name.to_string(), ordered.clone());
    ordered
}

fn is_readable_script_property(property_value: &Value) -> bool {
    matches!(
        property_value.get("Scriptability").and_then(Value::as_str),
        Some("Read") | Some("ReadWrite") | Some("Custom")
    )
}

fn is_serialized_property(property_value: &Value) -> bool {
    let Some(serialization) = property_value
        .get("Kind")
        .and_then(|kind| kind.get("Canonical"))
        .and_then(|canonical| canonical.get("Serialization"))
    else {
        return true;
    };

    match serialization {
        Value::String(mode) => mode != "DoesNotSerialize",
        Value::Object(_) => true,
        _ => true,
    }
}

fn is_supported_property_data_type(property_value: &Value) -> bool {
    let Some(data_type) = property_value.get("DataType").and_then(Value::as_object) else {
        return false;
    };

    if data_type.contains_key("Enum") {
        return true;
    }

    let Some(value_type) = data_type.get("Value").and_then(Value::as_str) else {
        return false;
    };

    matches!(
        value_type,
        "Bool"
            | "Int32"
            | "Int64"
            | "Float32"
            | "Float64"
            | "String"
            | "ContentId"
            | "Ref"
            | "Vector2"
            | "Vector3"
            | "UDim"
            | "UDim2"
            | "Color3"
            | "Color3uint8"
            | "ColorSequence"
            | "NumberSequence"
            | "CFrame"
            | "Rect"
            | "Font"
            | "BrickColor"
    )
}

fn export_single_service_parts(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    adaptive_instance_batches: bool,
    source_workers: usize,
    instance_workers: usize,
    cached_tune: Option<AdaptiveTuneEntry>,
    default_property_candidates_by_class: &PropertyCandidateMap,
) -> Result<ExportedSnapshotParts> {
    let service_started = Instant::now();
    let prepare_started = Instant::now();
    let prepare = bridge.call("prepare", json!({ "service": service }))?;
    log_timing(&format!("{service}: prepare"), prepare_started);
    let instance_count = prepare
        .get("instanceCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let script_count = prepare
        .get("scriptCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let pre_serialized = prepare
        .get("preSerialized")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    println!(
        "[roblox-sync-rs] {service}: prepared instances={}, scripts={}, pre_serialized={}",
        instance_count, script_count, pre_serialized
    );
    let service_property_candidates_by_class =
        parse_property_candidate_map(prepare.get("propertyCandidatesByClass"))?;
    let effective_property_candidates_by_class = if service_property_candidates_by_class.is_empty()
    {
        default_property_candidates_by_class.clone()
    } else {
        service_property_candidates_by_class
    };

    let (class_defaults, mut instance_fetch, source_by_key) =
        thread::scope(|scope| -> Result<_> {
            let class_defaults_task = scope.spawn(|| {
                let started = Instant::now();
                fetch_json_payload(chunk_size, |chunk_start, max_len| {
                    bridge.call(
                        "getClassDefaultsChunk",
                        json!({
                            "service": service,
                            "startIndex": chunk_start,
                            "maxLen": max_len,
                        }),
                    )
                })
                .map(|value| {
                    log_timing(&format!("{service}: class defaults fetch"), started);
                    value
                })
            });

            let script_paths_started = Instant::now();
            let script_paths_value = fetch_json_payload(chunk_size, |chunk_start, max_len| {
                bridge.call(
                    "getScriptPathsChunk",
                    json!({
                        "service": service,
                        "startIndex": chunk_start,
                        "maxLen": max_len,
                    }),
                )
            })?;
            log_timing(
                &format!("{service}: script path fetch"),
                script_paths_started,
            );
            let script_paths = script_paths_value
                .as_array()
                .with_context(|| format!("Invalid script path list for {service}"))?;
            let script_keys: Vec<String> = script_paths
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect();
            let source_worker_count = resolve_source_worker_count(
                source_workers,
                bridge.channel_count(),
                script_keys.len(),
            );

            println!(
                "[roblox-sync-rs] {service}: script sources={}, workers={}",
                script_keys.len(),
                source_worker_count
            );

            let script_keys_for_sources = script_keys.clone();
            let source_fetch_task = scope.spawn(move || {
                let started = Instant::now();
                fetch_script_sources(
                    bridge,
                    service,
                    chunk_size,
                    &script_keys_for_sources,
                    source_worker_count,
                )
                .map(|value| {
                    log_timing(&format!("{service}: script source fetch"), started);
                    value
                })
            });

            let instance_fetch_started = Instant::now();
            let instance_fetch = if adaptive_instance_batches {
                fetch_instances_adaptive(
                    bridge,
                    service,
                    chunk_size,
                    instance_count,
                    instance_workers,
                    cached_tune,
                    &effective_property_candidates_by_class,
                )?
            } else {
                let instance_batch_size = initial_instance_batch_size(instance_count);
                let instance_worker_count = resolve_instance_worker_count(
                    instance_workers,
                    bridge.channel_count(),
                    instance_count,
                    instance_batch_size,
                );
                println!(
                    "[roblox-sync-rs] {service}: instance batch size {} (fixed, workers={})",
                    instance_batch_size, instance_worker_count
                );
                InstanceFetchResult {
                    instances: fetch_instances_fixed(
                        bridge,
                        service,
                        chunk_size,
                        instance_count,
                        instance_batch_size,
                        instance_worker_count,
                        &effective_property_candidates_by_class,
                    )?,
                    tune: None,
                }
            };
            log_timing(
                &format!("{service}: instance fetch"),
                instance_fetch_started,
            );

            let class_defaults = match class_defaults_task.join() {
                Ok(value) => value?,
                Err(_) => bail!("Class defaults worker panicked for {service}"),
            };
            let source_by_key = match source_fetch_task.join() {
                Ok(value) => value?,
                Err(_) => bail!("Script source worker panicked for {service}"),
            };

            Ok((class_defaults, instance_fetch, source_by_key))
        })?;

    let merge_started = Instant::now();
    merge_script_sources(&mut instance_fetch.instances, &source_by_key);
    log_timing(&format!("{service}: merge script sources"), merge_started);

    let service_path = prepare
        .get("rootPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&format!("game.{service}"))
        .to_string();
    let service_class = prepare
        .get("rootClassName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(service)
        .to_string();
    let service_name = prepare
        .get("rootName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(service)
        .to_string();
    let generated_at = prepare
        .get("generatedAtUnix")
        .and_then(Value::as_i64)
        .unwrap_or(current_unix_ts());

    let release_started = Instant::now();
    let _ = bridge.call("release", json!({ "service": service }));
    log_timing(&format!("{service}: release"), release_started);
    log_timing(
        &format!("{service}: export assembly total"),
        service_started,
    );
    Ok(ExportedSnapshotParts {
        service_name,
        service_class,
        service_path,
        generated_at,
        script_count,
        class_defaults,
        instances: instance_fetch.instances,
        adaptive_tune: instance_fetch.tune,
    })
}

fn exported_parts_to_snapshot(parts: ExportedSnapshotParts) -> Value {
    json!({
        "metadata": {
            "generatedAtUnix": parts.generated_at,
            "serviceName": parts.service_name,
            "instanceCount": parts.instances.len(),
            "scriptCount": parts.script_count,
            "sourceChunked": true
        },
        "classDefaults": parts.class_defaults,
        "services": [{
            "name": parts.service_name,
            "className": parts.service_class,
            "path": parts.service_path
        }],
        "instances": parts.instances,
    })
}

fn exported_parts_to_service_state(
    service: &str,
    parts: ExportedSnapshotParts,
) -> Result<ServiceState> {
    let class_defaults_by_class = normalize_class_defaults(parts.class_defaults);
    build_service_state_from_instances(
        service,
        Some(parts.service_path),
        parts.instances,
        class_defaults_by_class,
    )
}

fn parse_bridge_chunk(value: Value) -> Result<BridgeChunk> {
    let chunk: BridgeChunk =
        serde_json::from_value(value).context("Invalid bridge chunk payload")?;
    Ok(chunk)
}

fn initial_instance_batch_size(instance_count: usize) -> usize {
    if instance_count >= 150_000 {
        1800
    } else if instance_count >= 100_000 {
        1400
    } else if instance_count >= 50_000 {
        1000
    } else if instance_count >= 20_000 {
        800
    } else {
        500
    }
}

fn adaptive_instance_batch_size(instance_count: usize) -> usize {
    if instance_count >= 150_000 {
        1800
    } else if instance_count >= 100_000 {
        1600
    } else if instance_count >= 50_000 {
        1400
    } else if instance_count >= 20_000 {
        1100
    } else {
        800
    }
}

fn auto_instance_worker_target(instance_count: usize, concurrency_cap: usize) -> usize {
    let concurrency_cap = concurrency_cap.max(1);
    let desired = if instance_count >= 20_000 {
        4
    } else if instance_count >= 5_000 {
        3
    } else if instance_count >= 250 {
        2
    } else {
        1
    };
    desired.clamp(1, concurrency_cap)
}

fn resolve_instance_worker_count(
    requested_instance_workers: usize,
    channel_count: usize,
    instance_count: usize,
    batch_size: usize,
) -> usize {
    let batch_size = batch_size.max(1);
    let batch_count = instance_count.div_ceil(batch_size).max(1);
    if batch_count <= 1 {
        return 1;
    }

    let channel_count = channel_count.max(1);
    let soft_target = auto_instance_worker_target(instance_count, channel_count);
    let hard_cap = channel_count.saturating_mul(2).clamp(2, 64);
    let cpu_cap = std::thread::available_parallelism()
        .map(|v| v.get().saturating_mul(2))
        .unwrap_or(8)
        .max(4);
    let effective_cap = hard_cap.min(cpu_cap);

    if requested_instance_workers > 0 {
        return requested_instance_workers
            .clamp(1, effective_cap.max(1))
            .min(batch_count.max(1));
    }

    soft_target
        .clamp(1, effective_cap.max(1))
        .min(batch_count.max(1))
}

fn fetch_instances_fixed(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    instance_count: usize,
    instance_batch_size: usize,
    instance_worker_count: usize,
    property_candidates_by_class: &PropertyCandidateMap,
) -> Result<Vec<SnapshotInstance>> {
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    if instance_count > 0 {
        let mut range_index = 0usize;
        let mut start = 1usize;
        while start <= instance_count {
            let take = (instance_count - start + 1).min(instance_batch_size.max(1));
            ranges.push((range_index, start, take));
            range_index += 1;
            start += take;
        }
    }

    let mut instances: Vec<SnapshotInstance> = Vec::with_capacity(instance_count);
    if ranges.is_empty() {
        println!("[roblox-sync-rs] {service}: instances 0/0");
    } else if instance_worker_count <= 1 || ranges.len() <= 1 {
        let mut total_hint = instance_count;
        for (range_idx, start_index, take_count) in ranges {
            let (hint, _, _, mut items) = fetch_instance_batch(
                bridge,
                service,
                chunk_size,
                start_index,
                take_count,
                instance_count,
                property_candidates_by_class,
            )?;
            instances.append(&mut items);
            total_hint = total_hint.max(hint);

            if (range_idx + 1) % 4 == 0
                || range_idx + 1 == total_hint.div_ceil(instance_batch_size.max(1))
            {
                println!(
                    "[roblox-sync-rs] {service}: instances {}/{}",
                    instances.len(),
                    total_hint
                );
            }
        }
        println!(
            "[roblox-sync-rs] {service}: instances {}/{}",
            instances.len(),
            total_hint
        );
    } else {
        let total_ranges = ranges.len();
        let progress_batches = std::sync::atomic::AtomicUsize::new(0);
        let progress_instances = std::sync::atomic::AtomicUsize::new(0);
        let progress_stride = (total_ranges / 12).max(1);

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(instance_worker_count)
            .build()
            .context("Failed to create instance batch worker pool")?;
        let mut fetched = pool.install(|| {
            ranges
                .par_iter()
                .map(
                    |(range_index, start_index, take_count)| -> Result<(usize, usize, f64, Vec<SnapshotInstance>)> {
                        let (total_hint, _, _, out) = fetch_instance_batch(
                            bridge,
                            service,
                            chunk_size,
                            *start_index,
                            *take_count,
                            instance_count,
                            property_candidates_by_class,
                        )?;

                        let done_batches =
                            progress_batches.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let done_instances = progress_instances
                            .fetch_add(out.len(), std::sync::atomic::Ordering::Relaxed)
                            + out.len();
                        if done_batches % progress_stride == 0 || done_batches == total_ranges {
                            println!(
                                "[roblox-sync-rs] {service}: instances {}/{} (batches {}/{})",
                                done_instances,
                                total_hint.max(instance_count),
                                done_batches,
                                total_ranges
                            );
                        }

                        Ok((*range_index, total_hint, 0.0, out))
                    },
                )
                .collect::<Result<Vec<_>>>()
        })?;

        fetched.sort_by_key(|(range_index, _, _, _)| *range_index);
        let mut total_hint = instance_count;
        for (_, hint, _, mut items) in fetched {
            total_hint = total_hint.max(hint);
            instances.append(&mut items);
        }
        println!(
            "[roblox-sync-rs] {service}: instances {}/{}",
            instances.len(),
            total_hint
        );
    }

    Ok(instances)
}

fn fetch_instances_adaptive(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    instance_count: usize,
    requested_instance_workers: usize,
    cached_tune: Option<AdaptiveTuneEntry>,
    property_candidates_by_class: &PropertyCandidateMap,
) -> Result<InstanceFetchResult> {
    if instance_count == 0 {
        println!("[roblox-sync-rs] {service}: instances 0/0");
        return Ok(InstanceFetchResult {
            instances: Vec::new(),
            tune: None,
        });
    }

    const LAG_FRAME_MS: f64 = 33.3;
    const LARGE_SERVICE_SEED_CLAMP_MIN_INSTANCES: usize = 5000;
    const INITIAL_SEED_CHUNKS_PER_BRIDGE_MIN: usize = 3;
    const INITIAL_SEED_CHUNKS_PER_BRIDGE_MAX: usize = 4;
    const BATCH_GROWTH_DIVISOR: usize = 8;
    const WORKER_GROWTH_WAVE_INTERVAL: usize = 2;
    let default_batch_size = adaptive_instance_batch_size(instance_count).max(1);
    let cached_batch_size = cached_tune
        .as_ref()
        .map(|tune| tune.batch_size)
        .filter(|value| *value > 0);
    let cached_or_default_batch_size = cached_batch_size.unwrap_or(default_batch_size);
    let bridge_concurrency = bridge.channel_count().max(1);
    let default_worker_target = auto_instance_worker_target(instance_count, bridge_concurrency);
    let cached_worker_target = cached_tune
        .as_ref()
        .map(|tune| tune.workers)
        .filter(|value| *value > 0);
    let seed_source = if requested_instance_workers > 0 {
        "manual"
    } else if cached_tune.is_some() {
        "cached"
    } else {
        "default"
    };
    let mut worker_target = if requested_instance_workers > 0 {
        requested_instance_workers
    } else if let Some(workers) = cached_worker_target {
        workers.max(default_worker_target)
    } else {
        default_worker_target
    };
    worker_target = worker_target.clamp(1, bridge_concurrency);
    let mut batch_size = cached_or_default_batch_size.min(instance_count.max(1));
    let mut seed_reason = if requested_instance_workers > 0 {
        "manual workers"
    } else if cached_tune.is_some() {
        "cached tune"
    } else {
        "default sizing"
    };

    if instance_count >= LARGE_SERVICE_SEED_CLAMP_MIN_INSTANCES {
        let max_total_chunks = bridge_concurrency
            .saturating_mul(INITIAL_SEED_CHUNKS_PER_BRIDGE_MAX)
            .clamp(1, 16);
        let min_total_chunks = bridge_concurrency
            .saturating_mul(INITIAL_SEED_CHUNKS_PER_BRIDGE_MIN)
            .clamp(1, 12);
        let min_seed_batch = instance_count.div_ceil(max_total_chunks).max(1);
        let max_seed_batch = instance_count.div_ceil(min_total_chunks).max(1);
        batch_size = batch_size.clamp(min_seed_batch, max_seed_batch);
        if batch_size != cached_or_default_batch_size {
            seed_reason = "bridge seed window";
        }
    }

    if batch_size < cached_or_default_batch_size {
        println!(
            "[roblox-sync-rs] {service}: clamped adaptive seed batch from {} to {} using {} bridge channels",
            cached_or_default_batch_size, batch_size, bridge_concurrency
        );
    } else if batch_size > cached_or_default_batch_size {
        println!(
            "[roblox-sync-rs] {service}: raised adaptive seed batch from {} to {} using {} bridge channels",
            cached_or_default_batch_size, batch_size, bridge_concurrency
        );
    }

    println!(
        "[roblox-sync-rs] {service}: adaptive seed source={} cached_batch={} cached_workers={} default_batch={} auto_workers={} final_batch={} final_workers={} reason={} lag_frame_ms={:.1}",
        seed_source,
        cached_batch_size
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        cached_worker_target
            .map(|value| value.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        default_batch_size,
        default_worker_target,
        batch_size,
        worker_target,
        seed_reason,
        LAG_FRAME_MS
    );

    let mut next_start = 1usize;
    let mut total_hint = instance_count;
    let mut wave_index = 0usize;
    let mut instances = Vec::with_capacity(instance_count);
    let mut suggested_batch_size = batch_size;
    let mut suggested_workers = worker_target;
    let mut last_frame_ms = None;

    while next_start <= total_hint {
        let remaining = total_hint - next_start + 1;
        let remaining_ranges = remaining.div_ceil(batch_size).max(1);
        let wave_workers = worker_target.max(1).min(remaining_ranges);
        let mut ranges: Vec<(usize, usize, usize)> = Vec::with_capacity(wave_workers);

        for range_index in 0..wave_workers {
            if next_start > total_hint {
                break;
            }
            let take = (total_hint - next_start + 1).min(batch_size);
            ranges.push((range_index, next_start, take));
            next_start += take;
        }

        let wave_started = Instant::now();
        let mut fetched = if ranges.len() <= 1 {
            let mut out = Vec::with_capacity(ranges.len());
            for (range_index, start_index, take_count) in &ranges {
                let (hint, bytes, request_ms, items) = fetch_instance_batch(
                    bridge,
                    service,
                    chunk_size,
                    *start_index,
                    *take_count,
                    instance_count,
                    property_candidates_by_class,
                )?;
                out.push((*range_index, hint, bytes, request_ms, items));
            }
            out
        } else {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(ranges.len())
                .build()
                .context("Failed to create adaptive instance batch worker pool")?;
            pool.install(|| {
                ranges
                    .par_iter()
                    .map(
                        |(range_index, start_index, take_count)| -> Result<(usize, usize, usize, f64, Vec<SnapshotInstance>)> {
                            let (hint, bytes, request_ms, items) = fetch_instance_batch(
                                bridge,
                                service,
                                chunk_size,
                                *start_index,
                                *take_count,
                                instance_count,
                                property_candidates_by_class,
                            )?;
                            Ok((*range_index, hint, bytes, request_ms, items))
                        },
                    )
                    .collect::<Result<Vec<_>>>()
            })?
        };

        fetched.sort_by_key(|(range_index, _, _, _, _)| *range_index);
        let wave_bytes: usize = fetched.iter().map(|(_, _, bytes, _, _)| *bytes).sum();
        let wave_requests = fetched.len();
        let total_request_ms: f64 = fetched
            .iter()
            .map(|(_, _, _, request_ms, _)| *request_ms)
            .sum();
        let max_request_ms = fetched
            .iter()
            .map(|(_, _, _, request_ms, _)| *request_ms)
            .fold(0.0, f64::max);
        let avg_request_ms = if wave_requests > 0 {
            total_request_ms / wave_requests as f64
        } else {
            0.0
        };
        let max_request_bytes = fetched
            .iter()
            .map(|(_, _, bytes, _, _)| *bytes)
            .max()
            .unwrap_or(0);
        let avg_request_bytes = if wave_requests > 0 {
            wave_bytes as f64 / wave_requests as f64
        } else {
            0.0
        };
        for (_, hint, _, _, mut items) in fetched {
            total_hint = total_hint.max(hint);
            instances.append(&mut items);
        }

        wave_index += 1;
        let wave_ms = wave_started.elapsed().as_secs_f64() * 1000.0;
        let frame_ms = read_bridge_frame_ms(bridge);
        last_frame_ms = frame_ms;
        let lagging = frame_ms.map(|ms| ms >= LAG_FRAME_MS).unwrap_or(false);
        println!(
            "[roblox-sync-rs] {service}: adaptive wave {} -> instances {}/{} (batch={}, workers={}, wave_ms={:.0}, bytes={:.1}MB, frame_ms={})",
            wave_index,
            instances.len(),
            total_hint,
            batch_size,
            wave_workers,
            wave_ms,
            wave_bytes as f64 / (1024.0 * 1024.0),
            format_frame_ms(frame_ms)
        );
        println!(
            "[roblox-sync-rs] {service}: adaptive wave {} request stats -> requests={}, avg_req_ms={:.1}, max_req_ms={:.1}, avg_req_mb={:.1}, max_req_mb={:.1}",
            wave_index,
            wave_requests,
            avg_request_ms,
            max_request_ms,
            avg_request_bytes / (1024.0 * 1024.0),
            max_request_bytes as f64 / (1024.0 * 1024.0)
        );

        if lagging {
            batch_size = batch_size.saturating_mul(3).div_ceil(4).max(1);
            worker_target = worker_target.saturating_mul(3).div_ceil(4).max(1);
        } else {
            let batch_step = (batch_size / BATCH_GROWTH_DIVISOR).max(1);
            batch_size = batch_size.saturating_add(batch_step).min(total_hint.max(1));
            if wave_index % WORKER_GROWTH_WAVE_INTERVAL == 0 {
                worker_target = worker_target.saturating_add(1);
            }
        }
        suggested_batch_size = batch_size;
        suggested_workers = worker_target;
    }

    println!(
        "[roblox-sync-rs] {service}: instances {}/{}",
        instances.len(),
        total_hint
    );
    Ok(InstanceFetchResult {
        instances,
        tune: Some(AdaptiveTuneEntry {
            batch_size: suggested_batch_size,
            workers: suggested_workers,
            instance_count: total_hint,
            frame_ms: last_frame_ms,
            updated_at_unix: current_unix_ts(),
        }),
    })
}

fn fetch_instance_batch(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    start_index: usize,
    take_count: usize,
    instance_count: usize,
    property_candidates_by_class: &PropertyCandidateMap,
) -> Result<(usize, usize, f64, Vec<SnapshotInstance>)> {
    let started = Instant::now();
    let (mut batch_value, bytes) =
        fetch_json_payload_with_size(chunk_size, |chunk_start, max_len| {
            bridge.call(
                "getInstanceBatchCompactChunk",
                json!({
                    "service": service,
                    "startIndex": start_index,
                    "maxCount": take_count,
                    "chunkStart": chunk_start,
                    "maxLen": max_len,
                }),
            )
        })?;

    let items = batch_value
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .with_context(|| format!("Invalid instance batch payload for {service}"))?;
    let raw_items = Value::Array(items.drain(..).collect());
    let out = match batch_value.get("format").and_then(Value::as_str) {
        Some("compact") | Some("compact-v2") => {
            parse_compact_instance_items(raw_items, property_candidates_by_class).with_context(
                || format!("Invalid compact instance batch item schema for {service}"),
            )?
        }
        Some("compact-v1") => {
            parse_legacy_compact_instance_items(raw_items).with_context(|| {
                format!("Invalid legacy compact instance batch item schema for {service}")
            })?
        }
        _ => serde_json::from_value(raw_items)
            .with_context(|| format!("Invalid instance batch item schema for {service}"))?,
    };

    let total_hint = batch_value
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(instance_count as u64) as usize;

    Ok((total_hint, bytes, elapsed_ms(started), out))
}

fn compact_string(items: &[Value], index: usize) -> String {
    items
        .get(index)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn compact_optional_string(items: &[Value], index: usize) -> Option<String> {
    items
        .get(index)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn compact_string_vec(items: &[Value], index: usize) -> Vec<String> {
    items
        .get(index)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn compact_map(items: &[Value], index: usize) -> Map<String, Value> {
    items
        .get(index)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn compact_property_name_for_id(
    class_name: &str,
    property_id: usize,
    property_candidates_by_class: &PropertyCandidateMap,
) -> Option<String> {
    if let Some(property_names) = property_candidates_by_class.get(class_name) {
        return property_names.get(property_id).cloned();
    }
    DEFAULT_PROPERTY_CANDIDATES
        .get(property_id)
        .map(|name| (*name).to_string())
}

fn compact_properties(
    items: &[Value],
    index: usize,
    class_name: &str,
    property_candidates_by_class: &PropertyCandidateMap,
) -> Result<Map<String, Value>> {
    let Some(value) = items.get(index) else {
        return Ok(Map::new());
    };

    if let Some(map) = value.as_object() {
        return Ok(map.clone());
    }

    let Some(pairs) = value.as_array() else {
        return Ok(Map::new());
    };

    if pairs.len() % 2 != 0 {
        bail!("Compact property pairs must have even length");
    }

    let mut out = Map::new();
    for pair in pairs.chunks_exact(2) {
        let property_name = match &pair[0] {
            Value::Number(number) => {
                let property_id = number
                    .as_u64()
                    .with_context(|| "Compact property id must be a non-negative integer")?
                    as usize;
                compact_property_name_for_id(class_name, property_id, property_candidates_by_class)
                    .with_context(|| {
                        format!("Unknown compact property id {property_id} for class {class_name}")
                    })?
            }
            Value::String(name) => name.clone(),
            _ => bail!("Compact property key must be a number or string"),
        };
        out.insert(property_name, pair[1].clone());
    }

    Ok(out)
}

fn parse_legacy_compact_instance_items(raw_items: Value) -> Result<Vec<SnapshotInstance>> {
    let values = raw_items
        .as_array()
        .with_context(|| "Legacy compact instance items must be an array")?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let items = value
            .as_array()
            .with_context(|| "Legacy compact instance item must be an array")?;
        out.push(SnapshotInstance {
            path: compact_string(items, 0),
            path_segments: compact_string_vec(items, 1),
            name: compact_string(items, 2),
            class_name: compact_string(items, 3),
            properties: compact_map(items, 4),
            source_key: compact_optional_string(items, 5),
            parent_path: compact_optional_string(items, 6),
            attributes: compact_map(items, 7),
            debug_id: compact_optional_string(items, 8),
            parent_debug_id: compact_optional_string(items, 9),
            instance_id: compact_optional_string(items, 10),
            parent_instance_id: compact_optional_string(items, 11),
        });
    }
    Ok(out)
}

fn parse_compact_instance_items(
    raw_items: Value,
    property_candidates_by_class: &PropertyCandidateMap,
) -> Result<Vec<SnapshotInstance>> {
    let values = raw_items
        .as_array()
        .with_context(|| "Compact instance items must be an array")?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let items = value
            .as_array()
            .with_context(|| "Compact instance item must be an array")?;
        if items.first().is_some_and(Value::is_array) {
            let path_segments = compact_string_vec(items, 0);
            let name = path_segments.last().cloned().unwrap_or_default();
            let path = path_segments.join(".");
            let parent_path = if path_segments.len() > 1 {
                Some(path_segments[..path_segments.len() - 1].join("."))
            } else {
                None
            };
            let class_name = compact_string(items, 1);
            out.push(SnapshotInstance {
                path,
                path_segments,
                name,
                class_name: class_name.clone(),
                properties: compact_properties(
                    items,
                    2,
                    class_name.as_str(),
                    property_candidates_by_class,
                )?,
                source_key: compact_optional_string(items, 3),
                parent_path,
                attributes: compact_map(items, 4),
                debug_id: None,
                parent_debug_id: None,
                instance_id: compact_optional_string(items, 5),
                parent_instance_id: compact_optional_string(items, 6),
            });
        } else {
            let class_name = compact_string(items, 1);
            out.push(SnapshotInstance {
                path: String::new(),
                path_segments: Vec::new(),
                name: compact_string(items, 0),
                class_name: class_name.clone(),
                properties: compact_properties(
                    items,
                    2,
                    class_name.as_str(),
                    property_candidates_by_class,
                )?,
                source_key: compact_optional_string(items, 3),
                parent_path: None,
                attributes: compact_map(items, 4),
                debug_id: None,
                parent_debug_id: None,
                instance_id: compact_optional_string(items, 5),
                parent_instance_id: compact_optional_string(items, 6),
            });
        }
    }
    Ok(out)
}

fn read_bridge_frame_ms(bridge: &BridgeServer) -> Option<f64> {
    bridge
        .call("getPerformanceStats", json!({}))
        .ok()
        .and_then(|value| value.get("frameMs").and_then(Value::as_f64))
}

fn format_frame_ms(frame_ms: Option<f64>) -> String {
    frame_ms
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn fetch_text_chunks<F>(chunk_size: usize, mut fetcher: F) -> Result<String>
where
    F: FnMut(usize, usize) -> Result<BridgeChunk>,
{
    let mut output = String::new();
    let mut start = 1usize;
    let max_len = chunk_size.max(256);
    loop {
        let chunk = fetcher(start, max_len)?;
        output.push_str(&chunk.chunk);

        if chunk.total == 0 || chunk.next_start <= start || chunk.next_start > chunk.total {
            break;
        }
        start = chunk.next_start;
    }
    Ok(output)
}

fn fetch_json_payload<F>(chunk_size: usize, mut fetcher: F) -> Result<Value>
where
    F: FnMut(usize, usize) -> Result<Value>,
{
    let (value, _) = fetch_json_payload_with_size(chunk_size, &mut fetcher)?;
    Ok(value)
}

fn fetch_json_payload_with_size<F>(chunk_size: usize, mut fetcher: F) -> Result<(Value, usize)>
where
    F: FnMut(usize, usize) -> Result<Value>,
{
    let text = fetch_text_chunks(chunk_size, |start, max_len| {
        let value = fetcher(start, max_len)?;
        parse_bridge_chunk(value)
    })?;
    let size = text.len();
    let value = serde_json::from_str(&text).context("Invalid chunked JSON payload")?;
    Ok((value, size))
}

fn merge_script_sources(
    instances: &mut [SnapshotInstance],
    source_by_key: &HashMap<String, String>,
) {
    for instance in instances.iter_mut() {
        let source = instance
            .source_key
            .as_deref()
            .and_then(|source_key| source_by_key.get(source_key))
            .or_else(|| {
                instance
                    .instance_id
                    .as_deref()
                    .and_then(|instance_id| source_by_key.get(&format!("id:{instance_id}")))
            })
            .or_else(|| {
                instance
                    .debug_id
                    .as_deref()
                    .and_then(|debug_id| source_by_key.get(&format!("debug:{debug_id}")))
            })
            .or_else(|| source_by_key.get(&instance.path));
        let Some(source) = source else {
            continue;
        };
        let is_placeholder = instance
            .properties
            .get("Source")
            .and_then(Value::as_str)
            .map(|v| v == "__SOURCE_EXTERNAL__")
            .unwrap_or(false);
        if is_placeholder {
            instance
                .properties
                .insert("Source".to_string(), Value::String(source.clone()));
        }
        instance.source_key = None;
    }
}

fn write_snapshot_file(snapshot_dir: &Path, service: &str, snapshot: &Value) -> Result<()> {
    let path = snapshot_dir.join(format!("{service}.json"));
    write_json_file(&path, snapshot, true)?;
    println!("[roblox-sync-rs] wrote {}", path.display());
    Ok(())
}

fn import_service_state_with_sourcemap(
    state: &ServiceState,
    project_root: &Path,
    src_root: &Path,
    service: &str,
    compact_meta_json: bool,
) -> Result<SourcemapNode> {
    let import_started = Instant::now();
    let node = import_service_state(state, project_root, src_root, service, compact_meta_json)?;
    log_timing(&format!("{service}: write src tree"), import_started);
    log_timing(&format!("{service}: build sourcemap node"), import_started);
    log_timing(&format!("{service}: import service total"), import_started);
    Ok(node)
}

#[allow(dead_code)]
fn import_service_snapshot_value(
    project_root: &Path,
    service: &str,
    snapshot: Value,
    compact_meta_json: bool,
    write_project: bool,
) -> Result<SourcemapNode> {
    let manifest: SnapshotManifest =
        serde_json::from_value(snapshot).context("Invalid snapshot value for import")?;
    let (instances, root_path_from_manifest, class_defaults_by_class) =
        collect_manifest_instances(manifest, None, service)?;
    let state = build_service_state_from_instances(
        service,
        root_path_from_manifest,
        instances,
        class_defaults_by_class,
    )?;
    let src_root = project_root.join("src");
    fs::create_dir_all(&src_root)
        .with_context(|| format!("Failed to create {}", src_root.display()))?;
    let sourcemap_node = import_service_state_with_sourcemap(
        &state,
        project_root,
        &src_root,
        service,
        compact_meta_json,
    )?;
    if write_project {
        write_generated_project(project_root, &[service.to_string()], compact_meta_json)?;
        let mut sourcemap_nodes = HashMap::new();
        sourcemap_nodes.insert(service.to_string(), sourcemap_node.clone());
        write_project_sourcemap_from_service_nodes(project_root, &sourcemap_nodes)?;
    }
    Ok(sourcemap_node)
}

fn current_unix_ts() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(v) => v.as_secs() as i64,
        Err(_) => 0,
    }
}

fn import_snapshots(args: ImportSnapshotsArgs) -> Result<()> {
    let project_root = args.project_root.canonicalize().with_context(|| {
        format!(
            "Failed to resolve project root: {}",
            args.project_root.display()
        )
    })?;
    let snapshot_dir = args.snapshot_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve snapshot directory: {}",
            args.snapshot_dir.display()
        )
    })?;
    let src_root = project_root.join("src");
    fs::create_dir_all(&src_root)
        .with_context(|| format!("Failed to create {}", src_root.display()))?;

    let services = parse_services(&args.services)?;
    let thread_count = resolve_thread_count(args.threads, services.len());
    println!(
        "[roblox-sync-rs] import-snapshots start: services={}, threads={}",
        services.len(),
        thread_count
    );
    let mut sourcemap_nodes: HashMap<String, SourcemapNode> = HashMap::new();

    if thread_count <= 1 || services.len() <= 1 {
        for service in &services {
            println!("[roblox-sync-rs] {service}: loading snapshot");
            let state = load_service_state(&snapshot_dir, service)?;
            println!("[roblox-sync-rs] {service}: writing src tree");
            let node = import_service_state_with_sourcemap(
                &state,
                &project_root,
                &src_root,
                service,
                args.compact_meta_json,
            )?;
            sourcemap_nodes.insert(service.clone(), node);
            println!("[roblox-sync-rs] {service}: done");
        }
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .build()
            .context("Failed to build thread pool")?;
        let shared_nodes = Arc::new(Mutex::new(HashMap::<String, SourcemapNode>::new()));
        pool.install(|| -> Result<()> {
            services.par_iter().try_for_each(|service| -> Result<()> {
                println!("[roblox-sync-rs] {service}: loading snapshot");
                let state = load_service_state(&snapshot_dir, service)?;
                println!("[roblox-sync-rs] {service}: writing src tree");
                let node = import_service_state_with_sourcemap(
                    &state,
                    &project_root,
                    &src_root,
                    service,
                    args.compact_meta_json,
                )?;
                let mut nodes = match shared_nodes.lock() {
                    Ok(lock) => lock,
                    Err(poisoned) => poisoned.into_inner(),
                };
                nodes.insert(service.clone(), node);
                println!("[roblox-sync-rs] {service}: done");
                Ok(())
            })
        })?;
        let drained_nodes = match shared_nodes.lock() {
            Ok(lock) => lock.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        sourcemap_nodes.extend(drained_nodes);
    }

    if !args.no_project_write {
        println!("[roblox-sync-rs] writing default.project.generated.json");
        write_generated_project(&project_root, &services, args.compact_meta_json)?;
    }

    let sourcemap_result = if args.no_project_write {
        write_project_sourcemap_with_updates(&project_root, &sourcemap_nodes)
    } else {
        write_project_sourcemap_from_service_nodes(&project_root, &sourcemap_nodes)
    };
    if let Err(err) = sourcemap_result {
        println!("[roblox-sync-rs] warning: {err}");
    }

    println!("[roblox-sync-rs] import-snapshots done");
    Ok(())
}

fn import_service(args: ImportServiceArgs) -> Result<()> {
    let project_root = args.project_root.canonicalize().with_context(|| {
        format!(
            "Failed to resolve project root: {}",
            args.project_root.display()
        )
    })?;
    let src_root = project_root.join("src");
    fs::create_dir_all(&src_root)
        .with_context(|| format!("Failed to create {}", src_root.display()))?;

    let service = parse_single_service(&args.service)?;
    println!("[roblox-sync-rs] import-service start: {service}");

    let payload = read_snapshot_payload(args.snapshot_file.as_deref())?;
    let manifest: SnapshotManifest =
        serde_json::from_str(&payload).context("Invalid snapshot JSON payload")?;

    let snapshot_dir_for_chunks = args
        .snapshot_file
        .as_deref()
        .and_then(|path| path.parent().map(Path::to_path_buf));

    let (instances, root_path_from_manifest, class_defaults_by_class) =
        collect_manifest_instances(manifest, snapshot_dir_for_chunks.as_deref(), &service)?;
    let state = build_service_state_from_instances(
        &service,
        root_path_from_manifest,
        instances,
        class_defaults_by_class,
    )?;
    let node = import_service_state_with_sourcemap(
        &state,
        &project_root,
        &src_root,
        &service,
        args.compact_meta_json,
    )?;
    let mut sourcemap_nodes = HashMap::new();
    sourcemap_nodes.insert(service.clone(), node);

    if !args.no_project_write {
        write_generated_project(&project_root, &[service.clone()], args.compact_meta_json)?;
    }
    let sourcemap_result = if args.no_project_write {
        write_project_sourcemap_with_updates(&project_root, &sourcemap_nodes)
    } else {
        write_project_sourcemap_from_service_nodes(&project_root, &sourcemap_nodes)
    };
    if let Err(err) = sourcemap_result {
        println!("[roblox-sync-rs] warning: {err}");
    }

    println!("[roblox-sync-rs] import-service done: {service}");
    Ok(())
}

fn parse_services(raw: &str) -> Result<Vec<String>> {
    if raw.trim().is_empty() {
        return Ok(DEFAULT_SERVICES.iter().map(|s| (*s).to_string()).collect());
    }

    let allowed: HashMap<&str, ()> = DEFAULT_SERVICES.iter().map(|s| (*s, ())).collect();
    let mut out = Vec::new();
    for token in raw.split(',') {
        let service = token.trim();
        if service.is_empty() {
            continue;
        }
        if !allowed.contains_key(service) {
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

fn direct_import_export_order(services: &[String]) -> Vec<String> {
    const PRIORITY: [&str; 3] = ["ServerStorage", "Workspace", "ReplicatedStorage"];
    let mut out = Vec::with_capacity(services.len());
    for service in PRIORITY {
        if services.iter().any(|candidate| candidate == service) {
            out.push(service.to_string());
        }
    }
    for service in services {
        if !out.iter().any(|candidate| candidate == service) {
            out.push(service.clone());
        }
    }
    out
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
        return requested.max(1).min(service_count);
    }
    match std::thread::available_parallelism() {
        Ok(value) => value.get().max(1).min(service_count),
        Err(_) => 1,
    }
}

fn resolve_source_worker_count(
    requested: usize,
    channel_count: usize,
    script_count: usize,
) -> usize {
    if script_count <= 1 {
        return 1;
    }

    let channel_count = channel_count.max(1);
    let soft_target = channel_count.max(1);
    let hard_cap = channel_count.saturating_mul(2).clamp(2, 64);
    let cpu_cap = std::thread::available_parallelism()
        .map(|v| v.get().saturating_mul(2))
        .unwrap_or(8)
        .max(4);
    let effective_cap = hard_cap.min(cpu_cap).min(script_count.max(1));

    if requested > 0 {
        return requested.clamp(1, effective_cap.max(1));
    }

    soft_target.clamp(1, effective_cap.max(1))
}

fn fetch_script_sources(
    bridge: &BridgeServer,
    service: &str,
    chunk_size: usize,
    script_keys: &[String],
    source_worker_count: usize,
) -> Result<HashMap<String, String>> {
    const SOURCE_BATCH_SIZE: usize = 16;
    let mut source_by_key: HashMap<String, String> = HashMap::new();
    let source_batches: Vec<Vec<String>> = script_keys
        .chunks(SOURCE_BATCH_SIZE)
        .map(|chunk| chunk.to_vec())
        .collect();
    if script_keys.len() <= 1 || source_worker_count <= 1 {
        let mut loaded_scripts = 0usize;
        for batch in &source_batches {
            println!(
                "[roblox-sync-rs] {service}: script {}/{}",
                loaded_scripts + 1,
                script_keys.len()
            );
            let encoded = fetch_text_chunks(chunk_size, |chunk_start, max_len| {
                let value = bridge.call(
                    "getSourceBatchChunk",
                    json!({
                        "service": service,
                        "instancePaths": batch,
                        "startIndex": chunk_start,
                        "maxLen": max_len,
                    }),
                )?;
                parse_bridge_chunk(value)
            })?;
            let fetched: HashMap<String, String> =
                serde_json::from_str(&encoded).context("Invalid batched source payload")?;
            loaded_scripts += batch.len();
            source_by_key.extend(fetched);
        }
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(source_worker_count)
            .build()
            .context("Failed to create script source worker pool")?;
        let fetched = pool.install(|| {
            source_batches
                .par_iter()
                .enumerate()
                .map(|(index, batch)| -> Result<HashMap<String, String>> {
                    let progress_scripts = ((index + 1) * SOURCE_BATCH_SIZE).min(script_keys.len());
                    if index == 0 || progress_scripts == script_keys.len() || (index + 1) % 2 == 0 {
                        println!(
                            "[roblox-sync-rs] {service}: script {}/{}",
                            progress_scripts,
                            script_keys.len()
                        );
                    }
                    let encoded = fetch_text_chunks(chunk_size, |chunk_start, max_len| {
                        let value = bridge.call(
                            "getSourceBatchChunk",
                            json!({
                                "service": service,
                                "instancePaths": batch,
                                "startIndex": chunk_start,
                                "maxLen": max_len,
                            }),
                        )?;
                        parse_bridge_chunk(value)
                    })?;
                    serde_json::from_str(&encoded).context("Invalid batched source payload")
                })
                .collect::<Result<Vec<_>>>()
        })?;

        for batch_sources in fetched {
            source_by_key.extend(batch_sources);
        }
    }

    Ok(source_by_key)
}

fn resolve_direct_import_workers(requested: usize) -> usize {
    if requested > 0 {
        let cpu_cap = std::thread::available_parallelism()
            .map(|v| v.get())
            .unwrap_or(4)
            .max(2);
        return requested.clamp(1, cpu_cap.clamp(2, 16));
    }
    std::thread::available_parallelism()
        .map(|v| v.get().clamp(2, 16))
        .unwrap_or(4)
}

fn load_service_state(snapshot_dir: &Path, service: &str) -> Result<ServiceState> {
    let snapshot_path = snapshot_dir.join(format!("{service}.json"));
    let manifest: SnapshotManifest = read_json_file(&snapshot_path)
        .with_context(|| format!("Failed to read snapshot: {}", snapshot_path.display()))?;

    let (instances, root_path_from_manifest, class_defaults_by_class) =
        collect_manifest_instances(manifest, Some(snapshot_dir), service)?;
    build_service_state_from_instances(
        service,
        root_path_from_manifest,
        instances,
        class_defaults_by_class,
    )
}

fn collect_manifest_instances(
    manifest: SnapshotManifest,
    snapshot_dir: Option<&Path>,
    service: &str,
) -> Result<(
    Vec<SnapshotInstance>,
    Option<String>,
    HashMap<String, Map<String, Value>>,
)> {
    let mut instances: Vec<SnapshotInstance> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();

    for instance in manifest.instances {
        add_instance_if_new(instance, &mut instances, &mut seen_keys);
    }

    for chunk_entry in manifest.instance_chunks {
        let file_name = match &chunk_entry {
            InstanceChunkEntry::FileName(name) => name.as_str(),
            InstanceChunkEntry::Entry { file } => file.as_str(),
        };
        if file_name.trim().is_empty() {
            bail!("Snapshot chunk entry missing file for service {service}");
        }

        let Some(base_dir) = snapshot_dir else {
            bail!(
                "Snapshot payload for {service} references chunk files but no snapshot directory is available"
            );
        };
        let chunk_path = base_dir.join(file_name);
        let chunk_text = fs::read_to_string(&chunk_path)
            .with_context(|| format!("Failed to read chunk: {}", chunk_path.display()))?;
        let chunk_instances = parse_chunk_instances(&chunk_text)
            .with_context(|| format!("Invalid chunk JSON in {}", chunk_path.display()))?;

        for instance in chunk_instances {
            add_instance_if_new(instance, &mut instances, &mut seen_keys);
        }
    }

    if instances.is_empty() {
        bail!("Snapshot has no instances for service {service}");
    }

    let root_path_from_manifest = manifest
        .services
        .first()
        .and_then(|svc| svc.path.as_ref())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let class_defaults_by_class = normalize_class_defaults(manifest.class_defaults);

    Ok((instances, root_path_from_manifest, class_defaults_by_class))
}

fn build_service_state_from_instances(
    service: &str,
    root_path_from_manifest: Option<String>,
    mut instances: Vec<SnapshotInstance>,
    class_defaults_by_class: HashMap<String, Map<String, Value>>,
) -> Result<ServiceState> {
    let mut children_by_parent_instance_id: HashMap<String, Vec<usize>> = HashMap::new();
    let mut children_by_parent_debug: HashMap<String, Vec<usize>> = HashMap::new();
    let mut index_by_instance_id: HashMap<String, usize> = HashMap::new();
    let mut index_by_debug_id: HashMap<String, usize> = HashMap::new();

    for (index, instance) in instances.iter().enumerate() {
        if let Some(instance_id) = instance.instance_id.as_deref().filter(|s| !s.is_empty()) {
            index_by_instance_id
                .entry(instance_id.to_string())
                .or_insert(index);
        }
        if let Some(debug_id) = instance.debug_id.as_deref().filter(|s| !s.is_empty()) {
            index_by_debug_id
                .entry(debug_id.to_string())
                .or_insert(index);
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
        root_path_from_manifest.as_deref(),
        &instances,
        &index_by_instance_id,
    )
    .with_context(|| {
        format!(
            "Snapshot missing root service instance: {service} (manifest root: {})",
            root_path_from_manifest.as_deref().unwrap_or("n/a")
        )
    })?;

    let needs_path_rebuild = instances
        .iter()
        .any(|instance| instance.path.is_empty() || instance.path_segments.is_empty());
    if needs_path_rebuild && !children_by_parent_instance_id.is_empty() {
        rebuild_instance_paths_from_ids(
            service,
            service_root_index,
            &children_by_parent_instance_id,
            &mut instances,
        );
    }

    let mut children_by_parent_path: HashMap<String, Vec<usize>> = HashMap::new();
    let mut index_by_path: HashMap<String, usize> = HashMap::new();
    let mut index_by_path_segments: HashMap<String, usize> = HashMap::new();

    for (index, instance) in instances.iter().enumerate() {
        if !instance.path.is_empty() {
            index_by_path.entry(instance.path.clone()).or_insert(index);
        }
        if !instance.path_segments.is_empty() {
            index_by_path_segments
                .entry(path_segments_key(&instance.path_segments))
                .or_insert(index);
        }

        let parent_path = instance
            .parent_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| derive_parent_path(&instance.path));

        if let Some(parent_path) = parent_path {
            children_by_parent_path
                .entry(parent_path)
                .or_default()
                .push(index);
        }
    }

    let rojo_ref_ids_by_index = collect_rojo_ref_ids(
        &instances,
        &index_by_instance_id,
        &index_by_debug_id,
        &index_by_path,
        &index_by_path_segments,
    );

    Ok(ServiceState {
        instances,
        children_by_parent_instance_id,
        children_by_parent_path,
        children_by_parent_debug,
        index_by_instance_id,
        index_by_debug_id,
        index_by_path,
        index_by_path_segments,
        rojo_ref_ids_by_index,
        service_root_index,
        class_defaults_by_class,
    })
}

fn find_service_root_index(
    service: &str,
    root_path_from_manifest: Option<&str>,
    instances: &[SnapshotInstance],
    index_by_instance_id: &HashMap<String, usize>,
) -> Option<usize> {
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
            instance
                .parent_instance_id
                .as_deref()
                .unwrap_or("")
                .is_empty()
                && (instance.name == service || instance.path == service)
        })
        .or_else(|| {
            instances.iter().position(|instance| {
                instance
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

    while let Some((index, proposed_segments)) = stack.pop() {
        if index >= instances.len() {
            continue;
        }

        let effective_segments = if instances[index].path_segments.is_empty() {
            proposed_segments
        } else {
            instances[index].path_segments.clone()
        };
        let path = effective_segments.join(".");

        if instances[index].path_segments.is_empty() {
            instances[index].path_segments = effective_segments.clone();
        }
        if instances[index].path.is_empty() {
            instances[index].path = path.clone();
        }
        if index != root_index
            && instances[index]
                .parent_path
                .as_deref()
                .unwrap_or("")
                .is_empty()
        {
            instances[index].parent_path = if effective_segments.len() > 1 {
                Some(effective_segments[..effective_segments.len() - 1].join("."))
            } else {
                None
            };
        }

        let Some(instance_id) = instances[index]
            .instance_id
            .as_deref()
            .filter(|value| !value.is_empty())
        else {
            continue;
        };

        let Some(child_indices) = children_by_parent_instance_id.get(instance_id) else {
            continue;
        };

        for child_index in child_indices.iter().rev() {
            if *child_index >= instances.len() {
                continue;
            }
            let child_name = if instances[*child_index].name.is_empty() {
                instances[*child_index].class_name.clone()
            } else {
                instances[*child_index].name.clone()
            };
            let mut child_segments = effective_segments.clone();
            child_segments.push(child_name);
            stack.push((*child_index, child_segments));
        }
    }
}

fn path_segments_key(segments: &[String]) -> String {
    let mut key = String::new();
    for segment in segments {
        key.push_str(&segment.len().to_string());
        key.push(':');
        key.push_str(segment);
        key.push('|');
    }
    key
}

fn ref_payload_object(value: &Value) -> Option<&Map<String, Value>> {
    let obj = value.as_object()?;
    if obj.get("_type").and_then(Value::as_str) == Some("Ref") {
        return Some(obj);
    }
    obj.get("Ref").and_then(Value::as_object)
}

fn resolve_ref_target_index_from_maps(
    raw_value: &Value,
    index_by_instance_id: &HashMap<String, usize>,
    index_by_debug_id: &HashMap<String, usize>,
    index_by_path: &HashMap<String, usize>,
    index_by_path_segments: &HashMap<String, usize>,
) -> Option<usize> {
    let obj = ref_payload_object(raw_value)?;

    if let Some(instance_id) = obj.get("instanceId").and_then(Value::as_str) {
        if let Some(index) = index_by_instance_id.get(instance_id) {
            return Some(*index);
        }
    }

    if let Some(debug_id) = obj.get("debugId").and_then(Value::as_str) {
        if let Some(index) = index_by_debug_id.get(debug_id) {
            return Some(*index);
        }
    }

    if let Some(path_segments) = obj.get("pathSegments").and_then(Value::as_array) {
        let segments: Vec<String> = path_segments
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        if !segments.is_empty() {
            if let Some(index) = index_by_path_segments.get(&path_segments_key(&segments)) {
                return Some(*index);
            }
        }
    }

    obj.get("path")
        .and_then(Value::as_str)
        .and_then(|path| index_by_path.get(path).copied())
}

fn resolve_ref_target_index(state: &ServiceState, raw_value: &Value) -> Option<usize> {
    resolve_ref_target_index_from_maps(
        raw_value,
        &state.index_by_instance_id,
        &state.index_by_debug_id,
        &state.index_by_path,
        &state.index_by_path_segments,
    )
}

fn collect_rojo_ref_ids(
    instances: &[SnapshotInstance],
    index_by_instance_id: &HashMap<String, usize>,
    index_by_debug_id: &HashMap<String, usize>,
    index_by_path: &HashMap<String, usize>,
    index_by_path_segments: &HashMap<String, usize>,
) -> Vec<Option<String>> {
    let mut ref_ids = vec![None; instances.len()];

    for instance in instances {
        for raw_value in instance.properties.values() {
            if let Some(target_index) = resolve_ref_target_index_from_maps(
                raw_value,
                index_by_instance_id,
                index_by_debug_id,
                index_by_path,
                index_by_path_segments,
            ) {
                if target_index < instances.len() && ref_ids[target_index].is_none() {
                    ref_ids[target_index] =
                        Some(stable_rojo_ref_id_for_instance(&instances[target_index]));
                }
            }
        }
    }

    ref_ids
}

fn stable_rojo_ref_id_for_instance(instance: &SnapshotInstance) -> String {
    let mut raw = String::new();
    raw.push_str("class:");
    raw.push_str(&instance.class_name);
    raw.push('|');
    if !instance.path_segments.is_empty() {
        raw.push_str("segments:");
        raw.push_str(&path_segments_key(&instance.path_segments));
    } else if let Some(instance_id) = instance.instance_id.as_deref().filter(|s| !s.is_empty()) {
        raw.push_str("instanceId:");
        raw.push_str(instance_id);
    } else if let Some(debug_id) = instance.debug_id.as_deref().filter(|s| !s.is_empty()) {
        raw.push_str("debugId:");
        raw.push_str(debug_id);
    } else {
        raw.push_str("path:");
        raw.push_str(&instance.path);
    }

    let first = fnv1a64_with_seed(raw.as_bytes(), 0xcbf29ce484222325);
    let second = fnv1a64_with_seed(raw.as_bytes(), 0x84222325cbf29ce4);
    format!("rbxsync_{first:016x}{second:016x}")
}

fn fnv1a64_with_seed(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn normalize_class_defaults(raw: Value) -> HashMap<String, Map<String, Value>> {
    let mut out = HashMap::new();
    let Some(class_defaults_obj) = raw.as_object() else {
        return out;
    };

    for (class_name, class_defaults_value) in class_defaults_obj {
        let Some(props_obj) = class_defaults_value.as_object() else {
            continue;
        };
        let mut props = Map::new();
        for (property_name, default_value) in props_obj {
            props.insert(property_name.clone(), default_value.clone());
        }
        if !props.is_empty() {
            out.insert(class_name.clone(), props);
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

fn parse_chunk_instances(text: &str) -> Result<Vec<SnapshotInstance>> {
    let parsed: Value = serde_json::from_str(text).context("Invalid JSON")?;
    match parsed {
        Value::Array(_) => serde_json::from_value(parsed).context("Invalid chunk array schema"),
        Value::Object(obj) => {
            if let Some(instances_value) = obj.get("instances") {
                serde_json::from_value(instances_value.clone())
                    .context("Invalid chunk object instances schema")
            } else {
                bail!("Chunk object missing instances field")
            }
        }
        _ => bail!("Chunk JSON must be an array or an object with instances"),
    }
}

fn add_instance_if_new(
    instance: SnapshotInstance,
    out: &mut Vec<SnapshotInstance>,
    seen: &mut HashSet<String>,
) {
    if let Some(key) = dedupe_key(&instance) {
        if !seen.insert(key) {
            return;
        }
    }
    out.push(instance);
}

fn dedupe_key(instance: &SnapshotInstance) -> Option<String> {
    if let Some(instance_id) = instance.instance_id.as_deref().filter(|s| !s.is_empty()) {
        return Some(format!("id:{instance_id}"));
    }
    if let Some(debug_id) = instance.debug_id.as_deref().filter(|s| !s.is_empty()) {
        return Some(format!("debug:{debug_id}"));
    }
    if !instance.path.is_empty() {
        return Some(format!("path:{}", instance.path));
    }
    None
}

fn derive_parent_path(path: &str) -> Option<String> {
    let last_dot = path.rfind('.')?;
    if last_dot == 0 {
        return None;
    }
    Some(path[..last_dot].to_string())
}

const DIRECT_IMPORT_SUBTREE_SPLIT_MIN_INSTANCES: usize = 5_000;
const DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN: usize = 2;
const DIRECT_IMPORT_RECURSIVE_SPLIT_TARGET: usize = 4_000;
const DIRECT_IMPORT_SUBTREE_GROUP_TARGET_INSTANCES: usize = 4_000;
const DIRECT_IMPORT_SUBTREE_GROUP_MAX_ITEMS: usize = 8;

fn direct_import_split_min_instances(service_instance_count: usize) -> usize {
    if service_instance_count >= 60_000 {
        2_000
    } else {
        DIRECT_IMPORT_SUBTREE_SPLIT_MIN_INSTANCES
    }
}

fn direct_import_recursive_split_target(service_instance_count: usize) -> usize {
    if service_instance_count >= 60_000 {
        2_000
    } else if service_instance_count >= 25_000 {
        4_000
    } else {
        DIRECT_IMPORT_RECURSIVE_SPLIT_TARGET
    }
}

fn direct_import_group_target_instances(service_instance_count: usize) -> usize {
    if service_instance_count >= 60_000 {
        1_200
    } else if service_instance_count >= 25_000 {
        2_000
    } else {
        DIRECT_IMPORT_SUBTREE_GROUP_TARGET_INSTANCES
    }
}

fn direct_import_group_max_items(service_instance_count: usize) -> usize {
    if service_instance_count >= 60_000 {
        4
    } else {
        DIRECT_IMPORT_SUBTREE_GROUP_MAX_ITEMS
    }
}

fn name_child_indices(state: &ServiceState, child_indices: &[usize]) -> Vec<(usize, String)> {
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut named_children: Vec<(usize, String)> = Vec::with_capacity(child_indices.len());
    for child_index in child_indices {
        let child = &state.instances[*child_index];
        let base = sanitize_name(&child.name);
        let count = counters.entry(base.clone()).or_insert(0);
        *count += 1;
        let child_stem = if *count == 1 {
            base
        } else {
            format!("{base}_{}", count)
        };
        named_children.push((*child_index, child_stem));
    }
    named_children
}

fn maybe_enqueue_split_import_tasks(
    sender: &mpsc::SyncSender<DirectImportTask>,
    pending_tasks: &Arc<AtomicUsize>,
    project_root: &Path,
    src_root: &Path,
    service: &str,
    compact_meta_json: bool,
    state: &ServiceState,
) -> Result<Option<Arc<SplitDirectImportState>>> {
    let service_instance_count = state.instances.len();
    let root_children = child_indices_for_instance(state, state.service_root_index);
    if service_instance_count < direct_import_split_min_instances(service_instance_count)
        || root_children.len() < DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN
    {
        return Ok(None);
    }

    let named_children = name_child_indices(state, &root_children);
    if named_children.len() < DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN {
        return Ok(None);
    }
    let subtree_sizes = compute_subtree_sizes(state);

    let service_dir = src_root.join(sanitize_name(service));
    fs::create_dir_all(&service_dir)
        .with_context(|| format!("Failed to create {}", service_dir.display()))?;
    let expected_paths = Arc::new(ImportPathSets::default());
    track_expected_dir(&expected_paths, &service_dir);

    let root = &state.instances[state.service_root_index];
    let root_meta = build_meta(state, state.service_root_index, root, false, true);
    let root_meta_path = service_dir.join("init.meta.json");
    write_json_file(&root_meta_path, &root_meta, compact_meta_json)?;
    track_expected_file(&expected_paths, &root_meta_path);

    let visited = Arc::new(
        (0..state.instances.len())
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>(),
    );
    let _ = mark_visited(&visited, state.service_root_index);

    let shared = Arc::new(SplitDirectImportState {
        service: service.to_string(),
        service_dir: service_dir.clone(),
        project_root: project_root.to_path_buf(),
        compact_meta_json,
        state: Arc::new(state.clone()),
        expected_paths,
        visited,
        slots: Mutex::new(Vec::new()),
        queued_tasks: AtomicUsize::new(0),
        completed_tasks: AtomicUsize::new(0),
        total_task_tenths_ms: AtomicU64::new(0),
        max_task_tenths_ms: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        started: Instant::now(),
    });

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

    plan_split_import_children(
        &shared,
        sender,
        pending_tasks,
        project_root,
        &service_dir,
        named_children,
        root_child_slots,
        Arc::clone(&root_assembly),
        &subtree_sizes,
    )?;

    Ok(Some(shared))
}

fn compute_subtree_sizes(state: &ServiceState) -> Vec<usize> {
    fn visit(
        state: &ServiceState,
        index: usize,
        sizes: &mut [usize],
        visiting: &mut HashSet<usize>,
    ) -> usize {
        if index >= sizes.len() {
            return 0;
        }
        if sizes[index] > 0 {
            return sizes[index];
        }
        if !visiting.insert(index) {
            return 0;
        }
        let mut size = 1usize;
        for child_index in child_indices_for_instance(state, index) {
            size = size.saturating_add(visit(state, child_index, sizes, visiting));
        }
        visiting.remove(&index);
        sizes[index] = size;
        size
    }

    let mut sizes = vec![0usize; state.instances.len()];
    let mut visiting = HashSet::new();
    let _ = visit(state, state.service_root_index, &mut sizes, &mut visiting);
    sizes
}

fn allocate_split_slots(shared: &Arc<SplitDirectImportState>, count: usize) -> Vec<usize> {
    let mut slots = match shared.slots.lock() {
        Ok(lock) => lock,
        Err(poisoned) => poisoned.into_inner(),
    };
    let start = slots.len();
    slots.resize(start + count, None);
    (start..start + count).collect()
}

fn child_indices_for_split(state: &ServiceState, index: usize) -> Vec<usize> {
    child_indices_for_instance(state, index)
}

fn queue_split_subtree_task(
    shared: &Arc<SplitDirectImportState>,
    sender: &mpsc::SyncSender<DirectImportTask>,
    pending_tasks: &Arc<AtomicUsize>,
    items: Vec<DirectImportSubtreeItem>,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    shared.queued_tasks.fetch_add(1, Ordering::AcqRel);
    pending_tasks.fetch_add(1, Ordering::AcqRel);
    sender
        .send(DirectImportTask::Subtree(DirectImportSubtreeTask {
            shared: Arc::clone(shared),
            items,
        }))
        .map_err(|err| {
            shared.queued_tasks.fetch_sub(1, Ordering::AcqRel);
            pending_tasks.fetch_sub(1, Ordering::AcqRel);
            anyhow::anyhow!("Failed to queue subtree import task: {err}")
        })
}

fn plan_split_import_children(
    shared: &Arc<SplitDirectImportState>,
    sender: &mpsc::SyncSender<DirectImportTask>,
    pending_tasks: &Arc<AtomicUsize>,
    project_root: &Path,
    parent_dir: &Path,
    named_children: Vec<(usize, String)>,
    child_slots: Vec<usize>,
    parent_assembly: Arc<SplitNodeAssembly>,
    subtree_sizes: &[usize],
) -> Result<()> {
    let service_instance_count = shared.state.instances.len();
    let recursive_split_target = direct_import_recursive_split_target(service_instance_count);
    let group_target_instances = direct_import_group_target_instances(service_instance_count);
    let group_max_items = direct_import_group_max_items(service_instance_count);
    let mut group_items = Vec::<DirectImportSubtreeItem>::new();
    let mut group_instances = 0usize;

    let flush_group =
        |items: &mut Vec<DirectImportSubtreeItem>, instances: &mut usize| -> Result<()> {
            if items.is_empty() {
                return Ok(());
            }
            queue_split_subtree_task(shared, sender, pending_tasks, std::mem::take(items))?;
            *instances = 0;
            Ok(())
        };

    for ((child_index, child_stem), child_slot) in
        named_children.into_iter().zip(child_slots.into_iter())
    {
        let child_indices = child_indices_for_split(&shared.state, child_index);
        let subtree_size = subtree_sizes.get(child_index).copied().unwrap_or(1);
        let should_recurse = subtree_size > recursive_split_target
            && child_indices.len() >= DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN;

        if should_recurse {
            flush_group(&mut group_items, &mut group_instances)?;
            plan_split_import_node(
                shared,
                sender,
                pending_tasks,
                project_root,
                parent_dir,
                child_index,
                child_stem,
                child_slot,
                Arc::clone(&parent_assembly),
                subtree_sizes,
            )?;
            continue;
        }

        if !group_items.is_empty()
            && (group_items.len() >= group_max_items
                || group_instances.saturating_add(subtree_size) > group_target_instances)
        {
            flush_group(&mut group_items, &mut group_instances)?;
        }

        group_instances = group_instances.saturating_add(subtree_size);
        group_items.push(DirectImportSubtreeItem {
            index: child_index,
            parent_dir: parent_dir.to_path_buf(),
            fs_stem: child_stem,
            output_slot: child_slot,
            parent_assembly: Arc::clone(&parent_assembly),
        });
    }

    flush_group(&mut group_items, &mut group_instances)
}

fn plan_split_import_node(
    shared: &Arc<SplitDirectImportState>,
    sender: &mpsc::SyncSender<DirectImportTask>,
    pending_tasks: &Arc<AtomicUsize>,
    project_root: &Path,
    parent_dir: &Path,
    index: usize,
    fs_stem: String,
    output_slot: usize,
    parent_assembly: Arc<SplitNodeAssembly>,
    subtree_sizes: &[usize],
) -> Result<()> {
    let service_instance_count = shared.state.instances.len();
    let recursive_split_target = direct_import_recursive_split_target(service_instance_count);
    let child_indices = child_indices_for_split(&shared.state, index);
    let subtree_size = subtree_sizes.get(index).copied().unwrap_or(1);
    if subtree_size <= recursive_split_target
        || child_indices.len() < DIRECT_IMPORT_SUBTREE_SPLIT_MIN_CHILDREN
    {
        queue_split_subtree_task(
            shared,
            sender,
            pending_tasks,
            vec![DirectImportSubtreeItem {
                index,
                parent_dir: parent_dir.to_path_buf(),
                fs_stem,
                output_slot,
                parent_assembly,
            }],
        )?;
        return Ok(());
    }

    let Some(shell) = emit_split_node_shell(
        &shared.state,
        index,
        project_root,
        parent_dir,
        &fs_stem,
        shared.compact_meta_json,
        &shared.visited,
        &shared.expected_paths,
    )?
    else {
        bail!("Failed to create split shell for {}", shared.service);
    };

    let named_children = name_child_indices(&shared.state, &child_indices);
    let child_slots = allocate_split_slots(shared, named_children.len());
    let assembly = Arc::new(SplitNodeAssembly {
        name: shell.name,
        class_name: shell.class_name,
        file_paths: shell.file_paths,
        child_slots: child_slots.clone(),
        remaining_children: AtomicUsize::new(child_slots.len()),
        output_slot: Some(output_slot),
        parent: Some(parent_assembly),
    });

    plan_split_import_children(
        shared,
        sender,
        pending_tasks,
        project_root,
        &shell.dir_path,
        named_children,
        child_slots,
        Arc::clone(&assembly),
        subtree_sizes,
    )
}

fn record_split_task_timing(shared: &Arc<SplitDirectImportState>, started: Instant) {
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

fn emit_split_node_shell(
    state: &ServiceState,
    index: usize,
    project_root: &Path,
    parent_dir: &Path,
    fs_stem: &str,
    compact_meta_json: bool,
    visited: &Arc<Vec<AtomicBool>>,
    expected_paths: &Arc<ImportPathSets>,
) -> Result<Option<SplitNodeShell>> {
    if !mark_visited(visited, index) {
        return Ok(None);
    }

    let instance = &state.instances[index];
    let class_name = instance.class_name.as_str();

    if let Some((source_file_name, _leaf_suffix)) = script_file_names(class_name) {
        let dir_path = parent_dir.join(fs_stem);
        fs::create_dir_all(&dir_path)
            .with_context(|| format!("Failed to create {}", dir_path.display()))?;
        track_expected_dir(expected_paths, &dir_path);

        let source = instance
            .properties
            .get("Source")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let source_path = dir_path.join(source_file_name);
        write_utf8_file(&source_path, &source)?;
        track_expected_file(expected_paths, &source_path);

        let meta = build_meta(state, index, instance, false, false);
        if meta_has_payload(&meta) {
            let init_meta_path = dir_path.join("init.meta.json");
            write_json_file(&init_meta_path, &meta, compact_meta_json)?;
            track_expected_file(expected_paths, &init_meta_path);
        }

        return Ok(Some(SplitNodeShell {
            name: fs_stem.to_string(),
            class_name: class_name.to_string(),
            file_paths: vec![path_to_sourcemap_relative(project_root, &source_path)],
            dir_path,
        }));
    }

    let dir_path = parent_dir.join(fs_stem);
    fs::create_dir_all(&dir_path)
        .with_context(|| format!("Failed to create {}", dir_path.display()))?;
    track_expected_dir(expected_paths, &dir_path);

    let meta = build_meta(state, index, instance, false, true);
    let has_meta_settings = meta_has_payload(&meta);
    let should_write_meta = !(class_name == "Folder" && !has_meta_settings);
    if should_write_meta {
        let init_meta_path = dir_path.join("init.meta.json");
        write_json_file(&init_meta_path, &meta, compact_meta_json)?;
        track_expected_file(expected_paths, &init_meta_path);
    }

    Ok(Some(SplitNodeShell {
        name: fs_stem.to_string(),
        class_name: "Folder".to_string(),
        file_paths: Vec::new(),
        dir_path,
    }))
}

fn complete_split_slot(
    shared: &Arc<SplitDirectImportState>,
    output_slot: usize,
    node: Option<SourcemapNode>,
    parent_assembly: &Arc<SplitNodeAssembly>,
    service_nodes: &Arc<Mutex<HashMap<String, SourcemapNode>>>,
    sourcemap_sender: Option<&mpsc::Sender<SourcemapWriterMessage>>,
) -> Result<()> {
    {
        let mut slots = match shared.slots.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
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
    shared: &Arc<SplitDirectImportState>,
    assembly: &Arc<SplitNodeAssembly>,
    service_nodes: &Arc<Mutex<HashMap<String, SourcemapNode>>>,
    sourcemap_sender: Option<&mpsc::Sender<SourcemapWriterMessage>>,
) -> Result<()> {
    if shared.failed.load(Ordering::Acquire) {
        return Ok(());
    }

    let children = {
        let slots = match shared.slots.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        assembly
            .child_slots
            .iter()
            .filter_map(|slot| slots.get(*slot).and_then(|node| node.clone()))
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
        spawn_cleanup_service_dir(
            shared.service_dir.clone(),
            Arc::clone(&shared.expected_paths),
        );

        if let Some(sender) = sourcemap_sender {
            let _ = sender.send(SourcemapWriterMessage::Service(
                shared.service.clone(),
                node.clone(),
            ));
        }

        let mut nodes = match service_nodes.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        nodes.insert(shared.service.clone(), node);
        let completed_tasks = shared.completed_tasks.load(Ordering::Acquire);
        if completed_tasks > 0 {
            let total_ms = shared.total_task_tenths_ms.load(Ordering::Acquire) as f64 / 10.0;
            let max_ms = shared.max_task_tenths_ms.load(Ordering::Acquire) as f64 / 10.0;
            println!(
                "[roblox-sync-rs] {}: subtree import tasks done -> tasks={}, avg_ms={:.1}, max_ms={:.1}",
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
    service_nodes: &Arc<Mutex<HashMap<String, SourcemapNode>>>,
    sourcemap_sender: Option<&mpsc::Sender<SourcemapWriterMessage>>,
    started: Instant,
) -> Result<()> {
    let shared = task.shared;
    let mut completed_slots = Vec::with_capacity(task.items.len());
    for item in task.items {
        let node = emit_node_index(
            &shared.state,
            item.index,
            &shared.project_root,
            &item.parent_dir,
            &item.fs_stem,
            shared.compact_meta_json,
            &shared.visited,
            &shared.expected_paths,
        );

        match node {
            Ok(node) => completed_slots.push((item.output_slot, node, item.parent_assembly)),
            Err(err) => {
                record_split_task_timing(&shared, started);
                shared.failed.store(true, Ordering::Release);
                return Err(err);
            }
        }
    }

    record_split_task_timing(&shared, started);
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

fn import_service_state(
    state: &ServiceState,
    project_root: &Path,
    src_root: &Path,
    service: &str,
    compact_meta_json: bool,
) -> Result<SourcemapNode> {
    let service_dir = src_root.join(sanitize_name(service));
    fs::create_dir_all(&service_dir)
        .with_context(|| format!("Failed to create {}", service_dir.display()))?;
    let expected_paths = Arc::new(ImportPathSets::default());
    track_expected_dir(&expected_paths, &service_dir);

    let root = &state.instances[state.service_root_index];
    let root_meta = build_meta(state, state.service_root_index, root, false, true);
    let root_meta_path = service_dir.join("init.meta.json");
    write_json_file(&root_meta_path, &root_meta, compact_meta_json)?;
    track_expected_file(&expected_paths, &root_meta_path);

    let visited = Arc::new(
        (0..state.instances.len())
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>(),
    );
    let _ = mark_visited(&visited, state.service_root_index);

    let root_children = child_indices_for_instance(state, state.service_root_index);
    let child_nodes = emit_children_indices(
        state,
        &root_children,
        project_root,
        &service_dir,
        compact_meta_json,
        &visited,
        &expected_paths,
    )?;

    spawn_cleanup_service_dir(service_dir.clone(), Arc::clone(&expected_paths));

    Ok(SourcemapNode {
        name: service.to_string(),
        class_name: service.to_string(),
        file_paths: Vec::new(),
        children: child_nodes,
    })
}

#[derive(Default)]
struct ImportPathSets {
    files: Mutex<HashSet<String>>,
    dirs: Mutex<HashSet<String>>,
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn track_expected_file(expected_paths: &Arc<ImportPathSets>, path: &Path) {
    let mut files = match expected_paths.files.lock() {
        Ok(lock) => lock,
        Err(poisoned) => poisoned.into_inner(),
    };
    files.insert(path_key(path));
}

fn track_expected_dir(expected_paths: &Arc<ImportPathSets>, path: &Path) {
    let mut dirs = match expected_paths.dirs.lock() {
        Ok(lock) => lock,
        Err(poisoned) => poisoned.into_inner(),
    };
    dirs.insert(path_key(path));
}

fn spawn_cleanup_service_dir(service_dir: PathBuf, expected_paths: Arc<ImportPathSets>) {
    thread::spawn(move || {
        let cleanup_started = Instant::now();
        if let Err(err) = cleanup_service_dir(&service_dir, &expected_paths) {
            println!(
                "[roblox-sync-rs] warning: cleanup failed for {}: {err:#}",
                service_dir.display()
            );
        } else {
            log_timing(
                &format!("cleanup {}", service_dir.display()),
                cleanup_started,
            );
        }
    });
}

fn cleanup_service_dir(service_dir: &Path, expected_paths: &Arc<ImportPathSets>) -> Result<()> {
    if !service_dir.exists() {
        return Ok(());
    }

    let expected_files = {
        let guard = match expected_paths.files.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    };
    let expected_dirs = {
        let guard = match expected_paths.dirs.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    };

    let mut stale_files = Vec::new();
    let mut stale_dirs = Vec::new();
    collect_stale_paths(
        service_dir,
        &expected_files,
        &expected_dirs,
        &mut stale_files,
        &mut stale_dirs,
    )?;

    if !stale_files.is_empty() {
        stale_files.par_iter().try_for_each(|path| -> Result<()> {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("Failed to remove {}", path.display()));
                }
            }
            Ok(())
        })?;
    }

    stale_dirs.sort_by(|a, b| b.components().count().cmp(&a.components().count()));
    for dir in stale_dirs {
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to remove {}", dir.display()));
            }
        }
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

fn emit_node_index(
    state: &ServiceState,
    index: usize,
    project_root: &Path,
    parent_dir: &Path,
    fs_stem: &str,
    compact_meta_json: bool,
    visited: &Arc<Vec<AtomicBool>>,
    expected_paths: &Arc<ImportPathSets>,
) -> Result<Option<SourcemapNode>> {
    if !mark_visited(visited, index) {
        return Ok(None);
    }

    let instance = &state.instances[index];
    let child_indices = child_indices_for_instance(state, index);
    let has_children = !child_indices.is_empty();
    let class_name = instance.class_name.as_str();

    if let Some((source_file_name, leaf_suffix)) = script_file_names(class_name) {
        let source = instance
            .properties
            .get("Source")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let meta = build_meta(state, index, instance, false, false);
        let has_meta_settings = meta_has_payload(&meta);

        if has_children {
            let dir_path = parent_dir.join(fs_stem);
            fs::create_dir_all(&dir_path)
                .with_context(|| format!("Failed to create {}", dir_path.display()))?;
            track_expected_dir(expected_paths, &dir_path);
            let source_path = dir_path.join(source_file_name);
            write_utf8_file(&source_path, &source)?;
            track_expected_file(expected_paths, &source_path);
            if has_meta_settings {
                let init_meta_path = dir_path.join("init.meta.json");
                write_json_file(&init_meta_path, &meta, compact_meta_json)?;
                track_expected_file(expected_paths, &init_meta_path);
            }

            let children = emit_children_indices(
                state,
                &child_indices,
                project_root,
                &dir_path,
                compact_meta_json,
                visited,
                expected_paths,
            )?;
            return Ok(Some(SourcemapNode {
                name: fs_stem.to_string(),
                class_name: class_name.to_string(),
                file_paths: vec![path_to_sourcemap_relative(project_root, &source_path)],
                children,
            }));
        } else {
            let script_path = parent_dir.join(format!("{fs_stem}{leaf_suffix}"));
            write_utf8_file(&script_path, &source)?;
            track_expected_file(expected_paths, &script_path);
            if has_meta_settings {
                let meta_path = parent_dir.join(format!("{fs_stem}.meta.json"));
                write_json_file(&meta_path, &meta, compact_meta_json)?;
                track_expected_file(expected_paths, &meta_path);
            }

            return Ok(Some(SourcemapNode {
                name: fs_stem.to_string(),
                class_name: class_name.to_string(),
                file_paths: vec![path_to_sourcemap_relative(project_root, &script_path)],
                children: Vec::new(),
            }));
        }
    }

    let dir_path = parent_dir.join(fs_stem);
    fs::create_dir_all(&dir_path)
        .with_context(|| format!("Failed to create {}", dir_path.display()))?;
    track_expected_dir(expected_paths, &dir_path);

    let meta = build_meta(state, index, instance, false, true);
    let has_meta_settings = meta_has_payload(&meta);
    let should_write_meta = !(class_name == "Folder" && !has_meta_settings);
    if should_write_meta {
        let init_meta_path = dir_path.join("init.meta.json");
        write_json_file(&init_meta_path, &meta, compact_meta_json)?;
        track_expected_file(expected_paths, &init_meta_path);
    }

    let children = emit_children_indices(
        state,
        &child_indices,
        project_root,
        &dir_path,
        compact_meta_json,
        visited,
        expected_paths,
    )?;

    if children.is_empty() {
        return Ok(None);
    }

    Ok(Some(SourcemapNode {
        name: fs_stem.to_string(),
        class_name: "Folder".to_string(),
        file_paths: Vec::new(),
        children,
    }))
}

fn emit_children_indices(
    state: &ServiceState,
    child_indices: &[usize],
    project_root: &Path,
    dir_path: &Path,
    compact_meta_json: bool,
    visited: &Arc<Vec<AtomicBool>>,
    expected_paths: &Arc<ImportPathSets>,
) -> Result<Vec<SourcemapNode>> {
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut named_children: Vec<(usize, String)> = Vec::with_capacity(child_indices.len());
    for child_index in child_indices {
        let child = &state.instances[*child_index];
        let base = sanitize_name(&child.name);
        let count = counters.entry(base.clone()).or_insert(0);
        *count += 1;
        let child_stem = if *count == 1 {
            base
        } else {
            format!("{base}_{}", count)
        };
        named_children.push((*child_index, child_stem));
    }

    const PARALLEL_CHILD_THRESHOLD: usize = 8;
    if named_children.len() >= PARALLEL_CHILD_THRESHOLD && rayon::current_num_threads() > 1 {
        let built: Vec<Option<SourcemapNode>> = named_children
            .par_iter()
            .map(|(child_index, child_stem)| {
                emit_node_index(
                    state,
                    *child_index,
                    project_root,
                    dir_path,
                    child_stem,
                    compact_meta_json,
                    visited,
                    expected_paths,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(built.into_iter().flatten().collect());
    } else {
        let mut built = Vec::with_capacity(named_children.len());
        for (child_index, child_stem) in named_children {
            if let Some(node) = emit_node_index(
                state,
                child_index,
                project_root,
                dir_path,
                &child_stem,
                compact_meta_json,
                visited,
                expected_paths,
            )? {
                built.push(node);
            }
        }
        return Ok(built);
    }
}

fn mark_visited(visited: &Arc<Vec<AtomicBool>>, index: usize) -> bool {
    if index >= visited.len() {
        return false;
    }
    !visited[index].swap(true, Ordering::AcqRel)
}

fn child_indices_for_instance(state: &ServiceState, parent_index: usize) -> Vec<usize> {
    let instance = &state.instances[parent_index];

    let raw_children = instance
        .instance_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|instance_id| state.children_by_parent_instance_id.get(instance_id))
        .or_else(|| {
            instance
                .debug_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|debug_id| state.children_by_parent_debug.get(debug_id))
        })
        .or_else(|| state.children_by_parent_path.get(&instance.path));

    let Some(raw_children) = raw_children else {
        return Vec::new();
    };

    let mut deduped: Vec<usize> = Vec::with_capacity(raw_children.len());
    let mut seen_child_indices: HashSet<usize> = HashSet::with_capacity(raw_children.len());
    for child_index in raw_children {
        if *child_index >= state.instances.len() {
            continue;
        }
        if seen_child_indices.insert(*child_index) {
            deduped.push(*child_index);
        }
    }

    deduped
}

fn script_file_names(class_name: &str) -> Option<(&'static str, &'static str)> {
    match class_name {
        "Script" => Some(("init.server.luau", ".server.luau")),
        "LocalScript" => Some(("init.client.luau", ".client.luau")),
        "ModuleScript" => Some(("init.luau", ".luau")),
        _ => None,
    }
}

fn build_meta(
    state: &ServiceState,
    index: usize,
    instance: &SnapshotInstance,
    keep_empty_service_props: bool,
    include_class_name: bool,
) -> Value {
    let mut props_out: BTreeMap<String, Value> = BTreeMap::new();
    let mut attrs_out: BTreeMap<String, Value> = BTreeMap::new();
    let mut instance_attrs_out: BTreeMap<String, Value> = BTreeMap::new();

    for (name, raw_value) in &instance.properties {
        let lower_name = name.to_ascii_lowercase();
        if matches!(
            lower_name.as_str(),
            "source" | "classname" | "parent" | "name" | "robloxlocked"
        ) {
            continue;
        }
        if name == "RunContext" && instance.class_name != "Script" {
            continue;
        }
        if is_default_property_value(state, &instance.class_name, name, raw_value) {
            continue;
        }
        if ref_payload_object(raw_value).is_some() {
            if let Some(target_index) = resolve_ref_target_index(state, raw_value) {
                if let Some(Some(target_id)) = state.rojo_ref_ids_by_index.get(target_index) {
                    attrs_out.insert(
                        format!("Rojo_Target_{name}"),
                        Value::String(target_id.clone()),
                    );
                }
            }
        }
        let converted = convert_value(raw_value);
        if !converted.is_null() {
            props_out.insert(name.clone(), converted);
        }
    }

    if !instance.attributes.is_empty() {
        for (k, v) in &instance.attributes {
            if k == "Rojo_Id" || k.starts_with("Rojo_Target_") {
                continue;
            }
            if let Some(converted) = convert_attribute_value(v) {
                instance_attrs_out.insert(k.clone(), converted);
            }
        }
        if !instance_attrs_out.is_empty() {
            props_out.insert(
                "Attributes".to_string(),
                serde_json::to_value(instance_attrs_out).unwrap_or(Value::Null),
            );
        }
    }

    let mut out = Map::new();
    if include_class_name {
        out.insert(
            "className".to_string(),
            Value::String(instance.class_name.clone()),
        );
    }
    if keep_empty_service_props || !props_out.is_empty() {
        out.insert(
            "properties".to_string(),
            serde_json::to_value(props_out).unwrap_or_else(|_| json!({})),
        );
    }
    if !attrs_out.is_empty() {
        out.insert(
            "attributes".to_string(),
            serde_json::to_value(attrs_out).unwrap_or_else(|_| json!({})),
        );
    }
    if let Some(Some(ref_id)) = state.rojo_ref_ids_by_index.get(index) {
        out.insert("id".to_string(), Value::String(ref_id.clone()));
    }
    Value::Object(out)
}

fn meta_has_payload(meta: &Value) -> bool {
    let Some(obj) = meta.as_object() else {
        return false;
    };
    if obj.get("id").is_some() {
        return true;
    }
    if obj
        .get("properties")
        .and_then(Value::as_object)
        .map(|x| !x.is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    obj.get("attributes")
        .and_then(Value::as_object)
        .map(|x| !x.is_empty())
        .unwrap_or(false)
}

fn is_default_property_value(
    state: &ServiceState,
    class_name: &str,
    property_name: &str,
    property_value: &Value,
) -> bool {
    if matches!(
        (property_name, property_value),
        ("Archivable", Value::Bool(true))
            | ("Sandboxed", Value::Bool(false))
            | ("CharacterAutoLoads", Value::Bool(true))
    ) {
        return true;
    }

    state
        .class_defaults_by_class
        .get(class_name)
        .and_then(|props| props.get(property_name))
        .map(|default| default == property_value)
        .unwrap_or(false)
}

fn convert_attribute_value(value: &Value) -> Option<Value> {
    if let Some(obj) = value.as_object() {
        for key in [
            "Bool",
            "String",
            "Float32",
            "Float64",
            "Int32",
            "Int64",
            "Vector2",
            "Vector3",
            "UDim",
            "UDim2",
            "Color3",
            "BrickColor",
            "CFrame",
            "Rect",
            "Enum",
            "Font",
            "ColorSequence",
            "NumberSequence",
        ] {
            if let Some(existing) = obj.get(key) {
                let mut wrapped = Map::new();
                wrapped.insert(key.to_string(), existing.clone());
                return Some(Value::Object(wrapped));
            }
        }
    }

    match value {
        Value::Bool(v) => Some(json!({ "Bool": v })),
        Value::Number(v) => Some(json!({ "Float64": v })),
        Value::String(v) => Some(json!({ "String": v })),
        _ => None,
    }
}

fn convert_value(value: &Value) -> Value {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
        Value::Array(arr) => Value::Array(arr.iter().map(convert_value).collect()),
        Value::Object(obj) => {
            if let Some(t) = obj.get("_type").and_then(Value::as_str) {
                return match t {
                    "Vector2" => json!([
                        obj.get("x").cloned().unwrap_or(Value::Null),
                        obj.get("y").cloned().unwrap_or(Value::Null)
                    ]),
                    "Vector3" => json!([
                        obj.get("x").cloned().unwrap_or(Value::Null),
                        obj.get("y").cloned().unwrap_or(Value::Null),
                        obj.get("z").cloned().unwrap_or(Value::Null)
                    ]),
                    "UDim" => json!({
                        "UDim": [
                            obj.get("scale").cloned().unwrap_or(Value::Null),
                            obj.get("offset").cloned().unwrap_or(Value::Null)
                        ]
                    }),
                    "UDim2" => json!({
                        "UDim2": [
                            [
                                obj.get("xScale").cloned().unwrap_or(Value::Null),
                                obj.get("xOffset").cloned().unwrap_or(Value::Null)
                            ],
                            [
                                obj.get("yScale").cloned().unwrap_or(Value::Null),
                                obj.get("yOffset").cloned().unwrap_or(Value::Null)
                            ]
                        ]
                    }),
                    "Color3" => json!([
                        obj.get("r").cloned().unwrap_or(Value::Null),
                        obj.get("g").cloned().unwrap_or(Value::Null),
                        obj.get("b").cloned().unwrap_or(Value::Null)
                    ]),
                    "BrickColor" => json!({
                        "BrickColor": obj.get("number").cloned().unwrap_or(Value::Null)
                    }),
                    "ColorSequence" => {
                        let mut keypoints_out = Vec::new();
                        if let Some(keypoints) = obj.get("keypoints").and_then(Value::as_array) {
                            for keypoint in keypoints {
                                if let Some(keypoint_obj) = keypoint.as_object() {
                                    let time =
                                        keypoint_obj.get("time").cloned().unwrap_or(Value::Null);
                                    let color = keypoint_obj
                                        .get("value")
                                        .and_then(Value::as_object)
                                        .map(|value| {
                                            json!([
                                                value.get("r").cloned().unwrap_or(Value::Null),
                                                value.get("g").cloned().unwrap_or(Value::Null),
                                                value.get("b").cloned().unwrap_or(Value::Null)
                                            ])
                                        })
                                        .unwrap_or_else(|| json!([0, 0, 0]));
                                    keypoints_out.push(json!({
                                        "time": time,
                                        "color": color
                                    }));
                                }
                            }
                        }
                        json!({
                            "ColorSequence": {
                                "keypoints": keypoints_out
                            }
                        })
                    }
                    "NumberSequence" => {
                        let mut keypoints_out = Vec::new();
                        if let Some(keypoints) = obj.get("keypoints").and_then(Value::as_array) {
                            for keypoint in keypoints {
                                if let Some(keypoint_obj) = keypoint.as_object() {
                                    keypoints_out.push(json!({
                                        "time": keypoint_obj.get("time").cloned().unwrap_or(Value::Null),
                                        "value": keypoint_obj.get("value").cloned().unwrap_or(Value::Null),
                                        "envelope": keypoint_obj.get("envelope").cloned().unwrap_or(Value::Null)
                                    }));
                                }
                            }
                        }
                        json!({
                            "NumberSequence": {
                                "keypoints": keypoints_out
                            }
                        })
                    }
                    "CFrame" => obj
                        .get("components")
                        .cloned()
                        .unwrap_or_else(|| Value::Array(Vec::new())),
                    "Rect" => json!({
                        "Rect": [
                            [
                                obj.get("minX").cloned().unwrap_or(Value::Null),
                                obj.get("minY").cloned().unwrap_or(Value::Null)
                            ],
                            [
                                obj.get("maxX").cloned().unwrap_or(Value::Null),
                                obj.get("maxY").cloned().unwrap_or(Value::Null)
                            ]
                        ]
                    }),
                    "EnumItem" => obj
                        .get("name")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new())),
                    "Font" => {
                        let mut map = Map::new();
                        if let Some(family) = obj.get("family").and_then(Value::as_str) {
                            map.insert("family".to_string(), Value::String(family.to_string()));
                        }
                        if let Some(weight) = obj.get("weight").and_then(Value::as_str) {
                            map.insert(
                                "weight".to_string(),
                                Value::String(
                                    weight.split('.').next_back().unwrap_or(weight).to_string(),
                                ),
                            );
                        }
                        if let Some(style) = obj.get("style").and_then(Value::as_str) {
                            map.insert(
                                "style".to_string(),
                                Value::String(
                                    style.split('.').next_back().unwrap_or(style).to_string(),
                                ),
                            );
                        }
                        Value::Object(map)
                    }
                    "Ref" => {
                        let mut ref_value = Map::new();
                        if let Some(path_segments) =
                            obj.get("pathSegments").and_then(Value::as_array)
                        {
                            let mut out_segments = Vec::with_capacity(path_segments.len());
                            for segment in path_segments {
                                if let Some(text) = segment.as_str() {
                                    out_segments.push(Value::String(text.to_string()));
                                }
                            }
                            if !out_segments.is_empty() {
                                ref_value
                                    .insert("pathSegments".to_string(), Value::Array(out_segments));
                            }
                        }
                        if let Some(instance_id) = obj.get("instanceId").and_then(Value::as_str) {
                            if !instance_id.is_empty() {
                                ref_value.insert(
                                    "instanceId".to_string(),
                                    Value::String(instance_id.to_string()),
                                );
                            }
                        }
                        if let Some(debug_id) = obj.get("debugId").and_then(Value::as_str) {
                            if !debug_id.is_empty() {
                                ref_value.insert(
                                    "debugId".to_string(),
                                    Value::String(debug_id.to_string()),
                                );
                            }
                        }
                        json!({
                            "Ref": Value::Object(ref_value)
                        })
                    }
                    _ => {
                        let mut out = Map::new();
                        for (k, v) in obj {
                            if k != "_type" {
                                out.insert(k.clone(), convert_value(v));
                            }
                        }
                        Value::Object(out)
                    }
                };
            }

            let mut out = Map::new();
            for (k, v) in obj {
                out.insert(k.clone(), convert_value(v));
            }
            Value::Object(out)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourcemapNode {
    name: String,
    #[serde(rename = "className")]
    class_name: String,
    #[serde(default, rename = "filePaths", skip_serializing_if = "Vec::is_empty")]
    file_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<SourcemapNode>,
}

fn path_to_sourcemap_relative(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn build_service_sourcemap_node_from_state(
    state: &ServiceState,
    service: &str,
    src_root: &Path,
    project_root: &Path,
) -> Result<SourcemapNode> {
    let service_dir = src_root.join(sanitize_name(service));
    let root_children = child_indices_for_instance(state, state.service_root_index);
    let children =
        build_sourcemap_children_from_state(state, &root_children, &service_dir, project_root)?;
    Ok(SourcemapNode {
        name: service.to_string(),
        class_name: service.to_string(),
        file_paths: Vec::new(),
        children,
    })
}

fn build_sourcemap_children_from_state(
    state: &ServiceState,
    child_indices: &[usize],
    parent_dir: &Path,
    project_root: &Path,
) -> Result<Vec<SourcemapNode>> {
    let mut counters: HashMap<String, usize> = HashMap::new();
    let mut nodes = Vec::with_capacity(child_indices.len());
    for child_index in child_indices {
        let child = &state.instances[*child_index];
        let base = sanitize_name(&child.name);
        let count = counters.entry(base.clone()).or_insert(0);
        *count += 1;
        let child_stem = if *count == 1 {
            base
        } else {
            format!("{base}_{}", count)
        };
        nodes.push(build_sourcemap_node_from_state(
            state,
            *child_index,
            parent_dir,
            &child_stem,
            project_root,
        )?);
    }
    Ok(nodes)
}

fn build_sourcemap_node_from_state(
    state: &ServiceState,
    index: usize,
    parent_dir: &Path,
    fs_stem: &str,
    project_root: &Path,
) -> Result<SourcemapNode> {
    let instance = &state.instances[index];
    let child_indices = child_indices_for_instance(state, index);
    let has_children = !child_indices.is_empty();
    let class_name = instance.class_name.as_str();

    if let Some((source_file_name, leaf_suffix)) = script_file_names(class_name) {
        if has_children {
            let dir_path = parent_dir.join(fs_stem);
            let init_source_path = dir_path.join(source_file_name);
            let children = build_sourcemap_children_from_state(
                state,
                &child_indices,
                &dir_path,
                project_root,
            )?;
            return Ok(SourcemapNode {
                name: instance.name.clone(),
                class_name: instance.class_name.clone(),
                file_paths: vec![path_to_sourcemap_relative(project_root, &init_source_path)],
                children,
            });
        }

        let source_path = parent_dir.join(format!("{fs_stem}{leaf_suffix}"));
        return Ok(SourcemapNode {
            name: instance.name.clone(),
            class_name: instance.class_name.clone(),
            file_paths: vec![path_to_sourcemap_relative(project_root, &source_path)],
            children: Vec::new(),
        });
    }

    let dir_path = parent_dir.join(fs_stem);
    let children =
        build_sourcemap_children_from_state(state, &child_indices, &dir_path, project_root)?;
    Ok(SourcemapNode {
        name: instance.name.clone(),
        class_name: "Folder".to_string(),
        file_paths: Vec::new(),
        children,
    })
}

fn sourcemap_root_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("game")
        .to_string()
}

fn make_sourcemap_root(project_root: &Path) -> SourcemapNode {
    SourcemapNode {
        name: sourcemap_root_name(project_root),
        class_name: "DataModel".to_string(),
        file_paths: Vec::new(),
        children: Vec::new(),
    }
}

fn empty_service_sourcemap_node(service: &str) -> SourcemapNode {
    SourcemapNode {
        name: service.to_string(),
        class_name: service.to_string(),
        file_paths: Vec::new(),
        children: Vec::new(),
    }
}

fn sort_sourcemap_root_children(root: &mut SourcemapNode) {
    let service_order: HashMap<&str, usize> = DEFAULT_SERVICES
        .iter()
        .enumerate()
        .map(|(index, name)| (*name, index))
        .collect();
    root.children.sort_by(|a, b| {
        let a_index = service_order
            .get(a.name.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        let b_index = service_order
            .get(b.name.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        a_index.cmp(&b_index).then_with(|| a.name.cmp(&b.name))
    });
}

fn write_sourcemap_root(project_root: &Path, mut root: SourcemapNode) -> Result<()> {
    sort_sourcemap_root_children(&mut root);
    let output_file = project_root.join("sourcemap.json");
    write_json_file(&output_file, &root, true).context("Failed to serialize sourcemap")?;
    println!("[roblox-sync-rs] wrote {}", output_file.display());
    Ok(())
}

fn load_existing_sourcemap_root(project_root: &Path) -> Result<Option<SourcemapNode>> {
    let sourcemap_path = project_root.join("sourcemap.json");
    if !sourcemap_path.is_file() {
        return Ok(None);
    }

    let root: SourcemapNode = read_json_file(&sourcemap_path)
        .with_context(|| format!("Invalid JSON in {}", sourcemap_path.display()))?;
    Ok(Some(root))
}

fn write_project_sourcemap_from_service_nodes(
    project_root: &Path,
    service_nodes: &HashMap<String, SourcemapNode>,
) -> Result<()> {
    let mut root = make_sourcemap_root(project_root);
    root.children = service_nodes.values().cloned().collect();
    write_sourcemap_root(project_root, root)
}

fn write_project_sourcemap_temp_from_service_nodes(
    project_root: &Path,
    service_nodes: &HashMap<String, SourcemapNode>,
) -> Result<()> {
    let mut root = make_sourcemap_root(project_root);
    root.children = service_nodes.values().cloned().collect();
    sort_sourcemap_root_children(&mut root);
    let temp_file = project_root.join("sourcemap.json.tmp");
    write_json_file(&temp_file, &root, true).context("Failed to serialize sourcemap")
}

fn finalize_project_sourcemap_temp(
    project_root: &Path,
    service_nodes: &HashMap<String, SourcemapNode>,
) -> Result<()> {
    write_project_sourcemap_temp_from_service_nodes(project_root, service_nodes)?;
    let temp_file = project_root.join("sourcemap.json.tmp");
    let output_file = project_root.join("sourcemap.json");
    match fs::remove_file(&output_file) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to remove {}", output_file.display()));
        }
    }
    fs::rename(&temp_file, &output_file).with_context(|| {
        format!(
            "Failed to publish sourcemap {} -> {}",
            temp_file.display(),
            output_file.display()
        )
    })?;
    println!("[roblox-sync-rs] wrote {}", output_file.display());
    Ok(())
}

fn write_project_sourcemap_with_updates(
    project_root: &Path,
    updated_nodes: &HashMap<String, SourcemapNode>,
) -> Result<()> {
    let mut root = load_existing_sourcemap_root(project_root)?
        .unwrap_or_else(|| make_sourcemap_root(project_root));
    let mut children_by_name: HashMap<String, SourcemapNode> = root
        .children
        .into_iter()
        .map(|child| (child.name.clone(), child))
        .collect();

    for (service_name, node) in updated_nodes {
        children_by_name.insert(service_name.clone(), node.clone());
    }

    root.children = children_by_name.into_values().collect();
    write_sourcemap_root(project_root, root)
}

fn infer_script_file_class_and_name(file_name: &str) -> Option<(&'static str, String)> {
    let patterns = [
        (".server.luau", "Script"),
        (".server.lua", "Script"),
        (".client.luau", "LocalScript"),
        (".client.lua", "LocalScript"),
        (".luau", "ModuleScript"),
        (".lua", "ModuleScript"),
    ];

    for (suffix, class_name) in patterns {
        if let Some(stem) = file_name.strip_suffix(suffix) {
            if !stem.is_empty() {
                return Some((class_name, stem.to_string()));
            }
        }
    }
    None
}

fn infer_init_script_class(file_name: &str) -> Option<&'static str> {
    match file_name {
        "init.server.luau" | "init.server.lua" => Some("Script"),
        "init.client.luau" | "init.client.lua" => Some("LocalScript"),
        "init.luau" | "init.lua" => Some("ModuleScript"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct SourcemapBuildNode {
    name: String,
    class_name: String,
    file_paths: Vec<String>,
    children: BTreeMap<String, SourcemapBuildNode>,
}

impl SourcemapBuildNode {
    fn new(name: impl Into<String>, class_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            class_name: class_name.into(),
            file_paths: Vec::new(),
            children: BTreeMap::new(),
        }
    }

    fn child_mut(&mut self, name: &str, class_name: &str) -> &mut SourcemapBuildNode {
        let entry = self
            .children
            .entry(name.to_string())
            .or_insert_with(|| SourcemapBuildNode::new(name.to_string(), class_name.to_string()));
        if class_name != "Folder" {
            entry.class_name = class_name.to_string();
        }
        entry
    }

    fn push_file_path(&mut self, file_path: String) {
        if !self
            .file_paths
            .iter()
            .any(|existing| existing == &file_path)
        {
            self.file_paths.push(file_path);
        }
    }

    fn into_sourcemap(self) -> SourcemapNode {
        SourcemapNode {
            name: self.name,
            class_name: self.class_name,
            file_paths: self.file_paths,
            children: self
                .children
                .into_values()
                .map(SourcemapBuildNode::into_sourcemap)
                .collect(),
        }
    }
}

fn insert_script_path_into_service_tree(
    service_root: &mut SourcemapBuildNode,
    service_path: &Path,
    file_path: &Path,
    project_root: &Path,
) -> Result<()> {
    let relative_path = file_path.strip_prefix(service_path).with_context(|| {
        format!(
            "Failed to resolve {} relative to {}",
            file_path.display(),
            service_path.display()
        )
    })?;
    let mut components: Vec<String> = relative_path
        .iter()
        .map(|component| component.to_string_lossy().to_string())
        .collect();
    if components.is_empty() {
        return Ok(());
    }

    let file_name = components.pop().unwrap_or_default();
    let sourcemap_path = path_to_sourcemap_relative(project_root, file_path);

    if let Some(class_name) = infer_init_script_class(&file_name) {
        if components.is_empty() {
            return Ok(());
        }

        let node_name = components.pop().unwrap_or_default();
        let mut current = service_root;
        for component in &components {
            current = current.child_mut(component, "Folder");
        }
        let target = current.child_mut(&node_name, class_name);
        target.push_file_path(sourcemap_path);
        return Ok(());
    }

    let Some((class_name, instance_name)) = infer_script_file_class_and_name(&file_name) else {
        return Ok(());
    };
    if instance_name == "init" {
        return Ok(());
    }

    let mut current = service_root;
    for component in &components {
        current = current.child_mut(component, "Folder");
    }
    let target = current.child_mut(&instance_name, class_name);
    target.push_file_path(sourcemap_path);
    Ok(())
}

fn build_service_sourcemap_node_from_paths(
    service_name: &str,
    service_path: &Path,
    project_root: &Path,
) -> Result<Option<SourcemapNode>> {
    let mut root = SourcemapBuildNode::new(service_name.to_string(), service_name.to_string());

    for entry in WalkDir::new(service_path) {
        let entry = entry.with_context(|| format!("Failed to walk {}", service_path.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        insert_script_path_into_service_tree(&mut root, service_path, entry.path(), project_root)?;
    }

    if root.file_paths.is_empty() && root.children.is_empty() {
        return Ok(None);
    }

    Ok(Some(root.into_sourcemap()))
}

fn generate_project_sourcemap(project_root: &Path) -> Result<()> {
    let src_root = project_root.join("src");
    if !src_root.is_dir() {
        bail!(
            "Cannot generate sourcemap: missing src directory in {}",
            project_root.display()
        );
    }

    let root_name = project_root
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("game")
        .to_string();

    let mut root = SourcemapNode {
        name: root_name,
        class_name: "DataModel".to_string(),
        file_paths: Vec::new(),
        children: Vec::new(),
    };

    let mut service_entries: Vec<_> = fs::read_dir(&src_root)
        .with_context(|| format!("Failed to read {}", src_root.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to iterate {}", src_root.display()))?;
    service_entries.sort_by(|a, b| {
        a.file_name()
            .to_string_lossy()
            .cmp(&b.file_name().to_string_lossy())
    });

    let built_children = service_entries
        .par_iter()
        .enumerate()
        .map(|(index, entry)| -> Result<Option<(usize, SourcemapNode)>> {
            let file_type = entry
                .file_type()
                .with_context(|| format!("Failed to stat {}", entry.path().display()))?;
            if !file_type.is_dir() {
                return Ok(None);
            }

            let service_name = entry.file_name().to_string_lossy().to_string();
            let node = build_service_sourcemap_node_from_paths(
                &service_name,
                &entry.path(),
                project_root,
            )?;
            Ok(node.map(|node| (index, node)))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut built_children: Vec<_> = built_children.into_iter().flatten().collect();
    built_children.sort_by_key(|(index, _)| *index);
    for (_, node) in built_children {
        root.children.push(node);
    }

    let output_file = project_root.join("sourcemap.json");
    write_json_file(&output_file, &root, true).context("Failed to serialize sourcemap")?;
    println!("[roblox-sync-rs] wrote {}", output_file.display());
    Ok(())
}

fn write_generated_project(project_root: &Path, services: &[String], compact: bool) -> Result<()> {
    let mut tree = Map::new();
    tree.insert(
        "$className".to_string(),
        Value::String("DataModel".to_string()),
    );
    for service in services {
        tree.insert(
            service.clone(),
            json!({
                "$path": format!("src/{service}")
            }),
        );
    }

    let content = json!({
        "name": "projest",
        "tree": tree
    });
    write_json_file(
        &project_root.join("default.project.generated.json"),
        &content,
        compact,
    )
}

fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    if matches!(reader.fill_buf(), Ok(buffer) if buffer.starts_with(&[0xEF, 0xBB, 0xBF])) {
        reader.consume(3);
    }
    serde_json::from_reader(reader).with_context(|| format!("Invalid JSON in {}", path.display()))
}

fn write_json_file<T: Serialize>(path: &Path, value: &T, compact: bool) -> Result<()> {
    if compact {
        write_json_streaming(path, value)
    } else {
        let value =
            serde_json::to_value(value).context("Failed to convert JSON value for formatting")?;
        let serialized = serialize_pretty_json_with_inline_numeric_arrays(&value)?;
        write_utf8_file(path, &(serialized + "\n"))
    }
}

fn serialize_pretty_json_with_inline_numeric_arrays(value: &Value) -> Result<String> {
    let mut out = String::new();
    write_pretty_json_value(value, 0, &mut out)?;
    Ok(out)
}

fn write_pretty_json_value(value: &Value, indent: usize, out: &mut String) -> Result<()> {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }

            out.push('{');
            out.push('\n');
            for (index, (key, child)) in map.iter().enumerate() {
                push_json_indent(out, indent + 1);
                out.push_str(&serde_json::to_string(key).context("Failed to serialize JSON key")?);
                out.push_str(": ");
                write_pretty_json_value(child, indent + 1, out)?;
                if index + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_json_indent(out, indent);
            out.push('}');
            Ok(())
        }
        Value::Array(array) => {
            if array.is_empty() {
                out.push_str("[]");
                return Ok(());
            }

            if is_inline_numeric_array(array) {
                out.push('[');
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(
                        &serde_json::to_string(item)
                            .context("Failed to serialize numeric array value")?,
                    );
                }
                out.push(']');
                return Ok(());
            }

            out.push('[');
            out.push('\n');
            for (index, item) in array.iter().enumerate() {
                push_json_indent(out, indent + 1);
                write_pretty_json_value(item, indent + 1, out)?;
                if index + 1 < array.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_json_indent(out, indent);
            out.push(']');
            Ok(())
        }
        _ => {
            out.push_str(&serde_json::to_string(value).context("Failed to serialize JSON value")?);
            Ok(())
        }
    }
}

fn is_inline_numeric_array(array: &[Value]) -> bool {
    !array.is_empty() && array.iter().all(|value| matches!(value, Value::Number(_)))
}

fn push_json_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_utf8_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

fn write_json_streaming<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let file = File::create(path).with_context(|| format!("Failed to write {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value).context("Failed to serialize JSON")?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn sanitize_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let invalid = matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*');
        if invalid || ch.is_control() {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim_end_matches([' ', '.']);
    let mut final_name = if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    };

    let upper = final_name.to_ascii_uppercase();
    let reserved = matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if reserved {
        final_name.insert(0, '_');
    }
    final_name
}
