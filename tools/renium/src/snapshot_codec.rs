use anyhow::{Context, Result, bail};
use rbx_dom_weak::Ustr as RbxUstr;
use serde_json::{Map, Value, json};

use super::bridge_server::SourceBatchMap;
use super::editor_paths::script_file_names;
use super::native_editor::decode_bridge_buffer;
use super::property_schema::{
    AXIS_NAMES, EnumValueNameMap, FACE_NAMES, PropertySchemaEntry, PropertySchemaMap, TYPE_ID_AXES,
    TYPE_ID_BINARY_STRING, TYPE_ID_BOOL, TYPE_ID_BRICK_COLOR, TYPE_ID_CFRAME,
    TYPE_ID_COLOR_SEQUENCE, TYPE_ID_COLOR3, TYPE_ID_CONTENT_ID, TYPE_ID_ENUM_ITEM, TYPE_ID_FACES,
    TYPE_ID_FONT, TYPE_ID_NUMBER, TYPE_ID_NUMBER_RANGE, TYPE_ID_NUMBER_SEQUENCE,
    TYPE_ID_PHYSICAL_PROPERTIES, TYPE_ID_RAY, TYPE_ID_RECT, TYPE_ID_REF, TYPE_ID_STRING,
    TYPE_ID_UDIM, TYPE_ID_UDIM2, TYPE_ID_VECTOR2, TYPE_ID_VECTOR3,
};
use super::rbx_decode::canonicalize_nonfinite_float_json;
use super::snapshot_types::{NativeOverlayItem, SnapshotInstance};

pub(super) fn decode_compact_batch_debug_ids(
    raw_debug_ids: Vec<Value>,
    strings: &[String],
) -> Result<Vec<Option<String>>> {
    if raw_debug_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(raw_debug_ids.len());
    for raw in raw_debug_ids {
        match raw {
            Value::Null | Value::Bool(false) => out.push(None),
            Value::String(text) if text.is_empty() => out.push(None),
            Value::String(text) => out.push(Some(text)),
            Value::Number(number) => {
                let numeric = number
                    .as_f64()
                    .with_context(|| format!("Compact debug id is not numeric: {number}"))?;
                if numeric.fract() != 0.0 || numeric.abs() > 9_007_199_254_740_991.0 {
                    bail!("Compact debug id is not an exact integer: {number}");
                }
                if numeric < 0.0 {
                    let numeric_id = (-numeric - 1.0) as u64;
                    out.push(Some(format!("0_{numeric_id}")));
                    continue;
                }
                let string_id = numeric as usize;
                out.push(Some(string_from_table(strings, string_id, "debug id")?));
            }
            _ => bail!("Compact debug id must be a string, string id, numeric id, or false"),
        }
    }
    Ok(out)
}

pub(super) fn decode_native_overlay_debug_ids(
    encoded: &Value,
    encoding: &str,
    encoded_len: usize,
    count: usize,
) -> Result<Vec<Option<String>>> {
    if encoding != "nul-text-v1" {
        bail!("Native overlay debug id buffer has unsupported encoding {encoding}");
    }
    if count == 0 {
        if encoded_len != 0 {
            bail!("Native overlay debug id buffer has data for an empty range");
        }
        return Ok(Vec::new());
    }
    let minimum_len = count.saturating_mul(2).saturating_sub(1);
    let maximum_len = count.saturating_mul(257).saturating_sub(1);
    if encoded_len < minimum_len || encoded_len > maximum_len {
        bail!(
            "Native overlay debug id buffer has invalid byte length {encoded_len} for {count} ids"
        );
    }
    let bytes = decode_bridge_buffer(encoded, encoded_len, "Native overlay debug id buffer")?;
    let mut out = Vec::with_capacity(count);
    for raw in bytes.split(|byte| *byte == 0) {
        if raw.is_empty() || raw.len() > 256 {
            bail!("Native overlay debug id buffer contains an invalid id length");
        }
        let text = std::str::from_utf8(raw)
            .context("Native overlay debug id buffer contains invalid UTF-8")?;
        out.push(Some(text.to_string()));
        if out.len() > count {
            bail!("Native overlay debug id buffer contains too many ids");
        }
    }
    if out.len() != count {
        bail!(
            "Native overlay debug id buffer contains {} ids; expected {count}",
            out.len()
        );
    }
    Ok(out)
}

pub(super) fn apply_compact_batch_debug_ids(
    instances: &mut [SnapshotInstance],
    debug_ids: Vec<Option<String>>,
) {
    if debug_ids.is_empty() {
        return;
    }
    for (instance, debug_id) in instances.iter_mut().zip(debug_ids) {
        if let Some(debug_id) = debug_id.filter(|value| !value.is_empty()) {
            instance.debug_id = Some(debug_id);
        }
    }
}

fn string_from_table(strings: &[String], string_id: usize, label: &str) -> Result<String> {
    let index = string_id
        .checked_sub(1)
        .with_context(|| format!("{label} string id must be >= 1"))?;
    strings
        .get(index)
        .cloned()
        .with_context(|| format!("Unknown {label} string id {string_id}"))
}

fn compact_class_name_for_id(class_id: usize, class_names: &[String]) -> Option<RbxUstr> {
    class_names
        .get(class_id)
        .map(|class_name| RbxUstr::from(class_name.as_str()))
}

fn compact_class_name_from_value(value: Value, class_names: &[String]) -> Result<RbxUstr> {
    match value {
        Value::String(class_name) if !class_name.is_empty() => Ok(class_name.into()),
        Value::Number(number) => {
            let class_id = number
                .as_u64()
                .with_context(|| "Compact class id must be a non-negative integer")?
                as usize;
            compact_class_name_for_id(class_id, class_names)
                .with_context(|| format!("Unknown compact class id {class_id}"))
        }
        _ => bail!("Compact class entry must be a string or class id"),
    }
}

fn compact_class_index_from_value(value: Value, class_names: &[String]) -> Result<usize> {
    match value {
        Value::String(class_name) if !class_name.is_empty() => class_names
            .iter()
            .position(|candidate| candidate == &class_name)
            .with_context(|| format!("Unknown compact class name {class_name}")),
        Value::Number(number) => {
            let class_id = number
                .as_u64()
                .with_context(|| "Compact class id must be a non-negative integer")?
                as usize;
            class_names
                .get(class_id)
                .map(|_| class_id)
                .with_context(|| format!("Unknown compact class id {class_id}"))
        }
        _ => bail!("Compact class entry must be a string or class id"),
    }
}

fn parse_hex_instance_index(text: &str) -> Option<usize> {
    usize::from_str_radix(text, 16).ok()
}

fn compact_parent_index(value: Value) -> Result<Option<usize>> {
    match value {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::String(text) if text.is_empty() => Ok(None),
        Value::String(text) => Ok(parse_hex_instance_index(&text)),
        Value::Number(number) => {
            let index = number
                .as_u64()
                .with_context(|| "Compact parent id must be a non-negative integer")?
                as usize;
            Ok(Some(index))
        }
        _ => bail!("Compact parent id must be a string or non-negative integer"),
    }
}

fn decode_compact_v5_string(raw: Value, strings: &[String], label: &str) -> Result<String> {
    match raw {
        Value::String(text) => Ok(text),
        Value::Number(number) => {
            let string_id = number
                .as_u64()
                .with_context(|| format!("{label} string id must be a non-negative integer"))?
                as usize;
            string_from_table(strings, string_id, label)
        }
        _ => bail!("{label} value must be a string or string id"),
    }
}

fn decode_compact_v5_ref(raw: Value, strings: &[String]) -> Result<Value> {
    match raw {
        Value::Number(number) => {
            let instance_index = number
                .as_u64()
                .with_context(|| "Compact-v5 internal ref must be a non-negative integer")?
                as usize;
            let mut out = compact_v5_typed_object("Ref", 1);
            out.insert(
                "instanceIndex".to_string(),
                Value::Number(serde_json::Number::from(instance_index as u64)),
            );
            Ok(Value::Object(out))
        }
        Value::Array(fields) => {
            let mut iter = fields.into_iter();
            let tag = iter.next().and_then(|value| value.as_u64());
            if tag != Some(0) {
                bail!("Compact-v5 external ref payload must start with 0");
            }
            let debug_id = match iter.next() {
                Some(Value::Bool(false)) | None => None,
                Some(value) => Some(decode_compact_v5_string(
                    value,
                    strings,
                    "external ref debug id",
                )?),
            };
            let path_ordinals = match iter.next() {
                Some(Value::Array(values)) => Some(Value::Array(values)),
                Some(value) => {
                    let mut values = Vec::new();
                    values.push(Value::String(decode_compact_v5_string(
                        value,
                        strings,
                        "external ref path segment",
                    )?));
                    for value in iter {
                        values.push(Value::String(decode_compact_v5_string(
                            value,
                            strings,
                            "external ref path segment",
                        )?));
                    }
                    let mut out = compact_v5_typed_object("Ref", 2);
                    if let Some(debug_id) = debug_id {
                        out.insert("debugId".to_string(), Value::String(debug_id));
                    }
                    out.insert("pathSegments".to_string(), Value::Array(values));
                    return Ok(Value::Object(out));
                }
                None => None,
            };
            let mut path_segments = Vec::new();
            for value in iter {
                path_segments.push(Value::String(decode_compact_v5_string(
                    value,
                    strings,
                    "external ref path segment",
                )?));
            }
            let mut out = compact_v5_typed_object("Ref", 2);
            if let Some(debug_id) = debug_id {
                out.insert("debugId".to_string(), Value::String(debug_id));
            }
            if let Some(path_ordinals) = path_ordinals {
                out.insert("pathOrdinals".to_string(), path_ordinals);
            }
            out.insert("pathSegments".to_string(), Value::Array(path_segments));
            Ok(Value::Object(out))
        }
        _ => bail!("Compact-v5 ref payload must be an integer or array"),
    }
}

fn compact_v5_array(raw: Value, label: &str) -> Result<Vec<Value>> {
    match raw {
        Value::Array(values) => Ok(values
            .into_iter()
            .map(canonicalize_nonfinite_float_json)
            .collect()),
        _ => bail!("{label} payload must be an array"),
    }
}

fn compact_v5_next_value(iter: &mut std::vec::IntoIter<Value>) -> Value {
    iter.next().unwrap_or(Value::Null)
}

fn compact_v5_typed_object(type_name: &'static str, extra_fields: usize) -> Map<String, Value> {
    let mut out = Map::with_capacity(extra_fields + 1);
    out.insert("_type".to_string(), Value::String(type_name.to_string()));
    out
}

pub(super) fn decode_compact_v5_value(
    type_id: u8,
    enum_type: Option<&str>,
    raw: Value,
    strings: &[String],
    enum_value_names_by_type: &EnumValueNameMap,
) -> Result<Value> {
    match type_id {
        TYPE_ID_BOOL => Ok(raw),
        TYPE_ID_NUMBER => Ok(canonicalize_nonfinite_float_json(raw)),
        TYPE_ID_STRING | TYPE_ID_CONTENT_ID | TYPE_ID_BINARY_STRING => Ok(Value::String(
            decode_compact_v5_string(raw, strings, "property string")?,
        )),
        TYPE_ID_VECTOR2 => {
            let mut values = compact_v5_array(raw, "Compact-v5 Vector2")?.into_iter();
            let mut out = compact_v5_typed_object("Vector2", 2);
            out.insert("x".to_string(), compact_v5_next_value(&mut values));
            out.insert("y".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_VECTOR3 => {
            let mut values = compact_v5_array(raw, "Compact-v5 Vector3")?.into_iter();
            let mut out = compact_v5_typed_object("Vector3", 3);
            out.insert("x".to_string(), compact_v5_next_value(&mut values));
            out.insert("y".to_string(), compact_v5_next_value(&mut values));
            out.insert("z".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_UDIM => {
            let mut values = compact_v5_array(raw, "Compact-v5 UDim")?.into_iter();
            let mut out = compact_v5_typed_object("UDim", 2);
            out.insert("scale".to_string(), compact_v5_next_value(&mut values));
            out.insert("offset".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_UDIM2 => {
            let mut values = compact_v5_array(raw, "Compact-v5 UDim2")?.into_iter();
            let mut out = compact_v5_typed_object("UDim2", 4);
            out.insert("xScale".to_string(), compact_v5_next_value(&mut values));
            out.insert("xOffset".to_string(), compact_v5_next_value(&mut values));
            out.insert("yScale".to_string(), compact_v5_next_value(&mut values));
            out.insert("yOffset".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_COLOR3 => {
            let mut values = compact_v5_array(raw, "Compact-v5 Color3")?.into_iter();
            let mut out = compact_v5_typed_object("Color3", 3);
            out.insert("r".to_string(), compact_v5_next_value(&mut values));
            out.insert("g".to_string(), compact_v5_next_value(&mut values));
            out.insert("b".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_BRICK_COLOR => {
            let mut out = compact_v5_typed_object("BrickColor", 1);
            out.insert("number".to_string(), raw);
            Ok(Value::Object(out))
        }
        TYPE_ID_ENUM_ITEM => {
            let mut out = compact_v5_typed_object("EnumItem", 2);
            if enum_type.is_none()
                && let Value::Array(values) = &raw
                && values.len() >= 2
            {
                out.insert(
                    "enumType".to_string(),
                    Value::String(decode_compact_v5_string(
                        values[0].clone(),
                        strings,
                        "enum attribute type",
                    )?),
                );
                out.insert(
                    "name".to_string(),
                    Value::String(decode_compact_v5_string(
                        values[1].clone(),
                        strings,
                        "enum attribute item",
                    )?),
                );
                return Ok(Value::Object(out));
            }
            let enum_type = enum_type.unwrap_or("");
            out.insert("enumType".to_string(), Value::String(enum_type.to_string()));
            let name = decode_compact_v5_enum_name(raw, enum_type, enum_value_names_by_type)?;
            out.insert("name".to_string(), Value::String(name));
            Ok(Value::Object(out))
        }
        TYPE_ID_CFRAME => {
            let values = compact_v5_array(raw, "Compact-v5 CFrame")?;
            let mut out = compact_v5_typed_object("CFrame", 1);
            out.insert("components".to_string(), Value::Array(values));
            Ok(Value::Object(out))
        }
        TYPE_ID_RECT => {
            let mut values = compact_v5_array(raw, "Compact-v5 Rect")?.into_iter();
            let mut out = compact_v5_typed_object("Rect", 4);
            out.insert("minX".to_string(), compact_v5_next_value(&mut values));
            out.insert("minY".to_string(), compact_v5_next_value(&mut values));
            out.insert("maxX".to_string(), compact_v5_next_value(&mut values));
            out.insert("maxY".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_FONT => {
            let mut values = compact_v5_array(raw, "Compact-v5 Font")?.into_iter();
            let mut out = compact_v5_typed_object("Font", 3);
            out.insert(
                "family".to_string(),
                Value::String(decode_compact_v5_string(
                    compact_v5_next_value(&mut values),
                    strings,
                    "font family",
                )?),
            );
            out.insert(
                "weight".to_string(),
                Value::String(decode_compact_v5_string(
                    compact_v5_next_value(&mut values),
                    strings,
                    "font weight",
                )?),
            );
            out.insert(
                "style".to_string(),
                Value::String(decode_compact_v5_string(
                    compact_v5_next_value(&mut values),
                    strings,
                    "font style",
                )?),
            );
            Ok(Value::Object(out))
        }
        TYPE_ID_COLOR_SEQUENCE => {
            let values = compact_v5_array(raw, "Compact-v5 ColorSequence")?;
            if values.len() % 4 != 0 {
                bail!("Compact-v5 ColorSequence payload must contain groups of 4 numbers");
            }
            let mut iter = values.into_iter();
            let mut keypoints = Vec::with_capacity(iter.len() / 4);
            while iter.len() >= 4 {
                let time = compact_v5_next_value(&mut iter);
                let mut color = Map::with_capacity(3);
                color.insert("r".to_string(), compact_v5_next_value(&mut iter));
                color.insert("g".to_string(), compact_v5_next_value(&mut iter));
                color.insert("b".to_string(), compact_v5_next_value(&mut iter));
                let mut keypoint = Map::with_capacity(2);
                keypoint.insert("time".to_string(), time);
                keypoint.insert("value".to_string(), Value::Object(color));
                keypoints.push(Value::Object(keypoint));
            }
            let mut out = compact_v5_typed_object("ColorSequence", 1);
            out.insert("keypoints".to_string(), Value::Array(keypoints));
            Ok(Value::Object(out))
        }
        TYPE_ID_NUMBER_SEQUENCE => {
            let values = compact_v5_array(raw, "Compact-v5 NumberSequence")?;
            if values.len() % 3 != 0 {
                bail!("Compact-v5 NumberSequence payload must contain groups of 3 numbers");
            }
            let mut iter = values.into_iter();
            let mut keypoints = Vec::with_capacity(iter.len() / 3);
            while iter.len() >= 3 {
                let mut keypoint = Map::with_capacity(3);
                keypoint.insert("time".to_string(), compact_v5_next_value(&mut iter));
                keypoint.insert("value".to_string(), compact_v5_next_value(&mut iter));
                keypoint.insert("envelope".to_string(), compact_v5_next_value(&mut iter));
                keypoints.push(Value::Object(keypoint));
            }
            let mut out = compact_v5_typed_object("NumberSequence", 1);
            out.insert("keypoints".to_string(), Value::Array(keypoints));
            Ok(Value::Object(out))
        }
        TYPE_ID_NUMBER_RANGE => {
            let mut values = compact_v5_array(raw, "Compact-v5 NumberRange")?.into_iter();
            let mut out = compact_v5_typed_object("NumberRange", 2);
            out.insert("min".to_string(), compact_v5_next_value(&mut values));
            out.insert("max".to_string(), compact_v5_next_value(&mut values));
            Ok(Value::Object(out))
        }
        TYPE_ID_PHYSICAL_PROPERTIES => {
            if raw.is_null() || raw.as_bool() == Some(false) {
                let mut out = compact_v5_typed_object("PhysicalProperties", 1);
                out.insert("customPhysics".to_string(), Value::Bool(false));
                return Ok(Value::Object(out));
            }

            let mut values = compact_v5_array(raw, "Compact-v5 PhysicalProperties")?.into_iter();
            let mut out = compact_v5_typed_object("PhysicalProperties", 7);
            out.insert("customPhysics".to_string(), Value::Bool(true));
            out.insert("density".to_string(), compact_v5_next_value(&mut values));
            out.insert("friction".to_string(), compact_v5_next_value(&mut values));
            out.insert("elasticity".to_string(), compact_v5_next_value(&mut values));
            out.insert(
                "frictionWeight".to_string(),
                compact_v5_next_value(&mut values),
            );
            out.insert(
                "elasticityWeight".to_string(),
                compact_v5_next_value(&mut values),
            );
            out.insert(
                "acousticAbsorption".to_string(),
                values.next().unwrap_or_else(|| json!(1.0)),
            );
            Ok(Value::Object(out))
        }
        TYPE_ID_AXES => {
            let bits = raw
                .as_u64()
                .with_context(|| "Compact-v5 Axes value must be a bitmask")?
                as u8;
            let mut out = compact_v5_typed_object("Axes", 1);
            out.insert(
                "axes".to_string(),
                Value::Array(bitmask_names(bits, &AXIS_NAMES)),
            );
            Ok(Value::Object(out))
        }
        TYPE_ID_FACES => {
            let bits = raw
                .as_u64()
                .with_context(|| "Compact-v5 Faces value must be a bitmask")?
                as u8;
            let mut out = compact_v5_typed_object("Faces", 1);
            out.insert(
                "faces".to_string(),
                Value::Array(bitmask_names(bits, &FACE_NAMES)),
            );
            Ok(Value::Object(out))
        }
        TYPE_ID_RAY => {
            let mut values = compact_v5_array(raw, "Compact-v5 Ray")?.into_iter();
            let mut origin = Map::with_capacity(3);
            origin.insert("x".to_string(), compact_v5_next_value(&mut values));
            origin.insert("y".to_string(), compact_v5_next_value(&mut values));
            origin.insert("z".to_string(), compact_v5_next_value(&mut values));
            let mut direction = Map::with_capacity(3);
            direction.insert("x".to_string(), compact_v5_next_value(&mut values));
            direction.insert("y".to_string(), compact_v5_next_value(&mut values));
            direction.insert("z".to_string(), compact_v5_next_value(&mut values));
            let mut out = compact_v5_typed_object("Ray", 2);
            out.insert("origin".to_string(), Value::Object(origin));
            out.insert("direction".to_string(), Value::Object(direction));
            Ok(Value::Object(out))
        }
        TYPE_ID_REF => decode_compact_v5_ref(raw, strings),
        _ => bail!("Unsupported compact-v5 type id {type_id}"),
    }
}

pub(super) fn bitmask_names(bits: u8, names: &[(u8, &str)]) -> Vec<Value> {
    names
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| Value::String((*name).to_string()))
        .collect()
}

fn decode_compact_v5_enum_name(
    raw: Value,
    enum_type: &str,
    enum_value_names_by_type: &EnumValueNameMap,
) -> Result<String> {
    let enum_value = raw
        .as_i64()
        .with_context(|| "Compact-v5 enum value must be an integer")?;
    enum_value_names_by_type
        .get(enum_type)
        .and_then(|value_names| value_names.get(&enum_value))
        .cloned()
        .with_context(|| format!("Unknown compact-v5 enum value {enum_value} for {enum_type}"))
}

fn decode_compact_v5_attributes(raw: Value, strings: &[String]) -> Result<Map<String, Value>> {
    match raw {
        Value::Null | Value::Bool(false) => Ok(Map::new()),
        Value::Array(values) => {
            if values.len() % 3 != 0 {
                bail!("Compact-v5 attributes must contain name/type/value triplets");
            }
            let mut out = Map::with_capacity(values.len() / 3);
            let empty_enum_value_names_by_type = EnumValueNameMap::new();
            let mut iter = values.into_iter();
            while let Some(name_value) = iter.next() {
                let type_id = iter.next().and_then(|value| value.as_u64()).with_context(
                    || "Compact-v5 attribute type id must be a non-negative integer",
                )? as u8;
                let raw_value = iter.next().with_context(
                    || "Compact-v5 attributes must contain name/type/value triplets",
                )?;
                let name_id = name_value.as_u64().with_context(
                    || "Compact-v5 attribute name id must be a non-negative integer",
                )? as usize;
                let name = string_from_table(strings, name_id, "attribute name")?;
                out.insert(
                    name,
                    decode_compact_v5_value(
                        type_id,
                        None,
                        raw_value,
                        strings,
                        &empty_enum_value_names_by_type,
                    )?,
                );
            }
            Ok(out)
        }
        _ => bail!("Compact-v5 attributes must be an array or false"),
    }
}

fn compact_properties_mask_take_v5_with_schema(
    mask_value: Value,
    values_value: Value,
    class_name: &str,
    property_schema: Option<&[PropertySchemaEntry]>,
    strings: &[String],
    enum_value_names_by_type: &EnumValueNameMap,
) -> Result<Map<String, Value>> {
    let mut encoded_property_count = 0usize;
    let mask_words = match mask_value {
        Value::Null | Value::Bool(false) => Vec::new(),
        Value::Number(word) => {
            let value = word
                .as_u64()
                .with_context(|| "Compact-v5 single mask word must be a non-negative integer")?;
            let word = value as u32;
            encoded_property_count += word.count_ones() as usize;
            vec![word]
        }
        Value::Array(words) => {
            let mut mask_words = Vec::with_capacity(words.len());
            for word in words {
                let value = word
                    .as_u64()
                    .with_context(|| "Compact-v5 mask words must be non-negative integers")?;
                let word = value as u32;
                encoded_property_count += word.count_ones() as usize;
                mask_words.push(word);
            }
            mask_words
        }
        _ => bail!("Compact-v5 property mask must be a non-negative integer, array, or false"),
    };

    let mut values_iter = match values_value {
        Value::Null | Value::Bool(false) => Vec::new().into_iter(),
        Value::Array(values) => values.into_iter(),
        _ => bail!("Compact-v5 property values must be an array or false"),
    };

    let mut out = Map::with_capacity(encoded_property_count);
    for (word_index, mut word) in mask_words.into_iter().enumerate() {
        while word != 0 {
            let bit_index = word.trailing_zeros() as usize;
            let property_index = word_index * 31 + bit_index;
            word &= !(1u32 << bit_index);

            let schema_entry = property_schema
                .and_then(|entries| entries.get(property_index))
                .with_context(|| {
                    format!(
                        "Unknown compact-v5 property id {property_index} for class {class_name}"
                    )
                })?;
            out.insert(
                schema_entry.name.clone(),
                decode_compact_v5_value(
                    schema_entry.type_id,
                    schema_entry.enum_type.as_deref(),
                    values_iter
                        .next()
                        .with_context(|| "Compact-v5 property mask/value counts do not match")?,
                    strings,
                    enum_value_names_by_type,
                )?,
            );
        }
    }

    if values_iter.next().is_some() {
        bail!("Compact-v5 property values contained more items than the property mask");
    }

    Ok(out)
}

struct CompactV5InstanceShape {
    class_name: RbxUstr,
    mask: Value,
}

fn parse_compact_v5_instance_shapes(
    raw_shapes: Vec<Value>,
    class_names: &[String],
) -> Result<Vec<CompactV5InstanceShape>> {
    let mut out = Vec::with_capacity(raw_shapes.len());
    for (shape_offset, raw_shape) in raw_shapes.into_iter().enumerate() {
        let mut fields = match raw_shape {
            Value::Array(fields) => fields.into_iter(),
            _ => bail!("Compact-v5 shape entry must be an array"),
        };
        let class_name =
            compact_class_name_from_value(fields.next().unwrap_or(Value::Null), class_names)
                .with_context(|| {
                    format!(
                        "Invalid class value in compact-v5 shape {}",
                        shape_offset + 1
                    )
                })?;
        let mask = fields.next().unwrap_or(Value::Bool(false));
        if fields.next().is_some() {
            bail!("Compact-v5 shape entry has unsupported field count greater than 2");
        }
        out.push(CompactV5InstanceShape { class_name, mask });
    }
    Ok(out)
}

fn compact_v5_mask_has_properties(mask: &Value) -> bool {
    match mask {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_u64().is_some_and(|value| value != 0),
        Value::Array(words) => words
            .iter()
            .any(|word| word.as_u64().is_some_and(|value| value != 0)),
        _ => true,
    }
}

fn compact_v5_shape_id(raw: Value) -> Result<usize> {
    let shape_id =
        raw.as_u64()
            .with_context(|| "Compact-v5 shape id must be a positive integer")? as usize;
    shape_id
        .checked_sub(1)
        .with_context(|| "Compact-v5 shape id must be >= 1")
}

pub(super) fn parse_compact_v5_shape_instance_items(
    raw_items: Value,
    strings: Vec<String>,
    raw_shapes: Vec<Value>,
    batch_start: usize,
    property_schema_by_class: &PropertySchemaMap,
    enum_value_names_by_type: &EnumValueNameMap,
    class_names: &[String],
) -> Result<Vec<SnapshotInstance>> {
    let Value::Array(values) = raw_items else {
        bail!("Compact-v5 shape instance items must be an array");
    };
    let shapes = parse_compact_v5_instance_shapes(raw_shapes, class_names)?;
    let mut out = Vec::with_capacity(values.len());

    for (row_offset, value) in values.into_iter().enumerate() {
        let mut fields = match value {
            Value::Array(fields) => fields.into_iter(),
            _ => bail!("Compact-v5 shape instance item must be an array"),
        };
        let name = decode_compact_v5_string(
            fields.next().unwrap_or(Value::Null),
            &strings,
            "instance name",
        )?;
        let parent_index = compact_parent_index(fields.next().unwrap_or(Value::Null))?;
        let shape_index = compact_v5_shape_id(fields.next().unwrap_or(Value::Null))?;
        let shape = shapes
            .get(shape_index)
            .with_context(|| format!("Unknown compact-v5 shape id {}", shape_index + 1))?;
        let field4 = fields.next();
        let field5 = fields.next();
        if fields.next().is_some() {
            bail!("Compact-v5 shape instance row has unsupported field count greater than 5");
        }

        let shape_has_properties = compact_v5_mask_has_properties(&shape.mask);
        let (attributes_raw, values_raw) = if shape_has_properties {
            match (field4, field5) {
                (None, None) => (Value::Bool(false), Value::Bool(false)),
                (Some(values_raw), None) => (Value::Bool(false), values_raw),
                (Some(attributes_raw), Some(values_raw)) => (attributes_raw, values_raw),
                (None, Some(_)) => {
                    bail!("Compact-v5 shape row cannot have property values without field 4")
                }
            }
        } else {
            match (field4, field5) {
                (None, None) => (Value::Bool(false), Value::Bool(false)),
                (Some(attributes_raw), None) => (attributes_raw, Value::Bool(false)),
                (Some(_), Some(_)) => {
                    bail!("Compact-v5 shape row without a property mask cannot contain values")
                }
                (None, Some(_)) => {
                    bail!("Compact-v5 shape row cannot have field 5 without field 4")
                }
            }
        };

        let attributes = decode_compact_v5_attributes(attributes_raw, &strings)?;
        let property_schema = property_schema_by_class.get(shape.class_name.as_str());
        let mut properties = compact_properties_mask_take_v5_with_schema(
            shape.mask.clone(),
            values_raw,
            shape.class_name.as_str(),
            property_schema.map(Vec::as_slice),
            &strings,
            enum_value_names_by_type,
        )?;
        let instance_index = batch_start + row_offset;
        if script_file_names(&shape.class_name).is_some() {
            properties.insert(
                "Source".to_string(),
                Value::String("__SOURCE_EXTERNAL__".to_string()),
            );
        }

        out.push(SnapshotInstance {
            path: String::new(),
            path_segments: Vec::new(),
            name,
            class_name: shape.class_name,
            properties,
            source_key: None,
            parent_path: None,
            attributes,
            debug_id: None,
            parent_debug_id: None,
            instance_id: None,
            parent_instance_id: None,
            instance_index: Some(instance_index),
            parent_index,
        });
    }

    Ok(out)
}

pub(super) fn parse_native_overlay_class_groups(
    raw_groups: Value,
    strings: Vec<String>,
    batch_start: usize,
    batch_count: usize,
    property_schema_by_class: &PropertySchemaMap,
    enum_value_names_by_type: &EnumValueNameMap,
    class_names: &[String],
) -> Result<Vec<NativeOverlayItem>> {
    let Value::Array(groups) = raw_groups else {
        bail!("Native overlay class groups must be an array");
    };
    let mut out = Vec::new();
    let mut seen_offsets = vec![false; batch_count];
    let mut seen_classes = vec![false; class_names.len()];
    for group in groups {
        let mut fields = match group {
            Value::Array(fields) if fields.len() == 2 => fields.into_iter(),
            Value::Array(_) => {
                bail!("Native overlay class group must contain a class and row array")
            }
            _ => bail!("Native overlay class group must be an array"),
        };
        let class_index =
            compact_class_index_from_value(fields.next().unwrap_or(Value::Null), class_names)?;
        if std::mem::replace(&mut seen_classes[class_index], true) {
            bail!("Native overlay class group is duplicated");
        }
        let class_name = &class_names[class_index];
        let property_schema = property_schema_by_class.get(class_name);
        let Value::Array(rows) = fields.next().unwrap_or(Value::Null) else {
            bail!("Native overlay class rows must be an array");
        };
        out.reserve(rows.len());
        for row in rows {
            let Value::Array(fields) = row else {
                bail!("Native overlay class row must be an array");
            };
            if fields.len() != 2 && fields.len() != 4 {
                bail!(
                    "Native overlay class row must contain offset, attributes, and optional property fields"
                );
            }
            let offset = fields[0]
                .as_u64()
                .map(|value| value as usize)
                .filter(|value| *value > 0 && *value <= batch_count)
                .context("Native overlay item offset is out of range")?;
            if std::mem::replace(&mut seen_offsets[offset - 1], true) {
                bail!("Native overlay item offset {offset} is duplicated");
            }
            let attributes = decode_compact_v5_attributes(fields[1].clone(), &strings)?;
            let properties = if fields.len() == 4 {
                compact_properties_mask_take_v5_with_schema(
                    fields[2].clone(),
                    fields[3].clone(),
                    class_name,
                    property_schema.map(Vec::as_slice),
                    &strings,
                    enum_value_names_by_type,
                )?
            } else {
                Map::new()
            };
            out.push(NativeOverlayItem {
                instance_index: batch_start + offset - 1,
                class_index,
                properties,
                attributes,
            });
        }
    }
    Ok(out)
}

pub(super) fn parse_compact_v5_instance_items(
    raw_items: Value,
    strings: Vec<String>,
    batch_start: usize,
    property_schema_by_class: &PropertySchemaMap,
    enum_value_names_by_type: &EnumValueNameMap,
    class_names: &[String],
) -> Result<Vec<SnapshotInstance>> {
    let Value::Array(values) = raw_items else {
        bail!("Compact-v5 instance items must be an array");
    };
    let mut out = Vec::with_capacity(values.len());
    for (row_offset, value) in values.into_iter().enumerate() {
        let mut fields = match value {
            Value::Array(fields) => fields.into_iter(),
            _ => bail!("Compact-v5 instance item must be an array"),
        };
        let name = match fields.next().unwrap_or(Value::Null) {
            Value::String(text) => text,
            Value::Number(number) => {
                let name_id = number
                    .as_u64()
                    .with_context(|| "Compact-v5 instance name id must be a non-negative integer")?
                    as usize;
                string_from_table(&strings, name_id, "instance name")?
            }
            _ => bail!("Compact-v5 instance name must be a string or string id"),
        };
        let class_name =
            compact_class_name_from_value(fields.next().unwrap_or(Value::Null), class_names)?;
        let property_schema = property_schema_by_class.get(class_name.as_str());
        let parent_index = compact_parent_index(fields.next().unwrap_or(Value::Null))?;
        let field4 = fields.next();
        let field5 = fields.next();
        let field6 = fields.next();
        if fields.next().is_some() {
            bail!("Compact-v5 instance row cannot contain more than 6 fields");
        }
        let (attributes_raw, mask_raw, values_raw) = match (field4, field5, field6) {
            (None, None, None) => (Value::Bool(false), Value::Bool(false), Value::Bool(false)),
            (Some(attributes_raw), None, None) => {
                (attributes_raw, Value::Bool(false), Value::Bool(false))
            }
            (Some(mask_raw), Some(values_raw), None) => (Value::Bool(false), mask_raw, values_raw),
            (Some(attributes_raw), Some(mask_raw), Some(values_raw)) => {
                (attributes_raw, mask_raw, values_raw)
            }
            _ => bail!("Compact-v5 instance row has missing intermediate fields"),
        };
        let attributes = decode_compact_v5_attributes(attributes_raw, &strings)?;
        let properties = compact_properties_mask_take_v5_with_schema(
            mask_raw,
            values_raw,
            class_name.as_str(),
            property_schema.map(Vec::as_slice),
            &strings,
            enum_value_names_by_type,
        )?;
        let instance_index = batch_start + row_offset;
        let mut properties = properties;
        if script_file_names(&class_name).is_some() {
            properties.insert(
                "Source".to_string(),
                Value::String("__SOURCE_EXTERNAL__".to_string()),
            );
        }
        out.push(SnapshotInstance {
            path: String::new(),
            path_segments: Vec::new(),
            name,
            class_name,
            properties,
            source_key: None,
            parent_path: None,
            attributes,
            debug_id: None,
            parent_debug_id: None,
            instance_id: None,
            parent_instance_id: None,
            instance_index: Some(instance_index),
            parent_index,
        });
    }
    Ok(out)
}

pub(super) fn parse_source_range_batch(raw: Value) -> Result<SourceBatchMap> {
    let items = raw
        .get("items")
        .and_then(Value::as_array)
        .with_context(|| "Source range payload items must be an array")?;
    if items.len() % 2 != 0 {
        bail!("Source range payload items must contain key/source pairs");
    }

    let mut out = SourceBatchMap::default();
    for pair in items.chunks_exact(2) {
        let source = pair[1]
            .as_str()
            .with_context(|| "Source range value must be a string")?;
        if let Some(index) = pair[0].as_u64() {
            out.by_index.insert(index as usize, source.to_string());
            continue;
        }

        let key = pair[0]
            .as_str()
            .with_context(|| "Source range key must be a string or non-negative integer")?;
        if let Some(index_text) = key.strip_prefix("id:")
            && let Some(index) = parse_hex_instance_index(index_text)
        {
            out.by_index.insert(index, source.to_string());
        }
        out.by_key.insert(key.to_string(), source.to_string());
    }
    Ok(out)
}
