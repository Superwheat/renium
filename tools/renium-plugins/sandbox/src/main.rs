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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_name: Option<String>,
    dedicated_single_host_pool: bool,
    ordered_inventory_complete: bool,
    slots: Vec<Slot>,
}

impl Pool {
    fn owner(&self) -> String {
        match (self.group_id, self.user_id) {
            (Some(group), _) => format!("groups/{group}"),
            (None, Some(user)) => format!("users/{user}"),
            (None, None) => unreachable!("the pool is validated to name an owner"),
        }
    }
}

fn changed_by_default() -> bool {
    true
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
    #[serde(default)]
    user_id: u64,
    universe_claim: renium_plugin_sdk::lease::Claim,
    source_project: Option<PathBuf>,
    source_place: Option<String>,
    directory: PathBuf,
    project: PathBuf,
    phase: Phase,
    pid: Option<u64>,
    // Start time of the owned Studio, so a reused PID is never mistaken for it.
    #[serde(default)]
    studio_started: Option<u64>,
    launch_requested: bool,
    // Journals written before this field existed may have changed the cloud.
    #[serde(default = "changed_by_default")]
    cloud_changed: bool,
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
    let file = File::open(&path).with_context(|| {
        format!(
            "Missing {}. Copy pool.example.json next to it and configure dedicated private test universes first",
            path.display()
        )
    })?;
    let pool: Pool =
        serde_json::from_reader(file).with_context(|| format!("Invalid {}", path.display()))?;
    if pool.schema_version != 1 || pool.slots.is_empty() || pool.slots.len() > 32 {
        bail!("Configure schemaVersion 1 and 1–32 slots");
    }
    if !matches!(
        (pool.group_id, pool.user_id),
        (Some(1..), None) | (None, Some(1..))
    ) {
        bail!(
            "Name the account that owns every slot's experience: exactly one of groupId or userId"
        );
    }
    if !pool.dedicated_single_host_pool || !pool.ordered_inventory_complete {
        bail!(
            "Confirm this is a dedicated single-host pool and declare every ordered store/scope before using it. Unknown ordered stores cannot be safely discovered through Open Cloud."
        );
    }
    let named = |name: &str, extra: &[u8]| {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || extra.contains(&b))
    };
    match (&pool.key_name, &pool.key_env) {
        (Some(name), None) if named(name, b"-.") => {}
        (None, Some(env)) if named(env, b"") => {}
        (Some(_), None) | (None, Some(_)) => bail!(
            "keyName must name a key stored with rbx oc key add, and keyEnv an environment variable; neither may contain a credential"
        ),
        _ => bail!(
            "Set exactly one of keyName (a key stored with rbx oc key add) or keyEnv (an environment variable holding the key)"
        ),
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
    let budget = workflow::Budget::new(std::time::Duration::from_secs(request.timeout_seconds));
    let ctx = request.context;
    let pool = load_pool(&ctx)?;
    let registry = Registry::user()?;
    if request.command == "status" {
        let mut slots = Vec::new();
        for slot in &pool.slots {
            let held = registry.read(&slot.resource())?;
            slots.push(match held {
                Some(held) => json!({"slot":slot.name,"owner":held.session,"workspace":display_path(&held.workspace),"phase":held.data.get("phase"),"available":false}),
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
    if session.slot != *slot
        || session.group_id != pool.group_id.unwrap_or(0)
        || session.user_id != pool.user_id.unwrap_or(0)
    {
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
        &budget,
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

// Canonical Windows paths carry a `\\?\` prefix that means nothing to a reader.
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\")
        .map_or(text.clone(), str::to_owned)
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
