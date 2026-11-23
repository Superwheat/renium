use std::collections::{HashMap, HashSet, VecDeque};
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

/// Like `align_pulled_settings_documents_to_reference`, when the caller has
/// already dropped the captured `ServiceState` and `observed` encodes to the
/// exact bytes that `ServiceState` would produce.
pub(crate) fn align_pulled_settings_documents_without_state(
    reference: SettingsBytecode,
    observed: SettingsBytecode,
    started: std::time::Instant,
    stored: &[u8],
) -> Result<PulledAlignment> {
    let stored_as_observed = AtomicBool::new(false);
    let alignment = align_settings_documents(
        reference,
        observed,
        AlignmentInput {
            observed_bytes: ObservedBytes::EncodeDocument,
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

/// Compact per-instance metadata computed while streaming a store block or
/// captured chunk, allowing full-fidelity alignment without materializing
/// whole `SettingsBytecode` documents in memory.
#[derive(Clone, Copy)]
pub(crate) struct CompactRowMeta {
    pub(crate) exact_digest: [u8; 16],
    pub(crate) full_digest: [u8; 16],
    pub(crate) compared_digest: [u8; 16],
    pub(crate) unique_id_digest: [u8; 16],
    pub(crate) keys_hash: u64,
    pub(crate) reflexive: bool,
    pub(crate) has_untyped_enum: bool,
    pub(crate) has_ref: bool,
}

/// Compact representation of a store document for streamed alignment: bare
/// instances retaining only reference-bearing properties/attributes and root
/// `CurrentCamera`, out-of-band `UniqueId` strings, per-row digests, and full
/// `Camera` rows for viewport preservation.
pub(crate) struct CompactDocument {
    pub(crate) document: SettingsBytecode,
    pub(crate) unique_ids: Vec<Option<Box<str>>>,
    pub(crate) meta: Vec<CompactRowMeta>,
    pub(crate) cameras: Vec<(usize, SettingsBytecodeInstance)>,
}

pub(crate) fn stabilize_instance_references(
    instance: &mut SettingsBytecodeInstance,
    ids: &[String],
) {
    stabilize_record_references(&mut instance.properties, ids);
    stabilize_record_references(&mut instance.attributes, ids);
}

fn digest_exact_value_and_check(
    digest: &mut Sha256,
    value: &Value,
    mut reflexive: Option<&mut bool>,
    has_untyped_enum: &mut bool,
) {
    if let Value::Object(map) = value
        && let Some(held_digest) = crate::settings::held::digest_once(value, |resolved| {
            let mut sub = Sha256::new();
            digest_value(&mut sub, resolved);
            let whole = <[u8; 32]>::from(sub.finalize());
            <[u8; 16]>::try_from(&whole[..16]).unwrap()
        })
    {
        let _ = map;
        digest.update([8]);
        digest.update(held_digest);
        return;
    }
    match value {
        Value::Null => digest.update([0]),
        Value::Bool(flag) => digest.update([1, u8::from(*flag)]),
        Value::Number(number) => {
            if let Some(v) = number.as_u64() {
                digest.update([2]);
                digest.update(v.to_le_bytes());
            } else if let Some(v) = number.as_i64() {
                digest.update([3]);
                digest.update(v.to_le_bytes());
            } else {
                let f = number.as_f64().unwrap_or(f64::NAN);
                let bits = if f == 0.0 {
                    0.0f64.to_bits()
                } else {
                    f.to_bits()
                };
                digest.update([4]);
                digest.update(bits.to_le_bytes());
            }
        }
        Value::String(text) => {
            digest.update([5]);
            digest_text(digest, text);
        }
        Value::Array(items) => {
            digest.update([6]);
            digest.update((items.len() as u64).to_le_bytes());
            for item in items {
                digest_exact_value_and_check(
                    digest,
                    item,
                    reflexive.as_deref_mut(),
                    has_untyped_enum,
                );
            }
        }
        Value::Object(map) => {
            digest.update([7]);
            digest.update((map.len() as u64).to_le_bytes());
            let check_refl = reflexive.as_deref().is_some_and(|r| *r);
            let mut need_cframe_components = false;
            let mut saw_cframe_components = false;
            let mut need_enum_field = false;
            let mut saw_enum_field = false;
            let single_key = map.len() == 1;
            if map.get("_type").and_then(Value::as_str) == Some("EnumItem")
                && map
                    .get("enumType")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                *has_untyped_enum = true;
            }
            for (k, v) in map {
                if check_refl {
                    let ks = k.as_str();
                    if ks == "_type" {
                        if let Some(type_name) = v.as_str() {
                            match type_name {
                                "CFrame" => need_cframe_components = true,
                                "EnumItem" => need_enum_field = true,
                                "NumberSequence" | "ColorSequence" | "Font" => {
                                    if let Some(refl) = reflexive.as_deref_mut() {
                                        *refl = false;
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else if single_key && matches!(ks, "NumberSequence" | "ColorSequence" | "Ref")
                    {
                        if let Some(refl) = reflexive.as_deref_mut() {
                            *refl = false;
                        }
                    } else if need_cframe_components && ks == "components" {
                        saw_cframe_components = matches!(
                            v,
                            Value::Array(arr) if arr.len() == 12 && arr.iter().all(Value::is_number)
                        );
                    } else if need_enum_field
                        && ((ks == "value" && v.is_u64()) || (ks == "name" && v.is_string()))
                    {
                        saw_enum_field = true;
                    }
                }
                digest_text(digest, k);
                digest_exact_value_and_check(digest, v, reflexive.as_deref_mut(), has_untyped_enum);
            }
            if check_refl
                && ((need_cframe_components && !saw_cframe_components)
                    || (need_enum_field && !saw_enum_field))
                && let Some(refl) = reflexive
            {
                *refl = false;
            }
        }
    }
}

fn value_has_untyped_enum(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("_type").and_then(Value::as_str) == Some("EnumItem")
                && map
                    .get("enumType")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                || map.values().any(value_has_untyped_enum)
        }
        Value::Array(values) => values.iter().any(value_has_untyped_enum),
        _ => false,
    }
}

/// Hashes the value of a stable non-ref property into `keys_hash` whenever
/// `reconciliation_values_equal(..., false)` compares that value strictly
/// (no float32 tolerance, no optional `EnumItem.enumType`, no Font filtering).
/// Values with any non-exact rule contribute a fixed sentinel so equal values
/// under `reconciliation_values_equal(..., false)` are mathematically guaranteed
/// to produce the exact same `keys_hash`.
fn hash_strict_identity_value(value: &Value, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash as _;
    if let Value::Object(_) = value
        && let Some(held_digest) = crate::settings::held::digest_once(value, |resolved| {
            let mut sub = Sha256::new();
            digest_value(&mut sub, resolved);
            let whole = <[u8; 32]>::from(sub.finalize());
            <[u8; 16]>::try_from(&whole[..16]).unwrap()
        })
    {
        0x51u8.hash(hasher);
        held_digest.hash(hasher);
        return;
    }
    let value = typed_scalar(value);
    match value {
        Value::Null => 0x00u8.hash(hasher),
        Value::Bool(flag) => {
            0x01u8.hash(hasher);
            flag.hash(hasher);
        }
        Value::Number(number) => {
            if let Some(i) = number
                .as_i64()
                .map(i128::from)
                .or_else(|| number.as_u64().map(i128::from))
            {
                0x02u8.hash(hasher);
                i.hash(hasher);
            } else if let Some(f) = number.as_f64() {
                if f.is_finite()
                    && f.fract() == 0.0
                    && f >= (i64::MIN as f64)
                    && f <= (u64::MAX as f64)
                {
                    let i = f as i128;
                    if (i as f64) == f {
                        0x02u8.hash(hasher);
                        i.hash(hasher);
                        return;
                    }
                }
                0x03u8.hash(hasher);
                let bits = if f == 0.0 {
                    0.0f64.to_bits()
                } else {
                    f.to_bits()
                };
                bits.hash(hasher);
            } else {
                0xffu8.hash(hasher);
            }
        }
        Value::String(text) => {
            0x04u8.hash(hasher);
            text.hash(hasher);
        }
        Value::Object(map) => {
            let type_name = map.get("_type").and_then(Value::as_str);
            if type_name == Some("BinaryString")
                && map.len() == 2
                && let Some(b64) = map.get("base64").and_then(Value::as_str)
            {
                0x05u8.hash(hasher);
                b64.hash(hasher);
            } else if type_name == Some("SharedString")
                && map.len() == 2
                && map.get("base64").and_then(Value::as_str).is_some()
            {
                let mut sub = Sha256::new();
                digest_value(&mut sub, value);
                let whole = <[u8; 32]>::from(sub.finalize());
                0x51u8.hash(hasher);
                whole[..16].hash(hasher);
            } else {
                0xffu8.hash(hasher);
            }
        }
        Value::Array(_) => 0xffu8.hash(hasher),
    }
}

/// Strips `instance` down to its compact form (keeping only root `CurrentCamera`
/// and reference-bearing properties/attributes, freeing empty `BTreeMap` nodes)
/// and returns its out-of-band `UniqueId`, compact metadata, and a full clone
/// when `instance.class_name == "Camera"`.
pub(crate) fn compact_row(
    instance: &mut SettingsBytecodeInstance,
) -> (
    Option<Box<str>>,
    CompactRowMeta,
    Option<SettingsBytecodeInstance>,
) {
    use std::hash::{Hash as _, Hasher as _};

    let database = rbx_reflection_database::get().ok();
    let camera = (instance.class_name == "Camera").then(|| instance.clone());
    let unique_id = persistent_identity(instance).map(Box::<str>::from);
    let mut exact = Sha256::new();
    let mut full = Sha256::new();
    let mut compared = Sha256::new();
    let mut unique_id_digest = [0u8; 16];
    let mut keys_hasher = fixed_identity_hasher();
    let mut reflexive = true;
    let mut has_untyped_enum = false;
    let mut has_ref = false;
    let is_root = instance.parent_index.is_none();
    let mut kept_props = Map::new();
    let mut kept_attrs = Map::new();

    exact.update([0x11]);
    full.update([0x11]);
    compared.update([0x11]);
    0x11u8.hash(&mut keys_hasher);
    for (name, value) in std::mem::take(&mut instance.properties) {
        let contains_ref = matches!(value, Value::Array(_) | Value::Object(_))
            && reconciliation_value_contains_reference(&value);
        if contains_ref {
            has_ref = true;
            if !has_untyped_enum {
                has_untyped_enum = value_has_untyped_enum(&value);
            }
            kept_props.insert(name, value);
            continue;
        }
        let mut entry_digest = Sha256::new();
        let stable = property_name_is_stable_no_ref(&name, &value);
        let mut prop_untyped = false;
        digest_exact_value_and_check(
            &mut entry_digest,
            &value,
            stable.then_some(&mut reflexive),
            &mut prop_untyped,
        );
        has_untyped_enum |= prop_untyped;
        let val_hash = <[u8; 32]>::from(entry_digest.finalize());
        if name == "UniqueId" {
            unique_id_digest.copy_from_slice(&val_hash[..16]);
        } else {
            full.update([1]);
            digest_text(&mut full, &name);
            full.update(&val_hash[..16]);
        }
        if !reconciliation_property_is_engine_state(database, &instance.class_name, &name, &value) {
            compared.update([1]);
            digest_text(&mut compared, &name);
            compared.update(&val_hash[..16]);
        }
        if stable {
            exact.update([1]);
            digest_text(&mut exact, &name);
            exact.update(&val_hash[..16]);
            name.hash(&mut keys_hasher);
            hash_strict_identity_value(&value, &mut keys_hasher);
        }
        if is_root && name == "CurrentCamera" {
            kept_props.insert(name, value);
        }
    }

    exact.update([0x22]);
    full.update([0x22]);
    compared.update([0x22]);
    0x22u8.hash(&mut keys_hasher);
    for (name, value) in std::mem::take(&mut instance.attributes) {
        let contains_ref = matches!(value, Value::Array(_) | Value::Object(_))
            && reconciliation_value_contains_reference(&value);
        if contains_ref {
            has_ref = true;
            if !has_untyped_enum {
                has_untyped_enum = value_has_untyped_enum(&value);
            }
            kept_attrs.insert(name, value);
            continue;
        }
        let mut entry_digest = Sha256::new();
        let mut attr_untyped = false;
        digest_exact_value_and_check(
            &mut entry_digest,
            &value,
            Some(&mut reflexive),
            &mut attr_untyped,
        );
        has_untyped_enum |= attr_untyped;
        let val_hash = <[u8; 32]>::from(entry_digest.finalize());
        full.update([1]);
        digest_text(&mut full, &name);
        full.update(&val_hash[..16]);
        if !is_engine_managed_attribute(&name) {
            compared.update([1]);
            digest_text(&mut compared, &name);
            compared.update(&val_hash[..16]);
        }
        exact.update([1]);
        digest_text(&mut exact, &name);
        exact.update(&val_hash[..16]);
        name.hash(&mut keys_hasher);
        hash_strict_identity_value(&value, &mut keys_hasher);
    }

    instance.properties = kept_props;
    instance.attributes = kept_attrs;
    let exact_full = <[u8; 32]>::from(exact.finalize());
    let full_full = <[u8; 32]>::from(full.finalize());
    let compared_full = <[u8; 32]>::from(compared.finalize());
    let meta = CompactRowMeta {
        exact_digest: <[u8; 16]>::try_from(&exact_full[..16]).unwrap(),
        full_digest: <[u8; 16]>::try_from(&full_full[..16]).unwrap(),
        compared_digest: <[u8; 16]>::try_from(&compared_full[..16]).unwrap(),
        unique_id_digest,
        keys_hash: keys_hasher.finish(),
        reflexive,
        has_untyped_enum,
        has_ref,
    };
    (unique_id, meta, camera)
}

type HydrateRowsFn<'a> =
    &'a (dyn Fn(&[usize]) -> Result<Vec<(usize, SettingsBytecodeInstance)>> + Sync);

struct CompactMatching<'a> {
    ref_unique_ids: &'a [Option<Box<str>>],
    obs_unique_ids: &'a [Option<Box<str>>],
    ref_meta: &'a [CompactRowMeta],
    obs_meta: &'a [CompactRowMeta],
    hydrate_ref: HydrateRowsFn<'a>,
    hydrate_obs: HydrateRowsFn<'a>,
}

fn persistent_identity_index_from_slice(
    unique_ids: &[Option<Box<str>>],
) -> AHashMap<&str, Option<usize>> {
    let mut result = AHashMap::new();
    for (index, id) in unique_ids.iter().enumerate() {
        if let Some(id) = id.as_deref() {
            result
                .entry(id)
                .and_modify(|value| *value = None)
                .or_insert(Some(index));
        }
    }
    result
}

fn compute_subtree_keys_compact(
    reference: &SettingsBytecode,
    ref_meta: &[CompactRowMeta],
    ref_required: Vec<bool>,
    observed: &SettingsBytecode,
    obs_meta: &[CompactRowMeta],
    obs_tree: &DocTree,
    obs_required: Vec<bool>,
) -> ExactSubtreeKeys {
    use std::hash::{Hash as _, Hasher as _};
    const OBS_BIT: u32 = 1 << 31;
    const IDX_MASK: u32 = !OBS_BIT;

    let ref_count = reference.instances.len();
    let obs_count = observed.instances.len();
    assert!(ref_count < (OBS_BIT as usize) && obs_count < (OBS_BIT as usize));

    let ((ref_tree, (ref_in_levels, ref_levels)), (obs_in_levels, obs_levels)) = rayon::join(
        || {
            let tree = DocTree::build(reference);
            let prepared = prepare_subtree_levels(reference, &tree, ref_required);
            (tree, prepared)
        },
        || prepare_subtree_levels(observed, obs_tree, obs_required),
    );

    let parallel = ref_count + obs_count >= 512 && rayon::current_num_threads() > 1;
    let hash_self = |inst: &SettingsBytecodeInstance, digest: &[u8; 16]| -> u64 {
        let mut hasher = fixed_identity_hasher();
        inst.name.hash(&mut hasher);
        inst.class_name.hash(&mut hasher);
        digest.hash(&mut hasher);
        hasher.finish()
    };
    let ((ref_self_hash, ref_reflexive), obs_self_hash) = rayon::join(
        || {
            let hashes: Vec<u64> = if parallel {
                (0..ref_count)
                    .into_par_iter()
                    .map(|i| {
                        if ref_in_levels[i] {
                            hash_self(&reference.instances[i], &ref_meta[i].exact_digest)
                        } else {
                            0
                        }
                    })
                    .collect()
            } else {
                (0..ref_count)
                    .map(|i| {
                        if ref_in_levels[i] {
                            hash_self(&reference.instances[i], &ref_meta[i].exact_digest)
                        } else {
                            0
                        }
                    })
                    .collect()
            };
            let refl = (0..ref_count)
                .map(|i| ref_in_levels[i] && ref_meta[i].reflexive)
                .collect::<Vec<_>>();
            (hashes, refl)
        },
        || {
            if parallel {
                (0..obs_count)
                    .into_par_iter()
                    .map(|i| {
                        if obs_in_levels[i] {
                            hash_self(&observed.instances[i], &obs_meta[i].exact_digest)
                        } else {
                            0
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                (0..obs_count)
                    .map(|i| {
                        if obs_in_levels[i] {
                            hash_self(&observed.instances[i], &obs_meta[i].exact_digest)
                        } else {
                            0
                        }
                    })
                    .collect::<Vec<_>>()
            }
        },
    );
    drop(ref_in_levels);
    drop(obs_in_levels);

    let mut ref_child_keys: Vec<u32> = vec![0; ref_tree.child_indices.len()];
    let mut obs_child_keys: Vec<u32> = vec![0; obs_tree.child_indices.len()];
    let mut ref_result: Vec<usize> = vec![0; ref_count];
    let mut obs_result: Vec<usize> = vec![0; obs_count];
    let mut next_id = 1usize;

    let max_levels = ref_levels.len().max(obs_levels.len());
    for height in 0..max_levels {
        let r_nodes: &[u32] = ref_levels.get(height).map_or(&[], Vec::as_slice);
        let o_nodes: &[u32] = obs_levels.get(height).map_or(&[], Vec::as_slice);
        let total = r_nodes.len() + o_nodes.len();
        if total == 0 {
            continue;
        }

        if height > 0 {
            for &node in r_nodes {
                let idx = node as usize;
                let s = ref_tree.child_offsets[idx] as usize;
                let e = ref_tree.child_offsets[idx + 1] as usize;
                let dst = &mut ref_child_keys[s..e];
                for (slot, &child) in dst.iter_mut().zip(&ref_tree.child_indices[s..e]) {
                    *slot = ref_result[child as usize] as u32;
                }
                dst.sort_unstable();
            }
            for &node in o_nodes {
                let idx = node as usize;
                let s = obs_tree.child_offsets[idx] as usize;
                let e = obs_tree.child_offsets[idx + 1] as usize;
                let dst = &mut obs_child_keys[s..e];
                for (slot, &child) in dst.iter_mut().zip(&obs_tree.child_indices[s..e]) {
                    *slot = obs_result[child as usize] as u32;
                }
                dst.sort_unstable();
            }
        }

        let full_hash_ref = |node: u32| -> u64 {
            let idx = node as usize;
            let mut h = ref_self_hash[idx];
            if height > 0 {
                let s = ref_tree.child_offsets[idx] as usize;
                let e = ref_tree.child_offsets[idx + 1] as usize;
                let mut hasher = fixed_identity_hasher();
                h.hash(&mut hasher);
                ref_child_keys[s..e].hash(&mut hasher);
                h = hasher.finish();
            }
            h
        };
        let full_hash_obs = |node: u32| -> u64 {
            let idx = node as usize;
            let mut h = obs_self_hash[idx];
            if height > 0 {
                let s = obs_tree.child_offsets[idx] as usize;
                let e = obs_tree.child_offsets[idx + 1] as usize;
                let mut hasher = fixed_identity_hasher();
                h.hash(&mut hasher);
                obs_child_keys[s..e].hash(&mut hasher);
                h = hasher.finish();
            }
            h
        };

        let num_shards = if total >= 512 && rayon::current_num_threads() > 1 {
            64usize
        } else {
            1usize
        };
        let mut shards: Vec<Vec<(u64, u32)>> = if num_shards == 1 {
            vec![Vec::with_capacity(total)]
        } else {
            let per = (total / num_shards).saturating_add(16);
            (0..num_shards).map(|_| Vec::with_capacity(per)).collect()
        };
        if num_shards == 1 {
            let shard = &mut shards[0];
            for &node in r_nodes {
                shard.push((full_hash_ref(node), node));
            }
            for &node in o_nodes {
                shard.push((full_hash_obs(node), node | OBS_BIT));
            }
        } else {
            for &node in r_nodes {
                let h = full_hash_ref(node);
                shards[(h >> 58) as usize].push((h, node));
            }
            for &node in o_nodes {
                let h = full_hash_obs(node);
                shards[(h >> 58) as usize].push((h, node | OBS_BIT));
            }
        }

        let nodes_equal = |left_packed: u32, right_packed: u32| -> bool {
            let (left_obs, left_idx) = (
                (left_packed & OBS_BIT) != 0,
                (left_packed & IDX_MASK) as usize,
            );
            let (right_obs, right_idx) = (
                (right_packed & OBS_BIT) != 0,
                (right_packed & IDX_MASK) as usize,
            );
            let left_inst = if left_obs {
                &observed.instances[left_idx]
            } else {
                &reference.instances[left_idx]
            };
            let right_inst = if right_obs {
                &observed.instances[right_idx]
            } else {
                &reference.instances[right_idx]
            };
            if left_inst.name != right_inst.name || left_inst.class_name != right_inst.class_name {
                return false;
            }
            let left_digest = if left_obs {
                obs_meta[left_idx].exact_digest
            } else {
                ref_meta[left_idx].exact_digest
            };
            let right_digest = if right_obs {
                obs_meta[right_idx].exact_digest
            } else {
                ref_meta[right_idx].exact_digest
            };
            if left_digest != right_digest {
                return false;
            }
            if height > 0 {
                let left_kids = if left_obs {
                    let s = obs_tree.child_offsets[left_idx] as usize;
                    let e = obs_tree.child_offsets[left_idx + 1] as usize;
                    &obs_child_keys[s..e]
                } else {
                    let s = ref_tree.child_offsets[left_idx] as usize;
                    let e = ref_tree.child_offsets[left_idx + 1] as usize;
                    &ref_child_keys[s..e]
                };
                let right_kids = if right_obs {
                    let s = obs_tree.child_offsets[right_idx] as usize;
                    let e = obs_tree.child_offsets[right_idx + 1] as usize;
                    &obs_child_keys[s..e]
                } else {
                    let s = ref_tree.child_offsets[right_idx] as usize;
                    let e = ref_tree.child_offsets[right_idx + 1] as usize;
                    &ref_child_keys[s..e]
                };
                if left_kids != right_kids {
                    return false;
                }
            }
            true
        };

        let intern_shard = |shard: &mut Vec<(u64, u32)>| -> usize {
            shard.sort_unstable_by_key(|&(h, _)| h);
            let mut local_next = 0usize;
            let mut reps: Vec<(u32, usize)> = Vec::new();
            let mut pos = 0usize;
            while pos < shard.len() {
                let h = shard[pos].0;
                let mut end = pos + 1;
                while end < shard.len() && shard[end].0 == h {
                    end += 1;
                }
                if end == pos + 1 {
                    shard[pos].0 = local_next as u64;
                    local_next += 1;
                } else {
                    reps.clear();
                    for item in &mut shard[pos..end] {
                        let packed = item.1;
                        let found = reps
                            .iter()
                            .find(|&&(rep, _)| nodes_equal(packed, rep))
                            .map(|&(_, id)| id);
                        let id = match found {
                            Some(id) => id,
                            None => {
                                let id = local_next;
                                local_next += 1;
                                reps.push((packed, id));
                                id
                            }
                        };
                        item.0 = id as u64;
                    }
                }
                pos = end;
            }
            local_next
        };

        let counts: Vec<usize> = if num_shards > 1 {
            shards.par_iter_mut().map(intern_shard).collect()
        } else {
            shards.iter_mut().map(intern_shard).collect()
        };

        for (shard, count) in shards.into_iter().zip(counts) {
            let base = next_id;
            next_id += count;
            for (local_id, packed) in shard {
                let id = base + local_id as usize;
                let idx = (packed & IDX_MASK) as usize;
                if (packed & OBS_BIT) != 0 {
                    obs_result[idx] = id;
                } else {
                    ref_result[idx] = id;
                }
            }
        }
    }

    ExactSubtreeKeys {
        keys: (ref_result, obs_result),
        ref_reflexive,
    }
}

const COMPACT_HYDRATE_BATCH: usize = 16_384;

/// Runs only the O(1) exact-subtree fast path (reading zero properties).
fn exact_subtree_fast_proposals(
    reference_graph: &ReferenceGraph,
    observed_graph: &ReferenceGraph,
    assigned_observed: &[Option<usize>],
    exact: &ExactSubtreeKeys,
    observed_group: &[usize],
    candidates: &[usize],
) -> (Vec<Option<usize>>, Vec<(usize, usize)>) {
    let unreferenced = |index: usize| !observed_graph.has_edges(index);
    let mut results: Vec<Option<usize>> = vec![None; observed_group.len()];
    let mut fallback: Vec<(usize, usize)> = Vec::new();
    if candidates.len() <= 8 {
        let mut eligible = [(0usize, 0usize); 8];
        let mut eligible_len = 0usize;
        for &candidate in candidates {
            let key = exact.keys.0[candidate];
            if key == 0 || reference_graph.has_outgoing(candidate) {
                continue;
            }
            if reference_graph
                .incoming(candidate)
                .iter()
                .any(|edge| assigned_observed[edge.source].is_some())
            {
                continue;
            }
            eligible[eligible_len] = (key, candidate);
            eligible_len += 1;
        }
        for (pos, &index) in observed_group.iter().enumerate() {
            if unreferenced(index) {
                let obs_key = exact.keys.1[index];
                if obs_key != 0 {
                    let mut first_cand = None;
                    let mut count = 0usize;
                    for &(key, cand) in &eligible[..eligible_len] {
                        if key == obs_key {
                            if first_cand.is_none() {
                                first_cand = Some(cand);
                            }
                            count += 1;
                        }
                    }
                    if let Some(candidate) = first_cand
                        && exact.ref_reflexive[candidate]
                    {
                        if count == 1 {
                            results[pos] = Some(candidate);
                        }
                        continue;
                    }
                }
            }
            fallback.push((pos, index));
        }
    } else {
        let mut by_subtree: AHashMap<usize, (usize, usize)> =
            AHashMap::with_capacity(candidates.len());
        for &candidate in candidates {
            let key = exact.keys.0[candidate];
            if key == 0 || reference_graph.has_outgoing(candidate) {
                continue;
            }
            if reference_graph
                .incoming(candidate)
                .iter()
                .any(|edge| assigned_observed[edge.source].is_some())
            {
                continue;
            }
            by_subtree
                .entry(key)
                .and_modify(|(_, count)| *count += 1)
                .or_insert((candidate, 1));
        }
        for (pos, &index) in observed_group.iter().enumerate() {
            if unreferenced(index) {
                let obs_key = exact.keys.1[index];
                if obs_key != 0
                    && let Some(&(candidate, count)) = by_subtree.get(&obs_key)
                    && exact.ref_reflexive[candidate]
                {
                    if count == 1 {
                        results[pos] = Some(candidate);
                    }
                    continue;
                }
            }
            fallback.push((pos, index));
        }
    }
    (results, fallback)
}

fn score_fallback_entries(
    context: &IdentityScoreContext<'_>,
    representatives: &[usize],
    multiplicities: &AHashMap<usize, usize>,
    content_index: Option<&IdentityCandidateIndex>,
    fallback: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let unreferenced = |index: usize| !context.observed_graph.has_edges(index);
    let propose = |&(pos, index): &(usize, usize)| {
        let mut best = None;
        let mut best_candidate = None;
        let mut tied = false;
        let obs_inst = context.observed.get(index);
        let narrowed_range = content_index
            .filter(|_| unreferenced(index))
            .and_then(|lookup| lookup.matching_range(obs_inst));
        let secondary_filter = narrowed_range
            .as_ref()
            .filter(|range| range.len() > 4)
            .and_then(|_| content_index?.secondary.as_ref())
            .and_then(|(sec_field, sec_values)| {
                let val = sec_field.read(obs_inst)?;
                Some((val, identity_number_radius(val), sec_values.as_slice()))
            });
        let mut visit_candidate = |candidate: usize| {
            let Some(score) = identity_score(context, candidate, index) else {
                return;
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
        };
        if let Some(range) = narrowed_range {
            let values = &content_index.unwrap().values;
            for slot in range {
                if let Some((sec_val, sec_radius, sec_values)) = secondary_filter {
                    let cand_sec = sec_values[slot];
                    if cand_sec.is_finite() && (cand_sec - sec_val).abs() > sec_radius {
                        continue;
                    }
                }
                visit_candidate(values[slot].1);
            }
        } else {
            for &candidate in representatives {
                visit_candidate(candidate);
            }
        }
        best_candidate
            .filter(|_| !tied)
            .map(|candidate| (pos, candidate))
    };
    if fallback.len() >= 32 && rayon::current_num_threads() > 1 {
        fallback.par_iter().filter_map(propose).collect()
    } else {
        fallback.iter().filter_map(propose).collect()
    }
}

/// Filters a ref-free group's fallback observed rows and candidate
/// representatives using `CompactRowMeta` alone (zero hydration):
/// 1. Any observed row whose `keys_hash` matches zero candidates can never have
///    `holds_same_content == true` (and has no reference evidence), so it scores
///    `None` for every candidate.
/// 2. When no candidate matches an observed row's exact subtree key, every
///    candidate has `subtree_matches == false`, so any observed row with >= 2
///    candidates matching its `exact_digest` (with `reflexive == true`) is
///    guaranteed tied at the maximum possible score `(false, 0, true, false)`.
/// 3. Any candidate representative whose `keys_hash` matches no remaining
///    observed row cannot score for any remaining observed row.
fn prune_ref_free_fallback(
    fallback: Vec<(usize, usize)>,
    representatives: Vec<usize>,
    multiplicities: &AHashMap<usize, usize>,
    exact: Option<&ExactSubtreeKeys>,
    ref_meta: &[CompactRowMeta],
    obs_meta: &[CompactRowMeta],
) -> (Vec<(usize, usize)>, Vec<usize>) {
    if fallback.is_empty() || representatives.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut ref_keys = AHashSet::with_capacity(representatives.len().min(64));
    let mut ref_exact_counts: AHashMap<[u8; 16], usize> = AHashMap::new();
    let mut ref_nonrefl_subtrees = AHashSet::new();
    for &rep in &representatives {
        let meta = ref_meta[rep];
        ref_keys.insert(meta.keys_hash);
        if meta.reflexive {
            *ref_exact_counts.entry(meta.exact_digest).or_default() += multiplicities[&rep];
        }
        if let Some(exact) = exact
            && !exact.ref_reflexive[rep]
            && exact.keys.0[rep] != 0
        {
            ref_nonrefl_subtrees.insert(exact.keys.0[rep]);
        }
    }
    let mut obs_keys_needed = AHashSet::new();
    let mut pruned_fallback = Vec::with_capacity(fallback.len());
    for (pos, obs_idx) in fallback {
        let meta = obs_meta[obs_idx];
        if !ref_keys.contains(&meta.keys_hash) {
            continue;
        }
        let could_have_subtree_match = exact.is_some_and(|exact| {
            let k = exact.keys.1[obs_idx];
            k != 0 && ref_nonrefl_subtrees.contains(&k)
        });
        if !could_have_subtree_match
            && ref_exact_counts
                .get(&meta.exact_digest)
                .is_some_and(|&count| count >= 2)
        {
            continue;
        }
        obs_keys_needed.insert(meta.keys_hash);
        pruned_fallback.push((pos, obs_idx));
    }
    if pruned_fallback.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let pruned_reps: Vec<usize> = representatives
        .into_iter()
        .filter(|&rep| obs_keys_needed.contains(&ref_meta[rep].keys_hash))
        .collect();
    (pruned_fallback, pruned_reps)
}

struct CompactRefBearingBatch {
    end_group_idx: usize,
    ref_indices: Vec<usize>,
    obs_indices: Vec<usize>,
}

struct CompactRefBearingCache<'a> {
    compact: &'a CompactMatching<'a>,
    batches: std::collections::VecDeque<CompactRefBearingBatch>,
    loaded_end: Option<usize>,
    ref_rows: AHashMap<usize, SettingsBytecodeInstance>,
    obs_rows: AHashMap<usize, SettingsBytecodeInstance>,
}

impl<'a> CompactRefBearingCache<'a> {
    fn plan(
        groups: &[(IdentityGroupKey<'_>, GroupIndices)],
        ref_bearing_groups: &[(usize, &[usize])],
        inputs: &ScoreInputs<'_, '_>,
        compact: &'a CompactMatching<'a>,
    ) -> Self {
        let mut batches = std::collections::VecDeque::new();
        let mut cur_ref: Vec<usize> = Vec::new();
        let mut cur_obs: Vec<usize> = Vec::new();
        let mut cur_end = 0usize;
        for &(group_idx, candidates) in ref_bearing_groups {
            let observed_slice = groups[group_idx].1.as_slice();
            let (representatives, _) = compact_candidate_representatives(
                inputs.reference_graph,
                inputs.subtree_keys,
                compact.ref_meta,
                candidates,
            );
            let mut need_ref = Vec::new();
            let mut need_obs = Vec::new();
            let mut seen_ref = AHashSet::new();
            let mut any_unref_ambiguous = false;
            for &obs_idx in observed_slice {
                let obs_m = compact.obs_meta[obs_idx];
                let mut obs_ambiguous = false;
                for &rep in &representatives {
                    let ref_m = compact.ref_meta[rep];
                    if ref_m.keys_hash == obs_m.keys_hash
                        && !(ref_m.reflexive && ref_m.exact_digest == obs_m.exact_digest)
                    {
                        obs_ambiguous = true;
                        if seen_ref.insert(rep) {
                            need_ref.push(rep);
                        }
                    }
                }
                if obs_ambiguous {
                    need_obs.push(obs_idx);
                    if !inputs.observed_graph.has_edges(obs_idx) {
                        any_unref_ambiguous = true;
                    }
                }
            }
            if need_obs.is_empty() {
                continue;
            }
            if representatives.len() >= 32 && any_unref_ambiguous {
                for &rep in &representatives {
                    if seen_ref.insert(rep) {
                        need_ref.push(rep);
                    }
                }
            }
            if need_ref.len() + need_obs.len() > COMPACT_HYDRATE_BATCH {
                if !cur_obs.is_empty() {
                    batches.push_back(CompactRefBearingBatch {
                        end_group_idx: cur_end,
                        ref_indices: std::mem::take(&mut cur_ref),
                        obs_indices: std::mem::take(&mut cur_obs),
                    });
                }
                continue;
            }
            if !cur_obs.is_empty()
                && cur_ref.len() + cur_obs.len() + need_ref.len() + need_obs.len()
                    > COMPACT_HYDRATE_BATCH
            {
                batches.push_back(CompactRefBearingBatch {
                    end_group_idx: cur_end,
                    ref_indices: std::mem::take(&mut cur_ref),
                    obs_indices: std::mem::take(&mut cur_obs),
                });
            }
            cur_ref.extend(need_ref);
            cur_obs.extend(need_obs);
            cur_end = group_idx;
        }
        if !cur_obs.is_empty() {
            batches.push_back(CompactRefBearingBatch {
                end_group_idx: cur_end,
                ref_indices: cur_ref,
                obs_indices: cur_obs,
            });
        }
        Self {
            compact,
            batches,
            loaded_end: None,
            ref_rows: AHashMap::new(),
            obs_rows: AHashMap::new(),
        }
    }

    fn ensure_loaded(&mut self, group_idx: usize) -> Result<()> {
        if self.loaded_end.is_some_and(|end| group_idx <= end) {
            return Ok(());
        }
        while let Some(front) = self.batches.front() {
            if group_idx <= front.end_group_idx {
                let batch = self.batches.pop_front().unwrap();
                self.ref_rows.clear();
                self.obs_rows.clear();
                let (ref_res, obs_res) = rayon::join(
                    || (self.compact.hydrate_ref)(&batch.ref_indices),
                    || (self.compact.hydrate_obs)(&batch.obs_indices),
                );
                self.ref_rows = ref_res?.into_iter().collect();
                self.obs_rows = obs_res?.into_iter().collect();
                self.loaded_end = Some(batch.end_group_idx);
                return Ok(());
            }
            self.batches.pop_front();
        }
        self.ref_rows.clear();
        self.obs_rows.clear();
        self.loaded_end = None;
        Ok(())
    }
}

type ScoredProposals = (Vec<(usize, usize)>, std::time::Duration);

fn log_slow_identity_group(
    key: IdentityGroupKey<'_>,
    observed_count: usize,
    candidate_count: usize,
    elapsed: std::time::Duration,
) {
    if elapsed.as_millis() >= 10 {
        log_global(
            4,
            format_args!(
                "[renium] identity candidate group: parent={:?} name={:?} class={} observed={observed_count} candidates={candidate_count} elapsed_ms={:.1}",
                key.0,
                key.1,
                key.2,
                elapsed.as_secs_f64() * 1000.0,
            ),
        );
    }
}

fn compact_group_has_ambiguous_pair(
    fallback: &[(usize, usize)],
    representatives: &[usize],
    compact: &CompactMatching<'_>,
) -> bool {
    fallback.iter().any(|&(_, obs_idx)| {
        let obs_m = compact.obs_meta[obs_idx];
        representatives.iter().any(|&rep| {
            let ref_m = compact.ref_meta[rep];
            ref_m.keys_hash == obs_m.keys_hash
                && !(ref_m.reflexive && ref_m.exact_digest == obs_m.exact_digest)
        })
    })
}

fn score_compact_with_content_check(
    fallback: &[(usize, usize)],
    representatives: &[usize],
    multiplicities: &AHashMap<usize, usize>,
    inputs: &ScoreInputs<'_, '_>,
    (assigned_reference, assigned_observed): (&[Option<usize>], &[Option<usize>]),
    results: &mut [Option<usize>],
    content_matches_fn: impl Fn(usize, usize) -> bool,
) {
    for &(pos, obs_idx) in fallback {
        let mut best = None;
        let mut best_candidate = None;
        let mut tied = false;
        for &candidate in representatives {
            let Some(reference_evidence) = matching_reference_evidence(
                candidate,
                obs_idx,
                inputs.reference_graph,
                inputs.observed_graph,
                assigned_reference,
                assigned_observed,
            ) else {
                continue;
            };
            let content_matches = content_matches_fn(candidate, obs_idx);
            if reference_evidence == 0 && !content_matches {
                continue;
            }
            let score = IdentityScore {
                has_reference_evidence: reference_evidence != 0,
                reference_evidence,
                content_matches,
                subtree_matches: inputs.subtree_keys.is_some_and(|exact| {
                    exact.keys.0[candidate] != 0 && exact.keys.0[candidate] == exact.keys.1[obs_idx]
                }),
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
        if let Some(candidate) = best_candidate.filter(|_| !tied) {
            results[pos] = Some(candidate);
        }
    }
}

fn try_score_compact_from_cache(
    fallback: &[(usize, usize)],
    representatives: &[usize],
    multiplicities: &AHashMap<usize, usize>,
    inputs: &ScoreInputs<'_, '_>,
    assignments: (&[Option<usize>], &[Option<usize>]),
    compact: &CompactMatching<'_>,
    cache: &CompactRefBearingCache<'_>,
) -> Option<Vec<(usize, usize)>> {
    let use_index = representatives.len() >= 32
        && fallback
            .iter()
            .any(|&(_, index)| !inputs.observed_graph.has_edges(index));
    let all_cached_present = if use_index {
        representatives
            .iter()
            .all(|r| cache.ref_rows.contains_key(r))
            && fallback.iter().all(|&(_, o)| {
                let obs_m = compact.obs_meta[o];
                let needs = representatives.iter().any(|&r| {
                    let ref_m = compact.ref_meta[r];
                    ref_m.keys_hash == obs_m.keys_hash
                        && !(ref_m.reflexive && ref_m.exact_digest == obs_m.exact_digest)
                });
                !needs || cache.obs_rows.contains_key(&o)
            })
    } else {
        fallback.iter().all(|&(_, o)| {
            let obs_m = compact.obs_meta[o];
            representatives.iter().all(|&r| {
                let ref_m = compact.ref_meta[r];
                !(ref_m.keys_hash == obs_m.keys_hash
                    && !(ref_m.reflexive && ref_m.exact_digest == obs_m.exact_digest))
                    || (cache.ref_rows.contains_key(&r) && cache.obs_rows.contains_key(&o))
            })
        })
    };
    if !all_cached_present {
        return None;
    }
    if use_index
        && fallback
            .iter()
            .all(|&(_, o)| cache.obs_rows.contains_key(&o))
    {
        let content_index =
            IdentityCandidateIndex::build(RowSource::Hydrated(&cache.ref_rows), representatives);
        let context = IdentityScoreContext {
            reference: RowSource::Hydrated(&cache.ref_rows),
            observed: RowSource::Hydrated(&cache.obs_rows),
            reference_graph: inputs.reference_graph,
            observed_graph: inputs.observed_graph,
            assigned_reference: assignments.0,
            assigned_observed: assignments.1,
            subtree_keys: inputs.subtree_keys.map(|exact| &exact.keys),
        };
        return Some(score_fallback_entries(
            &context,
            representatives,
            multiplicities,
            content_index.as_ref(),
            fallback,
        ));
    }
    let mut resolved = Vec::new();
    let mut temp = vec![None; fallback.len()];
    let local_fallback: Vec<(usize, usize)> = fallback
        .iter()
        .enumerate()
        .map(|(i, &(_, obs_idx))| (i, obs_idx))
        .collect();
    score_compact_with_content_check(
        &local_fallback,
        representatives,
        multiplicities,
        inputs,
        assignments,
        &mut temp,
        |candidate, obs_idx| {
            let ref_m = compact.ref_meta[candidate];
            let obs_m = compact.obs_meta[obs_idx];
            if ref_m.keys_hash != obs_m.keys_hash {
                false
            } else if ref_m.reflexive && ref_m.exact_digest == obs_m.exact_digest {
                true
            } else {
                holds_same_content_hint(
                    &cache.ref_rows[&candidate],
                    &cache.obs_rows[&obs_idx],
                    !inputs.reference_graph.has_outgoing(candidate),
                    !inputs.observed_graph.has_outgoing(obs_idx),
                )
            }
        },
    );
    for (local_i, &(pos, _)) in fallback.iter().enumerate() {
        if let Some(candidate) = temp[local_i] {
            resolved.push((pos, candidate));
        }
    }
    Some(resolved)
}

fn score_compact_single_group(
    observed_group: &[usize],
    candidates: &[usize],
    inputs: &ScoreInputs<'_, '_>,
    assignments: (&[Option<usize>], &[Option<usize>]),
    compact: &CompactMatching<'_>,
    cache: Option<(&mut CompactRefBearingCache<'_>, usize)>,
) -> Result<Vec<(usize, usize)>> {
    let (assigned_reference, assigned_observed) = assignments;
    let (mut results, fallback) = if let Some(exact) = inputs.subtree_keys {
        exact_subtree_fast_proposals(
            inputs.reference_graph,
            inputs.observed_graph,
            assigned_observed,
            exact,
            observed_group,
            candidates,
        )
    } else {
        (
            vec![None; observed_group.len()],
            observed_group
                .iter()
                .enumerate()
                .map(|(pos, &idx)| (pos, idx))
                .collect(),
        )
    };
    let finish = |results: Vec<Option<usize>>| {
        observed_group
            .iter()
            .zip(results)
            .filter_map(|(&index, candidate)| candidate.map(|candidate| (index, candidate)))
            .collect()
    };
    if fallback.is_empty() {
        return Ok(finish(results));
    }
    let (representatives, multiplicities) = compact_candidate_representatives(
        inputs.reference_graph,
        inputs.subtree_keys,
        compact.ref_meta,
        candidates,
    );
    if !compact_group_has_ambiguous_pair(&fallback, &representatives, compact) {
        score_compact_with_content_check(
            &fallback,
            &representatives,
            &multiplicities,
            inputs,
            assignments,
            &mut results,
            |candidate, obs_idx| {
                let ref_m = compact.ref_meta[candidate];
                ref_m.reflexive && ref_m.exact_digest == compact.obs_meta[obs_idx].exact_digest
            },
        );
        return Ok(finish(results));
    }

    if let Some((cache, group_idx)) = cache {
        cache.ensure_loaded(group_idx)?;
        if let Some(resolved) = try_score_compact_from_cache(
            &fallback,
            &representatives,
            &multiplicities,
            inputs,
            assignments,
            compact,
            cache,
        ) {
            for (pos, candidate) in resolved {
                results[pos] = Some(candidate);
            }
            return Ok(finish(results));
        }
    }

    let ref_rows: AHashMap<usize, SettingsBytecodeInstance> =
        (compact.hydrate_ref)(&representatives)?
            .into_iter()
            .collect();
    let content_index = if representatives.len() > 1
        && fallback
            .iter()
            .any(|&(_, index)| !inputs.observed_graph.has_edges(index))
    {
        IdentityCandidateIndex::build(RowSource::Hydrated(&ref_rows), &representatives)
    } else {
        None
    };
    for chunk in fallback.chunks(COMPACT_HYDRATE_BATCH) {
        let obs_indices: Vec<usize> = chunk.iter().map(|&(_, idx)| idx).collect();
        let obs_rows: AHashMap<usize, SettingsBytecodeInstance> =
            (compact.hydrate_obs)(&obs_indices)?.into_iter().collect();
        let context = IdentityScoreContext {
            reference: RowSource::Hydrated(&ref_rows),
            observed: RowSource::Hydrated(&obs_rows),
            reference_graph: inputs.reference_graph,
            observed_graph: inputs.observed_graph,
            assigned_reference,
            assigned_observed,
            subtree_keys: inputs.subtree_keys.map(|exact| &exact.keys),
        };
        for (pos, candidate) in score_fallback_entries(
            &context,
            &representatives,
            &multiplicities,
            content_index.as_ref(),
            chunk,
        ) {
            results[pos] = Some(candidate);
        }
    }
    Ok(finish(results))
}

fn score_compact_ref_free_jobs(
    groups: &[(IdentityGroupKey<'_>, GroupIndices)],
    ref_free_jobs: &[(usize, Vec<usize>)],
    inputs: &ScoreInputs<'_, '_>,
    assigned_reference: &[Option<usize>],
    assigned_observed: &[Option<usize>],
    compact: &CompactMatching<'_>,
) -> Result<Vec<ScoredProposals>> {
    struct PendingFallback {
        slot: usize,
        observed_group: Vec<usize>,
        results: Vec<Option<usize>>,
        fallback: Vec<(usize, usize)>,
        representatives: Vec<usize>,
        multiplicities: AHashMap<usize, usize>,
        min_ref_row: usize,
    }

    let mut final_results: Vec<Option<ScoredProposals>> = vec![None; ref_free_jobs.len()];
    let fast_pass = |(slot, (group_idx, candidates)): (usize, &(usize, Vec<usize>))| {
        let started = Instant::now();
        let observed_slice = groups[*group_idx].1.as_slice();
        let (results, fallback) = if let Some(exact) = inputs.subtree_keys {
            exact_subtree_fast_proposals(
                inputs.reference_graph,
                inputs.observed_graph,
                assigned_observed,
                exact,
                observed_slice,
                candidates,
            )
        } else {
            (
                vec![None; observed_slice.len()],
                observed_slice
                    .iter()
                    .enumerate()
                    .map(|(pos, &idx)| (pos, idx))
                    .collect(),
            )
        };
        if fallback.is_empty() {
            let proposals = observed_slice
                .iter()
                .zip(results)
                .filter_map(|(&index, candidate)| candidate.map(|candidate| (index, candidate)))
                .collect();
            return (slot, Ok((proposals, started.elapsed())));
        }
        let (representatives, multiplicities) = compact_candidate_representatives(
            inputs.reference_graph,
            inputs.subtree_keys,
            compact.ref_meta,
            candidates,
        );
        let (fallback, representatives) = prune_ref_free_fallback(
            fallback,
            representatives,
            &multiplicities,
            inputs.subtree_keys,
            compact.ref_meta,
            compact.obs_meta,
        );
        if fallback.is_empty() || representatives.is_empty() {
            let proposals = observed_slice
                .iter()
                .zip(results)
                .filter_map(|(&index, candidate)| candidate.map(|candidate| (index, candidate)))
                .collect();
            return (slot, Ok((proposals, started.elapsed())));
        }
        let min_ref_row = representatives.iter().copied().min().unwrap_or(0);
        (
            slot,
            Err(PendingFallback {
                slot,
                observed_group: observed_slice.to_vec(),
                results,
                fallback,
                representatives,
                multiplicities,
                min_ref_row,
            }),
        )
    };
    let outcomes: Vec<_> = if ref_free_jobs.len() >= 2 && rayon::current_num_threads() > 1 {
        ref_free_jobs
            .par_iter()
            .enumerate()
            .map(fast_pass)
            .collect()
    } else {
        ref_free_jobs.iter().enumerate().map(fast_pass).collect()
    };

    let mut pending: Vec<PendingFallback> = Vec::new();
    for (slot, outcome) in outcomes {
        match outcome {
            Ok(done) => final_results[slot] = Some(done),
            Err(job) => pending.push(job),
        }
    }
    pending.sort_unstable_by_key(|job| job.min_ref_row);

    let mut cursor = 0usize;
    while cursor < pending.len() {
        if pending[cursor].fallback.len() > COMPACT_HYDRATE_BATCH {
            let job = &mut pending[cursor];
            let started = Instant::now();
            let ref_rows: AHashMap<usize, SettingsBytecodeInstance> =
                (compact.hydrate_ref)(&job.representatives)?
                    .into_iter()
                    .collect();
            let content_index = if job.representatives.len() > 1 {
                IdentityCandidateIndex::build(RowSource::Hydrated(&ref_rows), &job.representatives)
            } else {
                None
            };
            for chunk in job.fallback.chunks(COMPACT_HYDRATE_BATCH) {
                let obs_indices: Vec<usize> = chunk.iter().map(|&(_, idx)| idx).collect();
                let obs_rows: AHashMap<usize, SettingsBytecodeInstance> =
                    (compact.hydrate_obs)(&obs_indices)?.into_iter().collect();
                let context = IdentityScoreContext {
                    reference: RowSource::Hydrated(&ref_rows),
                    observed: RowSource::Hydrated(&obs_rows),
                    reference_graph: inputs.reference_graph,
                    observed_graph: inputs.observed_graph,
                    assigned_reference,
                    assigned_observed,
                    subtree_keys: inputs.subtree_keys.map(|exact| &exact.keys),
                };
                for (pos, candidate) in score_fallback_entries(
                    &context,
                    &job.representatives,
                    &job.multiplicities,
                    content_index.as_ref(),
                    chunk,
                ) {
                    job.results[pos] = Some(candidate);
                }
            }
            let proposals = job
                .observed_group
                .iter()
                .zip(job.results.iter().copied())
                .filter_map(|(&index, candidate)| candidate.map(|candidate| (index, candidate)))
                .collect();
            final_results[job.slot] = Some((proposals, started.elapsed()));
            cursor += 1;
            continue;
        }

        let batch_start = cursor;
        let mut ref_set = Vec::new();
        let mut obs_set = Vec::new();
        while cursor < pending.len() {
            let add_ref = pending[cursor].representatives.len();
            let add_obs = pending[cursor].fallback.len();
            if cursor > batch_start
                && ref_set.len() + obs_set.len() + add_ref + add_obs > COMPACT_HYDRATE_BATCH
            {
                break;
            }
            ref_set.extend_from_slice(&pending[cursor].representatives);
            obs_set.extend(pending[cursor].fallback.iter().map(|&(_, idx)| idx));
            cursor += 1;
        }
        let (ref_res, obs_res) = rayon::join(
            || (compact.hydrate_ref)(&ref_set),
            || (compact.hydrate_obs)(&obs_set),
        );
        let ref_rows: AHashMap<usize, SettingsBytecodeInstance> = ref_res?.into_iter().collect();
        let obs_rows: AHashMap<usize, SettingsBytecodeInstance> = obs_res?.into_iter().collect();
        let context = IdentityScoreContext {
            reference: RowSource::Hydrated(&ref_rows),
            observed: RowSource::Hydrated(&obs_rows),
            reference_graph: inputs.reference_graph,
            observed_graph: inputs.observed_graph,
            assigned_reference,
            assigned_observed,
            subtree_keys: inputs.subtree_keys.map(|exact| &exact.keys),
        };
        let batch_slice = &mut pending[batch_start..cursor];
        let score_one = |job: &mut PendingFallback| {
            let started = Instant::now();
            let content_index = if job.representatives.len() > 1 {
                IdentityCandidateIndex::build(RowSource::Hydrated(&ref_rows), &job.representatives)
            } else {
                None
            };
            for (pos, candidate) in score_fallback_entries(
                &context,
                &job.representatives,
                &job.multiplicities,
                content_index.as_ref(),
                &job.fallback,
            ) {
                job.results[pos] = Some(candidate);
            }
            let proposals = job
                .observed_group
                .iter()
                .zip(job.results.iter().copied())
                .filter_map(|(&index, candidate)| candidate.map(|candidate| (index, candidate)))
                .collect();
            (job.slot, (proposals, started.elapsed()))
        };
        let scored: Vec<(usize, ScoredProposals)> =
            if batch_slice.len() >= 2 && rayon::current_num_threads() > 1 {
                batch_slice.par_iter_mut().map(score_one).collect()
            } else {
                batch_slice.iter_mut().map(score_one).collect()
            };
        for (slot, result) in scored {
            final_results[slot] = Some(result);
        }
    }

    Ok(final_results.into_iter().map(Option::unwrap).collect())
}

fn compact_persistent_identities_follow_mapping(
    ref_doc: &SettingsBytecode,
    ref_unique_ids: &[Option<Box<str>>],
    obs_doc: &SettingsBytecode,
    obs_unique_ids: &[Option<Box<str>>],
    reference_position: impl Fn(usize) -> Option<usize>,
) -> bool {
    let reference_ids = persistent_identity_index_from_slice(ref_unique_ids);
    if reference_ids.is_empty() {
        return true;
    }
    let observed_ids = persistent_identity_index_from_slice(obs_unique_ids);
    observed_ids.iter().all(|(id, observed_index)| {
        let (Some(reference_index), Some(observed_index)) =
            (reference_ids.get(id).copied().flatten(), *observed_index)
        else {
            return true;
        };
        reference_position(observed_index) == Some(reference_index)
            || is_reconciliation_protected_workspace_camera(ref_doc, reference_index)
                && is_reconciliation_protected_workspace_camera(obs_doc, observed_index)
    })
}

fn compact_reference_viewport_camera(compact: &CompactDocument) -> Option<ReferenceViewportCamera> {
    let root =
        compact.document.instances.iter().find(|instance| {
            instance.parent_index.is_none() && instance.class_name == "Workspace"
        })?;
    Some(ReferenceViewportCamera {
        root_names_camera: root.properties.contains_key("CurrentCamera"),
        current: workspace_current_camera_index(&compact.document)
            .map(|index| compact.document.instances[index].settings_id.clone()),
        cameras: compact.cameras.iter().map(|(_, cam)| cam.clone()).collect(),
    })
}

fn compact_viewport_camera_lacks_enum_types(compact: &CompactDocument) -> bool {
    let Some(index) = workspace_current_camera_index(&compact.document) else {
        return false;
    };
    compact
        .cameras
        .iter()
        .find(|(cam_idx, _)| *cam_idx == index)
        .is_some_and(|(_, cam)| instance_lacks_enum_types(cam))
}

fn compact_kept_camera<'a>(
    viewport_camera: Option<&'a ReferenceViewportCamera>,
    obs_doc: &SettingsBytecode,
    obs_cameras: &[(usize, SettingsBytecodeInstance)],
    obs_camera_id: Option<&str>,
) -> Option<(usize, &'a SettingsBytecodeInstance)> {
    let reference = viewport_camera?;
    let index = workspace_current_camera_index(obs_doc)?;
    let id = obs_camera_id.unwrap_or(obs_doc.instances[index].settings_id.as_str());
    if reference.root_names_camera && reference.current.as_deref() != Some(id) {
        return None;
    }
    let saved = reference
        .cameras
        .iter()
        .find(|camera| camera.settings_id == id)?;
    let obs_cam = obs_cameras
        .iter()
        .find(|(cam_idx, _)| *cam_idx == index)
        .map(|(_, cam)| cam);
    if let Some(obs_cam) = obs_cam
        && obs_cam.properties == saved.properties
        && obs_cam.attributes == saved.attributes
    {
        return None;
    }
    Some((index, saved))
}

fn compact_same_instances(
    reference: &CompactDocument,
    observed: &CompactDocument,
    kept_camera: Option<(usize, &SettingsBytecodeInstance)>,
) -> bool {
    reference.document.instances.len() == observed.document.instances.len()
        && reference
            .document
            .instances
            .par_iter()
            .enumerate()
            .all(|(i, ref_inst)| {
                let obs_inst = &observed.document.instances[i];
                if ref_inst.settings_id != obs_inst.settings_id
                    || ref_inst.name != obs_inst.name
                    || ref_inst.class_name != obs_inst.class_name
                    || ref_inst.parent_index != obs_inst.parent_index
                {
                    return false;
                }
                if let Some((cam_idx, saved_cam)) = kept_camera
                    && cam_idx == i
                {
                    return reference
                        .cameras
                        .iter()
                        .find(|(idx, _)| *idx == i)
                        .is_some_and(|(_, ref_cam)| {
                            ref_cam.properties == saved_cam.properties
                                && ref_cam.attributes == saved_cam.attributes
                        });
                }
                reference.meta[i].full_digest == observed.meta[i].full_digest
                    && reference.meta[i].unique_id_digest == observed.meta[i].unique_id_digest
                    && ref_inst.properties == obs_inst.properties
                    && ref_inst.attributes == obs_inst.attributes
            })
}

fn compact_positional_values_equivalent(
    reference: &CompactDocument,
    observed: &CompactDocument,
    hydrate_ref: HydrateRowsFn<'_>,
    hydrate_obs: HydrateRowsFn<'_>,
    identity_checks: Option<&[bool]>,
    preserve_viewport: bool,
    kept_camera: Option<(usize, &SettingsBytecodeInstance)>,
) -> Result<bool> {
    if workspace_current_camera_index(&reference.document)
        != workspace_current_camera_index(&observed.document)
    {
        return Ok(false);
    }
    let count = observed.document.instances.len();
    let reference_ids = reference
        .document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let observed_ids = observed
        .document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let ids = PairedStoreIds {
        service: store_service(&reference.document),
        left: &reference_ids,
        right: &observed_ids,
    };

    let mut need_hydrate: Vec<usize> = Vec::new();
    for i in 0..count {
        let ref_inst = &reference.document.instances[i];
        let obs_inst = &observed.document.instances[i];
        let is_kept_cam = kept_camera.is_some_and(|(idx, _)| idx == i);
        if !preserve_viewport
            && !is_kept_cam
            && reference.meta[i].unique_id_digest != observed.meta[i].unique_id_digest
        {
            return Ok(false);
        }
        if identity_checks.is_some_and(|checks| !checks[i])
            && !reference.meta[i].has_ref
            && !observed.meta[i].has_ref
        {
            continue;
        }
        if ref_inst.class_name == "PackageLink"
            || preserve_viewport
                && is_reconciliation_protected_workspace_camera(&reference.document, i)
                && is_reconciliation_protected_workspace_camera(&observed.document, i)
        {
            continue;
        }
        if is_kept_cam {
            need_hydrate.push(i);
            continue;
        }
        if reference.meta[i].compared_digest == observed.meta[i].compared_digest {
            if !reference.meta[i].has_ref && !observed.meta[i].has_ref {
                continue;
            }
            if reconciliation_maps_equal_with_ids(
                &ref_inst.class_name,
                &ref_inst.properties,
                &obs_inst.properties,
                ids,
            ) && reconciliation_values_map_equal_with_ids(
                &ref_inst.attributes,
                &obs_inst.attributes,
                ids,
            ) {
                continue;
            }
        }
        need_hydrate.push(i);
    }

    let check_chunk = |chunk: &[usize]| -> Result<bool> {
        let (ref_res, obs_res) = rayon::join(|| hydrate_ref(chunk), || hydrate_obs(chunk));
        let ref_rows: AHashMap<usize, SettingsBytecodeInstance> = ref_res?.into_iter().collect();
        let obs_rows: AHashMap<usize, SettingsBytecodeInstance> = obs_res?.into_iter().collect();
        Ok(chunk.par_iter().all(|&i| {
            let ref_inst = &ref_rows[&i];
            let obs_inst = match kept_camera {
                Some((cam_idx, saved)) if cam_idx == i => saved,
                _ => &obs_rows[&i],
            };
            if !preserve_viewport
                && ref_inst.properties.get("UniqueId") != obs_inst.properties.get("UniqueId")
            {
                return false;
            }
            reconciliation_maps_equal_with_ids(
                &ref_inst.class_name,
                &ref_inst.properties,
                &obs_inst.properties,
                ids,
            ) && reconciliation_values_map_equal_with_ids(
                &ref_inst.attributes,
                &obs_inst.attributes,
                ids,
            )
        }))
    };

    if need_hydrate.len() > 512 {
        let sample_len = 256usize;
        let step = need_hydrate.len() / sample_len;
        let sample: Vec<usize> = (0..sample_len).map(|k| need_hydrate[k * step]).collect();
        if !check_chunk(&sample)? {
            return Ok(false);
        }
    }

    for chunk in need_hydrate.chunks(COMPACT_HYDRATE_BATCH) {
        if !check_chunk(chunk)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn compact_positions_add_enum_types(
    reference: &CompactDocument,
    hydrate_ref: HydrateRowsFn<'_>,
    hydrate_obs: HydrateRowsFn<'_>,
    kept_camera: Option<(usize, &SettingsBytecodeInstance)>,
) -> Result<bool> {
    let candidates: Vec<usize> = reference
        .meta
        .iter()
        .enumerate()
        .filter_map(|(i, meta)| meta.has_untyped_enum.then_some(i))
        .collect();
    for chunk in candidates.chunks(COMPACT_HYDRATE_BATCH) {
        let ref_rows: AHashMap<usize, SettingsBytecodeInstance> =
            hydrate_ref(chunk)?.into_iter().collect();
        let obs_rows: AHashMap<usize, SettingsBytecodeInstance> =
            hydrate_obs(chunk)?.into_iter().collect();
        let adds = chunk.par_iter().any(|&i| {
            let obs_inst = match kept_camera {
                Some((cam_idx, saved)) if cam_idx == i => saved,
                _ => &obs_rows[&i],
            };
            instance_adds_enum_types(&ref_rows[&i], obs_inst)
        });
        if adds {
            return Ok(true);
        }
    }
    Ok(false)
}

fn compact_final_documents_equivalent(
    reference: &CompactDocument,
    observed: &CompactDocument,
    aligned_ids: &[String],
    obs_remap: &HashMap<String, String>,
    kept_camera: Option<(usize, &SettingsBytecodeInstance)>,
    hydrate_ref: HydrateRowsFn<'_>,
    hydrate_obs: HydrateRowsFn<'_>,
) -> Result<bool> {
    let count = reference.document.instances.len();
    if count != observed.document.instances.len() {
        return Ok(false);
    }
    let mut ref_by_id = AHashMap::with_capacity(count);
    for (i, inst) in reference.document.instances.iter().enumerate() {
        if ref_by_id.insert(inst.settings_id.as_str(), i).is_some() {
            return Ok(false);
        }
    }
    let mut obs_by_id = AHashMap::with_capacity(count);
    for (i, id) in aligned_ids.iter().enumerate() {
        if obs_by_id.insert(id.as_str(), i).is_some() {
            return Ok(false);
        }
    }
    let service = store_service(&reference.document);
    let no_ids = AHashMap::new();
    let ids = PairedStoreIds {
        service,
        left: &no_ids,
        right: &no_ids,
    };

    let mut pairs_to_hydrate: Vec<(usize, usize)> = Vec::new();
    for (ref_idx, ref_inst) in reference.document.instances.iter().enumerate() {
        let Some(&obs_idx) = obs_by_id.get(ref_inst.settings_id.as_str()) else {
            return Ok(false);
        };
        let obs_inst = &observed.document.instances[obs_idx];
        let is_kept_cam = kept_camera.is_some_and(|(idx, _)| idx == obs_idx);
        if ref_inst.name != obs_inst.name || ref_inst.class_name != obs_inst.class_name {
            return Ok(false);
        }
        let ref_parent_id = ref_inst
            .parent_index
            .and_then(|p| reference.document.instances.get(p))
            .map(|p| p.settings_id.as_str());
        let obs_parent_id = obs_inst
            .parent_index
            .and_then(|p| aligned_ids.get(p))
            .map(String::as_str);
        if ref_parent_id != obs_parent_id {
            return Ok(false);
        }
        if is_reconciliation_protected_workspace_camera(&reference.document, ref_idx)
            != is_reconciliation_protected_workspace_camera(&observed.document, obs_idx)
        {
            return Ok(false);
        }
        if !is_kept_cam
            && reference.meta[ref_idx].unique_id_digest != observed.meta[obs_idx].unique_id_digest
        {
            return Ok(false);
        }
        if ref_inst.class_name == "PackageLink" {
            if reference.meta[ref_idx].has_untyped_enum {
                pairs_to_hydrate.push((ref_idx, obs_idx));
            }
            continue;
        }
        if is_kept_cam {
            pairs_to_hydrate.push((ref_idx, obs_idx));
            continue;
        }
        if reference.meta[ref_idx].full_digest == observed.meta[obs_idx].full_digest
            && !reference.meta[ref_idx].has_untyped_enum
            && ((!reference.meta[ref_idx].has_ref && !observed.meta[obs_idx].has_ref)
                || (reconciliation_maps_equal_with_ids(
                    &ref_inst.class_name,
                    &ref_inst.properties,
                    &obs_inst.properties,
                    ids,
                ) && reconciliation_values_map_equal_with_ids(
                    &ref_inst.attributes,
                    &obs_inst.attributes,
                    ids,
                )))
        {
            continue;
        }
        pairs_to_hydrate.push((ref_idx, obs_idx));
    }

    let obs_service = store_service(&observed.document).map(str::to_string);
    for chunk in pairs_to_hydrate.chunks(COMPACT_HYDRATE_BATCH) {
        let ref_indices: Vec<usize> = chunk.iter().map(|&(r, _)| r).collect();
        let obs_indices: Vec<usize> = chunk.iter().map(|&(_, o)| o).collect();
        let (ref_res, obs_res) =
            rayon::join(|| hydrate_ref(&ref_indices), || hydrate_obs(&obs_indices));
        let (ref_idxs, mut ref_plain): (Vec<usize>, Vec<SettingsBytecodeInstance>) =
            ref_res?.into_iter().unzip();
        let (obs_idxs, mut obs_plain): (Vec<usize>, Vec<SettingsBytecodeInstance>) =
            obs_res?.into_iter().unzip();
        if !obs_remap.is_empty() {
            obs_plain.par_iter_mut().for_each(|inst| {
                remap_record_reference_ids(&mut inst.properties, obs_remap, obs_service.as_deref());
                remap_record_reference_ids(&mut inst.attributes, obs_remap, obs_service.as_deref());
            });
        }
        if let Some((cam_idx, saved)) = kept_camera {
            for (idx, inst) in obs_idxs.iter().zip(&mut obs_plain) {
                if *idx == cam_idx {
                    inst.properties.clone_from(&saved.properties);
                    inst.attributes.clone_from(&saved.attributes);
                }
            }
        }
        let (ref_res, obs_res) = rayon::join(
            || {
                ref_plain
                    .par_chunks_mut(1024)
                    .try_for_each(canonicalize_instances)
            },
            || {
                obs_plain
                    .par_chunks_mut(1024)
                    .try_for_each(canonicalize_instances)
            },
        );
        ref_res?;
        obs_res?;
        let ref_lookup: AHashMap<usize, &SettingsBytecodeInstance> = ref_idxs
            .iter()
            .zip(&ref_plain)
            .map(|(&idx, inst)| (idx, inst))
            .collect();
        let obs_lookup: AHashMap<usize, &SettingsBytecodeInstance> = obs_idxs
            .iter()
            .zip(&obs_plain)
            .map(|(&idx, inst)| (idx, inst))
            .collect();
        let batch_ok = chunk.par_iter().all(|&(r, o)| {
            let ref_inst = ref_lookup[&r];
            let obs_inst = obs_lookup[&o];
            if instance_adds_enum_types(ref_inst, obs_inst) {
                return false;
            }
            if ref_inst.class_name == "PackageLink" {
                return true;
            }
            ref_inst.properties.get("UniqueId") == obs_inst.properties.get("UniqueId")
                && reconciliation_maps_equal_with_ids(
                    &ref_inst.class_name,
                    &ref_inst.properties,
                    &obs_inst.properties,
                    ids,
                )
                && reconciliation_values_map_equal_with_ids(
                    &ref_inst.attributes,
                    &obs_inst.attributes,
                    ids,
                )
        });
        if !batch_ok {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) enum StreamingAlignmentPlan {
    Equivalent {
        stored_as_observed: bool,
        answer_follows_camera: bool,
    },
    Changed {
        aligned_ids: Vec<String>,
        kept_camera: Option<(usize, SettingsBytecodeInstance)>,
        stored_as_observed: bool,
        answer_follows_camera: bool,
    },
}

pub(crate) fn plan_captured_store_streaming(
    reference: CompactDocument,
    mut observed: CompactDocument,
    hydrate_ref: HydrateRowsFn<'_>,
    hydrate_obs: HydrateRowsFn<'_>,
    started: std::time::Instant,
) -> Result<StreamingAlignmentPlan> {
    let mut phases = AlignmentPhases {
        instances: observed.document.instances.len(),
        started,
    };
    phases.end("decode");
    let answer_follows_camera = compact_viewport_camera_lacks_enum_types(&reference);
    let viewport_camera = compact_reference_viewport_camera(&reference);

    let topology_matches = settings_topology_matches(&reference.document, &observed.document);
    let mut positionally_equivalent = topology_matches
        && compact_positional_values_equivalent(
            &reference,
            &observed,
            hydrate_ref,
            hydrate_obs,
            None,
            false,
            None,
        )?;
    let early_camera = (topology_matches && !positionally_equivalent)
        .then(|| {
            compact_kept_camera(
                viewport_camera.as_ref(),
                &observed.document,
                &observed.cameras,
                None,
            )
        })
        .flatten();
    if let Some(kept_cam) = early_camera {
        positionally_equivalent = compact_positional_values_equivalent(
            &reference,
            &observed,
            hydrate_ref,
            hydrate_obs,
            None,
            false,
            Some(kept_cam),
        )?;
    }
    phases.end("comparison by position");

    if positionally_equivalent
        && !compact_positions_add_enum_types(&reference, hydrate_ref, hydrate_obs, early_camera)?
    {
        return Ok(StreamingAlignmentPlan::Equivalent {
            stored_as_observed: compact_same_instances(&reference, &observed, early_camera),
            answer_follows_camera,
        });
    }

    let already_aligned = topology_matches && {
        let mut seen = AHashSet::with_capacity(observed.document.instances.len());
        let mut all_store_ids = true;
        let ids_match = reference.document.instances.len() == observed.document.instances.len()
            && observed
                .document
                .instances
                .iter()
                .enumerate()
                .all(|(i, obs)| {
                    let ref_inst = &reference.document.instances[i];
                    all_store_ids &= is_store_identity(&ref_inst.settings_id);
                    ref_inst.settings_id == obs.settings_id
                        && seen.insert(ref_inst.settings_id.as_str())
                })
            && workspace_current_camera_index(&reference.document)
                == workspace_current_camera_index(&observed.document);
        if !ids_match {
            false
        } else if all_store_ids {
            compact_persistent_identities_follow_mapping(
                &reference.document,
                &reference.unique_ids,
                &observed.document,
                &observed.unique_ids,
                Some,
            )
        } else {
            let ref_uids = persistent_identity_index_from_slice(&reference.unique_ids);
            let obs_uids = persistent_identity_index_from_slice(&observed.unique_ids);
            let camera = workspace_current_camera_index(&observed.document);
            let sole_root = reference
                .document
                .instances
                .iter()
                .filter(|inst| inst.parent_index.is_none())
                .count()
                == 1;
            let exact_ids = observed
                .document
                .instances
                .par_iter()
                .enumerate()
                .all(|(i, inst)| {
                    if camera == Some(i) {
                        return true;
                    }
                    let persistent = observed.unique_ids[i]
                        .as_deref()
                        .filter(|id| obs_uids.get(id) == Some(&Some(i)))
                        .and_then(|id| ref_uids.get(id).copied().flatten())
                        .filter(|c| reference.document.instances[*c].class_name == inst.class_name);
                    persistent.map_or_else(
                        || {
                            !inst.settings_id.starts_with("debug:")
                                || (sole_root && inst.parent_index.is_none())
                        },
                        |candidate| candidate == i,
                    )
                });
            if exact_ids {
                true
            } else if compact_persistent_identities_follow_mapping(
                &reference.document,
                &reference.unique_ids,
                &observed.document,
                &observed.unique_ids,
                Some,
            ) && store_identities_follow_mapping(
                &reference.document,
                &observed.document,
                Some,
            ) {
                let mut first_by_key = AHashMap::with_capacity(reference.document.instances.len());
                let mut ambiguous = vec![false; reference.document.instances.len()];
                for (i, inst) in reference.document.instances.iter().enumerate() {
                    let key = (
                        inst.parent_index,
                        inst.name.as_str(),
                        inst.class_name.as_str(),
                    );
                    if let Some(first) = first_by_key.insert(key, i) {
                        ambiguous[first] = true;
                        ambiguous[i] = true;
                    }
                }
                if !ambiguous.iter().any(|v| *v) {
                    true
                } else {
                    let mut ordered = true;
                    for (i, inst) in reference.document.instances.iter().enumerate() {
                        if let Some(parent) = inst.parent_index {
                            if parent >= i {
                                ordered = false;
                                break;
                            }
                            ambiguous[i] |= ambiguous[parent];
                        }
                    }
                    ordered
                        && compact_positional_values_equivalent(
                            &reference,
                            &observed,
                            hydrate_ref,
                            hydrate_obs,
                            Some(&ambiguous),
                            true,
                            None,
                        )?
                }
            } else {
                false
            }
        }
    };
    phases.end("identity check");

    let (aligned_ids, obs_remap, stored_as_observed_on_change, same_before_canon) =
        if already_aligned {
            let kept_cam = compact_kept_camera(
                viewport_camera.as_ref(),
                &observed.document,
                &observed.cameras,
                None,
            );
            let same = compact_same_instances(&reference, &observed, kept_cam);
            let ids: Vec<String> = observed
                .document
                .instances
                .iter()
                .map(|inst| inst.settings_id.clone())
                .collect();
            (ids, HashMap::new(), true, same)
        } else {
            let compact = CompactMatching {
                ref_unique_ids: &reference.unique_ids,
                obs_unique_ids: &observed.unique_ids,
                ref_meta: &reference.meta,
                obs_meta: &observed.meta,
                hydrate_ref,
                hydrate_obs,
            };
            let Some((ids, ..)) = match_settings_ids_with_compact(
                &reference.document,
                &observed.document,
                false,
                false,
                Some(&compact),
            )?
            else {
                bail!("duplicate instance identity is ambiguous after comparing references");
            };
            let mut remap = HashMap::new();
            let has_any_obs_ref = observed.meta.iter().any(|m| m.has_ref);
            if has_any_obs_ref {
                remap.reserve(observed.document.instances.len());
                for (inst, new_id) in observed.document.instances.iter().zip(&ids) {
                    if inst.settings_id != *new_id {
                        remap.insert(inst.settings_id.clone(), new_id.clone());
                    }
                }
            }
            take_settings_ids(&mut observed.document, ids.clone());
            phases.end("identity matching");
            (ids, remap, false, false)
        };

    let kept_cam = compact_kept_camera(
        viewport_camera.as_ref(),
        &observed.document,
        &observed.cameras,
        workspace_current_camera_index(&observed.document).map(|i| aligned_ids[i].as_str()),
    );
    let equivalent = compact_final_documents_equivalent(
        &reference,
        &observed,
        &aligned_ids,
        &obs_remap,
        kept_cam,
        hydrate_ref,
        hydrate_obs,
    )?;
    phases.end("final comparison");

    if equivalent {
        return Ok(StreamingAlignmentPlan::Equivalent {
            stored_as_observed: stored_as_observed_on_change && same_before_canon,
            answer_follows_camera,
        });
    }

    Ok(StreamingAlignmentPlan::Changed {
        aligned_ids,
        kept_camera: kept_cam.map(|(idx, saved)| (idx, saved.clone())),
        stored_as_observed: stored_as_observed_on_change,
        answer_follows_camera,
    })
}

/// Whether a pull holds the values of the observed instance for the stored
/// one's, for two instances that stand at one position under one id, name,
/// class and parent: first as the two hold them, which is how the alignment
/// compares instances by their places, then under the names their
/// properties go by today, which is how it compares what it aligned. Their
/// UniqueIds are the caller's to compare. `None` where an enum item gains
/// its type, which takes the whole documents to say.
/// Cached reflection default value for one-sided (default-elided) property comparison.
#[derive(Clone)]
pub(crate) enum FastDefaultVal {
    Bool(bool),
    Number { value: f64, uses_f32: bool },
    Str(String),
    Enum(String),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    UDim([f32; 2]),
    UDim2([f32; 4]),
    Color3([f32; 3]),
    Rect([f32; 4]),
    CFrame([f64; 12]),
    Other,
}

/// How a property present on only one side (stored or captured) affects reconciliation.
#[derive(Clone)]
pub(crate) enum OneSidedRule {
    /// Engine state or unknown when absent: one-sided presence is ignored.
    Ignored,
    /// Property has a reflection default value (or `BaseScript.Enabled = true`).
    Default(FastDefaultVal),
    /// Property is compared when one-sided and has no reflection default.
    NoDefault,
}

/// How `canonicalize_instances` affects a property on a given class.
#[derive(Clone, Copy)]
pub(crate) enum PropertyCanonical<'db> {
    /// Property is not renamed, migrated, or content-URI-aliased.
    Unchanged,
    /// Property is renamed to a single canonical name (which itself is not migrated).
    Renamed(&'db str),
    /// Property is migrated to one or more canonical replacement names (or `BaseScript.Disabled` -> `Enabled`).
    Migrated(&'db [&'db str]),
    /// Chained rename+migration or missing reflection DB: fall back.
    Complex,
}

struct ClassCacheEntry<'db> {
    is_base_script: bool,
    modern_uri: Option<(&'static str, &'static str)>,
    canonical: AHashMap<String, PropertyCanonical<'db>>,
    one_sided: AHashMap<String, OneSidedRule>,
    verify_one_sided: AHashMap<String, OneSidedRule>,
    uses_f32: AHashMap<String, bool>,
    color3uint8: AHashMap<String, bool>,
}

/// Per-worker cache for property canonicalization, default-elision, and f32 tolerance checks.
/// Avoids cloning instances and repeating reflection database lookups across thousands of rows.
pub(crate) struct CanonicalizeCache<'db> {
    database: Option<&'db rbx_reflection::ReflectionDatabase<'db>>,
    classes: AHashMap<String, ClassCacheEntry<'db>>,
}

impl<'db> CanonicalizeCache<'db> {
    pub(crate) fn new() -> Self {
        Self {
            database: rbx_reflection_database::get().ok(),
            classes: AHashMap::new(),
        }
    }

    fn class_entry(&mut self, class_name: &str) -> Option<&mut ClassCacheEntry<'db>> {
        let database = self.database?;
        if !self.classes.contains_key(class_name) {
            let is_base_script =
                crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BaseScript");
            let modern_uri = match class_name {
                class if crate::roblox::schema::has_protected_texture_pack(class) => {
                    Some(("TexturePackContent", "TexturePack"))
                }
                "PartOperation" | "UnionOperation" | "NegateOperation" | "IntersectOperation" => {
                    Some(("Content", "AssetId"))
                }
                "Shirt" => Some(("ShirtTemplateContent", "ShirtTemplate")),
                "Pants" => Some(("PantsTemplateContent", "PantsTemplate")),
                _ => None,
            };
            self.classes.insert(
                class_name.to_string(),
                ClassCacheEntry {
                    is_base_script,
                    modern_uri,
                    canonical: AHashMap::new(),
                    one_sided: AHashMap::new(),
                    verify_one_sided: AHashMap::new(),
                    uses_f32: AHashMap::new(),
                    color3uint8: AHashMap::new(),
                },
            );
        }
        self.classes.get_mut(class_name)
    }

    pub(crate) fn property_canonical(
        &mut self,
        class_name: &str,
        name: &str,
    ) -> PropertyCanonical<'db> {
        let Some(database) = self.database else {
            return PropertyCanonical::Complex;
        };
        let entry = self.class_entry(class_name).unwrap();
        if let Some((modern, legacy)) = entry.modern_uri
            && name == modern
        {
            return PropertyCanonical::Renamed(legacy);
        }
        if entry.is_base_script && name == "Disabled" {
            return PropertyCanonical::Migrated(&["Enabled"]);
        }
        if let Some(&cached) = entry.canonical.get(name) {
            return cached;
        }
        let info = if let Some(canonical) = serialized_only_logical_name(database, class_name, name)
            .or_else(|| rbx_logical_property_name(database, class_name, name))
            .filter(|canonical| *canonical != name)
        {
            if legacy_property_migration(database, class_name, canonical).is_some() {
                PropertyCanonical::Complex
            } else {
                PropertyCanonical::Renamed(canonical)
            }
        } else if let Some((_, migration)) = legacy_property_migration(database, class_name, name) {
            PropertyCanonical::Migrated(migration.new_property_names())
        } else {
            PropertyCanonical::Unchanged
        };
        entry.canonical.insert(name.to_string(), info);
        info
    }

    pub(crate) fn property_uses_f32(&mut self, class_name: &str, name: &str) -> bool {
        let Some(database) = self.database else {
            return false;
        };
        let entry = self.class_entry(class_name).unwrap();
        if let Some(&cached) = entry.uses_f32.get(name) {
            return cached;
        }
        let is_f32 =
            rbx_model_property_descriptor(database, class_name, name).is_some_and(|descriptor| {
                matches!(
                    descriptor.data_type,
                    RbxDataType::Value(RbxVariantType::Float32)
                )
            });
        entry.uses_f32.insert(name.to_string(), is_f32);
        is_f32
    }

    pub(crate) fn property_is_color3uint8(&mut self, class_name: &str, name: &str) -> bool {
        let Some(database) = self.database else {
            return false;
        };
        let entry = self.class_entry(class_name).unwrap();
        if let Some(&cached) = entry.color3uint8.get(name) {
            return cached;
        }
        let is_byte_color =
            rbx_model_property_descriptor(database, class_name, name).is_some_and(|descriptor| {
                matches!(
                    descriptor.data_type,
                    RbxDataType::Value(RbxVariantType::Color3uint8)
                )
            });
        entry.color3uint8.insert(name.to_string(), is_byte_color);
        is_byte_color
    }

    /// Stricter one-sided rule for push verification (`expected_map_mismatch` /
    /// `changed_map_mismatch`): only marks a property `Ignored` when it is metadata,
    /// always-engine-state, `capture_may_omit`, or `studio_recomputed_value_is_absent_elsewhere`
    /// (never `reconciliation_property_is_unknown_when_absent`).
    pub(crate) fn verify_one_sided_rule(&mut self, class_name: &str, name: &str) -> OneSidedRule {
        let Some(database) = self.database else {
            return OneSidedRule::Default(FastDefaultVal::Other);
        };
        let is_f32 = self.property_uses_f32(class_name, name);
        let entry = self.class_entry(class_name).unwrap();
        if let Some(cached) = entry.verify_one_sided.get(name) {
            return cached.clone();
        }
        let rule = if matches!(name, "UniqueId" | "CurrentCamera" | "HistoryId")
            || reconciliation_property_is_always_engine_state(Some(database), class_name, name)
            || capture_may_omit(class_name, name)
            || studio_recomputed_value_is_absent_elsewhere(class_name, name)
        {
            OneSidedRule::Ignored
        } else if entry.is_base_script && name == "Enabled" {
            OneSidedRule::Default(FastDefaultVal::Bool(true))
        } else {
            match reflection_default_settings_value(database, class_name, name) {
                None => OneSidedRule::NoDefault,
                Some(default) => {
                    let parsed = match &default {
                        Value::Bool(b) => FastDefaultVal::Bool(*b),
                        Value::Number(n) => match n.as_f64() {
                            Some(v) if v.is_finite() => FastDefaultVal::Number {
                                value: v,
                                uses_f32: is_f32,
                            },
                            _ => FastDefaultVal::Other,
                        },
                        Value::String(s) => FastDefaultVal::Str(s.clone()),
                        Value::Object(obj) => {
                            let f = |k: &str| obj.get(k).and_then(Value::as_f64).map(|v| v as f32);
                            match obj.get("_type").and_then(Value::as_str) {
                                Some("EnumItem") => obj
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .map_or(FastDefaultVal::Other, |s| {
                                        FastDefaultVal::Enum(s.to_string())
                                    }),
                                Some("Vector2") => match (f("x"), f("y")) {
                                    (Some(x), Some(y)) => FastDefaultVal::Vec2([x, y]),
                                    _ => FastDefaultVal::Other,
                                },
                                Some("Vector3") => match (f("x"), f("y"), f("z")) {
                                    (Some(x), Some(y), Some(z)) => FastDefaultVal::Vec3([x, y, z]),
                                    _ => FastDefaultVal::Other,
                                },
                                Some("UDim") => match (f("scale"), f("offset")) {
                                    (Some(s), Some(o)) => FastDefaultVal::UDim([s, o]),
                                    _ => FastDefaultVal::Other,
                                },
                                Some("UDim2") => {
                                    match (f("xScale"), f("xOffset"), f("yScale"), f("yOffset")) {
                                        (Some(xs), Some(xo), Some(ys), Some(yo)) => {
                                            FastDefaultVal::UDim2([xs, xo, ys, yo])
                                        }
                                        _ => FastDefaultVal::Other,
                                    }
                                }
                                Some("Color3") => match (f("r"), f("g"), f("b")) {
                                    (Some(r), Some(g), Some(b)) => {
                                        FastDefaultVal::Color3([r, g, b])
                                    }
                                    _ => FastDefaultVal::Other,
                                },
                                Some("Rect") => {
                                    match (f("minX"), f("minY"), f("maxX"), f("maxY")) {
                                        (Some(a), Some(b), Some(c), Some(d)) => {
                                            FastDefaultVal::Rect([a, b, c, d])
                                        }
                                        _ => FastDefaultVal::Other,
                                    }
                                }
                                Some("CFrame") => obj
                                    .get("components")
                                    .and_then(Value::as_array)
                                    .filter(|c| c.len() == 12)
                                    .and_then(|c| {
                                        let mut out = [0.0_f64; 12];
                                        for (i, v) in c.iter().enumerate() {
                                            out[i] = v.as_f64()?;
                                        }
                                        Some(FastDefaultVal::CFrame(out))
                                    })
                                    .unwrap_or(FastDefaultVal::Other),
                                _ => FastDefaultVal::Other,
                            }
                        }
                        _ => FastDefaultVal::Other,
                    };
                    OneSidedRule::Default(parsed)
                }
            }
        };
        entry
            .verify_one_sided
            .insert(name.to_string(), rule.clone());
        rule
    }

    pub(crate) fn one_sided_rule(&mut self, class_name: &str, name: &str) -> OneSidedRule {
        let Some(database) = self.database else {
            return OneSidedRule::Default(FastDefaultVal::Other);
        };
        let is_f32 = self.property_uses_f32(class_name, name);
        let entry = self.class_entry(class_name).unwrap();
        if let Some(cached) = entry.one_sided.get(name) {
            return cached.clone();
        }
        let rule = if matches!(name, "UniqueId" | "CurrentCamera" | "HistoryId")
            || reconciliation_property_is_always_engine_state(Some(database), class_name, name)
            || reconciliation_property_is_unknown_when_absent(Some(database), class_name, name)
        {
            OneSidedRule::Ignored
        } else if entry.is_base_script && name == "Enabled" {
            OneSidedRule::Default(FastDefaultVal::Bool(true))
        } else {
            match reflection_default_settings_value(database, class_name, name) {
                None => OneSidedRule::NoDefault,
                Some(default) => {
                    let parsed = match &default {
                        Value::Bool(b) => FastDefaultVal::Bool(*b),
                        Value::Number(n) => match n.as_f64() {
                            Some(v) if v.is_finite() => FastDefaultVal::Number {
                                value: v,
                                uses_f32: is_f32,
                            },
                            _ => FastDefaultVal::Other,
                        },
                        Value::String(s) => FastDefaultVal::Str(s.clone()),
                        Value::Object(obj) => {
                            let f = |k: &str| obj.get(k).and_then(Value::as_f64).map(|v| v as f32);
                            match obj.get("_type").and_then(Value::as_str) {
                                Some("EnumItem") => obj
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .map_or(FastDefaultVal::Other, |s| {
                                        FastDefaultVal::Enum(s.to_string())
                                    }),
                                Some("Vector2") => match (f("x"), f("y")) {
                                    (Some(x), Some(y)) => FastDefaultVal::Vec2([x, y]),
                                    _ => FastDefaultVal::Other,
                                },
                                Some("Vector3") => match (f("x"), f("y"), f("z")) {
                                    (Some(x), Some(y), Some(z)) => FastDefaultVal::Vec3([x, y, z]),
                                    _ => FastDefaultVal::Other,
                                },
                                Some("UDim") => match (f("scale"), f("offset")) {
                                    (Some(s), Some(o)) => FastDefaultVal::UDim([s, o]),
                                    _ => FastDefaultVal::Other,
                                },
                                Some("UDim2") => {
                                    match (f("xScale"), f("xOffset"), f("yScale"), f("yOffset")) {
                                        (Some(xs), Some(xo), Some(ys), Some(yo)) => {
                                            FastDefaultVal::UDim2([xs, xo, ys, yo])
                                        }
                                        _ => FastDefaultVal::Other,
                                    }
                                }
                                Some("Color3") => match (f("r"), f("g"), f("b")) {
                                    (Some(r), Some(g), Some(b)) => {
                                        FastDefaultVal::Color3([r, g, b])
                                    }
                                    _ => FastDefaultVal::Other,
                                },
                                Some("Rect") => {
                                    match (f("minX"), f("minY"), f("maxX"), f("maxY")) {
                                        (Some(a), Some(b), Some(c), Some(d)) => {
                                            FastDefaultVal::Rect([a, b, c, d])
                                        }
                                        _ => FastDefaultVal::Other,
                                    }
                                }
                                Some("CFrame") => obj
                                    .get("components")
                                    .and_then(Value::as_array)
                                    .filter(|a| a.len() == 12)
                                    .and_then(|a| {
                                        let mut c = [0.0_f64; 12];
                                        for (dst, v) in c.iter_mut().zip(a) {
                                            *dst = v.as_f64().filter(|x| x.is_finite())?;
                                        }
                                        Some(FastDefaultVal::CFrame(c))
                                    })
                                    .unwrap_or(FastDefaultVal::Other),
                                _ => FastDefaultVal::Other,
                            }
                        }
                        _ => FastDefaultVal::Other,
                    };
                    OneSidedRule::Default(parsed)
                }
            }
        };
        entry.one_sided.insert(name.to_string(), rule.clone());
        rule
    }

    fn needs_canonicalize(&mut self, instance: &SettingsBytecodeInstance) -> bool {
        let Some(database) = self.database else {
            return true;
        };
        let entry = self.class_entry(&instance.class_name).unwrap();
        if let Some((modern, _)) = entry.modern_uri
            && instance
                .properties
                .get(modern)
                .map(typed_scalar)
                .is_some_and(Value::is_string)
        {
            return true;
        }
        if entry.is_base_script && instance.properties.contains_key("Disabled") {
            return true;
        }
        for name in instance.properties.keys() {
            let info = if let Some(&cached) = entry.canonical.get(name.as_str()) {
                cached
            } else {
                let computed = if let Some(canonical) =
                    serialized_only_logical_name(database, &instance.class_name, name)
                        .or_else(|| rbx_logical_property_name(database, &instance.class_name, name))
                        .filter(|canonical| *canonical != name)
                {
                    if legacy_property_migration(database, &instance.class_name, canonical)
                        .is_some()
                    {
                        PropertyCanonical::Complex
                    } else {
                        PropertyCanonical::Renamed(canonical)
                    }
                } else if let Some((_, migration)) =
                    legacy_property_migration(database, &instance.class_name, name)
                {
                    PropertyCanonical::Migrated(migration.new_property_names())
                } else {
                    PropertyCanonical::Unchanged
                };
                entry.canonical.insert(name.clone(), computed);
                computed
            };
            if !matches!(info, PropertyCanonical::Unchanged) {
                return true;
            }
        }
        false
    }
}

pub(crate) fn pulled_instance_values_unchanged(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
    service: Option<&str>,
) -> Result<Option<(bool, bool)>> {
    let mut cache = CanonicalizeCache::new();
    pulled_instance_values_unchanged_with_cache(reference, observed, service, &mut cache)
}

pub(crate) fn pulled_instance_values_unchanged_with_cache(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
    service: Option<&str>,
    cache: &mut CanonicalizeCache<'_>,
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
    if !cache.needs_canonicalize(reference) && !cache.needs_canonicalize(observed) {
        return Ok(Some((as_held, as_held)));
    }
    let mut named = SettingsBytecode {
        version: super::bytecode::SETTINGS_BINARY_VERSION,
        instances: vec![reference.clone(), observed.clone()],
    };
    canonicalize_instances(&mut named.instances)?;
    let as_named = if named.instances[0] == *reference && named.instances[1] == *observed {
        as_held
    } else {
        unchanged(&named.instances[0], &named.instances[1])
    };
    Ok(Some((as_held, as_named)))
}

pub(super) trait DigestSink {
    fn update_bytes(&mut self, bytes: &[u8]);

    #[inline]
    fn update_text(&mut self, text: &str) {
        self.update_bytes(&(text.len() as u64).to_le_bytes());
        self.update_bytes(text.as_bytes());
    }
}

impl DigestSink for Sha256 {
    #[inline]
    fn update_bytes(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

#[inline]
pub(super) fn digest_text<D: DigestSink>(digest: &mut D, text: &str) {
    digest.update_text(text);
}

#[inline]
pub(super) fn digest_map<D: DigestSink>(digest: &mut D, map: &Map<String, Value>) {
    digest.update_bytes(&(map.len() as u64).to_le_bytes());
    for (name, value) in map {
        digest_text(digest, name);
        digest_value(digest, value);
    }
}

pub(super) fn digest_value<D: DigestSink>(digest: &mut D, value: &Value) {
    match &*crate::settings::held::resolve(value) {
        Value::Null => digest.update_bytes(&[0]),
        Value::Bool(value) => digest.update_bytes(&[1, u8::from(*value)]),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                digest.update_bytes(&[2]);
                digest.update_bytes(&value.to_le_bytes());
            } else if let Some(value) = number.as_i64() {
                digest.update_bytes(&[3]);
                digest.update_bytes(&value.to_le_bytes());
            } else {
                digest.update_bytes(&[4]);
                digest.update_bytes(&number.as_f64().map_or(u64::MAX, f64::to_bits).to_le_bytes());
            }
        }
        Value::String(text) => {
            digest.update_bytes(&[5]);
            digest_text(digest, text);
        }
        Value::Array(values) => {
            digest.update_bytes(&[6]);
            digest.update_bytes(&(values.len() as u64).to_le_bytes());
            for value in values {
                digest_value(digest, value);
            }
        }
        Value::Object(map) => {
            digest.update_bytes(&[7]);
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
    EncodeDocument,
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
        if let Some(key) = cache_key.filter(|_| !camera_kept_early) {
            cache_settings_alignment(key);
        }
        if let Some(stored_as_observed) = stored_as_observed {
            stored_as_observed.store(same_instances(&reference, &observed), Ordering::Relaxed);
        }
        drop_settings_documents(reference, observed);
        return Ok(SettingsAlignment::Equivalent);
    }
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
                ObservedBytes::EncodeDocument => {
                    encode_settings_bytecode_over(&observed, false, previous)?
                }
            }
        };
        phases.end("encode in place");
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
    canonicalize_settings_property_names(&mut reference)?;
    let restored = canonicalize_settings_property_names_recording(&mut observed)?;
    let equivalent = settings_documents_match(&reference, &observed, false)
        && !document_adds_enum_types(&reference, &observed);
    drop_settings_document(reference);
    phases.end("final comparison");
    if equivalent {
        if let Some(key) = cache_key.filter(|_| !camera_kept) {
            cache_settings_alignment(key);
        }
        drop_settings_document(observed);
        return Ok(SettingsAlignment::Equivalent);
    }
    for (index, original) in restored {
        observed.instances[index].properties = original;
    }
    let aligned = encode_settings_bytecode_over(&observed, false, previous)?;
    phases.end("encode");
    drop_settings_document(observed);
    Ok(SettingsAlignment::Changed(aligned))
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
    let count = instances.len();
    if count >= 8_192 && rayon::current_num_threads() > 1 {
        instances.into_par_iter().for_each(drop);
        if count >= 32_768 {
            crate::daemon::memory::release_between_phases();
        }
    } else {
        drop(instances);
    }
}

pub(crate) fn drop_settings_documents(left: SettingsBytecode, right: SettingsBytecode) {
    let total = left.instances.len().saturating_add(right.instances.len());
    if total >= 16_384 && rayon::current_num_threads() > 1 {
        let SettingsBytecode {
            instances: left, ..
        } = left;
        let SettingsBytecode {
            instances: right, ..
        } = right;
        left.into_par_iter().chain(right).for_each(drop);
        if total >= 65_536 {
            crate::daemon::memory::release_between_phases();
        }
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
    slot: std::sync::Arc<str>,
    target: ReferenceTarget,
}

struct IncomingReference {
    slot: std::sync::Arc<str>,
    source: usize,
}

struct ReferenceGraph {
    outgoing_offsets: Vec<u32>,
    outgoing_edges: Vec<ReferenceEdge>,
    incoming_offsets: Vec<u32>,
    incoming_edges: Vec<IncomingReference>,
}

impl ReferenceGraph {
    #[inline]
    fn outgoing(&self, index: usize) -> &[ReferenceEdge] {
        let start = self.outgoing_offsets[index] as usize;
        let end = self.outgoing_offsets[index + 1] as usize;
        &self.outgoing_edges[start..end]
    }

    #[inline]
    fn incoming(&self, index: usize) -> &[IncomingReference] {
        let start = self.incoming_offsets[index] as usize;
        let end = self.incoming_offsets[index + 1] as usize;
        &self.incoming_edges[start..end]
    }

    #[inline]
    fn has_outgoing(&self, index: usize) -> bool {
        self.outgoing_offsets[index] != self.outgoing_offsets[index + 1]
    }

    #[inline]
    fn has_edges(&self, index: usize) -> bool {
        self.outgoing_offsets[index] != self.outgoing_offsets[index + 1]
            || self.incoming_offsets[index] != self.incoming_offsets[index + 1]
    }
}

struct RawReferenceEdge<'a> {
    source: u32,
    slot: std::sync::Arc<str>,
    target_id: Option<&'a str>,
    value: &'a Value,
}

fn build_reference_graph(document: &SettingsBytecode) -> ReferenceGraph {
    fn collect_chunk<'a>(
        chunk_start: usize,
        instances: &'a [SettingsBytecodeInstance],
    ) -> Vec<RawReferenceEdge<'a>> {
        let mut slot = String::new();
        let mut raw: Vec<RawReferenceEdge<'a>> = Vec::new();
        for (offset, instance) in instances.iter().enumerate() {
            let source = (chunk_start + offset) as u32;
            slot.clear();
            slot.push_str("properties");
            collect_object_raw_references(&instance.properties, &mut slot, source, &mut raw);
            if !instance.attributes.is_empty() {
                slot.clear();
                slot.push_str("attributes");
                collect_object_raw_references(&instance.attributes, &mut slot, source, &mut raw);
            }
        }
        raw
    }
    let count = document.instances.len();
    let chunk_raw: Vec<Vec<RawReferenceEdge<'_>>> =
        if count >= 2_048 && rayon::current_num_threads() > 1 {
            document
                .instances
                .par_chunks(512)
                .enumerate()
                .map(|(chunk_idx, chunk)| collect_chunk(chunk_idx * 512, chunk))
                .collect()
        } else {
            vec![collect_chunk(0, &document.instances)]
        };
    let total_outgoing: usize = chunk_raw.iter().map(Vec::len).sum();
    if total_outgoing == 0 {
        return ReferenceGraph {
            outgoing_offsets: vec![0u32; count + 1],
            outgoing_edges: Vec::new(),
            incoming_offsets: vec![0u32; count + 1],
            incoming_edges: Vec::new(),
        };
    }
    let needs_by_id = chunk_raw
        .iter()
        .any(|chunk| chunk.iter().any(|edge| edge.target_id.is_some()));
    let by_id = if needs_by_id {
        let mut map = AHashMap::with_capacity(count);
        for (index, instance) in document.instances.iter().enumerate() {
            map.insert(instance.settings_id.as_str(), index);
        }
        map
    } else {
        AHashMap::new()
    };
    let mut outgoing_offsets = vec![0u32; count + 1];
    let mut outgoing_edges = Vec::with_capacity(total_outgoing);
    let mut incoming_counts = vec![0u32; count];
    let mut total_incoming = 0usize;
    let mut prev_source = 0usize;
    for chunk in chunk_raw {
        for raw in chunk {
            let source_idx = raw.source as usize;
            while prev_source < source_idx {
                prev_source += 1;
                outgoing_offsets[prev_source] = outgoing_edges.len() as u32;
            }
            let target = raw
                .target_id
                .and_then(|id| by_id.get(id).copied())
                .map(ReferenceTarget::Internal)
                .unwrap_or_else(|| ReferenceTarget::External(raw.value.clone()));
            if let ReferenceTarget::Internal(target_idx) = target {
                incoming_counts[target_idx] += 1;
                total_incoming += 1;
            }
            outgoing_edges.push(ReferenceEdge {
                slot: raw.slot,
                target,
            });
        }
    }
    drop(by_id);
    while prev_source < count {
        prev_source += 1;
        outgoing_offsets[prev_source] = outgoing_edges.len() as u32;
    }
    let mut incoming_offsets = Vec::with_capacity(count + 1);
    let mut running = 0u32;
    for &c in &incoming_counts {
        incoming_offsets.push(running);
        running += c;
    }
    incoming_offsets.push(running);
    let incoming_edges = if total_incoming == 0 {
        Vec::new()
    } else {
        let mut incoming_slots: Vec<Option<IncomingReference>> =
            (0..total_incoming).map(|_| None).collect();
        incoming_counts.copy_from_slice(&incoming_offsets[..count]);
        for source in 0..count {
            let start = outgoing_offsets[source] as usize;
            let end = outgoing_offsets[source + 1] as usize;
            for edge in &outgoing_edges[start..end] {
                if let ReferenceTarget::Internal(target) = edge.target {
                    let pos = incoming_counts[target] as usize;
                    incoming_counts[target] += 1;
                    incoming_slots[pos] = Some(IncomingReference {
                        slot: std::sync::Arc::clone(&edge.slot),
                        source,
                    });
                }
            }
        }
        incoming_slots.into_iter().map(Option::unwrap).collect()
    };
    ReferenceGraph {
        outgoing_offsets,
        outgoing_edges,
        incoming_offsets,
        incoming_edges,
    }
}

fn collect_map_references(
    values: &Map<String, Value>,
    prefix: &str,
    by_id: &AHashMap<&str, usize>,
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
    by_id: &AHashMap<&str, usize>,
    output: &mut Vec<ReferenceEdge>,
) {
    for (name, value) in values {
        if !matches!(value, Value::Array(_) | Value::Object(_))
            || !reconciliation_value_contains_reference(value)
        {
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
    by_id: &AHashMap<&str, usize>,
    output: &mut Vec<ReferenceEdge>,
) {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if !matches!(value, Value::Array(_) | Value::Object(_))
                    || !reconciliation_value_contains_reference(value)
                {
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
                slot: std::sync::Arc::from(slot.as_str()),
                target,
            });
        }
        Value::Object(object) => collect_object_references(object, slot, by_id, output),
        _ => {}
    }
}

fn collect_object_raw_references<'a>(
    values: &'a Map<String, Value>,
    slot: &mut String,
    source: u32,
    output: &mut Vec<RawReferenceEdge<'a>>,
) {
    for (name, value) in values {
        if !matches!(value, Value::Array(_) | Value::Object(_)) {
            continue;
        }
        if let Value::Object(object) = value
            && is_reference_object(object)
        {
            let original_len = slot.len();
            write!(slot, "/{}:{name}", name.len()).expect("writing to a String cannot fail");
            let target_id = object
                .get("settingsId")
                .or_else(|| object.get("instanceId"))
                .and_then(Value::as_str);
            output.push(RawReferenceEdge {
                source,
                slot: std::sync::Arc::from(slot.as_str()),
                target_id,
                value,
            });
            slot.truncate(original_len);
            continue;
        }
        if !reconciliation_value_contains_reference(value) {
            continue;
        }
        let original_len = slot.len();
        write!(slot, "/{}:{name}", name.len()).expect("writing to a String cannot fail");
        collect_value_raw_references(value, slot, source, output);
        slot.truncate(original_len);
    }
}

fn collect_value_raw_references<'a>(
    value: &'a Value,
    slot: &mut String,
    source: u32,
    output: &mut Vec<RawReferenceEdge<'a>>,
) {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if !matches!(value, Value::Array(_) | Value::Object(_))
                    || !reconciliation_value_contains_reference(value)
                {
                    continue;
                }
                let original_len = slot.len();
                write!(slot, "[{index}]").expect("writing to a String cannot fail");
                collect_value_raw_references(value, slot, source, output);
                slot.truncate(original_len);
            }
        }
        Value::Object(object) if is_reference_object(object) => {
            let target_id = object
                .get("settingsId")
                .or_else(|| object.get("instanceId"))
                .and_then(Value::as_str);
            output.push(RawReferenceEdge {
                source,
                slot: std::sync::Arc::from(slot.as_str()),
                target_id,
                value,
            });
        }
        Value::Object(object) => {
            collect_object_raw_references(object, slot, source, output);
        }
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
    let reference_outgoing = reference_graph.outgoing(reference_index);
    let observed_outgoing = observed_graph.outgoing(observed_index);
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

    let observed_incoming = observed_graph.incoming(observed_index);
    let reference_incoming = reference_graph.incoming(reference_index);
    for observed_edge in observed_incoming {
        let expected_source = if observed_edge.source == observed_index {
            Some(reference_index)
        } else {
            assigned_reference[observed_edge.source]
        };
        if let Some(expected_source) = expected_source {
            if !reference_incoming
                .iter()
                .any(|edge| edge.source == expected_source && edge.slot == observed_edge.slot)
            {
                return None;
            }
            evidence += 1;
        }
    }
    for reference_edge in reference_incoming {
        let expected_source = if reference_edge.source == reference_index {
            Some(observed_index)
        } else {
            assigned_observed[reference_edge.source]
        };
        if let Some(expected_source) = expected_source
            && !observed_incoming
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

#[cfg(test)]
#[derive(Hash, PartialEq, Eq)]
struct IdentitySubtreeKey<'a> {
    name: &'a str,
    class_name: &'a str,
    properties: Vec<(&'a str, &'a Value)>,
    attributes: Vec<(&'a str, &'a Value)>,
    children: Vec<usize>,
}

#[inline]
fn fixed_identity_hasher() -> ahash::AHasher {
    use std::hash::BuildHasher as _;
    ahash::RandomState::with_seeds(
        0x243f_6a88_85a3_08d3,
        0x1319_8a2e_0370_7344,
        0xa409_3822_299f_31d0,
        0x082e_fa98_ec4e_6c89,
    )
    .build_hasher()
}

#[inline]
fn hash_str_fast(text: &str, hasher: &mut ahash::AHasher) {
    use std::hash::Hash as _;
    let bytes = text.as_bytes();
    bytes.len().hash(hasher);
    if bytes.len() <= 256 {
        bytes.hash(hasher);
    } else {
        bytes[..64].hash(hasher);
        let mid = bytes.len() / 2;
        bytes[mid - 32..mid + 32].hash(hasher);
        bytes[bytes.len() - 64..].hash(hasher);
    }
}

fn hash_value_fast(value: &Value, hasher: &mut ahash::AHasher, mut reflexive: Option<&mut bool>) {
    use std::hash::Hash as _;
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(flag) => {
            1u8.hash(hasher);
            flag.hash(hasher);
        }
        Value::Number(number) => {
            2u8.hash(hasher);
            number.hash(hasher);
        }
        Value::String(text) => {
            3u8.hash(hasher);
            hash_str_fast(text, hasher);
        }
        Value::Array(items) => {
            4u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                hash_value_fast(item, hasher, reflexive.as_deref_mut());
            }
        }
        Value::Object(map) => {
            5u8.hash(hasher);
            map.len().hash(hasher);
            let check_refl = reflexive.as_deref().is_some_and(|r| *r);
            let mut need_cframe_components = false;
            let mut saw_cframe_components = false;
            let mut need_enum_field = false;
            let mut saw_enum_field = false;
            let single_key = map.len() == 1;
            for (k, v) in map {
                if check_refl {
                    let ks = k.as_str();
                    if ks == "_type" {
                        if let Some(type_name) = v.as_str() {
                            match type_name {
                                "CFrame" => need_cframe_components = true,
                                "EnumItem" => need_enum_field = true,
                                "NumberSequence" | "ColorSequence" | "Font" => {
                                    if let Some(refl) = reflexive.as_deref_mut() {
                                        *refl = false;
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else if single_key && matches!(ks, "NumberSequence" | "ColorSequence" | "Ref")
                    {
                        if let Some(refl) = reflexive.as_deref_mut() {
                            *refl = false;
                        }
                    } else if need_cframe_components && ks == "components" {
                        saw_cframe_components = matches!(
                            v,
                            Value::Array(arr) if arr.len() == 12 && arr.iter().all(Value::is_number)
                        );
                    } else if need_enum_field
                        && ((ks == "value" && v.is_u64()) || (ks == "name" && v.is_string()))
                    {
                        saw_enum_field = true;
                    }
                }
                k.hash(hasher);
                hash_value_fast(v, hasher, reflexive.as_deref_mut());
            }
            if check_refl
                && ((need_cframe_components && !saw_cframe_components)
                    || (need_enum_field && !saw_enum_field))
                && let Some(refl) = reflexive
            {
                *refl = false;
            }
        }
    }
}

struct CandidateRefKey<'a> {
    instance: &'a SettingsBytecodeInstance,
    subtree: usize,
    reference_identity: Option<usize>,
    hash: u64,
}

impl<'a> CandidateRefKey<'a> {
    fn new(
        instance: &'a SettingsBytecodeInstance,
        subtree: usize,
        reference_identity: Option<usize>,
    ) -> Self {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = fixed_identity_hasher();
        subtree.hash(&mut hasher);
        reference_identity.hash(&mut hasher);
        instance.properties.len().hash(&mut hasher);
        for (k, v) in &instance.properties {
            k.hash(&mut hasher);
            hash_value_fast(v, &mut hasher, None);
        }
        instance.attributes.len().hash(&mut hasher);
        for (k, v) in &instance.attributes {
            k.hash(&mut hasher);
            hash_value_fast(v, &mut hasher, None);
        }
        Self {
            instance,
            subtree,
            reference_identity,
            hash: hasher.finish(),
        }
    }
}

impl PartialEq for CandidateRefKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
            && self.subtree == other.subtree
            && self.reference_identity == other.reference_identity
            && self.instance.properties == other.instance.properties
            && self.instance.attributes == other.instance.attributes
    }
}

impl Eq for CandidateRefKey<'_> {}

impl std::hash::Hash for CandidateRefKey<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

fn identity_candidate_representatives(
    rows: RowSource<'_>,
    graph: &ReferenceGraph,
    subtree_keys: Option<&[usize]>,
    candidates: &[usize],
) -> (Vec<usize>, AHashMap<usize, usize>) {
    let mut groups = AHashMap::with_capacity(candidates.len());
    let mut representatives = Vec::new();
    let mut multiplicities = AHashMap::with_capacity(candidates.len());
    for &candidate in candidates {
        let instance = rows.get(candidate);
        let key = CandidateRefKey::new(
            instance,
            subtree_keys.map_or(0, |keys| keys[candidate]),
            graph.has_edges(candidate).then_some(candidate),
        );
        let representative = *groups.entry(key).or_insert_with(|| {
            representatives.push(candidate);
            candidate
        });
        *multiplicities.entry(representative).or_default() += 1;
    }
    (representatives, multiplicities)
}

fn identity_candidate_representatives_exact(
    rows: RowSource<'_>,
    graph: &ReferenceGraph,
    exact: &ExactSubtreeKeys,
    candidates: &[usize],
) -> (Vec<usize>, AHashMap<usize, usize>) {
    let mut by_exact_key: AHashMap<usize, usize> = AHashMap::new();
    let mut fallback_groups: AHashMap<CandidateRefKey<'_>, usize> = AHashMap::new();
    let mut representatives = Vec::new();
    let mut multiplicities = AHashMap::with_capacity(candidates.len());
    for &candidate in candidates {
        let subtree = exact.keys.0[candidate];
        let representative = if subtree != 0 && !graph.has_edges(candidate) {
            *by_exact_key.entry(subtree).or_insert_with(|| {
                representatives.push(candidate);
                candidate
            })
        } else {
            let instance = rows.get(candidate);
            let key = CandidateRefKey::new(
                instance,
                subtree,
                graph.has_edges(candidate).then_some(candidate),
            );
            *fallback_groups.entry(key).or_insert_with(|| {
                representatives.push(candidate);
                candidate
            })
        };
        *multiplicities.entry(representative).or_default() += 1;
    }
    (representatives, multiplicities)
}

/// Compact equivalent of `identity_candidate_representatives` / `_exact` that
/// requires zero row hydration: unreferenced candidates are deduplicated by
/// exact subtree key (when available) or by exact non-ref content digest, and
/// referenced candidates are kept individually (matching `CandidateRefKey`,
/// which distinguishes every referenced candidate by its own index).
fn compact_candidate_representatives(
    graph: &ReferenceGraph,
    exact: Option<&ExactSubtreeKeys>,
    ref_meta: &[CompactRowMeta],
    candidates: &[usize],
) -> (Vec<usize>, AHashMap<usize, usize>) {
    let mut by_exact_key: AHashMap<usize, usize> = AHashMap::new();
    let mut by_full_digest: AHashMap<([u8; 16], [u8; 16]), usize> = AHashMap::new();
    let mut representatives = Vec::new();
    let mut multiplicities = AHashMap::with_capacity(candidates.len());
    for &candidate in candidates {
        let representative = if graph.has_edges(candidate) {
            representatives.push(candidate);
            candidate
        } else if let Some(exact) = exact
            && exact.keys.0[candidate] != 0
        {
            *by_exact_key
                .entry(exact.keys.0[candidate])
                .or_insert_with(|| {
                    representatives.push(candidate);
                    candidate
                })
        } else {
            let m = ref_meta[candidate];
            *by_full_digest
                .entry((m.full_digest, m.unique_id_digest))
                .or_insert_with(|| {
                    representatives.push(candidate);
                    candidate
                })
        };
        *multiplicities.entry(representative).or_default() += 1;
    }
    (representatives, multiplicities)
}

struct DocTree {
    child_offsets: Vec<u32>,
    child_indices: Vec<u32>,
}

impl DocTree {
    fn build(document: &SettingsBytecode) -> Self {
        let count = document.instances.len();
        let mut counts = vec![0u32; count];
        for instance in &document.instances {
            if let Some(parent) = instance.parent_index
                && parent < count
            {
                counts[parent] += 1;
            }
        }
        let mut child_offsets = Vec::with_capacity(count + 1);
        let mut total = 0u32;
        for &c in &counts {
            child_offsets.push(total);
            total += c;
        }
        child_offsets.push(total);
        let mut child_indices = vec![0u32; total as usize];
        counts.copy_from_slice(&child_offsets[..count]);
        for (index, instance) in document.instances.iter().enumerate() {
            if let Some(parent) = instance.parent_index
                && parent < count
            {
                let slot = counts[parent] as usize;
                child_indices[slot] = index as u32;
                counts[parent] += 1;
            }
        }
        Self {
            child_offsets,
            child_indices,
        }
    }

    #[inline]
    fn children(&self, index: usize) -> &[u32] {
        let start = self.child_offsets[index] as usize;
        let end = self.child_offsets[index + 1] as usize;
        &self.child_indices[start..end]
    }
}

struct ExactSubtreeKeys {
    keys: (Vec<usize>, Vec<usize>),
    /// True when `holds_same_content(ref[i], ref[i])` is guaranteed true.
    ref_reflexive: Vec<bool>,
}

fn prepare_subtree_levels(
    document: &SettingsBytecode,
    tree: &DocTree,
    mut required: Vec<bool>,
) -> (Vec<bool>, Vec<Vec<u32>>) {
    let count = document.instances.len();
    let mut pending = required
        .iter()
        .enumerate()
        .filter_map(|(index, &needed)| needed.then_some(index as u32))
        .collect::<Vec<_>>();
    while let Some(index) = pending.pop() {
        for &child in tree.children(index as usize) {
            let child_idx = child as usize;
            if !required[child_idx] {
                required[child_idx] = true;
                pending.push(child);
            }
        }
    }
    let mut remaining = Vec::with_capacity(count);
    let mut ready = Vec::new();
    for (index, &is_required) in required.iter().enumerate() {
        let deg = tree.child_offsets[index + 1] - tree.child_offsets[index];
        remaining.push(deg);
        if deg == 0 && is_required {
            ready.push(index as u32);
        }
    }
    let mut height = vec![0u32; count];
    let mut in_levels = vec![false; count];
    let mut levels: Vec<Vec<u32>> = Vec::new();
    while let Some(node) = ready.pop() {
        let idx = node as usize;
        in_levels[idx] = true;
        let h = height[idx] as usize;
        if h >= levels.len() {
            levels.resize_with(h + 1, Vec::new);
        }
        levels[h].push(node);
        if let Some(parent) = document.instances[idx].parent_index
            && parent < count
            && required[parent]
        {
            let next_h = height[idx] + 1;
            if next_h > height[parent] {
                height[parent] = next_h;
            }
            remaining[parent] -= 1;
            if remaining[parent] == 0 {
                ready.push(parent as u32);
            }
        }
    }
    (in_levels, levels)
}

#[inline]
fn property_name_is_stable_no_ref(name: &str, value: &Value) -> bool {
    name != "ScriptGuid"
        && !reconciliation_property_is_derived(name)
        && !reconciliation_property_is_metadata(name, value)
}

fn hash_instance_self_content(
    instance: &SettingsBytecodeInstance,
    no_refs: bool,
    check_reflexive: bool,
) -> (u64, bool) {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = fixed_identity_hasher();
    let mut reflexive = check_reflexive;
    instance.name.hash(&mut hasher);
    instance.class_name.hash(&mut hasher);
    0x11u8.hash(&mut hasher);
    for (name, value) in &instance.properties {
        if !property_name_is_stable_no_ref(name, value) {
            continue;
        }
        if !no_refs && reconciliation_value_contains_reference(value) {
            continue;
        }
        name.hash(&mut hasher);
        hash_value_fast(
            value,
            &mut hasher,
            check_reflexive.then_some(&mut reflexive),
        );
    }
    0x22u8.hash(&mut hasher);
    for (name, value) in &instance.attributes {
        if !no_refs && reconciliation_value_contains_reference(value) {
            continue;
        }
        name.hash(&mut hasher);
        hash_value_fast(
            value,
            &mut hasher,
            check_reflexive.then_some(&mut reflexive),
        );
    }
    (hasher.finish(), reflexive)
}

fn exact_stable_map_eq(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ignore_transient: bool,
    left_no_refs: bool,
    right_no_refs: bool,
) -> bool {
    if !ignore_transient && left_no_refs && right_no_refs {
        return left == right;
    }
    let mut left_iter = left.iter().filter(|(name, value)| {
        (!ignore_transient || property_name_is_stable_no_ref(name, value))
            && (left_no_refs || !reconciliation_value_contains_reference(value))
    });
    let mut right_iter = right.iter().filter(|(name, value)| {
        (!ignore_transient || property_name_is_stable_no_ref(name, value))
            && (right_no_refs || !reconciliation_value_contains_reference(value))
    });
    loop {
        match (left_iter.next(), right_iter.next()) {
            (Some((lk, lv)), Some((rk, rv))) => {
                if lk != rk || lv != rv {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn compute_subtree_keys(
    reference: &SettingsBytecode,
    ref_graph: &ReferenceGraph,
    ref_required: Vec<bool>,
    observed: &SettingsBytecode,
    obs_graph: &ReferenceGraph,
    obs_tree: &DocTree,
    obs_required: Vec<bool>,
) -> ExactSubtreeKeys {
    use std::hash::{Hash as _, Hasher as _};
    const OBS_BIT: u32 = 1 << 31;
    const IDX_MASK: u32 = !OBS_BIT;

    let ref_count = reference.instances.len();
    let obs_count = observed.instances.len();
    assert!(ref_count < (OBS_BIT as usize) && obs_count < (OBS_BIT as usize));

    let ((ref_tree, (ref_in_levels, ref_levels)), (obs_in_levels, obs_levels)) = rayon::join(
        || {
            let tree = DocTree::build(reference);
            let prepared = prepare_subtree_levels(reference, &tree, ref_required);
            (tree, prepared)
        },
        || prepare_subtree_levels(observed, obs_tree, obs_required),
    );

    let parallel = ref_count + obs_count >= 512 && rayon::current_num_threads() > 1;
    let ((ref_self_hash, ref_reflexive), obs_self_hash) = rayon::join(
        || {
            let compute = |index: usize| {
                if !ref_in_levels[index] {
                    (0u64, false)
                } else {
                    hash_instance_self_content(
                        &reference.instances[index],
                        !ref_graph.has_outgoing(index),
                        true,
                    )
                }
            };
            let pairs: Vec<(u64, bool)> = if parallel {
                (0..ref_count).into_par_iter().map(compute).collect()
            } else {
                (0..ref_count).map(compute).collect()
            };
            let mut hashes = Vec::with_capacity(ref_count);
            let mut refl = Vec::with_capacity(ref_count);
            for (h, r) in pairs {
                hashes.push(h);
                refl.push(r);
            }
            (hashes, refl)
        },
        || {
            let compute = |index: usize| {
                if !obs_in_levels[index] {
                    0u64
                } else {
                    hash_instance_self_content(
                        &observed.instances[index],
                        !obs_graph.has_outgoing(index),
                        false,
                    )
                    .0
                }
            };
            if parallel {
                (0..obs_count)
                    .into_par_iter()
                    .map(compute)
                    .collect::<Vec<_>>()
            } else {
                (0..obs_count).map(compute).collect::<Vec<_>>()
            }
        },
    );
    drop(ref_in_levels);
    drop(obs_in_levels);

    let max_height = ref_levels.len().max(obs_levels.len());
    let mut ref_result = vec![0usize; ref_count];
    let mut obs_result = vec![0usize; obs_count];
    let mut ref_child_keys = if max_height > 1 {
        vec![0u32; ref_tree.child_indices.len()]
    } else {
        Vec::new()
    };
    let mut obs_child_keys = if max_height > 1 {
        vec![0u32; obs_tree.child_indices.len()]
    } else {
        Vec::new()
    };

    let mut next_id = 1usize;
    for height in 0..max_height {
        let r_nodes = ref_levels.get(height).map_or(&[][..], Vec::as_slice);
        let o_nodes = obs_levels.get(height).map_or(&[][..], Vec::as_slice);
        let total = r_nodes.len() + o_nodes.len();
        if total == 0 {
            continue;
        }
        if height > 0 {
            for &node in r_nodes {
                let idx = node as usize;
                let start = ref_tree.child_offsets[idx] as usize;
                let end = ref_tree.child_offsets[idx + 1] as usize;
                for (slot, &child) in ref_child_keys[start..end]
                    .iter_mut()
                    .zip(&ref_tree.child_indices[start..end])
                {
                    *slot = ref_result[child as usize] as u32;
                }
                ref_child_keys[start..end].sort_unstable();
            }
            for &node in o_nodes {
                let idx = node as usize;
                let start = obs_tree.child_offsets[idx] as usize;
                let end = obs_tree.child_offsets[idx + 1] as usize;
                for (slot, &child) in obs_child_keys[start..end]
                    .iter_mut()
                    .zip(&obs_tree.child_indices[start..end])
                {
                    *slot = obs_result[child as usize] as u32;
                }
                obs_child_keys[start..end].sort_unstable();
            }
        }

        let full_hash_ref = |node: u32| {
            let idx = node as usize;
            let base = ref_self_hash[idx];
            if height == 0 {
                base
            } else {
                let start = ref_tree.child_offsets[idx] as usize;
                let end = ref_tree.child_offsets[idx + 1] as usize;
                let mut h = fixed_identity_hasher();
                base.hash(&mut h);
                ref_child_keys[start..end].hash(&mut h);
                h.finish()
            }
        };
        let full_hash_obs = |node: u32| {
            let idx = node as usize;
            let base = obs_self_hash[idx];
            if height == 0 {
                base
            } else {
                let start = obs_tree.child_offsets[idx] as usize;
                let end = obs_tree.child_offsets[idx + 1] as usize;
                let mut h = fixed_identity_hasher();
                base.hash(&mut h);
                obs_child_keys[start..end].hash(&mut h);
                h.finish()
            }
        };

        let num_shards = if total >= 512 && rayon::current_num_threads() > 1 {
            64usize
        } else {
            1usize
        };
        let mut shards: Vec<Vec<(u64, u32)>> = if num_shards == 1 {
            vec![Vec::with_capacity(total)]
        } else {
            let per = (total / num_shards).saturating_add(16);
            (0..num_shards).map(|_| Vec::with_capacity(per)).collect()
        };
        if num_shards == 1 {
            let shard = &mut shards[0];
            for &node in r_nodes {
                shard.push((full_hash_ref(node), node));
            }
            for &node in o_nodes {
                shard.push((full_hash_obs(node), node | OBS_BIT));
            }
        } else {
            for &node in r_nodes {
                let h = full_hash_ref(node);
                shards[(h >> 58) as usize].push((h, node));
            }
            for &node in o_nodes {
                let h = full_hash_obs(node);
                shards[(h >> 58) as usize].push((h, node | OBS_BIT));
            }
        }

        let nodes_equal = |left_packed: u32, right_packed: u32| -> bool {
            let (left_obs, left_idx) = (
                (left_packed & OBS_BIT) != 0,
                (left_packed & IDX_MASK) as usize,
            );
            let (right_obs, right_idx) = (
                (right_packed & OBS_BIT) != 0,
                (right_packed & IDX_MASK) as usize,
            );
            let left_inst = if left_obs {
                &observed.instances[left_idx]
            } else {
                &reference.instances[left_idx]
            };
            let right_inst = if right_obs {
                &observed.instances[right_idx]
            } else {
                &reference.instances[right_idx]
            };
            if left_inst.name != right_inst.name || left_inst.class_name != right_inst.class_name {
                return false;
            }
            if height > 0 {
                let left_kids = if left_obs {
                    let s = obs_tree.child_offsets[left_idx] as usize;
                    let e = obs_tree.child_offsets[left_idx + 1] as usize;
                    &obs_child_keys[s..e]
                } else {
                    let s = ref_tree.child_offsets[left_idx] as usize;
                    let e = ref_tree.child_offsets[left_idx + 1] as usize;
                    &ref_child_keys[s..e]
                };
                let right_kids = if right_obs {
                    let s = obs_tree.child_offsets[right_idx] as usize;
                    let e = obs_tree.child_offsets[right_idx + 1] as usize;
                    &obs_child_keys[s..e]
                } else {
                    let s = ref_tree.child_offsets[right_idx] as usize;
                    let e = ref_tree.child_offsets[right_idx + 1] as usize;
                    &ref_child_keys[s..e]
                };
                if left_kids != right_kids {
                    return false;
                }
            }
            let left_no_refs = if left_obs {
                !obs_graph.has_outgoing(left_idx)
            } else {
                !ref_graph.has_outgoing(left_idx)
            };
            let right_no_refs = if right_obs {
                !obs_graph.has_outgoing(right_idx)
            } else {
                !ref_graph.has_outgoing(right_idx)
            };
            exact_stable_map_eq(
                &left_inst.properties,
                &right_inst.properties,
                true,
                left_no_refs,
                right_no_refs,
            ) && exact_stable_map_eq(
                &left_inst.attributes,
                &right_inst.attributes,
                false,
                left_no_refs,
                right_no_refs,
            )
        };

        let intern_shard = |shard: &mut Vec<(u64, u32)>| -> usize {
            shard.sort_unstable_by_key(|&(h, _)| h);
            let mut local_next = 0usize;
            let mut reps: Vec<(u32, usize)> = Vec::new();
            let mut pos = 0usize;
            while pos < shard.len() {
                let h = shard[pos].0;
                let mut end = pos + 1;
                while end < shard.len() && shard[end].0 == h {
                    end += 1;
                }
                if end == pos + 1 {
                    shard[pos].0 = local_next as u64;
                    local_next += 1;
                } else {
                    reps.clear();
                    for item in &mut shard[pos..end] {
                        let packed = item.1;
                        let found = reps
                            .iter()
                            .find(|&&(rep, _)| nodes_equal(packed, rep))
                            .map(|&(_, id)| id);
                        let id = match found {
                            Some(id) => id,
                            None => {
                                let id = local_next;
                                local_next += 1;
                                reps.push((packed, id));
                                id
                            }
                        };
                        item.0 = id as u64;
                    }
                }
                pos = end;
            }
            local_next
        };

        let counts: Vec<usize> = if num_shards > 1 {
            shards.par_iter_mut().map(intern_shard).collect()
        } else {
            shards.iter_mut().map(intern_shard).collect()
        };

        for (shard, count) in shards.into_iter().zip(counts) {
            let base = next_id;
            next_id += count;
            for (local_id, packed) in shard {
                let id = base + local_id as usize;
                let idx = (packed & IDX_MASK) as usize;
                if (packed & OBS_BIT) != 0 {
                    obs_result[idx] = id;
                } else {
                    ref_result[idx] = id;
                }
            }
        }
    }

    ExactSubtreeKeys {
        keys: (ref_result, obs_result),
        ref_reflexive,
    }
}

// Intern exact content, not hashes used as identity. Children are an unordered multiset;
// references remain separate, stronger evidence in identity_score. A non-match here
// never excludes a candidate (float rounding or an actual edit may change its key).
#[cfg(test)]
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

#[derive(Clone, Copy)]
enum RowSource<'a> {
    Full(&'a [SettingsBytecodeInstance]),
    Hydrated(&'a AHashMap<usize, SettingsBytecodeInstance>),
}

impl<'a> RowSource<'a> {
    #[inline]
    fn get(self, index: usize) -> &'a SettingsBytecodeInstance {
        match self {
            Self::Full(instances) => &instances[index],
            Self::Hydrated(rows) => &rows[&index],
        }
    }
}

struct IdentityScoreContext<'a> {
    reference: RowSource<'a>,
    observed: RowSource<'a>,
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
    let reference_instance = context.reference.get(reference_index);
    let observed_instance = context.observed.get(observed_index);
    let reference_evidence = matching_reference_evidence(
        reference_index,
        observed_index,
        context.reference_graph,
        context.observed_graph,
        context.assigned_reference,
        context.assigned_observed,
    )?;
    let content_matches = holds_same_content_hint(
        reference_instance,
        observed_instance,
        !context.reference_graph.has_outgoing(reference_index),
        !context.observed_graph.has_outgoing(observed_index),
    );
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
    holds_same_content_hint(reference, observed, false, false)
}

fn holds_same_content_hint(
    reference: &SettingsBytecodeInstance,
    observed: &SettingsBytecodeInstance,
    reference_no_refs: bool,
    observed_no_refs: bool,
) -> bool {
    reconciliation_identity_maps_equal_hint(
        &reference.properties,
        &observed.properties,
        true,
        reference_no_refs,
        observed_no_refs,
    ) && reconciliation_identity_maps_equal_hint(
        &reference.attributes,
        &observed.attributes,
        false,
        reference_no_refs,
        observed_no_refs,
    )
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
    if document.instances.len() >= 8_192 && rayon::current_num_threads() > 1 {
        let chunks: Vec<Vec<(&str, usize)>> = document
            .instances
            .par_chunks(2_048)
            .enumerate()
            .map(|(chunk_idx, chunk)| {
                let base = chunk_idx * 2_048;
                let mut found = Vec::new();
                for (offset, instance) in chunk.iter().enumerate() {
                    if let Some(id) = persistent_identity(instance) {
                        found.push((id, base + offset));
                    }
                }
                found
            })
            .collect();
        let total: usize = chunks.iter().map(Vec::len).sum();
        if total == 0 {
            return AHashMap::new();
        }
        let mut result = AHashMap::with_capacity(total);
        for chunk in chunks {
            for (id, index) in chunk {
                result
                    .entry(id)
                    .and_modify(|value| *value = None)
                    .or_insert(Some(index));
            }
        }
        return result;
    }
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

enum GroupIndices {
    One(usize),
    Many(Vec<usize>),
}

impl GroupIndices {
    #[inline]
    fn push(&mut self, index: usize) {
        match self {
            Self::One(first) => {
                let first = *first;
                *self = Self::Many(vec![first, index]);
            }
            Self::Many(indices) => indices.push(index),
        }
    }

    #[inline]
    fn as_slice(&self) -> &[usize] {
        match self {
            Self::One(index) => std::slice::from_ref(index),
            Self::Many(indices) => indices.as_slice(),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(indices) => indices.len(),
        }
    }
}

struct IdentityLookup<'a> {
    reference_by_id: AHashMap<&'a str, usize>,
    reference_by_unique_id: AHashMap<&'a str, Option<usize>>,
    observed_by_unique_id: AHashMap<&'a str, Option<usize>>,
    reference_ids_unique: bool,
}

struct ScoreInputs<'a, 'b> {
    reference_groups: &'b AHashMap<IdentityGroupKey<'a>, GroupIndices>,
    reference_graph: &'b ReferenceGraph,
    observed_graph: &'b ReferenceGraph,
    subtree_keys: Option<&'b ExactSubtreeKeys>,
    observed_tree: &'b DocTree,
    /// The matching stops at instances only what they hold tells apart.
    told_alone: bool,
    compact: Option<&'b CompactMatching<'b>>,
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
    /// The ids of the reference, lazily built on the first `assign_fresh`.
    reserved_reference_ids: Option<AHashSet<&'a str>>,
    /// The ids an instance without a counterpart cannot be given a made-up
    /// id of: those of both copies and the ones made up so far. Read at the
    /// first id that has to be made up.
    blocked_ids: Option<AHashSet<String>>,
    /// Fresh ids assigned to observed instances without a reference counterpart.
    assigned_fresh: AHashMap<usize, String>,
    assigned_reference: Vec<Option<usize>>,
    assigned_observed: Vec<Option<usize>>,
    /// Ids consumed by `assign_fresh`, lazily populated.
    fresh_used_ids: AHashSet<std::borrow::Cow<'a, str>>,
    generated: usize,
    remaining: usize,
    shape_keys: Option<(Vec<u64>, Vec<u64>)>,
    /// A matching that stops at what instances hold reached such a group.
    compares_values: bool,
    /// The observed instances such a matching took by their order among
    /// their namesakes under one parent, where it pairs them so.
    by_order: Option<Vec<(usize, ByOrder)>>,
    /// Ready unassigned observed indices for the frontier loop.
    active_ready: Vec<usize>,
    /// Newly unblocked children collected during a pass.
    newly_ready: Vec<usize>,
    /// Stalled ref-free groups with `key.0.is_some()` waiting for fallback.
    stalled_groups: Vec<(IdentityGroupKey<'a>, Vec<usize>)>,
}

/// Hash of an instance's subtree by names and classes only, so same-named
/// siblings that swapped places pair with the subtree they came from.
fn subtree_shape_keys(document: &SettingsBytecode) -> Vec<u64> {
    use std::hash::{Hash, Hasher};
    let count = document.instances.len();
    let mut counts = vec![0u32; count];
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index
            && parent < index
        {
            counts[parent] += 1;
        }
    }
    let mut offsets = Vec::with_capacity(count + 1);
    let mut total = 0u32;
    for &c in &counts {
        offsets.push(total);
        total += c;
    }
    offsets.push(total);
    let mut children = vec![0u32; total as usize];
    counts.copy_from_slice(&offsets[..count]);
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index
            && parent < index
        {
            let pos = counts[parent] as usize;
            children[pos] = index as u32;
            counts[parent] += 1;
        }
    }
    let mut keys = vec![0u64; count];
    let mut child_keys = Vec::new();
    for index in (0..count).rev() {
        let instance = &document.instances[index];
        let start = offsets[index] as usize;
        let end = offsets[index + 1] as usize;
        child_keys.clear();
        child_keys.extend(
            children[start..end]
                .iter()
                .map(|&child| keys[child as usize]),
        );
        child_keys.sort_unstable();
        let mut hasher = ahash::AHasher::default();
        instance.name.hash(&mut hasher);
        instance.class_name.hash(&mut hasher);
        child_keys.hash(&mut hasher);
        keys[index] = hasher.finish();
    }
    keys
}

enum NumberPointerStep {
    Key(String),
    Index(usize),
}

enum CompiledNumberAccess {
    Direct,
    Field(String),
    ArrayIndex(String, usize),
    Steps(Vec<NumberPointerStep>),
}

impl CompiledNumberAccess {
    fn from_steps(steps: &[NumberPointerStep]) -> Self {
        match steps {
            [] => Self::Direct,
            [NumberPointerStep::Key(k)] => Self::Field(k.clone()),
            [NumberPointerStep::Key(k), NumberPointerStep::Index(i)] => {
                Self::ArrayIndex(k.clone(), *i)
            }
            _ => Self::Steps(
                steps
                    .iter()
                    .map(|step| match step {
                        NumberPointerStep::Key(k) => NumberPointerStep::Key(k.clone()),
                        NumberPointerStep::Index(i) => NumberPointerStep::Index(*i),
                    })
                    .collect(),
            ),
        }
    }

    #[inline]
    fn read<'a>(&self, value: &'a Value) -> Option<&'a Value> {
        match self {
            Self::Direct => Some(value),
            Self::Field(key) => value.as_object()?.get(key),
            Self::ArrayIndex(key, index) => value.as_object()?.get(key)?.as_array()?.get(*index),
            Self::Steps(steps) => {
                let mut current = value;
                for step in steps {
                    current = match step {
                        NumberPointerStep::Key(key) => current.as_object()?.get(key)?,
                        NumberPointerStep::Index(index) => current.as_array()?.get(*index)?,
                    };
                }
                Some(current)
            }
        }
    }
}

struct IdentityNumberField {
    attribute: bool,
    property: String,
    #[allow(dead_code)]
    pointer: String,
    access: CompiledNumberAccess,
}

impl IdentityNumberField {
    #[inline]
    fn read(&self, instance: &SettingsBytecodeInstance) -> Option<f64> {
        let values = if self.attribute {
            &instance.attributes
        } else {
            &instance.properties
        };
        let value = values.get(&self.property)?;
        // Enum equality can use either name or number, so its number is not a safe filter.
        if !matches!(self.access, CompiledNumberAccess::Direct) && contains_identity_enum(value) {
            return None;
        }
        self.access
            .read(value)?
            .as_f64()
            .filter(|value| value.is_finite())
    }
}

fn contains_identity_enum(value: &Value) -> bool {
    match value {
        Value::Object(values) => values.iter().any(|(key, val)| {
            (key == "_type" && val.as_str() == Some("EnumItem"))
                || (matches!(val, Value::Object(_) | Value::Array(_))
                    && contains_identity_enum(val))
        }),
        Value::Array(values) => values.iter().any(|val| {
            matches!(val, Value::Object(_) | Value::Array(_)) && contains_identity_enum(val)
        }),
        _ => false,
    }
}

fn identity_number_pointers(
    value: &Value,
    pointer: &mut String,
    steps: &mut Vec<NumberPointerStep>,
    output: &mut Vec<(String, CompiledNumberAccess)>,
) {
    match value {
        Value::Number(_) => {
            output.push((pointer.clone(), CompiledNumberAccess::from_steps(steps)));
        }
        Value::Object(values) => {
            for (name, value) in values {
                let length = pointer.len();
                pointer.push('/');
                if name.contains('~') || name.contains('/') {
                    pointer.push_str(&name.replace('~', "~0").replace('/', "~1"));
                } else {
                    pointer.push_str(name);
                }
                steps.push(NumberPointerStep::Key(name.clone()));
                identity_number_pointers(value, pointer, steps, output);
                steps.pop();
                pointer.truncate(length);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let length = pointer.len();
                write!(pointer, "/{index}").expect("writing to a String cannot fail");
                steps.push(NumberPointerStep::Index(index));
                identity_number_pointers(value, pointer, steps, output);
                steps.pop();
                pointer.truncate(length);
            }
        }
        _ => {}
    }
}

struct IdentityCandidateIndex {
    field: IdentityNumberField,
    values: Vec<(f64, usize)>,
    secondary: Option<(IdentityNumberField, Vec<f64>)>,
}

#[inline]
fn identity_number_radius(value: f64) -> f64 {
    let epsilon = 4.0 * f64::from(f32::EPSILON);
    ((epsilon + 4.0 * f64::EPSILON) / (1.0 - epsilon) * value.abs().max(1.0)).max(
        CFRAME_ROTATION_EPSILON
            .max(CFRAME_ROTATION_NOISE)
            .max(CFRAME_POSITION_NOISE)
            + 4.0 * f64::EPSILON,
    )
}

impl IdentityCandidateIndex {
    fn build(rows: RowSource<'_>, candidates: &[usize]) -> Option<Self> {
        if candidates.len() < 32 {
            return None;
        }
        let first = rows.get(candidates[0]);
        let mut fields = Vec::new();
        let mut pointer_buf = String::new();
        let mut steps_buf = Vec::new();
        for (attribute, properties) in [(false, &first.properties), (true, &first.attributes)] {
            for (property, value) in properties {
                if !identity_value_is_stable(property, value, !attribute)
                    || contains_identity_enum(value)
                {
                    continue;
                }
                let mut pointers = Vec::new();
                pointer_buf.clear();
                steps_buf.clear();
                identity_number_pointers(value, &mut pointer_buf, &mut steps_buf, &mut pointers);
                for (pointer, access) in pointers {
                    fields.push(IdentityNumberField {
                        attribute,
                        property: property.clone(),
                        pointer,
                        access,
                    });
                }
            }
        }
        if fields.is_empty() {
            return None;
        }

        let mut seen = AHashSet::with_capacity(candidates.len().min(256));
        let (chosen_primary, chosen_secondary) = if candidates.len() > 256 {
            let sample_len = 128usize;
            let step = candidates.len() / sample_len;
            let sample = (0..sample_len)
                .map(|i| candidates[i * step])
                .collect::<Vec<_>>();
            let mut best_idx = None;
            let mut best_distinct = 1usize;
            let mut second_idx = None;
            let mut second_distinct = 1usize;
            for (idx, field) in fields.iter().enumerate() {
                seen.clear();
                for &cand in &sample {
                    if let Some(v) = field.read(rows.get(cand)) {
                        seen.insert(v.to_bits());
                    }
                }
                let distinct = seen.len();
                if distinct > best_distinct {
                    second_distinct = best_distinct;
                    second_idx = best_idx;
                    best_distinct = distinct;
                    best_idx = Some(idx);
                } else if distinct > second_distinct {
                    second_distinct = distinct;
                    second_idx = Some(idx);
                }
                if best_distinct == sample_len && second_distinct >= sample_len / 2 {
                    break;
                }
            }
            (best_idx, second_idx)
        } else {
            (None, None)
        };

        let mut secondary_field: Option<IdentityNumberField> = None;
        let mut best = if let Some(primary_idx) = chosen_primary {
            let mut fields = fields.into_iter().map(Some).collect::<Vec<_>>();
            let field = fields[primary_idx].take().unwrap();
            secondary_field = chosen_secondary.and_then(|idx| fields[idx].take());
            let values: Vec<(f64, usize)> =
                if candidates.len() >= 2_048 && rayon::current_num_threads() > 1 {
                    candidates
                        .par_iter()
                        .filter_map(|&index| field.read(rows.get(index)).map(|v| (v, index)))
                        .collect()
                } else {
                    candidates
                        .iter()
                        .filter_map(|&index| field.read(rows.get(index)).map(|v| (v, index)))
                        .collect()
                };
            Some(Self {
                field,
                values,
                secondary: None,
            })
        } else {
            let mut best: Option<Self> = None;
            let mut best_distinct = 1usize;
            for field in fields {
                seen.clear();
                let mut values = Vec::with_capacity(candidates.len());
                for &index in candidates {
                    if let Some(value) = field.read(rows.get(index)) {
                        seen.insert(value.to_bits());
                        values.push((value, index));
                    }
                }
                let distinct = seen.len();
                if distinct > best_distinct {
                    best_distinct = distinct;
                    best = Some(Self {
                        field,
                        values,
                        secondary: None,
                    });
                    if best_distinct == candidates.len() {
                        break;
                    }
                }
            }
            best
        };

        if let Some(index) = best.as_mut() {
            if index.values.len() >= 4_096 && rayon::current_num_threads() > 1 {
                index
                    .values
                    .par_sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
            } else {
                index
                    .values
                    .sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
            }
            if let Some(sec_field) = secondary_field {
                let sec_values: Vec<f64> =
                    if index.values.len() >= 2_048 && rayon::current_num_threads() > 1 {
                        index
                            .values
                            .par_iter()
                            .map(|&(_, cand)| sec_field.read(rows.get(cand)).unwrap_or(f64::NAN))
                            .collect()
                    } else {
                        index
                            .values
                            .iter()
                            .map(|&(_, cand)| sec_field.read(rows.get(cand)).unwrap_or(f64::NAN))
                            .collect()
                    };
                index.secondary = Some((sec_field, sec_values));
            }
        }
        best
    }

    fn matching_range(
        &self,
        instance: &SettingsBytecodeInstance,
    ) -> Option<std::ops::Range<usize>> {
        let value = self.field.read(instance)?;
        let radius = identity_number_radius(value);
        let start = self
            .values
            .partition_point(|(candidate, _)| *candidate < value - radius);
        let end = self
            .values
            .partition_point(|(candidate, _)| *candidate <= value + radius);
        Some(start..end)
    }

    fn matching(&self, instance: &SettingsBytecodeInstance) -> Option<&[(f64, usize)]> {
        let range = self.matching_range(instance)?;
        Some(&self.values[range])
    }
}

#[cfg(test)]
fn identity_group_proposals(
    context: &IdentityScoreContext<'_>,
    observed_group: &[usize],
    candidates: &[usize],
) -> Vec<(usize, usize)> {
    identity_group_proposals_with_exact(context, observed_group, candidates, None)
}

fn identity_group_proposals_with_exact(
    context: &IdentityScoreContext<'_>,
    observed_group: &[usize],
    candidates: &[usize],
    exact_subtree: Option<&ExactSubtreeKeys>,
) -> Vec<(usize, usize)> {
    let unreferenced = |index: usize| !context.observed_graph.has_edges(index);
    let (mut results, fallback) = if let Some(exact) = exact_subtree {
        exact_subtree_fast_proposals(
            context.reference_graph,
            context.observed_graph,
            context.assigned_observed,
            exact,
            observed_group,
            candidates,
        )
    } else {
        (
            vec![None; observed_group.len()],
            observed_group
                .iter()
                .enumerate()
                .map(|(pos, &idx)| (pos, idx))
                .collect(),
        )
    };
    if !fallback.is_empty() {
        let (representatives, multiplicities) = if let Some(exact) = exact_subtree {
            identity_candidate_representatives_exact(
                context.reference,
                context.reference_graph,
                exact,
                candidates,
            )
        } else {
            identity_candidate_representatives(
                context.reference,
                context.reference_graph,
                context
                    .subtree_keys
                    .map(|(reference, _)| reference.as_slice()),
                candidates,
            )
        };
        let content_index = if representatives.len() > 1
            && fallback.iter().any(|&(_, index)| unreferenced(index))
        {
            IdentityCandidateIndex::build(context.reference, &representatives)
        } else {
            None
        };
        for (pos, candidate) in score_fallback_entries(
            context,
            &representatives,
            &multiplicities,
            content_index.as_ref(),
            &fallback,
        ) {
            results[pos] = Some(candidate);
        }
    }
    observed_group
        .iter()
        .zip(results)
        .filter_map(|(&index, candidate)| candidate.map(|candidate| (index, candidate)))
        .collect()
}

impl<'a> IdentityAssignment<'a> {
    fn new(reference: &'a SettingsBytecode, observed: &'a SettingsBytecode) -> Self {
        Self {
            reference,
            observed,
            reserved_reference_ids: None,
            blocked_ids: None,
            assigned_fresh: AHashMap::new(),
            assigned_reference: vec![None; observed.instances.len()],
            assigned_observed: vec![None; reference.instances.len()],
            fresh_used_ids: AHashSet::new(),
            generated: 0,
            remaining: observed.instances.len(),
            shape_keys: None,
            compares_values: false,
            by_order: None,
            active_ready: Vec::new(),
            newly_ready: Vec::new(),
            stalled_groups: Vec::new(),
        }
    }

    #[inline]
    fn is_assigned(&self, index: usize) -> bool {
        self.assigned_reference[index].is_some()
            || (!self.assigned_fresh.is_empty() && self.assigned_fresh.contains_key(&index))
    }

    #[inline]
    fn is_reference_used(&self, candidate: usize) -> bool {
        self.assigned_observed[candidate].is_some()
    }

    fn assign(&mut self, index: usize, candidate: usize) {
        self.assigned_reference[index] = Some(candidate);
        self.assigned_observed[candidate] = Some(index);
        self.remaining -= 1;
    }

    fn assign_and_unblock(&mut self, index: usize, candidate: usize, observed_tree: &DocTree) {
        self.assign(index, candidate);
        for &child in observed_tree.children(index) {
            let child = child as usize;
            if !self.is_assigned(child) {
                self.newly_ready.push(child);
            }
        }
    }

    fn assign_fresh(&mut self, index: usize) {
        let (reference, observed) = (self.reference, self.observed);
        let reserved = self.reserved_reference_ids.get_or_insert_with(|| {
            let mut set = AHashSet::with_capacity(reference.instances.len());
            for instance in &reference.instances {
                set.insert(instance.settings_id.as_str());
            }
            set
        });
        let preferred = observed.instances[index].settings_id.as_str();
        let id = if !reserved.contains(preferred)
            && self
                .fresh_used_ids
                .insert(std::borrow::Cow::Borrowed(preferred))
        {
            preferred.to_string()
        } else {
            let blocked = self.blocked_ids.get_or_insert_with(|| {
                let mut set =
                    AHashSet::with_capacity(reference.instances.len() + observed.instances.len());
                for instance in reference.instances.iter().chain(&observed.instances) {
                    set.insert(instance.settings_id.clone());
                }
                set
            });
            loop {
                let candidate = format!("reconcile:{:x}", self.generated);
                self.generated += 1;
                if blocked.insert(candidate.clone())
                    && self
                        .fresh_used_ids
                        .insert(std::borrow::Cow::Owned(candidate.clone()))
                {
                    break candidate;
                }
            }
        };
        self.assigned_fresh.insert(index, id);
        self.remaining -= 1;
    }

    fn assign_fresh_and_unblock(&mut self, index: usize, observed_tree: &DocTree) {
        self.assign_fresh(index);
        for &child in observed_tree.children(index) {
            let child = child as usize;
            if !self.is_assigned(child) {
                self.newly_ready.push(child);
            }
        }
    }

    fn init_frontier(&mut self) {
        self.active_ready.clear();
        self.newly_ready.clear();
        self.stalled_groups.clear();
        let count = self.observed.instances.len();
        for (index, instance) in self.observed.instances.iter().enumerate() {
            if self.is_assigned(index) {
                continue;
            }
            let parent_ready = match instance.parent_index {
                Some(parent) => parent < count && self.is_assigned(parent),
                None => true,
            };
            if parent_ready {
                self.active_ready.push(index);
            }
        }
    }

    fn flush_frontier(&mut self) {
        let assigned_ref = &self.assigned_reference;
        let assigned_fresh = &self.assigned_fresh;
        self.active_ready.retain(|&index| {
            assigned_ref[index].is_none()
                && (assigned_fresh.is_empty() || !assigned_fresh.contains_key(&index))
        });
        self.active_ready.append(&mut self.newly_ready);
    }

    fn active_groups(&self) -> Vec<(IdentityGroupKey<'a>, GroupIndices)> {
        let mut by_key: AHashMap<IdentityGroupKey<'a>, GroupIndices> =
            AHashMap::with_capacity((self.active_ready.len() / 2).clamp(16, 4_096));
        for &index in &self.active_ready {
            if self.is_assigned(index) {
                continue;
            }
            let instance = &self.observed.instances[index];
            let reference_parent = instance
                .parent_index
                .and_then(|parent| self.assigned_reference[parent]);
            let key = (
                reference_parent,
                instance.name.as_str(),
                instance.class_name.as_str(),
            );
            match by_key.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().push(index);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(GroupIndices::One(index));
                }
            }
        }
        let mut groups: Vec<(IdentityGroupKey<'a>, GroupIndices)> = by_key.into_iter().collect();
        for (_, indices) in &mut groups {
            if let GroupIndices::Many(vec) = indices {
                vec.sort_unstable();
            }
        }
        groups.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        groups
    }

    fn run_exact(
        &mut self,
        lookup: &IdentityLookup<'a>,
        initial_camera_ref: Option<usize>,
        obs_unique_ids: Option<&[Option<Box<str>>]>,
    ) -> bool {
        let reference = self.reference;
        let observed = self.observed;
        let mut used_exact_ids = (!lookup.reference_ids_unique).then(|| {
            let mut set = AHashSet::new();
            if let Some(cam) = initial_camera_ref {
                set.insert(reference.instances[cam].settings_id.as_str());
            }
            set
        });
        let mut progressed = false;
        for (index, instance) in observed.instances.iter().enumerate() {
            if self.is_assigned(index) {
                continue;
            }
            let persistent_id = match obs_unique_ids {
                Some(ids) => ids[index].as_deref(),
                None => persistent_identity(instance),
            };
            let persistent = persistent_id
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
                    !self.is_reference_used(*candidate)
                        && used_exact_ids.as_ref().is_none_or(|used| {
                            !used.contains(reference.instances[*candidate].settings_id.as_str())
                        })
                })
            else {
                continue;
            };
            if let Some(used) = &mut used_exact_ids {
                used.insert(reference.instances[candidate].settings_id.as_str());
            }
            self.assign(index, candidate);
            progressed = true;
        }
        progressed
    }

    fn run_unique(&mut self, reference_groups: &AHashMap<IdentityGroupKey<'a>, GroupIndices>) {
        let observed = self.observed;
        let mut first_obs = AHashMap::with_capacity(observed.instances.len() / 4);
        let mut obs_unique = vec![true; observed.instances.len()];
        for (index, instance) in observed.instances.iter().enumerate() {
            let key = (
                instance.parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            );
            if let Some(prev) = first_obs.insert(key, index) {
                obs_unique[prev] = false;
                obs_unique[index] = false;
            }
        }
        drop(first_obs);
        // Resolve parent-before-child singleton groups in one pass. The scored
        // path makes the same decision, but builds a Vec for every unique child
        // and revisits the entire document at each depth after a reorder.
        for (index, instance) in observed.instances.iter().enumerate() {
            if self.is_assigned(index) || !obs_unique[index] {
                continue;
            }
            let parent = match instance.parent_index {
                Some(parent) => match self.assigned_reference.get(parent).copied().flatten() {
                    Some(parent) => Some(parent),
                    None => continue,
                },
                None => None,
            };
            let Some(GroupIndices::One(candidate)) = reference_groups.get(&(
                parent,
                instance.name.as_str(),
                instance.class_name.as_str(),
            )) else {
                continue;
            };
            if !self.is_reference_used(*candidate) {
                self.assign(index, *candidate);
            }
        }
    }

    fn run_scored(
        &mut self,
        inputs: &ScoreInputs<'a, '_>,
        unchanged_content_groups: &mut AHashMap<usize, (Vec<usize>, Vec<usize>)>,
    ) -> Result<bool> {
        let reference = self.reference;
        let observed = self.observed;
        let groups = self.active_groups();
        if groups.is_empty() {
            return Ok(false);
        }

        enum PreparedGroup {
            Empty,
            Singleton(usize),
            ToldAlone(Vec<usize>),
            Skipped,
            RefFree { slot: usize },
            RefBearing { candidates: Vec<usize> },
        }

        let mut prepared = Vec::with_capacity(groups.len());
        let mut ref_free_jobs: Vec<(usize, Vec<usize>)> = Vec::new();

        for (group_idx, (key, observed_group)) in groups.iter().enumerate() {
            let observed_slice = observed_group.as_slice();
            match inputs.reference_groups.get(key) {
                None => {
                    prepared.push(PreparedGroup::Empty);
                    continue;
                }
                Some(GroupIndices::One(candidate)) => {
                    if self.is_reference_used(*candidate) {
                        prepared.push(PreparedGroup::Empty);
                        continue;
                    }
                    if observed_slice.len() == 1 {
                        prepared.push(PreparedGroup::Singleton(*candidate));
                        continue;
                    }
                }
                Some(GroupIndices::Many(_)) => {}
            }
            let candidates = inputs
                .reference_groups
                .get(key)
                .map(GroupIndices::as_slice)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| !self.is_reference_used(*candidate))
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                prepared.push(PreparedGroup::Empty);
                continue;
            }
            if observed_slice.len() == 1 && candidates.len() == 1 {
                prepared.push(PreparedGroup::Singleton(candidates[0]));
                continue;
            }
            if inputs.told_alone {
                prepared.push(PreparedGroup::ToldAlone(candidates));
                continue;
            }
            let group_index = observed_slice[0];
            if key.0.is_none()
                && unchanged_content_groups.get(&group_index).is_some_and(
                    |(previous_observed, previous_candidates)| {
                        previous_observed.as_slice() == observed_slice
                            && previous_candidates == &candidates
                    },
                )
            {
                prepared.push(PreparedGroup::Skipped);
                continue;
            }
            let ref_free = observed_slice
                .iter()
                .all(|&index| !inputs.observed_graph.has_edges(index))
                && candidates
                    .iter()
                    .all(|&index| !inputs.reference_graph.has_edges(index));
            if ref_free {
                let slot = ref_free_jobs.len();
                ref_free_jobs.push((group_idx, candidates));
                prepared.push(PreparedGroup::RefFree { slot });
            } else {
                prepared.push(PreparedGroup::RefBearing { candidates });
            }
        }

        let mut ref_free_results: Vec<Option<ScoredProposals>> =
            if let Some(compact) = inputs.compact {
                score_compact_ref_free_jobs(
                    &groups,
                    &ref_free_jobs,
                    inputs,
                    &self.assigned_reference,
                    &self.assigned_observed,
                    compact,
                )?
                .into_iter()
                .map(Some)
                .collect()
            } else {
                let score_context_shared = IdentityScoreContext {
                    reference: RowSource::Full(&reference.instances),
                    observed: RowSource::Full(&observed.instances),
                    reference_graph: inputs.reference_graph,
                    observed_graph: inputs.observed_graph,
                    assigned_reference: &self.assigned_reference,
                    assigned_observed: &self.assigned_observed,
                    subtree_keys: inputs.subtree_keys.map(|exact| &exact.keys),
                };
                let score_job = |&(group_idx, ref candidates): &(usize, Vec<usize>)| {
                    let group_started = Instant::now();
                    let proposals = identity_group_proposals_with_exact(
                        &score_context_shared,
                        groups[group_idx].1.as_slice(),
                        candidates,
                        inputs.subtree_keys,
                    );
                    (proposals, group_started.elapsed())
                };
                let results: Vec<ScoredProposals> =
                    if ref_free_jobs.len() >= 2 && rayon::current_num_threads() > 1 {
                        ref_free_jobs.par_iter().map(score_job).collect()
                    } else {
                        ref_free_jobs.iter().map(score_job).collect()
                    };
                results.into_iter().map(Some).collect()
            };
        let mut ref_free_candidates: Vec<Vec<usize>> =
            ref_free_jobs.into_iter().map(|(_, c)| c).collect();

        let mut ref_bearing_cache = inputs.compact.map(|compact| {
            let ref_bearing_groups: Vec<(usize, &[usize])> = prepared
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| match item {
                    PreparedGroup::RefBearing { candidates } => Some((idx, candidates.as_slice())),
                    _ => None,
                })
                .collect();
            CompactRefBearingCache::plan(&groups, &ref_bearing_groups, inputs, compact)
        });

        let mut progressed = false;
        let mut stalled_indices: Vec<usize> = Vec::new();
        for (group_idx, ((key, observed_group), item)) in
            groups.into_iter().zip(prepared).enumerate()
        {
            match item {
                PreparedGroup::Empty => {
                    for &index in observed_group.as_slice() {
                        self.assign_fresh_and_unblock(index, inputs.observed_tree);
                    }
                    progressed = true;
                }
                PreparedGroup::Singleton(candidate) => {
                    self.assign_and_unblock(
                        observed_group.as_slice()[0],
                        candidate,
                        inputs.observed_tree,
                    );
                    progressed = true;
                }
                PreparedGroup::ToldAlone(candidates) => {
                    let Some(by_order) = &mut self.by_order else {
                        self.compares_values = true;
                        self.flush_frontier();
                        return Ok(progressed);
                    };
                    let observed_slice = observed_group.as_slice();
                    let how = if observed_slice.len() == candidates.len() {
                        ByOrder::Paired
                    } else {
                        ByOrder::BesideMore
                    };
                    by_order.extend(
                        observed_slice
                            .iter()
                            .skip(candidates.len())
                            .map(|index| (*index, ByOrder::More)),
                    );
                    let pairs = observed_slice
                        .iter()
                        .copied()
                        .zip(candidates)
                        .collect::<Vec<_>>();
                    by_order.extend(pairs.iter().map(|(index, _)| (*index, how)));
                    for (index, candidate) in pairs {
                        self.assign_and_unblock(index, candidate, inputs.observed_tree);
                    }
                    progressed = true;
                }
                PreparedGroup::Skipped => {}
                PreparedGroup::RefFree { slot } => {
                    let observed_count = observed_group.len();
                    let candidates = std::mem::take(&mut ref_free_candidates[slot]);
                    let (proposals, elapsed) = ref_free_results[slot].take().unwrap();
                    let assigned = self.apply_unique_proposals(proposals, inputs.observed_tree);
                    progressed |= assigned;
                    log_slow_identity_group(key, observed_count, candidates.len(), elapsed);
                    if !assigned {
                        let observed_vec = match observed_group {
                            GroupIndices::One(i) => vec![i],
                            GroupIndices::Many(v) => v,
                        };
                        if key.0.is_some() {
                            stalled_indices.extend_from_slice(&observed_vec);
                            self.stalled_groups.push((key, observed_vec));
                        } else {
                            let group_index = observed_vec[0];
                            unchanged_content_groups
                                .insert(group_index, (observed_vec, candidates));
                        }
                    }
                }
                PreparedGroup::RefBearing { candidates } => {
                    let group_started = Instant::now();
                    let observed_slice = observed_group.as_slice();
                    let observed_count = observed_slice.len();
                    let proposals = if let Some(compact) = inputs.compact {
                        score_compact_single_group(
                            observed_slice,
                            &candidates,
                            inputs,
                            (&self.assigned_reference, &self.assigned_observed),
                            compact,
                            ref_bearing_cache.as_mut().map(|c| (c, group_idx)),
                        )?
                    } else {
                        let score_context = IdentityScoreContext {
                            reference: RowSource::Full(&reference.instances),
                            observed: RowSource::Full(&observed.instances),
                            reference_graph: inputs.reference_graph,
                            observed_graph: inputs.observed_graph,
                            assigned_reference: &self.assigned_reference,
                            assigned_observed: &self.assigned_observed,
                            subtree_keys: inputs.subtree_keys.map(|exact| &exact.keys),
                        };
                        identity_group_proposals_with_exact(
                            &score_context,
                            observed_slice,
                            &candidates,
                            inputs.subtree_keys,
                        )
                    };
                    progressed |= self.apply_unique_proposals(proposals, inputs.observed_tree);
                    log_slow_identity_group(
                        key,
                        observed_count,
                        candidates.len(),
                        group_started.elapsed(),
                    );
                }
            }
        }
        if !stalled_indices.is_empty() {
            let mut stalled_mask = AHashSet::with_capacity(stalled_indices.len());
            stalled_mask.extend(stalled_indices);
            self.active_ready.retain(|idx| !stalled_mask.contains(idx));
        }
        self.flush_frontier();
        Ok(progressed)
    }

    fn apply_unique_proposals(
        &mut self,
        proposals: Vec<(usize, usize)>,
        observed_tree: &DocTree,
    ) -> bool {
        let mut proposal_counts = AHashMap::<usize, usize>::with_capacity(proposals.len());
        for &(_, candidate) in &proposals {
            *proposal_counts.entry(candidate).or_default() += 1;
        }
        let mut assigned = false;
        for (index, candidate) in proposals {
            if proposal_counts.get(&candidate) != Some(&1) || self.is_reference_used(candidate) {
                continue;
            }
            self.assign_and_unblock(index, candidate, observed_tree);
            assigned = true;
        }
        assigned
    }

    fn run_fallback(
        &mut self,
        reference_groups: &AHashMap<IdentityGroupKey<'a>, GroupIndices>,
        observed_tree: &DocTree,
    ) -> bool {
        let mut progressed = false;
        let shapes = self.shape_keys.take().unwrap_or_else(|| {
            rayon::join(
                || subtree_shape_keys(self.reference),
                || subtree_shape_keys(self.observed),
            )
        });
        let mut groups = self.active_groups();
        if !self.stalled_groups.is_empty() {
            for (key, indices) in self.stalled_groups.drain(..) {
                let group = if indices.len() == 1 {
                    GroupIndices::One(indices[0])
                } else {
                    GroupIndices::Many(indices)
                };
                groups.push((key, group));
            }
            groups.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        }
        for (key, observed_group) in groups {
            let candidates = reference_groups
                .get(&key)
                .map(GroupIndices::as_slice)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| !self.is_reference_used(*candidate))
                .collect::<Vec<_>>();
            let mut by_shape: AHashMap<u64, std::collections::VecDeque<usize>> =
                AHashMap::with_capacity(candidates.len());
            for (position, candidate) in candidates.iter().enumerate() {
                by_shape
                    .entry(shapes.0[*candidate])
                    .or_default()
                    .push_back(position);
            }
            let mut taken = vec![false; candidates.len()];
            let mut unpaired = Vec::new();
            for &index in observed_group.as_slice() {
                match by_shape
                    .get_mut(&shapes.1[index])
                    .and_then(std::collections::VecDeque::pop_front)
                {
                    Some(position) => {
                        taken[position] = true;
                        self.assign_and_unblock(index, candidates[position], observed_tree);
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
                    Some(candidate) => self.assign_and_unblock(index, candidate, observed_tree),
                    None => self.assign_fresh_and_unblock(index, observed_tree),
                }
                progressed = true;
            }
        }
        self.flush_frontier();
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
        observed.has_edges(index)
    }

    /// Whether an observed twin holds and receives the references its
    /// counterpart `at` does, at their places: whatever the matching paired
    /// at their places before, it then weighs that counterpart for the twin
    /// (`matching_reference_evidence`). A reference from an instance of the
    /// run weighs nothing there; one to an instance of the run may.
    fn named_alike(&self, index: usize, at: usize) -> bool {
        let [reference, observed] = self.graphs();
        self.name_alike(reference.outgoing(at), observed.outgoing(index))
            && observed.incoming(index).iter().all(|edge| {
                self.counterpart(edge.source).is_none_or(|source| {
                    reference
                        .incoming(at)
                        .iter()
                        .any(|other| other.source == source && other.slot == edge.slot)
                })
            })
            && reference.incoming(at).iter().all(|edge| {
                self.observed_of(edge.source).is_none_or(|source| {
                    observed
                        .incoming(index)
                        .iter()
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
                AHashMap::new()
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
        let content_index = IdentityCandidateIndex::build(
            RowSource::Full(&self.reference.instances),
            reference_twins,
        );
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
    let any_changed = observed
        .instances
        .iter()
        .zip(&ids)
        .any(|(instance, id)| instance.settings_id != *id);
    if !any_changed {
        return;
    }
    let has_any_ref = if observed.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        observed.instances.par_iter().any(|instance| {
            instance.properties.values().any(|value| {
                matches!(value, Value::Array(_) | Value::Object(_))
                    && reconciliation_value_contains_reference(value)
            }) || instance.attributes.values().any(|value| {
                matches!(value, Value::Array(_) | Value::Object(_))
                    && reconciliation_value_contains_reference(value)
            })
        })
    } else {
        true
    };
    if !has_any_ref {
        for (instance, id) in observed.instances.iter_mut().zip(ids) {
            instance.settings_id = id;
        }
        log_global(
            4,
            format_args!(
                "[renium] identity reference remap: {:.1}ms",
                phase.elapsed().as_secs_f64() * 1000.0
            ),
        );
        return;
    }
    let mut remap = HashMap::with_capacity(observed.instances.len());
    for (instance, id) in observed.instances.iter_mut().zip(ids) {
        let old_id = std::mem::replace(&mut instance.settings_id, id);
        remap.insert(old_id, instance.settings_id.clone());
    }
    let service = store_service(observed).map(str::to_string);
    let apply = |instance: &mut SettingsBytecodeInstance| {
        remap_record_reference_ids(&mut instance.properties, &remap, service.as_deref());
        remap_record_reference_ids(&mut instance.attributes, &remap, service.as_deref());
    };
    if observed.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        observed.instances.par_iter_mut().for_each(apply);
    } else {
        observed.instances.iter_mut().for_each(apply);
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
    match_settings_ids_with_compact(reference, observed, told_alone, in_order, None)
        .expect("in-memory identity matching cannot fail")
}

#[allow(clippy::type_complexity)]
fn match_settings_ids_with_compact(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    told_alone: bool,
    in_order: bool,
    compact: Option<&CompactMatching<'_>>,
) -> Result<Option<(Vec<String>, Vec<Option<usize>>, Vec<(usize, ByOrder)>)>> {
    let preparation_started = Instant::now();
    let cameras = (
        workspace_current_camera_index(reference),
        workspace_current_camera_index(observed),
    );
    let obs_has_non_debug_id = observed
        .instances
        .iter()
        .any(|instance| !instance.settings_id.starts_with("debug:"));
    // A fresh destination has only its retained containers. Index candidates
    // it can actually match rather than allocating a group for every incoming
    // descendant. Full comparisons keep the direct index construction.
    let candidate_filters = (observed.instances.len() < reference.instances.len() / 4).then(|| {
        (
            observed
                .instances
                .iter()
                .map(|instance| instance.settings_id.as_str())
                .collect::<AHashSet<_>>(),
            observed
                .instances
                .iter()
                .map(|instance| (instance.name.as_str(), instance.class_name.as_str()))
                .collect::<AHashSet<_>>(),
        )
    });
    let ((reference_by_id, reference_groups), (reference_by_unique_id, observed_by_unique_id)) =
        rayon::join(
            || {
                let cap = if candidate_filters.is_some() {
                    observed.instances.len()
                } else {
                    reference.instances.len()
                };
                let mut reference_by_id = if obs_has_non_debug_id {
                    AHashMap::with_capacity(cap)
                } else {
                    AHashMap::new()
                };
                let mut reference_groups =
                    AHashMap::<IdentityGroupKey, GroupIndices>::with_capacity(cap / 4);
                for (index, instance) in reference.instances.iter().enumerate() {
                    if obs_has_non_debug_id
                        && candidate_filters
                            .as_ref()
                            .is_none_or(|(ids, _)| ids.contains(instance.settings_id.as_str()))
                    {
                        reference_by_id.insert(instance.settings_id.as_str(), index);
                    }
                    if candidate_filters.as_ref().is_some_and(|(_, names)| {
                        !names.contains(&(instance.name.as_str(), instance.class_name.as_str()))
                    }) {
                        continue;
                    }
                    let key = (
                        instance.parent_index,
                        instance.name.as_str(),
                        instance.class_name.as_str(),
                    );
                    match reference_groups.entry(key) {
                        std::collections::hash_map::Entry::Occupied(mut entry) => {
                            entry.get_mut().push(index);
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(GroupIndices::One(index));
                        }
                    }
                }
                (reference_by_id, reference_groups)
            },
            || {
                rayon::join(
                    || match compact {
                        Some(compact) => {
                            persistent_identity_index_from_slice(compact.ref_unique_ids)
                        }
                        None => persistent_identity_index(reference),
                    },
                    || match compact {
                        Some(compact) => {
                            persistent_identity_index_from_slice(compact.obs_unique_ids)
                        }
                        None => persistent_identity_index(observed),
                    },
                )
            },
        );
    let reference_ids_unique = obs_has_non_debug_id
        && candidate_filters.is_none()
        && reference_by_id.len() == reference.instances.len();
    drop(candidate_filters);
    let lookup = IdentityLookup {
        reference_by_id,
        reference_by_unique_id,
        observed_by_unique_id,
        reference_ids_unique,
    };
    let mut state = IdentityAssignment::new(reference, observed);
    state.by_order = in_order.then(Vec::new);
    let initial_camera_ref = if let (Some(reference_index), Some(observed_index)) = cameras {
        state.assign(observed_index, reference_index);
        Some(reference_index)
    } else {
        None
    };
    state.run_exact(
        &lookup,
        initial_camera_ref,
        compact.map(|compact| compact.obs_unique_ids),
    );
    drop(lookup);
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
        let phase = Instant::now();
        let ((reference_graph, observed_graph), observed_tree) = rayon::join(
            || {
                rayon::join(
                    || build_reference_graph(reference),
                    || build_reference_graph(observed),
                )
            },
            || DocTree::build(observed),
        );
        log_global(
            4,
            format_args!(
                "[renium] identity reference graphs: {:.1}ms",
                phase.elapsed().as_secs_f64() * 1000.0
            ),
        );
        let phase = Instant::now();
        let needs_subtree_keys = !told_alone && {
            let mut is_parent = vec![false; reference.instances.len()];
            for instance in &reference.instances {
                if let Some(parent) = instance.parent_index
                    && parent < is_parent.len()
                {
                    is_parent[parent] = true;
                }
            }
            reference_groups.values().any(|group| {
                group.len() > 1 && group.as_slice().iter().any(|&index| is_parent[index])
            })
        };
        let subtree_keys = needs_subtree_keys.then(|| {
            let ref_required = state
                .assigned_observed
                .iter()
                .map(Option::is_none)
                .collect();
            let obs_required = (0..observed.instances.len())
                .map(|index| !state.is_assigned(index))
                .collect();
            match compact {
                Some(compact) => compute_subtree_keys_compact(
                    reference,
                    compact.ref_meta,
                    ref_required,
                    observed,
                    compact.obs_meta,
                    &observed_tree,
                    obs_required,
                ),
                None => compute_subtree_keys(
                    reference,
                    &reference_graph,
                    ref_required,
                    observed,
                    &observed_graph,
                    &observed_tree,
                    obs_required,
                ),
            }
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
            observed_tree: &observed_tree,
            told_alone,
            compact,
        };
        state.init_frontier();
        let mut unchanged_content_groups = AHashMap::new();
        // `run_exact` paired what ids pair before this: an instance it left
        // has no counterpart by id, or one that is taken for good.
        while state.remaining > 0 {
            let progressed = state.run_scored(&inputs, &mut unchanged_content_groups)?;
            if state.compares_values {
                return Ok(None);
            }
            if progressed || !told_alone && state.run_fallback(&reference_groups, &observed_tree) {
                continue;
            }
            return Ok(None);
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
        assigned_fresh,
        assigned_reference,
        by_order,
        ..
    } = state;
    let mut assigned_fresh = assigned_fresh;
    let ids = assigned_reference
        .iter()
        .enumerate()
        .map(|(index, reference_index)| match reference_index {
            Some(candidate) => reference.instances[*candidate].settings_id.clone(),
            None => assigned_fresh.remove(&index).unwrap(),
        })
        .collect();
    Ok(Some((
        ids,
        assigned_reference,
        by_order.unwrap_or_default(),
    )))
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

/// Canonicalizes property names in place and returns `(index, original_properties)`
/// only for instances whose properties actually changed, so callers can restore
/// `document` to its pre-canonicalized form before encoding.
fn canonicalize_settings_property_names_recording(
    document: &mut SettingsBytecode,
) -> Result<Vec<(usize, Map<String, Value>)>> {
    let chunks = document
        .instances
        .par_chunks_mut(4096)
        .enumerate()
        .map(|(chunk_idx, chunk)| canonicalize_instances_recording(chunk_idx * 4096, chunk))
        .collect::<Result<Vec<_>>>()?;
    Ok(chunks.into_iter().flatten().collect())
}

fn canonicalize_instances_recording(
    base_index: usize,
    instances: &mut [SettingsBytecodeInstance],
) -> Result<Vec<(usize, Map<String, Value>)>> {
    let database = rbx_reflection_database::get()?;
    let mut names_by_class = AHashMap::<&str, AHashMap<String, Option<&str>>>::new();
    let mut migrations_by_class = AHashMap::<&str, AHashMap<String, bool>>::new();
    let mut restored = Vec::new();
    for (offset, instance) in instances.iter_mut().enumerate() {
        let has_uri_alias =
            matches!(
                instance.class_name.as_str(),
                "PartOperation"
                    | "UnionOperation"
                    | "NegateOperation"
                    | "IntersectOperation"
                    | "Shirt"
                    | "Pants"
            ) || crate::roblox::schema::has_protected_texture_pack(&instance.class_name);
        let mut original = if has_uri_alias {
            let saved = instance.properties.clone();
            canonicalize_content_uri_alias(instance)?;
            Some(saved)
        } else {
            None
        };
        let names = names_by_class.entry(&instance.class_name).or_default();
        let mut renamed_property = |name: &str| {
            if let Some(renamed) = names.get(name) {
                return *renamed;
            }
            let renamed = serialized_only_logical_name(database, &instance.class_name, name)
                .or_else(|| rbx_logical_property_name(database, &instance.class_name, name))
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
        let legacy_disabled = instance.properties.contains_key("Disabled")
            && crate::rbx::decode::rbx_reflection_class_is_a(
                database,
                &instance.class_name,
                "BaseScript",
            );
        let needs_rewrite = legacy_disabled
            || instance
                .properties
                .keys()
                .any(|name| renamed_property(name).is_some() || migrated_property(name));
        if needs_rewrite {
            if original.is_none() {
                original = Some(instance.properties.clone());
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
        if let Some(original) = original
            && instance.properties != original
        {
            restored.push((base_index + offset, original));
        }
    }
    Ok(restored)
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

/// Fast self-equality check for identical JSON values when no store-id
/// remapping tables are active (`ids.left.is_empty() && ids.right.is_empty()`).
#[inline]
fn value_trivially_self_equal(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => true,
        Value::Object(map) => match map.get("_type").and_then(Value::as_str) {
            Some(
                "Vector2" | "Vector3" | "Color3" | "UDim" | "UDim2" | "Rect" | "UniqueId"
                | "SharedString" | "BinaryString" | "BrickColor" | "Ref",
            ) => true,
            Some("EnumItem") => map.get("name").and_then(Value::as_str).is_some(),
            Some("CFrame") => {
                map.get("components")
                    .and_then(Value::as_array)
                    .is_some_and(|components| {
                        components.len() == 12 && components.iter().all(Value::is_number)
                    })
            }
            _ => false,
        },
        Value::Array(_) => false,
    }
}

fn reconciliation_maps_equal_with_ids(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ids: PairedStoreIds<'_>,
) -> bool {
    let database = rbx_reflection_database::get().ok();
    let empty_ids = ids.left.is_empty() && ids.right.is_empty();
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
        let other = right.get(name);
        if empty_ids
            && let Some(other) = other
            && value == other
            && value_trivially_self_equal(value)
        {
            continue;
        }
        if !stable(name, value) {
            continue;
        }
        match other {
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
        left.contains_key(name)
            || !stable(name, value)
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

/// Sound one-sided numeric difference check: returns `true` only when two finite numbers
/// exceed both exact equality and the approximate f32 tolerance used by reconciliation.
#[inline]
pub(crate) fn f32_values_definitely_differ(a: f64, b: f64) -> bool {
    a.is_finite()
        && b.is_finite()
        && (a - b).abs() > 4.0 * f64::from(f32::EPSILON) * a.abs().max(b.abs()).max(1.0)
}

/// Returns `true` when two finite numbers match under f32 reconciliation tolerance.
#[inline]
pub(crate) fn f32_values_match(a: f64, b: f64) -> bool {
    a == b
        || (a.is_finite()
            && b.is_finite()
            && (a - b).abs() <= 4.0 * f64::from(f32::EPSILON) * a.abs().max(b.abs()).max(1.0))
}

/// Returns `true` when two finite CFrames match under either `reconciliation_values_equal_with_ids`
/// or `cframes_match_when_rederived`.
#[inline]
pub(crate) fn cframes_match_reconciliation(a: &[f64; 12], b: &[f64; 12]) -> bool {
    if a.iter().chain(b.iter()).any(|v| !v.is_finite()) {
        return false;
    }
    let pos_ok = (0..3)
        .all(|i| f32_values_match(a[i], b[i]) || (a[i] - b[i]).abs() <= CFRAME_POSITION_NOISE);
    if !pos_ok {
        return false;
    }
    let rot_tol = CFRAME_ROTATION_EPSILON.max(CFRAME_ROTATION_NOISE);
    if (3..12).all(|i| (a[i] - b[i]).abs() <= rot_tol) {
        return true;
    }
    if !(0..3).all(|i| (a[i] - b[i]).abs() <= CFRAME_POSITION_NOISE) {
        return false;
    }
    let (Some(rot_a), Some(rot_b)) = (rederived_rotation(a), rederived_rotation(b)) else {
        return false;
    };
    (third_axis_rewritten(a, &rot_a) || third_axis_rewritten(b, &rot_b))
        && rot_a
            .iter()
            .zip(&rot_b)
            .all(|(x, y)| (x - y).abs() <= CFRAME_ROTATION_NOISE)
}

/// Zero-allocation extraction of a cooked `CSGPHS` collision hull's fidelity byte (or v0 BLOCK placeholder)
/// from its base64 text. Only decodes the first 16 base64 characters (12 bytes) for v7+ hulls.
pub(crate) fn hull_fidelity_from_b64_prefix(b64: &str) -> Option<u8> {
    if b64.len() < 16 {
        return None;
    }
    let mut prefix = [0u8; 16];
    let n =
        base64::decode_config_slice(&b64.as_bytes()[..16], base64::STANDARD, &mut prefix).ok()?;
    if n < 12 || &prefix[..6] != b"CSGPHS" {
        return None;
    }
    let version = u32::from_le_bytes([prefix[6], prefix[7], prefix[8], prefix[9]]);
    if version >= 7 {
        return Some(prefix[10]);
    }
    if version == 0 && b64.len() <= 32 {
        let bytes = base64::decode(b64).ok()?;
        if bytes.ends_with(b"BLOCK") {
            return Some(2);
        }
    }
    None
}

/// Sound one-sided CFrame difference check: returns `true` only when two CFrames cannot
/// match under either `reconciliation_values_equal_with_ids` or `cframes_match_when_rederived`.
#[inline]
pub(crate) fn cframes_definitely_differ(a: &[f64; 12], b: &[f64; 12]) -> bool {
    if a.iter().chain(b.iter()).any(|v| !v.is_finite()) {
        return false;
    }
    for i in 0..3 {
        let diff = (a[i] - b[i]).abs();
        let tol = CFRAME_POSITION_NOISE
            .max(4.0 * f64::from(f32::EPSILON) * a[i].abs().max(b[i].abs()).max(1.0));
        if diff > tol {
            return true;
        }
    }
    let rot_tol = CFRAME_ROTATION_EPSILON.max(CFRAME_ROTATION_NOISE);
    let rot_differs = (3..12).any(|i| (a[i] - b[i]).abs() > rot_tol);
    if !rot_differs {
        return false;
    }
    let (Some(rot_a), Some(rot_b)) = (rederived_rotation(a), rederived_rotation(b)) else {
        return true;
    };
    if !(third_axis_rewritten(a, &rot_a) || third_axis_rewritten(b, &rot_b)) {
        return true;
    }
    rot_a
        .iter()
        .zip(&rot_b)
        .any(|(x, y)| (x - y).abs() > CFRAME_ROTATION_NOISE)
}

/// Whether `value` contains any `EnumItem` object anywhere within it.
pub(crate) fn value_contains_enum_item(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("_type").and_then(Value::as_str) == Some("EnumItem")
                || map.values().any(value_contains_enum_item)
        }
        Value::Array(items) => items.iter().any(value_contains_enum_item),
        _ => false,
    }
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

#[cfg(test)]
fn reconciliation_identity_maps_equal(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ignore_transient_properties: bool,
) -> bool {
    reconciliation_identity_maps_equal_hint(left, right, ignore_transient_properties, false, false)
}

#[inline]
fn value_has_large_payload(value: &Value) -> bool {
    match value {
        Value::String(text) => text.len() > 256,
        Value::Object(map) => {
            map.len() == 2
                && map
                    .get("base64")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.len() > 256)
        }
        _ => false,
    }
}

fn reconciliation_identity_maps_equal_hint(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    ignore_transient_properties: bool,
    left_no_refs: bool,
    right_no_refs: bool,
) -> bool {
    let mut left_iter = left.iter().filter(|(name, value)| {
        (!ignore_transient_properties || property_name_is_stable_no_ref(name, value))
            && (left_no_refs || !reconciliation_value_contains_reference(value))
    });
    let mut right_iter = right.iter().filter(|(name, value)| {
        (!ignore_transient_properties || property_name_is_stable_no_ref(name, value))
            && (right_no_refs || !reconciliation_value_contains_reference(value))
    });
    let mut deferred: [(&Value, &Value); 4] = [(&Value::Null, &Value::Null); 4];
    let mut deferred_len = 0usize;
    loop {
        match (left_iter.next(), right_iter.next()) {
            (Some((lk, lv)), Some((rk, rv))) => {
                if lk != rk {
                    return false;
                }
                if deferred_len < deferred.len()
                    && (value_has_large_payload(lv) || value_has_large_payload(rv))
                {
                    deferred[deferred_len] = (lv, rv);
                    deferred_len += 1;
                } else if !reconciliation_values_equal(lv, rv, false) {
                    return false;
                }
            }
            (None, None) => break,
            _ => return false,
        }
    }
    for &(lv, rv) in &deferred[..deferred_len] {
        if !reconciliation_values_equal(lv, rv, false) {
            return false;
        }
    }
    true
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
        Value::Array(values) => values.iter().any(|value| {
            matches!(value, Value::Array(_) | Value::Object(_))
                && reconciliation_value_contains_reference(value)
        }),
        Value::Object(object) => object.iter().any(|(key, value)| match key.as_str() {
            "_type" => {
                value.as_str() == Some("Ref")
                    || (matches!(value, Value::Array(_) | Value::Object(_))
                        && reconciliation_value_contains_reference(value))
            }
            "settingsId" | "instanceId" | "instanceIndex" => true,
            _ => {
                matches!(value, Value::Array(_) | Value::Object(_))
                    && reconciliation_value_contains_reference(value)
            }
        }),
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
    if object.len() == 1 {
        for type_name in ["NumberSequence", "ColorSequence"] {
            if let Some(sequence) = object.get(type_name) {
                let normalized =
                    crate::editor::review::normalize_editor_sequence_value(sequence, type_name);
                return (normalized != *value).then_some(normalized);
            }
        }
        if let Some(reference) = object.get("Ref").and_then(Value::as_object) {
            let mut out = reference.clone();
            out.insert("_type".to_string(), Value::String("Ref".to_string()));
            return Some(Value::Object(out));
        }
        if let Some(number) = object.get("BrickColor")
            && number.is_number()
        {
            return Some(serde_json::json!({"_type": "BrickColor", "number": number}));
        }
        return None;
    }
    if let Some(type_name @ ("NumberSequence" | "ColorSequence")) =
        object.get("_type").and_then(Value::as_str)
    {
        let normalized = crate::editor::review::normalize_editor_sequence_value(value, type_name);
        return (normalized != *value).then_some(normalized);
    }
    None
}

#[inline]
fn reconciliation_numbers_equal(
    left: &serde_json::Number,
    right: &serde_json::Number,
    approximate: bool,
) -> bool {
    if !approximate {
        return exact_json_numbers_equal(left, right);
    }
    let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) else {
        return left == right;
    };
    left == right
        || left.is_finite()
            && right.is_finite()
            && (left - right).abs()
                <= 4.0 * f64::from(f32::EPSILON) * left.abs().max(right.abs()).max(1.0)
}

fn reconciliation_values_equal_with_ids(
    left: &Value,
    right: &Value,
    approximate: bool,
    ids: Option<PairedStoreIds<'_>>,
) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => return true,
        (Value::Bool(left), Value::Bool(right)) => return left == right,
        (Value::String(left), Value::String(right)) => return left == right,
        (Value::Number(left), Value::Number(right)) => {
            return reconciliation_numbers_equal(left, right, approximate);
        }
        (Value::Object(lo), Value::Object(ro))
            if lo.len() == 2
                && ro.len() == 2
                && let (Some(lid), Some(rid)) = (
                    crate::settings::held::handle_id(lo),
                    crate::settings::held::handle_id(ro),
                )
                && lid == rid =>
        {
            return true;
        }
        _ => {}
    }
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
        (Value::Number(left), Value::Number(right)) => {
            reconciliation_numbers_equal(left, right, approximate)
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
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|((name, value), (other_name, other))| {
                        if name != other_name {
                            return false;
                        }
                        if type_name == Some("CFrame") && name == "components" {
                            let (Some(left), Some(right)) = (value.as_array(), other.as_array())
                            else {
                                return false;
                            };
                            return left.len() == 12
                                && right.len() == 12
                                && left.iter().zip(right).enumerate().all(
                                    |(index, (left, right))| {
                                        let (Some(a), Some(b)) = (left.as_f64(), right.as_f64())
                                        else {
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
                                    },
                                );
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
        let index =
            IdentityCandidateIndex::build(RowSource::Full(&reference.instances), &candidates)
                .unwrap();
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
                    RowSource::Full(&reference.instances),
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
                        reference: RowSource::Full(&reference.instances),
                        observed: RowSource::Full(&observed.instances),
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
        let (representatives, multiplicities) = identity_candidate_representatives(
            RowSource::Full(&reference.instances),
            &graph,
            None,
            &candidates,
        );
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
        let index =
            IdentityCandidateIndex::build(RowSource::Full(&reference.instances), &candidates)
                .unwrap();
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
        let index =
            IdentityCandidateIndex::build(RowSource::Full(&reference.instances), &candidates)
                .unwrap();
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
