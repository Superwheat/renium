use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Map;

use super::editor_diff::{editor_instance_descriptor_from_path, editor_sibling_group_counts};
use super::editor_paths::{editor_run_context_value, run_context_name};
use super::editor_types::{EditorSourceEnsureResult, EditorSourcePathSpec, EditorSourceTarget};
use super::file_io::{service_settings_path, validate_filesystem_instance_name};
use super::instance_api::{self, AddInstanceSpec};
use super::settings_bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
};
use super::settings_tree::{editor_child_stems, editor_service_root_index};

pub(super) fn read_editor_service_settings(
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

pub(super) struct EditorServiceDocument {
    pub service: String,
    pub settings_file: PathBuf,
    pub document: SettingsBytecode,
}

pub(super) fn read_editor_service_documents(src_root: &Path) -> Result<Vec<EditorServiceDocument>> {
    if !src_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for entry in
        fs::read_dir(src_root).with_context(|| format!("Failed to read {}", src_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let settings_file = service_settings_path(&entry.path());
        if settings_file.is_file() {
            documents.push(EditorServiceDocument {
                service: entry.file_name().to_string_lossy().into_owned(),
                document: SettingsBytecode::read_file(&settings_file)?,
                settings_file,
            });
        }
    }
    Ok(documents)
}

pub(super) fn ensure_editor_service_document(
    slot: &mut Option<SettingsBytecode>,
) -> &mut SettingsBytecode {
    slot.get_or_insert_with(|| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    })
}

pub(super) fn ensure_editor_source_target_in_bytecode(
    document: &mut SettingsBytecode,
    spec: &EditorSourcePathSpec,
) -> Result<EditorSourceEnsureResult> {
    let mut changed = false;
    let mut upsert_instance_paths = Vec::new();
    let mut replace_instance_paths = Vec::new();
    let mut target_class_replaced = false;
    let root_index = if let Some(index) = editor_service_root_index(document, &spec.service) {
        index
    } else {
        document.instances.push(SettingsBytecodeInstance {
            settings_id: "editor:0".to_string(),
            name: spec.service.clone(),
            class_name: spec.service.clone(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        });
        changed = true;
        document.instances.len() - 1
    };

    let mut current_index = root_index;
    let mut path_segments = vec![document.instances[root_index].name.clone()];
    let mut path_ordinals = vec![1];
    for component in &spec.parent_components {
        if let Some(child_index) = editor_child_by_stem(document, current_index, component) {
            let child_ordinal = editor_child_name_ordinal(document, current_index, child_index);
            current_index = child_index;
            path_segments.push(document.instances[current_index].name.clone());
            path_ordinals.push(child_ordinal);
            upsert_instance_paths.push((
                current_index,
                path_segments.clone(),
                path_ordinals.clone(),
            ));
            continue;
        }

        let component_class =
            editor_container_class_for_component(&spec.service, &path_segments, component);
        let added = instance_api::add_instance(
            document,
            AddInstanceSpec {
                settings_id: None,
                name: component.clone(),
                class_name: component_class.to_string(),
                parent_index: Some(current_index),
                properties: Map::new(),
                attributes: Map::new(),
            },
        )?;
        current_index = added.index;
        path_segments.push(component.clone());
        path_ordinals.push(editor_child_name_ordinal(
            document,
            document.instances[current_index]
                .parent_index
                .unwrap_or(root_index),
            current_index,
        ));
        changed = true;

        upsert_instance_paths.push((current_index, path_segments.clone(), path_ordinals.clone()));
    }

    let target_index = if let Some(child_index) =
        editor_child_by_stem(document, current_index, &spec.instance_stem)
    {
        if document.instances[child_index].class_name != spec.class_name {
            document.instances[child_index].class_name = spec.class_name.clone();
            changed = true;
            target_class_replaced = true;
        }
        child_index
    } else {
        let added = instance_api::add_instance(
            document,
            AddInstanceSpec {
                settings_id: None,
                name: spec.instance_name.clone(),
                class_name: spec.class_name.clone(),
                parent_index: Some(current_index),
                properties: Map::new(),
                attributes: Map::new(),
            },
        )?;
        changed = true;
        added.index
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

    let target_ordinal = editor_child_name_ordinal(document, current_index, target_index);
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
        upsert_instance_paths.push((target_index, path_segments.clone(), path_ordinals.clone()));
    }
    let sibling_counts = editor_sibling_group_counts(document);
    let upsert_instances = upsert_instance_paths
        .into_iter()
        .map(|(index, segments, ordinals)| {
            editor_instance_descriptor_from_path(
                document,
                index,
                segments,
                ordinals,
                &sibling_counts,
            )
            .context("Failed to describe a source upsert")
        })
        .collect::<Result<Vec<_>>>()?;
    let replace_instances = replace_instance_paths
        .into_iter()
        .map(|(index, segments, ordinals)| {
            editor_instance_descriptor_from_path(
                document,
                index,
                segments,
                ordinals,
                &sibling_counts,
            )
            .context("Failed to describe a source replacement")
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

pub(super) fn is_protected_starter_player_container(
    document: &SettingsBytecode,
    index: usize,
) -> bool {
    let Some(instance) = document.instances.get(index) else {
        return false;
    };
    if !matches!(
        instance.name.as_str(),
        "StarterCharacterScripts" | "StarterPlayerScripts"
    ) {
        return false;
    }
    let Some(parent_index) = instance.parent_index else {
        return false;
    };
    document.instances.get(parent_index).is_some_and(|parent| {
        parent.parent_index.is_none()
            && parent.name == "StarterPlayer"
            && parent.class_name == "StarterPlayer"
    })
}

pub(super) fn editor_child_by_stem(
    document: &SettingsBytecode,
    parent_index: usize,
    stem: &str,
) -> Option<usize> {
    let child_indices = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            (instance.parent_index == Some(parent_index)).then_some(index)
        })
        .collect::<Vec<_>>();
    editor_child_stems(document, &child_indices)
        .into_iter()
        .find_map(|(index, child_stem, _)| (child_stem == stem).then_some(index))
}

fn editor_child_name_ordinal(
    document: &SettingsBytecode,
    parent_index: usize,
    child_index: usize,
) -> usize {
    let Some(child) = document.instances.get(child_index) else {
        return 1;
    };
    let mut ordinal = 0;
    for (index, instance) in document.instances.iter().enumerate() {
        if instance.parent_index == Some(parent_index) && instance.name == child.name {
            ordinal += 1;
            if index == child_index {
                return ordinal.max(1);
            }
        }
    }
    1
}

pub(super) fn document_instance_index_by_settings_id(
    document: &SettingsBytecode,
    settings_id: &str,
) -> Option<usize> {
    document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
}
