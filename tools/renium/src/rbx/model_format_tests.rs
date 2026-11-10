use super::model::{
    RbxPlaceFormat, rbx_dom_instance_by_path_unique, rbx_dom_instance_path_parts,
    rbx_dom_path_import_refs,
};
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use serde_json::json;

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
