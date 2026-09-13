use super::*;
use clap::Parser;

#[test]
fn batch_paths_accept_cli_strings_and_lossless_segments() {
    assert_eq!(
        parse_bytecode_explorer_batch_ops("\u{feff}{\"ops\":[{\"type\":\"search\"}]}")
            .unwrap()
            .len(),
        1
    );
    for key in ["path", "pathSegments", "path_segments"] {
        for path in [
            json!("Workspace.Lobby.Barrier"),
            json!(["Workspace", "Lobby", "Barrier"]),
        ] {
            let op = json!({"type":"search", key:path});
            for payload in [json!({"ops":[op.clone()]}), json!([op])] {
                let ops = parse_bytecode_explorer_batch_ops(&payload.to_string()).unwrap();
                assert_eq!(
                    ops[0].path_segments.as_deref().unwrap(),
                    ["Workspace", "Lobby", "Barrier"]
                );
            }
        }
    }
    let ops = parse_bytecode_explorer_batch_ops(
        r#"[{"type":"search","path":["Workspace","Name.with/slashes","雪"]}]"#,
    )
    .unwrap();
    assert_eq!(
        ops[0].path_segments.as_deref().unwrap(),
        ["Workspace", "Name.with/slashes", "雪"]
    );
    for (raw, detail) in [
        (
            r#"{"ops":[{"type":"search","path":42}]}"#,
            "path must be a string",
        ),
        (
            r#"{"ops":[{"type":"search","path":["Workspace",42]}]}"#,
            "path must contain only string segments",
        ),
        (
            r#"{"ops":[{"type":"search","path":" "}]}"#,
            "Path target cannot be empty",
        ),
        (
            r#"{"ops":[{"type":"search","classNmae":"Part"}]}"#,
            "unknown field `classNmae`",
        ),
        (r#"{"ops":[{"path":"Workspace"}]}"#, "missing field `type`"),
        (r#"{"ops":false}"#, "invalid type: boolean"),
    ] {
        let error = parse_bytecode_explorer_batch_ops(raw).err().unwrap();
        assert!(format!("{error:#}").contains(detail), "{error:#}");
        assert!(
            !format!("{error:#}").contains("invalid type: map"),
            "{error:#}"
        );
    }
}

#[test]
fn empty_batch_search_lists_only_selected_subtree_with_fields_and_limit() {
    let (root, _) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let file = service_settings_path(&root.join("Workspace"));
    for (path, limit, expected_matches) in [
        (json!("Workspace.Folder10"), 20, 2),
        (json!(["Workspace", "Folder10"]), 1, 1),
        (json!("Folder10"), 0, 2),
    ] {
        let payload = json!({"ops":[{"type":"search", "path":path, "limit":limit,
            "fields":"lookup,prop:Archivable"}]})
        .to_string();
        let args = BytecodeExplorerBatchArgs::try_parse_from([
            "bb",
            "-f",
            file.to_str().unwrap(),
            "-s",
            "Workspace",
            "-j",
            &payload,
            "-o",
            "full",
        ])
        .unwrap();
        let result = bytecode_explorer_batch_result(args).unwrap();
        let result = &result["results"][0];
        assert_eq!(result["truncated"], limit == 1);
        assert_eq!(
            result["matchIds"].as_array().unwrap().len(),
            expected_matches
        );
        for node in result["nodes"].as_array().unwrap() {
            assert!(matches!(
                node["settingsId"].as_str().unwrap(),
                "folder-10" | "script-10"
            ));
            assert_eq!(node["properties"]["Archivable"], true, "{node}");
        }
    }
}

#[test]
fn batch_find_reports_truncation_only_when_more_matches_exist() {
    let (root, _) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let file = service_settings_path(&root.join("Workspace"));
    for (limit, expected, truncated) in [(0, 1000, false), (999, 999, true), (1000, 1000, false)] {
        let payload = json!([{"type":"find", "class":"Folder", "limit":limit}]).to_string();
        let result = bytecode_explorer_batch_result(
            BytecodeExplorerBatchArgs::try_parse_from([
                "bb",
                "-f",
                file.to_str().unwrap(),
                "-s",
                "Workspace",
                "-j",
                &payload,
                "-o",
                "full",
            ])
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["results"][0]["matches"].as_array().unwrap().len(),
            expected
        );
        assert_eq!(result["results"][0]["truncated"], truncated);
    }
}

#[test]
fn temporary_import_roots_never_become_explorer_services() {
    let (root, _) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let stage = ".replicatedstorage.25104-5.renium-import";
    for name in [
        stage,
        ".serverstorage.25104-4.renium-import",
        ".workspace.25104-6.renium-import",
        ".custom",
        "user.renium-import",
    ] {
        fs::create_dir(root.join(name)).unwrap();
    }
    let services = explorer_daemon_services(&root, "").unwrap();
    assert!(!services.iter().any(|name| is_import_stage_name(name)));
    assert!(services.iter().any(|name| name == ".custom"));
    assert!(services.iter().any(|name| name == "user.renium-import"));
    let mut state = ExplorerDaemonState::new(
        ExplorerDaemonArgs::try_parse_from([
            "ed",
            "--project-root",
            root.to_str().unwrap(),
            "--src",
            root.to_str().unwrap(),
        ])
        .unwrap(),
    )
    .unwrap();
    state.services.push(stage.into());
    state
        .service_states
        .insert(stage.into(), ExplorerServiceState::empty(stage));
    fs::remove_dir(root.join(stage)).unwrap();
    state.reload_services(&[stage.to_string()]).unwrap();
    assert!(!state.services.iter().any(|name| is_import_stage_name(name)));
    assert!(!state.service_states.contains_key(stage));
}

#[test]
fn corrupt_store_reload_reports_error_and_preserves_last_good_view() {
    let (root, _) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    fs::create_dir(root.join("src")).unwrap();
    fs::rename(root.join("Workspace"), root.join("src/Workspace")).unwrap();
    fs::write(
        root.join("renium.project.jsonc"),
        r#"{"schemaVersion":1,"sourceRoot":"src"}"#,
    )
    .unwrap();
    let mut state = ExplorerDaemonState::new(
        ExplorerDaemonArgs::try_parse_from([
            "ed",
            "--project-root",
            root.to_str().unwrap(),
            "--src",
            "src",
        ])
        .unwrap(),
    )
    .unwrap();
    state.initialize().unwrap();
    let before = state.service_states["Workspace"]
        .document
        .as_ref()
        .unwrap()
        .instances
        .len();
    let version = state.snapshot_version;
    let file = service_settings_path(&root.join("src/Workspace"));
    let valid = fs::read(&file).unwrap();
    fs::write(&file, b"truncated").unwrap();
    assert!(ExplorerServiceState::load(&state.src_root, "Workspace").is_err());
    let error = state.reload_services(&["Workspace".into()]).unwrap_err();
    assert!(format!("{error:#}").contains("Workspace"), "{error:#}");
    assert_eq!(state.snapshot_version, version);
    assert_eq!(
        state.service_states["Workspace"]
            .document
            .as_ref()
            .unwrap()
            .instances
            .len(),
        before
    );
    fs::write(&file, valid).unwrap();
    state.reload_services(&["Workspace".into()]).unwrap();
    assert!(state.snapshot_version > version);
}

fn fixture() -> (PathBuf, SettingsBytecode) {
    let root = crate::system::files::create_unique_directory(
        &std::env::temp_dir(),
        "renium-explorer-test-",
    )
    .unwrap();
    let service_dir = root.join("Workspace");
    fs::create_dir_all(&service_dir).unwrap();
    let mut instances = vec![SettingsBytecodeInstance::new(
        "root".into(),
        "Workspace".into(),
        "Workspace".into(),
        None,
    )];
    for n in 0..1000 {
        let parent = instances.len();
        instances.push(SettingsBytecodeInstance::new(
            format!("folder-{n}"),
            format!("Folder{n}"),
            "Folder".into(),
            Some(0),
        ));
        instances.push(SettingsBytecodeInstance::new(
            format!("script-{n}"),
            "Code".into(),
            "ModuleScript".into(),
            Some(parent),
        ));
    }
    let document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    };
    document
        .write_file(&service_settings_path(&service_dir))
        .unwrap();
    (root, document)
}

#[test]
fn subtree_source_query_is_scoped_and_counts_remain_compatible() {
    let (root, _) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let file = service_settings_path(&root.join("Workspace"));
    let ops = r#"[{"type":"sources","id":"folder-10"},{"type":"counts"}]"#;
    let args = BytecodeExplorerBatchArgs::try_parse_from([
        "bb",
        "-f",
        file.to_str().unwrap(),
        "-s",
        "Workspace",
        "-j",
        ops,
        "-o",
        "full",
    ])
    .unwrap();
    let result = bytecode_explorer_batch_result(args).unwrap();
    let sources = result["results"][0]["sourcePaths"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert!(sources[0].as_str().unwrap().contains("Folder10"));
    assert_eq!(result["results"][1]["descendants"], 2000);
}

#[test]
fn lazy_indexes_preserve_source_references_and_metadata_aliases() {
    let (root, mut document) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let service_dir = root.join("Workspace");
    let file = service_settings_path(&service_dir);
    let sources = build_editor_source_paths_by_index(&document, "Workspace", &service_dir);
    let script = sources[2].as_ref().unwrap();
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(script, "return 123").unwrap();
    document.instances[2]
        .properties
        .insert("Target".into(), json!({"_type":"Ref", "instanceIndex":2}));
    document.write_file(&file).unwrap();
    let args = BytecodeExplorerBatchArgs::try_parse_from([
        "bb", "-f", file.to_str().unwrap(), "-s", "Workspace", "-j",
        r#"[{"type":"instance","id":"script-0","fields":"Source,Target,f,canonical","output":"compact"}]"#,
        "-o", "full",
    ]).unwrap();
    let result = bytecode_explorer_batch_result(args).unwrap();
    let node = &result["results"][0];
    assert_eq!(node["props"]["Source"], "return 123");
    assert_eq!(node["props"]["Target"]["settingsId"], "folder-0");
    assert_eq!(
        node["props"]["Target"]["pathSegments"],
        json!(["Workspace", "Folder0"])
    );
    assert_eq!(node["canonicalSettingsId"], "script-0");
    assert_eq!(node["settingsFile"], json!(file));
}

#[test]
fn incremental_rows_match_full_rebuild_for_both_modes() {
    let (root, _) = fixture();
    let _cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let mut state = ExplorerDaemonState {
        services: vec!["Workspace".into()],
        service_states: HashMap::from([(
            "Workspace".into(),
            ExplorerServiceState::load(&root, "Workspace").unwrap(),
        )]),
        ..Default::default()
    };
    state.rebuild_rows(ExplorerViewMode::Normal);
    state.start_search(1, 1, "o");
    for mode in [ExplorerViewMode::Normal, ExplorerViewMode::Search] {
        for id in [
            "service:Workspace",
            "Workspace:folder-500",
            "Workspace:folder-0",
            "service:Workspace",
            "Workspace:folder-999",
        ] {
            state.expand(id, mode);
            assert_eq!(state.rows(mode), state.build_rows(mode));
            let version = state.view_version;
            state.expand(id, mode);
            assert_eq!(state.view_version, version);
            state.collapse(id, mode);
            assert_eq!(state.rows(mode), state.build_rows(mode));
            let version = state.view_version;
            state.collapse(id, mode);
            assert_eq!(state.view_version, version);
        }
    }
}
