use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::app::timing::log_timing;
use crate::bytecode::edit::{
    collect_settings_subtree_preorder, instance_path_parts_key, next_editor_settings_id_fast,
    path_ordinals_from_value, path_segments_from_value, prune_removed_source_dirs,
    reject_package_link_subtree_mutation,
};
use crate::bytecode::{
    apply_file_mutations, collect_source_path_updates, file_mutation_paths,
    lock_existing_service_store, preserve_source_path_extension,
};
use crate::editor::document::is_protected_engine_container;
use crate::editor::paths::{
    build_editor_instance_paths, build_editor_source_paths_by_index, script_file_names,
};
use crate::rbx::model::canonicalize_settings_references_for_move;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, encode_settings_bytecode,
    visit_reference_objects_mut,
};
use crate::settings::instance;
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::system::files::{exact_path_key, service_settings_path};

pub(crate) struct MovedReference {
    pub(crate) settings_id: String,
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
}

#[derive(PartialEq)]
struct PackageLinkState {
    settings_id: String,
    name: String,
    parent_settings_id: Option<String>,
    properties: Map<String, Value>,
    attributes: Map<String, Value>,
}

pub(crate) fn service_store_paths(src_root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let source = super::storage::source_for_instances(src_root);
    let src_root = source.as_deref().unwrap_or(src_root);
    let mut files = BTreeMap::new();
    for service_dir in super::storage::service_directories(src_root)? {
        let settings_file = service_settings_path(&service_dir);
        if settings_file.is_file() {
            files.insert(
                service_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                settings_file,
            );
        }
    }
    Ok(files)
}

fn package_link_states(document: &SettingsBytecode) -> Vec<PackageLinkState> {
    let mut states = document
        .instances
        .iter()
        .filter(|instance| instance.class_name == "PackageLink")
        .map(|instance| PackageLinkState {
            settings_id: instance.settings_id.clone(),
            name: instance.name.clone(),
            parent_settings_id: instance
                .parent_index
                .and_then(|index| document.instances.get(index))
                .map(|parent| parent.settings_id.clone()),
            properties: instance.properties.clone(),
            attributes: instance.attributes.clone(),
        })
        .collect::<Vec<_>>();
    states.sort_by(|left, right| left.settings_id.cmp(&right.settings_id));
    states
}

pub(crate) fn rewrite_moved_references(
    record: &mut Map<String, Value>,
    moved: &HashMap<String, MovedReference>,
) -> bool {
    let mut changed = false;
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
        changed = true;
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
    changed
}

struct ReferenceLocation {
    path_key: String,
    class_name: String,
    target: MovedReference,
}

fn reference_locations_by_id(
    documents: &BTreeMap<String, SettingsBytecode>,
) -> HashMap<String, Vec<ReferenceLocation>> {
    let mut locations = HashMap::<String, Vec<ReferenceLocation>>::new();
    for (service, document) in documents {
        let paths = build_editor_instance_paths(document, service);
        for (instance, path) in document.instances.iter().zip(paths) {
            let Some(path) = path else {
                continue;
            };
            locations
                .entry(instance.settings_id.clone())
                .or_default()
                .push(ReferenceLocation {
                    path_key: instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                    class_name: instance.class_name.clone(),
                    target: MovedReference {
                        settings_id: instance.settings_id.clone(),
                        path_segments: path.path_segments,
                        path_ordinals: path.path_ordinals,
                    },
                });
        }
    }
    locations
}

pub(crate) fn moved_references_between_documents(
    before: &BTreeMap<String, SettingsBytecode>,
    after: &BTreeMap<String, SettingsBytecode>,
) -> HashMap<String, MovedReference> {
    let before = reference_locations_by_id(before);
    let after = reference_locations_by_id(after);
    let mut moved = HashMap::new();
    for (settings_id, old_locations) in before {
        let Some(new_locations) = after.get(&settings_id) else {
            continue;
        };
        let removed = old_locations
            .iter()
            .filter(|old| !new_locations.iter().any(|new| new.path_key == old.path_key))
            .collect::<Vec<_>>();
        let added = new_locations
            .iter()
            .filter(|new| !old_locations.iter().any(|old| old.path_key == new.path_key))
            .collect::<Vec<_>>();
        if let ([old], [new]) = (removed.as_slice(), added.as_slice())
            && old.class_name == new.class_name
        {
            moved.insert(
                old.path_key.clone(),
                MovedReference {
                    settings_id: new.target.settings_id.clone(),
                    path_segments: new.target.path_segments.clone(),
                    path_ordinals: new.target.path_ordinals.clone(),
                },
            );
        }
    }
    moved
}

fn source_root_for_stores(source_file: &Path, target_file: &Path) -> Result<PathBuf> {
    let source_directory = super::storage::source_directory(source_file);
    let target_directory = super::storage::source_directory(target_file);
    let source_root = source_directory
        .parent()
        .context("Source settings file is not inside a service directory")?;
    let target_root = target_directory
        .parent()
        .context("Target settings file is not inside a service directory")?;
    if exact_path_key(source_root) != exact_path_key(target_root) {
        bail!("Cross-service moves require both services to use the same source root");
    }
    Ok(source_root.to_path_buf())
}

fn load_move_reference_documents(
    files: &BTreeMap<String, PathBuf>,
    source_service: &str,
    target_service: &str,
    source: SettingsBytecode,
    target: SettingsBytecode,
    moved_indices: &HashSet<usize>,
    old_paths: &[Option<crate::editor::types::EditorInstancePath>],
) -> Result<BTreeMap<String, SettingsBytecode>> {
    let structures = files
        .par_iter()
        .filter(|(service, _)| {
            service.as_str() != source_service && service.as_str() != target_service
        })
        .map(|(service, path)| {
            SettingsBytecode::read_structure_file(path).map(|document| (service.clone(), document))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut documents = structures.into_iter().collect::<BTreeMap<_, _>>();
    documents.insert(source_service.to_string(), source);
    documents.insert(target_service.to_string(), target);

    let moved_ids = moved_indices
        .iter()
        .map(|index| {
            documents[source_service].instances[*index]
                .settings_id
                .clone()
        })
        .collect::<HashSet<_>>();
    let mut moved_id_counts = moved_ids
        .iter()
        .map(|id| (id.clone(), 0usize))
        .collect::<HashMap<_, _>>();
    for document in documents.values() {
        for instance in &document.instances {
            if let Some(count) = moved_id_counts.get_mut(&instance.settings_id) {
                *count += 1;
            }
        }
    }
    let mut needles = moved_indices
        .iter()
        .filter_map(|index| old_paths.get(*index)?.as_ref())
        .map(|path| path.path_segments.clone())
        .collect::<Vec<_>>();
    for (settings_id, count) in moved_id_counts {
        if count != 1 {
            continue;
        }
        needles.push(vec![settings_id.clone()]);
        if let Some(debug_id) = settings_id.strip_prefix("debug:") {
            needles.push(vec![debug_id.to_string()]);
        }
    }
    needles.sort();
    needles.dedup();

    let candidates = files
        .par_iter()
        .filter(|(service, _)| {
            service.as_str() != source_service && service.as_str() != target_service
        })
        .map(|(service, path)| {
            SettingsBytecode::read_file_if_contains_any_string_set(path, &needles)
                .map(|document| document.map(|document| (service.clone(), document)))
        })
        .collect::<Result<Vec<_>>>()?;
    for (service, document) in candidates.into_iter().flatten() {
        documents.insert(service, document);
    }
    Ok(documents)
}

pub(crate) fn move_instance_between_service_stores(
    source_file: &Path,
    source_service: &str,
    source_settings_id: &str,
    target_file: &Path,
    target_service: &str,
    target_parent_settings_id: &str,
) -> Result<()> {
    let move_started = Instant::now();
    let src_root = source_root_for_stores(source_file, target_file)?;
    let files = service_store_paths(&src_root)?;
    let lock_paths = files.values().cloned().collect::<BTreeSet<_>>();
    let _locks = lock_paths
        .iter()
        .map(|path| lock_existing_service_store(path))
        .collect::<Result<Vec<_>>>()?;
    let load_started = Instant::now();
    let (source_before, target_before) = rayon::join(
        || SettingsBytecode::read_file(source_file),
        || SettingsBytecode::read_file(target_file),
    );
    let source_before = source_before?;
    let target_before = target_before?;
    let source_index = source_before
        .instances
        .iter()
        .position(|instance| instance.settings_id == source_settings_id)
        .with_context(|| {
            format!("Source service '{source_service}' has no instance id '{source_settings_id}'")
        })?;
    if source_before.instances[source_index].parent_index.is_none() {
        bail!("Service roots cannot be moved");
    }
    if is_protected_engine_container(&source_before, source_index) {
        bail!(
            "{} cannot be moved",
            source_before.instances[source_index].name
        );
    }
    let target_parent_index = target_before
        .instances
        .iter()
        .position(|instance| instance.settings_id == target_parent_settings_id)
        .with_context(|| {
            format!(
                "Target service '{target_service}' has no instance id '{target_parent_settings_id}'"
            )
        })?;

    let children = settings_children_by_parent(&source_before);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children, source_index, &mut subtree);
    reject_package_link_subtree_mutation(&source_before, &subtree, "moved between services")?;
    let moved_indices = subtree.iter().copied().collect::<HashSet<_>>();
    let old_paths = build_editor_instance_paths(&source_before, source_service);
    let mut documents = load_move_reference_documents(
        &files,
        source_service,
        target_service,
        source_before.clone(),
        target_before,
        &moved_indices,
        &old_paths,
    )?;
    log_timing("cross-service move load", load_started);
    let original_package_links = documents
        .iter()
        .map(|(service, document)| (service.clone(), package_link_states(document)))
        .collect::<BTreeMap<_, _>>();
    let canonicalize_started = Instant::now();
    let mut changed_services =
        canonicalize_settings_references_for_move(&mut documents, source_service, &moved_indices);
    log_timing(
        "cross-service move reference canonicalization",
        canonicalize_started,
    );
    changed_services.extend([source_service.to_string(), target_service.to_string()]);

    let mut source = documents
        .remove(source_service)
        .with_context(|| format!("Source service '{source_service}' has no Renium store"))?;
    let mut target = documents
        .remove(target_service)
        .with_context(|| format!("Target service '{target_service}' has no Renium store"))?;
    let source_paths_before = build_editor_source_paths_by_index(
        &source_before,
        source_service,
        &super::storage::source_directory(source_file),
    );
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
    for (service, document) in &mut documents {
        for instance in &mut document.instances {
            let changed = rewrite_moved_references(&mut instance.properties, &moved_references)
                | rewrite_moved_references(&mut instance.attributes, &moved_references);
            if changed {
                if instance.class_name == "PackageLink" {
                    bail!(
                        "A PackageLink refers to the moved instance and cannot be edited directly"
                    );
                }
                changed_services.insert(service.clone());
            }
        }
    }
    for (service, document) in &documents {
        if original_package_links.get(service) != Some(&package_link_states(document)) {
            bail!("Cross-service move would edit a PackageLink");
        }
    }

    let source_after = &documents[source_service];
    let target_after = &documents[target_service];
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    collect_source_path_updates(
        &source_before,
        &source_paths_before,
        source_after,
        source_service,
        &super::storage::source_directory(source_file),
        &mut writes,
        &mut removals,
    )?;
    let mut target_source_paths = build_editor_source_paths_by_index(
        target_after,
        target_service,
        &super::storage::source_directory(target_file),
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
    for service in &changed_services {
        let document = &documents[service];
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
    prune_removed_source_dirs(&super::storage::source_directory(source_file), &removals);

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
    )?;
    log_timing("cross-service move total", move_started);
    Ok(())
}
