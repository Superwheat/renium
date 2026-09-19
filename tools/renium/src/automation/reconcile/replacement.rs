use super::*;

pub(crate) fn project_replacement_paths(
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
) -> HashSet<PathBuf> {
    desired
        .entries
        .keys()
        .chain(observed.entries.keys())
        .filter(|path| !derived_project_path(path))
        .cloned()
        .collect()
}

pub(crate) fn prepare_project_replacement(
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<HashSet<PathBuf>> {
    let documents = paths
        .par_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        })
        .map(|path| {
            // Byte-identical settings files decode to identical documents.
            if desired.entries.get(path) == observed.entries.get(path) {
                return Ok((path.clone(), None));
            }
            let (current, previous) = rayon::join(
                || settings_document(desired.entries.get(path)),
                || settings_document(observed.entries.get(path)),
            );
            let mut current = current?;
            let previous = previous?;
            if settings_documents_positionally_equivalent(&current, &previous) {
                drop_settings_documents(current, previous);
                return Ok((path.clone(), None));
            }
            let previous = if current.instances.is_empty() {
                // An omitted store clears authored contents through the existing
                // removal policy; engine service objects themselves remain.
                previous
            } else if service_has_only_native_containers(&previous) {
                // Only identities that survive replacement need matching.
                // Incoming descendants are not compared with outgoing content.
                let mut retained = retained_settings_document(&previous);
                drop_settings_document(previous);
                if !align_settings_ids_to_reference(&current, &mut retained) {
                    bail!(
                        "Could not align retained Studio objects in {}",
                        path.display()
                    );
                }
                crate::settings::equivalence::inherit_workspace_viewport_reference(
                    &mut current,
                    &retained,
                );
                retained
            } else {
                let mut previous = previous;
                if !align_settings_ids_to_reference(&current, &mut previous) {
                    bail!(
                        "Could not align existing Studio objects in {}",
                        path.display()
                    );
                }
                crate::settings::equivalence::inherit_workspace_viewport_reference(
                    &mut current,
                    &previous,
                );
                if settings_documents_equivalent(&current, &previous) {
                    drop_settings_documents(current, previous);
                    return Ok((path.clone(), None));
                }
                let current_by_id = current
                    .instances
                    .iter()
                    .enumerate()
                    .map(|(index, instance)| (instance.settings_id.as_str(), index))
                    .collect::<HashMap<_, _>>();
                let reusable = previous
                    .instances
                    .iter()
                    .enumerate()
                    .any(|(index, instance)| {
                        instance.parent_index.is_some()
                            && !is_protected_engine_container(&previous, index)
                            && !is_reconciliation_protected_workspace_camera(&previous, index)
                            && current_by_id
                                .get(instance.settings_id.as_str())
                                .is_some_and(|&other| {
                                    settings_instances_equal(&current, other, &previous, index)
                                })
                    });
                // With no incoming authored objects there is no native payload
                // to replace this service. Keep outgoing identities so the delta
                // planner can remove them, including children of locked containers.
                if reusable || service_has_only_native_containers(&current) {
                    previous
                } else {
                    let retained = retained_settings_document(&previous);
                    drop_settings_document(previous);
                    retained
                }
            };
            Ok((
                path.clone(),
                Some(PreparedEditorSettingsChange { previous, current }),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut unchanged = HashSet::new();
    for (path, change) in documents {
        if let Some(change) = change {
            prepared.insert(path, change);
        } else {
            unchanged.insert(path);
        }
    }
    Ok(unchanged)
}

pub(crate) fn snapshot_differences_prepared(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
    mut prepared: Option<&mut HashMap<PathBuf, PreparedEditorSettingsChange>>,
) -> Result<HashSet<PathBuf>> {
    let mut paths = left
        .entries
        .keys()
        .chain(right.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    let prepare = prepared.is_some();
    let compared = paths
        .into_par_iter()
        .map(|path| {
            let mut settings = None;
            let equivalent = snapshot_entry_equivalent(
                &path,
                left.entries.get(&path),
                right.entries.get(&path),
                prepare.then_some(&mut settings),
            )?;
            Ok((!equivalent).then_some((path, settings)))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut differences = HashSet::new();
    for (path, settings) in compared.into_iter().flatten() {
        if let Some(prepared) = prepared.as_mut()
            && let Some(settings) = settings
        {
            prepared.insert(path.clone(), settings);
        }
        differences.insert(path);
    }
    Ok(differences)
}

// Copy immutable source bytes into the private push stage. Complete removals
// and directory creation first, then write independent files concurrently.
// Linked paths retain the ordered copy path because they can alias one another.
pub(crate) fn stage_snapshot_paths(
    root: &Path,
    paths: &HashSet<PathBuf>,
    snapshot: &ProjectSnapshot,
) -> Result<()> {
    if snapshot
        .entries
        .values()
        .any(|entry| matches!(entry, SnapshotEntry::Symlink { .. }))
    {
        return apply_snapshot_paths(root, paths, snapshot);
    }
    let mut ordered = paths
        .iter()
        .filter(|path| !derived_project_path(path))
        .collect::<Vec<_>>();
    ordered.sort();
    let mut files = Vec::new();
    let mut directories = BTreeSet::new();
    for relative in ordered {
        let path = root.join(relative);
        match snapshot.entries.get(relative) {
            Some(SnapshotEntry::File(bytes)) => {
                remove_path(&path)?;
                if let Some(parent) = path.parent() {
                    directories.insert(parent.to_path_buf());
                }
                files.push((path, bytes));
            }
            Some(SnapshotEntry::Directory) => {
                directories.insert(path);
            }
            None => remove_path(&path)?,
            Some(SnapshotEntry::Symlink { .. }) => unreachable!(),
        }
    }
    for directory in directories {
        fs::create_dir_all(&directory)
            .with_context(|| format!("Failed to create {}", directory.display()))?;
    }
    files.par_iter().try_for_each(|(path, bytes)| {
        fs::write(path, bytes).with_context(|| format!("Failed to write {}", path.display()))
    })
}

pub(crate) fn apply_snapshot_paths(
    root: &Path,
    paths: &HashSet<PathBuf>,
    snapshot: &ProjectSnapshot,
) -> Result<()> {
    let mut paths = paths.iter().collect::<Vec<_>>();
    paths.sort();
    for relative in paths {
        if derived_project_path(relative) {
            continue;
        }
        let path = root.join(relative);
        match snapshot.entries.get(relative) {
            None => remove_path(&path)?,
            Some(SnapshotEntry::Directory) => fs::create_dir_all(&path)
                .with_context(|| format!("Failed to create {}", path.display()))?,
            Some(SnapshotEntry::File(bytes)) => {
                remove_path(&path)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, bytes)
                    .with_context(|| format!("Failed to write {}", path.display()))?;
            }
            Some(SnapshotEntry::Symlink { target, directory }) => {
                remove_path(&path)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                create_symlink(target, &path, *directory)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn derived_project_path(path: &Path) -> bool {
    path == Path::new("sourcemap.json") || path.starts_with(".renium")
}

pub(crate) fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("Failed to remove {}", path.display()))
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path, _directory: bool) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("Failed to create symbolic link {}", link.display()))
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, link: &Path, directory: bool) -> Result<()> {
    if directory {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
    .with_context(|| format!("Failed to create symbolic link {}", link.display()))
}
