//! First-pairing evidence, kept separate from the mutable identity/reference remap.
//! Subtree candidates are complete. A group must cover its entire smaller side;
//! distinct root records can pair parents before comparing changed descendants.
use super::*;
use ahash::AHashSet;
use std::borrow::Cow;
use std::cell::RefCell;
use std::hash::{Hash, Hasher};

mod context;
mod search;
use context::Context;

#[cfg(test)]
thread_local! { static RECOVERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
pub(crate) fn take_recoveries() -> usize {
    RECOVERIES.with(|value| value.replace(0))
}

use crate::settings::tree::DUPLICATE_REPAIR_GROUP_LIMIT as GROUP_LIMIT;
type Assignment = Vec<(usize, usize)>; // (Studio index, editor index)
type GroupKey<'a> = (Option<usize>, &'a str, &'a str);

#[derive(Clone, PartialEq, Eq)]
enum ReferenceStep<'a> {
    Field(bool, &'a str),
    Key(&'a str),
    Index(usize),
}

struct Tree<'a> {
    document: &'a SettingsBytecode,
    properties: Vec<Cow<'a, Map<String, Value>>>,
    groups: Vec<Vec<usize>>,
    by_group: AHashMap<GroupKey<'a>, usize>,
    children: Vec<Vec<usize>>,
    by_id: AHashMap<&'a str, usize>,
    references: Vec<Vec<&'a Value>>,
    reference_paths: Vec<Vec<Vec<ReferenceStep<'a>>>>,
    reference_fields: Vec<Vec<(bool, &'a str)>>,
    reference_work: Vec<usize>,
    incoming: Vec<Vec<usize>>,
    reference_parent: Vec<Option<usize>>,
    subtree_size: Vec<usize>,
    #[cfg(test)]
    reference_comparisons: std::cell::Cell<usize>,
}

impl<'a> Tree<'a> {
    fn new(document: &'a SettingsBytecode) -> Self {
        let mut tree = Self {
            document,
            properties: document
                .instances
                .iter()
                .map(comparison_properties)
                .collect(),
            groups: Vec::new(),
            by_group: AHashMap::with_capacity(document.instances.len()),
            children: vec![Vec::new(); document.instances.len() + 1],
            by_id: AHashMap::with_capacity(document.instances.len()),
            references: Vec::with_capacity(document.instances.len()),
            reference_paths: Vec::with_capacity(document.instances.len()),
            reference_fields: Vec::with_capacity(document.instances.len()),
            reference_work: Vec::with_capacity(document.instances.len()),
            incoming: vec![Vec::new(); document.instances.len()],
            reference_parent: vec![None; document.instances.len()],
            subtree_size: vec![1; document.instances.len()],
            #[cfg(test)]
            reference_comparisons: std::cell::Cell::new(0),
        };
        for (index, instance) in document.instances.iter().enumerate() {
            tree.by_id.insert(&instance.settings_id, index);
            let key = (
                instance.parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            );
            let next = tree.groups.len();
            let group = *tree.by_group.entry(key).or_insert(next);
            if group == next {
                tree.groups.push(Vec::new());
                tree.children[instance.parent_index.unwrap_or(document.instances.len())]
                    .push(group);
            }
            tree.groups[group].push(index);
            let mut references = Vec::new();
            let mut paths = Vec::new();
            let mut fields = Vec::new();
            let mut path = Vec::new();
            if instance.class_name != "PackageLink" {
                for (attribute, map) in
                    [(false, &instance.properties), (true, &instance.attributes)]
                {
                    for (name, value) in map {
                        let before = references.len();
                        collect_reference_values(value, &mut references);
                        if references.len() != before {
                            fields.push((attribute, name.as_str()));
                            path.push(ReferenceStep::Field(attribute, name));
                            collect_reference_paths(value, &mut path, &mut paths);
                            path.pop();
                        }
                    }
                }
            }
            tree.reference_work.push(paths.iter().map(Vec::len).sum());
            tree.reference_paths.push(paths);
            tree.reference_fields.push(fields);
            tree.references.push(references);
        }
        for source in 0..document.instances.len() {
            for value in &tree.references[source] {
                if let Some(target) = tree.target(value) {
                    tree.incoming[target].push(source);
                }
            }
        }
        let mut pending = tree.children[document.instances.len()]
            .iter()
            .flat_map(|&g| tree.groups[g].iter().map(|&s| (s, None)))
            .collect::<Vec<_>>();
        let mut order = Vec::with_capacity(document.instances.len());
        while let Some((source, parent)) = pending.pop() {
            order.push(source);
            tree.reference_parent[source] = parent;
            let parent = if tree.incoming[source].is_empty() {
                parent
            } else {
                Some(source)
            };
            for &group in &tree.children[source] {
                pending.extend(tree.groups[group].iter().map(|&s| (s, parent)));
            }
        }
        for source in order.into_iter().rev() {
            if let Some(parent) = document.instances[source].parent_index {
                tree.subtree_size[parent] += tree.subtree_size[source];
            }
        }
        tree
    }

    fn counterpart(
        &self,
        parent: Option<usize>,
        other: &SettingsBytecodeInstance,
    ) -> Option<usize> {
        self.by_group
            .get(&(parent, other.name.as_str(), other.class_name.as_str()))
            .copied()
    }

    fn target(&self, value: &Value) -> Option<usize> {
        if let Some(id) = value
            .get("settingsId")
            .or_else(|| value.get("instanceId"))
            .and_then(Value::as_str)
        {
            return self.by_id.get(id).copied();
        }
        value
            .get("instanceIndex")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| index.checked_sub(1))
            .filter(|index| *index < self.document.instances.len())
    }
}

// Quantization is a proposal only, never equality or negative evidence. It
// catches ordinary floating-point drift without a quadratic candidate matrix;
// collisions, missing keys or failed complete proofs use the general matcher.
fn field_key(value: &Value, approximate: bool) -> u64 {
    fn hash(value: &Value, state: &mut impl Hasher) {
        std::mem::discriminant(value).hash(state);
        match value {
            Value::Number(n) => {
                let n = (n.as_f64().unwrap() * 10.0).round();
                (if n == 0.0 { 0 } else { n.to_bits() }).hash(state);
            }
            Value::Object(map) => {
                for (k, v) in map {
                    k.hash(state);
                    hash(v, state);
                }
            }
            Value::Array(array) => {
                for v in array {
                    hash(v, state);
                }
            }
            _ => value.hash(state),
        }
    }
    let mut state = std::collections::hash_map::DefaultHasher::new();
    if approximate {
        hash(value, &mut state);
    } else {
        value.hash(&mut state);
    }
    state.finish()
}

fn charge_value(value: &Value, spend: &impl Fn(usize) -> bool) -> bool {
    spend(1)
        && match value {
            Value::Object(v) => v.values().all(|v| charge_value(v, spend)),
            Value::Array(v) => v.iter().all(|v| charge_value(v, spend)),
            _ => true,
        }
}

fn collect_reference_values<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(_) if is_reference(value) => out.push(value),
        Value::Object(object) => {
            for value in object.values() {
                collect_reference_values(value, out);
            }
        }
        Value::Array(array) => {
            for value in array {
                collect_reference_values(value, out);
            }
        }
        _ => {}
    }
}

fn collect_reference_paths<'a>(
    value: &'a Value,
    path: &mut Vec<ReferenceStep<'a>>,
    paths: &mut Vec<Vec<ReferenceStep<'a>>>,
) {
    match value {
        Value::Object(_) if is_reference(value) => {
            paths.push(path.clone());
        }
        Value::Object(object) => {
            for (key, value) in object {
                path.push(ReferenceStep::Key(key));
                collect_reference_paths(value, path, paths);
                path.pop();
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                path.push(ReferenceStep::Index(index));
                collect_reference_paths(value, path, paths);
                path.pop();
            }
        }
        _ => {}
    }
}

fn is_reference(value: &Value) -> bool {
    value.get("_type").and_then(Value::as_str) == Some("Ref")
}

fn contains_reference(value: &Value) -> bool {
    match value {
        Value::Object(object) => is_reference(value) || object.values().any(contains_reference),
        Value::Array(array) => array.iter().any(contains_reference),
        _ => false,
    }
}

fn references_equal(
    a: Option<&Value>,
    b: Option<&Value>,
    equal: &impl Fn(&Value, &Value) -> bool,
) -> bool {
    match (a, b) {
        (Some(a), Some(b)) if is_reference(a) || is_reference(b) => {
            is_reference(a) && is_reference(b) && equal(a, b)
        }
        (Some(Value::Object(a)), Some(Value::Object(b))) => reference_maps_equal(a, b, equal),
        (Some(Value::Array(a)), Some(Value::Array(b))) => (0..a.len().max(b.len()))
            .all(|index| references_equal(a.get(index), b.get(index), equal)),
        _ => !a.is_some_and(contains_reference) && !b.is_some_and(contains_reference),
    }
}

fn reference_maps_equal(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    equal: &impl Fn(&Value, &Value) -> bool,
) -> bool {
    a.iter()
        .all(|(name, value)| references_equal(Some(value), b.get(name), equal))
        && b.iter()
            .all(|(name, value)| a.contains_key(name) || references_equal(None, Some(value), equal))
}

/// No property maps are copied per candidate. This shares the first-comparison
/// normalization and equality, including defaults, tags, assets and hull fidelity.
fn records_match(
    tree: &Tree<'_>,
    left: usize,
    other: &Tree<'_>,
    right: usize,
    verified_frame: bool,
    spend: &impl Fn(usize) -> bool,
) -> bool {
    let a = &tree.document.instances[left];
    let b = &other.document.instances[right];
    a.name == b.name
        && a.class_name == b.class_name
        && if a.class_name == "PackageLink" {
            spend(1)
                && crate::settings::equivalence::reconciliation_property_values_equal(
                    "PackageLink",
                    "PackageId",
                    a.properties.get("PackageId"),
                    b.properties.get("PackageId"),
                )
        } else {
            attributes_match(&a.attributes, &b.attributes, spend)
                && properties_match(
                    &a.class_name,
                    &tree.properties[left],
                    &other.properties[right],
                    verified_frame,
                    spend,
                )
        }
}

fn exact_maps(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    spend: &impl Fn(usize) -> bool,
) -> bool {
    maps_match(a, b, false, false, spend)
}

fn maps_match(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    references: bool,
    verified_frame: bool,
    spend: &impl Fn(usize) -> bool,
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|((ka, a), (kb, b))| {
            if ka != kb {
                spend(1);
                false
            } else if verified_frame && ka == "CFrame" {
                spend(1)
            } else {
                // The value visit charges this map entry, including its key.
                values_match(a, b, references, spend)
            }
        })
}

// Charge while comparing, stopping at the first difference or exhausted entry.
// One paired entry is one unit of work; no separate unbounded equality walk.
fn values_match(a: &Value, b: &Value, references: bool, spend: &impl Fn(usize) -> bool) -> bool {
    if !spend(1) {
        return false;
    }
    if references && is_reference(a) && is_reference(b) {
        return has_target(a) == has_target(b);
    }
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => maps_match(a, b, references, false, spend),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| values_match(a, b, references, spend))
        }
        _ => a == b,
    }
}

fn properties_match(
    class: &str,
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    verified_frame: bool,
    spend: &impl Fn(usize) -> bool,
) -> bool {
    if maps_match(a, b, true, verified_frame, spend) {
        return true;
    }
    let database = rbx_reflection_database::get().ok();
    let equal = |name: &str, value: &Value, other: Option<&Value>| {
        spend(1)
            && (name == "CFrame" && verified_frame
                || !crate::settings::equivalence::reconciliation_property_compares(
                    database,
                    class,
                    name,
                    value,
                    true,
                    other.is_some(),
                )
                || charge_value(value, spend)
                    && other.is_none_or(|v| charge_value(v, spend))
                    && (other.is_some_and(|other| reference_shapes_equal(value, other))
                        || reconciliation_property_values_equal(class, name, Some(value), other)))
    };
    a.iter()
        .all(|(name, value)| equal(name, value, b.get(name)))
        && b.iter()
            .all(|(name, value)| spend(1) && (a.contains_key(name) || equal(name, value, None)))
}

fn attributes_match(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    spend: &impl Fn(usize) -> bool,
) -> bool {
    let authored = |name: &str| !crate::settings::equivalence::is_engine_managed_attribute(name);
    if !spend(a.len() * 2 + b.len())
        || a.keys().filter(|n| authored(n)).count() != b.keys().filter(|n| authored(n)).count()
    {
        return false;
    }
    let equal = |name: &String, value: &Value| {
        b.get(name).is_some_and(|other| {
            charge_value(value, spend)
                && charge_value(other, spend)
                && (reference_shapes_equal(value, other)
                    || reconciliation_values_equal(value, other, false))
        })
    };
    a.iter()
        .filter(|(n, v)| {
            authored(n) && matches!(v, Value::Null | Value::Bool(_) | Value::Number(_))
        })
        .all(|(n, v)| equal(n, v))
        && a.iter()
            .all(|(n, v)| spend(1) && (!authored(n) || equal(n, v)))
}

// A reference is deferred, never erased. Missing/null references still differ.
fn reference_shapes_equal(a: &Value, b: &Value) -> bool {
    if is_reference(a) && is_reference(b) {
        return has_target(a) == has_target(b);
    }
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(name, value)| {
                    b.get(name)
                        .is_some_and(|other| reference_shapes_equal(value, other))
                })
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| reference_shapes_equal(a, b))
        }
        _ => a == b,
    }
}

fn has_target(value: &Value) -> bool {
    ["settingsId", "instanceId", "instanceIndex", "pathSegments"]
        .iter()
        .any(|name| value.get(name).is_some())
}

#[derive(Default)]
struct Candidates {
    #[cfg(test)]
    record_comparisons: std::cell::Cell<usize>,
    #[cfg(test)]
    unbudgeted: std::cell::Cell<usize>,
    records: RefCell<AHashMap<(usize, usize), bool>>,
    plain_subtrees: RefCell<AHashSet<(usize, usize)>>,
    // Identity-free content proofs must not reuse identity-constrained subtrees.
    subtrees: RefCell<AHashMap<(bool, usize, usize), Option<Assignment>>>,
}

struct Matcher<'a, 'd> {
    budget: Option<&'a search::Budget>,
    editor: &'a Tree<'d>,
    studio: &'a Tree<'d>,
    locked: &'a [Option<usize>],
    owners: &'a [Option<usize>],
    candidates: &'a Candidates,
}

impl Matcher<'_, '_> {
    fn spend(&self, work: usize) -> bool {
        self.budget.is_none_or(|budget| budget.spend(work))
    }

    fn record(&self, studio: usize, editor: usize) -> bool {
        if !self.spend(1) {
            return false;
        }
        if let Some(&equal) = self.candidates.records.borrow().get(&(studio, editor)) {
            return equal;
        }
        #[cfg(test)]
        self.candidates
            .record_comparisons
            .set(self.candidates.record_comparisons.get() + 1);
        #[cfg(test)]
        if self.budget.is_none() {
            self.candidates
                .unbudgeted
                .set(self.candidates.unbudgeted.get() + 1);
        }
        let frame = self.quick_frame(studio, editor);
        let equal = frame != Some(false)
            && records_match(
                self.editor,
                editor,
                self.studio,
                studio,
                frame == Some(true),
                &|n| self.spend(n),
            );
        if self.spend(0) {
            self.candidates
                .records
                .borrow_mut()
                .insert((studio, editor), equal);
        }
        equal
    }

    /// Spatial differences usually reject a reordered part immediately. Read
    /// this existing comparison field before unrelated maps or large attributes.
    fn quick_frame(&self, source: usize, target: usize) -> Option<bool> {
        let class = &self.editor.document.instances[target].class_name;
        if class == "PackageLink" {
            return None;
        }
        let (Some(a), Some(b)) = (
            self.editor.properties[target].get("CFrame"),
            self.studio.properties[source].get("CFrame"),
        ) else {
            return None;
        };
        if !self.spend(
            self.editor.reference_fields[target].len() + self.studio.reference_fields[source].len(),
        ) || self.editor.reference_fields[target]
            .iter()
            .chain(&self.studio.reference_fields[source])
            .any(|&(attribute, name)| !attribute && name == "CFrame")
        {
            return None;
        }
        let database = rbx_reflection_database::get().ok();
        Some(
            !crate::settings::equivalence::reconciliation_property_compares(
                database, class, "CFrame", a, true, true,
            ) || charge_value(a, &|n| self.spend(n))
                && charge_value(b, &|n| self.spend(n))
                && reconciliation_property_values_equal(class, "CFrame", Some(a), Some(b)),
        )
    }

    fn locked_pair(&self, studio: usize, editor: usize) -> bool {
        self.locked.get(studio).copied().flatten() == Some(editor)
    }

    fn allowed(&self, studio: usize, editor: usize) -> bool {
        self.locked
            .get(studio)
            .copied()
            .flatten()
            .is_none_or(|target| target == editor)
            && self
                .owners
                .get(editor)
                .copied()
                .flatten()
                .is_none_or(|source| source == studio)
    }

    /// Distinct root records can identify a complete sibling group even when
    /// a descendant was added or removed. Equal roots still need subtree proof.
    /// Reference-bearing roots require the candidate scopes used by `group`.
    fn distinct_roots(&self, studio: &[usize], editor: &[usize]) -> Option<Assignment> {
        if !self.spend(studio.len() + editor.len()) {
            return None;
        }
        if studio.len() != editor.len()
            || studio
                .iter()
                .any(|&s| !self.studio.references[s].is_empty())
            || editor
                .iter()
                .any(|&e| !self.editor.references[e].is_empty())
        {
            return None;
        }
        let mut result = Vec::with_capacity(studio.len());
        let mut claimed = AHashSet::new();
        for &source in studio {
            let mut candidates = editor.iter().copied().filter(|&target| {
                self.spend(1) && self.allowed(source, target) && self.record(source, target)
            });
            let target = candidates.next()?;
            if candidates.next().is_some() || !claimed.insert(target) {
                return None;
            }
            result.push((source, target));
        }
        Some(result)
    }

    fn subtree(&self, studio: usize, editor: usize) -> Option<Assignment> {
        if !self.spend(1) || self.studio.subtree_size[studio] != self.editor.subtree_size[editor] {
            return None;
        }
        let key = (self.locked.is_empty(), studio, editor);
        if let Some(plan) = self.candidates.subtrees.borrow().get(&key) {
            if !self.spend(plan.as_ref().map_or(0, Vec::len)) {
                return None;
            }
            return plan.clone();
        }
        let plan = self.build_subtree(studio, editor);
        // Exhaustion is not immutable negative evidence.
        if self.spend(plan.as_ref().map_or(0, Vec::len)) {
            self.candidates
                .subtrees
                .borrow_mut()
                .insert(key, plan.clone());
        }
        plan
    }

    fn build_subtree(&self, studio: usize, editor: usize) -> Option<Assignment> {
        let mut pending = vec![(studio, editor)];
        let mut result = Vec::new();
        while let Some((source, target)) = pending.pop() {
            if !self.spend(1)
                || !self.allowed(source, target)
                || !self.locked_pair(source, target) && !self.record(source, target)
                || self.editor.children[target].len() != self.studio.children[source].len()
            {
                return None;
            }
            result.push((source, target));
            for &group in &self.studio.children[source] {
                let members = &self.studio.groups[group];
                let first = &self.studio.document.instances[members[0]];
                let counterpart = self.editor.counterpart(Some(target), first)?;
                let targets = &self.editor.groups[counterpart];
                if members.len() != targets.len() {
                    return None;
                }
                if members.len() == 1 {
                    // Unique child groups need no candidate matrix or cached
                    // copy of every ancestor's growing descendant assignment.
                    pending.push((members[0], targets[0]));
                } else {
                    let children = self.group(members, targets, &Some)?;
                    if !self.spend(children.len()) {
                        return None;
                    }
                    result.extend(children);
                }
            }
        }
        Some(result)
    }

    /// A unique exact field proposes a bijection without an all-pairs scan.
    /// Every candidate still receives the complete ordinary subtree proof.
    fn keyed_group(
        &self,
        studio: &[usize],
        editor: &[usize],
        prepare: &impl Fn(Assignment) -> Option<Assignment>,
    ) -> Option<Assignment> {
        if studio.len() < 8 || studio.len() != editor.len() {
            return None;
        }
        for approximate in [false, true] {
            for name in self.editor.properties[editor[0]].keys() {
                if !self.spend(1) {
                    return None;
                }
                let Some(roots) = self.field_pairs(studio, editor, name, approximate) else {
                    continue;
                };
                let plan = roots
                    .into_iter()
                    .map(|(s, e)| self.subtree(s, e).and_then(prepare))
                    .collect::<Option<Vec<_>>>();
                if let Some(plan) = plan {
                    if !self.spend(plan.iter().map(Vec::len).sum()) {
                        return None;
                    }
                    return Some(plan.into_iter().flatten().collect());
                }
            }
        }
        None
    }

    fn field_pairs(
        &self,
        studio: &[usize],
        editor: &[usize],
        name: &str,
        approximate: bool,
    ) -> Option<Assignment> {
        let mut targets = AHashMap::new();
        for &target in editor {
            let value = self.editor.properties[target].get(name)?;
            if !charge_value(value, &|n| self.spend(n))
                || targets
                    .insert(field_key(value, approximate), target)
                    .is_some()
            {
                return None;
            }
        }
        let mut roots = Vec::new();
        let mut used = AHashSet::new();
        for &source in studio {
            let value = self.studio.properties[source].get(name)?;
            if !charge_value(value, &|n| self.spend(n)) {
                return None;
            }
            let target = *targets.get(&field_key(value, approximate))?;
            if !used.insert(target) {
                return None;
            }
            roots.push((source, target));
        }
        Some(roots)
    }

    fn group(
        &self,
        studio: &[usize],
        editor: &[usize],
        prepare: &impl Fn(Assignment) -> Option<Assignment>,
    ) -> Option<Assignment> {
        if studio.len() != editor.len() {
            return self.subset_group(studio, editor, prepare);
        }
        // Positional complete subtrees are reserved before searching alternatives.
        let mut positional = Vec::new();
        let mut failed = None;
        for (index, (&s, &e)) in studio.iter().zip(editor).enumerate() {
            let Some(plan) = self.subtree(s, e).and_then(prepare) else {
                failed = Some(index);
                break;
            };
            if !self.spend(plan.len()) {
                return None;
            }
            positional.extend(plan);
        }
        let Some(failed) = failed else {
            return Some(positional);
        };
        if studio.len() > GROUP_LIMIT {
            return None;
        }
        if let Some(plan) = self.keyed_group(studio, editor, prepare) {
            return Some(plan);
        }
        if studio.len() == 2 {
            let mut first = self.subtree(studio[0], editor[1]).and_then(prepare)?;
            first.extend(self.subtree(studio[1], editor[0]).and_then(prepare)?);
            return Some(first);
        }
        // Probe the failed positional member first. A single edited outlier
        // must not force an all-pairs matrix for a large otherwise equal group.
        if !self.spend(studio.len() * editor.len()) {
            return None;
        }
        let mut candidates = vec![Vec::new(); studio.len()];
        for s in std::iter::once(failed).chain((0..studio.len()).filter(|&s| s != failed)) {
            let row = editor
                .iter()
                .map(|&e| self.subtree(studio[s], e).and_then(prepare))
                .collect::<Vec<_>>();
            if row.iter().all(Option::is_none) {
                return None;
            }
            candidates[s] = row;
        }
        if (0..editor.len()).any(|e| candidates.iter().all(|row| row[e].is_none())) {
            return None;
        }
        self.match_candidates(candidates)
    }

    /// A one-sided addition must not shift every following duplicate's slot.
    /// Match every member of the smaller group to a complete unchanged subtree;
    /// unmatched members of the larger group remain independent instances.
    fn subset_group(
        &self,
        studio: &[usize],
        editor: &[usize],
        prepare: &impl Fn(Assignment) -> Option<Assignment>,
    ) -> Option<Assignment> {
        let larger = studio.len().max(editor.len());
        if larger > GROUP_LIMIT {
            return None;
        }
        let positional = studio
            .iter()
            .zip(editor)
            .map(|(&s, &e)| self.subtree(s, e).and_then(prepare))
            .collect::<Option<Vec<_>>>();
        if let Some(positional) = positional {
            return Some(positional.into_iter().flatten().collect());
        }
        if !self.spend(studio.len() * editor.len()) {
            return None;
        }
        let mut candidates = Vec::new();
        for a in 0..studio.len().min(editor.len()) {
            let row = (0..larger)
                .map(|b| {
                    let (s, e) = if studio.len() < editor.len() {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    self.subtree(studio[s], editor[e]).and_then(prepare)
                })
                .collect::<Vec<_>>();
            if row.iter().all(Option::is_none) {
                return None;
            }
            candidates.push(row);
        }
        self.match_candidates(candidates)
    }

    fn match_candidates(&self, candidates: Vec<Vec<Option<Assignment>>>) -> Option<Assignment> {
        if !self.spend(candidates.iter().map(|r| r.len() * 3).sum()) {
            return None;
        }
        let exact = candidates
            .iter()
            .map(|candidates| {
                candidates
                    .iter()
                    .enumerate()
                    .filter_map(|(target, plan)| {
                        plan.as_ref()
                            .filter(|plan| self.exact(plan))
                            .map(|_| target)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let edges = candidates
            .iter()
            .enumerate()
            .map(|(s, candidates)| {
                let mut edges = candidates
                    .iter()
                    .enumerate()
                    .filter_map(|(e, plan)| plan.as_ref().map(|_| e))
                    .collect::<Vec<_>>();
                let mut preferred = vec![false; candidates.len()];
                for &target in &exact[s] {
                    preferred[target] = true;
                }
                edges.sort_by(|&a, &b| {
                    self.spend(1);
                    (!preferred[a], a != s, a).cmp(&(!preferred[b], b != s, b))
                });
                edges
            })
            .collect::<Vec<_>>();
        let assignment = perfect_matching(self, &edges, &exact, candidates[0].len())?;
        if !self.spend(
            assignment
                .iter()
                .enumerate()
                .map(|(s, &e)| candidates[s][e].as_ref().unwrap().len())
                .sum(),
        ) {
            return None;
        }
        Some(
            assignment
                .into_iter()
                .enumerate()
                .flat_map(|(s, e)| candidates[s][e].as_ref().unwrap().iter().copied())
                .collect(),
        )
    }

    fn exact(&self, plan: &Assignment) -> bool {
        self.spend(plan.len())
            && plan.iter().all(|&(source, target)| {
                let a = &self.editor.document.instances[target];
                let b = &self.studio.document.instances[source];
                exact_maps(
                    &self.editor.properties[target],
                    &self.studio.properties[source],
                    &|n| self.spend(n),
                ) && exact_maps(&a.attributes, &b.attributes, &|n| self.spend(n))
            })
    }
}

/// Augmenting paths are necessary: tolerant equality is not an equivalence
/// relation, so overlapping candidate sets cannot be consumed greedily.
fn perfect_matching(
    matcher: &Matcher<'_, '_>,
    edges: &[Vec<usize>],
    exact: &[Vec<usize>],
    targets: usize,
) -> Option<Vec<usize>> {
    fn augment(
        matcher: &Matcher<'_, '_>,
        source: usize,
        edges: &[Vec<usize>],
        seen: &mut [bool],
        owner: &mut [Option<usize>],
    ) -> bool {
        for &target in &edges[source] {
            if !matcher.spend(1) {
                return false;
            }
            if seen[target] {
                continue;
            }
            seen[target] = true;
            if owner[target].is_none_or(|previous| augment(matcher, previous, edges, seen, owner)) {
                owner[target] = Some(source);
                return true;
            }
        }
        false
    }
    if !matcher.spend(targets + edges.len() * 3) {
        return None;
    }
    let mut owner = vec![None; targets];
    let mut exact_order = (0..edges.len()).collect::<Vec<_>>();
    exact_order.sort_by(|&a, &b| {
        matcher.spend(1);
        exact[a].len().cmp(&exact[b].len())
    });
    for source in exact_order {
        if !matcher.spend(targets) {
            return None;
        }
        augment(
            matcher,
            source,
            exact,
            &mut vec![false; targets],
            &mut owner,
        );
    }
    let reserved = owner.iter().flatten().copied().collect::<AHashSet<_>>();
    // Constrained members (including identity locks) get first choice.
    let mut order = (0..edges.len()).collect::<Vec<_>>();
    order.sort_by(|&a, &b| {
        matcher.spend(1);
        edges[a].len().cmp(&edges[b].len())
    });
    for source in order {
        if reserved.contains(&source) {
            continue;
        }
        if !matcher.spend(targets) {
            return None;
        }
        if !augment(
            matcher,
            source,
            edges,
            &mut vec![false; targets],
            &mut owner,
        ) {
            return None;
        }
    }
    let mut assignment = vec![0; edges.len()];
    for (target, source) in owner.into_iter().enumerate() {
        if let Some(source) = source {
            assignment[source] = target;
        }
    }
    Some(assignment)
}

struct Unit {
    studio: Vec<usize>,
    editor: Vec<usize>,
    // Include unselected subtrees: refinement can choose any of these sources.
    sources: Vec<usize>,
    positional: Assignment,
    proposed: Assignment,
    spent: usize,
    fixed: bool,
}

fn subtree_sources(tree: &Tree<'_>, roots: &[usize]) -> Vec<usize> {
    let mut sources = roots.to_vec();
    let mut next = 0;
    while next < sources.len() {
        let source = sources[next];
        next += 1;
        for &group in &tree.children[source] {
            sources.extend(&tree.groups[group]);
        }
    }
    sources
}

pub(crate) struct BudgetConflict {
    pub(crate) count: usize,
    pub(crate) roots: Vec<usize>, // editor roots, for a common parent display path
}

pub(crate) struct Resolved {
    pub(crate) budget_conflicts: Vec<BudgetConflict>,
    pub(crate) budget_sources: Vec<bool>,
    pub(crate) budget_targets: Vec<bool>,
    pub(crate) partners: Vec<Option<usize>>,
    pub(crate) locked: Vec<Option<usize>>,
    pub(crate) proven: Vec<bool>,
    // Only a successful subset proof may retire a preliminary positional guess.
    pub(crate) rejected: Vec<bool>,
    pub(crate) unchanged: Vec<bool>,
    #[cfg(test)]
    pub(crate) recovered: bool,
    reference_values: Vec<(usize, bool, String, Value)>,
    #[cfg(test)]
    pub(crate) reference_comparisons: usize,
    #[cfg(test)]
    pub(crate) record_comparisons: usize,
    #[cfg(test)]
    pub(crate) unbudgeted_comparisons: usize,
}

impl Resolved {
    pub(crate) fn align_references(&self, studio: &mut SettingsBytecode) {
        for (index, attribute, name, value) in &self.reference_values {
            let instance = &mut studio.instances[*index];
            let fields = if *attribute {
                &mut instance.attributes
            } else {
                &mut instance.properties
            };
            fields.insert(name.clone(), value.clone());
        }
    }
}

pub(crate) fn resolve(editor: &SettingsBytecode, studio: &SettingsBytecode) -> Resolved {
    let editor = Tree::new(editor);
    let studio = Tree::new(studio);
    let locked = identity_locks(editor.document, studio.document);
    let mut owners = vec![None; editor.document.instances.len()];
    for (source, target) in locked.iter().enumerate() {
        if let Some(target) = target {
            owners[*target] = Some(source);
        }
    }
    let candidates = Candidates::default();
    let matcher = Matcher {
        budget: None,
        candidates: &candidates,
        editor: &editor,
        studio: &studio,
        locked: &locked,
        owners: &owners,
    };
    // Identity is global: a rename or move does not release its target slot.
    let mut partners = locked.clone();
    let mut proven = vec![false; partners.len()];
    let units = collect_units(&matcher, &mut partners, &mut proven);
    let mut rejected = vec![false; partners.len()];
    let reads = RefCell::default();
    let context = Context::new(&References::new(&matcher, &partners, &reads), &units);
    let completions = resolve_references(
        &matcher,
        &units,
        &context,
        &mut partners,
        &mut proven,
        &mut rejected,
    );
    let publication = finish_publication(
        &matcher,
        &units,
        &context,
        completions,
        &mut partners,
        &mut proven,
        &mut rejected,
    );
    let Publication {
        budget_conflicts,
        budget_sources,
        budget_targets,
        mut reference_values,
        mut unchanged,
        valid,
    } = publication;
    let recovered = fence_resolution(&matcher, &mut partners, &mut proven, valid);
    if recovered {
        rejected.fill(false);
        reference_values.clear();
        unchanged.fill(false);
    }
    assert_eq!(
        partners.iter().flatten().collect::<AHashSet<_>>().len(),
        partners.iter().flatten().count(),
        "first-pairing assignment must be injective"
    );
    assert!(
        locked
            .iter()
            .enumerate()
            .all(|(s, t)| t.is_none() || partners[s] == *t),
        "persistent identity must stay locked"
    );
    Resolved {
        budget_conflicts,
        budget_sources,
        budget_targets,
        partners,
        locked,
        proven,
        reference_values,
        rejected,
        unchanged,
        #[cfg(test)]
        recovered,
        #[cfg(test)]
        reference_comparisons: editor.reference_comparisons.get(),
        #[cfg(test)]
        record_comparisons: candidates.record_comparisons.get(),
        #[cfg(test)]
        unbudgeted_comparisons: candidates.unbudgeted.get(),
    }
}

#[cfg(test)]
fn validate_resolution(
    matcher: &Matcher<'_, '_>,
    partners: &mut [Option<usize>],
    proven: &mut [bool],
) -> bool {
    let proof = partners
        .iter()
        .enumerate()
        .filter_map(|(source, &target)| {
            target
                .filter(|_| proven[source])
                .map(|target| (source, target))
        })
        .collect();
    let valid = References::new(matcher, partners, &RefCell::default()).validates(&proof);
    fence_resolution(matcher, partners, proven, valid)
}

fn fence_resolution(
    matcher: &Matcher<'_, '_>,
    partners: &mut [Option<usize>],
    proven: &mut [bool],
    valid: bool,
) -> bool {
    let injective = partners.iter().flatten().collect::<AHashSet<_>>().len()
        == partners.iter().flatten().count();
    let locks_preserved = matcher
        .locked
        .iter()
        .enumerate()
        .all(|(source, target)| target.is_none() || partners[source] == *target);
    if injective && locks_preserved && valid {
        return false;
    }
    #[cfg(test)]
    RECOVERIES.with(|value| value.set(value.get() + 1));
    eprintln!(
        "[renium] warning: first-pairing assignment failed final identity/reference validation; using locked positional pairing"
    );
    partners.copy_from_slice(matcher.locked);
    proven.fill(false);
    for &group in &matcher.studio.children[partners.len()] {
        let sources = &matcher.studio.groups[group];
        let first = &matcher.studio.document.instances[sources[0]];
        if let Some(other) = matcher.editor.counterpart(None, first) {
            let roots = positional_group(matcher, sources, &matcher.editor.groups[other]);
            apply(&positional_subtrees(matcher, roots), partners);
        }
    }
    true
}

fn identity_locks(editor: &SettingsBytecode, studio: &SettingsBytecode) -> Vec<Option<usize>> {
    let editor_ids = persistent_identity_index(editor);
    let studio_ids = persistent_identity_index(studio);
    studio
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| {
            let id = persistent_identity(instance)?;
            if studio_ids.get(id) != Some(&Some(index)) {
                return None;
            }
            let target = editor_ids.get(id).copied().flatten()?;
            let other = &editor.instances[target];
            (instance.class_name == other.class_name).then_some(target)
        })
        .collect()
}

fn positional_group(matcher: &Matcher<'_, '_>, studio: &[usize], editor: &[usize]) -> Assignment {
    let mut free = editor
        .iter()
        .copied()
        .filter(|&index| matcher.owners[index].is_none());
    studio
        .iter()
        .filter_map(|&source| {
            let target = match matcher.locked[source] {
                Some(target) => editor.binary_search(&target).ok().map(|_| target),
                None => free.next(),
            }?;
            Some((source, target))
        })
        .collect()
}

fn positional_subtrees(matcher: &Matcher<'_, '_>, roots: Assignment) -> Assignment {
    let mut result = roots;
    let mut next = 0;
    while next < result.len() {
        let (source, target) = result[next];
        next += 1;
        for group in &matcher.studio.children[source] {
            let members = &matcher.studio.groups[*group];
            let first = &matcher.studio.document.instances[members[0]];
            if let Some(other) = matcher.editor.counterpart(Some(target), first) {
                result.extend(positional_group(
                    matcher,
                    members,
                    &matcher.editor.groups[other],
                ));
            }
        }
    }
    result
}

fn collect_units(
    matcher: &Matcher<'_, '_>,
    partners: &mut [Option<usize>],
    proven: &mut [bool],
) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut pending = vec![(None, None)];
    while let Some((studio_parent, editor_parent)) = pending.pop() {
        for &group in &matcher.studio.children[studio_parent.unwrap_or(partners.len())] {
            let members = &matcher.studio.groups[group];
            let first = &matcher.studio.document.instances[members[0]];
            let Some(other) = matcher.editor.counterpart(editor_parent, first) else {
                continue;
            };
            let targets = &matcher.editor.groups[other];
            let positional = positional_group(matcher, members, targets);
            if members.len().max(targets.len()) > 1
                && members.len().max(targets.len()) <= GROUP_LIMIT
            {
                // Establish the allowance before the first candidate, including
                // collection/copies. Exhaustion retains a unit for explicit fallback.
                let budget = search::Budget::new(search::WORK_LIMIT);
                let bounded = Matcher {
                    budget: Some(&budget),
                    ..*matcher
                };
                let sources = subtree_sources(matcher.studio, members);
                let positional = positional_subtrees(matcher, positional);
                bounded.spend(sources.len() + positional.len() + members.len() + targets.len());
                // Sides that already agree positionally, the usual state right
                // after a pull, need no candidate matrix: prove each positional
                // root's subtree and accept that assignment outright.
                // The probe may spend at most a slice of the allowance, so a group that
                // needs the matrix (distinct twins, large records) keeps the rest.
                let probe = search::Budget::new(search::WORK_LIMIT / 2);
                let probing = Matcher {
                    budget: Some(&probe),
                    ..*matcher
                };
                let interchangeable = (members.len() == targets.len())
                    .then(|| interchangeable_roots(&probing, members, targets))
                    .flatten();
                bounded.spend(probe.used(search::WORK_LIMIT / 2));
                if let Some(roots) = interchangeable {
                    apply(&roots, partners);
                    units.push(Unit {
                        studio: members.clone(),
                        editor: targets.clone(),
                        sources,
                        positional,
                        proposed: roots,
                        spent: budget.used(search::WORK_LIMIT),
                        fixed: false,
                    });
                    continue;
                }
                let proposed = bounded.group(members, targets, &Some);
                if proposed.is_some() || budget.exhausted.get() {
                    let proposed = proposed.unwrap_or_default();
                    apply(
                        if budget.exhausted.get() {
                            &positional
                        } else {
                            &proposed
                        },
                        partners,
                    );
                    units.push(Unit {
                        studio: members.clone(),
                        editor: targets.clone(),
                        sources,
                        positional,
                        proposed,
                        spent: budget.used(search::WORK_LIMIT),
                        fixed: false,
                    });
                    continue;
                }
                if let Some(roots) = bounded.distinct_roots(members, targets) {
                    apply(&roots, partners);
                    pending.extend(roots.iter().map(|&(s, e)| (Some(s), Some(e))));
                    units.push(Unit {
                        studio: members.clone(),
                        editor: targets.clone(),
                        sources: members.clone(),
                        positional: positional_group(matcher, members, targets),
                        proposed: roots,
                        spent: budget.used(search::WORK_LIMIT),
                        fixed: true,
                    });
                    continue;
                }
                if budget.exhausted.get() {
                    apply(&positional, partners);
                    units.push(Unit {
                        studio: members.clone(),
                        editor: targets.clone(),
                        sources,
                        proposed: Vec::new(),
                        positional,
                        spent: search::WORK_LIMIT + 1,
                        fixed: false,
                    });
                    continue;
                }
                // A genuine content mismatch uses ordinary positional traversal.
                // Retain its accounting, but do not claim a complete subtree proof.
                let roots = positional_group(matcher, members, targets);
                apply(&roots, partners);
                pending.extend(roots.iter().map(|&(s, e)| (Some(s), Some(e))));
                units.push(Unit {
                    studio: members.clone(),
                    editor: targets.clone(),
                    sources: members.clone(),
                    positional: roots,
                    proposed: Vec::new(),
                    spent: budget.used(search::WORK_LIMIT),
                    fixed: true,
                });
                continue;
            }
            apply(&positional, partners);
            pending.extend(positional.into_iter().map(|(s, e)| (Some(s), Some(e))));
        }
    }
    // Proofs are published only after references and incoming constraints agree.
    proven.fill(false);
    units
}

/// The complete subtree assignment for a duplicate group whose members all
/// have an equal counterpart, found without a candidate matrix: first by
/// position (the usual state right after a pull), otherwise each member takes
/// the first unclaimed counterpart whose record and subtree prove equal.
/// Identical twins are interchangeable, so either bijection is as good as any
/// the matrix finds; the proofs stay under the group's budget and respect
/// identity locks, and the reference stage still validates the plan.
fn interchangeable_roots(
    matcher: &Matcher<'_, '_>,
    members: &[usize],
    targets: &[usize],
) -> Option<Assignment> {
    let positional = members
        .iter()
        .zip(targets)
        .map(|(&source, &target)| matcher.subtree(source, target))
        .collect::<Option<Vec<_>>>();
    if let Some(plans) = positional {
        return Some(plans.into_iter().flatten().collect());
    }
    let mut claimed = vec![false; targets.len()];
    let mut plan = Vec::new();
    for &source in members {
        let found = targets.iter().enumerate().find_map(|(index, &target)| {
            if claimed[index] || !matcher.spend(1) || !matcher.record(source, target) {
                return None;
            }
            matcher
                .subtree(source, target)
                .map(|subtree| (index, subtree))
        });
        let (index, subtree) = found?;
        claimed[index] = true;
        plan.extend(subtree);
    }
    Some(plan)
}

fn apply(assignment: &Assignment, partners: &mut [Option<usize>]) {
    for &(source, target) in assignment {
        partners[source] = Some(target);
    }
}

fn publish_unit(
    unit: &Unit,
    assignment: &Assignment,
    partners: &mut [Option<usize>],
    locked: &[Option<usize>],
) {
    // Positional fallback can have fewer paired descendants (different classes
    // or shapes). Clear the entire tentative subtree before restoring its slots.
    for &source in &unit.sources {
        partners[source] = locked[source];
    }
    apply(assignment, partners);
}

struct References<'a, 'd> {
    matcher: &'a Matcher<'a, 'd>,
    partners: &'a [Option<usize>],
    scope: Option<&'a ReferenceScope<'a>>,
    interchangeable: bool,
    incoming: Option<&'a Assignment>,
    context: Option<&'a Context>,
    availability_reads: Option<&'a RefCell<AHashSet<usize>>>,
    // Only search propagation uses unknown bindings; complete proofs omit this.
    pending: Option<&'a AHashSet<usize>>,
    claimed: Option<&'a AHashSet<usize>>,
    // Shared through nested candidate/content scopes, including roots outside units.
    reads: &'a RefCell<AHashSet<usize>>,
    // Directed answers are valid only while partners and scope are immutable.
    // Never merge tolerant matches into transitive equivalence classes.
    equivalent: RefCell<AHashMap<(usize, usize), bool>>,
}

struct ReferenceScope<'a> {
    // None shadows a displaced source; it must not use a stale outer partner.
    targets: AHashMap<usize, Option<usize>>,
    parent: Option<&'a ReferenceScope<'a>>,
}

impl ReferenceScope<'_> {
    fn replace(&mut self, unit: &Unit, assignment: &Assignment, locked: &[Option<usize>]) {
        for &source in &unit.sources {
            self.targets
                .insert(source, locked.get(source).copied().flatten());
        }
        self.targets
            .extend(assignment.iter().map(|&(s, e)| (s, Some(e))));
    }

    fn target(&self, source: usize) -> Option<Option<usize>> {
        self.targets
            .get(&source)
            .copied()
            .or_else(|| self.parent.and_then(|parent| parent.target(source)))
    }
}

impl<'a, 'd> References<'a, 'd> {
    fn new(
        matcher: &'a Matcher<'a, 'd>,
        partners: &'a [Option<usize>],
        reads: &'a RefCell<AHashSet<usize>>,
    ) -> Self {
        Self {
            matcher,
            partners,
            scope: None,
            interchangeable: true,
            incoming: None,
            context: None,
            availability_reads: None,
            pending: None,
            claimed: None,
            reads,
            equivalent: RefCell::default(),
        }
    }

    fn in_scope<'s>(&'s self, scope: &'s ReferenceScope<'s>) -> References<'s, 'd> {
        References {
            matcher: self.matcher,
            partners: self.partners,
            scope: Some(scope),
            interchangeable: self.interchangeable,
            incoming: self.incoming,
            context: self.context,
            availability_reads: self.availability_reads,
            pending: self.pending,
            claimed: self.claimed,
            reads: self.reads,
            equivalent: RefCell::default(),
        }
    }

    fn target(&self, source: usize) -> Option<usize> {
        self.reads.borrow_mut().insert(source);
        self.scope
            .and_then(|scope| scope.target(source))
            .unwrap_or(self.partners[source])
    }

    /// Resolve descendants in a candidate-relative scope. In particular a model's
    /// PrimaryPart never depends on that model's old ordinal on the other side.
    fn refine(&self, mut plan: Assignment) -> Option<Assignment> {
        if !self.matcher.spend(plan.len()) {
            return None;
        }
        if plan.iter().all(|&(s, e)| {
            self.matcher.studio.references[s].is_empty()
                && self.matcher.editor.references[e].is_empty()
        }) {
            return Some(plan);
        }
        if self.validates(&plan) {
            return Some(plan);
        }
        let (source, target) = plan[0];
        let mut scope = ReferenceScope {
            targets: plan.iter().map(|&(s, e)| (s, Some(e))).collect(),
            parent: self.scope,
        };
        let mut units = Vec::new();
        for group in &self.matcher.studio.children[source] {
            let members = &self.matcher.studio.groups[*group];
            let first = &self.matcher.studio.document.instances[members[0]];
            let other = self.matcher.editor.counterpart(Some(target), first)?;
            let mut pending = members.clone();
            let mut proposed = Vec::new();
            while let Some(source) = pending.pop() {
                proposed.push((source, scope.targets[&source]?));
                for &group in &self.matcher.studio.children[source] {
                    pending.extend(&self.matcher.studio.groups[group]);
                }
            }
            if !self
                .matcher
                .spend(members.len() + self.matcher.editor.groups[other].len() + proposed.len() * 2)
            {
                return None;
            }
            units.push(Unit {
                studio: members.clone(),
                editor: self.matcher.editor.groups[other].clone(),
                sources: subtree_sources(self.matcher.studio, members),
                positional: Vec::new(),
                proposed,
                spent: 0,
                fixed: false,
            });
        }
        for (_, children, _) in self.in_scope(&scope).resolve_units(&units) {
            let search::Outcome::Paired(children) = children else {
                return None;
            };
            scope
                .targets
                .extend(children.into_iter().map(|(s, e)| (s, Some(e))));
        }
        for (source, target) in &mut plan {
            *target = scope.targets[source]?;
        }
        self.validates(&plan).then_some(plan)
    }

    /// Newly observed binding dependencies trigger a fresh dependency order,
    /// before publishing any proof. The graph only grows, so cycles converge to
    /// joint components rather than discarding already-correct assignments.
    fn resolve_units(&self, units: &[Unit]) -> Vec<(Vec<usize>, search::Outcome, usize)> {
        let context = Context::local(self.matcher, units);
        self.resolve_units_with(units, &context)
    }

    fn resolve_units_with(
        &self,
        units: &[Unit],
        context: &Context,
    ) -> Vec<(Vec<usize>, search::Outcome, usize)> {
        let owner = &context.owner;
        let mut dependencies = context.dependencies.clone();
        self.matcher.spend(context.work.iter().sum());
        // Retain charged work across dependency expansion and component merges.
        let mut spent = units
            .iter()
            .enumerate()
            .map(|(i, unit)| unit.spent.saturating_add(context.work[i]))
            .collect::<Vec<_>>();
        loop {
            let mut scope = ReferenceScope {
                targets: AHashMap::new(),
                parent: self.scope,
            };
            for (index, unit) in units.iter().enumerate() {
                let work =
                    unit.sources.len() + unit.proposed.len() + dependencies[index].len() * 3 + 1;
                spent[index] = spent[index].saturating_add(work);
                self.matcher.spend(work);
                scope.replace(unit, &unit.proposed, self.matcher.locked);
            }
            let mut results = Vec::new();
            let mut expanded = false;
            for component in reference_components(&dependencies) {
                let reads = RefCell::default();
                let mut references = self.in_scope(&scope);
                references.reads = &reads;
                references.context = Some(context);
                let available = search::WORK_LIMIT
                    .saturating_sub(component.iter().map(|&index| spent[index]).sum::<usize>());
                let (proposal, used) = references.budgeted_component(units, &component, available);
                spent[component[0]] += used;
                let reads = reads.into_inner();
                self.reads.borrow_mut().extend(&reads);
                let index = component[0];
                spent[index] = spent[index].saturating_add(reads.len());
                for dependency in reads.iter().filter_map(|source| owner.get(source)).copied() {
                    if component.binary_search(&dependency).is_err() {
                        expanded |= dependencies[index].insert(dependency);
                        expanded |= dependencies[dependency].insert(index);
                    }
                }
                if expanded {
                    break;
                }
                for &index in &component {
                    let unit = &units[index];
                    let assignment = if proposal.plan().is_some() {
                        &Vec::new()
                    } else {
                        &unit.positional
                    };
                    scope.replace(unit, assignment, self.matcher.locked);
                }
                if let Some(proposal) = proposal.plan() {
                    scope
                        .targets
                        .extend(proposal.iter().map(|&(s, e)| (s, Some(e))));
                }
                let work = component
                    .iter()
                    .map(|&i| units[i].sources.len())
                    .sum::<usize>()
                    + reads.len();
                spent[component[0]] = spent[component[0]].saturating_add(work);
                self.matcher.spend(work);
                let used = component.iter().map(|&i| spent[i]).sum::<usize>();
                let proposal = if used > search::WORK_LIMIT {
                    search::Outcome::Exhausted
                } else {
                    proposal
                };
                results.push((component, proposal, search::WORK_LIMIT.saturating_sub(used)));
            }
            if !expanded {
                return results;
            }
        }
    }

    fn budgeted_component(
        &self,
        units: &[Unit],
        component: &[usize],
        available: usize,
    ) -> (search::Outcome, usize) {
        let budget = search::Budget::new(available);
        let shared = self.matcher.budget.unwrap_or(&budget);
        let matcher = Matcher {
            budget: Some(shared),
            ..*self.matcher
        };
        let mut references = References::new(&matcher, self.partners, self.reads);
        references.scope = self.scope;
        references.interchangeable = self.interchangeable;
        references.incoming = self.incoming;
        references.context = self.context;
        references.availability_reads = self.availability_reads;
        references.pending = self.pending;
        references.claimed = self.claimed;
        let before = shared.remaining();
        let plan = if shared.spend(1) {
            references.resolve_component(units, component)
        } else {
            None
        };
        let outcome = if let Some(plan) = plan {
            search::Outcome::Paired(plan)
        } else if !shared.exhausted.get() {
            references.partial_component(units, component)
        } else {
            search::Outcome::Exhausted
        };
        let outcome = if shared.exhausted.get() {
            search::Outcome::Exhausted
        } else {
            outcome
        };
        let used = before - shared.remaining() + usize::from(shared.exhausted.get());
        (outcome, used)
    }

    fn partial_component(&self, units: &[Unit], component: &[usize]) -> search::Outcome {
        let work = component
            .iter()
            .map(|&i| units[i].sources.len())
            .sum::<usize>();
        if !self.matcher.spend(work * 2) {
            return search::Outcome::Exhausted;
        }
        let owner = component
            .iter()
            .flat_map(|&i| units[i].sources.iter().map(move |&s| (s, i)))
            .collect();
        let dependencies = reference_dependencies(self.matcher, units, &owner);
        let components = reference_components(&dependencies)
            .into_iter()
            .filter(|xs| component.binary_search(&xs[0]).is_ok())
            .collect::<Vec<_>>();
        if components.len() == 1 {
            return search::Outcome::NoCounterpart;
        }
        let mut scope = ReferenceScope {
            targets: AHashMap::new(),
            parent: self.scope,
        };
        let mut plan = Vec::new();
        let mut proven = AHashSet::new();
        for part in components {
            let proposal = self.in_scope(&scope).resolve_component(units, &part);
            if let Some(proposal) = proposal {
                if !self.matcher.spend(proposal.len()) {
                    return search::Outcome::Exhausted;
                }
                proven.extend(proposal.iter().map(|&(s, _)| s));
                for &i in &part {
                    scope.replace(&units[i], &Vec::new(), self.matcher.locked);
                }
                scope
                    .targets
                    .extend(proposal.iter().map(|&(s, e)| (s, Some(e))));
                plan.extend(proposal);
            } else {
                for &i in &part {
                    let unit = &units[i];
                    if !self
                        .matcher
                        .spend(unit.sources.len() + unit.positional.len())
                    {
                        return search::Outcome::Exhausted;
                    }
                    scope.replace(unit, &unit.positional, self.matcher.locked);
                    plan.extend(&unit.positional);
                }
            }
        }
        let selected = plan
            .iter()
            .copied()
            .filter(|(s, _)| proven.contains(s))
            .collect();
        if proven.is_empty() || !self.in_scope(&scope).validates(&selected) {
            search::Outcome::NoCounterpart
        } else {
            search::Outcome::Partial { plan, proven }
        }
    }

    /// Include paired referrers outside the selected roots, including references
    /// to descendants. Sources in another undecided unit join the same component.
    fn outside(&self, _units: &[Unit], component: &[usize]) -> Assignment {
        let Some(context) = self.context else {
            return Vec::new();
        };
        let mut result = Assignment::new();
        for &i in component {
            if !self.matcher.spend(context.incoming[i].len()) {
                break;
            }
            result.extend(&context.incoming[i]);
        }
        result.sort_unstable_by(|a, b| {
            self.matcher.spend(1);
            a.cmp(b)
        });
        result.dedup();
        result
    }

    fn complete(&self, plan: &Assignment) -> bool {
        self.matcher.spend(plan.len())
            && self.validates(plan)
            && self.incoming.is_none_or(|incoming| {
                if incoming.is_empty() {
                    return true;
                }
                if !self.matcher.spend(plan.len() + incoming.len()) {
                    return false;
                }
                if plan.iter().all(|&(s, e)| {
                    self.scope
                        .and_then(|scope| scope.target(s))
                        .unwrap_or(self.partners[s])
                        == Some(e)
                }) {
                    return self.validates_fields(incoming);
                }
                let scope = ReferenceScope {
                    targets: plan.iter().map(|&(s, e)| (s, Some(e))).collect(),
                    parent: self.scope,
                };
                self.in_scope(&scope).validates_fields(incoming)
            })
    }

    /// Prefer exact reference correspondences, then allow proven interchangeable
    /// targets. Both searches use the same allowance and complete leaf proof.
    fn resolve_component(&self, units: &[Unit], component: &[usize]) -> Option<Assignment> {
        if component.iter().any(|&i| units[i].proposed.is_empty()) {
            return None;
        }
        if component.len() == 1 && units[component[0]].fixed {
            return Some(units[component[0]].proposed.clone());
        }
        let work = component
            .iter()
            .map(|&i| units[i].sources.len() + units[i].proposed.len())
            .sum();
        if !self.matcher.spend(work) {
            return None;
        }
        let incoming = self.outside(units, component);
        let mut references = self.in_scope(self.scope?);
        references.incoming = Some(&incoming);
        let plan = component
            .iter()
            .flat_map(|&i| units[i].proposed.iter().copied())
            .collect::<Assignment>();
        if references.complete(&plan) {
            return Some(plan);
        }
        search::resolve(&references, units, component)
    }

    fn unknown(&self, source: usize) -> bool {
        self.pending
            .is_some_and(|pending| pending.contains(&source))
            && self.target(source).is_none()
    }

    fn available(&self, target: usize) -> bool {
        if let Some(reads) = self.availability_reads {
            reads.borrow_mut().insert(target);
        }
        self.claimed
            .is_none_or(|claimed| !claimed.contains(&target))
    }

    fn value_equal(&self, a: &Value, b: &Value) -> bool {
        if !self.matcher.spend(1) {
            return false;
        }
        match (self.matcher.editor.target(a), self.matcher.studio.target(b)) {
            (Some(target), Some(source)) => {
                let mapped = self.target(source);
                let unknown = mapped.is_none()
                    && self
                        .pending
                        .is_some_and(|pending| pending.contains(&source));
                mapped == Some(target)
                    || unknown && self.available(target)
                    || (mapped.is_some() || unknown)
                        && self.interchangeable
                        && self.indistinguishable(target, source)
            }
            _ => reconciliation_values_equal(a, b, false),
        }
    }

    fn indistinguishable(&self, target: usize, source: usize) -> bool {
        if self.parents_match(target, source)
            && self
                .matcher
                .candidates
                .plain_subtrees
                .borrow()
                .contains(&(source, target))
        {
            return self.matcher.spend(1);
        }
        if let Some(&equal) = self.equivalent.borrow().get(&(target, source)) {
            return equal;
        }
        #[cfg(test)]
        self.matcher
            .editor
            .reference_comparisons
            .set(self.matcher.editor.reference_comparisons.get() + 1);
        let equal = self.compare_subtree(target, source);
        if self.matcher.spend(0) {
            self.equivalent.borrow_mut().insert((target, source), equal);
        }
        equal
    }

    fn parents_match(&self, target: usize, source: usize) -> bool {
        match (
            self.matcher.editor.document.instances[target].parent_index,
            self.matcher.studio.document.instances[source].parent_index,
        ) {
            (Some(target), Some(source)) => {
                self.unknown(source) && self.available(target)
                    || self.target(source) == Some(target)
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn compare_subtree(&self, target: usize, source: usize) -> bool {
        let a = &self.matcher.editor.document.instances[target];
        let b = &self.matcher.studio.document.instances[source];
        if !self.parents_match(target, source) || a.name != b.name || a.class_name != b.class_name {
            return false;
        }
        // Content proof deliberately ignores identity locks, without changing
        // the published identity assignment. Compare the actual Studio subtree.
        let matcher = Matcher {
            budget: self.matcher.budget,
            candidates: self.matcher.candidates,
            editor: self.matcher.editor,
            studio: self.matcher.studio,
            locked: &[],
            owners: &[],
        };
        let Some(plan) = matcher.subtree(source, target) else {
            return false;
        };
        if !matcher.spend(plan.len()) {
            return false;
        }
        if plan.iter().all(|&(s, e)| {
            matcher.studio.references[s].is_empty() && matcher.editor.references[e].is_empty()
        }) {
            matcher
                .candidates
                .plain_subtrees
                .borrow_mut()
                .insert((source, target));
            return true;
        }
        let references = References {
            matcher: &matcher,
            partners: self.partners,
            scope: self.scope,
            // Internal references use the candidate-local assignment; external
            // references require the established target, not a recursive guess.
            interchangeable: false,
            incoming: None,
            context: self.context,
            availability_reads: self.availability_reads,
            pending: self.pending,
            claimed: self.claimed,
            reads: self.reads,
            equivalent: RefCell::default(),
        };
        references.refine(plan).is_some()
    }

    fn validates(&self, assignment: &Assignment) -> bool {
        if !self.matcher.spend(assignment.len()) {
            return false;
        }
        let mut sources = AHashSet::with_capacity(assignment.len());
        let mut targets = AHashSet::with_capacity(assignment.len());
        if !assignment.iter().all(|&(source, target)| {
            sources.insert(source) && targets.insert(target) && self.matcher.allowed(source, target)
        }) {
            return false;
        }
        if assignment.iter().all(|&(source, target)| {
            self.matcher.studio.references[source].is_empty()
                && self.matcher.editor.references[target].is_empty()
        }) {
            return true;
        }
        if assignment.iter().all(|&(s, e)| {
            self.scope
                .and_then(|scope| scope.target(s))
                .unwrap_or(self.partners[s])
                == Some(e)
        }) {
            return self.validates_fields(assignment);
        }
        if !self.matcher.spend(assignment.len()) {
            return false;
        }
        let scope = ReferenceScope {
            targets: assignment.iter().map(|&(s, e)| (s, Some(e))).collect(),
            parent: self.scope,
        };
        self.in_scope(&scope).validates_fields(assignment)
    }

    fn validates_fields(&self, assignment: &Assignment) -> bool {
        assignment.iter().all(|&(source, target)| {
            self.matcher.locked_pair(source, target) || self.record_references_equal(source, target)
        })
    }

    fn record_references_equal(&self, source: usize, target: usize) -> bool {
        if !self.matcher.spend(1) {
            return false;
        }
        self.matcher.studio.references[source].is_empty()
            && self.matcher.editor.references[target].is_empty()
            || self.matcher.spend(
                self.matcher.studio.reference_work[source]
                    + self.matcher.editor.reference_work[target],
            ) && reference_fields_equal(
                self.matcher.editor,
                target,
                self.matcher.studio,
                source,
                &|a, b| self.value_equal(a, b),
            )
    }
}

fn reference_fields_equal(
    editor: &Tree<'_>,
    target: usize,
    studio: &Tree<'_>,
    source: usize,
    equal: &impl Fn(&Value, &Value) -> bool,
) -> bool {
    editor.reference_paths[target] == studio.reference_paths[source]
        && editor.references[target]
            .iter()
            .zip(&studio.references[source])
            .all(|(a, b)| equal(a, b))
}

/// Initial edges are cheap direct references. Proofs add all other bindings they
/// actually read, including references reached through subtrees outside units.
fn reference_dependencies(
    matcher: &Matcher<'_, '_>,
    units: &[Unit],
    owner: &AHashMap<usize, usize>,
) -> Vec<AHashSet<usize>> {
    let mut dependencies = vec![AHashSet::new(); units.len()];
    if !matcher.spend(units.len()) {
        return dependencies;
    }
    for (index, unit) in units.iter().enumerate() {
        if unit.fixed || !unit.sources.first().is_some_and(|s| owner.contains_key(s)) {
            continue;
        }
        for &source in &unit.sources {
            if !matcher.spend(1 + matcher.studio.references[source].len()) {
                return dependencies;
            }
            dependencies[index].extend(
                matcher.studio.references[source]
                    .iter()
                    .filter_map(|v| matcher.studio.target(v))
                    .filter_map(|s| owner.get(&s).copied())
                    .filter(|&other| other != index),
            );
        }
    }
    dependencies
}

/// Strongly connected components, ordered targets before their dependents.
fn reference_components(dependencies: &[AHashSet<usize>]) -> Vec<Vec<usize>> {
    let mut dependents = vec![Vec::new(); dependencies.len()];
    for (index, dependencies) in dependencies.iter().enumerate() {
        for &target in dependencies {
            dependents[target].push(index);
        }
    }
    // Iterative Kosaraju avoids recursion proportional to document size.
    let mut seen = vec![false; dependencies.len()];
    let mut order = Vec::new();
    for start in 0..dependencies.len() {
        let mut pending = vec![(start, false)];
        while let Some((index, finished)) = pending.pop() {
            if finished {
                order.push(index);
            } else if !seen[index] {
                seen[index] = true;
                pending.push((index, true));
                pending.extend(dependents[index].iter().map(|&next| (next, false)));
            }
        }
    }
    let mut components = Vec::new();
    seen.fill(false);
    for start in order.into_iter().rev() {
        let mut component = Vec::new();
        let mut pending = vec![start];
        while let Some(index) = pending.pop() {
            if !seen[index] {
                seen[index] = true;
                component.push(index);
                pending.extend(&dependencies[index]);
            }
        }
        if !component.is_empty() {
            component.sort_unstable();
            components.push(component);
        }
    }
    components
}

struct Completion {
    component: Vec<usize>,
    remaining: usize,
    exhausted: bool,
    paired: bool,
}

fn resolve_references(
    matcher: &Matcher<'_, '_>,
    units: &[Unit],
    context: &Context,
    partners: &mut [Option<usize>],
    proven: &mut [bool],
    rejected: &mut [bool],
) -> Vec<Completion> {
    let reads = RefCell::default();
    let proposals = References::new(matcher, partners, &reads).resolve_units_with(units, context);
    let mut completions = Vec::new();
    for (component, proposal, remaining) in proposals {
        let work = component
            .iter()
            .map(|&i| units[i].sources.len() * 2)
            .sum::<usize>()
            + proposal.plan().map_or(0, Vec::len);
        let exhausted = matches!(proposal, search::Outcome::Exhausted) || work > remaining;
        let paired = !exhausted && proposal.plan().is_some();
        if let Some(plan) = proposal.plan().filter(|_| !exhausted) {
            for &index in &component {
                publish_unit(&units[index], &Vec::new(), partners, matcher.locked);
            }
            apply(plan, partners);
            for &(source, _) in plan {
                proven[source] = match &proposal {
                    search::Outcome::Partial { proven, .. } => proven.contains(&source),
                    _ => true,
                };
            }
            for &index in &component {
                if units[index].proposed.iter().any(|&(s, _)| proven[s]) {
                    for &source in &units[index].sources {
                        rejected[source] = partners[source].is_none();
                    }
                }
            }
        } else {
            for &index in &component {
                publish_unit(
                    &units[index],
                    &units[index].positional,
                    partners,
                    matcher.locked,
                );
            }
        }
        completions.push(Completion {
            component,
            remaining: remaining.saturating_sub(work),
            exhausted,
            paired,
        });
    }
    completions
}

struct Publication {
    budget_conflicts: Vec<BudgetConflict>,
    budget_sources: Vec<bool>,
    budget_targets: Vec<bool>,
    reference_values: Vec<(usize, bool, String, Value)>,
    unchanged: Vec<bool>,
    valid: bool,
}

fn finish_publication(
    matcher: &Matcher<'_, '_>,
    units: &[Unit],
    context: &Context,
    completions: Vec<Completion>,
    partners: &mut [Option<usize>],
    proven: &mut [bool],
    rejected: &mut [bool],
) -> Publication {
    let mut result = Publication {
        budget_conflicts: Vec::new(),
        budget_sources: vec![false; partners.len()],
        budget_targets: vec![false; matcher.editor.document.instances.len()],
        reference_values: Vec::new(),
        unchanged: vec![false; partners.len()],
        valid: true,
    };
    let mut visited = vec![false; partners.len()];
    for completion in completions {
        let budget = search::Budget::new(completion.remaining);
        if completion.exhausted {
            budget.spend(completion.remaining + 1);
        }
        let bounded = Matcher {
            budget: Some(&budget),
            ..*matcher
        };
        let reads = RefCell::default();
        let mut references = References::new(&bounded, partners, &reads);
        references.context = Some(context);
        let incoming = if completion.component.iter().all(|&i| units[i].fixed) {
            Vec::new()
        } else {
            references.outside(units, &completion.component)
        };
        let proved = completion
            .component
            .iter()
            .copied()
            .filter(|&i| !units[i].fixed && units[i].sources.iter().any(|&s| proven[s]))
            .collect::<Vec<_>>();
        let proof_incoming = references.outside(units, &proved);
        references.incoming = Some(&proof_incoming);
        let mut sources = completion
            .component
            .iter()
            .flat_map(|&i| units[i].sources.iter().copied())
            .collect::<Vec<_>>();
        sources.extend(incoming.iter().map(|&(s, _)| s));
        sources.sort_unstable_by(|a, b| {
            bounded.spend(1);
            a.cmp(b)
        });
        sources.dedup();
        bounded.spend(sources.len());
        let proof = sources
            .iter()
            .filter(|&&s| proven[s])
            .filter_map(|&s| partners[s].map(|e| (s, e)))
            .collect();
        let valid = !completion.paired || references.complete(&proof);

        let (values, unchanged) = publish_records(&references, &sources, valid.then_some(&*proven));

        for &source in &sources {
            visited[source] = true;
        }
        if budget.exhausted.get() {
            let mut roots = Vec::new();
            let mut count = 0;
            for &i in &completion.component {
                let unit = &units[i];
                count += unit.studio.len().max(unit.editor.len());
                roots.extend(&unit.editor);
                publish_unit(unit, &unit.positional, partners, matcher.locked);
                for &source in &unit.sources {
                    proven[source] = false;
                    rejected[source] = false;
                }
                for target in subtree_sources(matcher.editor, &unit.editor) {
                    result.budget_targets[target] = true;
                }
            }
            for source in sources {
                result.budget_sources[source] = true;
            }
            result.exhausted(count, roots);
        } else {
            result.valid &= valid;
            result.reference_values.extend(values);
            for (source, same) in unchanged {
                result.unchanged[source] = same;
            }
        }
    }
    // Ordinary paired records also use bounded reference-equivalence proofs.
    // Each independent record is its own publication component; references into
    // twin components were included above, with their owning allowance.
    for source in 0..partners.len() {
        if visited[source] || partners[source].is_none() {
            continue;
        }
        let budget = search::Budget::new(search::WORK_LIMIT);
        let bounded = Matcher {
            budget: Some(&budget),
            ..*matcher
        };
        let reads = RefCell::default();
        let references = References::new(&bounded, partners, &reads);
        let (values, unchanged) = publish_records(&references, &[source], None);
        if budget.exhausted.get() {
            result.budget_sources[source] = true;
            let target = partners[source].unwrap();
            result.budget_targets[target] = true;
            result.exhausted(1, vec![target]);
        } else {
            result.reference_values.extend(values);
            result.unchanged[source] = unchanged[0].1;
        }
    }
    result
}

impl Publication {
    fn exhausted(&mut self, count: usize, roots: Vec<usize>) {
        eprintln!(
            "[renium] first-pairing comparison budget exhausted ({}, {} siblings); using positional pairing",
            search::WORK_LIMIT,
            count
        );
        self.budget_conflicts.push(BudgetConflict { count, roots });
    }
}

type ReferenceValues = Vec<(usize, bool, String, Value)>;

fn publish_records(
    references: &References<'_, '_>,
    sources: &[usize],
    verified: Option<&[bool]>,
) -> (ReferenceValues, Vec<(usize, bool)>) {
    let unchanged = sources
        .iter()
        .map(|&source| {
            let same = references.partners[source].is_some_and(|target| {
                references.matcher.record(source, target)
                    && (verified.is_some_and(|proof| proof[source])
                        && !references.matcher.locked_pair(source, target)
                        || references.record_references_equal(source, target))
            });
            (source, same)
        })
        .collect();
    (equivalent_reference_values(references, sources), unchanged)
}

fn equivalent_reference_values(
    references: &References<'_, '_>,
    sources: &[usize],
) -> Vec<(usize, bool, String, Value)> {
    let matcher = references.matcher;
    let partners = references.partners;
    let mut result = Vec::new();
    for &source in sources {
        if !matcher.spend(1) {
            break;
        }
        let Some(target) = partners[source] else {
            continue;
        };
        let a = &matcher.editor.document.instances[target];
        let b = &matcher.studio.document.instances[source];
        if a.class_name == "PackageLink" || matcher.editor.references[target].is_empty() {
            continue;
        }
        for &(attribute, name) in &matcher.editor.reference_fields[target] {
            let (left, right) = if attribute {
                (&a.attributes, &b.attributes)
            } else {
                (&a.properties, &b.properties)
            };
            let value = &left[name];
            let Some(other) = right.get(name) else {
                continue;
            };
            if !(0..3).all(|_| charge_value(value, &|n| matcher.spend(n)))
                || !(0..2).all(|_| charge_value(other, &|n| matcher.spend(n)))
            {
                break;
            }
            if reference_shapes_equal(value, other)
                && references_equal(Some(value), Some(other), &|a, b| {
                    references.value_equal(a, b)
                })
            {
                result.push((source, attribute, name.to_owned(), value.clone()));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn cache_document() -> SettingsBytecode {
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: [
                ("Workspace", "Workspace"),
                ("Left", "Part"),
                ("Right", "Part"),
                ("Twin", "ObjectValue"),
                ("Twin", "ObjectValue"),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (name, class))| {
                SettingsBytecodeInstance::new(
                    index.to_string(),
                    name.into(),
                    class.into(),
                    (index != 0).then_some(0),
                )
            })
            .collect(),
        };
        document.instances[3]
            .properties
            .insert("Value".into(), json!({"_type":"Ref", "settingsId":"1"}));
        document.instances[4]
            .properties
            .insert("Value".into(), json!({"_type":"Ref", "settingsId":"2"}));
        document
    }

    #[test]
    fn directed_reference_cache_expires_with_the_assignment_scope() {
        let document = cache_document();
        let editor = Tree::new(&document);
        let studio = Tree::new(&document);
        let candidates = Candidates::default();
        let matcher = Matcher {
            budget: None,
            candidates: &candidates,
            editor: &editor,
            studio: &studio,
            locked: &[],
            owners: &[],
        };
        let partners = (0..5).map(Some).collect::<Vec<_>>();
        let reads = RefCell::default();
        let references = References::new(&matcher, &partners, &reads);
        assert!(!references.indistinguishable(3, 4));
        assert!(!references.indistinguishable(3, 4));
        assert_eq!(editor.reference_comparisons.get(), 1);

        let swapped = ReferenceScope {
            targets: [(1, Some(2)), (2, Some(1))].into_iter().collect(),
            parent: None,
        };
        let candidate = references.in_scope(&swapped);
        assert!(candidate.indistinguishable(3, 4));
        assert!(candidate.indistinguishable(3, 4));
        assert!(candidate.indistinguishable(4, 3));
        assert_eq!(
            candidate.equivalent.borrow().len(),
            2,
            "cache keys are directed pairs"
        );
        assert_eq!(editor.reference_comparisons.get(), 3);

        let restored = ReferenceScope {
            targets: [(1, Some(1)), (2, Some(2))].into_iter().collect(),
            parent: None,
        };
        assert!(!references.in_scope(&restored).indistinguishable(3, 4));
        assert_eq!(editor.reference_comparisons.get(), 4);

        let displaced = ReferenceScope {
            targets: [(1, None)].into_iter().collect(),
            parent: Some(&swapped),
        };
        assert_eq!(references.in_scope(&displaced).target(1), None);
        assert_eq!(references.in_scope(&displaced).target(2), Some(1));
        assert_eq!(references.in_scope(&displaced).target(0), Some(0));

        // Exercise the last publication fence independently of candidate search.
        let locked = vec![None, Some(1), None, None, None];
        let owners = vec![None, Some(1), None, None, None];
        let matcher = Matcher {
            budget: None,
            locked: &locked,
            owners: &owners,
            ..matcher
        };
        for mut invalid in [
            vec![Some(0), Some(1), Some(1), Some(3), Some(4)],
            vec![Some(0), Some(1), Some(2), Some(4), Some(3)],
        ] {
            let mut proven = vec![true; 5];
            validate_resolution(&matcher, &mut invalid, &mut proven);
            assert_eq!(invalid, partners);
            assert!(proven.iter().all(|&value| !value));
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn immutable_candidates_reuse_work_without_sharing_identity_modes() {
        let document = super::tests::cache_document();
        let mut changed = document.clone();
        changed.instances[3]
            .attributes
            .insert("Edited".into(), json!(true));
        let editor = Tree::new(&document);
        let studio = Tree::new(&changed);
        let candidates = Candidates::default();
        let locked = vec![None, None, None, Some(3), None];
        let matcher = Matcher {
            budget: None,
            editor: &editor,
            studio: &studio,
            locked: &locked,
            owners: &locked,
            candidates: &candidates,
        };
        assert!(
            matcher.subtree(3, 3).is_some(),
            "identity survives the content edit"
        );
        let content = Matcher {
            locked: &[],
            owners: &[],
            ..matcher
        };
        assert!(content.subtree(3, 3).is_none());
        assert!(content.subtree(3, 3).is_none());
        assert!(
            !matcher.record(3, 3),
            "a lock must not masquerade as content evidence"
        );
        assert_eq!(candidates.record_comparisons.get(), 1);
        assert!(matcher.subtree(3, 3).is_some());
    }

    #[test]
    fn repeated_validation_reuses_only_the_current_scope_proof() {
        let document = super::tests::cache_document();
        let editor = Tree::new(&document);
        let studio = Tree::new(&document);
        let candidates = Candidates::default();
        let matcher = Matcher {
            budget: None,
            editor: &editor,
            studio: &studio,
            locked: &[],
            owners: &[],
            candidates: &candidates,
        };
        let partners = (0..5).map(Some).collect::<Vec<_>>();
        let reads = RefCell::default();
        let references = References::new(&matcher, &partners, &reads);
        let scope = ReferenceScope {
            targets: [(3, Some(4))].into_iter().collect(),
            parent: None,
        };
        let proof = references.in_scope(&scope);
        for _ in 0..2 {
            assert!(!proof.validates(&vec![(3, 4)]));
        }
        assert_eq!(editor.reference_comparisons.get(), 1);
        assert!(reads.borrow().contains(&1));
        let swapped = ReferenceScope {
            targets: [(1, Some(2))].into_iter().collect(),
            parent: Some(&scope),
        };
        assert!(references.in_scope(&swapped).validates(&vec![(3, 4)]));
        assert!(!proof.validates(&vec![(3, 4)]));
        assert_eq!(editor.reference_comparisons.get(), 1);
    }
}

#[cfg(test)]
mod review20;

#[cfg(test)]
mod review23;
