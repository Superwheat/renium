use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::studio::bridge::BridgeServer;

const GENERATED_PROPERTIES: [&str; 5] = [
    "PhysicalConfigData",
    "UnscaledCofm",
    "UnscaledVolInertiaDiags",
    "UnscaledVolInertiaOffDiags",
    "UnscaledVolume",
];

// Cooking changes these serialized fields without corresponding Changed signals.
// Adopt only fields not explicitly changed in the requested saved state.
pub(crate) fn generated_properties(
    before: &crate::settings::bytecode::SettingsBytecodeInstance,
    desired: &crate::settings::bytecode::SettingsBytecodeInstance,
) -> Vec<String> {
    use crate::settings::equivalence::reconciliation_property_values_equal as equal;
    if before.class_name != "MeshPart"
        || desired.class_name != "MeshPart"
        || equal(
            "MeshPart",
            "CollisionFidelity",
            before.properties.get("CollisionFidelity"),
            desired.properties.get("CollisionFidelity"),
        )
    {
        return Vec::new();
    }
    GENERATED_PROPERTIES
        .into_iter()
        .filter(|name| {
            equal(
                "MeshPart",
                name,
                before.properties.get(*name),
                desired.properties.get(*name),
            )
        })
        .map(str::to_string)
        .collect()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeometryReadback {
    pub(crate) service: String,
    pub(crate) settings_id: String,
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) properties: Vec<String>,
}

pub(crate) fn capture_generated(
    bridge: &BridgeServer,
    changes: &mut super::types::EditorChangeSet,
    src_root: &std::path::Path,
    transaction_id: Option<&str>,
) -> Result<()> {
    use super::types::EditorSettingsWrite;
    if changes.geometry_readbacks.is_empty() {
        return Ok(());
    }
    let transaction_id = transaction_id.context("Mesh readback requires an active transaction")?;
    let runtime =
        bridge.cached_bridge_info_for_target(crate::studio::bridge::BridgeTarget::Edit)?;
    let mut services = std::collections::BTreeMap::<String, Vec<GeometryReadback>>::new();
    for target in std::mem::take(&mut changes.geometry_readbacks) {
        services
            .entry(target.service.clone())
            .or_default()
            .push(target);
    }
    for (service, targets) in services {
        let path = crate::system::files::service_settings_path(&src_root.join(&service));
        let index = if let Some(index) = changes
            .settings_writes
            .iter()
            .position(|write| write.path == path)
        {
            index
        } else {
            let document = super::document::read_editor_service_settings(src_root, &service)?
                .context("Mesh settings document disappeared during sync")?;
            let index = changes.settings_writes.len();
            changes.settings_writes.push(EditorSettingsWrite {
                expected_hash: super::sync::settings_file_hash(&path)?,
                path,
                document,
            });
            index
        };
        let document = &mut changes.settings_writes[index].document;
        let by_id = document
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| (instance.settings_id.clone(), index))
            .collect::<std::collections::HashMap<_, _>>();
        for target in targets {
            let index = *by_id
                .get(&target.settings_id)
                .context("Mesh identity disappeared during sync")?;
            ensure!(
                document.instances[index].class_name == "MeshPart",
                "Mesh identity changed class"
            );
            let state = bridge.call_for_runtime_with_timeout(
                "getEditorTransactionState",
                serde_json::json!({"transactionId": transaction_id, "meshGeometry": target}),
                crate::studio::bridge::BridgeTarget::Edit,
                &runtime.runtime_id,
                Some(std::time::Duration::from_secs(3)),
            )?;
            let encoded = state["meshGeometry"]
                .as_str()
                .context("Studio omitted mesh geometry readback")?;
            let bytes = base64::decode(encoded).context("Invalid mesh geometry encoding")?;
            merge_geometry_readback(
                &mut document.instances[index].properties,
                &target.properties,
                &bytes,
            )?;
        }
    }
    Ok(())
}

fn merge_geometry_readback(
    properties: &mut Map<String, Value>,
    names: &[String],
    bytes: &[u8],
) -> Result<()> {
    use crate::rbx::{
        decode::rbx_variant_to_settings_json, encode::rbx_model_property_descriptor,
        model::BytecodeModelImportRefs,
    };
    let dom = rbx_binary::from_reader(std::io::Cursor::new(bytes))
        .context("Invalid mesh geometry snapshot")?;
    ensure!(
        dom.root().children().len() == 1,
        "Mesh geometry snapshot must contain one mesh"
    );
    let mesh = dom
        .get_by_ref(dom.root().children()[0])
        .context("Mesh snapshot is missing its root")?;
    ensure!(
        mesh.class.as_str() == "MeshPart" && mesh.children().is_empty(),
        "Unexpected mesh geometry snapshot contents"
    );
    let database = rbx_reflection_database::get()?;
    for name in names {
        ensure!(
            GENERATED_PROPERTIES.contains(&name.as_str()),
            "Unsupported generated mesh property"
        );
        if let Some(value) = mesh.properties.get(&name.as_str().into()) {
            let value = rbx_variant_to_settings_json(
                value,
                rbx_model_property_descriptor(database, "MeshPart", name),
                database,
                &BytecodeModelImportRefs::default(),
            )
            .with_context(|| format!("Could not decode generated mesh {name}"))?;
            properties.insert(name.clone(), value);
        } else {
            properties.remove(name);
        }
    }
    Ok(())
}

pub(crate) fn is_mesh_geometry_property(class: &str, name: &str) -> bool {
    class == "MeshPart"
        && matches!(
            name,
            "UnscaledCofm"
                | "UnscaledVolInertiaDiags"
                | "UnscaledVolInertiaOffDiags"
                | "UnscaledVolume"
        )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeometryWrite {
    class_name: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
    name: String,
    value: Value,
}

impl GeometryWrite {
    fn text(&self) -> Result<String> {
        ensure!(
            is_mesh_geometry_property(&self.class_name, &self.name),
            "Unsupported native mesh property"
        );
        ensure!(
            self.path_segments.len() > 1
                && self.path_segments.len() == self.path_ordinals.len()
                && self.path_ordinals.iter().all(|ordinal| *ordinal > 0),
            "Invalid mesh geometry path"
        );
        let number = |value: &Value| -> Result<f64> {
            let number = value
                .as_f64()
                .context("Mesh geometry requires numeric values")?;
            ensure!(
                number.is_finite() && (number as f32).is_finite(),
                "Mesh geometry value is outside the finite Float32 range"
            );
            Ok(number)
        };
        if self.name == "UnscaledVolume" {
            return Ok(number(&self.value)?.to_string());
        }
        ensure!(
            self.value["_type"] == "Vector3",
            "Mesh geometry requires a Vector3"
        );
        Ok(format!(
            "{}, {}, {}",
            number(&self.value["x"])?,
            number(&self.value["y"])?,
            number(&self.value["z"])?
        ))
    }
}

pub(crate) fn apply(
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    transaction_id: Option<&str>,
) -> Result<()> {
    let Some(rows) = summary.remove("nativeGeometryWrites") else {
        return Ok(());
    };
    let writes: Vec<GeometryWrite> =
        serde_json::from_value(rows).context("Invalid mesh geometry writes from Studio")?;
    if writes.is_empty() {
        return Ok(());
    }
    let transaction_id =
        transaction_id.context("Mesh geometry sync requires an active transaction")?;
    for write in writes {
        let text = write.text()?;
        let state = bridge.call(
            "getEditorTransactionState",
            serde_json::json!({"transactionId": transaction_id}),
        )?;
        ensure!(
            matches!(state["state"].as_str(), Some("open" | "prepared")),
            "Mesh geometry transaction is no longer active"
        );
        apply_write(bridge, &write, &text)?;
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
fn apply_write(bridge: &BridgeServer, write: &GeometryWrite, text: &str) -> Result<()> {
    use crate::studio::bridge::BridgeTarget;
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &info.runtime_id)?;
    let mut property = crate::studio::native::serializer::prepare_property(
        pid,
        &info.place_name,
        &write.path_segments,
        &write.path_ordinals,
        &write.name,
        std::time::Duration::from_secs(3),
    )?;
    ensure!(
        property.class_name == "MeshPart",
        "Native mesh target changed class"
    );
    let current = property.read()?;
    if !crate::automation::property_access::property_text_matches(
        &property.class_name,
        &write.name,
        text,
        &current,
    ) {
        property.write(text)?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn apply_write(_: &BridgeServer, _: &GeometryWrite, _: &str) -> Result<()> {
    anyhow::bail!("Native mesh geometry sync requires Windows or macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collision_cooking_refreshes_only_unedited_geometry_fields() {
        use crate::settings::bytecode::SettingsBytecodeInstance;
        let before = SettingsBytecodeInstance {
            settings_id: "mesh".into(),
            name: "Mesh".into(),
            class_name: "MeshPart".into(),
            parent_index: Some(0),
            properties: Map::from_iter([("UnscaledVolume".into(), json!(7))]),
            attributes: Map::new(),
        };
        let mut desired = before.clone();
        assert!(generated_properties(&before, &desired).is_empty());
        desired.properties.insert(
            "CollisionFidelity".into(),
            json!({"_type":"EnumItem", "enumType":"CollisionFidelity", "name":"Hull"}),
        );
        assert_eq!(generated_properties(&before, &desired).len(), 5);
        desired.properties.insert("UnscaledVolume".into(), json!(9));
        let names = generated_properties(&before, &desired);
        assert_eq!(names.len(), 4);
        assert!(!names.iter().any(|name| name == "UnscaledVolume"));
        desired
            .properties
            .insert("PhysicalConfigData".into(), json!("explicit"));
        assert_eq!(generated_properties(&before, &desired).len(), 3);
        desired.class_name = "Part".into();
        assert!(generated_properties(&before, &desired).is_empty());
    }

    #[test]
    fn generated_mesh_readback_preserves_other_fields_and_decodes_native_values() -> Result<()> {
        let dom = rbx_dom_weak::WeakDom::new(
            rbx_dom_weak::InstanceBuilder::new("MeshPart")
                .with_property("UnscaledVolume", 12.5_f32),
        );
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[dom.root_ref()])?;
        let mut properties = Map::from_iter([
            ("UnscaledVolume".into(), json!(7)),
            ("Transparency".into(), json!(0.5)),
        ]);
        merge_geometry_readback(&mut properties, &["UnscaledVolume".into()], &bytes)?;
        assert_eq!(properties["UnscaledVolume"], 12.5);
        assert_eq!(properties["Transparency"], 0.5);
        assert!(
            merge_geometry_readback(&mut properties, &["Capabilities".into()], &bytes).is_err()
        );
        assert!(
            merge_geometry_readback(&mut properties, &["UnscaledVolume".into()], b"invalid")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn geometry_sync_accepts_only_saved_mesh_mass_values() {
        let mut write = GeometryWrite {
            class_name: "MeshPart".into(),
            path_segments: vec!["Workspace".into(), "Mesh".into()],
            path_ordinals: vec![1, 1],
            name: "UnscaledCofm".into(),
            value: json!({"_type":"Vector3","x":1,"y":-2,"z":3}),
        };
        assert_eq!(write.text().unwrap(), "1, -2, 3");
        write.value["x"] = json!(1e100);
        assert!(write.text().is_err());
        write.name = "UnscaledVolume".into();
        write.value = json!(754.8604125976562);
        assert!(write.text().is_ok());
        write.name = "Capabilities".into();
        assert!(write.text().is_err());
        write.name = "UnscaledVolume".into();
        write.class_name = "Script".into();
        assert!(write.text().is_err());
    }
}
