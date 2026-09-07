use super::*;
use clap::Parser;
use rbx_dom_weak::{InstanceBuilder, types::Attributes};

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
fn full_diff_matches_reordered_duplicate_subtrees_and_references() {
    let result = diff(&fixture(false, 1.0), &fixture(true, 1.0), &[]);
    assert_eq!(result["matches"], true, "{result}");
    assert_eq!(result["unchanged"], 7);
}

#[test]
fn full_diff_reports_attributes_sources_references_additions_and_removals() {
    let before = fixture(false, 1.0);
    let mut after = fixture(false, 2.0);
    let root = after.root().children()[0];
    let children = after.get_by_ref(root).unwrap().children().to_vec();
    after
        .get_by_ref_mut(children[2])
        .unwrap()
        .properties
        .insert("Value".into(), Variant::Ref(children[1]));
    let script = after.get_by_ref(children[0]).unwrap().children()[0];
    after
        .get_by_ref_mut(script)
        .unwrap()
        .properties
        .insert("Source".into(), Variant::String("return 9".into()));
    after.insert(root, InstanceBuilder::new("Folder").with_name("Added"));
    let result = diff(&before, &after, &["--values", "--all"]);
    assert_eq!(result["added"], 1, "{result}");
    assert_eq!(result["changed"], 3, "{result}");
    assert_eq!(result["removed"], 0);
    assert!(result["differences"].as_array().unwrap().iter().any(|d| {
        d["attributes"].as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v["name"] == "Revision" && v["before"] == 1.0 && v["after"] == 2.0)
        })
    }));
    let reverse = diff(&after, &before, &["--limit", "1"]);
    assert_eq!(reverse["removed"], 1);
    assert_eq!(reverse["differenceCount"], 4);
    assert_eq!(reverse["truncated"], true);
    assert_eq!(reverse["differences"].as_array().unwrap().len(), 1);
}

#[test]
fn full_diff_reads_binary_and_xml_places_without_studio_and_view_keeps_exact_source() {
    let root =
        crate::system::files::create_unique_directory(&std::env::temp_dir(), "renium-place-diff-")
            .unwrap();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let mut original = fixture(true, 1.0);
    let workspace = original.root().children()[0];
    original
        .get_by_ref_mut(workspace)
        .unwrap()
        .properties
        .extend([
            (
                "FutureProperty".into(),
                Variant::String("saved value".into()),
            ),
            (
                "FutureEnum".into(),
                Variant::Enum(rbx_dom_weak::types::Enum::from_u32(1)),
            ),
        ]);
    let mut attributes = Attributes::new();
    attributes.insert("Text".to_string(), Variant::String("hello\0世界".into()));
    original.insert(
        workspace,
        InstanceBuilder::new("Model")
            .with_name("Empty")
            .with_property("Attributes", attributes),
    );
    let target = original.insert(
        workspace,
        InstanceBuilder::new("Part").with_property(
            "Material",
            rbx_dom_weak::types::EnumItem {
                ty: "Material".into(),
                value: 288,
            },
        ),
    );
    original.insert(
        workspace,
        InstanceBuilder::new("Model")
            .with_name("Linked")
            .with_property("PrimaryPart", target),
    );
    let mut copies = Vec::new();
    for extension in ["rbxl", "rbxlx"] {
        let file = root.join(format!("place.{extension}"));
        let format = crate::rbx::model::RbxPlaceFormat::from_path(&file).unwrap();
        format
            .write(&file, &original, original.root().children())
            .unwrap();
        copies.push(super::super::place_file::place_dom(&file).unwrap());
        let round_trip = diff(&original, copies.last().unwrap(), &["--all"]);
        assert_eq!(round_trip["matches"], true, "{extension}: {round_trip}");
    }
    let result = diff(&copies[0], &copies[1], &["--all"]);
    assert_eq!(result["matches"], true, "{result}");
    let view = document(&original, "view", None, false).unwrap();
    assert!(
        view.instances
            .iter()
            .any(|node| node.properties.get("Source") == Some(&json!("return 2\r\n")))
    );
}

#[test]
fn full_diff_does_not_hide_unknown_properties_or_emit_values_by_default() {
    let before = fixture(false, 1.0);
    let mut after = fixture(false, 1.0);
    let root = after.root().children()[0];
    after.get_by_ref_mut(root).unwrap().properties.insert(
        "FutureProperty".into(),
        Variant::String("private text".into()),
    );
    let result = diff(&before, &after, &[]);
    assert_eq!(result["changed"], 1);
    assert!(!result.to_string().contains("private text"));
    assert!(
        diff(&before, &after, &["--values"])
            .to_string()
            .contains("private text")
    );
}

#[test]
fn full_diff_normalizes_enum_encodings_aliases_and_explicit_defaults() {
    use rbx_dom_weak::types::{Enum, EnumItem};
    let mut before = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = before.insert(before.root_ref(), InstanceBuilder::new("Workspace"));
    before.insert(
        root,
        InstanceBuilder::new("Part")
            .with_property("Material", Enum::from_u32(256))
            .with_property("Transparency", 0.0_f32),
    );
    let mut after = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = after.insert(after.root_ref(), InstanceBuilder::new("Workspace"));
    let part = after.insert(
        root,
        InstanceBuilder::new("Part").with_property(
            "material",
            EnumItem {
                ty: "Material".into(),
                value: 256,
            },
        ),
    );
    assert_eq!(diff(&before, &after, &["--all"])["matches"], true);
    after.get_by_ref_mut(part).unwrap().properties.insert(
        "material".into(),
        Variant::EnumItem(EnumItem {
            ty: "Material".into(),
            value: 288,
        }),
    );
    let changed = diff(&before, &after, &["--all", "--values"]);
    assert_eq!(changed["changed"], 1, "{changed}");
    assert_eq!(
        changed["differences"][0]["properties"][0]["name"],
        "Material"
    );
    assert_eq!(
        changed["differences"][0]["properties"][0]["after"]["enumType"],
        "Enum.Material"
    );
}

#[test]
fn full_diff_reads_unknown_xml_properties_and_treats_tags_as_a_set() {
    let root =
        crate::system::files::create_unique_directory(&std::env::temp_dir(), "renium-place-diff-")
            .unwrap();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let path = root.join("unknown.rbxlx");
    std::fs::write(&path, r#"<roblox version="4"><Item class="Workspace" referent="root"><Properties><string name="Name">Workspace</string><string name="FutureProperty">saved value</string></Properties></Item></roblox>"#).unwrap();
    let before = super::super::place_file::place_dom(&path).unwrap();
    let view = document(&before, "view", None, false).unwrap();
    assert_eq!(
        view.instances[0].properties["FutureProperty"],
        "saved value"
    );
    let mut after = super::super::place_file::place_dom(&path).unwrap();
    let root = after.root().children()[0];
    after
        .get_by_ref_mut(root)
        .unwrap()
        .properties
        .remove(&"FutureProperty".into());
    assert_eq!(diff(&before, &after, &[])["changed"], 1);

    let mut before = before;
    let before_root = before.root().children()[0];
    before
        .get_by_ref_mut(before_root)
        .unwrap()
        .properties
        .remove(&"FutureProperty".into());
    let mut tags = rbx_dom_weak::types::Tags::new();
    tags.push("A");
    tags.push("B");
    before
        .get_by_ref_mut(before_root)
        .unwrap()
        .properties
        .insert("Tags".into(), Variant::Tags(tags));
    let mut tags = rbx_dom_weak::types::Tags::new();
    tags.push("B");
    tags.push("A");
    after
        .get_by_ref_mut(root)
        .unwrap()
        .properties
        .insert("Tags".into(), Variant::Tags(tags));
    assert_eq!(diff(&before, &after, &[])["matches"], true);
}

#[test]
fn project_diff_reports_nonserialized_properties_without_hiding_saved_physics() {
    let mut project = WeakDom::new(InstanceBuilder::new("DataModel"));
    let root = project.insert(project.root_ref(), InstanceBuilder::new("Workspace"));
    let mesh = project.insert(
        root,
        InstanceBuilder::new("MeshPart")
            .with_property("CollisionFidelity", rbx_dom_weak::types::Enum::from_u32(1))
            .with_property(
                "PhysicsData",
                rbx_dom_weak::types::BinaryString::from(vec![1, 2, 3]),
            ),
    );
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &project, project.root().children()).unwrap();
    let saved = rbx_binary::from_reader(bytes.as_slice()).unwrap();
    let mut xml = Vec::new();
    rbx_xml::to_writer(
        &mut xml,
        &project,
        project.root().children(),
        rbx_xml::EncodeOptions::new()
            .property_behavior(rbx_xml::EncodePropertyBehavior::WriteUnknown),
    )
    .unwrap();
    let saved_xml = rbx_xml::from_reader(
        xml.as_slice(),
        rbx_xml::DecodeOptions::new()
            .property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown),
    )
    .unwrap();
    assert_eq!(diff(&saved, &saved_xml, &[])["matches"], true);
    let omitted = omit_unsaved_project_properties(&mut project).unwrap();
    assert_eq!(
        omitted,
        BTreeMap::from([("CollisionFidelity".to_string(), 1)])
    );
    assert_eq!(diff(&saved, &project, &[])["matches"], true);
    project.get_by_ref_mut(mesh).unwrap().properties.insert(
        "PhysicsData".into(),
        Variant::BinaryString(rbx_dom_weak::types::BinaryString::from(vec![4, 5, 6])),
    );
    assert_eq!(diff(&saved, &project, &[])["changed"], 1);
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
