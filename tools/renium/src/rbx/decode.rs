use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rbx_dom_weak::types::{
    Attributes as RbxAttributes, CFrame as RbxCFrame, Color3 as RbxColor3, Content as RbxContent,
    ContentType as RbxContentType, Font as RbxFont, FontStyle as RbxFontStyle,
    FontWeight as RbxFontWeight, PhysicalProperties as RbxPhysicalProperties, Ref as RbxRef,
    Variant as RbxVariant, VariantType as RbxVariantType,
};
use rbx_reflection::{
    DataType as RbxDataType, PropertyDescriptor as RbxPropertyDescriptor,
    PropertyKind as RbxPropertyKind, PropertySerialization as RbxPropertySerialization,
    PropertyTag as RbxPropertyTag, ReflectionDatabase, Scriptability as RbxScriptability,
};
use serde_json::{Map, Number, Value, json};

use crate::app::timing::elapsed_ms;
use crate::editor::sync::is_lua_source_class;
use crate::rbx::encode::{
    binary_payload_json, enum_item_name_by_value, model_property_name_is_skipped,
    rbx_model_property_descriptor, rbx_property_descriptor, strip_enum_prefix,
};
use crate::rbx::model::{BytecodeModelImportRefs, imported_instance_index};
use crate::roblox::schema::{
    AXIS_NAMES, EnumValueNameMap, FACE_NAMES, PropertySchemaMap, TYPE_ID_REF,
};
use crate::snapshot::codec::{
    bitmask_names, decode_native_overlay_debug_ids, parse_native_overlay_class_groups,
};
use crate::snapshot::export::{fetch_typed_payload_with_size, is_supported_bridge_codec};
use crate::snapshot::types::{
    NativeOverlayFetch, NativeOverlayItem, NativeOverlayPayload, NativeSettingsProperty,
    NativeSettingsValue, SnapshotInstance,
};
use crate::studio::bridge::{BridgeServer, DEFAULT_EXPORT_CHUNK_SIZE};
use crate::studio::native::editor::rbx_variant_referent;

const NATIVE_OVERLAY_PROTOCOL_VERSION: &str = "native-overlay-v3";

pub(crate) struct NativePropertyFilter {
    pub(crate) allowed: HashSet<String>,
    pub(crate) renamed: HashMap<String, String>,
    pub(crate) output_names: HashSet<String>,
    pub(crate) reconstruct_decal_color_map: bool,
    pub(crate) reconstruct_weld_enabled: bool,
}

fn native_property_data_type_supported(data_type: &RbxDataType<'_>) -> bool {
    match data_type {
        RbxDataType::Enum(_) => true,
        RbxDataType::Value(value_type) => matches!(
            value_type,
            RbxVariantType::Bool
                | RbxVariantType::Int32
                | RbxVariantType::Int64
                | RbxVariantType::Float32
                | RbxVariantType::Float64
                | RbxVariantType::String
                | RbxVariantType::BinaryString
                | RbxVariantType::ContentId
                | RbxVariantType::Content
                | RbxVariantType::Ref
                | RbxVariantType::Vector2
                | RbxVariantType::Vector3
                | RbxVariantType::UDim
                | RbxVariantType::UDim2
                | RbxVariantType::Color3
                | RbxVariantType::Color3uint8
                | RbxVariantType::ColorSequence
                | RbxVariantType::NumberSequence
                | RbxVariantType::NumberRange
                | RbxVariantType::CFrame
                | RbxVariantType::OptionalCFrame
                | RbxVariantType::Rect
                | RbxVariantType::Font
                | RbxVariantType::BrickColor
                | RbxVariantType::PhysicalProperties
                | RbxVariantType::Axes
                | RbxVariantType::Faces
                | RbxVariantType::Ray
                | RbxVariantType::MaterialColors
        ),
        _ => false,
    }
}

fn native_property_descriptor_supported(descriptor: &RbxPropertyDescriptor<'_>) -> bool {
    let RbxPropertyKind::Canonical { serialization } = &descriptor.kind else {
        return false;
    };
    if matches!(serialization, RbxPropertySerialization::DoesNotSerialize) {
        return false;
    }
    let serializes_as = matches!(serialization, RbxPropertySerialization::SerializesAs(_));
    if descriptor.tags.iter().any(|tag| {
        matches!(
            tag,
            RbxPropertyTag::Deprecated
                | RbxPropertyTag::Hidden
                | RbxPropertyTag::NotBrowsable
                | RbxPropertyTag::WriteOnly
        ) || (*tag == RbxPropertyTag::ReadOnly && !serializes_as)
    }) {
        return false;
    }
    if !matches!(
        descriptor.scriptability,
        RbxScriptability::ReadWrite | RbxScriptability::Read | RbxScriptability::Custom
    ) {
        return false;
    }
    native_property_data_type_supported(&descriptor.data_type)
}

pub(crate) fn rbx_reflection_class_is_a(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    target: &str,
) -> bool {
    let Some(class) = database.classes.get(class_name) else {
        return false;
    };
    database
        .superclasses_iter(class)
        .any(|descriptor| descriptor.name == target)
}

pub(crate) fn native_property_filter(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
) -> NativePropertyFilter {
    let mut allowed = HashSet::new();
    let mut renamed = HashMap::new();
    if let Some(class) = database.classes.get(class_name) {
        for descriptor in database.superclasses_iter(class) {
            for property in descriptor.properties.values() {
                if !native_property_descriptor_supported(property) {
                    continue;
                }
                match &property.kind {
                    RbxPropertyKind::Canonical {
                        serialization: RbxPropertySerialization::Migrate(migration),
                    } => {
                        allowed.extend(
                            migration
                                .new_property_names()
                                .iter()
                                .map(|name| (*name).to_string()),
                        );
                    }
                    RbxPropertyKind::Canonical {
                        serialization: RbxPropertySerialization::SerializesAs(serialized_name),
                    } => {
                        allowed.insert(property.name.to_string());
                        allowed.insert((*serialized_name).to_string());
                        renamed.insert((*serialized_name).to_string(), property.name.to_string());
                    }
                    RbxPropertyKind::Canonical { .. } => {
                        allowed.insert(property.name.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    allowed.insert("Tags".to_string());
    if rbx_reflection_class_is_a(database, class_name, "TriangleMeshPart") {
        allowed.insert("InitialSize".to_string());
        renamed.insert("InitialSize".to_string(), "MeshSize".to_string());
        allowed.insert("FluidFidelityInternal".to_string());
        renamed.insert(
            "FluidFidelityInternal".to_string(),
            "FluidFidelity".to_string(),
        );
    }
    if rbx_reflection_class_is_a(database, class_name, "Model") {
        allowed.insert("WorldPivotData".to_string());
        renamed.insert("WorldPivotData".to_string(), "WorldPivot".to_string());
    }
    let reconstruct_decal_color_map = rbx_reflection_class_is_a(database, class_name, "Decal");
    let reconstruct_weld_enabled = class_name == "WeldConstraint";
    let output_names = allowed
        .iter()
        .map(|name| renamed.get(name).unwrap_or(name).clone())
        .collect();
    NativePropertyFilter {
        allowed,
        renamed,
        output_names,
        reconstruct_decal_color_map,
        reconstruct_weld_enabled,
    }
}

fn native_overlay_property_is_reconstructed(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    property_name: &str,
) -> bool {
    if rbx_reflection_class_is_a(database, class_name, "BasePart")
        && matches!(property_name, "Position" | "Orientation" | "Rotation")
    {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "TriangleMeshPart")
        && property_name == "MeshSize"
    {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "Model") && property_name == "WorldPivot" {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "TriangleMeshPart")
        && property_name == "FluidFidelity"
    {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "Attachment")
        && matches!(
            property_name,
            "Position"
                | "Orientation"
                | "Axis"
                | "SecondaryAxis"
                | "WorldPosition"
                | "WorldOrientation"
                | "WorldAxis"
                | "WorldSecondaryAxis"
                | "WorldCFrame"
        )
    {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "BaseScript") && property_name == "Enabled" {
        return true;
    }
    if class_name == "BodyColors"
        && matches!(
            property_name,
            "HeadColor"
                | "LeftArmColor"
                | "LeftLegColor"
                | "RightArmColor"
                | "RightLegColor"
                | "TorsoColor"
        )
    {
        return true;
    }
    if class_name == "Camera"
        && matches!(property_name, "DiagonalFieldOfView" | "MaxAxisFieldOfView")
    {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "PVInstance") && property_name == "Origin" {
        return true;
    }
    if rbx_reflection_class_is_a(database, class_name, "Decal")
        && property_name == "ColorMapContent"
    {
        return true;
    }
    if class_name == "WeldConstraint" && property_name == "Enabled" {
        return true;
    }
    false
}

pub(crate) fn native_overlay_property_schemas(
    database: &ReflectionDatabase<'_>,
    property_schema_by_class: &PropertySchemaMap,
    native_filters: &HashMap<String, NativePropertyFilter>,
) -> (PropertySchemaMap, PropertySchemaMap, PropertySchemaMap) {
    let combined = property_schema_by_class
        .iter()
        .filter_map(|(class_name, entries)| {
            let native_outputs = native_filters
                .get(class_name)
                .map(|filter| &filter.output_names);
            let overlay = entries
                .iter()
                .filter(|entry| {
                    let Some(descriptor) =
                        rbx_property_descriptor(database, class_name, &entry.name)
                    else {
                        return true;
                    };
                    match &descriptor.kind {
                        RbxPropertyKind::Alias { .. }
                        | RbxPropertyKind::Canonical {
                            serialization: RbxPropertySerialization::Migrate(_),
                        } => false,
                        RbxPropertyKind::Canonical {
                            serialization: RbxPropertySerialization::DoesNotSerialize,
                        } => !native_overlay_property_is_reconstructed(
                            database,
                            class_name,
                            &entry.name,
                        ),
                        RbxPropertyKind::Canonical { .. } => {
                            entry.type_id == TYPE_ID_REF
                                || native_outputs
                                    .is_none_or(|outputs| !outputs.contains(&entry.name))
                        }
                        _ => true,
                    }
                })
                .cloned()
                .collect::<Vec<_>>();
            (!overlay.is_empty()).then(|| (class_name.clone(), overlay))
        })
        .collect::<PropertySchemaMap>();
    let mut direct = PropertySchemaMap::new();
    let mut conditional_refs = PropertySchemaMap::new();
    for (class_name, entries) in &combined {
        let native_outputs = native_filters
            .get(class_name)
            .map(|filter| &filter.output_names);
        for entry in entries {
            let target = if entry.type_id == TYPE_ID_REF
                && native_outputs.is_some_and(|outputs| outputs.contains(&entry.name))
            {
                &mut conditional_refs
            } else {
                &mut direct
            };
            target
                .entry(class_name.clone())
                .or_default()
                .push(entry.clone());
        }
    }
    (combined, direct, conditional_refs)
}

pub(crate) fn overlay_property_names_value(
    schema: &PropertySchemaMap,
    native_filters: &HashMap<String, NativePropertyFilter>,
) -> Value {
    Value::Object(
        schema
            .iter()
            .map(|(class_name, entries)| {
                (
                    class_name.clone(),
                    Value::Array(
                        entries
                            .iter()
                            .map(|entry| {
                                if entry.type_id == TYPE_ID_REF
                                    && native_filters.get(class_name).is_some_and(|filter| {
                                        filter.output_names.contains(&entry.name)
                                    })
                                {
                                    Value::Array(vec![
                                        Value::String(entry.name.clone()),
                                        Value::Bool(true),
                                    ])
                                } else {
                                    Value::String(entry.name.clone())
                                }
                            })
                            .collect(),
                    ),
                )
            })
            .collect(),
    )
}

pub(crate) fn conditional_ref_overlay_request(
    instances: &[rbx_binary::FlatInstance],
    schema: &PropertySchemaMap,
) -> (PropertySchemaMap, Value, usize) {
    let mut candidates = HashMap::<(&str, &str), Vec<usize>>::new();
    for (index, instance) in instances.iter().enumerate() {
        let class_name = instance.class.as_str();
        let Some(entries) = schema.get(class_name) else {
            continue;
        };
        for entry in entries {
            let has_native_ref = instance
                .properties
                .iter()
                .find(|(name, _)| name.as_str() == entry.name)
                .map(|(_, value)| value)
                .and_then(rbx_variant_referent)
                .and_then(RbxRef::as_u128)
                .is_some();
            if !has_native_ref {
                candidates
                    .entry((class_name, entry.name.as_str()))
                    .or_default()
                    .push(index + 1);
            }
        }
    }

    let mut request_schema = PropertySchemaMap::new();
    let mut request_names = Map::new();
    let mut candidate_count = 0;
    for (class_name, entries) in schema {
        let mut class_schema = Vec::new();
        let mut class_names = Vec::new();
        for entry in entries {
            let Some(indices) = candidates.remove(&(class_name.as_str(), entry.name.as_str()))
            else {
                continue;
            };
            candidate_count += indices.len();
            let mut packed = Vec::with_capacity(indices.len() * 3);
            for index in &indices {
                let index = *index as u32;
                packed.push(index as u8);
                packed.push((index >> 8) as u8);
                packed.push((index >> 16) as u8);
            }
            class_schema.push(entry.clone());
            class_names.push(Value::Array(vec![
                Value::String(entry.name.clone()),
                json!({
                    "packed": base64::encode(packed),
                    "count": indices.len(),
                }),
            ]));
        }
        if !class_schema.is_empty() {
            request_schema.insert(class_name.clone(), class_schema);
            request_names.insert(class_name.clone(), Value::Array(class_names));
        }
    }
    (
        request_schema,
        Value::Object(request_names),
        candidate_count,
    )
}

#[derive(Clone, Copy)]
pub(crate) struct NativeOverlayRequest<'a> {
    pub service: &'a str,
    pub start_index: usize,
    pub take_count: usize,
    pub instance_count: usize,
    pub overlay_id: &'a str,
    pub overlay_variant: &'a str,
    pub include_debug_ids: bool,
    pub overlay_names: &'a Value,
    pub overlay_schema: &'a PropertySchemaMap,
    pub enum_value_names_by_type: &'a EnumValueNameMap,
    pub class_names: &'a [String],
}

pub(crate) fn fetch_native_overlay_batches(
    bridge: &BridgeServer,
    request: NativeOverlayRequest<'_>,
) -> Result<NativeOverlayFetch> {
    fetch_native_overlay_batch_once(bridge, &request, request.start_index, request.take_count)
}

fn fetch_native_overlay_batch_once(
    bridge: &BridgeServer,
    request: &NativeOverlayRequest<'_>,
    start_index: usize,
    take_count: usize,
) -> Result<NativeOverlayFetch> {
    let service = request.service;
    let started = Instant::now();
    let (batch, metrics) = fetch_typed_payload_with_size::<NativeOverlayPayload, _>(
        DEFAULT_EXPORT_CHUNK_SIZE,
        |chunk_start, max_len| {
            bridge.call_chunk(
                "getEditorBinaryOverlayChunk",
                json!({
                    "service": service,
                    "startIndex": start_index,
                    "maxCount": take_count,
                    "chunkStart": chunk_start,
                    "maxLen": max_len,
                    "overlayId": request.overlay_id,
                    "overlayVariant": request.overlay_variant,
                    "overlayPropertiesByClass": request.overlay_names,
                    "supportsStableInstanceIds": request.include_debug_ids,
                }),
            )
        },
    )?;
    if batch.format != NATIVE_OVERLAY_PROTOCOL_VERSION
        || !is_supported_bridge_codec(&batch.codec_version)
    {
        bail!(
            "Invalid native overlay format {} with codec {} for {}",
            batch.format,
            batch.codec_version,
            service
        );
    }
    if !request.include_debug_ids
        && (!batch.debug_id_buffer.is_null()
            || !batch.debug_id_encoding.is_empty()
            || batch.debug_id_buffer_bytes != 0)
    {
        bail!("Native overlay for {service} returned unexpected debug id data");
    }
    let compact_expand_started = Instant::now();
    let debug_ids = if request.include_debug_ids {
        decode_native_overlay_debug_ids(
            &batch.debug_id_buffer,
            &batch.debug_id_encoding,
            batch.debug_id_buffer_bytes,
            take_count,
        )
        .with_context(|| format!("Invalid native debug ids for {service}"))?
    } else {
        Vec::new()
    };
    let items = parse_native_overlay_class_groups(
        Value::Array(batch.items),
        &batch.strings,
        start_index,
        take_count,
        request.overlay_schema,
        request.enum_value_names_by_type,
        request.class_names,
    )
    .with_context(|| format!("Invalid native overlay items for {service}"))?;
    if batch.total != request.instance_count {
        bail!(
            "Native overlay range for {service} returned total={}; expected total={} at start={start_index}",
            batch.total,
            request.instance_count,
        );
    }
    Ok(NativeOverlayFetch {
        metrics,
        compact_expand_ms: elapsed_ms(compact_expand_started),
        request_ms: elapsed_ms(started),
        debug_ids,
        items,
    })
}

pub(crate) fn merge_native_overlay_items(
    target: &mut [SnapshotInstance],
    additional: Vec<NativeOverlayItem>,
    class_names: &[String],
) -> Result<()> {
    for additional in additional {
        let target_instance = additional
            .instance_index
            .checked_sub(1)
            .and_then(|index| target.get_mut(index))
            .context("Native overlay instance index is out of range")?;
        let class_name = class_names
            .get(additional.class_index)
            .context("Native overlay class index is out of range")?;
        if target_instance.instance_index != Some(additional.instance_index)
            || class_name.as_str() != target_instance.class_name.as_str()
        {
            bail!(
                "Native overlay passes disagree at instance {}",
                additional.instance_index
            );
        }
        target_instance.properties.extend(additional.properties);
        target_instance.attributes.extend(additional.attributes);
    }
    Ok(())
}

pub(crate) fn rbx_instance_to_settings_records(
    instance: &rbx_dom_weak::Instance,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
    elide_defaults: bool,
    native_filter: Option<&NativePropertyFilter>,
) -> (Map<String, Value>, Map<String, Value>, Option<String>) {
    let mut records = rbx_properties_to_settings_records(
        instance.class.as_str(),
        instance.properties.iter(),
        database,
        refs,
        RbxSettingsConversionOptions {
            elide_defaults,
            defaults_already_elided: false,
            native_properties_pre_filtered: false,
            native_filter,
        },
    );
    if rbx_model_primary_part_is_set(
        database,
        instance.class.as_str(),
        instance.properties.iter(),
    ) {
        records.0.remove("WorldPivot");
    }
    records
}

pub(crate) fn rbx_model_primary_part_is_set<'a>(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    properties: impl IntoIterator<Item = (&'a rbx_dom_weak::Ustr, &'a RbxVariant)>,
) -> bool {
    rbx_reflection_class_is_a(database, class_name, "Model")
        && properties.into_iter().any(|(name, value)| {
            name.as_str() == "PrimaryPart"
                && rbx_variant_referent(value).is_some_and(|referent| !referent.is_none())
        })
}

fn native_settings_enum_name(
    enum_name: Option<&str>,
    enum_value: u32,
    database: &ReflectionDatabase<'_>,
) -> String {
    enum_name
        .map(strip_enum_prefix)
        .and_then(|name| enum_item_name_by_value(database, name, enum_value))
        .unwrap_or_else(|| enum_value.to_string())
}

fn rbx_cframe_components(value: RbxCFrame) -> [f32; 12] {
    [
        value.position.x,
        value.position.y,
        value.position.z,
        value.orientation.x.x,
        value.orientation.x.y,
        value.orientation.x.z,
        value.orientation.y.x,
        value.orientation.y.y,
        value.orientation.y.z,
        value.orientation.z.x,
        value.orientation.z.y,
        value.orientation.z.z,
    ]
}

fn rbx_variant_to_native_settings_value(
    value: &RbxVariant,
    descriptor: Option<&RbxPropertyDescriptor<'_>>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
) -> Option<NativeSettingsValue> {
    match value {
        RbxVariant::Bool(value) => Some(NativeSettingsValue::Bool(*value)),
        RbxVariant::Int32(value) => Some(NativeSettingsValue::Int(i64::from(*value))),
        RbxVariant::Int64(value) => Some(NativeSettingsValue::Int(*value)),
        RbxVariant::Float32(value) if value.is_finite() => {
            Some(NativeSettingsValue::Float32(*value))
        }
        RbxVariant::Float64(value) if value.is_finite() => {
            Some(NativeSettingsValue::Float64(*value))
        }
        RbxVariant::String(value) => Some(NativeSettingsValue::String(value.clone())),
        RbxVariant::ContentId(value) => {
            Some(NativeSettingsValue::String(value.as_str().to_string()))
        }
        RbxVariant::Content(value) => match value.value() {
            RbxContentType::None => Some(NativeSettingsValue::String(String::new())),
            RbxContentType::Uri(value) => Some(NativeSettingsValue::String(value.clone())),
            RbxContentType::Object(value) => {
                imported_instance_index(refs, *value).map(NativeSettingsValue::Ref)
            }
            _ => None,
        },
        RbxVariant::Ref(value) => {
            imported_instance_index(refs, *value).map(NativeSettingsValue::Ref)
        }
        RbxVariant::Vector2(value) => Some(NativeSettingsValue::Vector2([value.x, value.y])),
        RbxVariant::Vector3(value) => {
            Some(NativeSettingsValue::Vector3([value.x, value.y, value.z]))
        }
        RbxVariant::UDim(value) => Some(NativeSettingsValue::UDim([
            value.scale,
            value.offset as f32,
        ])),
        RbxVariant::UDim2(value) => Some(NativeSettingsValue::UDim2([
            value.x.scale,
            value.x.offset as f32,
            value.y.scale,
            value.y.offset as f32,
        ])),
        RbxVariant::Color3(value) => Some(NativeSettingsValue::Color3([value.r, value.g, value.b])),
        RbxVariant::Color3uint8(value) => {
            let value = RbxColor3::from(*value);
            Some(NativeSettingsValue::Color3([value.r, value.g, value.b]))
        }
        RbxVariant::CFrame(value) | RbxVariant::OptionalCFrame(Some(value)) => {
            Some(NativeSettingsValue::CFrame(rbx_cframe_components(*value)))
        }
        RbxVariant::Rect(value) => Some(NativeSettingsValue::Rect([
            value.min.x,
            value.min.y,
            value.max.x,
            value.max.y,
        ])),
        RbxVariant::Enum(value) => {
            let enum_name = descriptor.and_then(|descriptor| match &descriptor.data_type {
                RbxDataType::Enum(enum_name) => Some(*enum_name),
                _ => None,
            });
            Some(NativeSettingsValue::Enum(native_settings_enum_name(
                enum_name,
                value.to_u32(),
                database,
            )))
        }
        RbxVariant::EnumItem(value) => Some(NativeSettingsValue::Enum(native_settings_enum_name(
            Some(&value.ty),
            value.value,
            database,
        ))),
        _ => None,
    }
}

type NativeSettingsRecords = (
    Vec<NativeSettingsProperty>,
    Map<String, Value>,
    Map<String, Value>,
    Option<String>,
);

fn enum_property_descriptor<'db>(
    value: &RbxVariant,
    database: &'db ReflectionDatabase<'db>,
    class_name: &str,
    property_name: &str,
) -> Option<&'db RbxPropertyDescriptor<'db>> {
    if matches!(value, RbxVariant::Enum(_)) {
        rbx_model_property_descriptor(database, class_name, property_name)
            .or_else(|| rbx_property_descriptor(database, class_name, property_name))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RbxSettingsConversionOptions<'a> {
    pub(crate) elide_defaults: bool,
    pub(crate) defaults_already_elided: bool,
    pub(crate) native_properties_pre_filtered: bool,
    pub(crate) native_filter: Option<&'a NativePropertyFilter>,
}

pub(crate) fn rbx_properties_to_native_settings_records<'a>(
    class_name: &str,
    property_entries: impl IntoIterator<Item = (&'a rbx_dom_weak::Ustr, &'a RbxVariant)>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
    native_filter: Option<&NativePropertyFilter>,
) -> NativeSettingsRecords {
    let mut native_properties = Vec::new();
    let mut properties = Map::new();
    let mut attributes = Map::new();
    let mut source = None;

    for (property_name, variant) in property_entries {
        let property_name = property_name.as_str();
        if let RbxVariant::Attributes(rbx_attributes) = variant {
            attributes.extend(rbx_attributes_to_settings_map(
                rbx_attributes,
                database,
                refs,
            ));
            continue;
        }
        if model_property_name_is_skipped(property_name) {
            continue;
        }
        if property_name.eq_ignore_ascii_case("Source") && is_lua_source_class(class_name) {
            source = rbx_variant_to_source_string(variant);
            continue;
        }
        if rbx_variant_referent(variant).is_some_and(|referent| {
            imported_instance_index(refs, referent).is_none()
                && !refs.path_segments_by_ref.contains_key(&referent)
        }) {
            continue;
        }
        if matches!(variant, RbxVariant::OptionalCFrame(None)) {
            continue;
        }
        let descriptor = enum_property_descriptor(variant, database, class_name, property_name);
        let output_name = native_filter
            .and_then(|filter| filter.renamed.get(property_name))
            .map_or(property_name, String::as_str);
        let keep_json = class_name == "Script" && output_name == "RunContext"
            || native_filter.is_some_and(|filter| {
                filter.reconstruct_decal_color_map && output_name == "TextureContent"
                    || filter.reconstruct_weld_enabled && output_name == "State"
            });
        if !keep_json
            && let Some(value) =
                rbx_variant_to_native_settings_value(variant, descriptor, database, refs)
        {
            native_properties.push(NativeSettingsProperty {
                name: output_name.to_string(),
                value,
            });
        } else if let Some(value) =
            rbx_variant_to_settings_json(variant, descriptor, database, refs)
        {
            properties.insert(output_name.to_string(), value);
        }
    }

    if is_lua_source_class(class_name) {
        properties.insert(
            "Source".to_string(),
            Value::String("__SOURCE_EXTERNAL__".to_string()),
        );
        source = Some(source.unwrap_or_default());
    }

    (native_properties, properties, attributes, source)
}

pub(crate) fn rbx_properties_to_settings_records<'a>(
    class_name: &str,
    property_entries: impl IntoIterator<Item = (&'a rbx_dom_weak::Ustr, &'a RbxVariant)>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
    options: RbxSettingsConversionOptions<'_>,
) -> (Map<String, Value>, Map<String, Value>, Option<String>) {
    let mut properties = Map::new();
    let mut attributes = Map::new();
    let mut source = None;

    for (property_name, variant) in property_entries {
        let property_name = property_name.as_str();
        if let RbxVariant::Attributes(rbx_attributes) = variant {
            attributes.extend(rbx_attributes_to_settings_map(
                rbx_attributes,
                database,
                refs,
            ));
            continue;
        }
        if model_property_name_is_skipped(property_name) {
            continue;
        }
        if property_name.eq_ignore_ascii_case("Source") && is_lua_source_class(class_name) {
            source = rbx_variant_to_source_string(variant);
            continue;
        }
        if !options.native_properties_pre_filtered
            && options
                .native_filter
                .is_some_and(|filter| !filter.allowed.contains(property_name))
        {
            continue;
        }
        if options.elide_defaults
            && rbx_variant_referent(variant).is_some_and(|referent| {
                imported_instance_index(refs, referent).is_none()
                    && !refs.path_segments_by_ref.contains_key(&referent)
            })
        {
            continue;
        }
        let descriptor = enum_property_descriptor(variant, database, class_name, property_name);
        if options.elide_defaults && !options.defaults_already_elided {
            let default = database
                .classes
                .get(class_name)
                .and_then(|class| database.find_default_property(class, property_name));
            let matches_default = default == Some(variant)
                || default.is_some_and(|default| {
                    let default_descriptor =
                        enum_property_descriptor(default, database, class_name, property_name);
                    match (
                        rbx_variant_to_settings_json(default, default_descriptor, database, refs),
                        rbx_variant_to_settings_json(variant, descriptor, database, refs),
                    ) {
                        (Some(default), Some(value)) => default == value,
                        _ => false,
                    }
                });
            if matches_default {
                continue;
            }
        }
        if let Some(value) = rbx_variant_to_settings_json(variant, descriptor, database, refs) {
            let output_name = options
                .native_filter
                .and_then(|filter| filter.renamed.get(property_name))
                .map_or(property_name, String::as_str);
            properties.insert(output_name.to_string(), value);
        }
    }

    if is_lua_source_class(class_name) {
        properties.insert(
            "Source".to_string(),
            Value::String("__SOURCE_EXTERNAL__".to_string()),
        );
        source = Some(source.unwrap_or_default());
    }

    (properties, attributes, source)
}

pub(crate) fn rbx_variant_to_source_string(value: &RbxVariant) -> Option<String> {
    match value {
        RbxVariant::String(value) => Some(value.clone()),
        RbxVariant::BinaryString(value) => {
            Some(String::from_utf8_lossy(value.as_ref()).to_string())
        }
        _ => None,
    }
}

fn rbx_attributes_to_settings_map(
    attributes: &RbxAttributes,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, value) in attributes {
        let value = match value {
            RbxVariant::BinaryString(value) => Some(Value::String(
                String::from_utf8_lossy(value.as_ref()).into_owned(),
            )),
            RbxVariant::EnumItem(value) => {
                let mut encoded = rbx_enum_to_settings_json(Some(&value.ty), value.value, database);
                if let Some(enum_type) = encoded
                    .as_object_mut()
                    .and_then(|object| object.get_mut("enumType"))
                    .and_then(|value| value.as_str())
                    .map(strip_enum_prefix)
                    .map(ToString::to_string)
                {
                    encoded["enumType"] = Value::String(enum_type);
                }
                Some(encoded)
            }
            _ => rbx_variant_to_settings_json(value, None, database, refs),
        };
        if let Some(value) = value {
            out.insert(name.clone(), value);
        }
    }
    out
}

pub(crate) fn rbx_variant_to_settings_json(
    value: &RbxVariant,
    descriptor: Option<&RbxPropertyDescriptor<'_>>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelImportRefs,
) -> Option<Value> {
    match value {
        RbxVariant::Bool(value) => Some(Value::Bool(*value)),
        RbxVariant::Int32(value) => Some(Value::Number(Number::from(*value))),
        RbxVariant::Int64(value) => Some(Value::Number(Number::from(*value))),
        RbxVariant::Float32(value) => Some(json_number_f64(*value as f64)),
        RbxVariant::Float64(value) => Some(json_number_f64(*value)),
        RbxVariant::String(value) => Some(Value::String(value.clone())),
        RbxVariant::BinaryString(value) => {
            Some(binary_payload_json("BinaryString", value.as_ref()))
        }
        RbxVariant::ContentId(value) => Some(Value::String(value.as_str().to_string())),
        RbxVariant::Content(value) => rbx_content_to_settings_json(value, refs),
        RbxVariant::Tags(value) => Some(Value::Array(
            value.iter().map(|tag| Value::String(tag.to_string())).collect(),
        )),
        RbxVariant::Ref(value) => Some(rbx_ref_to_settings_json(*value, refs)),
        RbxVariant::Vector2(value) => Some(json!({"_type":"Vector2","x":value.x,"y":value.y})),
        RbxVariant::Vector3(value) => Some(json!({"_type":"Vector3","x":value.x,"y":value.y,"z":value.z})),
        RbxVariant::Vector2int16(value) => Some(json!({"_type":"Vector2int16","x":value.x,"y":value.y})),
        RbxVariant::Vector3int16(value) => Some(json!({"_type":"Vector3int16","x":value.x,"y":value.y,"z":value.z})),
        RbxVariant::UDim(value) => Some(json!({"_type":"UDim","scale":value.scale,"offset":value.offset})),
        RbxVariant::UDim2(value) => Some(json!({
            "_type":"UDim2",
            "xScale": value.x.scale,
            "xOffset": value.x.offset,
            "yScale": value.y.scale,
            "yOffset": value.y.offset,
        })),
        RbxVariant::Color3(value) => Some(json!({"_type":"Color3","r":value.r,"g":value.g,"b":value.b})),
        RbxVariant::Color3uint8(value) => {
            let color = RbxColor3::from(*value);
            Some(json!({"_type":"Color3","r":color.r,"g":color.g,"b":color.b}))
        }
        RbxVariant::BrickColor(value) => Some(json!({"_type":"BrickColor","number": *value as u16})),
        RbxVariant::CFrame(value) => Some(rbx_cframe_to_settings_json(*value)),
        RbxVariant::OptionalCFrame(value) => value.map(rbx_cframe_to_settings_json).or(Some(Value::Null)),
        RbxVariant::Rect(value) => Some(json!({
            "_type":"Rect",
            "minX": value.min.x,
            "minY": value.min.y,
            "maxX": value.max.x,
            "maxY": value.max.y,
        })),
        RbxVariant::NumberRange(value) => Some(json!({"_type":"NumberRange","min":value.min,"max":value.max})),
        RbxVariant::NumberSequence(value) => Some(Value::Object(Map::from_iter([
            ("_type".to_string(), Value::String("NumberSequence".to_string())),
            ("keypoints".to_string(), Value::Array(value.keypoints.iter().map(|keypoint| json!({
                "time": keypoint.time,
                "value": keypoint.value,
                "envelope": keypoint.envelope,
            })).collect())),
        ]))),
        RbxVariant::ColorSequence(value) => Some(Value::Object(Map::from_iter([
            ("_type".to_string(), Value::String("ColorSequence".to_string())),
            ("keypoints".to_string(), Value::Array(value.keypoints.iter().map(|keypoint| json!({
                "time": keypoint.time,
                "value": {"r": keypoint.color.r, "g": keypoint.color.g, "b": keypoint.color.b},
            })).collect())),
        ]))),
        RbxVariant::PhysicalProperties(value) => Some(rbx_physical_properties_to_settings_json(*value)),
        RbxVariant::Font(value) => Some(rbx_font_to_settings_json(value)),
        RbxVariant::Enum(value) => {
            let enum_name = descriptor.and_then(|descriptor| match &descriptor.data_type {
                RbxDataType::Enum(enum_name) => Some(*enum_name),
                _ => None,
            });
            Some(rbx_enum_to_settings_json(enum_name, value.to_u32(), database))
        }
        RbxVariant::EnumItem(value) => Some(rbx_enum_to_settings_json(Some(&value.ty), value.value, database)),
        RbxVariant::Attributes(attributes) => Some(Value::Object(rbx_attributes_to_settings_map(attributes, database, refs))),
        RbxVariant::Axes(value) => Some(json!({
            "_type": "Axes",
            "axes": Value::Array(bitmask_names(value.bits(), &AXIS_NAMES)),
        })),
        RbxVariant::Faces(value) => Some(json!({
            "_type": "Faces",
            "faces": Value::Array(bitmask_names(value.bits(), &FACE_NAMES)),
        })),
        RbxVariant::Ray(value) => Some(json!({
            "_type": "Ray",
            "origin": {"x": value.origin.x, "y": value.origin.y, "z": value.origin.z},
            "direction": {"x": value.direction.x, "y": value.direction.y, "z": value.direction.z},
        })),
        RbxVariant::MaterialColors(value) => {
            Some(binary_payload_json("MaterialColors", &value.encode()))
        }
        RbxVariant::SharedString(value) => {
            Some(binary_payload_json("SharedString", value.as_ref()))
        }
        RbxVariant::NetAssetRef(value) => {
            Some(binary_payload_json("NetAssetRef", value.data()))
        }
        RbxVariant::Region3(value) => Some(json!({
            "_type": "Region3",
            "min": {"x": value.min.x, "y": value.min.y, "z": value.min.z},
            "max": {"x": value.max.x, "y": value.max.y, "z": value.max.z},
        })),
        RbxVariant::Region3int16(value) => Some(json!({
            "_type": "Region3int16",
            "min": {"x": value.min.x, "y": value.min.y, "z": value.min.z},
            "max": {"x": value.max.x, "y": value.max.y, "z": value.max.z},
        })),
        RbxVariant::UniqueId(value) => {
            Some(json!({"_type":"UniqueId","value":value.to_string()}))
        }
        RbxVariant::SecurityCapabilities(value) => {
            Some(json!({"_type":"SecurityCapabilities","bits":value.bits().to_string()}))
        }
        _ => None,
    }
}

pub(crate) fn json_number_f64(value: f64) -> Value {
    if let Some(number) = Number::from_f64(value) {
        return Value::Number(number);
    }
    nonfinite_float_json(value)
}

fn nonfinite_float_json(value: f64) -> Value {
    let text = if value.is_nan() {
        "nan"
    } else if value > 0.0 {
        "inf"
    } else {
        "-inf"
    };
    json!({ "_type": "Float", "value": text })
}

pub(crate) fn nonfinite_float_from_json(value: &Value) -> Option<f64> {
    let object = value.as_object()?;
    let encoded = if object.get("_type").and_then(Value::as_str) == Some("Float") {
        object.get("value").and_then(Value::as_str)
    } else if object.get("t").and_then(Value::as_str) == Some("numeric") {
        object.get("v").and_then(Value::as_str)
    } else {
        None
    }?;
    match encoded {
        "nan" => Some(f64::NAN),
        "inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

pub(crate) fn canonicalize_nonfinite_float_json(value: Value) -> Value {
    nonfinite_float_from_json(&value)
        .map(nonfinite_float_json)
        .unwrap_or(value)
}

fn rbx_cframe_to_settings_json(value: RbxCFrame) -> Value {
    json!({
        "_type": "CFrame",
        "components": rbx_cframe_components(value),
    })
}

fn rbx_content_to_settings_json(
    value: &RbxContent,
    refs: &BytecodeModelImportRefs,
) -> Option<Value> {
    match value.value() {
        RbxContentType::None => Some(Value::String(String::new())),
        RbxContentType::Uri(uri) => Some(Value::String(uri.clone())),
        RbxContentType::Object(referent) => Some(rbx_ref_to_settings_json(*referent, refs)),
        _ => None,
    }
}

fn rbx_ref_to_settings_json(referent: RbxRef, refs: &BytecodeModelImportRefs) -> Value {
    let mut out = Map::new();
    out.insert("_type".to_string(), Value::String("Ref".to_string()));
    if let Some(new_index) = imported_instance_index(refs, referent) {
        out.insert(
            "instanceIndex".to_string(),
            Value::Number(Number::from((new_index + 1) as u64)),
        );
        if let Some(settings_id) = refs.settings_id_by_ref.get(&referent) {
            out.insert("settingsId".to_string(), Value::String(settings_id.clone()));
        }
    }
    let path_segments = refs.path_segments_by_ref.get(&referent).or_else(|| {
        imported_instance_index(refs, referent)
            .and_then(|index| refs.path_segments_by_index.get(index))
            .and_then(Option::as_ref)
    });
    if let Some(path_segments) = path_segments {
        out.insert(
            "pathSegments".to_string(),
            Value::Array(path_segments.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(path_ordinals) = refs.path_ordinals_by_ref.get(&referent) {
        out.insert(
            "pathOrdinals".to_string(),
            Value::Array(
                path_ordinals
                    .iter()
                    .map(|ordinal| Value::Number(Number::from(*ordinal as u64)))
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

fn rbx_enum_to_settings_json(
    enum_name: Option<&str>,
    enum_value: u32,
    database: &ReflectionDatabase<'_>,
) -> Value {
    let mut out = Map::new();
    out.insert("_type".to_string(), Value::String("EnumItem".to_string()));
    if let Some(enum_name) = enum_name.map(strip_enum_prefix) {
        out.insert(
            "enumType".to_string(),
            Value::String(format!("Enum.{enum_name}")),
        );
        if let Some(name) = enum_item_name_by_value(database, enum_name, enum_value) {
            out.insert("name".to_string(), Value::String(name));
        }
    }
    out.entry("name".to_string())
        .or_insert_with(|| Value::String(enum_value.to_string()));
    out.insert("value".to_string(), Value::Number(Number::from(enum_value)));
    Value::Object(out)
}

fn rbx_font_to_settings_json(value: &RbxFont) -> Value {
    let mut out = Map::new();
    out.insert("_type".to_string(), Value::String("Font".to_string()));
    out.insert("family".to_string(), Value::String(value.family.clone()));
    out.insert(
        "weight".to_string(),
        Value::String(rbx_font_weight_name(value.weight).to_string()),
    );
    out.insert(
        "style".to_string(),
        Value::String(rbx_font_style_name(value.style).to_string()),
    );
    if let Some(cached_face_id) = value.cached_face_id.as_ref()
        && !cached_face_id.is_empty()
    {
        out.insert(
            "cachedFaceId".to_string(),
            Value::String(cached_face_id.clone()),
        );
    }
    Value::Object(out)
}

fn rbx_physical_properties_to_settings_json(value: RbxPhysicalProperties) -> Value {
    match value {
        RbxPhysicalProperties::Default => json!({
            "_type": "PhysicalProperties",
            "customPhysics": false,
        }),
        RbxPhysicalProperties::Custom(value) => json!({
            "_type": "PhysicalProperties",
            "density": value.density(),
            "friction": value.friction(),
            "elasticity": value.elasticity(),
            "frictionWeight": value.friction_weight(),
            "elasticityWeight": value.elasticity_weight(),
            "acousticAbsorption": value.acoustic_absorption(),
        }),
    }
}

fn rbx_font_weight_name(value: RbxFontWeight) -> &'static str {
    match value {
        RbxFontWeight::Thin => "Thin",
        RbxFontWeight::ExtraLight => "ExtraLight",
        RbxFontWeight::Light => "Light",
        RbxFontWeight::Medium => "Medium",
        RbxFontWeight::SemiBold => "SemiBold",
        RbxFontWeight::Bold => "Bold",
        RbxFontWeight::ExtraBold => "ExtraBold",
        RbxFontWeight::Heavy => "Heavy",
        _ => "Regular",
    }
}

fn rbx_font_style_name(value: RbxFontStyle) -> &'static str {
    match value {
        RbxFontStyle::Italic => "Italic",
        _ => "Normal",
    }
}
