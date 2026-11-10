//! References between instances as the stores hold them: completing the ones
//! a command writes, and keeping the stored ones on their targets when an
//! edit renames, moves or removes instances.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::bytecode::edit::{instance_path_key, instance_path_parts_key};
use crate::bytecode::explorer::compact_path_string;
use crate::editor::paths::{build_editor_instance_paths, build_editor_instance_paths_for_indices};
use crate::editor::types::EditorInstancePath;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, strict_reference_path,
    visit_reference_objects_mut,
};
use crate::settings::tree::editor_service_root_index;
use crate::system::files::{service_settings_path, validate_filesystem_instance_name};

/// A reference as a command writes it by hand: by settings id alone, or by
/// path alone, with the service the path leads into.
enum WrittenReference {
    SettingsId(String),
    Path(String),
}

fn written_reference(object: &Map<String, Value>) -> Option<WrittenReference> {
    let typed = object.get("_type").and_then(Value::as_str) == Some("Ref");
    let id = object
        .get("settingsId")
        .or_else(|| object.get("instanceId"));
    let segments = object
        .get("pathSegments")
        .and_then(Value::as_array)
        .filter(|segments| !segments.is_empty());
    match (id, segments) {
        (Some(id), None) => id
            .as_str()
            .map(|id| WrittenReference::SettingsId(id.to_string())),
        (None, Some(segments))
            if typed
                && !["instanceIndex", "debugId", "path", "referent", "ref"]
                    .iter()
                    .any(|selector| object.contains_key(*selector)) =>
        {
            segments[0]
                .as_str()
                .map(|service| WrittenReference::Path(service.to_string()))
        }
        _ => None,
    }
}

/// What the references in a written value leave open: settings ids given
/// without a path, and the services that paths given without an id lead into.
#[derive(Default)]
pub(crate) struct WrittenReferences {
    settings_ids: BTreeSet<String>,
    path_services: BTreeSet<String>,
}

impl WrittenReferences {
    pub(crate) fn of(value: &Value) -> Self {
        let mut written = Self::default();
        written.collect(value);
        written
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.settings_ids.is_empty() && self.path_services.is_empty()
    }

    /// Whether a settings id was given alone; its store is not known, so
    /// every store of the project is searched for it.
    pub(crate) fn needs_every_store(&self) -> bool {
        !self.settings_ids.is_empty()
    }

    pub(crate) fn path_services(&self) -> impl Iterator<Item = &str> {
        self.path_services.iter().map(String::as_str)
    }

    fn collect(&mut self, value: &Value) {
        match value {
            Value::Array(values) => values.iter().for_each(|value| self.collect(value)),
            Value::Object(object) => {
                match written_reference(object) {
                    Some(WrittenReference::SettingsId(id)) => {
                        self.settings_ids.insert(id);
                    }
                    Some(WrittenReference::Path(service)) => {
                        self.path_services.insert(service);
                    }
                    None => {}
                }
                object.values().for_each(|value| self.collect(value));
            }
            _ => {}
        }
    }
}

struct ReferenceTargetPath {
    service: String,
    path: EditorInstancePath,
}

/// What becomes of a path reference that names no instance of its store.
#[derive(Clone, Copy)]
pub(crate) enum MissingTarget {
    /// It stays as written, for a second look once the command has added
    /// its instances: an instance may refer to itself, and a batch to an
    /// instance one of its later ops adds.
    Defer,
    /// It is refused as the write of one command.
    Refuse,
    /// It is refused as what a batch leaves when its last op has run.
    RefuseAfterBatch,
}

/// Completes the hand-written references in `value` against the stores in
/// `documents`, by service name. A reference by settings id gains its
/// target's path, as a pull stores it, so both sides compare equal. A
/// reference by path gains its target's id and ordinals, so it stays on the
/// target through later renames and moves. A path that names no instance of
/// its store is refused or left for later, as `missing` says, and one into a
/// service without a store cannot be checked and stays as written. Returns
/// whether a reference was left for later.
pub(crate) fn qualify_reference_targets(
    value: &mut Value,
    documents: &BTreeMap<&str, &SettingsBytecode>,
    written: &WrittenReferences,
    missing: MissingTarget,
) -> Result<bool> {
    let mut targets_by_settings_id = HashMap::<&str, Vec<ReferenceTargetPath>>::new();
    for (service, document) in documents {
        if written.settings_ids.is_empty() {
            break;
        }
        let indices = document
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| written.settings_ids.contains(&instance.settings_id))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let mut paths = build_editor_instance_paths_for_indices(document, service, &indices);
        for index in indices {
            if let Some(path) = paths.remove(&index) {
                targets_by_settings_id
                    .entry(document.instances[index].settings_id.as_str())
                    .or_default()
                    .push(ReferenceTargetPath {
                        service: service.to_string(),
                        path,
                    });
            }
        }
    }
    let targets = QualifyTargets {
        documents,
        by_settings_id: &targets_by_settings_id,
        missing,
    };
    qualify(value, &targets)
}

struct QualifyTargets<'a> {
    documents: &'a BTreeMap<&'a str, &'a SettingsBytecode>,
    by_settings_id: &'a HashMap<&'a str, Vec<ReferenceTargetPath>>,
    missing: MissingTarget,
}

fn qualify(value: &mut Value, targets: &QualifyTargets<'_>) -> Result<bool> {
    let mut deferred = false;
    match value {
        Value::Array(values) => {
            for value in values {
                deferred |= qualify(value, targets)?;
            }
        }
        Value::Object(object) => {
            match written_reference(object) {
                Some(WrittenReference::SettingsId(id)) => {
                    let found = targets
                        .by_settings_id
                        .get(id.as_str())
                        .map(Vec::as_slice)
                        .unwrap_or_default();
                    qualify_settings_id(object, &id, found)?;
                }
                Some(WrittenReference::Path(service)) => {
                    match targets.documents.get(service.as_str()) {
                        Some(document) => {
                            deferred |= qualify_path(object, document, targets.missing)?;
                        }
                        // A later op of the batch may create the store.
                        None => deferred |= matches!(targets.missing, MissingTarget::Defer),
                    }
                }
                None => {}
            }
            for nested in object.values_mut() {
                deferred |= qualify(nested, targets)?;
            }
        }
        _ => {}
    }
    Ok(deferred)
}

fn qualify_settings_id(
    reference: &mut Map<String, Value>,
    settings_id: &str,
    targets: &[ReferenceTargetPath],
) -> Result<()> {
    let [target] = targets else {
        if targets.is_empty() {
            bail!("Reference target was not found: {settings_id}");
        }
        let services = targets
            .iter()
            .map(|target| target.service.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "Reference target is ambiguous: {settings_id} exists in {services}; include pathSegments and pathOrdinals"
        );
    };
    reference.insert("pathSegments".to_string(), json!(target.path.path_segments));
    reference.insert("pathOrdinals".to_string(), json!(target.path.path_ordinals));
    Ok(())
}

/// Completes one path reference; returns whether it was left for later.
fn qualify_path(
    reference: &mut Map<String, Value>,
    document: &SettingsBytecode,
    missing: MissingTarget,
) -> Result<bool> {
    let (segments, ordinals) =
        strict_reference_path(reference)?.context("Reference has no pathSegments")?;
    let service = segments[0].as_str();
    let matches = instances_at_path(document, &segments, ordinals.as_deref());
    let [index] = matches[..] else {
        let path = compact_path_string(&segments, ordinals.as_deref())
            .unwrap_or_else(|| segments.join("."));
        if !matches.is_empty() {
            bail!(
                "Reference target is ambiguous: {} instances are at {path}; give the ordinal, as in {path}[2]",
                matches.len()
            );
        }
        let lookup = format!(
            "`rbx f {service} NAME` finds a path, `rbx pl` refreshes the store from Studio"
        );
        match missing {
            MissingTarget::Defer => return Ok(true),
            MissingTarget::Refuse => bail!(
                "Reference target was not found: {path} is not in the {service} store; create the target first or add both in one `rbx ba --batch` ({lookup})"
            ),
            MissingTarget::RefuseAfterBatch => bail!(
                "Reference target was not found: {path} is not in the {service} store when the batch ends; add it in this batch or before it ({lookup})"
            ),
        }
    };
    let target = build_editor_instance_paths_for_indices(document, service, &[index])
        .remove(&index)
        .context("Reference target has no path")?;
    set_reference_target(reference, &document.instances[index].settings_id, &target);
    Ok(false)
}

/// The instances `segments` names from the service root down, in store
/// order; `ordinals` picks one among same-named siblings at each level.
fn instances_at_path(
    document: &SettingsBytecode,
    segments: &[String],
    ordinals: Option<&[usize]>,
) -> Vec<usize> {
    let Some(root) = editor_service_root_index(document, &segments[0])
        .filter(|root| document.instances[*root].name == segments[0])
    else {
        return Vec::new();
    };
    let mut level = vec![root];
    for (depth, segment) in segments.iter().enumerate().skip(1) {
        let ordinal = ordinals.and_then(|ordinals| ordinals.get(depth)).copied();
        let mut next = Vec::new();
        for parent in level {
            let children = document
                .instances
                .iter()
                .enumerate()
                .filter_map(|(index, instance)| {
                    (instance.parent_index == Some(parent) && instance.name == *segment)
                        .then_some(index)
                });
            match ordinal {
                Some(ordinal) => next.extend(children.skip(ordinal - 1).take(1)),
                None => next.extend(children),
            }
        }
        level = next;
    }
    level
}

fn set_reference_target(
    reference: &mut Map<String, Value>,
    settings_id: &str,
    path: &EditorInstancePath,
) {
    for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
        reference.remove(selector);
    }
    reference.insert("settingsId".to_string(), json!(settings_id));
    reference.insert("pathSegments".to_string(), json!(path.path_segments));
    reference.insert("pathOrdinals".to_string(), json!(path.path_ordinals));
}

/// The project's other stores as a store command reads them to complete the
/// references it writes. Each store is read once, when a reference first
/// needs it, and a value written to many instances is completed once.
#[derive(Default)]
pub(crate) struct ReferencedStores {
    stores: BTreeMap<String, SettingsBytecode>,
    completed: HashMap<String, Value>,
}

impl ReferencedStores {
    /// Completes the references in `value`, which is written to the store at
    /// `settings_file` whose content is `own_document`. Returns whether a
    /// path that names nothing was left for later, as `missing` allows.
    pub(crate) fn qualify(
        &mut self,
        value: &mut Value,
        settings_file: &Path,
        own_document: &SettingsBytecode,
        missing: MissingTarget,
    ) -> Result<bool> {
        let written = WrittenReferences::of(value);
        if written.is_empty() {
            return Ok(false);
        }
        let as_written = value.to_string();
        if let Some(completed) = self.completed.get(&as_written) {
            *value = completed.clone();
            return Ok(false);
        }
        let own_service = crate::project::storage::store_service_name(settings_file)
            .context("Service settings path has no service name")?;
        self.read(settings_file, &own_service, &written)?;
        let mut documents = self
            .stores
            .iter()
            .map(|(service, document)| (service.as_str(), document))
            .collect::<BTreeMap<_, _>>();
        documents.insert(&own_service, own_document);
        let deferred = qualify_reference_targets(value, &documents, &written, missing)?;
        if !deferred {
            self.completed.insert(as_written, value.clone());
        }
        Ok(deferred)
    }

    fn read(
        &mut self,
        settings_file: &Path,
        own_service: &str,
        written: &WrittenReferences,
    ) -> Result<()> {
        let source_directory = crate::project::storage::source_directory(settings_file);
        let Some(src_root) = source_directory.parent() else {
            return Ok(());
        };
        let service_dirs = if written.needs_every_store() {
            crate::project::storage::service_directories(src_root)?
        } else {
            written
                .path_services()
                .filter(|service| validate_filesystem_instance_name(service, "service").is_ok())
                .map(|service| src_root.join(service))
                .collect()
        };
        for service_dir in service_dirs {
            let path = service_settings_path(&service_dir);
            let service = crate::project::storage::store_service_name(&path)
                .context("Service settings path has no service name")?;
            if service != own_service && !self.stores.contains_key(&service) && path.is_file() {
                self.stores
                    .insert(service, SettingsBytecode::read_file(&path)?);
            }
        }
        Ok(())
    }
}

/// Where the instances of one edited store were before the edit.
struct EarlierTargets<'a> {
    by_settings_id: HashMap<&'a str, usize>,
    by_exact_path: HashMap<String, usize>,
    by_segments: HashMap<String, Option<usize>>,
    paths: Vec<Option<EditorInstancePath>>,
}

/// One store an edit renamed, moved or removed instances in.
struct EditedStore<'a> {
    service: &'a str,
    before: &'a SettingsBytecode,
    /// Where every instance of the store is now, by settings id.
    current: HashMap<String, EditorInstancePath>,
    /// The ids of the instances the edit added and removed again.
    short_lived: Option<&'a HashSet<String>>,
    earlier: OnceCell<EarlierTargets<'a>>,
}

/// What an edit did to the instance a reference names.
enum Followed<'a> {
    /// The instance is where the reference says, or the reference names
    /// nothing the edit touched.
    Unchanged,
    Moved(&'a str, &'a EditorInstancePath),
    Removed,
}

fn same_path(path: &EditorInstancePath, segments: &[String], ordinals: Option<&[usize]>) -> bool {
    path.path_segments == segments && ordinals == Some(path.path_ordinals.as_slice())
}

impl<'a> EditedStore<'a> {
    fn new(
        service: &'a str,
        before: &'a SettingsBytecode,
        after: Option<&SettingsBytecode>,
        short_lived: Option<&'a HashSet<String>>,
    ) -> Self {
        let current = after
            .map(|after| {
                after
                    .instances
                    .iter()
                    .zip(build_editor_instance_paths(after, service))
                    .filter_map(|(instance, path)| Some((instance.settings_id.clone(), path?)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            service,
            before,
            current,
            short_lived,
            earlier: OnceCell::new(),
        }
    }

    fn earlier(&self) -> &EarlierTargets<'a> {
        self.earlier.get_or_init(|| {
            let paths = build_editor_instance_paths(self.before, self.service);
            let mut by_settings_id = HashMap::with_capacity(paths.len());
            let mut by_exact_path = HashMap::with_capacity(paths.len());
            let mut by_segments = HashMap::<String, Option<usize>>::with_capacity(paths.len());
            for (index, path) in paths.iter().enumerate() {
                let Some(path) = path else {
                    continue;
                };
                by_settings_id.insert(self.before.instances[index].settings_id.as_str(), index);
                by_exact_path.insert(
                    instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                    index,
                );
                by_segments
                    .entry(instance_path_key(&path.path_segments))
                    .and_modify(|unique| *unique = None)
                    .or_insert(Some(index));
            }
            EarlierTargets {
                by_settings_id,
                by_exact_path,
                by_segments,
                paths,
            }
        })
    }

    /// What the edit did to the instance a reference names. The instance is
    /// the one with the reference's settings id when the store knows that
    /// id, now or before the edit, and otherwise the one that was at the
    /// reference's path before the edit. A reference whose target the edit
    /// left in place stays as it is, also when it stores another path. An
    /// instance the edit added and removed again is in neither document; a
    /// reference written to it in between is one to a removed instance.
    fn follow(
        &self,
        settings_id: Option<&str>,
        segments: &[String],
        ordinals: Option<&[usize]>,
    ) -> Followed<'_> {
        let known_now = settings_id.and_then(|id| self.current.get_key_value(id));
        if known_now.is_none()
            && let Some(id) = settings_id
            && self.short_lived.is_some_and(|ids| ids.contains(id))
        {
            return Followed::Removed;
        }
        if known_now.is_some_and(|(_, now)| same_path(now, segments, ordinals)) {
            return Followed::Unchanged;
        }
        let earlier = self.earlier();
        let earlier_index = settings_id
            .and_then(|id| earlier.by_settings_id.get(id).copied())
            .or_else(|| {
                known_now
                    .is_none()
                    .then(|| earlier.index_at(segments, ordinals))
                    .flatten()
            });
        let (target_id, now) = match earlier_index {
            Some(index) => {
                let target_id = self.before.instances[index].settings_id.as_str();
                match self.current.get_key_value(target_id) {
                    Some(now) => now,
                    None => return Followed::Removed,
                }
            }
            // An instance the edit created was at the stored path when the
            // reference was written; the edit may have moved it since.
            None => match known_now {
                Some(now) => now,
                None => return Followed::Unchanged,
            },
        };
        let stayed = earlier_index
            .and_then(|index| earlier.paths[index].as_ref())
            .is_some_and(|was| same_path(now, &was.path_segments, Some(&was.path_ordinals)));
        if stayed {
            Followed::Unchanged
        } else {
            Followed::Moved(target_id, now)
        }
    }
}

impl EarlierTargets<'_> {
    /// The instance that was at a path: at its exact place when the path
    /// has ordinals, and otherwise the only instance with these names.
    fn index_at(&self, segments: &[String], ordinals: Option<&[usize]>) -> Option<usize> {
        match ordinals {
            Some(ordinals) => self
                .by_exact_path
                .get(&instance_path_parts_key(segments, ordinals))
                .copied(),
            None => self
                .by_segments
                .get(&instance_path_key(segments))
                .copied()
                .flatten(),
        }
    }
}

fn names_edited_store(value: &Value, edited: &BTreeMap<String, &SettingsBytecode>) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| names_edited_store(value, edited)),
        Value::Object(object) => {
            object
                .get("pathSegments")
                .and_then(Value::as_array)
                .and_then(|segments| segments.first())
                .and_then(Value::as_str)
                .is_some_and(|service| edited.contains_key(service))
                || object
                    .values()
                    .any(|value| names_edited_store(value, edited))
        }
        _ => false,
    }
}

fn follow_reference(
    reference: &mut Map<String, Value>,
    stores: &HashMap<&str, EditedStore<'_>>,
) -> bool {
    let Ok(Some((segments, ordinals))) = strict_reference_path(reference) else {
        return false;
    };
    let Some(store) = segments
        .first()
        .and_then(|service| stores.get(service.as_str()))
    else {
        return false;
    };
    let settings_id = reference
        .get("settingsId")
        .or_else(|| reference.get("instanceId"))
        .and_then(Value::as_str);
    match store.follow(settings_id, &segments, ordinals.as_deref()) {
        Followed::Unchanged => false,
        Followed::Moved(settings_id, path) => {
            set_reference_target(reference, settings_id, path);
            true
        }
        Followed::Removed => {
            for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
                reference.remove(selector);
            }
            true
        }
    }
}

/// Brings the references into edited stores up to date. `edited` holds, by
/// service, each store an edit renamed, moved or removed instances in, as it
/// was before; `documents` holds the stores as they are now. A reference
/// that carries the path of an instance the edit renamed or moved (also by
/// renaming or moving an ancestor, or by shifting its ordinal) takes the new
/// path, and one to a removed instance is cleared, in whichever store it
/// sits. Returns the services whose document changed.
pub(crate) fn follow_edited_references(
    edited: &BTreeMap<String, &SettingsBytecode>,
    documents: &mut BTreeMap<String, SettingsBytecode>,
) -> BTreeSet<String> {
    follow_edited_references_with_short_lived(edited, &BTreeMap::new(), documents)
}

/// `follow_edited_references` for an edit of several steps. `short_lived`
/// holds, by service, the ids of the instances the edit added and removed
/// again: they are in no store before or after, and a reference a step wrote
/// to one of them is cleared like one to any removed instance.
pub(crate) fn follow_edited_references_with_short_lived(
    edited: &BTreeMap<String, &SettingsBytecode>,
    short_lived: &BTreeMap<String, &HashSet<String>>,
    documents: &mut BTreeMap<String, SettingsBytecode>,
) -> BTreeSet<String> {
    let mut holders = Vec::new();
    for (service, document) in documents.iter() {
        for (index, instance) in document.instances.iter().enumerate() {
            if instance
                .properties
                .values()
                .chain(instance.attributes.values())
                .any(|value| names_edited_store(value, edited))
            {
                holders.push((service.clone(), index));
            }
        }
    }
    let mut changed = BTreeSet::new();
    if holders.is_empty() {
        return changed;
    }
    let stores = edited
        .iter()
        .map(|(service, before)| {
            (
                service.as_str(),
                EditedStore::new(
                    service,
                    before,
                    documents.get(service),
                    short_lived.get(service).copied(),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    for (service, index) in holders {
        let Some(instance) = documents
            .get_mut(&service)
            .and_then(|document| document.instances.get_mut(index))
        else {
            continue;
        };
        let mut followed = false;
        for record in [&mut instance.properties, &mut instance.attributes] {
            visit_reference_objects_mut(record, |reference| {
                followed |= follow_reference(reference, &stores);
            });
        }
        if followed {
            changed.insert(service);
        }
    }
    changed
}
