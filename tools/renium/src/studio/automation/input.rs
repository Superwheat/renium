#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(windows)]
use super::{client_viewport_size, input_delta, os_input_preferred, resolve_player_window};
use super::{
    ensure_plugin_api_ok, note_system_ui, send_virtual_input, virtual_click_actions,
    wait_for_player_bridge,
};
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::studio::input as input_inject;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InputRequest {
    player: Option<String>,
    actions: Vec<InputAction>,
    #[serde(rename = "bridgePorts")]
    _bridge_ports: Option<String>,
    #[serde(rename = "bridgeWaitSeconds")]
    _bridge_wait_seconds: Option<f64>,
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
    Dismiss,
    Press,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MouseButton {
    Left,
    Right,
}

/// The plugin command that answers or hides the Roblox prompt at a point
/// (the viewport center by default), or presses one Roblox button.
fn core_gui_command(action: &InputAction) -> Result<Value> {
    let press = matches!(action.action, InputActionKind::Press);
    let mut command = json!({ "type": if press { "press" } else { "dismiss" } });
    match (action.x, action.y) {
        (Some(x), Some(y)) => {
            command["x"] = json!(x);
            command["y"] = json!(y);
        }
        (None, None) if press && action.text.is_none() => {
            bail!("press needs a button label or x/y")
        }
        (None, None) => {}
        _ => bail!("x and y must be supplied together"),
    }
    if press && let Some(label) = action.text.as_deref() {
        command["label"] = json!(label);
    }
    Ok(command)
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
    let response = send_virtual_input(bridge, player, actions, None)?;
    let mut result = json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "verifiedClicks": response.get("verifiedClicks").cloned().unwrap_or(Value::Null),
        "inputMethod": "virtual",
    });
    note_system_ui(&mut result, &response);
    Ok(Some(result))
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

pub(crate) fn input_result(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
    #[cfg(windows)]
    if os_input_preferred() {
        return os_input_result(parameters, bridge);
    }
    virtual_input_result(parameters, bridge)
}

#[cfg(windows)]
fn os_input_result(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
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
    let mut core_gui = None;

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
            InputActionKind::Dismiss | InputActionKind::Press => {
                let response =
                    send_virtual_input(bridge, player, vec![core_gui_command(action)?], None)?;
                core_gui = response.get("coreGui").cloned();
                used_virtual = true;
            }
        }
    }
    let input_method = match (used_os, used_virtual) {
        (true, true) => "mixed",
        (false, true) => "virtual",
        _ => "os",
    };
    let mut result = json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "inputMethod": input_method,
        "window": window.label,
    });
    if let Some(report) = core_gui {
        result["coreGui"] = report;
    }
    Ok(result)
}

fn virtual_input_result(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
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
            InputActionKind::Dismiss | InputActionKind::Press => {
                commands.push(core_gui_command(action)?);
            }
        }
    }
    let response = send_virtual_input(bridge, player, commands, None)?;
    let mut result = json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "inputMethod": "virtual",
    });
    note_system_ui(&mut result, &response);
    for key in ["heldKeys", "keysObserved", "verifiedClicks", "coreGui"] {
        if let Some(value) = response.get(key).filter(|value| !value.is_null()) {
            result[key] = value.clone();
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(value: Value) -> InputAction {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn core_gui_commands_carry_the_point_and_label() {
        assert_eq!(
            core_gui_command(&action(json!({ "action": "dismiss" }))).unwrap(),
            json!({ "type": "dismiss" })
        );
        assert_eq!(
            core_gui_command(&action(json!({ "action": "dismiss", "x": 40, "y": 700 }))).unwrap(),
            json!({ "type": "dismiss", "x": 40, "y": 700 })
        );
        assert_eq!(
            core_gui_command(&action(json!({ "action": "press", "text": "No" }))).unwrap(),
            json!({ "type": "press", "label": "No" })
        );
        assert_eq!(
            core_gui_command(&action(json!({ "action": "press", "x": 545, "y": 442 }))).unwrap(),
            json!({ "type": "press", "x": 545, "y": 442 })
        );
        assert!(core_gui_command(&action(json!({ "action": "press" }))).is_err());
        assert!(core_gui_command(&action(json!({ "action": "dismiss", "x": 1 }))).is_err());
    }
}
