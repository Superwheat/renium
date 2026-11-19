use super::*;
use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};
use crate::editor::types::EditorInstanceChange;
use crate::studio::bridge::routing_tests::{connect, edit_info, fixture};
use tungstenite::{Message, WebSocket, protocol::Role};

fn check_scoped_additive_route(absent: bool, external: bool) -> Result<()> {
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
    let changes = EditorChangeSet {
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
    assert_eq!(READER_PREFLIGHTS.get(), 0);
    let import = build_editor_binary_import(&args, &changes, &bridge, selection)?;
    let reads = ADDITIVE_STORE_READS
        .lock()
        .unwrap()
        .remove(&path)
        .unwrap_or_default();
    if absent {
        assert_eq!(
            reads,
            (1, 3, external),
            "only the three subtree rows should be materialized"
        );
        let import = import.context("absent root was not imported")?;
        assert_eq!(import.groups.len(), 1);
        assert_eq!(import.groups[0].change_generation, Some(7));
        let dom = rbx_binary::from_reader(import.bytes.as_slice())?;
        assert!(
            dom.descendants()
                .all(|row| row.name != "Unrelated" && row.name != "ExistingCamera")
        );
        let part = dom.descendants().find(|row| row.name == "Part").unwrap();
        assert_eq!(
            part.properties[&"Transparency".into()],
            RbxVariant::Float32(0.375)
        );
        if external {
            assert!(
                import
                    .post_apply_properties_by_path
                    .values()
                    .any(|names| names.contains("Value"))
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
        assert_eq!(reads, (0, 0, false), "existing root read its store");
    }
    drop(bridge);
    peer.join().unwrap();
    Ok(())
}

#[test]
fn existing_additive_root_reads_no_store_and_builds_nothing() -> Result<()> {
    check_scoped_additive_route(false, false)
}

#[test]
fn absent_additive_root_materializes_its_subtree_with_internal_references() -> Result<()> {
    check_scoped_additive_route(true, false)
}

#[test]
fn additive_external_reference_keeps_the_complete_import_index() -> Result<()> {
    check_scoped_additive_route(true, true)
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
