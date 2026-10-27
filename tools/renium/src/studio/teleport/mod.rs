//! Studio teleport sessions rooted in the original Edit controller.
#[cfg(any(windows, target_os = "macos"))]
mod automatic;
mod cache;
#[cfg(any(windows, target_os = "macos"))]
mod multiplayer;
mod native;
mod payload;
#[cfg(any(windows, target_os = "macos"))]
mod routing;
mod title;
#[cfg(any(windows, target_os = "macos"))]
pub(crate) use automatic::{start_manual, take_prepared};
pub(crate) use native::Dispatch;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, atomic::Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::output::ensure_plugin_api_ok;
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::LockRecover;
use crate::system::files::OnDrop;

const NOTICE: &str = "Content emulation retains the source engine context; use ReniumTeleport for emulated destination data. Stop Play returns to the original Edit document.";
const MULTIPLAYER_NATIVE_REQUIRED: &str = "Server & Clients teleport requires a verified native hook on Windows or macOS; the compatibility adapter supports solo Play only";

fn require_multiplayer_native(available: bool) -> Result<()> {
    ensure!(available, "{MULTIPLAYER_NATIVE_REQUIRED}");
    Ok(())
}

/// Newest Studio build whose native Server & Clients launch is verified. Studio
/// 0.745 passes every read-only probe and still exits during the launch, so
/// newer builds refuse multiplayer travel until the launch is verified again.
const MULTIPLAYER_LAUNCH_VERIFIED_BUILD: u32 = 744;

fn require_verified_launch_build(build: u32) -> Result<()> {
    ensure!(
        build <= MULTIPLAYER_LAUNCH_VERIFIED_BUILD,
        "Server & Clients teleport is not verified on Studio 0.{build} and is disabled because the native launch closes Studio; use solo Play, or start a local server from Studio's Test tab until Renium verifies this build"
    );
    Ok(())
}

pub(crate) fn preflight_multiplayer(
    bridge: &BridgeServer,
    owner: &str,
    prefer_native: bool,
) -> Result<()> {
    require_multiplayer_native(prefer_native && cfg!(any(windows, target_os = "macos")))?;
    // Windows exposes the read-only launch probe. macOS validates dispatch at
    // installation; its public PlayTeleportLaunch probe is not implemented.
    #[cfg(windows)]
    {
        use crate::studio::native::serializer::{probes, target_name};
        let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, owner)?;
        require_verified_launch_build(crate::studio::native::serializer::studio_build_number(
            pid,
        )?)?;
        let title = target_name(pid, "")?;
        if let Err(error) = probes::run(pid, &title, probes::NativeOperation::PlayTeleportLaunch) {
            crate::app::output::log_global(
                3,
                format_args!("[renium] multiplayer native preflight failed: {error:#}"),
            );
            anyhow::bail!("{MULTIPLAYER_NATIVE_REQUIRED}. Native probe: {error:#}");
        }
    }
    #[cfg(not(windows))]
    let _ = (bridge, owner);
    Ok(())
}

#[derive(Clone)]
struct Session {
    server: String,
    owner: String,
    nonce: String,
    universe: i64,
    source: i64,
    current_place: i64,
    logical_place: i64,
    root_nonce: String,
    prefer_native: bool,
    multiplayer: bool,
}

#[derive(Default)]
struct Registry {
    active: HashMap<String, Active>,
    pending: Vec<(Session, Option<Dispatch>)>,
}

struct Active {
    root: String,
    nonce: String,
    arriving_nonce: Option<String>,
    native: bool,
    compatibility: bool,
    prefer_native: bool,
}

static SESSIONS: OnceLock<Mutex<Registry>> = OnceLock::new();

pub(crate) fn owns_transition(owner: &str, status: &Value) -> bool {
    status["teleportTransition"].is_string()
        && SESSIONS
            .get()
            .is_some_and(|registry| registry.lock_recover().active.contains_key(owner))
}

impl Registry {
    fn activate(&mut self, session: &Session) -> bool {
        if self.active.get(&session.owner).is_some_and(|active| {
            active.nonce == session.nonce || active.arriving_nonce.as_ref() == Some(&session.nonce)
        }) {
            return false;
        }
        self.active.insert(
            session.owner.clone(),
            Active {
                root: session.root_nonce.clone(),
                nonce: session.nonce.clone(),
                arriving_nonce: None,
                native: false,
                compatibility: false,
                prefer_native: session.prefer_native,
            },
        );
        true
    }

    fn rebind(&mut self, session: &Session, compatibility: bool) -> Result<()> {
        let active = self
            .active
            .get_mut(&session.owner)
            .filter(|active| active.root == session.root_nonce)
            .context("The root Play session was replaced")?;
        active.nonce.clone_from(&session.nonce);
        active.arriving_nonce = None;
        active.compatibility = compatibility;
        active.prefer_native = session.prefer_native;
        Ok(())
    }

    #[cfg(any(windows, target_os = "macos", test))]
    fn arriving(&mut self, session: &Session, nonce: &str) -> Result<()> {
        let active = self
            .active
            .get_mut(&session.owner)
            .filter(|active| active.root == session.root_nonce && active.nonce == session.nonce)
            .context("The root Play session was replaced before destination launch")?;
        active.arriving_nonce = Some(nonce.to_owned());
        Ok(())
    }

    fn retire(&mut self, owner: &str, root: &str) {
        if self
            .active
            .get(owner)
            .is_some_and(|active| active.root == root)
        {
            self.active.remove(owner);
        }
    }
}

fn text_field(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("Teleport requires a current {key}; restart Play with Renium"))
}

impl Session {
    fn retain_origin(&mut self, inventory: &[Value]) -> Result<()> {
        let edit = inventory
            .iter()
            .find(|row| row["runtimeId"] == self.owner && row["role"] == "edit")
            .context("The original teleport Edit runtime ended")?;
        ensure!(
            edit["gameId"] == self.universe,
            "The original teleport universe changed"
        );
        self.source = edit["placeId"]
            .as_i64()
            .filter(|id| *id > 0)
            .context("The original teleport place is unpublished")?;
        Ok(())
    }
    fn from_info(info: &Value) -> Result<Self> {
        ensure!(
            info["role"] == "play-server",
            "Teleport requires a play server"
        );
        let universe = info["gameId"]
            .as_i64()
            .filter(|id| *id > 0)
            .context("Teleport requires a published source universe")?;
        let source = info["placeId"]
            .as_i64()
            .filter(|id| *id > 0)
            .context("Teleport requires a published source place")?;
        Ok(Self {
            server: text_field(info, "runtimeId")?,
            owner: text_field(info, "launchEditRuntimeId")?,
            nonce: text_field(info, "launchNonce")?,
            universe,
            source,
            current_place: source,
            logical_place: source,
            root_nonce: text_field(info, "launchNonce")?,
            prefer_native: true,
            multiplayer: false,
        })
    }

    fn owns(&self, info: &Value) -> bool {
        info["launchEditRuntimeId"] == self.owner
            && info["launchNonce"] == self.nonce
            && info["gameId"] == self.universe
            && info["placeId"] == self.current_place
    }

    fn registered(&self) -> bool {
        SESSIONS.get().is_some_and(|sessions| {
            sessions
                .lock_recover()
                .active
                .get(&self.owner)
                .is_some_and(|active| active.root == self.root_nonce)
        })
    }

    fn clients(&self, inventory: &[Value]) -> Result<HashMap<String, String>> {
        ensure!(
            inventory
                .iter()
                .any(|entry| entry["runtimeId"] == self.server
                    && entry["role"] == "play-server"
                    && self.owns(entry)),
            "Teleport play server ended or changed"
        );
        ensure!(
            inventory
                .iter()
                .any(|entry| entry["runtimeId"] == self.owner && entry["role"] == "edit"),
            "Teleport source Edit runtime ended"
        );
        let mut clients = HashMap::new();
        for entry in inventory
            .iter()
            .filter(|entry| entry["role"] == "play-client" && self.owns(entry))
        {
            let name = text_field(entry, "playerName")?;
            ensure!(
                clients
                    .insert(name, text_field(entry, "runtimeId")?)
                    .is_none(),
                "Ambiguous teleport client identity"
            );
        }
        ensure!(
            (self.multiplayer || !clients.is_empty()) && clients.len() <= 8,
            "Teleport needs 1 through 8 connected play clients"
        );
        Ok(clients)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Attempt {
    id: u64,
    place_id: i64,
    players: Vec<String>,
    data: Value,
    reserved: bool,
    #[serde(default)]
    #[cfg_attr(not(windows), allow(dead_code))]
    native: bool,
    error: Option<String>,
}

impl Attempt {
    fn validate(&self, clients: &HashMap<String, String>, last: u64) -> Result<()> {
        if let Some(error) = &self.error {
            anyhow::bail!("{error}");
        }
        ensure!(
            self.id > last && self.id <= 9_007_199_254_740_991,
            "Stale teleport attempt"
        );
        ensure!(
            (1..=9_007_199_254_740_991).contains(&self.place_id),
            "Teleport destination must be a positive exact place ID"
        );
        ensure!(
            !self.reserved,
            "Studio emulation cannot join reserved servers or a JobId"
        );
        let players = self.players.iter().collect::<HashSet<_>>();
        ensure!(
            !players.is_empty()
                && players.len() == self.players.len()
                && players.iter().all(|player| clients.contains_key(*player)),
            "Teleport player group changed or contains duplicate players"
        );
        let data = self
            .data
            .as_object()
            .context("Invalid per-player teleport data")?;
        ensure!(
            data.keys().collect::<HashSet<_>>() == players,
            "Teleport data does not match the player group"
        );
        ensure!(
            serde_json::to_vec(&self.data)?.len() <= 64 * 1024,
            "Teleport data exceeds 64 KiB"
        );
        Ok(())
    }
}

fn finish_call(result: Value, present: impl FnOnce(&Value) -> Result<()>) -> Result<Value> {
    ensure_plugin_api_ok(&result)?;
    // Presentation is independent of destination preparation/commit. A
    // CoreScript change must never reject an otherwise valid teleport.
    if let Err(error) = present(&result) {
        crate::app::output::log_global(
            2,
            format_args!(
                "[renium] teleport loading screen unavailable; travel continues: {error:#}"
            ),
        );
    }
    Ok(result)
}

fn call(
    bridge: &BridgeServer,
    runtime: &str,
    target: BridgeTarget,
    action: &str,
    mut params: Value,
) -> Result<Value> {
    params["runtimeId"] = json!(runtime);
    params["action"] = json!(action);
    #[cfg(target_os = "macos")]
    let attempt = params["id"].clone();
    let result = bridge.call_for_runtime_with_timeout(
        "playTeleport",
        params,
        target,
        runtime,
        Some(Duration::from_secs(60)),
    )?;
    finish_call(result, |_result| {
        #[cfg(windows)]
        if target == BridgeTarget::Client && _result["nativeGui"].is_object() {
            let result = _result;
            let path: Vec<String> = serde_json::from_value(result["nativeGui"]["path"].clone())?;
            let show = result["nativeGui"]["show"]
                .as_bool()
                .context("Invalid teleport GUI state")?;
            let pid = bridge.studio_pid_for_runtime(target, runtime)?;
            let title = crate::studio::input::studio_window_title(pid)?;
            crate::studio::native::serializer::notify_teleport_gui(
                pid, &title, runtime, &path, show,
            )?;
        }
        #[cfg(target_os = "macos")]
        if target == BridgeTarget::Client && _result["nativeGui"].is_object() {
            let path: Vec<String> = serde_json::from_value(_result["nativeGui"]["path"].clone())?;
            let show = _result["nativeGui"]["show"]
                .as_bool()
                .context("Invalid teleport GUI state")?;
            let pid = bridge.studio_pid_for_runtime(target, runtime)?;
            let model_name = _result["nativeGui"]["modelName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .context("Missing teleport GUI model name")?;
            let mut gui = crate::studio::native::serializer::NativeMacTeleportGui::arm(
                pid, runtime, model_name, &path, show,
            )?;
            let result = bridge.call_for_runtime_with_timeout(
                "playTeleport",
                json!({"action":"nativeGui","runtimeId":runtime,"id":attempt}),
                target,
                runtime,
                Some(Duration::from_secs(3)),
            )?;
            ensure_plugin_api_ok(&result)?;
            gui.complete()?;
        }
        Ok(())
    })
}

pub(crate) fn enable(
    bridge: &BridgeServer,
    prefer_native: bool,
    mode: &str,
    dispatch: Option<Dispatch>,
) -> Result<Value> {
    let runtime = bridge
        .play_runtime_for_selector(BridgeTarget::Server, None)
        .context("No play server for teleport emulation")?;
    enable_runtime(bridge, &runtime, prefer_native, mode, dispatch, false)
}

pub(crate) fn enable_runtime(
    bridge: &BridgeServer,
    runtime: &str,
    prefer_native: bool,
    mode: &str,
    dispatch: Option<Dispatch>,
    require_native_trigger: bool,
) -> Result<Value> {
    if mode != "play" && !cfg!(any(windows, target_os = "macos")) {
        let reason = "Server & Clients teleport requires destination server routing; teleport currently supports solo Play/Play Here only";
        let _ = call(
            bridge,
            runtime,
            BridgeTarget::Server,
            "disarm",
            json!({"reason":reason}),
        );
        anyhow::bail!("{reason}");
    }
    let inventory = bridge.list_bridge_clients();
    let info = inventory
        .iter()
        .find(|entry| entry["runtimeId"] == runtime)
        .context("Play server disconnected")?;
    let mut session = Session::from_info(info)?;
    session.retain_origin(&inventory)?;
    session.prefer_native = prefer_native;
    session.multiplayer = bridge.studio_pid_for_runtime(BridgeTarget::Server, runtime)?
        != bridge.studio_pid_for_runtime(BridgeTarget::Edit, &session.owner)?;
    if session.multiplayer {
        require_multiplayer_native(cfg!(any(windows, target_os = "macos")) && prefer_native)?;
    }
    session.clients(&inventory)?;
    {
        let mut registry = SESSIONS.get_or_init(Default::default).lock_recover();
        if !registry.activate(&session) {
            return Ok(policy(
                registry.active[&session.owner].prefer_native,
                registry.active[&session.owner].native,
                registry.active[&session.owner].compatibility,
            ));
        }
    }
    let armed = (|| -> Result<Value> {
        if dispatch.is_none() && !session.multiplayer {
            native::reconcile(bridge, &session)?;
        }
        let armed = call(bridge, runtime, BridgeTarget::Server, "arm", json!({}))?;
        dispatch
            .as_ref()
            .map_or(Ok(()), |dispatch| dispatch.bind(bridge, &session))?;
        Ok(armed)
    })();
    let armed = armed.inspect_err(|_| {
        SESSIONS
            .get()
            .unwrap()
            .lock_recover()
            .retire(&session.owner, &session.root_nonce);
    })?;
    let dispatch = dispatch.unwrap_or_else(|| Dispatch::arm(bridge, &session));
    let native = dispatch.ready();
    let compatibility = armed["compatibility"] != false;
    if let Some(place) = armed["emulatedPlaceId"].as_i64() {
        ensure!(
            (1..=9_007_199_254_740_991).contains(&place),
            "Invalid compatibility destination"
        );
        session.logical_place = place;
        session.prefer_native = false;
    }
    let prefer_native = session.prefer_native;
    let mut registry = SESSIONS.get().unwrap().lock_recover();
    if !native && (session.multiplayer || require_native_trigger || !compatibility) {
        registry.retire(&session.owner, &session.root_nonce);
        if session.multiplayer {
            require_multiplayer_native(false)?;
        }
        ensure!(
            !require_native_trigger,
            "Native teleport is unavailable; automatic setup waits for a successful native probe"
        );
        anyhow::bail!(
            "{}",
            armed["compatibilityError"]
                .as_str()
                .unwrap_or("No teleport executor is available")
        );
    }
    let active = registry
        .active
        .get_mut(&session.owner)
        .filter(|active| active.root == session.root_nonce)
        .context("Teleport session ended while arming")?;
    active.native = native;
    active.compatibility = compatibility;
    active.prefer_native = prefer_native;
    registry.pending.push((session, Some(dispatch)));
    Ok(policy(prefer_native, native, compatibility))
}

fn policy(prefer_native: bool, native: bool, compatibility: bool) -> Value {
    json!({"mode":"armed","nativePreferred":prefer_native,"fallback":if compatibility {"emulated"} else {"unavailable"},"access":"stored-key",
        "trigger":if native { "teleport-service" } else { "compatibility-api" },
        "api":if native { "TeleportService:TeleportAsync" } else { "ReplicatedStorage.ReniumTeleport.Teleport" },
        "windowTitle":if cfg!(windows) {"windows"} else if cfg!(target_os = "macos") {"macos"} else {"unavailable"},"fallbackNote":NOTICE})
}

pub(crate) fn start_pending(bridge: &Arc<BridgeServer>) {
    let Some(registry) = SESSIONS.get() else {
        return;
    };
    let pending = std::mem::take(&mut registry.lock_recover().pending);
    for (mut session, dispatch) in pending {
        let bridge = Arc::clone(bridge);
        thread::spawn(move || {
            let owner = session.owner.clone();
            let root = session.root_nonce.clone();
            let _retire = OnDrop::new(|| {
                SESSIONS.get().unwrap().lock_recover().retire(&owner, &root);
            });
            if let Err(error) = watch(&bridge, &mut session, dispatch) {
                let _ = call(
                    &bridge,
                    &session.server,
                    BridgeTarget::Server,
                    "disarm",
                    json!({}),
                );
                crate::app::output::log_global(
                    3,
                    format_args!("[renium] play teleport ended: {error:#}"),
                );
            }
        });
    }
}

fn notify_clients(
    bridge: &BridgeServer,
    clients: &HashMap<String, String>,
    action: &str,
    params: &Value,
) -> Result<()> {
    for runtime in clients.values() {
        call(
            bridge,
            runtime,
            BridgeTarget::Client,
            action,
            params.clone(),
        )?;
    }
    Ok(())
}

fn travel(
    bridge: &BridgeServer,
    session: &mut Session,
    attempt: &Attempt,
    clients: &HashMap<String, String>,
    from_place: i64,
    title: &title::Lease,
    dispatch: &mut native::Dispatch,
) -> Result<()> {
    ensure!(
        attempt.players.len() == clients.len(),
        "Content emulation moves the whole session; teleport every connected player together"
    );
    let started = std::time::Instant::now();
    title.transition(None)?;
    let mut params = json!({"id":attempt.id,"placeId":attempt.place_id,"sourcePlaceId":session.source,
        "gameId":session.universe,"data":attempt.data});
    params["sourcePlaceId"] = json!(from_place);
    params["originPlaceId"] = json!(session.source);
    params["mode"] = json!(if session.prefer_native {
        "checking-native"
    } else {
        "emulated"
    });
    notify_clients(bridge, clients, "loading", &params)?;
    let destination = crate::cloud::teleport::resolve(session.universe, attempt.place_id)?;
    title.transition(Some(&destination.display_name))?;
    params["displayName"] = json!(destination.display_name);
    notify_clients(bridge, clients, "loading", &params)?;
    let selection = native::try_switch(
        bridge,
        session,
        attempt,
        clients,
        &destination,
        title,
        dispatch,
    )?;
    if selection["mode"] == "native" {
        crate::app::output::log_global(
            3,
            format_args!(
                "[renium] teleport complete in {} ms",
                started.elapsed().as_millis()
            ),
        );
        return Ok(());
    }
    for (key, value) in selection
        .as_object()
        .expect("native selection is an object")
    {
        params[key] = value.clone();
    }
    if let Some(reason) = params["nativeUnavailable"].as_str() {
        crate::app::output::log_global(
            3,
            format_args!("[renium] play teleport mode=emulated nativeUnavailable={reason}"),
        );
    }
    notify_clients(bridge, clients, "loading", &params)?;
    let cached = cache::load(&destination)?;
    let payload = cached.payload;
    params["contentSource"] = json!(if cached.downloaded {
        "download"
    } else {
        "cache"
    });
    params["preparedCacheHit"] = json!(cached.prepared_hit);
    params["version"] = json!(destination.saved.version);
    crate::app::output::log_global(
        3,
        format_args!(
            "[renium] play teleport mode=emulated universe={} place={} version={} content={} preparation={}",
            session.universe,
            attempt.place_id,
            destination.saved.version,
            if cached.downloaded {
                "download"
            } else {
                "cache"
            },
            if cached.prepared_hit {
                "cache"
            } else {
                "built"
            },
        ),
    );
    ensure!(
        session.clients(&bridge.list_bridge_clients())? == *clients,
        "Players changed while downloading; source was retained"
    );
    notify_clients(bridge, clients, "loading", &params)?;
    let mut begin = params.clone();
    params["clientGroups"] = payload.manifest["groups"].clone();
    begin["totalBytes"] = json!(payload.bytes.len());
    begin["manifest"] = payload.manifest;
    call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "begin",
        begin,
    )?;
    for (index, bytes) in payload.bytes.chunks(1024 * 1024).enumerate() {
        call(
            bridge,
            &session.server,
            BridgeTarget::Server,
            "chunk",
            json!({"id":attempt.id,"offset":index * 1024 * 1024,"bytes":base64::encode(bytes)}),
        )?;
    }
    call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "prepare",
        json!({"id":attempt.id}),
    )?;
    ensure!(
        session.clients(&bridge.list_bridge_clients())? == *clients,
        "Players changed before teleport commit; source was retained"
    );
    call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "acceptNative",
        json!({"id":attempt.id}),
    )?;
    dispatch.wait_accepted(bridge, attempt)?;
    notify_clients(bridge, clients, "pause", &params)?;
    call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "commitBoundary",
        json!({"id":attempt.id}),
    )?;
    notify_clients(bridge, clients, "clear", &params)?;
    let receipt = call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "commit",
        json!({"id":attempt.id}),
    )?;
    params["replicatedCount"] = receipt["replicatedCount"].clone();
    notify_clients(bridge, clients, "arrive", &params)?;
    title.arrived(
        &destination.display_name,
        attempt.place_id == session.source,
    )?;
    call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "complete",
        json!({"id":attempt.id}),
    )?;
    Ok(())
}

fn fail(
    bridge: &BridgeServer,
    session: &Session,
    clients: &HashMap<String, String>,
    attempt: &Attempt,
    error: &anyhow::Error,
) {
    let mut params =
        json!({"id":attempt.id,"placeId":attempt.place_id,"error":format!("{error:#}")});
    let receipt = call(
        bridge,
        &session.server,
        BridgeTarget::Server,
        "fail",
        params.clone(),
    );
    params["stopRequired"] = json!(
        receipt
            .as_ref()
            .map_or(true, |receipt| receipt["stopRequired"] == true)
    );
    for runtime in clients.values() {
        let _ = call(
            bridge,
            runtime,
            BridgeTarget::Client,
            "fail",
            params.clone(),
        );
    }
    crate::app::output::log_global(2, format_args!("[renium] play teleport failed: {error:#}"));
}

fn watch(
    bridge: &Arc<BridgeServer>,
    session: &mut Session,
    dispatch: Option<Dispatch>,
) -> Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    if session.multiplayer {
        return multiplayer::watch(bridge, session, dispatch);
    }
    let title = title::Lease::start(bridge, session)?;
    if session.logical_place != session.source {
        let name = crate::cloud::teleport::display_name(session.universe, session.logical_place)?;
        title.arrived(&name, false)?;
    }
    let mut dispatch = dispatch.unwrap_or_else(|| native::Dispatch::arm(bridge, session));
    let mut last = 0;
    let mut logical_place = session.logical_place;
    while bridge.alive.load(Ordering::Relaxed) {
        ensure!(
            session.registered(),
            "The root Play session ended or was replaced"
        );
        let clients = session.clients(&bridge.list_bridge_clients())?;
        dispatch.renew()?;
        let status = call(
            bridge,
            &session.server,
            BridgeTarget::Server,
            "status",
            json!({}),
        )?;
        ensure!(
            status["phase"] != "off" && status["phase"] != "failed",
            "Teleport was disarmed or requires Stop Play"
        );
        if !status["pending"].is_null() {
            let attempt: Attempt = serde_json::from_value(status["pending"].clone())?;
            let previous_nonce = session.nonce.clone();
            let result = attempt.validate(&clients, last).and_then(|()| {
                travel(
                    bridge,
                    session,
                    &attempt,
                    &clients,
                    logical_place,
                    &title,
                    &mut dispatch,
                )
            });
            last = last.max(attempt.id);
            if let Err(error) = result {
                title.cancel_transition();
                fail(bridge, session, &clients, &attempt, &error);
            } else {
                logical_place = attempt.place_id;
                session.logical_place = logical_place;
                if session.nonce != previous_nonce {
                    last = 0;
                    if !dispatch.has_launch_scope() {
                        dispatch.release();
                        dispatch = native::Dispatch::arm(bridge, session);
                    }
                } else {
                    // A content import leaves the source VM alive. Preserve
                    // its compatibility semantics through further hops/reconnects.
                    session.prefer_native = false;
                    SESSIONS
                        .get()
                        .unwrap()
                        .lock_recover()
                        .rebind(session, true)?;
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplayer_rejects_compatibility_before_contacting_studio() {
        let bridge = crate::studio::bridge::routing_tests::fixture();
        let error = preflight_multiplayer(&bridge, "unconnected-edit", false).unwrap_err();
        assert_eq!(error.to_string(), MULTIPLAYER_NATIVE_REQUIRED);
        assert_eq!(
            require_multiplayer_native(false).unwrap_err().to_string(),
            error.to_string(),
            "a degraded dispatch has the same actionable error as disabled native travel"
        );
        require_multiplayer_native(true).unwrap();
    }

    #[test]
    fn multiplayer_refuses_unverified_studio_builds() {
        require_verified_launch_build(MULTIPLAYER_LAUNCH_VERIFIED_BUILD).unwrap();
        require_verified_launch_build(741).unwrap();
        let error = require_verified_launch_build(745).unwrap_err().to_string();
        assert!(error.contains("Studio 0.745"), "{error}");
        assert!(error.contains("solo Play"), "{error}");
    }

    #[test]
    fn loading_screen_failure_cannot_abort_travel_or_hide_a_real_failure() {
        let accepted = json!({"ok":true,"nativeGui":{"show":true}});
        assert_eq!(
            finish_call(accepted.clone(), |_| anyhow::bail!(
                "Roblox LoadingScript did not initialize"
            ))
            .unwrap(),
            accepted
        );
        assert!(
            finish_call(
                json!({"ok":false,"error":"Client teleport is busy"}),
                |_| { panic!("A failed teleport operation must not change its GUI") }
            )
            .is_err()
        );
    }

    fn session() -> Session {
        Session::from_info(
            &json!({"runtimeId":"server-a","role":"play-server","launchEditRuntimeId":"edit-a",
            "launchNonce":"launch-a","gameId":7,"placeId":11}),
        )
        .unwrap()
    }
    fn inventory() -> Vec<Value> {
        vec![
            json!({"runtimeId":"edit-a","role":"edit"}),
            json!({"runtimeId":"server-a","role":"play-server","launchEditRuntimeId":"edit-a","launchNonce":"launch-a","gameId":7,"placeId":11}),
            json!({"runtimeId":"client-a","role":"play-client","playerName":"Player1","launchEditRuntimeId":"edit-a","launchNonce":"launch-a","gameId":7,"placeId":11}),
        ]
    }

    #[test]
    fn teleport_policy_requires_owned_published_source() {
        let mut info = inventory().remove(1);
        for key in [
            "runtimeId",
            "launchEditRuntimeId",
            "launchNonce",
            "gameId",
            "placeId",
            "role",
        ] {
            let original = info[key].take();
            assert!(Session::from_info(&info).is_err(), "{key}");
            info[key] = original;
        }
    }

    #[test]
    fn teleport_ownership_excludes_other_places_and_replacement_runtimes() {
        let session = session();
        let mut rows = inventory();
        let mut other = rows[2].clone();
        other["runtimeId"] = json!("other");
        other["launchNonce"] = json!("other-launch");
        rows.push(other);
        assert_eq!(session.clients(&rows).unwrap().len(), 1);
        rows[1]["runtimeId"] = json!("replacement-server");
        assert!(session.clients(&rows).is_err());
        assert!(session.clients(&inventory()[1..]).is_err());
        assert!(session.clients(&inventory()[..2]).is_err());
        let mut duplicates = inventory();
        duplicates.push(duplicates[2].clone());
        assert!(session.clients(&duplicates).is_err());
        let mut crowded = inventory();
        for index in 2..=9 {
            let mut client = crowded[2].clone();
            client["runtimeId"] = json!(format!("client-{index}"));
            client["playerName"] = json!(format!("Player{index}"));
            crowded.push(client);
        }
        assert!(session.clients(&crowded).is_err());
    }

    #[test]
    fn teleport_decision_fences_replays_permissions_reserved_and_partial_groups() {
        let session = session();
        let clients = session.clients(&inventory()).unwrap();
        let good = json!({"id":1,"placeId":12,"players":["Player1"],"data":{"Player1":null},"reserved":false});
        let attempt: Attempt = serde_json::from_value(good.clone()).unwrap();
        assert!(attempt.validate(&clients, 0).is_ok());
        assert!(attempt.validate(&clients, 1).is_err());
        let mut another = good.clone();
        another["placeId"] = json!(99);
        assert!(
            serde_json::from_value::<Attempt>(another)
                .unwrap()
                .validate(&clients, 0)
                .is_ok()
        );
        for (key, value) in [
            ("id", json!(0)),
            ("id", json!(9_007_199_254_740_992u64)),
            ("error", json!("Conflicting destinations")),
            ("placeId", json!(0)),
            ("placeId", json!(-1)),
            ("placeId", json!(9_007_199_254_740_992i64)),
            ("reserved", json!(true)),
            ("players", json!([])),
            ("players", json!(["Player1", "Player1"])),
            ("data", json!({})),
            ("data", json!({"Player1":"x".repeat(65536)})),
        ] {
            let mut invalid = good.clone();
            invalid[key] = value;
            let attempt: Attempt = serde_json::from_value(invalid).unwrap();
            assert!(attempt.validate(&clients, 0).is_err(), "{key}");
        }
    }

    #[test]
    fn teleport_root_follows_hops_without_duplicate_watchers_or_late_retirement() {
        let mut registry = Registry::default();
        let mut original = session();
        assert!(registry.activate(&original));
        let old_nonce = original.root_nonce.clone();
        registry.arriving(&original, "destination-launch").unwrap();
        original.nonce = "destination-launch".into();
        original.current_place = 12;
        original.server = "destination-server".into();
        let mut in_flight = original.clone();
        in_flight.root_nonce.clone_from(&in_flight.nonce);
        assert!(
            !registry.activate(&in_flight),
            "status/start during arrival must not replace the watcher or recover its unfinished launch"
        );
        registry.rebind(&original, true).unwrap();
        let mut repeated = original.clone();
        repeated.root_nonce.clone_from(&repeated.nonce);
        assert!(
            !registry.activate(&repeated),
            "a repeated enable after travel must reuse its root"
        );
        let mut replacement = session();
        replacement.nonce = "manual-launch".into();
        replacement.root_nonce.clone_from(&replacement.nonce);
        assert!(registry.activate(&replacement));
        assert!(registry.rebind(&original, true).is_err());
        registry.retire(&original.owner, &old_nonce);
        assert!(
            !registry.activate(&replacement),
            "late cleanup must retain the new root"
        );
        registry.retire(&replacement.owner, &replacement.root_nonce);
        assert!(registry.activate(&replacement));
    }

    #[test]
    fn reconnected_destination_retains_its_original_edit_place() {
        let mut current = session();
        current.current_place = 12;
        current.source = 12;
        let mut rows = inventory();
        rows[0]["placeId"] = json!(11);
        rows[0]["gameId"] = json!(7);
        current.retain_origin(&rows).unwrap();
        assert_eq!((current.source, current.current_place), (11, 12));
        rows[0]["gameId"] = json!(8);
        assert!(current.retain_origin(&rows).is_err());
        rows[0]["runtimeId"] = json!("replacement-edit");
        assert!(current.retain_origin(&rows).is_err());
    }
}
