use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::settings::bytecode::{
    SettingsBytecode, reference_leads_out_of, visit_reference_objects_mut,
};

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

pub(crate) fn stabilize_record_references(record: &mut Map<String, Value>, ids: &[String]) {
    rewrite_record_reference_ids(record, Some(ids), None, false, None);
}

/// The service a document is the store of: the name of its root, when it
/// has one root and that root is a service. A model or a whole place is no
/// service's store, and every id in it is its own.
pub(crate) fn store_service(document: &SettingsBytecode) -> Option<&str> {
    let mut roots = document
        .instances
        .iter()
        .filter(|instance| instance.parent_index.is_none());
    let root = roots.next()?;
    if roots.next().is_some() {
        return None;
    }
    rbx_reflection_database::get()
        .ok()?
        .classes
        .get(root.class_name.as_str())?
        .tags
        .contains(&rbx_reflection::ClassTag::Service)
        .then_some(root.name.as_str())
}

/// Renames settings ids in the references of a record of one document.
/// `service` is the service the document is the store of, as `store_service`
/// gives it. Settings ids are unique within one store only: a reference
/// whose path leads into another service names an instance of that service's
/// store and keeps its id. Without a service every id is the document's own.
pub(crate) fn remap_record_reference_ids(
    record: &mut Map<String, Value>,
    ids: &HashMap<String, String>,
    service: Option<&str>,
) {
    rewrite_record_reference_ids(record, None, Some(ids), false, service);
}

/// `remap_record_reference_ids` for a record of the store of `service`.
pub(crate) fn remap_store_reference_ids(
    record: &mut Map<String, Value>,
    ids: &HashMap<String, String>,
    service: &str,
) {
    remap_record_reference_ids(record, ids, Some(service));
}

/// `stabilize_record_references` and `remap_record_reference_ids` in one
/// pass over a record of the document `service` is the store of.
pub(crate) fn remap_and_stabilize_record_references(
    record: &mut Map<String, Value>,
    ids: &[String],
    remap: &HashMap<String, String>,
    keep_instance_indices: bool,
    service: Option<&str>,
) {
    rewrite_record_reference_ids(
        record,
        Some(ids),
        Some(remap),
        keep_instance_indices,
        service,
    );
}

fn rewrite_record_reference_ids(
    record: &mut Map<String, Value>,
    indices: Option<&[String]>,
    remap: Option<&HashMap<String, String>>,
    keep_instance_indices: bool,
    store_service: Option<&str>,
) {
    visit_reference_objects_mut(record, |object| {
        let has_text_id = ["settingsId", "instanceId"]
            .into_iter()
            .any(|key| object.get(key).and_then(Value::as_str).is_some());
        if let Some(remap) = remap
            && !store_service.is_some_and(|service| reference_leads_out_of(object, service))
        {
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
