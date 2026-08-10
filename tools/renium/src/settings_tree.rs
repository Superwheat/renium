use std::collections::{HashMap, HashSet};

use crate::editor_types::EditorInstancePath;
use crate::file_io::unique_child_stem;
use crate::settings_bytecode::SettingsBytecode;

pub(super) fn assign_editor_instance_paths(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    index: usize,
    path_segments: &mut Vec<String>,
    path_ordinals: &mut Vec<usize>,
    out: &mut [Option<EditorInstancePath>],
) {
    if let Some(slot) = out.get_mut(index) {
        *slot = Some(EditorInstancePath {
            path_segments: path_segments.clone(),
            path_ordinals: path_ordinals.clone(),
        });
    }
    let mut seen_names: HashMap<String, usize> = HashMap::new();
    for child_index in children_by_parent
        .get(index)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let child = &document.instances[*child_index];
        let child_ordinal = seen_names
            .entry(child.name.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        path_segments.push(child.name.clone());
        path_ordinals.push(*child_ordinal);
        assign_editor_instance_paths(
            document,
            children_by_parent,
            *child_index,
            path_segments,
            path_ordinals,
            out,
        );
        path_segments.pop();
        path_ordinals.pop();
    }
}

pub(super) fn settings_children_by_parent(document: &SettingsBytecode) -> Vec<Vec<usize>> {
    let mut children = vec![Vec::new(); document.instances.len()];
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent_index) = instance.parent_index
            && let Some(bucket) = children.get_mut(parent_index)
        {
            bucket.push(index);
        }
    }
    children
}

pub(super) fn editor_service_root_index(
    document: &SettingsBytecode,
    service: &str,
) -> Option<usize> {
    document
        .instances
        .iter()
        .enumerate()
        .find_map(|(index, instance)| {
            (instance.parent_index.is_none() && instance.name == service).then_some(index)
        })
        .or_else(|| {
            document
                .instances
                .iter()
                .position(|instance| instance.parent_index.is_none())
        })
}

pub(super) fn editor_child_stems(
    document: &SettingsBytecode,
    child_indices: &[usize],
) -> Vec<(usize, String, usize)> {
    let mut name_counters: HashMap<&str, usize> = HashMap::new();
    let mut used_stem_keys = HashSet::new();
    let mut next_suffix_by_base = HashMap::new();
    let mut named_children = Vec::with_capacity(child_indices.len());
    for child_index in child_indices {
        let child = &document.instances[*child_index];
        let count = name_counters.entry(&child.name).or_insert(0);
        *count += 1;
        let child_stem =
            unique_child_stem(&child.name, &mut used_stem_keys, &mut next_suffix_by_base);
        named_children.push((*child_index, child_stem, *count));
    }
    named_children
}
