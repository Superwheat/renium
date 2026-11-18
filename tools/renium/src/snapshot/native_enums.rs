use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use super::types::{NativeSettingsProperty, NativeSettingsValue};
use crate::system::LockRecover;

/// The complete serialized enum value is shared, including its type and numeric
/// value when present. This pool lives only as long as one service conversion.
#[derive(Default)]
pub(crate) struct NativeEnums(Mutex<HashSet<Arc<Value>>>);

impl NativeEnums {
    pub(crate) fn compact(
        &self,
        properties: &mut Map<String, Value>,
        native: &mut Vec<NativeSettingsProperty>,
        keep_json: impl Fn(&str) -> bool,
    ) {
        properties.retain(|name, value| {
            if keep_json(name) || !shareable_enum(value) {
                return true;
            }
            let shared = {
                let mut values = self.0.lock_recover();
                if let Some(value) = values.get(value) {
                    Arc::clone(value)
                } else {
                    let value = Arc::new(std::mem::take(value));
                    values.insert(Arc::clone(&value));
                    value
                }
            };
            // JSON is authoritative over a same-named native row. Preserve
            // that rule when its value moves into the native representation.
            native.retain(|property| property.name != *name);
            native.push(NativeSettingsProperty {
                name: name.as_str().into(),
                value: NativeSettingsValue::Shared(shared),
            });
            false
        });
    }
}

fn shareable_enum(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("_type").and_then(Value::as_str) == Some("EnumItem")
        && object.iter().all(|(key, value)| match key.as_str() {
            "_type" | "name" | "enumType" => value.is_string(),
            "value" => value.is_number(),
            // These are Renium's value-envelope keys, not engine property
            // names. A future envelope keeps its original JSON representation.
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_sharing_keeps_the_entire_value_and_unknown_envelopes() {
        let pool = NativeEnums::default();
        let value = json!({"_type":"EnumItem","enumType":"Enum.Future","name":"Chosen","value":7});
        let mut rows = Vec::new();
        for value in [
            value.clone(),
            value.clone(),
            json!({"_type":"EnumItem","enumType":"Enum.Other","name":"Chosen","value":7}),
        ] {
            let mut properties = Map::from_iter([("FutureProperty".into(), value)]);
            let mut instance_rows = Vec::new();
            pool.compact(&mut properties, &mut instance_rows, |_| false);
            rows.extend(instance_rows);
            assert!(properties.is_empty());
        }
        let shared = |index: usize| match &rows[index].value {
            NativeSettingsValue::Shared(value) => value,
            _ => panic!("shared enum"),
        };
        assert!(Arc::ptr_eq(shared(0), shared(1)));
        assert!(!Arc::ptr_eq(shared(0), shared(2)));
        assert_eq!(shared(0).as_ref(), &value);
        for value in [
            json!({"_type":"EnumItem","name":"Chosen","future":{"path":"Workspace"}}),
            json!({"_type":"EnumItem","name":7}),
            json!({"_type":"EnumItem","name":"Chosen","value":"7"}),
            json!({"_type":"Ref","name":"Chosen"}),
        ] {
            let mut properties = Map::from_iter([("FutureProperty".into(), value)]);
            let before = properties.clone();
            pool.compact(&mut properties, &mut rows, |_| false);
            assert_eq!(properties, before);
        }
        let mut properties = Map::from_iter([("RunContext".into(), value)]);
        let before = properties.clone();
        pool.compact(&mut properties, &mut rows, |_| true);
        assert_eq!(properties, before);
        assert_eq!(rows.len(), 3);
        let mut properties = before;
        let mut native = vec![NativeSettingsProperty {
            name: "RunContext".into(),
            value: NativeSettingsValue::Int(99),
        }];
        pool.compact(&mut properties, &mut native, |_| false);
        assert_eq!(native.len(), 1);
        assert!(matches!(&native[0].value, NativeSettingsValue::Shared(_)));
    }
}
