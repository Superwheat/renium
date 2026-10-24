//! Adopt Play started with Studio's own controls, using its verified Edit owner.
use super::*;
use crate::automation::{BoundContext, State};
use crate::studio::automation::test_processes;
use std::time::Instant;

static OWNED: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
static GATES: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
static PREPARED: OnceLock<Mutex<HashMap<String, Prepared>>> = OnceLock::new();
static ATTEMPTS: OnceLock<Mutex<HashMap<String, SetupAttempt>>> = OnceLock::new();

#[derive(Default)]
struct SetupAttempt {
    running: bool,
    failures: u32,
    retry_at: Option<Instant>,
}

impl SetupAttempt {
    fn begin(&mut self, now: Instant) -> bool {
        if self.running || self.retry_at.is_some_and(|at| now < at) {
            return false;
        }
        self.running = true;
        true
    }

    fn failed(&mut self, now: Instant) {
        self.running = false;
        self.failures = (self.failures + 1).min(6);
        self.retry_at = Some(now + Duration::from_millis(500 << (self.failures - 1)));
    }
}

fn begin_setup(runtime: &str) -> bool {
    ATTEMPTS
        .get_or_init(Default::default)
        .lock_recover()
        .entry(runtime.to_owned())
        .or_default()
        .begin(Instant::now())
}

fn finish_setup(runtime: &str, result: Result<()>) {
    let mut attempts = ATTEMPTS.get().unwrap().lock_recover();
    if let Err(error) = result {
        attempts
            .entry(runtime.to_owned())
            .or_default()
            .failed(Instant::now());
        crate::app::output::log_global(
            2,
            format_args!("[renium] automatic teleport setup will retry for {runtime}: {error:#}"),
        );
    } else {
        attempts.remove(runtime);
    }
}

struct Prepared {
    dispatch: Dispatch,
    nonce: String,
}

pub(super) struct Ownership(String);

impl Ownership {
    pub(super) fn claim(owner: &str) -> Self {
        *OWNED
            .get_or_init(Default::default)
            .lock_recover()
            .entry(owner.to_owned())
            .or_default() += 1;
        Self(owner.to_owned())
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let mut owners = OWNED.get().unwrap().lock_recover();
        if let Some(count) = owners.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                owners.remove(&self.0);
            }
        }
    }
}

pub(super) fn gate(owner: &str) -> Arc<Mutex<()>> {
    GATES
        .get_or_init(Default::default)
        .lock_recover()
        .entry(owner.to_owned())
        .or_default()
        .clone()
}

pub(crate) fn take_prepared(owner: &str) -> Option<Dispatch> {
    PREPARED
        .get_or_init(Default::default)
        .lock_recover()
        .remove(owner)
        .map(|prepared| prepared.dispatch)
}

fn registered(owner: &str) -> bool {
    SESSIONS
        .get()
        .is_some_and(|registry| registry.lock_recover().active.contains_key(owner))
}

fn manual_server<'a>(row: &Value, contexts: &'a [BoundContext]) -> Option<&'a BoundContext> {
    if row["role"] != "play-server"
        || row["launchNonce"]
            .as_str()
            .is_some_and(|nonce| !nonce.is_empty() && !nonce.starts_with("manual-"))
        || row["placeId"].as_i64().is_none_or(|id| id <= 0)
        || row["gameId"].as_i64().is_none_or(|id| id <= 0)
    {
        return None;
    }
    let owner = row["launchEditRuntimeId"].as_str()?;
    contexts
        .iter()
        .find(|context| context.initialized && context.runtime_id.as_deref() == Some(owner))
}

fn connected_owner(context: &BoundContext, rows: &[Value]) -> Option<String> {
    if let Some(runtime) = &context.runtime_id
        && rows
            .iter()
            .any(|row| row["role"] == "edit" && row["runtimeId"] == *runtime)
    {
        return Some(runtime.clone());
    }
    // Opening Studio binds the project before its bridge exists. Recover that
    // binding by published identity without requiring another CLI command.
    let (place, universe) = (context.place_id?, context.game_id?);
    if place <= 0 || universe <= 0 {
        return None;
    }
    let mut matching = rows.iter().filter(|row| {
        row["role"] == "edit" && row["placeId"] == place && row["gameId"] == universe
    });
    let owner = matching.next()?["runtimeId"].as_str()?.to_owned();
    matching.next().is_none().then_some(owner)
}

pub(crate) fn start_manual(bridge: &Arc<BridgeServer>, state: &State) {
    let rows = bridge.list_bridge_clients();
    let contexts = state
        .bound_contexts()
        .into_iter()
        .filter_map(|mut context| {
            context.runtime_id = Some(connected_owner(&context, &rows)?);
            Some(context)
        })
        .collect::<Vec<_>>();
    ATTEMPTS
        .get_or_init(Default::default)
        .lock_recover()
        .retain(|runtime, attempt| {
            attempt.running || rows.iter().any(|row| row["runtimeId"] == *runtime)
        });
    prepare_idle(bridge, &rows, &contexts);
    for row in &rows {
        let Some(context) = manual_server(row, &contexts) else {
            continue;
        };
        let Some(runtime) = row["runtimeId"].as_str() else {
            continue;
        };
        if registered(row["launchEditRuntimeId"].as_str().unwrap_or_default()) {
            continue;
        }
        // A separate server can be armed before players connect. Same-window
        // Run has no clients and must not be mistaken for solo Play.
        let separate_server = row["pid"].as_u64().is_some_and(|pid| {
            rows.iter().any(|peer| {
                peer["runtimeId"] == row["launchEditRuntimeId"]
                    && peer["role"] == "edit"
                    && peer["pid"].as_u64().is_some_and(|edit| edit != pid)
            })
        });
        if (!separate_server
            && !rows.iter().any(|peer| {
                peer["role"] == "play-client"
                    && peer["playerName"]
                        .as_str()
                        .is_some_and(|name| !name.is_empty())
                    && peer["launchEditRuntimeId"] == row["launchEditRuntimeId"]
                    && peer["placeId"] == row["placeId"]
                    && peer["gameId"] == row["gameId"]
            }))
            || !begin_setup(runtime)
        {
            continue;
        }
        let bridge = Arc::clone(bridge);
        let runtime = runtime.to_owned();
        let row = row.clone();
        let root = context.root.clone();
        thread::spawn(move || {
            finish_setup(&runtime, adopt(&bridge, &row, &root));
        });
    }
}

fn prepare_idle(bridge: &Arc<BridgeServer>, rows: &[Value], contexts: &[BoundContext]) {
    {
        let mut prepared = PREPARED.get_or_init(Default::default).lock_recover();
        prepared.retain(|owner, _| {
            rows.iter()
                .any(|row| row["runtimeId"] == *owner && row["role"] == "edit")
        });
    }
    for context in contexts.iter().filter(|context| context.initialized) {
        let Some(owner) = context.runtime_id.as_deref() else {
            continue;
        };
        if !rows.iter().any(|row| {
            row["role"] == "edit"
                && row["runtimeId"] == owner
                && row["placeId"].as_i64().is_some_and(|id| id > 0)
                && row["gameId"].as_i64().is_some_and(|id| id > 0)
        }) || rows
            .iter()
            .any(|row| row["role"] == "play-server" && row["launchEditRuntimeId"] == owner)
            || registered(owner)
            || OWNED
                .get_or_init(Default::default)
                .lock_recover()
                .contains_key(owner)
            || !begin_setup(owner)
        {
            continue;
        }
        let bridge = Arc::clone(bridge);
        let owner = owner.to_owned();
        let root = context.root.clone();
        thread::spawn(move || {
            finish_setup(&owner, prepare(&bridge, &owner, &root));
        });
    }
}

fn prepare(bridge: &BridgeServer, owner: &str, root: &str) -> Result<()> {
    let gate = gate(owner);
    let _guard = gate.lock_recover();
    if OWNED
        .get_or_init(Default::default)
        .lock_recover()
        .contains_key(owner)
    {
        return Ok(());
    }
    let config = crate::project::config::load_merged_config(std::path::Path::new(root))?;
    if config["playTeleportEnabled"] == false {
        return Ok(());
    }
    let status = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({}),
        BridgeTarget::Edit,
        owner,
        Some(Duration::from_secs(3)),
    )?;
    ensure_plugin_api_ok(&status)?;
    ensure!(
        status["teleportManualProtocol"] == 1,
        "Reopen this Studio with the updated plugin for automatic Play teleport"
    );
    if status["readyForStart"] != true {
        return Ok(());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("Cannot create manual Play identity: {error}"))?;
    let nonce = format!("manual-{:032x}", u128::from_le_bytes(random));
    let dispatch = Dispatch::install_before_play(bridge, owner, &nonce)
        .context("Native Play dispatch could not be installed")?;
    let result = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "prepareManualTeleport":true,"runtimeId":owner,"launchNonce":nonce
        }),
        BridgeTarget::Edit,
        owner,
        Some(Duration::from_secs(3)),
    )?;
    ensure_plugin_api_ok(&result)?;
    if result["prepared"] == false {
        return Ok(());
    }
    PREPARED
        .get_or_init(Default::default)
        .lock_recover()
        .insert(
            owner.to_owned(),
            Prepared {
                dispatch,
                nonce: nonce.clone(),
            },
        );
    crate::app::output::log_global(
        3,
        format_args!("[renium] manual Play teleport ready before launch for {owner}"),
    );
    drop(_guard);
    let result = monitor_prepared(bridge, owner, &nonce, &result["manualPlayEpoch"]);
    let mut prepared = PREPARED.get().unwrap().lock_recover();
    if prepared
        .get(owner)
        .is_some_and(|entry| entry.nonce == nonce)
    {
        prepared.remove(owner);
    }
    result
}

fn monitor_prepared(bridge: &BridgeServer, owner: &str, nonce: &str, epoch: &Value) -> Result<()> {
    let mut renewed = std::time::Instant::now();
    loop {
        {
            let mut prepared = PREPARED.get().unwrap().lock_recover();
            let Some(entry) = prepared.get_mut(owner).filter(|entry| entry.nonce == nonce) else {
                return Ok(());
            };
            if !bridge.alive.load(Ordering::Relaxed) {
                return Ok(());
            }
            if renewed.elapsed() >= Duration::from_secs(30) {
                entry.dispatch.renew()?;
                renewed = std::time::Instant::now();
            }
        }
        let status = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            json!({}),
            BridgeTarget::Edit,
            owner,
            Some(Duration::from_secs(5)),
        )?;
        ensure_plugin_api_ok(&status)?;
        // The epoch records both edges, even if Start and early Stop occurred
        // between polls and no Play bridge ever registered.
        if status["manualPlayEpoch"] != *epoch
            && status["running"] == false
            && status["starting"] == false
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn adopt_runtime(
    bridge: &BridgeServer,
    target: BridgeTarget,
    runtime: &str,
    owner: &str,
    nonce: &str,
    mode: &str,
) -> Result<()> {
    let result = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "adoptTeleport":true,"runtimeId":runtime,"editRuntimeId":owner,
            "launchNonce":nonce,"mode":mode
        }),
        target,
        runtime,
        Some(Duration::from_secs(5)),
    )?;
    ensure_plugin_api_ok(&result)
}

fn adopt(bridge: &BridgeServer, info: &Value, root: &str) -> Result<()> {
    let config = crate::project::config::load_merged_config(std::path::Path::new(root))?;
    if config["playTeleportEnabled"] == false {
        if let Some(owner) = info["launchEditRuntimeId"].as_str() {
            drop(take_prepared(owner));
        }
        return Ok(());
    }
    let owner = text_field(info, "launchEditRuntimeId")?;
    let runtime = text_field(info, "runtimeId")?;
    let gate = gate(&owner);
    let _guard = gate.lock_recover();
    let nonce = PREPARED
        .get_or_init(Default::default)
        .lock_recover()
        .get(&owner)
        .map(|prepared| prepared.nonce.clone())
        .or_else(|| {
            info["launchNonce"]
                .as_str()
                .filter(|nonce| !nonce.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("manual-{runtime}"));
    let edit_pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &owner)?;
    let server_pid = bridge.studio_pid_for_runtime(BridgeTarget::Server, &runtime)?;
    let multiplayer = edit_pid != server_pid;
    let processes = test_processes::studio_process_table();
    let descendants = test_processes::test_descendants(&processes, edit_pid);
    ensure!(
        !multiplayer || descendants.contains(&server_pid),
        "Manual Play server is not a test process of its Edit window"
    );
    let rows = bridge.list_bridge_clients();
    ensure!(
        rows.iter()
            .filter(|row| row["role"] == "play-server" && row["launchEditRuntimeId"] == owner)
            .count()
            == 1,
        "Manual Play has ambiguous server ownership"
    );
    let clients = rows
        .iter()
        .filter(|row| {
            row["role"] == "play-client"
                && row["launchEditRuntimeId"] == owner
                && row["placeId"] == info["placeId"]
                && row["gameId"] == info["gameId"]
        })
        .collect::<Vec<_>>();
    ensure!(
        multiplayer || !clients.is_empty(),
        "Manual Play clients disconnected"
    );
    let mut peers = vec![(runtime.clone(), BridgeTarget::Server, server_pid)];
    for client in clients {
        let runtime = text_field(client, "runtimeId")?;
        let pid = bridge.studio_pid_for_runtime(BridgeTarget::Client, &runtime)?;
        ensure!(
            if multiplayer {
                descendants.contains(&pid)
            } else {
                pid == edit_pid
            },
            "Manual Play client does not belong to its Edit window"
        );
        peers.push((runtime, BridgeTarget::Client, pid));
    }
    let mode = if multiplayer { "multi" } else { "play" };
    // The plugin compares both the runtime and any existing launch identity.
    // A delayed adoption can never replace a different test or teleport.
    let status = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({}),
        BridgeTarget::Edit,
        &owner,
        Some(Duration::from_secs(3)),
    )?;
    let resumed =
        status["teleportJournal"]["nonce"] == nonce && status["teleportJournal"]["ready"] == true;
    for (runtime, target) in std::iter::once((&owner, BridgeTarget::Edit))
        .filter(|_| !resumed)
        .chain(peers.iter().map(|(runtime, target, _)| (runtime, *target)))
    {
        adopt_runtime(bridge, target, runtime, &owner, &nonce, mode)?;
    }
    for (runtime, target, pid) in &peers {
        bridge.refresh_launch_runtime(runtime, *target, *pid)?;
    }
    let mut dispatch = take_prepared(&owner);
    if multiplayer {
        drop(dispatch.take());
    }
    enable_runtime(
        bridge,
        &runtime,
        config["playTeleportNative"] != false,
        mode,
        dispatch,
        true,
    )?;
    crate::app::output::log_global(
        3,
        format_args!("[renium] manual Play teleport armed for {runtime}"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_teleport_retries_failed_setup_without_overlapping_attempts() {
        let mut attempt = SetupAttempt::default();
        let mut now = Instant::now();
        for failure in 0..20 {
            assert!(attempt.begin(now));
            assert!(!attempt.begin(now + Duration::from_secs(60)));
            attempt.failed(now);
            let delay = Duration::from_millis(500 << failure.min(5));
            assert!(!attempt.begin(now + delay - Duration::from_millis(1)));
            now += delay;
        }
        assert!(
            attempt.begin(now),
            "a transient error must never permanently disable setup"
        );
    }

    #[test]
    fn automatic_teleport_requires_a_bound_manual_server() {
        let context = BoundContext {
            id: 1,
            initialized: true,
            project: String::new(),
            root: String::new(),
            experience: String::new(),
            source: String::new(),
            resource_lease: None,
            place_id: Some(11),
            game_id: Some(7),
            selector: String::new(),
            runtime_id: Some("edit-a".into()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let contexts = [context];
        let row = json!({"role":"play-server","runtimeId":"server-a",
            "launchEditRuntimeId":"edit-a","placeId":11,"gameId":7});
        assert!(manual_server(&row, &contexts).is_some());
        let edit = json!({"role":"edit","runtimeId":"edit-a","placeId":11,"gameId":7});
        let mut opening = contexts[0].clone();
        opening.runtime_id = None;
        assert_eq!(
            connected_owner(&opening, std::slice::from_ref(&edit)).as_deref(),
            Some("edit-a")
        );
        let mut duplicate = edit.clone();
        duplicate["runtimeId"] = json!("edit-b");
        assert_eq!(
            connected_owner(&opening, &[edit.clone(), duplicate]),
            None,
            "a published identity must never choose between duplicate open windows"
        );
        opening.game_id = Some(8);
        assert_eq!(connected_owner(&opening, &[edit]), None);
        for (key, value) in [
            ("role", json!("play-client")),
            ("launchNonce", json!("owned-launch")),
            ("launchEditRuntimeId", json!("edit-b")),
            ("placeId", json!(0)),
            ("gameId", json!(0)),
        ] {
            let mut other = row.clone();
            other[key] = value;
            assert!(manual_server(&other, &contexts).is_none(), "{key}");
        }
    }
}
