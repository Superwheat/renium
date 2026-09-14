use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::settings::bytecode::{SettingsBytecode, visit_reference_objects_mut};

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
