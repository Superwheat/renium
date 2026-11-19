use super::*;
use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};
use crate::editor::types::EditorInstanceChange;
use crate::studio::bridge::routing_tests::{connect, edit_info, fixture};
use tungstenite::{Message, WebSocket, protocol::Role};

fn check_scoped_additive_route(absent: bool, external: bool, prepared: bool) -> Result<()> {
    let service = "Workspace";
    let root = crate::tests::support::temp_dir("scoped-additive-route");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let mut args = PushEditorChangesArgs::new(
        ProjectSourceArgs {
            project_root: root.clone(),
            src_root: "src".into(),
        },
        BridgeConnectionArgs::local(0.1),
    );
    // Existing roots must not even attempt discovery. Absent roots use the
    // ordinary binary transport so this route test needs no live process.
    args.disable_service_reader = absent;
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    for (id, name, class, parent) in [
        ("root", service, service, None),
        ("old", "ExistingCamera", "Camera", Some(0)),
        ("new", "Incoming", "Model", Some(0)),
        ("part", "Part", "Part", Some(2)),
        ("value", "Target", "ObjectValue", Some(2)),
    ] {
        document.instances.push(SettingsBytecodeInstance::new(
            id.into(),
            name.into(),
            class.into(),
            parent,
        ));
    }
    document.instances[0].properties.insert(
        "CurrentCamera".into(),
        json!({"_type":"Ref","settingsId":"old"}),
    );
    document.instances[3]
        .properties
        .insert("Transparency".into(), json!(0.375));
    document.instances[4].properties.insert(
        "Value".into(),
        if external {
            json!({"_type":"Ref","settingsId":"old"})
        } else {
            json!({"_type":"Ref","instanceIndex":4})
        },
    );
    for index in 0..256 {
        let mut row = SettingsBytecodeInstance::new(
            format!("unrelated:{index}"),
            "Unrelated".into(),
            "Folder".into(),
            Some(0),
        );
        row.attributes
            .insert("Payload".into(), json!("x".repeat(4096)));
        document.instances.push(row);
    }
    let path = service_settings_path(&root.join("src").join(service));
    std::fs::create_dir_all(path.parent().unwrap())?;
    document.write_file(&path)?;
    let mut changes = EditorChangeSet {
        instance_changes: vec![EditorInstanceChange {
            service: service.into(),
            mode: "upsertInstances".into(),
            allow_deletes: false,
            instances: vec![crate::editor::types::EditorInstanceDescriptor {
                settings_id: "new".into(),
                path_segments: vec![service.into(), "Incoming".into()],
                path_ordinals: vec![1, 1],
                class_name: "Model".into(),
                ..Default::default()
            }],
            preserve_instances: Vec::new(),
        }],
        ..Default::default()
    };
    if prepared {
        changes
            .native_property_documents
            .insert(service.into(), std::sync::Arc::new(document.clone()));
    }
    let bridge = fixture();
    let stream = connect(&bridge, 0, edit_info("scoped-additive", 21));
    let peer = std::thread::spawn(move || {
        let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
        let mut requests = 0;
        while let Ok(Message::Text(text)) = socket.read() {
            let req: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(req["method"], "getEditorServiceChangeGenerations");
            assert_eq!(req["params"]["rootNames"][service], json!(["Incoming"]));
            assert_eq!(
                req["params"]["rootIds"][service]["Incoming"],
                json!(["new"])
            );
            requests += 1;
            let names = if absent { vec!["Incoming"] } else { Vec::new() };
            socket.send(Message::Text(json!({"id":req["id"],"ok":true,"result":{
                "generations":{(service):7},"hasPackageLinks":{(service):false},"absentRoots":{(service):names}
            }}).to_string().into())).unwrap();
        }
        assert_eq!(requests, 1, "existence was requested twice");
    });
    READER_PREFLIGHTS.set(0);
    let selection = select_editor_binary_import(&args, &changes, &bridge)?;
    assert_eq!(selection.is_some(), absent);
    if !absent {
        assert_eq!(READER_PREFLIGHTS.get(), 0);
    }
    let import = build_editor_binary_import(&args, &changes, &bridge, selection)?;
    let reads = ADDITIVE_STORE_READS
        .lock()
        .unwrap()
        .remove(&path)
        .unwrap_or_default();
    if absent {
        assert_eq!(
            reads,
            if prepared {
                (0, 0, false)
            } else {
                (
                    1,
                    if external {
                        document.instances.len()
                    } else {
                        3
                    },
                    external,
                )
            },
            "selected rows or one complete decode from the retained columns"
        );
        let import = import.context("absent root was not imported")?;
        assert_eq!(import.groups.len(), 1);
        assert_eq!(import.groups[0].change_generation, Some(7));
        let dom = rbx_binary::from_reader(import.bytes.as_slice())?;
        let part = dom.descendants().find(|row| row.name == "Part").unwrap();
        assert_eq!(
            part.properties[&"Transparency".into()],
            RbxVariant::Float32(0.375)
        );
        assert!(
            !dom.descendants()
                .any(|row| matches!(row.name.as_str(), "Unrelated" | "ExistingCamera"))
        );
        if external {
            assert!(
                import
                    .post_apply_properties_by_path
                    .get(&instance_path_parts_key(
                        &[service.into(), "Incoming".into(), "Target".into()],
                        &[1, 1, 1]
                    ))
                    .is_some_and(|names| names.contains("Value")),
                "outside reference must be applied after import"
            );
        } else {
            let target = dom.descendants().find(|row| row.name == "Target").unwrap();
            assert_eq!(
                target.properties[&"Value".into()],
                RbxVariant::Ref(part.referent())
            );
        }
    } else {
        assert!(import.is_none());
    }
    drop(bridge);
    peer.join().unwrap();
    Ok(())
}

#[test]
fn existing_additive_root_reads_no_store_and_builds_nothing() -> Result<()> {
    check_scoped_additive_route(false, false, false)
}

#[test]
fn absent_additive_root_materializes_its_subtree_with_internal_references() -> Result<()> {
    check_scoped_additive_route(true, false, false)?;
    check_scoped_additive_route(true, false, true)
}

#[test]
fn additive_external_reference_keeps_the_complete_import_index() -> Result<()> {
    check_scoped_additive_route(true, true, false)
}

#[test]
fn additive_invalid_reference_is_not_retried_as_a_store_decode() -> Result<()> {
    let root = crate::tests::support::temp_dir("additive-invalid-reference");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let service = "Workspace";
    let dir = root.join(service);
    std::fs::create_dir_all(&dir)?;
    let path = service_settings_path(&dir);
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance::new("service".into(), service.into(), service.into(), None),
            SettingsBytecodeInstance::new("new".into(), "New".into(), "Folder".into(), Some(0)),
        ],
    };
    document.instances[1].properties.insert(
        "Nested".into(),
        json!([{"ref":{"_type":"Ref", "settingsId":""}}]),
    );
    document.write_file(&path)?;
    let error =
        additive_document_from_file(path.clone(), service, &HashSet::from(["New".into()]), &dir)
            .err()
            .context("invalid reference accepted")?;
    let message = format!("{error:#}");
    assert!(
        message.contains("Ref settingsId must be a non-empty string"),
        "{message}"
    );
    assert!(!message.contains("Failed to decode"), "{message}");
    assert_eq!(
        ADDITIVE_STORE_READS
            .lock()
            .unwrap()
            .remove(&path)
            .unwrap()
            .0,
        1
    );
    Ok(())
}

#[test]
fn additive_existence_errors_do_not_take_the_older_plugin_route() {
    for failed in [false, true] {
        let args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: ".".into(),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        let changes = EditorChangeSet {
            instance_changes: vec![EditorInstanceChange {
                service: "Workspace".into(),
                mode: "upsertInstances".into(),
                allow_deletes: false,
                instances: vec![crate::editor::types::EditorInstanceDescriptor {
                    settings_id: "new".into(),
                    class_name: "Folder".into(),
                    path_segments: vec!["Workspace".into(), "New".into()],
                    path_ordinals: vec![1, 1],
                    ..Default::default()
                }],
                preserve_instances: vec![],
            }],
            ..Default::default()
        };
        let bridge = fixture();
        let stream = connect(&bridge, 0, edit_info("additive-error", 21));
        let peer = std::thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            let Message::Text(text) = socket.read().unwrap() else {
                panic!("missing request")
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(request["method"], "getEditorServiceChangeGenerations");
            socket.send(Message::Text(json!({"id":request["id"],"ok":!failed,"error":"existence timed out","result":{"generations":{"Workspace":7}}}).to_string().into())).unwrap();
        });
        let result = select_editor_binary_import(&args, &changes, &bridge);
        if failed {
            assert!(result.err().unwrap().to_string().contains("retry the sync"));
        } else {
            assert!(result.unwrap().is_none(), "old plugin cannot prove absence");
        }
        drop(bridge);
        peer.join().unwrap();
    }
}

/// The captured service has one root with a PackageLink and two identity slots.
/// Its package is absent from the desired project and must survive replacement.
fn package_capture() -> Result<Vec<u8>> {
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let root = dom.root_ref();
    dom.insert(root, RbxInstanceBuilder::new("Folder").with_name("Marker"));
    dom.insert(
        root,
        RbxInstanceBuilder::new("Folder")
            .with_name("Linked")
            .with_child(RbxInstanceBuilder::new("PackageLink")),
    );
    dom.insert(
        root,
        RbxInstanceBuilder::new("Folder").with_name("Identity1"),
    );
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, dom.root().children())?;
    Ok(bytes)
}

#[test]
fn import_builder_preserves_planned_and_failed_reader_routes_and_live_packages() -> Result<()> {
    let service = "ReplicatedStorage";
    let root = crate::tests::support::temp_dir("import-builder-route");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let mut args = PushEditorChangesArgs::new(
        ProjectSourceArgs {
            project_root: root.clone(),
            src_root: "src".into(),
        },
        BridgeConnectionArgs::local(0.1),
    );
    std::fs::create_dir_all(root.join("src").join(service))?;
    args.disable_service_reader = true;
    let changes = EditorChangeSet {
        instance_changes: vec![EditorInstanceChange {
            service: service.into(),
            mode: "reconcileService".into(),
            allow_deletes: true,
            instances: vec![],
            preserve_instances: vec![],
        }],
        native_property_documents: HashMap::from([(
            service.into(),
            std::sync::Arc::new(SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    SettingsBytecodeInstance::new(
                        "root".into(),
                        service.into(),
                        service.into(),
                        None,
                    ),
                    SettingsBytecodeInstance::new(
                        "part".into(),
                        "New".into(),
                        "Folder".into(),
                        Some(0),
                    ),
                ],
            }),
        )]),
        ..Default::default()
    };
    for (package, failed) in [(false, false), (false, true), (true, false), (true, true)] {
        let bridge = fixture();
        let stream = connect(&bridge, 0, edit_info("builder-fixture", 21));
        let bytes = package_capture()?;
        let peer = std::thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            let mut generation_requests = 0;
            while let Ok(Message::Text(text)) = socket.read() {
                let req: Value = serde_json::from_str(&text).unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "getEditorServiceChangeGenerations" => {
                        generation_requests += 1;
                        json!({"generations":{(service):0},"hasPackageLinks":{(service):package}})
                    }
                    "beginEditorBinaryExport" => {
                        json!({"groups":[{"service":service,"targetPath":[service],"count":1,"instanceCount":3,"classNames":["Folder","PackageLink"],"identityCarrierClass":"Folder","identityCarrierPrefix":"Identity","identityCarrierSlots":["A","B"],"identityCarrierCount":1}],"propertySchemaByClass":{"Folder":[["Archivable",1]]}})
                    }
                    "readEditorBinaryExport" => {
                        json!({"start":1,"nextStart":bytes.len()+1,"total":bytes.len(),"chunk":base64::encode(&bytes)})
                    }
                    "finishEditorBinaryExport" => json!({"found":true}),
                    other => panic!("unexpected preflight request {other}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":req["id"], "ok":true, "result":result})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
            }
            if !package {
                assert_eq!(generation_requests, 1);
            }
        });
        let mut selected = select_editor_binary_import(&args, &changes, &bridge)?.unwrap();
        if failed {
            selected.fallback_reason = Some(ServiceReaderFallback::DiscoveryFailed(
                "missing reader".into(),
            ));
        }
        let import = build_editor_binary_import(&args, &changes, &bridge, Some(selected))?
            .context("import missing")?;
        let reason = import.fallback_reason.unwrap();
        if package {
            assert!(matches!(reason, ServiceReaderFallback::PackageRoots));
            assert_eq!(import.groups[0].package_roots.len(), 1);
            assert_eq!(
                import.groups[0].package_roots[0].path_segments,
                [service, "Linked"]
            );
        } else if failed {
            assert!(
                matches!(reason,ServiceReaderFallback::DiscoveryFailed(message) if message=="missing reader")
            );
        } else if cfg!(windows) {
            assert!(matches!(reason, ServiceReaderFallback::Disabled));
        } else {
            assert!(matches!(reason, ServiceReaderFallback::Platform));
        }
        assert!(import.native_replacement.is_none() && !import.bytes.is_empty());
        drop(bridge);
        peer.join().unwrap();
    }
    Ok(())
}

#[test]
fn import_builder_returns_exact_original_rows_for_the_supplemental_pass() -> Result<()> {
    use crate::editor::diff::{
        EditorPropertyScope, EditorTargetChangeOptions, append_editor_target_changes,
    };
    use crate::editor::types::EditorPropertyFilter;
    let service = "ReplicatedStorage";
    let root = crate::tests::support::temp_dir("import-kept-rows");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    let args = PushEditorChangesArgs::new(
        ProjectSourceArgs {
            project_root: root.clone(),
            src_root: "src".into(),
        },
        BridgeConnectionArgs::local(0.1),
    );
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    for (name, class, parent, properties) in [
        (service, service, None, json!({})),
        (
            "Actor",
            "Humanoid",
            Some(0),
            json!({"Health":77.0, "WalkSpeed":13.0}),
        ),
        (
            "Twin",
            "ObjectValue",
            Some(0),
            json!({"Value":{"_type":"Ref", "instanceIndex":2}}),
        ),
        (
            "Twin",
            "ObjectValue",
            Some(0),
            json!({"Value":{"_type":"Ref", "pathSegments":["Lighting","Outside"], "pathOrdinals":[1,1]}}),
        ),
        (
            "Pivot",
            "Model",
            Some(0),
            json!({"WorldPivot":{"_type":"CFrame", "components":[1,2,3,1,0,0,0,1,0,0,0,1]}}),
        ),
        (
            "Carried",
            "Part",
            Some(0),
            json!({"Transparency":0.25, "Material":{"_type":"EnumItem", "enumType":"Material", "name":"Wood", "value":512}}),
        ),
        (
            "FutureEnum",
            "Part",
            Some(0),
            json!({"Material":{"_type":"EnumItem", "enumType":"Material", "name":"ReniumFixtureFutureMaterial"}}),
        ),
        (
            "Aliased",
            "SurfaceAppearance",
            Some(0),
            json!({"TexturePackContent":{"_type":"Content", "value":"rbxassetid://123"}}),
        ),
    ] {
        let mut row = SettingsBytecodeInstance::new(
            format!("row:{}", document.instances.len()),
            name.into(),
            class.into(),
            parent,
        );
        row.properties = properties.as_object().unwrap().clone();
        row.attributes.insert("WholeRow".into(), json!("unchanged"));
        document.instances.push(row);
    }
    for index in 0..256 {
        let mut row = SettingsBytecodeInstance::new(
            format!("unknown:{index}"),
            "Flat".into(),
            "Part".into(),
            Some(0),
        );
        row.properties.insert("UnknownNumber".into(), json!(0.25));
        document.instances.push(row);
    }
    let path = service_settings_path(&root.join("src").join(service));
    std::fs::create_dir_all(path.parent().unwrap())?;
    document.write_file(&path)?;
    let bytes = std::fs::read(&path)?;
    let document = crate::settings::bytecode::decode_settings_bytecode_holding(&bytes)?;
    for native in [false, true] {
        let mut expected_payload = None;
        for (files, retain) in [(false, false), (true, false), (false, true), (true, true)] {
            let bridge = fixture();
            let stream = connect(&bridge, 0, edit_info("kept-rows", 21));
            let peer = std::thread::spawn(move || {
                let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
                let mut generation_requests = 0;
                while let Ok(Message::Text(text)) = socket.read() {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(req["method"], "getEditorServiceChangeGenerations");
                    generation_requests += 1;
                    socket.send(Message::Text(json!({"id":req["id"], "ok":true, "result":{"generations":{(service):0},"hasPackageLinks":{(service):false}}}).to_string().into())).unwrap();
                }
                assert_eq!(generation_requests, 1);
            });
            let inputs = if files {
                HashMap::new()
            } else {
                HashMap::from([(service.into(), RbxPlaceDocument::Borrowed(&document))])
            };
            let mut requested = document.clone();
            if retain {
                // The caller already has the Humanoid row; only unread rows
                // need originals returned by the builder.
                for row in requested.instances.iter_mut().skip(2) {
                    row.properties.clear();
                    crate::settings::held::stand_unchanged(&mut row.properties);
                }
            }
            let changes = EditorChangeSet {
                native_property_documents: HashMap::from([(
                    service.into(),
                    std::sync::Arc::new(requested),
                )]),
                ..Default::default()
            };
            crate::rbx::model::PATH_SIBLING_VISITS.set(0);
            let mut import = build_editor_binary_import_with_documents(
                &args,
                &changes,
                &bridge,
                BinaryImportSelection {
                    services: HashSet::from([service.into()]),
                    additive_roots: HashMap::new(),
                    fallback_reason: (!native).then_some(ServiceReaderFallback::Disabled),
                    first_generation: None,
                },
                inputs,
            )?
            .context("import missing")?;
            assert!(
                crate::rbx::model::PATH_SIBLING_VISITS.get() < document.instances.len() * 20,
                "omission paths must index sibling ordinals once, not scan each prefix"
            );
            if let Some(expected) = &expected_payload {
                assert_eq!(
                    &import.bytes, expected,
                    "retaining rows changed the payload"
                );
            } else {
                expected_payload = Some(import.bytes.clone());
            }
            if !retain {
                assert!(
                    import.kept_rows.is_empty(),
                    "fully read documents have no supplemental reader"
                );
                drop(bridge);
                peer.join().unwrap();
                continue;
            }
            assert!(
                import
                    .post_apply_properties_by_class
                    .get("Humanoid")
                    .is_some_and(|names| names.contains("Health"))
            );
            assert!(
                !import
                    .post_apply_properties_by_path
                    .contains_key(&instance_path_parts_key(
                        &[service.into(), "Actor".into()],
                        &[1, 1]
                    )),
                "class omissions must not duplicate per-instance paths"
            );
            assert!(
                import
                    .post_apply_properties_by_path
                    .contains_key(&instance_path_parts_key(
                        &[service.into(), "Twin".into()],
                        &[1, 2]
                    ))
            );
            let mut canonical = document.clone();
            crate::settings::equivalence::canonicalize_settings_property_names(&mut canonical)?;
            let mut changes = EditorChangeSet::default();
            append_editor_target_changes(
                &mut changes,
                &canonical,
                service,
                &EditorPropertyFilter::default(),
                EditorTargetChangeOptions {
                    upsert_instances: false,
                    properties: true,
                    property_scope: EditorPropertyScope::Imported(Some(&import)),
                    property_schema_by_class: &PropertySchemaMap::new(),
                    database: rbx_reflection_database::get()?,
                    outside_references: None,
                },
            );
            let expected = changes
                .property_changes
                .iter()
                .filter_map(|row| row.settings_id.clone())
                .filter(|id| id != "row:1")
                .collect::<HashSet<_>>();
            let kept = import
                .kept_rows
                .remove(service)
                .context("builder omitted its host rows")?;
            let actual = kept
                .rows
                .iter()
                .map(|(_, row)| row.settings_id.clone())
                .collect::<HashSet<_>>();
            assert_eq!(actual, expected);
            assert!(
                !actual.contains("row:2"),
                "the same-named sibling carried by the payload needs no setter"
            );
            assert!(
                actual.contains("row:6"),
                "the value-specific omission needs its original row"
            );
            assert!(
                !actual.contains("row:5"),
                "one unrepresentable value must not retain every row of its class"
            );
            if !native {
                assert!(
                    actual.contains("row:7"),
                    "content aliases must be selected as the supplemental pass sees them"
                );
            }
            for (index, row) in &kept.rows {
                assert_eq!(row, &document.instances[*index]);
            }
            drop(bridge);
            peer.join().unwrap();
        }
    }
    Ok(())
}

fn check_additive_source_layout(
    root: &std::path::Path,
    service: &str,
    separated: bool,
) -> Result<()> {
    let project = root.join(if separated { "project" } else { "legacy" });
    let src = project.join("src");
    std::fs::create_dir_all(&src)?;
    if separated {
        std::fs::write(
            project.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
        )?;
        crate::project::config::try_load_project(None, Some(&project))?
            .context("project not loaded")?;
    }

    let dir = src.join(service);
    std::fs::create_dir_all(&dir)?;
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "service".into(),
            service.into(),
            service.into(),
            None,
        )],
    };
    let long = "a".repeat(100);
    let pairs = [
        ("map".into(), "Map".into()),
        ("A:B".into(), "A_B".into()),
        ("Map.".into(), "Map ".into()),
        (format!("{long}1"), format!("{long}2")),
    ];
    let mut roots = HashSet::new();
    for (pair, (old, new)) in pairs.into_iter().enumerate() {
        roots.insert(new.clone());
        for (side, name) in [("old", old), ("new", new)] {
            let parent = document.instances.len();
            let id = format!("{side}-{pair}");
            document.instances.push(SettingsBytecodeInstance::new(
                id.clone(),
                name,
                "Model".into(),
                Some(0),
            ));
            for (child, class) in [("Main", "Script"), ("Same", "Part"), ("Same", "Part")] {
                let mut row = SettingsBytecodeInstance::new(
                    format!("{id}-{}", document.instances.len()),
                    child.into(),
                    class.into(),
                    Some(parent),
                );
                if class == "Script" {
                    row.properties.insert(
                        "Source".into(),
                        json!(crate::settings::EXTERNAL_SOURCE_MARKER),
                    );
                    row.properties.insert("RunContext".into(), json!({"_type":"EnumItem", "name":"Client", "enumType":"RunContext", "value":2}));
                }
                document.instances.push(row);
            }
            document.instances[parent].properties.insert(
                "PrimaryPart".into(),
                json!({"_type":"Ref", "instanceIndex":parent+4}),
            );
        }
    }
    let paths = crate::editor::paths::build_editor_source_paths_by_index(&document, service, &dir);
    for (index, path) in paths.iter().enumerate() {
        if let Some(path) = path {
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, format!("print({index})\n"))?;
        }
    }
    assert!(
        paths
            .iter()
            .flatten()
            .any(|path| path.to_string_lossy().contains("Map_2"))
    );
    let path = service_settings_path(&dir);
    std::fs::create_dir_all(path.parent().unwrap())?;
    assert_eq!(
        path.parent() == Some(project.join("instances").as_path()),
        separated
    );
    document.write_file(&path)?;
    // Compare against the decoded store: the codec normalizes reference selectors.
    let document = SettingsBytecode::read_file(&path)?;
    let selected = additive_subtree(&document, service, &roots).unwrap();
    assert!(additive_references_stay_inside(
        &document, service, &selected
    )?);
    let mut expected = None;
    for input in [
        RbxPlaceDocument::Borrowed(&document),
        independent_additive_document(&document, service, &selected, &dir),
        additive_document_from_file(path.clone(), service, &roots, &dir)?,
    ] {
        let mut build = build_rbx_place_with_documents(
            &src,
            vec![service.into()],
            HashMap::from([(service.into(), input)]),
            true,
            true,
            false,
        )?;
        for (&referent, id) in &build.settings_ids_by_ref {
            let instance = build.dom.get_by_ref(referent).unwrap();
            if instance.class.as_str() == "Script" {
                let original = document
                    .instances
                    .iter()
                    .position(|row| row.settings_id == *id)
                    .unwrap();
                assert_eq!(
                    instance.properties[&"Source".into()],
                    RbxVariant::String(format!("print({original})\n"))
                );
            }
        }
        let mut groups = pending_editor_binary_groups(&build.dom, &build.service_roots)?;
        groups.retain(|group| group.target_path.len() == 1);
        for group in &mut groups {
            group.additive = true;
            group.viewport_camera = None;
            group
                .roots
                .retain(|root| build.settings_ids_by_ref[root].starts_with("new-"));
            group.root_settings_ids = group
                .roots
                .iter()
                .map(|root| build.settings_ids_by_ref[root].clone())
                .collect();
        }
        let plans = groups
            .iter()
            .map(|_| EditorPackageGroupPlan {
                retained_roots: vec![],
                package_roots: vec![],
                mutation_package_roots: vec![],
                change_generation: Some(7),
            })
            .collect();
        clear_import_engine_identities(&mut build.dom);
        let (bytes, groups, _) = payload::encode_services(
            &mut build.dom,
            &build.service_roots,
            &build.settings_ids_by_ref,
            groups,
            plans,
            &mut HashMap::new(),
        )?;
        let actual = (bytes, serde_json::to_value(groups)?);
        assert_eq!(&actual, expected.get_or_insert_with(|| actual.clone()));
    }
    assert_eq!(
        ADDITIVE_STORE_READS.lock().unwrap().remove(&path).unwrap(),
        (1, selected.len() - 1, false)
    );
    check_additive_legacy_fallbacks(&document, service, &roots, &path, &dir)?;
    Ok(())
}

fn check_additive_legacy_fallbacks(
    document: &SettingsBytecode,
    service: &str,
    roots: &HashSet<String>,
    path: &std::path::Path,
    dir: &std::path::Path,
) -> Result<()> {
    // Ambiguous legacy selectors must use the complete decoded store once.
    for duplicate in [false, true] {
        let mut ambiguous = (*document).clone();
        if duplicate {
            ambiguous.instances[1].settings_id = ambiguous.instances[5].settings_id.clone();
        } else {
            ambiguous.instances[1].name = "old\0separator".into();
        }
        // Structural ambiguity remains even when the reference uses an index.
        let reference = json!({"_type":"Ref", "instanceIndex":8});
        ambiguous.instances[5]
            .properties
            .insert("PrimaryPart".into(), reference);
        let selected = additive_subtree(&ambiguous, service, roots).unwrap();
        assert!(!additive_references_stay_inside(
            &ambiguous, service, &selected
        )?);
        if duplicate {
            // Duplicate IDs can reach a prepared document, but the store codec refuses them.
            assert!(
                ambiguous
                    .write_file(path)
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate settings id")
            );
        } else {
            ambiguous.write_file(path)?;
            assert!(matches!(
                additive_document_from_file(path.to_path_buf(), service, roots, dir)?,
                RbxPlaceDocument::Owned(_)
            ));
            assert_eq!(
                ADDITIVE_STORE_READS.lock().unwrap().remove(path).unwrap(),
                (1, ambiguous.instances.len(), true)
            );
        }
    }

    Ok(())
}

#[test]
fn additive_subtree_payload_matches_the_whole_store_with_colliding_script_folders() -> Result<()> {
    let root = crate::tests::support::temp_dir("additive-source-oracle");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = std::fs::remove_dir_all(&root);
    });
    for separated in [false, true] {
        for service in ["Workspace", "ServerStorage"] {
            check_additive_source_layout(&root, service, separated)?;
        }
    }
    Ok(())
}
