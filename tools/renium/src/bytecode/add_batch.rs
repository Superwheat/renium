use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::edit::{
    NewInstanceValues, bytecode_service_name, clone_store_subtree,
    collect_settings_subtree_preorder, luau_string_list, new_script_source,
    next_editor_settings_id_fast, prepare_new_instance_values, prune_removed_source_dirs,
    reject_package_link_subtree_mutation,
};
use crate::bytecode::explorer::compact_path_string;
use crate::bytecode::query::resolve_document_id_or_target;
use crate::bytecode::references::{
    MissingTarget, WrittenReferences, follow_edited_references_with_short_lived,
    qualify_reference_targets,
};
use crate::bytecode::{
    SettingsFileLock, acquire_settings_file_locks, apply_file_mutations, check_new_source_file,
    collect_source_path_updates, ensure_settable_attribute_name, file_mutation_paths,
    fold_derived_property, high_level_split_path, missing_service_store_error,
    preserve_source_path_extension, reject_owned_instance_edit, reject_read_only_link_edit,
    reject_read_only_link_subtree_edit, resolve_bytecode_cli_settings_file,
    settings_lock_order_key, store_link_enforcement, stored_property_edit,
};
use crate::editor::document::is_protected_engine_container;
use crate::editor::paths::{
    build_editor_instance_paths, build_editor_instance_paths_for_indices,
    build_editor_source_paths_by_index, script_file_names,
};
use crate::editor::sync::is_lua_source_class;
use crate::editor::types::EditorInstancePath;
use crate::project::package_links::LinkEnforcement;
use crate::rbx::model::canonicalize_settings_references_for_moves;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode,
};
use crate::settings::instance::{self as instance_api, AddInstanceSpec, PropertyScope};
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::system::files::{path_key, service_settings_path, validate_filesystem_instance_name};

/// The longest first name of a dotted string that is looked up as a service.
const MAX_SERVICE_NAME_LEN: usize = 100;

/// An add op of a `ba --batch` request; an op without `op` adds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchAddItem {
    parent: String,
    #[serde(default)]
    service: Option<String>,
    name: String,
    #[serde(alias = "className", alias = "class_name", alias = "c")]
    class: String,
    #[serde(default, alias = "settingsId", alias = "settings_id")]
    id: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default, rename = "sourceFile", alias = "source_file")]
    source_file: Option<PathBuf>,
    #[serde(default, alias = "properties")]
    props: Map<String, Value>,
    #[serde(default, alias = "attributes")]
    attrs: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchSetOp {
    path: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default, alias = "properties")]
    props: Map<String, Value>,
    #[serde(default, alias = "attributes")]
    attrs: Map<String, Value>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default, rename = "sourceFile", alias = "source_file")]
    source_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchCloneOp {
    path: String,
    #[serde(default)]
    service: Option<String>,
    name: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default, alias = "properties")]
    props: Map<String, Value>,
    #[serde(default, alias = "attributes")]
    attrs: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchRenameOp {
    path: String,
    #[serde(default)]
    service: Option<String>,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchMoveOp {
    path: String,
    #[serde(default)]
    service: Option<String>,
    parent: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchRemoveOp {
    path: String,
    #[serde(default)]
    service: Option<String>,
}

/// One op of a `ba --batch` request, chosen by its `op` field.
pub(crate) enum BatchOp {
    Add(BatchAddItem),
    Set(BatchSetOp),
    Clone(BatchCloneOp),
    Rename(BatchRenameOp),
    Move(BatchMoveOp),
    Remove(BatchRemoveOp),
}

impl BatchOp {
    fn parse(item: Value) -> Result<Self> {
        let Value::Object(mut fields) = item else {
            bail!("an op must be a JSON object");
        };
        let kind = match fields.remove("op") {
            None => "add".to_string(),
            Some(Value::String(kind)) => kind,
            Some(other) => bail!("op must be a string, got {other}"),
        };
        let fields = Value::Object(fields);
        Ok(match kind.as_str() {
            "add" => Self::Add(serde_json::from_value(fields)?),
            "set" => Self::Set(serde_json::from_value(fields)?),
            "clone" => Self::Clone(serde_json::from_value(fields)?),
            "rename" => Self::Rename(serde_json::from_value(fields)?),
            "move" => Self::Move(serde_json::from_value(fields)?),
            "remove" => Self::Remove(serde_json::from_value(fields)?),
            other => bail!("unknown op {other:?}; use add, set, clone, rename, move or remove"),
        })
    }

    /// The paths and parents the op names, each with the op's `service`.
    fn store_targets(&self) -> Vec<(&str, Option<&str>)> {
        match self {
            Self::Add(item) => vec![(item.parent.as_str(), item.service.as_deref())],
            Self::Set(op) => vec![(op.path.as_str(), op.service.as_deref())],
            Self::Clone(op) => std::iter::once(op.path.as_str())
                .chain(op.parent.as_deref())
                .map(|raw| (raw, op.service.as_deref()))
                .collect(),
            Self::Rename(op) => vec![(op.path.as_str(), op.service.as_deref())],
            Self::Move(op) => vec![
                (op.path.as_str(), op.service.as_deref()),
                (op.parent.as_str(), op.service.as_deref()),
            ],
            Self::Remove(op) => vec![(op.path.as_str(), op.service.as_deref())],
        }
    }

    /// The property and attribute values the op writes.
    fn written_values(&self) -> Box<dyn Iterator<Item = &Value> + '_> {
        match self {
            Self::Add(item) => Box::new(item.props.values().chain(item.attrs.values())),
            Self::Set(op) => Box::new(op.props.values().chain(op.attrs.values())),
            Self::Clone(op) => Box::new(op.props.values().chain(op.attrs.values())),
            Self::Rename(_) | Self::Move(_) | Self::Remove(_) => Box::new(std::iter::empty()),
        }
    }

    /// Whether the op changes paths, so that references in any store of the
    /// project may have to follow.
    fn restructures(&self) -> bool {
        matches!(self, Self::Rename(_) | Self::Move(_) | Self::Remove(_))
    }

    fn label(&self) -> String {
        let (kind, reference) = match self {
            Self::Add(item) => ("add", item.reference.as_deref()),
            Self::Set(_) => ("set", None),
            Self::Clone(op) => ("clone", op.reference.as_deref()),
            Self::Rename(_) => ("rename", None),
            Self::Move(_) => ("move", None),
            Self::Remove(_) => ("remove", None),
        };
        match reference {
            Some(reference) => format!("{kind} #{reference}"),
            None => kind.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchRequest {
    ops: Vec<Value>,
}

/// Resolves a service name to its store file and service hint.
pub(crate) type StoreResolver<'a> = dyn Fn(&str) -> Result<(PathBuf, String)> + 'a;

type FileMutations = (BTreeMap<PathBuf, Vec<u8>>, Vec<PathBuf>);

pub(crate) struct BatchOutcome {
    pub(crate) result: Value,
    pub(crate) pushes: Vec<StorePush>,
    pub(crate) studio: StudioPrep,
}

pub(crate) struct StorePush {
    pub(crate) service: String,
    pub(crate) settings_file: PathBuf,
    pub(crate) settings_ids: Vec<String>,
}

/// A scoped push pairs instances with Studio by path, so the instances that
/// existed before the batch are first found at their old paths and given
/// their new names and parents, and the removed ones are destroyed. Slots
/// index `paths`.
#[derive(Default)]
pub(crate) struct StudioPrep {
    pub(crate) paths: Vec<Vec<String>>,
    pub(crate) renames: Vec<(usize, String)>,
    pub(crate) moves: Vec<(usize, usize)>,
    pub(crate) destroys: Vec<usize>,
}

pub(crate) fn bytecode_add_batch(
    input: &Path,
    pretty: bool,
    push: bool,
    override_packages: bool,
) -> Result<()> {
    let ops = parse_batch_ops(&read_batch_input(input)?)?;
    let BatchOutcome {
        mut result,
        pushes,
        studio,
    } = apply_store_batch(
        ops,
        &|service| resolve_bytecode_cli_settings_file(None, Some(service), None),
        push,
        override_packages,
    )?;
    if push {
        let mut verified = true;
        if !studio.is_empty() {
            let applied = studio.apply_in_studio()?;
            verified &= applied.get("destroyed").and_then(Value::as_u64)
                == Some(studio.destroys.len() as u64);
            result["studio"] = applied;
        }
        let mut summaries = Map::new();
        for store in pushes {
            let summary = crate::bytecode::edit::push_stored_instances(
                store.settings_file,
                store.settings_ids,
            )?;
            verified &= ["verified", "unchanged"]
                .iter()
                .any(|key| summary.get(*key) == Some(&Value::Bool(true)));
            summaries.insert(store.service, summary);
        }
        if !summaries.is_empty() {
            result["push"] = Value::Object(summaries);
        }
        result["verified"] = json!(verified);
    }
    print_json_output(&result, pretty)
}

pub(crate) fn read_batch_input(input: &Path) -> Result<String> {
    if input == Path::new("-") {
        let mut raw = String::new();
        io::stdin().read_to_string(&mut raw)?;
        return Ok(raw);
    }
    if let Some(inline) = input
        .to_str()
        .map(str::trim_start)
        .filter(|text| text.starts_with(['{', '[']))
    {
        return Ok(inline.to_string());
    }
    fs::read_to_string(input).with_context(|| format!("Failed to read {}", input.display()))
}

pub(crate) fn parse_batch_ops(raw: &str) -> Result<Vec<BatchOp>> {
    let raw = raw.trim_start_matches('\u{feff}');
    let items = match raw.trim_start().chars().next() {
        Some('{') => serde_json::from_str::<BatchRequest>(raw).map(|request| request.ops),
        Some('[') => serde_json::from_str::<Vec<Value>>(raw),
        _ => bail!("Batch input must be {{\"ops\":[...]}} or a JSON array of ops"),
    }
    .context("Invalid batch input")?;
    if items.is_empty() {
        bail!("Batch input has no ops");
    }
    items
        .into_iter()
        .enumerate()
        .map(|(position, item)| {
            BatchOp::parse(item).with_context(|| format!("Batch op {position}"))
        })
        .collect()
}

/// Applies every op in memory first, in order, then writes each touched
/// store and its script files in one step. A failing op leaves every store
/// untouched; `push` also refuses what a push could not take in one pass,
/// and `override_packages` allows edits inside read-only links.
pub(crate) fn apply_store_batch(
    ops: Vec<BatchOp>,
    resolve_store: &StoreResolver<'_>,
    push: bool,
    override_packages: bool,
) -> Result<BatchOutcome> {
    let mut batch = BatchState {
        resolve_store,
        override_packages,
        locks: BatchLocks::acquire(&ops, resolve_store)?,
        stores: Vec::new(),
        positions: HashMap::new(),
        refs: HashMap::new(),
        receipts: Vec::new(),
        labels: Vec::new(),
        last_moves: HashMap::new(),
        running: 0,
        pending: Vec::new(),
    };
    for (position, op) in ops.into_iter().enumerate() {
        let label = op.label();
        batch.running = position;
        batch
            .apply(position, op)
            .with_context(|| format!("Batch op {position} ({label})"))?;
        batch.labels.push(label);
    }
    batch.finish(push)
}

/// The service whose store an op's path or parent is in: the op's
/// `service`, or the first name of a dotted path. A `#ref` names the store
/// of an earlier op, and a settings id alone names none.
fn named_service(raw: &str, service: Option<&str>) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with('#') {
        return None;
    }
    match service.map(str::trim).filter(|value| !value.is_empty()) {
        Some(service) => Some(service.to_string()),
        None if raw.contains(':') => None,
        None => high_level_split_path(raw).into_iter().next(),
    }
}

/// Adds the services the strings in a written value may lead into: a
/// reference property takes a dotted path, with or without a leading `game`.
fn collect_dotted_path_services(value: &Value, services: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            let mut names = text
                .split('.')
                .filter(|name| !name.is_empty())
                .skip_while(|name| *name == "game");
            if let (Some(service), Some(_)) = (names.next(), names.next())
                && service.len() <= MAX_SERVICE_NAME_LEN
            {
                services.insert(service.to_string());
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_dotted_path_services(value, services)),
        Value::Object(object) => object
            .values()
            .for_each(|value| collect_dotted_path_services(value, services)),
        _ => {}
    }
}

/// The store locks a batch holds from before its first op until its files
/// are written.
struct BatchLocks {
    keys: HashSet<String>,
    _held: Vec<SettingsFileLock>,
}

impl BatchLocks {
    /// Locks every store the batch can read or write, in the order all
    /// commands take store locks in. Taking them one by one as the ops reach
    /// them would hold one store while waiting for another that a rename or
    /// a removal locked first. An op names its stores, a written value may
    /// lead into any service that has a store, and an op that changes paths
    /// or a reference by settings id alone reaches every store of the
    /// project.
    fn acquire(ops: &[BatchOp], resolve_store: &StoreResolver<'_>) -> Result<Self> {
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for (raw, service) in ops.iter().flat_map(BatchOp::store_targets) {
            if let Some(service) = named_service(raw, service)
                && let Ok((file, _)) = resolve_store(&service)
                && seen.insert(settings_lock_order_key(&file))
            {
                files.push(file);
            }
        }
        let mut source_roots = BTreeSet::new();
        for file in &files {
            // A pull writes the store under its own lock, so it comes first.
            crate::bytecode::pull_missing_service_store(file);
            if let Some(root) = crate::project::storage::source_directory(file).parent() {
                source_roots.insert(root.to_path_buf());
            }
        }
        let mut services = BTreeSet::new();
        let mut every_store = ops.iter().any(BatchOp::restructures);
        for value in ops.iter().flat_map(BatchOp::written_values) {
            let written = WrittenReferences::of(value);
            every_store |= written.needs_every_store();
            services.extend(written.path_services().map(str::to_string));
            collect_dotted_path_services(value, &mut services);
        }
        for source_root in &source_roots {
            let service_dirs = if every_store {
                crate::project::storage::service_directories(source_root)?
            } else {
                services
                    .iter()
                    .filter(|service| validate_filesystem_instance_name(service, "service").is_ok())
                    .map(|service| source_root.join(service))
                    .collect()
            };
            files.extend(
                service_dirs
                    .iter()
                    .map(|service_dir| service_settings_path(service_dir))
                    .filter(|path| path.is_file()),
            );
        }
        Ok(Self {
            keys: files
                .iter()
                .map(|file| settings_lock_order_key(file))
                .collect(),
            _held: acquire_settings_file_locks(files.iter().map(PathBuf::as_path))?,
        })
    }
}

struct StoreState {
    settings_file: PathBuf,
    service: String,
    service_dir: PathBuf,
    override_packages: bool,
    links: std::cell::OnceCell<LinkEnforcement>,
    before: SettingsBytecode,
    before_ids: HashSet<String>,
    reserved_ids: HashSet<String>,
    next_id_seed: usize,
    document: SettingsBytecode,
    source_paths_before: Vec<Option<PathBuf>>,
    sources: HashMap<String, Vec<u8>>,
    copied_from: HashMap<String, Option<PathBuf>>,
    created_by: HashMap<String, usize>,
    push_ids: Vec<String>,
    /// The renamed and moved instances with their descendants.
    moved: HashSet<String>,
    /// An op renamed, moved or removed an instance of the store.
    restructured: bool,
    /// The ids of the instances the batch added and removed again.
    short_lived: HashSet<String>,
    touched: bool,
}

struct Receipt {
    op: &'static str,
    store: usize,
    settings_id: Option<String>,
    path: Option<Value>,
    fields: Map<String, Value>,
    script_file: bool,
}

struct RefTarget {
    store: usize,
    settings_id: String,
    index: usize,
}

/// A reference property an op wrote by a path that named nothing when the
/// op ran. It is looked up again when the last op has run.
struct PendingReference {
    store: usize,
    settings_id: String,
    property: String,
    op: usize,
}

struct BatchState<'a> {
    resolve_store: &'a StoreResolver<'a>,
    override_packages: bool,
    locks: BatchLocks,
    stores: Vec<StoreState>,
    positions: HashMap<String, usize>,
    refs: HashMap<String, RefTarget>,
    receipts: Vec<Receipt>,
    labels: Vec<String>,
    last_moves: HashMap<(usize, String), usize>,
    /// The position of the op that is being applied.
    running: usize,
    pending: Vec<PendingReference>,
}

impl BatchState<'_> {
    fn apply(&mut self, position: usize, op: BatchOp) -> Result<()> {
        match op {
            BatchOp::Add(item) => self.add(position, item),
            BatchOp::Set(op) => self.set(op),
            BatchOp::Clone(op) => self.clone_instance(position, op),
            BatchOp::Rename(op) => self.rename(op),
            BatchOp::Move(op) => self.move_instance(position, op),
            BatchOp::Remove(op) => self.remove(op),
        }
    }

    fn add(&mut self, position: usize, item: BatchAddItem) -> Result<()> {
        self.check_new_ref(item.reference.as_deref())?;
        let (store, parent_index) =
            self.resolve(&item.parent, item.service.as_deref(), "parent", true)?;
        let mut properties = item.props;
        if let Some(source) =
            given_source(item.source, item.source_file.as_deref(), &mut properties)?
        {
            if !is_lua_source_class(&item.class) {
                bail!("source is only accepted for Script, LocalScript and ModuleScript");
            }
            properties.insert("Source".to_string(), Value::String(source));
        }
        let script = is_lua_source_class(&item.class);
        self.stores[store].reject_read_only_link_edit(parent_index)?;
        let NewInstanceValues {
            mut properties,
            mut attributes,
            given_source,
        } = prepare_new_instance_values(&item.class, properties, item.attrs)?;
        let mut deferred = Vec::new();
        for (name, value) in properties.iter_mut() {
            if self.qualify(store, value, MissingTarget::Defer)? {
                deferred.push(name.clone());
            }
        }
        for value in attributes.values_mut() {
            self.qualify(store, value, MissingTarget::Refuse)?;
        }
        let state = &mut self.stores[store];
        let settings_id = match item.id.filter(|id| !id.is_empty()) {
            Some(id) if state.reserved_ids.contains(&id) && state.index_of(&id).is_none() => {
                bail!("settingsId {id} belonged to an instance this batch removed")
            }
            Some(id) => id,
            None => {
                state.next_id_seed = state.next_id_seed.max(state.document.instances.len());
                next_editor_settings_id_fast(&mut state.reserved_ids, &mut state.next_id_seed)
            }
        };
        let added = instance_api::add_instance(
            &mut state.document,
            AddInstanceSpec {
                settings_id: Some(settings_id),
                name: item.name,
                class_name: item.class,
                parent_index: Some(parent_index),
                properties,
                attributes,
            },
        )?;
        state.reserved_ids.insert(added.settings_id.clone());
        if let Some(source) = given_source {
            state
                .sources
                .insert(added.settings_id.clone(), source.into_bytes());
        }
        if script {
            state.created_by.insert(added.settings_id.clone(), position);
        }
        state.touch(added.index);
        for property in deferred {
            self.defer_reference(store, added.index, property);
        }
        let fields = Map::from_iter([("id".to_string(), json!(added.settings_id))]);
        self.record("add", store, added.index, item.reference, fields, script);
        Ok(())
    }

    fn set(&mut self, op: BatchSetOp) -> Result<()> {
        let (store, index) = self.resolve(&op.path, op.service.as_deref(), "path", false)?;
        let mut props = op.props;
        let source = given_source(op.source, op.source_file.as_deref(), &mut props)?;
        if let Some(tags) = op.tags {
            if props.keys().any(|name| name.eq_ignore_ascii_case("tags")) {
                bail!("give tags or props.Tags, not both");
            }
            props.insert("Tags".to_string(), json!(tags));
        }
        if props.is_empty() && op.attrs.is_empty() && source.is_none() {
            bail!("set needs props, attrs, tags, source or sourceFile");
        }
        let fields = self.set_values(store, index, props, op.attrs)?;
        let state = &mut self.stores[store];
        let script = source.is_some();
        if let Some(source) = source {
            state.set_source(index, source)?;
        }
        state.touch(index);
        self.record("set", store, index, None, fields, script);
        Ok(())
    }

    fn clone_instance(&mut self, position: usize, op: BatchCloneOp) -> Result<()> {
        self.check_new_ref(op.reference.as_deref())?;
        if op.name.is_empty() {
            bail!("Instance name cannot be empty");
        }
        let (store, source_index) = self.resolve(&op.path, op.service.as_deref(), "path", false)?;
        let Some(source_parent) = self.stores[store].document.instances[source_index].parent_index
        else {
            bail!("Service roots cannot be copied");
        };
        let parent_index = match op.parent.as_deref() {
            Some(parent) => {
                let (parent_store, parent_index) =
                    self.resolve(parent, op.service.as_deref(), "parent", false)?;
                if parent_store != store {
                    bail!(
                        "parent {parent} is in {}, not {}; a copy stays in its store",
                        self.stores[parent_store].service,
                        self.stores[store].service
                    );
                }
                parent_index
            }
            None => source_parent,
        };
        let mut props = op.props;
        let source = given_source(None, None, &mut props)?;
        let state = &mut self.stores[store];
        state.reject_read_only_link_edit(parent_index)?;
        let from = state.path_of(source_index);
        let cloned = clone_store_subtree(
            &mut state.document,
            &state.service,
            source_index,
            parent_index,
            Some(&op.name),
            &state.reserved_ids,
        )?;
        for &(old_index, new_index) in &cloned.copies {
            let new_id = state.id(new_index);
            state.reserved_ids.insert(new_id.clone());
            state.touch(new_index);
            if script_file_names(&state.document.instances[old_index].class_name).is_none() {
                continue;
            }
            let old_id = state.id(old_index);
            if let Some(source) = state.current_source(&old_id)? {
                state.sources.insert(new_id.clone(), source);
            }
            let origin = match state.copied_from.get(&old_id) {
                Some(origin) => origin.clone(),
                None => state.before_file(&old_id).map(Path::to_path_buf),
            };
            state.copied_from.insert(new_id.clone(), origin);
            state.created_by.insert(new_id, position);
        }
        let root_index = cloned.root_index;
        let mut fields = if props.is_empty() && op.attrs.is_empty() {
            Map::new()
        } else {
            self.set_values(store, root_index, props, op.attrs)?
        };
        let state = &mut self.stores[store];
        if let Some(source) = source {
            state.set_source(root_index, source)?;
        }
        let script = is_lua_source_class(&state.document.instances[root_index].class_name);
        fields.insert("from".to_string(), from);
        fields.insert("id".to_string(), json!(state.id(root_index)));
        self.record("clone", store, root_index, op.reference, fields, script);
        Ok(())
    }

    fn rename(&mut self, op: BatchRenameOp) -> Result<()> {
        if op.name.is_empty() {
            bail!("Instance name cannot be empty");
        }
        let (store, index) = self.resolve(&op.path, op.service.as_deref(), "path", false)?;
        let state = &mut self.stores[store];
        let from = state.path_of(index);
        state.change_metadata(index, "Name", Value::String(op.name))?;
        let fields = Map::from_iter([("from".to_string(), from)]);
        self.record("rename", store, index, None, fields, false);
        Ok(())
    }

    fn move_instance(&mut self, position: usize, op: BatchMoveOp) -> Result<()> {
        let (store, index) = self.resolve(&op.path, op.service.as_deref(), "path", false)?;
        let (parent_store, parent_index) =
            self.resolve(&op.parent, op.service.as_deref(), "parent", false)?;
        if parent_store != store {
            bail!(
                "parent {} is in {}, not {}; moves between services go through `rbx mv`",
                op.parent,
                self.stores[parent_store].service,
                self.stores[store].service
            );
        }
        let state = &mut self.stores[store];
        state.reject_read_only_link_edit(parent_index)?;
        let from = state.path_of(index);
        let parent_id = state.id(parent_index);
        state.change_metadata(index, "Parent", Value::String(parent_id))?;
        self.last_moves.insert((store, state.id(index)), position);
        let fields = Map::from_iter([("from".to_string(), from)]);
        self.record("move", store, index, None, fields, false);
        Ok(())
    }

    fn remove(&mut self, op: BatchRemoveOp) -> Result<()> {
        let (store, index) = self.resolve(&op.path, op.service.as_deref(), "path", false)?;
        let state = &self.stores[store];
        if state.document.instances[index].parent_index.is_none() {
            bail!("Refusing to remove root instance");
        }
        reject_read_only_link_subtree_edit(state.links()?, &state.document, &state.service, index)?;
        let children = settings_children_by_parent(&state.document);
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children, index, &mut subtree);
        reject_package_link_subtree_mutation(&state.document, &subtree, "removed")?;
        if is_protected_engine_container(&state.document, index) {
            bail!("{} cannot be removed", state.document.instances[index].name);
        }
        let removed = subtree
            .iter()
            .map(|&index| (state.id(index), index))
            .collect::<HashMap<_, _>>();
        let short_lived = removed
            .keys()
            .filter(|settings_id| !state.before_ids.contains(*settings_id))
            .cloned()
            .collect::<Vec<_>>();
        let mut indices = vec![index];
        indices.extend(self.receipts.iter().filter_map(|receipt| {
            let settings_id = receipt.settings_id.as_ref()?;
            (receipt.store == store && receipt.path.is_none())
                .then(|| removed.get(settings_id).copied())
                .flatten()
        }));
        let paths =
            build_editor_instance_paths_for_indices(&state.document, &state.service, &indices);
        let path_at = |index: usize| {
            paths
                .get(&index)
                .map_or_else(|| json!(state.document.instances[index].name), compact_path)
        };
        for receipt in &mut self.receipts {
            if receipt.store == store
                && receipt.path.is_none()
                && let Some(&index) = receipt
                    .settings_id
                    .as_ref()
                    .and_then(|settings_id| removed.get(settings_id))
            {
                receipt.path = Some(path_at(index));
            }
        }
        let path = path_at(index);
        let state = &mut self.stores[store];
        instance_api::remove_instances_at_indices(&mut state.document, &[index], true)?;
        state.short_lived.extend(short_lived);
        state.restructured = true;
        state.touched = true;
        self.receipts.push(Receipt {
            op: "remove",
            store,
            settings_id: None,
            path: Some(path),
            fields: Map::from_iter([("removed".to_string(), json!(subtree.len()))]),
            script_file: false,
        });
        Ok(())
    }

    fn check_new_ref(&self, reference: Option<&str>) -> Result<()> {
        let Some(reference) = reference else {
            return Ok(());
        };
        if reference.is_empty() || reference.starts_with('#') {
            bail!("ref must be a non-empty name without a leading #");
        }
        if self.refs.contains_key(reference) {
            bail!("ref #{reference} is already used by an earlier op");
        }
        Ok(())
    }

    fn record(
        &mut self,
        op: &'static str,
        store: usize,
        index: usize,
        reference: Option<String>,
        mut fields: Map<String, Value>,
        script_file: bool,
    ) {
        let settings_id = self.stores[store].id(index);
        if let Some(reference) = reference {
            fields.insert("ref".to_string(), json!(reference));
            self.refs.insert(
                reference,
                RefTarget {
                    store,
                    settings_id: settings_id.clone(),
                    index,
                },
            );
        }
        self.receipts.push(Receipt {
            op,
            store,
            settings_id: Some(settings_id),
            path: None,
            fields,
            script_file,
        });
    }

    /// The store and index `raw` names: `#ref` for the instance an earlier op
    /// created, a bare service name for the store root, or a dotted path from
    /// the service (a settings id or unique name with `service`).
    fn resolve(
        &mut self,
        raw: &str,
        service: Option<&str>,
        role: &str,
        create: bool,
    ) -> Result<(usize, usize)> {
        let raw = raw.trim();
        let service = service.map(str::trim).filter(|value| !value.is_empty());
        if let Some(reference) = raw.strip_prefix('#') {
            let target = self
                .refs
                .get_mut(reference)
                .with_context(|| format!("{role} #{reference} does not name an earlier op"))?;
            let state = &self.stores[target.store];
            if let Some(service) = service
                && service != state.service
            {
                bail!(
                    "{role} #{reference} lives in {}, not {service}",
                    state.service
                );
            }
            if state
                .document
                .instances
                .get(target.index)
                .is_none_or(|instance| instance.settings_id != target.settings_id)
            {
                target.index = state
                    .index_of(&target.settings_id)
                    .with_context(|| format!("{role} #{reference} was removed by an earlier op"))?;
            }
            return Ok((target.store, target.index));
        }
        let service = match named_service(raw, service) {
            Some(service) => service,
            None if raw.contains(':') => {
                bail!("{role} {raw} is a settings id; add \"service\" to say which store it is in")
            }
            None => bail!("{role} {raw:?} names no service"),
        };
        let (settings_file, service_hint) = (self.resolve_store)(&service)?;
        let store = self.load(settings_file, &service_hint, create)?;
        let document = &self.stores[store].document;
        // A bare service name is the store root, never a search for an
        // instance that happens to carry the service's name.
        let index = if raw == service {
            editor_service_root_index(document, &service)
                .with_context(|| format!("The {service} store has no root"))?
        } else {
            resolve_document_id_or_target(document, raw)?
        };
        Ok((store, index))
    }

    fn load(&mut self, settings_file: PathBuf, service_hint: &str, create: bool) -> Result<usize> {
        let key = settings_lock_order_key(&settings_file);
        if let Some(position) = self.positions.get(&key) {
            return Ok(*position);
        }
        if !create && !settings_file.exists() {
            return Err(missing_service_store_error(&settings_file));
        }
        if !self.locks.keys.contains(&key) {
            bail!(
                "{} was not locked before the batch ran",
                settings_file.display()
            );
        }
        let document = if settings_file.exists() {
            SettingsBytecode::read_file(&settings_file)?
        } else {
            new_service_document(service_hint)
        };
        let service = bytecode_service_name(&document, &settings_file, service_hint);
        let service_dir = crate::project::storage::source_directory(&settings_file);
        let source_paths_before =
            build_editor_source_paths_by_index(&document, &service, &service_dir);
        let before_ids = document
            .instances
            .iter()
            .map(|instance| instance.settings_id.clone())
            .collect::<HashSet<_>>();
        self.stores.push(StoreState {
            settings_file,
            service,
            service_dir,
            override_packages: self.override_packages,
            links: std::cell::OnceCell::new(),
            before: document.clone(),
            reserved_ids: before_ids.clone(),
            before_ids,
            next_id_seed: document.instances.len(),
            document,
            source_paths_before,
            sources: HashMap::new(),
            copied_from: HashMap::new(),
            created_by: HashMap::new(),
            push_ids: Vec::new(),
            moved: HashSet::new(),
            restructured: false,
            short_lived: HashSet::new(),
            touched: false,
        });
        self.positions.insert(key, self.stores.len() - 1);
        Ok(self.stores.len() - 1)
    }

    /// Loads the stores of the project that keeps its script files under
    /// `source_root`: every store, or with `holding` only those that hold
    /// every name of one of these paths.
    fn load_project_stores(
        &mut self,
        source_root: &Path,
        holding: Option<&[Vec<String>]>,
    ) -> Result<()> {
        for service_dir in crate::project::storage::service_directories(source_root)? {
            let path = service_settings_path(&service_dir);
            if !path.is_file() {
                continue;
            }
            if let Some(paths) = holding
                && !self.positions.contains_key(&settings_lock_order_key(&path))
                && (paths.is_empty()
                    || SettingsBytecode::read_file_if_contains_any_string_set(&path, paths)?
                        .is_none())
            {
                continue;
            }
            let hint = crate::project::storage::store_service_name(&path).unwrap_or_default();
            self.load(path, &hint, false)?;
        }
        Ok(())
    }

    /// The paths the removed instances had before the batch, one for each
    /// removed subtree.
    fn removed_paths(&self) -> Vec<Vec<String>> {
        let mut paths = Vec::new();
        for store in self.stores.iter().filter(|store| store.restructured) {
            let kept = store
                .document
                .instances
                .iter()
                .map(|instance| instance.settings_id.as_str())
                .collect::<HashSet<_>>();
            let before = &store.before.instances;
            let gone = |index: usize| !kept.contains(before[index].settings_id.as_str());
            let roots = (0..before.len())
                .filter(|&index| {
                    gone(index)
                        && before[index]
                            .parent_index
                            .is_none_or(|parent| !gone(parent))
                })
                .collect::<Vec<_>>();
            paths.extend(
                build_editor_instance_paths_for_indices(&store.before, &store.service, &roots)
                    .into_values()
                    .map(|path| path.path_segments),
            );
        }
        paths
    }

    /// Completes the references in a value an op writes to `store`, against
    /// the stores as the batch has them so far: a path may name an instance
    /// an earlier op created, and not one an earlier op removed. Returns
    /// whether a path that names nothing yet was left for the end of the
    /// batch, as `missing` allows.
    fn qualify(&mut self, store: usize, value: &mut Value, missing: MissingTarget) -> Result<bool> {
        let written = WrittenReferences::of(value);
        if written.is_empty() {
            return Ok(false);
        }
        if let Some(source_root) = self.stores[store]
            .service_dir
            .parent()
            .map(Path::to_path_buf)
        {
            if written.needs_every_store() {
                self.load_project_stores(&source_root, None)?;
            }
            for service in written.path_services() {
                let path = service_settings_path(&source_root.join(service));
                if validate_filesystem_instance_name(service, "service").is_ok() && path.is_file() {
                    self.load(path, service, false)?;
                }
            }
        }
        let documents = self
            .stores
            .iter()
            .map(|store| (store.service.as_str(), &store.document))
            .collect::<BTreeMap<_, _>>();
        qualify_reference_targets(value, &documents, &written, missing)
    }

    /// Notes that the value the running op gave a property of the instance
    /// holds a path that names nothing yet.
    fn defer_reference(&mut self, store: usize, index: usize, property: String) {
        self.pending.push(PendingReference {
            store,
            settings_id: self.stores[store].id(index),
            property,
            op: self.running,
        });
    }

    /// Completes the references that named nothing when their op ran,
    /// against the stores as the batch leaves them. One that still names
    /// nothing fails the batch under the op that wrote it.
    fn resolve_pending_references(&mut self) -> Result<()> {
        for pending in std::mem::take(&mut self.pending) {
            let Some(index) = self.stores[pending.store].index_of(&pending.settings_id) else {
                continue;
            };
            let properties = &self.stores[pending.store].document.instances[index].properties;
            let Some(mut value) = properties.get(&pending.property).cloned() else {
                continue;
            };
            self.qualify(pending.store, &mut value, MissingTarget::RefuseAfterBatch)
                .with_context(|| {
                    format!("Batch op {} ({})", pending.op, self.labels[pending.op])
                })?;
            self.stores[pending.store].document.instances[index]
                .properties
                .insert(pending.property, value);
        }
        Ok(())
    }

    /// Sets `props` as `rbx bs` does (Position and Orientation fold into the
    /// CFrame, names and values take their stored form) and `attrs` as
    /// attributes; Name and Parent change only through rename and move ops.
    fn set_values(
        &mut self,
        store: usize,
        index: usize,
        props: Map<String, Value>,
        attrs: Map<String, Value>,
    ) -> Result<Map<String, Value>> {
        let state = &self.stores[store];
        reject_owned_instance_edit(&state.document, &state.service, index, state.links()?)?;
        let mut properties = Vec::with_capacity(props.len());
        for (mut name, mut value) in props {
            if name.eq_ignore_ascii_case("name") {
                bail!("Name changes through a rename op");
            }
            if name.eq_ignore_ascii_case("parent") {
                bail!("Parent changes through a move op");
            }
            if name.eq_ignore_ascii_case("classname") {
                bail!("ClassName is read-only");
            }
            let document = &mut self.stores[store].document;
            fold_derived_property(document, index, &mut name, &mut value)?;
            let (property, _) =
                stored_property_edit(document, index, &name, &mut value, PropertyScope::Auto)?;
            let deferred = self.qualify(store, &mut value, MissingTarget::Defer)?;
            instance_api::set_instance_property(
                &mut self.stores[store].document,
                index,
                &property,
                value,
                PropertyScope::Auto,
            )?;
            if deferred {
                self.defer_reference(store, index, property.clone());
            }
            properties.push(property);
        }
        let mut attributes = Vec::with_capacity(attrs.len());
        for (name, mut value) in attrs {
            if !value.is_null() {
                ensure_settable_attribute_name(&name)?;
            }
            self.qualify(store, &mut value, MissingTarget::Refuse)?;
            instance_api::set_instance_property(
                &mut self.stores[store].document,
                index,
                &name,
                value,
                PropertyScope::Attribute,
            )?;
            attributes.push(name);
        }
        let mut fields = Map::new();
        if !properties.is_empty() {
            fields.insert("props".to_string(), json!(properties));
        }
        if !attributes.is_empty() {
            fields.insert("attrs".to_string(), json!(attributes));
        }
        Ok(fields)
    }

    /// References to a renamed, moved or removed instance carry its path,
    /// wherever they are, so the stores of the project are brought up to
    /// date with what the batch did. A rename or a move can change any path
    /// below it, so every store is read; removals alone concern only the
    /// stores that hold a removed path.
    fn update_references(&mut self) -> Result<()> {
        let source_roots = self
            .stores
            .iter()
            .filter(|store| store.restructured)
            .filter_map(|store| store.service_dir.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>();
        let removed_paths = self
            .stores
            .iter()
            .all(|store| store.moved.is_empty())
            .then(|| self.removed_paths());
        for source_root in source_roots {
            self.load_project_stores(&source_root, removed_paths.as_deref())?;
        }
        let mut documents = BTreeMap::new();
        let mut moved = HashMap::new();
        for store in &mut self.stores {
            let indices = store
                .document
                .instances
                .iter()
                .enumerate()
                .filter(|(_, instance)| store.moved.contains(&instance.settings_id))
                .map(|(index, _)| index)
                .collect::<HashSet<_>>();
            if !indices.is_empty() {
                moved.insert(store.service.clone(), indices);
            }
            let document = std::mem::replace(&mut store.document, new_service_document(""));
            documents.insert(store.service.clone(), document);
        }
        let edited = self
            .stores
            .iter()
            .filter(|store| store.restructured)
            .map(|store| (store.service.clone(), &store.before))
            .collect::<BTreeMap<_, _>>();
        let short_lived = self
            .stores
            .iter()
            .filter(|store| !store.short_lived.is_empty())
            .map(|store| (store.service.clone(), &store.short_lived))
            .collect::<BTreeMap<_, _>>();
        let mut changed =
            follow_edited_references_with_short_lived(&edited, &short_lived, &mut documents);
        changed.extend(canonicalize_settings_references_for_moves(
            &mut documents,
            &moved,
        ));
        for store in &mut self.stores {
            if let Some(document) = documents.remove(&store.service) {
                store.document = document;
            }
            store.touched |= changed.contains(&store.service);
        }
        Ok(())
    }

    fn finish(mut self, push: bool) -> Result<BatchOutcome> {
        self.resolve_pending_references()?;
        if self.stores.iter().any(|store| store.restructured) {
            self.update_references()?;
        }
        let studio = if push {
            self.studio_prep()?
        } else {
            StudioPrep::default()
        };
        let mut writes = BTreeMap::new();
        let mut removals = Vec::new();
        let mut changed_paths = Vec::new();
        let mut script_files = vec![HashMap::new(); self.stores.len()];
        for (position, store) in self.stores.iter().enumerate() {
            if !store.touched {
                continue;
            }
            let (store_writes, store_removals) =
                store.plan(&self.labels, &mut script_files[position])?;
            changed_paths.extend(file_mutation_paths(&store_writes, &store_removals));
            writes.extend(store_writes);
            removals.extend(store_removals);
        }
        apply_file_mutations(&writes, &removals)?;
        for store in self.stores.iter().filter(|store| store.touched) {
            prune_removed_source_dirs(&store.service_dir, &removals);
        }
        let receipts = self.receipt_values(&script_files);
        let pushes = self.pushes();
        let touched = self.stores.iter().filter(|store| store.touched).count();
        Ok(BatchOutcome {
            result: json!({
                "ok": true,
                "ops": receipts,
                "stores": touched,
                "changedPaths": changed_paths,
            }),
            pushes,
            studio,
        })
    }

    fn receipt_values(&self, script_files: &[HashMap<String, (PathBuf, bool)>]) -> Vec<Value> {
        let mut final_paths = HashMap::<usize, HashMap<String, Value>>::new();
        for receipt in &self.receipts {
            if receipt.path.is_none() && !final_paths.contains_key(&receipt.store) {
                let store = &self.stores[receipt.store];
                let paths = build_editor_instance_paths(&store.document, &store.service);
                let by_id = store
                    .document
                    .instances
                    .iter()
                    .zip(&paths)
                    .filter_map(|(instance, path)| {
                        Some((instance.settings_id.clone(), compact_path(path.as_ref()?)))
                    })
                    .collect();
                final_paths.insert(receipt.store, by_id);
            }
        }
        self.receipts
            .iter()
            .map(|receipt| {
                let mut object = receipt.fields.clone();
                object.insert("op".to_string(), json!(receipt.op));
                let path = receipt.path.clone().or_else(|| {
                    let settings_id = receipt.settings_id.as_ref()?;
                    final_paths.get(&receipt.store)?.get(settings_id).cloned()
                });
                object.insert("path".to_string(), path.unwrap_or(Value::Null));
                if receipt.script_file
                    && let Some((source_path, adopted)) = receipt
                        .settings_id
                        .as_ref()
                        .and_then(|settings_id| script_files[receipt.store].get(settings_id))
                {
                    object.insert("sourcePath".to_string(), json!(source_path));
                    if *adopted {
                        object.insert("adoptedSource".to_string(), json!(true));
                    }
                }
                Value::Object(object)
            })
            .collect()
    }

    fn pushes(&self) -> Vec<StorePush> {
        let mut pushes = self
            .stores
            .iter()
            .filter_map(|store| {
                let present = store
                    .document
                    .instances
                    .iter()
                    .map(|instance| instance.settings_id.as_str())
                    .collect::<HashSet<_>>();
                let mut seen = HashSet::new();
                let settings_ids = store
                    .push_ids
                    .iter()
                    .filter(|id| present.contains(id.as_str()) && seen.insert(id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                (!settings_ids.is_empty()).then(|| StorePush {
                    service: store.service.clone(),
                    settings_file: store.settings_file.clone(),
                    settings_ids,
                })
            })
            .collect::<Vec<_>>();
        pushes.sort_by(|left, right| left.service.cmp(&right.service));
        pushes
    }

    fn studio_prep(&self) -> Result<StudioPrep> {
        let mut prep = StudioPrep::default();
        for (position, store) in self.stores.iter().enumerate() {
            if !store.touched {
                continue;
            }
            let before = &store.before;
            let now_by_id = store
                .document
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.as_str(), index))
                .collect::<HashMap<_, _>>();
            let before_by_id = before
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.as_str(), index))
                .collect::<HashMap<_, _>>();
            let gone = before
                .instances
                .iter()
                .map(|instance| !now_by_id.contains_key(instance.settings_id.as_str()))
                .collect::<Vec<_>>();
            let mut renames = Vec::new();
            let mut moves = Vec::new();
            let mut destroys = Vec::new();
            for (index, instance) in before.instances.iter().enumerate() {
                let Some(&now) = now_by_id.get(instance.settings_id.as_str()) else {
                    if instance.parent_index.is_some_and(|parent| !gone[parent]) {
                        destroys.push(index);
                    }
                    continue;
                };
                let current = &store.document.instances[now];
                if current.name != instance.name {
                    renames.push((index, current.name.clone()));
                }
                let (Some(old_parent), Some(new_parent)) =
                    (instance.parent_index, current.parent_index)
                else {
                    continue;
                };
                let new_parent_id = store.document.instances[new_parent].settings_id.as_str();
                if before.instances[old_parent].settings_id == new_parent_id {
                    continue;
                }
                let Some(&parent) = before_by_id.get(new_parent_id) else {
                    let label = self
                        .last_moves
                        .get(&(position, instance.settings_id.clone()))
                        .map_or_else(|| "Batch".to_string(), |op| format!("Batch op {op} (move)"));
                    bail!(
                        "{label}: with --push, {} cannot move into {}, which this batch creates (Studio would get a copy); create it in an earlier batch",
                        path_text(before, &store.service, index),
                        path_text(&store.document, &store.service, new_parent)
                    );
                };
                moves.push((index, parent));
            }
            let indices = renames
                .iter()
                .map(|(index, _)| *index)
                .chain(moves.iter().flat_map(|&(index, parent)| [index, parent]))
                .chain(destroys.iter().copied())
                .collect::<Vec<_>>();
            if indices.is_empty() {
                continue;
            }
            let paths = build_editor_instance_paths_for_indices(before, &store.service, &indices);
            let mut slots = HashMap::new();
            for index in indices {
                if let Some(path) = paths.get(&index)
                    && !slots.contains_key(&index)
                {
                    slots.insert(index, prep.paths.len());
                    prep.paths.push(path.path_segments.clone());
                }
            }
            prep.renames.extend(
                renames
                    .into_iter()
                    .filter_map(|(index, name)| Some((*slots.get(&index)?, name))),
            );
            prep.moves.extend(
                moves.into_iter().filter_map(|(index, parent)| {
                    Some((*slots.get(&index)?, *slots.get(&parent)?))
                }),
            );
            prep.destroys.extend(
                destroys
                    .into_iter()
                    .filter_map(|index| slots.get(&index).copied()),
            );
        }
        Ok(prep)
    }
}

impl StoreState {
    fn index_of(&self, settings_id: &str) -> Option<usize> {
        self.document
            .instances
            .iter()
            .position(|instance| instance.settings_id == settings_id)
    }

    fn id(&self, index: usize) -> String {
        self.document.instances[index].settings_id.clone()
    }

    fn path_of(&self, index: usize) -> Value {
        build_editor_instance_paths_for_indices(&self.document, &self.service, &[index])
            .get(&index)
            .map_or_else(|| json!(self.document.instances[index].name), compact_path)
    }

    /// The read-only links of the store's project, read at the first op
    /// that needs them.
    fn links(&self) -> Result<&LinkEnforcement> {
        if let Some(links) = self.links.get() {
            return Ok(links);
        }
        let links = store_link_enforcement(&self.settings_file, self.override_packages)?;
        Ok(self.links.get_or_init(|| links))
    }

    /// Refuses an op that puts an instance under the one at `index` inside
    /// a read-only link.
    fn reject_read_only_link_edit(&self, index: usize) -> Result<()> {
        reject_read_only_link_edit(self.links()?, &self.document, &self.service, index)
    }

    fn touch(&mut self, index: usize) {
        self.push_ids.push(self.id(index));
        self.touched = true;
    }

    fn set_source(&mut self, index: usize, source: String) -> Result<()> {
        let class_name = &self.document.instances[index].class_name;
        if script_file_names(class_name).is_none() {
            bail!("{class_name} is not a Lua source container");
        }
        self.sources.insert(self.id(index), source.into_bytes());
        Ok(())
    }

    /// Renames or reparents the instance as `rbx bs -S metadata` does.
    fn change_metadata(&mut self, index: usize, property: &str, value: Value) -> Result<()> {
        let links = self.links()?;
        reject_owned_instance_edit(&self.document, &self.service, index, links)?;
        reject_read_only_link_subtree_edit(links, &self.document, &self.service, index)?;
        if is_protected_engine_container(&self.document, index) {
            bail!(
                "{} metadata is read-only",
                self.document.instances[index].name
            );
        }
        instance_api::set_instance_property(
            &mut self.document,
            index,
            property,
            value,
            PropertyScope::Metadata,
        )?;
        let children = settings_children_by_parent(&self.document);
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children, index, &mut subtree);
        for moved in subtree {
            self.moved.insert(self.id(moved));
        }
        self.restructured = true;
        self.touch(index);
        Ok(())
    }

    fn before_file(&self, settings_id: &str) -> Option<&Path> {
        if !self.before_ids.contains(settings_id) {
            return None;
        }
        let index = self
            .before
            .instances
            .iter()
            .position(|instance| instance.settings_id == settings_id)?;
        self.source_paths_before.get(index)?.as_deref()
    }

    /// The script text an instance has at this point of the batch: what an
    /// earlier op gave it, or else its file from before the batch.
    fn current_source(&self, settings_id: &str) -> Result<Option<Vec<u8>>> {
        if let Some(source) = self.sources.get(settings_id) {
            return Ok(Some(source.clone()));
        }
        match self.before_file(settings_id) {
            Some(path) if path.is_file() => fs::read(path)
                .map(Some)
                .with_context(|| format!("Failed to read {}", path.display())),
            _ => Ok(None),
        }
    }

    /// The file writes and removals that bring the store and its script files
    /// to the batch's result. Script files of removed instances go, created
    /// scripts adopt a file already at their path unless the batch moves or
    /// removes it, and `script_files` gains every script file written or
    /// adopted for an op that created or sourced it.
    fn plan(
        &self,
        labels: &[String],
        script_files: &mut HashMap<String, (PathBuf, bool)>,
    ) -> Result<FileMutations> {
        let mut writes = BTreeMap::new();
        let mut removals = Vec::new();
        let after = collect_source_path_updates(
            &self.before,
            &self.source_paths_before,
            &self.document,
            &self.service,
            &self.service_dir,
            &mut writes,
            &mut removals,
        )?;
        let remaining = self
            .document
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<HashSet<_>>();
        for (instance, path) in self.before.instances.iter().zip(&self.source_paths_before) {
            if let Some(path) = path
                && !remaining.contains(instance.settings_id.as_str())
                && path.is_file()
            {
                removals.push(path.clone());
            }
        }
        let vacated = removals
            .iter()
            .map(|path| path_key(path))
            .collect::<HashSet<_>>();
        for (instance, path) in self.document.instances.iter().zip(&after) {
            let Some(path) = path else {
                continue;
            };
            if !is_lua_source_class(&instance.class_name) {
                continue;
            }
            let settings_id = instance.settings_id.as_str();
            let given = self.sources.get(settings_id).map(Vec::as_slice);
            if self.before_ids.contains(settings_id) {
                if let Some(given) = given {
                    writes.insert(path.clone(), given.to_vec());
                    script_files.insert(settings_id.to_string(), (path.clone(), false));
                }
                continue;
            }
            let mut path = path.clone();
            let copied_from = self.copied_from.get(settings_id);
            if let Some(Some(origin)) = copied_from {
                preserve_source_path_extension(origin, &mut path);
            }
            let bytes = if vacated.contains(&path_key(&path)) {
                Ok(Some(given.unwrap_or_default().to_vec()))
            } else if let (Some(_), Some(given)) = (copied_from, given) {
                check_new_source_file(&path, given).map(|()| Some(given.to_vec()))
            } else {
                new_script_source(&path, given)
            }
            .with_context(|| match self.created_by.get(settings_id) {
                Some(&op) => format!("Batch op {op} ({})", labels[op]),
                None => format!("Batch script {}", path.display()),
            })?;
            let adopted = bytes.is_none();
            if let Some(bytes) = bytes {
                writes.insert(path.clone(), bytes);
            }
            script_files.insert(settings_id.to_string(), (path, adopted));
        }
        removals.retain(|path| !writes.keys().any(|write| path_key(write) == path_key(path)));
        removals.sort_by_key(|path| path_key(path));
        removals.dedup_by(|left, right| path_key(left) == path_key(right));
        writes.insert(
            self.settings_file.clone(),
            encode_settings_bytecode(&self.document)?,
        );
        Ok((writes, removals))
    }
}

impl StudioPrep {
    pub(crate) fn is_empty(&self) -> bool {
        self.renames.is_empty() && self.moves.is_empty() && self.destroys.is_empty()
    }

    /// One Luau chunk for the Edit window: every instance is found before
    /// anything changes, and moved instances are detached first so a swap of
    /// parent and child never forms a cycle.
    pub(crate) fn luau(&self) -> String {
        let join = |items: Vec<String>| items.join(", ");
        let paths = join(
            self.paths
                .iter()
                .map(|path| luau_string_list(path))
                .collect(),
        );
        let renames = join(
            self.renames
                .iter()
                .map(|(slot, name)| format!("{{ {}, {name:?} }}", slot + 1))
                .collect(),
        );
        let moves = join(
            self.moves
                .iter()
                .map(|(slot, parent)| format!("{{ {}, {} }}", slot + 1, parent + 1))
                .collect(),
        );
        let destroys = join(
            self.destroys
                .iter()
                .map(|slot| (slot + 1).to_string())
                .collect(),
        );
        format!(
            "local function at(segments) local inst = game for _, name in segments do inst = inst:FindFirstChild(name) if not inst then return nil end end return inst end \
             local found, missing = {{}}, {{}} \
             for i, segments in {{{paths}}} do found[i] = at(segments) if not found[i] then table.insert(missing, table.concat(segments, '.')) end end \
             local renamed, moved, destroyed = 0, 0, 0 \
             for _, edit in {{{renames}}} do local inst = found[edit[1]] if inst then inst.Name = edit[2] renamed += 1 end end \
             local moves = {{{moves}}} \
             for _, edit in moves do if found[edit[1]] and found[edit[2]] then found[edit[1]].Parent = nil end end \
             for _, edit in moves do if found[edit[1]] and found[edit[2]] then found[edit[1]].Parent = found[edit[2]] moved += 1 end end \
             for _, slot in {{{destroys}}} do local inst = found[slot] if inst then inst:Destroy() destroyed += 1 end end \
             return {{ renamed = renamed, moved = moved, destroyed = destroyed, missing = missing }}"
        )
    }

    fn apply_in_studio(&self) -> Result<Value> {
        let result = crate::daemon::daemon_control_request(
            crate::automation::op::LUAU,
            None,
            json!({ "code": self.luau(), "edit": true, "bridgeWaitSeconds": 8.0, "bridgePorts": "8781,8782" }),
            false,
        )
        .context("The batch was written but Studio could not apply its renames, moves and removals; push the stores with `rbx ps`")?;
        Ok(result
            .get("results")
            .and_then(Value::as_array)
            .and_then(|results| results.first())
            .cloned()
            .unwrap_or(Value::Null))
    }
}

/// The script text an op gives through one of `source`, `sourceFile` and
/// `props.Source`; a file is read and syntax-checked as `rbx ck` does.
fn given_source(
    source: Option<String>,
    source_file: Option<&Path>,
    props: &mut Map<String, Value>,
) -> Result<Option<String>> {
    let from_props = props
        .keys()
        .find(|name| name.eq_ignore_ascii_case("source"))
        .cloned()
        .and_then(|name| props.remove(&name));
    if usize::from(source.is_some())
        + usize::from(source_file.is_some())
        + usize::from(from_props.is_some())
        > 1
    {
        bail!("give one of source, sourceFile or props.Source");
    }
    if let Some(path) = source_file {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Cannot read {} as UTF-8 text", path.display()))?;
        crate::studio::automation::validate_luau_syntax(&text)
            .with_context(|| format!("sourceFile {}", path.display()))?;
        return Ok(Some(text));
    }
    match from_props {
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => bail!("Source must be a string"),
        None => Ok(source),
    }
}

fn compact_path(path: &EditorInstancePath) -> Value {
    compact_path_string(&path.path_segments, Some(&path.path_ordinals))
        .map_or_else(|| json!(path.path_segments), Value::String)
}

fn path_text(document: &SettingsBytecode, service: &str, index: usize) -> String {
    build_editor_instance_paths_for_indices(document, service, &[index])
        .get(&index)
        .map_or_else(
            || document.instances[index].name.clone(),
            |path| path.path_segments.join("."),
        )
}

fn new_service_document(service: &str) -> SettingsBytecode {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let service = service.trim();
    if !service.is_empty() {
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            service.to_string(),
            service.to_string(),
            None,
        ));
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(root: &Path) -> impl Fn(&str) -> Result<(PathBuf, String)> + '_ {
        move |service| {
            Ok((
                root.join("instances").join(format!("{service}.renium")),
                service.to_string(),
            ))
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        crate::system::files::create_unique_directory(&std::env::temp_dir(), tag).unwrap()
    }

    fn run(root: &Path, raw: &str) -> Result<Value> {
        apply_store_batch(parse_batch_ops(raw)?, &resolver(root), false, false)
            .map(|outcome| outcome.result)
    }

    fn names(document: &SettingsBytecode) -> Vec<String> {
        document
            .instances
            .iter()
            .map(|instance| instance.name.clone())
            .collect()
    }

    #[test]
    fn where_matches_filters_a_subtree_by_class_and_property() {
        let root = scratch("renium-bs-where-");
        run(
            &root,
            r##"{"ops":[
                {"parent":"Workspace","name":"Lobby","class":"Folder","ref":"lobby"},
                {"parent":"#lobby","name":"Glass1","class":"Part","props":{"Transparency":0.7}},
                {"parent":"#lobby","name":"Wall","class":"Part","props":{"Transparency":0}},
                {"parent":"#lobby","name":"Light","class":"PointLight"},
                {"parent":"Workspace","name":"Glass9","class":"Part","props":{"Transparency":0.9}}
            ]}"##,
        )
        .unwrap();
        let document =
            SettingsBytecode::read_file(&root.join("instances").join("Workspace.renium")).unwrap();
        let lobby = document
            .instances
            .iter()
            .position(|instance| instance.name == "Lobby")
            .unwrap();
        let names_of = |indices: Vec<usize>| {
            indices
                .into_iter()
                .map(|index| document.instances[index].name.clone())
                .collect::<Vec<_>>()
        };
        let matches = |scope: usize, query: &str| {
            names_of(crate::bytecode::where_matches(&document, "Workspace", scope, query).unwrap())
        };
        assert_eq!(matches(lobby, "is:Part Transparency>0.5"), ["Glass1"]);
        let service_root =
            crate::settings::tree::editor_service_root_index(&document, "Workspace").unwrap();
        assert_eq!(
            matches(service_root, "is:Part Transparency>0.5"),
            ["Glass1", "Glass9"]
        );
        assert_eq!(
            matches(lobby, "Glass or is:PointLight"),
            ["Glass1", "Light"]
        );
        assert!(matches(lobby, "is:Model").is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lighting_clock_time_is_stored_as_time_of_day() {
        let root = scratch("renium-clocktime-");
        run(
            &root,
            r##"{"ops":[{"parent":"Lighting","name":"Haze","class":"Atmosphere"}]}"##,
        )
        .unwrap();
        let store = root.join("instances").join("Lighting.renium");
        let result =
            crate::bytecode::bytecode_set_property_result(crate::cli::BytecodeSetPropertyArgs {
                where_query: None,
                input: crate::cli::BytecodeFileArgs::settings_file(store.clone()),
                selector: crate::cli::BytecodeInstanceSelectorArgs::default(),
                property: vec!["ClockTime".to_string()],
                value_json: None,
                value_str: None,
                value_num: Some(18.25),
                value_bool: None,
                value_null: false,
                scope: "auto".to_string(),
                override_packages: false,
                push: false,
                pretty: false,
            })
            .unwrap();
        assert_eq!(result["property"], "TimeOfDay");
        let document = SettingsBytecode::read_file(&store).unwrap();
        let lighting = document
            .instances
            .iter()
            .find(|instance| instance.class_name == "Lighting")
            .unwrap();
        assert_eq!(lighting.properties["TimeOfDay"], "18:15:00");
        assert!(!lighting.properties.contains_key("ClockTime"));
        let database = rbx_reflection_database::get().unwrap();
        assert!(crate::editor::review::is_engine_managed_editor_property(
            "Lighting",
            "ClockTime",
            database
        ));
        assert!(!crate::editor::review::is_engine_managed_editor_property(
            "Lighting",
            "TimeOfDay",
            database
        ));
        assert_eq!(
            crate::bytecode::edit::time_of_day_from_hours(&json!(6.5)).unwrap(),
            "06:30:00"
        );
        assert!(crate::bytecode::edit::time_of_day_from_hours(&json!(25)).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_where_replaces_part_of_every_matching_name() {
        let root = scratch("renium-rn-where-");
        run(
            &root,
            r##"{"ops":[
                {"parent":"Workspace","name":"Lobby","class":"Folder","ref":"lobby"},
                {"parent":"#lobby","name":"Glass1","class":"Part"},
                {"parent":"#lobby","name":"Glass2","class":"Part"},
                {"parent":"#lobby","name":"Wall","class":"Part"},
                {"parent":"Workspace","name":"Glass9","class":"Part"}
            ]}"##,
        )
        .unwrap();
        let store = root.join("instances").join("Workspace.renium");
        let lobby_id = {
            let document = SettingsBytecode::read_file(&store).unwrap();
            document
                .instances
                .iter()
                .find(|instance| instance.name == "Lobby")
                .unwrap()
                .settings_id
                .clone()
        };
        crate::bytecode::bytecode_rename_where(
            store.clone(),
            Some(lobby_id),
            "is:Part",
            "Glass",
            "Window_",
            false,
            false,
        )
        .unwrap();
        let document = SettingsBytecode::read_file(&store).unwrap();
        let mut names = document
            .instances
            .iter()
            .map(|instance| instance.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert!(
            names.contains(&"Window_1") && names.contains(&"Window_2"),
            "{names:?}"
        );
        assert!(
            names.contains(&"Wall") && names.contains(&"Glass9"),
            "{names:?}"
        );
        assert!(!names.contains(&"Glass1"));
        let error = crate::bytecode::bytecode_rename_where(
            store.clone(),
            None,
            "is:Folder",
            "Glass",
            "Window_",
            false,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("has 'Glass' in its name"), "{error}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_creates_across_two_services_with_ref_parents() {
        let root = scratch("renium-ba-batch-");
        let result = run(
            &root,
            r##"{"ops":[
                {"parent":"ServerScriptService","name":"Battle","class":"Folder","ref":"battle"},
                {"parent":"#battle","name":"Abilities","class":"Folder","ref":"abilities"},
                {"parent":"#abilities","name":"CarMods","class":"ModuleScript","source":"return {}"},
                {"parent":"ReplicatedStorage","name":"Shared","class":"Folder","ref":"shared"},
                {"parent":"#shared","name":"Config","class":"ModuleScript","attrs":{"Version":3}},
                {"parent":"ServerScriptService.Battle","name":"Main","class":"Script"},
                {"parent":"#battle","name":"Mode","class":"StringValue","props":{"Value":"ranked"}}
            ]}"##,
        )
        .unwrap();
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["stores"], 2, "{result}");
        let ops = result["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 7, "{result}");
        assert_eq!(ops[1]["ref"], "abilities");
        assert_eq!(
            ops[2]["path"],
            "ServerScriptService.Battle.Abilities.CarMods"
        );
        assert_eq!(ops[4]["path"], "ReplicatedStorage.Shared.Config");
        assert_eq!(ops[5]["path"], "ServerScriptService.Battle.Main");
        assert!(ops[0].get("sourcePath").is_none(), "{result}");
        let car_mods = PathBuf::from(ops[2]["sourcePath"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&car_mods).unwrap(), "return {}");
        let config = PathBuf::from(ops[4]["sourcePath"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&config).unwrap(), "");
        let server =
            SettingsBytecode::read_file(&root.join("instances").join("ServerScriptService.renium"))
                .unwrap();
        assert_eq!(
            names(&server),
            [
                "ServerScriptService",
                "Battle",
                "Abilities",
                "CarMods",
                "Main",
                "Mode"
            ]
        );
        assert_eq!(server.instances[3].parent_index, Some(2));
        assert_eq!(server.instances[4].parent_index, Some(1));
        assert_eq!(server.instances[5].parent_index, Some(1));
        assert_eq!(server.instances[5].properties["Value"], "ranked");
        let shared =
            SettingsBytecode::read_file(&root.join("instances").join("ReplicatedStorage.renium"))
                .unwrap();
        assert_eq!(names(&shared), ["ReplicatedStorage", "Shared", "Config"]);
        assert_eq!(shared.instances[2].attributes["Version"], 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failing_op_leaves_the_store_untouched() {
        let root = scratch("renium-ba-batch-fail-");
        let settings_file = root.join("instances").join("ServerScriptService.renium");
        run(
            &root,
            r##"[{"parent":"ServerScriptService","name":"Existing","class":"Folder"}]"##,
        )
        .unwrap();
        let before = fs::read(&settings_file).unwrap();
        let error = run(
            &root,
            r##"[
                {"parent":"ServerScriptService","name":"Fresh","class":"Folder","ref":"fresh"},
                {"parent":"#missing","name":"Orphan","class":"ModuleScript","source":"return 1"}
            ]"##,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("op 1") && message.contains("#missing"),
            "{message}"
        );
        assert_eq!(fs::read(&settings_file).unwrap(), before);
        let service_dir = crate::project::storage::source_directory(&settings_file);
        assert!(!service_dir.join("Orphan.luau").exists());
        let bad_class = run(
            &root,
            r##"[
                {"parent":"ServerScriptService","name":"Fresh","class":"Folder"},
                {"parent":"ServerScriptService","name":"Loose","class":"Folder","source":"return 1"}
            ]"##,
        )
        .unwrap_err();
        assert!(format!("{bad_class:#}").contains("op 1"), "{bad_class:#}");
        assert_eq!(fs::read(&settings_file).unwrap(), before);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn script_op_writes_its_source_file_once_per_store() {
        let root = scratch("renium-ba-batch-script-");
        let settings_file = root.join("instances").join("ServerScriptService.renium");
        let result = run(
            &root,
            r##"[
                {"parent":"ServerScriptService","name":"Rounds","class":"ModuleScript","source":"return { rounds = 3 }\n"},
                {"parent":"ServerScriptService","name":"Main","class":"Script","source":"print('hi')"}
            ]"##,
        )
        .unwrap();
        assert_eq!(result["stores"], 1, "{result}");
        let changed = result["changedPaths"].as_array().unwrap();
        assert_eq!(changed.len(), 3, "{result}");
        let ops = result["ops"].as_array().unwrap();
        let rounds = PathBuf::from(ops[0]["sourcePath"].as_str().unwrap());
        assert!(rounds.ends_with("Rounds.luau"), "{result}");
        assert_eq!(
            fs::read_to_string(&rounds).unwrap(),
            "return { rounds = 3 }\n"
        );
        let main = PathBuf::from(ops[1]["sourcePath"].as_str().unwrap());
        assert_eq!(fs::read_to_string(&main).unwrap(), "print('hi')");
        let document = SettingsBytecode::read_file(&settings_file).unwrap();
        assert_eq!(
            document.instances[1].properties["Source"],
            crate::settings::EXTERNAL_SOURCE_MARKER
        );
        let _ = fs::remove_dir_all(&root);
    }
}
