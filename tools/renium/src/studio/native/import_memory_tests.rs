use super::*;

fn fixture(service: &str) -> SettingsBytecode {
    let mut rows = vec![SettingsBytecodeInstance::new(
        format!("root:{service}"),
        service.into(),
        service.into(),
        None,
    )];
    for (id, name, class, parent, properties) in [
        ("same-a", "Same", "Folder", 0, json!({})),
        ("same-b", "Same", "Folder", 0, json!({})),
        (
            "part",
            "Geometry",
            "Part",
            2,
            json!({
                "Anchored": true, "Transparency": 0.375,
                "Size": {"_type":"Vector3", "x":2, "y":3, "z":4},
                "Material": {"_type":"EnumItem", "enumType":"Material", "name":"Wood", "value":512}
            }),
        ),
        (
            "ref",
            "Pointer",
            "ObjectValue",
            1,
            json!({
                "Value":{"_type":"Ref", "settingsId":"part", "pathSegments":[service,"Same","Geometry"], "pathOrdinals":[1,2,1]}
            }),
        ),
        (
            "script",
            "Logic",
            "ModuleScript",
            1,
            json!({"Source":"return 42\n"}),
        ),
        (
            "blob",
            "Payload",
            "MeshPart",
            2,
            json!({
                "TexturePack":{"_type":"SharedString", "base64":base64::encode(b"fixture payload".repeat(64))}
            }),
        ),
    ] {
        let mut row =
            SettingsBytecodeInstance::new(id.into(), name.into(), class.into(), Some(parent));
        row.properties = properties.as_object().unwrap().clone();
        row.attributes.insert("Preserved".into(), json!(true));
        rows.push(row);
    }
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: rows,
    }
}

#[test]
fn native_import_payload_matches_main_ad5_bytes() -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "renium-import-memory-fixture-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root)?;
    let services = ["Workspace", "ReplicatedStorage"];
    let documents = services
        .iter()
        .map(|service| {
            std::fs::create_dir_all(root.join(service)).unwrap();
            (service.to_string(), fixture(service))
        })
        .collect::<HashMap<_, _>>();
    for mode in ["borrowed", "transferred", "files"] {
        let ordered_services = services.iter().map(|s| s.to_string()).collect();
        let build = match mode {
            "borrowed" => {
                let overrides = documents
                    .iter()
                    .map(|(service, doc)| (service.clone(), doc))
                    .collect();
                build_rbx_place(&root, ordered_services, Some(&overrides), true, true, false)?
            }
            "transferred" => {
                let mut owners = Vec::new();
                let transferred = documents
                    .iter()
                    .map(|(service, document)| {
                        let document = std::sync::Arc::new(document.clone());
                        owners.push(std::sync::Arc::downgrade(&document));
                        (
                            service.clone(),
                            crate::rbx::model::RbxPlaceDocument::Shared(document),
                        )
                    })
                    .collect();
                let build = crate::rbx::model::build_rbx_place_with_documents(
                    &root,
                    ordered_services,
                    transferred,
                    true,
                    true,
                    false,
                )?;
                assert!(owners.iter().all(|owner| owner.upgrade().is_none()));
                build
            }
            _ => {
                for (service, document) in &documents {
                    document.write_file(&service_settings_path(&root.join(service)))?;
                }
                build_rbx_place(&root, ordered_services, None, true, true, false)?
            }
        };
        assert_eq!(
            fixture_bytes(build)?.as_slice(),
            include_bytes!("fixtures/import-main-ad5.rbxm"),
            "{mode}"
        );
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}

fn fixture_bytes(mut build: crate::rbx::model::RbxPlaceBuild) -> Result<Vec<u8>> {
    let groups = pending_editor_binary_groups(&build.dom, &build.service_roots)?;
    let plans = groups
        .iter()
        .map(|_| EditorPackageGroupPlan {
            retained_roots: Vec::new(),
            package_roots: Vec::new(),
            mutation_package_roots: Vec::new(),
            change_generation: Some(0),
        })
        .collect();
    clear_import_engine_identities(&mut build.dom);
    let (bytes, _, _) = encode_services(
        &mut build.dom,
        &build.service_roots,
        &build.settings_ids_by_ref,
        groups,
        plans,
        &mut HashMap::new(),
    )?;
    Ok(bytes)
}

#[test]
fn native_import_consumes_values_without_losing_ancestor_geometry_or_other_owners() -> Result<()> {
    use crate::rbx::model::{RbxPlaceDocument, build_rbx_place_with_documents};
    use std::sync::Arc;
    let root = std::env::temp_dir().join(format!("renium-import-geometry-{}", std::process::id()));
    std::fs::create_dir_all(root.join("Workspace"))?;
    let mut document = fixture("Workspace");
    document.instances[2].class_name = "Model".into();
    document.instances[2]
        .properties
        .insert("Scale".into(), json!(0.25));
    document.instances[3].class_name = "Model".into();
    document.instances[3].properties.clear();
    document.instances[3]
        .properties
        .insert("Scale".into(), json!(0.5));
    document.instances[6].parent_index = Some(3);
    document.instances[6].properties.insert(
        "Size".into(),
        json!({"_type":"Vector3", "x":2, "y":4, "z":6}),
    );
    let expected_document = serde_json::to_value(&document)?;
    let mut expected_bytes = None;
    for shared in [false, true] {
        let document = Arc::new(document.clone());
        let owner = shared.then(|| Arc::clone(&document));
        let weak = Arc::downgrade(&document);
        let build = build_rbx_place_with_documents(
            &root,
            vec!["Workspace".into()],
            HashMap::from([("Workspace".into(), RbxPlaceDocument::Shared(document))]),
            true,
            true,
            false,
        )?;
        let mesh_ref = build
            .settings_ids_by_ref
            .iter()
            .find(|(_, id)| id.as_str() == "blob")
            .unwrap()
            .0;
        let mesh = build.dom.get_by_ref(*mesh_ref).unwrap();
        assert_eq!(
            mesh.properties[&crate::roblox::schema::MESH_INITIAL_SIZE_PROPERTY.into()],
            RbxVariant::Vector3(rbx_dom_weak::types::Vector3::new(16.0, 32.0, 48.0))
        );
        let bytes = fixture_bytes(build)?;
        if let Some(expected) = &expected_bytes {
            assert_eq!(&bytes, expected);
        } else {
            expected_bytes = Some(bytes);
        }
        if let Some(owner) = owner {
            assert_eq!(serde_json::to_value(owner.as_ref())?, expected_document);
            assert_eq!(Arc::strong_count(&owner), 1);
        }
        assert!(weak.upgrade().is_none());
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn native_import_file_values_come_from_the_indexed_snapshot() -> Result<()> {
    use crate::rbx::model::{RbxPlaceDocument, build_rbx_place_with_documents};
    let root = std::env::temp_dir().join(format!(
        "renium-import-file-snapshot-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("Workspace"))?;
    let path = service_settings_path(&root.join("Workspace"));
    let document = fixture("Workspace");
    document.write_file(&path)?;
    let bytes = std::fs::read(&path)?;
    let structure = SettingsBytecode::decode_structure(&bytes)?;
    // Replacing even the entire file must not change the already indexed input.
    std::fs::write(&path, b"subsequent edit, not a valid store")?;
    let actual = build_rbx_place_with_documents(
        &root,
        vec!["Workspace".into()],
        HashMap::from([(
            "Workspace".into(),
            RbxPlaceDocument::File {
                structure,
                path,
                bytes,
            },
        )]),
        true,
        true,
        false,
    )?;
    let expected = build_rbx_place(
        &root,
        vec!["Workspace".into()],
        Some(&HashMap::from([("Workspace".into(), &document)])),
        true,
        true,
        false,
    )?;
    assert_eq!(fixture_bytes(actual)?, fixture_bytes(expected)?);
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
#[ignore = "read-only allocation probe: set RENIUM_IMPORT_PROBE_SRC and RENIUM_PROFILE_MEMORY"]
fn import_document_lifetime_probe() -> Result<()> {
    use crate::rbx::model::{RbxPlaceDocument, build_rbx_place_with_documents};
    use clap::Parser;
    use std::sync::Arc;
    let src = std::path::PathBuf::from(
        std::env::var_os("RENIUM_IMPORT_PROBE_SRC").context("Set RENIUM_IMPORT_PROBE_SRC")?,
    );
    crate::project::config::try_load_project(None, src.parent())?
        .context("Import probe requires a scratch project with registered store layout")?;
    let cli = crate::cli::Cli::try_parse_from([
        "rbx",
        "--log-level",
        "trace",
        "--color",
        "never",
        "status",
    ])?;
    crate::app::output::configure(&cli);
    let started = Instant::now();
    let _request = crate::app::profile::request(started);
    let services = crate::roblox::services::DEFAULT_SYNC_SERVICES
        .iter()
        .filter(|service| service_settings_path(&src.join(service)).is_file())
        .map(|service| service.to_string())
        .collect::<Vec<_>>();
    let docs = if std::env::var_os("RENIUM_IMPORT_PROBE_FILES").is_some() {
        HashMap::new()
    } else {
        services
            .par_iter()
            .map(|service| {
                let path = service_settings_path(&src.join(service));
                let document = crate::settings::bytecode::decode_settings_bytecode_holding(
                    &std::fs::read(&path)?,
                )?;
                Ok((
                    service.clone(),
                    RbxPlaceDocument::Shared(Arc::new(document)),
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?
    };
    crate::app::profile::record("probe prepared document owners", started);
    let build = build_rbx_place_with_documents(&src, services, docs, true, true, false)?;
    crate::app::profile::record("probe place built with documents released", started);
    let bytes = fixture_bytes(build)?;
    if let Some(path) = std::env::var_os("RENIUM_IMPORT_PROBE_OUTPUT") {
        std::fs::write(path, &bytes)?;
    }
    crate::app::profile::record(
        &format!("probe native payload: {} bytes", bytes.len()),
        started,
    );
    eprintln!(
        "{}",
        crate::app::profile::report("consuming import probe", started).unwrap_or_default()
    );
    Ok(())
}
