use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use full_moon::ast;
use full_moon::node::Node;
use full_moon::visitors::Visitor;
use serde_json::{Map, Value, json};

use crate::app::output::{ensure_luau_api_ok, ensure_plugin_api_ok, log_global, print_json_output};
use crate::app::timing::current_millis;
use crate::automation::{commands::daemon_result, op};
use crate::cli::{
    BridgeConnectionArgs, ClickArgs, EditorReviewDecisionArgs, ExecuteLuauArgs, GotoArgs, KeyArgs,
    ListClientsArgs, PackageActionArgs, PressArgs, RecordEndArgs, RecordStartArgs, ShotArgs,
    StartStopPlayArgs, StudioChangeStateArgs, StudioDeviceArgs, TypeArgs, UiArgs, WaitUntilArgs,
};
use crate::daemon::{daemon_control_request, try_daemon_control_request};
use crate::snapshot::import::parse_services;
use crate::studio::bridge::{
    BRIDGE_DEFAULT_RESPONSE_TIMEOUT, BRIDGE_ROLE_EDIT, BRIDGE_ROLE_PLAY_CLIENT,
    BRIDGE_ROLE_PLAY_SERVER, BridgeServer, BridgeTarget,
};
use crate::studio::input as input_inject;

mod console;
mod heap;
mod input;
mod microprofiler;
pub(crate) mod monitor;
pub(crate) mod network;
#[cfg(any(windows, target_os = "macos"))]
mod process_exit;
pub(crate) mod property_access;
mod recording;
mod recording_review;
pub(crate) mod render_quality;
mod test_processes;

pub(crate) use console::{get_console_output_command, get_console_output_result};
pub(crate) use input::input_result;
pub(crate) use recording::{end as record_end_result, start as record_start_result};
pub(crate) use recording_review::command as record_review_command;

fn console_entry_level(entry: &Value) -> &str {
    entry
        .get("type")
        .or_else(|| entry.get("level"))
        .and_then(Value::as_str)
        .unwrap_or("output")
}

pub(crate) fn execute_luau_command(mut args: ExecuteLuauArgs) -> Result<()> {
    if args.code.is_none() {
        args.code = args.inline_code.take();
    }
    if args.runner.collect.is_some() && (args.code.is_some() || args.file.is_some()) {
        bail!("--collect reads a detached runner and takes no code");
    }
    if args.code.as_deref() == Some("-") || args.file.as_deref() == Some(std::path::Path::new("-"))
    {
        let mut code = String::new();
        io::stdin().read_to_string(&mut code)?;
        args.code = Some(code.trim_start_matches('\u{feff}').to_string());
        args.file = None;
    }
    let parameters = json!({
        "code": args.code,
        "file": args.file,
        "client": args.client,
        "player": args.player,
        "server": args.server,
        "edit": args.edit,
        "timeout": args.timeout,
        "detach": args.runner.detach,
        "collect": args.runner.collect,
        "stop": args.runner.stop,
        "lifetime": args.runner.lifetime,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let mut result = daemon_result(op::LUAU, None, parameters, false, Some(&args.bridge))?;
    if let Some(map) = result.as_object_mut() {
        if map.get("background") == Some(&Value::Bool(false)) {
            map.remove("background");
        }
        if map.get("ok") == Some(&Value::Bool(true)) {
            map.remove("path");
            map.remove("runner");
        }
        for key in ["output", "results"] {
            if map
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                map.remove(key);
            }
        }
    }
    print_json_output(&result, false)
}

pub(crate) fn validate_luau_syntax(code: &str) -> Result<()> {
    // Use Studio's actual language compiler, without loading or executing code.
    // Its recursion limit also rejects excessive nesting before AST traversal.
    mlua::Compiler::new()
        .compile(code)
        .context("Invalid Luau syntax")?;
    Ok(())
}

#[derive(Default)]
struct LuauLoopCheckpoints {
    offsets: Vec<usize>,
}

impl Visitor for LuauLoopCheckpoints {
    fn visit_generic_for(&mut self, node: &ast::GenericFor) {
        self.offsets
            .push(node.do_token().token().end_position().bytes());
    }
    fn visit_numeric_for(&mut self, node: &ast::NumericFor) {
        self.offsets
            .push(node.do_token().token().end_position().bytes());
    }
    fn visit_repeat(&mut self, node: &ast::Repeat) {
        self.offsets
            .push(node.repeat_token().token().end_position().bytes());
    }
    fn visit_while(&mut self, node: &ast::While) {
        self.offsets
            .push(node.do_token().token().end_position().bytes());
    }
}

fn instrument_luau(code: &str) -> Result<String> {
    let parsed = full_moon::parse_fallible(code, full_moon::LuaVersion::luau());
    if let Some(error) = parsed.errors().first() {
        let (start, _) = error.range();
        bail!(
            "Invalid Luau syntax at {}:{}: {}",
            start.line(),
            start.character(),
            error.error_message()
        );
    }
    let parsed = parsed.into_ast();
    let mut checkpoints = LuauLoopCheckpoints::default();
    checkpoints.visit_ast(&parsed);
    if checkpoints.offsets.is_empty() {
        return Ok(code.to_string());
    }
    let mut name = "__reniumCooperate".to_string();
    while code.contains(&name) {
        name.push('_');
    }
    let count = format!("{name}Count");
    let started = format!("{name}Started");
    let preamble = format!(
        "local {count}=64;local {started}=os.clock();local function {name}(){count}-=1;if {count}>0 then return end;{count}=64;local now=os.clock();if now-{started}>=0.004166666666666667 then task.wait();{started}=os.clock() end end;"
    );
    // Keep comments, directives and the user's formatting intact.
    // Collect byte offsets instead of repeatedly cloning each nested loop AST.
    let first = parsed
        .tokens()
        .next()
        .context("Missing Luau statement")?
        .token()
        .start_position()
        .bytes();
    let mut output = String::with_capacity(
        code.len() + preamble.len() + checkpoints.offsets.len() * (name.len() + 4),
    );
    output.push_str(&code[..first]);
    output.push_str(&preamble);
    let mut previous = first;
    checkpoints.offsets.sort_unstable();
    for offset in checkpoints.offsets {
        output.push_str(&code[previous..offset]);
        output.push(' ');
        output.push_str(&name);
        output.push_str("();");
        previous = offset;
    }
    output.push_str(&code[previous..]);
    validate_luau_syntax(&output)?;
    Ok(output)
}

pub(crate) fn cooperative_luau(code: &str) -> Result<String> {
    validate_luau_syntax(code)?;
    if !["for", "while", "repeat"]
        .iter()
        .any(|word| code.contains(word))
    {
        return Ok(code.to_string());
    }
    // full_moon's recursive parser can exhaust an ordinary daemon request
    // thread even with a few nested loops in a debug build. Keep parsing,
    // traversal and AST destruction on one joined worker with its own stack.
    thread::scope(|scope| {
        thread::Builder::new()
            .name("renium-luau-instrument".into())
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, || instrument_luau(code))
            .context("Could not start Luau instrumentation")?
            .join()
            .map_err(|_| anyhow::anyhow!("Luau instrumentation failed"))?
    })
}

fn luau_request(code: &str, chunk_name: &str, target: BridgeTarget, timeout: f64) -> Result<Value> {
    let code = cooperative_luau(code)?;
    Ok(json!({
        "code": code,
        "chunkName": chunk_name,
        "context": if target == BridgeTarget::Client { "client" } else { "plugin" },
        "timeoutSeconds": timeout,
    }))
}

fn call_execute_luau(
    bridge: &BridgeServer,
    target: BridgeTarget,
    player: Option<&str>,
    code: &str,
    chunk_name: &str,
    timeout: f64,
    response_padding: f64,
) -> Result<Value> {
    bridge.call_for_selector_with_timeout(
        "executeLuau",
        luau_request(code, chunk_name, target, timeout)?,
        target,
        player,
        Some(Duration::from_secs_f64(timeout + response_padding)),
    )
}

fn runner_name(name: &str) -> Result<&str> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        bail!("Runner names use up to 64 letters, digits, '_' or '-'");
    }
    Ok(name)
}

/// Where a Luau run landed, for error messages: an error from the Edit window
/// reads differently from one on the play server or a client.
fn luau_context_label(result: &Value, player: Option<&str>) -> String {
    match result.get("context").and_then(Value::as_str) {
        Some("client") => match player {
            Some(player) => format!("client {player}"),
            None => "play client".to_string(),
        },
        Some("server") => "play server".to_string(),
        Some("edit") | Some("plugin") => "Edit window".to_string(),
        _ => "Studio".to_string(),
    }
}

fn ensure_luau_ok_in_context(result: &Value, player: Option<&str>) -> Result<()> {
    ensure_luau_api_ok(result)
        .map_err(|error| anyhow::anyhow!("[{}] {error:#}", luau_context_label(result, player)))
}

pub(crate) fn execute_luau_result(args: ExecuteLuauArgs, bridge: &BridgeServer) -> Result<Value> {
    let client = args.client || args.player.is_some();
    let target = if args.server {
        BridgeTarget::Server
    } else if args.edit {
        BridgeTarget::Edit
    } else {
        BridgeTarget::main_or_client(client)
    };
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    if let Some(name) = args.runner.collect.as_deref() {
        let result = bridge.call_for_selector_with_timeout(
            "collectLuauRunner",
            json!({ "name": runner_name(name)?, "stop": args.runner.stop }),
            target,
            player,
            Some(Duration::from_secs(30)),
        )?;
        ensure_luau_ok_in_context(&result, player)?;
        return Ok(result);
    }
    let code = if let Some(code) = args.code.or(args.inline_code) {
        code
    } else if let Some(path) = args.file {
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?
    } else {
        bail!("Missing Luau code. Use -e <code> or -f <file>.");
    };
    let timeout = args.timeout.clamp(0.1, 120.0);
    let mut request = luau_request(&code, "Renium", target, timeout)?;
    let detached = args.runner.detach.as_deref().map(runner_name).transpose()?;
    if let Some(name) = detached {
        request["detach"] = json!(name);
        request["lifetimeSeconds"] = json!(args.runner.lifetime.clamp(1.0, 3600.0));
    }
    let result = bridge.call_for_selector_with_timeout(
        "executeLuau",
        request,
        target,
        player,
        Some(Duration::from_secs_f64(timeout + 10.0)),
    )?;
    ensure_luau_ok_in_context(&result, player)?;
    if detached.is_some() && result.get("detached").is_none() {
        bail!("The Studio plugin predates detached runners; run rbx setup and restart Studio");
    }
    Ok(result)
}

pub(crate) fn studio_device_command(args: StudioDeviceArgs) -> Result<()> {
    let parameters = json!({
        "action": args.action,
        "device": args.device,
        "orientation": args.orientation,
        "scalingMode": args.scaling_mode,
        "resolution": args.resolution,
        "pixelDensity": args.pixel_density,
        "details": args.details,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let result = daemon_result(op::DEVICE, None, parameters, false, Some(&args.bridge))?;
    print_json_output(&result, false)
}

pub(crate) fn package_action_command(
    args: PackageActionArgs,
    project: Option<&Path>,
    operation: u16,
) -> Result<()> {
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout > 600.0 {
        bail!("Package timeout must be >0 and <=600s");
    }
    let result = daemon_result(
        operation,
        project,
        json!({
            "target": args.target,
            "ords": args.ords,
            "pid": args.pid,
            "timeout": args.timeout,
        }),
        false,
        Some(&args.bridge),
    )?;
    print_json_output(&result, false)
}

pub(crate) fn studio_device_resolution(raw: &str) -> Result<(u32, u32)> {
    let normalized = raw.trim().to_ascii_lowercase().replace('×', "x");
    let (width, height) = normalized
        .split_once('x')
        .with_context(|| format!("Invalid resolution '{raw}'. Use WIDTHxHEIGHT."))?;
    let width = width
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid resolution width in '{raw}'"))?;
    let height = height
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid resolution height in '{raw}'"))?;
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        bail!("Resolution must use positive 32-bit dimensions");
    }
    Ok((width, height))
}

pub(crate) fn studio_device_result(
    args: &StudioDeviceArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let mut params = Map::new();
    params.insert("action".to_string(), Value::String(args.action.clone()));
    if let Some(device) = args.device.as_ref() {
        params.insert("device".to_string(), Value::String(device.clone()));
    }
    if let Some(orientation) = args.orientation.as_ref() {
        params.insert(
            "orientation".to_string(),
            Value::String(orientation.clone()),
        );
    }
    if let Some(scaling_mode) = args.scaling_mode.as_ref() {
        params.insert(
            "scalingMode".to_string(),
            Value::String(scaling_mode.clone()),
        );
    }
    if let Some(resolution) = args.resolution.as_deref() {
        let (width, height) = studio_device_resolution(resolution)?;
        params.insert("width".to_string(), json!(width));
        params.insert("height".to_string(), json!(height));
    }
    if let Some(pixel_density) = args.pixel_density {
        if !pixel_density.is_finite() || pixel_density <= 0.0 {
            bail!("Pixel density must be a finite number greater than zero");
        }
        params.insert("pixelDensity".to_string(), json!(pixel_density));
    }
    if args.details {
        params.insert("details".to_string(), Value::Bool(true));
    }
    let requested_params = params.clone();
    let result =
        bridge.call_for_target("deviceSimulator", Value::Object(params), BridgeTarget::Edit)?;
    ensure_plugin_api_ok(&result)?;
    match args.action.to_ascii_lowercase().as_str() {
        "stop" | "reset" => {
            #[cfg(any(windows, target_os = "macos"))]
            {
                let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
                input_inject::close_device_emulator_toolbar(pid)?;
            }
            bridge.set_desired_device_request(json!({ "action": "stop" }));
        }
        "set" | "select" | "apply" => {
            let mut request = Map::new();
            request.insert("action".to_string(), Value::String("set".to_string()));
            if let Some(device) = result["device"]["id"].as_str() {
                request.insert("device".to_string(), Value::String(device.to_string()));
            }
            if let Some(orientation) = result["orientation"].as_str() {
                request.insert(
                    "orientation".to_string(),
                    Value::String(orientation.to_string()),
                );
            }
            if let Some(scaling_mode) = result["scalingMode"].as_str() {
                request.insert(
                    "scalingMode".to_string(),
                    Value::String(scaling_mode.to_string()),
                );
            }
            for key in [
                "device",
                "orientation",
                "scalingMode",
                "width",
                "height",
                "pixelDensity",
            ] {
                if let Some(value) = requested_params.get(key) {
                    request.insert(key.to_string(), value.clone());
                }
            }
            if args.device.is_some() {
                bridge.set_desired_device_request(Value::Object(request));
            } else {
                bridge.merge_desired_device_request(Value::Object(request));
            }
        }
        _ => {}
    }
    Ok(result)
}

fn wait_for_player_bridge(bridge: &BridgeServer, player: &str, wait_seconds: f64) -> Result<()> {
    if bridge.wait_for_ready_player(player, Duration::from_secs_f64(wait_seconds.max(1.0))) {
        return Ok(());
    }
    bail!(
        "No connected play client matches player selector '{player}'. Connected bridges: {}",
        serde_json::to_string(&bridge.list_bridge_clients())?
    )
}

pub(crate) fn start_stop_play_command(args: StartStopPlayArgs) -> Result<()> {
    validate_play_args(&args)?;
    if args.kill_orphans {
        return kill_orphans_command(&args);
    }
    let operation = if args.stop {
        op::PLAY_STOP
    } else {
        op::PLAY_START
    };
    let parameters = json!({
        "players": args.players,
        "addPlayers": args.add_players,
        "leave": args.leave,
        "mode": args.mode,
        "restart": args.restart,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let mut result = daemon_control_request(operation, None, parameters, false)?;
    if let Some(condition) = args.until.as_deref()
        && result.get("ok").and_then(Value::as_bool) != Some(false)
    {
        let wait = daemon_control_request(
            op::WAIT,
            None,
            json!({
                "condition": condition,
                "timeout": args.until_timeout,
                "bridgeWaitSeconds": args.bridge.wait_seconds,
                "bridgePorts": args.bridge.ports,
            }),
            false,
        );
        match wait {
            Ok(wait) => result["until"] = wait,
            Err(error) => {
                print_json_output(&result, false)?;
                return Err(error);
            }
        }
    }
    print_json_output(&result, false)
}

fn kill_orphans_command(args: &StartStopPlayArgs) -> Result<()> {
    let result = daemon_control_request(
        op::STUDIOS,
        None,
        json!({
            "killOrphans": true,
            "place": crate::studio::target::place_filter(),
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
        false,
    )?;
    print_json_output(&result, false)?;
    if let Some(failed) = result["failed"]
        .as_array()
        .filter(|failed| !failed.is_empty())
    {
        bail!(
            "{} orphaned Studio test process(es) could not be closed",
            failed.len()
        );
    }
    Ok(())
}

pub(crate) fn studio_change_state_command(
    args: StudioChangeStateArgs,
    project: Option<&Path>,
) -> Result<()> {
    let operation = if args.stop {
        op::LIVE_STOP
    } else if args.no_start {
        op::LIVE_STATUS
    } else if args.clear_pending {
        op::DISCARD_PENDING
    } else {
        op::LIVE_START
    };
    studio_change_state_operation_command(args, operation, project)
}

pub(crate) fn studio_change_state_operation_command(
    mut args: StudioChangeStateArgs,
    operation: u16,
    project: Option<&Path>,
) -> Result<()> {
    args.stop = operation == op::LIVE_STOP;
    args.no_start = operation == op::LIVE_STATUS;
    args.clear_pending = operation == op::DISCARD_PENDING;
    if operation == op::LIVE_START {
        args.reset = true;
        args.replace_services = true;
    }
    let has_preference = args.prefer.is_some();
    let details = args.details;
    let parameters = json!({
        "services": args.services,
        "reset": args.reset,
        "replaceServices": args.replace_services,
        "ackSeq": args.ack_seq,
        "ackRuntimeSettingsSeq": args.ack_runtime_settings_seq,
        "ackActions": args.ack_actions,
        "ackActionResults": args.ack_action_results,
        "runtimeId": args.runtime_id,
        "suppressSeconds": args.suppress_seconds,
        "eventWaitSeconds": args.event_wait_seconds,
        "settleWaitSeconds": args.settle_wait_seconds,
        "contextBound": args.context_bound,
        "resolveConflictPreference": args.prefer,
        "initialSyncPriority": args.initial_sync_priority,
        "initialConflictPreference": args.initial_conflict_preference,
        "compact": !args.details,
        "manageFiles": true,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let result = daemon_result(operation, project, parameters, false, Some(&args.bridge))?;
    finish_studio_change_state_command(operation, has_preference, result, details)
}

fn finish_studio_change_state_command(
    operation: u16,
    has_preference: bool,
    mut result: Value,
    details: bool,
) -> Result<()> {
    let failed = result.get("ok").and_then(Value::as_bool) == Some(false);
    let resolution_required = result
        .pointer("/daemon/resolutionRequired")
        .and_then(Value::as_bool)
        == Some(true);
    if failed && resolution_required {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Studio and project files changed the same content");
        if operation == op::LIVE_START && has_preference {
            bail!(error.to_string());
        }
        let conflicts = result
            .pointer("/daemon/conflicts")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let listing = if details && conflicts.len() > 3 {
            format!(
                "\nAll {} conflicts:\n{}",
                conflicts.len(),
                conflicts.join("\n")
            )
        } else if error.contains(" more (") {
            "\n`rbx lst --details` lists every conflict and what differs".to_string()
        } else {
            String::new()
        };
        bail!(
            "{error}{listing}\nResolve with one:\nrbx lon --prefer studio\nrbx lon --prefer editor"
        );
    }
    if failed {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Live Sync could not complete the requested operation");
        bail!(error.to_string());
    }
    if !details {
        if let Some(map) = result.as_object_mut() {
            for key in ["runtimeId", "seq", "snapshotSeq", "runtimeSettingsSeq"] {
                map.remove(key);
            }
        }
        crate::app::output::strip_empty(&mut result);
    }
    print_json_output(&result, false)
}

pub(crate) fn studio_change_state_result(
    args: StudioChangeStateArgs,
    bridge: &BridgeServer,
    runtime_id: &str,
) -> Result<Value> {
    let details = args.details;
    let services = parse_services(&args.services)?;
    let action_results: Value = serde_json::from_str(&args.ack_action_results)
        .context("--ack-action-results must be a JSON object")?;
    if !action_results.is_object() {
        bail!("--ack-action-results must be a JSON object");
    }
    let result = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "services": services,
            "reset": args.reset,
            "replaceServices": args.replace_services,
            "clearPending": args.clear_pending,
            "start": !args.no_start && !args.clear_pending,
            "stop": args.stop,
            "ackSeq": args.ack_seq,
            "ackRuntimeSettingsSeq": args.ack_runtime_settings_seq,
            "ackEditorActions": args.ack_actions,
            "ackEditorActionResults": action_results,
            "runtimeId": args.runtime_id,
            "suppressSeconds": args.suppress_seconds,
            "waitSeconds": args.event_wait_seconds,
            "contextBound": args.context_bound,
            "compact": !details,
        }),
        BridgeTarget::Edit,
        runtime_id,
        None,
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(if details {
        result
    } else {
        compact_live_status(result)
    })
}

pub(crate) fn normalize_live_status(mut value: Value) -> Value {
    let error = value
        .pointer("/daemon/error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            (value.pointer("/daemon/settled").and_then(Value::as_bool) == Some(false)).then(|| {
                match value.pointer("/daemon/unsettled").and_then(Value::as_str) {
                    Some(reason) => {
                        format!("Live Sync did not settle before the wait ended: {reason}")
                    }
                    None => {
                        "Live Sync did not finish before the wait ended; inspect rbx lst --details"
                            .to_string()
                    }
                }
            })
        });
    if let Some(error) = error
        && let Some(result) = value.as_object_mut()
    {
        result.insert("ok".into(), Value::Bool(false));
        result.insert("error".into(), Value::String(error));
    }
    value
}

pub(crate) fn compact_live_status(value: Value) -> Value {
    let value = normalize_live_status(value);
    let Some(source) = value.as_object() else {
        return value;
    };
    let mut result = Map::new();
    for key in [
        "busy",
        "role",
        "runtimeId",
        "twoWaySyncEnabled",
        "tracking",
        "trackedServices",
        "connectedInstances",
        "onlyCodeMode",
        "seq",
        "snapshotSeq",
        "runtimeSettingsSeq",
        "conflictResolution",
        "dirtyServices",
        "fullSyncServices",
    ] {
        if let Some(value) = source.get(key) {
            result.insert(key.to_string(), value.clone());
        }
    }

    let mut pending_changes = 0usize;
    for (source_key, count_key) in [
        ("changes", "changeCount"),
        ("propertyChanges", "propertyChangeCount"),
        ("editorActions", "editorActionCount"),
        ("runtimeSettingChanges", "runtimeSettingChangeCount"),
    ] {
        let count = source
            .get(count_key)
            .and_then(Value::as_u64)
            .map(|count| count as usize)
            .or_else(|| {
                source
                    .get(source_key)
                    .and_then(Value::as_array)
                    .map(Vec::len)
            })
            .unwrap_or_default();
        pending_changes += count;
        if count > 0 {
            result.insert(count_key.to_string(), json!(count));
        }
    }
    result.insert("pendingChanges".to_string(), json!(pending_changes));

    let daemon = source.get("daemon").map(compact_live_daemon_status);
    let mut ok = source.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if let Some(error) = source.get("error") {
        ok = false;
        result.insert("error".to_string(), error.clone());
    }
    result.insert("ok".to_string(), Value::Bool(ok));
    if let Some(daemon) = daemon {
        result.insert("daemon".to_string(), daemon);
    }
    Value::Object(result)
}

fn compact_live_daemon_status(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return value.clone();
    };
    let mut result = Map::new();
    for key in [
        "running",
        "mode",
        "resolutionRequired",
        "pullChanges",
        "paused",
        "syncing",
        "settled",
        "pushes",
        "pulls",
        "autoDesyncedPackages",
        "autoDesyncedAtPush",
        "error",
        "previousError",
        "terrainObservation",
    ] {
        if let Some(value) = source.get(key) {
            result.insert(key.to_string(), value.clone());
        }
    }
    let pending = source
        .get("pendingCount")
        .and_then(Value::as_u64)
        .map(|count| count as usize)
        .or_else(|| {
            source
                .get("pendingPaths")
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or_default();
    result.insert("pendingCount".to_string(), json!(pending));
    if pending > 0
        && pending <= 8
        && let Some(paths) = source.get("pendingPaths")
    {
        result.insert("pendingPaths".to_string(), paths.clone());
    }
    Value::Object(result)
}

fn validate_play_args(args: &StartStopPlayArgs) -> Result<()> {
    if args.start && args.stop {
        bail!("Use either --start or --stop, not both");
    }
    if args.players.is_some() && args.stop {
        bail!("--players cannot be combined with --stop");
    }
    let mode = args.mode.as_deref().unwrap_or("play");
    if !matches!(mode, "play" | "run" | "server") {
        bail!("Invalid play mode '{mode}'; use play, run, or server");
    }
    if args.players.is_some() && mode != "play" {
        bail!("--players can only be used with --mode play");
    }
    if (args.add_players.is_some() || args.leave) && (args.start || args.stop) {
        bail!("--add-players and --leave change a running test; drop --start and --stop");
    }
    if let Some(count) = args.add_players {
        if args.leave || args.players.is_some() {
            bail!("Use --add-players N on its own; remove a client separately with --leave -p N");
        }
        if !(1..=8).contains(&count) {
            bail!("--add-players takes 1 through 8; Studio runs at most 8 clients");
        }
    }
    if args.leave {
        match args.players {
            None => bail!("Name the client to remove: rbx play --leave -p N (rbx cs lists them)"),
            Some(0) => bail!("Client indexes start at 1: rbx play --leave -p 1"),
            Some(_) => {}
        }
    }
    Ok(())
}

pub(crate) fn start_stop_play_result(
    args: StartStopPlayArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    validate_play_args(&args)?;
    let mode = args.mode.as_deref().unwrap_or("play");
    if args.stop {
        return stop_studio_play_with_bridge_result(bridge);
    }
    if args.restart {
        return restart_play_result(args, bridge);
    }
    if let Some(count) = args.add_players {
        return add_test_players_result(bridge, count, args.bridge.wait_seconds);
    }
    if let Some(index) = args.players.filter(|_| args.leave) {
        return leave_test_result(bridge, index, args.bridge.wait_seconds);
    }
    if args.start || args.players.is_some() {
        let plan = match args.players {
            Some(players) => PlayLaunchPlan::Multi(players),
            None if matches!(mode, "run" | "server") => PlayLaunchPlan::Run,
            None => PlayLaunchPlan::Play,
        };
        let mut result = start_play_with_retries(bridge, plan)?;
        result["serverReady"] = json!(server_answers_luau(bridge));
        return Ok(result);
    }
    let result = bridge.call_for_target("startStopPlay", json!({}), BridgeTarget::Edit)?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);
const OLDER_PLUGIN: &str = "The Renium plugin in this test is older than rbx and cannot add or remove clients; update the plugin and restart Studio";

fn session_play_clients(bridge: &BridgeServer, server_runtime_id: &str) -> Vec<Value> {
    let owner = bridge
        .list_bridge_clients()
        .into_iter()
        .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(server_runtime_id))
        .and_then(|entry| {
            entry
                .get("launchEditRuntimeId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            bridge
                .runtime_pin_for_selector(BridgeTarget::Edit, None)
                .ok()
                .map(|pin| pin.runtime_id)
        });
    let Some(owner) = owner else {
        return Vec::new();
    };
    studio_play_clients(bridge, &owner)
        .into_iter()
        .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
        .collect()
}

fn runtime_ids(clients: &[Value]) -> HashSet<String> {
    clients
        .iter()
        .filter_map(|entry| entry.get("runtimeId").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn added_clients_deadline(started: Instant, last_progress: Instant) -> Instant {
    (last_progress + Duration::from_secs(30)).min(started + Duration::from_secs(120))
}

fn add_test_players_result(bridge: &BridgeServer, count: u32, wait_seconds: f64) -> Result<Value> {
    if bridge
        .wait_for_target(wait_seconds, BridgeTarget::Server)
        .is_err()
    {
        bail!("No play test is running; start one with rbx play -s -p N, then add players");
    }
    let server = bridge
        .runtime_pin_for_selector(BridgeTarget::Server, None)?
        .runtime_id;
    let before = runtime_ids(&session_play_clients(bridge, &server));
    let requested = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({ "addPlayers": count }),
        BridgeTarget::Server,
        &server,
        None,
    )?;
    ensure_plugin_api_ok(&requested)?;
    if requested["action"] != "addPlayers" {
        bail!(OLDER_PLUGIN);
    }
    let started = Instant::now();
    let mut last_progress = started;
    let mut last_seen = 0;
    loop {
        let clients = session_play_clients(bridge, &server);
        let added = clients
            .iter()
            .filter(|entry| {
                entry
                    .get("runtimeId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !before.contains(id))
            })
            .collect::<Vec<_>>();
        let names = added
            .iter()
            .filter_map(|entry| entry.get("playerName").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if added.len() >= count as usize && names.len() == added.len() {
            return Ok(json!({
                "ok": true,
                "action": "addPlayers",
                "added": names,
                "clients": server_player_names(bridge, &server).map_or(clients.len(), |names| names.len()),
            }));
        }
        if added.len() != last_seen {
            last_seen = added.len();
            last_progress = Instant::now();
        }
        if Instant::now() >= added_clients_deadline(started, last_progress) {
            bail!(
                "Studio accepted --add-players {count}, but {} new client(s) connected in {} s; the rest may still be joining (rbx cs lists them)",
                added.len(),
                started.elapsed().as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn leave_test_result(bridge: &BridgeServer, index: u32, wait_seconds: f64) -> Result<Value> {
    let selector = index.to_string();
    if !bridge.wait_for_ready_player(&selector, Duration::from_secs_f64(wait_seconds.max(1.0))) {
        bail!("No play client {index} is connected; rbx cs lists the clients of the running test");
    }
    let runtime_id = bridge
        .runtime_pin_for_selector(BridgeTarget::Client, Some(&selector))?
        .runtime_id;
    let Some(server) = bridge.play_runtime_for_selector(BridgeTarget::Server, None) else {
        bail!("No play server is connected; --leave needs a running test (rbx play -s -p N)");
    };
    let player = client_player_name(bridge, &runtime_id).with_context(|| {
        format!("Client {index} has not reported its player name yet; retry in a moment")
    })?;
    let kick = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({ "kickPlayer": player, "reason": "rbx play --leave" }),
        BridgeTarget::Server,
        &server,
        Some(Duration::from_secs(2)),
    )?;
    ensure_plugin_api_ok(&kick)?;
    if kick["action"] != "kick" {
        bail!(OLDER_PLUGIN);
    }
    let kicked = kick["kicked"].as_bool() == Some(true);
    let leave_requested = bridge
        .call_for_runtime_with_timeout(
            "startStopPlay",
            json!({ "leave": true }),
            BridgeTarget::Client,
            &runtime_id,
            Some(Duration::from_secs(2)),
        )
        .is_ok_and(|reply| reply["ok"] == true && reply["action"] == "leave");
    let remaining =
        wait_for_server_to_drop(bridge, &server, &player).map_err(
            |error| match kick["kickError"].as_str() {
                Some(kick_error) => anyhow::anyhow!("{error} (Kick failed: {kick_error})"),
                None => error,
            },
        )?;
    let window_closed = leave_requested && wait_for_channel_to_close(bridge, &runtime_id);
    Ok(json!({
        "ok": true,
        "action": "leave",
        "player": player,
        "kicked": kicked,
        "serverListsPlayer": false,
        "windowClosed": window_closed,
        "clients": remaining.len(),
    }))
}

fn client_player_name(bridge: &BridgeServer, runtime_id: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let name = bridge
            .list_bridge_clients()
            .into_iter()
            .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id))
            .and_then(|entry| {
                entry
                    .get("playerName")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if name.is_some() || Instant::now() >= deadline {
            return name;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn server_player_names(bridge: &BridgeServer, server: &str) -> Result<Vec<String>> {
    let listed = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({ "listPlayers": true }),
        BridgeTarget::Server,
        server,
        Some(Duration::from_secs(2)),
    )?;
    ensure_plugin_api_ok(&listed)?;
    if listed["action"] != "players" {
        bail!(OLDER_PLUGIN);
    }
    Ok(listed["players"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
}

fn wait_for_server_to_drop(
    bridge: &BridgeServer,
    server: &str,
    player: &str,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + LEAVE_TIMEOUT;
    loop {
        let names = server_player_names(bridge, server)?;
        if !names.iter().any(|name| name == player) {
            return Ok(names);
        }
        if Instant::now() >= deadline {
            bail!(
                "{player} is still in the server's Players {} s after the kick; end the test with rbx play -x",
                LEAVE_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_channel_to_close(bridge: &BridgeServer, runtime_id: &str) -> bool {
    let deadline = Instant::now() + LEAVE_TIMEOUT;
    while runtime_ids(&bridge.list_bridge_clients()).contains(runtime_id) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
    true
}

// A freshly launched server bridge connects before its scripts can run; the
// first Luau call would otherwise be the one that discovers that.
fn server_answers_luau(bridge: &BridgeServer) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ready = call_execute_luau(
            bridge,
            BridgeTarget::Main,
            None,
            "return true",
            "ReniumReady",
            5.0,
            2.0,
        )
        .is_ok_and(|result| result.get("ok").and_then(Value::as_bool) == Some(true));
        if ready {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn new_play_launch(bridge: &BridgeServer, label: &str) -> Result<TestLaunch> {
    let edit_pin = bridge.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
    let edit_runtime_id = edit_pin.runtime_id;
    #[cfg(windows)]
    crate::project::workflows::windows_launch::protect_process(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, &edit_runtime_id)?,
    )?;
    #[cfg(target_os = "macos")]
    crate::studio::native::serializer::protect_studio_launch(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, &edit_runtime_id)?,
    )?;
    let sequence = bridge
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(TestLaunch {
        nonce: format!(
            "{label}-{}-{}-{sequence}",
            std::process::id(),
            current_millis()
        ),
        edit_runtime_id,
    })
}

fn cancel_test_launch_best_effort(bridge: &BridgeServer, launch: &TestLaunch) {
    request_play_runtimes_to_stop(
        bridge,
        &test_launch_clients(bridge, launch),
        Some(&launch.nonce),
    );
    let _ = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "stop": true,
            "launchNonce": launch.nonce,
            "waitForStopped": false,
        }),
        BridgeTarget::Edit,
        &launch.edit_runtime_id,
        Some(Duration::from_secs(2)),
    );
    close_leftover_test_processes(bridge, &launch.edit_runtime_id, Duration::from_secs(15));
}

fn multiplayer_start_deadline(started: Instant, last_progress: Instant) -> Instant {
    (last_progress + Duration::from_secs(60))
        .max(started + Duration::from_secs(90))
        .min(started + Duration::from_secs(240))
}

fn leftover_test_processes(edit_pid: u32) -> Vec<u32> {
    test_processes::test_descendants(&test_processes::studio_process_table(), edit_pid)
}

#[cfg(any(windows, target_os = "macos"))]
fn terminate_test_process(pid: u32) -> Result<()> {
    crate::studio::input::terminate_studio_process(pid)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn terminate_test_process(_pid: u32) -> Result<()> {
    Ok(())
}

fn close_leftover_test_processes(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    grace: Duration,
) -> Vec<u32> {
    let Ok(edit_pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id) else {
        return Vec::new();
    };
    let deadline = Instant::now() + grace;
    let mut leftover = leftover_test_processes(edit_pid);
    while !leftover.is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(250));
        leftover = leftover_test_processes(edit_pid);
    }
    let mut closed = Vec::new();
    for pid in leftover {
        match terminate_test_process(pid) {
            Ok(()) => closed.push(pid),
            Err(error) => log_global(
                5,
                format_args!("[renium] Studio test process {pid} could not be closed: {error:#}"),
            ),
        }
    }
    if !closed.is_empty() {
        log_global(
            5,
            format_args!("[renium] closed leftover Studio test processes: {closed:?}"),
        );
    }
    closed
}

fn edit_client<'a>(clients: &'a [Value], runtime_id: &str) -> Option<&'a Value> {
    clients.iter().find(|client| {
        client["role"] == BRIDGE_ROLE_EDIT
            && client.get("runtimeId").and_then(Value::as_str) == Some(runtime_id)
    })
}

/// Orphaned Studio test processes for `rbx cs` and `rbx status`. Given an Edit
/// runtime, connected orphans of other places are left out.
pub(crate) fn orphan_summaries(clients: &[Value], edit_runtime_id: Option<&str>) -> Vec<Value> {
    let edit = edit_runtime_id.and_then(|runtime_id| edit_client(clients, runtime_id));
    test_processes::orphan_test_processes(&test_processes::studio_process_table(), clients)
        .iter()
        .filter(|orphan| match (&orphan.client, edit) {
            (Some(client), Some(edit)) => test_processes::same_place(client, edit),
            _ => true,
        })
        .map(test_processes::Orphan::summary)
        .collect()
}

fn close_orphans(
    bridge: &BridgeServer,
    orphans: &[test_processes::Orphan],
) -> (Vec<u32>, Vec<Value>) {
    let mut closed = Vec::new();
    let mut failed = Vec::new();
    for orphan in orphans {
        match terminate_test_process(orphan.pid) {
            Ok(()) => {
                closed.push(orphan.pid);
                if let Some(runtime_id) = orphan.runtime_id() {
                    bridge.retire_runtime(runtime_id);
                }
            }
            Err(error) => {
                failed.push(json!({ "pid": orphan.pid, "error": format!("{error:#}") }));
            }
        }
    }
    if !closed.is_empty() {
        log_global(
            5,
            format_args!("[renium] closed orphaned Studio test processes: {closed:?}"),
        );
    }
    (closed, failed)
}

fn client_place_matches(client: &Value, selector: &str) -> bool {
    crate::studio::target::place_matches(
        &crate::studio::bridge::BridgeInfoPayload {
            place_id: client.get("placeId").and_then(Value::as_i64),
            game_id: client.get("gameId").and_then(Value::as_i64),
            place_name: client
                .get("placeName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ..Default::default()
        },
        selector,
    )
}

/// `rbx play --kill-orphans`: closes every orphaned test process, or with a
/// place selector the connected ones of matching places.
pub(crate) fn kill_orphans_result(bridge: &BridgeServer, place: Option<&str>) -> Value {
    let clients = bridge.list_bridge_clients();
    let orphans =
        test_processes::orphan_test_processes(&test_processes::studio_process_table(), &clients)
            .into_iter()
            .filter(|orphan| {
                place.is_none_or(|selector| {
                    orphan
                        .client
                        .as_ref()
                        .is_some_and(|client| client_place_matches(client, selector))
                })
            })
            .collect::<Vec<_>>();
    let (closed, failed) = close_orphans(bridge, &orphans);
    let mut result = json!({ "ok": failed.is_empty(), "action": "killOrphans", "closed": closed });
    if !failed.is_empty() {
        result["failed"] = json!(failed);
    }
    result
}

fn close_same_place_orphans(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
) -> (Vec<u32>, Vec<Value>) {
    let clients = bridge.list_bridge_clients();
    let Some(edit) = edit_client(&clients, edit_runtime_id) else {
        return Default::default();
    };
    let orphans =
        test_processes::orphan_test_processes(&test_processes::studio_process_table(), &clients)
            .into_iter()
            .filter(|orphan| {
                orphan
                    .client
                    .as_ref()
                    .is_some_and(|client| test_processes::same_place(client, edit))
            })
            .collect::<Vec<_>>();
    close_orphans(bridge, &orphans)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayLaunchPlan {
    Play,
    Run,
    Multi(u32),
}

impl PlayLaunchPlan {
    fn label(self) -> &'static str {
        match self {
            Self::Multi(_) => "multi",
            Self::Play | Self::Run => "play",
        }
    }

    fn plugin_mode(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Run => "run",
            Self::Multi(_) => "multi",
        }
    }

    fn session_kind(self) -> &'static str {
        match self {
            Self::Multi(_) => "multiplayer",
            Self::Play | Self::Run => "play",
        }
    }

    fn players(self) -> Option<u32> {
        match self {
            Self::Multi(players) => Some(players),
            Self::Play | Self::Run => None,
        }
    }

    fn clients_needed(self) -> usize {
        match self {
            Self::Play => 1,
            Self::Run => 0,
            Self::Multi(players) => players as usize,
        }
    }

    fn request(self, launch_nonce: &str) -> Value {
        match self {
            Self::Multi(players) => {
                json!({ "start": true, "players": players, "launchNonce": launch_nonce })
            }
            Self::Play | Self::Run => {
                json!({ "start": true, "mode": self.plugin_mode(), "launchNonce": launch_nonce })
            }
        }
    }

    fn start_deadline(self, started: Instant, last_progress: Instant) -> Instant {
        match self {
            Self::Multi(_) => multiplayer_start_deadline(started, last_progress),
            Self::Play | Self::Run => started + SINGLE_START_TIMEOUT,
        }
    }

    /// How long a start may show no sign of the test at all.
    fn sign_timeout(self) -> Duration {
        match self {
            Self::Multi(_) => MULTI_START_SIGN_TIMEOUT,
            Self::Play | Self::Run => SINGLE_START_TIMEOUT,
        }
    }

    /// Whether Studio dropped the start: the call returned without the test
    /// appearing, or nothing showed up at all.
    fn start_was_ignored(self, elapsed: Duration, begun: bool, executing: Option<bool>) -> bool {
        !begun
            && (elapsed >= self.sign_timeout()
                || executing == Some(false) && elapsed >= START_RETURN_GRACE)
    }
}

// Studio drops a test start while the previous test is still ending or a place
// it just opened is still settling: the start call returns at once, or no test
// ever appears. Such a start is cancelled and requested again.
const START_ATTEMPTS: u32 = 3;
const START_RETRY_WINDOW: Duration = Duration::from_secs(60);
const START_RETURN_GRACE: Duration = Duration::from_secs(5);
const SINGLE_START_TIMEOUT: Duration = Duration::from_secs(20);
const MULTI_START_SIGN_TIMEOUT: Duration = Duration::from_secs(30);
const LEFTOVER_GRACE: Duration = Duration::from_secs(10);

enum LaunchOutcome {
    Started(Value),
    Ignored(String),
}

enum AttemptOutcome {
    AlreadyRunning(Value),
    Started { result: Value, requested: Instant },
    Ignored { reason: String, requested: Instant },
}

static LAUNCHED_PLANS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PlayLaunchPlan>>,
> = std::sync::LazyLock::new(Default::default);

fn remember_launch(edit_runtime_id: &str, plan: PlayLaunchPlan) {
    use crate::system::LockRecover;
    LAUNCHED_PLANS
        .lock_recover()
        .insert(edit_runtime_id.to_string(), plan);
}

fn remembered_launch(edit_runtime_id: &str) -> Option<PlayLaunchPlan> {
    use crate::system::LockRecover;
    LAUNCHED_PLANS.lock_recover().get(edit_runtime_id).copied()
}

fn start_play_with_retries(bridge: &BridgeServer, plan: PlayLaunchPlan) -> Result<Value> {
    if plan
        .players()
        .is_some_and(|players| !(1..=8).contains(&players))
    {
        bail!("Multiplayer tests require between 1 and 8 players");
    }
    let edit_runtime_id = bridge
        .runtime_pin_for_selector(BridgeTarget::Edit, None)?
        .runtime_id;
    let (closed_orphans, orphan_errors) = close_same_place_orphans(bridge, &edit_runtime_id);
    let mut first_request: Option<Instant> = None;
    let mut last_reason = String::new();
    let mut attempts = 0;
    while attempts < START_ATTEMPTS
        && first_request.is_none_or(|first| first.elapsed() < START_RETRY_WINDOW)
    {
        attempts += 1;
        if attempts > 1 {
            thread::sleep(Duration::from_secs(u64::from(attempts - 1)));
        }
        let outcome =
            start_attempt(bridge, plan).map_err(|error| with_orphan_hint(bridge, error))?;
        let mut result = match outcome {
            AttemptOutcome::AlreadyRunning(existing) => existing,
            AttemptOutcome::Started {
                mut result,
                requested,
            } => {
                if let Some(first) = first_request {
                    result["retriedAfterMs"] = json!(requested.duration_since(first).as_millis());
                    result["startAttempts"] = json!(attempts);
                    result["retryReason"] = json!(last_reason);
                }
                result
            }
            AttemptOutcome::Ignored { reason, requested } => {
                log_global(
                    5,
                    format_args!(
                        "[renium] Studio did not start the test (attempt {attempts}): {reason}"
                    ),
                );
                first_request.get_or_insert(requested);
                last_reason = reason;
                continue;
            }
        };
        if !closed_orphans.is_empty() {
            result["closedOrphans"] = json!(closed_orphans);
        }
        if !orphan_errors.is_empty() {
            result["orphanErrors"] = json!(orphan_errors);
        }
        return Ok(result);
    }
    Err(with_orphan_hint(
        bridge,
        anyhow::anyhow!(
            "Studio did not start the {} session after {attempts} attempts: {last_reason}. Check the Edit window for a dialog, or run rbx status",
            plan.session_kind(),
        ),
    ))
}

fn with_orphan_hint(bridge: &BridgeServer, error: anyhow::Error) -> anyhow::Error {
    let orphans = test_processes::orphan_test_processes(
        &test_processes::studio_process_table(),
        &bridge.list_bridge_clients(),
    );
    if orphans.is_empty() {
        return error;
    }
    anyhow::anyhow!(
        "{error:#}. {} orphaned Studio test window(s) whose Edit window has exited are still open; rbx play --kill-orphans closes them",
        orphans.len()
    )
}

fn start_attempt(bridge: &BridgeServer, plan: PlayLaunchPlan) -> Result<AttemptOutcome> {
    let launch = new_play_launch(bridge, plan.label())?;
    let mut device_simulation = studio_device_status(bridge)?;
    let waited = Instant::now();
    let mut existing = wait_for_studio_play_ready(bridge, &launch.edit_runtime_id)?;
    if play_status_is_running(&existing) {
        if plan.players().is_some() {
            bail!("A Studio play session is already active in the selected window");
        }
        existing["deviceSimulation"] = device_simulation;
        return Ok(AttemptOutcome::AlreadyRunning(existing));
    }
    let closed = close_leftover_test_processes(bridge, &launch.edit_runtime_id, LEFTOVER_GRACE);
    let waited_ms = waited.elapsed().as_millis();
    let requested = Instant::now();
    match launch_play_session(bridge, &launch, plan) {
        Ok(LaunchOutcome::Started(mut result)) => {
            remember_launch(&launch.edit_runtime_id, plan);
            device_simulation["playRunning"] = Value::Bool(true);
            result["deviceSimulation"] = device_simulation;
            if waited_ms >= 500 {
                result["waitedForStudioMs"] = json!(waited_ms);
            }
            if !closed.is_empty() {
                result["closedProcesses"] = json!(closed);
            }
            Ok(AttemptOutcome::Started { result, requested })
        }
        Ok(LaunchOutcome::Ignored(reason)) => {
            cancel_test_launch_best_effort(bridge, &launch);
            Ok(AttemptOutcome::Ignored { reason, requested })
        }
        Err(error) => {
            cancel_test_launch_best_effort(bridge, &launch);
            Err(error)
        }
    }
}

/// Watches for any sign that Studio took a start: the Edit controller entering
/// the test, a new Studio process under the Edit window, or a test bridge.
struct LaunchWatch {
    edit_pid: Option<u32>,
    baseline: HashSet<u32>,
    begun: bool,
}

fn studio_child_pids(edit_pid: u32) -> HashSet<u32> {
    test_processes::studio_descendants(&test_processes::studio_process_table(), edit_pid)
        .iter()
        .map(|process| process.pid)
        .collect()
}

impl LaunchWatch {
    fn new(bridge: &BridgeServer, launch: &TestLaunch) -> Self {
        let edit_pid = bridge
            .studio_pid_for_runtime(BridgeTarget::Edit, &launch.edit_runtime_id)
            .ok();
        Self {
            edit_pid,
            baseline: edit_pid.map(studio_child_pids).unwrap_or_default(),
            begun: false,
        }
    }

    fn observe(&mut self, status: Option<&Value>, clients: &[Value]) {
        self.begun = self.begun
            || !clients.is_empty()
            || status.is_some_and(|status| {
                status["engineRunning"] == true || status["engineStarted"] == true
            })
            || self
                .edit_pid
                .is_some_and(|pid| !studio_child_pids(pid).is_subset(&self.baseline));
    }
}

fn launch_status_outcome(
    status: &Value,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
    begun: bool,
) -> Result<Option<LaunchOutcome>> {
    if let Some(error) = status
        .get("lastError")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    {
        if !begun {
            return Ok(Some(LaunchOutcome::Ignored(format!(
                "Studio refused the start: {error}"
            ))));
        }
        bail!(
            "Studio could not start the {} session: {error}",
            plan.session_kind()
        );
    }
    if status.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
        bail!(
            "Studio switched to a different {} session while starting",
            plan.session_kind()
        );
    }
    ensure_plugin_api_ok(status)?;
    Ok(None)
}

fn started_result(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
    clients: Vec<Value>,
) -> Value {
    let mut result = json!({
        "ok": true,
        "action": "start",
        "mode": plan.plugin_mode(),
        "launchNonce": launch.nonce,
        "editRuntimeId": launch.edit_runtime_id,
        "editPid": bridge.studio_pid_for_runtime(BridgeTarget::Edit, &launch.edit_runtime_id).ok(),
        "clients": clients,
    });
    if let Some(players) = plan.players() {
        result["players"] = json!(players);
    }
    result
}

fn start_timeout_error(
    bridge: &BridgeServer,
    plan: PlayLaunchPlan,
    progress: (bool, usize),
    start_result: &Value,
    last_status: &Value,
    clients: &[Value],
) -> anyhow::Error {
    let (server_ready, client_count) = progress;
    let text = |value: &Value| serde_json::to_string(value).unwrap_or_default();
    match plan {
        PlayLaunchPlan::Multi(players) => anyhow::anyhow!(
            "Timed out waiting for the multiplayer test instances to connect \
             (server ready: {server_ready}, clients connected: {client_count}/{players}). \
             Start request result: {start_result}; connected bridges: {}",
            text(&json!(clients))
        ),
        PlayLaunchPlan::Play | PlayLaunchPlan::Run => anyhow::anyhow!(
            "Timed out waiting for the play session to start; last status: {}, connected bridges: {}",
            text(last_status),
            text(&json!(bridge.list_bridge_clients()))
        ),
    }
}

fn launch_play_session(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
) -> Result<LaunchOutcome> {
    let mut watch = LaunchWatch::new(bridge, launch);
    let start_result = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        plan.request(&launch.nonce),
        BridgeTarget::Edit,
        &launch.edit_runtime_id,
        None,
    )?;
    if start_result.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
        bail!("Studio started a different {} session", plan.session_kind());
    }
    if start_result.get("ok").and_then(Value::as_bool) == Some(false)
        && start_result.get("starting").and_then(Value::as_bool) != Some(true)
    {
        ensure_plugin_api_ok(&start_result)?;
    }
    let started = Instant::now();
    let mut last_progress = started;
    let mut last_seen = (false, 0usize);
    loop {
        let status = studio_play_status_for_runtime(bridge, &launch.edit_runtime_id);
        let last_status = match &status {
            Ok(status) => {
                if let Some(outcome) = launch_status_outcome(status, launch, plan, watch.begun)? {
                    return Ok(outcome);
                }
                status.clone()
            }
            Err(error) => json!({ "error": format!("{error:#}") }),
        };
        let clients = test_launch_clients(bridge, launch);
        let server_ready = clients
            .iter()
            .any(|entry| entry["role"] == BRIDGE_ROLE_PLAY_SERVER);
        let client_count = clients
            .iter()
            .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
            .count();
        if server_ready && client_count >= plan.clients_needed() {
            return Ok(LaunchOutcome::Started(started_result(
                bridge, launch, plan, clients,
            )));
        }
        watch.observe(status.as_ref().ok(), &clients);
        let executing = status
            .as_ref()
            .ok()
            .and_then(|status| status.get("executing"))
            .and_then(Value::as_bool);
        if plan.start_was_ignored(started.elapsed(), watch.begun, executing) {
            return Ok(LaunchOutcome::Ignored(if executing == Some(false) {
                "Studio returned from the start without opening the test".to_string()
            } else {
                format!(
                    "no sign of the test {} s after the start request",
                    plan.sign_timeout().as_secs()
                )
            }));
        }
        if (server_ready, client_count) != last_seen {
            last_seen = (server_ready, client_count);
            last_progress = Instant::now();
        }
        if Instant::now() >= plan.start_deadline(started, last_progress) {
            return Err(start_timeout_error(
                bridge,
                plan,
                last_seen,
                &start_result,
                &last_status,
                &clients,
            ));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// The way a running session was launched, from the Edit window's play
/// controller; None when Renium did not launch it.
fn running_session_plan(
    status: &Value,
    clients: &[Value],
    remembered: Option<PlayLaunchPlan>,
) -> Option<PlayLaunchPlan> {
    status
        .get("launchNonce")
        .and_then(Value::as_str)
        .filter(|nonce| !nonce.is_empty())?;
    match status.get("mode").and_then(Value::as_str)? {
        "play" => Some(PlayLaunchPlan::Play),
        "run" => Some(PlayLaunchPlan::Run),
        "multi" => status
            .get("players")
            .and_then(Value::as_u64)
            .and_then(|players| u32::try_from(players).ok())
            .or_else(|| remembered.and_then(PlayLaunchPlan::players))
            .or_else(|| {
                let connected = clients
                    .iter()
                    .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
                    .count();
                u32::try_from(connected).ok().filter(|count| *count > 0)
            })
            .map(PlayLaunchPlan::Multi),
        _ => None,
    }
}

const RESTART_OUTSIDE_RENIUM: &str = "The running session was not started by Renium, so its mode and player count are unknown; it restarted as ordinary Play (add -p N or --mode to choose)";
const RESTART_NOTHING_KNOWN: &str = "No session was running and none started by this Renium daemon is known for this Edit window; it started ordinary Play (add -p N or --mode to choose)";

fn restart_plan(
    args: &StartStopPlayArgs,
    previous: Option<PlayLaunchPlan>,
    running: bool,
) -> (PlayLaunchPlan, Option<&'static str>) {
    if let Some(players) = args.players {
        return (PlayLaunchPlan::Multi(players), None);
    }
    if let Some(mode) = args.mode.as_deref() {
        let plan = if matches!(mode, "run" | "server") {
            PlayLaunchPlan::Run
        } else {
            PlayLaunchPlan::Play
        };
        return (plan, None);
    }
    match previous {
        Some(plan) => (plan, None),
        None if running => (PlayLaunchPlan::Play, Some(RESTART_OUTSIDE_RENIUM)),
        None => (PlayLaunchPlan::Play, Some(RESTART_NOTHING_KNOWN)),
    }
}

fn restart_play_result(args: StartStopPlayArgs, bridge: &BridgeServer) -> Result<Value> {
    let edit_runtime_id = bridge
        .runtime_pin_for_selector(BridgeTarget::Edit, None)?
        .runtime_id;
    let status = studio_play_status_for_runtime(bridge, &edit_runtime_id)?;
    let (clients, _) = play_clients_by_state(bridge, &edit_runtime_id);
    let running = play_status_is_running(&status) || !clients.is_empty();
    let remembered = remembered_launch(&edit_runtime_id);
    let previous = if running {
        running_session_plan(&status, &clients, remembered)
    } else {
        remembered
    };
    let (plan, note) = restart_plan(&args, previous, running);
    let restarted = running
        && stop_studio_play_with_bridge_result(bridge)?
            .get("ok")
            .and_then(Value::as_bool)
            == Some(true);
    let mut result = start_play_with_retries(bridge, plan)?;
    result["serverReady"] = json!(server_answers_luau(bridge));
    result["restarted"] = json!(restarted);
    if let Some(note) = note {
        result["restartNote"] = json!(note);
    }
    Ok(result)
}

#[cfg(windows)]
fn resolve_player_window(
    bridge: &BridgeServer,
    player: Option<&str>,
    viewport: Option<(i32, i32)>,
) -> Result<(input_inject::StudioWindow, i32, i32)> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
    let viewport = recover_client_viewport(bridge, player, pid, viewport)?;
    let window = input_inject::window_for_pid(pid, viewport)?;
    Ok((window, 0, 0))
}

fn client_viewport_size(bridge: &BridgeServer, player: Option<&str>) -> Option<(i32, i32)> {
    let result = bridge
        .call_for_selector("getMouseLocation", json!({}), BridgeTarget::Client, player)
        .ok()?;
    let width = result.get("viewportWidth").and_then(Value::as_f64)?;
    let height = result.get("viewportHeight").and_then(Value::as_f64)?;
    Some((width.round() as i32, height.round() as i32))
}

#[cfg(any(windows, target_os = "macos"))]
fn result_viewport_size(result: &Value) -> Option<(i32, i32)> {
    let width = result.get("viewportWidth").and_then(Value::as_f64)?;
    let height = result.get("viewportHeight").and_then(Value::as_f64)?;
    Some((width.round() as i32, height.round() as i32))
}

#[cfg(windows)]
fn recover_client_viewport(
    bridge: &BridgeServer,
    player: Option<&str>,
    pid: u32,
    mut viewport: Option<(i32, i32)>,
) -> Result<Option<(i32, i32)>> {
    if !viewport.is_some_and(|(width, height)| width <= 1 || height <= 1) {
        return Ok(viewport);
    }
    input_inject::recover_stalled_window_for_pid(pid)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
        viewport = client_viewport_size(bridge, player);
        if viewport.is_some_and(|(width, height)| width > 1 && height > 1) {
            break;
        }
    }
    Ok(viewport)
}

#[cfg(windows)]
fn input_delta(
    _bridge: &BridgeServer,
    _player: Option<&str>,
    _window: &input_inject::StudioWindow,
    _x: i32,
    _y: i32,
) -> (i32, i32) {
    (0, 0)
}

#[cfg(target_os = "macos")]
fn recover_client_viewport(
    _bridge: &BridgeServer,
    _player: Option<&str>,
    _pid: u32,
    viewport: Option<(i32, i32)>,
) -> Result<Option<(i32, i32)>> {
    Ok(viewport)
}

#[cfg(any(windows, target_os = "macos"))]
fn resolve_client_capture_window(
    bridge: &BridgeServer,
    player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
    let edit_pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    if pid == edit_pid {
        return resolve_edit_window(bridge, BridgeTarget::Client);
    }
    let viewport = client_viewport_size(bridge, player);
    let viewport = recover_client_viewport(bridge, player, pid, viewport)?;
    input_inject::window_for_pid(pid, viewport)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn resolve_client_capture_window(
    _bridge: &BridgeServer,
    _player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio screenshots are only supported on Windows and macOS")
}

/// Pointer input goes through the game's own virtual input: it moves the
/// game's mouse, so hover, `Mouse.Target` and ClickDetector events follow,
/// and the cursor and window focus stay untouched. RENIUM_INPUT_OS restores
/// window-level injection on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn os_input_preferred() -> bool {
    cfg!(windows) && std::env::var_os("RENIUM_INPUT_OS").is_some_and(|value| value != "0")
}

/// Copies the plugin's note that a pointer event was delivered through a
/// hidden system element, where a player could not have clicked.
fn note_system_ui(result: &mut Value, response: &Value) {
    if let Some(through) = response
        .get("throughSystemUi")
        .filter(|value| !value.is_null())
    {
        result["throughSystemUi"] = through.clone();
    }
}

fn send_virtual_input(
    bridge: &BridgeServer,
    player: Option<&str>,
    actions: Vec<Value>,
    expect_activated_id: Option<&str>,
) -> Result<Value> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _shield = {
        let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
        let window = input_inject::window_for_pid(pid, client_viewport_size(bridge, player))?;
        input_inject::input_shield(&window)?
    };
    let mut params = json!({ "actions": actions });
    if let Some(id) = expect_activated_id {
        params["expectActivatedId"] = Value::String(id.to_string());
    }
    let result =
        bridge.call_for_selector("sendVirtualInput", params, BridgeTarget::Client, player)?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

fn virtual_click_actions(
    x: i32,
    y: i32,
    right: bool,
    hold_ms: u64,
    move_pointer: bool,
) -> Vec<Value> {
    let button = if right { "right" } else { "left" };
    let mut actions = Vec::with_capacity(if move_pointer { 6 } else { 4 });
    if move_pointer {
        actions.extend([
            json!({ "type": "move", "x": x, "y": y }),
            json!({ "type": "wait", "ms": 0 }),
        ]);
    }
    actions.extend([
        json!({ "type": "button", "x": x, "y": y, "button": button, "down": true }),
        json!({ "type": "wait", "ms": hold_ms.min(10_000) }),
        json!({ "type": "button", "x": x, "y": y, "button": button, "down": false }),
        json!({ "type": "wait", "ms": 0 }),
    ]);
    actions
}

fn studio_device_status(bridge: &BridgeServer) -> Result<Value> {
    let result = bridge.call_for_target(
        "deviceSimulator",
        json!({ "action": "capture-status", "details": true, "includeSettle": true }),
        BridgeTarget::Edit,
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

fn studio_capture_status(bridge: &BridgeServer) -> Option<Value> {
    studio_device_status(bridge).ok()
}

#[cfg(windows)]
fn set_capture_probe_phase(
    bridge: &BridgeServer,
    target: BridgeTarget,
    phase: u8,
    colors: &[u32],
) -> Result<()> {
    let action = match phase {
        0 => "start",
        1 => "phase",
        2 => "stop",
        _ => bail!("Invalid capture probe phase {phase}"),
    };
    let result = bridge.call_for_target(
        "captureViewportProbe",
        json!({ "action": action, "colors": colors }),
        target,
    )?;
    ensure_plugin_api_ok(&result)
}

#[cfg(windows)]
fn resolve_edit_window(
    bridge: &BridgeServer,
    probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::verified_studio_window_for_pid(pid, |phase, colors| {
        set_capture_probe_phase(bridge, probe_target, phase, colors)
    })
}

#[cfg(target_os = "macos")]
fn resolve_edit_window(
    bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::window_for_pid(pid, None)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn resolve_edit_window(
    _bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio screenshots are only supported on Windows and macOS")
}

fn gui_input_bounds(
    bridge: &BridgeServer,
    player: Option<&str>,
    path: Option<&str>,
    id: Option<&str>,
    requested: &str,
) -> Result<(Value, f64, f64)> {
    let read = || {
        bridge.call_for_selector(
            "getGuiBounds",
            json!({ "path": path, "id": id, "scroll": true }),
            BridgeTarget::Client,
            player,
        )
    };
    let bounds = read()?;
    ensure_plugin_api_ok(&bounds)?;
    #[cfg(any(windows, target_os = "macos"))]
    let bounds =
        if result_viewport_size(&bounds).is_some_and(|(width, height)| width <= 1 || height <= 1) {
            let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
            recover_client_viewport(bridge, player, pid, result_viewport_size(&bounds))?;
            let bounds = read()?;
            ensure_plugin_api_ok(&bounds)?;
            bounds
        } else {
            bounds
        };
    let subject = bounds
        .get("fullName")
        .and_then(Value::as_str)
        .unwrap_or(requested);
    if bounds.get("onScreen").and_then(Value::as_bool) == Some(false) {
        bail!(
            "{subject} could not be brought on screen (auto-scroll was attempted; it is clipped \
             by a non-scrolling container or positioned outside the viewport)"
        );
    }
    if bounds.get("visible").and_then(Value::as_bool) == Some(false) {
        bail!("GUI element {subject} is not visible");
    }
    if bounds.get("hitTest").and_then(Value::as_bool) == Some(false) {
        if let Some(blocker) = bounds.get("blockedBy").and_then(Value::as_str) {
            bail!("GUI element {subject} is covered by {blocker}");
        }
        bail!("GUI element {subject} is not receiving pointer input at its center");
    }
    let x = bounds
        .get("x")
        .and_then(Value::as_f64)
        .context("getGuiBounds returned no x")?;
    let y = bounds
        .get("y")
        .and_then(Value::as_f64)
        .context("getGuiBounds returned no y")?;
    Ok((bounds, x, y))
}

pub(crate) fn press_result(args: &PressArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let requested = args
        .path
        .as_deref()
        .or(args.id.as_deref())
        .context("Provide a GUI path or --id")?;
    let (bounds, x, y) = if args.world {
        let path = args.path.as_deref().context("--world requires a path")?;
        let read = || {
            bridge.call_for_selector(
                "getWorldPoint",
                json!({ "path": path }),
                BridgeTarget::Client,
                player,
            )
        };
        let bounds = read()?;
        ensure_plugin_api_ok(&bounds)?;
        #[cfg(any(windows, target_os = "macos"))]
        let bounds = if result_viewport_size(&bounds)
            .is_some_and(|(width, height)| width <= 1 || height <= 1)
        {
            let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
            recover_client_viewport(bridge, player, pid, result_viewport_size(&bounds))?;
            let bounds = read()?;
            ensure_plugin_api_ok(&bounds)?;
            bounds
        } else {
            bounds
        };
        if bounds.get("onScreen").and_then(Value::as_bool) == Some(false) {
            let subject = bounds
                .get("fullName")
                .and_then(Value::as_str)
                .unwrap_or(requested);
            bail!(
                "{subject} is not on screen (behind the camera or outside the viewport); move \
                 the character or camera first (rbx goto)"
            );
        }
        let x = bounds
            .get("x")
            .and_then(Value::as_f64)
            .context("getWorldPoint returned no x")?;
        let y = bounds
            .get("y")
            .and_then(Value::as_f64)
            .context("getWorldPoint returned no y")?;
        (bounds, x, y)
    } else {
        gui_input_bounds(
            bridge,
            player,
            args.path.as_deref(),
            args.id.as_deref(),
            requested,
        )?
    };
    let mut result = json!({
        "ok": true,
        "action": "press",
        "target": bounds
            .get("fullName")
            .cloned()
            .unwrap_or_else(|| Value::String(requested.to_string())),
        "ordinalPath": bounds.get("ordinalPath").cloned().unwrap_or(Value::Null),
        "id": bounds.get("id").cloned().unwrap_or(Value::Null),
        "matchedCount": bounds.get("matchedCount").cloned().unwrap_or(Value::Null),
        "viewportX": x,
        "viewportY": y,
    });
    if !args.world && !args.right {
        let id = bounds
            .get("id")
            .and_then(Value::as_str)
            .context("The target GuiButton has no stable id")?;
        let response = send_virtual_input(
            bridge,
            player,
            virtual_click_actions(x.round() as i32, y.round() as i32, false, args.hold, false),
            Some(id),
        )?;
        result["inputMethod"] = json!("virtual");
        note_system_ui(&mut result, &response);
        return Ok(result);
    }
    #[cfg(windows)]
    if os_input_preferred() {
        let viewport = match (
            bounds.get("viewportWidth").and_then(Value::as_f64),
            bounds.get("viewportHeight").and_then(Value::as_f64),
        ) {
            (Some(width), Some(height)) if width >= 1.0 && height >= 1.0 => {
                Some((width.round() as i32, height.round() as i32))
            }
            _ => None,
        };
        let (window, offset_x, offset_y) = resolve_player_window(bridge, player, viewport)?;
        let _shield = input_inject::input_shield(&window)?;
        let (delta_x, delta_y) =
            input_delta(bridge, player, &window, x.round() as i32, y.round() as i32);
        input_inject::post_mouse_click(
            &window,
            x.round() as i32 + offset_x + delta_x,
            y.round() as i32 + offset_y + delta_y,
            args.right,
            args.hold,
        )?;
        result["inputMethod"] = json!("os");
        result["window"] = json!(window.label);
        return Ok(result);
    }
    let response = send_virtual_input(
        bridge,
        player,
        virtual_click_actions(
            x.round() as i32,
            y.round() as i32,
            args.right,
            args.hold,
            true,
        ),
        None,
    )?;
    result["inputMethod"] = json!("virtual");
    note_system_ui(&mut result, &response);
    Ok(result)
}

pub(crate) fn click_result(args: &ClickArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let mut result = json!({
        "ok": true,
        "action": "click",
        "viewportX": args.x,
        "viewportY": args.y,
    });
    #[cfg(windows)]
    if os_input_preferred() {
        let (window, offset_x, offset_y) =
            resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
        let _shield = input_inject::input_shield(&window)?;
        let (delta_x, delta_y) = input_delta(bridge, player, &window, args.x, args.y);
        input_inject::post_mouse_click(
            &window,
            args.x + offset_x + delta_x,
            args.y + offset_y + delta_y,
            args.right,
            args.hold,
        )?;
        result["inputMethod"] = json!("os");
        result["window"] = json!(window.label);
        return Ok(result);
    }
    let response = send_virtual_input(
        bridge,
        player,
        virtual_click_actions(args.x, args.y, args.right, args.hold, true),
        None,
    )?;
    result["inputMethod"] = json!("virtual");
    note_system_ui(&mut result, &response);
    Ok(result)
}

pub(crate) fn key_result(args: &KeyArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let key = input_inject::resolve_key(&args.key)?;
    if key.name == "Escape" {
        bail!(
            "Escape is reserved by Roblox CoreGui and cannot be injected; use the game's on-screen control or an alternate key, and `rbx inp dismiss` to close a Roblox prompt"
        );
    }
    #[cfg(windows)]
    if os_input_preferred() {
        let hold_ms = args.hold_ms.clamp(10, 2000);
        let (window, _, _) =
            resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
        let _shield = input_inject::input_shield(&window)?;
        input_inject::post_key(&window, &key, hold_ms)?;
        return Ok(json!({
            "ok": true,
            "action": "key",
            "key": key.name,
            "holdMs": hold_ms,
            "inputMethod": "os",
            "window": window.label,
        }));
    }
    let hold_ms = args.hold_ms.clamp(10, MAX_KEY_HOLD_MS);
    let mut actions = vec![json!({ "type": "key", "key": key.name, "down": true })];
    let mut remaining = hold_ms;
    while remaining > 0 {
        let step = remaining.min(MAX_VIRTUAL_WAIT_MS);
        actions.push(json!({ "type": "wait", "ms": step }));
        remaining -= step;
    }
    actions.push(json!({ "type": "key", "key": key.name, "down": false }));
    send_virtual_input(bridge, player, actions, None)?;
    Ok(json!({
        "ok": true,
        "action": "key",
        "key": key.name,
        "holdMs": hold_ms,
        "inputMethod": "virtual",
    }))
}

/// A key can be held for a minute; the plugin runs each wait for at most
/// ten seconds, so longer holds are several waits.
const MAX_KEY_HOLD_MS: u64 = 60_000;
const MAX_VIRTUAL_WAIT_MS: u64 = 10_000;

pub(crate) fn ui_result(args: &UiArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let read = || {
        bridge.call_for_selector(
            "getGuiInventory",
            json!({ "limit": args.limit, "includeOffscreen": args.include_offscreen }),
            BridgeTarget::Client,
            player,
        )
    };
    let result = read()?;
    ensure_plugin_api_ok(&result)?;
    #[cfg(any(windows, target_os = "macos"))]
    let result =
        if !args.include_offscreen && result.get("count").and_then(Value::as_u64) == Some(0) {
            let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
            let viewport = client_viewport_size(bridge, player);
            if viewport.is_some_and(|(width, height)| width <= 1 || height <= 1) {
                recover_client_viewport(bridge, player, pid, viewport)?;
                let result = read()?;
                ensure_plugin_api_ok(&result)?;
                result
            } else {
                result
            }
        } else {
            result
        };
    Ok(result)
}

pub(crate) fn type_result(args: &TypeArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    #[cfg(windows)]
    if os_input_preferred() {
        let (pressed, click, viewport) = if let Some(path) = args.path.as_ref() {
            let (bounds, x, y) = gui_input_bounds(bridge, player, Some(path), None, path)?;
            if bounds.get("className").and_then(Value::as_str) != Some("TextBox") {
                bail!("{path} is not a TextBox");
            }
            (
                bounds
                    .get("fullName")
                    .cloned()
                    .unwrap_or_else(|| Value::String(path.clone())),
                Some((x.round() as i32, y.round() as i32)),
                result_viewport_size(&bounds),
            )
        } else {
            (Value::Null, None, client_viewport_size(bridge, player))
        };
        let (window, offset_x, offset_y) = resolve_player_window(bridge, player, viewport)?;
        let _shield = input_inject::input_shield(&window)?;
        if let Some((x, y)) = click {
            let (delta_x, delta_y) = input_delta(bridge, player, &window, x, y);
            input_inject::post_mouse_click(
                &window,
                x + offset_x + delta_x,
                y + offset_y + delta_y,
                false,
                30,
            )?;
            thread::sleep(Duration::from_millis(250));
        }
        input_inject::post_text(&window, &args.text)?;
        if args.enter {
            let enter = input_inject::resolve_key("Enter")?;
            input_inject::post_key(&window, &enter, 40)?;
        }
        return Ok(json!({
            "ok": true,
            "action": "type",
            "chars": args.text.chars().count(),
            "focused": pressed,
            "enter": args.enter,
            "inputMethod": "os",
            "window": window.label,
        }));
    }
    let mut pressed = Value::Null;
    let mut actions = Vec::new();
    if let Some(path) = args.path.as_ref() {
        let (bounds, _, _) = gui_input_bounds(bridge, player, Some(path), None, path)?;
        if bounds.get("className").and_then(Value::as_str) != Some("TextBox") {
            bail!("{path} is not a TextBox");
        }
        pressed = bounds
            .get("fullName")
            .cloned()
            .unwrap_or_else(|| Value::String(path.clone()));
        actions.push(json!({
            "type": "focus",
            "id": bounds.get("id").context("The target TextBox has no stable id")?,
        }));
    }
    actions.push(json!({ "type": "text", "text": args.text }));
    if args.enter {
        actions.extend([
            json!({ "type": "wait", "ms": 0 }),
            json!({ "type": "key", "key": "Return", "down": true }),
            json!({ "type": "wait", "ms": 40 }),
            json!({ "type": "key", "key": "Return", "down": false }),
        ]);
    }
    send_virtual_input(bridge, player, actions, None)?;
    Ok(json!({
        "ok": true,
        "action": "type",
        "chars": args.text.chars().count(),
        "focused": pressed,
        "enter": args.enter,
        "inputMethod": "virtual",
    }))
}

pub(crate) fn wait_until_result(args: &WaitUntilArgs, bridge: &BridgeServer) -> Result<Value> {
    let client = args.client || args.player.is_some();
    let player = args.player.as_deref();
    let target = BridgeTarget::main_or_client(client);
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    if !args.timeout.is_finite() || !args.interval.is_finite() {
        bail!("--timeout and --interval must be finite numbers");
    }
    let timeout = args.timeout.clamp(0.1, 3600.0);
    let interval = args.interval.clamp(0.05, 10.0);
    let started = Instant::now();
    let mut detail = Value::Null;
    loop {
        let remaining = timeout - started.elapsed().as_secs_f64();
        if remaining <= 0.0 {
            bail!(
                "Timed out after {timeout}s waiting for condition (last value: {detail}); \
                 raise -t, or use rbx l --detach for a sampler"
            );
        }
        let slice = remaining.min(WAIT_RUNNER_SLICE_SECONDS);
        let code = format!(
            "local deadline = os.clock() + {slice}\n\
             local detail = nil\n\
             \twhile true do\n\
             \t\tlocal ok, value = pcall(function() return ({condition}) end)\n\
             \t\tdetail = value\n\
             \t\tif ok and value then return true, tostring(value) end\n\
             \t\tif os.clock() >= deadline then return false, tostring(detail) end\n\
             \t\ttask.wait({interval})\n\
             \tend",
            condition = args.condition,
        );
        let outcome = run_luau_task(bridge, target, player, &code, slice + 5.0)?;
        if outcome.success {
            return Ok(json!({
                "ok": true,
                "action": "wait",
                "condition": args.condition,
                "value": outcome.detail,
                "elapsedSeconds": started.elapsed().as_secs_f64(),
            }));
        }
        detail = outcome.detail;
    }
}

// Studio stops a runner after 120 s; each slice stays under that so a long
// wait keeps checking instead of dying with the runner.
const WAIT_RUNNER_SLICE_SECONDS: f64 = 100.0;

struct LuauTaskOutcome {
    success: bool,
    detail: Value,
    elapsed: f64,
}

fn run_luau_task(
    bridge: &BridgeServer,
    target: BridgeTarget,
    player: Option<&str>,
    code: &str,
    timeout: f64,
) -> Result<LuauTaskOutcome> {
    let started = Instant::now();
    let result = call_execute_luau(bridge, target, player, code, "ReniumTask", timeout, 2.0)?;
    ensure_luau_api_ok(&result)?;
    let results = result
        .get("results")
        .and_then(Value::as_array)
        .context("Luau task returned no results")?;
    let success = results
        .first()
        .and_then(Value::as_bool)
        .context("Luau task returned an invalid status")?;
    Ok(LuauTaskOutcome {
        success,
        detail: results.get(1).cloned().unwrap_or(Value::Null),
        elapsed: started.elapsed().as_secs_f64(),
    })
}

pub(crate) fn goto_result(args: &GotoArgs, bridge: &BridgeServer) -> Result<Value> {
    const ARRIVAL_RADIUS: f64 = 8.0;

    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let (x, y, z, label) = if let Some(pos) = args.pos.as_ref() {
        let mut parts = pos.split(',');
        let parse = |part: Option<&str>| {
            part.and_then(|value| value.trim().parse::<f64>().ok())
                .with_context(|| format!("--pos must be X,Y,Z numbers, got '{pos}'"))
        };
        let x = parse(parts.next())?;
        let y = parse(parts.next())?;
        let z = parse(parts.next())?;
        if parts.next().is_some() {
            bail!("--pos must have exactly three components, got '{pos}'");
        }
        (x, y, z, pos.clone())
    } else {
        let path = args
            .target
            .as_deref()
            .context("Provide a part path or --pos")?;
        let point = bridge.call_for_selector(
            "getWorldPoint",
            json!({ "path": path, "approach": true }),
            BridgeTarget::Client,
            player,
        )?;
        ensure_plugin_api_ok(&point)?;
        let position = point
            .get("worldPosition")
            .and_then(Value::as_array)
            .context("getWorldPoint returned no worldPosition")?;
        let [x, y, z] = position.as_slice() else {
            bail!("getWorldPoint returned malformed worldPosition");
        };
        let x = x
            .as_f64()
            .context("getWorldPoint returned malformed worldPosition")?;
        let y = y
            .as_f64()
            .context("getWorldPoint returned malformed worldPosition")?;
        let z = z
            .as_f64()
            .context("getWorldPoint returned malformed worldPosition")?;
        let label = point
            .get("fullName")
            .and_then(Value::as_str)
            .unwrap_or(path)
            .to_string();
        (x, y, z, label)
    };
    if !x.is_finite()
        || !y.is_finite()
        || !z.is_finite()
        || !args.timeout.is_finite()
        || !args.speed_multiplier.is_finite()
    {
        bail!("Position coordinates, --timeout, and --speed-multiplier must be finite numbers");
    }
    let timeout = args.timeout.clamp(1.0, 300.0);
    let speed_multiplier = args.speed_multiplier.clamp(0.1, 10.0);
    let movement = if args.tp {
        format!(
            "\tch:PivotTo(CFrame.new(targetPos + Vector3.new(0, 4, 0)))\n\
             \tlocal dist = (ch:GetPivot().Position - targetPos).Magnitude\n\
             \treturn dist < {ARRIVAL_RADIUS}, dist"
        )
    } else {
        format!(
            "\tlocal deadline = os.clock() + {timeout}\n\
             \tlocal function distance()\n\
             \t\treturn (ch:GetPivot().Position - targetPos).Magnitude\n\
             \tend\n\
             \tlocal PathfindingService = game:GetService('PathfindingService')\n\
             \twhile os.clock() < deadline do\n\
             \t\tlocal dist = distance()\n\
             \t\tif dist < {ARRIVAL_RADIUS} then return true, dist end\n\
             \t\tlocal path = PathfindingService:CreatePath()\n\
             \t\tlocal okCompute = pcall(function()\n\
             \t\t\tpath:ComputeAsync(ch:GetPivot().Position, targetPos)\n\
             \t\tend)\n\
             \t\tif okCompute and path.Status == Enum.PathStatus.Success then\n\
             \t\t\tfor _, waypoint in ipairs(path:GetWaypoints()) do\n\
             \t\t\t\tif os.clock() >= deadline or distance() < {ARRIVAL_RADIUS} then break end\n\
             \t\t\t\tif waypoint.Action == Enum.PathWaypointAction.Jump then hum.Jump = true end\n\
             \t\t\t\thum:MoveTo(waypoint.Position)\n\
             \t\t\t\tlocal reached = false\n\
             \t\t\t\tlocal conn = hum.MoveToFinished:Connect(function() reached = true end)\n\
             \t\t\t\tlocal waitDeadline = os.clock() + 4\n\
             \t\t\t\twhile not reached and os.clock() < waitDeadline do task.wait(0.1) end\n\
             \t\t\t\tconn:Disconnect()\n\
             \t\t\tend\n\
             \t\telse\n\
             \t\t\thum:MoveTo(targetPos)\n\
             \t\t\ttask.wait(1)\n\
             \t\tend\n\
             \t\ttask.wait(0.2)\n\
             \tend\n\
             \tlocal dist = distance()\n\
             \treturn dist < {ARRIVAL_RADIUS}, dist"
        )
    };
    let code = format!(
        "local targetPos = Vector3.new({x}, {y}, {z})\n\
         \tlocal lp = game:GetService('Players').LocalPlayer\n\
         \tlocal ch = lp.Character or lp.CharacterAdded:Wait()\n\
         \tlocal hum = ch:FindFirstChildOfClass('Humanoid')\n\
         \tif hum == nil then return false, 'no humanoid' end\n\
         \tlocal originalSpeed = hum.WalkSpeed\n\
         \thum.WalkSpeed = originalSpeed * {speed_multiplier}\n\
         \tlocal ok, reached, detail = pcall(function()\n\
         {movement}\n\
         \tend)\n\
         \thum.WalkSpeed = originalSpeed\n\
         \tif not ok then error(reached) end\n\
         \treturn reached, detail"
    );
    let outcome = run_luau_task(bridge, BridgeTarget::Client, player, &code, timeout + 5.0)?;
    if outcome.success {
        let final_distance = outcome
            .detail
            .as_f64()
            .context("Goto returned a malformed final distance")?;
        Ok(json!({
            "ok": true,
            "action": if args.tp { "teleport" } else { "goto" },
            "target": label,
            "position": [x, y, z],
            "arrivalRadius": ARRIVAL_RADIUS,
            "finalDistance": final_distance,
            "speedMultiplier": speed_multiplier,
            "elapsedSeconds": outcome.elapsed,
        }))
    } else {
        bail!(
            "Character did not reach {label} within {timeout}s ({})",
            outcome.detail
        )
    }
}

pub(crate) fn goto_command(args: GotoArgs) -> Result<()> {
    run_input_command(
        op::GOTO,
        json!({
            "target": args.target,
            "pos": args.pos,
            "player": args.player,
            "tp": args.tp,
            "timeout": args.timeout,
            "speedMultiplier": args.speed_multiplier,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn shot_result(args: &ShotArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let studio_status = studio_capture_status(bridge);
    let simulated = studio_status
        .as_ref()
        .and_then(|status| status.get("simulating"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let play_running = studio_status
        .as_ref()
        .and_then(|status| status.get("playRunning"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    #[cfg(any(windows, target_os = "macos"))]
    let selected_client_is_studio = player.is_none()
        || bridge
            .studio_pid_for_selector(BridgeTarget::Client, player)
            .ok()
            == bridge
                .studio_pid_for_selector(BridgeTarget::Edit, None)
                .ok();
    #[cfg(not(any(windows, target_os = "macos")))]
    let selected_client_is_studio = false;
    let use_simulator = simulated && !args.client && selected_client_is_studio;
    if use_simulator {
        let settle_seconds = studio_status
            .as_ref()
            .and_then(|status| status.get("settleSeconds"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .clamp(0.0, 5.0);
        if settle_seconds > 0.0 {
            thread::sleep(Duration::from_secs_f64(settle_seconds));
        }
    }
    let client_ready =
        bridge.channel_count_for_target(BridgeTarget::Client) >= bridge.expected_channel_count();
    let use_studio =
        args.studio || use_simulator || (player.is_none() && !args.client && !client_ready);
    let probe_target = if play_running {
        BridgeTarget::Client
    } else {
        BridgeTarget::Edit
    };
    let (window, target, bridge_target) = if use_studio {
        (
            resolve_edit_window(bridge, probe_target)?,
            "studio",
            BridgeTarget::Edit,
        )
    } else {
        (
            resolve_client_capture_window(bridge, player)?,
            "play-client",
            BridgeTarget::Client,
        )
    };
    let output = if args.output.is_absolute() {
        args.output.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&args.output)
    };
    let camera = match (&args.camera_position, &args.look_at) {
        (Some(position), Some(look_at)) => Some((
            parse_vector3(position, "--camera-position")?,
            parse_vector3(look_at, "--look-at")?,
        )),
        (None, None) => None,
        _ => bail!("--camera-position and --look-at must be used together"),
    };
    let token = if let Some((position, look_at)) = camera {
        let result = bridge.call_for_selector(
            "cameraCapture",
            json!({ "action": "prepare", "position": position, "lookAt": look_at }),
            bridge_target,
            player,
        )?;
        ensure_plugin_api_ok(&result)?;
        Some(
            result
                .get("token")
                .and_then(Value::as_str)
                .context("cameraCapture returned no token")?
                .to_string(),
        )
    } else {
        None
    };
    let capture = (|| {
        if token.is_some() {
            // The camera setter acknowledges the property change before rendering.
            // Cross two real frame boundaries while the camera lease is still held.
            let result = call_execute_luau(
                bridge,
                bridge_target,
                player,
                "local runService = game:GetService(\"RunService\")\nrunService.Heartbeat:Wait()\nrunService.Heartbeat:Wait()",
                "ReniumCaptureFrame",
                3.0,
                1.0,
            )?;
            ensure_luau_api_ok(&result)?;
        }
        input_inject::capture_window_png(&window, &output)
    })();
    let restore = token.map(|token| {
        bridge.call_for_selector(
            "cameraCapture",
            json!({ "action": "restore", "token": token }),
            bridge_target,
            player,
        )
    });
    let (width, height) = capture?;
    if let Some(result) = restore {
        ensure_plugin_api_ok(&result?)?;
    }
    Ok(json!({
        "ok": true,
        "action": "shot",
        "path": output.display().to_string(),
        "width": width,
        "height": height,
        "window": window.label,
        "target": target,
        "deviceSimulation": use_simulator,
    }))
}

fn test_launch_clients(bridge: &BridgeServer, launch: &TestLaunch) -> Vec<Value> {
    bridge
        .list_bridge_clients()
        .into_iter()
        .filter(|entry| {
            entry.get("launchNonce").and_then(Value::as_str) == Some(launch.nonce.as_str())
                && entry.get("launchEditRuntimeId").and_then(Value::as_str)
                    == Some(launch.edit_runtime_id.as_str())
                && (entry["role"] == BRIDGE_ROLE_PLAY_SERVER
                    || entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
        })
        .collect()
}

fn parse_vector3(text: &str, label: &str) -> Result<[f64; 3]> {
    let values = text
        .split(',')
        .map(|value| value.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("{label} must contain three comma-separated numbers"))?;
    let [x, y, z] = values.as_slice() else {
        bail!("{label} must contain exactly three numbers");
    };
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        bail!("{label} values must be finite");
    }
    Ok([*x, *y, *z])
}

fn compact_json(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, entry| !entry.is_null());
    }
    value
}

fn run_input_command(
    operation: u16,
    mut parameters: Value,
    bridge_args: &BridgeConnectionArgs,
    compact: bool,
) -> Result<()> {
    let object = parameters
        .as_object_mut()
        .context("Input operation parameters must be an object")?;
    object.insert(
        "bridgeWaitSeconds".to_string(),
        json!(bridge_args.wait_seconds),
    );
    object.insert("bridgePorts".to_string(), json!(bridge_args.ports));
    let result = daemon_control_request(operation, None, parameters, false)?;
    let result = if compact {
        compact_json(result)
    } else {
        result
    };
    print_json_output(&result, false)
}

pub(crate) fn press_command(args: PressArgs) -> Result<()> {
    run_input_command(
        op::PRESS,
        json!({
            "path": args.path,
            "id": args.id,
            "player": args.player,
            "right": args.right,
            "world": args.world,
            "hold": args.hold,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn click_command(args: ClickArgs) -> Result<()> {
    run_input_command(
        op::CLICK,
        json!({
            "x": args.x,
            "y": args.y,
            "player": args.player,
            "right": args.right,
            "hold": args.hold,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn key_command(args: KeyArgs) -> Result<()> {
    run_input_command(
        op::KEY,
        json!({ "key": args.key, "player": args.player, "holdMs": args.hold_ms }),
        &args.bridge,
        true,
    )
}

pub(crate) fn ui_command(args: UiArgs) -> Result<()> {
    run_input_command(
        op::UI,
        json!({
            "player": args.player,
            "limit": args.limit,
            "includeOffscreen": args.include_offscreen,
        }),
        &args.bridge,
        false,
    )
}

pub(crate) fn type_command(args: TypeArgs) -> Result<()> {
    run_input_command(
        op::TYPE,
        json!({
            "text": args.text,
            "path": args.path,
            "player": args.player,
            "enter": args.enter,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn wait_until_command(args: WaitUntilArgs) -> Result<()> {
    run_input_command(
        op::WAIT,
        json!({
            "condition": args.condition,
            "player": args.player,
            "client": args.client,
            "timeout": args.timeout,
            "interval": args.interval,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn shot_command(args: ShotArgs) -> Result<()> {
    let output = if args.output.is_absolute() {
        args.output.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&args.output)
    };
    run_input_command(
        op::SHOT,
        json!({
            "output": output,
            "player": args.player,
            "studio": args.studio,
            "client": args.client,
            "cameraPosition": args.camera_position,
            "lookAt": args.look_at,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn record_start_command(args: RecordStartArgs) -> Result<()> {
    let result = daemon_result(
        op::RECORD_START,
        None,
        json!({
            "output": args.output,
            "player": args.player,
            "studio": args.studio,
            "client": args.client,
            "fps": args.fps,
            "maxSeconds": args.max_seconds,
            "quality": args.quality,
        }),
        false,
        None,
    )?;
    print_json_output(&result, false)
}

pub(crate) fn record_end_command(args: RecordEndArgs) -> Result<()> {
    let mut result = try_daemon_control_request(
        op::RECORD_END,
        json!({
            "recordingId": args.recording_id,
        }),
    )?
    .context("No Renium recording is active")?;
    if !args.no_review {
        recording_review::attach_overview(&mut result);
    }
    print_json_output(&result, false)
}

pub(crate) fn list_clients_command(args: ListClientsArgs) -> Result<()> {
    let result = daemon_result(op::STUDIOS, None, json!({}), false, Some(&args.bridge))?;
    let mut clients = result
        .get("clients")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Ports and channels are daemon plumbing; a runtime id only selects
    // between several Studios.
    let single = clients.len() == 1;
    for client in clients.iter_mut().filter_map(Value::as_object_mut) {
        client.remove("channels");
        client.remove("ports");
        if single {
            client.remove("runtimeId");
        }
    }
    let mut output = json!({ "clients": clients });
    if let Some(orphans) = result.get("orphans") {
        output["orphans"] = orphans.clone();
    }
    print_json_output(&output, false)
}

pub(crate) fn editor_review_decision_result(
    args: &EditorReviewDecisionArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let result = bridge.call(
        "setEditorPushReviewDecision",
        json!({
            "reviewId": args.review_id,
            "decision": args.decision,
        }),
    )?;
    if result.get("accepted").and_then(Value::as_bool) != Some(true) {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Studio has no matching review awaiting a decision");
        bail!("{error}");
    }
    Ok(result)
}

pub(crate) fn editor_review_decision_command(args: EditorReviewDecisionArgs) -> Result<()> {
    let parameters = json!({
        "studioDecision": true,
        "decision": args.decision,
        "reviewId": args.review_id,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let result = try_daemon_control_request(op::REVIEW_APPLY, parameters)?
        .context("No Renium review is active")?;
    print_json_output(&result, false)
}

fn single_play_launch_nonce(clients: &[Value]) -> Option<String> {
    let mut nonces = clients
        .iter()
        .filter_map(|entry| {
            entry
                .get("launchNonce")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    nonces.sort_unstable();
    nonces.dedup();
    (nonces.len() == 1).then(|| nonces.remove(0))
}

fn studio_play_clients(bridge: &BridgeServer, edit_runtime_id: &str) -> Vec<Value> {
    let clients = bridge.list_bridge_clients();
    #[cfg(any(windows, target_os = "macos"))]
    let same_process_runtime_ids = bridge
        .studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id)
        .map(|edit_pid| {
            clients
                .iter()
                .filter_map(|entry| {
                    let role = entry.get("role").and_then(Value::as_str)?;
                    let target = match role {
                        BRIDGE_ROLE_PLAY_SERVER => BridgeTarget::Main,
                        BRIDGE_ROLE_PLAY_CLIENT => BridgeTarget::Client,
                        _ => return None,
                    };
                    let runtime_id = entry.get("runtimeId").and_then(Value::as_str)?;
                    (bridge.studio_pid_for_runtime(target, runtime_id).ok() == Some(edit_pid))
                        .then(|| runtime_id.to_string())
                })
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    #[cfg(not(any(windows, target_os = "macos")))]
    let same_process_runtime_ids = HashSet::<String>::new();

    clients
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.get("role").and_then(Value::as_str),
                Some(BRIDGE_ROLE_PLAY_SERVER | BRIDGE_ROLE_PLAY_CLIENT)
            ) && (entry.get("launchEditRuntimeId").and_then(Value::as_str) == Some(edit_runtime_id)
                || entry
                    .get("runtimeId")
                    .and_then(Value::as_str)
                    .is_some_and(|runtime_id| same_process_runtime_ids.contains(runtime_id)))
        })
        .collect()
}

fn runtime_stopped_state(
    bridge: &BridgeServer,
    target: BridgeTarget,
    runtime_id: &str,
) -> Option<bool> {
    bridge
        .call_for_runtime_with_timeout(
            "startStopPlay",
            json!({}),
            target,
            runtime_id,
            Some(Duration::from_millis(500)),
        )
        .ok()
        .map(|status| play_status_is_stopped(&status))
}

fn play_client_stopped_state(bridge: &BridgeServer, client: &Value) -> Option<bool> {
    let runtime_id = client.get("runtimeId").and_then(Value::as_str)?;
    let target = match client.get("role").and_then(Value::as_str) {
        Some(BRIDGE_ROLE_PLAY_SERVER) => BridgeTarget::Main,
        Some(BRIDGE_ROLE_PLAY_CLIENT) => BridgeTarget::Client,
        _ => return None,
    };
    runtime_stopped_state(bridge, target, runtime_id)
}

fn play_runtime_is_active(edit_stopped: Option<bool>, play_stopped: Option<bool>) -> bool {
    match play_stopped {
        Some(stopped) => !stopped,
        None => edit_stopped != Some(true),
    }
}

fn play_status_is_running(status: &Value) -> bool {
    status.get("running").and_then(Value::as_bool) == Some(true)
        || status.get("starting").and_then(Value::as_bool) == Some(true)
}

fn play_status_is_stopped(status: &Value) -> bool {
    !play_status_is_running(status)
        && status.get("running").and_then(Value::as_bool) == Some(false)
        && status.get("readyForStart").and_then(Value::as_bool) == Some(true)
}

fn retire_play_clients(bridge: &BridgeServer, clients: &[Value]) {
    for client in clients {
        if let Some(runtime_id) = client.get("runtimeId").and_then(Value::as_str) {
            bridge.retire_runtime(runtime_id);
        }
    }
}

fn request_play_runtimes_to_stop(
    bridge: &BridgeServer,
    clients: &[Value],
    launch_nonce: Option<&str>,
) {
    let mut params = Map::new();
    params.insert("stop".to_string(), Value::Bool(true));
    params.insert("waitForStopped".to_string(), Value::Bool(false));
    if let Some(launch_nonce) = launch_nonce {
        params.insert(
            "launchNonce".to_string(),
            Value::String(launch_nonce.to_string()),
        );
    }
    for client in clients {
        let target = match client.get("role").and_then(Value::as_str) {
            Some(BRIDGE_ROLE_PLAY_SERVER) => BridgeTarget::Main,
            Some(BRIDGE_ROLE_PLAY_CLIENT) => BridgeTarget::Client,
            _ => continue,
        };
        let Some(runtime_id) = client.get("runtimeId").and_then(Value::as_str) else {
            continue;
        };
        let _ = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            Value::Object(params.clone()),
            target,
            runtime_id,
            Some(Duration::from_millis(1_100)),
        );
    }
}

/// This Edit window's play runtimes, split into running ones and ones whose
/// test has stopped while their DataModel is still open.
fn play_clients_by_state(bridge: &BridgeServer, edit_runtime_id: &str) -> (Vec<Value>, Vec<Value>) {
    let clients = studio_play_clients(bridge, edit_runtime_id);
    if clients.is_empty() {
        return (clients, Vec::new());
    }
    let edit_stopped = runtime_stopped_state(bridge, BridgeTarget::Edit, edit_runtime_id);
    clients.into_iter().partition(|client| {
        play_runtime_is_active(edit_stopped, play_client_stopped_state(bridge, client))
    })
}

#[cfg(any(windows, target_os = "macos"))]
fn separate_play_processes(bridge: &BridgeServer, edit_runtime: &str) -> Vec<u32> {
    let Ok(edit_pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime) else {
        return Vec::new();
    };
    studio_play_clients(bridge, edit_runtime)
        .iter()
        .filter_map(|client| {
            let target = if client["role"] == BRIDGE_ROLE_PLAY_CLIENT {
                BridgeTarget::Client
            } else {
                BridgeTarget::Main
            };
            let pid = bridge
                .studio_pid_for_runtime(target, client["runtimeId"].as_str()?)
                .ok()?;
            (pid != edit_pid).then_some(pid)
        })
        .collect()
}

#[cfg(any(windows, target_os = "macos"))]
type PlayProcessExits = Vec<process_exit::ProcessExit>;
#[cfg(not(any(windows, target_os = "macos")))]
type PlayProcessExits = Vec<std::convert::Infallible>;

#[cfg(any(windows, target_os = "macos"))]
fn watch_play_processes(bridge: &BridgeServer, edit_runtime: &str) -> Result<PlayProcessExits> {
    separate_play_processes(bridge, edit_runtime)
        .into_iter()
        .map(process_exit::ProcessExit::watch)
        .filter_map(Result::transpose)
        .collect()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn watch_play_processes(_bridge: &BridgeServer, _edit_runtime: &str) -> Result<PlayProcessExits> {
    Ok(Vec::new())
}

// A stopped test DataModel closes its bridge channels when Studio tears it down
// and its plugin unloads. A test started before that is dropped by Studio.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Splits play runtimes into those hosted by the Edit window's own process,
/// whose DataModels close without a process exit to wait for, and the rest.
fn split_in_process(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    clients: Vec<Value>,
) -> (Vec<Value>, Vec<Value>) {
    let Ok(edit_pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id) else {
        return (clients, Vec::new());
    };
    clients.into_iter().partition(|client| {
        client
            .get("pid")
            .and_then(Value::as_u64)
            .is_none_or(|pid| pid == u64::from(edit_pid))
    })
}

fn wait_for_test_datamodels_to_close(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    deadline: Instant,
) -> Duration {
    let started = Instant::now();
    let deadline = deadline.min(started + TEARDOWN_TIMEOUT);
    while Instant::now() < deadline
        && !split_in_process(
            bridge,
            edit_runtime_id,
            studio_play_clients(bridge, edit_runtime_id),
        )
        .0
        .is_empty()
    {
        thread::sleep(Duration::from_millis(100));
    }
    started.elapsed()
}

fn finish_stop(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    processes: &PlayProcessExits,
    shutdown_deadline: Instant,
    result: &mut Value,
) -> Result<()> {
    let teardown = wait_for_test_datamodels_to_close(bridge, edit_runtime_id, shutdown_deadline);
    #[cfg(any(windows, target_os = "macos"))]
    process_exit::wait(processes, shutdown_deadline)?;
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = processes;
    let closed = close_leftover_test_processes(bridge, edit_runtime_id, Duration::from_secs(5));
    retire_play_clients(bridge, &studio_play_clients(bridge, edit_runtime_id));
    bridge.clear_runtime_pins();
    if teardown >= Duration::from_millis(250) {
        result["waitedForTeardownMs"] = json!(teardown.as_millis());
    }
    if !closed.is_empty() {
        result["closedProcesses"] = json!(closed);
    }
    Ok(())
}

fn stop_studio_play_with_bridge_result(bridge: &BridgeServer) -> Result<Value> {
    let edit_pin = bridge.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
    let edit_runtime_id = edit_pin.runtime_id;
    #[cfg(windows)]
    crate::project::workflows::windows_launch::protect_process(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, &edit_runtime_id)?,
    )?;
    #[cfg(target_os = "macos")]
    crate::studio::native::serializer::protect_studio_launch(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, &edit_runtime_id)?,
    )?;
    // Closing a test DataModel precedes process exit on Windows and macOS. Its later exit
    // notification can end the next multiplayer launch if stop returns early.
    let processes = watch_play_processes(bridge, &edit_runtime_id)?;
    #[cfg(any(windows, target_os = "macos"))]
    let shutdown_deadline = Instant::now() + process_exit::STOP_TIMEOUT;
    #[cfg(not(any(windows, target_os = "macos")))]
    let shutdown_deadline = Instant::now() + Duration::from_secs(40);
    let mut initial = studio_play_status_for_runtime(bridge, &edit_runtime_id)?;
    let (mut active_clients, _) = play_clients_by_state(bridge, &edit_runtime_id);
    if play_status_is_stopped(&initial) && active_clients.is_empty() {
        finish_stop(
            bridge,
            &edit_runtime_id,
            &processes,
            shutdown_deadline,
            &mut initial,
        )?;
        return Ok(initial);
    }
    let launch_nonce = initial
        .get("launchNonce")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let launch_nonce = launch_nonce.or_else(|| single_play_launch_nonce(&active_clients));
    let mut last_status = Value::Null;
    for attempt in 1..=3 {
        let mut params = Map::new();
        params.insert("stop".to_string(), Value::Bool(true));
        if let Some(launch_nonce) = launch_nonce.as_ref() {
            params.insert(
                "launchNonce".to_string(),
                Value::String(launch_nonce.clone()),
            );
        }
        params.insert("waitForStopped".to_string(), Value::Bool(false));
        request_play_runtimes_to_stop(bridge, &active_clients, launch_nonce.as_deref());
        let stop_result = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            Value::Object(params),
            BridgeTarget::Edit,
            &edit_runtime_id,
            Some(Duration::from_secs(2)),
        );
        let stop_result = match stop_result {
            Ok(result) => result,
            Err(error) => {
                last_status = json!({ "error": format!("{error:#}") });
                continue;
            }
        };
        ensure_plugin_api_ok(&stop_result)?;
        let mut stopped = false;
        while Instant::now() < shutdown_deadline {
            let status_stopped = match studio_play_status_for_runtime(bridge, &edit_runtime_id) {
                Ok(status) => {
                    last_status = status.clone();
                    active_clients = play_clients_by_state(bridge, &edit_runtime_id).0;
                    play_status_is_stopped(&status) && active_clients.is_empty()
                }
                Err(err) => {
                    last_status = json!({ "error": format!("{:#}", err) });
                    false
                }
            };
            stopped = status_stopped;
            if stopped {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if stopped {
            let mut result = json!({
                "ok": true,
                "action": "stop",
                "method": "pluginApi",
                "attempts": attempt,
                "status": last_status,
            });
            finish_stop(
                bridge,
                &edit_runtime_id,
                &processes,
                shutdown_deadline,
                &mut result,
            )?;
            return Ok(result);
        }
        if Instant::now() >= shutdown_deadline {
            break;
        }
    }
    bail!(
        "Studio did not finish stopping after the plugin stop request; last status: {}",
        serde_json::to_string(&last_status)?
    )
}

fn studio_play_status_for_runtime(bridge: &BridgeServer, runtime_id: &str) -> Result<Value> {
    let status = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({}),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_millis(1_100)),
    )?;
    ensure_plugin_api_ok(&status)?;
    Ok(status)
}

/// Waits until the Edit window can start a test: its controller is ready and
/// the previous test's DataModels are gone. Returns the status, which reports
/// a running session when one is still active.
fn wait_for_studio_play_ready(bridge: &BridgeServer, runtime_id: &str) -> Result<Value> {
    let deadline = Instant::now() + BRIDGE_DEFAULT_RESPONSE_TIMEOUT;
    loop {
        let mut status = studio_play_status_for_runtime(bridge, runtime_id)?;
        let (clients, closing) = play_clients_by_state(bridge, runtime_id);
        if !clients.is_empty() {
            retire_play_clients(bridge, &closing);
            let launch_nonce = single_play_launch_nonce(&clients);
            status["running"] = Value::Bool(true);
            status["starting"] = Value::Bool(false);
            status["clients"] = json!(clients);
            if let Some(launch_nonce) = launch_nonce {
                status["launchNonce"] = Value::String(launch_nonce);
            }
            return Ok(status);
        }
        let (closing, separate) = split_in_process(bridge, runtime_id, closing);
        retire_play_clients(bridge, &separate);
        if play_status_is_running(&status) {
            return Ok(status);
        }
        let controller_ready = status.get("readyForStart").and_then(Value::as_bool) != Some(false);
        if controller_ready && closing.is_empty() {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            if controller_ready {
                retire_play_clients(bridge, &closing);
                return Ok(status);
            }
            bail!("Studio did not finish the previous play session before the next start");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) struct TestLaunch {
    pub(crate) nonce: String,
    pub(crate) edit_runtime_id: String,
}

#[cfg(test)]
mod play_state_tests {
    use super::*;

    #[test]
    fn runner_names_are_short_identifiers() {
        assert!(runner_name("jump-sampler_2").is_ok());
        assert!(runner_name("").is_err());
        assert!(runner_name("has space").is_err());
        assert!(runner_name(&"x".repeat(65)).is_err());
    }

    fn play_args(parts: &[&str]) -> std::result::Result<StartStopPlayArgs, clap::Error> {
        use clap::Parser;
        match crate::cli::Cli::try_parse_from(parts)?.command {
            crate::cli::Commands::StartStopPlay(args) => Ok(args),
            _ => panic!("expected play"),
        }
    }

    #[test]
    fn add_players_parses_a_count_from_one_to_eight_on_its_own() {
        let args = play_args(&["rbx", "play", "--add-players", "2"]).unwrap();
        assert_eq!(args.add_players, Some(2));
        assert!(!args.start && !args.leave);
        validate_play_args(&args).unwrap();
        for parts in [
            &["rbx", "play", "--add-players", "0"][..],
            &["rbx", "play", "--add-players", "9"],
            &["rbx", "play", "--add-players", "2", "-s"],
            &["rbx", "play", "--add-players", "2", "-x"],
            &["rbx", "play", "--add-players", "2", "-p", "3"],
            &["rbx", "play", "--add-players", "2", "--leave"],
            &["rbx", "play", "--add-players", "2", "--until", "true"],
        ] {
            assert!(play_args(parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn leave_needs_a_client_index_and_no_session_flags() {
        let args = play_args(&["rbx", "play", "--leave", "-p", "2"]).unwrap();
        assert!(args.leave && !args.start);
        assert_eq!(args.players, Some(2));
        validate_play_args(&args).unwrap();
        let missing = validate_play_args(&play_args(&["rbx", "play", "--leave"]).unwrap());
        assert!(missing.unwrap_err().to_string().contains("--leave -p N"));
        let zero = validate_play_args(&play_args(&["rbx", "play", "--leave", "-p", "0"]).unwrap());
        assert!(zero.unwrap_err().to_string().contains("start at 1"));
        for parts in [
            &["rbx", "play", "--leave", "-p", "1", "-x"][..],
            &["rbx", "play", "--leave", "-p", "1", "-s"],
            &["rbx", "play", "--leave", "-p", "1", "-r"],
            &["rbx", "play", "--leave", "-p", "1", "--mode", "run"],
        ] {
            assert!(play_args(parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn added_clients_wait_while_new_clients_keep_arriving() {
        let started = Instant::now();
        assert_eq!(
            added_clients_deadline(started, started),
            started + Duration::from_secs(30)
        );
        assert_eq!(
            added_clients_deadline(started, started + Duration::from_secs(25)),
            started + Duration::from_secs(55)
        );
        assert_eq!(
            added_clients_deadline(started, started + Duration::from_secs(110)),
            started + Duration::from_secs(120)
        );
    }

    #[test]
    fn multiplayer_start_waits_while_instances_keep_arriving() {
        let started = Instant::now();
        assert_eq!(
            multiplayer_start_deadline(started, started),
            started + Duration::from_secs(90)
        );
        assert_eq!(
            multiplayer_start_deadline(started, started + Duration::from_secs(80)),
            started + Duration::from_secs(140)
        );
        assert_eq!(
            multiplayer_start_deadline(started, started + Duration::from_secs(600)),
            started + Duration::from_secs(240)
        );
    }

    #[test]
    fn only_a_ready_controller_counts_as_stopped() {
        assert!(play_status_is_stopped(&json!({
            "running": false, "starting": false, "readyForStart": true
        })));
        assert!(!play_status_is_stopped(&json!({
            "running": false, "starting": false, "readyForStart": false
        })));
    }

    #[test]
    fn play_runtime_activity_prefers_the_runtime_over_a_stale_editor_state() {
        assert!(play_runtime_is_active(Some(true), Some(false)));
        assert!(!play_runtime_is_active(Some(true), None));
        assert!(play_runtime_is_active(Some(false), None));
        assert!(play_runtime_is_active(None, None));
    }

    #[test]
    fn stale_bridge_failure_does_not_count_as_running() {
        assert!(!play_status_is_running(&json!({
            "ok": false,
            "error": "DataModel stopped",
        })));
        assert!(play_status_is_running(&json!({ "running": true })));
    }

    #[test]
    fn kill_orphans_parses_only_on_its_own() {
        let args = play_args(&["rbx", "play", "--kill-orphans"]).unwrap();
        assert!(args.kill_orphans && !args.start && !args.stop);
        validate_play_args(&args).unwrap();
        for parts in [
            &["rbx", "play", "--kill-orphans", "-s"][..],
            &["rbx", "play", "--kill-orphans", "-x"],
            &["rbx", "play", "--kill-orphans", "-r"],
            &["rbx", "play", "--kill-orphans", "-p", "2"],
            &["rbx", "play", "--kill-orphans", "--add-players", "1"],
            &["rbx", "play", "--kill-orphans", "--leave"],
            &["rbx", "play", "--kill-orphans", "--mode", "run"],
            &["rbx", "play", "--kill-orphans", "--until", "true"],
        ] {
            assert!(play_args(parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn restart_reuses_the_launch_unless_flags_override_it() {
        let restart = play_args(&["rbx", "play", "-r"]).unwrap();
        assert!(restart.restart && restart.players.is_none());
        assert!(play_args(&["rbx", "play", "-r", "-x"]).is_err());
        let multi = Some(PlayLaunchPlan::Multi(2));
        assert_eq!(
            restart_plan(&restart, multi, true),
            (PlayLaunchPlan::Multi(2), None)
        );
        assert_eq!(
            restart_plan(&restart, Some(PlayLaunchPlan::Run), true),
            (PlayLaunchPlan::Run, None)
        );
        assert_eq!(
            restart_plan(&restart, multi, false),
            (PlayLaunchPlan::Multi(2), None)
        );
        assert_eq!(
            restart_plan(&restart, None, true),
            (PlayLaunchPlan::Play, Some(RESTART_OUTSIDE_RENIUM))
        );
        assert_eq!(
            restart_plan(&restart, None, false),
            (PlayLaunchPlan::Play, Some(RESTART_NOTHING_KNOWN))
        );
        let three = play_args(&["rbx", "play", "-r", "-p", "3"]).unwrap();
        assert_eq!(
            restart_plan(&three, multi, true),
            (PlayLaunchPlan::Multi(3), None)
        );
        let solo = play_args(&["rbx", "play", "-r", "--mode", "play"]).unwrap();
        assert_eq!(
            restart_plan(&solo, multi, true),
            (PlayLaunchPlan::Play, None)
        );
        let server = play_args(&["rbx", "play", "-r", "--mode", "server"]).unwrap();
        assert_eq!(
            restart_plan(&server, None, true),
            (PlayLaunchPlan::Run, None)
        );
    }

    #[test]
    fn the_edit_controller_names_how_its_session_was_launched() {
        let status = |mode: &str, players: Value| json!({ "running": true, "launchNonce": "multi-1", "mode": mode, "players": players });
        let clients = [
            json!({ "role": BRIDGE_ROLE_PLAY_SERVER }),
            json!({ "role": BRIDGE_ROLE_PLAY_CLIENT }),
            json!({ "role": BRIDGE_ROLE_PLAY_CLIENT }),
        ];
        assert_eq!(
            running_session_plan(&status("multi", json!(3)), &clients, None),
            Some(PlayLaunchPlan::Multi(3))
        );
        assert_eq!(
            running_session_plan(
                &status("multi", Value::Null),
                &clients,
                Some(PlayLaunchPlan::Multi(4))
            ),
            Some(PlayLaunchPlan::Multi(4))
        );
        assert_eq!(
            running_session_plan(&status("multi", Value::Null), &clients, None),
            Some(PlayLaunchPlan::Multi(2))
        );
        assert_eq!(
            running_session_plan(&status("multi", Value::Null), &[], None),
            None
        );
        assert_eq!(
            running_session_plan(&status("play", Value::Null), &[], None),
            Some(PlayLaunchPlan::Play)
        );
        assert_eq!(
            running_session_plan(&status("run", Value::Null), &[], None),
            Some(PlayLaunchPlan::Run)
        );
        let manual = json!({ "running": true, "mode": Value::Null });
        assert_eq!(
            running_session_plan(&manual, &clients, Some(PlayLaunchPlan::Multi(2))),
            None
        );
        let unowned = json!({ "running": true, "launchNonce": "", "mode": "play" });
        assert_eq!(running_session_plan(&unowned, &[], None), None);
    }

    #[test]
    fn an_ignored_start_is_told_apart_from_a_slow_one() {
        let second = Duration::from_secs(1);
        for plan in [PlayLaunchPlan::Play, PlayLaunchPlan::Multi(2)] {
            let silent = plan.sign_timeout();
            assert!(!plan.start_was_ignored(second, false, Some(true)));
            assert!(!plan.start_was_ignored(second, false, Some(false)));
            assert!(plan.start_was_ignored(START_RETURN_GRACE, false, Some(false)));
            assert!(!plan.start_was_ignored(START_RETURN_GRACE, false, Some(true)));
            assert!(!plan.start_was_ignored(START_RETURN_GRACE, false, None));
            assert!(!plan.start_was_ignored(silent - second, false, Some(true)));
            assert!(plan.start_was_ignored(silent, false, Some(true)));
            assert!(plan.start_was_ignored(silent, false, None));
            assert!(!plan.start_was_ignored(silent * 10, true, Some(false)));
        }
        assert_eq!(PlayLaunchPlan::Run.sign_timeout(), SINGLE_START_TIMEOUT);
        assert!(PlayLaunchPlan::Multi(1).sign_timeout() > SINGLE_START_TIMEOUT);
    }

    #[test]
    fn launch_plans_ask_studio_for_their_own_session() {
        assert_eq!(
            PlayLaunchPlan::Multi(2).request("n"),
            json!({ "start": true, "players": 2, "launchNonce": "n" })
        );
        assert_eq!(
            PlayLaunchPlan::Run.request("n"),
            json!({ "start": true, "mode": "run", "launchNonce": "n" })
        );
        assert_eq!(PlayLaunchPlan::Play.clients_needed(), 1);
        assert_eq!(PlayLaunchPlan::Run.clients_needed(), 0);
        assert_eq!(PlayLaunchPlan::Multi(3).clients_needed(), 3);
        let started = Instant::now();
        assert_eq!(
            PlayLaunchPlan::Play.start_deadline(started, started + Duration::from_secs(5)),
            started + SINGLE_START_TIMEOUT
        );
        assert_eq!(
            PlayLaunchPlan::Multi(2).start_deadline(started, started),
            multiplayer_start_deadline(started, started)
        );
    }

    #[test]
    fn a_start_error_before_the_test_appears_is_retried() {
        let launch = TestLaunch {
            nonce: "multi-7".into(),
            edit_runtime_id: "edit".into(),
        };
        let plan = PlayLaunchPlan::Multi(2);
        let refused = json!({ "ok": true, "lastError": "Studio is busy" });
        match launch_status_outcome(&refused, &launch, plan, false).unwrap() {
            Some(LaunchOutcome::Ignored(reason)) => assert!(reason.contains("Studio is busy")),
            _ => panic!("a refusal before the test began must be retried"),
        }
        let failed = launch_status_outcome(&refused, &launch, plan, true)
            .err()
            .unwrap();
        assert!(
            failed
                .to_string()
                .contains("could not start the multiplayer session")
        );
        let other = json!({ "ok": true, "launchNonce": "multi-8" });
        assert!(launch_status_outcome(&other, &launch, plan, false).is_err());
        let current = json!({ "ok": true, "launchNonce": "multi-7", "starting": true });
        assert!(
            launch_status_outcome(&current, &launch, plan, false)
                .unwrap()
                .is_none()
        );
    }
}
