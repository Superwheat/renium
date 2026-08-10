use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{Map, Number, Value};

use crate::file_io::write_bytes_if_changed;
use crate::property_schema::MESH_SIZE_TRANSPORT_PROPERTY;
use crate::rbx_decode::{json_number_f64, nonfinite_float_from_json};
use crate::snapshot_types::{NativeSettingsValue, ServiceState, SnapshotInstance};
use crate::timing::log_timing;

const SETTINGS_BINARY_MAGIC: &[u8] = b"RBSSET\0";
const SETTINGS_BINARY_MIN_VERSION: u8 = 10;
pub(crate) const SETTINGS_BINARY_VERSION: u8 = 11;
pub(crate) const SETTINGS_REFERENCE_SELECTOR_KEYS: [&str; 9] = [
    "instanceIndex",
    "settingsId",
    "instanceId",
    "pathSegments",
    "pathOrdinals",
    "debugId",
    "path",
    "referent",
    "ref",
];

const MAX_SETTINGS_BYTECODE_BYTES: usize = 128 * 1024 * 1024;
const MAX_SETTINGS_COLLECTION_ITEMS: usize = 500_000;
const MAX_SETTINGS_STRING_BYTES: usize = 32 * 1024 * 1024;
const MAX_SETTINGS_VALUE_DEPTH: usize = 128;
const MAX_SETTINGS_HIERARCHY_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SettingsBytecode {
    pub(crate) version: u8,
    pub(crate) instances: Vec<SettingsBytecodeInstance>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsBytecodeInstance {
    pub(crate) settings_id: String,
    pub(crate) name: String,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_index: Option<usize>,
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
}

impl SettingsBytecodeInstance {
    pub(crate) fn new(
        settings_id: String,
        name: String,
        class_name: String,
        parent_index: Option<usize>,
    ) -> Self {
        Self {
            settings_id,
            name,
            class_name,
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        }
    }
}

impl SettingsBytecode {
    pub(crate) fn read_file(path: &Path) -> Result<Self> {
        for attempt in 0..6 {
            let file_len = fs::metadata(path)
                .with_context(|| format!("Failed to stat {}", path.display()))?
                .len();
            if file_len > MAX_SETTINGS_BYTECODE_BYTES as u64 {
                bail!(
                    "Settings bytecode file exceeds safe size limit of {} bytes: {}",
                    MAX_SETTINGS_BYTECODE_BYTES,
                    path.display()
                );
            }
            let bytes =
                fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
            match decode_settings_bytecode(&bytes) {
                Ok(document) => return Ok(document),
                Err(_) if attempt < 5 => {
                    thread::sleep(Duration::from_millis(15 * (attempt + 1) as u64));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to decode {}", path.display()));
                }
            }
        }
        unreachable!("settings bytecode retry loop always returns")
    }

    pub(crate) fn write_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let bytes = encode_settings_bytecode(self)?;
        write_bytes_if_changed(path, &bytes)
    }
}

pub(crate) fn settings_reference_index(value: &Value) -> Option<usize> {
    let one_based = value
        .as_u64()
        .or_else(|| {
            value
                .as_i64()
                .and_then(|number| (number >= 0).then_some(number as u64))
        })
        .or_else(|| {
            value.as_f64().and_then(|number| {
                number
                    .is_finite()
                    .then_some(number.trunc())
                    .filter(|truncated| (*truncated - number).abs() < f64::EPSILON)
                    .and_then(|truncated| (truncated >= 0.0).then_some(truncated as u64))
            })
        })?;
    usize::try_from(one_based).ok()?.checked_sub(1)
}

pub(crate) type ReferencePath = (Vec<String>, Option<Vec<usize>>);

pub(crate) fn strict_reference_path(object: &Map<String, Value>) -> Result<Option<ReferencePath>> {
    let Some(value) = object.get("pathSegments") else {
        if object.contains_key("pathOrdinals") {
            bail!("Ref pathOrdinals require pathSegments");
        }
        return Ok(None);
    };
    let segments = value
        .as_array()
        .context("Ref pathSegments must be an array")?
        .iter()
        .map(|segment| {
            segment
                .as_str()
                .map(str::to_string)
                .context("Ref pathSegments must contain strings")
        })
        .collect::<Result<Vec<_>>>()?;
    let ordinals = object
        .get("pathOrdinals")
        .map(|value| {
            value
                .as_array()
                .context("Ref pathOrdinals must be an array")?
                .iter()
                .map(|ordinal| {
                    ordinal
                        .as_u64()
                        .filter(|ordinal| *ordinal > 0)
                        .and_then(|ordinal| usize::try_from(ordinal).ok())
                        .context("Ref pathOrdinals must contain positive integers")
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    if ordinals
        .as_ref()
        .is_some_and(|ordinals| ordinals.len() != segments.len())
    {
        bail!("Ref pathOrdinals must match pathSegments length");
    }
    Ok(Some((segments, ordinals)))
}

pub(crate) fn instance_settings_id(index: usize, instance: &SnapshotInstance) -> String {
    if instance.instance_index == Some(1) && instance.parent_index.is_none() {
        return "1".to_string();
    }
    if let Some(debug_id) = instance
        .debug_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        return format!("debug:{debug_id}");
    }
    if let Some(instance_index) = instance.instance_index.filter(|value| *value > 0) {
        return format!("{instance_index:x}");
    }
    if let Some(instance_id) = instance
        .instance_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        return instance_id.to_string();
    }
    format!("index:{index:x}")
}

pub(crate) fn child_indices_for_instance(state: &ServiceState, parent_index: usize) -> &[usize] {
    state
        .children_by_index
        .get(parent_index)
        .map_or(&[][..], Vec::as_slice)
}

pub(crate) fn should_skip_binary_property(
    state: &ServiceState,
    instance: &SnapshotInstance,
    name: &str,
    raw_value: &Value,
) -> bool {
    if name.eq_ignore_ascii_case("source")
        || name.eq_ignore_ascii_case("classname")
        || name.eq_ignore_ascii_case("parent")
        || name.eq_ignore_ascii_case("name")
        || name.eq_ignore_ascii_case("robloxlocked")
    {
        return true;
    }
    if name == "RunContext" && instance.class_name != "Script" {
        return true;
    }
    if state.properties_default_elided {
        return false;
    }
    is_default_property_value(state, &instance.class_name, name, raw_value)
}

pub(crate) fn is_default_property_value(
    state: &ServiceState,
    class_name: &str,
    property_name: &str,
    property_value: &Value,
) -> bool {
    if property_name.eq_ignore_ascii_case(MESH_SIZE_TRANSPORT_PROPERTY) {
        return false;
    }
    if matches!(
        (property_name, property_value),
        ("Archivable" | "CharacterAutoLoads", Value::Bool(true))
            | ("Sandboxed", Value::Bool(false))
    ) {
        return true;
    }
    state
        .class_defaults_by_class
        .get(class_name)
        .and_then(|properties| properties.get(property_name))
        .is_some_and(|default| default == property_value)
}

pub(crate) fn decode_settings_bytecode(bytes: &[u8]) -> Result<SettingsBytecode> {
    if bytes.len() > MAX_SETTINGS_BYTECODE_BYTES {
        bail!("Settings bytecode exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes");
    }
    let mut reader = BytecodeReader::new(bytes);
    reader.read_magic()?;
    let version = reader
        .read_u8()
        .context("Missing settings bytecode version")?;
    if !(SETTINGS_BINARY_MIN_VERSION..=SETTINGS_BINARY_VERSION).contains(&version) {
        bail!("Unsupported settings bytecode version {version}");
    }

    let decoded_len = reader.read_len("compressed settings bytecode payload length")?;
    let encoded_len = reader.read_len("compressed settings bytecode byte length")?;
    if decoded_len > MAX_SETTINGS_BYTECODE_BYTES {
        bail!(
            "Decoded settings bytecode payload exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes"
        );
    }
    if encoded_len > MAX_SETTINGS_BYTECODE_BYTES {
        bail!(
            "Compressed settings bytecode payload exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes"
        );
    }
    let encoded = reader.read_bytes(encoded_len)?;
    reader.finish()?;
    let decoded = zstd::bulk::decompress(encoded, decoded_len)?;
    let mut payload_reader = BytecodeReader::new(&decoded);
    let document = decode_settings_bytecode_payload(version, &mut payload_reader)?;
    payload_reader.finish()?;
    Ok(document)
}

fn decode_settings_bytecode_payload(
    version: u8,
    reader: &mut BytecodeReader<'_>,
) -> Result<SettingsBytecode> {
    let string_count = reader.read_collection_len("string count")?;
    let mut strings = Vec::with_capacity(string_count);
    for _ in 0..string_count {
        let len = reader.read_len("string byte length")?;
        if len > MAX_SETTINGS_STRING_BYTES {
            bail!("Settings string exceeds safe size limit of {MAX_SETTINGS_STRING_BYTES} bytes");
        }
        let bytes = reader.read_bytes(len)?;
        strings.push(String::from_utf8(bytes.to_vec()).context("Invalid UTF-8 settings string")?);
    }

    let class_count = reader.read_collection_len("class count")?;
    let mut classes = Vec::with_capacity(class_count);
    for _ in 0..class_count {
        classes.push(reader.read_string(&strings, "class string id")?.to_string());
    }

    let property_count = reader.read_collection_len("property count")?;
    let mut properties = Vec::with_capacity(property_count);
    for _ in 0..property_count {
        properties.push(
            reader
                .read_string(&strings, "property string id")?
                .to_string(),
        );
    }

    let instance_count = reader.read_collection_len("instance count")?;
    let mut instances = Vec::with_capacity(instance_count);
    for instance_index in 0..instance_count {
        let settings_id = read_compact_settings_id(reader, &strings)?;
        let name = reader.read_string(&strings, "instance name")?.to_string();
        let class_id = reader.read_len("instance class id")?;
        let class_name = classes
            .get(class_id)
            .with_context(|| format!("Invalid class id {class_id}"))?
            .clone();
        let parent_raw = reader.read_len("parent index")?;
        let parent_index = decode_parent_index(parent_raw, instance_index, instance_count)?;
        instances.push(SettingsBytecodeInstance::new(
            settings_id,
            name,
            class_name,
            parent_index,
        ));
    }
    validate_settings_hierarchy(&instances)?;

    let group_count = reader.read_collection_len("property group count")?;
    let mut specs = Vec::with_capacity(group_count);
    for _ in 0..group_count {
        let property_id = reader.read_len("property id")?;
        let property_name = properties
            .get(property_id)
            .with_context(|| format!("Invalid property id {property_id}"))?;
        let kind = reader.read_u8().context("Missing property kind")?;
        let value_count = reader.read_collection_len("property value count")?;
        let body_len = reader.read_len("property group byte length")?;
        let body = reader.read_bytes(body_len)?;
        specs.push((property_name, kind, value_count, body));
    }
    let decoded_groups = specs
        .par_iter()
        .map(|(property_name, kind, value_count, body)| {
            decode_property_group_body(
                body,
                property_name.as_str(),
                *kind,
                *value_count,
                &strings,
                instance_count,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    for ((property_name, ..), entries) in specs.iter().zip(decoded_groups) {
        for entry in entries {
            match entry {
                DecodedGroupEntry::Property(instance_index, value) => {
                    instances[instance_index]
                        .properties
                        .insert((*property_name).clone(), value);
                }
                DecodedGroupEntry::Attributes(instance_index, attributes) => {
                    instances[instance_index].attributes = attributes;
                }
            }
        }
    }

    Ok(SettingsBytecode { version, instances })
}

enum DecodedGroupEntry {
    Property(usize, Value),
    Attributes(usize, Map<String, Value>),
}

fn decode_property_group_body(
    body: &[u8],
    property_name: &str,
    kind: u8,
    value_count: usize,
    strings: &[String],
    instance_count: usize,
) -> Result<Vec<DecodedGroupEntry>> {
    let mut reader = BytecodeReader::new(body);
    let cframe_group_decoder = decode_cframe_group_header(kind, &mut reader)?;
    let mut previous_instance_index = 0_usize;
    let mut previous_ref_target_index = 0_usize;
    let mut entries = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        let encoded_index = reader.read_len("property instance index")?;
        let instance_index = previous_instance_index
            .checked_add(encoded_index)
            .context("Property instance index delta overflow")?;
        previous_instance_index = instance_index;
        if instance_index >= instance_count {
            bail!("Invalid property instance index {instance_index}");
        }
        if property_name == "Attributes" && kind == 8 {
            entries.push(DecodedGroupEntry::Attributes(
                instance_index,
                decode_attributes_payload(&mut reader, strings, instance_count)?,
            ));
        } else {
            let value = if kind == 17 {
                decode_resolved_ref_group_payload(
                    &mut reader,
                    instance_count,
                    &mut previous_ref_target_index,
                )?
            } else {
                match &cframe_group_decoder {
                    CFrameGroupDecoder::Inline => {
                        decode_raw_value_payload(&mut reader, kind, strings, instance_count, 0)?
                    }
                    CFrameGroupDecoder::RotationTable(rotations) => {
                        decode_cframe_rotation_table_payload(&mut reader, rotations)?
                    }
                }
            };
            entries.push(DecodedGroupEntry::Property(instance_index, value));
        }
    }
    reader.finish()?;
    Ok(entries)
}

pub(crate) fn encode_settings_bytecode(document: &SettingsBytecode) -> Result<Vec<u8>> {
    validate_settings_hierarchy(&document.instances)?;
    let payload = encode_settings_bytecode_payload(document)?;
    wrap_settings_bytecode_payload(&payload)
}

fn validate_settings_hierarchy(instances: &[SettingsBytecodeInstance]) -> Result<()> {
    let mut settings_ids = HashSet::with_capacity(instances.len());
    for (index, instance) in instances.iter().enumerate() {
        if instance.settings_id.is_empty() {
            bail!("Settings bytecode instance {index} has an empty settings id");
        }
        if !settings_ids.insert(instance.settings_id.as_str()) {
            bail!(
                "Settings bytecode contains duplicate settings id {}",
                instance.settings_id
            );
        }
    }
    let mut states = vec![0_u8; instances.len()];
    let mut depths = vec![0_usize; instances.len()];

    for start in 0..instances.len() {
        if states[start] == 2 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        let mut base_depth = 0_usize;
        while let Some(index) = current {
            if index >= instances.len() {
                bail!("Invalid parent index {index}");
            }
            match states[index] {
                0 => {
                    states[index] = 1;
                    path.push(index);
                    current = instances[index].parent_index;
                }
                1 => bail!("Settings bytecode hierarchy contains a parent cycle at index {index}"),
                2 => {
                    base_depth = depths[index];
                    break;
                }
                _ => unreachable!("settings hierarchy state is internal"),
            }
        }
        if base_depth.saturating_add(path.len()) > MAX_SETTINGS_HIERARCHY_DEPTH {
            bail!(
                "Settings bytecode hierarchy exceeds safe depth of {MAX_SETTINGS_HIERARCHY_DEPTH}"
            );
        }
        while let Some(index) = path.pop() {
            base_depth += 1;
            depths[index] = base_depth;
            states[index] = 2;
        }
    }
    let root_count = instances
        .iter()
        .filter(|instance| instance.parent_index.is_none())
        .count();
    if !instances.is_empty() && root_count != 1 {
        bail!("Settings bytecode must contain exactly one root, found {root_count}");
    }
    Ok(())
}

fn encode_settings_bytecode_payload(document: &SettingsBytecode) -> Result<Vec<u8>> {
    let lookup = build_bytecode_instance_lookup(document);
    let mut collected = SettingsBinaryCollection::default();

    for (instance_index, instance) in document.instances.iter().enumerate() {
        if parse_numeric_debug_settings_id(&instance.settings_id).is_none() {
            add_count(
                &mut collected.string_counts,
                instance.settings_id.as_str(),
                1,
            );
        }
        add_count(&mut collected.string_counts, instance.name.as_str(), 1);
        add_count(
            &mut collected.string_counts,
            instance.class_name.as_str(),
            1,
        );
        add_count(&mut collected.class_counts, instance.class_name.as_str(), 1);

        for (property_name, raw_value) in &instance.properties {
            let kind = binary_raw_value_kind(raw_value, &lookup)?;
            if kind == 0 {
                continue;
            }
            let property_name = property_name.as_str();
            let source = SettingsBinaryValueSource::Property(raw_value);
            push_settings_binary_value(
                &mut collected,
                &lookup,
                instance_index,
                property_name,
                kind,
                source,
            )?;
        }

        if has_binary_attributes(&instance.attributes) {
            let property_name = "Attributes";
            let source = SettingsBinaryValueSource::Attributes(&instance.attributes);
            let kind = binary_source_value_kind(&source, &lookup)?;
            push_settings_binary_value(
                &mut collected,
                &lookup,
                instance_index,
                property_name,
                kind,
                source,
            )?;
        }
    }

    let strings = sorted_counted_strings(collected.string_counts);
    let string_ids = build_id_map(&strings);
    let classes = sorted_counted_strings(collected.class_counts);
    let class_ids = build_id_map(&classes);
    let properties = sorted_counted_strings(collected.property_counts);
    let property_ids = build_id_map(&properties);
    let property_group_entries =
        sorted_settings_property_groups(collected.property_groups, &property_ids);

    let estimated_capacity = document
        .instances
        .len()
        .saturating_mul(96)
        .clamp(1024, 32 * 1024 * 1024);
    let mut writer = Vec::with_capacity(estimated_capacity);
    write_settings_binary_header(
        &mut writer,
        &strings,
        &string_ids,
        &classes,
        &properties,
        document.instances.len(),
    )?;
    for (instance_index, instance) in document.instances.iter().enumerate() {
        write_compact_settings_id(&mut writer, &string_ids, instance.settings_id.as_str())?;
        write_binary_string_id(&mut writer, &string_ids, instance.name.as_str())?;
        write_lookup_id(
            &mut writer,
            &class_ids,
            instance.class_name.as_str(),
            "class",
        )?;
        write_var_u64(
            &mut writer,
            encode_parent_index(instance.parent_index, instance_index)?,
        )?;
    }
    write_settings_binary_property_groups(
        &mut writer,
        &property_group_entries,
        &property_ids,
        &string_ids,
        &lookup,
    )?;

    Ok(writer)
}

fn wrap_settings_bytecode_payload(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_SETTINGS_BYTECODE_BYTES {
        bail!(
            "Decoded settings bytecode payload exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes"
        );
    }
    let zstd = zstd::bulk::compress(payload, 1)?;
    if zstd.len() > MAX_SETTINGS_BYTECODE_BYTES {
        bail!(
            "Compressed settings bytecode payload exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes"
        );
    }
    let mut writer = Vec::with_capacity(
        SETTINGS_BINARY_MAGIC.len()
            + 1
            + var_u64_len(payload.len() as u64)
            + var_u64_len(zstd.len() as u64)
            + zstd.len(),
    );
    writer.extend_from_slice(SETTINGS_BINARY_MAGIC);
    writer.push(SETTINGS_BINARY_VERSION);
    write_var_u64(&mut writer, payload.len() as u64)?;
    write_var_u64(&mut writer, zstd.len() as u64)?;
    writer.extend_from_slice(&zstd);

    Ok(writer)
}

fn var_u64_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn build_bytecode_instance_lookup(document: &SettingsBytecode) -> SettingsBinaryInstanceLookup {
    let mut lookup = SettingsBinaryInstanceLookup::default();
    let paths = settings_document_path_parts(document);
    for (index, instance) in document.instances.iter().enumerate() {
        lookup.by_instance_index.entry(index + 1).or_insert(index);
        lookup
            .by_settings_id
            .entry(instance.settings_id.clone())
            .or_insert(index);
        if let Some((segments, ordinals)) = paths.get(index) {
            insert_unique_path(
                &mut lookup.by_path_segments,
                path_segments_key(segments),
                index,
            );
            insert_unique_path(
                &mut lookup.by_path_parts,
                path_parts_key(segments, ordinals),
                index,
            );
            insert_unique_path(&mut lookup.by_path, segments.join("."), index);
        }
    }
    lookup
}

struct BytecodeReader<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> BytecodeReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(bytes),
        }
    }

    fn read_magic(&mut self) -> Result<()> {
        let mut magic = vec![0_u8; SETTINGS_BINARY_MAGIC.len()];
        self.cursor
            .read_exact(&mut magic)
            .context("Missing settings bytecode magic")?;
        if magic != SETTINGS_BINARY_MAGIC {
            bail!("Invalid settings bytecode magic");
        }
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8> {
        let mut buf = [0_u8; 1];
        self.cursor.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let start = self.cursor.position() as usize;
        let end = start
            .checked_add(len)
            .context("Settings bytecode offset overflow")?;
        let byte_len = self.cursor.get_ref().len();
        if end > byte_len {
            bail!("Unexpected end of settings bytecode");
        }
        self.cursor.set_position(end as u64);
        Ok(&self.cursor.get_ref()[start..end])
    }

    fn read_var_u64(&mut self) -> Result<u64> {
        let mut value = 0_u64;
        let mut shift = 0_u32;
        for _ in 0..10 {
            let byte = self.read_u8()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
        bail!("Invalid varint in settings bytecode")
    }

    fn read_len(&mut self, label: &str) -> Result<usize> {
        let raw = self
            .read_var_u64()
            .with_context(|| format!("Missing {label}"))?;
        usize::try_from(raw).with_context(|| format!("{label} does not fit in usize"))
    }

    fn read_collection_len(&mut self, label: &str) -> Result<usize> {
        let len = self.read_len(label)?;
        if len > MAX_SETTINGS_COLLECTION_ITEMS {
            bail!("{label} exceeds safe collection limit of {MAX_SETTINGS_COLLECTION_ITEMS} items");
        }
        Ok(len)
    }

    fn read_string<'b>(&mut self, strings: &'b [String], label: &str) -> Result<&'b str> {
        let id = self.read_len(label)?;
        strings
            .get(id)
            .map(String::as_str)
            .with_context(|| format!("Invalid {label} {id}"))
    }

    fn finish(&self) -> Result<()> {
        let position = self.cursor.position() as usize;
        let len = self.cursor.get_ref().len();
        if position != len {
            bail!("Settings bytecode has {} trailing bytes", len - position);
        }
        Ok(())
    }
}

fn decode_raw_value(
    reader: &mut BytecodeReader<'_>,
    strings: &[String],
    instance_count: usize,
) -> Result<Value> {
    decode_raw_value_at_depth(reader, strings, instance_count, 0)
}

fn decode_raw_value_at_depth(
    reader: &mut BytecodeReader<'_>,
    strings: &[String],
    instance_count: usize,
    depth: usize,
) -> Result<Value> {
    if depth > MAX_SETTINGS_VALUE_DEPTH {
        bail!("Settings bytecode value nesting exceeds safe depth of {MAX_SETTINGS_VALUE_DEPTH}");
    }
    let kind = reader.read_u8().context("Missing value kind")?;
    decode_raw_value_payload(reader, kind, strings, instance_count, depth)
}

fn decode_raw_value_payload(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    strings: &[String],
    instance_count: usize,
    depth: usize,
) -> Result<Value> {
    match kind {
        0 => Ok(Value::Null),
        1 => Ok(Value::Bool(false)),
        2 => Ok(Value::Bool(true)),
        3 => Ok(Value::Number(Number::from(unzigzag_i64(
            reader.read_var_u64()?,
        )))),
        4 => Ok(Value::Number(Number::from(reader.read_var_u64()?))),
        5 => Ok(json_number_f64(read_f64(reader)?)),
        6 => Ok(Value::String(
            reader.read_string(strings, "value string id")?.to_string(),
        )),
        7 => {
            let len = reader.read_collection_len("array length")?;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(decode_raw_value_at_depth(
                    reader,
                    strings,
                    instance_count,
                    depth + 1,
                )?);
            }
            Ok(Value::Array(items))
        }
        8 => decode_object_payload(reader, strings, instance_count, depth),
        9 => {
            let len = reader.read_collection_len("numeric array length")?;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(json_number_f64(read_f64(reader)?));
            }
            Ok(Value::Array(items))
        }
        20 => Ok(json_number_f64(read_f32(reader)? as f64)),
        21 => {
            let len = reader.read_collection_len("f32 numeric array length")?;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(json_number_f64(read_f32(reader)? as f64));
            }
            Ok(Value::Array(items))
        }
        10 => decode_ref_fallback_payload(reader, strings),
        11 => decode_fixed_numeric_payload(reader, "Vector2", &["x", "y"]),
        12 => decode_fixed_numeric_payload(reader, "Vector3", &["x", "y", "z"]),
        13 => decode_fixed_numeric_payload(reader, "UDim", &["scale", "offset"]),
        14 => decode_fixed_numeric_payload(
            reader,
            "UDim2",
            &["xScale", "xOffset", "yScale", "yOffset"],
        ),
        15 => decode_fixed_numeric_payload(reader, "Color3", &["r", "g", "b"]),
        16 => decode_cframe_payload(reader),
        17 => {
            let target_index = reader.read_len("resolved ref index")?;
            if target_index >= instance_count {
                bail!("Invalid resolved ref index {target_index}");
            }
            Ok(typed_object(
                "Ref",
                [(
                    "instanceIndex",
                    Value::Number(Number::from(target_index + 1)),
                )],
            ))
        }
        18 => decode_fixed_numeric_payload(reader, "Rect", &["minX", "minY", "maxX", "maxY"]),
        19 => Ok(typed_object(
            "EnumItem",
            [(
                "name",
                Value::String(
                    reader
                        .read_string(strings, "enum item string id")?
                        .to_string(),
                ),
            )],
        )),
        _ => bail!("Unknown settings bytecode value kind {kind}"),
    }
}

fn decode_object_payload(
    reader: &mut BytecodeReader<'_>,
    strings: &[String],
    instance_count: usize,
    depth: usize,
) -> Result<Value> {
    let field_count = reader.read_collection_len("object field count")?;
    let mut out = Map::with_capacity(field_count);
    for _ in 0..field_count {
        let key = reader
            .read_string(strings, "object key string id")?
            .to_string();
        let value = decode_raw_value_at_depth(reader, strings, instance_count, depth + 1)?;
        out.insert(key, value);
    }
    Ok(Value::Object(out))
}

fn decode_attributes_payload(
    reader: &mut BytecodeReader<'_>,
    strings: &[String],
    instance_count: usize,
) -> Result<Map<String, Value>> {
    let field_count = reader.read_collection_len("attribute count")?;
    let mut out = Map::with_capacity(field_count);
    for _ in 0..field_count {
        let key = reader
            .read_string(strings, "attribute key string id")?
            .to_string();
        let value = decode_raw_value(reader, strings, instance_count)?;
        out.insert(key, unwrap_attribute_value(value));
    }
    Ok(out)
}

fn unwrap_attribute_value(value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    if object.len() != 1 {
        return Value::Object(object);
    }
    if let Some(value) = object.remove("Bool") {
        return value;
    }
    if let Some(value) = object.remove("Float64") {
        return value;
    }
    if let Some(value) = object.remove("String") {
        return value;
    }
    if let Some(value) = object.remove("EnumItem") {
        return normalize_enum_attribute_value(value);
    }
    for key in [
        "Vector2",
        "Vector3",
        "Color3",
        "UDim",
        "UDim2",
        "CFrame",
        "Rect",
        "Enum",
        "Font",
        "ColorSequence",
        "NumberSequence",
        "BrickColor",
        "NumberRange",
        "BinaryString",
    ] {
        if let Some(value) = object.remove(key) {
            return value;
        }
    }
    Value::Object(object)
}

fn decode_ref_fallback_payload(
    reader: &mut BytecodeReader<'_>,
    strings: &[String],
) -> Result<Value> {
    let flags = reader.read_var_u64().context("Missing ref flags")?;
    let mut out = Map::new();
    out.insert("_type".to_string(), Value::String("Ref".to_string()));
    if flags & 1 != 0 {
        out.insert(
            "instanceId".to_string(),
            Value::String(reader.read_string(strings, "ref instance id")?.to_string()),
        );
    }
    if flags & 2 != 0 {
        out.insert(
            "debugId".to_string(),
            Value::String(reader.read_string(strings, "ref debug id")?.to_string()),
        );
    }
    if flags & 4 != 0 {
        let len = reader.read_collection_len("ref path segment count")?;
        let mut segments = Vec::with_capacity(len);
        for _ in 0..len {
            segments.push(Value::String(
                reader.read_string(strings, "ref path segment")?.to_string(),
            ));
        }
        out.insert("pathSegments".to_string(), Value::Array(segments));
    }
    if flags & 8 != 0 {
        out.insert(
            "path".to_string(),
            Value::String(reader.read_string(strings, "ref path")?.to_string()),
        );
    }
    if flags & 16 != 0 {
        out.insert(
            "settingsId".to_string(),
            Value::String(reader.read_string(strings, "ref settings id")?.to_string()),
        );
    }
    if flags & 32 != 0 {
        let len = reader.read_collection_len("ref path ordinal count")?;
        let mut ordinals = Vec::with_capacity(len);
        for _ in 0..len {
            ordinals.push(Value::Number(Number::from(
                reader.read_var_u64().context("Missing ref path ordinal")?,
            )));
        }
        out.insert("pathOrdinals".to_string(), Value::Array(ordinals));
    }
    Ok(Value::Object(out))
}

fn decode_fixed_numeric_payload(
    reader: &mut BytecodeReader<'_>,
    type_name: &'static str,
    fields: &[&'static str],
) -> Result<Value> {
    let mut out = Map::with_capacity(fields.len() + 1);
    out.insert("_type".to_string(), Value::String(type_name.to_string()));
    for field in fields {
        out.insert(
            (*field).to_string(),
            json_number_f64(read_fixed_numeric_component(reader)?),
        );
    }
    Ok(Value::Object(out))
}

fn decode_cframe_payload(reader: &mut BytecodeReader<'_>) -> Result<Value> {
    let mut components = Vec::with_capacity(12);
    for _ in 0..12 {
        components.push(json_number_f64(read_fixed_numeric_component(reader)?));
    }
    Ok(typed_object(
        "CFrame",
        [("components", Value::Array(components))],
    ))
}

enum CFrameGroupDecoder {
    Inline,
    RotationTable(Vec<[f64; 9]>),
}

fn decode_cframe_group_header(
    kind: u8,
    reader: &mut BytecodeReader<'_>,
) -> Result<CFrameGroupDecoder> {
    if kind != 16 {
        return Ok(CFrameGroupDecoder::Inline);
    }

    let mode = reader.read_u8().context("Missing CFrame group codec")?;
    match mode {
        0 => Ok(CFrameGroupDecoder::Inline),
        1 => {
            let rotation_count = reader.read_collection_len("CFrame rotation table count")?;
            let mut rotations = Vec::with_capacity(rotation_count);
            for _ in 0..rotation_count {
                let mut rotation = [0.0_f64; 9];
                for component in &mut rotation {
                    *component = read_f32(reader)? as f64;
                }
                rotations.push(rotation);
            }
            Ok(CFrameGroupDecoder::RotationTable(rotations))
        }
        _ => bail!("Unknown CFrame group codec {mode}"),
    }
}

fn decode_cframe_rotation_table_payload(
    reader: &mut BytecodeReader<'_>,
    rotations: &[[f64; 9]],
) -> Result<Value> {
    let mut components = Vec::with_capacity(12);
    for _ in 0..3 {
        components.push(json_number_f64(read_f32(reader)? as f64));
    }
    let rotation_index = reader.read_len("CFrame rotation index")?;
    let rotation = rotations
        .get(rotation_index)
        .with_context(|| format!("Invalid CFrame rotation index {rotation_index}"))?;
    for component in rotation {
        components.push(json_number_f64(*component));
    }
    Ok(typed_object(
        "CFrame",
        [("components", Value::Array(components))],
    ))
}

fn decode_resolved_ref_group_payload(
    reader: &mut BytecodeReader<'_>,
    instance_count: usize,
    previous_target_index: &mut usize,
) -> Result<Value> {
    let raw_delta = reader
        .read_var_u64()
        .context("Missing resolved ref target index delta")?;
    let previous =
        i64::try_from(*previous_target_index).context("Ref target index does not fit in i64")?;
    let target = previous
        .checked_add(unzigzag_i64(raw_delta))
        .context("Resolved ref target index delta overflow")?;
    if target < 0 {
        bail!("Invalid resolved ref target index {target}");
    }
    let target_index =
        usize::try_from(target).context("Resolved ref target index does not fit in usize")?;
    if target_index >= instance_count {
        bail!("Invalid resolved ref index {target_index}");
    }
    *previous_target_index = target_index;
    Ok(typed_object(
        "Ref",
        [(
            "instanceIndex",
            Value::Number(Number::from(target_index + 1)),
        )],
    ))
}

fn typed_object<const N: usize>(
    type_name: &'static str,
    fields: [(&'static str, Value); N],
) -> Value {
    let mut out = Map::with_capacity(N + 1);
    out.insert("_type".to_string(), Value::String(type_name.to_string()));
    for (key, value) in fields {
        out.insert(key.to_string(), value);
    }
    Value::Object(out)
}

fn read_f64(reader: &mut BytecodeReader<'_>) -> Result<f64> {
    let bytes = reader.read_bytes(8)?;
    let mut raw = [0_u8; 8];
    raw.copy_from_slice(bytes);
    Ok(f64::from_le_bytes(raw))
}

fn read_f32(reader: &mut BytecodeReader<'_>) -> Result<f32> {
    let bytes = reader.read_bytes(4)?;
    let mut raw = [0_u8; 4];
    raw.copy_from_slice(bytes);
    Ok(f32::from_le_bytes(raw))
}

fn read_fixed_numeric_component(reader: &mut BytecodeReader<'_>) -> Result<f64> {
    Ok(read_f32(reader)? as f64)
}

fn unzigzag_i64(value: u64) -> i64 {
    ((value >> 1) as i64) ^ (-((value & 1) as i64))
}

fn decode_parent_index(
    raw: usize,
    instance_index: usize,
    instance_count: usize,
) -> Result<Option<usize>> {
    if raw == 0 {
        return Ok(None);
    }
    let current = i64::try_from(instance_index).context("Instance index does not fit in i64")?;
    let delta = unzigzag_i64((raw - 1) as u64);
    let parent = current
        .checked_add(delta)
        .context("Parent index delta overflow")?;
    if parent < 0 {
        bail!("Invalid parent index {parent}");
    }
    let index = usize::try_from(parent).context("Parent index does not fit in usize")?;
    if index >= instance_count {
        bail!("Invalid parent index {index}");
    }
    Ok(Some(index))
}

type SettingsString<'a> = Cow<'a, str>;
type SettingsStringCounts<'a> = HashMap<SettingsString<'a>, u64>;
type SettingsPropertyGroups<'a> = HashMap<(&'a str, u8), Vec<SettingsBinaryValue<'a>>>;
type SettingsStringIdMap<'a> = HashMap<&'a str, u64>;
type SettingsIndexMap<K> = HashMap<K, usize>;

struct SettingsBinaryInstance<'a> {
    source_index: usize,
    settings_id: SettingsBinaryId<'a>,
    name: &'a str,
    class_name: &'a str,
    parent_index: Option<usize>,
}

enum SettingsBinaryId<'a> {
    Text(SettingsString<'a>),
    NumericDebug(u64),
}

struct SettingsBinaryValue<'a> {
    instance_index: usize,
    source: SettingsBinaryValueSource<'a>,
}

struct CFrameRotationTable {
    rotations: Vec<[u32; 9]>,
    values: Vec<CFrameTableValue>,
}

struct CFrameTableValue {
    position: [u32; 3],
    rotation_index: usize,
}

#[derive(Default)]
struct SettingsBinaryCollection<'a> {
    string_counts: SettingsStringCounts<'a>,
    class_counts: SettingsStringCounts<'a>,
    property_counts: SettingsStringCounts<'a>,
    property_groups: SettingsPropertyGroups<'a>,
}

enum SettingsBinaryValueSource<'a> {
    Property(&'a Value),
    Native(&'a NativeSettingsValue),
    Attributes(&'a Map<String, Value>),
}

#[derive(Default)]
struct SettingsBinaryInstanceLookup {
    dense_instance_count: usize,
    by_instance_index: SettingsIndexMap<usize>,
    by_settings_id: SettingsIndexMap<String>,
    by_instance_id: SettingsIndexMap<String>,
    by_debug_id: SettingsIndexMap<String>,
    by_path: HashMap<String, Option<usize>>,
    by_path_segments: HashMap<String, Option<usize>>,
    by_path_parts: HashMap<String, Option<usize>>,
}

const SETTINGS_BINARY_PARALLEL_MIN_INSTANCES: usize = 2_048;
const SETTINGS_BINARY_PARALLEL_CHUNK_SIZE: usize = 4096;

#[derive(Clone, Copy)]
enum FixedNumericKind {
    Vector2,
    Vector3,
    UDim,
    UDim2,
    Color3,
    CFrame,
    Rect,
}

fn push_settings_binary_value<'a>(
    out: &mut SettingsBinaryCollection<'a>,
    lookup: &SettingsBinaryInstanceLookup,
    instance_index: usize,
    property_name: &'a str,
    kind: u8,
    source: SettingsBinaryValueSource<'a>,
) -> Result<()> {
    add_count(&mut out.string_counts, property_name, 1);
    add_count(&mut out.property_counts, property_name, 1);
    collect_binary_source_strings(&source, lookup, &mut out.string_counts)?;
    out.property_groups
        .entry((property_name, kind))
        .or_default()
        .push(SettingsBinaryValue {
            instance_index,
            source,
        });
    Ok(())
}

fn collect_settings_binary_chunk<'a>(
    state: &'a ServiceState,
    lookup: &'a SettingsBinaryInstanceLookup,
    instances: &'a [SettingsBinaryInstance<'a>],
    base_index: usize,
) -> Result<SettingsBinaryCollection<'a>> {
    let mut out = SettingsBinaryCollection::default();

    for (offset, record) in instances.iter().enumerate() {
        let instance_index = base_index + offset;
        if let SettingsBinaryId::Text(settings_id) = &record.settings_id {
            add_count(&mut out.string_counts, settings_id.as_ref(), 1);
        }
        add_count(&mut out.string_counts, record.name, 1);
        add_count(&mut out.string_counts, record.class_name, 1);
        add_count(&mut out.class_counts, record.class_name, 1);

        let instance = &state.instances[record.source_index];
        if let Some(native_properties) = state
            .native_properties_by_instance
            .as_ref()
            .and_then(|values| values.get(record.source_index))
        {
            for property in native_properties {
                if instance.properties.contains_key(property.name.as_str())
                    || property.name == "RunContext" && instance.class_name != "Script"
                {
                    continue;
                }
                let property_name = property.name.as_str();
                let source = SettingsBinaryValueSource::Native(&property.value);
                let kind = binary_source_value_kind(&source, lookup)?;
                push_settings_binary_value(
                    &mut out,
                    lookup,
                    instance_index,
                    property_name,
                    kind,
                    source,
                )?;
            }
        }
        for (property_name, raw_value) in &instance.properties {
            if should_skip_binary_property(state, instance, property_name, raw_value) {
                continue;
            }
            let property_name = property_name.as_str();
            let source = SettingsBinaryValueSource::Property(raw_value);
            let kind = binary_source_value_kind(&source, lookup)?;
            if kind == 0 {
                continue;
            }
            push_settings_binary_value(
                &mut out,
                lookup,
                instance_index,
                property_name,
                kind,
                source,
            )?;
        }

        if has_binary_attributes(&instance.attributes) {
            let property_name = "Attributes";
            let source = SettingsBinaryValueSource::Attributes(&instance.attributes);
            let kind = binary_source_value_kind(&source, lookup)?;
            push_settings_binary_value(
                &mut out,
                lookup,
                instance_index,
                property_name,
                kind,
                source,
            )?;
        }
    }

    Ok(out)
}

fn merge_settings_binary_collection<'a>(
    target: &mut SettingsBinaryCollection<'a>,
    source: SettingsBinaryCollection<'a>,
) {
    for (text, count) in source.string_counts {
        add_count_key(&mut target.string_counts, text, count);
    }
    for (text, count) in source.class_counts {
        add_count_key(&mut target.class_counts, text, count);
    }
    for (text, count) in source.property_counts {
        add_count_key(&mut target.property_counts, text, count);
    }
    for (key, mut values) in source.property_groups {
        target
            .property_groups
            .entry(key)
            .or_default()
            .append(&mut values);
    }
}

fn collect_settings_binary_data<'a>(
    state: &'a ServiceState,
    lookup: &'a SettingsBinaryInstanceLookup,
    instances: &'a [SettingsBinaryInstance<'a>],
) -> Result<SettingsBinaryCollection<'a>> {
    if instances.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES || rayon::current_num_threads() <= 1
    {
        return collect_settings_binary_chunk(state, lookup, instances, 0);
    }

    let partials = instances
        .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
        .enumerate()
        .map(|(chunk_index, records)| {
            collect_settings_binary_chunk(
                state,
                lookup,
                records,
                chunk_index * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let mut out = SettingsBinaryCollection::default();
    for partial in partials {
        merge_settings_binary_collection(&mut out, partial);
    }
    Ok(out)
}

pub(crate) fn write_service_settings_binary_file(path: &Path, state: &ServiceState) -> Result<()> {
    write_service_settings_binary_file_inner(path, state, false)
}

pub(crate) fn write_fresh_service_settings_binary_file(
    path: &Path,
    state: &ServiceState,
) -> Result<()> {
    write_service_settings_binary_file_inner(path, state, true)
}

fn write_service_settings_binary_file_inner(
    path: &Path,
    state: &ServiceState,
    fresh: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let collect_started = Instant::now();
    let instances = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &instances);
    let collected = collect_settings_binary_data(state, &lookup, &instances)?;
    log_timing(
        &format!("settings binary collect {}", path.display()),
        collect_started,
    );

    let write_started = Instant::now();
    let strings = sorted_counted_strings(collected.string_counts);
    let string_ids = build_id_map(&strings);
    let classes = sorted_counted_strings(collected.class_counts);
    let class_ids = build_id_map(&classes);
    let properties = sorted_counted_strings(collected.property_counts);
    let property_ids = build_id_map(&properties);
    let property_group_entries =
        sorted_settings_property_groups(collected.property_groups, &property_ids);
    let estimated_capacity = instances
        .len()
        .saturating_mul(96)
        .clamp(1024 * 1024, 32 * 1024 * 1024);
    let mut payload = Vec::with_capacity(estimated_capacity);
    write_settings_binary_header(
        &mut payload,
        &strings,
        &string_ids,
        &classes,
        &properties,
        instances.len(),
    )?;
    if instances.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES && rayon::current_num_threads() > 1
    {
        let bodies = instances
            .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
            .enumerate()
            .map(|(chunk_index, records)| {
                let base_index = chunk_index * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE;
                let mut body = Vec::with_capacity(records.len().saturating_mul(12));
                for (offset, record) in records.iter().enumerate() {
                    write_settings_binary_id(&mut body, &string_ids, &record.settings_id)?;
                    write_binary_string_id(&mut body, &string_ids, record.name)?;
                    write_lookup_id(&mut body, &class_ids, record.class_name, "class")?;
                    write_var_u64(
                        &mut body,
                        encode_parent_index(record.parent_index, base_index + offset)?,
                    )?;
                }
                Ok(body)
            })
            .collect::<Result<Vec<_>>>()?;
        for body in bodies {
            payload.extend_from_slice(&body);
        }
    } else {
        for (instance_index, record) in instances.iter().enumerate() {
            write_settings_binary_id(&mut payload, &string_ids, &record.settings_id)?;
            write_binary_string_id(&mut payload, &string_ids, record.name)?;
            write_lookup_id(&mut payload, &class_ids, record.class_name, "class")?;
            write_var_u64(
                &mut payload,
                encode_parent_index(record.parent_index, instance_index)?,
            )?;
        }
    }
    write_settings_binary_property_groups(
        &mut payload,
        &property_group_entries,
        &property_ids,
        &string_ids,
        &lookup,
    )?;

    let writer = wrap_settings_bytecode_payload(&payload)?;
    if fresh {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
        file.write_all(&writer)
            .with_context(|| format!("Failed to write {}", path.display()))?;
    } else {
        write_bytes_if_changed(path, &writer)?;
    }
    log_timing(
        &format!("settings binary write {}", path.display()),
        write_started,
    );
    Ok(())
}

fn collect_service_settings_binary_instances(
    state: &ServiceState,
) -> Vec<SettingsBinaryInstance<'_>> {
    if state.properties_default_elided && state.dense_index_topology {
        let mut out = Vec::with_capacity(state.instances.len());
        for (source_index, instance) in state.instances.iter().enumerate() {
            out.push(SettingsBinaryInstance {
                source_index,
                settings_id: settings_binary_id(source_index, instance),
                name: instance.name.as_str(),
                class_name: instance.class_name.as_str(),
                parent_index: instance.parent_index.map(|parent_index| parent_index - 1),
            });
        }
        return out;
    }

    let mut out = Vec::with_capacity(state.instances.len());
    let mut visited = HashSet::with_capacity(state.instances.len());
    let mut stack = vec![(state.service_root_index, None)];
    while let Some((source_index, parent_index)) = stack.pop() {
        if source_index >= state.instances.len() || !visited.insert(source_index) {
            continue;
        }

        let instance = &state.instances[source_index];
        let current_index = out.len();
        out.push(SettingsBinaryInstance {
            source_index,
            settings_id: settings_binary_id(source_index, instance),
            name: instance.name.as_str(),
            class_name: instance.class_name.as_str(),
            parent_index,
        });

        for &child_index in child_indices_for_instance(state, source_index).iter().rev() {
            stack.push((child_index, Some(current_index)));
        }
    }
    out
}

fn parse_numeric_debug_id(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("0_")?;
    if digits.is_empty() || digits.len() > 1 && digits.as_bytes()[0] == b'0' {
        return None;
    }
    let mut value = 0u64;
    for digit in digits.bytes() {
        let digit = digit.checked_sub(b'0').filter(|digit| *digit < 10)?;
        value = value.checked_mul(10)?.checked_add(u64::from(digit))?;
    }
    (value <= (u64::MAX >> 1)).then_some(value)
}

fn parse_numeric_debug_settings_id(text: &str) -> Option<u64> {
    parse_numeric_debug_id(text.strip_prefix("debug:")?)
}

fn settings_binary_id(source_index: usize, instance: &SnapshotInstance) -> SettingsBinaryId<'_> {
    if instance.instance_index == Some(1) && instance.parent_index.is_none() {
        return SettingsBinaryId::Text(Cow::Borrowed("1"));
    }
    if let Some(debug_id) = instance
        .debug_id
        .as_deref()
        .filter(|value| !value.is_empty())
        && let Some(value) = parse_numeric_debug_id(debug_id)
    {
        return SettingsBinaryId::NumericDebug(value);
    }
    SettingsBinaryId::Text(Cow::Owned(instance_settings_id(source_index, instance)))
}

fn build_settings_binary_instance_lookup(
    state: &ServiceState,
    instances: &[SettingsBinaryInstance<'_>],
) -> SettingsBinaryInstanceLookup {
    if state.dense_index_topology {
        return SettingsBinaryInstanceLookup {
            dense_instance_count: instances.len(),
            ..Default::default()
        };
    }
    let mut by_instance_index = HashMap::with_capacity(instances.len());
    let mut by_settings_id = if state.properties_default_elided {
        HashMap::new()
    } else {
        HashMap::with_capacity(instances.len())
    };
    let mut by_instance_id = if state.properties_default_elided {
        HashMap::new()
    } else {
        HashMap::with_capacity(instances.len())
    };
    let mut by_debug_id = if state.properties_default_elided {
        HashMap::new()
    } else {
        HashMap::with_capacity(instances.len() / 4)
    };
    let mut by_path: HashMap<String, Option<usize>> = if state.properties_default_elided {
        HashMap::new()
    } else {
        HashMap::with_capacity(instances.len())
    };
    let mut by_path_segments: HashMap<String, Option<usize>> = if state.properties_default_elided {
        HashMap::new()
    } else {
        HashMap::with_capacity(instances.len())
    };
    let mut by_path_parts: HashMap<String, Option<usize>> = if state.properties_default_elided {
        HashMap::new()
    } else {
        HashMap::with_capacity(instances.len())
    };
    let path_parts = settings_binary_path_parts(instances);

    for (binary_index, record) in instances.iter().enumerate() {
        let instance = &state.instances[record.source_index];
        by_settings_id
            .entry(instance_settings_id(record.source_index, instance))
            .or_insert(binary_index);
        if let Some(instance_index) = instance.instance_index.filter(|value| *value > 0) {
            by_instance_index
                .entry(instance_index)
                .or_insert(binary_index);
            if state.properties_default_elided {
                continue;
            }
            by_instance_id
                .entry(format!("{instance_index:x}"))
                .or_insert(binary_index);
        } else if let Some(instance_id) = instance.instance_id.as_deref().filter(|s| !s.is_empty())
        {
            by_instance_id
                .entry(instance_id.to_string())
                .or_insert(binary_index);
        }
        if let Some(debug_id) = instance.debug_id.as_deref().filter(|s| !s.is_empty()) {
            by_debug_id
                .entry(debug_id.to_string())
                .or_insert(binary_index);
        }
        if !instance.path.is_empty() {
            insert_unique_path(&mut by_path, instance.path.clone(), binary_index);
        }
        if let Some((segments, ordinals)) = path_parts.get(binary_index) {
            insert_unique_path(
                &mut by_path_segments,
                path_segments_key(segments),
                binary_index,
            );
            insert_unique_path(
                &mut by_path_parts,
                path_parts_key(segments, ordinals),
                binary_index,
            );
        }
    }

    SettingsBinaryInstanceLookup {
        dense_instance_count: 0,
        by_instance_index,
        by_settings_id,
        by_instance_id,
        by_debug_id,
        by_path,
        by_path_segments,
        by_path_parts,
    }
}

fn path_segments_key(segments: &[String]) -> String {
    let mut key = String::new();
    for segment in segments {
        key.push_str(&segment.len().to_string());
        key.push(':');
        key.push_str(segment);
        key.push('|');
    }
    key
}

fn path_parts_key(segments: &[String], ordinals: &[usize]) -> String {
    let mut key = path_segments_key(segments);
    key.push('#');
    for ordinal in ordinals {
        key.push_str(&ordinal.to_string());
        key.push('|');
    }
    key
}

fn insert_unique_path(map: &mut HashMap<String, Option<usize>>, key: String, index: usize) {
    match map.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Some(index));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().is_some_and(|existing| existing != index) {
                entry.insert(None);
            }
        }
    }
}

fn settings_document_path_parts(document: &SettingsBytecode) -> Vec<(Vec<String>, Vec<usize>)> {
    let names = document
        .instances
        .iter()
        .map(|instance| instance.name.as_str())
        .collect::<Vec<_>>();
    let parents = document
        .instances
        .iter()
        .map(|instance| instance.parent_index)
        .collect::<Vec<_>>();
    settings_path_parts(&names, &parents)
}

fn settings_binary_path_parts(
    instances: &[SettingsBinaryInstance<'_>],
) -> Vec<(Vec<String>, Vec<usize>)> {
    let names = instances
        .iter()
        .map(|instance| instance.name)
        .collect::<Vec<_>>();
    let parents = instances
        .iter()
        .map(|instance| instance.parent_index)
        .collect::<Vec<_>>();
    settings_path_parts(&names, &parents)
}

fn settings_path_parts(
    names: &[&str],
    parents: &[Option<usize>],
) -> Vec<(Vec<String>, Vec<usize>)> {
    let mut sibling_counts = HashMap::<(Option<usize>, &str), usize>::new();
    let mut ordinal_by_index = vec![1; names.len()];
    for (index, name) in names.iter().copied().enumerate() {
        let count = sibling_counts.entry((parents[index], name)).or_insert(0);
        *count += 1;
        ordinal_by_index[index] = *count;
    }
    let mut output = Vec::with_capacity(names.len());
    for index in 0..names.len() {
        let mut segments = Vec::new();
        let mut ordinals = Vec::new();
        let mut current = Some(index);
        let mut seen = HashSet::new();
        while let Some(value) = current {
            if value >= names.len() || !seen.insert(value) {
                break;
            }
            segments.push(names[value].to_string());
            ordinals.push(ordinal_by_index[value]);
            current = parents[value];
        }
        segments.reverse();
        ordinals.reverse();
        output.push((segments, ordinals));
    }
    output
}

fn add_count<'a>(counts: &mut SettingsStringCounts<'a>, text: &'a str, amount: u64) {
    if let Some(count) = counts.get_mut(text) {
        *count += amount;
    } else {
        counts.insert(Cow::Borrowed(text), amount);
    }
}

fn add_count_key<'a>(counts: &mut SettingsStringCounts<'a>, text: SettingsString<'a>, amount: u64) {
    if let Some(count) = counts.get_mut(text.as_ref()) {
        *count += amount;
    } else {
        counts.insert(text, amount);
    }
}

fn sorted_counted_strings<'a>(counts: SettingsStringCounts<'a>) -> Vec<SettingsString<'a>> {
    let mut items: Vec<(SettingsString<'a>, u64)> = counts.into_iter().collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.as_ref().cmp(b.0.as_ref())));
    items.into_iter().map(|(text, _)| text).collect()
}

fn build_id_map<'a>(items: &'a [SettingsString<'_>]) -> SettingsStringIdMap<'a> {
    let mut out = HashMap::with_capacity(items.len());
    for (index, text) in items.iter().enumerate() {
        out.insert(text.as_ref(), index as u64);
    }
    out
}

fn sorted_settings_property_groups<'a>(
    groups: SettingsPropertyGroups<'a>,
    property_ids: &SettingsStringIdMap<'_>,
) -> Vec<((&'a str, u8), Vec<SettingsBinaryValue<'a>>)> {
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(|a, b| {
        let a_id = property_ids.get(a.0.0).copied().unwrap_or(u64::MAX);
        let b_id = property_ids.get(b.0.0).copied().unwrap_or(u64::MAX);
        a_id.cmp(&b_id).then_with(|| a.0.1.cmp(&b.0.1))
    });
    groups
}

fn write_settings_binary_header<W: Write + ?Sized>(
    writer: &mut W,
    strings: &[SettingsString<'_>],
    string_ids: &SettingsStringIdMap<'_>,
    classes: &[SettingsString<'_>],
    properties: &[SettingsString<'_>],
    instance_count: usize,
) -> Result<()> {
    write_var_u64(writer, strings.len() as u64)?;
    for text in strings {
        let bytes = text.as_bytes();
        write_var_u64(writer, bytes.len() as u64)?;
        writer.write_all(bytes)?;
    }
    write_var_u64(writer, classes.len() as u64)?;
    for class_name in classes {
        write_binary_string_id(writer, string_ids, class_name)?;
    }
    write_var_u64(writer, properties.len() as u64)?;
    for property_name in properties {
        write_binary_string_id(writer, string_ids, property_name)?;
    }
    write_var_u64(writer, instance_count as u64)
}

fn write_settings_binary_property_groups<W: Write + ?Sized>(
    writer: &mut W,
    groups: &[((&str, u8), Vec<SettingsBinaryValue<'_>>)],
    property_ids: &SettingsStringIdMap<'_>,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
) -> Result<()> {
    write_var_u64(writer, groups.len() as u64)?;
    let bodies = groups
        .par_iter()
        .map(|((_, kind), values)| {
            let mut body = Vec::new();
            write_property_group_values(values, *kind, string_ids, lookup, &mut body)?;
            Ok(body)
        })
        .collect::<Result<Vec<_>>>()?;
    for (((property_name, kind), values), body) in groups.iter().zip(bodies) {
        write_lookup_id(writer, property_ids, property_name, "property")?;
        writer.write_all(&[*kind])?;
        write_var_u64(writer, values.len() as u64)?;
        write_var_u64(writer, body.len() as u64)?;
        writer.write_all(&body)?;
    }
    Ok(())
}

fn has_binary_attributes(attributes: &Map<String, Value>) -> bool {
    !attributes.is_empty()
}

fn binary_source_value_kind(
    source: &SettingsBinaryValueSource<'_>,
    lookup: &SettingsBinaryInstanceLookup,
) -> Result<u8> {
    Ok(match source {
        SettingsBinaryValueSource::Property(value) => binary_raw_value_kind(value, lookup)?,
        SettingsBinaryValueSource::Native(value) => native_value_kind(value),
        SettingsBinaryValueSource::Attributes(_) => 8,
    })
}

fn collect_binary_source_strings<'a>(
    source: &SettingsBinaryValueSource<'a>,
    lookup: &SettingsBinaryInstanceLookup,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    match source {
        SettingsBinaryValueSource::Property(value) => collect_raw_value_strings(value, lookup, out),
        SettingsBinaryValueSource::Native(value) => {
            collect_native_value_strings(value, out);
            Ok(())
        }
        SettingsBinaryValueSource::Attributes(attributes) => {
            for (name, value) in *attributes {
                add_count(out, name, 1);
                collect_attribute_value_strings(value, out)
                    .with_context(|| format!("Could not collect attribute {name}"))?;
            }
            Ok(())
        }
    }
}

fn write_binary_source_payload<W: Write + ?Sized>(
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    match source {
        SettingsBinaryValueSource::Property(value) => {
            write_raw_value_payload(value, kind, string_ids, lookup, writer)
        }
        SettingsBinaryValueSource::Native(value) => {
            write_native_value_payload(value, kind, string_ids, writer)
        }
        SettingsBinaryValueSource::Attributes(attributes) => {
            write_attributes_payload(attributes, string_ids, writer)
        }
    }
}

fn native_value_kind(value: &NativeSettingsValue) -> u8 {
    match value {
        NativeSettingsValue::Bool(false) => 1,
        NativeSettingsValue::Bool(true) => 2,
        NativeSettingsValue::Int(_) => 3,
        NativeSettingsValue::Float32(_) => 20,
        NativeSettingsValue::Float64(value) if exact_f32(*value).is_some() => 20,
        NativeSettingsValue::Float64(_) => 5,
        NativeSettingsValue::String(_) => 6,
        NativeSettingsValue::Ref(_) => 17,
        NativeSettingsValue::Vector2(_) => 11,
        NativeSettingsValue::Vector3(_) => 12,
        NativeSettingsValue::UDim(_) => 13,
        NativeSettingsValue::UDim2(_) => 14,
        NativeSettingsValue::Color3(_) => 15,
        NativeSettingsValue::CFrame(_) => 16,
        NativeSettingsValue::Rect(_) => 18,
        NativeSettingsValue::Enum(_) => 19,
    }
}

fn collect_native_value_strings<'a>(
    value: &'a NativeSettingsValue,
    out: &mut SettingsStringCounts<'a>,
) {
    if let NativeSettingsValue::String(value) | NativeSettingsValue::Enum(value) = value {
        add_count(out, value, 1);
    }
}

fn native_fixed_components(value: &NativeSettingsValue) -> Option<&[f32]> {
    match value {
        NativeSettingsValue::Vector2(value) | NativeSettingsValue::UDim(value) => Some(value),
        NativeSettingsValue::Vector3(value) | NativeSettingsValue::Color3(value) => Some(value),
        NativeSettingsValue::UDim2(value) | NativeSettingsValue::Rect(value) => Some(value),
        NativeSettingsValue::CFrame(value) => Some(value),
        _ => None,
    }
}

fn write_native_value_payload<W: Write + ?Sized>(
    value: &NativeSettingsValue,
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    match value {
        NativeSettingsValue::Bool(_) => {}
        NativeSettingsValue::Int(value) => write_var_u64(writer, zigzag_i64(*value))?,
        NativeSettingsValue::Float32(value) => writer.write_all(&value.to_le_bytes())?,
        NativeSettingsValue::Float64(value) if kind == 20 => {
            let value = exact_f32(*value).context("Expected exactly representable native f32")?;
            writer.write_all(&value.to_le_bytes())?;
        }
        NativeSettingsValue::Float64(value) => writer.write_all(&value.to_le_bytes())?,
        NativeSettingsValue::String(value) | NativeSettingsValue::Enum(value) => {
            write_binary_string_id(writer, string_ids, value)?;
        }
        NativeSettingsValue::Ref(index) => write_var_u64(writer, *index as u64)?,
        NativeSettingsValue::Vector2(_)
        | NativeSettingsValue::Vector3(_)
        | NativeSettingsValue::UDim(_)
        | NativeSettingsValue::UDim2(_)
        | NativeSettingsValue::Color3(_)
        | NativeSettingsValue::CFrame(_)
        | NativeSettingsValue::Rect(_) => {
            let values =
                native_fixed_components(value).context("Expected fixed native settings value")?;
            for value in values {
                writer.write_all(&value.to_le_bytes())?;
            }
        }
    }
    Ok(())
}

fn write_property_group_values<W: Write + ?Sized>(
    values: &[SettingsBinaryValue<'_>],
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    let mut previous_instance_index = 0_usize;
    if kind == 16 {
        if let Some(table) = build_cframe_rotation_table(values)? {
            writer.write_all(&[1])?;
            write_var_u64(writer, table.rotations.len() as u64)?;
            for rotation in &table.rotations {
                for component in rotation {
                    write_f32_bits(*component, writer)?;
                }
            }
            for (item, table_value) in values.iter().zip(table.values.iter()) {
                write_property_group_instance_index(
                    writer,
                    &mut previous_instance_index,
                    item.instance_index,
                )?;
                for component in table_value.position {
                    write_f32_bits(component, writer)?;
                }
                write_var_u64(writer, table_value.rotation_index as u64)?;
            }
            return Ok(());
        }
        writer.write_all(&[0])?;
    }

    if kind == 17 {
        let mut previous_target_index = 0_usize;
        for item in values {
            write_property_group_instance_index(
                writer,
                &mut previous_instance_index,
                item.instance_index,
            )?;
            write_resolved_ref_group_payload(
                &item.source,
                lookup,
                &mut previous_target_index,
                writer,
            )?;
        }
        return Ok(());
    }

    for item in values {
        write_property_group_instance_index(
            writer,
            &mut previous_instance_index,
            item.instance_index,
        )?;
        write_binary_source_payload(&item.source, kind, string_ids, lookup, writer)?;
    }
    Ok(())
}

fn write_resolved_ref_group_payload<W: Write + ?Sized>(
    source: &SettingsBinaryValueSource<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    previous_target_index: &mut usize,
    writer: &mut W,
) -> Result<()> {
    let value = match source {
        SettingsBinaryValueSource::Native(NativeSettingsValue::Ref(index)) => {
            let target = i64::try_from(*index).context("Ref target index does not fit in i64")?;
            let previous = i64::try_from(*previous_target_index)
                .context("Ref target index does not fit in i64")?;
            write_var_u64(writer, zigzag_i64(target - previous))?;
            *previous_target_index = *index;
            return Ok(());
        }
        SettingsBinaryValueSource::Native(_) | SettingsBinaryValueSource::Attributes(_) => {
            bail!("Expected resolved Ref property group")
        }
        SettingsBinaryValueSource::Property(value) => *value,
    };
    let ref_value = ref_payload_object(value).context("Expected Ref binary settings value")?;
    let target_index = resolve_ref_index(ref_value, lookup)?
        .context("Expected resolved Ref binary settings value")?;
    let target = i64::try_from(target_index).context("Ref target index does not fit in i64")?;
    let previous =
        i64::try_from(*previous_target_index).context("Ref target index does not fit in i64")?;
    write_var_u64(writer, zigzag_i64(target - previous))?;
    *previous_target_index = target_index;
    Ok(())
}

fn write_property_group_instance_index<W: Write + ?Sized>(
    writer: &mut W,
    previous_instance_index: &mut usize,
    instance_index: usize,
) -> Result<()> {
    if instance_index < *previous_instance_index {
        bail!("Property group instance indices must be sorted for delta encoding");
    }
    let delta = instance_index - *previous_instance_index;
    write_var_u64(writer, delta as u64)?;
    *previous_instance_index = instance_index;
    Ok(())
}

fn build_cframe_rotation_table(
    values: &[SettingsBinaryValue<'_>],
) -> Result<Option<CFrameRotationTable>> {
    let mut rotation_ids: HashMap<[u32; 9], usize> = HashMap::new();
    let mut rotations = Vec::new();
    let mut table_values = Vec::with_capacity(values.len());

    for item in values {
        let components = match &item.source {
            SettingsBinaryValueSource::Native(NativeSettingsValue::CFrame(value)) => {
                value.map(f32::to_bits)
            }
            SettingsBinaryValueSource::Native(_) | SettingsBinaryValueSource::Attributes(_) => {
                bail!("Expected CFrame property group")
            }
            SettingsBinaryValueSource::Property(value) => cframe_component_bits(value)?,
        };
        let position = [components[0], components[1], components[2]];
        let mut rotation = [0_u32; 9];
        rotation.copy_from_slice(&components[3..]);
        let rotation_index = *rotation_ids.entry(rotation).or_insert_with(|| {
            let index = rotations.len();
            rotations.push(rotation);
            index
        });
        table_values.push(CFrameTableValue {
            position,
            rotation_index,
        });
    }

    let inline_size = values.len() * fixed_numeric_len(FixedNumericKind::CFrame) * 4;
    let table_size = var_u64_len(rotations.len() as u64)
        + rotations.len() * 9 * 4
        + table_values
            .iter()
            .map(|value| 3 * 4 + var_u64_len(value.rotation_index as u64))
            .sum::<usize>();
    if table_size < inline_size {
        Ok(Some(CFrameRotationTable {
            rotations,
            values: table_values,
        }))
    } else {
        Ok(None)
    }
}

fn cframe_component_bits(value: &Value) -> Result<[u32; 12]> {
    let obj = value
        .as_object()
        .context("Expected CFrame binary settings value")?;
    let items = obj
        .get("components")
        .and_then(Value::as_array)
        .context("Expected CFrame components array")?;
    if items.len() != fixed_numeric_len(FixedNumericKind::CFrame) {
        bail!("Invalid CFrame component count");
    }

    let mut out = [0_u32; 12];
    for (index, item) in items.iter().enumerate() {
        let number = settings_number(item).context("Expected numeric CFrame component")?;
        out[index] = fixed_numeric_component_f32(number).to_bits();
    }
    Ok(out)
}

fn write_f32_bits<W: Write + ?Sized>(bits: u32, writer: &mut W) -> Result<()> {
    writer.write_all(&bits.to_le_bytes())?;
    Ok(())
}

fn binary_raw_value_kind(value: &Value, lookup: &SettingsBinaryInstanceLookup) -> Result<u8> {
    Ok(match value {
        Value::Null => 0,
        Value::Bool(false) => 1,
        Value::Bool(true) => 2,
        Value::Number(number) => binary_number_kind(number)?,
        Value::String(_) => 6,
        Value::Array(items) if is_numeric_array_f32_exact(items) => 21,
        Value::Array(items) if is_numeric_array(items) => 9,
        Value::Array(_) => 7,
        Value::Object(obj) => {
            if let Some(ref_value) = ref_payload_object(value) {
                return Ok(if resolve_ref_index(ref_value, lookup)?.is_some() {
                    17
                } else {
                    10
                });
            }
            obj.get("_type")
                .and_then(Value::as_str)
                .map_or(8, |type_name| match type_name {
                    "Vector2" => 11,
                    "Vector3" => 12,
                    "UDim" => 13,
                    "UDim2" => 14,
                    "Color3" => 15,
                    "CFrame" => 16,
                    "Rect" => 18,
                    "EnumItem" => 19,
                    _ => 8,
                })
        }
    })
}

fn binary_number_kind(number: &serde_json::Number) -> Result<u8> {
    if number.as_i64().is_some() {
        Ok(3)
    } else if number.as_u64().is_some() {
        Ok(4)
    } else if number.as_f64().and_then(exact_f32).is_some() {
        Ok(20)
    } else if number.as_f64().is_some() {
        Ok(5)
    } else {
        bail!("Unsupported JSON number in binary settings");
    }
}

fn is_numeric_array(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(|item| settings_number(item).is_some())
}

fn is_numeric_array_f32_exact(items: &[Value]) -> bool {
    !items.is_empty()
        && items
            .iter()
            .all(|item| settings_number(item).and_then(exact_f32).is_some())
}

fn exact_f32(value: f64) -> Option<f32> {
    let value32 = value as f32;
    (value32.is_finite() && value32 as f64 == value).then_some(value32)
}

fn settings_number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| nonfinite_float_from_json(value))
}

fn fixed_numeric_kind_from_tag(kind: u8) -> Option<FixedNumericKind> {
    match kind {
        11 => Some(FixedNumericKind::Vector2),
        12 => Some(FixedNumericKind::Vector3),
        13 => Some(FixedNumericKind::UDim),
        14 => Some(FixedNumericKind::UDim2),
        15 => Some(FixedNumericKind::Color3),
        16 => Some(FixedNumericKind::CFrame),
        18 => Some(FixedNumericKind::Rect),
        _ => None,
    }
}

fn fixed_numeric_len(kind: FixedNumericKind) -> usize {
    match kind {
        FixedNumericKind::Vector2 | FixedNumericKind::UDim => 2,
        FixedNumericKind::Vector3 | FixedNumericKind::Color3 => 3,
        FixedNumericKind::UDim2 | FixedNumericKind::Rect => 4,
        FixedNumericKind::CFrame => 12,
    }
}

fn write_named_numbers<W: Write + ?Sized>(
    obj: &Map<String, Value>,
    names: &[&str],
    writer: &mut W,
) -> Result<()> {
    for name in names {
        let number = obj
            .get(*name)
            .and_then(settings_number)
            .with_context(|| format!("Expected numeric field {name}"))?;
        write_fixed_numeric_component(number, writer)?;
    }
    Ok(())
}

fn write_fixed_numeric_component<W: Write + ?Sized>(number: f64, writer: &mut W) -> Result<()> {
    let value = fixed_numeric_component_f32(number);
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn fixed_numeric_component_f32(number: f64) -> f32 {
    if !number.is_finite() {
        return number as f32;
    }
    let value = number as f32;
    if value.is_finite() {
        value
    } else if value.is_sign_positive() {
        f32::MAX
    } else {
        f32::MIN
    }
}

fn write_fixed_numeric_components<W: Write + ?Sized>(
    value: &Value,
    kind: FixedNumericKind,
    writer: &mut W,
) -> Result<()> {
    let obj = value
        .as_object()
        .context("Expected fixed numeric binary settings value")?;
    match kind {
        FixedNumericKind::Vector2 => write_named_numbers(obj, &["x", "y"], writer)?,
        FixedNumericKind::Vector3 => write_named_numbers(obj, &["x", "y", "z"], writer)?,
        FixedNumericKind::UDim => write_named_numbers(obj, &["scale", "offset"], writer)?,
        FixedNumericKind::UDim2 => {
            write_named_numbers(obj, &["xScale", "xOffset", "yScale", "yOffset"], writer)?
        }
        FixedNumericKind::Color3 => write_named_numbers(obj, &["r", "g", "b"], writer)?,
        FixedNumericKind::CFrame => {
            let items = obj
                .get("components")
                .and_then(Value::as_array)
                .context("Expected CFrame components array")?;
            if items.len() != fixed_numeric_len(kind) {
                bail!("Invalid CFrame component count");
            }
            for item in items {
                let number = settings_number(item).context("Expected numeric CFrame component")?;
                write_fixed_numeric_component(number, writer)?;
            }
        }
        FixedNumericKind::Rect => {
            write_named_numbers(obj, &["minX", "minY", "maxX", "maxY"], writer)?
        }
    }
    Ok(())
}

fn write_numeric_array_components<W: Write + ?Sized>(
    value: &Value,
    writer: &mut W,
) -> Result<usize> {
    let items = value
        .as_array()
        .context("Expected numeric array binary settings value")?;
    if !is_numeric_array(items) {
        bail!("Expected numeric array binary settings value");
    }
    for item in items {
        let number =
            settings_number(item).context("Expected numeric array binary settings value")?;
        writer.write_all(&number.to_le_bytes())?;
    }
    Ok(items.len())
}

fn write_numeric_array_f32_components<W: Write + ?Sized>(
    value: &Value,
    writer: &mut W,
) -> Result<usize> {
    let items = value
        .as_array()
        .context("Expected f32 numeric array binary settings value")?;
    if !is_numeric_array_f32_exact(items) {
        bail!("Expected exactly representable f32 numeric array binary settings value");
    }
    for item in items {
        let number = settings_number(item)
            .and_then(exact_f32)
            .context("Expected exactly representable f32 numeric array item")?;
        writer.write_all(&number.to_le_bytes())?;
    }
    Ok(items.len())
}

fn write_numeric_slice_payload<W: Write + ?Sized>(numbers: &[f64], writer: &mut W) -> Result<()> {
    let is_f32_exact = numbers.iter().all(|number| exact_f32(*number).is_some());
    writer.write_all(&[if is_f32_exact { 21 } else { 9 }])?;
    write_var_u64(writer, numbers.len() as u64)?;
    if is_f32_exact {
        for number in numbers {
            let value = exact_f32(*number).context("Expected exactly representable f32 number")?;
            writer.write_all(&value.to_le_bytes())?;
        }
    } else {
        for number in numbers {
            writer.write_all(&number.to_le_bytes())?;
        }
    }
    Ok(())
}

fn color_components(value: &Value) -> Option<[f64; 3]> {
    let obj = value.as_object()?;
    Some([
        settings_number(obj.get("r")?)?,
        settings_number(obj.get("g")?)?,
        settings_number(obj.get("b")?)?,
    ])
}

fn ref_payload_object(value: &Value) -> Option<&Map<String, Value>> {
    let obj = value.as_object()?;
    if obj.get("_type").and_then(Value::as_str) == Some("Ref") {
        return Some(obj);
    }
    obj.get("Ref")?.as_object()
}

fn resolve_ref_index(
    ref_value: &Map<String, Value>,
    lookup: &SettingsBinaryInstanceLookup,
) -> Result<Option<usize>> {
    let mut resolved = None;
    let mut unresolved = false;
    let mut accept = |label: &str, candidate: usize| -> Result<()> {
        if let Some(existing) = resolved
            && existing != candidate
        {
            bail!("Ref selectors disagree at {label}");
        }
        resolved = Some(candidate);
        Ok(())
    };
    if let Some(raw_instance_index) = ref_value.get("instanceIndex") {
        let instance_index = raw_instance_index
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .context("Ref instanceIndex must be a positive integer")?;
        let mut index_candidate =
            (instance_index <= lookup.dense_instance_count).then_some(instance_index - 1);
        if let Some(index) = lookup.by_instance_index.get(&instance_index) {
            if index_candidate.is_some_and(|candidate| candidate != *index) {
                bail!("Ref instanceIndex mappings disagree");
            }
            index_candidate = Some(*index);
        }
        accept(
            "instanceIndex",
            index_candidate.context("Ref instanceIndex does not exist")?,
        )?;
    }
    let mut accept_stable = |label: &str, candidate: Option<usize>| -> Result<()> {
        if let Some(candidate) = candidate {
            accept(label, candidate)
        } else {
            unresolved = true;
            Ok(())
        }
    };
    if let Some(raw_settings_id) = ref_value.get("settingsId") {
        let settings_id = raw_settings_id
            .as_str()
            .context("Ref settingsId must be a string")?;
        accept_stable(
            "settingsId",
            lookup.by_settings_id.get(settings_id).copied(),
        )?;
    }
    if let Some(raw_instance_id) = ref_value.get("instanceId") {
        let instance_id = raw_instance_id
            .as_str()
            .context("Ref instanceId must be a string")?;
        accept_stable(
            "instanceId",
            lookup.by_instance_id.get(instance_id).copied(),
        )?;
    }
    for alias in ["referent", "ref"] {
        if let Some(raw_id) = ref_value.get(alias) {
            let id = raw_id
                .as_str()
                .with_context(|| format!("Ref {alias} must be a string"))?;
            accept_stable(alias, lookup.by_settings_id.get(id).copied())?;
        }
    }
    if let Some(raw_debug_id) = ref_value.get("debugId") {
        let debug_id = raw_debug_id
            .as_str()
            .context("Ref debugId must be a string")?;
        accept_stable("debugId", lookup.by_debug_id.get(debug_id).copied())?;
    }
    if let Some((segments, ordinals)) = strict_reference_path(ref_value)? {
        let candidate = if let Some(ordinals) = ordinals {
            lookup
                .by_path_parts
                .get(&path_parts_key(&segments, &ordinals))
                .copied()
                .flatten()
        } else {
            match lookup.by_path_segments.get(&path_segments_key(&segments)) {
                Some(None) => bail!("Ref pathSegments are ambiguous without pathOrdinals"),
                candidate => candidate.copied().flatten(),
            }
        };
        accept_stable("pathSegments", candidate)?;
    }
    if let Some(raw_path) = ref_value.get("path") {
        let path = raw_path.as_str().context("Ref path must be a string")?;
        if path.is_empty() {
            bail!("Ref path must not be empty");
        }
        accept_stable("path", None)?;
    }
    if unresolved && resolved.is_some() {
        bail!("Ref selectors mix local and nonlocal targets");
    }
    Ok(resolved)
}

fn collect_raw_value_strings<'a>(
    value: &'a Value,
    lookup: &SettingsBinaryInstanceLookup,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    match binary_raw_value_kind(value, lookup)? {
        0 | 1 | 2 | 3 | 4 | 5 | 9 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 20 | 21 => {}
        6 | 19 => {
            if let Some(text) = raw_string_payload(value) {
                add_count(out, text, 1);
            }
        }
        7 => {
            let items = value
                .as_array()
                .context("Expected array binary settings value")?;
            for item in items {
                collect_raw_value_strings(item, lookup, out)?;
            }
        }
        8 => collect_raw_object_strings(value, lookup, out)?,
        10 => {
            let ref_value =
                ref_payload_object(value).context("Expected Ref binary settings value")?;
            collect_ref_strings(ref_value, out);
        }
        kind => bail!("Unknown binary settings value kind {kind}"),
    }
    Ok(())
}

fn collect_raw_object_strings<'a>(
    value: &'a Value,
    lookup: &SettingsBinaryInstanceLookup,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    let obj = value
        .as_object()
        .context("Expected object binary settings value")?;
    if let Some(type_name) = obj.get("_type").and_then(Value::as_str) {
        match type_name {
            "BrickColor" => {
                add_count(out, "BrickColor", 1);
                collect_raw_value_strings(obj.get("number").unwrap_or(&Value::Null), lookup, out)?;
                return Ok(());
            }
            "ColorSequence" => {
                for key in ["ColorSequence", "keypoints", "time", "color"] {
                    add_count(out, key, 1);
                }
                for keypoint in sequence_keypoint_values(obj)
                    .iter()
                    .filter_map(Value::as_object)
                {
                    collect_raw_value_strings(
                        keypoint.get("time").unwrap_or(&Value::Null),
                        lookup,
                        out,
                    )?;
                }
                return Ok(());
            }
            "NumberSequence" => {
                for key in ["NumberSequence", "keypoints", "time", "value", "envelope"] {
                    add_count(out, key, 1);
                }
                for keypoint in sequence_keypoint_values(obj)
                    .iter()
                    .filter_map(Value::as_object)
                {
                    for key in ["time", "value", "envelope"] {
                        collect_raw_value_strings(
                            keypoint.get(key).unwrap_or(&Value::Null),
                            lookup,
                            out,
                        )?;
                    }
                }
                return Ok(());
            }
            "Font" => {
                collect_font_strings(obj, out);
                return Ok(());
            }
            _ => {}
        }
    }
    for (key, child) in obj {
        add_count(out, key, 1);
        collect_raw_value_strings(child, lookup, out)?;
    }
    Ok(())
}

fn collect_font_strings<'a>(obj: &'a Map<String, Value>, out: &mut SettingsStringCounts<'a>) {
    if let Some(family) = obj.get("family").and_then(Value::as_str) {
        add_count(out, "family", 1);
        add_count(out, family, 1);
    }
    if let Some(weight) = obj.get("weight").and_then(Value::as_str) {
        add_count(out, "weight", 1);
        add_count(out, split_enum_tail(weight), 1);
    }
    if let Some(style) = obj.get("style").and_then(Value::as_str) {
        add_count(out, "style", 1);
        add_count(out, split_enum_tail(style), 1);
    }
    if let Some(cached_face_id) = obj.get("cachedFaceId").and_then(Value::as_str) {
        add_count(out, "cachedFaceId", 1);
        add_count(out, cached_face_id, 1);
    }
}

fn collect_ref_strings<'a>(ref_value: &'a Map<String, Value>, out: &mut SettingsStringCounts<'a>) {
    if let Some(instance_index) = ref_value.get("instanceIndex").and_then(Value::as_u64) {
        let instance_id = format!("{instance_index:x}");
        add_count_key(out, Cow::Owned(instance_id), 1);
    }
    for key in ["settingsId", "instanceId", "debugId", "path"] {
        if let Some(text) = ref_value.get(key).and_then(Value::as_str) {
            add_count(out, text, 1);
        }
    }
    if let Some(path_segments) = ref_value.get("pathSegments").and_then(Value::as_array) {
        for segment in path_segments {
            if let Some(text) = segment.as_str() {
                add_count(out, text, 1);
            }
        }
    }
}

fn sequence_keypoint_values(obj: &Map<String, Value>) -> &[Value] {
    obj.get("keypoints")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

fn raw_string_payload(value: &Value) -> Option<&str> {
    if let Some(text) = value.as_str() {
        return Some(text);
    }
    let obj = value.as_object()?;
    match obj.get("_type").and_then(Value::as_str)? {
        "EnumItem" => Some(obj.get("name").and_then(Value::as_str).unwrap_or("")),
        _ => None,
    }
}

fn split_enum_tail(text: &str) -> &str {
    text.split('.').next_back().unwrap_or(text)
}

fn write_raw_value<W: Write + ?Sized>(
    value: &Value,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    let kind = binary_raw_value_kind(value, lookup)?;
    writer.write_all(&[kind])?;
    write_raw_value_payload(value, kind, string_ids, lookup, writer)
}

fn write_raw_value_payload<W: Write + ?Sized>(
    value: &Value,
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    match kind {
        0..=2 => {}
        3..=5 => write_number_payload(
            value
                .as_number()
                .context("Expected number binary settings value")?,
            kind,
            writer,
        )?,
        6 => {
            let text =
                raw_string_payload(value).context("Expected string binary settings value")?;
            write_binary_string_id(writer, string_ids, text)?;
        }
        7 => {
            let items = value
                .as_array()
                .context("Expected array binary settings value")?;
            write_var_u64(writer, items.len() as u64)?;
            for item in items {
                write_raw_value(item, string_ids, lookup, writer)?;
            }
        }
        8 => write_raw_object_payload(value, string_ids, lookup, writer)?,
        9 => {
            let numbers_len = value
                .as_array()
                .map(Vec::len)
                .context("Expected numeric array binary settings value")?;
            write_var_u64(writer, numbers_len as u64)?;
            write_numeric_array_components(value, writer)?;
        }
        20 => write_number_payload(
            value
                .as_number()
                .context("Expected f32 number binary settings value")?,
            kind,
            writer,
        )?,
        21 => {
            let numbers_len = value
                .as_array()
                .map(Vec::len)
                .context("Expected f32 numeric array binary settings value")?;
            write_var_u64(writer, numbers_len as u64)?;
            write_numeric_array_f32_components(value, writer)?;
        }
        11 | 12 | 13 | 14 | 15 | 16 | 18 => {
            let fixed_kind = fixed_numeric_kind_from_tag(kind)
                .context("Expected fixed numeric binary settings kind")?;
            write_fixed_numeric_components(value, fixed_kind, writer)?;
        }
        10 => {
            let ref_value =
                ref_payload_object(value).context("Expected Ref binary settings value")?;
            write_ref_fallback_payload(ref_value, string_ids, writer)?;
        }
        17 => {
            let ref_value =
                ref_payload_object(value).context("Expected Ref binary settings value")?;
            let target_index = resolve_ref_index(ref_value, lookup)?
                .context("Expected resolved Ref binary settings value")?;
            write_var_u64(writer, target_index as u64)?;
        }
        19 => {
            let text = raw_string_payload(value).context("Expected enum binary settings value")?;
            write_binary_string_id(writer, string_ids, text)?;
        }
        _ => bail!("Unknown binary settings value kind {kind}"),
    }
    Ok(())
}

fn write_number_payload<W: Write + ?Sized>(
    number: &serde_json::Number,
    kind: u8,
    writer: &mut W,
) -> Result<()> {
    match kind {
        3 => {
            let value = number
                .as_i64()
                .context("Expected signed integer binary settings value")?;
            write_var_u64(writer, zigzag_i64(value))
        }
        4 => {
            let value = number
                .as_u64()
                .context("Expected unsigned integer binary settings value")?;
            write_var_u64(writer, value)
        }
        5 => {
            let value = number
                .as_f64()
                .context("Expected float binary settings value")?;
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        20 => {
            let value = number
                .as_f64()
                .and_then(exact_f32)
                .context("Expected exactly representable f32 binary settings value")?;
            writer.write_all(&value.to_le_bytes())?;
            Ok(())
        }
        _ => bail!("Invalid binary number kind {kind}"),
    }
}

fn write_raw_object_payload<W: Write + ?Sized>(
    value: &Value,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    let obj = value
        .as_object()
        .context("Expected object binary settings value")?;
    if let Some(type_name) = obj.get("_type").and_then(Value::as_str) {
        match type_name {
            "BrickColor" => return write_brick_color_payload(obj, string_ids, lookup, writer),
            "ColorSequence" => {
                return write_color_sequence_payload(obj, string_ids, lookup, writer);
            }
            "NumberSequence" => {
                return write_number_sequence_payload(obj, string_ids, lookup, writer);
            }
            "Font" => return write_font_payload(obj, string_ids, writer),
            _ => {}
        }
    }

    let fields: Vec<_> = obj.iter().collect();
    write_var_u64(writer, fields.len() as u64)?;
    for (key, child) in fields {
        write_binary_string_id(writer, string_ids, key)?;
        write_raw_value(child, string_ids, lookup, writer)?;
    }
    Ok(())
}

fn write_brick_color_payload<W: Write + ?Sized>(
    obj: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    write_var_u64(writer, 1)?;
    write_binary_string_id(writer, string_ids, "BrickColor")?;
    write_raw_value(
        obj.get("number").unwrap_or(&Value::Null),
        string_ids,
        lookup,
        writer,
    )
}

fn write_sequence_payload_header<'a, W: Write + ?Sized>(
    obj: &'a Map<String, Value>,
    type_name: &str,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<&'a [Value]> {
    write_var_u64(writer, 1)?;
    write_binary_string_id(writer, string_ids, type_name)?;
    writer.write_all(&[8])?;
    write_var_u64(writer, 1)?;
    write_binary_string_id(writer, string_ids, "keypoints")?;
    writer.write_all(&[7])?;
    let keypoints = sequence_keypoint_values(obj);
    write_var_u64(
        writer,
        keypoints.iter().filter(|value| value.is_object()).count() as u64,
    )?;
    Ok(keypoints)
}

fn write_color_sequence_payload<W: Write + ?Sized>(
    obj: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    for keypoint_obj in write_sequence_payload_header(obj, "ColorSequence", string_ids, writer)?
        .iter()
        .filter_map(Value::as_object)
    {
        writer.write_all(&[8])?;
        write_var_u64(writer, 2)?;
        write_binary_string_id(writer, string_ids, "time")?;
        write_raw_value(
            keypoint_obj.get("time").unwrap_or(&Value::Null),
            string_ids,
            lookup,
            writer,
        )?;
        write_binary_string_id(writer, string_ids, "color")?;
        if let Some(color) = keypoint_obj.get("value").and_then(color_components) {
            write_numeric_slice_payload(&color, writer)?;
        } else {
            write_numeric_slice_payload(&[0.0_f64, 0.0, 0.0], writer)?;
        }
    }
    Ok(())
}

fn write_number_sequence_payload<W: Write + ?Sized>(
    obj: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup,
    writer: &mut W,
) -> Result<()> {
    for keypoint_obj in write_sequence_payload_header(obj, "NumberSequence", string_ids, writer)?
        .iter()
        .filter_map(Value::as_object)
    {
        writer.write_all(&[8])?;
        write_var_u64(writer, 3)?;
        for key in ["time", "value", "envelope"] {
            write_binary_string_id(writer, string_ids, key)?;
            write_raw_value(
                keypoint_obj.get(key).unwrap_or(&Value::Null),
                string_ids,
                lookup,
                writer,
            )?;
        }
    }
    Ok(())
}

fn write_font_payload<W: Write + ?Sized>(
    obj: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    let mut fields = Vec::new();
    if let Some(family) = obj.get("family").and_then(Value::as_str) {
        fields.push(("family", family));
    }
    if let Some(weight) = obj.get("weight").and_then(Value::as_str) {
        fields.push(("weight", split_enum_tail(weight)));
    }
    if let Some(style) = obj.get("style").and_then(Value::as_str) {
        fields.push(("style", split_enum_tail(style)));
    }
    if let Some(cached_face_id) = obj.get("cachedFaceId").and_then(Value::as_str) {
        fields.push(("cachedFaceId", cached_face_id));
    }

    write_var_u64(writer, fields.len() as u64)?;
    for (key, value) in fields {
        write_binary_string_id(writer, string_ids, key)?;
        writer.write_all(&[6])?;
        write_binary_string_id(writer, string_ids, value)?;
    }
    Ok(())
}

fn write_ref_fallback_payload<W: Write + ?Sized>(
    ref_value: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    let instance_id_owned = ref_value
        .get("instanceIndex")
        .and_then(Value::as_u64)
        .map(|instance_index| format!("{instance_index:x}"));
    let instance_id = ref_value
        .get("instanceId")
        .and_then(Value::as_str)
        .or(instance_id_owned.as_deref());
    let debug_id = ref_value.get("debugId").and_then(Value::as_str);
    let path_segments = ref_value.get("pathSegments").and_then(Value::as_array);
    let path = ref_value.get("path").and_then(Value::as_str);
    let settings_id = ref_value.get("settingsId").and_then(Value::as_str);
    let settings_id = settings_id
        .or_else(|| ref_value.get("referent").and_then(Value::as_str))
        .or_else(|| ref_value.get("ref").and_then(Value::as_str));
    let path_ordinals = ref_value.get("pathOrdinals").and_then(Value::as_array);
    let mut flags = 0_u64;
    if instance_id.is_some() {
        flags |= 1;
    }
    if debug_id.is_some() {
        flags |= 2;
    }
    if path_segments.is_some() {
        flags |= 4;
    }
    if path.is_some() {
        flags |= 8;
    }
    if settings_id.is_some() {
        flags |= 16;
    }
    if path_ordinals.is_some() {
        flags |= 32;
    }
    write_var_u64(writer, flags)?;
    if let Some(instance_id) = instance_id {
        write_binary_string_id(writer, string_ids, instance_id)?;
    }
    if let Some(debug_id) = debug_id {
        write_binary_string_id(writer, string_ids, debug_id)?;
    }
    if let Some(path_segments) = path_segments {
        write_var_u64(writer, path_segments.len() as u64)?;
        for segment in path_segments {
            let text = segment
                .as_str()
                .context("Expected Ref path segment string in binary settings value")?;
            write_binary_string_id(writer, string_ids, text)?;
        }
    }
    if let Some(path) = path {
        write_binary_string_id(writer, string_ids, path)?;
    }
    if let Some(settings_id) = settings_id {
        write_binary_string_id(writer, string_ids, settings_id)?;
    }
    if let Some(path_ordinals) = path_ordinals {
        write_var_u64(writer, path_ordinals.len() as u64)?;
        for ordinal in path_ordinals {
            let ordinal = ordinal
                .as_u64()
                .filter(|value| *value > 0)
                .context("Expected positive Ref path ordinal in binary settings value")?;
            write_var_u64(writer, ordinal)?;
        }
    }
    Ok(())
}

fn attribute_type_key(obj: &Map<String, Value>) -> Option<&'static str> {
    let type_name = obj.get("_type").and_then(Value::as_str)?;
    if type_name == "Float" {
        return matches!(
            obj.get("value").and_then(Value::as_str),
            Some("nan" | "inf" | "-inf")
        )
        .then_some("Float64");
    }
    let supported = [
        "Vector2",
        "Vector3",
        "Color3",
        "UDim",
        "UDim2",
        "CFrame",
        "Rect",
        "Enum",
        "EnumItem",
        "Font",
        "ColorSequence",
        "NumberSequence",
        "BrickColor",
        "NumberRange",
        "BinaryString",
    ];
    for key in supported {
        if obj.get(key).is_some() {
            return Some(key);
        }
    }
    if type_name == "EnumItem" {
        return Some("EnumItem");
    }
    supported
        .into_iter()
        .find(|&key| type_name == key)
        .map(|v| v as _)
}

fn attribute_payload_child<'a>(
    obj: &'a Map<String, Value>,
    key: &str,
    value: &'a Value,
) -> &'a Value {
    if let Some(child) = obj.get(key) {
        return child;
    }
    if key == "Enum"
        && let Some(child) = obj.get("EnumItem")
    {
        return child;
    }
    value
}

fn enum_item_attribute_fields(value: &Value) -> Option<(&str, &str)> {
    let obj = value.as_object()?;
    let enum_type = obj.get("enumType").and_then(Value::as_str)?;
    let name = obj.get("name").and_then(Value::as_str)?;
    Some((enum_type, name))
}

fn normalize_enum_attribute_value(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    obj.entry("_type".to_string())
        .or_insert_with(|| Value::String("EnumItem".to_string()));
    Value::Object(obj)
}

fn collect_attribute_value_strings<'a>(
    value: &'a Value,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    if let Some(obj) = value.as_object()
        && let Some(key) = attribute_type_key(obj)
    {
        add_count(out, key, 1);
        let lookup = SettingsBinaryInstanceLookup::default();
        let child = attribute_payload_child(obj, key, value);
        if key == "EnumItem" {
            if let Some((enum_type, name)) = enum_item_attribute_fields(child) {
                for text in ["enumType", enum_type, "name", name] {
                    add_count(out, text, 1);
                }
            } else {
                collect_raw_value_strings(child, &lookup, out)?;
            }
        } else {
            collect_raw_value_strings(child, &lookup, out)?;
        }
        return Ok(());
    }
    if value.is_boolean() {
        add_count(out, "Bool", 1);
    } else if value.is_number() {
        add_count(out, "Float64", 1);
    } else if let Some(text) = value.as_str() {
        add_count(out, "String", 1);
        add_count(out, text, 1);
    } else {
        bail!("Unsupported attribute binary value");
    }
    Ok(())
}

fn write_attributes_payload<W: Write + ?Sized>(
    attributes: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    write_var_u64(writer, attributes.len() as u64)?;
    for (key, value) in attributes {
        write_binary_string_id(writer, string_ids, key)?;
        write_attribute_value(value, string_ids, writer)
            .with_context(|| format!("Could not encode attribute {key}"))?;
    }
    Ok(())
}

fn write_attribute_value<W: Write + ?Sized>(
    value: &Value,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    let (key, child) = if let Some(obj) = value.as_object() {
        if let Some(key) = attribute_type_key(obj) {
            (key, attribute_payload_child(obj, key, value))
        } else {
            bail!("Unsupported attribute binary object");
        }
    } else if value.is_boolean() {
        ("Bool", value)
    } else if value.is_number() {
        ("Float64", value)
    } else if value.is_string() {
        ("String", value)
    } else {
        bail!("Unsupported attribute binary value");
    };

    writer.write_all(&[8])?;
    write_var_u64(writer, 1)?;
    write_binary_string_id(writer, string_ids, key)?;
    if key == "EnumItem"
        && let Some((enum_type, name)) = enum_item_attribute_fields(child)
    {
        write_enum_item_attribute_payload(enum_type, name, string_ids, writer)?;
        return Ok(());
    }
    let lookup = SettingsBinaryInstanceLookup::default();
    write_raw_value(child, string_ids, &lookup, writer)
}

fn write_enum_item_attribute_payload<W: Write + ?Sized>(
    enum_type: &str,
    name: &str,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    writer.write_all(&[8])?;
    write_var_u64(writer, 2)?;
    write_binary_string_id(writer, string_ids, "enumType")?;
    writer.write_all(&[6])?;
    write_binary_string_id(writer, string_ids, enum_type)?;
    write_binary_string_id(writer, string_ids, "name")?;
    writer.write_all(&[6])?;
    write_binary_string_id(writer, string_ids, name)
}

fn write_lookup_id<W: Write + ?Sized>(
    writer: &mut W,
    ids: &SettingsStringIdMap<'_>,
    text: &str,
    kind: &str,
) -> Result<()> {
    let id = ids
        .get(text)
        .with_context(|| format!("Missing binary {kind} id for {text:?}"))?;
    write_var_u64(writer, *id)
}

fn write_binary_string_id<W: Write + ?Sized>(
    writer: &mut W,
    string_ids: &SettingsStringIdMap<'_>,
    text: &str,
) -> Result<()> {
    let id = string_ids
        .get(text)
        .with_context(|| format!("Missing binary string id for {text:?}"))?;
    write_var_u64(writer, *id)
}

fn read_compact_settings_id(reader: &mut BytecodeReader<'_>, strings: &[String]) -> Result<String> {
    let token = reader
        .read_var_u64()
        .context("Missing compact instance settings id")?;
    if token & 1 == 1 {
        return Ok(format!("debug:0_{}", token >> 1));
    }
    let string_id = usize::try_from(token >> 1).context("Settings string id does not fit usize")?;
    strings
        .get(string_id)
        .cloned()
        .with_context(|| format!("Invalid instance settings string id {string_id}"))
}

fn write_compact_settings_id<W: Write + ?Sized>(
    writer: &mut W,
    string_ids: &SettingsStringIdMap<'_>,
    text: &str,
) -> Result<()> {
    if let Some(value) = parse_numeric_debug_settings_id(text) {
        return write_var_u64(writer, (value << 1) | 1);
    }
    let id = string_ids
        .get(text)
        .with_context(|| format!("Missing binary string id for {text:?}"))?;
    write_var_u64(
        writer,
        id.checked_shl(1)
            .context("Settings string id exceeds compact range")?,
    )
}

fn write_settings_binary_id<W: Write + ?Sized>(
    writer: &mut W,
    string_ids: &SettingsStringIdMap<'_>,
    settings_id: &SettingsBinaryId<'_>,
) -> Result<()> {
    match settings_id {
        SettingsBinaryId::Text(text) => {
            write_compact_settings_id(writer, string_ids, text.as_ref())
        }
        SettingsBinaryId::NumericDebug(value) => write_var_u64(writer, (value << 1) | 1),
    }
}

pub(crate) fn reindex_reference_indices(
    record: &mut Map<String, Value>,
    indices: &HashMap<String, usize>,
) {
    visit_reference_objects_mut(record, |object| {
        let index = object
            .get("settingsId")
            .or_else(|| object.get("instanceId"))
            .and_then(Value::as_str)
            .and_then(|id| indices.get(id))
            .copied();
        if let Some(index) = index {
            object.insert("instanceIndex".to_string(), Value::from(index + 1));
        } else {
            object.remove("instanceIndex");
        }
    });
}

pub(crate) fn visit_reference_objects_mut(
    record: &mut Map<String, Value>,
    mut visitor: impl FnMut(&mut Map<String, Value>),
) {
    fn visit(value: &mut Value, visitor: &mut impl FnMut(&mut Map<String, Value>)) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, visitor);
                }
            }
            Value::Object(object) => {
                if is_reference_object(object) {
                    visitor(object);
                }
                for value in object.values_mut() {
                    visit(value, visitor);
                }
            }
            _ => {}
        }
    }

    for value in record.values_mut() {
        visit(value, &mut visitor);
    }
}

pub(crate) fn stabilize_reference_objects(
    record: &mut Map<String, Value>,
    mut resolve: impl FnMut(&mut Map<String, Value>, usize),
) {
    visit_reference_objects_mut(record, |object| {
        if object.get("settingsId").and_then(Value::as_str).is_none()
            && object.get("instanceId").and_then(Value::as_str).is_none()
            && let Some(index) = object
                .get("instanceIndex")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| index.checked_sub(1))
        {
            resolve(object, index);
        }
        object.remove("instanceIndex");
    });
}

pub(crate) fn is_reference_object(object: &Map<String, Value>) -> bool {
    object.get("_type").and_then(Value::as_str) == Some("Ref")
        || object.contains_key("settingsId")
        || object.contains_key("instanceId")
        || object.contains_key("instanceIndex")
}

pub(crate) fn write_var_u64<W: Write + ?Sized>(writer: &mut W, mut value: u64) -> Result<()> {
    let mut buf = [0u8; 10];
    let mut len = 0usize;
    while value >= 0x80 {
        buf[len] = ((value as u8) & 0x7f) | 0x80;
        len += 1;
        value >>= 7;
    }
    buf[len] = value as u8;
    len += 1;
    writer.write_all(&buf[..len])?;
    Ok(())
}

fn zigzag_i64(value: i64) -> u64 {
    ((value as u64) << 1) ^ ((value >> 63) as u64)
}

fn encode_parent_index(parent_index: Option<usize>, instance_index: usize) -> Result<u64> {
    let Some(parent_index) = parent_index else {
        return Ok(0);
    };
    let parent = i64::try_from(parent_index).context("Parent index does not fit in i64")?;
    let current = i64::try_from(instance_index).context("Instance index does not fit in i64")?;
    Ok(zigzag_i64(parent - current) + 1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fixed_numeric_component_preserves_special_values_and_clamps_overflow() {
        assert_eq!(fixed_numeric_component_f32(1.5), 1.5_f32);
        assert_eq!(fixed_numeric_component_f32(1e39), f32::MAX);
        assert_eq!(fixed_numeric_component_f32(-1e39), f32::MIN);
        assert_eq!(fixed_numeric_component_f32(f64::INFINITY), f32::INFINITY);
        assert_eq!(
            fixed_numeric_component_f32(f64::NEG_INFINITY),
            f32::NEG_INFINITY
        );
        assert!(fixed_numeric_component_f32(f64::NAN).is_nan());
    }

    #[test]
    fn settings_bytecode_roundtrips_properties_and_attributes() {
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".to_string(),
                    "Workspace".to_string(),
                    "Workspace".to_string(),
                    None,
                ),
                SettingsBytecodeInstance {
                    settings_id: "child".to_string(),
                    name: "Target".to_string(),
                    class_name: "Part".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([
                        ("Tags".to_string(), json!(["Enemy"])),
                        ("Transparency".to_string(), json!(0.25)),
                        (
                            "CollisionGroupData".to_string(),
                            json!({"_type":"BinaryString","base64":"AAEC"}),
                        ),
                        (
                            "FontFace".to_string(),
                            json!({
                                "_type":"Font",
                                "family":"rbxasset://fonts/families/SourceSansPro.json",
                                "weight":"Regular",
                                "style":"Normal",
                                "cachedFaceId":"rbxasset://fonts/SourceSansPro-Regular.ttf",
                            }),
                        ),
                    ]),
                    attributes: Map::from_iter([
                        ("Health".to_string(), json!(100)),
                        (
                            "OriginalMaterial".to_string(),
                            json!({"_type":"BinaryString","base64":"UGxhc3RpYw=="}),
                        ),
                    ]),
                },
                SettingsBytecodeInstance {
                    settings_id: "input".to_string(),
                    name: "Inputs".to_string(),
                    class_name: "Configuration".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::from_iter([(
                        "gamepadEnterKeyCode".to_string(),
                        json!({
                            "_type": "EnumItem",
                            "enumType": "Enum.KeyCode",
                            "name": "ButtonL2",
                        }),
                    )]),
                },
            ],
        };

        let bytes = encode_settings_bytecode(&document).unwrap();
        let decoded = decode_settings_bytecode(&bytes).unwrap();

        assert_eq!(decoded.instances.len(), 3);
        assert_eq!(decoded.instances[1].parent_index, Some(0));
        assert_eq!(
            decoded.instances[1].properties.get("Tags"),
            Some(&json!(["Enemy"]))
        );
        assert_eq!(
            decoded.instances[1].properties.get("Transparency"),
            Some(&json!(0.25))
        );
        assert_eq!(
            decoded.instances[1].attributes.get("Health"),
            Some(&json!(100))
        );
        assert_eq!(
            decoded.instances[1].properties.get("CollisionGroupData"),
            Some(&json!({"_type":"BinaryString","base64":"AAEC"}))
        );
        assert_eq!(
            decoded.instances[1].properties.get("FontFace"),
            Some(&json!({
                "family":"rbxasset://fonts/families/SourceSansPro.json",
                "weight":"Regular",
                "style":"Normal",
                "cachedFaceId":"rbxasset://fonts/SourceSansPro-Regular.ttf",
            }))
        );
        assert_eq!(
            decoded.instances[1].attributes.get("OriginalMaterial"),
            Some(&json!({"_type":"BinaryString","base64":"UGxhc3RpYw=="}))
        );
        assert_eq!(
            decoded.instances[2].attributes.get("gamepadEnterKeyCode"),
            Some(&json!({
                "_type": "EnumItem",
                "enumType": "Enum.KeyCode",
                "name": "ButtonL2",
            }))
        );
    }

    #[test]
    fn settings_bytecode_uses_current_zstd_container_losslessly() {
        let instances = (0..512)
            .map(|index| SettingsBytecodeInstance {
                settings_id: format!("debug:0_{index}"),
                name: "RepeatedPart".to_string(),
                class_name: "Part".to_string(),
                parent_index: (index > 0).then_some(0),
                properties: Map::from_iter([
                    ("Anchored".to_string(), json!(true)),
                    ("CanCollide".to_string(), json!(false)),
                    ("Transparency".to_string(), json!(0.25)),
                    (
                        "Color".to_string(),
                        json!({ "_type": "Color3", "r": 0.25, "g": 0.5, "b": 0.75 }),
                    ),
                    (
                        "Size".to_string(),
                        json!({
                            "NumberSequence": {
                                "keypoints": [
                                    { "time": 0.0, "value": 1.0, "envelope": 0.0 },
                                    { "time": 1.0, "value": 2.0, "envelope": 0.5 }
                                ]
                            }
                        }),
                    ),
                    (
                        "Gradient".to_string(),
                        json!({
                            "ColorSequence": {
                                "keypoints": [
                                    { "time": 0.0, "color": [0.25, 0.5, 0.75] },
                                    { "time": 1.0, "color": [0.75, 0.5, 0.25] }
                                ]
                            }
                        }),
                    ),
                ]),
                attributes: Map::from_iter([("Role".to_string(), json!("Fixture"))]),
            })
            .collect();
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let payload = encode_settings_bytecode_payload(&document).unwrap();
        let encoded = encode_settings_bytecode(&document).unwrap();
        let decoded = decode_settings_bytecode(&encoded).unwrap();

        assert_eq!(
            encoded[SETTINGS_BINARY_MAGIC.len()],
            SETTINGS_BINARY_VERSION
        );
        assert!(
            encoded.len() < SETTINGS_BINARY_MAGIC.len() + 1 + payload.len(),
            "compressed bytecode should be smaller than the raw payload"
        );
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(&document).unwrap()
        );
    }

    #[test]
    fn settings_bytecode_rejects_oversized_decoded_payload_before_decompression() {
        let mut bytes = SETTINGS_BINARY_MAGIC.to_vec();
        bytes.push(SETTINGS_BINARY_VERSION);
        write_var_u64(&mut bytes, (MAX_SETTINGS_BYTECODE_BYTES as u64) + 1).unwrap();
        write_var_u64(&mut bytes, 0).unwrap();

        let error = decode_settings_bytecode(&bytes).unwrap_err().to_string();
        assert!(error.contains("Decoded settings bytecode payload exceeds safe size limit"));
    }

    #[test]
    fn settings_bytecode_rejects_excessively_nested_values() {
        let mut bytes = Vec::new();
        for _ in 0..=MAX_SETTINGS_VALUE_DEPTH {
            bytes.push(7);
            write_var_u64(&mut bytes, 1).unwrap();
        }
        bytes.push(0);
        let mut reader = BytecodeReader::new(&bytes);

        let error = decode_raw_value(&mut reader, &[], 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("value nesting exceeds safe depth"));
    }

    fn hierarchy_instance(
        settings_id: impl Into<String>,
        parent_index: Option<usize>,
    ) -> SettingsBytecodeInstance {
        SettingsBytecodeInstance::new(
            settings_id.into(),
            String::new(),
            "Folder".to_string(),
            parent_index,
        )
    }

    #[test]
    fn settings_bytecode_rejects_parent_cycles_on_decode() {
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                hierarchy_instance("a", Some(1)),
                hierarchy_instance("b", Some(0)),
            ],
        };
        let payload = encode_settings_bytecode_payload(&document).unwrap();
        let bytes = wrap_settings_bytecode_payload(&payload).unwrap();

        let error = decode_settings_bytecode(&bytes).unwrap_err().to_string();
        assert!(error.contains("parent cycle"));
    }

    #[test]
    fn settings_bytecode_rejects_excessively_deep_hierarchies() {
        let instances = (0..=MAX_SETTINGS_HIERARCHY_DEPTH)
            .map(|index| {
                hierarchy_instance(
                    format!("id-{index}"),
                    if index > 0 { Some(index - 1) } else { None },
                )
            })
            .collect::<Vec<_>>();

        let error = validate_settings_hierarchy(&instances)
            .unwrap_err()
            .to_string();
        assert!(error.contains("hierarchy exceeds safe depth"));
    }

    #[test]
    fn settings_bytecode_allows_acyclic_forward_parent_references() {
        let instances = vec![
            hierarchy_instance("a", Some(1)),
            hierarchy_instance("b", None),
        ];

        validate_settings_hierarchy(&instances).unwrap();
    }
}
