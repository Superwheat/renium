use super::*;

#[test]
fn relocated_supporting_stores_use_relative_snapshot_keys() -> Result<()> {
    let root = crate::tests::support::temp_dir("supporting-store-scope");
    let _cleanup = crate::system::files::OnDrop::new(|| {
        crate::project::storage::forget(&root);
        let _ = fs::remove_dir_all(&root);
    });
    let project = root.join("renium.project.jsonc");
    fs::write(
        &project,
        br#"{"schemaVersion":1,"sourceRoot":"code/scripts"}"#,
    )?;
    config::load_project(Some(&project), None)?;
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: project.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        experience: String::new(),
        source: root.join("code/scripts").to_string_lossy().into_owned(),
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: None,
        plugin_build: None,
        fingerprint: String::new(),
    };
    let store = PathBuf::from("instances/ServerStorage.renium");
    fs::create_dir_all(root.join("instances"))?;
    fs::write(root.join(&store), b"unchanged store")?;
    for path in [
        PathBuf::from("code/scripts/ServerStorage/Module.luau"),
        store.clone(),
    ] {
        let changed = HashSet::from([path]);
        assert_eq!(
            services_for_snapshot_paths(&context, &changed),
            ["ServerStorage"]
        );
        let scopes = supporting_settings_scopes(&context, &changed)?;
        assert_eq!(scopes, std::slice::from_ref(&store));
        let expected = capture_snapshot(&root, std::slice::from_ref(&store))?;
        let current = capture_snapshot(&root, &scopes)?;
        assert_eq!(
            expected.entries.keys().collect::<Vec<_>>(),
            current.entries.keys().collect::<Vec<_>>()
        );
        assert!(
            snapshot_path_differences(&expected, &current, &scopes.into_iter().collect())?
                .is_empty()
        );
    }
    fs::write(
        &project,
        br#"{"schemaVersion":1,"sourceRoot":"code/scripts","tree":{"StarterPlayer":{"StarterPlayerScripts":{"$className":"StarterPlayerScripts","$path":"code/scripts/client"}}}}"#,
    )?;
    fs::create_dir_all(root.join("code/scripts/client"))?;
    fs::write(
        root.join("code/scripts/client/Main.client.luau"),
        b"-- client\n",
    )?;
    let changed = HashSet::from([PathBuf::from("code/scripts/client/Main.client.luau")]);
    let services = services_for_snapshot_paths(&context, &changed);
    assert_eq!(services, ["StarterPlayer"]);
    let stage = ExportProjectStage::create(&root, Path::new("code/scripts"), &services)?;
    assert!(
        stage
            .loaded
            .as_ref()
            .unwrap()
            .project
            .tree
            .contains_key("StarterPlayer")
    );
    assert!(
        stage
            .import_project_root
            .join("StarterPlayer/StarterPlayerScripts/Main.client.luau")
            .is_file()
    );
    Ok(())
}

#[test]
fn native_bootstrap_does_not_replace_nonempty_service_deltas() {
    let document = Arc::new(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance::new(
                "root".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            ),
            SettingsBytecodeInstance::new("mesh".into(), "Mesh".into(), "MeshPart".into(), Some(0)),
        ],
    });
    let mut changes = EditorChangeSet::default();
    for (service, mode) in [
        ("TestService", "deleteInstances"),
        ("Workspace", "upsertInstances"),
        ("TestService", "upsertInstances"),
        ("Workspace", "deleteInstances"),
    ] {
        changes.instance_changes.push(EditorInstanceChange {
            service: service.into(),
            mode: mode.into(),
            allow_deletes: false,
            instances: Vec::new(),
            preserve_instances: Vec::new(),
        });
    }
    let empty = HashMap::new();
    let before = serde_json::to_value(&changes.instance_changes).unwrap();
    stage_native_bootstrap_services(&mut changes, &empty);
    assert_eq!(
        serde_json::to_value(&changes.instance_changes).unwrap(),
        before
    );
    stage_native_bootstrap_services(
        &mut changes,
        &HashMap::from([("Workspace".into(), Arc::clone(&document))]),
    );
    assert_eq!(changes.instance_changes.len(), 3);
    assert_eq!(changes.instance_changes[0].service, "TestService");
    assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
    assert_eq!(changes.instance_changes[1].service, "TestService");
    assert_eq!(changes.instance_changes[1].mode, "upsertInstances");
    let native = &changes.instance_changes[2];
    assert_eq!(native.service, "Workspace");
    assert_eq!(native.mode, "reconcileService");
    assert!(native.allow_deletes);
    assert_eq!(native.instances.len(), 1);
    assert_eq!(native.instances[0].settings_id, "mesh");
}

#[test]
fn native_bootstrap_preserves_retained_root_edits_and_source_paths() {
    let document = Arc::new(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance::new(
                "root".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            ),
            SettingsBytecodeInstance::new("mesh".into(), "Mesh".into(), "MeshPart".into(), Some(0)),
        ],
    });
    let mut desired = document.as_ref().clone();
    desired.instances[0]
        .properties
        .insert("Gravity".into(), json!(150));
    for index in 0..4096 {
        desired.instances.push(SettingsBytecodeInstance::new(
            format!("new-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        ));
    }
    let mut previous = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![document.instances[0].clone()],
    };
    previous.instances[0]
        .attributes
        .insert("Removed".into(), json!(true));
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let prepared = HashMap::from([(
        path.clone(),
        PreparedEditorSettingsChange {
            previous,
            current: desired,
        },
    )]);
    let paths = HashSet::from([path.clone()]);
    let snapshot = ProjectSnapshot::default();
    let full = reconciliation_push_plan_for_paths_with_prepared_settings(
        &snapshot, &snapshot, &paths, &prepared, true, false,
    )
    .unwrap();
    assert_eq!(full.changed_paths, vec![path.clone()]);
    assert_eq!(full.target_settings_ids, vec!["root"]);
    assert!(full.recreated_settings_ids.is_empty());
    assert_eq!(
        full.property_removals[0].deleted_attributes,
        vec!["Removed"]
    );
    assert!(full.initial_native_services.contains("Workspace"));
    let incremental = reconciliation_push_plan_for_paths_with_prepared_settings(
        &snapshot, &snapshot, &paths, &prepared, false, false,
    )
    .unwrap();
    assert_eq!(incremental.recreated_settings_ids.len(), 4097);
    assert_eq!(incremental.target_settings_ids.len(), 4098);
    assert_eq!(
        incremental.property_removals[0].deleted_attributes,
        vec!["Removed"]
    );

    let mut outgoing = prepared[&path].previous.clone();
    outgoing.instances.push(SettingsBytecodeInstance::new(
        "outgoing".into(),
        "OnlyInStudio".into(),
        "Folder".into(),
        Some(0),
    ));
    let mut before = ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&outgoing).unwrap()),
        )]),
    };
    let mut requested = ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&prepared[&path].current).unwrap()),
        )]),
    };
    let source_path = PathBuf::from("src/Workspace/Unchanged.server.luau");
    let source = SnapshotEntry::File(b"return 1".to_vec());
    before.entries.insert(source_path.clone(), source.clone());
    requested.entries.insert(source_path.clone(), source);
    let mut replacement = HashMap::new();
    let paths = project_replacement_paths(&requested, &before);
    prepare_project_replacement(&requested, &before, &paths, &mut replacement).unwrap();
    assert_eq!(replacement[&path].previous.instances.len(), 1);
    assert_eq!(replacement[&path].current.instances.len(), 4098);
    let replaced = reconciliation_push_plan_for_paths_with_prepared_settings(
        &before,
        &requested,
        &paths,
        &replacement,
        true,
        true,
    )
    .unwrap();
    assert_eq!(replaced.target_settings_ids, vec!["root"]);
    assert!(replaced.initial_native_services.contains("Workspace"));
    assert!(replaced.recreated_settings_ids.is_empty());
    assert!(replaced.instance_deletes.is_empty());
    assert!(replaced.changed_paths.contains(&source_path));
    assert_eq!(
        replaced.property_removals[0].deleted_attributes,
        vec!["Removed"]
    );
}

#[test]
fn full_push_clears_authored_contents_when_only_engine_containers_remain() {
    for (service, container) in [
        ("ReplicatedStorage", None),
        ("StarterPlayer", Some("StarterPlayerScripts")),
    ] {
        let mut current = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "root".into(),
                service.into(),
                service.into(),
                None,
            )],
        };
        if let Some(container) = container {
            current.instances.push(SettingsBytecodeInstance::new(
                "container".into(),
                container.into(),
                container.into(),
                Some(0),
            ));
        }
        let mut previous = current.clone();
        previous.instances.push(SettingsBytecodeInstance::new(
            "removed".into(),
            "OnlyInStudio".into(),
            "ModuleScript".into(),
            Some(current.instances.len() - 1),
        ));
        let path = PathBuf::from(format!("instances/{service}.renium"));
        let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
            entries: BTreeMap::from([(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
            )]),
        };
        let desired = snapshot(&current);
        let observed = snapshot(&previous);
        let paths = project_replacement_paths(&desired, &observed);
        let mut prepared = HashMap::new();
        let unchanged =
            prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
        assert!(!unchanged.contains(&path));
        assert!(!settings_documents_equivalent(
            &prepared[&path].current,
            &prepared[&path].previous,
        ));
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &observed, &desired, &paths, &prepared, true, false,
        )
        .unwrap();
        assert!(plan.initial_native_services.is_empty());
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "removed");
    }
}

#[test]
fn default_collision_fidelity_and_unchanged_native_values_are_not_pushed() {
    let root = SettingsBytecodeInstance::new(
        "root".into(),
        "ServerStorage".into(),
        "ServerStorage".into(),
        None,
    );
    let mut current = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    for index in 0..3 {
        let mut part = SettingsBytecodeInstance::new(
            format!("mesh-{index}"),
            format!("Mesh{index}"),
            "MeshPart".into(),
            Some(0),
        );
        part.properties
            .insert("MeshContent".into(), json!("rbxassetid://123"));
        part.properties.insert("SourceAssetId".into(), json!(456));
        part.properties.insert("Transparency".into(), json!(0.25));
        current.instances.push(part);
    }
    let mut previous = current.clone();
    for (index, instance) in previous.instances.iter_mut().enumerate().skip(1) {
        let fidelity = if index == 1 { "Hull" } else { "Default" };
        instance.properties.insert(
            "CollisionFidelity".into(),
            json!({"_type": "EnumItem", "enumType": "CollisionFidelity", "name": fidelity}),
        );
    }
    current.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.5));
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let desired = snapshot(&current);
    let observed = snapshot(&previous);
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &observed, &desired, &paths, &prepared, true, false,
    )
    .unwrap();
    assert_eq!(plan.target_settings_ids, vec!["mesh-1"]);
    let mut unchanged = plan
        .unchanged_native_root_properties
        .get(&("ServerStorage".into(), "mesh-1".into()))
        .cloned()
        .unwrap();
    unchanged.sort();
    assert_eq!(unchanged, vec!["MeshContent", "SourceAssetId"]);
}

#[test]
fn full_push_reuses_populated_instances_and_plans_only_the_delta() {
    let root = SettingsBytecodeInstance::new(
        "root".into(),
        "ServerStorage".into(),
        "ServerStorage".into(),
        None,
    );
    let mut current = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    for index in 0..4096 {
        let mut part = SettingsBytecodeInstance::new(
            format!("part-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        );
        part.attributes.insert("Keep".into(), json!(true));
        current.instances.push(part);
    }
    let mut previous = current.clone();
    current.instances[43]
        .attributes
        .insert("Keep".into(), json!(false));
    previous.instances.push(SettingsBytecodeInstance::new(
        "outgoing".into(),
        "OnlyInStudio".into(),
        "Folder".into(),
        Some(0),
    ));
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]),
    };
    let desired = snapshot(&current);
    let observed = snapshot(&previous);
    let paths = project_replacement_paths(&desired, &observed);
    let mut prepared = HashMap::new();
    prepare_project_replacement(&desired, &observed, &paths, &mut prepared).unwrap();
    assert_eq!(prepared[&path].previous.instances.len(), 4098);
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &observed, &desired, &paths, &prepared, true, false,
    )
    .unwrap();
    assert!(plan.initial_native_services.is_empty());
    assert!(plan.recreated_settings_ids.is_empty());
    assert_eq!(plan.target_settings_ids, vec!["part-42"]);
    assert!(
        plan.attribute_only_instances
            .contains(&("ServerStorage".into(), "part-42".into()))
    );
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert_eq!(
        plan.instance_deletes[0].instances[0]
            .path_segments
            .last()
            .unwrap(),
        "OnlyInStudio"
    );
    let mut changes = EditorChangeSet::default();
    changes.instance_changes.push(EditorInstanceChange {
        mode: "upsertInstances".into(),
        service: "ServerStorage".into(),
        allow_deletes: false,
        instances: vec![crate::editor::types::EditorInstanceDescriptor {
            settings_id: "part-42".into(),
            class_name: "Part".into(),
            ..Default::default()
        }],
        preserve_instances: Vec::new(),
    });
    changes.property_changes.push(EditorPropertyChange {
        service: "ServerStorage".into(),
        settings_id: Some("part-42".into()),
        class_name: "Part".into(),
        path_segments: Vec::new(),
        path_ordinals: Vec::new(),
        properties: Map::from_iter([("Anchored".into(), json!(true))]),
        reset_properties: Vec::new(),
        attributes: Map::from_iter([("Keep".into(), json!(false))]),
        deleted_attributes: vec!["Removed".into()],
        attributes_complete: false,
    });
    amend_reconciled_changes(&mut changes, plan).unwrap();
    assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
    assert!(!changes.instance_changes[0].instances[0].anchor_only);
    assert!(changes.instance_changes[1].instances[0].anchor_only);
    assert!(changes.property_changes[0].properties.is_empty());
    assert_eq!(changes.property_changes[0].attributes["Keep"], false);
    assert_eq!(changes.property_changes[0].deleted_attributes, ["Removed"]);
}

#[test]
fn in_place_edits_preserve_observed_paths_through_reordered_duplicate_ancestors() {
    let node = |id: &str, name: &str, class: &str, parent| {
        SettingsBytecodeInstance::new(id.into(), name.into(), class.into(), parent)
    };
    let mut desired = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            node("root", "Workspace", "Workspace", None),
            node("a", "Lampost", "Model", Some(0)),
            node("b", "Lampost", "Model", Some(0)),
            node("light-a", "Light", "Model", Some(1)),
            node("light-b", "Light", "Model", Some(2)),
            node("we", "we", "Part", Some(3)),
        ],
    };
    let mut observed = desired.clone();
    observed.instances.swap(1, 2);
    observed.instances[3].parent_index = Some(2);
    observed.instances[4].parent_index = Some(1);
    desired.instances[5]
        .attributes
        .insert("Changed".into(), json!(true));
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(
        Path::new("instances/Workspace.renium"),
        &desired,
        &observed,
        &mut plan,
    )
    .unwrap();
    assert_eq!(plan.target_settings_ids, ["we"]);
    let key = |id: &str| ("Workspace".to_string(), id.to_string());
    assert!(plan.in_place_instances.contains(&key("we")));
    assert_eq!(plan.previous_paths[&key("we")].path_ordinals, [1, 2, 1, 1]);
    assert_eq!(plan.previous_paths[&key("a")].path_ordinals, [1, 2]);
    assert_eq!(
        plan.previous_paths[&key("light-a")].path_ordinals,
        [1, 2, 1]
    );
    assert!(
        !plan.previous_paths.contains_key(&key("light-b")),
        "Do not traverse unrelated subtrees for a small edit"
    );
}

#[test]
fn folder_icon_tint_default_does_not_invent_full_push_changes() {
    let black = json!({"_type": "Color3", "r": 0.0, "g": 0.0, "b": 0.0});
    let red = json!({"_type": "Color3", "r": 1.0, "g": 0.0, "b": 0.0});
    assert!(reconciliation_property_values_equal(
        "Folder",
        "IconTint",
        None,
        Some(&black)
    ));
    assert!(!reconciliation_property_values_equal(
        "Folder",
        "IconTint",
        None,
        Some(&red)
    ));
    assert!(!reconciliation_values_map_equal(
        &Map::new(),
        &Map::from_iter([("IconTint".into(), black)])
    ));
}

#[test]
fn initial_native_import_requires_no_ordinary_instances() {
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type":"Ref", "settingsId":"viewport"}),
    );
    let mut document = SettingsBytecode {
        version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
        instances: vec![
            root,
            SettingsBytecodeInstance::new(
                "viewport".into(),
                "Renamed".into(),
                "Camera".into(),
                Some(0),
            ),
            SettingsBytecodeInstance::new(
                "terrain".into(),
                "Terrain".into(),
                "Terrain".into(),
                Some(0),
            ),
        ],
    };
    assert!(service_has_only_native_containers(&document));
    for (class, parent) in [
        ("Camera", 0),
        ("Part", 0),
        ("Folder", 2),
        ("StringValue", 1),
    ] {
        document.instances.push(SettingsBytecodeInstance::new(
            "ordinary".into(),
            "Camera".into(),
            class.into(),
            Some(parent),
        ));
        assert!(!service_has_only_native_containers(&document));
        document.instances.pop();
    }
    document.instances[0].properties.clear();
    assert!(!service_has_only_native_containers(&document));

    let mut chat = SettingsBytecode {
        version: document.version,
        instances: vec![SettingsBytecodeInstance::new(
            "chat".into(),
            "TextChatService".into(),
            "TextChatService".into(),
            None,
        )],
    };
    for class in [
        "ChatWindowConfiguration",
        "ChatInputBarConfiguration",
        "BubbleChatConfiguration",
        "ChannelTabsConfiguration",
    ] {
        chat.instances.push(SettingsBytecodeInstance::new(
            class.into(),
            class.into(),
            class.into(),
            Some(0),
        ));
    }
    assert!(service_has_only_native_containers(&chat));
    chat.instances.push(SettingsBytecodeInstance::new(
        "content".into(),
        "Content".into(),
        "Folder".into(),
        Some(3),
    ));
    assert!(
        !service_has_only_native_containers(&chat),
        "User contents must not qualify as an empty destination"
    );
}
use crate::editor::types::EditorSettingsWrite;
use crate::settings::bytecode::SettingsBytecodeInstance;

#[test]
fn staged_settings_redirect_uses_original_project_not_merged_stage() {
    let root = crate::tests::support::temp_dir("staged-settings-hash");
    let stage = root.join("stage");
    let relative = PathBuf::from("src/ServerScriptService/__roblox_sync_settings.renium");
    let destination = root.join(&relative);
    let staged_path = stage.join(&relative);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::create_dir_all(staged_path.parent().unwrap()).unwrap();
    let document = |id: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance {
            settings_id: id.into(),
            name: "ServerScriptService".into(),
            class_name: "ServerScriptService".into(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        }],
    };
    let original = encode_settings_bytecode(&document("editor-id")).unwrap();
    let merged = encode_settings_bytecode(&document("aligned-id")).unwrap();
    fs::write(&destination, &original).unwrap();
    fs::write(&staged_path, &merged).unwrap();
    let mut changes = EditorChangeSet {
        settings_writes: vec![EditorSettingsWrite {
            path: staged_path.clone(),
            expected_hash: settings_file_hash(&staged_path).unwrap(),
            document: document("aligned-id"),
        }],
        ..Default::default()
    };
    let project = file_snapshot(&[(relative.to_str().unwrap(), &original)]);
    let generated =
        redirect_staged_settings_writes(&mut changes, &stage, &root, Some(&project)).unwrap();
    let write = &changes.settings_writes[0];
    assert_eq!(write.path, destination);
    assert_eq!(
        write.expected_hash,
        settings_file_hash(&destination).unwrap()
    );
    assert!(generated.entries.get(&relative) == Some(&SnapshotEntry::File(merged)));
    assert_eq!(fs::read(&destination).unwrap(), original);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn staged_settings_redirect_keeps_concurrent_write_protection() {
    let root = crate::tests::support::temp_dir("staged-settings-concurrency");
    let stage = root.join("stage");
    let relative = Path::new("settings.renium");
    let destination = root.join(relative);
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let bytes = encode_settings_bytecode(&document).unwrap();
    for existed in [false, true] {
        for with_snapshot in [false, true] {
            let project = if existed {
                file_snapshot(&[("settings.renium", &bytes)])
            } else {
                ProjectSnapshot::default()
            };
            let expected = with_snapshot.then_some(&project);
            for concurrent_edit in [false, true] {
                if existed || concurrent_edit {
                    fs::write(
                        &destination,
                        if concurrent_edit {
                            b"newer editor data".as_slice()
                        } else {
                            bytes.as_slice()
                        },
                    )
                    .unwrap();
                } else if destination.exists() {
                    fs::remove_file(&destination).unwrap();
                }
                let hash = existed.then(|| Sha256::digest(&bytes).into());
                let mut changes = EditorChangeSet {
                    settings_writes: vec![EditorSettingsWrite {
                        path: stage.join(relative),
                        expected_hash: hash,
                        document: document.clone(),
                    }],
                    ..Default::default()
                };
                let result = redirect_staged_settings_writes(&mut changes, &stage, &root, expected);
                if concurrent_edit {
                    let message = result.err().unwrap().to_string();
                    assert!(message.contains("changed while its Studio update was being prepared"));
                    assert_eq!(fs::read(&destination).unwrap(), b"newer editor data");
                } else {
                    assert!(result.is_ok());
                    assert_eq!(changes.settings_writes[0].expected_hash, hash);
                    assert_eq!(changes.settings_writes[0].path, destination);
                }
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_binding_uses_saved_pairing_without_enabling_live_sync() {
    let root = crate::tests::support::temp_dir("saved-studio-pairing");
    let records = root.join(".renium").join(RECORD_DIR);
    fs::create_dir_all(&records).unwrap();
    let mut record = PairRecord {
        version: RECORD_VERSION,
        identity: PairIdentity {
            experience: canonical_string(&root).unwrap(),
            project: canonical_string(&root).unwrap(),
            fingerprint: "prior-configuration".into(),
            game_id: Some(10),
            place_id: Some(20),
            local_file: None,
        },
        mode: PairMode::Reconcile,
        conflict_preference: ConflictPreference::None,
        runtime_settings: Map::new(),
        baseline: None,
        head: None,
        conflicts: Vec::new(),
        resolution_required: false,
        last_runtime_id: None,
        local_file_stamp: None,
        local_file_digest: None,
        studio_checkpoint: None,
    };
    let write = |name: &str, record: &PairRecord| {
        fs::write(records.join(name), rmp_serde::to_vec(record).unwrap()).unwrap();
    };
    assert_eq!(saved_studio_target_for_root(&root, &root).unwrap(), None);
    write("first.rmp", &record);
    write("duplicate.rmp", &record);
    assert_eq!(
        saved_studio_target_for_root(&root, &root).unwrap(),
        Some(crate::automation::StudioReopenTarget {
            file: None,
            game_id: Some(10),
            place_id: Some(20),
        })
    );
    assert!(!root.join(".renium/live-watch-state.enabled").exists());
    record.identity.project = "a different project".into();
    write("foreign.rmp", &record);
    assert!(
        saved_studio_target_for_root(&root, &root)
            .unwrap()
            .is_some()
    );
    record.identity.project = canonical_string(&root).unwrap();
    record.identity.place_id = Some(30);
    write("different-place.rmp", &record);
    assert_eq!(saved_studio_target_for_root(&root, &root).unwrap(), None);
    fs::remove_dir_all(root).unwrap();
}

fn file_snapshot(entries: &[(&str, &[u8])]) -> ProjectSnapshot {
    ProjectSnapshot {
        entries: entries
            .iter()
            .map(|(path, bytes)| (PathBuf::from(path), SnapshotEntry::File(bytes.to_vec())))
            .collect(),
    }
}

#[test]
fn comparison_capture_retries_one_transient_studio_change() {
    let mut attempts = 0;
    let value = retry_transient_studio_capture(|| {
        attempts += 1;
        if attempts == 1 {
            anyhow::bail!(
                "Studio changed Workspace while native import was staged; retry the sync"
            );
        }
        Ok(42)
    })
    .unwrap();
    assert_eq!(value, 42);
    assert_eq!(attempts, 2);

    let mut persistent_attempts = 0;
    let error = retry_transient_studio_capture::<()>(|| {
        persistent_attempts += 1;
        anyhow::bail!("Studio changed Workspace while native import was staged; retry the sync")
    })
    .unwrap_err();
    assert!(error.to_string().contains("retry the sync"));
    assert_eq!(persistent_attempts, 2);

    let mut ordinary_attempts = 0;
    let error = retry_transient_studio_capture::<()>(|| {
        ordinary_attempts += 1;
        anyhow::bail!("invalid snapshot")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "invalid snapshot");
    assert_eq!(ordinary_attempts, 1);
}

#[test]
fn protected_snapshot_handoff_replaces_only_its_originating_runtime() {
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: "project".into(),
        root: "root".into(),
        experience: String::new(),
        source: "src".into(),
        resource_lease: None,
        place_id: Some(0),
        game_id: Some(0),
        selector: "Fixture".into(),
        runtime_id: Some("old".into()),
        plugin_build: None,
        fingerprint: "same".into(),
    };
    assert!(
        reopened_push_context(&context, &Map::new())
            .unwrap()
            .is_none()
    );
    let mut summary = Map::from_iter([(
        "protectedOfflineApply".into(),
        json!({
            "ok": true, "previousRuntimeId": "old", "reopenedRuntimeId": "new"
        }),
    )]);
    let replacement = reopened_push_context(&context, &summary).unwrap().unwrap();
    assert_eq!(replacement.runtime_id.as_deref(), Some("new"));
    assert_eq!(replacement.project, context.project);
    assert_eq!(replacement.fingerprint, context.fingerprint);
    for invalid in [
        json!({"ok":true,"previousRuntimeId":"foreign","reopenedRuntimeId":"new"}),
        json!({"ok":true,"previousRuntimeId":"old","reopenedRuntimeId":"old"}),
        json!({"ok":true,"previousRuntimeId":"old","reopenedRuntimeId":""}),
        json!({"ok":false,"previousRuntimeId":"old","reopenedRuntimeId":"new"}),
    ] {
        summary.insert("protectedOfflineApply".into(), invalid);
        assert!(reopened_push_context(&context, &summary).is_err());
    }
}

#[test]
fn studio_checkpoint_requires_uninterrupted_clean_tracking() {
    let context = BoundContext {
        id: 1,
        initialized: true,
        project: String::new(),
        root: String::new(),
        experience: String::new(),
        source: String::new(),
        resource_lease: None,
        place_id: None,
        game_id: None,
        selector: String::new(),
        runtime_id: Some("runtime".to_string()),
        plugin_build: None,
        fingerprint: String::new(),
    };
    let services = sync_services();
    let generations = services
        .iter()
        .map(|service| (service.clone(), Value::from(7)))
        .collect::<Map<_, _>>();
    let state = json!({
        "tracking": true,
        "trackedServices": services.len(),
        "dirtyServices": [],
        "fullSyncServices": [],
        "runtimeId": "runtime",
        "changeTrackerVersion": 4,
        "seq": 9,
        "serviceGenerations": generations.clone(),
        "checkpointGenerations": generations,
    });
    let checkpoint = StudioCheckpoint::from_state(&context, &state).unwrap();
    assert!(checkpoint.matches_state(&context, &state));
    assert_eq!(
        checkpoint.changed_services(&context, &state),
        Some(Vec::new())
    );

    let mut transaction_generation = state.clone();
    transaction_generation["serviceGenerations"][&services[0]] = Value::from(8);
    assert!(checkpoint.matches_state(&context, &transaction_generation));

    let mut interrupted = state.clone();
    interrupted["checkpointGenerations"][&services[0]] = Value::from(8);
    assert!(!checkpoint.matches_state(&context, &interrupted));
    assert_eq!(
        checkpoint.changed_services(&context, &interrupted),
        Some(vec![services[0].clone()])
    );

    let mut pending = state.clone();
    pending["dirtyServices"] = json!([&services[0]]);
    assert!(!checkpoint.matches_state(&context, &pending));
    assert_eq!(
        checkpoint.changed_services(&context, &pending),
        Some(vec![services[0].clone()])
    );

    let mut moved_references = state;
    moved_references["referencePathsMayChange"] = Value::Bool(true);
    assert_eq!(
        checkpoint.changed_services(&context, &moved_references),
        None
    );

    let mut unexpected_service = moved_references;
    unexpected_service["referencePathsMayChange"] = Value::Bool(false);
    unexpected_service["checkpointGenerations"]["UnexpectedService"] = Value::from(7);
    assert!(StudioCheckpoint::from_state(&context, &unexpected_service).is_none());
    assert_eq!(
        checkpoint.changed_services(&context, &unexpected_service),
        None
    );
}

#[test]
fn source_only_push_skips_service_readback_only_after_exact_verification() {
    let paths = HashSet::from([PathBuf::from(
        "src/ServerScriptService/Verified.server.luau",
    )]);
    let verified = Map::from_iter([
        ("sourceVerified".to_string(), Value::from(1)),
        ("sourceVerifyFailed".to_string(), Value::from(0)),
    ]);
    assert!(exact_source_push_verified(&verified, &paths));

    let unverified = Map::from_iter([
        ("sourceVerified".to_string(), Value::from(0)),
        ("sourceVerifyFailed".to_string(), Value::from(0)),
    ]);
    assert!(!exact_source_push_verified(&unverified, &paths));
}

#[test]
fn push_verification_distinguishes_engine_identity_from_authored_data() {
    let mut instance =
        SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), None);
    for name in ["UniqueId", "HistoryId"] {
        let identity = |value| {
            Map::from_iter([(
                name.to_string(),
                json!({"_type": "UniqueId", "value": value}),
            )])
        };
        let before = identity("00000001000000000000000000000001");
        let desired = identity("00000002000000000000000000000002");
        let observed = identity("00000003000000000000000000000003");
        assert_eq!(
            expected_map_mismatch(&desired, &observed, true, &instance),
            None
        );
        assert_eq!(
            changed_map_mismatch(&before, &desired, &observed, true, &instance),
            None
        );
        assert!(expected_map_mismatch(&desired, &observed, false, &instance).is_some());
        assert!(changed_map_mismatch(&before, &desired, &observed, false, &instance).is_some());
        instance.properties = desired.clone();
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![instance.clone()],
        };
        let decoded =
            decode_settings_bytecode(&encode_settings_bytecode(&document).unwrap()).unwrap();
        assert_eq!(decoded.instances[0].properties[name], desired[name]);
    }
    for name in ["MeshSize", "CollisionFidelity", "TexturePack", "Part0"] {
        let before = Map::from_iter([(name.to_string(), json!("before"))]);
        let desired = Map::from_iter([(name.to_string(), json!("source"))]);
        let observed = Map::from_iter([(name.to_string(), json!("destination"))]);
        assert!(expected_map_mismatch(&desired, &observed, true, &instance).is_some());
        assert!(changed_map_mismatch(&before, &desired, &observed, true, &instance).is_some());
    }
}

#[test]
fn push_verification_accepts_engine_migrations_and_forced_text_wrapping() {
    let part = SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), None);
    let before = Map::new();
    let desired = Map::from_iter([("InertiaMigrated".to_string(), json!(false))]);
    let observed = Map::from_iter([("InertiaMigrated".to_string(), json!(true))]);
    assert_eq!(
        expected_map_mismatch(&desired, &observed, true, &part),
        None
    );
    assert_eq!(
        changed_map_mismatch(&before, &desired, &observed, true, &part),
        None
    );

    let label =
        SettingsBytecodeInstance::new("label".into(), "Title".into(), "TextLabel".into(), None);
    let desired = Map::from_iter([
        ("TextScaled".to_string(), json!(true)),
        ("TextWrapped".to_string(), json!(false)),
    ]);
    let observed = Map::from_iter([
        ("TextScaled".to_string(), json!(true)),
        ("TextWrapped".to_string(), json!(true)),
    ]);
    assert_eq!(
        expected_map_mismatch(&desired, &observed, true, &label),
        None
    );
    assert_eq!(
        changed_map_mismatch(&before, &desired, &observed, true, &label),
        None
    );
    let unscaled = Map::from_iter([
        ("TextScaled".to_string(), json!(false)),
        ("TextWrapped".to_string(), json!(false)),
    ]);
    assert_eq!(
        expected_map_mismatch(&unscaled, &observed, true, &label).as_deref(),
        Some("TextScaled was not retained (expected false, Studio has true)")
    );
    let wrapped_only = Map::from_iter([("TextWrapped".to_string(), json!(false))]);
    assert_eq!(
        expected_map_mismatch(&wrapped_only, &observed, true, &label).as_deref(),
        Some("TextWrapped was not retained (expected false, Studio has true)")
    );
}

#[test]
fn push_verification_follows_game_settings_policy_without_hiding_saved_values() {
    let path = PathBuf::from("src/Players/__roblox_sync_settings.renium");
    let document = |name: &str, capacity: Option<i64>, auto_loads: bool| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance {
            settings_id: "players".to_string(),
            name: "Players".to_string(),
            class_name: "Players".to_string(),
            parent_index: None,
            properties: std::iter::once(("CharacterAutoLoads".to_string(), json!(auto_loads)))
                .chain(capacity.map(|value| (name.to_string(), json!(value))))
                .collect(),
            attributes: Map::new(),
        }],
    };
    let entry = |document: &SettingsBytecode| {
        SnapshotEntry::File(encode_settings_bytecode(document).unwrap())
    };
    for name in [
        "MaxPlayers",
        "PreferredPlayers",
        "MaxPlayersInternal",
        "PreferredPlayersInternal",
    ] {
        let before = entry(&document(name, Some(12), true));
        let desired_document = document(name, Some(60), false);
        let desired = entry(&desired_document);
        let observed = entry(&document(name, None, false));
        let failed_ordinary_write = entry(&document(name, None, true));
        for previous in [None, Some(&before)] {
            assert_eq!(
                settings_delta_mismatch(&path, previous, Some(&desired), Some(&observed), None)
                    .unwrap(),
                None,
                "{name} is not a push target"
            );
            assert!(
                settings_delta_mismatch(
                    &path,
                    previous,
                    Some(&desired),
                    Some(&failed_ordinary_write),
                    None
                )
                .unwrap()
                .is_some_and(|detail| detail.contains("CharacterAutoLoads"))
            );
        }
        let decoded = settings_document(Some(&desired)).unwrap();
        let logical_name = name.strip_suffix("Internal").unwrap_or(name);
        assert_eq!(decoded.instances[0].properties[logical_name], json!(60));
        assert!(!settings_documents_equivalent(
            &decoded,
            &settings_document(Some(&observed)).unwrap()
        ));

        // This policy applies to properties of the actual service root,
        // not attributes, a same-named Folder, or a nested instance.
        let missing = Map::new();
        let mut instance = desired_document.instances[0].clone();
        let requested = Map::from_iter([(name.to_string(), json!(60))]);
        for (class, parent, properties) in [
            ("Players", None, false),
            ("Folder", None, true),
            ("Players", Some(0), true),
        ] {
            instance.class_name = class.to_string();
            instance.parent_index = parent;
            assert!(expected_map_mismatch(&requested, &missing, properties, &instance).is_some());
            assert!(
                changed_map_mismatch(&missing, &requested, &missing, properties, &instance)
                    .is_some()
            );
        }
    }
}

#[test]
fn push_verification_batch_preserves_all_failures_and_decode_errors() {
    let before = ProjectSnapshot::default();
    let mut desired = ProjectSnapshot::default();
    let mut observed = ProjectSnapshot::default();
    for service in ["ReplicatedStorage", "ServerStorage", "Workspace"] {
        let path = PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium"));
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "root".to_string(),
                service.to_string(),
                service.to_string(),
                None,
            )],
        };
        for index in 1..=2_048 {
            let mut part = SettingsBytecodeInstance::new(
                format!("part{index}"),
                format!("Part{index:04}"),
                "Part".to_string(),
                Some(0),
            );
            part.properties.insert("Anchored".to_string(), json!(true));
            document.instances.push(part);
        }
        desired.entries.insert(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        );
        if service != "ServerStorage" {
            for index in [17, 1_537] {
                document.instances[index]
                    .properties
                    .insert("Anchored".to_string(), json!(false));
            }
        }
        observed.entries.insert(
            path,
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        );
    }
    let script = PathBuf::from("src/ServerScriptService/Script.server.luau");
    desired
        .entries
        .insert(script.clone(), SnapshotEntry::File(b"print(1)\n".to_vec()));
    observed
        .entries
        .insert(script, SnapshotEntry::File(b"print(1)\r\n".to_vec()));
    let paths = desired.entries.keys().cloned().collect();
    let expected_paths = vec![
        PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
        PathBuf::from("src/Workspace/__roblox_sync_settings.renium"),
    ];
    for workers in [1, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap();
        pool.install(|| {
            let (mismatches, detail) = snapshot_intended_delta_mismatches(
                &before,
                &desired,
                &observed,
                &paths,
                &mut HashMap::new(),
            )
            .unwrap();
            assert_eq!(mismatches, expected_paths);
            assert!(
                detail.as_deref().is_some_and(
                    |detail| detail.starts_with("Part0017.Anchored was not retained (")
                ),
                "{detail:?}"
            );
            let mut malformed = observed.clone();
            malformed
                .entries
                .insert(expected_paths[0].clone(), SnapshotEntry::File(vec![0]));
            assert!(
                snapshot_intended_delta_mismatches(
                    &before,
                    &desired,
                    &malformed,
                    &paths,
                    &mut HashMap::new()
                )
                .is_err()
            );
        });
    }
}

#[test]
fn targeted_push_verification_ignores_unrelated_studio_edits() {
    let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let snapshot = |anchored: bool, concurrent: Option<&str>| {
        let mut part = SettingsBytecodeInstance {
            settings_id: "part".to_string(),
            name: "Part".to_string(),
            class_name: "Part".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([("Anchored".to_string(), Value::Bool(anchored))]),
            attributes: Map::new(),
        };
        if let Some(value) = concurrent {
            part.attributes.insert(
                "ConcurrentProbe".to_string(),
                Value::String(value.to_string()),
            );
        }
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                part,
            ],
        };
        ProjectSnapshot {
            entries: [(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        }
    };
    let before = snapshot(true, None);
    let desired = snapshot(false, None);
    let observed = snapshot(false, Some("kept"));
    let paths = HashSet::from([path.clone()]);
    let overwritten = snapshot(true, Some("kept"));
    for reuse in [false, true] {
        let prepared = || {
            if reuse {
                HashMap::from([(
                    path.clone(),
                    PreparedPushVerification {
                        previous: settings_document(before.entries.get(&path)).unwrap(),
                        desired: Arc::new(settings_document(desired.entries.get(&path)).unwrap()),
                    },
                )])
            } else {
                HashMap::new()
            }
        };
        let (mismatches, _) = snapshot_intended_delta_mismatches(
            &before,
            &desired,
            &observed,
            &paths,
            &mut prepared(),
        )
        .unwrap();
        assert!(mismatches.is_empty());
        let (mismatches, detail) = snapshot_intended_delta_mismatches(
            &before,
            &desired,
            &overwritten,
            &paths,
            &mut prepared(),
        )
        .unwrap();
        assert_eq!(mismatches, vec![path.clone()]);
        assert!(detail.is_some_and(|detail| detail.contains("Anchored")));
    }
}

#[test]
fn targeted_push_verification_ignores_package_modified_state() {
    let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let instance = |id: &str, name: &str, class_name: &str, parent_index: Option<usize>| {
        SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        }
    };
    let snapshot = |modified_state: i64, archivable: Option<bool>| {
        let event_properties = archivable
            .map(|archivable| Map::from_iter([("Archivable".to_string(), Value::Bool(archivable))]))
            .unwrap_or_default();
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
                instance("package", "testPackage", "Folder", Some(0)),
                SettingsBytecodeInstance {
                    settings_id: "link".to_string(),
                    name: "PackageLink".to_string(),
                    class_name: "PackageLink".to_string(),
                    parent_index: Some(1),
                    properties: Map::from_iter([(
                        "ModifiedState".to_string(),
                        Value::from(modified_state),
                    )]),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "event".to_string(),
                    name: "RemoteEvent".to_string(),
                    class_name: "RemoteEvent".to_string(),
                    parent_index: Some(1),
                    properties: event_properties,
                    attributes: Map::new(),
                },
            ],
        };
        ProjectSnapshot {
            entries: [(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        }
    };
    let before = snapshot(1, Some(false));
    let desired = snapshot(-1, Some(true));
    let observed = snapshot(1, None);
    let paths = HashSet::from([path.clone()]);
    let empty = ProjectSnapshot {
        entries: BTreeMap::new(),
    };
    let (added_mismatches, added_detail) = snapshot_intended_delta_mismatches(
        &empty,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert!(added_mismatches.is_empty(), "{added_detail:?}");
    let (mismatches, _) = snapshot_intended_delta_mismatches(
        &before,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert!(mismatches.is_empty());

    let observed = snapshot(1, Some(false));
    let (mismatches, detail) = snapshot_intended_delta_mismatches(
        &before,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(mismatches, vec![path]);
    assert!(detail.is_some_and(|detail| detail.contains("Archivable")));
}

#[test]
fn targeted_push_verification_checks_script_files_not_settings_source_metadata() {
    let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let source_path = PathBuf::from("src/ReplicatedStorage/ProbeModule.luau");
    let snapshot = |settings_source: &str, file_source: &str| {
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "script".to_string(),
                    name: "ProbeModule".to_string(),
                    class_name: "ModuleScript".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([(
                        "Source".to_string(),
                        Value::String(settings_source.to_string()),
                    )]),
                    attributes: Map::new(),
                },
            ],
        };
        ProjectSnapshot {
            entries: [
                (
                    settings_path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
                ),
                (
                    source_path.clone(),
                    SnapshotEntry::File(file_source.as_bytes().to_vec()),
                ),
            ]
            .into_iter()
            .collect(),
        }
    };
    let before = ProjectSnapshot::default();
    let desired = snapshot("return 'desired'", "return 'desired'\n");
    let observed = snapshot("external", "return 'desired'\n");
    let paths = HashSet::from([settings_path.clone(), source_path.clone()]);
    let (mismatches, _) = snapshot_intended_delta_mismatches(
        &before,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert!(mismatches.is_empty());

    let observed = snapshot("return 'desired'", "return 'wrong'\n");
    let (mismatches, _) = snapshot_intended_delta_mismatches(
        &before,
        &desired,
        &observed,
        &paths,
        &mut HashMap::new(),
    )
    .unwrap();
    assert_eq!(mismatches, vec![source_path]);
}

#[test]
fn reconciliation_treats_elided_class_defaults_as_equal() {
    let omitted = Map::new();
    let default = Map::from_iter([("CanCollide".to_string(), Value::Bool(true))]);
    let changed = Map::from_iter([("CanCollide".to_string(), Value::Bool(false))]);
    assert!(reconciliation_maps_equal("Part", &default, &omitted));
    assert!(reconciliation_maps_equal("Part", &omitted, &default));
    assert!(!reconciliation_maps_equal("Part", &changed, &omitted));
}

#[test]
fn unchanged_local_file_bootstraps_a_replacement_runtime() {
    let digest = "same-digest";

    assert!(should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        Some("old-runtime"),
        Some("new-runtime"),
        Some(digest),
        Some(digest),
    ));
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Verify,
        Some("old-runtime"),
        Some("new-runtime"),
        Some(digest),
        Some(digest),
    ));
}

#[test]
fn changed_or_unknown_local_file_uses_normal_reconciliation() {
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        Some("old-runtime"),
        Some("new-runtime"),
        Some("previous-digest"),
        Some("changed-digest"),
    ));
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        None,
        Some("new-runtime"),
        Some("digest"),
        Some("digest"),
    ));
    assert!(!should_bootstrap_studio_from_editor(
        PairMode::Reconcile,
        Some("old-runtime"),
        Some("new-runtime"),
        None,
        Some("digest"),
    ));
}

#[test]
fn runtime_bootstrap_requires_tracking_without_fresh_edits() {
    assert!(studio_runtime_bootstrap_safe(&json!({
        "tracking": true,
        "dirtyServices": ["ReplicatedStorage"],
        "restoredPendingServices": ["ReplicatedStorage"],
    })));
    assert!(!studio_runtime_bootstrap_safe(&json!({
        "tracking": false,
        "dirtyServices": [],
        "restoredPendingServices": [],
    })));
    assert!(!studio_runtime_bootstrap_safe(&json!({
        "tracking": true,
        "dirtyServices": ["ReplicatedStorage", "StarterGui"],
        "restoredPendingServices": ["ReplicatedStorage"],
    })));
}

#[test]
fn source_equivalence_normalizes_crlf_and_lone_cr_without_allocating() {
    let path = Path::new("script.luau");
    let left = SnapshotEntry::File(b"first\r\nsecond\rthird\n".to_vec());
    let right = SnapshotEntry::File(b"first\nsecond\nthird\n".to_vec());
    assert!(entries_equivalent(path, Some(&left), Some(&right)));
}

#[test]
fn staged_source_root_accepts_cli_relative_and_bound_absolute_paths() {
    let root = if cfg!(windows) {
        Path::new("C:/project")
    } else {
        Path::new("/project")
    };

    assert_eq!(
        project_relative_source_root(root, Path::new("src")).unwrap(),
        Path::new("src")
    );
    assert_eq!(
        project_relative_source_root(root, &root.join("src")).unwrap(),
        Path::new("src")
    );
}

#[test]
fn target_ownership_ends_with_the_live_pair() {
    let coordinator = Coordinator::default();
    let first = PairIdentity {
        experience: "experience".to_string(),
        project: "first".to_string(),
        fingerprint: "first-fingerprint".to_string(),
        game_id: Some(1),
        place_id: Some(2),
        local_file: None,
    };
    let second = PairIdentity {
        project: "second".to_string(),
        fingerprint: "second-fingerprint".to_string(),
        ..first.clone()
    };

    assert_eq!(coordinator.claim_target(&first), None);
    assert_eq!(coordinator.claim_target(&second), Some("first".to_string()));

    coordinator.release_target(&first.pair_key());

    assert_eq!(coordinator.claim_target(&second), None);
}

#[test]
fn first_pairing_unions_separate_files_and_blocks_same_file_conflicts() {
    let editor = file_snapshot(&[("src/A.luau", b"return 'editor'")]);
    let studio = file_snapshot(&[("src/B.luau", b"return 'studio'")]);
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(merged.entries.len(), 2);

    let studio = file_snapshot(&[("src/A.luau", b"return 'studio'")]);
    let (_, conflicts) = merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert_eq!(conflicts.len(), 1);

    let editor = file_snapshot(&[("src/A.luau", b"local a = 1\r\nreturn a\r\n")]);
    let studio = file_snapshot(&[("src/A.luau", b"local a = 1\nreturn a\n")]);
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    assert!(merged == editor);
    assert!(snapshots_equivalent(&editor, &studio).unwrap());

    let service_root = |id: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "ReplicatedStorage".to_string(),
        class_name: "ReplicatedStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let child = |id: &str, name: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: name.to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let settings_snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let editor = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("editor-root"),
            child("editor-child", "FromEditor"),
        ],
    });
    let studio = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("studio-root"),
            child("studio-child", "FromStudio"),
        ],
    });
    let (merged, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let merged = settings_document(merged.entries.get(Path::new(
        "src/ReplicatedStorage/__roblox_sync_settings.renium",
    )))
    .unwrap();
    assert_eq!(merged.instances.len(), 3);

    let duplicate = |id: &str, value: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "Duplicate".to_string(),
        class_name: "StringValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("Value".to_string(), Value::String(value.to_string()))]),
        attributes: Map::new(),
    };
    let editor = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("editor-root"),
            duplicate("editor-a", "A"),
            duplicate("editor-b", "B"),
        ],
    });
    let studio = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            service_root("studio-root"),
            duplicate("studio-a", "B"),
            duplicate("studio-b", "A"),
        ],
    });
    let (_, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::Editor).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("ambiguous duplicate"))
    );

    let editor = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![service_root("editor-root"), child("editor-child", "Same")],
    });
    let mut studio_child = child("studio-child", "Same");
    studio_child.class_name = "Model".to_string();
    let studio = settings_snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![service_root("studio-root"), studio_child],
    });
    let (_, conflicts) =
        merge_snapshots(None, &editor, &studio, ConflictPreference::Editor).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("different classes"))
    );
}

#[test]
fn mismatch_details_ignore_instance_serialization_order() {
    let instance = |id: &str,
                    name: &str,
                    class_name: &str,
                    parent_index: Option<usize>,
                    marker: Option<f64>,
                    value: Option<&str>| {
        let mut instance = SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        };
        if let Some(marker) = marker {
            instance
                .attributes
                .insert("Marker".to_string(), json!(marker));
        }
        if let Some(value) = value {
            instance
                .properties
                .insert("Value".to_string(), json!(value));
        }
        instance
    };
    let snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let expected = snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("root", "ServerStorage", "ServerStorage", None, None, None),
            instance("burst", "Burst", "Folder", Some(0), None, None),
            instance("one", "Node", "Folder", Some(1), Some(1.0), None),
            instance("two", "Node", "Folder", Some(1), Some(2.0), None),
            instance(
                "holder",
                "Holder",
                "StringValue",
                Some(0),
                None,
                Some("editor"),
            ),
        ],
    });
    let observed = snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("root", "ServerStorage", "ServerStorage", None, None, None),
            instance(
                "holder",
                "Holder",
                "StringValue",
                Some(0),
                None,
                Some("studio"),
            ),
            instance("burst", "Burst", "Folder", Some(0), None, None),
            instance("two", "Node", "Folder", Some(2), Some(2.0), None),
            instance("one", "Node", "Folder", Some(2), Some(1.0), None),
        ],
    });
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");

    let detail = snapshot_mismatch_details(&observed, &expected, &[path])
        .unwrap()
        .unwrap();
    assert!(detail.contains("Holder.Value property"), "{detail}");
    assert!(!detail.contains("different structure"), "{detail}");
}

#[test]
fn live_sync_readback_retains_nonfinite_cframes_and_describes_real_differences() {
    let path = PathBuf::from("instances/ReplicatedStorage.renium");
    let mut root = SettingsBytecodeInstance::new(
        "root".into(),
        "ReplicatedStorage".into(),
        "ReplicatedStorage".into(),
        None,
    );
    root.attributes.insert("Revision".into(), json!(1));
    let mut model = SettingsBytecodeInstance::new(
        "car".into(),
        "Cat Mobile 5000".into(),
        "Part".into(),
        Some(0),
    );
    model.properties.insert(
        "CFrame".into(),
        json!({"_type":"CFrame",
        "components": vec![json!({"_type":"Float","value":"nan"});12]}),
    );
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, model],
    };
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let expected = snapshot(&document);
    let mut reordered = document.clone();
    reordered.instances[1].settings_id = "fresh-runtime".into();
    let observed = snapshot(&reordered);
    assert!(
        snapshot_differences(&observed, &expected)
            .unwrap()
            .is_empty()
    );
    assert!(
        snapshot_mismatch_details(&observed, &expected, std::slice::from_ref(&path))
            .unwrap()
            .is_none()
    );

    reordered.instances[1].properties["CFrame"]["components"][0] = json!(42);
    let observed = snapshot(&reordered);
    assert_eq!(
        snapshot_differences(&observed, &expected).unwrap(),
        HashSet::from([path.clone()])
    );
    let detail = snapshot_mismatch_details(&observed, &expected, &[path])
        .unwrap()
        .unwrap();
    assert!(
        detail.contains("Cat Mobile 5000.CFrame property"),
        "{detail}"
    );
    assert!(detail.contains("42") && detail.contains("nan"), "{detail}");
    assert!(detail.contains("CFrame(42.0, nan"), "{detail}");
    assert!(!detail.contains("is CFrame; expected CFrame"), "{detail}");
}

#[test]
fn three_way_merge_keeps_independent_changes() {
    let baseline = file_snapshot(&[
        ("src/A.luau", b"return 'base-a'"),
        ("src/B.luau", b"return 'base-b'"),
    ]);
    let editor = file_snapshot(&[
        ("src/A.luau", b"return 'editor'"),
        ("src/B.luau", b"return 'base-b'"),
    ]);
    let studio = file_snapshot(&[
        ("src/A.luau", b"return 'base-a'"),
        ("src/B.luau", b"return 'studio'"),
    ]);
    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    assert!(
        merged.entries.get(Path::new("src/A.luau")) == editor.entries.get(Path::new("src/A.luau"))
    );
    assert!(
        merged.entries.get(Path::new("src/B.luau")) == studio.entries.get(Path::new("src/B.luau"))
    );

    let shared = file_snapshot(&[("src/A.luau", b"return 'shared'")]);
    let reverted = file_snapshot(&[("src/A.luau", b"return 'base-a'")]);
    let changed = file_snapshot(&[("src/A.luau", b"return 'studio-again'")]);
    let (_, stale_conflicts) = merge_snapshots(
        Some(&baseline),
        &reverted,
        &changed,
        ConflictPreference::None,
    )
    .unwrap();
    assert!(stale_conflicts.is_empty());
    let (_, current_conflicts) =
        merge_snapshots(Some(&shared), &reverted, &changed, ConflictPreference::None).unwrap();
    assert_eq!(current_conflicts.len(), 1);

    let deleted = ProjectSnapshot::default();
    let (merged, structural_conflicts) = merge_snapshots(
        Some(&shared),
        &deleted,
        &changed,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(structural_conflicts.is_empty());
    assert!(!merged.entries.contains_key(Path::new("src/A.luau")));
    let (merged, structural_conflicts) = merge_snapshots(
        Some(&shared),
        &deleted,
        &changed,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(structural_conflicts.is_empty());
    assert!(
        merged.entries.get(Path::new("src/A.luau")) == changed.entries.get(Path::new("src/A.luau"))
    );

    let parent = PathBuf::from("src/Folder");
    let baseline = ProjectSnapshot {
        entries: [(parent.clone(), SnapshotEntry::Directory)]
            .into_iter()
            .collect(),
    };
    let studio = ProjectSnapshot {
        entries: [
            (parent, SnapshotEntry::Directory),
            (
                PathBuf::from("src/Folder/New.luau"),
                SnapshotEntry::File(b"return true".to_vec()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let (merged, structural_conflicts) = merge_snapshots(
        Some(&baseline),
        &ProjectSnapshot::default(),
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(structural_conflicts.is_empty());
    assert!(
        merged.entries.get(Path::new("src/Folder/New.luau"))
            == studio.entries.get(Path::new("src/Folder/New.luau"))
    );

    let instance = |id: &str, name: &str, parent_index: Option<usize>| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: name.to_string(),
        class_name: "Folder".to_string(),
        parent_index,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let settings_snapshot = |instances| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances,
                })
                .unwrap(),
            ),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = settings_snapshot(vec![
        instance("root", "ReplicatedStorage", None),
        instance("a", "A", Some(0)),
        instance("b", "B", Some(0)),
    ]);
    let editor = settings_snapshot(vec![
        instance("root", "ReplicatedStorage", None),
        instance("a", "A", Some(0)),
        instance("b", "B", Some(1)),
    ]);
    let studio = settings_snapshot(vec![
        instance("root", "ReplicatedStorage", None),
        instance("b", "B", Some(0)),
        instance("a", "A", Some(1)),
    ]);
    let (merged, cycle_conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(!cycle_conflicts.is_empty());
    assert_eq!(
        settings_document(merged.entries.values().next())
            .unwrap()
            .instances
            .len(),
        3
    );
    for (preference, expected_a, expected_b) in [
        (ConflictPreference::Studio, Some("b"), Some("root")),
        (ConflictPreference::Editor, Some("root"), Some("a")),
    ] {
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, preference).unwrap();
        assert!(conflicts.is_empty());
        let document = settings_document(merged.entries.values().next()).unwrap();
        let parents = document
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.settings_id.as_str(),
                    instance
                        .parent_index
                        .map(|parent| document.instances[parent].settings_id.as_str()),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(parents["a"], expected_a);
        assert_eq!(parents["b"], expected_b);
    }

    let script_snapshot = |name: &str, source: &[u8]| ProjectSnapshot {
        entries: [
            (
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(
                    encode_settings_bytecode(&SettingsBytecode {
                        version: SETTINGS_BINARY_VERSION,
                        instances: vec![
                            instance("root", "ReplicatedStorage", None),
                            SettingsBytecodeInstance {
                                settings_id: "script".to_string(),
                                name: name.to_string(),
                                class_name: "ModuleScript".to_string(),
                                parent_index: Some(0),
                                properties: Map::new(),
                                attributes: Map::new(),
                            },
                        ],
                    })
                    .unwrap(),
                ),
            ),
            (
                PathBuf::from(format!("src/ReplicatedStorage/{name}.luau")),
                SnapshotEntry::File(source.to_vec()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let baseline = script_snapshot("Original", b"return 'base'");
    let editor = script_snapshot("EditorName", b"return 'editor'");
    let studio = script_snapshot("StudioName", b"return 'studio'");
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(
        merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/EditorName.luau"))
    );
    assert!(
        !merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/StudioName.luau"))
    );
    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(
        merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/StudioName.luau"))
    );
    assert!(
        !merged
            .entries
            .contains_key(Path::new("src/ReplicatedStorage/EditorName.luau"))
    );
}

#[test]
fn source_edit_conflicts_with_script_deletion_before_source_enters_baseline() {
    let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let source_path = PathBuf::from("src/ReplicatedStorage/Logic.luau");
    let settings = |include_script| {
        let mut instances = vec![SettingsBytecodeInstance {
            settings_id: "root".to_string(),
            name: "ReplicatedStorage".to_string(),
            class_name: "ReplicatedStorage".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        }];
        if include_script {
            instances.push(SettingsBytecodeInstance {
                settings_id: "script".to_string(),
                name: "Logic".to_string(),
                class_name: "ModuleScript".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            });
        }
        SnapshotEntry::File(
            encode_settings_bytecode(&SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            })
            .unwrap(),
        )
    };
    let baseline = ProjectSnapshot {
        entries: [(settings_path.clone(), settings(true))]
            .into_iter()
            .collect(),
    };
    let editor = ProjectSnapshot {
        entries: [
            (settings_path.clone(), settings(true)),
            (
                source_path.clone(),
                SnapshotEntry::File(b"return 'edited before baseline'".to_vec()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let studio = ProjectSnapshot {
        entries: [(settings_path.clone(), settings(false))]
            .into_iter()
            .collect(),
    };

    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("deleted here but modified on the other side"));

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(merged.entries.contains_key(&source_path));
    assert_eq!(
        settings_document(merged.entries.get(&settings_path))
            .unwrap()
            .instances
            .len(),
        2
    );

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(!merged.entries.contains_key(&source_path));
    assert_eq!(
        settings_document(merged.entries.get(&settings_path))
            .unwrap()
            .instances
            .len(),
        1
    );

    let (_, conflicts) =
        merge_snapshots(Some(&baseline), &studio, &editor, ConflictPreference::None).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].contains("modified here but deleted on the other side"));

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &studio,
        &editor,
        ConflictPreference::Studio,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(merged.entries.contains_key(&source_path));

    let (merged, conflicts) = merge_snapshots(
        Some(&baseline),
        &studio,
        &editor,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(conflicts.is_empty());
    assert!(!merged.entries.contains_key(&source_path));
}

#[test]
fn three_way_settings_merge_keeps_concurrent_additions() {
    let instance = |id: &str, name: &str, parent_index: Option<usize>| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: name.to_string(),
        class_name: "Folder".to_string(),
        parent_index,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/StarterGui/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("root", "StarterGui", None),
            instance("existing", "Existing", Some(0)),
        ],
    });
    let editor = snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("root", "StarterGui", None),
            instance("existing", "Existing", Some(0)),
            instance("editor", "EditorIndependent", Some(0)),
        ],
    });
    let studio = snapshot(SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            instance("root", "StarterGui", None),
            instance("existing", "Existing", Some(0)),
            instance("studio", "StudioIndependent", Some(0)),
            instance("studio-retry", "EditorIndependent", Some(0)),
        ],
    });

    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let merged = settings_document(
        merged
            .entries
            .get(Path::new("src/StarterGui/__roblox_sync_settings.renium")),
    )
    .unwrap();
    let mut names = merged
        .instances
        .iter()
        .map(|instance| instance.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "EditorIndependent",
            "Existing",
            "StarterGui",
            "StudioIndependent",
        ]
    );
}

fn new_branch_fixture() -> (SettingsBytecode, SettingsBytecode, SettingsBytecode) {
    let instance = |id: &str, name: &str, class: &str, parent| {
        SettingsBytecodeInstance::new(id.to_string(), name.to_string(), class.to_string(), parent)
    };
    let base = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![instance(
            "root",
            "ReplicatedStorage",
            "ReplicatedStorage",
            None,
        )],
    };
    let mut editor = base.clone();
    editor.instances.extend([
        instance("button", "ShiftLockButton", "TextButton", Some(0)),
        instance("corner", "UICorner", "UICorner", Some(1)),
        instance("ref", "Reference", "ObjectValue", Some(1)),
    ]);
    for name in [
        "TopLeftRadius",
        "TopRightRadius",
        "BottomLeftRadius",
        "BottomRightRadius",
    ] {
        editor.instances[2].properties.insert(
            name.to_string(),
            json!({"_type":"UDim","scale":1.0,"offset":0.0}),
        );
    }
    editor.instances[3].properties.insert(
        "Value".to_string(),
        json!({"_type":"Ref","settingsId":"corner"}),
    );
    let mut studio = editor.clone();
    // New instance IDs can collide across observations, including cycles and
    // an unrelated addition occupying the ID wanted by a matching node.
    studio.instances[1].settings_id = "corner".to_string();
    studio.instances[2].settings_id = "button".to_string();
    studio.instances[3].settings_id = "studio-ref".to_string();
    studio.instances[3].properties.insert(
        "Value".to_string(),
        json!({"_type":"Ref","settingsId":"button"}),
    );
    studio.instances[1]
        .properties
        .insert("Sink".to_string(), json!({"_type":"EnumItem","name":"1"}));
    studio
        .instances
        .push(instance("ref", "StudioOnly", "Folder", Some(0)));
    (base, editor, studio)
}

#[test]
fn reconciliation_matches_new_branches_without_duplicating_property_differences() {
    let path = Path::new("src/ReplicatedStorage/__roblox_sync_settings.renium");
    for iteration in 0..32 {
        let (base, mut editor, mut studio) = new_branch_fixture();
        // Vary traversal indices independently of structural path IDs.
        for i in 0..iteration {
            studio.instances.push(SettingsBytecodeInstance::new(
                format!("unrelated-{i}"),
                format!("Unrelated{i}"),
                "Folder".to_string(),
                Some(0),
            ));
        }
        if iteration % 2 == 0 {
            std::mem::swap(&mut editor, &mut studio);
        }
        let snapshot = |doc: &SettingsBytecode| {
            file_snapshot(&[(
                path.to_str().unwrap(),
                &encode_settings_bytecode(doc).unwrap(),
            )])
        };
        let (merged, conflicts) = merge_snapshots(
            Some(&snapshot(&base)),
            &snapshot(&editor),
            &snapshot(&studio),
            ConflictPreference::None,
        )
        .unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
        let merged = settings_document(merged.entries.get(path)).unwrap();
        for name in ["ShiftLockButton", "UICorner", "Reference", "StudioOnly"] {
            assert_eq!(
                merged.instances.iter().filter(|i| i.name == name).count(),
                1,
                "{name}, iteration {iteration}"
            );
        }
        let corner = merged
            .instances
            .iter()
            .find(|i| i.name == "UICorner")
            .unwrap();
        assert_eq!(
            corner.properties,
            new_branch_fixture().1.instances[2].properties
        );
        let holder = merged
            .instances
            .iter()
            .find(|i| i.name == "Reference")
            .unwrap();
        assert_eq!(holder.properties["Value"]["settingsId"], corner.settings_id);
        assert_eq!(
            merged
                .instances
                .iter()
                .map(|i| &i.settings_id)
                .collect::<HashSet<_>>()
                .len(),
            merged.instances.len()
        );
        let mut plan = ReconcilePushPlan::default();
        append_settings_push_plan(path, &merged, &studio, &mut plan).unwrap();
        assert!(
            !plan.recreated_settings_ids.contains(&corner.settings_id),
            "untouched corner scheduled for recreation"
        );
        assert!(
            plan.property_removals
                .iter()
                .all(|change| change.settings_id.as_deref() != Some(&corner.settings_id)),
            "unchanged radii scheduled for reset"
        );
    }
}

#[test]
fn matched_new_property_conflicts_do_not_become_copies() {
    let (base, mut editor, mut studio) = new_branch_fixture();
    editor.instances[1]
        .properties
        .insert("Text".to_string(), json!("editor"));
    studio.instances[1]
        .properties
        .insert("Text".to_string(), json!("studio"));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let (merged, conflicts) = merge_reconciliation_settings_documents(
        &base,
        &editor,
        &studio,
        ConflictPreference::None,
        &HashSet::new(),
        &HashSet::new(),
    );
    assert_eq!(
        merged
            .instances
            .iter()
            .filter(|i| i.name == "ShiftLockButton")
            .count(),
        1
    );
    assert!(conflicts.iter().any(|c| c.detail.contains("Text")));
}

#[test]
fn new_duplicate_names_with_different_values_are_not_guessed() {
    let (base, editor, mut studio) = new_branch_fixture();
    studio.instances[1]
        .properties
        .insert("Text".to_string(), json!("studio"));
    let mut duplicate = studio.instances[1].clone();
    duplicate.settings_id = "duplicate".to_string();
    studio.instances.push(duplicate);
    let before = encode_settings_bytecode(&studio).unwrap();
    let error = align_new_instance_ids(&base, &editor, &mut studio).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Ambiguous new duplicate instances")
    );
    assert_eq!(encode_settings_bytecode(&studio).unwrap(), before);
}

#[test]
fn new_duplicates_in_a_different_sibling_order_pair_by_their_data() {
    let (base, editor, mut studio) = new_branch_fixture();
    let mut editor = editor;
    let border = |id: &str, x: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Border".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties.insert(
            "Size".to_string(),
            json!({"_type":"Vector3","x":x,"y":1.0,"z":1.0}),
        );
        part
    };
    editor.instances.push(border("left", 1.0));
    editor.instances.push(border("middle", 2.0));
    editor.instances.push(border("right", 3.0));
    studio.instances.push(border("s3", 3.0));
    studio.instances.push(border("s1", 1.0));
    studio.instances.push(border("s2", 2.0));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let id_of = |x: f64| {
        studio
            .instances
            .iter()
            .find(|i| i.name == "Border" && i.properties["Size"]["x"] == json!(x))
            .unwrap()
            .settings_id
            .clone()
    };
    assert_eq!(id_of(1.0), "left");
    assert_eq!(id_of(2.0), "middle");
    assert_eq!(id_of(3.0), "right");
}

#[test]
fn extra_new_duplicates_on_one_side_are_additions_once_the_rest_pair_by_data() {
    let border = |id: &str, x: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Border".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties.insert(
            "Size".to_string(),
            json!({"_type":"Vector3","x":x,"y":1.0,"z":1.0}),
        );
        part
    };
    let (base, mut editor, mut studio) = new_branch_fixture();
    editor.instances.push(border("left", 1.0));
    editor.instances.push(border("right", 2.0));
    studio.instances.push(border("extra", 9.0));
    studio.instances.push(border("s2", 2.0));
    studio.instances.push(border("s1", 1.0));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let id_of = |x: f64| {
        studio
            .instances
            .iter()
            .find(|i| i.name == "Border" && i.properties["Size"]["x"] == json!(x))
            .unwrap()
            .settings_id
            .clone()
    };
    assert_eq!(id_of(1.0), "left");
    assert_eq!(id_of(2.0), "right");
    assert_eq!(id_of(9.0), "extra");

    let (base, mut editor, mut studio) = new_branch_fixture();
    editor.instances.push(border("left", 1.0));
    editor.instances.push(border("changed", 2.0));
    studio.instances.push(border("s1", 1.0));
    studio.instances.push(border("s2", 3.0));
    let error = align_new_instance_ids(&base, &editor, &mut studio).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Ambiguous new duplicate instances")
    );
}

#[test]
fn children_follow_a_new_parent_paired_with_a_reordered_sibling() {
    let (base, mut editor, mut studio) = new_branch_fixture();
    let part = |id: &str, x: f64| {
        let mut part = SettingsBytecodeInstance::new(
            id.to_string(),
            "Part".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part.properties.insert(
            "Size".to_string(),
            json!({"_type":"Vector3","x":x,"y":1.0,"z":1.0}),
        );
        part
    };
    let texture = |id: &str, parent: usize, r: f64| {
        let mut texture = SettingsBytecodeInstance::new(
            id.to_string(),
            "Texture".to_string(),
            "Texture".to_string(),
            Some(parent),
        );
        texture.properties.insert(
            "Color3".to_string(),
            json!({"_type":"Color3","r":r,"g":1.0,"b":1.0}),
        );
        texture
    };
    let editor_first = editor.instances.len();
    editor.instances.push(part("first", 1.0));
    editor.instances.push(part("second", 2.0));
    editor
        .instances
        .push(texture("first-texture", editor_first, 1.0));
    editor
        .instances
        .push(texture("second-texture", editor_first + 1, 0.5));
    let studio_first = studio.instances.len();
    studio.instances.push(part("s2", 2.0));
    studio.instances.push(part("s1", 1.0));
    studio
        .instances
        .push(texture("s2-texture", studio_first, 0.5));
    studio
        .instances
        .push(texture("s1-texture", studio_first + 1, 1.0));
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    let id_of = |name: &str, r: f64| {
        studio
            .instances
            .iter()
            .find(|i| i.name == name && i.properties["Color3"]["r"] == json!(r))
            .unwrap()
            .settings_id
            .clone()
    };
    assert_eq!(id_of("Texture", 1.0), "first-texture");
    assert_eq!(id_of("Texture", 0.5), "second-texture");
}

#[test]
fn new_duplicates_keep_persistent_identity_after_edits_and_reordering() {
    let root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    let base = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    let mut editor = base.clone();
    for i in 1..=2 {
        let mut part = SettingsBytecodeInstance::new(
            format!("debug:old-{i}"),
            "Part".into(),
            "Part".into(),
            Some(0),
        );
        part.properties.insert(
            "UniqueId".into(),
            json!({"_type":"UniqueId","value":format!("{i:032x}")}),
        );
        part.properties
            .insert("Transparency".into(), json!(i as f64 / 10.0));
        editor.instances.push(part);
    }
    let mut studio = editor.clone();
    studio.instances[1].settings_id = "debug:new-1".into();
    studio.instances[2].settings_id = "debug:new-2".into();
    studio.instances[1]
        .properties
        .insert("Transparency".into(), json!(0.7));
    studio.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.8));
    studio.instances[0].properties.insert(
        "PrimaryPart".into(),
        json!({"_type":"Ref","settingsId":"debug:new-1"}),
    );
    studio.instances.swap(1, 2);
    let mut first_pair = studio.clone();
    let mut first_pair_conflicts = Vec::new();
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor.clone(),
        &mut first_pair,
        ConflictPreference::None,
        &mut first_pair_conflicts,
    );
    assert!(!first_pair_conflicts.is_empty());
    assert!(
        first_pair_conflicts.iter().all(
            |conflict| !conflict.contains("ambiguous") && !conflict.contains("different paths")
        ),
        "{first_pair_conflicts:?}"
    );
    align_new_instance_ids(&base, &editor, &mut studio).unwrap();
    assert_eq!(studio.instances[1].settings_id, "debug:old-2");
    assert_eq!(studio.instances[2].settings_id, "debug:old-1");
    assert_eq!(
        studio.instances[0].properties["PrimaryPart"]["settingsId"],
        "debug:old-1"
    );
    assert_eq!(studio.instances[2].properties["Transparency"], 0.7);
    let (merged, conflicts) = merge_reconciliation_settings_documents(
        &base,
        &editor,
        &studio,
        ConflictPreference::None,
        &HashSet::new(),
        &HashSet::new(),
    );
    assert_eq!(
        merged.instances.len(),
        3,
        "matched parts must not be duplicated"
    );
    assert!(
        !conflicts.is_empty(),
        "identity matching must retain the real value conflict"
    );
}

#[test]
fn first_pairing_ignores_only_the_identified_viewport_camera() {
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type":"Ref","settingsId":"viewport"}),
    );
    let mut camera =
        SettingsBytecodeInstance::new("viewport".into(), "View".into(), "Camera".into(), Some(0));
    camera.properties.insert("FieldOfView".into(), json!(70));
    let mut authored = camera.clone();
    authored.settings_id = "authored".into();
    authored.name = "AuthoredCamera".into();
    let editor = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, camera, authored],
    };
    let mut studio = editor.clone();
    studio.instances[1]
        .properties
        .insert("FieldOfView".into(), json!(90));
    studio.instances[2]
        .properties
        .insert("FieldOfView".into(), json!(100));
    let mut conflicts = Vec::new();
    align_first_pairing(
        Path::new("instances/Workspace.renium"),
        &mut editor.clone(),
        &mut studio,
        ConflictPreference::None,
        &mut conflicts,
    );
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].contains("AuthoredCamera"));
    assert_eq!(studio.instances[1].properties["FieldOfView"], 70);
}

#[test]
fn prepared_full_push_reuses_the_same_duplicate_and_reference_mapping() {
    let root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    let mut first = SettingsBytecodeInstance::new(
        "first".into(),
        "Duplicate".into(),
        "StringValue".into(),
        Some(0),
    );
    first.properties.insert("Value".into(), json!("first"));
    let mut second = first.clone();
    second.settings_id = "second".into();
    second.properties.insert("Value".into(), json!("second"));
    let mut pointer = SettingsBytecodeInstance::new(
        "pointer".into(),
        "Pointer".into(),
        "ObjectValue".into(),
        Some(0),
    );
    pointer
        .properties
        .insert("Value".into(), json!({"_type":"Ref","settingsId":"first"}));
    let mut desired = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, first, second, pointer],
    };
    let mut observed = desired.clone();
    observed.instances.swap(1, 2);
    observed.instances[1].settings_id = "debug:second".into();
    observed.instances[2].settings_id = "debug:first".into();
    observed.instances[3].properties.insert(
        "Value".into(),
        json!({"_type":"Ref","settingsId":"debug:first"}),
    );
    desired.instances[0]
        .attributes
        .insert("Revision".into(), json!(1));
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |doc: &SettingsBytecode| ProjectSnapshot {
        entries: BTreeMap::from([(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(doc).unwrap()),
        )]),
    };
    let studio = snapshot(&observed);
    let project = snapshot(&desired);
    let mut prepared = HashMap::new();
    let paths = snapshot_differences_prepared(&project, &studio, Some(&mut prepared)).unwrap();
    assert_eq!(paths, snapshot_differences(&project, &studio).unwrap());
    assert_eq!(prepared.len(), 1);
    let change = &prepared[&path];
    assert_eq!(change.previous.instances[1].settings_id, "second");
    assert_eq!(change.previous.instances[2].settings_id, "first");
    assert_eq!(
        change.previous.instances[3].properties["Value"]["settingsId"],
        "first"
    );
    let expected = reconciliation_push_plan_for_paths(&studio, &project, &paths).unwrap();
    let actual = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio, &project, &paths, &prepared, false, false,
    )
    .unwrap();
    assert_eq!(actual.changed_paths, expected.changed_paths);
    assert_eq!(actual.target_settings_ids, expected.target_settings_ids);
    assert_eq!(
        actual.recreated_settings_ids,
        expected.recreated_settings_ids
    );
    assert_eq!(
        serde_json::to_value(&actual.instance_deletes).unwrap(),
        serde_json::to_value(&expected.instance_deletes).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&actual.property_removals).unwrap(),
        serde_json::to_value(&expected.property_removals).unwrap()
    );
}

#[test]
fn viewport_retention_does_not_skip_other_camera_mutations_or_verification() {
    let path = Path::new("src/Workspace/__roblox_sync_settings.renium");
    let mut root =
        SettingsBytecodeInstance::new("root".into(), "Workspace".into(), "Workspace".into(), None);
    root.properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "settingsId": "viewport"}),
    );
    let mut camera =
        SettingsBytecodeInstance::new("viewport".into(), "Camera".into(), "Camera".into(), Some(0));
    camera.properties.insert("FieldOfView".into(), json!(70));
    let mut other = camera.clone();
    other.settings_id = "other".into();
    let before = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, camera, other],
    };
    let entry = |document: &SettingsBytecode| {
        SnapshotEntry::File(encode_settings_bytecode(document).unwrap())
    };
    let before_entry = entry(&before);
    let mut desired = before.clone();
    for instance in &mut desired.instances[1..] {
        instance.properties.insert("FieldOfView".into(), json!(85));
    }
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(path, &desired, &before, &mut plan).unwrap();
    assert_eq!(plan.target_settings_ids, ["other"]);
    let desired_entry = entry(&desired);
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&before_entry),
            None
        )
        .unwrap()
        .is_some()
    );
    let mut observed = before.clone();
    observed.instances[2]
        .properties
        .insert("FieldOfView".into(), json!(85));
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&entry(&observed)),
            None
        )
        .unwrap()
        .is_none()
    );

    desired.instances.truncate(1);
    desired.instances[0].properties.remove("CurrentCamera");
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(path, &desired, &before, &mut plan).unwrap();
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    let desired_entry = entry(&desired);
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&before_entry),
            None
        )
        .unwrap()
        .is_some()
    );
    observed.instances.truncate(2);
    assert!(
        settings_delta_mismatch(
            path,
            Some(&before_entry),
            Some(&desired_entry),
            Some(&entry(&observed)),
            None
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn omitted_engine_container_keeps_anchor_but_removes_ordinary_contents() {
    for (service, class_name) in [
        ("Workspace", "Terrain"),
        ("StarterPlayer", "StarterPlayerScripts"),
        ("StarterPlayer", "StarterCharacterScripts"),
        ("TextChatService", "ChatWindowConfiguration"),
        ("TextChatService", "ChatInputBarConfiguration"),
        ("TextChatService", "BubbleChatConfiguration"),
        ("TextChatService", "ChannelTabsConfiguration"),
    ] {
        let instance = |id: &str, name: &str, class: &str, parent_index| SettingsBytecodeInstance {
            settings_id: id.into(),
            name: name.into(),
            class_name: class.into(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let root = instance("root", service, service, None);
        let container = instance("anchor", class_name, class_name, Some(0));
        let child = instance("child", "RemovedContent", "Folder", Some(1));
        let namesake = instance("namesake", class_name, "Folder", Some(0));
        let document = |instances| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let before = document(vec![root.clone(), container.clone(), child, namesake]);
        let desired = document(vec![root.clone()]);
        let actual = document(vec![root, container]);
        let path = PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium"));
        let mut plan = ReconcilePushPlan::default();
        append_aligned_settings_push_plan(&path, &desired, &before, &mut plan).unwrap();
        let deleted: HashSet<_> = plan
            .instance_deletes
            .iter()
            .flat_map(|change| {
                change
                    .instances
                    .iter()
                    .map(|entry| entry.settings_id.as_str())
            })
            .collect();
        assert_eq!(
            deleted,
            HashSet::from(["child", "namesake"]),
            "{class_name}"
        );
        let entry =
            |doc: &SettingsBytecode| SnapshotEntry::File(encode_settings_bytecode(doc).unwrap());
        for desired in [Some(entry(&desired)), None] {
            assert_eq!(
                settings_delta_mismatch(
                    &path,
                    Some(&entry(&before)),
                    desired.as_ref(),
                    Some(&entry(&actual)),
                    None
                )
                .unwrap(),
                None,
                "{class_name}"
            );
            assert!(
                settings_delta_mismatch(
                    &path,
                    Some(&entry(&before)),
                    desired.as_ref(),
                    Some(&entry(&before)),
                    None
                )
                .unwrap()
                .is_some(),
                "Contents must be removed"
            );
        }
        let mut authored = actual.clone();
        authored.instances[1]
            .attributes
            .insert("Authored".into(), json!(true));
        assert!(
            settings_delta_mismatch(
                &path,
                Some(&entry(&actual)),
                Some(&entry(&authored)),
                Some(&entry(&actual)),
                None
            )
            .unwrap()
            .is_some(),
            "Authored anchor edits must verify"
        );
    }
}

#[test]
fn removed_service_store_keeps_only_the_engine_service() {
    let path = Path::new("src/ServerStorage/__roblox_sync_settings.renium");
    let root = SettingsBytecodeInstance {
        settings_id: "root".into(),
        name: "ServerStorage".into(),
        class_name: "ServerStorage".into(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::from_iter([("Preserved".into(), json!(true))]),
    };
    let child = SettingsBytecodeInstance {
        settings_id: "child".into(),
        name: "MovedPointer".into(),
        class_name: "ObjectValue".into(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let document = |instances| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    let entry =
        |doc: &SettingsBytecode| SnapshotEntry::File(encode_settings_bytecode(doc).unwrap());
    let before = document(vec![root.clone(), child.clone()]);
    let before_entry = entry(&before);
    let empty = document(Vec::new());
    let mut plan = ReconcilePushPlan::default();
    append_aligned_settings_push_plan(path, &empty, &before, &mut plan).unwrap();
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert!(plan.property_removals.is_empty());
    let remaining = entry(&document(vec![root.clone()]));
    assert_eq!(
        settings_delta_mismatch(path, Some(&before_entry), None, Some(&remaining), None).unwrap(),
        None
    );
    for changed_id in [false, true] {
        let mut retained = document(vec![root.clone(), child.clone()]);
        if changed_id {
            retained.instances[0].settings_id = "export-root".into();
            retained.instances[1].settings_id = "export-child".into();
        }
        assert!(
            settings_delta_mismatch(
                path,
                Some(&before_entry),
                None,
                Some(&entry(&retained)),
                None
            )
            .unwrap()
            .is_some_and(|message| message.contains("MovedPointer was not deleted"))
        );
    }
}

#[test]
fn ordinary_reconciliation_never_deletes_a_package_link() {
    let root = SettingsBytecodeInstance {
        settings_id: "root".to_string(),
        name: "ReplicatedStorage".to_string(),
        class_name: "ReplicatedStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let package_link = SettingsBytecodeInstance {
        settings_id: "link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let baseline_doc = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), package_link],
    };
    let editor_doc = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root],
    };
    let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(&baseline_doc);
    let editor = snapshot(&editor_doc);
    let studio = snapshot(&baseline_doc);
    let (_, conflicts) = merge_snapshots(
        Some(&baseline),
        &editor,
        &studio,
        ConflictPreference::Editor,
    )
    .unwrap();
    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.contains("PackageLink"))
    );
    assert!(
        validate_editor_package_links(&baseline, &editor, std::slice::from_ref(&settings_path))
            .is_err()
    );
}

#[test]
fn reconciliation_allows_replacing_an_entire_package_root() {
    let instance =
        |id: &str, name: &str, class_name: &str, parent_index| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index,
            properties: Map::new(),
            attributes: Map::new(),
        };
    let root = instance("root", "Workspace", "Workspace", None);
    let old_package = instance("old-package", "Old car", "Model", Some(0));
    let old_link = instance("old-link", "PackageLink", "PackageLink", Some(1));
    let new_package = instance("new-package", "New car", "Model", Some(0));
    let new_link = instance("new-link", "PackageLink", "PackageLink", Some(1));
    let document = |instances| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    let settings_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            settings_path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(document(vec![
        root.clone(),
        old_package.clone(),
        old_link.clone(),
    ]));
    let editor = baseline.clone();
    let studio = snapshot(document(vec![root.clone(), new_package, new_link]));

    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let merged = settings_document(merged.entries.get(&settings_path)).unwrap();
    assert!(
        merged
            .instances
            .iter()
            .any(|instance| instance.name == "New car")
    );
    assert!(
        !merged
            .instances
            .iter()
            .any(|instance| instance.name == "Old car")
    );

    let removed_package = snapshot(document(vec![root]));
    assert!(
        validate_editor_package_links(
            &baseline,
            &removed_package,
            std::slice::from_ref(&settings_path),
        )
        .is_ok()
    );
    let direct_link_removal = snapshot(document(vec![
        instance("root", "Workspace", "Workspace", None),
        old_package,
    ]));
    assert!(
        validate_editor_package_links(
            &baseline,
            &direct_link_removal,
            std::slice::from_ref(&settings_path),
        )
        .is_err()
    );
}

#[test]
fn package_link_guard_uses_project_identity_not_export_ids() {
    let document = |ids: [&str; 4], moved: bool, package_content: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: ids[0].to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: ids[1].to_string(),
                name: "Container".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: ids[2].to_string(),
                name: "Package".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(usize::from(moved)),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: ids[3].to_string(),
                name: "PackageLink".to_string(),
                class_name: "PackageLink".to_string(),
                parent_index: Some(2),
                properties: Map::from_iter([(
                    "PackageContent".to_string(),
                    Value::String(package_content.to_string()),
                )]),
                attributes: Map::new(),
            },
        ],
    };
    let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let baseline = snapshot(&document(
        ["root", "container", "package", "link"],
        false,
        "rbxassetid://1",
    ));
    let exported = snapshot(&document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://1",
    ));
    assert!(
        validate_editor_package_links(&baseline, &exported, std::slice::from_ref(&path)).is_ok()
    );
    let mut transport_omission = document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://1",
    );
    transport_omission.instances[3]
        .properties
        .remove("PackageContent");
    transport_omission.instances[3]
        .properties
        .insert("Archivable".to_string(), Value::Bool(true));
    assert!(
        validate_editor_package_links(
            &baseline,
            &snapshot(&transport_omission),
            std::slice::from_ref(&path),
        )
        .is_ok()
    );
    let mut runtime_state = document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://1",
    );
    runtime_state.instances[3]
        .properties
        .insert("ModifiedState".to_string(), Value::Number(1.into()));
    assert!(
        validate_editor_package_links(
            &baseline,
            &snapshot(&runtime_state),
            std::slice::from_ref(&path),
        )
        .is_ok()
    );
    let aligned = align_snapshot_ids(&baseline, &exported).unwrap();
    let aligned = settings_document(aligned.entries.get(&path)).unwrap();
    assert_eq!(aligned.instances[3].settings_id, "link");
    assert_eq!(
        aligned.instances[3].properties.get("PackageContent"),
        Some(&Value::String("rbxassetid://1".to_string()))
    );

    let moved = snapshot(&document(
        ["root", "container", "package", "link"],
        true,
        "rbxassetid://1",
    ));
    assert!(validate_editor_package_links(&baseline, &moved, std::slice::from_ref(&path)).is_ok());

    let edited = snapshot(&document(
        ["debug:1", "debug:2", "debug:3", "debug:4"],
        false,
        "rbxassetid://2",
    ));
    assert!(
        validate_editor_package_links(&baseline, &edited, std::slice::from_ref(&path)).is_err()
    );
}

#[test]
fn reconciliation_pushes_only_semantically_changed_instances() {
    let settings_snapshot = |service: &str, document: SettingsBytecode| {
        (
            PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium")),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )
    };
    let root = |service: &str| SettingsBytecodeInstance {
        settings_id: format!("{service}-root"),
        name: service.to_string(),
        class_name: service.to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let replicated = |value: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            root("ReplicatedStorage"),
            SettingsBytecodeInstance {
                settings_id: "changed-folder".to_string(),
                name: "Changed".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::from_iter([(
                    "Archivable".to_string(),
                    Value::String(value.to_string()),
                )]),
                attributes: Map::new(),
            },
        ],
    };
    let server = |asset_id: &str, replacement_class: &str| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            root("ServerStorage"),
            SettingsBytecodeInstance {
                settings_id: "replacement".to_string(),
                name: "Replacement".to_string(),
                class_name: replacement_class.to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "public".to_string(),
                name: "Public".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "package-link".to_string(),
                name: "PackageLink".to_string(),
                class_name: "PackageLink".to_string(),
                parent_index: Some(2),
                properties: Map::from_iter([(
                    "PackageId".to_string(),
                    Value::String(asset_id.to_string()),
                )]),
                attributes: Map::new(),
            },
        ],
    };
    let studio = ProjectSnapshot {
        entries: [
            settings_snapshot("ReplicatedStorage", replicated("before")),
            settings_snapshot("ServerStorage", server("old", "Folder")),
        ]
        .into_iter()
        .collect(),
    };
    let merged = ProjectSnapshot {
        entries: [
            settings_snapshot("ReplicatedStorage", replicated("after")),
            settings_snapshot("ServerStorage", server("new", "Model")),
        ]
        .into_iter()
        .collect(),
    };

    let plan = reconciliation_push_plan(&studio, &merged).unwrap();
    assert_eq!(
        plan.changed_paths,
        vec![
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
        ]
    );
    assert_eq!(
        plan.target_settings_ids,
        vec!["changed-folder", "replacement"]
    );
    assert_eq!(
        plan.previous_class_names
            .get(&("ServerStorage".to_string(), "replacement".to_string()))
            .map(String::as_str),
        Some("Folder")
    );
    assert!(
        !plan
            .target_settings_ids
            .iter()
            .any(|id| id == "package-link")
    );
    assert!(plan.instance_deletes.is_empty());

    let ordered = |ids: [&str; 3]| SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: std::iter::once(root("ReplicatedStorage"))
            .chain(ids.into_iter().map(|id| SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: id.to_uppercase(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            }))
            .collect(),
    };
    let mut order_plan = ReconcilePushPlan::default();
    append_settings_push_plan(
        Path::new("src/ReplicatedStorage/__roblox_sync_settings.renium"),
        &ordered(["a", "c", "b"]),
        &ordered(["a", "b", "c"]),
        &mut order_plan,
    )
    .unwrap();
    assert!(order_plan.target_settings_ids.is_empty());
}

#[test]
fn incremental_editor_plan_uses_persisted_ids_for_duplicate_siblings() {
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let root = SettingsBytecodeInstance {
        settings_id: "1".to_string(),
        name: "Workspace".to_string(),
        class_name: "Workspace".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let duplicate = |index: usize| SettingsBytecodeInstance {
        settings_id: format!("debug:{index}"),
        name: "Duplicate".to_string(),
        class_name: "StringValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("Value".to_string(), json!(index.to_string()))]),
        attributes: Map::new(),
    };
    let package_link = SettingsBytecodeInstance {
        settings_id: "debug:package-link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([("PackageContent".to_string(), json!("rbxassetid://1"))]),
        attributes: Map::new(),
    };
    let mut previous_instances = Vec::with_capacity(4_098);
    previous_instances.push(root.clone());
    previous_instances.extend((0..4_096).map(duplicate));
    previous_instances.push(package_link.clone());
    let previous_document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: previous_instances,
    };
    let mut current_document = previous_document.clone();
    current_document.instances[1_001]
        .properties
        .insert("Value".to_string(), json!("changed"));
    current_document.instances.remove(2_001);
    current_document.instances.push(SettingsBytecodeInstance {
        settings_id: "debug:new".to_string(),
        ..duplicate(4_096)
    });

    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let previous = snapshot(&previous_document);
    let current = snapshot(&current_document);
    let prepared =
        prepare_editor_settings_changes(&previous, &current, std::slice::from_ref(&path)).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &HashSet::from_iter([path.clone()]),
        &prepared,
        false,
        false,
    )
    .unwrap();

    assert_eq!(plan.changed_paths, vec![path]);
    assert_eq!(
        plan.target_settings_ids,
        vec!["debug:1000".to_string(), "debug:new".to_string()]
    );
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert_eq!(
        plan.instance_deletes[0].instances[0].settings_id,
        "debug:2000"
    );
}

#[test]
fn cross_service_recreation_reapplies_external_referrers() {
    let root = |service: &str| SettingsBytecodeInstance {
        settings_id: format!("{service}-root"),
        name: service.to_string(),
        class_name: service.to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let target = |parent_index| SettingsBytecodeInstance {
        settings_id: "target".to_string(),
        name: "Target".to_string(),
        class_name: "StringValue".to_string(),
        parent_index: Some(parent_index),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let holder = SettingsBytecodeInstance {
        settings_id: "holder".to_string(),
        name: "Holder".to_string(),
        class_name: "ObjectValue".to_string(),
        parent_index: Some(1),
        properties: Map::from_iter([(
            "Value".to_string(),
            json!({"_type": "Ref", "settingsId": "target"}),
        )]),
        attributes: Map::new(),
    };
    let container = SettingsBytecodeInstance {
        settings_id: "container".to_string(),
        name: "Container".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let snapshot = |workspace: SettingsBytecode, replicated: SettingsBytecode| ProjectSnapshot {
        entries: [
            (
                PathBuf::from("src/Workspace/__roblox_sync_settings.renium"),
                SnapshotEntry::File(encode_settings_bytecode(&workspace).unwrap()),
            ),
            (
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(encode_settings_bytecode(&replicated).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let studio = snapshot(
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                root("Workspace"),
                container.clone(),
                target(1),
                holder.clone(),
            ],
        },
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("ReplicatedStorage")],
        },
    );
    let desired = snapshot(
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("Workspace"), container, holder],
        },
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("ReplicatedStorage"), target(0)],
        },
    );

    let plan = reconciliation_push_plan(&studio, &desired).unwrap();

    assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
    assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));

    let paths = desired.entries.keys().cloned().collect::<HashSet<_>>();
    for mask in 0..4 {
        let prepared = prepare_editor_settings_changes(
            &studio,
            &desired,
            &paths.iter().cloned().collect::<Vec<_>>(),
        )
        .unwrap()
        .into_iter()
        .filter(|(path, _)| {
            let bit = if path.starts_with("src/Workspace") {
                1
            } else {
                2
            };
            mask & bit != 0
        })
        .collect();
        let cached = reconciliation_push_plan_for_paths_with_prepared_settings(
            &studio, &desired, &paths, &prepared, false, false,
        )
        .unwrap();
        assert_eq!(cached.changed_paths, plan.changed_paths);
        assert_eq!(cached.target_settings_ids, plan.target_settings_ids);
        assert_eq!(
            serde_json::to_value(&cached.instance_deletes).unwrap(),
            serde_json::to_value(&plan.instance_deletes).unwrap()
        );
    }
}

#[test]
fn incremental_cross_service_recreation_reapplies_target_service_referrers() {
    let root = |service: &str| SettingsBytecodeInstance {
        settings_id: format!("{service}-root"),
        name: service.to_string(),
        class_name: service.to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let target = SettingsBytecodeInstance {
        settings_id: "target".to_string(),
        name: "Target".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let holder = |service: &str| SettingsBytecodeInstance {
        settings_id: "holder".to_string(),
        name: "Holder".to_string(),
        class_name: "ObjectValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([(
            "Value".to_string(),
            json!({
                "_type": "Ref",
                "settingsId": "target",
                "pathSegments": [service, "Target"],
                "pathOrdinals": [1, 1]
            }),
        )]),
        attributes: Map::new(),
    };
    let replicated_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
    let storage_path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let previous_replicated = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ReplicatedStorage"), target.clone()],
    };
    let previous_storage = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ServerStorage"), holder("ReplicatedStorage")],
    };
    let current_replicated = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ReplicatedStorage")],
    };
    let current_storage = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root("ServerStorage"), holder("ServerStorage"), target],
    };
    let previous = ProjectSnapshot {
        entries: [
            (
                replicated_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&previous_replicated).unwrap()),
            ),
            (
                storage_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&previous_storage).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let current = ProjectSnapshot {
        entries: [
            (
                replicated_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&current_replicated).unwrap()),
            ),
            (
                storage_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&current_storage).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let scopes = vec![replicated_path.clone(), storage_path.clone()];
    let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &scopes.into_iter().collect(),
        &prepared,
        false,
        false,
    )
    .unwrap();

    assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
    assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
}

#[test]
fn incremental_added_target_reapplies_existing_same_service_referrer() {
    let root = SettingsBytecodeInstance {
        settings_id: "storage-root".to_string(),
        name: "ServerStorage".to_string(),
        class_name: "ServerStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let holder = |service: &str| SettingsBytecodeInstance {
        settings_id: "holder".to_string(),
        name: "Holder".to_string(),
        class_name: "ObjectValue".to_string(),
        parent_index: Some(0),
        properties: Map::from_iter([(
            "Value".to_string(),
            json!({
                "_type": "Ref",
                "settingsId": "target",
                "pathSegments": [service, "Target"],
                "pathOrdinals": [1, 1]
            }),
        )]),
        attributes: Map::new(),
    };
    let target = SettingsBytecodeInstance {
        settings_id: "target".to_string(),
        name: "Target".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
    let previous_document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root.clone(), holder("ReplicatedStorage")],
    };
    let current_document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![root, holder("ServerStorage"), target],
    };
    let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
        entries: [(
            path.clone(),
            SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    let previous = snapshot(&previous_document);
    let current = snapshot(&current_document);
    let scopes = vec![path.clone()];
    let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &previous,
        &current,
        &HashSet::from([path]),
        &prepared,
        false,
        false,
    )
    .unwrap();

    assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
    assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
}

#[test]
fn reconciliation_uses_targeted_deletes_and_blocks_direct_package_link_deletes() {
    let root = SettingsBytecodeInstance {
        settings_id: "root".to_string(),
        name: "ServerStorage".to_string(),
        class_name: "ServerStorage".to_string(),
        parent_index: None,
        properties: Map::new(),
        attributes: Map::new(),
    };
    let folder = SettingsBytecodeInstance {
        settings_id: "folder".to_string(),
        name: "Removed".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let snapshot = |instances| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances,
                })
                .unwrap(),
            ),
        )]
        .into_iter()
        .collect(),
    };
    let desired = snapshot(vec![root.clone()]);
    let studio = snapshot(vec![root.clone(), folder.clone()]);
    let plan = reconciliation_push_plan(&studio, &desired).unwrap();
    assert!(plan.changed_paths.is_empty());
    assert!(plan.target_settings_ids.is_empty());
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);

    let plan = reconciliation_push_plan(&studio, &ProjectSnapshot::default()).unwrap();
    assert!(plan.changed_paths.is_empty());
    assert!(plan.target_settings_ids.is_empty());
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "folder");

    let mut changes = EditorChangeSet::default();
    changes.instance_changes.push(EditorInstanceChange {
        mode: "upsertInstances".to_string(),
        service: "ReplicatedStorage".to_string(),
        allow_deletes: false,
        instances: Vec::new(),
        preserve_instances: Vec::new(),
    });
    amend_reconciled_changes(&mut changes, plan).unwrap();
    assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
    assert_eq!(changes.instance_changes[1].mode, "upsertInstances");

    let package_link = SettingsBytecodeInstance {
        settings_id: "link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(1),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let studio = snapshot(vec![root.clone(), folder.clone(), package_link]);
    let plan = reconciliation_push_plan(&studio, &desired).unwrap();
    assert_eq!(plan.instance_deletes.len(), 1);
    assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "folder");

    let package_link = SettingsBytecodeInstance {
        settings_id: "link".to_string(),
        name: "PackageLink".to_string(),
        class_name: "PackageLink".to_string(),
        parent_index: Some(1),
        properties: Map::new(),
        attributes: Map::new(),
    };
    let studio = snapshot(vec![root.clone(), folder.clone(), package_link]);
    let desired = snapshot(vec![root, folder]);
    assert!(reconciliation_push_plan(&studio, &desired).is_err());
}

#[test]
fn reconciliation_ignores_transient_instance_ids_and_script_guids() {
    let document =
        |root_id: &str, script_id: &str, script_guid: &str, value: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: root_id.to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: script_id.to_string(),
                    name: "Module".to_string(),
                    class_name: "ModuleScript".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([
                        (
                            "ScriptGuid".to_string(),
                            Value::String(script_guid.to_string()),
                        ),
                        ("Value".to_string(), Value::String(value.to_string())),
                    ]),
                    attributes: Map::new(),
                },
            ],
        };
    let snapshot = |document: SettingsBytecode| ProjectSnapshot {
        entries: [(
            PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
        )]
        .into_iter()
        .collect(),
    };
    assert!(
        snapshots_equivalent(
            &snapshot(document("root-a", "script-a", "guid-a", "same")),
            &snapshot(document("root-b", "script-b", "guid-b", "same")),
        )
        .unwrap()
    );

    let baseline = snapshot(document("root-a", "script-a", "guid-a", "base"));
    let editor = snapshot(document("root-a", "script-a", "guid-a", "editor"));
    let studio = snapshot(document("root-b", "script-b", "guid-b", "base"));
    let (merged, conflicts) =
        merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
    assert!(conflicts.is_empty());
    let settings = settings_document(merged.entries.values().next()).unwrap();
    assert_eq!(settings.instances.len(), 2);
    assert_eq!(
        settings.instances[1].properties.get("Value"),
        Some(&Value::String("editor".to_string()))
    );

    let mut editor = document("root-a", "script-a", "guid-a", "same");
    let mut studio = document("root-b", "script-b", "guid-b", "same");
    editor.instances[1].properties.extend([
        (
            "Position".to_string(),
            json!({"_type":"Vector3","x":0.10000000149011612,"y":2.0,"z":3.0}),
        ),
        (
            "WorldPosition".to_string(),
            json!({"_type":"Vector3","x":1.0,"y":2.0,"z":3.0}),
        ),
    ]);
    studio.instances[1].properties.extend([
        (
            "Position".to_string(),
            json!({"_type":"Vector3","x":0.10000000149011613,"y":2.0,"z":3.0}),
        ),
        (
            "WorldPosition".to_string(),
            json!({"_type":"Vector3","x":999.0,"y":2.0,"z":3.0}),
        ),
    ]);
    align_observation_ids_to_baseline(&editor, &mut studio);
    assert!(settings_documents_equivalent(&editor, &studio));

    let reference_document =
        |target_id: &str, holder_id: &str, reference_id: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: target_id.to_string(),
                    name: "Target".to_string(),
                    class_name: "Attachment".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: holder_id.to_string(),
                    name: "Holder".to_string(),
                    class_name: "WeldConstraint".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([(
                        "Attachment0".to_string(),
                        json!({"_type":"Ref","settingsId":reference_id}),
                    )]),
                    attributes: Map::new(),
                },
            ],
        };
    let baseline = reference_document("target", "holder", "target");
    let mut observed = reference_document("debug:target", "debug:holder", "debug:target");
    align_observation_ids_to_baseline(&baseline, &mut observed);
    assert_eq!(observed.instances[1].settings_id, "target");
    assert_eq!(observed.instances[2].settings_id, "holder");
    assert_eq!(
        observed.instances[2]
            .properties
            .get("Attachment0")
            .and_then(|value| value.get("settingsId")),
        Some(&Value::String("target".to_string()))
    );

    let duplicate = |id: &str, marker: &str| SettingsBytecodeInstance {
        settings_id: id.to_string(),
        name: "Duplicate".to_string(),
        class_name: "Folder".to_string(),
        parent_index: Some(0),
        properties: Map::new(),
        attributes: Map::from_iter([("Marker".to_string(), json!(marker))]),
    };
    let baseline = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            duplicate("a", "A"),
            duplicate("b", "B"),
        ],
    };
    let mut observed = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "observed-root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            duplicate("observed-b", "B"),
        ],
    };
    align_observation_ids_to_baseline(&baseline, &mut observed);
    assert_eq!(observed.instances[1].settings_id, "b");

    let baseline = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "debug:0_reserved".to_string(),
                name: "Existing".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
        ],
    };
    let mut observed = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![
            SettingsBytecodeInstance {
                settings_id: "observed-root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "debug:0_reserved".to_string(),
                name: "New".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
            SettingsBytecodeInstance {
                settings_id: "observed-existing".to_string(),
                name: "Existing".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
        ],
    };
    align_observation_ids_to_baseline(&baseline, &mut observed);
    assert_eq!(observed.instances[2].settings_id, "debug:0_reserved");
    assert_ne!(observed.instances[1].settings_id, "debug:0_reserved");
    encode_settings_bytecode(&observed).unwrap();
}
