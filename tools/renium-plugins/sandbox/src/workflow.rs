use crate::{Phase, Pool, Session, Slot, cloud, operation_lock, save, target, write_new};
use renium_plugin_sdk::{
    Context, PluginContext, Result, Value, bail, json,
    lease::{Lease, Registry},
};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

// Caps for individual Renium calls. Before each long step the remaining command
// budget must cover the step's cap, so the host never stops the plugin mid-step;
// otherwise the command returns at its last saved phase.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(300);
const PUSH_TIMEOUT: Duration = Duration::from_secs(480);
const LOAD_TIMEOUT: Duration =
    Duration::from_secs(SNAPSHOT_TIMEOUT.as_secs() + PUSH_TIMEOUT.as_secs());
const EXPORT_TIMEOUT: Duration = Duration::from_secs(120);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(EXPORT_TIMEOUT.as_secs() + 60);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);
const CLEANUP_BUDGET: Duration = Duration::from_secs(60);
const CONNECT_WAIT: Duration = Duration::from_secs(120);
const STOP_WAIT: Duration = Duration::from_secs(30);
const EXIT_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_secs(2);
const MARGIN: Duration = Duration::from_secs(30);

const RUNTIME_COMMANDS: &[&str] = &[
    "status",
    "studio-status",
    "cs",
    "clients",
    "studios",
    "list-clients",
    "net",
    "network",
    "perf",
    "pf",
    "performance-profile",
    "performance",
    "access",
    "play",
    "playtest",
    "start-stop-play",
    "tst",
    "test",
    "co",
    "console",
    "get-console-output",
    "l",
    "lx",
    "luau",
    "execute-luau",
    "lc",
    "execute-client-luau",
    "wait",
    "wait-until",
    "ss",
    "script-search",
    "sg",
    "script-grep",
    "sr",
    "script-read",
    "ui",
    "user-interface",
    "inp",
    "input",
    "clk",
    "click",
    "ky",
    "key",
    "pr",
    "press",
    "go",
    "goto",
    "ty",
    "type",
    "dev",
    "device",
    "studio-device",
    "sc",
    "shot",
    "screenshot",
    "rs",
    "record-start",
    "re",
    "record-end",
    "rf",
    "record-review",
    "record-frames",
];

/// Time left in the current command before the host stops the process tree,
/// less a margin for journaling and the final response.
pub(super) struct Budget {
    deadline: Instant,
}

impl Budget {
    pub(super) fn new(total: Duration) -> Self {
        Self {
            deadline: Instant::now() + total.saturating_sub(MARGIN),
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn fits(&self, need: Duration) -> bool {
        self.remaining() >= need
    }

    fn require(&self, need: Duration, step: &str) -> Result<()> {
        if self.fits(need) {
            return Ok(());
        }
        bail!(
            "This command's budget cannot cover {step} ({}s); raise its timeoutSeconds in renium-plugin.json and reinstall",
            need.as_secs()
        )
    }
}

pub(super) fn acquire(ctx: &PluginContext, pool: &Pool, wanted: Option<&str>) -> Result<Value> {
    let registry = Registry::user()?;
    let inventory = ctx.renium(&["status", "--all"])?;
    for slot in &pool.slots {
        if wanted.is_some_and(|name| name != slot.name) {
            continue;
        }
        let _lock = match operation_lock(ctx, slot) {
            Ok(lock) => lock,
            Err(_) if wanted.is_none() => continue,
            Err(error) => return Err(error),
        };
        if let Some(held) = registry.read(&slot.resource())? {
            if held.session == ctx.session()? && held.workspace == fs::canonicalize(&ctx.workspace)?
            {
                return Ok(
                    json!({"slot":slot.name,"phase":held.data.get("phase"),"next":"prepare","reusedLease":true}),
                );
            }
            continue;
        }
        if !matching_studios(&inventory, slot)?.is_empty() {
            continue;
        }
        let universe = format!("roblox-universe-{}", slot.universe_id);
        if registry.read(&universe)?.is_some() {
            continue;
        }
        let held = registry.acquire(
            &slot.resource(),
            &ctx.plugin,
            ctx.session()?,
            &ctx.workspace,
        )?;
        let universe =
            match registry.acquire(&universe, &ctx.plugin, ctx.session()?, &ctx.workspace) {
                Ok(lease) => lease,
                Err(error) => {
                    registry.release(&held.claim)?;
                    return Err(error);
                }
            };
        // Record both claims before creating files or making cloud/Studio changes.
        let directory = ctx.state_directory.join("runs").join(&held.claim.token);
        let session = Session {
            slot: slot.clone(),
            group_id: pool.group_id,
            universe_claim: universe.claim,
            source_project: ctx.project.clone(),
            source_place: ctx.place.clone(),
            project: directory.join("blank/project/renium.project.jsonc"),
            directory,
            phase: Phase::Reserved,
            pid: None,
            launch_requested: false,
            revision: 0,
            cleanup: cloud::Cleanup::default(),
        };
        save(&held, &session)?;
        return Ok(progress(&session, "prepare"));
    }
    bail!(
        "No available sandbox slot{}; active or unclean slots are never taken over. See rbx sandbox status",
        wanted
            .map(|name| format!(" named {name}"))
            .unwrap_or_default()
    )
}

pub(super) fn owned(
    ctx: &PluginContext,
    pool: &Pool,
    held: &Lease,
    session: &mut Session,
    command: &str,
    args: &Value,
    budget: &Budget,
) -> Result<Value> {
    match command {
        "prepare" => prepare(ctx, pool, held, session, budget),
        "refresh" => {
            if session.phase != Phase::Ready {
                bail!("Finish prepare before refreshing");
            }
            verify_stopped(ctx, held, session)?;
            budget.require(LOAD_TIMEOUT, "a snapshot and push")?;
            load_projection(ctx, held, session)?;
            Ok(progress(session, "run"))
        }
        "run" => run(ctx, held, session, args, budget),
        "release" => release(ctx, pool, held, session, args, budget),
        _ => bail!("Unknown sandbox command"),
    }
}

fn prepare(
    ctx: &PluginContext,
    pool: &Pool,
    held: &Lease,
    session: &mut Session,
    budget: &Budget,
) -> Result<Value> {
    match session.phase {
        Phase::Reserved => {
            ctx.renium(&["plugin", "runtime-check"])?;
            cloud::Client::new(pool, &session.slot)?.validate_slot(pool.group_id)?;
            create_blank(session)?;
            budget.require(PUBLISH_TIMEOUT, "publishing the blank place")?;
            publish_blank(ctx, pool, held, session)?;
            session.phase = Phase::Sanitizing;
            save(held, session)?;
            Ok(progress(session, "prepare"))
        }
        Phase::Sanitizing => {
            if clean_universe(pool, held, session, budget)? {
                session.phase = Phase::BlankPublished;
                session.cleanup = cloud::Cleanup::default();
                save(held, session)?;
            }
            Ok(progress(session, "prepare"))
        }
        Phase::BlankPublished => {
            let inventory = ctx.renium(&["status", "--all"])?;
            if !matching_studios(&inventory, &session.slot)?.is_empty() {
                bail!(
                    "The slot's place is already open in a Studio outside this task; no Studio was changed"
                );
            }
            // Persist intent first. Lost launch responses are not blindly replayed.
            session.phase = Phase::Launching;
            session.launch_requested = true;
            save(held, session)?;
            let result = target(ctx, held, session).renium_timeout(&["ro"], LAUNCH_TIMEOUT)?;
            if result["alreadyOpen"].as_bool() == Some(true) {
                bail!(
                    "Renium found an existing Studio instead of opening an owned one; slot remains reserved"
                );
            }
            session.pid = Some(result["pid"].as_u64().filter(|pid| *pid > 0).context(
                "Studio launch did not return its owning PID; keep the slot reserved for recovery",
            )?);
            save(held, session)?;
            prepare(ctx, pool, held, session, budget)
        }
        Phase::Launching => {
            let pid = session.pid.context(
                "The launch receipt was lost, so no Studio can be adopted. Close the test place yourself, then run release --confirm-closed",
            )?;
            let wait = CONNECT_WAIT.min(budget.remaining());
            let Some(studios) = await_connection(ctx, &session.slot, pid, wait)? else {
                return Ok(
                    json!({"slot":session.slot.name,"phase":session.phase,"ready":false,"pid":pid,"next":"prepare","note":"Studio is still starting; run prepare again"}),
                );
            };
            verify_owner(&studios, pid)?;
            verify_stopped(ctx, held, session)?;
            if !budget.fits(LOAD_TIMEOUT) {
                return Ok(deferred(session));
            }
            load_projection(ctx, held, session)?;
            Ok(progress(session, "run"))
        }
        Phase::Loading => {
            // A previous push may already have committed. An explicit repeat reconciles
            // that exact saved projection, never an arbitrary mutation or play request.
            verify_stopped(ctx, held, session)?;
            budget.require(PUSH_TIMEOUT, "a push")?;
            push(ctx, held, session)?;
            Ok(progress(session, "run"))
        }
        Phase::Ready => Ok(progress(session, "run")),
        _ => bail!("Cleanup has begun; continue release instead of prepare"),
    }
}

fn await_connection(
    ctx: &PluginContext,
    slot: &Slot,
    pid: u64,
    limit: Duration,
) -> Result<Option<Vec<Value>>> {
    let deadline = Instant::now() + limit;
    loop {
        let studios = matching_studios(&ctx.renium(&["status", "--all"])?, slot)?;
        if !studios.is_empty() {
            return Ok(Some(studios));
        }
        if !process_alive(ctx, pid)? {
            bail!("The launched Studio exited before connecting; run release to reset the slot");
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

fn clean_universe(
    pool: &Pool,
    held: &Lease,
    session: &mut Session,
    budget: &Budget,
) -> Result<bool> {
    let cloud = cloud::Client::new(pool, &session.slot)?;
    cloud.validate_slot(pool.group_id)?;
    let deadline = Instant::now() + CLEANUP_BUDGET.min(budget.remaining());
    while Instant::now() < deadline {
        let done = cloud.cleanup_step(&mut session.cleanup)?;
        save(held, session)?;
        if done {
            return Ok(true);
        }
    }
    Ok(false)
}

fn bind_project(root: &Path, slot: &Slot) -> Result<()> {
    write_new(
        &root.join("renium.experience.json"),
        &json!({"gameId":slot.universe_id,"places":{"sandbox":{"placeId":slot.place_id,"name":slot.name,"root":"project"}}}),
    )
}

fn create_blank(session: &Session) -> Result<()> {
    let root = session.directory.join("blank");
    fs::create_dir_all(root.join("project/src"))?;
    let project = root.join("project/renium.project.jsonc");
    if !project.try_exists()? {
        write_new(
            &project,
            &json!({"schemaVersion":1,"name":"Disposable blank","sourceRoot":"src"}),
        )?;
    }
    if !root.join("renium.experience.json").try_exists()? {
        bind_project(&root, &session.slot)?;
    }
    Ok(())
}

fn publish_blank(ctx: &PluginContext, pool: &Pool, held: &Lease, session: &Session) -> Result<()> {
    let mut blank = session.clone();
    blank.project = session.directory.join("blank/project/renium.project.jsonc");
    let target = target(ctx, held, &blank);
    let file = session.directory.join("blank.rbxl");
    target.renium_timeout(
        &["bep", "-o", file.to_str().context("Non-UTF8 path")?],
        EXPORT_TIMEOUT,
    )?;
    let cloud = cloud::Client::new(pool, &session.slot)?;
    cloud.validate_slot(pool.group_id)?;
    cloud.publish_blank(&file)
}

fn owned_run_root(ctx: &PluginContext, held: &Lease, session: &Session) -> Result<PathBuf> {
    let parent = fs::canonicalize(ctx.state_directory.join("runs"))?;
    let actual = fs::canonicalize(&session.directory)?;
    if actual != parent.join(&held.claim.token) || actual.parent() != Some(parent.as_path()) {
        bail!("Run directory changed; refusing to remove anything inside it");
    }
    Ok(actual)
}

/// Removes every numbered revision except the one `session.project` is bound to,
/// which is what Studio holds or is receiving. The allocated revision counter is
/// not that binding: staging can fail after the counter moves on.
fn prune_revisions(ctx: &PluginContext, held: &Lease, session: &Session) -> Result<()> {
    let root = owned_run_root(ctx, held, session)?;
    let retained = fs::canonicalize(
        session
            .project
            .parent()
            .and_then(Path::parent)
            .context("Bound project has no revision directory")?,
    )?;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        let numbered = name
            .to_str()
            .and_then(|name| name.strip_prefix("revision-"))
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
        if numbered && fs::canonicalize(entry.path())? != retained {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn load_projection(ctx: &PluginContext, held: &Lease, session: &mut Session) -> Result<()> {
    let mut source = ctx.clone();
    source.project = session.source_project.clone();
    source.place = session.source_place.clone();
    prune_revisions(ctx, held, session)?;
    session.revision += 1;
    // Persist the allocated revision before creating it; interrupted staging is never reused.
    save(held, session)?;
    let root = session
        .directory
        .join(format!("revision-{}", session.revision));
    fs::create_dir(&root)?;
    let snapshot = source.snapshot_timeout(&root.join("project"), SNAPSHOT_TIMEOUT)?;
    let project = snapshot["project"]
        .as_str()
        .context("Snapshot omitted its project")?;
    session.source_project = Some(PathBuf::from(
        snapshot["sourceProject"]
            .as_str()
            .context("Snapshot omitted source project")?,
    ));
    bind_project(&root, &session.slot)?;
    session.project = PathBuf::from(project);
    session.phase = Phase::Loading;
    save(held, session)?;
    push(ctx, held, session)
}

fn push(ctx: &PluginContext, held: &Lease, session: &mut Session) -> Result<()> {
    target(ctx, held, session)
        .renium_timeout(&["ps", "--yes", "--override-packages"], PUSH_TIMEOUT)?;
    session.phase = Phase::Ready;
    save(held, session)?;
    prune_revisions(ctx, held, session)
}

fn matching_studios(inventory: &Value, slot: &Slot) -> Result<Vec<Value>> {
    let studios = inventory["studios"]
        .as_array()
        .context("Studio inventory did not return studios; refusing to infer a free slot")?;
    Ok(studios
        .iter()
        .filter(|studio| {
            studio["placeId"].as_u64() == Some(slot.place_id)
                && studio["gameId"].as_u64() == Some(slot.universe_id)
        })
        .cloned()
        .collect())
}

fn verify_owner(studios: &[Value], pid: u64) -> Result<()> {
    if studios.len() != 1 || studios[0]["pid"].as_u64() != Some(pid) {
        bail!("Sandbox Studio ownership is ambiguous or changed; no operation was sent");
    }
    Ok(())
}

fn verify_studio(ctx: &PluginContext, session: &Session) -> Result<()> {
    let inventory = ctx.renium(&["status", "--all"])?;
    verify_owner(
        &matching_studios(&inventory, &session.slot)?,
        session.pid.context("Missing owning Studio PID")?,
    )
}

fn play_state(target: &PluginContext) -> Result<String> {
    Ok(target.renium(&["status"])?["playState"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned())
}

fn verify_stopped(ctx: &PluginContext, held: &Lease, session: &Session) -> Result<()> {
    verify_studio(ctx, session)?;
    if play_state(&target(ctx, held, session))? != "stopped" {
        bail!("Stop the sandbox Play session before replacing its contents");
    }
    Ok(())
}

fn process_alive(ctx: &PluginContext, pid: u64) -> Result<bool> {
    ctx.renium(&["plugin", "process", &pid.to_string()])?["alive"]
        .as_bool()
        .context("Process check did not report liveness; keep the slot reserved")
}

fn wait_until(limit: Duration, mut condition: impl FnMut() -> Result<bool>) -> Result<bool> {
    let deadline = Instant::now() + limit;
    loop {
        if condition()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}

fn run_arguments(options: &Value) -> Result<Vec<String>> {
    let args: Vec<String> =
        serde_json::from_str(options["args"].as_str().context("Missing JSON args")?)?;
    let command = args.first().context("args must contain a Renium command")?;
    // These commands observe or drive the reserved runtime. Publishing, sync,
    // daemon management, target overrides and package actions stay outside it.
    if !RUNTIME_COMMANDS.contains(&command.as_str()) {
        bail!(
            "'{command}' is not a sandbox runtime command. Use runtime, input or capture commands in run; use refresh to load worktree changes"
        );
    }
    for argument in &args[1..] {
        let flag = argument.split('=').next().unwrap_or_default();
        if matches!(
            flag,
            "--place"
                | "--project"
                | "--daemon"
                | "--pid"
                | "--output-mode"
                | "--ports"
                | "--context-bound"
        ) {
            bail!("Sandbox commands cannot override their target or connection");
        }
    }
    let starts_play = matches!(command.as_str(), "tst" | "test")
        || (matches!(command.as_str(), "play" | "playtest" | "start-stop-play")
            && !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "-x" | "--stop")));
    if starts_play
        && options["reason"]
            .as_str()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        bail!(
            "Starting Play needs --reason describing a runtime question that offline checks cannot answer"
        );
    }
    Ok(args)
}

fn run(
    ctx: &PluginContext,
    held: &Lease,
    session: &Session,
    options: &Value,
    budget: &Budget,
) -> Result<Value> {
    if session.phase != Phase::Ready {
        bail!("Sandbox is not ready; finish prepare first");
    }
    let args = run_arguments(options)?;
    verify_studio(ctx, session)?;
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = target(ctx, held, session).renium_timeout(&refs, budget.remaining())?;
    Ok(json!({"slot":session.slot.name,"result":result}))
}

fn release(
    ctx: &PluginContext,
    pool: &Pool,
    held: &Lease,
    session: &mut Session,
    args: &Value,
    budget: &Budget,
) -> Result<Value> {
    if session.phase == Phase::Clean {
        return finish(ctx, held, session);
    }
    if !matches!(
        session.phase,
        Phase::Closing | Phase::Resetting | Phase::Cleaning
    ) {
        session.phase = Phase::Closing;
        session.cleanup = cloud::Cleanup::default();
        save(held, session)?;
    }
    if session.phase == Phase::Closing {
        close_studio(
            ctx,
            held,
            session,
            args["confirm-closed"].as_bool() == Some(true),
        )?;
        session.phase = Phase::Resetting;
        save(held, session)?;
    }
    if session.phase == Phase::Resetting {
        if !budget.fits(PUBLISH_TIMEOUT) {
            return Ok(deferred(session));
        }
        create_blank(session)?;
        publish_blank(ctx, pool, held, session)?;
        session.phase = Phase::Cleaning;
        save(held, session)?;
    }
    if clean_universe(pool, held, session, budget)? {
        session.phase = Phase::Clean;
        save(held, session)?;
        return finish(ctx, held, session);
    }
    Ok(progress(session, "release"))
}

fn close_studio(
    ctx: &PluginContext,
    held: &Lease,
    session: &Session,
    confirmed_closed: bool,
) -> Result<()> {
    let inventory = ctx.renium(&["status", "--all"])?;
    let studios = matching_studios(&inventory, &session.slot)?;
    if !studios.is_empty() {
        let pid = session.pid.context(
            "Launch ownership was not recorded; close this test Studio yourself, then run release --confirm-closed",
        )?;
        verify_owner(&studios, pid)?;
        let target = target(ctx, held, session);
        let mut stop_requested = false;
        let deadline = Instant::now() + STOP_WAIT;
        loop {
            let state = play_state(&target)?;
            if state == "stopped" {
                break;
            }
            if state == "running" && !stop_requested {
                target.renium_timeout(&["play", "-x"], CLOSE_TIMEOUT)?;
                stop_requested = true;
            }
            if Instant::now() >= deadline {
                bail!(
                    "The sandbox Play session is still {state}; run release again once it has stopped"
                );
            }
            thread::sleep(POLL);
        }
        target.renium_timeout(&["sx", "--terminate"], CLOSE_TIMEOUT)?;
    }
    // Disconnection alone is not proof that the process ended. Ask the core's
    // read-only process check before erasing data that a late server could rewrite.
    if let Some(pid) = session.pid {
        if !wait_until(EXIT_WAIT, || Ok(!process_alive(ctx, pid)?))? {
            bail!(
                "The owned Studio process is still running; run release again once it has exited"
            );
        }
    } else if session.launch_requested && !confirmed_closed {
        bail!(
            "The launch receipt was lost, so cleanup cannot prove which Studio belongs to this slot. Close the test place yourself, then run release --confirm-closed. Unrelated places are never closed automatically"
        );
    }
    let inventory = ctx.renium(&["status", "--all"])?;
    if inventory["clients"]
        .as_array()
        .context("Missing runtime inventory")?
        .iter()
        .any(|client| client["placeId"].as_u64() == Some(session.slot.place_id))
    {
        bail!("A sandbox runtime is still connected; run release again once it has exited");
    }
    Ok(())
}

fn finish(ctx: &PluginContext, held: &Lease, session: &Session) -> Result<Value> {
    if session.directory.try_exists()? {
        fs::remove_dir_all(owned_run_root(ctx, held, session)?)?;
    }
    // Retain the Clean journal until both claims can be released. Crashes between
    // these steps never make an unclean universe reusable.
    let registry = Registry::user()?;
    if registry.read(&session.universe_claim.resource)?.is_some() {
        registry.release(&session.universe_claim)?;
    }
    registry.release(&held.claim)?;
    Ok(
        json!({"slot":session.slot.name,"released":true,"deletedEntries":session.cleanup.deleted,"historicalVersions":"Roblox retention still applies"}),
    )
}

fn progress(session: &Session, next: &str) -> Value {
    json!({"slot":session.slot.name,"phase":session.phase,"ready":session.phase == Phase::Ready,"placeId":session.slot.place_id,"next":next,"deletedEntries":session.cleanup.deleted})
}

fn deferred(session: &Session) -> Value {
    let next = if session.phase == Phase::Resetting {
        "release"
    } else {
        "prepare"
    };
    let mut value = progress(session, next);
    value["note"] =
        json!("Stopped at a saved step to stay within this command's budget; run it again");
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_commands_keep_the_sandbox_binding_and_play_needs_a_reason() {
        for args in [
            vec!["perf", "snapshot"],
            vec!["perf", "micro-start", "--frames", "256"],
            vec!["perf", "micro-stop", "--out", "capture.gprx"],
            vec!["play", "-x"],
            vec!["clk", "Button"],
            vec!["click", "Button"],
            vec!["sc"],
            vec!["l", "print(1)"],
        ] {
            let options = json!({"args":serde_json::to_string(&args).unwrap()});
            assert_eq!(run_arguments(&options).unwrap(), args);
        }
        for args in [
            vec!["perf", "snapshot", "--place=12:34"],
            vec!["perf", "--project", "elsewhere"],
            vec!["perf", "--pid", "123"],
            vec!["play", "-s"],
            vec!["tst"],
            vec!["publish"],
            vec!["ps"],
            vec!["pl"],
            vec!["dm", "stop"],
            vec![],
        ] {
            assert!(run_arguments(&json!({"args":serde_json::to_string(&args).unwrap()})).is_err());
        }
        for args in ["[\"play\",\"-s\"]", "[\"tst\",\"--mode\",\"server\"]"] {
            assert!(run_arguments(&json!({"args":args, "reason":"Verify replication"})).is_ok());
        }
    }

    #[test]
    fn long_steps_fit_their_command_budgets_before_they_start() {
        let load = Budget::new(Duration::from_secs(900));
        assert!(load.fits(LOAD_TIMEOUT + Duration::from_secs(40)));
        let release = Budget::new(Duration::from_secs(600));
        assert!(release.fits(PUBLISH_TIMEOUT + CLEANUP_BUDGET + Duration::from_secs(150)));
        assert!(!Budget::new(Duration::from_secs(20)).fits(PUSH_TIMEOUT));
        assert!(
            Budget::new(Duration::from_secs(20))
                .require(PUSH_TIMEOUT, "a push")
                .is_err()
        );
    }
}
