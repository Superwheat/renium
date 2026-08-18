use super::*;
use crate::cli::{
    LinkAddArgs, LinkApplyArgs, LinkBreakArgs, LinkDeletePackageArgs, LinkMoveTargetArgs,
    LinkPackArgs, LinkTargetArgs, ProjectSourceArgs,
};
use crate::editor::document::read_editor_service_settings;
use crate::tests::support::{settings_document, settings_instance, temp_dir};

fn test_link_apply_args(dir: &Path) -> LinkApplyArgs {
    LinkApplyArgs {
        project: ProjectSourceArgs {
            project_root: dir.to_path_buf(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        link: None,
        check: false,
        force_targets: false,
        force_target: Vec::new(),
        offline: true,
        strict: false,
        git_path: "git".into(),
        wally_path: "wally".into(),
        cache_dir: None,
        pretty: false,
    }
}

fn link_target_args(service: &str, path: &str) -> LinkTargetArgs {
    LinkTargetArgs {
        service: service.into(),
        path_segments_json: path.into(),
        path_ordinals_json: "[]".into(),
        writable: false,
    }
}

fn write_link_test_service(service_dir: &Path) {
    fs::create_dir_all(service_dir).unwrap();
    settings_document(vec![settings_instance(
        "root",
        "ReplicatedStorage",
        "ReplicatedStorage",
        None,
    )])
    .write_file(&service_settings_path(service_dir))
    .unwrap();
}

fn write_single_file_link_manifest(dir: &Path, read_only: bool) {
    write_link_manifest(
        &dir.join("renium-link.json"),
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![LinkEntry {
                id: "logger".into(),
                read_only,
                source: LinkSource::Local {
                    path: "links/Logger.luau".into(),
                },
                targets: vec![LinkTargetRef {
                    service: "ReplicatedStorage".into(),
                    path: vec!["ReplicatedStorage".into(), "Logger".into()],
                    ords: Vec::new(),
                }],
            }],
            broken: Vec::new(),
        },
    )
    .unwrap();
}

fn write_package_link_manifest(dir: &Path, service: &str, read_only: bool, targets: &[&str]) {
    write_link_manifest(
        &dir.join("renium-link.json"),
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![LinkEntry {
                id: "pkg".into(),
                read_only,
                source: LinkSource::Local {
                    path: "links/pkg.renium".into(),
                },
                targets: targets
                    .iter()
                    .map(|target| LinkTargetRef {
                        service: service.into(),
                        path: vec![service.into(), (*target).into()],
                        ords: Vec::new(),
                    })
                    .collect(),
            }],
            broken: Vec::new(),
        },
    )
    .unwrap();
}

fn write_script_package_settings(service_dir: &Path, source: &str) -> PathBuf {
    fs::create_dir_all(service_dir).unwrap();
    let settings_path = service_settings_path(service_dir);
    settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("pkg", "Pkg", "Folder", Some(0)),
        SettingsBytecodeInstance {
            settings_id: "script".into(),
            name: "Child".into(),
            class_name: "Script".into(),
            parent_index: Some(1),
            properties: Map::from_iter([("Source".to_string(), Value::String(source.to_string()))]),
            attributes: Map::new(),
        },
    ])
    .write_file(&settings_path)
    .unwrap();
    settings_path
}

fn setup_package_delete(name: &str, source: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let service_dir = dir.join("src").join("ReplicatedStorage");
    write_script_package_settings(&service_dir, source);
    let package_dir = dir.join("links");
    fs::create_dir_all(&package_dir).unwrap();
    settings_document(Vec::new())
        .write_file(&package_dir.join("pkg.renium"))
        .unwrap();
    write_package_link_manifest(&dir, "ReplicatedStorage", true, &["Pkg"]);
    let mut lock = LinkLock {
        version: LINK_MANIFEST_VERSION,
        ..Default::default()
    };
    lock.entries.insert("pkg".into(), LinkLockEntry::default());
    write_link_lock(&dir, &lock).unwrap();
    (dir, service_dir)
}

fn delete_test_package(dir: &Path, action: &str) {
    link_delete_package(LinkDeletePackageArgs {
        project: ProjectSourceArgs {
            project_root: dir.to_path_buf(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        id: "pkg".into(),
        action: action.into(),
        pretty: false,
    })
    .unwrap();
}

fn setup_locked_missing_package_targets(name: &str, targets: &[&str]) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let src_root = dir.join("src");
    write_link_test_service(&src_root.join("ReplicatedStorage"));

    let package_dir = dir.join("links");
    fs::create_dir_all(&package_dir).unwrap();
    let package_path = package_dir.join("pkg.renium");
    settings_document(vec![settings_instance("pkg:0", "Pkg", "Folder", None)])
        .write_file(&package_path)
        .unwrap();
    write_package_link_manifest(&dir, "ReplicatedStorage", true, targets);

    let package_hash = fs::read(&package_path)
        .map(|bytes| fnv1a_hex(&bytes))
        .unwrap();
    let mut lock = LinkLock {
        version: LINK_MANIFEST_VERSION,
        ..Default::default()
    };
    let files = &mut lock.entries.entry("pkg".into()).or_default().files;
    for target in targets {
        files.insert(
            package_lock_key(
                "package",
                "ReplicatedStorage",
                &[(*target).to_string()],
                &[1],
            ),
            package_hash.clone(),
        );
    }
    write_link_lock(&dir, &lock).unwrap();
    (dir, src_root)
}

fn write_link_lock(project_root: &Path, lock: &LinkLock) -> Result<()> {
    let path = link_lock_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(lock)? + "\n";
    write_utf8_file(&path, &content)
}

fn link_target_dir_and_leaf_at(
    target_path: &Path,
    containment_root: &Path,
    target: &LinkTargetRef,
) -> Result<(PathBuf, String)> {
    let segments = validate_filesystem_link_target_ref(target)?;
    let leaf = segments.last().cloned().unwrap();
    let dir = target_path
        .parent()
        .context("Link target has no parent directory")?
        .to_path_buf();
    ensure_existing_ancestor_inside(containment_root, &dir.join(&leaf), "link target")?;
    Ok((dir, leaf))
}

fn link_target_dir_and_leaf(src_root: &Path, target: &LinkTargetRef) -> Result<(PathBuf, String)> {
    let segments = validate_filesystem_link_target_ref(target)?;
    let target_path = src_root
        .join(&target.service)
        .join(segments.iter().collect::<PathBuf>());
    link_target_dir_and_leaf_at(&target_path, src_root, target)
}

fn link_target_file_pairs(
    src_root: &Path,
    target: &LinkTargetRef,
    source_root: &Path,
    source_is_dir: bool,
    naming: &config::ProjectScriptNaming,
) -> Result<Vec<LinkFilePair>> {
    let (parent_dir, leaf) = link_target_dir_and_leaf(src_root, target)?;
    link_target_file_pairs_at(
        target,
        source_root,
        source_is_dir,
        naming,
        &parent_dir.join(&leaf),
        src_root,
        false,
    )
}

fn resolve_editor_instance_by_path(
    document: &SettingsBytecode,
    service: &str,
    segments_after_service: &[String],
) -> Option<usize> {
    resolve_editor_instance_by_path_ordinals(document, service, segments_after_service, &[])
}

#[test]
fn link_add_unbreaks_matching_target() {
    let dir = temp_dir("link-add-unbreaks");
    let manifest_path = dir.join("renium-link.json");
    fs::write(
        &manifest_path,
        r#"{
                "version":1,
                "links":[{
                    "id":"pkg",
                    "source":{"type":"local","path":"links/pkg.renium"},
                    "targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Pkg"]}]
                }],
                "broken":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Pkg"]}]
            }"#,
    )
    .unwrap();

    link_add(LinkAddArgs {
        project_root: dir.clone(),
        manifest: PathBuf::from("renium-link.json"),
        id: Some("pkg".into()),
        source_type: "local".into(),
        source: None,
        source_ref: None,
        source_subpath: None,
        target: link_target_args("ReplicatedStorage", r#"["ReplicatedStorage","Pkg"]"#),
        pretty: false,
    })
    .unwrap();

    let manifest = read_link_manifest(&manifest_path).unwrap();
    assert_eq!(manifest.links[0].targets.len(), 1);
    assert!(manifest.broken.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_add_rejects_target_owned_by_another_link() {
    let dir = temp_dir("link-add-target-collision");
    let manifest_path = dir.join("renium-link.json");
    write_link_manifest(
        &manifest_path,
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![LinkEntry {
                id: "country-a".into(),
                read_only: true,
                source: LinkSource::Local {
                    path: "links/a.renium".into(),
                },
                targets: vec![LinkTargetRef {
                    service: "StarterGui".into(),
                    path: vec!["StarterGui".into(), "CountryService".into()],
                    ords: Vec::new(),
                }],
            }],
            broken: Vec::new(),
        },
    )
    .unwrap();

    let error = link_add(LinkAddArgs {
        project_root: dir.clone(),
        manifest: PathBuf::from("renium-link.json"),
        id: Some("country-b".into()),
        source_type: "local".into(),
        source: Some("links/b.renium".into()),
        source_ref: None,
        source_subpath: None,
        target: link_target_args("StarterGui", r#"["StarterGui","CountryService"]"#),
        pretty: false,
    })
    .unwrap_err()
    .to_string();

    assert!(error.contains("already a renium-link target"));
    assert_eq!(read_link_manifest(&manifest_path).unwrap().links.len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_move_target_rejects_target_collision() {
    let dir = temp_dir("link-move-target-collision");
    let manifest_path = dir.join("renium-link.json");
    write_link_manifest(
        &manifest_path,
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![
                LinkEntry {
                    id: "old-country".into(),
                    read_only: true,
                    source: LinkSource::Local {
                        path: "links/old.renium".into(),
                    },
                    targets: vec![LinkTargetRef {
                        service: "StarterGui".into(),
                        path: vec!["StarterGui".into(), "OldCountryService".into()],
                        ords: Vec::new(),
                    }],
                },
                LinkEntry {
                    id: "country".into(),
                    read_only: true,
                    source: LinkSource::Local {
                        path: "links/country.renium".into(),
                    },
                    targets: vec![LinkTargetRef {
                        service: "StarterGui".into(),
                        path: vec!["StarterGui".into(), "CountryService".into()],
                        ords: Vec::new(),
                    }],
                },
            ],
            broken: Vec::new(),
        },
    )
    .unwrap();

    let error = link_move_target(LinkMoveTargetArgs {
        project_root: dir.clone(),
        manifest: PathBuf::from("renium-link.json"),
        old_service: "StarterGui".into(),
        old_path_segments_json: r#"["StarterGui","OldCountryService"]"#.into(),
        old_path_ordinals_json: "[]".into(),
        new_service: "StarterGui".into(),
        new_path_segments_json: r#"["StarterGui","CountryService"]"#.into(),
        new_path_ordinals_json: "[]".into(),
        pretty: false,
    })
    .unwrap_err()
    .to_string();

    assert!(error.contains("already a renium-link target"));
    let manifest = read_link_manifest(&manifest_path).unwrap();
    assert_eq!(
        link_target_segments(&manifest.links[0].targets[0]),
        vec!["OldCountryService".to_string()]
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_pack_resaves_existing_package_source_path() {
    let dir = temp_dir("link-pack-resave-existing");
    let src_root = dir.join("src");
    let service_dir = src_root.join("ReplicatedStorage");
    fs::create_dir_all(&service_dir).unwrap();
    let settings_path = service_dir.join("__roblox_sync_settings.renium");
    settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("pkg", "Pkg", "Folder", Some(0)),
    ])
    .write_file(&settings_path)
    .unwrap();
    let manifest_path = dir.join("renium-link.json");
    write_link_manifest(
        &manifest_path,
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![LinkEntry {
                id: "country".into(),
                read_only: true,
                source: LinkSource::Local {
                    path: "packages/current.renium".into(),
                },
                targets: vec![LinkTargetRef {
                    service: "ReplicatedStorage".into(),
                    path: vec!["ReplicatedStorage".into(), "Pkg".into()],
                    ords: Vec::new(),
                }],
            }],
            broken: Vec::new(),
        },
    )
    .unwrap();

    link_pack(LinkPackArgs {
        project: ProjectSourceArgs {
            project_root: dir.clone(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        link_folder: Some(PathBuf::from("links")),
        id: Some("country".into()),
        target: link_target_args("ReplicatedStorage", r#"["ReplicatedStorage","Pkg"]"#),
        pretty: false,
    })
    .unwrap();

    assert!(dir.join("packages").join("current.renium").exists());
    assert!(!dir.join("links").join("country.renium").exists());
    let manifest = read_link_manifest(&manifest_path).unwrap();
    assert_eq!(manifest.links.len(), 1);
    assert_eq!(manifest.links[0].targets.len(), 1);
    match &manifest.links[0].source {
        LinkSource::Local { path } => assert_eq!(path, "packages/current.renium"),
        _ => panic!("expected local package source"),
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_pack_without_folder_saves_to_global_library() {
    let dir = temp_dir("link-pack-global");
    let global_dir = dir.join("global-packages");
    unsafe {
        std::env::set_var("RENIUM_GLOBAL_PACKAGES_DIR", &global_dir);
    }
    let src_root = dir.join("src");
    let service_dir = src_root.join("ReplicatedStorage");
    fs::create_dir_all(&service_dir).unwrap();
    settings_document(vec![
        settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("pkg", "Pkg", "Folder", Some(0)),
    ])
    .write_file(&service_settings_path(&service_dir))
    .unwrap();

    link_pack(LinkPackArgs {
        project: ProjectSourceArgs {
            project_root: dir.clone(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        link_folder: None,
        id: Some("pkg".into()),
        target: link_target_args("ReplicatedStorage", r#"["ReplicatedStorage","Pkg"]"#),
        pretty: false,
    })
    .unwrap();

    assert!(global_dir.join("pkg.renium").exists());
    assert!(!dir.join("links").exists());
    let manifest = read_link_manifest(&dir.join("renium-link.json")).unwrap();
    match &manifest.links[0].source {
        LinkSource::Local { path } => assert_eq!(path, "~global/pkg.renium"),
        _ => panic!("expected local package source"),
    }

    link_add(LinkAddArgs {
        project_root: dir.clone(),
        manifest: PathBuf::from("renium-link.json"),
        id: Some("pkg".into()),
        source_type: "local".into(),
        source: None,
        source_ref: None,
        source_subpath: None,
        target: link_target_args("ReplicatedStorage", r#"["ReplicatedStorage","PkgCopy"]"#),
        pretty: false,
    })
    .unwrap();
    let mut apply_args = test_link_apply_args(&dir);
    apply_args.link = Some("pkg".into());
    link_apply(apply_args).unwrap();
    let document = read_editor_service_settings(&src_root, "ReplicatedStorage")
        .unwrap()
        .unwrap();
    assert!(
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["PkgCopy".to_string()])
            .is_some()
    );
    unsafe {
        std::env::remove_var("RENIUM_GLOBAL_PACKAGES_DIR");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_pack_inlines_source_and_removes_disk_mirrors() {
    let dir = temp_dir("link-pack-cleanup");
    let src_root = dir.join("src");
    let service_dir = src_root.join("ReplicatedStorage");
    fs::create_dir_all(service_dir.join("Pkg")).unwrap();
    let settings_path = write_script_package_settings(&service_dir, "__SOURCE_EXTERNAL__");
    let source_path = service_dir.join("Pkg").join("Child.server.luau");
    fs::write(&source_path, "print('packed')").unwrap();

    link_pack(LinkPackArgs {
        project: ProjectSourceArgs {
            project_root: dir.clone(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        link_folder: Some(PathBuf::from("links")),
        id: None,
        target: link_target_args("ReplicatedStorage", r#"["ReplicatedStorage","Pkg"]"#),
        pretty: false,
    })
    .unwrap();

    assert!(!source_path.exists());
    assert!(!service_dir.join("Pkg").exists());

    let updated = SettingsBytecode::read_file(&settings_path).unwrap();
    let script = updated
        .instances
        .iter()
        .find(|instance| instance.settings_id == "script")
        .unwrap();
    assert_eq!(
        script.properties.get("Source"),
        Some(&json!("print('packed')"))
    );

    let package = SettingsBytecode::read_file(&dir.join("links").join("pkg.renium")).unwrap();
    let packaged_script = package
        .instances
        .iter()
        .find(|instance| instance.name == "Child")
        .unwrap();
    assert_eq!(
        packaged_script.properties.get("Source"),
        Some(&json!("print('packed')"))
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_delete_package_unlinks_uses_and_externalizes_sources() {
    let (dir, service_dir) = setup_package_delete("link-delete-unlink", "print('kept')");
    let settings_path = service_settings_path(&service_dir);
    let package_path = dir.join("links").join("pkg.renium");
    let manifest_path = dir.join("renium-link.json");
    delete_test_package(&dir, "unlink-uses");

    assert!(!package_path.exists());
    assert!(!manifest_path.exists());
    assert!(!link_lock_path(&dir).exists());
    let source_path = service_dir.join("Pkg").join("Child.server.luau");
    assert_eq!(fs::read_to_string(&source_path).unwrap(), "print('kept')");
    let updated = SettingsBytecode::read_file(&settings_path).unwrap();
    let script = updated
        .instances
        .iter()
        .find(|instance| instance.settings_id == "script")
        .unwrap();
    assert_eq!(
        script.properties.get("Source"),
        Some(&json!("__SOURCE_EXTERNAL__"))
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_delete_package_deletes_uses_and_package_file() {
    let (dir, service_dir) = setup_package_delete("link-delete-uses", "print('delete')");
    let settings_path = service_settings_path(&service_dir);
    let package_path = dir.join("links").join("pkg.renium");
    let manifest_path = dir.join("renium-link.json");
    delete_test_package(&dir, "delete-uses");

    assert!(!package_path.exists());
    assert!(!manifest_path.exists());
    assert!(!link_lock_path(&dir).exists());
    let updated = SettingsBytecode::read_file(&settings_path).unwrap();
    assert_eq!(updated.instances.len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_apply_breaks_previously_applied_missing_package_target() {
    let (dir, src_root) = setup_locked_missing_package_targets("apply-missing-package", &["Pkg"]);
    let manifest_path = dir.join("renium-link.json");

    link_apply(test_link_apply_args(&dir)).unwrap();

    let manifest = read_link_manifest(&manifest_path).unwrap();
    assert_eq!(manifest.broken.len(), 1);
    assert_eq!(
        link_target_ref_key(&manifest.broken[0]),
        "ReplicatedStorage\u{1}Pkg\u{1}1"
    );
    let document = read_editor_service_settings(&src_root, "ReplicatedStorage")
        .unwrap()
        .unwrap();
    assert!(
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["Pkg".to_string()],)
            .is_none()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_apply_force_targets_recreates_missing_locked_package_target() {
    let (dir, src_root) =
        setup_locked_missing_package_targets("apply-force-missing-package", &["Pkg"]);
    let manifest_path = dir.join("renium-link.json");

    let mut args = test_link_apply_args(&dir);
    args.link = Some("pkg".into());
    args.force_targets = true;
    link_apply(args).unwrap();

    assert!(
        read_link_manifest(&manifest_path)
            .unwrap()
            .broken
            .is_empty()
    );
    let document = read_editor_service_settings(&src_root, "ReplicatedStorage")
        .unwrap()
        .unwrap();
    assert!(
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["Pkg".to_string()],)
            .is_some()
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_apply_force_target_path_recreates_only_named_target() {
    let (dir, src_root) =
        setup_locked_missing_package_targets("apply-force-target-path", &["Pkg", "PkgOther"]);
    let manifest_path = dir.join("renium-link.json");

    let mut apply_args = test_link_apply_args(&dir);
    apply_args.link = Some("pkg".into());
    apply_args.force_target =
        vec![r#"{"service":"ReplicatedStorage","path":["ReplicatedStorage","Pkg"]}"#.into()];
    link_apply(apply_args).unwrap();

    let document = read_editor_service_settings(&src_root, "ReplicatedStorage")
        .unwrap()
        .unwrap();
    assert!(
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["Pkg".to_string()])
            .is_some()
    );
    assert!(
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["PkgOther".to_string()])
            .is_none()
    );
    assert_eq!(read_link_manifest(&manifest_path).unwrap().broken.len(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_apply_file_target_respects_write_policy() {
    for read_only in [false, true] {
        let dir = temp_dir(if read_only {
            "apply-readonly-file"
        } else {
            "apply-writable-file"
        });
        let service_dir = dir.join("src/ReplicatedStorage");
        write_link_test_service(&service_dir);
        let canonical = dir.join("links/Logger.luau");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        fs::write(&canonical, "return 1\n").unwrap();
        write_single_file_link_manifest(&dir, read_only);
        link_apply(test_link_apply_args(&dir)).unwrap();

        let mirror = service_dir.join("Logger.luau");
        set_path_readonly(&mirror, false).unwrap();
        fs::write(&mirror, "return 2\n").unwrap();
        link_apply(test_link_apply_args(&dir)).unwrap();
        assert_eq!(
            fs::read_to_string(&mirror).unwrap(),
            if read_only {
                "return 1\n"
            } else {
                "return 2\n"
            }
        );
        if !read_only {
            fs::write(&canonical, "return 3\n").unwrap();
            link_apply(test_link_apply_args(&dir)).unwrap();
            assert_eq!(fs::read_to_string(&mirror).unwrap(), "return 2\n");
        }
        set_path_readonly(&mirror, false).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn link_apply_writable_package_target_preserves_local_edits() {
    let dir = temp_dir("apply-writable-package");
    let src_root = dir.join("src");
    let service_dir = src_root.join("ReplicatedStorage");
    write_link_test_service(&service_dir);

    let package_dir = dir.join("links");
    fs::create_dir_all(&package_dir).unwrap();
    let package_path = package_dir.join("pkg.renium");
    settings_document(vec![settings_instance("pkg:0", "Pkg", "Folder", None)])
        .write_file(&package_path)
        .unwrap();

    write_package_link_manifest(&dir, "ReplicatedStorage", false, &["Pkg"]);

    link_apply(test_link_apply_args(&dir)).unwrap();

    let mut document = read_editor_service_settings(&src_root, "ReplicatedStorage")
        .unwrap()
        .unwrap();
    let pkg_index =
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["Pkg".to_string()])
            .unwrap();
    instance_api::add_instance(
        &mut document,
        AddInstanceSpec::new(None, "LocalChild".into(), "Folder".into(), Some(pkg_index)),
    )
    .unwrap();
    document
        .write_file(&service_settings_path(&service_dir))
        .unwrap();

    link_apply(test_link_apply_args(&dir)).unwrap();
    let document = read_editor_service_settings(&src_root, "ReplicatedStorage")
        .unwrap()
        .unwrap();
    assert!(
        resolve_editor_instance_by_path(
            &document,
            "ReplicatedStorage",
            &["Pkg".to_string(), "LocalChild".to_string()],
        )
        .is_some(),
        "local edit inside writable package target must survive link-apply"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_apply_strict_fails_on_warnings() {
    let dir = temp_dir("apply-strict");
    let src_root = dir.join("src");
    write_link_test_service(&src_root.join("ReplicatedStorage"));
    write_link_manifest(
        &dir.join("renium-link.json"),
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![LinkEntry {
                id: "missing".into(),
                read_only: true,
                source: LinkSource::Local {
                    path: "links/DoesNotExist.luau".into(),
                },
                targets: vec![LinkTargetRef {
                    service: "ReplicatedStorage".into(),
                    path: vec!["ReplicatedStorage".into(), "Missing".into()],
                    ords: Vec::new(),
                }],
            }],
            broken: Vec::new(),
        },
    )
    .unwrap();

    link_apply(test_link_apply_args(&dir)).unwrap();
    let mut strict_args = test_link_apply_args(&dir);
    strict_args.strict = true;
    let error = link_apply(strict_args).unwrap_err();
    assert!(error.to_string().contains("--strict"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_apply_removes_stale_mirror_when_canonical_file_disappears() {
    let dir = temp_dir("apply-stale-mirror");
    let src_root = dir.join("src");
    let service_dir = src_root.join("ReplicatedStorage");
    write_link_test_service(&service_dir);
    let canonical_dir = dir.join("links").join("Lib");
    fs::create_dir_all(&canonical_dir).unwrap();
    fs::write(canonical_dir.join("init.luau"), "return {}\n").unwrap();
    fs::write(canonical_dir.join("Extra.luau"), "return 1\n").unwrap();
    write_link_manifest(
        &dir.join("renium-link.json"),
        &LinkManifest {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: vec![LinkEntry {
                id: "lib".into(),
                read_only: true,
                source: LinkSource::Local {
                    path: "links/Lib".into(),
                },
                targets: vec![LinkTargetRef {
                    service: "ReplicatedStorage".into(),
                    path: vec!["ReplicatedStorage".into(), "Lib".into()],
                    ords: Vec::new(),
                }],
            }],
            broken: Vec::new(),
        },
    )
    .unwrap();

    link_apply(test_link_apply_args(&dir)).unwrap();
    let stale = service_dir.join("Lib").join("Extra.luau");
    assert!(stale.exists(), "first apply mirrors Extra.luau");

    fs::remove_file(canonical_dir.join("Extra.luau")).unwrap();
    link_apply(test_link_apply_args(&dir)).unwrap();
    assert!(
        !stale.exists(),
        "mirror of an upstream-deleted file must be cleaned up"
    );
    set_path_readonly(&service_dir.join("Lib").join("init.luau"), false).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn link_target_rejects_filesystem_escape_segments() {
    let target = LinkTargetRef {
        service: "ReplicatedStorage".into(),
        path: vec![
            "ReplicatedStorage".into(),
            "..".into(),
            "..".into(),
            "escaped".into(),
        ],
        ords: vec![],
    };

    assert!(validate_link_target_ref(&target).is_ok());
    assert!(validate_filesystem_link_target_ref(&target).is_err());
    assert!(link_target_dir_and_leaf(Path::new("src"), &target).is_err());
}

#[test]
fn link_target_file_pairs_preserves_extensions() {
    let root = temp_dir("pairs");
    let src_root = root.join("src");
    let source = root.join("source");
    fs::create_dir_all(source.join("Sub")).unwrap();
    fs::write(source.join("Foo.lua"), "return 1").unwrap();
    fs::write(source.join("Sub").join("init.server.lua"), "print('x')").unwrap();
    fs::write(source.join("notes.txt"), "ignore me").unwrap();
    let target = LinkTargetRef {
        service: "ReplicatedStorage".into(),
        path: vec!["ReplicatedStorage".into(), "Pkg".into()],
        ords: vec![],
    };
    let naming = config::ProjectScriptNaming::default();
    let pairs = link_target_file_pairs(&src_root, &target, &source, true, &naming).unwrap();
    let mirrors: HashSet<String> = pairs
        .iter()
        .map(|pair| {
            pair.mirror
                .strip_prefix(&src_root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert!(
        mirrors.contains("ReplicatedStorage/Pkg/Foo.lua"),
        "{mirrors:?}"
    );
    assert!(
        mirrors.contains("ReplicatedStorage/Pkg/Sub/init.server.lua"),
        "{mirrors:?}"
    );
    assert_eq!(pairs.len(), 2, "notes.txt should be skipped");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn link_single_file_source_maps_to_leaf_script() {
    let root = temp_dir("single");
    let src_root = root.join("src");
    let source = root.join("links").join("Logger.luau");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "return {}").unwrap();
    let target = LinkTargetRef {
        service: "ReplicatedStorage".into(),
        path: vec![
            "ReplicatedStorage".into(),
            "Modules".into(),
            "Logger".into(),
        ],
        ords: vec![],
    };
    let naming = config::ProjectScriptNaming::default();
    let pairs = link_target_file_pairs(&src_root, &target, &source, false, &naming).unwrap();
    assert_eq!(pairs.len(), 1);
    let rel = pairs[0]
        .mirror
        .strip_prefix(&src_root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    assert_eq!(rel, "ReplicatedStorage/Modules/Logger.luau");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn apply_link_enforcement_reverts_and_fans_out() {
    let root = temp_dir("enforce");
    let canonical = root.join("links").join("Logger.luau");
    let mirror_a = root
        .join("src")
        .join("ReplicatedStorage")
        .join("Logger.luau");
    let mirror_b = root
        .join("src")
        .join("ServerScriptService")
        .join("Logger.luau");
    for path in [&canonical, &mirror_a, &mirror_b] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "-- canonical v1").unwrap();
    }

    let mut enforcement = LinkEnforcement {
        active: true,
        ..LinkEnforcement::default()
    };
    enforcement
        .mirror_to_canonical
        .insert(path_key(&mirror_a), (canonical.clone(), true));
    enforcement
        .mirror_to_canonical
        .insert(path_key(&mirror_b), (canonical.clone(), true));
    enforcement.canonical_to_mirrors.insert(
        path_key(&canonical),
        vec![mirror_a.clone(), mirror_b.clone()],
    );

    fs::write(&mirror_a, "-- tampered").unwrap();
    let out = apply_link_enforcement_to_changed_paths(&root, &enforcement, vec![mirror_a.clone()])
        .unwrap();
    assert_eq!(fs::read_to_string(&mirror_a).unwrap(), "-- canonical v1");
    assert!(out.contains(&mirror_a));

    let _ = set_path_readonly(&mirror_a, false);
    let _ = set_path_readonly(&mirror_b, false);
    fs::write(&canonical, "-- canonical v2").unwrap();
    let out = apply_link_enforcement_to_changed_paths(&root, &enforcement, vec![canonical.clone()])
        .unwrap();
    assert_eq!(fs::read_to_string(&mirror_a).unwrap(), "-- canonical v2");
    assert_eq!(fs::read_to_string(&mirror_b).unwrap(), "-- canonical v2");
    assert!(out.contains(&mirror_a) && out.contains(&mirror_b) && out.contains(&canonical));

    let _ = set_path_readonly(&mirror_a, false);
    let _ = set_path_readonly(&mirror_b, false);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn apply_link_enforcement_matches_relative_changed_paths() {
    let root = temp_dir("rel");
    let canonical = root.join("links").join("L.luau");
    let mirror = root.join("src").join("ReplicatedStorage").join("L.luau");
    fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    fs::create_dir_all(mirror.parent().unwrap()).unwrap();
    fs::write(&canonical, "ok").unwrap();
    fs::write(&mirror, "tampered").unwrap();

    let mut enforcement = LinkEnforcement {
        active: true,
        ..LinkEnforcement::default()
    };
    enforcement
        .mirror_to_canonical
        .insert(link_path_key(&mirror), (canonical, true));

    let relative = PathBuf::from("src")
        .join("ReplicatedStorage")
        .join("L.luau");
    let out = apply_link_enforcement_to_changed_paths(&root, &enforcement, vec![relative.clone()])
        .unwrap();
    assert_eq!(fs::read_to_string(&mirror).unwrap(), "ok");
    assert!(out.iter().any(|path| path == &relative));

    let _ = set_path_readonly(&mirror, false);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn link_manifest_rejects_duplicate_ids() {
    let root = temp_dir("manifest-duplicate-id");
    let path = root.join("renium-link.json");
    fs::write(
            &path,
            r#"{
                "version": 1,
                "links": [
                    {"id":"dup","readOnly":true,"source":{"type":"local","path":"links/a.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","A"],"ords":[]}]},
                    {"id":"dup","readOnly":true,"source":{"type":"local","path":"links/b.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","B"],"ords":[]}]}
                ],
                "broken": []
            }"#,
        )
        .unwrap();

    let error = format!(
        "{:#}",
        read_link_manifest(&path)
            .err()
            .expect("duplicate ids should be rejected")
    );
    assert!(error.contains("duplicate renium-link id"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn link_apply_creates_missing_target_service() {
    let dir = temp_dir("missing-target-service");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("links")).unwrap();
    settings_document(vec![settings_instance("pkg:0", "Pkg", "Folder", None)])
        .write_file(&dir.join("links").join("pkg.renium"))
        .unwrap();
    write_package_link_manifest(&dir, "MissingService", true, &["Pkg"]);

    link_apply(test_link_apply_args(&dir)).unwrap();
    let settings_file = service_settings_path(&dir.join("src").join("MissingService"));
    let document = SettingsBytecode::read_file(&settings_file).unwrap();
    assert_eq!(document.instances[0].name, "MissingService");
    assert!(
        document
            .instances
            .iter()
            .any(|instance| instance.name == "Pkg")
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn link_break_remove_drops_manifest_target_and_keeps_mirror() {
    let dir = temp_dir("link-break-remove");
    let service_dir = dir.join("src").join("ReplicatedStorage");
    write_link_test_service(&service_dir);
    fs::create_dir_all(dir.join("links")).unwrap();
    fs::write(dir.join("links").join("Logger.luau"), "return 1\n").unwrap();
    write_single_file_link_manifest(&dir, true);
    link_apply(test_link_apply_args(&dir)).unwrap();

    let mirror = service_dir.join("Logger.luau");
    assert!(mirror.is_file());
    link_break(LinkBreakArgs {
        project: ProjectSourceArgs {
            project_root: dir.clone(),
            src_root: PathBuf::from("src"),
        },
        manifest: PathBuf::from("renium-link.json"),
        link: None,
        service: Some("ReplicatedStorage".into()),
        path_segments_json: Some(r#"["ReplicatedStorage","Logger"]"#.into()),
        path_ordinals_json: "[]".into(),
        remove: true,
        cache_dir: None,
        pretty: false,
    })
    .unwrap();

    assert!(!dir.join("renium-link.json").exists());
    assert!(!link_lock_path(&dir).exists());
    assert!(mirror.is_file());
    assert!(!fs::metadata(&mirror).unwrap().permissions().readonly());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn materialize_package_splices_any_instance_subtree() {
    let root = temp_dir("pkg");
    let src_root = root.join("src");

    let mut anchored = Map::new();
    anchored.insert("Anchored".to_string(), json!(true));
    let mut source = Map::new();
    source.insert("Source".to_string(), json!("return 42"));
    let package = settings_document(vec![
        settings_instance("pkg:0", "Widget", "Folder", None),
        SettingsBytecodeInstance {
            settings_id: "pkg:1".into(),
            name: "Block".into(),
            class_name: "Part".into(),
            parent_index: Some(0),
            properties: anchored,
            attributes: Map::new(),
        },
        SettingsBytecodeInstance {
            settings_id: "pkg:2".into(),
            name: "Mod".into(),
            class_name: "ModuleScript".into(),
            parent_index: Some(0),
            properties: source,
            attributes: Map::new(),
        },
    ]);
    let package_path = root.join("widget.renium");
    package.write_file(&package_path).unwrap();

    let mut doc = settings_document(vec![settings_instance(
        "editor:0",
        "ReplicatedStorage",
        "ReplicatedStorage",
        None,
    )]);
    let (changed, ids, _removed) = materialize_package_target(
        &mut doc,
        PackageMaterialization {
            service_dir: &src_root.join("ReplicatedStorage"),
            service: "ReplicatedStorage",
            target_segments: &["Pkgs".to_string(), "Widget".to_string()],
            target_ordinals: &[],
            package_path: &package_path,
            filesystem_target: true,
            external_references: &HashSet::new(),
        },
    )
    .unwrap();

    assert!(
        doc.instances
            .iter()
            .any(|i| i.name == "Widget" && i.class_name == "Folder")
    );
    let block = doc.instances.iter().find(|i| i.name == "Block").unwrap();
    assert_eq!(block.class_name, "Part");
    assert_eq!(block.properties.get("Anchored"), Some(&json!(true)));
    let module = doc.instances.iter().find(|i| i.name == "Mod").unwrap();
    assert_eq!(module.properties.get("Source"), Some(&json!("return 42")));
    assert_eq!(ids.len(), 3);
    assert!(changed.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn package_target_settings_ids_include_existing_subtree() {
    let document = settings_document(vec![
        settings_instance("editor:0", "ServerStorage", "ServerStorage", None),
        settings_instance("editor:1", "PackageRoot", "Folder", Some(0)),
        settings_instance("editor:2", "Child", "ModuleScript", Some(1)),
    ]);

    assert_eq!(
        package_target_settings_ids(
            &document,
            "ServerStorage",
            &["PackageRoot".to_string()],
            &[],
        ),
        vec!["editor:1".to_string(), "editor:2".to_string()]
    );
}

#[test]
fn link_apply_replaces_locked_package_target_with_added_child_drift() {
    let dir = temp_dir("package-added-child-drift");
    let src_root = dir.join("src");
    let service_dir = src_root.join("ReplicatedStorage");
    fs::create_dir_all(&service_dir).unwrap();
    settings_document(vec![settings_instance(
        "root",
        "ReplicatedStorage",
        "ReplicatedStorage",
        None,
    )])
    .write_file(&service_settings_path(&service_dir))
    .unwrap();

    fs::create_dir_all(dir.join("links")).unwrap();
    let package_path = dir.join("links").join("pkg.renium");
    settings_document(vec![settings_instance("pkg:0", "Pkg", "Folder", None)])
        .write_file(&package_path)
        .unwrap();
    write_package_link_manifest(&dir, "ReplicatedStorage", true, &["Pkg"]);

    let apply_args = || test_link_apply_args(&dir);
    link_apply(apply_args()).unwrap();

    let settings_path = service_settings_path(&service_dir);
    let mut document = SettingsBytecode::read_file(&settings_path).unwrap();
    let package_root =
        resolve_editor_instance_by_path(&document, "ReplicatedStorage", &["Pkg".to_string()])
            .unwrap();
    document.instances.push(settings_instance(
        "editor:injected",
        "Injected",
        "Folder",
        Some(package_root),
    ));
    document.write_file(&settings_path).unwrap();

    link_apply(apply_args()).unwrap();
    let updated = SettingsBytecode::read_file(&settings_path).unwrap();
    assert!(
        resolve_editor_instance_by_path(
            &updated,
            "ReplicatedStorage",
            &["Pkg".to_string(), "Injected".to_string()],
        )
        .is_none()
    );
    assert_eq!(updated.instances.len(), 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pack_then_materialize_round_trips_any_instance() {
    let root = temp_dir("roundtrip");
    let src_root = root.join("src");
    let svc_dir = src_root.join("ReplicatedStorage");
    fs::create_dir_all(svc_dir.join("Widget")).unwrap();
    fs::write(svc_dir.join("Widget").join("Mod.luau"), "return 7").unwrap();

    let mk = |id: &str, name: &str, class: &str, parent: Option<usize>| {
        settings_instance(id, name, class, parent)
    };
    let mut anchored = mk("editor:2", "Block", "Part", Some(1));
    anchored
        .properties
        .insert("Anchored".to_string(), json!(true));
    let source_doc = settings_document(vec![
        mk("editor:0", "ReplicatedStorage", "ReplicatedStorage", None),
        mk("editor:1", "Widget", "Folder", Some(0)),
        anchored,
        mk("editor:3", "Mod", "ModuleScript", Some(1)),
    ]);

    let source_paths =
        build_editor_source_paths_by_index(&source_doc, "ReplicatedStorage", &svc_dir);
    let widget_index =
        resolve_editor_instance_by_path(&source_doc, "ReplicatedStorage", &["Widget".to_string()])
            .unwrap();
    let (package, _) = pack_subtree_to_bytecode(&source_doc, widget_index, &source_paths).unwrap();
    assert_eq!(package.instances.len(), 3);
    let package_path = root.join("widget.renium");
    package.write_file(&package_path).unwrap();

    let mut target_doc = settings_document(vec![settings_instance(
        "editor:0",
        "ServerStorage",
        "ServerStorage",
        None,
    )]);
    let (changed, _ids, _removed) = materialize_package_target(
        &mut target_doc,
        PackageMaterialization {
            service_dir: &src_root.join("ServerStorage"),
            service: "ServerStorage",
            target_segments: &["Widget".to_string()],
            target_ordinals: &[],
            package_path: &package_path,
            filesystem_target: true,
            external_references: &HashSet::new(),
        },
    )
    .unwrap();

    assert!(
        target_doc
            .instances
            .iter()
            .any(|i| i.name == "Widget" && i.class_name == "Folder")
    );
    let block = target_doc
        .instances
        .iter()
        .find(|i| i.name == "Block")
        .unwrap();
    assert_eq!(block.class_name, "Part");
    assert_eq!(block.properties.get("Anchored"), Some(&json!(true)));
    let module = target_doc
        .instances
        .iter()
        .find(|i| i.name == "Mod")
        .unwrap();
    assert_eq!(module.properties.get("Source"), Some(&json!("return 7")));
    assert!(changed.is_empty());
    let _ = fs::remove_dir_all(&root);
}
