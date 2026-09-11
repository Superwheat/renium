use super::*;

fn fixture() -> BridgeServer {
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
        alive: Arc::new(AtomicBool::new(true)),
        next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        preferred_index: Default::default(),
        request_gate: Mutex::new(()),
        active_request_leases: Mutex::new(HashMap::new()),
        runtime_pins: Mutex::new(HashMap::new()),
        routing: Default::default(),
        final_console_snapshots: Mutex::new(HashMap::new()),
        desired_device_request: Default::default(),
        performance_manager: None,
    }
}

#[test]
fn parallel_transfer_workers_keep_parent_lease_ownership_and_cancellation() {
    let bridge = fixture();
    let lease = Arc::new(BridgeRequestLease::new("native-transfer".into()));
    let parent = bridge.activate_request_lease(Arc::clone(&lease)).unwrap();
    thread::scope(|scope| {
        for _ in 0..4 {
            let bridge = &bridge;
            let lease = Arc::clone(&lease);
            scope.spawn(move || {
                for _ in 0..20 {
                    let inherited = bridge.inherit_request_lease(Arc::clone(&lease)).unwrap();
                    assert!(Arc::ptr_eq(&bridge.active_request_lease().unwrap(), &lease));
                    assert!(bridge.inherit_request_lease(Arc::clone(&lease)).is_err());
                    drop(inherited);
                    assert!(bridge.active_request_lease().is_none());
                }
            });
        }
    });
    assert_eq!(bridge.active_request_leases.lock().unwrap().len(), 1);
    assert!(
        lease.cancel(),
        "worker cleanup must not disarm the parent lease"
    );
    thread::scope(|scope| {
        scope.spawn(|| assert!(bridge.inherit_request_lease(Arc::clone(&lease)).is_err()));
    });
    drop(parent);
    assert!(bridge.active_request_leases.lock().unwrap().is_empty());
}

#[test]
fn bound_local_play_clients_ignore_mutable_names_but_not_ownership() {
    // The production selection is process-global; isolate it from parallel tests.
    if std::env::var_os("RENIUM_TEST_BOUND_LOCAL_CLIENT").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "studio::bridge::routing_tests::bound_local_play_clients_ignore_mutable_names_but_not_ownership"])
            .env("RENIUM_TEST_BOUND_LOCAL_CLIENT", "1").status().unwrap();
        assert!(status.success());
        return;
    }
    let bridge = fixture();
    let _selection = crate::app::context::select_automation(
        Some("edit-dte".into()),
        ".".into(),
        Some("LocalNetworkTest.rbxl".into()),
    );
    let mut info = client_info("client-one", "launch");
    info.place_name = "Place1".into();
    info.player_name = "Player1".into();
    let _first = connect(&bridge, 0, info.clone());
    info.runtime_id = "unrelated-client".into();
    info.launch_edit_runtime_id = "another-edit".into();
    info.place_name = "LocalNetworkTest.rbxl".into();
    let _other = connect(&bridge, 1, info);
    assert_eq!(bridge.player_runtime_ids(), ["client-one"]);
    assert_eq!(
        bridge
            .runtime_pin_for_selector(BridgeTarget::Client, Some("1"))
            .unwrap()
            .runtime_id,
        "client-one"
    );
    assert_eq!(
        bridge.channel_count_for_selector(BridgeTarget::Client, Some("Player1")),
        1
    );
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

#[test]
fn staged_selection_restores_outer_place_and_project_after_success_or_error() {
    if std::env::var_os("RENIUM_TEST_NESTED_SELECTION").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "studio::bridge::routing_tests::staged_selection_restores_outer_place_and_project_after_success_or_error"])
            .env("RENIUM_TEST_NESTED_SELECTION", "1").status().unwrap();
        assert!(status.success());
        return;
    }
    let bridge = fixture();
    let context = crate::automation::BoundContext {
        id: 1,
        initialized: true,
        project: "dte/renium.project.jsonc".into(),
        root: "dte".into(),
        experience: String::new(),
        source: "dte/src".into(),
        resource_lease: None,
        place_id: Some(20),
        game_id: Some(10),
        selector: "10:20".into(),
        runtime_id: Some("edit-dte".into()),
        plugin_build: None,
        fingerprint: String::new(),
    };
    let mut peers = Vec::new();
    for (runtime, id, name) in [
        ("edit-dte", 20, "DTE"),
        ("edit-other", 30, "Baseplate"),
        ("edit-test", 0, "NetworkTest"),
    ] {
        peers.push(connect(
            &bridge,
            0,
            BridgeInfoPayload {
                runtime_id: runtime.into(),
                place_id: Some(id),
                game_id: Some(10),
                place_name: name.into(),
                bridge_role: BRIDGE_ROLE_EDIT.into(),
                ..Default::default()
            },
        ));
    }
    let selection = crate::automation::context::select(&context);
    for fails in [false, true] {
        let mut staged = context.clone();
        staged.project = "dte/staging/renium.project.jsonc".into();
        let result = (|| -> Result<()> {
            let _stage = crate::automation::context::select(&staged);
            assert_eq!(
                crate::app::context::project_override(),
                Some(staged.project.into())
            );
            if fails {
                bail!("staged push failed")
            }
            Ok(())
        })();
        assert_eq!(result.is_err(), fails);
        assert_eq!(
            crate::app::context::automation_runtime(),
            context.runtime_id
        );
        assert_eq!(
            crate::app::context::place_selector(),
            Some(context.selector.clone())
        );
        assert_eq!(
            crate::app::context::project_override(),
            Some(context.project.clone().into())
        );
        let mut sockets = bridge.channels[0].sockets.lock().unwrap();
        bridge
            .ensure_place_unambiguous(&mut sockets, BridgeTarget::Main, None)
            .unwrap();
        assert_eq!(
            bridge.distinct_places_for_selector(&sockets, BridgeTarget::Main, None),
            [(Some(20), "DTE".into())]
        );
    }
    drop(selection);
    assert_eq!(crate::app::context::automation_runtime(), None);
    assert_eq!(crate::app::context::place_selector(), None);
    assert_eq!(crate::app::context::project_override(), None);
}

// Only loopback sockets. No daemon, native watchers, Studio, or game mutations.
fn connect(bridge: &BridgeServer, channel_index: usize, info: BridgeInfoPayload) -> TcpStream {
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
            pending_final_console_snapshots: Vec::new(),
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
fn inventory_does_not_wait_for_an_unnamed_client_and_accepts_its_later_identity() {
    let bridge = Arc::new(fixture());
    let mut info = client_info("loading-client", "launch");
    info.player_name.clear();
    info.player_user_id = None;
    let stream = connect(&bridge, 0, info);
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let closer = stream.try_clone().unwrap();
    let worker_bridge = Arc::clone(&bridge);
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || sender.send(worker_bridge.list_bridge_clients()).unwrap());
    let inventory = receiver.recv_timeout(Duration::from_millis(500));
    if inventory.is_err() {
        let _ = closer.shutdown(std::net::Shutdown::Both);
        worker.join().unwrap();
        panic!("inventory blocked waiting for player metadata");
    }
    worker.join().unwrap();
    assert_eq!(inventory.unwrap().len(), 1);
    let mut peer = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None);
    let request = loop {
        if let Message::Text(text) = peer.read().unwrap() {
            break serde_json::from_str::<Value>(&text).unwrap();
        }
    };
    assert_eq!(request["method"], "getBridgeInfo");
    let next_id = bridge.next_id.load(Ordering::Relaxed);
    for _ in 0..100 {
        assert_eq!(bridge.list_bridge_clients().len(), 1);
    }
    assert_eq!(bridge.next_id.load(Ordering::Relaxed), next_id);
    {
        let mut sockets = bridge.channels[0].sockets.lock().unwrap();
        let socket = sockets.values_mut().next().unwrap();
        let mut socket = socket.io.lock().unwrap();
        for (field, wrong) in [
            ("runtimeId", "old-client"),
            ("bridgeRole", "edit"),
            ("launchNonce", "old-launch"),
            ("launchEditRuntimeId", "other-editor"),
        ] {
            let mut result = json!({"runtimeId": "loading-client", "bridgeRole": "play-client", "launchNonce": "launch", "launchEditRuntimeId": "edit-dte", "playerName": "wrong"});
            result[field] = json!(wrong);
            assert!(BridgeServer::capture_socket_notification(
                &mut socket,
                &json!({"id": request["id"], "ok": true, "result": result})
            ));
            assert!(socket.bridge_info.player_name.is_empty());
        }
        socket.pending_player_identity.as_mut().unwrap().1 -= BRIDGE_QUICK_SOCKET_ATTEMPT_TIMEOUT;
    }
    bridge.list_bridge_clients();
    // A slow response remains valid even after the next observation requests it again.
    assert_eq!(bridge.next_id.load(Ordering::Relaxed), next_id);
    peer.send(Message::Text(
        json!({"id": request["id"], "ok": true, "result": {
            "runtimeId": "loading-client", "bridgeRole": "play-client",
            "launchNonce": "launch", "launchEditRuntimeId": "edit-dte",
            "playerName": "The_SirMeme", "playerUserId": 1176961298
        }})
        .to_string()
        .into(),
    ))
    .unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let clients = bridge.list_bridge_clients();
        if clients[0]["playerName"] == "The_SirMeme" {
            assert_eq!(clients[0]["playerUserId"], 1176961298_i64);
            break;
        }
        assert!(Instant::now() < deadline, "late identity was not consumed");
        thread::yield_now();
    }
}

#[test]
fn retirement_excludes_a_busy_channel_from_player_selection_and_inventory() {
    let bridge = fixture();
    let _old = connect(&bridge, 1, client_info("09-old-client", "old-launch"));
    let _current = connect(
        &bridge,
        0,
        client_info("f2-current-client", "current-launch"),
    );
    let busy = bridge.channels[1].sockets.lock().unwrap();
    bridge.retire_runtime("09-old-client");
    assert_eq!(bridge.player_runtime_ids(), ["f2-current-client"]);
    assert_eq!(
        bridge
            .runtime_pin_for_selector(BridgeTarget::Client, Some("1"))
            .unwrap()
            .runtime_id,
        "f2-current-client"
    );
    assert!(
        bridge
            .list_bridge_clients()
            .iter()
            .all(|entry| entry["runtimeId"] != "09-old-client")
    );
    drop(busy);
}

#[test]
fn unresponsive_peer_cannot_extend_request_budget_during_send() {
    let bridge = Arc::new(fixture());
    let peer = connect(&bridge, 0, client_info("client", "launch"));
    let worker_bridge = Arc::clone(&bridge);
    let (sender, receiver) = std::sync::mpsc::channel();
    let (ready, prepared) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let params = json!({"probe": true});
        let mut sockets = worker_bridge.channels[0].sockets.lock().unwrap();
        let socket = sockets.values_mut().next().unwrap();
        let mut socket = socket.io.lock().unwrap();
        // Establish backpressure before timing the request. Large debug-build
        // JSON serialization would test CPU scheduling rather than blocked I/O.
        socket.socket.get_mut().set_nonblocking(true).unwrap();
        let mut blocked = false;
        for _ in 0..256 {
            match socket
                .socket
                .send(Message::Binary(vec![0; 64 * 1024].into()))
            {
                Ok(()) => {}
                Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                    blocked = true;
                    break;
                }
                Err(error) => panic!("could not establish socket backpressure: {error}"),
            }
        }
        assert!(blocked, "test peer's socket buffer never filled");
        socket.socket.get_mut().set_nonblocking(false).unwrap();
        ready.send(()).unwrap();
        let started = Instant::now();
        let result = BridgeServer::call_on_socket_with_timeout(
            &mut socket,
            10,
            "probe",
            &params,
            Some(Duration::from_millis(50)),
            None,
        );
        sender.send((result.is_err(), started.elapsed())).unwrap();
    });
    // Payload construction and worker scheduling are fixture setup, not I/O.
    prepared.recv_timeout(Duration::from_secs(3)).unwrap();
    let result = receiver.recv_timeout(Duration::from_millis(750));
    let _ = peer.shutdown(Shutdown::Both);
    worker.join().unwrap();
    let (failed, elapsed) = result.unwrap();
    assert!(failed, "unresponsive peer must time out ({elapsed:?})");
}

#[test]
fn an_expired_send_budget_writes_no_request() {
    let bridge = fixture();
    let peer = connect(&bridge, 0, edit_info("expired", 1));
    let connection = bridge.channels[0]
        .sockets
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let mut socket = connection.io.lock().unwrap();
    let error = BridgeServer::call_on_socket_with_timeout(
        &mut socket,
        1,
        "mutation",
        &json!({}),
        Some(Duration::from_nanos(1)),
        None,
    )
    .unwrap_err();
    assert!(
        error.downcast_ref::<BridgeResponseTimeout>().is_some(),
        "{error:#}"
    );
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.peek(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
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
            listener.set_nonblocking(true).unwrap();
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
    let stream = TcpStream::connect(&address).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (mut peer, _) = tungstenite::client(format!("ws://{address}"), stream).unwrap();
    let request: Value = serde_json::from_str(&peer.read().unwrap().into_text().unwrap()).unwrap();
    peer.send(Message::Text(json!({"id": request["id"], "ok": true, "result": {
        "runtimeId": info.runtime_id, "bridgeRole": info.bridge_role,
        "launchNonce": info.launch_nonce, "launchEditRuntimeId": info.launch_edit_runtime_id,
        "playerName": info.player_name, "playerUserId": info.player_user_id,
        "gameId": info.game_id, "placeId": info.place_id, "placeName": info.place_name,
        "protocolVersion": "compact-v5", "codecVersion": "compact-v5-schema-9",
        "chunkFrameProtocolVersion": "rbs2", "compactValueProtocolVersion": "compact-v5-schema-4",
        "registrationAck": true
    }}).to_string().into())).unwrap();
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

#[cfg(windows)]
#[test]
fn native_connection_preparation_does_not_block_registration_or_other_places() {
    let (entered, entries) = std::sync::mpsc::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    let resumed = Mutex::new(resumed);
    let first = AtomicBool::new(true);
    let preparation = Arc::new(NativeConnectionPreparation {
        pending: Default::default(),
        prepare: Box::new(move |pid, title| {
            assert_eq!(pid, std::process::id());
            entered.send(title.to_owned()).unwrap();
            if title == "place-1" && first.swap(false, Ordering::SeqCst) {
                resumed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                bail!("fixture discovery failure");
            }
            Ok(())
        }),
    });
    let bridge = listening_fixture_with_preparation(Some(Arc::clone(&preparation)));
    let _first_peer = handshake(&bridge, 0, &edit_info("native-one", 1)).unwrap();
    assert_eq!(
        entries.recv_timeout(Duration::from_secs(1)).unwrap(),
        "place-1"
    );
    // A second channel joins the same runtime while its native scan is blocked.
    let _same = handshake(&bridge, 1, &edit_info("native-one", 1)).unwrap();
    let other = handshake(&bridge, 0, &edit_info("native-two", 2)).unwrap();
    assert_eq!(
        entries.recv_timeout(Duration::from_secs(1)).unwrap(),
        "place-2"
    );
    let responder = respond(other, json!({"working": true}));
    assert_eq!(
        bridge
            .call_for_runtime_with_timeout(
                "getStudioState",
                json!({}),
                BridgeTarget::Edit,
                "native-two",
                Some(Duration::from_secs(1))
            )
            .unwrap(),
        json!({"working": true})
    );
    let _play = handshake(&bridge, 1, &client_info("native-play", "manual")).unwrap();
    assert!(
        entries.try_recv().is_err(),
        "play or duplicate channel started native discovery"
    );
    resume.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while preparation.pending.lock().unwrap().contains("native-one") {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let _reconnect = handshake(&bridge, 0, &edit_info("native-one", 1)).unwrap();
    assert_eq!(
        entries.recv_timeout(Duration::from_secs(1)).unwrap(),
        "place-1",
        "failed preparation prevented reconnect discovery"
    );
    bridge.alive.store(false, Ordering::SeqCst);
    for channel in &bridge.channels {
        for connection in channel.sockets.lock().unwrap().values() {
            preparation.run(connection, &bridge.alive);
            connection.close();
        }
    }
    assert!(
        entries.try_recv().is_err(),
        "stopped bridge started native discovery"
    );
    responder.join().unwrap();
}

fn edit_info(runtime: &str, place: i64) -> BridgeInfoPayload {
    BridgeInfoPayload {
        runtime_id: runtime.into(),
        bridge_role: BRIDGE_ROLE_EDIT.into(),
        game_id: Some(10),
        place_id: Some(place),
        place_name: format!("place-{place}"),
        protocol_version: "compact-v5".into(),
        codec_version: "compact-v5-schema-9".into(),
        chunk_frame_protocol_version: "rbs2".into(),
        compact_value_protocol_version: "compact-v5-schema-4".into(),
        ..Default::default()
    }
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn cached_runtime_metadata_is_exact_and_available_during_commands() {
    let bridge = fixture();
    let _first = connect(&bridge, 0, edit_info("metadata-one", 1));
    let _second = connect(&bridge, 1, edit_info("metadata-two", 2));
    let channel = &bridge.channels[0];
    let connection = channel
        .sockets
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    let busy = connection.io.lock().unwrap();
    for (runtime, name) in [("metadata-one", "place-1"), ("metadata-two", "place-2")] {
        assert_eq!(
            bridge
                .cached_bridge_info_for_runtime(BridgeTarget::Edit, runtime)
                .unwrap()
                .place_name,
            name
        );
    }
    assert!(
        bridge
            .cached_bridge_info_for_runtime(BridgeTarget::Edit, "missing")
            .is_err()
    );
    assert!(
        bridge
            .cached_bridge_info_for_runtime(BridgeTarget::Client, "metadata-one")
            .is_err()
    );
    drop(busy);
    let registry = channel.sockets.lock().unwrap();
    std::thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::channel();
        let bridge = &bridge;
        scope.spawn(move || {
            send.send(bridge.cached_bridge_info_for_runtime(BridgeTarget::Edit, "metadata-one"))
                .unwrap();
        });
        assert!(
            receive.recv_timeout(Duration::from_millis(20)).is_err(),
            "A short registry update must not report a missing Studio runtime"
        );
        drop(registry);
        assert_eq!(
            receive
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .place_name,
            "place-1"
        );
    });
    bridge.retire_runtime("metadata-one");
    assert!(
        bridge
            .cached_bridge_info_for_runtime(BridgeTarget::Edit, "metadata-one")
            .is_err()
    );
    assert!(
        bridge
            .cached_bridge_info_for_runtime(BridgeTarget::Edit, "metadata-two")
            .is_ok()
    );
    for socket in bridge.channels[1].sockets.lock().unwrap().values() {
        socket.close();
    }
    assert!(
        bridge
            .cached_bridge_info_for_runtime(BridgeTarget::Edit, "metadata-two")
            .is_err()
    );
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
        bridge.pin_runtime(BridgeTarget::Main, "export-edit");
        bridge.pin_runtime(BridgeTarget::Edit, "export-edit");
        for method in [
            "beginEditorBinaryExport",
            "awaitEditorBinaryExport",
            "finishEditorBinaryExport",
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
                .call_chunk("getSourceBatchChunk", json!({}))
                .unwrap()
                .chunk,
            "edit"
        );
        // A new runtime operation, without that binding, still chooses Play.
        let _command = crate::automation::runtime::select_bridge_context(&context, &bridge);
        assert_eq!(
            bridge.call("getGuiBounds", json!({})).unwrap()["runtime"],
            "play"
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
fn native_export_without_edit_does_not_fall_back_to_a_play_server() {
    let bridge = fixture();
    let mut info = edit_info("play-only", 20);
    info.bridge_role = BRIDGE_ROLE_PLAY_SERVER.into();
    let peer = connect(&bridge, 0, info);
    for method in ["beginEditorBinaryExport", "finishEditorBinaryExport"] {
        assert!(
            bridge
                .call(method, json!({}))
                .unwrap_err()
                .to_string()
                .contains("edit")
        );
    }
    for method in [
        "readEditorBinaryExport",
        "readEditorBinaryExportBatch",
        "getEditorBinaryOverlayChunk",
    ] {
        assert!(bridge.call_chunk(method, json!({})).is_err());
    }
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.peek(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

// Keep a real request in flight until the test releases it. The channel handshake
// and response-ID handling are production code, not simulated routing outcomes.
fn hold_request(
    bridge: &Arc<BridgeServer>,
    mut peer: WebSocket<TcpStream>,
    runtime: &str,
) -> (
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<Result<Value>>,
    thread::JoinHandle<()>,
) {
    let (received, wait_received) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel();
    let responder = thread::spawn(move || {
        while let Ok(message) = peer.read() {
            if let Message::Text(text) = message {
                let request: Value = serde_json::from_str(&text).unwrap();
                received.send(()).unwrap();
                let _ = wait_release.recv_timeout(Duration::from_secs(5));
                let _ = peer.send(Message::Text(
                    json!({
                        "id": request["id"], "ok": true, "result": {"completed": true}
                    })
                    .to_string()
                    .into(),
                ));
                break;
            }
        }
    });
    let bridge = Arc::clone(bridge);
    let runtime = runtime.to_string();
    let caller = thread::spawn(move || {
        bridge.call_for_runtime_with_timeout(
            "getStudioChangeState",
            json!({"waitSeconds": 25}),
            BridgeTarget::Edit,
            &runtime,
            Some(Duration::from_secs(5)),
        )
    });
    wait_received.recv_timeout(Duration::from_secs(1)).unwrap();
    (release, caller, responder)
}

#[test]
fn busy_places_do_not_block_other_places_registration_readiness_or_requests() {
    let bridge = Arc::new(listening_fixture());
    let a = hold_request(
        &bridge,
        handshake(&bridge, 0, &edit_info("a", 1)).unwrap(),
        "a",
    );
    let b = hold_request(
        &bridge,
        handshake(&bridge, 1, &edit_info("b", 2)).unwrap(),
        "b",
    );
    let started = Instant::now();
    let mut responders = Vec::new();
    for port in 0..2 {
        responders.push(respond(
            handshake(&bridge, port, &edit_info("c", 3)).unwrap(),
            json!({"runtime": "c"}),
        ));
    }
    // Exercise inventory and readiness with both listening ports serving long polls.
    assert_eq!(bridge.list_bridge_clients().len(), 3);
    assert_eq!(
        bridge.max_runtime_channel_coverage(BridgeTarget::Edit, None),
        2
    );
    assert!(
        bridge
            .missing_ports_for_target(BridgeTarget::Edit)
            .is_empty()
    );
    for _ in 0..100 {
        let result = bridge
            .call_for_runtime_with_timeout(
                "getStudioState",
                json!({}),
                BridgeTarget::Edit,
                "c",
                Some(Duration::from_millis(500)),
            )
            .unwrap();
        assert_eq!(result["runtime"], "c");
    }
    let elapsed = started.elapsed();
    for (release, caller, responder) in [a, b] {
        release.send(()).unwrap();
        assert_eq!(caller.join().unwrap().unwrap()["completed"], true);
        responder.join().unwrap();
    }
    drop(bridge);
    for responder in responders {
        responder.join().unwrap();
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "independent place stalled: {elapsed:?}"
    );
}

#[test]
fn cancelled_observers_release_occupied_channels_without_a_spare_socket() {
    let bridge = Arc::new(fixture());
    let (received, wait_received) = std::sync::mpsc::channel();
    let mut responders = Vec::new();
    for channel in 0..2 {
        let stream = connect(&bridge, channel, edit_info("observer", 1));
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut peer =
            WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Client, None);
        let received = received.clone();
        responders.push(thread::spawn(move || {
            for cycle in 0..30 {
                let request: Value =
                    serde_json::from_str(peer.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(request["method"], "getStudioChangeState");
                received.send(()).unwrap();
                let cancel: Value =
                    serde_json::from_str(peer.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(cancel["method"], "cancelRequestLease");
                assert_eq!(cancel["params"]["leaseId"], request["lease_id"]);
                let original =
                    json!({"id":request["id"],"ok":true,"result":{"waitCancelled":true}});
                let ack = json!({"id":cancel["id"],"ok":true,"result":{"ok":true}});
                let responses = if cycle % 2 == 0 {
                    [original, ack]
                } else {
                    [ack, original]
                };
                for response in responses {
                    peer.send(Message::Text(response.to_string().into()))
                        .unwrap();
                }
            }
        }));
    }
    for cycle in 0..30 {
        let mut callers = Vec::new();
        for index in 0..2 {
            let lease = Arc::new(BridgeRequestLease::new(format!("observer-{cycle}-{index}")));
            let active = Arc::clone(&lease);
            let bridge = Arc::clone(&bridge);
            let caller = thread::spawn(move || {
                let _lease = bridge.activate_request_lease(active)?;
                bridge.call_for_runtime_with_timeout(
                    "getStudioChangeState",
                    json!({"start":false,"waitSeconds":25}),
                    BridgeTarget::Edit,
                    "observer",
                    Some(Duration::from_secs(2)),
                )
            });
            callers.push((lease, caller));
        }
        for _ in 0..2 {
            wait_received.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        // Stop a third observer before it can acquire either occupied channel.
        let queued_lease = Arc::new(BridgeRequestLease::new(format!("queued-{cycle}")));
        let queued_active = Arc::clone(&queued_lease);
        let queued_bridge = Arc::clone(&bridge);
        let (armed, wait_armed) = std::sync::mpsc::channel();
        let queued = thread::spawn(move || {
            let _lease = queued_bridge.activate_request_lease(queued_active)?;
            armed.send(()).unwrap();
            queued_bridge.call_for_runtime_with_timeout(
                "getStudioChangeState",
                json!({"start":false,"waitSeconds":25}),
                BridgeTarget::Edit,
                "observer",
                Some(Duration::from_secs(2)),
            )
        });
        wait_armed.recv_timeout(Duration::from_secs(2)).unwrap();
        queued_lease.cancel();
        assert!(
            queued
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        let started = Instant::now();
        for (lease, _) in &callers {
            lease.cancel();
        }
        for (lease, caller) in callers {
            assert_eq!(caller.join().unwrap().unwrap()["waitCancelled"], true);
            assert!(
                bridge.activate_request_lease(lease).is_err(),
                "cancelled observer dispatched again"
            );
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(bridge.active_request_leases.lock().unwrap().is_empty());
        for channel in &bridge.channels {
            let sockets = channel.sockets.lock().unwrap();
            for connection in sockets.values() {
                assert!(
                    connection.io.try_lock().is_ok(),
                    "cancel returned before channel release"
                );
            }
        }
    }
    for responder in responders {
        responder.join().unwrap();
    }
}

#[test]
fn busy_runtime_keeps_its_connected_channel_count() {
    let bridge = Arc::new(listening_fixture());
    let a = hold_request(
        &bridge,
        handshake(&bridge, 0, &edit_info("same", 1)).unwrap(),
        "same",
    );
    let b = hold_request(
        &bridge,
        handshake(&bridge, 1, &edit_info("same", 1)).unwrap(),
        "same",
    );
    assert_eq!(
        bridge.max_runtime_channel_coverage(BridgeTarget::Edit, None),
        2
    );
    assert_eq!(bridge.channel_count_for_target(BridgeTarget::Edit), 2);
    assert!(
        bridge
            .missing_ports_for_target(BridgeTarget::Edit)
            .is_empty()
    );
    bridge.wait_for_all_target(1.0, BridgeTarget::Edit).unwrap();
    assert_eq!(
        bridge.wait_for_ready_channels_for_target(2, Duration::ZERO, BridgeTarget::Edit),
        2
    );
    let error = bridge
        .call_for_runtime_with_timeout(
            "getStudioState",
            json!({}),
            BridgeTarget::Edit,
            "same",
            Some(Duration::from_millis(20)),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("channels are busy"), "{error}");
    assert_eq!(
        bridge.list_bridge_clients()[0]["ports"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    for (release, caller, responder) in [a, b] {
        // Prove readiness and inventory did not wait for the requests to finish.
        // A wall-clock threshold also measures unrelated CI scheduler delays.
        assert!(!caller.is_finished());
        assert!(!responder.is_finished());
        release.send(()).unwrap();
        caller.join().unwrap().unwrap();
        responder.join().unwrap();
    }
}

#[test]
fn reconnect_during_an_outstanding_request_cannot_remove_its_replacement() {
    let bridge = Arc::new(listening_fixture());
    let info = edit_info("reconnect", 1);
    for _ in 0..50 {
        let old = hold_request(
            &bridge,
            handshake(&bridge, 0, &info).unwrap(),
            &info.runtime_id,
        );
        let reply = respond(
            handshake(&bridge, 0, &info).unwrap(),
            json!({"replacement": true}),
        );
        let _ = old.0.send(());
        // A transport error may retry the same request ID on the replacement.
        // Regardless of that outcome, it must not delete the replacement socket.
        let _ = old.1.join().unwrap();
        old.2.join().unwrap();
        for _ in 0..3 {
            assert_eq!(
                bridge
                    .call_for_runtime_with_timeout(
                        "getStudioState",
                        json!({}),
                        BridgeTarget::Edit,
                        &info.runtime_id,
                        Some(Duration::from_millis(500)),
                    )
                    .unwrap()["replacement"],
                true
            );
        }
        assert_eq!(bridge.list_bridge_clients().len(), 1);
        let connection = bridge.channels[0]
            .sockets
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        connection.close();
        reply.join().unwrap();
    }
}

#[test]
fn absent_runtime_is_not_reported_as_busy() {
    let bridge = fixture();
    let error = bridge
        .call_for_runtime_with_timeout(
            "getStudioState",
            json!({}),
            BridgeTarget::Edit,
            "missing",
            Some(Duration::from_millis(20)),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("disconnected"), "{error}");
    assert!(!error.contains("busy"), "{error}");
}

#[test]
fn retiring_an_in_flight_connection_does_not_wait_for_the_reconnect_timeout() {
    let bridge = Arc::new(listening_fixture());
    let info = edit_info("retiring", 1);
    let (release, caller, responder) = hold_request(
        &bridge,
        handshake(&bridge, 0, &info).unwrap(),
        &info.runtime_id,
    );
    let started = Instant::now();
    bridge.retire_runtime(&info.runtime_id);
    let _ = release.send(());
    let error = caller.join().unwrap().unwrap_err();
    responder.join().unwrap();
    assert!(bridge.list_bridge_clients().is_empty());
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "retired runtime kept retrying: {error:#}"
    );
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
                }
                completed.send(()).unwrap();
            });
        }
        // Release before joining even on regression: the test fails instead of
        // deadlocking forever behind the intentionally held mutation gate.
        let all_completed =
            (0..3).all(|_| completions.recv_timeout(Duration::from_secs(3)).is_ok());
        drop(gate);
        assert!(all_completed, "diagnostics waited for the active mutation");
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
fn rapid_session_turnover_rejects_late_handshakes_and_routes_both_selectors() {
    let bridge = listening_fixture();
    let mut previous = client_info("09-stale-client", "previous");
    let mut replies = Vec::new();
    let _stale = handshake(&bridge, 1, &previous).unwrap();
    for cycle in 1..=100 {
        let nonce = format!("launch-{cycle}");
        bridge
            .routing
            .lock()
            .unwrap()
            .observe_launch("edit-dte", cycle, &nonce);
        // Old reconnects cannot change the authoritative launch or re-enter the list.
        assert!(handshake(&bridge, cycle as usize % 2, &previous).is_none());
        let current = client_info(&format!("current-{cycle}"), &nonce);
        for port in [cycle as usize % 2, (cycle as usize + 1) % 2] {
            replies.push(respond(
                handshake(&bridge, port, &current).unwrap(),
                json!({"runtimeId": current.runtime_id}),
            ));
        }
        assert_eq!(
            bridge.player_runtime_ids(),
            std::slice::from_ref(&current.runtime_id)
        );
        for selector in ["1", "The_SirMeme"] {
            assert!(bridge.wait_for_ready_player(selector, Duration::from_millis(100)));
            for _ in 0..4 {
                let result = bridge
                    .call_for_selector_with_timeout(
                        "executeLuau",
                        json!({}),
                        BridgeTarget::Client,
                        Some(selector),
                        Some(Duration::from_secs(1)),
                    )
                    .unwrap();
                assert_eq!(result["runtimeId"], current.runtime_id);
            }
        }
        assert_eq!(bridge.list_bridge_clients().len(), 1);
        // One channel remains busy during stop. Both numeric and named routes
        // must become unavailable immediately, even before physical cleanup.
        let busy = bridge.channels[cycle as usize % 2].sockets.lock().unwrap();
        bridge.retire_runtime(&current.runtime_id);
        assert!(bridge.player_runtime_ids().is_empty());
        assert!(!bridge.runtime_is_routable(&current));
        drop(busy);
        assert!(bridge.list_bridge_clients().is_empty());
        previous = current;
    }
    drop(bridge);
    for reply in replies {
        reply.join().unwrap();
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
fn numeric_indices_use_current_sockets_not_removed_snapshot_entries() {
    let bridge = fixture();
    let _old = connect(&bridge, 0, client_info("09-old-client", "old"));
    let _current = connect(&bridge, 1, client_info("f2-current-client", "new"));
    bridge.channels[0].sockets.lock().unwrap().clear();
    assert_eq!(bridge.player_runtime_ids(), ["f2-current-client"]);
}

#[test]
fn reconnect_replaces_one_channel_without_duplicating_multiplayer_indices() {
    let bridge = listening_fixture();
    let mut replies = Vec::new();
    for index in (1..=8).rev() {
        let mut info = client_info(&format!("player-{index}"), "multi");
        info.player_name = format!("Player{index}");
        info.player_user_id = Some(-index);
        for channel in 0..2 {
            replies.push(respond(
                handshake(&bridge, channel, &info).unwrap(),
                json!({"runtimeId": info.runtime_id, "connection": "original"}),
            ));
        }
        // Reconnecting one port replaces a channel, not a player.
        replies.push(respond(
            handshake(&bridge, 0, &info).unwrap(),
            json!({"runtimeId": info.runtime_id, "connection": "reconnected"}),
        ));
    }
    assert_eq!(bridge.list_bridge_clients().len(), 8);
    assert_eq!(bridge.channels[0].sockets.lock().unwrap().len(), 8);
    for index in 1..=8 {
        let numeric = index.to_string();
        let name = format!("Player{index}");
        for selector in [&numeric, &name] {
            let result = bridge
                .call_for_selector_with_timeout(
                    "executeLuau",
                    json!({}),
                    BridgeTarget::Client,
                    Some(selector),
                    Some(Duration::from_secs(1)),
                )
                .unwrap();
            assert_eq!(result["runtimeId"], format!("player-{index}"));
        }
    }
    assert!(
        bridge
            .runtime_pin_for_selector(BridgeTarget::Client, Some("9"))
            .is_err()
    );
    drop(bridge);
    for reply in replies {
        reply.join().unwrap();
    }
}

#[test]
fn edit_status_response_filters_a_reconnected_daemons_previously_captured_inventory() {
    let bridge = fixture();
    let mut info = client_info("edit-dte", "");
    info.bridge_role = BRIDGE_ROLE_EDIT.into();
    let peer = connect(&bridge, 0, info);
    let reply = respond(
        WebSocket::from_raw_socket(peer, tungstenite::protocol::Role::Client, None),
        json!({"ok": true, "playRunning": true, "launchNonce": "current"}),
    );
    let _old = connect(&bridge, 1, client_info("09-old", "previous"));
    let _current = connect(&bridge, 1, client_info("f2-current", "current"));
    let mut inventory = bridge.list_bridge_clients();
    assert_eq!(inventory.len(), 3);
    bridge
        .call_for_runtime_with_timeout(
            "getStudioState",
            json!({}),
            BridgeTarget::Edit,
            "edit-dte",
            Some(Duration::from_secs(1)),
        )
        .unwrap();
    bridge.retain_current_clients(&mut inventory);
    assert_eq!(inventory.len(), 2);
    assert!(inventory.iter().all(|entry| entry["runtimeId"] != "09-old"));
    assert_eq!(bridge.player_runtime_ids(), ["f2-current"]);
    drop(bridge);
    reply.join().unwrap();
}
