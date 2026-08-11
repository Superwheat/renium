use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_with_config};

use crate::local_transport::normalize_loopback_host;
#[cfg(any(windows, target_os = "macos"))]
use crate::local_transport::{local_tcp_ports_owned_by_pid, pid_for_local_tcp_port};
use crate::place_target::{place_filter, place_matches};
use crate::snapshot_export::{parse_bridge_chunk, validate_bridge_chunk, validate_bridge_info};
use crate::studio_automation::TestLaunch;
use crate::timing::elapsed_ms;

#[cfg(target_os = "macos")]
use super::input_inject;

pub(super) const DEFAULT_EXPORT_CHUNK_SIZE: usize = 4 * 1024 * 1024;
pub(super) const MAX_BRIDGE_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_BRIDGE_REASSEMBLY_BYTES: usize = 512 * 1024 * 1024;
pub(super) const BRIDGE_DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const BRIDGE_ROLE_EDIT: &str = "edit";
pub(super) const BRIDGE_ROLE_PLAY_SERVER: &str = "play-server";
pub(super) const BRIDGE_ROLE_PLAY_CLIENT: &str = "play-client";
const BRIDGE_ROLE_UNKNOWN: &str = "unknown";
const BRIDGE_DUPLICATE_ROLE_KEY_SEPARATOR: char = '#';
const MIN_BRIDGE_CHUNK_BYTES: usize = 256;
const MAX_BRIDGE_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_BRIDGE_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_BRIDGE_UNRELATED_MESSAGES: usize = 64;
const BRIDGE_CHANNEL_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const BRIDGE_SLOW_RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);

pub(super) fn clamp_bridge_chunk_size(size: usize) -> usize {
    size.clamp(MIN_BRIDGE_CHUNK_BYTES, MAX_BRIDGE_CHUNK_BYTES)
}

fn bridge_response_timeout(method: &str) -> Duration {
    match method {
        "prepare"
        | "applyEditorChanges"
        | "beginEditorTransaction"
        | "commitEditorTransaction"
        | "rollbackEditorTransaction"
        | "appendEditorBinaryImport"
        | "appendEditorPushReview"
        | "finishEditorBinaryImport"
        | "awaitEditorBinaryExport"
        | "getInstanceBatchCompactChunk"
        | "getEditorBinaryOverlayChunk"
        | "getSourceBatchChunk"
        | "getSourceRangeBatchCompactChunk" => BRIDGE_SLOW_RESPONSE_TIMEOUT,
        _ => BRIDGE_DEFAULT_RESPONSE_TIMEOUT,
    }
}

fn bridge_channel_lock_timeout(method: &str) -> Duration {
    match method {
        "appendEditorBinaryImport" | "appendEditorPushReview" => BRIDGE_SLOW_RESPONSE_TIMEOUT,
        _ => BRIDGE_CHANNEL_LOCK_TIMEOUT,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BridgeChunk {
    pub(super) start: usize,
    pub(super) next_start: usize,
    pub(super) total: usize,
    pub(super) chunk: String,
    pub(super) plugin_server_ms: Option<f64>,
    pub(super) plugin_encode_ms: Option<f64>,
    #[serde(default)]
    pub(super) serialization_complete: bool,
}

#[derive(Clone, Copy, Default)]
pub(super) struct ChunkFetchMetrics {
    pub(super) bytes: usize,
    pub(super) chunks: usize,
    pub(super) max_chunk_bytes: usize,
    pub(super) plugin_server_ms: f64,
    pub(super) plugin_encode_ms: f64,
    pub(super) reassembly_ms: f64,
    pub(super) json_parse_ms: f64,
}

pub(super) enum BridgeResponse {
    Json(Value),
    Chunk(BridgeChunk),
}

#[derive(Debug)]
pub(super) struct BridgeApplicationError {
    pub(super) method: String,
    pub(super) message: String,
}

impl std::fmt::Display for BridgeApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Bridge method {} failed: {}",
            self.method, self.message
        )
    }
}

impl std::error::Error for BridgeApplicationError {}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct BridgeInfoPayload {
    pub(super) runtime_id: String,
    pub(super) launch_nonce: String,
    pub(super) launch_edit_runtime_id: String,
    pub(super) bridge_version: String,
    pub(super) bridge_build_unix: i64,
    pub(super) bridge_role: String,
    pub(super) player_name: String,
    pub(super) player_user_id: Option<i64>,
    pub(super) place_id: Option<i64>,
    pub(super) game_id: Option<i64>,
    pub(super) place_name: String,
    pub(super) protocol_version: String,
    pub(super) codec_version: String,
    pub(super) chunk_frame_protocol_version: String,
    pub(super) compact_value_protocol_version: String,
    pub(super) performance_mode: String,
    pub(super) export_all_properties: bool,
    pub(super) modified_default_bypass: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BridgePerformanceStats {
    pub(super) frame_ms: Option<f64>,
    pub(super) last_frame_ms: Option<f64>,
    pub(super) max_frame_ms: Option<f64>,
    pub(super) stall_count_over_33_ms: Option<u64>,
    pub(super) stall_count_over_50_ms: Option<u64>,
    pub(super) stall_count_over_100_ms: Option<u64>,
    pub(super) modified_default_checks: Option<u64>,
    pub(super) modified_default_elided: Option<u64>,
    pub(super) modified_default_validation_reads: Option<u64>,
    pub(super) modified_default_runtime_denylist_count: Option<u64>,
    pub(super) properties_read: Option<u64>,
    pub(super) properties_encoded: Option<u64>,
    pub(super) properties_default_skipped: Option<u64>,
    pub(super) safe_read_class_fallback_count: Option<u64>,
    pub(super) safe_read_property_fallback_count: Option<u64>,
}

#[derive(Default)]
pub(super) struct SourceBatchMap {
    pub(super) by_index: HashMap<usize, String>,
    pub(super) by_key: HashMap<String, String>,
}

pub(super) struct BridgeSocket {
    pub(super) port: u16,
    pub(super) peer: String,
    pub(super) role: String,
    pub(super) last_focused_at: Instant,
    pub(super) bridge_info: BridgeInfoPayload,
    pub(super) request_session_id: String,
    pub(super) pending_final_console_snapshots: Vec<Value>,
    pub(super) socket: WebSocket<TcpStream>,
}

pub(super) struct BridgeChannel {
    pub(super) port: u16,
    pub(super) sockets: Mutex<HashMap<String, BridgeSocket>>,
}

pub(super) struct BridgeServer {
    pub(super) channels: Vec<Arc<BridgeChannel>>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) next_id: std::sync::atomic::AtomicU64,
    pub(super) preferred_index: std::sync::atomic::AtomicUsize,

    pub(super) request_gate: Mutex<()>,

    pub(super) runtime_pins: Mutex<HashMap<RuntimePinKey, RuntimePin>>,
    pub(super) final_console_snapshots: Mutex<HashMap<String, FinalConsoleSnapshot>>,
}

pub(super) struct FinalConsoleSnapshot {
    pub(super) received_at: Instant,
    pub(super) payload: Value,
}

pub(super) struct BridgeListenMetrics {
    pub(super) bind_ms: f64,
    pub(super) wait_for_channels_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum BridgeTarget {
    Edit,
    Main,
    Client,
}

impl BridgeTarget {
    pub(super) const fn main_or_client(client: bool) -> Self {
        if client { Self::Client } else { Self::Main }
    }

    fn preferred_roles(self) -> &'static [&'static str] {
        match self {
            Self::Edit => &[BRIDGE_ROLE_EDIT],
            Self::Main => &[
                BRIDGE_ROLE_PLAY_SERVER,
                BRIDGE_ROLE_EDIT,
                BRIDGE_ROLE_UNKNOWN,
            ],
            Self::Client => &[BRIDGE_ROLE_PLAY_CLIENT],
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct RuntimePinKey {
    pub(super) target: BridgeTarget,
    pub(super) player: Option<String>,
}
#[derive(Clone)]
pub(super) struct RuntimePin {
    pub(super) runtime_id: String,
}

struct BridgeCallContext<'a> {
    id: u64,
    method: &'a str,
    params: &'a Value,
    target: BridgeTarget,
    player: Option<&'a str>,
    runtime_pin: &'a RuntimePin,
    start: usize,
    response_deadline: Option<Instant>,
}

pub(super) struct RuntimePinCandidate {
    pub(super) ports: HashSet<u16>,
    pub(super) role_rank: usize,
    pub(super) last_focused_at: Instant,
}

fn normalize_bridge_role(role: &str) -> &'static str {
    match role.trim().to_ascii_lowercase().as_str() {
        "" | "edit" | "studio" | "plugin" => BRIDGE_ROLE_EDIT,
        "server" | "play" | "play-server" => BRIDGE_ROLE_PLAY_SERVER,
        "client" | "local" | "play-client" => BRIDGE_ROLE_PLAY_CLIENT,
        _ => BRIDGE_ROLE_UNKNOWN,
    }
}

impl BridgeServer {
    pub(super) fn acquire_request_gate(&self, timeout: Duration) -> Result<MutexGuard<'_, ()>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.request_gate.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                    bail!(
                        "Renium daemon is busy with another bridge request; retry after it completes"
                    );
                }
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    pub(super) fn listen(
        host: &str,
        ports: &[u16],
        wait_seconds: f64,
    ) -> Result<(Self, BridgeListenMetrics)> {
        Self::listen_with_initial_wait(host, ports, wait_seconds, true)
    }

    pub(super) fn listen_with_initial_wait(
        host: &str,
        ports: &[u16],
        wait_seconds: f64,
        wait_for_initial_channels: bool,
    ) -> Result<(Self, BridgeListenMetrics)> {
        let bind_host = normalize_loopback_host(host)?;
        let bind_started = Instant::now();
        let alive = Arc::new(AtomicBool::new(true));
        let mut channels: Vec<Arc<BridgeChannel>> = Vec::with_capacity(ports.len());
        let request_session_id = format!(
            "{:x}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        );

        for port in ports {
            let listener = TcpListener::bind((bind_host.as_str(), *port)).with_context(|| {
                format!(
                    "Failed to bind bridge server on {bind_host}:{port}; close the Renium process using that port or run `rbx daemon list` and `rbx daemon stop --all`"
                )
            })?;
            listener.set_nonblocking(true).with_context(|| {
                format!("Failed to set nonblocking listener {bind_host}:{port}")
            })?;
            println!("[renium] bridge listening on {bind_host}:{port}");

            let channel = Arc::new(BridgeChannel {
                port: *port,
                sockets: Mutex::new(HashMap::new()),
            });
            Self::spawn_accept_loop(
                bind_host.clone(),
                *port,
                listener,
                channel.clone(),
                alive.clone(),
                request_session_id.clone(),
            );
            channels.push(channel);
        }

        #[cfg(any(windows, target_os = "macos"))]
        Self::spawn_focus_watcher(channels.clone(), alive.clone());

        let bind_ms = elapsed_ms(bind_started);
        let wait_started = Instant::now();
        let server = Self {
            channels,
            alive,
            next_id: std::sync::atomic::AtomicU64::new(21335),
            preferred_index: std::sync::atomic::AtomicUsize::new(0),
            request_gate: Mutex::new(()),
            runtime_pins: Mutex::new(HashMap::new()),
            final_console_snapshots: Mutex::new(HashMap::new()),
        };

        let required_channels = server.channels.len();
        if required_channels == 0 {
            bail!("No bridge ports configured");
        }

        if wait_for_initial_channels {
            let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds.max(1.0));
            let mut last_ready_channels = 0usize;
            while Instant::now() < deadline {
                let ready_channels = server.max_runtime_channel_coverage(BridgeTarget::Main, None);
                if ready_channels != last_ready_channels {
                    last_ready_channels = ready_channels;
                    println!(
                        "[renium] bridge ready channels: {ready_channels}/{required_channels}"
                    );
                }
                if ready_channels >= required_channels {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }

            let ready_channels = server.max_runtime_channel_coverage(BridgeTarget::Main, None);
            if ready_channels < required_channels {
                let missing_ports = server.missing_ports_for_target(BridgeTarget::Main);
                bail!(
                    "Only {}/{} plugin bridge channels connected within {:.1}s; all {} are required for stable full-speed export. Missing ports: {:?}",
                    ready_channels,
                    required_channels,
                    wait_seconds.max(1.0),
                    required_channels,
                    missing_ports
                );
            }

            println!("[renium] bridge all channels ready: {ready_channels}/{required_channels}");
        } else {
            println!(
                "[renium] bridge serving on {required_channels} port(s); waiting for plugin clients on demand"
            );
        }
        Ok((
            server,
            BridgeListenMetrics {
                bind_ms,
                wait_for_channels_ms: elapsed_ms(wait_started),
            },
        ))
    }

    pub(super) fn spawn_accept_loop(
        bind_host: String,
        port: u16,
        listener: TcpListener,
        channel: Arc<BridgeChannel>,
        alive: Arc<AtomicBool>,
        request_session_id: String,
    ) {
        thread::spawn(move || {
            while alive.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let peer = addr.to_string();
                        match Self::accept_ready_socket(
                            &bind_host,
                            port,
                            stream,
                            peer.clone(),
                            &request_session_id,
                        ) {
                            Ok(socket) => {
                                let mut guard = channel
                                    .sockets
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                let role = socket.role.clone();
                                let socket_key =
                                    Self::bridge_socket_key(&guard, &role, &socket.peer);
                                if socket_key == role {
                                    println!(
                                        "[renium] bridge channel ready on {}:{} role={} from {} build={}",
                                        bind_host,
                                        port,
                                        socket.role,
                                        socket.peer,
                                        socket.bridge_info.bridge_build_unix
                                    );
                                }
                                guard.insert(socket_key, socket);
                            }
                            Err(err) => {
                                println!(
                                    "[renium] warning: bridge channel handshake failed on {bind_host}:{port} from {peer}: {err:#}"
                                );
                            }
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(err) => {
                        if alive.load(Ordering::Relaxed) {
                            println!(
                                "[renium] warning: listener accept failed on {bind_host}:{port}: {err}"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
            }
        });
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(super) fn spawn_focus_watcher(channels: Vec<Arc<BridgeChannel>>, alive: Arc<AtomicBool>) {
        #[cfg(windows)]
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };

        thread::spawn(move || {
            let mut last_pid: u32 = 0;
            while alive.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(300));

                let multiple_plugins = channels.iter().any(|channel| {
                    let guard = channel
                        .sockets
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    guard.len() > 1
                });
                if !multiple_plugins {
                    last_pid = 0;
                    continue;
                }

                #[cfg(windows)]
                let pid = {
                    let mut pid: u32 = 0;
                    unsafe {
                        let hwnd = GetForegroundWindow();
                        if hwnd.is_null() {
                            continue;
                        }
                        GetWindowThreadProcessId(hwnd, &mut pid);
                    }
                    pid
                };
                #[cfg(target_os = "macos")]
                let pid = match input_inject::frontmost_studio_pid() {
                    Some(pid) => pid,
                    None => {
                        last_pid = 0;
                        continue;
                    }
                };
                if pid == 0 || pid == last_pid {
                    continue;
                }
                last_pid = pid;

                let owned_ports = local_tcp_ports_owned_by_pid(pid);
                if owned_ports.is_empty() {
                    continue;
                }

                let now = Instant::now();
                for channel in &channels {
                    let mut guard = channel
                        .sockets
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    for socket in guard.values_mut() {
                        let peer_port = socket
                            .peer
                            .rsplit_once(':')
                            .and_then(|(_, port)| port.parse::<u16>().ok());
                        if let Some(peer_port) = peer_port
                            && owned_ports.contains(&peer_port)
                        {
                            socket.last_focused_at = now;
                        }
                    }
                }
            }
        });
    }

    pub(super) fn accept_ready_socket(
        bind_host: &str,
        port: u16,
        stream: TcpStream,
        peer: String,
        request_session_id: &str,
    ) -> Result<BridgeSocket> {
        let accepted_at = Instant::now();
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

        let socket_config = WebSocketConfig::default()
            .read_buffer_size(256 * 1024)
            .write_buffer_size(32 * 1024)
            .max_write_buffer_size(4 * 1024 * 1024)
            .max_message_size(Some(MAX_BRIDGE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_BRIDGE_MESSAGE_BYTES))
            .accept_unmasked_frames(false);
        let socket = accept_with_config(stream, Some(socket_config))
            .with_context(|| format!("WebSocket upgrade failed on {bind_host}:{port}"))?;

        let mut bridge_socket = BridgeSocket {
            port,
            peer,
            role: BRIDGE_ROLE_UNKNOWN.to_string(),
            last_focused_at: accepted_at,
            bridge_info: BridgeInfoPayload::default(),
            request_session_id: request_session_id.to_string(),
            pending_final_console_snapshots: Vec::new(),
            socket,
        };

        let bridge_info = Self::probe_bridge_info_on_socket_with_id(&mut bridge_socket, 1)
            .with_context(|| format!("readiness getBridgeInfo failed on {bind_host}:{port}"))?;
        bridge_socket.role = normalize_bridge_role(&bridge_info.bridge_role).to_string();
        bridge_socket.bridge_info = bridge_info;

        let _ = bridge_socket
            .socket
            .get_mut()
            .set_read_timeout(Some(BRIDGE_DEFAULT_RESPONSE_TIMEOUT));
        let _ = bridge_socket
            .socket
            .get_mut()
            .set_write_timeout(Some(Duration::from_secs(10)));

        Ok(bridge_socket)
    }

    pub(super) fn probe_bridge_info_on_socket_with_id(
        bridge_socket: &mut BridgeSocket,
        id: u64,
    ) -> Result<BridgeInfoPayload> {
        let value = Self::call_on_socket_with_timeout(
            bridge_socket,
            id,
            "getBridgeInfo",
            &json!({}),
            None,
        )?;
        let info: BridgeInfoPayload =
            serde_json::from_value(value).context("Invalid getBridgeInfo response from plugin")?;
        validate_bridge_info(&info)?;
        Ok(info)
    }

    pub(super) fn bridge_role_key_base(role_key: &str) -> &str {
        role_key
            .split_once(BRIDGE_DUPLICATE_ROLE_KEY_SEPARATOR)
            .map_or(role_key, |(base, _)| base)
    }

    pub(super) fn bridge_socket_key(
        sockets: &HashMap<String, BridgeSocket>,
        role: &str,
        peer: &str,
    ) -> String {
        if !sockets.contains_key(role) {
            return role.to_string();
        }

        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let peer_key = peer.replace(BRIDGE_DUPLICATE_ROLE_KEY_SEPARATOR, "_");
        let mut suffix = 0usize;
        loop {
            let candidate = format!(
                "{role}{BRIDGE_DUPLICATE_ROLE_KEY_SEPARATOR}{now_nanos:x}-{peer_key}-{suffix}"
            );
            if !sockets.contains_key(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    pub(super) fn role_matches_target(role_key: &str, target: BridgeTarget) -> bool {
        let role = Self::bridge_role_key_base(role_key);
        match target {
            BridgeTarget::Edit => role == BRIDGE_ROLE_EDIT,
            BridgeTarget::Main => {
                role == BRIDGE_ROLE_EDIT
                    || role == BRIDGE_ROLE_PLAY_SERVER
                    || role == BRIDGE_ROLE_UNKNOWN
            }
            BridgeTarget::Client => role == BRIDGE_ROLE_PLAY_CLIENT,
        }
    }

    pub(super) fn player_matches_selector(info: &BridgeInfoPayload, selector: &str) -> bool {
        let selector = selector.trim();
        if selector.is_empty() {
            return true;
        }
        let name = info.player_name.trim();
        if !name.is_empty() && name.eq_ignore_ascii_case(selector) {
            return true;
        }
        if let Ok(index) = selector.parse::<i64>() {
            if !name.is_empty() && name.eq_ignore_ascii_case(&format!("Player{index}")) {
                return true;
            }
            if info.player_user_id == Some(-index) {
                return true;
            }
        }
        false
    }

    pub(super) fn socket_matches_selector(
        role_key: &str,
        socket: &BridgeSocket,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> bool {
        if !Self::role_matches_target(role_key, target) {
            return false;
        }
        if let Some(place) = place_filter()
            && !place_matches(&socket.bridge_info, &place)
        {
            return false;
        }
        if let Some(runtime_id) = crate::runtime_context::automation_runtime()
            && socket.bridge_info.runtime_id != runtime_id
        {
            return false;
        }
        player.is_none_or(|selector| Self::player_matches_selector(&socket.bridge_info, selector))
    }

    pub(super) fn distinct_places_for_selector(
        sockets: &HashMap<String, BridgeSocket>,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Vec<(Option<i64>, String)> {
        let mut places: Vec<(Option<i64>, String)> = Vec::new();
        for (role_key, socket) in sockets {
            if !Self::socket_matches_selector(role_key.as_str(), socket, target, player) {
                continue;
            }
            let info = &socket.bridge_info;
            if info.place_name.trim().is_empty() && info.place_id.is_none() {
                continue;
            }
            let entry = (info.place_id, info.place_name.clone());
            if !places.contains(&entry) {
                places.push(entry);
            }
        }
        places
    }

    pub(super) fn ensure_place_unambiguous(
        sockets: &mut HashMap<String, BridgeSocket>,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<()> {
        let mut places = Self::distinct_places_for_selector(sockets, target, player);
        if places.len() > 1 {
            let dead_keys: Vec<String> = sockets
                .iter_mut()
                .filter_map(|(key, socket)| {
                    if Self::socket_is_alive(socket) {
                        None
                    } else {
                        Some(key.clone())
                    }
                })
                .collect();
            for key in dead_keys {
                if let Some(mut dead_socket) = sockets.remove(&key) {
                    Self::close_socket(&mut dead_socket);
                }
            }
            places = Self::distinct_places_for_selector(sockets, target, player);
        }
        if places.len() > 1 {
            let listing = places
                .iter()
                .map(|(id, name)| {
                    id.as_ref().map_or_else(
                        || format!("'{name}'"),
                        |id| format!("'{name}' (placeId {id})"),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Multiple Studio places are connected and match this command: {listing}. Pin \
                 commands to one place with --place <name|id> or the RENIUM_PLACE env var"
            );
        }
        Ok(())
    }

    pub(super) fn target_label(target: BridgeTarget) -> &'static str {
        match target {
            BridgeTarget::Edit => "edit",
            BridgeTarget::Main => "main",
            BridgeTarget::Client => "play-client",
        }
    }

    pub(super) fn role_preference_rank(role_key: &str, target: BridgeTarget) -> usize {
        let role = Self::bridge_role_key_base(role_key);
        match target {
            BridgeTarget::Edit => usize::from(role != BRIDGE_ROLE_EDIT),
            BridgeTarget::Main => match role {
                BRIDGE_ROLE_PLAY_SERVER => 0,
                BRIDGE_ROLE_EDIT => 1,
                BRIDGE_ROLE_UNKNOWN => 2,
                _ => 3,
            },
            BridgeTarget::Client => usize::from(role != BRIDGE_ROLE_PLAY_CLIENT),
        }
    }

    pub(super) fn runtime_pin_key(target: BridgeTarget, player: Option<&str>) -> RuntimePinKey {
        RuntimePinKey {
            target,
            player: player.map(|value| value.trim().to_ascii_lowercase()),
        }
    }

    pub(super) fn clear_runtime_pins(&self) {
        self.runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(super) fn choose_runtime_pin(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<RuntimePin> {
        let mut candidates: HashMap<String, RuntimePinCandidate> = HashMap::new();
        let mut matching_socket_count = 0usize;

        for channel in &self.channels {
            let mut guard = channel
                .sockets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::ensure_place_unambiguous(&mut guard, target, player)?;
            for (role_key, socket) in guard.iter() {
                if !Self::socket_matches_selector(role_key, socket, target, player) {
                    continue;
                }
                matching_socket_count += 1;
                let runtime_id = socket.bridge_info.runtime_id.trim();
                if runtime_id.is_empty() {
                    continue;
                }
                let role_rank = Self::role_preference_rank(role_key, target);
                let candidate = candidates.entry(runtime_id.to_string()).or_insert_with(|| {
                    RuntimePinCandidate {
                        ports: HashSet::new(),
                        role_rank,
                        last_focused_at: socket.last_focused_at,
                    }
                });
                candidate.ports.insert(channel.port);
                candidate.role_rank = candidate.role_rank.min(role_rank);
                candidate.last_focused_at = candidate.last_focused_at.max(socket.last_focused_at);
            }
        }

        if let Some((runtime_id, _)) = candidates.into_iter().max_by(|(_, left), (_, right)| {
            right
                .role_rank
                .cmp(&left.role_rank)
                .then_with(|| left.ports.len().cmp(&right.ports.len()))
                .then_with(|| left.last_focused_at.cmp(&right.last_focused_at))
        }) {
            return Ok(RuntimePin { runtime_id });
        }

        if matching_socket_count == 0 {
            bail!(
                "No connected {} bridge{} found",
                Self::target_label(target),
                player
                    .map(|selector| format!(" for player {selector}"))
                    .unwrap_or_default()
            );
        }
        bail!("Matching Studio bridge omitted its runtime identity; reinstall the Renium plugin")
    }

    pub(super) fn runtime_pin_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<RuntimePin> {
        let key = Self::runtime_pin_key(target, player);
        let existing = self
            .runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(pin) = existing {
            return Ok(pin);
        }
        let pin = self.choose_runtime_pin(target, player)?;
        let mut pins = self
            .runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(pins.entry(key).or_insert(pin).clone())
    }

    pub(super) fn socket_matches_runtime_pin(socket: &BridgeSocket, pin: &RuntimePin) -> bool {
        socket.bridge_info.runtime_id == pin.runtime_id
    }

    pub(super) fn select_role_for_selector_with_pin(
        sockets: &HashMap<String, BridgeSocket>,
        target: BridgeTarget,
        player: Option<&str>,
        pin: &RuntimePin,
    ) -> Option<String> {
        Self::select_role_for_selector_inner(sockets, target, player, Some(pin))
    }

    fn select_role_for_selector_inner(
        sockets: &HashMap<String, BridgeSocket>,
        target: BridgeTarget,
        player: Option<&str>,
        pin: Option<&RuntimePin>,
    ) -> Option<String> {
        let matches = |key: &str, socket: &BridgeSocket| {
            Self::socket_matches_selector(key, socket, target, player)
                && pin.is_none_or(|pin| Self::socket_matches_runtime_pin(socket, pin))
        };
        for role in target.preferred_roles() {
            if let Some(key) = sockets
                .iter()
                .filter(|(key, socket)| {
                    Self::bridge_role_key_base(key.as_str()) == *role
                        && matches(key.as_str(), socket)
                })
                .max_by_key(|(_, socket)| socket.last_focused_at)
                .map(|(key, _)| key.clone())
            {
                return Some(key);
            }
        }
        sockets
            .iter()
            .filter(|(key, socket)| matches(key.as_str(), socket))
            .max_by_key(|(_, socket)| socket.last_focused_at)
            .map(|(key, _)| key.clone())
    }

    pub(super) fn wait_for_ready_channels_for_target(
        &self,
        required_channels: usize,
        timeout: Duration,
        target: BridgeTarget,
    ) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let key = Self::runtime_pin_key(target, None);
            let already_pinned = self
                .runtime_pins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&key);
            let coverage = self.max_runtime_channel_coverage(target, None);
            let ready_channels = if already_pinned || coverage >= required_channels {
                self.channel_count_for_target(target)
            } else {
                coverage
            };
            if ready_channels >= required_channels || Instant::now() >= deadline {
                return ready_channels;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    pub(super) fn wait_for_target(&self, wait_seconds: f64, target: BridgeTarget) -> Result<()> {
        let required_channels = self.expected_channel_count();
        let ready_channels = self.wait_for_ready_channels_for_target(
            required_channels,
            Duration::from_secs_f64(wait_seconds.max(1.0)),
            target,
        );
        if ready_channels < required_channels {
            bail!(
                "Only {}/{} persistent {} plugin bridge channels are ready{}. Missing ports: {:?}",
                ready_channels,
                required_channels,
                Self::target_label(target),
                place_filter()
                    .map(|place| format!(" for place filter '{place}'"))
                    .unwrap_or_default(),
                self.missing_ports_for_target(target)
            );
        }
        validate_bridge_info(&self.cached_bridge_info_for_target(target)?)
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(super) fn peer_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<String> {
        let runtime_pin = self.runtime_pin_for_selector(target, player)?;
        for channel in &self.channels {
            let mut guard = match channel.sockets.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            Self::ensure_place_unambiguous(&mut guard, target, player)?;
            if let Some(role) =
                Self::select_role_for_selector_with_pin(&guard, target, player, &runtime_pin)
                && let Some(socket) = guard.get(&role)
            {
                return Ok(socket.peer.clone());
            }
        }
        bail!(
            "No connected {} bridge{} found",
            Self::target_label(target),
            player
                .map(|selector| format!(" for player {selector}"))
                .unwrap_or_default()
        )
    }

    pub(super) fn cached_bridge_info_for_target(
        &self,
        target: BridgeTarget,
    ) -> Result<BridgeInfoPayload> {
        let runtime_pin = self.runtime_pin_for_selector(target, None)?;
        for channel in &self.channels {
            let guard = match channel.sockets.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            if let Some(role) =
                Self::select_role_for_selector_with_pin(&guard, target, None, &runtime_pin)
                && let Some(socket) = guard.get(&role)
            {
                return Ok(socket.bridge_info.clone());
            }
        }
        bail!(
            "No cached {} plugin bridge info is available; no ready bridge channels",
            Self::target_label(target)
        );
    }

    pub(super) fn cache_export_options_for_target(
        &self,
        target: BridgeTarget,
        performance_mode: &str,
        modified_default_bypass: bool,
        export_all_properties: bool,
    ) {
        let Ok(runtime_pin) = self.runtime_pin_for_selector(target, None) else {
            return;
        };
        for channel in &self.channels {
            let mut guard = channel
                .sockets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (role_key, socket) in guard.iter_mut() {
                if Self::socket_matches_selector(role_key, socket, target, None)
                    && Self::socket_matches_runtime_pin(socket, &runtime_pin)
                {
                    socket.bridge_info.performance_mode = performance_mode.to_string();
                    socket.bridge_info.modified_default_bypass = modified_default_bypass;
                    socket.bridge_info.export_all_properties = export_all_properties;
                }
            }
        }
    }

    pub(super) fn expected_channel_count(&self) -> usize {
        self.channels.len()
    }

    pub(super) fn missing_ports_for_target(&self, target: BridgeTarget) -> Vec<u16> {
        self.channels
            .iter()
            .filter_map(|channel| {
                let guard = match channel.sockets.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => return None,
                };
                if Self::select_role_for_selector_inner(&guard, target, None, None).is_some() {
                    None
                } else {
                    Some(channel.port)
                }
            })
            .collect()
    }

    pub(super) fn close_socket(socket: &mut BridgeSocket) {
        let _ = socket.socket.close(None);
        let _ = socket.socket.get_mut().shutdown(Shutdown::Both);
    }

    pub(super) fn is_transport_error_text(text: &str) -> bool {
        text.contains("Bridge send failed")
            || text.contains("Bridge read failed")
            || text.contains("Bridge closed while waiting")
            || text.contains("Connection reset")
            || text.contains("connection reset")
            || text.contains("Broken pipe")
            || text.contains("broken pipe")
            || text.contains("WouldBlock")
            || text.contains("TimedOut")
            || text.contains("Bridge response timed out")
    }

    pub(super) fn retire_failed_socket(
        sockets: &mut HashMap<String, BridgeSocket>,
        socket_role: &str,
        socket_port: u16,
        role: &str,
        error: anyhow::Error,
    ) -> Result<String> {
        if error.downcast_ref::<BridgeApplicationError>().is_some() {
            return Err(error);
        }
        let error_text = format!("{error:#}");
        if !Self::is_transport_error_text(&error_text) {
            return Err(error);
        }
        if let Some(mut socket) = sockets.remove(socket_role) {
            Self::close_socket(&mut socket);
        }
        Ok(format!("port {socket_port} role {role}: {error_text}"))
    }

    fn try_call_pinned_socket<T>(
        &self,
        context: &BridgeCallContext<'_>,
        validate_place: bool,
        last_error: &mut Option<String>,
        call: fn(&mut BridgeSocket, u64, &str, &Value, Option<Duration>) -> Result<T>,
    ) -> Result<(Option<T>, bool)> {
        let mut attempted_socket = false;
        for offset in 0..self.channels.len() {
            let channel = &self.channels[(context.start + offset) % self.channels.len()];
            let mut sockets = match channel.sockets.try_lock() {
                Ok(sockets) => sockets,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            if validate_place {
                Self::ensure_place_unambiguous(&mut sockets, context.target, context.player)?;
            }
            let Some(socket_role) = Self::select_role_for_selector_with_pin(
                &sockets,
                context.target,
                context.player,
                context.runtime_pin,
            ) else {
                continue;
            };
            let Some(socket) = sockets.get_mut(&socket_role) else {
                continue;
            };
            attempted_socket = true;
            let socket_port = socket.port;
            let role = socket.role.clone();
            let remaining_timeout = context
                .response_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if remaining_timeout.is_some_and(|timeout| timeout.is_zero()) {
                bail!(
                    "Bridge call {} exceeded its response deadline",
                    context.method
                );
            }
            let result = call(
                socket,
                context.id,
                context.method,
                context.params,
                remaining_timeout,
            );
            self.collect_socket_final_console_snapshots(socket);
            match result {
                Ok(result) => return Ok((Some(result), true)),
                Err(error) => {
                    *last_error = Some(Self::retire_failed_socket(
                        &mut sockets,
                        &socket_role,
                        socket_port,
                        &role,
                        error,
                    )?);
                }
            }
        }
        Ok((None, attempted_socket))
    }

    fn call_pinned_socket<T>(
        &self,
        context: &BridgeCallContext<'_>,
        validate_place: bool,
        call: fn(&mut BridgeSocket, u64, &str, &Value, Option<Duration>) -> Result<T>,
        label: &str,
    ) -> Result<T> {
        let mut last_error = None;
        for _ in 0..64 {
            let (result, _) =
                self.try_call_pinned_socket(context, validate_place, &mut last_error, call)?;
            if let Some(result) = result {
                return Ok(result);
            }
            thread::yield_now();
        }

        let mut lock_deadline = Instant::now() + bridge_channel_lock_timeout(context.method);
        if let Some(response_deadline) = context.response_deadline {
            lock_deadline = lock_deadline.min(response_deadline);
        }
        while Instant::now() < lock_deadline {
            let (result, attempted_socket) =
                self.try_call_pinned_socket(context, false, &mut last_error, call)?;
            if let Some(result) = result {
                return Ok(result);
            }
            if !attempted_socket && last_error.is_none() {
                last_error = Some("all compatible bridge channels are busy".to_string());
            }
            thread::sleep(Duration::from_millis(2));
        }

        bail!(
            "{label} failed for {} on {} target{}: {}",
            context.method,
            Self::target_label(context.target),
            context
                .player
                .map(|selector| format!(" (player {selector})"))
                .unwrap_or_default(),
            last_error.unwrap_or_else(|| {
                "the pinned Studio runtime disconnected or has no available channel".to_string()
            })
        )
    }

    pub(super) fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_for_target(method, params, BridgeTarget::Main)
    }

    pub(super) fn call_for_target(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
    ) -> Result<Value> {
        self.call_for_selector(method, params, target, None)
    }

    pub(super) fn call_for_selector(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<Value> {
        self.call_for_selector_with_timeout(method, params, target, player, None)
    }

    pub(super) fn call_for_selector_with_timeout(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        player: Option<&str>,
        response_timeout: Option<Duration>,
    ) -> Result<Value> {
        self.call_for_selector_runtime_with_timeout(
            method,
            params,
            target,
            player,
            None,
            response_timeout,
        )
    }

    pub(super) fn call_for_runtime_with_timeout(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        runtime_id: &str,
        response_timeout: Option<Duration>,
    ) -> Result<Value> {
        self.call_for_selector_runtime_with_timeout(
            method,
            params,
            target,
            None,
            Some(runtime_id),
            response_timeout,
        )
    }

    pub(super) fn call_for_selector_runtime_with_timeout(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        player: Option<&str>,
        runtime_id: Option<&str>,
        response_timeout: Option<Duration>,
    ) -> Result<Value> {
        let runtime_pin = if let Some(runtime_id) = runtime_id {
            RuntimePin {
                runtime_id: runtime_id.to_string(),
            }
        } else {
            self.runtime_pin_for_selector(target, player)?
        };
        let (id, start) = self.next_call()?;
        let response_deadline = response_timeout.map(|timeout| Instant::now() + timeout);
        let call_context = BridgeCallContext {
            id,
            method,
            params: &params,
            target,
            player,
            runtime_pin: &runtime_pin,
            start,
            response_deadline,
        };

        self.call_pinned_socket(
            &call_context,
            runtime_id.is_none(),
            Self::call_on_socket_with_timeout,
            "Bridge call",
        )
    }

    pub(super) fn channel_count_for_target(&self, target: BridgeTarget) -> usize {
        self.channel_count_for_selector(target, None)
    }

    pub(super) fn max_runtime_channel_coverage(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> usize {
        let mut ports_by_runtime: HashMap<String, HashSet<u16>> = HashMap::new();
        for channel in &self.channels {
            let guard = match channel.sockets.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            for (role_key, socket) in guard.iter() {
                if !Self::socket_matches_selector(role_key, socket, target, player) {
                    continue;
                }
                let runtime_id = socket.bridge_info.runtime_id.trim();
                if !runtime_id.is_empty() {
                    ports_by_runtime
                        .entry(runtime_id.to_string())
                        .or_default()
                        .insert(channel.port);
                }
            }
        }
        ports_by_runtime
            .values()
            .map(HashSet::len)
            .max()
            .unwrap_or(0)
    }

    pub(super) fn channel_count_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> usize {
        let Ok(pin) = self.runtime_pin_for_selector(target, player) else {
            return 0;
        };
        self.channels
            .iter()
            .filter(|channel| {
                let guard = match channel.sockets.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => return false,
                };
                Self::select_role_for_selector_with_pin(&guard, target, player, &pin).is_some()
            })
            .count()
    }

    pub(super) fn wait_for_ready_player(&self, player: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.channel_count_for_selector(BridgeTarget::Client, Some(player)) >= 1 {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub(super) fn capture_final_console_message(socket: &mut BridgeSocket, value: &Value) -> bool {
        if value.get("event").and_then(Value::as_str) != Some("finalConsoleSnapshot") {
            return false;
        }
        if value.get("runtimeId").and_then(Value::as_str).is_none()
            || value.get("launchNonce").and_then(Value::as_str).is_none()
            || value
                .get("launchEditRuntimeId")
                .and_then(Value::as_str)
                .is_none()
            || !value.get("snapshot").is_some_and(Value::is_object)
        {
            return true;
        }
        socket.pending_final_console_snapshots.push(value.clone());
        true
    }

    pub(super) fn retain_socket_final_console_snapshots(&self, socket: &mut BridgeSocket) {
        if socket.pending_final_console_snapshots.is_empty() {
            return;
        }
        let mut snapshots = self
            .final_console_snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        snapshots.retain(|_, snapshot| snapshot.received_at.elapsed() < Duration::from_secs(300));
        for payload in socket.pending_final_console_snapshots.drain(..) {
            let runtime_id = payload
                .get("runtimeId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let launch_nonce = payload
                .get("launchNonce")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let epoch = payload
                .pointer("/snapshot/epoch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = format!("{launch_nonce}\u{1f}{runtime_id}\u{1f}{epoch}");
            snapshots.insert(
                key,
                FinalConsoleSnapshot {
                    received_at: Instant::now(),
                    payload,
                },
            );
        }
    }

    pub(super) fn collect_socket_final_console_snapshots(&self, socket: &mut BridgeSocket) -> bool {
        let mut closed = false;
        if socket.socket.get_mut().set_nonblocking(true).is_ok() {
            loop {
                match socket.socket.read() {
                    Ok(Message::Text(text)) => {
                        if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                            Self::capture_final_console_message(socket, &value);
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        let _ = socket.socket.send(Message::Pong(payload));
                    }
                    Err(tungstenite::Error::Io(error))
                        if error.kind() == io::ErrorKind::WouldBlock =>
                    {
                        break;
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        closed = true;
                        break;
                    }
                    Ok(_) => {}
                }
            }
            let _ = socket.socket.get_mut().set_nonblocking(false);
        }
        self.retain_socket_final_console_snapshots(socket);
        closed
    }

    pub(super) fn take_final_console_snapshots(&self, launch: &TestLaunch) -> Vec<Value> {
        let mut snapshots = self
            .final_console_snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        snapshots.retain(|_, snapshot| snapshot.received_at.elapsed() < Duration::from_secs(300));
        snapshots
            .extract_if(|_, snapshot| {
                snapshot.payload.get("launchNonce").and_then(Value::as_str)
                    == Some(launch.nonce.as_str())
                    && snapshot
                        .payload
                        .get("launchEditRuntimeId")
                        .and_then(Value::as_str)
                        == Some(launch.edit_runtime_id.as_str())
            })
            .map(|(_, snapshot)| snapshot.payload)
            .collect()
    }

    pub(super) fn socket_is_alive(socket: &mut BridgeSocket) -> bool {
        let stream = socket.socket.get_ref();
        if stream.set_nonblocking(true).is_err() {
            return true;
        }
        let mut probe = [0u8; 1];
        let peek = stream.peek(&mut probe);
        let _ = stream.set_nonblocking(false);
        match peek {
            Ok(0) => false,
            Err(err) if err.kind() != io::ErrorKind::WouldBlock => false,
            _ => {
                socket.socket.send(Message::Ping(Vec::new().into())).is_ok()
                    && socket.socket.flush().is_ok()
            }
        }
    }

    pub(super) fn list_bridge_clients(&self) -> Vec<Value> {
        struct ClientEntry {
            runtime_id: String,
            launch_nonce: String,
            launch_edit_runtime_id: String,
            role: String,
            player_name: String,
            player_user_id: Option<i64>,
            place_id: Option<i64>,
            game_id: Option<i64>,
            place_name: String,
            build_unix: i64,
            ports: Vec<u16>,
        }
        let mut entries: Vec<ClientEntry> = Vec::new();
        for channel in &self.channels {
            let mut guard = match channel.sockets.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            let mut dead_keys = Vec::new();
            for (role_key, socket) in guard.iter_mut() {
                if self.collect_socket_final_console_snapshots(socket)
                    || !Self::socket_is_alive(socket)
                {
                    dead_keys.push(role_key.clone());
                    continue;
                }
                if Self::bridge_role_key_base(role_key) == BRIDGE_ROLE_PLAY_CLIENT
                    && socket.bridge_info.player_name.trim().is_empty()
                {
                    let id = self
                        .next_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Ok(info) = Self::probe_bridge_info_on_socket_with_id(socket, id) {
                        socket.role = normalize_bridge_role(&info.bridge_role).to_string();
                        socket.bridge_info = info;
                    }
                }
            }
            for key in dead_keys {
                if let Some(mut dead_socket) = guard.remove(&key) {
                    Self::close_socket(&mut dead_socket);
                }
            }
            for (role_key, socket) in guard.iter() {
                let role = Self::bridge_role_key_base(role_key).to_string();
                let info = &socket.bridge_info;
                let existing = entries.iter_mut().find(|entry| {
                    entry.runtime_id == info.runtime_id
                        && entry.launch_nonce == info.launch_nonce
                        && entry.launch_edit_runtime_id == info.launch_edit_runtime_id
                        && entry.role == role
                        && entry.player_name == info.player_name
                        && entry.player_user_id == info.player_user_id
                        && entry.place_id == info.place_id
                        && entry.game_id == info.game_id
                        && entry.place_name == info.place_name
                });
                match existing {
                    Some(entry) => {
                        if !entry.ports.contains(&socket.port) {
                            entry.ports.push(socket.port);
                        }
                    }
                    None => entries.push(ClientEntry {
                        runtime_id: info.runtime_id.clone(),
                        launch_nonce: info.launch_nonce.clone(),
                        launch_edit_runtime_id: info.launch_edit_runtime_id.clone(),
                        role,
                        player_name: info.player_name.clone(),
                        player_user_id: info.player_user_id,
                        place_id: info.place_id,
                        game_id: info.game_id,
                        place_name: info.place_name.clone(),
                        build_unix: info.bridge_build_unix,
                        ports: vec![socket.port],
                    }),
                }
            }
        }
        entries.sort_by(|a, b| {
            (&a.place_name, &a.role, &a.player_name, &a.runtime_id).cmp(&(
                &b.place_name,
                &b.role,
                &b.player_name,
                &b.runtime_id,
            ))
        });
        entries
            .into_iter()
            .map(|mut entry| {
                entry.ports.sort_unstable();
                let mut object = serde_json::Map::new();
                if !entry.runtime_id.is_empty() {
                    object.insert("runtimeId".to_string(), json!(entry.runtime_id));
                }
                if !entry.launch_nonce.is_empty() {
                    object.insert("launchNonce".to_string(), json!(entry.launch_nonce));
                }
                if !entry.launch_edit_runtime_id.is_empty() {
                    object.insert(
                        "launchEditRuntimeId".to_string(),
                        json!(entry.launch_edit_runtime_id),
                    );
                }
                object.insert("role".to_string(), json!(entry.role));
                if !entry.player_name.is_empty() {
                    object.insert("playerName".to_string(), json!(entry.player_name));
                }
                if let Some(user_id) = entry.player_user_id {
                    object.insert("playerUserId".to_string(), json!(user_id));
                }
                if !entry.place_name.is_empty() {
                    object.insert("placeName".to_string(), json!(entry.place_name));
                }
                if let Some(place_id) = entry.place_id {
                    object.insert("placeId".to_string(), json!(place_id));
                }
                if let Some(game_id) = entry.game_id {
                    object.insert("gameId".to_string(), json!(game_id));
                }
                object.insert("bridgeBuildUnix".to_string(), json!(entry.build_unix));
                object.insert("channels".to_string(), json!(entry.ports.len()));
                object.insert("ports".to_string(), json!(entry.ports));
                Value::Object(object)
            })
            .collect()
    }

    pub(super) fn channel_count(&self) -> usize {
        self.channel_count_for_target(BridgeTarget::Main)
    }

    pub(super) fn call_chunk(&self, method: &str, params: Value) -> Result<BridgeChunk> {
        self.call_chunk_for_target(method, params, BridgeTarget::Main)
    }

    pub(super) fn call_chunk_for_target(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
    ) -> Result<BridgeChunk> {
        let runtime_pin = self.runtime_pin_for_selector(target, None)?;
        let (id, start) = self.next_call()?;
        let call_context = BridgeCallContext {
            id,
            method,
            params: &params,
            target,
            player: None,
            runtime_pin: &runtime_pin,
            start,
            response_deadline: None,
        };

        self.call_pinned_socket(
            &call_context,
            false,
            Self::call_on_socket_chunk,
            "Bridge chunk call",
        )
    }

    fn next_call(&self) -> Result<(u64, usize)> {
        let total = self.channels.len();
        if total == 0 {
            bail!("No active bridge sockets");
        }
        Ok((
            self.next_id.fetch_add(1, Ordering::Relaxed),
            self.preferred_index.fetch_add(1, Ordering::Relaxed) % total,
        ))
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(super) fn studio_pid_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<u32> {
        let peer = self.peer_for_selector(target, player)?;
        let port = peer
            .rsplit(':')
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .with_context(|| format!("Could not parse peer port from '{peer}'"))?;
        pid_for_local_tcp_port(port)
            .with_context(|| format!("Could not map bridge connection {peer} to Studio"))
    }

    pub(super) fn call_on_socket_with_timeout(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
        response_timeout: Option<Duration>,
    ) -> Result<Value> {
        let response_timeout = response_timeout.unwrap_or_else(|| bridge_response_timeout(method));
        Self::configure_request_timeout(bridge_socket, response_timeout);
        Self::send_request(bridge_socket, id, method, params)?;

        match Self::read_response(bridge_socket, id, method, response_timeout)? {
            BridgeResponse::Json(result) => Ok(result),
            BridgeResponse::Chunk(_) => {
                bail!("Bridge method {method} returned a raw chunk response unexpectedly");
            }
        }
    }

    fn call_on_socket_chunk(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
        response_timeout: Option<Duration>,
    ) -> Result<BridgeChunk> {
        let response_timeout = response_timeout.unwrap_or_else(|| bridge_response_timeout(method));
        Self::configure_request_timeout(bridge_socket, response_timeout);
        Self::send_request(bridge_socket, id, method, params)?;

        match Self::read_response(bridge_socket, id, method, response_timeout)? {
            BridgeResponse::Chunk(chunk) => Ok(chunk),
            BridgeResponse::Json(result) => parse_bridge_chunk(result),
        }
    }

    pub(super) fn configure_request_timeout(bridge_socket: &mut BridgeSocket, timeout: Duration) {
        let _ = bridge_socket
            .socket
            .get_mut()
            .set_read_timeout(Some(timeout));
        let _ = bridge_socket
            .socket
            .get_mut()
            .set_write_timeout(Some(Duration::from_secs(10)));
    }

    pub(super) fn send_request(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct BridgeRequest<'a> {
            id: u64,
            session_id: &'a str,
            method: &'a str,
            params: &'a Value,
        }

        let payload = serde_json::to_string(&BridgeRequest {
            id,
            session_id: &bridge_socket.request_session_id,
            method,
            params,
        })?;
        if payload.len() > MAX_BRIDGE_REQUEST_BYTES {
            bail!(
                "Bridge request for {method} is {} bytes, above the {MAX_BRIDGE_REQUEST_BYTES}-byte safety limit",
                payload.len()
            );
        }

        bridge_socket
            .socket
            .send(Message::Text(payload.into()))
            .with_context(|| {
                format!(
                    "Bridge send failed for method {method} on port {}",
                    bridge_socket.port
                )
            })?;
        Ok(())
    }

    pub(super) fn read_response(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        timeout: Duration,
    ) -> Result<BridgeResponse> {
        let deadline = Instant::now() + timeout;
        let mut unrelated_messages = 0usize;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "Bridge response timed out awaiting {method} on port {} after {:.1}s",
                    bridge_socket.port,
                    timeout.as_secs_f64()
                );
            }
            let _ = bridge_socket
                .socket
                .get_mut()
                .set_read_timeout(Some(remaining));
            let message = bridge_socket.socket.read().with_context(|| {
                format!(
                    "Bridge read failed for method {method} on port {}",
                    bridge_socket.port
                )
            })?;
            match message {
                Message::Text(text) => {
                    if text.starts_with("RBS2 ") {
                        if let Some((raw_id, chunk)) = parse_bridge_raw_chunk(text.to_string())? {
                            if raw_id != id {
                                unrelated_messages = unrelated_messages.saturating_add(1);
                                if unrelated_messages > MAX_BRIDGE_UNRELATED_MESSAGES {
                                    bail!(
                                        "Bridge sent too many unrelated responses while waiting for {method}"
                                    );
                                }
                                continue;
                            }
                            return Ok(BridgeResponse::Chunk(chunk));
                        }
                        continue;
                    }
                    let mut parsed: Value =
                        serde_json::from_str(text.as_str()).with_context(|| {
                            format!("Invalid bridge JSON message ({} bytes)", text.len())
                        })?;
                    if Self::capture_final_console_message(bridge_socket, &parsed) {
                        continue;
                    }
                    let msg_id = parsed.get("id").and_then(Value::as_u64);
                    if msg_id != Some(id) {
                        unrelated_messages = unrelated_messages.saturating_add(1);
                        if unrelated_messages > MAX_BRIDGE_UNRELATED_MESSAGES {
                            bail!(
                                "Bridge sent too many unrelated responses while waiting for {method}"
                            );
                        }
                        continue;
                    }
                    let ok = parsed.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    if ok {
                        let result = parsed
                            .as_object_mut()
                            .and_then(|object| object.remove("result"))
                            .unwrap_or(Value::Null);
                        return Ok(BridgeResponse::Json(result));
                    }
                    let err = parsed
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("bridge error");
                    return Err(anyhow::Error::new(BridgeApplicationError {
                        method: method.to_string(),
                        message: err.to_string(),
                    }));
                }
                Message::Ping(payload) => {
                    let _ = bridge_socket.socket.send(Message::Pong(payload));
                }
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(frame) => {
                    bail!("Bridge closed while waiting for {method}: {frame:?}");
                }
            }
        }
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        for channel in &self.channels {
            let mut guard = channel
                .sockets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (_, mut socket) in guard.drain() {
                BridgeServer::close_socket(&mut socket);
            }
        }
    }
}

pub(super) fn parse_bridge_raw_chunk(mut text: String) -> Result<Option<(u64, BridgeChunk)>> {
    if !text.starts_with("RBS2 ") {
        return Ok(None);
    }

    let payload_index = text
        .find('\n')
        .map(|index| index + 1)
        .with_context(|| "Invalid raw bridge frame: missing payload separator")?;
    let (id, start, next_start, total, plugin_server_ms, plugin_encode_ms, serialization_complete) = {
        let header = &text["RBS2 ".len()..payload_index - 1];
        let mut parts = header.split_whitespace();
        let id = parts
            .next()
            .with_context(|| "Invalid raw bridge frame: missing header fields")?
            .parse::<u64>()
            .context("Invalid raw bridge frame id")?;
        let start = parts
            .next()
            .with_context(|| "Invalid raw bridge frame: missing header fields")?
            .parse::<usize>()
            .context("Invalid raw bridge frame start")?;
        let next_start = parts
            .next()
            .with_context(|| "Invalid raw bridge frame: missing header fields")?
            .parse::<usize>()
            .context("Invalid raw bridge frame next start")?;
        let total = parts
            .next()
            .with_context(|| "Invalid raw bridge frame: missing header fields")?
            .parse::<usize>()
            .context("Invalid raw bridge frame total")?;
        let plugin_server_ms = parts.next().and_then(|value| value.parse::<f64>().ok());
        let plugin_encode_ms = parts.next().and_then(|value| value.parse::<f64>().ok());
        let serialization_complete = match parts.next() {
            Some("0") => false,
            Some("1") => true,
            Some(_) => bail!("Invalid raw bridge frame serialization state"),
            None => bail!("Invalid raw bridge frame: missing serialization state"),
        };
        if parts.next().is_some() {
            bail!("Invalid raw bridge frame: too many header fields");
        }
        (
            id,
            start,
            next_start,
            total,
            plugin_server_ms,
            plugin_encode_ms,
            serialization_complete,
        )
    };
    let payload = text.split_off(payload_index);
    let chunk = BridgeChunk {
        start,
        next_start: if total == 0 { start } else { next_start },
        total,
        chunk: payload,
        plugin_server_ms,
        plugin_encode_ms,
        serialization_complete,
    };
    validate_bridge_chunk(&chunk)?;
    Ok(Some((id, chunk)))
}
