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

#[test]
fn two_spellings_of_an_engine_managed_value_agree_or_conflict_as_in_the_whole_document() {
    let payload = |bytes: &[u8]| {
        Variant::SharedString(rbx_dom_weak::types::SharedString::new(bytes.to_vec()))
    };
    let (boxed, hull, other) = (
        &b"CSGPHS\0\0\0\0BLOCK"[..],
        &b"CSGPHS-computed-hull"[..],
        &b"CSGPHS-other-hull"[..],
    );
    let services = BTreeSet::from(["Workspace".to_string()]);
    let mut conflicts = 0;
    for (first, second) in [(boxed, hull), (hull, other), (hull, hull), (boxed, boxed)] {
        for keep in [vec![], vec!["PhysicalConfigData".to_string()]] {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
            dom.insert(
                workspace,
                InstanceBuilder::new("MeshPart")
                    .with_name("Track")
                    .with_property("PhysicalConfigData", payload(first))
                    .with_property("physicalconfigdata", payload(second)),
            );
            let whole = document(&dom, "before", Some(&services), true).map(|mut document| {
                let omitted = omit_engine_managed_properties(&mut document, &keep);
                (document, omitted)
            });
            match (whole, compared_document(&dom, "before", &services, &keep)) {
                (Ok(whole), Ok(read)) => assert_eq!(whole, read),
                (Err(whole), Err(read)) => {
                    conflicts += 1;
                    assert_eq!(whole.to_string(), read.to_string());
                }
                (whole, read) => panic!("{whole:?} against {read:?}"),
            }
        }
    }
    assert_eq!(
        conflicts, 4,
        "two payloads conflict whether the engine manages one or not"
    );
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

/// `compare` of DOMs at hand.
fn compare(
    before: impl ComparedDom,
    after: impl ComparedDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> Result<Value> {
    if let Ok(Some(result)) = compare_by_rows(before.borrow(), after.borrow(), services, args) {
        return Ok(result);
    }
    compare_whole(before, after, services, args)
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
        lighting_fixture(1, 4, true, Some(4)),
        lighting_fixture(2, 5, true, None),
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
        lighting_fixture(1, 4, false, None),
        lighting_fixture(1, 5, false, None),
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

/// A Workspace of 41 parts under a folder, one of them `tall`, and `more`
/// folders beside it.
fn turned_place(tall: f32, more: usize) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    let props = dom.insert(workspace, InstanceBuilder::new("Folder").with_name("Props"));
    for index in 0..41 {
        let part = InstanceBuilder::new("Part").with_name(format!("Part{index}"));
        let size = Vector3::new(1.0, if index == 20 { tall } else { 2.0 }, 1.0);
        dom.insert(props, part.with_property("Size", size));
    }
    for index in 0..more {
        let folder = InstanceBuilder::new("Folder").with_name(format!("More{index}"));
        dom.insert(workspace, folder);
    }
    dom
}

#[test]
fn places_read_in_turn_are_read_again_only_for_the_rows_that_differ() {
    let services = BTreeSet::from(["Workspace".to_string()]);
    let args = args(&["--against", "Other.rbxl", "--values"]);
    // The answer of places read when asked for, the first as `before` gives
    // it at each of its reads, and how often the first was read.
    let in_turn = |before: &dyn Fn(usize) -> WeakDom, after: f32| {
        let reads = std::cell::Cell::new(0);
        let mut read = || {
            reads.set(reads.get() + 1);
            Ok(before(reads.get()))
        };
        let answer = super::compare(
            &mut read,
            &mut || Ok(turned_place(after, 0)),
            Some(&services),
            &args,
        );
        (answer.unwrap(), reads.get())
    };
    let at_hand = |before: WeakDom, after: f32| {
        compare(before, turned_place(after, 0), &services, &args).unwrap()
    };
    let same = in_turn(&|_| turned_place(4.0, 0), 4.0);
    assert_eq!(same, (at_hand(turned_place(4.0, 0), 4.0), 1));
    let (changed, reads) = in_turn(&|_| turned_place(4.0, 0), 5.0);
    assert_eq!(changed, at_hand(turned_place(4.0, 0), 5.0));
    assert_eq!((changed["changed"].as_u64(), reads), (Some(1), 2));
    assert!(changed.to_string().contains("\"Size\""), "{changed}");
    // A first place written between its reads is compared as it stands at
    // the last: its changed row holds another value, or it holds other
    // instances.
    let rewritten = |read| turned_place(if read == 1 { 4.0 } else { 6.0 }, 0);
    let answer = in_turn(&rewritten, 5.0);
    assert_eq!(answer, (at_hand(turned_place(6.0, 0), 5.0), 3));
    let grown = |read| turned_place(4.0, usize::from(read > 1));
    let answer = in_turn(&grown, 5.0);
    assert_eq!(answer, (at_hand(turned_place(4.0, 1), 5.0), 3));
    // Places whose instances stand at other places are each read once more
    // at most: the first for its whole document.
    let (added, reads) = in_turn(&|_| turned_place(4.0, 1), 4.0);
    assert_eq!(added, at_hand(turned_place(4.0, 1), 4.0));
    assert_eq!((added["removed"].as_u64(), reads), (Some(1), 2));
}

/// A Workspace of ten parts under a folder and a model whose `PrimaryPart`
/// names its body, with a folder of one part between them where `gated`. A
/// value past them names the gate's part where `seen`, the body otherwise.
fn gated_place(gated: bool, seen: bool, tall: f32) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    let props = dom.insert(workspace, InstanceBuilder::new("Folder").with_name("Props"));
    for index in 0..10 {
        let part = InstanceBuilder::new("Part").with_name(format!("Part{index}"));
        dom.insert(
            props,
            part.with_property("Size", Vector3::new(1.0, 2.0, 1.0)),
        );
    }
    let bar = gated.then(|| {
        let gate = dom.insert(workspace, InstanceBuilder::new("Folder").with_name("Gate"));
        dom.insert(gate, InstanceBuilder::new("Part").with_name("Bar"))
    });
    let car = dom.insert(workspace, InstanceBuilder::new("Model").with_name("Car"));
    let body = InstanceBuilder::new("Part").with_name("Body");
    let body = dom.insert(
        car,
        body.with_property("Size", Vector3::new(1.0, tall, 1.0)),
    );
    let named = dom.get_by_ref_mut(car).unwrap();
    named
        .properties
        .insert("PrimaryPart".into(), Variant::Ref(body));
    let value = InstanceBuilder::new("ObjectValue").with_name("Seen");
    let target = bar.filter(|_| seen).unwrap_or(body);
    dom.insert(
        workspace,
        value.with_property("Value", Variant::Ref(target)),
    );
    dom
}

#[test]
fn places_one_run_apart_are_compared_by_their_rows() {
    let services = BTreeSet::from(["Workspace".to_string()]);
    let args = args(&["--against", "Other.rbxl"]);
    let both = |before: &WeakDom, after: &WeakDom| {
        let by_rows = compare_by_rows(before, after, &services, &args).unwrap();
        let whole = compare_whole(before, after, &services, &args).unwrap();
        assert_eq!(by_rows.as_ref(), Some(&whole), "by rows, then whole");
        let count = |key: &str| whole[key].as_u64().unwrap();
        (count("added"), count("removed"), count("changed"))
    };
    let plain = gated_place(false, false, 2.0);
    let gated = gated_place(true, false, 2.0);
    // The rows past the run name each other at other places in the two.
    assert_eq!(both(&plain, &gated), (2, 0, 0));
    assert_eq!(both(&gated, &plain), (0, 2, 0));
    // A value past the run that differs, and one that names a row of it.
    assert_eq!(both(&plain, &gated_place(true, false, 3.0)), (2, 0, 1));
    assert_eq!(both(&gated_place(true, true, 2.0), &plain), (0, 2, 1));
    assert_eq!(both(&plain, &gated_place(true, true, 3.0)), (2, 0, 2));
}

/// A Workspace of rigs of one name, each a model whose `PrimaryPart` names
/// its torso, as tall as `tall` says: between two folders where `framed`,
/// so that the place stands two runs apart from one without them. A value
/// past the rigs names the torso of the rig `seen` says.
fn rigged_place(framed: bool, tall: &[f32], seen: Option<usize>) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    if framed {
        dom.insert(workspace, InstanceBuilder::new("Folder").with_name("Ahead"));
    }
    let mut torsos = Vec::new();
    for tall in tall {
        let rig = dom.insert(workspace, InstanceBuilder::new("Model").with_name("Rig"));
        let torso = InstanceBuilder::new("Part").with_name("Torso");
        let torso = dom.insert(
            rig,
            torso.with_property("Size", Vector3::new(1.0, *tall, 1.0)),
        );
        let named = dom.get_by_ref_mut(rig).unwrap();
        named
            .properties
            .insert("PrimaryPart".into(), Variant::Ref(torso));
        torsos.push(torso);
    }
    if let Some(seen) = seen {
        let value = InstanceBuilder::new("ObjectValue").with_name("Seen");
        dom.insert(
            workspace,
            value.with_property("Value", Variant::Ref(torsos[seen])),
        );
    }
    if framed {
        dom.insert(workspace, InstanceBuilder::new("Folder").with_name("Past"));
    }
    dom
}

#[test]
fn namesakes_that_differ_are_compared_by_their_rows() {
    let services = BTreeSet::from(["Workspace".to_string()]);
    let args = args(&["--against", "Other.rbxl"]);
    let both = |before: &WeakDom, after: &WeakDom| {
        let by_rows = compare_by_rows(before, after, &services, &args).unwrap();
        let whole = compare_whole(before, after, &services, &args).unwrap();
        assert_eq!(by_rows.as_ref(), Some(&whole), "by rows, then whole");
        let count = |key: &str| whole[key].as_u64().unwrap();
        (count("added"), count("removed"), count("changed"))
    };
    let plain = rigged_place(false, &[2.0, 3.0, 4.0], None);
    // Namesakes taken by their order that hold what their counterparts hold.
    assert_eq!(
        both(&plain, &rigged_place(true, &[2.0, 3.0, 4.0], None)),
        (2, 0, 0)
    );
    // One of them differs, in either place.
    assert_eq!(
        both(&plain, &rigged_place(true, &[2.0, 3.0, 5.0], None)),
        (2, 0, 1)
    );
    assert_eq!(
        both(&rigged_place(true, &[2.0, 3.0, 5.0], None), &plain),
        (0, 2, 1)
    );
    // Twins that swapped what they hold, and one place with more of them.
    both(&plain, &rigged_place(true, &[3.0, 2.0, 4.0], None));
    both(&plain, &rigged_place(true, &[3.0, 2.0, 5.0], None));
    assert_eq!(
        both(&plain, &rigged_place(true, &[2.0, 4.0], None)),
        (2, 2, 0)
    );
    assert_eq!(
        both(&plain, &rigged_place(true, &[2.0, 9.0, 3.0, 4.0], None)),
        (4, 0, 0)
    );
    both(&plain, &rigged_place(true, &[9.0, 4.0], None));
    // A row past the group names one of it.
    let seen = rigged_place(false, &[2.0, 3.0, 4.0], Some(0));
    assert_eq!(
        both(&seen, &rigged_place(true, &[2.0, 3.0, 5.0], Some(0))),
        (2, 0, 1)
    );
    both(&seen, &rigged_place(true, &[2.0, 3.0, 5.0], Some(2)));
    both(&seen, &rigged_place(true, &[3.0, 2.0, 5.0], Some(1)));
    both(&plain, &rigged_place(true, &[2.0, 3.0, 5.0], Some(1)));
    both(&seen, &rigged_place(true, &[2.0, 3.0, 5.0], None));
}

/// A Workspace whose folder holds two models of one name, a value that names
/// the part of the second, and a folder past it. The first of the models
/// holds a part too where `both`, and the last folder one more where `more`:
/// the places stand apart where no one run tells.
fn twinned_place(both: bool, more: bool) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    let folder = dom.insert(workspace, InstanceBuilder::new("Folder").with_name("F"));
    let mut part = None;
    for holds in [both, true] {
        let twin = dom.insert(folder, InstanceBuilder::new("Model").with_name("A"));
        if holds {
            part = Some(dom.insert(twin, InstanceBuilder::new("Part").with_name("C")));
        }
    }
    let value = InstanceBuilder::new("ObjectValue").with_name("R");
    dom.insert(
        workspace,
        value.with_property("Value", Variant::Ref(part.unwrap())),
    );
    let last = dom.insert(workspace, InstanceBuilder::new("Folder").with_name("G"));
    if more {
        dom.insert(last, InstanceBuilder::new("Folder").with_name("N"));
    }
    dom
}

#[test]
fn a_row_removed_under_namesakes_is_paired_as_the_whole_documents_pair_it() {
    let services = BTreeSet::from(["Workspace".to_string()]);
    let args = args(&["--against", "Other.rbxl"]);
    for (before, after) in [
        (twinned_place(true, false), twinned_place(false, true)),
        (twinned_place(false, true), twinned_place(true, false)),
        (twinned_place(false, false), twinned_place(true, true)),
    ] {
        let by_rows = compare_by_rows(&before, &after, &services, &args).unwrap();
        let whole = compare_whole(&before, &after, &services, &args).unwrap();
        assert_eq!(by_rows.as_ref(), Some(&whole), "by rows, then whole");
    }
}

/// A Workspace of three models of one name, each of a part as tall as `tall`
/// says and, the one `more` names, of a second part.
fn modelled_place(tall: [f32; 3], more: Option<usize>) -> WeakDom {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
    for (index, tall) in tall.into_iter().enumerate() {
        let model = dom.insert(workspace, InstanceBuilder::new("Model").with_name("Tree"));
        let trunk = InstanceBuilder::new("Part").with_name("Trunk");
        dom.insert(
            model,
            trunk.with_property("Size", Vector3::new(1.0, tall, 1.0)),
        );
        if more == Some(index) {
            dom.insert(model, InstanceBuilder::new("Part").with_name("Leaf"));
        }
    }
    dom
}

/// How many rows of the first place a comparison by rows reads whole beside
/// its services.
fn owed_by_rows(
    before: &WeakDom,
    after: &WeakDom,
    services: &BTreeSet<String>,
    args: &ComparePlaceArgs,
) -> usize {
    let reading = Reading::of(args);
    let (first, _) = keep_rows(before, "before", services, reading).unwrap();
    let paired = pair_rows(first, after, services, reading).unwrap();
    paired.unwrap().owed.len()
}

#[test]
fn twins_are_read_whole_only_where_they_may_be_paired_anew() {
    let services = BTreeSet::from(["Workspace".to_string()]);
    let args = args(&["--against", "Other.rbxl"]);
    // How many rows of the first place a comparison by rows reads whole
    // beside its service, with whether it answers as the whole documents do.
    let owed = |before: WeakDom, after: WeakDom| {
        let by_rows = compare_by_rows(&before, &after, &services, &args).unwrap();
        let whole = compare_whole(&before, &after, &services, &args).unwrap();
        assert_eq!(by_rows.as_ref(), Some(&whole), "by rows, then whole");
        (
            owed_by_rows(&before, &after, &services, &args),
            whole["changed"].as_u64().unwrap(),
        )
    };
    let stands = || modelled_place([4.0, 5.0, 6.0], None);
    assert_eq!(owed(stands(), stands()), (0, 0));
    // A twin that holds another value may be another twin's counterpart:
    // every twin and what it holds is read.
    assert_eq!(
        owed(stands(), modelled_place([4.0, 7.0, 6.0], None)),
        (6, 1)
    );
    // Twins that swapped what they hold are paired by it and hold the same.
    assert_eq!(
        owed(stands(), modelled_place([5.0, 4.0, 6.0], None)),
        (0, 0)
    );
    // A twin that holds a row more matches no counterpart, so no twin is
    // paired anew and none is read.
    assert_eq!(
        owed(stands(), modelled_place([4.0, 5.0, 6.0], Some(1))),
        (0, 0)
    );
    assert_eq!(
        owed(modelled_place([4.0, 5.0, 6.0], Some(1)), stands()),
        (1, 0)
    );
}

#[test]
fn a_property_named_as_the_digest_of_a_kept_row_is_compared_by_the_whole_documents() {
    let place = |value: &str| {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        let part = InstanceBuilder::new("Part").with_property(KEPT_VALUES, value.to_string());
        dom.insert(workspace, part);
        dom
    };
    let services = BTreeSet::from(["Workspace".to_string()]);
    let args = args(&["--against", "Other.rbxl"]);
    assert!(compare_by_rows(&place("a"), &place("b"), &services, &args).is_err());
    let whole = compare_whole(&place("a"), &place("b"), &services, &args).unwrap();
    assert_eq!(whole["changed"].as_u64(), Some(1), "{whole}");
    let in_turn = super::compare(
        &mut || Ok(place("a")),
        &mut || Ok(place("b")),
        Some(&services),
        &args,
    );
    assert_eq!(in_turn.unwrap(), whole);
}

#[test]
fn what_a_project_never_captures_leaves_twins_unread_against_a_project() {
    let database = rbx_reflection_database::get().unwrap();
    // A Bool a class has a default of and no project captures, the first by
    // the names.
    let mut bools = Vec::new();
    for (class_name, class) in &database.classes {
        if class.tags.contains(&rbx_reflection::ClassTag::Service) {
            continue;
        }
        for (name, default) in &class.default_properties {
            let apart = left_out(database, class_name, name);
            if let Variant::Bool(default) = default
                && apart.never_captured
                && !apart.one_sided
                && !engine_managed_property(class_name, name, &json!(!default))
            {
                bools.push((class_name.to_string(), name.to_string(), !default));
            }
        }
    }
    bools.sort();
    let (class_name, name, value) = bools.first().expect("a Bool no project captures").clone();
    let place = |holds: bool| {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        for _ in 0..3 {
            let mut twin = InstanceBuilder::new(class_name.as_str()).with_name("Twin");
            if holds {
                twin = twin.with_property(name.as_str(), value);
            }
            dom.insert(workspace, twin);
        }
        dom
    };
    let services = BTreeSet::from(["Workspace".to_string()]);
    let (before, after) = (place(true), place(false));
    for (extra, changed, owed) in [(&[][..], 0, 0), (&["--against", "Other.rbxl"][..], 3, 3)] {
        let args = args(extra);
        let by_rows = compare_by_rows(&before, &after, &services, &args).unwrap();
        let whole = compare_whole(&before, &after, &services, &args).unwrap();
        assert_eq!(by_rows.as_ref(), Some(&whole), "{class_name}.{name}");
        assert_eq!(
            whole["changed"].as_u64(),
            Some(changed),
            "{class_name}.{name}: {whole}"
        );
        assert_eq!(owed_by_rows(&before, &after, &services, &args), owed);
    }
}
