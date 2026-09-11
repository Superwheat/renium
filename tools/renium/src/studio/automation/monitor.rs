use super::*;
use anyhow::ensure;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    output_path: Option<PathBuf>,
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
        if let Some(path) = &self.output_path {
            ensure!(
                matches!(self.action.as_str(), "micro" | "micro-stop") && path.is_absolute(),
                "Only MicroProfiler capture accepts an absolute output path"
            );
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
        output_path: if micro {
            Some(std::path::absolute(args.out.as_ref().unwrap())?)
        } else {
            None
        },
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
        return print_json_output(&result, false);
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

// Edit diagnostics use an explicit bound runtime and never change place data or
// global routing. Play selectors still use the serialized selection path.
pub(crate) fn is_edit_request(parameters: &Value) -> bool {
    parameters.is_object()
        && parameters.get("player").is_none_or(Value::is_null)
        && parameters
            .get("server")
            .is_none_or(|value| value.as_bool() == Some(false))
}

pub(crate) fn result(
    parameters: &Value,
    bridge: &BridgeServer,
    wait_seconds: f64,
    edit_runtime: Option<&str>,
) -> Result<Value> {
    let mut parameters = parameters.clone();
    let object = parameters
        .as_object_mut()
        .context("Expected performance options")?;
    object.remove("bridgeWaitSeconds");
    object.remove("bridgePorts");
    let mut parameters: Parameters = serde_json::from_value(parameters)?;
    parameters.validate()?;
    // Save in the local daemon, so large captures never enter the bounded CLI
    // response line. A host path is not part of the Studio request.
    let output_path = parameters.output_path.take();
    let target = if parameters.player.is_some() {
        BridgeTarget::Client
    } else if parameters.server {
        BridgeTarget::Main
    } else {
        BridgeTarget::Edit
    };
    if edit_runtime.is_some() {
        ensure!(
            target == BridgeTarget::Edit,
            "Expected an Edit performance target"
        );
    } else if let Some(player) = &parameters.player {
        wait_for_player_bridge(bridge, player, wait_seconds)?;
    } else {
        bridge.wait_for_target(wait_seconds, target)?;
    }
    let runtime_id = match edit_runtime {
        Some(runtime) => runtime.to_owned(),
        None => {
            bridge
                .runtime_pin_for_selector(target, parameters.player.as_deref())?
                .runtime_id
        }
    };
    if parameters.server
        && !bridge.list_bridge_clients().iter().any(|client| {
            client["runtimeId"].as_str() == Some(&runtime_id)
                && client["role"].as_str() == Some(BRIDGE_ROLE_PLAY_SERVER)
        })
    {
        bail!("No play server is connected; omit --server to sample Edit mode");
    }
    let mut request = serde_json::to_value(&parameters)?;
    request["runtimeId"] = json!(runtime_id);
    let micro = matches!(parameters.action.as_str(), "micro" | "micro-stop");
    if micro {
        request["chunked"] = json!(true);
    }
    let call = |request| -> Result<Value> {
        let result = bridge.call_for_selector_runtime_with_timeout(
            "performance",
            request,
            target,
            parameters.player.as_deref(),
            Some(&runtime_id),
            Some(Duration::from_secs(3)),
        )?;
        ensure_plugin_api_ok(&result)?;
        ensure!(
            result["runtimeId"].as_str() == Some(&runtime_id),
            "Performance response came from a different runtime"
        );
        Ok(result)
    };
    let mut result = call(request)?;
    if micro && result["captureId"].is_string() {
        const CHUNK: u64 = 3 * 1024 * 1024;
        let total = result["bytes"]
            .as_u64()
            .context("Capture omitted its size")?;
        ensure!(
            (1..=128 * 1024 * 1024).contains(&total),
            "Capture size is invalid"
        );
        let id = result["captureId"].clone();
        let pages = total.div_ceil(CHUNK);
        let mut encoded = String::with_capacity(total.div_ceil(3) as usize * 4);
        for page in 1..=pages {
            let more;
            let chunk = if page == 1 {
                &result
            } else {
                more = call(
                    json!({"action":"micro-read", "runtimeId":runtime_id, "captureId":id, "page":page}),
                )?;
                &more
            };
            ensure!(
                chunk["captureId"] == id
                    && chunk["bytes"].as_u64() == Some(total)
                    && chunk["page"].as_u64() == Some(page)
                    && if page < pages {
                        chunk["nextPage"].as_u64() == Some(page + 1)
                    } else {
                        chunk["nextPage"].is_null()
                    },
                "MicroProfiler snapshot changed or omitted a page; no file was written"
            );
            let data = chunk["data"]
                .as_str()
                .context("MicroProfiler page omitted bytes")?;
            let expected = (total - (page - 1) * CHUNK).min(CHUNK).div_ceil(3) * 4;
            ensure!(
                data.len() as u64 == expected,
                "MicroProfiler snapshot page is incomplete"
            );
            encoded.push_str(data);
        }
        result["data"] = json!(encoded);
        result.as_object_mut().unwrap().remove("page");
        result.as_object_mut().unwrap().remove("nextPage");
    }
    result["pid"] = json!(bridge.studio_pid_for_runtime(target, &runtime_id)?);
    match output_path {
        Some(path) => super::microprofiler::save_capture(&path, &result),
        None => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_edit_diagnostics_bypass_the_mutation_gate() {
        for parameters in [
            json!({"action":"micro"}),
            json!({"action":"snapshot", "player":null, "server":false}),
        ] {
            assert!(is_edit_request(&parameters));
        }
        for parameters in [
            Value::Null,
            json!({"server":true}),
            json!({"server":"false"}),
            json!({"server":null}),
            json!({"player":"1"}),
            json!({"player":false}),
        ] {
            assert!(!is_edit_request(&parameters));
        }
        for parameters in [
            json!({"action":"executeLuau"}),
            json!({"action":"micro", "code":"return 1"}),
            json!({"action":"micro-start", "frames":257}),
        ] {
            let decoded = serde_json::from_value::<Parameters>(parameters);
            assert!(decoded.is_err() || decoded.unwrap().validate().is_err());
        }
    }

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
                    server,
                    output_path: None,
                }
                .validate()
                .is_ok(),
                valid
            );
        }
    }
}
