use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::types::{Variant as RbxVariant, VariantType as RbxVariantType};
use rbx_reflection::{
    DataType as RbxDataType, PropertyDescriptor as RbxPropertyDescriptor,
    PropertyKind as RbxPropertyKind, PropertySerialization as RbxPropertySerialization,
    PropertyTag as RbxPropertyTag, ReflectionDatabase, Scriptability as RbxScriptability,
};
use serde_json::{Map, Value, json};

use super::bridge_server::{BridgeServer, BridgeTarget};
use super::command_line::PushEditorChangesArgs;
use super::editor_types::{EditorChangeSet, EditorInstancePath};
use super::input_inject;
#[cfg(any(windows, target_os = "macos"))]
use super::local_transport::pid_for_local_tcp_port;
use super::native_editor::{
    merge_live_service_root_property_values, read_live_service_root_property_values,
    wait_for_editor_review_decision, write_live_editor_place_snapshot,
};
use super::output::global_log_enabled;
use super::property_schema::{
    MATERIAL_SERVICE_CLASS, PropertySchemaEntry, PropertySchemaMap, USE_2022_MATERIALS_PROPERTY,
};
use super::rbx_decode::rbx_variant_to_settings_json;
use super::rbx_encode::{
    json_to_rbx_attribute_variant, json_to_rbx_property_variant, rbx_model_property_descriptor,
    rbx_model_top_level_refs, rbx_property_descriptor,
};
use super::rbx_model::{
    BytecodeModelExportRefs, RbxPlaceFormat, rbx_dom_instance_by_path_unique,
    rbx_dom_path_import_refs,
};
#[cfg(target_os = "macos")]
use super::studio_native_serializer;
use super::timing::current_millis;

pub(super) fn is_workspace_camera_sync_target(
    service: &str,
    class_name: &str,
    path_segments: &[String],
) -> bool {
    class_name == "Camera"
        && service == "Workspace"
        && path_segments.len() == 2
        && path_segments
            .first()
            .is_some_and(|segment| segment == "Workspace")
        && path_segments
            .get(1)
            .is_some_and(|segment| segment == "Camera" || segment == "CurrentCamera")
}

pub(super) fn is_externally_managed_editor_property(
    service: &str,
    class_name: &str,
    path_segments: &[String],
    property_name: &str,
) -> bool {
    service.eq_ignore_ascii_case("Players")
        && class_name.eq_ignore_ascii_case("Players")
        && path_segments.len() == 1
        && path_segments[0].eq_ignore_ascii_case("Players")
        && matches!(
            property_name.to_ascii_lowercase().as_str(),
            "maxplayers" | "maxplayersinternal" | "preferredplayers" | "preferredplayersinternal"
        )
}

pub(super) fn is_engine_managed_editor_property(
    class_name: &str,
    property_name: &str,
    database: &ReflectionDatabase<'_>,
) -> bool {
    if class_name == MATERIAL_SERVICE_CLASS && property_name == USE_2022_MATERIALS_PROPERTY {
        return false;
    }
    let Some(descriptor) = rbx_property_descriptor(database, class_name, property_name) else {
        return false;
    };
    if matches!(
        &descriptor.data_type,
        RbxDataType::Value(RbxVariantType::UniqueId | RbxVariantType::SecurityCapabilities)
    ) {
        return true;
    }
    descriptor.tags.iter().any(|tag| {
        matches!(
            tag,
            RbxPropertyTag::Deprecated
                | RbxPropertyTag::Hidden
                | RbxPropertyTag::NotBrowsable
                | RbxPropertyTag::ReadOnly
                | RbxPropertyTag::WriteOnly
        )
    })
}

pub(super) fn is_externally_managed_protected_write(row: &Value) -> bool {
    let service = row.get("service").and_then(Value::as_str).unwrap_or("");
    let class_name = row.get("className").and_then(Value::as_str).unwrap_or("");
    let property_name = row.get("name").and_then(Value::as_str).unwrap_or("");
    let path_segments = row
        .get("pathSegments")
        .and_then(Value::as_array)
        .map(|segments| {
            segments
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    is_externally_managed_editor_property(service, class_name, &path_segments, property_name)
}

pub(super) fn is_user_facing_protected_write(
    row: &Value,
    database: &ReflectionDatabase<'_>,
) -> bool {
    let Some(name) = row.get("name").and_then(Value::as_str) else {
        return false;
    };
    if name.starts_with("RBX_") {
        return false;
    }
    if row.get("kind").and_then(Value::as_str) == Some("attribute") {
        return true;
    }
    let Some(class_name) = row.get("className").and_then(Value::as_str) else {
        return false;
    };
    if class_name == MATERIAL_SERVICE_CLASS && name == USE_2022_MATERIALS_PROPERTY {
        return row.get("service").and_then(Value::as_str) == Some(MATERIAL_SERVICE_CLASS)
            && row
                .get("pathSegments")
                .and_then(Value::as_array)
                .is_some_and(|segments| {
                    segments.len() == 1 && segments[0].as_str() == Some(MATERIAL_SERVICE_CLASS)
                });
    }
    let Some(descriptor) = rbx_property_descriptor(database, class_name, name) else {
        return false;
    };
    if is_engine_managed_editor_property(class_name, name, database) {
        return false;
    }
    if matches!(&descriptor.data_type, RbxDataType::Enum(name) if *name == "RolloutState") {
        return false;
    }
    if !matches!(descriptor.scriptability, RbxScriptability::Read) {
        return false;
    }
    class_name == "MeshPart"
        && name == "MeshId"
        && rbx_model_property_descriptor(database, class_name, name).is_some()
}

pub(super) fn property_schema_entry<'a>(
    property_schema_by_class: &'a PropertySchemaMap,
    class_name: &str,
    property_name: &str,
) -> Option<&'a PropertySchemaEntry> {
    property_schema_by_class
        .get(class_name)?
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(property_name))
}

pub(super) fn normalize_editor_bridge_value(
    value: &Value,
    schema_entry: Option<&PropertySchemaEntry>,
    paths_by_index: &[Option<EditorInstancePath>],
    settings_ids_by_index: &[&str],
) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| {
                    normalize_editor_bridge_value(item, None, paths_by_index, settings_ids_by_index)
                })
                .collect(),
        ),
        Value::Object(object) => normalize_editor_bridge_object(
            object,
            schema_entry,
            paths_by_index,
            settings_ids_by_index,
        ),
        _ => value.clone(),
    }
}

fn normalize_editor_bridge_object(
    object: &Map<String, Value>,
    schema_entry: Option<&PropertySchemaEntry>,
    paths_by_index: &[Option<EditorInstancePath>],
    settings_ids_by_index: &[&str],
) -> Value {
    if object.get("_type").and_then(Value::as_str) == Some("Ref") {
        return normalize_editor_ref_value(object, paths_by_index, settings_ids_by_index);
    }
    if let Some(ref_object) = object.get("Ref").and_then(Value::as_object) {
        return normalize_editor_ref_value(ref_object, paths_by_index, settings_ids_by_index);
    }
    if let Some(number) = object.get("BrickColor") {
        let mut out = Map::new();
        out.insert("_type".to_string(), Value::String("BrickColor".to_string()));
        out.insert("number".to_string(), number.clone());
        return Value::Object(out);
    }
    if let Some(sequence) = object.get("ColorSequence") {
        return normalize_editor_sequence_value(sequence, "ColorSequence");
    }
    if let Some(sequence) = object.get("NumberSequence") {
        return normalize_editor_sequence_value(sequence, "NumberSequence");
    }
    if object.get("_type").and_then(Value::as_str) == Some("ColorSequence") {
        return normalize_editor_sequence_value(&Value::Object(object.clone()), "ColorSequence");
    }
    if object.get("_type").and_then(Value::as_str) == Some("NumberSequence") {
        return normalize_editor_sequence_value(&Value::Object(object.clone()), "NumberSequence");
    }

    let mut out = Map::with_capacity(object.len() + 1);
    for (key, nested) in object {
        out.insert(
            key.clone(),
            normalize_editor_bridge_value(nested, None, paths_by_index, settings_ids_by_index),
        );
    }
    if object.get("_type").and_then(Value::as_str) == Some("EnumItem")
        && !object.contains_key("enumType")
        && let Some(enum_type) = schema_entry.and_then(|entry| entry.enum_type.as_ref())
    {
        out.insert("enumType".to_string(), Value::String(enum_type.clone()));
    }
    Value::Object(out)
}

fn normalize_editor_ref_value(
    object: &Map<String, Value>,
    paths_by_index: &[Option<EditorInstancePath>],
    settings_ids_by_index: &[&str],
) -> Value {
    let mut out = Map::with_capacity(object.len() + 3);
    for (key, nested) in object {
        out.insert(
            key.clone(),
            normalize_editor_bridge_value(nested, None, paths_by_index, settings_ids_by_index),
        );
    }
    out.insert("_type".to_string(), Value::String("Ref".to_string()));
    if let Some(instance_index) = object.get("instanceIndex").and_then(Value::as_u64)
        && let Some(zero_index) = instance_index.checked_sub(1).map(|value| value as usize)
    {
        if let Some(settings_id) = settings_ids_by_index.get(zero_index) {
            out.insert(
                "settingsId".to_string(),
                Value::String((*settings_id).to_string()),
            );
        }
        if let Some(Some(path)) = paths_by_index.get(zero_index) {
            out.insert(
                "pathSegments".to_string(),
                Value::Array(
                    path.path_segments
                        .iter()
                        .map(|segment| Value::String(segment.clone()))
                        .collect(),
                ),
            );
            out.insert(
                "pathOrdinals".to_string(),
                Value::Array(
                    path.path_ordinals
                        .iter()
                        .map(|ordinal| Value::Number(serde_json::Number::from(*ordinal as u64)))
                        .collect(),
                ),
            );
        }
    }
    Value::Object(out)
}

fn normalize_editor_sequence_value(value: &Value, type_name: &str) -> Value {
    let sequence = value.as_object();
    let keypoints = sequence
        .and_then(|object| {
            object
                .get("keypoints")
                .or_else(|| object.get("Keypoints"))
                .and_then(Value::as_array)
        })
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out_keypoints = Vec::with_capacity(keypoints.len());
    for raw_keypoint in keypoints {
        let Some(keypoint) = raw_keypoint.as_object() else {
            continue;
        };
        let mut out = Map::new();
        out.insert(
            "time".to_string(),
            keypoint
                .get("time")
                .or_else(|| keypoint.get("Time"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        if type_name == "ColorSequence" {
            let color = keypoint
                .get("value")
                .or_else(|| keypoint.get("color"))
                .or_else(|| keypoint.get("Value"));
            out.insert("value".to_string(), normalize_editor_color3_value(color));
        } else {
            out.insert(
                "value".to_string(),
                keypoint
                    .get("value")
                    .or_else(|| keypoint.get("Value"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            out.insert(
                "envelope".to_string(),
                keypoint
                    .get("envelope")
                    .or_else(|| keypoint.get("Envelope"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        out_keypoints.push(Value::Object(out));
    }

    let mut out = Map::new();
    out.insert("_type".to_string(), Value::String(type_name.to_string()));
    out.insert("keypoints".to_string(), Value::Array(out_keypoints));
    Value::Object(out)
}

fn normalize_editor_color3_value(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return json!({ "_type": "Color3", "r": 0, "g": 0, "b": 0 });
    };
    if let Some(items) = value.as_array() {
        return json!({
            "_type": "Color3",
            "r": items.first().cloned().unwrap_or(Value::Null),
            "g": items.get(1).cloned().unwrap_or(Value::Null),
            "b": items.get(2).cloned().unwrap_or(Value::Null),
        });
    }
    if let Some(object) = value.as_object() {
        let mut out = Map::new();
        out.insert("_type".to_string(), Value::String("Color3".to_string()));
        out.insert(
            "r".to_string(),
            object
                .get("r")
                .or_else(|| object.get("R"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        out.insert(
            "g".to_string(),
            object
                .get("g")
                .or_else(|| object.get("G"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        out.insert(
            "b".to_string(),
            object
                .get("b")
                .or_else(|| object.get("B"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        return Value::Object(out);
    }
    json!({ "_type": "Color3", "r": 0, "g": 0, "b": 0 })
}

fn editor_review_value(value: &Value) -> Value {
    let encoded_len = serde_json::to_vec(value).map_or(0, |encoded| encoded.len());
    if encoded_len <= 512 {
        return value.clone();
    }
    let summary = match value {
        Value::String(text) => {
            format!("String ({} characters)", text.chars().count())
        }
        Value::Array(items) => format!("Array ({} values)", items.len()),
        Value::Object(object) => {
            let label = object
                .get("_type")
                .and_then(Value::as_str)
                .unwrap_or("Object");
            if let Some(base64) = object.get("base64").and_then(Value::as_str) {
                let padding = base64
                    .as_bytes()
                    .iter()
                    .rev()
                    .take_while(|byte| **byte == b'=')
                    .count();
                let bytes = base64.len().saturating_mul(3) / 4 - padding;
                format!("{label} ({bytes} bytes)")
            } else {
                format!("{label} ({encoded_len} bytes)")
            }
        }
        _ => return value.clone(),
    };
    json!({ "_reviewTruncated": true, "summary": summary })
}

struct EditorReviewTarget<'a> {
    service: &'a str,
    settings_id: Option<&'a str>,
    path_segments: &'a [String],
    path_ordinals: &'a [usize],
    class_name: &'a str,
}

fn append_editor_review_entry(
    rows: &mut Vec<Value>,
    row_index_by_key: &mut HashMap<String, usize>,
    target: EditorReviewTarget<'_>,
    entry: Value,
) {
    let key = target.settings_id.map_or_else(
        || {
            serde_json::to_string(&(target.service, target.path_segments, target.path_ordinals))
                .unwrap_or_else(|_| {
                    format!("{}:{}", target.service, target.path_segments.join("."))
                })
        },
        |settings_id| format!("{}\0{settings_id}", target.service),
    );
    let row_index = if let Some(index) = row_index_by_key.get(&key) {
        *index
    } else {
        let index = rows.len();
        rows.push(json!({
            "service": target.service,
            "settingsId": target.settings_id,
            "pathSegments": target.path_segments,
            "pathOrdinals": target.path_ordinals,
            "className": target.class_name,
            "entries": [],
        }));
        row_index_by_key.insert(key, index);
        index
    };
    if let Some(entries) = rows[row_index]
        .as_object_mut()
        .and_then(|row| row.get_mut("entries"))
        .and_then(Value::as_array_mut)
    {
        entries.push(entry);
    }
}

pub(super) fn editor_review_payload(changes: &EditorChangeSet) -> (u64, Vec<Value>) {
    let mut rows = Vec::new();
    let mut row_index_by_key = HashMap::new();
    let mut change_count: u64 = 0;
    for change in &changes.instance_changes {
        let kind = match change.mode.as_str() {
            "deleteInstances" => "instanceRemove",
            "replaceInstances" => "instanceReplace",
            "upsertInstances" => "instanceAdd",
            _ => "instanceReconcile",
        };
        for instance in &change.instances {
            change_count += 1;
            append_editor_review_entry(
                &mut rows,
                &mut row_index_by_key,
                EditorReviewTarget {
                    service: &change.service,
                    settings_id: Some(&instance.settings_id),
                    path_segments: &instance.path_segments,
                    path_ordinals: &instance.path_ordinals,
                    class_name: &instance.class_name,
                },
                json!({ "kind": kind, "allowDeletes": change.allow_deletes }),
            );
        }
    }
    for change in &changes.source_changes {
        change_count += 1;
        append_editor_review_entry(
            &mut rows,
            &mut row_index_by_key,
            EditorReviewTarget {
                service: &change.service,
                settings_id: change.settings_id.as_deref(),
                path_segments: &change.path_segments,
                path_ordinals: &change.path_ordinals,
                class_name: &change.class_name,
            },
            json!({ "kind": "source", "deleted": change.deleted }),
        );
    }
    for change in &changes.property_changes {
        for (kind, values) in [
            ("property", &change.properties),
            ("attribute", &change.attributes),
        ] {
            for (name, value) in values {
                change_count += 1;
                append_editor_review_entry(
                    &mut rows,
                    &mut row_index_by_key,
                    EditorReviewTarget {
                        service: &change.service,
                        settings_id: change.settings_id.as_deref(),
                        path_segments: &change.path_segments,
                        path_ordinals: &change.path_ordinals,
                        class_name: &change.class_name,
                    },
                    json!({
                        "kind": kind,
                        "name": name,
                        "value": editor_review_value(value),
                    }),
                );
            }
        }
        for name in &change.deleted_attributes {
            change_count += 1;
            append_editor_review_entry(
                &mut rows,
                &mut row_index_by_key,
                EditorReviewTarget {
                    service: &change.service,
                    settings_id: change.settings_id.as_deref(),
                    path_segments: &change.path_segments,
                    path_ordinals: &change.path_ordinals,
                    class_name: &change.class_name,
                },
                json!({
                    "kind": "attribute",
                    "name": name,
                    "deleted": true,
                }),
            );
        }
    }
    (change_count, rows)
}

fn editor_review_chunks(rows: Vec<Value>, max_bytes: usize) -> Result<Vec<Vec<Value>>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut chunk_bytes = 2usize;
    for row in rows {
        let row_bytes = serde_json::to_vec(&row)?.len() + usize::from(!chunk.is_empty());
        if row_bytes + 2 > max_bytes {
            bail!("One editor review row is too large to preview safely");
        }
        if !chunk.is_empty() && chunk_bytes + row_bytes > max_bytes {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = 2;
        }
        chunk_bytes += row_bytes;
        chunk.push(row);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    Ok(chunks)
}

pub(super) fn request_editor_push_review(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
) -> Result<bool> {
    let (change_count, rows) = editor_review_payload(changes);
    if change_count == 0 {
        return Ok(true);
    }
    let row_count = rows.len();
    if global_log_enabled(5) {
        println!(
            "[renium] review rows: {}",
            serde_json::to_string(&rows).unwrap_or_default()
        );
    }
    let chunks = editor_review_chunks(rows, 1024 * 1024)?;
    let response = match if chunks.len() == 1 {
        bridge.call(
            "requestEditorPushReview",
            json!({
                "changeCount": change_count,
                "rows": chunks.into_iter().next().unwrap_or_default(),
            }),
        )
    } else {
        let upload_id = format!("{}-{change_count}", current_millis());
        bridge.call(
            "beginEditorPushReview",
            json!({
                "uploadId": &upload_id,
                "changeCount": change_count,
                "rowCount": row_count,
                "totalChunks": chunks.len(),
            }),
        )?;
        let upload_result = (|| -> Result<Value> {
            let transfer_threads = bridge
                .channel_count()
                .saturating_sub(1)
                .max(1)
                .min(chunks.len());
            rayon::ThreadPoolBuilder::new()
                .num_threads(transfer_threads)
                .build()
                .context("Failed to initialize editor review transfer workers")?
                .install(|| {
                    chunks
                        .par_iter()
                        .enumerate()
                        .try_for_each(|(index, rows)| -> Result<()> {
                            bridge.call(
                                "appendEditorPushReview",
                                json!({
                                    "uploadId": &upload_id,
                                    "index": index + 1,
                                    "rows": rows,
                                }),
                            )?;
                            Ok(())
                        })
                })?;
            bridge.call("finishEditorPushReview", json!({ "uploadId": &upload_id }))
        })();
        if upload_result.is_err() {
            let _ = bridge.call("cancelEditorPushReview", json!({ "uploadId": &upload_id }));
        }
        upload_result
    } {
        Ok(value) => value,
        Err(err) => {
            return Err(err.context("Editor push review is unavailable"));
        }
    };
    wait_for_editor_review_decision(bridge, response, change_count, "editor push")
}

pub(super) fn request_protected_write_review(
    bridge: &BridgeServer,
    rows: &[Value],
) -> Result<bool> {
    if rows.is_empty() {
        return Ok(true);
    }
    let response = bridge
        .call("requestProtectedWriteReview", json!({ "rows": rows }))
        .context("Protected property review is unavailable")?;
    wait_for_editor_review_decision(
        bridge,
        response,
        rows.len() as u64,
        "protected property fallback",
    )
}

pub(super) fn protected_write_matches_previous(row: &Value) -> bool {
    if row.get("oldValueKnown").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let old_value_missing = row.get("oldValueMissing").and_then(Value::as_bool) == Some(true);
    if row.get("deleted").and_then(Value::as_bool) == Some(true) {
        return old_value_missing;
    }
    !old_value_missing
        && row
            .get("oldValue")
            .zip(row.get("value"))
            .is_some_and(|(previous, requested)| {
                previous == requested || protected_enum_values_equal(previous, requested)
            })
}

fn protected_enum_values_equal(previous: &Value, requested: &Value) -> bool {
    let Some(previous) = previous.as_object() else {
        return false;
    };
    let Some(requested) = requested.as_object() else {
        return false;
    };
    if previous.get("_type").and_then(Value::as_str) != Some("EnumItem")
        || requested.get("_type").and_then(Value::as_str) != Some("EnumItem")
    {
        return false;
    }
    let previous_type = previous.get("enumType").and_then(Value::as_str);
    let requested_type = requested.get("enumType").and_then(Value::as_str);
    if previous_type.is_some() && requested_type.is_some() && previous_type != requested_type {
        return false;
    }
    match (
        previous.get("name").and_then(Value::as_str),
        requested.get("name").and_then(Value::as_str),
    ) {
        (Some(previous), Some(requested)) => previous == requested,
        _ => previous.get("value") == requested.get("value"),
    }
}

pub(super) fn protected_write_rows_with_previous_values(
    path: &Path,
    rows: &[Value],
) -> Result<Vec<Value>> {
    let format = RbxPlaceFormat::from_path(path)?;
    let input = File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let reader = BufReader::new(input);
    let dom = match format {
        RbxPlaceFormat::Binary => rbx_binary::from_reader(reader)
            .with_context(|| format!("Failed to read {}", path.display()))?,
        RbxPlaceFormat::Xml => rbx_xml::from_reader_default(reader)
            .with_context(|| format!("Failed to read {}", path.display()))?,
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let refs = rbx_dom_path_import_refs(&dom, true);
    let attributes_key = rbx_dom_weak::Ustr::from("Attributes");
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut row = row.clone();
        let Some(object) = row.as_object_mut() else {
            out.push(row);
            continue;
        };
        let path_segments = object
            .get("pathSegments")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let path_ordinals = object
            .get("pathOrdinals")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|value| value as usize)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Some(name) = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            out.push(row);
            continue;
        };
        let Ok(referent) = rbx_dom_instance_by_path_unique(&dom, &path_segments, &path_ordinals)
        else {
            out.push(row);
            continue;
        };
        let Some(instance) = dom.get_by_ref(referent) else {
            out.push(row);
            continue;
        };
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("property");
        let previous = if kind == "attribute" {
            instance
                .properties
                .get(&attributes_key)
                .and_then(|value| match value {
                    RbxVariant::Attributes(attributes) => attributes.get(name.as_str()),
                    _ => None,
                })
                .and_then(|value| rbx_variant_to_settings_json(value, None, database, &refs))
        } else {
            let class_name = instance.class.as_str();
            let descriptor = rbx_model_property_descriptor(database, class_name, &name);
            let serialized_name = descriptor.map(|value| value.name).unwrap_or(name.as_str());
            let find_value = |property_name: &str| {
                instance
                    .properties
                    .get(&rbx_dom_weak::Ustr::from(property_name))
                    .or_else(|| {
                        database
                            .classes
                            .get(class_name)
                            .and_then(|class| database.find_default_property(class, property_name))
                    })
            };
            let mut value = find_value(serialized_name);
            let mut value_descriptor = descriptor;
            if value.is_none()
                && let Some(RbxPropertyDescriptor {
                    kind:
                        RbxPropertyKind::Canonical {
                            serialization: RbxPropertySerialization::Migrate(migration),
                        },
                    ..
                }) = descriptor
            {
                for migrated_name in migration.new_property_names() {
                    if let Some(migrated_value) = find_value(migrated_name) {
                        value = Some(migrated_value);
                        value_descriptor =
                            rbx_model_property_descriptor(database, class_name, migrated_name);
                        break;
                    }
                }
            }
            value.and_then(|value| {
                rbx_variant_to_settings_json(value, value_descriptor, database, &refs)
            })
        };
        object.insert("oldValueKnown".to_string(), Value::Bool(true));
        if let Some(previous) = previous {
            object.insert("oldValue".to_string(), previous);
        } else {
            object.insert("oldValueMissing".to_string(), Value::Bool(true));
        }
        out.push(row);
    }
    Ok(out)
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn protected_root_write_rows_with_live_values(
    bridge: &BridgeServer,
    rows: Vec<Value>,
) -> std::result::Result<Vec<Value>, Vec<Value>> {
    let Ok(database) = rbx_reflection_database::get() else {
        return Err(rows);
    };
    let Ok(serialized_values) =
        read_live_service_root_property_values(bridge, MATERIAL_SERVICE_CLASS, database)
    else {
        return Err(rows);
    };
    let mut values = Map::new();
    merge_live_service_root_property_values(
        MATERIAL_SERVICE_CLASS,
        &mut values,
        &serialized_values,
        database,
    );
    let mut enriched = Vec::with_capacity(rows.len());
    for mut row in rows {
        let Some(object) = row.as_object_mut() else {
            enriched.push(row);
            continue;
        };
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        object.insert("oldValueKnown".to_string(), Value::Bool(true));
        if let Some(value) = name.as_deref().and_then(|name| values.get(name)) {
            object.insert("oldValue".to_string(), value.clone());
        } else {
            object.insert("oldValueMissing".to_string(), Value::Bool(true));
        }
        enriched.push(row);
    }
    Ok(enriched)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn protected_root_write_rows_with_live_values(
    _bridge: &BridgeServer,
    rows: Vec<Value>,
) -> std::result::Result<Vec<Value>, Vec<Value>> {
    Err(rows)
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn studio_pid_for_bridge(bridge: &BridgeServer) -> Result<u32> {
    #[cfg(windows)]
    let target = BridgeTarget::Main;
    #[cfg(target_os = "macos")]
    let target = BridgeTarget::Edit;
    let peer = bridge.peer_for_selector(target, None)?;
    let port = peer
        .rsplit(':')
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .with_context(|| format!("Could not parse peer port from '{peer}'"))?;
    pid_for_local_tcp_port(port)
        .with_context(|| format!("Could not map bridge connection {peer} to Studio"))
}

#[cfg(windows)]
pub(super) fn studio_title_for_bridge(bridge: &BridgeServer, pid: u32) -> Result<String> {
    input_inject::studio_window_title(pid).or_else(|_| {
        Ok(bridge
            .cached_bridge_info_for_target(BridgeTarget::Edit)?
            .place_name)
    })
}

#[cfg(target_os = "macos")]
pub(super) fn studio_title_for_bridge(bridge: &BridgeServer, _pid: u32) -> Result<String> {
    Ok(bridge
        .cached_bridge_info_for_target(BridgeTarget::Edit)?
        .place_name)
}

#[cfg(windows)]
fn local_place_path_from_studio_title(title: &str) -> Option<PathBuf> {
    let value = title.strip_suffix(" - Roblox Studio")?.trim();
    let path = PathBuf::from(value);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    (path.is_absolute() && matches!(extension.as_str(), "rbxl" | "rbxlx")).then_some(path)
}

#[cfg(windows)]
pub(super) fn local_place_path_for_bridge(bridge: &BridgeServer) -> Option<PathBuf> {
    let pid = studio_pid_for_bridge(bridge).ok()?;
    let title = input_inject::studio_window_title(pid).ok()?;
    local_place_path_from_studio_title(&title)
}

#[cfg(not(windows))]
pub(super) fn local_place_path_for_bridge(_bridge: &BridgeServer) -> Option<PathBuf> {
    None
}

#[cfg(any(windows, test))]
pub(super) fn patch_place_protected_writes(path: &Path, rows: &[Value]) -> Result<usize> {
    let format = RbxPlaceFormat::from_path(path)?;
    let input = File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let reader = BufReader::new(input);
    let mut dom = match format {
        RbxPlaceFormat::Binary => rbx_binary::from_reader(reader)
            .with_context(|| format!("Failed to read {}", path.display()))?,
        RbxPlaceFormat::Xml => rbx_xml::from_reader_default(reader)
            .with_context(|| format!("Failed to read {}", path.display()))?,
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let refs = BytecodeModelExportRefs {
        by_index: HashMap::new(),
        by_settings_id: HashMap::new(),
        global_by_settings_id: None,
        by_path_key: HashMap::new(),
        global_by_path_key: None,
        by_path_segments_key: HashMap::new(),
        global_by_path_segments_key: None,
    };
    let mut applied = 0usize;
    for row in rows {
        let path_segments = row
            .get("pathSegments")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let path_ordinals = row
            .get("pathOrdinals")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|value| value as usize)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .context("Protected write is missing its name")?;
        let referent = rbx_dom_instance_by_path_unique(&dom, &path_segments, &path_ordinals)?;
        let kind = row
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("property");
        let deleted = row.get("deleted").and_then(Value::as_bool) == Some(true);
        let value = row.get("value");
        if deleted && kind != "attribute" {
            bail!("Only protected attributes can be deleted");
        }
        if kind == "attribute" {
            let instance = dom
                .get_by_ref_mut(referent)
                .context("Protected write target disappeared")?;
            let attributes_key = rbx_dom_weak::Ustr::from("Attributes");
            let mut attributes = instance
                .properties
                .get(&attributes_key)
                .and_then(|value| match value {
                    RbxVariant::Attributes(attributes) => Some(attributes.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            if deleted {
                attributes.remove(name);
            } else {
                let variant = json_to_rbx_attribute_variant(
                    value.context("Protected write is missing its value")?,
                    database,
                    &refs,
                )
                .with_context(|| format!("Could not encode protected attribute {name}"))?;
                attributes.insert(name.to_string(), variant);
            }
            instance
                .properties
                .insert(attributes_key, RbxVariant::Attributes(attributes));
        } else {
            let class_name = dom
                .get_by_ref(referent)
                .map(|instance| instance.class.to_string())
                .context("Protected write target disappeared")?;
            let descriptor = rbx_model_property_descriptor(database, &class_name, name);
            let legacy_variant = json_to_rbx_property_variant(
                value.context("Protected write is missing its value")?,
                descriptor,
                database,
                &refs,
            )
            .with_context(|| format!("Could not encode protected property {name}"))?;
            let mut serialized_name = descriptor.map(|value| value.name).unwrap_or(name);
            let mut variant = legacy_variant;
            if let Some(RbxPropertyDescriptor {
                kind:
                    RbxPropertyKind::Canonical {
                        serialization: RbxPropertySerialization::Migrate(migration),
                    },
                ..
            }) = descriptor
                && let [migrated_name] = migration.new_property_names()
            {
                variant = migration.perform(&variant).with_context(|| {
                    format!("Could not migrate protected property {name} to {migrated_name}")
                })?;
                serialized_name = migrated_name;
            }
            let instance = dom
                .get_by_ref_mut(referent)
                .context("Protected write target disappeared")?;
            if serialized_name != name {
                instance.properties.remove(&rbx_dom_weak::Ustr::from(name));
            }
            instance.properties.insert(serialized_name.into(), variant);
        }
        applied += 1;
    }
    let top_level_refs = rbx_model_top_level_refs(&dom);
    let output =
        File::create(path).with_context(|| format!("Failed to write {}", path.display()))?;
    let writer = BufWriter::new(output);
    match format {
        RbxPlaceFormat::Binary => rbx_binary::to_writer(writer, &dom, &top_level_refs)
            .with_context(|| format!("Failed to write {}", path.display()))?,
        RbxPlaceFormat::Xml => rbx_xml::to_writer_default(writer, &dom, &top_level_refs)
            .with_context(|| format!("Failed to write {}", path.display()))?,
    }
    Ok(applied)
}

#[cfg(windows)]
pub(super) fn apply_protected_writes_offline(
    bridge: &BridgeServer,
    args: &PushEditorChangesArgs,
    rows: &[Value],
) -> Result<Value> {
    let pid = studio_pid_for_bridge(bridge)?;
    let executable = input_inject::process_executable_path(pid)?;
    let title = input_inject::studio_window_title(pid)?;
    let original_path = local_place_path_from_studio_title(&title);
    let extension = original_path
        .as_ref()
        .and_then(|path| path.extension())
        .and_then(|value| value.to_str())
        .filter(|value| matches!(value.to_ascii_lowercase().as_str(), "rbxl" | "rbxlx"))
        .unwrap_or("rbxl");
    let snapshot_name = format!(
        ".renium-protected-{}-{}.{}",
        pid,
        current_millis(),
        extension
    );
    let snapshot = original_path
        .as_ref()
        .and_then(|path| path.parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.join(&snapshot_name))
        .unwrap_or_else(|| std::env::temp_dir().join(&snapshot_name));
    let local_file = original_path.is_some();
    let exported_instances =
        match write_live_editor_place_snapshot(bridge, args, &snapshot, original_path.as_deref()) {
            Ok(count) => count,
            Err(error) => {
                let _ = fs::remove_file(&snapshot);
                return Err(error);
            }
        };
    let applied = match patch_place_protected_writes(&snapshot, rows) {
        Ok(applied) => applied,
        Err(error) => {
            let _ = fs::remove_file(&snapshot);
            return Err(error);
        }
    };
    input_inject::terminate_studio_process(pid)?;
    let reopen_path = if let Some(original_path) = original_path {
        let file_name = original_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("place.rbxl");
        let backup = original_path.with_file_name(format!(
            ".{file_name}.renium-backup-{}-{}",
            pid,
            current_millis()
        ));
        if let Err(error) = fs::rename(&original_path, &backup) {
            let _ = Command::new(&executable).arg(&original_path).spawn();
            return Err(error).with_context(|| {
                format!(
                    "Failed to prepare {} for protected snapshot replacement",
                    original_path.display()
                )
            });
        }
        if let Err(error) = fs::rename(&snapshot, &original_path) {
            let restore = fs::rename(&backup, &original_path);
            if restore.is_ok() {
                let _ = Command::new(&executable).arg(&original_path).spawn();
            }
            if let Err(restore_error) = restore {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to replace {} and failed to restore {}: {restore_error}",
                        original_path.display(),
                        backup.display()
                    )
                });
            }
            return Err(error).with_context(|| {
                format!(
                    "Failed to replace {} with protected snapshot",
                    original_path.display()
                )
            });
        }
        if let Err(error) = Command::new(&executable).arg(&original_path).spawn() {
            let preserve = fs::rename(&original_path, &snapshot);
            let restore = preserve
                .as_ref()
                .map_err(|value| io::Error::other(format!("not attempted: {value}")))
                .and_then(|_| fs::rename(&backup, &original_path));
            if restore.is_ok() && preserve.is_ok() {
                let _ = Command::new(&executable).arg(&original_path).spawn();
            }
            return Err(error).with_context(|| {
                format!(
                    "Failed to reopen Studio; replacement preservation: {}; original restoration: {}; backup: {}",
                    preserve
                        .as_ref()
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|value| value.to_string()),
                    restore
                        .as_ref()
                        .map(|_| "ok".to_string())
                        .unwrap_or_else(|value| value.to_string()),
                    backup.display()
                )
            });
        }
        if let Err(error) = fs::remove_file(&backup) {
            eprintln!(
                "[renium] warning: could not remove protected-write backup {}: {error}",
                backup.display()
            );
        }
        original_path
    } else {
        Command::new(&executable)
            .arg(&snapshot)
            .spawn()
            .with_context(|| format!("Failed to reopen Studio from {}", executable.display()))?;
        snapshot
    };
    Ok(json!({
        "ok": true,
        "applied": applied,
        "exportedInstances": exported_instances,
        "reopenedPath": reopen_path,
        "localFile": local_file,
        "cloudSaved": false,
        "nativeSnapshot": true,
    }))
}

#[cfg(not(windows))]
pub(super) fn apply_protected_writes_offline(
    _bridge: &BridgeServer,
    _args: &PushEditorChangesArgs,
    _rows: &[Value],
) -> Result<Value> {
    bail!("Protected offline place writes currently require Windows")
}
