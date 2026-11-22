use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde_json::{Map, Value, json};

use super::{client, op};
use crate::app;
use crate::cli::BridgeConnectionArgs;
use crate::cloud;
use crate::daemon::{daemon_control_request, daemon_project_root};
use crate::project::config;
use crate::system::files::place_file_path;

const STATUS_DEADLINE: Duration = Duration::from_secs(10);
const STUDIO_CONNECT_WAIT_SECONDS: f64 = 120.0;

/// After a launch (or the reuse of an open Studio), waits until that process
/// is connected as an Edit client or gives up, so the next command finds a
/// connected runtime. `so` and `ro` return at once with `--no-wait`.
pub(crate) fn wait_for_launched_studio(
    project: Option<&Path>,
    result: &mut Value,
    wait_seconds: f64,
) -> Result<()> {
    let Some(pid) = result
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(());
    };
    let root = daemon_project_root(project);
    let started = Instant::now();
    let wait = if wait_seconds.is_finite() {
        wait_seconds.clamp(STUDIO_CONNECT_WAIT_SECONDS, 3600.0)
    } else {
        STUDIO_CONNECT_WAIT_SECONDS
    };
    let deadline = started + Duration::from_secs_f64(wait);
    let mut delay = Duration::from_millis(250);
    let mut empty_probes = 0;
    loop {
        let clients = daemon_control_request(op::STUDIOS, root, json!({}), false)?
            .get("clients")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if clients.iter().any(|client| {
            client["pid"].as_u64() == Some(u64::from(pid)) && client["role"] == "edit"
        }) {
            let object = result
                .as_object_mut()
                .context("Studio launch result must be an object")?;
            object.insert("connected".to_string(), json!(true));
            object.insert(
                "connectSeconds".to_string(),
                json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0),
            );
            return Ok(());
        }
        let diagnosis = crate::studio::diagnosis::diagnose(&clients, root);
        let verdict = diagnosis["verdict"].as_str().unwrap_or_default();
        let listed = |key: &str| {
            diagnosis[key]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["pid"] == json!(pid)))
        };
        // Two empty probes in a row distinguish an exited Studio from a
        // process listing that failed once.
        if crate::studio::diagnosis::studio_process_ids().contains(&pid) {
            empty_probes = 0;
        } else {
            empty_probes += 1;
            if empty_probes >= 2 {
                bail!("Studio (pid {pid}) exited before it connected. {verdict}");
            }
        }
        if listed("openFailures") || listed("startupHangs") {
            bail!("Studio (pid {pid}) could not open the place. {verdict}");
        }
        if let Some(held) = crate::studio::diagnosis::held_by_prompt_verdict(&diagnosis, pid) {
            bail!("{held}");
        }
        if Instant::now() >= deadline {
            bail!("Studio (pid {pid}) has not connected within {wait:.0}s. {verdict}");
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(1));
    }
}

#[derive(Args)]
pub(crate) struct StudioStatusArgs {
    #[arg(help = "Include Studios bound to other projects", long)]
    all: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct StudioReopenArgs {
    #[arg(
        value_name = "TARGET",
        help = "Place file, PLACE_ID or UNIVERSE_ID:PLACE_ID. Without it, the --place id or file, else Live Sync's place, the project's bound ids, the place last opened or closed, Live Sync's paired file, then the project's place file"
    )]
    file: Option<PathBuf>,
    #[arg(help = "Report what would open and why without launching", long)]
    check: bool,
    #[arg(
        help = "Return right after the launch instead of waiting for the place to connect",
        long
    )]
    no_wait: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct StudioCloseArgs {
    #[arg(
        help = "Save a local place file, then close",
        long,
        conflicts_with = "terminate"
    )]
    save: bool,
    #[arg(
        help = "Close without saving (unsaved local edits are lost)",
        long,
        conflicts_with = "save"
    )]
    terminate: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct MultiEditArgs {
    file: String,
    // The first pair is two values of their own: clap lets a single value
    // start with a hyphen (a Luau comment) and still reads the options after
    // it, which it cannot do for a value list.
    #[arg(value_name = "OLD", allow_hyphen_values = true)]
    old: String,
    #[arg(value_name = "NEW", allow_hyphen_values = true)]
    new: String,
    #[arg(help = "More replacements", value_names = ["OLD", "NEW"], num_args = 0..)]
    more: Vec<String>,
    #[arg(short, long)]
    all: bool,
    #[arg(short, long)]
    class: Option<String>,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct InputArgs {
    #[arg(short = 'p', long)]
    player: Option<String>,
    #[arg(
        help = "After the actions, evaluate this Luau expression (or chunk) on the same client and return it as result",
        long,
        value_name = "LUAU",
        allow_hyphen_values = true
    )]
    result: Option<String>,
    #[arg(
        help = "After the actions, screenshot the same client to this PNG path and return it as shot",
        long,
        value_name = "PATH"
    )]
    shot: Option<PathBuf>,
    #[arg(
        required_unless_present_any = ["result", "shot"],
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    actions: Vec<String>,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct PlaceAddArgs {
    place_id: i64,
    name: String,
    #[arg(long)]
    game_id: Option<i64>,
    #[arg(long)]
    alias: Option<String>,
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct PlaceRenameArgs {
    place_id: i64,
    alias: String,
}

#[derive(Args)]
pub(crate) struct PlaceReorderArgs {
    #[arg(required = true, num_args = 1..)]
    place_ids: Vec<i64>,
}

#[derive(Args)]
pub(crate) struct ImageUploadArgs {
    #[arg(required = true, num_args = 1..)]
    images: Vec<String>,
    #[arg(long)]
    user: Option<u64>,
    #[arg(long, conflicts_with = "user")]
    group: Option<u64>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value = "")]
    description: String,
    #[arg(long, default_value = "ROBLOX_API_KEY")]
    key_env: String,
    #[arg(long)]
    oauth_env: Option<String>,
    #[arg(long, default_value_t = 30.0)]
    upload_wait_seconds: f64,
    #[arg(long)]
    open_cloud: bool,
    #[arg(
        long,
        conflicts_with_all = ["open_cloud", "oauth_env"],
        help = "Upload under the Studio login, also when a key is stored"
    )]
    login: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

pub(crate) fn daemon_result(
    operation: u16,
    project: Option<&Path>,
    mut parameters: Value,
    reviewed: bool,
    bridge: Option<&BridgeConnectionArgs>,
) -> Result<Value> {
    let project = daemon_project_root(project);
    if let Some(bridge) = bridge {
        let object = parameters
            .as_object_mut()
            .context("Command parameters must be an object")?;
        object.insert("bridgeWaitSeconds".to_string(), json!(bridge.wait_seconds));
        object.insert("bridgePorts".to_string(), json!(bridge.ports));
    }
    daemon_control_request(operation, project, parameters, reviewed)
}

pub(super) fn run_daemon(
    operation: u16,
    project: Option<&Path>,
    parameters: Value,
    reviewed: bool,
    bridge: Option<&BridgeConnectionArgs>,
) -> Result<()> {
    let result = daemon_result(operation, project, parameters, reviewed, bridge)?;
    app::output::print_json_output(&result, false)
}

pub(crate) fn studio_status(args: StudioStatusArgs, project: Option<&Path>) -> Result<()> {
    let started = Instant::now();
    let deadline = STATUS_DEADLINE.max(client::status_response_limit(args.bridge.wait_seconds));
    let project = project.map(Path::to_path_buf);
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(studio_status_result(&args, project.as_deref()));
    });
    let mut result = match receiver.recv_timeout(deadline) {
        Ok(Err(error)) if client::is_response_timeout(&error) => {
            Err(client::daemon_busy_error("rbx status", started.elapsed()))
        }
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(client::daemon_busy_error("rbx status", deadline)),
        Err(RecvTimeoutError::Disconnected) => Err(anyhow!("rbx status stopped without a result")),
    }?;
    if app::output::compact_text_output() {
        // Only the listing of every Studio still carries `studios`.
        if result.get("studios").is_some() {
            result = crate::studio::automation::connected_clients_output(&result);
        } else {
            trim_status_text(&mut result);
        }
        summarize_native_operations(&mut result);
    }
    if let Some(map) = result.as_object_mut()
        && let Some(version) = app::update::cached_available_update()
    {
        map.insert("updateAvailable".to_string(), json!(version));
    }
    app::output::print_json_output(&result, false)
}

/// Text mode drops what a status repeats or leaves at zero: the top-level
/// `teamCreate` that `persistence` and the Edit client's record state, the
/// `placeVersion` of a place that was never published, a Live Sync `pending`
/// count of zero, and for one Studio the ids of its runtimes with the
/// `selected` one.
fn trim_status_text(result: &mut Value) {
    let Some(map) = result.as_object_mut() else {
        return;
    };
    if let Some(clients) = map.get_mut("clients").and_then(Value::as_array_mut)
        && crate::studio::automation::compact_one_studio_clients(clients)
    {
        map.remove("selected");
    }
    if map.contains_key("persistence") {
        map.remove("teamCreate");
    }
    if map.get("placeVersion").and_then(Value::as_i64) == Some(0) {
        map.remove("placeVersion");
    }
    if let Some(live) = map.get_mut("liveSync").and_then(Value::as_object_mut)
        && live.get("pending").and_then(Value::as_u64) == Some(0)
    {
        live.remove("pending");
    }
}

/// Text mode reports the native probe as verdict counts; which operations
/// fall back and the per-operation readings (addresses, RVAs) stay in json
/// mode and `rbx native-probe`.
fn summarize_native_operations(result: &mut Value) {
    let Some(report) = result
        .as_object_mut()
        .and_then(|map| map.remove("nativeOperations"))
    else {
        return;
    };
    let Some(operations) = report.get("operations").and_then(Value::as_array) else {
        return;
    };
    let mut counts = serde_json::Map::new();
    for operation in operations {
        let verdict = operation["verdict"].as_str().unwrap_or("unknown");
        let count = counts.entry(verdict.to_string()).or_insert(json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
    }
    result["native"] = Value::Object(counts);
}

fn studio_status_result(args: &StudioStatusArgs, project: Option<&Path>) -> Result<Value> {
    if args.all {
        return daemon_result(op::STUDIOS, None, json!({}), false, Some(&args.bridge));
    }
    match daemon_result(
        op::STUDIO_STATUS,
        project,
        json!({ "all": false }),
        false,
        Some(&args.bridge),
    ) {
        Ok(mut result) => {
            if let Some(map) = result.as_object_mut() {
                map.remove("studios");
                map.remove("studioState");
                let text = !app::output::global_json_output();
                if text {
                    map.remove("availableDataModels");
                }
                // A runtime id only selects between several Studios.
                let single = map
                    .get("clients")
                    .and_then(Value::as_array)
                    .is_some_and(|clients| clients.len() == 1);
                if single && text {
                    map.remove("selected");
                }
                for client in map
                    .get_mut("clients")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_object_mut)
                {
                    client.remove("bridgeBuildUnix");
                    client.remove("channels");
                    client.remove("ports");
                    if text {
                        for key in ["gameId", "placeId"] {
                            if client.get(key).and_then(Value::as_i64) == Some(0) {
                                client.remove(key);
                            }
                        }
                    }
                    if single {
                        client.remove("runtimeId");
                    }
                }
                // The verdict is the sentence a reader acts on; the process
                // and plugin details behind it stay in json mode and `rbx dr`.
                if text
                    && let Some(verdict) = map
                        .get("diagnosis")
                        .and_then(|diagnosis| diagnosis.get("verdict"))
                        .cloned()
                {
                    map.insert("diagnosis".to_string(), json!({ "verdict": verdict }));
                }
            }
            app::output::strip_empty(&mut result);
            Ok(result)
        }
        Err(error)
            if error
                .to_string()
                .contains("More than one Studio runtime matches this project") =>
        {
            daemon_result(op::STUDIOS, None, json!({}), false, Some(&args.bridge))
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn studio_reopen(args: StudioReopenArgs, project: Option<&Path>) -> Result<()> {
    let parameters = reopen_parameters(args.file.as_deref(), project, args.check)?;
    if args.check {
        let result = reopen_check(project, parameters)?;
        return app::output::emit_global_output(&result, &open_check_text(&result));
    }
    let mut result = daemon_result(
        op::STUDIO_OPEN,
        project,
        parameters,
        true,
        Some(&args.bridge),
    )?;
    if !args.no_wait {
        wait_for_launched_studio(project, &mut result, args.bridge.wait_seconds)?;
    }
    app::output::print_json_output(&result, false)
}

/// What `ro --check` answers: a running daemon's answer, else what `ro`
/// would open with no Studio connected, read from the project's files. It
/// never starts a daemon.
pub(crate) fn reopen_check(project: Option<&Path>, parameters: Value) -> Result<Value> {
    let root = daemon_project_root(project).unwrap_or(Path::new("."));
    match crate::daemon::try_scoped_daemon_control_request(
        op::STUDIO_OPEN,
        root,
        parameters.clone(),
    )? {
        Some(answer) => Ok(answer),
        None => crate::automation::runtime::offline_open_check(project, &parameters),
    }
}

/// What `ro` asks the daemon to open: the given file or place, else the pin.
/// The pin goes along with a given target, so the bind never takes the
/// project's ids from it; the persistent pin names what later commands in
/// this environment still carry.
pub(super) fn reopen_parameters(
    target: Option<&Path>,
    project: Option<&Path>,
    check: bool,
) -> Result<Value> {
    let mut parameters = json!({
        "pin": place_pin(project),
        "check": check,
        "persistentPin": crate::app::context::persistent_place_selector(),
    });
    if let Some(target) = target {
        match crate::automation::published_argument(target, target.is_file())? {
            Some(_) => parameters["place"] = json!(target.to_string_lossy()),
            None => parameters["file"] = json!(place_file_path(target)),
        }
    }
    Ok(parameters)
}

/// The published place or existing place file the place pin (`--place`,
/// RENIUM_PLACE or the stored setting) names for `so` and `ro`. Anything
/// else, a place the experience configures included, selects as before and
/// pins nothing here.
pub(crate) fn place_pin(project: Option<&Path>) -> Option<String> {
    let selector = crate::studio::target::place_filter()?;
    let selector = selector.trim();
    if crate::automation::published_place(selector).is_some() {
        let start = project.unwrap_or(Path::new("."));
        let configured = crate::project::experience::configures_place(start, selector);
        return (!configured).then(|| selector.to_string());
    }
    let lower = selector.to_ascii_lowercase();
    let file = place_file_path(Path::new(selector));
    ((lower.ends_with(".rbxl") || lower.ends_with(".rbxlx")) && file.is_file())
        .then(|| file.display().to_string())
}

/// What an open target names: its file, its place, or a place built from the
/// project.
pub(crate) fn target_name(target: &Value) -> String {
    match (target["file"].as_str(), target["placeId"].as_i64()) {
        (Some(file), _) => file.to_string(),
        (None, Some(place)) => format!("place {place} of universe {}", target["gameId"]),
        (None, None) => "a place built from the project".to_string(),
    }
}

fn open_check_text(result: &Value) -> String {
    let named = target_name(&result["target"]);
    let reason = result["reason"].as_str().unwrap_or_default();
    format!(
        "Would open {named} ({reason}){}",
        offline_check_line(result)
    )
}

/// The line a check answered with no daemon running adds: it looked at no
/// open Studio, which a real open may reuse or adopt.
pub(crate) fn offline_check_line(answer: &Value) -> &'static str {
    if answer["daemon"] == false {
        "\nNo daemon is running, so no open Studio was considered; a real open may reuse or adopt one."
    } else {
        ""
    }
}

pub(crate) fn studio_close(args: StudioCloseArgs, project: Option<&Path>) -> Result<()> {
    let local_action = match (args.save, args.terminate) {
        (true, false) => Some("saveAndClose"),
        (false, true) => Some("terminate"),
        _ => None,
    };
    run_daemon(
        op::STUDIO_CLOSE,
        project,
        json!({ "localAction": local_action }),
        true,
        Some(&args.bridge),
    )
}

pub(crate) fn multi_edit(args: MultiEditArgs, project: Option<&Path>) -> Result<()> {
    let (more, remainder) = args.more.as_chunks::<2>();
    if !remainder.is_empty() {
        bail!("Each OLD value needs a following NEW value");
    }
    let first = [args.old, args.new];
    let pairs: Vec<&[String; 2]> = std::iter::once(&first).chain(more).collect();
    let edits = pairs
        .iter()
        .map(|pair| {
            json!({
                "oldString": pair[0],
                "newString": pair[1],
                "replaceAll": args.all,
            })
        })
        .collect::<Vec<_>>();
    // The guide's form names the project file; Studio edits the instance it
    // maps to, and the file gets the same replacements so both sides agree
    // whether or not Live Sync is running.
    let file = script_file_for_multi_edit(&args.file, project)?;
    let target = match &file {
        Some((_, instance_path)) => instance_path.clone(),
        None => args.file.clone(),
    };
    let mut result = daemon_result(
        op::MULTI_EDIT,
        project,
        json!({
            "filePath": target,
            "className": args.class,
            "edits": edits,
        }),
        false,
        Some(&args.bridge),
    )?;
    // The path, name and class of an existing script repeat the request.
    if args.class.is_none()
        && app::output::compact_text_output()
        && let Some(map) = result.as_object_mut()
    {
        for key in ["className", "name", "path"] {
            map.remove(key);
        }
    }
    app::output::print_json_output(&result, false)?;
    if let Some((path, _)) = file {
        let mut source = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        for pair in pairs {
            source = if args.all {
                source.replace(&pair[0], &pair[1])
            } else {
                source.replacen(&pair[0], &pair[1], 1)
            };
        }
        std::fs::write(&path, source)
            .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    Ok(())
}

/// A project script file named on the command line, with the Studio instance
/// path the sourcemap maps it to; `None` when the argument is not an existing
/// file (an instance path, or a new script to create).
fn script_file_for_multi_edit(
    argument: &str,
    project: Option<&Path>,
) -> Result<Option<(PathBuf, String)>> {
    let candidate = PathBuf::from(argument);
    if !candidate.is_file() {
        return Ok(None);
    }
    let loaded = config::load_project(project, None)?;
    let root = loaded.root.clone();
    let absolute = std::fs::canonicalize(&candidate).unwrap_or(candidate.clone());
    let relative =
        crate::project::sourcemap::path_to_sourcemap_relative(&root, &absolute).to_lowercase();
    let sourcemap = match crate::project::sourcemap::load_existing_sourcemap_root(&root)? {
        Some(sourcemap) => sourcemap,
        None => {
            crate::project::sourcemap::generate_project_sourcemap(&root)?;
            crate::project::sourcemap::load_existing_sourcemap_root(&root)?
                .context("The sourcemap could not be generated")?
        }
    };
    fn search(
        node: &crate::project::sourcemap::SourcemapNode,
        relative: &str,
        trail: &mut Vec<String>,
    ) -> Option<String> {
        if node
            .file_paths
            .iter()
            .any(|path| path.replace('\\', "/").to_lowercase() == relative)
        {
            return Some(trail.join("."));
        }
        for child in &node.children {
            trail.push(child.name.clone());
            if let Some(found) = search(child, relative, trail) {
                return Some(found);
            }
            trail.pop();
        }
        None
    }
    let mut trail = Vec::new();
    let Some(instance_path) = search(&sourcemap, &relative, &mut trail) else {
        bail!(
            "{argument} is not a script the project maps to an instance; pass the instance path (ServerScriptService.Main) or -c CLASS to create a new script"
        );
    };
    Ok(Some((absolute, instance_path)))
}

fn target(value: &str) -> Result<Map<String, Value>> {
    let mut result = Map::new();
    if let Some((x, y)) = value.split_once(',') {
        result.insert("x".to_string(), json!(x.trim().parse::<i32>()?));
        result.insert("y".to_string(), json!(y.trim().parse::<i32>()?));
    } else {
        result.insert("path".to_string(), json!(value));
    }
    Ok(result)
}

fn action(name: &str) -> &'static str {
    match name {
        "kd" | "key-down" => "key-down",
        "ku" | "key-up" => "key-up",
        "key" | "kp" | "key-press" => "key-press",
        "text" | "type" => "text",
        "move" => "move",
        "down" | "mouse-down" => "mouse-down",
        "up" | "mouse-up" => "mouse-up",
        "right-down" => "mouse-down",
        "right-up" => "mouse-up",
        "click" => "click",
        "right" | "right-click" => "click",
        "scroll-up" | "su" => "scroll-up",
        "scroll-down" | "sd" => "scroll-down",
        "wait" => "wait",
        "hold" => "hold",
        "dismiss" => "dismiss",
        "press" => "press",
        _ => "",
    }
}

fn point(value: &str) -> Option<Map<String, Value>> {
    let (x, y) = value.split_once(',')?;
    let mut result = Map::new();
    result.insert("x".to_string(), json!(x.trim().parse::<i32>().ok()?));
    result.insert("y".to_string(), json!(y.trim().parse::<i32>().ok()?));
    Some(result)
}

/// Milliseconds from `300`, `300ms`, `0.3s` or `1.5`; bare fractions are seconds.
fn wait_milliseconds(value: &str) -> Result<u64> {
    let text = value.trim().to_ascii_lowercase();
    let (number, unit) = if let Some(stripped) = text.strip_suffix("ms") {
        (stripped, "ms")
    } else if let Some(stripped) = text.strip_suffix('s') {
        (stripped, "s")
    } else {
        (text.as_str(), "")
    };
    let number: f64 = number.trim().parse().with_context(|| {
        format!("wait takes a duration such as 300, 300ms or 0.3s, not '{value}'")
    })?;
    if !number.is_finite() || number < 0.0 {
        bail!("wait takes a non-negative duration, not '{value}'");
    }
    let milliseconds = match unit {
        "ms" => number,
        "s" => number * 1000.0,
        _ if number.fract() != 0.0 => number * 1000.0,
        _ => number,
    };
    Ok(milliseconds.round() as u64)
}

fn input_actions(tokens: &[String]) -> Result<Vec<Value>> {
    let mut actions = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let command = tokens[index].as_str();
        let kind = action(command);
        if kind.is_empty() {
            bail!("Unknown input action '{command}'");
        }
        if kind == "dismiss" {
            let mut entry = Map::new();
            entry.insert("action".to_string(), json!(kind));
            index += 1;
            if let Some(position) = tokens.get(index).and_then(|value| point(value)) {
                entry.extend(position);
                index += 1;
            }
            actions.push(Value::Object(entry));
            continue;
        }
        let value = tokens
            .get(index + 1)
            .with_context(|| format!("Input action '{command}' needs a value"))?;
        if kind == "hold" {
            let duration = tokens
                .get(index + 2)
                .with_context(|| "hold needs a key and a duration, e.g. hold W 3000")?;
            let ms = wait_milliseconds(duration)?;
            actions.push(json!({ "action": "key-down", "key": value }));
            actions.push(json!({ "action": "wait", "ms": ms }));
            actions.push(json!({ "action": "key-up", "key": value }));
            index += 3;
            continue;
        }
        let mut entry = Map::new();
        entry.insert("action".to_string(), json!(kind));
        match kind {
            "key-down" | "key-up" | "key-press" => {
                entry.insert("key".to_string(), json!(value));
            }
            "text" => {
                entry.insert("text".to_string(), json!(value));
            }
            "wait" => {
                entry.insert("ms".to_string(), json!(wait_milliseconds(value)?));
            }
            "press" => match point(value) {
                Some(position) => entry.extend(position),
                None => {
                    entry.insert("text".to_string(), json!(value));
                }
            },
            _ => entry.extend(target(value)?),
        }
        if matches!(command, "right" | "right-click" | "right-down" | "right-up") {
            entry.insert("button".to_string(), json!("right"));
        }
        actions.push(Value::Object(entry));
        index += 2;
    }
    Ok(actions)
}

/// The `inp` options written after the actions, where clap keeps them as
/// action tokens: `inp -p 1 click Close wait 600 --shot out.png --result 'return 1'`
/// reads the same as the option-first order.
#[derive(Default, PartialEq, Debug)]
struct TrailingInputOptions {
    actions: Vec<String>,
    player: Option<String>,
    result: Option<String>,
    shot: Option<PathBuf>,
}

fn lift_trailing_input_options(tokens: &[String]) -> Result<TrailingInputOptions> {
    let mut lifted = TrailingInputOptions::default();
    let mut iter = tokens.iter();
    while let Some(token) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .with_context(|| format!("{token} needs a value"))
        };
        match token.as_str() {
            "--result" => lifted.result = Some(value()?),
            "--player" | "-p" => lifted.player = Some(value()?),
            "--shot" => lifted.shot = Some(PathBuf::from(value()?)),
            _ => lifted.actions.push(token.clone()),
        }
    }
    Ok(lifted)
}

pub(crate) fn input(mut args: InputArgs, project: Option<&Path>) -> Result<()> {
    let lifted = lift_trailing_input_options(&args.actions)?;
    args.player = lifted.player.or(args.player);
    args.result = lifted.result.or(args.result);
    args.shot = lifted.shot.or(args.shot);
    // Without actions the call only looks: `inp -p 1 --shot out.png`.
    let mut result = if lifted.actions.is_empty() {
        json!({ "ok": true, "actions": 0 })
    } else {
        daemon_result(
            op::INPUT,
            project,
            json!({ "player": args.player, "actions": input_actions(&lifted.actions)? }),
            false,
            Some(&args.bridge),
        )?
    };
    let mut requested_shot = None;
    // The follow-ups run on the same client once the actions are in, so one
    // call covers "act, then look": a typed observation and a screenshot.
    if result.get("ok") != Some(&Value::Bool(false)) {
        if let Some(code) = args.result.as_deref() {
            let observed = daemon_result(
                op::LUAU,
                project,
                json!({
                    "code": crate::studio::automation::returning_luau_body(code, "result expression")?,
                    "client": true,
                    "player": args.player,
                    "timeout": 10,
                    "bridgeWaitSeconds": args.bridge.wait_seconds,
                    "bridgePorts": args.bridge.ports,
                }),
                false,
                Some(&args.bridge),
            )?;
            result["result"] = observed
                .get("results")
                .and_then(Value::as_array)
                .and_then(|values| values.first().cloned())
                .unwrap_or(Value::Null);
        }
        if let Some(path) = args.shot.as_ref() {
            let output = crate::studio::automation::absolute_from_cwd(path.clone());
            let shot = daemon_result(
                op::SHOT,
                project,
                json!({
                    "output": output,
                    "player": args.player,
                    "client": true,
                    "bridgeWaitSeconds": args.bridge.wait_seconds,
                    "bridgePorts": args.bridge.ports,
                }),
                false,
                Some(&args.bridge),
            )?;
            result["shot"] = shot.get("path").cloned().unwrap_or(json!(output));
            requested_shot = Some(json!(output));
        }
    }
    if app::output::compact_text_output() {
        crate::studio::automation::compact_input_result(
            op::INPUT,
            &mut result,
            requested_shot.as_ref(),
        );
    }
    app::output::print_json_output(&result, false)
}

pub(crate) fn place_add(args: PlaceAddArgs, project: Option<&Path>) -> Result<()> {
    run_daemon(
        op::PLACE_ADD,
        project,
        json!({
            "placeId": args.place_id,
            "name": args.name,
            "gameId": args.game_id,
            "alias": args.alias,
            "root": args.root,
        }),
        false,
        None,
    )
}

pub(crate) fn place_rename(args: PlaceRenameArgs, project: Option<&Path>) -> Result<()> {
    run_daemon(
        op::PLACE_RENAME,
        project,
        json!({ "placeId": args.place_id, "alias": args.alias }),
        false,
        None,
    )
}

pub(crate) fn place_reorder(args: PlaceReorderArgs, project: Option<&Path>) -> Result<()> {
    run_daemon(
        op::PLACE_REORDER,
        project,
        json!({ "order": args.place_ids }),
        false,
        None,
    )
}

pub(crate) fn image_upload(args: ImageUploadArgs, project: Option<&Path>) -> Result<()> {
    if args.user == Some(0) || args.group == Some(0) {
        bail!("Creator IDs must be greater than zero");
    }
    let keyed =
        !args.login && cloud::assets::has_credential(&args.key_env, args.oauth_env.as_deref());
    let parameters = json!({
        "images": args.images,
        "userId": args.user,
        "groupId": args.group,
        "name": args.name,
        "description": args.description,
        "keyEnv": args.key_env,
        "oauthEnv": args.oauth_env,
        "waitSeconds": args.upload_wait_seconds,
        "via": upload_route(args.login, args.open_cloud, keyed),
    });
    let named_creator = args.user.is_some() || args.group.is_some();
    if named_creator && keyed {
        let project = daemon_project_root(project);
        let root = config::try_load_project(project, None)?
            .map_or_else(std::env::current_dir, |loaded| Ok(loaded.root))?;
        let result =
            cloud::assets::upload(&root, &parameters, None).map_err(cloud::command::cloud_error)?;
        return app::output::print_json_output(&result, false);
    }
    run_daemon(
        op::IMAGE_UPLOAD,
        project,
        parameters,
        false,
        Some(&args.bridge),
    )
}

/// The route an upload asks for: the Studio login when asked, Open Cloud
/// when asked or when the caller holds a credential (`keyed`), which the
/// background process may not see, and otherwise whichever is there.
fn upload_route(login: bool, open_cloud: bool, keyed: bool) -> Option<&'static str> {
    if login {
        Some("login")
    } else {
        (open_cloud || keyed).then_some("open-cloud")
    }
}

#[cfg(test)]
mod input_action_tests {
    use super::*;

    #[test]
    fn an_upload_with_a_credential_at_hand_never_asks_for_the_login() {
        assert_eq!(upload_route(true, false, false), Some("login"));
        assert_eq!(upload_route(false, true, false), Some("open-cloud"));
        assert_eq!(upload_route(false, false, true), Some("open-cloud"));
        assert_eq!(upload_route(false, false, false), None);
    }

    #[test]
    fn wait_durations_accept_milliseconds_and_seconds() {
        assert_eq!(wait_milliseconds("300").unwrap(), 300);
        assert_eq!(wait_milliseconds("300ms").unwrap(), 300);
        assert_eq!(wait_milliseconds("0.3s").unwrap(), 300);
        assert_eq!(wait_milliseconds("1.5").unwrap(), 1500);
        assert_eq!(wait_milliseconds("2s").unwrap(), 2000);
        assert!(wait_milliseconds("fast").is_err());
        assert!(wait_milliseconds("-1").is_err());
    }

    #[test]
    fn hold_expands_to_a_press_with_a_wait() {
        let actions = input_actions(&[
            "hold".into(),
            "W".into(),
            "0.5s".into(),
            "ku".into(),
            "D".into(),
        ])
        .unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[0], json!({ "action": "key-down", "key": "W" }));
        assert_eq!(actions[1], json!({ "action": "wait", "ms": 500 }));
        assert_eq!(actions[2], json!({ "action": "key-up", "key": "W" }));
        assert_eq!(actions[3], json!({ "action": "key-up", "key": "D" }));
        assert!(input_actions(&["hold".into(), "W".into()]).is_err());
    }

    #[test]
    fn dismiss_takes_an_optional_point_and_press_a_label_or_point() {
        let tokens = [
            "dismiss", "key", "E", "dismiss", "40,700", "press", "No", "press", "545, 442",
        ]
        .map(String::from);
        let actions = input_actions(&tokens).unwrap();
        assert_eq!(actions[0], json!({ "action": "dismiss" }));
        assert_eq!(actions[1], json!({ "action": "key-press", "key": "E" }));
        assert_eq!(
            actions[2],
            json!({ "action": "dismiss", "x": 40, "y": 700 })
        );
        assert_eq!(actions[3], json!({ "action": "press", "text": "No" }));
        assert_eq!(actions[4], json!({ "action": "press", "x": 545, "y": 442 }));
        assert_eq!(
            input_actions(&["dismiss".into()]).unwrap(),
            vec![json!({ "action": "dismiss" })]
        );
        assert!(input_actions(&["press".into()]).is_err());
    }

    #[test]
    fn options_after_the_actions_are_lifted_out() {
        let tokens = [
            "click",
            "Main.Close",
            "wait",
            "600",
            "--shot",
            "out.png",
            "--result",
            "return 1",
            "-p",
            "2",
        ]
        .map(String::from);
        assert_eq!(
            lift_trailing_input_options(&tokens).unwrap(),
            TrailingInputOptions {
                actions: ["click", "Main.Close", "wait", "600"]
                    .map(String::from)
                    .to_vec(),
                player: Some("2".into()),
                result: Some("return 1".into()),
                shot: Some(PathBuf::from("out.png")),
            }
        );
        assert_eq!(
            lift_trailing_input_options(&["key".into(), "E".into()]).unwrap(),
            TrailingInputOptions {
                actions: vec!["key".into(), "E".into()],
                ..TrailingInputOptions::default()
            }
        );
        assert!(lift_trailing_input_options(&["key".into(), "E".into(), "--shot".into()]).is_err());
    }
}

#[cfg(test)]
mod native_summary_tests {
    use super::*;

    #[test]
    fn status_text_mode_keeps_verdict_counts_only() {
        let mut result = json!({"ok": true, "nativeOperations": {"pid": 5, "readOnly": true, "operations": [
            {"operation": "snapshot", "verdict": "native", "reason": "ok", "fallback": "plugin", "readings": {"base": 1}},
            {"operation": "upload", "verdict": "degraded", "reason": "layout moved", "fallback": "plugin upload", "readings": {"base": 2}}
        ]}});
        summarize_native_operations(&mut result);
        assert!(result.get("nativeOperations").is_none());
        assert_eq!(result["native"], json!({"native": 1, "degraded": 1}));
    }

    #[test]
    fn status_text_mode_lists_one_studio_without_runtime_ids() {
        let mut playing = json!({
            "clients": [
                {"pid": 5, "placeName": "place.rbxl", "role": "edit", "runtimeId": "a", "teamCreate": false},
                {"launchEditRuntimeId": "a", "launchNonce": "n", "pid": 5, "placeName": "place.rbxl",
                    "playerName": "Builder", "playerUserId": 7, "role": "play-client", "runtimeId": "b"},
                {"launchEditRuntimeId": "a", "launchNonce": "n", "pid": 5, "placeName": "place.rbxl",
                    "role": "play-server", "runtimeId": "c"}
            ],
            "persistence": "unsaved", "playState": "running", "selected": "a"
        });
        trim_status_text(&mut playing);
        assert_eq!(
            playing,
            json!({
                "clients": [
                    {"pid": 5, "placeName": "place.rbxl", "role": "edit", "teamCreate": false},
                    {"playerName": "Builder", "role": "play-client"},
                    {"role": "play-server"}
                ],
                "persistence": "unsaved", "playState": "running"
            })
        );
        let mut two = json!({
            "clients": [
                {"pid": 5, "placeName": "One", "role": "edit", "runtimeId": "a"},
                {"pid": 6, "placeName": "Two", "role": "edit", "runtimeId": "b"}
            ],
            "selected": "a"
        });
        let before = two.clone();
        trim_status_text(&mut two);
        assert_eq!(two, before);
    }

    #[test]
    fn status_text_mode_drops_repeated_and_zero_fields() {
        let mut local = json!({
            "clients": [{"pid": 5, "placeName": "place.rbxl", "role": "edit", "teamCreate": false}],
            "liveSync": {"pending": 0, "running": true},
            "persistence": "unsaved", "placeVersion": 0, "playState": "stopped", "teamCreate": false
        });
        trim_status_text(&mut local);
        assert_eq!(
            local,
            json!({
                "clients": [{"pid": 5, "placeName": "place.rbxl", "role": "edit", "teamCreate": false}],
                "liveSync": {"running": true},
                "persistence": "unsaved", "playState": "stopped"
            })
        );
        let mut published = json!({
            "liveSync": {"pending": 2, "running": true, "see": "rbx lst --details"},
            "placeVersion": 2924, "playState": "running", "teamCreate": true
        });
        let before = published.clone();
        trim_status_text(&mut published);
        assert_eq!(published, before);
    }

    #[test]
    fn a_listing_of_every_studio_prints_as_cs_does() {
        let listing = json!({
            "selected": null,
            "studios": [{"runtimeId": "a", "role": "edit"}, {"runtimeId": "b", "role": "edit"}],
            "clients": [
                {"runtimeId": "a", "role": "edit", "placeName": "One", "placeId": 0, "gameId": 0,
                    "teamCreate": false, "pid": 1, "bridgeBuildUnix": 9, "channels": 2, "ports": [1, 2]},
                {"runtimeId": "b", "role": "edit", "placeName": "Two", "placeId": 7, "gameId": 8,
                    "teamCreate": true, "pid": 2, "bridgeBuildUnix": 9, "channels": 1, "ports": [3]}
            ],
            "orphans": [{"pid": 3}]
        });
        assert_eq!(
            crate::studio::automation::connected_clients_output(&listing),
            json!({
                "clients": [
                    {"runtimeId": "a", "role": "edit", "placeName": "One", "teamCreate": false, "pid": 1},
                    {"runtimeId": "b", "role": "edit", "placeName": "Two", "placeId": 7, "gameId": 8,
                        "teamCreate": true, "pid": 2}
                ],
                "orphans": [{"pid": 3}]
            })
        );
    }
}

#[cfg(test)]
mod open_check_tests {
    use super::*;

    #[test]
    fn check_names_the_target_and_the_reason() {
        let place =
            json!({"ok": true, "reason": "--place", "target": {"gameId": 4567, "placeId": 123}});
        assert_eq!(
            open_check_text(&place),
            "Would open place 123 of universe 4567 (--place)"
        );
        let file = json!({"ok": true, "reason": "given", "target": {"file": "E:/a.rbxl"}});
        assert_eq!(open_check_text(&file), "Would open E:/a.rbxl (given)");
        let build = json!({"ok": true, "reason": "project file", "target": null});
        assert_eq!(
            open_check_text(&build),
            "Would open a place built from the project (project file)"
        );
    }

    #[test]
    fn ro_asks_for_the_given_file_or_place_else_the_place_pin() {
        let root = crate::tests::support::temp_dir("ro-parameters");
        let file = root.join("Place.rbxl");
        std::fs::write(&file, []).unwrap();
        let asked = |pin: &str, target: Option<&Path>| {
            let _selection = app::context::select_automation_for_thread(
                None,
                PathBuf::new(),
                Some(pin.to_string()),
            );
            reopen_parameters(target, Some(root.as_path()), false)
                .map_err(|error| error.to_string())
        };
        assert_eq!(
            asked("123", Some(&file)),
            Ok(
                json!({ "file": place_file_path(&file), "pin": "123", "check": false, "persistentPin": null })
            )
        );
        assert_eq!(
            asked("10:123", Some(Path::new("4567:123"))),
            Ok(
                json!({ "place": "4567:123", "pin": "10:123", "check": false, "persistentPin": null })
            )
        );
        assert!(
            asked("123", Some(Path::new("+123")))
                .is_err_and(|error| error.starts_with("\"+123\" is not a place"))
        );
        for pin in ["123", "4567:123"] {
            assert_eq!(
                asked(pin, None),
                Ok(json!({ "pin": pin, "check": false, "persistentPin": null }))
            );
        }
        for pin in [
            "lobby",
            "Place1 - Roblox Studio",
            "Place1.rbxl",
            "places/missing.rbxl",
            "+123",
            "99999999999999999999",
        ] {
            assert_eq!(
                asked(pin, None),
                Ok(json!({ "pin": null, "check": false, "persistentPin": null }))
            );
        }
        assert!(
            asked(&file.display().to_string(), None).is_ok_and(|asked| asked["pin"].is_string())
        );
        std::fs::create_dir_all(root.join("lobby")).unwrap();
        std::fs::write(
            root.join("renium.experience.json"),
            json!({ "gameId": 4567, "places": { "lobby": { "root": "lobby", "placeId": 123 } } })
                .to_string(),
        )
        .unwrap();
        for (pin, asks) in [
            ("123", None),
            ("4567:123", None),
            ("4567:9", Some("4567:9")),
        ] {
            assert_eq!(
                asked(pin, None),
                Ok(json!({ "pin": asks, "check": false, "persistentPin": null }))
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ro_check_answers_from_the_project_files_and_never_starts_a_daemon() {
        let experience = crate::tests::support::temp_dir("ro-check-offline");
        let root = experience.join("main");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".renium")).unwrap();
        std::fs::create_dir_all(experience.join("lobby")).unwrap();
        std::fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
        )
        .unwrap();
        let places =
            json!({ "main": { "root": "main" }, "lobby": { "root": "lobby", "placeId": 123 } });
        std::fs::write(
            experience.join("renium.experience.json"),
            json!({ "gameId": 77, "places": places }).to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(".renium/live-watch-state.enabled"),
            json!({ "version": 1, "target": "published:77:5" }).to_string(),
        )
        .unwrap();
        let _selection = app::context::select_automation_for_thread(
            None,
            root.join("renium.project.jsonc"),
            None,
        );
        crate::daemon::TEST_DAEMON.set(Some(|_, _| Value::Null));
        let check = |target: Option<&str>| {
            let parameters = reopen_parameters(target.map(Path::new), Some(&root), true).unwrap();
            reopen_check(Some(&root), parameters)
                .map(|answer| (answer["reason"].clone(), answer["target"].clone()))
                .map_err(|error| error.to_string())
        };
        let answers = [
            check(None),
            check(Some("123")),
            check(Some("10:9")),
            check(Some("9")),
        ];
        let offline = reopen_check(
            Some(&root),
            reopen_parameters(None, Some(&root), true).unwrap(),
        )
        .unwrap();
        crate::daemon::TEST_DAEMON.set(None);
        assert_eq!(
            open_check_text(&offline),
            "Would open place 5 of universe 77 (live sync)\nNo daemon is running, so no open Studio was considered; a real open may reuse or adopt one."
        );
        assert_eq!(offline["daemon"], false);
        assert_eq!(
            answers,
            [
                Ok((json!("live sync"), json!({ "gameId": 77, "placeId": 5 }))),
                Ok((json!("given"), json!({ "gameId": 77, "placeId": 123 }))),
                Ok((json!("given"), json!({ "gameId": 10, "placeId": 9 }))),
                Err(
                    "Renium does not know the universe of place 9; pass UNIVERSE_ID:9.".to_string()
                ),
            ]
        );
        for folder in [&experience, &root] {
            assert!(
                !folder.join("RENIUM.md").exists(),
                "a check writes no project instructions"
            );
        }
        std::fs::remove_dir_all(experience).unwrap();
    }
}
