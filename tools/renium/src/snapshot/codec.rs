use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::rbx::decode::canonicalize_nonfinite_float_json;
use crate::roblox::schema::{
    AXIS_NAMES, EnumValueNameMap, FACE_NAMES, PropertySchemaEntry, PropertySchemaMap, TYPE_ID_AXES,
    TYPE_ID_BINARY_STRING, TYPE_ID_BOOL, TYPE_ID_BRICK_COLOR, TYPE_ID_CFRAME,
    TYPE_ID_COLOR_SEQUENCE, TYPE_ID_COLOR3, TYPE_ID_CONTENT_ID, TYPE_ID_ENUM_ITEM, TYPE_ID_FACES,
    TYPE_ID_FONT, TYPE_ID_NUMBER, TYPE_ID_NUMBER_RANGE, TYPE_ID_NUMBER_SEQUENCE,
    TYPE_ID_PHYSICAL_PROPERTIES, TYPE_ID_RAY, TYPE_ID_RECT, TYPE_ID_REF, TYPE_ID_STRING,
    TYPE_ID_UDIM, TYPE_ID_UDIM2, TYPE_ID_VECTOR2, TYPE_ID_VECTOR3,
};
use crate::snapshot::types::NativeOverlayItem;
use crate::studio::bridge::SourceBatchMap;
use crate::studio::native::editor::decode_bridge_buffer;

pub(crate) fn decode_native_overlay_debug_ids(
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

pub(crate) fn decode_batch_settings_ids(
    raw_settings_ids: Vec<Value>,
    count: usize,
    label: &str,
) -> Result<Vec<(usize, String)>> {
    let mut seen = vec![false; count];
    let mut out = Vec::with_capacity(raw_settings_ids.len());
    for raw in raw_settings_ids {
        let Value::Array(row) = raw else {
            bail!("{label} row must be an array");
        };
        if row.len() != 2 {
            bail!("{label} row must contain an index and id");
        }
        let index = row[0]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .and_then(|value| value.checked_sub(1))
            .filter(|index| *index < count)
            .with_context(|| format!("{label} index is out of range"))?;
        let settings_id = row[1]
            .as_str()
            .filter(|value| !value.is_empty())
            .with_context(|| format!("{label} id must be a non-empty string"))?;
        if std::mem::replace(&mut seen[index], true) {
            bail!("{label} index {} is duplicated", index + 1);
        }
        out.push((index, settings_id.to_string()));
    }
    Ok(out)
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
            out.insert("instanceIndex".to_string(), json!(instance_index as u64));
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
            let decode_path_segment = |value| {
                decode_compact_v5_string(value, strings, "external ref path segment")
                    .map(Value::String)
            };
            let (path_ordinals, path_segments) = match iter.next() {
                Some(Value::Array(values)) => (
                    Some(Value::Array(values)),
                    iter.map(decode_path_segment).collect::<Result<Vec<_>>>()?,
                ),
                Some(value) => (
                    None,
                    std::iter::once(value)
                        .chain(iter)
                        .map(decode_path_segment)
                        .collect::<Result<Vec<_>>>()?,
                ),
                None => (None, Vec::new()),
            };
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

pub(crate) fn decode_compact_v5_value(
    type_id: u8,
    enum_type: Option<&str>,
    raw: Value,
    strings: &[String],
    enum_value_names_by_type: &EnumValueNameMap,
) -> Result<Value> {
    match type_id {
        TYPE_ID_BOOL => Ok(raw),
        TYPE_ID_NUMBER => Ok(canonicalize_nonfinite_float_json(raw)),
        TYPE_ID_BINARY_STRING if raw.is_object() => Ok(raw),
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

pub(crate) fn bitmask_names(bits: u8, names: &[(u8, &str)]) -> Vec<Value> {
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
    mask_value: &Value,
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

pub(crate) fn parse_native_overlay_class_groups(
    raw_groups: Value,
    strings: &[String],
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
            let attributes = decode_compact_v5_attributes(fields[1].clone(), strings)?;
            let properties = if fields.len() == 4 {
                compact_properties_mask_take_v5_with_schema(
                    &fields[2],
                    fields[3].clone(),
                    class_name,
                    property_schema.map(Vec::as_slice),
                    strings,
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

pub(crate) fn parse_source_range_batch(raw: Value) -> Result<SourceBatchMap> {
    let items = raw
        .get("items")
        .and_then(Value::as_array)
        .with_context(|| "Source range payload items must be an array")?;
    if items.len() % 2 != 0 {
        bail!("Source range payload items must contain key/source pairs");
    }

    let mut out = SourceBatchMap::default();
    for pair in items.as_chunks::<2>().0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_settings_ids_are_validated_and_applied() {
        let decoded = decode_batch_settings_ids(
            vec![json!([2, "editor:stable"]), json!([4, "editor:moved"])],
            4,
            "Test settings id",
        )
        .unwrap();
        assert_eq!(
            decoded,
            vec![
                (1, "editor:stable".to_string()),
                (3, "editor:moved".to_string())
            ]
        );
    }

    #[test]
    fn sparse_settings_ids_reject_duplicate_and_out_of_range_indices() {
        assert!(
            decode_batch_settings_ids(
                vec![json!([1, "editor:a"]), json!([1, "editor:b"])],
                2,
                "Test settings id",
            )
            .is_err()
        );
        assert!(
            decode_batch_settings_ids(vec![json!([3, "editor:a"])], 2, "Test settings id",)
                .is_err()
        );
    }
}

#[cfg(test)]
mod compact_v5_binary_string_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn binary_strings_arrive_interned_or_as_base64_payloads() {
        let strings = vec![String::from("plain")];
        let enums = EnumValueNameMap::default();
        assert_eq!(
            decode_compact_v5_value(TYPE_ID_BINARY_STRING, None, json!(1), &strings, &enums)
                .unwrap(),
            json!("plain")
        );
        let payload = json!({"_type": "BinaryString", "base64": "AAAAIEEAAIA/"});
        assert_eq!(
            decode_compact_v5_value(
                TYPE_ID_BINARY_STRING,
                None,
                payload.clone(),
                &strings,
                &enums
            )
            .unwrap(),
            payload
        );
        assert!(decode_compact_v5_value(TYPE_ID_STRING, None, payload, &strings, &enums).is_err());
    }
}
