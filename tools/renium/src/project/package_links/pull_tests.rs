use super::*;
use crate::automation::reconcile::SnapshotEntry;
use crate::snapshot::export::{
    ExportProjectStage, PublishedProjectChanges, collect_publish_hashes, finish_export_publication,
};
use clap::Parser;

const SERVICE: &str = "ReplicatedStorage";

fn prepare_pull(root: &Path, services: &[&str]) -> ExportProjectStage {
    let services = services
        .iter()
        .map(|service| (*service).to_string())
        .collect::<Vec<_>>();
    let mut stage = ExportProjectStage::create(root, Path::new("src"), &services).unwrap();
    stage
        .prepare_read_only_links(root, Path::new("src"), &services)
        .unwrap();
    stage
}

fn finish_pull(root: &Path, stage: ExportProjectStage) -> Result<PublishedProjectChanges> {
    finish_export_publication(Some(stage), root, false, None, || Ok(()), || Ok(()))
        .map(|(published, _)| published)
}

fn write_mirror(root: &Path, text: &str) -> PathBuf {
    let path = SharedFixture::script_file(root, "Config");
    set_path_readonly(&path, false).unwrap();
    fs::write(&path, text).unwrap();
    path
}

#[test]
fn pull_restores_shared_mirror_before_publication_and_keeps_studio_baseline() {
    let fixture = shared_fixture("pull-staged-shared");
    share_public(&fixture, false);
    write_mirror(&fixture.race, "return 'before'");
    let stage = prepare_pull(&fixture.race, &[SERVICE]);
    let staged_mirror = write_mirror(&stage.project_root, "return 'studio'");
    let relative = staged_mirror
        .strip_prefix(&stage.project_root)
        .unwrap()
        .to_path_buf();
    let published = finish_pull(&fixture.race, stage).unwrap();
    assert_eq!(published.read_only_links_restored.len(), 1);
    assert_eq!(published.read_only_links_restored[0]["restored"], true);
    assert_eq!(
        SharedFixture::mirrored_source(&fixture.race, "Config"),
        "return 1"
    );
    let expected = collect_publish_hashes(&fixture.race, std::slice::from_ref(&relative)).unwrap();
    assert!(published.expected[&relative].as_ref() == expected.get(&relative));
    assert!(
        published.studio_overrides[&relative]
            == Some(SnapshotEntry::File(b"return 'studio'".to_vec()))
    );
    let again = finish_pull(&fixture.race, prepare_pull(&fixture.race, &[SERVICE])).unwrap();
    assert!(again.read_only_links_restored.is_empty());
    assert!(again.studio_overrides.is_empty());
    let _ = fs::remove_dir_all(fixture.root);
}

#[test]
fn pull_uses_saved_shared_package_even_when_source_matches_studio() {
    let fixture = shared_fixture("pull-shared-source-ahead");
    share_public(&fixture, false);
    fs::write(&fixture.config_path, "return 2").unwrap();
    let package = fixture.root.join("links/public.renium");
    let original_package = fs::read(&package).unwrap();
    let stage = prepare_pull(&fixture.race, &[SERVICE]);
    let staged = write_mirror(&stage.project_root, "return 2");
    let relative = staged
        .strip_prefix(&stage.project_root)
        .unwrap()
        .to_path_buf();
    let result = finish_pull(&fixture.race, stage).unwrap();
    assert_eq!(result.read_only_links_restored.len(), 1);
    assert_eq!(
        SharedFixture::mirrored_source(&fixture.race, "Config"),
        "return 1"
    );
    assert_eq!(fs::read(package).unwrap(), original_package);
    assert_eq!(
        fs::read_to_string(&fixture.config_path).unwrap(),
        "return 2"
    );
    // Even when the final files equal their pre-pull bytes, Studio is different.
    assert!(result.studio_overrides[&relative] == Some(SnapshotEntry::File(b"return 2".to_vec())));
    let _ = fs::remove_dir_all(fixture.root);
}

#[test]
fn pull_restores_shared_stray_only_drift_without_overwriting_previous_replacements() {
    let fixture = shared_fixture("pull-shared-stray");
    share_public(&fixture, false);
    let old = fixture.race.join(".renium/link-replaced/public/Stray.luau");
    fs::write(&old, "return 'previous'").unwrap();
    let stage = prepare_pull(&fixture.race, &[SERVICE]);
    let stray = PathBuf::from("src/ReplicatedStorage/Public/Stray.luau");
    fs::write(stage.project_root.join(&stray), "return 'studio stray'").unwrap();
    let result = finish_pull(&fixture.race, stage).unwrap();
    assert_eq!(result.read_only_links_restored[0]["replaced"], 1);
    assert!(!fixture.race.join(&stray).exists());
    assert_eq!(fs::read_to_string(&old).unwrap(), "return 'previous'");
    let backups = WalkDir::new(fixture.race.join(".renium/link-replaced/public"))
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect::<Vec<_>>();
    assert!(backups.contains(&"return 'studio stray'".to_string()));
    assert!(
        result.studio_overrides[&stray]
            == Some(SnapshotEntry::File(b"return 'studio stray'".to_vec()))
    );
    let _ = fs::remove_dir_all(fixture.root);
}

fn local_package_fixture(name: &str) -> PathBuf {
    let root = temp_dir(name);
    fs::write(
        root.join("renium.project.jsonc"),
        r#"{"schemaVersion":1,"sourceRoot":"src"}"#,
    )
    .unwrap();
    config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    write_link_test_service(&root.join("src/ReplicatedStorage"));
    fs::create_dir_all(root.join("links")).unwrap();
    let mut script = settings_instance("pkg:config", "Config", "ModuleScript", Some(0));
    script
        .properties
        .insert("Source".into(), json!("return 'package'"));
    settings_document(vec![
        settings_instance("pkg:root", "Pkg", "Folder", None),
        script,
    ])
    .write_file(&root.join("links/pkg.renium"))
    .unwrap();
    write_package_link_manifest(&root, SERVICE, true, &["Pkg"]);
    link_apply(test_link_apply_args(&root)).unwrap();
    root
}

fn local_studio_copy(root: &Path) -> PathBuf {
    let service = root.join("src/ReplicatedStorage");
    local_studio_copy_at(&service)
}

fn local_studio_copy_at(service: &Path) -> PathBuf {
    let store = service_settings_path(service);
    let mut document = SettingsBytecode::read_file(&store).unwrap();
    let index = document
        .instances
        .iter()
        .position(|instance| instance.name == "Config")
        .unwrap();
    document.instances[index]
        .properties
        .insert("Source".into(), json!(EXTERNAL_SOURCE_MARKER));
    let path = build_editor_source_paths_by_index(&document, SERVICE, service)[index]
        .clone()
        .unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    if path.exists() {
        set_path_readonly(&path, false).unwrap();
    }
    fs::write(&path, "return 'studio'").unwrap();
    document.write_file(&store).unwrap();
    path
}

#[test]
fn pull_restores_ordinary_package_and_a_following_push_passes_protection() {
    let root = local_package_fixture("pull-ordinary");
    let stage = prepare_pull(&root, &[SERVICE]);
    let path = local_studio_copy(&stage.project_root);
    let relative = path
        .strip_prefix(&stage.project_root)
        .unwrap()
        .to_path_buf();
    let result = finish_pull(&root, stage).unwrap();
    assert_eq!(result.read_only_links_restored[0]["link"], "pkg");
    assert_eq!(
        fs::read_to_string(root.join(&relative)).unwrap(),
        "return 'package'"
    );
    assert!(
        fs::metadata(root.join(&relative))
            .unwrap()
            .permissions()
            .readonly()
    );
    let enforcement = build_link_enforcement(&root, &root.join("src"), None).unwrap();
    let target = &enforcement.read_only_packages[0];
    let document =
        SettingsBytecode::read_file(&service_settings_path(&root.join("src/ReplicatedStorage")))
            .unwrap();
    assert!(
        read_only_target_current(
            &document,
            SERVICE,
            &root.join("src/ReplicatedStorage"),
            &target.target_segments,
            &target.target_ordinals,
            target.package.as_deref(),
            &target.expected_fingerprint
        )
        .unwrap()
    );
    let changes = collect_editor_changes(&PushEditorChangesArgs {
        project: ProjectSourceArgs {
            project_root: root.clone(),
            src_root: root.join("src"),
        },
        changed_paths: vec![root.join(&relative)],
        ..PushEditorChangesArgs::try_parse_from(["push"]).unwrap()
    });
    assert!(changes.is_ok(), "{}", changes.err().unwrap());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pull_recreates_missing_package_target_and_regenerates_sourcemap() {
    let root = local_package_fixture("pull-missing-package");
    fs::write(root.join("sourcemap.json"), r#"{"name":"game","className":"DataModel","children":[{"name":"ServerStorage","className":"ServerStorage","children":[{"name":"Untouched","className":"Folder"}]}]}"#).unwrap();
    let stage = prepare_pull(&root, &[SERVICE]);
    write_link_test_service(&stage.project_root.join("src/ReplicatedStorage"));
    let result = finish_pull(&root, stage).unwrap();
    assert_eq!(result.read_only_links_restored.len(), 1);
    let map: Value =
        serde_json::from_slice(&fs::read(root.join("sourcemap.json")).unwrap()).unwrap();
    let text = map.to_string();
    assert!(text.contains("Config.luau"), "{text}");
    assert!(text.contains("Untouched"), "{text}");
    assert!(
        read_link_manifest(&root.join("renium-link.json"))
            .unwrap()
            .broken
            .is_empty()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pull_refuses_ordinary_unowned_files_before_publication_or_acknowledgement() {
    let root = local_package_fixture("pull-unowned");
    let store = service_settings_path(&root.join("src/ReplicatedStorage"));
    let before = fs::read(&store).unwrap();
    let lock = fs::read(link_lock_path(&root)).unwrap();
    let stage = prepare_pull(&root, &[SERVICE]);
    local_studio_copy(&stage.project_root);
    fs::write(
        stage
            .project_root
            .join("src/ReplicatedStorage/Pkg/Notes.txt"),
        "mine",
    )
    .unwrap();
    let error = finish_export_publication(
        Some(stage),
        &root,
        false,
        None,
        || Ok(()),
        || panic!("failed restore must not acknowledge the pull"),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("unowned file"), "{error:#}");
    assert_eq!(fs::read(store).unwrap(), before);
    assert_eq!(fs::read(link_lock_path(&root)).unwrap(), lock);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pull_scopes_each_link_target_and_skips_unresolved_unpulled_services() {
    let root = local_package_fixture("pull-service-scope");
    let unrelated = root.join("src/ServerStorage");
    fs::create_dir_all(&unrelated).unwrap();
    let document = settings_document(vec![
        settings_instance("ss", "ServerStorage", "ServerStorage", None),
        settings_instance("other", "Pkg", "Folder", Some(0)),
    ]);
    document
        .write_file(&service_settings_path(&unrelated))
        .unwrap();
    let before = fs::read(service_settings_path(&unrelated)).unwrap();
    let mut manifest = read_link_manifest(&root.join("renium-link.json")).unwrap();
    manifest.links[0].targets.push(LinkTargetRef {
        service: "ServerStorage".into(),
        path: vec!["Pkg".into()],
        ords: Vec::new(),
    });
    manifest.links.push(LinkEntry {
        id: "missing".into(),
        read_only: true,
        source: LinkSource::Local {
            path: "links/missing.renium".into(),
        },
        targets: vec![LinkTargetRef {
            service: "ServerStorage".into(),
            path: vec!["Missing".into()],
            ords: Vec::new(),
        }],
    });
    write_link_manifest(&root.join("renium-link.json"), &manifest).unwrap();
    let stage = prepare_pull(&root, &[SERVICE]);
    local_studio_copy(&stage.project_root);
    let result = finish_pull(&root, stage).unwrap();
    assert_eq!(result.read_only_links_restored.len(), 1);
    assert_eq!(fs::read(service_settings_path(&unrelated)).unwrap(), before);
    assert!(!unrelated.join("Missing").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pull_aborts_when_package_or_project_changes_during_export() {
    for change_package in [true, false] {
        let root = local_package_fixture("pull-concurrent");
        let stage = prepare_pull(&root, &[SERVICE]);
        local_studio_copy(&stage.project_root);
        let changed = if change_package {
            root.join("links/pkg.renium")
        } else {
            service_settings_path(&root.join("src/ReplicatedStorage"))
        };
        let mut document = SettingsBytecode::read_file(&changed).unwrap();
        document.instances[0]
            .attributes
            .insert("Outside".into(), json!(true));
        document.write_file(&changed).unwrap();
        let bytes = fs::read(&changed).unwrap();
        let error = finish_pull(&root, stage).err().unwrap();
        assert!(
            format!("{error:#}").contains("changed during")
                || format!("{error:#}").contains("Project files changed"),
            "{error:#}"
        );
        assert_eq!(fs::read(changed).unwrap(), bytes);
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn push_readback_keeps_an_intentional_override() {
    let root = local_package_fixture("pull-override-readback");
    // Push verification uses this unmodified export stage; only a pull opts
    // into restoration. Otherwise --override-packages would undo its own push.
    let stage = ExportProjectStage::create(&root, Path::new("src"), &[SERVICE.into()]).unwrap();
    let path = local_studio_copy(&stage.project_root);
    let relative = path
        .strip_prefix(&stage.project_root)
        .unwrap()
        .to_path_buf();
    let result = finish_pull(&root, stage).unwrap();
    assert!(result.read_only_links_restored.is_empty());
    assert!(result.studio_overrides.is_empty());
    assert_eq!(
        fs::read_to_string(root.join(&relative)).unwrap(),
        "return 'studio'"
    );
    let mut push = PushEditorChangesArgs {
        project: ProjectSourceArgs {
            project_root: root.clone(),
            src_root: PathBuf::from("src"),
        },
        changed_paths: vec![root.join(&relative)],
        ..PushEditorChangesArgs::try_parse_from(["push"]).unwrap()
    };
    assert!(collect_editor_changes(&push).is_err());
    push.override_packages = true;
    assert!(collect_editor_changes(&push).is_ok());
    let pulled = finish_pull(&root, prepare_pull(&root, &[SERVICE])).unwrap();
    assert_eq!(pulled.read_only_links_restored.len(), 1);
    assert_eq!(
        fs::read_to_string(root.join(relative)).unwrap(),
        "return 'package'"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pull_cannot_remove_an_identity_referenced_by_an_unpulled_service() {
    let root = local_package_fixture("pull-unpulled-reference");
    let store = service_settings_path(&root.join("src/ReplicatedStorage"));
    let mut document = SettingsBytecode::read_file(&store).unwrap();
    document.instances.push(settings_instance(
        "external-target",
        "StudioOnly",
        "Part",
        Some(1),
    ));
    document.write_file(&store).unwrap();
    let mut value = settings_instance("reference", "Reference", "ObjectValue", Some(0));
    value.properties.insert("Value".into(), json!({"_type":"Ref","settingsId":"external-target","pathSegments":["ReplicatedStorage","Pkg","StudioOnly"],"pathOrdinals":[1,1,1]}));
    settings_document(vec![
        settings_instance("ss", "ServerStorage", "ServerStorage", None),
        value,
    ])
    .write_file(&root.join("instances/ServerStorage.renium"))
    .unwrap();
    let before = fs::read(&store).unwrap();
    let stage = prepare_pull(&root, &[SERVICE]);
    let error = finish_pull(&root, stage)
        .err()
        .expect("a dangling external reference must abort the pull");
    assert!(format!("{error:#}").contains("referenc"), "{error:#}");
    assert_eq!(fs::read(store).unwrap(), before);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pull_restores_into_custom_source_roots_and_projected_owners() {
    for mapped in [false, true] {
        let root = local_package_fixture("pull-layout");
        fs::create_dir_all(root.join("code")).unwrap();
        fs::rename(
            root.join("src/ReplicatedStorage"),
            root.join("code/ReplicatedStorage"),
        )
        .unwrap();
        let configuration = if mapped {
            json!({"schemaVersion":1,"sourceRoot":"src","tree":{"ReplicatedStorage":{"$path":"code/ReplicatedStorage"}}})
        } else {
            json!({"schemaVersion":1,"sourceRoot":"code"})
        };
        fs::write(
            root.join("renium.project.jsonc"),
            serde_json::to_vec(&configuration).unwrap(),
        )
        .unwrap();
        config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
        let source = Path::new(if mapped { "src" } else { "code" });
        let mut stage = ExportProjectStage::create(&root, source, &[SERVICE.into()]).unwrap();
        stage
            .prepare_read_only_links(&root, source, &[SERVICE.into()])
            .unwrap();
        local_studio_copy_at(
            &stage
                .import_project_root
                .join(&stage.import_src_dir)
                .join(SERVICE),
        );
        let result = finish_pull(&root, stage).unwrap();
        assert_eq!(result.read_only_links_restored.len(), 1);
        let restored = PathBuf::from("code/ReplicatedStorage/Pkg/Config.luau");
        assert_eq!(
            fs::read_to_string(root.join(&restored)).unwrap(),
            "return 'package'"
        );
        assert!(result.expected.contains_key(&restored));
        let map = fs::read_to_string(root.join("sourcemap.json")).unwrap();
        assert!(
            map.contains("code/ReplicatedStorage/Pkg/Config.luau"),
            "{map}"
        );
        assert!(!root.join("src/ReplicatedStorage/Pkg/Config.luau").exists());
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn pull_leaves_writable_and_broken_links_at_the_studio_version() {
    for broken in [false, true] {
        let root = local_package_fixture("pull-unprotected");
        let path = root.join("renium-link.json");
        let mut manifest = read_link_manifest(&path).unwrap();
        if broken {
            manifest.broken.push(manifest.links[0].targets[0].clone());
        } else {
            manifest.links[0].read_only = false;
        }
        write_link_manifest(&path, &manifest).unwrap();
        let stage = prepare_pull(&root, &[SERVICE]);
        let mirror = local_studio_copy(&stage.project_root);
        let relative = mirror
            .strip_prefix(&stage.project_root)
            .unwrap()
            .to_path_buf();
        let result = finish_pull(&root, stage).unwrap();
        assert!(result.read_only_links_restored.is_empty());
        assert!(result.studio_overrides.is_empty());
        assert_eq!(
            fs::read_to_string(root.join(relative)).unwrap(),
            "return 'studio'"
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn pull_recreates_a_missing_service_store_from_the_package() {
    let root = local_package_fixture("pull-missing-store");
    let stage = prepare_pull(&root, &[SERVICE]);
    fs::remove_file(service_settings_path(
        &stage.project_root.join("src/ReplicatedStorage"),
    ))
    .unwrap();
    let result = finish_pull(&root, stage).unwrap();
    assert_eq!(result.read_only_links_restored.len(), 1);
    let store = service_settings_path(&root.join("src/ReplicatedStorage"));
    let document = SettingsBytecode::read_file(&store).unwrap();
    assert!(
        document
            .instances
            .iter()
            .any(|instance| instance.name == "Config")
    );
    assert!(result.studio_overrides[store.strip_prefix(&root).unwrap()].is_none());
    let _ = fs::remove_dir_all(root);
}
