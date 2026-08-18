use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::edit::{
    collect_settings_subtree_preorder, instance_path_parts_key, next_editor_settings_id_fast,
    path_ordinals_from_value, path_segments_from_value, prune_removed_source_dirs,
};
use crate::bytecode::{
    apply_file_mutations, collect_source_path_updates, file_mutation_paths,
    lock_existing_service_store, preserve_source_path_extension,
};
use crate::editor::document::is_protected_starter_player_container;
use crate::editor::paths::{
    build_editor_instance_paths, build_editor_source_paths_by_index, script_file_names,
};
use crate::rbx::model::canonicalize_settings_reference_documents;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, encode_settings_bytecode,
    stabilize_reference_objects, visit_reference_objects_mut,
};
use crate::settings::instance;
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::system::files::{exact_path_key, service_settings_path};

struct MovedReference {
    settings_id: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
}

fn service_store_paths(src_root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    for entry in
        fs::read_dir(src_root).with_context(|| format!("Failed to read {}", src_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let settings_file = service_settings_path(&entry.path());
        if settings_file.is_file() {
            files.insert(
                entry.file_name().to_string_lossy().into_owned(),
                settings_file,
            );
        }
    }
    Ok(files)
}

fn stabilize_document_references(document: &mut SettingsBytecode) {
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    for instance in &mut document.instances {
        for record in [&mut instance.properties, &mut instance.attributes] {
            stabilize_reference_objects(record, |object, index| {
                if let Some(settings_id) = ids.get(index) {
                    object.insert("settingsId".to_string(), Value::String(settings_id.clone()));
                }
            });
        }
    }
}

fn rewrite_moved_references(
    record: &mut Map<String, Value>,
    moved: &HashMap<String, MovedReference>,
) {
    visit_reference_objects_mut(record, |object| {
        let Some(path_segments) = object
            .get("pathSegments")
            .and_then(path_segments_from_value)
        else {
            return;
        };
        let Some(path_ordinals) = object
            .get("pathOrdinals")
            .and_then(path_ordinals_from_value)
        else {
            return;
        };
        let Some(target) = moved.get(&instance_path_parts_key(&path_segments, &path_ordinals))
        else {
            return;
        };
        for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
            object.remove(selector);
        }
        object.insert(
            "settingsId".to_string(),
            Value::String(target.settings_id.clone()),
        );
        object.insert(
            "pathSegments".to_string(),
            Value::Array(
                target
                    .path_segments
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        object.insert(
            "pathOrdinals".to_string(),
            Value::Array(
                target
                    .path_ordinals
                    .iter()
                    .map(|ordinal| Value::from(*ordinal))
                    .collect(),
            ),
        );
    });
}

fn source_root_for_stores(source_file: &Path, target_file: &Path) -> Result<PathBuf> {
    let source_root = source_file
        .parent()
        .and_then(Path::parent)
        .context("Source settings file is not inside a service directory")?;
    let target_root = target_file
        .parent()
        .and_then(Path::parent)
        .context("Target settings file is not inside a service directory")?;
    if exact_path_key(source_root) != exact_path_key(target_root) {
        bail!("Cross-service moves require both services to use the same source root");
    }
    Ok(source_root.to_path_buf())
}

pub(crate) fn move_instance_between_service_stores(
    source_file: &Path,
    source_service: &str,
    source_settings_id: &str,
    target_file: &Path,
    target_service: &str,
    target_parent_settings_id: &str,
) -> Result<()> {
    let src_root = source_root_for_stores(source_file, target_file)?;
    let files = service_store_paths(&src_root)?;
    let lock_paths = files.values().cloned().collect::<BTreeSet<_>>();
    let _locks = lock_paths
        .iter()
        .map(|path| lock_existing_service_store(path))
        .collect::<Result<Vec<_>>>()?;
    let mut documents = files
        .iter()
        .map(|(service, path)| {
            SettingsBytecode::read_file(path).map(|document| (service.clone(), document))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    canonicalize_settings_reference_documents(&mut documents);
    for document in documents.values_mut() {
        stabilize_document_references(document);
    }

    let mut source = documents
        .remove(source_service)
        .with_context(|| format!("Source service '{source_service}' has no Renium store"))?;
    let mut target = documents
        .remove(target_service)
        .with_context(|| format!("Target service '{target_service}' has no Renium store"))?;
    let source_before = source.clone();
    let source_index = source
        .instances
        .iter()
        .position(|instance| instance.settings_id == source_settings_id)
        .with_context(|| {
            format!("Source service '{source_service}' has no instance id '{source_settings_id}'")
        })?;
    if source.instances[source_index].parent_index.is_none() {
        bail!("Service roots cannot be moved");
    }
    if is_protected_starter_player_container(&source, source_index) {
        bail!("{} cannot be moved", source.instances[source_index].name);
    }
    let target_parent_index = target
        .instances
        .iter()
        .position(|instance| instance.settings_id == target_parent_settings_id)
        .with_context(|| {
            format!(
                "Target service '{target_service}' has no instance id '{target_parent_settings_id}'"
            )
        })?;

    let children = settings_children_by_parent(&source);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children, source_index, &mut subtree);
    let source_paths_before = build_editor_source_paths_by_index(
        &source_before,
        source_service,
        source_file
            .parent()
            .context("Source settings file has no parent")?,
    );
    let old_paths = build_editor_instance_paths(&source, source_service);
    let mut target_ids = target
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut next_id_seed = target.instances.len();
    let mut new_index_by_old = HashMap::with_capacity(subtree.len());

    for old_index in subtree.iter().copied() {
        let mut moved = source.instances[old_index].clone();
        moved.parent_index = if old_index == source_index {
            Some(target_parent_index)
        } else {
            Some(
                moved
                    .parent_index
                    .and_then(|parent| new_index_by_old.get(&parent).copied())
                    .context("Moved subtree is missing its parent")?,
            )
        };
        if !target_ids.insert(moved.settings_id.clone()) {
            moved.settings_id = next_editor_settings_id_fast(&mut target_ids, &mut next_id_seed);
        }
        let new_index = target.instances.len();
        target.instances.push(moved);
        new_index_by_old.insert(old_index, new_index);
    }

    instance::remove_instances_at_indices(&mut source, &[source_index], true)?;
    documents.insert(source_service.to_string(), source);
    documents.insert(target_service.to_string(), target);

    let target_paths = build_editor_instance_paths(&documents[target_service], target_service);
    let moved_references = subtree
        .iter()
        .filter_map(|old_index| {
            let old_path = old_paths.get(*old_index)?.as_ref()?;
            let new_index = new_index_by_old[old_index];
            let new_path = target_paths.get(new_index)?.as_ref()?;
            let moved = &documents[target_service].instances[new_index];
            Some((
                instance_path_parts_key(&old_path.path_segments, &old_path.path_ordinals),
                MovedReference {
                    settings_id: moved.settings_id.clone(),
                    path_segments: new_path.path_segments.clone(),
                    path_ordinals: new_path.path_ordinals.clone(),
                },
            ))
        })
        .collect::<HashMap<_, _>>();
    for document in documents.values_mut() {
        for instance in &mut document.instances {
            rewrite_moved_references(&mut instance.properties, &moved_references);
            rewrite_moved_references(&mut instance.attributes, &moved_references);
        }
    }
    canonicalize_settings_reference_documents(&mut documents);

    let source_after = &documents[source_service];
    let target_after = &documents[target_service];
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    collect_source_path_updates(
        &source_before,
        &source_paths_before,
        source_after,
        source_service,
        source_file
            .parent()
            .context("Source settings file has no parent")?,
        &mut writes,
        &mut removals,
    )?;
    let mut target_source_paths = build_editor_source_paths_by_index(
        target_after,
        target_service,
        target_file
            .parent()
            .context("Target settings file has no parent")?,
    );
    for old_index in subtree.iter().copied() {
        if script_file_names(&source_before.instances[old_index].class_name).is_none() {
            continue;
        }
        let Some(Some(from)) = source_paths_before.get(old_index) else {
            continue;
        };
        let new_index = new_index_by_old[&old_index];
        let Some(Some(to)) = target_source_paths.get_mut(new_index) else {
            continue;
        };
        preserve_source_path_extension(from, to);
        if from.is_file() {
            writes.insert(
                to.clone(),
                fs::read(from).with_context(|| format!("Failed to read {}", from.display()))?,
            );
            removals.push(from.clone());
        }
    }

    let source_store_removed = source_after.instances.is_empty()
        || (source_after.instances.len() == 1
            && editor_service_root_index(source_after, source_service).is_some());
    for (service, document) in &documents {
        let path = &files[service];
        if service == source_service && source_store_removed {
            removals.push(path.clone());
        } else {
            writes.insert(path.clone(), encode_settings_bytecode(document)?);
        }
    }
    removals.retain(|path| {
        !writes
            .keys()
            .any(|write| exact_path_key(write) == exact_path_key(path))
    });
    removals.sort_by_key(|path| exact_path_key(path));
    removals.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    if let Some(source_dir) = source_file.parent() {
        prune_removed_source_dirs(source_dir, &removals);
    }

    let root_new_index = new_index_by_old[&source_index];
    let root = &target_after.instances[root_new_index];
    let root_path = target_paths[root_new_index].as_ref();
    print_json_output(
        &json!({
            "ok": true,
            "sourceService": source_service,
            "targetService": target_service,
            "sourceSettingsFile": source_file,
            "targetSettingsFile": target_file,
            "settingsId": root.settings_id,
            "name": root.name,
            "className": root.class_name,
            "pathSegments": root_path.map(|path| &path.path_segments),
            "pathOrdinals": root_path.map(|path| &path.path_ordinals),
            "movedInstances": subtree.len(),
            "sourceStoreRemoved": source_store_removed,
            "changedPaths": changed_paths,
        }),
        true,
    )
}
