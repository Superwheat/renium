use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use ahash::{AHashMap, AHashSet};
use anyhow::{Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::types::VariantType as RbxVariantType;
use rbx_reflection::DataType as RbxDataType;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::EXTERNAL_SOURCE_MARKER;
use super::bytecode::{
    SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
    encode_settings_bytecode_over, is_known_default_property_value, is_reference_object,
};
#[cfg(test)]
use super::bytecode::{encode_settings_bytecode, encode_settings_bytecode_with_dense_references};
use crate::app::output::log_global;
use crate::app::timing::verbose_timing_logs;
use crate::rbx::decode::{nonfinite_float_from_json, rbx_variant_to_settings_json};
use crate::rbx::encode::{
    json_to_rbx_color3, json_to_rbx_property_variant, rbx_logical_property_name,
    rbx_model_property_descriptor, rbx_property_descriptor,
};
use crate::rbx::model::{BytecodeModelExportRefs, BytecodeModelImportRefs};
use crate::snapshot::refs::{
    remap_and_stabilize_record_references, remap_record_reference_ids, stabilize_record_references,
    store_service,
};
use crate::system::LockRecover;

pub(crate) enum SettingsAlignment {
    Equivalent,
    Changed(Vec<u8>),
}

const SETTINGS_ALIGNMENT_CACHE_CAPACITY: usize = 64;
const SETTINGS_ALIGNMENT_CACHE_MIN_BYTES: usize = 64 * 1024;
const SETTINGS_PARALLEL_DECODE_MIN_BYTES: usize = 64 * 1024;
// Studio's native CFrame roundtrip can accumulate several f32 operations in
// the rotation basis (observed drift: 1.67e-6). Keep this absolute allowance
// confined to rotation; positions and other numbers retain their usual precision.
const CFRAME_ROTATION_EPSILON: f64 = 16.0 * f32::EPSILON as f64;
/// A rotation column stored as Float32 and re-orthonormalized by Studio
/// comes back a few millionths away. Regenerated model pivots have shown
/// 8.94e-6 drift; an edit of 1e-5 in a column still counts.
const CFRAME_ROTATION_NOISE: f64 = 9e-6;
/// Studio recomputes a model's WorldPivot and a welded part's CFrame after a
/// load; the result lands within about 1e-4 studs of the saved value (a
/// wheel pivot read 60.933189 against 60.933082). A 1e-3 edit still counts.
const CFRAME_POSITION_NOISE: f64 = 2e-4;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct SettingsAlignmentCacheKey {
    reference: [u8; 32],
    observed: [u8; 32],
}

#[derive(Default)]
struct SettingsAlignmentCache {
    order: VecDeque<SettingsAlignmentCacheKey>,
    entries: HashSet<SettingsAlignmentCacheKey>,
}

static SETTINGS_ALIGNMENT_CACHE: OnceLock<Mutex<SettingsAlignmentCache>> = OnceLock::new();

fn settings_alignment_cache_key(
    reference_bytes: &[u8],
    observed_bytes: &[u8],
) -> Option<SettingsAlignmentCacheKey> {
    if reference_bytes.len().saturating_add(observed_bytes.len())
        < SETTINGS_ALIGNMENT_CACHE_MIN_BYTES
    {
        return None;
    }
    let (reference, observed) = rayon::join(
        || Sha256::digest(reference_bytes).into(),
        || Sha256::digest(observed_bytes).into(),
    );
    Some(SettingsAlignmentCacheKey {
        reference,
        observed,
    })
}

fn settings_alignment_is_cached(key: SettingsAlignmentCacheKey) -> bool {
    SETTINGS_ALIGNMENT_CACHE
        .get_or_init(|| Mutex::new(SettingsAlignmentCache::default()))
        .lock_recover()
        .entries
        .contains(&key)
}

fn cache_settings_alignment(key: SettingsAlignmentCacheKey) {
    let mut cache = SETTINGS_ALIGNMENT_CACHE
        .get_or_init(|| Mutex::new(SettingsAlignmentCache::default()))
        .lock_recover();
    if !cache.entries.insert(key) {
        return;
    }
    cache.order.push_back(key);
    if cache.order.len() > SETTINGS_ALIGNMENT_CACHE_CAPACITY
        && let Some(expired) = cache.order.pop_front()
    {
        cache.entries.remove(&expired);
    }
}

pub(crate) fn align_settings_bytes_to_reference(
    reference_bytes: &[u8],
    observed_bytes: &[u8],
) -> Result<SettingsAlignment> {
    align_settings_bytes(reference_bytes, observed_bytes, false)
}

/// As `align_settings_bytes_to_reference`, for a store pulled out of Studio:
/// its viewport camera keeps the values the reference holds for it. The
/// aligned store is written once, with the camera already in place.
pub(crate) fn align_pulled_settings_bytes_to_reference(
    reference_bytes: &[u8],
    observed_bytes: &[u8],
) -> Result<SettingsAlignment> {
    align_settings_bytes(reference_bytes, observed_bytes, true)
}

fn align_settings_bytes(
    reference_bytes: &[u8],
    observed_bytes: &[u8],
    keep_viewport_camera: bool,
) -> Result<SettingsAlignment> {
    if reference_bytes == observed_bytes {
        return Ok(SettingsAlignment::Equivalent);
    }
    let decode_started = std::time::Instant::now();
    let cache_key = settings_alignment_cache_key(reference_bytes, observed_bytes);
    if cache_key.is_some_and(settings_alignment_is_cached) {
        return Ok(SettingsAlignment::Equivalent);
    }
    let (reference, observed) = if reference_bytes.len().saturating_add(observed_bytes.len())
        >= SETTINGS_PARALLEL_DECODE_MIN_BYTES
    {
        rayon::join(
            || crate::settings::bytecode::decode_settings_bytecode_holding(reference_bytes),
            || crate::settings::bytecode::decode_settings_bytecode_holding(observed_bytes),
        )
    } else {
        (
            decode_settings_bytecode(reference_bytes),
            decode_settings_bytecode(observed_bytes),
        )
    };
    align_settings_documents(
        reference?,
        observed?,
        AlignmentInput {
            observed_bytes: ObservedBytes::Given(observed_bytes),
            keep_viewport_camera,
            cache_key,
            started: decode_started,
            stored_as_observed: None,
            previous: Some(reference_bytes),
        },
    )
}

/// A pull's alignment for a caller that holds both stores decoded already:
/// the observed one straight from the captured state, never as bytes. Answers
/// what `align_pulled_settings_bytes_to_reference` answers for the stored
/// file `stored` and the bytes `observed_bytes` gives, which are only asked
/// for where the observed store is written as it came in.
pub(crate) fn align_pulled_settings_documents_to_reference(
    reference: SettingsBytecode,
    observed: SettingsBytecode,
    observed_bytes: &dyn Fn() -> Result<Vec<u8>>,
    started: std::time::Instant,
    stored: &[u8],
) -> Result<PulledAlignment> {
    let stored_as_observed = AtomicBool::new(false);
    let alignment = align_settings_documents(
        reference,
        observed,
        AlignmentInput {
            observed_bytes: ObservedBytes::Encode(observed_bytes),
            keep_viewport_camera: true,
            cache_key: None,
            started,
            stored_as_observed: Some(&stored_as_observed),
            previous: Some(stored),
        },
    )?;
    Ok(PulledAlignment {
        alignment,
        stored_as_observed: stored_as_observed.into_inner(),
    })
}

/// A pull's alignment, and whether the store it leaves on disk holds the
/// observed document, with the viewport camera a pull keeps: exactly so where
/// the store was left as it was, and as the encoder of documents writes that
/// document where the store was written.
pub(crate) struct PulledAlignment {
    pub(crate) alignment: SettingsAlignment,
    pub(crate) stored_as_observed: bool,
}

/// Whether a pull holds the values of the observed instance for the stored
/// one's, for two instances that stand at one position under one id, name,
/// class and parent: first as the two hold them, which is how the alignment
/// compares instances by their places, then under the names their
/// properties go by today, which is how it compares what it aligned. Their
/// UniqueIds are the caller's to compare. `None` where an enum item gains
/// its type, which takes the whole documents to say.
pub(crate) fn pulled_instance_values_unchanged(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
    service: Option<&str>,
) -> Result<Option<(bool, bool)>> {
    if instance_adds_enum_types(reference, observed) {
        return Ok(None);
    }
    let no_ids = AHashMap::new();
    let ids = PairedStoreIds {
        service,
        left: &no_ids,
        right: &no_ids,
    };
    let unchanged = |reference: &SettingsBytecodeInstance, observed: &SettingsBytecodeInstance| {
        reference.class_name == "PackageLink"
            || reconciliation_maps_equal_with_ids(
                &reference.class_name,
                &reference.properties,
                &observed.properties,
                ids,
            ) && reconciliation_values_map_equal_with_ids(
                &reference.attributes,
                &observed.attributes,
                ids,
            )
    };
    let as_held = unchanged(reference, observed);
    let mut named = SettingsBytecode {
        version: super::bytecode::SETTINGS_BINARY_VERSION,
        instances: vec![reference.clone(), observed.clone()],
    };
    canonicalize_settings_property_names(&mut named)?;
    let as_named = if named.instances[0] == *reference && named.instances[1] == *observed {
        as_held
    } else {
        unchanged(&named.instances[0], &named.instances[1])
    };
    Ok(Some((as_held, as_named)))
}

pub(super) fn digest_text(digest: &mut Sha256, text: &str) {
    digest.update((text.len() as u64).to_le_bytes());
    digest.update(text.as_bytes());
}

pub(super) fn digest_map(digest: &mut Sha256, map: &Map<String, Value>) {
    digest.update((map.len() as u64).to_le_bytes());
    for (name, value) in map {
        digest_text(digest, name);
        digest_value(digest, value);
    }
}

pub(super) fn digest_value(digest: &mut Sha256, value: &Value) {
    match &*crate::settings::held::resolve(value) {
        Value::Null => digest.update([0]),
        Value::Bool(value) => digest.update([1, u8::from(*value)]),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                digest.update([2]);
                digest.update(value.to_le_bytes());
            } else if let Some(value) = number.as_i64() {
                digest.update([3]);
                digest.update(value.to_le_bytes());
            } else {
                digest.update([4]);
                digest.update(number.as_f64().map_or(u64::MAX, f64::to_bits).to_le_bytes());
            }
        }
        Value::String(text) => {
            digest.update([5]);
            digest_text(digest, text);
        }
        Value::Array(values) => {
            digest.update([6]);
            digest.update((values.len() as u64).to_le_bytes());
            for value in values {
                digest_value(digest, value);
            }
        }
        Value::Object(map) => {
            digest.update([7]);
            digest_map(digest, map);
        }
    }
}

/// Whether the stored viewport camera holds an enum item that can gain its
/// type: one inside another value, or an attribute. (A property that is an
/// enum item itself is stored and captured without a type.) A pull puts the
/// stored camera back, so a captured store whose camera moved leaves such a
/// store as it is, while one whose camera stood still gives it the type: two
/// captured stores that differ in the camera alone then get different
/// answers.
pub(crate) fn viewport_camera_lacks_enum_types(document: &SettingsBytecode) -> bool {
    workspace_current_camera_index(document)
        .is_some_and(|index| instance_lacks_enum_types(&document.instances[index]))
}

/// Whether an instance holds an enum item that can gain its type (see
/// `viewport_camera_lacks_enum_types`).
pub(crate) fn instance_lacks_enum_types(instance: &SettingsBytecodeInstance) -> bool {
    fn lacks(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                map.get("_type").and_then(Value::as_str) == Some("EnumItem")
                    && map
                        .get("enumType")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                    || map.values().any(lacks)
            }
            Value::Array(values) => values.iter().any(lacks),
            _ => false,
        }
    }
    let inside = |value: &Value| match value {
        Value::Object(map) => map.values().any(lacks),
        Value::Array(values) => values.iter().any(lacks),
        _ => false,
    };
    instance.properties.values().any(inside) || instance.attributes.values().any(lacks)
}

/// The observed store as it came in: its bytes, or how to get them. Under
/// store ids, with nothing to rewrite, those bytes are what is written.
#[derive(Clone, Copy)]
enum ObservedBytes<'a> {
    Given(&'a [u8]),
    Encode(&'a dyn Fn() -> Result<Vec<u8>>),
}

struct AlignmentInput<'a> {
    observed_bytes: ObservedBytes<'a>,
    keep_viewport_camera: bool,
    cache_key: Option<SettingsAlignmentCacheKey>,
    started: std::time::Instant,
    /// Set where the store the alignment leaves holds exactly the observed
    /// document, with the viewport camera it kept.
    stored_as_observed: Option<&'a AtomicBool>,
    /// The file of the store aligned to, which a store written for it is
    /// written over.
    previous: Option<&'a [u8]>,
}

fn same_instances(reference: &SettingsBytecode, observed: &SettingsBytecode) -> bool {
    reference.instances.len() == observed.instances.len()
        && reference
            .instances
            .par_iter()
            .zip(&observed.instances)
            .all(|(reference, observed)| reference == observed)
}

fn align_settings_documents(
    mut reference: SettingsBytecode,
    mut observed: SettingsBytecode,
    input: AlignmentInput<'_>,
) -> Result<SettingsAlignment> {
    let AlignmentInput {
        observed_bytes,
        keep_viewport_camera,
        cache_key,
        started,
        stored_as_observed,
        previous,
    } = input;
    let mut phases = AlignmentPhases {
        instances: observed.instances.len(),
        started,
    };
    phases.end("decode");
    // Taken before alignment rewrites either document.
    let viewport_camera = keep_viewport_camera
        .then(|| reference_viewport_camera(&reference))
        .flatten();
    let keep_camera = |observed: &mut SettingsBytecode| {
        viewport_camera
            .as_ref()
            .is_some_and(|camera| keep_reference_viewport_camera(camera, observed))
    };
    let topology_matches = settings_topology_matches(&reference, &observed);
    let mut positionally_equivalent = topology_matches
        && positional_values_equivalent(&reference, &observed, None, false, Placed::default());
    // A pull keeps the viewport camera. Where every instance stands as in the
    // reference and the camera carries its stored id, the reference's camera
    // goes back before anything else is compared: a store whose camera alone
    // moved is unchanged the way one whose camera stood still is.
    let camera_kept_early =
        topology_matches && !positionally_equivalent && keep_camera(&mut observed);
    if camera_kept_early {
        positionally_equivalent =
            positional_values_equivalent(&reference, &observed, None, false, Placed::default());
    }
    phases.end("comparison by position");
    if positionally_equivalent {
        if positions_add_enum_types(&reference, &observed) {
            if !align_settings_ids_to_reference(&reference, &mut observed) {
                bail!("duplicate instance identity is ambiguous after comparing references");
            }
            keep_camera(&mut observed);
            let upgraded = encode_settings_bytecode_over(&observed, false, previous)?;
            drop_settings_documents(reference, observed);
            return Ok(SettingsAlignment::Changed(upgraded));
        }
        // The cache answers for the bytes alone, also to a caller that keeps
        // no camera.
        if let Some(key) = cache_key.filter(|_| !camera_kept_early) {
            cache_settings_alignment(key);
        }
        if let Some(stored_as_observed) = stored_as_observed {
            stored_as_observed.store(same_instances(&reference, &observed), Ordering::Relaxed);
        }
        drop_settings_documents(reference, observed);
        return Ok(SettingsAlignment::Equivalent);
    }
    // A store whose instances stand where the reference's stand, under the
    // same ids, already carries the reference's identities: nothing is left
    // to align. This is a pull after values changed in Studio, the common
    // one. Under store ids its own bytes are what the encoder writes for it
    // again; any other id passes through the encoder, which normalizes it.
    let already_aligned =
        topology_matches && identities_already_aligned(&reference, &observed, Placed::default());
    phases.end("identity check");
    if already_aligned {
        rayon::join(
            || stabilize_settings_reference_ids(&mut reference),
            || stabilize_settings_reference_ids(&mut observed),
        );
        let camera_kept = keep_camera(&mut observed) || camera_kept_early;
        phases.end("reference ids");
        let keep_encoding = reference
            .instances
            .iter()
            .all(|instance| is_store_identity(&instance.settings_id));
        let aligned = if camera_kept || !keep_encoding {
            encode_settings_bytecode_over(&observed, false, previous)?
        } else {
            match observed_bytes {
                ObservedBytes::Given(bytes) => bytes.to_vec(),
                ObservedBytes::Encode(encode) => encode()?,
            }
        };
        phases.end("encode in place");
        // The bytes written here are the observed document's own.
        let same = stored_as_observed.map(|_| same_instances(&reference, &observed));
        let result =
            finish_settings_alignment(reference, observed, aligned, cache_key, camera_kept);
        if let (Some(stored_as_observed), Some(same), Ok(result)) =
            (stored_as_observed, same, &result)
        {
            stored_as_observed.store(
                same || matches!(result, SettingsAlignment::Changed(_)),
                Ordering::Relaxed,
            );
        }
        phases.end("final comparison");
        return result;
    }
    match align_settings_ids_for_contiguous_structural_change(&mut reference, &mut observed) {
        ContiguousStructuralAlignment::Aligned { ids_kept } => {
            phases.end("structural matching");
            keep_camera(&mut observed);
            let aligned = encode_settings_bytecode_over(&observed, true, previous)?;
            if let Some(stored_as_observed) = stored_as_observed {
                stored_as_observed.store(ids_kept, Ordering::Relaxed);
            }
            drop_settings_documents(reference, observed);
            phases.end("encode after structural matching");
            return Ok(SettingsAlignment::Changed(aligned));
        }
        ContiguousStructuralAlignment::PreparedMismatch => {}
        ContiguousStructuralAlignment::NotApplicable => {
            rayon::join(
                || stabilize_settings_reference_ids(&mut reference),
                || stabilize_settings_reference_ids(&mut observed),
            );
        }
    }
    phases.end("structural attempt");
    if !align_settings_ids_to_reference(&reference, &mut observed) {
        bail!("duplicate instance identity is ambiguous after comparing references");
    }
    phases.end("identity matching");
    let camera_kept = keep_camera(&mut observed) || camera_kept_early;
    let aligned = encode_settings_bytecode_over(&observed, false, previous)?;
    phases.end("encode");
    let result = finish_settings_alignment(reference, observed, aligned, cache_key, camera_kept);
    phases.end("final comparison");
    result
}

/// Step timings of one alignment in the trace log, for stores large enough
/// to matter.
struct AlignmentPhases {
    instances: usize,
    started: std::time::Instant,
}

impl AlignmentPhases {
    fn end(&mut self, phase: &str) {
        if self.instances >= 4_096 {
            crate::app::timing::log_timing(
                &format!(
                    "settings alignment of {} instances: {phase}",
                    self.instances
                ),
                self.started,
            );
        }
        self.started = std::time::Instant::now();
    }
}

/// Equal, unique IDs can be kept when matching is guaranteed to preserve
/// their positions. Store IDs are matched directly; runtime and numeric IDs
/// additionally need the same topology and either exact persistent matches
/// or unchanged duplicate subtrees with consistent incoming references.
/// A moved persistent ID or viewport role invalidates either proof.
fn identities_already_aligned(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    placed: Placed,
) -> bool {
    let mut seen = AHashSet::with_capacity(observed.instances.len());
    let mut all_store_ids = true;
    if reference.instances.len() != observed.instances.len() + placed.len
        || !observed
            .instances
            .iter()
            .enumerate()
            .all(|(index, observed)| {
                let reference = &reference.instances[placed.at(index)];
                all_store_ids &= is_store_identity(&reference.settings_id);
                reference.settings_id == observed.settings_id
                    && seen.insert(reference.settings_id.as_str())
            })
        || placed.camera(reference)
            != workspace_current_camera_index(observed).map(|index| placed.at(index))
    {
        return false;
    }
    if all_store_ids {
        persistent_identities_follow_mapping(reference, observed, |index| Some(placed.at(index)))
    } else {
        settings_topology_placed(reference, observed, placed)
            && (positional_identities_match_exactly(reference, observed, placed)
                || positional_identity_preserved(reference, observed, placed))
    }
}

// Equal, unique IDs and topology are checked by the caller. The matcher
// assigns the viewport first, then unique persistent IDs, then
// non-debug IDs. A sole root without any of those is its unique root group.
// Every other runtime ID still needs the content-based positional proof.
fn positional_identities_match_exactly(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    placed: Placed,
) -> bool {
    let (reference_ids, observed_ids) = rayon::join(
        || persistent_identity_index(reference),
        || persistent_identity_index(observed),
    );
    let camera = workspace_current_camera_index(observed);
    let sole_root = (reference.instances.iter().enumerate())
        .filter(|(index, instance)| !placed.passes(*index) && instance.parent_index.is_none())
        .count()
        == 1;
    observed
        .instances
        .par_iter()
        .enumerate()
        .all(|(index, instance)| {
            if camera == Some(index) {
                return true;
            }
            let persistent = persistent_identity(instance)
                .filter(|id| observed_ids.get(id) == Some(&Some(index)))
                .and_then(|id| reference_ids.get(id).copied().flatten())
                .filter(|candidate| {
                    reference.instances[*candidate].class_name == instance.class_name
                });
            persistent.map_or_else(
                || {
                    !instance.settings_id.starts_with("debug:")
                        || (sole_root && instance.parent_index.is_none())
                },
                |candidate| candidate == placed.at(index),
            )
        })
}

/// Whether the aligned store says the same as the reference, in which case
/// the reference's bytes stay.
fn finish_settings_alignment(
    mut reference: SettingsBytecode,
    mut observed: SettingsBytecode,
    aligned: Vec<u8>,
    cache_key: Option<SettingsAlignmentCacheKey>,
    camera_kept: bool,
) -> Result<SettingsAlignment> {
    canonicalize_settings_property_names(&mut reference)?;
    canonicalize_settings_property_names(&mut observed)?;
    let result = if settings_documents_match(&reference, &observed, false)
        && !document_adds_enum_types(&reference, &observed)
    {
        // The cache answers for the bytes alone, also to a caller that keeps
        // no camera: a pair that only matches with the camera kept stays out.
        if let Some(key) = cache_key.filter(|_| !camera_kept) {
            cache_settings_alignment(key);
        }
        SettingsAlignment::Equivalent
    } else {
        SettingsAlignment::Changed(aligned)
    };
    drop_settings_documents(reference, observed);
    Ok(result)
}

pub(crate) fn drop_settings_document(document: SettingsBytecode) {
    let SettingsBytecode { instances, .. } = document;
    if instances.len() >= 8_192 && rayon::current_num_threads() > 1 {
        instances.into_par_iter().for_each(drop);
    } else {
        drop(instances);
    }
}

pub(crate) fn drop_settings_documents(left: SettingsBytecode, right: SettingsBytecode) {
    if left.instances.len().saturating_add(right.instances.len()) >= 16_384
        && rayon::current_num_threads() > 1
    {
        let SettingsBytecode {
            instances: left, ..
        } = left;
        let SettingsBytecode {
            instances: right, ..
        } = right;
        left.into_par_iter().chain(right).for_each(drop);
    } else {
        drop(left);
        drop(right);
    }
}

pub(crate) fn stabilize_settings_reference_ids(document: &mut SettingsBytecode) {
    stabilize_settings_reference_ids_with_remap(document, None, false);
}

/// `stabilize_settings_reference_ids` for the instances at `rows` of a
/// document that holds its instances under `ids`.
pub(crate) fn stabilize_rows(
    document: &mut SettingsBytecode,
    rows: std::ops::Range<usize>,
    ids: &[String],
) {
    document.instances[rows]
        .par_iter_mut()
        .for_each(|instance| {
            stabilize_record_references(&mut instance.properties, ids);
            stabilize_record_references(&mut instance.attributes, ids);
        });
}

/// Gives the references of the instances at `rows` the ids `remap` gives
/// the instances they name, as an alignment by position does for a store of
/// `service` whose instances took those ids (`remap_positional_ids`).
pub(crate) fn remap_rows(
    document: &mut SettingsBytecode,
    rows: std::ops::Range<usize>,
    remap: &HashMap<String, String>,
    service: &str,
) {
    document.instances[rows]
        .par_iter_mut()
        .for_each(|instance| {
            remap_record_reference_ids(&mut instance.properties, remap, Some(service));
            remap_record_reference_ids(&mut instance.attributes, remap, Some(service));
        });
}

/// Where each instance of `reference` stands in `observed`, when the two
/// copies of a store hold the same instances and they may be told by their
/// places: the two hold the same names, classes and parents in the same
/// order, or, under a parent, the same children with those of one name and
/// class in the same order (Studio keeps a camera or a terrain of its own
/// ahead of what an import brings beside it). The viewport camera is the
/// same instance in both, and no identity says an instance stands elsewhere
/// in the other copy. The values of instances that have a namesake are the
/// caller's to compare.
///
/// The second answer marks the instances of copies in another order that
/// nothing but their order among namesakes tells, with all under them: the
/// caller takes such a pair for one instance only where the two hold the
/// same in full.
pub(crate) fn instance_places(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> Option<(Vec<usize>, Vec<bool>)> {
    let count = reference.instances.len();
    let (places, by_order) = if settings_topology_matches(reference, observed) {
        ((0..count).collect::<Vec<_>>(), vec![false; count])
    } else {
        places_among_namesakes(reference, observed)?
    };
    let mut origins = vec![0; places.len()];
    for (index, place) in places.iter().enumerate() {
        origins[*place] = index;
    }
    if places
        .iter()
        .enumerate()
        .any(|(index, place)| index != *place)
    {
        // An id the store gave, or an import numbered, that both copies hold
        // names one instance: the whole documents pair the two by it.
        let ids = reference
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| (instance.settings_id.as_str(), index))
            .collect::<AHashMap<_, _>>();
        let follows = observed
            .instances
            .iter()
            .enumerate()
            .all(|(place, instance)| {
                let id = instance.settings_id.as_str();
                id.starts_with("debug:") || ids.get(id).is_none_or(|index| *index == origins[place])
            });
        if !follows {
            return None;
        }
    }
    let origin = |place: usize| Some(origins[place]);
    let cameras = match (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    ) {
        (Some(reference), Some(observed)) => places[reference] == observed,
        _ => true,
    };
    (cameras
        && persistent_identities_follow_mapping(reference, observed, origin)
        && store_identities_follow_mapping(reference, observed, origin))
    .then_some((places, by_order))
}

/// Each instance of `reference` with the instance of `observed` that has
/// its name and class under its parent's counterpart, the namesakes of one
/// parent in their order, and whether its order among namesakes, or that of
/// an instance above it, is all that tells it. `None` where the two hold
/// other instances.
fn places_among_namesakes(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> Option<(Vec<usize>, Vec<bool>)> {
    let count = reference.instances.len();
    if observed.instances.len() != count {
        return None;
    }
    let mut namesakes = AHashMap::<_, (Vec<usize>, usize)>::new();
    for (index, instance) in observed.instances.iter().enumerate() {
        let key = (
            instance.parent_index,
            instance.name.as_str(),
            instance.class_name.as_str(),
        );
        namesakes.entry(key).or_default().0.push(index);
    }
    // The instances by their parents, each parent's in their order: a store
    // can hold a parent after its child.
    let mut by_parent = (0..count).collect::<Vec<_>>();
    by_parent.sort_by_key(|index| reference.instances[*index].parent_index);
    let children = |parent: Option<usize>| {
        let start =
            by_parent.partition_point(|index| reference.instances[*index].parent_index < parent);
        by_parent[start..]
            .iter()
            .copied()
            .take_while(move |index| reference.instances[*index].parent_index == parent)
    };
    let mut places = vec![usize::MAX; count];
    let mut by_order = vec![false; count];
    let mut placed = Vec::with_capacity(count);
    let mut place = |index: usize, under: Option<usize>| {
        let instance = &reference.instances[index];
        let (namesakes, next) =
            namesakes.get_mut(&(under, instance.name.as_str(), instance.class_name.as_str()))?;
        *next += 1;
        Some((*namesakes.get(*next - 1)?, namesakes.len() > 1))
    };
    for root in children(None) {
        (places[root], by_order[root]) = place(root, None)?;
        placed.push(root);
    }
    let mut next = 0;
    while let Some(parent) = placed.get(next).copied() {
        next += 1;
        let under = Some(places[parent]);
        for child in children(Some(parent)) {
            let (at, among_namesakes) = place(child, under)?;
            places[child] = at;
            by_order[child] = among_namesakes || by_order[parent];
            placed.push(child);
        }
    }
    (placed.len() == count).then_some((places, by_order))
}

fn stabilize_settings_reference_ids_with_remap(
    document: &mut SettingsBytecode,
    remap: Option<&HashMap<String, String>>,
    keep_instance_indices: bool,
) {
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    let empty_remap = HashMap::new();
    let service = remap
        .and_then(|_| store_service(document))
        .map(str::to_string);
    let stabilize = |instance: &mut SettingsBytecodeInstance| {
        if keep_instance_indices || remap.is_some() {
            let remap = remap.unwrap_or(&empty_remap);
            remap_and_stabilize_record_references(
                &mut instance.properties,
                &ids,
                remap,
                keep_instance_indices,
                service.as_deref(),
            );
            remap_and_stabilize_record_references(
                &mut instance.attributes,
                &ids,
                remap,
                keep_instance_indices,
                service.as_deref(),
            );
        } else {
            stabilize_record_references(&mut instance.properties, &ids);
            stabilize_record_references(&mut instance.attributes, &ids);
        }
    };
    if document.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        document.instances.par_iter_mut().for_each(stabilize);
    } else {
        document.instances.iter_mut().for_each(stabilize);
    }
}

enum ReferenceTarget {
    Internal(usize),
    External(Value),
}

struct ReferenceEdge {
    slot: String,
    target: ReferenceTarget,
}

struct IncomingReference {
    slot: String,
    source: usize,
}

struct ReferenceGraph {
    outgoing: Vec<Vec<ReferenceEdge>>,
    incoming: Vec<Vec<IncomingReference>>,
}

fn build_reference_graph(document: &SettingsBytecode) -> ReferenceGraph {
    let by_id = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let collect = |instance: &SettingsBytecodeInstance| {
        let mut edges = Vec::new();
        collect_map_references(&instance.properties, "properties", &by_id, &mut edges);
        collect_map_references(&instance.attributes, "attributes", &by_id, &mut edges);
        edges
    };
    let outgoing = if document.instances.len() >= 2_048 {
        document
            .instances
            .par_iter()
            .map(collect)
            .collect::<Vec<_>>()
    } else {
        document.instances.iter().map(collect).collect::<Vec<_>>()
    };
    let mut incoming = (0..document.instances.len())
        .map(|_| Vec::new())
        .collect::<Vec<_>>();
    for (source, edges) in outgoing.iter().enumerate() {
        for edge in edges {
            if let ReferenceTarget::Internal(target) = edge.target {
                incoming[target].push(IncomingReference {
                    slot: edge.slot.clone(),
                    source,
                });
            }
        }
    }
    ReferenceGraph { outgoing, incoming }
}

fn collect_map_references(
    values: &Map<String, Value>,
    prefix: &str,
    by_id: &HashMap<&str, usize>,
    output: &mut Vec<ReferenceEdge>,
) {
    let mut slot = prefix.to_string();
    collect_object_references(values, &mut slot, by_id, output);
}

fn collect_text_reference_ids(values: &Map<String, Value>, output: &mut AHashSet<String>) {
    for value in values.values() {
        collect_text_reference_ids_from_value(value, output);
    }
}

fn collect_text_reference_ids_from_value(value: &Value, output: &mut AHashSet<String>) {
    if !reconciliation_value_contains_reference(value) {
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                collect_text_reference_ids_from_value(value, output);
            }
        }
        Value::Object(object) => {
            if is_reference_object(object) {
                for key in ["settingsId", "instanceId"] {
                    if let Some(id) = object.get(key).and_then(Value::as_str) {
                        output.insert(id.to_string());
                    }
                }
            }
            for value in object.values() {
                collect_text_reference_ids_from_value(value, output);
            }
        }
        _ => {}
    }
}

fn collect_object_references(
    values: &Map<String, Value>,
    slot: &mut String,
    by_id: &HashMap<&str, usize>,
    output: &mut Vec<ReferenceEdge>,
) {
    for (name, value) in values {
        if !reconciliation_value_contains_reference(value) {
            continue;
        }
        let original_len = slot.len();
        write!(slot, "/{}:{name}", name.len()).expect("writing to a String cannot fail");
        collect_value_references(value, slot, by_id, output);
        slot.truncate(original_len);
    }
}

fn collect_value_references(
    value: &Value,
    slot: &mut String,
    by_id: &HashMap<&str, usize>,
    output: &mut Vec<ReferenceEdge>,
) {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if !reconciliation_value_contains_reference(value) {
                    continue;
                }
                let original_len = slot.len();
                write!(slot, "[{index}]").expect("writing to a String cannot fail");
                collect_value_references(value, slot, by_id, output);
                slot.truncate(original_len);
            }
        }
        Value::Object(object) if is_reference_object(object) => {
            let id = object
                .get("settingsId")
                .or_else(|| object.get("instanceId"))
                .and_then(Value::as_str);
            let target = id
                .and_then(|id| by_id.get(id).copied())
                .map(ReferenceTarget::Internal)
                .unwrap_or_else(|| ReferenceTarget::External(value.clone()));
            output.push(ReferenceEdge {
                slot: slot.clone(),
                target,
            });
        }
        Value::Object(object) => collect_object_references(object, slot, by_id, output),
        _ => {}
    }
}

fn matching_reference_evidence(
    reference_index: usize,
    observed_index: usize,
    reference_graph: &ReferenceGraph,
    observed_graph: &ReferenceGraph,
    assigned_reference: &[Option<usize>],
    assigned_observed: &[Option<usize>],
) -> Option<usize> {
    let reference_outgoing = &reference_graph.outgoing[reference_index];
    let observed_outgoing = &observed_graph.outgoing[observed_index];
    if reference_outgoing.len() != observed_outgoing.len() {
        return None;
    }

    let mut evidence = 0usize;
    for observed_edge in observed_outgoing {
        let reference_edge = reference_outgoing
            .iter()
            .find(|edge| edge.slot == observed_edge.slot)?;
        match (&reference_edge.target, &observed_edge.target) {
            (ReferenceTarget::External(reference), ReferenceTarget::External(observed)) => {
                if !reconciliation_values_equal(reference, observed, false) {
                    return None;
                }
                evidence += 1;
            }
            (
                ReferenceTarget::Internal(reference_target),
                ReferenceTarget::Internal(observed_target),
            ) => {
                let expected_reference = if *observed_target == observed_index {
                    Some(reference_index)
                } else {
                    assigned_reference[*observed_target]
                };
                let expected_observed = if *reference_target == reference_index {
                    Some(observed_index)
                } else {
                    assigned_observed[*reference_target]
                };
                if expected_reference.is_some_and(|target| target != *reference_target)
                    || expected_observed.is_some_and(|target| target != *observed_target)
                {
                    return None;
                }
                if expected_reference.is_some() || expected_observed.is_some() {
                    evidence += 1;
                }
            }
            _ => return None,
        }
    }

    for observed_edge in &observed_graph.incoming[observed_index] {
        let expected_source = if observed_edge.source == observed_index {
            Some(reference_index)
        } else {
            assigned_reference[observed_edge.source]
        };
        if let Some(expected_source) = expected_source {
            if !reference_graph.incoming[reference_index]
                .iter()
                .any(|edge| edge.source == expected_source && edge.slot == observed_edge.slot)
            {
                return None;
            }
            evidence += 1;
        }
    }
    for reference_edge in &reference_graph.incoming[reference_index] {
        let expected_source = if reference_edge.source == reference_index {
            Some(observed_index)
        } else {
            assigned_observed[reference_edge.source]
        };
        if let Some(expected_source) = expected_source
            && !observed_graph.incoming[observed_index]
                .iter()
                .any(|edge| edge.source == expected_source && edge.slot == reference_edge.slot)
        {
            return None;
        }
    }
    Some(evidence)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct IdentityScore {
    has_reference_evidence: bool,
    reference_evidence: usize,
    content_matches: bool,
    subtree_matches: bool,
}

#[derive(Hash, PartialEq, Eq)]
struct IdentitySubtreeKey<'a> {
    name: &'a str,
    class_name: &'a str,
    properties: Vec<(&'a str, &'a Value)>,
    attributes: Vec<(&'a str, &'a Value)>,
    children: Vec<usize>,
}

#[derive(Hash, PartialEq, Eq)]
struct IdentityCandidateKey<'a> {
    properties: Vec<(&'a String, &'a Value)>,
    attributes: Vec<(&'a String, &'a Value)>,
    subtree: usize,
    reference_identity: Option<usize>,
}

// Collapse only candidates that receive exactly the same identity_score for every
// observed instance. Multiplicity still makes a winning bucket ambiguous; this
// does not choose an identity by content hash or skip reference evidence.
fn identity_candidate_representatives(
    document: &SettingsBytecode,
    graph: &ReferenceGraph,
    subtree_keys: Option<&[usize]>,
    candidates: &[usize],
) -> (Vec<usize>, AHashMap<usize, usize>) {
    let mut groups = AHashMap::new();
    let mut representatives = Vec::new();
    let mut multiplicities = AHashMap::new();
    for &candidate in candidates {
        let instance = &document.instances[candidate];
        let key = IdentityCandidateKey {
            properties: instance.properties.iter().collect(),
            attributes: instance.attributes.iter().collect(),
            subtree: subtree_keys.map_or(0, |keys| keys[candidate]),
            reference_identity: (!graph.outgoing[candidate].is_empty()
                || !graph.incoming[candidate].is_empty())
            .then_some(candidate),
        };
        let representative = *groups.entry(key).or_insert_with(|| {
            representatives.push(candidate);
            candidate
        });
        *multiplicities.entry(representative).or_default() += 1;
    }
    (representatives, multiplicities)
}

// Intern exact content, not hashes used as identity. Children are an unordered multiset;
// references remain separate, stronger evidence in identity_score. A non-match here
// never excludes a candidate (float rounding or an actual edit may change its key).
fn identity_subtree_keys<'a>(
    document: &'a SettingsBytecode,
    keys: &mut AHashMap<IdentitySubtreeKey<'a>, usize>,
    mut required: Vec<bool>,
) -> Vec<usize> {
    let mut children = vec![Vec::new(); document.instances.len()];
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index {
            children[parent].push(index);
        }
    }
    // Already-assigned instances cannot be candidates. Only hash unresolved
    // subtrees, including every descendant needed to distinguish their roots.
    let mut pending = required
        .iter()
        .enumerate()
        .filter_map(|(index, needed)| needed.then_some(index))
        .collect::<Vec<_>>();
    while let Some(index) = pending.pop() {
        for &child in &children[index] {
            if !required[child] {
                required[child] = true;
                pending.push(child);
            }
        }
    }
    let mut remaining = children.iter().map(Vec::len).collect::<Vec<_>>();
    let mut ready = remaining
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0 && required[index]).then_some(index))
        .collect::<Vec<_>>();
    let mut result = vec![0; document.instances.len()];
    while let Some(index) = ready.pop() {
        let instance = &document.instances[index];
        let mut child_keys = std::mem::take(&mut children[index]);
        for child in &mut child_keys {
            *child = result[*child];
        }
        child_keys.sort_unstable();
        let stable = |values: &'a Map<String, Value>, properties| {
            values
                .iter()
                .filter(|(name, value)| identity_value_is_stable(name, value, properties))
                .map(|(name, value)| (name.as_str(), value))
                .collect()
        };
        let key = IdentitySubtreeKey {
            name: &instance.name,
            class_name: &instance.class_name,
            properties: stable(&instance.properties, true),
            attributes: stable(&instance.attributes, false),
            children: child_keys,
        };
        let next = keys.len() + 1;
        let id = *keys.entry(key).or_insert(next);
        result[index] = id;
        if let Some(parent) = instance.parent_index.filter(|parent| required[*parent]) {
            remaining[parent] -= 1;
            if remaining[parent] == 0 {
                ready.push(parent);
            }
        }
    }
    result
}

struct IdentityScoreContext<'a> {
    reference: &'a SettingsBytecode,
    observed: &'a SettingsBytecode,
    reference_graph: &'a ReferenceGraph,
    observed_graph: &'a ReferenceGraph,
    assigned_reference: &'a [Option<usize>],
    assigned_observed: &'a [Option<usize>],
    subtree_keys: Option<&'a (Vec<usize>, Vec<usize>)>,
}

fn identity_score(
    context: &IdentityScoreContext<'_>,
    reference_index: usize,
    observed_index: usize,
) -> Option<IdentityScore> {
    let reference_instance = &context.reference.instances[reference_index];
    let observed_instance = &context.observed.instances[observed_index];
    let reference_evidence = matching_reference_evidence(
        reference_index,
        observed_index,
        context.reference_graph,
        context.observed_graph,
        context.assigned_reference,
        context.assigned_observed,
    )?;
    let content_matches = holds_same_content(reference_instance, observed_instance);
    if reference_evidence == 0 && !content_matches {
        return None;
    }
    Some(IdentityScore {
        has_reference_evidence: reference_evidence != 0,
        reference_evidence,
        content_matches,
        subtree_matches: context.subtree_keys.is_some_and(|(reference, observed)| {
            reference[reference_index] != 0
                && reference[reference_index] == observed[observed_index]
        }),
    })
}

/// Whether two instances hold the same to the matching: what they hold
/// beside the references among it and what Studio keeps for itself.
fn holds_same_content(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
) -> bool {
    reconciliation_identity_maps_equal(&reference.properties, &observed.properties, true)
        && reconciliation_identity_maps_equal(&reference.attributes, &observed.attributes, false)
}

fn settings_topology_matches(reference: &SettingsBytecode, observed: &SettingsBytecode) -> bool {
    settings_topology_placed(reference, observed, Placed::default())
}

/// Whether each observed instance stands under the name, class and parent of
/// its counterpart (`Placed`).
fn settings_topology_placed(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    placed: Placed,
) -> bool {
    reference.instances.len() == observed.instances.len() + placed.len
        && (observed.instances.iter().enumerate()).all(|(index, observed)| {
            let reference = &reference.instances[placed.at(index)];
            reference.name == observed.name
                && reference.class_name == observed.class_name
                && reference.parent_index == observed.parent_index.map(|parent| placed.at(parent))
        })
}

pub(crate) fn persistent_identity(instance: &SettingsBytecodeInstance) -> Option<&str> {
    persistent_identity_of(&instance.properties)
}

/// The same from an instance's properties alone.
pub(crate) fn persistent_identity_of(properties: &Map<String, Value>) -> Option<&str> {
    let value = properties.get("UniqueId")?.as_object()?;
    if value.get("_type")?.as_str()? != "UniqueId" {
        return None;
    }
    let id = value.get("value")?.as_str()?;
    persistent_identity_text(id)
}

pub(crate) fn persistent_identity_text(id: &str) -> Option<&str> {
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && id.bytes().any(|b| b != b'0'))
        .then_some(id)
}

pub(crate) fn persistent_identity_index(
    document: &SettingsBytecode,
) -> AHashMap<&str, Option<usize>> {
    let mut result = AHashMap::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(id) = persistent_identity(instance) {
            result
                .entry(id)
                .and_modify(|value| *value = None)
                .or_insert(Some(index));
        }
    }
    result
}

fn persistent_identities_follow_mapping(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    reference_position: impl Fn(usize) -> Option<usize>,
) -> bool {
    let reference_ids = persistent_identity_index(reference);
    if reference_ids.is_empty() {
        return true;
    }
    let observed_ids = persistent_identity_index(observed);
    observed_ids.iter().all(|(id, observed_index)| {
        let (Some(reference_index), Some(observed_index)) =
            (reference_ids.get(id).copied().flatten(), *observed_index)
        else {
            return true;
        };
        reference_position(observed_index) == Some(reference_index)
            || is_reconciliation_protected_workspace_camera(reference, reference_index)
                && is_reconciliation_protected_workspace_camera(observed, observed_index)
    })
}

/// Whether the id an observed instance arrives with is one a store gave it:
/// Studio's plugin hands the store id of an instance a push created or
/// matched back with a capture. A debug id is Studio's own and recycled
/// between sessions, and a bare number is the position an import numbers an
/// instance by; neither says which stored instance it is.
pub(crate) fn is_store_identity(id: &str) -> bool {
    !id.starts_with("debug:") && !id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether a mapping of observed positions onto reference positions keeps
/// every store identity the observed document arrives with on the reference
/// instance of that identity. A mapping by position or by path that moves
/// such an identity to another instance would hand it to whatever stands
/// where its instance was before a rename or a move.
fn store_identities_follow_mapping(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    reference_position: impl Fn(usize) -> Option<usize>,
) -> bool {
    let mut reference_ids = None;
    observed
        .instances
        .iter()
        .enumerate()
        .all(|(index, instance)| {
            let id = instance.settings_id.as_str();
            if !is_store_identity(id)
                || reference_position(index)
                    .is_some_and(|position| reference.instances[position].settings_id == id)
            {
                return true;
            }
            !reference_ids
                .get_or_insert_with(|| {
                    reference
                        .instances
                        .iter()
                        .map(|instance| instance.settings_id.as_str())
                        .collect::<AHashSet<_>>()
                })
                .contains(id)
        })
}

type IdentityGroupKey<'a> = (Option<usize>, &'a str, &'a str);

struct IdentityLookup<'a> {
    reference_by_id: HashMap<&'a str, usize>,
    reference_by_unique_id: AHashMap<&'a str, Option<usize>>,
    observed_by_unique_id: AHashMap<&'a str, Option<usize>>,
}

struct ScoreInputs<'a, 'b> {
    reference_groups: &'b HashMap<IdentityGroupKey<'a>, Vec<usize>>,
    reference_graph: &'b ReferenceGraph,
    observed_graph: &'b ReferenceGraph,
    subtree_keys: Option<&'b (Vec<usize>, Vec<usize>)>,
    /// The matching stops at instances only what they hold tells apart.
    told_alone: bool,
}

/// How a matching by order took an observed instance among its namesakes
/// under one parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ByOrder {
    /// Paired, where both copies hold as many of the namesakes.
    Paired,
    /// Paired, where one copy holds more of them.
    BesideMore,
    /// One of those more: it is no instance of the reference.
    More,
}

struct IdentityAssignment<'a> {
    reference: &'a SettingsBytecode,
    observed: &'a SettingsBytecode,
    old_ids: Vec<String>,
    reserved_reference_ids: HashSet<String>,
    blocked_ids: HashSet<String>,
    assigned_ids: Vec<Option<String>>,
    assigned_reference: Vec<Option<usize>>,
    assigned_observed: Vec<Option<usize>>,
    used_reference: Vec<bool>,
    used_ids: HashSet<String>,
    generated: usize,
    remaining: usize,
    shape_keys: Option<(Vec<u64>, Vec<u64>)>,
    /// A matching that stops at what instances hold reached such a group.
    compares_values: bool,
    /// The observed instances such a matching took by their order among
    /// their namesakes under one parent, where it pairs them so.
    by_order: Option<Vec<(usize, ByOrder)>>,
}

/// Hash of an instance's subtree by names and classes only, so same-named
/// siblings that swapped places pair with the subtree they came from.
fn subtree_shape_keys(document: &SettingsBytecode) -> Vec<u64> {
    use std::hash::{Hash, Hasher};
    let count = document.instances.len();
    let mut children = vec![Vec::new(); count];
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index
            && parent < index
        {
            children[parent].push(index);
        }
    }
    let mut keys = vec![0u64; count];
    for index in (0..count).rev() {
        let instance = &document.instances[index];
        let mut child_keys = children[index]
            .iter()
            .map(|child| keys[*child])
            .collect::<Vec<_>>();
        child_keys.sort_unstable();
        let mut hasher = ahash::AHasher::default();
        instance.name.hash(&mut hasher);
        instance.class_name.hash(&mut hasher);
        child_keys.hash(&mut hasher);
        keys[index] = hasher.finish();
    }
    keys
}

struct IdentityNumberField {
    attribute: bool,
    property: String,
    pointer: String,
}

impl IdentityNumberField {
    fn read(&self, instance: &SettingsBytecodeInstance) -> Option<f64> {
        let values = if self.attribute {
            &instance.attributes
        } else {
            &instance.properties
        };
        let value = values.get(&self.property)?;
        // Enum equality can use either name or number, so its number is not a safe filter.
        if contains_identity_enum(value) {
            return None;
        }
        value
            .pointer(&self.pointer)?
            .as_f64()
            .filter(|value| value.is_finite())
    }
}

fn contains_identity_enum(value: &Value) -> bool {
    match value {
        Value::Object(values) => {
            values.get("_type").and_then(Value::as_str) == Some("EnumItem")
                || values.values().any(contains_identity_enum)
        }
        Value::Array(values) => values.iter().any(contains_identity_enum),
        _ => false,
    }
}

fn identity_number_pointers(value: &Value, pointer: &mut String, output: &mut Vec<String>) {
    match value {
        Value::Number(_) => output.push(pointer.clone()),
        Value::Object(values) => {
            for (name, value) in values {
                let length = pointer.len();
                pointer.push('/');
                pointer.push_str(&name.replace('~', "~0").replace('/', "~1"));
                identity_number_pointers(value, pointer, output);
                pointer.truncate(length);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let length = pointer.len();
                write!(pointer, "/{index}").expect("writing to a String cannot fail");
                identity_number_pointers(value, pointer, output);
                pointer.truncate(length);
            }
        }
        _ => {}
    }
}

struct IdentityCandidateIndex {
    field: IdentityNumberField,
    values: Vec<(f64, usize)>,
}

impl IdentityCandidateIndex {
    fn build(document: &SettingsBytecode, candidates: &[usize]) -> Option<Self> {
        if candidates.len() < 32 {
            return None;
        }
        let first = &document.instances[candidates[0]];
        let mut best = None;
        let mut best_distinct = 1;
        for (attribute, properties) in [(false, &first.properties), (true, &first.attributes)] {
            for (property, value) in properties {
                if !identity_value_is_stable(property, value, !attribute)
                    || contains_identity_enum(value)
                {
                    continue;
                }
                let mut pointers = Vec::new();
                identity_number_pointers(value, &mut String::new(), &mut pointers);
                for pointer in pointers {
                    let field = IdentityNumberField {
                        attribute,
                        property: property.clone(),
                        pointer,
                    };
                    let values = candidates
                        .iter()
                        .filter_map(|index| {
                            field
                                .read(&document.instances[*index])
                                .map(|value| (value, *index))
                        })
                        .collect::<Vec<_>>();
                    let distinct = values
                        .iter()
                        .map(|(value, _)| value.to_bits())
                        .collect::<AHashSet<_>>()
                        .len();
                    if distinct > best_distinct {
                        best_distinct = distinct;
                        best = Some(Self { field, values });
                    }
                }
            }
        }
        if let Some(index) = best.as_mut() {
            index
                .values
                .sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
        }
        best
    }

    fn matching(&self, instance: &SettingsBytecodeInstance) -> Option<&[(f64, usize)]> {
        let value = self.field.read(instance)?;
        // A conservative superset of the existing f32 comparison, including its relative
        // tolerance and rounding at either boundary. identity_score remains authoritative.
        let epsilon = 4.0 * f64::from(f32::EPSILON);
        let radius = ((epsilon + 4.0 * f64::EPSILON) / (1.0 - epsilon) * value.abs().max(1.0)).max(
            CFRAME_ROTATION_EPSILON
                .max(CFRAME_ROTATION_NOISE)
                .max(CFRAME_POSITION_NOISE)
                + 4.0 * f64::EPSILON,
        );
        let start = self
            .values
            .partition_point(|(candidate, _)| *candidate < value - radius);
        let end = self
            .values
            .partition_point(|(candidate, _)| *candidate <= value + radius);
        Some(&self.values[start..end])
    }
}

fn identity_group_proposals(
    context: &IdentityScoreContext<'_>,
    observed_group: &[usize],
    candidates: &[usize],
) -> Vec<(usize, usize)> {
    let (representatives, multiplicities) = identity_candidate_representatives(
        context.reference,
        context.reference_graph,
        context
            .subtree_keys
            .map(|(reference, _)| reference.as_slice()),
        candidates,
    );
    let unreferenced = |index: usize| {
        context.observed_graph.outgoing[index].is_empty()
            && context.observed_graph.incoming[index].is_empty()
    };
    let content_index =
        if representatives.len() > 1 && observed_group.iter().any(|index| unreferenced(*index)) {
            IdentityCandidateIndex::build(context.reference, &representatives)
        } else {
            None
        };
    let propose = |&index: &usize| {
        let mut best = None;
        let mut best_candidate = None;
        let mut tied = false;
        let narrowed = content_index
            .as_ref()
            .filter(|_| unreferenced(index))
            .and_then(|lookup| lookup.matching(&context.observed.instances[index]));
        let count = narrowed.map_or(representatives.len(), <[_]>::len);
        for offset in 0..count {
            let candidate =
                narrowed.map_or_else(|| representatives[offset], |values| values[offset].1);
            let Some(score) = identity_score(context, candidate, index) else {
                continue;
            };
            match best {
                None => {
                    best = Some(score);
                    best_candidate = Some(candidate);
                    tied = multiplicities[&candidate] > 1;
                }
                Some(current) if score > current => {
                    best = Some(score);
                    best_candidate = Some(candidate);
                    tied = multiplicities[&candidate] > 1;
                }
                Some(current) if score == current => tied = true,
                _ => {}
            }
        }
        best_candidate
            .filter(|_| !tied)
            .map(|candidate| (index, candidate))
    };
    // Scores use one immutable assignment snapshot. Preserve observed order;
    // conflicting proposals are rejected before any assignment is applied.
    if observed_group.len() >= 32 && rayon::current_num_threads() > 1 {
        observed_group.par_iter().filter_map(propose).collect()
    } else {
        observed_group.iter().filter_map(propose).collect()
    }
}

impl<'a> IdentityAssignment<'a> {
    fn new(reference: &'a SettingsBytecode, observed: &'a SettingsBytecode) -> Self {
        Self {
            reference,
            observed,
            old_ids: observed
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
                .collect(),
            reserved_reference_ids: HashSet::new(),
            blocked_ids: HashSet::new(),
            assigned_ids: vec![None; observed.instances.len()],
            assigned_reference: vec![None; observed.instances.len()],
            assigned_observed: vec![None; reference.instances.len()],
            used_reference: vec![false; reference.instances.len()],
            used_ids: HashSet::new(),
            generated: 0,
            remaining: observed.instances.len(),
            shape_keys: None,
            compares_values: false,
            by_order: None,
        }
    }

    fn assign(&mut self, index: usize, candidate: usize) {
        let id = self.reference.instances[candidate].settings_id.clone();
        self.used_reference[candidate] = true;
        self.used_ids.insert(id.clone());
        self.assigned_reference[index] = Some(candidate);
        self.assigned_observed[candidate] = Some(index);
        self.assigned_ids[index] = Some(id);
        self.remaining -= 1;
    }

    fn assign_fresh(&mut self, index: usize) {
        let preferred = &self.old_ids[index];
        let id = if !self.reserved_reference_ids.contains(preferred)
            && self.used_ids.insert(preferred.clone())
        {
            preferred.clone()
        } else {
            loop {
                let candidate = format!("reconcile:{:x}", self.generated);
                self.generated += 1;
                if self.blocked_ids.insert(candidate.clone())
                    && self.used_ids.insert(candidate.clone())
                {
                    break candidate;
                }
            }
        };
        self.assigned_ids[index] = Some(id);
        self.remaining -= 1;
    }

    fn unassigned_groups(&self) -> BTreeMap<IdentityGroupKey<'a>, Vec<usize>> {
        let mut groups = BTreeMap::<IdentityGroupKey<'a>, Vec<usize>>::new();
        for (index, instance) in self.observed.instances.iter().enumerate() {
            if self.assigned_ids[index].is_some() {
                continue;
            }
            let reference_parent = match instance.parent_index {
                Some(parent) => {
                    if self
                        .assigned_ids
                        .get(parent)
                        .and_then(Option::as_ref)
                        .is_none()
                    {
                        continue;
                    }
                    self.assigned_reference[parent]
                }
                None => None,
            };
            groups
                .entry((
                    reference_parent,
                    instance.name.as_str(),
                    instance.class_name.as_str(),
                ))
                .or_default()
                .push(index);
        }
        groups
    }

    fn run_exact(&mut self, lookup: &IdentityLookup<'_>) -> bool {
        let reference = self.reference;
        let observed = self.observed;
        let mut progressed = false;
        for (index, instance) in observed.instances.iter().enumerate() {
            if self.assigned_ids[index].is_some() {
                continue;
            }
            let persistent = persistent_identity(instance)
                .filter(|id| lookup.observed_by_unique_id.get(id) == Some(&Some(index)))
                .and_then(|id| lookup.reference_by_unique_id.get(id).copied().flatten())
                .filter(|candidate| {
                    reference.instances[*candidate].class_name == instance.class_name
                });
            let Some(candidate) = persistent
                .or_else(|| {
                    (!instance.settings_id.starts_with("debug:"))
                        .then(|| {
                            lookup
                                .reference_by_id
                                .get(instance.settings_id.as_str())
                                .copied()
                        })
                        .flatten()
                })
                .filter(|candidate| {
                    !self.used_reference[*candidate]
                        && !self
                            .used_ids
                            .contains(&reference.instances[*candidate].settings_id)
                })
            else {
                continue;
            };
            self.assign(index, candidate);
            progressed = true;
        }
        progressed
    }

    fn run_unique(&mut self, reference_groups: &HashMap<IdentityGroupKey<'a>, Vec<usize>>) {
        let observed = self.observed;
        let mut observed_counts = AHashMap::with_capacity(observed.instances.len());
        for instance in &observed.instances {
            *observed_counts
                .entry((
                    instance.parent_index,
                    instance.name.as_str(),
                    instance.class_name.as_str(),
                ))
                .or_insert(0usize) += 1;
        }
        // Resolve parent-before-child singleton groups in one pass. The scored
        // path makes the same decision, but builds a Vec for every unique child
        // and revisits the entire document at each depth after a reorder.
        for (index, instance) in observed.instances.iter().enumerate() {
            if self.assigned_ids[index].is_some()
                || observed_counts[&(
                    instance.parent_index,
                    instance.name.as_str(),
                    instance.class_name.as_str(),
                )] != 1
            {
                continue;
            }
            let parent = match instance.parent_index {
                Some(parent) => match self.assigned_reference.get(parent).copied().flatten() {
                    Some(parent) => Some(parent),
                    None => continue,
                },
                None => None,
            };
            let Some([candidate]) = reference_groups
                .get(&(parent, instance.name.as_str(), instance.class_name.as_str()))
                .map(Vec::as_slice)
            else {
                continue;
            };
            if !self.used_reference[*candidate] {
                self.assign(index, *candidate);
            }
        }
    }

    fn run_scored(
        &mut self,
        inputs: &ScoreInputs<'a, '_>,
        unchanged_content_groups: &mut HashMap<usize, (Vec<usize>, Vec<usize>)>,
    ) -> bool {
        let reference = self.reference;
        let observed = self.observed;
        let mut progressed = false;
        for (key, observed_group) in self.unassigned_groups() {
            let group_started = Instant::now();
            let observed_count = observed_group.len();
            let candidates = inputs
                .reference_groups
                .get(&key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| !self.used_reference[*candidate])
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                for index in observed_group {
                    self.assign_fresh(index);
                }
                progressed = true;
                continue;
            }
            if observed_group.len() == 1 && candidates.len() == 1 {
                self.assign(observed_group[0], candidates[0]);
                progressed = true;
                continue;
            }

            if inputs.told_alone {
                // Namesakes under one parent stand in the order it was given
                // them in both copies; what one copy holds more comes last.
                let Some(by_order) = &mut self.by_order else {
                    self.compares_values = true;
                    return progressed;
                };
                let how = if observed_group.len() == candidates.len() {
                    ByOrder::Paired
                } else {
                    ByOrder::BesideMore
                };
                by_order.extend(
                    observed_group
                        .iter()
                        .skip(candidates.len())
                        .map(|index| (*index, ByOrder::More)),
                );
                let pairs = observed_group
                    .into_iter()
                    .zip(candidates)
                    .collect::<Vec<_>>();
                by_order.extend(pairs.iter().map(|(index, _)| (*index, how)));
                for (index, candidate) in pairs {
                    self.assign(index, candidate);
                }
                progressed = true;
                continue;
            }
            let group_index = observed_group[0];
            if unchanged_content_groups.get(&group_index).is_some_and(
                |(previous_observed, previous_candidates)| {
                    previous_observed == &observed_group && previous_candidates == &candidates
                },
            ) {
                continue;
            }

            let score_context = IdentityScoreContext {
                reference,
                observed,
                reference_graph: inputs.reference_graph,
                observed_graph: inputs.observed_graph,
                assigned_reference: &self.assigned_reference,
                assigned_observed: &self.assigned_observed,
                subtree_keys: inputs.subtree_keys,
            };
            let proposals = identity_group_proposals(&score_context, &observed_group, &candidates);
            let mut proposal_counts = HashMap::<usize, usize>::new();
            for (_, candidate) in &proposals {
                *proposal_counts.entry(*candidate).or_default() += 1;
            }
            let mut assigned = false;
            for (index, candidate) in proposals {
                if proposal_counts.get(&candidate) != Some(&1) || self.used_reference[candidate] {
                    continue;
                }
                self.assign(index, candidate);
                progressed = true;
                assigned = true;
            }
            if group_started.elapsed().as_millis() >= 10 {
                log_global(
                    4,
                    format_args!(
                        "[renium] identity candidate group: parent={:?} name={:?} class={} observed={observed_count} candidates={} elapsed_ms={:.1}",
                        key.0,
                        key.1,
                        key.2,
                        candidates.len(),
                        group_started.elapsed().as_secs_f64() * 1000.0,
                    ),
                );
            }
            // No score can change without different group membership when
            // neither side has reference edges. Leave ties for the existing
            // fallback pass instead of rescoring them at every hierarchy depth.
            if !assigned
                && observed_group.iter().all(|index| {
                    inputs.observed_graph.outgoing[*index].is_empty()
                        && inputs.observed_graph.incoming[*index].is_empty()
                })
                && candidates.iter().all(|index| {
                    inputs.reference_graph.outgoing[*index].is_empty()
                        && inputs.reference_graph.incoming[*index].is_empty()
                })
            {
                unchanged_content_groups.insert(group_index, (observed_group, candidates));
            }
        }
        progressed
    }

    fn run_fallback(
        &mut self,
        reference_groups: &HashMap<IdentityGroupKey<'a>, Vec<usize>>,
    ) -> bool {
        let mut progressed = false;
        let shapes = self.shape_keys.take().unwrap_or_else(|| {
            (
                subtree_shape_keys(self.reference),
                subtree_shape_keys(self.observed),
            )
        });
        for (key, observed_group) in self.unassigned_groups() {
            let candidates = reference_groups
                .get(&key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| !self.used_reference[*candidate])
                .collect::<Vec<_>>();
            let mut by_shape: HashMap<u64, std::collections::VecDeque<usize>> = HashMap::new();
            for (position, candidate) in candidates.iter().enumerate() {
                by_shape
                    .entry(shapes.0[*candidate])
                    .or_default()
                    .push_back(position);
            }
            let mut taken = vec![false; candidates.len()];
            let mut unpaired = Vec::new();
            for index in observed_group {
                match by_shape
                    .get_mut(&shapes.1[index])
                    .and_then(std::collections::VecDeque::pop_front)
                {
                    Some(position) => {
                        taken[position] = true;
                        self.assign(index, candidates[position]);
                        progressed = true;
                    }
                    None => unpaired.push(index),
                }
            }
            let mut leftovers = candidates
                .iter()
                .zip(&taken)
                .filter(|(_, taken)| !**taken)
                .map(|(candidate, _)| *candidate);
            for index in unpaired {
                match leftovers.next() {
                    Some(candidate) => self.assign(index, candidate),
                    None => self.assign_fresh(index),
                }
                progressed = true;
            }
        }
        self.shape_keys = Some(shapes);
        progressed
    }
}

pub(crate) fn align_settings_ids_to_reference(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> bool {
    // A store read back after it was aligned once: matching would leave
    // each instance where it stands.
    if identities_already_aligned(reference, observed, Placed::default())
        && settings_topology_matches(reference, observed)
    {
        return true;
    }
    // Where one copy holds one run of instances the other lacks, and its
    // place tells that run, the instances beside it stand where their
    // places say, as a push of the instances that differ aligns them. A
    // fresh destination holds its retained containers alone, which the
    // matching finds without indexing the rest (`match_settings_ids`).
    let run = (observed.instances.len() >= reference.instances.len() / 4)
        .then(|| {
            super::bytecode::property_edit::Moved::between_copies(
                &observed.instances,
                &reference.instances,
            )
        })
        .flatten()
        .flatten()
        .and_then(|moved| moved.run())
        .filter(|run| run_is_told(run.clone(), &reference.instances, &observed.instances));
    if let Some((run, of_reference)) = run
        && align_settings_ids_beside_run(reference, observed, run, !of_reference)
    {
        return true;
    }
    align_settings_ids_by_matching(reference, observed)
}

/// Whether the run of instances one copy of a store alone holds is the one
/// its place gives. Where the instance ahead of it is a namesake of its last,
/// the run could as well start earlier, and only a UniqueId both copies hold
/// for the instance ahead says it does not. An instance of the run under a
/// UniqueId the other copy holds was moved, not added. A UniqueId is one
/// `persistent_identity` reads.
pub(crate) fn run_is_told(
    (run, of_reference): (std::ops::Range<usize>, bool),
    reference: &[SettingsBytecodeInstance],
    observed: &[SettingsBytecodeInstance],
) -> bool {
    let (longer, shorter) = if of_reference {
        (reference, observed)
    } else {
        (observed, reference)
    };
    let of_run = longer[run.clone()]
        .iter()
        .filter_map(persistent_identity)
        .collect::<HashSet<_>>();
    if !of_run.is_empty()
        && shorter
            .iter()
            .filter_map(persistent_identity)
            .any(|held| of_run.contains(held))
    {
        return false;
    }
    let (Some(ahead), Some(last)) = (run.start.checked_sub(1), run.end.checked_sub(1)) else {
        return true;
    };
    if longer[ahead].name != longer[last].name
        || longer[ahead].class_name != longer[last].class_name
    {
        return true;
    }
    matches!(
        (
            persistent_identity(&longer[ahead]),
            shorter.get(ahead).and_then(persistent_identity)
        ),
        (Some(one), Some(other)) if one == other
    )
}

/// `align_settings_ids_to_reference` where it leaves every instance at its
/// place: the identities are aligned as they stand, or the observed instances
/// take the ids of the reference by position. `false` where it would match
/// the instances by what tells them apart, with `observed` as it was. The
/// conditions are those of `align_settings_ids_to_reference` and of the
/// start of `align_settings_ids_by_matching`.
pub(crate) fn align_settings_ids_by_place(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> bool {
    align_settings_ids_placed(reference, observed, Placed::default())
}

/// `align_settings_ids_by_place` with each observed instance taken for the
/// reference's at the place `placed` gives it.
fn align_settings_ids_placed(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
    placed: Placed,
) -> bool {
    if !settings_topology_placed(reference, observed, placed) {
        return false;
    }
    if identities_already_aligned(reference, observed, placed) {
        return true;
    }
    let camera_positions_match = match (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    ) {
        (Some(reference), Some(observed)) => reference == placed.at(observed),
        _ => true,
    };
    if camera_positions_match && positional_identity_preserved(reference, observed, placed) {
        remap_positional_ids(reference, observed, placed);
        return true;
    }
    false
}

/// Takes the instances at `run` out of a document: the others stand as in
/// a document that never held them. `None`, with the document as it was,
/// where an instance beside the run stands under one of them.
fn take_run(
    document: &mut SettingsBytecode,
    run: &std::ops::Range<usize>,
) -> Option<Vec<SettingsBytecodeInstance>> {
    if run.end > document.instances.len()
        || document
            .instances
            .iter()
            .enumerate()
            .any(|(index, instance)| {
                !run.contains(&index)
                    && instance
                        .parent_index
                        .is_some_and(|parent| run.contains(&parent))
            })
    {
        return None;
    }
    let taken = document.instances.drain(run.clone()).collect();
    for instance in &mut document.instances {
        if let Some(parent) = instance.parent_index.as_mut()
            && *parent >= run.end
        {
            *parent -= run.len();
        }
    }
    Some(taken)
}

/// Puts the instances `take_run` took back where they stood.
fn put_run(
    document: &mut SettingsBytecode,
    run: &std::ops::Range<usize>,
    taken: Vec<SettingsBytecodeInstance>,
) {
    for instance in &mut document.instances {
        if let Some(parent) = instance.parent_index.as_mut()
            && *parent >= run.start
        {
            *parent += run.len();
        }
    }
    document.instances.splice(run.start..run.start, taken);
}

/// `align_settings_ids_by_place` for two copies of a store of which one
/// holds one run of instances the other lacks, every other instance standing
/// as its counterpart stands. The instances beside the run are aligned by
/// their places. An instance of a run `observed` holds has no counterpart:
/// it keeps its id, or takes a new one where the reference holds that id for
/// another instance, as the matching gives one. `false` where the instances
/// beside the run are not aligned by place, where the matching may pair them
/// otherwise by the viewport cameras, by their ids (`ids_paired_elsewhere`)
/// or by what twins hold (`twins_paired_otherwise`), or where more than one
/// instance of the run needs a new id; `observed` is then as it was.
pub(crate) fn align_settings_ids_beside_run(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
    run: std::ops::Range<usize>,
    observed_holds_run: bool,
) -> bool {
    #[cfg(test)]
    let before = observed.clone();
    let aligned = aligned_beside_run(reference, observed, run, observed_holds_run);
    #[cfg(test)]
    if aligned {
        paired_as_the_matching_pairs(reference, before, observed);
    }
    aligned
}

fn aligned_beside_run(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
    run: std::ops::Range<usize>,
    observed_holds_run: bool,
) -> bool {
    let placed = Placed {
        start: run.start,
        len: run.len(),
    };
    let (longer, shorter) = if observed_holds_run {
        (&*observed, reference)
    } else {
        (reference, &*observed)
    };
    if run.end > longer.instances.len()
        || shorter.instances.len() + run.len() != longer.instances.len()
        || matches!(
            (
                workspace_current_camera_index(reference),
                workspace_current_camera_index(observed),
            ),
            (Some(reference), Some(observed))
                if placed.counterpart(observed, observed_holds_run) != Some(reference)
        )
        || ids_paired_elsewhere(reference, observed, placed, observed_holds_run)
        || twins_paired_otherwise(reference, observed, placed, observed_holds_run)
    {
        return false;
    }
    if !observed_holds_run {
        // An identity of the store's own that the other copy holds as well
        // is no instance this copy alone holds.
        let held = observed
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<AHashSet<_>>();
        return !reference.instances[run].iter().any(|instance| {
            !instance.settings_id.starts_with("debug:")
                && held.contains(instance.settings_id.as_str())
        }) && align_settings_ids_placed(reference, observed, placed);
    }
    let service = store_service(observed).map(str::to_string);
    // The id of an instance of the run that the reference holds for another
    // instance is renamed while every reference to it still says the old one.
    let renamed = {
        let reserved = reference
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<AHashSet<_>>();
        let mut colliding = run
            .clone()
            .filter(|index| reserved.contains(observed.instances[*index].settings_id.as_str()));
        let first = colliding.next();
        // Studio numbers its instances anew when it opens a place: only an
        // id of that kind is another instance's by chance.
        if colliding.next().is_some()
            || first
                .is_some_and(|index| !observed.instances[index].settings_id.starts_with("debug:"))
        {
            return false;
        }
        first.map(|index| {
            let held = observed
                .instances
                .iter()
                .map(|instance| instance.settings_id.as_str())
                .collect::<AHashSet<_>>();
            let fresh = (0_usize..)
                .map(|generated| format!("reconcile:{generated:x}"))
                .find(|candidate| {
                    !reserved.contains(candidate.as_str()) && !held.contains(candidate.as_str())
                })
                .unwrap_or_default();
            (index, fresh)
        })
    };
    let rename = |observed: &mut SettingsBytecode, index: usize, id: String| {
        let old = std::mem::replace(&mut observed.instances[index].settings_id, id.clone());
        let remap = HashMap::from([(old.clone(), id)]);
        for instance in &mut observed.instances {
            remap_record_reference_ids(&mut instance.properties, &remap, service.as_deref());
            remap_record_reference_ids(&mut instance.attributes, &remap, service.as_deref());
        }
        old
    };
    let renamed = renamed.map(|(index, fresh)| (index, rename(observed, index, fresh)));
    let Some(mut taken) = take_run(observed, &run) else {
        if let Some((index, old)) = renamed {
            rename(observed, index, old);
        }
        return false;
    };
    let before = observed
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    let aligned = align_settings_ids_by_place(reference, observed);
    if aligned {
        let remap = before
            .into_iter()
            .zip(&observed.instances)
            .filter(|(before, instance)| *before != instance.settings_id)
            .map(|(before, instance)| (before, instance.settings_id.clone()))
            .collect::<HashMap<_, _>>();
        if !remap.is_empty() {
            for instance in &mut taken {
                remap_record_reference_ids(&mut instance.properties, &remap, service.as_deref());
                remap_record_reference_ids(&mut instance.attributes, &remap, service.as_deref());
            }
        }
    }
    put_run(observed, &run, taken);
    if !aligned && let Some((index, old)) = renamed {
        rename(observed, index, old);
    }
    aligned
}

/// The most comparisons of what twins hold the alignment beside a run makes;
/// past that it leaves the copies to the matching.
const TWINS_WEIGHED: usize = 1 << 16;

/// Whether the matching pairs an instance beside the run by its id with
/// another than its counterpart: it takes every id the reference holds that
/// is not Studio's own (`IdentityAssignment::run_exact`), a bare number too.
fn ids_paired_elsewhere(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    placed: Placed,
    observed_holds_run: bool,
) -> bool {
    let mut held = None;
    (observed.instances.iter().enumerate()).any(|(index, instance)| {
        let id = instance.settings_id.as_str();
        !id.starts_with("debug:")
            && placed
                .counterpart(index, observed_holds_run)
                .is_some_and(|at| reference.instances[at].settings_id != id)
            && held
                .get_or_insert_with(|| {
                    (reference.instances.iter())
                        .map(|instance| instance.settings_id.as_str())
                        .collect::<AHashSet<_>>()
                })
                .contains(id)
    })
}

/// Whether the matching (`match_settings_ids`) may pair some twins,
/// namesakes under one parent, otherwise than the places of `placed` do
/// (the run the observed copy's with `observed_holds_run`): an alignment by
/// those places stands for the matching only where this is false. The
/// matching pairs the viewport cameras and the instances an id or a
/// UniqueId tells first, at their places where `align_settings_ids_beside_run`
/// goes on. Twins the run leaves as they stood, down to their descendants,
/// it takes for their counterparts whatever else holds the same, once each
/// instance beside the run holds the references its counterpart does at
/// their places, as `positional_identity_preserved` requires before the
/// alignment by places goes on; that check refuses namesakes beside the run
/// that changed, and so does this one. That check leaves out what the
/// viewport cameras name, which the matching weighs, so cameras that name
/// otherwise than the places do leave the copies to the matching. Of a group
/// the run touches, with a twin or under one, it pairs a twin that holds
/// what its counterpart holds, and no other twin of the other copy does, in
/// its first round, where the twin holds and receives the references its
/// counterpart does at their places; twins that hold what none of those
/// holds, and hold and receive no reference, it leaves to `run_fallback`,
/// which pairs them by their shapes in their order. A twin left unread says
/// nothing of what it holds, so a group the run touches with one is left to
/// the matching.
fn twins_paired_otherwise(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    placed: Placed,
    observed_holds_run: bool,
) -> bool {
    let (longer, shorter) = if observed_holds_run {
        (observed, reference)
    } else {
        (reference, observed)
    };
    fn key(
        instance: &SettingsBytecodeInstance,
        parent: Option<usize>,
    ) -> (Option<usize>, &str, &str) {
        (parent, instance.name.as_str(), instance.class_name.as_str())
    }
    let mut counts = AHashMap::with_capacity(longer.instances.len());
    for instance in &longer.instances {
        *counts
            .entry(key(instance, instance.parent_index))
            .or_insert(0_usize) += 1;
    }
    let mut groups = AHashMap::<_, (Vec<usize>, Vec<usize>)>::new();
    for (index, instance) in longer.instances.iter().enumerate() {
        let key = key(instance, instance.parent_index);
        if counts[&key] > 1 && (instance.parent_index).is_none_or(|parent| !placed.passes(parent)) {
            groups.entry(key).or_default().0.push(index);
        }
    }
    if groups.is_empty() {
        return false;
    }
    for (index, instance) in shorter.instances.iter().enumerate() {
        let parent = instance.parent_index.map(|parent| placed.at(parent));
        if let Some((_, twins)) = groups.get_mut(&key(instance, parent)) {
            twins.push(index);
        }
    }
    let twins = Twins::new(reference, observed, placed, observed_holds_run);
    !twins.cameras_name_alike()
        || groups.into_values().any(|(longer_twins, shorter_twins)| {
            let (observed_twins, reference_twins) = if observed_holds_run {
                (longer_twins, shorter_twins)
            } else {
                (shorter_twins, longer_twins)
            };
            twins.paired_otherwise(observed_twins, reference_twins)
        })
}

/// The copies `twins_paired_otherwise` weighs, with what it reads of them
/// once a group of twins asks for it, each of the reference and then of the
/// observed copy.
struct Twins<'a> {
    reference: &'a SettingsBytecode,
    observed: &'a SettingsBytecode,
    placed: Placed,
    observed_holds_run: bool,
    cameras: Option<(usize, usize)>,
    /// The instances beside the run that hold one of its instances, at any
    /// depth, in the copy that holds it.
    run_holders: AHashSet<usize>,
    /// The comparisons of what twins hold made so far (`TWINS_WEIGHED`).
    weighed: std::cell::Cell<usize>,
    unique_ids: std::cell::OnceCell<[AHashMap<&'a str, Option<usize>>; 2]>,
    graphs: std::cell::OnceCell<[ReferenceGraph; 2]>,
    shapes: std::cell::OnceCell<[Vec<u64>; 2]>,
    children: std::cell::OnceCell<Vec<Vec<usize>>>,
}

impl<'a> Twins<'a> {
    fn new(
        reference: &'a SettingsBytecode,
        observed: &'a SettingsBytecode,
        placed: Placed,
        observed_holds_run: bool,
    ) -> Self {
        let longer = if observed_holds_run {
            observed
        } else {
            reference
        };
        let mut run_holders = AHashSet::new();
        for index in placed.start..placed.start + placed.len {
            let mut parent = longer.instances[index].parent_index;
            while let Some(at) = parent.filter(|at| !placed.passes(*at)) {
                if !run_holders.insert(at) {
                    break;
                }
                parent = (longer.instances.get(at)).and_then(|holder| holder.parent_index);
            }
        }
        Twins {
            reference,
            observed,
            placed,
            observed_holds_run,
            cameras: workspace_current_camera_index(reference)
                .zip(workspace_current_camera_index(observed)),
            run_holders,
            weighed: std::cell::Cell::new(0),
            unique_ids: std::cell::OnceCell::new(),
            graphs: std::cell::OnceCell::new(),
            shapes: std::cell::OnceCell::new(),
            children: std::cell::OnceCell::new(),
        }
    }

    fn counterpart(&self, index: usize) -> Option<usize> {
        self.placed.counterpart(index, self.observed_holds_run)
    }

    /// The observed instance a reference instance stands for.
    fn observed_of(&self, index: usize) -> Option<usize> {
        if self.observed_holds_run {
            Some(self.placed.at(index))
        } else if self.placed.passes(index) {
            None
        } else if index < self.placed.start {
            Some(index)
        } else {
            Some(index - self.placed.len)
        }
    }

    /// Whether the matching pairs an observed twin with its counterpart
    /// before the others: the viewport camera, and one whose id, or whose
    /// UniqueId each copy holds once, its counterpart holds as well.
    fn paired_first(&self, index: usize) -> bool {
        let Some(at) = self.counterpart(index) else {
            return false;
        };
        let (held, other) = (
            &self.observed.instances[index],
            &self.reference.instances[at],
        );
        if self.cameras == Some((at, index))
            || !held.settings_id.starts_with("debug:") && held.settings_id == other.settings_id
        {
            return true;
        }
        let (Some(id), Some(other_id)) = (persistent_identity(held), persistent_identity(other))
        else {
            return false;
        };
        let [of_reference, of_observed] = self.unique_ids.get_or_init(|| {
            [
                persistent_identity_index(self.reference),
                persistent_identity_index(self.observed),
            ]
        });
        id == other_id
            && of_reference.get(id) == Some(&Some(at))
            && of_observed.get(id) == Some(&Some(index))
    }

    /// Whether the matching may pair a group of twins otherwise than their
    /// places, the observed ones and the reference's each in their order.
    fn paired_otherwise(
        &self,
        mut observed_twins: Vec<usize>,
        mut reference_twins: Vec<usize>,
    ) -> bool {
        let mut taken = Vec::new();
        observed_twins.retain(|index| {
            let first = self.paired_first(*index);
            if first {
                taken.extend(self.counterpart(*index));
            }
            !first
        });
        if observed_twins.is_empty() {
            return false;
        }
        let mut taken = taken.into_iter().peekable();
        reference_twins.retain(|index| taken.next_if_eq(index).is_none());
        let longer_twins = if self.observed_holds_run {
            &observed_twins
        } else {
            &reference_twins
        };
        let touched = (observed_twins.iter()).any(|index| self.counterpart(*index).is_none())
            || !self.observed_holds_run
                && (reference_twins.iter()).any(|index| self.placed.passes(*index))
            || (longer_twins.iter()).any(|index| self.run_holders.contains(index));
        if !touched {
            return !(observed_twins.iter()).all(|index| self.unchanged(*index));
        }
        let stands_in = |document: &SettingsBytecode, index: &usize| {
            crate::settings::held::stands_unchanged(&document.instances[*index].properties)
        };
        if (observed_twins.iter()).any(|index| stands_in(self.observed, index))
            || (reference_twins.iter()).any(|index| stands_in(self.reference, index))
        {
            return true;
        }
        let beside = (reference_twins.iter())
            .filter(|index| self.observed_of(**index).is_some())
            .count();
        if beside > 1
            && (observed_twins.iter())
                .any(|index| self.counterpart(*index).is_some() && !self.unchanged(*index))
        {
            return true;
        }
        self.told_otherwise(&observed_twins, &reference_twins)
    }

    fn graphs(&self) -> &[ReferenceGraph; 2] {
        self.graphs.get_or_init(|| {
            let (reference, observed) = rayon::join(
                || build_reference_graph(self.reference),
                || build_reference_graph(self.observed),
            );
            [reference, observed]
        })
    }

    /// Whether an observed twin holds a reference or another instance names
    /// it by its id: either weighs in the matching
    /// (`matching_reference_evidence`).
    fn named(&self, index: usize) -> bool {
        let [_, observed] = self.graphs();
        !observed.outgoing[index].is_empty() || !observed.incoming[index].is_empty()
    }

    /// Whether an observed twin holds and receives the references its
    /// counterpart `at` does, at their places: whatever the matching paired
    /// at their places before, it then weighs that counterpart for the twin
    /// (`matching_reference_evidence`). A reference from an instance of the
    /// run weighs nothing there; one to an instance of the run may.
    fn named_alike(&self, index: usize, at: usize) -> bool {
        let [reference, observed] = self.graphs();
        self.name_alike(&reference.outgoing[at], &observed.outgoing[index])
            && observed.incoming[index].iter().all(|edge| {
                self.counterpart(edge.source).is_none_or(|source| {
                    (reference.incoming[at].iter())
                        .any(|other| other.source == source && other.slot == edge.slot)
                })
            })
            && reference.incoming[at].iter().all(|edge| {
                self.observed_of(edge.source).is_none_or(|source| {
                    (observed.incoming[index].iter())
                        .any(|other| other.source == source && other.slot == edge.slot)
                })
            })
    }

    /// Whether the references of a reference instance, `held`, and of an
    /// observed one, `edges`, name slot for slot instances that stand at
    /// each other's places, or the same beyond the copies.
    fn name_alike(&self, held: &[ReferenceEdge], edges: &[ReferenceEdge]) -> bool {
        edges.len() == held.len()
            && edges.iter().all(|edge| {
                let other = held.iter().find(|other| other.slot == edge.slot);
                other.is_some_and(|other| match (&other.target, &edge.target) {
                    (ReferenceTarget::Internal(other), ReferenceTarget::Internal(target)) => {
                        self.counterpart(*target) == Some(*other)
                    }
                    (ReferenceTarget::External(other), ReferenceTarget::External(value)) => {
                        reconciliation_values_equal(other, value, false)
                    }
                    _ => false,
                })
            })
    }

    /// Whether the viewport cameras name what stands at each other's places
    /// (`name_alike`): the matching pairs them first and weighs what they
    /// name for the instances they name (`matching_reference_evidence`),
    /// which the comparison by places leaves out
    /// (`positional_values_equivalent`).
    fn cameras_name_alike(&self) -> bool {
        let Some((reference_camera, observed_camera)) = self.cameras else {
            return true;
        };
        let named = |document: &SettingsBytecode, camera: usize| {
            let camera = &document.instances[camera];
            let mut ids = AHashSet::new();
            collect_text_reference_ids(&camera.properties, &mut ids);
            collect_text_reference_ids(&camera.attributes, &mut ids);
            let by_id = if ids.is_empty() {
                HashMap::new()
            } else {
                (document.instances.iter().enumerate())
                    .filter(|(_, instance)| ids.contains(&instance.settings_id))
                    .map(|(index, instance)| (instance.settings_id.as_str(), index))
                    .collect()
            };
            let mut edges = Vec::new();
            collect_map_references(&camera.properties, "properties", &by_id, &mut edges);
            collect_map_references(&camera.attributes, "attributes", &by_id, &mut edges);
            edges
        };
        let held = named(self.reference, reference_camera);
        self.name_alike(&held, &named(self.observed, observed_camera))
    }

    /// Whether an observed twin and each of its descendants beside the run
    /// hold, value for value, what the matching keys their counterparts by
    /// (`identity_subtree_keys`). An instance that stands for its
    /// counterpart unread holds what that one holds.
    fn unchanged(&self, index: usize) -> bool {
        let children = self.children.get_or_init(|| {
            let mut children = vec![Vec::new(); self.observed.instances.len()];
            for (index, instance) in self.observed.instances.iter().enumerate() {
                if let Some(children) = instance.parent_index.and_then(|at| children.get_mut(at)) {
                    children.push(index);
                }
            }
            children
        });
        let mut pending = vec![index];
        let mut seen = 0;
        while let Some(index) = pending.pop() {
            seen += 1;
            if seen > children.len() {
                return false;
            }
            let Some(at) = self.counterpart(index) else {
                continue;
            };
            if !holds_exactly(
                &self.reference.instances[at],
                &self.observed.instances[index],
            ) {
                return false;
            }
            pending.extend(&children[index]);
        }
        true
    }

    /// Whether the matching may pair a group of twins otherwise where each
    /// observed twin holds what its counterpart holds and no other twin of
    /// the reference does, which pairs it first, or what none of them does
    /// and no reference names, which leaves it to `run_fallback`.
    fn told_otherwise(&self, observed_twins: &[usize], reference_twins: &[usize]) -> bool {
        let content_index = IdentityCandidateIndex::build(self.reference, reference_twins);
        let narrowed = (observed_twins.iter())
            .map(|index| {
                (content_index.as_ref())
                    .and_then(|lookup| lookup.matching(&self.observed.instances[*index]))
            })
            .collect::<Vec<_>>();
        let count_of = |narrowed: Option<&[_]>| narrowed.map_or(reference_twins.len(), <[_]>::len);
        let weighed: usize = narrowed.iter().map(|each| count_of(*each)).sum();
        self.weighed.set(self.weighed.get() + weighed);
        if self.weighed.get() > TWINS_WEIGHED {
            return true;
        }
        let mut told = AHashSet::new();
        let mut untold = Vec::new();
        for (&index, narrowed) in observed_twins.iter().zip(narrowed) {
            let instance = &self.observed.instances[index];
            let at = self.counterpart(index);
            let count = count_of(narrowed);
            let mut holds_counterpart = false;
            for offset in 0..count {
                let candidate =
                    narrowed.map_or_else(|| reference_twins[offset], |values| values[offset].1);
                if holds_same_content(&self.reference.instances[candidate], instance) {
                    if Some(candidate) != at {
                        return true;
                    }
                    holds_counterpart = true;
                }
            }
            match at {
                Some(at) if holds_counterpart => {
                    if !self.named_alike(index, at) {
                        return true;
                    }
                    told.insert(at);
                }
                _ if self.named(index) => return true,
                _ => untold.push(index),
            }
        }
        if untold.is_empty() {
            return false;
        }
        let left = (reference_twins.iter())
            .copied()
            .filter(|index| !told.contains(index))
            .collect::<Vec<_>>();
        self.left_otherwise(&untold, &left)
    }

    /// Whether `run_fallback` pairs the observed twins `untold` otherwise
    /// than their places among the reference twins `left`: by their shapes,
    /// each in their order, and the rest in order.
    fn left_otherwise(&self, untold: &[usize], left: &[usize]) -> bool {
        let [reference_shapes, observed_shapes] = self.shapes.get_or_init(|| {
            let (reference, observed) = rayon::join(
                || subtree_shape_keys(self.reference),
                || subtree_shape_keys(self.observed),
            );
            [reference, observed]
        });
        let mut by_shape = AHashMap::<u64, VecDeque<usize>>::new();
        for (position, candidate) in left.iter().enumerate() {
            (by_shape.entry(reference_shapes[*candidate]))
                .or_default()
                .push_back(position);
        }
        let mut taken = vec![false; left.len()];
        let mut unpaired = Vec::new();
        for &index in untold {
            match (by_shape.get_mut(&observed_shapes[index])).and_then(VecDeque::pop_front) {
                Some(position) => {
                    taken[position] = true;
                    if self.counterpart(index) != Some(left[position]) {
                        return true;
                    }
                }
                None => unpaired.push(index),
            }
        }
        let mut leftovers = (left.iter().zip(&taken))
            .filter(|(_, taken)| !**taken)
            .map(|(candidate, _)| *candidate);
        (unpaired.into_iter()).any(|index| leftovers.next() != self.counterpart(index))
    }
}

/// Whether two instances hold, exactly, the values the matching keys an
/// instance by (`identity_subtree_keys`). One that stands for its
/// counterpart unread holds what that one holds.
pub(crate) fn holds_exactly(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
) -> bool {
    fn stable(
        values: &Map<String, Value>,
        properties: bool,
    ) -> impl Iterator<Item = (&String, &Value)> {
        (values.iter())
            .filter(move |(name, value)| identity_value_is_stable(name, value, properties))
    }
    crate::settings::held::stands_unchanged(&reference.properties)
        || crate::settings::held::stands_unchanged(&observed.properties)
        || stable(&reference.properties, true).eq(stable(&observed.properties, true))
            && stable(&reference.attributes, false).eq(stable(&observed.attributes, false))
}

/// Panics where the matching pairs an observed instance with another
/// reference instance than an alignment beside a run did: that alignment
/// stands for the matching. Copies with instances left unread are the
/// matching's only once those are read, so they are not weighed here.
#[cfg(test)]
fn paired_as_the_matching_pairs(
    reference: &SettingsBytecode,
    matched: SettingsBytecode,
    aligned: &SettingsBytecode,
) {
    if holds_unread_instances(reference) || holds_unread_instances(&matched) {
        return;
    }
    paired_as_the_whole_matching_pairs(reference, matched, reference, aligned);
}

/// Panics where the matching of two whole copies, `whole_reference` and
/// `whole_observed`, pairs an observed instance with another reference
/// instance than an alignment beside a run of the same copies read in part,
/// `reference` and `aligned`, did. Each instance stands at the same place in
/// a copy read whole and in part.
#[cfg(test)]
pub(crate) fn paired_as_the_whole_matching_pairs(
    whole_reference: &SettingsBytecode,
    mut whole_observed: SettingsBytecode,
    reference: &SettingsBytecode,
    aligned: &SettingsBytecode,
) {
    assert!(
        align_settings_ids_by_matching(whole_reference, &mut whole_observed),
        "the matching aligns no ids"
    );
    let pairs = |reference: &SettingsBytecode, document: &SettingsBytecode| {
        let ids = (reference.instances.iter().enumerate())
            .map(|(index, instance)| (instance.settings_id.as_str(), index))
            .collect::<AHashMap<_, _>>();
        (document.instances.iter())
            .map(|instance| ids.get(instance.settings_id.as_str()).copied())
            .collect::<Vec<_>>()
    };
    let (by_run, by_matching) = (
        pairs(reference, aligned),
        pairs(whole_reference, &whole_observed),
    );
    let count = by_run.len().max(by_matching.len());
    if let Some(index) = (0..count).find(|index| by_run.get(*index) != by_matching.get(*index)) {
        let instance = (whole_observed.instances.get(index))
            .map(|instance| (instance.class_name.as_str(), instance.name.as_str()));
        panic!(
            "beside a run {instance:?} ({index}) takes {:?}, in the matching {:?}",
            by_run.get(index),
            by_matching.get(index)
        );
    }
}

fn align_settings_ids_by_matching(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> bool {
    let cameras = (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    );
    let camera_positions_match = match cameras {
        (Some(reference), Some(observed)) => reference == observed,
        _ => true,
    };
    let positional_topology_matches =
        camera_positions_match && settings_topology_matches(reference, observed);
    if positional_topology_matches
        && positional_identity_preserved(reference, observed, Placed::default())
    {
        remap_positional_ids(reference, observed, Placed::default());
        return true;
    }
    let Some((ids, ..)) = match_settings_ids(reference, observed, false, false) else {
        return false;
    };
    take_settings_ids(observed, ids);
    true
}

/// The id `align_settings_ids_to_reference` gives each observed instance of
/// a store whose instances stand at other places than the reference's, and
/// the reference instance it takes each for, where an id, a UniqueId or the
/// place under its parent tells every one of them by itself. `None` where
/// the alignment compares what two instances hold: that takes the whole
/// documents. Nothing an instance holds but its UniqueId, and the camera a
/// root names, is read here.
pub(crate) fn settings_ids_told_alone(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> Option<(Vec<String>, Vec<Option<usize>>)> {
    match_settings_ids(reference, observed, true, false).map(|(ids, places, _)| (ids, places))
}

/// `settings_ids_told_alone` where namesakes under one parent that nothing
/// else tells apart are paired by their order, and with it the observed
/// instances taken so. Such a pair is one instance only where both hold the
/// same, which is the caller's to see.
#[allow(clippy::type_complexity)]
pub(crate) fn settings_ids_told_in_order(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> Option<(Vec<String>, Vec<Option<usize>>, Vec<(usize, ByOrder)>)> {
    match_settings_ids(reference, observed, true, true)
}

/// Gives the observed instances the ids a matching assigned them, in every
/// reference among them as well.
pub(crate) fn take_settings_ids(observed: &mut SettingsBytecode, ids: Vec<String>) {
    let phase = Instant::now();
    let remap = observed
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .zip(ids.iter().cloned())
        .collect::<HashMap<_, _>>();
    let service = store_service(observed).map(str::to_string);
    for (instance, id) in observed.instances.iter_mut().zip(ids) {
        instance.settings_id = id;
        remap_record_reference_ids(&mut instance.properties, &remap, service.as_deref());
        remap_record_reference_ids(&mut instance.attributes, &remap, service.as_deref());
    }
    log_global(
        4,
        format_args!(
            "[renium] identity reference remap: {:.1}ms",
            phase.elapsed().as_secs_f64() * 1000.0
        ),
    );
}

/// The matching of `align_settings_ids_by_matching`: for each observed
/// instance its id and the reference instance it is. With `told_alone` it
/// stops where two instances are told apart by what they hold, or with
/// `in_order` pairs those by their order and says which.
#[allow(clippy::type_complexity)]
fn match_settings_ids(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    told_alone: bool,
    in_order: bool,
) -> Option<(Vec<String>, Vec<Option<usize>>, Vec<(usize, ByOrder)>)> {
    let preparation_started = Instant::now();
    let cameras = (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    );
    // A fresh destination has only its retained containers. Index candidates
    // it can actually match rather than allocating a group for every incoming
    // descendant. Full comparisons keep the direct index construction.
    let candidate_filters = (observed.instances.len() < reference.instances.len() / 4).then(|| {
        (
            observed
                .instances
                .iter()
                .map(|instance| instance.settings_id.as_str())
                .collect::<HashSet<_>>(),
            observed
                .instances
                .iter()
                .map(|instance| (instance.name.as_str(), instance.class_name.as_str()))
                .collect::<HashSet<_>>(),
        )
    });
    let lookup = IdentityLookup {
        reference_by_id: reference
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| {
                candidate_filters
                    .as_ref()
                    .is_none_or(|(ids, _)| ids.contains(instance.settings_id.as_str()))
            })
            .map(|(index, instance)| (instance.settings_id.as_str(), index))
            .collect(),
        reference_by_unique_id: persistent_identity_index(reference),
        observed_by_unique_id: persistent_identity_index(observed),
    };
    let mut reference_groups = HashMap::<IdentityGroupKey, Vec<usize>>::new();
    for (index, instance) in reference.instances.iter().enumerate() {
        if candidate_filters.as_ref().is_some_and(|(_, names)| {
            !names.contains(&(instance.name.as_str(), instance.class_name.as_str()))
        }) {
            continue;
        }
        reference_groups
            .entry((
                instance.parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            ))
            .or_default()
            .push(index);
    }
    let mut state = IdentityAssignment::new(reference, observed);
    state.by_order = in_order.then(Vec::new);
    if let (Some(reference_index), Some(observed_index)) = cameras {
        state.assign(observed_index, reference_index);
    }
    state.run_exact(&lookup);
    state.run_unique(&reference_groups);
    log_global(
        4,
        format_args!(
            "[renium] identity preparation: {:.1}ms remaining={}",
            preparation_started.elapsed().as_secs_f64() * 1000.0,
            state.remaining
        ),
    );

    if state.remaining > 0 {
        // Collision sets are needed only when matching must synthesize IDs.
        // An empty destination normally resolves all retained roots directly;
        // don't clone every incoming ID twice for that completed mapping.
        state.reserved_reference_ids = reference
            .instances
            .iter()
            .map(|instance| instance.settings_id.clone())
            .collect();
        state.blocked_ids = state
            .reserved_reference_ids
            .iter()
            .chain(&state.old_ids)
            .cloned()
            .collect();
        let phase = Instant::now();
        let (reference_graph, observed_graph) = rayon::join(
            || build_reference_graph(reference),
            || build_reference_graph(observed),
        );
        log_global(
            4,
            format_args!(
                "[renium] identity reference graphs: {:.1}ms",
                phase.elapsed().as_secs_f64() * 1000.0
            ),
        );
        let phase = Instant::now();
        let parents = reference
            .instances
            .iter()
            .filter_map(|instance| instance.parent_index)
            .collect::<AHashSet<_>>();
        let subtree_keys = (!told_alone
            && reference_groups
                .values()
                .any(|group| group.len() > 1 && group.iter().any(|index| parents.contains(index))))
        .then(|| {
            let mut keys = AHashMap::new();
            (
                identity_subtree_keys(
                    reference,
                    &mut keys,
                    state.used_reference.iter().map(|used| !used).collect(),
                ),
                identity_subtree_keys(
                    observed,
                    &mut keys,
                    state.assigned_ids.iter().map(Option::is_none).collect(),
                ),
            )
        });
        log_global(
            4,
            format_args!(
                "[renium] identity subtree keys: {:.1}ms",
                phase.elapsed().as_secs_f64() * 1000.0
            ),
        );
        let phase = Instant::now();
        let inputs = ScoreInputs {
            reference_groups: &reference_groups,
            reference_graph: &reference_graph,
            observed_graph: &observed_graph,
            subtree_keys: subtree_keys.as_ref(),
            told_alone,
        };
        let mut unchanged_content_groups = HashMap::new();
        while state.remaining > 0 {
            let mut progressed = state.run_exact(&lookup);
            progressed |= state.run_scored(&inputs, &mut unchanged_content_groups);
            if state.compares_values {
                return None;
            }
            if progressed || !told_alone && state.run_fallback(&reference_groups) {
                continue;
            }
            return None;
        }
        log_global(
            4,
            format_args!(
                "[renium] identity matching passes: {:.1}ms",
                phase.elapsed().as_secs_f64() * 1000.0
            ),
        );
    }
    let IdentityAssignment {
        assigned_ids,
        assigned_reference,
        by_order,
        ..
    } = state;
    Some((
        assigned_ids.into_iter().map(Option::unwrap).collect(),
        assigned_reference,
        by_order.unwrap_or_default(),
    ))
}

/// Where the counterpart of each observed instance stands among the
/// reference's: at the same place, or past a run of `len` instances at
/// `start` that the reference alone holds.
#[derive(Clone, Copy, Default)]
struct Placed {
    start: usize,
    len: usize,
}

impl Placed {
    fn at(self, index: usize) -> usize {
        if index < self.start {
            index
        } else {
            index + self.len
        }
    }

    /// Whether an instance of the copy that holds the run is one of it.
    fn passes(self, index: usize) -> bool {
        (self.start..self.start + self.len).contains(&index)
    }

    /// The reference instance an observed one stands for, where the run is
    /// the reference's or, with `observed_holds_run`, the observed copy's:
    /// none for an instance of the run.
    fn counterpart(self, index: usize, observed_holds_run: bool) -> Option<usize> {
        if !observed_holds_run {
            Some(self.at(index))
        } else if self.passes(index) {
            None
        } else if index < self.start {
            Some(index)
        } else {
            Some(index - self.len)
        }
    }

    /// The reference's viewport camera, where it stands beside the run.
    fn camera(self, reference: &SettingsBytecode) -> Option<usize> {
        workspace_current_camera_index(reference).filter(|index| !self.passes(*index))
    }
}

fn remap_positional_ids(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
    placed: Placed,
) {
    let counterpart = |index: usize| &reference.instances[placed.at(index)];
    let remap = (observed.instances.iter().enumerate())
        .map(|(index, observed)| {
            let id = counterpart(index).settings_id.clone();
            (observed.settings_id.clone(), id)
        })
        .collect::<HashMap<_, _>>();
    let service = store_service(observed).map(str::to_string);
    let rename = |(index, observed): (usize, &mut SettingsBytecodeInstance)| {
        observed
            .settings_id
            .clone_from(&counterpart(index).settings_id);
        remap_record_reference_ids(&mut observed.properties, &remap, service.as_deref());
        remap_record_reference_ids(&mut observed.attributes, &remap, service.as_deref());
    };
    // Every record is walked for references: seconds on a large store when
    // one thread does it.
    if observed.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        observed
            .instances
            .par_iter_mut()
            .enumerate()
            .for_each(rename);
    } else {
        observed.instances.iter_mut().enumerate().for_each(rename);
    }
}

pub(crate) fn settings_documents_positionally_equivalent(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> bool {
    if !settings_topology_matches(reference, observed) {
        return false;
    }

    positional_documents_equivalent(reference, observed)
}

fn positional_documents_equivalent(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> bool {
    positional_values_equivalent(reference, observed, None, true, Placed::default())
}

fn positional_identity_preserved(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    placed: Placed,
) -> bool {
    let at = |index| Some(placed.at(index));
    if !persistent_identities_follow_mapping(reference, observed, at)
        || !store_identities_follow_mapping(reference, observed, at)
    {
        return false;
    }
    let beside =
        || (reference.instances.iter().enumerate()).filter(|(index, _)| !placed.passes(*index));
    let mut first_by_key = AHashMap::with_capacity(reference.instances.len());
    let mut ambiguous = vec![false; reference.instances.len()];
    for (index, instance) in beside() {
        let key = (
            instance.parent_index,
            instance.name.as_str(),
            instance.class_name.as_str(),
        );
        if let Some(first) = first_by_key.insert(key, index) {
            ambiguous[first] = true;
            ambiguous[index] = true;
        }
    }
    if !ambiguous.iter().any(|value| *value) {
        return true;
    }
    // Unchanged duplicate subtrees may retain their positional identities even
    // when an unrelated unique instance changed. Reference-bearing instances
    // outside those subtrees must still agree, including incoming references.
    for (index, instance) in beside() {
        if let Some(parent) = instance.parent_index {
            if parent >= index {
                return false;
            }
            ambiguous[index] |= ambiguous[parent];
        }
    }
    positional_values_equivalent(reference, observed, Some(&ambiguous), true, placed)
}

fn positional_values_equivalent(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    identity_checks: Option<&[bool]>,
    preserve_viewport: bool,
    placed: Placed,
) -> bool {
    if placed.camera(reference)
        != workspace_current_camera_index(observed).map(|index| placed.at(index))
    {
        return false;
    }
    let reference_ids = reference
        .instances
        .iter()
        .enumerate()
        .filter(|(index, _)| !placed.passes(*index))
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let observed_ids = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), placed.at(index)))
        .collect::<AHashMap<_, _>>();
    let ids = PairedStoreIds {
        service: store_service(reference),
        left: &reference_ids,
        right: &observed_ids,
    };
    let equivalent = |(observed_index, observed_instance): (usize, &SettingsBytecodeInstance)| {
        let index = placed.at(observed_index);
        let reference_instance = &reference.instances[index];
        // An instance that stands for its unchanged counterpart holds what
        // the counterpart holds.
        if crate::settings::held::stands_unchanged(&reference_instance.properties)
            || crate::settings::held::stands_unchanged(&observed_instance.properties)
        {
            return true;
        }
        if !preserve_viewport
            && reference_instance.properties.get("UniqueId")
                != observed_instance.properties.get("UniqueId")
        {
            return false;
        }
        if identity_checks.is_some_and(|checks| !checks[index])
            && !reference_instance
                .properties
                .values()
                .chain(reference_instance.attributes.values())
                .chain(observed_instance.properties.values())
                .chain(observed_instance.attributes.values())
                .any(reconciliation_value_contains_reference)
        {
            return true;
        }
        reference_instance.class_name == "PackageLink"
            || preserve_viewport
                && is_reconciliation_protected_workspace_camera(reference, index)
                && is_reconciliation_protected_workspace_camera(observed, observed_index)
            || reconciliation_maps_equal_with_ids(
                &reference_instance.class_name,
                &reference_instance.properties,
                &observed_instance.properties,
                ids,
            ) && reconciliation_values_map_equal_with_ids(
                &reference_instance.attributes,
                &observed_instance.attributes,
                ids,
            )
    };
    if observed.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        observed.instances.par_iter().enumerate().all(equivalent)
    } else {
        observed.instances.iter().enumerate().all(equivalent)
    }
}

enum ContiguousStructuralAlignment {
    NotApplicable,
    PreparedMismatch,
    /// With whether every observed instance kept the id it came with.
    Aligned {
        ids_kept: bool,
    },
}

#[derive(Clone, Copy)]
struct ContiguousIndexMap {
    changed_at: usize,
    changed_len: usize,
    added_to_observed: bool,
}

impl ContiguousIndexMap {
    fn observed_to_reference(self, index: usize) -> Option<usize> {
        if !self.added_to_observed {
            return Some(if index < self.changed_at {
                index
            } else {
                index + self.changed_len
            });
        }
        if index < self.changed_at {
            Some(index)
        } else if index < self.changed_at + self.changed_len {
            None
        } else {
            Some(index - self.changed_len)
        }
    }

    fn pair_at(self, position: usize) -> (usize, usize) {
        if position < self.changed_at {
            (position, position)
        } else if self.added_to_observed {
            (position, position + self.changed_len)
        } else {
            (position + self.changed_len, position)
        }
    }
}

fn contiguous_inserted_id_replacements(
    observed: &SettingsBytecode,
    index_map: ContiguousIndexMap,
    reserved_ids: &AHashSet<&str>,
) -> Vec<Option<String>> {
    let inserted_len = if index_map.added_to_observed {
        index_map.changed_len
    } else {
        0
    };
    let mut used = AHashSet::with_capacity(inserted_len);
    let mut replacements = Vec::with_capacity(inserted_len);
    let mut generated = 0usize;
    for instance in observed
        .instances
        .iter()
        .skip(index_map.changed_at)
        .take(inserted_len)
    {
        let replacement = if !reserved_ids.contains(instance.settings_id.as_str())
            && used.insert(instance.settings_id.clone())
        {
            None
        } else {
            loop {
                let candidate = format!("reconcile:{generated:x}");
                generated += 1;
                if !reserved_ids.contains(candidate.as_str()) && used.insert(candidate.clone()) {
                    break Some(candidate);
                }
            }
        };
        replacements.push(replacement);
    }
    replacements
}

fn document_text_reference_ids(document: &SettingsBytecode) -> AHashSet<String> {
    if document.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        return document
            .instances
            .par_iter()
            .fold(AHashSet::new, |mut ids, instance| {
                collect_text_reference_ids(&instance.properties, &mut ids);
                collect_text_reference_ids(&instance.attributes, &mut ids);
                ids
            })
            .reduce(AHashSet::new, |mut left, right| {
                left.extend(right);
                left
            });
    }
    let mut ids = AHashSet::new();
    for instance in &document.instances {
        collect_text_reference_ids(&instance.properties, &mut ids);
        collect_text_reference_ids(&instance.attributes, &mut ids);
    }
    ids
}

fn align_settings_ids_for_contiguous_structural_change(
    reference: &mut SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> ContiguousStructuralAlignment {
    if reference.instances.len() == observed.instances.len() {
        return ContiguousStructuralAlignment::NotApplicable;
    }
    let common_len = reference.instances.len().min(observed.instances.len());
    let added_to_observed = observed.instances.len() > reference.instances.len();
    let changed_len = reference.instances.len().abs_diff(observed.instances.len());
    let mut changed_at = 0usize;
    while changed_at < common_len {
        let reference = &reference.instances[changed_at];
        let observed = &observed.instances[changed_at];
        if reference.name != observed.name
            || reference.class_name != observed.class_name
            || reference.parent_index != observed.parent_index
        {
            break;
        }
        changed_at += 1;
    }

    let index_map = ContiguousIndexMap {
        changed_at,
        changed_len,
        added_to_observed,
    };
    if let (Some(reference_camera), Some(observed_camera)) = (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    ) && index_map.observed_to_reference(observed_camera) != Some(reference_camera)
    {
        return ContiguousStructuralAlignment::NotApplicable;
    }
    let topology_matches = (changed_at..common_len).all(|position| {
        let (reference_index, observed_index) = index_map.pair_at(position);
        let reference = &reference.instances[reference_index];
        let observed = &observed.instances[observed_index];
        let parent_matches = match observed.parent_index {
            None => reference.parent_index.is_none(),
            Some(parent) => index_map
                .observed_to_reference(parent)
                .is_some_and(|parent| reference.parent_index == Some(parent)),
        };
        reference.name == observed.name
            && reference.class_name == observed.class_name
            && parent_matches
    });
    if !topology_matches {
        if verbose_timing_logs() {
            println!(
                "[renium] contiguous settings alignment rejected: topology differs at or after index {changed_at}"
            );
        }
        return ContiguousStructuralAlignment::NotApplicable;
    }
    if !persistent_identities_follow_mapping(reference, observed, |index| {
        index_map.observed_to_reference(index)
    }) || !store_identities_follow_mapping(reference, observed, |index| {
        index_map.observed_to_reference(index)
    }) {
        return ContiguousStructuralAlignment::NotApplicable;
    }

    let reserved_ids = reference
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<AHashSet<_>>();
    if reserved_ids.len() != reference.instances.len() {
        return ContiguousStructuralAlignment::NotApplicable;
    }
    let mut inserted_replacements =
        contiguous_inserted_id_replacements(observed, index_map, &reserved_ids);

    let text_reference_ids = document_text_reference_ids(observed);
    let mut old_id_counts = AHashMap::<&str, usize>::with_capacity(text_reference_ids.len());
    for instance in &observed.instances {
        if text_reference_ids.contains(instance.settings_id.as_str()) {
            *old_id_counts
                .entry(instance.settings_id.as_str())
                .or_default() += 1;
        }
    }
    let mut remap = HashMap::with_capacity(text_reference_ids.len());
    for (index, instance) in observed.instances.iter().enumerate() {
        let current = instance.settings_id.as_str();
        if old_id_counts.get(current) != Some(&1) {
            continue;
        }
        let desired = if let Some(reference_index) = index_map.observed_to_reference(index) {
            reference.instances[reference_index].settings_id.as_str()
        } else {
            inserted_replacements[index - changed_at]
                .as_deref()
                .unwrap_or(current)
        };
        if current != desired {
            remap.insert(current.to_string(), desired.to_string());
        }
    }
    let mut ids_kept = true;
    for (index, instance) in observed.instances.iter_mut().enumerate() {
        if let Some(reference_index) = index_map.observed_to_reference(index) {
            let id = &reference.instances[reference_index].settings_id;
            ids_kept &= instance.settings_id == *id;
            instance.settings_id.clone_from(id);
        } else if let Some(replacement) = inserted_replacements[index - changed_at].take() {
            ids_kept = false;
            instance.settings_id = replacement;
        }
    }
    rayon::join(
        || stabilize_settings_reference_ids_with_remap(reference, None, true),
        || stabilize_settings_reference_ids_with_remap(observed, Some(&remap), true),
    );
    let reference_ids = reference
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let observed_ids = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| {
            (
                instance.settings_id.as_str(),
                index_map
                    .observed_to_reference(index)
                    .unwrap_or(reference.instances.len() + index),
            )
        })
        .collect::<AHashMap<_, _>>();
    let ids = PairedStoreIds {
        service: store_service(reference),
        left: &reference_ids,
        right: &observed_ids,
    };
    let equivalent = |position: usize| {
        let (reference_index, observed_index) = index_map.pair_at(position);
        let reference = &reference.instances[reference_index];
        let observed = &observed.instances[observed_index];
        reference.class_name == "PackageLink"
            || reconciliation_maps_equal_with_ids(
                &reference.class_name,
                &reference.properties,
                &observed.properties,
                ids,
            ) && reconciliation_values_map_equal_with_ids(
                &reference.attributes,
                &observed.attributes,
                ids,
            )
    };
    let common_matches = if common_len >= 2_048 && rayon::current_num_threads() > 1 {
        (0..common_len).into_par_iter().all(equivalent)
    } else {
        (0..common_len).all(equivalent)
    };
    if !common_matches {
        if verbose_timing_logs()
            && let Some(position) = (0..common_len).find(|position| !equivalent(*position))
        {
            let (reference_index, observed_index) = index_map.pair_at(position);
            let reference = &reference.instances[reference_index];
            let observed = &observed.instances[observed_index];
            println!(
                "[renium] contiguous settings alignment rejected: common content differs at reference index {reference_index} ({}/{}) and observed index {observed_index} ({}/{})",
                reference.class_name, reference.name, observed.class_name, observed.name
            );
        }
        return ContiguousStructuralAlignment::PreparedMismatch;
    }

    ContiguousStructuralAlignment::Aligned { ids_kept }
}

fn settings_parent_id(document: &SettingsBytecode, index: usize) -> Option<&str> {
    document.instances[index]
        .parent_index
        .and_then(|parent| document.instances.get(parent))
        .map(|parent| parent.settings_id.as_str())
}

// Reflection lists these file-only fields as properties of their own, so a
// record decoded from a model or place file carries them under the serialized
// name while Studio and pulls report the logical one.
pub(crate) fn serialized_only_logical_name(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
) -> Option<&'static str> {
    use crate::rbx::decode::rbx_reflection_class_is_a;
    match name {
        "WorldPivotData" if rbx_reflection_class_is_a(database, class_name, "Model") => {
            Some("WorldPivot")
        }
        "InitialSize" if rbx_reflection_class_is_a(database, class_name, "TriangleMeshPart") => {
            Some("MeshSize")
        }
        "FluidFidelityInternal"
            if rbx_reflection_class_is_a(database, class_name, "TriangleMeshPart") =>
        {
            Some("FluidFidelity")
        }
        _ => None,
    }
}

fn canonicalize_content_uri_alias(instance: &mut SettingsBytecodeInstance) -> Result<()> {
    let (modern, legacy) = match instance.class_name.as_str() {
        class if crate::roblox::schema::has_protected_texture_pack(class) => {
            ("TexturePackContent", "TexturePack")
        }
        "PartOperation" | "UnionOperation" | "NegateOperation" | "IntersectOperation" => {
            ("Content", "AssetId")
        }
        "Shirt" => ("ShirtTemplateContent", "ShirtTemplate"),
        "Pants" => ("PantsTemplateContent", "PantsTemplate"),
        _ => return Ok(()),
    };
    let Some(value) = instance
        .properties
        .get(modern)
        .map(typed_scalar)
        .filter(|value| value.is_string())
    else {
        return Ok(());
    };
    // Studio now serializes these URIs through Content, while older files
    // and protected setters use ContentId. Object-backed Content stays typed.
    // An empty mirror under either name is harmless; conflicting authored
    // URIs are not.
    if value.as_str() == Some("") && instance.properties.contains_key(legacy) {
        instance.properties.remove(modern);
        return Ok(());
    }
    if let Some(existing) = instance.properties.get(legacy)
        && typed_scalar(existing).as_str() != Some("")
        && !reconciliation_values_equal(existing, value, false)
    {
        bail!(
            "{} contains conflicting values for properties {legacy} and {modern}",
            instance.name
        );
    }
    let value = value.clone();
    instance.properties.remove(modern);
    instance.properties.insert(legacy.to_string(), value);
    Ok(())
}

/// Whether a document left instances in its store file: each stands there
/// with its UniqueId alone (`held::stand_unchanged`) until it is read.
pub(crate) fn holds_unread_instances(document: &SettingsBytecode) -> bool {
    crate::settings::tree::count_whole_store_pass();
    document
        .instances
        .iter()
        .any(|instance| crate::settings::held::stands_unchanged(&instance.properties))
}

const STORE_CHANGED: &str = "A store changed while its push was being prepared; retry the sync";

/// Reads every instance a document left in its store file: afterwards it is
/// the document a whole decode of that file gives, with the instances that
/// were read before as they stand.
pub(crate) fn read_unread_instances(document: &mut SettingsBytecode, store: &[u8]) -> Result<()> {
    if !holds_unread_instances(document) {
        return Ok(());
    }
    let mut whole = crate::settings::bytecode::decode_settings_bytecode_holding(store)?;
    stabilize_settings_reference_ids(&mut whole);
    canonicalize_settings_property_names(&mut whole)?;
    let same_places = whole.instances.len() == document.instances.len()
        && whole
            .instances
            .iter()
            .zip(&document.instances)
            .all(|(read, held)| {
                read.settings_id == held.settings_id
                    && read.name == held.name
                    && read.class_name == held.class_name
                    && read.parent_index == held.parent_index
            });
    anyhow::ensure!(same_places, STORE_CHANGED);
    for (instance, read) in document.instances.iter_mut().zip(whole.instances) {
        if crate::settings::held::stands_unchanged(&instance.properties) {
            *instance = read;
        }
    }
    Ok(())
}

/// The document of a store file with only some of its instances read: the
/// ones `document` has read, and the ones at `rows`, each as a whole decode
/// of the file gives it. Every other instance holds no value.
#[cfg(test)]
pub(crate) fn read_instances(
    document: &SettingsBytecode,
    store: &[u8],
    rows: &[usize],
) -> Result<SettingsBytecode> {
    let unread = |instance: &SettingsBytecodeInstance| {
        crate::settings::held::stands_unchanged(&instance.properties)
    };
    let rows = rows
        .iter()
        .copied()
        .filter(|row| document.instances.get(*row).is_some_and(unread))
        .collect::<Vec<_>>();
    let (read, ids) = crate::settings::bytecode::property_edit::decode_rows(store, &rows)?;
    anyhow::ensure!(
        ids.len() == document.instances.len()
            && ids
                .iter()
                .zip(&document.instances)
                .all(|(id, instance)| *id == instance.settings_id),
        STORE_CHANGED
    );
    let mut partly = SettingsBytecode {
        version: document.version,
        instances: document
            .instances
            .iter()
            .map(|instance| {
                if unread(instance) {
                    SettingsBytecodeInstance::new(
                        instance.settings_id.clone(),
                        instance.name.clone(),
                        instance.class_name.clone(),
                        instance.parent_index,
                    )
                } else {
                    instance.clone()
                }
            })
            .collect(),
    };
    for (index, mut row) in rows.iter().zip(read) {
        let held = &partly.instances[*index];
        anyhow::ensure!(
            row.name == held.name
                && row.class_name == held.class_name
                && row.parent_index == held.parent_index,
            STORE_CHANGED
        );
        for value in row.properties.values_mut() {
            *value = crate::settings::held::held(std::mem::take(value));
        }
        partly.instances[*index] = row;
    }
    for &index in &rows {
        stabilize_rows(&mut partly, index..index + 1, &ids);
        canonicalize_instances(std::slice::from_mut(&mut partly.instances[index]))?;
    }
    Ok(partly)
}

pub(crate) fn canonicalize_settings_property_names(document: &mut SettingsBytecode) -> Result<()> {
    // A dominant service must not serialize canonicalization after decoding.
    // Bounded chunks retain a small class/name cache without a shared lock.
    document
        .instances
        .par_chunks_mut(4096)
        .try_for_each(canonicalize_instances)
}

/// `canonicalize_settings_property_names` for some instances of a document.
pub(crate) fn canonicalize_instances(instances: &mut [SettingsBytecodeInstance]) -> Result<()> {
    let database = rbx_reflection_database::get()?;
    {
        {
            let mut names_by_class = AHashMap::<&str, AHashMap<String, Option<&str>>>::new();
            let mut migrations_by_class = AHashMap::<&str, AHashMap<String, bool>>::new();
            for instance in instances {
                canonicalize_content_uri_alias(instance)?;
                let names = names_by_class.entry(&instance.class_name).or_default();
                let mut renamed_property = |name: &str| {
                    if let Some(renamed) = names.get(name) {
                        return *renamed;
                    }
                    let renamed =
                        serialized_only_logical_name(database, &instance.class_name, name)
                            .or_else(|| {
                                rbx_logical_property_name(database, &instance.class_name, name)
                            })
                            .filter(|canonical| *canonical != name);
                    names.insert(name.to_string(), renamed);
                    renamed
                };
                let migrations = migrations_by_class.entry(&instance.class_name).or_default();
                let mut migrated_property = |name: &str| {
                    if let Some(migrated) = migrations.get(name) {
                        return *migrated;
                    }
                    let migrated =
                        legacy_property_migration(database, &instance.class_name, name).is_some();
                    migrations.insert(name.to_string(), migrated);
                    migrated
                };
                // Scripts serialize the legacy `Disabled` flag while Studio reports
                // `Enabled`; one fact, two spellings, kept as `Enabled`.
                let legacy_disabled = instance.properties.contains_key("Disabled")
                    && crate::rbx::decode::rbx_reflection_class_is_a(
                        database,
                        &instance.class_name,
                        "BaseScript",
                    );
                if !legacy_disabled
                    && !instance
                        .properties
                        .keys()
                        .any(|name| renamed_property(name).is_some() || migrated_property(name))
                {
                    continue;
                }
                let mut canonical = Map::new();
                for (name, value) in std::mem::take(&mut instance.properties) {
                    let name = renamed_property(&name).map_or(name, str::to_string);
                    if let Some(existing) = canonical.get(&name)
                        && !reconciliation_values_equal(existing, &value, false)
                    {
                        bail!(
                            "{} contains conflicting values for property {name}",
                            instance.name
                        );
                    }
                    canonical.insert(name, value);
                }
                let legacy = canonical
                    .keys()
                    .filter(|name| migrated_property(name))
                    .cloned()
                    .collect::<Vec<_>>();
                for name in legacy {
                    migrate_legacy_property(database, &instance.class_name, &name, &mut canonical);
                }
                if legacy_disabled
                    && let Some(disabled) = canonical.remove("Disabled")
                    && !canonical.contains_key("Enabled")
                    && let Some(flag) = disabled.as_bool()
                {
                    canonical.insert("Enabled".to_string(), Value::Bool(!flag));
                }
                instance.properties = canonical;
            }
            Ok(())
        }
    }
}

fn legacy_property_migration<'db>(
    database: &'db rbx_reflection::ReflectionDatabase<'db>,
    class_name: &str,
    name: &str,
) -> Option<(
    &'db rbx_reflection::PropertyDescriptor<'db>,
    &'db rbx_reflection::PropertyMigration<'db>,
)> {
    let descriptor = rbx_property_descriptor(database, class_name, name)?;
    match &descriptor.kind {
        rbx_reflection::PropertyKind::Canonical {
            serialization: rbx_reflection::PropertySerialization::Migrate(migration),
        } if descriptor.name == name => Some((descriptor, migration)),
        _ => None,
    }
}

// Studio serializes a migrated property under its replacement names only, so
// a document keeps the replacements it has and derives the missing ones from
// the legacy value; the legacy field itself never survives a round trip.
fn migrate_legacy_property(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
    properties: &mut Map<String, Value>,
) {
    let Some((descriptor, migration)) = legacy_property_migration(database, class_name, name)
    else {
        return;
    };
    let targets = migration.new_property_names();
    if targets
        .iter()
        .any(|target| properties.contains_key(*target))
    {
        properties.remove(name);
        return;
    }
    let Some(value) = properties.get(name) else {
        return;
    };
    let Some(variant) = json_to_rbx_property_variant(
        value,
        Some(descriptor),
        database,
        &BytecodeModelExportRefs::default(),
    ) else {
        return;
    };
    let variant = match variant {
        rbx_dom_weak::types::Variant::EnumItem(item) => {
            rbx_dom_weak::types::Variant::Enum(rbx_dom_weak::types::Enum::from_u32(item.value))
        }
        other => other,
    };
    let Ok(migrated) = migration.perform(&variant) else {
        return;
    };
    let mut replacements = Vec::with_capacity(targets.len());
    for target in targets {
        let replacement = if migrated.ty() == variant.ty() {
            value.clone()
        } else {
            let Some(json) = rbx_variant_to_settings_json(
                &migrated,
                rbx_property_descriptor(database, class_name, target),
                database,
                &BytecodeModelImportRefs::default(),
            ) else {
                return;
            };
            json
        };
        replacements.push(((*target).to_string(), replacement));
    }
    properties.remove(name);
    properties.extend(replacements);
}

pub(crate) fn settings_documents_equivalent(
    left: &SettingsBytecode,
    right: &SettingsBytecode,
) -> bool {
    settings_documents_match(left, right, true)
}

// Push protects the user's viewport; a pull must persist its observed values.
fn settings_documents_match(
    left: &SettingsBytecode,
    right: &SettingsBytecode,
    preserve_viewport: bool,
) -> bool {
    if left.instances.len() != right.instances.len() {
        return false;
    }
    let left_ids = left
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<AHashSet<_>>();
    let right_by_id = right
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    if left_ids.len() != left.instances.len() || right_by_id.len() != right.instances.len() {
        return false;
    }
    let no_ids = AHashMap::new();
    let ids = PairedStoreIds {
        service: store_service(left),
        left: &no_ids,
        right: &no_ids,
    };
    // Each instance is compared on its own; a large store uses every core.
    left.instances
        .par_iter()
        .enumerate()
        .all(|(left_index, left_instance)| {
            let Some(right_index) = right_by_id.get(left_instance.settings_id.as_str()).copied()
            else {
                return false;
            };
            let right_instance = &right.instances[right_index];
            // An instance that stands for its unchanged counterpart holds what
            // the counterpart holds.
            if crate::settings::held::stands_unchanged(&left_instance.properties)
                || crate::settings::held::stands_unchanged(&right_instance.properties)
            {
                return true;
            }
            left_instance.name == right_instance.name
                && (preserve_viewport
                    || left_instance.properties.get("UniqueId")
                        == right_instance.properties.get("UniqueId"))
                && left_instance.class_name == right_instance.class_name
                && settings_parent_id(left, left_index) == settings_parent_id(right, right_index)
                && is_reconciliation_protected_workspace_camera(left, left_index)
                    == is_reconciliation_protected_workspace_camera(right, right_index)
                && (left_instance.class_name == "PackageLink"
                    || preserve_viewport
                        && is_reconciliation_protected_workspace_camera(left, left_index)
                        && is_reconciliation_protected_workspace_camera(right, right_index)
                    || reconciliation_maps_equal_with_ids(
                        &left_instance.class_name,
                        &left_instance.properties,
                        &right_instance.properties,
                        ids,
                    ) && reconciliation_values_map_equal_with_ids(
                        &left_instance.attributes,
                        &right_instance.attributes,
                        ids,
                    ))
        })
}

pub(crate) fn is_reconciliation_protected_workspace_camera(
    document: &SettingsBytecode,
    index: usize,
) -> bool {
    let instance = &document.instances[index];
    if instance.class_name != "Camera" {
        return false;
    }
    let mut root = instance;
    while let Some(parent) = root.parent_index {
        root = &document.instances[parent];
    }
    if root.class_name != "Workspace" {
        return false;
    }
    let Some(reference) = root
        .properties
        .get("CurrentCamera")
        .and_then(Value::as_object)
    else {
        return false;
    };
    if !is_reference_object(reference) {
        return false;
    }
    if let Some(id) = reference.get("settingsId").and_then(Value::as_str) {
        return id == instance.settings_id;
    }
    reference.get("instanceIndex").and_then(Value::as_u64) == Some(index as u64 + 1)
}

/// The camera a Workspace store's root names as the viewport's.
pub(crate) fn workspace_current_camera_index(document: &SettingsBytecode) -> Option<usize> {
    document
        .instances
        .iter()
        .enumerate()
        .find_map(|(index, _)| {
            is_reconciliation_protected_workspace_camera(document, index).then_some(index)
        })
}

pub(crate) fn align_reconciliation_protected_workspace_cameras(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) {
    let (Some(reference_index), Some(observed_index)) = (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    ) else {
        return;
    };
    let reference_instance = &reference.instances[reference_index];
    let observed_instance = &mut observed.instances[observed_index];
    if observed_instance.settings_id == reference_instance.settings_id {
        observed_instance
            .properties
            .clone_from(&reference_instance.properties);
        observed_instance
            .attributes
            .clone_from(&reference_instance.attributes);
    }
}

/// The cameras of a Workspace store as it stood before alignment, for the
/// values its viewport camera keeps through a pull.
pub(crate) struct ReferenceViewportCamera {
    root_names_camera: bool,
    current: Option<String>,
    cameras: Vec<SettingsBytecodeInstance>,
}

pub(crate) fn reference_viewport_camera(
    reference: &SettingsBytecode,
) -> Option<ReferenceViewportCamera> {
    let root = reference
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none() && instance.class_name == "Workspace")?;
    Some(ReferenceViewportCamera {
        root_names_camera: root.properties.contains_key("CurrentCamera"),
        current: workspace_current_camera_index(reference)
            .map(|index| reference.instances[index].settings_id.clone()),
        cameras: reference
            .instances
            .iter()
            .filter(|instance| instance.class_name == "Camera")
            .cloned()
            .collect(),
    })
}

/// Gives the observed viewport camera the values the reference holds for the
/// same identity; says whether that changed it. A reference that names no
/// current camera takes the observed one for it when it holds a camera of
/// that identity (see `inherit_workspace_viewport_reference`).
pub(crate) fn keep_reference_viewport_camera(
    reference: &ReferenceViewportCamera,
    observed: &mut SettingsBytecode,
) -> bool {
    let Some(index) = workspace_current_camera_index(observed) else {
        return false;
    };
    let observed = &mut observed.instances[index];
    if reference.root_names_camera && reference.current.as_ref() != Some(&observed.settings_id) {
        return false;
    }
    let Some(saved) = reference
        .cameras
        .iter()
        .find(|camera| camera.settings_id == observed.settings_id)
    else {
        return false;
    };
    if observed.properties == saved.properties && observed.attributes == saved.attributes {
        return false;
    }
    observed.properties.clone_from(&saved.properties);
    observed.attributes.clone_from(&saved.attributes);
    true
}

/// Upgrade old stores only after identity alignment has matched the live
/// viewport to an existing saved camera. Never infer the role from its name.
pub(crate) fn inherit_workspace_viewport_reference(
    desired: &mut SettingsBytecode,
    observed: &SettingsBytecode,
) {
    let Some(root) = desired
        .instances
        .iter()
        .position(|instance| instance.parent_index.is_none() && instance.class_name == "Workspace")
    else {
        return;
    };
    if desired.instances[root]
        .properties
        .contains_key("CurrentCamera")
    {
        return;
    }
    let Some(viewport) = workspace_current_camera_index(observed) else {
        return;
    };
    let id = &observed.instances[viewport].settings_id;
    if desired
        .instances
        .iter()
        .any(|instance| instance.settings_id == *id && instance.class_name == "Camera")
    {
        desired.instances[root].properties.insert(
            "CurrentCamera".into(),
            serde_json::json!({"_type": "Ref", "settingsId": id}),
        );
    }
}

/// The settings ids of two copies of one store, by position, with the
/// service the store is of. A reference into the store is the same in both
/// copies when it names the same position, whatever id each copy gives it.
/// Without positions the ids themselves compare, and the service still says
/// which store a reference without a path is of.
#[derive(Clone, Copy)]
struct PairedStoreIds<'a> {
    service: Option<&'a str>,
    left: &'a AHashMap<&'a str, usize>,
    right: &'a AHashMap<&'a str, usize>,
}

impl PairedStoreIds<'_> {
    /// Whether a reference with this path names an instance of the store.
    /// Settings ids are unique within one store only, so an id of the store
    /// says nothing about a reference whose path leads into another service.
    fn hold(&self, path_service: Option<&str>) -> bool {
        match (path_service, self.service) {
            (Some(target), Some(own)) => target == own,
            _ => true,
        }
    }
}

/// The service a reference's path leads into, when it has a path.
fn reference_path_service(reference: &Map<String, Value>) -> Option<&str> {
    reference
        .get("pathSegments")
        .and_then(Value::as_array)
        .and_then(|segments| segments.first())
        .and_then(Value::as_str)
}

pub(crate) fn reconciliation_maps_equal(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    reconciliation_store_maps_equal(None, class_name, left, right)
}

/// `reconciliation_maps_equal` for two records of the store of `service`. A
/// reference without a path names an instance of that store, so it is not
/// the one a reference into another service names, also when both carry one
/// id.
pub(crate) fn reconciliation_store_maps_equal(
    service: Option<&str>,
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    let no_ids = AHashMap::new();
    let ids = PairedStoreIds {
        service,
        left: &no_ids,
        right: &no_ids,
    };
    reconciliation_maps_equal_with_ids(class_name, left, right, ids)
}

fn reconciliation_maps_equal_with_ids(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ids: PairedStoreIds<'_>,
) -> bool {
    let database = rbx_reflection_database::get().ok();
    let stable = |name: &str, value: &Value| {
        reconciliation_property_compares(
            database,
            class_name,
            name,
            value,
            left.contains_key(name),
            right.contains_key(name),
        )
    };
    for (name, value) in left {
        if !stable(name, value) {
            continue;
        }
        match right.get(name) {
            Some(other) if name == "Tags" && tags_equal(value, other) => {}
            Some(other) if solid_fidelities_match(class_name, name, value, other) => {}
            Some(other)
                if hulls_match(name, value, other)
                    && geometry_sources_equal(class_name, left, right) => {}
            Some(_)
                if name == "PhysicsGrid"
                    && terrain_physics_caches_match(class_name, left, right) => {}
            Some(other)
                if is_asset_reference_property(class_name, name)
                    && asset_texts_equal(value, other) => {}
            Some(other)
                if reconciliation_values_equal_with_ids(
                    value,
                    other,
                    reconciliation_property_uses_f32(class_name, name, value, other),
                    Some(ids),
                ) => {}
            Some(other)
                if studio_recomputed_values_match(class_name, name, value, other)
                    || studio_rewritten_values_match(class_name, name, value, other)
                    || cframes_match_when_rederived(value, other) => {}
            None if reconciliation_property_value_is_default(class_name, name, value) => {}
            _ => return false,
        }
    }
    right.iter().all(|(name, value)| {
        !stable(name, value)
            || left.contains_key(name)
            || reconciliation_property_value_is_default(class_name, name, value)
    })
}

/// Studio rebuilds Terrain's collision cache after loading voxels. Preserve
/// the stored cache only when both records carry exactly the same voxel grid;
/// absent grids and property-only comparisons provide no such proof.
pub(crate) fn terrain_physics_caches_match(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    let binary = |value: &Value| {
        let value = crate::settings::held::resolve(value);
        value.get("_type").and_then(Value::as_str) == Some("BinaryString")
            && value.get("base64").is_some_and(Value::is_string)
    };
    class_name == "Terrain"
        && left
            .get("SmoothGrid")
            .is_some_and(|grid| binary(grid) && right.get("SmoothGrid") == Some(grid))
        && left.get("PhysicsGrid").is_some_and(binary)
        && right.get("PhysicsGrid").is_some_and(binary)
}

/// What a part's collision hull says about its authored fidelity: byte 10 of a
/// version-8 `CSGPHS` blob, or Box for the old BLOCK placeholder. The hull
/// bytes themselves are the engine's and differ from one cook to the next, so
/// two hulls that encode one fidelity compare as equal and a pull keeps the
/// bytes the files have; hulls without a readable code compare by bytes.
pub(crate) fn hull_fidelity_code(value: &Value) -> Option<u8> {
    let value = crate::settings::held::resolve(value);
    let bytes = base64::decode(value.get("base64")?.as_str()?).ok()?;
    if !bytes.starts_with(b"CSGPHS") || bytes.len() < 10 {
        return None;
    }
    let version = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    if version == 0 && bytes.ends_with(b"BLOCK") {
        return Some(2);
    }
    // Studio 0.745 still cooks version 7 hulls for places saved by older
    // builds; the fidelity byte sits where versions 8 and up keep it.
    (version >= 7 && bytes.len() >= 12).then(|| bytes[10])
}

/// Values Studio issues itself and rewrites on every save or load: the
/// packed texture asset of a SurfaceAppearance (a new `rbxassetid://` each
/// time it repacks) and a union's triangle count (recounted when its mesh
/// loads). Two issued values are one authored state; an empty pack is not.
fn studio_recomputed_values_match(
    class_name: &str,
    name: &str,
    left: &Value,
    right: &Value,
) -> bool {
    match (class_name, name) {
        ("SurfaceAppearance", "TexturePack") => [left, right].iter().all(|value| {
            value
                .as_str()
                .is_some_and(|text| text.starts_with("rbxassetid://") && text.len() > 13)
        }),
        ("UnionOperation" | "NegateOperation" | "IntersectOperation", "TriangleCount") => {
            left.is_number() && right.is_number()
        }
        _ => false,
    }
}

/// Values Studio rewrites while loading a place, so that the saved form and
/// Studio's are one value: no edit can be told apart from the rewrite.
fn studio_rewritten_values_match(
    class_name: &str,
    name: &str,
    left: &Value,
    right: &Value,
) -> bool {
    match (class_name, name) {
        // Studio states the clock within one day: a saved "-10:35:20" loads
        // as 13:24:40.
        ("Lighting", "TimeOfDay") => match (
            left.as_str().and_then(time_of_day_seconds),
            right.as_str().and_then(time_of_day_seconds),
        ) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        },
        ("AnimationRigData", _) => rig_tables_match(left, right),
        ("HumanoidDescription", "EmotesDataInternal" | "EquippedEmotesDataInternal") => {
            emote_tables_equal(left, right)
        }
        ("HumanoidDescription", name) if name.ends_with("Accessory") => {
            accessory_lists_match(left, right)
        }
        _ => false,
    }
}

/// The seconds past midnight a `TimeOfDay` text states: hours, minutes and
/// seconds separated by colons, a leading minus counting back from midnight,
/// and any overflow wrapping around the day as Studio wraps it.
pub(crate) fn time_of_day_seconds(text: &str) -> Option<f64> {
    let text = text.trim();
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut parts = body.split(':').map(|part| part.trim().parse::<f64>().ok());
    let hours = parts.next()??;
    let minutes = parts.next().unwrap_or(Some(0.0))?;
    let seconds = parts.next().unwrap_or(Some(0.0))?;
    if parts.next().is_some() {
        return None;
    }
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    let total = if negative { -total } else { total };
    total.is_finite().then(|| total.rem_euclid(86_400.0))
}

/// A `TimeOfDay` text as Studio keeps it: `HH:MM:SS` within one day.
pub(crate) fn canonical_time_of_day(text: &str) -> Option<String> {
    let seconds = time_of_day_seconds(text)?.round() as i64 % 86_400;
    Some(format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    ))
}

/// Studio regenerates every AnimationRigData table when it loads a rig (the
/// version word that leads each one and the transforms it recomputes), and no
/// plugin API writes one, so two tables are the engine's single value.
fn rig_tables_match(left: &Value, right: &Value) -> bool {
    [left, right]
        .iter()
        .all(|value| value.get("_type").and_then(Value::as_str) == Some("BinaryString"))
}

/// A HumanoidDescription's accessory lists name the same asset ids in
/// whichever order Studio last wrote them.
fn accessory_lists_match(left: &Value, right: &Value) -> bool {
    match (accessory_ids(left), accessory_ids(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn accessory_ids(value: &Value) -> Option<Vec<&str>> {
    let mut ids = value
        .as_str()?
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    Some(ids)
}

/// A HumanoidDescription's emote tables name the same entries in whichever
/// order Studio last wrote them.
fn emote_tables_equal(left: &Value, right: &Value) -> bool {
    match (emote_entries(left), emote_entries(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn emote_entries(value: &Value) -> Option<Vec<&str>> {
    let mut entries = value
        .as_str()?
        .split('\\')
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    entries.sort_unstable();
    Some(entries)
}

/// Studio keeps a saved rotation's first two axes and re-derives the third
/// from them, so a matrix saved with a mirrored or skewed third axis loads as
/// the matrix those two axes span. A matrix whose third axis already is that
/// one is compared as saved, so an edit of a few millionths still counts.
fn cframes_match_when_rederived(left: &Value, right: &Value) -> bool {
    let components = |value: &Value| -> Option<[f64; 12]> {
        if value.get("_type").and_then(Value::as_str) != Some("CFrame") {
            return None;
        }
        let values = value.get("components")?.as_array()?;
        if values.len() != 12 {
            return None;
        }
        let mut out = [0.0; 12];
        for (slot, value) in out.iter_mut().zip(values) {
            *slot = value.as_f64().filter(|value| value.is_finite())?;
        }
        Some(out)
    };
    let (Some(left), Some(right)) = (components(left), components(right)) else {
        return false;
    };
    let (Some(left_rotation), Some(right_rotation)) =
        (rederived_rotation(&left), rederived_rotation(&right))
    else {
        return false;
    };
    (third_axis_rewritten(&left, &left_rotation) || third_axis_rewritten(&right, &right_rotation))
        && left[..3]
            .iter()
            .zip(&right[..3])
            .all(|(a, b)| (a - b).abs() <= CFRAME_POSITION_NOISE)
        && left_rotation
            .iter()
            .zip(&right_rotation)
            .all(|(a, b)| (a - b).abs() <= CFRAME_ROTATION_NOISE)
}

/// Whether a saved matrix's third axis is far from the one its first two
/// axes span: a mirrored axis is two units away, float drift and small edits
/// stay within millionths.
fn third_axis_rewritten(components: &[f64; 12], rederived: &[f64; 9]) -> bool {
    const REWRITTEN_AXIS_DISTANCE: f64 = 1e-3;
    [5, 8, 11].iter().zip([2, 5, 8]).any(|(saved, derived)| {
        (components[*saved] - rederived[derived]).abs() > REWRITTEN_AXIS_DISTANCE
    })
}

/// The rotation Studio builds from a saved matrix: the first axis
/// normalized, the second made perpendicular to it, the third their cross
/// product; row-major like the saved components.
fn rederived_rotation(components: &[f64; 12]) -> Option<[f64; 9]> {
    let column = |index: usize| {
        [
            components[3 + index],
            components[6 + index],
            components[9 + index],
        ]
    };
    let normalize = |v: [f64; 3]| -> Option<[f64; 3]> {
        let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        (length > 1e-9).then(|| [v[0] / length, v[1] / length, v[2] / length])
    };
    let x = normalize(column(0))?;
    let up = column(1);
    let along = x[0] * up[0] + x[1] * up[1] + x[2] * up[2];
    let y = normalize([
        up[0] - x[0] * along,
        up[1] - x[1] * along,
        up[2] - x[2] * along,
    ])?;
    let z = [
        x[1] * y[2] - x[2] * y[1],
        x[2] * y[0] - x[0] * y[2],
        x[0] * y[1] - x[1] * y[0],
    ];
    Some([x[0], y[0], z[0], x[1], y[1], z[1], x[2], y[2], z[2]])
}

/// A font value, typed or spelled by its fields alone as stores keep it.
fn is_font_value(value: &Map<String, Value>) -> bool {
    match value.get("_type").and_then(Value::as_str) {
        Some(type_name) => type_name == "Font",
        None => {
            value.len() <= 4
                && ["family", "weight", "style"]
                    .iter()
                    .all(|field| value.get(*field).is_some_and(Value::is_string))
        }
    }
}

/// The fields of a font value that are the font: Studio's cached face file
/// is its own lookup and changes from one build to the next.
fn authored_font_fields(font: &Map<String, Value>) -> impl Iterator<Item = (&String, &Value)> {
    font.iter()
        .filter(|(name, _)| !matches!(name.as_str(), "cachedFaceId" | "_type"))
}

/// Hulls that say the same thing: both encode one fidelity, or one is the
/// box placeholder Studio writes before a mesh has cooked, which the engine
/// replaces by the cooked hull of that very mesh once it loads.
fn hulls_match(name: &str, left: &Value, right: &Value) -> bool {
    name == crate::rbx::encode::COLLISION_HULL_PROPERTY
        && (hull_is_placeholder(left)
            || hull_is_placeholder(right)
            || match (hull_fidelity(left), hull_fidelity(right)) {
                (Some(left), Some(right)) => left == right,
                // A hull in a format whose fidelity cannot be read (version 6
                // from older saves) is still the engine's cook of the same
                // mesh; nothing authored can differ between two of them.
                _ => is_cooked_hull(left) && is_cooked_hull(right),
            })
}

/// The fidelity a hull record states: a `CollisionHullFidelity` marker the
/// first comparison stands in for a readable hull, or the hull's own byte.
fn hull_fidelity(value: &Value) -> Option<u8> {
    if value.get("_type").and_then(Value::as_str) == Some("CollisionHullFidelity") {
        return value
            .get("code")
            .and_then(Value::as_u64)
            .and_then(|code| u8::try_from(code).ok());
    }
    hull_fidelity_code(value)
}

fn is_cooked_hull(value: &Value) -> bool {
    let value = crate::settings::held::resolve(value);
    value.get("_type").and_then(Value::as_str) == Some("CollisionHullFidelity")
        || value
            .get("base64")
            .and_then(Value::as_str)
            .and_then(|text| base64::decode(text).ok())
            .is_some_and(|bytes| bytes.starts_with(b"CSGPHS") && bytes.len() >= 10)
}

/// The engine reports a solid operation's fidelity as Box while its mesh is
/// a placeholder and as Default once its hull is cooked, and honours neither
/// write; the two are one authored value. Hull and PreciseConvexDecomposition
/// stay real choices.
fn solid_fidelities_match(class_name: &str, name: &str, left: &Value, right: &Value) -> bool {
    fn box_or_default(value: &Value) -> bool {
        match value.get("name").and_then(Value::as_str) {
            Some(name) => matches!(name, "Box" | "Default"),
            None => matches!(value.get("value").and_then(Value::as_u64), Some(0 | 2)),
        }
    }
    name == "CollisionFidelity"
        && crate::rbx::encode::is_solid_model_operation(class_name)
        && box_or_default(left)
        && box_or_default(right)
}

/// A hull is cooked from the mesh an instance names or carries; two hulls
/// compare by fidelity only while both records hold the same mesh: the asset
/// selectors and, for a solid operation, its CSG tree and mesh data.
fn geometry_sources_equal(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    let hull = crate::rbx::encode::COLLISION_HULL_PROPERTY;
    ["MeshId", "MeshContent", "AssetId"]
        .iter()
        .copied()
        .chain(
            crate::rbx::encode::engine_geometry_properties(class_name)
                .iter()
                .copied()
                .filter(|name| *name != hull && *name != "AeroMeshData"),
        )
        .all(|name| {
            reconciliation_property_values_equal(class_name, name, left.get(name), right.get(name))
        })
}

fn hull_is_placeholder(value: &Value) -> bool {
    crate::settings::held::resolve(value)
        .get("base64")
        .and_then(Value::as_str)
        .and_then(|text| base64::decode(text).ok())
        .is_some_and(|bytes| {
            bytes.starts_with(b"CSGPHS")
                && bytes.len() >= 10
                && u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]) == 0
                && bytes.ends_with(b"BLOCK")
        })
}

/// Tags are a set: Studio and the store may list them in any order.
fn tags_equal(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_array(), right.as_array()) else {
        return false;
    };
    let mut left: Vec<&str> = left.iter().filter_map(Value::as_str).collect();
    let mut right: Vec<&str> = right.iter().filter_map(Value::as_str).collect();
    left.sort_unstable();
    left.dedup();
    right.sort_unstable();
    right.dedup();
    left == right
}

/// Asset references spell one asset several ways (`rbxassetid://N`,
/// `http://www.roblox.com/asset/?id=N`, `https://...`); Studio hands back the
/// long form for a short one it was given. Only a whole reference in one of
/// those exact forms is rewritten; anything else compares as written.
pub(crate) fn canonical_asset_text(text: &str) -> std::borrow::Cow<'_, str> {
    static FORMS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let forms = FORMS.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^\s*(?:rbxassetid://|https?://(?:www\.)?roblox\.com/asset/?\?id=)(\d+)(\?.*|&.*)?\s*$",
        )
        .expect("asset reference pattern")
    });
    match forms.captures(text) {
        Some(captures) => std::borrow::Cow::Owned(format!(
            "rbxassetid://{}{}",
            &captures[1],
            captures
                .get(2)
                .map_or("", |suffix| suffix.as_str().trim_end())
        )),
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// Properties whose string value names an asset, by reflection type or by
/// the names Studio uses for the Content mirrors the database may not know.
pub(crate) fn is_asset_reference_property(class_name: &str, name: &str) -> bool {
    if let Ok(database) = rbx_reflection_database::get()
        && let Some(descriptor) =
            crate::rbx::encode::rbx_property_descriptor(database, class_name, name)
        && let rbx_reflection::DataType::Value(kind) = &descriptor.data_type
    {
        return matches!(kind, RbxVariantType::Content | RbxVariantType::ContentId);
    }
    name.ends_with("Content")
        || matches!(
            name,
            "TextureId" | "TextureID" | "MeshId" | "SoundId" | "Image" | "Texture"
        )
}

fn asset_texts_equal(left: &Value, right: &Value) -> bool {
    let (left, right) = (typed_scalar(left), typed_scalar(right));
    match (left.as_str(), right.as_str()) {
        (Some(left), Some(right)) => canonical_asset_text(left) == canonical_asset_text(right),
        _ => false,
    }
}

pub(crate) fn reconciliation_values_map_equal(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    reconciliation_store_values_map_equal(None, left, right)
}

/// `reconciliation_values_map_equal` for two records of the store of
/// `service`, as `reconciliation_store_maps_equal` compares properties.
pub(crate) fn reconciliation_store_values_map_equal(
    service: Option<&str>,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    let no_ids = AHashMap::new();
    let ids = PairedStoreIds {
        service,
        left: &no_ids,
        right: &no_ids,
    };
    reconciliation_values_map_equal_with_ids(left, right, ids)
}

// Studio writes RBX_ attributes for itself (reimport ids, migration markers)
// and drops them from synced writes, so they never take part in equivalence.
pub(crate) fn is_engine_managed_attribute(name: &str) -> bool {
    name.starts_with("RBX_")
}

fn reconciliation_values_map_equal_with_ids(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ids: PairedStoreIds<'_>,
) -> bool {
    let authored = |(name, _): &(&String, &Value)| !is_engine_managed_attribute(name);
    left.iter().filter(authored).count() == right.iter().filter(authored).count()
        && left.iter().filter(authored).all(|(name, value)| {
            right.get(name).is_some_and(|other| {
                reconciliation_values_equal_with_ids(value, other, false, Some(ids))
            })
        })
}

fn reconciliation_identity_maps_equal(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ignore_transient_properties: bool,
) -> bool {
    let stable = |name: &str, value: &Value| {
        identity_value_is_stable(name, value, ignore_transient_properties)
    };
    left.iter()
        .filter(|(name, value)| stable(name, value))
        .count()
        == right
            .iter()
            .filter(|(name, value)| stable(name, value))
            .count()
        && left
            .iter()
            .filter(|(name, value)| stable(name, value))
            .all(|(name, value)| {
                right
                    .get(name)
                    .is_some_and(|other| reconciliation_values_equal(value, other, false))
            })
}

pub(crate) fn identity_value_is_stable(
    name: &str,
    value: &Value,
    ignore_transient_properties: bool,
) -> bool {
    (!ignore_transient_properties
        || name != "ScriptGuid"
            && !reconciliation_property_is_derived(name)
            && !reconciliation_property_is_metadata(name, value))
        && !reconciliation_value_contains_reference(value)
}

pub(crate) fn reconciliation_value_contains_reference(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(reconciliation_value_contains_reference),
        Value::Object(object) => {
            is_reference_object(object)
                || object.values().any(reconciliation_value_contains_reference)
        }
        _ => false,
    }
}

pub(crate) fn reconciliation_values_equal(left: &Value, right: &Value, approximate: bool) -> bool {
    reconciliation_values_equal_with_ids(left, right, approximate, None)
}

/// `reconciliation_values_equal` for two values an instance of the store of
/// `service` holds.
fn reconciliation_store_values_equal(
    service: Option<&str>,
    left: &Value,
    right: &Value,
    approximate: bool,
) -> bool {
    let no_ids = AHashMap::new();
    let ids = service.map(|service| PairedStoreIds {
        service: Some(service),
        left: &no_ids,
        right: &no_ids,
    });
    reconciliation_values_equal_with_ids(left, right, approximate, ids)
}

/// Stores written by older model imports keep scalars typed, as
/// `{"_type":"Float32","value":n}` or `{"_type":"Content","value":"..."}`;
/// pulls store the plain number or string.
fn typed_scalar(value: &Value) -> &Value {
    let Some(object) = value.as_object() else {
        return value;
    };
    let inner = object.get("value");
    match object.get("_type").and_then(Value::as_str) {
        Some("Int32" | "Int64" | "Float32" | "Float64") => inner.filter(|inner| inner.is_number()),
        Some("Content" | "ContentId") => inner.filter(|inner| inner.is_string()),
        _ => None,
    }
    .unwrap_or(value)
}

/// Bridge-shaped values wrap the type as the only key (`{"NumberSequence":
/// {...}}`, `{"Ref": {...}}`, `{"BrickColor": n}`) and spell colour keypoints
/// as arrays; stores merged from such exports carry them verbatim.
fn canonical_wrapped_value(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    for type_name in ["NumberSequence", "ColorSequence"] {
        let sequence = if object.len() == 1 {
            object.get(type_name)
        } else if object.get("_type").and_then(Value::as_str) == Some(type_name) {
            Some(value)
        } else {
            None
        };
        if let Some(sequence) = sequence {
            let normalized =
                crate::editor::review::normalize_editor_sequence_value(sequence, type_name);
            return (normalized != *value).then_some(normalized);
        }
    }
    if object.len() == 1
        && let Some(reference) = object.get("Ref").and_then(Value::as_object)
    {
        let mut out = reference.clone();
        out.insert("_type".to_string(), Value::String("Ref".to_string()));
        return Some(Value::Object(out));
    }
    if object.len() == 1
        && let Some(number) = object.get("BrickColor")
        && number.is_number()
    {
        return Some(serde_json::json!({"_type": "BrickColor", "number": number}));
    }
    None
}

fn reconciliation_values_equal_with_ids(
    left: &Value,
    right: &Value,
    approximate: bool,
    ids: Option<PairedStoreIds<'_>>,
) -> bool {
    // Two handles of one payload hold one value; any other pair compares
    // as the payloads they stand for.
    let held = (
        crate::settings::held::resolve(left),
        crate::settings::held::resolve(right),
    );
    if held.0.same_payload(&held.1) {
        return true;
    }
    let (left, right): (&Value, &Value) = (&held.0, &held.1);
    let (left, right) = (typed_scalar(left), typed_scalar(right));
    let canonical = (
        canonical_wrapped_value(left),
        canonical_wrapped_value(right),
    );
    let (left, right) = (
        canonical.0.as_ref().unwrap_or(left),
        canonical.1.as_ref().unwrap_or(right),
    );
    match (left, right) {
        (Value::Number(left), Value::Number(right)) if !approximate => {
            exact_json_numbers_equal(left, right)
        }
        (Value::Number(left), Value::Number(right)) if approximate => {
            let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) else {
                return left == right;
            };
            left == right
                || left.is_finite()
                    && right.is_finite()
                    && (left - right).abs()
                        <= 4.0 * f64::from(f32::EPSILON) * left.abs().max(right.abs()).max(1.0)
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    reconciliation_values_equal_with_ids(left, right, approximate, ids)
                })
        }
        (Value::Object(left), Value::Object(right)) => {
            let type_name = left.get("_type").and_then(Value::as_str);
            if type_name == Some("Ref") && right.get("_type").and_then(Value::as_str) == type_name {
                let left_id = left
                    .get("settingsId")
                    .or_else(|| left.get("instanceId"))
                    .and_then(Value::as_str);
                let right_id = right
                    .get("settingsId")
                    .or_else(|| right.get("instanceId"))
                    .and_then(Value::as_str);
                if let (Some(left_id), Some(right_id)) = (left_id, right_id) {
                    // Settings ids are unique within one store only, and the
                    // path's service says which store an id is of: the same
                    // id under two services names two instances. A reference
                    // without a path is one of the store that holds it.
                    let own_service = ids.and_then(|ids| ids.service);
                    let left_service = reference_path_service(left).or(own_service);
                    let right_service = reference_path_service(right).or(own_service);
                    if let (Some(left_service), Some(right_service)) = (left_service, right_service)
                        && left_service != right_service
                    {
                        return false;
                    }
                    let positions = ids
                        .filter(|ids| ids.hold(left_service) && ids.hold(right_service))
                        .and_then(|ids| ids.left.get(left_id).zip(ids.right.get(right_id)));
                    return match positions {
                        Some((left, right)) => left == right,
                        None => left_id == right_id,
                    };
                }
                let left_path = (left.get("pathSegments"), left.get("pathOrdinals"));
                let right_path = (right.get("pathSegments"), right.get("pathOrdinals"));
                if left_path.0.is_some() && left_path == right_path {
                    return true;
                }
            }
            if type_name == Some("EnumItem")
                && right.get("_type").and_then(Value::as_str) == type_name
            {
                let enum_types_match = match (
                    left.get("enumType").and_then(Value::as_str),
                    right.get("enumType").and_then(Value::as_str),
                ) {
                    (Some(left), Some(right)) => left == right,
                    _ => true,
                };
                // A store written before the reflection database knew an enum
                // keeps the item's number as its name.
                let enum_number = |value: &Map<String, Value>| {
                    value.get("value").and_then(Value::as_u64).or_else(|| {
                        value
                            .get("name")
                            .and_then(Value::as_str)
                            .and_then(|name| name.parse::<u64>().ok())
                    })
                };
                let values_match = match (enum_number(left), enum_number(right)) {
                    (Some(left), Some(right)) => left == right,
                    _ => false,
                };
                let names_match = match (
                    left.get("name").and_then(Value::as_str),
                    right.get("name").and_then(Value::as_str),
                ) {
                    (Some(left), Some(right)) => left == right,
                    _ => false,
                };
                return enum_types_match && (values_match || names_match);
            }
            if is_font_value(left) && is_font_value(right) {
                return authored_font_fields(left).count() == authored_font_fields(right).count()
                    && authored_font_fields(left).all(|(name, value)| {
                        right.get(name).is_some_and(|other| {
                            reconciliation_values_equal_with_ids(value, other, approximate, ids)
                        })
                    });
            }
            let approximate = approximate || type_name.is_some_and(reconciliation_value_uses_f32);
            left.len() == right.len()
                && left.iter().all(|(name, value)| {
                    let Some(other) = right.get(name) else {
                        return false;
                    };
                    if type_name == Some("CFrame") && name == "components" {
                        let (Some(left), Some(right)) = (value.as_array(), other.as_array()) else {
                            return false;
                        };
                        return left.len() == 12
                            && right.len() == 12
                            && left
                                .iter()
                                .zip(right)
                                .enumerate()
                                .all(|(index, (left, right))| {
                                    let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) else {
                                        // Saved engine fields can contain NaN/inf sentinels.
                                        // Preserve identical encodings, not numeric NaN equality.
                                        return left == right
                                            && nonfinite_float_from_json(left).is_some();
                                    };
                                    a.is_finite()
                                        && b.is_finite()
                                        && if index < 3 {
                                            reconciliation_values_equal(left, right, true)
                                                || (a - b).abs() <= CFRAME_POSITION_NOISE
                                        } else {
                                            (a - b).abs()
                                                <= CFRAME_ROTATION_EPSILON
                                                    .max(CFRAME_ROTATION_NOISE)
                                        }
                                });
                    }
                    if type_name == Some("Ref")
                        && matches!(name.as_str(), "settingsId" | "instanceId")
                    {
                        let (Some(left), Some(right)) = (value.as_str(), other.as_str()) else {
                            return value == other;
                        };
                        return match (
                            ids.and_then(|ids| ids.left.get(left)),
                            ids.and_then(|ids| ids.right.get(right)),
                        ) {
                            (Some(left), Some(right)) => left == right,
                            (None, None) => left == right,
                            _ => false,
                        };
                    }
                    let approximate = approximate
                        && !matches!(
                            (type_name, name.as_str()),
                            (Some("UDim"), "offset") | (Some("UDim2"), "xOffset" | "yOffset")
                        );
                    reconciliation_values_equal_with_ids(value, other, approximate, ids)
                })
        }
        _ => left == right,
    }
}

fn exact_json_numbers_equal(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    if left == right {
        return true;
    }
    let (float, integer) = if left.is_f64() && !right.is_f64() {
        (left, right)
    } else if right.is_f64() && !left.is_f64() {
        (right, left)
    } else {
        return false;
    };
    let integer = integer
        .as_i64()
        .map(i128::from)
        .or_else(|| integer.as_u64().map(i128::from));
    float.as_f64().is_some_and(|value| {
        value.is_finite() && value.fract() == 0.0 && integer == Some(value as i128)
    })
}

fn reconciliation_value_uses_f32(type_name: &str) -> bool {
    matches!(
        type_name,
        "CFrame"
            | "Color3"
            | "ColorSequence"
            | "NumberRange"
            | "NumberSequence"
            | "PhysicalProperties"
            | "Ray"
            | "Rect"
            | "Region3"
            | "UDim"
            | "UDim2"
            | "Vector2"
            | "Vector3"
    )
}

pub(crate) fn reconciliation_property_is_derived(name: &str) -> bool {
    // Studio recomputes the World* fields from the local ones, flips the
    // migration flags itself while loading a tree, caches a model's mesh
    // bounds after insertion, counts a MeshPart's vertices and integrates a
    // mesh's mass properties once its mesh has loaded, and refreshes a
    // WeldConstraint's cached offset from its parts, so none of them can be
    // authored or retained through a sync.
    matches!(
        name,
        "WorldCFrame"
            | "WorldPosition"
            | "WorldOrientation"
            | "WorldAxis"
            | "WorldSecondaryAxis"
            | "InertiaMigrated"
            | "NeedsPivotMigration"
            | "ModelMeshCFrame"
            | "ModelMeshSize"
            | "ModelMeshData"
            | "CanvasPosition"
            | "VertexCount"
            | "UnscaledCofm"
            | "UnscaledVolInertiaDiags"
            | "UnscaledVolInertiaOffDiags"
            | "UnscaledVolume"
            | "CFrame0"
    )
}

// Studio's TextScaled setter turns TextWrapped on and keeps it on, so a saved
// TextWrapped=false next to TextScaled=true cannot survive a Studio write.
pub(crate) fn reconciliation_property_is_forced(
    name: &str,
    properties: &Map<String, Value>,
) -> bool {
    name == "TextWrapped" && properties.get("TextScaled") == Some(&Value::Bool(true))
}

/// Whether the stored values carry a TextWrapped that Studio would override:
/// off, or not stored at all, which reads as the class default, off.
pub(crate) fn forced_property_differs(properties: &Map<String, Value>) -> bool {
    properties.get("TextScaled") == Some(&Value::Bool(true))
        && properties.get("TextWrapped") != Some(&Value::Bool(true))
}

/// Gives a forced property the value Studio keeps, so the store and the
/// place agree after a push.
pub(crate) fn settle_forced_properties(properties: &mut Map<String, Value>) {
    if forced_property_differs(properties) {
        properties.insert("TextWrapped".to_string(), Value::Bool(true));
    }
}

// Reflection defaults are keyed by the logical name; a serialized alias such as
// GuiObject.Sink only reaches its default through the logical InputSink.
pub(crate) fn reflection_default_settings_value(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
) -> Option<Value> {
    let class = database.classes.get(class_name)?;
    let descriptor = rbx_model_property_descriptor(database, class_name, name);
    let default = database.find_default_property(class, name).or_else(|| {
        let serialized_name = descriptor.map(|descriptor| descriptor.name)?;
        (serialized_name != name)
            .then(|| database.find_default_property(class, serialized_name))
            .flatten()
    })?;
    rbx_variant_to_settings_json(
        default,
        descriptor,
        database,
        &BytecodeModelImportRefs::default(),
    )
}

pub(crate) fn reconciliation_property_is_metadata(name: &str, value: &Value) -> bool {
    // Copies receive engine-owned identities/history; these are not write targets.
    // Keep the captured fields on disk. Structural IDs and Ref targets are compared
    // separately, so excluding these values does not hide lost instance references.
    matches!(name, "CurrentCamera" | "UniqueId" | "HistoryId")
        || name == "Source" && value.as_str() == Some(EXTERNAL_SOURCE_MARKER)
}

pub(crate) fn reconciliation_property_value<'a>(
    properties: &'a Map<String, Value>,
    name: &str,
) -> Option<&'a Value> {
    properties
        .get(name)
        .filter(|value| !reconciliation_property_is_metadata(name, value))
}

// A Decal's ColorMapContent is the engine's view of TextureContent: Studio
// reports it and captures reconstruct it, but it is never authored separately.
pub(crate) fn reconciliation_property_is_engine_mirror(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
) -> bool {
    name == "ColorMapContent"
        && crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "Decal")
}

/// The one rule for whether a property takes part in comparing a Studio
/// record with a file record. Equivalence, retention reporting and the merge
/// driver all ask this, so a value never counts as equal in one place and
/// different in another.
pub(crate) fn reconciliation_property_compares(
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    class_name: &str,
    name: &str,
    value: &Value,
    left_has: bool,
    right_has: bool,
) -> bool {
    !reconciliation_property_is_engine_state(database, class_name, name, value)
        && (left_has && right_has
            || !reconciliation_property_is_unknown_when_absent(database, class_name, name))
}

/// Engine-owned values: identities, migration flags, caches the engine
/// recomputes on load, mirrors of another property and service fields Studio
/// never exposes. They are neither authored nor retained through a sync.
pub(crate) fn reconciliation_property_is_engine_state(
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    class_name: &str,
    name: &str,
    value: &Value,
) -> bool {
    reconciliation_property_is_metadata(name, value)
        || reconciliation_property_is_always_engine_state(database, class_name, name)
}

/// The same for a property that is engine state whatever it holds. No
/// comparison ever looks at one, so a digest of what an instance holds
/// leaves it out.
pub(crate) fn reconciliation_property_is_always_engine_state(
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    class_name: &str,
    name: &str,
) -> bool {
    name == "ScriptGuid"
        || name == "HistoryId"
        || reconciliation_property_is_derived(name)
        // The engine sets a WeldConstraint's State from whether its parts are
        // currently joined; a recreated weld settles to another value.
        || class_name == "WeldConstraint" && name == "State"
        // Workspace.InsertPoint is transient editor insertion state: reflection
        // marks it DoesNotSerialize and pushes already leave it alone.
        || class_name == "Workspace" && name == "InsertPoint"
        // The deprecated numeric id is the engine's per-place index of the
        // CollisionGroup name; a copy placed in another place gets another id.
        || name == "CollisionGroupId"
        || database.is_some_and(|database| {
            reconciliation_property_is_engine_mirror(database, class_name, name)
        })
}

// Scripts cannot read a NotScriptable property, so a capture that lacks one
// says nothing about its value.
// Audio attenuation curves are not scriptable, but the plugin reads and
// writes them through Get/SetDistanceAttenuation and Get/SetAngleAttenuation.
pub(crate) fn plugin_accesses_property_natively(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
) -> bool {
    matches!(name, "DistanceAttenuation" | "AngleAttenuation")
        && (crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "AudioEmitter")
            || crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "AudioListener"))
}

pub(crate) fn reconciliation_property_is_unreadable(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class_name: &str,
    name: &str,
) -> bool {
    !plugin_accesses_property_natively(database, class_name, name)
        && crate::rbx::encode::rbx_property_descriptor(database, class_name, name).is_some_and(
            |descriptor| {
                matches!(
                    descriptor.scriptability,
                    rbx_reflection::Scriptability::None
                )
            },
        )
}

// A record can lack these without saying anything: MeshSize is what the
// engine measured once a mesh loaded, CollisionFidelity and ClockTime are
// filled in by Studio, and NotScriptable properties cannot be captured. Service
// settings without a plugin API arrive only through a native capture, so they
// compare when both records carry them and say nothing when one lacks them.
/// Fields a Studio capture or a file may leave out without saying anything
/// about them: Studio fills the Lighting fields in when a place saved before
/// they existed loads, never captures Workspace's pivot, and reports a gui's
/// InputSinkEnabled only on some builds. A push never resets them, and a
/// missing capture value cannot verify them; explicit captured values still
/// must match. The first pairing does not compare them one-sided.
pub(crate) fn capture_may_omit(class_name: &str, name: &str) -> bool {
    name == "InputSinkEnabled"
        || matches!(
            (class_name, name),
            ("Lighting", "LightingStyle" | "PrioritizeLightingQuality")
                | ("Workspace", "WorldPivot")
        )
}

pub(crate) fn reconciliation_property_is_unknown_when_absent(
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    class_name: &str,
    name: &str,
) -> bool {
    // Every capture and store spells a script's Disabled flag as Enabled and
    // carries it only for a disabled script, so a record without it says the
    // script is enabled; reflection alone would call the field unserialized.
    if name == "Enabled"
        && database.is_some_and(|database| {
            crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BaseScript")
        })
    {
        return false;
    }
    matches!(
        name,
        "CollisionFidelity" | "ClockTime" | "MeshSize" | "PhysicalConfigData"
    )
        || capture_may_omit(class_name, name)
        || studio_recomputed_value_is_absent_elsewhere(class_name, name)
        // A native capture of Studio 744 carries no InputSink although the
        // saved place does, and reports ResetOnSpawn false for a gui that
        // lives in the world while the saved place carries nothing; neither
        // record says anything about the other's value.
        || name == "InputSink"
        || matches!(
            (class_name, name),
            ("BillboardGui" | "SurfaceGui", "ResetOnSpawn")
        )
        || database.is_some_and(|database| {
            reconciliation_property_is_unreadable(database, class_name, name)
                || crate::rbx::decode::is_unexposed_service_property(database, class_name, name)
                || match crate::rbx::encode::rbx_property_descriptor(database, class_name, name) {
                    None => true,
                    Some(descriptor) => {
                        descriptor
                            .tags
                            .contains(&rbx_reflection::PropertyTag::Deprecated)
                            || matches!(
                                descriptor.kind,
                                rbx_reflection::PropertyKind::Canonical {
                                    serialization:
                                        rbx_reflection::PropertySerialization::DoesNotSerialize
                                }
                            )
                    }
                }
        })
}

pub(crate) fn reconciliation_property_value_is_default(
    class_name: &str,
    name: &str,
    value: &Value,
) -> bool {
    if is_known_default_property_value(name, value) {
        return true;
    }
    let Ok(database) = rbx_reflection_database::get() else {
        return false;
    };
    // Reflection knows the legacy Disabled=false default, not the canonical
    // Enabled spelling; a legacy store that said Disabled=false arrives here
    // as Enabled=true.
    if name == "Enabled"
        && value == &Value::Bool(true)
        && crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BaseScript")
    {
        return true;
    }
    let Some(default) = reflection_default_settings_value(database, class_name, name) else {
        return false;
    };
    reconciliation_values_equal(
        value,
        &default,
        reconciliation_property_uses_f32(class_name, name, value, &default),
    )
}

fn reconciliation_property_uses_f32(
    class_name: &str,
    name: &str,
    left: &Value,
    right: &Value,
) -> bool {
    // Structured values carry their own numeric type. Only unequal scalar numbers
    // need reflection to distinguish Float32 rounding from exact Float64 values.
    left.is_number()
        && right.is_number()
        && left != right
        && rbx_reflection_database::get()
            .ok()
            .and_then(|database| rbx_model_property_descriptor(database, class_name, name))
            .is_some_and(|descriptor| {
                matches!(
                    descriptor.data_type,
                    RbxDataType::Value(RbxVariantType::Float32)
                )
            })
}

pub(crate) fn reconciliation_property_values_equal(
    class_name: &str,
    name: &str,
    left: Option<&Value>,
    right: Option<&Value>,
) -> bool {
    reconciliation_store_property_values_equal(None, class_name, name, left, right)
}

/// `reconciliation_property_values_equal` for two values of one property of
/// an instance of the store of `service`, as
/// `reconciliation_store_maps_equal` compares records.
fn reconciliation_store_property_values_equal(
    service: Option<&str>,
    class_name: &str,
    name: &str,
    left: Option<&Value>,
    right: Option<&Value>,
) -> bool {
    let left = left.filter(|value| !reconciliation_property_is_metadata(name, value));
    let right = right.filter(|value| !reconciliation_property_is_metadata(name, value));
    match (left, right) {
        (Some(left), Some(right)) if name == "Tags" && tags_equal(left, right) => true,
        (Some(left), Some(right)) if solid_fidelities_match(class_name, name, left, right) => true,
        (Some(left), Some(right)) if hulls_match(name, left, right) => true,
        (Some(left), Some(right)) if byte_colors_match(class_name, name, left, right) => true,
        (Some(left), Some(right))
            if studio_recomputed_values_match(class_name, name, left, right)
                || studio_rewritten_values_match(class_name, name, left, right) =>
        {
            true
        }
        (Some(left), Some(right))
            if is_asset_reference_property(class_name, name) && asset_texts_equal(left, right) =>
        {
            true
        }
        (Some(left), Some(right)) => {
            reconciliation_store_values_equal(
                service,
                left,
                right,
                reconciliation_property_uses_f32(class_name, name, left, right),
            ) || cframes_match_when_rederived(left, right)
        }
        (Some(value), None) | (None, Some(value)) => {
            studio_recomputed_value_is_absent_elsewhere(class_name, name)
                || reconciliation_property_value_is_default(class_name, name, value)
        }
        (None, None) => true,
    }
}

/// A color Studio keeps in bytes (a part's `Color` saves as `Color3uint8`):
/// the files may say 0.3 while the open place reads 77/255, and both name
/// the same byte, so they are one color.
fn byte_colors_match(class_name: &str, name: &str, left: &Value, right: &Value) -> bool {
    let stored_in_bytes = rbx_reflection_database::get()
        .ok()
        .and_then(|database| rbx_model_property_descriptor(database, class_name, name))
        .is_some_and(|descriptor| {
            matches!(
                descriptor.data_type,
                RbxDataType::Value(RbxVariantType::Color3uint8)
            )
        });
    if !stored_in_bytes {
        return false;
    }
    let (Some(left), Some(right)) = (json_to_rbx_color3(left), json_to_rbx_color3(right)) else {
        return false;
    };
    let bytes = |color: rbx_dom_weak::types::Color3| {
        [color.r, color.g, color.b].map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8)
    };
    bytes(left) == bytes(right)
}

/// A value Studio counts for itself and the files never carry (a union's
/// triangle count): its presence on one side only is not a difference.
fn studio_recomputed_value_is_absent_elsewhere(class_name: &str, name: &str) -> bool {
    matches!(
        (class_name, name),
        (
            "UnionOperation" | "NegateOperation" | "IntersectOperation",
            "TriangleCount"
        )
    )
}

/// A saved field the files carry with a value the open place does not have
/// and that no plugin API can set; a push applies it through the native
/// property writer, or names it when that is impossible.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SavedFieldDifference {
    pub(crate) path_segments: Vec<String>,
    pub(crate) class_name: String,
    pub(crate) property: String,
    pub(crate) value: Value,
    pub(crate) ambiguous: bool,
}

impl SavedFieldDifference {
    pub(crate) fn label(&self) -> String {
        format!("{}.{}", self.class_name, self.property)
    }
}

pub(crate) fn saved_field_differences(
    files: &SettingsBytecode,
    studio: &SettingsBytecode,
) -> Vec<SavedFieldDifference> {
    let _span = crate::app::profile::span("saved field differences");
    let Ok(database) = rbx_reflection_database::get() else {
        return Vec::new();
    };
    let studio_by_id = studio
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance))
        .collect::<HashMap<_, _>>();
    let mut sibling_counts: HashMap<(Option<usize>, &str), usize> = HashMap::new();
    for instance in &files.instances {
        *sibling_counts
            .entry((instance.parent_index, instance.name.as_str()))
            .or_default() += 1;
    }
    let mut differences = Vec::new();
    for instance in &files.instances {
        let Some(observed) = studio_by_id.get(instance.settings_id.as_str()) else {
            continue;
        };
        if crate::settings::held::stands_unchanged(&observed.properties) {
            continue;
        }
        let class_name = &instance.class_name;
        let mut path = None;
        for (name, value) in &instance.properties {
            if !crate::editor::review::plugin_cannot_write_property(database, class_name, name) {
                continue;
            }
            if reconciliation_property_values_equal(
                class_name,
                name,
                Some(value),
                observed.properties.get(name),
            ) {
                continue;
            }
            let (segments, ambiguous) = path
                .get_or_insert_with(|| {
                    let mut segments = Vec::new();
                    let mut ambiguous = false;
                    let mut cursor = Some(instance);
                    while let Some(current) = cursor {
                        ambiguous |= sibling_counts
                            .get(&(current.parent_index, current.name.as_str()))
                            .is_some_and(|count| *count > 1);
                        segments.push(current.name.clone());
                        cursor = current
                            .parent_index
                            .and_then(|index| files.instances.get(index));
                    }
                    segments.reverse();
                    (segments, ambiguous)
                })
                .clone();
            differences.push(SavedFieldDifference {
                path_segments: segments,
                class_name: class_name.clone(),
                property: name.clone(),
                value: value.clone(),
                ambiguous,
            });
        }
    }
    differences.sort_by(|left, right| {
        (&left.path_segments, &left.property).cmp(&(&right.path_segments, &right.property))
    });
    differences
}

pub(crate) fn remove_reconciliation_derived_properties(document: &mut SettingsBytecode) {
    let remove = |instance: &mut SettingsBytecodeInstance| {
        instance
            .properties
            .retain(|name, _| !reconciliation_property_is_derived(name));
    };
    if document.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        document.instances.par_iter_mut().for_each(remove);
    } else {
        document.instances.iter_mut().for_each(remove);
    }
}

pub(crate) fn align_equivalent_values(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) {
    let observed_by_id = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let service = store_service(reference);
    for reference_instance in &reference.instances {
        let Some(observed_index) = observed_by_id
            .get(reference_instance.settings_id.as_str())
            .copied()
        else {
            continue;
        };
        let observed_instance = &mut observed.instances[observed_index];
        align_equivalent_property_map_values(
            service,
            &reference_instance.class_name,
            &reference_instance.properties,
            &mut observed_instance.properties,
        );
        align_equivalent_map_values(
            service,
            &reference_instance.attributes,
            &mut observed_instance.attributes,
        );
    }
}

fn align_equivalent_property_map_values(
    service: Option<&str>,
    class_name: &str,
    reference: &Map<String, Value>,
    observed: &mut Map<String, Value>,
) {
    let geometry_same = geometry_sources_equal(class_name, reference, observed);
    let terrain_same = terrain_physics_caches_match(class_name, reference, observed);
    for (name, reference_value) in reference {
        let Some(observed_value) = observed.get_mut(name) else {
            continue;
        };
        if name == crate::rbx::encode::COLLISION_HULL_PROPERTY && !geometry_same {
            continue;
        }
        if reference_value != observed_value
            && (name == "PhysicsGrid" && terrain_same
                || reconciliation_store_property_values_equal(
                    service,
                    class_name,
                    name,
                    Some(reference_value),
                    Some(observed_value),
                ))
            && !reference_loses_enum_types(reference_value, observed_value)
        {
            observed_value.clone_from(reference_value);
        }
    }
}

/// True when an equivalent observed document carries nested enum types that
/// the stored reference lacks, so the store is worth rewriting.
fn document_adds_enum_types(reference: &SettingsBytecode, observed: &SettingsBytecode) -> bool {
    let observed_by_id = observed
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance))
        .collect::<AHashMap<_, _>>();
    reference
        .instances
        .par_iter()
        .enumerate()
        .any(|(index, reference_instance)| {
            observed_by_id
                .get(reference_instance.settings_id.as_str())
                .copied()
                .or_else(|| observed.instances.get(index))
                .is_some_and(|observed_instance| {
                    instance_adds_enum_types(reference_instance, observed_instance)
                })
        })
}

/// The same for two documents whose instances pair by position, whatever
/// their ids say.
fn positions_add_enum_types(reference: &SettingsBytecode, observed: &SettingsBytecode) -> bool {
    reference
        .instances
        .par_iter()
        .zip(&observed.instances)
        .any(|(reference, observed)| instance_adds_enum_types(reference, observed))
}

fn instance_adds_enum_types(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
) -> bool {
    let adds = |reference: &Map<String, Value>, observed: &Map<String, Value>| {
        observed.iter().any(|(name, observed_value)| {
            reference.get(name).is_some_and(|reference_value| {
                reference_loses_enum_types(reference_value, observed_value)
            })
        })
    };
    adds(&reference.properties, &observed.properties)
        || adds(&reference.attributes, &observed.attributes)
}

/// Stores written before nested enum items kept their enum type hold only
/// the item name; adopting such a reference value would discard the type the
/// observed value carries, so the observed value wins there.
fn reference_loses_enum_types(reference: &Value, observed: &Value) -> bool {
    match (reference, observed) {
        (Value::Object(reference), Value::Object(observed)) => {
            if observed.get("_type").and_then(Value::as_str) == Some("EnumItem")
                && reference.get("_type").and_then(Value::as_str) == Some("EnumItem")
            {
                let has_type = |map: &Map<String, Value>| {
                    map.get("enumType")
                        .and_then(Value::as_str)
                        .is_some_and(|enum_type| !enum_type.is_empty())
                };
                return has_type(observed) && !has_type(reference);
            }
            observed.iter().any(|(key, observed_value)| {
                reference.get(key).is_some_and(|reference_value| {
                    reference_loses_enum_types(reference_value, observed_value)
                })
            })
        }
        (Value::Array(reference), Value::Array(observed)) => {
            reference
                .iter()
                .zip(observed)
                .any(|(reference_value, observed_value)| {
                    reference_loses_enum_types(reference_value, observed_value)
                })
        }
        _ => false,
    }
}

fn align_equivalent_map_values(
    service: Option<&str>,
    reference: &Map<String, Value>,
    observed: &mut Map<String, Value>,
) {
    for (name, reference_value) in reference {
        let Some(observed_value) = observed.get_mut(name) else {
            continue;
        };
        if reference_value != observed_value
            && reconciliation_store_values_equal(service, reference_value, observed_value, false)
            && !reference_loses_enum_types(reference_value, observed_value)
        {
            observed_value.clone_from(reference_value);
        }
    }
}

#[cfg(test)]
#[path = "identity_sim.rs"]
mod identity_sim;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::settings::bytecode::SETTINGS_BINARY_VERSION;

    #[test]
    fn a_script_record_without_enabled_means_enabled() {
        let database = rbx_reflection_database::get().unwrap();
        for class_name in ["Script", "LocalScript"] {
            assert!(!reconciliation_property_is_unknown_when_absent(
                Some(database),
                class_name,
                "Enabled"
            ));
        }
        let disabled = Map::from_iter([("Enabled".to_string(), json!(false))]);
        assert!(!reconciliation_maps_equal("Script", &Map::new(), &disabled));
        assert!(!reconciliation_maps_equal("Script", &disabled, &Map::new()));
        let enabled = Map::from_iter([("Enabled".to_string(), json!(true))]);
        assert!(reconciliation_maps_equal("Script", &Map::new(), &enabled));
        assert!(reconciliation_maps_equal(
            "LocalScript",
            &enabled,
            &Map::new()
        ));
        assert!(!reconciliation_maps_equal("Script", &enabled, &disabled));
        // The delta planner resets only serialized fields; a push must be able
        // to re-enable a script the files no longer mark disabled.
        assert!(crate::rbx::decode::property_has_serialized_form(
            database, "Script", "Enabled"
        ));
        assert!(crate::rbx::decode::property_has_serialized_form(
            database,
            "LocalScript",
            "Enabled"
        ));
    }

    #[test]
    fn ids_are_aligned_beside_a_run_one_copy_alone_holds() {
        let store = |rows: &[(&str, &str, &str, Option<usize>)]| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: rows
                .iter()
                .map(|(id, name, class, parent)| {
                    SettingsBytecodeInstance::new(
                        (*id).into(),
                        (*name).into(),
                        (*class).into(),
                        *parent,
                    )
                })
                .collect(),
        };
        let names = |id: &str| json!({"_type": "Ref", "settingsId": id});
        let ids = |document: &SettingsBytecode| {
            document
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
                .collect::<Vec<_>>()
        };
        let files = store(&[
            ("debug:0", "Workspace", "Workspace", None),
            ("debug:1", "A", "Part", Some(0)),
            ("debug:2", "B", "Part", Some(0)),
            ("debug:3", "Under", "Folder", Some(2)),
        ]);
        // Studio opened the place again. It holds a folder the files lack,
        // under the id the files hold for another instance.
        let mut studio = store(&[
            ("debug:10", "Workspace", "Workspace", None),
            ("debug:11", "A", "Part", Some(0)),
            ("debug:2", "Gone", "Folder", Some(1)),
            ("debug:12", "B", "Part", Some(0)),
            ("debug:13", "Under", "Folder", Some(3)),
        ]);
        studio.instances[1]
            .properties
            .insert("Target".into(), names("debug:2"));
        studio.instances[2]
            .properties
            .insert("Target".into(), names("debug:12"));
        assert!(align_settings_ids_beside_run(
            &files,
            &mut studio,
            2..3,
            true
        ));
        assert_eq!(
            ids(&studio),
            ["debug:0", "debug:1", "reconcile:0", "debug:2", "debug:3"]
        );
        // Each reference names the instance it named.
        assert_eq!(
            studio.instances[1].properties["Target"],
            names("reconcile:0")
        );
        assert_eq!(studio.instances[2].properties["Target"], names("debug:2"));
        assert_eq!(studio.instances[2].parent_index, Some(1));
        assert_eq!(studio.instances[4].parent_index, Some(3));

        // The files hold an instance Studio lacks.
        let files = store(&[
            ("debug:0", "Workspace", "Workspace", None),
            ("debug:1", "A", "Part", Some(0)),
            ("editor:1", "New", "Part", Some(1)),
            ("debug:2", "B", "Part", Some(0)),
            ("debug:3", "Under", "Folder", Some(3)),
        ]);
        let mut studio = store(&[
            ("debug:10", "Workspace", "Workspace", None),
            ("debug:11", "A", "Part", Some(0)),
            ("debug:12", "B", "Part", Some(0)),
            ("debug:13", "Under", "Folder", Some(2)),
        ]);
        assert!(align_settings_ids_beside_run(
            &files,
            &mut studio,
            2..3,
            false
        ));
        assert_eq!(ids(&studio), ["debug:0", "debug:1", "debug:2", "debug:3"]);

        // An instance beside the run stands under one of the run: the run is
        // no run of instances with all under them.
        let mut studio = store(&[
            ("debug:10", "Workspace", "Workspace", None),
            ("debug:11", "Gone", "Folder", Some(0)),
            ("debug:12", "A", "Part", Some(1)),
        ]);
        let files = store(&[
            ("debug:0", "Workspace", "Workspace", None),
            ("debug:1", "A", "Part", Some(0)),
        ]);
        assert!(!align_settings_ids_beside_run(
            &files,
            &mut studio,
            1..2,
            true
        ));
    }

    /// Two copies of a store one run of instances apart pair the namesakes
    /// beside it as the matching does, which is not by their places: the
    /// files removed the first of two twins and changed the other, which
    /// holds what neither of Studio's does, or gave the first of two the
    /// same a child, which sets it apart from Studio's.
    #[test]
    fn namesakes_beside_a_run_one_copy_alone_holds_are_paired_as_the_matching_pairs_them() {
        let store = |rows: &[(&str, &str, Option<usize>, Option<&str>)]| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: rows
                .iter()
                .map(|(id, name, parent, material)| {
                    let class = if *name == "Added" { "Folder" } else { "Part" };
                    let mut instance = SettingsBytecodeInstance::new(
                        (*id).into(),
                        (*name).into(),
                        class.into(),
                        *parent,
                    );
                    if let Some(material) = material {
                        instance
                            .properties
                            .insert("Material".into(), json!(material));
                    }
                    instance
                })
                .collect(),
        };
        let ids = |document: &SettingsBytecode| {
            document
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
                .collect::<Vec<_>>()
        };
        // Studio opened the place again and numbered its instances anew.
        let files = store(&[
            ("debug:0", "Root", None, None),
            ("debug:1", "Mid", Some(0), None),
            ("debug:2", "Twin", Some(0), Some("Glass")),
        ]);
        let mut studio = store(&[
            ("debug:10", "Root", None, None),
            ("debug:11", "Twin", Some(0), Some("Wood")),
            ("debug:12", "Mid", Some(0), None),
            ("debug:13", "Twin", Some(0), Some("Neon")),
        ]);
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(ids(&studio), ["debug:0", "debug:2", "debug:1", "debug:13"]);

        let files = store(&[
            ("debug:0", "Root", None, None),
            ("debug:1", "Twin", Some(0), None),
            ("editor:1", "Added", Some(1), None),
            ("debug:2", "Twin", Some(0), None),
        ]);
        let mut studio = store(&[
            ("debug:10", "Root", None, None),
            ("debug:11", "Twin", Some(0), None),
            ("debug:12", "Twin", Some(0), None),
        ]);
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(ids(&studio), ["debug:0", "debug:2", "debug:1"]);
    }

    /// Twins beside a run whose children Studio swapped are left to the
    /// matching, which pairs them by those children, read whole or standing
    /// unread for their counterparts while the push read the children alone;
    /// so is a twin left unread beside a run that holds a namesake of it.
    #[test]
    fn twins_whose_children_studio_swapped_beside_a_run_are_left_to_the_matching() {
        let store = |rows: &[(&str, &str, Option<usize>, &str)]| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: rows
                .iter()
                .map(|(id, name, parent, material)| {
                    let class = if *name == "Added" { "Folder" } else { "Part" };
                    let mut instance = SettingsBytecodeInstance::new(
                        (*id).into(),
                        (*name).into(),
                        class.into(),
                        *parent,
                    );
                    if !material.is_empty() {
                        instance
                            .properties
                            .insert("Material".into(), json!(material));
                    }
                    instance
                })
                .collect(),
        };
        let files = store(&[
            ("debug:0", "Root", None, ""),
            ("debug:1", "Added", Some(0), ""),
            ("debug:2", "Twin", Some(0), "Glass"),
            ("debug:3", "D", Some(2), "Wood"),
            ("debug:4", "Twin", Some(0), "Glass"),
            ("debug:5", "D", Some(4), "Neon"),
        ]);
        let studio = store(&[
            ("debug:10", "Root", None, ""),
            ("debug:11", "Twin", Some(0), "Glass"),
            ("debug:12", "D", Some(1), "Neon"),
            ("debug:13", "Twin", Some(0), "Glass"),
            ("debug:14", "D", Some(3), "Wood"),
        ]);
        let mut matched = studio.clone();
        assert!(align_settings_ids_by_matching(&files, &mut matched));
        assert_eq!(
            (matched.instances.iter())
                .map(|instance| instance.settings_id.as_str())
                .collect::<Vec<_>>(),
            ["debug:0", "debug:4", "debug:5", "debug:2", "debug:3"]
        );
        let mut unread = studio.clone();
        for index in [1, 3] {
            let twin = &mut unread.instances[index];
            *twin = twin.bare();
            crate::settings::held::stand_unchanged(&mut twin.properties);
        }
        for mut observed in [studio, unread] {
            assert!(!align_settings_ids_beside_run(
                &files,
                &mut observed,
                1..2,
                false
            ));
        }

        // The files added a namesake of a twin with the child Studio gave
        // that twin, whose own values the push left unread.
        let files = store(&[
            ("debug:0", "Root", None, ""),
            ("debug:1", "A", Some(0), "Wood"),
            ("debug:2", "D", Some(1), "Wood"),
            ("debug:3", "Y", Some(0), ""),
            ("debug:4", "Added", Some(0), ""),
            ("debug:5", "A", Some(0), "Wood"),
            ("debug:6", "D", Some(5), "Neon"),
        ]);
        let studio = store(&[
            ("debug:10", "Root", None, ""),
            ("debug:11", "A", Some(0), "Wood"),
            ("debug:12", "D", Some(1), "Neon"),
            ("debug:13", "Y", Some(0), ""),
        ]);
        let mut matched = studio.clone();
        assert!(align_settings_ids_by_matching(&files, &mut matched));
        assert_eq!(matched.instances[1].settings_id, "debug:5");
        let mut unread = studio;
        let twin = &mut unread.instances[1];
        *twin = twin.bare();
        crate::settings::held::stand_unchanged(&mut twin.properties);
        assert!(!align_settings_ids_beside_run(
            &files,
            &mut unread,
            4..7,
            false
        ));
    }

    /// Twins beside a run that hold what their counterparts hold, which the
    /// viewport cameras name otherwise than their places, are left to the
    /// matching: it pairs the cameras first and the twins by what those
    /// name. Cameras that name the twins at their places leave them there.
    #[test]
    fn twins_the_viewport_cameras_name_otherwise_than_their_places_are_left_to_the_matching() {
        let store = |prefix: &str, added: bool, subject: usize| {
            let instance = |id: &str, name: &str, class: &str| {
                let id = format!("{prefix}{id}");
                let parent = (name != "Workspace").then_some(0);
                SettingsBytecodeInstance::new(id, name.into(), class.into(), parent)
            };
            let named = |id: &str| json!({"_type": "Ref", "settingsId": format!("{prefix}{id}")});
            let mut root = instance("root", "Workspace", "Workspace");
            root.properties
                .insert("CurrentCamera".into(), named("camera"));
            let mut camera = instance("camera", "Camera", "Camera");
            camera
                .properties
                .insert("CameraSubject".into(), named(&format!("a{subject}")));
            let mut instances = vec![root, camera];
            for (at, material) in ["Wood", "Neon"].into_iter().enumerate() {
                let mut twin = instance(&format!("a{at}"), "A", "Part");
                twin.properties.insert("Material".into(), json!(material));
                instances.push(twin);
            }
            if added {
                instances.push(instance("added", "Added", "Folder"));
            }
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            }
        };
        let twins = |document: &SettingsBytecode| {
            (document.instances[2..4].iter())
                .map(|instance| instance.settings_id.clone())
                .collect::<Vec<_>>()
        };
        let files = store("editor:", true, 1);
        let studio = store("debug:", false, 0);
        let mut matched = studio.clone();
        assert!(align_settings_ids_by_matching(&files, &mut matched));
        assert_eq!(twins(&matched), ["editor:a1", "editor:a0"]);
        let mut aligned = studio;
        assert!(!align_settings_ids_beside_run(
            &files,
            &mut aligned,
            4..5,
            false
        ));
        let mut aligned = store("debug:", false, 1);
        assert!(align_settings_ids_beside_run(
            &files,
            &mut aligned,
            4..5,
            false
        ));
        assert_eq!(twins(&aligned), ["editor:a0", "editor:a1"]);
    }

    /// Twins a run one copy alone holds stands among are paired by what
    /// tells them apart, not by their places: Studio removed a folder and
    /// the second of two twins or added them, kept the twin whose child the
    /// files hold the same, or kept the viewport camera of two.
    #[test]
    fn twins_beside_a_run_are_paired_by_what_tells_them_apart() {
        let store = |rows: &[(&str, &str, Option<usize>, &str)]| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: rows
                .iter()
                .map(|(id, name, parent, material)| {
                    let class = match *name {
                        "X" => "Folder",
                        "Workspace" | "Camera" => name,
                        _ => "Part",
                    };
                    let mut instance = SettingsBytecodeInstance::new(
                        (*id).into(),
                        (*name).into(),
                        class.into(),
                        *parent,
                    );
                    if !material.is_empty() {
                        instance
                            .properties
                            .insert("Material".into(), json!(material));
                    }
                    instance
                })
                .collect(),
        };
        let ids = |document: &SettingsBytecode| {
            document
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
                .collect::<Vec<_>>()
        };
        let files = store(&[
            ("editor:w", "Workspace", None, ""),
            ("editor:x", "X", Some(0), ""),
            ("editor:wood", "A", Some(0), "Wood"),
            ("editor:neon", "A", Some(0), "Neon"),
        ]);
        let mut studio = store(&[
            ("debug:0", "Workspace", None, ""),
            ("debug:1", "A", Some(0), "Wood"),
        ]);
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(ids(&studio), ["editor:w", "editor:wood"]);

        let files = store(&[
            ("editor:w", "Workspace", None, ""),
            ("editor:wood", "A", Some(0), "Wood"),
        ]);
        let mut studio = store(&[
            ("debug:0", "Workspace", None, ""),
            ("debug:1", "X", Some(0), ""),
            ("debug:2", "A", Some(0), "Wood"),
            ("debug:3", "A", Some(0), "Neon"),
        ]);
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(
            ids(&studio),
            ["editor:w", "debug:1", "editor:wood", "debug:3"]
        );

        let files = store(&[
            ("editor:w", "Workspace", None, ""),
            ("editor:x", "X", Some(0), ""),
            ("editor:a1", "A", Some(0), ""),
            ("editor:wood", "D", Some(2), "Wood"),
            ("editor:a2", "A", Some(0), ""),
            ("editor:neon", "D", Some(4), "Neon"),
        ]);
        let mut studio = store(&[
            ("debug:0", "Workspace", None, ""),
            ("debug:1", "A", Some(0), ""),
            ("debug:2", "D", Some(1), "Wood"),
        ]);
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(ids(&studio), ["editor:w", "editor:a1", "editor:wood"]);

        let viewport = |mut document: SettingsBytecode, camera: &str| {
            document.instances[0].properties.insert(
                "CurrentCamera".into(),
                json!({"_type": "Ref", "settingsId": camera}),
            );
            document
        };
        let files = viewport(
            store(&[
                ("editor:w", "Workspace", None, ""),
                ("editor:x", "X", Some(0), ""),
                ("editor:current", "Camera", Some(0), ""),
                ("editor:other", "Camera", Some(0), ""),
            ]),
            "editor:current",
        );
        let mut studio = viewport(
            store(&[
                ("debug:0", "Workspace", None, ""),
                ("debug:1", "Camera", Some(0), ""),
            ]),
            "debug:1",
        );
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(ids(&studio), ["editor:w", "editor:current"]);

        let files = viewport(
            store(&[
                ("editor:w", "Workspace", None, ""),
                ("editor:current", "Camera", Some(0), ""),
            ]),
            "editor:current",
        );
        let mut studio = viewport(
            store(&[
                ("debug:0", "Workspace", None, ""),
                ("debug:1", "X", Some(0), ""),
                ("debug:2", "Camera", Some(0), ""),
                ("debug:3", "Camera", Some(0), ""),
            ]),
            "debug:2",
        );
        assert!(align_settings_ids_to_reference(&files, &mut studio));
        assert_eq!(
            ids(&studio),
            ["editor:w", "debug:1", "editor:current", "debug:3"]
        );
    }

    /// A UniqueId of nothing but zeros is none: two of them ahead of a run
    /// do not say where it starts.
    #[test]
    fn nil_unique_ids_ahead_of_a_run_do_not_tell_it() {
        let copy = |unique_ids: &[Option<&str>]| {
            let mut instances = vec![SettingsBytecodeInstance::new(
                "w".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )];
            for (index, unique_id) in unique_ids.iter().enumerate() {
                let mut instance = SettingsBytecodeInstance::new(
                    format!("a{index}"),
                    "A".into(),
                    "Part".into(),
                    Some(0),
                );
                if let Some(unique_id) = unique_id {
                    instance.properties.insert(
                        "UniqueId".into(),
                        json!({"_type": "UniqueId", "value": unique_id}),
                    );
                }
                instances.push(instance);
            }
            instances
        };
        let (held, nil) = ("1f".repeat(16), "0".repeat(32));
        let told = |ahead: &str| {
            run_is_told(
                (2..3, true),
                &copy(&[Some(ahead), None]),
                &copy(&[Some(ahead)]),
            )
        };
        assert!(told(&held));
        assert!(!told(&nil));
    }

    #[test]
    fn instances_in_another_order_are_placed_among_their_namesakes() {
        let store = |rows: &[(&str, &str, Option<usize>)]| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: rows
                .iter()
                .enumerate()
                .map(|(index, (name, class, parent))| {
                    SettingsBytecodeInstance::new(
                        format!("debug:{index}"),
                        (*name).into(),
                        (*class).into(),
                        *parent,
                    )
                })
                .collect(),
        };
        let files = store(&[
            ("Workspace", "Workspace", None),
            ("Twin", "Part", Some(0)),
            ("Group", "Model", Some(0)),
            ("Twin", "Part", Some(2)),
            ("Twin", "Part", Some(0)),
            ("Terrain", "Terrain", Some(0)),
        ]);
        // Studio's terrain stands first, and the group ahead of the parts.
        let studio = store(&[
            ("Workspace", "Workspace", None),
            ("Terrain", "Terrain", Some(0)),
            ("Group", "Model", Some(0)),
            ("Twin", "Part", Some(2)),
            ("Twin", "Part", Some(0)),
            ("Twin", "Part", Some(0)),
        ]);
        // The parts under the service are told by their order alone; the one
        // in the group has no namesake there.
        assert_eq!(
            instance_places(&files, &studio),
            Some((
                vec![0, 4, 2, 3, 5, 1],
                vec![false, true, false, false, true, false]
            ))
        );
        assert_eq!(
            instance_places(&files, &files),
            Some(((0..6).collect::<Vec<_>>(), vec![false; 6]))
        );
        // An id of the store's that both hold names one instance.
        let (mut held, mut other) = (files.clone(), studio.clone());
        held.instances[4].settings_id = "part-b".into();
        other.instances[5].settings_id = "part-b".into();
        assert!(instance_places(&held, &other).is_some());
        other.instances[5].settings_id = "debug:9".into();
        other.instances[4].settings_id = "part-b".into();
        assert_eq!(instance_places(&held, &other), None);
        // Another class, another parent or one instance more is another store.
        let mut other = studio.clone();
        other.instances[5].class_name = "MeshPart".into();
        assert_eq!(instance_places(&files, &other), None);
        let mut other = studio.clone();
        other.instances[5].parent_index = Some(2);
        assert_eq!(instance_places(&files, &other), None);
        let mut other = studio.clone();
        other.instances.push(other.instances[5].clone());
        assert_eq!(instance_places(&files, &other), None);
        // A UniqueId that says an instance stands elsewhere is believed.
        let unique = |document: &mut SettingsBytecode, index: usize, id: u32| {
            document.instances[index].properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{id:032x}")}),
            );
        };
        let (mut files, mut studio) = (files, studio);
        unique(&mut files, 1, 7);
        unique(&mut studio, 4, 7);
        assert!(instance_places(&files, &studio).is_some());
        unique(&mut files, 4, 8);
        unique(&mut studio, 4, 8);
        assert_eq!(instance_places(&files, &studio), None);
    }

    #[test]
    fn references_into_different_services_differ_when_their_ids_match() {
        let reference = |service: &str, name: &str, id: &str| {
            json!({
                "_type": "Ref",
                "settingsId": id,
                "pathSegments": [service, name],
                "pathOrdinals": [1, 1],
            })
        };
        let document = |target_id: &str, value: &Value| {
            let instance = |id: &str, name: &str, class_name: &str, parent| {
                SettingsBytecodeInstance::new(
                    id.to_string(),
                    name.to_string(),
                    class_name.to_string(),
                    parent,
                )
            };
            let mut holder = instance("holder", "Holder", "ObjectValue", Some(0));
            holder.properties.insert("Value".to_string(), value.clone());
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    instance("ws", "Workspace", "Workspace", None),
                    instance(target_id, "A", "Folder", Some(0)),
                    holder,
                ],
            }
        };
        // Stores number their instances alike, so ReplicatedStorage.A and
        // ServerStorage.B can both be `editor:1`.
        let baseline = reference("ReplicatedStorage", "A", "editor:1");
        let edited = reference("ServerStorage", "B", "editor:1");
        assert!(!reconciliation_values_equal(&baseline, &edited, false));
        assert!(reconciliation_values_equal(&baseline, &baseline, false));
        let mut observed = document("editor:1", &edited);
        align_equivalent_values(&document("editor:1", &baseline), &mut observed);
        assert_eq!(observed.instances[2].properties["Value"], edited);

        // Two copies of one store pair their own instances by position, and
        // a reference into the store with them. An id of another store is
        // not one of theirs, also when the store has an instance with it.
        let outside = |id: &str| reference("ReplicatedStorage", "A", id);
        assert!(!settings_documents_positionally_equivalent(
            &document("first", &outside("first")),
            &document("second", &outside("second")),
        ));
        let inside = |id: &str| reference("Workspace", "A", id);
        assert!(settings_documents_positionally_equivalent(
            &document("first", &inside("first")),
            &document("second", &inside("second")),
        ));
    }

    fn reference_test_document(target_id: &str, value: &Value) -> SettingsBytecode {
        let instance = |id: &str, name: &str, class_name: &str, parent| {
            SettingsBytecodeInstance::new(
                id.to_string(),
                name.to_string(),
                class_name.to_string(),
                parent,
            )
        };
        let mut holder = instance("holder", "Holder", "ObjectValue", Some(0));
        holder.properties.insert("Value".to_string(), value.clone());
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("ws", "Workspace", "Workspace", None),
                instance(target_id, "A", "Folder", Some(0)),
                holder,
            ],
        }
    }

    fn path_reference(service: &str, name: &str, id: &str) -> Value {
        json!({
            "_type": "Ref",
            "settingsId": id,
            "pathSegments": [service, name],
            "pathOrdinals": [1, 1],
        })
    }

    #[test]
    fn a_reference_without_a_path_is_one_of_the_store_that_holds_it() {
        let document = reference_test_document;
        let local = json!({"_type": "Ref", "settingsId": "2"});
        let inside = path_reference("Workspace", "A", "2");
        let outside = path_reference("ReplicatedStorage", "Target", "2");
        let baseline = document("2", &local);

        // Workspace's `2` and ReplicatedStorage's `2` are two instances: a
        // holder retargeted from one to the other changed.
        let retargeted = document("2", &outside);
        assert!(!settings_documents_equivalent(&baseline, &retargeted));
        assert!(!settings_documents_positionally_equivalent(
            &baseline,
            &retargeted
        ));
        let mut observed = retargeted.clone();
        align_equivalent_values(&baseline, &mut observed);
        assert_eq!(observed.instances[2].properties["Value"], outside);
        assert!(matches!(
            align_settings_bytes_to_reference(
                &encode_settings_bytecode(&baseline).unwrap(),
                &encode_settings_bytecode(&retargeted).unwrap(),
            )
            .unwrap(),
            SettingsAlignment::Changed(_)
        ));

        // The same instance named with and without its path is one value.
        let spelled = document("2", &inside);
        assert!(settings_documents_equivalent(&baseline, &spelled));
        assert!(settings_documents_positionally_equivalent(
            &baseline, &spelled
        ));
        let mut observed = spelled;
        align_equivalent_values(&baseline, &mut observed);
        assert_eq!(observed.instances[2].properties["Value"], local);
    }

    fn held_value(document: &SettingsBytecode, holder: &str) -> Value {
        document
            .instances
            .iter()
            .find(|instance| instance.name == holder)
            .unwrap()
            .properties["Value"]
            .clone()
    }

    fn with_inside_holder(mut document: SettingsBytecode, target_id: &str) -> SettingsBytecode {
        let mut second = SettingsBytecodeInstance::new(
            "second".to_string(),
            "Second".to_string(),
            "ObjectValue".to_string(),
            Some(0),
        );
        second.properties.insert(
            "Value".to_string(),
            path_reference("Workspace", "A", target_id),
        );
        document.instances.push(second);
        document
    }

    fn decoded_test_document(bytes: &[u8]) -> SettingsBytecode {
        let mut document = decode_settings_bytecode(bytes).unwrap();
        stabilize_settings_reference_ids(&mut document);
        document
    }

    // In the three tests below the store knows Workspace.A as `kept` and
    // the observed copy numbers it `2`, the id ReplicatedStorage.Target has
    // in its own store and Holder refers to.

    #[test]
    fn positional_id_alignment_keeps_the_ids_of_references_into_other_services() {
        let outside = path_reference("ReplicatedStorage", "Target", "2");
        let reference = reference_test_document("kept", &outside);
        let mut observed = reference_test_document("2", &outside);
        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(observed.instances[1].settings_id, "kept");
        assert_eq!(held_value(&observed, "Holder"), outside);
    }

    #[test]
    fn id_alignment_keeps_the_ids_of_references_into_other_services() {
        let outside = path_reference("ReplicatedStorage", "Target", "2");
        let reference = with_inside_holder(reference_test_document("kept", &outside), "kept");
        let mut observed = with_inside_holder(reference_test_document("2", &outside), "2");
        observed.instances.swap(2, 3);
        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(held_value(&observed, "Holder"), outside);
        assert_eq!(held_value(&observed, "Second")["settingsId"], "kept");
    }

    #[test]
    fn contiguous_id_alignment_keeps_the_ids_of_references_into_other_services() {
        let outside = path_reference("ReplicatedStorage", "Target", "2");
        let reference = reference_test_document("kept", &outside);
        let observed = with_inside_holder(reference_test_document("2", &outside), "2");
        let SettingsAlignment::Changed(aligned) = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&reference).unwrap(),
            &encode_settings_bytecode(&observed).unwrap(),
        )
        .unwrap() else {
            panic!("an added instance must change the store");
        };
        let aligned = decoded_test_document(&aligned);
        assert_eq!(aligned.instances[1].settings_id, "kept");
        assert_eq!(held_value(&aligned, "Holder"), outside);
        assert_eq!(held_value(&aligned, "Second")["settingsId"], "kept");
    }

    /// The ids a pull of ReplicatedStorage leaves, by name. The store is as
    /// `ba --push` left it: RefTarget is the editor's `editor:6c71`, and no
    /// pull has recorded Studio's identity for it. `captured` are the
    /// children Studio has now, as (id, name).
    fn pulled_ids_after_a_push(captured: &[(&str, &str)]) -> Vec<(String, String)> {
        let instance = |id: &str, name: &str, unique_id: Option<usize>| {
            let mut instance = SettingsBytecodeInstance::new(
                id.to_string(),
                name.to_string(),
                if id == "1" { name } else { "Folder" }.to_string(),
                (id != "1").then_some(0),
            );
            if let Some(unique_id) = unique_id {
                instance.properties.insert(
                    "UniqueId".to_string(),
                    json!({"_type": "UniqueId", "value": format!("{unique_id:032x}")}),
                );
            }
            instance
        };
        let document = |children: Vec<SettingsBytecodeInstance>| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: std::iter::once(instance("1", "ReplicatedStorage", None))
                .chain(children)
                .collect(),
        };
        let store = document(vec![
            instance("debug:0_10", "Shared", Some(1)),
            instance("editor:6c71", "RefTarget", None),
        ]);
        let captured = document(
            captured
                .iter()
                .enumerate()
                .map(|(index, (id, name))| {
                    let unique_id = if *name == "Shared" { 1 } else { index + 2 };
                    instance(id, name, Some(unique_id))
                })
                .collect(),
        );
        let SettingsAlignment::Changed(aligned) = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&store).unwrap(),
            &encode_settings_bytecode(&captured).unwrap(),
        )
        .unwrap() else {
            panic!("a renamed or an added instance must change the store");
        };
        decoded_test_document(&aligned)
            .instances
            .into_iter()
            .skip(1)
            .map(|instance| (instance.name, instance.settings_id))
            .collect()
    }

    fn named_ids(ids: &[(&str, &str)]) -> Vec<(String, String)> {
        ids.iter()
            .map(|(name, id)| (name.to_string(), id.to_string()))
            .collect()
    }

    #[test]
    fn a_pull_keeps_a_pushed_identity_on_the_instance_studio_renamed() {
        // Studio renamed the pushed folder and a new folder took its name.
        // The capture names the pushed instance by its store id.
        assert_eq!(
            pulled_ids_after_a_push(&[
                ("debug:0_10", "Shared"),
                ("editor:6c71", "RefTargetMoved"),
                ("debug:0_31", "RefTarget"),
            ]),
            named_ids(&[
                ("Shared", "debug:0_10"),
                ("RefTargetMoved", "editor:6c71"),
                ("RefTarget", "debug:0_31"),
            ])
        );
        // A rename alone, and a new folder beside the pushed one.
        assert_eq!(
            pulled_ids_after_a_push(
                &[("debug:0_10", "Shared"), ("editor:6c71", "RefTargetMoved"),]
            ),
            named_ids(&[("Shared", "debug:0_10"), ("RefTargetMoved", "editor:6c71")])
        );
        assert_eq!(
            pulled_ids_after_a_push(&[
                ("debug:0_10", "Shared"),
                ("editor:6c71", "RefTarget"),
                ("debug:0_31", "Other"),
            ]),
            named_ids(&[
                ("Shared", "debug:0_10"),
                ("RefTarget", "editor:6c71"),
                ("Other", "debug:0_31"),
            ])
        );
    }

    #[test]
    fn a_pull_keeps_a_pushed_identity_when_the_new_instance_comes_first() {
        assert_eq!(
            pulled_ids_after_a_push(&[
                ("debug:0_10", "Shared"),
                ("debug:0_31", "RefTarget"),
                ("editor:6c71", "RefTargetMoved"),
            ]),
            named_ids(&[
                ("Shared", "debug:0_10"),
                ("RefTarget", "debug:0_31"),
                ("RefTargetMoved", "editor:6c71"),
            ])
        );
    }

    #[test]
    fn equivalent_store_is_rewritten_when_nested_enum_types_arrive() {
        let mut rule = SettingsBytecodeInstance::new(
            "UIListLayout".to_string(),
            "StyleRule".to_string(),
            "ReplicatedStorage/UIListLayout".to_string(),
            Some(0),
        );
        let root = SettingsBytecodeInstance::new(
            "ReplicatedStorage".to_string(),
            "ReplicatedStorage".to_string(),
            "ReplicatedStorage".to_string(),
            None,
        );
        rule.properties.insert(
            "Properties".to_string(),
            json!({"SortOrder": {"_type":"EnumItem","name":"LayoutOrder"}}),
        );
        let reference = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), rule.clone()],
        };
        rule.properties.insert(
            "Properties".to_string(),
            json!({"SortOrder": {"_type":"EnumItem","enumType":"SortOrder","name":"LayoutOrder"}}),
        );
        let observed = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root, rule],
        };
        let reference_bytes = encode_settings_bytecode(&reference).unwrap();
        let observed_bytes = encode_settings_bytecode(&observed).unwrap();
        let SettingsAlignment::Changed(upgraded) =
            align_settings_bytes_to_reference(&reference_bytes, &observed_bytes).unwrap()
        else {
            panic!("typed nested enum must rewrite the store");
        };
        let decoded = decode_settings_bytecode(&upgraded).unwrap();
        assert_eq!(
            decoded.instances[1].properties["Properties"]["SortOrder"]["enumType"],
            json!("SortOrder")
        );
        assert!(matches!(
            align_settings_bytes_to_reference(&upgraded, &observed_bytes).unwrap(),
            SettingsAlignment::Equivalent
        ));
        assert!(matches!(
            align_settings_bytes_to_reference(&upgraded, &reference_bytes).unwrap(),
            SettingsAlignment::Equivalent
        ));
    }

    #[test]
    fn alignment_keeps_nested_enum_types_the_reference_lacks() {
        let reference = json!({"SortOrder": {"_type":"EnumItem","name":"LayoutOrder"}});
        let typed =
            json!({"SortOrder": {"_type":"EnumItem","enumType":"SortOrder","name":"LayoutOrder"}});
        assert!(reference_loses_enum_types(&reference, &typed));
        assert!(!reference_loses_enum_types(&typed, &reference));
        assert!(!reference_loses_enum_types(&typed, &typed));
        let mut observed = Map::new();
        observed.insert("Properties".to_string(), typed.clone());
        let mut reference_map = Map::new();
        reference_map.insert("Properties".to_string(), reference);
        align_equivalent_property_map_values(None, "StyleRule", &reference_map, &mut observed);
        assert_eq!(observed["Properties"], typed);
    }

    #[test]
    fn exact_numeric_reconciliation_accepts_integer_float_encoding_without_rounding() {
        for (integer, float) in [
            (json!(102), json!(102.0)),
            (json!(-102), json!(-102.0)),
            (json!(0), json!(-0.0)),
            (json!(1u64 << 60), json!((1u64 << 60) as f64)),
            (json!(i64::MIN), json!(i64::MIN as f64)),
        ] {
            assert!(reconciliation_property_values_equal(
                "NumberValue",
                "Value",
                Some(&integer),
                Some(&float)
            ));
            assert!(reconciliation_values_equal(&integer, &float, false));
            assert!(reconciliation_values_equal(&float, &integer, false));
        }
        for (left, right) in [
            (json!(102), json!(102.0000000001)),
            (
                json!(9_007_199_254_740_993u64),
                json!(9_007_199_254_740_992.0),
            ),
            (json!(u64::MAX), json!(u64::MAX as f64)),
            (json!(i64::MAX), json!(i64::MAX as f64)),
            (json!(-1), json!(u64::MAX)),
        ] {
            assert!(!reconciliation_values_equal(&left, &right, false));
            assert!(!reconciliation_values_equal(&right, &left, false));
        }
    }

    fn identity_fixture() -> SettingsBytecode {
        SettingsBytecode {
            version: super::super::bytecode::SETTINGS_BINARY_VERSION,
            instances: [
                ("Workspace", "Workspace", None),
                ("Left", "Folder", Some(0)),
                ("Value", "NumberValue", Some(1)),
                ("Right", "Folder", Some(0)),
                ("Reference", "ObjectValue", Some(0)),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, class, parent))| {
                let mut instance = SettingsBytecodeInstance::new(
                    format!("debug:old:{index}"),
                    name.into(),
                    class.into(),
                    parent,
                );
                instance.properties.insert(
                    "UniqueId".into(),
                    json!({
                        "_type": "UniqueId", "value": format!("{:032x}", index + 1)
                    }),
                );
                if index == 4 {
                    instance.properties.insert(
                        "Value".into(),
                        json!({"_type":"Ref","settingsId":"debug:old:2"}),
                    );
                }
                instance
            })
            .collect(),
        }
    }

    #[test]
    fn native_identity_preserves_renamed_and_moved_objects_with_recycled_debug_ids() {
        let reference = identity_fixture();
        let mut observed = reference.clone();
        for (index, instance) in observed.instances.iter_mut().enumerate() {
            instance.settings_id = format!("debug:fresh:{index}");
        }
        observed.instances[4].properties.insert(
            "Value".into(),
            json!({"_type":"Ref","settingsId":"debug:fresh:2"}),
        );
        observed.instances[2].name = "Renamed".into();
        observed.instances[2].parent_index = Some(3);
        observed.instances[2]
            .attributes
            .insert("Changed".into(), json!(true));
        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(observed.instances[2].settings_id, "debug:old:2");
        assert_eq!(observed.instances[2].parent_index, Some(3));
        assert_eq!(
            observed.instances[4].properties["Value"]["settingsId"],
            "debug:old:2"
        );
        assert_eq!(observed.instances[2].attributes["Changed"], true);
    }

    #[test]
    fn native_identity_prevents_positional_and_contiguous_duplicate_swaps() {
        let mut reference = identity_fixture();
        reference.instances.truncate(3);
        reference.instances[1].name = "Duplicate".into();
        reference.instances[2].name = "Duplicate".into();
        reference.instances[2].class_name = "Folder".into();
        reference.instances[2].parent_index = Some(0);
        for inserted in [false, true] {
            let mut observed = reference.clone();
            observed.instances.swap(1, 2);
            for (index, instance) in observed.instances.iter_mut().enumerate() {
                instance.settings_id = format!("debug:fresh:{index}");
            }
            if inserted {
                observed.instances.push(SettingsBytecodeInstance::new(
                    "added".into(),
                    "New".into(),
                    "Folder".into(),
                    Some(0),
                ));
            }
            let SettingsAlignment::Changed(bytes) = align_settings_bytes_to_reference(
                &encode_settings_bytecode(&reference).unwrap(),
                &encode_settings_bytecode(&observed).unwrap(),
            )
            .unwrap() else {
                // Identical duplicates with preserved identities are equivalent
                // regardless of sibling enumeration when there is no addition.
                assert!(!inserted);
                assert!(align_settings_ids_to_reference(&reference, &mut observed));
                assert_eq!(observed.instances[1].settings_id, "debug:old:2");
                continue;
            };
            let aligned = decode_settings_bytecode(&bytes).unwrap();
            assert_eq!(aligned.instances[1].settings_id, "debug:old:2");
            assert_eq!(aligned.instances[2].settings_id, "debug:old:1");
        }
    }

    #[test]
    fn pull_refreshes_native_identity_metadata_without_making_it_a_push_target() {
        let reference = identity_fixture();
        let mut observed = reference.clone();
        observed.instances[2].properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId","value":"00000000000000000000000000000042"}),
        );
        assert!(settings_documents_equivalent(&reference, &observed));
        let SettingsAlignment::Changed(bytes) = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&reference).unwrap(),
            &encode_settings_bytecode(&observed).unwrap(),
        )
        .unwrap() else {
            panic!("pull discarded identity");
        };
        assert_eq!(
            decode_settings_bytecode(&bytes).unwrap().instances[2].properties["UniqueId"],
            observed.instances[2].properties["UniqueId"]
        );
    }

    #[test]
    fn selective_subtree_keys_preserve_descendant_evidence_and_skip_assigned_branches() {
        let document = SettingsBytecode {
            version: super::super::bytecode::SETTINGS_BINARY_VERSION,
            instances: [
                ("Workspace", "Workspace", None),
                ("Leaf", "Part", Some(2)),
                ("Duplicate", "Folder", Some(0)),
                ("Duplicate", "Folder", Some(0)),
                ("Leaf", "Part", Some(3)),
                ("Unrelated", "Part", Some(0)),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, class, parent))| {
                SettingsBytecodeInstance::new(index.to_string(), name.into(), class.into(), parent)
            })
            .collect(),
        };
        let mut interned = AHashMap::new();
        let all = identity_subtree_keys(&document, &mut interned, vec![true; 6]);
        assert_eq!(all[2], all[3]);
        let subset = identity_subtree_keys(
            &document,
            &mut interned,
            vec![false, false, true, false, false, false],
        );
        assert_eq!(subset, vec![0, all[1], all[2], 0, 0, 0]);
        assert_eq!(
            identity_subtree_keys(&document, &mut interned, vec![false; 6]),
            vec![0; 6]
        );
    }

    fn duplicate_geometry(count: usize) -> SettingsBytecode {
        let mut instances = vec![SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )];
        for index in 0..count {
            let mut instance = SettingsBytecodeInstance::new(
                format!("editor:{index}"),
                "Wedge".into(),
                "WedgePart".into(),
                Some(0),
            );
            instance.properties.insert(
                "Position".into(),
                json!({
                    "_type": "Vector3", "x": index as f64, "y": 0, "z": 0,
                }),
            );
            instances.push(instance);
        }
        SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances,
        }
    }

    #[test]
    fn duplicate_geometry_index_preserves_reordered_ids_and_edits() {
        let reference = duplicate_geometry(6_178);
        let mut observed = reference.clone();
        observed.instances[1..].reverse();
        for (index, instance) in observed.instances.iter_mut().enumerate() {
            instance.settings_id = format!("debug:fresh:{index}");
        }
        observed.instances[0]
            .attributes
            .insert("Edited".into(), json!(true));
        let candidates = (1..reference.instances.len()).collect::<Vec<_>>();
        let index = IdentityCandidateIndex::build(&reference, &candidates).unwrap();
        for instance in &observed.instances[1..] {
            assert_eq!(index.matching(instance).unwrap().len(), 1);
        }
        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        for instance in &observed.instances[1..] {
            let position = instance.properties["Position"]["x"].as_f64().unwrap() as usize;
            assert_eq!(instance.settings_id, format!("editor:{position}"));
        }
        assert_eq!(observed.instances[0].attributes["Edited"], true);
    }

    #[test]
    fn candidate_buckets_preserve_exhaustive_scores_and_ambiguity() {
        for with_references in [false, true] {
            let mut reference = duplicate_geometry(24);
            for (index, instance) in reference.instances.iter_mut().enumerate().skip(1) {
                instance.properties.clear();
                instance
                    .properties
                    .insert("Texture".into(), json!(format!("asset{}", index % 3)));
                instance.properties.insert(
                    "Face".into(),
                    json!({"_type":"EnumItem", "enumType":"NormalId", "value":index % 6}),
                );
            }
            if with_references {
                let target_id = reference.instances[2].settings_id.clone();
                reference.instances[1].properties.insert(
                    "Target".into(),
                    json!({"_type":"Ref", "settingsId":target_id}),
                );
            }
            let mut observed = reference.clone();
            observed.instances[3]
                .properties
                .insert("Texture".into(), json!("changed"));
            let reference_graph = build_reference_graph(&reference);
            let observed_graph = build_reference_graph(&observed);
            let candidates = (1..reference.instances.len()).collect::<Vec<_>>();
            for with_subtrees in [false, true] {
                let keys = (0..reference.instances.len())
                    .map(|index| index % 4 + 1)
                    .collect::<Vec<_>>();
                let subtree_keys = with_subtrees.then(|| (keys.clone(), keys));
                let (representatives, multiplicities) = identity_candidate_representatives(
                    &reference,
                    &reference_graph,
                    subtree_keys.as_ref().map(|(keys, _)| keys.as_slice()),
                    &candidates,
                );
                assert!(representatives.len() < candidates.len());
                assert_eq!(multiplicities.values().sum::<usize>(), candidates.len());
                for assigned in [false, true] {
                    let assignment = (0..reference.instances.len())
                        .map(|index| assigned.then_some(index))
                        .collect::<Vec<_>>();
                    let context = IdentityScoreContext {
                        reference: &reference,
                        observed: &observed,
                        reference_graph: &reference_graph,
                        observed_graph: &observed_graph,
                        assigned_reference: &assignment,
                        assigned_observed: &assignment,
                        subtree_keys: subtree_keys.as_ref(),
                    };
                    for index in &candidates {
                        let winner = |indices: &[usize], grouped: bool| {
                            let mut best = None;
                            let mut winner = None;
                            let mut tied = false;
                            for &candidate in indices {
                                if let Some(score) = identity_score(&context, candidate, *index) {
                                    if best.is_none_or(|previous| score > previous) {
                                        best = Some(score);
                                        winner = Some(candidate);
                                        tied = grouped && multiplicities[&candidate] > 1;
                                    } else if best == Some(score) {
                                        tied = true;
                                    }
                                }
                            }
                            (best, tied, if tied { None } else { winner })
                        };
                        assert!(winner(&candidates, false) == winner(&representatives, true));
                    }
                }
            }
        }
        let mut reference = duplicate_geometry(2400);
        for (index, instance) in reference.instances.iter_mut().enumerate().skip(1) {
            instance.properties.clear();
            instance.properties.insert("Face".into(), json!(index % 6));
        }
        let graph = build_reference_graph(&reference);
        let candidates = (1..reference.instances.len()).collect::<Vec<_>>();
        let (representatives, multiplicities) =
            identity_candidate_representatives(&reference, &graph, None, &candidates);
        assert_eq!(representatives.len(), 6);
        assert!(multiplicities.values().all(|count| *count == 400));
    }

    #[test]
    fn numeric_candidate_filter_is_a_superset_of_float_equality() {
        let mut reference = duplicate_geometry(80);
        for (index, instance) in reference.instances[1..].iter_mut().enumerate() {
            let value = if index % 2 == 0 { -1.0 } else { 1.0 } * 10.0_f64.powi(index as i32 - 40);
            instance.properties["Position"]["x"] = json!(value);
        }
        let candidates = (1..reference.instances.len()).collect::<Vec<_>>();
        let index = IdentityCandidateIndex::build(&reference, &candidates).unwrap();
        for candidate in &candidates {
            let original = &reference.instances[*candidate];
            let value = original.properties["Position"]["x"].as_f64().unwrap();
            for fraction in [-1.000001, -1.0, -0.999999, 0.0, 0.999999, 1.0, 1.000001] {
                let mut observed = original.clone();
                observed.properties["Position"]["x"] =
                    json!(value + fraction * 4.0 * f64::from(f32::EPSILON) * value.abs().max(1.0));
                if reconciliation_identity_maps_equal(
                    &original.properties,
                    &observed.properties,
                    true,
                ) {
                    assert!(
                        index
                            .matching(&observed)
                            .unwrap()
                            .iter()
                            .any(|(_, actual)| actual == candidate)
                    );
                }
            }
        }
        let mut enum_observed = reference.instances[1].clone();
        enum_observed.properties["Position"]["_type"] = json!("EnumItem");
        assert!(index.matching(&enum_observed).is_none());
        enum_observed.properties.clear();
        assert!(index.matching(&enum_observed).is_none());
    }

    #[test]
    fn uniform_duplicate_geometry_keeps_deterministic_ties() {
        let mut reference = duplicate_geometry(6_178);
        for instance in &mut reference.instances[1..] {
            instance.properties.clear();
        }
        let mut observed = reference.clone();
        for (index, instance) in observed.instances.iter_mut().enumerate() {
            instance.settings_id = format!("debug:{index}");
        }
        observed.instances[0]
            .attributes
            .insert("Edited".into(), json!(true));
        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        for (expected, actual) in reference.instances.iter().zip(&observed.instances) {
            assert_eq!(expected.settings_id, actual.settings_id);
        }
        assert_eq!(observed.instances[0].attributes["Edited"], true);
    }

    #[test]
    fn unchanged_duplicate_scores_survive_deep_identity_passes() {
        for referenced in [false, true] {
            let mut reference = duplicate_geometry(64);
            for instance in &mut reference.instances[1..] {
                instance.properties.clear();
            }
            let mut parents = [0, 0];
            for depth in 0..8 {
                for (side, parent) in parents.iter_mut().enumerate() {
                    let mut child = SettingsBytecodeInstance::new(
                        format!("branch:{depth}:{side}"),
                        "Branch".into(),
                        "Folder".into(),
                        Some(*parent),
                    );
                    child.attributes.insert("Side".into(), json!(side));
                    *parent = reference.instances.len();
                    reference.instances.push(child);
                }
            }
            if referenced {
                let mut holder = SettingsBytecodeInstance::new(
                    "holder".into(),
                    "Holder".into(),
                    "ObjectValue".into(),
                    Some(parents[0]),
                );
                holder.properties.insert(
                    "Value".into(),
                    json!({"_type":"Ref","settingsId":"editor:0"}),
                );
                reference.instances.push(holder);
            }
            let mut observed = reference.clone();
            let mut remap = HashMap::new();
            for (index, instance) in observed.instances.iter_mut().enumerate() {
                let next_id = format!("observed:{index}");
                remap.insert(instance.settings_id.clone(), next_id.clone());
                instance.settings_id = next_id;
            }
            for instance in &mut observed.instances {
                remap_record_reference_ids(&mut instance.properties, &remap, None);
            }
            if referenced {
                observed.instances.swap(1, 2);
            }
            observed.instances.push(SettingsBytecodeInstance::new(
                "added".into(),
                "New".into(),
                "Folder".into(),
                Some(0),
            ));
            assert!(align_settings_ids_to_reference(&reference, &mut observed));
            for index in 0..reference.instances.len() {
                let expected = if referenced && index == 1 {
                    2
                } else if referenced && index == 2 {
                    1
                } else {
                    index
                };
                assert_eq!(
                    observed.instances[index].settings_id,
                    reference.instances[expected].settings_id,
                    "referenced={referenced}, index={index}"
                );
            }
            if referenced {
                assert_eq!(
                    observed.instances[reference.instances.len() - 1].properties["Value"]["settingsId"],
                    "editor:0"
                );
            }
        }
    }

    #[test]
    fn indexed_duplicates_still_prioritize_reference_evidence_over_content() {
        let mut reference = duplicate_geometry(40);
        let mut holder = SettingsBytecodeInstance::new(
            "holder".into(),
            "Holder".into(),
            "ObjectValue".into(),
            Some(0),
        );
        holder.properties.insert(
            "Value".into(),
            json!({"_type": "Ref", "settingsId": "editor:0"}),
        );
        reference.instances.push(holder);
        let mut observed = reference.clone();
        for (index, instance) in observed.instances[1..41].iter_mut().enumerate() {
            instance.settings_id = format!("debug:{index}");
        }
        observed.instances[1].properties["Position"]["x"] = json!(999.0);
        observed.instances[41].properties["Value"]["settingsId"] = json!("debug:0");
        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(observed.instances[1].settings_id, "editor:0");
        assert_eq!(observed.instances[1].properties["Position"]["x"], 999.0);
        assert_eq!(
            observed.instances[41].properties["Value"]["settingsId"],
            "editor:0"
        );
    }

    fn string_value_with_properties(properties: Map<String, Value>) -> SettingsBytecode {
        SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![crate::settings::bytecode::SettingsBytecodeInstance {
                settings_id: "probe".to_string(),
                name: "Probe".to_string(),
                class_name: "StringValue".to_string(),
                parent_index: None,
                properties,
                attributes: Map::new(),
            }],
        }
    }

    #[test]
    fn serialized_aliases_resolve_their_reflection_defaults() {
        let database = rbx_reflection_database::get().unwrap();
        for class_name in ["Frame", "TextButton", "TextLabel"] {
            let default = reflection_default_settings_value(database, class_name, "InputSink")
                .expect("InputSink default");
            assert!(reconciliation_property_value_is_default(
                class_name,
                "InputSink",
                &default
            ));
            assert!(!reconciliation_property_value_is_default(
                class_name,
                "InputSink",
                &json!({"_type": "Enum", "value": 1})
            ));
        }
        assert!(reconciliation_property_value_is_default(
            "InputContext",
            "Sink",
            &json!(false)
        ));
        assert!(reflection_default_settings_value(database, "Frame", "NoSuchProperty").is_none());
        assert!(reconciliation_property_is_derived("InertiaMigrated"));
        assert!(reconciliation_property_is_forced(
            "TextWrapped",
            &Map::from_iter([("TextScaled".to_string(), json!(true))])
        ));
        assert!(!reconciliation_property_is_forced(
            "TextWrapped",
            &Map::from_iter([("TextScaled".to_string(), json!(false))])
        ));
    }

    #[test]
    fn content_uri_aliases_preserve_stores_in_both_directions_and_keep_object_content() {
        for (class, modern, legacy) in [
            ("SurfaceAppearance", "TexturePackContent", "TexturePack"),
            ("MaterialVariant", "TexturePackContent", "TexturePack"),
            ("TerrainDetail", "TexturePackContent", "TexturePack"),
            ("Decal", "TexturePackContent", "TexturePack"),
            ("UnionOperation", "Content", "AssetId"),
            ("NegateOperation", "Content", "AssetId"),
            ("IntersectOperation", "Content", "AssetId"),
            ("Shirt", "ShirtTemplateContent", "ShirtTemplate"),
            ("Pants", "PantsTemplateContent", "PantsTemplate"),
        ] {
            let document = |properties| {
                let mut document = string_value_with_properties(properties);
                document.instances[0].class_name = class.into();
                document
            };
            for uri in ["", "rbxassetid://7"] {
                let old = document(Map::from_iter([(legacy.into(), json!(uri))]));
                for value in [json!(uri), json!({"_type": "Content", "value": uri})] {
                    let new = document(Map::from_iter([
                        (modern.into(), value),
                        (legacy.into(), json!("")),
                    ]));
                    let old_bytes = encode_settings_bytecode(&old).unwrap();
                    let new_bytes = encode_settings_bytecode(&new).unwrap();
                    for (reference, observed) in
                        [(&old_bytes, &new_bytes), (&new_bytes, &old_bytes)]
                    {
                        assert!(
                            matches!(
                                align_settings_bytes_to_reference(reference, observed).unwrap(),
                                SettingsAlignment::Equivalent
                            ),
                            "{class}"
                        );
                    }
                    let mut normalized = new;
                    canonicalize_settings_property_names(&mut normalized).unwrap();
                    assert_eq!(
                        normalized.instances[0].properties,
                        old.instances[0].properties
                    );
                }
            }
            for empty in [json!(""), json!({"_type": "Content", "value": ""})] {
                let mut mirrored = document(Map::from_iter([
                    (modern.into(), empty),
                    (legacy.into(), json!("rbxassetid://7")),
                ]));
                canonicalize_settings_property_names(&mut mirrored).unwrap();
                assert_eq!(
                    mirrored.instances[0].properties,
                    Map::from_iter([(legacy.to_string(), json!("rbxassetid://7"))]),
                    "{class}"
                );
            }
            let mut object = document(Map::from_iter([(
                modern.into(),
                json!({"_type": "Ref", "pathSegments": ["Workspace", "Mesh"]}),
            )]));
            let original = object.instances[0].properties.clone();
            canonicalize_settings_property_names(&mut object).unwrap();
            assert_eq!(object.instances[0].properties, original);
            let mut conflicting = document(Map::from_iter([
                (legacy.into(), json!("rbxassetid://1")),
                (modern.into(), json!("rbxassetid://2")),
            ]));
            assert!(canonicalize_settings_property_names(&mut conflicting).is_err());
        }
        let mut unrelated =
            string_value_with_properties(Map::from_iter([("Content".into(), json!("authored"))]));
        canonicalize_settings_property_names(&mut unrelated).unwrap();
        assert_eq!(unrelated.instances[0].properties["Content"], "authored");
    }

    #[test]
    fn legacy_migrated_properties_canonicalize_to_their_replacements() {
        let corner = |properties: Map<String, Value>| SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![crate::settings::bytecode::SettingsBytecodeInstance {
                settings_id: "corner".to_string(),
                name: "Circle".to_string(),
                class_name: "UICorner".to_string(),
                parent_index: None,
                properties,
                attributes: Map::new(),
            }],
        };
        let radius = |scale: f64| json!({"_type": "UDim", "scale": scale, "offset": 0});
        let corners = [
            "TopLeftRadius",
            "TopRightRadius",
            "BottomLeftRadius",
            "BottomRightRadius",
        ];

        let mut mixed = corner(Map::from_iter(
            [("CornerRadius", radius(1.0))]
                .into_iter()
                .chain(corners.iter().map(|name| (*name, radius(0.5))))
                .map(|(name, value)| (name.to_string(), value)),
        ));
        canonicalize_settings_property_names(&mut mixed).unwrap();
        assert_eq!(
            mixed.instances[0].properties,
            Map::from_iter(corners.iter().map(|name| (name.to_string(), radius(0.5))))
        );

        let mut legacy = corner(Map::from_iter([("CornerRadius".to_string(), radius(1.0))]));
        canonicalize_settings_property_names(&mut legacy).unwrap();
        assert_eq!(
            legacy.instances[0].properties,
            Map::from_iter(corners.iter().map(|name| (name.to_string(), radius(1.0))))
        );

        let mut font = string_value_with_properties(Map::new());
        font.instances[0].class_name = "TextLabel".to_string();
        font.instances[0]
            .properties
            .insert("Font".to_string(), json!({"_type": "Enum", "value": 3}));
        canonicalize_settings_property_names(&mut font).unwrap();
        assert!(!font.instances[0].properties.contains_key("Font"));
        assert_eq!(
            font.instances[0].properties["FontFace"]["family"],
            json!("rbxasset://fonts/families/SourceSansPro.json")
        );
    }

    #[test]
    fn parallel_canonicalization_preserves_aliases_across_chunk_boundaries() {
        let seed = string_value_with_properties(Map::from_iter([
            ("archivable".into(), json!(false)),
            ("Value".into(), json!("saved")),
        ]));
        let mut document = seed.clone();
        document.instances = vec![seed.instances[0].clone(); 8193];
        let expected = Map::from_iter([
            ("Archivable".into(), json!(false)),
            ("Value".into(), json!("saved")),
        ]);
        for workers in [1, 4] {
            let mut current = document.clone();
            rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap()
                .install(|| canonicalize_settings_property_names(&mut current))
                .unwrap();
            assert!(
                current
                    .instances
                    .iter()
                    .all(|instance| instance.properties == expected)
            );
            current.instances[8192]
                .properties
                .insert("archivable".into(), json!(true));
            assert!(canonicalize_settings_property_names(&mut current).is_err());
        }
    }

    #[test]
    fn conflicting_property_aliases_are_rejected() {
        let mut document = string_value_with_properties(Map::from_iter([
            ("Archivable".to_string(), json!(true)),
            ("archivable".to_string(), json!(false)),
        ]));

        let error = canonicalize_settings_property_names(&mut document).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicting values for property Archivable")
        );
    }

    #[test]
    fn canonical_properties_keep_their_storage_and_alias_cache_is_class_scoped() {
        let mut document = string_value_with_properties(Map::from_iter([
            ("Archivable".to_string(), json!(false)),
            ("Value".to_string(), json!("saved")),
            ("FutureProperty".to_string(), json!({"future": true})),
        ]));
        let key = document.instances[0]
            .properties
            .keys()
            .next()
            .unwrap()
            .as_ptr();
        let mut alias = document.instances[0].clone();
        alias.properties.insert("archivable".into(), json!(false));
        document.instances.push(alias);
        let mut unknown_class = document.instances[0].clone();
        unknown_class.class_name = "FutureClass".into();
        unknown_class
            .properties
            .insert("archivable".into(), json!(true));
        document.instances.push(unknown_class);

        canonicalize_settings_property_names(&mut document).unwrap();

        assert_eq!(
            document.instances[0]
                .properties
                .keys()
                .next()
                .unwrap()
                .as_ptr(),
            key
        );
        assert_eq!(
            document.instances[0].properties,
            document.instances[1].properties
        );
        assert_eq!(document.instances[2].properties["archivable"], true);
        assert_eq!(document.instances[2].properties["Archivable"], false);
    }

    #[test]
    fn aligned_settings_bytes_preserve_equivalent_reference_bytes() {
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                crate::settings::bytecode::SettingsBytecodeInstance::new(
                    "1".to_string(),
                    "ReplicatedStorage".to_string(),
                    "ReplicatedStorage".to_string(),
                    None,
                ),
                crate::settings::bytecode::SettingsBytecodeInstance::new(
                    "editor:value".to_string(),
                    "Value".to_string(),
                    "StringValue".to_string(),
                    Some(0),
                ),
            ],
        };
        let mut observed = reference.clone();
        observed.instances[1].settings_id = "debug:0_12".to_string();
        let reference_bytes = encode_settings_bytecode(&reference).unwrap();
        let observed_bytes = encode_settings_bytecode(&observed).unwrap();

        assert!(matches!(
            align_settings_bytes_to_reference(&reference_bytes, &observed_bytes).unwrap(),
            SettingsAlignment::Equivalent
        ));
    }

    #[test]
    fn positional_suffix_removal_preserves_surviving_ids_and_references() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "1".to_string(),
            "ServerStorage".to_string(),
            "ServerStorage".to_string(),
            None,
        );
        let target = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:target".to_string(),
            "Target".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let mut holder = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:holder".to_string(),
            "Holder".to_string(),
            "ObjectValue".to_string(),
            Some(0),
        );
        holder.properties.insert(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "editor:target"}),
        );
        let removed = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:removed".to_string(),
            "Removed".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), target.clone(), holder.clone(), removed],
        };
        let mut observed_target = target;
        observed_target.settings_id = "debug:0_12".to_string();
        let mut observed_holder = holder;
        observed_holder.settings_id = "debug:0_13".to_string();
        observed_holder.properties.insert(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "debug:0_12"}),
        );
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, observed_target, observed_holder],
        };

        let SettingsAlignment::Changed(aligned) = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&reference).unwrap(),
            &encode_settings_bytecode(&observed).unwrap(),
        )
        .unwrap() else {
            panic!("suffix removal must produce changed settings bytes");
        };
        let mut aligned = decode_settings_bytecode(&aligned).unwrap();
        stabilize_settings_reference_ids(&mut aligned);

        assert_eq!(aligned.instances.len(), 3);
        assert_eq!(aligned.instances[1].settings_id, "editor:target");
        assert_eq!(aligned.instances[2].settings_id, "editor:holder");
        assert_eq!(
            aligned.instances[2].properties["Value"]["settingsId"],
            "editor:target"
        );
    }

    #[test]
    fn middle_insertion_preserves_surviving_ids_and_references() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "root".to_string(),
            "ServerStorage".to_string(),
            "ServerStorage".to_string(),
            None,
        );
        let target = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:target".to_string(),
            "Target".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let mut holder = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:holder".to_string(),
            "Holder".to_string(),
            "ObjectValue".to_string(),
            Some(0),
        );
        holder.properties.insert(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "editor:target"}),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), target.clone(), holder.clone()],
        };
        let inserted = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "debug:inserted".to_string(),
            "Inserted".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let mut observed_target = target;
        observed_target.settings_id = "debug:target".to_string();
        let mut observed_holder = holder;
        observed_holder.settings_id = "debug:holder".to_string();
        observed_holder.properties.insert(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "debug:target"}),
        );
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, inserted, observed_target, observed_holder],
        };

        let SettingsAlignment::Changed(aligned) = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&reference).unwrap(),
            &encode_settings_bytecode(&observed).unwrap(),
        )
        .unwrap() else {
            panic!("middle insertion must produce changed settings bytes");
        };
        let mut aligned = decode_settings_bytecode(&aligned).unwrap();
        stabilize_settings_reference_ids(&mut aligned);

        assert_eq!(aligned.instances[1].settings_id, "debug:inserted");
        assert_eq!(aligned.instances[2].settings_id, "editor:target");
        assert_eq!(aligned.instances[3].settings_id, "editor:holder");
        assert_eq!(
            aligned.instances[3].properties["Value"]["settingsId"],
            "editor:target"
        );
    }

    #[test]
    fn indistinguishable_duplicate_siblings_use_a_deterministic_fallback() {
        let mut reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![crate::settings::bytecode::SettingsBytecodeInstance::new(
                "1".to_string(),
                "Workspace".to_string(),
                "Workspace".to_string(),
                None,
            )],
        };
        for id in ["editor:first", "editor:second"] {
            reference
                .instances
                .push(crate::settings::bytecode::SettingsBytecodeInstance::new(
                    id.to_string(),
                    "Attachment".to_string(),
                    "Attachment".to_string(),
                    Some(0),
                ));
        }
        let mut observed = reference.clone();
        observed.instances[1].settings_id = "debug:0_12".to_string();
        observed.instances[2].settings_id = "debug:0_13".to_string();
        let reference_bytes = encode_settings_bytecode(&reference).unwrap();
        let observed_bytes = encode_settings_bytecode(&observed).unwrap();

        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(
            observed
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
                .collect::<Vec<_>>(),
            ["1", "editor:first", "editor:second"]
        );
        assert!(matches!(
            align_settings_bytes_to_reference(&reference_bytes, &observed_bytes).unwrap(),
            SettingsAlignment::Equivalent
        ));
    }

    #[test]
    fn duplicate_siblings_align_by_outgoing_references() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "root".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        let target = |id: &str, name: &str| {
            crate::settings::bytecode::SettingsBytecodeInstance::new(
                id.to_string(),
                name.to_string(),
                "Folder".to_string(),
                Some(0),
            )
        };
        let holder = |id: &str, target: &str| {
            let mut instance = crate::settings::bytecode::SettingsBytecodeInstance::new(
                id.to_string(),
                "Duplicate".to_string(),
                "ObjectValue".to_string(),
                Some(0),
            );
            instance.properties.insert(
                "Value".to_string(),
                json!({"_type": "Ref", "settingsId": target}),
            );
            instance
        };
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root.clone(),
                target("editor:target-a", "TargetA"),
                target("editor:target-b", "TargetB"),
                holder("editor:holder-a", "editor:target-a"),
                holder("editor:holder-b", "editor:target-b"),
            ],
        };
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root,
                target("debug:target-a", "TargetA"),
                target("debug:target-b", "TargetB"),
                holder("debug:holder-b", "debug:target-b"),
                holder("debug:holder-a", "debug:target-a"),
            ],
        };

        assert!(matches!(
            align_settings_bytes_to_reference(
                &encode_settings_bytecode(&reference).unwrap(),
                &encode_settings_bytecode(&observed).unwrap(),
            )
            .unwrap(),
            SettingsAlignment::Equivalent
        ));
    }

    #[test]
    fn duplicate_siblings_align_by_incoming_references() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "root".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        let duplicate = |id: &str| {
            crate::settings::bytecode::SettingsBytecodeInstance::new(
                id.to_string(),
                "Duplicate".to_string(),
                "Folder".to_string(),
                Some(0),
            )
        };
        let holder = |id: &str, name: &str, target: &str| {
            let mut instance = crate::settings::bytecode::SettingsBytecodeInstance::new(
                id.to_string(),
                name.to_string(),
                "ObjectValue".to_string(),
                Some(0),
            );
            instance.properties.insert(
                "Value".to_string(),
                json!({"_type": "Ref", "settingsId": target}),
            );
            instance
        };
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root.clone(),
                duplicate("editor:duplicate-a"),
                duplicate("editor:duplicate-b"),
                holder("editor:holder-a", "HolderA", "editor:duplicate-a"),
                holder("editor:holder-b", "HolderB", "editor:duplicate-b"),
            ],
        };
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root,
                duplicate("debug:duplicate-b"),
                duplicate("debug:duplicate-a"),
                holder("debug:holder-a", "HolderA", "debug:duplicate-a"),
                holder("debug:holder-b", "HolderB", "debug:duplicate-b"),
            ],
        };

        assert!(matches!(
            align_settings_bytes_to_reference(
                &encode_settings_bytecode(&reference).unwrap(),
                &encode_settings_bytecode(&observed).unwrap(),
            )
            .unwrap(),
            SettingsAlignment::Equivalent
        ));
    }

    #[test]
    fn duplicate_containers_align_by_distinct_descendant_content() {
        for count in [2, 2_000] {
            let mut reference = duplicate_geometry(count);
            for instance in &mut reference.instances[1..] {
                instance.name = "Wood Crate".into();
                instance.class_name = "Folder".into();
                instance.properties.clear();
            }
            for index in 1..=count {
                let mut child = SettingsBytecodeInstance::new(
                    format!("editor:child-{index}"),
                    "Part".into(),
                    "Part".into(),
                    Some(index),
                );
                child.properties.insert(
                    "Position".into(),
                    json!({"_type":"Vector3", "x":index, "y":0, "z":0}),
                );
                reference.instances.push(child);
            }
            let mut observed = reference.clone();
            observed.instances[1..=count].reverse();
            for index in 1..=count {
                observed.instances[count + index].parent_index = Some(count + 1 - index);
            }
            for (index, instance) in observed.instances.iter_mut().enumerate() {
                instance.settings_id = format!("debug:{index}");
            }
            assert!(align_settings_ids_to_reference(&reference, &mut observed));
            assert_eq!(
                observed.instances[1].settings_id,
                reference.instances[count].settings_id
            );
            assert!(settings_documents_equivalent(&reference, &observed));
        }
    }

    #[test]
    fn stable_reference_identity_ignores_stale_fallback_paths() {
        let before = json!({
            "_type": "Ref",
            "settingsId": "target",
            "pathSegments": ["StarterGui", "Before"],
            "pathOrdinals": [1, 1],
        });
        let after = json!({
            "_type": "Ref",
            "settingsId": "target",
            "pathSegments": ["StarterGui", "After"],
            "pathOrdinals": [1, 1],
        });
        assert!(reconciliation_values_equal(&before, &after, false));
    }

    #[test]
    fn exact_reference_path_matches_when_only_one_side_has_a_stable_id() {
        let with_id = json!({
            "_type": "Ref",
            "settingsId": "target",
            "pathSegments": ["StarterGui", "Target"],
            "pathOrdinals": [1, 1],
        });
        let without_id = json!({
            "_type": "Ref",
            "pathSegments": ["StarterGui", "Target"],
            "pathOrdinals": [1, 1],
        });
        assert!(reconciliation_values_equal(&with_id, &without_id, false));
    }

    #[test]
    fn pull_refreshes_viewport_values_without_making_them_push_targets() {
        let mut root = SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        );
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type":"Ref","settingsId":"camera"}),
        );
        let mut camera = SettingsBytecodeInstance::new(
            "camera".into(),
            "Camera".into(),
            "Camera".into(),
            Some(0),
        );
        camera.properties.insert("FieldOfView".into(), json!(70));
        camera.attributes.insert("Revision".into(), json!(1));
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root,
                camera,
                SettingsBytecodeInstance::new(
                    "other".into(),
                    "Other".into(),
                    "Folder".into(),
                    Some(0),
                ),
            ],
        };
        let original = encode_settings_bytecode(&reference).unwrap();
        for reorder in [false, true] {
            let mut observed = reference.clone();
            observed.instances[1]
                .properties
                .insert("FieldOfView".into(), json!(85));
            observed.instances[1]
                .attributes
                .insert("Revision".into(), json!(2));
            if reorder {
                observed.instances.swap(1, 2);
            }
            assert!(settings_documents_equivalent(&reference, &observed));
            let bytes = encode_settings_bytecode(&observed).unwrap();
            let SettingsAlignment::Changed(aligned) =
                align_settings_bytes_to_reference(&original, &bytes).unwrap()
            else {
                panic!("Pull discarded the changed viewport state");
            };
            let aligned = decode_settings_bytecode(&aligned).unwrap();
            let camera = aligned
                .instances
                .iter()
                .find(|instance| instance.class_name == "Camera")
                .unwrap();
            assert_eq!(camera.properties["FieldOfView"], json!(85));
            assert_eq!(camera.attributes["Revision"], json!(2));
            assert!(matches!(
                align_settings_bytes_to_reference(&bytes, &bytes).unwrap(),
                SettingsAlignment::Equivalent
            ));
        }
    }

    #[test]
    fn workspace_camera_viewport_values_do_not_affect_reconciliation() {
        let mut root = SettingsBytecodeInstance::new(
            "root".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "settingsId": "camera"}),
        );
        let mut camera = SettingsBytecodeInstance::new(
            "camera".to_string(),
            "Camera".to_string(),
            "Camera".to_string(),
            Some(0),
        );
        camera.properties.insert(
            "CFrame".to_string(),
            json!({"_type": "CFrame", "components": [0.0, 1.0, 2.0]}),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), camera.clone()],
        };
        let mut observed = reference.clone();
        observed.instances[1].properties.insert(
            "CFrame".to_string(),
            json!({"_type": "CFrame", "components": [100.0, 200.0, 300.0]}),
        );

        assert!(settings_documents_positionally_equivalent(
            &reference, &observed
        ));
        assert!(settings_documents_equivalent(&reference, &observed));
        align_reconciliation_protected_workspace_cameras(&reference, &mut observed);
        assert_eq!(
            observed.instances[1].properties,
            reference.instances[1].properties
        );

        let folder = SettingsBytecodeInstance::new(
            "folder".to_string(),
            "Folder".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        camera.parent_index = Some(1);
        let nested_reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, folder, camera],
        };
        let mut nested_observed = nested_reference.clone();
        nested_observed.instances[2].properties.insert(
            "CFrame".to_string(),
            json!({"_type": "CFrame", "components": [9.0]}),
        );
        assert!(settings_documents_equivalent(
            &nested_reference,
            &nested_observed
        ));
        nested_observed.instances[0]
            .properties
            .remove("CurrentCamera");
        assert!(!settings_documents_equivalent(
            &nested_reference,
            &nested_observed
        ));
    }

    #[test]
    fn legacy_viewport_metadata_requires_a_matched_saved_identity() {
        let mut root = SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        );
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "settingsId": "active"}),
        );
        let active = SettingsBytecodeInstance::new(
            "active".into(),
            "Camera".into(),
            "Camera".into(),
            Some(0),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, active],
        };
        let mut legacy = reference.clone();
        legacy.instances[0].properties.remove("CurrentCamera");
        assert!(!settings_documents_positionally_equivalent(
            &legacy, &reference
        ));
        assert!(!settings_documents_equivalent(&legacy, &reference));
        let SettingsAlignment::Changed(upgraded) = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&legacy).unwrap(),
            &encode_settings_bytecode(&reference).unwrap(),
        )
        .unwrap() else {
            panic!("A metadata-only pull must persist the viewport role");
        };
        let upgraded = decode_settings_bytecode(&upgraded).unwrap();
        assert!(is_reconciliation_protected_workspace_camera(&upgraded, 1));
        inherit_workspace_viewport_reference(&mut legacy, &reference);
        assert_eq!(
            legacy.instances[0].properties,
            reference.instances[0].properties
        );
        legacy.instances[0].properties.remove("CurrentCamera");
        legacy.instances[1].settings_id = "different-camera-with-the-same-name".into();
        inherit_workspace_viewport_reference(&mut legacy, &reference);
        assert!(!legacy.instances[0].properties.contains_key("CurrentCamera"));
    }

    #[test]
    fn viewport_role_precedes_duplicate_names_ordinals_and_recycled_ids() {
        for name in ["Camera", "CurrentCamera", "Viewport"] {
            let mut root = SettingsBytecodeInstance::new(
                "root".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            );
            root.properties.insert(
                "CurrentCamera".into(),
                json!({"_type": "Ref", "settingsId": "active"}),
            );
            let active = SettingsBytecodeInstance::new(
                "active".into(),
                name.into(),
                "Camera".into(),
                Some(0),
            );
            let mut extra = active.clone();
            extra.settings_id = "extra".into();
            let reference = SettingsBytecode {
                version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                instances: vec![root, active, extra],
            };
            let mut observed = reference.clone();
            // A fresh runtime enumerates the viewport second and recycles the
            // old ID for the other identically named camera.
            observed.instances[0].properties.insert(
                "CurrentCamera".into(),
                json!({"_type": "Ref", "settingsId": "extra"}),
            );
            observed.instances[2]
                .properties
                .insert("FieldOfView".into(), json!(85));
            assert!(!settings_documents_positionally_equivalent(
                &reference, &observed
            ));
            assert!(align_settings_ids_to_reference(&reference, &mut observed));
            assert_eq!(observed.instances[2].settings_id, "active");
            assert_eq!(observed.instances[1].settings_id, "extra");
            assert!(is_reconciliation_protected_workspace_camera(&observed, 2));
            assert!(!is_reconciliation_protected_workspace_camera(&observed, 1));
            assert!(settings_documents_equivalent(&reference, &observed));
            observed.instances[1]
                .properties
                .insert("FieldOfView".into(), json!(40));
            assert!(!settings_documents_equivalent(&reference, &observed));
            let bytes = encode_settings_bytecode_with_dense_references(&observed).unwrap();
            let decoded = decode_settings_bytecode(&bytes).unwrap();
            assert!(is_reconciliation_protected_workspace_camera(&decoded, 2));
            assert!(!is_reconciliation_protected_workspace_camera(&decoded, 1));
        }
    }

    #[test]
    fn cframe_rotation_roundtrip_preserves_position_and_real_edit_detection() {
        let expected = json!({"_type": "CFrame", "components":
            [0, 10, 500, 1, 0, 0, 0, 1, 0, 0, 0, 1]});
        for drift in [
            1.1920928955078125e-6,
            1.6689300537109375e-6,
            8.940696716308594e-6,
        ] {
            let mut observed = expected.clone();
            observed["components"][3] = json!(1.0 + drift);
            observed["components"][11] = json!(1.0 + drift);
            assert!(reconciliation_property_values_equal(
                "Part",
                "CFrame",
                Some(&expected),
                Some(&observed),
            ));
            assert!(reconciliation_values_equal(&observed, &expected, false));
            observed["components"][0] = json!(0.001);
            assert!(!reconciliation_values_equal(&expected, &observed, false));
            assert!(!reconciliation_values_equal(
                &json!(1.0),
                &json!(1.0 + drift),
                true
            ));
        }
        for (index, value) in [(3, 1.00001), (4, 0.00001), (0, 0.001), (2, 500.001)] {
            let mut edited = expected.clone();
            edited["components"][index] = json!(value);
            assert!(!reconciliation_values_equal(&expected, &edited, false));
        }
        let mut malformed = expected.clone();
        malformed["components"][3] = Value::Null;
        assert!(!reconciliation_values_equal(&expected, &malformed, false));
        malformed["components"] = json!([0, 10, 500]);
        assert!(!reconciliation_values_equal(&expected, &malformed, false));
    }

    #[test]
    fn cframe_preserves_encoded_nonfinite_components_without_hiding_edits() {
        for token in ["nan", "inf", "-inf"] {
            let value = json!({"_type": "CFrame", "components":
                vec![json!({"_type": "Float", "value": token}); 12]});
            assert!(reconciliation_property_values_equal(
                "Model",
                "ModelMeshCFrame",
                Some(&value),
                Some(&value),
            ));
            for index in 0..12 {
                for replacement in [
                    json!(0),
                    json!(null),
                    json!({"_type":"Float", "value":"invalid"}),
                    json!({"_type":"Float", "value":if token == "nan" {"inf"} else {"nan"}}),
                ] {
                    let mut changed = value.clone();
                    changed["components"][index] = replacement;
                    assert!(!reconciliation_values_equal(&value, &changed, false));
                    assert!(!reconciliation_values_equal(&changed, &value, false));
                }
            }
            let mut mixed = value.clone();
            mixed["components"][0] = json!(10);
            mixed["components"][3] = json!(1);
            let mut rounded = mixed.clone();
            rounded["components"][3] = json!(1.0000016689300537);
            assert!(reconciliation_values_equal(&mixed, &rounded, false));
            rounded["components"][0] = json!(10.001);
            assert!(!reconciliation_values_equal(&mixed, &rounded, false));
        }
    }

    #[test]
    fn numeric_candidate_filter_includes_cframe_rotation_roundtrip() {
        let mut reference = duplicate_geometry(80);
        for (index, instance) in reference.instances[1..].iter_mut().enumerate() {
            instance.properties.clear();
            instance.properties.insert(
                "CFrame".into(),
                json!({"_type": "CFrame",
                "components": [0, 0, 0, 1.0 - index as f64 * 0.0001, 0, 0, 0, 1, 0, 0, 0, 1]}),
            );
        }
        let candidates = (1..reference.instances.len()).collect::<Vec<_>>();
        let index = IdentityCandidateIndex::build(&reference, &candidates).unwrap();
        assert_eq!(index.field.pointer, "/components/3");
        for candidate in candidates {
            let original = &reference.instances[candidate];
            for drift in [-CFRAME_ROTATION_EPSILON, CFRAME_ROTATION_EPSILON] {
                let mut observed = original.clone();
                observed.properties["CFrame"]["components"][3] = json!(
                    original.properties["CFrame"]["components"][3]
                        .as_f64()
                        .unwrap()
                        + drift
                );
                assert!(reconciliation_identity_maps_equal(
                    &original.properties,
                    &observed.properties,
                    true
                ));
                assert!(
                    index
                        .matching(&observed)
                        .unwrap()
                        .iter()
                        .any(|(_, actual)| *actual == candidate)
                );
            }
        }
    }
}

#[cfg(test)]
mod never_serialized_properties {
    use super::*;

    #[test]
    fn audio_emitter_curves_are_readable_through_their_accessors() {
        let database = rbx_reflection_database::get().unwrap();
        for class_name in ["AudioEmitter", "AudioListener"] {
            for name in ["DistanceAttenuation", "AngleAttenuation"] {
                assert!(!reconciliation_property_is_unreadable(
                    database, class_name, name
                ));
                assert!(!reconciliation_property_is_unknown_when_absent(
                    Some(database),
                    class_name,
                    name
                ));
            }
        }
        assert!(reconciliation_property_is_unreadable(
            database,
            "Terrain",
            "SmoothGrid"
        ));
        assert!(!plugin_accesses_property_natively(
            database,
            "Part",
            "DistanceAttenuation"
        ));
    }

    #[test]
    fn a_placeholder_hull_matches_the_hull_cooked_from_it() {
        let hull = |bytes: &[u8]| serde_json::json!({"_type": "SharedString", "base64": base64::encode(bytes)});
        let placeholder = hull(b"CSGPHS    BLOCK");
        let cooked = hull(b"CSGPHS     cooked default");
        let cooked_hull = hull(b"CSGPHS    cooked hull");
        let other_cook = hull(b"CSGPHS    another cook");
        let equal = |left: &Value, right: &Value| {
            reconciliation_property_values_equal(
                "MeshPart",
                "PhysicalConfigData",
                Some(left),
                Some(right),
            )
        };
        assert!(equal(&placeholder, &cooked));
        assert!(equal(&cooked, &placeholder));
        assert!(equal(&cooked_hull, &other_cook));
        assert!(!equal(&cooked, &cooked_hull));
    }

    #[test]
    fn a_version_seven_hull_reads_its_fidelity_like_later_versions() {
        let hull = |version: u32, code: u8| {
            let mut bytes = b"CSGPHS".to_vec();
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.push(code);
            bytes.extend_from_slice(b"cooked data");
            serde_json::json!({"_type": "SharedString", "base64": base64::encode(&bytes)})
        };
        assert_eq!(hull_fidelity_code(&hull(7, 2)), Some(2));
        assert_eq!(hull_fidelity_code(&hull(8, 3)), Some(3));
        assert_eq!(hull_fidelity_code(&hull(6, 2)), None);
        assert!(reconciliation_property_values_equal(
            "MeshPart",
            "PhysicalConfigData",
            Some(&hull(8, 2)),
            Some(&hull(7, 2)),
        ));
        assert!(!reconciliation_property_values_equal(
            "MeshPart",
            "PhysicalConfigData",
            Some(&hull(8, 2)),
            Some(&hull(7, 3)),
        ));
    }

    #[test]
    fn values_studio_rewrites_while_loading_a_place_are_one_value() {
        let equal = |class: &str, name: &str, left: serde_json::Value, right: serde_json::Value| {
            reconciliation_property_values_equal(class, name, Some(&left), Some(&right))
        };
        let time = |text: &str| serde_json::json!(text);
        assert!(equal(
            "Lighting",
            "TimeOfDay",
            time("-10:35:20"),
            time("13:24:40")
        ));
        assert!(equal(
            "Lighting",
            "TimeOfDay",
            time("25:00"),
            time("01:00:00")
        ));
        assert!(!equal(
            "Lighting",
            "TimeOfDay",
            time("13:24:41"),
            time("13:24:40")
        ));
        assert_eq!(
            canonical_time_of_day("-10:35:20").as_deref(),
            Some("13:24:40")
        );
        assert_eq!(
            canonical_time_of_day("14:00:00").as_deref(),
            Some("14:00:00")
        );
        assert_eq!(canonical_time_of_day("noon"), None);

        let table = |base64: &str| serde_json::json!({"_type": "BinaryString", "base64": base64});
        assert!(equal(
            "AnimationRigData",
            "transform",
            table("AQAAABUAAAAAAAAAAgAAAAsAAAA="),
            table("AgAAABUAAAAAAAAAAgAAAAwAAAA=")
        ));
        assert!(!equal(
            "AnimationRigData",
            "label",
            table("AQAAABUAAAAAAAAAAgAAAAsAAAA="),
            serde_json::json!("AgAAABUAAAAAAAAAAgAAAAwAAAA=")
        ));
        assert!(!equal(
            "KeyframeSequence",
            "GuidBinaryString",
            table("AQAAABUAAAAAAAAAAgAAAAsAAAA="),
            table("AgAAABUAAAAAAAAAAgAAAAwAAAA=")
        ));

        assert!(equal(
            "HumanoidDescription",
            "FrontAccessory",
            serde_json::json!("10550727846,13600579295,14215737975"),
            serde_json::json!("14215737975,13600579295,10550727846")
        ));
        assert!(!equal(
            "HumanoidDescription",
            "FrontAccessory",
            serde_json::json!("10550727846,13600579295"),
            serde_json::json!("14215737975,13600579295,10550727846")
        ));

        assert!(equal(
            "HumanoidDescription",
            "EmotesDataInternal",
            serde_json::json!("Shrug^3576968026^\\Hello^3576686446^\\"),
            serde_json::json!("Hello^3576686446^\\Shrug^3576968026^\\")
        ));
        assert!(!equal(
            "HumanoidDescription",
            "EquippedEmotesDataInternal",
            serde_json::json!("1^Hello\\2^Stadium\\"),
            serde_json::json!("1^Hello\\2^Shrug\\")
        ));

        let font = |face: &str, weight: &str| {
            serde_json::json!({
                "_type": "Font",
                "family": "rbxasset://fonts/families/LegacyArial.json",
                "weight": weight,
                "style": "Normal",
                "cachedFaceId": face,
            })
        };
        assert!(equal(
            "TextBox",
            "FontFace",
            font("rbxasset://fonts/arial.ttf", "Regular"),
            font("rbxasset://fonts/Arimo-Regular.ttf", "Regular")
        ));
        assert!(!equal(
            "TextBox",
            "FontFace",
            font("rbxasset://fonts/arial.ttf", "Regular"),
            font("rbxasset://fonts/arial.ttf", "Bold")
        ));
        assert!(
            reconciliation_property_values_equal(
                "TextBox",
                "FontFace",
                Some(&font("rbxasset://fonts/arial.ttf", "Regular")),
                None
            ),
            "the legacy face file of the default family is the default font"
        );
        let untyped = serde_json::json!({
            "family": "rbxasset://fonts/families/GothamSSm.json",
            "weight": "Medium",
            "style": "Normal",
            "cachedFaceId": "rbxasset://fonts/GothamSSm-Medium.otf",
        });
        let mut files = Map::new();
        files.insert("FontFace".to_string(), untyped);
        assert!(
            reconciliation_maps_equal("BubbleChatConfiguration", &files, &Map::new()),
            "a store spells a font by its fields; the default's cached face differs"
        );
        let mut studio = Map::new();
        studio.insert(
            "C0".to_string(),
            serde_json::json!({"_type": "CFrame", "components": [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
        );
        let mut saved = Map::new();
        saved.insert(
            "C0".to_string(),
            serde_json::json!({"_type": "CFrame", "components": [0.0, 0.0, 0.0, 0.9999996423721313, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, -0.9999996423721313]}),
        );
        assert!(
            reconciliation_maps_equal("Weld", &saved, &studio),
            "the first pairing and the reconciled-state check accept the re-derived axis"
        );

        let cframe = |rotation: [f64; 9]| {
            let mut components = vec![
                0.2833123803138733,
                9.715829219203442e-05,
                0.0003780018596444279,
            ];
            components.extend(rotation);
            serde_json::json!({"_type": "CFrame", "components": components})
        };
        let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        assert!(equal(
            "Weld",
            "C0",
            cframe([
                0.9999996423721313,
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                -0.9999996423721313
            ]),
            cframe(identity)
        ));
        assert!(!equal(
            "Weld",
            "C0",
            cframe([0.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0]),
            cframe(identity)
        ));
        assert!(
            !equal(
                "Weld",
                "C0",
                cframe([1.00001, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]),
                cframe(identity)
            ),
            "a saved rotation Studio keeps is compared as saved"
        );
    }

    #[test]
    fn studio_issued_texture_packs_and_triangle_counts_are_one_value() {
        let equal = |class: &str, name: &str, left: serde_json::Value, right: serde_json::Value| {
            let property_equal =
                reconciliation_property_values_equal(class, name, Some(&left), Some(&right));
            let left = Map::from_iter([(name.to_string(), left)]);
            let right = Map::from_iter([(name.to_string(), right)]);
            assert_eq!(
                reconciliation_maps_equal(class, &left, &right),
                property_equal
            );
            assert_eq!(
                reconciliation_maps_equal(class, &right, &left),
                property_equal
            );
            property_equal
        };
        assert!(equal(
            "SurfaceAppearance",
            "TexturePack",
            serde_json::json!("rbxassetid://78214130152845"),
            serde_json::json!("rbxassetid://85495674897887")
        ));
        assert!(!equal(
            "SurfaceAppearance",
            "TexturePack",
            serde_json::json!("rbxassetid://1"),
            serde_json::json!("")
        ));
        assert!(!equal(
            "Decal",
            "TexturePack",
            serde_json::json!("rbxassetid://1"),
            serde_json::json!("rbxassetid://2")
        ));
        assert!(equal(
            "UnionOperation",
            "TriangleCount",
            serde_json::json!(120),
            serde_json::json!(124)
        ));
        assert!(!equal(
            "MeshPart",
            "TriangleCount",
            serde_json::json!(120),
            serde_json::json!(124)
        ));
    }

    #[test]
    fn an_unreadable_hull_format_matches_a_readable_hull_of_the_same_mesh() {
        let hull = |version: u32, rest: &[u8]| {
            let mut bytes = b"CSGPHS".to_vec();
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.extend_from_slice(rest);
            serde_json::json!({"_type": "SharedString", "base64": base64::encode(&bytes)})
        };
        let marker = serde_json::json!({"_type": "CollisionHullFidelity", "code": 2});
        let equal = |left: &Value, right: &Value| {
            reconciliation_property_values_equal(
                "MeshPart",
                "PhysicalConfigData",
                Some(left),
                Some(right),
            )
        };
        assert!(equal(&hull(6, b"@B old cook"), &hull(8, b" new cook")));
        assert!(equal(&marker, &hull(6, b"@B old cook")));
        assert!(equal(&marker, &hull(7, b" cook")));
        assert!(!equal(&marker, &hull(7, b" cook")));
        assert!(!equal(
            &hull(6, b" old"),
            &serde_json::json!({"_type": "SharedString", "base64": base64::encode(b"not a hull")})
        ));
    }

    #[test]
    fn terrain_physics_cache_alignment_requires_identical_voxels() {
        let grid = |bytes: &str| serde_json::json!({"_type": "BinaryString", "base64": base64::encode(bytes)});
        let properties = |voxels: &str, cache: &str| {
            Map::from_iter([
                ("SmoothGrid".into(), grid(voxels)),
                ("PhysicsGrid".into(), grid(cache)),
            ])
        };
        let saved = properties("voxels", "saved cache");
        let rebuilt = properties("voxels", "rebuilt cache");
        let edited = properties("painted voxels", "rebuilt cache");
        assert!(reconciliation_maps_equal("Terrain", &saved, &rebuilt));
        assert!(reconciliation_maps_equal("Terrain", &rebuilt, &saved));
        assert!(!reconciliation_maps_equal("Terrain", &saved, &edited));
        assert!(!terrain_physics_caches_match("Part", &saved, &rebuilt));
        assert!(!terrain_physics_caches_match(
            "Terrain",
            &saved,
            &Map::new()
        ));
        let mut missing_voxels = rebuilt.clone();
        missing_voxels.remove("SmoothGrid");
        assert!(!reconciliation_maps_equal(
            "Terrain",
            &saved,
            &missing_voxels
        ));
        assert!(!reconciliation_property_values_equal(
            "Terrain",
            "PhysicsGrid",
            saved.get("PhysicsGrid"),
            rebuilt.get("PhysicsGrid")
        ));

        let document = |properties| {
            let mut terrain = SettingsBytecodeInstance::new(
                "terrain".into(),
                "Terrain".into(),
                "Terrain".into(),
                None,
            );
            terrain.properties = properties;
            SettingsBytecode {
                version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                instances: vec![terrain],
            }
        };
        let saved_bytes = encode_settings_bytecode(&document(saved.clone())).unwrap();
        let rebuilt_bytes = encode_settings_bytecode(&document(rebuilt.clone())).unwrap();
        assert!(matches!(
            align_settings_bytes_to_reference(&saved_bytes, &rebuilt_bytes).unwrap(),
            SettingsAlignment::Equivalent
        ));
        for (mut observed, expected) in [(rebuilt, saved.clone()), (edited.clone(), edited)] {
            align_equivalent_property_map_values(None, "Terrain", &saved, &mut observed);
            assert_eq!(observed, expected);
        }
    }

    #[test]
    fn a_recut_solid_operation_keeps_its_new_hull() {
        let hull = |bytes: &[u8]| serde_json::json!({"_type": "SharedString", "base64": base64::encode(bytes)});
        let csg = |mesh: &str, hull_bytes: &[u8]| {
            Map::from_iter([
                (
                    "MeshData2".to_string(),
                    serde_json::json!({"_type": "SharedString", "base64": base64::encode(mesh)}),
                ),
                ("PhysicalConfigData".to_string(), hull(hull_bytes)),
            ])
        };
        let union = csg("tree", b"CSGPHS\x08\0\0\0\x01\0union cook");
        let recook = csg("tree", b"CSGPHS\x08\0\0\0\x01\0later cook");
        let recut = csg("other tree", b"CSGPHS\x08\0\0\0\x01\0recut cook");
        assert!(reconciliation_maps_equal("UnionOperation", &union, &recook));
        assert!(!reconciliation_maps_equal("UnionOperation", &union, &recut));
        let mut kept = recut.clone();
        align_equivalent_property_map_values(None, "UnionOperation", &union, &mut kept);
        assert_eq!(kept["PhysicalConfigData"], recut["PhysicalConfigData"]);
    }

    #[test]
    fn a_solid_model_operations_box_and_default_fidelity_are_one_value() {
        let fidelity = |name: &str| serde_json::json!({"_type": "EnumItem", "name": name});
        let equal = |class_name: &str, left: &str, right: &str| {
            reconciliation_property_values_equal(
                class_name,
                "CollisionFidelity",
                Some(&fidelity(left)),
                Some(&fidelity(right)),
            )
        };
        for class_name in ["UnionOperation", "NegateOperation", "IntersectOperation"] {
            assert!(equal(class_name, "Default", "Box"));
            assert!(equal(class_name, "Box", "Default"));
            assert!(!equal(class_name, "Hull", "Default"));
            assert!(!equal(class_name, "Hull", "PreciseConvexDecomposition"));
        }
        assert!(!equal("MeshPart", "Default", "Box"));
    }

    #[test]
    fn a_hull_is_not_aligned_across_a_mesh_change() {
        let hull = |bytes: &[u8]| serde_json::json!({"_type": "SharedString", "base64": base64::encode(bytes)});
        let record = |mesh: &str, hull_bytes: &[u8]| {
            Map::from_iter([
                ("MeshId".to_string(), serde_json::json!(mesh)),
                ("PhysicalConfigData".to_string(), hull(hull_bytes)),
            ])
        };
        let reference = record("rbxassetid://1", b"CSGPHS\x08\0\0\0\x01\0first cook");
        let same_mesh = record("rbxassetid://1", b"CSGPHS\x08\0\0\0\x01\0second cook");
        let other_mesh = record("rbxassetid://2", b"CSGPHS\x08\0\0\0\x01\0other cook");
        assert!(reconciliation_maps_equal(
            "MeshPart", &reference, &same_mesh
        ));
        assert!(!reconciliation_maps_equal(
            "MeshPart",
            &reference,
            &other_mesh
        ));
        let mut aligned = same_mesh.clone();
        align_equivalent_property_map_values(None, "MeshPart", &reference, &mut aligned);
        assert_eq!(
            aligned["PhysicalConfigData"],
            reference["PhysicalConfigData"]
        );
        let mut kept = other_mesh.clone();
        align_equivalent_property_map_values(None, "MeshPart", &reference, &mut kept);
        assert_eq!(kept["PhysicalConfigData"], other_mesh["PhysicalConfigData"]);
    }
}

#[cfg(test)]
mod verify_noise_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tags_compare_as_a_set() {
        let mut files = Map::new();
        files.insert("Tags".into(), json!(["PassengerSeat", "invisibleseat"]));
        let mut studio = Map::new();
        studio.insert("Tags".into(), json!(["invisibleseat", "PassengerSeat"]));
        assert!(reconciliation_maps_equal("Seat", &files, &studio));
        studio.insert("Tags".into(), json!(["invisibleseat"]));
        assert!(!reconciliation_maps_equal("Seat", &files, &studio));
    }

    #[test]
    fn tag_order_does_not_count_in_the_shared_comparator_either() {
        assert!(reconciliation_property_values_equal(
            "Seat",
            "Tags",
            Some(&json!(["a", "b"])),
            Some(&json!(["b", "a"]))
        ));
        assert!(!reconciliation_property_values_equal(
            "Seat",
            "Tags",
            Some(&json!(["a", "b"])),
            Some(&json!(["b"]))
        ));
    }

    #[test]
    fn world_guis_reset_on_spawn_says_nothing_when_one_record_lacks_it() {
        let mut explicit = Map::new();
        explicit.insert("ResetOnSpawn".into(), json!(false));
        let absent = Map::new();
        assert!(reconciliation_maps_equal(
            "BillboardGui",
            &absent,
            &explicit
        ));
        assert!(reconciliation_maps_equal("SurfaceGui", &explicit, &absent));
        assert!(!reconciliation_maps_equal("ScreenGui", &absent, &explicit));
        let mut other = Map::new();
        other.insert("ResetOnSpawn".into(), json!(true));
        assert!(!reconciliation_maps_equal(
            "BillboardGui",
            &explicit,
            &other
        ));
    }
}

#[cfg(test)]
mod rotation_tests;

#[cfg(test)]
mod viewport_camera_tests {
    use super::*;
    use crate::tests::support::{settings_document, settings_instance};
    use serde_json::json;

    /// What a pull writes for `pulled` over the store `store`.
    fn pulled_over(store: &SettingsBytecode, pulled: &SettingsBytecode) -> SettingsBytecode {
        let aligned = align_pulled_settings_bytes_to_reference(
            &encode_settings_bytecode(store).unwrap(),
            &encode_settings_bytecode(pulled).unwrap(),
        )
        .unwrap();
        match aligned {
            SettingsAlignment::Changed(bytes) => decode_settings_bytecode(&bytes).unwrap(),
            SettingsAlignment::Equivalent => store.clone(),
        }
    }

    #[test]
    fn a_pull_keeps_the_stores_viewport_camera_values() {
        let mut root = settings_instance("ws", "Workspace", "Workspace", None);
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "settingsId": "cam"}),
        );
        let mut camera = settings_instance("cam", "Camera", "Camera", Some(0));
        camera.properties.insert("FieldOfView".into(), json!(70.0));
        let mut part = settings_instance("part", "Part", "Part", Some(0));
        part.properties.insert("Anchored".into(), json!(true));
        let store = settings_document(vec![root.clone(), camera.clone(), part.clone()]);

        let mut pulled_camera = camera;
        pulled_camera
            .properties
            .insert("FieldOfView".into(), json!(60.0));
        let mut pulled_part = part;
        pulled_part
            .properties
            .insert("Anchored".into(), json!(false));
        let pulled = settings_document(vec![root, pulled_camera, pulled_part]);

        let kept = pulled_over(&store, &pulled);
        assert_eq!(
            kept.instances[1].properties.get("FieldOfView"),
            Some(&json!(70.0))
        );
        assert_eq!(
            kept.instances[2].properties.get("Anchored"),
            Some(&json!(false))
        );
        // The same pair, compared without a camera to keep, differs: the
        // answer for the pull must not be remembered for that caller.
        let plain = align_settings_bytes_to_reference(
            &encode_settings_bytecode(&store).unwrap(),
            &encode_settings_bytecode(&pulled).unwrap(),
        )
        .unwrap();
        let SettingsAlignment::Changed(plain) = plain else {
            panic!("a changed camera and part are a change");
        };
        assert_eq!(
            decode_settings_bytecode(&plain).unwrap().instances[1]
                .properties
                .get("FieldOfView"),
            Some(&json!(60.0))
        );
    }

    #[test]
    fn a_store_that_already_holds_the_references_ids_is_written_as_it_came() {
        let root = settings_instance("ws", "Workspace", "Workspace", None);
        let mut part = settings_instance("part", "Part", "Part", Some(0));
        part.properties.insert("Anchored".into(), json!(true));
        let mut value = settings_instance("value", "ObjectValue", "ObjectValue", Some(0));
        value.properties.insert(
            "Value".into(),
            json!({"_type": "Ref", "settingsId": "part"}),
        );
        let store = settings_document(vec![root.clone(), part.clone(), value.clone()]);
        let mut edited_part = part.clone();
        edited_part
            .properties
            .insert("Anchored".into(), json!(false));
        let pulled = settings_document(vec![root.clone(), edited_part, value.clone()]);
        let (store_bytes, pulled_bytes) = (
            encode_settings_bytecode(&store).unwrap(),
            encode_settings_bytecode(&pulled).unwrap(),
        );
        for aligned in [
            align_pulled_settings_bytes_to_reference(&store_bytes, &pulled_bytes).unwrap(),
            align_settings_bytes_to_reference(&store_bytes, &pulled_bytes).unwrap(),
        ] {
            let SettingsAlignment::Changed(aligned) = aligned else {
                panic!("an edited value is a change");
            };
            assert!(
                aligned == pulled_bytes,
                "the pulled bytes are kept as they are"
            );
        }

        // The same instances under other ids are aligned the long way and
        // come out under the reference's ids.
        let mut renamed_part = part;
        renamed_part.settings_id = "studio-1".into();
        renamed_part
            .properties
            .insert("Anchored".into(), json!(false));
        let mut renamed_value = value;
        renamed_value.settings_id = "studio-2".into();
        renamed_value.properties.insert(
            "Value".into(),
            json!({"_type": "Ref", "settingsId": "studio-1"}),
        );
        let renamed = settings_document(vec![root, renamed_part, renamed_value]);
        let SettingsAlignment::Changed(aligned) = align_pulled_settings_bytes_to_reference(
            &store_bytes,
            &encode_settings_bytecode(&renamed).unwrap(),
        )
        .unwrap() else {
            panic!("an edited value is a change");
        };
        assert!(aligned == pulled_bytes, "other ids align to the same store");
    }

    #[test]
    fn a_pull_that_changed_only_the_viewport_camera_changes_nothing() {
        let mut root = settings_instance("ws", "Workspace", "Workspace", None);
        root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "settingsId": "cam"}),
        );
        let mut camera = settings_instance("cam", "Camera", "Camera", Some(0));
        camera.properties.insert("FieldOfView".into(), json!(70.0));
        let store = settings_document(vec![root.clone(), camera.clone()]);
        let mut pulled_camera = camera;
        pulled_camera
            .properties
            .insert("FieldOfView".into(), json!(60.0));
        let pulled = settings_document(vec![root, pulled_camera]);
        let (store_bytes, pulled_bytes) = (
            encode_settings_bytecode(&store).unwrap(),
            encode_settings_bytecode(&pulled).unwrap(),
        );
        for _ in 0..2 {
            assert!(matches!(
                align_pulled_settings_bytes_to_reference(&store_bytes, &pulled_bytes).unwrap(),
                SettingsAlignment::Equivalent
            ));
            assert!(matches!(
                align_settings_bytes_to_reference(&store_bytes, &pulled_bytes).unwrap(),
                SettingsAlignment::Changed(_)
            ));
        }
    }

    #[test]
    fn a_pull_keeps_the_viewport_camera_of_a_store_that_names_no_current_camera() {
        let root = settings_instance("ws", "Workspace", "Workspace", None);
        let mut viewport = settings_instance("cam", "Camera", "Camera", Some(0));
        viewport
            .properties
            .insert("FieldOfView".into(), json!(70.0));
        let mut other = settings_instance("cam2", "Camera", "Camera", Some(0));
        other.properties.insert("FieldOfView".into(), json!(50.0));
        let store = settings_document(vec![root.clone(), viewport.clone(), other.clone()]);

        let mut pulled_root = root;
        pulled_root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "settingsId": "cam"}),
        );
        let mut pulled_viewport = viewport;
        pulled_viewport
            .properties
            .insert("FieldOfView".into(), json!(60.0));
        let mut pulled_other = other;
        pulled_other
            .properties
            .insert("FieldOfView".into(), json!(55.0));
        let pulled = settings_document(vec![pulled_root, pulled_viewport, pulled_other]);

        let kept = pulled_over(&store, &pulled);
        assert_eq!(
            kept.instances[1].properties.get("FieldOfView"),
            Some(&json!(70.0)),
            "the matched viewport row keeps the store's values"
        );
        assert_eq!(
            kept.instances[2].properties.get("FieldOfView"),
            Some(&json!(55.0)),
            "another Camera is pulled like any instance"
        );
        assert_eq!(
            kept.instances[0].properties.get("CurrentCamera"),
            Some(&json!({"_type": "Ref", "instanceIndex": 2})),
            "the pulled store records the reference to the viewport row"
        );
    }

    #[test]
    fn a_viewport_camera_of_another_identity_is_pulled() {
        let root = settings_instance("ws", "Workspace", "Workspace", None);
        let mut saved = settings_instance("old-cam", "Camera", "Camera", Some(0));
        saved.properties.insert("FieldOfView".into(), json!(70.0));
        let store = settings_document(vec![root.clone(), saved]);

        let mut pulled_root = root;
        pulled_root.properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "settingsId": "cam"}),
        );
        let mut live = settings_instance("cam", "Camera", "Camera", Some(0));
        live.properties.insert("FieldOfView".into(), json!(60.0));
        let mut pulled = settings_document(vec![pulled_root, live]);

        // The step by itself never takes a camera for the viewport by its name.
        let reference = reference_viewport_camera(&store).unwrap();
        assert!(!keep_reference_viewport_camera(&reference, &mut pulled));
        assert_eq!(
            pulled.instances[1].properties.get("FieldOfView"),
            Some(&json!(60.0))
        );
    }
}

#[cfg(test)]
mod already_aligned_tests {
    use super::*;
    use crate::tests::support::{settings_document, settings_instance};
    use serde_json::json;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }
    }

    fn unique_id(number: usize) -> Value {
        json!({"_type": "UniqueId", "value": format!("{:032x}", number + 1)})
    }

    /// A Workspace store of same-named siblings, some of them cameras, under
    /// ids of every kind the alignment tells apart.
    fn store(rng: &mut Rng) -> SettingsBytecode {
        let mut root = settings_instance("ws", "Workspace", "Workspace", None);
        let count = 3 + rng.below(6);
        let mut instances = Vec::new();
        for index in 0..count {
            let class = ["Part", "Camera", "Folder"][rng.below(3)];
            let id = match rng.below(12) {
                0 | 1 => format!("debug:{index}"),
                2 | 3 => format!("{index:x}"),
                // A broken store: the id of the instance before it.
                4 if index > 0 => format!("editor:{}", index - 1),
                _ => format!("editor:{index}"),
            };
            let name = ["A", "B"][rng.below(2)];
            let mut instance = settings_instance(&id, name, class, Some(0));
            instance
                .properties
                .insert("Mark".into(), json!(rng.below(3)));
            if rng.below(3) == 0 {
                instance
                    .properties
                    .insert("UniqueId".into(), unique_id(index));
            }
            instances.push(instance);
        }
        let cameras = instances
            .iter()
            .filter(|instance| instance.class_name == "Camera")
            .map(|instance| instance.settings_id.clone())
            .collect::<Vec<_>>();
        if !cameras.is_empty() && rng.below(2) == 0 {
            root.properties.insert(
                "CurrentCamera".into(),
                json!({"_type": "Ref", "settingsId": cameras[rng.below(cameras.len())]}),
            );
        }
        settings_document(std::iter::once(root).chain(instances).collect())
    }

    /// What Studio may hand back for that store: values edited, a saved
    /// UniqueId moved to a sibling, another camera in the viewport, two
    /// siblings of one name with their contents traded.
    fn edited(store: &SettingsBytecode, rng: &mut Rng) -> SettingsBytecode {
        let mut edited = store.clone();
        let last = edited.instances.len() - 1;
        for _ in 0..rng.below(4) {
            let index = 1 + rng.below(last);
            match rng.below(5) {
                0 | 1 => {
                    edited.instances[index]
                        .properties
                        .insert("Mark".into(), json!(rng.below(3)));
                }
                4 => {
                    // Two siblings of one name trade what they hold.
                    let other = 1 + rng.below(last);
                    if edited.instances[index].name == edited.instances[other].name
                        && edited.instances[index].class_name == edited.instances[other].class_name
                    {
                        let held = edited.instances[index].properties.clone();
                        edited.instances[index].properties =
                            edited.instances[other].properties.clone();
                        edited.instances[other].properties = held;
                    }
                }
                2 => {
                    let other = 1 + rng.below(last);
                    let moved = edited.instances[index].properties.remove("UniqueId");
                    let back = edited.instances[other].properties.remove("UniqueId");
                    for (target, value) in [(other, moved), (index, back)] {
                        if let Some(value) = value {
                            edited.instances[target]
                                .properties
                                .insert("UniqueId".into(), value);
                        }
                    }
                }
                _ => {
                    if edited.instances[index].class_name == "Camera" {
                        let id = edited.instances[index].settings_id.clone();
                        edited.instances[0].properties.insert(
                            "CurrentCamera".into(),
                            json!({"_type": "Ref", "settingsId": id}),
                        );
                    }
                }
            }
        }
        edited
    }

    #[test]
    fn a_store_said_to_be_aligned_is_one_the_matching_leaves_as_it_is() {
        let (mut said_aligned, mut said_not) = (0, 0);
        for seed in 1..=4_000u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let reference = store(&mut rng);
            let observed = edited(&reference, &mut rng);
            if !identities_already_aligned(&reference, &observed, Placed::default()) {
                said_not += 1;
                continue;
            }
            said_aligned += 1;
            let mut matched = observed.clone();
            assert!(
                align_settings_ids_by_matching(&reference, &mut matched),
                "seed {seed}: the matching refused a store said to be aligned"
            );
            let mut expected = observed;
            stabilize_settings_reference_ids(&mut expected);
            stabilize_settings_reference_ids(&mut matched);
            assert!(
                matched == expected,
                "seed {seed}: the matching moved an identity of a store said to be aligned"
            );
        }
        assert!(
            said_aligned > 200 && said_not > 200,
            "the seeds must reach both answers: {said_aligned} aligned, {said_not} not"
        );
    }
}

#[cfg(test)]
#[path = "alignment_sim.rs"]
mod alignment_sim;

/// Namesakes under one parent that no UniqueId tells apart are paired by
/// their order only where the matching is asked to, and it says which.
#[test]
fn namesakes_nothing_tells_apart_are_paired_by_their_order_where_asked() {
    let row = |id: &str, name: &str| {
        SettingsBytecodeInstance::new(id.into(), name.into(), "Part".into(), Some(0))
    };
    let document = |root: &str, rows: Vec<SettingsBytecodeInstance>| {
        let mut instances = vec![SettingsBytecodeInstance::new(
            root.into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        )];
        instances.extend(rows);
        SettingsBytecode {
            version: super::bytecode::SETTINGS_BINARY_VERSION,
            instances,
        }
    };
    let files = document(
        "root",
        vec![
            row("a", "Part"),
            row("b", "Part"),
            row("new", "Other"),
            row("c", "Part"),
        ],
    );
    // Studio holds one more of them, the last it was given.
    let studio = document(
        "debug:0",
        (1..=4)
            .map(|index| row(&format!("debug:{index}"), "Part"))
            .collect(),
    );
    assert!(settings_ids_told_alone(&files, &studio).is_none());
    let (ids, places, by_order) = settings_ids_told_in_order(&files, &studio).unwrap();
    assert_eq!(places, [Some(0), Some(1), Some(2), Some(4), None]);
    assert_eq!(ids, ["root", "a", "b", "c", "debug:4"]);
    assert_eq!(
        by_order,
        [
            (4, ByOrder::More),
            (1, ByOrder::BesideMore),
            (2, ByOrder::BesideMore),
            (3, ByOrder::BesideMore)
        ]
    );
}
