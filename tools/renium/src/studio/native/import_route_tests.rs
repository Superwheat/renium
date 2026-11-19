use super::*;
use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};
use crate::editor::types::EditorInstanceChange;
use crate::studio::bridge::routing_tests::{connect, edit_info, fixture};
use tungstenite::{Message, WebSocket, protocol::Role};

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
            while let Ok(Message::Text(text)) = socket.read() {
                let req: Value = serde_json::from_str(&text).unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "getEditorServiceChangeGenerations" => {
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
                while let Ok(Message::Text(text)) = socket.read() {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(req["method"], "getEditorServiceChangeGenerations");
                    socket.send(Message::Text(json!({"id":req["id"], "ok":true, "result":{"generations":{(service):0},"hasPackageLinks":{(service):false}}}).to_string().into())).unwrap();
                }
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
