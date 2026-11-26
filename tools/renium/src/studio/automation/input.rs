#[cfg(windows)]
use std::collections::BTreeMap;
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::{Mutex, PoisonError};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(windows)]
use super::{
    MAX_KEY_HOLD_MS, call_execute_luau, client_viewport_size, ensure_luau_ok_in_context,
    input_delta, merge_virtual_input_results, os_input_preferred, resolve_player_window,
};
use super::{
    VirtualInputFailure, ensure_plugin_api_ok, note_system_ui, send_virtual_input,
    virtual_click_actions, virtual_wait_actions, wait_for_player_bridge,
};
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::studio::input as input_inject;
#[cfg(windows)]
use crate::studio::native::serializer::{
    GamepadAction, GamepadInitialState, GamepadSessionReady, GamepadStateReport, VIRTUAL_DEVICE_ID,
    connect_gamepad_session, execute_gamepad_actions, has_live_gamepad_session,
    prune_gamepad_sessions, release_gamepad_inputs, set_gamepad_connected, set_gamepad_slot,
};

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

#[derive(Clone, Deserialize)]
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
    to_path: Option<String>,
    to_x: Option<i32>,
    to_y: Option<i32>,
    #[serde(alias = "mouse_button")]
    button: Option<MouseButton>,
    #[serde(alias = "wait_time_ms", alias = "hold_ms")]
    ms: Option<u64>,
    stick: Option<String>,
    trigger: Option<String>,
    axis_x: Option<f32>,
    axis_y: Option<f32>,
    value: Option<f32>,
}

#[derive(Clone, Copy, Deserialize)]
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
    Drag,
    #[serde(alias = "scrollUp")]
    ScrollUp,
    #[serde(alias = "scrollDown")]
    ScrollDown,
    Wait,
    Dismiss,
    Press,
    Stick,
    Trigger,
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

/// The end of a drag as an action of its own, so both ends are located the
/// same way.
fn drag_end(action: &InputAction) -> InputAction {
    InputAction {
        action: InputActionKind::Move,
        key: None,
        text: None,
        path: action.to_path.clone(),
        x: action.to_x,
        y: action.to_y,
        to_path: None,
        to_x: None,
        to_y: None,
        button: action.button,
        ms: None,
        stick: None,
        trigger: None,
        axis_x: None,
        axis_y: None,
        value: None,
    }
}

const DEFAULT_DRAG_MS: u64 = 300;
const MAX_DRAG_MS: u64 = 10_000;

/// The plugin's drag: press at the start, a move every frame for `ms`, and
/// the release at the end. The client reads a GUI path when the drag's turn
/// comes; `locate` reads a world part or a point now.
fn drag_command(action: &InputAction, step: usize, locate: &mut Locate) -> Result<Value> {
    let right = matches!(action.button, Some(MouseButton::Right));
    let ms = action.ms.unwrap_or(DEFAULT_DRAG_MS);
    if ms > MAX_DRAG_MS {
        bail!("drag takes up to 10 s; hold the button with down, wait and up for longer");
    }
    let mut command = json!({
        "type": "drag",
        "step": step,
        "ms": ms,
        "button": if right { "right" } else { "left" },
    });
    let end = drag_end(action);
    for (target, path_key, x_key, y_key) in
        [(action, "path", "x", "y"), (&end, "toPath", "toX", "toY")]
    {
        match target.path.as_deref() {
            Some(path) if !is_world_path(path) && target.x.is_none() && target.y.is_none() => {
                command[path_key] = json!(path);
            }
            _ => {
                let (x, y) = locate(target, None)?;
                command[x_key] = json!(x);
                command[y_key] = json!(y);
            }
        }
    }
    Ok(command)
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
fn virtual_input_commands(
    actions: &[InputAction],
    step_offset: usize,
    allow_strict: bool,
    locate: &mut Locate,
) -> Result<Vec<Value>> {
    let mut pointer = Pointer::Unset;
    let mut commands = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        let step = step_offset + index + 1;
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
            InputActionKind::Drag => {
                let command = drag_command(action, step, locate)?;
                pointer = match (command["toX"].as_i64(), command["toY"].as_i64()) {
                    (Some(x), Some(y)) => Pointer::At(x as i32, y as i32),
                    _ => Pointer::Unset,
                };
                commands.push(command);
            }
            InputActionKind::Wait => match action.ms.unwrap_or(0) {
                0 => commands.push(json!({ "type": "wait", "ms": 0 })),
                ms => commands.extend(virtual_wait_actions(ms)),
            },
            InputActionKind::Dismiss | InputActionKind::Press => {
                commands.push(core_gui_command(action)?);
            }
            InputActionKind::Stick | InputActionKind::Trigger => {
                bail!("Gamepad axis actions must run on the virtual gamepad path");
            }
        }
    }
    // Clicks and waits alone keep the strict check of each button's events.
    // Beside other actions a button that answers its press another way (a
    // prompt, a panel that replaces it) is counted as unconfirmed instead.
    let strict = allow_strict
        && actions.iter().all(|action| {
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
        InputActionKind::Drag => "drag",
        InputActionKind::Move => "move",
        InputActionKind::MouseDown => "down",
        InputActionKind::MouseUp => "up",
        InputActionKind::Text => "text",
        InputActionKind::KeyDown => "kd",
        InputActionKind::KeyUp => "ku",
        InputActionKind::Wait => "wait",
        InputActionKind::Stick => "stick",
        InputActionKind::Trigger => "trigger",
        InputActionKind::Dismiss => "dismiss",
        InputActionKind::Press => "press",
        _ => "key",
    };
    let target = action
        .path
        .as_deref()
        .or(action.key.as_deref())
        .or(action.stick.as_deref())
        .or(action.trigger.as_deref())
        .unwrap_or_default();
    let label = if target.is_empty() {
        name.to_string()
    } else {
        format!("{name} {target}")
    };
    match action.to_path.as_deref() {
        Some(end) => format!("{label} to {end}"),
        None => label,
    }
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

fn is_gamepad_action(action: &InputAction) -> bool {
    match action.action {
        InputActionKind::Stick | InputActionKind::Trigger => true,
        InputActionKind::KeyDown | InputActionKind::KeyUp | InputActionKind::KeyPress => action
            .key
            .as_deref()
            .and_then(input_inject::resolve_gamepad_button)
            .is_some(),
        _ => false,
    }
}

fn resolve_gamepad_action(action: &InputAction) -> Result<input_inject::GamepadButtonSpec> {
    match action.action {
        InputActionKind::KeyDown | InputActionKind::KeyUp | InputActionKind::KeyPress => action
            .key
            .as_deref()
            .and_then(input_inject::resolve_gamepad_button)
            .context("Gamepad action requires a gamepad button key"),
        InputActionKind::Stick => {
            let stick = input_inject::resolve_gamepad_stick(
                action
                    .stick
                    .as_deref()
                    .context("stick action requires stick (L or R)")?,
            )?;
            let x = action.axis_x.context("stick action requires axisX")?;
            let y = action.axis_y.context("stick action requires axisY")?;
            if !x.is_finite()
                || !y.is_finite()
                || !(-1.0..=1.0).contains(&x)
                || !(-1.0..=1.0).contains(&y)
            {
                bail!("stick coordinates must be finite numbers in -1..=1");
            }
            Ok(stick)
        }
        InputActionKind::Trigger => {
            let trigger = input_inject::resolve_gamepad_trigger(
                action
                    .trigger
                    .as_deref()
                    .context("trigger action requires trigger (L2 or R2)")?,
            )?;
            let value = action.value.context("trigger action requires value")?;
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                bail!("trigger value must be a finite number in 0..=1");
            }
            Ok(trigger)
        }
        _ => bail!("Not a gamepad action"),
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq)]
enum ParsedGamepadStep {
    ButtonDown(&'static str, i32),
    ButtonUp(&'static str, i32),
    ButtonPress(&'static str, i32, u64),
    Stick(&'static str, i32, f32, f32),
    Trigger(&'static str, i32, f32),
}

#[cfg(windows)]
fn parse_gamepad_step(action: &InputAction) -> Result<ParsedGamepadStep> {
    let spec = resolve_gamepad_action(action)?;
    match action.action {
        InputActionKind::KeyDown => Ok(ParsedGamepadStep::ButtonDown(spec.name, spec.key_code)),
        InputActionKind::KeyUp => Ok(ParsedGamepadStep::ButtonUp(spec.name, spec.key_code)),
        InputActionKind::KeyPress => Ok(ParsedGamepadStep::ButtonPress(
            spec.name,
            spec.key_code,
            action.ms.unwrap_or(60).min(MAX_KEY_HOLD_MS),
        )),
        InputActionKind::Stick => Ok(ParsedGamepadStep::Stick(
            spec.name,
            spec.key_code,
            action.axis_x.context("stick action requires axisX")?,
            action.axis_y.context("stick action requires axisY")?,
        )),
        InputActionKind::Trigger => Ok(ParsedGamepadStep::Trigger(
            spec.name,
            spec.key_code,
            action.value.context("trigger action requires value")?,
        )),
        _ => bail!("Not a gamepad action"),
    }
}

#[cfg(windows)]
static GAMEPAD_NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);
#[cfg(windows)]
static GAMEPAD_CONNECT: Mutex<()> = Mutex::new(());

#[cfg(windows)]
fn next_gamepad_nonce() -> String {
    let seq = GAMEPAD_NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__ReniumGamepadNonce_{}_{seq}", std::process::id())
}

#[cfg(windows)]
fn gamepad_slot_attribute() -> String {
    format!("__ReniumGamepadSlot_{VIRTUAL_DEVICE_ID}")
}

#[cfg(windows)]
fn gamepad_init_luau(nonce: &str, slot_attr: &str) -> String {
    format!(
        "local vim = game:GetService('VirtualInputManager') \
         local p = game:GetService('Players').LocalPlayer \
         if not p then return nil end \
         local old = vim:FindFirstChild('{nonce}') if old then old:Destroy() end \
         local marker = Instance.new('Folder') marker.Name = '{nonce}' marker.Archivable = false marker.Parent = vim \
         local uis = game:GetService('UserInputService') \
         local before = {{}} local beforeSet = {{}} \
         for _, pad in ipairs(uis:GetConnectedGamepads()) do \
           table.insert(before, pad.Name) beforeSet[pad.Name] = true \
         end \
         local saved = vim:GetAttribute('{slot_attr}') \
         local slot = if type(saved) == 'string' and beforeSet[saved] then saved else nil \
         local buttons, sticks, triggers = {{}}, {{}}, {{}} \
         if slot then \
           local pad = Enum.UserInputType[slot] \
           if pad then \
             for _, obj in ipairs(uis:GetGamepadState(pad)) do \
               local n = obj.KeyCode.Name \
               if n == 'Thumbstick1' or n == 'Thumbstick2' then \
                 if obj.Position.X ~= 0 or obj.Position.Y ~= 0 then \
                   sticks[n] = {{ obj.Position.X, obj.Position.Y }} \
                 end \
               elseif (n == 'ButtonL2' or n == 'ButtonR2') and obj.Position.Z ~= 0 then \
                 triggers[n] = obj.Position.Z \
               elseif uis:IsGamepadButtonDown(pad, obj.KeyCode) then \
                 table.insert(buttons, n) \
               end \
             end \
           end \
         end \
         return {{ player = p.Name, connectedBefore = before, savedSlot = slot, buttons = buttons, sticks = sticks, triggers = triggers }}"
    )
}

#[cfg(windows)]
struct GamepadInitSnapshot {
    initial: GamepadInitialState,
    connected_before: Vec<String>,
}

#[cfg(windows)]
fn parse_gamepad_initial_state(value: &Value, nonce: String) -> Result<GamepadInitSnapshot> {
    let player_name = value
        .get("player")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .context("Play client has no LocalPlayer yet; retry once the client has loaded")?
        .to_string();
    let connected_before = value
        .get("connectedBefore")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let existing_slot = value
        .get("savedSlot")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let mut held_buttons = Vec::new();
    let mut sticks = Vec::new();
    let mut triggers = Vec::new();
    if existing_slot.is_some() {
        parse_saved_gamepad_controls(value, &mut held_buttons, &mut sticks, &mut triggers);
    }
    Ok(GamepadInitSnapshot {
        initial: GamepadInitialState {
            player_name,
            nonce,
            existing_slot,
            held_buttons,
            sticks,
            triggers,
        },
        connected_before,
    })
}

#[cfg(windows)]
fn parse_saved_gamepad_controls(
    value: &Value,
    held_buttons: &mut Vec<(&'static str, i32)>,
    sticks: &mut Vec<(&'static str, i32, f32, f32)>,
    triggers: &mut Vec<(&'static str, i32, f32)>,
) {
    if let Some(buttons) = value.get("buttons").and_then(Value::as_array) {
        for item in buttons {
            if let Some(spec) = item.as_str().and_then(input_inject::resolve_gamepad_button) {
                held_buttons.push((spec.name, spec.key_code));
            }
        }
    }
    if let Some(map) = value.get("sticks").and_then(Value::as_object) {
        for (name, coords) in map {
            if let Ok(spec) = input_inject::resolve_gamepad_stick(name)
                && let Some(values) = coords.as_array()
                && values.len() >= 2
                && let (Some(x), Some(y)) = (values[0].as_f64(), values[1].as_f64())
            {
                sticks.push((spec.name, spec.key_code, x as f32, y as f32));
            }
        }
    }
    if let Some(map) = value.get("triggers").and_then(Value::as_object) {
        for (name, amount) in map {
            if let Ok(spec) = input_inject::resolve_gamepad_trigger(name)
                && let Some(val) = amount.as_f64()
            {
                triggers.push((spec.name, spec.key_code, val as f32));
            }
        }
    }
}

#[cfg(windows)]
fn fetch_gamepad_init(
    bridge: &BridgeServer,
    player: Option<&str>,
    nonce: &str,
    slot_attr: &str,
) -> Result<GamepadInitSnapshot> {
    let code = gamepad_init_luau(nonce, slot_attr);
    let init = call_execute_luau(
        bridge,
        BridgeTarget::Client,
        player,
        &code,
        "ReniumGamepadInit",
        10.0,
        5.0,
    )?;
    ensure_luau_ok_in_context(&init, player, None)?;
    let snapshot = init
        .get("results")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .filter(|value| !value.is_null())
        .context("Play client has no LocalPlayer yet; retry once the client has loaded")?;
    parse_gamepad_initial_state(snapshot, nonce.to_string())
}

#[cfg(windows)]
fn remove_gamepad_nonce(bridge: &BridgeServer, player: Option<&str>, nonce: &str) {
    let code = format!(
        "local m = game:GetService('VirtualInputManager'):FindFirstChild('{nonce}') \
         if m then m:Destroy() end"
    );
    let _ = call_execute_luau(
        bridge,
        BridgeTarget::Client,
        player,
        &code,
        "ReniumGamepadNonceCleanup",
        5.0,
        2.0,
    );
}

#[cfg(windows)]
const GAMEPAD_SLOT_WAIT_SECONDS: f64 = 1.0;

#[cfg(windows)]
fn detect_connected_gamepad_slot(
    bridge: &BridgeServer,
    player: Option<&str>,
    nonce: &str,
    slot_attr: &str,
    connected_before: &[String],
) -> Result<Option<String>> {
    let before_entries: String = connected_before
        .iter()
        .filter(|name| name.chars().all(|ch| ch.is_ascii_alphanumeric()))
        .map(|name| format!("['{name}']=true"))
        .collect::<Vec<_>>()
        .join(",");
    let code = format!(
        "local vim = game:GetService('VirtualInputManager') \
         local m = vim:FindFirstChild('{nonce}') if m then m:Destroy() end \
         local uis = game:GetService('UserInputService') \
         local rs = game:GetService('RunService') \
         local before = {{ {before_entries} }} \
         local deadline = os.clock() + {GAMEPAD_SLOT_WAIT_SECONDS} \
         local found = nil \
         repeat \
           for _, pad in ipairs(uis:GetConnectedGamepads()) do \
             if not before[pad.Name] then found = pad.Name break end \
           end \
           if found then break end \
           rs.Heartbeat:Wait() \
         until os.clock() >= deadline \
         if found then vim:SetAttribute('{slot_attr}', found) end \
         return found"
    );
    let response = call_execute_luau(
        bridge,
        BridgeTarget::Client,
        player,
        &code,
        "ReniumGamepadSlot",
        10.0,
        5.0,
    )?;
    ensure_luau_ok_in_context(&response, player, None)?;
    Ok(response
        .get("results")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string))
}

#[cfg(windows)]
fn wait_connected_gamepads_after_disconnect(
    bridge: &BridgeServer,
    player: Option<&str>,
    before: &[String],
) -> Result<Vec<String>> {
    let expected_count = before.len();
    let code = format!(
        "local uis = game:GetService('UserInputService') \
         local rs = game:GetService('RunService') \
         local deadline = os.clock() + {GAMEPAD_SLOT_WAIT_SECONDS} \
         local list = {{}} \
         repeat \
           list = {{}} \
           for _, pad in ipairs(uis:GetConnectedGamepads()) do \
             table.insert(list, pad.Name) \
           end \
           if #list < {expected_count} then break end \
           rs.Heartbeat:Wait() \
         until os.clock() >= deadline \
         return list"
    );
    let response = call_execute_luau(
        bridge,
        BridgeTarget::Client,
        player,
        &code,
        "ReniumGamepadDisconnectWait",
        10.0,
        5.0,
    )?;
    ensure_luau_ok_in_context(&response, player, None)?;
    Ok(response
        .get("results")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(windows)]
fn ensure_gamepad_ready(
    bridge: &BridgeServer,
    player: Option<&str>,
    pid: u32,
    runtime_id: &str,
) -> Result<String> {
    let _connecting = GAMEPAD_CONNECT
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(slot) = has_live_gamepad_session(pid, runtime_id) {
        return Ok(slot);
    }
    let nonce = next_gamepad_nonce();
    let slot_attr = gamepad_slot_attribute();
    let snapshot = fetch_gamepad_init(bridge, player, &nonce, &slot_attr)?;
    match connect_gamepad_session(pid, runtime_id, &snapshot.initial) {
        Ok(GamepadSessionReady::Connected(slot)) => {
            remove_gamepad_nonce(bridge, player, &nonce);
            Ok(slot)
        }
        Ok(GamepadSessionReady::NeedsSlotDetection) => {
            let slot = match detect_connected_gamepad_slot(
                bridge,
                player,
                &nonce,
                &slot_attr,
                &snapshot.connected_before,
            )? {
                Some(slot) => slot,
                None if !snapshot.connected_before.is_empty() => {
                    // Device id was already connected from a prior Play session that stopped
                    // outside `rbx play -x`; cycle it once so the slot is uniquely identified.
                    set_gamepad_connected(pid, runtime_id, false)?;
                    let after_disconnect = wait_connected_gamepads_after_disconnect(
                        bridge,
                        player,
                        &snapshot.connected_before,
                    )?;
                    set_gamepad_connected(pid, runtime_id, true)?;
                    detect_connected_gamepad_slot(
                        bridge,
                        player,
                        &nonce,
                        &slot_attr,
                        &after_disconnect,
                    )?
                    .context(
                        "Virtual gamepad reconnected but no free gamepad slot appeared on the Play client",
                    )?
                }
                None => bail!(
                    "Virtual gamepad connected but no free gamepad slot appeared on the Play client"
                ),
            };
            set_gamepad_slot(pid, runtime_id, slot.clone());
            Ok(slot)
        }
        Err(error) => {
            remove_gamepad_nonce(bridge, player, &nonce);
            Err(error)
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum ExpectedGamepadInput {
    Button(bool),
    Stick(f32, f32),
    Trigger(f32),
}

#[cfg(windows)]
fn build_gamepad_observe_luau(slot: &str, batch: &[(usize, GamepadAction)]) -> Option<String> {
    let mut expected: BTreeMap<&'static str, ExpectedGamepadInput> = BTreeMap::new();
    for &(_, action) in batch {
        match action {
            GamepadAction::Button { name, pressed, .. } => {
                expected.insert(name, ExpectedGamepadInput::Button(pressed));
            }
            GamepadAction::Stick { name, x, y, .. } => {
                expected.insert(name, ExpectedGamepadInput::Stick(x, y));
            }
            GamepadAction::Trigger { name, value, .. } => {
                expected.insert(name, ExpectedGamepadInput::Trigger(value));
            }
        }
    }
    if expected.is_empty() {
        return None;
    }
    let mut checks = Vec::with_capacity(expected.len());
    for (name, target) in expected {
        let entry = match target {
            ExpectedGamepadInput::Button(pressed) => {
                format!("{{ k = '{name}', t = 'b', v = {pressed} }}")
            }
            ExpectedGamepadInput::Stick(x, y) => {
                format!("{{ k = '{name}', t = 's', x = {x:?}, y = {y:?} }}")
            }
            ExpectedGamepadInput::Trigger(value) => {
                format!("{{ k = '{name}', t = 't', z = {value:?} }}")
            }
        };
        checks.push(entry);
    }
    Some(format!(
        "local uis = game:GetService('UserInputService') \
         local rs = game:GetService('RunService') \
         local pad = Enum.UserInputType['{slot}'] \
         local checks = {{ {} }} \
         local deadline = os.clock() + 0.5 \
         local observed = {{}} \
         repeat \
           rs.Heartbeat:Wait() \
           local stateMap = {{}} \
           for _, obj in ipairs(uis:GetGamepadState(pad)) do stateMap[obj.KeyCode.Name] = obj.Position end \
           local allOk = true \
           for _, c in ipairs(checks) do \
             local ok = false \
             if c.t == 'b' then \
               ok = (uis:IsGamepadButtonDown(pad, Enum.KeyCode[c.k]) == c.v) \
             elseif c.t == 's' then \
               local p = stateMap[c.k] \
               ok = p ~= nil and math.abs(p.X - c.x) <= 0.002 and math.abs(p.Y - c.y) <= 0.002 \
             else \
               local p = stateMap[c.k] \
               ok = p ~= nil and math.abs(p.Z - c.z) <= 0.002 \
             end \
             observed[c.k] = ok \
             if not ok then allOk = false end \
           end \
           if allOk then break end \
         until os.clock() >= deadline \
         return observed",
        checks.join(", ")
    ))
}

#[cfg(windows)]
fn observe_gamepad_batch(
    bridge: &BridgeServer,
    player: Option<&str>,
    slot: &str,
    batch: &[(usize, GamepadAction)],
    observed: &mut serde_json::Map<String, Value>,
) -> Result<()> {
    let Some(code) = build_gamepad_observe_luau(slot, batch) else {
        return Ok(());
    };
    let response = call_execute_luau(
        bridge,
        BridgeTarget::Client,
        player,
        &code,
        "ReniumGamepadObserve",
        10.0,
        5.0,
    )?;
    ensure_luau_ok_in_context(&response, player, None)?;
    if let Some(map) = response
        .get("results")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_object)
    {
        for (key, value) in map {
            observed.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn merge_segment_result(previous: Option<Value>, next: Value) -> Value {
    let Some(prev) = previous else {
        return next;
    };
    let prev_window = prev.get("window").cloned();
    let prev_held_keys = prev.get("heldKeys").cloned();
    let prev_held_buttons = prev.get("heldButtons").cloned();
    let mut merged = merge_virtual_input_results(prev, next);
    if merged.get("window").is_none_or(Value::is_null)
        && let Some(window) = prev_window.filter(|value| !value.is_null())
    {
        merged["window"] = window;
    }
    if merged.get("heldKeys").is_none_or(Value::is_null)
        && let Some(keys) = prev_held_keys.filter(|value| !value.is_null())
    {
        merged["heldKeys"] = keys;
    }
    if merged.get("heldButtons").is_none_or(Value::is_null)
        && let Some(buttons) = prev_held_buttons.filter(|value| !value.is_null())
    {
        merged["heldButtons"] = buttons;
    }
    merged
}

#[cfg(windows)]
fn apply_gamepad_state(
    result: &mut Value,
    slot: &str,
    state: &GamepadStateReport,
    observed: &serde_json::Map<String, Value>,
) {
    let reported_slot = if state.slot.is_empty() {
        slot
    } else {
        state.slot.as_str()
    };
    if !reported_slot.is_empty() {
        result["gamepad"] = json!(reported_slot);
    }
    if !observed.is_empty() {
        result["gamepadObserved"] = Value::Object(observed.clone());
    }
    if !state.held_buttons.is_empty() {
        result["heldGamepadButtons"] = json!(state.held_buttons);
    }
    if !state.sticks.is_empty() {
        let sticks: BTreeMap<_, _> = state
            .sticks
            .iter()
            .map(|(&name, &[x, y])| (name, [reported_axis(x), reported_axis(y)]))
            .collect();
        result["gamepadSticks"] = json!(sticks);
    }
    if !state.triggers.is_empty() {
        let triggers: BTreeMap<_, _> = state
            .triggers
            .iter()
            .map(|(&name, &value)| (name, reported_axis(value)))
            .collect();
        result["gamepadTriggers"] = json!(triggers);
    }
}

#[cfg(windows)]
fn reported_axis(value: f32) -> f64 {
    (f64::from(value) * 1000.0).round() / 1000.0
}

#[cfg(windows)]
fn client_has_held_virtual_gamepad(bridge: &BridgeServer, player: Option<&str>) -> bool {
    let slot_attr = gamepad_slot_attribute();
    let code = format!(
        "local vim = game:GetService('VirtualInputManager') \
         local slot = vim:GetAttribute('{slot_attr}') \
         if type(slot) ~= 'string' then return false end \
         local pad = Enum.UserInputType[slot] \
         if not pad then return false end \
         local uis = game:GetService('UserInputService') \
         if not uis:GetGamepadConnected(pad) then return false end \
         for _, obj in ipairs(uis:GetGamepadState(pad)) do \
           local n = obj.KeyCode.Name \
           if n == 'Thumbstick1' or n == 'Thumbstick2' then \
             if obj.Position.X ~= 0 or obj.Position.Y ~= 0 then return true end \
           elseif (n == 'ButtonL2' or n == 'ButtonR2') and obj.Position.Z ~= 0 then \
             return true \
           elseif uis:IsGamepadButtonDown(pad, obj.KeyCode) then \
             return true \
           end \
         end \
         return false"
    );
    let Ok(response) = call_execute_luau(
        bridge,
        BridgeTarget::Client,
        player,
        &code,
        "ReniumGamepadHeldCheck",
        5.0,
        2.0,
    ) else {
        return false;
    };
    response
        .get("results")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_bool)
        == Some(true)
}

#[cfg(windows)]
fn release_all_held_on_failure(bridge: &BridgeServer, player: Option<&str>, release_virtual: bool) {
    if release_virtual {
        let _ = send_virtual_input(bridge, player, vec![json!({ "type": "release" })], None);
    }
    let Ok(pin) = bridge.runtime_pin_for_selector(BridgeTarget::Client, player) else {
        return;
    };
    let Ok(pid) = bridge.studio_pid_for_selector(BridgeTarget::Client, player) else {
        return;
    };
    if release_gamepad_inputs(pid, &pin.runtime_id) {
        return;
    }
    if client_has_held_virtual_gamepad(bridge, player)
        && ensure_gamepad_ready(bridge, player, pid, &pin.runtime_id).is_ok()
    {
        release_gamepad_inputs(pid, &pin.runtime_id);
    }
}

#[cfg(windows)]
fn flush_non_gamepad_actions(
    actions: Vec<InputAction>,
    step_offset: usize,
    player: Option<&str>,
    bridge: &BridgeServer,
    sent: &mut bool,
) -> Result<Value> {
    let sub = InputRequest {
        player: player.map(str::to_string),
        actions,
        _bridge_ports: None,
        _bridge_wait_seconds: None,
    };
    if os_input_preferred() && sub.actions.iter().any(needs_window) {
        os_input_result_at(&sub, bridge, step_offset, sent)
    } else {
        virtual_input_result_at(&sub, bridge, step_offset, false, sent)
    }
}

#[cfg(windows)]
fn gamepad_step_error(error: anyhow::Error, step_index: usize) -> anyhow::Error {
    anyhow::Error::from(VirtualInputFailure {
        message: format!("{error:#}"),
        step: Some(step_index + 1),
    })
}

#[cfg(windows)]
fn combined_input_method(used_virtual: bool, used_os: bool) -> &'static str {
    match (used_virtual, used_os) {
        (false, false) => "gamepad",
        (true, false) => "gamepad+virtual",
        (false, true) => "gamepad+os",
        (true, true) => "gamepad+mixed",
    }
}

#[cfg(windows)]
fn record_other_method(result: &Value, used_virtual: &mut bool, used_os: &mut bool) {
    match result.get("inputMethod").and_then(Value::as_str) {
        Some("os") => *used_os = true,
        Some("mixed") => {
            *used_virtual = true;
            *used_os = true;
        }
        _ => *used_virtual = true,
    }
}

#[cfg(windows)]
struct GamepadRunContext<'a> {
    pid: u32,
    runtime_id: &'a str,
    player: Option<&'a str>,
    bridge: &'a BridgeServer,
}

#[cfg(windows)]
#[derive(Default)]
struct GamepadSequenceState {
    slot: Option<String>,
    pending_other: Vec<InputAction>,
    other_start: Option<usize>,
    pending_gamepad: Vec<(usize, GamepadAction)>,
    used_virtual: bool,
    used_os: bool,
    observed: serde_json::Map<String, Value>,
    last_gamepad_state: Option<GamepadStateReport>,
}

#[cfg(windows)]
impl GamepadSequenceState {
    fn flush_gamepad(&mut self, ctx: &GamepadRunContext<'_>, sent: &mut bool) -> Result<()> {
        if self.pending_gamepad.is_empty() {
            return Ok(());
        }
        let slot = match &self.slot {
            Some(slot) => slot.clone(),
            None => {
                let slot = ensure_gamepad_ready(ctx.bridge, ctx.player, ctx.pid, ctx.runtime_id)?;
                self.slot = Some(slot.clone());
                slot
            }
        };
        let batch = std::mem::take(&mut self.pending_gamepad);
        *sent = true;
        let state = execute_gamepad_actions(ctx.pid, ctx.runtime_id, &batch)
            .map_err(|(step_index, error)| gamepad_step_error(error, step_index))?;
        observe_gamepad_batch(ctx.bridge, ctx.player, &slot, &batch, &mut self.observed)?;
        self.last_gamepad_state = Some(state);
        Ok(())
    }

    fn flush_other(
        &mut self,
        ctx: &GamepadRunContext<'_>,
        merged_other: &mut Option<Value>,
        sent: &mut bool,
    ) -> Result<()> {
        if self.pending_other.is_empty() {
            return Ok(());
        }
        let offset = self.other_start.take().unwrap_or(0);
        let result = flush_non_gamepad_actions(
            std::mem::take(&mut self.pending_other),
            offset,
            ctx.player,
            ctx.bridge,
            sent,
        )?;
        record_other_method(&result, &mut self.used_virtual, &mut self.used_os);
        *merged_other = Some(merge_segment_result(merged_other.take(), result));
        Ok(())
    }
}

#[cfg(windows)]
fn gamepad_input_result(request: &InputRequest, bridge: &BridgeServer) -> Result<Value> {
    let mut parsed = Vec::with_capacity(request.actions.len());
    for action in &request.actions {
        if is_gamepad_action(action) {
            parsed.push(Some(parse_gamepad_step(action)?));
        } else {
            parsed.push(None);
        }
    }
    let active_runtimes: Vec<String> = bridge
        .list_bridge_clients()
        .into_iter()
        .filter(|client| client.get("role").and_then(Value::as_str) == Some("play-client"))
        .filter_map(|client| {
            client
                .get("runtimeId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    prune_gamepad_sessions(&active_runtimes);

    let player = request.player.as_deref();
    let pin = bridge.runtime_pin_for_selector(BridgeTarget::Client, player)?;
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;

    let ctx = GamepadRunContext {
        pid,
        runtime_id: &pin.runtime_id,
        player,
        bridge,
    };
    let mut merged_other: Option<Value> = None;
    let mut sent = false;
    match run_gamepad_sequence(
        &request.actions,
        &parsed,
        &ctx,
        &mut merged_other,
        &mut sent,
    ) {
        Ok(result) => Ok(result),
        Err(error) => {
            if sent {
                release_all_held_on_failure(bridge, player, true);
            }
            Err(error)
        }
    }
}

#[cfg(windows)]
fn run_gamepad_sequence(
    actions: &[InputAction],
    parsed: &[Option<ParsedGamepadStep>],
    ctx: &GamepadRunContext<'_>,
    merged_other: &mut Option<Value>,
    sent: &mut bool,
) -> Result<Value> {
    let mut state = GamepadSequenceState::default();

    for (index, (action, step)) in actions.iter().zip(parsed.iter().copied()).enumerate() {
        if let Some(step) = step {
            state.flush_other(ctx, merged_other, sent)?;
            queue_gamepad_step(&mut state, ctx, index, step, sent)?;
            continue;
        }
        if matches!(action.action, InputActionKind::Wait) && state.pending_other.is_empty() {
            state.flush_gamepad(ctx, sent)?;
            let ms = action.ms.unwrap_or(0).min(MAX_KEY_HOLD_MS);
            if ms > 0 {
                thread::sleep(Duration::from_millis(ms));
            }
            continue;
        }
        state.flush_gamepad(ctx, sent)?;
        state.other_start.get_or_insert(index);
        state.pending_other.push(action.clone());
    }

    state.flush_gamepad(ctx, sent)?;
    state.flush_other(ctx, merged_other, sent)?;

    let mut result = merged_other
        .take()
        .unwrap_or_else(|| json!({ "ok": true, "action": "input" }));
    result["ok"] = json!(true);
    result["action"] = json!("input");
    result["actions"] = json!(actions.len());
    result["inputMethod"] = json!(combined_input_method(state.used_virtual, state.used_os));
    if let Some(report) = state.last_gamepad_state.as_ref() {
        apply_gamepad_state(
            &mut result,
            state.slot.as_deref().unwrap_or(""),
            report,
            &state.observed,
        );
    }
    Ok(result)
}

#[cfg(windows)]
fn queue_gamepad_step(
    state: &mut GamepadSequenceState,
    ctx: &GamepadRunContext<'_>,
    index: usize,
    step: ParsedGamepadStep,
    sent: &mut bool,
) -> Result<()> {
    match step {
        ParsedGamepadStep::ButtonDown(name, key_code) => {
            state.pending_gamepad.push((
                index,
                GamepadAction::Button {
                    name,
                    key_code,
                    pressed: true,
                },
            ));
        }
        ParsedGamepadStep::ButtonUp(name, key_code) => {
            state.pending_gamepad.push((
                index,
                GamepadAction::Button {
                    name,
                    key_code,
                    pressed: false,
                },
            ));
        }
        ParsedGamepadStep::ButtonPress(name, key_code, hold_ms) => {
            state.pending_gamepad.push((
                index,
                GamepadAction::Button {
                    name,
                    key_code,
                    pressed: true,
                },
            ));
            state.flush_gamepad(ctx, sent)?;
            if hold_ms > 0 {
                thread::sleep(Duration::from_millis(hold_ms));
            }
            state.pending_gamepad.push((
                index,
                GamepadAction::Button {
                    name,
                    key_code,
                    pressed: false,
                },
            ));
        }
        ParsedGamepadStep::Stick(name, key_code, x, y) => {
            state.pending_gamepad.push((
                index,
                GamepadAction::Stick {
                    name,
                    key_code,
                    x,
                    y,
                },
            ));
        }
        ParsedGamepadStep::Trigger(name, key_code, value) => {
            state.pending_gamepad.push((
                index,
                GamepadAction::Trigger {
                    name,
                    key_code,
                    value,
                },
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn gamepad_input_result(request: &InputRequest, _bridge: &BridgeServer) -> Result<Value> {
    let mut first = None;
    for action in &request.actions {
        if is_gamepad_action(action) {
            let name = resolve_gamepad_action(action)?.name;
            first.get_or_insert(name);
        }
    }
    bail!(
        "Virtual gamepad input ({}) is only supported on Windows",
        first.unwrap_or("gamepad")
    )
}

pub(crate) fn input_result(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
    let request: InputRequest = serde_json::from_value(parameters.clone())?;
    if request.actions.is_empty() || request.actions.len() > 256 {
        bail!("input requires 1 through 256 actions");
    }
    if let Some(player) = request.player.as_deref() {
        wait_for_player_bridge(bridge, player, 8.0)?;
    }
    if request.actions.iter().any(is_gamepad_action) {
        return gamepad_input_result(&request, bridge)
            .map_err(|error| stopped_chain_error(error, &request.actions));
    }
    #[cfg(windows)]
    {
        let mut sent = false;
        let use_os = os_input_preferred() && request.actions.iter().any(needs_window);
        let outcome = if use_os {
            os_input_result_at(&request, bridge, 0, &mut sent)
        } else {
            virtual_input_result_at(&request, bridge, 0, true, &mut sent)
        };
        if outcome.is_err() && sent {
            release_all_held_on_failure(bridge, request.player.as_deref(), use_os);
        }
        return outcome.map_err(|error| stopped_chain_error(error, &request.actions));
    }
    #[cfg(not(windows))]
    virtual_input_result(&request, bridge)
        .map_err(|error| stopped_chain_error(error, &request.actions))
}

/// Verified GUI clicks and waits run in the client and need no Studio window.
#[cfg(windows)]
fn needs_window(action: &InputAction) -> bool {
    !matches!(action.action, InputActionKind::Wait) && verified_click_command(action, 0).is_none()
}

#[cfg(windows)]
fn os_input_result_at(
    request: &InputRequest,
    bridge: &BridgeServer,
    step_offset: usize,
    sent: &mut bool,
) -> Result<Value> {
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
                    *sent = true;
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
                *sent = true;
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
                    *sent = true;
                    input_inject::post_mouse_click(
                        &window,
                        x + offset_x + dx,
                        y + offset_y + dy,
                        false,
                        30,
                    )?;
                    position = Some((x, y));
                }
                let text = action
                    .text
                    .as_deref()
                    .context("text action requires text")?;
                *sent = true;
                input_inject::post_text(&window, text)?;
            }
            InputActionKind::Move
            | InputActionKind::MouseDown
            | InputActionKind::MouseUp
            | InputActionKind::Click
            | InputActionKind::ScrollUp
            | InputActionKind::ScrollDown => {
                if let Some(click) = verified_click_command(action, step_offset + index + 1) {
                    let (x, y) = action_position(action, bridge, player, position)?;
                    *sent = true;
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
                *sent = true;
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
            InputActionKind::Drag => {
                used_os = true;
                let (from_x, from_y) = action_position(action, bridge, player, position)?;
                let (to_x, to_y) = action_position(&drag_end(action), bridge, player, None)?;
                let (dx, dy) = *calibration
                    .get_or_insert_with(|| input_delta(bridge, player, &window, from_x, from_y));
                let right = matches!(action.button, Some(MouseButton::Right));
                let at = |x: i32, y: i32| (x + offset_x + dx, y + offset_y + dy);
                let (x, y) = at(from_x, from_y);
                *sent = true;
                input_inject::post_mouse_move(&window, x, y)?;
                input_inject::post_mouse_button(&window, x, y, right, true)?;
                let ms = action.ms.unwrap_or(DEFAULT_DRAG_MS).min(MAX_DRAG_MS);
                let moves = (ms / 16).max(4) as i32;
                let moved = (1..=moves).try_for_each(|index| {
                    thread::sleep(Duration::from_millis(16));
                    let (x, y) = at(
                        from_x + (to_x - from_x) * index / moves,
                        from_y + (to_y - from_y) * index / moves,
                    );
                    input_inject::post_mouse_move(&window, x, y)
                });
                thread::sleep(Duration::from_millis(16));
                let (x, y) = at(to_x, to_y);
                input_inject::post_mouse_button(&window, x, y, right, false)?;
                moved?;
                position = Some((to_x, to_y));
            }
            InputActionKind::Wait => {
                thread::sleep(Duration::from_millis(action.ms.unwrap_or(0).min(10_000)));
            }
            InputActionKind::Dismiss | InputActionKind::Press => {
                let command = core_gui_command(action)?;
                *sent = true;
                let response = send_virtual_input(bridge, player, vec![command], None)?;
                core_gui = response.get("coreGui").cloned();
                used_virtual = true;
            }
            InputActionKind::Stick | InputActionKind::Trigger => {
                bail!("Gamepad axis actions must run on the virtual gamepad path");
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
        [action.path.as_deref(), action.to_path.as_deref()]
            .into_iter()
            .flatten()
            .any(|path| !is_world_path(path))
    })
}

/// The plugin build from which pointer events are paced a frame apart, so a
/// pressed button stays down while the pointer moves, and `drag` exists.
const HELD_POINTER_BUILD_UNIX: i64 = 1_795_662_400;

fn holds_a_button(actions: &[InputAction]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action.action,
            InputActionKind::MouseDown | InputActionKind::Drag
        )
    })
}

fn predates(clients: &[Value], build: i64) -> bool {
    clients
        .iter()
        .filter_map(|client| client.get("bridgeBuildUnix").and_then(Value::as_i64))
        .any(|client| client < build)
}

fn predates_gui_path_input(clients: &[Value]) -> bool {
    predates(clients, GUI_PATH_INPUT_BUILD_UNIX)
}

#[cfg(not(windows))]
fn virtual_input_result(request: &InputRequest, bridge: &BridgeServer) -> Result<Value> {
    let mut sent = false;
    virtual_input_result_at(request, bridge, 0, true, &mut sent)
}

fn virtual_input_result_at(
    request: &InputRequest,
    bridge: &BridgeServer,
    step_offset: usize,
    allow_strict: bool,
    sent: &mut bool,
) -> Result<Value> {
    if names_gui_path(&request.actions) && predates_gui_path_input(&bridge.list_bridge_clients()) {
        bail!(
            "A connected Studio runs an older Renium plugin than input on GUI paths needs; restart Studio to load the installed one"
        );
    }
    if holds_a_button(&request.actions)
        && predates(&bridge.list_bridge_clients(), HELD_POINTER_BUILD_UNIX)
    {
        bail!(
            "A connected Studio runs an older Renium plugin than a held mouse button needs; restart Studio to load the installed one"
        );
    }
    let player = request.player.as_deref();
    let commands = virtual_input_commands(
        &request.actions,
        step_offset,
        allow_strict,
        &mut |action, previous| action_position(action, bridge, player, previous),
    )?;
    *sent = true;
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
        "heldButtons",
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
        let commands =
            virtual_input_commands(&request.actions, 0, true, &mut |action, previous| {
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
    fn a_drag_is_one_plugin_command_with_both_ends() {
        let (commands, located) = plan(json!([
            { "action": "drag", "path": "Settings.Volume.Knob", "toX": 315, "toY": 430 },
            { "action": "mouse-down" },
            { "action": "drag", "path": "Workspace.Crate", "toPath": "Inventory.Slot", "ms": 10_000, "button": "right" },
        ]));
        assert_eq!(types(&commands), ["drag", "move", "button", "drag"]);
        assert_eq!(
            commands[0],
            json!({ "type": "drag", "step": 1, "ms": 300, "button": "left", "path": "Settings.Volume.Knob", "toX": 315, "toY": 430 })
        );
        assert_eq!(commands[1], json!({ "type": "move", "x": 315, "y": 430 }));
        assert_eq!(
            commands[3],
            json!({ "type": "drag", "step": 3, "ms": 10_000, "button": "right", "x": 100, "y": 200, "toPath": "Inventory.Slot" })
        );
        assert_eq!(located, ["Workspace.Crate"]);

        let request = |actions: Value| request(actions).actions;
        let drag =
            request(json!([{ "action": "drag", "x": 1, "y": 2, "toPath": "Inventory.Slot" }]));
        assert!(names_gui_path(&drag) && holds_a_button(&drag));
        assert_eq!(action_label(&drag[0]), "drag to Inventory.Slot");
        let long = request(
            json!([{ "action": "drag", "x": 1, "y": 2, "toX": 3, "toY": 4, "ms": 10_001 }]),
        );
        assert!(virtual_input_commands(&long, 0, true, &mut |_, _| Ok((0, 0))).is_err());
        assert!(holds_a_button(&request(
            json!([{ "action": "mouse-down", "x": 1, "y": 2 }])
        )));
        assert!(!holds_a_button(&request(
            json!([{ "action": "click", "x": 1, "y": 2 }])
        )));
        let older =
            json!({ "role": "play-client", "bridgeBuildUnix": HELD_POINTER_BUILD_UNIX - 1 });
        let current = json!({ "role": "edit", "bridgeBuildUnix": HELD_POINTER_BUILD_UNIX });
        assert!(predates(&[current.clone(), older], HELD_POINTER_BUILD_UNIX));
        assert!(!predates(&[current], HELD_POINTER_BUILD_UNIX));
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
            virtual_input_commands(&nowhere.actions, 0, true, &mut |_, previous| previous
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

    #[test]
    fn mixed_gamepad_chain_keeps_full_step_numbers_and_labels() {
        let full = request(json!([
            { "action": "key-down", "key": "ButtonA" },
            { "action": "click", "path": "Missing.Btn" },
            { "action": "wait", "ms": 100 },
        ]));
        let sub_commands =
            virtual_input_commands(&full.actions[1..], 1, false, &mut |_, _| Ok((0, 0))).unwrap();
        assert_eq!(sub_commands[0]["step"], 2);
        assert_eq!(sub_commands[0]["lenient"], true);
        let failure = anyhow::Error::from(VirtualInputFailure {
            message: "GUI element Missing.Btn was not found".to_string(),
            step: Some(sub_commands[0]["step"].as_u64().unwrap() as usize),
        });
        assert_eq!(
            stopped_chain_error(failure, &full.actions).to_string(),
            "Action 2 of 3 (click Missing.Btn) failed; action 1 ran, action 3 did not: GUI element Missing.Btn was not found"
        );
        assert_eq!(action_label(&full.actions[0]), "kd ButtonA");
    }

    #[cfg(windows)]
    #[test]
    fn gamepad_hold_uses_sixty_second_cap_and_merges_mixed_fields() {
        let long_press = action(json!({ "action": "key-press", "key": "ButtonA", "ms": 30_000 }));
        assert_eq!(
            parse_gamepad_step(&long_press).unwrap(),
            ParsedGamepadStep::ButtonPress("ButtonA", 1002, 30_000)
        );
        let capped = action(json!({ "action": "key-press", "key": "ButtonA", "ms": 90_000 }));
        assert_eq!(
            parse_gamepad_step(&capped).unwrap(),
            ParsedGamepadStep::ButtonPress("ButtonA", 1002, MAX_KEY_HOLD_MS)
        );

        let merged = merge_segment_result(
            Some(json!({
                "actions": 1,
                "heldKeys": ["W"],
                "keysObserved": { "W": true },
                "verifiedClicks": 1,
                "window": "Client"
            })),
            json!({
                "actions": 1,
                "verifiedClicks": 2,
                "unconfirmedClicks": 1,
                "coreGui": { "dismissed": true },
                "throughSystemUi": ["Prompt"]
            }),
        );
        assert_eq!(merged["heldKeys"], json!(["W"]));
        assert_eq!(merged["keysObserved"], json!({ "W": true }));
        assert_eq!(merged["verifiedClicks"], json!(3));
        assert_eq!(merged["unconfirmedClicks"], json!(1));
        assert_eq!(merged["window"], json!("Client"));
        assert_eq!(merged["coreGui"], json!({ "dismissed": true }));
        assert_eq!(merged["throughSystemUi"], json!(["Prompt"]));

        let mut reported = merged;
        let mut state = GamepadStateReport {
            slot: "Gamepad2".to_string(),
            ..Default::default()
        };
        state.held_buttons.push("ButtonA");
        state.sticks.insert("Thumbstick1", [0.0, 1.0]);
        state.triggers.insert("ButtonR2", 0.7);
        let mut observed = serde_json::Map::new();
        observed.insert("ButtonA".to_string(), json!(true));
        apply_gamepad_state(&mut reported, "Gamepad2", &state, &observed);
        assert_eq!(reported["gamepad"], json!("Gamepad2"));
        assert_eq!(reported["gamepadObserved"], json!({ "ButtonA": true }));
        assert_eq!(reported["heldGamepadButtons"], json!(["ButtonA"]));
        assert_eq!(
            reported["gamepadSticks"],
            json!({ "Thumbstick1": [0.0, 1.0] })
        );
        assert_eq!(reported["gamepadTriggers"], json!({ "ButtonR2": 0.7 }));
    }

    #[cfg(windows)]
    #[test]
    fn gamepad_init_slot_recovery_and_error_chain() {
        // Fresh connect (no savedSlot) never seeds real-controller state.
        let fresh = parse_gamepad_initial_state(
            &json!({
                "player": "Player1",
                "connectedBefore": ["Gamepad1"],
                "savedSlot": null,
                "buttons": ["ButtonA"],
                "sticks": { "Thumbstick1": [0.2, -0.3] },
                "triggers": { "ButtonR2": 0.5 }
            }),
            "nonce-1".to_string(),
        )
        .unwrap();
        assert_eq!(fresh.connected_before, vec!["Gamepad1"]);
        assert!(fresh.initial.existing_slot.is_none());
        assert!(fresh.initial.held_buttons.is_empty());
        assert!(fresh.initial.sticks.is_empty());
        assert!(fresh.initial.triggers.is_empty());

        // Daemon restart recovery (savedSlot present) seeds only Renium's own slot.
        let recovered = parse_gamepad_initial_state(
            &json!({
                "player": "Player1",
                "connectedBefore": ["Gamepad1", "Gamepad2"],
                "savedSlot": "Gamepad2",
                "buttons": ["ButtonA", "ButtonL2"],
                "sticks": { "Thumbstick1": [0.5, -1.0] },
                "triggers": { "ButtonR2": 0.75 }
            }),
            "nonce-2".to_string(),
        )
        .unwrap();
        assert_eq!(recovered.initial.player_name, "Player1");
        assert_eq!(recovered.initial.existing_slot.as_deref(), Some("Gamepad2"));
        assert_eq!(
            recovered.initial.held_buttons,
            vec![("ButtonA", 1002), ("ButtonL2", 1007)]
        );
        assert_eq!(
            recovered.initial.sticks,
            vec![("Thumbstick1", 1016, 0.5, -1.0)]
        );
        assert_eq!(recovered.initial.triggers, vec![("ButtonR2", 1006, 0.75)]);

        let chained = anyhow::anyhow!("inner cause").context("outer failure");
        let step_err = gamepad_step_error(chained, 2);
        let failure = step_err.downcast_ref::<VirtualInputFailure>().unwrap();
        assert_eq!(failure.step, Some(3));
        assert!(failure.message.contains("outer failure"));
        assert!(failure.message.contains("inner cause"));
        assert_eq!(combined_input_method(false, false), "gamepad");
        assert_eq!(combined_input_method(true, false), "gamepad+virtual");
        assert_eq!(combined_input_method(false, true), "gamepad+os");
        assert_eq!(combined_input_method(true, true), "gamepad+mixed");
    }

    #[test]
    fn resolve_gamepad_action_validates_axis_ranges() {
        let stick = |x: f32, y: f32| InputAction {
            action: InputActionKind::Stick,
            key: None,
            text: None,
            path: None,
            x: None,
            y: None,
            to_path: None,
            to_x: None,
            to_y: None,
            button: None,
            ms: None,
            stick: Some("L".to_string()),
            trigger: None,
            axis_x: Some(x),
            axis_y: Some(y),
            value: None,
        };
        let trigger = |value: f32| InputAction {
            action: InputActionKind::Trigger,
            key: None,
            text: None,
            path: None,
            x: None,
            y: None,
            to_path: None,
            to_x: None,
            to_y: None,
            button: None,
            ms: None,
            stick: None,
            trigger: Some("R2".to_string()),
            axis_x: None,
            axis_y: None,
            value: Some(value),
        };

        assert_eq!(
            resolve_gamepad_action(&stick(-1.0, 1.0)).unwrap().name,
            "Thumbstick1"
        );
        assert_eq!(
            resolve_gamepad_action(&trigger(0.0)).unwrap().name,
            "ButtonR2"
        );
        assert_eq!(
            resolve_gamepad_action(&trigger(1.0)).unwrap().name,
            "ButtonR2"
        );

        for bad in [
            stick(1.5, 0.0),
            stick(0.0, -1.1),
            stick(f32::NAN, 0.0),
            stick(0.0, f32::INFINITY),
            trigger(-0.1),
            trigger(1.1),
            trigger(f32::NAN),
        ] {
            assert!(resolve_gamepad_action(&bad).is_err());
        }
    }
}
