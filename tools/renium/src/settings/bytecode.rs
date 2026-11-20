use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result, bail};
use memchr::memmem;
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::rbx::decode::{json_number_f64, nonfinite_float_from_json, nonfinite_float_json};
use crate::roblox::schema::MESH_SIZE_TRANSPORT_PROPERTY;
use crate::snapshot::types::{NativeSettingsValue, ServiceState, SnapshotInstance};
use crate::system::LockRecover;
use crate::system::files::write_bytes_if_changed;

#[path = "property_edit.rs"]
pub(crate) mod property_edit;

#[path = "blocks.rs"]
pub(crate) mod blocks;

#[path = "captured.rs"]
mod captured;
#[cfg(test)]
#[path = "captured_sim.rs"]
pub(crate) mod captured_sim;

/// The same document as decoding `encode_service_settings_binary(state)`.
pub(crate) fn settings_bytecode_from_service_state(
    state: &ServiceState,
) -> Result<SettingsBytecode> {
    captured::convert(state)
}

/// What a pull needs to know of a captured state to say it is one it took
/// before: a digest of all the state holds apart from its viewport camera's
/// values, and whether that camera holds an enum value without its type
/// (`viewport_camera_lacks_enum_types`).
pub(crate) struct PulledDigest {
    pub(crate) digest: [u8; 32],
    pub(crate) viewport_camera_lacks_enum_types: bool,
    pub(crate) rows: Arc<PulledRows>,
}

/// The digest's parts: one for what the state holds beside its instances,
/// and two for each instance, in the state's order. What places an instance
/// in its store (ids, name, class, parent, paths and its UniqueId) is apart
/// from the values it holds.
pub(crate) struct PulledRows {
    frame: [u8; 32],
    rows: Vec<PulledRow>,
    /// Where the viewport camera stands, whose values no digest covers.
    viewport_camera: Option<usize>,
    /// Whether the state's own encoding is the encoding of its document
    /// (see `values_encode_alike`).
    encodes_as_its_document: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct PulledRow {
    placed: [u8; 16],
    values: [u8; 16],
}

impl PulledRows {
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether a store written from the state's document is the state's own
    /// encoding, so that the file holds exactly what these rows were taken of.
    pub(crate) fn encodes_as_its_document(&self) -> bool {
        self.encodes_as_its_document
    }
}

/// Whether the encoder of documents writes these values of an instance as
/// the encoder of captured states writes them. It settles a forced property
/// (see `forced_property_differs`), gives an attribute in a wrapped form its
/// type, refuses an attribute it has no form for, and finds the instance of
/// the store that a reference names by an id or a path; the encoder of states
/// does none of these. A reference by position, and one whose path leads into
/// another service, they write alike.
fn values_encode_alike(
    properties: &Map<String, Value>,
    attributes: &Map<String, Value>,
    root_service: Option<&str>,
) -> bool {
    fn references_alike(value: &Value, root_service: Option<&str>) -> bool {
        match value {
            Value::Array(items) => items
                .iter()
                .all(|item| references_alike(item, root_service)),
            Value::Object(object) => {
                let named = SETTINGS_REFERENCE_SELECTOR_KEYS[1..]
                    .iter()
                    .any(|key| object.contains_key(*key));
                if object.get("Ref").is_some_and(Value::is_object) {
                    return false;
                }
                if named {
                    return matches!(
                        strict_reference_path(object),
                        Ok(Some((segments, _)))
                            if root_service.is_some_and(|root| {
                                segments.first().is_some_and(|service| service != root)
                            })
                    );
                }
                object
                    .values()
                    .all(|item| references_alike(item, root_service))
            }
            _ => true,
        }
    }
    !properties.contains_key("Attributes")
        && attributes.values().all(|value| {
            // An object without a type is one of the wrapped forms, whether
            // the encoder of documents knows it or not.
            value
                .as_object()
                .is_none_or(|object| object.contains_key("_type"))
                && canonical_attribute_value(value).is_none()
                && collect_attribute_value_strings(value, &mut SettingsStringCounts::default())
                    .is_ok()
        })
        && properties
            .values()
            .chain(attributes.values())
            .all(|value| references_alike(value, root_service))
}

/// What a pull of `state` leaves of the stored file `stored`, where that file
/// holds exactly the document of an earlier state with the rows `before`, and
/// only values changed since, in instances a pull can compare one by one.
/// `None` where the pull has to align the whole documents.
/// Import's dedicated settings thread supplies its export's gate, which
/// bounds what the stores of one export hold in memory at a time: a store is
/// encoded under it, and a large one is read under it. Other callers use
/// None.
pub(crate) fn pull_changed_values(
    state: &ServiceState,
    before: &PulledRows,
    now: &PulledRows,
    stored: &[u8],
    store_gate: Option<&Mutex<()>>,
) -> Result<Option<crate::settings::equivalence::SettingsAlignment>> {
    captured::changed_values(state, before, now, stored, store_gate)
}

/// The same for a stored file no pull remembers, by comparing the two
/// documents instance by instance. The answer comes with whether every
/// instance but the viewport camera is the same in both.
/// The optional gate has the same dedicated-thread contract as above.
pub(crate) fn pull_changed_document_rows(
    state: &ServiceState,
    now: &PulledRows,
    stored: &[u8],
    store_gate: Option<&Mutex<()>>,
) -> Result<Option<(crate::settings::equivalence::SettingsAlignment, bool)>> {
    captured::changed_document_rows(state, now, stored, store_gate)
}

/// The digest and the UniqueId digest `property_edit::whole_row_digests`
/// gives each instance of the file a document was decoded from, taken from
/// the document as it was decoded.
pub(crate) fn whole_instance_digests(document: &SettingsBytecode) -> Vec<([u8; 16], [u8; 16])> {
    document
        .instances
        .par_iter()
        .map(captured::whole_document_row_digest)
        .collect()
}

/// The `PulledDigest` of this state, without holding its document.
pub(crate) fn pulled_digest_from_service_state(state: &ServiceState) -> Result<PulledDigest> {
    captured::pulled_digest(state)
}

const SETTINGS_BINARY_MAGIC: &[u8] = b"RBSSET\0";
const SETTINGS_BINARY_MIN_VERSION: u8 = 10;
/// The last version of a store in one frame (see `blocks` for the version
/// every store is written in).
const SETTINGS_COLUMN_VERSION: u8 = 11;
pub(crate) const SETTINGS_BINARY_VERSION: u8 = 12;
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
/// The memory a decoded store may take: this much for any file, and more for
/// a larger one. A decoded value takes far more memory than its bytes in the
/// file, and a payload of one repeated value compresses to almost nothing, so
/// the byte limits alone leave a file of a few kilobytes room for gigabytes.
/// Real stores decode to a few hundred megabytes at most and compress less
/// than tenfold.
const SETTINGS_DECODE_BASE_BYTES: usize = 1 << 30;
const SETTINGS_DECODE_BYTES_PER_FILE_BYTE: usize = 256;
/// What the decoder charges, rounded up from what the types take: a value
/// slot (in a group, an array or a map), a map's first node, and the fixed
/// part of each value kind.
const VALUE_SLOT_BYTES: usize = 64;
const MAP_BYTES: usize = 640;
const INSTANCE_BYTES: usize = 256;
/// Properties a store never holds: the record's own name, class and parent say
/// what the instance is and where it sits.
const SETTINGS_RECORD_FIELDS: [&str; 3] = ["Name", "ClassName", "Parent"];
const MAX_SETTINGS_HIERARCHY_DEPTH: usize = 512;

fn checked_settings_collection_len(len: usize, label: &str) -> Result<usize> {
    if len > MAX_SETTINGS_COLLECTION_ITEMS {
        bail!("{label} exceeds safe collection limit of {MAX_SETTINGS_COLLECTION_ITEMS} items");
    }
    Ok(len)
}

fn check_settings_string_len(len: usize) -> Result<()> {
    if len > MAX_SETTINGS_STRING_BYTES {
        bail!("Settings string exceeds safe size limit of {MAX_SETTINGS_STRING_BYTES} bytes");
    }
    Ok(())
}

fn check_settings_file_len(len: usize) -> Result<()> {
    if len > MAX_SETTINGS_BYTECODE_BYTES {
        bail!("Settings bytecode exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes");
    }
    Ok(())
}

fn check_settings_payload_len(len: usize, label: &str) -> Result<()> {
    if len > MAX_SETTINGS_BYTECODE_BYTES {
        bail!(
            "{label} settings bytecode payload exceeds safe size limit of {MAX_SETTINGS_BYTECODE_BYTES} bytes"
        );
    }
    Ok(())
}

fn checked_cframe_rotation_count(count: usize) -> Result<usize> {
    checked_settings_collection_len(count, "CFrame rotation table count")
}
// One document per service store; memory is bounded by the instance and byte
// limits below, so a project's whole set of small stores stays cached. A
// store that passes a limit by itself is decoded for its caller and not kept:
// its document is gigabytes, and the next edit of the store changes its bytes.
const MAX_CACHED_SETTINGS_DOCUMENTS: usize = 64;
const MAX_CACHED_SETTINGS_INSTANCES: usize = 80_000;
const MAX_CACHED_SETTINGS_BYTES: usize = 8 * 1024 * 1024;

struct CachedSettingsDocument {
    source: Box<[u8]>,
    document: Arc<SettingsBytecode>,
}

#[derive(Default)]
struct SettingsDocumentCache {
    entries: VecDeque<CachedSettingsDocument>,
    source_bytes: usize,
    instances: usize,
}

static SETTINGS_DOCUMENT_CACHE: OnceLock<Mutex<SettingsDocumentCache>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SettingsBytecode {
    pub(crate) version: u8,
    pub(crate) instances: Vec<SettingsBytecodeInstance>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsBytecodeInstance {
    pub(crate) settings_id: String,
    pub(crate) name: String,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_index: Option<usize>,
    /// A binary payload of some size is held once for the process, and the
    /// property holds its handle (see `settings::held`).
    #[serde(serialize_with = "crate::settings::held::serialize_properties")]
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
}

impl PartialEq for SettingsBytecodeInstance {
    fn eq(&self, other: &Self) -> bool {
        self.settings_id == other.settings_id
            && self.name == other.name
            && self.class_name == other.class_name
            && self.parent_index == other.parent_index
            && crate::settings::held::same_properties(&self.properties, &other.properties)
            && self.attributes == other.attributes
    }
}

impl SettingsBytecodeInstance {
    /// The instance's record, without its values.
    pub(crate) fn bare(&self) -> Self {
        Self::new(
            self.settings_id.clone(),
            self.name.clone(),
            self.class_name.clone(),
            self.parent_index,
        )
    }

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
        read_settings_file(path, decode_settings_bytecode)
    }

    pub(crate) fn read_file_cached(path: &Path) -> Result<Arc<Self>> {
        read_settings_file(path, cached_settings_document)
    }

    pub(crate) fn read_structure_file(path: &Path) -> Result<Self> {
        read_settings_file(path, decode_settings_bytecode_structure)
    }

    /// A store file for a command that reads a few of its rows: a v12 store
    /// without its values, with the reader that gives a row them, and a
    /// store of an older version whole.
    pub(crate) fn read_by_rows(path: &Path) -> Result<(Self, Option<blocks::RowReader>)> {
        Self::read_rows(path, false)
    }

    /// `read_by_rows` for a command that answers for the whole store: a v12
    /// store with a damaged frame is refused, as `read_file` refuses it.
    pub(crate) fn read_whole_by_rows(path: &Path) -> Result<(Self, Option<blocks::RowReader>)> {
        Self::read_rows(path, true)
    }

    fn read_rows(path: &Path, whole: bool) -> Result<(Self, Option<blocks::RowReader>)> {
        read_settings_file(path, |bytes| Self::decode_rows(bytes, whole))
    }

    /// `read_whole_by_rows` of the bytes of a store file.
    pub(crate) fn decode_whole_by_rows(bytes: &[u8]) -> Result<(Self, Option<blocks::RowReader>)> {
        Self::decode_rows(bytes, true)
    }

    fn decode_rows(bytes: &[u8], whole: bool) -> Result<(Self, Option<blocks::RowReader>)> {
        if !blocks::is_block_store(bytes) {
            return Ok((decode_settings_bytecode(bytes)?, None));
        }
        let (document, reader) = blocks::RowReader::decode(bytes)?;
        if whole {
            reader.check()?;
        }
        Ok((document, Some(reader)))
    }

    /// Decodes a store's instances without their properties and attributes.
    pub(crate) fn decode_structure(bytes: &[u8]) -> Result<Self> {
        decode_settings_bytecode_structure(bytes)
    }

    /// Decodes a store when it holds every string of one of the sets in
    /// `required`.
    pub(crate) fn decode_if_contains_any_string_set(
        bytes: &[u8],
        required: &[Vec<String>],
    ) -> Result<Option<Self>> {
        decode_settings_bytecode_if_contains_any_string_set(bytes, required)
    }

    pub(crate) fn read_file_if_contains_any_string_set(
        path: &Path,
        required: &[Vec<String>],
    ) -> Result<Option<Self>> {
        read_settings_file(path, |bytes| {
            decode_settings_bytecode_if_contains_any_string_set(bytes, required)
        })
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

fn cached_settings_document(bytes: &[u8]) -> Result<Arc<SettingsBytecode>> {
    let cache =
        SETTINGS_DOCUMENT_CACHE.get_or_init(|| Mutex::new(SettingsDocumentCache::default()));
    {
        let mut cache = cache.lock_recover();
        if let Some(index) = cache
            .entries
            .iter()
            .position(|entry| entry.source.as_ref() == bytes)
        {
            let entry = cache
                .entries
                .remove(index)
                .expect("cached settings document index should exist");
            let document = Arc::clone(&entry.document);
            cache.entries.push_back(entry);
            return Ok(document);
        }
    }

    let document = Arc::new(decode_settings_bytecode(bytes)?);
    if bytes.len() > MAX_CACHED_SETTINGS_BYTES
        || document.instances.len() > MAX_CACHED_SETTINGS_INSTANCES
    {
        return Ok(document);
    }
    let mut evicted = Vec::new();
    let mut cache = cache.lock_recover();
    if let Some(index) = cache
        .entries
        .iter()
        .position(|entry| entry.source.as_ref() == bytes)
    {
        let entry = cache
            .entries
            .remove(index)
            .expect("cached settings document index should exist");
        let document = Arc::clone(&entry.document);
        cache.entries.push_back(entry);
        return Ok(document);
    }
    cache.source_bytes += bytes.len();
    cache.instances += document.instances.len();
    cache.entries.push_back(CachedSettingsDocument {
        source: bytes.to_vec().into_boxed_slice(),
        document: Arc::clone(&document),
    });
    while cache.entries.len() > MAX_CACHED_SETTINGS_DOCUMENTS
        || cache.source_bytes > MAX_CACHED_SETTINGS_BYTES
        || cache.instances > MAX_CACHED_SETTINGS_INSTANCES
    {
        let entry = cache
            .entries
            .pop_front()
            .expect("non-empty settings cache should have a front entry");
        cache.source_bytes -= entry.source.len();
        cache.instances -= entry.document.instances.len();
        evicted.push(entry);
    }
    drop(cache);
    for entry in evicted {
        if let Ok(document) = Arc::try_unwrap(entry.document) {
            crate::settings::equivalence::drop_settings_document(document);
        }
    }
    Ok(document)
}

fn read_settings_file<T>(path: &Path, mut decode: impl FnMut(&[u8]) -> Result<T>) -> Result<T> {
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
        let bytes = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
        match decode(&bytes) {
            Ok(document) => return Ok(document),
            Err(_) if attempt < 5 => {
                thread::sleep(Duration::from_millis(15 * (attempt + 1) as u64));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to decode {}", path.display()));
            }
        }
    }
    unreachable!("settings bytecode retry loop always returns")
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
    if let Some(settings_id) = instance
        .transported_settings_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        return settings_id.to_string();
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

/// What no store holds as a property: the record's own fields and a script's
/// text, by their exact names. Roblox property names are case-sensitive, and
/// `AnimationRigData` carries real `name` and `parent` tables beside the
/// instance's Name and Parent.
fn is_record_property(name: &str) -> bool {
    matches!(
        name,
        "Source" | "ClassName" | "Parent" | "Name" | "RobloxLocked"
    )
}

pub(crate) fn should_skip_binary_property(
    state: &ServiceState,
    instance: &SnapshotInstance,
    name: &str,
    raw_value: &Value,
) -> bool {
    if is_record_property(name) {
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
    if is_known_default_property_value(property_name, property_value) {
        return true;
    }
    state
        .class_defaults_by_class
        .get(class_name)
        .and_then(|properties| properties.get(property_name))
        .is_some_and(|default| default == property_value)
}

pub(crate) fn is_known_default_property_value(property_name: &str, value: &Value) -> bool {
    matches!(
        (property_name, value),
        ("Archivable" | "CharacterAutoLoads", Value::Bool(true))
            | ("Sandboxed", Value::Bool(false))
    ) || is_null_reference_value(value)
}

// A reference that names no target (a cleared PrimaryPart, an unset
// ObjectValue) is what Studio reports by omitting the property.
pub(crate) fn is_null_reference_value(value: &Value) -> bool {
    let Value::Object(object) = value else {
        return false;
    };
    object.get("_type").and_then(Value::as_str) == Some("Ref")
        && ![
            "settingsId",
            "instanceId",
            "instanceIndex",
            "path",
            "pathSegments",
        ]
        .iter()
        .any(|key| object.get(*key).is_some_and(|value| !value.is_null()))
}

pub(crate) fn decode_settings_bytecode(bytes: &[u8]) -> Result<SettingsBytecode> {
    let _span = crate::app::profile::span("store decode");
    if blocks::is_block_store(bytes) {
        return blocks::BlockStore::open(bytes)?.decode();
    }
    let (version, decoded) = decode_settings_bytecode_container(bytes)?;
    decode_settings_bytecode_document(version, &decoded, &DecodeBudget::for_file(bytes.len()))
}

/// The document of a store for a pull or a push to compare, plan from and
/// write: it holds each binary payload of some size once, by its handle
/// (see `settings::held`), where a store's instances share the text of one
/// payload thousands of times. Whatever writes one of its values out
/// resolves the handle.
pub(crate) fn decode_settings_bytecode_holding(bytes: &[u8]) -> Result<SettingsBytecode> {
    let _span = crate::app::profile::span("store decode");
    if blocks::is_block_store(bytes) {
        return blocks::BlockStore::open(bytes)?.holding().decode();
    }
    let (version, decoded) = decode_settings_bytecode_container(bytes)?;
    decode_settings_bytecode_document(
        version,
        &decoded,
        &DecodeBudget::for_file(bytes.len()).holding_payloads(),
    )
}

/// Whether a store holds every string of one of the `required` sets, as
/// `decode_settings_bytecode_if_contains_any_string_set` asks before it
/// decodes one. A store that cannot be read counts as holding them: its
/// decode is the one to say why.
pub(crate) fn holds_any_string_set(bytes: &[u8], required: &[Vec<String>]) -> bool {
    if blocks::is_block_store(bytes) {
        return blocks::BlockStore::open(bytes)
            .and_then(|store| store.holds_any(required))
            .unwrap_or(true);
    }
    let Ok((_, decoded)) = decode_settings_bytecode_container(bytes) else {
        return true;
    };
    required.iter().any(|needles| {
        needles
            .iter()
            .all(|needle| memmem::find(&decoded, needle.as_bytes()).is_some())
    })
}

fn decode_settings_bytecode_if_contains_any_string_set(
    bytes: &[u8],
    required: &[Vec<String>],
) -> Result<Option<SettingsBytecode>> {
    if blocks::is_block_store(bytes) {
        return blocks::decode_if_contains(bytes, required);
    }
    let (version, decoded) = decode_settings_bytecode_container(bytes)?;
    if !required.is_empty()
        && !required.iter().any(|needles| {
            needles
                .iter()
                .all(|needle| memmem::find(&decoded, needle.as_bytes()).is_some())
        })
    {
        return Ok(None);
    }
    Ok(Some(decode_settings_bytecode_document(
        version,
        &decoded,
        &DecodeBudget::for_file(bytes.len()),
    )?))
}

fn decode_settings_bytecode_container(bytes: &[u8]) -> Result<(u8, Vec<u8>)> {
    let _span = crate::app::profile::span("store decompress");
    check_settings_file_len(bytes.len())?;
    let mut reader = BytecodeReader::new(bytes);
    reader.read_magic()?;
    let version = reader
        .read_u8()
        .context("Missing settings bytecode version")?;
    if !(SETTINGS_BINARY_MIN_VERSION..=SETTINGS_COLUMN_VERSION).contains(&version) {
        bail!("Unsupported settings bytecode version {version}");
    }

    let (decoded_len, encoded_len) = read_frame_lengths(&mut reader)?;
    let encoded = reader.read_bytes(encoded_len)?;
    reader.finish()?;
    let decoded = zstd::bulk::decompress(encoded, decoded_len)?;
    Ok((version, decoded))
}

/// The length of a compressed frame's content and of the frame, each within
/// what a store may hold.
fn read_frame_lengths(reader: &mut BytecodeReader<'_>) -> Result<(usize, usize)> {
    let decoded_len = reader.read_len("compressed settings bytecode payload length")?;
    let encoded_len = reader.read_len("compressed settings bytecode byte length")?;
    check_settings_payload_len(decoded_len, "Decoded")?;
    check_settings_payload_len(encoded_len, "Compressed")?;
    Ok((decoded_len, encoded_len))
}

fn decode_settings_bytecode_document(
    version: u8,
    decoded: &[u8],
    budget: &DecodeBudget,
) -> Result<SettingsBytecode> {
    let mut payload_reader = BytecodeReader::new(decoded);
    let document = decode_settings_bytecode_payload(version, &mut payload_reader, budget)?;
    payload_reader.finish()?;
    Ok(document)
}

fn decode_settings_bytecode_structure(bytes: &[u8]) -> Result<SettingsBytecode> {
    if blocks::is_block_store(bytes) {
        return blocks::BlockStore::open(bytes)?.structure();
    }
    let (version, decoded) = decode_settings_bytecode_container(bytes)?;
    let mut reader = BytecodeReader::new(&decoded);
    let header = decode_settings_payload_header(&mut reader, &DecodeBudget::for_file(bytes.len()))?;
    Ok(SettingsBytecode {
        version,
        instances: header.instances,
    })
}

struct SettingsPayloadHeader {
    strings: Vec<String>,
    properties: Vec<String>,
    instances: Vec<SettingsBytecodeInstance>,
    instance_count: usize,
}

fn decode_settings_payload_header(
    reader: &mut BytecodeReader<'_>,
    budget: &DecodeBudget,
) -> Result<SettingsPayloadHeader> {
    let _span = crate::app::profile::span("store header decode");
    let string_count = reader.read_collection_len("string count")?;
    let mut strings = Vec::with_capacity(reader.capacity(string_count, 1));
    for _ in 0..string_count {
        let len = reader.read_len("string byte length")?;
        check_settings_string_len(len)?;
        let bytes = reader.read_bytes(len)?;
        strings.push(String::from_utf8(bytes.to_vec()).context("Invalid UTF-8 settings string")?);
    }

    let class_count = reader.read_collection_len("class count")?;
    let mut classes = Vec::with_capacity(reader.capacity(class_count, 1));
    for _ in 0..class_count {
        classes.push(budget.copied(reader.read_string(&strings, "class string id")?)?);
    }

    let property_count = reader.read_collection_len("property count")?;
    let mut properties = Vec::with_capacity(reader.capacity(property_count, 1));
    for _ in 0..property_count {
        properties.push(budget.copied(reader.read_string(&strings, "property string id")?)?);
    }

    let instance_count = reader.read_collection_len("instance count")?;
    let instances = decode_settings_instances(reader, &strings, &classes, instance_count, budget)?;
    validate_settings_hierarchy(&instances)?;

    Ok(SettingsPayloadHeader {
        strings,
        properties,
        instances,
        instance_count,
    })
}

fn decode_settings_bytecode_payload(
    version: u8,
    reader: &mut BytecodeReader<'_>,
    budget: &DecodeBudget,
) -> Result<SettingsBytecode> {
    let SettingsPayloadHeader {
        strings,
        properties,
        mut instances,
        instance_count,
    } = decode_settings_payload_header(reader, budget)?;

    let specs = read_settings_property_groups(reader, &properties)?;
    let scope = ValueScope {
        strings: &strings,
        instance_count,
        budget,
    };
    let groups_span = crate::app::profile::span("store property columns decode");
    let decoded_groups = specs
        .par_iter()
        .map(|(property_name, kind, value_count, body)| {
            decode_property_group_body(body, property_name.as_str(), *kind, *value_count, scope)
        })
        .collect::<Result<Vec<_>>>()?;
    drop(groups_span);
    apply_decoded_property_groups(&mut instances, &specs, decoded_groups);

    Ok(SettingsBytecode { version, instances })
}

type PropertyGroup<'a, 'b> = (&'a String, u8, usize, &'b [u8]);

fn read_settings_property_groups<'a, 'b>(
    reader: &mut BytecodeReader<'b>,
    properties: &'a [String],
) -> Result<Vec<PropertyGroup<'a, 'b>>> {
    let group_count = reader.read_collection_len("property group count")?;
    let mut specs = Vec::with_capacity(reader.capacity(group_count, 4));
    for _ in 0..group_count {
        let property_id = reader.read_len("property id")?;
        let property_name = properties
            .get(property_id)
            .with_context(|| format!("Invalid property id {property_id}"))?;
        let kind = reader.read_u8().context("Missing property kind")?;
        let value_count = reader.read_collection_len("property value count")?;
        let body_len = reader.read_len("property group byte length")?;
        let body = reader.read_bytes(body_len)?;
        if SETTINGS_RECORD_FIELDS.contains(&property_name.as_str()) {
            continue;
        }
        specs.push((property_name, kind, value_count, body));
    }
    Ok(specs)
}

fn decode_settings_instance_range(
    bytes: &[u8],
    strings: &[String],
    classes: &[String],
    first_index: usize,
    instance_count: usize,
    count: usize,
    budget: &DecodeBudget,
) -> Result<Vec<SettingsBytecodeInstance>> {
    let mut reader = BytecodeReader::new(bytes);
    budget.instances(count)?;
    let mut instances = Vec::with_capacity(count);
    for offset in 0..count {
        let instance_index = first_index + offset;
        let settings_id = read_compact_settings_id(&mut reader, strings)?;
        budget.take(settings_id.len())?;
        let name = budget.copied(reader.read_string(strings, "instance name")?)?;
        let class_id = reader.read_len("instance class id")?;
        let class_name = budget.copied(
            classes
                .get(class_id)
                .with_context(|| format!("Invalid class id {class_id}"))?,
        )?;
        let parent_raw = reader.read_len("parent index")?;
        let parent_index = decode_parent_index(parent_raw, instance_index, instance_count)?;
        instances.push(SettingsBytecodeInstance::new(
            settings_id,
            name,
            class_name,
            parent_index,
        ));
    }
    reader.finish()?;
    Ok(instances)
}

fn decode_settings_instances(
    reader: &mut BytecodeReader<'_>,
    strings: &[String],
    classes: &[String],
    instance_count: usize,
    budget: &DecodeBudget,
) -> Result<Vec<SettingsBytecodeInstance>> {
    if instance_count < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES || rayon::current_num_threads() <= 1
    {
        let start = reader.position();
        for _ in 0..instance_count {
            reader.read_var_u64()?;
            reader.read_var_u64()?;
            reader.read_var_u64()?;
            reader.read_var_u64()?;
        }
        let bytes = &reader.bytes()[start..reader.position()];
        return decode_settings_instance_range(
            bytes,
            strings,
            classes,
            0,
            instance_count,
            instance_count,
            budget,
        );
    }

    let start = reader.position();
    let chunk_count = instance_count.div_ceil(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE);
    let mut offsets = Vec::with_capacity(chunk_count + 1);
    offsets.push(start);
    for index in 0..instance_count {
        reader.read_var_u64()?;
        reader.read_var_u64()?;
        reader.read_var_u64()?;
        reader.read_var_u64()?;
        if (index + 1) % SETTINGS_BINARY_PARALLEL_CHUNK_SIZE == 0 {
            offsets.push(reader.position());
        }
    }
    if offsets.last().copied() != Some(reader.position()) {
        offsets.push(reader.position());
    }
    let bytes = reader.bytes();
    let chunks = offsets
        .par_windows(2)
        .enumerate()
        .map(|(chunk_index, offsets)| {
            let first_index = chunk_index * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE;
            let count = (instance_count - first_index).min(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE);
            decode_settings_instance_range(
                &bytes[offsets[0]..offsets[1]],
                strings,
                classes,
                first_index,
                instance_count,
                count,
                budget,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(chunks.into_iter().flatten().collect())
}

enum DecodedGroupEntry {
    Property(usize, Value),
    Attributes(usize, Map<String, Value>),
}

impl DecodedGroupEntry {
    fn instance_index(&self) -> usize {
        match self {
            Self::Property(index, _) | Self::Attributes(index, _) => *index,
        }
    }
}

fn apply_decoded_property_groups(
    instances: &mut [SettingsBytecodeInstance],
    specs: &[(&String, u8, usize, &[u8])],
    decoded_groups: Vec<Vec<DecodedGroupEntry>>,
) {
    apply_decoded_property_groups_in_range(
        instances,
        0,
        specs,
        decoded_groups,
        SETTINGS_BINARY_PARALLEL_CHUNK_SIZE,
    );
}

/// A bounded row's values, independent of the retained structural index.
#[derive(Default)]
pub(crate) struct DecodedRowValues {
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
}

trait DecodedPropertyTarget: Send {
    fn maps(&mut self) -> (&mut Map<String, Value>, &mut Map<String, Value>);
}

impl DecodedPropertyTarget for SettingsBytecodeInstance {
    fn maps(&mut self) -> (&mut Map<String, Value>, &mut Map<String, Value>) {
        (&mut self.properties, &mut self.attributes)
    }
}

impl DecodedPropertyTarget for DecodedRowValues {
    fn maps(&mut self) -> (&mut Map<String, Value>, &mut Map<String, Value>) {
        (&mut self.properties, &mut self.attributes)
    }
}

fn apply_decoded_property_groups_in_range<T: DecodedPropertyTarget>(
    instances: &mut [T],
    first: usize,
    specs: &[(&String, u8, usize, &[u8])],
    mut decoded_groups: Vec<Vec<DecodedGroupEntry>>,
    chunk_size: usize,
) {
    let _span = crate::app::profile::span("store property columns apply");
    if instances.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES || rayon::current_num_threads() <= 1
    {
        for ((property_name, ..), entries) in specs.iter().zip(decoded_groups) {
            for entry in entries {
                match entry {
                    DecodedGroupEntry::Property(instance_index, value) => {
                        instances[instance_index - first]
                            .maps()
                            .0
                            .insert((*property_name).clone(), value);
                    }
                    DecodedGroupEntry::Attributes(instance_index, attributes) => {
                        *instances[instance_index - first].maps().1 = attributes;
                    }
                }
            }
        }
        return;
    }

    let chunk_count = instances.len().div_ceil(chunk_size);
    let mut by_chunk = (0..chunk_count).map(|_| Vec::new()).collect::<Vec<_>>();
    for (property_index, entries) in decoded_groups.iter_mut().enumerate() {
        let mut remaining = entries.as_mut_slice();
        // Both decoders validate strictly increasing indices within a group.
        // Slice those ranges without copying every value through a serial queue;
        // group order still decides which value wins across repeated groups.
        while let Some(entry) = remaining.first() {
            let chunk_index = (entry.instance_index() - first) / chunk_size;
            let end = first + (chunk_index + 1) * chunk_size;
            let count = remaining.partition_point(|entry| entry.instance_index() < end);
            let (chunk, tail) = remaining.split_at_mut(count);
            by_chunk[chunk_index].push((property_index, chunk));
            remaining = tail;
        }
    }
    instances
        .par_chunks_mut(chunk_size)
        .zip(by_chunk.into_par_iter())
        .for_each(|(instances, groups)| {
            for (property_index, entries) in groups {
                for entry in entries {
                    let index = (entry.instance_index() - first) % chunk_size;
                    match entry {
                        DecodedGroupEntry::Property(_, value) => {
                            instances[index]
                                .maps()
                                .0
                                .insert(specs[property_index].0.clone(), std::mem::take(value));
                        }
                        DecodedGroupEntry::Attributes(_, attributes) => {
                            *instances[index].maps().1 = std::mem::take(attributes);
                        }
                    }
                }
            }
        });
    decoded_groups.into_par_iter().for_each(drop);
}

/// What a value needs from its document: the string table, the instance
/// count its indices must stay under, and the document's memory budget.
#[derive(Clone, Copy)]
struct ValueScope<'a> {
    strings: &'a [String],
    instance_count: usize,
    budget: &'a DecodeBudget,
}

impl ValueScope<'_> {
    /// A copy of the string the next id names, charged to the budget: the
    /// table holds a string once, and every use of it is a copy.
    fn string(&self, reader: &mut BytecodeReader<'_>, label: &str) -> Result<String> {
        self.budget.copied(reader.read_string(self.strings, label)?)
    }
}

/// The memory a document may still decode into, shared by the parts that
/// decode in parallel and by every view of it.
struct DecodeBudget {
    remaining: Arc<AtomicUsize>,
    /// Whether the document holds each payload of some size once, by its
    /// handle (see `settings::held`).
    holds_payloads: bool,
}

impl DecodeBudget {
    fn new(bytes: usize) -> Self {
        Self {
            remaining: Arc::new(AtomicUsize::new(bytes)),
            holds_payloads: false,
        }
    }

    fn holding_payloads(mut self) -> Self {
        self.holds_payloads = true;
        self
    }

    /// This budget, charged as it is, for a decode that holds no payloads.
    fn not_holding(&self) -> Self {
        Self {
            remaining: Arc::clone(&self.remaining),
            holds_payloads: false,
        }
    }

    /// A property's value as its document holds it.
    fn property(&self, value: Value) -> Value {
        if self.holds_payloads {
            crate::settings::held::held(value)
        } else {
            value
        }
    }

    fn for_file(file_bytes: usize) -> Self {
        Self::new(
            SETTINGS_DECODE_BASE_BYTES
                .saturating_add(file_bytes.saturating_mul(SETTINGS_DECODE_BYTES_PER_FILE_BYTE)),
        )
    }

    /// Gives back all a reader of a file of `file_bytes` bytes may take, for
    /// a pass that reads again what an earlier pass of the reader read.
    fn renew(&self, file_bytes: usize) {
        let fresh = Self::for_file(file_bytes).remaining.load(Ordering::Relaxed);
        self.remaining.store(fresh, Ordering::Relaxed);
    }

    fn take(&self, bytes: usize) -> Result<()> {
        self.remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(bytes)
            })
            .map(drop)
            .map_err(|_| {
                anyhow::anyhow!(
                    "Settings bytecode decodes to more memory than a file of its size may take"
                )
            })
    }

    fn copied(&self, text: &str) -> Result<String> {
        self.take(text.len())?;
        Ok(text.to_string())
    }

    fn instances(&self, count: usize) -> Result<()> {
        self.take(count.saturating_mul(INSTANCE_BYTES))
    }

    fn property_group(&self, count: usize, name: &str, kind: u8) -> Result<()> {
        self.take(count.saturating_mul(VALUE_SLOT_BYTES + name.len() + value_kind_bytes(kind)))
    }

    fn numeric_array(&self, count: usize, kind: u8) -> Result<()> {
        let label = if kind == 21 {
            "f32 numeric array length"
        } else {
            "numeric array length"
        };
        checked_settings_collection_len(count, label)?;
        self.take(count.saturating_mul(VALUE_SLOT_BYTES))
    }
}

/// The fixed memory of one value of a kind, beyond its slot: the map a typed
/// value is, with its fields, or the header of an array.
fn value_kind_bytes(kind: u8) -> usize {
    match kind {
        0..=6 | 20 => 0,
        7 | 9 | 21 => VALUE_SLOT_BYTES,
        8 => MAP_BYTES,
        16 => MAP_BYTES + 16 * VALUE_SLOT_BYTES,
        _ => MAP_BYTES + 8 * VALUE_SLOT_BYTES,
    }
}

fn decode_property_group_body(
    body: &[u8],
    property_name: &str,
    kind: u8,
    value_count: usize,
    scope: ValueScope<'_>,
) -> Result<Vec<DecodedGroupEntry>> {
    let ValueScope { instance_count, .. } = scope;
    scope
        .budget
        .property_group(value_count, property_name, kind)?;
    let mut reader = BytecodeReader::new(body);
    let cframe_group_decoder = decode_cframe_group_header(kind, &mut reader)?;
    // Large numeric columns otherwise occupy one Rayon worker while the other
    // property groups finish. Locate values without allocating their JSON trees,
    // then construct those independent values in bounded parallel chunks.
    let numeric_bytes = match kind {
        11 | 13 => Some(8),
        12 | 15 => Some(12),
        14 | 18 => Some(16),
        16 => Some(match &cframe_group_decoder {
            CFrameGroupDecoder::Inline => 48,
            CFrameGroupDecoder::RotationTable(_) => 12,
        }),
        _ => None,
    };
    if let Some(numeric_bytes) = numeric_bytes
        && value_count >= SETTINGS_BINARY_PARALLEL_CHUNK_SIZE
        && rayon::current_num_threads() > 1
    {
        let mut values = Vec::with_capacity(reader.capacity(value_count, numeric_bytes + 1));
        let mut instance_index = 0_usize;
        for position in 0..value_count {
            instance_index = next_group_instance(
                instance_index,
                reader.read_len("property instance index")?,
                position,
            )?;
            if instance_index >= instance_count {
                bail!("Invalid property instance index {instance_index}");
            }
            let start = reader.position();
            reader.read_bytes(numeric_bytes)?;
            if matches!(cframe_group_decoder, CFrameGroupDecoder::RotationTable(_)) {
                reader.read_len("CFrame rotation index")?;
            }
            values.push((instance_index, &body[start..reader.position()]));
        }
        reader.finish()?;
        return values
            .into_par_iter()
            .with_min_len(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
            .map(|(index, bytes)| {
                let mut reader = BytecodeReader::new(bytes);
                let value = match &cframe_group_decoder {
                    CFrameGroupDecoder::Inline => {
                        decode_raw_value_payload(&mut reader, kind, scope, 0)?
                    }
                    CFrameGroupDecoder::RotationTable(rotations) => {
                        decode_cframe_rotation_table_payload(&mut reader, rotations)?
                    }
                };
                Ok(DecodedGroupEntry::Property(
                    index,
                    scope.budget.property(value),
                ))
            })
            .collect();
    }
    let mut previous_instance_index = 0_usize;
    let mut previous_ref_target_index = 0_usize;
    let mut entries = Vec::with_capacity(reader.capacity(value_count, 1));
    for position in 0..value_count {
        let instance_index = next_group_instance(
            previous_instance_index,
            reader.read_len("property instance index")?,
            position,
        )?;
        previous_instance_index = instance_index;
        if instance_index >= instance_count {
            bail!("Invalid property instance index {instance_index}");
        }
        entries.push(decode_property_group_entry(
            &mut reader,
            property_name,
            kind,
            instance_index,
            scope,
            &cframe_group_decoder,
            &mut previous_ref_target_index,
        )?);
    }
    reader.finish()?;
    Ok(entries)
}

// The shared value decoder for eager columns and bounded row materialization.
// Delta state and CFrame tables belong to the group, not to a row chunk.
fn decode_property_group_entry(
    reader: &mut BytecodeReader<'_>,
    property_name: &str,
    kind: u8,
    instance_index: usize,
    scope: ValueScope<'_>,
    cframe: &CFrameGroupDecoder,
    previous_ref_target: &mut usize,
) -> Result<DecodedGroupEntry> {
    if property_name == "Attributes" && kind == 8 {
        return Ok(DecodedGroupEntry::Attributes(
            instance_index,
            decode_attributes_payload(reader, scope)?,
        ));
    }
    let value = if kind == 17 {
        decode_resolved_ref_group_payload(reader, scope.instance_count, previous_ref_target)?
    } else {
        match cframe {
            CFrameGroupDecoder::Inline => decode_raw_value_payload(reader, kind, scope, 0)?,
            CFrameGroupDecoder::RotationTable(rotations) => {
                decode_cframe_rotation_table_payload(reader, rotations)?
            }
        }
    };
    Ok(DecodedGroupEntry::Property(
        instance_index,
        scope.budget.property(value),
    ))
}

/// The instance of a group's next value. A group holds one value per
/// instance in rising order, so after the first every step moves on.
fn next_group_instance(previous: usize, step: usize, position: usize) -> Result<usize> {
    if position > 0 && step == 0 {
        bail!("Settings bytecode gives one instance two values of a property");
    }
    previous
        .checked_add(step)
        .context("Property instance index delta overflow")
}

pub(crate) fn encode_settings_bytecode(document: &SettingsBytecode) -> Result<Vec<u8>> {
    let _span = crate::app::profile::span("store encode");
    blocks::encode_document(document, true, None)
}

#[cfg(test)]
pub(crate) fn encode_settings_bytecode_with_dense_references(
    document: &SettingsBytecode,
) -> Result<Vec<u8>> {
    let _span = crate::app::profile::span("store encode");
    blocks::encode_document(document, false, None)
}

/// `encode_settings_bytecode`, or the dense-references encoder where `dense`
/// says so, for a store whose file holds `previous`: the same bytes, each
/// frame that comes out as that file holds it copied from it where the same
/// writer wrote it.
pub(crate) fn encode_settings_bytecode_over(
    document: &SettingsBytecode,
    dense: bool,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let _span = crate::app::profile::span("store encode");
    blocks::encode_document(document, !dense, previous)
}

/// The v11 encoder, which the tests keep to write the stores every reader
/// still reads.
#[cfg(test)]
pub(crate) fn encode_settings_bytecode_v11(document: &SettingsBytecode) -> Result<Vec<u8>> {
    let document = settled_settings_document(document)?;
    wrap_settings_bytecode_payload(&encode_settings_bytecode_payload(&document)?)
}

/// `document` as a store holds it: refused where no store can hold it, its
/// attributes in their typed forms and its forced properties settled.
fn settled_settings_document(document: &SettingsBytecode) -> Result<Cow<'_, SettingsBytecode>> {
    validate_settings_hierarchy(&document.instances)?;
    validate_settings_record_properties(&document.instances)?;
    if document
        .instances
        .iter()
        .any(|instance| crate::settings::held::stands_unchanged(&instance.properties))
    {
        bail!("A document whose instances stand for unchanged counterparts is not written");
    }
    let document =
        canonical_attribute_document(document).map_or(Cow::Borrowed(document), Cow::Owned);
    Ok(forced_properties_settled_document(&document).map_or(document, Cow::Owned))
}

/// Refuses a property a store cannot give back: one named for a field of the
/// instance record, which a store keeps only in the record, and an object
/// named Attributes, which a store reads back as the instance's attributes.
fn validate_settings_record_properties(instances: &[SettingsBytecodeInstance]) -> Result<()> {
    let lookup = SettingsBinaryInstanceLookup::default();
    for instance in instances {
        let held = |name: &str| {
            instance
                .properties
                .get(name)
                .filter(|value| !value.is_null())
        };
        if let Some(field) = SETTINGS_RECORD_FIELDS
            .into_iter()
            .find(|field| held(field).is_some())
        {
            bail!(
                "Instance {} ({}) holds a {field} property; a store keeps the instance's {field} in its record",
                instance.settings_id,
                instance.name
            );
        }
        if held("Attributes").is_some_and(|value| {
            ref_payload_object(value).is_none()
                && matches!(binary_raw_value_kind(value, &lookup), Ok(8))
        }) {
            bail!(
                "Instance {} ({}) holds an object property named Attributes, which a store reads back as its attributes; set them as attributes",
                instance.settings_id,
                instance.name
            );
        }
    }
    Ok(())
}

fn validate_settings_hierarchy(instances: &[SettingsBytecodeInstance]) -> Result<()> {
    let mut settings_ids = AHashSet::with_capacity(instances.len());
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

#[cfg(test)]
fn collect_settings_bytecode_chunk<'a>(
    lookup: &SettingsBinaryInstanceLookup<'_>,
    instances: &'a [SettingsBytecodeInstance],
    base_index: usize,
) -> Result<SettingsBinaryCollection<'a>> {
    let mut out = SettingsBinaryCollection::default();
    for (offset, instance) in instances.iter().enumerate() {
        collect_settings_record_strings(&mut out, instance);
        collect_settings_instance_values(&mut out, lookup, base_index + offset, instance)?;
    }
    Ok(out)
}

/// Counts the strings of an instance's record: its id, name and class.
fn collect_settings_record_strings<'a>(
    out: &mut SettingsBinaryCollection<'a>,
    instance: &'a SettingsBytecodeInstance,
) {
    if parse_numeric_debug_settings_id(&instance.settings_id).is_none() {
        add_count(&mut out.string_counts, instance.settings_id.as_str());
    }
    add_count(&mut out.string_counts, instance.name.as_str());
    add_count(&mut out.string_counts, instance.class_name.as_str());
    add_count(&mut out.class_counts, instance.class_name.as_str());
}

/// Collects an instance's properties and attributes as the values of the
/// row `instance_index`.
fn collect_settings_instance_values<'a>(
    out: &mut SettingsBinaryCollection<'a>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    instance_index: usize,
    instance: &'a SettingsBytecodeInstance,
) -> Result<()> {
    for (property_name, raw_value) in &instance.properties {
        let source = SettingsBinaryValueSource::of_property(raw_value);
        let kind = binary_source_value_kind(&source, lookup)?;
        if kind == 0 {
            continue;
        }
        let property_name = property_name.as_str();
        push_settings_binary_value(out, lookup, instance_index, property_name, kind, source)?;
    }

    if !instance.attributes.is_empty() {
        let source = SettingsBinaryValueSource::Attributes(&instance.attributes);
        let kind = binary_source_value_kind(&source, lookup)?;
        push_settings_binary_value(out, lookup, instance_index, "Attributes", kind, source)?;
    }
    Ok(())
}

#[cfg(test)]
fn collect_settings_bytecode_data<'a>(
    document: &'a SettingsBytecode,
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<SettingsBinaryCollection<'a>> {
    let _span = crate::app::profile::span("store columns collect");
    if document.instances.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
        || rayon::current_num_threads() <= 1
    {
        return collect_settings_bytecode_chunk(lookup, &document.instances, 0);
    }

    let partials = document
        .instances
        .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
        .enumerate()
        .map(|(chunk_index, instances)| {
            collect_settings_bytecode_chunk(
                lookup,
                instances,
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

#[cfg(test)]
fn encode_settings_bytecode_payload(document: &SettingsBytecode) -> Result<Vec<u8>> {
    let lookup = build_bytecode_instance_lookup(document);
    let collected = collect_settings_bytecode_data(document, &lookup)?;
    encode_settings_bytecode_payload_collected(document, &lookup, collected)
}

/// The lookup that finds the instances references name: by every selector
/// the document's values use, or by instanceIndex alone where `by_selectors`
/// says otherwise.
fn document_reference_lookup(
    document: &SettingsBytecode,
    by_selectors: bool,
) -> SettingsBinaryInstanceLookup<'_> {
    if by_selectors {
        return build_bytecode_instance_lookup(document);
    }
    SettingsBinaryInstanceLookup {
        dense_instance_count: document.instances.len(),
        dense_instances: Some(&document.instances),
        root_service: document_root_service(document),
        ..Default::default()
    }
}

fn encode_settings_bytecode_payload_collected(
    document: &SettingsBytecode,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    collected: SettingsBinaryCollection<'_>,
) -> Result<Vec<u8>> {
    let _span = crate::app::profile::span("store payload encode");
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
    if document.instances.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
        && rayon::current_num_threads() > 1
    {
        let bodies = document
            .instances
            .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
            .enumerate()
            .map(|(chunk_index, instances)| {
                let base_index = chunk_index * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE;
                let mut body = Vec::with_capacity(instances.len().saturating_mul(12));
                for (offset, instance) in instances.iter().enumerate() {
                    write_compact_settings_id(
                        &mut body,
                        &string_ids,
                        instance.settings_id.as_str(),
                    )?;
                    write_binary_string_id(&mut body, &string_ids, instance.name.as_str())?;
                    write_lookup_id(&mut body, &class_ids, instance.class_name.as_str(), "class")?;
                    write_var_u64(
                        &mut body,
                        encode_parent_index(instance.parent_index, base_index + offset)?,
                    )?;
                }
                Ok(body)
            })
            .collect::<Result<Vec<_>>>()?;
        for body in bodies {
            writer.extend_from_slice(&body);
        }
    } else {
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
    }
    write_settings_binary_property_groups(
        &mut writer,
        &property_group_entries,
        &property_ids,
        &string_ids,
        lookup,
    )?;

    Ok(writer)
}

/// The level every store is compressed at. Level 6 with long-distance
/// matching on several threads takes the time level 1 took on one, and a
/// large store comes out a sixth smaller.
const STORE_COMPRESSION_LEVEL: i32 = 6;
/// How far back a match may lie: a store repeats values megabytes apart.
/// Every zstd reader accepts this window without being told to.
const STORE_WINDOW_LOG: u32 = 27;
/// A payload from this size on is searched for distant matches and split
/// over threads. What a store's bytes are depends on its size alone, never on
/// the machine: zstd cuts the work by size, not by how many threads take it.
const LARGE_STORE_PAYLOAD_BYTES: usize = 1 << 20;

#[cfg(test)]
fn compression_workers() -> u32 {
    thread::available_parallelism().map_or(1, |cores| cores.get().min(8) as u32)
}

#[cfg(test)]
fn compress_store_payload(payload: &[u8], workers: u32) -> Result<Vec<u8>> {
    compress_store_frame(payload, workers, false)
}

/// `compress_store_payload`, with zstd's checksum of the content in the frame
/// where `checksum` says so.
fn compress_store_frame(payload: &[u8], workers: u32, checksum: bool) -> Result<Vec<u8>> {
    use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer, zstd_sys::ZSTD_EndDirective};
    if payload.len() < LARGE_STORE_PAYLOAD_BYTES {
        if !checksum {
            return Ok(zstd::bulk::compress(payload, STORE_COMPRESSION_LEVEL)?);
        }
        let mut compressor = zstd::bulk::Compressor::new(STORE_COMPRESSION_LEVEL)?;
        compressor.set_parameter(CParameter::ChecksumFlag(true))?;
        return Ok(compressor.compress(payload)?);
    }
    // The frame leaves the compressor a piece at a time: one call with room
    // for the whole of it reserves as many bytes as the payload has, several
    // times what a store compresses to.
    let failed = |code| anyhow::anyhow!("{}", zstd::zstd_safe::get_error_name(code));
    let mut context = CCtx::create();
    for parameter in [
        CParameter::CompressionLevel(STORE_COMPRESSION_LEVEL),
        CParameter::EnableLongDistanceMatching(true),
        CParameter::WindowLog(STORE_WINDOW_LOG),
        CParameter::NbWorkers(workers.max(1)),
        CParameter::ChecksumFlag(checksum),
    ] {
        context.set_parameter(parameter).map_err(failed)?;
    }
    context
        .set_pledged_src_size(Some(payload.len() as u64))
        .map_err(failed)?;
    let mut input = InBuffer::around(payload);
    let mut piece = vec![0_u8; 1 << 20];
    let mut compressed = Vec::with_capacity(payload.len() / 4);
    loop {
        let mut output = OutBuffer::around(&mut piece[..]);
        let left = context
            .compress_stream2(&mut output, &mut input, ZSTD_EndDirective::ZSTD_e_end)
            .map_err(failed)?;
        compressed.extend_from_slice(output.as_slice());
        if left == 0 {
            return Ok(compressed);
        }
    }
}

/// `compress_store_payload` as one call with room for the whole frame.
#[cfg(test)]
fn compress_store_payload_at_once(payload: &[u8], workers: u32) -> Result<Vec<u8>> {
    use zstd::zstd_safe::CParameter;
    let mut compressor = zstd::bulk::Compressor::new(STORE_COMPRESSION_LEVEL)?;
    if payload.len() >= LARGE_STORE_PAYLOAD_BYTES {
        compressor.set_parameter(CParameter::EnableLongDistanceMatching(true))?;
        compressor.set_parameter(CParameter::WindowLog(STORE_WINDOW_LOG))?;
        compressor.set_parameter(CParameter::NbWorkers(workers.max(1)))?;
    }
    Ok(compressor.compress(payload)?)
}

#[cfg(test)]
fn wrap_settings_bytecode_payload(payload: &[u8]) -> Result<Vec<u8>> {
    let _span = crate::app::profile::span("store compress");
    check_settings_payload_len(payload.len(), "Decoded")?;
    let zstd = compress_store_payload(payload, compression_workers())?;
    check_settings_payload_len(zstd.len(), "Compressed")?;
    let mut writer = Vec::with_capacity(
        SETTINGS_BINARY_MAGIC.len()
            + 1
            + var_u64_len(payload.len() as u64)
            + var_u64_len(zstd.len() as u64)
            + zstd.len(),
    );
    writer.extend_from_slice(SETTINGS_BINARY_MAGIC);
    writer.push(SETTINGS_COLUMN_VERSION);
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

fn document_root_service(document: &SettingsBytecode) -> Option<String> {
    document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
}

fn build_bytecode_instance_lookup(document: &SettingsBytecode) -> SettingsBinaryInstanceLookup<'_> {
    let root_service = document_root_service(document);
    let selectors = settings_document_reference_selectors(document, root_service.as_deref());
    instance_lookup(document, root_service, selectors)
}

/// `build_bytecode_instance_lookup` of a document whose values use every
/// selector. It finds what that lookup finds for each reference that a
/// document's values hold: a reference that needs a selector makes its
/// document use it, and a settings id beside an index finds that index or
/// none either way, the ids being unique.
fn build_bytecode_instance_lookup_by_every_selector(
    document: &SettingsBytecode,
) -> SettingsBinaryInstanceLookup<'_> {
    let every = SettingsReferenceSelectors {
        settings_id: true,
        path: true,
    };
    instance_lookup(document, document_root_service(document), every)
}

fn instance_lookup(
    document: &SettingsBytecode,
    root_service: Option<String>,
    selectors: SettingsReferenceSelectors,
) -> SettingsBinaryInstanceLookup<'_> {
    let _span = crate::app::profile::span("store reference indexes");
    let mut lookup = SettingsBinaryInstanceLookup {
        dense_instance_count: document.instances.len(),
        dense_instances: Some(&document.instances),
        root_service,
        by_settings_id: if selectors.settings_id {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        by_path: if selectors.path {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        by_path_segments: if selectors.path {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        by_path_parts: if selectors.path {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        ..Default::default()
    };
    let paths = selectors
        .path
        .then(|| settings_document_path_parts(document));
    for (index, instance) in document.instances.iter().enumerate() {
        if selectors.settings_id {
            lookup
                .by_settings_id
                .entry(Cow::Borrowed(instance.settings_id.as_str()))
                .or_insert(index);
        }
        if let Some((segments, ordinals)) = paths.as_ref().and_then(|paths| paths.get(index)) {
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

#[derive(Default)]
struct SettingsReferenceSelectors {
    settings_id: bool,
    path: bool,
}

fn settings_document_reference_selectors(
    document: &SettingsBytecode,
    root_service: Option<&str>,
) -> SettingsReferenceSelectors {
    if document.instances.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
        || rayon::current_num_threads() <= 1
    {
        return settings_instance_reference_selectors(&document.instances, root_service);
    }
    document
        .instances
        .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
        .map(|instances| settings_instance_reference_selectors(instances, root_service))
        .reduce(SettingsReferenceSelectors::default, |left, right| {
            SettingsReferenceSelectors {
                settings_id: left.settings_id || right.settings_id,
                path: left.path || right.path,
            }
        })
}

fn settings_instance_reference_selectors(
    instances: &[SettingsBytecodeInstance],
    root_service: Option<&str>,
) -> SettingsReferenceSelectors {
    let mut selectors = SettingsReferenceSelectors::default();
    for instance in instances {
        for value in instance
            .properties
            .values()
            .chain(instance.attributes.values())
        {
            collect_settings_reference_selectors(value, root_service, &mut selectors);
            if selectors.settings_id && selectors.path {
                return selectors;
            }
        }
    }
    selectors
}

fn collect_settings_reference_selectors(
    value: &Value,
    root_service: Option<&str>,
    selectors: &mut SettingsReferenceSelectors,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_settings_reference_selectors(value, root_service, selectors);
            }
        }
        Value::Object(object) => {
            if let Some(reference) = ref_payload_object(value) {
                // Resolution checks the path before consulting any local index.
                // An outside path either stays outside or fails validation; its
                // other selectors cannot make a local lookup useful.
                if root_service.is_some_and(|service| {
                    reference
                        .get("pathSegments")
                        .and_then(Value::as_array)
                        .and_then(|segments| segments.first())
                        .and_then(Value::as_str)
                        .is_some_and(|root| root != service)
                }) {
                    return;
                }
                selectors.settings_id |= reference.get("instanceIndex").is_none()
                    && ["settingsId", "referent", "ref"]
                        .iter()
                        .any(|key| reference.contains_key(*key));
                selectors.path |= ["path", "pathSegments", "pathOrdinals"]
                    .iter()
                    .any(|key| reference.contains_key(*key));
            } else {
                for value in object.values() {
                    collect_settings_reference_selectors(value, root_service, selectors);
                }
            }
        }
        _ => {}
    }
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

    fn bytes(&self) -> &'a [u8] {
        self.cursor.get_ref()
    }

    fn position(&self) -> usize {
        self.cursor.position() as usize
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

    /// The room to reserve for `count` items that each take at least
    /// `item_bytes` of the remaining input: a count the input cannot hold
    /// reserves nothing beyond what it can.
    fn capacity(&self, count: usize, item_bytes: usize) -> usize {
        let remaining = self.cursor.get_ref().len() - self.position();
        count.min(remaining / item_bytes.max(1))
    }

    fn read_collection_len(&mut self, label: &str) -> Result<usize> {
        let len = self.read_len(label)?;
        checked_settings_collection_len(len, label)
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

fn decode_raw_value(reader: &mut BytecodeReader<'_>, scope: ValueScope<'_>) -> Result<Value> {
    decode_raw_value_at_depth(reader, scope, 0)
}

fn decode_raw_value_at_depth(
    reader: &mut BytecodeReader<'_>,
    scope: ValueScope<'_>,
    depth: usize,
) -> Result<Value> {
    if depth > MAX_SETTINGS_VALUE_DEPTH {
        bail!("Settings bytecode value nesting exceeds safe depth of {MAX_SETTINGS_VALUE_DEPTH}");
    }
    let kind = reader.read_u8().context("Missing value kind")?;
    scope.budget.take(value_kind_bytes(kind))?;
    decode_raw_value_payload(reader, kind, scope, depth)
}

fn decode_raw_value_payload(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    scope: ValueScope<'_>,
    depth: usize,
) -> Result<Value> {
    let ValueScope {
        instance_count,
        budget,
        ..
    } = scope;
    match kind {
        0 => Ok(Value::Null),
        1 => Ok(Value::Bool(false)),
        2 => Ok(Value::Bool(true)),
        3 => Ok(json!(unzigzag_i64(reader.read_var_u64()?,))),
        4 => Ok(json!(reader.read_var_u64()?)),
        5 => Ok(json_number_f64(read_f64(reader)?)),
        6 => Ok(Value::String(scope.string(reader, "value string id")?)),
        7 => {
            let len = reader.read_collection_len("array length")?;
            budget.take(len.saturating_mul(VALUE_SLOT_BYTES))?;
            let mut items = Vec::with_capacity(reader.capacity(len, 1));
            for _ in 0..len {
                items.push(decode_raw_value_at_depth(reader, scope, depth + 1)?);
            }
            Ok(Value::Array(items))
        }
        8 => decode_object_payload(reader, scope, depth),
        9 => {
            let len = reader.read_len("numeric array length")?;
            budget.numeric_array(len, kind)?;
            let mut items = Vec::with_capacity(reader.capacity(len, 8));
            for _ in 0..len {
                items.push(json_number_f64(read_f64(reader)?));
            }
            Ok(Value::Array(items))
        }
        20 => Ok(json_number_f64(read_f32(reader)? as f64)),
        21 => {
            let len = reader.read_len("f32 numeric array length")?;
            budget.numeric_array(len, kind)?;
            let mut items = Vec::with_capacity(reader.capacity(len, 4));
            for _ in 0..len {
                items.push(json_number_f64(read_f32(reader)? as f64));
            }
            Ok(Value::Array(items))
        }
        10 => decode_ref_fallback_payload(reader, scope),
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
        17 => decoded_reference(reader.read_len("resolved ref index")?, instance_count),
        18 => decode_fixed_numeric_payload(reader, "Rect", &["minX", "minY", "maxX", "maxY"]),
        19 => Ok(typed_object(
            "EnumItem",
            [(
                "name",
                Value::String(scope.string(reader, "enum item string id")?),
            )],
        )),
        NESTED_ENUM_ITEM_KIND => {
            let enum_type = scope.string(reader, "nested enum type string id")?;
            let name = scope.string(reader, "nested enum item string id")?;
            Ok(typed_object(
                "EnumItem",
                [
                    ("enumType", Value::String(enum_type)),
                    ("name", Value::String(name)),
                ],
            ))
        }
        _ => bail!("Unknown settings bytecode value kind {kind}"),
    }
}

fn decoded_reference(index: usize, instance_count: usize) -> Result<Value> {
    check_resolved_ref_index(index, instance_count)?;
    Ok(typed_object("Ref", [("instanceIndex", json!(index + 1))]))
}

fn check_resolved_ref_index(index: usize, instance_count: usize) -> Result<()> {
    if index >= instance_count {
        bail!("Invalid resolved ref index {index}");
    }
    Ok(())
}

fn decode_object_payload(
    reader: &mut BytecodeReader<'_>,
    scope: ValueScope<'_>,
    depth: usize,
) -> Result<Value> {
    let field_count = reader.read_collection_len("object field count")?;
    scope
        .budget
        .take(field_count.saturating_mul(VALUE_SLOT_BYTES))?;
    let mut out = Map::new();
    for _ in 0..field_count {
        let key = scope.string(reader, "object key string id")?;
        let value = decode_raw_value_at_depth(reader, scope, depth + 1)?;
        out.insert(key, value);
    }
    // A handle stands for a payload this process holds; a file has none.
    if crate::settings::held::is_handle(&out) {
        bail!("Settings bytecode holds the handle of a payload in place of the payload");
    }
    Ok(Value::Object(out))
}

fn decode_attributes_payload(
    reader: &mut BytecodeReader<'_>,
    scope: ValueScope<'_>,
) -> Result<Map<String, Value>> {
    let field_count = reader.read_collection_len("attribute count")?;
    scope
        .budget
        .take(field_count.saturating_mul(VALUE_SLOT_BYTES))?;
    let mut out = Map::new();
    for _ in 0..field_count {
        let key = scope.string(reader, "attribute key string id")?;
        let value = decode_raw_value(reader, scope)?;
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
    scope: ValueScope<'_>,
) -> Result<Value> {
    let budget = scope.budget;
    let flags = reader.read_var_u64().context("Missing ref flags")?;
    let mut out = Map::new();
    out.insert("_type".to_string(), Value::String("Ref".to_string()));
    if flags & 1 != 0 {
        out.insert(
            "instanceId".to_string(),
            Value::String(scope.string(reader, "ref instance id")?),
        );
    }
    if flags & 2 != 0 {
        out.insert(
            "debugId".to_string(),
            Value::String(scope.string(reader, "ref debug id")?),
        );
    }
    if flags & 4 != 0 {
        let len = reader.read_collection_len("ref path segment count")?;
        budget.take(len.saturating_mul(VALUE_SLOT_BYTES))?;
        let mut segments = Vec::with_capacity(reader.capacity(len, 1));
        for _ in 0..len {
            segments.push(Value::String(scope.string(reader, "ref path segment")?));
        }
        out.insert("pathSegments".to_string(), Value::Array(segments));
    }
    if flags & 8 != 0 {
        out.insert(
            "path".to_string(),
            Value::String(scope.string(reader, "ref path")?),
        );
    }
    if flags & 16 != 0 {
        out.insert(
            "settingsId".to_string(),
            Value::String(scope.string(reader, "ref settings id")?),
        );
    }
    if flags & 32 != 0 {
        let len = reader.read_collection_len("ref path ordinal count")?;
        budget.take(len.saturating_mul(VALUE_SLOT_BYTES))?;
        let mut ordinals = Vec::with_capacity(reader.capacity(len, 1));
        for _ in 0..len {
            ordinals.push(json!(
                reader.read_var_u64().context("Missing ref path ordinal")?
            ));
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
            let rotation_count =
                checked_cframe_rotation_count(reader.read_len("CFrame rotation table count")?)?;
            let mut rotations = Vec::with_capacity(reader.capacity(rotation_count, 36));
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
    let target_index = decode_resolved_ref_target(reader, instance_count, previous_target_index)?;
    Ok(typed_object(
        "Ref",
        [("instanceIndex", json!(target_index + 1))],
    ))
}

fn decode_resolved_ref_target(
    reader: &mut BytecodeReader<'_>,
    instance_count: usize,
    previous_target_index: &mut usize,
) -> Result<usize> {
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
    check_resolved_ref_index(target_index, instance_count)?;
    *previous_target_index = target_index;
    Ok(target_index)
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
type SettingsStringCounts<'a> = AHashMap<SettingsString<'a>, u64>;
type SettingsPropertyGroups<'a> = AHashMap<(&'a str, u8), Vec<SettingsBinaryValue<'a>>>;
type SettingsStringIdMap<'a> = AHashMap<&'a str, u64>;
type SettingsIndexMap<K> = AHashMap<K, usize>;

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

// A store of 200,000 instances collects millions of these before it is
// written, so each stays three words: the rare settled value is boxed.
const _: () = assert!(std::mem::size_of::<SettingsBinaryValue<'static>>() <= 24);

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
    /// A property value as `settled_property_value` gives it.
    Settled(Box<Value>),
    Native(&'a NativeSettingsValue),
    Attributes(&'a Map<String, Value>),
}

impl<'a> SettingsBinaryValueSource<'a> {
    fn of_property(value: &'a Value) -> Self {
        settled_property_value(value).map_or(Self::Property(value), |settled| {
            Self::Settled(Box::new(settled))
        })
    }
}

#[derive(Default)]
struct SettingsBinaryInstanceLookup<'a> {
    dense_instance_count: usize,
    dense_instances: Option<&'a [SettingsBytecodeInstance]>,
    /// The service this store describes; a path into another service is an
    /// outside reference whatever the other selectors say.
    root_service: Option<String>,
    by_instance_index: SettingsIndexMap<usize>,
    by_settings_id: SettingsIndexMap<Cow<'a, str>>,
    by_instance_id: SettingsIndexMap<String>,
    by_debug_id: SettingsIndexMap<String>,
    by_path: AHashMap<String, Option<usize>>,
    by_path_segments: AHashMap<String, Option<usize>>,
    by_path_parts: AHashMap<String, Option<usize>>,
}

pub(crate) const SETTINGS_BINARY_PARALLEL_MIN_INSTANCES: usize = 2_048;
pub(crate) const SETTINGS_BINARY_PARALLEL_CHUNK_SIZE: usize = 4096;

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
    lookup: &SettingsBinaryInstanceLookup<'_>,
    instance_index: usize,
    property_name: &'a str,
    kind: u8,
    source: SettingsBinaryValueSource<'a>,
) -> Result<()> {
    add_count(&mut out.string_counts, property_name);
    add_count(&mut out.property_counts, property_name);
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

/// One instance whose values the encoder takes from a document's instance
/// in place of the captured state's.
#[derive(Clone, Copy)]
struct ReplacedRow<'a> {
    source_index: usize,
    instance: &'a SettingsBytecodeInstance,
}

fn visit_replaced_row<'a>(
    instance: &'a SettingsBytecodeInstance,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    mut visit: impl FnMut(&'a str, u8, SettingsBinaryValueSource<'a>) -> Result<()>,
) -> Result<()> {
    for (name, value) in &instance.properties {
        let source = SettingsBinaryValueSource::of_property(value);
        let kind = binary_source_value_kind(&source, lookup)?;
        if kind != 0 {
            visit(name.as_str(), kind, source)?;
        }
    }
    if !instance.attributes.is_empty() {
        let source = SettingsBinaryValueSource::Attributes(&instance.attributes);
        let kind = binary_source_value_kind(&source, lookup)?;
        visit("Attributes", kind, source)?;
    }
    Ok(())
}

fn collect_settings_binary_chunk<'a>(
    state: &'a ServiceState,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    instances: &'a [SettingsBinaryInstance<'a>],
    base_index: usize,
    replaced: Option<ReplacedRow<'a>>,
) -> Result<SettingsBinaryCollection<'a>> {
    let mut out = SettingsBinaryCollection::default();
    for (offset, record) in instances.iter().enumerate() {
        collect_settings_binary_record_strings(&mut out, record);
        collect_settings_binary_record_values(
            &mut out,
            (state, lookup),
            base_index + offset,
            record,
            replaced,
        )?;
    }
    Ok(out)
}

/// Counts the strings of a captured instance's record: its id, name and
/// class.
fn collect_settings_binary_record_strings<'a>(
    out: &mut SettingsBinaryCollection<'a>,
    record: &'a SettingsBinaryInstance<'a>,
) {
    if let SettingsBinaryId::Text(settings_id) = &record.settings_id {
        add_count(&mut out.string_counts, settings_id.as_ref());
    }
    add_count(&mut out.string_counts, record.name);
    add_count(&mut out.string_counts, record.class_name);
    add_count(&mut out.class_counts, record.class_name);
}

/// Collects a captured instance's values, or those of the row that replaces
/// it, as the values of the row `instance_index`.
fn collect_settings_binary_record_values<'a>(
    out: &mut SettingsBinaryCollection<'a>,
    (state, lookup): (&'a ServiceState, &SettingsBinaryInstanceLookup<'_>),
    instance_index: usize,
    record: &SettingsBinaryInstance<'_>,
    replaced: Option<ReplacedRow<'a>>,
) -> Result<()> {
    let push = |name, kind, source| {
        push_settings_binary_value(out, lookup, instance_index, name, kind, source)
    };
    match replaced.filter(|row| row.source_index == record.source_index) {
        Some(row) => visit_replaced_row(row.instance, lookup, push),
        None => visit_settings_binary_properties(state, record, lookup, push),
    }
}

fn visit_settings_binary_properties<'a>(
    state: &'a ServiceState,
    record: &SettingsBinaryInstance<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    mut visit: impl FnMut(&'a str, u8, SettingsBinaryValueSource<'a>) -> Result<()>,
) -> Result<()> {
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
            if !state.properties_default_elided
                && let NativeSettingsValue::UniqueId(value) = &property.value
                && should_skip_binary_property(
                    state,
                    instance,
                    property_name,
                    &serde_json::json!({"_type": "UniqueId", "value": value}),
                )
            {
                continue;
            }
            // A shared value is the JSON value it stands for, to every reader.
            if let NativeSettingsValue::Shared(value) = &property.value {
                if should_skip_binary_property(state, instance, property_name, value) {
                    continue;
                }
                let source = SettingsBinaryValueSource::of_property(value);
                let kind = binary_source_value_kind(&source, lookup)?;
                if kind != 0 {
                    visit(property_name, kind, source)?;
                }
                continue;
            }
            // A decoder discards the record's own fields, and reads a float
            // that is no number back as the value that names it: both are
            // written as the encoder of documents writes them. So is a value
            // of several floats of which one is no number: a store holds that
            // one in one form, whatever bits Studio gave it.
            if SETTINGS_RECORD_FIELDS.contains(&property_name) {
                continue;
            }
            let no_number = match &property.value {
                NativeSettingsValue::Float32(value) if !value.is_finite() => {
                    Some(nonfinite_float_json(f64::from(*value)))
                }
                NativeSettingsValue::Float64(value) if !value.is_finite() => {
                    Some(nonfinite_float_json(*value))
                }
                value
                    if native_fixed_components(value)
                        .is_some_and(|floats| floats.iter().any(|float| float.is_nan())) =>
                {
                    captured::fixed_native_value(value)
                }
                _ => None,
            };
            let source = match no_number {
                Some(value) => SettingsBinaryValueSource::Settled(Box::new(value)),
                None => SettingsBinaryValueSource::Native(&property.value),
            };
            let kind = binary_source_value_kind(&source, lookup)?;
            visit(property_name, kind, source)?;
        }
    }
    for (property_name, raw_value) in &instance.properties {
        if should_skip_binary_property(state, instance, property_name, raw_value) {
            continue;
        }
        let property_name = property_name.as_str();
        let source = SettingsBinaryValueSource::of_property(raw_value);
        let kind = binary_source_value_kind(&source, lookup)?;
        if kind == 0 {
            continue;
        }
        visit(property_name, kind, source)?;
    }

    if !instance.attributes.is_empty() {
        let property_name = "Attributes";
        let source = SettingsBinaryValueSource::Attributes(&instance.attributes);
        let kind = binary_source_value_kind(&source, lookup)?;
        visit(property_name, kind, source)?;
    }
    Ok(())
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
    instances: &'a [SettingsBinaryInstance<'a>],
    replaced: Option<ReplacedRow<'a>>,
) -> Result<SettingsBinaryCollection<'a>> {
    let _span = crate::app::profile::span("captured store columns collect");
    if instances.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES || rayon::current_num_threads() <= 1
    {
        return collect_settings_binary_chunk(state, lookup, instances, 0, replaced);
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
                replaced,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let mut out = SettingsBinaryCollection::default();
    for partial in partials {
        merge_settings_binary_collection(&mut out, partial);
    }
    Ok(out)
}

#[cfg(test)]
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

    let writer = encode_service_settings_binary(state)?;
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
    Ok(())
}

pub(crate) fn encode_service_settings_binary(state: &ServiceState) -> Result<Vec<u8>> {
    encode_service_settings_binary_over(state, None)
}

/// The v11 encoder of captured states, which the tests keep beside the v11
/// encoder of documents.
#[cfg(test)]
pub(crate) fn encode_service_settings_binary_v11(state: &ServiceState) -> Result<Vec<u8>> {
    let instances = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &instances);
    let collected = collect_settings_binary_data(state, &lookup, &instances, None)?;
    wrap_settings_bytecode_payload(&encode_prepared_service_payload(
        &instances, &lookup, collected,
    )?)
}

/// `encode_service_settings_binary` for a store whose file holds `previous`
/// (see `encode_settings_bytecode_over`).
pub(crate) fn encode_service_settings_binary_over(
    state: &ServiceState,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let _span = crate::app::profile::span("captured store encode");
    let instances = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &instances);
    blocks::encode_state(state, (&instances, &lookup), None, previous)
}

/// The payload of a captured store before it is compressed: in a v12 store,
/// its structure, from a collection of the records alone.
fn encode_prepared_service_payload(
    instances: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    collected: SettingsBinaryCollection<'_>,
) -> Result<Vec<u8>> {
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
        lookup,
    )?;
    Ok(payload)
}

fn collect_service_settings_binary_instances(
    state: &ServiceState,
) -> Vec<SettingsBinaryInstance<'_>> {
    let _span = crate::app::profile::span("captured store records");
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
    let mut visited = vec![false; state.instances.len()];
    let mut stack = vec![(state.service_root_index, None)];
    while let Some((source_index, parent_index)) = stack.pop() {
        if source_index >= state.instances.len()
            || std::mem::replace(&mut visited[source_index], true)
        {
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

/// An id a store holds, as the encoder of documents writes it: one of
/// Studio's ids is the number it names.
fn settings_binary_id_of<'a>(id: &str) -> SettingsBinaryId<'a> {
    match parse_numeric_debug_settings_id(id) {
        Some(value) => SettingsBinaryId::NumericDebug(value),
        None => SettingsBinaryId::Text(Cow::Owned(id.to_string())),
    }
}

fn settings_binary_id(source_index: usize, instance: &SnapshotInstance) -> SettingsBinaryId<'_> {
    if instance.instance_index == Some(1) && instance.parent_index.is_none() {
        return SettingsBinaryId::Text(Cow::Borrowed("1"));
    }
    if let Some(settings_id) = instance
        .transported_settings_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        // One of Studio's ids that came along as text is the number it names.
        return match parse_numeric_debug_settings_id(settings_id) {
            Some(value) => SettingsBinaryId::NumericDebug(value),
            None => SettingsBinaryId::Text(Cow::Borrowed(settings_id)),
        };
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

fn build_settings_binary_instance_lookup<'a>(
    state: &'a ServiceState,
    instances: &[SettingsBinaryInstance<'a>],
) -> SettingsBinaryInstanceLookup<'a> {
    let _span = crate::app::profile::span("captured store reference indexes");
    let root_service = state
        .instances
        .get(state.service_root_index)
        .map(|instance| instance.name.clone());
    if state.dense_index_topology {
        return SettingsBinaryInstanceLookup {
            dense_instance_count: instances.len(),
            root_service,
            ..Default::default()
        };
    }
    let mut by_instance_index = AHashMap::with_capacity(instances.len());
    let mut by_settings_id = if state.properties_default_elided {
        AHashMap::new()
    } else {
        AHashMap::with_capacity(instances.len())
    };
    let mut by_instance_id = if state.properties_default_elided {
        AHashMap::new()
    } else {
        AHashMap::with_capacity(instances.len())
    };
    let mut by_debug_id = if state.properties_default_elided {
        AHashMap::new()
    } else {
        AHashMap::with_capacity(instances.len() / 4)
    };
    let mut by_path: AHashMap<String, Option<usize>> = if state.properties_default_elided {
        AHashMap::new()
    } else {
        AHashMap::with_capacity(instances.len())
    };
    let mut by_path_segments: AHashMap<String, Option<usize>> = if state.properties_default_elided {
        AHashMap::new()
    } else {
        AHashMap::with_capacity(instances.len())
    };
    let mut by_path_parts: AHashMap<String, Option<usize>> = if state.properties_default_elided {
        AHashMap::new()
    } else {
        AHashMap::with_capacity(instances.len())
    };
    let path_parts = settings_binary_path_parts(instances);

    for (binary_index, record) in instances.iter().enumerate() {
        let instance = &state.instances[record.source_index];
        by_settings_id
            .entry(Cow::Owned(instance_settings_id(
                record.source_index,
                instance,
            )))
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
        dense_instances: None,
        root_service,
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

fn insert_unique_path(map: &mut AHashMap<String, Option<usize>>, key: String, index: usize) {
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
        let mut remaining = names.len();
        while let Some(value) = current {
            if value >= names.len() || remaining == 0 {
                break;
            }
            remaining -= 1;
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

fn add_count<'a>(counts: &mut SettingsStringCounts<'a>, text: &'a str) {
    if let Some(count) = counts.get_mut(text) {
        *count += 1;
    } else {
        counts.insert(Cow::Borrowed(text), 1);
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
    let order = |a: &(SettingsString<'_>, u64), b: &(SettingsString<'_>, u64)| {
        b.1.cmp(&a.1).then_with(|| a.0.as_ref().cmp(b.0.as_ref()))
    };
    if items.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES && rayon::current_num_threads() > 1 {
        // Counts have unique string keys, so the tie breaker makes this a total
        // order independent of the map's iteration order or worker count.
        items.par_sort_unstable_by(order);
    } else {
        items.sort_by(order);
    }
    items.into_iter().map(|(text, _)| text).collect()
}

fn build_id_map<'a>(items: &'a [SettingsString<'_>]) -> SettingsStringIdMap<'a> {
    let mut out = AHashMap::with_capacity(items.len());
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

fn write_settings_binary_property_groups(
    writer: &mut Vec<u8>,
    groups: &[((&str, u8), Vec<SettingsBinaryValue<'_>>)],
    property_ids: &SettingsStringIdMap<'_>,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<()> {
    let _span = crate::app::profile::span("store property columns encode");
    write_var_u64(writer, groups.len() as u64)?;
    let bodies = groups
        .par_iter()
        .map(|((_, kind), values)| {
            let mut body = Vec::new();
            write_property_group_values(values, *kind, string_ids, lookup, &mut body)?;
            Ok(body)
        })
        .collect::<Result<Vec<_>>>()?;
    // The payload takes its room once: growing by doubling held the columns
    // of a large store twice over while they were copied in.
    writer.reserve_exact(bodies.iter().map(|body| body.len() + 32).sum::<usize>());
    for (((property_name, kind), values), body) in groups.iter().zip(bodies) {
        write_lookup_id(writer, property_ids, property_name, "property")?;
        writer.write_all(&[*kind])?;
        write_var_u64(writer, values.len() as u64)?;
        write_var_u64(writer, body.len() as u64)?;
        writer.write_all(&body)?;
    }
    Ok(())
}

fn binary_source_value_kind(
    source: &SettingsBinaryValueSource<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<u8> {
    Ok(match source {
        SettingsBinaryValueSource::Property(value) => binary_raw_value_kind(value, lookup)?,
        SettingsBinaryValueSource::Settled(value) => binary_raw_value_kind(value, lookup)?,
        SettingsBinaryValueSource::Native(value) => native_value_kind(value),
        SettingsBinaryValueSource::Attributes(_) => 8,
    })
}

fn collect_binary_source_strings<'a>(
    source: &SettingsBinaryValueSource<'a>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    match source {
        SettingsBinaryValueSource::Property(value) => collect_raw_value_strings(value, lookup, out),
        SettingsBinaryValueSource::Settled(value) => {
            let mut counts = SettingsStringCounts::default();
            collect_raw_value_strings(value, lookup, &mut counts)?;
            for (text, count) in counts {
                add_count_key(out, Cow::Owned(text.into_owned()), count);
            }
            Ok(())
        }
        SettingsBinaryValueSource::Native(value) => {
            collect_native_value_strings(value, out);
            Ok(())
        }
        SettingsBinaryValueSource::Attributes(attributes) => {
            for (name, value) in *attributes {
                add_count(out, name);
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
    writer: &mut W,
) -> Result<()> {
    match source {
        SettingsBinaryValueSource::Property(value) => {
            write_raw_value_payload(value, kind, string_ids, lookup, writer)
        }
        SettingsBinaryValueSource::Settled(value) => {
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
        NativeSettingsValue::UniqueId(_) => 8,
        NativeSettingsValue::Ref(_) => 17,
        NativeSettingsValue::Vector2(_) => 11,
        NativeSettingsValue::Vector3(_) => 12,
        NativeSettingsValue::UDim(_) => 13,
        NativeSettingsValue::UDim2(_) => 14,
        NativeSettingsValue::Color3(_) => 15,
        NativeSettingsValue::CFrame(_) => 16,
        NativeSettingsValue::Rect(_) => 18,
        NativeSettingsValue::Enum(_) => 19,
        // Visited as the JSON value it holds, never as a typed value.
        NativeSettingsValue::Shared(_) => 0,
    }
}

fn collect_native_value_strings<'a>(
    value: &'a NativeSettingsValue,
    out: &mut SettingsStringCounts<'a>,
) {
    if let NativeSettingsValue::String(value) | NativeSettingsValue::Enum(value) = value {
        add_count(out, value);
    }
    if let NativeSettingsValue::UniqueId(value) = value {
        for text in ["_type", "UniqueId", "value", value.as_str()] {
            add_count(out, text);
        }
    }
}

fn native_fixed_components(value: &NativeSettingsValue) -> Option<&[f32]> {
    match value {
        NativeSettingsValue::Vector2(value) | NativeSettingsValue::UDim(value) => Some(value),
        NativeSettingsValue::Vector3(value) | NativeSettingsValue::Color3(value) => Some(value),
        NativeSettingsValue::UDim2(value) | NativeSettingsValue::Rect(value) => Some(value),
        NativeSettingsValue::CFrame(value) => Some(value.as_ref()),
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
        NativeSettingsValue::UniqueId(value) => {
            // Exactly the existing JSON object's wire form, including key order.
            write_var_u64(writer, 2)?;
            for (key, text) in [("_type", "UniqueId"), ("value", value.as_str())] {
                write_binary_string_id(writer, string_ids, key)?;
                writer.write_all(&[6])?;
                write_binary_string_id(writer, string_ids, text)?;
            }
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
        NativeSettingsValue::Shared(_) => {
            bail!("A shared settings value is written as the JSON value it holds")
        }
    }
    Ok(())
}

fn write_property_group_values<W: Write + ?Sized>(
    values: &[SettingsBinaryValue<'_>],
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    writer: &mut W,
) -> Result<()> {
    if values.len() < SETTINGS_BINARY_PARALLEL_CHUNK_SIZE || rayon::current_num_threads() <= 1 {
        return write_property_group_values_serial(values, kind, string_ids, lookup, writer);
    }
    let table = if kind == 16 {
        let table = build_cframe_rotation_table(values)?;
        write_cframe_table_header(table.as_ref(), writer)?;
        table
    } else {
        None
    };
    let bodies = values
        .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
        .enumerate()
        .map(|(chunk, items)| {
            let start = chunk * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE;
            let previous = start.checked_sub(1).map(|index| &values[index]);
            let rotations = table
                .as_ref()
                .map(|table| &table.values[start..start + items.len()]);
            let mut body = Vec::new();
            write_property_group_chunk(
                items, kind, string_ids, lookup, rotations, previous, &mut body,
            )?;
            Ok(body)
        })
        .collect::<Result<Vec<_>>>()?;
    for body in bodies {
        writer.write_all(&body)?;
    }
    Ok(())
}

fn write_cframe_table_header<W: Write + ?Sized>(
    table: Option<&CFrameRotationTable>,
    writer: &mut W,
) -> Result<()> {
    writer.write_all(&[u8::from(table.is_some())])?;
    if let Some(table) = table {
        write_var_u64(writer, table.rotations.len() as u64)?;
        for rotation in &table.rotations {
            for component in rotation {
                write_f32_bits(*component, writer)?;
            }
        }
    }
    Ok(())
}

fn write_property_group_chunk<W: Write + ?Sized>(
    values: &[SettingsBinaryValue<'_>],
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    rotations: Option<&[CFrameTableValue]>,
    previous: Option<&SettingsBinaryValue<'_>>,
    writer: &mut W,
) -> Result<()> {
    let mut previous_instance = previous.map_or(0, |item| item.instance_index);
    let mut previous_target = 0;
    // A chunk continues the preceding value's deltas, even when references
    // point backwards. The rotation dictionary belongs to the whole group.
    if kind == 17
        && let Some(previous) = previous
    {
        write_resolved_ref_group_payload(
            &previous.source,
            lookup,
            &mut previous_target,
            &mut std::io::sink(),
        )?;
    }
    for (offset, item) in values.iter().enumerate() {
        write_property_group_instance_index(writer, &mut previous_instance, item.instance_index)?;
        if let Some(rotations) = rotations {
            for component in rotations[offset].position {
                write_f32_bits(component, writer)?;
            }
            write_var_u64(writer, rotations[offset].rotation_index as u64)?;
        } else if kind == 17 {
            write_resolved_ref_group_payload(&item.source, lookup, &mut previous_target, writer)?;
        } else {
            write_binary_source_payload(&item.source, kind, string_ids, lookup, writer)?;
        }
    }
    Ok(())
}

fn write_property_group_values_serial<W: Write + ?Sized>(
    values: &[SettingsBinaryValue<'_>],
    kind: u8,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
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
        SettingsBinaryValueSource::Settled(value) => value.as_ref(),
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

    let components = if values.len() >= SETTINGS_BINARY_PARALLEL_CHUNK_SIZE
        && rayon::current_num_threads() > 1
    {
        Some(
            values
                .par_iter()
                .with_min_len(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
                .map(cframe_binary_components)
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        None
    };
    for (index, item) in values.iter().enumerate() {
        let components = match &components {
            Some(components) => components[index],
            None => cframe_binary_components(item)?,
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

fn cframe_binary_components(item: &SettingsBinaryValue<'_>) -> Result<[u32; 12]> {
    match &item.source {
        SettingsBinaryValueSource::Native(NativeSettingsValue::CFrame(value)) => {
            Ok(value.map(f32::to_bits))
        }
        SettingsBinaryValueSource::Native(_) | SettingsBinaryValueSource::Attributes(_) => {
            bail!("Expected CFrame property group")
        }
        SettingsBinaryValueSource::Property(value) => cframe_component_bits(value),
        SettingsBinaryValueSource::Settled(value) => cframe_component_bits(value),
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

fn binary_raw_value_kind(value: &Value, lookup: &SettingsBinaryInstanceLookup<'_>) -> Result<u8> {
    let held = crate::settings::held::resolve(value);
    let value: &Value = &held;
    // A handle this process holds no payload for is never written.
    if value
        .as_object()
        .is_some_and(crate::settings::held::is_handle)
    {
        bail!("A value is the handle of a payload this process does not hold");
    }
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<Option<usize>> {
    // A path into another service names an outside instance; a settings id or
    // index that happens to exist in this store must not capture it.
    if let Some((segments, _)) = strict_reference_path(ref_value)?
        && let Some(root_service) = lookup.root_service.as_deref()
        && segments
            .first()
            .is_some_and(|service| service != root_service)
    {
        return Ok(None);
    }
    let mut resolved = None;
    let mut unresolved = false;
    let mut explicit_instance_index = None;
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
        let index_candidate = index_candidate.context("Ref instanceIndex does not exist")?;
        explicit_instance_index = Some(index_candidate);
        accept("instanceIndex", index_candidate)?;
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
        let candidate = lookup.by_settings_id.get(settings_id).copied().or_else(|| {
            let index = explicit_instance_index?;
            lookup
                .dense_instances?
                .get(index)
                .filter(|instance| instance.settings_id == settings_id)
                .map(|_| index)
        });
        accept_stable("settingsId", candidate)?;
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
            let candidate = lookup.by_settings_id.get(id).copied().or_else(|| {
                let index = explicit_instance_index?;
                lookup
                    .dense_instances?
                    .get(index)
                    .filter(|instance| instance.settings_id == id)
                    .map(|_| index)
            });
            accept_stable(alias, candidate)?;
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
        bail!(
            "Ref selectors mix local and nonlocal targets: {}",
            serde_json::to_string(ref_value).unwrap_or_default()
        );
    }
    Ok(resolved)
}

fn collect_raw_value_strings<'a>(
    value: &'a Value,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    let held = crate::settings::held::resolve(value);
    if matches!(held, crate::settings::held::Resolved::Held(_)) {
        // The payload is kept only while this runs: its strings are counted
        // as copies, as a settled value's are.
        let mut counts = SettingsStringCounts::default();
        collect_raw_value_strings(&held, lookup, &mut counts)?;
        for (text, count) in counts {
            add_count_key(out, Cow::Owned(text.into_owned()), count);
        }
        return Ok(());
    }
    match binary_raw_value_kind(value, lookup)? {
        0 | 1 | 2 | 3 | 4 | 5 | 9 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 20 | 21 => {}
        6 | 19 => {
            if let Some(text) = raw_string_payload(value) {
                add_count(out, text);
            }
        }
        7 => {
            let items = value
                .as_array()
                .context("Expected array binary settings value")?;
            for item in items {
                collect_nested_raw_value_strings(item, lookup, out)?;
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    let obj = value
        .as_object()
        .context("Expected object binary settings value")?;
    if let Some(type_name) = obj.get("_type").and_then(Value::as_str) {
        match type_name {
            "BrickColor" => {
                add_count(out, "BrickColor");
                collect_raw_value_strings(obj.get("number").unwrap_or(&Value::Null), lookup, out)?;
                return Ok(());
            }
            "ColorSequence" => {
                for key in ["ColorSequence", "keypoints", "time", "color"] {
                    add_count(out, key);
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
                    add_count(out, key);
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
        add_count(out, key);
        collect_nested_raw_value_strings(child, lookup, out)?;
    }
    Ok(())
}

fn collect_font_strings<'a>(obj: &'a Map<String, Value>, out: &mut SettingsStringCounts<'a>) {
    if let Some(family) = obj.get("family").and_then(Value::as_str) {
        add_count(out, "family");
        add_count(out, family);
    }
    if let Some(weight) = obj.get("weight").and_then(Value::as_str) {
        add_count(out, "weight");
        add_count(out, split_enum_tail(weight));
    }
    if let Some(style) = obj.get("style").and_then(Value::as_str) {
        add_count(out, "style");
        add_count(out, split_enum_tail(style));
    }
    if let Some(cached_face_id) = obj.get("cachedFaceId").and_then(Value::as_str) {
        add_count(out, "cachedFaceId");
        add_count(out, cached_face_id);
    }
}

fn collect_ref_strings<'a>(ref_value: &'a Map<String, Value>, out: &mut SettingsStringCounts<'a>) {
    if let Some(instance_id) = fallback_instance_id(ref_value) {
        add_count_key(out, instance_id, 1);
    }
    if let Some(settings_id) = fallback_settings_id(ref_value) {
        add_count(out, settings_id);
    }
    for key in ["debugId", "path"] {
        if let Some(text) = ref_value.get(key).and_then(Value::as_str) {
            add_count(out, text);
        }
    }
    if let Some(path_segments) = ref_value.get("pathSegments").and_then(Value::as_array) {
        for segment in path_segments {
            if let Some(text) = segment.as_str() {
                add_count(out, text);
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

const NESTED_ENUM_ITEM_KIND: u8 = 22;

/// An enum item nested inside an object or array value has no property
/// descriptor to supply its enum type, so the type is stored with the name.
fn nested_enum_item(value: &Value) -> Option<(&str, &str)> {
    let obj = value.as_object()?;
    if obj.get("_type").and_then(Value::as_str) != Some("EnumItem") {
        return None;
    }
    let enum_type = obj
        .get("enumType")
        .and_then(Value::as_str)
        .filter(|enum_type| !enum_type.is_empty())?;
    let name = obj.get("name").and_then(Value::as_str)?;
    Some((enum_type, name))
}

fn write_nested_raw_value<W: Write + ?Sized>(
    value: &Value,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    writer: &mut W,
) -> Result<()> {
    if let Some((enum_type, name)) = nested_enum_item(value) {
        writer.write_all(&[NESTED_ENUM_ITEM_KIND])?;
        write_binary_string_id(writer, string_ids, enum_type)?;
        return write_binary_string_id(writer, string_ids, name);
    }
    write_raw_value(value, string_ids, lookup, writer)
}

fn collect_nested_raw_value_strings<'a>(
    value: &'a Value,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    if let Some((enum_type, name)) = nested_enum_item(value) {
        add_count(out, enum_type);
        add_count(out, name);
        return Ok(());
    }
    collect_raw_value_strings(value, lookup, out)
}

fn write_raw_value<W: Write + ?Sized>(
    value: &Value,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
    writer: &mut W,
) -> Result<()> {
    let held = crate::settings::held::resolve(value);
    let value: &Value = &held;
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
                write_nested_raw_value(item, string_ids, lookup, writer)?;
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
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
        write_nested_raw_value(child, string_ids, lookup, writer)?;
    }
    Ok(())
}

fn write_brick_color_payload<W: Write + ?Sized>(
    obj: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
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
    lookup: &SettingsBinaryInstanceLookup<'_>,
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

/// The instance id an unresolved reference is written with: its instanceId,
/// or else its instanceIndex in hex. The string table counts this one.
fn fallback_instance_id(ref_value: &Map<String, Value>) -> Option<Cow<'_, str>> {
    ref_value
        .get("instanceId")
        .and_then(Value::as_str)
        .map(Cow::Borrowed)
        .or_else(|| {
            ref_value
                .get("instanceIndex")
                .and_then(Value::as_u64)
                .map(|instance_index| Cow::Owned(format!("{instance_index:x}")))
        })
}

/// The settings id an unresolved reference is written with: its settingsId,
/// or else its referent or ref alias. The string table counts this one.
fn fallback_settings_id(ref_value: &Map<String, Value>) -> Option<&str> {
    ["settingsId", "referent", "ref"]
        .into_iter()
        .find_map(|key| ref_value.get(key).and_then(Value::as_str))
}

fn write_ref_fallback_payload<W: Write + ?Sized>(
    ref_value: &Map<String, Value>,
    string_ids: &SettingsStringIdMap<'_>,
    writer: &mut W,
) -> Result<()> {
    let instance_id = fallback_instance_id(ref_value);
    let debug_id = ref_value.get("debugId").and_then(Value::as_str);
    let path_segments = ref_value.get("pathSegments").and_then(Value::as_array);
    let path = ref_value.get("path").and_then(Value::as_str);
    let settings_id = fallback_settings_id(ref_value);
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
    if let Some(instance_id) = &instance_id {
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

/// Decoded attributes carry the raw key form (`{"BrickColor":1004}`);
/// the plugin and model imports carry `_type`. Both must re-encode.
fn attribute_type_key(obj: &Map<String, Value>) -> Option<&'static str> {
    let type_name = obj.get("_type").and_then(Value::as_str);
    if type_name == Some("Float") {
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
    let type_name = type_name?;
    if type_name == "EnumItem" {
        return Some("EnumItem");
    }
    supported.into_iter().find(|&key| type_name == key)
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

/// Model imports keep numeric types (`{"_type":"Float64","value":1.0}`);
/// attributes only store f64, so those wrappers reduce to their number.
fn typed_number_attribute(value: &Value) -> Option<&Value> {
    let obj = value.as_object()?;
    let type_name = obj.get("_type").and_then(Value::as_str)?;
    if !matches!(type_name, "Float64" | "Float32" | "Int32" | "Int64") {
        return None;
    }
    obj.get("value").filter(|inner| inner.is_number())
}

/// Decoding drops the outer attribute key, which leaves BrickColor, the
/// sequences and Font without the `_type` marker the writers key on. Restore
/// it so a decoded store re-encodes to the same bytes.
pub(crate) fn canonical_attribute_value(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    if obj.contains_key("_type") {
        return None;
    }
    if obj.len() == 1 {
        let (key, inner) = obj.iter().next()?;
        match key.as_str() {
            "BrickColor" if inner.is_number() => {
                return Some(json!({"_type": "BrickColor", "number": inner}));
            }
            "NumberSequence" if inner.is_object() => {
                let mut typed = inner.as_object()?.clone();
                typed.insert("_type".to_string(), json!("NumberSequence"));
                return Some(Value::Object(typed));
            }
            "ColorSequence" if inner.is_object() => {
                let keypoints = inner
                    .get("keypoints")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice)
                    .iter()
                    .map(|keypoint| {
                        let Some(fields) = keypoint.as_object() else {
                            return keypoint.clone();
                        };
                        match fields.get("color").and_then(Value::as_array) {
                            Some(components) if components.len() == 3 => json!({
                                "time": fields.get("time").cloned().unwrap_or(Value::Null),
                                "value": {
                                    "r": components[0],
                                    "g": components[1],
                                    "b": components[2],
                                },
                            }),
                            _ => keypoint.clone(),
                        }
                    })
                    .collect::<Vec<_>>();
                return Some(json!({"_type": "ColorSequence", "keypoints": keypoints}));
            }
            _ => {}
        }
    }
    if obj.contains_key("family") && obj.contains_key("weight") && obj.contains_key("style") {
        let mut typed = obj.clone();
        typed.insert("_type".to_string(), json!("Font"));
        return Some(Value::Object(typed));
    }
    None
}

/// A property value with each typed Font, ColorSequence and NumberSequence in
/// it replaced by the form a store reads back for it, for the generic writer:
/// the typed form and the decoded one encode to the same bytes. None when the
/// value holds none. Attributes keep the typed payloads, which they restore.
fn settled_property_value(value: &Value) -> Option<Value> {
    holds_typed_value(value).then(|| settled_value(value))
}

fn settled_type(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    if ref_payload_object(value).is_some() {
        return Some("Ref");
    }
    object.get("_type").and_then(Value::as_str)
}

/// Whether the generic writer reaches a typed Font, ColorSequence or
/// NumberSequence in `value`: it writes the fields of plain objects and the
/// items of arrays, and nothing inside the other typed values.
fn holds_typed_value(value: &Value) -> bool {
    match settled_type(value) {
        Some("Font" | "ColorSequence" | "NumberSequence") => true,
        Some(
            "Ref" | "Vector2" | "Vector3" | "UDim" | "UDim2" | "Color3" | "CFrame" | "Rect"
            | "EnumItem" | "BrickColor",
        ) => false,
        _ => match value {
            Value::Array(items) => items.iter().any(holds_typed_value),
            Value::Object(object) => object.values().any(holds_typed_value),
            _ => false,
        },
    }
}

fn settled_value(value: &Value) -> Value {
    match (settled_type(value), value) {
        (Some("Font"), Value::Object(object)) => settled_font(object),
        (Some("ColorSequence"), Value::Object(object)) => settled_color_sequence(object),
        (Some("NumberSequence"), Value::Object(object)) => settled_number_sequence(object),
        _ if !holds_typed_value(value) => value.clone(),
        (_, Value::Array(items)) => Value::Array(items.iter().map(settled_value).collect()),
        (_, Value::Object(object)) => Value::Object(
            object
                .iter()
                .map(|(key, child)| (key.clone(), settled_value(child)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn settled_font(object: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for key in ["family", "weight", "style", "cachedFaceId"] {
        if let Some(text) = object.get(key).and_then(Value::as_str) {
            let text = match key {
                "weight" | "style" => split_enum_tail(text),
                _ => text,
            };
            out.insert(key.to_string(), Value::String(text.to_string()));
        }
    }
    Value::Object(out)
}

/// A keypoint field as the sequence writers wrote it: as a property value,
/// where an enum item keeps only its name.
fn settled_keypoint_field(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    if let Some((_, name)) = nested_enum_item(value) {
        return typed_object("EnumItem", [("name", Value::String(name.to_string()))]);
    }
    settled_value(value)
}

fn settled_color_sequence(object: &Map<String, Value>) -> Value {
    let keypoints = sequence_keypoint_values(object)
        .iter()
        .filter_map(Value::as_object)
        .map(|keypoint| {
            let color = keypoint
                .get("value")
                .and_then(color_components)
                .unwrap_or([0.0; 3]);
            json!({
                "time": settled_keypoint_field(keypoint.get("time")),
                "color": color.map(json_number_f64),
            })
        })
        .collect::<Vec<_>>();
    json!({"ColorSequence": {"keypoints": keypoints}})
}

fn settled_number_sequence(object: &Map<String, Value>) -> Value {
    let keypoints = sequence_keypoint_values(object)
        .iter()
        .filter_map(Value::as_object)
        .map(|keypoint| {
            json!({
                "time": settled_keypoint_field(keypoint.get("time")),
                "value": settled_keypoint_field(keypoint.get("value")),
                "envelope": settled_keypoint_field(keypoint.get("envelope")),
            })
        })
        .collect::<Vec<_>>();
    json!({"NumberSequence": {"keypoints": keypoints}})
}

/// Studio keeps TextWrapped on while TextScaled is on, so a store that says
/// otherwise would differ from the place after every push and make the next
/// push rewrite it; every encoded store carries the value Studio keeps. None
/// when every instance already does.
fn forced_properties_settled_document(document: &SettingsBytecode) -> Option<SettingsBytecode> {
    use crate::settings::equivalence::{forced_property_differs, settle_forced_properties};
    if !document
        .instances
        .iter()
        .any(|instance| forced_property_differs(&instance.properties))
    {
        return None;
    }
    let mut settled = document.clone();
    for instance in &mut settled.instances {
        settle_forced_properties(&mut instance.properties);
    }
    Some(settled)
}

fn canonical_attribute_document(document: &SettingsBytecode) -> Option<SettingsBytecode> {
    let needs_copy = document.instances.iter().any(|instance| {
        instance
            .attributes
            .values()
            .any(|value| canonical_attribute_value(value).is_some())
    });
    if !needs_copy {
        return None;
    }
    let mut copy = document.clone();
    for instance in &mut copy.instances {
        for value in instance.attributes.values_mut() {
            if let Some(canonical) = canonical_attribute_value(value) {
                *value = canonical;
            }
        }
    }
    Some(copy)
}

fn collect_attribute_value_strings<'a>(
    value: &'a Value,
    out: &mut SettingsStringCounts<'a>,
) -> Result<()> {
    let value = typed_number_attribute(value).unwrap_or(value);
    if let Some(obj) = value.as_object()
        && let Some(key) = attribute_type_key(obj)
    {
        add_count(out, key);
        let lookup = SettingsBinaryInstanceLookup::default();
        let child = attribute_payload_child(obj, key, value);
        if key == "EnumItem" {
            if let Some((enum_type, name)) = enum_item_attribute_fields(child) {
                for text in ["enumType", enum_type, "name", name] {
                    add_count(out, text);
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
        add_count(out, "Bool");
    } else if value.is_number() {
        add_count(out, "Float64");
    } else if let Some(text) = value.as_str() {
        add_count(out, "String");
        add_count(out, text);
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
    let value = typed_number_attribute(value).unwrap_or(value);
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
    visit_reference_objects_mut(record, |object| reindex_reference(object, indices));
}

/// `reindex_reference_indices` for a record of the store of `service`, whose
/// instances `indices` holds. A reference whose path leads into another
/// service names no instance of this store, also when its id is one of the
/// store's, and carries no index.
pub(crate) fn reindex_store_reference_indices(
    record: &mut Map<String, Value>,
    indices: &HashMap<String, usize>,
    service: &str,
) {
    visit_reference_objects_mut(record, |object| {
        if reference_leads_out_of(object, service) {
            object.remove("instanceIndex");
        } else {
            reindex_reference(object, indices);
        }
    });
}

fn reindex_reference(object: &mut Map<String, Value>, indices: &HashMap<String, usize>) {
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
}

/// Whether a reference has a path into another service than `service`. Such
/// a reference names an instance of that service's store: settings ids are
/// unique within one store only, and the path says which store an id is of.
pub(crate) fn reference_leads_out_of(reference: &Map<String, Value>, service: &str) -> bool {
    reference
        .get("pathSegments")
        .and_then(Value::as_array)
        .and_then(|segments| segments.first())
        .and_then(Value::as_str)
        .is_some_and(|target| target != service)
}

pub(crate) fn visit_reference_objects_mut(
    record: &mut Map<String, Value>,
    mut visitor: impl FnMut(&mut Map<String, Value>),
) {
    for value in record.values_mut() {
        for_each_reference_mut(value, &mut visitor);
    }
}

/// Calls `visit` for every reference object inside `value`, an outer one
/// before the ones it holds.
pub(crate) fn for_each_reference(value: &Value, visit: &mut impl FnMut(&Map<String, Value>)) {
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|value| for_each_reference(value, visit)),
        Value::Object(object) => {
            if is_reference_object(object) {
                visit(object);
            }
            object
                .values()
                .for_each(|value| for_each_reference(value, visit));
        }
        _ => {}
    }
}

/// `for_each_reference` over a value the visitor may change.
pub(crate) fn for_each_reference_mut(
    value: &mut Value,
    visit: &mut impl FnMut(&mut Map<String, Value>),
) {
    match value {
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| for_each_reference_mut(value, visit)),
        Value::Object(object) => {
            if is_reference_object(object) {
                visit(object);
            }
            object
                .values_mut()
                .for_each(|value| for_each_reference_mut(value, visit));
        }
        _ => {}
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
    /// A store over the cache's own limit is decoded for its caller and not
    /// kept: the cache held such a document, gigabytes of it, until another
    /// store was read.
    #[test]
    fn a_store_over_the_cache_limit_is_not_kept() {
        let document = |count: usize, name: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: (0..count)
                .map(|index| {
                    SettingsBytecodeInstance::new(
                        format!("{name}{index}"),
                        name.into(),
                        "Folder".into(),
                        (index > 0).then_some(0),
                    )
                })
                .collect(),
        };
        let large =
            encode_settings_bytecode(&document(MAX_CACHED_SETTINGS_INSTANCES + 1, "L")).unwrap();
        let decoded = cached_settings_document(&large).unwrap();
        assert_eq!(Arc::strong_count(&decoded), 1);
        let small = encode_settings_bytecode(&document(3, "CacheLimitProbe")).unwrap();
        let first = cached_settings_document(&small).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &cached_settings_document(&small).unwrap()
        ));
    }

    use serde_json::json;

    use super::*;

    /// A store's bytes are the same on every machine: the number of threads
    /// that compress a large payload does not show in them, and a small
    /// payload never takes the large path.
    #[test]
    fn a_store_payload_compresses_to_the_same_bytes_on_any_number_of_threads() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut payload = Vec::with_capacity(6 << 20);
        while payload.len() < 6 << 20 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let length = 64 + (state >> 40) as usize % 4_000;
            // Runs that repeat far apart, as a store's values do.
            if state.is_multiple_of(3) && payload.len() > length {
                let from = (state >> 8) as usize % (payload.len() - length);
                payload.extend_from_within(from..from + length);
            } else {
                payload.extend(state.to_le_bytes().iter().cycle().take(length));
            }
        }
        let one = compress_store_payload(&payload, 1).unwrap();
        // A frame taken a piece at a time is the frame one call gives, so a
        // store written by an older Renium compresses to the same bytes.
        assert!(one == compress_store_payload_at_once(&payload, 1).unwrap());
        for workers in [2, 8, 32] {
            assert!(
                one == compress_store_payload(&payload, workers).unwrap(),
                "{workers} threads"
            );
        }
        assert!(zstd::bulk::decompress(&one, payload.len()).unwrap() == payload);
        assert!(one.len() < zstd::bulk::compress(&payload, 1).unwrap().len());

        let small = &payload[..LARGE_STORE_PAYLOAD_BYTES - 1];
        let packed = compress_store_payload(small, 1).unwrap();
        assert!(packed == compress_store_payload(small, 8).unwrap());
        assert!(packed == zstd::bulk::compress(small, STORE_COMPRESSION_LEVEL).unwrap());
        assert!(zstd::bulk::decompress(&packed, small.len()).unwrap() == small);
    }

    #[test]
    fn large_numeric_columns_preserve_values_order_and_validation() {
        let serial = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let parallel = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let count = SETTINGS_BINARY_PARALLEL_CHUNK_SIZE * 2 + 7;
        for (kind, fields, table) in [
            (11, 2, false),
            (12, 3, false),
            (13, 2, false),
            (14, 4, false),
            (15, 3, false),
            (18, 4, false),
            (16, 12, false),
            (16, 3, true),
        ] {
            let mut body = Vec::new();
            if kind == 16 {
                body.push(u8::from(table));
                if table {
                    write_var_u64(&mut body, 1).unwrap();
                    for value in [1_f32, 0., 0., 0., 1., 0., 0., 0., 1.] {
                        body.extend(value.to_le_bytes());
                    }
                }
            }
            for index in 0..count {
                // Include non-unit index deltas at chunk boundaries.
                write_var_u64(&mut body, (1 + index % 3) as u64).unwrap();
                for field in 0..fields {
                    body.extend(((index as f32 - field as f32) / 4.).to_le_bytes());
                }
                if table {
                    write_var_u64(&mut body, 0).unwrap();
                }
            }
            let decode = |bytes: &[u8], pool: &rayon::ThreadPool| {
                pool.install(|| {
                    let budget = DecodeBudget::new(usize::MAX);
                    let scope = ValueScope {
                        strings: &[],
                        instance_count: count * 4,
                        budget: &budget,
                    };
                    decode_property_group_body(bytes, "Numeric", kind, count, scope)
                })
            };
            let expected = decode(&body, &serial).unwrap();
            let actual = decode(&body, &parallel).unwrap();
            assert_eq!(expected.len(), actual.len());
            for (expected, actual) in expected.into_iter().zip(actual) {
                match (expected, actual) {
                    (
                        DecodedGroupEntry::Property(i, left),
                        DecodedGroupEntry::Property(j, right),
                    ) => {
                        assert_eq!(i, j);
                        assert_eq!(left, right);
                    }
                    _ => panic!("Numeric group produced attributes"),
                }
            }
            assert!(decode(&body[..body.len() - 1], &parallel).is_err());
            body.push(0);
            assert!(decode(&body, &parallel).is_err());
            if table {
                body.pop();
                *body.last_mut().unwrap() = 1;
                assert!(decode(&body, &parallel).is_err());
            }
        }
    }

    #[test]
    fn transported_settings_id_precedes_live_debug_id() {
        let instance = SnapshotInstance {
            transported_settings_id: Some("editor:stable".to_string()),
            debug_id: Some("0_123".to_string()),
            instance_index: Some(2),
            parent_index: Some(1),
            ..Default::default()
        };

        assert_eq!(instance_settings_id(1, &instance), "editor:stable");
        match settings_binary_id(1, &instance) {
            SettingsBinaryId::Text(settings_id) => assert_eq!(settings_id, "editor:stable"),
            SettingsBinaryId::NumericDebug(_) => panic!("transported id was bypassed"),
        }
    }

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
    fn settings_bytecode_roundtrips_style_rule_enum_properties() {
        let mut rule = SettingsBytecodeInstance::new(
            "UIListLayout".to_string(),
            "StyleRule".to_string(),
            "ReplicatedStorage/Design/BaseStyleSheet/UIListLayout".to_string(),
            Some(0),
        );
        rule.properties.insert(
            "Properties".to_string(),
            json!({"SortOrder": {"_type":"EnumItem","enumType":"SortOrder","name":"LayoutOrder","value":2}}),
        );
        rule.properties.insert(
            "Order".to_string(),
            json!([{"_type":"EnumItem","enumType":"SortOrder","name":"Name"}, {"_type":"EnumItem","name":"Untyped"}]),
        );
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ReplicatedStorage".to_string(),
                    "ReplicatedStorage".to_string(),
                    "ReplicatedStorage".to_string(),
                    None,
                ),
                rule,
            ],
        };
        let encoded = encode_settings_bytecode(&document).unwrap();
        let decoded = decode_settings_bytecode(&encoded).unwrap();
        assert_eq!(
            decoded.instances[1].properties["Properties"],
            json!({"SortOrder": {"_type":"EnumItem","enumType":"SortOrder","name":"LayoutOrder"}})
        );
        assert_eq!(
            decoded.instances[1].properties["Order"],
            json!([{"_type":"EnumItem","enumType":"SortOrder","name":"Name"}, {"_type":"EnumItem","name":"Untyped"}])
        );
        let reencoded = encode_settings_bytecode(&decoded).unwrap();
        assert_eq!(reencoded, encoded);
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
                            "Division".to_string(),
                            json!({"_type":"Float64","value":2.0}),
                        ),
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
        let structure = decode_settings_bytecode_structure(&bytes).unwrap();

        assert_eq!(decoded.instances.len(), 3);
        assert_eq!(structure.instances.len(), 3);
        assert_eq!(structure.instances[1].settings_id, "child");
        assert_eq!(structure.instances[1].name, "Target");
        assert_eq!(structure.instances[1].class_name, "Part");
        assert_eq!(structure.instances[1].parent_index, Some(0));
        assert!(structure.instances[1].properties.is_empty());
        assert!(structure.instances[1].attributes.is_empty());
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
            decoded.instances[1].attributes.get("Division"),
            Some(&json!(2.0))
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
    fn settings_bytecode_rejects_oversized_decoded_payload_before_decompression() {
        let mut bytes = SETTINGS_BINARY_MAGIC.to_vec();
        bytes.push(SETTINGS_BINARY_VERSION);
        bytes.extend([1, 1]);
        write_var_u64(&mut bytes, (MAX_SETTINGS_BYTECODE_BYTES as u64) + 1).unwrap();
        write_var_u64(&mut bytes, 0).unwrap();

        let error = decode_settings_bytecode(&bytes).unwrap_err().to_string();
        assert!(error.contains("Decoded settings bytecode payload exceeds safe size limit"));
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

    /// A payload with one root instance and one property group whose body
    /// is `body`, wrapped as a store file.
    fn store_with_group(kind: u8, value_count: usize, body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        for text in ["editor:0", "Workspace", "Value"] {
            if payload.is_empty() {
                write_var_u64(&mut payload, 3).unwrap();
            }
            write_var_u64(&mut payload, text.len() as u64).unwrap();
            payload.extend_from_slice(text.as_bytes());
        }
        // One class and one property, both by string id; one root instance.
        for id in [1, 1, 1, 2, 1, 0, 1, 0, 0] {
            write_var_u64(&mut payload, id).unwrap();
        }
        // One group: property 0, its kind, value count and body.
        write_var_u64(&mut payload, 1).unwrap();
        write_var_u64(&mut payload, 0).unwrap();
        payload.push(kind);
        write_var_u64(&mut payload, value_count as u64).unwrap();
        write_var_u64(&mut payload, body.len() as u64).unwrap();
        payload.extend_from_slice(body);
        wrap_settings_bytecode_payload(&payload).unwrap()
    }

    fn group_body(body: &[u8], kind: u8, values: usize, budget: usize) -> Result<usize> {
        let budget = DecodeBudget::new(budget);
        let scope = ValueScope {
            strings: &[],
            instance_count: 4,
            budget: &budget,
        };
        decode_property_group_body(body, "Value", kind, values, scope).map(|entries| entries.len())
    }

    #[test]
    fn a_count_the_input_cannot_hold_reserves_no_room_for_it() {
        let reader = BytecodeReader::new(&[0; 10]);
        assert_eq!(reader.capacity(500_000, 1), 10);
        assert_eq!(reader.capacity(500_000, 4), 2);
        assert_eq!(reader.capacity(3, 1), 3);

        // Arrays that each claim the largest length and hold one more such
        // array reserved 16 MB a level before; the body is a few hundred bytes.
        let mut body = Vec::new();
        write_var_u64(&mut body, 0).unwrap();
        for _ in 0..=MAX_SETTINGS_VALUE_DEPTH {
            write_var_u64(&mut body, MAX_SETTINGS_COLLECTION_ITEMS as u64).unwrap();
            body.push(7);
        }
        let error = group_body(&body, 7, 1, usize::MAX).unwrap_err().to_string();
        assert!(error.contains("nesting exceeds safe depth"), "{error}");
        // A file this small may not decode into that much either.
        let error = decode_settings_bytecode(&store_with_group(7, 1, &body))
            .unwrap_err()
            .to_string();
        assert!(error.contains("more memory than a file"), "{error}");
    }

    #[test]
    fn a_store_decodes_into_no_more_memory_than_its_budget() {
        let mut body = Vec::new();
        write_var_u64(&mut body, 0).unwrap();
        write_var_u64(&mut body, 8).unwrap();
        body.extend_from_slice(&[0; 8]);
        // The value in its group, the array it is and its eight items.
        let cost = VALUE_SLOT_BYTES + "Value".len() + value_kind_bytes(7) + 8 * VALUE_SLOT_BYTES;
        assert_eq!(group_body(&body, 7, 1, cost).unwrap(), 1);
        let error = group_body(&body, 7, 1, cost - 1).unwrap_err().to_string();
        assert!(error.contains("more memory than a file"), "{error}");
        assert!(decode_settings_bytecode(&store_with_group(7, 1, &body)).is_ok());

        // One long string named by many instances is a copy for each.
        let name = "n".repeat(4096);
        let instances = (0..64)
            .map(|index| {
                SettingsBytecodeInstance::new(
                    format!("i{index}"),
                    name.clone(),
                    "Folder".to_string(),
                    (index > 0).then_some(0),
                )
            })
            .collect();
        let payload = encode_settings_bytecode_payload(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        })
        .unwrap();
        let decode = |budget: usize| {
            decode_settings_bytecode_document(
                SETTINGS_BINARY_VERSION,
                &payload,
                &DecodeBudget::new(budget),
            )
        };
        assert!(decode(64 * (4096 + INSTANCE_BYTES) + 4096).is_ok());
        let error = decode(32 * 4096).unwrap_err().to_string();
        assert!(error.contains("more memory than a file"), "{error}");
    }

    #[test]
    fn a_group_holds_one_value_for_an_instance() {
        // Two booleans: the second for the next instance, or for the same one.
        assert_eq!(group_body(&[0, 1], 2, 2, usize::MAX).unwrap(), 2);
        let error = group_body(&[0, 0], 2, 2, usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(error.contains("two values of a property"), "{error}");
        assert_eq!(group_body(&[0], 2, 1, usize::MAX).unwrap(), 1);
    }

    #[test]
    fn a_store_cannot_carry_an_instances_name_class_or_parent_as_properties() {
        let mut root = SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        for name in ["Name", "ClassName", "Parent", "Anchored"] {
            root.properties.insert(name.to_string(), json!("x"));
        }
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root],
        };
        let payload = encode_settings_bytecode_payload(&document).unwrap();
        let decoded =
            decode_settings_bytecode(&wrap_settings_bytecode_payload(&payload).unwrap()).unwrap();
        assert_eq!(
            decoded.instances[0].properties.keys().collect::<Vec<_>>(),
            ["Anchored"]
        );
    }

    #[test]
    fn an_unresolved_reference_alias_is_written_as_its_settings_id() {
        // The target's id is compact, so the string table holds it only
        // through the alias.
        let mut holder = SettingsBytecodeInstance::new(
            "editor:2".to_string(),
            "Holder".to_string(),
            "ObjectValue".to_string(),
            Some(0),
        );
        holder.properties.insert(
            "Gone".to_string(),
            json!({"_type": "Ref", "referent": "gone"}),
        );
        holder.properties.insert(
            "Local".to_string(),
            json!({"_type": "Ref", "ref": "debug:0_7"}),
        );
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                hierarchy_instance("editor:0", None),
                hierarchy_instance("debug:0_7", Some(0)),
                holder,
            ],
        };
        let bytes = encode_settings_bytecode(&document).unwrap();
        let decoded = decode_settings_bytecode(&bytes).unwrap();
        assert_eq!(
            decoded.instances[2].properties["Gone"],
            json!({"_type": "Ref", "settingsId": "gone"})
        );
        assert_eq!(
            decoded.instances[2].properties["Local"],
            json!({"_type": "Ref", "instanceIndex": 2})
        );
        assert_eq!(encode_settings_bytecode(&decoded).unwrap(), bytes);
        let dense = encode_settings_bytecode_with_dense_references(&document).unwrap();
        assert_eq!(
            decode_settings_bytecode(&dense).unwrap().instances[2].properties["Local"],
            json!({"_type": "Ref", "settingsId": "debug:0_7"})
        );
    }

    #[test]
    fn an_outside_reference_with_an_id_and_an_index_reencodes_to_its_bytes() {
        let mut root = hierarchy_instance("editor:0", None);
        root.name = "Workspace".to_string();
        root.properties.insert(
            "Target".to_string(),
            json!({
                "_type": "Ref",
                "pathSegments": ["Lighting", "Sun"],
                "instanceId": "a",
                "instanceIndex": 24,
            }),
        );
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root],
        };
        let bytes = encode_settings_bytecode(&document).unwrap();
        let decoded = decode_settings_bytecode(&bytes).unwrap();
        assert_eq!(
            decoded.instances[0].properties["Target"],
            json!({"_type": "Ref", "instanceId": "a", "pathSegments": ["Lighting", "Sun"]})
        );
        assert_eq!(encode_settings_bytecode(&decoded).unwrap(), bytes);
    }

    #[test]
    fn a_property_a_store_cannot_give_back_is_refused() {
        let document = |name: &str, value: Value| {
            let mut part = hierarchy_instance("editor:1", Some(0));
            part.properties.insert(name.to_string(), value);
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![hierarchy_instance("editor:0", None), part],
            }
        };
        for (name, value) in [
            ("Name", json!("Door")),
            ("ClassName", json!("Model")),
            ("Parent", json!("editor:0")),
            ("Attributes", json!({"Health": 5})),
        ] {
            let error = encode_settings_bytecode(&document(name, value.clone()))
                .unwrap_err()
                .to_string();
            assert!(error.contains(name), "{error}");
            assert!(
                encode_settings_bytecode_with_dense_references(&document(name, value)).is_err()
            );
        }
        assert!(encode_settings_bytecode(&document("Name", Value::Null)).is_ok());
        let kept = document("Attributes", json!("text"));
        let decoded = decode_settings_bytecode(&encode_settings_bytecode(&kept).unwrap()).unwrap();
        assert_eq!(decoded.instances[1].properties["Attributes"], json!("text"));
    }

    #[test]
    fn a_typed_font_or_sequence_encodes_as_its_decoded_form() {
        // A store an earlier build wrote with these three in the typed layout.
        const TYPED_LAYOUT: &[u8] = &[
            82, 66, 83, 83, 69, 84, 0, 11, 165, 2, 243, 1, 40, 181, 47, 253, 96, 37, 0, 77, 7, 0,
            242, 207, 52, 39, 16, 205, 54, 7, 128, 138, 24, 36, 240, 47, 230, 49, 160, 95, 194, 7,
            32, 181, 2, 100, 205, 181, 10, 172, 31, 32, 136, 80, 28, 62, 230, 253, 39, 10, 178, 45,
            137, 76, 1, 8, 157, 255, 17, 77, 255, 31, 14, 182, 186, 238, 27, 61, 235, 248, 234, 46,
            244, 16, 117, 204, 53, 1, 214, 6, 2, 90, 99, 207, 80, 167, 83, 135, 173, 50, 247, 255,
            211, 255, 169, 243, 111, 1, 227, 171, 204, 49, 197, 213, 61, 250, 136, 186, 239, 175,
            48, 80, 128, 182, 11, 233, 108, 96, 120, 28, 209, 88, 3, 171, 13, 196, 144, 51, 37,
            116, 181, 116, 56, 128, 213, 139, 9, 146, 228, 39, 201, 121, 53, 70, 146, 36, 147, 165,
            172, 36, 42, 17, 194, 129, 207, 170, 48, 141, 51, 13, 121, 115, 177, 139, 72, 98, 207,
            119, 97, 23, 27, 182, 5, 226, 137, 71, 121, 184, 98, 105, 38, 13, 88, 89, 177, 39, 77,
            130, 98, 108, 169, 168, 121, 53, 229, 34, 95, 96, 83, 141, 45, 23, 107, 32, 152, 18,
            197, 134, 189, 0, 93, 96, 43, 60, 108, 189, 249, 200, 121, 189, 206, 112, 248, 97, 65,
            16, 6, 0, 24, 97, 76, 142, 76, 186, 121, 199, 118, 182, 13, 166, 84, 147, 182, 228, 1,
        ];
        let typed = [
            (
                "FontFace",
                json!({"_type": "Font", "family": "rbxasset://fonts/families/Arial.json", "weight": "Enum.FontWeight.Bold", "style": "Normal"}),
            ),
            (
                "Color",
                json!({"_type": "ColorSequence", "keypoints": [{"time": 0, "value": {"r": 1, "g": 0.5, "b": 0}}, {"time": 1, "value": {"r": 0, "g": 0, "b": 1}}]}),
            ),
            (
                "Size",
                json!({"_type": "NumberSequence", "keypoints": [{"time": 0, "value": 2, "envelope": 0}, {"time": 1, "value": 0.5, "envelope": 0.25}]}),
            ),
        ];
        let document = |values: &[(&str, Value)]| {
            let mut root = SettingsBytecodeInstance::new(
                "editor:0".to_string(),
                "Fx".to_string(),
                "Folder".to_string(),
                None,
            );
            for (name, value) in values {
                root.properties.insert((*name).to_string(), value.clone());
            }
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![root],
            }
        };
        let decoded = decode_settings_bytecode(TYPED_LAYOUT).unwrap();
        assert_eq!(
            Value::Object(decoded.instances[0].properties.clone()),
            json!({
                "FontFace": {"family": "rbxasset://fonts/families/Arial.json", "weight": "Bold", "style": "Normal"},
                "Color": {"ColorSequence": {"keypoints": [{"time": 0, "color": [1.0, 0.5, 0.0]}, {"time": 1, "color": [0.0, 0.0, 1.0]}]}},
                "Size": {"NumberSequence": {"keypoints": [{"time": 0, "value": 2, "envelope": 0}, {"time": 1, "value": 0.5, "envelope": 0.25}]}},
            })
        );
        let bytes = encode_settings_bytecode(&document(&typed)).unwrap();
        assert_eq!(
            decode_settings_bytecode(&bytes).unwrap().instances,
            decoded.instances
        );
        assert_eq!(encode_settings_bytecode(&decoded).unwrap(), bytes);
        for (name, value) in typed {
            let untyped = decoded.instances[0].properties[name].clone();
            assert_eq!(
                encode_settings_bytecode(&document(&[(name, value)])).unwrap(),
                encode_settings_bytecode(&document(&[(name, untyped)])).unwrap(),
                "{name}"
            );
        }
    }
}

#[cfg(test)]
#[path = "reference_index_sim.rs"]
mod reference_index_sim;

#[cfg(test)]
#[path = "parallel_codec_sim.rs"]
pub(crate) mod parallel_codec_sim;
