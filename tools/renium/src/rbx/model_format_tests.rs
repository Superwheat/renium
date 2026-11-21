use super::model::{
    RbxPlaceFormat, rbx_dom_instance_by_path_unique, rbx_dom_instance_path_parts,
    rbx_dom_path_import_refs,
};
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use serde_json::json;

#[test]
fn indexed_paths_match_sibling_scans_for_generated_hierarchies() {
    use super::model::RbxDomPaths;
    use crate::tests::support::{SimRng, run_seeds};
    run_seeds(128, |seed, _| {
        let mut rng = SimRng(seed);
        let mut dom = WeakDom::new(
            InstanceBuilder::new(["DataModel", "Workspace", "Folder"][rng.below(3)])
                .with_name("Root"),
        );
        let mut ids = vec![dom.root_ref()];
        for index in 0..192 {
            let parent = if index % 3 == 0 {
                0
            } else {
                rng.below(ids.len())
            };
            let class = ["Folder", "Part", "Workspace", "ServerStorage"][rng.below(4)];
            let name = ["", "a", "a", "a.b", "A", "世界"][rng.below(6)];
            ids.push(dom.insert(ids[parent], InstanceBuilder::new(class).with_name(name)));
        }
        for round in 0..2 {
            rng.shuffle(&mut ids);
            let mut paths = RbxDomPaths::new(&dom);
            for id in &ids {
                let expected = rbx_dom_instance_path_parts(&dom, *id);
                if paths.parts(*id) != expected || paths.parts(*id) != expected {
                    return Err(format!("path differs in round {round}"));
                }
            }
            drop(paths);
            // An index borrows the hierarchy, so edits require a new index.
            for id in ids
                .iter()
                .filter(|id| **id != dom.root_ref())
                .copied()
                .take(8)
                .collect::<Vec<_>>()
            {
                dom.get_by_ref_mut(id).unwrap().name = "changed".into();
                dom.transfer_within(id, dom.root_ref());
            }
        }
        Ok(())
    });
}

fn external_target(dom: &WeakDom) -> Ref {
    let reference = dom
        .descendants()
        .find(|node| node.name == "External")
        .unwrap();
    let Variant::Ref(target) = reference.properties[&"Value".into()] else {
        panic!("ObjectValue.Value is not a reference");
    };
    target
}

#[test]
fn renamed_service_raw_dom_path_parts_use_class_and_descendant_ordinals() {
    let dom = crate::tests::support::renamed_service_place();
    let (segments, ordinals) = rbx_dom_instance_path_parts(&dom, external_target(&dom));
    assert_eq!(segments, ["ServerStorage", "InService", "Renamed"]);
    assert_eq!(ordinals, [1, 1, 2]);
    let service = dom
        .descendants()
        .find(|node| node.class == "ServerStorage")
        .unwrap();
    assert_eq!(
        service.name, "Vault",
        "path construction must not rename the DOM"
    );
}

#[test]
fn renamed_service_raw_dom_import_refs_encode_canonical_paths() {
    let dom = crate::tests::support::renamed_service_place();
    let target = external_target(&dom);
    for include_indices in [false, true] {
        let refs = rbx_dom_path_import_refs(&dom, include_indices);
        let value = crate::rbx::decode::rbx_variant_to_persisted_settings_json(
            &Variant::Ref(target),
            None,
            rbx_reflection_database::get().unwrap(),
            &refs,
        )
        .unwrap();
        assert_eq!(
            value["pathSegments"],
            json!(["ServerStorage", "InService", "Renamed"])
        );
        assert_eq!(value["pathOrdinals"], json!([1, 1, 2]));
        assert_eq!(value.get("instanceIndex").is_some(), include_indices);
        if include_indices {
            let index = refs.new_index_by_ref[&target];
            assert_eq!(
                refs.path_segments_by_index[index].as_ref(),
                refs.path_segments_by_ref.get(&target)
            );
        }
    }
}

#[test]
fn renamed_service_raw_dom_lookup_and_protected_writes_ignore_namesake() {
    use crate::editor::review::{
        patch_place_protected_writes, protected_write_rows_with_previous_values,
    };
    let dom = crate::tests::support::renamed_service_place();
    let segments = ["ServerStorage".into(), "InService".into(), "Renamed".into()];
    assert_eq!(
        rbx_dom_instance_by_path_unique(&dom, &segments, &[1, 1, 2]).unwrap(),
        external_target(&dom)
    );
    assert!(rbx_dom_instance_by_path_unique(&dom, &segments, &[2, 1, 2]).is_err());
    assert!(rbx_dom_instance_by_path_unique(&dom, &segments, &[0, 1, 2]).is_err());
    assert!(rbx_dom_instance_by_path_unique(&dom, &segments, &[1, 1, 3]).is_err());
    let directory = crate::tests::support::temp_dir("renamed-service-writes");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&directory);
    });
    for format in [RbxPlaceFormat::Binary, RbxPlaceFormat::Xml] {
        let path = directory.join(format!("place.{}", format.label()));
        format.write(&path, &dom, dom.root().children()).unwrap();
        let rows = vec![json!({"kind": "property", "pathSegments": segments,
            "pathOrdinals": [1, 1, 2], "name": "Anchored", "value": false})];
        assert_eq!(
            protected_write_rows_with_previous_values(&path, &rows).unwrap()[0]["oldValue"],
            true
        );
        assert_eq!(patch_place_protected_writes(&path, &rows).unwrap(), 1);
        let read = format.read(&path).unwrap();
        assert_eq!(
            read.get_by_ref(external_target(&read)).unwrap().properties[&"Anchored".into()],
            false.into()
        );
        let namesake = read
            .descendants()
            .find(|node| node.class == "Folder" && node.name == "ServerStorage")
            .unwrap();
        assert_eq!(
            namesake.properties[&"SourceAssetId".into()],
            Variant::Int64(99)
        );
        assert_eq!(
            read.descendants()
                .find(|node| node.class == "ServerStorage")
                .unwrap()
                .name,
            "Vault"
        );
    }
    let mut absent = crate::tests::support::renamed_service_place();
    let service = absent
        .descendants()
        .find(|node| node.class == "ServerStorage")
        .unwrap()
        .referent();
    absent.destroy(service);
    assert!(
        rbx_dom_instance_by_path_unique(&absent, &segments, &[1, 1, 2]).is_err(),
        "a namesake is not a missing service"
    );
}

#[test]
fn renamed_service_detached_roots_and_ordinary_models_keep_path_semantics() {
    for (class, name, expected) in [
        ("ServerStorage", "Vault", "ServerStorage"),
        ("Folder", "ServerStorage", "ServerStorage"),
        ("Folder", "Scene", "Scene"),
        ("Model", "Custom", "Custom"),
    ] {
        let dom = WeakDom::new(
            InstanceBuilder::new(class).with_name(name).with_child(
                InstanceBuilder::new("Folder")
                    .with_name("ServerStorage")
                    .with_child(InstanceBuilder::new("Part").with_name("Item")),
            ),
        );
        let target = dom
            .descendants()
            .find(|node| node.name == "Item")
            .unwrap()
            .referent();
        let (segments, ordinals) = rbx_dom_instance_path_parts(&dom, target);
        assert_eq!(segments, [expected, "ServerStorage", "Item"]);
        assert_eq!(ordinals, [1, 1, 1]);
        let refs = rbx_dom_path_import_refs(&dom, true);
        assert_eq!(refs.path_segments_by_ref[&target], segments);
        assert_eq!(refs.path_ordinals_by_ref[&target], ordinals);
        // Reading a saved model supplies a DataModel wrapper. Only its first
        // segment may select a service; the nested Folder keeps its Name.
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[dom.root_ref()]).unwrap();
        let saved = rbx_binary::from_reader(bytes.as_slice()).unwrap();
        let resolved = rbx_dom_instance_by_path_unique(&saved, &segments, &ordinals);
        if class == "Folder" && name == "ServerStorage" {
            assert!(
                resolved.is_err(),
                "a reserved service key cannot select a Folder"
            );
        } else {
            let target = saved
                .descendants()
                .find(|node| node.name == "Item")
                .unwrap();
            assert_eq!(resolved.unwrap(), target.referent());
        }
    }
}

#[test]
fn a_place_is_read_by_its_content_not_its_name() {
    let dir = std::env::temp_dir().join(format!(
        "renium-place-format-{}",
        crate::automation::authorization::random_id().unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dom = WeakDom::new(
        InstanceBuilder::new("DataModel").with_child(
            InstanceBuilder::new("Workspace")
                .with_child(InstanceBuilder::new("Part").with_name("Marker")),
        ),
    );
    let roots = [dom.root_ref()];
    let xml_named_binary = dir.join("place.rbxl");
    RbxPlaceFormat::Xml
        .write(&xml_named_binary, &dom, &roots)
        .unwrap();
    let binary_named_xml = dir.join("place.rbxlx");
    RbxPlaceFormat::Binary
        .write(&binary_named_xml, &dom, &roots)
        .unwrap();
    for path in [&xml_named_binary, &binary_named_xml] {
        let read = RbxPlaceFormat::from_path(path).unwrap().read(path).unwrap();
        assert!(
            read.descendants().any(|instance| instance.name == "Marker"),
            "{}",
            path.display()
        );
    }
    assert_eq!(
        RbxPlaceFormat::sniff(b"<roblox!\x89\xff"),
        Some(RbxPlaceFormat::Binary)
    );
    assert_eq!(
        RbxPlaceFormat::sniff(b"\xef\xbb\xbf  <!-- Saved by a tool -->\n<roblox"),
        Some(RbxPlaceFormat::Xml)
    );
    assert_eq!(RbxPlaceFormat::sniff(b"not a place"), None);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Numbers brought to their declared width by a lookup of every value.
fn coerce_by_every_lookup(dom: &mut WeakDom) {
    use rbx_dom_weak::types::VariantType;
    use rbx_reflection::DataType;
    let database = rbx_reflection_database::get().unwrap();
    let referents = dom
        .descendants()
        .map(|instance| instance.referent())
        .collect::<Vec<_>>();
    for referent in referents {
        let instance = dom.get_by_ref_mut(referent).unwrap();
        let class_name = instance.class;
        for (name, value) in instance.properties.iter_mut() {
            let Some(DataType::Value(declared)) =
                super::encode::rbx_property_descriptor(database, &class_name, name)
                    .map(|descriptor| &descriptor.data_type)
            else {
                continue;
            };
            *value = match (&*value, declared) {
                (Variant::Float64(number), VariantType::Float32) => {
                    Variant::Float32(*number as f32)
                }
                (Variant::Float32(number), VariantType::Float64) => {
                    Variant::Float64(f64::from(*number))
                }
                (Variant::Int64(number), VariantType::Int32) => Variant::Int32(*number as i32),
                (Variant::Int32(number), VariantType::Int64) => Variant::Int64(i64::from(*number)),
                _ => continue,
            };
        }
    }
}

/// Places of every class's numbers at both widths, numbers of no declared
/// property and other values come out of the save's width coercion as out
/// of a lookup of every value, and some numbers change width.
#[test]
fn numbers_take_their_declared_width_as_a_lookup_of_every_value_gives() {
    use crate::tests::support::{SimRng, run_seeds};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let generate = |seed: u64| {
        let mut rng = SimRng(seed);
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let mut ids = vec![dom.root_ref()];
        for _ in 0..200 {
            let class = [
                "Workspace",
                "Part",
                "MeshPart",
                "IntValue",
                "NumberValue",
                "Sound",
                "Folder",
                "NotAClass",
            ][rng.below(8)];
            let mut builder = InstanceBuilder::new(class);
            for name in [
                "Transparency",
                "Reflectance",
                "Value",
                "Volume",
                "TimePosition",
                "Gravity",
                "StreamingMinRadius",
                "StreamingTargetRadius",
                "NotAProperty",
            ] {
                let number = rng.below(1 << 20);
                let value = match rng.below(8) {
                    0 => Variant::Float64(number as f64 / 7.0),
                    1 => Variant::Float32(number as f32 / 7.0),
                    2 => Variant::Int64(number as i64 - (1 << 19)),
                    3 => Variant::Int32(number as i32 - (1 << 19)),
                    4 => Variant::Bool(number.is_multiple_of(2)),
                    5 => Variant::String(number.to_string()),
                    _ => continue,
                };
                builder = builder.with_property(name, value);
            }
            let parent = ids[rng.below(ids.len())];
            ids.push(dom.insert(parent, builder));
        }
        dom
    };
    let changed = AtomicUsize::new(0);
    run_seeds(64, |seed, _| {
        let (mut expected, mut found) = (generate(seed), generate(seed));
        coerce_by_every_lookup(&mut expected);
        super::model::coerce_numeric_property_widths(&mut found);
        let before = generate(seed);
        for ((expected, found), before) in expected
            .descendants()
            .zip(found.descendants())
            .zip(before.descendants())
        {
            if expected.properties != found.properties {
                return Err(format!(
                    "{} {:?} took other widths",
                    expected.class, expected.properties
                ));
            }
            changed.fetch_add(
                before
                    .properties
                    .iter()
                    .filter(|(name, value)| expected.properties.get(*name) != Some(*value))
                    .count(),
                Ordering::Relaxed,
            );
        }
        Ok(())
    });
    assert!(
        changed.load(Ordering::Relaxed) > 0,
        "no number changed width"
    );
}
