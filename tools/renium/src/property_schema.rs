use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::bridge_server::BridgeServer;
use super::file_io::{read_json_file, write_json_file};

pub(super) const MATERIAL_SERVICE_CLASS: &str = "MaterialService";
pub(super) const USE_2022_MATERIALS_PROPERTY: &str = "Use2022Materials";
pub(super) const MESH_INITIAL_SIZE_PROPERTY: &str = "InitialSize";
pub(super) const MESH_SIZE_TRANSPORT_PROPERTY: &str = "MeshSize";
pub(super) const TRIANGLE_MESH_PART_CLASS: &str = "TriangleMeshPart";
const PROPERTY_SCHEMA_CACHE_VERSION: u32 = 7;

pub(super) type PropertySchemaMap = HashMap<String, Vec<PropertySchemaEntry>>;
pub(super) type EnumValueNameMap = HashMap<String, HashMap<i64, String>>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PropertySchemaEntry {
    pub(super) name: String,
    pub(super) type_id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) enum_type: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PropertySchemaCacheFile {
    version: u32,
    source_len: u64,
    source_mtime_ms: u64,
    classes: PropertySchemaMap,
}

pub(super) const TYPE_ID_BOOL: u8 = 1;
pub(super) const TYPE_ID_NUMBER: u8 = 2;
pub(super) const TYPE_ID_STRING: u8 = 3;
pub(super) const TYPE_ID_VECTOR2: u8 = 4;
pub(super) const TYPE_ID_VECTOR3: u8 = 5;
pub(super) const TYPE_ID_UDIM: u8 = 6;
pub(super) const TYPE_ID_UDIM2: u8 = 7;
pub(super) const TYPE_ID_COLOR3: u8 = 8;
pub(super) const TYPE_ID_BRICK_COLOR: u8 = 9;
pub(super) const TYPE_ID_ENUM_ITEM: u8 = 10;
pub(super) const TYPE_ID_CFRAME: u8 = 11;
pub(super) const TYPE_ID_RECT: u8 = 12;
pub(super) const TYPE_ID_FONT: u8 = 13;
pub(super) const TYPE_ID_COLOR_SEQUENCE: u8 = 14;
pub(super) const TYPE_ID_NUMBER_SEQUENCE: u8 = 15;
pub(super) const TYPE_ID_REF: u8 = 16;
pub(super) const TYPE_ID_CONTENT_ID: u8 = 17;
pub(super) const TYPE_ID_BINARY_STRING: u8 = 18;
pub(super) const TYPE_ID_NUMBER_RANGE: u8 = 19;
pub(super) const TYPE_ID_PHYSICAL_PROPERTIES: u8 = 20;
pub(super) const TYPE_ID_AXES: u8 = 21;
pub(super) const TYPE_ID_FACES: u8 = 22;
pub(super) const TYPE_ID_RAY: u8 = 23;

pub(super) const AXIS_NAMES: [(u8, &str); 3] = [(1, "X"), (2, "Y"), (4, "Z")];
pub(super) const FACE_NAMES: [(u8, &str); 6] = [
    (1, "Right"),
    (2, "Top"),
    (4, "Back"),
    (8, "Left"),
    (16, "Bottom"),
    (32, "Front"),
];

fn property_type_info_from_rbx_dom(property_value: &Value) -> Option<(u8, Option<String>)> {
    let data_type = property_value.get("DataType")?.as_object()?;
    if let Some(enum_name) = data_type.get("Enum").and_then(Value::as_str) {
        return Some((TYPE_ID_ENUM_ITEM, Some(format!("Enum.{enum_name}"))));
    }
    let value_type = data_type.get("Value").and_then(Value::as_str)?;
    let type_id = match value_type {
        "Bool" => TYPE_ID_BOOL,
        "Int32" | "Int64" | "Float32" | "Float64" => TYPE_ID_NUMBER,
        "String" => TYPE_ID_STRING,
        "ContentId" | "Content" => TYPE_ID_CONTENT_ID,
        "BinaryString" => TYPE_ID_BINARY_STRING,
        "Ref" => TYPE_ID_REF,
        "Vector2" => TYPE_ID_VECTOR2,
        "Vector3" => TYPE_ID_VECTOR3,
        "UDim" => TYPE_ID_UDIM,
        "UDim2" => TYPE_ID_UDIM2,
        "Color3" | "Color3uint8" => TYPE_ID_COLOR3,
        "BrickColor" => TYPE_ID_BRICK_COLOR,
        "CFrame" | "OptionalCFrame" => TYPE_ID_CFRAME,
        "Rect" => TYPE_ID_RECT,
        "Font" => TYPE_ID_FONT,
        "ColorSequence" => TYPE_ID_COLOR_SEQUENCE,
        "NumberSequence" => TYPE_ID_NUMBER_SEQUENCE,
        "NumberRange" => TYPE_ID_NUMBER_RANGE,
        "PhysicalProperties" => TYPE_ID_PHYSICAL_PROPERTIES,
        "Axes" => TYPE_ID_AXES,
        "Faces" => TYPE_ID_FACES,
        "Ray" => TYPE_ID_RAY,
        _ => return None,
    };
    Some((type_id, None))
}

fn property_schema_cache_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".renium")
        .join("cache")
        .join("property_schema_cache.v6.json")
}

fn schema_property_name_for_class(class_name: &str, property_name: &str) -> String {
    if matches!(class_name, "Model" | "WorldModel") && property_name == "WorldPivotData" {
        "WorldPivot".to_string()
    } else {
        property_name.to_string()
    }
}

fn rbx_dom_class_is_a(
    classes: &Map<String, Value>,
    class_name: &str,
    superclass_name: &str,
) -> bool {
    let mut current = class_name;
    let mut seen = HashSet::new();
    while !current.is_empty() {
        if current == superclass_name {
            return true;
        }
        if !seen.insert(current.to_string()) {
            return false;
        }
        let Some(class_value) = classes.get(current).and_then(Value::as_object) else {
            return false;
        };
        let Some(next) = class_value
            .get("Superclass")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        current = next;
    }
    false
}

fn rbx_dom_property_data_for_class<'a>(
    classes: &'a Map<String, Value>,
    class_name: &str,
    property_name: &str,
) -> Option<&'a Value> {
    let mut current = class_name;
    let mut seen = HashSet::new();
    while !current.is_empty() {
        if !seen.insert(current.to_string()) {
            return None;
        }
        let class_value = classes.get(current)?.as_object()?;
        if let Some(property_value) = class_value
            .get("Properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(property_name))
        {
            return Some(property_value);
        }
        current = class_value
            .get("Superclass")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
    }
    None
}

fn add_supplemental_readable_transport_properties(
    class_name: &str,
    classes: &Map<String, Value>,
    ordered: &mut Vec<PropertySchemaEntry>,
    seen: &mut HashSet<String>,
) {
    if !rbx_dom_class_is_a(classes, class_name, TRIANGLE_MESH_PART_CLASS) {
        return;
    }

    let Some(property_value) =
        rbx_dom_property_data_for_class(classes, class_name, MESH_SIZE_TRANSPORT_PROPERTY)
    else {
        return;
    };
    if rbx_dom_property_value_type(property_value) != Some("Vector3") {
        return;
    }

    let Some((type_id, enum_type)) = property_type_info_from_rbx_dom(property_value) else {
        return;
    };
    let schema_name = schema_property_name_for_class(class_name, MESH_SIZE_TRANSPORT_PROPERTY);
    let key = schema_name.to_ascii_lowercase();
    if seen.insert(key) {
        ordered.push(PropertySchemaEntry {
            name: schema_name,
            type_id,
            enum_type,
        });
    }
}

fn metadata_modified_unix_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .unwrap_or(0)
}

fn try_read_property_schema_cache(
    cache_path: &Path,
    source_len: u64,
    source_mtime_ms: u64,
) -> Option<PropertySchemaMap> {
    if !cache_path.exists() {
        return None;
    }
    let cache: PropertySchemaCacheFile = match read_json_file(cache_path) {
        Ok(value) => value,
        Err(err) => {
            println!(
                "[renium] warning: ignoring invalid property schema cache {}: {err}",
                cache_path.display()
            );
            return None;
        }
    };
    if cache.version != PROPERTY_SCHEMA_CACHE_VERSION
        || cache.source_len != source_len
        || cache.source_mtime_ms != source_mtime_ms
    {
        return None;
    }
    Some(cache.classes)
}

fn write_property_schema_cache(
    cache_path: &Path,
    source_len: u64,
    source_mtime_ms: u64,
    classes: &PropertySchemaMap,
) {
    let Some(parent) = cache_path.parent() else {
        return;
    };
    if let Err(err) = fs::create_dir_all(parent) {
        println!(
            "[renium] warning: failed to create property schema cache dir {}: {err}",
            parent.display()
        );
        return;
    }
    let payload = PropertySchemaCacheFile {
        version: PROPERTY_SCHEMA_CACHE_VERSION,
        source_len,
        source_mtime_ms,
        classes: classes.clone(),
    };
    if let Err(err) = write_json_file(cache_path, &payload, true) {
        println!(
            "[renium] warning: failed to write property schema cache {}: {err}",
            cache_path.display()
        );
    }
}

pub(super) fn load_rbx_dom_property_schema(
    project_root: &Path,
) -> Result<Option<PropertySchemaMap>> {
    let database_path = [
        "_external/rbx-dom/rbx_dom_lua/src/database.json",
        "tools/vendor/rbx-dom/rbx_dom_lua/src/database.json",
    ]
    .iter()
    .map(|candidate| project_root.join(candidate))
    .find(|path| path.exists());
    let Some(database_path) = database_path else {
        return Ok(None);
    };

    let metadata = fs::metadata(&database_path)
        .with_context(|| format!("Failed to stat {}", database_path.display()))?;
    let source_len = metadata.len();
    let source_mtime_ms = metadata_modified_unix_ms(&metadata);
    let cache_path = property_schema_cache_path(project_root);
    if let Some(cached) = try_read_property_schema_cache(&cache_path, source_len, source_mtime_ms) {
        return Ok(Some(cached));
    }

    let database_value: Value = read_json_file(&database_path)?;
    let classes = database_value
        .get("Classes")
        .and_then(Value::as_object)
        .with_context(|| format!("Missing Classes object in {}", database_path.display()))?;

    let mut by_class: PropertySchemaMap = HashMap::new();
    let mut memo: HashMap<String, Vec<PropertySchemaEntry>> = HashMap::new();
    let mut visiting: HashSet<String> = HashSet::new();

    for class_name in classes.keys() {
        let property_names =
            collect_rbx_dom_properties_for_class(class_name, classes, &mut memo, &mut visiting);
        if property_names.is_empty() {
            continue;
        }
        by_class.insert(class_name.clone(), property_names);
    }

    if by_class.is_empty() {
        return Ok(None);
    }
    write_property_schema_cache(&cache_path, source_len, source_mtime_ms, &by_class);
    Ok(Some(by_class))
}

fn property_schema_map_to_value(by_class: &PropertySchemaMap) -> Value {
    let mut out = Map::new();
    for (class_name, property_entries) in by_class {
        out.insert(
            class_name.clone(),
            Value::Array(
                property_entries
                    .iter()
                    .map(|entry| {
                        let mut fields = vec![
                            Value::String(entry.name.clone()),
                            Value::Number(serde_json::Number::from(entry.type_id)),
                        ];
                        if let Some(enum_type) = &entry.enum_type {
                            fields.push(Value::String(enum_type.clone()));
                        }
                        Value::Array(fields)
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

pub(super) fn configure_bridge_property_candidates(
    bridge: &BridgeServer,
    property_schema_by_class: &PropertySchemaMap,
) -> Result<()> {
    let result = bridge.call(
        "configurePropertyCandidates",
        json!({ "classes": property_schema_map_to_value(property_schema_by_class) }),
    )?;
    let class_count = result
        .get("classCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let property_count = result
        .get("propertyCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!(
        "[renium] configured property candidates from rbx-dom: classes={class_count}, properties={property_count}"
    );
    Ok(())
}

pub(super) fn parse_property_schema_map(value: Option<&Value>) -> Result<PropertySchemaMap> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let Some(classes) = value.as_object() else {
        bail!("propertySchemaByClass must be an object");
    };

    let mut out = HashMap::with_capacity(classes.len());
    for (class_name, property_entries_value) in classes {
        let entries = property_entries_value
            .as_array()
            .with_context(|| format!("propertySchemaByClass.{class_name} must be an array"))?
            .iter()
            .filter_map(|value| {
                let fields = value.as_array()?;
                let name = fields.first()?.as_str()?.to_string();
                let type_id = fields.get(1)?.as_u64()? as u8;
                let enum_type = fields
                    .get(2)
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                Some(PropertySchemaEntry {
                    name,
                    type_id,
                    enum_type,
                })
            })
            .collect::<Vec<_>>();
        if !entries.is_empty() {
            out.insert(class_name.clone(), entries);
        }
    }
    Ok(out)
}

pub(super) fn parse_enum_value_name_map(value: Option<&Value>) -> Result<EnumValueNameMap> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    if let Some(items) = value.as_array()
        && items.is_empty()
    {
        return Ok(HashMap::new());
    }
    let Some(enum_types) = value.as_object() else {
        bail!("enumValueNamesByType must be an object");
    };

    let mut out = HashMap::with_capacity(enum_types.len());
    for (enum_type, value_names_value) in enum_types {
        let value_names = value_names_value
            .as_object()
            .with_context(|| format!("enumValueNamesByType.{enum_type} must be an object"))?;
        let mut by_value = HashMap::with_capacity(value_names.len());
        for (raw_value, name_value) in value_names {
            let Ok(enum_value) = raw_value.parse::<i64>() else {
                continue;
            };
            let Some(name) = name_value.as_str().filter(|name| !name.is_empty()) else {
                continue;
            };
            by_value.insert(enum_value, name.to_string());
        }
        if !by_value.is_empty() {
            out.insert(enum_type.clone(), by_value);
        }
    }
    Ok(out)
}

pub(super) fn parse_string_list(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        bail!("Expected string array");
    };
    Ok(items
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect())
}

pub(super) fn collect_rbx_dom_properties_for_class(
    class_name: &str,
    classes: &Map<String, Value>,
    memo: &mut HashMap<String, Vec<PropertySchemaEntry>>,
    visiting: &mut HashSet<String>,
) -> Vec<PropertySchemaEntry> {
    if let Some(cached) = memo.get(class_name) {
        return cached.clone();
    }
    if !visiting.insert(class_name.to_string()) {
        return Vec::new();
    }

    let mut ordered: Vec<PropertySchemaEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(class_value) = classes.get(class_name).and_then(Value::as_object) {
        if let Some(superclass) = class_value
            .get("Superclass")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let inherited =
                collect_rbx_dom_properties_for_class(superclass, classes, memo, visiting);
            for mut inherited_entry in inherited {
                inherited_entry.name =
                    schema_property_name_for_class(class_name, &inherited_entry.name);
                let key = inherited_entry.name.to_ascii_lowercase();
                if seen.insert(key) {
                    ordered.push(inherited_entry);
                }
            }
        }

        if let Some(properties) = class_value.get("Properties").and_then(Value::as_object) {
            for (property_name, property_value) in properties {
                if property_name.eq_ignore_ascii_case("source")
                    || property_name.eq_ignore_ascii_case("robloxlocked")
                {
                    continue;
                }
                if is_engine_managed_studio_property(classes, class_name, property_value) {
                    continue;
                }
                if has_blocked_studio_property_tag(property_value)
                    || !is_serialized_property(property_value)
                {
                    continue;
                }
                if !is_supported_property_data_type(property_value) {
                    continue;
                }

                let Some((type_id, enum_type)) = property_type_info_from_rbx_dom(property_value)
                else {
                    continue;
                };

                let schema_name = schema_property_name_for_class(class_name, property_name);
                let key = schema_name.to_ascii_lowercase();
                if seen.insert(key) {
                    ordered.push(PropertySchemaEntry {
                        name: schema_name,
                        type_id,
                        enum_type,
                    });
                }
            }
        }

        add_supplemental_readable_transport_properties(
            class_name,
            classes,
            &mut ordered,
            &mut seen,
        );
    }

    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    visiting.remove(class_name);
    memo.insert(class_name.to_string(), ordered.clone());
    ordered
}

fn has_blocked_studio_property_tag(property_value: &Value) -> bool {
    let Some(tags) = property_value.get("Tags").and_then(Value::as_array) else {
        return false;
    };
    tags.iter().filter_map(Value::as_str).any(|tag| {
        matches!(tag, "Hidden" | "Deprecated" | "NotBrowsable" | "WriteOnly")
            || (tag == "ReadOnly" && !is_serializing_alias_canonical_property(property_value))
    })
}

fn is_serializing_alias_canonical_property(property_value: &Value) -> bool {
    property_value
        .get("Kind")
        .and_then(|kind| kind.get("Canonical"))
        .and_then(|canonical| canonical.get("Serialization"))
        .and_then(|serialization| serialization.get("SerializesAs"))
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
}

fn is_serialized_property(property_value: &Value) -> bool {
    if property_value
        .get("Kind")
        .and_then(|kind| kind.get("Alias"))
        .is_some()
    {
        return false;
    }
    let Some(serialization) = property_value
        .get("Kind")
        .and_then(|kind| kind.get("Canonical"))
        .and_then(|canonical| canonical.get("Serialization"))
    else {
        return true;
    };

    !matches!(serialization, Value::String(mode) if mode == "DoesNotSerialize")
}

fn rbx_dom_property_value_type(property_value: &Value) -> Option<&str> {
    property_value
        .get("DataType")
        .and_then(Value::as_object)
        .and_then(|data_type| data_type.get("Value"))
        .and_then(Value::as_str)
}

fn class_has_tag(classes: &Map<String, Value>, class_name: &str, tag_name: &str) -> bool {
    classes
        .get(class_name)
        .and_then(|class_value| class_value.get("Tags"))
        .and_then(Value::as_array)
        .is_some_and(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .any(|tag| tag == tag_name)
        })
}

fn is_engine_managed_studio_property(
    classes: &Map<String, Value>,
    class_name: &str,
    property_value: &Value,
) -> bool {
    if matches!(
        rbx_dom_property_value_type(property_value),
        Some("UniqueId" | "SecurityCapabilities")
    ) {
        return true;
    }

    rbx_dom_property_value_type(property_value) == Some("Ref")
        && class_has_tag(classes, class_name, "Service")
}

fn is_supported_property_data_type(property_value: &Value) -> bool {
    let Some(data_type) = property_value.get("DataType").and_then(Value::as_object) else {
        return false;
    };

    if data_type.contains_key("Enum") {
        return true;
    }

    let Some(value_type) = data_type.get("Value").and_then(Value::as_str) else {
        return false;
    };

    matches!(
        value_type,
        "Bool"
            | "Int32"
            | "Int64"
            | "Float32"
            | "Float64"
            | "String"
            | "BinaryString"
            | "ContentId"
            | "Content"
            | "Ref"
            | "Vector2"
            | "Vector3"
            | "UDim"
            | "UDim2"
            | "Color3"
            | "Color3uint8"
            | "ColorSequence"
            | "NumberSequence"
            | "NumberRange"
            | "CFrame"
            | "OptionalCFrame"
            | "Rect"
            | "Font"
            | "BrickColor"
            | "PhysicalProperties"
            | "Axes"
            | "Faces"
            | "Ray"
    )
}
