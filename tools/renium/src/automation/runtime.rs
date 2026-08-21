use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::app::output::ensure_plugin_api_ok;
use crate::automation::context as bound_context;
use crate::automation::local;
use crate::automation::places;
use crate::automation::studio_args;
use crate::automation::{self, op};
use crate::bytecode::explorer::bytecode_explorer_batch_result;
use crate::cli::{
    ApplyEditorDeleteArgs, ApplyEditorPropertyArgs, BridgeConnectionArgs,
    BytecodeExplorerBatchArgs, BytecodeFileArgs, EditorMutationArgs, ExportSnapshotsArgs,
    ProjectSourceArgs, PushEditorChangesArgs,
};
use crate::daemon::transport::{BoundedLineRead, MAX_DAEMON_LINE_BYTES, read_bounded_line};
#[cfg(any(windows, target_os = "macos"))]
use crate::editor::review::{
    local_place_path_for_bridge, local_place_path_for_pid, studio_pid_for_bridge,
};
use crate::editor::sync::{
    apply_editor_delete_with_warm_bridge, apply_editor_property_with_warm_bridge,
    push_editor_changes_with_warm_bridge,
};
use crate::project::workflows;
use crate::snapshot::export::{PublishedProjectChanges, export_snapshots_with_warm_bridge};
use crate::snapshot::import::parse_services;
use crate::studio::automation::{
    click_result, editor_review_decision_result, execute_luau_result, get_console_output_result,
    goto_result, input_result, key_result, press_result, record_end_result, record_start_result,
    shot_result, start_stop_play_result, studio_change_state_result, studio_device_result,
    timed_test_result, type_result, ui_result, wait_until_result,
};
use crate::studio::bridge::{BridgeServer, BridgeTarget, DEFAULT_EXPORT_CHUNK_SIZE};
#[cfg(any(windows, target_os = "macos"))]
use crate::studio::input as input_inject;

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

pub(super) fn automation_failure(error: anyhow::Error) -> automation::Failure {
    automation_failure_ref(&error)
}

pub(super) fn automation_failure_ref(error: &anyhow::Error) -> automation::Failure {
    if let Some(verification) =
        error.downcast_ref::<crate::editor::sync::EditorSourceVerificationError>()
    {
        return automation::Failure::new("conflict", verification.to_string(), false, "context")
            .detail(json!({ "sourceVerifyErrors": verification.details }));
    }
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
        || lower.contains("pinned studio runtime disconnected")
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
    if lower.contains("conflict")
        || lower.contains("changed while")
        || lower.contains("no editor revert history")
        || lower.starts_with("stop play before ")
    {
        return automation::Failure::new("conflict", message, false, "context");
    }
    if lower.contains("need a close choice") {
        return automation::Failure::new("rejected", message, false, "update-studios");
    }
    if lower.contains("invalid")
        || lower.contains("requires")
        || lower.contains("expected")
        || lower.starts_with("provide ")
        || lower.contains("cannot be combined")
    {
        return automation::Failure::new("bad_req", message, false, "context");
    }
    automation::Failure::new("internal", message, false, "context")
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

pub(super) fn automation_pull_args(
    context: &automation::BoundContext,
    parameters: &Value,
    import: bool,
) -> Result<ExportSnapshotsArgs> {
    let object = parameters.as_object().context("p must be an object")?;
    Ok(ExportSnapshotsArgs {
        project_root: PathBuf::from(&context.root),
        src_dir: automation_string(object, "srcDir")
            .map(PathBuf::from)
            .map_or_else(|| bound_context::source_dir(context), Ok)?,
        snapshot_dir: bound_context::path(
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

pub(super) fn automation_push_args(
    context: &automation::BoundContext,
    parameters: &Value,
    reviewed: bool,
) -> Result<PushEditorChangesArgs> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut args = PushEditorChangesArgs::new(
        ProjectSourceArgs {
            project_root: PathBuf::from(&context.root),
            src_root: automation_string(object, "srcDir")
                .map(PathBuf::from)
                .map_or_else(|| bound_context::source_dir(context), Ok)?,
        },
        automation_bridge(object, 2.0)?,
    );
    args.changed_paths = automation_strings(object, "changedPaths")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    args.changed_paths_files = automation_strings(object, "changedPathsFiles")
        .into_iter()
        .map(PathBuf::from)
        .map(|path| bound_context::path(context, path))
        .collect();
    args.target_settings_ids = automation_strings(object, "targetSettingsIds");
    args.target_settings_id_files = automation_strings(object, "targetSettingsIdFiles")
        .into_iter()
        .map(PathBuf::from)
        .map(|path| bound_context::path(context, path))
        .collect();
    args.target_properties = automation_strings(object, "targetProperties");
    args.probe_events = automation_bool(object, "probeEvents", false)?;
    args.verify_sources = automation_bool(object, "verifySources", false)?;
    args.upsert_instances_only = automation_bool(object, "upsertInstancesOnly", false)?;
    args.override_packages = automation_bool(object, "overridePackages", false)?;
    args.link_cache_dir = automation_string(object, "linkCacheDir")
        .map(PathBuf::from)
        .map(|path| bound_context::path(context, path));
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
    let source_dir = automation_string(object, "srcDir")
        .map(PathBuf::from)
        .map_or_else(|| bound_context::source_dir(context), Ok)?;
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
    let source_dir = automation_string(object, "srcDir")
        .map(PathBuf::from)
        .map_or_else(|| bound_context::source_dir(context), Ok)?;
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

fn automation_requires_runtime(operation: u16, parameters: &Value) -> bool {
    automation::opcode_by_id(operation).is_ok_and(|operation| operation.runtime)
        || matches!(operation, op::SET_PROPERTY | op::REMOVE)
            && parameters.get("editor").and_then(Value::as_bool) == Some(true)
}

fn pending_change_ack(bridge: &BridgeServer, services: &[String]) -> Result<Option<(u64, String)>> {
    let state = bridge.call(
        "getStudioChangeState",
        json!({
            "services": services,
            "start": false,
        }),
    )?;
    ensure_plugin_api_ok(&state)?;
    let has_pending = ["dirtyServices", "propertyChanges", "changes"]
        .iter()
        .any(|key| {
            state[*key]
                .as_array()
                .is_some_and(|values| !values.is_empty())
        });
    if !has_pending {
        return Ok(None);
    }
    let seq = state["seq"]
        .as_u64()
        .context("Studio change state did not include seq")?;
    let runtime_id = state["runtimeId"]
        .as_str()
        .context("Studio change state did not include runtimeId")?
        .to_string();
    Ok(Some((seq, runtime_id)))
}

pub(super) fn acknowledge_pulled_changes(
    bridge: &BridgeServer,
    services: &[String],
    seq: u64,
    runtime_id: &str,
) -> Result<()> {
    let result = bridge.call(
        "getStudioChangeState",
        json!({
            "services": services,
            "start": false,
            "ackSeq": seq,
            "runtimeId": runtime_id,
        }),
    )?;
    ensure_plugin_api_ok(&result)
}

fn compact_push_summary(summary: &Map<String, Value>, parameters: &Value) -> Map<String, Value> {
    let mut result = Map::new();
    result.insert(
        "ok".to_string(),
        summary.get("ok").cloned().unwrap_or(Value::Bool(true)),
    );
    result.insert(
        "direction".to_string(),
        Value::String("files-to-studio".to_string()),
    );
    let filtered = push_is_filtered(parameters);
    result.insert(
        "selection".to_string(),
        Value::String(if filtered { "filtered" } else { "full" }.to_string()),
    );
    for key in [
        "changedPaths",
        "targetSettingsIds",
        "targetProperties",
        "skippedByReview",
        "sourceVerified",
        "sourceVerifyFailed",
        "sourceVerifyErrors",
        "protectedWrites",
        "protectedApplied",
    ] {
        let value = parameters.get(key).or_else(|| summary.get(key));
        if let Some(value) = value.filter(|value| automation_value_is_non_empty(value)) {
            result.insert(key.to_string(), value.clone());
        }
    }
    result
}

fn automation_value_is_non_empty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn push_is_filtered(parameters: &Value) -> bool {
    [
        "changedPaths",
        "changedPathsFiles",
        "targetSettingsIds",
        "targetSettingsIdFiles",
        "targetProperties",
    ]
    .into_iter()
    .any(|key| {
        parameters
            .get(key)
            .is_some_and(automation_value_is_non_empty)
    }) || parameters
        .get("upsertInstancesOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn automation_pull_operation(
    context: &automation::BoundContext,
    parameters: &Value,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
) -> Result<(Value, PublishedProjectChanges)> {
    let target = BridgeTarget::Main;
    bridge.wait_for_target(bridge_wait_seconds, target)?;
    let info = bridge.cached_bridge_info_for_target(target)?;
    let args = automation_pull_args(context, parameters, true)?;
    let services = args.services.clone();
    let parsed_services = parse_services(&services)?;
    let pending_ack = pending_change_ack(bridge, &parsed_services)?;
    let acknowledged_pending = pending_ack.is_some();
    let published = export_snapshots_with_warm_bridge(args, bridge, &info, 0.0, false)?;
    if let Some((seq, runtime_id)) = pending_ack {
        acknowledge_pulled_changes(bridge, &parsed_services, seq, &runtime_id)?;
    }
    Ok((
        json!({
            "direction": "studio-to-files",
            "services": parsed_services,
            "pendingChangesAcknowledged": acknowledged_pending,
        }),
        published,
    ))
}

#[cfg(any(windows, target_os = "macos"))]
#[derive(Clone)]
struct ConnectedStudio {
    pid: u32,
    runtime_id: String,
    local: bool,
    target: automation::StudioReopenTarget,
}

#[cfg(any(windows, target_os = "macos"))]
fn connected_edit_studios(bridge: &BridgeServer) -> Vec<ConnectedStudio> {
    let mut studios = Vec::new();
    for client in bridge.list_bridge_clients() {
        if client.get("role").and_then(Value::as_str) != Some("edit") {
            continue;
        }
        let Some(runtime_id) = client.get("runtimeId").and_then(Value::as_str) else {
            continue;
        };
        let Ok(pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime_id) else {
            continue;
        };
        if studios
            .iter()
            .any(|studio: &ConnectedStudio| studio.pid == pid)
        {
            continue;
        }
        let file = local_place_path_for_pid(pid).filter(|path| path.is_file());
        let game_id = client
            .get("gameId")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0);
        let place_id = client
            .get("placeId")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0);
        studios.push(ConnectedStudio {
            pid,
            runtime_id: runtime_id.to_string(),
            local: file.is_some() || game_id.is_none() || place_id.is_none(),
            target: automation::StudioReopenTarget {
                file,
                game_id,
                place_id,
            },
        });
    }
    studios
}

#[cfg(any(windows, target_os = "macos"))]
fn studio_clients(bridge: &BridgeServer) -> Vec<Value> {
    let mut clients = bridge.list_bridge_clients();
    for client in &mut clients {
        if client.get("role").and_then(Value::as_str) != Some("edit") {
            continue;
        }
        let Some(runtime_id) = client.get("runtimeId").and_then(Value::as_str) else {
            continue;
        };
        let Ok(pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime_id) else {
            continue;
        };
        let Some(object) = client.as_object_mut() else {
            continue;
        };
        object.insert("pid".to_string(), json!(pid));
        if let Some(file) = local_place_path_for_pid(pid).filter(|path| path.is_file()) {
            object.insert("localFile".to_string(), json!(file));
        }
    }
    clients
}

#[cfg(not(any(windows, target_os = "macos")))]
fn studio_clients(bridge: &BridgeServer) -> Vec<Value> {
    bridge.list_bridge_clients()
}

#[cfg(any(windows, target_os = "macos"))]
fn save_local_studio(studio: &ConnectedStudio, bridge: &BridgeServer) -> Result<()> {
    let file = studio
        .target
        .file
        .as_deref()
        .context("The local Studio place has no saved file path")?;
    let staging = crate::system::files::sibling_temp_path(file);
    let saved = crate::studio::native::editor::write_connected_editor_place_snapshot(
        bridge,
        &studio.runtime_id,
        &staging,
        file,
    )
    .with_context(|| {
        format!(
            "Could not save the local Studio place to {}",
            file.display()
        )
    });
    if let Err(error) = saved {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) =
        crate::system::files::replace_file_with_backup(&staging, file, "saved Studio place")
    {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn prepare_studios_for_update(parameters: &Value, bridge: &BridgeServer) -> Result<Value> {
    let local_action = parameters
        .get("localAction")
        .and_then(Value::as_str)
        .unwrap_or("ask");
    if !matches!(
        local_action,
        "ask" | "leaveOpen" | "saveAndClose" | "terminate"
    ) {
        bail!("localAction must be ask, leaveOpen, saveAndClose, or terminate");
    }
    let runtime_ids = parameters
        .get("runtimeIds")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>());
    let studios = connected_edit_studios(bridge)
        .into_iter()
        .filter(|studio| {
            runtime_ids
                .as_ref()
                .is_none_or(|runtime_ids| runtime_ids.contains(&studio.runtime_id.as_str()))
        })
        .collect::<Vec<_>>();
    let local_count = studios.iter().filter(|studio| studio.local).count();
    if local_action == "ask" && local_count > 0 {
        bail!(
            "{local_count} connected local Studio place(s) need a close choice before the plugin can be updated"
        );
    }
    if local_action == "saveAndClose" {
        for studio in studios.iter().filter(|studio| studio.local) {
            if studio.target.file.is_none() {
                bail!(
                    "A connected local Studio place has no saved file path; leave it open or terminate it without saving"
                );
            }
            save_local_studio(studio, bridge)?;
        }
    }
    let mut targets: Vec<automation::StudioReopenTarget> = Vec::new();
    let mut left_open = 0usize;
    for studio in studios {
        if studio.local && local_action == "leaveOpen" {
            left_open += 1;
            continue;
        }
        if studio.local && local_action == "ask" {
            unreachable!("local Studio ask mode is rejected above");
        }
        if let Err(error) = input_inject::terminate_studio_process(studio.pid) {
            for target in &targets {
                let _ = workflows::launch_exact_studio(
                    target.file.as_deref(),
                    target.game_id,
                    target.place_id,
                );
            }
            return Err(error);
        }
        if studio.target.file.is_some()
            || studio.target.game_id.is_some() && studio.target.place_id.is_some()
        {
            targets.push(studio.target);
        }
    }
    Ok(json!({
        "reopenTargets": targets,
        "localPlaces": local_count,
        "leftOpen": left_open,
    }))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn prepare_studios_for_update(_parameters: &Value, _bridge: &BridgeServer) -> Result<Value> {
    Ok(json!({ "reopenTargets": [], "localPlaces": 0, "leftOpen": 0 }))
}

#[cfg(any(windows, target_os = "macos"))]
fn close_studio(
    context: &automation::BoundContext,
    parameters: &Value,
    state: &automation::State,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
) -> Result<Value> {
    bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let file = local_place_path_for_bridge(bridge).filter(|path| path.is_file());
    let game_id = info.game_id.filter(|id| *id > 0).or(context.game_id);
    let place_id = info.place_id.filter(|id| *id > 0).or(context.place_id);
    let local_action = parameters.get("localAction").and_then(Value::as_str);
    let unpublished = !matches!(
        (game_id, place_id),
        (Some(game), Some(place)) if game > 0 && place > 0
    );
    if file.is_none() && unpublished && local_action != Some("terminate") {
        bail!("Renium could not determine how to reopen this Studio place, so it was not closed");
    }
    let target = automation::StudioReopenTarget {
        file,
        game_id,
        place_id,
    };
    let pid = studio_pid_for_bridge(bridge)?;
    if target.file.is_some() || unpublished {
        match local_action {
            Some("saveAndClose") => save_local_studio(
                &ConnectedStudio {
                    pid,
                    runtime_id: info.runtime_id,
                    local: true,
                    target: target.clone(),
                },
                bridge,
            )?,
            Some("terminate") => {}
            _ => bail!(
                "Closing a local Studio place requires --save or --terminate so Renium never guesses whether to keep unsaved work"
            ),
        }
    }
    state.remember_studio_target(context, target.clone());
    input_inject::terminate_studio_process(pid)?;
    state.clear_context_runtime(context.id);
    Ok(json!({ "closed": true, "pid": pid, "reopenTarget": target }))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn close_studio(
    _context: &automation::BoundContext,
    _parameters: &Value,
    _state: &automation::State,
    _bridge: &BridgeServer,
    _bridge_wait_seconds: f64,
) -> Result<Value> {
    bail!("Studio close is unsupported on this platform")
}

fn open_studio(
    context: &automation::BoundContext,
    parameters: &Value,
    state: &automation::State,
) -> Result<Value> {
    if let Some(file) = parameters
        .get("file")
        .and_then(Value::as_str)
        .map(PathBuf::from)
    {
        let file = bound_context::path(context, file);
        return workflows::launch_studio(Some(&file), None);
    }
    let target = state
        .studio_target(context)
        .unwrap_or(automation::StudioReopenTarget {
            file: None,
            game_id: context.game_id,
            place_id: context.place_id,
        });
    if target.file.is_some() || target.game_id.is_some() && target.place_id.is_some() {
        return workflows::launch_exact_studio(
            target.file.as_deref(),
            target.game_id,
            target.place_id,
        );
    }
    workflows::launch_studio(None, Some(Path::new(&context.project)))
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
    let _selection = bound_context::select(context);
    bridge.clear_runtime_pins();
    match operation {
        op::PULL => automation_pull_operation(context, parameters, bridge, bridge_wait_seconds)
            .map(|(result, _)| result),
        op::EXPORT_SNAPSHOTS => {
            let target = BridgeTarget::Main;
            bridge.wait_for_target(bridge_wait_seconds, target)?;
            let info = bridge.cached_bridge_info_for_target(target)?;
            let args = automation_pull_args(context, parameters, false)?;
            export_snapshots_with_warm_bridge(args, bridge, &info, 0.0, false)?;
            Ok(json!({ "direction": "snapshots" }))
        }
        op::PUSH => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            let summary = push_editor_changes_with_warm_bridge(
                automation_push_args(context, parameters, reviewed)?,
                bridge,
            )?;
            Ok(Value::Object(compact_push_summary(&summary, parameters)))
        }
        op::LIVE_START
        | op::LIVE_STOP
        | op::LIVE_STATUS
        | op::RETRY_PENDING
        | op::DISCARD_PENDING => {
            if operation != op::LIVE_STATUS {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            }
            studio_change_state_result(studio_args::live(operation, parameters)?, bridge)
        }
        op::SET_PROPERTY if parameters.get("editor").and_then(Value::as_bool) == Some(true) => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            Ok(Value::Object(apply_editor_property_with_warm_bridge(
                automation_editor_property_args(context, parameters, reviewed)?,
                bridge,
            )?))
        }
        op::REMOVE if parameters.get("editor").and_then(Value::as_bool) == Some(true) => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            Ok(Value::Object(apply_editor_delete_with_warm_bridge(
                automation_editor_delete_args(context, parameters)?,
                bridge,
            )?))
        }
        op::MULTI_EDIT => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
            let result =
                bridge.call_for_target("multiEdit", parameters.clone(), BridgeTarget::Edit)?;
            ensure_plugin_api_ok(&result)?;
            Ok(result)
        }
        op::FIND
        | op::TREE
        | op::INSPECT
        | op::GET_PROPERTY
        | op::SET_PROPERTY
        | op::SET_SOURCE
        | op::ADD
        | op::CLONE
        | op::MOVE
        | op::REMOVE
        | op::REVERT
        | op::IMPORT_MODEL
        | op::EXPORT_MODEL
        | op::EXPORT_PLACE
        | op::IMPORT_SNAPSHOTS
        | op::SOURCEMAP
        | op::PROJECT_INIT
        | op::PROJECT_VALIDATE => local::execute(operation, context, parameters),
        op::BATCH => automation_batch(context, parameters),
        op::STUDIO_STATUS => {
            let clients = bridge.list_bridge_clients();
            let mut result = json!({
                "studios": bound_context::studio_candidates_from(
                    &clients,
                    if parameters.get("all").and_then(Value::as_bool) == Some(true) { "" } else { &context.selector },
                ),
                "clients": bound_context::context_clients(clients, context),
                "selected": context.runtime_id,
            });
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
            result["playState"] = json!(if clients
                .iter()
                .any(|client| client["role"] == "play-server" || client["role"] == "play-client")
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
            Ok(result)
        }
        op::LUAU => {
            let parsed = studio_args::luau(Path::new(&context.root), parameters)?;
            if parsed.player.is_none() {
                let target = BridgeTarget::main_or_client(parsed.client);
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            execute_luau_result(parsed, bridge)
        }
        op::CONSOLE => {
            let parsed = studio_args::console(parameters)?;
            if parsed.player.is_none() {
                let target = BridgeTarget::main_or_client(parsed.client);
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            get_console_output_result(&parsed, bridge)
        }
        op::PLAY_START if parameters.get("test").and_then(Value::as_bool) == Some(true) => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Main)?;
            timed_test_result(studio_args::test(parameters)?, bridge)
        }
        op::PLAY_START | op::PLAY_STOP => {
            start_stop_play_result(studio_args::play(operation, parameters)?, bridge)
        }
        op::SHOT => {
            let parsed = studio_args::shot(Path::new(&context.root), parameters)?;
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
        op::DEVICE => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
            studio_device_result(&studio_args::device(parameters)?, bridge)
        }
        op::UI => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            ui_result(&studio_args::ui(parameters)?, bridge)
        }
        op::PRESS => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            press_result(&studio_args::press(parameters)?, bridge)
        }
        op::CLICK => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            click_result(&studio_args::click(parameters)?, bridge)
        }
        op::KEY => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            key_result(&studio_args::key(parameters)?, bridge)
        }
        op::TYPE => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            type_result(&studio_args::type_text(parameters)?, bridge)
        }
        op::WAIT => {
            let parsed = studio_args::wait(parameters)?;
            if parsed.player.is_none() {
                let target = BridgeTarget::main_or_client(parsed.client);
                bridge.wait_for_target(bridge_wait_seconds, target)?;
            }
            wait_until_result(&parsed, bridge)
        }
        op::GOTO => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            goto_result(&studio_args::goto(parameters)?, bridge)
        }
        op::INPUT => {
            if parameters.get("player").is_none() {
                bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Client)?;
            }
            input_result(parameters, bridge)
        }
        op::RECORD_START => {
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
        op::RECORD_END => record_end_result(parameters),
        op::ASSET_INSERT | op::GENERATE_MODEL | op::JOB_STATUS => {
            bridge.wait_for_target(bridge_wait_seconds, BridgeTarget::Edit)?;
            let method = match operation {
                op::ASSET_INSERT => "insertAsset",
                op::GENERATE_MODEL => "generateModel",
                op::JOB_STATUS => "creatorJob",
                _ => unreachable!(),
            };
            let wait_seconds = if operation == op::JOB_STATUS {
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
                if operation != op::JOB_STATUS
                    || status != Some("running")
                    || Instant::now() >= deadline
                {
                    break Ok(result);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        op::PLACE_ADD => places::add(context, parameters),
        op::PLACE_RENAME => places::rename(context, parameters),
        op::PLACE_REORDER => places::reorder(context, parameters),
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
        if !reviewed && matches!(operation, op::PUSH | op::SET_PROPERTY) && protected_count > 0 {
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

fn automation_dispatch_managed(
    operation: u16,
    context: &automation::BoundContext,
    parameters: &Value,
    state: &automation::State,
    bridge: &BridgeServer,
    bridge_wait_seconds: f64,
    reviewed: bool,
) -> std::result::Result<Value, automation::Failure> {
    if operation == op::STUDIO_CLOSE {
        return close_studio(context, parameters, state, bridge, bridge_wait_seconds)
            .map_err(automation_failure);
    }
    if operation == op::STUDIO_OPEN {
        return open_studio(context, parameters, state).map_err(automation_failure);
    }
    let writes_project = matches!(
        operation,
        op::PULL
            | op::SET_SOURCE
            | op::ADD
            | op::CLONE
            | op::MOVE
            | op::REVERT
            | op::IMPORT_MODEL
            | op::IMPORT_SNAPSHOTS
            | op::PROJECT_INIT
            | op::PLACE_ADD
            | op::PLACE_RENAME
            | op::PLACE_REORDER
    ) || matches!(operation, op::SET_PROPERTY | op::REMOVE)
        && parameters.get("editor").and_then(Value::as_bool) != Some(true);
    let pauses_file_watcher = writes_project || operation == op::PUSH;
    if pauses_file_watcher {
        state.live_sync().pause(context.id);
    }
    let push_capture = if operation == op::PUSH {
        let changed_paths = parameters
            .get("changedPaths")
            .map(|value| match value {
                Value::Array(values) => values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect(),
                Value::String(value) => value.split(',').map(PathBuf::from).collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        let capture_paths = if push_is_filtered(parameters) {
            (!changed_paths.is_empty()).then_some(changed_paths.as_slice())
        } else {
            None
        };
        if push_is_filtered(parameters) && capture_paths.is_none() {
            None
        } else {
            match state.live_sync().capture(context, capture_paths) {
                Ok(captured) => captured,
                Err(error) => {
                    state
                        .live_sync()
                        .resume(context.id, std::iter::empty::<PathBuf>());
                    return Err(automation_failure(error));
                }
            }
        }
    } else {
        None
    };
    let mut published = None;
    let result = if operation == op::PULL {
        let first = automation_pull_operation(context, parameters, bridge, bridge_wait_seconds)
            .map_err(automation_failure);
        let pulled = match first {
            Err(failure) if failure.0.rt == 1 => {
                automation_pull_operation(context, parameters, bridge, bridge_wait_seconds)
                    .map_err(automation_failure)
            }
            result => result,
        };
        pulled.map(|(result, changes)| {
            published = Some(changes);
            result
        })
    } else {
        automation_dispatch_with_retry(
            operation,
            context,
            parameters,
            bridge,
            bridge_wait_seconds,
            reviewed,
        )
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if pauses_file_watcher {
                state
                    .live_sync()
                    .resume(context.id, std::iter::empty::<PathBuf>());
            }
            return Err(error);
        }
    };
    if result.get("ok").and_then(Value::as_bool) != Some(false) {
        let paths = result
            .get("changedPaths")
            .or_else(|| parameters.get("changedPaths"))
            .map(|value| match value {
                Value::Array(values) => values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                Value::String(value) => value.split(',').map(str::to_string).collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        if writes_project {
            if operation == op::PULL {
                let published = published
                    .context("Pull did not return published project changes")
                    .map_err(automation_failure)?;
                state
                    .live_sync()
                    .reconcile_then_resume(context.id, published)
                    .map_err(automation_failure)?;
            } else {
                state.live_sync().resume(context.id, std::iter::empty());
            }
        } else if operation == op::PUSH {
            if let Some(captured) = push_capture {
                state
                    .live_sync()
                    .rebase_then_resume(context.id, captured)
                    .map_err(automation_failure)?;
            } else {
                state.live_sync().resume(context.id, std::iter::empty());
            }
        } else if !paths.is_empty() {
            state.live_sync().settle(context.id, paths);
        }
    } else if pauses_file_watcher {
        state
            .live_sync()
            .resume(context.id, std::iter::empty::<PathBuf>());
    }
    Ok(result)
}

fn automation_live_operation(
    operation: u16,
    context: &automation::BoundContext,
    parameters: &Value,
    state: &automation::State,
    bridge: &Arc<BridgeServer>,
    bridge_wait_seconds: f64,
) -> std::result::Result<Value, automation::Failure> {
    let object = automation_object(parameters)?;
    let manage_files = automation_bool(object, "manageFiles", true).map_err(automation_failure)?;
    let files_only = automation_bool(object, "filesOnly", false).map_err(automation_failure)?;
    let files_paused = automation_bool(object, "filesPaused", false).map_err(automation_failure)?;
    let reset_files_paused =
        automation_bool(object, "resetFilesPaused", false).map_err(automation_failure)?;
    let pull_changes = object
        .get("pullChanges")
        .map(|_| automation_bool(object, "pullChanges", true).map_err(automation_failure))
        .transpose()?;
    let settle_paths = automation_strings(object, "settlePaths")
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let queue_paths = automation_strings(object, "queuePaths")
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if manage_files
        && operation == op::LIVE_STATUS
        && let Some(file_writes) = object.get("fileWrites").and_then(Value::as_str)
    {
        let daemon = match file_writes {
            "pause" => state.live_sync().pause(context.id),
            "resume" if settle_paths.is_empty() => {
                state.live_sync().resume(context.id, std::iter::empty())
            }
            "resume" => {
                let captured = match state.live_sync().capture(context, Some(&settle_paths)) {
                    Ok(Some(captured)) => captured,
                    Ok(None) => {
                        return Ok(json!({ "daemon": state.live_sync().status(context.id) }));
                    }
                    Err(error) => {
                        state
                            .live_sync()
                            .resume(context.id, std::iter::empty::<PathBuf>());
                        return Err(automation_failure(error));
                    }
                };
                state
                    .live_sync()
                    .rebase_then_resume(context.id, captured)
                    .map_err(automation_failure)?
            }
            _ => {
                return Err(automation::Failure::new(
                    "bad_req",
                    "p.fileWrites must be pause or resume",
                    false,
                    "live-status",
                ));
            }
        };
        return Ok(json!({ "daemon": daemon }));
    }
    if manage_files && operation == op::LIVE_STATUS && !settle_paths.is_empty() {
        state.live_sync().settle(context.id, settle_paths);
        return Ok(json!({ "daemon": state.live_sync().status(context.id) }));
    }
    if manage_files && operation == op::LIVE_STATUS && !queue_paths.is_empty() {
        return Ok(json!({
            "daemon": state.live_sync().queue(context.id, queue_paths)
        }));
    }

    if manage_files && matches!(operation, op::RETRY_PENDING | op::DISCARD_PENDING) {
        let plugin = {
            let _gate = bridge.acquire_request_gate();
            automation_dispatch_with_retry(
                operation,
                context,
                parameters,
                bridge,
                bridge_wait_seconds,
                false,
            )?
        };
        let daemon = if operation == op::RETRY_PENDING {
            state.live_sync().retry(context.id)
        } else {
            state
                .live_sync()
                .discard(context)
                .map_err(automation_failure)?
        };
        return Ok(merge_live_status(plugin, daemon));
    }

    if manage_files && files_only && operation == op::LIVE_STATUS {
        return Ok(json!({ "daemon": state.live_sync().status(context.id) }));
    }

    if manage_files && operation == op::LIVE_STOP {
        let plugin = {
            let _gate = bridge.acquire_request_gate();
            automation_dispatch_with_retry(
                operation,
                context,
                parameters,
                bridge,
                bridge_wait_seconds,
                false,
            )
        };
        let daemon = state.live_sync().stop(context.id);
        return plugin.map(|plugin| merge_live_status(plugin, daemon));
    }

    let daemon = match operation {
        op::LIVE_START if manage_files => {
            let plugin = {
                let _gate = bridge.acquire_request_gate();
                automation_dispatch_with_retry(
                    operation,
                    context,
                    parameters,
                    bridge,
                    bridge_wait_seconds,
                    false,
                )?
            };
            let daemon = match state.live_sync().start(
                context.clone(),
                Arc::clone(bridge),
                pull_changes,
                files_paused,
                reset_files_paused,
            ) {
                Ok(daemon) => daemon,
                Err(error) => {
                    let cleanup = {
                        let _gate = bridge.acquire_request_gate();
                        automation_dispatch_with_retry(
                            op::LIVE_STOP,
                            context,
                            parameters,
                            bridge,
                            bridge_wait_seconds,
                            false,
                        )
                    };
                    let mut failure = automation_failure(error);
                    if let Err(cleanup) = cleanup {
                        failure
                            .0
                            .m
                            .push_str("; Studio live mode also failed to stop: ");
                        failure.0.m.push_str(&cleanup.0.m);
                    }
                    return Err(failure);
                }
            };
            return Ok(merge_live_status(plugin, daemon));
        }
        _ => state.live_sync().status(context.id),
    };
    let plugin = {
        let _gate = bridge.acquire_request_gate();
        automation_dispatch_with_retry(
            operation,
            context,
            parameters,
            bridge,
            bridge_wait_seconds,
            false,
        )?
    };
    Ok(merge_live_status(plugin, daemon))
}

fn merge_live_status(plugin: Value, daemon: Value) -> Value {
    let mut result = plugin
        .as_object()
        .cloned()
        .unwrap_or_else(|| Map::from_iter([("plugin".to_string(), plugin)]));
    result.insert("daemon".to_string(), daemon);
    Value::Object(result)
}

fn automation_execute_request(
    request: &automation::Request,
    state: &automation::State,
    bridge: &Arc<BridgeServer>,
    bridge_wait_seconds: f64,
) -> std::result::Result<Value, automation::Failure> {
    let operation = request.validate()?;
    match operation.id {
        op::CAP => automation::capabilities().map_err(automation_failure),
        op::BIND => bound_context::bind(state, bridge, &request.p),
        op::STUDIOS => {
            let clients = studio_clients(bridge);
            Ok(json!({
                "studios": bound_context::studio_candidates_from(&clients, ""),
                "clients": clients,
                "selected": Value::Null,
            }))
        }
        op::UPDATE_STUDIOS => {
            prepare_studios_for_update(&request.p, bridge).map_err(automation_failure)
        }
        _ => {
            let context_id = request
                .cx
                .expect("validated requests after bind require cx");
            if operation.id == op::UNBIND {
                return Ok(json!({ "removed": state.remove_context(context_id) }));
            }
            if operation.id == op::LIVE_STATUS
                && request.p.get("filesOnly").and_then(Value::as_bool) == Some(true)
            {
                let context = state.context(context_id).ok_or_else(|| {
                    automation::Failure::new(
                        "stale_cx",
                        "Context is no longer valid",
                        false,
                        "bind",
                    )
                })?;
                return automation_live_operation(
                    operation.id,
                    &context,
                    &request.p,
                    state,
                    bridge,
                    bridge_wait_seconds,
                );
            }
            let disconnected_open = operation.id == op::STUDIO_OPEN
                || operation.id == op::REVIEW_PREPARE
                    && request.p.get("op").and_then(Value::as_u64)
                        == Some(u64::from(op::STUDIO_OPEN))
                || operation.id == op::REVIEW_APPLY
                    && request
                        .p
                        .get("reviewId")
                        .and_then(Value::as_str)
                        .and_then(|id| state.review_operation(id))
                        == Some(op::STUDIO_OPEN);
            let context = if disconnected_open {
                bound_context::resolve_project(state, context_id)?
            } else {
                bound_context::resolve(state, bridge, context_id)?
            };
            if operation.id == op::CONTEXT {
                return serde_json::to_value(context).map_err(|error| {
                    automation::Failure::new("internal", error.to_string(), false, "bind")
                });
            }
            if operation.id == op::IMAGE_STORE {
                return crate::cloud::assets::store_image(&context, &request.p);
            }
            if !context.initialized
                && !matches!(operation.id, op::PROJECT_INIT | op::PROJECT_VALIDATE)
            {
                return Err(automation::Failure::new(
                    "no_project",
                    "This bootstrap context can only initialize or validate its project",
                    false,
                    "project-init",
                ));
            }
            if matches!(
                operation.id,
                op::LIVE_START
                    | op::LIVE_STOP
                    | op::LIVE_STATUS
                    | op::RETRY_PENDING
                    | op::DISCARD_PENDING
            ) {
                return automation_live_operation(
                    operation.id,
                    &context,
                    &request.p,
                    state,
                    bridge,
                    bridge_wait_seconds,
                );
            }
            if matches!(operation.id, op::ASSET_SEARCH | op::IMAGE_UPLOAD) {
                let _selection = bound_context::select(&context);
                bridge.clear_runtime_pins();
                return if operation.id == op::ASSET_SEARCH {
                    crate::cloud::assets::search(&request.p, Some(bridge))
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
                    crate::cloud::assets::upload(
                        std::path::Path::new(&context.root),
                        &request.p,
                        Some(bridge),
                    )
                };
            }
            match operation.id {
                op::SCRIPT_SEARCH => {
                    return crate::automation::tools::script_search(&context, &request.p);
                }
                op::SCRIPT_READ => {
                    return crate::automation::tools::script_read(&context, &request.p);
                }
                op::SCRIPT_GREP => {
                    return crate::automation::tools::script_grep(&context, &request.p);
                }
                _ => {}
            }
            if operation.id == op::REVIEW_PREPARE {
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
            if operation.id == op::REVIEW_REJECT {
                let object = automation_object(&request.p)?;
                let review_id = automation_string(object, "reviewId").ok_or_else(|| {
                    automation::Failure::new(
                        "bad_req",
                        "review-reject requires p.reviewId",
                        false,
                        "review-prepare",
                    )
                })?;
                return Ok(json!({ "rejected": state.reject_review(&review_id) }));
            }
            if operation.id == op::REVIEW_APPLY {
                if request.p.get("studioDecision").and_then(Value::as_bool) == Some(true) {
                    if context.runtime_id.is_none() {
                        return Err(automation::Failure::new(
                            "no_studio",
                            "No Studio runtime is bound to this context",
                            false,
                            "studios",
                        ));
                    }
                    let _selection = bound_context::select(&context);
                    bridge.clear_runtime_pins();
                    bridge
                        .wait_for_target(bridge_wait_seconds, BridgeTarget::Main)
                        .map_err(automation_failure)?;
                    let args = studio_args::review(&request.p).map_err(automation_failure)?;
                    return editor_review_decision_result(&args, bridge)
                        .map_err(automation_failure);
                }
                let object = automation_object(&request.p)?;
                let review_id = automation_string(object, "reviewId").ok_or_else(|| {
                    automation::Failure::new(
                        "bad_req",
                        "review-apply requires p.reviewId",
                        false,
                        "review-prepare",
                    )
                })?;
                let review = state.take_review(&review_id).ok_or_else(|| {
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
                return automation_dispatch_managed(
                    review.operation,
                    &context,
                    &review.parameters,
                    state,
                    bridge,
                    bridge_wait_seconds,
                    true,
                );
            }
            let requires_review = operation.review
                && (matches!(operation.id, op::STUDIO_OPEN | op::STUDIO_CLOSE)
                    || request.p.get("destructive").and_then(Value::as_bool) == Some(true));
            if requires_review {
                return Err(automation::Failure::new(
                    "rejected",
                    "This operation requires a review receipt",
                    false,
                    "review-prepare",
                ));
            }
            automation_dispatch_managed(
                operation.id,
                &context,
                &request.p,
                state,
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
    bridge: &Arc<BridgeServer>,
    bridge_wait_seconds: f64,
) -> automation::Response {
    let started = Instant::now();
    let result = if automation::opcode_by_id(request.op).is_ok_and(|operation| !operation.queued) {
        automation_execute_request(&request, state, bridge, bridge_wait_seconds)
    } else {
        let _guard = bridge.acquire_request_gate();
        automation_execute_request(&request, state, bridge, bridge_wait_seconds)
    };
    let response = match result {
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
    };
    response.with_update(state.available_update())
}

pub(crate) fn automation_parse_response(
    text: &str,
    state: &automation::State,
    bridge: &Arc<BridgeServer>,
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
    bridge: &Arc<BridgeServer>,
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
