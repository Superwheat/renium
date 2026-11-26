use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ahash::AHashMap;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde_json::{Map, Value, json};

use crate::app::timing::log_timing;
use crate::bytecode::edit::{
    collect_settings_subtree_preorder, instance_path_parts_key, next_editor_settings_id_fast,
    path_ordinals_from_value, path_segments_from_value, prune_removed_source_dirs,
    reject_package_link_subtree_mutation,
};
use crate::bytecode::references::{
    materialize_for_moves, names_of_moved, names_row_of, read_referring_stores,
};
use crate::bytecode::{
    FolderSpellings, SourcePathsBefore, collect_source_path_updates, first_of_each_file,
    lock_existing_service_stores, preserve_source_path_extension, read_store_for_structure,
    release_settings_file_locks,
};
use crate::editor::document::is_protected_engine_container;
use crate::editor::paths::{
    build_editor_instance_paths_for_indices, build_editor_source_paths_by_index, script_file_names,
};
use crate::rbx::model::canonicalize_settings_references_for_move;
use crate::settings::bytecode::property_edit::RetainedPropertyColumns;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, visit_reference_objects_mut,
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

enum IndexedLocations {
    One(usize, usize, u32),
    Many(Vec<(usize, usize, u32)>),
}

impl IndexedLocations {
    fn push(&mut self, item: (usize, usize, u32)) {
        match self {
            Self::One(s, i, p) => {
                *self = Self::Many(vec![(*s, *i, *p), item]);
            }
            Self::Many(list) => list.push(item),
        }
    }

    fn as_slice<'a>(
        &'a self,
        single: &'a mut [(usize, usize, u32); 1],
    ) -> &'a [(usize, usize, u32)] {
        match self {
            Self::One(s, i, p) => {
                single[0] = (*s, *i, *p);
                &single[..]
            }
            Self::Many(list) => list.as_slice(),
        }
    }
}

fn index_document_locations<'a>(
    docs: &[(&'a str, &'a SettingsBytecode)],
    intern: &mut AHashMap<(u32, &'a str, usize), u32>,
    next_id: &mut u32,
) -> AHashMap<&'a str, IndexedLocations> {
    let total: usize = docs.iter().map(|(_, doc)| doc.instances.len()).sum();
    let mut locations = AHashMap::<&'a str, IndexedLocations>::with_capacity(total);
    let mut name_counts = AHashMap::<&'a str, usize>::new();
    let mut stack = Vec::<(usize, u32)>::new();
    for (doc_index, &(service, document)) in docs.iter().enumerate() {
        let count = document.instances.len();
        let Some(root_index) = editor_service_root_index(document, service) else {
            continue;
        };
        let mut head = vec![usize::MAX; count];
        let mut next = vec![usize::MAX; count];
        for index in (0..count).rev() {
            if let Some(parent) = document.instances[index].parent_index
                && parent < count
            {
                next[index] = head[parent];
                head[parent] = index;
            }
        }
        let mut visited = vec![false; count];
        let root_name = document.instances[root_index].name.as_str();
        let root_path_id = *intern.entry((0, root_name, 1)).or_insert_with(|| {
            let id = *next_id;
            *next_id += 1;
            id
        });
        visited[root_index] = true;
        stack.clear();
        stack.push((root_index, root_path_id));
        while let Some((index, path_id)) = stack.pop() {
            let id = document.instances[index].settings_id.as_str();
            let entry = (doc_index, index, path_id);
            locations
                .entry(id)
                .and_modify(|existing| existing.push(entry))
                .or_insert(IndexedLocations::One(doc_index, index, path_id));
            let first_child = head[index];
            if first_child == usize::MAX {
                continue;
            }
            let single_child = next[first_child] == usize::MAX;
            if !single_child {
                name_counts.clear();
            }
            let mut child = first_child;
            while child != usize::MAX {
                if !visited[child] {
                    visited[child] = true;
                    let name = document.instances[child].name.as_str();
                    let ordinal = if single_child {
                        1
                    } else {
                        let count = name_counts.entry(name).or_insert(0);
                        *count += 1;
                        *count
                    };
                    let child_path_id =
                        *intern.entry((path_id, name, ordinal)).or_insert_with(|| {
                            let id = *next_id;
                            *next_id += 1;
                            id
                        });
                    stack.push((child, child_path_id));
                }
                child = next[child];
            }
        }
    }
    locations
}

pub(crate) fn moved_references_between_documents(
    before: &BTreeMap<String, SettingsBytecode>,
    after: &BTreeMap<String, SettingsBytecode>,
) -> HashMap<String, MovedReference> {
    let _span = crate::app::profile::span("moved references between documents");
    let before_docs: Vec<(&str, &SettingsBytecode)> = before
        .iter()
        .map(|(service, doc)| (service.as_str(), doc))
        .collect();
    let after_docs: Vec<(&str, &SettingsBytecode)> = after
        .iter()
        .map(|(service, doc)| (service.as_str(), doc))
        .collect();
    let total_nodes: usize = before_docs
        .iter()
        .map(|(_, doc)| doc.instances.len())
        .sum::<usize>()
        .max(after_docs.iter().map(|(_, doc)| doc.instances.len()).sum());
    let mut intern = AHashMap::<(u32, &str, usize), u32>::with_capacity(total_nodes);
    let mut next_id = 1_u32;
    let before_locs = index_document_locations(&before_docs, &mut intern, &mut next_id);
    let after_locs = index_document_locations(&after_docs, &mut intern, &mut next_id);
    drop(intern);

    let mut moved_pairs = Vec::<(usize, usize, usize, usize)>::new();
    let mut old_single = [(0, 0, 0)];
    let mut new_single = [(0, 0, 0)];
    for (settings_id, old_entry) in &before_locs {
        let Some(new_entry) = after_locs.get(settings_id) else {
            continue;
        };
        if let (IndexedLocations::One(bs, bi, bp), IndexedLocations::One(as_, ai, ap)) =
            (old_entry, new_entry)
        {
            if bp != ap
                && before_docs[*bs].1.instances[*bi].class_name
                    == after_docs[*as_].1.instances[*ai].class_name
            {
                moved_pairs.push((*bs, *bi, *as_, *ai));
            }
            continue;
        }
        let old_slice = old_entry.as_slice(&mut old_single);
        let new_slice = new_entry.as_slice(&mut new_single);
        let removed: Vec<_> = old_slice
            .iter()
            .copied()
            .filter(|(_, _, bp)| !new_slice.iter().any(|(_, _, ap)| ap == bp))
            .collect();
        let added: Vec<_> = new_slice
            .iter()
            .copied()
            .filter(|(_, _, ap)| !old_slice.iter().any(|(_, _, bp)| bp == ap))
            .collect();
        if let ([(bs, bi, _)], [(as_, ai, _)]) = (removed.as_slice(), added.as_slice())
            && before_docs[*bs].1.instances[*bi].class_name
                == after_docs[*as_].1.instances[*ai].class_name
        {
            moved_pairs.push((*bs, *bi, *as_, *ai));
        }
    }
    if moved_pairs.is_empty() {
        return HashMap::new();
    }

    let mut before_indices: Vec<Vec<usize>> = vec![Vec::new(); before_docs.len()];
    let mut after_indices: Vec<Vec<usize>> = vec![Vec::new(); after_docs.len()];
    for &(bs, bi, as_, ai) in &moved_pairs {
        before_indices[bs].push(bi);
        after_indices[as_].push(ai);
    }
    let before_paths: Vec<HashMap<usize, crate::editor::types::EditorInstancePath>> = before_docs
        .iter()
        .zip(before_indices)
        .map(|((service, doc), indices)| {
            if indices.is_empty() {
                HashMap::new()
            } else {
                build_editor_instance_paths_for_indices(doc, service, &indices)
            }
        })
        .collect();
    let after_paths: Vec<HashMap<usize, crate::editor::types::EditorInstancePath>> = after_docs
        .iter()
        .zip(after_indices)
        .map(|((service, doc), indices)| {
            if indices.is_empty() {
                HashMap::new()
            } else {
                build_editor_instance_paths_for_indices(doc, service, &indices)
            }
        })
        .collect();

    let mut moved = HashMap::with_capacity(moved_pairs.len());
    for (bs, bi, as_, ai) in moved_pairs {
        let (Some(old_path), Some(new_path)) =
            (before_paths[bs].get(&bi), after_paths[as_].get(&ai))
        else {
            continue;
        };
        let old_key = instance_path_parts_key(&old_path.path_segments, &old_path.path_ordinals);
        moved.insert(
            old_key,
            MovedReference {
                settings_id: after_docs[as_].1.instances[ai].settings_id.clone(),
                path_segments: new_path.path_segments.clone(),
                path_ordinals: new_path.path_ordinals.clone(),
            },
        );
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

/// The documents of a move between services by service: `source` and
/// `target` as given, each other store that may refer to a moved instance
/// read by its instances, with what writes it, and any other by its
/// instances alone. `names` are the strings a reference to a moved instance
/// holds.
fn load_move_reference_documents(
    files: &BTreeMap<String, PathBuf>,
    (source_service, source): (&str, SettingsBytecode),
    (target_service, target): (&str, SettingsBytecode),
    moved_indices: &HashSet<usize>,
    old_paths: &HashMap<usize, crate::editor::types::EditorInstancePath>,
    names: &HashSet<String>,
) -> Result<(
    BTreeMap<String, SettingsBytecode>,
    BTreeMap<String, RetainedPropertyColumns>,
)> {
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
        .filter_map(|index| old_paths.get(index))
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

    let mut stores = BTreeMap::new();
    for (service, path) in files {
        if service.as_str() == source_service || service.as_str() == target_service {
            continue;
        }
        let read = read_referring_stores(std::iter::once(path), names, &needles)?;
        if let Some(store) = read.into_values().next() {
            documents.insert(service.clone(), store.document);
            stores.insert(service.clone(), store.stored);
        }
    }
    Ok((documents, stores))
}

pub(crate) fn move_instance_between_service_stores(
    source_file: &Path,
    source_service: &str,
    source_settings_id: &str,
    target_file: &Path,
    target_service: &str,
    target_parent_settings_id: &str,
) -> Result<Value> {
    let move_started = Instant::now();
    let src_root = source_root_for_stores(source_file, target_file)?;
    let mut files = service_store_paths(&src_root)?;
    let named = [source_service, target_service].map(|service| files.get(service).cloned());
    let listed = named.iter().flatten().cloned();
    let kept = first_of_each_file(listed.chain(files.values().cloned()));
    // Two services whose stores the disk takes for one file are one store.
    if let [Some(_), Some(target)] = &named
        && source_service != target_service
        && !kept.contains(target)
    {
        bail!(
            "{source_service} and {target_service} are one store on this disk ({}); nothing was moved",
            target.display()
        );
    }
    files.retain(|_, path| kept.contains(path));
    let locks = lock_existing_service_stores(files.values().map(PathBuf::as_path))?;
    let load_started = Instant::now();
    let (source_before, target_before) = rayon::join(
        || read_store_for_structure(source_file),
        || read_store_for_structure(target_file),
    );
    let (mut source_before, mut source_stored) = source_before?;
    let (mut target_before, mut target_stored) = target_before?;
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
    let old_paths =
        build_editor_instance_paths_for_indices(&source_before, source_service, &subtree);
    let names = names_of_moved((source_file, source_service), &source_before, &subtree);
    let source_read = (&mut source_before, &mut source_stored);
    materialize_for_moves(source_read, &subtree, &names)?;
    // A reference by position to a moved instance takes its path before
    // the instance leaves the store.
    let to_moved = |value: &Value| names_row_of(value, &moved_indices);
    source_stored.materialize_where(&mut source_before, None, to_moved)?;
    materialize_for_moves((&mut target_before, &mut target_stored), &[], &names)?;
    let (mut documents, other_stores) = load_move_reference_documents(
        &files,
        (source_service, source_before.clone()),
        (target_service, target_before),
        &moved_indices,
        &old_paths,
        &names,
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

    let new_indices = (subtree.iter())
        .map(|old_index| new_index_by_old[old_index])
        .collect::<Vec<_>>();
    let target_paths = build_editor_instance_paths_for_indices(
        &documents[target_service],
        target_service,
        &new_indices,
    );
    let moved_references = subtree
        .iter()
        .filter_map(|old_index| {
            let old_path = old_paths.get(old_index)?;
            let new_index = new_index_by_old[old_index];
            let new_path = target_paths.get(&new_index)?;
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
        &SourcePathsBefore::capture(&source_before, &source_paths_before),
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
        } else if service == source_service {
            writes.insert(path.clone(), source_stored.encode_restructured(document)?);
        } else if service == target_service {
            writes.insert(path.clone(), target_stored.encode_restructured(document)?);
        } else {
            let stored = (other_stores.get(service))
                .context("A store read without its values is not written")?;
            writes.insert(path.clone(), stored.encode(document)?);
        }
    }
    let written = writes.keys().map(|write| exact_path_key(write));
    let written = written.collect::<HashSet<_>>();
    removals.retain(|path| !written.contains(&exact_path_key(path)));
    removals.sort_by_key(|path| exact_path_key(path));
    removals.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    let folders = FolderSpellings::respell(&writes, &removals, &[]);
    let changed_paths = folders.changed_paths(&writes, &removals);
    folders.apply(&writes, &removals)?;
    prune_removed_source_dirs(&super::storage::source_directory(source_file), &removals);
    release_settings_file_locks(locks);

    let root_new_index = new_index_by_old[&source_index];
    let root = &target_after.instances[root_new_index];
    let root_path = target_paths.get(&root_new_index);
    let result = json!({
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
    });
    let read = (source_before, source_stored, target_stored, other_stores);
    rayon::spawn(move || drop((read, documents, source_paths_before)));
    log_timing("cross-service move total", move_started);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::bytecode::{SETTINGS_BINARY_VERSION, SettingsBytecodeInstance};

    fn make_doc(instances: Vec<SettingsBytecodeInstance>) -> SettingsBytecode {
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    }

    fn inst(id: &str, name: &str, class: &str, parent: Option<usize>) -> SettingsBytecodeInstance {
        SettingsBytecodeInstance::new(id.into(), name.into(), class.into(), parent)
    }

    #[test]
    fn moved_references_detects_rename_reparent_ordinal_shift_and_cross_service_move() {
        let mut before = BTreeMap::new();
        before.insert(
            "Workspace".to_string(),
            make_doc(vec![
                inst("ws", "Workspace", "Workspace", None),
                inst("a", "FolderA", "Folder", Some(0)),
                inst("b", "FolderB", "Folder", Some(0)),
                inst("same1", "Dup", "Part", Some(1)),
                inst("same2", "Dup", "Part", Some(1)),
                inst("xmove", "Cross", "Part", Some(1)),
                inst("cls", "Morph", "Part", Some(1)),
                inst("dup_id", "Clone", "Part", Some(1)),
                inst("dup_id", "Clone", "Part", Some(2)),
            ]),
        );
        before.insert(
            "ServerStorage".to_string(),
            make_doc(vec![inst("ss", "ServerStorage", "ServerStorage", None)]),
        );

        let mut after = BTreeMap::new();
        after.insert(
            "Workspace".to_string(),
            make_doc(vec![
                inst("ws", "Workspace", "Workspace", None),
                inst("a", "FolderA", "Folder", Some(0)),
                inst("b", "FolderB", "Folder", Some(0)),
                // same1 removed so same2 shifts from ordinal 2 to ordinal 1
                inst("same2", "Dup", "Part", Some(1)),
                // cls changed class, must not be reported as moved
                inst("cls", "MorphRenamed", "Model", Some(1)),
                // one of two duplicate settings_ids moved
                inst("dup_id", "Clone", "Part", Some(1)),
                inst("dup_id", "CloneMoved", "Part", Some(2)),
            ]),
        );
        after.insert(
            "ServerStorage".to_string(),
            make_doc(vec![
                inst("ss", "ServerStorage", "ServerStorage", None),
                inst("xmove", "Cross", "Part", Some(0)),
            ]),
        );

        let moved = moved_references_between_documents(&before, &after);
        assert_eq!(moved.len(), 3);

        let ord_key = instance_path_parts_key(
            &["Workspace".into(), "FolderA".into(), "Dup".into()],
            &[1, 1, 2],
        );
        assert_eq!(moved[&ord_key].settings_id, "same2");
        assert_eq!(moved[&ord_key].path_ordinals, vec![1, 1, 1]);

        let cross_key = instance_path_parts_key(
            &["Workspace".into(), "FolderA".into(), "Cross".into()],
            &[1, 1, 1],
        );
        assert_eq!(moved[&cross_key].settings_id, "xmove");
        assert_eq!(
            moved[&cross_key].path_segments,
            vec!["ServerStorage", "Cross"]
        );

        let dup_key = instance_path_parts_key(
            &["Workspace".into(), "FolderB".into(), "Clone".into()],
            &[1, 1, 1],
        );
        assert_eq!(moved[&dup_key].settings_id, "dup_id");
        assert_eq!(
            moved[&dup_key].path_segments,
            vec!["Workspace", "FolderB", "CloneMoved"]
        );
    }
}
