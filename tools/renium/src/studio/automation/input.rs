#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    VirtualInputFailure, ensure_plugin_api_ok, note_system_ui, send_virtual_input,
    virtual_click_actions, virtual_wait_actions, wait_for_player_bridge,
};
#[cfg(windows)]
use super::{client_viewport_size, input_delta, os_input_preferred, resolve_player_window};
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

/// A path under Workspace names a part, which the plugin resolves to a
/// viewport point; only GUI paths take the plugin's verified click.
fn is_world_path(path: &str) -> bool {
    path == "Workspace"
        || path == "game.Workspace"
        || path.starts_with("Workspace.")
        || path.starts_with("game.Workspace.")
}

/// The plugin's verified click on a GUI path: when its turn in the batch
/// comes, the client resolves the path, scrolls the element into view, refuses
/// it when a player could not see or reach it, clicks it and checks that the
/// click arrived. `step` is the action's place in the request, counted from
/// one, which a refusal names.
fn gui_click_command(path: &str, hold_ms: u64, step: usize) -> Value {
    json!({ "type": "click", "path": path, "holdMs": hold_ms.min(10_000), "step": step })
}

/// The verified click for a left `click` on a GUI path; coordinates, world
/// parts and right clicks stay pointer input at a point.
fn verified_click_command(action: &InputAction, step: usize) -> Option<Value> {
    let path = action.path.as_deref()?;
    let verified = matches!(action.action, InputActionKind::Click)
        && !is_world_path(path)
        && !matches!(action.button, Some(MouseButton::Right))
        && action.x.is_none()
        && action.y.is_none();
    verified.then(|| gui_click_command(path, action.ms.unwrap_or(30), step))
}

/// Where the pointer was left for an action that names no target of its own.
enum Pointer<'a> {
    Unset,
    At(i32, i32),
    /// The element a verified click went to; its point is read only when a
    /// later action asks for it.
    Element(&'a InputAction),
}

type Locate<'a> = dyn FnMut(&InputAction, Option<(i32, i32)>) -> Result<(i32, i32)> + 'a;

fn key_commands(action: &InputAction) -> Result<Vec<Value>> {
    let key = input_inject::resolve_key(
        action
            .key
            .as_deref()
            .context("Keyboard actions require key")?,
    )?;
    let state = |down: bool| json!({ "type": "key", "key": key.name, "down": down });
    Ok(match action.action {
        InputActionKind::KeyDown => vec![state(true)],
        InputActionKind::KeyUp => vec![state(false)],
        _ => {
            let mut commands = vec![state(true)];
            commands.extend(virtual_wait_actions(action.ms.unwrap_or(60)));
            commands.push(state(false));
            commands
        }
    })
}

fn pointer_commands(action: &InputAction, x: i32, y: i32) -> Vec<Value> {
    let right = matches!(action.button, Some(MouseButton::Right));
    let button = if right { "right" } else { "left" };
    let moved = json!({ "type": "move", "x": x, "y": y });
    let pressed =
        |down: bool| json!({ "type": "button", "x": x, "y": y, "button": button, "down": down });
    let scrolled = |delta: i32| json!({ "type": "scroll", "x": x, "y": y, "delta": delta });
    match action.action {
        InputActionKind::MouseDown => vec![moved, pressed(true)],
        InputActionKind::MouseUp => vec![moved, pressed(false)],
        InputActionKind::Click => virtual_click_actions(x, y, right, action.ms.unwrap_or(30), true),
        InputActionKind::ScrollUp => vec![moved, scrolled(1)],
        InputActionKind::ScrollDown => vec![moved, scrolled(-1)],
        _ => vec![moved],
    }
}

/// The GUI path of a move, press, release or scroll that names one. The
/// client reads its point when the action's turn comes, so a target that an
/// earlier action of the list opens is found.
fn deferred_pointer_path(action: &InputAction) -> Option<&str> {
    let path = action.path.as_deref()?;
    let deferred = !matches!(action.action, InputActionKind::Click)
        && !is_world_path(path)
        && action.x.is_none()
        && action.y.is_none();
    deferred.then_some(path)
}

fn path_pointer_commands(action: &InputAction, path: &str, step: usize) -> Vec<Value> {
    let right = matches!(action.button, Some(MouseButton::Right));
    let button = if right { "right" } else { "left" };
    let moved = json!({ "type": "move", "path": path, "step": step });
    let pressed = |down: bool| json!({ "type": "button", "path": path, "step": step, "button": button, "down": down });
    let scrolled =
        |delta: i32| json!({ "type": "scroll", "path": path, "step": step, "delta": delta });
    match action.action {
        InputActionKind::MouseDown => vec![moved, pressed(true)],
        InputActionKind::MouseUp => vec![moved, pressed(false)],
        InputActionKind::ScrollUp => vec![moved, scrolled(1)],
        InputActionKind::ScrollDown => vec![moved, scrolled(-1)],
        _ => vec![moved],
    }
}

/// The click that focuses a key or text action's own path before it: the
/// verified click for a GUI path, a click at its point for a world part.
fn focus_commands<'a>(
    action: &'a InputAction,
    step: usize,
    locate: &mut Locate,
) -> Result<Option<(Vec<Value>, Pointer<'a>)>> {
    let Some(path) = action.path.as_deref() else {
        return Ok(None);
    };
    if is_world_path(path) {
        let (x, y) = locate(action, None)?;
        let click = virtual_click_actions(x, y, false, 30, false);
        return Ok(Some((click, Pointer::At(x, y))));
    }
    let click = vec![gui_click_command(path, 30, step)];
    Ok(Some((click, Pointer::Element(action))))
}

/// The point of a pointer action: its own target, else where the pointer
/// was left.
fn pointer_target(
    action: &InputAction,
    pointer: &Pointer,
    locate: &mut Locate,
) -> Result<(i32, i32)> {
    let targeted = action.path.is_some() || action.x.is_some() || action.y.is_some();
    let previous = match pointer {
        Pointer::At(x, y) => Some((*x, *y)),
        Pointer::Element(clicked) if !targeted => Some(locate(clicked, None)?),
        _ => None,
    };
    locate(action, previous)
}

/// The plugin commands for a request, in the order of its actions. A click on
/// a GUI path, and the click that focuses a key or text action's GUI path, is
/// the plugin's verified click wherever it stands in the list; `locate` reads
/// the viewport point of every other target.
fn virtual_input_commands(actions: &[InputAction], locate: &mut Locate) -> Result<Vec<Value>> {
    let mut pointer = Pointer::Unset;
    let mut commands = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let step = index + 1;
        if let Some(click) = verified_click_command(action, step) {
            commands.push(click);
            pointer = Pointer::Element(action);
            continue;
        }
        match action.action {
            InputActionKind::KeyDown
            | InputActionKind::KeyUp
            | InputActionKind::KeyPress
            | InputActionKind::Text => {
                if let Some((click, left_at)) = focus_commands(action, step, locate)? {
                    commands.extend(click);
                    pointer = left_at;
                }
                if matches!(action.action, InputActionKind::Text) {
                    commands.push(json!({
                        "type": "text",
                        "text": action.text.as_deref().context("text action requires text")?,
                    }));
                    commands.push(json!({ "type": "wait", "ms": 0 }));
                } else {
                    commands.extend(key_commands(action)?);
                }
            }
            InputActionKind::Move
            | InputActionKind::MouseDown
            | InputActionKind::MouseUp
            | InputActionKind::Click
            | InputActionKind::ScrollUp
            | InputActionKind::ScrollDown => {
                if let Some(path) = deferred_pointer_path(action) {
                    commands.extend(path_pointer_commands(action, path, step));
                    pointer = Pointer::Element(action);
                } else {
                    let (x, y) = pointer_target(action, &pointer, locate)?;
                    commands.extend(pointer_commands(action, x, y));
                    pointer = Pointer::At(x, y);
                }
            }
            InputActionKind::Wait => match action.ms.unwrap_or(0) {
                0 => commands.push(json!({ "type": "wait", "ms": 0 })),
                ms => commands.extend(virtual_wait_actions(ms)),
            },
            InputActionKind::Dismiss | InputActionKind::Press => {
                commands.push(core_gui_command(action)?);
            }
        }
    }
    // Clicks and waits alone keep the strict check of each button's events.
    // Beside other actions a button that answers its press another way (a
    // prompt, a panel that replaces it) is counted as unconfirmed instead.
    let strict = actions.iter().all(|action| {
        matches!(
            action.action,
            InputActionKind::Click | InputActionKind::Wait
        )
    });
    if !strict {
        for command in &mut commands {
            if command["type"] == "click" {
                command["lenient"] = json!(true);
            }
        }
    }
    Ok(commands)
}

fn action_label(action: &InputAction) -> String {
    let name = match action.action {
        InputActionKind::Click => "click",
        InputActionKind::Text => "text",
        InputActionKind::KeyDown => "kd",
        InputActionKind::KeyUp => "ku",
        _ => "key",
    };
    format!("{name} {}", action.path.as_deref().unwrap_or_default())
}

fn action_range(first: usize, last: usize) -> String {
    if first == last {
        format!("action {first}")
    } else {
        format!("actions {first}-{last}")
    }
}

/// A refused click's message for a request of several actions: which action
/// stopped the list, which ones ran before it and which did not run.
fn stopped_chain_message(message: &str, step: usize, actions: &[InputAction]) -> String {
    let total = actions.len();
    let progress = match step {
        1 => "nothing ran".to_string(),
        _ if step == total => format!("{} ran", action_range(1, step - 1)),
        _ => format!(
            "{} ran, {} did not",
            action_range(1, step - 1),
            action_range(step + 1, total)
        ),
    };
    format!(
        "Action {step} of {total} ({}) failed; {progress}: {message}",
        action_label(&actions[step - 1])
    )
}

/// Names the action a client refused in a list of several; any other error,
/// and the refusal of a single action, passes through unchanged.
fn stopped_chain_error(error: anyhow::Error, actions: &[InputAction]) -> anyhow::Error {
    let stopped = error
        .downcast_ref::<VirtualInputFailure>()
        .and_then(|failure| Some((failure.step?, failure.message.as_str())))
        .filter(|(step, _)| actions.len() > 1 && (1..=actions.len()).contains(step))
        .map(|(step, message)| stopped_chain_message(message, step, actions));
    match stopped {
        Some(message) => anyhow!(message),
        None => error,
    }
}

fn action_position(
    action: &InputAction,
    bridge: &BridgeServer,
    player: Option<&str>,
    previous: Option<(i32, i32)>,
) -> Result<(i32, i32)> {
    if let Some(path) = action.path.as_deref() {
        let world = is_world_path(path);
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
    let request: InputRequest = serde_json::from_value(parameters.clone())?;
    if request.actions.is_empty() || request.actions.len() > 256 {
        bail!("input requires 1 through 256 actions");
    }
    if let Some(player) = request.player.as_deref() {
        wait_for_player_bridge(bridge, player, 8.0)?;
    }
    #[cfg(windows)]
    if os_input_preferred() && request.actions.iter().any(needs_window) {
        return os_input_result(&request, bridge)
            .map_err(|error| stopped_chain_error(error, &request.actions));
    }
    virtual_input_result(&request, bridge)
        .map_err(|error| stopped_chain_error(error, &request.actions))
}

/// Verified GUI clicks and waits run in the client and need no Studio window.
#[cfg(windows)]
fn needs_window(action: &InputAction) -> bool {
    !matches!(action.action, InputActionKind::Wait) && verified_click_command(action, 0).is_none()
}

#[cfg(windows)]
fn os_input_result(request: &InputRequest, bridge: &BridgeServer) -> Result<Value> {
    let player = request.player.as_deref();
    let (window, offset_x, offset_y) =
        resolve_player_window(bridge, player, client_viewport_size(bridge, player))?;
    let _shield = input_inject::input_shield(&window)?;
    let mut position = None;
    let mut calibration = None;
    let mut used_os = false;
    let mut used_virtual = false;
    let mut core_gui = None;

    for (index, action) in request.actions.iter().enumerate() {
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
                if let Some(click) = verified_click_command(action, index + 1) {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    send_virtual_input(bridge, player, vec![click], None)?;
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

/// The plugin build from which a GUI path in an input list is resolved,
/// checked and clicked by the client when its turn comes. An older plugin
/// reads such an action as the point 0,0 or as a button it does not find.
const GUI_PATH_INPUT_BUILD_UNIX: i64 = 1_794_366_810;

fn names_gui_path(actions: &[InputAction]) -> bool {
    actions.iter().any(|action| {
        action
            .path
            .as_deref()
            .is_some_and(|path| !is_world_path(path))
    })
}

fn predates_gui_path_input(clients: &[Value]) -> bool {
    clients
        .iter()
        .filter_map(|client| client.get("bridgeBuildUnix").and_then(Value::as_i64))
        .any(|build| build < GUI_PATH_INPUT_BUILD_UNIX)
}

fn virtual_input_result(request: &InputRequest, bridge: &BridgeServer) -> Result<Value> {
    if names_gui_path(&request.actions) && predates_gui_path_input(&bridge.list_bridge_clients()) {
        bail!(
            "A connected Studio runs an older Renium plugin than input on GUI paths needs; restart Studio to load the installed one"
        );
    }
    let player = request.player.as_deref();
    let commands = virtual_input_commands(&request.actions, &mut |action, previous| {
        action_position(action, bridge, player, previous)
    })?;
    let response = send_virtual_input(bridge, player, commands, None)?;
    let mut result = json!({
        "ok": true,
        "action": "input",
        "actions": request.actions.len(),
        "inputMethod": "virtual",
    });
    note_system_ui(&mut result, &response);
    for key in [
        "heldKeys",
        "keysObserved",
        "verifiedClicks",
        "unconfirmedClicks",
        "coreGui",
    ] {
        if let Some(value) = response.get(key).filter(|value| !value.is_null()) {
            result[key] = value.clone();
        }
    }
    if response.get("textObserved") == Some(&Value::Bool(false)) {
        result["textObserved"] = json!(false);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(value: Value) -> InputAction {
        serde_json::from_value(value).unwrap()
    }

    fn request(actions: Value) -> InputRequest {
        serde_json::from_value(json!({ "player": "1", "actions": actions })).unwrap()
    }

    /// Plans `actions` with every path at 100,200 and returns the commands
    /// and the paths whose point was read.
    fn plan(actions: Value) -> (Vec<Value>, Vec<String>) {
        let request = request(actions);
        let mut located = Vec::new();
        let commands = virtual_input_commands(&request.actions, &mut |action, previous| {
            if let Some(path) = action.path.as_deref() {
                located.push(path.to_string());
                return Ok((100, 200));
            }
            match (action.x, action.y) {
                (Some(x), Some(y)) => Ok((x, y)),
                _ => previous.context("no earlier position"),
            }
        })
        .unwrap();
        (commands, located)
    }

    fn types(commands: &[Value]) -> Vec<&str> {
        commands
            .iter()
            .map(|command| command["type"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn gui_clicks_are_verified_wherever_they_stand_in_a_chain() {
        let (commands, located) = plan(json!([
            { "action": "click", "path": "Main.Shop.Codes" },
            { "action": "text", "text": "ABC" },
            { "action": "key-press", "key": "Return" },
            { "action": "wait", "ms": 300 },
            { "action": "scroll-down", "path": "Main.Shop.Slots" },
            { "action": "click", "x": 450, "y": 323 },
            { "action": "click", "path": "Main.Shop.Redeem", "ms": 80 },
        ]));
        assert_eq!(
            types(&commands),
            [
                "click", "text", "wait", "key", "wait", "key", "wait", "move", "scroll", "move",
                "wait", "button", "wait", "button", "wait", "click"
            ]
        );
        assert_eq!(
            commands[0],
            json!({ "type": "click", "path": "Main.Shop.Codes", "holdMs": 30, "step": 1, "lenient": true })
        );
        assert_eq!(commands[1]["text"], "ABC");
        assert_eq!(
            commands[3],
            json!({ "type": "key", "key": "Return", "down": true })
        );
        assert_eq!(commands[6]["ms"], 300);
        assert_eq!(
            commands[7],
            json!({ "type": "move", "path": "Main.Shop.Slots", "step": 5 })
        );
        assert_eq!(
            commands[8],
            json!({ "type": "scroll", "path": "Main.Shop.Slots", "step": 5, "delta": -1 })
        );
        assert_eq!(commands[11]["x"], 450);
        assert_eq!(
            commands[15],
            json!({ "type": "click", "path": "Main.Shop.Redeem", "holdMs": 80, "step": 7, "lenient": true })
        );
        assert!(
            located.is_empty(),
            "no GUI path is resolved ahead of its turn: {located:?}"
        );
    }

    #[test]
    fn gui_paths_wait_for_a_plugin_that_resolves_them_at_their_turn() {
        let request = |actions: Value| request(actions).actions;
        assert!(names_gui_path(&request(
            json!([{ "action": "scroll-down", "path": "Main.Shop.Slots" }])
        )));
        assert!(!names_gui_path(&request(json!([
            { "action": "click", "path": "Workspace.Door" },
            { "action": "click", "x": 4, "y": 5 },
            { "action": "key-press", "key": "E" },
        ]))));
        let older =
            json!({ "role": "play-client", "bridgeBuildUnix": GUI_PATH_INPUT_BUILD_UNIX - 1 });
        let current = json!({ "role": "edit", "bridgeBuildUnix": GUI_PATH_INPUT_BUILD_UNIX });
        assert!(predates_gui_path_input(&[current.clone(), older]));
        assert!(!predates_gui_path_input(&[
            current,
            json!({ "role": "edit" })
        ]));
    }

    #[test]
    fn world_parts_right_clicks_and_points_stay_pointer_input() {
        for path in [
            "Workspace.Props.Crate[1]",
            "game.Workspace.Door",
            "Workspace",
        ] {
            assert!(is_world_path(path));
            let (commands, located) = plan(json!([{ "action": "click", "path": path }]));
            assert_eq!(
                types(&commands),
                ["move", "wait", "button", "wait", "button", "wait"]
            );
            assert_eq!(located, [path]);
        }
        assert!(!is_world_path("WorkspaceHud.Button"));
        let (right, located) =
            plan(json!([{ "action": "click", "path": "Shop.BuyButton", "button": "right" }]));
        assert_eq!(right[2]["button"], "right");
        assert_eq!(located, ["Shop.BuyButton"]);
        let point = action(json!({ "action": "click", "x": 4, "y": 5 }));
        assert!(verified_click_command(&point, 1).is_none());
    }

    #[test]
    fn clicks_and_waits_alone_keep_the_strict_button_check() {
        let (commands, _) = plan(json!([
            { "action": "click", "path": "Main.Open" },
            { "action": "wait", "ms": 0 },
            { "action": "click", "path": "Main.Close" },
        ]));
        assert!(
            commands
                .iter()
                .all(|command| command.get("lenient").is_none())
        );
    }

    #[test]
    fn a_key_or_text_path_is_focused_with_a_verified_click() {
        let (typed, located) = plan(json!([
            { "action": "wait", "ms": 0 },
            { "action": "text", "path": "Chat.Box", "text": "hi" },
            { "action": "key-down", "path": "Workspace.Lever", "key": "E" },
        ]));
        assert_eq!(
            typed[1],
            json!({ "type": "click", "path": "Chat.Box", "holdMs": 30, "step": 2, "lenient": true })
        );
        assert_eq!(
            types(&typed),
            [
                "wait", "click", "text", "wait", "button", "wait", "button", "wait", "key"
            ]
        );
        assert_eq!(located, ["Workspace.Lever"]);
    }

    #[test]
    fn an_action_without_a_target_uses_where_the_pointer_was_left() {
        let (after_click, located) = plan(json!([
            { "action": "click", "path": "Shop.Open" },
            { "action": "mouse-down" },
            { "action": "mouse-up" },
        ]));
        assert_eq!(
            types(&after_click),
            ["click", "move", "button", "move", "button"]
        );
        assert_eq!(
            after_click[1],
            json!({ "type": "move", "x": 100, "y": 200 })
        );
        assert_eq!(
            located,
            ["Shop.Open"],
            "read once, when the first action asks"
        );
        let (after_point, _) = plan(json!([
            { "action": "move", "x": 7, "y": 9 },
            { "action": "scroll-up" },
        ]));
        assert_eq!(
            after_point[2],
            json!({ "type": "scroll", "x": 7, "y": 9, "delta": 1 })
        );
        let nowhere = request(json!([{ "action": "mouse-down" }]));
        assert!(
            virtual_input_commands(&nowhere.actions, &mut |_, previous| previous
                .context("no earlier position"))
            .is_err()
        );
    }

    #[test]
    fn long_waits_and_holds_split_into_plugin_sized_waits() {
        let (commands, _) = plan(
            json!([{ "action": "click", "path": "Shop.BuyButton" }, { "action": "wait", "ms": 15000 }]),
        );
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[1]["ms"], 10000);
        assert_eq!(commands[2]["ms"], 5000);
        let (zero, _) = plan(json!([{ "action": "wait", "ms": 0 }]));
        assert_eq!(zero, vec![json!({ "type": "wait", "ms": 0 })]);
        let held = virtual_wait_actions(65_000);
        assert_eq!(held.len(), 6);
        assert!(held.iter().all(|wait| wait["ms"] == 10000));
    }

    #[test]
    fn a_refused_click_names_its_action_and_what_ran() {
        let reason = "GUI element Main.Shop.Redeem is not visible: Main.Shop has Visible=false. \
                      Clickable now: Main.DailyRewards.Header.Exit";
        let refused = |step: Option<usize>| {
            anyhow::Error::from(VirtualInputFailure {
                message: reason.to_string(),
                step,
            })
        };
        let chain = request(json!([
            { "action": "click", "path": "Main.Shop.Codes" },
            { "action": "text", "text": "ABC" },
            { "action": "click", "path": "Main.Shop.Redeem" },
            { "action": "wait", "ms": 300 },
            { "action": "text", "path": "Main.Shop.Note", "text": "x" },
        ]));
        let message =
            |step: usize| stopped_chain_error(refused(Some(step)), &chain.actions).to_string();
        assert_eq!(
            message(3),
            format!(
                "Action 3 of 5 (click Main.Shop.Redeem) failed; actions 1-2 ran, actions 4-5 \
                 did not: {reason}"
            )
        );
        assert_eq!(
            message(1),
            format!("Action 1 of 5 (click Main.Shop.Codes) failed; nothing ran: {reason}")
        );
        assert_eq!(
            message(5),
            format!("Action 5 of 5 (text Main.Shop.Note) failed; actions 1-4 ran: {reason}")
        );
        let pair = request(json!([
            { "action": "wait", "ms": 100 },
            { "action": "click", "path": "Shop.Buy" },
        ]));
        assert_eq!(
            stopped_chain_error(refused(Some(2)), &pair.actions).to_string(),
            format!("Action 2 of 2 (click Shop.Buy) failed; action 1 ran: {reason}")
        );
        let single = request(json!([{ "action": "click", "path": "Main.Shop.Redeem" }]));
        assert_eq!(
            stopped_chain_error(refused(Some(1)), &single.actions).to_string(),
            reason
        );
        for step in [None, Some(0), Some(6)] {
            assert_eq!(
                stopped_chain_error(refused(step), &chain.actions).to_string(),
                reason
            );
        }
        assert_eq!(
            stopped_chain_error(anyhow!("Studio is busy"), &chain.actions).to_string(),
            "Studio is busy"
        );
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
