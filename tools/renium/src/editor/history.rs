use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::app::output::{log_global, print_json_output};
use crate::app::timing::current_millis;
use crate::bytecode::{
    acquire_settings_file_lock, apply_file_mutations, collect_source_path_updates,
    file_mutation_paths,
};
use crate::cli::{EditorRevertArgs, ProjectSourceArgs, PushEditorChangesArgs};
use crate::editor::document::document_instance_index_by_settings_id;
use crate::editor::paths::build_editor_source_paths_by_index;
use crate::editor::sync::push_editor_changes_result;
use crate::editor::types::{EditorChangeSet, EditorHistoryEntry, EditorRevertManifest};
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::project::sourcemap::path_to_sourcemap_relative;
use crate::rbx::encode::rbx_serialized_property_name_for_logical;
use crate::settings::bytecode::{
    SettingsBytecode, encode_settings_bytecode, reindex_reference_indices,
};
use crate::settings::instance::{
    AddInstanceSpec, PropertyScope, add_instance, remove_instances_at_indices,
    set_instance_property,
};
use crate::settings::tree::settings_children_by_parent;
use crate::snapshot::refs::{settings_instance_path, stabilize_record_references};
use crate::studio::bridge::BridgeServer;
use crate::system::files::{
    absolutize_under, create_unique_directory, ensure_existing_ancestor_inside, path_key,
    read_json_file, resolve_project_root_if_present, sanitize_ascii_identifier,
    service_settings_path, validate_filesystem_instance_name, write_json_file, write_utf8_file,
};

pub(crate) struct EditorHistoryTransaction {
    stage_root: PathBuf,
    history_root: PathBuf,
    published: Vec<(PathBuf, PathBuf)>,
    active: bool,
}

impl EditorHistoryTransaction {
    fn create(project_root: &Path) -> Result<Self> {
        let renium_root = project_root.join(".renium");
        let stage_root = create_unique_directory(&renium_root, ".editor-history-stage-")?;
        Ok(Self {
            stage_root,
            history_root: renium_root.join("editor-history"),
            published: Vec::new(),
            active: true,
        })
    }

    pub(crate) fn publish(&mut self) -> Result<()> {
        fs::create_dir_all(&self.history_root)
            .with_context(|| format!("Failed to create {}", self.history_root.display()))?;
        let mut entries = fs::read_dir(&self.stage_root)
            .with_context(|| format!("Failed to read {}", self.stage_root.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let source = entry.path();
            let destination = self.history_root.join(entry.file_name());
            if let Err(error) = fs::rename(&source, &destination) {
                let rollback = self.rollback_published();
                if let Err(rollback_error) = rollback {
                    self.active = false;
                    bail!(
                        "Failed to publish editor history: {error}; rollback failed: {rollback_error}; recovery files remain in {}",
                        self.stage_root.display()
                    );
                }
                return Err(error).with_context(|| {
                    format!(
                        "Failed to publish editor history to {}",
                        destination.display()
                    )
                });
            }
            self.published.push((source, destination));
        }
        Ok(())
    }

    fn rollback_published(&mut self) -> Result<()> {
        for (source, destination) in self.published.iter().rev() {
            if destination.exists() {
                fs::rename(destination, source).with_context(|| {
                    format!(
                        "Failed to restore pending editor history from {}",
                        destination.display()
                    )
                })?;
            }
        }
        self.published.clear();
        Ok(())
    }

    pub(crate) fn commit(mut self) {
        self.active = false;
        let _ = fs::remove_dir(&self.stage_root);
    }
}

impl Drop for EditorHistoryTransaction {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Err(error) = self.rollback_published() {
            eprintln!(
                "[renium] editor history rollback failed: {error:#}; recovery files remain in {}",
                self.stage_root.display()
            );
            self.active = false;
            return;
        }
        let _ = fs::remove_dir_all(&self.stage_root);
    }
}

#[derive(Deserialize)]
struct EditorHistorySourceBatch {
    rows: Vec<EditorHistorySourceRow>,
}

#[derive(Deserialize)]
struct EditorHistorySourceRow {
    index: usize,
    source: Option<String>,
    error: Option<String>,
}

fn fetch_editor_history_sources(
    bridge: &BridgeServer,
    entries: &[EditorHistoryEntry],
) -> (HashMap<usize, String>, HashMap<usize, String>) {
    let mut indexes_by_service = BTreeMap::<&str, Vec<usize>>::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.source_key.is_some() {
            indexes_by_service
                .entry(&entry.service)
                .or_default()
                .push(index);
        }
    }

    let mut sources = HashMap::new();
    let mut errors = HashMap::new();
    for (service, indexes) in indexes_by_service {
        for batch in indexes.chunks(64) {
            let selectors = batch
                .iter()
                .map(|index| {
                    let entry = &entries[*index];
                    json!({
                        "index": index,
                        "pathSegments": &entry.path_segments,
                        "pathOrdinals": &entry.path_ordinals,
                    })
                })
                .collect::<Vec<_>>();
            for index in batch {
                errors.insert(
                    *index,
                    "Studio did not return the script Source".to_string(),
                );
            }
            let response = bridge
                .call(
                    "getLiveSourceBatch",
                    json!({ "service": service, "selectors": selectors }),
                )
                .and_then(|value| {
                    serde_json::from_value::<EditorHistorySourceBatch>(value)
                        .context("Studio returned an invalid live source batch")
                });
            match response {
                Ok(response) => {
                    for row in response.rows {
                        if let Some(source) = row.source {
                            sources.insert(row.index, source);
                            errors.remove(&row.index);
                        } else if let Some(error) = row.error {
                            errors.insert(row.index, error);
                        }
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    for index in batch {
                        errors.insert(*index, message.clone());
                    }
                }
            }
        }
    }
    (sources, errors)
}

pub(crate) fn save_editor_history_entries(
    bridge: &BridgeServer,
    project_root_arg: &Path,
    changes: &EditorChangeSet,
) -> Result<Option<EditorHistoryTransaction>> {
    if changes.history_entries.is_empty() {
        return Ok(None);
    }
    let project_root = resolve_project_root_if_present(project_root_arg)?;
    let mut transaction = None;
    let mut seen = HashSet::new();
    let (source_before_by_index, source_errors_by_index) =
        fetch_editor_history_sources(bridge, &changes.history_entries);

    for (sequence, entry) in changes.history_entries.iter().enumerate() {
        let identity = format!(
            "{}:{}:{}",
            entry.service,
            entry.settings_id.as_deref().unwrap_or(""),
            entry
                .source_path
                .as_ref()
                .map(|path| path_key(path))
                .unwrap_or_default()
        );
        if !seen.insert(identity) {
            continue;
        }

        let source_path = entry
            .source_path
            .as_ref()
            .map(|path| absolutize_under(&project_root, path));
        let source_before = source_before_by_index.get(&sequence);
        let source_changed = source_before.is_some_and(|before| {
            source_path
                .as_ref()
                .and_then(|path| fs::read_to_string(path).ok())
                .as_ref()
                != Some(before)
        });
        if entry.settings_before.is_none() && !source_changed {
            if let Some(error) = source_errors_by_index
                .get(&sequence)
                .filter(|error| error.as_str() != "Script was not found")
            {
                log_global(
                    5,
                    format_args!(
                        "[renium] editor history skipped Source for {}: {}",
                        entry.path_segments.join("."),
                        error
                    ),
                );
            }
            continue;
        }

        let created_unix_ms = current_millis();
        let safe_name = sanitize_history_component(
            entry
                .settings_id
                .as_deref()
                .or_else(|| entry.path_segments.last().map(String::as_str))
                .unwrap_or("item"),
        );
        let transaction =
            transaction.get_or_insert(EditorHistoryTransaction::create(&project_root)?);
        let entry_dir = transaction.stage_root.join(format!(
            "{created_unix_ms}-{sequence}-{}-{safe_name}",
            entry.service
        ));
        fs::create_dir_all(&entry_dir)
            .with_context(|| format!("Failed to create {}", entry_dir.display()))?;

        let settings_backup = if let Some(document) = entry.settings_before.as_ref() {
            let file_name = "settings.renium";
            document.write_file(&entry_dir.join(file_name))?;
            Some(file_name.to_string())
        } else {
            None
        };

        let source_backup = if source_changed {
            let file_name = "source.luau";
            write_utf8_file(
                &entry_dir.join(file_name),
                source_before.expect("changed Source has a previous value"),
            )?;
            Some(file_name.to_string())
        } else {
            None
        };

        let source_path = source_path
            .as_ref()
            .map(|path| path_to_sourcemap_relative(&project_root, path));
        let manifest = EditorRevertManifest {
            version: 1,
            created_unix_ms,
            service: entry.service.clone(),
            source_path,
            settings_id: entry.settings_id.clone(),
            path_segments: entry.path_segments.clone(),
            class_name: entry.class_name.clone(),
            settings_backup,
            source_backup,
        };
        write_json_file(&entry_dir.join("manifest.json"), &manifest, false)?;
    }

    Ok(transaction)
}

pub(crate) fn editor_revert(mut args: EditorRevertArgs) -> Result<()> {
    apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    let project_root = resolve_project_root_if_present(&args.project_root)?;
    let src_root = absolutize_under(&project_root, &args.src_dir);
    if let Some(id) = &args.sync {
        let restored =
            crate::automation::reconcile::history::revert_sync(&project_root, &src_root, id)?;
        let mut output = json!({
            "ok": true,
            "historyId": restored.id,
            "changedPathCount": restored.paths.len(),
        });
        if args.details {
            output["changedPaths"] = json!(
                restored
                    .paths
                    .iter()
                    .map(|path| path_to_sourcemap_relative(&project_root, path))
                    .collect::<Vec<_>>()
            );
        }
        if let Some(studio) = apply_reverted_paths(args, project_root, restored.paths)? {
            output["studio"] = studio;
        }
        return print_json_output(&output, false);
    }
    let requested_path = args
        .path
        .as_ref()
        .map(|path| absolutize_under(&project_root, path));
    if requested_path.is_none() && args.settings_id.is_none() {
        bail!("Provide --path, --settings-id, or --sync ID|latest");
    }

    let history_root = project_root.join(".renium").join("editor-history");
    let manifest_path = match requested_path
        .as_ref()
        .and_then(|path| history_manifest_from_request(&history_root, path))
    {
        Some(path) => path,
        None => find_newest_history_manifest(
            &project_root,
            &history_root,
            args.service.as_deref(),
            args.settings_id.as_deref(),
            requested_path.as_deref().map(path_key).as_deref(),
        )?,
    };
    let (mut output, changed_paths) =
        revert_history_manifest(&project_root, &src_root, &manifest_path)?;
    if let Some(studio) = apply_reverted_paths(args, project_root, changed_paths)? {
        output["studio"] = studio;
    }
    print_json_output(&output, false)
}

fn history_manifest_from_request(history_root: &Path, requested: &Path) -> Option<PathBuf> {
    let candidate = if requested
        .file_name()
        .is_some_and(|name| name == "manifest.json")
    {
        requested.to_path_buf()
    } else {
        requested.join("manifest.json")
    };
    let inside_history = path_key(&candidate).starts_with(&format!("{}/", path_key(history_root)));
    (inside_history && candidate.is_file()).then_some(candidate)
}

fn find_newest_history_manifest(
    project_root: &Path,
    history_root: &Path,
    service: Option<&str>,
    settings_id: Option<&str>,
    requested_path_key: Option<&str>,
) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if history_root.exists() {
        for entry in WalkDir::new(history_root) {
            let entry = entry?;
            if !entry.file_type().is_file() || entry.file_name() != "manifest.json" {
                continue;
            }
            let manifest: EditorRevertManifest = read_json_file(entry.path())?;
            if let Some(service) = service
                && manifest.service != service
            {
                continue;
            }
            let settings_matches = settings_id
                .is_some_and(|settings_id| manifest.settings_id.as_deref() == Some(settings_id));
            let path_matches = requested_path_key.is_some_and(|requested| {
                manifest
                    .source_path
                    .as_ref()
                    .map(|source_path| {
                        path_key(&absolutize_under(project_root, Path::new(source_path)))
                    })
                    .as_deref()
                    == Some(requested)
            });
            if settings_matches || path_matches {
                candidates.push((manifest.created_unix_ms, entry.path().to_path_buf()));
            }
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, path)| path)
        .context("No editor revert history found for requested path/item")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorRevertManifestExtras {
    property_name: Option<String>,
    property_scope: Option<String>,
    settings_file: Option<PathBuf>,
}

pub(crate) fn revert_history_manifest(
    project_root: &Path,
    src_root: &Path,
    manifest_path: &Path,
) -> Result<(Value, Vec<PathBuf>)> {
    let history_root = project_root.join(".renium/editor-history");
    ensure_existing_ancestor_inside(&history_root, manifest_path, "history manifest")?;
    let manifest: EditorRevertManifest = read_json_file(manifest_path)?;
    let extras: EditorRevertManifestExtras = read_json_file(manifest_path)?;
    validate_filesystem_instance_name(&manifest.service, "history service")?;
    let manifest_dir = manifest_path
        .parent()
        .context("Editor revert manifest has no parent directory")?;
    let history_id = manifest_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source_target = manifest
        .source_path
        .as_deref()
        .map(|source_path| absolutize_under(project_root, Path::new(source_path)));

    // Read every backup before changing any destination. The store and source
    // moves then use the same rollback-capable transaction as ordinary edits.
    let source = manifest
        .source_backup
        .as_deref()
        .map(|name| {
            let path = manifest_dir.join(name);
            ensure_existing_ancestor_inside(manifest_dir, &path, "source backup")?;
            fs::read(&path)
                .with_context(|| format!("Failed to read source backup {}", path.display()))
        })
        .transpose()?;
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let project_path = project_root.join(config::PROJECT_FILE_NAME);
    let loaded = project_path
        .is_file()
        .then(|| config::load_project(Some(&project_path), Some(project_root)))
        .transpose()?;
    let roots = loaded
        .as_ref()
        .map(config::project_source_roots)
        .transpose()?
        .unwrap_or_else(|| vec![src_root.to_path_buf()]);
    let default_store = service_settings_path(&src_root.join(&manifest.service));
    let store_path = extras.settings_file.as_ref().map_or_else(
        || default_store.clone(),
        |path| absolutize_under(project_root, path),
    );
    ensure_existing_ancestor_inside(project_root, &store_path, "history store")?;
    if !roots.iter().any(|root| store_path.starts_with(root)) && store_path != default_store {
        bail!(
            "History store is outside project sources: {}",
            store_path.display()
        );
    }
    let _lock = acquire_settings_file_lock(&store_path)?;
    let mut resolved_source = source_target.clone();
    let mut output = json!({
        "ok": true,
        "historyId": history_id,
        "service": manifest.service,
        "settingsId": manifest.settings_id,
        "sourcePath": manifest.source_path,
    });
    if let Some(settings_backup) = manifest.settings_backup.as_deref() {
        let Some(settings_id) = manifest.settings_id.as_deref() else {
            bail!(
                "History entry {history_id} has a store backup but no settingsId, so it cannot be restored without replacing the whole {} store",
                manifest.service
            );
        };
        let backup_path = manifest_dir.join(settings_backup);
        ensure_existing_ancestor_inside(manifest_dir, &backup_path, "settings backup")?;
        let backup = SettingsBytecode::read_file(&backup_path)?;
        if !store_path.is_file() {
            bail!(
                "The {} store no longer exists at {}",
                manifest.service,
                store_path.display()
            );
        }
        let current = SettingsBytecode::read_file(&store_path)?;
        if extras
            .property_scope
            .as_deref()
            .is_some_and(|scope| !matches!(scope, "property" | "metadata"))
        {
            bail!("Unsupported history property scope; no files were restored");
        }
        let (next, restored) = restore_history_item(
            &current,
            &backup,
            settings_id,
            extras.property_name.as_deref(),
            source_target.as_deref().is_some_and(Path::is_file),
        )?;
        let service_dir = crate::project::storage::source_directory(&store_path);
        let before = build_editor_source_paths_by_index(&current, &manifest.service, &service_dir);
        let after = collect_source_path_updates(
            &current,
            &before,
            &next,
            &manifest.service,
            &service_dir,
            &mut writes,
            &mut removals,
        )?;
        let current_paths = current
            .instances
            .iter()
            .zip(&before)
            .filter_map(|(instance, path)| {
                path.as_ref()
                    .map(|path| (instance.settings_id.as_str(), path))
            })
            .collect::<HashMap<_, _>>();
        let moving_from = next
            .instances
            .iter()
            .zip(&after)
            .filter_map(|(instance, to)| {
                let from = current_paths.get(instance.settings_id.as_str())?;
                (Some(*from) != to.as_ref()).then(|| path_key(from))
            })
            .collect::<HashSet<_>>();
        for destination in writes.keys() {
            if destination.exists() && !moving_from.contains(&path_key(destination)) {
                bail!(
                    "History source destination is occupied: {}",
                    destination.display()
                );
            }
        }
        if let Some(index) = document_instance_index_by_settings_id(&next, settings_id)
            && let Some(path) = after[index].as_ref()
        {
            resolved_source = Some(path.clone());
            if source.is_some()
                && path.is_file()
                && current_paths
                    .get(settings_id)
                    .is_none_or(|current| path_key(current) != path_key(path))
                && !moving_from.contains(&path_key(path))
            {
                bail!("History source destination is occupied: {}", path.display());
            }
        }
        output["store"] = json!(restored.label());
        if restored != HistoryStoreRestore::Unchanged {
            writes.insert(store_path.clone(), encode_settings_bytecode(&next)?);
        }
    }
    if let (Some(source), Some(to)) = (source, resolved_source) {
        ensure_existing_ancestor_inside(project_root, &to, "history source")?;
        if !roots.iter().any(|root| to.starts_with(root)) {
            bail!(
                "History source is outside project sources: {}",
                to.display()
            );
        }
        writes.insert(to, source);
    }
    for path in writes.keys().chain(removals.iter()) {
        ensure_existing_ancestor_inside(project_root, path, "history destination")?;
    }
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    output["changedPaths"] = json!(
        changed_paths
            .iter()
            .map(|path| path_to_sourcemap_relative(project_root, path))
            .collect::<Vec<_>>()
    );
    Ok((output, changed_paths))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryStoreRestore {
    Unchanged,
    PropertyRestored,
    InstanceRestored,
    SubtreeReinserted(usize),
    InstanceRemoved,
}

impl HistoryStoreRestore {
    fn label(self) -> String {
        match self {
            Self::Unchanged => "unchanged".to_string(),
            Self::PropertyRestored => "propertyRestored".to_string(),
            Self::InstanceRestored => "instanceRestored".to_string(),
            Self::SubtreeReinserted(count) => format!("reinserted:{count}"),
            Self::InstanceRemoved => "instanceRemoved".to_string(),
        }
    }
}

pub(crate) fn restore_history_item(
    current: &SettingsBytecode,
    backup: &SettingsBytecode,
    settings_id: &str,
    property_name: Option<&str>,
    source_file_exists: bool,
) -> Result<(SettingsBytecode, HistoryStoreRestore)> {
    let mut backup = backup.clone();
    let backup_ids = backup
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    for instance in &mut backup.instances {
        stabilize_record_references(&mut instance.properties, &backup_ids);
        stabilize_record_references(&mut instance.attributes, &backup_ids);
    }
    let backup_index = document_instance_index_by_settings_id(&backup, settings_id);
    let current_index = document_instance_index_by_settings_id(current, settings_id);
    let mut next = current.clone();
    let outcome = match (backup_index, current_index) {
        (None, None) => HistoryStoreRestore::Unchanged,
        (None, Some(index)) => {
            let path = settings_instance_path(current, index);
            if let Some(property) = property_name {
                bail!(
                    "{path} was not in the history backup, so its {property} value cannot be restored"
                );
            }
            if source_file_exists {
                bail!(
                    "This history entry recorded creating {path} and its source file still exists; delete the file to undo the creation, or restore a later entry"
                );
            }
            if current
                .instances
                .iter()
                .any(|instance| instance.parent_index == Some(index))
            {
                bail!(
                    "{path} gained children after this history entry; remove them first so the restore stays limited to this item"
                );
            }
            remove_instances_at_indices(&mut next, &[index], false)?;
            HistoryStoreRestore::InstanceRemoved
        }
        (Some(backup_index), Some(index)) => {
            let recorded = &backup.instances[backup_index];
            let existing = &current.instances[index];
            if recorded.name != existing.name && recorded.class_name != existing.class_name {
                bail!(
                    "{settings_id} now identifies {} ({}) instead of {} ({}); refusing to restore over a different instance",
                    settings_instance_path(current, index),
                    existing.class_name,
                    settings_instance_path(&backup, backup_index),
                    recorded.class_name
                );
            }
            match property_name {
                Some(name) => {
                    restore_instance_property(&mut next, index, &backup, backup_index, name)?;
                }
                None => restore_instance_record(&mut next, index, &backup, backup_index)?,
            }
            if next == *current {
                HistoryStoreRestore::Unchanged
            } else if property_name.is_some() {
                HistoryStoreRestore::PropertyRestored
            } else {
                HistoryStoreRestore::InstanceRestored
            }
        }
        (Some(backup_index), None) => {
            if let Some(property) = property_name {
                bail!(
                    "{} no longer exists in the store; restore its deletion entry before its {property} value",
                    settings_instance_path(&backup, backup_index)
                );
            }
            let count = reinsert_backup_subtree(&mut next, &backup, backup_index)?;
            HistoryStoreRestore::SubtreeReinserted(count)
        }
    };
    Ok((next, outcome))
}

fn restore_instance_parent(
    document: &mut SettingsBytecode,
    index: usize,
    backup: &SettingsBytecode,
    backup_index: usize,
) -> Result<()> {
    let Some(backup_parent) = backup.instances[backup_index].parent_index else {
        if document.instances[index].parent_index.is_some() {
            bail!(
                "{} was the service root in the history backup; refusing to move it back",
                settings_instance_path(document, index)
            );
        }
        return Ok(());
    };
    let parent_id = &backup.instances[backup_parent].settings_id;
    let Some(parent_index) = document_instance_index_by_settings_id(document, parent_id) else {
        bail!(
            "The parent of {} ({}) no longer exists; restore it first",
            settings_instance_path(document, index),
            settings_instance_path(backup, backup_parent)
        );
    };
    if document.instances[index].parent_index != Some(parent_index) {
        set_instance_property(
            document,
            index,
            "Parent",
            Value::String(parent_id.clone()),
            PropertyScope::Metadata,
        )?;
    }
    Ok(())
}

fn restore_instance_record(
    document: &mut SettingsBytecode,
    index: usize,
    backup: &SettingsBytecode,
    backup_index: usize,
) -> Result<()> {
    restore_instance_parent(document, index, backup, backup_index)?;
    let recorded = &backup.instances[backup_index];
    let instance = &mut document.instances[index];
    instance.name.clone_from(&recorded.name);
    instance.class_name.clone_from(&recorded.class_name);
    instance.properties.clone_from(&recorded.properties);
    instance.attributes.clone_from(&recorded.attributes);
    let indices = settings_indices_by_id(document);
    let instance = &mut document.instances[index];
    reindex_reference_indices(&mut instance.properties, &indices);
    reindex_reference_indices(&mut instance.attributes, &indices);
    Ok(())
}

fn restore_instance_property(
    document: &mut SettingsBytecode,
    index: usize,
    backup: &SettingsBytecode,
    backup_index: usize,
    name: &str,
) -> Result<()> {
    let recorded = &backup.instances[backup_index];
    match name {
        "Name" => document.instances[index].name.clone_from(&recorded.name),
        "ClassName" => document.instances[index]
            .class_name
            .clone_from(&recorded.class_name),
        "Parent" => restore_instance_parent(document, index, backup, backup_index)?,
        _ => {
            // The properties panel exposes these logical names; backups contain
            // their saved fields, without applying the live setter conversions.
            let serialized = match name {
                "WorldPivot" | "Origin"
                    if matches!(recorded.class_name.as_str(), "Model" | "Workspace") =>
                {
                    Some("WorldPivotData")
                }
                "Enabled" if matches!(recorded.class_name.as_str(), "Script" | "LocalScript") => {
                    Some("Disabled")
                }
                _ => rbx_reflection_database::get().ok().and_then(|db| {
                    rbx_serialized_property_name_for_logical(db, &recorded.class_name, name)
                }),
            };
            let matches = |key: &str| {
                key.eq_ignore_ascii_case(name)
                    || serialized.is_some_and(|serialized| key.eq_ignore_ascii_case(serialized))
            };
            let mut record = Map::new();
            if let Some((key, value)) = recorded.properties.iter().find(|(key, _)| matches(key)) {
                record.insert(key.clone(), value.clone());
            }
            let indices = settings_indices_by_id(document);
            reindex_reference_indices(&mut record, &indices);
            let properties = &mut document.instances[index].properties;
            properties.retain(|key, _| !matches(key));
            properties.append(&mut record);
        }
    }
    Ok(())
}

fn reinsert_backup_subtree(
    document: &mut SettingsBytecode,
    backup: &SettingsBytecode,
    backup_index: usize,
) -> Result<usize> {
    let children = settings_children_by_parent(backup);
    let mut order = vec![backup_index];
    let mut visited = HashSet::from([backup_index]);
    let mut cursor = 0;
    while cursor < order.len() {
        for child in &children[order[cursor]] {
            if visited.insert(*child) {
                order.push(*child);
            }
        }
        cursor += 1;
    }

    let target_path = settings_instance_path(backup, backup_index);
    let Some(backup_parent) = backup.instances[backup_index].parent_index else {
        bail!("Cannot restore the service root {target_path} from history");
    };
    let parent_id = &backup.instances[backup_parent].settings_id;
    let Some(parent_index) = document_instance_index_by_settings_id(document, parent_id) else {
        bail!(
            "The parent of {target_path} ({}) no longer exists; restore it first",
            settings_instance_path(backup, backup_parent)
        );
    };
    for index in &order {
        let id = &backup.instances[*index].settings_id;
        if let Some(existing) = document_instance_index_by_settings_id(document, id) {
            bail!(
                "{id} already exists in the store as {}; restoring {target_path} would duplicate it",
                settings_instance_path(document, existing)
            );
        }
    }
    let name = &backup.instances[backup_index].name;
    if document
        .instances
        .iter()
        .any(|instance| instance.parent_index == Some(parent_index) && instance.name == *name)
    {
        bail!(
            "An instance named {name} already exists under {}; restoring {target_path} would create a duplicate",
            settings_instance_path(document, parent_index)
        );
    }

    let mut inserted = HashMap::new();
    for index in &order {
        let recorded = &backup.instances[*index];
        let parent = if *index == backup_index {
            parent_index
        } else {
            let backup_parent = recorded
                .parent_index
                .context("history backup descendant has no parent")?;
            *inserted
                .get(&backup_parent)
                .context("history backup descendant was visited before its parent")?
        };
        let added = add_instance(
            document,
            AddInstanceSpec {
                settings_id: Some(recorded.settings_id.clone()),
                name: recorded.name.clone(),
                class_name: recorded.class_name.clone(),
                parent_index: Some(parent),
                properties: recorded.properties.clone(),
                attributes: recorded.attributes.clone(),
            },
        )?;
        inserted.insert(*index, added.index);
    }

    let indices = settings_indices_by_id(document);
    let inserted_indices = inserted.values().copied().collect::<HashSet<_>>();
    for index in &inserted_indices {
        let instance = &mut document.instances[*index];
        reindex_reference_indices(&mut instance.properties, &indices);
        reindex_reference_indices(&mut instance.attributes, &indices);
    }

    // Other instances keep their present reference values. An empty reference
    // does not prove deletion cleared it; it may be a later deliberate edit.
    Ok(order.len())
}

fn settings_indices_by_id(document: &SettingsBytecode) -> HashMap<String, usize> {
    document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect()
}

fn apply_reverted_paths(
    args: EditorRevertArgs,
    project_root: PathBuf,
    changed_paths: Vec<PathBuf>,
) -> Result<Option<Value>> {
    if args.apply_studio && !changed_paths.is_empty() {
        return push_editor_changes_result(PushEditorChangesArgs {
            changed_paths,
            verify_sources: true,
            ..PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root,
                    src_root: args.src_dir,
                },
                args.bridge,
            )
        })
        .map(Some)
        .context(
            "Files were restored locally, but Studio sync failed; retry syncing the restored paths",
        );
    }

    Ok(None)
}

fn sanitize_history_component(value: &str) -> String {
    let out = sanitize_ascii_identifier(value);
    if out.is_empty() {
        "item".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::{Map, Value, json};

    use super::*;
    use crate::settings::bytecode::SettingsBytecodeInstance;
    use crate::system::files::{create_unique_directory, service_settings_path};

    fn instance(
        settings_id: &str,
        name: &str,
        class_name: &str,
        parent_index: Option<usize>,
        properties: &[(&str, Value)],
    ) -> SettingsBytecodeInstance {
        let mut instance = SettingsBytecodeInstance::new(
            settings_id.to_string(),
            name.to_string(),
            class_name.to_string(),
            parent_index,
        );
        instance.properties = properties
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect::<Map<_, _>>();
        instance
    }

    fn document(instances: Vec<SettingsBytecodeInstance>) -> SettingsBytecode {
        SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances,
        }
    }

    fn by_id<'a>(
        document: &'a SettingsBytecode,
        settings_id: &str,
    ) -> &'a SettingsBytecodeInstance {
        document
            .instances
            .iter()
            .find(|instance| instance.settings_id == settings_id)
            .unwrap_or_else(|| panic!("{settings_id} should exist"))
    }

    struct HistoryProject {
        root: PathBuf,
        src_root: PathBuf,
        store: PathBuf,
    }

    impl HistoryProject {
        fn new(current: &SettingsBytecode) -> Self {
            let root = create_unique_directory(&std::env::temp_dir(), "renium-history-test-")
                .expect("temp project");
            let src_root = root.join("src");
            let store = service_settings_path(&src_root.join("Workspace"));
            current.write_file(&store).expect("write current store");
            Self {
                root,
                src_root,
                store,
            }
        }

        fn write_entry(
            &self,
            id: &str,
            backup: Option<&SettingsBytecode>,
            source: Option<(&str, &str)>,
            manifest_extra: &[(&str, Value)],
        ) -> PathBuf {
            let entry_dir = self.root.join(".renium").join("editor-history").join(id);
            fs::create_dir_all(&entry_dir).expect("entry dir");
            let mut manifest = json!({
                "version": 1,
                "createdUnixMs": 1,
                "service": "Workspace",
                "settingsId": "a",
                "pathSegments": ["Workspace", "A"],
                "className": "Script",
            });
            if let Some(backup) = backup {
                backup
                    .write_file(&entry_dir.join("settings.renium"))
                    .expect("write backup");
                manifest["settingsBackup"] = json!("settings.renium");
            }
            if let Some((relative_path, content)) = source {
                fs::write(entry_dir.join("source.luau"), content).expect("write source backup");
                manifest["sourceBackup"] = json!("source.luau");
                manifest["sourcePath"] = json!(relative_path);
            }
            for (key, value) in manifest_extra {
                manifest[*key] = value.clone();
            }
            let manifest_path = entry_dir.join("manifest.json");
            fs::write(
                &manifest_path,
                serde_json::to_string_pretty(&manifest).expect("manifest json"),
            )
            .expect("write manifest");
            manifest_path
        }

        fn read_store(&self) -> SettingsBytecode {
            SettingsBytecode::read_file(&self.store).expect("read store")
        }

        fn leftover_files(&self) -> Vec<String> {
            fs::read_dir(self.store.parent().expect("store parent"))
                .expect("read store dir")
                .map(|entry| {
                    entry
                        .expect("dir entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|name| name.ends_with(".renium-tmp") || name.ends_with(".lock"))
                .collect()
        }
    }

    impl Drop for HistoryProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn deleted_script_fixture() -> (SettingsBytecode, SettingsBytecode) {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("a", "A", "Script", Some(0), &[("Disabled", json!(true))]),
            instance("b", "B", "Part", Some(0), &[("Transparency", json!(0.0))]),
        ]);
        let current = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("b", "B", "Part", Some(0), &[("Transparency", json!(0.5))]),
            instance("c", "C", "Folder", Some(0), &[]),
        ]);
        (backup, current)
    }

    #[test]
    fn missing_source_backup_does_not_partially_restore_the_store() {
        let (backup, current) = deleted_script_fixture();
        let project = HistoryProject::new(&current);
        let manifest = project.write_entry(
            "missing-source",
            Some(&backup),
            Some(("src/Workspace/A.server.luau", "return 1")),
            &[],
        );
        fs::remove_file(manifest.parent().unwrap().join("source.luau")).unwrap();
        assert!(revert_history_manifest(&project.root, &project.src_root, &manifest).is_err());
        assert_eq!(
            project.read_store(),
            current,
            "failed restores must not commit their store first"
        );
    }

    #[test]
    fn restoring_a_script_name_moves_its_file_and_preserves_source() {
        let (backup, _) = deleted_script_fixture();
        let mut current = backup.clone();
        current.instances[1].name = "Renamed".into();
        let project = HistoryProject::new(&current);
        let before = project.src_root.join("Workspace/Renamed.server.luau");
        fs::write(&before, "return 'latest source'").unwrap();
        let manifest = project.write_entry(
            "renamed",
            Some(&backup),
            None,
            &[("propertyName", json!("Name"))],
        );
        revert_history_manifest(&project.root, &project.src_root, &manifest).unwrap();
        assert!(!before.exists());
        assert_eq!(
            fs::read_to_string(project.src_root.join("Workspace/A.server.luau")).unwrap(),
            "return 'latest source'"
        );
    }

    #[test]
    fn restoring_a_property_resolves_its_serialized_name() {
        let (mut backup, _) = deleted_script_fixture();
        backup.instances[1].class_name = "Model".into();
        let initial = json!({"_type":"CFrame", "components":[1,2,3,1,0,0,0,1,0,0,0,1]});
        let changed = json!({"_type":"CFrame", "components":[4,5,6,1,0,0,0,1,0,0,0,1]});
        backup.instances[1]
            .properties
            .insert("WorldPivotData".into(), initial.clone());
        let mut current = backup.clone();
        current.instances[1]
            .properties
            .insert("WorldPivotData".into(), changed);
        let (restored, _) =
            restore_history_item(&current, &backup, "a", Some("WorldPivot"), false).unwrap();
        assert_eq!(by_id(&restored, "a").properties["WorldPivotData"], initial);
    }

    #[test]
    fn restoring_enabled_uses_the_saved_disabled_field() {
        let (backup, _) = deleted_script_fixture();
        let mut current = backup.clone();
        current.instances[1]
            .properties
            .insert("Disabled".into(), json!(false));
        let (restored, _) =
            restore_history_item(&current, &backup, "a", Some("Enabled"), false).unwrap();
        assert_eq!(by_id(&restored, "a").properties["Disabled"], json!(true));
    }

    #[test]
    fn script_rename_refuses_to_overwrite_an_unrelated_file() {
        let (backup, _) = deleted_script_fixture();
        let mut current = backup.clone();
        current.instances[1].name = "Renamed".into();
        let project = HistoryProject::new(&current);
        let from = project.src_root.join("Workspace/Renamed.server.luau");
        let to = project.src_root.join("Workspace/A.server.luau");
        fs::write(&from, "latest source").unwrap();
        fs::write(&to, "unrelated source").unwrap();
        let manifest = project.write_entry(
            "collision",
            Some(&backup),
            None,
            &[("propertyName", json!("Name"))],
        );
        assert!(revert_history_manifest(&project.root, &project.src_root, &manifest).is_err());
        assert_eq!(project.read_store(), current);
        assert_eq!(fs::read_to_string(from).unwrap(), "latest source");
        assert_eq!(fs::read_to_string(to).unwrap(), "unrelated source");
    }

    #[test]
    fn mounted_history_restores_the_owned_store_and_script_path() {
        let (backup, _) = deleted_script_fixture();
        let mut current = backup.clone();
        current.instances[1].name = "Renamed".into();
        let project = HistoryProject::new(&current);
        let config_path = project.root.join(config::PROJECT_FILE_NAME);
        fs::write(
            &config_path,
            r#"{"name":"history","sourceRoot":"src","tree":{"Workspace":{"$path":"code/server"}}}"#,
        )
        .unwrap();
        fs::create_dir_all(project.root.join("code/server")).unwrap();
        let loaded = config::load_project(Some(&config_path), Some(&project.root)).unwrap();
        let store = service_settings_path(&loaded.root.join("code/server"));
        current.write_file(&store).unwrap();
        fs::write(
            project.root.join("code/server/Renamed.server.luau"),
            "latest source",
        )
        .unwrap();
        let manifest = project.write_entry(
            "mounted",
            Some(&backup),
            None,
            &[
                ("propertyName", json!("Name")),
                ("settingsFile", json!(store)),
            ],
        );
        revert_history_manifest(&project.root, &project.src_root, &manifest).unwrap();
        assert_eq!(
            SettingsBytecode::read_file(&store).unwrap().instances[1].name,
            "A"
        );
        assert_eq!(
            fs::read_to_string(project.root.join("code/server/A.server.luau")).unwrap(),
            "latest source"
        );
        assert!(
            !project
                .root
                .join("code/server/Renamed.server.luau")
                .exists()
        );
        crate::project::storage::forget(&project.root);
    }

    #[test]
    fn restoring_a_deleted_script_keeps_unrelated_newer_store_edits() {
        let (backup, current) = deleted_script_fixture();
        let project = HistoryProject::new(&current);
        let source_path = project.src_root.join("Workspace").join("A.server.luau");
        let manifest_path = project.write_entry(
            "1-0-Workspace-a",
            Some(&backup),
            Some(("src/Workspace/A.server.luau", "print('restored')\n")),
            &[],
        );

        let (output, changed_paths) =
            revert_history_manifest(&project.root, &project.src_root, &manifest_path)
                .expect("revert succeeds");

        let restored = project.read_store();
        assert_eq!(
            by_id(&restored, "b").properties.get("Transparency"),
            Some(&json!(0.5)),
            "B's newer edit must survive restoring A"
        );
        assert_eq!(
            by_id(&restored, "c").class_name,
            "Folder",
            "C added later must survive"
        );
        let a = by_id(&restored, "a");
        assert_eq!(a.class_name, "Script");
        assert_eq!(a.parent_index, Some(0));
        assert_eq!(a.properties.get("Disabled"), Some(&json!(true)));
        assert_eq!(
            fs::read_to_string(&source_path).expect("source restored"),
            "print('restored')\n"
        );
        assert_eq!(
            changed_paths.into_iter().collect::<HashSet<_>>(),
            HashSet::from([project.store.clone(), source_path])
        );
        assert_eq!(output["ok"], json!(true));
        assert!(
            project.leftover_files().is_empty(),
            "no temp or lock files may remain next to the store: {:?}",
            project.leftover_files()
        );
    }

    #[test]
    fn manifest_selection_accepts_a_history_entry_path() {
        let (backup, current) = deleted_script_fixture();
        let project = HistoryProject::new(&current);
        let manifest_path = project.write_entry("1-0-Workspace-a", Some(&backup), None, &[]);
        let history_root = project.root.join(".renium").join("editor-history");
        let entry_dir = manifest_path.parent().expect("entry dir").to_path_buf();

        assert_eq!(
            history_manifest_from_request(&history_root, &manifest_path),
            Some(manifest_path.clone())
        );
        assert_eq!(
            history_manifest_from_request(&history_root, &entry_dir),
            Some(manifest_path)
        );
        assert_eq!(
            history_manifest_from_request(&history_root, &project.src_root),
            None
        );
        let outside = project.root.join("manifest.json");
        fs::write(&outside, "{}").expect("write outside manifest");
        assert_eq!(history_manifest_from_request(&history_root, &outside), None);
    }

    #[test]
    fn missing_store_is_refused_instead_of_recreated_from_the_backup() {
        let (backup, current) = deleted_script_fixture();
        let project = HistoryProject::new(&current);
        let manifest_path = project.write_entry("1-0-Workspace-a", Some(&backup), None, &[]);
        fs::remove_file(&project.store).expect("remove store");

        let error = revert_history_manifest(&project.root, &project.src_root, &manifest_path)
            .expect_err("missing store must be refused");

        assert!(error.to_string().contains("no longer exists"), "{error:#}");
        assert!(!project.store.exists());
    }

    fn reference(settings_id: &str, index: usize) -> Value {
        json!({ "_type": "Ref", "instanceIndex": index + 1, "settingsId": settings_id })
    }

    #[test]
    fn reinserting_a_deleted_subtree_remaps_its_references_and_preserves_other_instances() {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "holder",
                "Holder",
                "ObjectValue",
                Some(0),
                &[("Value", reference("a", 2))],
            ),
            instance(
                "a",
                "A",
                "Folder",
                Some(0),
                &[("Link", reference("holder", 1))],
            ),
            instance(
                "child",
                "Child",
                "Script",
                Some(2),
                &[("Owner", reference("a", 2))],
            ),
            instance("b", "B", "Part", Some(0), &[]),
        ]);
        let current = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "holder",
                "Holder",
                "ObjectValue",
                Some(0),
                &[("Value", json!({ "_type": "Ref" }))],
            ),
            instance("b", "B", "Part", Some(0), &[("Anchored", json!(true))]),
            instance("later", "Later", "Part", Some(0), &[]),
        ]);

        let (restored, outcome) =
            restore_history_item(&current, &backup, "a", None, false).expect("restore");

        assert_eq!(outcome, HistoryStoreRestore::SubtreeReinserted(2));
        let a_index = restored
            .instances
            .iter()
            .position(|instance| instance.settings_id == "a")
            .expect("a restored");
        let child = by_id(&restored, "child");
        assert_eq!(child.parent_index, Some(a_index));
        assert_eq!(
            child.properties.get("Owner"),
            Some(&reference("a", a_index))
        );
        assert_eq!(
            by_id(&restored, "a").properties.get("Link"),
            Some(&reference("holder", 1))
        );
        assert_eq!(
            by_id(&restored, "holder").properties.get("Value"),
            Some(&json!({"_type":"Ref"}))
        );
        assert_eq!(
            by_id(&restored, "b").properties.get("Anchored"),
            Some(&json!(true))
        );
        assert_eq!(by_id(&restored, "later").name, "Later");
        assert_eq!(restored.instances.len(), 6);
    }

    #[test]
    fn reinsertion_leaves_references_that_were_repointed_later() {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "holder",
                "Holder",
                "ObjectValue",
                Some(0),
                &[("Value", reference("a", 2))],
            ),
            instance("a", "A", "Folder", Some(0), &[]),
            instance("b", "B", "Part", Some(0), &[]),
        ]);
        let current = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "holder",
                "Holder",
                "ObjectValue",
                Some(0),
                &[("Value", reference("b", 2))],
            ),
            instance("b", "B", "Part", Some(0), &[]),
        ]);

        let (restored, _) =
            restore_history_item(&current, &backup, "a", None, false).expect("restore");

        assert_eq!(
            by_id(&restored, "holder").properties.get("Value"),
            Some(&reference("b", 2))
        );
    }

    #[test]
    fn property_entry_restores_only_that_property() {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "a",
                "A",
                "Part",
                Some(0),
                &[("Transparency", json!(0.0)), ("Anchored", json!(false))],
            ),
        ]);
        let current = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "a",
                "A",
                "Part",
                Some(0),
                &[
                    ("Transparency", json!(0.75)),
                    ("Anchored", json!(true)),
                    ("CanCollide", json!(false)),
                ],
            ),
        ]);

        let (restored, outcome) =
            restore_history_item(&current, &backup, "a", Some("Transparency"), false)
                .expect("restore");

        assert_eq!(outcome, HistoryStoreRestore::PropertyRestored);
        let a = by_id(&restored, "a");
        assert_eq!(a.properties.get("Transparency"), Some(&json!(0.0)));
        assert_eq!(a.properties.get("Anchored"), Some(&json!(true)));
        assert_eq!(a.properties.get("CanCollide"), Some(&json!(false)));

        let (restored, outcome) =
            restore_history_item(&current, &backup, "a", Some("CanCollide"), false)
                .expect("restore removes a property that was absent before");
        assert_eq!(outcome, HistoryStoreRestore::PropertyRestored);
        assert_eq!(by_id(&restored, "a").properties.get("CanCollide"), None);

        let (restored, outcome) =
            restore_history_item(&current, &backup, "a", Some("Name"), false).expect("restore");
        assert_eq!(outcome, HistoryStoreRestore::Unchanged);
        assert_eq!(restored, current);
    }

    #[test]
    fn whole_item_restore_keeps_children_added_later() {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance(
                "a",
                "A",
                "Script",
                Some(0),
                &[("RunContext", json!("Server"))],
            ),
            instance("b", "B", "Part", Some(0), &[]),
        ]);
        let current = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("a", "A", "Folder", Some(0), &[]),
            instance("b", "B", "Part", Some(0), &[("Anchored", json!(true))]),
            instance("inner", "Inner", "ModuleScript", Some(1), &[]),
        ]);

        let (restored, outcome) =
            restore_history_item(&current, &backup, "a", None, false).expect("restore");

        assert_eq!(outcome, HistoryStoreRestore::InstanceRestored);
        assert_eq!(by_id(&restored, "a").class_name, "Script");
        assert_eq!(
            by_id(&restored, "a").properties.get("RunContext"),
            Some(&json!("Server"))
        );
        assert_eq!(by_id(&restored, "inner").parent_index, Some(1));
        assert_eq!(
            by_id(&restored, "b").properties.get("Anchored"),
            Some(&json!(true))
        );
    }

    #[test]
    fn created_entry_removes_the_instance_only_when_it_is_safe() {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("b", "B", "Part", Some(0), &[]),
        ]);
        let current = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("b", "B", "Part", Some(0), &[("Anchored", json!(true))]),
            instance("a", "A", "Script", Some(0), &[]),
        ]);

        let (restored, outcome) =
            restore_history_item(&current, &backup, "a", None, false).expect("restore");
        assert_eq!(outcome, HistoryStoreRestore::InstanceRemoved);
        assert!(
            restored
                .instances
                .iter()
                .all(|instance| instance.settings_id != "a")
        );
        assert_eq!(
            by_id(&restored, "b").properties.get("Anchored"),
            Some(&json!(true))
        );

        let error = restore_history_item(&current, &backup, "a", None, true)
            .expect_err("existing source file must be refused");
        assert!(
            error.to_string().contains("source file still exists"),
            "{error:#}"
        );

        let mut with_child = current.clone();
        with_child
            .instances
            .push(instance("child", "Child", "Folder", Some(2), &[]));
        let error = restore_history_item(&with_child, &backup, "a", None, false)
            .expect_err("children added later must be refused");
        assert!(error.to_string().contains("gained children"), "{error:#}");

        let error = restore_history_item(&current, &backup, "a", Some("Anchored"), false)
            .expect_err("property restore needs the instance in the backup");
        assert!(
            error.to_string().contains("was not in the history backup"),
            "{error:#}"
        );
    }

    #[test]
    fn ambiguous_restores_are_refused_without_touching_the_store() {
        let backup = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("folder", "Folder", "Folder", Some(0), &[]),
            instance("a", "A", "Script", Some(1), &[]),
        ]);

        let parent_gone = document(vec![instance("root", "Workspace", "Workspace", None, &[])]);
        let error = restore_history_item(&parent_gone, &backup, "a", None, false)
            .expect_err("missing parent");
        assert!(error.to_string().contains("no longer exists"), "{error:#}");

        let duplicate_name = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("folder", "Folder", "Folder", Some(0), &[]),
            instance("a2", "A", "Script", Some(1), &[]),
        ]);
        let error = restore_history_item(&duplicate_name, &backup, "a", None, false)
            .expect_err("duplicate name");
        assert!(
            error.to_string().contains("already exists under"),
            "{error:#}"
        );

        let reused_id = document(vec![
            instance("root", "Workspace", "Workspace", None, &[]),
            instance("folder", "Folder", "Folder", Some(0), &[]),
            instance("a", "Lamp", "PointLight", Some(1), &[]),
        ]);
        let error = restore_history_item(&reused_id, &backup, "a", None, false)
            .expect_err("reused settings id");
        assert!(
            error.to_string().contains("different instance"),
            "{error:#}"
        );

        let error = restore_history_item(&parent_gone, &backup, "a", Some("Disabled"), false)
            .expect_err("property restore on a deleted instance");
        assert!(
            error.to_string().contains("restore its deletion entry"),
            "{error:#}"
        );

        let root_only = document(vec![instance("root", "Workspace", "Workspace", None, &[])]);
        let (restored, outcome) = restore_history_item(&root_only, &backup, "missing", None, false)
            .expect("unknown id is a no-op");
        assert_eq!(outcome, HistoryStoreRestore::Unchanged);
        assert_eq!(restored, root_only);
    }
}
