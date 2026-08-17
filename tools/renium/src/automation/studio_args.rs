use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::op;
use crate::cli::{
    BridgeConnectionArgs, ClickArgs, EditorReviewDecisionArgs, ExecuteLuauArgs, GotoArgs, KeyArgs,
    PluginConsoleOutputArgs, PressArgs, ShotArgs, StartStopPlayArgs, StudioChangeStateArgs,
    StudioDeviceArgs, TestArgs, TypeArgs, UiArgs, WaitUntilArgs,
};

fn object(parameters: &Value) -> Result<&Map<String, Value>> {
    parameters.as_object().context("p must be an object")
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String> {
    string(object, key).with_context(|| format!("p.{key} is required"))
}

fn boolean(object: &Map<String, Value>, key: &str) -> Result<bool> {
    match object.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => bail!("p.{key} must be a boolean"),
    }
}

fn number<T>(object: &Map<String, Value>, key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let Some(value) = string(object, key) else {
        return Ok(default);
    };
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("p.{key} has an invalid numeric value: {error}"))
}

fn optional_number<T>(object: &Map<String, Value>, key: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    string(object, key)
        .map(|value| {
            value
                .parse()
                .map_err(|error| anyhow::anyhow!("p.{key} has an invalid numeric value: {error}"))
        })
        .transpose()
}

fn required_number<T>(object: &Map<String, Value>, key: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    optional_number(object, key)?.with_context(|| format!("p.{key} is required"))
}

fn strings(object: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    match object.get(key) {
        None => Ok(Vec::new()),
        Some(Value::String(value)) => Ok(value.split(',').map(str::to_string).collect()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .with_context(|| format!("p.{key} must contain strings"))
            })
            .collect(),
        Some(_) => bail!("p.{key} must be a string or string array"),
    }
}

fn bridge(object: &Map<String, Value>) -> Result<BridgeConnectionArgs> {
    let mut bridge = BridgeConnectionArgs::local(number(object, "bridgeWaitSeconds", 2.0)?);
    if let Some(ports) = string(object, "bridgePorts") {
        bridge.ports = ports;
    }
    Ok(bridge)
}

pub(super) fn live(operation: u16, parameters: &Value) -> Result<StudioChangeStateArgs> {
    let object = object(parameters)?;
    let action_results = match object.get("ackActionResults") {
        None => "{}".to_string(),
        Some(Value::String(value)) => value.clone(),
        Some(value @ Value::Object(_)) => serde_json::to_string(value)?,
        Some(_) => bail!("p.ackActionResults must be an object or JSON string"),
    };
    Ok(StudioChangeStateArgs {
        bridge: bridge(object)?,
        services: string(object, "services").unwrap_or_default(),
        reset: boolean(object, "reset")?,
        replace_services: boolean(object, "replaceServices")?,
        clear_pending: operation == op::DISCARD_PENDING,
        no_start: operation == op::LIVE_STATUS,
        stop: operation == op::LIVE_STOP,
        ack_seq: optional_number(object, "ackSeq")?,
        ack_runtime_settings_seq: optional_number(object, "ackRuntimeSettingsSeq")?,
        ack_actions: strings(object, "ackActions")?,
        ack_action_results: action_results,
        runtime_id: string(object, "runtimeId"),
        suppress_seconds: optional_number(object, "suppressSeconds")?,
        event_wait_seconds: optional_number(object, "eventWaitSeconds")?,
        context_bound: boolean(object, "contextBound")?,
    })
}

pub(super) fn luau(root: &Path, parameters: &Value) -> Result<ExecuteLuauArgs> {
    let object = object(parameters)?;
    let file = string(object, "file").map(PathBuf::from).map(|file| {
        if file.is_absolute() {
            file
        } else {
            root.join(file)
        }
    });
    Ok(ExecuteLuauArgs {
        bridge: bridge(object)?,
        code: string(object, "code"),
        file,
        client: boolean(object, "client")?,
        player: string(object, "player"),
        timeout: number(object, "timeout", 10.0)?,
    })
}

pub(super) fn console(parameters: &Value) -> Result<PluginConsoleOutputArgs> {
    let object = object(parameters)?;
    Ok(PluginConsoleOutputArgs {
        bridge: bridge(object)?,
        limit: number(object, "limit", 200)?,
        since_seq: number(object, "sinceSeq", 0)?,
        from_oldest: boolean(object, "fromOldest")?,
        clear: boolean(object, "clear")?,
        client: boolean(object, "client")?,
        player: string(object, "player"),
        follow: boolean(object, "follow")?,
        grep: string(object, "grep"),
        level: string(object, "level"),
        interval_ms: number(object, "intervalMs", 200)?,
    })
}

pub(super) fn play(operation: u16, parameters: &Value) -> Result<StartStopPlayArgs> {
    let object = object(parameters)?;
    Ok(StartStopPlayArgs {
        bridge: bridge(object)?,
        start: operation == op::PLAY_START,
        stop: operation == op::PLAY_STOP,
        players: optional_number(object, "players")?,
        mode: string(object, "mode"),
    })
}

pub(super) fn test(parameters: &Value) -> Result<TestArgs> {
    let object = object(parameters)?;
    Ok(TestArgs {
        mode: string(object, "mode").unwrap_or_else(|| "play".to_string()),
        players: optional_number(object, "players")?,
        timeout: number(object, "timeout", 30.0)?,
        fail_on_error: boolean(object, "failOnError")?,
        player: string(object, "player"),
    })
}

fn vector(object: &Map<String, Value>, names: &[&str]) -> Result<Option<String>> {
    let value = names.iter().find_map(|name| object.get(*name));
    match value {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Array(values)) if values.len() == 3 && values.iter().all(Value::is_number) => {
            Ok(Some(
                values
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ))
        }
        Some(_) => bail!("Camera vectors must be strings or arrays of three numbers"),
    }
}

pub(super) fn shot(root: &Path, parameters: &Value) -> Result<ShotArgs> {
    let object = object(parameters)?;
    let output = string(object, "output")
        .or_else(|| {
            string(object, "captureId")
                .or_else(|| string(object, "capture_id"))
                .map(|id| format!("{id}.png"))
        })
        .unwrap_or_else(|| "shot.png".to_string());
    let output = PathBuf::from(output);
    let output = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    let camera_position = vector(object, &["cameraPosition", "camera_position"])?;
    let look_at = vector(object, &["lookAt", "lookAtPosition", "look_at_position"])?;
    if camera_position.is_some() != look_at.is_some() {
        bail!("shot cameraPosition and lookAt must be used together");
    }
    let studio = boolean(object, "studio")?;
    let client = boolean(object, "client")?;
    let player = string(object, "player");
    if studio && (client || player.is_some()) {
        bail!("shot studio cannot be combined with client or player");
    }
    Ok(ShotArgs {
        bridge: bridge(object)?,
        output,
        player,
        studio,
        client,
        camera_position,
        look_at,
    })
}

pub(super) fn device(parameters: &Value) -> Result<StudioDeviceArgs> {
    let object = object(parameters)?;
    Ok(StudioDeviceArgs {
        action: string(object, "action").unwrap_or_else(|| "status".to_string()),
        device: string(object, "device"),
        orientation: string(object, "orientation"),
        scaling_mode: string(object, "scalingMode"),
        resolution: string(object, "resolution"),
        pixel_density: optional_number(object, "pixelDensity")?,
        bridge: bridge(object)?,
    })
}

pub(super) fn ui(parameters: &Value) -> Result<UiArgs> {
    let object = object(parameters)?;
    Ok(UiArgs {
        bridge: bridge(object)?,
        player: string(object, "player"),
        limit: number(object, "limit", 200)?,
        include_offscreen: boolean(object, "includeOffscreen")?,
    })
}

pub(super) fn press(parameters: &Value) -> Result<PressArgs> {
    let object = object(parameters)?;
    Ok(PressArgs {
        bridge: bridge(object)?,
        path: string(object, "path"),
        id: string(object, "id"),
        player: string(object, "player"),
        right: boolean(object, "right")?,
        world: boolean(object, "world")?,
        hold: number(object, "hold", 30)?,
    })
}

pub(super) fn click(parameters: &Value) -> Result<ClickArgs> {
    let object = object(parameters)?;
    Ok(ClickArgs {
        bridge: bridge(object)?,
        x: required_number(object, "x")?,
        y: required_number(object, "y")?,
        player: string(object, "player"),
        right: boolean(object, "right")?,
        hold: number(object, "hold", 30)?,
    })
}

pub(super) fn key(parameters: &Value) -> Result<KeyArgs> {
    let object = object(parameters)?;
    Ok(KeyArgs {
        bridge: bridge(object)?,
        key: required_string(object, "key")?,
        player: string(object, "player"),
        hold_ms: number(object, "holdMs", 60)?,
    })
}

pub(super) fn type_text(parameters: &Value) -> Result<TypeArgs> {
    let object = object(parameters)?;
    Ok(TypeArgs {
        bridge: bridge(object)?,
        text: required_string(object, "text")?,
        path: string(object, "path"),
        player: string(object, "player"),
        enter: boolean(object, "enter")?,
    })
}

pub(super) fn wait(parameters: &Value) -> Result<WaitUntilArgs> {
    let object = object(parameters)?;
    Ok(WaitUntilArgs {
        bridge: bridge(object)?,
        condition: required_string(object, "condition")?,
        player: string(object, "player"),
        client: boolean(object, "client")?,
        timeout: number(object, "timeout", 10.0)?,
        interval: number(object, "interval", 0.25)?,
    })
}

pub(super) fn goto(parameters: &Value) -> Result<GotoArgs> {
    let object = object(parameters)?;
    Ok(GotoArgs {
        bridge: bridge(object)?,
        target: string(object, "target"),
        pos: string(object, "pos"),
        player: string(object, "player"),
        tp: boolean(object, "tp")?,
        timeout: number(object, "timeout", 30.0)?,
        speed_multiplier: number(object, "speedMultiplier", 1.0)?,
    })
}

pub(super) fn review(parameters: &Value) -> Result<EditorReviewDecisionArgs> {
    let object = object(parameters)?;
    Ok(EditorReviewDecisionArgs {
        decision: string(object, "decision").unwrap_or_else(|| "apply".to_string()),
        review_id: string(object, "reviewId"),
        bridge: bridge(object)?,
    })
}
