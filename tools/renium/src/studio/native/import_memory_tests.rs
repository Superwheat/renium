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

fn fixture_bytes(build: crate::rbx::model::RbxPlaceBuild) -> Result<Vec<u8>> {
    Ok(fixture_payload_and_rows(build)?.0)
}

type FixtureRows = HashMap<String, Vec<(usize, SettingsBytecodeInstance)>>;

fn fixture_payload_and_rows(
    mut build: crate::rbx::model::RbxPlaceBuild,
) -> Result<(Vec<u8>, FixtureRows)> {
    let paths = build.import_rows.paths(&build.dom);
    let mut omitted_paths = HashMap::new();
    for (referent, names) in std::mem::take(&mut build.omitted_properties_by_ref) {
        let (segments, ordinals) = rbx_dom_instance_path_parts(&build.dom, referent);
        omitted_paths.insert(instance_path_parts_key(&segments, &ordinals), names);
    }
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
        &mut omitted_paths,
    )?;
    let rows = build
        .import_rows
        .finish(
            &paths,
            &import_property_omissions(build.omitted_properties_by_class, true),
            &omitted_paths,
        )?
        .into_iter()
        .map(|(service, rows)| (service, rows.rows))
        .collect();
    Ok((bytes, rows))
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
    let input = RbxPlaceDocument::from_file_bytes(path.clone(), bytes)?;
    // Replacing even the entire file must not change the already indexed input.
    std::fs::write(&path, b"subsequent edit, not a valid store")?;
    let actual = build_rbx_place_with_documents(
        &root,
        vec!["Workspace".into()],
        HashMap::from([("Workspace".into(), input)]),
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
fn parallel_import_reports_the_first_service_error_in_input_order() -> Result<()> {
    use crate::rbx::model::{PlaceBuildTuning, RbxPlaceDocument, build_rbx_place_with_tuning};
    let root = crate::tests::support::temp_dir("import-error-order");
    let services = vec![
        "ReplicatedStorage".to_string(),
        "Workspace".to_string(),
        "Lighting".to_string(),
    ];
    for service in &services {
        std::fs::create_dir_all(root.join(service))?;
    }
    let mut documents = services
        .iter()
        .map(|service| fixture(service))
        .collect::<Vec<_>>();
    for (document, name) in documents
        .iter_mut()
        .zip(["FirstBadReference", "SecondBadReference"])
    {
        document.instances[4].name = name.into();
        document.instances[4]
            .properties
            .insert("Value".into(), json!({"_type":"Ref", "settingsId":9}));
    }
    documents[1].instances.push(SettingsBytecodeInstance::new(
        "extra".into(),
        "Extra".into(),
        "Folder".into(),
        Some(0),
    ));
    for degree in [1, 2] {
        crate::rbx::model::CONVERSION_VISITS
            .lock()
            .unwrap()
            .insert(root.clone(), Vec::new());
        let inputs = services
            .iter()
            .zip(&documents)
            .map(|(service, document)| (service.clone(), RbxPlaceDocument::Borrowed(document)))
            .collect();
        let result = build_rbx_place_with_tuning(
            &root,
            services.clone(),
            inputs,
            true,
            true,
            false,
            PlaceBuildTuning {
                services: std::num::NonZeroUsize::new(degree).unwrap(),
                ..Default::default()
            },
        );
        let visits = crate::rbx::model::CONVERSION_VISITS
            .lock()
            .unwrap()
            .remove(&root)
            .unwrap();
        // A parallel batch may cancel the second failure before it starts.
        let required = if degree == 1 { 2 } else { 1 };
        assert!(
            services[..required]
                .iter()
                .all(|service| visits.contains(service)),
            "failing services were not converted: {visits:?}"
        );
        assert!(
            !visits.contains(&services[2]),
            "later-ranked service was converted: {visits:?}"
        );
        let error = result.err().context("invalid reference was accepted")?;
        assert!(
            format!("{error:#}").contains("FirstBadReference"),
            "{error:#}"
        );
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn import_retains_no_unused_or_null_reference_rows() -> Result<()> {
    use crate::rbx::model::PlaceImportProperties;
    let root = crate::tests::support::temp_dir("import-null-rows");
    std::fs::create_dir_all(root.join("Workspace"))?;
    let mut document = fixture("Workspace");
    // All values are carried; null references cannot become cross-batch or
    // external references and do not justify retaining their whole rows.
    document.instances[4]
        .properties
        .insert("Value".into(), json!({"_type":"Ref"}));
    document.instances.push(SettingsBytecodeInstance::new(
        "no-pivot".into(),
        "NoPivot".into(),
        "Model".into(),
        Some(0),
    ));
    for retain in [false, true] {
        let options = if retain {
            PlaceImportProperties::retaining(HashSet::from(["Workspace".into()]))
        } else {
            true.into()
        };
        let build = build_rbx_place(
            &root,
            vec!["Workspace".into()],
            Some(&HashMap::from([("Workspace".into(), &document)])),
            true,
            options,
            false,
        )?;
        if retain {
            assert!(
                build.import_rows.services["Workspace"].1.is_empty(),
                "no field needs its original row"
            );
        } else {
            assert!(build.import_rows.services.is_empty());
        }
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn imported_references_keep_originals_only_across_payload_or_live_identity_boundaries() -> Result<()>
{
    use crate::rbx::model::PlaceImportProperties;
    let root = crate::tests::support::temp_dir("import-reference-rows");
    std::fs::create_dir_all(root.join("Workspace"))?;
    for (parent, limit, viewport, expected) in [
        (2, NATIVE_IMPORT_BATCH_INSTANCES, false, false),
        (1, NATIVE_IMPORT_BATCH_INSTANCES, false, true),
        (2, 1, false, true),
        (2, NATIVE_IMPORT_BATCH_INSTANCES, true, true),
    ] {
        let mut document = fixture("Workspace");
        document.instances[4].parent_index = Some(parent);
        if viewport {
            document.instances[3].class_name = "Camera".into();
            document.instances[0].properties.insert(
                "CurrentCamera".into(),
                json!({"_type":"Ref", "settingsId":"part"}),
            );
        }
        document.write_file(&service_settings_path(&root.join("Workspace")))?;
        for files in [false, true] {
            let mut properties =
                PlaceImportProperties::retaining(HashSet::from(["Workspace".into()]));
            properties.reference_subtree_limit = Some(limit);
            let documents = HashMap::from([("Workspace".into(), &document)]);
            let build = build_rbx_place(
                &root,
                vec!["Workspace".into()],
                (!files).then_some(&documents),
                true,
                properties,
                false,
            )?;
            assert_eq!(
                build.import_rows.services["Workspace"]
                    .1
                    .iter()
                    .any(|(_, index, _)| *index == 4),
                expected,
                "parent {parent}, bound {limit}, viewport {viewport}, file {files}"
            );
        }
    }
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

#[test]
fn native_import_file_chunks_preserve_all_cross_row_facts() -> Result<()> {
    use crate::rbx::model::{PlaceBuildTuning, RbxPlaceDocument, build_rbx_place_with_tuning};
    let root =
        std::env::temp_dir().join(format!("renium-import-row-chunks-{}", std::process::id()));
    let service_dir = root.join("Workspace");
    std::fs::create_dir_all(&service_dir)?;
    let mut document = fixture("Workspace");
    let parent = document.instances.len() + 8201;
    for index in document.instances.len()..parent {
        let class = match index % 3 {
            0 => "UnionOperation",
            1 => "MeshPart",
            _ => "ObjectValue",
        };
        let mut row = SettingsBytecodeInstance::new(
            format!("row:{index}"),
            "Same".into(),
            class.into(),
            Some(parent),
        );
        if class == "ObjectValue" {
            row.properties.insert(
                "Value".into(),
                if index % 5 == 0 {
                    json!({"_type":"Ref"})
                } else {
                    json!({"_type":"Ref", "instanceIndex":index - 1})
                },
            );
        } else {
            row.properties.insert(
                "Size".into(),
                json!({"_type":"Vector3", "x":2, "y":4, "z":6}),
            );
            row.properties.insert(
                "CFrame".into(),
                json!({"_type":"CFrame", "components":[index,0,0,1,0,0,0,1,0,0,0,1]}),
            );
            let key = if class == "MeshPart" {
                "TexturePack"
            } else {
                "ChildData2"
            };
            row.properties.insert(key.into(), json!({"_type":"SharedString", "base64":base64::encode([b"payload held across chunks".repeat(20), (index % 5).to_le_bytes().to_vec()].concat())}));
        }
        row.attributes.insert("AcrossChunks".into(), json!(index));
        if index % 4 != 0 {
            row.properties
                .insert("FutureUnreflectedFlag".into(), json!(index % 5 < 2));
            row.properties.insert(
                "FutureUnreflectedText".into(),
                json!(format!("value:{}", index % 71)),
            );
        }
        document.instances.push(row);
    }
    let mut model = SettingsBytecodeInstance::new(
        "late-parent".into(),
        "Model".into(),
        "Model".into(),
        Some(0),
    );
    model.properties.insert("Scale".into(), json!(0.25));
    document.instances.push(model);
    // Source filenames depend on a property column, not only the hierarchy.
    document.instances[5].class_name = "Script".into();
    document.instances[5].properties.insert(
        "RunContext".into(),
        json!({"_type":"EnumItem", "enumType":"RunContext", "name":"Client", "value":2}),
    );
    let source_paths = crate::editor::paths::build_editor_source_paths_by_index(
        &document,
        "Workspace",
        &service_dir,
    );
    let source = source_paths[5].as_ref().unwrap();
    std::fs::create_dir_all(source.parent().unwrap())?;
    std::fs::write(source, "print('source selected by RunContext')\n")?;
    let path = service_settings_path(&service_dir);
    document.write_file(&path)?;
    // Compare the same encoded store, including wire normalization, through
    // every route; the small golden separately pins the original main bytes.
    let document =
        crate::settings::bytecode::decode_settings_bytecode_holding(&std::fs::read(path)?)?;
    let second_dir = root.join("ReplicatedStorage");
    std::fs::create_dir_all(&second_dir)?;
    let mut second = fixture("ReplicatedStorage");
    // Force cross-service path resolution, including the second same-named
    // ancestor, without an identity or row-index shortcut.
    second.instances[4].properties.insert(
        "Value".into(),
        json!({
            "_type":"Ref", "pathSegments":["Workspace", "Same", "Geometry"],
            "pathOrdinals":[1,2,1]
        }),
    );
    let second_path = service_settings_path(&second_dir);
    second.write_file(&second_path)?;
    let second =
        crate::settings::bytecode::decode_settings_bytecode_holding(&std::fs::read(second_path)?)?;
    let documents = HashMap::from([
        ("Workspace".to_string(), document),
        ("ReplicatedStorage".to_string(), second),
    ]);
    let services = vec!["Workspace".to_string(), "ReplicatedStorage".to_string()];
    let expected = fixture_payload_and_rows(build_rbx_place(
        &root,
        services.clone(),
        Some(
            &documents
                .iter()
                .map(|(name, document)| (name.clone(), document))
                .collect(),
        ),
        true,
        crate::rbx::model::PlaceImportProperties::retaining(services.iter().cloned().collect()),
        false,
    )?)?;
    for (size, parallel_services, workers) in
        [(1, 1, 1), (7, 2, 4), (4096, 4, 4), (usize::MAX, 1, 4)]
    {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()?;
        for mode in ["file", "owned", "shared"] {
            let inputs = documents
                .iter()
                .filter_map(|(name, document)| match mode {
                    "owned" => Some((name.clone(), RbxPlaceDocument::Owned(document.clone()))),
                    "shared" => Some((
                        name.clone(),
                        RbxPlaceDocument::Shared(std::sync::Arc::new(document.clone())),
                    )),
                    _ => None,
                })
                .collect();
            let build = pool.install(|| {
                let mut properties = crate::rbx::model::PlaceImportProperties::retaining(
                    services.iter().cloned().collect(),
                );
                properties.reference_subtree_limit = Some(NATIVE_IMPORT_BATCH_INSTANCES);
                build_rbx_place_with_tuning(
                    &root,
                    services.clone(),
                    inputs,
                    true,
                    properties,
                    false,
                    PlaceBuildTuning {
                        rows: std::num::NonZeroUsize::new(size).unwrap(),
                        services: std::num::NonZeroUsize::new(parallel_services).unwrap(),
                        prefetch: parallel_services != 1,
                    },
                )
            })?;
            let actual = fixture_payload_and_rows(build)?;
            assert!(
                actual.0 == expected.0,
                "{mode}, chunk size {size}, services {parallel_services}, workers {workers}: payload bytes changed"
            );
            assert_eq!(
                actual.1, expected.1,
                "{mode}, chunk size {size}: retained original rows changed"
            );
        }
    }
    std::fs::remove_dir_all(root)?;
    Ok(())
}
