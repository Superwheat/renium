use super::*;
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Args)]
pub(crate) struct NetworkArgs {
    #[arg(default_value = "show", value_parser = ["show", "set", "reset", "restore", "presets"])]
    action: String,
    #[arg(
        short,
        long,
        help = "Play client name or index; required for changes during Play"
    )]
    player: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "Starting template for set; explicit values override the template"
    )]
    preset: Option<Preset>,
    #[command(flatten)]
    values: NetworkValues,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum Preset {
    Normal,
    Mid,
    High,
    Poor,
}

impl Preset {
    fn values(self) -> NetworkValues {
        let (delay, jitter, loss) = match self {
            Self::Normal => (15.0, 2.0, 0.0),
            Self::Mid => (50.0, 10.0, 0.05),
            Self::High => (100.0, 15.0, 0.1),
            Self::Poor => (100.0, 100.0, 0.5),
        };
        NetworkValues {
            in_delay: Some(delay),
            out_delay: Some(delay),
            in_jitter: Some(jitter),
            out_jitter: Some(jitter),
            in_loss: Some(loss),
            out_loss: Some(loss),
        }
    }
}

#[derive(Args, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NetworkValues {
    #[arg(long, help = "Server-to-client minimum delay, 0–100 ms")]
    #[serde(skip_serializing_if = "Option::is_none")]
    in_delay: Option<f64>,
    #[arg(long, help = "Client-to-server minimum delay, 0–100 ms")]
    #[serde(skip_serializing_if = "Option::is_none")]
    out_delay: Option<f64>,
    #[arg(long, help = "Server-to-client jitter, 0–100 ms")]
    #[serde(skip_serializing_if = "Option::is_none")]
    in_jitter: Option<f64>,
    #[arg(long, help = "Client-to-server jitter, 0–100 ms")]
    #[serde(skip_serializing_if = "Option::is_none")]
    out_jitter: Option<f64>,
    #[arg(
        long,
        help = "Server-to-client packet loss, 0–0.5 percent (0.5 means 0.5%)"
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    in_loss: Option<f64>,
    #[arg(long, help = "Client-to-server packet loss, 0–0.5 percent")]
    #[serde(skip_serializing_if = "Option::is_none")]
    out_loss: Option<f64>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NetworkRequest {
    action: String,
    player: Option<String>,
    preset: Option<Preset>,
    #[serde(default)]
    values: NetworkValues,
}

impl NetworkRequest {
    fn validate(&self) -> Result<()> {
        if !matches!(self.action.as_str(), "show" | "set" | "reset" | "restore") {
            bail!("Network action must be show, set, reset or restore");
        }
        if self.preset.is_some() && self.action != "set" {
            bail!("Only net set accepts --preset");
        }
        let mut count = 0;
        for (name, value, max) in [
            ("in-delay", self.values.in_delay, 100.0),
            ("out-delay", self.values.out_delay, 100.0),
            ("in-jitter", self.values.in_jitter, 100.0),
            ("out-jitter", self.values.out_jitter, 100.0),
            ("in-loss", self.values.in_loss, 0.5),
            ("out-loss", self.values.out_loss, 0.5),
        ] {
            if let Some(value) = value {
                if !value.is_finite() || !(0.0..=max).contains(&value) {
                    bail!("{name} must be a finite number between 0 and {max}");
                }
                count += 1;
            }
        }
        if (self.action == "set") != (count > 0 || self.preset.is_some()) {
            bail!("Only net set accepts values; set needs at least one value");
        }
        if self
            .player
            .as_ref()
            .is_some_and(|p| p.trim().is_empty() || p == "0")
        {
            bail!("Choose a play client name or a positive index");
        }
        if self.action == "restore" && self.player.is_none() {
            bail!("Restore requires --player; it restores that client's pre-override settings");
        }
        Ok(())
    }

    #[cfg(any(windows, target_os = "macos", test))]
    fn resolve_preset(&mut self) {
        if let Some(preset) = self.preset {
            let base = preset.values();
            self.values.in_delay = self.values.in_delay.or(base.in_delay);
            self.values.out_delay = self.values.out_delay.or(base.out_delay);
            self.values.in_jitter = self.values.in_jitter.or(base.in_jitter);
            self.values.out_jitter = self.values.out_jitter.or(base.out_jitter);
            self.values.in_loss = self.values.in_loss.or(base.in_loss);
            self.values.out_loss = self.values.out_loss.or(base.out_loss);
        }
    }
}

pub(crate) fn command(args: NetworkArgs) -> Result<()> {
    let request = NetworkRequest {
        action: args.action,
        player: args.player,
        preset: args.preset,
        values: args.values,
    };
    if request.action == "presets" {
        if request.preset.is_some()
            || request.player.is_some()
            || serde_json::to_value(&request.values)?
                .as_object()
                .is_some_and(|v| !v.is_empty())
        {
            bail!("net presets lists templates offline and accepts no settings or player");
        }
        let presets = [Preset::Normal, Preset::Mid, Preset::High, Preset::Poor]
            .map(|preset| json!({"name":preset,"settings":preset.values()}));
        return print_json_output(
            &json!({"presets":presets,"units":{"delay":"ms per direction","jitter":"ms per direction","loss":"percent"}}),
            false,
        );
    }
    request.validate()?;
    let result = daemon_result(
        op::NETWORK_SIMULATION,
        None,
        serde_json::to_value(request)?,
        false,
        Some(&args.bridge),
    )?;
    print_json_output(&result, false)
}

// NetworkSettings belong to a process. Never pretend a shared process is a
// per-client override, or change the server's settings in place of a client.
#[cfg(any(windows, target_os = "macos", test))]
fn verify_scope(
    clients: &[Value],
    runtime: &str,
    pid: u32,
    client: bool,
    mutation: bool,
) -> Result<()> {
    for entry in clients {
        if !client
            && mutation
            && entry["role"]
                .as_str()
                .is_some_and(|role| role.starts_with("play-"))
            && entry["launchEditRuntimeId"].as_str() == Some(runtime)
        {
            bail!("Use --player to change network simulation during Play");
        }
        if entry["pid"].as_u64() != Some(u64::from(pid))
            || entry["runtimeId"].as_str() == Some(runtime)
        {
            continue;
        }
        let role = entry["role"].as_str().unwrap_or_default();
        if client && role == BRIDGE_ROLE_PLAY_CLIENT {
            bail!(
                "This Studio process hosts multiple play clients; independent network simulation is unavailable in this layout"
            );
        }
        if client && role == "edit" {
            let selected = clients
                .iter()
                .find(|entry| entry["runtimeId"].as_str() == Some(runtime));
            if selected.is_some_and(|selected| {
                selected["placeId"] != entry["placeId"] || selected["gameId"] != entry["gameId"]
            }) {
                bail!(
                    "This Studio process also hosts another place; its network settings cannot be changed independently"
                );
            }
        }
        if !client && mutation && role.starts_with("play-") {
            bail!("Use --player to change network simulation during Play");
        }
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
        .context("Expected network options")?;
    object.remove("bridgeWaitSeconds");
    object.remove("bridgePorts");
    let mut request: NetworkRequest = serde_json::from_value(parameters)?;
    request.validate()?;
    request.resolve_preset();
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
    let mut clients = bridge.list_bridge_clients();
    for entry in &mut clients {
        let runtime = entry["runtimeId"]
            .as_str()
            .context("Network inventory omitted runtime identity")?;
        let target = match entry["role"].as_str() {
            Some("edit") => BridgeTarget::Edit,
            Some("play-client") => BridgeTarget::Client,
            Some("play-server") => BridgeTarget::Main,
            _ => continue,
        };
        entry["pid"] = json!(bridge.studio_pid_for_runtime(target, runtime)?);
    }
    verify_scope(
        &clients,
        &pin.runtime_id,
        pid,
        client,
        request.action != "show",
    )?;
    let mut result = bridge
        .call_for_selector_runtime_with_timeout(
            "networkSimulation",
            json!({"action":request.action,"values":request.values,"client":client}),
            target,
            request.player.as_deref(),
            Some(&pin.runtime_id),
            Some(Duration::from_secs(3)),
        )
        .context(
            "Network simulation failed; this command requires the updated Renium Studio plugin",
        )?;
    ensure_plugin_api_ok(&result)?;
    result["pid"] = json!(pid);
    result["player"] = json!(request.player);
    result["preset"] = json!(request.preset);
    Ok(result)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn result(_: &Value, _: &BridgeServer, _: f64) -> Result<Value> {
    bail!("Studio network simulation requires Windows or macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_validation_rejects_partial_invalid_requests_before_mutation() {
        for value in [f64::NAN, f64::INFINITY, -1.0, 100.01] {
            let request = NetworkRequest {
                action: "set".into(),
                player: Some("1".into()),
                preset: None,
                values: NetworkValues {
                    in_delay: Some(value),
                    ..Default::default()
                },
            };
            assert!(request.validate().is_err());
        }
        for (action, values, ok) in [
            ("set", json!({}), false),
            ("show", json!({"inDelay":1}), false),
            ("set", json!({"outLoss":0.51}), false),
            ("set", json!({"outLoss":0.5,"inDelay":100}), true),
            ("reset", json!({}), true),
        ] {
            let request: NetworkRequest =
                serde_json::from_value(json!({"action":action,"player":"1","values":values}))
                    .unwrap();
            assert_eq!(request.validate().is_ok(), ok);
        }
        assert!(
            serde_json::from_value::<NetworkRequest>(json!({"action":"set","values":{"other":1}}))
                .is_err()
        );
    }

    #[test]
    fn network_scope_never_broadcasts_or_retargets_another_client() {
        let clients = vec![
            json!({"pid":1,"role":"edit","runtimeId":"edit"}),
            json!({"pid":1,"role":"play-client","runtimeId":"one"}),
            json!({"pid":2,"role":"play-client","runtimeId":"two"}),
        ];
        assert!(verify_scope(&clients, "one", 1, true, true).is_ok());
        assert!(verify_scope(&clients, "edit", 1, false, true).is_err());
        assert!(verify_scope(&clients, "edit", 1, false, false).is_ok());
        let mut shared = clients;
        shared.push(json!({"pid":1,"role":"play-client","runtimeId":"three"}));
        assert!(verify_scope(&shared, "one", 1, true, true).is_err());
    }

    #[test]
    fn network_presets_are_complete_and_custom_values_win() {
        for preset in [Preset::Normal, Preset::Mid, Preset::High, Preset::Poor] {
            let mut request = NetworkRequest {
                action: "set".into(),
                player: Some("1".into()),
                preset: Some(preset),
                values: NetworkValues {
                    out_delay: Some(7.0),
                    ..Default::default()
                },
            };
            request.validate().unwrap();
            request.resolve_preset();
            request.validate().unwrap();
            assert_eq!(request.values.out_delay, Some(7.0));
            assert_eq!(
                serde_json::to_value(&request.values)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .len(),
                6
            );
        }
    }
}
