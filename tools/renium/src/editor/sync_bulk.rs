//! What a collection of many changed script files shares instead of reading
//! a whole store for each file: one copy of the store for the history
//! entries, and one removal for a run of deleted scripts.

use super::*;
use crate::bytecode::edit::collect_settings_subtree_preorder;
use crate::editor::diff::{EditorSiblingGroupCounts, editor_instance_descriptor_from_path};
use crate::editor::document::EditorDocumentIndex;
use crate::editor::paths::editor_source_index_with_cache;
use crate::editor::types::EditorSourcePathSpec;
use crate::settings::bytecode::encode_settings_bytecode;
use crate::settings::tree::{editor_child_stems, editor_service_root_index};
use crate::system::files::sanitize_name;

#[cfg(test)]
thread_local! {
    /// Makes a collection take one file at a time, as the tests that compare
    /// both ways need.
    pub(super) static PER_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn per_file() -> bool {
    PER_FILE.with(std::cell::Cell::get)
}

#[cfg(not(test))]
pub(super) fn per_file() -> bool {
    false
}

/// The store the history entries of one service keep from before their
/// change, encoded as its file holds it. Undoing an entry reads from it
/// whether it held the entry's instance and, where it did, that instance's
/// record, parent and subtree. Entries share one copy where those are as
/// they were just before each one's change: `touched` names the instances
/// added, removed, changed or left with fewer children since the copy was
/// taken.
#[derive(Default)]
pub(super) struct HistorySnapshot {
    copy: Option<Arc<Vec<u8>>>,
    touched: HashSet<String>,
}

impl HistorySnapshot {
    /// The store for the entry of a change to the instances `reached` names:
    /// the shared copy where none of them was touched since it was taken.
    pub(super) fn before_changing<'a>(
        &mut self,
        document: &SettingsBytecode,
        reached: impl IntoIterator<Item = &'a str>,
    ) -> Result<Arc<Vec<u8>>> {
        if per_file() || reached.into_iter().any(|id| self.touched.contains(id)) {
            self.copy = None;
        }
        if let Some(copy) = &self.copy {
            return Ok(Arc::clone(copy));
        }
        self.touched.clear();
        let copy = Arc::new(encode_settings_bytecode(document)?);
        self.copy = Some(Arc::clone(&copy));
        Ok(copy)
    }

    /// The store for the entry of the script `spec` names: a new instance
    /// reaches nothing the store held, and one the store held reaches its
    /// subtree.
    pub(super) fn before_creating(
        &mut self,
        document: &SettingsBytecode,
        index: &EditorDocumentIndex,
        spec: &EditorSourcePathSpec,
    ) -> Result<Arc<Vec<u8>>> {
        let target = index.source_target(document, spec);
        self.before_changing_subtree(document, &index.children, target)
    }

    /// The store for the entry of a change to the instance at `target`,
    /// which undoing it where the instance is gone puts back with its
    /// subtree. `children` are the document's children by parent.
    pub(super) fn before_changing_subtree(
        &mut self,
        document: &SettingsBytecode,
        children: &[Vec<usize>],
        target: Option<usize>,
    ) -> Result<Arc<Vec<u8>>> {
        let mut subtree = Vec::new();
        if let Some(target) = target {
            collect_settings_subtree_preorder(children, target, &mut subtree);
        }
        let reached = subtree
            .iter()
            .map(|at| document.instances[*at].settings_id.as_str());
        self.before_changing(document, reached)
    }

    pub(super) fn touch(&mut self, ids: impl IntoIterator<Item = String>) {
        self.touched.extend(ids);
    }

    /// Notes the instances an ensure added from `added_from` on, and its
    /// `target`, whose class or RunContext it may have changed.
    pub(super) fn created(
        &mut self,
        document: &SettingsBytecode,
        added_from: usize,
        target: Option<&str>,
    ) {
        let added = document.instances[added_from..].iter();
        let added = added.map(|instance| instance.settings_id.clone());
        self.touch(added.chain(target.map(str::to_string)));
    }
}

/// The settings ids of the instances that refer by index to one of
/// `removed`: removing those drops the references, which changes the
/// records that held them.
pub(super) fn referring_by_index(document: &SettingsBytecode, removed: &[usize]) -> Vec<String> {
    fn refers(value: &Value, removed: &HashSet<usize>) -> bool {
        match value {
            Value::Array(values) => values.iter().any(|value| refers(value, removed)),
            Value::Object(object) => {
                (object.get("instanceIndex"))
                    .and_then(settings_reference_index)
                    .is_some_and(|index| removed.contains(&index))
                    || object.values().any(|value| refers(value, removed))
            }
            _ => false,
        }
    }
    crate::settings::tree::count_whole_store_pass();
    let removed = removed.iter().copied().collect::<HashSet<_>>();
    (document.instances.iter())
        .filter(|instance| {
            let mut values = instance.properties.values();
            values.any(|value| refers(value, &removed))
                || instance
                    .attributes
                    .values()
                    .any(|value| refers(value, &removed))
        })
        .map(|instance| instance.settings_id.clone())
        .collect()
}

/// After a script was created in `service`: drops the child stems of the
/// parents that gained the instances from `added_from` on, while the other
/// parents keep theirs and the index stays as the creation kept it.
pub(super) fn forget_added(
    (indexes, stems): (
        &mut HashMap<String, EditorDocumentIndex>,
        &mut HashMap<String, EditorChildStemCache>,
    ),
    service: &str,
    document: &SettingsBytecode,
    added_from: usize,
) {
    if per_file() {
        indexes.remove(service);
        stems.remove(service);
        return;
    }
    let Some(stems) = stems.get_mut(service) else {
        return;
    };
    for instance in &document.instances[added_from..] {
        if let Some(parent) = instance.parent_index {
            stems.remove(&parent);
        }
    }
}

/// After a script file of `service` was deleted: drops the service's index
/// and child stems, which the folder an `init` file's script turned into
/// leaves as they were.
pub(super) fn forget_deleted(
    (indexes, stems): (
        &mut HashMap<String, EditorDocumentIndex>,
        &mut HashMap<String, EditorChildStemCache>,
    ),
    service: &str,
    init: bool,
) {
    if !init || per_file() {
        indexes.remove(service);
        stems.remove(service);
    }
}

/// `read_whole_for_edit` of a service's document the first time the
/// collection edits it; the document stays whole after.
pub(super) fn read_whole_once(
    read: &mut HashSet<String>,
    documents: &mut HashMap<String, Option<Arc<SettingsBytecode>>>,
    src_root: &Path,
    service: &str,
) -> Result<()> {
    if !read.insert(service.to_string()) {
        return Ok(());
    }
    read_whole_for_edit(
        documents.get_mut(service).and_then(Option::as_mut),
        src_root,
        service,
    )
}

pub(super) type InspectedPath = (
    PathBuf,
    Option<super::ChangedPathKind>,
    Option<std::io::Result<String>>,
);

/// What the values of a store refer to: instances by settings id, those it
/// names by index as well, and paths. A collection reads it once for each
/// service, as removing instances only drops references and adding scripts
/// adds none, so it names at least every reference the store holds.
#[derive(Default)]
pub(super) struct StoreReferences {
    ids: HashSet<String>,
    paths: HashSet<Vec<String>>,
}

impl StoreReferences {
    fn of(document: &SettingsBytecode) -> Self {
        crate::settings::tree::count_whole_store_pass();
        let mut references = Self::default();
        for instance in &document.instances {
            let values = instance.properties.values();
            for value in values.chain(instance.attributes.values()) {
                references.take(document, value);
            }
        }
        references
    }

    fn take(&mut self, document: &SettingsBytecode, value: &Value) {
        match value {
            Value::Array(values) => values.iter().for_each(|value| self.take(document, value)),
            Value::Object(object) => {
                if is_reference_object(object) {
                    let by_index = (object.get("instanceIndex"))
                        .and_then(settings_reference_index)
                        .and_then(|index| document.instances.get(index));
                    self.ids
                        .extend(by_index.map(|instance| instance.settings_id.clone()));
                    let by_id = ["settingsId", "instanceId", "referent", "ref"]
                        .iter()
                        .filter_map(|key| object.get(*key)?.as_str());
                    self.ids.extend(by_id.map(str::to_string));
                    let path = (object.get("pathSegments").and_then(Value::as_array)).and_then(
                        |segments| {
                            (segments.iter())
                                .map(|segment| segment.as_str().map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        },
                    );
                    self.paths.extend(path);
                }
                object.values().for_each(|value| self.take(document, value));
            }
            _ => {}
        }
    }
}

/// Which instances of a store hold references, by what the references name:
/// an index, a settings id or a path, each with the positions of the
/// instances whose values hold one, and the first position of each settings
/// id. A collection reads it once for each service and keeps it while every
/// instance it read stays where it was with the values it had: adding
/// scripts appends instances without references and replacing a class
/// changes no reference, while a removal drops it.
pub(super) struct StoreReferrers {
    read: usize,
    first: HashMap<String, usize>,
    by_index: HashMap<usize, Vec<usize>>,
    by_id: HashMap<String, Vec<usize>>,
    by_path: HashMap<Vec<String>, Vec<usize>>,
    repairs: Option<RepairTables>,
}

/// What repairing the references to a target reads of the whole store: the
/// path of each instance and the last position of each settings id. Adding
/// scripts appends instances after their namesakes and keeps the service
/// root, which leaves the paths of the other instances as they were, so the
/// tables grow by the added instances.
struct RepairTables {
    paths: Vec<Option<EditorInstancePath>>,
    last: HashMap<String, usize>,
}

impl StoreReferrers {
    fn of(document: &SettingsBytecode) -> Self {
        let _span = crate::app::profile::span("store referrers read");
        crate::settings::tree::count_whole_store_pass();
        let mut referrers = Self {
            read: document.instances.len(),
            first: HashMap::with_capacity(document.instances.len()),
            by_index: HashMap::new(),
            by_id: HashMap::new(),
            by_path: HashMap::new(),
            repairs: None,
        };
        for (at, instance) in document.instances.iter().enumerate() {
            (referrers.first)
                .entry(instance.settings_id.clone())
                .or_insert(at);
            let values = instance.properties.values();
            for value in values.chain(instance.attributes.values()) {
                referrers.take(at, value);
            }
        }
        referrers
    }

    fn take(&mut self, at: usize, value: &Value) {
        match value {
            Value::Array(values) => values.iter().for_each(|value| self.take(at, value)),
            Value::Object(object) => {
                if is_reference_object(object) {
                    let index = object
                        .get("instanceIndex")
                        .and_then(settings_reference_index);
                    if let Some(index) = index {
                        self.by_index.entry(index).or_default().push(at);
                    }
                    let ids = ["settingsId", "instanceId", "referent", "ref"]
                        .iter()
                        .filter_map(|key| object.get(*key)?.as_str());
                    for id in ids {
                        self.by_id.entry(id.to_string()).or_default().push(at);
                    }
                    let path = (object.get("pathSegments").and_then(Value::as_array)).and_then(
                        |segments| {
                            (segments.iter())
                                .map(|segment| segment.as_str().map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        },
                    );
                    if let Some(path) = path {
                        self.by_path.entry(path).or_default().push(at);
                    }
                }
                object.values().for_each(|value| self.take(at, value));
            }
            _ => {}
        }
    }

    /// The first position of `settings_id` in `document`.
    fn first_index(&self, document: &SettingsBytecode, settings_id: &str) -> Option<usize> {
        (self.first.get(settings_id).copied()).or_else(|| {
            (document.instances[self.read..].iter())
                .position(|instance| instance.settings_id == settings_id)
                .map(|at| self.read + at)
        })
    }

    /// `editor_reference_hits` of `document`, read from the instances that
    /// hold a reference to what names the target and from those added since
    /// the store was read. `children` are the document's children by parent.
    fn hits(
        &self,
        document: &SettingsBytecode,
        service: &str,
        children: &[Vec<usize>],
        target_settings_id: &str,
    ) -> Vec<EditorReferenceHit> {
        let Some(target_index) = self.first_index(document, target_settings_id) else {
            return Vec::new();
        };
        let Some(target_path) = indexed_path(document, service, children, target_index) else {
            return Vec::new();
        };
        let mut candidates = [
            self.by_index.get(&target_index),
            self.by_id.get(target_settings_id),
            self.by_path.get(&target_path.path_segments),
        ]
        .into_iter()
        .flatten()
        .flatten()
        .copied()
        .chain(self.read..document.instances.len())
        .collect::<Vec<_>>();
        candidates.sort_unstable();
        candidates.dedup();
        (candidates.into_iter())
            .filter_map(|at| {
                editor_reference_hit(
                    &document.instances[at],
                    target_index,
                    target_settings_id,
                    &target_path,
                )
            })
            .collect()
    }

    /// The repair tables of `document`, read once and grown by the
    /// instances added since. `children` are the document's children by
    /// parent.
    fn repair_tables(
        &mut self,
        document: &SettingsBytecode,
        service: &str,
        children: &[Vec<usize>],
    ) -> &RepairTables {
        let tables = self.repairs.get_or_insert_with(|| RepairTables {
            paths: build_editor_instance_paths(document, service),
            last: (document.instances.iter().enumerate())
                .map(|(at, instance)| (instance.settings_id.clone(), at))
                .collect(),
        });
        for at in tables.paths.len()..document.instances.len() {
            (tables.paths).push(indexed_path(document, service, children, at));
            (tables.last).insert(document.instances[at].settings_id.clone(), at);
        }
        tables
    }
}

/// The path `build_editor_instance_paths_for_indices` gives the instance at
/// `index`, read from `children`, the document's children by parent, in
/// index order: the service root's ordinal is 1, as no instance without a
/// parent stands before it with its name.
fn indexed_path(
    document: &SettingsBytecode,
    service: &str,
    children: &[Vec<usize>],
    index: usize,
) -> Option<EditorInstancePath> {
    let root = editor_service_root_index(document, service)?;
    let instances = &document.instances;
    let mut hierarchy = Vec::new();
    let mut current = Some(index).filter(|index| *index < instances.len());
    while let Some(at) = current {
        hierarchy.push(at);
        current = instances[at].parent_index;
    }
    if hierarchy.last() != Some(&root) {
        return None;
    }
    hierarchy.reverse();
    let ordinal = |at: usize| {
        let Some(parent) = instances[at].parent_index else {
            return 1;
        };
        let name = &instances[at].name;
        (children[parent].iter())
            .filter(|sibling| instances[**sibling].name == *name)
            .position(|sibling| *sibling == at)
            .map_or(1, |position| position + 1)
    };
    Some(EditorInstancePath {
        path_segments: (hierarchy.iter())
            .map(|at| instances[*at].name.clone())
            .collect(),
        path_ordinals: hierarchy.iter().map(|at| ordinal(*at)).collect(),
    })
}

/// The repairs of the references to the instance `target_settings_id`, whose
/// class alone changed, which leaves every reference and path of `document`
/// as it was. A collection reads where the values of a store refer, and
/// what the repairs read of it, once while its instances keep their places.
/// `children` are the document's children by parent.
pub(super) fn repair_references(
    referrers: &mut HashMap<String, StoreReferrers>,
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    children: &[Vec<usize>],
    target_settings_id: &str,
) {
    if per_file() {
        let hits = editor_reference_hits(document, service, target_settings_id);
        append_editor_reference_hit_repairs(changes, &hits, document, service);
        return;
    }
    let referrers =
        (referrers.entry(service.to_string())).or_insert_with(|| StoreReferrers::of(document));
    let hits = referrers.hits(document, service, children, target_settings_id);
    if hits.is_empty() {
        return;
    }
    let tables = referrers.repair_tables(document, service, children);
    append_editor_reference_repairs_from(changes, &hits, document, service, &tables.paths, |id| {
        tables.last.get(id).copied()
    });
}

/// `document_instance_index_by_settings_id`, read from the referrers of the
/// service's store where the collection keeps them.
pub(super) fn instance_index(
    referrers: &HashMap<String, StoreReferrers>,
    document: &SettingsBytecode,
    service: &str,
    settings_id: &str,
) -> Option<usize> {
    match referrers.get(service) {
        Some(referrers) => referrers.first_index(document, settings_id),
        None => document_instance_index_by_settings_id(document, settings_id),
    }
}

/// `editor_instance_descriptor_for_known_path`, which reads the instances
/// that share the name and parent of the one at `index` from `children`, the
/// document's children by parent, in place of a pass over the store.
pub(super) fn descriptor_for_known_path(
    document: &SettingsBytecode,
    children: &[Vec<usize>],
    index: usize,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
) -> Option<EditorInstanceDescriptor> {
    let instance = document.instances.get(index)?;
    let siblings = (instance.parent_index).and_then(|parent| Some((parent, children.get(parent)?)));
    let Some((parent, siblings)) = siblings.filter(|_| !per_file()) else {
        return editor_instance_descriptor_for_known_path(
            document,
            index,
            path_segments,
            path_ordinals,
        );
    };
    let name = instance.name.as_str();
    let namesakes = (siblings.iter().copied())
        .filter(|sibling| document.instances[*sibling].name == name)
        .collect();
    let sibling_counts = EditorSiblingGroupCounts::from([((parent, name), namesakes)]);
    editor_instance_descriptor_from_path(
        document,
        index,
        path_segments,
        path_ordinals,
        &sibling_counts,
    )
}

/// A deleted script file of a run: the scope the collection records for it
/// and the instance it pointed at, with that instance's index.
struct DeletedScript {
    path: PathBuf,
    scope: (Vec<String>, Vec<usize>),
    target: Option<EditorSourceTarget>,
    index: usize,
}

/// What a run checked so far: the instances it takes, whether each parent's
/// children have their names as file stems, and what refers to the scripts
/// it takes: the settings ids of the instances, each with the names of the
/// values that do, and the hits in the order of the scripts.
#[derive(Default)]
struct RunCheck {
    indices: HashSet<usize>,
    parents: HashMap<usize, bool>,
    referrers: HashSet<String>,
    values: HashSet<(String, bool, String)>,
    hits: Vec<EditorReferenceHit>,
}

impl RunCheck {
    /// Whether the run can take the script `settings_id` that something in
    /// `document` refers to: no value that refers to it refers to a script
    /// the run took. Removing the scripts one at a time then leaves each
    /// value that refers to one of them as removing them together does.
    /// `children` are the document's children by parent.
    fn takes_referred(
        &mut self,
        referrers: &StoreReferrers,
        document: &SettingsBytecode,
        (service, children): (&str, &[Vec<usize>]),
        settings_id: &str,
    ) -> bool {
        let hits = referrers.hits(document, service, children, settings_id);
        let values = (hits.iter())
            .flat_map(|hit| {
                let properties = hit.properties.iter().map(|name| (false, name));
                let attributes = hit.attributes.iter().map(|name| (true, name));
                (properties.chain(attributes)).map(move |(attribute, name)| {
                    (hit.settings_id.clone(), attribute, name.clone())
                })
            })
            .collect::<Vec<_>>();
        if values.iter().any(|value| self.values.contains(value)) {
            return false;
        }
        (self.referrers).extend(hits.iter().map(|hit| hit.settings_id.clone()));
        self.values.extend(values);
        self.hits.extend(hits);
        true
    }
}

/// Deleted script files of one service that follow each other in a
/// collection, removed from the store in one step. The run takes the files
/// where removing their scripts one at a time leaves every other path and
/// ordinal as it was, and every reference but those to the scripts: each
/// script is a leaf whose siblings all keep their names as file stems, it
/// refers to no script the run took before it, and no value refers to two
/// of them. It ends before the first file that is not such a script, which
/// the collection takes on its own. Each file of the run gets the change
/// set, history entry and store result it gets alone, with the repairs of
/// the references to its script.
pub(super) struct DeletedScriptRun<'a> {
    pub(super) src_root: &'a Path,
    pub(super) service: &'a str,
    pub(super) full_reconcile: bool,
    pub(super) link_enforcement: &'a LinkEnforcement,
    pub(super) instance_paths: &'a HashSet<String>,
    pub(super) documents: &'a mut HashMap<String, Option<Arc<SettingsBytecode>>>,
    pub(super) read_whole: &'a mut HashSet<String>,
    pub(super) references: &'a mut HashMap<String, StoreReferences>,
    pub(super) referrers: &'a mut HashMap<String, StoreReferrers>,
    pub(super) indexes: &'a mut HashMap<String, EditorDocumentIndex>,
    pub(super) stems: &'a mut HashMap<String, EditorChildStemCache>,
    pub(super) seen_paths: &'a mut HashSet<String>,
    pub(super) changed_services: &'a mut EditorChangedServices,
    pub(super) retired_ids: &'a mut HashMap<String, HashSet<String>>,
    pub(super) snapshots: &'a mut HashMap<String, HistorySnapshot>,
    pub(super) changes: &'a mut EditorChangeSet,
}

impl DeletedScriptRun<'_> {
    /// Collects the deleted script at `path`, whose pairing and scope the
    /// collection already took, with the deleted scripts that follow it.
    /// Returns how many of `following` it collected, or None where it took
    /// no run.
    pub(super) fn collect(
        mut self,
        path: &Path,
        following: &[InspectedPath],
    ) -> Result<Option<usize>> {
        if per_file() {
            return Ok(None);
        }
        read_whole_once(self.read_whole, self.documents, self.src_root, self.service)?;
        let mut check = RunCheck::default();
        let Some(first) = (self.located(path)).filter(|first| self.removable(first, &mut check))
        else {
            return Ok(None);
        };
        let mut run = vec![first];
        let mut keys = HashSet::from([path_key(path)]);
        for (path, kind, _) in following {
            match self.follower(path, kind.is_some(), &keys) {
                Some(deleted) if self.removable(&deleted, &mut check) => {
                    keys.insert(path_key(path));
                    run.push(deleted);
                }
                _ => break,
            }
        }
        if run.len() < 2 {
            return Ok(None);
        }
        self.remove(&run, &check.hits)?;
        Ok(Some(run.len() - 1))
    }

    /// The next file of the run: a deleted script of the same service that
    /// the collection has not seen.
    fn follower(
        &mut self,
        path: &Path,
        exists: bool,
        keys: &HashSet<String>,
    ) -> Option<DeletedScript> {
        let key = path_key(path);
        if exists
            || self.seen_paths.contains(&key)
            || keys.contains(&key)
            || self.instance_paths.contains(&key)
            || service_from_changed_path(self.src_root, path).as_deref() != Some(self.service)
        {
            return None;
        }
        self.located(path)
    }

    /// The deleted script file at `path` with where it pointed in the store.
    fn located(&mut self, path: &Path) -> Option<DeletedScript> {
        let spec = infer_editor_source_path_spec(self.src_root, self.service, path)
            .filter(|spec| !spec.is_init)?;
        let document = self.documents.get(self.service)?.as_ref()?;
        let found = editor_source_index_with_cache(
            document,
            self.service,
            &self.src_root.join(self.service),
            path,
            &self.indexes.get(self.service)?.children,
            self.stems.entry(self.service.to_string()).or_default(),
        );
        let path = path.to_path_buf();
        Some(match found {
            Some((index, target)) => DeletedScript {
                path,
                scope: (target.path_segments.clone(), target.path_ordinals.clone()),
                target: Some(target),
                index,
            },
            None => DeletedScript {
                path,
                scope: (spec.path_segments, Vec::new()),
                target: None,
                index: 0,
            },
        })
    }

    /// Whether the run can take `script` after the scripts it took.
    fn removable(&mut self, script: &DeletedScript, check: &mut RunCheck) -> bool {
        let (segments, ordinals) = &script.scope;
        if (self.link_enforcement)
            .read_only_package_for_path(self.service, segments, ordinals)
            .is_some()
        {
            return false;
        }
        let Some(target) = &script.target else {
            return true;
        };
        let Some(document) = self.documents.get(self.service).and_then(Option::as_deref) else {
            return false;
        };
        let Some(children) = self.indexes.get(self.service).map(|index| &index.children) else {
            return false;
        };
        let instance = &document.instances[script.index];
        let Some(parent) = instance.parent_index else {
            return false;
        };
        let references = (self.references)
            .entry(self.service.to_string())
            .or_insert_with(|| StoreReferences::of(document));
        let referred = references.ids.contains(&instance.settings_id)
            || references.paths.contains(&target.path_segments);
        check.indices.insert(script.index)
            && children[script.index].is_empty()
            && is_lua_source_class(&instance.class_name)
            && *(check.parents)
                .entry(parent)
                .or_insert_with(|| stems_are_names(document, &children[parent]))
            && !check.referrers.contains(&instance.settings_id)
            && (!referred || {
                let referrers = (self.referrers)
                    .entry(self.service.to_string())
                    .or_insert_with(|| StoreReferrers::of(document));
                let store = (self.service, children.as_slice());
                check.takes_referred(referrers, document, store, &instance.settings_id)
            })
    }

    /// Removes the scripts of `run` from the store, with `hits`, what
    /// referred to them, in the order of the scripts.
    fn remove(&mut self, run: &[DeletedScript], hits: &[EditorReferenceHit]) -> Result<()> {
        let service = self.service.to_string();
        let slot = self.documents.get_mut(&service).and_then(Option::as_mut);
        let document = Arc::make_mut(slot.context("The deleted scripts' store is not loaded")?);
        let snapshot = self.snapshots.entry(service.clone()).or_default();
        let reached =
            (run.iter()).filter_map(|script| script.target.as_ref()?.settings_id.as_deref());
        let before = snapshot.before_changing(document, reached)?;
        let children = &(self.indexes.get(&service))
            .context("The deleted scripts' store has no index")?
            .children;
        let mut sibling_counts = EditorSiblingGroupCounts::new();
        for script in run.iter().filter(|script| script.target.is_some()) {
            let instance = &document.instances[script.index];
            if let Some(parent) = instance.parent_index {
                let name = instance.name.as_str();
                sibling_counts.entry((parent, name)).or_insert_with(|| {
                    (children[parent].iter().copied())
                        .filter(|sibling| document.instances[*sibling].name == name)
                        .collect()
                });
            }
        }
        let mut indices = Vec::with_capacity(run.len());
        for (position, script) in run.iter().enumerate() {
            if position > 0 {
                self.seen_paths.insert(path_key(&script.path));
                let (segments, ordinals) = &script.scope;
                self.changed_services.record_scope(
                    self.full_reconcile,
                    &service,
                    Some((segments, ordinals)),
                );
            }
            let (Some(target), index) = (&script.target, &script.index) else {
                continue;
            };
            self.changed_services.dirty.insert(service.clone());
            let descriptor = editor_instance_descriptor_from_path(
                document,
                *index,
                target.path_segments.clone(),
                target.path_ordinals.clone(),
                &sibling_counts,
            )
            .context("Failed to describe the deleted source instance")?;
            let settings_id = document.instances[*index].settings_id.clone();
            self.retired_ids
                .entry(service.clone())
                .or_default()
                .insert(settings_id.clone());
            self.changes.instance_changes.push(EditorInstanceChange {
                mode: "deleteInstances".to_string(),
                service: service.clone(),
                allow_deletes: false,
                instances: vec![descriptor],
                preserve_instances: Vec::new(),
            });
            self.changes.history_entries.push(EditorHistoryEntry {
                service: service.clone(),
                source_path: Some(script.path.clone()),
                settings_id: Some(settings_id.clone()),
                path_segments: target.path_segments.clone(),
                path_ordinals: target.path_ordinals.clone(),
                class_name: document.instances[*index].class_name.clone(),
                source_key: Some(editor_source_key_from_target(target)),
                settings_before: Some(Arc::clone(&before)),
            });
            let parent = (document.instances[*index].parent_index)
                .map(|parent| document.instances[parent].settings_id.clone());
            snapshot.touch([settings_id].into_iter().chain(parent));
            indices.push(*index);
        }
        drop(sibling_counts);
        if !hits.is_empty() {
            snapshot.touch(referring_by_index(document, &indices));
        }
        remove_instances_at_indices(document, &indices, true)?;
        append_editor_reference_hit_repairs(self.changes, hits, document, &service);
        self.referrers.remove(&service);
        self.indexes.remove(&service);
        self.stems.remove(&service);
        Ok(())
    }
}

/// Whether every child of a parent has its own name as its file stem: no
/// two share a stem, so removing some of them changes no other's stem or
/// ordinal.
fn stems_are_names(document: &SettingsBytecode, children: &[usize]) -> bool {
    editor_child_stems(document, children)
        .iter()
        .all(|(index, stem, _)| *stem == sanitize_name(&document.instances[*index].name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};
    use crate::editor::history::restore_history_item;
    use crate::editor::paths::build_editor_source_paths_by_index;
    use crate::settings::EXTERNAL_SOURCE_MARKER;
    use crate::settings::bytecode::{SettingsBytecodeInstance, decode_settings_bytecode};
    use crate::settings::tree::WHOLE_STORE_PASSES;
    use crate::tests::support::{
        SimRng, run_seeds, settings_document, settings_instance, temp_dir,
    };

    const SERVICE: &str = "ServerScriptService";
    const NAMES: [&str; 6] = ["Main", "Util", "util", "Shared", "Main", "Helper"];
    const CLASSES: [&str; 3] = ["Script", "LocalScript", "ModuleScript"];

    fn service_dir(root: &Path) -> PathBuf {
        root.join("src").join(SERVICE)
    }

    fn project_file(root: &Path) {
        fs::write(
            root.join("renium.project.jsonc"),
            "{\"schemaVersion\": 1}\n",
        )
        .unwrap();
        config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    }

    fn script(id: String, name: &str, class: &str, parent: usize) -> SettingsBytecodeInstance {
        let mut script = settings_instance(id, name, class, Some(parent));
        script
            .properties
            .insert("Source".to_string(), json!(EXTERNAL_SOURCE_MARKER));
        script
    }

    /// A ServerScriptService of nested folders and scripts with namesakes,
    /// scripts with children, and references by settings id, index and path
    /// into and out of its scripts and their children, each script with its
    /// file.
    fn write_project(root: &Path, rng: &mut SimRng) {
        project_file(root);
        let mut instances = vec![settings_instance("sss", SERVICE, SERVICE, None)];
        let mut containers = vec![0];
        let mut scripts = Vec::new();
        for at in 1..40 + rng.below(40) {
            let parent = rng.pick(&containers).unwrap();
            let id = format!("editor:t{at:x}");
            let instance = match rng.below(8) {
                0 | 1 => {
                    containers.push(instances.len());
                    let name = format!("Folder{}", rng.below(6));
                    settings_instance(id, name, "Folder", Some(parent))
                }
                2 => settings_instance(id, format!("Link{at}"), "ObjectValue", Some(parent)),
                _ => {
                    if rng.below(5) == 0 {
                        containers.push(instances.len());
                    }
                    scripts.push(instances.len());
                    let name = rng.pick(&NAMES).unwrap();
                    script(id, name, rng.pick(&CLASSES).unwrap(), parent)
                }
            };
            instances.push(instance);
        }
        let mut document = settings_document(instances);
        let count = document.instances.len();
        let instance_paths = build_editor_instance_paths(&document, SERVICE);
        for at in 0..count {
            if scripts.is_empty() || rng.below(5) != 0 {
                continue;
            }
            let target = match rng.below(3) {
                0 => 1 + rng.below(count - 1),
                _ => rng.pick(&scripts).unwrap(),
            };
            let path = instance_paths[target].as_ref().unwrap();
            let value = match rng.below(3) {
                0 => json!({"_type": "Ref", "settingsId": document.instances[target].settings_id}),
                1 => json!({"_type": "Ref", "instanceIndex": target + 1}),
                _ => json!({
                    "_type": "Ref",
                    "pathSegments": path.path_segments,
                    "pathOrdinals": path.path_ordinals,
                }),
            };
            document.instances[at]
                .properties
                .insert("Value".to_string(), value);
        }
        let service_dir = service_dir(root);
        document
            .write_file(&service_settings_path(&service_dir))
            .unwrap();
        let paths = build_editor_source_paths_by_index(&document, SERVICE, &service_dir);
        for path in paths.into_iter().flatten() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("-- {}\n", path.display())).unwrap();
        }
    }

    /// Deletes, creates and edits script files, deletes a folder with its
    /// scripts, and returns the changed paths. Some new scripts are made from
    /// a `.lua` file while their `.luau` file is gone: a collection takes the
    /// gone path after the new file and removes the script just made.
    fn change_files(root: &Path, rng: &mut SimRng) -> Vec<PathBuf> {
        let service_dir = service_dir(root);
        let entries = |min_depth| {
            walkdir::WalkDir::new(&service_dir)
                .min_depth(min_depth)
                .into_iter()
                .map(|entry| entry.unwrap().into_path())
                .collect::<Vec<_>>()
        };
        let mut changed = Vec::new();
        for file in entries(0).into_iter().filter(|path| path.is_file()) {
            match rng.below(6) {
                0..=2 => fs::remove_file(&file).unwrap(),
                3 => fs::write(&file, "-- edited\n").unwrap(),
                _ => continue,
            }
            changed.push(file);
        }
        let folders = entries(1)
            .into_iter()
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        if let Some(folder) = rng.pick(&folders) {
            for entry in walkdir::WalkDir::new(&folder) {
                changed.push(entry.unwrap().into_path());
            }
            fs::remove_dir_all(&folder).unwrap();
        }
        let suffixes = [".server.luau", ".client.luau", ".luau"];
        for at in 0..rng.below(12) {
            let mut dir = rng
                .pick(&folders)
                .filter(|folder| folder.is_dir())
                .unwrap_or_else(|| service_dir.clone());
            if rng.below(3) == 0 {
                dir = dir.join(format!("New{}", rng.below(3)));
            }
            let name = match rng.below(4) {
                0 => "init".to_string(),
                1 => rng.pick(&NAMES).unwrap().to_string(),
                _ => format!("Made{at}"),
            };
            let mut file = dir.join(format!("{name}{}", rng.pick(&suffixes).unwrap()));
            fs::create_dir_all(&dir).unwrap();
            if rng.below(2) == 0 {
                changed.push(file.clone());
                file.set_extension("lua");
            }
            fs::write(&file, "-- made\n").unwrap();
            changed.push(file);
        }
        rng.shuffle(&mut changed);
        changed
    }

    fn collect(root: &Path, changed: &[PathBuf], per_file: bool) -> Result<EditorChangeSet> {
        PER_FILE.with(|flag| flag.set(per_file));
        let collected = collect_editor_changes(&PushEditorChangesArgs {
            changed_paths: changed.to_vec(),
            ..PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.to_path_buf(),
                    src_root: PathBuf::from("src"),
                },
                BridgeConnectionArgs::local(0.0),
            )
        });
        PER_FILE.with(|flag| flag.set(false));
        collected
    }

    fn written(changes: &EditorChangeSet) -> Option<&SettingsBytecode> {
        changes
            .settings_writes
            .iter()
            .find(|write| write.path.to_string_lossy().contains(SERVICE))
            .map(|write| &*write.document)
    }

    /// What undoing a history entry does to `current`.
    fn undone(current: &SettingsBytecode, entry: &EditorHistoryEntry) -> Option<String> {
        let backup = decode_settings_bytecode(entry.settings_before.as_ref()?).unwrap();
        let exists = entry
            .source_path
            .as_ref()
            .is_some_and(|path| path.is_file());
        let id = entry.settings_id.as_deref().unwrap_or_default();
        Some(
            match restore_history_item(current, &backup, id, None, exists) {
                Ok((document, outcome)) => {
                    format!("{outcome:?} {:?}", encode_settings_bytecode(&document).ok())
                }
                Err(error) => format!("refused: {error:#}"),
            },
        )
    }

    fn summary(changes: &EditorChangeSet) -> Value {
        let entries = changes
            .history_entries
            .iter()
            .map(|entry| {
                json!([
                    entry.service,
                    entry.source_path,
                    entry.settings_id,
                    entry.path_segments,
                    entry.path_ordinals,
                    entry.class_name,
                    entry.source_key,
                    entry.settings_before.is_some(),
                ])
            })
            .collect::<Vec<_>>();
        json!([
            changes.instance_changes,
            changes.property_changes,
            changes.source_changes,
            entries,
            written(changes)
                .map(|document| format!("{:?}", encode_settings_bytecode(document).ok())),
        ])
    }

    /// The collection of `changed` one file at a time and with the work the
    /// files share give the same change set, store and history effect, the
    /// last also where an entry's instance is gone.
    fn compare(root: &Path, changed: &[PathBuf]) -> std::result::Result<(), String> {
        let text = |error: &anyhow::Error| format!("{error:#}");
        let (one_by_one, together) =
            match (collect(root, changed, true), collect(root, changed, false)) {
                (Ok(one_by_one), Ok(together)) => (one_by_one, together),
                (Err(one_by_one), Err(together)) if text(&one_by_one) == text(&together) => {
                    return Ok(());
                }
                (one_by_one, together) => {
                    return Err(format!(
                        "one by one {:?}, together {:?}",
                        one_by_one.err().as_ref().map(text),
                        together.err().as_ref().map(text)
                    ));
                }
            };
        if summary(&one_by_one) != summary(&together) {
            return Err(format!(
                "the collections differ:\n{}\n{}",
                summary(&one_by_one),
                summary(&together)
            ));
        }
        let original = SettingsBytecode::read_file(&service_settings_path(&service_dir(root)))
            .map_err(|error| text(&error))?;
        let after = written(&together).unwrap_or(&original);
        let entries = one_by_one
            .history_entries
            .iter()
            .zip(&together.history_entries);
        for (left, right) in entries {
            let mut gone = after.clone();
            if let Some(index) = left
                .settings_id
                .as_deref()
                .and_then(|id| document_instance_index_by_settings_id(&gone, id))
            {
                remove_instances_at_indices(&mut gone, &[index], true).unwrap();
            }
            for current in [&original, after, &gone] {
                if undone(current, left) != undone(current, right) {
                    return Err(format!(
                        "undoing {:?} differs: {:?} / {:?}",
                        left.source_path,
                        undone(current, left),
                        undone(current, right)
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn collecting_many_scripts_together_matches_collecting_them_one_by_one() {
        run_seeds(48, |seed, log| {
            let mut rng = SimRng(seed);
            let root = temp_dir(&format!("bulk-scripts-{seed}"));
            write_project(&root, &mut rng);
            let changed = change_files(&root, &mut rng);
            log.push(format!("{changed:#?}"));
            let outcome = compare(&root, &changed);
            let _ = fs::remove_dir_all(&root);
            outcome
        });
    }

    /// Collects the script files `changed` names, given from the service's
    /// directory, from a store of `instances`: the ones that end in
    /// `.client.luau` written, the others gone. Compares collecting them one
    /// by one and together, and returns the history entries they make.
    fn compare_files(
        name: &str,
        instances: Vec<SettingsBytecodeInstance>,
        changed: &[&str],
    ) -> usize {
        let root = temp_dir(name);
        project_file(&root);
        let service_dir = service_dir(&root);
        settings_document(instances)
            .write_file(&service_settings_path(&service_dir))
            .unwrap();
        let changed = (changed.iter())
            .map(|path| (path.split('/')).fold(service_dir.clone(), |dir, part| dir.join(part)))
            .collect::<Vec<_>>();
        for file in changed
            .iter()
            .filter(|file| file.to_string_lossy().ends_with(".client.luau"))
        {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "-- renamed\n").unwrap();
        }
        let entries = collect(&root, &changed, false).map(|changes| changes.history_entries.len());
        let outcome = compare(&root, &changed);
        let _ = fs::remove_dir_all(&root);
        outcome.unwrap();
        entries.unwrap()
    }

    /// `compare_files` of deleted script files, each with its entry.
    fn compare_deleted(name: &str, instances: Vec<SettingsBytecodeInstance>, changed: &[&str]) {
        assert_eq!(compare_files(name, instances, changed), changed.len());
    }

    fn with_reference(
        id: &str,
        name: &str,
        class: &str,
        parent: usize,
        target: &str,
    ) -> SettingsBytecodeInstance {
        let mut instance = settings_instance(id, name, class, Some(parent));
        let value = json!({"_type": "Ref", "settingsId": target});
        instance.properties.insert("Value".to_string(), value);
        instance
    }

    /// A script whose file sits beside the folder of its child, deleted with
    /// the child: the child goes first and leaves the script with fewer
    /// children, so undoing the script reads a store taken after that.
    #[test]
    fn a_script_deleted_after_its_child_is_undone_without_the_child() {
        compare_deleted(
            "bulk-script-after-child",
            vec![
                settings_instance("sss", SERVICE, SERVICE, None),
                script("editor:a".to_string(), "A", "Script", 0),
                script("editor:d".to_string(), "D", "ModuleScript", 1),
            ],
            &["A.server.luau", "A/D.luau"],
        );
    }

    /// A script that refers to a script deleted before it: that deletion
    /// strips the reference, so undoing the second where it is gone puts it
    /// back without the reference.
    #[test]
    fn a_script_is_undone_without_the_reference_an_earlier_deletion_stripped() {
        compare_deleted(
            "bulk-script-after-reference",
            vec![
                settings_instance("sss", SERVICE, SERVICE, None),
                script("editor:a".to_string(), "A", "Script", 0),
                with_reference("editor:r", "R", "Script", 0, "editor:a"),
            ],
            &["A.server.luau", "R.server.luau"],
        );
    }

    /// An init script whose child refers to a script deleted before it: the
    /// init script turns into a folder, and undoing it where it is gone puts
    /// back its child without the reference that deletion stripped.
    #[test]
    fn an_init_script_is_undone_with_its_child_as_an_earlier_deletion_left_it() {
        compare_deleted(
            "bulk-init-after-reference",
            vec![
                settings_instance("sss", SERVICE, SERVICE, None),
                script("editor:a".to_string(), "A", "Script", 0),
                script("editor:x".to_string(), "X", "Script", 0),
                with_reference("editor:d", "D", "ObjectValue", 2, "editor:a"),
            ],
            &["A.server.luau", "X/init.server.luau"],
        );
    }

    /// Scripts that something refers to, deleted in one run that repairs
    /// each reference, then a script whose child referred to the first: it
    /// is undone with its child as the run left it, without the reference.
    #[test]
    fn scripts_something_refers_to_are_deleted_in_one_run() {
        compare_deleted(
            "bulk-referred-run",
            vec![
                settings_instance("sss", SERVICE, SERVICE, None),
                script("editor:a".to_string(), "A", "Script", 0),
                script("editor:b".to_string(), "B", "Script", 0),
                script("editor:c".to_string(), "C", "Script", 0),
                script("editor:d".to_string(), "D", "Script", 0),
                with_reference("editor:la", "LA", "ObjectValue", 4, "editor:a"),
                with_reference("editor:lb", "LB", "ObjectValue", 0, "editor:b"),
                with_reference("editor:lc", "LC", "ObjectValue", 0, "editor:c"),
            ],
            &[
                "A.server.luau",
                "B.server.luau",
                "C.server.luau",
                "D.server.luau",
            ],
        );
    }

    /// Scripts that one instance refers to: the first two through one value,
    /// which removing the first changes, so the second does not join the
    /// first's run, and the third through another value, so it joins the
    /// run of the second.
    #[test]
    fn scripts_one_value_refers_to_go_one_at_a_time() {
        let reference = |id: &str| json!({"_type": "Ref", "settingsId": id});
        let mut list = with_reference("editor:l", "L", "ObjectValue", 0, "editor:c");
        let both = json!([reference("editor:a"), reference("editor:b")]);
        list.properties.insert("Targets".to_string(), both);
        compare_deleted(
            "bulk-shared-value",
            vec![
                settings_instance("sss", SERVICE, SERVICE, None),
                script("editor:a".to_string(), "A", "Script", 0),
                script("editor:b".to_string(), "B", "Script", 0),
                script("editor:c".to_string(), "C", "Script", 0),
                list,
            ],
            &["A.server.luau", "B.server.luau", "C.server.luau"],
        );
    }

    /// Scripts renamed to `.client.luau` before and after deletions that
    /// move the instances behind them: a run of two, and one referred to
    /// that goes alone. The last renamed script is referred to by index, so
    /// its rename finds that reference where the deletions left it.
    #[test]
    fn a_script_renamed_after_deletions_finds_the_references_to_it() {
        let instances = || {
            let mut instances = vec![settings_instance("sss", SERVICE, SERVICE, None)];
            for name in ["A", "B", "C", "D", "E"] {
                let id = format!("editor:{}", name.to_lowercase());
                instances.push(script(id, name, "Script", 0));
            }
            instances.push(with_reference(
                "editor:l",
                "L",
                "ObjectValue",
                0,
                "editor:d",
            ));
            instances.push(with_reference(
                "editor:m",
                "M",
                "ObjectValue",
                0,
                "editor:e",
            ));
            instances
        };
        let rename = ["A.client.luau", "A.server.luau"];
        let run = ["B.server.luau", "C.server.luau"];
        let alone = ["D.server.luau"];
        let renamed = ["E.client.luau", "E.server.luau"];
        for (name, changed, entries) in [
            (
                "bulk-rename-after-run",
                [&rename[..], &run[..], &renamed[..]].concat(),
                6,
            ),
            (
                "bulk-rename-after-one",
                [&rename[..], &alone[..], &renamed[..]].concat(),
                5,
            ),
        ] {
            assert_eq!(
                compare_files(name, instances(), &changed),
                entries,
                "{name}"
            );
        }
    }

    /// The hits read from the referrers of a store, and the repairs made of
    /// them, are the ones a scan of the whole store finds, for every target:
    /// where references name their targets by index, settings id or path,
    /// also inside arrays and objects, where settings ids repeat, and where
    /// instances were added after the referrers were read, and again after
    /// the first repairs, with references of their own, at an index a
    /// reference named beyond the store's end.
    #[test]
    fn referrers_find_the_hits_a_scan_of_the_whole_store_finds() {
        fn grow(document: &mut SettingsBytecode, rng: &mut SimRng, count: usize) {
            for _ in 0..count {
                let at = document.instances.len();
                let id = match rng.below(8) {
                    0 => format!("editor:t{:x}", rng.below(at)),
                    _ => format!("editor:t{at:x}"),
                };
                let name = rng.pick(&NAMES).unwrap();
                let parent = Some(rng.below(at));
                (document.instances).push(settings_instance(id, name, "Folder", parent));
            }
        }
        fn refer(document: &mut SettingsBytecode, rng: &mut SimRng, from: usize) {
            let count = document.instances.len();
            let paths = build_editor_instance_paths(document, SERVICE);
            for at in from..count {
                if rng.below(3) == 0 {
                    continue;
                }
                let target = rng.below(count + 3);
                let id = (document.instances.get(target)).map_or_else(
                    || format!("editor:t{target:x}"),
                    |target| target.settings_id.clone(),
                );
                let path = paths.get(target).cloned().flatten();
                let key = rng
                    .pick(&["settingsId", "instanceId", "referent", "ref"])
                    .unwrap();
                let reference = match (rng.below(4), path) {
                    (0, _) => json!({"_type": "Ref", "instanceIndex": target + 1}),
                    (1, Some(path)) => json!({"_type": "Ref", "pathSegments": path.path_segments}),
                    (2, Some(path)) => json!({
                        "_type": "Ref",
                        "pathSegments": path.path_segments,
                        "pathOrdinals": path.path_ordinals,
                    }),
                    _ => {
                        let mut reference = Map::new();
                        reference.insert("_type".to_string(), json!("Ref"));
                        reference.insert(key.to_string(), json!(id));
                        Value::Object(reference)
                    }
                };
                let value = match rng.below(3) {
                    0 => reference,
                    1 => json!([1, reference]),
                    _ => json!({"inner": reference}),
                };
                let instance = &mut document.instances[at];
                let values = match rng.below(2) {
                    0 => &mut instance.properties,
                    _ => &mut instance.attributes,
                };
                values.insert(format!("Value{}", rng.below(2)), value);
            }
        }
        let hit = |hit: EditorReferenceHit| (hit.settings_id, hit.properties, hit.attributes);
        run_seeds(64, |seed, _| {
            let mut rng = SimRng(seed);
            let root = settings_instance("sss", SERVICE, SERVICE, None);
            let mut document = settings_document(vec![root]);
            let count = 20 + rng.below(30);
            grow(&mut document, &mut rng, count);
            refer(&mut document, &mut rng, 0);
            let mut referrers =
                HashMap::from([(SERVICE.to_string(), StoreReferrers::of(&document))]);
            for _ in 0..2 {
                let from = document.instances.len();
                let added = rng.below(8);
                grow(&mut document, &mut rng, added);
                refer(&mut document, &mut rng, from);
                let children = settings_children_by_parent(&document);
                for target in &document.instances {
                    let id = target.settings_id.as_str();
                    let scanned = editor_reference_hits(&document, SERVICE, id);
                    let mut scanned_repairs = EditorChangeSet::default();
                    append_editor_reference_hit_repairs(
                        &mut scanned_repairs,
                        &scanned,
                        &document,
                        SERVICE,
                    );
                    let read = referrers[SERVICE].hits(&document, SERVICE, &children, id);
                    let (scanned, read) = (
                        scanned.into_iter().map(hit).collect::<Vec<_>>(),
                        read.into_iter().map(hit).collect::<Vec<_>>(),
                    );
                    if scanned != read {
                        return Err(format!("{id}: scanned {scanned:?}, read {read:?}"));
                    }
                    let mut read_repairs = EditorChangeSet::default();
                    repair_references(
                        &mut referrers,
                        &mut read_repairs,
                        &document,
                        SERVICE,
                        &children,
                        id,
                    );
                    let (scanned, read) = (
                        json!(scanned_repairs.property_changes),
                        json!(read_repairs.property_changes),
                    );
                    if scanned != read {
                        return Err(format!("{id}: scanned {scanned}, repaired {read}"));
                    }
                }
            }
            Ok(())
        });
    }

    /// What a push in the tests of whole-store passes does to its scripts.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Push {
        Deleted,
        Blocked,
        Referred,
        Created,
        Renamed,
        Init,
    }

    /// The whole-store passes a collection makes of `scripts` script files
    /// in a ServerScriptService of 3,000 other instances, deleted, created,
    /// renamed from `.server.luau` to `.client.luau`, or deleted as the
    /// `init` files of scripts with a child each. Deleted `Blocked`, the
    /// script in the middle has a child, so it joins no run, and something
    /// refers to the last one, whose removal repairs the reference; the same
    /// reference is repaired when the last is renamed or turns into a
    /// folder. Deleted `Referred`, something refers to each script, and each
    /// reference is repaired. The scripts' names sort as they are numbered,
    /// and their files come in that order. A created or renamed script has
    /// the history entry of its instance and that of its Source.
    fn passes(push: Push, scripts: usize, per_file: bool) -> usize {
        let root = temp_dir(&format!("bulk-passes-{push:?}-{scripts}-{per_file}"));
        project_file(&root);
        let mut instances = vec![settings_instance("sss", SERVICE, SERVICE, None)];
        for at in 0..3_000 {
            let id = format!("editor:r{at:x}");
            instances.push(settings_instance(id, format!("Row{at}"), "Folder", Some(0)));
        }
        let bulk = instances.len();
        instances.push(settings_instance("editor:bulk", "Bulk", "Folder", Some(0)));
        let file = |at: usize, suffix: &str| {
            service_dir(&root)
                .join("Bulk")
                .join(format!("S{at:04}.{suffix}.luau"))
        };
        let mut files = Vec::new();
        for at in 0..scripts {
            if push == Push::Created {
                files.push(file(at, "server"));
                fs::create_dir_all(file(at, "server").parent().unwrap()).unwrap();
                fs::write(file(at, "server"), "-- made\n").unwrap();
                continue;
            }
            let id = format!("editor:b{at:x}");
            instances.push(script(id, &format!("S{at:04}"), "Script", bulk));
            if push == Push::Init {
                let id = format!("editor:c{at:x}");
                let parent = Some(instances.len() - 1);
                instances.push(settings_instance(id, "Child", "Folder", parent));
                let folder = service_dir(&root).join("Bulk").join(format!("S{at:04}"));
                files.push(folder.join("init.server.luau"));
                continue;
            }
            if push == Push::Renamed {
                files.push(file(at, "client"));
                fs::create_dir_all(file(at, "client").parent().unwrap()).unwrap();
                fs::write(file(at, "client"), "-- renamed\n").unwrap();
            }
            files.push(file(at, "server"));
        }
        if push == Push::Blocked {
            let middle = bulk + 1 + scripts / 2;
            instances.push(settings_instance(
                "editor:child",
                "Child",
                "Folder",
                Some(middle),
            ));
        }
        let linked = match push {
            Push::Referred => 0..scripts,
            Push::Blocked | Push::Renamed | Push::Init => scripts - 1..scripts,
            Push::Deleted | Push::Created => 0..0,
        };
        for at in linked.clone() {
            let (id, target) = (format!("editor:link{at:x}"), format!("editor:b{at:x}"));
            instances.push(with_reference(&id, "Link", "ObjectValue", 0, &target));
        }
        settings_document(instances)
            .write_file(&service_settings_path(&service_dir(&root)))
            .unwrap();
        WHOLE_STORE_PASSES.with(|passes| passes.set(0));
        let changes = collect(&root, &files, per_file).unwrap();
        let passes = WHOLE_STORE_PASSES.with(std::cell::Cell::get);
        let entries = match push {
            Push::Deleted | Push::Blocked | Push::Referred | Push::Init => scripts,
            Push::Created | Push::Renamed => 2 * scripts,
        };
        assert_eq!(changes.history_entries.len(), entries, "{push:?}");
        let repairs = (changes.property_changes.iter())
            .filter(|change| {
                (change.settings_id.as_deref()).is_some_and(|id| id.starts_with("editor:link"))
            })
            .count();
        assert_eq!(repairs, linked.len(), "{push:?}");
        let _ = fs::remove_dir_all(&root);
        passes
    }

    /// One file at a time reads the store again for each file, which the
    /// count catches.
    #[test]
    fn many_changed_scripts_read_their_store_a_fixed_number_of_times() {
        let pushes = [
            Push::Deleted,
            Push::Blocked,
            Push::Referred,
            Push::Created,
            Push::Renamed,
            Push::Init,
        ];
        for push in pushes {
            let few = passes(push, 10, false);
            let many = passes(push, 200, false);
            assert_eq!(few, many, "{push:?}: {few} passes for 10, {many} for 200");
            let one_by_one = passes(push, 200, true) - passes(push, 10, true);
            assert!(one_by_one >= 190, "{push:?}: {one_by_one}");
        }
    }
}
