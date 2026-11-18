use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use super::types::{NativeSettingsProperty, NativeSettingsValue};
use crate::system::LockRecover;

/// Complete value envelopes are shared without changing their serialized form.
/// This pool lives only as long as one service conversion.
#[derive(Default)]
pub(crate) struct NativeValuePool(Mutex<HashSet<Arc<Value>>>);

impl NativeValuePool {
    pub(crate) fn compact(
        &self,
        properties: &mut Map<String, Value>,
        native: &mut Vec<NativeSettingsProperty>,
        keep_json: impl Fn(&str) -> bool,
    ) {
        properties.retain(|name, value| {
            if keep_json(name) || !shareable_value(value) {
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

fn shareable_value(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    // These are Renium's value-envelope keys, not engine property names.
    // Unknown envelopes and content references retain their JSON path.
    match object.get("_type").and_then(Value::as_str) {
        Some("EnumItem") => object.iter().all(|(key, value)| match key.as_str() {
            "_type" | "name" | "enumType" => value.is_string(),
            "value" => value.is_number(),
            _ => false,
        }),
        Some("Float") => {
            object.len() == 2
                && matches!(
                    object.get("value").and_then(Value::as_str),
                    Some("nan" | "inf" | "-inf")
                )
        }
        Some("Content") => object.len() == 2 && object.get("value").is_some_and(Value::is_string),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_sharing_keeps_the_entire_value_and_unknown_envelopes() {
        let pool = NativeValuePool::default();
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

    #[test]
    fn scalar_envelopes_keep_their_values_and_reject_unknown_shapes() {
        let pool = NativeValuePool::default();
        let values = [
            json!({"_type":"Float","value":"nan"}),
            json!({"_type":"Float","value":"inf"}),
            json!({"_type":"Float","value":"-inf"}),
            json!({"_type":"Content","value":""}),
            json!({"_type":"Content","value":"rbxassetid://123"}),
            json!({"_type":"Content","value":"inf"}),
        ];
        let mut shared = Vec::new();
        for value in values {
            let mut rows = Vec::new();
            for _ in 0..2 {
                let mut properties = Map::from_iter([("FutureProperty".into(), value.clone())]);
                pool.compact(&mut properties, &mut rows, |_| false);
                assert!(properties.is_empty());
                // Each row belongs to another instance.
                let NativeSettingsValue::Shared(held) = rows.pop().unwrap().value else {
                    panic!("shared value")
                };
                assert_eq!(held.as_ref(), &value);
                shared.push(held);
            }
            let end = shared.len();
            assert!(Arc::ptr_eq(&shared[end - 2], &shared[end - 1]));
        }
        for (index, value) in shared.iter().step_by(2).enumerate() {
            assert!(
                shared
                    .iter()
                    .step_by(2)
                    .skip(index + 1)
                    .all(|other| !Arc::ptr_eq(value, other))
            );
        }
        for value in [
            json!({"_type":"Float","value":7}),
            json!({"_type":"Float","value":"future"}),
            json!({"_type":"Float","value":"inf","future":true}),
            json!({"_type":"Content"}),
            json!({"_type":"Content","value":7}),
            json!({"_type":"Content","value":"","future":true}),
            json!({"_type":"Content","value":{"_type":"Ref","instanceIndex":2}}),
        ] {
            let mut properties = Map::from_iter([("FutureProperty".into(), value)]);
            let before = properties.clone();
            let mut rows = Vec::new();
            pool.compact(&mut properties, &mut rows, |_| false);
            assert_eq!(properties, before);
            assert!(rows.is_empty());
        }
    }
}
