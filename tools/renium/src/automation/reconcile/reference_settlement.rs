//! References between stores after a merge.
//!
//! Each store is merged by itself. One side can remove an instance of one
//! store while the other side points a reference in another store at it:
//! each store changed on one side only, both merges are clean, and the
//! reference is left without a target. This pass looks at the merged stores
//! together and settles such a reference the way one store's merge settles
//! its own: a conflict without a preference; with one, the instance comes
//! back when the preferred side still has it, and the reference is cleared
//! when that side removed it. An instance that comes back is settled in
//! turn, with the references it holds into its own store.

use super::*;
use crate::bytecode::edit::{instance_path_key, instance_path_parts_key};
use crate::bytecode::explorer::compact_path_string;
use crate::editor::paths::build_editor_instance_paths;
use crate::project::version_control::restore_removed_instances;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, for_each_reference, for_each_reference_mut,
    strict_reference_path,
};

fn is_store_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_service_settings_file_name)
}

fn store_service(document: &SettingsBytecode) -> Option<&str> {
    document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.as_str())
}

fn reference_id(reference: &Map<String, Value>) -> Option<&str> {
    reference
        .get("settingsId")
        .or_else(|| reference.get("instanceId"))
        .and_then(Value::as_str)
}

fn reference_service(reference: &Map<String, Value>) -> Option<&str> {
    reference
        .get("pathSegments")
        .and_then(Value::as_array)
        .and_then(|segments| segments.first())
        .and_then(Value::as_str)
}

fn exact_path_keys(document: &SettingsBytecode, service: &str) -> HashSet<String> {
    build_editor_instance_paths(document, service)
        .into_iter()
        .flatten()
        .map(|path| instance_path_parts_key(&path.path_segments, &path.path_ordinals))
        .collect()
}

fn classes_by_exact_path(document: &SettingsBytecode, service: &str) -> HashMap<String, String> {
    build_editor_instance_paths(document, service)
        .into_iter()
        .zip(&document.instances)
        .filter_map(|(path, instance)| {
            let path = path?;
            Some((
                instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                instance.class_name.clone(),
            ))
        })
        .collect()
}

/// The instances of one store that the merge's base had and the merged
/// store lacks.
struct LostTargets {
    store: PathBuf,
    /// The store as the base had it: names, classes, ids and parents.
    base: SettingsBytecode,
    paths: Vec<Option<EditorInstancePath>>,
    lost: HashSet<usize>,
    by_id: HashMap<String, usize>,
    by_exact_path: HashMap<String, usize>,
    by_segments: HashMap<String, Option<usize>>,
    /// The exact paths of the instances the merged store holds.
    merged_paths: HashSet<String>,
}

impl LostTargets {
    fn find(
        store: &Path,
        base: &SnapshotEntry,
        merged: &SnapshotEntry,
    ) -> Result<Option<(String, Self)>> {
        let (SnapshotEntry::File(base), SnapshotEntry::File(merged)) = (base, merged) else {
            return Ok(None);
        };
        if base == merged {
            return Ok(None);
        }
        let base = SettingsBytecode::decode_structure(base)?;
        let merged = SettingsBytecode::decode_structure(merged)?;
        let Some(service) = store_service(&base).map(str::to_string) else {
            return Ok(None);
        };
        let kept = merged
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<HashSet<_>>();
        let lost = base
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| !kept.contains(instance.settings_id.as_str()))
            .map(|(index, _)| index)
            .collect::<HashSet<_>>();
        if lost.is_empty() {
            return Ok(None);
        }
        let paths = build_editor_instance_paths(&base, &service);
        let mut by_id = HashMap::with_capacity(paths.len());
        let mut by_exact_path = HashMap::with_capacity(paths.len());
        let mut by_segments = HashMap::<String, Option<usize>>::with_capacity(paths.len());
        for (index, path) in paths.iter().enumerate() {
            let Some(path) = path else {
                continue;
            };
            by_id.insert(base.instances[index].settings_id.clone(), index);
            by_exact_path.insert(
                instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                index,
            );
            by_segments
                .entry(instance_path_key(&path.path_segments))
                .and_modify(|unique| *unique = None)
                .or_insert(Some(index));
        }
        let merged_paths = exact_path_keys(&merged, &service);
        Ok(Some((
            service,
            Self {
                store: store.to_path_buf(),
                base,
                paths,
                lost,
                by_id,
                by_exact_path,
                by_segments,
                merged_paths,
            },
        )))
    }

    /// The lost instance a reference names. An id of the store decides: the
    /// instance with it is the target, lost or kept, whatever stands at the
    /// reference's path now. Without one the path does, and a path the
    /// merged store still resolves keeps a target, as the place build takes
    /// the path when the id names nothing.
    fn named(&self, reference: &Map<String, Value>) -> Option<usize> {
        if let Some(index) = reference_id(reference).and_then(|id| self.by_id.get(id).copied()) {
            return self.lost.contains(&index).then_some(index);
        }
        let (segments, ordinals) = strict_reference_path(reference).ok().flatten()?;
        let by_path = match ordinals {
            Some(ordinals) => {
                let exact = instance_path_parts_key(&segments, &ordinals);
                if self.merged_paths.contains(&exact) {
                    return None;
                }
                self.by_exact_path.get(&exact).copied()
            }
            None => self
                .by_segments
                .get(&instance_path_key(&segments))
                .copied()
                .flatten(),
        };
        by_path.filter(|index| self.lost.contains(index))
    }

    fn exact_path(&self, index: usize) -> Option<String> {
        self.paths[index]
            .as_ref()
            .map(|path| instance_path_parts_key(&path.path_segments, &path.path_ordinals))
    }

    fn label(&self, index: usize) -> String {
        self.paths[index].as_ref().map_or_else(
            || self.base.instances[index].name.clone(),
            |path| {
                compact_path_string(&path.path_segments, Some(&path.path_ordinals))
                    .unwrap_or_else(|| path.path_segments.join("."))
            },
        )
    }
}

/// One side of the merge, read store by store as the settlement asks.
struct Side<'a> {
    snapshot: &'a ProjectSnapshot,
    instances: HashMap<PathBuf, (HashSet<String>, HashMap<String, String>)>,
    documents: HashMap<PathBuf, Option<SettingsBytecode>>,
}

impl<'a> Side<'a> {
    fn new(snapshot: &'a ProjectSnapshot) -> Self {
        Self {
            snapshot,
            instances: HashMap::new(),
            documents: HashMap::new(),
        }
    }

    /// Whether the side still has the instance: under its id, or at its
    /// path when the side numbers its instances differently. An instance of
    /// another class at that path is one that took its place.
    fn has_target(&mut self, lost: &LostTargets, service: &str, index: usize) -> Result<bool> {
        if !self.instances.contains_key(&lost.store) {
            let held = match self.snapshot.entries.get(&lost.store) {
                Some(SnapshotEntry::File(bytes)) => {
                    let document = SettingsBytecode::decode_structure(bytes)?;
                    (
                        document
                            .instances
                            .iter()
                            .map(|instance| instance.settings_id.clone())
                            .collect(),
                        classes_by_exact_path(&document, service),
                    )
                }
                _ => Default::default(),
            };
            self.instances.insert(lost.store.clone(), held);
        }
        let (ids, classes) = &self.instances[&lost.store];
        let target = &lost.base.instances[index];
        Ok(ids.contains(&target.settings_id)
            || lost
                .exact_path(index)
                .is_some_and(|path| classes.get(&path) == Some(&target.class_name)))
    }

    /// Whether the side's copy of a store holds this reference.
    fn has_reference(&mut self, store: &Path, reference: &Map<String, Value>) -> Result<bool> {
        if !self.documents.contains_key(store) {
            let document = match self.snapshot.entries.get(store) {
                entry @ Some(SnapshotEntry::File(_)) => Some(decoded_settings_document(entry)?),
                _ => None,
            };
            self.documents.insert(store.to_path_buf(), document);
        }
        let Some(document) = &self.documents[store] else {
            return Ok(false);
        };
        let mut found = false;
        for instance in &document.instances {
            for value in instance
                .properties
                .values()
                .chain(instance.attributes.values())
            {
                for_each_reference(value, &mut |held| {
                    found |= held.get("pathSegments") == reference.get("pathSegments")
                        && reference_id(held) == reference_id(reference);
                });
            }
        }
        Ok(found)
    }
}

/// A reference of a merged store to an instance a store lost.
struct BrokenReference {
    store: PathBuf,
    instance: usize,
    attribute: bool,
    field: String,
    reference: Map<String, Value>,
    service: String,
    target: usize,
}

/// The references to lost instances: every one that leads into another
/// store, and those the instances in `restored` (put back by the settlement,
/// by store) hold into their own store, which no store's merge has seen.
fn broken_references(
    merged: &BTreeMap<PathBuf, SnapshotEntry>,
    lost: &BTreeMap<String, LostTargets>,
    restored: &BTreeMap<PathBuf, HashSet<String>>,
    holders: &mut BTreeMap<PathBuf, SettingsBytecode>,
) -> Result<Vec<BrokenReference>> {
    let needles = lost
        .keys()
        .map(|service| vec![service.clone()])
        .collect::<Vec<_>>();
    let mut broken = Vec::new();
    for (path, entry) in merged {
        let SnapshotEntry::File(bytes) = entry else {
            continue;
        };
        if !is_store_path(path) {
            continue;
        }
        let Some(document) = SettingsBytecode::decode_if_contains_any_string_set(bytes, &needles)?
        else {
            continue;
        };
        let Some(holder_service) = store_service(&document).map(str::to_string) else {
            continue;
        };
        let found = broken.len();
        let restored = restored.get(path);
        for (instance, holder) in document.instances.iter().enumerate() {
            let restored = restored.is_some_and(|ids| ids.contains(&holder.settings_id));
            for (attribute, record) in [(false, &holder.properties), (true, &holder.attributes)] {
                for (field, value) in record {
                    for_each_reference(value, &mut |reference| {
                        let service = match reference_service(reference) {
                            Some(service) if service != holder_service => service,
                            _ if restored => holder_service.as_str(),
                            _ => return,
                        };
                        if let Some(target) =
                            lost.get(service).and_then(|lost| lost.named(reference))
                        {
                            broken.push(BrokenReference {
                                store: path.clone(),
                                instance,
                                attribute,
                                field: field.clone(),
                                reference: reference.clone(),
                                service: service.to_string(),
                                target,
                            });
                        }
                    });
                }
            }
        }
        if broken.len() > found {
            holders.insert(path.clone(), document);
        }
    }
    Ok(broken)
}

/// The merge as the settlement rewrites it.
struct MergeOutcome<'a> {
    sides: &'a SnapshotSides<'a>,
    merged: &'a mut BTreeMap<PathBuf, SnapshotEntry>,
    changes: &'a mut MergeChanges,
}

impl MergeOutcome<'_> {
    /// Replaces a merged store and records what that means for each side.
    /// The files are compared with the result. Studio matched the store
    /// before the rewrite when it was not marked changed, so it now differs
    /// and is paired by that earlier content.
    fn rewrite_store(
        &mut self,
        path: &Path,
        before: SettingsBytecode,
        document: &SettingsBytecode,
    ) -> Result<()> {
        self.merged.insert(
            path.to_path_buf(),
            SnapshotEntry::File(encode_settings_bytecode(document)?),
        );
        let unchanged_in_files = match self.sides.editor.entries.get(path) {
            Some(entry) => settings_documents_equivalent(
                &settings_document(Some(entry))?,
                &settings_document(self.merged.get(path))?,
            ),
            None => false,
        };
        if unchanged_in_files {
            self.changes.editor.remove(path);
        } else {
            self.changes.editor.insert(path.to_path_buf());
        }
        if self.changes.studio.insert(path.to_path_buf()) {
            self.changes
                .aligned_studio
                .insert(path.to_path_buf(), before);
        }
        Ok(())
    }

    /// Gives restored scripts their source files back: the merge dropped
    /// each with its instance, and the preferred side still has it.
    fn restore_script_sources(
        &mut self,
        store: &Path,
        restored: &HashSet<String>,
        preferred: &ProjectSnapshot,
    ) -> Result<()> {
        let merged_paths = script_paths_by_settings_id(self.merged, store)?;
        let preferred_paths = script_paths_by_settings_id(&preferred.entries, store)?;
        for settings_id in restored {
            let (Some(path), Some(source)) = (
                merged_paths.get(settings_id),
                preferred_paths
                    .get(settings_id)
                    .and_then(|path| preferred.entries.get(path)),
            ) else {
                continue;
            };
            self.merged.insert(path.clone(), source.clone());
            for (side, changed) in [
                (self.sides.editor, &mut self.changes.editor),
                (self.sides.studio, &mut self.changes.studio),
            ] {
                if entries_equivalent(path, side.entries.get(path), Some(source)) {
                    changed.remove(path);
                } else {
                    changed.insert(path.clone());
                }
            }
        }
        Ok(())
    }

    /// Clears the given references of one merged store.
    fn clear_references(
        &mut self,
        store: &Path,
        mut document: SettingsBytecode,
        cleared: Vec<BrokenReference>,
    ) -> Result<()> {
        let before = settings_document(self.merged.get(store))?;
        for reference in cleared {
            let holder = &mut document.instances[reference.instance];
            let record = if reference.attribute {
                &mut holder.attributes
            } else {
                &mut holder.properties
            };
            let Some(value) = record.get_mut(&reference.field) else {
                continue;
            };
            for_each_reference_mut(value, &mut |held| {
                if *held == reference.reference {
                    for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
                        held.remove(selector);
                    }
                }
            });
        }
        self.rewrite_store(store, before, &document)
    }

    /// Puts lost instances of one store back, as the merge's base had them.
    /// Returns the ids of the instances the store gained.
    fn restore_targets(
        &mut self,
        store: &Path,
        settings_ids: &HashSet<String>,
        baseline: &ProjectSnapshot,
        preferred: &ProjectSnapshot,
    ) -> Result<HashSet<String>> {
        let before = settings_document(self.merged.get(store))?;
        let mut source = settings_document(baseline.entries.get(store))?;
        remove_reconciliation_derived_properties(&mut source);
        let restored = restore_removed_instances(&before, &source, settings_ids);
        let held = before
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<HashSet<_>>();
        let restored_ids = restored
            .instances
            .iter()
            .filter(|instance| !held.contains(instance.settings_id.as_str()))
            .map(|instance| instance.settings_id.clone())
            .collect::<HashSet<_>>();
        self.rewrite_store(store, before, &restored)?;
        self.restore_script_sources(store, &restored_ids, preferred)?;
        Ok(restored_ids)
    }
}

/// The baseline instances each store lost in the merge, by service. Only a
/// store that the merge changed for a side can have lost one.
fn lost_targets(
    baseline: &ProjectSnapshot,
    merged: &BTreeMap<PathBuf, SnapshotEntry>,
    changes: &MergeChanges,
) -> Result<BTreeMap<String, LostTargets>> {
    let changed_stores = changes
        .editor
        .iter()
        .chain(&changes.studio)
        .filter(|path| is_store_path(path))
        .collect::<BTreeSet<_>>();
    let mut lost = BTreeMap::new();
    for path in changed_stores {
        let (Some(base), Some(now)) = (baseline.entries.get(path), merged.get(path)) else {
            continue;
        };
        if let Some((service, targets)) = LostTargets::find(path, base, now)? {
            lost.insert(service, targets);
        }
    }
    Ok(lost)
}

/// What the preference decides for the references the merge broke.
#[derive(Default)]
struct Settlement {
    /// The lost instances to put back, by service.
    restores: BTreeMap<String, HashSet<String>>,
    /// The references to clear, by store.
    clears: BTreeMap<PathBuf, Vec<BrokenReference>>,
}

/// Decides each broken reference. One that was already broken on the side
/// it came from is not the merge's doing and stays as it is. Without a
/// preference the others are conflicts; with one, the target comes back
/// when the preferred side still has it and the reference is cleared when
/// that side removed it.
fn decide(
    broken: Vec<BrokenReference>,
    lost: &BTreeMap<String, LostTargets>,
    holders: &BTreeMap<PathBuf, SettingsBytecode>,
    sides: &SnapshotSides<'_>,
    prefer_editor: Option<bool>,
    conflicts: &mut Vec<String>,
) -> Result<Settlement> {
    let mut editor = Side::new(sides.editor);
    let mut studio = Side::new(sides.studio);
    let mut settlement = Settlement::default();
    for reference in broken {
        let targets = &lost[&reference.service];
        let valid_on = |side: &mut Side<'_>| -> Result<bool> {
            Ok(
                side.has_target(targets, &reference.service, reference.target)?
                    && side.has_reference(&reference.store, &reference.reference)?,
            )
        };
        if !valid_on(&mut editor)? && !valid_on(&mut studio)? {
            continue;
        }
        let target_id = &targets.base.instances[reference.target].settings_id;
        let preferred = match prefer_editor {
            Some(true) => &mut editor,
            Some(false) => &mut studio,
            None => {
                conflicts.push(format!(
                    "{}: {}: {}: refers to {} (id={target_id}), which the other side removed",
                    reference.store.display(),
                    holders[&reference.store].instances[reference.instance].name,
                    reference.field,
                    targets.label(reference.target),
                ));
                continue;
            }
        };
        if preferred.has_target(targets, &reference.service, reference.target)? {
            settlement
                .restores
                .entry(reference.service.clone())
                .or_default()
                .insert(target_id.clone());
        } else {
            settlement
                .clears
                .entry(reference.store.clone())
                .or_default()
                .push(reference);
        }
    }
    Ok(settlement)
}

/// Settles the references the per-store merges left without a target. An
/// instance that comes back brings its own references along, into its store
/// and into others, and their targets can be lost as well, so the settlement
/// runs again until a round restores nothing.
pub(super) fn settle_references_between_stores(
    sides: &SnapshotSides<'_>,
    preference: ConflictPreference,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    conflicts: &mut Vec<String>,
    changes: &mut MergeChanges,
) -> Result<()> {
    let Some(baseline) = sides.baseline else {
        return Ok(());
    };
    let prefer_editor = preference_bool(preference);
    let preferred = if prefer_editor == Some(true) {
        sides.editor
    } else {
        sides.studio
    };
    let mut restored = BTreeMap::<PathBuf, HashSet<String>>::new();
    loop {
        let lost = lost_targets(baseline, merged, changes)?;
        if lost.is_empty() {
            return Ok(());
        }
        let mut holders = BTreeMap::new();
        let broken = broken_references(merged, &lost, &restored, &mut holders)?;
        if broken.is_empty() {
            return Ok(());
        }
        let settlement = decide(broken, &lost, &holders, sides, prefer_editor, conflicts)?;
        let mut outcome = MergeOutcome {
            sides,
            merged,
            changes,
        };
        for (store, cleared) in settlement.clears {
            if let Some(document) = holders.remove(&store) {
                outcome.clear_references(&store, document, cleared)?;
            }
        }
        let mut gained = false;
        for (service, settings_ids) in settlement.restores {
            let store = &lost[&service].store;
            let added = outcome.restore_targets(store, &settings_ids, baseline, preferred)?;
            gained |= !added.is_empty();
            restored.entry(store.clone()).or_default().extend(added);
        }
        if !gained {
            return Ok(());
        }
    }
}
