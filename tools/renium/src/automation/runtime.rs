use std::collections::HashSet;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::app::output::{capture_json_output, ensure_plugin_api_ok};
use crate::app::timing::current_millis;
use crate::automation;
use crate::bytecode::explorer::bytecode_explorer_batch_result;
use crate::cli::dispatch;
use crate::cli::{
    ApplyEditorDeleteArgs, ApplyEditorPropertyArgs, AutomationArgs, BridgeConnectionArgs,
    BytecodeExplorerBatchArgs, BytecodeFileArgs, Cli, EditorMutationArgs, EditorReviewDecisionArgs,
    ExecuteLuauArgs, ExportSnapshotsArgs, PluginConsoleOutputArgs, ProjectSourceArgs,
    PushEditorChangesArgs, ShotArgs, WaitUntilArgs,
};
use crate::daemon::daemon_control_endpoints;
use crate::daemon::transport::{
    BoundedLineRead, DAEMON_CONTROL_CONNECT_TIMEOUT, DAEMON_CONTROL_IDLE_TIMEOUT,
    DAEMON_CONTROL_QUEUE_TIMEOUT, DAEMON_CONTROL_RESPONSE_TIMEOUT, MAX_DAEMON_LINE_BYTES,
    read_bounded_line,
};
#[cfg(any(windows, target_os = "macos"))]
use crate::editor::review::studio_pid_for_bridge;
use crate::editor::sync::{
    apply_editor_delete_with_warm_bridge, apply_editor_property_with_warm_bridge,
    push_editor_changes_with_warm_bridge,
};
use crate::project::config;
use crate::project::workflows;
use crate::snapshot::export::export_snapshots_with_warm_bridge;
use crate::studio::automation::{
    click_result, editor_review_decision_result, execute_luau_result, get_console_output_result,
    goto_result, input_result, key_result, press_result, record_end_result, record_start_result,
    shot_result, start_stop_play_result, studio_change_state_result, studio_device_result,
    timed_test_result, type_result, ui_result, wait_until_result,
};
use crate::studio::bridge::{
    BRIDGE_ROLE_EDIT, BridgeInfoPayload, BridgeServer, BridgeTarget, DEFAULT_EXPORT_CHUNK_SIZE,
};
#[cfg(any(windows, target_os = "macos"))]
use crate::studio::input as input_inject;
use crate::studio::target::{place_matches, set_place_filter};
use crate::system::files::{
    absolutize_for_daemon, atomic_write_file, canonical_path, read_json_file,
};

fn parse_daemon_request_args<T: Parser>(command: &str, request_args: &[String]) -> Result<T> {
    let mut argv = Vec::with_capacity(request_args.len() + 1);
    argv.push(command.to_string());
    argv.extend(request_args.iter().cloned());
    T::try_parse_from(argv).map_err(|error| {
        let rendered = error.to_string();
        let message = rendered
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("Invalid operation parameters")
            .trim_start_matches("error: ");
        anyhow::anyhow!(message.to_string())
    })
}

fn automation_transport_failure(id: u64, error: anyhow::Error) -> automation::Response {
    automation::Response::failure(
        id,
        Instant::now(),
        automation::Failure::new("bridge_off", format!("{error:#}"), true, "bind"),
    )
}

pub(crate) fn automation_command(args: AutomationArgs) {
    let operation = match automation::opcode_by_name(args.operation.trim()) {
        Ok(operation) => operation,
        Err(error) => automation_cli_failure("bad_op", format!("{error:#}"), "cap"),
    };
    let (cx, parameters) = match automation_cli_parameters(operation, &args.args) {
        Ok(parsed) => parsed,
        Err(error) => {
            automation_cli_failure("bad_req", format!("{error:#}"), operation.name.as_str())
        }
    };
    let request = automation::Request {
        v: automation::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: operation.id,
        cx,
        p: parameters,
    };
    let reviewed = operation.review
        && request.cx.is_some()
        && (matches!(operation.id, 52 | 53)
            || request.p.get("destructive").and_then(Value::as_bool) == Some(true));
    let mut response = if reviewed {
        send_reviewed_automation_request(&request)
    } else {
        send_automation_control_request(&request)
    }
    .unwrap_or_else(|error| automation_transport_failure(request.id, error));
    if !reviewed
        && operation.review
        && response
            .e
            .as_ref()
            .is_some_and(|error| error.c == "rejected" && error.n == "review-prepare")
    {
        response = send_reviewed_automation_request(&request)
            .unwrap_or_else(|error| automation_transport_failure(request.id, error));
    }
    print_automation_response(&response);
    if response.ok == 0 {
        std::process::exit(1);
    }
}

fn send_reviewed_automation_request(request: &automation::Request) -> Result<automation::Response> {
    let prepared = send_automation_control_request(&automation::Request {
        v: automation::PROTOCOL_VERSION,
        id: request.id,
        op: 80,
        cx: request.cx,
        p: json!({ "op": request.op, "p": request.p }),
    })?;
    if prepared.ok == 0 {
        return Ok(prepared);
    }
    let review_id = prepared
        .r
        .as_ref()
        .and_then(|value| value.get("reviewId"))
        .and_then(Value::as_str)
        .context("review-prepare did not return reviewId")?;
    send_automation_control_request(&automation::Request {
        v: automation::PROTOCOL_VERSION,
        id: request.id,
        op: 81,
        cx: request.cx,
        p: json!({ "reviewId": review_id }),
    })
}

fn automation_cli_failure(code: &str, message: String, next: &str) -> ! {
    let response = automation::Response::failure(
        0,
        Instant::now(),
        automation::Failure::new(code, message, false, next),
    );
    print_automation_response(&response);
    std::process::exit(1);
}

fn print_automation_response(response: &automation::Response) {
    std::println!(
        "{}",
        serde_json::to_string(response)
            .unwrap_or_else(|_| "{\"v\":1,\"id\":0,\"ok\":0}".to_string())
    );
}

fn read_automation_json(source: &str) -> Result<Value> {
    let text = if source == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(source).with_context(|| format!("Failed to read {source}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("Invalid JSON in {source}"))
}

fn normalize_automation_bind_payload(mut payload: Value) -> Result<Value> {
    let object = payload
        .as_object_mut()
        .context("bind payload must be a JSON object")?;
    let root = object.get("root").and_then(Value::as_str).unwrap_or(".");
    object.insert(
        "root".to_string(),
        Value::String(absolutize_for_daemon(Path::new(root)).display().to_string()),
    );
    Ok(payload)
}

fn automation_cli_parameters(
    operation: &automation::Opcode,
    args: &[String],
) -> Result<(Option<u64>, Value)> {
    if operation.id == 0 {
        if !args.is_empty() {
            bail!("Expected: rbx a cap");
        }
        return Ok((None, json!({})));
    }
    if operation.id == 1 {
        if args
            .first()
            .is_some_and(|value| value == "-J" || value == "--json-file")
        {
            if args.len() != 2 {
                bail!("Expected: rbx a bind -J FILE");
            }
            return Ok((
                None,
                normalize_automation_bind_payload(read_automation_json(&args[1])?)?,
            ));
        }
        if args.len() > 2 {
            bail!("Expected: rbx a bind [project] [place]");
        }
        return Ok((
            None,
            normalize_automation_bind_payload(json!({
                "root": args.first().map_or(".", String::as_str),
                "place": args.get(1),
            }))?,
        ));
    }
    let cx = args
        .first()
        .with_context(|| format!("Expected: rbx a {} CX", operation.name))?
        .parse::<u64>()
        .with_context(|| format!("Context ID must be an integer for {}", operation.name))?;
    let remaining = &args[1..];
    if remaining.is_empty() {
        return Ok((
            Some(cx),
            if matches!(operation.id, 10 | 11) {
                json!({ "destructive": true })
            } else {
                json!({})
            },
        ));
    }
    let (service, source) = match remaining {
        [flag, source] if flag == "-J" || flag == "--json-file" => (None, source),
        [service, flag, source]
            if operation.id == 23 && (flag == "-J" || flag == "--json-file") =>
        {
            (Some(service), source)
        }
        _ if operation.id == 23 => bail!("Expected: rbx a bb CX [SERVICE] -J FILE"),
        _ => bail!("Expected: rbx a {} CX -J FILE", operation.name),
    };
    let mut payload = match read_automation_json(source)? {
        Value::Object(object) => Value::Object(object),
        Value::Array(values) if operation.id == 23 => json!({ "ops": values }),
        _ => bail!("{} payload must be a JSON object", operation.name),
    };
    if let Some(service) = service {
        payload
            .as_object_mut()
            .context("Batch payload must be an object")?
            .insert("service".to_string(), Value::String(service.clone()));
    }
    if matches!(operation.id, 10 | 11) {
        payload
            .as_object_mut()
            .context("Sync payload must be an object")?
            .insert("destructive".to_string(), Value::Bool(true));
    }
    Ok((Some(cx), payload))
}

pub(crate) fn send_automation_control_request(
    request: &automation::Request,
) -> Result<automation::Response> {
    try_send_automation_control_request(request)?.context("Renium daemon is not running")
}

pub(crate) fn try_send_automation_control_request(
    request: &automation::Request,
) -> Result<Option<automation::Response>> {
    let Some(stream) = daemon_control_endpoints().into_iter().find_map(|address| {
        TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT).ok()
    }) else {
        return Ok(None);
    };
    send_automation_control_request_on_stream(stream, request).map(Some)
}

fn send_automation_control_request_on_stream(
    mut stream: TcpStream,
    request: &automation::Request,
) -> Result<automation::Response> {
    let _ = stream.set_read_timeout(Some(DAEMON_CONTROL_RESPONSE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    writeln!(stream, "{}", serde_json::to_string(request)?)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
        BoundedLineRead::Line => {}
        BoundedLineRead::Eof => bail!("Renium daemon closed the connection before responding"),
        BoundedLineRead::TooLong => bail!("Renium daemon response exceeded the protocol limit"),
    }
    serde_json::from_str(line.trim()).context("Invalid Renium daemon response")
}

fn automation_object(
    value: &Value,
) -> std::result::Result<&Map<String, Value>, automation::Failure> {
    value
        .as_object()
        .ok_or_else(|| automation::Failure::new("bad_req", "p must be an object", false, "context"))
}

fn automation_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn automation_bool(object: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match object.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => bail!("p.{key} must be a boolean"),
    }
}

fn automation_number<T>(object: &Map<String, Value>, key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(value) = automation_string(object, key) else {
        return Ok(default);
    };
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("p.{key} has an invalid numeric value: {error}"))
}

fn automation_strings(object: &Map<String, Value>, key: &str) -> Vec<String> {
    match object.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(value)) => value.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn automation_bridge(
    object: &Map<String, Value>,
    default_wait: f64,
) -> Result<BridgeConnectionArgs> {
    let mut bridge = BridgeConnectionArgs::local(automation_number(
        object,
        "bridgeWaitSeconds",
        default_wait,
    )?);
    if let Some(ports) = automation_string(object, "bridgePorts") {
        bridge.ports = ports;
    }
    Ok(bridge)
}

fn automation_project_fingerprint(project: &Path, experience: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(project.as_os_str().to_string_lossy().as_bytes());
    if project.is_file() {
        hash.update(
            fs::read(project).with_context(|| format!("Failed to read {}", project.display()))?,
        );
    } else {
        hash.update(b"missing-project");
    }
    let manifest = experience.join("renium.experience.json");
    if manifest.is_file() {
        hash.update(
            fs::read(&manifest)
                .with_context(|| format!("Failed to read {}", manifest.display()))?,
        );
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn automation_experience_root(project_root: &Path) -> PathBuf {
    let mut current = project_root.to_path_buf();
    loop {
        if current.join("renium.experience.json").is_file() {
            return current;
        }
        if !current.pop() {
            return project_root.to_path_buf();
        }
    }
}

fn automation_manifest_identity(
    experience: &Path,
    project_root: &Path,
) -> Result<(Option<i64>, Option<i64>, Option<String>)> {
    let path = experience.join("renium.experience.json");
    if !path.is_file() {
        return Ok((None, None, None));
    }
    let value: Value = read_json_file(&path)?;
    let game_id = value.get("gameId").and_then(Value::as_i64);
    let Some(places) = value.get("places").and_then(Value::as_object) else {
        return Ok((game_id, None, None));
    };
    let wanted = canonical_path(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    for (alias, place) in places {
        let Some(root) = place.get("root").and_then(Value::as_str) else {
            continue;
        };
        let candidate =
            canonical_path(&experience.join(root)).unwrap_or_else(|_| experience.join(root));
        if candidate == wanted {
            return Ok((
                game_id,
                place.get("placeId").and_then(Value::as_i64),
                Some(alias.clone()),
            ));
        }
    }
    Ok((game_id, None, None))
}

fn automation_client_matches(entry: &Value, selector: &str) -> bool {
    if selector.trim().is_empty() {
        return true;
    }
    place_matches(
        &BridgeInfoPayload {
            runtime_id: entry
                .get("runtimeId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            place_id: entry.get("placeId").and_then(Value::as_i64),
            game_id: entry.get("gameId").and_then(Value::as_i64),
            place_name: entry
                .get("placeName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ..BridgeInfoPayload::default()
        },
        selector,
    )
}

fn automation_studio_candidates_from(clients: &[Value], selector: &str) -> Vec<Value> {
    let mut seen = HashSet::new();
    clients
        .iter()
        .filter(|entry| entry.get("role").and_then(Value::as_str) == Some(BRIDGE_ROLE_EDIT))
        .filter(|entry| automation_client_matches(entry, selector))
        .filter(|entry| {
            let id = entry
                .get("runtimeId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            !id.is_empty() && seen.insert(id.to_string())
        })
        .cloned()
        .collect()
}

fn automation_studio_candidates(bridge: &BridgeServer, selector: &str) -> Vec<Value> {
    automation_studio_candidates_from(&bridge.list_bridge_clients(), selector)
}

fn automation_context_clients(
    clients: Vec<Value>,
    context: &automation::BoundContext,
) -> Vec<Value> {
    clients
        .into_iter()
        .filter(|entry| {
            context.runtime_id.as_deref().map_or_else(
                || automation_client_matches(entry, &context.selector),
                |runtime_id| {
                    entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id)
                        || entry.get("launchEditRuntimeId").and_then(Value::as_str)
                            == Some(runtime_id)
                },
            )
        })
        .collect()
}

fn automation_bootstrap_context(
    state: &automation::State,
    root: &Path,
) -> std::result::Result<Value, automation::Failure> {
    let project_root = canonical_path(root).map_err(|error| {
        automation::Failure::new("no_project", format!("{error:#}"), false, "project-init")
    })?;
    if !project_root.is_dir() {
        return Err(automation::Failure::new(
            "no_project",
            "Bootstrap root must be an existing directory",
            false,
            "project-init",
        ));
    }
    let project_path = project_root.join(config::PROJECT_FILE_NAME);
    let fingerprint =
        automation_project_fingerprint(&project_path, &project_root).map_err(automation_failure)?;
    let context = state.insert_context(automation::BoundContext {
        id: 0,
        initialized: false,
        project: project_path.display().to_string(),
        root: project_root.display().to_string(),
        experience: project_root.display().to_string(),
        source: project_root.join("src").display().to_string(),
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint,
    });
    serde_json::to_value(context)
        .map_err(|error| automation::Failure::new("internal", error.to_string(), false, "bind"))
}

fn automation_bind(
    state: &automation::State,
    bridge: &BridgeServer,
    parameters: &Value,
) -> std::result::Result<Value, automation::Failure> {
    let object = automation_object(parameters)?;
    let root = PathBuf::from(automation_string(object, "root").unwrap_or_else(|| ".".to_string()));
    if !root.is_absolute() {
        return Err(automation::Failure::new(
            "bad_req",
            "bind p.root must be an absolute path",
            false,
            "bind",
        ));
    }
    let root = canonical_path(&root).map_err(|error| {
        automation::Failure::new("no_project", format!("{error:#}"), false, "project-init")
    })?;
    let explicit_project = automation_string(object, "project")
        .map(PathBuf::from)
        .map(|project| {
            if project.is_absolute() {
                project
            } else {
                root.join(project)
            }
        });
    let direct_project = explicit_project
        .clone()
        .unwrap_or_else(|| root.join(config::PROJECT_FILE_NAME));
    if object.get("bootstrap").and_then(Value::as_bool) == Some(true) && !direct_project.is_file() {
        return automation_bootstrap_context(state, &root);
    }
    let loaded = (|| -> Result<config::LoadedProject> {
        if explicit_project.is_some() {
            return config::load_project(explicit_project.as_deref(), Some(&root));
        }
        if let Some(loaded) = config::try_load_project(None, Some(&root))?
            && loaded.root == root
        {
            return Ok(loaded);
        }
        let project = workflows::initialize_place_root(&root, Path::new("src"))?;
        config::load_project(Some(&project), None)
    })()
    .map_err(|error| {
        automation::Failure::new("no_project", format!("{error:#}"), false, "project-init")
    })?;
    let project_root = canonical_path(&loaded.root).map_err(|error| {
        automation::Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    let project_path = canonical_path(&loaded.path).map_err(|error| {
        automation::Failure::new(
            "no_project",
            format!("{error:#}"),
            false,
            "project-validate",
        )
    })?;
    let experience = automation_experience_root(&project_root);
    let (manifest_game_id, manifest_place_id, alias) =
        automation_manifest_identity(&experience, &project_root).map_err(|error| {
            automation::Failure::new(
                "no_project",
                format!("{error:#}"),
                false,
                "project-validate",
            )
        })?;
    let requested_place =
        automation_string(object, "place").filter(|value| !value.trim().is_empty());
    let selector =
        requested_place
            .clone()
            .unwrap_or_else(|| match (manifest_game_id, manifest_place_id) {
                (Some(game_id), Some(place_id)) if game_id > 0 && place_id > 0 => {
                    format!("{game_id}:{place_id}")
                }
                (_, Some(place_id)) if place_id > 0 => place_id.to_string(),
                _ => alias.clone().unwrap_or_default(),
            });
    let requested_runtime = automation_string(object, "runtime");
    let mut candidates = automation_studio_candidates(bridge, &selector);
    if let Some(runtime) = requested_runtime.as_deref() {
        candidates.retain(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime));
    }
    if candidates.len() > 1 {
        let compact = candidates
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.get("runtimeId"),
                    "n": entry.get("placeName"),
                    "p": entry.get("placeId"),
                })
            })
            .collect::<Vec<_>>();
        return Err(automation::Failure::new(
            "ambiguous_place",
            "More than one Studio runtime matches this project",
            false,
            "studios",
        )
        .detail(json!({ "candidates": compact })));
    }
    let candidate = candidates.first();
    let runtime_id = candidate
        .and_then(|entry| entry.get("runtimeId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let plugin_build = candidate
        .and_then(|entry| entry.get("bridgeBuildUnix"))
        .and_then(Value::as_i64);
    let place_id = requested_place
        .as_deref()
        .and_then(|value| {
            value
                .rsplit_once(':')
                .map_or(value, |(_, id)| id)
                .parse::<i64>()
                .ok()
        })
        .or_else(|| {
            candidate
                .and_then(|entry| entry.get("placeId"))
                .and_then(Value::as_i64)
        })
        .or(manifest_place_id);
    let game_id = candidate
        .and_then(|entry| entry.get("gameId"))
        .and_then(Value::as_i64)
        .or(manifest_game_id);
    let fingerprint =
        automation_project_fingerprint(&project_path, &experience).map_err(|error| {
            automation::Failure::new(
                "no_project",
                format!("{error:#}"),
                false,
                "project-validate",
            )
        })?;
    let context = state.insert_context(automation::BoundContext {
        id: 0,
        initialized: true,
        project: project_path.display().to_string(),
        root: project_root.display().to_string(),
        experience: experience.display().to_string(),
        source: project_root
            .join(&loaded.project.source_root)
            .display()
            .to_string(),
        place_id,
        game_id,
        selector,
        runtime_id,
        plugin_build,
        fingerprint,
    });
    serde_json::to_value(context)
        .map_err(|error| automation::Failure::new("internal", error.to_string(), false, "bind"))
}

fn automation_context(
    state: &automation::State,
    bridge: &BridgeServer,
    id: u64,
) -> std::result::Result<automation::BoundContext, automation::Failure> {
    let context = state.context(id).ok_or_else(|| {
        automation::Failure::new("stale_cx", "Context is no longer valid", false, "bind")
    })?;
    let fingerprint =
        automation_project_fingerprint(Path::new(&context.project), Path::new(&context.experience))
            .map_err(|_| {
                automation::Failure::new("stale_cx", "Project identity changed", false, "bind")
            })?;
    if fingerprint != context.fingerprint {
        return Err(automation::Failure::new(
            "stale_cx",
            "Project identity changed",
            false,
            "bind",
        ));
    }
    if let Some(runtime_id) = context.runtime_id.as_deref() {
        let candidate = automation_studio_candidates(bridge, &context.selector)
            .into_iter()
            .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id))
            .ok_or_else(|| {
                automation::Failure::new(
                    "stale_cx",
                    "The selected Studio runtime disconnected",
                    false,
                    "bind",
                )
            })?;
        if candidate.get("bridgeBuildUnix").and_then(Value::as_i64) != context.plugin_build {
            return Err(automation::Failure::new(
                "stale_cx",
                "The selected Studio plugin build changed",
                false,
                "bind",
            ));
        }
    }
    Ok(context)
}

struct AutomationSelection;

impl Drop for AutomationSelection {
    fn drop(&mut self) {
        set_place_filter(None);
        crate::app::context::clear_automation();
    }
}

fn select_automation_context(context: &automation::BoundContext) -> AutomationSelection {
    set_place_filter((!context.selector.is_empty()).then(|| context.selector.clone()));
    crate::app::context::select_automation(
        context.runtime_id.clone(),
        PathBuf::from(&context.project),
    );
    AutomationSelection
}

fn automation_failure(error: anyhow::Error) -> automation::Failure {
    let message = format!("{error:#}")
        .lines()
        .filter(|line| !line.contains("--help"))
        .collect::<Vec<_>>()
        .join("\n");
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        let waiting_for_connection = (lower.contains("waiting for")
            && lower.contains("bridge channel"))
            || lower.contains("plugin bridge did not connect");
        return automation::Failure::new("timeout", message, waiting_for_connection, "studios");
    }
    if lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("broken pipe")
        || lower.contains("transport closed")
        || lower.contains("bridge channel closed")
    {
        return automation::Failure::new("bridge_off", message, true, "studios");
    }
    if lower.contains("no studio runtime")
        || lower.contains("no connected")
        || lower.contains("no plugin bridge")
    {
        return automation::Failure::new("no_studio", message, false, "studios");
    }
    if lower.contains("only ") && lower.contains("bridge channel") {
        return automation::Failure::new("bridge_off", message, true, "studios");
    }
    if lower.contains("multiple studio") || lower.contains("ambiguous") {
        return automation::Failure::new("ambiguous_place", message, false, "studios");
    }
    if lower.contains("unsupported") || lower.contains("does not support") {
        return automation::Failure::new("unsupported", message, false, "cap");
    }
    if lower.contains("conflict") || lower.contains("changed while") {
        return automation::Failure::new("conflict", message, false, "context");
    }
    if lower.contains("invalid")
        || lower.contains("requires")
        || lower.contains("expected")
        || lower.contains("cannot be combined")
    {
        return automation::Failure::new("bad_req", message, false, "context");
    }
    automation::Failure::new("internal", message, false, "context")
}

fn automation_payload_args(parameters: &Value) -> Result<Vec<String>> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut arguments = Vec::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "reviewId" | "op" | "p" | "service" | "bridgePorts" | "bridgeWaitSeconds"
        ) {
            continue;
        }
        let mut flag = String::from("--");
        for ch in key.chars() {
            if ch.is_ascii_uppercase() {
                flag.push('-');
                flag.push(ch.to_ascii_lowercase());
            } else {
                flag.push(ch);
            }
        }
        match value {
            Value::Bool(true) => arguments.push(flag),
            Value::Bool(false) | Value::Null => {}
            Value::Array(values) => {
                for value in values {
                    arguments.push(flag.clone());
                    arguments.push(
                        value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_string),
                    );
                }
            }
            Value::String(value) => {
                arguments.push(flag);
                arguments.push(value.clone());
            }
            Value::Number(value) => {
                arguments.push(flag);
                arguments.push(value.to_string());
            }
            Value::Object(_) => {
                arguments.push(format!("{flag}={}", serde_json::to_string(value)?));
            }
        }
    }
    Ok(arguments)
}

fn automation_local_command(
    operation: u16,
    context: &automation::BoundContext,
    parameters: &Value,
) -> Result<Value> {
    if operation == 37 && parameters.get("applyStudio").and_then(Value::as_bool) == Some(true) {
        bail!("Revert with applyStudio is unsupported; use push after reverting the files");
    }
    let command = match operation {
        20 => "find",
        21 => "tree",
        22 => "inspect",
        30 => "bg",
        31 => "bs",
        32 => "bss",
        33 => "create",
        34 => "clone",
        35 => "move",
        36 => "remove",
        37 => "rev",
        40 => "import-model",
        41 => "export-model",
        42 => "bep",
        43 => "im",
        45 => "sm",
        70 => "init",
        71 => "doctor",
        _ => bail!("Unsupported local automation opcode {operation}"),
    };
    let mut arguments = vec!["renium".to_string()];
    if operation != 70 {
        arguments.extend(["--project".to_string(), context.project.clone()]);
    }
    arguments.push(command.to_string());
    if matches!(
        operation,
        20 | 21 | 22 | 30 | 31 | 32 | 33 | 34 | 35 | 36 | 40 | 41
    ) && let Some(service) = parameters.get("service").and_then(Value::as_str)
    {
        arguments.push(service.to_string());
    }
    if operation == 70 {
        arguments.push(context.root.clone());
    } else if operation == 71 {
        arguments.extend(["--root".to_string(), context.root.clone()]);
    } else if matches!(operation, 37 | 43) {
        arguments.extend([
            "--project-root".to_string(),
            context.root.clone(),
            "--src-dir".to_string(),
            automation_source_dir(context)?.display().to_string(),
        ]);
    }
    arguments.extend(automation_local_cli_args(operation, context, parameters)?);
    let cli = Cli::try_parse_from(arguments).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    capture_json_output(|| dispatch::dispatch(cli.command, cli.project.as_deref()))
}

fn automation_local_cli_args(
    operation: u16,
    context: &automation::BoundContext,
    parameters: &Value,
) -> Result<Vec<String>> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut flags = object.clone();
    for key in [
        "service",
        "editor",
        "destructive",
        "bootstrap",
        "bridgeWaitSeconds",
        "bridgePorts",
        "root",
        "project",
        "projectRoot",
        "srcDir",
        "srcRoot",
    ] {
        flags.remove(key);
    }
    if operation == 70 {
        flags.remove("path");
    }
    let mut arguments = Vec::new();
    let path_fields: &[(&str, &str)] = match operation {
        30 | 31 => &[("settingsFile", "--settings-file")],
        32 => &[
            ("settingsFile", "--settings-file"),
            ("sourceFile", "--source-file"),
        ],
        40 => &[("model", "--model")],
        41 | 42 | 45 => &[("output", "--output")],
        _ => &[],
    };
    for (key, flag) in path_fields {
        if let Some(value) = flags.remove(*key) {
            let path = value
                .as_str()
                .with_context(|| format!("p.{key} must be a path string"))?;
            arguments.extend([
                (*flag).to_string(),
                automation_context_path(context, PathBuf::from(path))
                    .display()
                    .to_string(),
            ]);
        }
    }
    for (key, flag) in [("pathSegments", "--path"), ("pathOrdinals", "--ords")] {
        if let Some(value) = flags.remove(key) {
            arguments.push(format!("{flag}={}", serde_json::to_string(&value)?));
        }
    }
    if operation == 31
        && let Some(value) = flags.remove("value")
    {
        arguments.push(format!("--value={}", serde_json::to_string(&value)?));
    }
    if operation == 32
        && let Some(source) = flags
            .remove("source")
            .and_then(|value| value.as_str().map(str::to_string))
    {
        arguments.extend(["--str".to_string(), source]);
    }
    if operation == 35
        && let Some(target) = flags
            .remove("targetService")
            .and_then(|value| value.as_str().map(str::to_string))
    {
        arguments.extend(["--to-service".to_string(), target]);
    }
    if operation == 33 {
        for (key, flag) in [("properties", "--property"), ("attributes", "--attribute")] {
            let Some(values) = flags.remove(key) else {
                continue;
            };
            let values = values
                .as_array()
                .with_context(|| format!("p.{key} must be a string array"))?;
            for value in values {
                arguments.extend([
                    flag.to_string(),
                    value
                        .as_str()
                        .with_context(|| format!("p.{key} must contain strings"))?
                        .to_string(),
                ]);
            }
        }
    }
    if operation == 43 {
        if let Some(snapshot_dir) = flags
            .remove("snapshotDir")
            .and_then(|value| value.as_str().map(str::to_string))
        {
            arguments.extend([
                "--snapshot-dir".to_string(),
                automation_context_path(context, PathBuf::from(snapshot_dir))
                    .display()
                    .to_string(),
            ]);
        }
        if let Some(services) = flags.remove("services") {
            let services = automation_string_list(&services)
                .context("import-snapshots p.services must be a string or string array")?;
            arguments.extend(["--services".to_string(), services]);
        }
    }
    arguments.extend(automation_payload_args(&Value::Object(flags))?);
    Ok(arguments)
}

fn automation_source_dir(context: &automation::BoundContext) -> Result<PathBuf> {
    let relative = Path::new(&context.source)
        .strip_prefix(&context.root)
        .context("Bound source root is outside the project root")?;
    Ok(if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative.to_path_buf()
    })
}

fn automation_context_path(context: &automation::BoundContext, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        Path::new(&context.root).join(path)
    }
}

fn automation_string_list(value: &Value) -> Option<String> {
    value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .or_else(|| value.as_str().map(str::to_string))
}

fn automation_pull_args(
    context: &automation::BoundContext,
    parameters: &Value,
    import: bool,
) -> Result<ExportSnapshotsArgs> {
    let object = parameters.as_object().context("p must be an object")?;
    Ok(ExportSnapshotsArgs {
        project_root: PathBuf::from(&context.root),
        src_dir: automation_source_dir(context)?,
        snapshot_dir: automation_context_path(
            context,
            PathBuf::from(
                automation_string(object, "snapshotDir")
                    .unwrap_or_else(|| ".renium/snapshots".to_string()),
            ),
        ),
        services: object
            .get("services")
            .and_then(automation_string_list)
            .unwrap_or_default(),
        chunk_size: automation_number(object, "chunkSize", DEFAULT_EXPORT_CHUNK_SIZE)?,
        adaptive_seed_batch: automation_number(object, "adaptiveSeedBatch", 0)?,
        bridge: automation_bridge(object, 2.0)?,
        run_import: import,
        no_run_import: !import,
        import_mode: automation_string(object, "importMode")
            .unwrap_or_else(|| "direct".to_string()),
        source_workers: automation_number(object, "sourceWorkers", 0)?,
        instance_workers: automation_number(object, "instanceWorkers", 0)?,
        import_workers: automation_number(object, "importWorkers", 0)?,
        performance_mode: automation_string(object, "performanceMode")
            .unwrap_or_else(|| "throughput".to_string()),
        modified_default_bypass: automation_bool(object, "modifiedDefaultBypass", false)?,
        no_modified_default_bypass: automation_bool(object, "noModifiedDefaultBypass", false)?,
        no_adaptive_throttle: !automation_bool(object, "adaptiveThrottle", true)?,
        export_all_properties: automation_bool(object, "exportAllProperties", false)?,
        no_export_all_properties: automation_bool(object, "noExportAllProperties", false)?,
        quiet_timings: true,
    })
}

fn automation_push_args(
    context: &automation::BoundContext,
    parameters: &Value,
    reviewed: bool,
) -> Result<PushEditorChangesArgs> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut args = PushEditorChangesArgs::new(
        ProjectSourceArgs {
            project_root: PathBuf::from(&context.root),
            src_root: automation_source_dir(context)?,
        },
        automation_bridge(object, 2.0)?,
    );
    args.changed_paths = automation_strings(object, "changedPaths")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    args.target_settings_ids = automation_strings(object, "targetSettingsIds");
    args.target_properties = automation_strings(object, "targetProperties");
    args.verify_sources = automation_bool(object, "verifySources", false)?;
    args.upsert_instances_only = automation_bool(object, "upsertInstancesOnly", false)?;
    args.override_packages = automation_bool(object, "overridePackages", false)?;
    args.link_cache_dir = automation_string(object, "linkCacheDir")
        .map(PathBuf::from)
        .map(|path| automation_context_path(context, path));
    args.no_review = !reviewed;
    args.yes = true;
    Ok(args)
}

fn automation_editor_property_args(
    context: &automation::BoundContext,
    parameters: &Value,
    reviewed: bool,
) -> Result<ApplyEditorPropertyArgs> {
    let object = parameters.as_object().context("p must be an object")?;
    let source_dir = automation_source_dir(context)?;
    Ok(ApplyEditorPropertyArgs {
        target: automation_editor_mutation_args(context, source_dir, object, "set-property")?,
        scope: automation_string(object, "scope").unwrap_or_else(|| "property".to_string()),
        property: automation_string(object, "property")
            .context("set-property requires p.property")?,
        value_json: serde_json::to_string(object.get("value").unwrap_or(&Value::Null))?,
        no_review: !reviewed,
        yes: reviewed,
    })
}

fn automation_editor_delete_args(
    context: &automation::BoundContext,
    parameters: &Value,
) -> Result<ApplyEditorDeleteArgs> {
    let object = parameters.as_object().context("p must be an object")?;
    let source_dir = automation_source_dir(context)?;
    Ok(ApplyEditorDeleteArgs {
        target: automation_editor_mutation_args(context, source_dir, object, "remove")?,
    })
}

fn automation_editor_mutation_args(
    context: &automation::BoundContext,
    source_dir: PathBuf,
    object: &Map<String, Value>,
    operation: &str,
) -> Result<EditorMutationArgs> {
    let service = automation_string(object, "service")
        .with_context(|| format!("{operation} requires p.service"))?;
    let settings_id = automation_string(object, "settingsId");
    let mut path_segments: Vec<String> = serde_json::from_value(
        object
            .get("pathSegments")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .context("p.pathSegments must be a string array")?;
    let mut path_ordinals: Vec<usize> = serde_json::from_value(
        object
            .get("pathOrdinals")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .context("p.pathOrdinals must be an integer array")?;
    let mut class_name = automation_string(object, "className").unwrap_or_default();
    if path_segments.is_empty()
        && let Some(settings_id) = settings_id.as_deref()
    {
        let resolved = automation_batch(
            context,
            &json!({
                "service": service,
                "ops": [{ "type": "instance", "id": settings_id, "fields": "brief,ords" }]
            }),
        )?;
        let instance = resolved
            .get("rs")
            .and_then(Value::as_array)
            .and_then(|results| results.first())
            .with_context(|| format!("No project instance has settings ID {settings_id}"))?;
        path_segments = serde_json::from_value(instance["path"].clone())?;
        path_ordinals = serde_json::from_value(instance["ords"].clone())?;
        if class_name.is_empty() {
            class_name = instance
                .get("c")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
    }
    Ok(EditorMutationArgs {
        project: ProjectSourceArgs {
            project_root: PathBuf::from(&context.root),
            src_root: source_dir,
        },
        bridge: automation_bridge(object, 2.0)?,
        service,
        settings_id,
        class_name,
        path_segments_json: serde_json::to_string(&path_segments)?,
        path_ordinals_json: serde_json::to_string(&path_ordinals)?,
        override_packages: object
            .get("overridePackages")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn automation_batch(context: &automation::BoundContext, parameters: &Value) -> Result<Value> {
    let object = parameters.as_object().context("p must be an object")?;
    let service = automation_string(object, "service").context("batch requires p.service")?;
    let ops = object.get("ops").cloned().context("batch requires p.ops")?;
    let ops = if ops.is_array() {
        json!({ "ops": ops })
    } else {
        ops
    };
    bytecode_explorer_batch_result(BytecodeExplorerBatchArgs {
        input: BytecodeFileArgs::default(),
        service,
        project_root: Some(PathBuf::from(&context.root)),
        ops_json: Some(serde_json::to_string(&ops)?),
        ops_file: None,
        output: automation_string(object, "output"),
        fields: automation_string(object, "fields"),
        pretty: false,
    })
}

fn automation_bridge_args(parameters: &Value, positional: &[&str]) -> Result<Vec<String>> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut arguments = positional
        .iter()
        .filter_map(|key| automation_string(object, key))
        .collect::<Vec<_>>();
    let mut flags = object.clone();
    for key in positional {
        flags.remove(*key);
    }
    arguments.extend(automation_payload_args(&Value::Object(flags))?);
    Ok(arguments)
}

fn automation_place_alias(value: &str, place_id: i64) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if character.is_ascii_whitespace() || character == '_' {
            separator = !output.is_empty();
        }
    }
    if output.is_empty() {
        format!("place{place_id}")
    } else {
        output
    }
}

fn automation_write_experience(path: &Path, manifest: &Value) -> Result<()> {
    atomic_write_file(path, &serde_json::to_vec_pretty(manifest)?)
}

fn automation_read_experience(context: &automation::BoundContext) -> Result<(PathBuf, Value)> {
    let path = Path::new(&context.experience).join("renium.experience.json");
    let manifest = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?,
    )?;
    Ok((path, manifest))
}

fn automation_place_add(context: &automation::BoundContext, parameters: &Value) -> Result<Value> {
    let object = parameters.as_object().context("p must be an object")?;
    let place_id = object
        .get("placeId")
        .and_then(Value::as_i64)
        .context("place-add requires p.placeId")?;
    let game_id = object
        .get("gameId")
        .and_then(Value::as_i64)
        .or(context.game_id)
        .unwrap_or(0);
    let name = automation_string(object, "name").context("place-add requires p.name")?;
    let requested_alias = automation_string(object, "alias").unwrap_or_else(|| name.clone());
    let alias = automation_place_alias(&requested_alias, place_id);
    let experience = Path::new(&context.experience);
    let manifest_path = experience.join("renium.experience.json");
    let mut manifest = if manifest_path.is_file() {
        serde_json::from_slice::<Value>(&fs::read(&manifest_path)?)?
    } else {
        let current_name = Path::new(&context.root)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("main")
            .to_string();
        let current_alias = automation_place_alias(&current_name, context.place_id.unwrap_or(0));
        let current_root = experience.join("places").join(&current_alias);
        fs::create_dir_all(&current_root)?;
        for entry in [
            PathBuf::from("renium.project.jsonc"),
            PathBuf::from("renium.project.json"),
            PathBuf::from(&context.source)
                .strip_prefix(&context.root)
                .unwrap_or_else(|_| Path::new("src"))
                .to_path_buf(),
            PathBuf::from(".renium"),
            PathBuf::from("sourcemap.json"),
            PathBuf::from("renium-link.json"),
            PathBuf::from("wally.toml"),
            PathBuf::from("wally.lock"),
            PathBuf::from("Packages"),
        ] {
            let source = experience.join(&entry);
            let destination = current_root.join(&entry);
            if source.exists() {
                if destination.exists() {
                    bail!(
                        "Cannot migrate {} because {} already exists",
                        source.display(),
                        destination.display()
                    );
                }
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&source, &destination)
                    .with_context(|| format!("Failed to move {}", source.display()))?;
            }
        }
        let source_root = PathBuf::from(&context.source)
            .strip_prefix(&context.root)
            .unwrap_or_else(|_| Path::new("src"))
            .to_path_buf();
        workflows::initialize_place_root(&current_root, &source_root)?;
        json!({
            "version": 2,
            "gameId": context.game_id.unwrap_or(game_id),
            "startPlace": current_alias,
            "placeOrder": context.place_id.filter(|id| *id > 0).into_iter().collect::<Vec<_>>(),
            "places": {
                (current_alias.clone()): {
                    "placeId": context.place_id.unwrap_or(0),
                    "name": current_name,
                    "root": format!("places/{current_alias}")
                }
            }
        })
    };
    let manifest_game_id = manifest.get("gameId").and_then(Value::as_i64).unwrap_or(0);
    if manifest_game_id > 0 && game_id > 0 && manifest_game_id != game_id {
        bail!("Place gameId {game_id} does not match project gameId {manifest_game_id}");
    }
    let places = manifest
        .get_mut("places")
        .and_then(Value::as_object_mut)
        .context("Experience places must be an object")?;
    if places.contains_key(&alias)
        || places
            .values()
            .any(|place| place.get("placeId").and_then(Value::as_i64) == Some(place_id))
    {
        bail!("Place {place_id} is already configured");
    }
    let relative_root =
        automation_string(object, "root").unwrap_or_else(|| format!("places/{alias}"));
    let relative_path = Path::new(&relative_root);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("Place root must be a relative path inside the experience project");
    }
    let place_root = experience.join(&relative_root);
    if !place_root.starts_with(experience) || place_root == experience {
        bail!("Place root must stay inside the experience project");
    }
    workflows::initialize_place_root(&place_root, Path::new("src"))?;
    places.insert(
        alias.clone(),
        json!({ "placeId": place_id, "name": name, "root": relative_root.clone() }),
    );
    let order = manifest
        .get_mut("placeOrder")
        .and_then(Value::as_array_mut)
        .context("Experience placeOrder must be an array")?;
    if place_id > 0 {
        order.push(json!(place_id));
    }
    manifest["version"] = json!(2);
    if manifest_game_id == 0 && game_id > 0 {
        manifest["gameId"] = json!(game_id);
    }
    automation_write_experience(&manifest_path, &manifest)?;
    Ok(json!({ "alias": alias, "placeId": place_id, "root": relative_root }))
}

fn automation_place_rename(
    context: &automation::BoundContext,
    parameters: &Value,
) -> Result<Value> {
    let object = parameters.as_object().context("p must be an object")?;
    let place_id = object
        .get("placeId")
        .and_then(Value::as_i64)
        .context("place-rename requires p.placeId")?;
    let requested = automation_string(object, "alias").context("place-rename requires p.alias")?;
    let alias = automation_place_alias(&requested, place_id);
    let (path, mut manifest) = automation_read_experience(context)?;
    let places = manifest
        .get_mut("places")
        .and_then(Value::as_object_mut)
        .context("Experience places must be an object")?;
    if places.contains_key(&alias) {
        bail!("Place alias {alias} already exists");
    }
    let current = places
        .iter()
        .find_map(|(name, place)| {
            (place.get("placeId").and_then(Value::as_i64) == Some(place_id)).then(|| name.clone())
        })
        .context("place-rename placeId is not configured")?;
    if current == alias {
        return Ok(json!({ "alias": alias, "placeId": place_id }));
    }
    let mut place = places
        .remove(&current)
        .expect("configured place disappeared");
    let old_root = place
        .get("root")
        .and_then(Value::as_str)
        .context("Place root is missing")?
        .to_string();
    let expected_old = format!("places/{current}");
    if old_root.replace('\\', "/") == expected_old {
        let new_root = format!("places/{alias}");
        let source = Path::new(&context.experience).join(&old_root);
        let destination = Path::new(&context.experience).join(&new_root);
        if destination.exists() {
            bail!("Place root {} already exists", destination.display());
        }
        fs::rename(&source, &destination)
            .with_context(|| format!("Failed to rename {}", source.display()))?;
        place["root"] = Value::String(new_root);
    }
    places.insert(alias.clone(), place);
    if manifest.get("startPlace").and_then(Value::as_str) == Some(&current) {
        manifest["startPlace"] = Value::String(alias.clone());
    }
    automation_write_experience(&path, &manifest)?;
    Ok(json!({ "alias": alias, "placeId": place_id }))
}

fn automation_place_reorder(
    context: &automation::BoundContext,
    parameters: &Value,
) -> Result<Value> {
    let requested = parameters
        .get("order")
        .and_then(Value::as_array)
        .context("place-reorder requires p.order")?;
    let order = requested
        .iter()
        .map(|value| value.as_i64().context("p.order must contain place IDs"))
        .collect::<Result<Vec<_>>>()?;
    let (path, mut manifest) = automation_read_experience(context)?;
    let configured = manifest
        .get("places")
        .and_then(Value::as_object)
        .context("Experience places must be an object")?
        .values()
        .filter_map(|place| place.get("placeId").and_then(Value::as_i64))
        .filter(|id| *id > 0)
        .collect::<HashSet<_>>();
    let requested_set = order.iter().copied().collect::<HashSet<_>>();
    if order.len() != requested_set.len() || requested_set != configured {
        bail!("p.order must contain every configured published place ID exactly once");
    }
    manifest["placeOrder"] = serde_json::to_value(&order)?;
    automation_write_experience(&path, &manifest)?;
    Ok(json!({ "order": order }))
}

fn automation_requires_runtime(operation: u16, parameters: &Value) -> bool {
    matches!(operation, 10..=16 | 38 | 44 | 53..=68 | 92..=94)
        || matches!(operation, 31 | 36)
            && parameters.get("editor").and_then(Value::as_bool) == Some(true)
}

fn automation_dispatch_operation(
    operation: u16,
    context: &automation::BoundContext,
    parameters: &Value,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
    reviewed: bool,
) -> Result<Value> {
    if automation_requires_runtime(operation, parameters) && context.runtime_id.is_none() {
        bail!("No Studio runtime is bound to this context");
    }
    let _selection = select_automation_context(context);
    bridge.clear_runtime_pins();
    match operation {
        10 | 44 => {
            let target = BridgeTarget::Main;
            bridge.wait_for_target(bridge_wait_seconds, target)?;
            let info = bridge.cached_bridge_info_for_target(target)?;
            export_snapshots_with_warm_bridge(
                automation_pull_args(context, parameters, operation == 10)?,
                bridge,
                &info,
                0.0,
                false,
            )?;
            Ok(
                json!({ "direction": if operation == 10 { "studio-to-files" } else { "snapshots" } }),
            )
        }
        11 => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            Ok(Value::Object(push_editor_changes_with_warm_bridge(
                automation_push_args(context, parameters, reviewed)?,
                bridge,
            )?))
        }
        12..=16 => {
            if operation != 14 {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            }
            let mut arguments = automation_payload_args(parameters)?;
            match operation {
                13 => arguments.push("--stop".to_string()),
                14 => arguments.push("--no-start".to_string()),
                16 => arguments.push("--clear-pending".to_string()),
                _ => {}
            }
            studio_change_state_result(
                parse_daemon_request_args("studio-change-state", &arguments)?,
                bridge,
            )
        }
        31 if parameters.get("editor").and_then(Value::as_bool) == Some(true) => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            Ok(Value::Object(apply_editor_property_with_warm_bridge(
                automation_editor_property_args(context, parameters, reviewed)?,
                bridge,
            )?))
        }
        36 if parameters.get("editor").and_then(Value::as_bool) == Some(true) => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            Ok(Value::Object(apply_editor_delete_with_warm_bridge(
                automation_editor_delete_args(context, parameters)?,
                bridge,
            )?))
        }
        38 => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
            let result =
                bridge.call_for_target("multiEdit", parameters.clone(), BridgeTarget::Edit)?;
            ensure_plugin_api_ok(&result)?;
            Ok(result)
        }
        20 | 21 | 22 | 30..=37 | 40..=43 | 45 | 70 | 71 => {
            automation_local_command(operation, context, parameters)
        }
        52 => {
            let file = parameters
                .get("file")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .map(|file| automation_context_path(context, file));
            workflows::launch_studio(file.as_deref(), Some(Path::new(&context.project)))
        }
        53 => {
            #[cfg(any(windows, target_os = "macos"))]
            {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
                let pid = studio_pid_for_bridge(bridge)?;
                input_inject::terminate_studio_process(pid)?;
                Ok(json!({ "closed": true, "pid": pid }))
            }
            #[cfg(not(any(windows, target_os = "macos")))]
            bail!("Studio close is unsupported on this platform")
        }
        23 => automation_batch(context, parameters),
        50 | 51 => {
            let clients = bridge.list_bridge_clients();
            let mut result = json!({
                "studios": automation_studio_candidates_from(
                    &clients,
                    if parameters.get("all").and_then(Value::as_bool) == Some(true) { "" } else { &context.selector },
                ),
                "clients": automation_context_clients(clients, context),
                "selected": context.runtime_id,
            });
            if operation == 51 {
                let clients = result["clients"].as_array().cloned().unwrap_or_default();
                let mut available = Vec::new();
                let has_edit = clients.iter().any(|client| client["role"] == "edit");
                if has_edit {
                    available.push("Edit");
                }
                if clients.iter().any(|client| client["role"] == "play-server") {
                    available.push("Server");
                }
                if clients.iter().any(|client| client["role"] == "play-client") {
                    available.push("Client");
                }
                result["availableDataModels"] = json!(available);
                result["playState"] =
                    json!(if clients
                        .iter()
                        .any(|client| client["role"] == "play-server"
                            || client["role"] == "play-client")
                    {
                        "running"
                    } else {
                        "stopped"
                    });
                if has_edit
                    && let Ok(state) = bridge.call_for_selector_with_timeout(
                        "getStudioState",
                        json!({}),
                        BridgeTarget::Edit,
                        None,
                        Some(Duration::from_millis(200)),
                    )
                {
                    result["studioState"] = state;
                }
            }
            Ok(result)
        }
        54 => {
            let arguments = automation_bridge_args(parameters, &[])?;
            let mut parsed: ExecuteLuauArgs =
                parse_daemon_request_args("execute-luau", &arguments)?;
            if let Some(file) = parsed.file.take() {
                parsed.file = Some(automation_context_path(context, file));
            }
            if parsed.player.is_none() {
                let target = BridgeTarget::main_or_client(parsed.client);
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            execute_luau_result(parsed, bridge)
        }
        55 => {
            let arguments = automation_bridge_args(parameters, &[])?;
            let parsed: PluginConsoleOutputArgs =
                parse_daemon_request_args("get-console-output", &arguments)?;
            if parsed.player.is_none() {
                let target = BridgeTarget::main_or_client(parsed.client);
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            get_console_output_result(&parsed, bridge)
        }
        56 if parameters.get("test").and_then(Value::as_bool) == Some(true) => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            timed_test_result(
                parse_daemon_request_args("test", &automation_payload_args(parameters)?)?,
                bridge,
            )
        }
        56 | 57 => {
            let mut arguments = automation_payload_args(parameters)?;
            arguments.push(if operation == 56 { "--start" } else { "--stop" }.to_string());
            start_stop_play_result(
                parse_daemon_request_args("start-stop-play", &arguments)?,
                bridge,
            )
        }
        58 => {
            let mut payload = parameters.clone();
            let object = payload.as_object_mut().context("p must be an object")?;
            if !object.contains_key("output")
                && let Some(capture_id) = object
                    .remove("captureId")
                    .or_else(|| object.remove("capture_id"))
                    .and_then(|value| value.as_str().map(str::to_string))
            {
                object.insert(
                    "output".to_string(),
                    Value::String(format!("{capture_id}.png")),
                );
            }
            for (source, target) in [
                ("camera_position", "cameraPosition"),
                ("look_at_position", "lookAt"),
                ("lookAtPosition", "lookAt"),
            ] {
                if !object.contains_key(target)
                    && let Some(value) = object.remove(source)
                {
                    object.insert(target.to_string(), value);
                }
            }
            for name in ["cameraPosition", "lookAt"] {
                if let Some(values) = object.get(name).and_then(Value::as_array) {
                    if values.len() != 3 || values.iter().any(|value| !value.is_number()) {
                        bail!("shot p.{name} must contain three numbers");
                    }
                    object.insert(
                        name.to_string(),
                        Value::String(
                            values
                                .iter()
                                .map(Value::to_string)
                                .collect::<Vec<_>>()
                                .join(","),
                        ),
                    );
                }
            }
            let arguments = automation_bridge_args(&payload, &[])?;
            let mut parsed: ShotArgs = parse_daemon_request_args("shot", &arguments)?;
            parsed.output = automation_context_path(context, parsed.output);
            if parsed.player.is_none() {
                let target = if parsed.studio {
                    BridgeTarget::Edit
                } else {
                    BridgeTarget::main_or_client(parsed.client)
                };
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            shot_result(&parsed, bridge)
        }
        59 => {
            let arguments = automation_bridge_args(parameters, &["action", "device"])?;
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
            studio_device_result(
                &parse_daemon_request_args("studio-device", &arguments)?,
                bridge,
            )
        }
        60 => {
            let arguments = automation_bridge_args(parameters, &[])?;
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            ui_result(&parse_daemon_request_args("ui", &arguments)?, bridge)
        }
        61 => {
            let arguments = automation_bridge_args(parameters, &["path"])?;
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            press_result(&parse_daemon_request_args("press", &arguments)?, bridge)
        }
        62 => {
            let arguments = automation_bridge_args(parameters, &["x", "y"])?;
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            click_result(&parse_daemon_request_args("click", &arguments)?, bridge)
        }
        63 => {
            let arguments = automation_bridge_args(parameters, &["key"])?;
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            key_result(&parse_daemon_request_args("key", &arguments)?, bridge)
        }
        64 => {
            let arguments = automation_bridge_args(parameters, &["text"])?;
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            type_result(&parse_daemon_request_args("type", &arguments)?, bridge)
        }
        65 => {
            let arguments = automation_bridge_args(parameters, &["condition"])?;
            let parsed: WaitUntilArgs = parse_daemon_request_args("wait-until", &arguments)?;
            if parsed.player.is_none() {
                let target = BridgeTarget::main_or_client(parsed.client);
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            wait_until_result(&parsed, bridge)
        }
        66 => {
            let arguments = automation_bridge_args(parameters, &["target"])?;
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            goto_result(&parse_daemon_request_args("goto", &arguments)?, bridge)
        }
        67 => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            input_result(parameters, bridge)
        }
        68 => {
            let target = if parameters.get("studio").and_then(Value::as_bool) == Some(true) {
                BridgeTarget::Edit
            } else {
                BridgeTarget::main_or_client(
                    parameters.get("client").and_then(Value::as_bool) == Some(true)
                        || parameters.get("player").is_some(),
                )
            };
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            record_start_result(
                parameters,
                bridge,
                Path::new(&context.root),
                bridge_wait_seconds,
            )
        }
        69 => record_end_result(parameters),
        92..=94 => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
            let method = match operation {
                92 => "insertAsset",
                93 => "generateModel",
                94 => "creatorJob",
                _ => unreachable!(),
            };
            let wait_seconds = if operation == 94 {
                parameters
                    .get("waitSeconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            if !wait_seconds.is_finite() || !(0.0..=120.0).contains(&wait_seconds) {
                bail!("job-status waitSeconds must be from 0 through 120")
            }
            let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds);
            loop {
                let result =
                    bridge.call_for_target(method, parameters.clone(), BridgeTarget::Edit)?;
                ensure_plugin_api_ok(&result)?;
                let status = result.get("status").and_then(Value::as_str);
                if operation != 94 || status != Some("running") || Instant::now() >= deadline {
                    break Ok(result);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        72 => automation_place_add(context, parameters),
        73 => automation_place_rename(context, parameters),
        74 => automation_place_reorder(context, parameters),
        _ => bail!("Unsupported automation opcode {operation}"),
    }
}

fn automation_dispatch_with_retry(
    operation: u16,
    context: &automation::BoundContext,
    parameters: &Value,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
    reviewed: bool,
) -> std::result::Result<Value, automation::Failure> {
    let check_result = |result: Value| {
        let protected_count = result
            .get("protectedWrites")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if !reviewed && matches!(operation, 11 | 31) && protected_count > 0 {
            return Err(automation::Failure::new(
                "rejected",
                "Protected property fallback requires a review receipt",
                false,
                "review-prepare",
            )
            .detail(json!({ "protectedWrites": protected_count })));
        }
        Ok(result)
    };
    let first = automation_dispatch_operation(
        operation,
        context,
        parameters,
        bridge,
        bridge_wait_seconds,
        reviewed,
    )
    .map_err(automation_failure)
    .and_then(check_result);
    match first {
        Err(failure) if failure.0.rt == 1 => automation_dispatch_operation(
            operation,
            context,
            parameters,
            bridge,
            bridge_wait_seconds,
            reviewed,
        )
        .map_err(automation_failure)
        .and_then(check_result),
        result => result,
    }
}

fn automation_execute_request(
    request: &automation::Request,
    state: &automation::State,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
) -> std::result::Result<Value, automation::Failure> {
    let operation = request.validate()?;
    match operation.id {
        0 => automation::capabilities().map_err(automation_failure),
        1 => automation_bind(state, bridge, &request.p),
        _ => {
            let context_id = request
                .cx
                .expect("validated requests after bind require cx");
            if operation.id == 3 {
                return Ok(json!({ "removed": state.remove_context(context_id) }));
            }
            let context = automation_context(state, bridge, context_id)?;
            if operation.id == 2 {
                return serde_json::to_value(context).map_err(|error| {
                    automation::Failure::new("internal", error.to_string(), false, "bind")
                });
            }
            if operation.id == 90 {
                return crate::cloud::execute(&context, &request.p);
            }
            if operation.id == 96 {
                return crate::cloud::assets::store_image(&context, &request.p);
            }
            if operation.id == 97 {
                return crate::automation::tools::http_get(&request.p);
            }
            if !context.initialized && !matches!(operation.id, 70 | 71) {
                return Err(automation::Failure::new(
                    "no_project",
                    "This bootstrap context can only initialize or validate its project",
                    false,
                    "project-init",
                ));
            }
            if matches!(operation.id, 91 | 95) {
                let _selection = select_automation_context(&context);
                bridge.clear_runtime_pins();
                return if operation.id == 91 {
                    crate::cloud::assets::search(&request.p, bridge)
                } else if crate::cloud::assets::studio_upload(&request.p) {
                    bridge
                        .wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)
                        .map_err(automation_failure)?;
                    let result = bridge
                        .call_for_target("uploadImages", request.p.clone(), BridgeTarget::Edit)
                        .map_err(automation_failure)?;
                    ensure_plugin_api_ok(&result).map_err(automation_failure)?;
                    Ok(result)
                } else {
                    crate::cloud::assets::upload(&context, &request.p, bridge)
                };
            }
            match operation.id {
                24 => return crate::automation::tools::script_search(&context, &request.p),
                25 => return crate::automation::tools::script_read(&context, &request.p),
                26 => return crate::automation::tools::script_grep(&context, &request.p),
                _ => {}
            }
            if operation.id == 80 {
                let object = automation_object(&request.p)?;
                let target = object
                    .get("op")
                    .and_then(Value::as_u64)
                    .and_then(|value| u16::try_from(value).ok())
                    .ok_or_else(|| {
                        automation::Failure::new(
                            "bad_req",
                            "review-prepare requires numeric p.op",
                            false,
                            "cap",
                        )
                    })?;
                let target_operation = automation::opcode_by_id(target).map_err(|_| {
                    automation::Failure::new(
                        "bad_op",
                        format!("Unknown opcode {target}"),
                        false,
                        "cap",
                    )
                })?;
                if !target_operation.review {
                    return Err(automation::Failure::new(
                        "bad_req",
                        format!("{} does not require review", target_operation.name),
                        false,
                        "cap",
                    ));
                }
                let parameters = object.get("p").cloned().unwrap_or_else(|| json!({}));
                if !parameters.is_object() {
                    return Err(automation::Failure::new(
                        "bad_req",
                        "review-prepare p.p must be an object",
                        false,
                        "review-prepare",
                    ));
                }
                let review_id = state.prepare_review(&context, target, parameters);
                return Ok(
                    json!({ "reviewId": review_id, "op": target, "name": target_operation.name }),
                );
            }
            if operation.id == 82 {
                let review_id = request
                    .p
                    .get("reviewId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        automation::Failure::new(
                            "bad_req",
                            "review-reject requires p.reviewId",
                            false,
                            "review-prepare",
                        )
                    })?;
                return Ok(json!({ "rejected": state.reject_review(review_id) }));
            }
            if operation.id == 81 {
                if request.p.get("studioDecision").and_then(Value::as_bool) == Some(true) {
                    if context.runtime_id.is_none() {
                        return Err(automation::Failure::new(
                            "no_studio",
                            "No Studio runtime is bound to this context",
                            false,
                            "studios",
                        ));
                    }
                    let _selection = select_automation_context(&context);
                    bridge.clear_runtime_pins();
                    bridge
                        .wait_for_target(bridge_wait_seconds, BridgeTarget::Main)
                        .map_err(automation_failure)?;
                    let args = parse_daemon_request_args::<EditorReviewDecisionArgs>(
                        "editor-review-decision",
                        &automation_payload_args(&request.p).map_err(automation_failure)?,
                    )
                    .map_err(automation_failure)?;
                    return editor_review_decision_result(&args, bridge)
                        .map_err(automation_failure);
                }
                let review_id = request
                    .p
                    .get("reviewId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        automation::Failure::new(
                            "bad_req",
                            "review-apply requires p.reviewId",
                            false,
                            "review-prepare",
                        )
                    })?;
                let review = state.take_review(review_id).ok_or_else(|| {
                    automation::Failure::new(
                        "rejected",
                        "Review receipt is invalid or expired",
                        false,
                        "review-prepare",
                    )
                })?;
                if (&review.context_id, &review.runtime_id) != (&context.id, &context.runtime_id) {
                    return Err(automation::Failure::new(
                        "rejected",
                        "Review receipt does not match this context",
                        false,
                        "review-prepare",
                    ));
                }
                return automation_dispatch_with_retry(
                    review.operation,
                    &context,
                    &review.parameters,
                    bridge,
                    bridge_wait_seconds,
                    true,
                );
            }
            let requires_review = operation.review
                && (matches!(operation.id, 52 | 53)
                    || request.p.get("destructive").and_then(Value::as_bool) == Some(true));
            if requires_review {
                return Err(automation::Failure::new(
                    "rejected",
                    "This operation requires a review receipt",
                    false,
                    "review-prepare",
                ));
            }
            automation_dispatch_with_retry(
                operation.id,
                &context,
                &request.p,
                bridge,
                bridge_wait_seconds,
                false,
            )
        }
    }
}

fn automation_response(
    request: automation::Request,
    state: &automation::State,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
) -> automation::Response {
    let started = Instant::now();
    let result = if matches!(
        request.op,
        0 | 1 | 2 | 3 | 24 | 25 | 26 | 50 | 51 | 80 | 82 | 90 | 91 | 96 | 97
    ) {
        automation_execute_request(&request, state, bridge, bridge_wait_seconds)
    } else {
        bridge
            .acquire_request_gate(DAEMON_CONTROL_QUEUE_TIMEOUT)
            .map_err(automation_failure)
            .and_then(|_guard| {
                automation_execute_request(&request, state, bridge, bridge_wait_seconds)
            })
    };
    match result {
        Ok(result) => automation::Response::success(request.id, started, result),
        Err(failure) => {
            let response = automation::Response::failure(request.id, started, failure);
            if let Some(error) = response.e.as_ref() {
                eprintln!(
                    "[renium] daemon request failed: id={}, op={}, elapsed_ms={:.1},  error={}",
                    request.id, request.op, response.ms, error.m
                );
            }
            response
        }
    }
}

pub(crate) fn automation_parse_response(
    text: &str,
    state: &automation::State,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
) -> automation::Response {
    let started = Instant::now();
    match serde_json::from_str::<automation::Request>(text) {
        Ok(request) => automation_response(request, state, bridge, bridge_wait_seconds),
        Err(error) => automation::Response::failure(
            0,
            started,
            automation::Failure::new(
                "bad_req",
                format!("Invalid request JSON: {error}"),
                false,
                "cap",
            ),
        ),
    }
}

pub(crate) fn oversized_automation_request_response() -> automation::Response {
    automation::Response::failure(
        0,
        Instant::now(),
        automation::Failure::new(
            "bad_req",
            format!("Request exceeds {MAX_DAEMON_LINE_BYTES} bytes"),
            false,
            "cap",
        ),
    )
}

pub(crate) fn run_automation_stdio(
    bridge: &BridgeServer,
    state: &automation::State,
    bridge_wait_seconds: f64,
) -> Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
            BoundedLineRead::Eof => break,
            BoundedLineRead::Line => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let response =
                    automation_parse_response(trimmed, state, bridge, bridge_wait_seconds);
                std::println!("{}", serde_json::to_string(&response)?);
                io::stdout().flush()?;
            }
            BoundedLineRead::TooLong => {
                let response = oversized_automation_request_response();
                std::println!("{}", serde_json::to_string(&response)?);
                io::stdout().flush()?;
            }
        }
    }
    Ok(())
}
