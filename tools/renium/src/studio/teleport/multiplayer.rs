//! One server per destination and one native document per traveling client.
use super::routing::{Place, Routes, Server, ServerState};
use super::*;
use crate::studio::automation::test_processes;
use std::collections::BTreeMap;
use std::time::Instant;

struct Endpoint {
    session: Session,
    dispatch: Dispatch,
    last: u64,
    // Studio's local transport belongs to the server, not a player. Retain
    // its native join configuration for the lifetime of this exact endpoint.
    configuration: Option<Value>,
}

struct Test {
    root: Session,
    source_pid: u32,
    source_creation: String,
    edit_pid: u32,
    routes: Routes,
    endpoints: BTreeMap<Place, Endpoint>,
    captions: HashMap<u32, title::ClientCaption>,
}

impl Test {
    fn alive(&self, bridge: &BridgeServer) -> Result<()> {
        ensure!(
            bridge.alive.load(Ordering::Relaxed)
                && self.root.registered()
                && crate::app::update::process_start_identity(self.source_pid).as_deref()
                    == Some(&self.source_creation),
            "The root Server & Clients test ended"
        );
        Ok(())
    }

    fn wait(&self, bridge: &BridgeServer, deadline: Instant) -> Result<()> {
        self.alive(bridge)?;
        ensure!(
            Instant::now() < deadline,
            "Play client did not become ready"
        );
        thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    fn clients(&self, bridge: &BridgeServer, place: Place) -> Result<HashMap<String, String>> {
        let known = self.routes.clients(place);
        let mut clients = HashMap::new();
        for row in bridge.list_bridge_clients().iter().filter(|row| {
            row["role"] == "play-client"
                && row["launchEditRuntimeId"] == self.root.owner
                && row["gameId"] == place.universe
                && row["placeId"] == place.place
        }) {
            let pid = row["pid"].as_u64().context("Client has no process owner")?;
            let runtime = text_field(row, "runtimeId")?;
            if known.get(&pid.to_string()) == Some(&runtime) {
                ensure!(
                    clients
                        .insert(text_field(row, "playerName")?, runtime)
                        .is_none(),
                    "Duplicate player in destination server"
                );
            }
        }
        Ok(clients)
    }

    fn refresh_members(&mut self, bridge: &BridgeServer) -> Result<()> {
        let mut rows = bridge.list_bridge_clients();
        // Studio's manual Add Players path does not inherit Renium test args.
        // Accept new source clients only after proving process ancestry, then
        // publish the same launch identity to every channel before routing.
        if self.root.nonce.starts_with("manual-") {
            let pending = rows
                .iter()
                .filter(|row| {
                    row["role"] == "play-client"
                        && row["launchEditRuntimeId"] == self.root.owner
                        && row["launchNonce"].as_str().is_none_or(str::is_empty)
                        && row["placeId"] == self.root.source
                        && row["gameId"] == self.root.universe
                })
                .collect::<Vec<_>>();
            if !pending.is_empty() {
                let descendants = test_processes::test_descendants(
                    &test_processes::studio_process_table(),
                    self.edit_pid,
                );
                for row in pending {
                    let runtime = text_field(row, "runtimeId")?;
                    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Client, &runtime)?;
                    ensure!(
                        descendants.contains(&pid),
                        "Manual client is not a test process of its Edit window"
                    );
                    automatic::adopt_runtime(
                        bridge,
                        BridgeTarget::Client,
                        &runtime,
                        &self.root.owner,
                        &self.root.nonce,
                        "multi",
                    )?;
                    bridge.refresh_launch_runtime(&runtime, BridgeTarget::Client, pid)?;
                }
                rows = bridge.list_bridge_clients();
            }
        }
        let places = self.endpoints.keys().copied().collect::<Vec<_>>();
        let mut all = HashMap::new();
        for place in places {
            for (pid, runtime) in self.routes.clients(place) {
                if !rows
                    .iter()
                    .any(|row| row["runtimeId"] == runtime && row["role"] == "play-client")
                {
                    self.routes.disconnect(&pid, &runtime);
                    if let Ok(pid) = pid.parse() {
                        self.captions.remove(&pid);
                    }
                } else {
                    all.insert(pid, runtime);
                }
            }
        }
        for row in rows
            .iter()
            .filter(|row| row["role"] == "play-client" && self.root.owns(row))
        {
            let pid = row["pid"]
                .as_u64()
                .context("Client has no process owner")?
                .to_string();
            if !all.contains_key(&pid) {
                self.routes.add_client(pid, text_field(row, "runtimeId")?)?;
            }
        }
        Ok(())
    }

    fn destination(
        &mut self,
        bridge: &BridgeServer,
        travel: &routing::Travel,
        destination: &crate::cloud::teleport::Destination,
        clients: &[u32],
    ) -> Result<Server> {
        self.alive(bridge)?;
        if self.routes.start_server(travel)? {
            let result = (|| -> Result<Server> {
                let bytes = cache::saved_place(destination)?;
                let directory = std::env::temp_dir()
                    .join("renium-native")
                    .join("test-servers");
                std::fs::create_dir_all(&directory)?;
                let file = directory.join(format!(
                    "{}-{}-{}.rbxl",
                    self.source_pid, travel.to.place, destination.saved.version
                ));
                crate::system::files::atomic_write_file(&file, &bytes.bytes)?;
                let _file = OnDrop::new(|| {
                    let _ = std::fs::remove_file(&file);
                });
                let pid = crate::studio::native::serializer::start_destination_server(
                    self.source_pid,
                    self.edit_pid,
                    travel.to.place,
                    travel.to.universe,
                    &file,
                )?;
                for &client in clients {
                    crate::studio::native::serializer::prepare_test_client(client)?;
                }
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let inventory = bridge.list_bridge_clients();
                    if let Some(info) = inventory.iter().find(|row| {
                        row["pid"] == pid
                            && row["role"] == "play-server"
                            && row["placeId"] == travel.to.place
                            && row["gameId"] == travel.to.universe
                    }) {
                        if info["launchNonce"].as_str().is_none_or(str::is_empty) {
                            // Manual Server & Clients has no __renium test args
                            // to inherit. Bind only the exact process returned
                            // by our owned destination launch before using it.
                            let runtime = text_field(info, "runtimeId")?;
                            automatic::adopt_runtime(
                                bridge,
                                BridgeTarget::Server,
                                &runtime,
                                &self.root.owner,
                                &self.root.nonce,
                                "multi",
                            )?;
                            bridge.refresh_launch_runtime(&runtime, BridgeTarget::Server, pid)?;
                            continue;
                        }
                        ensure!(
                            info["launchEditRuntimeId"] == self.root.owner
                                && info["launchNonce"] == self.root.nonce,
                            "Destination test server belongs to another root launch"
                        );
                        let mut session = Session::from_info(info)?;
                        session.source = self.root.source;
                        session.root_nonce.clone_from(&self.root.root_nonce);
                        session.multiplayer = true;
                        call(
                            bridge,
                            &session.server,
                            BridgeTarget::Server,
                            "arm",
                            json!({}),
                        )?;
                        let dispatch = Dispatch::arm(bridge, &session);
                        ensure!(
                            dispatch.ready(),
                            "Destination TeleportService hook did not arm"
                        );
                        let server = Server {
                            runtime: session.server.clone(),
                            nonce: session.nonce.clone(),
                            pid,
                            version: destination.saved.version,
                        };
                        #[cfg(windows)]
                        crate::project::workflows::windows_launch::match_test_server_window(
                            self.source_pid,
                            pid,
                        )?;
                        self.endpoints.insert(
                            travel.to,
                            Endpoint {
                                session,
                                dispatch,
                                last: 0,
                                configuration: None,
                            },
                        );
                        self.routes.server_ready(travel, server.clone())?;
                        return Ok(server);
                    }
                    self.wait(bridge, deadline)?;
                }
            })();
            if result.is_err() {
                self.routes.server_failed(travel);
            }
            return result;
        }
        match self.routes.servers.get(&travel.to) {
            Some(ServerState::Ready(server)) => Ok(server.clone()),
            _ => anyhow::bail!("Destination server is still starting"),
        }
    }

    fn travel(
        &mut self,
        bridge: &BridgeServer,
        from: Place,
        attempt: &Attempt,
        clients: &HashMap<String, String>,
    ) -> Result<()> {
        use crate::studio::native::serializer::test_client_configuration;
        let started = Instant::now();
        let selected: HashMap<_, _> = attempt
            .players
            .iter()
            .map(|name| (name.clone(), clients[name].clone()))
            .collect();
        let mut participants = Vec::new();
        for (name, runtime) in &selected {
            let pid = bridge.studio_pid_for_runtime(BridgeTarget::Client, runtime)?;
            participants.push((name.clone(), runtime.clone(), pid));
        }
        let route = self.routes.reserve(
            from,
            Place {
                universe: from.universe,
                place: attempt.place_id,
            },
            &participants
                .iter()
                .map(|(_, _, pid)| pid.to_string())
                .collect::<Vec<_>>(),
        )?;
        let result = (|| -> Result<()> {
            for (_, _, pid) in &participants {
                if !self.captions.contains_key(pid) {
                    self.captions
                        .insert(*pid, title::ClientCaption::start(*pid)?);
                }
                self.captions.get_mut(pid).unwrap().update(None, true)?;
            }
            // Returning uses the source's existing native transport. Manual
            // Add Clients would launch the entire player-count selection again.
            if self.endpoints[&from].configuration.is_none() && from.place == self.root.source {
                #[cfg(windows)]
                let command = crate::studio::performance::process_command_line(participants[0].2)
                    .context("Cannot read the source client's test configuration")?;
                #[cfg(target_os = "macos")]
                let command =
                    crate::studio::native::serializer::process_arguments(participants[0].2)?;
                self.endpoints.get_mut(&from).unwrap().configuration =
                    Some(test_client_configuration(
                        &command,
                        self.edit_pid,
                        self.source_pid,
                        from.universe,
                        from.place,
                    )?);
            }
            notify_clients(
                bridge,
                &selected,
                "loading",
                &json!({"id":attempt.id,"placeId":attempt.place_id}),
            )?;
            let destination = crate::cloud::teleport::resolve(from.universe, attempt.place_id)?;
            notify_clients(
                bridge,
                &selected,
                "loading",
                &json!({"id":attempt.id,"placeId":attempt.place_id,"displayName":destination.display_name}),
            )?;
            let resolved = Instant::now();
            for (_, _, pid) in &participants {
                self.captions
                    .get_mut(pid)
                    .unwrap()
                    .update(Some(&destination.display_name), true)?;
            }
            let client_pids = participants
                .iter()
                .map(|(_, _, pid)| *pid)
                .collect::<Vec<_>>();
            let server = self.destination(bridge, &route, &destination, &client_pids)?;
            let server_ready = Instant::now();
            self.alive(bridge)?;
            ensure!(
                self.routes.current(&route),
                "Teleport group changed before travel"
            );
            let source = self.endpoints.get(&from).context("Source server ended")?;
            call(
                bridge,
                &source.session.server,
                BridgeTarget::Server,
                "acceptNative",
                json!({"id":attempt.id}),
            )?;
            source.dispatch.wait_accepted(bridge, attempt)?;
            let mut arrived = BTreeMap::new();
            let inventory = bridge.list_bridge_clients();
            let members = participants
                .iter()
                .filter_map(|(_, runtime, _)| {
                    inventory
                        .iter()
                        .find(|row| row["runtimeId"] == *runtime)
                        .and_then(|row| row["playerUserId"].as_i64())
                })
                .collect::<Vec<_>>();
            for (name, runtime, pid) in &participants {
                self.alive(bridge)?;
                let identity = call(
                    bridge,
                    &self.endpoints[&from].session.server,
                    BridgeTarget::Server,
                    "routeIdentity",
                    json!({"playerName":name}),
                )?["identity"]
                    .clone();
                let join = json!({"SourcePlaceId":from.place,"SourceGameId":from.universe,
                    "TeleportData":attempt.data[name]["teleportData"],
                    "Members":members});
                let mut arrivals = Vec::new();
                for (target_pid, target_runtime, role, client) in [
                    (
                        server.pid,
                        server.runtime.as_str(),
                        BridgeTarget::Server,
                        false,
                    ),
                    (*pid, runtime.as_str(), BridgeTarget::Client, true),
                ] {
                    let relay = call(
                        bridge,
                        target_runtime,
                        role,
                        "routeJoin",
                        json!({"join":join,"identity":identity}),
                    )?;
                    let path = serde_json::from_value::<Vec<String>>(relay["path"].clone())?;
                    #[cfg(windows)]
                    let arrival = crate::studio::native::serializer::NativeTestJoin::arm(
                        target_pid,
                        target_runtime,
                        &path,
                        attempt.place_id,
                        from.universe,
                        client,
                        &identity,
                    )?;
                    #[cfg(target_os = "macos")]
                    let arrival = {
                        let mut arrival = crate::studio::native::serializer::NativeTestJoin::arm(
                            target_pid,
                            target_runtime,
                            &path,
                            crate::studio::native::serializer::TestJoinTarget {
                                place: attempt.place_id,
                                universe: from.universe,
                                client,
                            },
                            &identity,
                            relay["modelName"]
                                .as_str()
                                .context("Routed model has no name")?,
                        )?;
                        call(
                            bridge,
                            target_runtime,
                            role,
                            "invokeRoute",
                            json!({"kind":"join"}),
                        )?;
                        arrival.complete()?;
                        arrival
                    };
                    arrivals.push(arrival);
                }
                let configuration = if let Some(configuration) =
                    &self.endpoints[&route.to].configuration
                {
                    configuration.clone()
                } else {
                    #[cfg(windows)]
                    let mut capture =
                        crate::project::workflows::windows_launch::TestClientRoute::arm(
                            server.pid, *pid,
                        )?;
                    #[cfg(target_os = "macos")]
                    let mut capture =
                        crate::studio::native::serializer::TestClientRoute::arm(server.pid, *pid)?;
                    if self.root.nonce.starts_with("manual-") {
                        // AddPlayers only accepts ExecuteMultiplayerTestAsync
                        // sessions. Manual servers use Studio's Add Client action;
                        // the same launch capture redirects it into this window.
                        let title = crate::studio::input::studio_window_title(server.pid)?;
                        crate::studio::native::serializer::trigger_studio_action(
                            server.pid,
                            &title,
                            "addClientsAction",
                        )?;
                    } else {
                        call(
                            bridge,
                            &server.runtime,
                            BridgeTarget::Server,
                            "addRouteClient",
                            json!({}),
                        )?;
                    }
                    let deadline = Instant::now() + Duration::from_secs(15);
                    let command = loop {
                        if let Some(command) = capture.command()? {
                            break command;
                        }
                        self.wait(bridge, deadline)?;
                    };
                    let configuration = test_client_configuration(
                        &command,
                        self.edit_pid,
                        server.pid,
                        from.universe,
                        attempt.place_id,
                    )?;
                    self.endpoints.get_mut(&route.to).unwrap().configuration =
                        Some(configuration.clone());
                    configuration
                };
                let prepared = call(
                    bridge,
                    runtime,
                    BridgeTarget::Client,
                    "clientRoute",
                    json!({"id":attempt.id,"configuration":configuration}),
                )?;
                let path: Vec<String> = serde_json::from_value(prepared["path"].clone())?;
                #[cfg(windows)]
                crate::studio::native::serializer::rejoin_test_client(*pid, runtime, &path)?;
                #[cfg(target_os = "macos")]
                let _transition = {
                    let mut transition =
                        crate::studio::native::serializer::NativeMacTestClient::arm(
                            *pid,
                            runtime,
                            &path,
                            prepared["modelName"]
                                .as_str()
                                .context("Client model has no name")?,
                            self.captions[pid].window(),
                        )?;
                    let invoked = call(
                        bridge,
                        runtime,
                        BridgeTarget::Client,
                        "invokeRoute",
                        json!({"kind":"client"}),
                    );
                    // A departing model may close its channel after consuming
                    // the command. The native receipt resolves that ambiguity.
                    transition
                        .complete()
                        .or_else(|error| invoked.map(|_| ()).and(Err(error)))?;
                    transition
                };
                bridge.retire_runtime(runtime);
                let deadline = Instant::now() + Duration::from_secs(25);
                loop {
                    let inventory = bridge.list_bridge_clients();
                    if let Some(info) = inventory.iter().find(|row| {
                        row["pid"] == *pid
                            && row["role"] == "play-client"
                            && row["placeId"] == attempt.place_id
                            && row["gameId"] == from.universe
                            && row["runtimeId"] != *runtime
                            && row["playerName"]
                                .as_str()
                                .is_some_and(|name| !name.is_empty())
                    }) {
                        let runtime = text_field(info, "runtimeId")?;
                        call(
                            bridge,
                            &runtime,
                            BridgeTarget::Client,
                            "routeArrival",
                            json!({"owner":self.root.owner,"nonce":self.root.nonce}),
                        )?;
                        bridge.refresh_routed_client(&runtime, *pid)?;
                        for (runtime, role) in [
                            (&runtime, BridgeTarget::Client),
                            (&server.runtime, BridgeTarget::Server),
                        ] {
                            let actual = call(
                                bridge,
                                runtime,
                                role,
                                "routeVerify",
                                json!({"userId":info["playerUserId"]}),
                            )?;
                            ensure!(
                                actual["join"]["SourcePlaceId"] == from.place
                                    && actual["join"]["SourceGameId"] == from.universe
                                    && actual["join"]["TeleportData"] == join["TeleportData"]
                                    && actual["identity"] == identity
                                    && (role != BridgeTarget::Client
                                        || actual["data"] == join["TeleportData"]),
                                "Routed player arrival data did not match its teleport"
                            );
                        }
                        arrived.insert(pid.to_string(), runtime);
                        break;
                    }
                    self.wait(bridge, deadline)?;
                }
                drop(arrivals);
                self.captions.get_mut(pid).unwrap().update(
                    (attempt.place_id != self.root.source)
                        .then_some(destination.display_name.as_str()),
                    false,
                )?;
            }
            self.routes.arrive(&route, &arrived)?;
            let source = &self.endpoints[&from];
            call(
                bridge,
                &source.session.server,
                BridgeTarget::Server,
                "completeRoute",
                json!({"id":attempt.id}),
            )?;
            crate::app::output::log_global(
                3,
                format_args!(
                    "[renium] client teleport complete place={} players={} server={} elapsedMs={} prepareMs={} serverMs={} handoffMs={}",
                    attempt.place_id,
                    participants.len(),
                    server.pid,
                    started.elapsed().as_millis(),
                    resolved.duration_since(started).as_millis(),
                    server_ready.duration_since(resolved).as_millis(),
                    server_ready.elapsed().as_millis()
                ),
            );
            Ok(())
        })();
        if result.is_err() {
            self.routes.cancel(&route);
            for (_, _, pid) in &participants {
                if let Some(caption) = self.captions.get_mut(pid) {
                    let _ = caption.update(None, false);
                }
            }
        }
        result
    }
}

pub(super) fn watch(
    bridge: &Arc<BridgeServer>,
    root: &Session,
    dispatch: Option<Dispatch>,
) -> Result<()> {
    let source_pid = bridge.studio_pid_for_runtime(BridgeTarget::Server, &root.server)?;
    let source = Place {
        universe: root.universe,
        place: root.source,
    };
    let mut test = Test {
        root: root.clone(),
        source_pid,
        source_creation: crate::app::update::process_start_identity(source_pid)
            .context("Source test ended")?,
        edit_pid: bridge.studio_pid_for_runtime(BridgeTarget::Edit, &root.owner)?,
        routes: Routes::new(
            source,
            Server {
                runtime: root.server.clone(),
                nonce: root.nonce.clone(),
                pid: source_pid,
                version: 0,
            },
        ),
        endpoints: BTreeMap::from([(
            source,
            Endpoint {
                session: root.clone(),
                dispatch: dispatch.unwrap_or_else(|| Dispatch::arm(bridge, root)),
                last: 0,
                configuration: None,
            },
        )]),
        captions: HashMap::new(),
    };
    let result = (|| -> Result<()> {
        loop {
            test.alive(bridge)?;
            test.refresh_members(bridge)?;
            let places = test.endpoints.keys().copied().collect::<Vec<_>>();
            for place in places {
                let endpoint = test.endpoints.get_mut(&place).unwrap();
                endpoint.dispatch.renew()?;
                let status = call(
                    bridge,
                    &endpoint.session.server,
                    BridgeTarget::Server,
                    "status",
                    json!({}),
                )?;
                ensure!(
                    status["phase"] != "off" && status["phase"] != "failed",
                    "Server teleport routing ended"
                );
                if !status["pending"].is_null() {
                    let attempt: Attempt = serde_json::from_value(status["pending"].clone())?;
                    let last = endpoint.last;
                    let mut clients = test.clients(bridge, place)?;
                    let result = (|| -> Result<()> {
                        // PlayerAdded can teleport before that player's plugin
                        // registers. Retain this exact pending request while
                        // its real client bridge catches up; do not reject the
                        // player or replay TeleportAsync with another request.
                        let deadline = Instant::now() + Duration::from_secs(15);
                        while attempt
                            .players
                            .iter()
                            .any(|name| !clients.contains_key(name))
                        {
                            test.wait(bridge, deadline)?;
                            test.refresh_members(bridge)?;
                            clients = test.clients(bridge, place)?;
                        }
                        attempt.validate(&clients, last)?;
                        test.travel(bridge, place, &attempt, &clients)
                    })();
                    let endpoint = test.endpoints.get_mut(&place).unwrap();
                    endpoint.last = endpoint.last.max(attempt.id);
                    if let Err(error) = result {
                        let selected = clients
                            .into_iter()
                            .filter(|(name, _)| attempt.players.contains(name))
                            .collect();
                        fail(bridge, &endpoint.session, &selected, &attempt, &error);
                    }
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    })();
    test.routes.stop();
    let _request_guard = bridge.acquire_request_gate();
    let _ = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "finishManualTeleport":true,"launchNonce":root.nonce
        }),
        BridgeTarget::Edit,
        &root.owner,
        Some(Duration::from_secs(3)),
    );
    result
}
