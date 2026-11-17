use super::*;
use crate::settings::equivalence::SettingsAlignment;
use crate::snapshot::types::NativeSettingsProperty;
use sha2::{Digest, Sha256};
use std::io;

#[derive(Clone, Copy)]
struct ConversionScope<'a, 'b> {
    lookup: &'a SettingsBinaryInstanceLookup<'b>,
    instance_count: usize,
    budget: &'a DecodeBudget,
    string_ids: &'a SettingsStringIdMap<'a>,
    /// Where the size of the encoded store is counted, for a conversion that
    /// has a limit to keep.
    wire_bytes: Option<&'a AtomicUsize>,
}

pub(super) fn convert(state: &ServiceState) -> Result<SettingsBytecode> {
    let _span = crate::app::profile::span("captured store document");
    let records = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let collected = collect_settings_binary_data(state, &lookup, &records, None)?;
    // Every file gets at least this budget. Outside the proved bounds, the
    // actual wire path decides acceptance and the exact first error.
    match convert_prepared(
        state,
        &records,
        &lookup,
        &collected,
        &DecodeBudget::for_file(0),
    ) {
        Ok(document) => Ok(document),
        Err(_) => {
            let _span = crate::app::profile::span("captured store exact limit check");
            decode_settings_bytecode(&encode_prepared_service_settings(
                &records, &lookup, collected,
            )?)
        }
    }
}

#[cfg(test)]
pub(super) fn convert_with_budget(
    state: &ServiceState,
    budget: &DecodeBudget,
) -> Result<SettingsBytecode> {
    let records = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let collected = collect_settings_binary_data(state, &lookup, &records, None)?;
    convert_prepared(state, &records, &lookup, &collected, budget)
}

/// A digest of everything this state holds, except what its viewport camera
/// holds: a pull puts the stored camera back whatever the captured one says.
/// A store's document comes from its state alone, so two states with one
/// digest give one document apart from that camera, and no document is built
/// to find that out. No decoder memory limit applies: the digest only says
/// whether this is a state a pull took before, and that pull decided whether
/// its store may be written.
pub(super) fn pulled_digest(state: &ServiceState) -> Result<PulledDigest> {
    use crate::settings::equivalence::{
        viewport_camera_lacks_enum_types, workspace_current_camera_index,
    };

    let _span = crate::app::profile::span("captured store digest");
    let records = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let skeleton = skeleton(state, &records, &lookup)?;
    let viewport_camera =
        workspace_current_camera_index(&skeleton).map(|index| records[index].source_index);
    let (digest, rows) = state_digests(state, viewport_camera);
    Ok(PulledDigest {
        digest,
        viewport_camera_lacks_enum_types: viewport_camera_lacks_enum_types(&skeleton),
        rows: Arc::new(rows),
    })
}

/// The most changed instances a pull compares one by one; past that it
/// aligns the whole documents.
const MAX_CHANGED_ROWS: usize = 2_048;

pub(super) fn changed_values(
    state: &ServiceState,
    before: &PulledRows,
    now: &PulledRows,
    stored: &[u8],
) -> Result<Option<SettingsAlignment>> {
    use crate::settings::equivalence::{
        instance_lacks_enum_types, is_store_identity, persistent_identity_of,
        pulled_instance_unchanged,
    };

    let _span = crate::app::profile::span("captured store changed values");
    // What is written here is the state's own encoding, which is what the
    // alignment writes only where the two encoders agree, and the stored
    // camera goes back by the positions of a state numbered as it stands.
    if before.frame != now.frame
        || before.rows.len() != now.rows.len()
        || before.viewport_camera != now.viewport_camera
        || !now.encodes_as_its_document
        || !state.dense_index_topology
    {
        return Ok(None);
    }
    let mut changed = Vec::new();
    for (index, (before, now)) in before.rows.iter().zip(&now.rows).enumerate() {
        if before.placed != now.placed {
            return Ok(None);
        }
        if before.values != now.values {
            if changed.len() == MAX_CHANGED_ROWS {
                return Ok(None);
            }
            changed.push(index);
        }
    }
    let records = collect_service_settings_binary_instances(state);
    // Rows are told apart by their place in the state, which is the store's
    // order only where every instance is written where it stands.
    if changed.is_empty()
        || records.len() != now.rows.len()
        || records
            .iter()
            .enumerate()
            .any(|(index, record)| record.source_index != index)
    {
        return Ok(None);
    }
    let budget = DecodeBudget::for_file(stored.len());
    let ids = records
        .iter()
        .map(|record| Ok(bare_instance(record, &budget)?.settings_id))
        .collect::<Result<Vec<_>>>()?;
    if ids.iter().collect::<AHashSet<_>>().len() != ids.len() {
        return Ok(None);
    }
    // The alignment keeps every instance where it stands under ids a store
    // gave, and under any other ids where each instance is told by a UniqueId
    // of its own, or by an id that is not Studio's. Past that it matches
    // instances by what they hold, which takes the whole documents.
    let by_position = ids.iter().all(|id| is_store_identity(id)) || {
        let mut unique = AHashMap::with_capacity(records.len());
        for (index, instance) in state.instances.iter().enumerate() {
            if let Some(id) = persistent_identity_of(&instance.properties) {
                unique
                    .entry(id)
                    .and_modify(|at| *at = None)
                    .or_insert(Some(index));
            }
        }
        let sole_root = records
            .iter()
            .filter(|record| record.parent_index.is_none())
            .count()
            == 1;
        records.iter().enumerate().all(|(index, record)| {
            now.viewport_camera == Some(index)
                || persistent_identity_of(&state.instances[index].properties)
                    .is_some_and(|id| unique[id] == Some(index))
                || !ids[index].starts_with("debug:")
                || sole_root && record.parent_index.is_none()
        })
    };
    if !by_position {
        return Ok(None);
    }
    let mut rows = changed.clone();
    rows.extend(now.viewport_camera);
    let stored_rows = crate::settings::bytecode::property_edit::decode_rows(stored, &rows)?;
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let scope = ConversionScope {
        lookup: &lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let mut captured_rows = Vec::with_capacity(rows.len());
    for &row in &rows {
        captured_rows.extend(convert_chunk(
            state,
            std::slice::from_ref(&records[row]),
            scope,
        )?);
    }
    // The file must be the one the earlier rows were taken of.
    if stored_rows
        .iter()
        .zip(&captured_rows)
        .any(|(stored, captured)| {
            stored.settings_id != captured.settings_id
                || stored.name != captured.name
                || stored.class_name != captured.class_name
                || stored.parent_index != captured.parent_index
        })
    {
        return Ok(None);
    }
    // A stored camera whose enum items can gain their types makes the answer
    // follow the captured camera, which no digest covers.
    if now.viewport_camera.is_some() && stored_rows.last().is_some_and(instance_lacks_enum_types) {
        return Ok(None);
    }
    // A stored instance the encoder of documents would write another way
    // (an older store's) is compared and kept by the alignment.
    let root_service = lookup.root_service.as_deref();
    if stored_rows.iter().any(|stored| {
        crate::settings::equivalence::forced_property_differs(&stored.properties)
            || !values_encode_alike(&stored.properties, &stored.attributes, root_service)
    }) {
        return Ok(None);
    }
    let mut unchanged = true;
    for (stored, captured) in stored_rows.iter().zip(&captured_rows).take(changed.len()) {
        match pulled_instance_unchanged(stored, captured, root_service)? {
            Some(same) => unchanged &= same,
            None => return Ok(None),
        }
    }
    if unchanged {
        return Ok(Some(SettingsAlignment::Equivalent));
    }
    // A pull keeps the stored viewport camera whatever the captured one says.
    let kept_camera = now.viewport_camera.and_then(|camera| {
        let stored = stored_rows.last()?;
        let captured = captured_rows.last()?;
        (stored.properties != captured.properties || stored.attributes != captured.attributes)
            .then_some(ReplacedRow {
                source_index: camera,
                instance: stored,
            })
    });
    let collected = collect_settings_binary_data(state, &lookup, &records, kept_camera)?;
    Ok(Some(SettingsAlignment::Changed(
        encode_prepared_service_settings(&records, &lookup, collected)?,
    )))
}

/// The state's document without its values: every instance with its id, name,
/// class and parent. Only a service root, whose values name the viewport
/// camera, and the cameras, whose values say whether their enum items carry
/// their types, hold what the document gives them.
fn skeleton(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<SettingsBytecode> {
    // The values converted here are held to what a whole store may hold: a
    // state past that has no digest, and its pull says why.
    let budget = DecodeBudget::new(MAX_SETTINGS_BYTECODE_BYTES);
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let unlimited = DecodeBudget::new(usize::MAX);
    let mut instances = Vec::with_capacity(records.len());
    for record in records {
        if record.parent_index.is_none() || record.class_name == "Camera" {
            instances.extend(convert_chunk(state, std::slice::from_ref(record), scope)?);
        } else {
            instances.push(bare_instance(record, &unlimited)?);
        }
    }
    validate_settings_hierarchy(&instances)?;
    Ok(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    })
}

/// Names the way the state is digested: a digest taken another way is never
/// taken for one of these.
const STATE_DIGEST_SCHEME: &[u8] = b"renium captured state 2";

#[cfg(test)]
pub(super) fn state_digest(state: &ServiceState, viewport_camera: Option<usize>) -> [u8; 32] {
    state_digests(state, viewport_camera).0
}

fn state_digests(state: &ServiceState, viewport_camera: Option<usize>) -> ([u8; 32], PulledRows) {
    use crate::settings::equivalence::{digest_map, digest_text};

    // Every field by name: a field added to the state is a field to digest.
    let ServiceState {
        instances,
        native_properties_by_instance,
        children_by_index,
        source_in_subtree,
        service_root_index,
        class_defaults_by_class,
        properties_default_elided,
        dense_index_topology,
    } = state;
    let shared = Mutex::new(AHashMap::new());
    let root_service = instances
        .get(*service_root_index)
        .map(|instance| instance.name.as_str());
    let (rows, alike): (Vec<_>, Vec<_>) = instances
        .par_iter()
        .enumerate()
        .map(|(index, instance)| {
            let native = native_properties_by_instance
                .as_ref()
                .and_then(|rows| rows.get(index));
            (
                instance_state_digest(instance, native, viewport_camera == Some(index), &shared),
                instance_encodes_as_its_document(instance, native, root_service),
            )
        })
        .unzip();
    let count = |digest: &mut Sha256, count: usize| digest.update((count as u64).to_le_bytes());
    let mut digest = Sha256::new();
    digest.update([
        u8::from(*properties_default_elided),
        u8::from(*dense_index_topology),
    ]);
    count(&mut digest, *service_root_index);
    count(
        &mut digest,
        native_properties_by_instance
            .as_ref()
            .map_or(0, |rows| rows.len() + 1),
    );
    count(&mut digest, children_by_index.len());
    for children in children_by_index {
        count(&mut digest, children.len());
        for child in children {
            count(&mut digest, *child);
        }
    }
    count(&mut digest, source_in_subtree.len());
    for holds_source in source_in_subtree {
        digest.update([u8::from(*holds_source)]);
    }
    let mut classes = class_defaults_by_class.iter().collect::<Vec<_>>();
    classes.sort_unstable_by_key(|(class, _)| class.as_str());
    count(&mut digest, classes.len());
    for (class, defaults) in classes {
        digest_text(&mut digest, class);
        digest_map(&mut digest, defaults);
    }
    let frame = <[u8; 32]>::from(digest.finalize());
    let mut digest = Sha256::new();
    digest.update(STATE_DIGEST_SCHEME);
    digest.update(frame);
    count(&mut digest, rows.len());
    for row in &rows {
        digest.update(row.placed);
        digest.update(row.values);
    }
    (
        digest.finalize().into(),
        PulledRows {
            frame,
            rows,
            viewport_camera,
            encodes_as_its_document: alike.into_iter().all(|alike| alike),
        },
    )
}

/// Whether the state's encoding of this instance is the encoding of its
/// document (see `values_encode_alike`).
fn instance_encodes_as_its_document(
    instance: &SnapshotInstance,
    native: Option<&Vec<NativeSettingsProperty>>,
    root_service: Option<&str>,
) -> bool {
    let holds = |name: &str| {
        instance
            .properties
            .get(name)
            .map(|value| value == &Value::Bool(true))
            .or_else(|| {
                native?
                    .iter()
                    .find(|property| property.name == name)
                    .map(|property| matches!(property.value, NativeSettingsValue::Bool(true)))
            })
    };
    !(holds("TextScaled") == Some(true) && holds("TextWrapped") != Some(true))
        && values_encode_alike(&instance.properties, &instance.attributes, root_service)
}

/// The digests of the values many instances share, by where each value is
/// held: one is digested once however many instances hold it.
type SharedValueDigests = Mutex<AHashMap<usize, [u8; 32]>>;

fn instance_state_digest(
    instance: &SnapshotInstance,
    native: Option<&Vec<NativeSettingsProperty>>,
    viewport_camera: bool,
    shared: &SharedValueDigests,
) -> PulledRow {
    use crate::settings::equivalence::{digest_map, digest_text};

    let SnapshotInstance {
        path,
        path_segments,
        name,
        class_name,
        properties,
        source_key,
        parent_path,
        attributes,
        debug_id,
        parent_debug_id,
        transported_settings_id,
        instance_id,
        parent_instance_id,
        instance_index,
        parent_index,
    } = instance;
    let mut digest = Sha256::new();
    digest_text(&mut digest, path);
    digest.update((path_segments.len() as u64).to_le_bytes());
    for segment in path_segments {
        digest_text(&mut digest, segment);
    }
    digest_text(&mut digest, name);
    digest_text(&mut digest, class_name.as_str());
    for text in [
        source_key,
        parent_path,
        debug_id,
        parent_debug_id,
        transported_settings_id,
        instance_id,
        parent_instance_id,
    ] {
        match text {
            Some(text) => {
                digest.update([1]);
                digest_text(&mut digest, text);
            }
            None => digest.update([0]),
        }
    }
    for index in [instance_index, parent_index] {
        digest.update(index.map_or(0, |index| index as u64 + 1).to_le_bytes());
    }
    // The UniqueId says which stored instance this is, like an id.
    if let Some(unique) = properties.get("UniqueId") {
        crate::settings::equivalence::digest_value(&mut digest, unique);
    }
    for property in native.into_iter().flatten() {
        if property.name == "UniqueId" {
            digest_native_value(&mut digest, &property.value, shared);
        }
    }
    let half = |digest: Sha256| {
        let whole = <[u8; 32]>::from(digest.finalize());
        <[u8; 16]>::try_from(&whole[..16]).unwrap()
    };
    let placed = half(digest);
    let mut digest = Sha256::new();
    if viewport_camera {
        return PulledRow {
            placed,
            values: half(digest),
        };
    }
    digest.update([1]);
    // A store holds none of the record's own properties, so their values are
    // no part of its document: a script's text changes without its store.
    // Their presence is, since a property the instance holds hides the typed
    // one of its name.
    digest.update((properties.len() as u64).to_le_bytes());
    for (name, value) in properties {
        digest_text(&mut digest, name);
        if is_record_property(name) {
            digest.update([0xfe]);
        } else {
            crate::settings::equivalence::digest_value(&mut digest, value);
        }
    }
    digest_map(&mut digest, attributes);
    digest.update(
        native
            .map_or(0, |values| values.len() as u64 + 1)
            .to_le_bytes(),
    );
    for property in native.into_iter().flatten() {
        let NativeSettingsProperty { name, value } = property;
        digest_text(&mut digest, name);
        digest_native_value(&mut digest, value, shared);
    }
    PulledRow {
        placed,
        values: half(digest),
    }
}

fn digest_native_value(
    digest: &mut Sha256,
    value: &NativeSettingsValue,
    shared: &SharedValueDigests,
) {
    let floats = |digest: &mut Sha256, kind: u8, values: &[f32]| {
        digest.update([kind]);
        for value in values {
            digest.update(value.to_bits().to_le_bytes());
        }
    };
    match value {
        NativeSettingsValue::Bool(value) => digest.update([1, u8::from(*value)]),
        NativeSettingsValue::Int(value) => {
            digest.update([2]);
            digest.update(value.to_le_bytes());
        }
        NativeSettingsValue::Float32(value) => floats(digest, 3, &[*value]),
        NativeSettingsValue::Float64(value) => {
            digest.update([4]);
            digest.update(value.to_bits().to_le_bytes());
        }
        NativeSettingsValue::String(value) => {
            digest.update([5]);
            crate::settings::equivalence::digest_text(digest, value);
        }
        NativeSettingsValue::Ref(index) => {
            digest.update([6]);
            digest.update((*index as u64).to_le_bytes());
        }
        NativeSettingsValue::Vector2(values) => floats(digest, 7, values),
        NativeSettingsValue::Vector3(values) => floats(digest, 8, values),
        NativeSettingsValue::UDim(values) => floats(digest, 9, values),
        NativeSettingsValue::UDim2(values) => floats(digest, 10, values),
        NativeSettingsValue::Color3(values) => floats(digest, 11, values),
        NativeSettingsValue::CFrame(values) => floats(digest, 12, values),
        NativeSettingsValue::Rect(values) => floats(digest, 13, values),
        NativeSettingsValue::Enum(value) => {
            digest.update([14]);
            crate::settings::equivalence::digest_text(digest, value);
        }
        NativeSettingsValue::Shared(value) => {
            digest.update([15]);
            let held_at = Arc::as_ptr(value) as usize;
            let known = shared.lock_recover().get(&held_at).copied();
            digest.update(known.unwrap_or_else(|| {
                let mut of_value = Sha256::new();
                crate::settings::equivalence::digest_value(&mut of_value, value);
                let of_value = <[u8; 32]>::from(of_value.finalize());
                shared.lock_recover().insert(held_at, of_value);
                of_value
            }));
        }
    }
}

fn convert_prepared(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    collected: &SettingsBinaryCollection<'_>,
    budget: &DecodeBudget,
) -> Result<SettingsBytecode> {
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: convert_prepared_each(state, records, lookup, collected, budget, |instance| {
            instance
        })?,
    };
    validate_settings_hierarchy(&document.instances)?;
    Ok(document)
}

/// Converts the state chunk by chunk and keeps of each instance what `keep`
/// returns for it, in order. The hierarchy is the caller's to check.
fn convert_prepared_each<T: Send>(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    collected: &SettingsBinaryCollection<'_>,
    budget: &DecodeBudget,
    keep: impl Fn(SettingsBytecodeInstance) -> T + Sync,
) -> Result<Vec<T>> {
    let _span = crate::app::profile::span("captured store validated conversion");
    let strings = collected
        .string_counts
        .keys()
        .map(|text| Cow::Borrowed(text.as_ref()))
        .collect::<Vec<_>>();
    let string_ids = strings
        .iter()
        .map(|text| (text.as_ref(), u64::MAX))
        .collect();
    let wire_bytes = AtomicUsize::new(validate_header(
        records,
        collected,
        &strings,
        &string_ids,
        budget,
    )?);
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget,
        string_ids: &string_ids,
        wire_bytes: Some(&wire_bytes),
    };
    let kept_chunk = |records: &[SettingsBinaryInstance<'_>]| -> Result<Vec<T>> {
        Ok(convert_chunk(state, records, scope)?
            .into_iter()
            .map(&keep)
            .collect())
    };
    let instances = if records.len() < SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
        || rayon::current_num_threads() <= 1
    {
        kept_chunk(records)?
    } else {
        records
            .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
            .map(kept_chunk)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    };
    let bound = wire_bytes.load(Ordering::Relaxed);
    check_settings_payload_len(bound, "Decoded")?;
    let compressed = zstd::zstd_safe::compress_bound(bound);
    check_settings_payload_len(compressed, "Compressed")?;
    check_settings_file_len(compressed.saturating_add(SETTINGS_BINARY_MAGIC.len() + 1 + 20))?;
    Ok(instances)
}

#[derive(Default)]
struct SizeWriter(usize);

impl Write for SizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_header(
    records: &[SettingsBinaryInstance<'_>],
    collected: &SettingsBinaryCollection<'_>,
    strings: &[SettingsString<'_>],
    string_ids: &SettingsStringIdMap<'_>,
    budget: &DecodeBudget,
) -> Result<usize> {
    let _span = crate::app::profile::span("captured store limit bounds");
    checked_settings_collection_len(strings.len(), "string count")?;
    for text in strings {
        check_settings_string_len(text.len())?;
    }
    let classes = collected
        .class_counts
        .keys()
        .map(|text| Cow::Borrowed(text.as_ref()))
        .collect::<Vec<_>>();
    let properties = collected
        .property_counts
        .keys()
        .map(|text| Cow::Borrowed(text.as_ref()))
        .collect::<Vec<_>>();
    checked_settings_collection_len(classes.len(), "class count")?;
    checked_settings_collection_len(properties.len(), "property count")?;
    for text in classes.iter().chain(&properties) {
        budget.take(text.len())?;
    }
    checked_settings_collection_len(records.len(), "instance count")?;
    budget.instances(records.len())?;
    checked_settings_collection_len(collected.property_groups.len(), "property group count")?;
    let mut size = SizeWriter::default();
    write_settings_binary_header(
        &mut size,
        strings,
        string_ids,
        &classes,
        &properties,
        records.len(),
    )?;
    for (index, record) in records.iter().enumerate() {
        write_settings_binary_id(&mut size, string_ids, &record.settings_id)?;
        write_binary_string_id(&mut size, string_ids, record.name)?;
        write_binary_string_id(&mut size, string_ids, record.class_name)?;
        write_var_u64(&mut size, encode_parent_index(record.parent_index, index)?)?;
    }
    write_var_u64(&mut size, collected.property_groups.len() as u64)?;
    for ((name, kind), values) in &collected.property_groups {
        checked_settings_collection_len(values.len(), "property value count")?;
        if *kind == 16 {
            // A rotation table cannot contain more rotations than its values.
            // If that bound fails, the wire path checks the chosen table.
            checked_cframe_rotation_count(values.len())?;
        }
        if !SETTINGS_RECORD_FIELDS.contains(name) {
            budget.property_group(values.len(), name, *kind)?;
        }
        // Group header: property id, kind, count, byte length. All varints
        // fit ten bytes. A CFrame table is chosen only when smaller than inline.
        size.0 = size.0.saturating_add(31 + usize::from(*kind == 16));
    }
    Ok(size.0)
}

fn convert_chunk<'a>(
    state: &'a ServiceState,
    records: &[SettingsBinaryInstance<'a>],
    scope: ConversionScope<'_, '_>,
) -> Result<Vec<SettingsBytecodeInstance>> {
    let mut sources = Vec::new();
    let mut scratch = Vec::new();
    let mut wire = SizeWriter::default();
    let mut instances = Vec::with_capacity(records.len());
    for record in records {
        let mut instance = bare_instance(record, scope.budget)?;
        visit_settings_binary_properties(state, record, scope.lookup, |name, kind, source| {
            // Count through the real value writer with maximal-width string
            // ids. Instance/ref deltas also fit ten bytes, independent of order.
            if scope.wire_bytes.is_some() {
                wire.0 = wire.0.saturating_add(10);
                if kind == 17 {
                    wire.0 = wire.0.saturating_add(10);
                } else {
                    write_binary_source_payload(
                        &source,
                        kind,
                        scope.string_ids,
                        scope.lookup,
                        &mut wire,
                    )?;
                }
            }
            // A decoder discards these columns, including native capture fields.
            if !SETTINGS_RECORD_FIELDS.contains(&name) {
                sources.push((name, kind, source));
            }
            Ok(())
        })?;
        // Different wire kinds of one name overwrite in ascending kind order.
        // Repeating a row within the same group is rejected by the decoder.
        sources.sort_unstable_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
        if sources
            .windows(2)
            .any(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
        {
            next_group_instance(0, 0, 1)?;
        }
        for (name, kind, source) in sources.drain(..) {
            if name == "Attributes" && kind == 8 {
                let Value::Object(attributes) =
                    normalize_complex(&source, kind, true, scope, &mut scratch)?
                else {
                    bail!("Expected decoded attributes");
                };
                instance.attributes = attributes;
            } else {
                let value = normalize_source(&source, kind, scope, &mut scratch)?;
                instance.properties.insert(name.to_string(), value);
            }
        }
        instances.push(instance);
    }
    if let Some(wire_bytes) = scope.wire_bytes {
        wire_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                Some(total.saturating_add(wire.0))
            })
            .unwrap();
    }
    Ok(instances)
}

/// The document's instance of a record, before its values.
fn bare_instance(
    record: &SettingsBinaryInstance<'_>,
    budget: &DecodeBudget,
) -> Result<SettingsBytecodeInstance> {
    let id = match &record.settings_id {
        SettingsBinaryId::Text(text) => checked_string(text, budget)?,
        SettingsBinaryId::NumericDebug(id) => budget.copied(&format!("debug:0_{id}"))?,
    };
    Ok(SettingsBytecodeInstance::new(
        id,
        checked_string(record.name, budget)?,
        checked_string(record.class_name, budget)?,
        record.parent_index,
    ))
}

fn checked_string(text: &str, budget: &DecodeBudget) -> Result<String> {
    check_settings_string_len(text.len())?;
    budget.copied(text)
}

fn normalize_source(
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
) -> Result<Value> {
    match source {
        SettingsBinaryValueSource::Native(value) => native_value(value, scope),
        SettingsBinaryValueSource::Property(value) => {
            property_value(value, source, kind, scope, scratch)
        }
        SettingsBinaryValueSource::Settled(value) => {
            property_value(value, source, kind, scope, scratch)
        }
        SettingsBinaryValueSource::Attributes(_) => {
            normalize_complex(source, kind, true, scope, scratch)
        }
    }
}

fn property_value(
    value: &Value,
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
) -> Result<Value> {
    match kind {
        0..=5 | 20 => Ok(value.clone()),
        6 => Ok(Value::String(checked_string(
            raw_string_payload(value).context("Expected string binary settings value")?,
            scope.budget,
        )?)),
        19 => Ok(typed_object(
            "EnumItem",
            [(
                "name",
                Value::String(checked_string(
                    raw_string_payload(value).context("Expected enum binary settings value")?,
                    scope.budget,
                )?),
            )],
        )),
        9 | 21 => {
            let values = value
                .as_array()
                .context("Expected numeric array binary settings value")?;
            scope.budget.numeric_array(values.len(), kind)?;
            Ok(Value::Array(
                values
                    .iter()
                    .map(|value| {
                        settings_number(value)
                            .map(json_number_f64)
                            .context("Expected numeric array item")
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        11..=16 | 18 => {
            let fixed = fixed_numeric_kind_from_tag(kind).context("Expected fixed numeric kind")?;
            if kind == 16 {
                return Ok(fixed_value(
                    fixed,
                    &cframe_component_bits(value)?.map(f32::from_bits),
                ));
            }
            let fields = fixed_fields(fixed).1;
            let object = value
                .as_object()
                .context("Expected fixed numeric binary settings value")?;
            let mut components = [0_f32; 4];
            for (index, field) in fields.iter().enumerate() {
                components[index] = fixed_numeric_component_f32(
                    settings_number(object.get(*field).unwrap_or(&Value::Null))
                        .with_context(|| format!("Expected numeric field {field}"))?,
                );
            }
            Ok(fixed_value(fixed, &components[..fields.len()]))
        }
        17 => {
            let reference =
                ref_payload_object(value).context("Expected Ref binary settings value")?;
            let index = resolve_ref_index(reference, scope.lookup)?
                .context("Expected resolved Ref binary settings value")?;
            decoded_reference(index, scope.instance_count)
        }
        _ => normalize_complex(source, kind, false, scope, scratch),
    }
}

fn native_value(value: &NativeSettingsValue, scope: ConversionScope<'_, '_>) -> Result<Value> {
    match value {
        NativeSettingsValue::Bool(value) => Ok(json!(value)),
        NativeSettingsValue::Int(value) => Ok(json!(value)),
        NativeSettingsValue::Float32(value) => Ok(json_number_f64(f64::from(*value))),
        NativeSettingsValue::Float64(value) => Ok(json_number_f64(*value)),
        NativeSettingsValue::String(value) => {
            Ok(Value::String(checked_string(value, scope.budget)?))
        }
        NativeSettingsValue::Enum(value) => Ok(typed_object(
            "EnumItem",
            [("name", Value::String(checked_string(value, scope.budget)?))],
        )),
        NativeSettingsValue::Ref(index) => decoded_reference(*index, scope.instance_count),
        NativeSettingsValue::Shared(value) => Ok(Value::clone(value)),
        NativeSettingsValue::Vector2(_)
        | NativeSettingsValue::Vector3(_)
        | NativeSettingsValue::UDim(_)
        | NativeSettingsValue::UDim2(_)
        | NativeSettingsValue::Color3(_)
        | NativeSettingsValue::Rect(_)
        | NativeSettingsValue::CFrame(_) => {
            let kind = fixed_numeric_kind_from_tag(native_value_kind(value))
                .context("Expected fixed native value")?;
            Ok(fixed_value(
                kind,
                native_fixed_components(value).context("Expected native components")?,
            ))
        }
    }
}

fn fixed_fields(kind: FixedNumericKind) -> (&'static str, &'static [&'static str]) {
    match kind {
        FixedNumericKind::Vector2 => ("Vector2", &["x", "y"]),
        FixedNumericKind::Vector3 => ("Vector3", &["x", "y", "z"]),
        FixedNumericKind::UDim => ("UDim", &["scale", "offset"]),
        FixedNumericKind::UDim2 => ("UDim2", &["xScale", "xOffset", "yScale", "yOffset"]),
        FixedNumericKind::Color3 => ("Color3", &["r", "g", "b"]),
        FixedNumericKind::CFrame => ("CFrame", &[]),
        FixedNumericKind::Rect => ("Rect", &["minX", "minY", "maxX", "maxY"]),
    }
}

fn fixed_value(kind: FixedNumericKind, components: &[f32]) -> Value {
    let (name, fields) = fixed_fields(kind);
    if matches!(kind, FixedNumericKind::CFrame) {
        return typed_object(
            name,
            [(
                "components",
                Value::Array(
                    components
                        .iter()
                        .map(|value| json_number_f64(f64::from(*value)))
                        .collect(),
                ),
            )],
        );
    }
    let mut object = Map::new();
    object.insert("_type".into(), json!(name));
    for (field, value) in fields.iter().zip(components) {
        object.insert((*field).into(), json_number_f64(f64::from(*value)));
    }
    Value::Object(object)
}

fn normalize_complex(
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    attributes: bool,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
) -> Result<Value> {
    // Complex and extensible forms keep the codec's normalization rules in one
    // place. Only this value uses scratch wire data; no store is built, sorted
    // or compressed, and each worker reuses its buffer.
    let mut counts = SettingsStringCounts::default();
    collect_binary_source_strings(source, scope.lookup, &mut counts)?;
    let strings = counts
        .into_keys()
        .map(|value| {
            check_settings_string_len(value.len())?;
            Ok(value.into_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    let ids = strings
        .iter()
        .enumerate()
        .map(|(index, text)| (text.as_str(), index as u64))
        .collect();
    scratch.clear();
    write_binary_source_payload(source, kind, &ids, scope.lookup, scratch)?;
    let scope = ValueScope {
        strings: &strings,
        instance_count: scope.instance_count,
        budget: scope.budget,
    };
    let mut reader = BytecodeReader::new(scratch);
    let value = if attributes {
        Value::Object(decode_attributes_payload(&mut reader, scope)?)
    } else {
        decode_raw_value_payload(&mut reader, kind, scope, 0)?
    };
    reader.finish()?;
    Ok(value)
}
