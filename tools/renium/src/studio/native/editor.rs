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

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
#[cfg(any(windows, target_os = "macos", test))]
use rbx_dom_weak::WeakDom as RbxWeakDom;
use rbx_dom_weak::types::{ContentType as RbxContentType, Ref as RbxRef, Variant as RbxVariant};
#[cfg(any(windows, target_os = "macos", test))]
use rbx_reflection::ReflectionDatabase;
use serde_json::{Map, Value, json};

use crate::bytecode::edit::instance_path_parts_key;
#[cfg(windows)]
use crate::bytecode::edit::{insert_unique_rbx_path, instance_path_key};
#[cfg(windows)]
use crate::cli::PushEditorChangesArgs;
#[cfg(any(windows, target_os = "macos", test))]
use crate::editor::review::is_externally_managed_editor_property;
use crate::editor::review::request_editor_push_review;
#[cfg(any(windows, target_os = "macos"))]
use crate::editor::review::{studio_pid_for_bridge, studio_title_for_bridge};
use crate::editor::types::{
    EditorBinaryExport, EditorBinaryExportGroup, EditorBinaryImport,
    EditorBinarySerializationBatch, EditorChangeSet, EditorPropertyChange,
};
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::decode::rbx_variant_to_settings_json;
use crate::rbx::decode::{
    NativeOverlayRequest, conditional_ref_overlay_request, fetch_native_overlay_batches,
    merge_native_overlay_items, native_overlay_property_schemas, native_property_filter,
    overlay_property_names_value, rbx_properties_to_native_settings_records,
};
#[cfg(windows)]
use crate::rbx::encode::collect_rbx_subtree_preorder;
use crate::rbx::encode::json_i64;
#[cfg(any(windows, test))]
use crate::rbx::encode::json_to_rbx_property_variant;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::encode::rbx_canonical_property_descriptor_for_serialized_name;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::encode::{rbx_model_property_descriptor, rbx_model_top_level_refs};
#[cfg(any(windows, test))]
use crate::rbx::model::BytecodeModelExportRefs;
use crate::rbx::model::BytecodeModelImportRefs;
#[cfg(any(windows, target_os = "macos"))]
use crate::rbx::model::RbxPlaceFormat;
#[cfg(any(windows, target_os = "macos", test))]
use crate::rbx::model::rbx_dom_path_import_refs;
#[cfg(windows)]
use crate::rbx::model::{RbxPlaceBuild, build_rbx_place, rbx_dom_instance_path_parts};
use crate::roblox::schema::{
    EnumValueNameMap, parse_enum_value_name_map, parse_property_schema_map,
};
#[cfg(any(windows, target_os = "macos"))]
use crate::roblox::schema::{MATERIAL_SERVICE_CLASS, USE_2022_MATERIALS_PROPERTY};
use crate::snapshot::export::{log_chunk_fetch_metrics, merge_chunk_fetch_metrics};
use crate::snapshot::types::{
    ExportedSnapshotParts, NativeConditionalOverlayFetch, NativeConditionalOverlayRequest,
    NativeOverlayItem, NativeServiceFetch, NativeServiceFinishDependencies,
    NativeServiceFinishInput, NativeSettingsProperty, NativeSettingsValue, ServiceExecutionSpan,
    ServiceExportOutput, SnapshotInstance,
};
use crate::studio::bridge::{BridgeChunk, BridgeServer, ChunkFetchMetrics, MAX_BRIDGE_CHUNK_BYTES};
#[cfg(any(windows, target_os = "macos"))]
use crate::system::files::sanitize_name;
use crate::system::files::{OnDrop, fnv1a_hex};
#[cfg(windows)]
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
            && (change.service == "Workspace" && change.class_name == "Terrain"
                || change.service == "StarterPlayer"
                    && matches!(
                        change.class_name.as_str(),
                        "StarterPlayerScripts" | "StarterCharacterScripts"
                    ))
}

#[cfg(windows)]
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
    metadata_only: bool,
) -> Result<EditorBinaryExport> {
    let export_id = format!("{}-{}", current_millis(), std::process::id());
    #[cfg(any(windows, target_os = "macos"))]
    let needs_material_access = !metadata_only
        && service_order.is_none_or(|services| {
            services
                .iter()
                .any(|service| service == MATERIAL_SERVICE_CLASS)
        });
    let begin_request = || {
        bridge.call(
            "beginEditorBinaryExport",
            json!({
                "exportId": &export_id,
                "partitioned": partitioned,
                "serviceOrder": service_order,
                "serializationWorkers": partitioned.then_some(2),
                "metadataOnly": metadata_only,
            }),
        )
    };
    #[cfg(any(windows, target_os = "macos"))]
    let (begin, material_root_properties) = if needs_material_access {
        let (begin, properties) = rayon::join(begin_request, || {
            let database =
                rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
            read_live_service_root_property_values(bridge, MATERIAL_SERVICE_CLASS, database)
        });
        #[cfg(any(windows, target_os = "macos"))]
        let properties = Some(properties?);
        (begin?, properties)
    } else {
        (begin_request()?, None)
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let begin = begin_request()?;
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
        #[cfg(any(windows, target_os = "macos"))]
        let groups = {
            let mut groups = groups;
            if let Some(properties) = material_root_properties {
                let database = rbx_reflection_database::get()
                    .context("Failed to load Roblox reflection DB")?;
                let material = groups
                    .iter_mut()
                    .find(|group| group.service == MATERIAL_SERVICE_CLASS)
                    .context("Studio native export omitted MaterialService")?;
                merge_live_service_root_property_values(
                    MATERIAL_SERVICE_CLASS,
                    &mut material.root_properties,
                    &properties,
                    database,
                );
                if material
                    .root_properties
                    .get(USE_2022_MATERIALS_PROPERTY)
                    .and_then(Value::as_bool)
                    .is_none()
                {
                    bail!("Native MaterialService root omitted Use2022Materials");
                }
            }
            groups
        };
        let serialization_batches = serde_json::from_value::<Vec<EditorBinarySerializationBatch>>(
            begin
                .get("serializationBatches")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        )
        .context("Studio returned invalid native serialization batches")?;
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
        Ok(EditorBinaryExport {
            #[cfg(windows)]
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

fn receive_editor_binary_export_bytes(
    bridge: &BridgeServer,
    export_id: &str,
    service: Option<&str>,
    serialization_complete: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    const MAX_EXPORT_BYTES: usize = 512 * 1024 * 1024;
    let service_label = service.map_or(String::new(), |value| format!("{value} "));
    let raw_chunk_bytes = native_binary_chunk_bytes();
    let read_started = Instant::now();
    let first = bridge.call_chunk(
        "readEditorBinaryExport",
        json!({
            "exportId": export_id,
            "service": service,
            "offset": 0,
            "length": raw_chunk_bytes,
            "clampLength": true,
            "waitForReady": true,
            "timeoutSeconds": 80,
            "rawBase64": true,
        }),
    )?;
    observe_native_serialization_complete(&first, serialization_complete);
    let total_bytes = first.total;
    if total_bytes == 0 || total_bytes > MAX_EXPORT_BYTES {
        bail!("Studio returned an invalid native export size");
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
            let response = bridge.call_chunk(
                "readEditorBinaryExport",
                json!({
                    "exportId": export_id,
                    "service": service,
                    "offset": offset,
                    "length": length,
                    "rawBase64": true,
                }),
            )?;
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
}

impl EditorBinaryExportFinishGuard<'_> {
    pub(crate) fn finish(&mut self, record_sync_completion: bool) -> Result<bool> {
        let Some(export_id) = self.export_id.as_deref() else {
            return Ok(false);
        };
        let result = self.bridge.call(
            "finishEditorBinaryExport",
            json!({
                "exportId": export_id,
                "recordSyncCompletion": record_sync_completion,
            }),
        )?;
        self.export_id = None;
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

#[cfg(windows)]
fn receive_editor_binary_export(bridge: &BridgeServer) -> Result<EditorBinaryExport> {
    let mut export = begin_editor_binary_export(bridge, false, None, false)?;
    let _finish_guard = EditorBinaryExportFinishGuard {
        bridge,
        export_id: export.export_id.clone(),
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
    path_segments_by_ref: Arc<HashMap<RbxRef, Vec<String>>>,
    path_ordinals_by_ref: Arc<HashMap<RbxRef, Vec<usize>>>,
}

struct NativeServiceExportResult {
    output: ServiceExportOutput,
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
        .elide_defaults(false)
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
    if flat.root_indices.len() != group.count + 1 {
        bail!(
            "Studio native {} snapshot contains {} roots; expected {}",
            group.service,
            flat.root_indices.len(),
            group.count + 1
        );
    }
    let marker_index = flat.root_indices[0];
    if marker_index != 0 {
        bail!("Studio native {} marker is out of order", group.service);
    }
    flat.instances[marker_index].class = group.service.as_str().into();
    flat.instances[marker_index].name.clone_from(&group.service);
    for root_index in flat.root_indices.iter().skip(1) {
        flat.instances[*root_index].parent_index = Some(marker_index);
    }
    if flat.instances.len() != group.instance_count {
        bail!(
            "Studio native {} snapshot contains {} instances; expected {}",
            group.service,
            flat.instances.len(),
            group.instance_count
        );
    }
    Ok(NativeServiceDom {
        instances: flat.instances,
        new_index_by_dense_ref: None,
        path_segments_by_ref: Arc::new(HashMap::new()),
        path_ordinals_by_ref: Arc::new(HashMap::new()),
    })
}

fn decode_native_serialization_batch(
    bytes: &[u8],
    batch: &EditorBinarySerializationBatch,
    service_groups: &[EditorBinaryExportGroup],
    property_filter: Arc<HashMap<String, HashSet<String>>>,
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
            .checked_add(group.count + 1)
            .context("Native serialization batch root count overflowed")
    })?;
    let expected_instances = groups.iter().try_fold(0_usize, |total, group| {
        total
            .checked_add(group.instance_count)
            .context("Native serialization batch instance count overflowed")
    })?;
    let decode_started = Instant::now();
    let flat = match rbx_binary::Deserializer::new()
        .elide_defaults(false)
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
        let root_end = root_offset + group.count + 1;
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
        if end - start != group.instance_count {
            bail!(
                "Studio native {} batch partition contains {} instances; expected {}",
                group.service,
                end - start,
                group.instance_count
            );
        }
        let marker = &flat.instances[start];
        if marker.parent_index.is_some()
            || marker.class.as_str() != "Folder"
            || marker.name != group.service
        {
            bail!("Studio native {} batch marker is invalid", group.service);
        }
        spans.push((start, end, root_offset, root_end));
        root_offset = root_end;
        expected_start = end;
    }
    let total_instances = flat.instances.len();
    let mut global_index_by_dense_ref = vec![usize::MAX; total_instances];
    let mut owner_by_dense_ref = vec![usize::MAX; total_instances];
    let mut local_index_by_dense_ref = vec![vec![usize::MAX; total_instances]; groups.len()];
    for (group_index, (start, end, _, _)) in spans.iter().copied().enumerate() {
        for (local_index, global_index) in (start..end).enumerate() {
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
            local_index_by_dense_ref[group_index][dense_index] = local_index;
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
            instances[*root_index - start].parent_index = Some(0);
        }
        if doms
            .insert(
                group.service.clone(),
                NativeServiceDom {
                    instances,
                    new_index_by_dense_ref: Some(std::mem::take(
                        &mut local_index_by_dense_ref[group_index],
                    )),
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

fn native_overlay_reference_debug_id(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    (object.get("_type").and_then(Value::as_str) == Some("Ref"))
        .then(|| object.get("debugId").and_then(Value::as_str))
        .flatten()
}

fn normalize_native_overlay_internal_references(
    overlays: &mut [NativeOverlayItem],
    debug_ids: &[Option<String>],
) {
    let requested = overlays
        .iter()
        .flat_map(|overlay| overlay.properties.values())
        .filter_map(native_overlay_reference_debug_id)
        .collect::<HashSet<_>>();
    if requested.is_empty() {
        return;
    }
    let internal_indices = debug_ids
        .iter()
        .enumerate()
        .filter_map(|(index, debug_id)| {
            let debug_id = debug_id.as_deref()?;
            requested
                .contains(debug_id)
                .then(|| (debug_id.to_string(), index + 1))
        })
        .collect::<HashMap<_, _>>();
    drop(requested);
    if internal_indices.is_empty() {
        return;
    }
    for overlay in overlays {
        for value in overlay.properties.values_mut() {
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
}

fn convert_native_service_output(
    dependencies: &NativeServiceFinishDependencies<'_, '_>,
    group: &EditorBinaryExportGroup,
    native: NativeServiceDom,
    mut overlay_instances: Vec<NativeOverlayItem>,
    debug_ids: Vec<Option<String>>,
    export_started_ms: f64,
) -> Result<ServiceExportOutput> {
    let NativeServiceDom {
        instances: native_instances,
        new_index_by_dense_ref,
        path_segments_by_ref,
        path_ordinals_by_ref,
    } = native;
    if debug_ids.len() != native_instances.len() {
        bail!(
            "Native debug ids contain {} {} instances; expected {}",
            debug_ids.len(),
            group.service,
            native_instances.len()
        );
    }
    normalize_native_overlay_internal_references(&mut overlay_instances, &debug_ids);
    let mut overlay_by_index = Vec::with_capacity(native_instances.len());
    overlay_by_index.resize_with(native_instances.len(), || None);
    for overlay in overlay_instances {
        let index = overlay
            .instance_index
            .checked_sub(1)
            .filter(|index| *index < overlay_by_index.len())
            .context("Native overlay instance index is out of range")?;
        if overlay_by_index[index].replace(overlay).is_some() {
            bail!("Native overlay instance index {} is duplicated", index + 1);
        }
    }
    let new_index_by_dense_ref = if let Some(new_index_by_dense_ref) = new_index_by_dense_ref {
        new_index_by_dense_ref
    } else {
        let mut new_index_by_dense_ref = vec![usize::MAX; native_instances.len()];
        for (index, instance) in native_instances.iter().enumerate() {
            let dense_index = instance
                .referent
                .as_u128()
                .and_then(|value| usize::try_from(value).ok())
                .and_then(|value| value.checked_sub(1))
                .filter(|value| *value < native_instances.len())
                .context("Native snapshot contains an invalid dense referent")?;
            if new_index_by_dense_ref[dense_index] != usize::MAX {
                bail!("Native snapshot contains a duplicate dense referent");
            }
            new_index_by_dense_ref[dense_index] = index;
        }
        if new_index_by_dense_ref.contains(&usize::MAX) {
            bail!("Native snapshot has an incomplete dense referent map");
        }
        new_index_by_dense_ref
    };
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
        .enumerate()
        .map(
            |(index, ((rbx_instance, overlay), debug_id))| -> Result<_> {
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
                    let tags = properties.remove("Tags");
                    properties.clone_from(&group.root_properties);
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
                    if overlay.instance_index != index + 1
                        || overlay_class.as_str() != rbx_instance.class.as_str()
                    {
                        bail!(
                            "Native snapshot and overlay disagree at {} instance {}",
                            group.service,
                            index + 1
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
                Ok((
                    SnapshotInstance {
                        name: rbx_instance.name,
                        class_name: rbx_instance.class,
                        properties,
                        attributes,
                        debug_id: debug_id.filter(|value| !value.is_empty()),
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
    Ok(ServiceExportOutput {
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
    })
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
    let export = begin_editor_binary_export(bridge, true, Some(requested_services), false)?;
    let finish_guard = EditorBinaryExportFinishGuard {
        bridge,
        export_id: export.export_id.clone(),
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
    let mut batched_groups = requested_groups
        .iter()
        .filter(|group| {
            group.instance_count < NATIVE_SERIALIZATION_SERVICE_LIMIT
                && !serialization_batch_by_service.contains_key(group.service.as_str())
        })
        .copied()
        .collect::<Vec<_>>();
    if batched_groups.len() < 2 {
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
    let priority_worker_count = if worker_count == 4
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
    rayon::scope_fifo(|scope| -> Result<()> {
        for worker_index in 0..worker_count {
            let sender = sender.clone();
            let overlay_names = &overlay_names;
            let overlay_schema = &overlay_schema;
            let direct_overlay_names = &direct_overlay_names;
            let direct_overlay_schema = &direct_overlay_schema;
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
            let serialization_complete_signal = &serialization_complete_signal;
            let service_groups = &export.groups;
            let finish_dependencies = &finish_dependencies;
            scope.spawn_fifo(move |_| {
                    if worker_index >= priority_worker_count {
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
                        let result = thread::scope(|reference_scope| -> Result<NativeServiceExportResult> {
                        let selective_refs =
                            group.instance_count >= NATIVE_SERIALIZATION_SERVICE_LIMIT
                            && group.class_names.iter().any(|class_name| {
                                conditional_ref_schema.contains_key(class_name)
                            });
                        let first_overlay_names = if selective_refs {
                            direct_overlay_names
                        } else {
                            overlay_names
                        };
                        let first_overlay_schema = if selective_refs {
                            direct_overlay_schema
                        } else {
                            overlay_schema
                        };
                        let (reference_sender, reference_receiver) = mpsc::sync_channel(1);
                        let (native, overlay) = rayon::join(
                            || -> Result<NativeServiceFetch> {
                                let (native, one_chunk) = if let Some(batch) =
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
                                let reference_request = selective_refs
                                    .then(|| {
                                        conditional_ref_overlay_request(
                                            &native.instances,
                                            conditional_ref_schema,
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
                                fetch_native_overlay_batches(
                                    bridge,
                                    NativeOverlayRequest {
                                        service: &group.service,
                                        start_index: 1,
                                        take_count: group.instance_count,
                                        instance_count: group.instance_count,
                                        overlay_id: export_id,
                                        overlay_variant: if selective_refs {
                                            "direct"
                                        } else {
                                            "combined"
                                        },
                                        include_debug_ids: true,
                                        overlay_names: first_overlay_names,
                                        overlay_schema: first_overlay_schema,
                                        enum_value_names_by_type,
                                        class_names: &group.class_names,
                                    },
                                )
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
                        let (native, reference_prefetched, reference_request) = native?;
                        let mut overlay = overlay?;
                        let debug_ids = std::mem::take(&mut overlay.debug_ids);
                        finish_native_service_export(
                            finish_dependencies,
                            group,
                            NativeServiceFinishInput {
                            native,
                            debug_ids,
                            overlay,
                            reference_prefetch: reference_prefetched
                                .then_some(reference_receiver),
                            reference_request,
                            export_started_ms,
                        },
                        )
                        });
                        priority_release.run();
                        if sender.send(result).is_err() {
                            break;
                        }
                    }
                });
        }
        drop(sender);
        for _ in 0..requested_services.len() {
            let result = receiver
                .lock()
                .map_err(|_| anyhow::anyhow!("Native service export receiver was poisoned"))?
                .recv()
                .context("Native service export worker closed")??;
            merge_chunk_fetch_metrics(&mut metrics, result.metrics);
            compact_expand_ms += result.compact_expand_ms;
            on_output(result.output)?;
            if !serialization_complete && serialization_complete_signal.load(Ordering::Acquire) {
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
    })?;
    log_chunk_fetch_metrics("native editor overlay payloads", metrics);
    log_timing_ms("native editor overlay compact expansion", compact_expand_ms);
    log_timing("native editor streaming export", stream_started);
    Ok(finish_guard)
}

#[cfg(windows)]
fn rbx_dom_path_export_refs(dom: &RbxWeakDom) -> BytecodeModelExportRefs {
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
        native,
        debug_ids,
        overlay,
        reference_prefetch,
        reference_request,
        export_started_ms,
    } = input;
    let mut service_metrics = overlay.metrics;
    let mut service_compact_expand_ms = overlay.compact_expand_ms;
    let has_reference_work = reference_prefetch.is_some() || reference_request.is_some();
    let convert = move || {
        convert_native_service_output(
            dependencies,
            group,
            native,
            overlay.items,
            debug_ids,
            export_started_ms,
        )
    };
    let output = if has_reference_work {
        let (output, reference_overlay) = rayon::join(convert, || {
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
        let mut output = output?;
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
            merge_native_overlay_items(
                &mut output.parts.instances,
                reference_overlay.items,
                &group.class_names,
            )?;
            merge_chunk_fetch_metrics(&mut service_metrics, reference_overlay.metrics);
            service_compact_expand_ms += reference_overlay.compact_expand_ms;
        }
        output
    } else {
        convert()?
    };
    Ok(NativeServiceExportResult {
        output,
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

#[cfg(any(windows, test))]
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

#[cfg(windows)]
pub(crate) fn write_live_editor_place_snapshot(
    bridge: &BridgeServer,
    args: &PushEditorChangesArgs,
    output_path: &Path,
    existing_place: Option<&Path>,
) -> Result<usize> {
    #[cfg(any(windows, target_os = "macos"))]
    if path_extension_is(output_path, &["rbxl"]) {
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
    let export = receive_editor_binary_export(bridge)?;
    let mut dom = rbx_binary::from_reader(std::io::Cursor::new(&export.bytes))
        .context("Studio returned an invalid native place snapshot")?;
    let roots = dom.root().children().to_vec();
    let expected_roots = export
        .groups
        .iter()
        .map(|group| group.count + 1)
        .sum::<usize>();
    if roots.len() != expected_roots {
        bail!("Studio native place snapshot has the wrong root count");
    }
    let project_root = resolve_project_root_if_present(&args.project.project_root)?;
    let src_root = absolutize_under(&project_root, &args.project.src_root);
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
    let attributes_key = rbx_dom_weak::Ustr::from("Attributes");
    let tags_key = rbx_dom_weak::Ustr::from("Tags");
    let mut cursor = 0usize;
    let mut service_roots = Vec::with_capacity(export.groups.len());
    let mut live_metadata = Vec::with_capacity(export.groups.len());
    for group in &export.groups {
        let marker_ref = roots[cursor];
        cursor += 1;
        let child_refs = roots[cursor..cursor + group.count].to_vec();
        cursor += group.count;
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
    let total_instances = dom.descendants().count();
    let build = RbxPlaceBuild {
        dom,
        service_roots,
        documents_by_service: HashMap::new(),
        paths_by_service: HashMap::new(),
        settings_writes: Vec::new(),
        total_instances,
        omitted_properties_by_class: HashMap::new(),
        logical_properties_by_ref: HashMap::new(),
    };
    write_rbx_place_build(output_path, &build, RbxPlaceFormat::from_path(output_path)?)?;
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
) -> Result<Value> {
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
            "transactionId": transaction_id,
        }),
    )?;
    log_timing("native editor import begin", started);
    let import_result = (|| -> Result<Value> {
        let started = Instant::now();
        let transfer_threads = bridge.channel_count().max(1).min(total_chunks.max(1));
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
            json!({ "importId": &import_id }),
        );
        log_timing("native editor import finish", started);
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

    if let Some(binary_import) = binary_import {
        let transaction_id =
            transaction_id.context("Native editor import requires an active transaction")?;
        let result = send_editor_binary_import(bridge, binary_import, transaction_id)?;
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

    const INSTANCE_BATCH_SIZE: usize = 5000;
    const SOURCE_BATCH_SIZE: usize = 16;
    const PROPERTY_BATCH_MAX_ITEMS: usize = 512;

    for instance_change in changes
        .instance_changes
        .iter()
        .filter(|_| binary_import.is_none())
    {
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

    let source_changes = changes
        .source_changes
        .iter()
        .filter(|_| binary_import.is_none())
        .collect::<Vec<_>>();
    summary.insert(
        "sourceSent".to_string(),
        Value::Number(serde_json::Number::from(source_changes.len() as u64)),
    );
    for source_batch in source_changes.chunks(SOURCE_BATCH_SIZE) {
        let result = bridge.call(
            "applyEditorChanges",
            json!({
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
        if binary_import.is_some_and(|import| {
            import.retains_path(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            )
        }) {
            continue;
        }
        let send_all = binary_import.is_none() || property_change_needs_post_native_apply(change);
        if send_all {
            property_changes.push(change.clone());
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
        if class_names.is_none() && path_names.is_none() {
            continue;
        }
        let properties = change
            .properties
            .iter()
            .filter(|(name, _)| {
                class_names.is_some_and(|names| names.contains(*name))
                    || path_names.is_some_and(|names| names.contains(*name))
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
        Value::Number(serde_json::Number::from(property_changes.len() as u64)),
    );
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
                "probeEvents": probe_events,
                "instanceChanges": [],
                "sourceChanges": [],
                "propertyChanges": property_batch,
                "transactionId": transaction_id,
            }),
        )?;
        merge_editor_summary_checked(&mut summary, &result)?;
        property_start = property_end;
    }

    Ok(summary)
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
        if key == "protectedWrites" {
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
    merge_editor_summary(summary, result);
    let errors = result.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
    if result.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
        bail!("Studio rejected or failed an editor push batch");
    }
    Ok(())
}
