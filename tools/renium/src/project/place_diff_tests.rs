use super::*;
use clap::Parser;
use rbx_dom_weak::{
    InstanceBuilder,
    types::{Attributes, CFrame, Matrix3, Vector3},
};

#[test]
fn changed_field_scan_preserves_names_values_and_presence() {
    let before = serde_json::from_value::<Map<String, Value>>(
        json!({"same": 1,"removed": null,"edited": 1}),
    )
    .unwrap();
    let after =
        serde_json::from_value::<Map<String, Value>>(json!({"same": 1,"added": null,"edited": 2}))
            .unwrap();
    assert!(changed_fields(&before, &before, true).is_empty());
    assert_eq!(
        changed_fields(&before, &after, false),
        json!([{"name":"added"},{"name":"edited"},{"name":"removed"}])
            .as_array()
            .unwrap()
            .clone()
    );
    let values = changed_fields(&before, &after, true);
    assert_eq!(values[0]["beforePresent"], false);
    assert_eq!(values[0]["afterPresent"], true);
    assert_eq!(values[1]["before"], 1);
    assert_eq!(values[1]["after"], 2);
    assert_eq!(values[2]["beforePresent"], true);
    assert_eq!(values[2]["afterPresent"], false);
}

#[test]
fn rig_table_omissions_keep_authored_instance_properties() {
    use crate::tests::support::{settings_document, settings_instance};
    let mut rig = settings_instance("rig", "AnimationRigData", "AnimationRigData", None);
    rig.properties = serde_json::from_value(json!({
        "rootMotion": {"_type": "BinaryString", "base64": "AQI="},
        "articulatedJoint": "",
        "Tags": ["authored"],
        "Archivable": false,
    }))
    .unwrap();
    let mut document = settings_document(vec![rig]);
    assert_eq!(
        omit_engine_managed_properties(&mut document, &[]),
        BTreeMap::from([("articulatedJoint".into(), 1), ("rootMotion".into(), 1)]),
        "an unfilled table is saved as an empty string"
    );
    assert_eq!(
        document.instances[0].properties["Tags"],
        json!(["authored"])
    );
    assert_eq!(document.instances[0].properties["Archivable"], false);
}

#[test]
fn changed_field_scan_knows_class_defaults_and_asset_spellings() {
    let saved = serde_json::from_value::<Map<String, Value>>(json!({
        "Gravity": 196.2,
        "StreamingEnabled": false,
        "FallenPartsDestroyHeight": -500.0,
    }))
    .unwrap();
    let project =
        serde_json::from_value::<Map<String, Value>>(json!({"FallenPartsDestroyHeight": -500.0}))
            .unwrap();
    assert!(
        changed_fields_of(Some("Workspace"), &saved, &project, false).is_empty(),
        "a saved place spells every field, a project only the ones off their default"
    );
    let mut lighter = saved.clone();
    lighter["Gravity"] = json!(50.0);
    assert_eq!(
        changed_fields_of(Some("Workspace"), &lighter, &project, false),
        json!([{"name":"Gravity"}]).as_array().unwrap().clone()
    );
    assert_eq!(
        changed_fields(&saved, &project, false).len(),
        2,
        "without a class nothing is a default"
    );

    let mut place = WeakDom::new(InstanceBuilder::new("DataModel"));
    place.insert(
        place.root_ref(),
        InstanceBuilder::new("Workspace")
            .with_property("Gravity", 50.0f32)
            .with_property(
                "CSGAsyncDynamicCollision",
                rbx_dom_weak::types::Enum::from_u32(0),
            )
            .with_property(
                "WorldPivotData",
                CFrame::new(Vector3::new(0.0, 0.0, 0.0), Matrix3::identity()),
            ),
    );
    place.insert(
        place.root_ref(),
        InstanceBuilder::new("StarterPlayer")
            .with_property("GameSettingsAssetIDFace", 0i64)
            .with_property("CharacterWalkSpeed", 16.0f32),
    );
    let omitted = omit_unsaved_service_properties(&mut place).unwrap();
    assert_eq!(
        omitted,
        BTreeMap::from([
            ("CSGAsyncDynamicCollision".to_string(), 1),
            ("GameSettingsAssetIDFace".to_string(), 1),
            ("WorldPivotData".to_string(), 1),
        ]),
        "Studio's own service state the project never carries"
    );
    let workspace = place.get_by_ref(place.root().children()[0]).unwrap();
    assert!(
        workspace
            .properties
            .contains_key(&rbx_dom_weak::Ustr::from("Gravity"))
    );
    let attribute =
        serde_json::from_value::<Map<String, Value>>(json!({"Anchored": false})).unwrap();
    assert_eq!(
        changed_fields(&attribute, &Map::new(), false),
        json!([{"name":"Anchored"}]).as_array().unwrap().clone(),
        "an attribute named like a property is still a deletion"
    );
    let placeholder = serde_json::from_value::<Map<String, Value>>(json!({
        "PhysicalConfigData": {"_type": "BinaryString", "base64": "Q1NHUEhTAAAAAEJMT0NL"},
    }))
    .unwrap();
    assert_eq!(
        changed_fields_of(Some("MeshPart"), &placeholder, &Map::new(), false).len(),
        1,
        "a box placeholder is reported even if a default happened to match"
    );

    let url = serde_json::from_value::<Map<String, Value>>(
        json!({"TextureContent": "http://www.roblox.com/asset/?id=13125202269"}),
    )
    .unwrap();
    let uri = serde_json::from_value::<Map<String, Value>>(
        json!({"TextureContent": "rbxassetid://13125202269"}),
    )
    .unwrap();
    assert!(changed_fields_of(Some("Decal"), &url, &uri, false).is_empty());
    let other = serde_json::from_value::<Map<String, Value>>(
        json!({"TextureContent": "rbxassetid://13125202270"}),
    )
    .unwrap();
    assert_eq!(
        changed_fields_of(Some("Decal"), &url, &other, false).len(),
        1
    );
}

#[test]
fn members_saved_by_one_side_only_are_set_aside_per_class() {
    use crate::tests::support::{settings_document, settings_instance};
    let mut before = settings_document(vec![
        settings_instance("w", "Workspace", "Workspace", None),
        settings_instance("a", "Skin", "SurfaceAppearance", Some(0)),
        settings_instance("p", "Part", "Part", Some(0)),
    ]);
    before.instances[2]
        .properties
        .insert("Anchored".into(), json!(true));
    let mut after = before.clone();
    after.instances[1]
        .properties
        .insert("UndefaultedSlot".into(), json!(-1));
    after.instances[2]
        .properties
        .insert("Anchored".into(), json!(false));
    let database = rbx_reflection_database::get().unwrap();
    let omitted = omit_one_sided_class_properties(&mut before, &mut after, database);
    assert_eq!(
        omitted,
        BTreeMap::from([("UndefaultedSlot".to_string(), 1)])
    );
    assert!(after.instances[1].properties.is_empty());
    assert_eq!(after.instances[2].properties["Anchored"], false);
}

#[test]
fn full_diff_reports_box_hulls_even_when_they_equal_the_reflection_default() {
    let make = |bytes: &[u8]| {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        dom.insert(
            workspace,
            InstanceBuilder::new("MeshPart")
                .with_name("Track")
                .with_property(
                    "PhysicalConfigData",
                    rbx_dom_weak::types::SharedString::new(bytes.to_vec()),
                ),
        );
        dom
    };
    let boxed = make(b"CSGPHS\0\0\0\0BLOCK");
    let hull = make(b"CSGPHS-computed-hull");
    let result = diff(&boxed, &hull, &["--all", "--values"]);
    assert_eq!(result["changed"], 1, "{result}");
    assert_eq!(
        result["differences"][0]["properties"][0]["name"],
        "PhysicalConfigData"
    );
    assert_eq!(diff(&boxed, &boxed, &[])["matches"], true);
}

fn fixture(reverse: bool, revision: f64) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    let mut targets = HashMap::new();
    for value in if reverse { [2, 1] } else { [1, 2] } {
        let folder = dom.insert(root, InstanceBuilder::new("Folder").with_name("Duplicate"));
        dom.insert(
            folder,
            InstanceBuilder::new("ModuleScript")
                .with_name("Code")
                .with_property(
                    "Source",
                    format!("return {value}{}", if reverse { "\r\n" } else { "\n" }),
                ),
        );
        targets.insert(value, folder);
    }
    dom.insert(
        root,
        InstanceBuilder::new("ObjectValue")
            .with_name("Target")
            .with_property("Value", targets[&1]),
    );
    let mut attributes = Attributes::new();
    attributes.insert("Revision".to_string(), Variant::Float64(revision));
    dom.insert(
        root,
        InstanceBuilder::new("Folder")
            .with_name("Tagged")
            .with_property("Attributes", attributes),
    );
    dom
}

fn args(extra: &[&str]) -> ComparePlaceArgs {
    ComparePlaceArgs::try_parse_from(
        ["cmp", "Before.rbxl", "--full"]
            .into_iter()
            .chain(extra.iter().copied()),
    )
    .unwrap()
}

fn diff(before: &WeakDom, after: &WeakDom, extra: &[&str]) -> Value {
    compare(
        before,
        after,
        &BTreeSet::from(["Workspace".to_string()]),
        &args(extra),
    )
    .unwrap()
}

#[test]
fn comparison_decode_elision_preserves_normalized_values() {
    let mut source = fixture(false, 2.0);
    let root = source.root().children()[0];
    let database = rbx_reflection_database::get().unwrap();
    for class in [
        "Part",
        "MeshPart",
        "Attachment",
        "Model",
        "UICorner",
        "Frame",
        "ModuleScript",
        "Texture",
        "WeldConstraint",
    ] {
        let defaults = &database.classes[class].default_properties;
        source.insert(
            root,
            InstanceBuilder::new(class).with_properties(
                defaults
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone())),
            ),
        );
    }
    source.insert(
        root,
        InstanceBuilder::new("Part")
            .with_property("Anchored", true)
            .with_property("Size", rbx_dom_weak::types::Vector3::new(7.0, 8.0, 9.0))
            .with_property("FutureSavedProperty", "retained"),
    );
    let root_dir = crate::system::files::create_unique_directory(
        &std::env::temp_dir(),
        "renium-comparison-decode-",
    )
    .unwrap();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root_dir);
    });
    for extension in ["rbxl", "rbxlx"] {
        let path = root_dir.join(format!("comparison.{extension}"));
        let format = crate::rbx::model::RbxPlaceFormat::from_path(&path).unwrap();
        format
            .write(&path, &source, source.root().children())
            .unwrap();
        let original = format.read(&path).unwrap();
        let elided = format.read_for_comparison(&path).unwrap();
        assert_eq!(
            document(&original, "same", None, true).unwrap(),
            document(&elided, "same", None, true).unwrap(),
            "{extension}"
        );
        assert_eq!(diff(&original, &elided, &["--values"])["matches"], true);
        if extension == "rbxl" {
            assert!(
                original
                    .descendants()
                    .map(|node| node.properties.len())
                    .sum::<usize>()
                    > elided
                        .descendants()
                        .map(|node| node.properties.len())
                        .sum::<usize>()
            );
        }
    }
}

#[test]
fn place_comparison_takes_the_target_positionally_and_skips_engine_session_services() {
    let root =
        crate::system::files::create_unique_directory(&std::env::temp_dir(), "renium-place-cmp-")
            .unwrap();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let mut files = Vec::new();
    for (name, revision) in [("before", 1.0), ("after", 2.0)] {
        let mut dom = fixture(false, 1.0);
        let session = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("ConfigureServerService").with_name("ConfigureServerService"),
        );
        let mut attributes = Attributes::new();
        attributes.insert("Session".to_string(), Variant::Float64(revision));
        dom.insert(
            session,
            InstanceBuilder::new("Folder")
                .with_name(format!("Session{revision}"))
                .with_property("Attributes", attributes),
        );
        let file = root.join(format!("{name}.rbxl"));
        crate::rbx::model::RbxPlaceFormat::from_path(&file)
            .unwrap()
            .write(&file, &dom, dom.root().children())
            .unwrap();
        files.push(file);
    }
    let before = files[0].to_str().unwrap();
    let after = files[1].to_str().unwrap();
    let result = crate::app::output::capture_json_output(|| {
        super::super::place_file::compare_place(
            ComparePlaceArgs::try_parse_from(["cmp", before, after, "--full"]).unwrap(),
            None,
        )
    })
    .unwrap();
    assert_eq!(result["matches"], true, "{result}");
    assert_eq!(result["targetKind"], "place");
    assert_eq!(
        result["engineManagedServices"],
        json!(["ConfigureServerService"])
    );
    assert!(ComparePlaceArgs::try_parse_from(["cmp", before, after, "--against", after]).is_err());
}

#[test]
fn failed_place_encoding_preserves_the_existing_file() {
    let root =
        crate::system::files::create_unique_directory(&std::env::temp_dir(), "renium-place-diff-")
            .unwrap();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let mut invalid = WeakDom::new(InstanceBuilder::new("DataModel"));
    invalid.insert(
        invalid.root_ref(),
        InstanceBuilder::new("Part").with_property(
            "Unencodable",
            rbx_dom_weak::types::Region3::new(
                rbx_dom_weak::types::Vector3::new(0.0, 0.0, 0.0),
                rbx_dom_weak::types::Vector3::new(1.0, 1.0, 1.0),
            ),
        ),
    );
    for extension in ["rbxl", "rbxlx"] {
        let path = root.join(format!("existing.{extension}"));
        std::fs::write(&path, b"existing place").unwrap();
        assert!(
            crate::rbx::model::RbxPlaceFormat::from_path(&path)
                .unwrap()
                .write(&path, &invalid, invalid.root().children())
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"existing place");
    }
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
}

fn lighting_fixture(state: i32, technology: u32, marker: bool, original: Option<i32>) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    dom.insert(
        root,
        InstanceBuilder::new("WeldConstraint").with_property("State", state),
    );
    dom.insert(
        root,
        InstanceBuilder::new("MeshPart").with_property(
            "PhysicalConfigData",
            rbx_dom_weak::types::SharedString::new(vec![state as u8, 7]),
        ),
    );
    let mut attributes = Attributes::new();
    if marker {
        attributes.insert(
            "RBX_LightingTechnologyUnifiedMigration".to_string(),
            Variant::Bool(true),
        );
    }
    if let Some(original) = original {
        attributes.insert(
            "RBX_OriginalTechnologyOnFileLoad".to_string(),
            Variant::Int32(original),
        );
    }
    dom.insert(
        dom.root_ref(),
        InstanceBuilder::new("Lighting")
            .with_property(
                "Technology",
                rbx_dom_weak::types::Enum::from_u32(technology),
            )
            .with_property("Attributes", attributes),
    );
    dom
}

#[test]
fn full_diff_skips_engine_managed_values_and_migration_markers() {
    let services = BTreeSet::from(["Workspace".to_string(), "Lighting".to_string()]);
    let result = compare(
        &lighting_fixture(1, 4, true, Some(4)),
        &lighting_fixture(2, 5, true, None),
        &services,
        &args(&[]),
    )
    .unwrap();
    assert_eq!(result["matches"], true, "{result}");
    assert_eq!(result["engineManagedProperties"]["State"], 2);
    assert_eq!(result["engineManagedProperties"]["PhysicalConfigData"], 2);
    assert_eq!(result["engineManagedProperties"]["Technology"], 1);
    assert_eq!(
        result["engineManagedProperties"]["RBX_OriginalTechnologyOnFileLoad"],
        1
    );
    let legacy = compare(
        &lighting_fixture(1, 4, false, None),
        &lighting_fixture(1, 5, false, None),
        &services,
        &args(&["--values"]),
    )
    .unwrap();
    assert_eq!(legacy["changed"], 1, "{legacy}");
    assert_eq!(
        legacy["differences"][0]["properties"][0]["name"],
        "Technology"
    );
    assert!(
        legacy["engineManagedProperties"]
            .get("Technology")
            .is_none()
    );
}

#[test]
fn project_diff_omits_unsaved_instances_and_fields_the_project_never_captures() {
    let mut place = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = place.insert(place.root_ref(), InstanceBuilder::new("Workspace"));
    place.insert(root, InstanceBuilder::new("Part").with_name("Saved"));
    place.insert(
        place.root_ref(),
        InstanceBuilder::new("TextChatService").with_property("IsLegacyChatDisabled", true),
    );
    let mut project = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = project.insert(project.root_ref(), InstanceBuilder::new("Workspace"));
    project.insert(root, InstanceBuilder::new("Part").with_name("Saved"));
    let hats = project.insert(
        root,
        InstanceBuilder::new("Folder")
            .with_name("Hats")
            .with_property("Archivable", false),
    );
    project.insert(hats, InstanceBuilder::new("Part").with_name("Hat"));
    project.insert(project.root_ref(), InstanceBuilder::new("TextChatService"));
    let services = BTreeSet::from(["Workspace".to_string(), "TextChatService".to_string()]);
    let against = compare(
        &place,
        &project,
        &services,
        &args(&["--against", "Other.rbxl"]),
    )
    .unwrap();
    assert_eq!(against["added"], 2, "{against}");
    assert_eq!(against["changed"], 1);
    assert_eq!(omit_unsaved_project_instances(&mut project), 2);
    let result = compare(&place, &project, &services, &args(&[])).unwrap();
    assert_eq!(result["matches"], true, "{result}");
    assert_eq!(
        result["notCapturedProjectProperties"]["IsLegacyChatDisabled"],
        1
    );
}
