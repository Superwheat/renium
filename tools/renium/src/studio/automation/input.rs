#[cfg(any(windows, target_os = "macos"))]
use std::thread;
#[cfg(any(windows, target_os = "macos"))]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(not(any(windows, target_os = "macos")))]
use super::virtual_click_actions;
#[cfg(any(windows, target_os = "macos"))]
use super::{client_viewport_size, input_delta, resolve_player_window};
use super::{ensure_plugin_api_ok, send_virtual_input, wait_for_player_bridge};
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::studio::input as input_inject;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InputRequest {
    player: Option<String>,
    actions: Vec<InputAction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InputAction {
    action: InputActionKind,
    #[serde(alias = "key_code")]
    key: Option<String>,
    #[serde(alias = "text_inputs")]
    text: Option<String>,
    #[serde(alias = "instance_path")]
    path: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
    #[serde(alias = "mouse_button")]
    button: Option<MouseButton>,
    #[serde(alias = "wait_time_ms", alias = "hold_ms")]
    ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum InputActionKind {
    #[serde(alias = "keyDown")]
    KeyDown,
    #[serde(alias = "keyUp")]
    KeyUp,
    #[serde(alias = "keyPress")]
    KeyPress,
    #[serde(alias = "textInput")]
    Text,
    #[serde(alias = "moveTo")]
    Move,
    #[serde(alias = "mouseButtonDown")]
    MouseDown,
    #[serde(alias = "mouseButtonUp")]
    MouseUp,
    #[serde(alias = "mouseButtonClick")]
    Click,
    #[serde(alias = "scrollUp")]
    ScrollUp,
    #[serde(alias = "scrollDown")]
    ScrollDown,
    Wait,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MouseButton {
    Left,
    Right,
}

fn semantic_click_batch(request: &InputRequest) -> Option<Vec<Value>> {
    request
        .actions
        .iter()
        .map(|action| match action.action {
            InputActionKind::Click
                if action.path.is_some()
                    && !matches!(action.button, Some(MouseButton::Right))
                    && action.x.is_none()
                    && action.y.is_none() =>
            {
                Some(json!({
                    "type": "click",
                    "path": action.path,
                    "holdMs": action.ms.unwrap_or(30).min(10_000),
                }))
            }
            InputActionKind::Wait => {
                Some(json!({ "type": "wait", "ms": action.ms.unwrap_or(0).min(10_000) }))
            }
            _ => None,
        })
        .collect()
}

fn semantic_click_batch_result(
    request: &InputRequest,
    bridge: &BridgeServer,
    player: Option<&str>,
) -> Result<Option<Value>> {
    let Some(actions) = semantic_click_batch(request) else {
        return Ok(None);
    };
    let result = send_virtual_input(bridge, player, actions, None)?;
    Ok(Some(json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "verifiedClicks": result.get("verifiedClicks").cloned().unwrap_or(Value::Null),
        "inputMethod": "virtual",
    })))
}

fn action_position(
    action: &InputAction,
    bridge: &BridgeServer,
    player: Option<&str>,
    previous: Option<(i32, i32)>,
) -> Result<(i32, i32)> {
    if let Some(path) = action.path.as_deref() {
        let world = path == "Workspace"
            || path == "game.Workspace"
            || path.starts_with("Workspace.")
            || path.starts_with("game.Workspace.");
        let method = if world {
            "getWorldPoint"
        } else {
            "getGuiBounds"
        };
        let params = if world {
            json!({ "path": path })
        } else {
            json!({ "path": path, "scroll": true })
        };
        let result = bridge.call_for_selector(method, params, BridgeTarget::Client, player)?;
        ensure_plugin_api_ok(&result)?;
        if result.get("onScreen").and_then(Value::as_bool) == Some(false) {
            bail!("{path} is outside the target client viewport");
        }
        let x = result
            .get("x")
            .and_then(Value::as_f64)
            .context("Input target returned no x coordinate")?;
        let y = result
            .get("y")
            .and_then(Value::as_f64)
            .context("Input target returned no y coordinate")?;
        return Ok((x.round() as i32, y.round() as i32));
    }
    match (action.x, action.y) {
        (Some(x), Some(y)) => Ok((x, y)),
        (None, None) => {
            previous.context("This input action needs x/y, path, or an earlier position")
        }
        _ => bail!("x and y must be supplied together"),
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn input_result(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
    let request: InputRequest = serde_json::from_value(parameters.clone())?;
    if request.actions.is_empty() || request.actions.len() > 256 {
        bail!("input requires 1 through 256 actions");
    }
    let player = request.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, 8.0)?;
    }
    if let Some(result) = semantic_click_batch_result(&request, bridge, player)? {
        return Ok(result);
    }
    let (window, offset_x, offset_y) =
        resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
    let _shield = input_inject::input_shield(&window)?;
    let mut position = None;
    let mut calibration = None;
    let mut used_os = false;
    let mut used_virtual = false;

    for action in &request.actions {
        match action.action {
            InputActionKind::KeyDown | InputActionKind::KeyUp | InputActionKind::KeyPress => {
                used_os = true;
                if action.path.is_some() {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    let (dx, dy) = *calibration
                        .get_or_insert_with(|| input_delta(bridge, player, &window, x, y));
                    input_inject::post_mouse_click(
                        &window,
                        x + offset_x + dx,
                        y + offset_y + dy,
                        false,
                        30,
                    )?;
                    position = Some((x, y));
                }
                let key = input_inject::resolve_key(
                    action
                        .key
                        .as_deref()
                        .context("Keyboard actions require key")?,
                )?;
                match action.action {
                    InputActionKind::KeyDown => input_inject::post_key_state(&window, &key, true)?,
                    InputActionKind::KeyUp => input_inject::post_key_state(&window, &key, false)?,
                    InputActionKind::KeyPress => {
                        input_inject::post_key(&window, &key, action.ms.unwrap_or(60))?
                    }
                    _ => unreachable!(),
                }
            }
            InputActionKind::Text => {
                used_os = true;
                if action.path.is_some() {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    let (dx, dy) = *calibration
                        .get_or_insert_with(|| input_delta(bridge, player, &window, x, y));
                    input_inject::post_mouse_click(
                        &window,
                        x + offset_x + dx,
                        y + offset_y + dy,
                        false,
                        30,
                    )?;
                    position = Some((x, y));
                }
                input_inject::post_text(
                    &window,
                    action
                        .text
                        .as_deref()
                        .context("text action requires text")?,
                )?;
            }
            InputActionKind::Move
            | InputActionKind::MouseDown
            | InputActionKind::MouseUp
            | InputActionKind::Click
            | InputActionKind::ScrollUp
            | InputActionKind::ScrollDown => {
                if matches!(action.action, InputActionKind::Click)
                    && action.path.is_some()
                    && !matches!(action.button, Some(MouseButton::Right))
                {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    send_virtual_input(
                        bridge,
                        player,
                        vec![json!({
                            "type": "click",
                            "path": action.path,
                            "holdMs": action.ms.unwrap_or(30).min(10_000),
                        })],
                        None,
                    )?;
                    used_virtual = true;
                    position = Some((x, y));
                    continue;
                }
                used_os = true;
                let (x, y) = action_position(action, bridge, player, position)?;
                let (dx, dy) =
                    *calibration.get_or_insert_with(|| input_delta(bridge, player, &window, x, y));
                let window_x = x + offset_x + dx;
                let window_y = y + offset_y + dy;
                let right = matches!(action.button, Some(MouseButton::Right));
                match action.action {
                    InputActionKind::Move => {
                        input_inject::post_mouse_move(&window, window_x, window_y)?
                    }
                    InputActionKind::MouseDown => {
                        input_inject::post_mouse_button(&window, window_x, window_y, right, true)?
                    }
                    InputActionKind::MouseUp => {
                        input_inject::post_mouse_button(&window, window_x, window_y, right, false)?
                    }
                    InputActionKind::Click => input_inject::post_mouse_click(
                        &window,
                        window_x,
                        window_y,
                        right,
                        action.ms.unwrap_or(30),
                    )?,
                    InputActionKind::ScrollUp => {
                        input_inject::post_mouse_scroll(&window, window_x, window_y, 1)?
                    }
                    InputActionKind::ScrollDown => {
                        input_inject::post_mouse_scroll(&window, window_x, window_y, -1)?
                    }
                    _ => unreachable!(),
                }
                position = Some((x, y));
            }
            InputActionKind::Wait => {
                thread::sleep(Duration::from_millis(action.ms.unwrap_or(0).min(10_000)));
            }
        }
    }
    let input_method = match (used_os, used_virtual) {
        (true, true) => "mixed",
        (false, true) => "virtual",
        _ => "os",
    };
    Ok(json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "inputMethod": input_method,
        "window": window.label,
    }))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn input_result(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
    let request: InputRequest = serde_json::from_value(parameters.clone())?;
    if request.actions.is_empty() || request.actions.len() > 256 {
        bail!("input requires 1 through 256 actions");
    }
    let player = request.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, 8.0)?;
    }
    if let Some(result) = semantic_click_batch_result(&request, bridge, player)? {
        return Ok(result);
    }
    let mut position = None;
    let mut commands = Vec::new();
    for action in &request.actions {
        match action.action {
            InputActionKind::KeyDown | InputActionKind::KeyUp | InputActionKind::KeyPress => {
                if action.path.is_some() {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    commands.extend(virtual_click_actions(x, y, false, 30, false));
                    position = Some((x, y));
                }
                let key = input_inject::resolve_key(
                    action
                        .key
                        .as_deref()
                        .context("Keyboard actions require key")?,
                )?;
                match action.action {
                    InputActionKind::KeyDown => {
                        commands.push(json!({ "type": "key", "key": key.name, "down": true }))
                    }
                    InputActionKind::KeyUp => {
                        commands.push(json!({ "type": "key", "key": key.name, "down": false }))
                    }
                    InputActionKind::KeyPress => commands.extend([
                        json!({ "type": "key", "key": key.name, "down": true }),
                        json!({ "type": "wait", "ms": action.ms.unwrap_or(60).min(10_000) }),
                        json!({ "type": "key", "key": key.name, "down": false }),
                    ]),
                    _ => unreachable!(),
                }
            }
            InputActionKind::Text => {
                if action.path.is_some() {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    commands.extend(virtual_click_actions(x, y, false, 30, false));
                    position = Some((x, y));
                }
                commands.push(json!({
                    "type": "text",
                    "text": action.text.as_deref().context("text action requires text")?,
                }));
                commands.push(json!({ "type": "wait", "ms": 0 }));
            }
            InputActionKind::Move
            | InputActionKind::MouseDown
            | InputActionKind::MouseUp
            | InputActionKind::Click
            | InputActionKind::ScrollUp
            | InputActionKind::ScrollDown => {
                let (x, y) = action_position(action, bridge, player, position)?;
                let button = if matches!(action.button, Some(MouseButton::Right)) {
                    "right"
                } else {
                    "left"
                };
                match action.action {
                    InputActionKind::Move => {
                        commands.push(json!({ "type": "move", "x": x, "y": y }))
                    }
                    InputActionKind::MouseDown => commands.extend([
                        json!({ "type": "move", "x": x, "y": y }),
                        json!({ "type": "button", "x": x, "y": y, "button": button, "down": true }),
                    ]),
                    InputActionKind::MouseUp => commands.extend([
                        json!({ "type": "move", "x": x, "y": y }),
                        json!({ "type": "button", "x": x, "y": y, "button": button, "down": false }),
                    ]),
                    InputActionKind::Click => commands.extend(virtual_click_actions(
                        x,
                        y,
                        button == "right",
                        action.ms.unwrap_or(30),
                        true,
                    )),
                    InputActionKind::ScrollUp => commands.extend([
                        json!({ "type": "move", "x": x, "y": y }),
                        json!({ "type": "scroll", "x": x, "y": y, "delta": 1 }),
                    ]),
                    InputActionKind::ScrollDown => commands.extend([
                        json!({ "type": "move", "x": x, "y": y }),
                        json!({ "type": "scroll", "x": x, "y": y, "delta": -1 }),
                    ]),
                    _ => unreachable!(),
                }
                position = Some((x, y));
            }
            InputActionKind::Wait => {
                commands.push(json!({ "type": "wait", "ms": action.ms.unwrap_or(0).min(10_000) }));
            }
        }
    }
    send_virtual_input(bridge, player, commands, None)?;
    Ok(json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "inputMethod": "virtual",
    }))
}
