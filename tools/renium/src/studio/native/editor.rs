use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(any(windows, target_os = "macos"))]
use std::fs;
use std::io::{self, Write};
#[cfg(any(windows, target_os = "macos"))]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::WeakDom as RbxWeakDom;
use rbx_dom_weak::types::{ContentType as RbxContentType, Ref as RbxRef, Variant as RbxVariant};
#[cfg(any(windows, target_os = "macos", test))]
use rbx_reflection::ReflectionDatabase;
use serde_json::{Map, Value, json};

use crate::bytecode::edit::instance_path_parts_key;
#[cfg(any(windows, target_os = "macos"))]
use crate::bytecode::edit::{insert_unique_rbx_path, instance_path_key};
#[cfg(any(windows, target_os = "macos"))]
use crate::cli::PushEditorChangesArgs;
#[cfg(any(windows, target_os = "macos", test))]
use crate::editor::review::is_externally_managed_editor_property;
use crate::editor::review::request_editor_push_review;
#[cfg(any(windows, target_os = "macos"))]
use crate::editor::review::{studio_pid_for_bridge, studio_title_for_bridge};
use crate::editor::types::{
    EditorBinaryExport, EditorBinaryExportGroup, EditorBinaryImport,
    EditorBinarySerializationBatch, EditorChangeSet, EditorPropertyChange, EditorSourceChange,
};
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::decode::rbx_variant_to_settings_json;
use crate::rbx::decode::{
    NativeOverlayRequest, conditional_ref_overlay_request, fetch_native_overlay_batches,
    merge_native_overlay_items, native_overlay_property_schemas, native_property_filter,
    overlay_property_names_value, rbx_properties_to_native_settings_records,
};
#[cfg(any(windows, target_os = "macos"))]
use crate::rbx::encode::collect_rbx_subtree_preorder;
use crate::rbx::encode::json_i64;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::encode::json_to_rbx_property_variant;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::encode::rbx_canonical_property_descriptor_for_serialized_name;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::encode::{rbx_model_property_descriptor, rbx_model_top_level_refs};
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::model::BytecodeModelExportRefs;
use crate::rbx::model::BytecodeModelImportRefs;
#[cfg(any(windows, target_os = "macos"))]
use crate::rbx::model::RbxPlaceFormat;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::model::rbx_dom_path_import_refs;
#[cfg(any(windows, target_os = "macos"))]
use crate::rbx::model::{RbxPlaceBuild, build_rbx_place, rbx_dom_instance_path_parts};
use crate::roblox::schema::{
    EnumValueNameMap, parse_enum_value_name_map, parse_property_schema_map,
};
use crate::snapshot::export::{log_chunk_fetch_metrics, merge_chunk_fetch_metrics};
use crate::snapshot::import::{
    fetch_script_sources, merge_script_sources, resolve_source_worker_count,
};
use crate::snapshot::types::{
    ExportedSnapshotParts, NativeConditionalOverlayFetch, NativeConditionalOverlayRequest,
    NativeOverlayFetch, NativeOverlayItem, NativeServiceFetch, NativeServiceFinishDependencies,
    NativeServiceFinishInput, NativeSettingsProperty, NativeSettingsValue, ServiceExecutionSpan,
    ServiceExportOutput, SnapshotInstance,
};
use crate::studio::bridge::{
    BridgeChunk, BridgeServer, ChunkFetchMetrics, DEFAULT_EXPORT_CHUNK_SIZE,
    MAX_BRIDGE_CHUNK_BYTES, SourceBatchMap,
};
#[cfg(any(windows, target_os = "macos"))]
use crate::system::files::sanitize_name;
use crate::system::files::{OnDrop, fnv1a_hex};
#[cfg(any(windows, target_os = "macos"))]
use crate::system::files::{
    absolutize_under, path_extension_is, resolve_project_root_if_present, service_settings_path,
};

use crate::app::timing::{
    current_millis, elapsed_ms, log_timing, log_timing_ms, verbose_timing_logs,
};
#[cfg(any(windows, target_os = "macos"))]
use crate::studio::native::serializer;

const NATIVE_SERIALIZATION_SERVICE_LIMIT: usize = 4_096;
const NATIVE_SERIALIZATION_BATCH_LIMIT: usize = 8_192;

pub(crate) fn property_change_needs_post_native_apply(change: &EditorPropertyChange) -> bool {
    change.path_segments.as_slice() == [change.service.as_str()]
        || change.path_segments.len() == 2
            && crate::roblox::services::is_engine_managed_container(
                &change.service,
                &change.class_name,
            )
}

#[cfg(any(windows, target_os = "macos"))]
fn write_rbx_place_build(
    output_path: &Path,
    build: &RbxPlaceBuild,
    format: RbxPlaceFormat,
) -> Result<()> {
    let top_level_refs = build
        .service_roots
        .iter()
        .map(|(_, referent)| *referent)
        .collect::<Vec<_>>();
    format.write(output_path, &build.dom, &top_level_refs)
}

pub(crate) fn begin_editor_binary_export(
    bridge: &BridgeServer,
    partitioned: bool,
    service_order: Option<&[String]>,
    service_filter: Option<&[String]>,
    metadata_only: bool,
) -> Result<EditorBinaryExport> {
    begin_editor_binary_export_for_runtime(
        bridge,
        partitioned,
        service_order,
        service_filter,
        metadata_only,
        None,
        true,
    )
}

fn begin_editor_binary_export_for_runtime(
    bridge: &BridgeServer,
    partitioned: bool,
    service_order: Option<&[String]>,
    service_filter: Option<&[String]>,
    metadata_only: bool,
    runtime_id: Option<&str>,
    capture_root_properties: bool,
) -> Result<EditorBinaryExport> {
    let export_id = format!("{}-{}", current_millis(), std::process::id());
    let request_native_capture =
        cfg!(windows) && partitioned && !metadata_only && !capture_root_properties;
    #[cfg(windows)]
    let mut attribute_guard = if request_native_capture && runtime_id.is_none() {
        let prepared = (|| {
            let pid = studio_pid_for_bridge(bridge)?;
            let title = studio_title_for_bridge(bridge, pid)?;
            serializer::begin_attribute_guard(
                pid,
                &title,
                service_filter.context("Native export service filter missing")?,
                Duration::from_secs(120),
            )
        })();
        match prepared {
            Ok(guard) => Some(guard),
            Err(error) => {
                crate::app::output::log_global(
                    5,
                    format_args!("[renium] using plugin attribute observation: {error:#}"),
                );
                None
            }
        }
    } else {
        None
    };
    #[cfg(windows)]
    let native_attribute_guard = attribute_guard.is_some();
    #[cfg(not(windows))]
    let native_attribute_guard = false;
    let parameters = json!({
        "exportId": &export_id,
        "partitioned": partitioned,
        "serviceOrder": service_order,
        "serviceFilter": service_filter,
        "serializationWorkers": partitioned.then_some(4),
        "metadataOnly": metadata_only,
        "nativeCapture": request_native_capture,
        "nativeAttributeGuard": native_attribute_guard,
        "profile": verbose_timing_logs(),
    });
    let begin = if let Some(runtime_id) = runtime_id {
        bridge.call_for_runtime_with_timeout(
            "beginEditorBinaryExport",
            parameters,
            crate::studio::bridge::BridgeTarget::Edit,
            runtime_id,
            None,
        )?
    } else {
        bridge.call("beginEditorBinaryExport", parameters)?
    };
    if verbose_timing_logs()
        && let Some(profile) = begin.get("profile")
    {
        println!("[renium] native editor begin profile: {profile}");
        crate::app::timing::trace_profile("Studio binary export begin", profile);
    }
    let _metadata_trace = crate::app::timing::trace_scope("native.export", "parse export metadata");
    let result = (|| -> Result<EditorBinaryExport> {
        if begin.get("supported").and_then(Value::as_bool) == Some(false) {
            let reason = begin
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Studio cannot represent this place with native serialization");
            bail!("{reason}");
        }
        let groups = serde_json::from_value::<Vec<EditorBinaryExportGroup>>(
            begin
                .get("groups")
                .cloned()
                .context("Studio native export omitted its service groups")?,
        )
        .context("Studio returned invalid native export groups")?;
        if groups.is_empty()
            || groups.iter().any(|group| {
                group.service.is_empty()
                    || group.target_path.len() != 1
                    || group.target_path[0] != group.service
                    || group.instance_count == 0
            })
        {
            bail!("Studio returned invalid native export groups");
        }
        let native_capture = begin
            .get("nativeCapture")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        anyhow::ensure!(
            !native_capture || request_native_capture,
            "Studio returned an unrequested native capture"
        );
        if !metadata_only && !native_capture {
            for group in &groups {
                native_identity_carrier_count(group)?;
            }
        }
        let serialization_batches = serde_json::from_value::<Vec<EditorBinarySerializationBatch>>(
            begin
                .get("serializationBatches")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        )
        .context("Studio returned invalid native serialization batches")?;
        anyhow::ensure!(
            !native_capture || serialization_batches.is_empty(),
            "Native capture cannot include plugin serialization jobs"
        );
        {
            let group_by_service = groups
                .iter()
                .map(|group| (group.service.as_str(), group))
                .collect::<HashMap<_, _>>();
            let mut batch_ids = HashSet::new();
            let mut batched_services = HashSet::new();
            for batch in &serialization_batches {
                if batch.id.is_empty()
                    || group_by_service.contains_key(batch.id.as_str())
                    || !batch_ids.insert(batch.id.as_str())
                    || batch.services.len() < 2
                {
                    bail!("Studio returned an invalid native serialization batch");
                }
                let mut batch_instances = 0_usize;
                let mut batch_services = HashSet::new();
                for service in &batch.services {
                    let group = group_by_service.get(service.as_str()).with_context(|| {
                        format!(
                            "Studio native serialization batch {} included unknown service {}",
                            batch.id, service
                        )
                    })?;
                    if !batch_services.insert(service.as_str())
                        || !batched_services.insert(service.as_str())
                        || group.instance_count >= NATIVE_SERIALIZATION_SERVICE_LIMIT
                    {
                        bail!("Studio returned an invalid native serialization batch");
                    }
                    batch_instances = batch_instances
                        .checked_add(group.instance_count)
                        .context("Studio native serialization batch is too large")?;
                }
                if batch_instances > NATIVE_SERIALIZATION_BATCH_LIMIT {
                    bail!("Studio returned an invalid native serialization batch");
                }
            }
            if !partitioned && !serialization_batches.is_empty() {
                bail!("Studio returned serialization batches for an unpartitioned export");
            }
        }
        let property_schema_by_class =
            parse_property_schema_map(begin.get("propertySchemaByClass"))?;
        let enum_value_names_by_type =
            parse_enum_value_name_map(begin.get("enumValueNamesByType"))?;
        if property_schema_by_class.is_empty() {
            bail!("Studio native export omitted its property schema");
        }
        #[cfg(any(windows, target_os = "macos"))]
        let groups = {
            let mut groups = groups;
            if capture_root_properties {
                capture_native_service_root_properties(bridge, runtime_id, &mut groups)?;
            }
            groups
        };
        Ok(EditorBinaryExport {
            #[cfg(windows)]
            attribute_guard: attribute_guard.take(),
            native_capture,
            #[cfg(any(windows, target_os = "macos"))]
            bytes: Vec::new(),
            groups,
            serialization_batches,
            export_id: Some(export_id.clone()),
            property_schema_by_class,
            enum_value_names_by_type,
        })
    })();
    if result.is_err() {
        let _ = bridge.call("finishEditorBinaryExport", json!({ "exportId": export_id }));
    }
    result
}

#[cfg(any(windows, target_os = "macos"))]
fn capture_native_service_root_properties(
    bridge: &BridgeServer,
    runtime_id: Option<&str>,
    groups: &mut [EditorBinaryExportGroup],
) -> Result<()> {
    let _trace = crate::app::timing::trace_scope("native.property", "capture service roots");
    use crate::editor::native_roots::{capture_properties, decode_service_property};
    use crate::studio::bridge::BridgeTarget;

    if !groups
        .iter()
        .any(|group| !capture_properties(&group.service).is_empty())
    {
        return Ok(());
    }
    let started = Instant::now();
    let info = if let Some(runtime_id) = runtime_id {
        bridge.cached_bridge_info_for_runtime(BridgeTarget::Edit, runtime_id)?
    } else {
        bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?
    };
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &info.runtime_id)?;
    // Service roots cannot enter SerializeInstancesAsync. Capture these blocked
    // saved settings through existing identity-checked reads, not a second
    // whole-place export. The active export guard still fences outside edits.
    for group in groups {
        for &name in capture_properties(&group.service) {
            let _trace = crate::app::timing::trace_scope("native.property", name);
            let text = serializer::read_property(
                pid,
                &info.place_name,
                &group.target_path,
                &[1],
                &group.service,
                name,
                Duration::from_secs(2),
            )?;
            let value = decode_service_property(&group.service, name, &text)?;
            group.root_properties.insert(name.into(), value);
        }
    }
    log_timing("native service root capture", started);
    Ok(())
}

pub(crate) fn decode_bridge_buffer(
    value: &Value,
    expected_len: usize,
    label: &str,
) -> Result<Vec<u8>> {
    let encoded = if let Some(encoded) = value.as_str() {
        let bytes =
            base64::decode(encoded).with_context(|| format!("{label} is not valid base64"))?;
        if bytes.len() != expected_len {
            bail!("{label} has {} bytes; expected {expected_len}", bytes.len());
        }
        return Ok(bytes);
    } else {
        let object = value
            .as_object()
            .with_context(|| format!("{label} is not a string or buffer"))?;
        if object.get("t").and_then(Value::as_str) != Some("buffer") {
            bail!("{label} has an invalid buffer type");
        }
        object
    };
    if let Some(raw) = encoded.get("base64").and_then(Value::as_str) {
        let bytes = base64::decode(raw).with_context(|| format!("{label} is not valid base64"))?;
        if bytes.len() != expected_len {
            bail!("{label} has {} bytes; expected {expected_len}", bytes.len());
        }
        return Ok(bytes);
    }
    let compressed = encoded
        .get("zbase64")
        .and_then(Value::as_str)
        .with_context(|| format!("{label} buffer omitted its data"))?;
    let compressed =
        base64::decode(compressed).with_context(|| format!("{label} is not valid zbase64"))?;
    let bytes = zstd::bulk::decompress(&compressed, expected_len)
        .with_context(|| format!("{label} has invalid zstd data"))?;
    if bytes.len() != expected_len {
        bail!("{label} has {} bytes; expected {expected_len}", bytes.len());
    }
    Ok(bytes)
}

fn native_binary_chunk_bytes() -> usize {
    std::env::var("RENIUM_NATIVE_CHUNK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4 * 1024 * 1024)
        .clamp(256 * 1024, 8 * 1024 * 1024)
}

const NATIVE_PAYLOAD_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const NATIVE_PAYLOAD_CACHE_MAX_ENTRIES: usize = 32;

#[derive(Default)]
struct NativePayloadCache {
    entries: VecDeque<(String, Arc<[u8]>)>,
    last_hash_by_slot: AHashMap<String, String>,
    total_bytes: usize,
}

fn native_payload_cache() -> &'static Mutex<NativePayloadCache> {
    static CACHE: OnceLock<Mutex<NativePayloadCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(NativePayloadCache::default()))
}

fn native_payload_cache_for_slot(slot: &str) -> Option<(String, Arc<[u8]>)> {
    let cache = native_payload_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let hash = cache.last_hash_by_slot.get(slot)?;
    cache
        .entries
        .iter()
        .find_map(|(entry_hash, bytes)| (entry_hash == hash).then(|| (hash.clone(), bytes.clone())))
}

fn native_payload_cache_insert(slot: String, hash: String, bytes: &[u8]) {
    if bytes.len() > NATIVE_PAYLOAD_CACHE_MAX_BYTES {
        return;
    }
    let mut cache = native_payload_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache.last_hash_by_slot.insert(slot, hash.clone());
    if cache
        .entries
        .iter()
        .any(|(entry_hash, _)| entry_hash == &hash)
    {
        return;
    }
    while cache.entries.len() >= NATIVE_PAYLOAD_CACHE_MAX_ENTRIES
        || cache.total_bytes.saturating_add(bytes.len()) > NATIVE_PAYLOAD_CACHE_MAX_BYTES
    {
        let Some((_, removed)) = cache.entries.pop_front() else {
            break;
        };
        cache.total_bytes = cache.total_bytes.saturating_sub(removed.len());
    }
    cache.total_bytes = cache.total_bytes.saturating_add(bytes.len());
    cache.entries.push_back((hash, Arc::from(bytes)));
}

fn observe_native_serialization_complete(
    chunk: &BridgeChunk,
    serialization_complete: Option<&AtomicBool>,
) {
    if chunk.serialization_complete
        && let Some(serialization_complete) = serialization_complete
    {
        serialization_complete.store(true, Ordering::Release);
    }
}

pub(crate) fn receive_editor_binary_export_bytes(
    bridge: &BridgeServer,
    export_id: &str,
    service: Option<&str>,
    serialization_complete: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    receive_editor_binary_export_bytes_for_runtime(
        bridge,
        export_id,
        service,
        serialization_complete,
        None,
    )
}

fn receive_editor_binary_export_bytes_for_runtime(
    bridge: &BridgeServer,
    export_id: &str,
    service: Option<&str>,
    serialization_complete: Option<&AtomicBool>,
    runtime_id: Option<&str>,
) -> Result<Vec<u8>> {
    let cache_slot = format!(
        "{}:{}",
        runtime_id.unwrap_or("current"),
        service.unwrap_or("full")
    );
    receive_editor_binary_export_bytes_with_cache(
        export_id,
        service,
        serialization_complete,
        &cache_slot,
        |parameters| {
            if let Some(runtime_id) = runtime_id {
                bridge.call_chunk_for_runtime(
                    "readEditorBinaryExport",
                    parameters,
                    crate::studio::bridge::BridgeTarget::Edit,
                    runtime_id,
                )
            } else {
                bridge.call_chunk("readEditorBinaryExport", parameters)
            }
        },
    )
}

fn receive_editor_binary_export_bytes_with_cache(
    export_id: &str,
    service: Option<&str>,
    serialization_complete: Option<&AtomicBool>,
    cache_slot: &str,
    read: impl Fn(Value) -> Result<BridgeChunk> + Sync,
) -> Result<Vec<u8>> {
    const MAX_EXPORT_BYTES: usize = 512 * 1024 * 1024;
    let service_label = service.map_or(String::new(), |value| format!("{value} "));
    // Retain the advertised allocation until Studio replies. Parallel service
    // exports may evict it from the bounded shared cache during this request.
    let known_payload = native_payload_cache_for_slot(cache_slot);
    let known_payload_hash = known_payload.as_ref().map(|(hash, _)| hash.as_str());
    let raw_chunk_bytes = native_binary_chunk_bytes();
    let read_started = Instant::now();
    let first_parameters = json!({
        "exportId": export_id,
        "service": service,
        "offset": 0,
        "length": raw_chunk_bytes,
        "clampLength": true,
        "waitForReady": true,
        "timeoutSeconds": 80,
        "rawBase64": true,
        "supportsPayloadCache": true,
        "knownPayloadHash": known_payload_hash,
    });
    let first = read(first_parameters)?;
    observe_native_serialization_complete(&first, serialization_complete);
    let total_bytes = first.total;
    if total_bytes == 0 || total_bytes > MAX_EXPORT_BYTES {
        bail!("Studio returned an invalid native export size");
    }
    if first.payload_cache_hit {
        let payload_hash = first
            .payload_hash
            .as_deref()
            .context("Studio native export cache hit omitted its payload hash")?;
        if known_payload_hash != Some(payload_hash) {
            bail!("Studio native export returned an unexpected payload cache hit");
        }
        let (_, bytes) =
            known_payload.context("Studio native export payload cache entry is missing")?;
        if bytes.len() != total_bytes {
            bail!("Studio native export payload cache entry has the wrong size");
        }
        log_timing(
            &format!("native editor {service_label}binary cache hit"),
            read_started,
        );
        return Ok(bytes.to_vec());
    }
    let first_length = raw_chunk_bytes.min(total_bytes);
    if first.start != 1 || first.next_start != first_length + 1 {
        bail!("Studio native export first chunk returned an invalid range");
    }
    if verbose_timing_logs() {
        println!(
            "[renium] native editor {service_label}binary payload: bytes={total_bytes}, chunk_bytes={raw_chunk_bytes}"
        );
    }
    let mut bytes = vec![0_u8; total_bytes];
    let first_decoded = base64::decode_config_slice(
        first.chunk.as_bytes(),
        base64::STANDARD,
        &mut bytes[..first_length],
    )
    .context("Studio native export first chunk is not valid base64")?;
    if first_decoded != first_length {
        bail!(
            "Studio native export first chunk has {first_decoded} bytes; expected {first_length}"
        );
    }
    bytes[first_length..]
        .par_chunks_mut(raw_chunk_bytes)
        .enumerate()
        .map(|(chunk_index, target)| -> Result<()> {
            let offset = first_length + chunk_index * raw_chunk_bytes;
            let length = target.len();
            let parameters = json!({
                "exportId": export_id,
                "service": service,
                "offset": offset,
                "length": length,
                "rawBase64": true,
            });
            let response = read(parameters)?;
            observe_native_serialization_complete(&response, serialization_complete);
            if response.start != offset + 1
                || response.next_start != offset + length + 1
                || response.total != total_bytes
            {
                bail!("Studio native export chunk returned an invalid range");
            }
            let decoded =
                base64::decode_config_slice(response.chunk.as_bytes(), base64::STANDARD, target)
                    .context("Studio native export chunk is not valid base64")?;
            if decoded != length {
                bail!("Studio native export chunk has {decoded} bytes; expected {length}");
            }
            Ok(())
        })
        .collect::<Result<()>>()?;
    if let Some(payload_hash) = first.payload_hash {
        native_payload_cache_insert(cache_slot.to_owned(), payload_hash, &bytes);
    }
    log_timing(
        &format!("native editor {service_label}binary transfer"),
        read_started,
    );
    Ok(bytes)
}

struct NativeBinaryBatchPart {
    bytes: Arc<[u8]>,
    start: usize,
    end: usize,
}

struct NativeBinaryBatches {
    parts: HashMap<String, NativeBinaryBatchPart>,
}

fn receive_editor_binary_export_batch_payload(
    bridge: &BridgeServer,
    export_id: &str,
    services: &[String],
    serialization_complete: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    const MAX_BATCH_BYTES: usize = 512 * 1024 * 1024 + 1024;
    let raw_chunk_bytes = native_binary_chunk_bytes();
    let first = bridge.call_chunk(
        "readEditorBinaryExportBatch",
        json!({
            "exportId": export_id,
            "services": services,
            "offset": 0,
            "length": raw_chunk_bytes,
            "clampLength": true,
            "timeoutSeconds": 80,
        }),
    )?;
    observe_native_serialization_complete(&first, serialization_complete);
    let total_bytes = first.total;
    if total_bytes == 0 || total_bytes > MAX_BATCH_BYTES {
        bail!("Studio returned an invalid native export batch size");
    }
    let first_length = raw_chunk_bytes.min(total_bytes);
    if first.start != 1 || first.next_start != first_length + 1 {
        bail!("Studio native export batch first chunk returned an invalid range");
    }
    let mut bytes = vec![0_u8; total_bytes];
    let decoded = base64::decode_config_slice(
        first.chunk.as_bytes(),
        base64::STANDARD,
        &mut bytes[..first_length],
    )
    .context("Studio native export batch first chunk is not valid base64")?;
    if decoded != first_length {
        bail!(
            "Studio native export batch first chunk has {decoded} bytes; expected {first_length}"
        );
    }
    bytes[first_length..]
        .par_chunks_mut(raw_chunk_bytes)
        .enumerate()
        .map(|(chunk_index, target)| -> Result<()> {
            let offset = first_length + chunk_index * raw_chunk_bytes;
            let length = target.len();
            let response = bridge.call_chunk(
                "readEditorBinaryExportBatch",
                json!({
                    "exportId": export_id,
                    "services": services,
                    "offset": offset,
                    "length": length,
                }),
            )?;
            observe_native_serialization_complete(&response, serialization_complete);
            if response.start != offset + 1
                || response.next_start != offset + length + 1
                || response.total != total_bytes
            {
                bail!("Studio native export batch chunk returned an invalid range");
            }
            let decoded =
                base64::decode_config_slice(response.chunk.as_bytes(), base64::STANDARD, target)
                    .context("Studio native export batch chunk is not valid base64")?;
            if decoded != length {
                bail!("Studio native export batch chunk has {decoded} bytes; expected {length}");
            }
            Ok(())
        })
        .collect::<Result<()>>()?;
    Ok(bytes)
}

fn receive_editor_binary_export_batches(
    bridge: &BridgeServer,
    export_id: &str,
    services: &[String],
    serialization_complete: Option<&AtomicBool>,
) -> Result<NativeBinaryBatches> {
    let started = Instant::now();
    let mut parts = HashMap::with_capacity(services.len());
    let mut service_offset = 0;
    let mut payload_bytes = 0;
    while service_offset < services.len() {
        let remaining = &services[service_offset..];
        let bytes = receive_editor_binary_export_batch_payload(
            bridge,
            export_id,
            remaining,
            serialization_complete,
        )?;
        if bytes.len() < 4 {
            bail!("Studio native export batch omitted its header");
        }
        let batch_count =
            u32::from_le_bytes(bytes[0..4].try_into().expect("four-byte batch count")) as usize;
        if batch_count == 0 || batch_count > remaining.len() {
            bail!("Studio native export batch returned an invalid service count");
        }
        let header_bytes = 4 + batch_count * 4;
        if header_bytes > bytes.len() {
            bail!("Studio native export batch has a truncated header");
        }
        let bytes = Arc::<[u8]>::from(bytes);
        let mut byte_offset = header_bytes;
        for (index, service) in remaining.iter().take(batch_count).enumerate() {
            let length_offset = 4 + index * 4;
            let length = u32::from_le_bytes(
                bytes[length_offset..length_offset + 4]
                    .try_into()
                    .expect("four-byte batch length"),
            ) as usize;
            if length == 0 {
                bail!("Studio native export batch contains an empty service payload");
            }
            let end = byte_offset
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .context("Studio native export batch contains an invalid service length")?;
            if parts
                .insert(
                    service.clone(),
                    NativeBinaryBatchPart {
                        bytes: Arc::clone(&bytes),
                        start: byte_offset,
                        end,
                    },
                )
                .is_some()
            {
                bail!("Studio native export batch duplicated a service");
            }
            payload_bytes += length;
            byte_offset = end;
        }
        if byte_offset != bytes.len() {
            bail!("Studio native export batch contains trailing data");
        }
        service_offset += batch_count;
    }
    if verbose_timing_logs() {
        println!(
            "[renium] native editor binary batch: services={}, payload_bytes={payload_bytes}",
            parts.len()
        );
    }
    log_timing("native editor batched binary transfer", started);
    Ok(NativeBinaryBatches { parts })
}

pub(crate) struct EditorBinaryExportFinishGuard<'a> {
    pub(crate) bridge: &'a BridgeServer,
    pub(crate) export_id: Option<String>,
    #[cfg(windows)]
    pub(crate) attribute_guard: Option<serializer::AttributeGuard>,
}

fn validate_native_export_finished(result: &Value) -> Result<()> {
    anyhow::ensure!(
        result.get("found").and_then(Value::as_bool) == Some(true),
        "Studio native export expired before its snapshot could be validated; retry the sync"
    );
    Ok(())
}

impl EditorBinaryExportFinishGuard<'_> {
    pub(crate) fn finish(&mut self, record_sync_completion: bool) -> Result<bool> {
        let Some(export_id) = self.export_id.as_deref() else {
            return Ok(false);
        };
        #[cfg(windows)]
        if let Some(mut guard) = self.attribute_guard.take()
            && let Err(error) = guard.finish()
        {
            let _ = self.bridge.call(
                "finishEditorBinaryExport",
                json!({"exportId":export_id, "nativeAttributeGuardFailed":true}),
            );
            self.export_id = None;
            return Err(error);
        }
        let result = self.bridge.call(
            "finishEditorBinaryExport",
            json!({
                "exportId": export_id,
                "recordSyncCompletion": record_sync_completion,
            }),
        )?;
        self.export_id = None;
        validate_native_export_finished(&result)?;
        Ok(result
            .get("syncCompletionRecorded")
            .and_then(Value::as_bool)
            == Some(true))
    }
}

impl Drop for EditorBinaryExportFinishGuard<'_> {
    fn drop(&mut self) {
        let _ = self.finish(false);
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn receive_editor_binary_export(bridge: &BridgeServer) -> Result<EditorBinaryExport> {
    let mut export = begin_editor_binary_export(bridge, false, None, None, false)?;
    let mut finish_guard = EditorBinaryExportFinishGuard {
        bridge,
        export_id: export.export_id.clone(),
        #[cfg(windows)]
        attribute_guard: export.attribute_guard.take(),
    };
    export.bytes = receive_editor_binary_export_bytes(
        bridge,
        export
            .export_id
            .as_deref()
            .context("Native export id is missing")?,
        None,
        None,
    )?;
    finish_guard.finish(false)?;
    Ok(export)
}

#[cfg(any(windows, target_os = "macos"))]
fn receive_editor_binary_export_for_runtime(
    bridge: &BridgeServer,
    runtime_id: &str,
) -> Result<EditorBinaryExport> {
    let mut export = begin_editor_binary_export_for_runtime(
        bridge,
        false,
        None,
        None,
        false,
        Some(runtime_id),
        true,
    )?;
    let result = receive_editor_binary_export_bytes_for_runtime(
        bridge,
        export
            .export_id
            .as_deref()
            .context("Native export id is missing")?,
        None,
        None,
        Some(runtime_id),
    );
    let finish = bridge.call_for_runtime_with_timeout(
        "finishEditorBinaryExport",
        json!({
            "exportId": export.export_id.as_deref(),
            "recordSyncCompletion": false,
        }),
        crate::studio::bridge::BridgeTarget::Edit,
        runtime_id,
        None,
    );
    export.bytes = result?;
    validate_native_export_finished(&finish?)?;
    Ok(export)
}

pub(crate) fn rbx_variant_referent(value: &RbxVariant) -> Option<RbxRef> {
    match value {
        RbxVariant::Ref(referent) => Some(*referent),
        RbxVariant::Content(content) => match content.value() {
            RbxContentType::Object(referent) => Some(*referent),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) struct NativeServiceDom {
    instances: Vec<rbx_binary::FlatInstance>,
    new_index_by_dense_ref: Option<Vec<usize>>,
    native_index_by_overlay_index: Vec<usize>,
    captured_debug_ids: Option<Vec<String>>,
    captured_root_properties: Map<String, Value>,
    path_segments_by_ref: Arc<HashMap<RbxRef, Vec<String>>>,
    path_ordinals_by_ref: Arc<HashMap<RbxRef, Vec<usize>>>,
}

struct NativeIdentityOutput {
    instances: Vec<rbx_binary::FlatInstance>,
    native_index_by_overlay_index: Vec<usize>,
    new_index_by_dense_ref: Vec<usize>,
}

// Native capture joins by engine identity, never by names or sibling order.
fn native_capture_debug_ids(
    instances: &[rbx_binary::FlatInstance],
    rows: &[u8],
) -> Result<Vec<String>> {
    use rbx_dom_weak::types::UniqueId;
    anyhow::ensure!(
        rows.len().is_multiple_of(72) && rows.len() / 72 == instances.len(),
        "Native identity capture has the wrong row count"
    );
    let mut row_by_id = AHashMap::with_capacity(instances.len());
    let mut seen_debug_ids = AHashSet::with_capacity(instances.len());
    let mut captured = Vec::with_capacity(instances.len());
    for (index, row) in rows.chunks_exact(72).enumerate() {
        let word = |offset| u32::from_le_bytes(row[offset..offset + 4].try_into().unwrap());
        // The getter's native struct stores four LE words in display order,
        // not a little-endian u64 followed by time/index.
        let id = UniqueId::new(
            word(12),
            word(8),
            ((u64::from(word(0)) << 32) | u64::from(word(4))) as i64,
        );
        anyhow::ensure!(
            !id.is_nil() && row_by_id.insert(id, index).is_none(),
            "Native identity capture contains a nil or duplicate UniqueId"
        );
        let text = &row[16..64];
        let length = text
            .iter()
            .position(|byte| *byte == 0)
            .context("Native debug identity is unterminated")?;
        let debug_id =
            std::str::from_utf8(&text[..length]).context("Native debug identity is not UTF-8")?;
        anyhow::ensure!(
            length > 0
                && seen_debug_ids.insert(debug_id)
                && text[length..].iter().all(|byte| *byte == 0),
            "Native debug identity is empty, duplicated, or has invalid padding"
        );
        let parent = word(64);
        anyhow::ensure!(
            word(68) == 0 && (parent == u32::MAX || (parent as usize) < index),
            "Native identity capture has an invalid parent or reserved field"
        );
        captured.push((debug_id, (parent != u32::MAX).then_some(parent as usize)));
    }
    let mut row_by_native_index = Vec::with_capacity(instances.len());
    let mut seen_rows = vec![false; instances.len()];
    for instance in instances {
        let id = instance
            .properties
            .iter()
            .find_map(|(name, value)| {
                if name.as_str() == "UniqueId"
                    && let RbxVariant::UniqueId(id) = value
                {
                    return Some(id);
                }
                None
            })
            .context("Native serialized instance omitted UniqueId")?;
        let row = *row_by_id
            .get(id)
            .context("Serialized identity is absent from native capture")?;
        anyhow::ensure!(
            !std::mem::replace(&mut seen_rows[row], true),
            "Serialized snapshot repeats a captured identity"
        );
        row_by_native_index.push(row);
    }
    instances
        .iter()
        .zip(&row_by_native_index)
        .map(|(instance, row)| {
            let parent = instance
                .parent_index
                .map(|parent| {
                    row_by_native_index
                        .get(parent)
                        .copied()
                        .context("Native serialized parent is out of range")
                })
                .transpose()?;
            anyhow::ensure!(
                parent == captured[*row].1,
                "Native serialized parent disagrees with identity capture"
            );
            Ok(captured[*row].0.to_string())
        })
        .collect()
}

fn native_identity_carrier_count(group: &EditorBinaryExportGroup) -> Result<usize> {
    if group.identity_carrier_class.is_empty()
        || group.identity_carrier_prefix.is_empty()
        || group.identity_carrier_slots.is_empty()
        || group.identity_carrier_slots.iter().any(String::is_empty)
        || group
            .identity_carrier_slots
            .iter()
            .collect::<HashSet<_>>()
            .len()
            != group.identity_carrier_slots.len()
    {
        bail!("Studio native {} identity schema is invalid", group.service);
    }
    let expected = group
        .instance_count
        .saturating_sub(1)
        .div_ceil(group.identity_carrier_slots.len());
    if group.identity_carrier_count != expected {
        bail!(
            "Studio native {} identity carrier count is invalid",
            group.service
        );
    }
    Ok(expected)
}

fn native_serialized_instance_count(group: &EditorBinaryExportGroup) -> Result<usize> {
    group
        .instance_count
        .checked_add(native_identity_carrier_count(group)?)
        .context("Native serialized instance count overflowed")
}

fn native_serialized_root_count(group: &EditorBinaryExportGroup) -> Result<usize> {
    group
        .count
        .checked_add(native_identity_carrier_count(group)?)
        .and_then(|count| count.checked_add(1))
        .context("Native serialized root count overflowed")
}

pub(super) fn serialized_service_roots(
    dom: &mut RbxWeakDom,
    groups: &[EditorBinaryExportGroup],
) -> Result<Vec<(RbxRef, Vec<RbxRef>)>> {
    let roots = dom.root().children().to_vec();
    let expected_roots = groups.iter().try_fold(0_usize, |total, group| {
        total
            .checked_add(native_serialized_root_count(group)?)
            .context("Studio plugin place snapshot root count overflowed")
    })?;
    if roots.len() != expected_roots {
        bail!(
            "Studio plugin place snapshot has {} roots; expected {expected_roots}",
            roots.len()
        );
    }

    let mut cursor = 0;
    let mut carrier_refs = Vec::new();
    let mut service_roots = Vec::with_capacity(groups.len());
    for group in groups {
        let marker_ref = roots[cursor];
        cursor += 1;
        let children_end = cursor + group.count;
        let child_refs = roots[cursor..children_end].to_vec();
        cursor = children_end;

        let carrier_count = native_identity_carrier_count(group)?;
        let carriers_end = cursor + carrier_count;
        for (index, carrier_ref) in roots[cursor..carriers_end].iter().copied().enumerate() {
            let carrier = dom
                .get_by_ref(carrier_ref)
                .context("Studio plugin place snapshot lost an identity carrier")?;
            let expected_name = format!(
                "{}{ordinal}",
                group.identity_carrier_prefix,
                ordinal = index + 1
            );
            if carrier.class.as_str() != group.identity_carrier_class
                || carrier.name != expected_name
                || !carrier.children().is_empty()
            {
                bail!(
                    "Studio plugin {} identity carrier {} is invalid",
                    group.service,
                    index + 1
                );
            }
            carrier_refs.push(carrier_ref);
        }
        cursor = carriers_end;
        service_roots.push((marker_ref, child_refs));
    }
    for carrier_ref in carrier_refs {
        dom.destroy(carrier_ref);
    }
    Ok(service_roots)
}

fn decode_native_identity(
    instances: Vec<rbx_binary::FlatInstance>,
    group: &EditorBinaryExportGroup,
    dense_ref_count: usize,
) -> Result<NativeIdentityOutput> {
    if instances.len() != native_serialized_instance_count(group)? || instances.is_empty() {
        bail!(
            "Studio native {} identity payload has the wrong size",
            group.service
        );
    }
    let mut instance_index_by_ref = AHashMap::with_capacity(instances.len());
    for (index, instance) in instances.iter().enumerate() {
        if instance_index_by_ref
            .insert(instance.referent, index)
            .is_some()
        {
            bail!(
                "Studio native {} snapshot contains duplicate referents",
                group.service
            );
        }
    }
    let carrier_count = native_identity_carrier_count(group)?;
    let mut carrier_index_by_name = AHashMap::with_capacity(carrier_count);
    for (index, instance) in instances.iter().enumerate() {
        if instance.parent_index.is_none()
            && instance.class.as_str() == group.identity_carrier_class
            && instance.name.starts_with(&group.identity_carrier_prefix)
            && carrier_index_by_name
                .insert(instance.name.as_str(), index)
                .is_some()
        {
            bail!(
                "Studio native {} identity carrier is duplicated",
                group.service
            );
        }
    }
    let mut carrier_indices = Vec::with_capacity(carrier_count);
    for ordinal in 1..=carrier_count {
        let expected_name = format!("{}{ordinal}", group.identity_carrier_prefix);
        let index = carrier_index_by_name
            .remove(expected_name.as_str())
            .with_context(|| {
                format!(
                    "Studio native {} identity carrier {ordinal} is missing",
                    group.service
                )
            })?;
        carrier_indices.push(index);
    }
    if !carrier_index_by_name.is_empty() {
        bail!(
            "Studio native {} identity carrier is unexpected",
            group.service
        );
    }
    let carrier_index_set = carrier_indices.iter().copied().collect::<AHashSet<_>>();
    let mut old_index_by_overlay_index = Vec::with_capacity(group.instance_count);
    old_index_by_overlay_index.push(0);
    for carrier_index in carrier_indices {
        let carrier = &instances[carrier_index];
        for property_name in &group.identity_carrier_slots {
            if old_index_by_overlay_index.len() == group.instance_count {
                break;
            }
            let target = carrier
                .properties
                .iter()
                .find(|(name, _)| name.as_str() == property_name)
                .and_then(|(_, value)| rbx_variant_referent(value))
                .with_context(|| {
                    format!(
                        "Studio native {} identity carrier omitted {}",
                        group.service, property_name
                    )
                })?;
            let target_index = instance_index_by_ref
                .get(&target)
                .copied()
                .context("Native identity carrier points outside its snapshot")?;
            if carrier_index_set.contains(&target_index) {
                bail!("Native identity carrier points to another carrier");
            }
            old_index_by_overlay_index.push(target_index);
        }
    }
    if old_index_by_overlay_index.len() != group.instance_count
        || old_index_by_overlay_index
            .iter()
            .copied()
            .collect::<AHashSet<_>>()
            .len()
            != group.instance_count
    {
        bail!(
            "Studio native {} identity mapping is incomplete",
            group.service
        );
    }
    let mut old_to_new = vec![usize::MAX; instances.len()];
    let mut next_index = 0;
    for (old_index, new_index) in old_to_new.iter_mut().enumerate() {
        if !carrier_index_set.contains(&old_index) {
            *new_index = next_index;
            next_index += 1;
        }
    }
    if next_index != group.instance_count {
        bail!(
            "Studio native {} identity payload contains extra instances",
            group.service
        );
    }
    let native_index_by_overlay_index = old_index_by_overlay_index
        .into_iter()
        .map(|old_index| {
            old_to_new[old_index]
                .ne(&usize::MAX)
                .then_some(old_to_new[old_index])
                .context("Native identity resolved to a removed carrier")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut new_index_by_dense_ref = vec![usize::MAX; dense_ref_count];
    let mut output = Vec::with_capacity(group.instance_count);
    for (old_index, mut instance) in instances.into_iter().enumerate() {
        if carrier_index_set.contains(&old_index) {
            continue;
        }
        if let Some(parent_index) = instance.parent_index {
            instance.parent_index = Some(
                old_to_new
                    .get(parent_index)
                    .copied()
                    .filter(|index| *index != usize::MAX)
                    .context("Native instance parent resolved to an identity carrier")?,
            );
        }
        let dense_index = instance
            .referent
            .as_u128()
            .and_then(|value| usize::try_from(value).ok())
            .and_then(|value| value.checked_sub(1))
            .filter(|index| *index < dense_ref_count)
            .context("Native snapshot contains an invalid dense referent")?;
        new_index_by_dense_ref[dense_index] = output.len();
        output.push(instance);
    }
    Ok(NativeIdentityOutput {
        instances: output,
        native_index_by_overlay_index,
        new_index_by_dense_ref,
    })
}

// These focused identity tests stay beside the transformation they characterize.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod native_identity_tests {
    use super::*;

    #[test]
    fn native_export_finalization_requires_a_live_guard() {
        assert!(validate_native_export_finished(&json!({"found": true})).is_ok());
        for result in [json!({"found": false}), json!({}), json!({"found": "true"})] {
            assert!(validate_native_export_finished(&result).is_err());
        }
    }
    use rbx_dom_weak::InstanceBuilder;

    fn capture_row(serial: u32, parent: u32, debug_id: &str) -> [u8; 72] {
        let mut row = [0; 72];
        row[..4].copy_from_slice(&0x12345678u32.to_le_bytes());
        row[4..8].copy_from_slice(&0x90abcdefu32.to_le_bytes());
        row[8..12].copy_from_slice(&0x10203040u32.to_le_bytes());
        row[12..16].copy_from_slice(&serial.to_le_bytes());
        row[16..16 + debug_id.len()].copy_from_slice(debug_id.as_bytes());
        row[64..68].copy_from_slice(&parent.to_le_bytes());
        row
    }

    #[test]
    fn native_capture_joins_by_identity_not_same_named_sibling_order() {
        let make = |serial, parent| {
            flat_instance(
                u128::from(serial),
                parent,
                "Same",
                "Folder",
                vec![(
                    "UniqueId".into(),
                    RbxVariant::UniqueId(rbx_dom_weak::types::UniqueId::new(
                        serial,
                        0x10203040,
                        0x1234567890abcdef,
                    )),
                )],
            )
        };
        let instances = vec![make(1, None), make(3, Some(0)), make(2, Some(0))];
        let rows = [
            capture_row(1, u32::MAX, "0_001"),
            capture_row(2, 0, "0_002"),
            capture_row(3, 0, "0_003"),
        ]
        .concat();
        assert_eq!(
            native_capture_debug_ids(&instances, &rows).unwrap(),
            ["0_001", "0_003", "0_002"]
        );
        for (offset, bytes) in [
            (72 + 12, 1u32.to_le_bytes()),  // duplicate UniqueId
            (72 + 64, 1u32.to_le_bytes()),  // cyclic parent
            (144 + 64, 1u32.to_le_bytes()), // valid but wrong parent
            (144 + 68, 1u32.to_le_bytes()), // unknown row layout
        ] {
            let mut changed = rows.clone();
            changed[offset..offset + 4].copy_from_slice(&bytes);
            assert!(native_capture_debug_ids(&instances, &changed).is_err());
        }
        let mut duplicate_debug_id = rows.clone();
        duplicate_debug_id[72 + 16..72 + 64].copy_from_slice(&rows[16..64]);
        assert!(native_capture_debug_ids(&instances, &duplicate_debug_id).is_err());
        let mut unterminated = rows.clone();
        unterminated[16..64].fill(b'a');
        assert!(native_capture_debug_ids(&instances, &unterminated).is_err());
        assert!(native_capture_debug_ids(&instances, &rows[..rows.len() - 1]).is_err());
        let missing = vec![make(1, None), make(4, Some(0)), make(2, Some(0))];
        assert!(native_capture_debug_ids(&missing, &rows).is_err());
        let duplicated = vec![make(1, None), make(2, Some(0)), make(2, Some(0))];
        assert!(native_capture_debug_ids(&duplicated, &rows).is_err());
    }

    #[test]
    #[ignore = "Requires explicit files from an owned native-capture fixture; no Studio calls"]
    fn native_capture_identity_saved_fixture() -> Result<()> {
        let input = std::env::var("RENIUM_CAPTURE_IDENTITY_RBXL")?;
        let identities = std::env::var("RENIUM_CAPTURE_IDENTITY_ROWS")?;
        let decode_started = Instant::now();
        let bytes = std::fs::read(input)?;
        let flat = rbx_binary::Deserializer::new()
            .elide_defaults(true)
            .deserialize_flat(bytes.as_slice())?;
        let decode_ms = elapsed_ms(decode_started);
        let rows = std::fs::read(identities)?;
        let map_started = Instant::now();
        let debug_ids = native_capture_debug_ids(&flat.instances, &rows)?;
        println!(
            "{}",
            json!({"identities":debug_ids.len(),"decodeMs":decode_ms,"identityMapMs":elapsed_ms(map_started)})
        );
        let (groups, batch) = captured_test_groups(&flat);
        let partition_started = Instant::now();
        let doms = decode_native_serialization_batch(
            &bytes,
            &batch,
            &groups,
            Arc::default(),
            Some(&rows),
        )?;
        assert_eq!(
            doms.values().map(|dom| dom.instances.len()).sum::<usize>(),
            flat.instances.len()
        );
        println!(
            "{}",
            json!({"nativeServicePartitions": doms.len(), "partitionWithDecodeMs": elapsed_ms(partition_started)})
        );
        Ok(())
    }

    fn captured_test_groups(
        flat: &rbx_binary::FlatDom,
    ) -> (Vec<EditorBinaryExportGroup>, EditorBinarySerializationBatch) {
        let groups = flat.root_indices.iter().enumerate().map(|(index, start)| {
            let root = &flat.instances[*start];
            let end = flat.root_indices.get(index + 1).copied().unwrap_or(flat.instances.len());
            serde_json::from_value(json!({"service": root.class.as_str(), "targetPath": [root.class.as_str()],
                "count": flat.instances[*start..end].iter().filter(|item| item.parent_index == Some(*start)).count(),
                "instanceCount": end - start, "classNames": []})).unwrap()
        }).collect::<Vec<EditorBinaryExportGroup>>();
        let batch = EditorBinarySerializationBatch {
            id: "capture".into(),
            services: groups.iter().map(|group| group.service.clone()).collect(),
        };
        (groups, batch)
    }

    #[test]
    fn native_capture_partitions_preserve_cross_service_and_duplicate_identity() {
        let mut dom = RbxWeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let make = |class: &str, name: &str, serial| {
            InstanceBuilder::new(class).with_name(name).with_property(
                "UniqueId",
                rbx_dom_weak::types::UniqueId::new(serial, 0x10203040, 0x1234567890abcdef),
            )
        };
        let workspace = dom.insert(
            root,
            make("Workspace", "Renamed workspace", 1)
                .with_property(
                    "ModelStreamingBehavior",
                    rbx_dom_weak::types::Enum::from_u32(0),
                )
                .with_property("StreamOutBehavior", rbx_dom_weak::types::Enum::from_u32(0))
                .with_property(
                    "StreamingIntegrityMode",
                    rbx_dom_weak::types::Enum::from_u32(0),
                )
                .with_property("StreamingTargetRadius", 1024i32)
                .with_property(
                    "UseNewLuauTypeSolver",
                    rbx_dom_weak::types::Enum::from_u32(0),
                ),
        );
        let storage = dom.insert(root, make("ReplicatedStorage", "ReplicatedStorage", 2));
        let first = dom.insert(workspace, make("Folder", "Same", 3));
        let second = dom.insert(workspace, make("Folder", "Same", 4));
        dom.insert(
            first,
            make("Part", "Payload", 5).with_property("Transparency", 0.0f32),
        );
        let target = dom.insert(
            second,
            make("Part", "Payload", 6).with_property("Transparency", 0.0f32),
        );
        dom.insert(
            storage,
            make("ObjectValue", "Target", 7).with_property("Value", target),
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[workspace, storage]).unwrap();
        let flat = rbx_binary::Deserializer::new()
            .elide_defaults(true)
            .deserialize_flat(bytes.as_slice())
            .unwrap();
        let (groups, batch) = captured_test_groups(&flat);
        let rows = [u32::MAX, u32::MAX, 0, 0, 2, 3, 1]
            .iter()
            .enumerate()
            .flat_map(|(index, parent)| {
                capture_row(index as u32 + 1, *parent, &format!("id{index}"))
            })
            .collect::<Vec<_>>();
        let decode = || {
            decode_native_serialization_batch(&bytes, &batch, &groups, Arc::default(), Some(&rows))
                .unwrap()
        };
        let mut services = decode();
        assert_eq!(services["Workspace"].captured_root_properties.len(), 5);
        for instance in &services["Workspace"].instances {
            if instance.class.as_str() == "Part" {
                assert!(
                    !instance
                        .properties
                        .iter()
                        .any(|(name, _)| name.as_str() == "Transparency"),
                    "Keeping service defaults must not expand ordinary instance defaults"
                );
            }
        }
        let storage = &services["ReplicatedStorage"];
        let target = storage.instances[1]
            .properties
            .iter()
            .find(|(name, _)| name.as_str() == "Value")
            .unwrap()
            .1
            .clone();
        let target = rbx_variant_referent(&target).unwrap();
        assert_eq!(
            storage.path_segments_by_ref[&target],
            ["Workspace", "Same", "Payload"]
        );
        assert_eq!(storage.path_ordinals_by_ref[&target], [1, 2, 1]);
        let workspace = services.get_mut("Workspace").unwrap();
        assert_eq!(workspace.instances[0].class.as_str(), "Workspace");
        let captured = workspace.captured_debug_ids.clone().unwrap();
        let order = [0, 4, 3, 2, 1];
        let overlay = order
            .iter()
            .map(|index| Some(captured[*index].clone()))
            .collect::<Vec<_>>();
        match_native_capture_overlay(workspace, &overlay).unwrap();
        assert_eq!(workspace.native_index_by_overlay_index, order);
        for broken in [
            vec![None; 5],
            vec![Some("unknown".into()); 5],
            vec![overlay[0].clone(); 5],
            overlay[..4].to_vec(),
        ] {
            let mut services = decode();
            assert!(
                match_native_capture_overlay(services.get_mut("Workspace").unwrap(), &broken)
                    .is_err()
            );
        }
    }

    fn flat_instance(
        referent: u128,
        parent_index: Option<usize>,
        name: &str,
        class: &str,
        properties: Vec<(rbx_dom_weak::Ustr, RbxVariant)>,
    ) -> rbx_binary::FlatInstance {
        rbx_binary::FlatInstance {
            referent: RbxRef::some(referent),
            parent_index,
            name: name.to_string(),
            class: class.into(),
            properties,
        }
    }

    #[test]
    fn identity_carriers_map_overlay_rows_to_native_order() {
        let group = EditorBinaryExportGroup {
            service: "Workspace".to_string(),
            target_path: vec!["Workspace".to_string()],
            count: 2,
            instance_count: 3,
            identity_carrier_class: "HumanoidRigDescription".to_string(),
            identity_carrier_prefix: "__ReniumNativeIdentity:test:Workspace:".to_string(),
            identity_carrier_slots: vec!["Chest".to_string(), "HeadBase".to_string()],
            identity_carrier_count: 1,
            script_count: 0,
            class_names: vec!["Folder".to_string()],
            non_archivable_indices: Vec::new(),
            root_properties: Map::new(),
        };
        let identity = decode_native_identity(
            vec![
                flat_instance(1, None, "Workspace", "Folder", Vec::new()),
                flat_instance(2, Some(0), "Second", "Folder", Vec::new()),
                flat_instance(
                    3,
                    None,
                    "__ReniumNativeIdentity:test:Workspace:1",
                    "HumanoidRigDescription",
                    vec![
                        ("Chest".into(), RbxVariant::Ref(RbxRef::some(4))),
                        ("HeadBase".into(), RbxVariant::Ref(RbxRef::some(2))),
                    ],
                ),
                flat_instance(4, Some(0), "First", "Folder", Vec::new()),
            ],
            &group,
            4,
        )
        .unwrap();

        assert_eq!(identity.native_index_by_overlay_index, vec![0, 2, 1]);
        assert_eq!(identity.new_index_by_dense_ref, vec![0, 1, usize::MAX, 2]);
        assert_eq!(
            identity
                .instances
                .iter()
                .map(|instance| instance.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Workspace", "Second", "First"]
        );
    }

    #[test]
    fn script_sources_follow_identity_remapping() {
        let mut sources = SourceBatchMap::default();
        sources.by_index.insert(2, "first".to_string());
        sources.by_index.insert(3, "second".to_string());

        remap_script_source_indices(&mut sources, &[0, 2, 1]).unwrap();

        assert_eq!(sources.by_index.get(&2).map(String::as_str), Some("second"));
        assert_eq!(sources.by_index.get(&3).map(String::as_str), Some("first"));
    }

    #[test]
    fn root_camera_reference_uses_native_order_like_other_overlay_references() {
        let mut root = Map::from_iter([
            (
                "CurrentCamera".into(),
                json!({"_type": "Ref", "debugId": "viewport", "pathSegments": ["Workspace", "Camera"]}),
            ),
            (
                "External".into(),
                json!({"_type": "Ref", "debugId": "other-service"}),
            ),
        ]);
        // Debug IDs have already been reordered from the plugin's traversal.
        normalize_native_overlay_internal_references(
            &mut [],
            Some(&mut root),
            [Some("root"), Some("extra"), Some("viewport")],
        );
        assert_eq!(
            root["CurrentCamera"],
            json!({"_type": "Ref", "instanceIndex": 3})
        );
        assert_eq!(root["External"]["debugId"], "other-service");
    }

    fn plugin_place_group(
        count: usize,
        instance_count: usize,
        carrier_count: usize,
    ) -> EditorBinaryExportGroup {
        EditorBinaryExportGroup {
            service: "Workspace".to_string(),
            target_path: vec!["Workspace".to_string()],
            count,
            instance_count,
            identity_carrier_class: "HumanoidRigDescription".to_string(),
            identity_carrier_prefix: "__ReniumNativeIdentity:Workspace:".to_string(),
            identity_carrier_slots: (1..=22).map(|index| format!("Slot{index}")).collect(),
            identity_carrier_count: carrier_count,
            script_count: 0,
            class_names: vec!["Folder".to_string(), "Part".to_string()],
            non_archivable_indices: Vec::new(),
            root_properties: Map::new(),
        }
    }

    #[test]
    fn large_plugin_place_count_includes_identity_carriers() {
        let group = plugin_place_group(86, 95_855, 4_357);

        assert_eq!(group.count + 1, 87);
        assert_eq!(native_serialized_root_count(&group).unwrap(), 4_444);
    }

    #[test]
    fn plugin_place_reconstruction_removes_only_identity_carrier_roots() {
        let mut source = RbxWeakDom::new(InstanceBuilder::new("DataModel"));
        let data_model = source.root_ref();
        let marker = source.insert(
            data_model,
            InstanceBuilder::new("Folder").with_name("Workspace"),
        );
        let mut top_level_refs = vec![marker];
        for index in 1..=17 {
            let child_builder = if index == 17 {
                InstanceBuilder::new("HumanoidRigDescription")
                    .with_name("__ReniumNativeIdentity:Workspace:1")
            } else {
                InstanceBuilder::new("Folder").with_name(format!("Root{index}"))
            };
            let child = source.insert(data_model, child_builder);
            top_level_refs.push(child);
            if index == 1 {
                for descendant in 1..=6 {
                    source.insert(
                        child,
                        InstanceBuilder::new("Part").with_name(format!("Descendant{descendant}")),
                    );
                }
            }
        }
        for index in 1..=2 {
            let carrier = source.insert(
                data_model,
                InstanceBuilder::new("HumanoidRigDescription")
                    .with_name(format!("__ReniumNativeIdentity:Workspace:{index}")),
            );
            top_level_refs.push(carrier);
        }
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &source, &top_level_refs).unwrap();
        let mut reconstructed = rbx_binary::from_reader(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reconstructed.root().children().len(), 20);

        let groups =
            serialized_service_roots(&mut reconstructed, &[plugin_place_group(17, 24, 2)]).unwrap();
        let (marker_ref, child_refs) = &groups[0];
        for child_ref in child_refs {
            reconstructed.transfer_within(*child_ref, *marker_ref);
        }

        assert_eq!(reconstructed.root().children(), &[*marker_ref]);
        assert_eq!(
            reconstructed
                .get_by_ref(*marker_ref)
                .unwrap()
                .children()
                .len(),
            17
        );
        assert_eq!(reconstructed.descendants_of(*marker_ref).count(), 24);
        assert_eq!(
            reconstructed
                .descendants_of(*marker_ref)
                .filter(|instance| {
                    instance.class.as_str() == "HumanoidRigDescription"
                        && instance.name == "__ReniumNativeIdentity:Workspace:1"
                })
                .count(),
            1
        );
    }
}

struct NativeServiceExportResult {
    output: ServiceExportOutput,
    native_index_by_overlay_index: Vec<usize>,
    metrics: ChunkFetchMetrics,
    compact_expand_ms: f64,
}

struct NativePriorityWorkerGate {
    released: Mutex<bool>,
    ready: Condvar,
}

impl NativePriorityWorkerGate {
    fn new(released: bool) -> Self {
        Self {
            released: Mutex::new(released),
            ready: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut released = self.released.lock().unwrap_or_else(PoisonError::into_inner);
        while !*released {
            released = self
                .ready
                .wait(released)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn release(&self) {
        let mut released = self.released.lock().unwrap_or_else(PoisonError::into_inner);
        if *released {
            return;
        }
        *released = true;
        drop(released);
        self.ready.notify_all();
    }
}

fn decode_native_service_dom(
    bytes: &[u8],
    group: &EditorBinaryExportGroup,
    property_filter: Arc<HashMap<String, HashSet<String>>>,
) -> Result<NativeServiceDom> {
    let decode_started = Instant::now();
    let mut flat = rbx_binary::Deserializer::new()
        .elide_defaults(true)
        .flat_property_filter(property_filter)
        .deserialize_flat(std::io::Cursor::new(bytes))
        .with_context(|| {
            format!(
                "Studio returned an invalid native {} snapshot",
                group.service
            )
        })?;
    log_timing(
        &format!("{}: native binary decode", group.service),
        decode_started,
    );
    if flat.root_indices.len() != native_serialized_root_count(group)? {
        bail!(
            "Studio native {} snapshot contains {} roots; expected {}",
            group.service,
            flat.root_indices.len(),
            native_serialized_root_count(group)?
        );
    }
    let marker_index = flat.root_indices[0];
    if marker_index != 0 {
        bail!("Studio native {} marker is out of order", group.service);
    }
    flat.instances[marker_index].class = group.service.as_str().into();
    flat.instances[marker_index].name.clone_from(&group.service);
    for root_index in flat.root_indices.iter().skip(1) {
        let instance = &mut flat.instances[*root_index];
        if instance.class.as_str() != group.identity_carrier_class
            || !instance.name.starts_with(&group.identity_carrier_prefix)
        {
            instance.parent_index = Some(marker_index);
        }
    }
    let serialized_instance_count = native_serialized_instance_count(group)?;
    if flat.instances.len() != serialized_instance_count {
        bail!(
            "Studio native {} snapshot contains {} instances; expected {}",
            group.service,
            flat.instances.len(),
            serialized_instance_count
        );
    }
    let identity = decode_native_identity(flat.instances, group, serialized_instance_count)?;
    Ok(NativeServiceDom {
        instances: identity.instances,
        new_index_by_dense_ref: Some(identity.new_index_by_dense_ref),
        native_index_by_overlay_index: identity.native_index_by_overlay_index,
        captured_debug_ids: None,
        captured_root_properties: Map::new(),
        path_segments_by_ref: Arc::new(HashMap::new()),
        path_ordinals_by_ref: Arc::new(HashMap::new()),
    })
}

fn decode_native_serialization_batch(
    bytes: &[u8],
    batch: &EditorBinarySerializationBatch,
    service_groups: &[EditorBinaryExportGroup],
    property_filter: Arc<HashMap<String, HashSet<String>>>,
    identity_rows: Option<&[u8]>,
) -> Result<HashMap<String, NativeServiceDom>> {
    let groups = batch
        .services
        .iter()
        .map(|service| {
            service_groups
                .iter()
                .find(|group| group.service == *service)
                .with_context(|| {
                    format!(
                        "Native serialization batch {} omitted service {}",
                        batch.id, service
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let expected_roots = groups.iter().try_fold(0_usize, |total, group| {
        total
            .checked_add(if identity_rows.is_some() {
                1
            } else {
                native_serialized_root_count(group)?
            })
            .context("Native serialization batch root count overflowed")
    })?;
    let expected_instances = groups.iter().try_fold(0_usize, |total, group| {
        total
            .checked_add(if identity_rows.is_some() {
                group.instance_count
            } else {
                native_serialized_instance_count(group)?
            })
            .context("Native serialization batch instance count overflowed")
    })?;
    let decode_started = Instant::now();
    let mut flat = match rbx_binary::Deserializer::new()
        .elide_defaults(true)
        .retain_defaults_for_classes(if identity_rows.is_some() {
            groups.iter().map(|group| group.service.clone()).collect()
        } else {
            HashSet::new()
        })
        .flat_property_filter(property_filter)
        .deserialize_flat(std::io::Cursor::new(bytes))
    {
        Ok(flat) => flat,
        Err(error) => bail!(
            "Studio returned an invalid native serialization batch {}: {}",
            batch.id,
            error
        ),
    };
    log_timing(
        &format!("{}: native binary decode", batch.id),
        decode_started,
    );
    let captured_debug_ids = identity_rows
        .map(|rows| native_capture_debug_ids(&flat.instances, rows))
        .transpose()?;
    if flat.root_indices.len() != expected_roots {
        bail!(
            "Studio native serialization batch {} contains {} roots; expected {}",
            batch.id,
            flat.root_indices.len(),
            expected_roots
        );
    }
    if flat.instances.len() != expected_instances {
        bail!(
            "Studio native serialization batch {} contains {} instances; expected {}",
            batch.id,
            flat.instances.len(),
            expected_instances
        );
    }
    let mut spans = Vec::with_capacity(groups.len());
    let mut root_offset = 0;
    let mut expected_start = 0;
    for group in &groups {
        let root_end = root_offset
            + if identity_rows.is_some() {
                1
            } else {
                native_serialized_root_count(group)?
            };
        let start = flat.root_indices[root_offset];
        let end = flat
            .root_indices
            .get(root_end)
            .copied()
            .unwrap_or(flat.instances.len());
        if start != expected_start
            || end <= start
            || flat.root_indices[root_offset..root_end]
                .iter()
                .any(|root| *root < start || *root >= end)
        {
            bail!(
                "Studio native {} batch partition is out of order",
                group.service
            );
        }
        let serialized_instance_count = if identity_rows.is_some() {
            group.instance_count
        } else {
            native_serialized_instance_count(group)?
        };
        if end - start != serialized_instance_count {
            bail!(
                "Studio native {} batch partition contains {} instances; expected {}",
                group.service,
                end - start,
                serialized_instance_count
            );
        }
        let marker = &flat.instances[start];
        if marker.parent_index.is_some()
            || marker.class.as_str()
                != if identity_rows.is_some() {
                    group.service.as_str()
                } else {
                    "Folder"
                }
            || identity_rows.is_none() && marker.name != group.service
        {
            bail!("Studio native {} batch marker is invalid", group.service);
        }
        if identity_rows.is_some() {
            flat.instances[start].name.clone_from(&group.service);
        }
        spans.push((start, end, root_offset, root_end));
        root_offset = root_end;
        expected_start = end;
    }
    let total_instances = flat.instances.len();
    let mut global_index_by_dense_ref = vec![usize::MAX; total_instances];
    let mut owner_by_dense_ref = vec![usize::MAX; total_instances];
    for (group_index, (start, end, _, _)) in spans.iter().copied().enumerate() {
        for global_index in start..end {
            let dense_index = flat.instances[global_index]
                .referent
                .as_u128()
                .and_then(|value| usize::try_from(value).ok())
                .and_then(|value| value.checked_sub(1))
                .filter(|value| *value < total_instances)
                .context("Native serialization batch contains an invalid dense referent")?;
            if global_index_by_dense_ref[dense_index] != usize::MAX {
                bail!("Native serialization batch contains a duplicate dense referent");
            }
            global_index_by_dense_ref[dense_index] = global_index;
            owner_by_dense_ref[dense_index] = group_index;
        }
    }
    if global_index_by_dense_ref.contains(&usize::MAX) {
        bail!("Native serialization batch has an incomplete dense referent map");
    }
    let mut sibling_counts = HashMap::<(Option<usize>, String), usize>::new();
    let mut ordinal_by_global_index = Vec::with_capacity(total_instances);
    for instance in &flat.instances {
        let key = (instance.parent_index, instance.name.clone());
        let ordinal = sibling_counts.entry(key).or_insert(0);
        *ordinal += 1;
        ordinal_by_global_index.push(*ordinal);
    }
    let mut cross_service_targets = HashSet::new();
    for (group_index, (start, end, _, _)) in spans.iter().copied().enumerate() {
        for instance in &flat.instances[start..end] {
            for (_, value) in &instance.properties {
                let Some(dense_index) = rbx_variant_referent(value)
                    .and_then(RbxRef::as_u128)
                    .and_then(|value| usize::try_from(value).ok())
                    .and_then(|value| value.checked_sub(1))
                    .filter(|value| *value < total_instances)
                else {
                    continue;
                };
                if owner_by_dense_ref[dense_index] != group_index {
                    cross_service_targets.insert(global_index_by_dense_ref[dense_index]);
                }
            }
        }
    }
    let mut path_segments_by_ref = HashMap::with_capacity(cross_service_targets.len());
    let mut path_ordinals_by_ref = HashMap::with_capacity(cross_service_targets.len());
    for global_index in cross_service_targets {
        let mut path_segments = Vec::new();
        let mut path_ordinals = Vec::new();
        let mut current_index = Some(global_index);
        while let Some(index) = current_index {
            let instance = &flat.instances[index];
            path_segments.push(instance.name.clone());
            path_ordinals.push(ordinal_by_global_index[index]);
            current_index = instance.parent_index;
        }
        path_segments.reverse();
        path_ordinals.reverse();
        path_segments_by_ref.insert(flat.instances[global_index].referent, path_segments);
        path_ordinals_by_ref.insert(flat.instances[global_index].referent, path_ordinals);
    }
    let path_segments_by_ref = Arc::new(path_segments_by_ref);
    let path_ordinals_by_ref = Arc::new(path_ordinals_by_ref);
    let root_indices = flat.root_indices;
    let mut source_instances = flat.instances.into_iter();
    let mut doms = HashMap::with_capacity(groups.len());
    for (group_index, group) in groups.into_iter().enumerate() {
        let (start, end, root_start, root_end) = spans[group_index];
        let mut instances = source_instances
            .by_ref()
            .take(end - start)
            .collect::<Vec<_>>();
        if instances.len() != end - start {
            bail!(
                "Studio native {} batch partition is incomplete",
                group.service
            );
        }
        let partition_len = instances.len();
        for instance in &mut instances {
            if let Some(parent_index) = instance.parent_index {
                instance.parent_index = Some(
                    parent_index
                        .checked_sub(start)
                        .filter(|parent| *parent < partition_len)
                        .with_context(|| {
                            format!(
                                "Studio native {} batch parent leaves its partition",
                                group.service
                            )
                        })?,
                );
            }
        }
        instances[0].class = group.service.as_str().into();
        instances[0].name.clone_from(&group.service);
        for root_index in &root_indices[root_start + 1..root_end] {
            let instance = &mut instances[*root_index - start];
            if instance.class.as_str() != group.identity_carrier_class
                || !instance.name.starts_with(&group.identity_carrier_prefix)
            {
                instance.parent_index = Some(0);
            }
        }
        let (identity, captured_debug_ids) = if let Some(ids) = &captured_debug_ids {
            anyhow::ensure!(
                instances.len() == group.instance_count,
                "Native captured {} instance count changed",
                group.service
            );
            let mut new_index_by_dense_ref = vec![usize::MAX; total_instances];
            for (index, instance) in instances.iter().enumerate() {
                // The complete dense map was validated before partitioning.
                new_index_by_dense_ref[instance.referent.as_u128().unwrap() as usize - 1] = index;
            }
            (
                NativeIdentityOutput {
                    instances,
                    native_index_by_overlay_index: Vec::new(),
                    new_index_by_dense_ref,
                },
                Some(ids[start..end].to_vec()),
            )
        } else {
            (
                decode_native_identity(instances, group, total_instances)?,
                None,
            )
        };
        let mut captured_root_properties = Map::new();
        if identity_rows.is_some() {
            let database = rbx_reflection_database::get()?;
            for &name in crate::editor::native_roots::capture_properties(&group.service) {
                let saved_name = crate::rbx::encode::rbx_serialized_property_name_for_logical(
                    database,
                    &group.service,
                    name,
                )
                .unwrap_or(name);
                let value = identity.instances[0]
                    .properties
                    .iter()
                    .find(|(property, _)| property.as_str() == saved_name)
                    .with_context(|| format!("Native capture omitted {}.{name}", group.service))?;
                let descriptor = crate::rbx::encode::rbx_model_property_descriptor(
                    database,
                    &group.service,
                    name,
                )
                .with_context(|| {
                    format!("Native capture has no schema for {}.{name}", group.service)
                })?;
                let value = crate::rbx::decode::rbx_variant_to_settings_json(
                    &value.1,
                    Some(descriptor),
                    database,
                    &BytecodeModelImportRefs::default(),
                )
                .with_context(|| {
                    format!("Native capture cannot decode {}.{name}", group.service)
                })?;
                captured_root_properties.insert(name.into(), value);
            }
        }
        if doms
            .insert(
                group.service.clone(),
                NativeServiceDom {
                    instances: identity.instances,
                    new_index_by_dense_ref: Some(identity.new_index_by_dense_ref),
                    native_index_by_overlay_index: identity.native_index_by_overlay_index,
                    captured_debug_ids,
                    captured_root_properties,
                    path_segments_by_ref: Arc::clone(&path_segments_by_ref),
                    path_ordinals_by_ref: Arc::clone(&path_ordinals_by_ref),
                },
            )
            .is_some()
        {
            bail!("Native serialization batch duplicated {}", group.service);
        }
    }
    if source_instances.next().is_some() {
        bail!("Native serialization batch contains trailing instances");
    }
    Ok(doms)
}

fn match_native_capture_overlay(
    native: &mut NativeServiceDom,
    debug_ids: &[Option<String>],
) -> Result<()> {
    let Some(captured) = native.captured_debug_ids.take() else {
        return Ok(());
    };
    anyhow::ensure!(
        debug_ids.len() == captured.len(),
        "Native capture overlay identity count changed"
    );
    let mut by_id = captured
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let indices = debug_ids
        .iter()
        .map(|id| {
            let id = id
                .as_deref()
                .context("Native capture overlay omitted an identity")?;
            by_id
                .remove(id)
                .context("Native capture overlay contains an unknown or duplicate identity")
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(by_id.is_empty(), "Native capture overlay omitted instances");
    native.native_index_by_overlay_index = indices;
    Ok(())
}

fn native_overlay_reference_debug_id(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    (object.get("_type").and_then(Value::as_str) == Some("Ref"))
        .then(|| object.get("debugId").and_then(Value::as_str))
        .flatten()
}

fn normalize_native_overlay_internal_references<'a>(
    overlays: &mut [NativeOverlayItem],
    root_properties: Option<&mut Map<String, Value>>,
    debug_ids: impl IntoIterator<Item = Option<&'a str>>,
) {
    let requested = overlays
        .iter()
        .flat_map(|overlay| overlay.properties.values())
        .chain(root_properties.as_deref().into_iter().flat_map(Map::values))
        .filter_map(native_overlay_reference_debug_id)
        .collect::<HashSet<_>>();
    if requested.is_empty() {
        return;
    }
    let internal_indices = debug_ids
        .into_iter()
        .enumerate()
        .filter_map(|(index, debug_id)| {
            let debug_id = debug_id?;
            requested
                .contains(debug_id)
                .then(|| (debug_id.to_string(), index + 1))
        })
        .collect::<HashMap<_, _>>();
    drop(requested);
    if internal_indices.is_empty() {
        return;
    }
    for value in overlays
        .iter_mut()
        .flat_map(|overlay| overlay.properties.values_mut())
        .chain(root_properties.into_iter().flat_map(Map::values_mut))
    {
        let Some(instance_index) = native_overlay_reference_debug_id(value)
            .and_then(|debug_id| internal_indices.get(debug_id))
            .copied()
        else {
            continue;
        };
        *value = json!({
            "_type": "Ref",
            "instanceIndex": instance_index,
        });
    }
}

fn reorder_overlay_items<T>(
    items: Vec<T>,
    native_index_by_overlay_index: &[usize],
    label: &str,
) -> Result<Vec<T>> {
    if items.len() != native_index_by_overlay_index.len() {
        bail!("Native {label} index map has the wrong length");
    }
    let mut reordered = Vec::with_capacity(items.len());
    reordered.resize_with(items.len(), || None);
    for (overlay_index, item) in items.into_iter().enumerate() {
        let slot = native_index_by_overlay_index
            .get(overlay_index)
            .and_then(|index| reordered.get_mut(*index))
            .with_context(|| format!("Native {label} index map is out of range"))?;
        if slot.replace(item).is_some() {
            bail!("Native {label} index map contains a duplicate index");
        }
    }
    reordered
        .into_iter()
        .map(|item| item.with_context(|| format!("Native {label} index map is incomplete")))
        .collect()
}

fn remap_native_overlay_items(
    items: &mut [NativeOverlayItem],
    native_index_by_overlay_index: &[usize],
) -> Result<()> {
    for item in items {
        let overlay_index = item
            .instance_index
            .checked_sub(1)
            .context("Native overlay instance index is zero")?;
        item.instance_index = native_index_by_overlay_index
            .get(overlay_index)
            .copied()
            .context("Native overlay instance index is out of range")?
            + 1;
    }
    Ok(())
}

fn remap_script_source_indices(
    source_map: &mut SourceBatchMap,
    native_index_by_overlay_index: &[usize],
) -> Result<()> {
    let by_overlay_index = std::mem::take(&mut source_map.by_index);
    source_map.by_index.reserve(by_overlay_index.len());
    for (overlay_index, source) in by_overlay_index {
        let native_index = overlay_index
            .checked_sub(1)
            .and_then(|index| native_index_by_overlay_index.get(index))
            .copied()
            .context("Script source instance index is out of range")?
            + 1;
        if source_map.by_index.insert(native_index, source).is_some() {
            bail!("Script source identity map contains a duplicate index");
        }
    }
    Ok(())
}

fn convert_native_service_output(
    dependencies: &NativeServiceFinishDependencies<'_, '_>,
    group: &EditorBinaryExportGroup,
    native: NativeServiceDom,
    mut overlay_instances: Vec<NativeOverlayItem>,
    debug_ids: Vec<Option<String>>,
    settings_ids: Vec<(usize, String)>,
    export_started_ms: f64,
) -> Result<(ServiceExportOutput, Vec<usize>)> {
    let NativeServiceDom {
        instances: native_instances,
        new_index_by_dense_ref,
        native_index_by_overlay_index,
        path_segments_by_ref,
        path_ordinals_by_ref,
        captured_debug_ids: _,
        captured_root_properties,
    } = native;
    if debug_ids.len() != native_instances.len() {
        bail!(
            "Native debug ids contain {} {} instances; expected {}",
            debug_ids.len(),
            group.service,
            native_instances.len()
        );
    }
    let debug_ids = reorder_overlay_items(debug_ids, &native_index_by_overlay_index, "debug id")?;
    let mut root_properties = group.root_properties.clone();
    root_properties.extend(captured_root_properties);
    normalize_native_overlay_internal_references(
        &mut overlay_instances,
        Some(&mut root_properties),
        debug_ids.iter().map(|debug_id| debug_id.as_deref()),
    );
    let new_index_by_dense_ref =
        new_index_by_dense_ref.context("Native snapshot omitted its dense referent map")?;
    let mut settings_id_by_dense_index = vec![None; native_instances.len()];
    for (index, settings_id) in settings_ids {
        let slot = settings_id_by_dense_index
            .get_mut(index)
            .context("Native settings id index is out of range")?;
        if slot.replace(settings_id).is_some() {
            bail!("Native settings id index {} is duplicated", index + 1);
        }
    }
    let settings_id_by_index = reorder_overlay_items(
        settings_id_by_dense_index,
        &native_index_by_overlay_index,
        "settings id",
    )?;
    let mut non_archivable_by_dense_index = vec![false; native_instances.len()];
    for &instance_index in &group.non_archivable_indices {
        let index = instance_index
            .checked_sub(1)
            .filter(|index| *index < non_archivable_by_dense_index.len())
            .context("Native non-Archivable instance index is out of range")?;
        if std::mem::replace(&mut non_archivable_by_dense_index[index], true) {
            bail!("Native non-Archivable instance index {instance_index} is duplicated");
        }
    }
    let non_archivable_by_index = reorder_overlay_items(
        non_archivable_by_dense_index,
        &native_index_by_overlay_index,
        "non-Archivable",
    )?;
    let mut overlay_by_dense_index = Vec::with_capacity(native_instances.len());
    overlay_by_dense_index.resize_with(native_instances.len(), || None);
    for overlay in overlay_instances {
        let index = overlay
            .instance_index
            .checked_sub(1)
            .filter(|index| *index < overlay_by_dense_index.len())
            .context("Native overlay instance index is out of range")?;
        if overlay_by_dense_index[index].replace(overlay).is_some() {
            bail!("Native overlay instance index {} is duplicated", index + 1);
        }
    }
    let overlay_by_index = reorder_overlay_items(
        overlay_by_dense_index,
        &native_index_by_overlay_index,
        "overlay",
    )?;
    let refs = BytecodeModelImportRefs {
        settings_id_by_ref: HashMap::new(),
        path_segments_by_ref,
        path_ordinals_by_ref,
        new_index_by_ref: HashMap::new(),
        new_index_by_dense_ref: Some(new_index_by_dense_ref),
        path_segments_by_index: Vec::new(),
    };
    let conversion_started = Instant::now();
    let converted = native_instances
        .into_par_iter()
        .zip(overlay_by_index.into_par_iter())
        .zip(debug_ids.into_par_iter())
        .zip(settings_id_by_index.into_par_iter())
        .enumerate()
        .map(
            |(index, (((rbx_instance, overlay), debug_id), transported_settings_id))| -> Result<_> {
                let parent_index = rbx_instance.parent_index.map(|parent| parent + 1);
                let native_filter = dependencies.native_filters.get(rbx_instance.class.as_str());
                let (mut native_properties, mut properties, mut attributes, source) =
                    rbx_properties_to_native_settings_records(
                        rbx_instance.class.as_str(),
                        rbx_instance
                            .properties
                            .iter()
                            .map(|(property_name, variant)| (property_name, variant)),
                        dependencies.database,
                        &refs,
                        native_filter,
                    );
                if native_filter.is_some_and(|filter| filter.reconstruct_decal_color_map) {
                    let value = properties
                        .get("TextureContent")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new()));
                    properties.insert("ColorMapContent".to_string(), value);
                }
                if native_filter.is_some_and(|filter| filter.reconstruct_weld_enabled)
                    && let Some(raw_state) = properties.remove("State")
                {
                    let state =
                        json_i64(&raw_state).context("WeldConstraint.State was not an integer")?;
                    native_properties.push(NativeSettingsProperty {
                        name: "State".to_string(),
                        value: NativeSettingsValue::Int(state),
                    });
                    if state == 0 {
                        properties.insert("Enabled".to_string(), Value::Bool(false));
                    }
                }
                if index == 0 {
                    native_properties.clear();
                    // Native place capture includes engine migration metadata.
                    // Match the existing service snapshot marker's attribute policy.
                    attributes.retain(|name, _| !name.starts_with("RBX"));
                    let tags = properties.remove("Tags");
                    properties.clone_from(&root_properties);
                    if let Some(tags) = tags {
                        properties.insert("Tags".to_string(), tags);
                    }
                }
                if let Some(source) = source {
                    properties.insert("Source".to_string(), Value::String(source));
                }
                if let Some(overlay) = overlay {
                    let overlay_class = group
                        .class_names
                        .get(overlay.class_index)
                        .context("Native overlay class index is out of range")?;
                    if overlay_class.as_str() != rbx_instance.class.as_str() {
                        let native_unique_id = rbx_instance
                            .properties
                            .iter()
                            .find(|(name, _)| name.as_str() == "UniqueId")
                            .map(|(_, value)| format!("{value:?}"));
                        bail!(
                            "Native snapshot and overlay disagree at {} flat instance {}: native class {}, overlay class {}, overlay instance {}, native unique id {:?}, overlay debug id {:?}",
                            group.service,
                            index + 1,
                            rbx_instance.class,
                            overlay_class,
                            overlay.instance_index,
                            native_unique_id,
                            debug_id,
                        );
                    }
                    let mut overlay_properties = overlay.properties;
                    overlay_properties.remove("Source");
                    if !overlay_properties.is_empty() {
                        native_properties.retain(|property| {
                            !overlay_properties.contains_key(property.name.as_str())
                        });
                    }
                    properties.extend(overlay_properties);
                    attributes.extend(overlay.attributes);
                }
                if non_archivable_by_index[index] {
                    native_properties.retain(|property| property.name != "Archivable");
                    properties.insert("Archivable".to_string(), Value::Bool(false));
                }
                Ok((
                    SnapshotInstance {
                        name: rbx_instance.name,
                        class_name: rbx_instance.class,
                        properties,
                        attributes,
                        debug_id: debug_id.filter(|value| !value.is_empty()),
                        transported_settings_id,
                        instance_index: Some(index + 1),
                        parent_index,
                        ..Default::default()
                    },
                    native_properties,
                ))
            },
        )
        .collect::<Result<Vec<_>>>()?;
    let (instances, native_properties_by_instance): (
        Vec<SnapshotInstance>,
        Vec<Vec<NativeSettingsProperty>>,
    ) = converted.into_iter().unzip();
    log_timing(
        &format!("{}: native instance conversion", group.service),
        conversion_started,
    );
    let export_end_ms = elapsed_ms(dependencies.run_started);
    Ok((
        ServiceExportOutput {
            parts: ExportedSnapshotParts {
                class_defaults: Value::Object(Map::new()),
                instances,
                native_properties_by_instance: Some(native_properties_by_instance),
            },
            span: ServiceExecutionSpan {
                service: group.service.clone(),
                export_start_ms: export_started_ms,
                export_end_ms,
            },
            tune: None,
        },
        native_index_by_overlay_index,
    ))
}

pub(crate) fn editor_binary_export_parts<'a>(
    bridge: &'a BridgeServer,
    requested_services: &[String],
    run_started: Instant,
    on_output: &mut (impl FnMut(ServiceExportOutput) -> Result<()> + Send),
    on_serialization_complete: &mut (impl FnMut() -> Result<()> + Send),
) -> Result<EditorBinaryExportFinishGuard<'a>> {
    let export_started_ms = elapsed_ms(run_started);
    let begin_started = Instant::now();
    let export = begin_editor_binary_export_for_runtime(
        bridge,
        true,
        Some(requested_services),
        Some(requested_services),
        false,
        None,
        false,
    )?;
    let finish_guard = EditorBinaryExportFinishGuard {
        bridge,
        export_id: export.export_id.clone(),
        #[cfg(windows)]
        attribute_guard: export.attribute_guard,
    };
    log_timing("native editor export begin", begin_started);

    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let native_filters = export
        .property_schema_by_class
        .keys()
        .map(|class_name| {
            (
                class_name.clone(),
                native_property_filter(database, class_name),
            )
        })
        .collect::<HashMap<_, _>>();
    let native_decode_filter = Arc::new(HashMap::new());
    let (overlay_schema, direct_overlay_schema, conditional_ref_schema) =
        native_overlay_property_schemas(
            database,
            &export.property_schema_by_class,
            &native_filters,
        );
    if verbose_timing_logs() {
        let property_count = overlay_schema.values().map(Vec::len).sum::<usize>();
        let mesh_part = overlay_schema
            .get("MeshPart")
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        println!(
            "[renium] native editor overlay schema: classes={}, properties={}, MeshPart=[{}]",
            overlay_schema.len(),
            property_count,
            mesh_part
        );
    }
    let stream_started = Instant::now();
    let export_id = export
        .export_id
        .as_deref()
        .context("Native export id is missing")?;
    let overlay_names = overlay_property_names_value(&overlay_schema, &native_filters);
    let direct_overlay_names =
        overlay_property_names_value(&direct_overlay_schema, &native_filters);
    let requested_groups = requested_services
        .iter()
        .map(|service| {
            export
                .groups
                .iter()
                .find(|group| group.service == *service)
                .with_context(|| format!("Native export omitted {service}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let worker_count = std::env::var("RENIUM_NATIVE_SERVICE_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| bridge.channel_count())
        .max(1)
        .min(bridge.channel_count().max(1))
        .min(requested_groups.len().max(1));
    let serialization_batch_by_service = export
        .serialization_batches
        .iter()
        .flat_map(|batch| {
            batch
                .services
                .iter()
                .map(move |service| (service.as_str(), batch))
        })
        .collect::<HashMap<_, _>>();
    let native_serialization_batches = export
        .serialization_batches
        .iter()
        .map(|batch| {
            (
                batch.id.as_str(),
                OnceLock::<Result<Mutex<HashMap<String, NativeServiceDom>>, String>>::new(),
            )
        })
        .collect::<HashMap<_, _>>();
    #[cfg(windows)]
    let captured_native_services =
        OnceLock::<Result<Mutex<HashMap<String, NativeServiceDom>>, String>>::new();
    let native_capture = export.native_capture;
    let fetch_overlay = |group: &EditorBinaryExportGroup| {
        let selective_refs = group.instance_count >= NATIVE_SERIALIZATION_SERVICE_LIMIT
            && group
                .class_names
                .iter()
                .any(|class_name| conditional_ref_schema.contains_key(class_name));
        let _trace = crate::app::timing::trace_scope("native.export", "fetch overlay");
        fetch_native_overlay_batches(
            bridge,
            NativeOverlayRequest {
                service: &group.service,
                start_index: 1,
                take_count: group.instance_count,
                instance_count: group.instance_count,
                overlay_id: export_id,
                overlay_variant: if selective_refs { "direct" } else { "combined" },
                include_debug_ids: true,
                overlay_names: if selective_refs {
                    &direct_overlay_names
                } else {
                    &overlay_names
                },
                overlay_schema: if selective_refs {
                    &direct_overlay_schema
                } else {
                    &overlay_schema
                },
                enum_value_names_by_type: &export.enum_value_names_by_type,
                class_names: &group.class_names,
            },
        )
    };
    // The whole capture has no per-service transport to overlap. Keep the
    // existing bounded overlay workers busy instead of waiting for that capture
    // after each service. Receivers retain errors and the export guard's lifetime.
    let mut overlay_queue = VecDeque::new();
    let mut overlay_receivers = HashMap::new();
    if native_capture {
        for group in &requested_groups {
            let (sender, receiver) = mpsc::sync_channel::<Result<NativeOverlayFetch>>(1);
            overlay_queue.push_back((*group, sender));
            overlay_receivers.insert(group.service.as_str(), Mutex::new(receiver));
        }
    }
    let overlay_queue = Mutex::new(overlay_queue);
    let mut batched_groups = requested_groups
        .iter()
        .filter(|group| {
            group.instance_count < NATIVE_SERIALIZATION_SERVICE_LIMIT
                && !serialization_batch_by_service.contains_key(group.service.as_str())
        })
        .copied()
        .collect::<Vec<_>>();
    if native_capture || batched_groups.len() < 2 {
        batched_groups.clear();
    }
    let batched_service_names = batched_groups
        .iter()
        .map(|group| group.service.clone())
        .collect::<Vec<_>>();
    let batched_service_set = batched_service_names
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let native_binary_batches = OnceLock::<Result<Arc<NativeBinaryBatches>, String>>::new();
    let finish_dependencies = NativeServiceFinishDependencies {
        bridge,
        export_id,
        enum_value_names_by_type: &export.enum_value_names_by_type,
        database,
        native_filters: &native_filters,
        run_started,
    };
    let (sender, receiver) = mpsc::channel::<Result<NativeServiceExportResult>>();
    let receiver = Mutex::new(receiver);
    let mut metrics = ChunkFetchMetrics::default();
    let mut compact_expand_ms = 0.0;
    let serialization_complete_signal = AtomicBool::new(false);
    let mut serialization_complete = false;
    let priority_worker_count = if !native_capture
        && worker_count == 4
        && requested_groups
            .first()
            .is_some_and(|group| group.instance_count >= 25_000)
    {
        2
    } else {
        worker_count
    };
    let priority_groups = requested_groups
        .iter()
        .take(priority_worker_count)
        .copied()
        .collect::<Vec<_>>();
    let service_queue = Mutex::new(
        requested_groups
            .into_iter()
            .skip(priority_worker_count)
            .collect::<VecDeque<_>>(),
    );
    let priority_gate = NativePriorityWorkerGate::new(priority_worker_count == worker_count);
    let trace_context = crate::app::timing::trace_context();
    thread::scope(|overlay_scope| {
        // Identity and binary reads share the existing export fence. Start both
        // before waiting so a queued native identity read does not idle all the
        // binary/overlay workers. No output is published until identity succeeds.
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let identity_worker = overlay_scope.spawn(move || {
            let _trace_context =
                trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
            let _trace =
                crate::app::timing::trace_scope("native.export", "capture persistent identities");
            let pid = studio_pid_for_bridge(bridge)?;
            let title = studio_title_for_bridge(bridge, pid)?;
            serializer::capture_identities(pid, &title, requested_services, Duration::from_secs(3))
        });
        if native_capture {
            for _ in 0..worker_count {
                let overlay_queue = &overlay_queue;
                let fetch_overlay = &fetch_overlay;
                overlay_scope.spawn(move || {
                    let _trace_context = trace_context
                        .map(|context| crate::app::timing::enter_trace_context(Some(context)));
                    loop {
                        let next = overlay_queue
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .pop_front();
                        let Some((group, sender)) = next else {
                            break;
                        };
                        let _ = sender.send(fetch_overlay(group));
                    }
                });
            }
        }
        rayon::scope_fifo(|scope| -> Result<()> {
            for worker_index in 0..worker_count {
                let sender = sender.clone();
                let fetch_overlay = &fetch_overlay;
                let overlay_receivers = &overlay_receivers;
                let conditional_ref_schema = &conditional_ref_schema;
                let enum_value_names_by_type = &export.enum_value_names_by_type;
                let native_decode_filter = &native_decode_filter;
                let priority_groups = &priority_groups;
                let service_queue = &service_queue;
                let priority_gate = &priority_gate;
                let batched_service_names = &batched_service_names;
                let batched_service_set = &batched_service_set;
                let native_binary_batches = &native_binary_batches;
                let serialization_batch_by_service = &serialization_batch_by_service;
                let native_serialization_batches = &native_serialization_batches;
                #[cfg(windows)]
                let captured_native_services = &captured_native_services;
                let serialization_complete_signal = &serialization_complete_signal;
                let service_groups = &export.groups;
                let finish_dependencies = &finish_dependencies;
                scope.spawn_fifo(move |_| {
                    let _trace_context = trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
                    if worker_index >= priority_worker_count {
                        let _wait = crate::app::timing::trace_scope("wait", "native export priority gate");
                        priority_gate.wait();
                    }
                    let mut priority_release = OnDrop::new(|| {
                        if priority_worker_count < worker_count && worker_index == 0 {
                            priority_gate.release();
                        }
                    });
                    let mut priority_service = priority_groups.get(worker_index).copied();
                    loop {
                        let service = priority_service.take().or_else(|| {
                            service_queue
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .pop_front()
                        });
                        let Some(group) = service else {
                            break;
                        };
                        let _service_trace = crate::app::timing::trace_scope("native.export", &group.service);
                        let result = thread::scope(|reference_scope| -> Result<NativeServiceExportResult> {
                        // Root reads use the same active export guard, but must
                        // not hold up unrelated binary decoding and file writes.
                        #[cfg(any(windows, target_os = "macos"))]
                        let root_capture = (!native_capture && !crate::editor::native_roots::capture_properties(&group.service).is_empty())
                            .then(|| reference_scope.spawn(move || -> Result<EditorBinaryExportGroup> {
                                let _trace_context = trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
                                let mut captured = group.clone();
                                capture_native_service_root_properties(bridge, None, std::slice::from_mut(&mut captured))?;
                                Ok(captured)
                            }));
                        let selective_refs =
                            group.instance_count >= NATIVE_SERIALIZATION_SERVICE_LIMIT
                            && group.class_names.iter().any(|class_name| {
                                conditional_ref_schema.contains_key(class_name)
                            });
                        let (reference_sender, reference_receiver) = mpsc::sync_channel(1);
                        let (native, overlay) = rayon::join(
                            || -> Result<NativeServiceFetch> {
                                let _trace_context = trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
                                let _trace = crate::app::timing::trace_scope("native.export", "fetch and decode binary");
                                let (native, one_chunk) = if native_capture {
                                    #[cfg(windows)]
                                    {
                                        let captured = captured_native_services.get_or_init(|| {
                                            (|| -> Result<_> {
                                                let info = bridge.cached_bridge_info_for_target(crate::studio::bridge::BridgeTarget::Edit)?;
                                                let pid = bridge.studio_pid_for_runtime(crate::studio::bridge::BridgeTarget::Edit, &info.runtime_id)?;
                                                let services = service_groups.iter().map(|group| group.service.clone()).collect::<Vec<_>>();
                                                let captured = serializer::capture_live_services(pid,
                                                    &info.place_name, &services, Duration::from_secs(15))?;
                                                serialization_complete_signal.store(true, Ordering::Release);
                                                let batch = EditorBinarySerializationBatch { id: "native-capture".into(), services };
                                                decode_native_serialization_batch(&captured.bytes, &batch, service_groups,
                                                    Arc::clone(native_decode_filter), Some(&captured.identities)).map(Mutex::new)
                                            })().map_err(|error| format!("{error:#}"))
                                        });
                                        let captured = match captured { Ok(captured) => captured, Err(error) => bail!("{error}") };
                                        let native = captured.lock().unwrap_or_else(PoisonError::into_inner).remove(&group.service)
                                            .with_context(|| format!("Native capture omitted {}", group.service))?;
                                        (native, false)
                                    }
                                    #[cfg(not(windows))]
                                    { bail!("Native service capture is unavailable on this platform"); }
                                } else if let Some(batch) =
                                    serialization_batch_by_service.get(group.service.as_str())
                                {
                                    let batch_doms = native_serialization_batches
                                        .get(batch.id.as_str())
                                        .context("Native serialization batch state is missing")?
                                        .get_or_init(|| {
                                            (|| -> Result<_> {
                                                let bytes = receive_editor_binary_export_bytes(
                                                    bridge,
                                                    export_id,
                                                    Some(&batch.id),
                                                    Some(serialization_complete_signal),
                                                )?;
                                                decode_native_serialization_batch(
                                                    &bytes,
                                                    batch,
                                                    service_groups,
                                                    Arc::clone(native_decode_filter),
                                                    None,
                                                )
                                                .map(Mutex::new)
                                            })()
                                            .map_err(|error| format!("{error:#}"))
                                        });
                                    let batch_doms = match batch_doms {
                                        Ok(batch_doms) => batch_doms,
                                        Err(error) => bail!("{error}"),
                                    };
                                    let native = batch_doms
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner)
                                        .remove(&group.service)
                                    .with_context(|| {
                                        format!(
                                            "Native serialization batch {} omitted {}",
                                            batch.id, group.service
                                        )
                                    })
                                    ?;
                                    (native, false)
                                } else if batched_service_set.contains(group.service.as_str()) {
                                    let batches = native_binary_batches.get_or_init(|| {
                                        receive_editor_binary_export_batches(
                                            bridge,
                                            export_id,
                                            batched_service_names,
                                            Some(serialization_complete_signal),
                                        )
                                        .map(Arc::new)
                                        .map_err(|error| format!("{error:#}"))
                                    });
                                    let batches = match batches {
                                        Ok(batches) => batches,
                                        Err(error) => bail!("{error}"),
                                    };
                                    let part =
                                        batches.parts.get(&group.service).with_context(|| {
                                            format!(
                                                "Native binary batch omitted {}",
                                                group.service
                                            )
                                        })?;
                                    (
                                        decode_native_service_dom(
                                            &part.bytes[part.start..part.end],
                                            group,
                                            Arc::clone(native_decode_filter),
                                        )?,
                                        false,
                                    )
                                } else {
                                    let bytes = receive_editor_binary_export_bytes(
                                        bridge,
                                        export_id,
                                        Some(&group.service),
                                        Some(serialization_complete_signal),
                                    )?;
                                    let one_chunk = bytes.len() <= native_binary_chunk_bytes();
                                    (
                                        decode_native_service_dom(
                                            &bytes,
                                            group,
                                            Arc::clone(native_decode_filter),
                                        )?,
                                        one_chunk,
                                    )
                                };
                                let reference_request = (selective_refs && !native_capture)
                                    .then(|| {
                                        conditional_ref_overlay_request(
                                            &native.instances,
                                            conditional_ref_schema,
                                            &native.native_index_by_overlay_index,
                                        )
                                    })
                                    .filter(|request| request.2 > 0);
                                let reference_prefetched =
                                    one_chunk && reference_request.is_some();
                                if reference_prefetched {
                                    let request = reference_request
                                        .clone()
                                        .context("Native conditional-reference request is missing")?;
                                    reference_scope.spawn(move || {
                                        let _trace_context = trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
                                        let _trace = crate::app::timing::trace_scope("native.export", "conditional references");
                                        let result = fetch_native_conditional_overlay(
                                            bridge,
                                            export_id,
                                            group,
                                            enum_value_names_by_type,
                                            request,
                                        );
                                        let _ = reference_sender.send(result);
                                    });
                                }
                                Ok((
                                    native,
                                    reference_prefetched,
                                    (!reference_prefetched)
                                        .then_some(reference_request)
                                        .flatten(),
                                ))
                            },
                            || {
                                let _trace_context = trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
                                if let Some(receiver) = overlay_receivers.get(group.service.as_str()) {
                                    let _wait = crate::app::timing::trace_scope("wait", "prefetched native overlay");
                                    receiver.lock().unwrap_or_else(PoisonError::into_inner).recv()
                                        .context("Native overlay worker ended without a result")?
                                } else {
                                    fetch_overlay(group)
                                }
                            },
                        );
                        if let Ok(overlay) = &overlay && verbose_timing_logs() {
                            println!(
                                "[renium] timing: native editor {} overlay fetch took {:.1}ms -> bytes={}, chunks={}, parse_ms={:.1}, expand_ms={:.1}",
                                group.service,
                                overlay.request_ms,
                                overlay.metrics.bytes,
                                overlay.metrics.chunks,
                                overlay.metrics.json_parse_ms,
                                overlay.compact_expand_ms
                            );
                        }
                        let (mut native, reference_prefetched, mut reference_request) = native?;
                        let mut overlay = overlay?;
                        let debug_ids = std::mem::take(&mut overlay.debug_ids);
                        if native_capture {
                            match_native_capture_overlay(&mut native, &debug_ids)?;
                            reference_request = selective_refs.then(|| conditional_ref_overlay_request(
                                &native.instances, conditional_ref_schema, &native.native_index_by_overlay_index))
                                .filter(|request| request.2 > 0);
                        }
                        let settings_ids = std::mem::take(&mut overlay.settings_ids);
                        #[cfg(any(windows, target_os = "macos"))]
                        let captured_group = root_capture.map(|task| task.join().expect("native root capture panicked")).transpose()?;
                        #[cfg(any(windows, target_os = "macos"))]
                        let group = captured_group.as_ref().unwrap_or(group);
                        let mut result = finish_native_service_export(
                            finish_dependencies,
                            group,
                            NativeServiceFinishInput {
                            native,
                            debug_ids,
                            settings_ids,
                            overlay,
                            reference_prefetch: reference_prefetched
                                .then_some(reference_receiver),
                            reference_request,
                            export_started_ms,
                        },
                        )?;
                        if group.script_count > 0 {
                            let worker_count = resolve_source_worker_count(
                                0,
                                bridge.channel_count(),
                                group.script_count,
                                group.instance_count,
                            );
                            let mut sources = fetch_script_sources(
                                bridge,
                                &group.service,
                                DEFAULT_EXPORT_CHUNK_SIZE,
                                group.script_count,
                                worker_count,
                                Some(export_id),
                            )?;
                            remap_script_source_indices(
                                &mut sources,
                                &result.native_index_by_overlay_index,
                            )?;
                            merge_script_sources(&mut result.output.parts.instances, &sources);
                        }
                        Ok(result)
                        });
                        priority_release.run();
                        if sender.send(result).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(sender);
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            let persistent_identities = identity_worker
                .join()
                .map_err(|_| anyhow::anyhow!("Native identity capture worker panicked"))??;
            for _ in 0..requested_services.len() {
                let result = receiver
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Native service export receiver was poisoned"))?
                    .recv()
                    .context("Native service export worker closed")??;
                merge_chunk_fetch_metrics(&mut metrics, result.metrics);
                compact_expand_ms += result.compact_expand_ms;
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                let result = {
                    let mut result = result;
                    for instance in &mut result.output.parts.instances {
                        let id = instance
                            .debug_id
                            .as_deref()
                            .and_then(|id| persistent_identities.get(id))
                            .context("Exported instance is missing from native identity capture")?;
                        // Use the existing metadata codec. The final export guard
                        // covers the whole interval from identity read to publication.
                        instance.properties.insert(
                            "UniqueId".into(),
                            json!({"_type": "UniqueId", "value": id.to_string()}),
                        );
                    }
                    result
                };
                on_output(result.output)?;
                if !serialization_complete && serialization_complete_signal.load(Ordering::Acquire)
                {
                    serialization_complete = true;
                    on_serialization_complete()?;
                    if verbose_timing_logs() {
                        println!(
                            "[renium] native editor serialization complete at {:.1}ms",
                            elapsed_ms(run_started)
                        );
                    }
                }
            }
            Ok(())
        })
    })?;
    log_chunk_fetch_metrics("native editor overlay payloads", metrics);
    log_timing_ms("native editor overlay compact expansion", compact_expand_ms);
    log_timing("native editor streaming export", stream_started);
    Ok(finish_guard)
}

#[cfg(any(windows, target_os = "macos"))]
fn rbx_dom_path_export_refs(dom: &RbxWeakDom) -> BytecodeModelExportRefs<'static> {
    let mut refs_preorder = Vec::new();
    for referent in rbx_model_top_level_refs(dom) {
        collect_rbx_subtree_preorder(dom, referent, &mut refs_preorder);
    }
    let mut by_path_key = HashMap::with_capacity(refs_preorder.len());
    let mut by_path_segments_key = HashMap::with_capacity(refs_preorder.len());
    for referent in refs_preorder {
        let (segments, ordinals) = rbx_dom_instance_path_parts(dom, referent);
        insert_unique_rbx_path(
            &mut by_path_segments_key,
            instance_path_key(&segments),
            referent,
        );
        by_path_key.insert(instance_path_parts_key(&segments, &ordinals), referent);
    }
    BytecodeModelExportRefs {
        by_path_key,
        by_path_segments_key,
        ..Default::default()
    }
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn rbx_dom_service_root_property_values(
    dom: &RbxWeakDom,
    service_names: &HashSet<String>,
    database: &ReflectionDatabase<'_>,
) -> HashMap<String, Map<String, Value>> {
    let refs = rbx_dom_path_import_refs(dom, true);
    let mut result = HashMap::new();
    for referent in rbx_model_top_level_refs(dom) {
        let Some(instance) = dom.get_by_ref(referent) else {
            continue;
        };
        let service = if service_names.contains(instance.class.as_str()) {
            instance.class.to_string()
        } else if service_names.contains(&instance.name) {
            instance.name.clone()
        } else {
            continue;
        };
        let mut values = Map::new();
        for (name, value) in &instance.properties {
            if matches!(name.as_str(), "Attributes" | "Tags") {
                continue;
            }
            let descriptor =
                rbx_model_property_descriptor(database, instance.class.as_str(), name.as_str());
            if let Some(value) = rbx_variant_to_settings_json(value, descriptor, database, &refs) {
                values.insert(name.to_string(), value);
            }
        }
        result.insert(service, values);
    }
    result
}

fn fetch_native_conditional_overlay(
    bridge: &BridgeServer,
    export_id: &str,
    group: &EditorBinaryExportGroup,
    enum_value_names_by_type: &EnumValueNameMap,
    request: NativeConditionalOverlayRequest,
) -> Result<Option<NativeConditionalOverlayFetch>> {
    let (reference_schema, reference_names, candidate_count) = request;
    if candidate_count == 0 {
        return Ok(None);
    }
    let overlay = fetch_native_overlay_batches(
        bridge,
        NativeOverlayRequest {
            service: &group.service,
            start_index: 1,
            take_count: group.instance_count,
            instance_count: group.instance_count,
            overlay_id: export_id,
            overlay_variant: "conditional-references",
            include_debug_ids: false,
            overlay_names: &reference_names,
            overlay_schema: &reference_schema,
            enum_value_names_by_type,
            class_names: &group.class_names,
        },
    )?;
    Ok(Some(NativeConditionalOverlayFetch {
        candidate_count,
        overlay,
    }))
}

fn finish_native_service_export(
    dependencies: &NativeServiceFinishDependencies<'_, '_>,
    group: &EditorBinaryExportGroup,
    input: NativeServiceFinishInput,
) -> Result<NativeServiceExportResult> {
    let NativeServiceFinishInput {
        mut native,
        debug_ids,
        settings_ids,
        overlay,
        reference_prefetch,
        reference_request,
        export_started_ms,
    } = input;
    match_native_capture_overlay(&mut native, &debug_ids)?;
    let mut service_metrics = overlay.metrics;
    let mut service_compact_expand_ms = overlay.compact_expand_ms;
    let has_reference_work = reference_prefetch.is_some() || reference_request.is_some();
    let trace_context = crate::app::timing::trace_context();
    let convert = move || {
        let _trace_context =
            trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
        let _trace = crate::app::timing::trace_scope("native.export", "convert service");
        convert_native_service_output(
            dependencies,
            group,
            native,
            overlay.items,
            debug_ids,
            settings_ids,
            export_started_ms,
        )
    };
    let (output, native_index_by_overlay_index) = if has_reference_work {
        let (output, reference_overlay) = rayon::join(convert, || {
            let _trace_context =
                trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
            let _trace =
                crate::app::timing::trace_scope("native.export", "complete conditional references");
            if let Some(receiver) = reference_prefetch {
                return receiver
                    .recv()
                    .context("Native conditional-reference prefetch worker closed")?;
            }
            fetch_native_conditional_overlay(
                dependencies.bridge,
                dependencies.export_id,
                group,
                dependencies.enum_value_names_by_type,
                reference_request.context("Native conditional-reference request is missing")?,
            )
        });
        let (mut output, native_index_by_overlay_index) = output?;
        if let Some(reference_fetch) = reference_overlay? {
            let reference_overlay = reference_fetch.overlay;
            if verbose_timing_logs() {
                println!(
                    "[renium] timing: native editor {} conditional reference overlay took {:.1}ms -> candidates={}, bytes={}, chunks={}",
                    group.service,
                    reference_overlay.request_ms,
                    reference_fetch.candidate_count,
                    reference_overlay.metrics.bytes,
                    reference_overlay.metrics.chunks
                );
            }
            let mut items = reference_overlay.items;
            remap_native_overlay_items(&mut items, &native_index_by_overlay_index)?;
            normalize_native_overlay_internal_references(
                &mut items,
                None,
                output
                    .parts
                    .instances
                    .iter()
                    .map(|instance| instance.debug_id.as_deref()),
            );
            merge_native_overlay_items(&mut output.parts.instances, items, &group.class_names)?;
            merge_chunk_fetch_metrics(&mut service_metrics, reference_overlay.metrics);
            service_compact_expand_ms += reference_overlay.compact_expand_ms;
        }
        (output, native_index_by_overlay_index)
    } else {
        convert()?
    };
    Ok(NativeServiceExportResult {
        output,
        native_index_by_overlay_index,
        metrics: service_metrics,
        compact_expand_ms: service_compact_expand_ms,
    })
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn read_place_service_root_property_values(
    path: &Path,
    service_names: &HashSet<String>,
    database: &ReflectionDatabase<'_>,
) -> Result<HashMap<String, Map<String, Value>>> {
    let dom = RbxPlaceFormat::from_path(path)?.read(path)?;
    Ok(rbx_dom_service_root_property_values(
        &dom,
        service_names,
        database,
    ))
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn read_live_service_root_property_values(
    bridge: &BridgeServer,
    service: &str,
    database: &ReflectionDatabase<'_>,
) -> Result<Map<String, Value>> {
    let pid = studio_pid_for_bridge(bridge)?;
    let title = studio_title_for_bridge(bridge, pid)?;
    let path = std::env::temp_dir().join("renium-native").join(format!(
        ".renium-service-{pid}-{}-{}.rbxl",
        sanitize_name(service),
        current_millis()
    ));
    let started = Instant::now();
    let result = (|| -> Result<Map<String, Value>> {
        let snapshot = serializer::write_live_service(pid, &title, service, &path)?;
        let service_names = HashSet::from([service.to_string()]);
        let mut values = read_place_service_root_property_values(&path, &service_names, database)?;
        let values = values
            .remove(service)
            .with_context(|| format!("Native snapshot omitted {service} root properties"))?;
        if verbose_timing_logs() {
            eprintln!(
                "[renium] native {service} root: total={:.1}ms invoke={:.1}ms serialize={:.1}ms instances={} bytes={}",
                snapshot.elapsed_ms,
                snapshot.invoke_ms,
                snapshot.serialize_ms,
                snapshot.instance_count,
                snapshot.output_size
            );
        }
        Ok(values)
    })();
    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    log_timing(&format!("{service}: native service-root read"), started);
    result
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn merge_live_service_root_property_values(
    service: &str,
    values: &mut Map<String, Value>,
    live_values: &Map<String, Value>,
    database: &ReflectionDatabase<'_>,
) {
    let path_segments = [service.to_string()];
    for (name, value) in live_values {
        if is_externally_managed_editor_property(service, service, &path_segments, name) {
            continue;
        }
        let Some(descriptor) =
            rbx_canonical_property_descriptor_for_serialized_name(database, service, name)
                .or_else(|| rbx_model_property_descriptor(database, service, name))
        else {
            continue;
        };
        values.insert(descriptor.name.to_string(), value.clone());
    }
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn encode_service_root_property_values(
    service: &str,
    values: &Map<String, Value>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelExportRefs,
) -> rbx_dom_weak::UstrMap<RbxVariant> {
    values
        .iter()
        .filter_map(|(name, value)| {
            let descriptor = rbx_model_property_descriptor(database, service, name)?;
            let value = json_to_rbx_property_variant(value, Some(descriptor), database, refs)?;
            Some((descriptor.name.into(), value))
        })
        .collect()
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn write_live_editor_place_snapshot(
    bridge: &BridgeServer,
    args: &PushEditorChangesArgs,
    output_path: &Path,
    existing_place: Option<&Path>,
) -> Result<usize> {
    write_editor_place_snapshot(bridge, Some(args), output_path, existing_place, None, true)
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn write_connected_editor_place_snapshot(
    bridge: &BridgeServer,
    runtime_id: &str,
    output_path: &Path,
    existing_place: &Path,
) -> Result<usize> {
    write_editor_place_snapshot(
        bridge,
        None,
        output_path,
        Some(existing_place),
        Some(runtime_id),
        false,
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn write_editor_place_snapshot(
    bridge: &BridgeServer,
    args: Option<&PushEditorChangesArgs>,
    output_path: &Path,
    existing_place: Option<&Path>,
    runtime_id: Option<&str>,
    try_native: bool,
) -> Result<usize> {
    if try_native && path_extension_is(output_path, &["rbxl"]) {
        let pid = studio_pid_for_bridge(bridge)?;
        let title = studio_title_for_bridge(bridge, pid)?;
        match serializer::write_live_place(pid, &title, output_path) {
            Ok(snapshot) => {
                eprintln!(
                    "[renium] native snapshot: total={:.1}ms trace={:.1}ms discover={:.1}ms helper={:.1}ms invoke={:.1}ms validate={:.1}ms context={:.1}ms roots={:.1}ms serialize={:.1}ms write={:.1}ms bytes={}",
                    snapshot.elapsed_ms,
                    snapshot.trace_ms,
                    snapshot.discover_ms,
                    snapshot.helper_ms,
                    snapshot.invoke_ms,
                    snapshot.validate_ms,
                    snapshot.context_ms,
                    snapshot.collect_ms,
                    snapshot.serialize_ms,
                    snapshot.write_ms,
                    snapshot.output_size
                );
                return Ok(snapshot.instance_count);
            }
            Err(error) => {
                eprintln!(
                    "[renium] native snapshot unavailable; using Studio export fallback: {error:#}"
                );
            }
        }
    }
    let export = match runtime_id {
        Some(runtime_id) => receive_editor_binary_export_for_runtime(bridge, runtime_id)?,
        None => receive_editor_binary_export(bridge)?,
    };
    let mut dom = rbx_binary::from_reader(std::io::Cursor::new(&export.bytes))
        .context("Studio returned an invalid native place snapshot")?;
    let plugin_service_roots = serialized_service_roots(&mut dom, &export.groups)?;
    let service_names = export
        .groups
        .iter()
        .map(|group| group.service.clone())
        .collect::<Vec<_>>();
    let service_name_set = service_names.iter().cloned().collect::<HashSet<_>>();
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let mut root_property_values = if let Some(path) = existing_place {
        read_place_service_root_property_values(path, &service_name_set, database)?
    } else {
        HashMap::new()
    };
    if let Some(args) = args {
        let project_root = resolve_project_root_if_present(&args.project.project_root)?;
        let src_root = absolutize_under(&project_root, &args.project.src_root);
        let project_services = service_names
            .iter()
            .filter(|service| !root_property_values.contains_key(*service))
            .filter(|service| service_settings_path(&src_root.join(service)).exists())
            .cloned()
            .collect::<Vec<_>>();
        if !project_services.is_empty() {
            let base = build_rbx_place(&src_root, project_services, None, false, false, false)?;
            for (service, values) in
                rbx_dom_service_root_property_values(&base.dom, &service_name_set, database)
            {
                root_property_values.entry(service).or_insert(values);
            }
        }
    }
    let attributes_key = rbx_dom_weak::Ustr::from("Attributes");
    let tags_key = rbx_dom_weak::Ustr::from("Tags");
    let mut service_roots = Vec::with_capacity(export.groups.len());
    let mut live_metadata = Vec::with_capacity(export.groups.len());
    for (group, (marker_ref, child_refs)) in export.groups.iter().zip(plugin_service_roots) {
        let marker = dom
            .get_by_ref_mut(marker_ref)
            .context("Studio native place snapshot lost a service marker")?;
        let live_attributes = marker.properties.get(&attributes_key).cloned();
        let live_tags = marker.properties.get(&tags_key).cloned();
        marker.class = group.service.as_str().into();
        marker.name.clone_from(&group.service);
        for child_ref in child_refs {
            dom.transfer_within(child_ref, marker_ref);
        }
        service_roots.push((group.service.clone(), marker_ref));
        live_metadata.push((live_attributes, live_tags));
    }
    let target_refs = rbx_dom_path_export_refs(&dom);
    for ((group, (_, marker_ref)), (live_attributes, live_tags)) in export
        .groups
        .iter()
        .zip(service_roots.iter())
        .zip(live_metadata)
    {
        let values = root_property_values
            .entry(group.service.clone())
            .or_default();
        merge_live_service_root_property_values(
            &group.service,
            values,
            &group.root_properties,
            database,
        );
        let mut properties =
            encode_service_root_property_values(&group.service, values, database, &target_refs);
        if let Some(value) = live_attributes {
            properties.insert(attributes_key, value);
        }
        if let Some(value) = live_tags {
            properties.insert(tags_key, value);
        }
        dom.get_by_ref_mut(*marker_ref)
            .context("Studio native place snapshot lost a service root")?
            .properties = properties;
    }
    let mut total_instances = 0;
    let mut has_package_links = false;
    for instance in dom.descendants() {
        total_instances += 1;
        has_package_links |= instance.class.as_str() == "PackageLink";
    }
    let build = RbxPlaceBuild {
        dom,
        service_roots,
        total_instances,
        has_package_links,
        omitted_properties_by_class: HashMap::new(),
        logical_properties_by_ref: HashMap::new(),
        unresolved_reference_properties_by_ref: HashMap::new(),
    };
    let format_path = existing_place.unwrap_or(output_path);
    write_rbx_place_build(output_path, &build, RbxPlaceFormat::from_path(format_path)?)?;
    Ok(build.total_instances)
}

pub(crate) fn wait_for_editor_review_decision(
    bridge: &BridgeServer,
    response: Value,
    change_count: u64,
    label: &str,
) -> Result<bool> {
    if response.get("required").and_then(Value::as_bool) != Some(true) {
        return Ok(true);
    }
    let Some(review_id) = response
        .get("reviewId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        bail!("Studio required a review but did not return a review id");
    };
    println!("[renium] {label} held for review in Studio: id={review_id}, changes={change_count}");
    let _ = io::stdout().flush();
    let deadline = Instant::now() + Duration::from_secs(610);
    let mut consecutive_errors = 0u32;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(300));
        match bridge.call(
            "getEditorPushReviewDecision",
            json!({ "reviewId": &review_id }),
        ) {
            Ok(result) => {
                consecutive_errors = 0;
                if let Some(error) = result.get("error").and_then(Value::as_str) {
                    bail!("Editor review {review_id} failed: {error}");
                }
                if result.get("decided").and_then(Value::as_bool) == Some(true) {
                    let decision = result
                        .get("decision")
                        .and_then(Value::as_str)
                        .unwrap_or("skip");
                    return Ok(decision != "skip");
                }
            }
            Err(err) => {
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    return Err(err.context("Editor push review polling failed"));
                }
            }
        }
    }
    Ok(false)
}

fn send_editor_binary_import(
    bridge: &BridgeServer,
    binary_import: &EditorBinaryImport,
    transaction_id: &str,
    container_settings: &[EditorPropertyChange],
) -> Result<Value> {
    #[cfg(windows)]
    if binary_import.native_replacement.is_some() {
        return send_editor_service_replacement(
            bridge,
            binary_import,
            transaction_id,
            container_settings,
        );
    }
    let _ = container_settings;
    const RAW_CHUNK_BYTES: usize = 2 * 1024 * 1024;
    let import_id = format!("{}-{}", current_millis(), fnv1a_hex(&binary_import.bytes));
    let total_chunks = binary_import.bytes.len().div_ceil(RAW_CHUNK_BYTES);
    let started = Instant::now();
    bridge.call(
        "beginEditorBinaryImport",
        json!({
            "importId": &import_id,
            "totalBytes": binary_import.bytes.len(),
            "totalChunks": total_chunks,
            "instanceCount": binary_import.instance_count,
            "groups": &binary_import.groups,
            "externalReferencesPostApplied": binary_import.external_references_post_applied,
            "viewportReferencesPostApplied": binary_import.viewport_references_post_applied,
            "transactionId": transaction_id,
        }),
    )?;
    log_timing("native editor import begin", started);
    let import_result = (|| -> Result<Value> {
        let started = Instant::now();
        let request_lease = bridge.active_request_lease();
        let transfer_threads = bridge.channel_count().max(1).min(total_chunks.max(1));
        let trace_context = crate::app::timing::trace_context();
        rayon::ThreadPoolBuilder::new()
            .num_threads(transfer_threads)
            .build()
            .context("Failed to initialize native import transfer workers")?
            .install(|| {
                binary_import
                    .bytes
                    .par_chunks(RAW_CHUNK_BYTES)
                    .enumerate()
                    .try_for_each(|(index, chunk)| -> Result<()> {
                        let _trace_context = trace_context
                            .map(|context| crate::app::timing::enter_trace_context(Some(context)));
                        let _lease = request_lease
                            .as_ref()
                            .map(|lease| bridge.inherit_request_lease(Arc::clone(lease)))
                            .transpose()?;
                        let data = base64::encode(chunk);
                        bridge.call(
                            "appendEditorBinaryImport",
                            json!({
                                "importId": &import_id,
                                "index": index + 1,
                                "data": data,
                            }),
                        )?;
                        Ok(())
                    })
            })?;
        log_timing("native editor import transfer", started);
        let started = Instant::now();
        let result = bridge.call(
            "finishEditorBinaryImport",
            json!({ "importId": &import_id, "profile": verbose_timing_logs() }),
        );
        log_timing("native editor import finish", started);
        if let Ok(response) = &result
            && let Some(profile) = response.get("profile")
        {
            crate::app::timing::trace_profile("Studio binary import finish", profile);
        }
        result
    })();
    if import_result.is_err() {
        let _ = bridge.call(
            "cancelEditorBinaryImport",
            json!({ "importId": &import_id }),
        );
    }
    import_result
}

#[cfg(windows)]
fn send_editor_service_replacement(
    bridge: &BridgeServer,
    import: &EditorBinaryImport,
    transaction_id: &str,
    container_settings: &[EditorPropertyChange],
) -> Result<Value> {
    let plan = import
        .native_replacement
        .as_ref()
        .context("Native replacement plan is missing")?;
    let import_id = format!("{}-{}", current_millis(), fnv1a_hex(&import.bytes));
    let pid = studio_pid_for_bridge(bridge)?;
    let title = studio_title_for_bridge(bridge, pid)?;
    let begin = bridge.call("beginEditorBinaryImport", json!({
        "importId": &import_id, "transactionId": transaction_id,
        "totalBytes": import.bytes.len(), "totalChunks": import.bytes.len().div_ceil(2 * 1024 * 1024),
        "instanceCount": import.instance_count, "groups": &import.groups,
        "externalReferencesPostApplied": import.external_references_post_applied,
        "viewportReferencesPostApplied": import.viewport_references_post_applied,
        "nativeReplacement": plan, "containerSettings": container_settings,
        "nativeReceiptFormat": 2,
    }))?;
    let mut invoked = false;
    let result = (|| {
        anyhow::ensure!(
            begin.get("nativeReceiptFormat").and_then(Value::as_u64) == Some(2),
            "Studio's Renium plugin needs updating for compact native receipts"
        );
        let held = begin
            .get("nativeTargets")
            .context("Studio did not return native transaction targets")?;
        let ready = bridge.call(
            "finishEditorBinaryImport",
            json!({
                "importId": &import_id, "nativePhase": "prepare", "profile": verbose_timing_logs(),
            }),
        )?;
        // Native code must not run merely because a target lookup succeeded.
        // The plugin first arms rollback, owns the mutation and its observation.
        anyhow::ensure!(
            ready.get("nativeReaderReady").and_then(Value::as_bool) == Some(true),
            "Studio has not armed the native replacement transaction"
        );
        // Keep the binary batches, but share one native task and factory scope.
        // Its creation ordinals already match the transaction-wide identity plan.
        let (created, outcome) = match serializer::read_service_payload(
            pid,
            &title,
            &import.bytes,
            plan,
            held,
            Duration::from_secs(20),
            &mut invoked,
        ) {
            Ok(receipt) => (
                receipt.created,
                (receipt.status, receipt.state, receipt.error),
            ),
            // Nothing ran: disarm the native phase so ordinary rollback can run.
            Err(error) if !invoked => (Vec::new(), (1, 8, error.to_string())),
            // An unknown native outcome must not be retried or blindly cancelled.
            Err(error) => return Err(error),
        };
        // The helper's fixed-width ABI is not the bridge's wire format. Keep
        // exact identities/ordinals, without transporting 48-byte padded strings.
        let mut chunks = vec![Vec::new()];
        for row in created.chunks_exact(serializer::CREATED_ROW) {
            let class = u32::from_le_bytes(row[..4].try_into()?);
            let class =
                u16::try_from(class).context("Native receipt class exceeds its wire limit")?;
            let length = row[8..]
                .iter()
                .position(|byte| *byte == 0)
                .context("Native receipt identity is unterminated")?;
            anyhow::ensure!(
                length > 0 && length < 48,
                "Native receipt identity is empty or oversized"
            );
            if chunks.last().unwrap().len() + 7 + length > 2 * 1024 * 1024 {
                chunks.push(Vec::new());
            }
            let chunk = chunks.last_mut().unwrap();
            chunk.extend_from_slice(&class.to_le_bytes());
            chunk.extend_from_slice(&row[4..8]);
            chunk.push(length as u8);
            chunk.extend_from_slice(&row[8..8 + length]);
        }
        // The last receipt and its completion are one ordered operation. Older
        // plugins keep the explicit upload request; the begin reply negotiates it.
        let inline_receipt =
            begin.get("nativeInlineReceipt").and_then(Value::as_bool) == Some(true);
        let mut final_receipt = None;
        let chunks = chunks
            .iter()
            .filter(|chunk| !chunk.is_empty())
            .collect::<Vec<_>>();
        for (index, chunk) in chunks.iter().enumerate() {
            let receipt = json!({ "index": index + 1, "data": base64::encode(chunk) });
            if inline_receipt && index + 1 == chunks.len() {
                final_receipt = Some(receipt);
                break;
            }
            bridge.call(
                "appendEditorBinaryImport",
                json!({
                    "importId": &import_id, "nativeReceipt": true, "index": index + 1,
                    "data": receipt["data"],
                }),
            )?;
        }
        let response = bridge.call(
            "finishEditorBinaryImport",
            json!({
                "importId": &import_id, "nativePhase": "complete", "profile": verbose_timing_logs(),
                "nativeStatus": outcome.0, "nativeState": outcome.1, "nativeError": outcome.2,
                "nativeCreated": created.len() / serializer::CREATED_ROW,
                "nativeReceiptChunk": final_receipt,
            }),
        )?;
        if let Some(profile) = response.get("profile") {
            crate::app::timing::trace_profile("Studio native import tracking", profile);
        }
        anyhow::ensure!(
            response["ok"] == true
                && response["nativeInserted"] == true
                && response["instanceCreated"].as_u64()
                    == Some((created.len() / serializer::CREATED_ROW) as u64),
            "Studio did not confirm the native replacement: {response}"
        );
        Ok(response)
    })();
    if result.is_err() && !invoked {
        let _ = bridge.call(
            "cancelEditorBinaryImport",
            json!({ "importId": &import_id }),
        );
    }
    result
}

const INSTANCE_BATCH_SIZE: usize = 5000;
const SOURCE_BATCH_SIZE: usize = 16;
const PROPERTY_BATCH_MAX_ITEMS: usize = 512;

fn combined_editor_change_batch(
    changes: &EditorChangeSet,
    probe_events: bool,
    transaction_id: Option<&str>,
) -> Result<Option<Value>> {
    let categories = usize::from(!changes.instance_changes.is_empty())
        + usize::from(!changes.source_changes.is_empty())
        + usize::from(!changes.property_changes.is_empty());
    if categories < 2
        || changes.instance_changes.len() > INSTANCE_BATCH_SIZE
        || changes.source_changes.len() > SOURCE_BATCH_SIZE
        || changes.property_changes.len() > PROPERTY_BATCH_MAX_ITEMS
        || changes.instance_changes.iter().any(|change| {
            change.instances.len() > INSTANCE_BATCH_SIZE
                || change.preserve_instances.len() > INSTANCE_BATCH_SIZE
                || matches!(
                    change.mode.as_str(),
                    "beginReconcileService" | "reconcileServiceChunk" | "finishReconcileService"
                )
        })
    {
        return Ok(None);
    }
    let mut sources = changes.source_changes.iter().collect::<Vec<_>>();
    sources.sort_by(|left, right| source_change_apply_order(left, right));
    // applyEditorChanges already applies instances, then ordered sources, then
    // properties. Keep oversized and streaming operations on their chunked path.
    let request = json!({
        "profile": verbose_timing_logs(),
        "probeEvents": probe_events,
        "instanceChanges": &changes.instance_changes,
        "sourceChanges": sources,
        "propertyChanges": &changes.property_changes,
        "transactionId": transaction_id,
    });
    Ok((serde_json::to_vec(&request)?.len() <= MAX_BRIDGE_CHUNK_BYTES).then_some(request))
}

fn material_mode_value(change: &EditorPropertyChange) -> Option<&Value> {
    (change.service == "MaterialService"
        && change.class_name == "MaterialService"
        && change.path_segments == ["MaterialService"])
    .then(|| change.properties.get("Use2022Materials"))
    .flatten()
}

pub(crate) fn send_editor_change_batches(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    probe_events: bool,
    review: bool,
    auto_apply_review: bool,
    binary_import: Option<&EditorBinaryImport>,
    transaction_id: Option<&str>,
) -> Result<Map<String, Value>> {
    let mut summary = Map::new();
    summary.insert("ok".to_string(), Value::Bool(true));
    let instance_queued = changes
        .instance_changes
        .iter()
        .map(|change| change.instances.len())
        .sum::<usize>();
    summary.insert(
        "instanceQueued".to_string(),
        Value::Number(serde_json::Number::from(instance_queued as u64)),
    );
    summary.insert(
        "sourceQueued".to_string(),
        Value::Number(serde_json::Number::from(changes.source_changes.len() as u64)),
    );
    summary.insert(
        "propertyQueued".to_string(),
        Value::Number(serde_json::Number::from(
            changes.property_changes.len() as u64
        )),
    );

    if changes.instance_changes.is_empty()
        && changes.source_changes.is_empty()
        && changes.property_changes.is_empty()
    {
        if probe_events {
            let result = bridge.call(
                "applyEditorChanges",
                json!({
                    "profile": verbose_timing_logs(),
                    "probeEvents": true,
                    "instanceChanges": [],
                    "sourceChanges": [],
                    "propertyChanges": [],
                    "transactionId": transaction_id,
                }),
            )?;
            merge_editor_summary_checked(&mut summary, &result)?;
        }
        summary.insert(
            "noops".to_string(),
            Value::Number(serde_json::Number::from(0)),
        );
        return Ok(summary);
    }

    if review && !auto_apply_review && !request_editor_push_review(bridge, changes)? {
        summary.insert("skippedByReview".to_string(), Value::Bool(true));
        summary.insert(
            "noops".to_string(),
            Value::Number(serde_json::Number::from(0)),
        );
        return Ok(summary);
    }

    // Material mode changes Terrain's default colors. Complete that engine
    // setter before ordinary properties restore the explicitly saved palette.
    let material_changes = changes
        .property_changes
        .iter()
        .filter_map(|change| {
            let value = material_mode_value(change)?;
            let mut first = change.clone();
            first.properties = Map::from_iter([("Use2022Materials".into(), value.clone())]);
            first.reset_properties.clear();
            first.attributes.clear();
            first.deleted_attributes.clear();
            Some(first)
        })
        .collect::<Vec<_>>();
    if !material_changes.is_empty() {
        let result = bridge.call(
            "applyEditorChanges",
            json!({
                "profile": verbose_timing_logs(),
                "probeEvents": probe_events,
                "instanceChanges": [], "sourceChanges": [],
                "propertyChanges": material_changes, "transactionId": transaction_id,
            }),
        )?;
        merge_editor_summary_checked(&mut summary, &result)?;
        crate::editor::native_roots::apply(bridge, &mut summary, transaction_id)?;
    }

    let mut payload_verified_services = std::collections::BTreeSet::new();
    if let Some(binary_import) = binary_import {
        let transaction_id =
            transaction_id.context("Native editor import requires an active transaction")?;
        let container_settings = container_setting_rows(changes, binary_import);
        let result =
            send_editor_binary_import(bridge, binary_import, transaction_id, &container_settings)?;
        payload_verified_services = verified_payload_services(binary_import, &result);
        merge_editor_summary_checked(&mut summary, &result)?;
        summary.insert(
            "binaryBytes".to_string(),
            Value::Number(serde_json::Number::from(binary_import.bytes.len() as u64)),
        );
        summary.insert(
            "binaryInstances".to_string(),
            Value::Number(serde_json::Number::from(
                binary_import.instance_count as u64,
            )),
        );
    }

    if binary_import.is_none()
        && material_changes.is_empty()
        && let Some(request) = combined_editor_change_batch(changes, probe_events, transaction_id)?
    {
        let result = bridge.call("applyEditorChanges", request)?;
        merge_editor_summary_checked(&mut summary, &result)?;
        summary.insert(
            "sourceSent".to_string(),
            json!(changes.source_changes.len()),
        );
        summary.insert(
            "propertySent".to_string(),
            json!(changes.property_changes.len()),
        );
        let native_root_verification =
            crate::editor::native_roots::apply(bridge, &mut summary, transaction_id)?;
        crate::editor::native_geometry::apply(bridge, &mut summary, transaction_id)?;
        verify_in_place_editor_fields(
            bridge,
            changes,
            binary_import,
            transaction_id,
            &native_root_verification,
            &mut summary,
        )?;
        return Ok(summary);
    }

    for instance_change in changes.instance_changes.iter().filter_map(|change| {
        let Some(import) = binary_import else {
            return Some(std::borrow::Cow::Borrowed(change));
        };
        if import.imports_service(&change.service) {
            return None;
        }
        if change.mode != "upsertInstances"
            || !change.instances.iter().any(|instance| {
                import.imports_path(
                    &change.service,
                    &instance.path_segments,
                    &instance.path_ordinals,
                )
            })
        {
            return Some(std::borrow::Cow::Borrowed(change));
        }
        let mut remaining = change.clone();
        remaining.instances.retain(|instance| {
            !import.imports_path(
                &change.service,
                &instance.path_segments,
                &instance.path_ordinals,
            )
        });
        (!remaining.instances.is_empty()).then_some(std::borrow::Cow::Owned(remaining))
    }) {
        if instance_change.mode == "reconcileService"
            && (instance_change.instances.len() > INSTANCE_BATCH_SIZE
                || instance_change.preserve_instances.len() > INSTANCE_BATCH_SIZE)
        {
            let session_id = format!(
                "{}-{}",
                instance_change.service,
                fnv1a_hex(
                    format!(
                        "{}:{}:{}",
                        instance_change.service,
                        instance_change.instances.len(),
                        instance_change.allow_deletes
                    )
                    .as_bytes()
                )
            );
            let total_chunks = instance_change
                .instances
                .len()
                .div_ceil(INSTANCE_BATCH_SIZE)
                .max(
                    instance_change
                        .preserve_instances
                        .len()
                        .div_ceil(INSTANCE_BATCH_SIZE),
                );
            for chunk_index in 0..total_chunks {
                let instance_start =
                    (chunk_index * INSTANCE_BATCH_SIZE).min(instance_change.instances.len());
                let instance_end =
                    (instance_start + INSTANCE_BATCH_SIZE).min(instance_change.instances.len());
                let preserve_start = (chunk_index * INSTANCE_BATCH_SIZE)
                    .min(instance_change.preserve_instances.len());
                let preserve_end = (preserve_start + INSTANCE_BATCH_SIZE)
                    .min(instance_change.preserve_instances.len());
                let instance_batch = &instance_change.instances[instance_start..instance_end];
                let preserve_batch =
                    &instance_change.preserve_instances[preserve_start..preserve_end];
                let mode = if chunk_index == 0 {
                    "beginReconcileService"
                } else if chunk_index + 1 == total_chunks {
                    "finishReconcileService"
                } else {
                    "reconcileServiceChunk"
                };
                let result = match bridge.call(
                    "applyEditorChanges",
                    json!({
                        "profile": verbose_timing_logs(),
                        "probeEvents": probe_events,
                        "instanceChanges": [{
                            "mode": mode,
                            "service": &instance_change.service,
                            "allowDeletes": chunk_index + 1 == total_chunks && instance_change.allow_deletes,
                            "reconcileSession": &session_id,
                            "instances": instance_batch,
                            "preserveInstances": preserve_batch,
                        }],
                        "sourceChanges": [],
                        "propertyChanges": [],
                        "transactionId": transaction_id,
                    }),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        let _ = bridge.call(
                            "cancelEditorReconcile",
                            json!({
                                "service": &instance_change.service,
                                "reconcileSession": &session_id,
                            }),
                        );
                        return Err(error);
                    }
                };
                merge_editor_summary_checked(&mut summary, &result)?;
            }
        } else if instance_change.instances.len() > INSTANCE_BATCH_SIZE {
            for instance_batch in instance_change.instances.chunks(INSTANCE_BATCH_SIZE) {
                let result = bridge.call(
                    "applyEditorChanges",
                    json!({
                    "profile": verbose_timing_logs(),
                    "probeEvents": probe_events,
                    "instanceChanges": [{
                        "mode": &instance_change.mode,
                        "service": &instance_change.service,
                        "allowDeletes": false,
                        "instances": instance_batch,
                    }],
                        "sourceChanges": [],
                        "propertyChanges": [],
                        "transactionId": transaction_id,
                    }),
                )?;
                merge_editor_summary_checked(&mut summary, &result)?;
            }
        } else {
            let result = bridge.call(
                "applyEditorChanges",
                json!({
                    "profile": verbose_timing_logs(),
                    "probeEvents": probe_events,
                    "instanceChanges": [instance_change],
                    "sourceChanges": [],
                    "propertyChanges": [],
                    "transactionId": transaction_id,
                }),
            )?;
            merge_editor_summary_checked(&mut summary, &result)?;
        }
    }

    let mut source_changes = changes
        .source_changes
        .iter()
        .filter(|change| {
            !binary_import.is_some_and(|import| {
                import.imports_path(
                    &change.service,
                    &change.path_segments,
                    &change.path_ordinals,
                )
            })
        })
        .collect::<Vec<_>>();
    source_changes.sort_by(|left, right| source_change_apply_order(left, right));
    summary.insert(
        "sourceSent".to_string(),
        Value::Number(serde_json::Number::from(source_changes.len() as u64)),
    );
    for source_batch in source_changes.chunks(SOURCE_BATCH_SIZE) {
        let result = bridge.call(
            "applyEditorChanges",
            json!({
                "profile": verbose_timing_logs(),
                "probeEvents": probe_events,
                "instanceChanges": [],
                "sourceChanges": source_batch,
                "propertyChanges": [],
                "transactionId": transaction_id,
            }),
        )?;
        merge_editor_summary_checked(&mut summary, &result)?;
    }

    let mut property_changes = Vec::new();
    for change in &changes.property_changes {
        let imported = binary_import.is_some_and(|import| {
            import.imports_path(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            )
        });
        if imported
            && binary_import.is_some_and(|import| {
                import.retains_path(
                    &change.service,
                    &change.path_segments,
                    &change.path_ordinals,
                )
            })
        {
            continue;
        }
        let payload_container = imported
            && binary_import.is_some_and(|import| {
                import.carries_container_settings(&change.service, &change.path_segments)
            });
        let send_all =
            !imported || property_change_needs_post_native_apply(change) && !payload_container;
        if send_all {
            let mut remaining = change.clone();
            if material_mode_value(change).is_some() {
                remaining.properties.remove("Use2022Materials");
            }
            if !remaining.properties.is_empty()
                || !remaining.reset_properties.is_empty()
                || !remaining.attributes.is_empty()
                || !remaining.deleted_attributes.is_empty()
            {
                property_changes.push(remaining);
            }
            continue;
        }
        let class_names = binary_import.and_then(|import| {
            import
                .post_apply_properties_by_class
                .get(&change.class_name)
        });
        let path_names = binary_import.and_then(|import| {
            import
                .post_apply_properties_by_path
                .get(&instance_path_parts_key(
                    &change.path_segments,
                    &change.path_ordinals,
                ))
        });
        let retained_name = |name: &str| {
            class_names.is_some_and(|names| names.contains(name))
                || path_names.is_some_and(|names| names.contains(name))
        };
        if payload_container {
            let mut remaining = change.clone();
            remaining.attributes.clear();
            remaining.properties.retain(|name, _| {
                crate::editor::native_roots::is_property(&change.class_name, name)
                    || retained_name(name)
            });
            if material_mode_value(change).is_some() {
                remaining.properties.remove("Use2022Materials");
            }
            if !remaining.properties.is_empty()
                || !remaining.reset_properties.is_empty()
                || !remaining.deleted_attributes.is_empty()
            {
                property_changes.push(remaining);
            }
            continue;
        }
        if class_names.is_none() && path_names.is_none() {
            continue;
        }
        let properties = change
            .properties
            .iter()
            .filter(|(name, _)| {
                !(change.class_name == "Model" && name.as_str() == "WorldPivot")
                    && retained_name(name)
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Map<_, _>>();
        if properties.is_empty() {
            continue;
        }
        let mut post_apply = change.clone();
        post_apply.properties = properties;
        post_apply.attributes.clear();
        post_apply.deleted_attributes.clear();
        property_changes.push(post_apply);
    }
    summary.insert(
        "propertySent".to_string(),
        Value::Number(serde_json::Number::from(
            (property_changes.len() + material_changes.len()) as u64,
        )),
    );
    send_property_batches(
        bridge,
        &property_changes,
        probe_events,
        transaction_id,
        &mut summary,
    )?;

    let native_root_verification =
        crate::editor::native_roots::apply(bridge, &mut summary, transaction_id)?;
    crate::editor::native_geometry::apply(bridge, &mut summary, transaction_id)?;
    if let Some(import) = binary_import.filter(|import| {
        import.native_replacement.is_some() || !payload_verified_services.is_empty()
    }) {
        let started = Instant::now();
        let mut rows = container_setting_rows(changes, import);
        rows.extend(property_changes);
        if import.native_replacement.is_none() {
            rows.extend(material_changes);
            rows.retain(|row| payload_verified_services.contains(&row.service));
        }
        verify_native_property_rows(
            bridge,
            &rows,
            transaction_id.context("Native verification requires a transaction")?,
            &native_root_verification,
            &mut summary,
        )?;
        // The native factory or detached tree checked its complete receipt. Verify the
        // retained containers and every value applied outside that reader here,
        // rather than exporting all newly loaded instances a second time.
        // Additive groups and property resets retain ordinary readback.
        let services = import
            .groups
            .iter()
            .filter(|group| {
                (import.native_replacement.is_some()
                    || payload_verified_services.contains(&group.service))
                    && !import
                        .groups
                        .iter()
                        .any(|other| other.service == group.service && other.additive)
                    && !rows
                        .iter()
                        .any(|row| row.service == group.service && !row.reset_properties.is_empty())
            })
            .map(|group| group.service.clone())
            .collect::<std::collections::BTreeSet<_>>();
        summary.insert("nativeVerifiedServices".into(), json!(services));
        log_timing("native editor supplemental verification", started);
    }
    verify_in_place_editor_fields(
        bridge,
        changes,
        binary_import,
        transaction_id,
        &native_root_verification,
        &mut summary,
    )?;
    Ok(summary)
}

fn verify_in_place_editor_fields(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    binary_import: Option<&EditorBinaryImport>,
    transaction_id: Option<&str>,
    native_roots: &crate::editor::native_roots::NativeRootVerification,
    summary: &mut Map<String, Value>,
) -> Result<()> {
    // Filtered plans can intentionally omit requested fields. Their remaining
    // writes cannot prove the complete desired service snapshot.
    if changes.files_to_studio_filters_active {
        return Ok(());
    }
    let Some(transaction_id) = transaction_id else {
        return Ok(());
    };
    let field_services = changes
        .property_changes
        .iter()
        .filter(|row| {
            !row.properties.is_empty()
                || !row.attributes.is_empty()
                || !row.deleted_attributes.is_empty()
        })
        .map(|row| row.service.as_str())
        .chain(
            changes
                .instance_changes
                .iter()
                .map(|row| row.service.as_str()),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|service| {
            changes
                .property_changes
                .iter()
                .filter(|row| row.service == *service)
                .all(|row| row.reset_properties.is_empty())
                && !changes.instance_changes.iter().any(|change| {
                    change.service == *service
                        && !matches!(change.mode.as_str(), "upsertInstances" | "deleteInstances")
                })
                && !changes
                    .source_changes
                    .iter()
                    .any(|change| change.service == *service && change.deleted)
                && binary_import.is_none_or(|import| {
                    !import.groups.iter().any(|group| group.service == *service)
                        || (import.native_replacement.is_some()
                            && import
                                .groups
                                .iter()
                                .filter(|group| group.service == *service)
                                .all(|group| group.additive))
                })
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut verified_services = Vec::new();
    for service in field_services {
        let structural = changes
            .instance_changes
            .iter()
            .filter(|change| change.service == service)
            .any(|change| {
                change.mode == "deleteInstances"
                    || change
                        .instances
                        .iter()
                        .any(|instance| !instance.anchor_only)
            });
        if structural {
            let expected_instances = changes
                .instance_changes
                .iter()
                .filter(|change| change.service == service)
                .map(|change| {
                    change
                        .instances
                        .iter()
                        .filter(|instance| {
                            change.mode == "deleteInstances"
                                || binary_import.is_none_or(|import| {
                                    !import.imports_path(
                                        service,
                                        &instance.path_segments,
                                        &instance.path_ordinals,
                                    )
                                })
                        })
                        .count()
                })
                .sum::<usize>();
            if summary
                .get("instancesVerified")
                .and_then(|counts| counts.get(service))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                != expected_instances as u64
            {
                continue;
            }
        }
        let rows = changes
            .property_changes
            .iter()
            .filter(|row| row.service == service)
            // Additive native roots already have the reader's identity/class
            // receipt and supplemental-field verification above. Verify the
            // existing objects changed alongside those roots here.
            .filter(|row| {
                binary_import.is_none_or(|import| {
                    !import.imports_path(service, &row.path_segments, &row.path_ordinals)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let expected = rows
            .iter()
            .map(|row| row.properties.len() + row.attributes.len() + row.deleted_attributes.len())
            .sum::<usize>();
        let verified =
            verify_native_property_rows(bridge, &rows, transaction_id, native_roots, summary)?;
        crate::app::timing::trace_profile(
            "editor.verification.coverage",
            &json!({
                "service": service, "expected": expected, "verified": verified,
            }),
        );
        // Skipped native-only fields are not a proof. Keep that service's full
        // readback without discarding complete proofs for other services.
        if verified == expected as u64 {
            verified_services.push(service);
        }
    }
    if !verified_services.is_empty() {
        summary.insert("fieldVerifiedServices".into(), json!(verified_services));
    }

    Ok(())
}

fn verified_payload_services(
    import: &EditorBinaryImport,
    result: &Value,
) -> std::collections::BTreeSet<String> {
    let reported = result
        .get("payloadVerifiedServices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    import
        .groups
        .iter()
        .filter(|group| {
            reported.contains(group.service.as_str())
                && import
                    .groups
                    .iter()
                    .filter(|other| other.service == group.service)
                    .all(|other| other.expected_structure.is_some() && !other.additive)
        })
        .map(|group| group.service.clone())
        .collect()
}

fn container_setting_rows(
    changes: &EditorChangeSet,
    binary_import: &EditorBinaryImport,
) -> Vec<EditorPropertyChange> {
    changes
        .property_changes
        .iter()
        .filter(|change| {
            binary_import.carries_container_settings(&change.service, &change.path_segments)
        })
        .map(|change| {
            let mut row = change.clone();
            row.reset_properties.clear();
            row.deleted_attributes.clear();
            row
        })
        .collect()
}

fn verify_native_property_rows(
    bridge: &BridgeServer,
    rows: &[EditorPropertyChange],
    transaction_id: &str,
    native_roots: &crate::editor::native_roots::NativeRootVerification,
    summary: &mut Map<String, Value>,
) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut verified = rows
        .iter()
        .map(|row| native_roots.verified_fields(row))
        .sum::<u64>();
    let mut mismatches = Vec::new();
    for batch in rows.chunks(PROPERTY_BATCH_MAX_ITEMS) {
        let result = bridge.call(
            "applyEditorChanges",
            json!({
                "verifyOnly": true, "instanceChanges": [], "sourceChanges": [],
                "propertyChanges": batch, "transactionId": transaction_id,
            }),
        )?;
        anyhow::ensure!(
            result["ok"] == true
                && result["verifyOnly"] == true
                && result["errors"].as_f64() == Some(0.0),
            "Studio did not complete read-only native property verification: {result}"
        );
        verified += result["verified"]
            .as_u64()
            .context("Studio omitted the verified property count")?;
        for mismatch in result["verifyMismatches"]
            .as_array()
            .context("Studio omitted container verification results")?
        {
            mismatches.push(
                mismatch
                    .as_str()
                    .context("Invalid container verification result")?
                    .to_string(),
            );
        }
    }
    anyhow::ensure!(
        mismatches.is_empty(),
        "Studio did not retain native supplemental properties: {}",
        mismatches.join(", ")
    );
    summary.insert("nativePropertiesVerified".to_string(), json!(verified));
    Ok(verified)
}

fn source_change_apply_order(
    left: &EditorSourceChange,
    right: &EditorSourceChange,
) -> std::cmp::Ordering {
    left.service
        .cmp(&right.service)
        .then_with(|| left.path_segments.len().cmp(&right.path_segments.len()))
        .then_with(|| left.path_segments.cmp(&right.path_segments))
        .then_with(|| left.path_ordinals.cmp(&right.path_ordinals))
}

fn merge_editor_summary(summary: &mut Map<String, Value>, result: &Value) {
    let Some(result) = result.as_object() else {
        return;
    };
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        summary.insert("ok".to_string(), Value::Bool(false));
    }
    for (key, value) in result {
        if key == "ok" {
            continue;
        }
        if key == "instancesVerified" {
            if let Some(counts) = value.as_object() {
                let target = summary.entry(key.clone()).or_insert_with(|| json!({}));
                if let Some(target) = target.as_object_mut() {
                    for (service, count) in counts {
                        if let Some(count) = count.as_u64() {
                            let previous = target.get(service).and_then(Value::as_u64).unwrap_or(0);
                            target.insert(service.clone(), json!(previous + count));
                        }
                    }
                }
            }
        } else if key == "protectedWrites"
            || key == "nativeGeometryWrites"
            || key == "nativeRootWrites"
        {
            let target = summary
                .entry(key.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let (Some(target), Some(values)) = (target.as_array_mut(), value.as_array()) {
                target.extend(values.iter().cloned());
            }
        } else if let Some(next) = value.as_f64() {
            let current = summary.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            if let Some(number) = serde_json::Number::from_f64(current + next) {
                summary.insert(key.clone(), Value::Number(number));
            }
        } else if key == "errors" {
            summary.insert(key.clone(), value.clone());
        }
    }
}

fn merge_editor_summary_checked(summary: &mut Map<String, Value>, result: &Value) -> Result<()> {
    if let Some(profile) = result.get("profile")
        && profile.get("applyMs").is_some()
    {
        crate::app::timing::trace_profile("Studio editor apply", profile);
    }
    merge_editor_summary(summary, result);
    let errors = result.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
    if result.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            bail!("Studio rejected or failed an editor push batch: {error}");
        }
        bail!("Studio rejected or failed an editor push batch");
    }
    Ok(())
}

#[cfg(test)]
mod source_change_tests {
    use super::*;

    #[test]
    fn structural_receipts_accumulate_per_service_across_batches() {
        let mut summary = Map::new();
        for result in [
            json!({"instancesVerified": {"Workspace": 3, "ServerStorage": 1}}),
            json!({"instancesVerified": {"Workspace": 2}}),
            json!({"ok": true}),
            json!({"instancesVerified": {"Workspace": "unsupported"}}),
        ] {
            merge_editor_summary(&mut summary, &result);
        }
        assert_eq!(
            summary["instancesVerified"],
            json!({"Workspace": 5, "ServerStorage": 1})
        );
    }

    #[test]
    fn native_payload_cache_hit_survives_in_flight_eviction() {
        let slot = "native-in-flight-cache-test";
        let expected = b"native binary payload";
        let hash = "native-in-flight-cache-hash";
        native_payload_cache_insert(slot.into(), hash.into(), expected);
        let complete = AtomicBool::new(false);
        let result = receive_editor_binary_export_bytes_with_cache(
            "export",
            Some("Workspace"),
            Some(&complete),
            slot,
            |request| {
                assert_eq!(request["knownPayloadHash"], hash);
                assert_eq!(request["offset"], 0);
                // Concurrent services evict the entry after this export has
                // advertised it, before Studio replies with a cache hit.
                for index in 0..=NATIVE_PAYLOAD_CACHE_MAX_ENTRIES {
                    let key = format!("native-cache-pressure-{index}");
                    native_payload_cache_insert(key.clone(), key, &[index as u8]);
                }
                Ok(BridgeChunk {
                    start: 1,
                    next_start: 1,
                    total: expected.len(),
                    chunk: String::new(),
                    plugin_server_ms: None,
                    plugin_encode_ms: None,
                    serialization_complete: true,
                    payload_hash: Some(hash.into()),
                    payload_cache_hit: true,
                    compression: None,
                    uncompressed_bytes: None,
                })
            },
        )
        .unwrap();
        assert_eq!(result, expected);
        assert!(complete.load(Ordering::Acquire));
    }

    fn source_change(path: &[&str]) -> EditorSourceChange {
        EditorSourceChange {
            service: path[0].to_string(),
            settings_id: None,
            path_segments: path.iter().map(|segment| (*segment).to_string()).collect(),
            path_ordinals: vec![1; path.len()],
            class_name: "ModuleScript".to_string(),
            source: Some("return true".to_string()),
            deleted: false,
        }
    }

    #[test]
    fn source_containers_are_applied_before_their_children() {
        let parent = source_change(&["ReplicatedStorage", "Package", "Controller"]);
        let child = source_change(&["ReplicatedStorage", "Package", "Controller", "Maid"]);
        let mut changes = [&child, &parent];

        changes.sort_by(|left, right| source_change_apply_order(left, right));

        assert_eq!(changes[0].path_segments, parent.path_segments);
        assert_eq!(changes[1].path_segments, child.path_segments);
    }

    #[test]
    fn combined_batches_preserve_order_scope_and_chunk_limits() {
        use crate::editor::types::{EditorInstanceChange, EditorInstanceDescriptor};

        let parent = source_change(&["ReplicatedStorage", "Controller"]);
        let child = source_change(&["ReplicatedStorage", "Controller", "Maid"]);
        let mut changes = EditorChangeSet {
            source_changes: vec![child.clone(), parent.clone()],
            instance_changes: vec![EditorInstanceChange {
                mode: "upsert".into(),
                service: "ReplicatedStorage".into(),
                allow_deletes: false,
                instances: vec![EditorInstanceDescriptor::default()],
                preserve_instances: vec![],
            }],
            ..Default::default()
        };
        let request = combined_editor_change_batch(&changes, true, Some("tx"))
            .unwrap()
            .unwrap();
        assert_eq!(
            request["sourceChanges"][0]["pathSegments"],
            json!(parent.path_segments)
        );
        assert_eq!(
            request["sourceChanges"][1]["pathSegments"],
            json!(child.path_segments)
        );
        assert_eq!(request["transactionId"], "tx");
        assert_eq!(request["probeEvents"], true);
        changes.source_changes[0].source = Some("x".repeat(MAX_BRIDGE_CHUNK_BYTES));
        assert!(
            combined_editor_change_batch(&changes, false, None)
                .unwrap()
                .is_none()
        );
        changes.source_changes = vec![parent.clone(); SOURCE_BATCH_SIZE + 1];
        assert!(
            combined_editor_change_batch(&changes, false, None)
                .unwrap()
                .is_none()
        );
        changes.source_changes = vec![parent];
        changes.instance_changes[0].mode = "beginReconcileService".into();
        assert!(
            combined_editor_change_batch(&changes, false, None)
                .unwrap()
                .is_none()
        );
        changes.instance_changes[0].mode = "upsert".into();
        changes.instance_changes[0].instances = (0..=INSTANCE_BATCH_SIZE)
            .map(|_| EditorInstanceDescriptor::default())
            .collect();
        assert!(
            combined_editor_change_batch(&changes, false, None)
                .unwrap()
                .is_none()
        );
    }
}

fn send_property_batches(
    bridge: &BridgeServer,
    property_changes: &[EditorPropertyChange],
    probe_events: bool,
    transaction_id: Option<&str>,
    summary: &mut Map<String, Value>,
) -> Result<()> {
    let mut property_start = 0;
    while property_start < property_changes.len() {
        let mut property_end = property_start;
        let mut estimated_bytes = 256usize;
        while property_end < property_changes.len()
            && property_end - property_start < PROPERTY_BATCH_MAX_ITEMS
        {
            let change_bytes = serde_json::to_vec(&property_changes[property_end])?.len() + 1;
            if property_end > property_start
                && estimated_bytes.saturating_add(change_bytes) > MAX_BRIDGE_CHUNK_BYTES
            {
                break;
            }
            estimated_bytes = estimated_bytes.saturating_add(change_bytes);
            property_end += 1;
        }
        let property_batch = &property_changes[property_start..property_end];
        let result = bridge.call(
            "applyEditorChanges",
            json!({
                "profile": verbose_timing_logs(),
                "probeEvents": probe_events,
                "instanceChanges": [],
                "sourceChanges": [],
                "propertyChanges": property_batch,
                "transactionId": transaction_id,
            }),
        )?;
        merge_editor_summary_checked(summary, &result)?;
        property_start = property_end;
    }

    Ok(())
}
