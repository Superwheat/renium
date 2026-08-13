use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::app::output::{automation_token, ensure_luau_api_ok, ensure_plugin_api_ok, log_global};
use crate::app::timing::current_millis;
use crate::cli::{
    BridgeConnectionArgs, ClickArgs, EditorReviewDecisionArgs, ExecuteLuauArgs, GotoArgs, KeyArgs,
    ListClientsArgs, PressArgs, ShotArgs, StartStopPlayArgs, StudioChangeStateArgs,
    StudioDeviceArgs, TestArgs, TypeArgs, UiArgs, WaitUntilArgs,
};
use crate::daemon::{
    execute_luau_daemon_args, start_stop_play_daemon_args, studio_change_state_daemon_args,
    studio_device_daemon_args, try_daemon_control_request,
};
use crate::snapshot::export::parse_bridge_ports;
use crate::snapshot::import::parse_services;
use crate::studio::bridge::{
    BRIDGE_DEFAULT_RESPONSE_TIMEOUT, BRIDGE_ROLE_PLAY_CLIENT, BRIDGE_ROLE_PLAY_SERVER,
    BridgeServer, BridgeTarget,
};
use crate::studio::input as input_inject;

mod console;
mod input;
mod recording;

pub(crate) use console::{get_console_output_command, get_console_output_result};
pub(crate) use input::input_result;
pub(crate) use recording::{end as record_end_result, start as record_start_result};

fn console_entry_level(entry: &Value) -> &str {
    entry
        .get("type")
        .or_else(|| entry.get("level"))
        .and_then(Value::as_str)
        .unwrap_or("output")
}

pub(crate) fn execute_luau_command(args: ExecuteLuauArgs) -> Result<()> {
    if let Some(result) = try_daemon_control_request("lx", execute_luau_daemon_args(&args))? {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let target = BridgeTarget::main_or_client(args.client || args.player.is_some());
    let (bridge, _listen_metrics) = BridgeServer::listen_with_initial_wait(
        &args.bridge.host,
        &ports,
        args.bridge.wait_seconds,
        false,
    )?;
    bridge.wait_for_target(args.bridge.wait_seconds, target)?;
    let result = execute_luau_result(args, &bridge)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(crate) fn validate_luau_syntax(code: &str) -> Result<()> {
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
    Ok(())
}

pub(crate) fn execute_luau_result(args: ExecuteLuauArgs, bridge: &BridgeServer) -> Result<Value> {
    let code = if let Some(code) = args.code {
        code
    } else if let Some(path) = args.file {
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?
    } else {
        bail!("Missing Luau code. Use -e <code> or -f <file>.");
    };
    validate_luau_syntax(&code)?;
    let client = args.client || args.player.is_some();
    let timeout = args.timeout.clamp(0.1, 20.0);
    let target = BridgeTarget::main_or_client(client);
    if let Some(player) = args.player.as_deref() {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let result = bridge.call_for_selector(
        "executeLuau",
        json!({
            "code": code,
            "chunkName": "Renium",
            "context": if client { "client" } else { "plugin" },
            "timeoutSeconds": timeout,
        }),
        target,
        args.player.as_deref(),
    )?;
    ensure_luau_api_ok(&result)?;
    Ok(result)
}

pub(crate) fn studio_device_command(args: StudioDeviceArgs) -> Result<()> {
    if let Some(result) = try_daemon_control_request("device", studio_device_daemon_args(&args))? {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, _listen_metrics) = BridgeServer::listen_with_initial_wait(
        &args.bridge.host,
        &ports,
        args.bridge.wait_seconds,
        false,
    )?;
    bridge.wait_for_target(args.bridge.wait_seconds, BridgeTarget::Edit)?;
    let result = studio_device_result(&args, &bridge)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
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
    let result =
        bridge.call_for_target("deviceSimulator", Value::Object(params), BridgeTarget::Edit)?;
    ensure_plugin_api_ok(&result)?;
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
    if let Some(result) = try_daemon_control_request("play", start_stop_play_daemon_args(&args))? {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if args.stop {
        bail!("Stopping play mode requires an active Renium bridge daemon");
    }
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, _listen_metrics) =
        BridgeServer::listen(&args.bridge.host, &ports, args.bridge.wait_seconds)?;
    let result = start_stop_play_result(args, &bridge)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(crate) fn studio_change_state_command(args: StudioChangeStateArgs) -> Result<()> {
    if let Some(result) = try_daemon_control_request("st", studio_change_state_daemon_args(&args))?
    {
        println!(
            "__ROBLOX_SYNC_STUDIO_CHANGE_STATE__ {}",
            serde_json::to_string(&result)?
        );
        return Ok(());
    }
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, _listen_metrics) =
        BridgeServer::listen(&args.bridge.host, &ports, args.bridge.wait_seconds)?;
    let result = studio_change_state_result(args, &bridge)?;
    println!(
        "__ROBLOX_SYNC_STUDIO_CHANGE_STATE__ {}",
        serde_json::to_string(&result)?
    );
    Ok(())
}

pub(crate) fn studio_change_state_result(
    args: StudioChangeStateArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let services = parse_services(&args.services)?;
    let action_results: Value = serde_json::from_str(&args.ack_action_results)
        .context("--ack-action-results must be a JSON object")?;
    if !action_results.is_object() {
        bail!("--ack-action-results must be a JSON object");
    }
    let result = bridge.call(
        "getStudioChangeState",
        json!({
            "services": services,
            "reset": args.reset,
            "replaceServices": args.replace_services,
            "clearPending": args.clear_pending,
            "start": !args.no_start,
            "stop": args.stop,
            "ackSeq": args.ack_seq,
            "ackEditorActions": args.ack_actions,
            "ackEditorActionResults": action_results,
            "runtimeId": args.runtime_id,
            "suppressSeconds": args.suppress_seconds,
            "waitSeconds": args.wait_seconds,
            "contextBound": args.context_bound,
        }),
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

pub(crate) fn start_stop_play_result(
    args: StartStopPlayArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
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
    if args.stop {
        return stop_studio_play_with_bridge_result(bridge);
    }
    if let Some(players) = args.players {
        return start_multiplayer_test_result(bridge, players);
    }
    if args.start {
        return start_single_play_result(bridge, mode);
    }
    let result = bridge.call_for_target("startStopPlay", json!({}), BridgeTarget::Edit)?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

fn new_play_launch(bridge: &BridgeServer, label: &str) -> Result<TestLaunch> {
    let edit_pin = bridge.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
    let edit_runtime_id = edit_pin.runtime_id;
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
}

fn start_single_play_result(bridge: &BridgeServer, mode: &str) -> Result<Value> {
    let plugin_mode = if matches!(mode, "run" | "server") {
        "run"
    } else {
        "play"
    };
    let launch = new_play_launch(bridge, "play")?;
    let existing = studio_play_status_for_runtime(bridge, &launch.edit_runtime_id)?;
    if existing.get("running").and_then(Value::as_bool) == Some(true)
        || existing.get("starting").and_then(Value::as_bool) == Some(true)
    {
        return Ok(existing);
    }
    let result = (|| -> Result<Value> {
        let start_result = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            json!({
                "start": true,
                "mode": plugin_mode,
                "launchNonce": launch.nonce,
            }),
            BridgeTarget::Edit,
            &launch.edit_runtime_id,
            None,
        )?;
        if start_result.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
            bail!("Studio started a different play session");
        }
        if start_result.get("ok").and_then(Value::as_bool) == Some(false)
            && start_result.get("starting").and_then(Value::as_bool) != Some(true)
        {
            ensure_plugin_api_ok(&start_result)?;
        }
        if start_result.get("ok").and_then(Value::as_bool) == Some(true)
            && start_result.get("running").and_then(Value::as_bool) == Some(true)
        {
            return Ok(start_result);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last_status = start_result;
        loop {
            if Instant::now() >= deadline {
                bail!(
                    "Timed out waiting for the play session to start; last status: {}, connected bridges: {}",
                    serde_json::to_string(&last_status)?,
                    serde_json::to_string(&bridge.list_bridge_clients())?
                );
            }
            match studio_play_status_for_runtime(bridge, &launch.edit_runtime_id) {
                Ok(status) => {
                    if status.get("launchNonce").and_then(Value::as_str)
                        != Some(launch.nonce.as_str())
                    {
                        bail!("Studio switched to a different play session while starting");
                    }
                    if status.get("running").and_then(Value::as_bool) == Some(true) {
                        return Ok(status);
                    }
                    if let Some(error) = status
                        .get("lastError")
                        .and_then(Value::as_str)
                        .filter(|error| !error.is_empty())
                    {
                        bail!("Studio could not start the play session: {error}");
                    }
                    ensure_plugin_api_ok(&status)?;
                    last_status = status;
                }
                Err(err) => {
                    last_status = json!({ "error": format!("{err:#}") });
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
    })();
    if result.is_err() {
        cancel_test_launch_best_effort(bridge, &launch);
    }
    result
}

fn start_multiplayer_test_result(bridge: &BridgeServer, players: u32) -> Result<Value> {
    if !(1..=8).contains(&players) {
        bail!("Multiplayer tests require between 1 and 8 players");
    }
    let launch = new_play_launch(bridge, "multi")?;
    let existing = studio_play_status_for_runtime(bridge, &launch.edit_runtime_id)?;
    if existing.get("running").and_then(Value::as_bool) == Some(true)
        || existing.get("starting").and_then(Value::as_bool) == Some(true)
    {
        bail!("A Studio play session is already active in the selected window");
    }
    let result = (|| -> Result<Value> {
        let start_result = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            json!({
                "start": true,
                "players": players,
                "launchNonce": launch.nonce,
            }),
            BridgeTarget::Edit,
            &launch.edit_runtime_id,
            None,
        )?;
        if start_result.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
            bail!("Studio started a different multiplayer session");
        }
        if start_result.get("ok").and_then(Value::as_bool) == Some(false)
            && start_result.get("starting").and_then(Value::as_bool) != Some(true)
        {
            ensure_plugin_api_ok(&start_result)?;
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let status = studio_play_status_for_runtime(bridge, &launch.edit_runtime_id)?;
            if status.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
                bail!("Studio switched to a different multiplayer session while starting");
            }
            if let Some(error) = status
                .get("lastError")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
            {
                bail!("Studio could not start the multiplayer session: {error}");
            }
            let clients = test_launch_clients(bridge, &launch);
            let client_count = clients
                .iter()
                .filter(|entry| entry["role"] == "play-client")
                .count();
            let server_ready = clients.iter().any(|entry| entry["role"] == "play-server");
            if server_ready && client_count >= players as usize {
                return Ok(json!({
                    "ok": true,
                    "action": "start",
                    "mode": "multi",
                    "players": players,
                    "clients": clients,
                }));
            }
            if Instant::now() >= deadline {
                bail!(
                    "Timed out waiting for the multiplayer test instances to connect \
                     (server ready: {server_ready}, clients connected: {client_count}/{players}). \
                     Start request result: {start_result}; connected bridges: {}",
                    serde_json::to_string(&clients)?
                );
            }
            thread::sleep(Duration::from_millis(250));
        }
    })();
    if result.is_err() {
        cancel_test_launch_best_effort(bridge, &launch);
    }
    result
}

#[cfg(any(windows, target_os = "macos"))]
fn resolve_player_window(
    bridge: &BridgeServer,
    player: Option<&str>,
    viewport: Option<(i32, i32)>,
) -> Result<(input_inject::StudioWindow, i32, i32)> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
    let window = input_inject::window_for_pid(pid, viewport)?;
    Ok((window, 0, 0))
}

fn client_viewport_size(bridge: &BridgeServer, player: Option<&str>) -> Option<(i32, i32)> {
    let result = bridge
        .call_for_selector("getMouseLocation", json!({}), BridgeTarget::Client, player)
        .ok()?;
    let width = result.get("viewportWidth").and_then(Value::as_f64)?;
    let height = result.get("viewportHeight").and_then(Value::as_f64)?;
    if width < 1.0 || height < 1.0 {
        return None;
    }
    Some((width.round() as i32, height.round() as i32))
}

fn studio_capture_status(bridge: &BridgeServer) -> Option<Value> {
    let result = bridge
        .call_for_target(
            "deviceSimulator",
            json!({ "action": "status" }),
            BridgeTarget::Edit,
        )
        .ok()?;
    ensure_plugin_api_ok(&result).ok()?;
    Some(result)
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

#[cfg(not(any(windows, target_os = "macos")))]
fn resolve_player_window(
    _bridge: &BridgeServer,
    _player: Option<&str>,
    _viewport: Option<(i32, i32)>,
) -> Result<(input_inject::StudioWindow, i32, i32)> {
    bail!("Input injection is only supported on Windows and macOS")
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
    let bounds = if args.world {
        let path = args.path.as_deref().context("--world requires a path")?;
        bridge.call_for_selector(
            "getWorldPoint",
            json!({ "path": path }),
            BridgeTarget::Client,
            player,
        )?
    } else {
        bridge.call_for_selector(
            "getGuiBounds",
            json!({ "path": args.path, "id": args.id, "scroll": true }),
            BridgeTarget::Client,
            player,
        )?
    };
    ensure_plugin_api_ok(&bounds)?;
    if bounds.get("onScreen").and_then(Value::as_bool) == Some(false) {
        let subject = bounds
            .get("fullName")
            .and_then(Value::as_str)
            .unwrap_or(requested);
        if args.world {
            bail!(
                "{subject} is not on screen (behind the camera or outside the viewport); move \
                 the character or camera first (rbx goto)"
            );
        }
        bail!(
            "{subject} could not be brought on screen (auto-scroll was attempted; it is clipped \
             by a non-scrolling container or positioned outside the viewport)"
        );
    }
    let x = bounds
        .get("x")
        .and_then(Value::as_f64)
        .context("getGuiBounds returned no x")?;
    let y = bounds
        .get("y")
        .and_then(Value::as_f64)
        .context("getGuiBounds returned no y")?;
    if bounds.get("visible").and_then(Value::as_bool) == Some(false) {
        bail!(
            "GUI element {} is not visible",
            bounds
                .get("fullName")
                .and_then(Value::as_str)
                .unwrap_or(requested)
        );
    }
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
    let (delta_x, delta_y) =
        calibrate_click_delta(bridge, player, &window, x.round() as i32, y.round() as i32);
    let click_x = x.round() as i32 + offset_x + delta_x;
    let click_y = y.round() as i32 + offset_y + delta_y;
    input_inject::post_mouse_click(&window, click_x, click_y, args.right, args.hold)?;
    Ok(json!({
        "ok": true,
        "action": "press",
        "calibrationDelta": [delta_x, delta_y],
        "target": bounds
            .get("fullName")
            .cloned()
            .unwrap_or_else(|| Value::String(requested.to_string())),
        "ordinalPath": bounds.get("ordinalPath").cloned().unwrap_or(Value::Null),
        "id": bounds.get("id").cloned().unwrap_or(Value::Null),
        "matchedCount": bounds.get("matchedCount").cloned().unwrap_or(Value::Null),
        "viewportX": x,
        "viewportY": y,
        "window": window.label,
    }))
}

fn read_client_mouse_location(bridge: &BridgeServer, player: Option<&str>) -> Result<(f64, f64)> {
    let result =
        bridge.call_for_selector("getMouseLocation", json!({}), BridgeTarget::Client, player)?;
    ensure_plugin_api_ok(&result)?;
    Ok((
        result
            .get("x")
            .and_then(Value::as_f64)
            .context("Mouse probe returned no x coordinate")?,
        result
            .get("y")
            .and_then(Value::as_f64)
            .context("Mouse probe returned no y coordinate")?,
    ))
}

fn calibrate_click_delta(
    bridge: &BridgeServer,
    player: Option<&str>,
    window: &input_inject::StudioWindow,
    x: i32,
    y: i32,
) -> (i32, i32) {
    let Ok(initial) = read_client_mouse_location(bridge, player) else {
        return (0, 0);
    };
    if input_inject::post_mouse_move(window, x, y).is_err() {
        return (0, 0);
    }
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(20));
        if let Ok((seen_x, seen_y)) = read_client_mouse_location(bridge, player) {
            let delta_x = x - seen_x.round() as i32;
            let delta_y = y - seen_y.round() as i32;
            let moved = (seen_x - initial.0).abs() > 0.5 || (seen_y - initial.1).abs() > 0.5;
            if !moved && (delta_x.abs() > 1 || delta_y.abs() > 1) {
                continue;
            }
            if delta_x.abs() > 300 || delta_y.abs() > 300 {
                return (0, 0);
            }
            return (delta_x, delta_y);
        }
    }
    println!("[renium] warning: input position calibration was not observed by the client");
    (0, 0)
}

pub(crate) fn click_result(args: &ClickArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let (window, offset_x, offset_y) =
        resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
    let (delta_x, delta_y) = calibrate_click_delta(bridge, player, &window, args.x, args.y);
    input_inject::post_mouse_click(
        &window,
        args.x + offset_x + delta_x,
        args.y + offset_y + delta_y,
        args.right,
        args.hold,
    )?;
    Ok(json!({
        "ok": true,
        "action": "click",
        "viewportX": args.x,
        "viewportY": args.y,
        "window": window.label,
    }))
}

pub(crate) fn key_result(args: &KeyArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let key = input_inject::resolve_key(&args.key)?;
    let (window, _, _) =
        resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
    input_inject::post_key(&window, &key, args.hold_ms)?;
    Ok(json!({
        "ok": true,
        "action": "key",
        "key": key.name,
        "window": window.label,
    }))
}

pub(crate) fn ui_result(args: &UiArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let result = bridge.call_for_selector(
        "getGuiInventory",
        json!({ "limit": args.limit, "includeOffscreen": args.include_offscreen }),
        BridgeTarget::Client,
        player,
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

pub(crate) fn type_result(args: &TypeArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let mut pressed = Value::Null;
    if let Some(path) = args.path.as_ref() {
        let press_args = PressArgs {
            bridge: args.bridge.clone(),
            path: Some(path.clone()),
            id: None,
            player: args.player.clone(),
            right: false,
            world: false,
            hold: 30,
        };
        let press = press_result(&press_args, bridge)?;
        pressed = press
            .get("target")
            .cloned()
            .unwrap_or_else(|| Value::String(path.clone()));
        thread::sleep(Duration::from_millis(250));
    }
    let (window, _, _) =
        resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
    input_inject::post_text(&window, &args.text)?;
    if args.enter {
        let enter = input_inject::resolve_key("Enter")?;
        input_inject::post_key(&window, &enter, 40)?;
    }
    Ok(json!({
        "ok": true,
        "action": "type",
        "chars": args.text.chars().count(),
        "focused": pressed,
        "enter": args.enter,
        "window": window.label,
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
    let timeout = args.timeout.clamp(0.1, 600.0);
    let interval = args.interval.clamp(0.05, 10.0);
    let token = automation_token("__RENIUM_WAIT");
    let code = format!(
        "task.spawn(function()\n\
         \tlocal deadline = os.clock() + {timeout}\n\
         \twhile true do\n\
         \t\tlocal ok, value = pcall(function() return ({condition}) end)\n\
         \t\tif ok and value then print('{token}_TRUE') return end\n\
         \t\tif os.clock() >= deadline then print('{token}_END ' .. tostring(value)) return end\n\
         \t\ttask.wait({interval})\n\
         \tend\n\
         end)",
        condition = args.condition,
    );
    match run_console_task(bridge, target, player, client, &code, &token, timeout + 5.0)? {
        ConsoleTaskOutcome::True(elapsed) => Ok(json!({
            "ok": true,
            "action": "wait",
            "condition": args.condition,
            "elapsedSeconds": elapsed,
        })),
        ConsoleTaskOutcome::End(detail) => {
            bail!("Condition did not become true within {timeout}s (last value:{detail})")
        }
    }
}

enum ConsoleTaskOutcome {
    True(f64),
    End(String),
}

fn run_console_task(
    bridge: &BridgeServer,
    target: BridgeTarget,
    player: Option<&str>,
    client_context: bool,
    code: &str,
    token: &str,
    poll_timeout: f64,
) -> Result<ConsoleTaskOutcome> {
    let seed = bridge.call_for_selector(
        "getConsoleOutput",
        json!({ "cursorOnly": true }),
        target,
        player,
    )?;
    ensure_plugin_api_ok(&seed)?;
    let mut since_seq = seed.get("nextSeq").and_then(Value::as_u64).unwrap_or(0);
    let exec = bridge.call_for_selector(
        "executeLuau",
        json!({
            "code": code,
            "chunkName": "ReniumTask",
            "context": if client_context { "client" } else { "plugin" },
            "backgroundLifetimeSeconds": poll_timeout,
        }),
        target,
        player,
    )?;
    ensure_plugin_api_ok(&exec)?;
    let execution_id = exec
        .get("executionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let outcome = (|| -> Result<ConsoleTaskOutcome> {
        let true_marker = format!("{token}_TRUE");
        let end_marker = format!("{token}_END");
        let end_prefix = format!("{end_marker} ");
        let started = Instant::now();
        let deadline = started + Duration::from_secs_f64(poll_timeout);
        loop {
            let console = bridge.call_for_selector(
                "getConsoleOutput",
                json!({ "limit": 100, "sinceSeq": since_seq }),
                target,
                player,
            )?;
            ensure_plugin_api_ok(&console)?;
            if console
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                bail!("Task output was overwritten before Renium could read its completion marker");
            }
            since_seq = console
                .get("nextSeq")
                .and_then(Value::as_u64)
                .unwrap_or(since_seq);
            if let Some(entries) = console.get("entries").and_then(Value::as_array) {
                for entry in entries {
                    let Some(message) = entry.get("message").and_then(Value::as_str) else {
                        continue;
                    };
                    if message.trim() == true_marker {
                        return Ok(ConsoleTaskOutcome::True(started.elapsed().as_secs_f64()));
                    }
                    if let Some(rest) = message.trim().strip_prefix(&end_prefix) {
                        return Ok(ConsoleTaskOutcome::End(rest.trim_end().to_string()));
                    }
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "Task runner produced no result within {poll_timeout}s; the injected code may \
                     have a syntax error (check the client console)"
                );
            }
            thread::sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    if let Some(execution_id) = execution_id {
        let _ = bridge.call_for_selector(
            "cancelLuauExecution",
            json!({ "executionId": execution_id }),
            target,
            player,
        );
    }
    outcome
}

pub(crate) fn goto_result(args: &GotoArgs, bridge: &BridgeServer) -> Result<Value> {
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
            json!({ "path": path }),
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
    let token = automation_token("__RENIUM_GOTO");
    let movement = if args.tp {
        format!(
            "\tch:PivotTo(CFrame.new(targetPos + Vector3.new(0, 4, 0)))\n\
             \tlocal dist = (ch:GetPivot().Position - targetPos).Magnitude\n\
             \tif dist < 8 then print('{token}_TRUE') return end\n\
             \tprint('{token}_END dist=' .. tostring(math.floor(dist + 0.5)))"
        )
    } else {
        format!(
            "\tlocal deadline = os.clock() + {timeout}\n\
             \tlocal function arrived()\n\
             \t\treturn (ch:GetPivot().Position - targetPos).Magnitude < 8\n\
             \tend\n\
             \tlocal PathfindingService = game:GetService('PathfindingService')\n\
             \twhile os.clock() < deadline do\n\
             \t\tif arrived() then print('{token}_TRUE') return end\n\
             \t\tlocal path = PathfindingService:CreatePath()\n\
             \t\tlocal okCompute = pcall(function()\n\
             \t\t\tpath:ComputeAsync(ch:GetPivot().Position, targetPos)\n\
             \t\tend)\n\
             \t\tif okCompute and path.Status == Enum.PathStatus.Success then\n\
             \t\t\tfor _, waypoint in ipairs(path:GetWaypoints()) do\n\
             \t\t\t\tif os.clock() >= deadline or arrived() then break end\n\
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
             \tif arrived() then print('{token}_TRUE') return end\n\
             \tprint('{token}_END dist=' .. tostring(math.floor((ch:GetPivot().Position - targetPos).Magnitude + 0.5)))"
        )
    };
    let code = format!(
        "task.spawn(function()\n\
         \tlocal targetPos = Vector3.new({x}, {y}, {z})\n\
         \tlocal lp = game:GetService('Players').LocalPlayer\n\
         \tlocal ch = lp.Character or lp.CharacterAdded:Wait()\n\
         \tlocal hum = ch:FindFirstChildOfClass('Humanoid')\n\
         \tif hum == nil then print('{token}_END no humanoid') return end\n\
         \tlocal originalSpeed = hum.WalkSpeed\n\
         \thum.WalkSpeed = originalSpeed * {speed_multiplier}\n\
         \tlocal ok, err = pcall(function()\n\
         {movement}\n\
         \tend)\n\
         \thum.WalkSpeed = originalSpeed\n\
         \tif not ok then error(err) end\n\
         end)"
    );
    match run_console_task(
        bridge,
        BridgeTarget::Client,
        player,
        true,
        &code,
        &token,
        timeout + 5.0,
    )? {
        ConsoleTaskOutcome::True(elapsed) => Ok(json!({
            "ok": true,
            "action": if args.tp { "teleport" } else { "goto" },
            "target": label,
        "position": [x, y, z],
            "speedMultiplier": speed_multiplier,
            "elapsedSeconds": elapsed,
        })),
        ConsoleTaskOutcome::End(detail) => bail!(
            "Character did not reach {label} within {timeout}s ({})",
            detail.trim()
        ),
    }
}

pub(crate) fn goto_command(args: GotoArgs) -> Result<()> {
    let mut daemon_args = Vec::new();
    if let Some(target) = args.target.as_ref() {
        daemon_args.push(target.clone());
    }
    if let Some(pos) = args.pos.as_ref() {
        daemon_args.push("--pos".to_string());
        daemon_args.push(pos.clone());
    }
    if args.tp {
        daemon_args.push("--tp".to_string());
    }
    daemon_args.push("-t".to_string());
    daemon_args.push(args.timeout.to_string());
    daemon_args.push("--speed-multiplier".to_string());
    daemon_args.push(args.speed_multiplier.to_string());
    run_input_command(
        "goto",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| goto_result(&args, bridge),
    )
}

pub(crate) fn shot_result(args: &ShotArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let studio_status = if player.is_none() && !args.client {
        studio_capture_status(bridge)
    } else {
        None
    };
    let simulated = studio_status
        .as_ref()
        .and_then(|status| status.get("simulating"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if simulated {
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
        args.studio || (player.is_none() && !args.client && (simulated || !client_ready));
    let probe_target = if simulated && client_ready {
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
            resolve_player_window(bridge, player, client_viewport_size(bridge, player))?.0,
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
    let capture = input_inject::capture_window_png(&window, &output);
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
        "deviceSimulation": simulated,
    }))
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

fn input_daemon_args(player: Option<&str>, rest: Vec<String>) -> Vec<String> {
    let mut out = rest;
    if let Some(player) = player {
        out.push("--player".to_string());
        out.push(player.to_string());
    }
    out
}

fn run_input_command(
    daemon_command: &str,
    daemon_args: Vec<String>,
    bridge_args: &BridgeConnectionArgs,
    compact: bool,
    direct: impl FnOnce(&BridgeServer) -> Result<Value>,
) -> Result<()> {
    let result = if let Some(result) = try_daemon_control_request(daemon_command, daemon_args)? {
        result
    } else {
        let ports = parse_bridge_ports(&bridge_args.ports)?;
        let (bridge, _listen_metrics) =
            BridgeServer::listen(&bridge_args.host, &ports, bridge_args.wait_seconds)?;
        direct(&bridge)?
    };
    let result = if compact {
        compact_json(result)
    } else {
        result
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub(crate) fn press_command(args: PressArgs) -> Result<()> {
    let mut daemon_args = Vec::new();
    if let Some(path) = args.path.as_ref() {
        daemon_args.push(path.clone());
    }
    if let Some(id) = args.id.as_ref() {
        daemon_args.push("--id".to_string());
        daemon_args.push(id.clone());
    }
    if args.right {
        daemon_args.push("--right".to_string());
    }
    if args.world {
        daemon_args.push("--world".to_string());
    }
    daemon_args.push("--hold".to_string());
    daemon_args.push(args.hold.to_string());
    run_input_command(
        "press",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| press_result(&args, bridge),
    )
}

pub(crate) fn click_command(args: ClickArgs) -> Result<()> {
    let mut daemon_args = vec![args.x.to_string(), args.y.to_string()];
    if args.right {
        daemon_args.push("--right".to_string());
    }
    daemon_args.push("--hold".to_string());
    daemon_args.push(args.hold.to_string());
    run_input_command(
        "click",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| click_result(&args, bridge),
    )
}

pub(crate) fn key_command(args: KeyArgs) -> Result<()> {
    let daemon_args = vec![
        args.key.clone(),
        "--hold-ms".to_string(),
        args.hold_ms.to_string(),
    ];
    run_input_command(
        "key",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| key_result(&args, bridge),
    )
}

pub(crate) fn ui_command(args: UiArgs) -> Result<()> {
    let mut daemon_args = vec!["-n".to_string(), args.limit.to_string()];
    if args.include_offscreen {
        daemon_args.push("--include-offscreen".to_string());
    }
    run_input_command(
        "ui",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        false,
        |bridge| ui_result(&args, bridge),
    )
}

pub(crate) fn type_command(args: TypeArgs) -> Result<()> {
    let mut daemon_args = vec![args.text.clone()];
    if let Some(path) = args.path.as_ref() {
        daemon_args.push("--path".to_string());
        daemon_args.push(path.clone());
    }
    if args.enter {
        daemon_args.push("--enter".to_string());
    }
    run_input_command(
        "type",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| type_result(&args, bridge),
    )
}

pub(crate) fn wait_until_command(args: WaitUntilArgs) -> Result<()> {
    let mut daemon_args = vec![
        args.condition.clone(),
        "-t".to_string(),
        args.timeout.to_string(),
        "--interval".to_string(),
        args.interval.to_string(),
    ];
    if args.client {
        daemon_args.push("-c".to_string());
    }
    run_input_command(
        "wait-until",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| wait_until_result(&args, bridge),
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
    let mut daemon_args = vec!["-o".to_string(), output.display().to_string()];
    if args.studio {
        daemon_args.push("--studio".to_string());
    }
    if args.client {
        daemon_args.push("--client".to_string());
    }
    run_input_command(
        "shot",
        input_daemon_args(args.player.as_deref(), daemon_args),
        &args.bridge,
        true,
        |bridge| shot_result(&args, bridge),
    )
}

pub(crate) fn list_clients_command(args: ListClientsArgs) -> Result<()> {
    if let Some(result) = try_daemon_control_request("clients", Vec::new())? {
        println!("{}", serde_json::to_string(&compact_json(result))?);
        return Ok(());
    }
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, _listen_metrics) =
        BridgeServer::listen(&args.bridge.host, &ports, args.bridge.wait_seconds)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ "clients": bridge.list_bridge_clients() }))?
    );
    Ok(())
}

fn editor_review_decision_daemon_args(args: &EditorReviewDecisionArgs) -> Vec<String> {
    let mut out = vec![
        args.decision.clone(),
        "-w".to_string(),
        args.bridge.wait_seconds.to_string(),
        "-H".to_string(),
        args.bridge.host.clone(),
        "-P".to_string(),
        args.bridge.ports.clone(),
    ];
    if let Some(review_id) = &args.review_id {
        out.push("-i".to_string());
        out.push(review_id.clone());
    }
    out
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
    let daemon_args = editor_review_decision_daemon_args(&args);
    let result = try_daemon_control_request("review", daemon_args)?
        .context("No Renium daemon is running; start `rbx bd` first")?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn stop_studio_play_with_bridge_result(bridge: &BridgeServer) -> Result<Value> {
    let edit_pin = bridge.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
    let edit_runtime_id = edit_pin.runtime_id;
    let initial = studio_play_status_for_runtime(bridge, &edit_runtime_id)?;
    if studio_status_indicates_stopped(&initial)
        && initial.get("starting").and_then(Value::as_bool) != Some(true)
    {
        return Ok(initial);
    }
    let launch_nonce = initial
        .get("launchNonce")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut last_status = Value::Null;
    let mut last_play_roles = 0;
    for attempt in 1..=3 {
        let mut params = Map::new();
        params.insert("stop".to_string(), Value::Bool(true));
        if let Some(launch_nonce) = launch_nonce.as_ref() {
            params.insert(
                "launchNonce".to_string(),
                Value::String(launch_nonce.clone()),
            );
        }
        let stop_result = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            Value::Object(params),
            BridgeTarget::Edit,
            &edit_runtime_id,
            None,
        )?;
        ensure_plugin_api_ok(&stop_result)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status_stopped = false;
        while Instant::now() < deadline {
            match studio_play_status_for_runtime(bridge, &edit_runtime_id) {
                Ok(status) => {
                    last_status = status.clone();
                    status_stopped = studio_status_indicates_stopped(&status);
                }
                Err(err) => {
                    last_status = json!({ "error": format!("{:#}", err) });
                    status_stopped = false;
                }
            }
            if status_stopped {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if status_stopped {
            let teardown_deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let (clients, play_roles) = if let Some(launch_nonce) = launch_nonce.as_ref() {
                    let launch = TestLaunch {
                        nonce: launch_nonce.clone(),
                        edit_runtime_id: edit_runtime_id.clone(),
                    };
                    let clients = test_launch_clients(bridge, &launch);
                    let count = clients.len();
                    (clients, count)
                } else {
                    (Vec::new(), 0)
                };
                last_play_roles = play_roles;
                if play_roles == 0 {
                    return Ok(json!({
                        "ok": true,
                        "action": "stop",
                        "method": "pluginApi",
                        "attempts": attempt,
                        "stopResult": stop_result,
                        "status": last_status,
                        "clients": clients,
                    }));
                }
                if Instant::now() >= teardown_deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
    if last_play_roles > 0 {
        bail!(
            "Studio reported edit mode but {} play bridge(s) are still connected after stop; last status: {}, connected bridges: {}",
            last_play_roles,
            serde_json::to_string(&last_status)?,
            serde_json::to_string(&bridge.list_bridge_clients())?
        );
    }
    bail!(
        "Studio did not report edit mode after plugin stop request; last status: {}",
        serde_json::to_string(&last_status)?
    )
}

fn studio_play_status_for_runtime(bridge: &BridgeServer, runtime_id: &str) -> Result<Value> {
    let status = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({}),
        BridgeTarget::Edit,
        runtime_id,
        None,
    )?;
    ensure_plugin_api_ok(&status)?;
    Ok(status)
}

fn studio_status_indicates_stopped(status: &Value) -> bool {
    if let Some(running) = status.get("running").and_then(Value::as_bool) {
        return !running;
    }
    status
        .get("studioTest")
        .and_then(|value| value.get("editModeActive"))
        .and_then(Value::as_bool)
        == Some(true)
}

pub(crate) fn test_command(args: TestArgs) -> Result<()> {
    if !matches!(args.mode.as_str(), "play" | "run" | "server") {
        bail!(
            "Invalid test mode '{}'; use play, run, or server",
            args.mode
        );
    }
    if args.players.is_some() && args.mode != "play" {
        bail!("--players can only be used with --mode play");
    }
    let mut daemon_args = vec![
        "--mode".to_string(),
        args.mode,
        "--timeout".to_string(),
        args.timeout.to_string(),
    ];
    if let Some(players) = args.players {
        daemon_args.push("--players".to_string());
        daemon_args.push(players.to_string());
    }
    if args.fail_on_error {
        daemon_args.push("--fail-on-error".to_string());
    }
    if let Some(player) = args.player {
        daemon_args.push("--player".to_string());
        daemon_args.push(player);
    }
    let result = try_daemon_control_request("test", daemon_args)?
        .context("No Renium daemon is running; start `rbx bd` first")?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        let count = result
            .get("errors")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        bail!("Test console reported {count} error(s)");
    }
    Ok(())
}

#[derive(Default)]
struct TestConsoleCursor {
    epoch: Option<String>,
    seq: u64,
}

#[derive(Default)]
struct TestConsoleCapture {
    cursors: HashMap<String, TestConsoleCursor>,
    errors: Vec<String>,
    truncated: bool,
}

pub(crate) struct TestLaunch {
    pub(crate) nonce: String,
    pub(crate) edit_runtime_id: String,
}

fn ingest_test_console_payload(
    runtime_id: &str,
    label: &str,
    capture: &mut TestConsoleCapture,
    console: &Value,
    restart_on_epoch_change: bool,
) -> bool {
    capture.truncated |= console.get("truncated").and_then(Value::as_bool) == Some(true);
    let next_epoch = console
        .get("epoch")
        .and_then(Value::as_str)
        .map(str::to_string);
    let previous_epoch = capture
        .cursors
        .get(runtime_id)
        .and_then(|cursor| cursor.epoch.clone());
    if previous_epoch.is_some() && previous_epoch != next_epoch {
        let cursor = capture.cursors.entry(runtime_id.to_string()).or_default();
        cursor.epoch.clone_from(&next_epoch);
        cursor.seq = 0;
        if restart_on_epoch_change {
            return true;
        }
    }
    let previous_seq = capture
        .cursors
        .get(runtime_id)
        .map_or(0, |cursor| cursor.seq);
    let mut highest_seq = previous_seq;
    if let Some(entries) = console.get("entries").and_then(Value::as_array) {
        for entry in entries {
            let entry_seq = entry.get("seq").and_then(Value::as_u64);
            if entry_seq.is_some_and(|seq| seq <= previous_seq) {
                continue;
            }
            if let Some(seq) = entry_seq {
                highest_seq = highest_seq.max(seq);
            }
            let level = console_entry_level(entry);
            let message = entry
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            println!("[{label} {level}] {message}");
            if level.eq_ignore_ascii_case("error") || level.eq_ignore_ascii_case("messageerror") {
                capture.errors.push(format!("{label}: {message}"));
            }
        }
    }
    highest_seq = highest_seq.max(
        console
            .get("nextSeq")
            .and_then(Value::as_u64)
            .unwrap_or(highest_seq),
    );
    let cursor = capture.cursors.entry(runtime_id.to_string()).or_default();
    cursor.epoch = next_epoch;
    cursor.seq = highest_seq;
    false
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

fn drain_test_console(
    bridge: &BridgeServer,
    target: BridgeTarget,
    runtime_id: &str,
    label: &str,
    capture: &mut TestConsoleCapture,
    deadline: Instant,
) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let cursor_seq = capture
            .cursors
            .get(runtime_id)
            .map_or(0, |cursor| cursor.seq);
        let console = bridge.call_for_runtime_with_timeout(
            "getConsoleOutput",
            json!({
                "limit": 200,
                "sinceSeq": cursor_seq,
                "fromOldest": cursor_seq == 0,
                "clear": false,
            }),
            target,
            runtime_id,
            Some(remaining.min(BRIDGE_DEFAULT_RESPONSE_TIMEOUT)),
        )?;
        ensure_plugin_api_ok(&console)?;
        if ingest_test_console_payload(runtime_id, label, capture, &console, true) {
            continue;
        }
        if console.get("hasMore").and_then(Value::as_bool) != Some(true) {
            return Ok(());
        }
        let next_seq = capture
            .cursors
            .get(runtime_id)
            .map_or(0, |cursor| cursor.seq);
        if next_seq <= cursor_seq {
            bail!("Studio console page cursor did not advance");
        }
    }
}

fn test_client_matches_selector(entry: &Value, selector: &str) -> bool {
    if entry["role"] != BRIDGE_ROLE_PLAY_CLIENT {
        return false;
    }
    let name = entry
        .get("playerName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.eq_ignore_ascii_case(selector) {
        return true;
    }
    selector.parse::<i64>().ok().is_some_and(|index| {
        name.eq_ignore_ascii_case(&format!("Player{index}"))
            || entry.get("playerUserId").and_then(Value::as_i64) == Some(-index)
    })
}

fn drain_test_consoles(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    capture: &mut TestConsoleCapture,
    deadline: Instant,
) -> Result<Vec<Value>> {
    let clients = test_launch_clients(bridge, launch);
    for entry in &clients {
        let Some(runtime_id) = entry.get("runtimeId").and_then(Value::as_str) else {
            bail!("A launched Studio bridge has no runtime identity");
        };
        let role = entry
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let target = if role == BRIDGE_ROLE_PLAY_CLIENT {
            BridgeTarget::Client
        } else {
            BridgeTarget::Main
        };
        let player = entry
            .get("playerName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        let label = player.map_or_else(|| role.to_string(), |name| format!("{role}:{name}"));
        drain_test_console(bridge, target, runtime_id, &label, capture, deadline)?;
    }
    for final_snapshot in bridge.take_final_console_snapshots(launch) {
        let runtime_id = final_snapshot
            .get("runtimeId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let role = final_snapshot
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("play");
        let player = final_snapshot
            .get("playerName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty());
        let label = player.map_or_else(|| role.to_string(), |name| format!("{role}:{name}"));
        if let Some(snapshot) = final_snapshot.get("snapshot") {
            ingest_test_console_payload(runtime_id, &label, capture, snapshot, false);
        }
    }
    Ok(clients)
}

fn start_test_session_with_deadline(
    args: &TestArgs,
    bridge: &BridgeServer,
    launch: &TestLaunch,
    deadline: Instant,
    owned: &mut bool,
) -> Result<Value> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Timed test expired before Studio could start");
    }
    let mode = if matches!(args.mode.as_str(), "run" | "server") {
        "run"
    } else {
        "play"
    };
    let start_result = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "start": true,
            "mode": mode,
            "players": args.players,
            "launchNonce": launch.nonce,
        }),
        BridgeTarget::Edit,
        &launch.edit_runtime_id,
        Some(remaining.min(BRIDGE_DEFAULT_RESPONSE_TIMEOUT)),
    );
    let start = match start_result {
        Ok(start) => start,
        Err(error) => {
            let probe_timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1));
            if !probe_timeout.is_zero()
                && let Ok(status) = bridge.call_for_runtime_with_timeout(
                    "startStopPlay",
                    json!({}),
                    BridgeTarget::Edit,
                    &launch.edit_runtime_id,
                    Some(probe_timeout),
                )
                && status.get("launchNonce").and_then(Value::as_str) == Some(launch.nonce.as_str())
            {
                *owned = true;
            }
            return Err(error);
        }
    };
    if start.get("launchNonce").and_then(Value::as_str) == Some(launch.nonce.as_str()) {
        *owned = true;
    }
    ensure_plugin_api_ok(&start)?;
    if start.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
        bail!("Studio started a different test session");
    }
    loop {
        let clients = test_launch_clients(bridge, launch);
        let server_ready = clients
            .iter()
            .any(|entry| entry["role"] == BRIDGE_ROLE_PLAY_SERVER);
        let player_count_ready = args.players.is_none_or(|players| {
            clients
                .iter()
                .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
                .count()
                >= players as usize
        });
        let selected_player_ready = args.player.as_deref().is_none_or(|selector| {
            clients
                .iter()
                .any(|entry| test_client_matches_selector(entry, selector))
        });
        let ready = server_ready && player_count_ready && selected_player_ready;
        if ready {
            return Ok(json!({
                "ok": true,
                "mode": args.mode,
                "players": args.players,
                "launchNonce": launch.nonce,
                "editRuntimeId": launch.edit_runtime_id,
                "startResult": start,
                "clients": clients,
            }));
        }
        if Instant::now() >= deadline {
            bail!("Timed test expired while waiting for Studio play mode");
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn stop_test_session_with_deadline(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    capture: &mut TestConsoleCapture,
    deadline: Instant,
) -> Result<Value> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Timed test exhausted its teardown deadline");
    }
    let initial_drain_deadline = Instant::now()
        + deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(1));
    let mut drain_error =
        drain_test_consoles(bridge, launch, capture, initial_drain_deadline).err();
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Timed test exhausted its teardown deadline before requesting stop");
    }
    let stop = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "stop": true,
            "launchNonce": launch.nonce,
            "waitForStopped": false,
        }),
        BridgeTarget::Edit,
        &launch.edit_runtime_id,
        Some(remaining.min(BRIDGE_DEFAULT_RESPONSE_TIMEOUT)),
    )?;
    ensure_plugin_api_ok(&stop)?;
    loop {
        let drain_deadline = Instant::now()
            + deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(750));
        if let Err(error) = drain_test_consoles(bridge, launch, capture, drain_deadline)
            && drain_error.is_none()
        {
            drain_error = Some(error);
        }
        if Instant::now() >= deadline {
            bail!("Studio test bridges did not stop before the teardown deadline");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let terminal = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            json!({}),
            BridgeTarget::Edit,
            &launch.edit_runtime_id,
            Some(remaining.min(BRIDGE_DEFAULT_RESPONSE_TIMEOUT)),
        )?;
        ensure_plugin_api_ok(&terminal)?;
        let clients = test_launch_clients(bridge, launch);
        let stopped = terminal.get("running").and_then(Value::as_bool) == Some(false)
            && terminal.get("starting").and_then(Value::as_bool) != Some(true);
        if stopped && clients.is_empty() {
            let final_drain_deadline = Instant::now()
                + deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(1));
            if let Err(error) = drain_test_consoles(bridge, launch, capture, final_drain_deadline)
                && drain_error.is_none()
            {
                drain_error = Some(error);
            }
            if let Some(error) = drain_error {
                return Err(error.context("Studio stopped, but final console collection failed"));
            }
            return Ok(json!({
                "ok": true,
                "action": "stop",
                "launchNonce": launch.nonce,
                "stopResult": stop,
                "terminalResult": terminal,
                "clients": clients,
            }));
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(crate) fn timed_test_result(args: TestArgs, bridge: &BridgeServer) -> Result<Value> {
    if !matches!(args.mode.as_str(), "play" | "run" | "server") {
        bail!(
            "Invalid test mode '{}'; use play, run, or server",
            args.mode
        );
    }
    if args.players.is_some() && args.mode != "play" {
        bail!("--players can only be used with --mode play");
    }
    if !args.timeout.is_finite() || args.timeout <= 0.0 {
        bail!("--timeout must be a finite number greater than zero");
    }
    let timeout = args.timeout.clamp(0.1, 60.0 * 60.0);
    let started = Instant::now();
    let run_deadline = started + Duration::from_secs_f64(timeout);
    let launch = new_play_launch(bridge, "test")?;
    let mut console = TestConsoleCapture::default();
    let mut owns_test = false;
    let start = match start_test_session_with_deadline(
        &args,
        bridge,
        &launch,
        run_deadline,
        &mut owns_test,
    ) {
        Ok(start) => start,
        Err(error) => {
            cancel_test_launch_best_effort(bridge, &launch);
            if !owns_test {
                return Err(
                    error.context("Timed test failed to start; exact launch cleanup was requested")
                );
            }
            let teardown_deadline = Instant::now() + Duration::from_secs(10);
            let cleanup =
                stop_test_session_with_deadline(bridge, &launch, &mut console, teardown_deadline);
            return match cleanup {
                Ok(_) => Err(error.context("Timed test failed to start; cleanup completed")),
                Err(cleanup_error) => Err(error.context(format!(
                    "Timed test failed to start and cleanup also failed: {cleanup_error:#}"
                ))),
            };
        }
    };
    let run_result = (|| -> Result<()> {
        while Instant::now() < run_deadline {
            drain_test_consoles(bridge, &launch, &mut console, run_deadline)?;
            thread::sleep(
                Duration::from_millis(250)
                    .min(run_deadline.saturating_duration_since(Instant::now())),
            );
        }
        Ok(())
    })();
    let teardown_deadline = Instant::now() + Duration::from_secs(10);
    let stop_result =
        stop_test_session_with_deadline(bridge, &launch, &mut console, teardown_deadline);
    if let Err(run_error) = run_result {
        return match stop_result {
            Ok(_) => Err(run_error),
            Err(stop_error) => Err(run_error.context(format!(
                "Timed test execution failed and teardown also failed: {stop_error:#}"
            ))),
        };
    }
    let stop = stop_result?;
    let terminal_error = stop
        .pointer("/terminalResult/lastError")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map(str::to_string);
    if let Some(message) = terminal_error.as_ref() {
        console
            .errors
            .push(format!("Studio test service: {message}"));
    }
    if console.truncated {
        let message = "Studio console history was truncated during the timed test".to_string();
        if args.fail_on_error {
            console.errors.push(message);
        } else {
            log_global(2, format_args!("{message}"));
        }
    }
    Ok(json!({
        "ok": terminal_error.is_none() && (!args.fail_on_error || console.errors.is_empty()),
        "mode": args.mode,
        "players": args.players,
        "launchNonce": launch.nonce,
        "editRuntimeId": launch.edit_runtime_id,
        "elapsedSeconds": started.elapsed().as_secs_f64(),
        "errors": console.errors,
        "terminalError": terminal_error,
        "consoleTruncated": console.truncated,
        "start": start,
        "stop": stop,
    }))
}
