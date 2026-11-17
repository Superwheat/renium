use super::*;

#[cfg(test)]
pub(crate) fn merge_snapshots(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    preference: ConflictPreference,
) -> Result<(ProjectSnapshot, Vec<String>)> {
    let (merged, conflicts, _) =
        merge_snapshots_with_changes(baseline, editor, studio, preference, None)?;
    Ok((merged, conflicts))
}

pub(crate) fn merge_snapshots_with_changes(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    preference: ConflictPreference,
    known_differences: Option<&HashSet<PathBuf>>,
) -> Result<(ProjectSnapshot, Vec<String>, MergeChanges)> {
    let sides = SnapshotSides {
        baseline,
        editor,
        studio,
    };
    let mut paths = baseline
        .into_iter()
        .flat_map(|snapshot| snapshot.entries.keys())
        .chain(editor.entries.keys())
        .chain(studio.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut changes = MergeChanges::default();
    for path in paths {
        let base = baseline.and_then(|snapshot| snapshot.entries.get(&path));
        let editor_entry = editor.entries.get(&path);
        let studio_entry = studio.entries.get(&path);
        let result = if known_differences.is_some_and(|paths| !paths.contains(&path)) {
            MergedEntry {
                value: editor_entry.cloned(),
                editor_changed: false,
                studio_changed: false,
                aligned_studio: None,
                editor_script_paths: None,
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            merge_settings_entry(&path, &sides, preference, base.is_none(), &mut conflicts)?
        } else {
            merge_entry(
                &path,
                base,
                editor_entry,
                studio_entry,
                preference,
                baseline.is_none(),
                &mut conflicts,
            )
        };
        if result.editor_changed {
            changes.editor.insert(path.clone());
        }
        if result.studio_changed {
            changes.studio.insert(path.clone());
            if let Some(document) = result.aligned_studio {
                changes.aligned_studio.insert(path.clone(), document);
            }
        }
        if let Some(paths) = result.editor_script_paths {
            changes.editor_script_paths.insert(path.clone(), paths);
        }
        if let Some(value) = result.value {
            merged.insert(path, value);
        }
    }
    reference_settlement::settle_references_between_stores(
        &sides,
        preference,
        &mut merged,
        &mut conflicts,
        &mut changes,
    )?;
    keep_directories_implied_by_files(&mut merged, &sides, &mut changes);
    settle_script_paths(
        &sides,
        preference,
        &mut merged,
        &mut conflicts,
        &mut changes,
    )?;
    conflicts.sort();
    conflicts.dedup();
    Ok((ProjectSnapshot { entries: merged }, conflicts, changes))
}

/// A directory stands as long as a file beneath it does. A capture of a
/// service whose folder lost its last script carries no entry for the folder,
/// so a merge that keeps that script (an edit preferred over Studio's
/// deletion) would otherwise drop the folder the files hold it in and fail the
/// readback against the folder Studio recreated around the script.
fn keep_directories_implied_by_files(
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    sides: &SnapshotSides<'_>,
    changes: &mut MergeChanges,
) {
    let files = merged
        .iter()
        .filter(|(_, entry)| matches!(entry, SnapshotEntry::File(_)))
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    for file in files {
        keep_directories_implied_by_file(&file, merged, sides, changes);
    }
}

fn keep_directories_implied_by_file(
    file: &Path,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    sides: &SnapshotSides<'_>,
    changes: &mut MergeChanges,
) {
    let is_directory = |snapshot: &ProjectSnapshot, path: &Path| {
        matches!(snapshot.entries.get(path), Some(SnapshotEntry::Directory))
    };
    for ancestor in file.ancestors().skip(1) {
        if merged.contains_key(ancestor) {
            continue;
        }
        let known = is_directory(sides.editor, ancestor)
            || is_directory(sides.studio, ancestor)
            || sides
                .baseline
                .is_some_and(|baseline| is_directory(baseline, ancestor));
        if !known {
            continue;
        }
        merged.insert(ancestor.to_path_buf(), SnapshotEntry::Directory);
        if is_directory(sides.editor, ancestor) {
            changes.editor.remove(ancestor);
        } else {
            changes.editor.insert(ancestor.to_path_buf());
        }
        if is_directory(sides.studio, ancestor) {
            changes.studio.remove(ancestor);
        } else {
            changes.studio.insert(ancestor.to_path_buf());
        }
    }
}

pub(crate) fn script_paths_by_settings_id(
    entries: &BTreeMap<PathBuf, SnapshotEntry>,
    settings_path: &Path,
) -> Result<HashMap<String, PathBuf>> {
    let Some(entry) = entries.get(settings_path) else {
        return Ok(HashMap::new());
    };
    script_paths_of(&settings_document(Some(entry))?, settings_path)
}

fn script_paths_of(
    document: &SettingsBytecode,
    settings_path: &Path,
) -> Result<HashMap<String, PathBuf>> {
    Ok(document
        .instances
        .iter()
        .zip(source_paths_by_settings_index(document, settings_path)?)
        .filter_map(|(instance, path)| path.map(|path| (instance.settings_id.clone(), path)))
        .collect())
}

pub(crate) fn source_paths_by_settings_index(
    document: &SettingsBytecode,
    settings_path: &Path,
) -> Result<Vec<Option<PathBuf>>> {
    if !document
        .instances
        .iter()
        .any(|instance| is_lua_source_class(&instance.class_name))
    {
        return Ok(vec![None; document.instances.len()]);
    }
    let (service_dir, project_root) = if settings_path
        .file_name()
        .is_some_and(|name| name == crate::system::files::SERVICE_SETTINGS_FILE_NAME)
    {
        (
            settings_path
                .parent()
                .context("A service settings path has no parent")?
                .to_path_buf(),
            None,
        )
    } else {
        let project = crate::app::context::project_override()
            .context("Script reconciliation requires the selected project")?;
        let root = project.parent().context("Selected project has no root")?;
        (
            crate::project::storage::source_directory(&root.join(settings_path)),
            Some(root.to_path_buf()),
        )
    };
    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .context("A service settings document has no root")?;
    let paths = build_editor_source_paths_by_index(document, &service.name, &service_dir);
    let Some(root) = project_root else {
        return Ok(paths);
    };
    paths
        .into_iter()
        .map(|path| {
            path.map(|path| {
                path.strip_prefix(&root)
                    .map(Path::to_path_buf)
                    .context("Script source is outside its project")
            })
            .transpose()
        })
        .collect()
}

pub(crate) fn changed_script_source_ids(
    settings_path: &Path,
    baseline: &SettingsBytecode,
    baseline_entries: &BTreeMap<PathBuf, SnapshotEntry>,
    observed: &SettingsBytecode,
    observed_entries: &BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<HashSet<String>> {
    let baseline_indices = baseline
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let baseline_paths = source_paths_by_settings_index(baseline, settings_path)?;
    let observed_paths = source_paths_by_settings_index(observed, settings_path)?;
    Ok(observed
        .instances
        .iter()
        .zip(observed_paths)
        .filter_map(|(instance, observed_path)| {
            let observed_path = observed_path?;
            let baseline_entry = baseline_indices
                .get(instance.settings_id.as_str())
                .and_then(|index| baseline_paths[*index].as_ref())
                .and_then(|path| baseline_entries.get(path));
            let observed_entry = observed_entries.get(&observed_path);
            (!entries_equivalent(&observed_path, baseline_entry, observed_entry))
                .then(|| instance.settings_id.clone())
        })
        .collect())
}

/// The source a script ends with when its file does not stand at one path on
/// every side: one side renamed or moved the script, or something above it,
/// or gave it its first child or took its last, while the other side may have
/// edited the text. The files merged path by path know nothing of that, so
/// the text is merged by the script's identity instead. A conflict names the
/// path the files hold the script at.
fn source_merged_by_identity(
    paths: (&Path, &Path),
    baseline: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    conflicts: &mut Vec<String>,
) -> Option<SnapshotEntry> {
    let (desired, editor_path) = paths;
    let (Some(editor @ SnapshotEntry::File(_)), Some(studio @ SnapshotEntry::File(_))) =
        (editor, studio)
    else {
        return None;
    };
    let same = |left: &SnapshotEntry, right: Option<&SnapshotEntry>| {
        entries_equivalent(desired, Some(left), right)
    };
    let source = if same(editor, Some(studio)) || same(studio, baseline) {
        editor
    } else if same(editor, baseline) {
        studio
    } else {
        match preference {
            ConflictPreference::Editor => editor,
            ConflictPreference::Studio => studio,
            ConflictPreference::None => {
                conflicts.push(format!("{} changed on both sides", editor_path.display()));
                editor
            }
        }
    };
    Some(source.clone())
}

/// Adds the folders a placed script file needs that no side has yet, up to
/// the nearest folder some side knows.
fn place_directories_above(
    file: &Path,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    sides: &SnapshotSides<'_>,
    touched: &mut BTreeSet<PathBuf>,
) {
    let known = |path: &Path| {
        let is_directory = |snapshot: &ProjectSnapshot| {
            matches!(snapshot.entries.get(path), Some(SnapshotEntry::Directory))
        };
        is_directory(sides.editor)
            || is_directory(sides.studio)
            || sides.baseline.is_some_and(is_directory)
    };
    let mut missing = Vec::new();
    for ancestor in file.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            return;
        }
        if matches!(merged.get(ancestor), Some(SnapshotEntry::Directory)) || known(ancestor) {
            break;
        }
        missing.push(ancestor.to_path_buf());
    }
    for directory in missing {
        merged.insert(directory.clone(), SnapshotEntry::Directory);
        touched.insert(directory);
    }
}

/// Removes the folders above a script file that left its place, as far up as
/// they hold nothing any more. A folder a side holds with nothing in it does
/// not stand there for its scripts (a service's folder stays when its last
/// script goes), so it is kept.
fn drop_directories_emptied_above(
    file: &Path,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    sides: &SnapshotSides<'_>,
    touched: &mut BTreeSet<PathBuf>,
) {
    // What a folder holds sorts right behind it.
    let holds_something = |entries: &BTreeMap<PathBuf, SnapshotEntry>, folder: &Path| {
        entries
            .range::<Path, _>((
                std::ops::Bound::Excluded(folder),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .is_some_and(|(path, _)| path.starts_with(folder))
    };
    let stands_empty = |snapshot: &ProjectSnapshot, folder: &Path| {
        matches!(snapshot.entries.get(folder), Some(SnapshotEntry::Directory))
            && !holds_something(&snapshot.entries, folder)
    };
    for ancestor in file.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty()
            || holds_something(merged, ancestor)
            || stands_empty(sides.editor, ancestor)
            || stands_empty(sides.studio, ancestor)
        {
            return;
        }
        if matches!(merged.get(ancestor), Some(SnapshotEntry::Directory)) {
            merged.remove(ancestor);
            touched.insert(ancestor.to_path_buf());
        }
    }
}

/// Brings the script files of every store the merge changed in line with
/// the merged store. A script whose file stands at different paths on the
/// two sides gets its source, merged by its identity, at the path the merged
/// store gives it. A script only one side has gets that side's source there.
/// A file at a path no script of the merged store has is removed, with the
/// folders that leaves empty.
pub(crate) fn settle_script_paths(
    sides: &SnapshotSides<'_>,
    preference: ConflictPreference,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    conflicts: &mut Vec<String>,
    changes: &mut MergeChanges,
) -> Result<()> {
    let (baseline, editor, studio) = (sides.baseline, sides.editor, sides.studio);
    let is_store = |path: &&PathBuf| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    };
    // A store both sides changed from the shared state moved its scripts on
    // both sides too, even where they now hold it alike: a sync that stopped
    // halfway leaves one side holding the result of the other's rename.
    let changed_on_both_sides = baseline.into_iter().flat_map(|baseline| {
        baseline
            .entries
            .iter()
            .filter(|(path, _)| is_store(path))
            .filter_map(|(path, entry)| {
                (editor.entries.get(path) != Some(entry) && studio.entries.get(path) != Some(entry))
                    .then_some(path)
            })
    });
    let settings_paths = changes
        .editor
        .iter()
        .chain(changes.studio.iter())
        .filter(is_store)
        .chain(changed_on_both_sides)
        .filter(|path| merged.contains_key(*path))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut touched = BTreeSet::new();
    let mut placed = Vec::new();
    let mut vacated = Vec::new();
    let mut settled_by_identity = HashSet::new();
    let mut identity_conflicts = Vec::new();
    for settings_path in settings_paths {
        let baseline_paths = baseline
            .map(|snapshot| script_paths_by_settings_id(&snapshot.entries, &settings_path))
            .transpose()?
            .unwrap_or_default();
        let editor_paths = script_paths_by_settings_id(&editor.entries, &settings_path)?;
        let studio_paths = script_paths_by_settings_id(&studio.entries, &settings_path)?;
        let merged_paths = script_paths_by_settings_id(merged, &settings_path)?;
        // Each side by the identities the merged store uses. Where the merge
        // left a side's store as it was, that store is the merged one.
        let editor_by_merged_id = changes
            .editor_script_paths
            .get(&settings_path)
            .unwrap_or(&editor_paths);
        let studio_by_merged_id = match changes.aligned_studio.get(&settings_path) {
            Some(aligned) => script_paths_of(aligned, &settings_path)?,
            None if !changes.studio.contains(&settings_path) => merged_paths.clone(),
            None => studio_paths.clone(),
        };
        let mut one_sided = Vec::new();
        for (id, desired) in &merged_paths {
            let (editor_path, studio_path) =
                match (editor_by_merged_id.get(id), studio_by_merged_id.get(id)) {
                    (Some(editor_path), Some(studio_path)) => (editor_path, studio_path),
                    (Some(path), None) => {
                        one_sided.push((desired, editor.entries.get(path)));
                        continue;
                    }
                    (None, Some(path)) => {
                        one_sided.push((desired, studio.entries.get(path)));
                        continue;
                    }
                    (None, None) => continue,
                };
            let baseline_path = baseline_paths.get(id);
            if editor_path == desired
                && studio_path == desired
                && baseline_path.is_none_or(|path| path == desired)
            {
                continue;
            }
            let Some(source) = source_merged_by_identity(
                (desired, editor_path),
                baseline_path
                    .and_then(|path| baseline.and_then(|snapshot| snapshot.entries.get(path))),
                editor.entries.get(editor_path),
                studio.entries.get(studio_path),
                preference,
                &mut identity_conflicts,
            ) else {
                continue;
            };
            settled_by_identity.extend(
                [
                    Some(desired),
                    Some(editor_path),
                    Some(studio_path),
                    baseline_path,
                ]
                .into_iter()
                .flatten()
                .cloned(),
            );
            merged.insert(desired.clone(), source);
            touched.insert(desired.clone());
            placed.push(desired.clone());
        }
        // A script only one side has takes its text from that side: where a
        // preference brought it back against the other side's removal, and
        // where its path is one a script settled above stood at, so the file
        // found there is that other script's.
        for (desired, source) in one_sided {
            let Some(source @ SnapshotEntry::File(_)) = source else {
                continue;
            };
            if matches!(merged.get(desired), Some(SnapshotEntry::File(_)))
                && !settled_by_identity.contains(desired)
            {
                continue;
            }
            merged.insert(desired.clone(), source.clone());
            touched.insert(desired.clone());
            placed.push(desired.clone());
        }
        let desired_paths = merged_paths.values().collect::<HashSet<_>>();
        let stale = baseline_paths
            .values()
            .chain(editor_paths.values())
            .chain(studio_paths.values())
            .filter(|path| !desired_paths.contains(path))
            .cloned()
            .collect::<BTreeSet<_>>();
        for path in stale {
            merged.remove(&path);
            touched.insert(path.clone());
            vacated.push(path);
        }
    }
    // What the path by path merge said about those files is void: a file one
    // side moved away and the other edited is no conflict of its own.
    conflicts.retain(|conflict| {
        !settled_by_identity
            .iter()
            .any(|path| *conflict == format!("{} changed on both sides", path.display()))
    });
    conflicts.append(&mut identity_conflicts);
    for path in &placed {
        place_directories_above(path, merged, sides, &mut touched);
    }
    for path in &vacated {
        drop_directories_emptied_above(path, merged, sides, &mut touched);
    }
    for path in touched {
        let merged_entry = merged.get(&path);
        if entries_equivalent(&path, editor.entries.get(&path), merged_entry) {
            changes.editor.remove(&path);
        } else {
            changes.editor.insert(path.clone());
        }
        if entries_equivalent(&path, studio.entries.get(&path), merged_entry) {
            changes.studio.remove(&path);
        } else {
            changes.studio.insert(path);
        }
    }
    for path in placed {
        keep_directories_implied_by_file(&path, merged, sides, changes);
    }
    Ok(())
}

pub(crate) fn merge_entry(
    path: &Path,
    base: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> MergedEntry {
    let unchanged = |side: Option<&SnapshotEntry>| {
        if first_pairing {
            side.is_none()
        } else {
            entries_equivalent(path, side, base)
        }
    };
    let value = if entries_equivalent(path, editor, studio) {
        editor.cloned()
    } else if unchanged(editor) {
        studio.cloned()
    } else if unchanged(studio) {
        editor.cloned()
    } else {
        merge_conflicting_entry(
            path,
            base,
            editor,
            studio,
            preference,
            first_pairing,
            conflicts,
        )
    };
    MergedEntry {
        editor_changed: !entries_equivalent(path, editor, value.as_ref()),
        studio_changed: !entries_equivalent(path, studio, value.as_ref()),
        value,
        aligned_studio: None,
        editor_script_paths: None,
    }
}

pub(crate) fn merge_conflicting_entry(
    path: &Path,
    base: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> Option<SnapshotEntry> {
    let ordinary_file_conflict = ((first_pairing || base.is_none())
        && matches!(editor, Some(SnapshotEntry::File(_)))
        && matches!(studio, Some(SnapshotEntry::File(_))))
        || (matches!(base, Some(SnapshotEntry::File(_)))
            && matches!(editor, None | Some(SnapshotEntry::File(_)))
            && matches!(studio, None | Some(SnapshotEntry::File(_))));
    if ordinary_file_conflict {
        match preference {
            ConflictPreference::Editor => return editor.cloned(),
            ConflictPreference::Studio => return studio.cloned(),
            ConflictPreference::None => {}
        }
    }
    conflicts.push(format!("{} changed on both sides", path.display()));
    editor.cloned()
}

pub(crate) fn is_source_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "lua" | "luau"))
}

pub(crate) fn entries_equivalent(
    path: &Path,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
) -> bool {
    match (left, right) {
        (Some(SnapshotEntry::File(left)), Some(SnapshotEntry::File(right)))
            if is_source_path(path) =>
        {
            left == right
                || crate::system::text::normalized_source_bytes(left)
                    .eq(crate::system::text::normalized_source_bytes(right))
        }
        _ => left == right,
    }
}

pub(crate) fn decoded_settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    let mut document = match entry {
        Some(SnapshotEntry::File(bytes)) => decode_settings_bytecode(bytes),
        None => Ok(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        }),
        Some(_) => bail!("A Renium settings store is not a regular file"),
    }?;
    stabilize_settings_reference_ids(&mut document);
    Ok(document)
}

pub(crate) fn editor_settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    decoded_settings_document(entry)
}

pub(crate) fn settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    let mut document = decoded_settings_document(entry)?;
    canonicalize_settings_property_names(&mut document)?;
    Ok(document)
}

/// A store that the baseline knew and the files no longer hold says nothing
/// about the service: Live Sync never reads it as "every instance deleted".
pub(crate) fn missing_store_message(path: &Path) -> String {
    format!(
        "{} is missing; a deleted store never empties its service in Studio. Restore the file and push it with `rbx ps {} -u`, or write it from Studio with `rbx pl`",
        path.display(),
        path.display()
    )
}

pub(crate) fn short_reconciliation_value(value: Option<&Value>) -> String {
    let text = value.map_or_else(|| "<absent>".to_string(), Value::to_string);
    if text.len() <= 96 {
        text
    } else {
        format!("{}...", text.chars().take(93).collect::<String>())
    }
}

pub(crate) fn first_record_difference(
    class_name: &str,
    observed: (&Map<String, Value>, &Map<String, Value>),
    expected: (&Map<String, Value>, &Map<String, Value>),
) -> Option<String> {
    let mut names = observed
        .0
        .keys()
        .chain(expected.0.keys())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    let database = rbx_reflection_database::get().ok();
    for name in names {
        let left = observed.0.get(name);
        let right = expected.0.get(name);
        if !crate::settings::equivalence::reconciliation_property_compares(
            database,
            class_name,
            name,
            left.or(right).unwrap_or(&Value::Null),
            left.is_some(),
            right.is_some(),
        ) {
            continue;
        }
        if !reconciliation_property_values_equal(class_name, name, left, right) {
            return Some(format!(
                "Studio {name} is {}; the files have {}",
                short_reconciliation_value(left),
                short_reconciliation_value(right)
            ));
        }
    }
    let mut names = observed
        .1
        .keys()
        .chain(expected.1.keys())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    for name in names {
        if crate::settings::equivalence::is_engine_managed_attribute(name) {
            continue;
        }
        let left = observed.1.get(name);
        let right = expected.1.get(name);
        let equal = left
            .zip(right)
            .is_some_and(|(left, right)| reconciliation_values_equal(left, right, false));
        if !equal {
            return Some(format!(
                "Studio attribute {name} is {}; the files have {}",
                short_reconciliation_value(left),
                short_reconciliation_value(right)
            ));
        }
    }
    None
}

pub(crate) fn first_three_way_property_difference(
    baseline: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &SettingsBytecode,
) -> Option<String> {
    let editor_by_id = editor
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance))
        .collect::<HashMap<_, _>>();
    let studio_by_id = studio
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance))
        .collect::<HashMap<_, _>>();
    for base in &baseline.instances {
        let (Some(editor), Some(studio)) = (
            editor_by_id.get(base.settings_id.as_str()).copied(),
            studio_by_id.get(base.settings_id.as_str()).copied(),
        ) else {
            continue;
        };
        let keys = base
            .properties
            .keys()
            .chain(editor.properties.keys())
            .chain(studio.properties.keys())
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for name in keys {
            let baseline_value = reconciliation_property_value(&base.properties, name);
            let editor_value = reconciliation_property_value(&editor.properties, name);
            let studio_value = reconciliation_property_value(&studio.properties, name);
            let editor_changed = !reconciliation_property_values_equal(
                &base.class_name,
                name,
                baseline_value,
                editor_value,
            );
            let studio_changed = !reconciliation_property_values_equal(
                &base.class_name,
                name,
                baseline_value,
                studio_value,
            );
            if editor_changed || studio_changed {
                return Some(format!(
                    "{} ({}) {name}: baseline={} editor={} studio={} editor_changed={editor_changed} studio_changed={studio_changed}",
                    base.name,
                    base.settings_id,
                    short_reconciliation_value(baseline_value),
                    short_reconciliation_value(editor_value),
                    short_reconciliation_value(studio_value),
                ));
            }
        }
    }
    None
}

pub(crate) fn merge_settings_entry(
    path: &Path,
    sides: &SnapshotSides<'_>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> Result<MergedEntry> {
    let base = sides
        .baseline
        .and_then(|snapshot| snapshot.entries.get(path));
    let editor = sides.editor.entries.get(path);
    let studio = sides.studio.entries.get(path);
    if editor == studio {
        return Ok(MergedEntry {
            value: editor.cloned(),
            editor_changed: false,
            studio_changed: false,
            aligned_studio: None,
            editor_script_paths: None,
        });
    }
    if !first_pairing && editor.is_none() && studio.is_some() {
        if !matches!(preference, ConflictPreference::Studio) {
            conflicts.push(missing_store_message(path));
        }
        return Ok(MergedEntry {
            value: studio.cloned(),
            editor_changed: true,
            studio_changed: false,
            aligned_studio: None,
            editor_script_paths: None,
        });
    }
    let mut editor_doc = settings_document(editor)
        .with_context(|| format!("Could not read the store {}", path.display()))?;
    let mut studio_doc = settings_document(studio)
        .with_context(|| format!("Could not read Studio's copy of {}", path.display()))?;
    if settings_documents_equivalent(&editor_doc, &studio_doc) {
        return Ok(MergedEntry {
            value: editor.cloned(),
            editor_changed: false,
            studio_changed: false,
            aligned_studio: None,
            editor_script_paths: None,
        });
    }
    remove_reconciliation_derived_properties(&mut editor_doc);
    remove_reconciliation_derived_properties(&mut studio_doc);
    let editor_script_paths;
    let (merged, editor_equivalent, studio_equivalent, aligned_studio) = if first_pairing {
        let previous_conflicts = conflicts.len();
        let alignment = align_first_pairing(
            path,
            &mut editor_doc,
            &mut studio_doc,
            preference,
            conflicts,
        );
        editor_script_paths = script_paths_of(&editor_doc, path)?;
        crate::settings::equivalence::inherit_workspace_viewport_reference(
            &mut editor_doc,
            &studio_doc,
        );
        align_reconciliation_protected_workspace_cameras(&editor_doc, &mut studio_doc);
        if conflicts.len() > previous_conflicts {
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &editor_doc);
            (editor_doc, true, studio_equivalent, Some(studio_doc))
        } else {
            let no_source_changes = HashSet::new();
            let empty = SettingsBytecode {
                version: editor_doc.version.max(studio_doc.version),
                instances: Vec::new(),
            };
            let (merged, merge_conflicts) = merge_reconciliation_settings_documents(
                &empty,
                &editor_doc,
                &studio_doc,
                preference,
                &no_source_changes,
                &no_source_changes,
            );
            conflicts.extend(merge_conflicts.into_iter().map(|conflict| {
                format!("{}: {}: {}", path.display(), conflict.path, conflict.detail)
            }));
            let editor_equivalent = !alignment.editor_overwritten
                && settings_documents_equivalent(&editor_doc, &merged);
            let studio_equivalent = alignment.studio_before.is_none()
                && settings_documents_equivalent(&studio_doc, &merged);
            (
                merged,
                editor_equivalent,
                studio_equivalent,
                Some(alignment.studio_before.unwrap_or(studio_doc)),
            )
        }
    } else {
        let mut base_doc = settings_document(base)?;
        remove_reconciliation_derived_properties(&mut base_doc);
        let referenced_elsewhere =
            reassigned_ids_referenced_by_other_stores(path, sides.editor, &base_doc, &editor_doc)?;
        release_reassigned_editor_ids(
            &base_doc,
            &mut editor_doc,
            &studio_doc,
            &referenced_elsewhere,
        );
        align_observation_ids_to_baseline(&base_doc, &mut editor_doc);
        align_observation_ids_to_baseline(&base_doc, &mut studio_doc);
        align_new_instance_ids(&base_doc, &editor_doc, &mut studio_doc)?;
        crate::settings::equivalence::inherit_workspace_viewport_reference(
            &mut editor_doc,
            &studio_doc,
        );
        align_reconciliation_protected_workspace_cameras(&editor_doc, &mut studio_doc);
        align_equivalent_values(&base_doc, &mut editor_doc);
        align_equivalent_values(&base_doc, &mut studio_doc);
        align_equivalent_values(&editor_doc, &mut studio_doc);
        protect_package_links(
            path,
            Some(&base_doc),
            &mut editor_doc,
            &mut studio_doc,
            conflicts,
        );
        align_transient_script_guids(&mut base_doc, &mut editor_doc, &mut studio_doc);
        editor_script_paths = script_paths_of(&editor_doc, path)?;
        if global_log_enabled(5) {
            log_global(
                5,
                format_args!(
                    "[renium] reconcile first property difference in {}: {}",
                    path.display(),
                    first_three_way_property_difference(&base_doc, &editor_doc, &studio_doc)
                        .unwrap_or_else(|| "none".to_string())
                ),
            );
        }
        let baseline_entries = &sides
            .baseline
            .context("A reconciliation baseline is missing")?
            .entries;
        let editor_source_changes = changed_script_source_ids(
            path,
            &base_doc,
            baseline_entries,
            &editor_doc,
            &sides.editor.entries,
        )?;
        let studio_source_changes = changed_script_source_ids(
            path,
            &base_doc,
            baseline_entries,
            &studio_doc,
            &sides.studio.entries,
        )?;
        if settings_documents_equivalent(&editor_doc, &studio_doc) {
            (editor_doc, true, true, None)
        } else if editor_source_changes.is_empty()
            && settings_documents_equivalent(&editor_doc, &base_doc)
        {
            (studio_doc, false, true, None)
        } else if studio_source_changes.is_empty()
            && settings_documents_equivalent(&studio_doc, &base_doc)
        {
            (editor_doc, true, false, Some(studio_doc))
        } else {
            let (merged, merge_conflicts) = merge_reconciliation_settings_documents(
                &base_doc,
                &editor_doc,
                &studio_doc,
                preference,
                &editor_source_changes,
                &studio_source_changes,
            );
            conflicts.extend(merge_conflicts.into_iter().map(|conflict| {
                format!("{}: {}: {}", path.display(), conflict.path, conflict.detail)
            }));
            let editor_equivalent = settings_documents_equivalent(&editor_doc, &merged);
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &merged);
            (
                merged,
                editor_equivalent,
                studio_equivalent,
                Some(studio_doc),
            )
        }
    };
    let value = SnapshotEntry::File(
        encode_settings_bytecode(&merged)
            .with_context(|| format!("Failed to merge {}", path.display()))?,
    );
    Ok(MergedEntry {
        value: Some(value),
        editor_changed: editor.is_none() || !editor_equivalent,
        studio_changed: studio.is_none() || !studio_equivalent,
        aligned_studio: aligned_studio.filter(|_| studio.is_some()),
        editor_script_paths: Some(editor_script_paths),
    })
}

/// The identities this store reuses for another class that some other store
/// of the same files references; those stores are decoded only when a reuse
/// exists.
fn reassigned_ids_referenced_by_other_stores(
    path: &Path,
    editor: &ProjectSnapshot,
    base_doc: &SettingsBytecode,
    editor_doc: &SettingsBytecode,
) -> Result<HashSet<String>> {
    let baseline_classes = base_doc
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance.class_name.as_str()))
        .collect::<HashMap<_, _>>();
    let reassigned = editor_doc
        .instances
        .iter()
        .filter(|instance| {
            baseline_classes
                .get(instance.settings_id.as_str())
                .is_some_and(|class_name| *class_name != instance.class_name)
        })
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let mut referenced = HashSet::new();
    if reassigned.is_empty() {
        return Ok(referenced);
    }
    for (other_path, entry) in &editor.entries {
        if other_path == path
            || !other_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        {
            continue;
        }
        let document = settings_document(Some(entry))?;
        for instance in &document.instances {
            for value in instance
                .properties
                .values()
                .chain(instance.attributes.values())
            {
                collect_referenced_ids(value, &reassigned, &mut referenced);
            }
        }
    }
    Ok(referenced)
}

fn collect_referenced_ids(value: &Value, wanted: &HashSet<&str>, output: &mut HashSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_referenced_ids(value, wanted, output);
            }
        }
        Value::Object(object) => {
            let reference = if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                Some(object)
            } else {
                object.get("Ref").and_then(Value::as_object)
            };
            if let Some(reference) = reference {
                for selector in ["settingsId", "instanceId"] {
                    if let Some(id) = reference.get(selector).and_then(Value::as_str)
                        && wanted.contains(id)
                    {
                        output.insert(id.to_string());
                    }
                }
            }
            for (key, value) in object {
                if key != "Ref" {
                    collect_referenced_ids(value, wanted, output);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn merge_reconciliation_settings_documents(
    base: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &SettingsBytecode,
    preference: ConflictPreference,
    editor_source_changes: &HashSet<String>,
    studio_source_changes: &HashSet<String>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    let prefer_studio = preference_bool(preference).map(|prefer_editor| !prefer_editor);
    let mut interner = PathInterner::new();
    let editor_paths = structural_path_ids(editor, &mut interner);
    let studio_paths = structural_path_ids(studio, &mut interner);
    let editor_by_id = editor
        .instances
        .iter()
        .zip(editor_paths)
        .map(|(instance, path)| (instance.settings_id.as_str(), path))
        .collect::<HashMap<_, _>>();
    let editor_persistent = persistent_identity_index(editor);
    let studio_persistent = persistent_identity_index(studio);
    let aligned_additions = studio
        .instances
        .iter()
        .zip(studio_paths)
        .enumerate()
        .filter(|(index, (instance, path))| {
            editor_by_id.get(instance.settings_id.as_str()) == Some(path)
                || persistent_identity(instance).is_some_and(|id| {
                    studio_persistent.get(id) == Some(&Some(*index))
                        && editor_persistent.get(id).copied().flatten().is_some_and(
                            |editor_index| {
                                let matched = &editor.instances[editor_index];
                                matched.settings_id == instance.settings_id
                                    && matched.class_name == instance.class_name
                            },
                        )
                })
        })
        .map(|(_, (instance, _))| instance.settings_id.clone())
        .collect();
    merge_aligned_settings_documents(
        base,
        studio,
        editor,
        prefer_studio,
        studio_source_changes,
        editor_source_changes,
        &aligned_additions,
    )
}

pub(crate) fn preference_bool(preference: ConflictPreference) -> Option<bool> {
    match preference {
        ConflictPreference::None => None,
        ConflictPreference::Editor => Some(true),
        ConflictPreference::Studio => Some(false),
    }
}
