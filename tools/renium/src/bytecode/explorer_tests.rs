use super::*;
use clap::Parser;

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
