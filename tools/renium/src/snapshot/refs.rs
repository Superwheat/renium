use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::editor::sync::is_lua_source_class;
use crate::project::config;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::{
    SettingsBytecode, reindex_reference_indices, visit_reference_objects_mut,
};
use crate::snapshot::types::SnapshotInstance;

pub(crate) fn settings_instance_path(document: &SettingsBytecode, index: usize) -> String {
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

pub(crate) fn settings_document_as_snapshot_instances(
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
                    Value::String(EXTERNAL_SOURCE_MARKER.to_string()),
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

pub(crate) fn stabilize_snapshot_references(instances: &mut [SnapshotInstance], ids: &[String]) {
    for instance in instances {
        stabilize_record_references(&mut instance.properties, ids);
        stabilize_record_references(&mut instance.attributes, ids);
    }
}

pub(crate) fn reindex_snapshot_references(
    instance: &mut SnapshotInstance,
    indices: &HashMap<String, usize>,
) {
    reindex_reference_indices(&mut instance.properties, indices);
    reindex_reference_indices(&mut instance.attributes, indices);
}

pub(crate) fn stabilize_record_references(record: &mut Map<String, Value>, ids: &[String]) {
    rewrite_record_reference_ids(record, Some(ids), None, false);
}

pub(crate) fn remap_record_reference_ids(
    record: &mut Map<String, Value>,
    ids: &HashMap<String, String>,
) {
    rewrite_record_reference_ids(record, None, Some(ids), false);
}

pub(crate) fn remap_and_stabilize_record_references(
    record: &mut Map<String, Value>,
    ids: &[String],
    remap: &HashMap<String, String>,
    keep_instance_indices: bool,
) {
    rewrite_record_reference_ids(record, Some(ids), Some(remap), keep_instance_indices);
}

fn rewrite_record_reference_ids(
    record: &mut Map<String, Value>,
    indices: Option<&[String]>,
    remap: Option<&HashMap<String, String>>,
    keep_instance_indices: bool,
) {
    visit_reference_objects_mut(record, |object| {
        let has_text_id = ["settingsId", "instanceId"]
            .into_iter()
            .any(|key| object.get(key).and_then(Value::as_str).is_some());
        if let Some(remap) = remap {
            for key in ["settingsId", "instanceId"] {
                if let Some(current) = object.get(key).and_then(Value::as_str)
                    && let Some(next) = remap.get(current)
                {
                    object.insert(key.to_string(), Value::String(next.clone()));
                }
            }
        }
        if let Some(ids) = indices {
            if !has_text_id
                && let Some(id) = object
                    .get("instanceIndex")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|index| ids.get(index))
            {
                object.insert("settingsId".to_string(), Value::String(id.clone()));
            }
            if !keep_instance_indices {
                object.remove("instanceIndex");
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

pub(crate) fn syncback_filter_allows_instance(
    filters: &[config::FilterRule],
    current: Option<&SnapshotInstance>,
    baseline: Option<&SnapshotInstance>,
) -> Result<bool> {
    syncback_filter_allows_pair(filters, current, baseline, SyncbackFilterScope::Instance)
}

fn syncback_filter_allows_pair(
    filters: &[config::FilterRule],
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
    filters: &[config::FilterRule],
    instance: &SnapshotInstance,
    scope: SyncbackFilterScope<'_>,
) -> Result<bool> {
    let fields = config::filter_candidate_fields(&instance.properties, &instance.attributes);
    let candidate = fields.candidate(
        instance.instance_id.as_deref().unwrap_or(""),
        &instance.path,
        &instance.name,
        &instance.class_name,
    );
    match scope {
        SyncbackFilterScope::Instance => config::filter_allows_instance(
            filters,
            config::FilterDirection::StudioToFiles,
            &candidate,
        ),
        SyncbackFilterScope::Property(property) => config::filter_allows_property(
            filters,
            config::FilterDirection::StudioToFiles,
            &candidate,
            property,
        ),
        SyncbackFilterScope::Attribute(attribute) => config::filter_allows_attribute(
            filters,
            config::FilterDirection::StudioToFiles,
            &candidate,
            attribute,
        ),
    }
}

pub(crate) fn merge_syncback_instance_fields(
    filters: &[config::FilterRule],
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

pub(crate) fn snapshot_service_exists(snapshot_dir: &Path, service: &str) -> bool {
    snapshot_dir.join(service).exists()
        || snapshot_dir.join(format!("{service}.json")).exists()
        || snapshot_dir
            .join(format!("manifest-{service}.json"))
            .exists()
}
