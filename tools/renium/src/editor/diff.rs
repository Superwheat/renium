use std::collections::{HashMap, HashSet};

use rbx_reflection::ReflectionDatabase;
use serde_json::{Map, Value};

use crate::bytecode::edit::instance_path_parts_key;
use crate::editor::paths::build_editor_instance_paths;
use crate::editor::review::{
    is_engine_managed_editor_property, is_externally_managed_editor_property,
    is_workspace_camera_sync_target, normalize_editor_bridge_value, property_schema_entry,
};
use crate::editor::sync::is_lua_source_class;
use crate::editor::types::{
    EditorBinaryImport, EditorChangeSet, EditorInstanceChange, EditorInstanceDescriptor,
    EditorInstancePath, EditorPropertyChange, EditorPropertyFilter, EditorSourceChange,
};
use crate::roblox::schema::{MESH_SIZE_TRANSPORT_PROPERTY, PropertySchemaMap};
use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};
use crate::settings::tree::editor_service_root_index;

const MAX_EDITOR_MATCH_FIELDS: usize = 64;
const MAX_EDITOR_MATCH_VALUE_BYTES: usize = 256;
const MAX_EDITOR_MATCH_TOTAL_BYTES: usize = 512;
const MAX_EDITOR_MATCH_CANDIDATES_TO_SCORE: usize = 32;

pub(crate) type EditorSiblingGroupCounts<'a> = HashMap<(usize, &'a str, &'a str), usize>;

pub(crate) fn editor_sibling_group_counts(
    document: &SettingsBytecode,
) -> EditorSiblingGroupCounts<'_> {
    let mut counts = HashMap::new();
    for instance in &document.instances {
        let Some(parent_index) = instance.parent_index else {
            continue;
        };
        *counts
            .entry((
                parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            ))
            .or_insert(0) += 1;
    }
    counts
}

fn editor_match_field_priority(attribute: bool, name: &str) -> usize {
    if attribute {
        return 0;
    }
    if matches!(
        name,
        "Value"
            | "Text"
            | "CFrame"
            | "Position"
            | "Orientation"
            | "Size"
            | "Color"
            | "Transparency"
            | "Enabled"
    ) {
        return 1;
    }
    2
}

fn settings_value_contains_reference(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(settings_value_contains_reference),
        Value::Object(object) => {
            object.get("_type").and_then(Value::as_str) == Some("Ref")
                || object.contains_key("Ref")
                || object.values().any(settings_value_contains_reference)
        }
        _ => false,
    }
}

fn editor_match_records(
    instance: &SettingsBytecodeInstance,
) -> (Map<String, Value>, Map<String, Value>) {
    let mut candidates = Vec::new();
    for (attribute, records) in [(false, &instance.properties), (true, &instance.attributes)] {
        for (name, value) in records {
            if !attribute
                && ["source", "classname", "name", "parent", "tags", "meshsize"]
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate))
            {
                continue;
            }
            if settings_value_contains_reference(value) {
                continue;
            }
            let normalized = normalize_editor_bridge_value(value, None, &[], &[]);
            let Ok(value_bytes) = serde_json::to_vec(&normalized) else {
                continue;
            };
            let encoded_bytes = name.len().saturating_add(value_bytes.len());
            if encoded_bytes > MAX_EDITOR_MATCH_VALUE_BYTES {
                continue;
            }
            candidates.push((
                editor_match_field_priority(attribute, name),
                encoded_bytes,
                attribute,
                name.clone(),
                normalized,
            ));
        }
    }
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.3.cmp(&right.3))
    });

    let mut properties = Map::new();
    let mut attributes = Map::new();
    let mut total_bytes = 0usize;
    for (_, encoded_bytes, attribute, name, value) in candidates {
        if properties.len() + attributes.len() >= MAX_EDITOR_MATCH_FIELDS
            || total_bytes.saturating_add(encoded_bytes) > MAX_EDITOR_MATCH_TOTAL_BYTES
        {
            continue;
        }
        total_bytes += encoded_bytes;
        if attribute {
            attributes.insert(name, value);
        } else {
            properties.insert(name, value);
        }
    }
    (properties, attributes)
}

pub(crate) fn editor_instance_descriptor_from_path(
    document: &SettingsBytecode,
    index: usize,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
    sibling_counts: &EditorSiblingGroupCounts<'_>,
) -> Option<EditorInstanceDescriptor> {
    let instance = document.instances.get(index)?;
    let sibling_count = instance.parent_index.map_or(0, |parent_index| {
        sibling_counts
            .get(&(
                parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            ))
            .copied()
            .unwrap_or(0)
    });
    let ambiguous_siblings = sibling_count > 1;
    let (match_properties, match_attributes) =
        if ambiguous_siblings && sibling_count <= MAX_EDITOR_MATCH_CANDIDATES_TO_SCORE {
            editor_match_records(instance)
        } else {
            (Map::new(), Map::new())
        };
    Some(EditorInstanceDescriptor {
        settings_id: instance.settings_id.clone(),
        path_segments,
        path_ordinals,
        class_name: instance.class_name.clone(),
        ambiguous_siblings,
        anchor_only: false,
        match_properties,
        match_attributes,
    })
}

pub(crate) fn editor_instance_descriptor_for_known_path(
    document: &SettingsBytecode,
    index: usize,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
) -> Option<EditorInstanceDescriptor> {
    editor_instance_descriptor_from_path(
        document,
        index,
        path_segments,
        path_ordinals,
        &editor_sibling_group_counts(document),
    )
}

fn editor_instance_descriptor(
    document: &SettingsBytecode,
    paths_by_index: &[Option<EditorInstancePath>],
    service: &str,
    index: usize,
    sibling_counts: &EditorSiblingGroupCounts<'_>,
) -> Option<EditorInstanceDescriptor> {
    let path_info = paths_by_index.get(index)?.clone()?;
    if !path_info.is_descendant_of(service) {
        return None;
    }
    editor_instance_descriptor_from_path(
        document,
        index,
        path_info.path_segments,
        path_info.path_ordinals,
        sibling_counts,
    )
}

pub(crate) fn push_editor_instance_change(
    changes: &mut EditorChangeSet,
    mode: &str,
    service: &str,
    allow_deletes: bool,
    mut instances: Vec<EditorInstanceDescriptor>,
) {
    instances.sort_by(|a, b| {
        a.path_segments
            .len()
            .cmp(&b.path_segments.len())
            .then_with(|| a.path_segments.cmp(&b.path_segments))
            .then_with(|| a.path_ordinals.cmp(&b.path_ordinals))
            .then_with(|| a.settings_id.cmp(&b.settings_id))
    });

    if instances.is_empty() && !(mode == "reconcileService" && allow_deletes) {
        return;
    }

    changes.instance_changes.push(EditorInstanceChange {
        mode: mode.to_string(),
        service: service.to_string(),
        allow_deletes,
        instances,
        preserve_instances: Vec::new(),
    });
}

pub(crate) fn append_editor_instance_reconcile(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
) {
    let paths_by_index = build_editor_instance_paths(document, service);
    let sibling_counts = editor_sibling_group_counts(document);
    let instances = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            instance.parent_index?;
            editor_instance_descriptor(document, &paths_by_index, service, index, &sibling_counts)
        })
        .collect::<Vec<_>>();
    push_editor_instance_change(changes, "reconcileService", service, true, instances);
}

pub(crate) fn append_editor_target_instance_upserts(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
) {
    let paths_by_index = build_editor_instance_paths(document, service);
    let sibling_counts = editor_sibling_group_counts(document);
    let mut selected_indices = HashSet::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if !filter.includes_instance(&instance.settings_id) {
            continue;
        }
        if instance.class_name == "PackageLink" {
            continue;
        }
        let mut current = Some(index);
        while let Some(current_index) = current {
            let Some(current_instance) = document.instances.get(current_index) else {
                break;
            };
            if current_instance.parent_index.is_none() {
                break;
            }
            selected_indices.insert(current_index);
            current = current_instance.parent_index;
        }
    }

    let instances = selected_indices
        .into_iter()
        .filter_map(|index| {
            editor_instance_descriptor(document, &paths_by_index, service, index, &sibling_counts)
        })
        .collect::<Vec<_>>();
    push_editor_instance_change(changes, "upsertInstances", service, false, instances);
}

pub(crate) fn append_editor_target_inline_source_changes(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
) {
    let paths_by_index = build_editor_instance_paths(document, service);
    for (index, instance) in document.instances.iter().enumerate() {
        if !filter.includes_instance(&instance.settings_id)
            || !is_lua_source_class(&instance.class_name)
        {
            continue;
        }
        let Some(source) = instance.properties.get("Source").and_then(Value::as_str) else {
            continue;
        };
        if source == "__SOURCE_EXTERNAL__" {
            continue;
        }
        let Some(path_info) = paths_by_index.get(index).and_then(std::clone::Clone::clone) else {
            continue;
        };
        if !path_info.is_descendant_of(service) {
            continue;
        }
        changes.source_changes.push(EditorSourceChange {
            service: service.to_string(),
            settings_id: Some(instance.settings_id.clone()),
            path_segments: path_info.path_segments,
            path_ordinals: path_info.path_ordinals,
            class_name: instance.class_name.clone(),
            source: Some(source.to_string()),
            deleted: false,
        });
    }
}

pub(crate) fn append_editor_property_changes(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    property_schema_by_class: &PropertySchemaMap,
    filter: &EditorPropertyFilter,
    database: &ReflectionDatabase<'_>,
) {
    let paths_by_index = build_editor_instance_paths(document, service);
    let settings_ids_by_index = editor_settings_ids(document);
    for (index, instance) in document.instances.iter().enumerate() {
        if !filter.includes_instance(&instance.settings_id) {
            continue;
        }
        let Some(path_info) = paths_by_index.get(index).and_then(std::clone::Clone::clone) else {
            continue;
        };
        let path_segments = path_info.path_segments.clone();
        if is_workspace_camera_sync_target(service, &instance.class_name, &path_segments) {
            continue;
        }

        let mut properties = Map::new();
        for (name, value) in &instance.properties {
            if name.eq_ignore_ascii_case("Source")
                || name.eq_ignore_ascii_case(MESH_SIZE_TRANSPORT_PROPERTY)
            {
                continue;
            }
            if !filter.includes_property(name) {
                continue;
            }
            if is_externally_managed_editor_property(
                service,
                &instance.class_name,
                &path_segments,
                name,
            ) || is_engine_managed_editor_property(&instance.class_name, name, database)
            {
                continue;
            }
            let schema_entry =
                property_schema_entry(property_schema_by_class, &instance.class_name, name);
            properties.insert(
                name.clone(),
                normalize_editor_bridge_value(
                    value,
                    schema_entry,
                    &paths_by_index,
                    &settings_ids_by_index,
                ),
            );
        }

        let attributes = if filter.is_active() {
            Map::new()
        } else {
            normalized_editor_attributes(instance, &paths_by_index, &settings_ids_by_index)
        };

        append_editor_property_change(
            changes, service, instance, path_info, properties, attributes,
        );
    }
}

#[derive(Clone, Copy)]
pub(crate) struct NativeEditorPropertyRules<'a, 'db> {
    pub property_schema_by_class: &'a PropertySchemaMap,
    pub post_apply_properties_by_class: &'a HashMap<String, HashSet<String>>,
    pub post_apply_properties_by_path: &'a HashMap<String, HashSet<String>>,
    pub database: &'db ReflectionDatabase<'db>,
}

pub(crate) fn append_native_editor_full_property_changes(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    paths_by_index: &[Option<EditorInstancePath>],
    service: &str,
    binary_import: &EditorBinaryImport,
    rules: NativeEditorPropertyRules<'_, '_>,
) {
    let root_index = editor_service_root_index(document, service);
    let settings_ids_by_index = editor_settings_ids(document);
    for (index, instance) in document.instances.iter().enumerate() {
        let direct_service_child = instance.parent_index == root_index;
        if service == "Workspace"
            && direct_service_child
            && instance.class_name == "Camera"
            && matches!(instance.name.as_str(), "Camera" | "CurrentCamera")
        {
            continue;
        }
        let send_all = Some(index) == root_index
            || (direct_service_child
                && ((service == "Workspace" && instance.class_name == "Terrain")
                    || (service == "StarterPlayer"
                        && matches!(
                            instance.class_name.as_str(),
                            "StarterPlayerScripts" | "StarterCharacterScripts"
                        ))));
        let class_post_apply_names = rules
            .post_apply_properties_by_class
            .get(&instance.class_name);
        if !send_all
            && class_post_apply_names.is_none()
            && rules.post_apply_properties_by_path.is_empty()
        {
            continue;
        }
        let Some(path_info) = paths_by_index.get(index).and_then(std::clone::Clone::clone) else {
            continue;
        };
        let retained =
            binary_import.retains_path(service, &path_info.path_segments, &path_info.path_ordinals);
        let path_segments = path_info.path_segments.clone();
        let path_post_apply_names = if rules.post_apply_properties_by_path.is_empty() {
            None
        } else {
            rules
                .post_apply_properties_by_path
                .get(&instance_path_parts_key(
                    &path_segments,
                    &path_info.path_ordinals,
                ))
        };
        let post_apply_names = if retained {
            None
        } else {
            class_post_apply_names
        };
        if !send_all && post_apply_names.is_none() && path_post_apply_names.is_none() {
            continue;
        }

        let mut properties = Map::new();
        for (name, value) in &instance.properties {
            if name.eq_ignore_ascii_case("Source")
                || name.eq_ignore_ascii_case(MESH_SIZE_TRANSPORT_PROPERTY)
                || (name == "WorldPivot" && instance.properties.contains_key("PrimaryPart"))
                || (!send_all
                    && post_apply_names.is_none_or(|names| !names.contains(name))
                    && path_post_apply_names.is_none_or(|names| !names.contains(name)))
                || is_externally_managed_editor_property(
                    service,
                    &instance.class_name,
                    &path_segments,
                    name,
                )
                || is_engine_managed_editor_property(&instance.class_name, name, rules.database)
            {
                continue;
            }
            let schema_entry =
                property_schema_entry(rules.property_schema_by_class, &instance.class_name, name);
            properties.insert(
                name.clone(),
                normalize_editor_bridge_value(
                    value,
                    schema_entry,
                    paths_by_index,
                    &settings_ids_by_index,
                ),
            );
        }

        let attributes = if send_all {
            normalized_editor_attributes(instance, paths_by_index, &settings_ids_by_index)
        } else {
            Map::new()
        };
        append_editor_property_change(
            changes, service, instance, path_info, properties, attributes,
        );
    }
}

fn editor_settings_ids(document: &SettingsBytecode) -> Vec<&str> {
    document
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect()
}

fn normalized_editor_attributes(
    instance: &SettingsBytecodeInstance,
    paths_by_index: &[Option<EditorInstancePath>],
    settings_ids_by_index: &[&str],
) -> Map<String, Value> {
    instance
        .attributes
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                normalize_editor_bridge_value(value, None, paths_by_index, settings_ids_by_index),
            )
        })
        .collect()
}

fn append_editor_property_change(
    changes: &mut EditorChangeSet,
    service: &str,
    instance: &SettingsBytecodeInstance,
    path: EditorInstancePath,
    properties: Map<String, Value>,
    attributes: Map<String, Value>,
) {
    if properties.is_empty() && attributes.is_empty() {
        return;
    }
    changes.property_changes.push(EditorPropertyChange {
        service: service.to_string(),
        settings_id: Some(instance.settings_id.clone()),
        path_segments: path.path_segments,
        path_ordinals: path.path_ordinals,
        class_name: instance.class_name.clone(),
        properties,
        attributes,
        deleted_attributes: Vec::new(),
    });
}
