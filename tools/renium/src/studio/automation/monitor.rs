use super::*;
use clap::Args;
use serde::{Deserialize, Serialize};

#[derive(Args)]
pub(crate) struct MonitorArgs {
    #[arg(default_value = "snapshot", value_parser = ["snapshot", "start", "stop", "read", "export", "micro", "micro-start", "micro-stop", "analyze"])]
    action: String,
    #[arg(help = "Saved .gprx capture for offline analyze")]
    file: Option<PathBuf>,
    #[arg(long, help = "Analyze one MicroProfiler frame ID")]
    frame: Option<u32>,
    #[arg(
        long,
        default_value_t = 10,
        help = "Maximum scopes/counters in an analysis (1–50)"
    )]
    top: usize,
    #[arg(
        long,
        help = "MicroProfiler rolling frame limit (1–256), micro-start only"
    )]
    frames: Option<u32>,
    #[arg(
        long,
        help = "Analyze matching scope names; * wildcard and | alternatives"
    )]
    scope: Option<String>,
    #[arg(long, help = "Analyze matching thread names")]
    thread: Option<String>,
    #[arg(long, help = "Analyze matching counter paths, e.g. **/Luau/**")]
    counter: Option<String>,
    #[arg(long, help = "Capture ID returned by start")]
    capture: Option<String>,
    #[arg(long, help = "Capture duration, 0.1–120 seconds; start defaults to 10")]
    seconds: Option<f64>,
    #[arg(long, help = "Read one page of 1000 frames")]
    page: Option<usize>,
    #[arg(short, long, conflicts_with = "server")]
    player: Option<String>,
    #[arg(long, help = "Sample the play server; default is Edit mode")]
    server: bool,
    #[arg(long, help = "Destination: JSON for export/analyze, .gprx for micro")]
    out: Option<PathBuf>,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Parameters {
    action: String,
    frames: Option<u32>,
    capture_id: Option<String>,
    seconds: Option<f64>,
    page: Option<usize>,
    player: Option<String>,
    #[serde(default)]
    server: bool,
}

impl Parameters {
    fn validate(&self) -> Result<()> {
        if !matches!(
            self.action.as_str(),
            "snapshot" | "start" | "stop" | "read" | "micro" | "micro-start" | "micro-stop"
        ) {
            bail!("Unknown performance action");
        }
        if self.frames.is_some_and(|n| !(1..=256).contains(&n))
            || self.frames.is_some() && self.action != "micro-start"
        {
            bail!("Only micro-start accepts --frames (1–256)");
        }
        if self
            .seconds
            .is_some_and(|s| !s.is_finite() || !(0.1..=120.0).contains(&s))
            || self.seconds.is_some() && self.action != "start"
        {
            bail!("Only start accepts --seconds (0.1–120)");
        }
        if self.page.is_some_and(|p| !(1..=60).contains(&p))
            || self.page.is_some() && self.action != "read"
        {
            bail!("Only read accepts --page (1–60)");
        }
        if matches!(self.action.as_str(), "stop" | "read") != self.capture_id.is_some()
            || self
                .capture_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 128)
        {
            bail!("Stop/read require a capture ID; snapshot/start do not accept one");
        }
        if self.player.is_some() && self.server
            || self
                .player
                .as_ref()
                .is_some_and(|p| p.trim().is_empty() || p == "0")
        {
            bail!("Select either a play client (--player) or --server");
        }
        Ok(())
    }
}

pub(crate) fn command(args: MonitorArgs) -> Result<()> {
    if args.action == "analyze" {
        if args.capture.is_some()
            || args.seconds.is_some()
            || args.page.is_some()
            || args.player.is_some()
            || args.server
            || args.frames.is_some()
        {
            bail!("analyze reads a saved capture offline; live capture options do not apply");
        }
        let path = args
            .file
            .as_deref()
            .context("analyze requires a .gprx capture file")?;
        return print_json_output(
            &super::microprofiler::analyze(
                path,
                args.frame,
                args.top,
                [
                    args.scope.as_deref(),
                    args.thread.as_deref(),
                    args.counter.as_deref(),
                ],
                args.out.as_deref(),
            )?,
            false,
        );
    }
    if args.file.is_some()
        || args.frame.is_some()
        || args.top != 10
        || args.scope.is_some()
        || args.thread.is_some()
        || args.counter.is_some()
    {
        bail!("FILE, --frame, --top and analysis filters apply only to analyze");
    }
    let export = args.action == "export";
    let micro = matches!(args.action.as_str(), "micro" | "micro-stop");
    if (export || micro) != args.out.is_some() || export && args.page.is_some() {
        bail!("export/micro require --out; export reads every frame");
    }
    let mut parameters = Parameters {
        frames: args.frames,
        action: if export { "read".into() } else { args.action },
        capture_id: args.capture,
        seconds: args.seconds,
        page: if export { Some(1) } else { args.page },
        player: args.player,
        server: args.server,
    };
    parameters.validate()?;
    let call = |parameters: &Parameters| {
        daemon_result(
            op::PERFORMANCE_MONITOR,
            None,
            serde_json::to_value(parameters)?,
            false,
            Some(&args.bridge),
        )
    };
    let mut result = call(&parameters)?;
    if micro {
        return print_json_output(
            &super::microprofiler::save_capture(
                &args.out.context("micro requires --out")?,
                &result,
            )?,
            false,
        );
    }
    if let Some(path) = args.out {
        if result["state"] != "complete" {
            bail!("Stop the capture before exporting a stable, complete recording");
        }
        let runtime = result["runtimeId"].clone();
        let capture = result["captureId"].clone();
        let expected_count = result["frameCount"]
            .as_u64()
            .context("Capture omitted frame count")?;
        let mut frames = result["frames"]
            .as_array()
            .context("Capture omitted frames")?
            .clone();
        let mut next = result["nextPage"].as_u64();
        while let Some(page) = next {
            if page > 60
                || parameters
                    .page
                    .is_some_and(|previous| page <= previous as u64)
            {
                bail!("Invalid performance capture pagination");
            }
            parameters.page = Some(page as usize);
            let more = call(&parameters)?;
            if more["runtimeId"] != runtime
                || more["captureId"] != capture
                || more["state"] != "complete"
                || more["frameCount"].as_u64() != Some(expected_count)
            {
                bail!("Performance capture changed while exporting; no file was written");
            }
            frames.extend_from_slice(
                more["frames"]
                    .as_array()
                    .context("Capture page omitted frames")?,
            );
            next = more["nextPage"].as_u64();
        }
        if frames.len() as u64 != expected_count {
            bail!("Performance capture omitted frames; no file was written");
        }
        result["frames"] = json!(frames);
        let object = result.as_object_mut().context("Invalid capture response")?;
        object.remove("nextPage");
        object.remove("page");
        let path = std::path::absolute(path)?;
        crate::system::files::atomic_write_file(&path, &serde_json::to_vec(&result)?)?;
        return print_json_output(
            &json!({"path":path,"runtimeId":runtime,"captureId":capture,"frameCount":expected_count}),
            false,
        );
    }
    print_json_output(&result, false)
}

pub(crate) fn result(
    parameters: &Value,
    bridge: &BridgeServer,
    wait_seconds: f64,
) -> Result<Value> {
    let mut parameters = parameters.clone();
    let object = parameters
        .as_object_mut()
        .context("Expected performance options")?;
    object.remove("bridgeWaitSeconds");
    object.remove("bridgePorts");
    let parameters: Parameters = serde_json::from_value(parameters)?;
    parameters.validate()?;
    let target = if parameters.player.is_some() {
        BridgeTarget::Client
    } else if parameters.server {
        BridgeTarget::Main
    } else {
        BridgeTarget::Edit
    };
    if let Some(player) = &parameters.player {
        wait_for_player_bridge(bridge, player, wait_seconds)?;
    } else {
        bridge.wait_for_target(wait_seconds, target)?;
    }
    let pin = bridge.runtime_pin_for_selector(target, parameters.player.as_deref())?;
    if parameters.server
        && !bridge.list_bridge_clients().iter().any(|client| {
            client["runtimeId"].as_str() == Some(&pin.runtime_id)
                && client["role"].as_str() == Some(BRIDGE_ROLE_PLAY_SERVER)
        })
    {
        bail!("No play server is connected; omit --server to sample Edit mode");
    }
    let mut request = serde_json::to_value(&parameters)?;
    request["runtimeId"] = json!(pin.runtime_id);
    let mut result = bridge.call_for_selector_runtime_with_timeout(
        "performance",
        request,
        target,
        parameters.player.as_deref(),
        Some(&pin.runtime_id),
        Some(Duration::from_secs(3)),
    )?;
    ensure_plugin_api_ok(&result)?;
    if result["runtimeId"].as_str() != Some(&pin.runtime_id) {
        bail!("Performance response came from a different runtime");
    }
    result["pid"] = json!(bridge.studio_pid_for_runtime(target, &pin.runtime_id)?);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_rejects_invalid_or_ambiguous_requests_before_studio() {
        for (action, capture, seconds, page, player, server, valid) in [
            ("snapshot", None, None, None, None, false, true),
            ("start", None, Some(10.), None, Some("1"), false, true),
            ("stop", Some("id"), None, None, None, true, true),
            ("read", Some("id"), None, Some(2), None, false, true),
            ("start", None, Some(f64::NAN), None, None, false, false),
            ("start", None, Some(121.), None, None, false, false),
            ("stop", None, None, None, None, false, false),
            ("read", Some("id"), None, Some(0), None, false, false),
            ("read", Some("id"), None, None, Some("1"), true, false),
            ("snapshot", None, Some(10.), None, None, false, false),
        ] {
            assert_eq!(
                Parameters {
                    action: action.into(),
                    frames: None,
                    capture_id: capture.map(str::to_owned),
                    seconds,
                    page,
                    player: player.map(str::to_owned),
                    server
                }
                .validate()
                .is_ok(),
                valid
            );
        }
    }
}
