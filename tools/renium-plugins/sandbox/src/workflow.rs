use crate::{Phase, Pool, Session, Slot, cloud, operation_lock, save, target, write_new};
use renium_plugin_sdk::{
    Context, PluginContext, Result, Value, bail, json,
    lease::{Lease, Registry},
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
        "No available sandbox slot{}; active or unclean slots are never taken over",
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
) -> Result<Value> {
    match command {
        "prepare" => prepare(ctx, pool, held, session),
        "refresh" => {
            if session.phase != Phase::Ready {
                bail!("Finish prepare before refreshing");
            }
            verify_stopped(ctx, held, session)?;
            load_projection(ctx, held, session)?;
            Ok(progress(session, "run"))
        }
        "run" => run(ctx, held, session, args),
        "release" => release(ctx, pool, held, session),
        _ => bail!("Unknown sandbox command"),
    }
}

fn prepare(ctx: &PluginContext, pool: &Pool, held: &Lease, session: &mut Session) -> Result<Value> {
    match session.phase {
        Phase::Reserved => {
            ctx.renium(&["plugin", "runtime-check"])?;
            cloud::Client::new(pool, &session.slot)?.validate_slot(pool.group_id)?;
            create_blank(session)?;
            publish_blank(ctx, pool, held, session)?;
            session.phase = Phase::Sanitizing;
            save(held, session)?;
            Ok(progress(session, "prepare"))
        }
        Phase::Sanitizing => {
            let cloud = cloud::Client::new(pool, &session.slot)?;
            cloud.validate_slot(pool.group_id)?;
            let deadline = Instant::now() + Duration::from_secs(12);
            while Instant::now() < deadline {
                if cloud.cleanup_step(&mut session.cleanup)? {
                    session.phase = Phase::BlankPublished;
                    session.cleanup = cloud::Cleanup::default();
                    save(held, session)?;
                    break;
                }
                save(held, session)?;
            }
            Ok(progress(session, "prepare"))
        }
        Phase::BlankPublished => {
            let inventory = ctx.renium(&["status", "--all"])?;
            if !matching_studios(&inventory, &session.slot)?.is_empty() {
                bail!("The slot was opened outside this workflow; no Studio was changed");
            }
            // Persist intent first. Lost launch responses are not blindly replayed.
            session.phase = Phase::Launching;
            session.launch_requested = true;
            save(held, session)?;
            let result = target(ctx, held, session).renium(&["ro"])?;
            if result["alreadyOpen"].as_bool() == Some(true) {
                bail!(
                    "Renium found an existing Studio instead of opening an owned one; slot remains reserved"
                );
            }
            session.pid = Some(result["pid"].as_u64().filter(|pid| *pid > 0).context(
                "Studio launch did not return its owning PID; keep the slot reserved for recovery",
            )?);
            save(held, session)?;
            Ok(progress(session, "prepare"))
        }
        Phase::Launching => {
            let pid = session.pid.context("Launch response was lost; do not adopt an arbitrary Studio. Close the test place manually, then release the slot")?;
            let inventory = ctx.renium(&["status", "--all"])?;
            let matches = matching_studios(&inventory, &session.slot)?;
            if matches.is_empty() {
                return Ok(
                    json!({"slot":session.slot.name,"phase":"launching","ready":false,"pid":pid,"next":"Run prepare when Studio has connected; do not poll continuously"}),
                );
            }
            verify_owner(&matches, pid)?;
            verify_stopped(ctx, held, session)?;
            load_projection(ctx, held, session)?;
            Ok(progress(session, "run"))
        }
        Phase::Loading => {
            // A previous push may already have committed. An explicit repeat reconciles
            // that exact saved projection, never an arbitrary mutation or play request.
            verify_stopped(ctx, held, session)?;
            target(ctx, held, session).renium(&["ps", "--yes", "--override-packages"])?;
            session.phase = Phase::Ready;
            save(held, session)?;
            Ok(progress(session, "run"))
        }
        Phase::Ready => Ok(progress(session, "run")),
        _ => bail!("Cleanup has begun; continue release instead of prepare"),
    }
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
    target.renium(&["bep", "-o", file.to_str().context("Non-UTF8 path")?])?;
    let cloud = cloud::Client::new(pool, &session.slot)?;
    cloud.validate_slot(pool.group_id)?;
    cloud.publish_blank(&file)
}

fn load_projection(ctx: &PluginContext, held: &Lease, session: &mut Session) -> Result<()> {
    let mut source = ctx.clone();
    source.project = session.source_project.clone();
    source.place = session.source_place.clone();
    session.revision += 1;
    // Persist the allocated revision before creating it; interrupted staging is never reused.
    save(held, session)?;
    let root = session
        .directory
        .join(format!("revision-{}", session.revision));
    fs::create_dir(&root)?;
    let snapshot = source.snapshot(&root.join("project"))?;
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
    target(ctx, held, session).renium(&["ps", "--yes", "--override-packages"])?;
    session.phase = Phase::Ready;
    save(held, session)
}

fn matching_studios<'a>(inventory: &'a Value, slot: &Slot) -> Result<Vec<&'a Value>> {
    let studios = inventory["studios"]
        .as_array()
        .context("Studio inventory did not return studios; refusing to infer a free slot")?;
    Ok(studios
        .iter()
        .filter(|studio| {
            studio["placeId"].as_u64() == Some(slot.place_id)
                && studio["gameId"].as_u64() == Some(slot.universe_id)
        })
        .collect())
}

fn verify_owner(studios: &[&Value], pid: u64) -> Result<()> {
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

fn verify_stopped(ctx: &PluginContext, held: &Lease, session: &Session) -> Result<()> {
    verify_studio(ctx, session)?;
    let status = target(ctx, held, session).renium(&["status"])?;
    if status["playState"].as_str() != Some("stopped") {
        bail!("Stop the sandbox Play session before replacing its contents");
    }
    Ok(())
}

fn run(ctx: &PluginContext, held: &Lease, session: &Session, options: &Value) -> Result<Value> {
    if session.phase != Phase::Ready {
        bail!("Sandbox is not ready; finish prepare first");
    }
    let args: Vec<String> =
        serde_json::from_str(options["args"].as_str().context("Missing JSON args")?)?;
    let command = args.first().context("args must contain a Renium command")?;
    // These commands operate on the reserved runtime. Place publishing, daemon management,
    // target overrides and package publishing are deliberately outside a disposable test.
    if !matches!(
        command.as_str(),
        "status"
            | "net"
            | "play"
            | "cs"
            | "l"
            | "lc"
            | "co"
            | "sc"
            | "rs"
            | "re"
            | "rf"
            | "ui"
            | "inp"
            | "click"
            | "key"
            | "press"
            | "goto"
            | "type"
            | "device"
    ) {
        bail!("Use a test/capture command in run; use refresh to load worktree changes");
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
    if command == "play"
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-x" | "--stop"))
        && options["reason"]
            .as_str()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        bail!(
            "Starting Play needs --reason describing a runtime question that offline checks cannot answer"
        );
    }
    verify_studio(ctx, session)?;
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let result = target(ctx, held, session).renium(&refs)?;
    Ok(json!({"slot":session.slot.name,"result":result}))
}

fn release(ctx: &PluginContext, pool: &Pool, held: &Lease, session: &mut Session) -> Result<Value> {
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
        let inventory = ctx.renium(&["status", "--all"])?;
        let studios = matching_studios(&inventory, &session.slot)?;
        if !studios.is_empty() {
            verify_owner(&studios, session.pid.context("Launch ownership was not recorded; close this test Studio manually before release")?)?;
            let target = target(ctx, held, session);
            let status = target.renium(&["status"])?;
            match status["playState"].as_str() {
                Some("stopped") => {}
                Some("running") => {
                    target.renium(&["play", "-x"])?;
                    return Ok(progress(session, "release"));
                }
                _ => {
                    bail!("Sandbox play state is transitional or unknown; cleanup remains reserved")
                }
            }
            target.renium(&["sx", "--terminate"])?;
            return Ok(progress(session, "release"));
        }
        // Disconnection alone is not proof that the process ended. Ask the core's
        // read-only process check before erasing data that a late server could rewrite.
        if let Some(pid) = session.pid {
            let state = ctx.renium(&["plugin", "process", &pid.to_string()])?;
            if state["alive"].as_bool() != Some(false) {
                bail!(
                    "The owned Studio process is still alive; keep the slot reserved until it exits"
                );
            }
        } else if session.launch_requested {
            bail!(
                "The launch response was lost. Cleanup cannot prove which process belongs to this slot; keep it reserved for manual ownership recovery. Do not close unrelated places automatically."
            );
        }
        if inventory["clients"]
            .as_array()
            .context("Missing runtime inventory")?
            .iter()
            .any(|client| client["placeId"].as_u64() == Some(session.slot.place_id))
        {
            bail!("A sandbox runtime is still connected; cleanup remains reserved until it exits");
        }
        session.phase = Phase::Resetting;
        save(held, session)?;
    }
    if session.phase == Phase::Resetting {
        create_blank(session)?;
        publish_blank(ctx, pool, held, session)?;
        session.phase = Phase::Cleaning;
        save(held, session)?;
        return Ok(progress(session, "release"));
    }
    let cloud = cloud::Client::new(pool, &session.slot)?;
    cloud.validate_slot(pool.group_id)?;
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if cloud.cleanup_step(&mut session.cleanup)? {
            session.phase = Phase::Clean;
            save(held, session)?;
            return finish(ctx, held, session);
        }
        save(held, session)?;
    }
    Ok(progress(session, "release"))
}

fn finish(ctx: &PluginContext, held: &Lease, session: &Session) -> Result<Value> {
    let parent = fs::canonicalize(ctx.state_directory.join("runs"))?;
    let expected = parent.join(&held.claim.token);
    if session.directory.try_exists()? {
        let actual = fs::canonicalize(&session.directory)?;
        if actual != expected || actual.parent() != Some(parent.as_path()) {
            bail!("Cleanup directory changed; refusing to remove it");
        }
        fs::remove_dir_all(actual)?;
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
