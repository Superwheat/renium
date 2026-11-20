use super::*;
use crate::rbx::model::{
    PATH_SIBLING_VISITS, rbx_dom_instance_path_parts, rbx_dom_path_import_refs,
};
use rbx_dom_weak::{
    InstanceBuilder as Builder,
    types::{ContentId, SharedString, UniqueId},
};

fn add(dom: &mut RbxWeakDom, parent: RbxRef, class: &str, name: &str) -> RbxRef {
    dom.insert(parent, Builder::new(class).with_name(name))
}

fn encode(dom: &RbxWeakDom, roots: &[RbxRef]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, dom, roots)?;
    Ok(bytes)
}

fn fixture() -> Result<(Vec<u8>, EditorBinaryExport)> {
    let mut dom = RbxWeakDom::new(Builder::new("DataModel"));
    let root = dom.root_ref();
    let workspace = add(&mut dom, root, "Workspace", "Workspace");
    let storage = add(&mut dom, root, "ReplicatedStorage", "ReplicatedStorage");
    let lighting = add(&mut dom, root, "Lighting", "Lighting");
    let mut parts = Vec::new();
    for value in [0.25_f32, 0.75] {
        let folder = add(&mut dom, workspace, "Folder", "same");
        let builder = Builder::new("Part")
            .with_property("Transparency", value)
            .with_property("Anchored", false)
            .with_property("UniqueId", UniqueId::new(parts.len() as u32, 0, 0))
            .with_property("Reflectance", if value > 0.5 { -0.0_f32 } else { 0.0 });
        parts.push(dom.insert(folder, builder));
    }
    let camera = add(&mut dom, workspace, "Camera", "camera");
    let target = dom.insert(
        storage,
        Builder::new("MeshPart")
            .with_property("MeshId", ContentId::from("rbxassetid://1"))
            .with_property("PhysicalConfigData", SharedString::new(vec![1, 2, 3])),
    );
    for (parent, class, value) in [
        (workspace, "ObjectValue", RbxVariant::Ref(target)),
        (storage, "ObjectValue", parts[0].into()),
        (lighting, "BoolValue", true.into()),
    ] {
        dom.insert(parent, Builder::new(class).with_property("Value", value));
    }
    let properties = &mut dom.get_by_ref_mut(workspace).unwrap().properties;
    properties.insert("CurrentCamera".into(), camera.into());
    properties.insert("Gravity".into(), 196.2_f32.into());
    let base = encode(&dom, dom.root().children())?;
    let target_properties = &mut dom.get_by_ref_mut(target).unwrap().properties;
    target_properties.remove(&"PhysicalConfigData".into());
    let mut groups = Vec::new();
    let mut roots = Vec::new();
    for (referent, service) in [(workspace, "Workspace"), (storage, "ReplicatedStorage")] {
        let count = dom.descendants_of(referent).count();
        let children = dom.get_by_ref(referent).unwrap().children().to_vec();
        let marker = dom.get_by_ref_mut(referent).unwrap();
        marker.class = "Folder".into();
        marker.properties.clear();
        for child in &children {
            dom.transfer_within(*child, root);
        }
        roots.push(referent);
        roots.extend(&children);
        roots.push(add(&mut dom, root, "Folder", "carrier:1"));
        groups.push(serde_json::from_value(json!({
            "service": service, "targetPath": [service], "count": children.len(),
            "instanceCount": count, "classNames": [], "identityCarrierClass": "Folder",
            "identityCarrierPrefix": "carrier:", "identityCarrierCount": 1,
            "identityCarrierSlots": (0..16).map(|i| format!("slot{i}")).collect::<Vec<_>>(),
            "rootProperties": if service == "Workspace" { json!({"Gravity": 99}) } else { json!({}) }
        }))?);
    }
    Ok((
        base,
        EditorBinaryExport {
            #[cfg(windows)]
            attribute_guard: None,
            bytes: encode(&dom, &roots)?,
            groups,
            native_capture: false,
            serialization_batches: Vec::new(),
            export_id: None,
            property_schema_by_class: Default::default(),
            enum_value_names_by_type: Default::default(),
        },
    ))
}

fn check_indexes(dom: &RbxWeakDom) -> Result<()> {
    let mut wide = RbxWeakDom::new(Builder::new("DataModel"));
    let parent = wide.root_ref();
    for _ in 0..64 {
        add(&mut wide, parent, "Folder", "same");
    }
    for dom in [dom, &wide] {
        PATH_SIBLING_VISITS.set(0);
        let actual = rbx_dom_path_export_refs(dom);
        assert!(PATH_SIBLING_VISITS.get() <= dom.descendants().count());
        let mut expected = BytecodeModelExportRefs::default();
        for instance in dom.descendants().skip(1) {
            let referent = instance.referent();
            let (segments, ordinals) = rbx_dom_instance_path_parts(dom, referent);
            insert_unique_rbx_path(
                &mut expected.by_path_segments_key,
                instance_path_key(&segments),
                referent,
            );
            expected
                .by_path_key
                .insert(instance_path_parts_key(&segments, &ordinals), referent);
        }
        assert_eq!(actual.by_path_key, expected.by_path_key);
        assert_eq!(actual.by_path_segments_key, expected.by_path_segments_key);
    }
    let database = rbx_reflection_database::get()?;
    let refs = rbx_dom_path_import_refs(dom, true);
    let mut expected = HashMap::new();
    for root in dom.root().children() {
        let instance = dom.get_by_ref(*root).unwrap();
        let mut values = Map::new();
        for (name, value) in &instance.properties {
            if matches!(name.as_str(), "Attributes" | "Tags") {
                continue;
            }
            let descriptor =
                rbx_model_property_descriptor(database, instance.class.as_str(), name.as_str());
            if let Some(value) = rbx_variant_to_settings_json(value, descriptor, database, &refs) {
                values.insert(name.to_string(), value);
            }
        }
        expected.insert(instance.class.to_string(), values);
    }
    let services = expected.keys().cloned().collect();
    assert_eq!(
        rbx_dom_service_root_property_values(dom, &services, database),
        expected
    );
    Ok(())
}

fn corrupt(export: &mut EditorBinaryExport, case: usize) -> Result<()> {
    match case {
        0 => export.groups[0].instance_count += 1,
        1 => export.groups[0].count += 1,
        2 => export.groups[0].identity_carrier_prefix.push('x'),
        3 => export.groups[1] = export.groups[0].clone(),
        4 => export.bytes.truncate(export.bytes.len() / 2),
        5 => export.groups[0].identity_carrier_slots.push("slot0".into()),
        6 => export.groups[0].count = usize::MAX,
        7 => export.groups.clear(),
        8 => export.groups[0].target_path.clear(),
        9 => export.groups[0].instance_count = 0,
        10 => export.groups[0].identity_carrier_class.clear(),
        11 => export.groups[0].identity_carrier_slots.clear(),
        12 => export.groups[0].identity_carrier_count += 1,
        18 => export.groups[0].service.clear(),
        19 => export.groups[0].identity_carrier_slots[0].clear(),
        _ => {
            let mut dom = rbx_binary::from_reader(export.bytes.as_slice())?;
            let marker = dom.root().children()[0];
            let carrier = dom.root().children()[1 + export.groups[0].count];
            match case {
                13 => dom.get_by_ref_mut(marker).unwrap().class = "Model".into(),
                14 => dom.get_by_ref_mut(marker).unwrap().name = "changed".into(),
                15 => {
                    add(&mut dom, marker, "Folder", "child");
                }
                16 => dom.get_by_ref_mut(carrier).unwrap().class = "Model".into(),
                _ => {
                    add(&mut dom, carrier, "Folder", "child");
                }
            }
            export.bytes = encode(&dom, dom.root().children())?;
        }
    }
    Ok(())
}

#[test]
fn saved_snapshot_checks_contract_references_and_order() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../.save-snapshot-test/{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let _cleanup = OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let file = root.join("base.rbxl");
    let (base, _) = fixture()?;
    fs::write(&file, &base)?;
    let dom = rbx_binary::from_reader(base.as_slice())?;
    check_indexes(&dom)?;
    for case in 0..20 {
        let (_, mut malformed) = fixture()?;
        corrupt(&mut malformed, case)?;
        assert!(
            write_editor_export_snapshot(malformed, None, &file, Some(&file)).is_err(),
            "case {case}"
        );
        assert_eq!(fs::read(&file)?, base, "case {case} changed the original");
    }
    for extension in ["rbxl", "rbxlx"] {
        let file = root.join(format!("saved.{extension}"));
        let (_, export) = fixture()?;
        fs::write(&file, &base)?;
        write_editor_export_snapshot(export, None, &file, Some(&file))?;
        let saved = RbxPlaceFormat::Binary.read(&file)?;
        assert_eq!(saved.descendants().count(), dom.descendants().count());
        let get = |referent| saved.get_by_ref(referent).unwrap();
        let value =
            |referent, name: &str| &get(referent).properties[&rbx_dom_weak::Ustr::from(name)];
        let roots = saved.root().children();
        let classes: Vec<_> = roots.iter().map(|r| get(*r).class.as_str()).collect();
        assert_eq!(classes, ["Lighting", "Workspace", "ReplicatedStorage"]);
        let lighting = get(roots[0]).children();
        assert_eq!(lighting.len(), 1);
        assert_eq!(value(lighting[0], "Value"), &RbxVariant::Bool(true));
        let children = get(roots[1]).children();
        let names: Vec<_> = children.iter().map(|r| get(*r).name.as_str()).collect();
        assert_eq!(names, ["same", "same", "camera", "ObjectValue"]);
        let part = |index| get(children[index]).children()[0];
        for (index, number) in [0.25_f32, 0.75].into_iter().enumerate() {
            assert_eq!(value(part(index), "Transparency"), &number.into());
            assert_eq!(
                value(part(index), "UniqueId"),
                &UniqueId::new(index as u32, 0, 0).into()
            );
        }
        assert_eq!(value(roots[1], "Gravity"), &RbxVariant::Float32(99.0));
        assert_eq!(value(roots[1], "CurrentCamera"), &children[2].into());
        let storage = get(roots[2]).children();
        assert_eq!(storage.len(), 2);
        assert_eq!(get(storage[0]).class.as_str(), "MeshPart");
        assert_eq!(
            value(storage[0], "PhysicalConfigData"),
            &SharedString::new(vec![1, 2, 3]).into()
        );
        assert_eq!(value(part(1), "Anchored"), &RbxVariant::Bool(false));
        assert!(
            matches!(value(part(1), "Reflectance"), RbxVariant::Float32(v) if v.to_bits() == (-0.0_f32).to_bits())
        );
        assert_eq!(value(children[3], "Value"), &RbxVariant::Ref(storage[0]));
        assert_eq!(value(storage[1], "Value"), &RbxVariant::Ref(part(0)));
    }
    Ok(())
}
