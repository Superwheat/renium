use super::*;
use crate::tests::support::{SimRng, settings_document, settings_instance};

pub(super) fn encode_oracle(document: &SettingsBytecode) -> Result<Vec<u8>> {
    validate_settings_hierarchy(&document.instances)?;
    validate_settings_record_properties(&document.instances)?;
    let canonical = canonical_attribute_document(document);
    let document = canonical.as_ref().unwrap_or(document);
    let settled = forced_properties_settled_document(document);
    let document = settled.as_ref().unwrap_or(document);
    let lookup = build_bytecode_instance_lookup_oracle(document);
    wrap_settings_bytecode_payload(&encode_settings_bytecode_payload_using_lookup(
        document, &lookup,
    )?)
}

fn generated(seed: u64) -> SettingsBytecode {
    let mut rng = SimRng(seed);
    let count = if seed < 2 { 2_052 } else { 4 + rng.below(35) };
    let root_name = ["Workspace", "Service", "", "Unicode-root"][rng.below(4)];
    let mut instances = vec![settings_instance("root", root_name, "Folder", None)];
    for index in 1..count {
        let id = format!("id:{index}");
        let name = if rng.below(3) == 0 {
            "duplicate".into()
        } else {
            format!("child-{index}")
        };
        let mut instance = settings_instance(&id, &name, "Part", Some(rng.below(index)));
        instance
            .properties
            .insert("Value".into(), json!(rng.below(500)));
        instances.push(instance);
    }
    if seed.is_multiple_of(2) {
        instances[1].parent_index = Some(2);
        instances[2].parent_index = Some(0);
    }
    settings_document(instances)
}

fn compare(document: &SettingsBytecode) {
    match (encode_oracle(document), encode_settings_bytecode(document)) {
        (Ok(expected), Ok(actual)) => assert_eq!(expected, actual),
        (Err(expected), Err(actual)) => assert_eq!(format!("{expected:#}"), format!("{actual:#}")),
        _ => panic!("encoder acceptance differs"),
    }
}

#[test]
fn local_indexes_match_the_unfiltered_encoder_on_seeded_references() {
    for seed in 0..48 {
        let original = generated(seed);
        let root = original.instances[0].name.clone();
        let outside = format!("{root}-outside");
        let references = [
            json!({"_type":"Ref", "pathSegments":[outside,"child"], "settingsId":"id:1", "instanceIndex":2}),
            json!({"_type":"Ref", "pathSegments":[root], "settingsId":"root", "instanceIndex":1}),
            json!({"_type":"Ref", "settingsId":"id:1"}),
            json!({"Ref":{"pathSegments":[outside,"duplicate"],"pathOrdinals":[1,2],"referent":"id:1"}}),
            json!({"_type":"Ref", "pathSegments":[root,"duplicate"]}),
            json!({"_type":"Ref", "pathSegments":[outside], "pathOrdinals":[0]}),
            json!({"_type":"Ref", "pathSegments":[outside,7]}),
            json!({"_type":"Ref", "pathOrdinals":[1]}),
            json!({"_type":"Ref", "pathSegments":[], "settingsId":"id:1"}),
            json!({"_type":"Ref", "path":"unresolved", "settingsId":"id:1"}),
        ];
        for reference in references {
            let mut document = original.clone();
            document.instances[0]
                .properties
                .insert("Nested".into(), json!({"list":[0,reference]}));
            compare(&document);
            let before = settings_document_reference_selectors_oracle(&document);
            let no_root = settings_document_reference_selectors(&document, None);
            assert_eq!(
                (before.path, before.settings_id),
                (no_root.path, no_root.settings_id)
            );
            document.instances[1]
                .properties
                .insert("Local".into(), json!({"_type":"Ref","settingsId":"id:2"}));
            compare(&document);
        }
        let mut external = original;
        external.instances[0].properties.insert(
            "Target".into(),
            json!({"_type":"Ref", "pathSegments":[outside], "settingsId":"root"}),
        );
        let lookup = build_bytecode_instance_lookup(&external);
        assert!(
            lookup.by_path.is_empty()
                && lookup.by_path_parts.is_empty()
                && lookup.by_path_segments.is_empty()
                && lookup.by_settings_id.is_empty()
        );
        compare(&external);
    }
}

fn build_bytecode_instance_lookup_oracle(
    document: &SettingsBytecode,
) -> SettingsBinaryInstanceLookup<'_> {
    let selectors = settings_document_reference_selectors_oracle(document);
    let mut lookup = SettingsBinaryInstanceLookup {
        dense_instance_count: document.instances.len(),
        dense_instances: Some(&document.instances),
        root_service: document_root_service(document),
        by_settings_id: if selectors.settings_id {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        by_path: if selectors.path {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        by_path_segments: if selectors.path {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        by_path_parts: if selectors.path {
            AHashMap::with_capacity(document.instances.len())
        } else {
            AHashMap::new()
        },
        ..Default::default()
    };
    let paths = selectors
        .path
        .then(|| settings_document_path_parts(document));
    for (index, instance) in document.instances.iter().enumerate() {
        if selectors.settings_id {
            lookup
                .by_settings_id
                .entry(Cow::Borrowed(instance.settings_id.as_str()))
                .or_insert(index);
        }
        if let Some((segments, ordinals)) = paths.as_ref().and_then(|paths| paths.get(index)) {
            insert_unique_path(
                &mut lookup.by_path_segments,
                path_segments_key(segments),
                index,
            );
            insert_unique_path(
                &mut lookup.by_path_parts,
                path_parts_key(segments, ordinals),
                index,
            );
            insert_unique_path(&mut lookup.by_path, segments.join("."), index);
        }
    }
    lookup
}

fn settings_document_reference_selectors_oracle(
    document: &SettingsBytecode,
) -> SettingsReferenceSelectors {
    let mut selectors = SettingsReferenceSelectors::default();
    for instance in &document.instances {
        for value in instance
            .properties
            .values()
            .chain(instance.attributes.values())
        {
            collect_settings_reference_selectors_oracle(value, &mut selectors);
            if selectors.settings_id && selectors.path {
                return selectors;
            }
        }
    }
    selectors
}

fn collect_settings_reference_selectors_oracle(
    value: &Value,
    selectors: &mut SettingsReferenceSelectors,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_settings_reference_selectors_oracle(value, selectors);
            }
        }
        Value::Object(object) => {
            if let Some(reference) = ref_payload_object(value) {
                selectors.settings_id |= reference.get("instanceIndex").is_none()
                    && ["settingsId", "referent", "ref"]
                        .iter()
                        .any(|key| reference.contains_key(*key));
                selectors.path |= ["path", "pathSegments", "pathOrdinals"]
                    .iter()
                    .any(|key| reference.contains_key(*key));
            } else {
                for value in object.values() {
                    collect_settings_reference_selectors_oracle(value, selectors);
                }
            }
        }
        _ => {}
    }
}
