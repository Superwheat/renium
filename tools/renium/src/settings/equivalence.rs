use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock, PoisonError};
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
    SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode, encode_settings_bytecode,
    encode_settings_bytecode_with_dense_references, is_known_default_property_value,
    is_reference_object,
};
use crate::app::timing::{log_timing, verbose_timing_logs};
use crate::rbx::decode::rbx_variant_to_settings_json;
use crate::rbx::encode::{rbx_logical_property_name, rbx_model_property_descriptor};
use crate::rbx::model::BytecodeModelImportRefs;
use crate::snapshot::refs::{
    remap_and_stabilize_record_references, remap_record_reference_ids, stabilize_record_references,
};

pub(crate) enum SettingsAlignment {
    Equivalent,
    Changed(Vec<u8>),
}

const SETTINGS_ALIGNMENT_CACHE_CAPACITY: usize = 64;
const SETTINGS_ALIGNMENT_CACHE_MIN_BYTES: usize = 64 * 1024;
const SETTINGS_PARALLEL_DECODE_MIN_BYTES: usize = 64 * 1024;

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
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entries
        .contains(&key)
}

fn cache_settings_alignment(key: SettingsAlignmentCacheKey) {
    let mut cache = SETTINGS_ALIGNMENT_CACHE
        .get_or_init(|| Mutex::new(SettingsAlignmentCache::default()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
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
    if reference_bytes == observed_bytes {
        return Ok(SettingsAlignment::Equivalent);
    }
    let cache_started = Instant::now();
    let cache_key = settings_alignment_cache_key(reference_bytes, observed_bytes);
    if cache_key.is_some_and(settings_alignment_is_cached) {
        log_timing("settings alignment cache hit", cache_started);
        return Ok(SettingsAlignment::Equivalent);
    }
    let decode_started = Instant::now();
    let (reference, observed) = if reference_bytes.len().saturating_add(observed_bytes.len())
        >= SETTINGS_PARALLEL_DECODE_MIN_BYTES
    {
        rayon::join(
            || decode_settings_bytecode(reference_bytes),
            || decode_settings_bytecode(observed_bytes),
        )
    } else {
        (
            decode_settings_bytecode(reference_bytes),
            decode_settings_bytecode(observed_bytes),
        )
    };
    let mut reference = reference?;
    let mut observed = observed?;
    log_timing("settings alignment decode", decode_started);
    let positional_started = Instant::now();
    let positionally_equivalent = settings_documents_positionally_equivalent(&reference, &observed);
    log_timing(
        "settings alignment positional comparison",
        positional_started,
    );
    if positionally_equivalent {
        if let Some(key) = cache_key {
            cache_settings_alignment(key);
        }
        drop_settings_documents(reference, observed);
        return Ok(SettingsAlignment::Equivalent);
    }
    let structure_started = Instant::now();
    match align_settings_ids_for_contiguous_structural_change(&mut reference, &mut observed) {
        ContiguousStructuralAlignment::Aligned => {
            log_timing("settings alignment contiguous structure", structure_started);
            let encode_started = Instant::now();
            let aligned = encode_settings_bytecode_with_dense_references(&observed)?;
            log_timing("settings alignment encode", encode_started);
            drop_settings_documents(reference, observed);
            return Ok(SettingsAlignment::Changed(aligned));
        }
        ContiguousStructuralAlignment::PreparedMismatch => {}
        ContiguousStructuralAlignment::NotApplicable => {
            let stabilize_started = Instant::now();
            rayon::join(
                || stabilize_settings_reference_ids(&mut reference),
                || stabilize_settings_reference_ids(&mut observed),
            );
            log_timing("settings alignment stabilize references", stabilize_started);
        }
    }
    let identity_started = Instant::now();
    if !align_settings_ids_to_reference(&reference, &mut observed) {
        bail!("duplicate instance identity is ambiguous after comparing references");
    }
    log_timing("settings alignment identity", identity_started);
    let encode_started = Instant::now();
    let aligned = encode_settings_bytecode(&observed)?;
    log_timing("settings alignment encode", encode_started);
    let canonicalize_started = Instant::now();
    canonicalize_settings_property_names(&mut reference)?;
    canonicalize_settings_property_names(&mut observed)?;
    log_timing("settings alignment canonicalize", canonicalize_started);
    let compare_started = Instant::now();
    let result = if settings_documents_equivalent(&reference, &observed) {
        if let Some(key) = cache_key {
            cache_settings_alignment(key);
        }
        SettingsAlignment::Equivalent
    } else {
        SettingsAlignment::Changed(aligned)
    };
    log_timing("settings alignment compare", compare_started);
    drop_settings_documents(reference, observed);
    Ok(result)
}

pub(crate) fn drop_settings_document(document: SettingsBytecode) {
    let started = Instant::now();
    let SettingsBytecode { instances, .. } = document;
    if instances.len() >= 8_192 && rayon::current_num_threads() > 1 {
        instances.into_par_iter().for_each(drop);
    } else {
        drop(instances);
    }
    log_timing("settings document release", started);
}

pub(crate) fn drop_settings_documents(left: SettingsBytecode, right: SettingsBytecode) {
    let started = Instant::now();
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
    log_timing("settings document release", started);
}

pub(crate) fn stabilize_settings_reference_ids(document: &mut SettingsBytecode) {
    stabilize_settings_reference_ids_with_remap(document, None, false);
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
    let stabilize = |instance: &mut SettingsBytecodeInstance| {
        if keep_instance_indices || remap.is_some() {
            let remap = remap.unwrap_or(&empty_remap);
            remap_and_stabilize_record_references(
                &mut instance.properties,
                &ids,
                remap,
                keep_instance_indices,
            );
            remap_and_stabilize_record_references(
                &mut instance.attributes,
                &ids,
                remap,
                keep_instance_indices,
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
    let mut outgoing = (0..document.instances.len())
        .map(|_| Vec::new())
        .collect::<Vec<_>>();
    for (source, instance) in document.instances.iter().enumerate() {
        collect_map_references(
            &instance.properties,
            "properties",
            &by_id,
            &mut outgoing[source],
        );
        collect_map_references(
            &instance.attributes,
            "attributes",
            &by_id,
            &mut outgoing[source],
        );
    }
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

// Intern exact content, not hashes used as identity. Children are an unordered multiset;
// references remain separate, stronger evidence in identity_score. A non-match here
// never excludes a candidate (float rounding or an actual edit may change its key).
fn identity_subtree_keys<'a>(
    document: &'a SettingsBytecode,
    keys: &mut AHashMap<IdentitySubtreeKey<'a>, usize>,
) -> Vec<usize> {
    let mut remaining = vec![0; document.instances.len()];
    for instance in &document.instances {
        if let Some(parent) = instance.parent_index {
            remaining[parent] += 1;
        }
    }
    let mut ready = remaining
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect::<Vec<_>>();
    let mut children = vec![Vec::new(); document.instances.len()];
    let mut result = vec![0; document.instances.len()];
    while let Some(index) = ready.pop() {
        let instance = &document.instances[index];
        let mut child_keys = std::mem::take(&mut children[index]);
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
        if let Some(parent) = instance.parent_index {
            children[parent].push(id);
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
    let content_matches = reconciliation_identity_maps_equal(
        &reference_instance.properties,
        &observed_instance.properties,
        true,
    ) && reconciliation_identity_maps_equal(
        &reference_instance.attributes,
        &observed_instance.attributes,
        false,
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

pub(crate) fn align_settings_ids_to_reference(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> bool {
    align_settings_ids_to_reference_impl(reference, observed)
}

fn settings_topology_matches(reference: &SettingsBytecode, observed: &SettingsBytecode) -> bool {
    reference.instances.len() == observed.instances.len()
        && reference
            .instances
            .iter()
            .zip(&observed.instances)
            .all(|(reference, observed)| {
                reference.name == observed.name
                    && reference.class_name == observed.class_name
                    && reference.parent_index == observed.parent_index
            })
}

struct ExactIdentityPass<'a> {
    reference: &'a SettingsBytecode,
    observed: &'a SettingsBytecode,
    reference_by_id: &'a HashMap<&'a str, usize>,
    assigned_ids: &'a mut [Option<String>],
    assigned_reference: &'a mut [Option<usize>],
    assigned_observed: &'a mut [Option<usize>],
    used_reference: &'a mut [bool],
    used_ids: &'a mut HashSet<String>,
    remaining: &'a mut usize,
}

impl ExactIdentityPass<'_> {
    fn run(&mut self) -> bool {
        let mut progressed = false;
        for (index, instance) in self.observed.instances.iter().enumerate() {
            if self.assigned_ids[index].is_some() {
                continue;
            }
            let Some(candidate) = self
                .reference_by_id
                .get(instance.settings_id.as_str())
                .copied()
                .filter(|candidate| {
                    !instance.settings_id.starts_with("debug:")
                        && !self.used_reference[*candidate]
                        && !self
                            .used_ids
                            .contains(&self.reference.instances[*candidate].settings_id)
                })
            else {
                continue;
            };
            let id = self.reference.instances[candidate].settings_id.clone();
            self.used_reference[candidate] = true;
            self.used_ids.insert(id.clone());
            self.assigned_reference[index] = Some(candidate);
            self.assigned_observed[candidate] = Some(index);
            self.assigned_ids[index] = Some(id);
            *self.remaining -= 1;
            progressed = true;
        }
        progressed
    }
}

fn next_alignment_id(
    preferred: &str,
    reserved_reference_ids: &HashSet<String>,
    blocked_ids: &mut HashSet<String>,
    used_ids: &mut HashSet<String>,
    generated: &mut usize,
) -> String {
    if !reserved_reference_ids.contains(preferred) && used_ids.insert(preferred.to_string()) {
        return preferred.to_string();
    }
    loop {
        let candidate = format!("reconcile:{:x}", *generated);
        *generated += 1;
        if blocked_ids.insert(candidate.clone()) && used_ids.insert(candidate.clone()) {
            return candidate;
        }
    }
}

struct FallbackIdentityPass<'a> {
    reference: &'a SettingsBytecode,
    observed: &'a SettingsBytecode,
    reference_groups: &'a HashMap<(Option<usize>, &'a str, &'a str), Vec<usize>>,
    old_ids: &'a [String],
    reserved_reference_ids: &'a HashSet<String>,
    blocked_ids: &'a mut HashSet<String>,
    assigned_ids: &'a mut [Option<String>],
    assigned_reference: &'a mut [Option<usize>],
    assigned_observed: &'a mut [Option<usize>],
    used_reference: &'a mut [bool],
    used_ids: &'a mut HashSet<String>,
    generated: &'a mut usize,
    remaining: &'a mut usize,
}

struct ScoredIdentityPass<'a> {
    reference: &'a SettingsBytecode,
    observed: &'a SettingsBytecode,
    reference_groups: &'a HashMap<(Option<usize>, &'a str, &'a str), Vec<usize>>,
    reference_graph: &'a ReferenceGraph,
    observed_graph: &'a ReferenceGraph,
    subtree_keys: Option<&'a (Vec<usize>, Vec<usize>)>,
    old_ids: &'a [String],
    reserved_reference_ids: &'a HashSet<String>,
    blocked_ids: &'a mut HashSet<String>,
    assigned_ids: &'a mut [Option<String>],
    assigned_reference: &'a mut [Option<usize>],
    assigned_observed: &'a mut [Option<usize>],
    used_reference: &'a mut [bool],
    used_ids: &'a mut HashSet<String>,
    generated: &'a mut usize,
    remaining: &'a mut usize,
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
        let radius = (epsilon + 4.0 * f64::EPSILON) / (1.0 - epsilon) * value.abs().max(1.0);
        let start = self
            .values
            .partition_point(|(candidate, _)| *candidate < value - radius);
        let end = self
            .values
            .partition_point(|(candidate, _)| *candidate <= value + radius);
        Some(&self.values[start..end])
    }
}

impl ScoredIdentityPass<'_> {
    fn run(&mut self) -> bool {
        let mut observed_groups = HashMap::<(Option<usize>, &str, &str), Vec<usize>>::new();
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
            observed_groups
                .entry((
                    reference_parent,
                    instance.name.as_str(),
                    instance.class_name.as_str(),
                ))
                .or_default()
                .push(index);
        }

        let mut progressed = false;
        for (key, observed_group) in observed_groups {
            let candidates = self
                .reference_groups
                .get(&key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| !self.used_reference[*candidate])
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                for index in observed_group {
                    self.assigned_ids[index] = Some(next_alignment_id(
                        &self.old_ids[index],
                        self.reserved_reference_ids,
                        self.blocked_ids,
                        self.used_ids,
                        self.generated,
                    ));
                    *self.remaining -= 1;
                    progressed = true;
                }
                continue;
            }
            if observed_group.len() == 1 && candidates.len() == 1 {
                self.assign_reference(observed_group[0], candidates[0]);
                progressed = true;
                continue;
            }

            let score_context = IdentityScoreContext {
                reference: self.reference,
                observed: self.observed,
                reference_graph: self.reference_graph,
                observed_graph: self.observed_graph,
                assigned_reference: self.assigned_reference,
                assigned_observed: self.assigned_observed,
                subtree_keys: self.subtree_keys,
            };
            let mut proposals = Vec::with_capacity(observed_group.len());
            let mut proposal_counts = HashMap::<usize, usize>::new();
            // Identical unreferenced candidates all receive the same score. Preserve the
            // existing tie/fallback decision without evaluating every identical pair.
            let representative = &self.reference.instances[candidates[0]];
            let uniform = candidates.iter().all(|candidate| {
                self.reference_graph.outgoing[*candidate].is_empty()
                    && self.reference_graph.incoming[*candidate].is_empty()
                    && self.reference.instances[*candidate].properties == representative.properties
                    && self.reference.instances[*candidate].attributes == representative.attributes
            });
            let mut uniform_subtrees = AHashMap::<usize, Vec<usize>>::new();
            if uniform && let Some((reference, _)) = self.subtree_keys {
                for candidate in &candidates {
                    uniform_subtrees
                        .entry(reference[*candidate])
                        .or_default()
                        .push(*candidate);
                }
            }
            let content_index = if !uniform
                && observed_group.iter().any(|index| {
                    self.observed_graph.outgoing[*index].is_empty()
                        && self.observed_graph.incoming[*index].is_empty()
                }) {
                IdentityCandidateIndex::build(self.reference, &candidates)
            } else {
                None
            };
            for index in observed_group {
                let candidates = self
                    .subtree_keys
                    .and_then(|(_, observed)| uniform_subtrees.get(&observed[index]))
                    .map_or(candidates.as_slice(), Vec::as_slice);
                let mut best = None;
                let mut best_candidate = None;
                let mut tied = false;
                let narrowed = content_index
                    .as_ref()
                    .filter(|_| {
                        self.observed_graph.outgoing[index].is_empty()
                            && self.observed_graph.incoming[index].is_empty()
                    })
                    .and_then(|lookup| lookup.matching(&self.observed.instances[index]));
                let count = if uniform {
                    1
                } else {
                    narrowed.map_or(candidates.len(), <[_]>::len)
                };
                for offset in 0..count {
                    let candidate =
                        narrowed.map_or_else(|| candidates[offset], |values| values[offset].1);
                    let Some(score) = identity_score(&score_context, candidate, index) else {
                        continue;
                    };
                    match best {
                        None => {
                            best = Some(score);
                            best_candidate = Some(candidate);
                            tied = false;
                        }
                        Some(current) if score > current => {
                            best = Some(score);
                            best_candidate = Some(candidate);
                            tied = false;
                        }
                        Some(current) if score == current => tied = true,
                        _ => {}
                    }
                }
                tied |= uniform && candidates.len() > 1;
                if !tied && let Some(candidate) = best_candidate {
                    proposals.push((index, candidate));
                    *proposal_counts.entry(candidate).or_default() += 1;
                }
            }
            for (index, candidate) in proposals {
                if proposal_counts.get(&candidate) != Some(&1) || self.used_reference[candidate] {
                    continue;
                }
                self.assign_reference(index, candidate);
                progressed = true;
            }
        }
        progressed
    }

    fn assign_reference(&mut self, index: usize, candidate: usize) {
        let id = self.reference.instances[candidate].settings_id.clone();
        self.used_reference[candidate] = true;
        self.used_ids.insert(id.clone());
        self.assigned_reference[index] = Some(candidate);
        self.assigned_observed[candidate] = Some(index);
        self.assigned_ids[index] = Some(id);
        *self.remaining -= 1;
    }
}

impl FallbackIdentityPass<'_> {
    fn run(&mut self) -> bool {
        let mut groups = BTreeMap::<(Option<usize>, &str, &str), Vec<usize>>::new();
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
        let mut progressed = false;
        for (key, observed_group) in groups {
            let candidates = self
                .reference_groups
                .get(&key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|candidate| !self.used_reference[*candidate])
                .collect::<Vec<_>>();
            let paired = observed_group.len().min(candidates.len());
            for offset in 0..paired {
                let index = observed_group[offset];
                let candidate = candidates[offset];
                let id = self.reference.instances[candidate].settings_id.clone();
                self.used_reference[candidate] = true;
                self.used_ids.insert(id.clone());
                self.assigned_reference[index] = Some(candidate);
                self.assigned_observed[candidate] = Some(index);
                self.assigned_ids[index] = Some(id);
                *self.remaining -= 1;
                progressed = true;
            }
            for index in observed_group.into_iter().skip(paired) {
                self.assigned_ids[index] = Some(next_alignment_id(
                    &self.old_ids[index],
                    self.reserved_reference_ids,
                    self.blocked_ids,
                    self.used_ids,
                    self.generated,
                ));
                *self.remaining -= 1;
                progressed = true;
            }
        }
        progressed
    }
}

fn align_settings_ids_to_reference_impl(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) -> bool {
    let positional_topology_matches = settings_topology_matches(reference, observed);
    if positional_topology_matches && positional_identity_preserved(reference, observed) {
        remap_positional_ids(reference, observed);
        return true;
    }

    let reference_by_id = reference
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut reference_groups = HashMap::<(Option<usize>, &str, &str), Vec<usize>>::new();
    for (index, instance) in reference.instances.iter().enumerate() {
        reference_groups
            .entry((
                instance.parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            ))
            .or_default()
            .push(index);
    }
    let old_ids = observed
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    let mut blocked_ids = observed
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let reserved_reference_ids = reference
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    blocked_ids.extend(reserved_reference_ids.iter().cloned());
    let mut assigned_ids = vec![None; observed.instances.len()];
    let mut assigned_reference = vec![None; observed.instances.len()];
    let mut assigned_observed = vec![None; reference.instances.len()];
    let mut used_reference = vec![false; reference.instances.len()];
    let mut used_ids = HashSet::new();
    let mut generated = 0usize;

    let reference_graph = build_reference_graph(reference);
    let observed_graph = build_reference_graph(observed);
    let parents = reference
        .instances
        .iter()
        .filter_map(|instance| instance.parent_index)
        .collect::<AHashSet<_>>();
    let subtree_keys = reference_groups
        .values()
        .any(|group| group.len() > 1 && group.iter().any(|index| parents.contains(index)))
        .then(|| {
            let mut keys = AHashMap::new();
            (
                identity_subtree_keys(reference, &mut keys),
                identity_subtree_keys(observed, &mut keys),
            )
        });
    let mut remaining = assigned_ids.len();
    while remaining > 0 {
        let mut progressed = ExactIdentityPass {
            reference,
            observed,
            reference_by_id: &reference_by_id,
            assigned_ids: &mut assigned_ids,
            assigned_reference: &mut assigned_reference,
            assigned_observed: &mut assigned_observed,
            used_reference: &mut used_reference,
            used_ids: &mut used_ids,
            remaining: &mut remaining,
        }
        .run();

        progressed |= ScoredIdentityPass {
            reference,
            observed,
            reference_groups: &reference_groups,
            reference_graph: &reference_graph,
            observed_graph: &observed_graph,
            subtree_keys: subtree_keys.as_ref(),
            old_ids: &old_ids,
            reserved_reference_ids: &reserved_reference_ids,
            blocked_ids: &mut blocked_ids,
            assigned_ids: &mut assigned_ids,
            assigned_reference: &mut assigned_reference,
            assigned_observed: &mut assigned_observed,
            used_reference: &mut used_reference,
            used_ids: &mut used_ids,
            generated: &mut generated,
            remaining: &mut remaining,
        }
        .run();
        if progressed {
            continue;
        }
        progressed = FallbackIdentityPass {
            reference,
            observed,
            reference_groups: &reference_groups,
            old_ids: &old_ids,
            reserved_reference_ids: &reserved_reference_ids,
            blocked_ids: &mut blocked_ids,
            assigned_ids: &mut assigned_ids,
            assigned_reference: &mut assigned_reference,
            assigned_observed: &mut assigned_observed,
            used_reference: &mut used_reference,
            used_ids: &mut used_ids,
            generated: &mut generated,
            remaining: &mut remaining,
        }
        .run();
        if progressed {
            continue;
        }
        return false;
    }
    let assigned = assigned_ids
        .into_iter()
        .map(Option::unwrap)
        .collect::<Vec<_>>();
    let remap = old_ids
        .iter()
        .cloned()
        .zip(assigned.iter().cloned())
        .collect::<HashMap<_, _>>();
    for (instance, id) in observed.instances.iter_mut().zip(assigned) {
        instance.settings_id = id;
        remap_record_reference_ids(&mut instance.properties, &remap);
        remap_record_reference_ids(&mut instance.attributes, &remap);
    }
    true
}

fn remap_positional_ids(reference: &SettingsBytecode, observed: &mut SettingsBytecode) {
    let remap = observed
        .instances
        .iter()
        .zip(&reference.instances)
        .map(|(observed, reference)| (observed.settings_id.clone(), reference.settings_id.clone()))
        .collect::<HashMap<_, _>>();
    for (observed, reference) in observed.instances.iter_mut().zip(&reference.instances) {
        observed.settings_id.clone_from(&reference.settings_id);
        remap_record_reference_ids(&mut observed.properties, &remap);
        remap_record_reference_ids(&mut observed.attributes, &remap);
    }
}

pub(crate) fn settings_documents_positionally_equivalent(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> bool {
    let structure_started = Instant::now();
    if !settings_topology_matches(reference, observed) {
        return false;
    }
    log_timing(
        "settings positional structure comparison",
        structure_started,
    );
    let values_started = Instant::now();
    let equivalent = positional_documents_equivalent(reference, observed);
    log_timing("settings positional value comparison", values_started);
    equivalent
}

fn positional_documents_equivalent(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> bool {
    positional_values_equivalent(reference, observed, None)
}

fn positional_identity_preserved(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> bool {
    let mut first_by_key = AHashMap::with_capacity(reference.instances.len());
    let mut ambiguous = vec![false; reference.instances.len()];
    for (index, instance) in reference.instances.iter().enumerate() {
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
    for (index, instance) in reference.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index {
            if parent >= index {
                return false;
            }
            ambiguous[index] |= ambiguous[parent];
        }
    }
    positional_values_equivalent(reference, observed, Some(&ambiguous))
}

fn positional_values_equivalent(
    reference: &SettingsBytecode,
    observed: &SettingsBytecode,
    identity_checks: Option<&[bool]>,
) -> bool {
    let ids_started = Instant::now();
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
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    log_timing("settings positional identity maps", ids_started);
    let equivalent = |(index, (reference_instance, observed_instance)): (
        usize,
        (&SettingsBytecodeInstance, &SettingsBytecodeInstance),
    )| {
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
            || is_reconciliation_protected_workspace_camera(reference, index)
            || reconciliation_maps_equal_with_ids(
                &reference_instance.class_name,
                &reference_instance.properties,
                &observed_instance.properties,
                &reference_ids,
                &observed_ids,
            ) && reconciliation_values_map_equal_with_ids(
                &reference_instance.attributes,
                &observed_instance.attributes,
                &reference_ids,
                &observed_ids,
            )
    };
    if reference.instances.len() >= 2_048 && rayon::current_num_threads() > 1 {
        reference
            .instances
            .par_iter()
            .zip(observed.instances.par_iter())
            .enumerate()
            .all(equivalent)
    } else {
        reference
            .instances
            .iter()
            .zip(&observed.instances)
            .enumerate()
            .all(equivalent)
    }
}

enum ContiguousStructuralAlignment {
    NotApplicable,
    PreparedMismatch,
    Aligned,
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
    let topology_started = Instant::now();
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
    log_timing("settings alignment contiguous topology", topology_started);
    if !topology_matches {
        if verbose_timing_logs() {
            println!(
                "[renium] contiguous settings alignment rejected: topology differs at or after index {changed_at}"
            );
        }
        return ContiguousStructuralAlignment::NotApplicable;
    }

    let reserved_started = Instant::now();
    let reserved_ids = reference
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<AHashSet<_>>();
    if reserved_ids.len() != reference.instances.len() {
        return ContiguousStructuralAlignment::NotApplicable;
    }
    log_timing(
        "settings alignment contiguous reserved ids",
        reserved_started,
    );

    let desired_started = Instant::now();
    let mut inserted_replacements =
        contiguous_inserted_id_replacements(observed, index_map, &reserved_ids);
    log_timing("settings alignment contiguous desired ids", desired_started);

    let reference_ids_started = Instant::now();
    let text_reference_ids = document_text_reference_ids(observed);
    log_timing(
        "settings alignment contiguous reference ids",
        reference_ids_started,
    );

    let remap_started = Instant::now();
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
    log_timing("settings alignment contiguous remap", remap_started);

    let apply_started = Instant::now();
    for (index, instance) in observed.instances.iter_mut().enumerate() {
        if let Some(reference_index) = index_map.observed_to_reference(index) {
            instance
                .settings_id
                .clone_from(&reference.instances[reference_index].settings_id);
        } else if let Some(replacement) = inserted_replacements[index - changed_at].take() {
            instance.settings_id = replacement;
        }
    }
    log_timing("settings alignment contiguous apply ids", apply_started);
    let stabilize_started = Instant::now();
    rayon::join(
        || stabilize_settings_reference_ids_with_remap(reference, None, true),
        || stabilize_settings_reference_ids_with_remap(observed, Some(&remap), true),
    );
    log_timing("settings alignment stabilize references", stabilize_started);

    let maps_started = Instant::now();
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
    log_timing("settings alignment contiguous identity maps", maps_started);
    let equivalent = |position: usize| {
        let (reference_index, observed_index) = index_map.pair_at(position);
        let reference = &reference.instances[reference_index];
        let observed = &observed.instances[observed_index];
        reference.class_name == "PackageLink"
            || reconciliation_maps_equal_with_ids(
                &reference.class_name,
                &reference.properties,
                &observed.properties,
                &reference_ids,
                &observed_ids,
            ) && reconciliation_values_map_equal_with_ids(
                &reference.attributes,
                &observed.attributes,
                &reference_ids,
                &observed_ids,
            )
    };
    let common_started = Instant::now();
    let common_matches = if common_len >= 2_048 && rayon::current_num_threads() > 1 {
        (0..common_len).into_par_iter().all(equivalent)
    } else {
        (0..common_len).all(equivalent)
    };
    log_timing(
        "settings alignment contiguous common content",
        common_started,
    );
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

    ContiguousStructuralAlignment::Aligned
}

fn settings_parent_id(document: &SettingsBytecode, index: usize) -> Option<&str> {
    document.instances[index]
        .parent_index
        .and_then(|parent| document.instances.get(parent))
        .map(|parent| parent.settings_id.as_str())
}

pub(crate) fn canonicalize_settings_property_names(document: &mut SettingsBytecode) -> Result<()> {
    let database = rbx_reflection_database::get()?;
    let mut names_by_class = AHashMap::<&str, AHashMap<String, Option<&str>>>::new();
    for instance in &mut document.instances {
        let names = names_by_class.entry(&instance.class_name).or_default();
        let mut renamed_property = |name: &str| {
            if let Some(renamed) = names.get(name) {
                return *renamed;
            }
            let renamed = rbx_logical_property_name(database, &instance.class_name, name)
                .filter(|canonical| *canonical != name);
            names.insert(name.to_string(), renamed);
            renamed
        };
        if !instance
            .properties
            .keys()
            .any(|name| renamed_property(name).is_some())
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
        instance.properties = canonical;
    }
    Ok(())
}

pub(crate) fn settings_documents_equivalent(
    left: &SettingsBytecode,
    right: &SettingsBytecode,
) -> bool {
    if left.instances.len() != right.instances.len() {
        return false;
    }
    let left_ids = left
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let right_by_id = right
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    if left_ids.len() != left.instances.len() || right_by_id.len() != right.instances.len() {
        return false;
    }
    left.instances
        .iter()
        .enumerate()
        .all(|(left_index, left_instance)| {
            let Some(right_index) = right_by_id.get(left_instance.settings_id.as_str()).copied()
            else {
                return false;
            };
            let right_instance = &right.instances[right_index];
            left_instance.name == right_instance.name
                && left_instance.class_name == right_instance.class_name
                && settings_parent_id(left, left_index) == settings_parent_id(right, right_index)
                && (left_instance.class_name == "PackageLink"
                    || is_reconciliation_protected_workspace_camera(left, left_index)
                    || reconciliation_maps_equal(
                        &left_instance.class_name,
                        &left_instance.properties,
                        &right_instance.properties,
                    ) && reconciliation_values_map_equal(
                        &left_instance.attributes,
                        &right_instance.attributes,
                    ))
        })
}

pub(crate) fn is_reconciliation_protected_workspace_camera(
    document: &SettingsBytecode,
    index: usize,
) -> bool {
    let instance = &document.instances[index];
    if instance.class_name != "Camera"
        || !matches!(instance.name.as_str(), "Camera" | "CurrentCamera")
    {
        return false;
    }
    let Some(parent) = instance
        .parent_index
        .and_then(|index| document.instances.get(index))
    else {
        return false;
    };
    parent.class_name == "Workspace" && parent.parent_index.is_none()
}

pub(crate) fn align_reconciliation_protected_workspace_cameras(
    reference: &SettingsBytecode,
    observed: &mut SettingsBytecode,
) {
    let observed_by_id = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for (reference_index, reference_instance) in reference.instances.iter().enumerate() {
        if !is_reconciliation_protected_workspace_camera(reference, reference_index) {
            continue;
        }
        let Some(observed_index) = observed_by_id.get(&reference_instance.settings_id).copied()
        else {
            continue;
        };
        if !is_reconciliation_protected_workspace_camera(observed, observed_index) {
            continue;
        }
        let observed_instance = &mut observed.instances[observed_index];
        observed_instance
            .properties
            .clone_from(&reference_instance.properties);
        observed_instance
            .attributes
            .clone_from(&reference_instance.attributes);
    }
}

pub(crate) fn reconciliation_maps_equal(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    reconciliation_maps_equal_with_ids(class_name, left, right, &AHashMap::new(), &AHashMap::new())
}

fn reconciliation_maps_equal_with_ids(
    class_name: &str,
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    left_ids: &AHashMap<&str, usize>,
    right_ids: &AHashMap<&str, usize>,
) -> bool {
    let stable = |name: &str, value: &Value| {
        name != "ScriptGuid"
            && !reconciliation_property_is_derived(name)
            && !reconciliation_property_is_metadata(name, value)
    };
    for (name, value) in left {
        if !stable(name, value) {
            continue;
        }
        match right.get(name) {
            Some(other)
                if reconciliation_values_equal_with_ids(
                    value,
                    other,
                    reconciliation_property_uses_f32(class_name, name, value, other),
                    Some(left_ids),
                    Some(right_ids),
                ) => {}
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

pub(crate) fn reconciliation_values_map_equal(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
) -> bool {
    reconciliation_values_map_equal_with_ids(left, right, &AHashMap::new(), &AHashMap::new())
}

fn reconciliation_values_map_equal_with_ids(
    left: &Map<String, Value>,
    right: &Map<String, Value>,
    left_ids: &AHashMap<&str, usize>,
    right_ids: &AHashMap<&str, usize>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(name, value)| {
            right.get(name).is_some_and(|other| {
                reconciliation_values_equal_with_ids(
                    value,
                    other,
                    false,
                    Some(left_ids),
                    Some(right_ids),
                )
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

fn identity_value_is_stable(name: &str, value: &Value, ignore_transient_properties: bool) -> bool {
    (!ignore_transient_properties
        || name != "ScriptGuid"
            && !reconciliation_property_is_derived(name)
            && !reconciliation_property_is_metadata(name, value))
        && !reconciliation_value_contains_reference(value)
}

fn reconciliation_value_contains_reference(value: &Value) -> bool {
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
    reconciliation_values_equal_with_ids(left, right, approximate, None, None)
}

fn reconciliation_values_equal_with_ids(
    left: &Value,
    right: &Value,
    approximate: bool,
    left_ids: Option<&AHashMap<&str, usize>>,
    right_ids: Option<&AHashMap<&str, usize>>,
) -> bool {
    match (left, right) {
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
                    reconciliation_values_equal_with_ids(
                        left,
                        right,
                        approximate,
                        left_ids,
                        right_ids,
                    )
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
                    return match (
                        left_ids.and_then(|ids| ids.get(left_id)),
                        right_ids.and_then(|ids| ids.get(right_id)),
                    ) {
                        (Some(left), Some(right)) => left == right,
                        _ => left_id == right_id,
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
                let values_match = match (
                    left.get("value").and_then(Value::as_u64),
                    right.get("value").and_then(Value::as_u64),
                ) {
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
            let approximate = approximate || type_name.is_some_and(reconciliation_value_uses_f32);
            left.len() == right.len()
                && left.iter().all(|(name, value)| {
                    let Some(other) = right.get(name) else {
                        return false;
                    };
                    if type_name == Some("Ref")
                        && matches!(name.as_str(), "settingsId" | "instanceId")
                    {
                        let (Some(left), Some(right)) = (value.as_str(), other.as_str()) else {
                            return value == other;
                        };
                        return match (
                            left_ids.and_then(|ids| ids.get(left)),
                            right_ids.and_then(|ids| ids.get(right)),
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
                    reconciliation_values_equal_with_ids(
                        value,
                        other,
                        approximate,
                        left_ids,
                        right_ids,
                    )
                })
        }
        _ => left == right,
    }
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
    matches!(
        name,
        "WorldCFrame" | "WorldPosition" | "WorldOrientation" | "WorldAxis" | "WorldSecondaryAxis"
    )
}

pub(crate) fn reconciliation_property_is_metadata(name: &str, value: &Value) -> bool {
    name == "Source" && value.as_str() == Some(EXTERNAL_SOURCE_MARKER)
}

pub(crate) fn reconciliation_property_value<'a>(
    properties: &'a Map<String, Value>,
    name: &str,
) -> Option<&'a Value> {
    properties
        .get(name)
        .filter(|value| !reconciliation_property_is_metadata(name, value))
}

fn reconciliation_property_value_is_default(class_name: &str, name: &str, value: &Value) -> bool {
    if is_known_default_property_value(name, value) {
        return true;
    }
    let Ok(database) = rbx_reflection_database::get() else {
        return false;
    };
    let descriptor = rbx_model_property_descriptor(database, class_name, name);
    let serialized_name = descriptor.map_or(name, |descriptor| descriptor.name);
    let Some(default) = database
        .classes
        .get(class_name)
        .and_then(|class| database.find_default_property(class, serialized_name))
        .and_then(|default| {
            rbx_variant_to_settings_json(
                default,
                descriptor,
                database,
                &BytecodeModelImportRefs::default(),
            )
        })
    else {
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
    let left = left.filter(|value| !reconciliation_property_is_metadata(name, value));
    let right = right.filter(|value| !reconciliation_property_is_metadata(name, value));
    match (left, right) {
        (Some(left), Some(right)) => reconciliation_values_equal(
            left,
            right,
            reconciliation_property_uses_f32(class_name, name, left, right),
        ),
        (Some(value), None) | (None, Some(value)) => {
            reconciliation_property_value_is_default(class_name, name, value)
        }
        (None, None) => true,
    }
}

pub(crate) fn remove_reconciliation_derived_properties(document: &mut SettingsBytecode) {
    for instance in &mut document.instances {
        instance
            .properties
            .retain(|name, _| !reconciliation_property_is_derived(name));
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
    for reference_instance in &reference.instances {
        let Some(observed_index) = observed_by_id
            .get(reference_instance.settings_id.as_str())
            .copied()
        else {
            continue;
        };
        let observed_instance = &mut observed.instances[observed_index];
        align_equivalent_property_map_values(
            &reference_instance.class_name,
            &reference_instance.properties,
            &mut observed_instance.properties,
        );
        align_equivalent_map_values(
            &reference_instance.attributes,
            &mut observed_instance.attributes,
        );
    }
}

fn align_equivalent_property_map_values(
    class_name: &str,
    reference: &Map<String, Value>,
    observed: &mut Map<String, Value>,
) {
    for (name, reference_value) in reference {
        let Some(observed_value) = observed.get_mut(name) else {
            continue;
        };
        if reference_value != observed_value
            && reconciliation_property_values_equal(
                class_name,
                name,
                Some(reference_value),
                Some(observed_value),
            )
        {
            observed_value.clone_from(reference_value);
        }
    }
}

fn align_equivalent_map_values(reference: &Map<String, Value>, observed: &mut Map<String, Value>) {
    for (name, reference_value) in reference {
        let Some(observed_value) = observed.get_mut(name) else {
            continue;
        };
        if reference_value != observed_value
            && reconciliation_values_equal(reference_value, observed_value, false)
        {
            observed_value.clone_from(reference_value);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
    fn positional_identity_checks_duplicate_subtrees_and_incoming_references() {
        let mut reference = duplicate_geometry(2);
        for parent in [1, 2] {
            let mut child = SettingsBytecodeInstance::new(
                format!("child:{parent}"),
                "Leaf".into(),
                "StringValue".into(),
                Some(parent),
            );
            child.properties.insert("Value".into(), json!(parent));
            reference.instances.push(child);
        }
        let mut pointer = SettingsBytecodeInstance::new(
            "pointer".into(),
            "Pointer".into(),
            "ObjectValue".into(),
            Some(0),
        );
        pointer.properties.insert(
            "Value".into(),
            json!({"_type":"Ref", "settingsId":"editor:1"}),
        );
        reference.instances.push(pointer);
        let mut observed = reference.clone();
        observed.instances[0]
            .attributes
            .insert("Edited".into(), json!(true));
        assert!(positional_identity_preserved(&reference, &observed));
        assert!(!settings_documents_positionally_equivalent(
            &reference, &observed
        ));
        observed.instances[1]
            .attributes
            .insert("Edited".into(), json!(true));
        assert!(!positional_identity_preserved(&reference, &observed));
        observed.instances[1].attributes.clear();
        observed.instances[3]
            .properties
            .insert("Value".into(), json!(2));
        observed.instances[4]
            .properties
            .insert("Value".into(), json!(1));
        assert!(!positional_identity_preserved(&reference, &observed));
        observed = reference.clone();
        observed.instances[5].properties["Value"]["settingsId"] = json!("editor:2");
        assert!(!positional_identity_preserved(&reference, &observed));
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
    fn serialized_property_aliases_canonicalize_to_logical_names() {
        let mut document = string_value_with_properties(Map::from_iter([(
            "archivable".to_string(),
            json!(false),
        )]));

        canonicalize_settings_property_names(&mut document).unwrap();

        assert_eq!(
            document.instances[0].properties,
            Map::from_iter([("Archivable".to_string(), json!(false))])
        );
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
    fn observed_changes_keep_reference_ids_and_remap_internal_refs() {
        let mut reference_root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "1".to_string(),
            "ReplicatedStorage".to_string(),
            "ReplicatedStorage".to_string(),
            None,
        );
        reference_root
            .properties
            .insert("Archivable".to_string(), json!(true));
        let mut reference_value = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:value".to_string(),
            "Value".to_string(),
            "StringValue".to_string(),
            Some(0),
        );
        reference_value
            .properties
            .insert("Value".to_string(), json!("before"));
        let mut reference_holder = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:holder".to_string(),
            "Holder".to_string(),
            "ObjectValue".to_string(),
            Some(0),
        );
        reference_holder.properties.insert(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "editor:value"}),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![reference_root.clone(), reference_value, reference_holder],
        };

        let mut observed_value = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "debug:value".to_string(),
            "Value".to_string(),
            "StringValue".to_string(),
            Some(0),
        );
        observed_value
            .properties
            .insert("Value".to_string(), json!("after"));
        let mut observed_holder = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "debug:holder".to_string(),
            "Holder".to_string(),
            "ObjectValue".to_string(),
            Some(0),
        );
        observed_holder.properties.insert(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "debug:value"}),
        );
        let mut observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![reference_root, observed_value, observed_holder],
        };

        align_settings_ids_to_reference(&reference, &mut observed);

        assert_eq!(observed.instances[1].settings_id, "editor:value");
        assert_eq!(observed.instances[2].settings_id, "editor:holder");
        assert_eq!(
            observed.instances[2].properties["Value"]["settingsId"],
            "editor:value"
        );
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
    fn alignment_pairs_only_remaining_indistinguishable_duplicates() {
        let mut reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![crate::settings::bytecode::SettingsBytecodeInstance::new(
                "root".to_string(),
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
        observed.instances[0].settings_id = "debug:root".to_string();
        observed.instances[1].settings_id = "debug:first".to_string();
        observed.instances[2].settings_id = "debug:second".to_string();

        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(observed.instances[0].settings_id, "root");
        assert_eq!(observed.instances[1].settings_id, "editor:first");
        assert_eq!(observed.instances[2].settings_id, "editor:second");
    }

    #[test]
    fn alignment_keeps_unique_identity_when_content_changed() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "root".to_string(),
            "ReplicatedStorage".to_string(),
            "ReplicatedStorage".to_string(),
            None,
        );
        let mut reference_child = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:child".to_string(),
            "NonLobby".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        reference_child
            .attributes
            .insert("State".to_string(), json!("project"));
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), reference_child],
        };
        let mut observed_child = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "debug:child".to_string(),
            "NonLobby".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        observed_child
            .attributes
            .insert("State".to_string(), json!("studio"));
        let mut observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, observed_child],
        };

        assert!(align_settings_ids_to_reference(&reference, &mut observed));
        assert_eq!(observed.instances[1].settings_id, "editor:child");
        assert_eq!(observed.instances[1].attributes["State"], "studio");
    }

    #[test]
    fn aligned_settings_bytes_ignore_sibling_enumeration_order() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "1".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        let first = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:first".to_string(),
            "First".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let second = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:second".to_string(),
            "Second".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), first.clone(), second.clone()],
        };
        let mut observed_first = first;
        observed_first.settings_id = "debug:first".to_string();
        let mut observed_second = second;
        observed_second.settings_id = "debug:second".to_string();
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, observed_second, observed_first],
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
    fn aligned_settings_bytes_ignore_reordered_duplicate_siblings() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "1".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        let mut first = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:first".to_string(),
            "Attachment".to_string(),
            "Attachment".to_string(),
            Some(0),
        );
        first.attributes.insert("Side".to_string(), json!("first"));
        let mut second = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:second".to_string(),
            "Attachment".to_string(),
            "Attachment".to_string(),
            Some(0),
        );
        second
            .attributes
            .insert("Side".to_string(), json!("second"));
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), first.clone(), second.clone()],
        };
        first.settings_id = "debug:first".to_string();
        second.settings_id = "debug:second".to_string();
        let positional_observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), first.clone(), second.clone()],
        };
        assert!(settings_documents_positionally_equivalent(
            &reference,
            &positional_observed
        ));
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![root, second, first],
        };
        assert!(!settings_documents_positionally_equivalent(
            &reference, &observed
        ));

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
    fn aligned_settings_bytes_detect_reparenting() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "1".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        let first_parent = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:first-parent".to_string(),
            "First".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let second_parent = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:second-parent".to_string(),
            "Second".to_string(),
            "Folder".to_string(),
            Some(0),
        );
        let child = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "editor:child".to_string(),
            "Child".to_string(),
            "Folder".to_string(),
            Some(1),
        );
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root.clone(),
                first_parent.clone(),
                second_parent.clone(),
                child.clone(),
            ],
        };
        let mut observed_first_parent = first_parent;
        observed_first_parent.settings_id = "debug:first-parent".to_string();
        let mut observed_second_parent = second_parent;
        observed_second_parent.settings_id = "debug:second-parent".to_string();
        let mut observed_child = child;
        observed_child.settings_id = "debug:child".to_string();
        observed_child.parent_index = Some(2);
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root,
                observed_first_parent,
                observed_second_parent,
                observed_child,
            ],
        };

        assert!(matches!(
            align_settings_bytes_to_reference(
                &encode_settings_bytecode(&reference).unwrap(),
                &encode_settings_bytecode(&observed).unwrap(),
            )
            .unwrap(),
            SettingsAlignment::Changed(_)
        ));
    }

    #[test]
    fn aligned_settings_bytes_remap_references_across_reordering() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "1".to_string(),
            "ReplicatedStorage".to_string(),
            "ReplicatedStorage".to_string(),
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
            instances: vec![root, observed_holder, observed_target],
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
    fn duplicate_subtrees_pair_by_content_before_references() {
        let root = crate::settings::bytecode::SettingsBytecodeInstance::new(
            "root".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        );
        let model = |id: &str, pivot: i64, child_id: &str| {
            let mut instance = crate::settings::bytecode::SettingsBytecodeInstance::new(
                id.to_string(),
                "Duplicate".to_string(),
                "Model".to_string(),
                Some(0),
            );
            instance
                .properties
                .insert("PivotMarker".to_string(), json!(pivot));
            instance.properties.insert(
                "PrimaryPart".to_string(),
                json!({"_type": "Ref", "settingsId": child_id}),
            );
            instance
        };
        let child = |id: &str, parent_index: usize| {
            crate::settings::bytecode::SettingsBytecodeInstance::new(
                id.to_string(),
                "Primary".to_string(),
                "Part".to_string(),
                Some(parent_index),
            )
        };
        let reference = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root.clone(),
                model("editor:model-a", 10, "editor:child-a"),
                child("editor:child-a", 1),
                model("editor:model-b", 20, "editor:child-b"),
                child("editor:child-b", 3),
            ],
        };
        let observed = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                root,
                model("debug:model-b", 20, "debug:child-b"),
                child("debug:child-b", 1),
                model("debug:model-a", 10, "debug:child-a"),
                child("debug:child-a", 3),
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
    fn aligned_settings_bytes_keep_ids_when_content_changes() {
        let mut reference = SettingsBytecode {
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
        reference.instances[1]
            .properties
            .insert("Value".to_string(), json!("before"));
        let mut observed = reference.clone();
        observed.instances[1].settings_id = "debug:0_12".to_string();
        observed.instances[1]
            .properties
            .insert("Value".to_string(), json!("after"));
        let reference_bytes = encode_settings_bytecode(&reference).unwrap();
        let observed_bytes = encode_settings_bytecode(&observed).unwrap();

        let SettingsAlignment::Changed(aligned) =
            align_settings_bytes_to_reference(&reference_bytes, &observed_bytes).unwrap()
        else {
            panic!("changed content was treated as equivalent");
        };
        let aligned = decode_settings_bytecode(&aligned).unwrap();
        assert_eq!(aligned.instances[1].settings_id, "editor:value");
        assert_eq!(aligned.instances[1].properties["Value"], "after");
    }

    #[test]
    fn enum_items_allow_compact_and_expanded_forms() {
        let compact = json!({"_type": "EnumItem", "name": "Sensor"});
        let expanded = json!({
            "_type": "EnumItem",
            "enumType": "Enum.ScreenOrientation",
            "name": "Sensor",
            "value": 2,
        });
        assert!(reconciliation_values_equal(&compact, &expanded, false));
        assert!(reconciliation_values_equal(&expanded, &compact, false));
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
    fn workspace_camera_viewport_values_do_not_affect_reconciliation() {
        let root = SettingsBytecodeInstance::new(
            "root".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
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
        assert!(!settings_documents_equivalent(
            &nested_reference,
            &nested_observed
        ));
    }

    #[test]
    fn float32_properties_use_roblox_precision_without_weakening_other_numbers() {
        let concise = json!(0.2);
        let studio_float32 = json!(0.20000000298023224);
        let different = json!(0.21);

        assert!(reconciliation_property_values_equal(
            "Part",
            "Transparency",
            Some(&concise),
            Some(&studio_float32),
        ));
        assert!(!reconciliation_property_values_equal(
            "Part",
            "Transparency",
            Some(&concise),
            Some(&different),
        ));
        assert!(!reconciliation_values_equal(
            &concise,
            &studio_float32,
            false,
        ));
    }
}
