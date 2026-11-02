use super::*;

pub(crate) fn supporting_settings_scopes(
    context: &BoundContext,
    paths: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source);
    services_for_snapshot_paths(context, paths)
        .into_iter()
        .map(|service| {
            service_settings_path(&source.join(service))
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .context("Supporting store is outside its project root")
        })
        .collect()
}

/// The link enforcement a comparison folds packed scripts with, built once
/// per operation: None without an active manifest; a manifest whose package
/// cannot be read or resolved is an error, never "nothing is packed".
pub(crate) fn packing_link_enforcement(
    root: &Path,
    src_root: &Path,
) -> Result<Option<crate::project::package_links::LinkEnforcement>> {
    let enforcement = crate::project::package_links::build_link_enforcement(root, src_root, None)
        .with_context(|| {
        format!(
            "Could not place the packed link scripts of {}",
            root.display()
        )
    })?;
    Ok(enforcement.is_active().then_some(enforcement))
}

/// `rbx lkp` leaves a packed script's text in its store and removes its file,
/// while a Studio capture carries that text in a file and the external marker
/// in the store. Folding the captured text into Studio's row lets the two
/// stores compare, and the capture's file no longer reads as a script to
/// pull. Which scripts count is decided by the link manifest, the signal the
/// push side uses, not by the store carrying inline text.
pub(crate) fn pack_read_only_link_scripts_with(
    enforcement: &crate::project::package_links::LinkEnforcement,
    root: &Path,
    editor: &ProjectSnapshot,
    studio: &mut ProjectSnapshot,
) -> Result<()> {
    let stores = editor
        .entries
        .keys()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut removed = Vec::new();
    for path in stores {
        let (Some(SnapshotEntry::File(editor_bytes)), Some(SnapshotEntry::File(studio_bytes))) =
            (editor.entries.get(&path), studio.entries.get(&path))
        else {
            continue;
        };
        let editor_doc = decode_settings_bytecode(editor_bytes)
            .with_context(|| format!("Could not read the store {}", path.display()))?;
        let service = settings_service_name(&path, &editor_doc, &editor_doc)?;
        let editor_paths = crate::editor::paths::build_editor_instance_paths(&editor_doc, &service);
        let packed = editor_doc
            .instances
            .iter()
            .zip(&editor_paths)
            .filter(|(instance, _)| is_lua_source_class(&instance.class_name))
            .filter_map(|(instance, instance_path)| {
                let text = instance.properties.get("Source")?.as_str()?;
                if text == crate::settings::EXTERNAL_SOURCE_MARKER {
                    return None;
                }
                let instance_path = instance_path.as_ref()?;
                enforcement.read_only_package_for_path(
                    &service,
                    &instance_path.path_segments,
                    &instance_path.path_ordinals,
                )?;
                Some((
                    (
                        instance_path.path_segments.clone(),
                        instance_path.path_ordinals.clone(),
                    ),
                    text.to_string(),
                ))
            })
            .collect::<HashMap<_, _>>();
        if packed.is_empty() {
            continue;
        }
        let mut studio_doc = decode_settings_bytecode(studio_bytes)
            .with_context(|| format!("Could not read Studio's copy of {}", path.display()))?;
        let studio_paths = crate::editor::paths::build_editor_instance_paths(&studio_doc, &service);
        let service_dir = crate::project::storage::source_directory(&root.join(&path));
        let source_paths = build_editor_source_paths_by_index(&studio_doc, &service, &service_dir);
        let mut folded = false;
        for (index, instance_path) in studio_paths.iter().enumerate() {
            let Some(instance_path) = instance_path else {
                continue;
            };
            let Some(inline) = packed.get(&(
                instance_path.path_segments.clone(),
                instance_path.path_ordinals.clone(),
            )) else {
                continue;
            };
            let Some(relative) = source_paths[index]
                .as_ref()
                .and_then(|source_path| source_path.strip_prefix(root).ok())
            else {
                continue;
            };
            let Some(SnapshotEntry::File(captured)) = studio.entries.get(relative) else {
                continue;
            };
            let same = crate::system::text::normalized_source_bytes(captured).eq(
                crate::system::text::normalized_source_bytes(inline.as_bytes()),
            );
            let text = if same {
                inline.clone()
            } else {
                String::from_utf8_lossy(captured).into_owned()
            };
            studio_doc.instances[index]
                .properties
                .insert("Source".to_string(), Value::String(text));
            let relative = relative.to_path_buf();
            studio.entries.remove(&relative);
            removed.push(relative);
            folded = true;
        }
        if folded {
            studio.entries.insert(
                path,
                SnapshotEntry::File(encode_settings_bytecode(&studio_doc)?),
            );
        }
    }
    drop_directories_emptied_by_folding(editor, studio, &removed);
    drop_stripped_package_links(enforcement, editor, studio)
}

/// `lkp --share` strips the PackageLink at the root of a shared copy, so the
/// consumer's store holds none there while Studio keeps its own. The link is
/// Studio's to keep until `upl` removes it: it leaves the capture before the
/// comparison, so neither a pull writes it into the store nor a push plans
/// its removal.
fn drop_stripped_package_links(
    enforcement: &crate::project::package_links::LinkEnforcement,
    editor: &ProjectSnapshot,
    studio: &mut ProjectSnapshot,
) -> Result<()> {
    let stores = studio
        .entries
        .keys()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        })
        .cloned()
        .collect::<Vec<_>>();
    for path in stores {
        let Some(SnapshotEntry::File(studio_bytes)) = studio.entries.get(&path) else {
            continue;
        };
        let mut studio_doc = decode_settings_bytecode(studio_bytes)
            .with_context(|| format!("Could not read Studio's copy of {}", path.display()))?;
        let service = settings_service_name(&path, &studio_doc, &studio_doc)?;
        let editor_doc = match editor.entries.get(&path) {
            Some(SnapshotEntry::File(bytes)) => Some(
                decode_settings_bytecode(bytes)
                    .with_context(|| format!("Could not read the store {}", path.display()))?,
            ),
            _ => None,
        };
        let editor_link_parents = editor_doc
            .as_ref()
            .map(|document| {
                let paths = crate::editor::paths::build_editor_instance_paths(document, &service);
                document
                    .instances
                    .iter()
                    .filter(|instance| instance.class_name == "PackageLink")
                    .filter_map(|instance| paths[instance.parent_index?].clone())
                    .map(|parent| (parent.path_segments, parent.path_ordinals))
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let studio_paths = crate::editor::paths::build_editor_instance_paths(&studio_doc, &service);
        let stripped = studio_doc
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| instance.class_name == "PackageLink")
            .filter_map(|(index, instance)| {
                let parent = studio_paths[instance.parent_index?].as_ref()?;
                let key = (parent.path_segments.clone(), parent.path_ordinals.clone());
                (enforcement.unlink_matches_package(&service, &key.0, &key.1)
                    && !editor_link_parents.contains(&key))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if stripped.is_empty() {
            continue;
        }
        crate::settings::instance::remove_instances_at_indices(&mut studio_doc, &stripped, true)?;
        studio.entries.insert(
            path,
            SnapshotEntry::File(encode_settings_bytecode(&studio_doc)?),
        );
    }
    Ok(())
}

/// A folded script's directory existed in the capture only to hold its
/// file; when nothing else is left in it and the files have no such
/// directory, the capture's entry would read as a directory the push removed.
fn drop_directories_emptied_by_folding(
    editor: &ProjectSnapshot,
    studio: &mut ProjectSnapshot,
    removed: &[PathBuf],
) {
    for relative in removed {
        for directory in relative.ancestors().skip(1) {
            if directory.as_os_str().is_empty() || editor.entries.contains_key(directory) {
                break;
            }
            let occupied = studio
                .entries
                .keys()
                .any(|entry| entry != directory && entry.starts_with(directory));
            if occupied {
                break;
            }
            if studio.entries.remove(directory).is_none() {
                break;
            }
        }
    }
}

pub(crate) fn baseline_scopes(context: &BoundContext, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source);
    let loaded = config::load_project(Some(Path::new(&context.project)), None)?;
    let inputs = project_watch_inputs(&loaded)?;
    let project = Path::new(&context.project);
    let mut scopes = Vec::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        let relative = absolute.strip_prefix(root).with_context(|| {
            format!(
                "Acknowledged path {} is outside {}",
                absolute.display(),
                root.display()
            )
        })?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
            || derived_project_path(relative)
            || absolute == project
        {
            continue;
        }
        let represented = absolute.starts_with(source)
            || inputs.files.contains(&absolute)
            || inputs
                .directories
                .iter()
                .any(|directory| absolute.starts_with(directory));
        if represented {
            scopes.push(relative.to_path_buf());
        }
    }
    scopes.sort_by_key(|path| path.components().count());
    let mut collapsed = Vec::<PathBuf>::new();
    for scope in scopes {
        if !collapsed.iter().any(|parent| scope.starts_with(parent)) {
            collapsed.push(scope);
        }
    }
    Ok(collapsed)
}

pub(crate) fn validate_editor_package_links(
    baseline: &ProjectSnapshot,
    current: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> Result<()> {
    for path in service_settings_paths(baseline, current, scopes) {
        if baseline.entries.get(&path) == current.entries.get(&path) {
            continue;
        }
        let before = settings_document(baseline.entries.get(&path))?;
        let mut after = settings_document(current.entries.get(&path))?;
        align_observation_ids_to_baseline(&before, &mut after);
        validate_package_link_documents(&path, &before, &after)?;
    }
    Ok(())
}

pub(crate) fn service_settings_paths(
    baseline: &ProjectSnapshot,
    current: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> Vec<PathBuf> {
    let scopes = ScopeSet::new(scopes);
    let mut paths = baseline
        .entries
        .keys()
        .chain(current.entries.keys())
        .filter(|path| {
            scopes.contains(path)
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_service_settings_file_name)
        })
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn prepare_editor_settings_changes(
    previous: &ProjectSnapshot,
    current: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> Result<HashMap<PathBuf, PreparedEditorSettingsChange>> {
    service_settings_paths(previous, current, scopes)
        .into_iter()
        .filter(|path| previous.entries.get(path) != current.entries.get(path))
        .map(|path| {
            let previous_entry = previous.entries.get(&path);
            let current_entry = current.entries.get(&path);
            if previous_entry.is_some() && current_entry.is_none() {
                bail!("{}", missing_store_message(&path));
            }
            let (previous_document, current_document) = rayon::join(
                || editor_settings_document(previous_entry),
                || {
                    editor_settings_document(current_entry)
                        .with_context(|| format!("Could not read the store {}", path.display()))
                },
            );
            let mut change = PreparedEditorSettingsChange {
                previous: previous_document?,
                current: current_document?,
            };
            crate::settings::equivalence::inherit_workspace_viewport_reference(
                &mut change.current,
                &change.previous,
            );
            validate_package_link_documents(&path, &change.previous, &change.current)?;
            Ok((path, change))
        })
        .collect()
}

pub(crate) fn validate_package_link_documents(
    path: &Path,
    before: &SettingsBytecode,
    after: &SettingsBytecode,
) -> Result<()> {
    let before_links = before
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let after_links = after
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mismatch = before_links
        .iter()
        .find_map(|(id, before_index)| match after_links.get(id).copied() {
            Some(after_index)
                if package_link_instances_equal(before, *before_index, after, after_index) =>
            {
                None
            }
            Some(after_index) => Some(package_link_mismatch_detail(
                id,
                before,
                *before_index,
                after,
                after_index,
            )),
            None if package_parent_is_missing(before, *before_index, after) => None,
            None => Some(format!("PackageLink identity {id} is missing")),
        })
        .or_else(|| {
            after_links
                .keys()
                .find(|id| !before_links.contains_key(*id))
                .map(|id| format!("PackageLink identity {id} was created"))
        });
    if let Some(mismatch) = mismatch {
        bail!(
            "{} changes a PackageLink directly; use the package workflow instead ({mismatch})",
            path.display(),
        );
    }
    Ok(())
}

pub(crate) fn package_link_mismatch_detail(
    id: &str,
    left: &SettingsBytecode,
    left_index: usize,
    right: &SettingsBytecode,
    right_index: usize,
) -> String {
    let left_instance = &left.instances[left_index];
    let right_instance = &right.instances[right_index];
    if left_instance.name != right_instance.name {
        return format!("PackageLink {id} was renamed");
    }
    if left_instance.class_name != right_instance.class_name {
        return format!("PackageLink {id} changed class");
    }
    if settings_parent_id(left, left_index) != settings_parent_id(right, right_index) {
        return format!("PackageLink {id} changed parent");
    }
    if !reconciliation_values_map_equal(&left_instance.attributes, &right_instance.attributes) {
        return format!("PackageLink {id} changed attributes");
    }
    format!("PackageLink {id} changed properties")
}

pub(crate) fn package_link_instances_equal(
    left: &SettingsBytecode,
    left_index: usize,
    right: &SettingsBytecode,
    right_index: usize,
) -> bool {
    let left_instance = &left.instances[left_index];
    let right_instance = &right.instances[right_index];
    let stable_properties_equal = || {
        let stable = |name: &str| name != "ModifiedState";
        left_instance
            .properties
            .iter()
            .filter(|(name, _)| stable(name))
            .all(|(name, value)| {
                right_instance
                    .properties
                    .get(name)
                    .is_none_or(|other| reconciliation_values_equal(value, other, false))
            })
            && right_instance
                .properties
                .iter()
                .filter(|(name, _)| stable(name))
                .all(|(name, value)| {
                    left_instance
                        .properties
                        .get(name)
                        .is_none_or(|other| reconciliation_values_equal(value, other, false))
                })
    };
    left_instance.name == right_instance.name
        && left_instance.class_name == right_instance.class_name
        && settings_parent_id(left, left_index) == settings_parent_id(right, right_index)
        && stable_properties_equal()
        && reconciliation_values_map_equal(&left_instance.attributes, &right_instance.attributes)
}

#[cfg(test)]
pub(crate) fn align_snapshot_ids(
    reference: &ProjectSnapshot,
    observed: &ProjectSnapshot,
) -> Result<ProjectSnapshot> {
    let mut aligned = observed.clone();
    for (path, entry) in &mut aligned.entries {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            continue;
        }
        let Some(reference_entry) = reference.entries.get(path) else {
            continue;
        };
        let reference = settings_document(Some(reference_entry))?;
        let mut observed = settings_document(Some(entry))?;
        if !align_settings_ids_to_reference(&reference, &mut observed) {
            bail!(
                "Failed to align duplicate instance identities in {}",
                path.display()
            );
        }
        *entry = SnapshotEntry::File(encode_settings_bytecode(&observed)?);
    }
    Ok(aligned)
}

pub(crate) fn conflict_message(conflicts: &[String]) -> String {
    let shown = conflicts.iter().take(3).cloned().collect::<Vec<_>>();
    let remainder = &conflicts[shown.len()..];
    if remainder.is_empty() {
        return format!("Sync needs review: {}", shown.join("; "));
    }
    let mut counts = Vec::<(String, usize)>::new();
    for conflict in remainder {
        let subject = conflict
            .split_once(": property ")
            .map(|(_, rest)| ("property", rest))
            .or_else(|| {
                conflict
                    .split_once(": attribute ")
                    .map(|(_, rest)| ("attribute", rest))
            })
            .and_then(|(kind, rest)| {
                rest.split(':')
                    .next()
                    .map(|name| format!("{kind} {}", name.trim()))
            })
            .unwrap_or_else(|| "other".to_string());
        match counts.iter_mut().find(|(name, _)| *name == subject) {
            Some((_, count)) => *count += 1,
            None => counts.push((subject, 1)),
        }
    }
    let breakdown = counts
        .iter()
        .map(|(name, count)| format!("{name} x{count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Sync needs review: {}; and {} more ({breakdown})",
        shown.join("; "),
        remainder.len()
    )
}

pub(crate) fn sync_services() -> Vec<String> {
    DEFAULT_SYNC_SERVICES
        .iter()
        .map(|service| (*service).to_string())
        .collect()
}

pub(crate) fn selected_push_delta_services(
    context: &BoundContext,
    args: &PushEditorChangesArgs,
) -> Result<Option<Vec<String>>> {
    if !args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || !args.target_properties.is_empty()
        || args.upsert_instances_only
    {
        return Ok(None);
    }
    let changed_paths = expand_editor_changed_paths(args)?;
    if !changed_paths.iter().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    }) {
        return Ok(None);
    }
    let root = Path::new(&context.root);
    let mut relative_paths = HashSet::with_capacity(changed_paths.len());
    for path in changed_paths {
        let absolute = absolutize_under(root, &path);
        let Ok(relative) = absolute.strip_prefix(root) else {
            return Ok(Some(sync_services()));
        };
        relative_paths.insert(relative.to_path_buf());
    }
    Ok(Some(services_for_snapshot_paths(context, &relative_paths)))
}
