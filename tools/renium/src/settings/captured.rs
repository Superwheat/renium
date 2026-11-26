use super::*;
use crate::settings::equivalence::{CanonicalizeCache, DigestSink, SettingsAlignment};
use crate::snapshot::types::NativeSettingsProperty;
use sha2::{Digest, Sha256};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::sync::LazyLock;

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
    let summary = collect_settings_binary_summary(state, &lookup, &records)?;
    let budget = if let Ok(extra_budget) = proven_budget(&records, &lookup, &summary) {
        extra_budget
    } else {
        DecodeBudget::for_file(0)
    };
    match convert_prepared(state, &records, &lookup, &summary, &budget) {
        Ok(document) => Ok(document),
        Err(_) => {
            let _span = crate::app::profile::span("captured store exact limit check");
            drop(summary);
            if records.len() >= 32_768 {
                crate::daemon::memory::release_between_phases();
            }
            let encoded = blocks::encode_state(state, (&records, &lookup), None, None)?;
            if records.len() >= 32_768 {
                crate::daemon::memory::release_between_phases();
            }
            decode_settings_bytecode_holding(&encoded)
        }
    }
}

/// Returns a `DecodeBudget` strictly bounded by what `decode_settings_bytecode`
/// would allow on the encoded v12 file of `records` and `summary`, reserving
/// headroom for block frames, pooled-string table copies, and shared payloads.
/// Only computes the structure+pool compressed lower bound when `records` is
/// large enough to potentially exceed `DecodeBudget::for_file(0)`.
fn proven_budget(
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    summary: &SettingsBinarySummary<'_>,
) -> Result<DecodeBudget> {
    let base = DecodeBudget::for_file(0).remaining.load(Ordering::Relaxed);
    let estimated = records
        .len()
        .saturating_mul(INSTANCE_BYTES)
        .saturating_add(summary.copied_string_bytes)
        .saturating_add(
            summary
                .property_group_counts
                .iter()
                .filter(|((name, _), _)| !SETTINGS_RECORD_FIELDS.contains(name))
                .map(|((name, kind), &count)| {
                    count.saturating_mul(VALUE_SLOT_BYTES + name.len() + value_kind_bytes(*kind))
                })
                .fold(0usize, usize::saturating_add),
        );
    if estimated < (base / 10).saturating_mul(9) {
        return Ok(DecodeBudget::new(base));
    }
    let long_strings = summary
        .string_counts
        .keys()
        .map(Cow::as_ref)
        .filter(|text| text.len() >= blocks::POOL_MIN_BYTES)
        .collect::<Vec<_>>();
    let pooled_bytes = long_strings
        .iter()
        .map(|text| text.len())
        .fold(0usize, usize::saturating_add);
    let block_count = records.len().div_ceil(blocks::BLOCK_ROWS);
    let min_file_bytes = blocks::min_encoded_file_bytes(records, lookup, &long_strings)?;
    let total_budget = DecodeBudget::for_file(min_file_bytes)
        .remaining
        .load(Ordering::Relaxed);
    let short_bytes = summary
        .string_counts
        .keys()
        .map(|text| text.len())
        .filter(|&len| len < blocks::POOL_MIN_BYTES)
        .fold(0usize, usize::saturating_add);
    let reserve = MAX_SETTINGS_BYTECODE_BYTES
        .saturating_add(block_count.saturating_mul(pooled_bytes))
        .saturating_add(block_count.saturating_mul(short_bytes.min(MAX_SETTINGS_BYTECODE_BYTES)));
    let proved = total_budget.saturating_sub(reserve);
    Ok(DecodeBudget::new(proved.max(base)))
}

#[cfg(test)]
pub(super) fn convert_with_budget(
    state: &ServiceState,
    budget: &DecodeBudget,
) -> Result<SettingsBytecode> {
    let records = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let summary = collect_settings_binary_summary(state, &lookup, &records)?;
    convert_prepared(state, &records, &lookup, &summary, budget)
}

/// A digest of everything this state holds, except what its viewport camera
/// holds: a pull puts the stored camera back whatever the captured one says.
/// A store's document comes from its state alone, so two states with one
/// digest give one document apart from that camera, and no document is built
/// to find that out. No decoder memory limit applies: the digest only says
/// whether this is a state a pull took before, and that pull decided whether
/// its store may be written.
pub(super) fn pulled_digest(state: &ServiceState) -> Result<PulledDigest> {
    use crate::settings::equivalence::{instance_lacks_enum_types, workspace_current_camera_index};

    let _span = crate::app::profile::span("captured store digest");
    let records = collect_service_settings_binary_instances(state);
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let (viewport_camera, camera_lacks_enum_types) = validate_records_and_camera(
        state,
        &records,
        &lookup,
        workspace_current_camera_index,
        instance_lacks_enum_types,
    )?;
    let (digest, rows) = state_digests(state, viewport_camera);
    Ok(PulledDigest {
        digest,
        viewport_camera_lacks_enum_types: camera_lacks_enum_types,
        rows: Arc::new(rows),
    })
}

/// Validates `records` (string lengths, unique settings ids, hierarchy cycles/depth/single root)
/// without allocating `SettingsBytecodeInstance`s for all 140k+ rows, and converts only the
/// root and `Camera` instance(s) to find the viewport camera and check its enum types.
fn validate_records_and_camera(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    camera_index_of: fn(&SettingsBytecode) -> Option<usize>,
    lacks_enum_types: fn(&SettingsBytecodeInstance) -> bool,
) -> Result<(Option<usize>, bool)> {
    let mut prev_debug: Option<u64> = None;
    let mut all_debug = true;
    let mut strictly_inc = true;
    let mut root_count = 0_usize;
    for (index, record) in records.iter().enumerate() {
        if record.parent_index.is_none() {
            root_count += 1;
        }
        check_settings_string_len(record.name.len())?;
        check_settings_string_len(record.class_name.len())?;
        match &record.settings_id {
            SettingsBinaryId::NumericDebug(id) => {
                if prev_debug.is_some_and(|p| *id <= p) {
                    strictly_inc = false;
                }
                prev_debug = Some(*id);
            }
            SettingsBinaryId::Text(text) => {
                if text.is_empty() {
                    bail!("Settings bytecode instance {index} has an empty settings id");
                }
                check_settings_string_len(text.len())?;
                all_debug = false;
                strictly_inc = false;
            }
        }
    }
    if !records.is_empty() && root_count != 1 {
        bail!("Settings bytecode must contain exactly one root, found {root_count}");
    }
    if !strictly_inc {
        if all_debug {
            let mut seen = AHashSet::with_capacity(records.len());
            for record in records {
                if let SettingsBinaryId::NumericDebug(id) = record.settings_id
                    && !seen.insert(id)
                {
                    bail!(
                        "Settings bytecode contains duplicate settings id {}",
                        format_debug_id(id)
                    );
                }
            }
        } else {
            let mut seen = AHashSet::with_capacity(records.len());
            for record in records {
                let id: Cow<'_, str> = match &record.settings_id {
                    SettingsBinaryId::Text(t) => Cow::Borrowed(t.as_ref()),
                    SettingsBinaryId::NumericDebug(id) => Cow::Owned(format_debug_id(*id)),
                };
                if !seen.insert(id.clone()) {
                    bail!("Settings bytecode contains duplicate settings id {id}");
                }
            }
        }
    }
    let n = records.len();
    let mut states = vec![0_u8; n];
    let mut depths = vec![0_usize; n];
    let mut path = Vec::new();
    for start in 0..n {
        if states[start] == 2 {
            continue;
        }
        path.clear();
        let mut current = Some(start);
        let mut base_depth = 0_usize;
        while let Some(index) = current {
            if index >= n {
                bail!("Invalid parent index {index}");
            }
            match states[index] {
                0 => {
                    states[index] = 1;
                    path.push(index);
                    current = records[index].parent_index;
                }
                1 => bail!("Settings bytecode hierarchy contains a parent cycle at index {index}"),
                2 => {
                    base_depth = depths[index];
                    break;
                }
                _ => unreachable!(),
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
    let budget = DecodeBudget::new(MAX_SETTINGS_BYTECODE_BYTES);
    let scope = ConversionScope {
        lookup,
        instance_count: n,
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let mut root_idx = None;
    let mut cameras: Vec<(usize, SettingsBytecodeInstance)> = Vec::new();
    let mut root_inst = None;
    for (idx, record) in records.iter().enumerate() {
        if record.parent_index.is_none() {
            let mut converted = convert_chunk(state, std::slice::from_ref(record), scope)?;
            root_idx = Some(idx);
            root_inst = converted.pop();
        } else if record.class_name == "Camera" {
            let mut converted = convert_chunk(state, std::slice::from_ref(record), scope)?;
            if let Some(cam) = converted.pop() {
                cameras.push((idx, cam));
            }
        }
    }
    let Some(root_inst) = root_inst else {
        return Ok((None, false));
    };
    let Some(root_idx) = root_idx else {
        return Ok((None, false));
    };
    if root_inst.class_name != "Workspace" || cameras.is_empty() {
        let _ = (root_idx, camera_index_of);
        return Ok((None, false));
    }
    if let Some(current_cam) = root_inst.properties.get("CurrentCamera")
        && let Some(obj) = current_cam.as_object()
        && obj.get("_type").and_then(Value::as_str) == Some("Ref")
        && let Some(one_based) = obj.get("instanceIndex").and_then(Value::as_u64)
        && let Some(cam_idx) = (one_based as usize).checked_sub(1)
        && let Some((_, cam_inst)) = cameras.iter().find(|(idx, _)| *idx == cam_idx)
        && records[cam_idx].parent_index == Some(root_idx)
    {
        return Ok((
            Some(records[cam_idx].source_index),
            lacks_enum_types(cam_inst),
        ));
    }
    let skel = skeleton(state, records, lookup)?;
    let cam = camera_index_of(&skel);
    let lacks = cam.is_some_and(|i| lacks_enum_types(&skel.instances[i]));
    Ok((cam.map(|i| records[i].source_index), lacks))
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

/// The changed instances a pull holds whole on both sides at one time while
/// it compares them one by one: what each pair says is kept, and the pair
/// is dropped before the next ones are read.
const COMPARED_AT_ONCE: usize = if cfg!(test) { 3 } else { 4_096 };

/// What a changed instance and the stored one at its place say of each
/// other.
#[derive(Clone, Copy)]
pub(super) struct ComparedPair {
    /// The two stand under one name, class and parent.
    pub(super) placed: bool,
    pub(super) same_class: bool,
    /// The stored one is written as documents are today.
    pub(super) written_as_today: bool,
    pub(super) same_unique_id: bool,
    /// One of the two holds a reference.
    pub(super) holds_reference: bool,
}

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
    let rows = (records, &places, &(|| false) as &dyn Fn() -> bool);
    changed_rows(state, now, rows, &changed, stored, None, store_gate)
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

/// Whether each stored instance stands under the name, class and parent of
/// the state's instance at its place.
fn stand_alike(
    stored: &[SettingsBytecodeInstance],
    records: &[SettingsBinaryInstance<'_>],
) -> bool {
    stored.len() == records.len()
        && stored.iter().zip(records).all(|(stored, record)| {
            stored.name == record.name
                && stored.class_name == record.class_name
                && stored.parent_index == record.parent_index
        })
}

/// How the stored instances stand among the state's, each paired by itself,
/// and the id the alignment gives each of the state's
/// (`equivalence::settings_ids_told_alone`). Of the store only the instances
/// with their UniqueIds, and the camera a root names, are read. `None` where
/// the whole documents have to say.
fn told_alone_with_reference(
    reference: &SettingsBytecode,
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Option<(Moved, Vec<String>)> {
    let mut observed = skeleton(state, records, lookup).ok()?;
    for (index, instance) in observed.instances.iter_mut().enumerate() {
        if !instance.properties.contains_key(UNIQUE_ID)
            && let Some(id) = captured_persistent_identity(state, index)
        {
            instance
                .properties
                .insert(UNIQUE_ID.into(), json!({"_type": "UniqueId", "value": id}));
        }
    }
    let (ids, in_store) =
        crate::settings::equivalence::settings_ids_told_alone(reference, &observed)?;
    let mut in_state = vec![None; reference.instances.len()];
    for (index, stored) in in_store.into_iter().enumerate() {
        if let Some(stored) = stored {
            in_state[stored] = Some(index);
        }
    }
    Some((Moved::paired(in_state, records.len()), ids))
}

fn told_alone_in_block_store(
    store: &blocks::BlockStore<'_>,
    instances: &mut Vec<SettingsBytecodeInstance>,
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Option<(Moved, Vec<String>)> {
    let _span = crate::app::profile::span("store instances told alone");
    let mut reference = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: std::mem::take(instances),
    };
    let (ref_uids, obs_pair) = rayon::join(
        || store.extract_unique_ids_and_root_camera(&mut reference.instances),
        || {
            let observed = skeleton_parallel(state, records, lookup).ok()?;
            let obs_uids: Vec<Option<Box<str>>> = (0..records.len())
                .into_par_iter()
                .map(|index| {
                    let instance = &observed.instances[index];
                    if let Some(uid) = instance.properties.get(UNIQUE_ID).and_then(|v| {
                        v.as_object()
                            .filter(|o| o.get("_type").and_then(Value::as_str) == Some("UniqueId"))
                            .and_then(|o| o.get("value").and_then(Value::as_str))
                    }) {
                        let trimmed = uid.trim();
                        if !trimmed.is_empty() && !trimmed.chars().all(|c| c == '0') {
                            return Some(trimmed.into());
                        }
                    }
                    captured_persistent_identity(state, index)
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '0'))
                        .map(Box::<str>::from)
                })
                .collect();
            Some((observed, obs_uids))
        },
    );
    let result = ref_uids
        .ok()
        .zip(obs_pair)
        .and_then(|(ref_uids, (observed, obs_uids))| {
            let (ids, in_store) =
                crate::settings::equivalence::settings_ids_told_alone_with_unique_ids(
                    &reference, &observed, &ref_uids, &obs_uids,
                )?;
            let mut in_state = vec![None; reference.instances.len()];
            for (index, stored) in in_store.into_iter().enumerate() {
                if let Some(stored) = stored {
                    in_state[stored] = Some(index);
                }
            }
            Some((Moved::paired(in_state, records.len()), ids))
        });
    if result.is_some() {
        for instance in &mut reference.instances {
            if !instance.properties.is_empty() {
                instance.properties.clear();
            }
        }
        *instances = reference.instances;
    }
    result
}

fn told_alone(
    stored: &[u8],
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Option<(Moved, Vec<String>)> {
    use crate::settings::bytecode::property_edit::decode_for_row_chunks;
    let _span = crate::app::profile::span("store instances told alone");
    let (mut reference, columns) = decode_for_row_chunks(stored).ok()?;
    columns
        .project_properties(
            &mut reference,
            |name| name == UNIQUE_ID || name == "CurrentCamera",
            |instance, name, value| {
                (name == UNIQUE_ID || instance.parent_index.is_none()).then_some(value)
            },
        )
        .ok()?;
    told_alone_with_reference(&reference, state, records, lookup)
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
    let opened_store = if blocks::is_block_store(stored) {
        Some(blocks::BlockStore::open(stored)?.holding())
    } else {
        None
    };
    let mut told_ids = None;
    let (stored_rows, (captured_rows, long_hashes)) = if let Some(store) = opened_store.as_ref() {
        let mut instances = store.structure()?.instances;
        let moved = match Moved::between(&instances, &records) {
            Some(None) if stand_alike(&instances, &records) => Some(None),
            Some(Some(run)) => Some(Some(run)),
            _ => told_alone_in_block_store(store, &mut instances, state, &records, &lookup).map(
                |(moved, ids)| {
                    told_ids = Some(ids);
                    Some(moved)
                },
            ),
        };
        let Some(moved) = moved else {
            drop(instances);
            drop(opened_store);
            if records.len() >= 32_768 {
                crate::daemon::memory::release_between_phases();
            }
            return whole("nothing tells every instance of the store by itself among the state's");
        };
        let camera = match (now.viewport_camera, &moved) {
            (Some(camera), Some(moved)) => match moved.in_store(camera) {
                Some(in_store) => Some(in_store),
                None => {
                    return whole(
                        "nothing tells every instance of the store by itself among the state's",
                    );
                }
            },
            (camera, _) => camera,
        };
        let own_ids = moved.is_none()
            && told_ids.is_none()
            && places
                .ids
                .iter()
                .zip(&instances)
                .all(|(id, inst)| id == &inst.settings_id);
        if own_ids && !places.kept_under_their_ids() {
            let unresolvable = namesakes(&records).is_some_and(|has_namesake| {
                let mut stored_id_set: Option<AHashSet<&str>> = None;
                has_namesake
                    .iter()
                    .enumerate()
                    .any(|(index, &is_namesake)| {
                        if !is_namesake || places.told_alone[index] {
                            return false;
                        }
                        let id = places.ids[index].as_str();
                        id.starts_with("debug:")
                            || !stored_id_set
                                .get_or_insert_with(|| {
                                    instances.iter().map(|i| i.settings_id.as_str()).collect()
                                })
                                .contains(id)
                    })
            });
            if unresolvable {
                return whole("nothing tells every instance for the stored one at its place");
            }
        }
        if records.len() >= 1_024 {
            let mut probe_pairs: Vec<(usize, usize)> = records
                .iter()
                .enumerate()
                .filter(|(index, record)| {
                    record.parent_index.is_none() && now.viewport_camera != Some(*index)
                })
                .filter_map(|(index, _)| {
                    let stored_idx = moved.as_ref().map_or(Some(index), |m| m.in_store(index))?;
                    Some((index, stored_idx))
                })
                .collect();
            if own_ids {
                for index in 0..records.len().min(32) {
                    if now.viewport_camera != Some(index)
                        && !probe_pairs.iter().any(|&(o, _)| o == index)
                    {
                        probe_pairs.push((index, index));
                    }
                }
            }
            if !probe_pairs.is_empty() {
                let stored_probe_indices: Vec<usize> =
                    probe_pairs.iter().map(|&(_, s)| s).collect();
                let budget = DecodeBudget::for_file(stored.len());
                let scope = ConversionScope {
                    lookup: &lookup,
                    instance_count: records.len(),
                    budget: &budget,
                    string_ids: &SettingsStringIdMap::default(),
                    wire_bytes: None,
                };
                let stored_probe_rows: AHashMap<usize, DecodedRowValues> =
                    store.rows(&stored_probe_indices)?.into_iter().collect();
                let mut root_differs = false;
                let mut probed_renamed = false;
                for &(obs_idx, stored_idx) in &probe_pairs {
                    let Some(values) = stored_probe_rows.get(&stored_idx) else {
                        continue;
                    };
                    let base = &instances[stored_idx];
                    let mut stored_inst = SettingsBytecodeInstance {
                        settings_id: base.settings_id.clone(),
                        name: base.name.clone(),
                        class_name: base.class_name.clone(),
                        parent_index: base.parent_index,
                        properties: values.properties.clone(),
                        attributes: values.attributes.clone(),
                    };
                    if let Some(m) = moved.as_ref() {
                        m.in_state_whole(&mut stored_inst);
                    }
                    let obs_inst =
                        convert_chunk(state, std::slice::from_ref(&records[obs_idx]), scope)?
                            .into_iter()
                            .next()
                            .context("Expected converted probed instance")?;
                    let (stored_val, stored_uid) = document_row_digest(&stored_inst);
                    let (obs_val, obs_uid) = document_row_digest(&obs_inst);
                    if stored_val != obs_val {
                        if records[obs_idx].parent_index.is_none() {
                            root_differs = true;
                            break;
                        }
                    } else if stored_uid != obs_uid {
                        probed_renamed = true;
                    }
                }
                if root_differs {
                    return whole("a root differs");
                }
                if own_ids && probed_renamed {
                    let in_place =
                        told_alone_in_block_store(store, &mut instances, state, &records, &lookup)
                            .is_some_and(|(m, _)| {
                                (0..records.len()).all(|i| m.in_store(i) == Some(i))
                            });
                    if !in_place {
                        return whole(
                            "nothing tells every instance for the stored one at its place",
                        );
                    }
                }
            }
        }
        let (stored_rows, captured_res) = rayon::join(
            || {
                let _span = crate::app::profile::span("store instance digests");
                store
                    .row_digests_of_instances_with_structure(instances, false, &[], camera, moved)
                    .map(Some)
            },
            || document_row_digests(state, &records, &lookup, stored.len()),
        );
        (stored_rows?, captured_res?)
    } else {
        let (stored_rows, captured_res) = rayon::join(
            || {
                crate::settings::bytecode::property_edit::row_digests(stored, |instances| {
                    let moved = match Moved::between(instances, &records) {
                        Some(None) if stand_alike(instances, &records) => None,
                        Some(Some(run)) => Some(run),
                        _ => {
                            let (moved, ids) = told_alone(stored, state, &records, &lookup)?;
                            told_ids = Some(ids);
                            Some(moved)
                        }
                    };
                    let camera = match (now.viewport_camera, &moved) {
                        (Some(camera), Some(moved)) => Some(moved.in_store(camera)?),
                        (camera, _) => camera,
                    };
                    Some((camera, moved))
                })
            },
            || document_row_digests(state, &records, &lookup, stored.len()),
        );
        (stored_rows?, captured_res?)
    };
    let Some(stored_rows) = stored_rows else {
        return whole("nothing tells every instance of the store by itself among the state's");
    };
    let moved = stored_rows.moved;
    let paired = told_ids.is_some();
    // Where the places alone do not say that each instance is the stored
    // one at its place, the alignment's own matching can.
    let file = stored;
    let unresolvable_alone = namesakes(&records).is_some_and(|has_namesake| {
        let mut stored_id_set: Option<AHashSet<&str>> = None;
        has_namesake
            .iter()
            .enumerate()
            .any(|(index, &is_namesake)| {
                if !is_namesake || places.told_alone[index] {
                    return false;
                }
                let id = places.ids[index].as_str();
                id.starts_with("debug:")
                    || !stored_id_set
                        .get_or_insert_with(|| stored_rows.ids.iter().map(String::as_str).collect())
                        .contains(id)
            })
    });
    let told_in_place = || {
        if unresolvable_alone {
            return false;
        }
        let records = collect_service_settings_binary_instances(state);
        let lookup = build_settings_binary_instance_lookup(state, &records);
        told_alone(file, state, &records, &lookup).is_some_and(|(moved, _)| {
            (0..records.len()).all(|index| moved.in_store(index) == Some(index))
        })
    };
    let mut changed = Vec::new();
    let mut renamed = false;
    let mut exact_row_match = if opened_store.is_some()
        && moved.is_none()
        && !paired
        && stored_rows.engine_digests.len() == records.len()
    {
        Some(vec![true; records.len()])
    } else {
        None
    };
    for (index, (values, unique_id, engine)) in captured_rows.iter().enumerate() {
        if now.viewport_camera == Some(index) {
            continue;
        }
        // An instance the store does not hold yet has nothing to differ from.
        let stored_index = (moved.as_ref()).map_or(Some(index), |moved| moved.in_store(index));
        let Some(stored_index) = stored_index else {
            continue;
        };
        if stored_rows.digests[stored_index] != *values {
            // A root that differs can name another viewport camera than the
            // one left out here, which only the whole documents show.
            if records[index].parent_index.is_none() {
                return whole("a root differs");
            }
            changed.push(index);
            if let Some(exact) = exact_row_match.as_mut() {
                exact[index] = false;
            }
        } else {
            let uid_diff = stored_rows.unique_ids[stored_index] != *unique_id;
            renamed |= uid_diff;
            if let Some(exact) = exact_row_match.as_mut()
                && (uid_diff || stored_rows.engine_digests[stored_index] != *engine)
            {
                exact[index] = false;
            }
        }
    }
    let stored = StoredRows {
        file: stored,
        read: Some(ReadStore {
            viewport_camera: stored_rows.kept,
            ids: stored_rows.ids,
            unique_ids: stored_rows.unique_ids,
            captured_unique_ids: captured_rows
                .into_iter()
                .map(|(_, unique_id, _)| unique_id)
                .collect(),
            renamed,
            moved,
            told_ids,
            opened_store,
            structure: stored_rows.structure,
            block_pool_entries: stored_rows.block_pool_entries,
            exact_row_match,
            long_hashes: Some(long_hashes),
        }),
    };
    // A store kept with its instances at other places is not the state's
    // rows in their order.
    let rows = (records, &places, &told_in_place as &dyn Fn() -> bool);
    Ok(changed_rows(
        state,
        now,
        rows,
        &changed,
        stored,
        Some(&lookup),
        store_gate,
    )?
    .map(|alignment| (alignment, changed.is_empty() && !renamed && !paired)))
}

/// Replaces stabilized `settingsId` references inside a saved viewport camera
/// with dense 1-based `instanceIndex` references pointing into `aligned_ids`,
/// and resolves any `held` handles so `blocks::encode_state` writes the exact
/// bytes `encode_settings_bytecode_over` writes for that camera.
fn prepare_camera_for_encode(
    saved: &SettingsBytecodeInstance,
    aligned_ids: &[String],
    root_service: Option<&str>,
) -> SettingsBytecodeInstance {
    fn remap_refs(value: &mut Value, by_id: &AHashMap<&str, usize>, root_service: Option<&str>) {
        match value {
            Value::Object(map) => {
                if map.get("_type").and_then(Value::as_str) == Some("Ref")
                    && let Some(id) = map
                        .get("settingsId")
                        .or_else(|| map.get("instanceId"))
                        .and_then(Value::as_str)
                {
                    let same_service = map
                        .get("pathSegments")
                        .and_then(Value::as_array)
                        .and_then(|segs| segs.first())
                        .and_then(Value::as_str)
                        .is_none_or(|svc| root_service.is_none_or(|root| svc == root));
                    if same_service && let Some(&obs_idx) = by_id.get(id) {
                        map.clear();
                        map.insert("_type".to_string(), Value::String("Ref".to_string()));
                        map.insert("instanceIndex".to_string(), json!(obs_idx + 1));
                        return;
                    }
                }
                for child in map.values_mut() {
                    remap_refs(child, by_id, root_service);
                }
            }
            Value::Array(items) => {
                for child in items {
                    remap_refs(child, by_id, root_service);
                }
            }
            _ => {}
        }
    }

    let mut camera = saved.clone();
    camera
        .properties
        .values_mut()
        .for_each(crate::settings::held::resolve_all);
    camera
        .attributes
        .values_mut()
        .for_each(crate::settings::held::resolve_all);
    let has_ref = camera
        .properties
        .values()
        .chain(camera.attributes.values())
        .any(crate::settings::equivalence::reconciliation_value_contains_reference);
    if has_ref {
        let by_id: AHashMap<&str, usize> = aligned_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        for value in camera.properties.values_mut() {
            remap_refs(value, &by_id, root_service);
        }
        for value in camera.attributes.values_mut() {
            remap_refs(value, &by_id, root_service);
        }
    }
    camera
}

/// Streams a captured `ServiceState` in bounded batches into a compact
/// representation for streamed alignment without allocating full documents.
fn compact_captured_state(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<(crate::settings::equivalence::CompactDocument, Vec<String>)> {
    use crate::settings::equivalence::{
        CompactDocument, CompactRowMeta, compact_row, stabilize_instance_references,
    };
    const COMPACT_BATCH_ROWS: usize = 4_096;
    const COMPACT_SUBCHUNK_ROWS: usize = 256;

    let _span = crate::app::profile::span("captured store compact document");
    let unlimited = DecodeBudget::new(usize::MAX);
    let ids: Vec<String> = records
        .iter()
        .map(|record| Ok(bare_instance(record, &unlimited)?.settings_id))
        .collect::<Result<Vec<_>>>()?;
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &unlimited,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let count = records.len();
    let mut instances = Vec::with_capacity(count);
    let mut unique_ids: Vec<Option<Box<str>>> = Vec::with_capacity(count);
    let mut meta: Vec<CompactRowMeta> = Vec::with_capacity(count);
    let mut cameras: Vec<(usize, SettingsBytecodeInstance)> = Vec::new();

    for (batch_idx, batch) in records.chunks(COMPACT_BATCH_ROWS).enumerate() {
        let batch_base = batch_idx * COMPACT_BATCH_ROWS;
        let subchunks: Vec<_> = batch
            .par_chunks(COMPACT_SUBCHUNK_ROWS)
            .enumerate()
            .map(|(sub_idx, sub)| {
                let sub_base = batch_base + sub_idx * COMPACT_SUBCHUNK_ROWS;
                let mut converted = convert_chunk(state, sub, scope)?;
                let mut local_uids = Vec::with_capacity(converted.len());
                let mut local_meta = Vec::with_capacity(converted.len());
                let mut local_cams = Vec::new();
                for (offset, inst) in converted.iter_mut().enumerate() {
                    stabilize_instance_references(inst, &ids);
                    let (uid, row_meta, cam) = compact_row(inst);
                    local_uids.push(uid);
                    local_meta.push(row_meta);
                    if let Some(cam) = cam {
                        local_cams.push((sub_base + offset, cam));
                    }
                }
                Ok((converted, local_uids, local_meta, local_cams))
            })
            .collect::<Result<Vec<_>>>()?;
        for (converted, local_uids, local_meta, local_cams) in subchunks {
            instances.extend(converted);
            unique_ids.extend(local_uids);
            meta.extend(local_meta);
            cameras.extend(local_cams);
        }
    }
    validate_settings_hierarchy(&instances)?;
    Ok((
        CompactDocument {
            document: SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            },
            unique_ids,
            meta,
            cameras,
        },
        ids,
    ))
}

fn hydrate_captured_rows(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    obs_ids: &[String],
    rows: &[usize],
) -> Result<Vec<(usize, SettingsBytecodeInstance)>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut sorted = rows.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let unlimited = DecodeBudget::new(usize::MAX);
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &unlimited,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    sorted
        .par_chunks(64)
        .map(|chunk| {
            let batch: Vec<SettingsBinaryInstance<'_>> =
                chunk.iter().map(|&row| records[row].clone()).collect();
            let converted = convert_chunk(state, &batch, scope)?;
            Ok(chunk
                .iter()
                .copied()
                .zip(converted)
                .map(|(row, mut inst)| {
                    crate::settings::equivalence::stabilize_instance_references(&mut inst, obs_ids);
                    (row, inst)
                })
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.into_iter().flatten().collect())
}

pub(super) fn align_captured_store_streaming(
    state: &ServiceState,
    now: &PulledRows,
    stored: &[u8],
    store_gate: Option<&Mutex<()>>,
    started: std::time::Instant,
) -> Result<Option<(crate::settings::equivalence::PulledAlignment, bool)>> {
    use crate::settings::equivalence::{PulledAlignment, StreamingAlignmentPlan};

    if !blocks::is_block_store(stored) || !comparable_by_instance(state, now) {
        return Ok(None);
    }
    let records = collect_service_settings_binary_instances(state);
    if !written_where_they_stand(&records, now) {
        return Ok(None);
    }
    let instance_count = records.len();
    let lookup = build_settings_binary_instance_lookup(state, &records);
    let root_service = lookup.root_service.clone();
    let store = blocks::BlockStore::open(stored)?.holding();
    let (ref_res, obs_res) = rayon::join(
        || store.compact(),
        || compact_captured_state(state, &records, &lookup),
    );
    let (ref_compact, ref_ids) = ref_res?;
    let (obs_compact, obs_ids) = obs_res?;
    let ref_bare: Vec<SettingsBytecodeInstance> = ref_compact
        .document
        .instances
        .iter()
        .map(SettingsBytecodeInstance::bare)
        .collect();
    let hydrate_ref = |rows: &[usize]| store.hydrate_rows(&ref_bare, &ref_ids, rows);
    let hydrate_obs =
        |rows: &[usize]| hydrate_captured_rows(state, &records, &lookup, &obs_ids, rows);
    let plan = crate::settings::equivalence::plan_captured_store_streaming(
        ref_compact,
        obs_compact,
        &hydrate_ref,
        &hydrate_obs,
        started,
    )?;
    drop((ref_bare, ref_ids, obs_ids, store, lookup, records));

    match plan {
        StreamingAlignmentPlan::Equivalent {
            stored_as_observed,
            answer_follows_camera,
        } => Ok(Some((
            PulledAlignment {
                alignment: SettingsAlignment::Equivalent,
                stored_as_observed,
            },
            answer_follows_camera,
        ))),
        StreamingAlignmentPlan::Changed {
            aligned_ids,
            kept_camera,
            stored_as_observed,
            answer_follows_camera,
        } => {
            let encode_started = std::time::Instant::now();
            let _store_guard = store_gate.map(LockRecover::lock_recover);
            let records = collect_service_settings_binary_instances(state);
            let aligned_records = under_stored_ids(records, &aligned_ids);
            let aligned_lookup = build_settings_binary_instance_lookup(state, &aligned_records);
            let prepared_camera = kept_camera.map(|(idx, saved)| {
                (
                    idx,
                    prepare_camera_for_encode(&saved, &aligned_ids, root_service.as_deref()),
                )
            });
            let replaced = prepared_camera.as_ref().map(|(idx, cam)| ReplacedRow {
                source_index: *idx,
                instance: cam,
            });
            let aligned = blocks::encode_state(
                state,
                (&aligned_records, &aligned_lookup),
                replaced,
                Some(stored),
            )?;
            crate::app::profile::record(
                &format!("settings alignment of {instance_count} instances: encode"),
                encode_started,
            );
            Ok(Some((
                PulledAlignment {
                    alignment: SettingsAlignment::Changed(aligned),
                    stored_as_observed,
                },
                answer_follows_camera,
            )))
        }
    }
}

/// A stored file and, where a caller read the two documents already, what
/// it read of them.
struct StoredRows<'a> {
    file: &'a [u8],
    read: Option<ReadStore<'a>>,
}

pub(super) type LongHashMap = blocks::ShardedMap<(usize, usize), [u8; 16]>;
type CapturedRowDigest = ([u8; 16], [u8; 16], [u8; 16]);
type FastCompareEntry = Option<(ComparedPair, (bool, bool))>;
type ComparedRowResult = Result<Option<(bool, bool)>>;

/// What the digests of two documents say beside which instances differ.
struct ReadStore<'a> {
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
    /// of instances more than the store, or one less, or holds them at
    /// other places.
    moved: Option<Moved>,
    /// The id the alignment gives each instance of the state, where `moved`
    /// pairs each by itself.
    told_ids: Option<Vec<String>>,
    /// Already-opened v12 store with its pool cache, reused for row reading and encoding.
    opened_store: Option<blocks::BlockStore<'a>>,
    /// Already-decoded v12 bare structure, reused so `RowBatches` does not decode it again.
    structure: Option<Vec<SettingsBytecodeInstance>>,
    /// Pool entry indices named by each block of the v12 store.
    block_pool_entries: Vec<Vec<usize>>,
    /// When `moved.is_none() && !paired`, whether each non-camera row matches the stored row
    /// in every digested property, UniqueId, and engine-state property.
    exact_row_match: Option<Vec<bool>>,
    /// Precomputed SHA-256 hashes of borrowed long strings (`>= POOL_MIN_BYTES`) in `state`,
    /// reused by `fast_compare_changed` and `encode_state_streaming`.
    long_hashes: Option<Arc<LongHashMap>>,
}

/// How the instances of a stored file stand in a state that holds other
/// instances than the file, or holds them at other places.
#[derive(Clone)]
pub(crate) enum Moved {
    /// The state holds one run of instances the file does not hold, or
    /// lacks one run the file holds: the run starts at `at` and is `by` long.
    Run { at: usize, by: usize, added: bool },
    /// Each stored instance stands where `in_state` says, and each of the
    /// state's is the stored one `in_store` says: `u32::MAX` for an instance
    /// the other does not hold.
    Paired(std::sync::Arc<(Vec<u32>, Vec<u32>)>),
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
        Self::between_by(stored, records.len(), |index| {
            let record = &records[index];
            (record.name, record.class_name, record.parent_index)
        })
    }

    /// `between` for another copy of the store as the state: a push's files
    /// beside the store Studio holds.
    pub(crate) fn between_copies(
        stored: &[SettingsBytecodeInstance],
        state: &[SettingsBytecodeInstance],
    ) -> Option<Option<Self>> {
        Self::between_by(stored, state.len(), |index| {
            let instance = &state[index];
            (
                instance.name.as_str(),
                instance.class_name.as_str(),
                instance.parent_index,
            )
        })
    }

    fn between_by<'a>(
        stored: &[SettingsBytecodeInstance],
        count: usize,
        state: impl Fn(usize) -> (&'a str, &'a str, Option<usize>),
    ) -> Option<Option<Self>> {
        if stored.len() == count {
            return Some(None);
        }
        let common = stored.len().min(count);
        let at = (0..common)
            .find(|&index| {
                let (stored, (name, class_name, parent_index)) = (&stored[index], state(index));
                stored.name != name
                    || stored.class_name != class_name
                    || stored.parent_index != parent_index
            })
            .unwrap_or(common);
        let (by, added) = (stored.len().abs_diff(count), count > stored.len());
        let moved = Self::Run { at, by, added };
        (at..common)
            .all(|position| {
                let (stored_index, index) = if added {
                    (position, position + by)
                } else {
                    (position + by, position)
                };
                let (stored, (name, class_name, parent_index)) =
                    (&stored[stored_index], state(index));
                stored.name == name
                    && stored.class_name == class_name
                    && match parent_index {
                        None => stored.parent_index.is_none(),
                        Some(parent) => moved
                            .in_store(parent)
                            .is_some_and(|parent| stored.parent_index == Some(parent)),
                    }
            })
            .then_some(Some(moved))
    }

    /// The stored instances as `in_state` places each of them, for a state
    /// of `count` instances.
    pub(crate) fn paired(in_state: Vec<Option<usize>>, count: usize) -> Self {
        let mut in_store = vec![u32::MAX; count];
        let in_state = (in_state.into_iter().enumerate())
            .map(
                |(stored, state)| match state.filter(|state| *state < count) {
                    Some(state) => {
                        in_store[state] = stored as u32;
                        state as u32
                    }
                    None => u32::MAX,
                },
            )
            .collect();
        Self::Paired(std::sync::Arc::new((in_state, in_store)))
    }

    /// The run of instances one copy holds and the other lacks, as it
    /// stands in the copy that holds it, and whether that is the state.
    /// `None` for instances paired each by itself.
    pub(crate) fn run(&self) -> Option<(std::ops::Range<usize>, bool)> {
        match *self {
            Self::Run { at, by, added } => Some((at..at + by, added)),
            Self::Paired(_) => None,
        }
    }

    /// Which stored instance the state's instance at `index` is.
    pub(crate) fn in_store(&self, index: usize) -> Option<usize> {
        match *self {
            Self::Run { at, by, added } => match (added, index < at) {
                (_, true) => Some(index),
                (true, false) => index.checked_sub(by).filter(|index| *index >= at),
                (false, false) => Some(index + by),
            },
            Self::Paired(ref pairs) => (pairs.1.get(index).copied())
                .filter(|stored| *stored != u32::MAX)
                .map(|stored| stored as usize),
        }
    }

    /// Where the stored instance at `index` stands in the state.
    pub(crate) fn in_state(&self, index: usize) -> Option<usize> {
        match *self {
            Self::Run { at, by, added } => match (added, index < at) {
                (_, true) => Some(index),
                (true, false) => Some(index + by),
                (false, false) => index.checked_sub(by).filter(|index| *index >= at),
            },
            Self::Paired(ref pairs) => (pairs.0.get(index).copied())
                .filter(|state| *state != u32::MAX)
                .map(|state| state as usize),
        }
    }

    /// The parent of a stored instance as the position it has in the state.
    /// A parent the state lacks is no position of the state's.
    pub(super) fn parent_index_in_state(&self, parent_index: Option<usize>) -> Option<usize> {
        parent_index.map(|parent| self.in_state(parent).unwrap_or(usize::MAX - 1))
    }

    pub(super) fn parent_in_state(&self, instance: &mut SettingsBytecodeInstance) {
        instance.parent_index = self.parent_index_in_state(instance.parent_index);
    }

    /// A stored value with every reference by position at the position its
    /// instance has in the state: `None` where it holds none.
    pub(super) fn value_in_state(&self, value: &Value) -> Option<Value> {
        fn refers(value: &Value) -> bool {
            match value {
                Value::Object(map) => map.contains_key("instanceIndex") || map.values().any(refers),
                Value::Array(values) => values.iter().any(refers),
                _ => false,
            }
        }
        fn place(moved: &Moved, value: &mut Value) {
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
    pub(super) fn values_in_state(
        &self,
        values: &Map<String, Value>,
    ) -> Option<Map<String, Value>> {
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
    pub(crate) fn in_state_whole(&self, instance: &mut SettingsBytecodeInstance) {
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
    fn written_ids(&self, own: &[String], stored: &[String]) -> Vec<String> {
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

/// Fast deterministic 128-bit in-memory hasher (two independent keyed AHashers)
/// for comparing document rows within a process. On-disk store digests in `blocks`
/// continue to use SHA-256.
const ROW_HASH_A: ahash::RandomState = ahash::RandomState::with_seeds(
    0x243f_6a88_85a3_08d3,
    0x1319_8a2e_0370_7344,
    0xa409_3822_299f_31d0,
    0x082e_fa98_ec4e_6c89,
);
const ROW_HASH_B: ahash::RandomState = ahash::RandomState::with_seeds(
    0x4528_21e6_38d0_1377,
    0xbe54_66cf_34e9_0c6c,
    0xc0ac_29b7_c97c_50dd,
    0x3f84_d5b5_b547_0917,
);

pub(super) struct RowHasher(ahash::AHasher, ahash::AHasher);

impl RowHasher {
    #[inline]
    pub(super) fn new() -> Self {
        Self(ROW_HASH_A.build_hasher(), ROW_HASH_B.build_hasher())
    }

    #[inline]
    pub(super) fn update(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
        self.1.write(bytes);
    }

    /// Feeds a long string (`>= blocks::POOL_MIN_BYTES`) by its 16-byte SHA-256 hash
    /// (`blocks::digest`), prefixed with `u64::MAX` so it is domain-separated from all
    /// short strings (whose length is `< POOL_MIN_BYTES < u64::MAX`).
    #[inline]
    pub(super) fn update_long_hash(&mut self, hash: &[u8; 16]) {
        self.update(&u64::MAX.to_le_bytes());
        self.update(hash);
    }

    #[inline]
    pub(super) fn finish(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&self.0.finish().to_le_bytes());
        out[8..].copy_from_slice(&self.1.finish().to_le_bytes());
        out
    }
}

impl DigestSink for RowHasher {
    #[inline]
    fn update_bytes(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }

    #[inline]
    fn update_text(&mut self, text: &str) {
        if text.len() >= blocks::POOL_MIN_BYTES {
            let hash = blocks::digest(&[text.as_bytes()]);
            self.update_long_hash(&hash);
        } else {
            self.update(&(text.len() as u64).to_le_bytes());
            self.update(text.as_bytes());
        }
    }
}

/// Either a short borrowed string or a precomputed 16-byte SHA-256 hash of a long/pooled string.
#[derive(Clone, Copy)]
pub(super) enum StrOrHash<'a> {
    Text(&'a str),
    Hash([u8; 16]),
}

#[inline]
fn digest_str_or_hash_into(h: &mut RowHasher, item: StrOrHash<'_>) {
    match item {
        StrOrHash::Text(t) => crate::settings::equivalence::digest_text(h, t),
        StrOrHash::Hash(hash) => h.update_long_hash(&hash),
    }
}

#[inline]
pub(super) fn digest_long_string_hash(hash: &[u8; 16]) -> [u8; 16] {
    let mut h = RowHasher::new();
    h.update(&[5]);
    h.update_long_hash(hash);
    h.finish()
}

#[inline]
pub(super) fn digest_two_string_object_with_hashes(
    k0: &str,
    v0: StrOrHash<'_>,
    k1: &str,
    v1: StrOrHash<'_>,
) -> [u8; 16] {
    use crate::settings::equivalence::digest_text;
    let mut h = RowHasher::new();
    h.update(&[7]);
    if k0 == k1 {
        h.update(&1_u64.to_le_bytes());
        digest_text(&mut h, k1);
        h.update(&[5]);
        digest_str_or_hash_into(&mut h, v1);
    } else if k0 < k1 {
        h.update(&2_u64.to_le_bytes());
        digest_text(&mut h, k0);
        h.update(&[5]);
        digest_str_or_hash_into(&mut h, v0);
        digest_text(&mut h, k1);
        h.update(&[5]);
        digest_str_or_hash_into(&mut h, v1);
    } else {
        h.update(&2_u64.to_le_bytes());
        digest_text(&mut h, k1);
        h.update(&[5]);
        digest_str_or_hash_into(&mut h, v1);
        digest_text(&mut h, k0);
        h.update(&[5]);
        digest_str_or_hash_into(&mut h, v0);
    }
    h.finish()
}

#[inline]
pub(super) fn cached_long_hash(
    text: &str,
    long_hashes: &blocks::ShardedMap<(usize, usize), [u8; 16]>,
) -> [u8; 16] {
    let key = (text.as_ptr() as usize, text.len());
    if let Some(cached) = long_hashes.get(&key) {
        return cached;
    }
    let computed = blocks::digest(&[text.as_bytes()]);
    long_hashes.insert(key, computed);
    computed
}

pub(super) static EMPTY_ATTRIBUTES_DIGEST: LazyLock<[u8; 16]> =
    LazyLock::new(|| attributes_digest(&Map::new()));
pub(super) static NULL_VALUE_DIGEST: LazyLock<[u8; 16]> = LazyLock::new(|| {
    let mut h = RowHasher::new();
    h.update(&[0]);
    h.finish()
});
pub(super) static BOOL_FALSE_DIGEST: LazyLock<[u8; 16]> = LazyLock::new(|| {
    let mut h = RowHasher::new();
    h.update(&[1, 0]);
    h.finish()
});
pub(super) static BOOL_TRUE_DIGEST: LazyLock<[u8; 16]> = LazyLock::new(|| {
    let mut h = RowHasher::new();
    h.update(&[1, 1]);
    h.finish()
});

#[inline]
pub(super) fn row_digest_header(
    name: &str,
    class_name: &str,
    parent_index: Option<usize>,
) -> RowHasher {
    use crate::settings::equivalence::digest_text;

    let mut digest = RowHasher::new();
    digest_text(&mut digest, name);
    digest_text(&mut digest, class_name);
    digest.update(
        &parent_index
            .map_or(0, |parent| parent as u64 + 1)
            .to_le_bytes(),
    );
    digest
}

#[inline]
pub(super) fn digest_i64_value(value: i64) -> [u8; 16] {
    let mut h = RowHasher::new();
    if value >= 0 {
        h.update(&[2]);
        h.update(&(value as u64).to_le_bytes());
    } else {
        h.update(&[3]);
        h.update(&value.to_le_bytes());
    }
    h.finish()
}

#[inline]
pub(super) fn digest_u64_value(value: u64) -> [u8; 16] {
    let mut h = RowHasher::new();
    h.update(&[2]);
    h.update(&value.to_le_bytes());
    h.finish()
}

#[inline]
fn digest_f64_into(h: &mut RowHasher, value: f64) {
    if value.is_finite() {
        h.update(&[4]);
        h.update(&value.to_bits().to_le_bytes());
    } else {
        crate::settings::equivalence::digest_value(h, &nonfinite_float_json(value));
    }
}

#[inline]
pub(super) fn digest_f64_value(value: f64) -> [u8; 16] {
    let mut h = RowHasher::new();
    digest_f64_into(&mut h, value);
    h.finish()
}

#[inline]
pub(super) fn digest_string_value(text: &str) -> [u8; 16] {
    let mut h = RowHasher::new();
    h.update(&[5]);
    crate::settings::equivalence::digest_text(&mut h, text);
    h.finish()
}

#[inline]
pub(super) fn digest_two_string_object_value(k0: &str, v0: &str, k1: &str, v1: &str) -> [u8; 16] {
    use crate::settings::equivalence::digest_text;
    let mut h = RowHasher::new();
    h.update(&[7]);
    if k0 == k1 {
        h.update(&1_u64.to_le_bytes());
        digest_text(&mut h, k1);
        h.update(&[5]);
        digest_text(&mut h, v1);
    } else if k0 < k1 {
        h.update(&2_u64.to_le_bytes());
        digest_text(&mut h, k0);
        h.update(&[5]);
        digest_text(&mut h, v0);
        digest_text(&mut h, k1);
        h.update(&[5]);
        digest_text(&mut h, v1);
    } else {
        h.update(&2_u64.to_le_bytes());
        digest_text(&mut h, k1);
        h.update(&[5]);
        digest_text(&mut h, v1);
        digest_text(&mut h, k0);
        h.update(&[5]);
        digest_text(&mut h, v0);
    }
    h.finish()
}

#[inline]
pub(super) fn digest_enum_value(name: &str) -> [u8; 16] {
    digest_two_string_object_value("_type", "EnumItem", "name", name)
}

#[inline]
pub(super) fn digest_unique_id_value(id: &str) -> [u8; 16] {
    digest_two_string_object_value("_type", "UniqueId", "value", id)
}

#[inline]
pub(super) fn digest_ref_one_based(one_based: u64) -> [u8; 16] {
    use crate::settings::equivalence::digest_text;
    let mut h = RowHasher::new();
    h.update(&[7]);
    h.update(&2_u64.to_le_bytes());
    digest_text(&mut h, "_type");
    h.update(&[5]);
    digest_text(&mut h, "Ref");
    digest_text(&mut h, "instanceIndex");
    h.update(&[2]);
    h.update(&one_based.to_le_bytes());
    h.finish()
}

#[inline]
pub(super) fn digest_ref_value(index: usize) -> [u8; 16] {
    digest_ref_one_based(index as u64 + 1)
}

/// Digests a fixed numeric type directly in `serde_json::Map` (`BTreeMap`) alphabetical key order,
/// producing the exact same `[u8; 16]` as `value_digest(&fixed_value(...))`.
#[inline]
pub(super) fn digest_fixed_floats_value(kind: u8, components: &[f32]) -> [u8; 16] {
    use crate::settings::equivalence::digest_text;
    let mut h = RowHasher::new();
    h.update(&[7]);
    if kind == 16 {
        h.update(&2_u64.to_le_bytes());
        digest_text(&mut h, "_type");
        h.update(&[5]);
        digest_text(&mut h, "CFrame");
        digest_text(&mut h, "components");
        h.update(&[6]);
        h.update(&12_u64.to_le_bytes());
        for &c in components {
            digest_f64_into(&mut h, f64::from(c));
        }
        return h.finish();
    }
    let (type_name, fields): (&str, &[(&str, usize)]) = match kind {
        11 => ("Vector2", &[("x", 0), ("y", 1)]),
        12 => ("Vector3", &[("x", 0), ("y", 1), ("z", 2)]),
        13 => ("UDim", &[("offset", 1), ("scale", 0)]),
        14 => (
            "UDim2",
            &[("xOffset", 1), ("xScale", 0), ("yOffset", 3), ("yScale", 2)],
        ),
        15 => ("Color3", &[("b", 2), ("g", 1), ("r", 0)]),
        18 => (
            "Rect",
            &[("maxX", 2), ("maxY", 3), ("minX", 0), ("minY", 1)],
        ),
        _ => unreachable!("unexpected fixed numeric kind {kind}"),
    };
    h.update(&((fields.len() + 1) as u64).to_le_bytes());
    digest_text(&mut h, "_type");
    h.update(&[5]);
    digest_text(&mut h, type_name);
    for &(field, idx) in fields {
        digest_text(&mut h, field);
        digest_f64_into(&mut h, f64::from(components[idx]));
    }
    h.finish()
}

/// Digests a CFrame with 3 f32 position components and 9 f64 rotation components (from a rotation table).
#[inline]
pub(super) fn digest_cframe_f64_value(pos: [f32; 3], rot: &[f64; 9]) -> [u8; 16] {
    use crate::settings::equivalence::digest_text;
    let mut h = RowHasher::new();
    h.update(&[7]);
    h.update(&2_u64.to_le_bytes());
    digest_text(&mut h, "_type");
    h.update(&[5]);
    digest_text(&mut h, "CFrame");
    digest_text(&mut h, "components");
    h.update(&[6]);
    h.update(&12_u64.to_le_bytes());
    for c in pos {
        digest_f64_into(&mut h, f64::from(c));
    }
    for &c in rot {
        digest_f64_into(&mut h, c);
    }
    h.finish()
}

/// The instances each thread digests in one task.
const DIGESTED_AT_ONCE: usize = 512;

/// The digest of each instance of the state's document, chunk by chunk directly
/// from `ServiceState` without allocating JSON trees for native/scalar values.
/// Returns `((values_digest, unique_id_digest, engine_state_digest) per instance, shared long-string SHA-256 cache)`.
#[allow(clippy::type_complexity)]
fn document_row_digests(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    stored_len: usize,
) -> Result<(
    Vec<([u8; 16], [u8; 16], [u8; 16])>,
    Arc<blocks::ShardedMap<(usize, usize), [u8; 16]>>,
)> {
    let _span = crate::app::profile::span("captured store instance digests");
    let budget = DecodeBudget::for_file(stored_len);
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let database = rbx_reflection_database::get().ok();
    let long_hashes = Arc::new(blocks::ShardedMap::<(usize, usize), [u8; 16]>::new());
    let chunks = records
        .par_chunks(DIGESTED_AT_ONCE)
        .map(|records| digest_captured_chunk(state, records, scope, database, &long_hashes))
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(records.len());
    for chunk in chunks {
        out.extend(chunk);
    }
    Ok((out, long_hashes))
}

/// Fast zero-allocation digest for simple attributes (`Bool`, finite `Number`, `String`,
/// fixed-numeric typed objects). Returns `None` if any attribute requires `normalize_complex`.
fn try_digest_simple_attributes(
    attributes: &Map<String, Value>,
    long_hashes: &blocks::ShardedMap<(usize, usize), [u8; 16]>,
    charged: &mut usize,
) -> Result<Option<[u8; 16]>> {
    use crate::settings::equivalence::digest_text;

    let mut h = RowHasher::new();
    h.update(&(attributes.len() as u64).to_le_bytes());
    let mut cost = attributes.len().saturating_mul(VALUE_SLOT_BYTES);
    for (name, value) in attributes {
        check_settings_string_len(name.len())?;
        cost = cost.saturating_add(name.len());
        digest_text(&mut h, name);
        match value {
            Value::Bool(b) => h.update(&[1, u8::from(*b)]),
            Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    h.update(&[2]);
                    h.update(&u.to_le_bytes());
                } else if let Some(i) = n.as_i64() {
                    h.update(&[3]);
                    h.update(&i.to_le_bytes());
                } else if let Some(f) = n.as_f64().filter(|f| f.is_finite()) {
                    digest_f64_into(&mut h, f);
                } else {
                    return Ok(None);
                }
            }
            Value::String(s) => {
                check_settings_string_len(s.len())?;
                h.update(&[5]);
                if s.len() >= blocks::POOL_MIN_BYTES {
                    let hash = cached_long_hash(s, long_hashes);
                    h.update_long_hash(&hash);
                    cost = cost.saturating_add(16);
                } else {
                    digest_text(&mut h, s);
                    cost = cost.saturating_add(s.len());
                }
            }
            _ => return Ok(None),
        }
    }
    *charged = charged.saturating_add(cost);
    Ok(Some(h.finish()))
}

fn digest_captured_chunk<'a>(
    state: &'a ServiceState,
    records: &[SettingsBinaryInstance<'a>],
    scope: ConversionScope<'_, '_>,
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    long_hashes: &LongHashMap,
) -> Result<Vec<CapturedRowDigest>> {
    use crate::settings::equivalence::digest_text;

    let mut sources: Vec<(&'a str, u8, SettingsBinaryValueSource<'a>)> = Vec::new();
    let mut scratch = Vec::new();
    let mut shared_digests = AHashMap::<usize, [u8; 16]>::new();
    let mut out = Vec::with_capacity(records.len());
    let mut charged = 0_usize;
    let empty_attr = *EMPTY_ATTRIBUTES_DIGEST;

    for record in records {
        let id_len = match &record.settings_id {
            SettingsBinaryId::Text(text) => {
                check_settings_string_len(text.len())?;
                text.len()
            }
            SettingsBinaryId::NumericDebug(id) => 8 + decimal_digits(*id),
        };
        check_settings_string_len(record.name.len())?;
        check_settings_string_len(record.class_name.len())?;
        charged = charged.saturating_add(id_len + record.name.len() + record.class_name.len());

        sources.clear();
        visit_settings_binary_properties(state, record, scope.lookup, |name, kind, source| {
            if !SETTINGS_RECORD_FIELDS.contains(&name) {
                sources.push((name, kind, source));
            }
            Ok(())
        })?;
        sources.sort_unstable_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
        if sources
            .windows(2)
            .any(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1))
        {
            next_group_instance(0, 0, 1)?;
        }

        let mut row_hash = row_digest_header(record.name, record.class_name, record.parent_index);
        let mut engine_hash: Option<RowHasher> = None;
        let mut unique_id = [0u8; 16];
        let mut attr_digest = empty_attr;

        let len = sources.len();
        for idx in 0..len {
            let (name, kind, ref source) = sources[idx];
            if name == "Attributes" && kind == 8 {
                if let SettingsBinaryValueSource::Attributes(attrs) = source
                    && let Some(fast) =
                        try_digest_simple_attributes(attrs, long_hashes, &mut charged)?
                {
                    attr_digest = fast;
                    continue;
                }
                let Value::Object(attributes) =
                    normalize_complex(source, kind, true, scope, &mut scratch)?
                else {
                    bail!("Expected decoded attributes");
                };
                attr_digest = attributes_digest(&attributes);
                continue;
            }
            let overwritten = idx + 1 < len
                && sources[idx + 1].0 == name
                && !(sources[idx + 1].0 == "Attributes" && sources[idx + 1].1 == 8);
            let digest = digest_captured_source(
                source,
                kind,
                scope,
                &mut scratch,
                &mut charged,
                &mut shared_digests,
                long_hashes,
            )?;
            if overwritten {
                continue;
            }
            if name == UNIQUE_ID {
                unique_id = digest;
            } else if is_digested_with_db(database, record.class_name, name) {
                row_hash.update(&[1]);
                digest_text(&mut row_hash, name);
                row_hash.update(&digest);
            } else {
                let eh = engine_hash.get_or_insert_with(RowHasher::new);
                eh.update(&[1]);
                digest_text(eh, name);
                eh.update(&digest);
            }
        }
        row_hash.update(&[0]);
        row_hash.update(&attr_digest);
        out.push((
            row_hash.finish(),
            unique_id,
            engine_hash.map_or([0; 16], RowHasher::finish),
        ));
    }
    scope.budget.take(charged)?;
    Ok(out)
}

#[inline]
fn decimal_digits(mut n: u64) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

fn digest_captured_source(
    source: &SettingsBinaryValueSource<'_>,
    kind: u8,
    scope: ConversionScope<'_, '_>,
    scratch: &mut Vec<u8>,
    charged: &mut usize,
    shared_digests: &mut AHashMap<usize, [u8; 16]>,
    long_hashes: &blocks::ShardedMap<(usize, usize), [u8; 16]>,
) -> Result<[u8; 16]> {
    match source {
        SettingsBinaryValueSource::Native(native) => match native {
            NativeSettingsValue::Bool(false) => Ok(*BOOL_FALSE_DIGEST),
            NativeSettingsValue::Bool(true) => Ok(*BOOL_TRUE_DIGEST),
            NativeSettingsValue::Int(v) => Ok(digest_i64_value(*v)),
            NativeSettingsValue::Float32(v) => Ok(digest_f64_value(f64::from(*v))),
            NativeSettingsValue::Float64(v) => Ok(digest_f64_value(*v)),
            NativeSettingsValue::String(s) => {
                check_settings_string_len(s.len())?;
                if s.len() >= blocks::POOL_MIN_BYTES {
                    *charged = charged.saturating_add(16);
                    let hash = cached_long_hash(s, long_hashes);
                    Ok(digest_long_string_hash(&hash))
                } else {
                    *charged = charged.saturating_add(s.len());
                    Ok(digest_string_value(s))
                }
            }
            NativeSettingsValue::Enum(s) => {
                check_settings_string_len(s.len())?;
                *charged = charged.saturating_add(s.len());
                Ok(digest_enum_value(s))
            }
            NativeSettingsValue::UniqueId(s) => {
                check_settings_string_len(s.len())?;
                *charged = charged.saturating_add(
                    2 * VALUE_SLOT_BYTES
                        + "_type".len()
                        + "UniqueId".len()
                        + "value".len()
                        + s.len(),
                );
                Ok(digest_unique_id_value(s))
            }
            NativeSettingsValue::Ref(index) => {
                check_resolved_ref_index(*index, scope.instance_count)?;
                Ok(digest_ref_value(*index))
            }
            NativeSettingsValue::Vector2(c) => Ok(digest_fixed_floats_value(11, c)),
            NativeSettingsValue::Vector3(c) => Ok(digest_fixed_floats_value(12, c)),
            NativeSettingsValue::UDim(c) => Ok(digest_fixed_floats_value(13, c)),
            NativeSettingsValue::UDim2(c) => Ok(digest_fixed_floats_value(14, c)),
            NativeSettingsValue::Color3(c) => Ok(digest_fixed_floats_value(15, c)),
            NativeSettingsValue::CFrame(c) => Ok(digest_fixed_floats_value(16, c.as_ref())),
            NativeSettingsValue::Rect(c) => Ok(digest_fixed_floats_value(18, c)),
            NativeSettingsValue::Shared(arc) => {
                let ptr = Arc::as_ptr(arc) as usize;
                if let Some(&digest) = shared_digests.get(&ptr) {
                    return Ok(digest);
                }
                let digest = if let Some(obj) = arc.as_object()
                    && obj.len() == 2
                    && let Some(ty) = obj.get("_type").and_then(Value::as_str)
                    && matches!(ty, "SharedString" | "BinaryString")
                    && let Some(b64) = obj.get("base64").and_then(Value::as_str)
                {
                    let v1 = if b64.len() >= blocks::POOL_MIN_BYTES {
                        StrOrHash::Hash(cached_long_hash(b64, long_hashes))
                    } else {
                        StrOrHash::Text(b64)
                    };
                    digest_two_string_object_with_hashes("_type", StrOrHash::Text(ty), "base64", v1)
                } else {
                    value_digest(arc)
                };
                shared_digests.insert(ptr, digest);
                Ok(digest)
            }
        },
        SettingsBinaryValueSource::Property(_) | SettingsBinaryValueSource::Settled(_) => {
            let (value, is_borrowed_property): (&Value, bool) = match source {
                SettingsBinaryValueSource::Property(value) => (value, true),
                SettingsBinaryValueSource::Settled(value) => (value.as_ref(), false),
                _ => unreachable!(),
            };
            match kind {
                0 => Ok(*NULL_VALUE_DIGEST),
                1 => Ok(*BOOL_FALSE_DIGEST),
                2 => Ok(*BOOL_TRUE_DIGEST),
                3..=5 | 20 => Ok(value_digest(value)),
                6 => {
                    let text = raw_string_payload(value)
                        .context("Expected string binary settings value")?;
                    check_settings_string_len(text.len())?;
                    if text.len() >= blocks::POOL_MIN_BYTES {
                        *charged = charged.saturating_add(16);
                        let hash = cached_long_hash(text, long_hashes);
                        Ok(digest_long_string_hash(&hash))
                    } else {
                        *charged = charged.saturating_add(text.len());
                        Ok(digest_string_value(text))
                    }
                }
                19 => {
                    let text =
                        raw_string_payload(value).context("Expected enum binary settings value")?;
                    check_settings_string_len(text.len())?;
                    *charged = charged.saturating_add(text.len());
                    Ok(digest_enum_value(text))
                }
                17 => {
                    let reference =
                        ref_payload_object(value).context("Expected Ref binary settings value")?;
                    let index = resolve_ref_index(reference, scope.lookup)?
                        .context("Expected resolved Ref binary settings value")?;
                    check_resolved_ref_index(index, scope.instance_count)?;
                    Ok(digest_ref_value(index))
                }
                11..=16 | 18 => {
                    if kind == 16 {
                        let bits = cframe_component_bits(value)?;
                        return Ok(digest_fixed_floats_value(16, &bits.map(f32::from_bits)));
                    }
                    let fixed =
                        fixed_numeric_kind_from_tag(kind).context("Expected fixed numeric kind")?;
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
                    Ok(digest_fixed_floats_value(kind, &components[..fields.len()]))
                }
                7 => {
                    if let Some(items) = value.as_array()
                        && items.iter().all(Value::is_string)
                    {
                        let mut h = RowHasher::new();
                        h.update(&[6]);
                        h.update(&(items.len() as u64).to_le_bytes());
                        let mut cost = items.len().saturating_mul(VALUE_SLOT_BYTES);
                        for item in items {
                            let s = item.as_str().unwrap();
                            check_settings_string_len(s.len())?;
                            h.update(&[5]);
                            if s.len() >= blocks::POOL_MIN_BYTES {
                                let hash = cached_long_hash(s, long_hashes);
                                h.update_long_hash(&hash);
                                cost = cost.saturating_add(16);
                            } else {
                                crate::settings::equivalence::digest_text(&mut h, s);
                                cost = cost.saturating_add(s.len());
                            }
                        }
                        *charged = charged.saturating_add(cost);
                        return Ok(h.finish());
                    }
                    let val = normalize_complex(source, kind, false, scope, scratch)?;
                    Ok(value_digest(&val))
                }
                8 if is_borrowed_property => {
                    if let Some(obj) = value.as_object()
                        && obj.len() == 2
                        && let Some(ty) = obj.get("_type").and_then(Value::as_str)
                        && matches!(ty, "SharedString" | "BinaryString")
                        && let Some(b64) = obj.get("base64").and_then(Value::as_str)
                    {
                        let ptr = value as *const Value as usize;
                        let slot_cost = 2 * VALUE_SLOT_BYTES
                            + "_type".len()
                            + ty.len()
                            + "base64".len()
                            + if b64.len() >= blocks::POOL_MIN_BYTES {
                                16
                            } else {
                                b64.len()
                            };
                        if let Some(&digest) = shared_digests.get(&ptr) {
                            *charged = charged.saturating_add(slot_cost);
                            return Ok(digest);
                        }
                        check_settings_string_len(b64.len())?;
                        let v1 = if b64.len() >= blocks::POOL_MIN_BYTES {
                            StrOrHash::Hash(cached_long_hash(b64, long_hashes))
                        } else {
                            StrOrHash::Text(b64)
                        };
                        let digest = digest_two_string_object_with_hashes(
                            "_type",
                            StrOrHash::Text(ty),
                            "base64",
                            v1,
                        );
                        shared_digests.insert(ptr, digest);
                        *charged = charged.saturating_add(slot_cost);
                        return Ok(digest);
                    }
                    let val = normalize_complex(source, kind, false, scope, scratch)?;
                    Ok(value_digest(&val))
                }
                _ => {
                    let val = property_value(value, source, kind, scope, scratch)?;
                    Ok(value_digest(&val))
                }
            }
        }
        SettingsBinaryValueSource::Attributes(_) => {
            let val = normalize_complex(source, kind, true, scope, scratch)?;
            Ok(value_digest(&val))
        }
    }
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
    let database = rbx_reflection_database::get().ok();
    (
        row_digest(
            instance,
            instance
                .properties
                .iter()
                .filter(|(name, _)| {
                    whole && name.as_str() != UNIQUE_ID
                        || is_digested_with_db(database, &instance.class_name, name)
                })
                .map(|(name, value)| (name.as_str(), value_digest(value))),
            if instance.attributes.is_empty() {
                *EMPTY_ATTRIBUTES_DIGEST
            } else {
                attributes_digest(&instance.attributes)
            },
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
    is_digested_with_db(rbx_reflection_database::get().ok(), class_name, name)
}

#[inline]
pub(super) fn is_digested_with_db(
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    class_name: &str,
    name: &str,
) -> bool {
    name != UNIQUE_ID
        && !crate::settings::equivalence::reconciliation_property_is_always_engine_state(
            database, class_name, name,
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

    let mut digest = row_digest_header(&instance.name, &instance.class_name, instance.parent_index);
    for (name, value) in properties {
        digest.update(&[1]);
        digest_text(&mut digest, name);
        digest.update(&value);
    }
    digest.update(&[0]);
    digest.update(&attributes);
    digest.finish()
}

/// A payload held once is digested once for the process.
pub(super) fn value_digest(value: &Value) -> [u8; 16] {
    let of = |value: &Value| {
        let mut digest = RowHasher::new();
        crate::settings::equivalence::digest_value(&mut digest, value);
        digest.finish()
    };
    crate::settings::held::digest_once(value, of).unwrap_or_else(|| of(value))
}

pub(super) fn attributes_digest(attributes: &Map<String, Value>) -> [u8; 16] {
    if attributes.is_empty() {
        let mut digest = RowHasher::new();
        crate::settings::equivalence::digest_map(&mut digest, attributes);
        return digest.finish();
    }
    let mut digest = RowHasher::new();
    crate::settings::equivalence::digest_map(&mut digest, attributes);
    digest.finish()
}

pub(super) fn half_digest(digest: Sha256) -> [u8; 16] {
    let whole = <[u8; 32]>::from(digest.finalize());
    <[u8; 16]>::try_from(&whole[..16]).unwrap()
}

fn split_fast_fallback(
    fast_results: Option<&[FastCompareEntry]>,
    changed: &[usize],
    rows: &[usize],
    stored_at: &[usize],
) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let Some(fast) = fast_results else {
        return (Vec::new(), rows.to_vec(), stored_at.to_vec());
    };
    let mut fpos = Vec::new();
    let mut frows = Vec::new();
    let mut fstored = Vec::new();
    for (pos, res) in fast.iter().enumerate() {
        if res.is_none() {
            fpos.push(pos);
            frows.push(changed[pos]);
            fstored.push(stored_at[pos]);
        }
    }
    (fpos, frows, fstored)
}

fn plan_compare_batches(
    fallback_rows: &[usize],
    fallback_stored_at: &[usize],
    blocks_ref: Option<&blocks::RowBatches<'_>>,
) -> Vec<std::ops::Range<usize>> {
    if fallback_rows.is_empty() {
        return Vec::new();
    }
    let batch_size = COMPARED_AT_ONCE;
    if let Some(blocks_ref) = blocks_ref
        && fallback_stored_at.windows(2).all(|w| w[0] < w[1])
    {
        let mut ranges = Vec::new();
        let mut start = 0;
        while start < fallback_stored_at.len() {
            let mut end = start;
            let mut last_block = blocks_ref.block_of(fallback_stored_at[start]);
            let mut last_boundary = start;
            while end < fallback_stored_at.len() && end - start < batch_size {
                let blk = blocks_ref.block_of(fallback_stored_at[end]);
                if blk != last_block {
                    last_boundary = end;
                    last_block = blk;
                }
                end += 1;
            }
            let cut = if end < fallback_stored_at.len()
                && blocks_ref.block_of(fallback_stored_at[end]) == last_block
                && last_boundary > start
            {
                last_boundary
            } else {
                end
            };
            ranges.push(start..cut);
            start = cut;
        }
        return ranges;
    }
    (0..fallback_rows.len())
        .step_by(batch_size)
        .map(|start| start..(start + batch_size).min(fallback_rows.len()))
        .collect()
}

fn merge_fast_compare_results(
    fast_results: Option<Vec<FastCompareEntry>>,
    fallback_pos: Vec<usize>,
    fallback_pairs: Vec<ComparedPair>,
    fallback_compared: Vec<ComparedRowResult>,
) -> (Vec<ComparedPair>, Vec<ComparedRowResult>) {
    let Some(fast) = fast_results else {
        return (fallback_pairs, fallback_compared);
    };
    let mut pairs: Vec<Option<ComparedPair>> = Vec::with_capacity(fast.len());
    let mut compared: Vec<ComparedRowResult> = Vec::with_capacity(fast.len());
    for item in fast {
        match item {
            Some((pair, flags)) => {
                pairs.push(Some(pair));
                compared.push(Ok(Some(flags)));
            }
            None => {
                pairs.push(None);
                compared.push(Ok(None));
            }
        }
    }
    for ((pos, pair), cmp) in fallback_pos
        .into_iter()
        .zip(fallback_pairs)
        .zip(fallback_compared)
    {
        pairs[pos] = Some(pair);
        compared[pos] = cmp;
    }
    (
        pairs
            .into_iter()
            .map(|p| p.expect("every changed row resolved"))
            .collect(),
        compared,
    )
}

/// The answer for a store whose instances are the stored ones but for
/// `changed`, which hold other values, and for the viewport camera.
/// `told_in_place` says whether the alignment's matching takes each instance
/// for the stored one at its place, and is asked where the places do not.
fn plan_unchanged_blocks(
    structure_unchanged: bool,
    record_count: usize,
    exact_row_match: Option<&[bool]>,
    from_remembered: bool,
    changed: &[usize],
    viewport_camera: Option<usize>,
) -> Option<Vec<bool>> {
    if !structure_unchanged || record_count == 0 {
        return None;
    }
    let num_blocks = record_count.div_ceil(blocks::BLOCK_ROWS);
    let mut unchanged = if let Some(exact) = exact_row_match {
        (0..num_blocks)
            .map(|b| {
                let start = b * blocks::BLOCK_ROWS;
                let end = (start + blocks::BLOCK_ROWS).min(record_count);
                exact[start..end].iter().all(|&m| m)
            })
            .collect::<Vec<_>>()
    } else if from_remembered {
        let mut u = vec![true; num_blocks];
        for &row in changed {
            u[row / blocks::BLOCK_ROWS] = false;
        }
        u
    } else {
        vec![false; num_blocks]
    };
    if let Some(cam) = viewport_camera {
        unchanged[cam / blocks::BLOCK_ROWS] = false;
    }
    Some(unchanged)
}

fn build_camera_compared_pair(
    stored: &SettingsBytecodeInstance,
    captured: &SettingsBytecodeInstance,
    root_service: Option<&str>,
) -> ComparedPair {
    use crate::settings::equivalence::reconciliation_value_contains_reference as holds_reference;
    let holds_a_reference = |instance: &SettingsBytecodeInstance| {
        instance
            .properties
            .values()
            .chain(instance.attributes.values())
            .any(holds_reference)
    };
    ComparedPair {
        placed: stored.name == captured.name
            && stored.class_name == captured.class_name
            && stored.parent_index == captured.parent_index,
        same_class: stored.class_name == captured.class_name,
        written_as_today: !crate::settings::equivalence::forced_property_differs(
            &stored.properties,
        ) && values_encode_alike(
            &stored.properties,
            &stored.attributes,
            root_service,
        ),
        same_unique_id: stored.properties.get(UNIQUE_ID) == captured.properties.get(UNIQUE_ID),
        holds_reference: holds_a_reference(stored) || holds_a_reference(captured),
    }
}

fn precheck_namesakes_in_place(
    paired: bool,
    moved: Option<&crate::settings::bytecode::property_edit::Moved>,
    read: Option<&ReadStore<'_>>,
    places: &Places,
    records: &[SettingsBinaryInstance<'_>],
    stored_at_index: impl Fn(usize) -> Option<usize>,
    check_told_in_place: &mut impl FnMut() -> bool,
) -> Option<Option<Vec<bool>>> {
    if paired || moved.is_some() {
        return Some(None);
    }
    let Some(read) = read else {
        return Some(None);
    };
    if places.ids == read.ids {
        if (read.renamed || !places.kept_under_their_ids()) && !check_told_in_place() {
            return None;
        }
        return Some(None);
    }
    if !read.renamed && places.kept_under_other_ids(&read.ids) {
        return Some(None);
    }
    let namesakes_vec = places
        .stored_identities_stand(&read.ids, read, stored_at_index)
        .then(|| namesakes(records))
        .flatten();
    if namesakes_vec.is_none() && !check_told_in_place() {
        return None;
    }
    Some(namesakes_vec)
}

fn changed_rows<'a>(
    state: &'a ServiceState,
    now: &PulledRows,
    (records, places, told_in_place): (Vec<SettingsBinaryInstance<'a>>, &Places, &dyn Fn() -> bool),
    changed: &[usize],
    stored: StoredRows<'_>,
    existing_lookup: Option<&SettingsBinaryInstanceLookup<'_>>,
    store_gate: Option<&Mutex<()>>,
) -> Result<Option<SettingsAlignment>> {
    use crate::settings::bytecode::property_edit::RowBatches;
    use crate::settings::equivalence::{
        instance_lacks_enum_types, pulled_instance_values_unchanged,
        pulled_instance_values_unchanged_with_cache,
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
    let moved = read.as_ref().and_then(|read| read.moved.clone());
    let moved = moved.as_ref();
    let told_ids = read.as_mut().and_then(|read| read.told_ids.take());
    let paired = told_ids.is_some();
    // The camera is the last row where the state has one.
    let mut rows = changed.to_vec();
    if camera.is_none() {
        rows.extend(now.viewport_camera);
    }
    let Some(stored_at) = rows
        .iter()
        .map(|row| moved.map_or(Some(*row), |moved| moved.in_store(*row)))
        .collect::<Option<Vec<_>>>()
    else {
        return whole("a changed instance is none the store holds");
    };
    let in_state = |mut stored_rows: Vec<SettingsBytecodeInstance>| {
        if let Some(moved) = moved {
            (stored_rows.par_iter_mut()).for_each(|stored| moved.in_state_whole(stored));
        }
        stored_rows
    };
    let stored_at_index = |index: usize| moved.map_or(Some(index), |moved| moved.in_store(index));
    let mut told_in_place_cached: Option<bool> = None;
    let mut check_told_in_place = || {
        if moved.is_some() {
            return false;
        }
        *told_in_place_cached.get_or_insert_with(told_in_place)
    };
    let precheck_other_ids_namesakes = match precheck_namesakes_in_place(
        paired,
        moved,
        read.as_ref(),
        places,
        &records,
        stored_at_index,
        &mut check_told_in_place,
    ) {
        Some(namesakes) => namesakes,
        None => return whole("nothing tells every instance for the stored one at its place"),
    };
    let preopened = read.as_mut().and_then(|read| {
        let store = read.opened_store.take()?;
        let structure = read.structure.take()?;
        Some(blocks::RowBatches::from_opened(store, structure))
    });
    let mut stored_batches = preopened.map(RowBatches::from_blocks);
    let mut first_stored_from_legacy: Option<Vec<SettingsBytecodeInstance>> = None;
    let can_fast_compare = !changed.is_empty();
    let ids = if let Some(read) = read.as_mut() {
        std::mem::take(&mut read.ids)
    } else {
        if stored_batches.is_none() {
            stored_batches = Some(RowBatches::open_holding(file)?);
        }
        match stored_batches.as_ref().unwrap() {
            RowBatches::Blocks(blocks_batches) => blocks_batches.ids(),
            RowBatches::Columns(_) => {
                let first_end = rows.len().min(COMPARED_AT_ONCE);
                let (stored_rows, ids) = stored_batches
                    .as_ref()
                    .unwrap()
                    .first(&stored_at[..first_end])?;
                first_stored_from_legacy = Some(in_state(stored_rows));
                ids
            }
        }
    };
    // A pull leaves each instance the id its store holds it under.
    let written_ids = told_ids.or_else(|| moved.map(|moved| moved.written_ids(&places.ids, &ids)));
    let written_ids = written_ids.as_ref().unwrap_or(&ids);
    let own_ids = places.ids == *written_ids;
    let records = if own_ids {
        records
    } else {
        under_stored_ids(records, written_ids)
    };
    let rebuilt_lookup = if own_ids && existing_lookup.is_some() {
        None
    } else {
        Some(build_settings_binary_instance_lookup(state, &records))
    };
    let lookup = rebuilt_lookup
        .as_ref()
        .or(existing_lookup)
        .expect("lookup available");
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let root_service = lookup.root_service.as_deref();
    let holds_a_reference = |instance: &SettingsBytecodeInstance| {
        instance
            .properties
            .values()
            .chain(instance.attributes.values())
            .any(holds_reference)
    };
    let long_hashes = read.as_mut().and_then(|r| r.long_hashes.take());
    let fast_results: Option<Vec<FastCompareEntry>> = if can_fast_compare
        && !changed.is_empty()
        && let Some(blocks_batches) = stored_batches.as_ref().and_then(RowBatches::as_blocks)
    {
        Some(blocks_batches.fast_compare_changed(
            state,
            &records,
            lookup,
            (changed, &stored_at[..changed.len()], moved),
            root_service,
            long_hashes.as_deref(),
        )?)
    } else {
        None
    };

    if let (Some(fast), Some(namesakes_vec)) = (
        fast_results.as_deref(),
        precheck_other_ids_namesakes.as_ref(),
    ) && fast.iter().zip(changed).any(|(entry, &row)| {
        entry.as_ref().is_some_and(|(pair, (as_held, _))| {
            !(*as_held && pair.placed) && (namesakes_vec[row] || pair.holds_reference)
        })
    }) && !check_told_in_place()
    {
        return whole("nothing tells every instance for the stored one at its place");
    }

    let (fallback_pos, fallback_rows, fallback_stored_at) =
        split_fast_fallback(fast_results.as_deref(), changed, &rows, &stored_at);
    let batch_ranges = plan_compare_batches(
        &fallback_rows,
        &fallback_stored_at,
        stored_batches.as_ref().and_then(RowBatches::as_blocks),
    );

    let mut fallback_pairs = Vec::with_capacity(fallback_rows.len());
    let mut fallback_compared = Vec::with_capacity(fallback_rows.len());
    let mut last = None;
    let compare_limit = if fast_results.is_some() {
        fallback_rows.len()
    } else {
        changed.len()
    };
    let mut compare_pair_batch = |mut stored_rows: Vec<SettingsBytecodeInstance>,
                                  mut captured_rows: Vec<SettingsBytecodeInstance>,
                                  out_pairs: &mut Vec<ComparedPair>,
                                  out_compared: &mut Vec<ComparedRowResult>,
                                  keep_last: bool| {
        let _span = crate::app::profile::span("changed rows compared");
        let unchanged = compare_limit.saturating_sub(out_compared.len());
        let (said, values): (Vec<_>, Vec<_>) = stored_rows
            .par_iter()
            .zip(captured_rows.par_iter())
            .enumerate()
            .map_init(
                CanonicalizeCache::new,
                |cache, (index, (stored, captured))| {
                    let pair = ComparedPair {
                        placed: stored.name == captured.name
                            && stored.class_name == captured.class_name
                            && stored.parent_index == captured.parent_index,
                        same_class: stored.class_name == captured.class_name,
                        written_as_today: !crate::settings::equivalence::forced_property_differs(
                            &stored.properties,
                        ) && values_encode_alike(
                            &stored.properties,
                            &stored.attributes,
                            root_service,
                        ),
                        same_unique_id: stored.properties.get(UNIQUE_ID)
                            == captured.properties.get(UNIQUE_ID),
                        holds_reference: holds_a_reference(stored) || holds_a_reference(captured),
                    };
                    let values = (index < unchanged).then(|| {
                        pulled_instance_values_unchanged_with_cache(
                            stored,
                            captured,
                            root_service,
                            cache,
                        )
                    });
                    (pair, values)
                },
            )
            .unzip();
        out_pairs.extend(said);
        out_compared.extend(values.into_iter().flatten());
        if keep_last {
            last = stored_rows.pop().zip(captured_rows.pop());
        }
        if stored_rows.len() >= 256 {
            rayon::join(
                || stored_rows.into_par_iter().for_each(drop),
                || captured_rows.into_par_iter().for_each(drop),
            );
        }
        anyhow::Ok(())
    };

    let keep_last = fast_results.is_none();
    if !batch_ranges.is_empty() {
        if stored_batches.is_none() {
            stored_batches = Some(RowBatches::open_holding(file)?);
        }
        let stored_batches_ref = stored_batches.as_ref().context("No store was opened")?;
        let mut batches = batch_ranges.iter().map(|range| {
            (
                &fallback_rows[range.clone()],
                &fallback_stored_at[range.clone()],
            )
        });
        if let Some(first_stored) = first_stored_from_legacy {
            let (first_rows, _) = batches.next().unwrap();
            let converted = crate::app::profile::span("changed rows converted");
            let first_captured = convert_rows_parallel(state, &records, first_rows, scope)?;
            drop(converted);
            compare_pair_batch(
                first_stored,
                first_captured,
                &mut fallback_pairs,
                &mut fallback_compared,
                keep_last,
            )?;
        }
        for (batch_rows, batch_stored_at) in batches {
            let (stored_rows, captured_rows) = rayon::join(
                || stored_batches_ref.read(batch_stored_at).map(&in_state),
                || {
                    let _converted = crate::app::profile::span("changed rows converted");
                    convert_rows_parallel(state, &records, batch_rows, scope)
                },
            );
            compare_pair_batch(
                stored_rows?,
                captured_rows?,
                &mut fallback_pairs,
                &mut fallback_compared,
                keep_last,
            )?;
        }
    }

    let used_fast = fast_results.is_some();
    let (mut pairs, compared) = merge_fast_compare_results(
        fast_results,
        fallback_pos,
        fallback_pairs,
        fallback_compared,
    );

    let camera_pair = if used_fast {
        if let Some(cam_row) = now.viewport_camera {
            let stored_cam = match camera {
                Some(c) => c,
                None => {
                    let stored_cam_row = stored_at[changed.len()];
                    let stored_batches_ref =
                        stored_batches.as_ref().context("No store was opened")?;
                    let mut read_cam = in_state(stored_batches_ref.read(&[stored_cam_row])?);
                    read_cam.pop().context("Missing viewport camera row")?
                }
            };
            let converted = crate::app::profile::span("changed rows converted");
            let mut captured_cam = convert_rows_parallel(state, &records, &[cam_row], scope)?;
            drop(converted);
            let captured = captured_cam.pop().expect("single camera row converted");
            pairs.push(build_camera_compared_pair(
                &stored_cam,
                &captured,
                root_service,
            ));
            Some((stored_cam, captured))
        } else {
            None
        }
    } else {
        if let (Some(camera), Some(row)) = (camera, now.viewport_camera) {
            let converted = crate::app::profile::span("changed rows converted");
            let mut captured_cam = convert_rows_parallel(state, &records, &[row], scope)?;
            drop(converted);
            let captured = captured_cam.pop().expect("single camera row converted");
            pairs.push(build_camera_compared_pair(&camera, &captured, root_service));
            last = Some((camera, captured));
        }
        last.filter(|_| now.viewport_camera.is_some())
    };
    if pairs
        .iter()
        .enumerate()
        .any(|(index, pair)| !(pair.placed || paired && index < changed.len() && pair.same_class))
    {
        return whole("an instance stands under another name, class or parent than the stored one");
    }
    if pairs.iter().any(|pair| !pair.written_as_today) {
        return whole("a stored instance is written another way than documents are today");
    }
    let compared = compared.into_iter().collect::<Result<Vec<_>>>()?;
    let mut same_values = Vec::with_capacity(changed.len());
    let mut same_under_todays_names = true;
    for (compared, pair) in compared.into_iter().zip(&pairs) {
        match compared {
            Some((as_held, as_named)) => {
                same_values.push(as_held && pair.placed);
                same_under_todays_names &= as_named && pair.placed;
            }
            None => return whole("an enum item of a changed instance gains its type"),
        }
    }
    // A stored camera whose enum items can gain their types makes the answer
    // follow the captured camera, which no digest covers.
    if camera_pair
        .as_ref()
        .is_some_and(|(stored, _)| instance_lacks_enum_types(stored))
    {
        return whole("the stored viewport camera holds an enum item without its type");
    }
    let changed_pairs = || pairs.iter().take(changed.len());
    let renamed = read.as_ref().is_some_and(|read| read.renamed)
        || changed_pairs().any(|pair| !pair.same_unique_id);
    let stored_at = stored_at_index;
    let camera_holds_a_reference = camera_pair
        .as_ref()
        .is_some_and(|(stored, _)| holds_a_reference(stored));
    let kept = if paired {
        // Each instance is the stored one the alignment itself takes it
        // for. The stored camera goes back as the store holds it, which a
        // reference by position does not survive.
        !camera_holds_a_reference
    } else if moved.is_some() {
        // A state with one run of instances more or less than the store: the
        // alignment takes every instance beside the run for the stored one
        // where each holds the stored values as a pull counts them, the
        // viewport camera too. Where one does not, it matches each instance
        // by what tells it alone, and gives a new one the id it came with.
        // Nothing may say an instance is another stored one, and the stored
        // camera goes back as the store holds it, which a reference by
        // position does not survive.
        let camera_alike = match &camera_pair {
            Some((stored, captured)) => {
                match pulled_instance_values_unchanged(stored, captured, root_service)? {
                    Some((as_held, _)) => as_held,
                    None => return whole("an enum item of the viewport camera gains its type"),
                }
            }
            None => true,
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
                && precheck_other_ids_namesakes.as_ref().map_or_else(
                    || {
                        namesakes(&records).is_some_and(|namesakes| {
                            changed_pairs().zip(changed).zip(&same_values).all(
                                |((pair, row), same)| {
                                    *same || !namesakes[*row] && !pair.holds_reference
                                },
                            )
                        })
                    },
                    |namesakes| {
                        changed_pairs()
                            .zip(changed)
                            .zip(&same_values)
                            .all(|((pair, row), same)| {
                                *same || !namesakes[*row] && !pair.holds_reference
                            })
                    },
                )
        });
        stand_as_stored || !renamed && places.kept_under_other_ids(&ids)
    };
    if !(kept || check_told_in_place()) {
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
        && let Some((stored, captured)) = &camera_pair
        && (!crate::settings::held::same_properties(&stored.properties, &captured.properties)
            || stored.attributes != captured.attributes)
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
    // Paired each by itself, the two hold the same instances where each of
    // one is one of the other: those the alignment compares under the names
    // their properties go by today.
    let all_paired = paired
        && ids.len() == records.len()
        && (0..records.len()).all(|index| stored_at(index).is_some());
    if (moved.is_none() || all_paired) && !renamed {
        let same_as_named = same_under_todays_names && !camera_holds_a_reference;
        if same_as_named || !paired && own_ids && same_as_held {
            return Ok(Some(SettingsAlignment::Equivalent));
        }
        if !paired && same_as_held {
            return whole("instances that hold the stored values compare otherwise by name");
        }
    }
    // A pull keeps the stored viewport camera whatever the captured one says.
    let kept_camera_index = now.viewport_camera.filter(|_| {
        camera_pair.as_ref().is_some_and(|(stored, captured)| {
            !crate::settings::held::same_properties(&stored.properties, &captured.properties)
                || stored.attributes != captured.attributes
        })
    });
    let kept_camera = kept_camera_index.zip(camera_pair.map(|(stored, _)| stored));
    let exact_row_match = read.as_mut().and_then(|r| r.exact_row_match.take());
    let block_pool_entries = read
        .as_mut()
        .map(|r| std::mem::take(&mut r.block_pool_entries))
        .unwrap_or_default();
    let from_remembered = read.is_none();
    let structure_stands = if moved.is_none() && !paired {
        if !from_remembered {
            true
        } else {
            stored_batches
                .as_ref()
                .and_then(|b| b.structure())
                .is_some_and(|s| stand_alike(s, &records))
        }
    } else {
        false
    };
    let prev_store = stored_batches.and_then(|b| b.into_block_store());
    drop(read);

    let structure_unchanged = structure_stands
        && prev_store
            .as_ref()
            .is_some_and(|s| s.of_this_writer() && s.cut_whole());
    let unchanged_blocks = plan_unchanged_blocks(
        structure_unchanged,
        records.len(),
        exact_row_match.as_deref(),
        from_remembered,
        changed,
        now.viewport_camera,
    );

    let kept_camera = kept_camera.as_ref().map(|(camera, stored)| ReplacedRow {
        source_index: *camera,
        instance: stored,
    });
    let _store_guard = store_gate.map(LockRecover::lock_recover);
    let hint = blocks::EncodeStateHint {
        previous: prev_store.as_ref(),
        structure_unchanged,
        unchanged_blocks: unchanged_blocks.as_deref(),
        block_pool_entries: (!block_pool_entries.is_empty()).then_some(&block_pool_entries),
        long_hashes: long_hashes.as_deref(),
    };
    Ok(Some(SettingsAlignment::Changed(
        blocks::encode_state_with_hint(state, (&records, lookup), kept_camera, Some(file), hint)?,
    )))
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
        let _span = crate::app::profile::span("captured store places");
        let budget = DecodeBudget::for_file(stored_len);
        let (ids, charged): (Vec<String>, usize) = if records.len()
            >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
            && rayon::current_num_threads() > 1
        {
            let chunks = records
                .par_chunks(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE)
                .map(|chunk| -> Result<(Vec<String>, usize)> {
                    let mut out = Vec::with_capacity(chunk.len());
                    let mut bytes = 0_usize;
                    for record in chunk {
                        let id = match &record.settings_id {
                            SettingsBinaryId::Text(text) => {
                                check_settings_string_len(text.len())?;
                                text.to_string()
                            }
                            SettingsBinaryId::NumericDebug(id) => format_debug_id(*id),
                        };
                        check_settings_string_len(record.name.len())?;
                        check_settings_string_len(record.class_name.len())?;
                        bytes = bytes
                            .saturating_add(id.len() + record.name.len() + record.class_name.len());
                        out.push(id);
                    }
                    Ok((out, bytes))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut ids = Vec::with_capacity(records.len());
            let mut total = 0_usize;
            for (chunk_ids, bytes) in chunks {
                ids.extend(chunk_ids);
                total = total.saturating_add(bytes);
            }
            (ids, total)
        } else {
            let mut ids = Vec::with_capacity(records.len());
            let mut total = 0_usize;
            for record in records {
                let id = match &record.settings_id {
                    SettingsBinaryId::Text(text) => {
                        check_settings_string_len(text.len())?;
                        text.to_string()
                    }
                    SettingsBinaryId::NumericDebug(id) => format_debug_id(*id),
                };
                check_settings_string_len(record.name.len())?;
                check_settings_string_len(record.class_name.len())?;
                total =
                    total.saturating_add(id.len() + record.name.len() + record.class_name.len());
                ids.push(id);
            }
            (ids, total)
        };
        budget.take(charged)?;

        let check_unique_ids = || -> bool {
            let mut prev_debug: Option<u64> = None;
            let mut all_debug = true;
            let mut strictly_inc = true;
            for record in records {
                match record.settings_id {
                    SettingsBinaryId::NumericDebug(id) => {
                        if prev_debug.is_some_and(|p| id <= p) {
                            strictly_inc = false;
                        }
                        prev_debug = Some(id);
                    }
                    SettingsBinaryId::Text(_) => {
                        all_debug = false;
                        strictly_inc = false;
                        break;
                    }
                }
            }
            if strictly_inc {
                return true;
            }
            if all_debug {
                let mut seen = AHashSet::with_capacity(records.len());
                return records.iter().all(|r| match r.settings_id {
                    SettingsBinaryId::NumericDebug(id) => seen.insert(id),
                    SettingsBinaryId::Text(_) => unreachable!(),
                });
            }
            let mut seen = AHashSet::with_capacity(ids.len());
            ids.iter().all(|id| seen.insert(id.as_str()))
        };
        let build_told_alone = || -> Vec<bool> {
            let mut told_alone = vec![false; records.len()];
            let n = state.instances.len();
            if n >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES && rayon::current_num_threads() > 1 {
                const PARTS: usize = 16;
                let chunk_count = n.div_ceil(SETTINGS_BINARY_PARALLEL_CHUNK_SIZE);
                let chunk_buckets: Vec<[Vec<(&str, usize)>; PARTS]> = (0..chunk_count)
                    .into_par_iter()
                    .map(|chunk_idx| {
                        let start = chunk_idx * SETTINGS_BINARY_PARALLEL_CHUNK_SIZE;
                        let end = (start + SETTINGS_BINARY_PARALLEL_CHUNK_SIZE).min(n);
                        let mut buckets: [Vec<(&str, usize)>; PARTS] =
                            std::array::from_fn(|_| Vec::new());
                        for index in start..end {
                            if let Some(id) = captured_persistent_identity(state, index) {
                                let mut h = ROW_HASH_A.build_hasher();
                                h.write(id.as_bytes());
                                let b = (h.finish() as usize) & (PARTS - 1);
                                buckets[b].push((id, index));
                            }
                        }
                        buckets
                    })
                    .collect();
                let unique_indices: Vec<Vec<usize>> = (0..PARTS)
                    .into_par_iter()
                    .map(|part| {
                        let total: usize = chunk_buckets.iter().map(|c| c[part].len()).sum();
                        let mut seen: AHashMap<&str, Option<usize>> =
                            AHashMap::with_capacity(total);
                        for chunk in &chunk_buckets {
                            for &(id, index) in &chunk[part] {
                                seen.entry(id)
                                    .and_modify(|prev| *prev = None)
                                    .or_insert(Some(index));
                            }
                        }
                        seen.into_values().flatten().collect()
                    })
                    .collect();
                for indices in unique_indices {
                    for index in indices {
                        if index < told_alone.len() {
                            told_alone[index] = true;
                        }
                    }
                }
            } else {
                let mut unique: AHashMap<&str, Option<usize>> =
                    AHashMap::with_capacity(records.len());
                for index in 0..n {
                    if let Some(id) = captured_persistent_identity(state, index) {
                        match unique.entry(id) {
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                entry.insert(Some(index));
                                if index < told_alone.len() {
                                    told_alone[index] = true;
                                }
                            }
                            std::collections::hash_map::Entry::Occupied(mut entry) => {
                                if let Some(prev) = entry.get_mut().take()
                                    && prev < told_alone.len()
                                {
                                    told_alone[prev] = false;
                                }
                            }
                        }
                    }
                }
            }
            if let Some(cam) = now.viewport_camera
                && cam < told_alone.len()
            {
                told_alone[cam] = true;
            }
            told_alone
        };

        let (all_unique, told_alone) = if records.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES
            && rayon::current_num_threads() > 1
        {
            rayon::join(check_unique_ids, build_told_alone)
        } else {
            if !check_unique_ids() {
                return Ok(None);
            }
            (true, build_told_alone())
        };
        if !all_unique {
            return Ok(None);
        }
        let mut roots = records
            .iter()
            .enumerate()
            .filter(|(_, record)| record.parent_index.is_none())
            .map(|(index, _)| index);
        let sole_root = roots.next().filter(|_| roots.next().is_none());
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
fn skeleton_parallel(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<SettingsBytecode> {
    let budget = DecodeBudget::new(MAX_SETTINGS_BYTECODE_BYTES);
    let scope = ConversionScope {
        lookup,
        instance_count: records.len(),
        budget: &budget,
        string_ids: &SettingsStringIdMap::default(),
        wire_bytes: None,
    };
    let unlimited = DecodeBudget::new(usize::MAX);
    let instances = records
        .par_iter()
        .map(|record| {
            if record.parent_index.is_none() || record.class_name == "Camera" {
                convert_chunk(state, std::slice::from_ref(record), scope)?
                    .into_iter()
                    .next()
                    .context("Missing converted skeleton instance")
            } else {
                bare_instance(record, &unlimited)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    validate_settings_hierarchy(&instances)?;
    Ok(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    })
}

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
    let shared = SharedValueDigests::new();
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
type SharedValueDigests = blocks::ShardedMap<usize, [u8; 16]>;

/// A task reuses its recent digests without a shared lock. Misses still
/// consult the invocation's shared cache before hashing a potentially large
/// payload; neither cache survives the immutable state being digested.
struct SharedValueCache<'a> {
    shared: &'a SharedValueDigests,
    local: AHashMap<usize, [u8; 16]>,
}

impl SharedValueCache<'_> {
    fn digest_ptr(&mut self, value: &Value) -> [u8; 16] {
        let held_at = value as *const Value as usize;
        if let Some(known) = self.local.get(&held_at) {
            return *known;
        }
        if let Some(d) = self.shared.get(&held_at) {
            self.local.insert(held_at, d);
            return d;
        }
        let mut of_value = RowHasher::new();
        crate::settings::equivalence::digest_value(&mut of_value, value);
        let d = of_value.finish();
        self.shared.insert(held_at, d);
        self.local.insert(held_at, d);
        d
    }

    fn digest(&mut self, value: &Arc<Value>) -> [u8; 16] {
        self.digest_ptr(value.as_ref())
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
    let mut digest = RowHasher::new();
    digest_text(&mut digest, path);
    digest.update(&(path_segments.len() as u64).to_le_bytes());
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
                digest.update(&[1]);
                digest_text(&mut digest, text);
            }
            None => digest.update(&[0]),
        }
    }
    for index in [instance_index, parent_index] {
        digest.update(&index.map_or(0, |index| index as u64 + 1).to_le_bytes());
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
    let placed = digest.finish();
    let mut digest = RowHasher::new();
    if viewport_camera {
        return PulledRow {
            placed,
            values: digest.finish(),
        };
    }
    digest.update(&[1]);
    // A store holds none of the record's own properties, so their values are
    // no part of its document: a script's text changes without its store.
    // Their presence is, since a property the instance holds hides the typed
    // one of its name.
    digest.update(&(properties.len() as u64).to_le_bytes());
    for (name, value) in properties {
        digest_text(&mut digest, name);
        if is_record_property(name) {
            digest.update(&[0xfe]);
        } else {
            crate::settings::equivalence::digest_value(&mut digest, value);
        }
    }
    digest_map(&mut digest, attributes);
    digest.update(
        &native
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
        values: digest.finish(),
    }
}

fn digest_native_value(
    digest: &mut RowHasher,
    value: &NativeSettingsValue,
    shared: &mut SharedValueCache<'_>,
) {
    let floats = |digest: &mut RowHasher, kind: u8, values: &[f32]| {
        digest.update(&[kind]);
        for value in values {
            digest.update(&value.to_bits().to_le_bytes());
        }
    };
    match value {
        NativeSettingsValue::Bool(value) => digest.update(&[1, u8::from(*value)]),
        NativeSettingsValue::Int(value) => {
            digest.update(&[2]);
            digest.update(&value.to_le_bytes());
        }
        NativeSettingsValue::Float32(value) => floats(digest, 3, &[*value]),
        NativeSettingsValue::Float64(value) => {
            digest.update(&[4]);
            digest.update(&value.to_bits().to_le_bytes());
        }
        NativeSettingsValue::String(value) => {
            digest.update(&[5]);
            crate::settings::equivalence::digest_text(digest, value);
        }
        NativeSettingsValue::UniqueId(value) => {
            digest.update(&[16]);
            crate::settings::equivalence::digest_text(digest, value);
        }
        NativeSettingsValue::Ref(index) => {
            digest.update(&[6]);
            digest.update(&(*index as u64).to_le_bytes());
        }
        NativeSettingsValue::Vector2(values) => floats(digest, 7, values),
        NativeSettingsValue::Vector3(values) => floats(digest, 8, values),
        NativeSettingsValue::UDim(values) => floats(digest, 9, values),
        NativeSettingsValue::UDim2(values) => floats(digest, 10, values),
        NativeSettingsValue::Color3(values) => floats(digest, 11, values),
        NativeSettingsValue::CFrame(values) => floats(digest, 12, values.as_ref()),
        NativeSettingsValue::Rect(values) => floats(digest, 13, values),
        NativeSettingsValue::Enum(value) => {
            digest.update(&[14]);
            crate::settings::equivalence::digest_text(digest, value);
        }
        NativeSettingsValue::Shared(value) => {
            digest.update(&[15]);
            digest.update(&shared.digest(value));
        }
    }
}

fn convert_prepared(
    state: &ServiceState,
    records: &[SettingsBinaryInstance<'_>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    summary: &SettingsBinarySummary<'_>,
    budget: &DecodeBudget,
) -> Result<SettingsBytecode> {
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: convert_prepared_each(state, records, lookup, summary, budget, |instance| {
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
    summary: &SettingsBinarySummary<'_>,
    budget: &DecodeBudget,
    keep: impl Fn(SettingsBytecodeInstance) -> T + Sync,
) -> Result<Vec<T>> {
    let _span = crate::app::profile::span("captured store validated conversion");
    let strings = summary
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
        summary,
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
        let rows = convert_chunk_budgeted(state, records, scope)?;
        Ok(rows.into_iter().map(&keep).collect())
    };
    const CONVERT_CHUNK_ROWS: usize = 1024;
    let instances = if records.len() <= CONVERT_CHUNK_ROWS {
        kept_chunk(records)?
    } else {
        records
            .par_chunks(CONVERT_CHUNK_ROWS)
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
    summary: &SettingsBinarySummary<'_>,
    strings: &[SettingsString<'_>],
    string_ids: &SettingsStringIdMap<'_>,
    budget: &DecodeBudget,
) -> Result<usize> {
    let _span = crate::app::profile::span("captured store limit bounds");
    checked_settings_collection_len(strings.len(), "string count")?;
    for text in strings {
        check_settings_string_len(text.len())?;
    }
    let classes = summary
        .class_counts
        .keys()
        .map(|text| Cow::Borrowed(text.as_ref()))
        .collect::<Vec<_>>();
    let properties = summary
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
    checked_settings_collection_len(summary.property_group_counts.len(), "property group count")?;
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
    write_var_u64(&mut size, summary.property_group_counts.len() as u64)?;
    for ((name, kind), &count) in &summary.property_group_counts {
        checked_settings_collection_len(count, "property value count")?;
        if *kind == 16 {
            // A rotation table cannot contain more rotations than its values.
            // If that bound fails, the wire path checks the chosen table.
            checked_cframe_rotation_count(count)?;
        }
        if !SETTINGS_RECORD_FIELDS.contains(name) {
            budget.property_group(count, name, *kind)?;
        }
        // Group header: property id, kind, count, byte length. All varints
        // fit ten bytes. A CFrame table is chosen only when smaller than inline.
        size.0 = size.0.saturating_add(31 + usize::from(*kind == 16));
    }
    if summary.copied_string_bytes > budget.remaining.load(Ordering::Relaxed) {
        bail!("Settings bytecode decodes to more memory than a file of its size may take");
    }
    Ok(size.0)
}

/// Converts the rows at `row_indices` in parallel chunks, reusing scratch buffers
/// across rows instead of allocating fresh buffers per single row.
fn convert_rows_parallel<'a>(
    state: &'a ServiceState,
    records: &[SettingsBinaryInstance<'a>],
    row_indices: &[usize],
    scope: ConversionScope<'_, '_>,
) -> Result<Vec<SettingsBytecodeInstance>> {
    if row_indices.is_empty() {
        return Ok(Vec::new());
    }
    const CHUNK: usize = 128;
    if row_indices.len() <= CHUNK {
        return convert_selected_rows(state, records, row_indices, scope);
    }
    let chunks = row_indices
        .par_chunks(CHUNK)
        .map(|chunk| convert_selected_rows(state, records, chunk, scope))
        .collect::<Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(row_indices.len());
    for chunk in chunks {
        out.extend(chunk);
    }
    Ok(out)
}

fn convert_selected_rows<'a>(
    state: &'a ServiceState,
    records: &[SettingsBinaryInstance<'a>],
    row_indices: &[usize],
    scope: ConversionScope<'_, '_>,
) -> Result<Vec<SettingsBytecodeInstance>> {
    let mut sources = Vec::new();
    let mut scratch = Vec::new();
    let mut instances = Vec::with_capacity(row_indices.len());
    for &row in row_indices {
        let record = &records[row];
        let mut instance = bare_instance(record, scope.budget)?;
        visit_settings_binary_properties(state, record, scope.lookup, |name, kind, source| {
            if !SETTINGS_RECORD_FIELDS.contains(&name) {
                sources.push((name, kind, source));
            }
            Ok(())
        })?;
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
    Ok(instances)
}

fn convert_chunk_budgeted<'a>(
    state: &'a ServiceState,
    records: &[SettingsBinaryInstance<'a>],
    scope: ConversionScope<'_, '_>,
) -> Result<Vec<SettingsBytecodeInstance>> {
    let available = scope.budget.remaining.load(Ordering::Relaxed);
    let slab = (available / 32).min(16 * 1024 * 1024);
    let local_budget =
        (slab >= 256 * 1024 && scope.budget.take(slab).is_ok()).then(|| DecodeBudget::new(slab));
    let active_scope = match &local_budget {
        Some(local) => ConversionScope {
            budget: local,
            ..scope
        },
        None => scope,
    };
    let converted = convert_chunk(state, records, active_scope);
    match (converted, local_budget) {
        (Ok(rows), Some(local)) => {
            let unused = local.remaining.load(Ordering::Relaxed);
            if unused > 0 {
                scope.budget.remaining.fetch_add(unused, Ordering::Relaxed);
            }
            Ok(rows)
        }
        (Err(_), Some(local)) => {
            let unused = local.remaining.load(Ordering::Relaxed);
            if unused > 0 {
                scope.budget.remaining.fetch_add(unused, Ordering::Relaxed);
            }
            convert_chunk(state, records, scope)
        }
        (result, None) => result,
    }
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
    let mut seen_wire_shared = AHashSet::<usize>::new();
    let mut held_shared_cache = AHashMap::<usize, Option<u64>>::new();
    let mut instances = Vec::with_capacity(records.len());
    for record in records {
        let mut instance = bare_instance(record, scope.budget)?;
        visit_settings_binary_properties(state, record, scope.lookup, |name, kind, source| {
            // Count through the real value writer with maximal-width string
            // ids. Instance/ref deltas also fit ten bytes, independent of order.
            if scope.wire_bytes.is_some() {
                wire.0 = wire.0.saturating_add(10);
                let shared = match &source {
                    SettingsBinaryValueSource::Property(value) if kind == 8 => {
                        shared_string_payload_text(value)
                            .map(|text| (*value as *const Value as usize, text))
                    }
                    _ => None,
                };
                if let Some((ptr, text)) = shared {
                    wire.0 = wire.0.saturating_add(43);
                    add_count(&mut table, name);
                    if seen_wire_shared.insert(ptr) {
                        add_count(&mut table, "_type");
                        add_count(&mut table, "SharedString");
                        add_count(&mut table, "base64");
                        add_count(&mut table, text);
                    }
                } else {
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
                }
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
            } else if kind == 8
                && let SettingsBinaryValueSource::Property(value) = &source
                && let Some(text) = shared_string_payload_text(value)
            {
                check_settings_string_len(text.len())?;
                scope.budget.take(
                    2 * VALUE_SLOT_BYTES
                        + "_type".len()
                        + "SharedString".len()
                        + "base64".len()
                        + text.len(),
                )?;
                let ptr = *value as *const Value as usize;
                let held_id = *held_shared_cache
                    .entry(ptr)
                    .or_insert_with(|| crate::settings::held::held_shared_string_id(text));
                let stored = match held_id {
                    Some(id) => crate::settings::held::handle_value(id),
                    None => Value::clone(value),
                };
                instance.properties.insert(name.to_string(), stored);
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
        SettingsBinaryId::NumericDebug(id) => {
            let formatted = format_debug_id(*id);
            budget.take(formatted.len())?;
            formatted
        }
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
        SettingsBinaryValueSource::Native(NativeSettingsValue::UniqueId(value)) => {
            scope
                .budget
                .take(2 * VALUE_SLOT_BYTES + "_type".len() + "UniqueId".len() + "value".len())?;
            Ok(typed_object(
                "UniqueId",
                [("value", Value::String(checked_string(value, scope.budget)?))],
            ))
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
        NESTED_ENUM_ITEM_KIND => {
            let (enum_type, name) =
                nested_enum_item(value).context("Expected nested enum binary settings value")?;
            Ok(typed_object(
                "EnumItem",
                [
                    (
                        "enumType",
                        Value::String(checked_string(enum_type, scope.budget)?),
                    ),
                    ("name", Value::String(checked_string(name, scope.budget)?)),
                ],
            ))
        }
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
    let scope = ValueScope::from_owned(&strings, scope.instance_count, scope.budget);
    let mut reader = BytecodeReader::new(scratch);
    let value = if attributes {
        Value::Object(decode_attributes_payload(&mut reader, scope)?)
    } else {
        decode_raw_value_payload(&mut reader, kind, scope, 0)?
    };
    reader.finish()?;
    Ok(value)
}

#[cfg(test)]
mod batch_planning_tests {
    use super::*;

    #[test]
    fn plan_compare_batches_aligns_to_block_boundaries_in_tests() -> Result<()> {
        let mut instances = vec![SettingsBytecodeInstance::new(
            "ws".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )];
        for index in 1..8 {
            let mut part = SettingsBytecodeInstance::new(
                format!("p{index}"),
                "Part".into(),
                format!("P{index}"),
                Some(0),
            );
            part.properties.insert("Transparency".into(), json!(0.1));
            instances.push(part);
        }
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let lookup = document_reference_lookup(&document, false);
        let bytes = blocks::encode_with_lookup(&document, &lookup, None, 2)?;
        let batches = blocks::RowBatches::open_holding(&bytes)?;
        let rows = [0, 1, 2, 3, 4, 5];
        let planned = plan_compare_batches(&rows, &rows, Some(&batches));
        assert_eq!(planned, vec![0..2, 2..4, 4..6]);
        Ok(())
    }
}
