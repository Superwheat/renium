use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Map, Value, json};

use super::op;
use crate::app;
use crate::cli::BridgeConnectionArgs;
use crate::cloud;
use crate::daemon::{daemon_project_root, start_shared_daemon, try_daemon_control_request};
use crate::project::config;

#[derive(Args)]
pub(crate) struct StudioStatusArgs {
    #[arg(long)]
    all: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct StudioReopenArgs {
    file: Option<PathBuf>,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct StudioCloseArgs {
    #[arg(long, conflicts_with = "terminate")]
    save: bool,
    #[arg(long, conflicts_with = "save")]
    terminate: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Args)]
pub(crate) struct MultiEditArgs {
    file: String,
    #[arg(required = true, num_args = 2.., value_names = ["OLD", "NEW"])]
    edits: Vec<String>,
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
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
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
    let result = match try_daemon_control_request(operation, project, parameters.clone(), reviewed)?
    {
        Some(result) => result,
        None => {
            let (ports, wait) = bridge
                .map(|bridge| (bridge.ports.as_str(), bridge.wait_seconds))
                .unwrap_or(("8781,8782", 1.0));
            if !start_shared_daemon(ports, wait) {
                bail!("Could not start the Renium daemon");
            }
            try_daemon_control_request(operation, project, parameters, reviewed)?
                .context("The Renium daemon did not accept the command")?
        }
    };
    Ok(result)
}

pub(super) fn run_daemon(
    operation: u16,
    project: Option<&Path>,
    parameters: Value,
    reviewed: bool,
    bridge: Option<&BridgeConnectionArgs>,
) -> Result<()> {
    let result = daemon_result(operation, project, parameters, reviewed, bridge)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub(crate) fn studio_status(args: StudioStatusArgs, project: Option<&Path>) -> Result<()> {
    run_daemon(
        op::STUDIO_STATUS,
        project,
        json!({ "all": args.all }),
        false,
        Some(&args.bridge),
    )
}

pub(crate) fn studio_reopen(args: StudioReopenArgs, project: Option<&Path>) -> Result<()> {
    run_daemon(
        op::STUDIO_OPEN,
        project,
        json!({ "file": args.file }),
        true,
        Some(&args.bridge),
    )
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
    let pairs = args.edits.chunks_exact(2);
    if !pairs.remainder().is_empty() {
        bail!("Each OLD value needs a following NEW value");
    }
    let edits = pairs
        .map(|pair| {
            json!({
                "oldString": pair[0],
                "newString": pair[1],
                "replaceAll": args.all,
            })
        })
        .collect::<Vec<_>>();
    run_daemon(
        op::MULTI_EDIT,
        project,
        json!({
            "filePath": args.file,
            "className": args.class,
            "edits": edits,
        }),
        false,
        Some(&args.bridge),
    )
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
        _ => "",
    }
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
        let value = tokens
            .get(index + 1)
            .with_context(|| format!("Input action '{command}' needs a value"))?;
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
                entry.insert("ms".to_string(), json!(value.parse::<u64>()?));
            }
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

pub(crate) fn input(args: InputArgs, project: Option<&Path>) -> Result<()> {
    let actions = input_actions(&args.actions)?;
    run_daemon(
        op::INPUT,
        project,
        json!({ "player": args.player, "actions": actions }),
        false,
        Some(&args.bridge),
    )
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
    let parameters = json!({
        "images": args.images,
        "userId": args.user,
        "groupId": args.group,
        "name": args.name,
        "description": args.description,
        "keyEnv": args.key_env,
        "oauthEnv": args.oauth_env,
        "waitSeconds": args.upload_wait_seconds,
        "via": args.open_cloud.then_some("open-cloud"),
    });
    if args.user.is_some() || args.group.is_some() {
        let project = daemon_project_root(project);
        let root = config::try_load_project(project, None)?
            .map_or_else(std::env::current_dir, |loaded| Ok(loaded.root))?;
        let result =
            cloud::assets::upload(&root, &parameters, None).map_err(cloud::command::cloud_error)?;
        return app::output::print_json_output(&result, true);
    }
    run_daemon(
        op::IMAGE_UPLOAD,
        project,
        parameters,
        false,
        Some(&args.bridge),
    )
}
