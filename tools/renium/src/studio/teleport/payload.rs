use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use rbx_dom_weak::{
    InstanceBuilder, WeakDom,
    types::{Ref, Variant},
};
use serde_json::{Value, json};

use crate::rbx::decode::{property_has_serialized_form, rbx_variant_to_settings_json};
use crate::rbx::encode::rbx_property_descriptor;
use crate::rbx::model::{BytecodeModelImportRefs, RbxPlaceFormat};

pub(super) const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_INSTANCES: usize = 250_000;
const SERVICES: &[&str] = &[
    "Workspace",
    "Lighting",
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer",
    "Teams",
    "SoundService",
];
const SCRIPT_STATE: &str = "__ReniumTeleportDisabled";
const REF_ID: &str = "__ReniumTeleportRef";

pub(super) struct Payload {
    pub bytes: Vec<u8>,
    pub manifest: Value,
}

pub(super) fn read(path: &Path) -> Result<Payload> {
    ensure!(
        path.metadata()?.len() <= MAX_BYTES as u64,
        "Teleport place exceeds 128 MiB"
    );
    build(RbxPlaceFormat::from_path(path)?.read(path)?)
}

fn special_property(name: &str) -> bool {
    matches!(
        name,
        "Name"
            | "Parent"
            | "Archivable"
            | "UniqueId"
            | "HistoryId"
            | "SourceAssetId"
            | "Attributes"
            | "Tags"
            | "Capabilities"
            | "Sandboxed"
    )
}

fn container_properties(dom: &WeakDom, root: Ref) -> Result<Vec<Value>> {
    let node = dom
        .get_by_ref(root)
        .context("Teleport container is missing")?;
    let database = rbx_reflection_database::get()?;
    let refs = BytecodeModelImportRefs::default();
    let mut rows = Vec::new();
    let mut values = node.properties.clone();
    if let Some(class) = database.classes.get(node.class.as_str()) {
        for ancestor in database.superclasses_iter(class) {
            for property in ancestor.properties.values() {
                if matches!(
                    property.scriptability,
                    rbx_reflection::Scriptability::ReadWrite
                ) && property_has_serialized_form(database, &node.class, property.name)
                    && !crate::editor::review::is_engine_managed_editor_property(
                        &node.class,
                        property.name,
                        database,
                    )
                    && let Some(default) = database.find_default_property(class, property.name)
                {
                    values
                        .entry(property.name.into())
                        .or_insert_with(|| default.clone());
                }
            }
        }
    }
    for (name, value) in &values {
        if special_property(name) || matches!(value, Variant::UniqueId(_)) {
            continue;
        }
        if matches!(value, Variant::Ref(reference) if reference.is_none()) {
            continue;
        }
        ensure!(
            !matches!(value, Variant::Ref(_)),
            "Teleport cannot rebind container reference {}.{name}",
            node.class
        );
        let descriptor = rbx_property_descriptor(database, &node.class, name);
        let encoded = rbx_variant_to_settings_json(value, descriptor, database, &refs)
            .with_context(|| format!("Teleport cannot represent {}.{name}", node.class))?;
        let writable = name.as_str() == "CollisionGroupData"
            || descriptor.is_some_and(|property| {
                matches!(
                    property.scriptability,
                    rbx_reflection::Scriptability::ReadWrite
                )
            });
        rows.push(json!({"name":name.as_str(),"value":encoded,"write":writable}));
    }
    rows.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(rows)
}

fn unmanaged_checks(dom: &mut WeakDom) -> Result<Vec<Value>> {
    let mut checks = Vec::new();
    let database = rbx_reflection_database::get()?;
    let mut retained = std::collections::HashSet::new();
    for root in dom.root().children().to_vec() {
        let node = dom.get_by_ref(root).context("Missing place service")?;
        let class = node.class.to_string();
        if SERVICES.contains(&class.as_str()) {
            continue;
        }
        ensure!(
            retained.insert(class.clone()),
            "Duplicate place service {class}"
        );
        ensure!(
            database
                .classes
                .get(class.as_str())
                .is_some_and(|descriptor| {
                    descriptor.tags.contains(&rbx_reflection::ClassTag::Service)
                }),
            "Unsupported saved service {class}"
        );
        let empty_service =
            !crate::roblox::services::DEFAULT_SYNC_SERVICES.contains(&class.as_str());
        let mut containers = vec![(root, vec![class.clone()])];
        for child in node.children() {
            let child_node = dom.get_by_ref(*child).context("Missing service child")?;
            ensure!(
                crate::roblox::services::is_engine_managed_container(&class, &child_node.class)
                    && child_node.children().is_empty(),
                "Teleport emulation does not support content in {class}"
            );
            containers.push((*child, vec![class.clone(), child_node.name.clone()]));
        }
        for (reference, path) in containers {
            let node = dom.get_by_ref(reference).unwrap();
            ensure!(
                node.properties.get(&"Attributes".into()).is_none_or(
                    |value| matches!(value, Variant::Attributes(values) if values.is_empty())
                ),
                "Unsupported attributes on {}",
                node.class
            );
            ensure!(
                node.properties.get(&"Tags".into()).is_none_or(
                    |value| matches!(value, Variant::Tags(values) if values.is_empty())
                ),
                "Unsupported tags on {}",
                node.class
            );
            let mut properties = container_properties(dom, reference)?;
            for row in &mut properties {
                row["write"] = json!(false);
            }
            checks.push(
                json!({"path":path,"class":node.class.as_str(),"properties":properties,
                "empty":empty_service}),
            );
        }
        dom.destroy(root);
    }
    Ok(checks)
}

fn empty_terrain(dom: &WeakDom, root: Ref) -> Result<()> {
    let terrain = dom.get_by_ref(root).context("Terrain is missing")?;
    ensure!(
        terrain.children().is_empty(),
        "Teleport emulation cannot import Terrain children"
    );
    for name in ["SmoothGrid", "PhysicsGrid"] {
        if let Some(value) = terrain.properties.get(&name.into()) {
            ensure!(
                matches!(value, Variant::BinaryString(bytes) if <_ as AsRef<[u8]>>::as_ref(bytes).is_empty()),
                "Teleport emulation cannot import saved Terrain voxels; native Play binding is unavailable"
            );
        }
    }
    Ok(())
}

fn group_roots(dom: &mut WeakDom) -> Result<Vec<(Ref, Vec<String>)>> {
    let mut services = HashMap::new();
    for root in dom.root().children().to_vec() {
        let node = dom.get_by_ref(root).context("Missing place service")?;
        let class = node.class.to_string();
        ensure!(
            services.insert(class.clone(), root).is_none(),
            "Duplicate place service {class}"
        );
    }
    let mut groups = Vec::new();
    for service in SERVICES {
        let root = *services
            .entry(service.to_string())
            .or_insert_with(|| dom.insert(dom.root_ref(), InstanceBuilder::new(*service)));
        groups.push((root, vec![service.to_string()]));
        if *service == "Workspace" {
            let camera = dom
                .get_by_ref_mut(root)
                .unwrap()
                .properties
                .remove(&"CurrentCamera".into());
            if let Some(Variant::Ref(camera)) = camera
                && dom
                    .get_by_ref(camera)
                    .is_some_and(|node| node.class == "Camera")
            {
                dom.destroy(camera);
            }
            let terrain = dom
                .get_by_ref(root)
                .unwrap()
                .children()
                .iter()
                .copied()
                .filter(|child| dom.get_by_ref(*child).unwrap().class == "Terrain")
                .collect::<Vec<_>>();
            for child in terrain {
                empty_terrain(dom, child)?;
                dom.destroy(child);
            }
        }
        if *service == "StarterPlayer" {
            for class in ["StarterPlayerScripts", "StarterCharacterScripts"] {
                let matches = dom
                    .get_by_ref(root)
                    .unwrap()
                    .children()
                    .iter()
                    .copied()
                    .filter(|child| dom.get_by_ref(*child).unwrap().class == class)
                    .collect::<Vec<_>>();
                ensure!(matches.len() <= 1, "Duplicate {class}");
                let container = matches
                    .first()
                    .copied()
                    .unwrap_or_else(|| dom.insert(root, InstanceBuilder::new(class)));
                dom.transfer_within(container, dom.root_ref());
                groups.push((container, vec![service.to_string(), class.to_string()]));
            }
            ensure!(
                dom.get_by_ref(root).unwrap().children().is_empty(),
                "Unsupported StarterPlayer children"
            );
        }
    }
    Ok(groups)
}

fn mark_scripts(dom: &mut WeakDom) -> Result<()> {
    let nodes = dom
        .descendants()
        .map(|node| node.referent())
        .collect::<Vec<_>>();
    ensure!(
        nodes.len() <= MAX_INSTANCES,
        "Teleport place exceeds 250,000 instances"
    );
    for reference in nodes {
        let node = dom.get_by_ref_mut(reference).unwrap();
        ensure!(
            node.class != "PackageLink",
            "Teleport emulation does not support linked packages"
        );
        ensure!(
            node.name != "ReniumTeleport",
            "ReniumTeleport is reserved for the playtest compatibility shim"
        );
        node.properties
            .retain(|_, value| !matches!(value, Variant::UniqueId(_)));
        let attributes = node
            .properties
            .entry("Attributes".into())
            .or_insert_with(|| Variant::Attributes(Default::default()));
        let Variant::Attributes(attributes) = attributes else {
            bail!("Invalid instance attributes");
        };
        ensure!(
            attributes.get(SCRIPT_STATE).is_none()
                && attributes.get(REF_ID).is_none()
                && attributes.get("__ReniumTeleportArchivable").is_none(),
            "Reserved teleport attribute in saved place"
        );
        attributes.insert(REF_ID.to_string(), Variant::String(reference.to_string()));
        if matches!(node.class.as_str(), "Script" | "LocalScript") {
            let disabled = node
                .properties
                .get(&"Disabled".into())
                .cloned()
                .unwrap_or(Variant::Bool(false));
            let Variant::Attributes(attributes) =
                node.properties.get_mut(&"Attributes".into()).unwrap()
            else {
                unreachable!()
            };
            attributes.insert(SCRIPT_STATE.to_string(), disabled);
            node.properties
                .insert("Disabled".into(), Variant::Bool(true));
        }
    }
    Ok(())
}

fn build(mut dom: WeakDom) -> Result<Payload> {
    let checks = unmanaged_checks(&mut dom)?;
    let groups = group_roots(&mut dom)?;
    let targets = groups
        .iter()
        .map(|(root, path)| (*root, path))
        .collect::<HashMap<_, _>>();
    let mut references = Vec::new();
    for node in dom.descendants() {
        if targets.contains_key(&node.referent()) {
            continue;
        }
        for (property, value) in &node.properties {
            if let Variant::Ref(target) = value {
                if let Some(path) = targets.get(target) {
                    references.push(json!({"id":node.referent().to_string(),"property":property.as_str(),"path":path}));
                } else {
                    ensure!(
                        target.is_none() || dom.get_by_ref(*target).is_some(),
                        "Teleport reference targets an unsupported service"
                    );
                }
            }
        }
    }
    let mut manifest = Vec::new();
    for (index, (root, path)) in groups.iter().enumerate() {
        let properties = container_properties(&dom, *root)?;
        let node = dom.get_by_ref_mut(*root).unwrap();
        node.name = format!("__ReniumTeleportGroup_{index}");
        node.class = "Folder".into();
        node.properties
            .retain(|name, _| matches!(name.as_str(), "Attributes" | "Tags"));
        manifest.push(json!({"name":node.name,"path":path,"properties":properties}));
    }
    mark_scripts(&mut dom)?;
    let roots = groups.iter().map(|(root, _)| *root).collect::<Vec<_>>();
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, &roots)?;
    ensure!(bytes.len() <= MAX_BYTES, "Teleport payload exceeds 128 MiB");
    Ok(Payload {
        bytes,
        manifest: json!({"groups":manifest,"references":references,"checks":checks}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn teleport_payload_restores_defaults_and_checks_retained_containers() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let chat = dom.insert(dom.root_ref(), InstanceBuilder::new("TextChatService"));
        dom.insert(chat, InstanceBuilder::new("ChatWindowConfiguration"));
        dom.insert(dom.root_ref(), InstanceBuilder::new("TimerService"));
        let payload = build(dom).unwrap();
        let checks = payload.manifest["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 3);
        assert!(
            checks
                .iter()
                .any(|check| check["class"] == "TimerService" && check["empty"] == true)
        );
        let lighting = payload.manifest["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["path"] == json!(["Lighting"]))
            .unwrap();
        assert!(
            lighting["properties"]
                .as_array()
                .unwrap()
                .iter()
                .any(|property| property["name"] == "Brightness")
        );
    }

    #[test]
    fn teleport_payload_rejects_duplicate_services_and_dangling_references() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        assert!(build(dom).is_err());
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let service = dom.insert(dom.root_ref(), InstanceBuilder::new("ServerStorage"));
        dom.insert(
            service,
            InstanceBuilder::new("ObjectValue").with_property("Value", Ref::new()),
        );
        assert!(build(dom).is_err());
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let service = dom.insert(dom.root_ref(), InstanceBuilder::new("ServerStorage"));
        let attributes = rbx_dom_weak::types::Attributes::new().with(REF_ID, "reserved");
        dom.insert(
            service,
            InstanceBuilder::new("Folder").with_property("Attributes", attributes),
        );
        assert!(build(dom).is_err());
    }

    #[test]
    fn teleport_payload_preserves_graph_and_disables_scripts_before_parenting() {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let storage = dom.insert(dom.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
        let part = dom.insert(storage, InstanceBuilder::new("Part").with_name("Target"));
        dom.insert(
            storage,
            InstanceBuilder::new("ObjectValue").with_property("Value", part),
        );
        dom.insert(
            storage,
            InstanceBuilder::new("ObjectValue").with_property("Value", storage),
        );
        dom.insert(
            storage,
            InstanceBuilder::new("Script").with_property("Source", "print('arrived')".to_string()),
        );
        let payload = build(dom).unwrap();
        assert_eq!(
            payload.manifest["groups"].as_array().unwrap().len(),
            SERVICES.len() + 2
        );
        assert_eq!(payload.manifest["references"].as_array().unwrap().len(), 1);
        let decoded = rbx_binary::from_reader(payload.bytes.as_slice()).unwrap();
        let script = decoded
            .descendants()
            .find(|node| node.class == "Script")
            .unwrap();
        assert_eq!(script.properties[&"Disabled".into()], Variant::Bool(true));
        assert_eq!(
            script.properties[&"Source".into()],
            Variant::String("print('arrived')".into())
        );
        let part = decoded
            .descendants()
            .find(|node| node.class == "Part")
            .unwrap();
        assert!(decoded.descendants().any(
            |node| node.properties.get(&"Value".into()) == Some(&Variant::Ref(part.referent()))
        ));
        let names = decoded
            .root()
            .children()
            .iter()
            .map(|root| &decoded.get_by_ref(*root).unwrap().name)
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), SERVICES.len() + 2);
    }

    #[test]
    fn teleport_payload_rejects_unsupported_content_before_mutation() {
        for (class, child) in [("TimerService", true), ("Folder", false)] {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let root = dom.insert(dom.root_ref(), InstanceBuilder::new(class));
            if child {
                dom.insert(root, InstanceBuilder::new("Folder"));
            }
            assert!(build(dom).is_err());
        }
        for class in ["PackageLink", "Terrain"] {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let workspace = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
            dom.insert(
                workspace,
                InstanceBuilder::new(class).with_property(
                    "SmoothGrid",
                    rbx_dom_weak::types::BinaryString::from(vec![1]),
                ),
            );
            assert!(build(dom).is_err());
        }
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let unsupported = dom.insert(dom.root_ref(), InstanceBuilder::new("TextChatService"));
        dom.insert(unsupported, InstanceBuilder::new("Folder"));
        assert!(build(dom).is_err());
    }
}
