use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Map, Value, json};

use super::op;
use crate::app;
use crate::cli::BridgeConnectionArgs;
use crate::cloud;
use crate::daemon::{daemon_control_request, daemon_project_root};
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
    let mut result = studio_status_result(&args, project)?;
    if let Some(map) = result.as_object_mut()
        && let Some(version) = app::update::cached_available_update()
    {
        map.insert("updateAvailable".to_string(), json!(version));
    }
    app::output::print_json_output(&result, false)
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
                // A runtime id only selects between several Studios.
                let single = map
                    .get("clients")
                    .and_then(Value::as_array)
                    .is_some_and(|clients| clients.len() == 1);
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
                    if single {
                        client.remove("runtimeId");
                    }
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
    let (pairs, remainder) = args.edits.as_chunks::<2>();
    if !remainder.is_empty() {
        bail!("Each OLD value needs a following NEW value");
    }
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
        "hold" => "hold",
        _ => "",
    }
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

#[cfg(test)]
mod input_action_tests {
    use super::*;

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
}
