use std::collections::{HashMap, HashSet};

use rbx_reflection::ReflectionDatabase;
use serde_json::{Map, Value};

use crate::editor::paths::{build_editor_instance_paths, build_editor_instance_paths_for_indices};
use crate::editor::review::{
    EditorReferenceIds, is_engine_managed_editor_property, is_externally_managed_editor_property,
    normalize_editor_bridge_value, property_schema_entry,
};
use crate::editor::sync::is_lua_source_class;
use crate::editor::types::{
    EditorBinaryImport, EditorChangeSet, EditorInstanceChange, EditorInstanceDescriptor,
    EditorInstancePath, EditorPropertyChange, EditorPropertyFilter, EditorSourceChange,
};
use crate::rbx::encode::rbx_logical_property_name;
use crate::roblox::schema::PropertySchemaMap;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};
use crate::settings::equivalence::is_reconciliation_protected_workspace_camera;
use crate::settings::tree::editor_service_root_index;

const MAX_EDITOR_MATCH_FIELDS: usize = 64;
const MAX_EDITOR_MATCH_VALUE_BYTES: usize = 256;
const MAX_EDITOR_MATCH_TOTAL_BYTES: usize = 512;
const MAX_EDITOR_MATCH_CANDIDATES_TO_SCORE: usize = 32;

pub(crate) type EditorSiblingGroupCounts<'a> = HashMap<(usize, &'a str), usize>;

pub(crate) fn editor_sibling_group_counts(
    document: &SettingsBytecode,
) -> EditorSiblingGroupCounts<'_> {
    let mut counts = HashMap::new();
    for instance in &document.instances {
        let Some(parent_index) = instance.parent_index else {
            continue;
        };
        *counts
            .entry((parent_index, instance.name.as_str()))
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

fn collect_editor_reference_indices(
    value: &Value,
    ids: &EditorReferenceIds<'_>,
    indices: &mut HashSet<usize>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_editor_reference_indices(item, ids, indices);
            }
        }
        Value::Object(object) => {
            let reference = if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                Some(object)
            } else {
                object.get("Ref").and_then(Value::as_object)
            };
            if let Some(index) = reference.and_then(|reference| ids.resolve(reference)) {
                indices.insert(index);
            }
            for nested in object.values() {
                collect_editor_reference_indices(nested, ids, indices);
            }
        }
        _ => {}
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
            let normalized =
                normalize_editor_bridge_value(value, None, &[], &EditorReferenceIds::default());
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
    let name_sibling_count = instance.parent_index.map_or(0, |parent_index| {
        sibling_counts
            .get(&(parent_index, instance.name.as_str()))
            .copied()
            .unwrap_or(0)
    });
    let ambiguous_siblings = name_sibling_count > 1;
    let (match_properties, match_attributes) =
        if ambiguous_siblings && name_sibling_count <= MAX_EDITOR_MATCH_CANDIDATES_TO_SCORE {
            editor_match_records(instance)
        } else {
            (Map::new(), Map::new())
        };
    Some(EditorInstanceDescriptor {
        settings_id: instance.settings_id.clone(),
        path_segments,
        path_ordinals,
        previous_path_segments: Vec::new(),
        previous_path_ordinals: Vec::new(),
        class_name: instance.class_name.clone(),
        previous_class_name: None,
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
    let sibling_counts = editor_sibling_group_counts(document);
    editor_instance_descriptor_from_path(
        document,
        index,
        path_segments,
        path_ordinals,
        &sibling_counts,
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
    let instance = document.instances.get(index)?;
    let anchor_only = path_info.path_segments.len() == 2
        && crate::roblox::services::is_engine_managed_container(service, &instance.class_name)
        && path_info.path_segments[1] == instance.class_name;
    let mut descriptor = editor_instance_descriptor_from_path(
        document,
        index,
        path_info.path_segments,
        path_info.path_ordinals,
        sibling_counts,
    )?;
    descriptor.anchor_only = anchor_only;
    Some(descriptor)
}

pub(crate) fn push_editor_instance_change(
    changes: &mut EditorChangeSet,
    mode: &str,
    service: &str,
    allow_deletes: bool,
    mut instances: Vec<EditorInstanceDescriptor>,
) {
    if mode == "upsertInstances" {
        // Source-file discovery may already have emitted batches whose parents
        // are anchors. Merge the service's planned upserts so newly created
        // parents precede every dependent script, including across wire batches.
        let mut index = 0;
        while index < changes.instance_changes.len() {
            let change = &changes.instance_changes[index];
            if change.mode == mode && change.service == service {
                let mut change = changes.instance_changes.remove(index);
                instances.append(&mut change.instances);
            } else {
                index += 1;
            }
        }
    }
    instances.sort_by(|a, b| {
        a.path_segments
            .len()
            .cmp(&b.path_segments.len())
            .then_with(|| a.path_segments.cmp(&b.path_segments))
            .then_with(|| a.path_ordinals.cmp(&b.path_ordinals))
            .then_with(|| a.settings_id.cmp(&b.settings_id))
            .then_with(|| a.anchor_only.cmp(&b.anchor_only))
    });
    if mode == "upsertInstances" {
        instances.dedup_by(|right, left| {
            right.settings_id == left.settings_id
                && right.path_segments == left.path_segments
                && right.path_ordinals == left.path_ordinals
                && right.class_name == left.class_name
        });
    }

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

#[cfg(test)]
pub(crate) fn append_editor_target_instance_upserts(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
) {
    let paths_by_index = build_editor_instance_paths(document, service);
    append_editor_target_instance_upserts_with_paths(
        changes,
        document,
        service,
        filter,
        &paths_by_index,
    );
}

fn append_editor_target_instance_upserts_with_paths(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
    paths_by_index: &[Option<EditorInstancePath>],
) {
    let (target_indices, mut selected_indices) = editor_target_indices(document, filter);
    let sibling_counts = expand_ambiguous_editor_siblings(document, &mut selected_indices);
    append_editor_target_instance_upserts_for_indices(
        changes,
        document,
        service,
        paths_by_index,
        &sibling_counts,
        &target_indices,
        &selected_indices,
    );
}

fn editor_target_indices(
    document: &SettingsBytecode,
    filter: &EditorPropertyFilter,
) -> (HashSet<usize>, HashSet<usize>) {
    let mut target_indices = HashSet::new();
    let mut selected_indices = HashSet::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if !filter.includes_instance(&instance.settings_id) {
            continue;
        }
        if instance.class_name == "PackageLink" {
            continue;
        }
        target_indices.insert(index);
        let mut current = Some(index);
        while let Some(current_index) = current {
            let Some(current_instance) = document.instances.get(current_index) else {
                break;
            };
            if current_instance.parent_index.is_none() {
                break;
            }
            // A previously selected ancestor already contributed its full chain.
            if !selected_indices.insert(current_index) {
                break;
            }
            current = current_instance.parent_index;
        }
    }
    (target_indices, selected_indices)
}

fn expand_ambiguous_editor_siblings<'a>(
    document: &'a SettingsBytecode,
    selected_indices: &mut HashSet<usize>,
) -> EditorSiblingGroupCounts<'a> {
    let selected_groups = selected_indices
        .iter()
        .map(|index| {
            let instance = &document.instances[*index];
            (instance.parent_index, instance.name.as_str())
        })
        .collect::<HashSet<_>>();
    let mut sibling_groups: HashMap<(Option<usize>, &str), Vec<usize>> = HashMap::new();
    for (index, instance) in document.instances.iter().enumerate() {
        let key = (instance.parent_index, instance.name.as_str());
        if !selected_groups.contains(&key) {
            continue;
        }
        sibling_groups.entry(key).or_default().push(index);
    }
    for siblings in sibling_groups.values() {
        if siblings.len() > 1 {
            selected_indices.extend(siblings);
        }
    }
    sibling_groups
        .into_iter()
        .filter_map(|((parent_index, name), siblings)| {
            parent_index.map(|parent_index| ((parent_index, name), siblings.len()))
        })
        .collect()
}

fn append_editor_target_instance_upserts_for_indices(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    paths_by_index: &[Option<EditorInstancePath>],
    sibling_counts: &EditorSiblingGroupCounts<'_>,
    target_indices: &HashSet<usize>,
    selected_indices: &HashSet<usize>,
) {
    let instances = selected_indices
        .iter()
        .copied()
        .filter_map(|index| {
            let mut descriptor = editor_instance_descriptor(
                document,
                paths_by_index,
                service,
                index,
                sibling_counts,
            )?;
            descriptor.anchor_only |= !target_indices.contains(&index);
            Some(descriptor)
        })
        .collect::<Vec<_>>();
    push_editor_instance_change(changes, "upsertInstances", service, false, instances);
}

#[cfg(test)]
pub(crate) fn append_editor_target_inline_source_changes(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
) {
    let paths_by_index = build_editor_instance_paths(document, service);
    append_editor_target_inline_source_changes_with_paths(
        changes,
        document,
        service,
        filter,
        &paths_by_index,
    );
}

fn append_editor_target_inline_source_changes_with_paths(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
    paths_by_index: &[Option<EditorInstancePath>],
) {
    for (index, instance) in document.instances.iter().enumerate() {
        if !filter.includes_instance(&instance.settings_id)
            || !is_lua_source_class(&instance.class_name)
        {
            continue;
        }
        let Some(source) = instance.properties.get("Source").and_then(Value::as_str) else {
            continue;
        };
        if source == EXTERNAL_SOURCE_MARKER {
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

#[cfg(test)]
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
    append_editor_property_changes_with_paths(
        changes,
        document,
        service,
        property_schema_by_class,
        filter,
        database,
        EditorPropertyPaths {
            paths_by_index: &paths_by_index,
            settings_ids_by_index: &settings_ids_by_index,
            scope: EditorPropertyScope::All,
        },
    );
}

struct EditorPropertyPaths<'a> {
    paths_by_index: &'a [Option<EditorInstancePath>],
    settings_ids_by_index: &'a EditorReferenceIds<'a>,
    scope: EditorPropertyScope<'a>,
}

#[derive(Clone, Copy)]
pub(crate) enum EditorPropertyScope<'a> {
    All,
    RetainedContainers,
    Imported(Option<&'a EditorBinaryImport>),
}

fn append_editor_property_changes_with_paths(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    property_schema_by_class: &PropertySchemaMap,
    filter: &EditorPropertyFilter,
    database: &ReflectionDatabase<'_>,
    paths: EditorPropertyPaths<'_>,
) {
    for (index, instance) in document.instances.iter().enumerate() {
        if instance.class_name == "PackageLink" {
            continue;
        }
        if !filter.includes_instance(&instance.settings_id) {
            continue;
        }
        let Some(path_info) = paths.paths_by_index.get(index).and_then(Option::as_ref) else {
            continue;
        };
        let path_segments = &path_info.path_segments;
        if is_reconciliation_protected_workspace_camera(document, index) {
            continue;
        }
        let retained_container = !matches!(paths.scope, EditorPropertyScope::All)
            && (path_segments.as_slice() == [service]
                || path_segments.len() == 2
                    && crate::roblox::services::is_engine_managed_container(
                        service,
                        &instance.class_name,
                    ));
        let import = match paths.scope {
            EditorPropertyScope::All => None,
            EditorPropertyScope::RetainedContainers if !retained_container => continue,
            EditorPropertyScope::RetainedContainers => None,
            EditorPropertyScope::Imported(_) if retained_container => continue,
            EditorPropertyScope::Imported(import) => import.filter(|import| {
                import.imports_path(service, path_segments, &path_info.path_ordinals)
            }),
        };
        let retained_payload = import.is_some_and(|import| {
            import.retains_path(service, path_segments, &path_info.path_ordinals)
        });
        let class_names = import.and_then(|import| {
            import
                .post_apply_properties_by_class
                .get(&instance.class_name)
        });
        let path_names = import.and_then(|import| {
            import.post_apply_properties_by_path.get(
                &crate::bytecode::edit::instance_path_parts_key(
                    path_segments,
                    &path_info.path_ordinals,
                ),
            )
        });

        let mut properties = Map::new();
        for (name, value) in &instance.properties {
            let logical_name =
                rbx_logical_property_name(database, &instance.class_name, name).unwrap_or(name);
            if name.eq_ignore_ascii_case("Source") {
                continue;
            }
            if !filter.includes_property(logical_name) {
                continue;
            }
            if import.is_some()
                && !(instance.class_name == "Model" && logical_name == "WorldPivot")
                && (retained_payload
                    || !(class_names.is_some_and(|names| names.contains(logical_name))
                        || path_names.is_some_and(|names| names.contains(logical_name))))
            {
                continue;
            }
            if is_externally_managed_editor_property(
                service,
                &instance.class_name,
                path_segments,
                logical_name,
            ) || is_engine_managed_editor_property(&instance.class_name, logical_name, database)
            {
                continue;
            }
            let schema_entry =
                property_schema_entry(property_schema_by_class, &instance.class_name, logical_name);
            properties.insert(
                logical_name.to_string(),
                normalize_editor_bridge_value(
                    value,
                    schema_entry,
                    paths.paths_by_index,
                    paths.settings_ids_by_index,
                ),
            );
        }

        let attributes = if !filter.property_names.is_empty() || import.is_some() {
            Map::new()
        } else {
            normalized_editor_attributes(
                instance,
                paths.paths_by_index,
                paths.settings_ids_by_index,
            )
        };

        if !properties.is_empty() || !attributes.is_empty() {
            append_editor_property_change(
                changes,
                service,
                instance,
                path_info.clone(),
                properties,
                attributes,
            );
        }
    }
}

pub(crate) struct EditorTargetChangeOptions<'a, 'db> {
    pub(crate) upsert_instances: bool,
    pub(crate) properties: bool,
    pub(crate) property_scope: EditorPropertyScope<'a>,
    pub(crate) property_schema_by_class: &'a PropertySchemaMap,
    pub(crate) database: &'db ReflectionDatabase<'db>,
}

// A full native import needs paths only for retained fields, inline sources and
// serializer exceptions. Decide that before allocating every descendant's path.
// Additive/partial imports keep the full-path route: their ordinary fields still
// depend on exact root membership. Final emission below remains authoritative.
fn native_target_path_indices(
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
    options: &EditorTargetChangeOptions<'_, '_>,
    ids: &EditorReferenceIds<'_>,
) -> Option<HashSet<usize>> {
    let retained = matches!(
        options.property_scope,
        EditorPropertyScope::RetainedContainers
    );
    if options.upsert_instances && !retained {
        return None;
    }
    let import = match options.property_scope {
        EditorPropertyScope::RetainedContainers => None,
        EditorPropertyScope::Imported(Some(import)) if import.imports_service(service) => {
            Some(import)
        }
        _ => return None,
    };
    let mut indices = HashSet::new();
    let Some(root) = editor_service_root_index(document, service) else {
        return Some(indices);
    };
    // Path-specific exceptions cannot be resolved yet. Their union is a safe
    // candidate superset; emission checks the exact path AND sibling ordinals.
    let path_names = import
        .into_iter()
        .flat_map(|import| import.post_apply_properties_by_path.values())
        .flatten()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut property_candidates: HashMap<&str, HashMap<&str, bool>> = HashMap::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if !filter.includes_instance(&instance.settings_id) {
            continue;
        }
        if options.upsert_instances
            && is_lua_source_class(&instance.class_name)
            && instance
                .properties
                .get("Source")
                .and_then(Value::as_str)
                .is_some_and(|source| source != EXTERNAL_SOURCE_MARKER)
        {
            indices.insert(index);
        }
        let retained_container = index == root && instance.name == service
            || instance.parent_index == Some(root)
                && crate::roblox::services::is_engine_managed_container(
                    service,
                    &instance.class_name,
                );
        if !options.properties
            || instance.class_name == "PackageLink"
            || retained != retained_container
        {
            continue;
        }
        let class_names = import.and_then(|import| {
            import
                .post_apply_properties_by_class
                .get(&instance.class_name)
        });
        if !retained
            && instance.class_name != "Model"
            && path_names.is_empty()
            && class_names.is_none_or(HashSet::is_empty)
        {
            continue;
        }
        // Resolve each raw/serialized/alias name once per class, rather than
        // repeating reflection lookup for every value in the service.
        let candidates = property_candidates.entry(&instance.class_name).or_default();
        for (name, value) in &instance.properties {
            let selected = *candidates.entry(name).or_insert_with(|| {
                let logical =
                    rbx_logical_property_name(options.database, &instance.class_name, name)
                        .unwrap_or(name);
                !name.eq_ignore_ascii_case("Source")
                    && filter.includes_property(logical)
                    && (retained
                        || instance.class_name == "Model" && logical == "WorldPivot"
                        || class_names.is_some_and(|names| names.contains(logical))
                        || path_names.contains(logical))
            });
            if selected {
                indices.insert(index);
                collect_editor_reference_indices(value, ids, &mut indices);
            }
        }
        if retained && filter.property_names.is_empty() && !instance.attributes.is_empty() {
            indices.insert(index);
            for value in instance.attributes.values() {
                collect_editor_reference_indices(value, ids, &mut indices);
            }
        }
    }
    Some(indices)
}

pub(crate) fn append_editor_target_changes(
    changes: &mut EditorChangeSet,
    document: &SettingsBytecode,
    service: &str,
    filter: &EditorPropertyFilter,
    options: EditorTargetChangeOptions<'_, '_>,
) {
    let settings_ids_by_index = editor_settings_ids(document);
    let native_indices =
        native_target_path_indices(document, service, filter, &options, &settings_ids_by_index);
    if !filter.settings_ids.is_empty() && native_indices.is_none() {
        let (target_indices, mut selected_indices) = editor_target_indices(document, filter);
        let upsert_descriptors = options.upsert_instances
            && !matches!(
                options.property_scope,
                EditorPropertyScope::RetainedContainers
            );
        let sibling_counts = if upsert_descriptors {
            expand_ambiguous_editor_siblings(document, &mut selected_indices)
        } else {
            EditorSiblingGroupCounts::new()
        };
        let mut path_indices = selected_indices.clone();
        if options.properties {
            // Upsert selection excludes the service root (it cannot be created),
            // but targeted root properties/attributes still need its live path.
            path_indices.extend(target_indices.iter().copied());
            for index in &target_indices {
                let instance = &document.instances[*index];
                for value in instance
                    .properties
                    .values()
                    .chain(instance.attributes.values())
                {
                    collect_editor_reference_indices(
                        value,
                        &settings_ids_by_index,
                        &mut path_indices,
                    );
                }
            }
        }
        let path_indices = path_indices.into_iter().collect::<Vec<_>>();
        let mut paths_by_index = vec![None; document.instances.len()];
        for (index, path) in
            build_editor_instance_paths_for_indices(document, service, &path_indices)
        {
            paths_by_index[index] = Some(path);
        }
        if options.upsert_instances {
            if upsert_descriptors {
                append_editor_target_instance_upserts_for_indices(
                    changes,
                    document,
                    service,
                    &paths_by_index,
                    &sibling_counts,
                    &target_indices,
                    &selected_indices,
                );
            }
            append_editor_target_inline_source_changes_with_paths(
                changes,
                document,
                service,
                filter,
                &paths_by_index,
            );
        }
        if options.properties {
            append_editor_property_changes_with_paths(
                changes,
                document,
                service,
                options.property_schema_by_class,
                filter,
                options.database,
                EditorPropertyPaths {
                    paths_by_index: &paths_by_index,
                    settings_ids_by_index: &settings_ids_by_index,
                    scope: options.property_scope,
                },
            );
        }
        return;
    }

    let paths_by_index = if let Some(indices) = native_indices {
        let mut paths = vec![None; document.instances.len()];
        for (index, path) in build_editor_instance_paths_for_indices(
            document,
            service,
            &indices.into_iter().collect::<Vec<_>>(),
        ) {
            paths[index] = Some(path);
        }
        paths
    } else {
        build_editor_instance_paths(document, service)
    };
    if options.upsert_instances {
        if !matches!(
            options.property_scope,
            EditorPropertyScope::RetainedContainers
        ) {
            append_editor_target_instance_upserts_with_paths(
                changes,
                document,
                service,
                filter,
                &paths_by_index,
            );
        }
        append_editor_target_inline_source_changes_with_paths(
            changes,
            document,
            service,
            filter,
            &paths_by_index,
        );
    }
    if options.properties {
        append_editor_property_changes_with_paths(
            changes,
            document,
            service,
            options.property_schema_by_class,
            filter,
            options.database,
            EditorPropertyPaths {
                paths_by_index: &paths_by_index,
                settings_ids_by_index: &settings_ids_by_index,
                scope: options.property_scope,
            },
        );
    }
}

fn editor_settings_ids(document: &SettingsBytecode) -> EditorReferenceIds<'_> {
    EditorReferenceIds::new(
        document
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str()),
    )
}

fn normalized_editor_attributes(
    instance: &SettingsBytecodeInstance,
    paths_by_index: &[Option<EditorInstancePath>],
    settings_ids_by_index: &EditorReferenceIds<'_>,
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
        reset_properties: Vec::new(),
        attributes,
        deleted_attributes: Vec::new(),
    });
}

#[cfg(test)]
mod native_property_tests {
    use super::*;
    use crate::bytecode::edit::instance_path_parts_key;
    use crate::editor::types::{EditorBinaryImportGroup, EditorBinaryRetainedRoot};
    use serde_json::json;

    // The production full-path algorithm before target-first selection. Keep the
    // emitter unchanged on both sides so this detects lost paths/reference closure
    // as well as changed source, property and ambiguity output.
    fn full_path_collection(
        document: &SettingsBytecode,
        service: &str,
        filter: &EditorPropertyFilter,
        options: &EditorTargetChangeOptions<'_, '_>,
    ) -> EditorChangeSet {
        let mut changes = EditorChangeSet::default();
        let paths = build_editor_instance_paths(document, service);
        if options.upsert_instances {
            if !matches!(
                options.property_scope,
                EditorPropertyScope::RetainedContainers
            ) {
                append_editor_target_instance_upserts_with_paths(
                    &mut changes,
                    document,
                    service,
                    filter,
                    &paths,
                );
            }
            append_editor_target_inline_source_changes_with_paths(
                &mut changes,
                document,
                service,
                filter,
                &paths,
            );
        }
        if options.properties {
            append_editor_property_changes_with_paths(
                &mut changes,
                document,
                service,
                options.property_schema_by_class,
                filter,
                options.database,
                EditorPropertyPaths {
                    paths_by_index: &paths,
                    settings_ids_by_index: &editor_settings_ids(document),
                    scope: options.property_scope,
                },
            );
        }
        changes
    }

    fn wire_changes(changes: &EditorChangeSet) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "instances": &changes.instance_changes,
            "sources": &changes.source_changes,
            "properties": &changes.property_changes,
        }))
        .unwrap()
    }

    fn target_first_fixture(extra_folders: usize) -> SettingsBytecode {
        let mut document = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        for (name, class, parent, properties) in [
            (
                "Workspace",
                "Workspace",
                None,
                json!({"Gravity":150, "CurrentCamera":{"_type":"Ref","instanceIndex":10}}),
            ),
            (
                "Terrain",
                "Terrain",
                Some(0),
                json!({"WaterTransparency":0.4}),
            ),
            ("Same", "Folder", Some(0), json!({})),
            ("Same", "Folder", Some(0), json!({})),
            // No MeshPart exception is requested: its path is needed ONLY as a
            // reference target, including the second same-named parent ordinal.
            ("Target", "MeshPart", Some(3), json!({"Transparency":0.5})),
            (
                "Reference",
                "ObjectValue",
                Some(2),
                json!({"Value":{"Ref":{"settingsId":"id-4"}}}),
            ),
            (
                "Car",
                "Model",
                Some(0),
                json!({"WorldPivot":{"_type":"CFrame","components":[1,2,3,1,0,0,0,1,0,0,0,1]},
                    "PrimaryPart":{"_type":"Ref","instanceIndex":5}}),
            ),
            (
                "Inline",
                "ModuleScript",
                Some(2),
                json!({"Source":"return 42"}),
            ),
            (
                "External",
                "ModuleScript",
                Some(3),
                json!({"Source":EXTERNAL_SOURCE_MARKER}),
            ),
            ("Active", "Camera", Some(0), json!({"FieldOfView":75})),
            ("Gui", "Frame", Some(0), json!({"Sink":true})),
            (
                "Joint",
                "WeldConstraint",
                Some(2),
                json!({"Part0":{"_type":"Ref","settingsId":"id-4","instanceIndex":1},
                    "Part1":{"_type":"Ref","instanceIndex":0}}),
            ),
            ("Retained", "Folder", Some(0), json!({})),
            ("Part", "Part", Some(12), json!({"Transparency":0.7})),
            (
                "Model",
                "Model",
                Some(12),
                json!({"WorldPivot":{"_type":"CFrame","components":[4,5,6,1,0,0,0,1,0,0,0,1]}}),
            ),
            (
                "PackageLink",
                "PackageLink",
                Some(12),
                json!({"PackageId":"rbxassetid://1"}),
            ),
            ("Data", "StringValue", Some(2), json!({"Value":"keep\0雪"})),
            ("Part", "Part", Some(0), json!({"transparency":0.25})),
        ] {
            let mut instance = SettingsBytecodeInstance::new(
                format!("id-{}", document.instances.len()),
                name.into(),
                class.into(),
                parent,
            );
            instance.properties = properties.as_object().unwrap().clone();
            instance.attributes.insert("Kept".into(), json!(true));
            document.instances.push(instance);
        }
        document.instances[1].attributes.insert(
            "Nested".into(),
            json!([
                {"Ref":{"settingsId":"id-4"}},
                {"child":{"_type":"Ref","instanceIndex":10}},
                {"_type":"Ref","instanceIndex":0},
                {"_type":"Ref","instanceIndex":999999},
                {"_type":"Ref","pathSegments":["ReplicatedStorage","Outside"],"pathOrdinals":[1,2]}
            ]),
        );
        for index in 0..extra_folders {
            let mut instance = SettingsBytecodeInstance::new(
                format!("unused-{index}"),
                format!("Folder-{index}"),
                "Folder".into(),
                Some(2),
            );
            instance
                .attributes
                .insert("SavedByNative".into(), json!(true));
            document.instances.push(instance);
        }
        document
    }

    fn target_first_import(additive: bool) -> EditorBinaryImport {
        let logical_sink =
            rbx_logical_property_name(rbx_reflection_database::get().unwrap(), "Frame", "Sink")
                .unwrap_or("Sink");
        EditorBinaryImport {
            bytes: Vec::new(),
            native_replacement: None,
            instance_count: 18,
            external_references_post_applied: true,
            viewport_references_post_applied: true,
            groups: vec![EditorBinaryImportGroup {
                service: "Workspace".into(),
                additive,
                target_path: vec!["Workspace".into()],
                count: 1,
                payload_root_name: "payload".into(),
                expected_structure: None,
                root_paths: vec![crate::editor::types::EditorBinaryRootPath {
                    path_segments: vec!["Workspace".into(), "Same".into()],
                    path_ordinals: vec![1, 2],
                }],
                viewport_camera: None,
                retained_roots: vec![EditorBinaryRetainedRoot {
                    path_segments: vec!["Workspace".into(), "Retained".into()],
                    path_ordinals: vec![1, 1],
                    class_name: "Folder".into(),
                    payload_index: 1,
                    instance_count: 4,
                    payload_omitted: true,
                }],
                package_roots: Vec::new(),
                mutation_package_roots: Vec::new(),
                change_generation: Some(0),
            }],
            post_apply_properties_by_class: HashMap::from([
                ("Part".into(), HashSet::from(["Transparency".into()])),
                ("ObjectValue".into(), HashSet::from(["Value".into()])),
                (
                    "WeldConstraint".into(),
                    HashSet::from(["Part0".into(), "Part1".into()]),
                ),
            ]),
            post_apply_properties_by_path: HashMap::from([(
                instance_path_parts_key(&["Workspace".into(), "Gui".into()], &[1, 1]),
                HashSet::from([logical_sink.into()]),
            )]),
        }
    }

    #[test]
    fn target_first_native_collection_matches_full_path_output() {
        let document = target_first_fixture(64);
        let import = target_first_import(false);
        let additive = target_first_import(true);
        let schema = PropertySchemaMap::new();
        for scope in [
            EditorPropertyScope::All,
            EditorPropertyScope::RetainedContainers,
            EditorPropertyScope::Imported(None),
            EditorPropertyScope::Imported(Some(&import)),
            EditorPropertyScope::Imported(Some(&additive)),
        ] {
            for filter in [
                EditorPropertyFilter::default(),
                EditorPropertyFilter {
                    property_names: HashSet::from(["transparency".into(), "value".into()]),
                    ..Default::default()
                },
                EditorPropertyFilter {
                    settings_ids: HashSet::from([
                        "id-0".into(),
                        "id-1".into(),
                        "id-3".into(),
                        "id-5".into(),
                        "id-7".into(),
                        "id-11".into(),
                    ]),
                    ..Default::default()
                },
            ] {
                for upsert_instances in [false, true] {
                    for properties in [false, true] {
                        let options = EditorTargetChangeOptions {
                            upsert_instances,
                            properties,
                            property_scope: scope,
                            property_schema_by_class: &schema,
                            database: rbx_reflection_database::get().unwrap(),
                        };
                        let expected =
                            full_path_collection(&document, "Workspace", &filter, &options);
                        let mut actual = EditorChangeSet::default();
                        append_editor_target_changes(
                            &mut actual,
                            &document,
                            "Workspace",
                            &filter,
                            options,
                        );
                        assert_eq!(wire_changes(&actual), wire_changes(&expected));
                    }
                }
            }
        }
    }

    #[test]
    fn target_first_paths_skip_native_only_nodes_and_keep_reference_ordinals() {
        let document = target_first_fixture(2048);
        let import = target_first_import(false);
        let schema = PropertySchemaMap::new();
        for filter in [
            EditorPropertyFilter::default(),
            EditorPropertyFilter {
                settings_ids: document
                    .instances
                    .iter()
                    .map(|instance| instance.settings_id.clone())
                    .collect(),
                ..Default::default()
            },
        ] {
            for scope in [
                EditorPropertyScope::RetainedContainers,
                EditorPropertyScope::Imported(Some(&import)),
            ] {
                let options = EditorTargetChangeOptions {
                    upsert_instances: matches!(scope, EditorPropertyScope::RetainedContainers),
                    properties: true,
                    property_scope: scope,
                    property_schema_by_class: &schema,
                    database: rbx_reflection_database::get().unwrap(),
                };
                let indices = native_target_path_indices(
                    &document,
                    "Workspace",
                    &filter,
                    &options,
                    &editor_settings_ids(&document),
                )
                .unwrap();
                assert!(
                    !indices
                        .iter()
                        .any(|index| *index >= 18 && *index < document.instances.len())
                );
                assert!(indices.contains(&4), "Reference target was dropped");
                let selective = build_editor_instance_paths_for_indices(
                    &document,
                    "Workspace",
                    &indices.into_iter().collect::<Vec<_>>(),
                );
                assert!(
                    selective.len() < 18,
                    "Materialized paths for native-only descendants"
                );
                assert_eq!(selective[&4].path_ordinals, vec![1, 2, 1]);
                let expected = full_path_collection(&document, "Workspace", &filter, &options);
                let mut actual = EditorChangeSet::default();
                append_editor_target_changes(&mut actual, &document, "Workspace", &filter, options);
                assert_eq!(wire_changes(&actual), wire_changes(&expected));
                if let Some(reference) = actual
                    .property_changes
                    .iter()
                    .find(|change| change.class_name == "ObjectValue")
                {
                    assert_eq!(
                        reference.properties["Value"]["pathSegments"],
                        json!(["Workspace", "Same", "Target"])
                    );
                    assert_eq!(
                        reference.properties["Value"]["pathOrdinals"],
                        json!([1, 2, 1])
                    );
                }
            }
        }
    }

    #[test]
    fn target_first_collection_preserves_root_fallback_and_disconnected_trees() {
        let mut document = target_first_fixture(0);
        document.instances[0].name = "RenamedWorkspace".into();
        let other_root = document.instances.len();
        document.instances.push(SettingsBytecodeInstance::new(
            "other-root".into(),
            "Other".into(),
            "Folder".into(),
            None,
        ));
        let mut other = SettingsBytecodeInstance::new(
            "other-script".into(),
            "Script".into(),
            "ModuleScript".into(),
            Some(other_root),
        );
        other
            .properties
            .insert("Source".into(), json!("return 'outside'"));
        document.instances.push(other);
        let import = target_first_import(false);
        let schema = PropertySchemaMap::new();
        let filter = EditorPropertyFilter::default();
        for empty in [false, true] {
            if empty {
                document.instances.clear();
            }
            for scope in [
                EditorPropertyScope::RetainedContainers,
                EditorPropertyScope::Imported(Some(&import)),
            ] {
                let options = EditorTargetChangeOptions {
                    upsert_instances: matches!(scope, EditorPropertyScope::RetainedContainers),
                    properties: true,
                    property_scope: scope,
                    property_schema_by_class: &schema,
                    database: rbx_reflection_database::get().unwrap(),
                };
                let expected = full_path_collection(&document, "Workspace", &filter, &options);
                let mut actual = EditorChangeSet::default();
                append_editor_target_changes(&mut actual, &document, "Workspace", &filter, options);
                assert_eq!(wire_changes(&actual), wire_changes(&expected));
            }
        }
    }

    #[test]
    fn native_plan_materializes_only_exceptions_and_retained_containers() {
        let mut document = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        for (name, class, parent, properties) in [
            ("Workspace", "Workspace", None, json!({"Gravity": 150})),
            (
                "Terrain",
                "Terrain",
                Some(0),
                json!({"WaterTransparency": 0.4}),
            ),
            (
                "Same",
                "Part",
                Some(0),
                json!({"Transparency": 0.5, "Reflectance": 0.2}),
            ),
            (
                "Same",
                "Part",
                Some(0),
                json!({"Transparency": 0.6, "Reflectance": 0.3}),
            ),
            (
                "Car",
                "Model",
                Some(0),
                json!({"WorldPivot": {"_type":"CFrame", "components":[1,2,3,1,0,0,0,1,0,0,0,1]}}),
            ),
            (
                "Logic",
                "ModuleScript",
                Some(0),
                json!({"Source":"return 42"}),
            ),
        ] {
            let mut instance = SettingsBytecodeInstance::new(
                document.instances.len().to_string(),
                name.into(),
                class.into(),
                parent,
            );
            instance.properties = properties.as_object().unwrap().clone();
            instance.attributes.insert("Kept".into(), json!(true));
            document.instances.push(instance);
        }
        fn run(
            document: &SettingsBytecode,
            scope: EditorPropertyScope<'_>,
            upsert_instances: bool,
        ) -> EditorChangeSet {
            let mut changes = EditorChangeSet::default();
            append_editor_target_changes(
                &mut changes,
                document,
                "Workspace",
                &EditorPropertyFilter::default(),
                EditorTargetChangeOptions {
                    upsert_instances,
                    properties: true,
                    property_scope: scope,
                    property_schema_by_class: &HashMap::new(),
                    database: rbx_reflection_database::get().unwrap(),
                },
            );
            changes
        }
        let all = run(&document, EditorPropertyScope::All, true);
        let roots = run(&document, EditorPropertyScope::RetainedContainers, true);
        assert!(roots.instance_changes.is_empty());
        assert_eq!(
            serde_json::to_value(&roots.source_changes).unwrap(),
            serde_json::to_value(&all.source_changes).unwrap()
        );
        assert_eq!(roots.property_changes.len(), 2);
        assert_eq!(roots.property_changes[0].properties["Gravity"], 150);
        assert_eq!(roots.property_changes[1].attributes["Kept"], true);

        // If no binary plan can be used, the original complete property plan
        // remains representable without silently losing imported fields.
        let rest = run(&document, EditorPropertyScope::Imported(None), false);
        let mut reconstructed = roots.property_changes;
        reconstructed.extend(rest.property_changes);
        assert_eq!(
            serde_json::to_value(reconstructed).unwrap(),
            serde_json::to_value(&all.property_changes).unwrap()
        );

        let segments = vec!["Workspace".into(), "Same".into()];
        let mut import = EditorBinaryImport {
            bytes: Vec::new(),
            native_replacement: None,
            instance_count: 6,
            external_references_post_applied: true,
            viewport_references_post_applied: false,
            groups: vec![EditorBinaryImportGroup {
                service: "Workspace".into(),
                additive: false,
                target_path: vec!["Workspace".into()],
                count: 5,
                payload_root_name: "payload".into(),
                expected_structure: None,
                root_paths: Vec::new(),
                viewport_camera: None,
                retained_roots: Vec::new(),
                package_roots: Vec::new(),
                mutation_package_roots: Vec::new(),
                change_generation: Some(0),
            }],
            post_apply_properties_by_class: HashMap::from([(
                "Part".into(),
                HashSet::from(["Transparency".into()]),
            )]),
            post_apply_properties_by_path: HashMap::from([(
                instance_path_parts_key(&segments, &[1, 2]),
                HashSet::from(["Reflectance".into()]),
            )]),
        };
        let supplemental = run(
            &document,
            EditorPropertyScope::Imported(Some(&import)),
            false,
        );
        assert_eq!(supplemental.property_changes.len(), 3);
        assert_eq!(
            supplemental.property_changes[0].properties,
            Map::from_iter([("Transparency".into(), json!(0.5))])
        );
        assert_eq!(
            supplemental.property_changes[1].properties,
            Map::from_iter([
                ("Transparency".into(), json!(0.6)),
                ("Reflectance".into(), json!(0.3))
            ])
        );
        assert!(
            supplemental.property_changes[2]
                .properties
                .contains_key("WorldPivot")
        );
        assert!(
            supplemental
                .property_changes
                .iter()
                .all(|change| change.attributes.is_empty())
        );
        import.groups[0]
            .retained_roots
            .push(EditorBinaryRetainedRoot {
                path_segments: segments,
                path_ordinals: vec![1, 2],
                class_name: "Part".into(),
                payload_index: 3,
                instance_count: 1,
                payload_omitted: true,
            });
        assert_eq!(
            run(
                &document,
                EditorPropertyScope::Imported(Some(&import)),
                false
            )
            .property_changes
            .len(),
            2
        );
    }
}
