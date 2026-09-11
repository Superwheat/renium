use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::studio::bridge::BridgeServer;

// Saved service fields unavailable to plugin property reads. Every listed
// setter has also been checked for exact Studio history cancellation.
pub(crate) fn capture_properties(class: &str) -> &'static [&'static str] {
    match class {
        "MaterialService" => &["Use2022Materials"],
        "Workspace" => &[
            "ModelStreamingBehavior",
            "StreamOutBehavior",
            "StreamingIntegrityMode",
            "StreamingTargetRadius",
            "UseNewLuauTypeSolver",
        ],
        "StarterPlayer" => &["GameSettingsAvatar"],
        "Terrain" => &["AcquisitionMethod"],
        _ => &[],
    }
}

pub(crate) fn decode_service_property(class: &str, name: &str, text: &str) -> Result<Value> {
    use rbx_dom_weak::types::{Enum, Variant};
    ensure!(
        capture_properties(class).contains(&name),
        "Unsupported native service field"
    );
    let database = rbx_reflection_database::get()?;
    let descriptor = crate::rbx::encode::rbx_model_property_descriptor(database, class, name)
        .context("Native service field is missing reflection metadata")?;
    let value = match &descriptor.data_type {
        rbx_reflection::DataType::Enum(enum_name) => {
            let number = database
                .enums
                .get(*enum_name)
                .and_then(|items| items.items.get(text))
                .with_context(|| format!("Invalid {class}.{name} enum value: {text}"))?;
            Variant::Enum(Enum::from_u32(*number))
        }
        rbx_reflection::DataType::Value(rbx_dom_weak::types::VariantType::Bool) => {
            Variant::Bool(text.parse().context("Invalid native service boolean")?)
        }
        rbx_reflection::DataType::Value(rbx_dom_weak::types::VariantType::Int32) => {
            Variant::Int32(text.parse().context("Invalid native service integer")?)
        }
        _ => anyhow::bail!("Unsupported native service field type"),
    };
    crate::rbx::decode::rbx_variant_to_settings_json(
        &value,
        Some(descriptor),
        database,
        &crate::rbx::model::BytecodeModelImportRefs::default(),
    )
    .context("Could not decode native service field")
}

// These engine setters participate in Studio's undo recording. Mesh fields use
// their saved setters, avoiding asset downloads and ApplyMesh's coupled writes.
// Player limits are managed through Roblox Game Settings, not Studio sync.
pub(crate) fn is_property(class: &str, name: &str) -> bool {
    capture_properties(class).contains(&name)
        || matches!(
            (class, name),
            ("Lighting", "LightingStyle" | "PrioritizeLightingQuality")
                | ("Terrain", "Decoration" | "SmoothGrid" | "PhysicsGrid")
                | ("TextChatService", "ChatVersion")
                | ("SurfaceAppearance", "TexturePack")
                | (
                    "MeshPart",
                    "MeshId"
                        | "MeshContent"
                        | "MeshSize"
                        | "CollisionFidelity"
                        | "RenderFidelity"
                        | "FluidFidelity"
                )
        )
}

pub(crate) fn normalize_resets(changes: &mut super::types::EditorChangeSet) -> Result<()> {
    for change in &mut changes.property_changes {
        let names = change
            .reset_properties
            .iter()
            .filter(|name| is_property(&change.class_name, name))
            .cloned()
            .collect::<Vec<_>>();
        for name in names {
            // CollisionFidelity is encoded in cooked mesh data, so reflection
            // has no serialized default. Studio can read its constructor default
            // and queue the same verified native setter used for explicit values.
            if change.class_name == "MeshPart" && name == "CollisionFidelity" {
                continue;
            }
            // Service roots cannot be instantiated just to read a default in
            // the plugin. Use the same reflection defaults as saved data.
            let value = default_value(&change.class_name, &name)?;
            change.properties.entry(name.clone()).or_insert(value);
            change.reset_properties.retain(|other| other != &name);
        }
    }
    Ok(())
}

fn default_value(class: &str, name: &str) -> Result<Value> {
    let database = rbx_reflection_database::get()?;
    let saved_name =
        crate::rbx::encode::rbx_serialized_property_name_for_logical(database, class, name)
            .unwrap_or(name);
    let value = database
        .classes
        .get(class)
        .and_then(|class| database.find_default_property(class, saved_name))
        .with_context(|| format!("No saved default is available for {class}.{name}"))?;
    crate::rbx::decode::rbx_variant_to_settings_json(
        value,
        crate::rbx::encode::rbx_model_property_descriptor(database, class, name),
        database,
        &crate::rbx::model::BytecodeModelImportRefs::default(),
    )
    .with_context(|| format!("Could not decode the saved default of {class}.{name}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootWrite {
    index: usize,
    class_name: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
    name: String,
    value: Value,
}

impl RootWrite {
    fn text(&self) -> Result<String> {
        let valid_path = match self.class_name.as_str() {
            "Lighting" | "TextChatService" | "MaterialService" | "Workspace" | "StarterPlayer" => {
                self.path_segments == [self.class_name.as_str()]
            }
            "Terrain" => {
                self.path_segments.len() == 2
                    && self.path_segments[0] == "Workspace"
                    && !self.path_segments[1].is_empty()
            }
            "MeshPart" | "SurfaceAppearance" => {
                self.path_segments.len() > 1
                    && self.path_segments.iter().all(|part| !part.is_empty())
            }
            _ => false,
        };
        ensure!(
            self.index > 0
                && valid_path
                && self.path_ordinals.len() == self.path_segments.len()
                && self.path_ordinals.first() == Some(&1)
                && self.path_ordinals.iter().all(|ordinal| *ordinal > 0)
                && is_property(&self.class_name, &self.name),
            "Unsupported native root property or target"
        );
        if self.class_name == "Terrain" && self.name == "SmoothGrid" {
            ensure!(
                self.value.is_object(),
                "Terrain sync requires native grid fields"
            );
            return Ok(String::new());
        }
        if matches!(self.class_name.as_str(), "Workspace" | "StarterPlayer")
            || self.class_name == "Terrain" && self.name == "AcquisitionMethod"
        {
            let text = if self.name == "StreamingTargetRadius" {
                let number = self
                    .value
                    .as_i64()
                    .context("StreamingTargetRadius requires an integer")?;
                ensure!(
                    (0..=i64::from(i32::MAX)).contains(&number),
                    "Invalid streaming radius"
                );
                number.to_string()
            } else {
                ensure!(
                    self.value["_type"] == "EnumItem",
                    "Native service field requires a typed enum"
                );
                self.value["name"]
                    .as_str()
                    .context("Native service enum omitted its name")?
                    .to_owned()
            };
            let decoded = decode_service_property(&self.class_name, &self.name, &text)?;
            if let Some(enum_type) = self.value.get("enumType") {
                ensure!(
                    enum_type.as_str().is_some_and(|name| {
                        name.strip_prefix("Enum.").unwrap_or(name)
                            == decoded["enumType"]
                                .as_str()
                                .unwrap_or("")
                                .trim_start_matches("Enum.")
                    }),
                    "Native service enum has the wrong type"
                );
            }
            if let Some(number) = self.value.get("value") {
                ensure!(
                    number == &decoded["value"],
                    "Native service enum has a conflicting numeric value"
                );
            }
            return Ok(text);
        }
        if self.class_name == "SurfaceAppearance" {
            return Ok(self
                .value
                .as_str()
                .context("TexturePack requires a URI string")?
                .to_owned());
        }
        if self.class_name == "MeshPart" {
            return match self.name.as_str() {
                "MeshId" | "MeshContent" => Ok(self
                    .value
                    .as_str()
                    .context("Mesh content requires a URI string")?
                    .to_owned()),
                "MeshSize" => {
                    ensure!(
                        self.value["_type"] == "Vector3",
                        "MeshSize requires a Vector3"
                    );
                    let number = |key: &str| -> Result<String> {
                        let n = self.value[key]
                            .as_f64()
                            .context("MeshSize requires three numbers")?;
                        ensure!(
                            n.is_finite() && n >= 0.0 && (n as f32).is_finite(),
                            "MeshSize must be finite and nonnegative"
                        );
                        Ok(n.to_string())
                    };
                    Ok(format!(
                        "{}, {}, {}",
                        number("x")?,
                        number("y")?,
                        number("z")?
                    ))
                }
                _ => {
                    ensure!(
                        self.value["_type"] == "EnumItem"
                            && self.value.get("enumType").is_none_or(|value| {
                                value.as_str().is_some_and(|name| {
                                    name.strip_prefix("Enum.").unwrap_or(name) == self.name
                                })
                            }),
                        "Mesh fidelity requires its typed enum value"
                    );
                    let name = self.value["name"]
                        .as_str()
                        .context("Missing mesh fidelity name")?;
                    let database = rbx_reflection_database::get()?;
                    ensure!(
                        database
                            .enums
                            .get(self.name.as_str())
                            .is_some_and(|items| items.items.contains_key(name)),
                        "Unsupported mesh fidelity value"
                    );
                    Ok(name.to_owned())
                }
            };
        }
        let enum_names: Option<&[&str]> = match self.name.as_str() {
            "LightingStyle" => Some(&["Soft", "Realistic"]),
            "ChatVersion" => Some(&["LegacyChatService", "TextChatService"]),
            _ => None,
        };
        if let Some(enum_names) = enum_names {
            ensure!(
                self.value["_type"] == "EnumItem"
                    && self
                        .value
                        .get("enumType")
                        .is_none_or(|value| value
                            .as_str()
                            .is_some_and(
                                |name| name.strip_prefix("Enum.").unwrap_or(name) == self.name
                            )),
                "{} requires its typed enum value",
                self.name
            );
            let name = self.value["name"]
                .as_str()
                .context("Missing native root enum name")?;
            ensure!(enum_names.contains(&name), "Unsupported {}", self.name);
            return Ok(name.to_string());
        }
        Ok(self
            .value
            .as_bool()
            .context("Root property requires a boolean")?
            .to_string())
    }
}

#[derive(Default)]
pub(crate) struct NativeRootVerification {
    fields: HashMap<(String, String), Map<String, Value>>,
}

impl NativeRootVerification {
    fn target_key(class: &str, path: &[String], ordinals: &[usize]) -> (String, String) {
        let mut ordinals = ordinals.to_vec();
        ordinals.resize(path.len(), 1);
        (
            class.to_string(),
            crate::bytecode::edit::instance_path_parts_key(path, &ordinals),
        )
    }

    fn record(&mut self, write: RootWrite) {
        let key = Self::target_key(
            &write.class_name,
            &write.path_segments,
            &write.path_ordinals,
        );
        let fields = self.fields.entry(key).or_default();
        match write.value {
            Value::Object(values)
                if write.class_name == "Terrain" && write.name == "SmoothGrid" =>
            {
                fields.extend(values)
            }
            value => {
                fields.insert(write.name, value);
            }
        }
    }

    pub(crate) fn verified_fields(&self, row: &crate::editor::types::EditorPropertyChange) -> u64 {
        let key = Self::target_key(&row.class_name, &row.path_segments, &row.path_ordinals);
        let Some(fields) = self.fields.get(&key) else {
            return 0;
        };
        row.properties
            .iter()
            .filter(|(name, value)| fields.get(*name) == Some(*value))
            .count() as u64
    }
}

pub(crate) fn apply(
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    transaction_id: Option<&str>,
) -> Result<NativeRootVerification> {
    let mut verified = NativeRootVerification::default();
    let Some(rows) = summary.remove("nativeRootWrites") else {
        return Ok(verified);
    };
    let writes: Vec<RootWrite> =
        serde_json::from_value(rows).context("Invalid native root writes")?;
    let transaction_id = transaction_id.context("Native root writes require a transaction")?;
    for write in writes {
        let text = write.text()?;
        apply_write(bridge, transaction_id, &write, &text)?;
        // Keep exact native readback local to this transaction and requested
        // values. Terrain also rechecks both grids at the native commit boundary.
        verified.record(write);
    }
    Ok(verified)
}

#[cfg(any(windows, target_os = "macos"))]
fn apply_write(
    bridge: &BridgeServer,
    transaction_id: &str,
    write: &RootWrite,
    text: &str,
) -> Result<()> {
    use crate::studio::bridge::BridgeTarget;
    let started = std::time::Instant::now();
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &info.runtime_id)?;
    let property_name = match (write.class_name.as_str(), write.name.as_str()) {
        ("MeshPart", "MeshContent") => "MeshId",
        ("MeshPart", "MeshSize") => "InitialSize",
        _ => &write.name,
    };
    let is_terrain = write.class_name == "Terrain" && write.name == "SmoothGrid";
    let grid = |name: &str| -> Result<Option<Vec<u8>>> {
        write
            .value
            .get(name)
            .map(|value| {
                ensure!(
                    value["_type"] == "BinaryString",
                    "Terrain grid requires a BinaryString"
                );
                base64::decode(
                    value["base64"]
                        .as_str()
                        .context("Terrain grid omitted its bytes")?,
                )
                .context("Invalid Terrain grid encoding")
            })
            .transpose()
    };
    let smooth = if is_terrain {
        grid("SmoothGrid")?
    } else {
        None
    };
    let physics = if is_terrain {
        grid("PhysicsGrid")?
    } else {
        None
    };
    ensure!(
        !is_terrain || smooth.is_some() || physics.is_some(),
        "Terrain transfer has no grids"
    );
    let mut terrain = if is_terrain {
        Some(crate::studio::native::serializer::prepare_terrain(
            pid,
            &info.place_name,
            &write.path_segments,
            &write.path_ordinals,
            std::time::Duration::from_secs(3),
        )?)
    } else {
        None
    };
    let mut property = if is_terrain {
        None
    } else {
        Some(
            crate::studio::native::serializer::prepare_property(
                pid,
                &info.place_name,
                &write.path_segments,
                &write.path_ordinals,
                property_name,
                std::time::Duration::from_secs(2),
            )
            .with_context(|| {
                format!(
                    "Preparing native sync of {}.{}",
                    write.path_segments.join("."),
                    write.name
                )
            })?,
        )
    };
    let discovered = std::time::Instant::now();
    if let Some(property) = &property {
        ensure!(
            property.class_name == write.class_name,
            "Native root target changed class"
        );
        property.ensure_writable()?;
    }
    let state = |finish: bool, changed: bool, terrain_baseline: Option<String>| {
        bridge.call_for_runtime_with_timeout(
        "getEditorTransactionState",
        serde_json::json!({"transactionId":transaction_id, "nativeRootWrite":write.index, "finishNativeRootWrite":finish, "nativeRootChanged":changed, "nativeTerrainBaseline":terrain_baseline}),
        BridgeTarget::Edit, &info.runtime_id, Some(std::time::Duration::from_secs(2)),
    )
    };
    let begin = state(false, false, None)?;
    let fenced = std::time::Instant::now();
    ensure!(
        begin["nativeRootWrite"] == write.index,
        "Studio did not prepare the native root write"
    );
    let mut changed = false;
    let result: Result<()> = (|| {
        let token = begin["historyRecording"]
            .as_str()
            .context("Studio omitted the native write recording")?;
        // A native import can start a new recording after its insertion phase.
        crate::studio::native::serializer::register_history(pid, &info.place_name, token)?;
        if let Some(terrain) = &mut terrain {
            terrain.expect(
                begin["terrainBaseline"]
                    .as_str()
                    .context("Studio omitted the Terrain baseline")?,
            )?;
            changed = terrain.write(token, smooth.as_deref(), physics.as_deref())?;
            return Ok(());
        }
        let property = property.as_mut().unwrap();
        let current = property.read()?;
        if !crate::automation::property_access::property_text_matches(
            &write.class_name,
            property_name,
            text,
            &current,
        ) {
            property.write(text)?;
            changed = true;
            ensure!(
                crate::automation::property_access::property_text_matches(
                    &write.class_name,
                    property_name,
                    text,
                    &property.read()?,
                ),
                "Studio did not retain {}.{} before commit",
                write.path_segments.join("."),
                write.name,
            );
        }
        Ok(())
    })();
    let mutated = std::time::Instant::now();
    // Release the plugin's operation fence even after a failed read/write. If
    // the daemon dies, the bounded plugin fence expires and cancels the recording.
    let finish = state(
        true,
        changed,
        if result.is_ok() {
            terrain.as_ref().map(|terrain| terrain.fingerprint())
        } else {
            None
        },
    );
    crate::app::output::log_global(
        5,
        format_args!(
            "[renium] native root phases: index={} property={} prepare_ms={:.3} fence_ms={:.3} mutate_ms={:.3} finish_ms={:.3}",
            write.index,
            property_name,
            discovered.duration_since(started).as_secs_f64() * 1000.0,
            fenced.duration_since(discovered).as_secs_f64() * 1000.0,
            mutated.duration_since(fenced).as_secs_f64() * 1000.0,
            mutated.elapsed().as_secs_f64() * 1000.0,
        ),
    );
    result?;
    finish?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn apply_write(_: &BridgeServer, _: &str, _: &RootWrite, _: &str) -> Result<()> {
    anyhow::bail!("Native root property sync requires Windows or macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terrain_receipt_only_verifies_the_written_target_and_bytes() {
        use crate::editor::types::EditorPropertyChange;
        let grid = json!({"_type": "BinaryString", "base64": "YWJj"});
        let mut row = EditorPropertyChange {
            service: "Workspace".into(),
            settings_id: None,
            path_segments: vec!["Workspace".into(), "Terrain".into()],
            path_ordinals: vec![],
            class_name: "Terrain".into(),
            properties: Map::from_iter([
                ("SmoothGrid".into(), grid.clone()),
                ("PhysicsGrid".into(), grid.clone()),
            ]),
            reset_properties: vec![],
            attributes: Map::new(),
            deleted_attributes: vec![],
        };
        assert_eq!(NativeRootVerification::default().verified_fields(&row), 0);
        let mut verified = NativeRootVerification::default();
        verified.record(RootWrite {
            index: 1,
            class_name: "Terrain".into(),
            path_segments: row.path_segments.clone(),
            path_ordinals: vec![1, 1],
            name: "SmoothGrid".into(),
            value: json!({"SmoothGrid": grid}),
        });
        assert_eq!(verified.verified_fields(&row), 1);
        row.properties.insert(
            "SmoothGrid".into(),
            json!({"_type": "BinaryString", "base64": "ZA=="}),
        );
        assert_eq!(verified.verified_fields(&row), 0);
        row.properties.insert("SmoothGrid".into(), grid);
        row.path_ordinals = vec![1, 2];
        assert_eq!(verified.verified_fields(&row), 0);
        row.path_ordinals.clear();
        row.class_name = "TerrainRegion".into();
        assert_eq!(verified.verified_fields(&row), 0);
    }

    #[test]
    fn captured_service_fields_roundtrip_through_typed_native_writes() {
        for (class, name, text) in [
            ("Workspace", "ModelStreamingBehavior", "Improved"),
            ("Workspace", "StreamOutBehavior", "Opportunistic"),
            (
                "Workspace",
                "StreamingIntegrityMode",
                "PauseOutsideLoadedArea",
            ),
            ("Workspace", "StreamingTargetRadius", "900"),
            ("Workspace", "UseNewLuauTypeSolver", "Disabled"),
            ("StarterPlayer", "GameSettingsAvatar", "R6"),
        ] {
            let mut write = RootWrite {
                index: 1,
                class_name: class.into(),
                path_segments: vec![class.into()],
                path_ordinals: vec![1],
                name: name.into(),
                value: decode_service_property(class, name, text).unwrap(),
            };
            assert_eq!(write.text().unwrap(), text, "{class}.{name}");
            write.value = default_value(class, name).unwrap();
            assert!(
                write.text().is_ok(),
                "Default {class}.{name}: {:?}",
                write.value
            );
            if name != "StreamingTargetRadius" {
                write.value["enumType"] = json!("WrongType");
                assert!(write.text().is_err());
                write.value = decode_service_property(class, name, text).unwrap();
                write.value["value"] = json!(9999);
                assert!(write.text().is_err());
                assert!(decode_service_property(class, name, "MissingValue").is_err());
            } else {
                for invalid in [json!(-1), json!(1.5), json!(2147483648_i64), json!("900")] {
                    write.value = invalid;
                    assert!(write.text().is_err());
                }
            }
            write.path_segments.push("Foreign".into());
            write.path_ordinals.push(1);
            assert!(write.text().is_err());
        }
        for (class, name) in [
            ("Workspace", "SignalBehavior"),
            ("Lighting", "Technology"),
            ("Lighting", "Outlines"),
        ] {
            assert!(!is_property(class, name));
            assert!(decode_service_property(class, name, "Default").is_err());
        }
    }

    #[test]
    fn material_mode_preserves_boolean_values_and_serialized_default() {
        let mut write = RootWrite {
            index: 1,
            class_name: "MaterialService".into(),
            path_segments: vec!["MaterialService".into()],
            path_ordinals: vec![1],
            name: "Use2022Materials".into(),
            value: json!(true),
        };
        assert_eq!(write.text().unwrap(), "true");
        write.value = json!(false);
        assert_eq!(write.text().unwrap(), "false");
        assert_eq!(
            default_value("MaterialService", "Use2022Materials").unwrap(),
            json!(false)
        );
        write.value = json!("false");
        assert!(write.text().is_err());
        write.value = json!(true);
        write.path_segments.push("Foreign".into());
        write.path_ordinals.push(1);
        assert!(write.text().is_err());
        assert!(!is_property("MaterialService", "Use2022MaterialsXml"));
        assert!(!is_property("Folder", "Use2022Materials"));
    }

    #[test]
    fn texture_pack_setter_requires_exact_class_uri_and_instance_path() {
        let mut write = RootWrite {
            index: 1,
            class_name: "SurfaceAppearance".into(),
            path_segments: vec![
                "Workspace".into(),
                "Mesh".into(),
                "SurfaceAppearance".into(),
            ],
            path_ordinals: vec![1, 2, 1],
            name: "TexturePack".into(),
            value: json!("rbxassetid://101329920584785"),
        };
        assert_eq!(write.text().unwrap(), "rbxassetid://101329920584785");
        write.value = json!("");
        assert_eq!(write.text().unwrap(), "");
        assert_eq!(
            default_value("SurfaceAppearance", "TexturePack").unwrap(),
            ""
        );
        write.value = json!({"_type":"Content", "Object":"foreign"});
        assert!(write.text().is_err());
        write.value = json!("rbxassetid://101329920584785");
        write.class_name = "Folder".into();
        assert!(write.text().is_err());
        write.class_name = "SurfaceAppearance".into();
        write.path_segments.truncate(1);
        write.path_ordinals.truncate(1);
        assert!(write.text().is_err());
        assert!(!is_property("MaterialVariant", "TexturePack"));
    }

    #[test]
    fn mesh_setters_validate_content_dimensions_and_fidelity() {
        let mut write = RootWrite {
            index: 1,
            class_name: "MeshPart".into(),
            path_segments: vec!["TestService".into(), "Mesh".into()],
            path_ordinals: vec![1, 1],
            name: "MeshContent".into(),
            value: json!("rbxassetid://140047959108196"),
        };
        assert_eq!(write.text().unwrap(), "rbxassetid://140047959108196");
        write.value = json!({"_type":"Content", "Object":"foreign"});
        assert!(write.text().is_err());
        write.name = "MeshSize".into();
        write.value = json!({"_type":"Vector3","x":1.5,"y":0,"z":3});
        assert_eq!(write.text().unwrap(), "1.5, 0, 3");
        write.value["x"] = json!(-1);
        assert!(write.text().is_err());
        write.name = "CollisionFidelity".into();
        write.value = json!({"_type":"EnumItem","name":"Hull"});
        assert_eq!(write.text().unwrap(), "Hull");
        write.value["enumType"] = json!("Material");
        assert!(write.text().is_err());
        write.value = json!({"_type":"EnumItem","name":"Missing"});
        assert!(write.text().is_err());
        write.path_segments.clear();
        assert!(write.text().is_err());
    }

    #[test]
    fn protected_root_resets_use_saved_defaults() {
        assert_eq!(default_value("Terrain", "Decoration").unwrap(), false);
        assert_eq!(
            default_value("Lighting", "PrioritizeLightingQuality").unwrap(),
            false
        );
        assert_eq!(
            default_value("Lighting", "LightingStyle").unwrap()["name"],
            "Soft"
        );
        assert!(default_value("Lighting", "NotAProperty").is_err());
    }

    #[test]
    fn native_root_writes_validate_scope_and_exact_value_types() {
        let mut write = RootWrite {
            index: 1,
            class_name: "Terrain".into(),
            path_segments: vec!["Workspace".into(), "Terrain".into()],
            path_ordinals: vec![1, 1],
            name: "Decoration".into(),
            value: json!(true),
        };
        assert_eq!(write.text().unwrap(), "true");
        write.value = json!("true");
        assert!(write.text().is_err());
        write.value = json!(false);
        write.path_ordinals[1] = 0;
        assert!(write.text().is_err());
        write.path_ordinals[1] = 1;
        write.path_segments[1] = "Other".into();
        write.path_ordinals[1] = 2;
        assert_eq!(write.text().unwrap(), "false");
        write.path_segments[0] = "OtherService".into();
        assert!(write.text().is_err());
        write.class_name = "Lighting".into();
        write.path_segments = vec!["Lighting".into()];
        write.path_ordinals = vec![1];
        write.name = "LightingStyle".into();
        write.value = json!({"_type":"EnumItem","enumType":"LightingStyle","name":"Realistic"});
        assert_eq!(write.text().unwrap(), "Realistic");
        write.value["enumType"] = json!("Material");
        assert!(write.text().is_err());
        write.value.as_object_mut().unwrap().remove("enumType");
        assert_eq!(write.text().unwrap(), "Realistic");
        assert!(!is_property("Players", "PreferredPlayersInternal"));
        write.class_name = "TextChatService".into();
        write.path_segments = vec!["TextChatService".into()];
        write.name = "ChatVersion".into();
        write.value =
            json!({"_type":"EnumItem","enumType":"Enum.ChatVersion","name":"TextChatService"});
        assert_eq!(write.text().unwrap(), "TextChatService");
        write.value["name"] = json!("Realistic");
        assert!(write.text().is_err());
        write.path_segments.push("Foreign".into());
        write.value["name"] = json!("LegacyChatService");
        assert!(write.text().is_err());
    }
}
