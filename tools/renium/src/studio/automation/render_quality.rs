use super::*;
use clap::Args;
use serde::{Deserialize, Serialize};

const MAX_LEVEL: u8 = 21;

#[derive(Args)]
pub(crate) struct RenderQualityArgs {
    #[arg(
        default_value = "show",
        value_parser = ["show", "set", "max", "auto", "restore"],
        help = "show, set, max (both levels 21), auto (both automatic) or restore"
    )]
    action: String,
    #[arg(
        short,
        long,
        help = "Play client name or index; changes that client's Studio process"
    )]
    player: Option<String>,
    #[arg(
        long,
        value_name = "LEVEL",
        value_parser = parse_level,
        help = "Edit-mode render quality: 1-21 or auto"
    )]
    edit: Option<u8>,
    #[arg(
        long,
        value_name = "LEVEL",
        value_parser = parse_level,
        help = "Play render quality: 1-21 or auto"
    )]
    play: Option<u8>,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

fn parse_level(text: &str) -> std::result::Result<u8, String> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("auto") || text.eq_ignore_ascii_case("automatic") {
        return Ok(0);
    }
    match text.parse::<u8>() {
        Ok(level) if (1..=MAX_LEVEL).contains(&level) => Ok(level),
        _ => Err(format!("use a level from 1 to {MAX_LEVEL}, or auto")),
    }
}

#[derive(Args, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct QualityValues {
    #[serde(skip_serializing_if = "Option::is_none")]
    edit: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    play: Option<u8>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QualityRequest {
    action: String,
    player: Option<String>,
    #[serde(default)]
    values: QualityValues,
}

impl QualityRequest {
    fn from_args(args: &RenderQualityArgs) -> Result<Self> {
        let (action, values) = match args.action.as_str() {
            "max" => (
                "set",
                QualityValues {
                    edit: Some(MAX_LEVEL),
                    play: Some(MAX_LEVEL),
                },
            ),
            "auto" => (
                "set",
                QualityValues {
                    edit: Some(0),
                    play: Some(0),
                },
            ),
            action => (
                action,
                QualityValues {
                    edit: args.edit,
                    play: args.play,
                },
            ),
        };
        if matches!(args.action.as_str(), "max" | "auto")
            && (args.edit.is_some() || args.play.is_some())
        {
            bail!(
                "{} sets both levels itself; use set with --edit/--play for other values",
                args.action
            );
        }
        let request = Self {
            action: action.to_string(),
            player: args.player.clone(),
            values,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        if !matches!(self.action.as_str(), "show" | "set" | "restore") {
            bail!("Render quality action must be show, set, max, auto or restore");
        }
        let mut count = 0;
        for (name, value) in [("edit", self.values.edit), ("play", self.values.play)] {
            if let Some(value) = value {
                if value > MAX_LEVEL {
                    bail!("--{name} must be a level from 1 to {MAX_LEVEL}, or auto");
                }
                count += 1;
            }
        }
        if (self.action == "set") != (count > 0) {
            bail!("Only set accepts --edit/--play; set needs at least one of them");
        }
        if self
            .player
            .as_ref()
            .is_some_and(|player| player.trim().is_empty() || player == "0")
        {
            bail!("Choose a play client name or a positive index");
        }
        Ok(())
    }
}

pub(crate) fn command(args: RenderQualityArgs) -> Result<()> {
    let request = QualityRequest::from_args(&args)?;
    let result = daemon_result(
        op::RENDER_QUALITY,
        None,
        serde_json::to_value(request)?,
        false,
        Some(&args.bridge),
    )?;
    print_json_output(&result, false)
}

// RenderSettings belong to a Studio process. A process that hosts several
// play clients cannot carry a per-client value.
#[cfg(any(windows, target_os = "macos", test))]
fn verify_process_scope(clients: &[Value], runtime: &str, pid: u32) -> Result<()> {
    let shared = clients.iter().any(|entry| {
        entry["pid"].as_u64() == Some(u64::from(pid))
            && entry["runtimeId"].as_str() != Some(runtime)
            && entry["role"].as_str() == Some(BRIDGE_ROLE_PLAY_CLIENT)
    });
    if shared {
        bail!(
            "This Studio process hosts multiple play clients; render quality cannot be set per client in this layout"
        );
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn result(
    parameters: &Value,
    bridge: &BridgeServer,
    wait_seconds: f64,
) -> Result<Value> {
    let mut parameters = parameters.clone();
    let object = parameters
        .as_object_mut()
        .context("Expected render quality options")?;
    object.remove("bridgeWaitSeconds");
    object.remove("bridgePorts");
    let request: QualityRequest = serde_json::from_value(parameters)?;
    request.validate()?;
    let client = request.player.is_some();
    let target = if client {
        BridgeTarget::Client
    } else {
        BridgeTarget::Edit
    };
    if let Some(player) = &request.player {
        wait_for_player_bridge(bridge, player, wait_seconds)?;
    } else {
        bridge.wait_for_target(wait_seconds, target)?;
    }
    let pin = bridge.runtime_pin_for_selector(target, request.player.as_deref())?;
    let pid = bridge.studio_pid_for_runtime(target, &pin.runtime_id)?;
    if client {
        let mut clients = bridge.list_bridge_clients();
        for entry in &mut clients {
            if entry["role"].as_str() == Some(BRIDGE_ROLE_PLAY_CLIENT)
                && let Some(runtime) = entry["runtimeId"].as_str()
            {
                entry["pid"] = json!(bridge.studio_pid_for_runtime(BridgeTarget::Client, runtime)?);
            }
        }
        verify_process_scope(&clients, &pin.runtime_id, pid)?;
    }
    let mut result = bridge
        .call_for_selector_runtime_with_timeout(
            "renderQuality",
            json!({ "action": request.action, "values": request.values, "client": client }),
            target,
            request.player.as_deref(),
            Some(&pin.runtime_id),
            Some(Duration::from_secs(3)),
        )
        .context("Render quality failed; this command requires the updated Renium Studio plugin")?;
    ensure_plugin_api_ok(&result)?;
    result["pid"] = json!(pid);
    result["player"] = json!(request.player);
    Ok(result)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn result(_: &Value, _: &BridgeServer, _: f64) -> Result<Value> {
    bail!("Studio render quality requires Windows or macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(parts: &[&str]) -> RenderQualityArgs {
        match crate::cli::Cli::try_parse_from(parts).unwrap().command {
            crate::cli::Commands::RenderQuality(args) => args,
            _ => panic!("expected gfx"),
        }
    }

    #[test]
    fn levels_parse_as_numbers_or_automatic() {
        assert_eq!(parse_level("auto"), Ok(0));
        assert_eq!(parse_level("Automatic"), Ok(0));
        assert_eq!(parse_level("21"), Ok(21));
        assert!(parse_level("0").is_err());
        assert!(parse_level("22").is_err());
        assert!(parse_level("high").is_err());
    }

    #[test]
    fn presets_expand_and_explicit_values_are_validated() {
        let max = QualityRequest::from_args(&args(&["rbx", "gfx", "max"])).unwrap();
        assert_eq!(max.action, "set");
        assert_eq!((max.values.edit, max.values.play), (Some(21), Some(21)));
        let auto =
            QualityRequest::from_args(&args(&["rbx", "gfx", "auto", "--player", "2"])).unwrap();
        assert_eq!((auto.values.edit, auto.values.play), (Some(0), Some(0)));
        assert_eq!(auto.player.as_deref(), Some("2"));
        let set =
            QualityRequest::from_args(&args(&["rbx", "gfx", "set", "--play", "auto"])).unwrap();
        assert_eq!((set.values.edit, set.values.play), (None, Some(0)));
        assert!(QualityRequest::from_args(&args(&["rbx", "gfx", "set"])).is_err());
        assert!(QualityRequest::from_args(&args(&["rbx", "gfx", "max", "--edit", "5"])).is_err());
        assert!(QualityRequest::from_args(&args(&["rbx", "gfx", "show", "--edit", "5"])).is_err());
        assert!(crate::cli::Cli::try_parse_from(["rbx", "gfx", "set", "--edit", "22"]).is_err());
        assert!(
            serde_json::from_value::<QualityRequest>(json!({"action":"set","values":{"game":10}}))
                .is_err()
        );
    }

    #[test]
    fn a_process_hosting_several_clients_cannot_take_a_per_client_value() {
        let clients = vec![
            json!({"pid":1,"role":"edit","runtimeId":"edit"}),
            json!({"pid":2,"role":"play-client","runtimeId":"one"}),
            json!({"pid":3,"role":"play-client","runtimeId":"two"}),
        ];
        assert!(verify_process_scope(&clients, "one", 2).is_ok());
        let mut shared = clients;
        shared.push(json!({"pid":2,"role":"play-client","runtimeId":"three"}));
        assert!(verify_process_scope(&shared, "one", 2).is_err());
    }
}
