use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value};

use super::editor_sync::is_lua_source_class;
use super::project_config;
use super::settings_bytecode::{
    SettingsBytecode, reindex_reference_indices, stabilize_reference_objects,
    visit_reference_objects_mut,
};
use super::snapshot_types::SnapshotInstance;

pub(super) fn settings_instance_path(document: &SettingsBytecode, index: usize) -> String {
    let mut parts = Vec::new();
    let mut current = Some(index);
    let mut hops = 0usize;
    while let Some(i) = current {
        if hops > document.instances.len() {
            break;
        }
        let instance = &document.instances[i];
        parts.push(instance.name.clone());
        current = instance.parent_index;
        hops += 1;
    }
    parts.reverse();
    parts.join("/")
}

pub(super) fn settings_document_as_snapshot_instances(
    document: &SettingsBytecode,
) -> Vec<SnapshotInstance> {
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    document
        .instances
        .iter()
        .enumerate()
        .map(|(index, current)| {
            let mut indices = Vec::new();
            let mut cursor = Some(index);
            while let Some(current_index) = cursor {
                indices.push(current_index);
                cursor = document.instances[current_index].parent_index;
            }
            indices.reverse();
            let path_segments = indices
                .iter()
                .map(|current_index| document.instances[*current_index].name.clone())
                .collect::<Vec<_>>();
            let mut properties = current.properties.clone();
            let mut attributes = current.attributes.clone();
            stabilize_record_references(&mut properties, &ids);
            stabilize_record_references(&mut attributes, &ids);
            if is_lua_source_class(&current.class_name) {
                properties.insert(
                    "Source".to_string(),
                    Value::String("__SOURCE_EXTERNAL__".to_string()),
                );
            }
            SnapshotInstance {
                path: path_segments.join("/"),
                path_segments,
                name: current.name.clone(),
                class_name: current.class_name.clone().into(),
                properties,
                parent_path: current
                    .parent_index
                    .map(|parent| settings_instance_path(document, parent)),
                attributes,
                instance_id: Some(current.settings_id.clone()),
                parent_instance_id: current
                    .parent_index
                    .map(|parent| document.instances[parent].settings_id.clone()),
                instance_index: current.parent_index.is_none().then_some(1),
                ..Default::default()
            }
        })
        .collect()
}

pub(super) fn stabilize_snapshot_references(instances: &mut [SnapshotInstance], ids: &[String]) {
    for instance in instances {
        stabilize_record_references(&mut instance.properties, ids);
        stabilize_record_references(&mut instance.attributes, ids);
    }
}

pub(super) fn reindex_snapshot_references(
    instance: &mut SnapshotInstance,
    indices: &HashMap<String, usize>,
) {
    reindex_reference_indices(&mut instance.properties, indices);
    reindex_reference_indices(&mut instance.attributes, indices);
}

pub(super) fn stabilize_record_references(record: &mut Map<String, Value>, ids: &[String]) {
    stabilize_reference_objects(record, |object, index| {
        if let Some(id) = ids.get(index) {
            object.insert("settingsId".to_string(), Value::String(id.clone()));
        }
    });
}

pub(super) fn remap_record_reference_ids(
    record: &mut Map<String, Value>,
    ids: &HashMap<String, String>,
) {
    visit_reference_objects_mut(record, |object| {
        for key in ["settingsId", "instanceId"] {
            if let Some(current) = object.get(key).and_then(Value::as_str)
                && let Some(next) = ids.get(current)
            {
                object.insert(key.to_string(), Value::String(next.clone()));
            }
        }
    });
}

#[derive(Clone, Copy)]
enum SyncbackFilterScope<'a> {
    Instance,
    Property(&'a str),
    Attribute(&'a str),
}

pub(super) fn syncback_filter_allows_instance(
    filters: &[project_config::FilterRule],
    current: Option<&SnapshotInstance>,
    baseline: Option<&SnapshotInstance>,
) -> Result<bool> {
    syncback_filter_allows_pair(filters, current, baseline, SyncbackFilterScope::Instance)
}

fn syncback_filter_allows_pair(
    filters: &[project_config::FilterRule],
    current: Option<&SnapshotInstance>,
    baseline: Option<&SnapshotInstance>,
    scope: SyncbackFilterScope<'_>,
) -> Result<bool> {
    let current_allowed = current
        .map(|instance| syncback_filter_allows_one(filters, instance, scope))
        .transpose()?;
    let baseline_allowed = baseline
        .map(|instance| syncback_filter_allows_one(filters, instance, scope))
        .transpose()?;
    Ok(current_allowed
        .into_iter()
        .chain(baseline_allowed)
        .all(|allowed| allowed))
}

fn syncback_filter_allows_one(
    filters: &[project_config::FilterRule],
    instance: &SnapshotInstance,
    scope: SyncbackFilterScope<'_>,
) -> Result<bool> {
    let fields =
        project_config::filter_candidate_fields(&instance.properties, &instance.attributes);
    let candidate = fields.candidate(
        instance.instance_id.as_deref().unwrap_or(""),
        &instance.path,
        &instance.name,
        &instance.class_name,
    );
    match scope {
        SyncbackFilterScope::Instance => project_config::filter_allows_instance(
            filters,
            project_config::FilterDirection::StudioToFiles,
            &candidate,
        ),
        SyncbackFilterScope::Property(property) => project_config::filter_allows_property(
            filters,
            project_config::FilterDirection::StudioToFiles,
            &candidate,
            property,
        ),
        SyncbackFilterScope::Attribute(attribute) => project_config::filter_allows_attribute(
            filters,
            project_config::FilterDirection::StudioToFiles,
            &candidate,
            attribute,
        ),
    }
}

pub(super) fn merge_syncback_instance_fields(
    filters: &[project_config::FilterRule],
    output: &mut SnapshotInstance,
    current: &SnapshotInstance,
    baseline: &SnapshotInstance,
) -> Result<()> {
    let property_names = current
        .properties
        .keys()
        .chain(baseline.properties.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for property in property_names {
        if syncback_filter_allows_pair(
            filters,
            Some(current),
            Some(baseline),
            SyncbackFilterScope::Property(&property),
        )? {
            continue;
        }
        if let Some(value) = baseline.properties.get(&property) {
            output.properties.insert(property, value.clone());
        } else {
            output.properties.remove(&property);
        }
    }
    let attribute_names = current
        .attributes
        .keys()
        .chain(baseline.attributes.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for attribute in attribute_names {
        if syncback_filter_allows_pair(
            filters,
            Some(current),
            Some(baseline),
            SyncbackFilterScope::Attribute(&attribute),
        )? {
            continue;
        }
        if let Some(value) = baseline.attributes.get(&attribute) {
            output.attributes.insert(attribute, value.clone());
        } else {
            output.attributes.remove(&attribute);
        }
    }
    Ok(())
}

pub(super) fn snapshot_service_exists(snapshot_dir: &Path, service: &str) -> bool {
    snapshot_dir.join(service).exists()
        || snapshot_dir.join(format!("{service}.json")).exists()
        || snapshot_dir
            .join(format!("manifest-{service}.json"))
            .exists()
}
