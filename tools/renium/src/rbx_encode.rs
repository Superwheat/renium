use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rbx_dom_weak::types::{
    Attributes as RbxAttributes, Axes as RbxAxes, BinaryString as RbxBinaryString,
    BrickColor as RbxBrickColor, CFrame as RbxCFrame, Color3 as RbxColor3,
    Color3uint8 as RbxColor3uint8, ColorSequence as RbxColorSequence,
    ColorSequenceKeypoint as RbxColorSequenceKeypoint, Content as RbxContent,
    ContentId as RbxContentId, CustomPhysicalProperties as RbxCustomPhysicalProperties,
    Enum as RbxEnum, EnumItem as RbxEnumItem, Faces as RbxFaces, Font as RbxFont,
    FontStyle as RbxFontStyle, FontWeight as RbxFontWeight, MaterialColors as RbxMaterialColors,
    Matrix3 as RbxMatrix3, NetAssetRef as RbxNetAssetRef, NumberRange as RbxNumberRange,
    NumberSequence as RbxNumberSequence, NumberSequenceKeypoint as RbxNumberSequenceKeypoint,
    PhysicalProperties as RbxPhysicalProperties, Ray as RbxRay, Rect as RbxRect, Ref as RbxRef,
    Region3 as RbxRegion3, Region3int16 as RbxRegion3int16,
    SecurityCapabilities as RbxSecurityCapabilities, SharedString as RbxSharedString,
    Tags as RbxTags, UDim as RbxUDim, UDim2 as RbxUDim2, UniqueId as RbxUniqueId,
    Variant as RbxVariant, VariantType as RbxVariantType, Vector2 as RbxVector2,
    Vector2int16 as RbxVector2int16, Vector3 as RbxVector3, Vector3int16 as RbxVector3int16,
};
use rbx_dom_weak::{InstanceBuilder as RbxInstanceBuilder, WeakDom as RbxWeakDom};
use rbx_reflection::{
    ClassDescriptor as RbxClassDescriptor, DataType as RbxDataType,
    PropertyDescriptor as RbxPropertyDescriptor, PropertyKind as RbxPropertyKind,
    PropertySerialization as RbxPropertySerialization, ReflectionDatabase,
};
use serde_json::{Map, Value, json};

use crate::bytecode_edit::{
    instance_path_key, instance_path_parts_key, path_ordinals_from_value, path_segments_from_value,
};
use crate::editor_sync::is_lua_source_class;
use crate::property_schema::{
    AXIS_NAMES, FACE_NAMES, MESH_INITIAL_SIZE_PROPERTY, MESH_SIZE_TRANSPORT_PROPERTY,
};
use crate::rbx_decode::{
    nonfinite_float_from_json, rbx_reflection_class_is_a, rbx_variant_to_settings_json,
};
use crate::rbx_model::{
    BytecodeExportClassMetadata, BytecodeExportMetadata, BytecodeExportPropertyMetadata,
    BytecodeModelExportRefs, BytecodeModelImportRefs,
};
use crate::settings_bytecode::{
    SettingsBytecode, SettingsBytecodeInstance, settings_reference_index, strict_reference_path,
};

#[derive(Default)]
pub(super) struct BytecodeRbxBuildOptions<'a> {
    pub source_path: Option<&'a Path>,
    pub omitted_properties_by_class: Option<&'a mut HashMap<String, HashSet<String>>>,
    pub logical_omitted_properties: Option<&'a mut HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
}

pub(super) struct BytecodeRbxEncoder<'a, 'db> {
    document: &'a SettingsBytecode,
    database: &'db ReflectionDatabase<'db>,
    metadata: &'a mut BytecodeExportMetadata<'db>,
    refs: &'a BytecodeModelExportRefs,
}

impl<'a, 'db> BytecodeRbxEncoder<'a, 'db> {
    pub fn new(
        document: &'a SettingsBytecode,
        database: &'db ReflectionDatabase<'db>,
        metadata: &'a mut BytecodeExportMetadata<'db>,
        refs: &'a BytecodeModelExportRefs,
    ) -> Self {
        Self {
            document,
            database,
            metadata,
            refs,
        }
    }

    pub fn build(
        &mut self,
        index: usize,
        mut options: BytecodeRbxBuildOptions<'_>,
    ) -> Result<RbxInstanceBuilder> {
        let instance = &self.document.instances[index];
        let referent = *self
            .refs
            .by_index
            .get(&index)
            .ok_or_else(|| anyhow::anyhow!("Missing export referent"))?;
        let mut builder = RbxInstanceBuilder::new(instance.class_name.as_str())
            .with_name(instance.name.clone())
            .with_referent(referent);
        let class_metadata = self
            .metadata
            .entry(instance.class_name.clone())
            .or_insert_with(|| BytecodeExportClassMetadata {
                triangle_mesh_part: rbx_reflection_class_is_a(
                    self.database,
                    &instance.class_name,
                    "TriangleMeshPart",
                ),
                model: rbx_reflection_class_is_a(self.database, &instance.class_name, "Model"),
                decal: rbx_reflection_class_is_a(self.database, &instance.class_name, "Decal"),
                properties: HashMap::new(),
            });
        let synthesized_initial_size = synthesized_mesh_initial_size_for_rbx_export_class(
            self.document,
            index,
            class_metadata.triangle_mesh_part,
        );

        for (name, value) in &instance.properties {
            let property_metadata = class_metadata
                .properties
                .entry(name.clone())
                .or_insert_with(|| {
                    let serialized_name =
                        if class_metadata.triangle_mesh_part && name == "FluidFidelity" {
                            Some("FluidFidelityInternal")
                        } else if class_metadata.model && name == "WorldPivot" {
                            Some("WorldPivotData")
                        } else if class_metadata.decal && name == "ColorMapContent" {
                            Some("TextureContent")
                        } else {
                            None
                        };
                    let property =
                        rbx_property_descriptor(self.database, &instance.class_name, name);
                    let descriptor = rbx_model_property_descriptor(
                        self.database,
                        &instance.class_name,
                        serialized_name.unwrap_or(name),
                    );
                    BytecodeExportPropertyMetadata {
                        property,
                        descriptor,
                        serialized_name,
                        native_setter_property: class_metadata.triangle_mesh_part
                            && name == "CollisionFidelity",
                        skipped: model_property_name_is_skipped(name)
                            || name.eq_ignore_ascii_case("Source"),
                    }
                });
            if property_metadata.skipped {
                continue;
            }
            if synthesized_initial_size.is_some()
                && name.eq_ignore_ascii_case(MESH_INITIAL_SIZE_PROPERTY)
            {
                if let Some(logical) = options.logical_omitted_properties.as_deref_mut()
                    && let Some(variant) = json_to_rbx_property_variant(
                        value,
                        rbx_property_descriptor(self.database, &instance.class_name, name),
                        self.database,
                        self.refs,
                    )
                {
                    logical.insert(rbx_dom_weak::Ustr::from(name.as_str()), variant);
                }
                continue;
            }
            validate_model_export_reference_value(value, self.refs).with_context(|| {
                format!(
                    "{} ({}) property {name} contains an invalid reference",
                    instance.name, instance.class_name
                )
            })?;
            let property = property_metadata.property;
            let serialized_name = property_metadata.serialized_name.unwrap_or(name);
            let descriptor = property_metadata.descriptor;
            let unsupported = property.is_some()
                && descriptor.is_none()
                && !property_metadata.native_setter_property;
            let variant = if unsupported {
                None
            } else {
                json_to_rbx_property_variant(
                    value,
                    descriptor.or(property),
                    self.database,
                    self.refs,
                )
            };
            let Some(variant) = variant else {
                if !unsupported && options.omitted_properties_by_class.is_none() {
                    bail!(
                        "{} ({}) property {name} cannot be represented in a Roblox model",
                        instance.name,
                        instance.class_name
                    );
                }
                if let Some(omitted) = options.omitted_properties_by_class.as_deref_mut() {
                    omitted
                        .entry(instance.class_name.clone())
                        .or_default()
                        .insert(name.clone());
                }
                if let Some(logical) = options.logical_omitted_properties.as_deref_mut()
                    && let Some(variant) =
                        json_to_rbx_property_variant(value, property, self.database, self.refs)
                {
                    logical.insert(rbx_dom_weak::Ustr::from(name.as_str()), variant);
                }
                continue;
            };
            builder.add_property(serialized_name, variant);
        }

        if let Some(initial_size) = synthesized_initial_size
            && !builder.has_property(MESH_INITIAL_SIZE_PROPERTY)
        {
            builder.add_property(
                MESH_INITIAL_SIZE_PROPERTY,
                RbxVariant::Vector3(initial_size),
            );
        }

        if is_lua_source_class(&instance.class_name) {
            builder.add_property(
                "Source",
                RbxVariant::String(bytecode_export_script_source(
                    instance,
                    options.source_path,
                )?),
            );
        }

        let attributes = json_attributes_to_rbx(&instance.attributes, self.database, self.refs)
            .with_context(|| {
                format!(
                    "{} ({}) attributes cannot be represented in a Roblox model",
                    instance.name, instance.class_name
                )
            })?;
        if !attributes.is_empty() {
            builder.add_property("Attributes", RbxVariant::Attributes(attributes));
        }

        Ok(builder)
    }
}

pub(super) fn synthesized_mesh_initial_size_for_rbx_export_class(
    document: &SettingsBytecode,
    index: usize,
    triangle_mesh_part: bool,
) -> Option<RbxVector3> {
    let instance = document.instances.get(index)?;
    if !triangle_mesh_part {
        return None;
    }
    if instance
        .properties
        .get(MESH_INITIAL_SIZE_PROPERTY)
        .or_else(|| instance.properties.get("initialSize"))
        .and_then(json_to_rbx_vector3)
        .is_some_and(|value| rbx_vector3_is_non_zero(&value))
    {
        return None;
    }

    if let Some(mesh_size) = instance
        .properties
        .get(MESH_SIZE_TRANSPORT_PROPERTY)
        .or_else(|| instance.properties.get("meshSize"))
        .and_then(json_to_rbx_vector3)
        .filter(rbx_vector3_is_non_zero)
    {
        return Some(mesh_size);
    }

    let size = instance
        .properties
        .get("Size")
        .or_else(|| instance.properties.get("size"))
        .and_then(json_to_rbx_vector3)
        .filter(rbx_vector3_is_non_zero)?;
    let scale = cumulative_ancestor_model_scale(document, index).unwrap_or(1.0);
    if scale.is_finite() && scale.abs() > f32::EPSILON && (scale - 1.0).abs() > 1.0e-6 {
        return Some(RbxVector3::new(
            size.x / scale,
            size.y / scale,
            size.z / scale,
        ));
    }

    Some(size)
}

fn rbx_vector3_is_non_zero(value: &RbxVector3) -> bool {
    value.x.is_finite()
        && value.y.is_finite()
        && value.z.is_finite()
        && (value.x.abs() > f32::EPSILON
            || value.y.abs() > f32::EPSILON
            || value.z.abs() > f32::EPSILON)
}

fn cumulative_ancestor_model_scale(document: &SettingsBytecode, index: usize) -> Option<f32> {
    let mut scale = 1.0_f32;
    let mut parent_index = document.instances.get(index)?.parent_index;
    let mut visited = HashSet::new();

    while let Some(parent) = parent_index {
        if parent >= document.instances.len() || !visited.insert(parent) {
            break;
        }
        let instance = &document.instances[parent];
        if matches!(instance.class_name.as_str(), "Model" | "WorldModel")
            && let Some(value) = instance
                .properties
                .get("Scale")
                .or_else(|| instance.properties.get("ScaleFactor"))
                .and_then(json_f32)
                .filter(|value| value.is_finite())
        {
            scale *= value;
        }
        parent_index = instance.parent_index;
    }

    Some(scale)
}

pub(super) fn bytecode_export_script_source(
    instance: &SettingsBytecodeInstance,
    source_path: Option<&Path>,
) -> Result<String> {
    let stored_source = instance.properties.get("Source").and_then(Value::as_str);
    if stored_source == Some("__SOURCE_EXTERNAL__") {
        let path = source_path.with_context(|| {
            format!(
                "{} ({}) has external source but no source path",
                instance.name, instance.class_name
            )
        })?;
        return fs::read_to_string(path)
            .with_context(|| format!("Failed to read external source {}", path.display()));
    }
    if let Some(path) = source_path
        && path.is_file()
    {
        return fs::read_to_string(path)
            .with_context(|| format!("Failed to read source {}", path.display()));
    }
    Ok(stored_source.unwrap_or("").to_string())
}

pub(super) fn model_property_name_is_skipped(name: &str) -> bool {
    if name.eq_ignore_ascii_case(MESH_SIZE_TRANSPORT_PROPERTY) {
        return true;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "name" | "classname" | "parent" | "attributes" | "attributesserialize" | "clocktime"
    )
}

pub(super) fn rbx_model_property_descriptor<'db>(
    database: &'db ReflectionDatabase<'db>,
    class_name: &str,
    property_name: &str,
) -> Option<&'db RbxPropertyDescriptor<'db>> {
    let (class, property) = rbx_class_property_descriptor(database, class_name, property_name)?;
    rbx_serialized_property_descriptor(class, property)
}

#[cfg(any(windows, target_os = "macos", test))]
pub(super) fn rbx_canonical_property_descriptor_for_serialized_name<'db>(
    database: &'db ReflectionDatabase<'db>,
    class_name: &str,
    serialized_name: &str,
) -> Option<&'db RbxPropertyDescriptor<'db>> {
    let class_descriptor = database.classes.get(class_name)?;
    let mut exact = None;
    for class in database.superclasses_iter(class_descriptor) {
        for property in class.properties.values() {
            let RbxPropertyKind::Canonical { serialization } = &property.kind else {
                continue;
            };
            match serialization {
                RbxPropertySerialization::Serializes if property.name == serialized_name => {
                    exact = Some(property);
                }
                RbxPropertySerialization::SerializesAs(name) if *name == serialized_name => {
                    return Some(property);
                }
                RbxPropertySerialization::Migrate(migration) => {
                    if migration.new_property_names().contains(&serialized_name) {
                        return Some(property);
                    }
                    if property.name == serialized_name {
                        exact = Some(property);
                    }
                }
                _ => {}
            }
        }
    }
    exact
}

pub(super) fn rbx_property_descriptor<'db>(
    database: &'db ReflectionDatabase<'db>,
    class_name: &str,
    property_name: &str,
) -> Option<&'db RbxPropertyDescriptor<'db>> {
    rbx_class_property_descriptor(database, class_name, property_name).map(|(_, property)| property)
}

fn rbx_class_property_descriptor<'db>(
    database: &'db ReflectionDatabase<'db>,
    class_name: &str,
    property_name: &str,
) -> Option<(
    &'db RbxClassDescriptor<'db>,
    &'db RbxPropertyDescriptor<'db>,
)> {
    let class_descriptor = database.classes.get(class_name)?;
    for class in database.superclasses_iter(class_descriptor) {
        if let Some(property) = class.properties.get(property_name) {
            return Some((class, property));
        }
    }
    None
}

fn rbx_serialized_property_descriptor<'db>(
    class: &'db RbxClassDescriptor<'db>,
    property: &'db RbxPropertyDescriptor<'db>,
) -> Option<&'db RbxPropertyDescriptor<'db>> {
    match &property.kind {
        RbxPropertyKind::Canonical { serialization } => match serialization {
            RbxPropertySerialization::Serializes | RbxPropertySerialization::Migrate(_) => {
                Some(property)
            }
            RbxPropertySerialization::SerializesAs(serialized_name) => {
                class.properties.get(*serialized_name)
            }
            _ => None,
        },
        RbxPropertyKind::Alias { alias_for } => class
            .properties
            .get(*alias_for)
            .and_then(|canonical| rbx_serialized_property_descriptor(class, canonical)),
        _ => None,
    }
}

pub(super) fn json_to_rbx_property_variant(
    value: &Value,
    descriptor: Option<&RbxPropertyDescriptor<'_>>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelExportRefs,
) -> Option<RbxVariant> {
    if value.is_null() {
        return None;
    }
    match descriptor.map(|descriptor| &descriptor.data_type) {
        Some(RbxDataType::Enum(enum_name)) => {
            json_to_rbx_enum_variant(value, Some(*enum_name), database)
        }
        Some(RbxDataType::Value(target_type)) => {
            json_to_rbx_variant_for_type(value, *target_type, database, refs)
        }
        Some(_) | None => json_to_rbx_inferred_variant(value, database, refs),
    }
}

fn json_to_rbx_variant_for_type(
    value: &Value,
    target_type: RbxVariantType,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelExportRefs,
) -> Option<RbxVariant> {
    match target_type {
        RbxVariantType::Bool => value.as_bool().map(RbxVariant::Bool),
        RbxVariantType::Int32 => json_i32(value).map(RbxVariant::Int32),
        RbxVariantType::Int64 => json_i64(value).map(RbxVariant::Int64),
        RbxVariantType::Float32 => json_f32(value).map(RbxVariant::Float32),
        RbxVariantType::Float64 => json_f64(value).map(RbxVariant::Float64),
        RbxVariantType::String => {
            json_string_or_wrapped(value, "String").map(|text| RbxVariant::String(text.to_string()))
        }
        RbxVariantType::BinaryString => json_binary_string(value).map(RbxVariant::BinaryString),
        RbxVariantType::ContentId => json_string_or_wrapped(value, "ContentId")
            .map(|text| RbxVariant::ContentId(RbxContentId::from(text))),
        RbxVariantType::Content => json_to_rbx_content(value, refs).map(RbxVariant::Content),
        RbxVariantType::Tags => json_to_rbx_tags(value).map(RbxVariant::Tags),
        RbxVariantType::Ref => Some(RbxVariant::Ref(json_to_rbx_ref(value, refs))),
        RbxVariantType::Vector2 => json_to_rbx_vector2(value).map(RbxVariant::Vector2),
        RbxVariantType::Vector3 => json_to_rbx_vector3(value).map(RbxVariant::Vector3),
        RbxVariantType::Vector2int16 => {
            json_to_rbx_vector2int16(value).map(RbxVariant::Vector2int16)
        }
        RbxVariantType::Vector3int16 => {
            json_to_rbx_vector3int16(value).map(RbxVariant::Vector3int16)
        }
        RbxVariantType::UDim => json_to_rbx_udim(value).map(RbxVariant::UDim),
        RbxVariantType::UDim2 => json_to_rbx_udim2(value).map(RbxVariant::UDim2),
        RbxVariantType::Color3 => json_to_rbx_color3(value).map(RbxVariant::Color3),
        RbxVariantType::Color3uint8 => json_to_rbx_color3(value)
            .map(RbxColor3uint8::from)
            .map(RbxVariant::Color3uint8),
        RbxVariantType::BrickColor => json_to_rbx_brick_color(value).map(RbxVariant::BrickColor),
        RbxVariantType::CFrame => json_to_rbx_cframe(value).map(RbxVariant::CFrame),
        RbxVariantType::OptionalCFrame => {
            if value.is_null() {
                Some(RbxVariant::OptionalCFrame(None))
            } else {
                json_to_rbx_cframe(value).map(|cframe| RbxVariant::OptionalCFrame(Some(cframe)))
            }
        }
        RbxVariantType::Rect => json_to_rbx_rect(value).map(RbxVariant::Rect),
        RbxVariantType::NumberRange => json_to_rbx_number_range(value).map(RbxVariant::NumberRange),
        RbxVariantType::NumberSequence => {
            json_to_rbx_number_sequence(value).map(RbxVariant::NumberSequence)
        }
        RbxVariantType::ColorSequence => {
            json_to_rbx_color_sequence(value).map(RbxVariant::ColorSequence)
        }
        RbxVariantType::Font => json_to_rbx_font(value).map(RbxVariant::Font),
        RbxVariantType::Enum | RbxVariantType::EnumItem => {
            json_to_rbx_enum_variant(value, None, database)
        }
        RbxVariantType::PhysicalProperties => {
            json_to_rbx_physical_properties(value).map(RbxVariant::PhysicalProperties)
        }
        RbxVariantType::Axes => json_to_rbx_axes(value).map(RbxVariant::Axes),
        RbxVariantType::Faces => json_to_rbx_faces(value).map(RbxVariant::Faces),
        RbxVariantType::Ray => json_to_rbx_ray(value).map(RbxVariant::Ray),
        RbxVariantType::MaterialColors => json_binary_payload(value, "MaterialColors")
            .and_then(|bytes| RbxMaterialColors::decode(&bytes).ok())
            .map(RbxVariant::MaterialColors),
        RbxVariantType::SharedString => json_binary_payload(value, "SharedString")
            .map(RbxSharedString::new)
            .map(RbxVariant::SharedString),
        RbxVariantType::NetAssetRef => json_binary_payload(value, "NetAssetRef")
            .map(RbxNetAssetRef::new)
            .map(RbxVariant::NetAssetRef),
        RbxVariantType::Region3 => json_to_rbx_region3(value).map(RbxVariant::Region3),
        RbxVariantType::Region3int16 => {
            json_to_rbx_region3int16(value).map(RbxVariant::Region3int16)
        }
        RbxVariantType::UniqueId => json_to_rbx_unique_id(value).map(RbxVariant::UniqueId),
        RbxVariantType::SecurityCapabilities => {
            json_to_rbx_security_capabilities(value).map(RbxVariant::SecurityCapabilities)
        }
        _ => None,
    }
}

fn json_name_list_bits(value: &Value, key: &str, names: &[(u8, &str)]) -> Option<u8> {
    let items = value.as_object()?.get(key)?.as_array()?;
    let mut bits = 0u8;
    for item in items {
        let name = item.as_str()?;
        let (bit, _) = names.iter().find(|(_, candidate)| *candidate == name)?;
        bits |= bit;
    }
    Some(bits)
}

pub(super) fn json_to_rbx_axes(value: &Value) -> Option<RbxAxes> {
    RbxAxes::from_bits(json_name_list_bits(value, "axes", &AXIS_NAMES)?)
}

pub(super) fn json_to_rbx_faces(value: &Value) -> Option<RbxFaces> {
    RbxFaces::from_bits(json_name_list_bits(value, "faces", &FACE_NAMES)?)
}

pub(super) fn json_to_rbx_ray(value: &Value) -> Option<RbxRay> {
    let object = value.as_object()?;
    Some(RbxRay::new(
        json_to_rbx_vector3(object.get("origin")?)?,
        json_to_rbx_vector3(object.get("direction")?)?,
    ))
}

pub(super) fn binary_payload_json(kind: &str, bytes: &[u8]) -> Value {
    json!({ "_type": kind, "base64": base64::encode(bytes) })
}

fn json_binary_payload(value: &Value, kind: &str) -> Option<Vec<u8>> {
    let object = value.as_object()?;
    if object.get("_type").and_then(Value::as_str) != Some(kind) {
        return None;
    }
    base64::decode(object.get("base64")?.as_str()?).ok()
}

fn json_to_rbx_unique_id(value: &Value) -> Option<RbxUniqueId> {
    let object = value.as_object()?;
    if object.get("_type").and_then(Value::as_str) != Some("UniqueId") {
        return None;
    }
    object.get("value")?.as_str()?.parse().ok()
}

fn json_to_rbx_security_capabilities(value: &Value) -> Option<RbxSecurityCapabilities> {
    let object = value.as_object()?;
    if object.get("_type").and_then(Value::as_str) != Some("SecurityCapabilities") {
        return None;
    }
    let bits = object
        .get("bits")?
        .as_str()
        .and_then(|bits| bits.parse::<u64>().ok())
        .or_else(|| object.get("bits")?.as_u64())?;
    Some(RbxSecurityCapabilities::from_bits(bits))
}

fn json_to_rbx_region3(value: &Value) -> Option<RbxRegion3> {
    let object = value.as_object()?;
    Some(RbxRegion3::new(
        json_to_rbx_vector3(object.get("min")?)?,
        json_to_rbx_vector3(object.get("max")?)?,
    ))
}

fn json_to_rbx_region3int16(value: &Value) -> Option<RbxRegion3int16> {
    let object = value.as_object()?;
    Some(RbxRegion3int16::new(
        json_to_rbx_vector3int16(object.get("min")?)?,
        json_to_rbx_vector3int16(object.get("max")?)?,
    ))
}

fn json_to_rbx_inferred_variant(
    value: &Value,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelExportRefs,
) -> Option<RbxVariant> {
    match value {
        Value::Bool(value) => Some(RbxVariant::Bool(*value)),
        Value::Number(_) => json_f64(value).map(RbxVariant::Float64),
        Value::String(value) => Some(RbxVariant::String(value.clone())),
        Value::Object(object) => match object.get("_type").and_then(Value::as_str) {
            Some("Float") => json_f64(value).map(RbxVariant::Float64),
            Some("Vector2") => json_to_rbx_vector2(value).map(RbxVariant::Vector2),
            Some("Vector3") => json_to_rbx_vector3(value).map(RbxVariant::Vector3),
            Some("Vector2int16") => json_to_rbx_vector2int16(value).map(RbxVariant::Vector2int16),
            Some("Vector3int16") => json_to_rbx_vector3int16(value).map(RbxVariant::Vector3int16),
            Some("BinaryString") => json_binary_string(value).map(RbxVariant::BinaryString),
            Some("UDim") => json_to_rbx_udim(value).map(RbxVariant::UDim),
            Some("UDim2") => json_to_rbx_udim2(value).map(RbxVariant::UDim2),
            Some("Color3") => json_to_rbx_color3(value).map(RbxVariant::Color3),
            Some("Color3uint8") => json_to_rbx_color3(value)
                .map(RbxColor3uint8::from)
                .map(RbxVariant::Color3uint8),
            Some("BrickColor") => json_to_rbx_brick_color(value).map(RbxVariant::BrickColor),
            Some("CFrame") => json_to_rbx_cframe(value).map(RbxVariant::CFrame),
            Some("Rect") => json_to_rbx_rect(value).map(RbxVariant::Rect),
            Some("NumberRange") => json_to_rbx_number_range(value).map(RbxVariant::NumberRange),
            Some("NumberSequence") => {
                json_to_rbx_number_sequence(value).map(RbxVariant::NumberSequence)
            }
            Some("ColorSequence") => {
                json_to_rbx_color_sequence(value).map(RbxVariant::ColorSequence)
            }
            Some("PhysicalProperties") => {
                json_to_rbx_physical_properties(value).map(RbxVariant::PhysicalProperties)
            }
            Some("Font") => json_to_rbx_font(value).map(RbxVariant::Font),
            Some("Axes") => json_to_rbx_axes(value).map(RbxVariant::Axes),
            Some("Faces") => json_to_rbx_faces(value).map(RbxVariant::Faces),
            Some("Ray") => json_to_rbx_ray(value).map(RbxVariant::Ray),
            Some("MaterialColors") => json_binary_payload(value, "MaterialColors")
                .and_then(|bytes| RbxMaterialColors::decode(&bytes).ok())
                .map(RbxVariant::MaterialColors),
            Some("SharedString") => json_binary_payload(value, "SharedString")
                .map(RbxSharedString::new)
                .map(RbxVariant::SharedString),
            Some("NetAssetRef") => json_binary_payload(value, "NetAssetRef")
                .map(RbxNetAssetRef::new)
                .map(RbxVariant::NetAssetRef),
            Some("Region3") => json_to_rbx_region3(value).map(RbxVariant::Region3),
            Some("Region3int16") => json_to_rbx_region3int16(value).map(RbxVariant::Region3int16),
            Some("UniqueId") => json_to_rbx_unique_id(value).map(RbxVariant::UniqueId),
            Some("SecurityCapabilities") => {
                json_to_rbx_security_capabilities(value).map(RbxVariant::SecurityCapabilities)
            }
            Some("Enum" | "EnumItem") => json_to_rbx_enum_variant(value, None, database),
            Some("Ref") => Some(RbxVariant::Ref(json_to_rbx_ref(value, refs))),
            _ => object
                .get("NumberRange")
                .and_then(|_| json_to_rbx_number_range(value).map(RbxVariant::NumberRange))
                .or_else(|| {
                    object.get("NumberSequence").and_then(|_| {
                        json_to_rbx_number_sequence(value).map(RbxVariant::NumberSequence)
                    })
                })
                .or_else(|| {
                    object.get("ColorSequence").and_then(|_| {
                        json_to_rbx_color_sequence(value).map(RbxVariant::ColorSequence)
                    })
                })
                .or_else(|| {
                    object.get("PhysicalProperties").and_then(|_| {
                        json_to_rbx_physical_properties(value).map(RbxVariant::PhysicalProperties)
                    })
                })
                .or_else(|| {
                    object
                        .get("ContentId")
                        .and_then(|_| json_string_or_wrapped(value, "ContentId"))
                        .map(|text| RbxVariant::ContentId(RbxContentId::from(text)))
                }),
        },
        _ => None,
    }
}

pub(crate) fn normalize_project_typed_value(
    class_name: Option<&str>,
    property_name: Option<&str>,
    value: &Value,
) -> Result<Value> {
    let database =
        rbx_reflection_database::get().context("Roblox reflection database is unavailable")?;
    let export_refs = BytecodeModelExportRefs::default();
    let descriptor = class_name
        .zip(property_name)
        .and_then(|(class_name, property_name)| {
            rbx_property_descriptor(database, class_name, property_name)
        });
    let variant = json_to_rbx_property_variant(value, descriptor, database, &export_refs)
        .or_else(|| serde_json::from_value::<RbxVariant>(value.clone()).ok());
    let Some(variant) = variant else {
        if value.is_null() || value.is_boolean() || value.is_number() || value.is_string() {
            return Ok(value.clone());
        }
        bail!("Value cannot be represented as a Roblox property literal");
    };
    let import_refs = BytecodeModelImportRefs::default();
    rbx_variant_to_settings_json(&variant, descriptor, database, &import_refs)
        .context("Roblox property literal isn't supported by Renium")
}

fn json_attributes_to_rbx(
    attributes: &Map<String, Value>,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelExportRefs,
) -> Result<RbxAttributes> {
    let mut out = RbxAttributes::new();
    for (name, value) in attributes {
        validate_model_export_reference_value(value, refs)
            .with_context(|| format!("Attribute {name} contains an invalid reference"))?;
        let variant = json_to_rbx_attribute_variant(value, database, refs)
            .with_context(|| format!("Attribute {name} has an unsupported value"))?;
        out.insert(name.clone(), variant);
    }
    Ok(out)
}

pub(super) fn json_to_rbx_attribute_variant(
    value: &Value,
    database: &ReflectionDatabase<'_>,
    refs: &BytecodeModelExportRefs,
) -> Option<RbxVariant> {
    let variant = json_to_rbx_inferred_variant(value, database, refs)?;
    if matches!(
        variant,
        RbxVariant::Ref(_)
            | RbxVariant::Content(_)
            | RbxVariant::ContentId(_)
            | RbxVariant::Tags(_)
            | RbxVariant::Attributes(_)
    ) {
        None
    } else {
        Some(variant)
    }
}

fn json_object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn json_wrapped_value<'a>(value: &'a Value, wrapper_name: &str) -> &'a Value {
    value
        .as_object()
        .and_then(|object| object.get(wrapper_name))
        .unwrap_or(value)
}

fn json_string_or_wrapped<'a>(value: &'a Value, wrapper_name: &str) -> Option<&'a str> {
    value.as_str().or_else(|| {
        let object = value.as_object()?;
        object
            .get(wrapper_name)
            .and_then(Value::as_str)
            .or_else(|| object.get("value").and_then(Value::as_str))
    })
}

pub(super) fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .or_else(|| nonfinite_float_from_json(value))
}

fn json_f32(value: &Value) -> Option<f32> {
    json_f64(value).map(|value| value as f32)
}

pub(super) fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| {
            json_f64(value).and_then(|value| {
                let truncated = value.trunc();
                ((truncated - value).abs() < f64::EPSILON).then_some(truncated as i64)
            })
        })
}

fn json_i32(value: &Value) -> Option<i32> {
    json_i64(value).and_then(|value| i32::try_from(value).ok())
}

fn json_i16(value: &Value) -> Option<i16> {
    json_i64(value).and_then(|value| i16::try_from(value).ok())
}

fn json_u8(value: &Value) -> Option<u8> {
    json_i64(value).and_then(|value| u8::try_from(value).ok())
}

fn json_binary_string(value: &Value) -> Option<RbxBinaryString> {
    if let Some(bytes) = json_binary_payload(value, "BinaryString") {
        return Some(RbxBinaryString::from(bytes));
    }
    if let Some(text) = value.as_str() {
        return Some(RbxBinaryString::from(text.as_bytes().to_vec()));
    }
    let bytes = value
        .as_array()?
        .iter()
        .map(json_u8)
        .collect::<Option<Vec<_>>>()?;
    Some(RbxBinaryString::from(bytes))
}

fn json_to_rbx_vector2(value: &Value) -> Option<RbxVector2> {
    let value = json_wrapped_value(value, "Vector2");
    if let Some(items) = value.as_array()
        && items.len() == 2
    {
        return Some(RbxVector2::new(json_f32(&items[0])?, json_f32(&items[1])?));
    }
    let obj = json_object(value)?;
    Some(RbxVector2::new(
        json_f32(obj.get("x")?)?,
        json_f32(obj.get("y")?)?,
    ))
}

fn json_to_rbx_vector3(value: &Value) -> Option<RbxVector3> {
    let value = json_wrapped_value(value, "Vector3");
    if let Some(items) = value.as_array()
        && items.len() == 3
    {
        return Some(RbxVector3::new(
            json_f32(&items[0])?,
            json_f32(&items[1])?,
            json_f32(&items[2])?,
        ));
    }
    let obj = json_object(value)?;
    Some(RbxVector3::new(
        json_f32(obj.get("x")?)?,
        json_f32(obj.get("y")?)?,
        json_f32(obj.get("z")?)?,
    ))
}

fn json_to_rbx_vector2int16(value: &Value) -> Option<RbxVector2int16> {
    let obj = json_object(value)?;
    Some(RbxVector2int16::new(
        json_i16(obj.get("x")?)?,
        json_i16(obj.get("y")?)?,
    ))
}

fn json_to_rbx_vector3int16(value: &Value) -> Option<RbxVector3int16> {
    let obj = json_object(value)?;
    Some(RbxVector3int16::new(
        json_i16(obj.get("x")?)?,
        json_i16(obj.get("y")?)?,
        json_i16(obj.get("z")?)?,
    ))
}

fn json_to_rbx_udim(value: &Value) -> Option<RbxUDim> {
    let obj = json_object(value)?;
    Some(RbxUDim::new(
        json_f32(obj.get("scale")?)?,
        json_i32(obj.get("offset")?)?,
    ))
}

fn json_to_rbx_udim2(value: &Value) -> Option<RbxUDim2> {
    let obj = json_object(value)?;
    Some(RbxUDim2::new(
        RbxUDim::new(
            json_f32(obj.get("xScale")?)?,
            json_i32(obj.get("xOffset")?)?,
        ),
        RbxUDim::new(
            json_f32(obj.get("yScale")?)?,
            json_i32(obj.get("yOffset")?)?,
        ),
    ))
}

fn json_to_rbx_color3(value: &Value) -> Option<RbxColor3> {
    let value = json_wrapped_value(value, "Color3");
    if let Some(items) = value.as_array() {
        if items.len() < 3 {
            return None;
        }
        return Some(RbxColor3::new(
            json_f32(&items[0])?,
            json_f32(&items[1])?,
            json_f32(&items[2])?,
        ));
    }
    let obj = json_object(value)?;
    if obj.get("r").is_none()
        && obj.get("g").is_none()
        && obj.get("b").is_none()
        && let Some(nested) = obj.get("value").or_else(|| obj.get("color"))
    {
        return json_to_rbx_color3(nested);
    }
    if obj.get("_type").and_then(Value::as_str) == Some("Color3uint8") {
        return Some(RbxColor3::from(RbxColor3uint8::new(
            json_u8(obj.get("r")?)?,
            json_u8(obj.get("g")?)?,
            json_u8(obj.get("b")?)?,
        )));
    }
    Some(RbxColor3::new(
        json_f32(obj.get("r")?)?,
        json_f32(obj.get("g")?)?,
        json_f32(obj.get("b")?)?,
    ))
}

fn json_to_rbx_brick_color(value: &Value) -> Option<RbxBrickColor> {
    let value = json_wrapped_value(value, "BrickColor");
    let number = json_object(value)
        .and_then(|obj| obj.get("number"))
        .and_then(json_i64)
        .or_else(|| json_i64(value))?;
    let number = u16::try_from(number).ok()?;
    RbxBrickColor::from_number(number)
}

fn json_to_rbx_cframe(value: &Value) -> Option<RbxCFrame> {
    let value = json_wrapped_value(value, "CFrame");
    let components = value
        .as_array()
        .or_else(|| json_object(value)?.get("components")?.as_array())?;
    if components.len() != 12 {
        return None;
    }
    let numbers = components
        .iter()
        .map(json_f32)
        .collect::<Option<Vec<_>>>()?;
    Some(RbxCFrame::new(
        RbxVector3::new(numbers[0], numbers[1], numbers[2]),
        RbxMatrix3::new(
            RbxVector3::new(numbers[3], numbers[4], numbers[5]),
            RbxVector3::new(numbers[6], numbers[7], numbers[8]),
            RbxVector3::new(numbers[9], numbers[10], numbers[11]),
        ),
    ))
}

fn json_to_rbx_rect(value: &Value) -> Option<RbxRect> {
    let obj = json_object(value)?;
    Some(RbxRect::new(
        RbxVector2::new(json_f32(obj.get("minX")?)?, json_f32(obj.get("minY")?)?),
        RbxVector2::new(json_f32(obj.get("maxX")?)?, json_f32(obj.get("maxY")?)?),
    ))
}

fn json_to_rbx_number_range(value: &Value) -> Option<RbxNumberRange> {
    let obj = json_object(json_wrapped_value(value, "NumberRange"))?;
    Some(RbxNumberRange::new(
        json_f32(obj.get("min")?)?,
        json_f32(obj.get("max")?)?,
    ))
}

fn json_to_rbx_number_sequence(value: &Value) -> Option<RbxNumberSequence> {
    let keypoints = json_object(json_wrapped_value(value, "NumberSequence"))?
        .get("keypoints")?
        .as_array()?;
    let keypoints = keypoints
        .iter()
        .filter_map(|keypoint| {
            let obj = keypoint.as_object()?;
            Some(RbxNumberSequenceKeypoint::new(
                json_f32(obj.get("time")?)?,
                json_f32(obj.get("value")?)?,
                obj.get("envelope").and_then(json_f32).unwrap_or(0.0),
            ))
        })
        .collect::<Vec<_>>();
    (!keypoints.is_empty()).then_some(RbxNumberSequence { keypoints })
}

pub(super) fn json_to_rbx_color_sequence(value: &Value) -> Option<RbxColorSequence> {
    let keypoints = json_object(json_wrapped_value(value, "ColorSequence"))?
        .get("keypoints")?
        .as_array()?;
    let keypoints = keypoints
        .iter()
        .filter_map(|keypoint| {
            let obj = keypoint.as_object()?;
            let color = obj.get("value").or_else(|| obj.get("color"))?;
            Some(RbxColorSequenceKeypoint::new(
                json_f32(obj.get("time")?)?,
                json_to_rbx_color3(color)?,
            ))
        })
        .collect::<Vec<_>>();
    (!keypoints.is_empty()).then_some(RbxColorSequence { keypoints })
}

fn json_to_rbx_physical_properties(value: &Value) -> Option<RbxPhysicalProperties> {
    let value = json_wrapped_value(value, "PhysicalProperties");
    if value.as_bool() == Some(false) {
        return Some(RbxPhysicalProperties::Default);
    }
    if let Some(text) = value.as_str() {
        return text
            .eq_ignore_ascii_case("Default")
            .then_some(RbxPhysicalProperties::Default);
    }

    let obj = json_object(value)?;
    if obj.get("customPhysics").and_then(Value::as_bool) == Some(false) {
        return Some(RbxPhysicalProperties::Default);
    }

    Some(RbxPhysicalProperties::Custom(
        RbxCustomPhysicalProperties::new(
            json_f32(obj.get("density")?)?,
            json_f32(obj.get("friction")?)?,
            json_f32(obj.get("elasticity")?)?,
            json_f32(obj.get("frictionWeight")?)?,
            json_f32(obj.get("elasticityWeight")?)?,
            obj.get("acousticAbsorption")
                .and_then(json_f32)
                .unwrap_or(1.0),
        ),
    ))
}

fn json_to_rbx_font(value: &Value) -> Option<RbxFont> {
    let obj = json_object(value)?;
    let family = obj
        .get("family")
        .and_then(Value::as_str)
        .unwrap_or("rbxasset://fonts/families/SourceSansPro.json");
    let weight = obj
        .get("weight")
        .and_then(font_weight_from_json)
        .unwrap_or_default();
    let style = obj
        .get("style")
        .and_then(font_style_from_json)
        .unwrap_or_default();
    let mut font = RbxFont::new(family, weight, style);
    font.cached_face_id = obj
        .get("cachedFaceId")
        .or_else(|| obj.get("cached_face_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    Some(font)
}

fn font_weight_from_json(value: &Value) -> Option<RbxFontWeight> {
    if let Some(weight) = json_i64(value).and_then(|value| u16::try_from(value).ok()) {
        return RbxFontWeight::from_u16(weight);
    }
    match enum_tail(value.as_str()?).to_ascii_lowercase().as_str() {
        "thin" => RbxFontWeight::from_u16(100),
        "extralight" => RbxFontWeight::from_u16(200),
        "light" => RbxFontWeight::from_u16(300),
        "regular" => RbxFontWeight::from_u16(400),
        "medium" => RbxFontWeight::from_u16(500),
        "semibold" => RbxFontWeight::from_u16(600),
        "bold" => RbxFontWeight::from_u16(700),
        "extrabold" => RbxFontWeight::from_u16(800),
        "heavy" => RbxFontWeight::from_u16(900),
        other => other.parse::<u16>().ok().and_then(RbxFontWeight::from_u16),
    }
}

fn font_style_from_json(value: &Value) -> Option<RbxFontStyle> {
    if let Some(style) = json_i64(value).and_then(|value| u8::try_from(value).ok()) {
        return RbxFontStyle::from_u8(style);
    }
    match enum_tail(value.as_str()?).to_ascii_lowercase().as_str() {
        "normal" => RbxFontStyle::from_u8(0),
        "italic" => RbxFontStyle::from_u8(1),
        other => other.parse::<u8>().ok().and_then(RbxFontStyle::from_u8),
    }
}

fn json_to_rbx_tags(value: &Value) -> Option<RbxTags> {
    if let Some(items) = value.as_array() {
        return Some(RbxTags::from(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        ));
    }
    value.as_str().map(|text| {
        RbxTags::from(
            text.split('\0')
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )
    })
}

fn json_to_rbx_content(value: &Value, refs: &BytecodeModelExportRefs) -> Option<RbxContent> {
    if value.is_null() {
        return Some(RbxContent::none());
    }
    if let Some(text) = value.as_str() {
        return if text.is_empty() || text.eq_ignore_ascii_case("None") {
            Some(RbxContent::none())
        } else {
            Some(RbxContent::from_uri(text))
        };
    }
    if let Some(object) = value.as_object() {
        if let Some(wrapped) = object.get("Content") {
            return json_to_rbx_content(wrapped, refs);
        }
        if let Some(text) = object
            .get("Uri")
            .or_else(|| object.get("uri"))
            .and_then(Value::as_str)
        {
            return Some(RbxContent::from_uri(text));
        }
        if object.get("None").and_then(Value::as_bool) == Some(true) {
            return Some(RbxContent::none());
        }
        if let Some(object_ref) = object.get("Object").or_else(|| object.get("object")) {
            return Some(RbxContent::from_referent(json_to_rbx_ref(object_ref, refs)));
        }
        if object.get("_type").and_then(Value::as_str) == Some("Ref") || object.get("Ref").is_some()
        {
            return Some(RbxContent::from_referent(json_to_rbx_ref(value, refs)));
        }
    }
    Some(RbxContent::from_referent(json_to_rbx_ref(value, refs)))
}

fn accept_model_export_ref(
    resolved: &mut Option<RbxRef>,
    selector: &str,
    candidate: Option<RbxRef>,
) -> Result<()> {
    let candidate =
        candidate.with_context(|| format!("Ref {selector} does not resolve inside the model"))?;
    if resolved.is_some_and(|resolved| resolved != candidate) {
        bail!("Ref selectors identify different instances");
    }
    *resolved = Some(candidate);
    Ok(())
}

fn model_export_ref_by_settings_id(
    refs: &BytecodeModelExportRefs,
    settings_id: &str,
) -> Option<RbxRef> {
    refs.by_settings_id.get(settings_id).copied().or_else(|| {
        refs.global_by_settings_id
            .as_ref()
            .and_then(|global| global.get(settings_id).copied())
    })
}

fn model_export_ref_by_path_key(refs: &BytecodeModelExportRefs, path_key: &str) -> Option<RbxRef> {
    refs.by_path_key.get(path_key).copied().or_else(|| {
        refs.global_by_path_key
            .as_ref()
            .and_then(|global| global.get(path_key).copied())
    })
}

fn model_export_ref_by_path_segments_key(
    refs: &BytecodeModelExportRefs,
    path_key: &str,
) -> Option<RbxRef> {
    refs.by_path_segments_key
        .get(path_key)
        .copied()
        .flatten()
        .or_else(|| {
            refs.global_by_path_segments_key
                .as_ref()
                .and_then(|global| global.get(path_key).copied().flatten())
        })
}

fn strict_model_export_ref(
    object: &Map<String, Value>,
    refs: &BytecodeModelExportRefs,
) -> Result<RbxRef> {
    let mut resolved = None;
    for selector in ["settingsId", "instanceId"] {
        if let Some(value) = object.get(selector) {
            let id = value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| format!("Ref {selector} must be a non-empty string"))?;
            accept_model_export_ref(
                &mut resolved,
                selector,
                model_export_ref_by_settings_id(refs, id),
            )?;
        }
    }
    if let Some((segments, ordinals)) = strict_reference_path(object)? {
        let candidate = if let Some(ordinals) = ordinals {
            model_export_ref_by_path_key(refs, &instance_path_parts_key(&segments, &ordinals))
        } else {
            model_export_ref_by_path_segments_key(refs, &instance_path_key(&segments))
        };
        accept_model_export_ref(&mut resolved, "pathSegments", candidate)?;
    }
    if let Some(value) = object.get("instanceIndex") {
        let index = settings_reference_index(value).context("Ref instanceIndex must be 1-based")?;
        accept_model_export_ref(
            &mut resolved,
            "instanceIndex",
            refs.by_index.get(&index).copied(),
        )?;
    }
    for selector in ["referent", "ref"] {
        if let Some(value) = object.get(selector) {
            let referent = value
                .as_str()
                .and_then(|value| value.parse::<RbxRef>().ok())
                .with_context(|| format!("Ref {selector} must be a valid referent"))?;
            accept_model_export_ref(&mut resolved, selector, Some(referent))?;
        }
    }
    if object.contains_key("debugId") && resolved.is_none() {
        bail!("Ref debugId cannot resolve without a stable model selector");
    }
    Ok(resolved.unwrap_or_else(RbxRef::none))
}

fn validate_model_export_reference_value(
    value: &Value,
    refs: &BytecodeModelExportRefs,
) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                validate_model_export_reference_value(value, refs)?;
            }
        }
        Value::Object(object) => {
            if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                strict_model_export_ref(object, refs)?;
                return Ok(());
            }
            if let Some(reference) = object.get("Ref").and_then(Value::as_object) {
                strict_model_export_ref(reference, refs)?;
            }
            for (name, value) in object {
                if name != "Ref" {
                    validate_model_export_reference_value(value, refs)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_to_rbx_ref(value: &Value, refs: &BytecodeModelExportRefs) -> RbxRef {
    let Some(object) = value.as_object().and_then(|object| {
        if object.get("_type").and_then(Value::as_str) == Some("Ref") {
            Some(object)
        } else {
            object.get("Ref").and_then(Value::as_object)
        }
    }) else {
        return value
            .as_str()
            .and_then(|raw| raw.parse::<RbxRef>().ok())
            .unwrap_or_else(RbxRef::none);
    };

    object
        .get("settingsId")
        .or_else(|| object.get("instanceId"))
        .and_then(Value::as_str)
        .and_then(|settings_id| model_export_ref_by_settings_id(refs, settings_id))
        .or_else(|| {
            let segments = object
                .get("pathSegments")
                .and_then(path_segments_from_value)?;
            if let Some(ordinals) = object
                .get("pathOrdinals")
                .and_then(path_ordinals_from_value)
            {
                return (segments.len() == ordinals.len())
                    .then(|| instance_path_parts_key(&segments, &ordinals))
                    .and_then(|key| model_export_ref_by_path_key(refs, &key));
            }
            model_export_ref_by_path_segments_key(refs, &instance_path_key(&segments))
        })
        .or_else(|| {
            object
                .get("instanceIndex")
                .and_then(settings_reference_index)
                .and_then(|index| refs.by_index.get(&index).copied())
        })
        .or_else(|| {
            object
                .get("referent")
                .or_else(|| object.get("ref"))
                .and_then(Value::as_str)
                .and_then(|raw| raw.parse::<RbxRef>().ok())
        })
        .unwrap_or_else(RbxRef::none)
}

fn json_to_rbx_enum_variant(
    value: &Value,
    enum_name: Option<&str>,
    database: &ReflectionDatabase<'_>,
) -> Option<RbxVariant> {
    let object = value.as_object();
    let enum_name = object
        .and_then(|object| object.get("enumType").and_then(Value::as_str))
        .or(enum_name)
        .map(strip_enum_prefix);
    let item_name = object
        .and_then(|object| object.get("name").and_then(Value::as_str))
        .or_else(|| value.as_str())
        .map(enum_tail);
    let numeric = object
        .and_then(|object| object.get("value").or_else(|| object.get("number")))
        .and_then(json_i64)
        .or_else(|| json_i64(value))
        .or_else(|| item_name.and_then(|name| name.parse::<i64>().ok()))
        .and_then(|value| u32::try_from(value).ok());
    let value = numeric.or_else(|| {
        let enum_name = enum_name?;
        let item_name = item_name?;
        enum_item_value_by_name(database, enum_name, item_name)
    })?;
    if let Some(enum_name) = enum_name {
        Some(RbxVariant::EnumItem(RbxEnumItem {
            ty: enum_name.to_string(),
            value,
        }))
    } else {
        Some(RbxVariant::Enum(RbxEnum::from_u32(value)))
    }
}

pub(super) fn strip_enum_prefix(value: &str) -> &str {
    value.strip_prefix("Enum.").unwrap_or(value)
}

fn enum_tail(value: &str) -> &str {
    value.split('.').next_back().unwrap_or(value)
}

fn enum_item_value_by_name(
    database: &ReflectionDatabase<'_>,
    enum_name: &str,
    item_name: &str,
) -> Option<u32> {
    let descriptor = database.enums.get(strip_enum_prefix(enum_name))?;
    descriptor
        .items
        .get(item_name)
        .copied()
        .or_else(|| descriptor.items.get(enum_tail(item_name)).copied())
}

pub(super) fn enum_item_name_by_value(
    database: &ReflectionDatabase<'_>,
    enum_name: &str,
    enum_value: u32,
) -> Option<String> {
    database
        .enums
        .get(strip_enum_prefix(enum_name))?
        .items
        .iter()
        .find_map(|(name, value)| (*value == enum_value).then(|| name.to_string()))
}

pub(super) fn rbx_model_top_level_refs(dom: &RbxWeakDom) -> Vec<RbxRef> {
    if dom.root().class.as_str() == "DataModel" {
        dom.root().children().to_vec()
    } else {
        vec![dom.root_ref()]
    }
}

pub(super) fn collect_rbx_subtree_preorder(
    dom: &RbxWeakDom,
    referent: RbxRef,
    out: &mut Vec<RbxRef>,
) {
    out.push(referent);
    if let Some(instance) = dom.get_by_ref(referent) {
        for child in instance.children() {
            collect_rbx_subtree_preorder(dom, *child, out);
        }
    }
}

pub(super) fn settings_root_indices(document: &SettingsBytecode) -> Vec<usize> {
    document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect()
}
