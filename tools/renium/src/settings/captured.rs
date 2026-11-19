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
            drop(collected);
            decode_settings_bytecode(&blocks::encode_state(
                state,
                (&records, &lookup),
                None,
                None,
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

/// No answer from the instances alone: the pull aligns the whole documents,
/// and the trace log says why.
fn whole<T>(why: &str) -> Result<Option<T>> {
    crate::app::output::log_global(
        4,
        format_args!("[renium] store compared as whole documents: {why}"),
    );
    Ok(None)
}

/// A stored file of this size or more is read under the gate that store
/// encoders share (`read_gate`).
pub(super) const GATED_STORE_BYTES: usize = 1 << 20;

/// The most changed instances a pull compares one by one, each held whole on
/// both sides while it is compared; past that it aligns the whole documents.
const MAX_CHANGED_ROWS: usize = 65_536;

pub(super) fn changed_values(
    state: &ServiceState,
    before: &PulledRows,
    now: &PulledRows,
    stored: &[u8],
    store_gate: Option<&Mutex<()>>,
) -> Result<Option<SettingsAlignment>> {
    let _span = crate::app::profile::span("captured store changed values");
    if before.frame != now.frame
        || before.rows.len() != now.rows.len()
        || before.viewport_camera != now.viewport_camera
        || !comparable_by_instance(state, now)
    {
        return whole("the state is not comparable with what the pull remembers");
    }
    let mut changed = Vec::new();
    for (index, (before, now)) in before.rows.iter().zip(&now.rows).enumerate() {
        if before.placed != now.placed {
            return whole("an instance stands elsewhere than the pull remembers");
        }
        if before.values != now.values {
            if changed.len() == MAX_CHANGED_ROWS {
                return whole("more instances changed than are compared one by one");
            }
            changed.push(index);
        }
    }
    let records = collect_service_settings_binary_instances(state);
    if changed.is_empty() || !written_where_they_stand(&records, now) {
        return whole(
            "nothing changed that the pull remembers, or the state is not written in its order",
        );
    }
    let Some(places) = Places::of(state, now, &records, stored.len())? else {
        return whole("two instances of the state carry one id");
    };
    let (_reading, store_gate) = read_gate(stored, store_gate);
    let stored = StoredRows {
        file: stored,
        read: None,
    };
    changed_rows(state, now, (records, &places), &changed, stored, store_gate)
}

/// Reading a large store, for its digests or for the instances that changed,
/// takes as much memory as writing one, so two such stores are not read side
/// by side: the comparison of a large store holds the gate that store
/// encoders share from its read on. With the guard comes the gate the
/// comparison still has to take to build a store, which is none where it
/// holds it already.
fn read_gate<'a>(
    stored: &[u8],
    store_gate: Option<&'a Mutex<()>>,
) -> (Option<std::sync::MutexGuard<'a, ()>>, Option<&'a Mutex<()>>) {
    match store_gate {
        Some(gate) if stored.len() >= GATED_STORE_BYTES => (Some(gate.lock_recover()), None),
        gate => (None, gate),
    }
}

/// What is written for a store compared by its instances is the state's own
/// encoding, which is what the alignment writes only where the two encoders
/// agree, and the stored camera goes back by the positions of a state
/// numbered as it stands.
fn comparable_by_instance(state: &ServiceState, now: &PulledRows) -> bool {
    now.encodes_as_its_document && state.dense_index_topology
}

/// Rows are told apart by their place in the state, which is the store's
/// order only where every instance is written where it stands.
fn written_where_they_stand(records: &[SettingsBinaryInstance<'_>], now: &PulledRows) -> bool {
    records.len() == now.rows.len()
        && records
            .iter()
            .enumerate()
            .all(|(index, record)| record.source_index == index)
}

/// What a pull of `state` leaves of the stored file `stored` where no pull
/// remembers that file: the digests of the two documents' instances say
/// which of them differ, and those alone are compared. Whether every
/// instance but the viewport camera is the same in both comes with the
/// answer. `None` where the pull has to align the whole documents.
pub(super) fn changed_document_rows(
    state: &ServiceState,
    now: &PulledRows,
    stored: &[u8],
    store_gate: Option<&Mutex<()>>,
) -> Result<Option<(SettingsAlignment, bool)>> {
    let _span = crate::app::profile::span("captured store compared by instance");
    if !comparable_by_instance(state, now) {
        return whole("the state is written another way than its document");
    }
    let records = collect_service_settings_binary_instances(state);
    // What the state alone can say comes before the two documents are read.
    if !written_where_they_stand(&records, now) {
        return whole("the state is not written in its order");
    }
    let Some(places) = Places::of(state, now, &records, stored.len())? else {
        return whole("two instances of the state carry one id");
    };
    let (_reading, store_gate) = read_gate(stored, store_gate);
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let (stored_rows, captured_rows) = rayon::join(
        || {
            crate::settings::bytecode::property_edit::row_digests(stored, |stored| {
                // The stored camera is the one that stands where the state's
                // does.
                let moved = Moved::between(stored, &records)?;
                let camera = match (now.viewport_camera, moved) {
                    (Some(camera), Some(moved)) => Some(moved.in_store(camera)?),
                    (camera, _) => camera,
                };
                Some((camera, moved))
            })
        },
        || document_row_digests(state, &records, &lookup, stored.len()),
    );
    let (Some(stored_rows), captured_rows) = (stored_rows?, captured_rows?) else {
        return whole("the store differs from the state by more than one run of instances");
    };
    let moved = stored_rows.moved;
    let mut changed = Vec::new();
    let mut renamed = false;
    for (index, (values, unique_id)) in captured_rows.iter().enumerate() {
        if now.viewport_camera == Some(index) {
            continue;
        }
        // An instance the store does not hold yet has nothing to differ from.
        let Some(stored_index) = moved.map_or(Some(index), |moved| moved.in_store(index)) else {
            continue;
        };
        if stored_rows.digests[stored_index] != *values {
            // A root that differs can name another viewport camera than the
            // one left out here, which only the whole documents show.
            if records[index].parent_index.is_none() {
                return whole("a root differs");
            }
            changed.push(index);
        } else {
            renamed |= stored_rows.unique_ids[stored_index] != *unique_id;
        }
    }
    if changed.len() > MAX_CHANGED_ROWS {
        return whole(&format!(
            "{} instances differ, more than are compared one by one",
            changed.len()
        ));
    }
    let stored = StoredRows {
        file: stored,
        read: Some(ReadStore {
            viewport_camera: stored_rows.kept,
            ids: stored_rows.ids,
            unique_ids: stored_rows.unique_ids,
            captured_unique_ids: captured_rows
                .into_iter()
                .map(|(_, unique_id)| unique_id)
                .collect(),
            renamed,
            moved,
        }),
    };
    Ok(
        changed_rows(state, now, (records, &places), &changed, stored, store_gate)?
            .map(|alignment| (alignment, changed.is_empty() && !renamed)),
    )
}

/// A stored file and, where a caller read the two documents already, what
/// it read of them.
struct StoredRows<'a> {
    file: &'a [u8],
    read: Option<ReadStore>,
}

/// What the digests of two documents say beside which instances differ.
struct ReadStore {
    /// The stored viewport camera, where the store has one at that place.
    viewport_camera: Option<SettingsBytecodeInstance>,
    /// The id of each stored instance.
    ids: Vec<String>,
    /// The digest of the UniqueId of each stored instance, and of each
    /// captured one: all zero for an instance without one.
    unique_ids: Vec<[u8; 16]>,
    captured_unique_ids: Vec<[u8; 16]>,
    /// Whether an instance with the stored values holds another UniqueId
    /// than the stored one.
    renamed: bool,
    /// How the stored instances stand in the state, where it holds one run
    /// of instances more than the store, or one less.
    moved: Option<Moved>,
}

/// How the instances of a stored file stand in a state that holds one run of
/// instances the file does not hold, or lacks one run the file holds: the
/// run starts at `at` and is `by` long.
#[derive(Clone, Copy)]
pub(crate) struct Moved {
    at: usize,
    by: usize,
    added: bool,
}

impl Moved {
    /// How the stored instances stand among the state's: `Some(None)` where
    /// the two hold as many, `None` where they differ by more than one run.
    /// The run starts where a name, class or parent first differs, as the
    /// alignment takes it, and every instance past it stands under the name,
    /// class and parent of the stored one it is taken for.
    pub(super) fn between(
        stored: &[SettingsBytecodeInstance],
        records: &[SettingsBinaryInstance<'_>],
    ) -> Option<Option<Self>> {
        if stored.len() == records.len() {
            return Some(None);
        }
        let common = stored.len().min(records.len());
        let at = (0..common)
            .find(|&index| {
                let (stored, record) = (&stored[index], &records[index]);
                stored.name != record.name
                    || stored.class_name != record.class_name
                    || stored.parent_index != record.parent_index
            })
            .unwrap_or(common);
        let moved = Self {
            at,
            by: stored.len().abs_diff(records.len()),
            added: records.len() > stored.len(),
        };
        (at..common)
            .all(|position| {
                let (stored_index, index) = if moved.added {
                    (position, position + moved.by)
                } else {
                    (position + moved.by, position)
                };
                let (stored, record) = (&stored[stored_index], &records[index]);
                stored.name == record.name
                    && stored.class_name == record.class_name
                    && match record.parent_index {
                        None => stored.parent_index.is_none(),
                        Some(parent) => moved
                            .in_store(parent)
                            .is_some_and(|parent| stored.parent_index == Some(parent)),
                    }
            })
            .then_some(Some(moved))
    }

    /// Which stored instance the state's instance at `index` is.
    pub(super) fn in_store(self, index: usize) -> Option<usize> {
        match (self.added, index < self.at) {
            (_, true) => Some(index),
            (true, false) => index.checked_sub(self.by).filter(|index| *index >= self.at),
            (false, false) => Some(index + self.by),
        }
    }

    /// Where the stored instance at `index` stands in the state.
    fn in_state(self, index: usize) -> Option<usize> {
        match (self.added, index < self.at) {
            (_, true) => Some(index),
            (true, false) => Some(index + self.by),
            (false, false) => index.checked_sub(self.by).filter(|index| *index >= self.at),
        }
    }

    /// The parent of a stored instance as the position it has in the state.
    /// A parent the state lacks is no position of the state's.
    pub(super) fn parent_in_state(self, instance: &mut SettingsBytecodeInstance) {
        instance.parent_index = instance
            .parent_index
            .map(|parent| self.in_state(parent).unwrap_or(usize::MAX - 1));
    }

    /// A stored value with every reference by position at the position its
    /// instance has in the state: `None` where it holds none.
    pub(super) fn value_in_state(self, value: &Value) -> Option<Value> {
        fn refers(value: &Value) -> bool {
            match value {
                Value::Object(map) => map.contains_key("instanceIndex") || map.values().any(refers),
                Value::Array(values) => values.iter().any(refers),
                _ => false,
            }
        }
        fn place(moved: Moved, value: &mut Value) {
            match value {
                Value::Object(map) => {
                    if map.get("_type").and_then(Value::as_str) == Some("Ref")
                        && let Some(index) = map.get("instanceIndex").and_then(Value::as_u64)
                    {
                        // A reference to an instance the state lacks points
                        // at no instance.
                        let index = (index as usize)
                            .checked_sub(1)
                            .and_then(|index| moved.in_state(index))
                            .map_or(0, |index| index + 1);
                        map.insert("instanceIndex".into(), json!(index));
                    }
                    map.values_mut().for_each(|value| place(moved, value));
                }
                Value::Array(values) => values.iter_mut().for_each(|value| place(moved, value)),
                _ => {}
            }
        }
        refers(value).then(|| {
            let mut value = value.clone();
            place(self, &mut value);
            value
        })
    }

    /// The same for the values of a map.
    pub(super) fn values_in_state(self, values: &Map<String, Value>) -> Option<Map<String, Value>> {
        let placed = values
            .iter()
            .filter_map(|(name, value)| Some((name, self.value_in_state(value)?)))
            .collect::<Vec<_>>();
        (!placed.is_empty()).then(|| {
            let mut values = values.clone();
            for (name, value) in placed {
                values.insert(name.clone(), value);
            }
            values
        })
    }

    /// A stored instance read whole, as it stands in the state.
    pub(super) fn in_state_whole(self, instance: &mut SettingsBytecodeInstance) {
        self.parent_in_state(instance);
        if let Some(properties) = self.values_in_state(&instance.properties) {
            instance.properties = properties;
        }
        if let Some(attributes) = self.values_in_state(&instance.attributes) {
            instance.attributes = attributes;
        }
    }

    /// The ids the state's instances are written under: each the id its
    /// stored instance has, and an instance the store does not hold yet its
    /// own, unless the store or an earlier new instance holds that one.
    fn written_ids(self, own: &[String], stored: &[String]) -> Vec<String> {
        let reserved = stored.iter().map(String::as_str).collect::<AHashSet<_>>();
        let mut used = AHashSet::new();
        let mut generated = 0_usize;
        own.iter()
            .enumerate()
            .map(|(index, id)| match self.in_store(index) {
                Some(stored_index) => stored[stored_index].clone(),
                None if !reserved.contains(id.as_str()) && used.insert(id.clone()) => id.clone(),
                None => loop {
                    let candidate = format!("reconcile:{generated:x}");
                    generated += 1;
                    if !reserved.contains(candidate.as_str()) && used.insert(candidate.clone()) {
                        break candidate;
                    }
                },
            })
            .collect()
    }
}

/// The instances each thread holds as a document while it digests them: few,
/// since a document's instance takes a hundred times its digest.
const DIGESTED_AT_ONCE: usize = 256;

/// The digest of each instance of the state's document, chunk by chunk: the
/// document is never whole in memory.
fn document_row_digests(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    stored_len: usize,
) -> Result<Vec<([u8; 16], [u8; 16])>> {
    let _span = crate::app::profile::span("captured store instance digests");
    let budget = DecodeBudget::for_file(stored_len);
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let chunks = records
        .par_chunks(DIGESTED_AT_ONCE)
        .map(|records| {
            Ok(convert_chunk(state, records, scope)?
                .iter()
                .map(document_row_digest)
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(chunks.into_iter().flatten().collect())
}

/// The digest of one instance of a document: its name, class and parent, and
/// every value it holds that a comparison looks at, but its UniqueId, whose
/// digest comes beside it (all zero where the instance has none). The id is
/// no part of either, and the UniqueId stands apart, because Studio gives
/// the instances of a place new ones each time it opens the place.
pub(super) fn document_row_digest(instance: &SettingsBytecodeInstance) -> ([u8; 16], [u8; 16]) {
    instance_digest(instance, false)
}

/// `document_row_digest` over every value the instance holds, what the
/// engine alone sets included (see `property_edit::whole_row_digests`).
pub(super) fn whole_document_row_digest(
    instance: &SettingsBytecodeInstance,
) -> ([u8; 16], [u8; 16]) {
    instance_digest(instance, true)
}

fn instance_digest(instance: &SettingsBytecodeInstance, whole: bool) -> ([u8; 16], [u8; 16]) {
    (
        row_digest(
            instance,
            instance
                .properties
                .iter()
                .filter(|(name, _)| {
                    whole && name.as_str() != UNIQUE_ID || is_digested(&instance.class_name, name)
                })
                .map(|(name, value)| (name.as_str(), value_digest(value))),
            attributes_digest(&instance.attributes),
        ),
        instance
            .properties
            .get(UNIQUE_ID)
            .map_or([0; 16], value_digest),
    )
}

/// Whether a property is part of its instance's digest: not the UniqueId,
/// and not what the engine alone sets, which Studio computes anew whenever
/// it loads the instance and no comparison looks at.
pub(super) fn is_digested(class_name: &str, name: &str) -> bool {
    name != UNIQUE_ID
        && !crate::settings::equivalence::reconciliation_property_is_always_engine_state(
            rbx_reflection_database::get().ok(),
            class_name,
            name,
        )
}

/// The property that says which instance a stored one is.
pub(super) const UNIQUE_ID: &str = "UniqueId";

/// The digest of an instance from the digests of its values, by property
/// name in the order a document holds them.
pub(super) fn row_digest<'a>(
    instance: &SettingsBytecodeInstance,
    properties: impl Iterator<Item = (&'a str, [u8; 16])>,
    attributes: [u8; 16],
) -> [u8; 16] {
    use crate::settings::equivalence::digest_text;

    let mut digest = Sha256::new();
    digest_text(&mut digest, &instance.name);
    digest_text(&mut digest, &instance.class_name);
    digest.update(
        instance
            .parent_index
            .map_or(0, |parent| parent as u64 + 1)
            .to_le_bytes(),
    );
    for (name, value) in properties {
        digest.update([1]);
        digest_text(&mut digest, name);
        digest.update(value);
    }
    digest.update([0]);
    digest.update(attributes);
    half_digest(digest)
}

/// A payload held once is digested once for the process.
pub(super) fn value_digest(value: &Value) -> [u8; 16] {
    let of = |value: &Value| {
        let mut digest = Sha256::new();
        crate::settings::equivalence::digest_value(&mut digest, value);
        half_digest(digest)
    };
    crate::settings::held::digest_once(value, of).unwrap_or_else(|| of(value))
}

pub(super) fn attributes_digest(attributes: &Map<String, Value>) -> [u8; 16] {
    let mut digest = Sha256::new();
    crate::settings::equivalence::digest_map(&mut digest, attributes);
    half_digest(digest)
}

pub(super) fn half_digest(digest: Sha256) -> [u8; 16] {
    let whole = <[u8; 32]>::from(digest.finalize());
    <[u8; 16]>::try_from(&whole[..16]).unwrap()
}

/// The answer for a store whose instances are the stored ones but for
/// `changed`, which hold other values, and for the viewport camera.
fn changed_rows<'a>(
    state: &'a ServiceState,
    now: &PulledRows,
    (records, places): (Vec<SettingsBinaryInstance<'a>>, &Places),
    changed: &[usize],
    stored: StoredRows<'_>,
    store_gate: Option<&Mutex<()>>,
) -> Result<Option<SettingsAlignment>> {
    use crate::settings::bytecode::property_edit::decode_rows;
    use crate::settings::equivalence::{
        instance_lacks_enum_types, pulled_instance_values_unchanged,
        reconciliation_value_contains_reference as holds_reference,
    };

    crate::app::output::log_global(
        4,
        format_args!(
            "[renium] captured store comparison: {} changed rows of {}",
            changed.len(),
            records.len()
        ),
    );

    let file = stored.file;
    let budget = DecodeBudget::for_file(file.len());
    let (camera, mut read) = match stored.read {
        Some(mut read) => (read.viewport_camera.take(), Some(read)),
        None => (None, None),
    };
    let moved = read.as_ref().and_then(|read| read.moved);
    // The camera is the last row where the state has one.
    let mut rows = changed.to_vec();
    if camera.is_none() {
        rows.extend(now.viewport_camera);
    }
    let (mut stored_rows, ids) = match &mut read {
        Some(read) if rows.is_empty() => (Vec::new(), std::mem::take(&mut read.ids)),
        _ => {
            let Some(stored_at) = rows
                .iter()
                .map(|row| moved.map_or(Some(*row), |moved| moved.in_store(*row)))
                .collect::<Option<Vec<_>>>()
            else {
                return whole("a changed instance is none the store holds");
            };
            decode_rows(file, &stored_at)?
        }
    };
    if let Some(moved) = moved {
        for stored in &mut stored_rows {
            moved.in_state_whole(stored);
        }
    }
    if let (Some(camera), Some(row)) = (camera, now.viewport_camera) {
        rows.push(row);
        stored_rows.push(camera);
    }
    // A pull leaves each instance the id its store holds it under.
    let written_ids = moved.map(|moved| moved.written_ids(&places.ids, &ids));
    let written_ids = written_ids.as_ref().unwrap_or(&ids);
    let own_ids = places.ids == *written_ids;
    let records = if own_ids {
        records
    } else {
        under_stored_ids(records, written_ids)
    };
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let scope = ConversionScope {
        lookup: &lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let captured_rows = rows
        .par_iter()
        .map(|row| convert_chunk(state, std::slice::from_ref(&records[*row]), scope))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    // Each pair is one instance in one place: an instance under another
    // name, class or parent is matched by the alignment.
    if stored_rows
        .iter()
        .zip(&captured_rows)
        .any(|(stored, captured)| {
            stored.name != captured.name
                || stored.class_name != captured.class_name
                || stored.parent_index != captured.parent_index
        })
    {
        return whole("an instance stands under another name, class or parent than the stored one");
    }
    // A stored camera whose enum items can gain their types makes the answer
    // follow the captured camera, which no digest covers.
    if now.viewport_camera.is_some() && stored_rows.last().is_some_and(instance_lacks_enum_types) {
        return whole("the stored viewport camera holds an enum item without its type");
    }
    // A stored instance the encoder of documents would write another way
    // (an older store's) is compared and kept by the alignment.
    let root_service = lookup.root_service.as_deref();
    if stored_rows.iter().any(|stored| {
        crate::settings::equivalence::forced_property_differs(&stored.properties)
            || !values_encode_alike(&stored.properties, &stored.attributes, root_service)
    }) {
        return whole("a stored instance is written another way than documents are today");
    }
    // What each changed instance says: whether its values are the stored
    // ones as a pull counts them, and whether its UniqueId is.
    let pairs = || stored_rows.iter().zip(&captured_rows).take(changed.len());
    let compared = stored_rows
        .par_iter()
        .zip(captured_rows.par_iter())
        .take(changed.len())
        .map(|(stored, captured)| pulled_instance_values_unchanged(stored, captured, root_service))
        .collect::<Result<Vec<_>>>()?;
    let mut same_values = Vec::with_capacity(changed.len());
    let mut same_under_todays_names = true;
    for compared in compared {
        match compared {
            Some((as_held, as_named)) => {
                same_values.push(as_held);
                same_under_todays_names &= as_named;
            }
            None => return whole("an enum item of a changed instance gains its type"),
        }
    }
    let renamed = read.as_ref().is_some_and(|read| read.renamed)
        || pairs().any(|(stored, captured)| {
            stored.properties.get(UNIQUE_ID) != captured.properties.get(UNIQUE_ID)
        });
    let stored_at = |index: usize| moved.map_or(Some(index), |moved| moved.in_store(index));
    let camera_holds_a_reference = now.viewport_camera.is_some()
        && stored_rows.last().is_some_and(|stored| {
            stored
                .properties
                .values()
                .chain(stored.attributes.values())
                .any(holds_reference)
        });
    let kept = if moved.is_some() {
        // A state with one run of instances more or less than the store: the
        // alignment takes every instance beside the run for the stored one
        // where each holds the stored values as a pull counts them, the
        // viewport camera too. Where one does not, it matches each instance
        // by what tells it alone, and gives a new one the id it came with.
        // Nothing may say an instance is another stored one, and the stored
        // camera goes back as the store holds it, which a reference by
        // position does not survive.
        let camera_alike = match (stored_rows.last(), captured_rows.last()) {
            (Some(stored), Some(captured)) if now.viewport_camera.is_some() => {
                match pulled_instance_values_unchanged(stored, captured, root_service)? {
                    Some((as_held, _)) => as_held,
                    None => return whole("an enum item of the viewport camera gains its type"),
                }
            }
            _ => true,
        };
        !camera_holds_a_reference
            && read.as_ref().is_some_and(|read| {
                places.stored_identities_stand(&ids, read, stored_at)
                    && (camera_alike && same_values.iter().all(|same| *same)
                        || places.matched_alone(&ids, read, now.viewport_camera, stored_at))
            })
    } else if own_ids {
        // An instance that took another UniqueId under its id became another
        // instance, which the alignment matches.
        !renamed && places.kept_under_their_ids()
    } else {
        // The alignment first takes every instance for the stored one at its
        // place where nothing says otherwise: each changed instance either
        // has no namesake under its parent and holds no reference on either
        // side, or holds the stored values as a pull counts them. Past that
        // it tells each instance by itself, which takes the stored UniqueIds.
        let stand_as_stored = read.as_ref().is_some_and(|read| {
            places.stored_identities_stand(&ids, read, stored_at)
                && namesakes(&records).is_some_and(|namesakes| {
                    pairs().zip(changed).zip(&same_values).all(
                        |(((stored, captured), row), same)| {
                            *same
                                || !namesakes[*row]
                                    && ![stored, captured].iter().any(|instance| {
                                        instance
                                            .properties
                                            .values()
                                            .chain(instance.attributes.values())
                                            .any(holds_reference)
                                    })
                        },
                    )
                })
        });
        stand_as_stored || !renamed && places.kept_under_other_ids(&ids)
    };
    if !kept {
        return whole("nothing tells every instance for the stored one at its place");
    }
    // The alignment puts the stored camera back before it compares anything
    // where the state's camera carries the stored one's id. Under another id
    // it knows the camera only once it matched the instances, and puts a
    // stored camera that holds a reference back in another form than it
    // compares: that one it decides.
    let camera_by_id = now
        .viewport_camera
        .is_none_or(|camera| places.ids[camera] == written_ids[camera]);
    if !camera_by_id
        && camera_holds_a_reference
        && let (Some(stored), Some(captured)) = (stored_rows.last(), captured_rows.last())
        && (stored.properties != captured.properties || stored.attributes != captured.attributes)
    {
        return whole("the stored viewport camera holds a reference and differs");
    }
    // The alignment first compares the instances at their places as they
    // hold their values; where that fails, it matches them and compares
    // what they hold under the names their properties go by today. A store
    // that says the same either way stays, with two limits. A stored camera
    // that holds a reference never compares alike the second way, so such a
    // store is written. And under other ids than the state's the first
    // comparison is not the comparison of instance with instance made here,
    // so only the second counts. One with other instances is written.
    let same_as_held = same_values.iter().all(|same| *same);
    if moved.is_none() && !renamed {
        let same_as_named = same_under_todays_names && !camera_holds_a_reference;
        if same_as_named || own_ids && same_as_held {
            return Ok(Some(SettingsAlignment::Equivalent));
        }
        if same_as_held {
            return whole("instances that hold the stored values compare otherwise by name");
        }
    }
    // A pull keeps the stored viewport camera whatever the captured one says.
    let kept_camera_index = now.viewport_camera.filter(|_| {
        stored_rows
            .last()
            .zip(captured_rows.last())
            .is_some_and(|(stored, captured)| {
                stored.properties != captured.properties || stored.attributes != captured.attributes
            })
    });
    let kept_camera = kept_camera_index.zip(stored_rows.pop());
    // Comparison is finished. Only the old camera can participate in the
    // encoding; do not retain decoded changed rows beside the output columns.
    drop(stored_rows);
    drop(captured_rows);
    drop(read);
    let kept_camera = kept_camera.as_ref().map(|(camera, stored)| ReplacedRow {
        source_index: *camera,
        instance: stored,
    });
    // Comparisons run in parallel. Only the new store's columns and compressed
    // output share the export's allocation gate with other encoders.
    let _store_guard = store_gate.map(LockRecover::lock_recover);
    Ok(Some(SettingsAlignment::Changed(blocks::encode_state(
        state,
        (&records, &lookup),
        kept_camera,
        Some(file),
    )?)))
}

/// Where the alignment leaves the instances of a state, as far as the state
/// alone says: the id of each, whether the alignment tells each by itself
/// whatever its id is, as it does the viewport camera and an instance with a
/// UniqueId of its own, and the root where the state has one root only.
struct Places {
    ids: Vec<String>,
    told_alone: Vec<bool>,
    sole_root: Option<usize>,
}

fn captured_persistent_identity(state: &ServiceState, index: usize) -> Option<&str> {
    use crate::settings::equivalence::{persistent_identity_of, persistent_identity_text};
    let properties = &state.instances[index].properties;
    if properties.contains_key(UNIQUE_ID) {
        return persistent_identity_of(properties);
    }
    let mut matches = state
        .native_properties_by_instance
        .as_ref()?
        .get(index)?
        .iter()
        .filter(|property| property.name == UNIQUE_ID);
    let property = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    match &property.value {
        NativeSettingsValue::UniqueId(id) => persistent_identity_text(id),
        _ => None,
    }
}

impl Places {
    /// `None` where two instances carry one id: those the alignment matches
    /// by what they hold, which takes the whole documents.
    fn of(
        state: &ServiceState,
        now: &PulledRows,
        records: &[SettingsBinaryInstance<'_>],
        stored_len: usize,
    ) -> Result<Option<Self>> {
        let budget = DecodeBudget::for_file(stored_len);
        let ids = records
            .iter()
            .map(|record| Ok(bare_instance(record, &budget)?.settings_id))
            .collect::<Result<Vec<_>>>()?;
        if ids.iter().collect::<AHashSet<_>>().len() != ids.len() {
            return Ok(None);
        }
        let mut unique = AHashMap::with_capacity(records.len());
        for index in 0..state.instances.len() {
            if let Some(id) = captured_persistent_identity(state, index) {
                unique
                    .entry(id)
                    .and_modify(|at| *at = None)
                    .or_insert(Some(index));
            }
        }
        let mut roots = records
            .iter()
            .enumerate()
            .filter(|(_, record)| record.parent_index.is_none())
            .map(|(index, _)| index);
        let sole_root = roots.next().filter(|_| roots.next().is_none());
        let told_alone = (0..records.len())
            .map(|index| {
                now.viewport_camera == Some(index)
                    || captured_persistent_identity(state, index)
                        .is_some_and(|id| unique[id] == Some(index))
            })
            .collect();
        Ok(Some(Self {
            ids,
            told_alone,
            sole_root,
        }))
    }

    /// Whether the alignment keeps every instance where it stands against a
    /// store that holds them under the state's own ids: where a store gave
    /// every id, or where each instance is told by itself, is the sole root,
    /// or carries an id that is not Studio's. Past that the alignment matches
    /// instances by what they hold.
    fn kept_under_their_ids(&self) -> bool {
        use crate::settings::equivalence::is_store_identity;

        self.ids.iter().all(|id| is_store_identity(id))
            || self
                .told_alone
                .iter()
                .zip(&self.ids)
                .enumerate()
                .all(|(index, (alone, id))| {
                    *alone || self.sole_root == Some(index) || !id.starts_with("debug:")
                })
    }

    /// The same against a store that holds them under the ids `stored`, which
    /// is a place Studio opened again and numbered anew: where each instance
    /// is told by itself, but for a sole root that is stored under its id.
    fn kept_under_other_ids(&self, stored: &[String]) -> bool {
        self.told_alone
            .iter()
            .zip(self.ids.iter().zip(stored))
            .enumerate()
            .all(|(index, (alone, (id, stored)))| {
                *alone || self.sole_root == Some(index) && id == stored
            })
    }

    /// Whether the alignment, matching instance by instance, takes each for
    /// the stored one `stored_at` gives: the viewport camera, a sole root, an
    /// instance whose own UniqueId is the stored one's and no other stored
    /// instance's, and any instance the store holds under an id that is not
    /// Studio's, which it is matched by once it carries that id.
    fn matched_alone(
        &self,
        stored_ids: &[String],
        read: &ReadStore,
        viewport_camera: Option<usize>,
        stored_at: impl Fn(usize) -> Option<usize>,
    ) -> bool {
        let stored_places = unique_id_places(&read.unique_ids);
        (0..self.ids.len()).all(|index| {
            let Some(stored) = stored_at(index) else {
                return true;
            };
            viewport_camera == Some(index)
                || self.sole_root == Some(index)
                || !stored_ids[stored].starts_with("debug:")
                || self.told_alone[index]
                    && stored_places.get(&read.captured_unique_ids[index]) == Some(&Some(stored))
        })
    }

    /// Whether nothing an instance is told by says it is another stored
    /// instance than the one `stored_at` takes it for: no UniqueId that one
    /// instance of the state holds is the one of a single stored instance
    /// elsewhere, and no id a store gave stands for another stored instance.
    fn stored_identities_stand(
        &self,
        stored_ids: &[String],
        read: &ReadStore,
        stored_at: impl Fn(usize) -> Option<usize>,
    ) -> bool {
        use crate::settings::equivalence::is_store_identity;

        let (stored, captured) = (
            unique_id_places(&read.unique_ids),
            unique_id_places(&read.captured_unique_ids),
        );
        let unique_ids_stand = captured.iter().all(|(digest, at)| {
            let (Some(at), Some(Some(stored_place))) = (at, stored.get(digest)) else {
                return true;
            };
            stored_at(*at) == Some(*stored_place)
        });
        unique_ids_stand && {
            let mut known = None;
            self.ids.iter().enumerate().all(|(index, id)| {
                !is_store_identity(id)
                    || stored_at(index).is_some_and(|stored| stored_ids[stored] == *id)
                    || !known
                        .get_or_insert_with(|| stored_ids.iter().collect::<AHashSet<_>>())
                        .contains(id)
            })
        }
    }
}

/// Where each UniqueId stands, by its digest: `None` for one that more than
/// one instance holds. An instance without one has none.
fn unique_id_places(digests: &[[u8; 16]]) -> AHashMap<[u8; 16], Option<usize>> {
    let mut places = AHashMap::with_capacity(digests.len());
    for (index, digest) in digests.iter().enumerate() {
        if *digest != [0; 16] {
            places
                .entry(*digest)
                .and_modify(|at| *at = None)
                .or_insert(Some(index));
        }
    }
    places
}

/// For each instance, whether it or an instance above it has a sibling of
/// its name and class. `None` where an instance stands before its parent,
/// which the alignment takes for no order at all.
fn namesakes(records: &[SettingsBinaryInstance<'_>]) -> Option<Vec<bool>> {
    let mut first = AHashMap::with_capacity(records.len());
    let mut namesakes = vec![false; records.len()];
    for (index, record) in records.iter().enumerate() {
        if let Some(earlier) =
            first.insert((record.parent_index, record.name, record.class_name), index)
        {
            namesakes[earlier] = true;
            namesakes[index] = true;
        }
    }
    if namesakes.iter().any(|namesake| *namesake) {
        for (index, record) in records.iter().enumerate() {
            if let Some(parent) = record.parent_index {
                if parent >= index {
                    return None;
                }
                namesakes[index] |= namesakes[parent];
            }
        }
    }
    Some(namesakes)
}

/// The records of a state under the ids a store holds its instances under,
/// each written as the encoder of documents writes that id.
fn under_stored_ids<'a>(
    records: Vec<SettingsBinaryInstance<'a>>,
    stored: &[String],
) -> Vec<SettingsBinaryInstance<'a>> {
    records
        .into_iter()
        .zip(stored)
        .map(|(record, id)| SettingsBinaryInstance {
            settings_id: settings_binary_id_of(id),
            ..record
        })
        .collect()
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
        .map_init(
            || SharedValueCache {
                shared: &shared,
                local: AHashMap::new(),
            },
            |cache, (index, instance)| {
                let native = native_properties_by_instance
                    .as_ref()
                    .and_then(|rows| rows.get(index));
                (
                    instance_state_digest(instance, native, viewport_camera == Some(index), cache),
                    instance_encodes_as_its_document(instance, native, root_service),
                )
            },
        )
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
    // A property a typed row holds twice is written twice, where a document
    // holds the one of them that a store reads back.
    let held_once = native.is_none_or(|values| {
        values
            .iter()
            .enumerate()
            .all(|(at, property)| values[..at].iter().all(|other| other.name != property.name))
    });
    held_once
        && !(holds("TextScaled") == Some(true) && holds("TextWrapped") != Some(true))
        && values_encode_alike(&instance.properties, &instance.attributes, root_service)
}

/// The digests of the values many instances share, by where each value is
/// held: one is digested once however many instances hold it.
type SharedValueDigests = Mutex<AHashMap<usize, [u8; 32]>>;

/// A task reuses its recent digests without a shared lock. Misses still
/// consult the invocation's shared cache before hashing a potentially large
/// payload; neither cache survives the immutable state being digested.
struct SharedValueCache<'a> {
    shared: &'a SharedValueDigests,
    local: AHashMap<usize, [u8; 32]>,
}

impl SharedValueCache<'_> {
    fn digest(&mut self, value: &Arc<Value>) -> [u8; 32] {
        let held_at = Arc::as_ptr(value) as usize;
        if let Some(known) = self.local.get(&held_at) {
            return *known;
        }
        let known = self.shared.lock_recover().get(&held_at).copied();
        let known = known.unwrap_or_else(|| {
            let mut of_value = Sha256::new();
            crate::settings::equivalence::digest_value(&mut of_value, value);
            let of_value = <[u8; 32]>::from(of_value.finalize());
            self.shared.lock_recover().insert(held_at, of_value);
            of_value
        });
        self.local.insert(held_at, known);
        known
    }
}

fn instance_state_digest(
    instance: &SnapshotInstance,
    native: Option<&Vec<NativeSettingsProperty>>,
    viewport_camera: bool,
    shared: &mut SharedValueCache<'_>,
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
    shared: &mut SharedValueCache<'_>,
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
        NativeSettingsValue::UniqueId(value) => {
            digest.update([16]);
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
        NativeSettingsValue::CFrame(values) => floats(digest, 12, values.as_ref()),
        NativeSettingsValue::Rect(values) => floats(digest, 13, values),
        NativeSettingsValue::Enum(value) => {
            digest.update([14]);
            crate::settings::equivalence::digest_text(digest, value);
        }
        NativeSettingsValue::Shared(value) => {
            digest.update([15]);
            digest.update(shared.digest(value));
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
    // Each chunk is counted as the value block of its rows.
    let instances = if records.len() <= blocks::BLOCK_ROWS {
        kept_chunk(records)?
    } else {
        records
            .par_chunks(blocks::BLOCK_ROWS)
            .map(kept_chunk)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    };
    // A v12 store holds the structure, which the header bound covers, the
    // blocks the chunks counted, and each long string once in its pool.
    let long = strings
        .iter()
        .filter(|text| text.len() >= blocks::POOL_MIN_BYTES);
    let (long, pooled) = long.fold((0_usize, 0_usize), |(count, bytes), text| {
        (count + 1, bytes.saturating_add(text.len() + 10))
    });
    let bound = wire_bytes.load(Ordering::Relaxed).saturating_add(pooled);
    check_settings_payload_len(bound, "Decoded")?;
    // Its file: each frame within zstd's bound of its content, which over all
    // frames is the bound of all content and 64 bytes a frame, with its
    // checksum, and a head of 46 bytes a frame, 26 a pool entry, and four
    // varints of ten bytes: the writer's two and the two counts.
    let frames = 1 + long + records.len().div_ceil(blocks::BLOCK_ROWS);
    let file = zstd::zstd_safe::compress_bound(bound)
        .saturating_add(frames.saturating_mul(64 + 4 + 46))
        .saturating_add(long.saturating_mul(26))
        .saturating_add(SETTINGS_BINARY_MAGIC.len() + 1 + 4 * 10 + 16);
    check_settings_file_len(file)?;
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
    let (mut table, mut groups) = (SettingsStringCounts::default(), AHashSet::new());
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
                add_count(&mut table, name);
                collect_binary_source_strings(&source, scope.lookup, &mut table)?;
                groups.insert((name, kind));
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
                instance
                    .properties
                    .insert(name.to_string(), crate::settings::held::held(value));
            }
        }
        instances.push(instance);
    }
    if let Some(wire_bytes) = scope.wire_bytes {
        // The chunk is a value block of the store: its counts, its table with
        // each short string and the hash of each long one, a property
        // id and a group head for each group, all varints of ten bytes.
        let table = (table.keys())
            .map(|text| match text.len() < blocks::POOL_MIN_BYTES {
                true => text.len() + 2,
                false => 17,
            })
            .fold(30, usize::saturating_add);
        wire.0 = (wire.0.saturating_add(table)).saturating_add(groups.len().saturating_mul(42));
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
        SettingsBinaryValueSource::Native(NativeSettingsValue::UniqueId(_)) => {
            normalize_complex(source, kind, false, scope, scratch)
        }
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
        NativeSettingsValue::UniqueId(_) => {
            bail!("A native identity is normalized through the object codec")
        }
        NativeSettingsValue::Enum(value) => Ok(typed_object(
            "EnumItem",
            [("name", Value::String(checked_string(value, scope.budget)?))],
        )),
        NativeSettingsValue::Ref(index) => decoded_reference(*index, scope.instance_count),
        NativeSettingsValue::Shared(value) => Ok(crate::settings::held::held_copy(value)),
        NativeSettingsValue::Vector2(_)
        | NativeSettingsValue::Vector3(_)
        | NativeSettingsValue::UDim(_)
        | NativeSettingsValue::UDim2(_)
        | NativeSettingsValue::Color3(_)
        | NativeSettingsValue::Rect(_)
        | NativeSettingsValue::CFrame(_) => {
            fixed_native_value(value).context("Expected fixed native value")
        }
    }
}

/// A typed value of several floats as a document holds it.
pub(super) fn fixed_native_value(value: &NativeSettingsValue) -> Option<Value> {
    Some(fixed_value(
        fixed_numeric_kind_from_tag(native_value_kind(value))?,
        native_fixed_components(value)?,
    ))
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
