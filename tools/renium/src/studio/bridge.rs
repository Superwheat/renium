use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket, accept_with_config};

use crate::app::timing::elapsed_ms;
#[cfg(any(windows, target_os = "macos"))]
use crate::app::update;
#[cfg(any(windows, target_os = "macos"))]
use crate::daemon::transport::local_tcp_ports_owned_by_pid;
use crate::daemon::transport::normalize_loopback_host;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use crate::daemon::transport::pid_for_local_tcp_port;
use crate::snapshot::export::{parse_bridge_chunk, validate_bridge_chunk, validate_bridge_info};
use crate::studio::automation::TestLaunch;
use crate::studio::target::{place_filter, place_matches};
use crate::system::net::SharedTcpStream;

#[cfg(any(windows, target_os = "macos"))]
use crate::studio::input as input_inject;

pub(crate) const DEFAULT_EXPORT_CHUNK_SIZE: usize = 4 * 1024 * 1024;
pub(crate) const MAX_BRIDGE_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_BRIDGE_REASSEMBLY_BYTES: usize = 512 * 1024 * 1024;
pub(crate) const BRIDGE_DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const BRIDGE_ROLE_EDIT: &str = "edit";
pub(crate) const BRIDGE_ROLE_PLAY_SERVER: &str = "play-server";
pub(crate) const BRIDGE_ROLE_PLAY_CLIENT: &str = "play-client";
const BRIDGE_ROLE_UNKNOWN: &str = "unknown";
const BRIDGE_DUPLICATE_ROLE_KEY_SEPARATOR: char = '#';
const MIN_BRIDGE_CHUNK_BYTES: usize = 256;
pub(crate) const MAX_BRIDGE_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_BRIDGE_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_BRIDGE_UNRELATED_MESSAGES: usize = 64;
const BRIDGE_CHANNEL_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const BRIDGE_SLOW_RESPONSE_TIMEOUT: Duration = Duration::from_secs(90);
const BRIDGE_QUICK_SOCKET_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

// Manual Studio Play has no Renium launch token. Resolve its owner from the
// process first, or from a unique published place; never guess between windows.
fn play_owner_runtime(
    info: &BridgeInfoPayload,
    studio_pid: Option<u32>,
    edits: impl Iterator<Item = BridgeSocketSnapshot>,
) -> Option<String> {
    if !info.launch_edit_runtime_id.is_empty() {
        return Some(info.launch_edit_runtime_id.clone());
    }
    if !matches!(
        normalize_bridge_role(&info.bridge_role),
        BRIDGE_ROLE_PLAY_CLIENT | BRIDGE_ROLE_PLAY_SERVER
    ) {
        return None;
    }
    let mut process_owners = HashSet::new();
    let mut place_owners = HashSet::new();
    for edit in edits {
        if BridgeServer::bridge_role_key_base(&edit.role_key) != BRIDGE_ROLE_EDIT
            || edit.bridge_info.runtime_id.is_empty()
        {
            continue;
        }
        let same_place = info.place_id.is_some_and(|id| id > 0)
            && info.place_id == edit.bridge_info.place_id
            && info.game_id == edit.bridge_info.game_id;
        let same_process = studio_pid.is_some() && studio_pid == edit.studio_pid;
        if same_process && (same_place || info.place_id.is_none_or(|id| id <= 0)) {
            process_owners.insert(edit.bridge_info.runtime_id.clone());
        }
        if same_place {
            place_owners.insert(edit.bridge_info.runtime_id);
        }
    }
    let owners = if process_owners.is_empty() {
        place_owners
    } else {
        process_owners
    };
    (owners.len() == 1).then(|| owners.into_iter().next().unwrap())
}

struct HandshakePermit(Arc<AtomicUsize>);

impl HandshakePermit {
    fn acquire(pending: &Arc<AtomicUsize>) -> Option<Self> {
        pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 8).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(pending)))
    }
}

impl Drop for HandshakePermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
pub(crate) fn clamp_bridge_chunk_size(size: usize) -> usize {
    size.clamp(MIN_BRIDGE_CHUNK_BYTES, MAX_BRIDGE_CHUNK_BYTES)
}

#[derive(Debug)]
pub(crate) struct BridgeRequestTooLarge {
    method: String,
    bytes: usize,
}

impl std::fmt::Display for BridgeRequestTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Bridge request for {} is {} bytes, above the {MAX_BRIDGE_REQUEST_BYTES}-byte safety limit",
            self.method, self.bytes
        )
    }
}

impl std::error::Error for BridgeRequestTooLarge {}

#[derive(Debug)]
struct BridgeResponseTimeout(String);

impl std::fmt::Display for BridgeResponseTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BridgeResponseTimeout {}

fn is_status_observation(method: &str, params: &Value) -> bool {
    method == "getStudioState"
        || method == "startStopPlay" && params.as_object().is_some_and(serde_json::Map::is_empty)
}

fn default_method_target(method: &str) -> BridgeTarget {
    // Native exports describe saved Edit state, never a running simulation.
    // Main intentionally prefers a play server for runtime commands; using it
    // here can even replace an existing Edit pin when Play starts.
    match method {
        "beginEditorBinaryExport"
        | "awaitEditorBinaryExport"
        | "readEditorBinaryExport"
        | "readEditorBinaryExportBatch"
        | "getEditorBinaryOverlayChunk"
        | "finishEditorBinaryExport" => BridgeTarget::Edit,
        _ => BridgeTarget::Main,
    }
}

fn bridge_response_timeout(method: &str) -> Duration {
    match method {
        "prepare"
        | "applyEditorChanges"
        | "beginEditorTransaction"
        | "beginEditorTransactionUpload"
        | "appendEditorTransactionUpload"
        | "finishEditorTransactionUpload"
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

fn bridge_socket_attempt_timeout(method: &str, params: &Value, remaining: Duration) -> Duration {
    let retry_quickly = matches!(method, "getStudioState" | "startStopPlay")
        || (method == "deviceSimulator"
            && matches!(
                params.get("action").and_then(Value::as_str),
                Some("capture-status" | "status")
            ));
    if retry_quickly {
        remaining.min(BRIDGE_QUICK_SOCKET_ATTEMPT_TIMEOUT)
    } else {
        remaining
    }
}

fn bridge_channel_lock_timeout(method: &str) -> Duration {
    match method {
        "appendEditorBinaryImport" | "appendEditorPushReview" | "appendEditorTransactionUpload" => {
            BRIDGE_SLOW_RESPONSE_TIMEOUT
        }
        _ => BRIDGE_CHANNEL_LOCK_TIMEOUT,
    }
}

fn bridge_request_allowed_after_cancel(method: &str, params: &Value) -> bool {
    (method == "startStopPlay" && params.get("start").and_then(Value::as_bool) != Some(true))
        || matches!(
            method,
            "cancelRequestLease"
                | "getEditorTransactionState"
                | "rollbackEditorTransaction"
                | "cancelEditorTransactionUpload"
                | "cancelEditorBinaryImport"
                | "cancelEditorReconcile"
                | "cancelEditorPushReview"
                | "finishEditorBinaryExport"
        )
}

#[cfg(test)]
#[path = "bridge_routing_tests.rs"]
mod routing_tests;

#[cfg(test)]
mod request_cancellation_tests {
    use super::*;

    #[test]
    fn manual_play_ownership_uses_process_or_unique_place_not_names() {
        let edit = |runtime: &str, place: i64, pid: u32| BridgeSocketSnapshot {
            port: 8781,
            peer: String::new(),
            studio_pid: Some(pid),
            role_key: BRIDGE_ROLE_EDIT.into(),
            bridge_info: BridgeInfoPayload {
                runtime_id: runtime.into(),
                bridge_role: BRIDGE_ROLE_EDIT.into(),
                game_id: Some(10),
                place_id: Some(place),
                place_name: "Place1".into(),
                ..BridgeInfoPayload::default()
            },
        };
        let mut client = BridgeInfoPayload {
            runtime_id: "client".into(),
            bridge_role: BRIDGE_ROLE_PLAY_CLIENT.into(),
            game_id: Some(10),
            place_id: Some(20),
            place_name: "DTE @ current test".into(),
            player_name: "The_SirMeme".into(),
            ..BridgeInfoPayload::default()
        };
        let mut edits = vec![edit("dte", 20, 100), edit("baseplate", 30, 200)];
        // Duplicate bridge channels are one owner, not two Studio windows.
        edits.push(edits[0].clone());
        for role in [BRIDGE_ROLE_PLAY_CLIENT, BRIDGE_ROLE_PLAY_SERVER] {
            client.bridge_role = role.into();
            assert_eq!(
                play_owner_runtime(&client, Some(100), edits.clone().into_iter()).as_deref(),
                Some("dte")
            );
            assert_eq!(
                play_owner_runtime(&client, Some(300), edits.clone().into_iter()).as_deref(),
                Some("dte")
            );
        }
        edits.push(edit("second-dte", 20, 400));
        assert_eq!(
            play_owner_runtime(&client, Some(100), edits.clone().into_iter()).as_deref(),
            Some("dte")
        );
        assert_eq!(
            play_owner_runtime(&client, None, edits.clone().into_iter()),
            None
        );
        client.launch_edit_runtime_id = "second-dte".into();
        assert_eq!(
            play_owner_runtime(&client, Some(100), edits.clone().into_iter()).as_deref(),
            Some("second-dte")
        );
        client.launch_edit_runtime_id.clear();
        client.place_id = Some(0);
        assert_eq!(
            play_owner_runtime(&client, Some(100), edits.clone().into_iter()).as_deref(),
            Some("dte")
        );
        assert_eq!(
            play_owner_runtime(&client, None, edits.clone().into_iter()),
            None
        );
        client.place_id = Some(99);
        assert_eq!(
            play_owner_runtime(&client, Some(100), edits.into_iter()),
            None
        );
    }

    #[test]
    fn stalled_handshake_does_not_block_registration_and_ack_follows_inventory() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let channel = Arc::new(BridgeChannel {
            port: address.port(),
            sockets: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
        });
        let alive = Arc::new(AtomicBool::new(true));
        BridgeServer::spawn_accept_loop(
            "127.0.0.1".into(),
            address.port(),
            listener,
            Arc::clone(&channel),
            BridgeAcceptState {
                alive: Arc::clone(&alive),
                request_session_id: "handshake-test".into(),
                next_id: Default::default(),
                routing: Default::default(),
                desired_device_request: Default::default(),
                device_reconciled_runtimes: Default::default(),
                all_channels: Arc::new(Mutex::new(vec![Arc::clone(&channel)])),
                expected_channels: 1,
                reconcile_device_on_connect: false,
                performance_manager: None,
                #[cfg(any(windows, target_os = "macos"))]
                update_checked_runtimes: Default::default(),
                #[cfg(any(windows, target_os = "macos"))]
                check_updates_on_connect: false,
            },
        );
        let stalled = TcpStream::connect(address).unwrap();
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (mut ready, _) = tungstenite::client(format!("ws://{address}"), stream).unwrap();
        let request: Value =
            serde_json::from_str(&ready.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(request["params"]["registrationAck"], true);
        ready.send(Message::Text(json!({
            "id": request["id"], "ok": true, "result": {
                "runtimeId": "handshake-test", "bridgeRole": "play-server", "registrationAck": true,
                "protocolVersion": "compact-v5", "codecVersion": "compact-v5-schema-9",
                "chunkFrameProtocolVersion": "rbs2", "compactValueProtocolVersion": "compact-v5-schema-4",
            },
        }).to_string().into())).unwrap();
        let ack: Value = serde_json::from_str(&ready.read().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(ack["method"], "bridgeRegistered");
        assert_eq!(ack["session_id"], "handshake-test");
        assert_eq!(
            channel.snapshots.lock().unwrap().len(),
            1,
            "ack must follow registration"
        );
        alive.store(false, Ordering::Relaxed);
        drop(stalled);
        drop(ready);

        let pending = Arc::new(AtomicUsize::new(0));
        let mut permits: Vec<_> = (0..8)
            .map(|_| HandshakePermit::acquire(&pending).unwrap())
            .collect();
        assert!(HandshakePermit::acquire(&pending).is_none());
        permits.pop();
        assert!(HandshakePermit::acquire(&pending).is_some());
    }

    #[test]
    fn slow_status_keeps_connection_and_ignores_its_late_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (resume, delayed) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut socket =
                WebSocket::from_raw_socket(peer, tungstenite::protocol::Role::Server, None);
            for delayed_reply in [true, false] {
                let message = socket.read().unwrap().into_text().unwrap();
                let request: Value = serde_json::from_str(&message).unwrap();
                if delayed_reply {
                    delayed.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                socket
                    .send(Message::Text(
                        json!({
                            "id": request["id"], "ok": true, "result": {"late": delayed_reply}
                        })
                        .to_string()
                        .into(),
                    ))
                    .unwrap();
            }
        });
        let socket = BridgeSocket {
            port: address.port(),
            peer: address.to_string(),
            studio_pid: None,
            role: BRIDGE_ROLE_EDIT.to_string(),
            last_focused_at: Instant::now(),
            bridge_info: BridgeInfoPayload {
                runtime_id: "test-runtime".to_string(),
                ..Default::default()
            },
            request_session_id: "test".to_string(),
            pending_final_console_snapshots: Vec::new(),
            pending_player_identity: None,
            active_request_lease: None,
            cancel_request_id: None,
            socket: WebSocket::from_raw_socket(
                client.into(),
                tungstenite::protocol::Role::Client,
                None,
            ),
        };
        let channel = Arc::new(BridgeChannel {
            port: address.port(),
            sockets: Mutex::new(HashMap::from([(
                BRIDGE_ROLE_EDIT.to_string(),
                BridgeConnection::new(socket),
            )])),
            snapshots: Mutex::new(HashMap::new()),
        });
        let bridge = BridgeServer {
            channels: vec![Arc::clone(&channel)],
            alive: Arc::new(AtomicBool::new(true)),
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            preferred_index: Default::default(),
            request_gate: Mutex::new(()),
            active_request_leases: Mutex::new(HashMap::new()),
            runtime_pins: Mutex::new(HashMap::new()),
            final_console_snapshots: Mutex::new(HashMap::new()),
            desired_device_request: Arc::new(Mutex::new(Value::Null)),
            routing: Default::default(),
            performance_manager: None,
        };
        let result = bridge.call_for_runtime_with_timeout(
            "getStudioState",
            json!({}),
            BridgeTarget::Edit,
            "test-runtime",
            Some(Duration::from_millis(10)),
        );
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<BridgeResponseTimeout>()
                .is_some()
        );
        assert_eq!(channel.sockets.lock().unwrap().len(), 1);
        resume.send(()).unwrap();
        let result = bridge
            .call_for_runtime_with_timeout(
                "getStudioState",
                json!({}),
                BridgeTarget::Edit,
                "test-runtime",
                Some(Duration::from_secs(1)),
            )
            .unwrap();
        assert_eq!(result, json!({"late": false}));
        assert_eq!(channel.sockets.lock().unwrap().len(), 1);
        worker.join().unwrap();
        assert!(is_status_observation("startStopPlay", &json!({})));
        assert!(!is_status_observation(
            "startStopPlay",
            &json!({"start": true})
        ));
        assert!(!is_status_observation(
            "startStopPlay",
            &json!({"stop": true})
        ));
        assert!(!is_status_observation("applyEditorChanges", &json!({})));
    }

    #[test]
    fn play_status_and_stop_survive_bridge_reload_but_start_does_not() {
        assert!(bridge_request_allowed_after_cancel(
            "startStopPlay",
            &json!({})
        ));
        assert!(bridge_request_allowed_after_cancel(
            "startStopPlay",
            &json!({ "stop": true })
        ));
        assert!(!bridge_request_allowed_after_cancel(
            "startStopPlay",
            &json!({ "start": true })
        ));
    }
}

fn bridge_request_lease_id<'a>(
    method: &str,
    lease: Option<&'a BridgeRequestLease>,
) -> Option<&'a str> {
    lease.and_then(|lease| {
        (!lease.is_cancelled() || method != "getEditorTransactionState").then(|| lease.id())
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeChunk {
    pub(crate) start: usize,
    pub(crate) next_start: usize,
    pub(crate) total: usize,
    pub(crate) chunk: String,
    pub(crate) plugin_server_ms: Option<f64>,
    pub(crate) plugin_encode_ms: Option<f64>,
    #[serde(default)]
    pub(crate) serialization_complete: bool,
    #[serde(default)]
    pub(crate) payload_hash: Option<String>,
    #[serde(default)]
    pub(crate) payload_cache_hit: bool,
    #[serde(default)]
    pub(crate) compression: Option<String>,
    #[serde(default)]
    pub(crate) uncompressed_bytes: Option<usize>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ChunkFetchMetrics {
    pub(crate) bytes: usize,
    pub(crate) chunks: usize,
    pub(crate) max_chunk_bytes: usize,
    pub(crate) plugin_server_ms: f64,
    pub(crate) plugin_encode_ms: f64,
    pub(crate) reassembly_ms: f64,
    pub(crate) json_parse_ms: f64,
}

pub(crate) enum BridgeResponse {
    Json(Value),
    Chunk(BridgeChunk),
}

#[derive(Debug)]
pub(crate) struct BridgeApplicationError {
    pub(crate) method: String,
    pub(crate) message: String,
}

impl std::fmt::Display for BridgeApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let first_line = self.message.lines().next().unwrap_or(&self.message);
        let message = first_line
            .split_once(": ")
            .filter(|(prefix, _)| {
                prefix
                    .rsplit_once(':')
                    .is_some_and(|(_, line)| line.parse::<u32>().is_ok())
            })
            .map_or(first_line, |(_, message)| message);
        write!(
            formatter,
            "Bridge method {} failed: {}",
            self.method, message
        )
    }
}

impl std::error::Error for BridgeApplicationError {}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct BridgeInfoPayload {
    pub(crate) runtime_id: String,
    pub(crate) launch_nonce: String,
    pub(crate) launch_edit_runtime_id: String,
    pub(crate) bridge_version: String,
    pub(crate) bridge_build_unix: i64,
    pub(crate) bridge_role: String,
    pub(crate) player_name: String,
    pub(crate) player_user_id: Option<i64>,
    pub(crate) place_id: Option<i64>,
    pub(crate) game_id: Option<i64>,
    pub(crate) place_name: String,
    pub(crate) protocol_version: String,
    pub(crate) codec_version: String,
    pub(crate) chunk_frame_protocol_version: String,
    pub(crate) compact_value_protocol_version: String,
    pub(crate) performance_mode: String,
    pub(crate) export_all_properties: bool,
    pub(crate) modified_default_bypass: bool,
    pub(crate) registration_ack: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgePerformanceStats {
    pub(crate) frame_ms: Option<f64>,
    pub(crate) last_frame_ms: Option<f64>,
    pub(crate) max_frame_ms: Option<f64>,
    pub(crate) stall_count_over_33_ms: Option<u64>,
    pub(crate) stall_count_over_50_ms: Option<u64>,
    pub(crate) stall_count_over_100_ms: Option<u64>,
    pub(crate) modified_default_checks: Option<u64>,
    pub(crate) modified_default_elided: Option<u64>,
    pub(crate) modified_default_validation_reads: Option<u64>,
    pub(crate) modified_default_runtime_denylist_count: Option<u64>,
    pub(crate) properties_read: Option<u64>,
    pub(crate) properties_encoded: Option<u64>,
    pub(crate) properties_default_skipped: Option<u64>,
    pub(crate) safe_read_class_fallback_count: Option<u64>,
    pub(crate) safe_read_property_fallback_count: Option<u64>,
}

#[derive(Default)]
pub(crate) struct SourceBatchMap {
    pub(crate) by_index: HashMap<usize, String>,
    pub(crate) by_key: HashMap<String, String>,
}

pub(crate) struct BridgeSocket {
    pub(crate) port: u16,
    pub(crate) peer: String,
    pub(crate) studio_pid: Option<u32>,
    pub(crate) role: String,
    pub(crate) last_focused_at: Instant,
    pub(crate) bridge_info: BridgeInfoPayload,
    pub(crate) request_session_id: String,
    pub(crate) pending_final_console_snapshots: Vec<Value>,
    pending_player_identity: Option<(u64, Instant)>,
    active_request_lease: Option<Arc<BridgeRequestLease>>,
    cancel_request_id: Option<u64>,
    pub(crate) socket: WebSocket<SharedTcpStream>,
}

// Routing metadata stays available while this connection is executing a request.
// Never hold the channel registry while waiting for this connection's I/O lock.
#[derive(Clone)]
pub(crate) struct BridgeConnection {
    port: u16,
    peer: String,
    studio_pid: Option<u32>,
    role: String,
    last_focused_at: Instant,
    bridge_info: BridgeInfoPayload,
    io: Arc<Mutex<BridgeSocket>>,
    shutdown: SharedTcpStream,
}

impl BridgeConnection {
    fn new(socket: BridgeSocket) -> Self {
        Self {
            port: socket.port,
            peer: socket.peer.clone(),
            studio_pid: socket.studio_pid,
            role: socket.role.clone(),
            last_focused_at: socket.last_focused_at,
            bridge_info: socket.bridge_info.clone(),
            shutdown: socket.socket.get_ref().clone(),
            io: Arc::new(Mutex::new(socket)),
        }
    }

    fn close(&self) {
        // Also interrupts an outstanding read on a replaced/retired connection.
        let _ = self.shutdown.shutdown(Shutdown::Both);
    }

    fn is_alive(&self) -> bool {
        match self.io.try_lock() {
            Ok(mut socket) => BridgeServer::socket_is_alive(&mut socket),
            Err(TryLockError::Poisoned(poisoned)) => {
                BridgeServer::socket_is_alive(&mut poisoned.into_inner())
            }
            Err(TryLockError::WouldBlock) => true,
        }
    }
}

#[derive(Clone)]
struct BridgeSocketSnapshot {
    port: u16,
    peer: String,
    studio_pid: Option<u32>,
    role_key: String,
    bridge_info: BridgeInfoPayload,
}

#[derive(Default)]
struct RuntimeRouting {
    retired: HashSet<String>,
    // Only the owning edit runtime establishes the launch, never a late client.
    launches: HashMap<String, (u64, String)>,
}

impl RuntimeRouting {
    fn allows(&self, info: &BridgeInfoPayload) -> bool {
        self.allows_identity(
            &info.runtime_id,
            &info.bridge_role,
            &info.launch_edit_runtime_id,
            &info.launch_nonce,
        )
    }

    fn allows_identity(&self, runtime: &str, role: &str, owner: &str, nonce: &str) -> bool {
        if self.retired.contains(runtime) {
            return false;
        }
        if nonce.is_empty() || role == BRIDGE_ROLE_EDIT {
            return true;
        }
        self.launches
            .get(owner)
            .is_none_or(|(_, current)| current == nonce)
    }

    fn observe_launch(&mut self, edit: &str, sequence: u64, nonce: &str) {
        if !nonce.is_empty()
            && self
                .launches
                .get(edit)
                .is_none_or(|(previous, _)| sequence > *previous)
        {
            self.launches.insert(edit.into(), (sequence, nonce.into()));
        }
    }
}

pub(crate) struct BridgeChannel {
    pub(crate) port: u16,
    pub(crate) sockets: Mutex<HashMap<String, BridgeConnection>>,
    snapshots: Mutex<HashMap<String, BridgeSocketSnapshot>>,
}

#[derive(Clone)]
struct BridgeAcceptState {
    alive: Arc<AtomicBool>,
    request_session_id: String,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    routing: Arc<Mutex<RuntimeRouting>>,
    desired_device_request: Arc<Mutex<Value>>,
    device_reconciled_runtimes: Arc<Mutex<HashSet<String>>>,
    all_channels: Arc<Mutex<Vec<Arc<BridgeChannel>>>>,
    expected_channels: usize,
    reconcile_device_on_connect: bool,
    performance_manager: Option<Arc<crate::studio::performance::Manager>>,
    #[cfg(any(windows, target_os = "macos"))]
    update_checked_runtimes: Arc<Mutex<HashSet<String>>>,
    #[cfg(any(windows, target_os = "macos"))]
    check_updates_on_connect: bool,
}

pub(crate) struct BridgeServer {
    pub(crate) channels: Vec<Arc<BridgeChannel>>,
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) next_id: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) preferred_index: std::sync::atomic::AtomicUsize,

    pub(crate) request_gate: Mutex<()>,
    active_request_leases: Mutex<HashMap<thread::ThreadId, Arc<BridgeRequestLease>>>,

    pub(crate) runtime_pins: Mutex<HashMap<RuntimePinKey, RuntimePin>>,
    routing: Arc<Mutex<RuntimeRouting>>,
    pub(crate) final_console_snapshots: Mutex<HashMap<String, FinalConsoleSnapshot>>,
    desired_device_request: Arc<Mutex<Value>>,
    performance_manager: Option<Arc<crate::studio::performance::Manager>>,
}

pub(crate) struct BridgeRequestLease {
    id: String,
    state: AtomicU8,
}

const REQUEST_LEASE_PENDING: u8 = 0;
const REQUEST_LEASE_ARMED: u8 = 1;
const REQUEST_LEASE_CANCELLED: u8 = 2;
const REQUEST_LEASE_FINISHED: u8 = 3;
const REQUEST_LEASE_CANCELLED_FINISHED: u8 = 4;

impl BridgeRequestLease {
    pub(crate) fn new(id: String) -> Self {
        Self {
            id,
            state: AtomicU8::new(REQUEST_LEASE_PENDING),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn cancel(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let next = match state {
                REQUEST_LEASE_PENDING | REQUEST_LEASE_ARMED => REQUEST_LEASE_CANCELLED,
                _ => return false,
            };
            if self
                .state
                .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return state == REQUEST_LEASE_ARMED;
            }
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            REQUEST_LEASE_CANCELLED | REQUEST_LEASE_CANCELLED_FINISHED
        )
    }

    fn arm(&self) -> Result<()> {
        self.state
            .compare_exchange(
                REQUEST_LEASE_PENDING,
                REQUEST_LEASE_ARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|state| {
                if matches!(
                    state,
                    REQUEST_LEASE_CANCELLED | REQUEST_LEASE_CANCELLED_FINISHED
                ) {
                    anyhow::anyhow!("Renium request was cancelled because its client disconnected")
                } else {
                    anyhow::anyhow!("Renium request lease is not available")
                }
            })
    }

    fn disarm(&self) {
        let _ = self.state.compare_exchange(
            REQUEST_LEASE_ARMED,
            REQUEST_LEASE_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn finish(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let next = match state {
                REQUEST_LEASE_CANCELLED => REQUEST_LEASE_CANCELLED_FINISHED,
                REQUEST_LEASE_CANCELLED_FINISHED | REQUEST_LEASE_FINISHED => return,
                _ => REQUEST_LEASE_FINISHED,
            };
            if self
                .state
                .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    pub(crate) fn ensure_active(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("Renium request was cancelled because its client disconnected");
        }
        Ok(())
    }
}

pub(crate) struct BridgeRequestLeaseGuard<'a> {
    bridge: &'a BridgeServer,
    lease: Arc<BridgeRequestLease>,
    thread_id: thread::ThreadId,
}

impl Drop for BridgeRequestLeaseGuard<'_> {
    fn drop(&mut self) {
        let mut active = self
            .bridge
            .active_request_leases
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if active
            .get(&self.thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.lease))
        {
            active.remove(&self.thread_id);
        }
        self.lease.disarm();
    }
}

pub(crate) struct FinalConsoleSnapshot {
    pub(crate) received_at: Instant,
    pub(crate) payload: Value,
}

pub(crate) struct BridgeListenMetrics {
    pub(crate) bind_ms: f64,
    pub(crate) wait_for_channels_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BridgeTarget {
    Edit,
    Main,
    Client,
}

impl BridgeTarget {
    pub(crate) const fn main_or_client(client: bool) -> Self {
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
pub(crate) struct RuntimePinKey {
    pub(crate) target: BridgeTarget,
    pub(crate) player: Option<String>,
}
#[derive(Clone)]
pub(crate) struct RuntimePin {
    pub(crate) runtime_id: String,
    // An operation's explicit binding is not a role-preference cache entry.
    exact: bool,
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
    lease_id: Option<&'a str>,
    request_lease: Option<Arc<BridgeRequestLease>>,
    cancel_request_id: Option<u64>,
}

type BridgeSocketCall<T> =
    fn(&mut BridgeSocket, u64, &str, &Value, Option<Duration>, Option<&str>) -> Result<T>;

pub(crate) struct RuntimePinCandidate {
    pub(crate) ports: HashSet<u16>,
    pub(crate) role_rank: usize,
    pub(crate) last_focused_at: Instant,
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
    pub(crate) fn acquire_request_gate(&self) -> MutexGuard<'_, ()> {
        self.request_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn acquire_request_gate_for_lease(
        &self,
        lease: &BridgeRequestLease,
    ) -> Result<MutexGuard<'_, ()>> {
        loop {
            lease.ensure_active()?;
            match self.request_gate.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    pub(crate) fn activate_request_lease(
        &self,
        lease: Arc<BridgeRequestLease>,
    ) -> Result<BridgeRequestLeaseGuard<'_>> {
        lease.arm()?;
        let thread_id = thread::current().id();
        let mut active = self
            .active_request_leases
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if active.contains_key(&thread_id) {
            lease.disarm();
            bail!("Another Renium request lease is already active");
        }
        active.insert(thread_id, Arc::clone(&lease));
        Ok(BridgeRequestLeaseGuard {
            bridge: self,
            lease,
            thread_id,
        })
    }

    fn active_request_lease(&self) -> Option<Arc<BridgeRequestLease>> {
        self.active_request_leases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&thread::current().id())
            .cloned()
    }

    pub(crate) fn listen(
        host: &str,
        ports: &[u16],
        wait_seconds: f64,
    ) -> Result<(Self, BridgeListenMetrics)> {
        Self::listen_with_initial_wait(host, ports, wait_seconds, true)
    }

    pub(crate) fn listen_with_initial_wait(
        host: &str,
        ports: &[u16],
        wait_seconds: f64,
        wait_for_initial_channels: bool,
    ) -> Result<(Self, BridgeListenMetrics)> {
        Self::listen_configured(host, ports, wait_seconds, wait_for_initial_channels, false)
    }

    pub(crate) fn listen_daemon(
        host: &str,
        ports: &[u16],
        wait_seconds: f64,
    ) -> Result<(Self, BridgeListenMetrics)> {
        Self::listen_configured(host, ports, wait_seconds, false, true)
    }

    fn listen_configured(
        host: &str,
        ports: &[u16],
        wait_seconds: f64,
        wait_for_initial_channels: bool,
        check_updates_on_connect: bool,
    ) -> Result<(Self, BridgeListenMetrics)> {
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = check_updates_on_connect;
        let bind_host = normalize_loopback_host(host)?;
        let bind_started = Instant::now();
        let alive = Arc::new(AtomicBool::new(true));
        let next_id = Arc::new(std::sync::atomic::AtomicU64::new(21335));
        let desired_device_request = Arc::new(Mutex::new(json!({ "action": "stop" })));
        let device_reconciled_runtimes = Arc::new(Mutex::new(HashSet::new()));
        let performance_manager =
            check_updates_on_connect.then(crate::studio::performance::Manager::load);
        let all_channels = Arc::new(Mutex::new(Vec::with_capacity(ports.len())));
        #[cfg(any(windows, target_os = "macos"))]
        let update_checked_runtimes = Arc::new(Mutex::new(HashSet::new()));
        let mut channels: Vec<Arc<BridgeChannel>> = Vec::with_capacity(ports.len());
        let request_session_id = format!(
            "{:x}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        );
        let accept_state = BridgeAcceptState {
            alive: Arc::clone(&alive),
            request_session_id,
            next_id: Arc::clone(&next_id),
            routing: Arc::new(Mutex::new(RuntimeRouting::default())),
            desired_device_request: Arc::clone(&desired_device_request),
            device_reconciled_runtimes: Arc::clone(&device_reconciled_runtimes),
            all_channels: Arc::clone(&all_channels),
            expected_channels: ports.len(),
            reconcile_device_on_connect: check_updates_on_connect,
            performance_manager: performance_manager.clone(),
            #[cfg(any(windows, target_os = "macos"))]
            update_checked_runtimes,
            #[cfg(any(windows, target_os = "macos"))]
            check_updates_on_connect,
        };

        for port in ports {
            let listener = TcpListener::bind((bind_host.as_str(), *port)).with_context(|| {
                format!(
                    "Failed to bind bridge server on {bind_host}:{port}; close the Renium process using that port or run `rbx daemon list` and `rbx daemon stop --all`"
                )
            })?;
            listener.set_nonblocking(true).with_context(|| {
                format!("Failed to set nonblocking listener {bind_host}:{port}")
            })?;
            crate::app::output::log_global(
                4,
                format_args!("[renium] bridge listening on {bind_host}:{port}"),
            );

            let channel = Arc::new(BridgeChannel {
                port: *port,
                sockets: Mutex::new(HashMap::new()),
                snapshots: Mutex::new(HashMap::new()),
            });
            all_channels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Arc::clone(&channel));
            Self::spawn_accept_loop(
                bind_host.clone(),
                *port,
                listener,
                Arc::clone(&channel),
                accept_state.clone(),
            );
            channels.push(channel);
        }

        #[cfg(any(windows, target_os = "macos"))]
        Self::spawn_focus_watcher(channels.clone(), Arc::clone(&alive));

        let bind_ms = elapsed_ms(bind_started);
        let wait_started = Instant::now();
        let server = Self {
            channels,
            alive,
            next_id,
            preferred_index: std::sync::atomic::AtomicUsize::new(0),
            request_gate: Mutex::new(()),
            active_request_leases: Mutex::new(HashMap::new()),
            runtime_pins: Mutex::new(HashMap::new()),
            routing: Arc::clone(&accept_state.routing),
            final_console_snapshots: Mutex::new(HashMap::new()),
            desired_device_request,
            performance_manager,
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
                    crate::app::output::log_global(
                        4,
                        format_args!(
                            "[renium] bridge ready channels: {ready_channels}/{required_channels}"
                        ),
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

            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bridge all channels ready: {ready_channels}/{required_channels}"
                ),
            );
        } else {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bridge serving on {required_channels} port(s); waiting for plugin clients on demand"
                ),
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

    fn spawn_accept_loop(
        bind_host: String,
        port: u16,
        listener: TcpListener,
        channel: Arc<BridgeChannel>,
        state: BridgeAcceptState,
    ) {
        thread::spawn(move || {
            let pending = Arc::new(AtomicUsize::new(0));
            while state.alive.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let Some(permit) = HandshakePermit::acquire(&pending) else {
                            continue;
                        };
                        let peer = addr.to_string();
                        let state = state.clone();
                        let bind_host = bind_host.clone();
                        let channel = Arc::clone(&channel);
                        thread::spawn(move || {
                            let _permit = permit;
                            let BridgeAcceptState {
                                alive,
                                request_session_id,
                                next_id,
                                routing,
                                desired_device_request,
                                device_reconciled_runtimes,
                                all_channels,
                                expected_channels,
                                reconcile_device_on_connect,
                                performance_manager,
                                #[cfg(any(windows, target_os = "macos"))]
                                update_checked_runtimes,
                                #[cfg(any(windows, target_os = "macos"))]
                                check_updates_on_connect,
                            } = state;
                            match Self::accept_ready_socket(
                                &bind_host,
                                port,
                                stream,
                                peer.clone(),
                                &request_session_id,
                            ) {
                                Ok(socket) => {
                                    if !alive.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    let device_runtime = (reconcile_device_on_connect
                                        && socket.role == BRIDGE_ROLE_EDIT)
                                        .then(|| socket.bridge_info.runtime_id.clone());
                                    let performance_peer = (socket.role == BRIDGE_ROLE_EDIT)
                                        .then(|| socket.peer.clone());
                                    #[cfg(target_os = "macos")]
                                    let auto_recovery_peer = (socket.role == BRIDGE_ROLE_EDIT)
                                        .then(|| socket.peer.clone());
                                    #[cfg(any(windows, target_os = "macos"))]
                                    let update_target = (check_updates_on_connect
                                        && socket.role == BRIDGE_ROLE_EDIT)
                                        .then(|| socket.bridge_info.runtime_id.clone());
                                    let mut guard = channel
                                        .sockets
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner);
                                    {
                                        let mut routing =
                                            routing.lock().unwrap_or_else(PoisonError::into_inner);
                                        if !routing.allows(&socket.bridge_info) {
                                            return;
                                        }
                                        if socket.role == BRIDGE_ROLE_EDIT {
                                            routing.observe_launch(
                                                &socket.bridge_info.runtime_id,
                                                0,
                                                &socket.bridge_info.launch_nonce,
                                            );
                                        }
                                    }
                                    let role = socket.role.clone();
                                    guard.retain(|_, previous| {
                                        let replaced = previous.bridge_info.runtime_id
                                            == socket.bridge_info.runtime_id
                                            && previous.role == role;
                                        if replaced {
                                            previous.close();
                                        }
                                        !replaced
                                    });
                                    let socket_key =
                                        Self::bridge_socket_key(&guard, &role, &socket.peer);
                                    if socket_key == role {
                                        crate::app::output::log_global(
                                            4,
                                            format_args!(
                                                "[renium] bridge channel ready on {}:{} role={} from {} build={}",
                                                bind_host,
                                                port,
                                                socket.role,
                                                socket.peer,
                                                socket.bridge_info.bridge_build_unix
                                            ),
                                        );
                                    }
                                    let connection = BridgeConnection::new(socket);
                                    // Register before acknowledging, but don't send under the registry lock.
                                    let mut socket = connection
                                        .io
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner);
                                    guard.insert(socket_key.clone(), connection.clone());
                                    Self::refresh_channel_snapshots(&channel, &guard);
                                    drop(guard);
                                    if socket.bridge_info.registration_ack {
                                        let ack = json!({
                                            "id": 0, "method": "bridgeRegistered", "params": {},
                                            "session_id": request_session_id,
                                        });
                                        if socket
                                            .socket
                                            .send(Message::Text(ack.to_string().into()))
                                            .is_err()
                                        {
                                            Self::remove_connection(
                                                &channel,
                                                &socket_key,
                                                &connection,
                                            );
                                            return;
                                        }
                                    }
                                    drop(socket);
                                    if let (Some(manager), Some(peer)) =
                                        (&performance_manager, performance_peer)
                                        && let Ok(pid) = Self::studio_pid_for_peer(&peer)
                                    {
                                        let manager = Arc::clone(manager);
                                        thread::spawn(move || {
                                            if let Err(error) = manager.enroll(pid) {
                                                eprintln!(
                                                    "[renium] performance profile could not enroll Studio {pid}: {error:#}"
                                                );
                                            }
                                        });
                                    }
                                    #[cfg(target_os = "macos")]
                                    if let Some(peer) = auto_recovery_peer
                                        && let Ok(pid) = Self::studio_pid_for_peer(&peer)
                                    {
                                        input_inject::watch_auto_recovery_dialog_for_pid(pid);
                                    }
                                    if let Some(runtime_id) = device_runtime {
                                        let channels = all_channels
                                            .lock()
                                            .unwrap_or_else(PoisonError::into_inner)
                                            .clone();
                                        let ready = channels
                                            .iter()
                                            .filter(|channel| {
                                                channel
                                                    .sockets
                                                    .lock()
                                                    .unwrap_or_else(PoisonError::into_inner)
                                                    .values()
                                                    .any(|socket| {
                                                        socket.role == BRIDGE_ROLE_EDIT
                                                            && socket.bridge_info.runtime_id
                                                                == runtime_id
                                                    })
                                            })
                                            .count();
                                        if ready >= expected_channels
                                            && device_reconciled_runtimes
                                                .lock()
                                                .unwrap_or_else(PoisonError::into_inner)
                                                .insert(runtime_id.clone())
                                        {
                                            Self::apply_desired_device_state(
                                                runtime_id,
                                                Arc::clone(&channel),
                                                Arc::clone(&next_id),
                                                Arc::clone(&desired_device_request),
                                                Arc::clone(&device_reconciled_runtimes),
                                            );
                                        }
                                    }
                                    #[cfg(any(windows, target_os = "macos"))]
                                    if let Some(runtime_id) = update_target {
                                        Self::check_for_update(
                                            runtime_id,
                                            Arc::clone(&channel),
                                            Arc::clone(&next_id),
                                            Arc::clone(&update_checked_runtimes),
                                        );
                                    }
                                }
                                Err(err) => {
                                    println!(
                                        "[renium] warning: bridge channel handshake failed on {bind_host}:{port} from {peer}: {err:#}"
                                    );
                                }
                            }
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(err) => {
                        if state.alive.load(Ordering::Relaxed) {
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

    fn apply_desired_device_state(
        runtime_id: String,
        channel: Arc<BridgeChannel>,
        next_id: Arc<std::sync::atomic::AtomicU64>,
        desired_device_request: Arc<Mutex<Value>>,
        device_reconciled_runtimes: Arc<Mutex<HashSet<String>>>,
    ) {
        thread::spawn(move || {
            let call_device = |request: &Value| -> Result<(Value, String)> {
                let sockets = channel
                    .sockets
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let socket = sockets
                    .values()
                    .find(|socket| {
                        socket.role == BRIDGE_ROLE_EDIT
                            && socket.bridge_info.runtime_id == runtime_id
                    })
                    .cloned()
                    .context("Studio disconnected before device state could be restored")?;
                drop(sockets);
                let mut socket = socket.io.lock().unwrap_or_else(PoisonError::into_inner);
                let peer = socket.peer.clone();
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                let result = Self::call_on_socket_with_timeout(
                    &mut socket,
                    id,
                    "deviceSimulator",
                    request,
                    None,
                    None,
                )?;
                if result.get("ok").and_then(Value::as_bool) == Some(false) {
                    bail!(
                        "{}",
                        result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("Studio rejected the request")
                    );
                }
                Ok((result, peer))
            };
            let applied = (|| -> Result<(String, bool)> {
                let (status, _) = call_device(&json!({
                    "action": "capture-status",
                    "includeSettle": true,
                }))?;
                if let Some(settle_seconds) = status.get("settleSeconds").and_then(Value::as_f64)
                    && settle_seconds.is_finite()
                    && settle_seconds > 0.0
                {
                    thread::sleep(Duration::from_secs_f64(settle_seconds));
                }

                let request = desired_device_request
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                let apply = |request: &Value| -> Result<String> {
                    call_device(request).map(|(_, peer)| peer)
                };
                if request.get("action").and_then(Value::as_str) == Some("set") {
                    apply(&json!({ "action": "stop" }))?;
                }
                let should_close_toolbar =
                    request.get("action").and_then(Value::as_str) == Some("stop");
                apply(&request).map(|peer| (peer, should_close_toolbar))
            })();
            #[cfg(any(windows, target_os = "macos"))]
            let applied = applied.and_then(|(peer, should_close_toolbar)| {
                if !should_close_toolbar {
                    return Ok(());
                }
                (|| -> Result<()> {
                    let port = peer
                        .rsplit(':')
                        .next()
                        .and_then(|value| value.parse::<u16>().ok())
                        .with_context(|| format!("Could not parse peer port from '{peer}'"))?;
                    let pid = pid_for_local_tcp_port(port).with_context(|| {
                        format!("Could not map bridge connection {peer} to Studio")
                    })?;
                    input_inject::close_device_emulator_toolbar_when_visible(pid)?;
                    Ok(())
                })()
            });
            #[cfg(not(any(windows, target_os = "macos")))]
            let applied = applied.map(|_| ());
            if let Err(error) = &applied {
                eprintln!("[renium] failed to restore device simulator state: {error:#}");
            }
            if applied.is_err() {
                device_reconciled_runtimes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&runtime_id);
            }
        });
    }

    pub(crate) fn set_desired_device_request(&self, request: Value) {
        *self
            .desired_device_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = request;
    }

    pub(crate) fn merge_desired_device_request(&self, request: Value) {
        let mut desired = self
            .desired_device_request
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (Some(current), Some(update)) = (desired.as_object_mut(), request.as_object()) else {
            *desired = request;
            return;
        };
        if current.get("action").and_then(Value::as_str) != Some("set") {
            *desired = request;
            return;
        }
        current.extend(update.clone());
    }

    pub(crate) fn performance_profile_command(&self, parameters: &Value) -> Result<Value> {
        let manager = self
            .performance_manager
            .as_ref()
            .context("Performance profiles require the shared Renium daemon")?;
        let mut pids = Vec::new();
        if matches!(
            parameters.get("action").and_then(Value::as_str),
            Some("use" | "advanced")
        ) {
            let mut found = HashSet::new();
            for channel in &self.channels {
                for snapshot in Self::cached_channel_snapshots(channel) {
                    if Self::role_matches_target(&snapshot.role_key, BridgeTarget::Main)
                        && let Ok(pid) = Self::studio_pid_for_peer(&snapshot.peer)
                    {
                        found.insert(pid);
                    }
                }
            }
            pids.extend(found);
        }
        manager.command_with_enrollments(parameters, &pids)
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn check_for_update(
        runtime_id: String,
        channel: Arc<BridgeChannel>,
        next_id: Arc<std::sync::atomic::AtomicU64>,
        checked_runtimes: Arc<Mutex<HashSet<String>>>,
    ) {
        {
            let mut checked = checked_runtimes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !checked.insert(runtime_id.clone()) {
                return;
            }
        }

        thread::spawn(move || {
            let version = match update::latest_release_version() {
                Ok(version) => version,
                Err(error) => {
                    checked_runtimes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&runtime_id);
                    eprintln!("[renium] update check failed: {error:#}");
                    return;
                }
            };
            let sockets = channel
                .sockets
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(socket) = sockets
                .values()
                .find(|socket| {
                    socket.role == BRIDGE_ROLE_EDIT && socket.bridge_info.runtime_id == runtime_id
                })
                .cloned()
            else {
                checked_runtimes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&runtime_id);
                return;
            };
            drop(sockets);
            let mut socket = socket.io.lock().unwrap_or_else(PoisonError::into_inner);
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = Self::call_on_socket_with_timeout(
                &mut socket,
                id,
                "setUpdateStatus",
                &json!({ "latestVersion": version }),
                None,
                None,
            ) {
                checked_runtimes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&runtime_id);
                eprintln!("[renium] failed to send update status to Studio: {error:#}");
            }
        });
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn spawn_focus_watcher(channels: Vec<Arc<BridgeChannel>>, alive: Arc<AtomicBool>) {
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

    pub(crate) fn accept_ready_socket(
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
            .max_write_buffer_size(MAX_BRIDGE_MESSAGE_BYTES)
            .max_message_size(Some(MAX_BRIDGE_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_BRIDGE_MESSAGE_BYTES))
            .accept_unmasked_frames(false);
        let socket = accept_with_config(SharedTcpStream::from(stream), Some(socket_config))
            .with_context(|| format!("WebSocket upgrade failed on {bind_host}:{port}"))?;

        let mut bridge_socket = BridgeSocket {
            port,
            peer,
            studio_pid: None,
            role: BRIDGE_ROLE_UNKNOWN.to_string(),
            last_focused_at: accepted_at,
            bridge_info: BridgeInfoPayload::default(),
            request_session_id: request_session_id.to_string(),
            pending_final_console_snapshots: Vec::new(),
            pending_player_identity: None,
            active_request_lease: None,
            cancel_request_id: None,
            socket,
        };

        let bridge_info = Self::probe_bridge_info_on_socket_with_id(&mut bridge_socket, 1)
            .with_context(|| format!("readiness getBridgeInfo failed on {bind_host}:{port}"))?;
        bridge_socket.role = normalize_bridge_role(&bridge_info.bridge_role).to_string();
        bridge_socket.bridge_info = bridge_info;
        bridge_socket.studio_pid = Self::studio_pid_for_peer(&bridge_socket.peer).ok();

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

    pub(crate) fn probe_bridge_info_on_socket_with_id(
        bridge_socket: &mut BridgeSocket,
        id: u64,
    ) -> Result<BridgeInfoPayload> {
        let value = Self::call_on_socket_with_timeout(
            bridge_socket,
            id,
            "getBridgeInfo",
            &json!({"registrationAck": true}),
            None,
            None,
        )?;
        let info: BridgeInfoPayload =
            serde_json::from_value(value).context("Invalid getBridgeInfo response from plugin")?;
        validate_bridge_info(&info)?;
        Ok(info)
    }

    pub(crate) fn bridge_role_key_base(role_key: &str) -> &str {
        role_key
            .split_once(BRIDGE_DUPLICATE_ROLE_KEY_SEPARATOR)
            .map_or(role_key, |(base, _)| base)
    }

    pub(crate) fn bridge_socket_key(
        sockets: &HashMap<String, BridgeConnection>,
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

    pub(crate) fn role_matches_target(role_key: &str, target: BridgeTarget) -> bool {
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

    pub(crate) fn player_matches_selector(info: &BridgeInfoPayload, selector: &str) -> bool {
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

    fn play_owner_runtime(
        &self,
        info: &BridgeInfoPayload,
        studio_pid: Option<u32>,
    ) -> Option<String> {
        play_owner_runtime(
            info,
            studio_pid,
            self.channels
                .iter()
                .flat_map(|channel| Self::cached_channel_snapshots(channel)),
        )
    }

    fn bridge_info_matches_selector(
        &self,
        role_key: &str,
        info: &BridgeInfoPayload,
        studio_pid: Option<u32>,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> bool {
        if !Self::role_matches_target(role_key, target) || !self.runtime_is_routable(info) {
            return false;
        }
        if let Some(runtime_id) = crate::app::context::automation_runtime() {
            // Binding already resolved the place. Local test children can report
            // "Place1" while the owning edit runtime reports its .rbxl filename.
            // Use that exact runtime ownership, not a second mutable-name filter.
            if info.runtime_id != runtime_id
                && self.play_owner_runtime(info, studio_pid).as_deref() != Some(&runtime_id)
            {
                return false;
            }
        } else if let Some(place) = place_filter()
            && !place_matches(info, &place)
        {
            return false;
        }
        player.is_none_or(|selector| Self::player_matches_selector(info, selector))
    }

    pub(crate) fn socket_matches_selector(
        &self,
        role_key: &str,
        socket: &BridgeConnection,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> bool {
        self.bridge_info_matches_selector(
            role_key,
            &socket.bridge_info,
            socket.studio_pid,
            target,
            player,
        )
    }

    pub(crate) fn distinct_places_for_selector(
        &self,
        sockets: &HashMap<String, BridgeConnection>,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Vec<(Option<i64>, String)> {
        let mut places: Vec<(Option<i64>, String)> = Vec::new();
        for (role_key, socket) in sockets {
            if !self.socket_matches_selector(role_key.as_str(), socket, target, player) {
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

    pub(crate) fn ensure_place_unambiguous(
        &self,
        sockets: &mut HashMap<String, BridgeConnection>,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<()> {
        if crate::app::context::automation_runtime().is_some() {
            return Ok(());
        }
        let mut places = self.distinct_places_for_selector(sockets, target, player);
        if places.len() > 1 {
            let dead_keys: Vec<String> = sockets
                .iter_mut()
                .filter_map(|(key, socket)| {
                    if socket.is_alive() {
                        None
                    } else {
                        Some(key.clone())
                    }
                })
                .collect();
            for key in dead_keys {
                if let Some(dead_socket) = sockets.remove(&key) {
                    dead_socket.close();
                }
            }
            places = self.distinct_places_for_selector(sockets, target, player);
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

    pub(crate) fn target_label(target: BridgeTarget) -> &'static str {
        match target {
            BridgeTarget::Edit => "edit",
            BridgeTarget::Main => "main",
            BridgeTarget::Client => "play-client",
        }
    }

    pub(crate) fn role_preference_rank(role_key: &str, target: BridgeTarget) -> usize {
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

    pub(crate) fn runtime_pin_key(target: BridgeTarget, player: Option<&str>) -> RuntimePinKey {
        RuntimePinKey {
            target,
            player: player.map(|value| value.trim().to_ascii_lowercase()),
        }
    }

    pub(crate) fn clear_runtime_pins(&self) {
        self.runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn retire_runtime(&self, runtime_id: &str) {
        // Retire before best-effort cleanup: neither a busy channel nor an
        // in-flight handshake may resurrect this runtime.
        self.routing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retired
            .insert(runtime_id.to_string());
        for channel in &self.channels {
            let mut retired = {
                let mut sockets = match channel.sockets.try_lock() {
                    Ok(sockets) => sockets,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => continue,
                };
                let keys = sockets
                    .iter()
                    .filter(|(_, socket)| socket.bridge_info.runtime_id == runtime_id)
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                let retired = keys
                    .into_iter()
                    .filter_map(|key| sockets.remove(&key))
                    .collect::<Vec<_>>();
                Self::refresh_channel_snapshots(channel, &sockets);
                retired
            };
            for socket in &mut retired {
                socket.close();
            }
        }
        self.runtime_pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, pin| pin.runtime_id != runtime_id);
    }

    pub(crate) fn pin_runtime(&self, target: BridgeTarget, runtime_id: &str) {
        self.runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                Self::runtime_pin_key(target, None),
                RuntimePin {
                    runtime_id: runtime_id.to_string(),
                    exact: true,
                },
            );
    }

    pub(crate) fn choose_runtime_pin(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<RuntimePin> {
        let lock_deadline = Instant::now() + BRIDGE_CHANNEL_LOCK_TIMEOUT;
        let matching_socket_count = loop {
            let mut candidates: HashMap<String, RuntimePinCandidate> = HashMap::new();
            let mut matching_socket_count = 0usize;
            let mut busy = false;

            for channel in &self.channels {
                let mut guard = match channel.sockets.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        busy = true;
                        continue;
                    }
                };
                self.ensure_place_unambiguous(&mut guard, target, player)?;
                for (role_key, socket) in guard.iter() {
                    if !self.socket_matches_selector(role_key, socket, target, player) {
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
                    candidate.last_focused_at =
                        candidate.last_focused_at.max(socket.last_focused_at);
                }
            }

            let candidate = candidates.into_iter().max_by(|(_, left), (_, right)| {
                right
                    .role_rank
                    .cmp(&left.role_rank)
                    .then_with(|| left.ports.len().cmp(&right.ports.len()))
                    .then_with(|| left.last_focused_at.cmp(&right.last_focused_at))
            });
            if let Some((runtime_id, _)) = candidate {
                return Ok(RuntimePin {
                    runtime_id,
                    exact: false,
                });
            }
            if !busy || Instant::now() >= lock_deadline {
                break matching_socket_count;
            }
            thread::sleep(Duration::from_millis(2));
        };

        if target == BridgeTarget::Client
            && let Some(index) = player.and_then(|value| value.parse::<usize>().ok())
            && index > 0
            && let Some(runtime_id) = self.player_runtime_ids().get(index - 1)
        {
            return Ok(RuntimePin {
                runtime_id: runtime_id.clone(),
                exact: false,
            });
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

    fn player_runtime_ids(&self) -> Vec<String> {
        let mut players = Vec::new();
        for channel in &self.channels {
            let snapshots = match channel.sockets.try_lock() {
                Ok(sockets) => Self::refresh_channel_snapshots(channel, &sockets),
                Err(TryLockError::Poisoned(poisoned)) => {
                    Self::refresh_channel_snapshots(channel, &poisoned.into_inner())
                }
                Err(TryLockError::WouldBlock) => Self::cached_channel_snapshots(channel),
            };
            for snapshot in snapshots {
                let runtime_id = snapshot.bridge_info.runtime_id.trim();
                if !runtime_id.is_empty()
                    && self.bridge_info_matches_selector(
                        &snapshot.role_key,
                        &snapshot.bridge_info,
                        snapshot.studio_pid,
                        BridgeTarget::Client,
                        None,
                    )
                    && !players.iter().any(|(_, id)| id == runtime_id)
                {
                    players.push((
                        snapshot.bridge_info.player_name.to_ascii_lowercase(),
                        runtime_id.to_string(),
                    ));
                }
            }
        }
        players.sort_unstable();
        players
            .into_iter()
            .map(|(_, runtime_id)| runtime_id)
            .collect()
    }

    pub(crate) fn runtime_pin_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<RuntimePin> {
        let key = Self::runtime_pin_key(target, player);
        if target == BridgeTarget::Client
            && let Some(index) = player.and_then(|value| value.parse::<usize>().ok())
            && index > 0
        {
            let runtime_id = self
                .player_runtime_ids()
                .get(index - 1)
                .cloned()
                .with_context(|| format!("No connected play client exists at index {index}"))?;
            let pin = RuntimePin {
                runtime_id,
                exact: false,
            };
            self.runtime_pins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, pin.clone());
            return Ok(pin);
        }
        let existing = self
            .runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(pin) = existing {
            if pin.exact {
                return Ok(pin);
            }
            let mut best_rank = usize::MAX;
            let mut pinned_rank = None;
            for channel in &self.channels {
                for snapshot in Self::cached_channel_snapshots(channel) {
                    if !self.bridge_info_matches_selector(
                        &snapshot.role_key,
                        &snapshot.bridge_info,
                        snapshot.studio_pid,
                        target,
                        player,
                    ) {
                        continue;
                    }
                    let rank = Self::role_preference_rank(&snapshot.role_key, target);
                    best_rank = best_rank.min(rank);
                    if snapshot.bridge_info.runtime_id == pin.runtime_id {
                        pinned_rank =
                            Some(pinned_rank.map_or(rank, |current: usize| current.min(rank)));
                    }
                }
            }
            if pinned_rank.is_some_and(|rank| rank <= best_rank) {
                return Ok(pin);
            }
        }
        self.runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
        let pin = self.choose_runtime_pin(target, player)?;
        let mut pins = self
            .runtime_pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(pins.entry(key).or_insert(pin).clone())
    }

    pub(crate) fn socket_matches_runtime_pin(socket: &BridgeConnection, pin: &RuntimePin) -> bool {
        socket.bridge_info.runtime_id == pin.runtime_id
    }

    pub(crate) fn select_role_for_selector_with_pin(
        &self,
        sockets: &HashMap<String, BridgeConnection>,
        target: BridgeTarget,
        player: Option<&str>,
        pin: &RuntimePin,
    ) -> Option<String> {
        self.select_role_for_selector_inner(sockets, target, player, Some(pin))
    }

    fn select_role_for_selector_inner(
        &self,
        sockets: &HashMap<String, BridgeConnection>,
        target: BridgeTarget,
        player: Option<&str>,
        pin: Option<&RuntimePin>,
    ) -> Option<String> {
        let matched_player = if pin.is_some()
            && player
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|index| index > 0)
        {
            None
        } else {
            player
        };
        let matches = |key: &str, socket: &BridgeConnection| {
            if let Some(pin) = pin {
                // The caller already resolved an exact runtime. In particular, a
                // background long poll must not inherit another request's selection.
                Self::socket_matches_runtime_pin(socket, pin)
                    && Self::role_matches_target(key, target)
                    && self.runtime_is_routable(&socket.bridge_info)
                    && matched_player.is_none_or(|player| {
                        Self::player_matches_selector(&socket.bridge_info, player)
                    })
            } else {
                self.socket_matches_selector(key, socket, target, matched_player)
            }
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

    pub(crate) fn wait_for_ready_channels_for_target(
        &self,
        required_channels: usize,
        timeout: Duration,
        target: BridgeTarget,
    ) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let ready_channels = self.max_runtime_channel_coverage(target, None);
            if ready_channels >= required_channels || Instant::now() >= deadline {
                return ready_channels;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for_target_channels(
        &self,
        wait_seconds: f64,
        target: BridgeTarget,
        required_channels: usize,
    ) -> Result<()> {
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

    pub(crate) fn wait_for_target(&self, wait_seconds: f64, target: BridgeTarget) -> Result<()> {
        self.wait_for_target_channels(wait_seconds, target, 1)
    }

    pub(crate) fn wait_for_all_target(
        &self,
        wait_seconds: f64,
        target: BridgeTarget,
    ) -> Result<()> {
        self.wait_for_target_channels(wait_seconds, target, self.expected_channel_count())
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn peer_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<String> {
        let runtime_pin = self.runtime_pin_for_selector(target, player)?;
        for channel in &self.channels {
            let guard = match channel.sockets.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            if let Some(role) =
                self.select_role_for_selector_with_pin(&guard, target, player, &runtime_pin)
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

    pub(crate) fn cached_bridge_info_for_target(
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
                self.select_role_for_selector_with_pin(&guard, target, None, &runtime_pin)
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

    pub(crate) fn cache_export_options_for_target(
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
            let mut guard = match channel.sockets.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            for (role_key, socket) in guard.iter_mut() {
                if self.socket_matches_selector(role_key, socket, target, None)
                    && Self::socket_matches_runtime_pin(socket, &runtime_pin)
                {
                    socket.bridge_info.performance_mode = performance_mode.to_string();
                    socket.bridge_info.modified_default_bypass = modified_default_bypass;
                    socket.bridge_info.export_all_properties = export_all_properties;
                }
            }
        }
    }

    pub(crate) fn expected_channel_count(&self) -> usize {
        self.channels.len()
    }

    pub(crate) fn missing_ports_for_target(&self, target: BridgeTarget) -> Vec<u16> {
        self.channels
            .iter()
            .filter_map(|channel| {
                let guard = match channel.sockets.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => return None,
                };
                if self
                    .select_role_for_selector_inner(&guard, target, None, None)
                    .is_some()
                {
                    None
                } else {
                    Some(channel.port)
                }
            })
            .collect()
    }

    fn remove_connection(channel: &BridgeChannel, key: &str, connection: &BridgeConnection) {
        let mut sockets = channel
            .sockets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A late failure belongs to the old connection, never its replacement.
        if sockets
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(&current.io, &connection.io))
        {
            sockets.remove(key);
            Self::refresh_channel_snapshots(channel, &sockets);
        }
        connection.close();
    }

    fn refresh_channel_snapshots(
        channel: &BridgeChannel,
        sockets: &HashMap<String, BridgeConnection>,
    ) -> Vec<BridgeSocketSnapshot> {
        let snapshots = sockets
            .iter()
            .map(|(role_key, socket)| {
                (
                    role_key.clone(),
                    BridgeSocketSnapshot {
                        port: socket.port,
                        peer: socket.peer.clone(),
                        studio_pid: socket.studio_pid,
                        role_key: role_key.clone(),
                        bridge_info: socket.bridge_info.clone(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let values = snapshots.values().cloned().collect();
        *channel
            .snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = snapshots;
        values
    }

    fn cached_channel_snapshots(channel: &BridgeChannel) -> Vec<BridgeSocketSnapshot> {
        channel
            .snapshots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn is_transport_error_text(text: &str) -> bool {
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

    fn retire_failed_socket(
        channel: &BridgeChannel,
        connection: &BridgeConnection,
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
        Self::remove_connection(channel, socket_role, connection);
        Ok(format!("port {socket_port} role {role}: {error_text}"))
    }

    fn try_call_pinned_socket<T>(
        &self,
        context: &BridgeCallContext<'_>,
        last_error: &mut Option<String>,
        call: BridgeSocketCall<T>,
    ) -> Result<(Option<T>, bool)> {
        if self
            .routing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retired
            .contains(&context.runtime_pin.runtime_id)
        {
            bail!(
                "The pinned Studio runtime ended: {} was retired; its outstanding response is no longer valid",
                context.runtime_pin.runtime_id
            );
        }
        if !bridge_request_allowed_after_cancel(context.method, context.params)
            && let Some(lease) = self.active_request_lease()
        {
            lease.ensure_active()?;
        }
        let mut connected_socket = false;
        for offset in 0..self.channels.len() {
            let channel = &self.channels[(context.start + offset) % self.channels.len()];
            let sockets = match channel.sockets.try_lock() {
                Ok(sockets) => sockets,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            let Some(socket_role) = self.select_role_for_selector_with_pin(
                &sockets,
                context.target,
                context.player,
                context.runtime_pin,
            ) else {
                continue;
            };
            let Some(connection) = sockets.get(&socket_role).cloned() else {
                continue;
            };
            connected_socket = true;
            drop(sockets);
            let mut socket = match connection.io.try_lock() {
                Ok(socket) => socket,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            if !self.runtime_is_routable(&socket.bridge_info) {
                continue;
            }
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
            socket.active_request_lease = context.request_lease.clone();
            socket.cancel_request_id = context.cancel_request_id;
            let attempt_timeout = remaining_timeout.map(|remaining| {
                bridge_socket_attempt_timeout(context.method, context.params, remaining)
            });
            let result = call(
                &mut socket,
                context.id,
                context.method,
                context.params,
                attempt_timeout,
                context.lease_id,
            );
            socket.active_request_lease = None;
            socket.cancel_request_id = None;
            self.collect_socket_final_console_snapshots(&mut socket);
            let routable = self.runtime_is_routable(&socket.bridge_info);
            drop(socket);
            if !routable {
                bail!(
                    "The pinned Studio runtime ended: {} closed before its response could be accepted",
                    context.runtime_pin.runtime_id
                );
            }
            match result {
                Ok(result) => return Ok((Some(result), true)),
                Err(error) => {
                    // A queued/slow observation doesn't prove that its socket is dead.
                    // Late replies carry IDs and read_response skips them on the next call.
                    if is_status_observation(context.method, context.params)
                        && error.downcast_ref::<BridgeResponseTimeout>().is_some()
                    {
                        return Err(error);
                    }
                    *last_error = Some(Self::retire_failed_socket(
                        channel,
                        &connection,
                        &socket_role,
                        socket_port,
                        &role,
                        error,
                    )?);
                }
            }
        }
        Ok((None, connected_socket))
    }

    fn call_pinned_socket<T>(
        &self,
        context: &BridgeCallContext<'_>,
        call: BridgeSocketCall<T>,
        label: &str,
    ) -> Result<T> {
        let mut last_error = None;
        let mut connected = false;
        let mut lock_deadline = Instant::now() + bridge_channel_lock_timeout(context.method);
        if let Some(response_deadline) = context.response_deadline {
            lock_deadline = lock_deadline.min(response_deadline);
        }
        for _ in 0..64 {
            let (result, found) = self.try_call_pinned_socket(context, &mut last_error, call)?;
            connected = found;
            if let Some(result) = result {
                return Ok(result);
            }
            if Instant::now() >= lock_deadline {
                break;
            }
            thread::yield_now();
        }

        while Instant::now() < lock_deadline {
            let (result, found) = self.try_call_pinned_socket(context, &mut last_error, call)?;
            connected = found;
            if let Some(result) = result {
                return Ok(result);
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
                if connected {
                    "all compatible bridge channels are busy".to_string()
                } else {
                    "the pinned Studio runtime disconnected or has no connected channel".to_string()
                }
            })
        )
    }

    pub(crate) fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_for_target(method, params, default_method_target(method))
    }

    pub(crate) fn cancel_request_lease(&self, lease_id: &str) -> Result<Value> {
        let runtime_pin = self.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
        let (id, start) = self.next_call()?;
        let params = json!({ "leaseId": lease_id });
        let call_context = BridgeCallContext {
            id,
            method: "cancelRequestLease",
            params: &params,
            target: BridgeTarget::Edit,
            player: None,
            runtime_pin: &runtime_pin,
            start,
            response_deadline: Some(Instant::now() + BRIDGE_DEFAULT_RESPONSE_TIMEOUT),
            lease_id: None,
            request_lease: None,
            cancel_request_id: None,
        };
        self.call_pinned_socket(
            &call_context,
            Self::call_on_socket_with_timeout,
            "Bridge cancellation",
        )
    }

    pub(crate) fn call_for_target(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
    ) -> Result<Value> {
        self.call_for_selector(method, params, target, None)
    }

    pub(crate) fn call_for_selector(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<Value> {
        self.call_for_selector_with_timeout(method, params, target, player, None)
    }

    pub(crate) fn call_for_selector_with_timeout(
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

    pub(crate) fn call_for_runtime_with_timeout(
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

    pub(crate) fn call_for_selector_runtime_with_timeout(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        player: Option<&str>,
        runtime_id: Option<&str>,
        response_timeout: Option<Duration>,
    ) -> Result<Value> {
        let response_deadline = Some(
            Instant::now() + response_timeout.unwrap_or_else(|| bridge_response_timeout(method)),
        );
        let lease = self.active_request_lease();
        let allowed_after_cancel = bridge_request_allowed_after_cancel(method, &params);
        if !allowed_after_cancel && let Some(lease) = &lease {
            lease.ensure_active()?;
        }
        let runtime_pin = if let Some(runtime_id) = runtime_id {
            RuntimePin {
                runtime_id: runtime_id.to_string(),
                exact: true,
            }
        } else {
            self.runtime_pin_for_selector(target, player)?
        };
        let (id, start) = self.next_call()?;
        let call_context = BridgeCallContext {
            id,
            method,
            params: &params,
            target,
            player,
            runtime_pin: &runtime_pin,
            start,
            response_deadline,
            lease_id: if method == "startStopPlay" && allowed_after_cancel {
                None
            } else {
                bridge_request_lease_id(method, lease.as_deref())
            },
            request_lease: if allowed_after_cancel {
                None
            } else {
                lease.clone()
            },
            cancel_request_id: if allowed_after_cancel {
                None
            } else {
                lease
                    .as_ref()
                    .map(|_| self.next_id.fetch_add(1, Ordering::Relaxed))
            },
        };

        let result = self.call_pinned_socket(
            &call_context,
            Self::call_on_socket_with_timeout,
            "Bridge call",
        )?;
        if target == BridgeTarget::Edit
            && matches!(method, "startStopPlay" | "getStudioState" | "getBridgeInfo")
            && result.get("ok").and_then(Value::as_bool) != Some(false)
            && let Some(nonce) = result.get("launchNonce").and_then(Value::as_str)
        {
            self.routing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .observe_launch(&runtime_pin.runtime_id, id, nonce);
        }
        Ok(result)
    }

    pub(crate) fn channel_count_for_target(&self, target: BridgeTarget) -> usize {
        self.channel_count_for_selector(target, None)
    }

    pub(crate) fn max_runtime_channel_coverage(
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
                if !self.socket_matches_selector(role_key, socket, target, player) {
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

    pub(crate) fn channel_count_for_selector(
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
                self.select_role_for_selector_with_pin(&guard, target, player, &pin)
                    .is_some()
            })
            .count()
    }

    pub(crate) fn wait_for_ready_player(&self, player: &str, timeout: Duration) -> bool {
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

    fn capture_socket_notification(socket: &mut BridgeSocket, value: &Value) -> bool {
        if socket
            .pending_player_identity
            .is_some_and(|(id, _)| value["id"].as_u64() == Some(id))
        {
            // Only enrich this connection's player metadata. A reply must never retarget it.
            let info = &socket.bridge_info;
            let result = &value["result"];
            if value["ok"] == true
                && result["runtimeId"].as_str() == Some(info.runtime_id.as_str())
                && result["bridgeRole"].as_str() == Some(info.bridge_role.as_str())
                && result["launchNonce"].as_str() == Some(info.launch_nonce.as_str())
                && result["launchEditRuntimeId"].as_str()
                    == Some(info.launch_edit_runtime_id.as_str())
                && let Some(name) = result["playerName"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
            {
                socket.bridge_info.player_name = name.to_owned();
                socket.bridge_info.player_user_id = result["playerUserId"].as_i64();
            }
            return true;
        }
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

    pub(crate) fn retain_socket_final_console_snapshots(&self, socket: &mut BridgeSocket) {
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

    pub(crate) fn collect_socket_final_console_snapshots(&self, socket: &mut BridgeSocket) -> bool {
        let mut closed = false;
        if socket.socket.get_mut().set_nonblocking(true).is_ok() {
            // Inventory is observational: a chatty client must not monopolize it.
            for _ in 0..128 {
                match socket.socket.read() {
                    Ok(Message::Text(text)) => {
                        if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                            Self::capture_socket_notification(socket, &value);
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

    pub(crate) fn take_final_console_snapshots(&self, launch: &TestLaunch) -> Vec<Value> {
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

    pub(crate) fn socket_is_alive(socket: &mut BridgeSocket) -> bool {
        let stream = socket.socket.get_ref();
        if stream.set_nonblocking(true).is_err() {
            return true;
        }
        let mut probe = [0u8; 1];
        let peek = stream.peek(&mut probe);
        let alive = match peek {
            Ok(0) => false,
            Err(err) if err.kind() != io::ErrorKind::WouldBlock => false,
            _ => match socket.socket.send(Message::Ping(Vec::new().into())) {
                Ok(()) => true,
                Err(tungstenite::Error::Io(error)) => error.kind() == io::ErrorKind::WouldBlock,
                Err(_) => false,
            },
        };
        let _ = socket.socket.get_mut().set_nonblocking(false);
        alive
    }

    fn request_missing_player_identity(&self, socket: &mut BridgeSocket) {
        if socket.bridge_info.bridge_role != BRIDGE_ROLE_PLAY_CLIENT
            || !socket.bridge_info.player_name.trim().is_empty()
            || socket
                .pending_player_identity
                .is_some_and(|(_, sent)| sent.elapsed() < BRIDGE_QUICK_SOCKET_ATTEMPT_TIMEOUT)
            || socket.socket.get_mut().set_nonblocking(true).is_err()
        {
            return;
        }
        let id = socket.pending_player_identity.map_or_else(
            || self.next_id.fetch_add(1, Ordering::Relaxed),
            |(id, _)| id,
        );
        socket.pending_player_identity = Some((id, Instant::now()));
        // The existing response drain consumes this read-only reply, including while
        // another command is running. No inventory caller waits for Studio here.
        let _ = Self::send_request(
            socket,
            id,
            "getBridgeInfo",
            &json!({"registrationAck": true}),
            None,
        );
        let _ = socket.socket.get_mut().set_nonblocking(false);
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn cached_snapshot_is_live(snapshot: &BridgeSocketSnapshot) -> bool {
        snapshot.studio_pid.map_or_else(
            || Self::studio_pid_for_peer(&snapshot.peer).is_ok(),
            crate::daemon::is_process_alive,
        )
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    fn cached_snapshot_is_live(_snapshot: &BridgeSocketSnapshot) -> bool {
        true
    }

    fn runtime_is_routable(&self, info: &BridgeInfoPayload) -> bool {
        self.routing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .allows(info)
    }

    pub(crate) fn retain_current_clients(&self, clients: &mut Vec<Value>) {
        let routing = self.routing.lock().unwrap_or_else(PoisonError::into_inner);
        clients.retain(|client| {
            routing.allows_identity(
                client["runtimeId"].as_str().unwrap_or_default(),
                client["role"].as_str().unwrap_or_default(),
                client["launchEditRuntimeId"].as_str().unwrap_or_default(),
                client["launchNonce"].as_str().unwrap_or_default(),
            )
        });
    }

    pub(crate) fn list_bridge_clients(&self) -> Vec<Value> {
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
            let connections = match channel.sockets.try_lock() {
                Ok(sockets) => Some(sockets.clone()),
                Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner().clone()),
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(connections) = connections {
                for (key, connection) in connections {
                    let mut socket = match connection.io.try_lock() {
                        Ok(socket) => socket,
                        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                        Err(TryLockError::WouldBlock) => continue,
                    };
                    let dead = !self.runtime_is_routable(&socket.bridge_info)
                        || self.collect_socket_final_console_snapshots(&mut socket)
                        || !Self::socket_is_alive(&mut socket);
                    if dead {
                        drop(socket);
                        Self::remove_connection(channel, &key, &connection);
                        continue;
                    }
                    self.request_missing_player_identity(&mut socket);
                    let player_name = socket.bridge_info.player_name.clone();
                    let player_user_id = socket.bridge_info.player_user_id;
                    drop(socket);
                    let mut sockets = channel
                        .sockets
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some(current) = sockets.get_mut(&key)
                        && Arc::ptr_eq(&current.io, &connection.io)
                    {
                        current.bridge_info.player_name = player_name;
                        current.bridge_info.player_user_id = player_user_id;
                    }
                }
            }
            let snapshots = match channel.sockets.try_lock() {
                Ok(sockets) => Self::refresh_channel_snapshots(channel, &sockets),
                Err(TryLockError::Poisoned(poisoned)) => {
                    Self::refresh_channel_snapshots(channel, &poisoned.into_inner())
                }
                Err(TryLockError::WouldBlock) => Self::cached_channel_snapshots(channel)
                    .into_iter()
                    .filter(Self::cached_snapshot_is_live)
                    .collect(),
            };
            for snapshot in snapshots {
                if !self.runtime_is_routable(&snapshot.bridge_info) {
                    continue;
                }
                let role = Self::bridge_role_key_base(&snapshot.role_key).to_string();
                let info = &snapshot.bridge_info;
                let existing = entries.iter_mut().find(|entry| {
                    entry.role == role
                        && if info.runtime_id.is_empty() {
                            entry.runtime_id.is_empty()
                                && entry.launch_nonce == info.launch_nonce
                                && entry.launch_edit_runtime_id == info.launch_edit_runtime_id
                                && entry.player_name == info.player_name
                                && entry.player_user_id == info.player_user_id
                                && entry.place_id == info.place_id
                                && entry.game_id == info.game_id
                        } else {
                            entry.runtime_id == info.runtime_id
                        }
                });
                match existing {
                    Some(entry) => {
                        if !entry.ports.contains(&snapshot.port) {
                            entry.ports.push(snapshot.port);
                        }
                        if entry.launch_nonce.is_empty() {
                            entry.launch_nonce.clone_from(&info.launch_nonce);
                        }
                        if entry.launch_edit_runtime_id.is_empty() {
                            entry.launch_edit_runtime_id = self
                                .play_owner_runtime(info, snapshot.studio_pid)
                                .unwrap_or_default();
                        }
                        if entry.player_name.is_empty() {
                            entry.player_name.clone_from(&info.player_name);
                        }
                        entry.player_user_id = entry.player_user_id.or(info.player_user_id);
                        entry.place_id = entry.place_id.or(info.place_id);
                        entry.game_id = entry.game_id.or(info.game_id);
                        if entry.place_name.is_empty() {
                            entry.place_name.clone_from(&info.place_name);
                        }
                        entry.build_unix = entry.build_unix.max(info.bridge_build_unix);
                    }
                    None => entries.push(ClientEntry {
                        runtime_id: info.runtime_id.clone(),
                        launch_nonce: info.launch_nonce.clone(),
                        launch_edit_runtime_id: self
                            .play_owner_runtime(info, snapshot.studio_pid)
                            .unwrap_or_default(),
                        role,
                        player_name: info.player_name.clone(),
                        player_user_id: info.player_user_id,
                        place_id: info.place_id,
                        game_id: info.game_id,
                        place_name: info.place_name.clone(),
                        build_unix: info.bridge_build_unix,
                        ports: vec![snapshot.port],
                    }),
                }
            }
        }
        let edit_place_names = entries
            .iter()
            .filter(|entry| entry.role == BRIDGE_ROLE_EDIT && !entry.place_name.is_empty())
            .map(|entry| (entry.runtime_id.clone(), entry.place_name.clone()))
            .collect::<HashMap<_, _>>();
        for entry in &mut entries {
            if let Some(place_name) = edit_place_names.get(&entry.launch_edit_runtime_id) {
                entry.place_name.clone_from(place_name);
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

    pub(crate) fn channel_count(&self) -> usize {
        self.channel_count_for_target(BridgeTarget::Main)
    }

    pub(crate) fn call_chunk(&self, method: &str, params: Value) -> Result<BridgeChunk> {
        self.call_chunk_for_target(method, params, default_method_target(method))
    }

    pub(crate) fn call_chunk_for_target(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
    ) -> Result<BridgeChunk> {
        let lease = self.active_request_lease();
        let allowed_after_cancel = bridge_request_allowed_after_cancel(method, &params);
        if !allowed_after_cancel && let Some(lease) = &lease {
            lease.ensure_active()?;
        }
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
            response_deadline: Some(Instant::now() + bridge_response_timeout(method)),
            lease_id: bridge_request_lease_id(method, lease.as_deref()),
            request_lease: if allowed_after_cancel {
                None
            } else {
                lease.clone()
            },
            cancel_request_id: lease
                .as_ref()
                .map(|_| self.next_id.fetch_add(1, Ordering::Relaxed)),
        };

        self.call_pinned_socket(
            &call_context,
            Self::call_on_socket_chunk,
            "Bridge chunk call",
        )
    }

    pub(crate) fn call_chunk_for_runtime(
        &self,
        method: &str,
        params: Value,
        target: BridgeTarget,
        runtime_id: &str,
    ) -> Result<BridgeChunk> {
        let lease = self.active_request_lease();
        let allowed_after_cancel = bridge_request_allowed_after_cancel(method, &params);
        if !allowed_after_cancel && let Some(lease) = &lease {
            lease.ensure_active()?;
        }
        let runtime_pin = RuntimePin {
            runtime_id: runtime_id.to_string(),
            exact: true,
        };
        let (id, start) = self.next_call()?;
        let call_context = BridgeCallContext {
            id,
            method,
            params: &params,
            target,
            player: None,
            runtime_pin: &runtime_pin,
            start,
            response_deadline: Some(Instant::now() + bridge_response_timeout(method)),
            lease_id: bridge_request_lease_id(method, lease.as_deref()),
            request_lease: if allowed_after_cancel {
                None
            } else {
                lease.clone()
            },
            cancel_request_id: lease
                .as_ref()
                .map(|_| self.next_id.fetch_add(1, Ordering::Relaxed)),
        };

        self.call_pinned_socket(
            &call_context,
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

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn studio_pid_for_selector(
        &self,
        target: BridgeTarget,
        player: Option<&str>,
    ) -> Result<u32> {
        let peer = self.peer_for_selector(target, player)?;
        for channel in &self.channels {
            if let Some(pid) = Self::cached_channel_snapshots(channel)
                .into_iter()
                .find(|snapshot| snapshot.peer == peer)
                .and_then(|snapshot| snapshot.studio_pid)
            {
                return Ok(pid);
            }
        }
        Self::studio_pid_for_peer(&peer)
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn studio_pid_for_runtime(
        &self,
        target: BridgeTarget,
        runtime_id: &str,
    ) -> Result<u32> {
        for channel in &self.channels {
            for snapshot in Self::cached_channel_snapshots(channel) {
                if Self::role_matches_target(&snapshot.role_key, target)
                    && snapshot.bridge_info.runtime_id == runtime_id
                    && let Some(pid) = snapshot
                        .studio_pid
                        .or_else(|| Self::studio_pid_for_peer(&snapshot.peer).ok())
                {
                    return Ok(pid);
                }
            }
        }
        bail!("No connected Studio bridge found for runtime {runtime_id}")
    }

    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn runtime_id_for_studio_pid(
        &self,
        target: BridgeTarget,
        pid: u32,
    ) -> Result<String> {
        let mut matches = HashSet::new();
        for channel in &self.channels {
            for snapshot in Self::cached_channel_snapshots(channel) {
                if Self::role_matches_target(&snapshot.role_key, target)
                    && snapshot
                        .studio_pid
                        .or_else(|| Self::studio_pid_for_peer(&snapshot.peer).ok())
                        == Some(pid)
                {
                    matches.insert(snapshot.bridge_info.runtime_id);
                }
            }
        }
        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("one runtime remains")),
            0 => bail!("Studio process {pid} has no connected edit bridge"),
            count => bail!("Studio process {pid} has {count} edit runtimes"),
        }
    }

    fn studio_pid_for_peer(peer: &str) -> Result<u32> {
        let port = peer
            .rsplit(':')
            .next()
            .and_then(|value| value.parse::<u16>().ok())
            .with_context(|| format!("Could not parse peer port from '{peer}'"))?;
        pid_for_local_tcp_port(port)
            .with_context(|| format!("Could not map bridge connection {peer} to Studio"))
    }

    pub(crate) fn call_on_socket_with_timeout(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
        response_timeout: Option<Duration>,
        lease_id: Option<&str>,
    ) -> Result<Value> {
        match Self::request_on_socket(
            bridge_socket,
            id,
            method,
            params,
            response_timeout,
            lease_id,
        )? {
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
        lease_id: Option<&str>,
    ) -> Result<BridgeChunk> {
        match Self::request_on_socket(
            bridge_socket,
            id,
            method,
            params,
            response_timeout,
            lease_id,
        )? {
            BridgeResponse::Chunk(chunk) => Ok(chunk),
            BridgeResponse::Json(result) => parse_bridge_chunk(result),
        }
    }

    fn request_on_socket(
        socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
        timeout: Option<Duration>,
        lease_id: Option<&str>,
    ) -> Result<BridgeResponse> {
        let started = Instant::now();
        let timeout = timeout.unwrap_or_else(|| bridge_response_timeout(method));
        if timeout.is_zero() {
            return Err(BridgeResponseTimeout(format!(
                "Bridge response timed out before {method}"
            ))
            .into());
        }
        let payload = Self::encode_request(socket, id, method, params, lease_id)?;
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(BridgeResponseTimeout(format!(
                "Bridge response timed out before sending {method}"
            ))
            .into());
        }
        Self::configure_request_timeout(socket, remaining);
        Self::send_encoded_request(socket, method, payload)?;
        Self::read_response(
            socket,
            id,
            method,
            timeout.saturating_sub(started.elapsed()),
        )
    }

    pub(crate) fn configure_request_timeout(bridge_socket: &mut BridgeSocket, timeout: Duration) {
        let _ = bridge_socket
            .socket
            .get_mut()
            .set_read_timeout(Some(timeout));
        let _ = bridge_socket
            .socket
            .get_mut()
            .set_write_timeout(Some(timeout.min(Duration::from_secs(10))));
    }

    pub(crate) fn send_request(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
        lease_id: Option<&str>,
    ) -> Result<()> {
        let payload = Self::encode_request(bridge_socket, id, method, params, lease_id)?;
        Self::send_encoded_request(bridge_socket, method, payload)
    }

    fn encode_request(
        bridge_socket: &BridgeSocket,
        id: u64,
        method: &str,
        params: &Value,
        lease_id: Option<&str>,
    ) -> Result<String> {
        #[derive(Serialize)]
        struct BridgeRequest<'a> {
            id: u64,
            session_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            lease_id: Option<&'a str>,
            method: &'a str,
            params: &'a Value,
        }

        let payload = serde_json::to_string(&BridgeRequest {
            id,
            session_id: &bridge_socket.request_session_id,
            lease_id,
            method,
            params,
        })?;
        if payload.len() > MAX_BRIDGE_REQUEST_BYTES {
            return Err(anyhow::Error::new(BridgeRequestTooLarge {
                method: method.to_string(),
                bytes: payload.len(),
            }));
        }

        Ok(payload)
    }

    fn send_encoded_request(
        bridge_socket: &mut BridgeSocket,
        method: &str,
        payload: String,
    ) -> Result<()> {
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

    pub(crate) fn read_response(
        bridge_socket: &mut BridgeSocket,
        id: u64,
        method: &str,
        timeout: Duration,
    ) -> Result<BridgeResponse> {
        let deadline = Instant::now() + timeout;
        let mut unrelated_messages = 0usize;
        let mut cancel_sent = false;
        let mut cancel_acknowledged = false;
        let mut original_response: Option<Result<BridgeResponse>> = None;
        loop {
            if cancel_acknowledged && let Some(response) = original_response.take() {
                return response;
            }
            if !cancel_sent
                && let Some(lease) = bridge_socket.active_request_lease.as_deref()
                && lease.is_cancelled()
            {
                let cancel_id = bridge_socket
                    .cancel_request_id
                    .context("Cancelled bridge request has no cancellation message id")?;
                Self::send_request(
                    bridge_socket,
                    cancel_id,
                    "cancelRequestLease",
                    &json!({ "leaseId": lease.id() }),
                    None,
                )?;
                cancel_sent = true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(BridgeResponseTimeout(format!(
                    "Bridge response timed out awaiting {method} on port {} after {:.1}s",
                    bridge_socket.port,
                    timeout.as_secs_f64()
                ))
                .into());
            }
            let poll_timeout = if bridge_socket.active_request_lease.is_some() {
                remaining.min(Duration::from_millis(5))
            } else {
                remaining
            };
            let _ = bridge_socket
                .socket
                .get_mut()
                .set_read_timeout(Some(poll_timeout));
            let message = match bridge_socket.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "Bridge read failed for method {method} on port {}",
                            bridge_socket.port
                        )
                    });
                }
            };
            match message {
                Message::Text(text) => {
                    if text.starts_with("RBS2 ")
                        || text.starts_with("RBS3 ")
                        || text.starts_with("RBS4 ")
                    {
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
                            let response = Ok(BridgeResponse::Chunk(chunk));
                            if cancel_sent && !cancel_acknowledged {
                                original_response = Some(response);
                                continue;
                            }
                            return response;
                        }
                        continue;
                    }
                    let mut parsed: Value =
                        serde_json::from_str(text.as_str()).with_context(|| {
                            format!("Invalid bridge JSON message ({} bytes)", text.len())
                        })?;
                    if Self::capture_socket_notification(bridge_socket, &parsed) {
                        continue;
                    }
                    let msg_id = parsed.get("id").and_then(Value::as_u64);
                    if cancel_sent && msg_id == bridge_socket.cancel_request_id {
                        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
                            let error = parsed
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("Studio rejected request cancellation");
                            bail!("Bridge cancellation failed while awaiting {method}: {error}");
                        }
                        cancel_acknowledged = true;
                        if let Some(response) = original_response.take() {
                            return response;
                        }
                        continue;
                    }
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
                    let response = if ok {
                        let result = parsed
                            .as_object_mut()
                            .and_then(|object| object.remove("result"))
                            .unwrap_or(Value::Null);
                        Ok(BridgeResponse::Json(result))
                    } else {
                        let err = parsed
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("bridge error");
                        Err(anyhow::Error::new(BridgeApplicationError {
                            method: method.to_string(),
                            message: err.to_string(),
                        }))
                    };
                    if cancel_sent && !cancel_acknowledged {
                        original_response = Some(response);
                        continue;
                    }
                    return response;
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
            for (_, socket) in guard.drain() {
                socket.close();
            }
        }
    }
}

pub(crate) fn parse_bridge_raw_chunk(mut text: String) -> Result<Option<(u64, BridgeChunk)>> {
    let version = if text.starts_with("RBS4 ") {
        4
    } else if text.starts_with("RBS3 ") {
        3
    } else if text.starts_with("RBS2 ") {
        2
    } else {
        return Ok(None);
    };

    let payload_index = text
        .find('\n')
        .map(|index| index + 1)
        .with_context(|| "Invalid raw bridge frame: missing payload separator")?;
    let (
        id,
        start,
        next_start,
        total,
        plugin_server_ms,
        plugin_encode_ms,
        serialization_complete,
        payload_cache_hit,
        payload_hash,
        compression,
        uncompressed_bytes,
    ) = {
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
        let payload_cache_hit = if version == 3 || version == 4 {
            match parts.next() {
                Some("0") => false,
                Some("1") => true,
                Some(_) => bail!("Invalid raw bridge frame payload cache state"),
                None => bail!("Invalid raw bridge frame: missing payload cache state"),
            }
        } else {
            false
        };
        let payload_hash = if version == 3 || version == 4 {
            let value = parts
                .next()
                .filter(|value| !value.is_empty())
                .context("Invalid raw bridge frame: missing payload hash")?;
            (value != "-").then(|| value.to_string())
        } else {
            None
        };
        let compression = if version == 4 {
            Some(
                parts
                    .next()
                    .filter(|value| !value.is_empty())
                    .context("Invalid raw bridge frame: missing compression")?
                    .to_string(),
            )
        } else {
            None
        };
        let uncompressed_bytes = if version == 4 {
            Some(
                parts
                    .next()
                    .context("Invalid raw bridge frame: missing uncompressed size")?
                    .parse::<usize>()
                    .context("Invalid raw bridge frame uncompressed size")?,
            )
        } else {
            None
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
            payload_cache_hit,
            payload_hash,
            compression,
            uncompressed_bytes,
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
        payload_hash,
        payload_cache_hit,
        compression,
        uncompressed_bytes,
    };
    validate_bridge_chunk(&chunk)?;
    Ok(Some((id, chunk)))
}
