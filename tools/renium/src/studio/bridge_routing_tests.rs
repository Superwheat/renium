use super::*;

pub(crate) fn fixture() -> BridgeServer {
    BridgeServer {
        #[cfg(any(windows, target_os = "macos"))]
        verified_full_pushes: Default::default(),
        channels: (0..2)
            .map(|port| {
                Arc::new(BridgeChannel {
                    port,
                    sockets: Mutex::new(HashMap::new()),
                    snapshots: Mutex::new(HashMap::new()),
                })
            })
            .collect(),
        listening: Vec::new(),
        alive: Arc::new(AtomicBool::new(true)),
        next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        preferred_index: Default::default(),
        request_gate: Default::default(),
        runtime_gates: Mutex::new(HashMap::new()),
        runtime_claims: Mutex::new(HashMap::new()),
        active_request_leases: Mutex::new(HashMap::new()),
        runtime_pins: Mutex::new(HashMap::new()),
        routing: Default::default(),
        desired_device_request: Default::default(),
        performance_manager: None,
        team_create: Mutex::new(HashMap::new()),
    }
}

#[test]
fn unfiltered_verify_uses_only_plugin_sources_when_native_capture_is_unavailable() {
    use crate::automation::authorization::signed_fixture_request;
    use crate::automation::{State, op, runtime::automation_parse_response_with_lease};
    // Isolate process-global project selection from the parallel test suite.
    if std::env::var_os("RENIUM_TEST_VERIFY_FALLBACK").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "studio::bridge::routing_tests::unfiltered_verify_uses_only_plugin_sources_when_native_capture_is_unavailable"])
            .env("RENIUM_TEST_VERIFY_FALLBACK", "1").status().unwrap();
        assert!(status.success());
        return;
    }
    let root = crate::tests::support::temp_dir("verify-native-fallback");
    std::fs::create_dir_all(root.join("src/ServerScriptService")).unwrap();
    std::fs::write(
        root.join("src/ServerScriptService/Canary.server.luau"),
        "return 42\n",
    )
    .unwrap();
    std::fs::write(
        root.join("renium.project.jsonc"),
        r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
    )
    .unwrap();
    let mut bridge = fixture();
    bridge.channels.truncate(1);
    let bridge = Arc::new(bridge);
    // The test process is deliberately not Studio: native capture cannot pass.
    let stream = connect(&bridge, 0, edit_info("verify-edit", 20));
    let worker = thread::spawn(move || {
        let mut peer =
            WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None);
        let mut methods = Vec::new();
        while let Ok(message) = peer.read() {
            let Message::Text(text) = message else {
                if let Message::Ping(bytes) = message {
                    peer.send(Message::Pong(bytes)).unwrap();
                }
                continue;
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            methods.push(request["method"].as_str().unwrap().to_string());
            assert_eq!(
                request["method"], "getLiveSourceBatch",
                "unexpected mutation or capture: {request}"
            );
            assert!(request["params"]["transactionId"].is_null());
            peer.send(Message::Text(
                json!({"id":request["id"], "ok":true, "result":{
                    "rows":[{"index":0,"source":"return 42\n","className":"Script"}]
                }})
                .to_string()
                .into(),
            ))
            .unwrap();
        }
        methods
    });
    let state = State::default();
    let bind = automation_parse_response_with_lease(
        &signed_fixture_request(
            &state,
            json!({
                "v":1,"id":1,"op":op::BIND,"p":{"root":root,"place":"10:20"}
            }),
        ),
        &state,
        &bridge,
        0.1,
        None,
    );
    assert_eq!(bind.ok, 1, "{}", serde_json::to_string(&bind).unwrap());
    let context = bind.r.unwrap()["id"].as_u64().unwrap();
    let response = automation_parse_response_with_lease(
        &signed_fixture_request(
            &state,
            json!({
                "v":1,"id":2,"op":op::PUSH,"cx":context,"p":{"verify":true,"compact":true}
            }),
        ),
        &state,
        &bridge,
        0.1,
        None,
    );
    assert_eq!(
        response.ok,
        1,
        "{}",
        serde_json::to_string(&response).unwrap()
    );
    let result = response.r.unwrap();
    assert_eq!(result["verifyOnly"], true, "{result}");
    assert_eq!(result["sourceVerified"], 1, "{result}");
    assert_eq!(
        result["sourceVerification"], "plugin ScriptDocument/source batch",
        "{result}"
    );
    drop(bridge);
    assert_eq!(worker.join().unwrap(), ["getLiveSourceBatch"]);
    std::fs::remove_dir_all(root).unwrap();
}

fn client_info(runtime: &str, launch: &str) -> BridgeInfoPayload {
    BridgeInfoPayload {
        runtime_id: runtime.into(),
        launch_nonce: launch.into(),
        launch_edit_runtime_id: "edit-dte".into(),
        bridge_role: BRIDGE_ROLE_PLAY_CLIENT.into(),
        player_name: "The_SirMeme".into(),
        player_user_id: Some(1176961298),
        ..Default::default()
    }
}

// Only loopback sockets. No daemon, native watchers, Studio, or game mutations.
pub(crate) fn connect(
    bridge: &BridgeServer,
    channel_index: usize,
    info: BridgeInfoPayload,
) -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (stream, address) = listener.accept().unwrap();
    let channel = &bridge.channels[channel_index];
    let mut sockets = channel.sockets.lock().unwrap();
    let key = BridgeServer::bridge_socket_key(&sockets, &info.bridge_role, &address.to_string());
    sockets.insert(
        key,
        BridgeConnection::new(BridgeSocket {
            port: channel.port,
            peer: address.to_string(),
            studio_pid: Some(std::process::id()),
            role: info.bridge_role.clone(),
            last_focused_at: Instant::now(),
            bridge_info: info,
            request_session_id: "routing-test".into(),
            pending_player_identity: None,
            active_request_lease: None,
            cancel_request_id: None,
            socket: WebSocket::from_raw_socket(
                stream.into(),
                tungstenite::protocol::Role::Server,
                None,
            ),
        }),
    );
    BridgeServer::refresh_channel_snapshots(channel, &sockets);
    peer
}

#[test]
fn verify_push_dispatch_reads_sources_without_replacement_or_review() {
    // Runtime selection is process-global; keep this wire test isolated.
    if std::env::var_os("RENIUM_TEST_VERIFY_DISPATCH").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "studio::bridge::routing_tests::verify_push_dispatch_reads_sources_without_replacement_or_review"])
            .env("RENIUM_TEST_VERIFY_DISPATCH", "1").status().unwrap();
        assert!(status.success());
        return;
    }
    use crate::automation::authorization::signed_fixture_request;
    use crate::automation::{State, op, runtime::automation_parse_response_with_lease};
    use crate::tests::support::{settings_document, settings_instance, temp_dir};
    let root = temp_dir("verify-dispatch");
    let service = root.join("src/ReplicatedStorage");
    std::fs::create_dir_all(&service).unwrap();
    std::fs::write(
        root.join("renium.project.jsonc"),
        r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("instances")).unwrap();
    let store = root.join("instances/ReplicatedStorage.renium");
    settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("code", "Code", "ModuleScript", Some(0)),
    ])
    .write_file(&store)
    .unwrap();
    let script = service.join("Code.luau");
    std::fs::write(&script, "return 1").unwrap();
    let before = std::fs::read(&store).unwrap();
    let bridge = Arc::new(listening_fixture());
    let methods = Arc::new(Mutex::new(Vec::new()));
    let mut responders = Vec::new();
    for channel in 0..2 {
        let mut peer = handshake(&bridge, channel, &edit_info("verify-only", 20)).unwrap();
        let received = Arc::clone(&methods);
        responders.push(thread::spawn(move || {
        while let Ok(message) = peer.read() {
            let Message::Text(text) = message else { continue; };
            let request: Value = serde_json::from_str(&text).unwrap();
            let method = request["method"].as_str().unwrap().to_string();
            received.lock().unwrap().push(method.clone());
            let result = if method == "getLiveSourceBatch" {
                json!({"rows": [{"index": 0, "source": "return 1", "className": "ModuleScript"}]})
            } else {
                json!({})
            };
            if peer
                .send(Message::Text(
                    json!({"id": request["id"], "ok": true, "result": result})
                        .to_string()
                        .into(),
                ))
                .is_err()
            {
                break;
            }
        }
        }));
    }
    let state = State::default();
    let request = |value| {
        automation_parse_response_with_lease(
            &signed_fixture_request(&state, value),
            &state,
            &bridge,
            0.1,
            None,
        )
    };
    let bound =
        request(json!({"v": 1, "id": 1, "op": op::BIND, "p": {"root": root, "place": "10:20"}}));
    assert_eq!(bound.ok, 1, "{}", serde_json::to_string(&bound).unwrap());
    assert_eq!(
        bound.r.as_ref().unwrap()["runtimeId"],
        "verify-only",
        "{:?}",
        methods.lock().unwrap()
    );
    let context = bound.r.unwrap()["id"].as_u64().unwrap();
    for (index, paths) in [json!([]), json!([store]), json!([service]), json!([script])]
        .into_iter()
        .enumerate()
    {
        let response = request(
            json!({"v": 1, "id": index + 2, "op": op::PUSH, "cx": context,
            "p": {"verify": true, "destructive": true, "changedPaths": paths}}),
        );
        assert_eq!(
            response.ok,
            1,
            "{}",
            serde_json::to_string(&response).unwrap()
        );
        let result = response.r.unwrap();
        assert_eq!(result["verifyOnly"], true);
        // This store carries no inline sources; a store-only selection checks
        // no scripts and must still never apply its instance/property changes.
        assert_eq!(result["sourceVerified"], if index == 1 { 0 } else { 1 });
    }
    let response = request(
        json!({"v": 1, "id": 6, "op": op::REVIEW_PREPARE, "cx": context,
        "p": {"op": op::PUSH, "p": {"verify": true}}}),
    );
    assert_eq!(response.ok, 0);
    assert_eq!(response.e.unwrap().c, "bad_req");
    assert_eq!(std::fs::read(&store).unwrap(), before);
    assert_eq!(*methods.lock().unwrap(), vec!["getLiveSourceBatch"; 3]);
    drop(bridge);
    for responder in responders {
        responder.join().unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dropping_the_server_ends_its_accept_loops() {
    let bridge = listening_fixture();
    let listening = bridge.listening.clone();
    assert_eq!(listening.len(), 2);
    drop(bridge);
    for address in listening {
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Err(error) = TcpListener::bind(address) {
            assert!(
                Instant::now() < deadline,
                "the accept loop still holds {address}: {error}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn a_failed_bind_leaves_the_ports_bound_before_it_free() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let ports = [free.port(), taken.local_addr().unwrap().port()];
    assert!(BridgeServer::listen_configured("127.0.0.1", &ports, 0.0, false, false).is_err());
    // A process another test is spawning holds a copy of the closed listener
    // until it execs; an accept loop would hold the port for good.
    let started = Instant::now();
    while let Err(error) = TcpListener::bind(free) {
        assert!(started.elapsed() < Duration::from_secs(5), "{error}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn listening_fixture() -> BridgeServer {
    listening_fixture_with_preparation(
        #[cfg(any(windows, target_os = "macos"))]
        None,
    )
}

fn listening_fixture_with_preparation(
    #[cfg(any(windows, target_os = "macos"))] native_preparation: Option<
        Arc<NativeConnectionPreparation>,
    >,
) -> BridgeServer {
    let mut bridge = fixture();
    let mut listeners = Vec::new();
    bridge.channels = (0..2)
        .map(|_| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            bridge.listening.push(listener.local_addr().unwrap());
            let channel = Arc::new(BridgeChannel {
                port: listener.local_addr().unwrap().port(),
                sockets: Mutex::new(HashMap::new()),
                snapshots: Mutex::new(HashMap::new()),
            });
            listeners.push(listener);
            channel
        })
        .collect();
    let state = BridgeAcceptState {
        alive: Arc::clone(&bridge.alive),
        request_session_id: "routing-test".into(),
        next_id: Arc::clone(&bridge.next_id),
        routing: Arc::clone(&bridge.routing),
        desired_device_request: Default::default(),
        device_reconciled_runtimes: Default::default(),
        all_channels: Arc::new(Mutex::new(bridge.channels.clone())),
        expected_channels: 2,
        reconcile_device_on_connect: false,
        performance_manager: None,
        #[cfg(any(windows, target_os = "macos"))]
        update_checked_runtimes: Default::default(),
        #[cfg(any(windows, target_os = "macos"))]
        check_updates_on_connect: false,
        #[cfg(any(windows, target_os = "macos"))]
        native_preparation,
    };
    for (channel, listener) in bridge.channels.iter().zip(listeners) {
        BridgeServer::spawn_accept_loop(
            "127.0.0.1".into(),
            channel.port,
            listener,
            Arc::clone(channel),
            state.clone(),
        );
    }
    bridge
}

fn handshake(
    bridge: &BridgeServer,
    channel: usize,
    info: &BridgeInfoPayload,
) -> Option<WebSocket<TcpStream>> {
    let address = format!("127.0.0.1:{}", bridge.channels[channel].port);
    // A loaded machine (another test binary or a release build on the same
    // runner) can reset the socket during the WebSocket upgrade or before the
    // first frame; that is noise from the host, not from the bridge, so the
    // opening handshake is retried a few times before the test gives up.
    let (mut peer, request) = connect_until_registered(&address);
    peer.send(Message::Text(
        json!({"id": request["id"], "ok": true, "result": {
            "runtimeId": info.runtime_id, "bridgeRole": info.bridge_role,
            "launchNonce": info.launch_nonce, "launchEditRuntimeId": info.launch_edit_runtime_id,
            "playerName": info.player_name, "playerUserId": info.player_user_id,
            "gameId": info.game_id, "placeId": info.place_id, "placeName": info.place_name,
            "protocolVersion": "compact-v5", "codecVersion": "compact-v5-schema-9",
            "registrationAck": true
        }})
        .to_string()
        .into(),
    ))
    .unwrap();
    let ack = peer.read().ok()?;
    let ack: Value = serde_json::from_str(&ack.into_text().ok()?).ok()?;
    assert_eq!(ack["method"], "bridgeRegistered");
    #[cfg(target_os = "macos")]
    {
        // macOS's real owner lookup deliberately accepts only Studio processes.
        // These real sockets belong to this test executable, so supply its
        // actual PID before exercising the busy-registry snapshot path.
        let channel = &bridge.channels[channel];
        let mut sockets = channel.sockets.lock().unwrap();
        for connection in sockets.values_mut() {
            if connection.bridge_info.runtime_id == info.runtime_id {
                connection.studio_pid = Some(std::process::id());
            }
        }
        BridgeServer::refresh_channel_snapshots(channel, &sockets);
    }
    Some(peer)
}

fn connect_until_registered(address: &str) -> (WebSocket<TcpStream>, Value) {
    let mut last_error = String::new();
    for attempt in 0..8 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(150 * attempt));
        }
        let stream = match TcpStream::connect(address) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = format!("connect: {error}");
                continue;
            }
        };
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut peer = match tungstenite::client(format!("ws://{address}"), stream) {
            Ok((peer, _)) => peer,
            Err(error) => {
                last_error = format!("upgrade: {error}");
                continue;
            }
        };
        match peer.read() {
            Ok(message) => {
                let request: Value = serde_json::from_str(&message.into_text().unwrap()).unwrap();
                return (peer, request);
            }
            Err(error) => {
                last_error = format!("first frame: {error}");
            }
        }
    }
    panic!("the test peer could not open a bridge channel on {address}: {last_error}");
}

fn respond(mut peer: WebSocket<TcpStream>, result: Value) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(message) = peer.read() {
            if let Message::Text(text) = message {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = json!({"id": request["id"], "ok": true, "result": result});
                if peer.send(Message::Text(result.to_string().into())).is_err() {
                    break;
                }
            }
        }
    })
}

pub(crate) fn edit_info(runtime: &str, place: i64) -> BridgeInfoPayload {
    BridgeInfoPayload {
        runtime_id: runtime.into(),
        bridge_role: BRIDGE_ROLE_EDIT.into(),
        game_id: Some(10),
        place_id: Some(place),
        place_name: format!("place-{place}"),
        protocol_version: "compact-v5".into(),
        codec_version: "compact-v5-schema-9".into(),
        ..Default::default()
    }
}

/// A play runtime the Edit window `owner` launched.
pub(crate) fn play_info(
    runtime: &str,
    owner: &str,
    role: &str,
    player: &str,
    place: i64,
) -> BridgeInfoPayload {
    let mut info = edit_info(runtime, place);
    info.bridge_role = role.into();
    info.launch_edit_runtime_id = owner.into();
    info.launch_nonce = format!("launch-{owner}");
    info.player_name = player.into();
    info
}

/// Marks `runtime_id` as the window focused last.
pub(crate) fn focus(bridge: &BridgeServer, runtime_id: &str) {
    let now = Instant::now() + Duration::from_secs(60);
    for channel in &bridge.channels {
        let mut sockets = channel.sockets.lock().unwrap();
        for socket in sockets.values_mut() {
            if socket.bridge_info.runtime_id == runtime_id {
                socket.last_focused_at = now;
            }
        }
        BridgeServer::refresh_channel_snapshots(channel, &sockets);
    }
}

#[test]
fn native_export_stays_in_edit_during_rapid_play_server_replacement() {
    // Context selection is global; isolate the real routing test from other tests.
    if std::env::var_os("RENIUM_TEST_EXPORT_ROUTING").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "studio::bridge::routing_tests::native_export_stays_in_edit_during_rapid_play_server_replacement"])
            .env("RENIUM_TEST_EXPORT_ROUTING", "1").status().unwrap();
        assert!(status.success());
        return;
    }
    let _selection = crate::app::context::select_automation(
        Some("export-edit".into()),
        ".".into(),
        Some("10:20".into()),
    );
    let bridge = fixture();
    let peer = connect(&bridge, 0, edit_info("export-edit", 20));
    let context = crate::automation::BoundContext {
        id: 1,
        initialized: true,
        project: "routing/renium.project.jsonc".into(),
        root: "routing".into(),
        experience: String::new(),
        source: "routing/src".into(),
        resource_lease: None,
        place_id: Some(20),
        game_id: Some(10),
        selector: "10:20".into(),
        runtime_id: Some("export-edit".into()),
        plugin_build: None,
        fingerprint: String::new(),
    };
    peer.set_nodelay(true).unwrap();
    let edit_worker = respond(
        WebSocket::from_raw_socket(peer, tungstenite::protocol::Role::Client, None),
        json!({"runtime":"export-edit","start":1,"nextStart":5,"total":4,"chunk":"edit"}),
    );
    for cycle in 0..32 {
        let mut info = edit_info(&format!("play-{cycle}"), 20);
        info.bridge_role = BRIDGE_ROLE_PLAY_SERVER.into();
        info.launch_edit_runtime_id = "export-edit".into();
        let peer = connect(&bridge, 1, info);
        peer.set_nodelay(true).unwrap();
        let play_worker = respond(
            WebSocket::from_raw_socket(peer, tungstenite::protocol::Role::Client, None),
            json!({"runtime":"play","start":1,"nextStart":5,"total":4,"chunk":"play"}),
        );
        // Reproduce Live Sync's Edit pin while a higher-ranked server exists.
        bridge.clear_runtime_pins();
        bridge.pin_runtime(BridgeTarget::Edit, "export-edit");
        for method in [
            "beginEditorBinaryExport",
            "awaitEditorBinaryExport",
            "finishEditorBinaryExport",
            "beginEditorTransaction",
            "commitEditorTransaction",
            "rollbackEditorTransaction",
            "getLiveSourceBatch",
            "getStudioChangeState",
            "configurePropertyCandidates",
            "setExportOptions",
            "recordSyncCompletion",
        ] {
            assert_eq!(
                bridge.call(method, json!({})).unwrap()["runtime"],
                "export-edit",
                "{method}, cycle {cycle}"
            );
        }
        for method in [
            "readEditorBinaryExport",
            "readEditorBinaryExportBatch",
            "getEditorBinaryOverlayChunk",
        ] {
            assert_eq!(
                bridge.call_chunk(method, json!({})).unwrap().chunk,
                "edit",
                "{method}, cycle {cycle}"
            );
        }
        // The same snapshot also fetches ordinary source chunks. Its explicit
        // Edit binding must survive role-preference changes for those requests.
        assert_eq!(
            bridge
                .call_chunk("getSourceRangeBatchCompactChunk", json!({}))
                .unwrap()
                .chunk,
            "edit"
        );
        // A runtime command chooses Play even while the Edit pin stands.
        assert_eq!(
            bridge.call("getGuiBounds", json!({})).unwrap()["runtime"],
            "play"
        );
        // A play-runtime request keeps the shared pins; a new runtime
        // operation re-selects and still chooses Play.
        let _play =
            crate::automation::runtime::select_bridge_context_for_request(&context, &bridge, true);
        assert_eq!(
            bridge.call("commitEditorTransaction", json!({})).unwrap()["runtime"],
            "export-edit"
        );
        {
            let mut other = context.clone();
            other.runtime_id = Some("other-place-edit".into());
            let _other = crate::automation::runtime::select_bridge_context_for_request(
                &other, &bridge, true,
            );
            assert_eq!(
                bridge
                    .runtime_pins
                    .lock()
                    .unwrap()
                    .get(&BridgeServer::runtime_pin_key(BridgeTarget::Edit, None))
                    .unwrap()
                    .runtime_id,
                "export-edit",
                "a concurrent play request from another place must not replace the Edit pin"
            );
        }
        let _command = crate::automation::runtime::select_bridge_context(&context, &bridge);
        assert_eq!(
            bridge.call("getGuiBounds", json!({})).unwrap()["runtime"],
            "play"
        );
        assert_eq!(
            bridge.call("commitEditorTransaction", json!({})).unwrap()["runtime"],
            "export-edit"
        );
        let channel = &bridge.channels[1];
        let mut sockets = channel.sockets.lock().unwrap();
        for (_, connection) in sockets.drain() {
            connection.close();
        }
        BridgeServer::refresh_channel_snapshots(channel, &sockets);
        drop(sockets);
        play_worker.join().unwrap();
        assert_eq!(
            bridge.call("finishEditorBinaryExport", json!({})).unwrap()["runtime"],
            "export-edit"
        );
    }
    drop(bridge);
    edit_worker.join().unwrap();
}

#[test]
fn parallel_status_and_edit_profiling_leave_foreground_routing_untouched() {
    use crate::automation::authorization::signed_fixture_request;
    use crate::automation::{State, runtime::automation_parse_response_with_lease};
    struct Project(std::path::PathBuf);
    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let project = Project(std::env::temp_dir().join(format!(
            "renium-bridge-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir_all(project.0.join("src")).unwrap();
    std::fs::write(
        project.0.join("renium.project.jsonc"),
        r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
    )
    .unwrap();
    let bridge = Arc::new(listening_fixture());
    let state = State::default();
    let mut contexts = Vec::new();
    let mut responders = Vec::new();
    for place in 1..=3 {
        let runtime = format!("status-{place}");
        for port in 0..2 {
            responders.push(respond(
                handshake(&bridge, port, &edit_info(&runtime, place)).unwrap(),
                json!({"runtimeId":runtime, "placeId": place, "playRunning": false, "playStarting": false}),
            ));
        }
        let response = automation_parse_response_with_lease(
            &signed_fixture_request(
                &state,
                json!({
                    "v": 1, "id": place, "op": 1,
                    "p": {"root": project.0, "place": format!("10:{place}")}
                }),
            ),
            &state,
            &bridge,
            0.1,
            None,
        );
        assert_eq!(
            response.ok,
            1,
            "{}",
            serde_json::to_string(&response).unwrap()
        );
        contexts.push((place, response.r.unwrap()["id"].as_u64().unwrap()));
    }
    bridge.pin_runtime(BridgeTarget::Edit, "foreground-operation");
    let gate = bridge.acquire_request_gate();
    let (completed, completions) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        for (place, context) in contexts {
            let bridge = &bridge;
            let state = &state;
            let completed = completed.clone();
            scope.spawn(move || {
                for id in 10..110 {
                    let response = automation_parse_response_with_lease(
                        &signed_fixture_request(
                            state,
                            json!({
                                "v": 1, "id": id, "op": 51, "cx": context, "p": {}
                            }),
                        ),
                        state,
                        bridge,
                        0.1,
                        None,
                    );
                    assert_eq!(
                        response.ok,
                        1,
                        "{}",
                        serde_json::to_string(&response).unwrap()
                    );
                    let result = response.r.unwrap();
                    assert_eq!(result["selected"], format!("status-{place}"));
                    assert_eq!(result["studioState"]["placeId"], place, "{result}");
                    assert_eq!(result["playState"], "stopped");
                    let response = automation_parse_response_with_lease(
                        &signed_fixture_request(
                            state,
                            json!({
                                "v":1, "id":id+400, "op":14, "cx":context,
                                "p":{"manageFiles":false,"compact":true,"eventWaitSeconds":0.01}
                            }),
                        ),
                        state,
                        bridge,
                        0.1,
                        None,
                    );
                    assert_eq!(
                        response.ok,
                        1,
                        "{}",
                        serde_json::to_string(&response).unwrap()
                    );
                    assert_eq!(response.r.unwrap()["runtimeId"], format!("status-{place}"));
                    completed.send(false).unwrap();
                }
                for (id, action) in ["snapshot", "micro-start", "micro", "micro-stop"]
                    .into_iter()
                    .enumerate()
                {
                    let response = automation_parse_response_with_lease(
                        &signed_fixture_request(
                            state,
                            json!({
                                "v":1, "id":id+200, "op":101, "cx":context,
                                "p":{"action":action}
                            }),
                        ),
                        state,
                        bridge,
                        0.1,
                        None,
                    );
                    assert_eq!(
                        response.ok,
                        1,
                        "{}",
                        serde_json::to_string(&response).unwrap()
                    );
                    assert_eq!(response.r.unwrap()["runtimeId"], format!("status-{place}"));
                    completed.send(false).unwrap();
                }
                // Both fail before HTTP: invalid scope, then a mock Studio
                // without a creator ID. Neither may change another request's target.
                for (scope_name, code) in [("invalid", "bad_req"), ("user", "unsupported")] {
                    let response = automation_parse_response_with_lease(
                        &signed_fixture_request(
                            state,
                            json!({
                                "v":1, "id":900, "op":91, "cx":context,
                                "p":{"scope":scope_name}
                            }),
                        ),
                        state,
                        bridge,
                        0.1,
                        None,
                    );
                    assert_eq!(response.ok, 0);
                    assert_eq!(response.e.unwrap().c, code);
                    completed.send(false).unwrap();
                }
                completed.send(true).unwrap();
            });
        }
        // Release before joining even on regression: the test fails instead of
        // deadlocking forever behind the intentionally held mutation gate.
        // This proves that requests progress while the gate is held, not that
        // hundreds of debug-build requests finish within one throughput budget.
        let mut remaining = 3;
        while remaining > 0 {
            match completions.recv_timeout(Duration::from_secs(3)) {
                Ok(true) => remaining -= 1,
                Ok(false) => {}
                Err(_) => break,
            }
        }
        drop(gate);
        assert_eq!(remaining, 0, "diagnostics waited for the active mutation");
    });
    assert_eq!(
        bridge.runtime_pins.lock().unwrap()
            [&BridgeServer::runtime_pin_key(BridgeTarget::Edit, None)]
            .runtime_id,
        "foreground-operation"
    );
    drop(bridge);
    for responder in responders {
        responder.join().unwrap();
    }
}

#[test]
fn live_pull_acknowledgments_stay_on_origin_runtime() {
    let bridge = listening_fixture();
    let mut responders = Vec::new();
    for place in 1..=2 {
        let runtime = format!("ack-{place}");
        responders.push(respond(
            handshake(&bridge, 0, &edit_info(&runtime, place)).unwrap(),
            json!({"runtimeId":runtime}),
        ));
    }
    for cycle in 0..100 {
        // File publication releases the mutation gate before acknowledging.
        // Another place can own the foreground selection by this point.
        let foreground = format!("ack-{}", cycle % 2 + 1);
        let origin = format!("ack-{}", (cycle + 1) % 2 + 1);
        bridge.pin_runtime(BridgeTarget::Edit, &foreground);
        let result = crate::automation::runtime::acknowledge_pulled_changes(
            &bridge,
            &["ServerStorage".into()],
            cycle,
            &origin,
        )
        .unwrap();
        assert_eq!(result["runtimeId"], origin);
        assert_eq!(
            bridge
                .runtime_pin_for_selector(BridgeTarget::Edit, None)
                .unwrap()
                .runtime_id,
            foreground
        );
    }
    drop(bridge);
    for responder in responders {
        responder.join().unwrap();
    }
}

#[test]
fn launch_filter_preserves_other_places_manual_play_and_reordered_observations() {
    let bridge = fixture();
    let current = client_info("current", "current-launch");
    let mut other = client_info("other-place", "other-launch");
    other.launch_edit_runtime_id = "other-editor".into();
    let manual = client_info("manual-client", "");
    let mut routing = bridge.routing.lock().unwrap();
    routing.observe_launch("edit-dte", 12, "current-launch");
    routing.observe_launch("edit-dte", 11, "late-old-status");
    routing.observe_launch("edit-dte", 0, "late-old-handshake");
    assert!(routing.allows(&current));
    assert!(routing.allows(&other));
    assert!(routing.allows(&manual));
    assert!(!routing.allows(&client_info("previous", "old-launch")));
    drop(routing);
    bridge.retire_runtime(&manual.runtime_id);
    assert!(!bridge.runtime_is_routable(&manual));
    assert!(bridge.runtime_is_routable(&current));
}

#[test]
fn a_request_for_both_gates_is_not_kept_out_by_its_studios_brief_requests() {
    let bridge = Arc::new(fixture());
    let stop = Arc::new(AtomicBool::new(false));
    let brief_requests = {
        let (bridge, stop) = (Arc::clone(&bridge), Arc::clone(&stop));
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match bridge.try_acquire_runtime_gate("fair-a", "luau") {
                    Some(_gate) => thread::sleep(Duration::from_millis(40)),
                    None => thread::sleep(Duration::from_millis(1)),
                }
            }
        })
    };
    let another_studio = {
        let (bridge, stop) = (Arc::clone(&bridge), Arc::clone(&stop));
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let gate = bridge.acquire_request_gate();
                thread::sleep(Duration::from_millis(40));
                drop(gate);
                thread::sleep(Duration::from_millis(40));
            }
        })
    };
    thread::sleep(Duration::from_millis(60));
    let (entered, waited) = std::sync::mpsc::channel();
    let queued = {
        let bridge = Arc::clone(&bridge);
        thread::spawn(move || {
            let started = Instant::now();
            let gates = bridge.acquire_edit_gates("fair-a", "push", None).unwrap();
            let _ = entered.send(started.elapsed());
            drop(gates);
        })
    };
    let waited = waited.recv_timeout(Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    brief_requests.join().unwrap();
    another_studio.join().unwrap();
    let waited = waited.expect("the queued request never got both gates");
    queued.join().unwrap();
    assert!(waited < Duration::from_secs(1), "waited {waited:?}");
    assert!(
        bridge.try_acquire_runtime_gate("fair-a", "luau").is_some(),
        "the claim outlived the request"
    );
}
