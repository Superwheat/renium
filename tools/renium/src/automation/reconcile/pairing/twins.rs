//! First-pairing evidence, kept separate from the mutable identity/reference remap.
//! Subtree candidates are complete. A group must cover its entire smaller side;
//! distinct root records can pair parents before comparing changed descendants.
use super::*;
use ahash::AHashSet;
use std::borrow::Cow;
use std::cell::RefCell;

use crate::settings::tree::DUPLICATE_REPAIR_GROUP_LIMIT as GROUP_LIMIT;
type Assignment = Vec<(usize, usize)>; // (Studio index, editor index)
type GroupKey<'a> = (Option<usize>, &'a str, &'a str);

struct Tree<'a> {
    document: &'a SettingsBytecode,
    properties: Vec<Cow<'a, Map<String, Value>>>,
    groups: Vec<Vec<usize>>,
    by_group: AHashMap<GroupKey<'a>, usize>,
    children: Vec<Vec<usize>>,
    by_id: AHashMap<&'a str, usize>,
    references: Vec<Vec<&'a Value>>,
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
            for value in instance
                .properties
                .values()
                .chain(instance.attributes.values())
                .filter(|_| instance.class_name != "PackageLink")
            {
                collect_references(value, &mut references);
            }
            tree.references.push(references);
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

fn collect_references<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(_) if is_reference(value) => out.push(value),
        Value::Object(object) => {
            for value in object.values() {
                collect_references(value, out);
            }
        }
        Value::Array(array) => {
            for value in array {
                collect_references(value, out);
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
fn records_match(tree: &Tree<'_>, left: usize, other: &Tree<'_>, right: usize) -> bool {
    let a = &tree.document.instances[left];
    let b = &other.document.instances[right];
    a.name == b.name
        && a.class_name == b.class_name
        // protect_package_links copies a link's version, state and
        // attributes from Studio after pairing, so only the package it names
        // is evidence here: two links to different packages are never twins.
        && (if a.class_name == "PackageLink" {
            crate::settings::equivalence::reconciliation_property_values_equal(
                "PackageLink",
                "PackageId",
                a.properties.get("PackageId"),
                b.properties.get("PackageId"),
            )
        } else {
            properties_match(
                &a.class_name,
                &tree.properties[left],
                &other.properties[right],
            ) && attributes_match(&a.attributes, &b.attributes)
        })
}

fn properties_match(class: &str, a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    if a == b {
        return true;
    }
    let database = rbx_reflection_database::get().ok();
    let equal = |name: &str, value: &Value, other: Option<&Value>| {
        !crate::settings::equivalence::reconciliation_property_compares(
            database,
            class,
            name,
            value,
            true,
            other.is_some(),
        ) || other.is_some_and(|other| reference_shapes_equal(value, other))
            || reconciliation_property_values_equal(class, name, Some(value), other)
    };
    a.iter()
        .all(|(name, value)| equal(name, value, b.get(name)))
        && b.iter()
            .all(|(name, value)| a.contains_key(name) || equal(name, value, None))
}

fn attributes_match(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    let authored = |name: &str| !crate::settings::equivalence::is_engine_managed_attribute(name);
    a.keys().filter(|name| authored(name)).count() == b.keys().filter(|name| authored(name)).count()
        // Reject small scalar differences before reading large string/blob values.
        // Use the same pairwise numeric tolerance as the full comparison.
        && a.iter().filter(|(name, value)| authored(name) && matches!(value, Value::Null | Value::Bool(_) | Value::Number(_)))
            .all(|(name, value)| b.get(name).is_some_and(|other| reconciliation_values_equal(value, other, false)))
        && a.iter()
            .filter(|(name, _)| authored(name))
            .all(|(name, value)| {
                b.get(name).is_some_and(|other| {
                    reference_shapes_equal(value, other)
                        || reconciliation_values_equal(value, other, false)
                })
            })
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

struct Matcher<'a, 'd> {
    editor: &'a Tree<'d>,
    studio: &'a Tree<'d>,
    locked: &'a [Option<usize>],
    owners: &'a [Option<usize>],
}

impl Matcher<'_, '_> {
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
                self.allowed(source, target)
                    && records_match(self.editor, target, self.studio, source)
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
        if !self.allowed(studio, editor)
            || !self.locked_pair(studio, editor)
                && !records_match(self.editor, editor, self.studio, studio)
            || self.editor.children[editor].len() != self.studio.children[studio].len()
        {
            return None;
        }
        let mut result = vec![(studio, editor)];
        for group in &self.studio.children[studio] {
            let members = &self.studio.groups[*group];
            let first = &self.studio.document.instances[members[0]];
            let counterpart = self.editor.counterpart(Some(editor), first)?;
            // Candidate subtrees remain complete. Unequal sibling groups are
            // handled at their own boundary, after their parents have paired.
            if members.len() != self.editor.groups[counterpart].len() {
                return None;
            }
            result.extend(self.group(members, &self.editor.groups[counterpart], &Some)?);
        }
        Some(result)
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
            positional.extend(plan);
        }
        let Some(failed) = failed else {
            return Some(positional);
        };
        if studio.len() > GROUP_LIMIT {
            return None;
        }
        if studio.len() == 2 {
            let mut first = self.subtree(studio[0], editor[1]).and_then(prepare)?;
            first.extend(self.subtree(studio[1], editor[0]).and_then(prepare)?);
            return Some(first);
        }
        // Probe the failed positional member first. A single edited outlier
        // must not force an all-pairs matrix for a large otherwise equal group.
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
                edges.sort_by_key(|&e| (!preferred[e], e != s, e));
                edges
            })
            .collect::<Vec<_>>();
        let assignment = perfect_matching(&edges, &exact, candidates[0].len())?;
        Some(
            assignment
                .into_iter()
                .enumerate()
                .flat_map(|(s, e)| candidates[s][e].as_ref().unwrap().iter().copied())
                .collect(),
        )
    }

    fn exact(&self, plan: &Assignment) -> bool {
        plan.iter().all(|&(source, target)| {
            let a = &self.editor.document.instances[target];
            let b = &self.studio.document.instances[source];
            self.editor.properties[target] == self.studio.properties[source]
                && a.attributes == b.attributes
        })
    }
}

/// Augmenting paths are necessary: tolerant equality is not an equivalence
/// relation, so overlapping candidate sets cannot be consumed greedily.
fn perfect_matching(
    edges: &[Vec<usize>],
    exact: &[Vec<usize>],
    targets: usize,
) -> Option<Vec<usize>> {
    fn augment(
        source: usize,
        edges: &[Vec<usize>],
        seen: &mut [bool],
        owner: &mut [Option<usize>],
    ) -> bool {
        for &target in &edges[source] {
            if seen[target] {
                continue;
            }
            seen[target] = true;
            if owner[target].is_none_or(|previous| augment(previous, edges, seen, owner)) {
                owner[target] = Some(source);
                return true;
            }
        }
        false
    }
    let mut owner = vec![None; targets];
    let mut exact_order = (0..edges.len()).collect::<Vec<_>>();
    exact_order.sort_by_key(|&source| exact[source].len());
    for source in exact_order {
        augment(source, exact, &mut vec![false; targets], &mut owner);
    }
    let reserved = owner.iter().flatten().copied().collect::<AHashSet<_>>();
    // Constrained members (including identity locks) get first choice.
    let mut order = (0..edges.len()).collect::<Vec<_>>();
    order.sort_by_key(|&source| edges[source].len());
    for source in order {
        if reserved.contains(&source) {
            continue;
        }
        if !augment(source, edges, &mut vec![false; targets], &mut owner) {
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

pub(crate) struct Resolved {
    pub(crate) partners: Vec<Option<usize>>,
    pub(crate) locked: Vec<Option<usize>>,
    pub(crate) proven: Vec<bool>,
    reference_values: Vec<(usize, bool, String, Value)>,
    #[cfg(test)]
    pub(crate) reference_comparisons: usize,
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
    let matcher = Matcher {
        editor: &editor,
        studio: &studio,
        locked: &locked,
        owners: &owners,
    };
    // Identity is global: a rename or move does not release its target slot.
    let mut partners = locked.clone();
    let mut proven = vec![false; partners.len()];
    let units = collect_units(&matcher, &mut partners, &mut proven);
    resolve_references(&matcher, &units, &mut partners, &mut proven);
    // Proof flags are tentative until checked against the complete final map.
    // Accepted store input must never turn a failed proof into a reconcile panic.
    validate_resolution(&matcher, &mut partners, &mut proven);
    assert_eq!(
        partners.iter().flatten().collect::<AHashSet<_>>().len(),
        partners.iter().flatten().count(),
        "first-pairing assignment must be injective"
    );
    assert!(
        locked
            .iter()
            .enumerate()
            .all(|(source, target)| target.is_none() || partners[source] == *target),
        "persistent identity must stay locked"
    );
    let reference_values = equivalent_reference_values(&matcher, &partners);
    Resolved {
        partners,
        locked,
        proven,
        reference_values,
        #[cfg(test)]
        reference_comparisons: editor.reference_comparisons.get(),
    }
}

fn validate_resolution(
    matcher: &Matcher<'_, '_>,
    partners: &mut [Option<usize>],
    proven: &mut [bool],
) {
    let injective = partners.iter().flatten().collect::<AHashSet<_>>().len()
        == partners.iter().flatten().count();
    let locks_preserved = matcher
        .locked
        .iter()
        .enumerate()
        .all(|(source, target)| target.is_none() || partners[source] == *target);
    let proof = partners
        .iter()
        .enumerate()
        .filter_map(|(source, &target)| {
            target
                .filter(|_| proven[source])
                .map(|target| (source, target))
        })
        .collect();
    if injective && locks_preserved && References::new(matcher, partners).validates(&proof) {
        return;
    }
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
                if let Some(proposed) = matcher.group(members, targets, &Some) {
                    let positional = positional_subtrees(matcher, positional);
                    apply(&proposed, partners);
                    units.push(Unit {
                        studio: members.clone(),
                        editor: targets.clone(),
                        sources: subtree_sources(matcher.studio, members),
                        positional,
                        proposed,
                    });
                    continue;
                }
                if let Some(roots) = matcher.distinct_roots(members, targets) {
                    apply(&roots, partners);
                    for (source, target) in roots {
                        proven[source] = true;
                        pending.push((Some(source), Some(target)));
                    }
                    continue;
                }
            }
            apply(&positional, partners);
            pending.extend(positional.into_iter().map(|(s, e)| (Some(s), Some(e))));
        }
    }
    units
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
    fn new(matcher: &'a Matcher<'a, 'd>, partners: &'a [Option<usize>]) -> Self {
        Self {
            matcher,
            partners,
            scope: None,
            interchangeable: true,
            equivalent: RefCell::default(),
        }
    }

    fn in_scope<'s>(&'s self, scope: &'s ReferenceScope<'s>) -> References<'s, 'd> {
        References {
            matcher: self.matcher,
            partners: self.partners,
            scope: Some(scope),
            interchangeable: self.interchangeable,
            equivalent: RefCell::default(),
        }
    }

    fn target(&self, source: usize) -> Option<usize> {
        self.scope
            .and_then(|scope| scope.target(source))
            .unwrap_or(self.partners[source])
    }

    /// Resolve descendants in a candidate-relative scope. In particular a model's
    /// PrimaryPart never depends on that model's old ordinal on the other side.
    fn refine(&self, mut plan: Assignment) -> Option<Assignment> {
        if plan.iter().all(|&(s, e)| {
            self.matcher.studio.references[s].is_empty()
                && self.matcher.editor.references[e].is_empty()
        }) {
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
            units.push(Unit {
                studio: members.clone(),
                editor: self.matcher.editor.groups[other].clone(),
                sources: subtree_sources(self.matcher.studio, members),
                positional: Vec::new(),
                proposed,
            });
        }
        for component in reference_components(self.matcher, &units) {
            let children = self
                .in_scope(&scope)
                .resolve_component(&units, &component)?;
            scope
                .targets
                .extend(children.into_iter().map(|(s, e)| (s, Some(e))));
        }
        for (source, target) in &mut plan {
            *target = scope.targets[source]?;
        }
        self.validates(&plan).then_some(plan)
    }

    /// Dependency components publish atomically. Cycles revisit tentative
    /// assignments until they validate or repeat; there is no fixed pass limit.
    fn resolve_component(&self, units: &[Unit], component: &[usize]) -> Option<Assignment> {
        let mut assignments = component
            .iter()
            .map(|&index| units[index].proposed.clone())
            .collect::<Vec<_>>();
        let mut scope = ReferenceScope {
            targets: AHashMap::new(),
            parent: self.scope,
        };
        let mut seen = AHashSet::new();
        loop {
            // Rebuild, never extend the old plan: subset membership can change.
            let mut plan = assignments
                .iter()
                .flatten()
                .copied()
                .collect::<Assignment>();
            plan.sort_unstable();
            for (&index, assignment) in component.iter().zip(&assignments) {
                scope.replace(&units[index], assignment, self.matcher.locked);
            }
            if self.in_scope(&scope).validates(&plan) {
                return Some(plan);
            }
            if !seen.insert(plan) {
                return None;
            }
            for (&index, assignment) in component.iter().zip(&mut assignments) {
                let unit = &units[index];
                let proposed = {
                    let references = self.in_scope(&scope);
                    self.matcher
                        .group(&unit.studio, &unit.editor, &|plan| references.refine(plan))
                };
                if let Some(proposed) = proposed {
                    *assignment = proposed;
                    scope.replace(unit, assignment, self.matcher.locked);
                }
            }
        }
    }

    fn value_equal(&self, a: &Value, b: &Value) -> bool {
        match (self.matcher.editor.target(a), self.matcher.studio.target(b)) {
            (Some(target), Some(source)) => self.target(source).is_some_and(|mapped| {
                target == mapped || self.interchangeable && self.indistinguishable(target, source)
            }),
            _ => reconciliation_values_equal(a, b, false),
        }
    }

    fn indistinguishable(&self, target: usize, source: usize) -> bool {
        if let Some(&equal) = self.equivalent.borrow().get(&(target, source)) {
            return equal;
        }
        #[cfg(test)]
        self.matcher
            .editor
            .reference_comparisons
            .set(self.matcher.editor.reference_comparisons.get() + 1);
        let equal = self.compare_subtree(target, source);
        self.equivalent.borrow_mut().insert((target, source), equal);
        equal
    }

    fn compare_subtree(&self, target: usize, source: usize) -> bool {
        let a = &self.matcher.editor.document.instances[target];
        let b = &self.matcher.studio.document.instances[source];
        let parents_match = match (a.parent_index, b.parent_index) {
            (Some(target), Some(source)) => self.target(source) == Some(target),
            (None, None) => true,
            _ => false,
        };
        if !parents_match || a.name != b.name || a.class_name != b.class_name {
            return false;
        }
        // Content proof deliberately ignores identity locks, without changing
        // the published identity assignment. Compare the actual Studio subtree.
        let matcher = Matcher {
            editor: self.matcher.editor,
            studio: self.matcher.studio,
            locked: &[],
            owners: &[],
        };
        let Some(plan) = matcher.subtree(source, target) else {
            return false;
        };
        let references = References {
            matcher: &matcher,
            partners: self.partners,
            scope: self.scope,
            // Internal references use the candidate-local assignment; external
            // references require the established target, not a recursive guess.
            interchangeable: false,
            equivalent: RefCell::default(),
        };
        references.refine(plan).is_some()
    }

    fn validates(&self, assignment: &Assignment) -> bool {
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
        let scope = ReferenceScope {
            targets: assignment.iter().map(|&(s, e)| (s, Some(e))).collect(),
            parent: self.scope,
        };
        let references = self.in_scope(&scope);
        assignment.iter().all(|&(source, target)| {
            self.matcher.locked_pair(source, target)
                || reference_fields_equal(
                    self.matcher.editor,
                    target,
                    self.matcher.studio,
                    source,
                    &|a, b| references.value_equal(a, b),
                )
        })
    }
}

fn reference_fields_equal(
    editor: &Tree<'_>,
    target: usize,
    studio: &Tree<'_>,
    source: usize,
    equal: &impl Fn(&Value, &Value) -> bool,
) -> bool {
    let a = &editor.document.instances[target];
    let b = &studio.document.instances[source];
    if a.class_name == "PackageLink" && b.class_name == "PackageLink" {
        return true;
    }
    [
        (&a.properties, &b.properties),
        (&a.attributes, &b.attributes),
    ]
    .iter()
    .all(|(a, b)| reference_maps_equal(a, b, equal))
}

/// Strongly connected components, ordered targets before their dependents.
/// The same scheduler is used for document units and candidate-local children.
fn reference_components(matcher: &Matcher<'_, '_>, units: &[Unit]) -> Vec<Vec<usize>> {
    let mut owner = AHashMap::new();
    for (index, unit) in units.iter().enumerate() {
        for &source in &unit.sources {
            owner.insert(source, index);
        }
    }
    let mut dependents = vec![Vec::new(); units.len()];
    let mut dependencies = vec![Vec::new(); units.len()];
    for (index, unit) in units.iter().enumerate() {
        dependencies[index] = unit
            .sources
            .iter()
            .flat_map(|&source| &matcher.studio.references[source])
            .filter_map(|value| matcher.studio.target(value))
            .filter_map(|target| owner.get(&target).copied())
            .filter(|&target| target != index)
            .collect::<AHashSet<_>>()
            .into_iter()
            .collect();
        dependencies[index].sort_unstable();
        for &target in &dependencies[index] {
            dependents[target].push(index);
        }
    }
    // Iterative Kosaraju avoids recursion proportional to document size.
    let mut seen = vec![false; units.len()];
    let mut order = Vec::new();
    for start in 0..units.len() {
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

fn resolve_references(
    matcher: &Matcher<'_, '_>,
    units: &[Unit],
    partners: &mut [Option<usize>],
    proven: &mut [bool],
) {
    if units.iter().all(|unit| {
        unit.proposed.iter().all(|&(source, target)| {
            matcher.studio.references[source].is_empty()
                && matcher.editor.references[target].is_empty()
        })
    }) {
        for unit in units {
            for &(source, _) in &unit.proposed {
                proven[source] = true;
            }
        }
        return;
    }
    for component in reference_components(matcher, units) {
        let proposal = References::new(matcher, partners).resolve_component(units, &component);
        if let Some(proposal) = proposal {
            // Restore abandoned tentative partners before publishing new ones.
            for &index in &component {
                publish_unit(&units[index], &Vec::new(), partners, matcher.locked);
            }
            apply(&proposal, partners);
            for (source, _) in proposal {
                proven[source] = true;
            }
        } else {
            for index in component {
                let unit = &units[index];
                publish_unit(unit, &unit.positional, partners, matcher.locked);
            }
        }
    }
}

fn equivalent_reference_values(
    matcher: &Matcher<'_, '_>,
    partners: &[Option<usize>],
) -> Vec<(usize, bool, String, Value)> {
    let references = References::new(matcher, partners);
    let mut result = Vec::new();
    for (source, target) in partners.iter().enumerate() {
        let Some(target) = target else {
            continue;
        };
        let a = &matcher.editor.document.instances[*target];
        let b = &matcher.studio.document.instances[source];
        if a.class_name == "PackageLink" {
            continue;
        }
        for (attribute, left, right) in [
            (false, &a.properties, &b.properties),
            (true, &a.attributes, &b.attributes),
        ] {
            for (name, value) in left.iter().filter(|(_, value)| contains_reference(value)) {
                if right.get(name).is_some_and(|other| {
                    reference_shapes_equal(value, other)
                        && references_equal(Some(value), Some(other), &|a, b| {
                            references.value_equal(a, b)
                        })
                }) {
                    result.push((source, attribute, name.clone(), value.clone()));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn directed_reference_cache_expires_with_the_assignment_scope() {
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
        let editor = Tree::new(&document);
        let studio = Tree::new(&document);
        let matcher = Matcher {
            editor: &editor,
            studio: &studio,
            locked: &[],
            owners: &[],
        };
        let partners = (0..5).map(Some).collect::<Vec<_>>();
        let references = References::new(&matcher, &partners);
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
