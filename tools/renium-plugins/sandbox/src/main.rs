use renium_plugin_sdk::{
    Context, Invocation, PluginContext, Result, Value, bail, json,
    lease::{Lease, Registry},
};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

mod cloud;
mod workflow;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pool {
    schema_version: u32,
    group_id: u64,
    key_env: String,
    dedicated_single_host_pool: bool,
    ordered_inventory_complete: bool,
    slots: Vec<Slot>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Slot {
    name: String,
    universe_id: u64,
    place_id: u64,
    ordered_stores: Vec<OrderedStore>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OrderedStore {
    name: String,
    scope: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Session {
    slot: Slot,
    group_id: u64,
    universe_claim: renium_plugin_sdk::lease::Claim,
    source_project: Option<PathBuf>,
    source_place: Option<String>,
    directory: PathBuf,
    project: PathBuf,
    phase: Phase,
    pid: Option<u64>,
    launch_requested: bool,
    revision: u64,
    cleanup: cloud::Cleanup,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Phase {
    Reserved,
    Sanitizing,
    BlankPublished,
    Launching,
    Loading,
    Ready,
    Closing,
    Resetting,
    Cleaning,
    Clean,
}

impl Slot {
    fn resource(&self) -> String {
        format!("studio-place-{}", self.place_id)
    }
    fn selector(&self) -> String {
        format!("{}:{}", self.universe_id, self.place_id)
    }
}

fn main() -> std::process::ExitCode {
    renium_plugin_sdk::serve(dispatch)
}

fn load_pool(ctx: &PluginContext) -> Result<Pool> {
    let path = ctx.directory.join("pool.json");
    let pool: Pool = serde_json::from_reader(File::open(path).context(
        "Copy pool.example.json to pool.json and configure dedicated private test universes first",
    )?)?;
    if pool.schema_version != 1
        || pool.group_id == 0
        || pool.slots.is_empty()
        || pool.slots.len() > 32
    {
        bail!("Configure schemaVersion 1, a groupId and 1–32 slots");
    }
    if !pool.dedicated_single_host_pool || !pool.ordered_inventory_complete {
        bail!(
            "Confirm this is a dedicated single-host pool and declare every ordered store/scope before using it. Unknown ordered stores cannot be safely discovered through Open Cloud."
        );
    }
    if pool.key_env.is_empty()
        || !pool
            .key_env
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("keyEnv must name an environment variable, not contain a credential");
    }
    let mut names = std::collections::HashSet::new();
    let mut universes = std::collections::HashSet::new();
    let mut places = std::collections::HashSet::new();
    for slot in &pool.slots {
        if !renium_plugin_sdk::valid_name(&slot.name)
            || slot.universe_id == 0
            || slot.place_id == 0
            || slot.universe_id > i64::MAX as u64
            || slot.place_id > i64::MAX as u64
            || !names.insert(&slot.name)
            || !universes.insert(slot.universe_id)
            || !places.insert(slot.place_id)
        {
            bail!(
                "Slots need distinct names, place IDs and universe IDs; DataStores are shared across places in a universe"
            );
        }
        let mut ordered = std::collections::HashSet::new();
        for store in &slot.ordered_stores {
            if store.name.is_empty()
                || store.scope.is_empty()
                || store.scope == "-"
                || !ordered.insert((&store.name, &store.scope))
            {
                bail!("Invalid/duplicate ordered store inventory");
            }
        }
    }
    Ok(pool)
}

fn dispatch(request: Invocation) -> Result<Value> {
    let ctx = request.context;
    let pool = load_pool(&ctx)?;
    let registry = Registry::user()?;
    if request.command == "status" {
        let mut slots = Vec::new();
        for slot in &pool.slots {
            let held = registry.read(&slot.resource())?;
            slots.push(match held {
                Some(held) => json!({"slot":slot.name,"owner":held.session,"workspace":held.workspace,"phase":held.data.get("phase"),"available":false}),
                None => json!({"slot":slot.name,"available":registry.read(&format!("roblox-universe-{}", slot.universe_id))?.is_none()}),
            });
        }
        return Ok(json!({"slots":slots}));
    }
    ctx.session()?;
    if request.command == "acquire" {
        return workflow::acquire(&ctx, &pool, request.arguments["slot"].as_str());
    }
    let name = request.arguments["slot"].as_str().context("Missing slot")?;
    let slot = pool
        .slots
        .iter()
        .find(|slot| slot.name == name)
        .context("Unknown slot")?;
    // Keep the OS lock for the whole command, not just when writing its journal.
    let _operation = operation_lock(&ctx, slot)?;
    let held = registry
        .read(&slot.resource())?
        .context("Slot is not owned")?;
    if held.session != ctx.session()?
        || held.workspace != fs::canonicalize(&ctx.workspace)?
        || held.plugin != ctx.plugin
    {
        bail!("This slot belongs to another task/worktree");
    }
    let mut session: Session = serde_json::from_value(held.data.clone()).context(
        "Slot journal is incomplete; keep it reserved and recover the journal before reuse",
    )?;
    if session.slot != *slot || session.group_id != pool.group_id {
        bail!(
            "Pool configuration changed while this slot was in use; restore its original configuration before cleanup"
        );
    }
    if session.phase != Phase::Clean || registry.read(&session.universe_claim.resource)?.is_some() {
        registry.verify(
            &session.universe_claim.resource,
            Some(&session.universe_claim),
        )?;
    }
    workflow::owned(
        &ctx,
        &pool,
        &held,
        &mut session,
        &request.command,
        &request.arguments,
    )
}

fn operation_lock(ctx: &PluginContext, slot: &Slot) -> Result<File> {
    fs::create_dir_all(ctx.state_directory.join("locks"))?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(
            ctx.state_directory
                .join("locks")
                .join(format!("{}.lock", slot.name)),
        )?;
    file.try_lock()
        .context("Another command is already using this slot; wait for its result")?;
    Ok(file)
}

fn save(held: &Lease, session: &Session) -> Result<()> {
    Registry::user()?.update(&held.claim, serde_json::to_value(session)?)
}

fn target(ctx: &PluginContext, held: &Lease, session: &Session) -> PluginContext {
    let mut target = ctx.target(&session.project, &session.slot.selector());
    target.workspace = session.project.parent().unwrap().to_path_buf();
    target.resource_lease = Some(held.claim.clone());
    target
}

fn write_new(path: &Path, value: &Value) -> Result<()> {
    use std::io::Write;
    let mut file = File::options().write(true).create_new(true).open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    Ok(())
}
