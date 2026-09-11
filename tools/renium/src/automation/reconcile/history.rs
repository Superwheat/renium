use super::*;

use crate::app::timing::current_millis;
use crate::project::sourcemap::generate_project_sourcemap_at;

/// File-backed undo for a sync, independent of Studio's change-history service.
/// The pre-state comes from reconciliation's existing capture; never recapture it.
pub(super) struct SyncHistory {
    directory: PathBuf,
    record: SyncHistoryRecord,
}

#[derive(Serialize, Deserialize)]
struct SyncHistoryRecord {
    version: u8,
    committed_at: u64,
    source: PathBuf,
    scopes: Vec<PathBuf>,
    before: StoredSnapshot,
    expected: Option<[u8; 32]>,
    reverted: bool,
    #[serde(default)]
    separate_stores: bool,
}

impl SyncHistory {
    pub(super) fn begin(
        root: &Path,
        source: &Path,
        before: &ProjectSnapshot,
        paths: &HashSet<PathBuf>,
    ) -> Result<Self> {
        let _trace = crate::app::timing::trace_scope("sync", "history prepare");
        let source = absolutize_under(root, source)
            .strip_prefix(root)?
            .to_path_buf();
        let mut scopes = Vec::<PathBuf>::new();
        for path in paths.iter().collect::<BTreeSet<_>>() {
            if !derived_project_path(path)
                && scopes.last().is_none_or(|scope| !path.starts_with(scope))
            {
                scopes.push(path.clone());
            }
        }
        validate_paths(root, &scopes)?;
        let directory = create_unique_directory(
            &root.join(".renium/editor-history/sync"),
            &format!("{}-", current_millis()),
        )?;
        let mut cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&directory);
        });
        let before = StoredSnapshot::write_at(
            &directory.join("packs"),
            before
                .entries
                .iter()
                .filter(|(path, _)| selected(path, &scopes)),
        )?;
        let history = Self {
            directory: directory.clone(),
            record: SyncHistoryRecord {
                version: 1,
                committed_at: 0,
                source,
                scopes,
                before,
                expected: None,
                reverted: false,
                separate_stores: true,
            },
        };
        history.save()?;
        cleanup.disarm();
        Ok(history)
    }

    pub(super) fn commit(
        mut self,
        after: &ProjectSnapshot,
        generated: &ProjectSnapshot,
    ) -> Result<String> {
        let _trace = crate::app::timing::trace_scope("sync", "history commit");
        self.record.expected = Some(snapshot_digest(after, generated, &self.record.scopes));
        self.record.committed_at = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_nanos(),
        )?;
        self.save().with_context(|| format!(
            "Studio changes were applied, but undo history could not be finalized; recovery data remains in {}",
            self.directory.display(),
        ))?;
        Ok(self
            .directory
            .file_name()
            .context("History has no ID")?
            .to_string_lossy()
            .into_owned())
    }

    fn save(&self) -> Result<()> {
        atomic_write_file(
            &self.directory.join("sync.rmp"),
            &rmp_serde::to_vec(&self.record)?,
        )
    }
}

fn selected(path: &Path, scopes: &[PathBuf]) -> bool {
    scopes
        .partition_point(|scope| scope.as_path() <= path)
        .checked_sub(1)
        .is_some_and(|index| path.starts_with(&scopes[index]))
}

fn snapshot_digest(
    snapshot: &ProjectSnapshot,
    generated: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> [u8; 32] {
    // Hash borrowed file bytes directly, not a second encoded/copy of the place.
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|(path, _)| selected(path, scopes))
        .collect::<BTreeMap<_, _>>();
    entries.extend(
        generated
            .entries
            .iter()
            .filter(|(path, _)| selected(path, scopes)),
    );
    let mut hash = Sha256::new();
    let mut bytes = |value: &[u8]| {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    };
    for (path, entry) in entries {
        bytes(&(path.components().count() as u64).to_le_bytes());
        for component in path.components() {
            bytes(component.as_os_str().to_string_lossy().as_bytes());
        }
        match entry {
            SnapshotEntry::Directory => bytes(b"directory"),
            SnapshotEntry::File(value) => {
                bytes(b"file");
                bytes(value);
            }
            SnapshotEntry::Symlink { target, directory } => {
                bytes(if *directory {
                    b"directory-link"
                } else {
                    b"file-link"
                });
                bytes(target.to_string_lossy().as_bytes());
            }
        }
    }
    hash.finalize().into()
}

fn validate_paths(root: &Path, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        if path.as_os_str().is_empty()
            || derived_project_path(path)
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!("Invalid sync history path: {}", path.display());
        }
        // A saved symlink itself can be restored; never traverse an ancestor link.
        let mut ancestor = root.to_path_buf();
        for part in path.components().take(path.components().count() - 1) {
            ancestor.push(part);
            match fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "Sync history path crosses a symbolic link: {}",
                        ancestor.display()
                    );
                }
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct RevertedSync {
    pub(crate) id: String,
    pub(crate) paths: Vec<PathBuf>,
}

pub(crate) fn revert_sync(root: &Path, source: &Path, id: &str) -> Result<RevertedSync> {
    let history_root = root.join(".renium/editor-history/sync");
    let directory = if id == "latest" {
        let entries = fs::read_dir(&history_root)
            .context("No sync history found")?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let source = absolutize_under(root, source)
            .strip_prefix(root)?
            .to_path_buf();
        let mut found: Option<(u64, PathBuf)> = None;
        for entry in entries {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let record_path = entry.path().join("sync.rmp");
            if !record_path.is_file() {
                continue;
            }
            let record: SyncHistoryRecord = rmp_serde::from_slice(&fs::read(record_path)?)?;
            if record.source == source
                && record.expected.is_some()
                && !record.reverted
                && found
                    .as_ref()
                    .is_none_or(|(stamp, _)| record.committed_at > *stamp)
            {
                found = Some((record.committed_at, entry.path()));
            }
        }
        found
            .context("No completed sync history found for this source root")?
            .1
    } else {
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("Invalid sync history ID");
        }
        history_root.join(id)
    };
    let mut history = SyncHistory {
        record: rmp_serde::from_slice(&fs::read(directory.join("sync.rmp"))?)?,
        directory,
    };
    if history.record.version != 1 || history.record.reverted {
        bail!("Sync history is unsupported or already reverted");
    }
    if absolutize_under(root, source).strip_prefix(root)? != history.record.source {
        bail!("Sync history belongs to a different source root");
    }
    let expected = history
        .record
        .expected
        .context("Sync outcome is unconfirmed; its recovery snapshot has been retained")?;
    validate_paths(root, &history.record.scopes)?;
    let mut before = history
        .record
        .before
        .load_at(&history.directory.join("packs"), |_| true)?;
    validate_paths(root, &before.entries.keys().cloned().collect::<Vec<_>>())?;
    if before
        .entries
        .keys()
        .any(|path| !selected(path, &history.record.scopes))
    {
        bail!("Sync history contains data outside its recorded scopes");
    }
    // Verify older undo records using their original path spelling, then restore
    // their bytes into the migrated layout. Never weaken the newer-edit guard.
    let mut scopes = history.record.scopes.clone();
    let mut migrated = BTreeMap::new();
    if !history.record.separate_stores {
        for (old, new) in crate::project::storage::legacy_store_paths(root)?
            .into_iter()
            .chain(before.entries.keys().filter_map(|old| {
                crate::project::storage::relocated_legacy_path(root, old)
                    .map(|new| (old.clone(), new))
            }))
        {
            if selected(&old, &history.record.scopes) {
                migrated.insert(old, new);
            }
        }
        scopes = scopes
            .into_iter()
            .map(|path| migrated.get(&path).cloned().unwrap_or(path))
            .collect();
        scopes.extend(migrated.values().cloned());
        scopes.sort();
        scopes.dedup();
    }
    validate_paths(root, &scopes)?;
    let stage = ExportProjectStage::create(root, &history.record.source, &sync_services())?;
    if scopes.iter().any(|path| {
        !stage
            .publish_paths()
            .iter()
            .any(|scope| path.starts_with(scope))
    }) {
        bail!("Project layout changed; sync history no longer belongs to the configured sources");
    }
    let mut current = capture_snapshot(root, &scopes)?;
    for (old, new) in &migrated {
        if let Some(entry) = current.entries.remove(new)
            && current.entries.insert(old.clone(), entry).is_some()
        {
            bail!("Both legacy and migrated store paths exist; resolve them before undo");
        }
    }
    if snapshot_digest(
        &current,
        &ProjectSnapshot::default(),
        &history.record.scopes,
    ) != expected
    {
        bail!("Affected project files changed after this sync; undo would overwrite newer edits");
    }
    for (old, new) in migrated {
        if let Some(entry) = before.entries.remove(&old)
            && before.entries.insert(new, entry).is_some()
        {
            bail!("Sync history contains conflicting legacy and migrated stores");
        }
    }
    for path in &scopes {
        remove_path(&stage.project_root.join(path))?;
    }
    apply_snapshot_paths(
        &stage.project_root,
        &before.entries.keys().cloned().collect(),
        &before,
    )?;
    generate_project_sourcemap_at(
        &stage.project_root,
        &stage.project_root.join(&history.record.source),
    )?;
    stage.publish(root, false)?;
    history.record.reverted = true;
    history
        .save()
        .context("Files were restored, but the sync history completion mark could not be saved")?;
    Ok(RevertedSync {
        id: history
            .directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        paths: scopes.into_iter().map(|path| root.join(path)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::temp_dir;

    #[test]
    fn legacy_sync_undo_survives_store_migration_and_still_guards_newer_edits() -> Result<()> {
        for whole_service in [false, true] {
            let root = temp_dir("legacy-sync-undo");
            let _cleanup = OnDrop::new(|| {
                crate::project::storage::forget(&root);
                let _ = fs::remove_dir_all(&root);
            });
            let old = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
            let scopes = vec![if whole_service {
                PathBuf::from("src/Workspace")
            } else {
                old.clone()
            }];
            let mut document = SettingsBytecode {
                version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                instances: vec![crate::settings::bytecode::SettingsBytecodeInstance::new(
                    "workspace".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                )],
            };
            document.write_file(&root.join(&old))?;
            let original = fs::read(root.join(&old))?;
            let before = capture_snapshot(&root, &scopes)?;
            let mut history = SyncHistory::begin(
                &root,
                Path::new("src"),
                &before,
                &scopes.iter().cloned().collect(),
            )?;
            history.record.separate_stores = false;
            document.instances[0]
                .attributes
                .insert("Revision".into(), json!(2));
            document.write_file(&root.join(&old))?;
            let after = capture_snapshot(&root, &scopes)?;
            let id = history.commit(&after, &ProjectSnapshot::default())?;
            fs::write(root.join("renium.project.jsonc"), br#"{"schemaVersion":1}"#)?;
            config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
            let new = root.join("instances/Workspace.renium");
            let expected = fs::read(&new)?;
            document.instances[0]
                .attributes
                .insert("Revision".into(), json!(3));
            document.write_file(&new)?;
            assert!(
                revert_sync(&root, Path::new("src"), &id)
                    .unwrap_err()
                    .to_string()
                    .contains("newer edits")
            );
            fs::write(&new, expected)?;
            revert_sync(&root, Path::new("src"), &id)?;
            assert_eq!(fs::read(new)?, original);
            assert!(!root.join(old).exists());
        }
        Ok(())
    }

    fn snapshot(files: &[(&str, &[u8])]) -> ProjectSnapshot {
        ProjectSnapshot {
            entries: files
                .iter()
                .map(|(path, bytes)| (PathBuf::from(path), SnapshotEntry::File(bytes.to_vec())))
                .collect(),
        }
    }

    #[test]
    fn sync_history_cli_requires_an_unambiguous_selector() {
        use crate::cli::EditorRevertArgs;
        use clap::Parser;
        let args =
            EditorRevertArgs::try_parse_from(["rev", "--sync", "latest", "--details"]).unwrap();
        assert!(args.details);
        assert_eq!(args.sync.as_deref(), Some("latest"));
        for args in [
            vec!["rev", "--details"],
            vec!["rev", "--sync", "latest", "--path", "src/Workspace/A.luau"],
            vec!["rev", "--sync", "latest", "--settings-id", "editor:1"],
        ] {
            assert!(EditorRevertArgs::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn sync_history_latest_is_scoped_to_its_source_and_skips_unconfirmed_records() {
        let root = temp_dir("sync-history-source");
        let mut ids = Vec::new();
        for source in ["src", "places/other/src"] {
            let path = format!("{source}/Workspace/A.luau");
            let paths = HashSet::from([PathBuf::from(&path)]);
            let before = snapshot(&[(&path, b"return 1")]);
            let after = snapshot(&[(&path, b"return 2")]);
            ids.push(
                SyncHistory::begin(&root, Path::new(source), &before, &paths)
                    .unwrap()
                    .commit(&after, &ProjectSnapshot::default())
                    .unwrap(),
            );
            apply_snapshot_paths(&root, &paths, &after).unwrap();
            // A later request with an unknown outcome must not hide this undo.
            SyncHistory::begin(&root, Path::new(source), &after, &paths).unwrap();
        }
        assert!(revert_sync(&root, Path::new("src"), &ids[1]).is_err());
        assert_eq!(
            revert_sync(&root, Path::new("src"), "latest").unwrap().id,
            ids[0]
        );
        assert_eq!(
            fs::read(root.join("places/other/src/Workspace/A.luau")).unwrap(),
            b"return 2"
        );
        assert_eq!(
            revert_sync(&root, Path::new("places/other/src"), "latest")
                .unwrap()
                .id,
            ids[1]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_history_restores_created_deleted_and_changed_files_without_touching_other_work() {
        let root = temp_dir("sync-history");
        let before = snapshot(&[
            ("src/Workspace/A.luau", b"return 1"),
            ("src/Workspace/Deleted.luau", b"return 2"),
        ]);
        let after = snapshot(&[
            ("src/Workspace/A.luau", b"return 3"),
            ("src/Workspace/New.luau", b"return 4"),
        ]);
        let paths = before
            .entries
            .keys()
            .chain(after.entries.keys())
            .cloned()
            .collect();
        let history = SyncHistory::begin(&root, Path::new("src"), &before, &paths).unwrap();
        let pending: SyncHistoryRecord =
            rmp_serde::from_slice(&fs::read(history.directory.join("sync.rmp")).unwrap()).unwrap();
        assert!(pending.expected.is_none());
        let id = history.commit(&after, &ProjectSnapshot::default()).unwrap();
        apply_snapshot_paths(&root, &paths, &after).unwrap();
        fs::write(root.join("src/Workspace/Other.luau"), b"return 'keep'").unwrap();
        let changed = revert_sync(&root, &root.join("src"), &id).unwrap();
        assert_eq!(changed.id, id);
        assert_eq!(changed.paths.len(), 3);
        assert_eq!(
            fs::read(root.join("src/Workspace/A.luau")).unwrap(),
            b"return 1"
        );
        assert_eq!(
            fs::read(root.join("src/Workspace/Deleted.luau")).unwrap(),
            b"return 2"
        );
        assert!(!root.join("src/Workspace/New.luau").exists());
        assert_eq!(
            fs::read(root.join("src/Workspace/Other.luau")).unwrap(),
            b"return 'keep'"
        );
        assert!(revert_sync(&root, Path::new("src"), &id).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_history_guards_newer_edits_and_uses_generated_file_bytes() {
        let root = temp_dir("sync-history-conflict");
        let path = PathBuf::from("src/Workspace/A.luau");
        let paths = HashSet::from([path.clone()]);
        let before = snapshot(&[("src/Workspace/A.luau", b"return 1")]);
        let after = snapshot(&[("src/Workspace/A.luau", b"return 2")]);
        let generated = snapshot(&[("src/Workspace/A.luau", b"return 3")]);
        SyncHistory::begin(&root, Path::new("src"), &before, &paths)
            .unwrap()
            .commit(&after, &generated)
            .unwrap();
        apply_snapshot_paths(&root, &paths, &after).unwrap();
        let error = revert_sync(&root, Path::new("src"), "latest").unwrap_err();
        assert!(error.to_string().contains("newer edits"));
        assert_eq!(fs::read(root.join(&path)).unwrap(), b"return 2");
        apply_snapshot_paths(&root, &paths, &generated).unwrap();
        let restored = revert_sync(&root, Path::new("src"), "latest").unwrap();
        assert_ne!(restored.id, "latest");
        assert_eq!(fs::read(root.join(path)).unwrap(), b"return 1");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_history_restores_directory_file_type_changes_and_rejects_extra_descendants() {
        let root = temp_dir("sync-history-types");
        let scope = PathBuf::from("src/Workspace/Asset");
        let paths = HashSet::from([scope.clone()]);
        let file = snapshot(&[("src/Workspace/Asset", b"binary")]);
        let mut directory = snapshot(&[("src/Workspace/Asset/Child.luau", b"return 1")]);
        directory
            .entries
            .insert(scope.clone(), SnapshotEntry::Directory);
        SyncHistory::begin(&root, Path::new("src"), &file, &paths)
            .unwrap()
            .commit(&directory, &ProjectSnapshot::default())
            .unwrap();
        apply_snapshot_paths(
            &root,
            &directory.entries.keys().cloned().collect(),
            &directory,
        )
        .unwrap();
        fs::write(root.join(&scope).join("Extra"), b"new").unwrap();
        assert!(revert_sync(&root, Path::new("src"), "latest").is_err());
        fs::remove_file(root.join(&scope).join("Extra")).unwrap();
        revert_sync(&root, Path::new("src"), "latest").unwrap();
        assert_eq!(fs::read(root.join(&scope)).unwrap(), b"binary");
        SyncHistory::begin(&root, Path::new("src"), &directory, &paths)
            .unwrap()
            .commit(&file, &ProjectSnapshot::default())
            .unwrap();
        revert_sync(&root, Path::new("src"), "latest").unwrap();
        assert_eq!(
            fs::read(root.join(&scope).join("Child.luau")).unwrap(),
            b"return 1"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_history_rejects_unsafe_paths_and_unconfirmed_outcomes() {
        let root = temp_dir("sync-history-safety");
        for path in [
            PathBuf::from("../outside"),
            PathBuf::from(".renium/state"),
            PathBuf::new(),
            root.join("outside"),
        ] {
            assert!(validate_paths(&root, &[path]).is_err());
        }
        let paths = HashSet::from([PathBuf::from("src/Workspace/A.luau")]);
        let before = snapshot(&[("src/Workspace/A.luau", b"return 1")]);
        let history = SyncHistory::begin(&root, Path::new("src"), &before, &paths).unwrap();
        let id = history.directory.file_name().unwrap().to_str().unwrap();
        assert!(
            revert_sync(&root, Path::new("src"), id)
                .unwrap_err()
                .to_string()
                .contains("unconfirmed")
        );
        assert!(revert_sync(&root, Path::new("src"), "../outside").is_err());
        let pack = fs::read_dir(history.directory.join("packs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(pack, b"corrupt").unwrap();
        let id = history
            .commit(&before, &ProjectSnapshot::default())
            .unwrap();
        assert!(
            revert_sync(&root, Path::new("src"), &id)
                .unwrap_err()
                .to_string()
                .contains("corrupted")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
