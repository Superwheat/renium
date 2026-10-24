//! Replace the test through its original Edit controller. The source document
//! remains the lifecycle root, including while neither Play model exists.
use super::*;

pub(crate) struct Dispatch {
    #[cfg(windows)]
    lease: Option<crate::studio::native::serializer::NativeTeleportDispatch>,
}

impl Dispatch {
    pub(super) fn ready(&self) -> bool {
        #[cfg(windows)]
        {
            self.lease.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
    pub(crate) fn before_play(bridge: &BridgeServer, owner: &str, nonce: &str) -> Option<Self> {
        #[cfg(windows)]
        {
            let result = (|| -> Result<_> {
                let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, owner)?;
                let title = crate::studio::input::studio_window_title(pid)?;
                crate::studio::native::serializer::NativeTeleportDispatch::before_play(
                    pid, &title, nonce,
                )
            })();
            match result {
                Ok(lease) => Some(Self { lease: Some(lease) }),
                Err(error) => {
                    crate::app::output::log_global(
                        2,
                        format_args!("[renium] native teleport startup unavailable: {error:#}"),
                    );
                    None
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (bridge, owner, nonce);
            None
        }
    }

    pub(super) fn has_launch_scope(&self) -> bool {
        #[cfg(windows)]
        {
            self.lease
                .as_ref()
                .is_some_and(|lease| lease.has_launch_scope())
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub(super) fn bind(&self, bridge: &BridgeServer, session: &Session) -> Result<()> {
        if self.has_launch_scope() {
            call(
                bridge,
                &session.server,
                BridgeTarget::Server,
                "nativeBridge",
                json!({"scope":session.nonce}),
            )?;
        }
        Ok(())
    }

    pub(super) fn arm(bridge: &BridgeServer, session: &Session) -> Self {
        #[cfg(windows)]
        {
            let result = (|| -> Result<_> {
                let relay = call(
                    bridge,
                    &session.server,
                    BridgeTarget::Server,
                    "nativeBridge",
                    json!({}),
                )?;
                let path: Vec<String> = serde_json::from_value(relay["path"].clone())?;
                let pid = bridge.studio_pid_for_runtime(BridgeTarget::Server, &session.server)?;
                let title = crate::studio::input::studio_window_title(pid)?;
                crate::studio::native::serializer::NativeTeleportDispatch::arm(pid, &title, &path)
            })();
            match result {
                Ok(lease) => {
                    crate::app::output::log_global(
                        3,
                        format_args!(
                            "[renium] native teleport trigger armed for {}",
                            session.server
                        ),
                    );
                    Self { lease: Some(lease) }
                }
                Err(error) => {
                    crate::app::output::log_global(
                        2,
                        format_args!("[renium] native teleport trigger unavailable: {error:#}"),
                    );
                    Self { lease: None }
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (bridge, session);
            Self {}
        }
    }

    pub(super) fn renew(&mut self) -> Result<()> {
        #[cfg(windows)]
        if let Some(lease) = &mut self.lease {
            lease.renew()?;
        }
        Ok(())
    }

    pub(super) fn wait_accepted(&self, bridge: &BridgeServer, attempt: &Attempt) -> Result<()> {
        #[cfg(windows)]
        if attempt.native {
            let trigger = self
                .lease
                .as_ref()
                .context("The native teleport trigger ended")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !trigger.accepted(attempt.id, attempt.place_id)? {
                waiting(bridge, deadline)?;
            }
        }
        #[cfg(not(windows))]
        let _ = (bridge, attempt);
        Ok(())
    }
}

#[cfg(windows)]
fn controller(bridge: &BridgeServer, session: &Session, params: Value) -> Result<Value> {
    let result = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        params,
        BridgeTarget::Edit,
        &session.owner,
        Some(Duration::from_secs(5)),
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

/// Reconcile with the surviving Edit controller before a replacement daemon
/// accepts travel. An unacknowledged arrival is stopped, never adopted as ready.
pub(super) fn reconcile(bridge: &BridgeServer, session: &Session) -> Result<()> {
    #[cfg(windows)]
    {
        let status = controller(bridge, session, json!({}))?;
        ensure!(
            status["teleportLoadProtocol"] == 2,
            "Update the Studio plugin and reopen this window for teleport recovery"
        );
        let journal = &status["teleportJournal"];
        ensure!(
            journal["nonce"] == session.nonce && journal["mode"] == "play",
            "The original Edit controller no longer owns this teleport test"
        );
        if journal["ready"] != true {
            let result = bridge.call_for_runtime_with_timeout(
                "startStopPlay",
                json!({"stop":true,"launchNonce":session.nonce,"waitForStopped":false}),
                BridgeTarget::Server,
                &session.server,
                Some(Duration::from_secs(5)),
            )?;
            ensure_plugin_api_ok(&result)?;
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while controller(bridge, session, json!({}))?["running"] == true {
                waiting(bridge, deadline)?;
            }
            anyhow::bail!(
                "The previous teleport was interrupted; the original Edit was restored. Start Play again"
            );
        }
        controller(
            bridge,
            session,
            json!({"resumeTeleport":true,"launchNonce":session.nonce}),
        )?;
    }
    #[cfg(not(windows))]
    let _ = (bridge, session);
    Ok(())
}

#[cfg(windows)]
fn waiting(bridge: &BridgeServer, deadline: std::time::Instant) -> Result<()> {
    ensure!(
        bridge.alive.load(Ordering::Relaxed),
        "Teleport host stopped"
    );
    ensure!(
        std::time::Instant::now() < deadline,
        "Destination test did not become ready; source Edit was retained"
    );
    thread::sleep(Duration::from_millis(50));
    Ok(())
}

#[cfg(windows)]
struct Prepared {
    pid: u32,
    bytes: cache::SavedPlace,
    arrival: Option<crate::studio::native::serializer::NativePlayJoin>,
    dispatch: u64,
}

#[cfg(windows)]
fn prepare(
    bridge: &BridgeServer,
    session: &Session,
    clients: &HashMap<String, String>,
    destination: &crate::cloud::teleport::Destination,
    attempt: &Attempt,
    dispatch: &Dispatch,
) -> Result<Prepared> {
    use crate::studio::native::serializer::probes::{self, NativeOperation};
    let status = controller(bridge, session, json!({}))?;
    ensure!(
        status["teleportLoadProtocol"] == 2,
        "Update the Studio plugin for native destination loading"
    );
    ensure!(
        status["mode"] == "play"
            && status["running"] == true
            && status["launchNonce"] == session.nonce,
        "Native teleport requires the current owned solo Play test"
    );
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &session.owner)?;
    ensure!(
        bridge.studio_pid_for_runtime(BridgeTarget::Server, &session.server)? == pid
            && clients.len() == 1,
        "Native solo teleport must retain its original Studio window"
    );
    for runtime in clients.values() {
        ensure!(
            bridge.studio_pid_for_runtime(BridgeTarget::Client, runtime)? == pid,
            "Teleport client belongs to another Studio process"
        );
    }
    let title = crate::studio::input::studio_window_title(pid)?;
    probes::run(pid, &title, NativeOperation::PlayTeleportLoad)?;
    let (arrival, dispatch) = if attempt.native {
        let token = dispatch
            .lease
            .as_ref()
            .context("The native teleport trigger ended")?
            .token();
        (
            Some(crate::studio::native::serializer::NativePlayJoin::prepare(
                pid, &title, true,
            )?),
            token,
        )
    } else {
        ensure!(
            attempt
                .data
                .as_object()
                .is_none_or(|data| data.values().all(|row| row["teleportData"].is_null())),
            "Native arrival data requires TeleportService:TeleportAsync"
        );
        (None, 0)
    };
    let bytes = cache::saved_place(destination)?;
    ensure!(
        session.clients(&bridge.list_bridge_clients())? == *clients,
        "Players changed while preparing the destination; source was retained"
    );
    Ok(Prepared {
        pid,
        bytes,
        arrival,
        dispatch,
    })
}

#[cfg(windows)]
fn switch(
    bridge: &BridgeServer,
    session: &mut Session,
    attempt: &Attempt,
    mut prepared: Prepared,
    destination: &crate::cloud::teleport::Destination,
    title: &title::Lease,
    dispatch: &mut Dispatch,
) -> Result<()> {
    use crate::studio::native::serializer::NativePlayLoad;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("Cannot create teleport launch identity: {error}"))?;
    let transition = format!("teleport-{:032x}", u128::from_le_bytes(random));
    let nonce = format!("{transition}-play");
    let clients = session.clients(&bridge.list_bridge_clients())?;
    title.transition()?;
    // This is the commit boundary. Every error below escapes normally; no
    // transplant or second native launch may run over an uncertain transition.
    let result = (|| -> Result<()> {
        controller(
            bridge,
            session,
            json!({"stop":true,"launchNonce":session.nonce,"prepareTeleport":true,
            "teleportTransition":transition,"waitForStopped":false}),
        )?;
        call(
            bridge,
            &session.server,
            BridgeTarget::Server,
            "acceptNative",
            json!({"id":attempt.id}),
        )?;
        dispatch.wait_accepted(bridge, attempt)?;
        let ended = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            json!({"stop":true,"launchNonce":session.nonce,"waitForStopped":false}),
            BridgeTarget::Server,
            &session.server,
            Some(Duration::from_secs(5)),
        )?;
        ensure_plugin_api_ok(&ended)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(35);
        loop {
            let status = controller(bridge, session, json!({}))?;
            ensure!(
                status["teleportTransition"] == transition,
                "Teleport was cancelled while the source stopped"
            );
            if status["readyForStart"] == true {
                break;
            }
            waiting(bridge, deadline)?;
        }
        bridge.retire_runtime(&session.server);
        for runtime in clients.values() {
            bridge.retire_runtime(runtime);
        }
        let window_title = crate::studio::input::studio_window_title(prepared.pid)?;
        if let Some(lease) = &mut dispatch.lease {
            lease.advance(attempt.place_id, &nonce)?;
        }
        if let Some(arrival) = &mut prepared.arrival {
            arrival.arm(
                (session.source, session.universe),
                attempt.place_id,
                &nonce,
                prepared.dispatch,
                attempt.id,
            )?;
        }
        let mut load = NativePlayLoad::arm(
            prepared.pid,
            &window_title,
            (session.source, session.universe),
            (attempt.place_id, session.universe),
            &prepared.bytes.bytes,
            &nonce,
        )?;
        SESSIONS
            .get()
            .context("Teleport root registry ended")?
            .lock_recover()
            .arriving(session, &nonce)?;
        let status = controller(
            bridge,
            session,
            json!({"start":true,"mode":"play","launchNonce":nonce,
            "teleportTransition":transition,"args":{"teleport":{
                "sourcePlaceId":session.current_place,"sourceGameId":session.universe,"data":attempt.data
            }}}),
        )?;
        ensure!(
            status["launchNonce"] == nonce,
            "Destination controller did not accept this teleport launch"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut arrived = session.clone();
        arrived.nonce.clone_from(&nonce);
        arrived.current_place = attempt.place_id;
        arrived.logical_place = attempt.place_id;
        loop {
            let inventory = bridge.list_bridge_clients();
            let servers = inventory
                .iter()
                .filter(|row| row["role"] == "play-server" && arrived.owns(row))
                .collect::<Vec<_>>();
            ensure!(
                servers.len() <= 1,
                "Destination has ambiguous server ownership"
            );
            if let Some(server) = servers.first() {
                arrived.server = text_field(server, "runtimeId")?;
                if let Ok(peers) = arrived.clients(&inventory) {
                    ensure!(
                        peers.keys().collect::<HashSet<_>>() == clients.keys().collect(),
                        "Destination player membership changed"
                    );
                    ensure!(
                        bridge.studio_pid_for_runtime(BridgeTarget::Server, &arrived.server)?
                            == prepared.pid,
                        "Destination server left the original Studio window"
                    );
                    for runtime in peers.values() {
                        ensure!(
                            bridge.studio_pid_for_runtime(BridgeTarget::Client, runtime)?
                                == prepared.pid,
                            "Destination client left the original Studio window"
                        );
                    }
                    ensure!(
                        load.consumed()?,
                        "Destination loader has no consumption receipt"
                    );
                    let armed = call(
                        bridge,
                        &arrived.server,
                        BridgeTarget::Server,
                        "arm",
                        json!({}),
                    )?;
                    dispatch.bind(bridge, &arrived)?;
                    if let Some(arrival) = &mut prepared.arrival {
                        for (runtime, target) in
                            std::iter::once((&arrived.server, BridgeTarget::Server)).chain(
                                peers
                                    .values()
                                    .map(|runtime| (runtime, BridgeTarget::Client)),
                            )
                        {
                            let data = call(
                                bridge,
                                runtime,
                                target,
                                "nativeArrival",
                                json!({
                                    "id":attempt.id,"mode":"native","placeId":attempt.place_id,
                                    "sourcePlaceId":session.current_place,"originPlaceId":session.source,
                                    "gameId":session.universe,"version":destination.saved.version,
                                    "contentSource":if prepared.bytes.downloaded { "download" } else { "cache" }
                                }),
                            )?;
                            let receipt = arrival.receipt()?;
                            let expected = attempt
                                .data
                                .as_object()
                                .and_then(|data| data.values().next())
                                .and_then(|row| row.get("teleportData"))
                                .unwrap_or(&Value::Null);
                            ensure!(
                                data["join"]["SourcePlaceId"] == session.current_place
                                    && data["join"]["SourceGameId"] == session.universe
                                    && &data["join"]["TeleportData"] == expected,
                                "Native destination {target:?} join data did not match the accepted teleport (receipt {receipt:#x})"
                            );
                            if target == BridgeTarget::Client {
                                ensure!(
                                    &data["data"] == expected,
                                    "Native client teleport data did not match the accepted teleport"
                                );
                            }
                        }
                    }
                    controller(
                        bridge,
                        &arrived,
                        json!({
                            "completeTeleport":true,"launchNonce":nonce,"teleportTransition":transition
                        }),
                    )?;
                    *session = arrived;
                    SESSIONS
                        .get()
                        .context("Teleport root registry ended")?
                        .lock_recover()
                        .rebind(session, armed["compatibility"] != false)?;
                    title.rebind(session, &destination.display_name)?;
                    crate::app::output::log_global(
                        3,
                        format_args!(
                            "[renium] play teleport mode=native universe={} place={} version={} content={}",
                            session.universe,
                            session.current_place,
                            destination.saved.version,
                            if prepared.bytes.downloaded {
                                "download"
                            } else {
                                "cache"
                            }
                        ),
                    );
                    return Ok(());
                }
            }
            let status = controller(bridge, session, json!({}))?;
            ensure!(
                status["launchNonce"] == nonce
                    && (status["running"] == true || status["starting"] == true),
                "Destination Play was stopped or replaced: {}",
                status["lastError"]
                    .as_str()
                    .unwrap_or("the launch owner ended")
            );
            waiting(bridge, deadline)?;
        }
    })();
    if result.is_err() {
        // EndTest must run on the destination server. Stopping RunService in
        // Edit alone leaves ExecutePlayModeAsync and its native models alive.
        let mut destination_owner = session.clone();
        destination_owner.nonce.clone_from(&nonce);
        destination_owner.current_place = attempt.place_id;
        for row in bridge
            .list_bridge_clients()
            .iter()
            .filter(|row| row["role"] == "play-server" && destination_owner.owns(row))
        {
            if let Some(runtime) = row["runtimeId"].as_str() {
                let _ = bridge.call_for_runtime_with_timeout(
                    "startStopPlay",
                    json!({"stop":true,"launchNonce":nonce,"waitForStopped":false}),
                    BridgeTarget::Server,
                    runtime,
                    Some(Duration::from_secs(5)),
                );
            }
        }
        // The plugin checks the nonce again. A delayed response cannot stop a
        // new test started manually after this teleport was cancelled.
        if let Ok(status) = controller(bridge, session, json!({})) {
            if status["launchNonce"] == nonce {
                let _ = controller(
                    bridge,
                    session,
                    json!({"stop":true,"launchNonce":nonce,"waitForStopped":false}),
                );
            } else if status["teleportTransition"] == transition {
                let _ = controller(
                    bridge,
                    session,
                    json!({"stop":true,"launchNonce":session.nonce,"waitForStopped":false}),
                );
            }
        }
        title.restore();
    }
    result
}

pub(super) fn try_switch(
    bridge: &BridgeServer,
    session: &mut Session,
    attempt: &Attempt,
    clients: &HashMap<String, String>,
    destination: &crate::cloud::teleport::Destination,
    title: &title::Lease,
    dispatch: &mut Dispatch,
) -> Result<Value> {
    if !session.prefer_native {
        return Ok(json!({"mode":"emulated","nativeRequested":false}));
    }
    #[cfg(windows)]
    let unavailable = match prepare(bridge, session, clients, destination, attempt, dispatch) {
        Ok(prepared) => {
            switch(
                bridge,
                session,
                attempt,
                prepared,
                destination,
                title,
                dispatch,
            )?;
            return Ok(json!({"mode":"native","nativeRequested":true}));
        }
        Err(error) => format!("{error:#}"),
    };
    #[cfg(not(windows))]
    let unavailable = {
        let _ = (
            bridge,
            session,
            attempt,
            clients,
            destination,
            title,
            dispatch,
        );
        "Native destination loading is unavailable on this platform".to_owned()
    };
    Ok(json!({"mode":"emulated","nativeRequested":true,"nativeUnavailable":unavailable}))
}
