//! Optional allocation accounting. Kept out of ordinary builds and timings.
use std::collections::{BTreeMap, HashSet};
use std::mem::size_of;

use serde_json::{Map, Value, json};

use super::types::{NativeSettingsProperty, NativeSettingsValue, ServiceState, SnapshotInstance};
use crate::app::profile::measure_drop;

fn spare_value(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity() - text.len(),
        Value::Array(values) => {
            (values.capacity() - values.len()) * size_of::<Value>()
                + values.iter().map(spare_value).sum::<usize>()
        }
        Value::Object(map) => spare_map(map),
        _ => 0,
    }
}

fn spare_map(map: &Map<String, Value>) -> usize {
    map.iter()
        .map(|(key, value)| key.capacity() - key.len() + spare_value(value))
        .sum()
}

fn map_heap(map: &Map<String, Value>) -> usize {
    // BTreeMap cloning preserves the node structure. Strings and arrays clone
    // only their occupied capacity, which the second term restores.
    measure_drop(map.clone()) + spare_map(map)
}

#[derive(Default)]
struct Strings<'a> {
    bytes: usize,
    occupied: usize,
    count: usize,
    unique: HashSet<&'a str>,
}

impl<'a> Strings<'a> {
    fn add(&mut self, text: &'a String) {
        self.bytes += text.capacity();
        self.occupied += text.len();
        self.count += 1;
        self.unique.insert(text.as_str());
    }

    fn report(&self, instances: usize) -> Value {
        let unique_bytes = self.unique.iter().map(|s| s.len()).sum::<usize>();
        json!({
            "heap": self.bytes, "occupied": self.occupied, "present": self.count,
            "unique": self.unique.len(), "uniqueUtf8": unique_bytes,
            "stringHeaders": instances * size_of::<String>(),
            "boxedStrTotal": self.occupied + instances * size_of::<Box<str>>(),
            "arenaWithU32Indices": unique_bytes + (self.unique.len() + 1) * size_of::<u64>()
                + instances * size_of::<u32>(),
        })
    }
}

pub(super) fn measure(service: &str, state: &ServiceState) {
    if std::env::var_os("RENIUM_PROFILE_CAPTURE_FIELDS").is_none() {
        return;
    }
    let mut strings = BTreeMap::<&str, Strings<'_>>::new();
    let mut properties = 0;
    let mut attributes = 0;
    let mut property_count = 0;
    let mut property_kinds = BTreeMap::<String, (usize, usize)>::new();
    let mut key_heap = 0;
    let mut value_heap = 0;
    let mut enums = 0;
    let mut enum_heap = 0;
    let mut enum_names = Strings::default();
    let mut enum_types = Strings::default();
    let mut path_segment_storage = 0;
    let mut native = 0;
    let mut native_slots = 0;
    let mut native_occupied = 0;
    let mut cframes = 0;
    let mut shared = 0;
    let mut shared_seen = HashSet::new();
    for instance in &state.instances {
        properties += map_heap(&instance.properties);
        attributes += map_heap(&instance.attributes);
        property_count += instance.properties.len();
        for (key, value) in &instance.properties {
            key_heap += key.capacity();
            let heap = measure_drop(value.clone()) + spare_value(value);
            value_heap += heap;
            let kind = value
                .get("_type")
                .and_then(Value::as_str)
                .unwrap_or(match value {
                    Value::String(_) => "string",
                    Value::Object(_) => "object",
                    Value::Array(_) => "array",
                    Value::Number(_) => "number",
                    Value::Bool(_) => "bool",
                    Value::Null => "null",
                });
            let total = property_kinds.entry(kind.into()).or_default();
            total.0 += 1;
            total.1 += heap;
            if value.get("_type").and_then(Value::as_str) == Some("EnumItem") {
                enums += 1;
                enum_heap += measure_drop(value.clone()) + spare_value(value);
                if let Some(Value::String(name)) = value.get("name") {
                    enum_names.add(name);
                }
                if let Some(Value::String(name)) = value.get("enumType") {
                    enum_types.add(name);
                }
            }
        }
        for (label, value) in [
            ("name", Some(&instance.name)),
            ("path", Some(&instance.path)),
            ("parentPath", instance.parent_path.as_ref()),
            ("sourceKey", instance.source_key.as_ref()),
            ("debugId", instance.debug_id.as_ref()),
            ("parentDebugId", instance.parent_debug_id.as_ref()),
            ("settingsId", instance.transported_settings_id.as_ref()),
            ("instanceId", instance.instance_id.as_ref()),
            ("parentInstanceId", instance.parent_instance_id.as_ref()),
        ] {
            let category = strings.entry(label).or_default();
            if let Some(value) = value {
                category.add(value);
            }
        }
        path_segment_storage += instance.path_segments.capacity() * size_of::<String>();
        for segment in &instance.path_segments {
            strings.entry("pathSegments").or_default().add(segment);
        }
    }
    if let Some(rows) = &state.native_properties_by_instance {
        native += rows.capacity() * size_of::<Vec<NativeSettingsProperty>>();
        for row in rows {
            native_slots += row.capacity();
            native_occupied += row.len();
            native += row.capacity() * size_of::<NativeSettingsProperty>();
            for property in row {
                native += property.name.capacity();
                match &property.value {
                    NativeSettingsValue::CFrame(value) => {
                        cframes += 1;
                        native += std::mem::size_of_val(value.as_ref());
                    }
                    NativeSettingsValue::String(value)
                    | NativeSettingsValue::Enum(value)
                    | NativeSettingsValue::UniqueId(value) => native += value.capacity(),
                    NativeSettingsValue::Shared(value)
                        if shared_seen.insert(std::sync::Arc::as_ptr(value)) =>
                    {
                        shared += measure_drop(value.as_ref().clone())
                            + spare_value(value)
                            + size_of::<Value>()
                            + 2 * size_of::<usize>();
                    }
                    _ => {}
                }
            }
        }
    }
    let strings = strings
        .into_iter()
        .map(|(name, strings)| (name, strings.report(state.instances.len())))
        .collect::<BTreeMap<_, _>>();
    let report = json!({
        "service": service, "instances": state.instances.len(),
        "instanceRecords": state.instances.capacity() * size_of::<SnapshotInstance>(),
        "propertiesHeap": properties, "propertyCount": property_count, "propertyKeyHeap": key_heap,
        "propertyKinds": property_kinds,
        "propertyValueHeap": value_heap, "propertyMapNodes": properties - key_heap - value_heap,
        "flatPropertySlots": property_count * size_of::<(String, Value)>(),
        "enumCount": enums, "enumHeap": enum_heap,
        "enumNames": enum_names.report(enums), "enumTypes": enum_types.report(enums),
        "attributesHeap": attributes, "strings": strings, "pathSegmentStorage": path_segment_storage,
        "nativeRows": native, "nativeSharedValues": shared,
        "nativeSlots": native_slots, "nativePropertySize": size_of::<NativeSettingsProperty>(),
        "nativeOccupied": native_occupied,
        "cframeCount": cframes,
        "children": state.children_by_index.capacity() * size_of::<Vec<usize>>()
            + state.children_by_index.iter().map(|c| c.capacity()*size_of::<usize>()).sum::<usize>(),
        "sourceBits": state.source_in_subtree.capacity(),
    });
    crate::app::output::log_global(4, format_args!("[renium] capture field memory: {report}"));
}
