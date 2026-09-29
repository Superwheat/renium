use super::model::RbxPlaceFormat;
use rbx_dom_weak::{InstanceBuilder, WeakDom};

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
