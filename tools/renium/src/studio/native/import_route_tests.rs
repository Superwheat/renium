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
