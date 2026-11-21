//! What a collection of many changed script files shares instead of reading
//! a whole store for each file: one copy of the store for the history
//! entries, and one removal for a run of deleted scripts.

use super::*;
use crate::editor::diff::{editor_instance_descriptor_from_path, editor_sibling_group_counts};
use crate::editor::document::EditorDocumentIndex;
use crate::editor::paths::editor_source_index_with_cache;
use crate::editor::types::EditorSourcePathSpec;
use crate::settings::tree::editor_child_stems;
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
/// change. Undoing an entry reads from it whether it held the entry's
/// instance and, where it did, that instance's record, parent and subtree.
/// Entries share one copy where those are as they were just before each
/// one's change: `touched` names the instances added, removed or left with
/// fewer children since the copy was taken.
#[derive(Default)]
pub(super) struct HistorySnapshot {
    copy: Option<Arc<SettingsBytecode>>,
    touched: HashSet<String>,
}

impl HistorySnapshot {
    fn shared(&mut self, document: &SettingsBytecode) -> Arc<SettingsBytecode> {
        if self.copy.is_none() {
            self.touched.clear();
        }
        Arc::clone(self.copy.get_or_insert_with(|| Arc::new(document.clone())))
    }

    pub(super) fn forget(&mut self) {
        self.copy = None;
        self.touched.clear();
    }

    /// The store for the entry of a script `spec` creates: undoing a new
    /// instance reads only that the store did not hold it, while one the
    /// store held keeps the document as it stands.
    pub(super) fn before_creating(
        &mut self,
        document: &SettingsBytecode,
        index: &EditorDocumentIndex,
        spec: &EditorSourcePathSpec,
    ) -> Arc<SettingsBytecode> {
        if per_file() || index.source_target(document, spec).is_some() {
            self.forget();
            return Arc::new(document.clone());
        }
        self.shared(document)
    }

    /// Notes the instances an ensure added from `added_from` on.
    pub(super) fn created(&mut self, document: &SettingsBytecode, added_from: usize) {
        self.touched.extend(
            document.instances[added_from..]
                .iter()
                .map(|instance| instance.settings_id.clone()),
        );
    }
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

/// `read_whole_for_edit` of a service's document the first time the
/// collection creates a script in it; the document stays whole after.
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
    Option<fs::Metadata>,
    Option<std::io::Result<String>>,
);

/// What the values of a store refer to: instances by settings id, those it
/// names by index as well, and paths. A collection reads it once for each
/// service, as removing instances and adding scripts leave every reference
/// the store holds as it was.
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

/// A deleted script file of a run: the scope the collection records for it
/// and the instance it pointed at, with that instance's index.
struct DeletedScript {
    path: PathBuf,
    scope: (Vec<String>, Vec<usize>),
    target: Option<EditorSourceTarget>,
    index: usize,
}

/// What a run checked so far: the instances it takes and whether each
/// parent's children have their names as file stems.
#[derive(Default)]
struct RunCheck {
    indices: HashSet<usize>,
    parents: HashMap<usize, bool>,
}

/// Deleted script files of one service that follow each other in a
/// collection, removed from the store in one step. The run takes the files
/// where removing their scripts one at a time leaves every other path,
/// ordinal and reference as it was: each script is a leaf whose siblings all
/// keep their names as file stems, and nothing refers to any of them. It
/// ends before the first file that is not such a script, which the
/// collection takes on its own. Each file of the run gets the change set,
/// history entry and store result it gets alone.
pub(super) struct DeletedScriptRun<'a> {
    pub(super) src_root: &'a Path,
    pub(super) service: &'a str,
    pub(super) full_reconcile: bool,
    pub(super) link_enforcement: &'a LinkEnforcement,
    pub(super) instance_paths: &'a HashSet<String>,
    pub(super) documents: &'a mut HashMap<String, Option<Arc<SettingsBytecode>>>,
    pub(super) read_whole: &'a mut HashSet<String>,
    pub(super) references: &'a mut HashMap<String, StoreReferences>,
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
        for (path, metadata, _) in following {
            match self.follower(path, metadata.as_ref(), &keys) {
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
        self.remove(&run)?;
        Ok(Some(run.len() - 1))
    }

    /// The next file of the run: a deleted script of the same service that
    /// the collection has not seen.
    fn follower(
        &mut self,
        path: &Path,
        metadata: Option<&fs::Metadata>,
        keys: &HashSet<String>,
    ) -> Option<DeletedScript> {
        let key = path_key(path);
        if metadata.is_some()
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
        check.indices.insert(script.index)
            && children[script.index].is_empty()
            && is_lua_source_class(&instance.class_name)
            && *(check.parents)
                .entry(parent)
                .or_insert_with(|| stems_are_names(document, &children[parent]))
            && !references.ids.contains(&instance.settings_id)
            && !references.paths.contains(&target.path_segments)
    }

    fn remove(&mut self, run: &[DeletedScript]) -> Result<()> {
        let service = self.service.to_string();
        let slot = self.documents.get_mut(&service).and_then(Option::as_mut);
        let document = Arc::make_mut(slot.context("The deleted scripts' store is not loaded")?);
        let snapshot = self.snapshots.entry(service.clone()).or_default();
        if run
            .iter()
            .filter_map(|script| script.target.as_ref()?.settings_id.as_ref())
            .any(|id| snapshot.touched.contains(id))
        {
            snapshot.forget();
        }
        let before = snapshot.shared(document);
        let sibling_counts = editor_sibling_group_counts(document);
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
            snapshot.touched.insert(settings_id);
            if let Some(parent) = document.instances[*index].parent_index {
                let parent = document.instances[parent].settings_id.clone();
                snapshot.touched.insert(parent);
            }
            indices.push(*index);
        }
        drop(sibling_counts);
        remove_instances_at_indices(document, &indices, true)?;
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
    use crate::settings::bytecode::{SettingsBytecodeInstance, encode_settings_bytecode};
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
    /// scripts with children, and references into and out of its scripts,
    /// each script with its file.
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
        for at in 0..instances.len() {
            if scripts.is_empty() || rng.below(5) != 0 {
                continue;
            }
            let target = &instances[rng.pick(&scripts).unwrap()];
            let value = json!({"_type": "Ref", "settingsId": target.settings_id});
            instances[at].properties.insert("Value".to_string(), value);
        }
        let document = settings_document(instances);
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
    /// scripts, and returns the changed paths.
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
                // The script is made from its `.lua` file, and its `.luau`
                // file is gone: a collection takes that path after the new
                // file and removes the script just made.
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
            .map(|write| &write.document)
    }

    /// What undoing a history entry does to `current`.
    fn undone(current: &SettingsBytecode, entry: &EditorHistoryEntry) -> Option<String> {
        let backup = entry.settings_before.as_ref()?;
        let exists = entry
            .source_path
            .as_ref()
            .is_some_and(|path| path.is_file());
        let id = entry.settings_id.as_deref().unwrap_or_default();
        Some(
            match restore_history_item(current, backup, id, None, exists) {
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
    /// files share give the same change set, store and history effect.
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
            for current in [&original, after] {
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

    /// The whole-store passes a collection makes of `scripts` deleted or
    /// created script files in a ServerScriptService of 3,000 other
    /// instances. With `blocked`, the deleted script in the middle has a
    /// child and something refers to the last one, so neither joins a run.
    /// The scripts' names sort as they are numbered, and their files come in
    /// that order.
    fn passes(deleted: bool, blocked: bool, scripts: usize, per_file: bool) -> usize {
        let root = temp_dir(&format!(
            "bulk-passes-{deleted}-{blocked}-{scripts}-{per_file}"
        ));
        project_file(&root);
        let mut instances = vec![settings_instance("sss", SERVICE, SERVICE, None)];
        for at in 0..3_000 {
            let id = format!("editor:r{at:x}");
            instances.push(settings_instance(id, format!("Row{at}"), "Folder", Some(0)));
        }
        let bulk = instances.len();
        instances.push(settings_instance("editor:bulk", "Bulk", "Folder", Some(0)));
        let files = (0..scripts)
            .map(|at| {
                service_dir(&root)
                    .join("Bulk")
                    .join(format!("S{at:04}.server.luau"))
            })
            .collect::<Vec<_>>();
        if deleted {
            for at in 0..scripts {
                instances.push(script(
                    format!("editor:b{at:x}"),
                    &format!("S{at:04}"),
                    "Script",
                    bulk,
                ));
            }
            if blocked {
                let middle = bulk + 1 + scripts / 2;
                let child = settings_instance("editor:child", "Child", "Folder", Some(middle));
                let mut link = settings_instance("editor:link", "Link", "ObjectValue", Some(0));
                let last = format!("editor:b{:x}", scripts - 1);
                let value = json!({"_type": "Ref", "settingsId": last});
                link.properties.insert("Value".to_string(), value);
                instances.extend([child, link]);
            }
        } else {
            for file in &files {
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, "-- made\n").unwrap();
            }
        }
        settings_document(instances)
            .write_file(&service_settings_path(&service_dir(&root)))
            .unwrap();
        WHOLE_STORE_PASSES.with(|passes| passes.set(0));
        let changes = collect(&root, &files, per_file).unwrap();
        let passes = WHOLE_STORE_PASSES.with(std::cell::Cell::get);
        // A new script has the entry of its instance and that of its Source.
        let entries = if deleted { scripts } else { 2 * scripts };
        assert_eq!(changes.history_entries.len(), entries);
        let _ = fs::remove_dir_all(&root);
        passes
    }

    #[test]
    fn many_deleted_or_created_scripts_read_their_store_a_fixed_number_of_times() {
        for (deleted, blocked) in [(true, false), (true, true), (false, false)] {
            let few = passes(deleted, blocked, 10, false);
            let many = passes(deleted, blocked, 200, false);
            assert_eq!(
                few, many,
                "deleted {deleted}, blocked {blocked}: {few} passes for 10, {many} for 200"
            );
            // One file at a time reads the store again for each file, which
            // the count catches.
            let one_by_one =
                passes(deleted, blocked, 200, true) - passes(deleted, blocked, 10, true);
            assert!(one_by_one >= 190, "deleted {deleted}: {one_by_one}");
        }
    }
}
