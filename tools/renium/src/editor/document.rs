use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::editor::diff::{EditorSiblingGroupCounts, editor_instance_descriptor_from_path};
use crate::editor::paths::{editor_run_context_value, run_context_name};
use crate::editor::types::{EditorSourceEnsureResult, EditorSourcePathSpec, EditorSourceTarget};
use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};
use crate::settings::instance::{self as instance_api, AddInstanceSpec};
use crate::settings::tree::{
    editor_child_stems, editor_service_root_index, settings_children_by_parent,
};
use crate::system::files::{
    normalized_child_stem_key, service_settings_path, validate_filesystem_instance_name,
};

pub(crate) fn read_editor_service_settings(
    src_root: &Path,
    service: &str,
) -> Result<Option<SettingsBytecode>> {
    validate_filesystem_instance_name(service, "service")?;
    let service_dir = src_root.join(service);
    let settings_path = service_settings_path(&service_dir);
    if !settings_path.exists() {
        return Ok(None);
    }
    SettingsBytecode::read_file(&settings_path).map(Some)
}

pub(crate) fn read_editor_service_settings_cached(
    src_root: &Path,
    service: &str,
) -> Result<Option<Arc<SettingsBytecode>>> {
    validate_filesystem_instance_name(service, "service")?;
    let settings_path = service_settings_path(&src_root.join(service));
    if !settings_path.exists() {
        return Ok(None);
    }
    SettingsBytecode::read_file_cached(&settings_path).map(Some)
}

pub(crate) struct EditorServiceDocument {
    pub service: String,
    pub document: SettingsBytecode,
}

pub(crate) fn read_editor_service_documents(src_root: &Path) -> Result<Vec<EditorServiceDocument>> {
    let mut documents = Vec::new();
    for service_dir in crate::project::storage::service_directories(src_root)? {
        let settings_file = service_settings_path(&service_dir);
        if settings_file.is_file() {
            documents.push(EditorServiceDocument {
                service: service_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                document: SettingsBytecode::read_file(&settings_file)?,
            });
        }
    }
    Ok(documents)
}

pub(crate) fn ensure_editor_source_target_in_bytecode(
    document: &mut SettingsBytecode,
    spec: &EditorSourcePathSpec,
) -> Result<EditorSourceEnsureResult> {
    let mut index = EditorDocumentIndex::new(document);
    ensure_editor_source_target_with_index(document, spec, &HashSet::new(), &mut index)
}

/// The children and settings ids of a document, kept up to date with the
/// instances an ensure adds, so that ensuring many sources reads the whole
/// document once. The ids are read when an ensure first adds an instance.
#[derive(Default)]
pub(crate) struct EditorDocumentIndex {
    pub(crate) children: Vec<Vec<usize>>,
    ids: Option<HashSet<String>>,
}

impl EditorDocumentIndex {
    pub(crate) fn new(document: &SettingsBytecode) -> Self {
        Self {
            children: settings_children_by_parent(document),
            ids: None,
        }
    }

    fn ids(&mut self, document: &SettingsBytecode) -> &HashSet<String> {
        self.ids.get_or_insert_with(|| {
            crate::settings::tree::count_whole_store_pass();
            (document.instances.iter())
                .map(|instance| instance.settings_id.clone())
                .collect()
        })
    }

    fn added(&mut self, document: &SettingsBytecode, index: usize) {
        let instance = &document.instances[index];
        self.children.resize_with(index + 1, Vec::new);
        if let Some(parent) = instance.parent_index
            && parent < index
        {
            self.children[parent].push(index);
        }
        if let Some(ids) = &mut self.ids {
            ids.insert(instance.settings_id.clone());
        }
    }

    fn child_by_stem(
        &self,
        document: &SettingsBytecode,
        parent: usize,
        stem: &str,
    ) -> Option<usize> {
        let wanted = normalized_child_stem_key(stem);
        editor_child_stems(document, &self.children[parent])
            .into_iter()
            .find_map(|(index, child_stem, _)| {
                (normalized_child_stem_key(&child_stem) == wanted).then_some(index)
            })
    }

    fn name_ordinal(&self, document: &SettingsBytecode, parent: usize, child: usize) -> usize {
        let name = &document.instances[child].name;
        self.children[parent]
            .iter()
            .filter(|index| document.instances[**index].name == *name)
            .position(|index| *index == child)
            .map_or(1, |position| position + 1)
    }

    fn next_settings_id(
        &mut self,
        document: &SettingsBytecode,
        reserved: &HashSet<String>,
    ) -> String {
        let ids = self.ids(document);
        (document.instances.len()..usize::MAX)
            .map(|index| format!("editor:{index:x}"))
            .find(|candidate| !ids.contains(candidate) && !reserved.contains(candidate))
            .unwrap_or_else(|| "editor:new".to_string())
    }

    fn add(
        &mut self,
        document: &mut SettingsBytecode,
        name: String,
        class_name: String,
        parent: usize,
        reserved: &HashSet<String>,
    ) -> Result<usize> {
        let settings_id = self.next_settings_id(document, reserved);
        let ids = self.ids(document);
        let added = instance_api::add_instance_where(
            document,
            AddInstanceSpec::new(Some(settings_id), name, class_name, Some(parent)),
            |_, settings_id| ids.contains(settings_id),
        )?;
        self.added(document, added.index);
        Ok(added.index)
    }

    /// The instance `spec` names where the document already holds it.
    pub(crate) fn source_target(
        &self,
        document: &SettingsBytecode,
        spec: &EditorSourcePathSpec,
    ) -> Option<usize> {
        let mut index = editor_service_root_index(document, &spec.service)?;
        for component in &spec.parent_components {
            index = self.child_by_stem(document, index, component)?;
        }
        self.child_by_stem(document, index, &spec.instance_stem)
    }
}

/// `ensure_editor_source_target_in_bytecode` with the document's index, which
/// it keeps up to date. `reserved` names identities the same collection
/// already removed from `document` (a renamed directory deletes its scripts
/// before it creates the new ones); a created instance never takes one of
/// them.
pub(crate) fn ensure_editor_source_target_with_index(
    document: &mut SettingsBytecode,
    spec: &EditorSourcePathSpec,
    reserved: &HashSet<String>,
    index: &mut EditorDocumentIndex,
) -> Result<EditorSourceEnsureResult> {
    let mut changed = false;
    let mut upsert_instance_paths = Vec::new();
    let mut replace_instance_paths = Vec::new();
    let mut target_class_replaced = false;
    let mut target_previous_class_name = None;
    let root_index = if let Some(index) = editor_service_root_index(document, &spec.service) {
        index
    } else {
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            spec.service.clone(),
            spec.service.clone(),
            None,
        ));
        index.added(document, document.instances.len() - 1);
        changed = true;
        document.instances.len() - 1
    };

    let mut current_index = root_index;
    let mut path_segments = vec![document.instances[root_index].name.clone()];
    let mut path_ordinals = vec![1];
    for component in &spec.parent_components {
        if let Some(child_index) = index.child_by_stem(document, current_index, component) {
            let child_ordinal = index.name_ordinal(document, current_index, child_index);
            current_index = child_index;
            path_segments.push(document.instances[current_index].name.clone());
            path_ordinals.push(child_ordinal);
            upsert_instance_paths.push((
                current_index,
                path_segments.clone(),
                path_ordinals.clone(),
                true,
            ));
            continue;
        }

        let component_class =
            editor_container_class_for_component(&spec.service, &path_segments, component);
        let parent_index = current_index;
        current_index = index.add(
            document,
            component.clone(),
            component_class.to_string(),
            parent_index,
            reserved,
        )?;
        path_segments.push(component.clone());
        path_ordinals.push(index.name_ordinal(document, parent_index, current_index));
        changed = true;

        upsert_instance_paths.push((
            current_index,
            path_segments.clone(),
            path_ordinals.clone(),
            false,
        ));
    }

    let target_index = if let Some(child_index) =
        index.child_by_stem(document, current_index, &spec.instance_stem)
    {
        if document.instances[child_index].class_name != spec.class_name {
            target_previous_class_name = Some(document.instances[child_index].class_name.clone());
            document.instances[child_index]
                .class_name
                .clone_from(&spec.class_name);
            changed = true;
            target_class_replaced = true;
        }
        child_index
    } else {
        changed = true;
        index.add(
            document,
            spec.instance_name.clone(),
            spec.class_name.clone(),
            current_index,
            reserved,
        )?
    };

    if document.instances[target_index].class_name == "Script" {
        if let Some(run_context) = spec.run_context.as_ref()
            && document.instances[target_index]
                .properties
                .get("RunContext")
                .and_then(run_context_name)
                .is_none_or(|value| !value.eq_ignore_ascii_case(run_context))
        {
            document.instances[target_index].properties.insert(
                "RunContext".to_string(),
                editor_run_context_value(run_context),
            );
            changed = true;
        }
    } else if document.instances[target_index]
        .properties
        .remove("RunContext")
        .is_some()
    {
        changed = true;
    }

    let target_ordinal = index.name_ordinal(document, current_index, target_index);
    path_segments.push(document.instances[target_index].name.clone());
    path_ordinals.push(target_ordinal);
    let target = EditorSourceTarget {
        service: spec.service.clone(),
        settings_id: Some(document.instances[target_index].settings_id.clone()),
        path_segments: path_segments.clone(),
        path_ordinals: path_ordinals.clone(),
        class_name: document.instances[target_index].class_name.clone(),
    };
    if target_class_replaced {
        replace_instance_paths.push((target_index, path_segments.clone(), path_ordinals.clone()));
    } else {
        upsert_instance_paths.push((
            target_index,
            path_segments.clone(),
            path_ordinals.clone(),
            false,
        ));
    }
    let document: &SettingsBytecode = document;
    let mut sibling_counts = EditorSiblingGroupCounts::new();
    for &instance in upsert_instance_paths
        .iter()
        .map(|(instance, ..)| instance)
        .chain(replace_instance_paths.iter().map(|(instance, ..)| instance))
    {
        let name = document.instances[instance].name.as_str();
        if let Some(parent) = document.instances[instance].parent_index {
            sibling_counts.entry((parent, name)).or_insert_with(|| {
                index.children[parent]
                    .iter()
                    .copied()
                    .filter(|sibling| document.instances[*sibling].name == name)
                    .collect()
            });
        }
    }
    let upsert_instances = upsert_instance_paths
        .into_iter()
        .map(|(index, segments, ordinals, anchor_only)| {
            let mut descriptor = editor_instance_descriptor_from_path(
                document,
                index,
                segments,
                ordinals,
                &sibling_counts,
            )
            .context("Failed to describe a source upsert")?;
            descriptor.anchor_only = anchor_only;
            Ok(descriptor)
        })
        .collect::<Result<Vec<_>>>()?;
    let replace_instances = replace_instance_paths
        .into_iter()
        .map(|(index, segments, ordinals)| {
            let mut descriptor = editor_instance_descriptor_from_path(
                document,
                index,
                segments.clone(),
                ordinals.clone(),
                &sibling_counts,
            )
            .context("Failed to describe a source replacement")?;
            descriptor.previous_path_segments = segments;
            descriptor.previous_path_ordinals = ordinals;
            descriptor.previous_class_name = target_previous_class_name.clone();
            Ok(descriptor)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(EditorSourceEnsureResult {
        target,
        upsert_instances,
        replace_instances,
        changed,
    })
}

fn editor_container_class_for_component(
    service: &str,
    parent_segments: &[String],
    component: &str,
) -> &'static str {
    if service == "StarterPlayer" && parent_segments.len() == 1 {
        match component {
            "StarterCharacterScripts" => return "StarterCharacterScripts",
            "StarterPlayerScripts" => return "StarterPlayerScripts",
            _ => {}
        }
    }
    "Folder"
}

pub(crate) fn is_protected_engine_container(document: &SettingsBytecode, index: usize) -> bool {
    let Some(instance) = document.instances.get(index) else {
        return false;
    };
    if instance.name != instance.class_name {
        return false;
    }
    let Some(parent_index) = instance.parent_index else {
        return false;
    };
    document.instances.get(parent_index).is_some_and(|parent| {
        parent.parent_index.is_none()
            && parent.name == parent.class_name
            && crate::roblox::services::is_engine_managed_container(
                &parent.class_name,
                &instance.class_name,
            )
    })
}

pub(crate) fn editor_child_by_stem(
    document: &SettingsBytecode,
    parent_index: usize,
    stem: &str,
) -> Option<usize> {
    crate::settings::tree::count_whole_store_pass();
    let child_indices = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            (instance.parent_index == Some(parent_index)).then_some(index)
        })
        .collect::<Vec<_>>();
    let wanted = normalized_child_stem_key(stem);
    editor_child_stems(document, &child_indices)
        .into_iter()
        .find_map(|(index, child_stem, _)| {
            (normalized_child_stem_key(&child_stem) == wanted).then_some(index)
        })
}

pub(crate) fn document_instance_index_by_settings_id(
    document: &SettingsBytecode,
    settings_id: &str,
) -> Option<usize> {
    crate::settings::tree::count_whole_store_pass();
    document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::{SimRng, settings_document, settings_instance};

    /// The ordinal among same-named siblings as it was counted over the
    /// whole document.
    fn scanned_name_ordinal(document: &SettingsBytecode, parent: usize, child: usize) -> usize {
        let mut ordinal = 0;
        for (index, instance) in document.instances.iter().enumerate() {
            if instance.parent_index == Some(parent)
                && instance.name == document.instances[child].name
            {
                ordinal += 1;
                if index == child {
                    return ordinal;
                }
            }
        }
        1
    }

    #[test]
    fn the_index_finds_children_as_the_whole_document_scans_did() {
        const NAMES: [&str; 6] = ["A", "a", "B", "init", "A b", "A_b"];
        crate::tests::support::run_seeds(64, |seed, _| {
            let mut rng = SimRng(seed);
            let mut instances = vec![settings_instance("root", "Workspace", "Workspace", None)];
            for at in 1..2 + rng.below(60) {
                let parent = rng.below(at);
                let name = NAMES[rng.below(NAMES.len())];
                instances.push(settings_instance(
                    format!("i{at}"),
                    name,
                    "Folder",
                    Some(parent),
                ));
            }
            let mut document = settings_document(instances);
            let mut index = EditorDocumentIndex::new(&document);
            for _ in 0..rng.below(8) {
                let parent = rng.below(document.instances.len());
                let name = NAMES[rng.below(NAMES.len())].to_string();
                index
                    .add(
                        &mut document,
                        name,
                        "Folder".to_string(),
                        parent,
                        &HashSet::new(),
                    )
                    .map_err(|error| error.to_string())?;
            }
            for parent in 0..document.instances.len() {
                for stem in NAMES.iter().chain(&["A_2", "a_3", "init_2", "A_b_2"]) {
                    let found = index.child_by_stem(&document, parent, stem);
                    if found != editor_child_by_stem(&document, parent, stem) {
                        return Err(format!("stem {stem} under {parent}"));
                    }
                }
                for &child in &index.children[parent] {
                    if index.name_ordinal(&document, parent, child)
                        != scanned_name_ordinal(&document, parent, child)
                    {
                        return Err(format!("ordinal of {child}"));
                    }
                }
            }
            Ok(())
        });
    }
}
