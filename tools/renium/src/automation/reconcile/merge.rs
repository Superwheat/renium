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
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            merge_settings_entry(
                &path,
                &sides,
                preference,
                baseline.is_none(),
                &mut conflicts,
            )?
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
        }
        if let Some(value) = result.value {
            merged.insert(path, value);
        }
    }
    remove_superseded_script_paths(baseline, editor, studio, &mut merged, &mut changes)?;
    conflicts.sort();
    conflicts.dedup();
    Ok((ProjectSnapshot { entries: merged }, conflicts, changes))
}

pub(crate) fn script_paths_by_settings_id(
    entries: &BTreeMap<PathBuf, SnapshotEntry>,
    settings_path: &Path,
) -> Result<HashMap<String, PathBuf>> {
    let Some(entry) = entries.get(settings_path) else {
        return Ok(HashMap::new());
    };
    let document = settings_document(Some(entry))?;
    let source_paths = source_paths_by_settings_index(&document, settings_path)?;
    Ok(document
        .instances
        .iter()
        .zip(source_paths)
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

pub(crate) fn remove_superseded_script_paths(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    changes: &mut MergeChanges,
) -> Result<()> {
    let settings_paths = changes
        .editor
        .iter()
        .chain(changes.studio.iter())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut touched = BTreeSet::new();
    for settings_path in settings_paths {
        let baseline_paths = baseline
            .map(|snapshot| script_paths_by_settings_id(&snapshot.entries, &settings_path))
            .transpose()?
            .unwrap_or_default();
        let editor_paths = script_paths_by_settings_id(&editor.entries, &settings_path)?;
        let studio_paths = script_paths_by_settings_id(&studio.entries, &settings_path)?;
        let merged_paths = script_paths_by_settings_id(merged, &settings_path)?;
        let desired_paths = merged_paths.values().collect::<HashSet<_>>();
        let ids = baseline_paths
            .keys()
            .chain(editor_paths.keys())
            .chain(studio_paths.keys())
            .cloned()
            .collect::<HashSet<_>>();
        for id in ids {
            let desired = merged_paths.get(&id);
            for path in [
                baseline_paths.get(&id),
                editor_paths.get(&id),
                studio_paths.get(&id),
            ]
            .into_iter()
            .flatten()
            {
                if desired == Some(path) || desired_paths.contains(path) {
                    continue;
                }
                merged.remove(path);
                touched.insert(path.clone());
            }
        }
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
        (None, None) | (Some(SnapshotEntry::Directory), Some(SnapshotEntry::Directory)) => true,
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
        let equal = match (left, right) {
            (Some(left), Some(right)) => reconciliation_values_equal(left, right, false),
            (None, None) => true,
            _ => false,
        };
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
        });
    }
    let mut editor_doc = settings_document(editor)?;
    let mut studio_doc = settings_document(studio)?;
    if settings_documents_equivalent(&editor_doc, &studio_doc) {
        return Ok(MergedEntry {
            value: editor.cloned(),
            editor_changed: false,
            studio_changed: false,
        });
    }
    remove_reconciliation_derived_properties(&mut editor_doc);
    remove_reconciliation_derived_properties(&mut studio_doc);
    let (merged, editor_equivalent, studio_equivalent) = if first_pairing {
        let previous_conflicts = conflicts.len();
        align_first_pairing(
            path,
            &mut editor_doc,
            &mut studio_doc,
            preference,
            conflicts,
        );
        crate::settings::equivalence::inherit_workspace_viewport_reference(
            &mut editor_doc,
            &studio_doc,
        );
        align_reconciliation_protected_workspace_cameras(&editor_doc, &mut studio_doc);
        if conflicts.len() > previous_conflicts {
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &editor_doc);
            (editor_doc, true, studio_equivalent)
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
            let editor_equivalent = settings_documents_equivalent(&editor_doc, &merged);
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &merged);
            (merged, editor_equivalent, studio_equivalent)
        }
    } else {
        let mut base_doc = settings_document(base)?;
        remove_reconciliation_derived_properties(&mut base_doc);
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
            (editor_doc, true, true)
        } else if editor_source_changes.is_empty()
            && settings_documents_equivalent(&editor_doc, &base_doc)
        {
            (studio_doc, false, true)
        } else if studio_source_changes.is_empty()
            && settings_documents_equivalent(&studio_doc, &base_doc)
        {
            (editor_doc, true, false)
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
            (merged, editor_equivalent, studio_equivalent)
        }
    };
    let value = if merged.instances.is_empty() && editor.is_none() && studio.is_none() {
        None
    } else {
        Some(SnapshotEntry::File(
            encode_settings_bytecode(&merged)
                .with_context(|| format!("Failed to merge {}", path.display()))?,
        ))
    };
    let editor_changed = match editor {
        Some(_) => !editor_equivalent,
        None => value.is_some(),
    };
    let studio_changed = match studio {
        Some(_) => !studio_equivalent,
        None => value.is_some(),
    };
    Ok(MergedEntry {
        value,
        editor_changed,
        studio_changed,
    })
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
