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

/// An incremental push creates every new instance as its own row, and every
/// engine-set mesh or surface field it has to write (a new MeshPart's MeshId,
/// InitialSize and CollisionFidelity, a SurfaceAppearance's TexturePack, a
/// changed fidelity on an existing part) costs a native engine call that waits
/// on a Studio frame. Past these counts the same end state is reached faster
/// by replacing the service's authored contents through one native import, at
/// the cost of fresh engine identities for its instances. Changed values the
/// plugin can set itself are not counted: they are cheap, and some of them are
/// engine-recomputed fields that compare unequal after every import.
const HEAVY_DELTA_NATIVE_WRITES: usize = 64;
const HEAVY_DELTA_NEW_INSTANCES: usize = 2000;

#[derive(Debug, Default, PartialEq, Eq)]
struct DeltaWeight {
    new_instances: usize,
    native_writes: usize,
    fields: BTreeMap<String, usize>,
}

impl DeltaWeight {
    fn heavy(&self) -> bool {
        self.native_writes >= HEAVY_DELTA_NATIVE_WRITES
            || self.new_instances >= HEAVY_DELTA_NEW_INSTANCES
    }
}

pub(crate) fn promote_heavy_deltas_to_replacement(
    context: &BoundContext,
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    differences: &mut HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<()> {
    // Files-to-Studio rules, mounts that need a stage and linked packages all
    // keep the native import off, and a bootstrap row without one fails the
    // push; those projects keep the incremental path.
    let root = Path::new(&context.root);
    if crate::editor::sync::files_to_studio_filter_rules(root)?.is_some()
        || root.join("renium-link.json").exists()
        || config::try_load_project(None, Some(root))?
            .as_ref()
            .map(config::project_requires_temporary_stage)
            .transpose()?
            .unwrap_or(false)
    {
        return Ok(());
    }
    let mut promoted = Vec::new();
    for (path, change) in prepared.iter_mut() {
        if change.current.instances.is_empty()
            || service_has_only_native_containers(&change.previous)
        {
            continue;
        }
        let weight = delta_weight(&change.current, &change.previous);
        if !weight.heavy() {
            continue;
        }
        // The retained document keeps the active camera's authored ancestors;
        // when those survive, the bootstrap rule would not fire and the planner
        // would run its incremental pass against a truncated previous document.
        let retained = retained_settings_document(&change.previous);
        if !service_has_only_native_containers(&retained) {
            drop_settings_document(retained);
            continue;
        }
        let service = settings_service_name(path, &change.current, &change.previous)?;
        log_global(
            3,
            format_args!(
                "[renium] push replaces {service} natively: {} new instances and {} engine-set fields are too many to apply one at a time ({})",
                weight.new_instances,
                weight.native_writes,
                weight
                    .fields
                    .iter()
                    .map(|(field, count)| format!("{field} {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        drop_settings_document(std::mem::replace(&mut change.previous, retained));
        crate::settings::equivalence::inherit_workspace_viewport_reference(
            &mut change.current,
            &change.previous,
        );
        promoted.push(service);
    }
    if promoted.is_empty() {
        return Ok(());
    }
    promoted.sort();
    // Unchanged script files accompany a native replacement so their external
    // Source markers resolve, as in a full replacement push.
    for path in project_replacement_paths(desired, observed) {
        if services_for_snapshot_paths(context, &HashSet::from([path.clone()]))
            .iter()
            .any(|service| promoted.contains(service))
        {
            differences.insert(path);
        }
    }
    Ok(())
}

fn native_writes_for_new_instance(class_name: &str) -> usize {
    match class_name {
        "MeshPart" => 3,
        "SurfaceAppearance" => 1,
        _ => 0,
    }
}

fn delta_weight(current: &SettingsBytecode, previous: &SettingsBytecode) -> DeltaWeight {
    let previous_by_id = previous
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut weight = DeltaWeight::default();
    for instance in &current.instances {
        if instance.parent_index.is_none() {
            continue;
        }
        let Some(&other) = previous_by_id.get(instance.settings_id.as_str()) else {
            weight.new_instances += 1;
            let writes = native_writes_for_new_instance(&instance.class_name);
            if writes > 0 {
                weight.native_writes += writes;
                *weight
                    .fields
                    .entry(format!("new {}", instance.class_name))
                    .or_default() += writes;
            }
            continue;
        };
        let other = &previous.instances[other];
        // The native import does not carry TexturePack, so after a replacement
        // every existing surface differs until the incremental pass writes it.
        // Counting those would replace the service again on each push instead
        // of letting the writes land; only a new surface counts.
        if instance.class_name == "SurfaceAppearance" {
            continue;
        }
        // The same rules as the comparison: an asset id in another spelling, a
        // hull that encodes the same fidelity or a field the capture cannot
        // know is no write. A saved override that the files dropped is reset
        // to its default through the same native setter.
        for name in instance
            .properties
            .keys()
            .chain(other.properties.keys())
            .collect::<BTreeSet<_>>()
        {
            if !crate::editor::native_roots::is_property(&instance.class_name, name)
                || reconciliation_maps_equal(
                    &instance.class_name,
                    &instance
                        .properties
                        .get(name)
                        .map(|value| Map::from_iter([(name.clone(), value.clone())]))
                        .unwrap_or_default(),
                    &other
                        .properties
                        .get(name)
                        .map(|previous| Map::from_iter([(name.clone(), previous.clone())]))
                        .unwrap_or_default(),
                )
            {
                continue;
            }
            weight.native_writes += 1;
            *weight
                .fields
                .entry(format!("{}.{name}", instance.class_name))
                .or_default() += 1;
        }
    }
    weight
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

#[cfg(test)]
mod heavy_delta_tests {
    use super::*;

    fn document(rows: usize, class_name: &str) -> SettingsBytecode {
        let mut instances = vec![SettingsBytecodeInstance::new(
            "root".into(),
            "ReplicatedStorage".into(),
            "ReplicatedStorage".into(),
            None,
        )];
        for index in 0..rows {
            instances.push(SettingsBytecodeInstance::new(
                format!("editor:{index}"),
                format!("Item{index}"),
                class_name.into(),
                Some(0),
            ));
        }
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        }
    }

    #[test]
    fn new_mesh_parts_or_many_new_instances_promote_a_service() {
        let previous = document(10, "MeshPart");
        assert!(!delta_weight(&document(10, "MeshPart"), &previous).heavy());
        let weight = delta_weight(&document(31, "MeshPart"), &previous);
        assert_eq!((weight.new_instances, weight.native_writes), (21, 63));
        assert_eq!(weight.fields["new MeshPart"], 63);
        assert!(!weight.heavy());
        assert!(delta_weight(&document(32, "MeshPart"), &previous).heavy());
        let empty = document(0, "Folder");
        assert!(!delta_weight(&document(HEAVY_DELTA_NEW_INSTANCES - 1, "Folder"), &empty).heavy());
        assert!(delta_weight(&document(HEAVY_DELTA_NEW_INSTANCES, "Folder"), &empty).heavy());
    }

    #[test]
    fn changed_plugin_settable_values_do_not_promote_but_changed_fidelities_count() {
        let previous = document(100, "MeshPart");
        let mut current = document(100, "MeshPart");
        for instance in current.instances.iter_mut().skip(1) {
            instance
                .properties
                .insert("Anchored".into(), serde_json::Value::Bool(true));
        }
        assert_eq!(delta_weight(&current, &previous), DeltaWeight::default());
        for instance in current.instances.iter_mut().skip(1).take(64) {
            instance.properties.insert(
                "CollisionFidelity".into(),
                serde_json::json!({"_type": "EnumItem", "enumType": "Enum.CollisionFidelity", "name": "Hull", "value": 1}),
            );
        }
        // A fidelity the capture does not carry says nothing about Studio.
        assert_eq!(delta_weight(&current, &previous), DeltaWeight::default());
        let mut previous = previous;
        for instance in previous.instances.iter_mut().skip(1).take(64) {
            instance.properties.insert(
                "CollisionFidelity".into(),
                serde_json::json!({"_type": "EnumItem", "enumType": "Enum.CollisionFidelity", "name": "Box", "value": 3}),
            );
        }
        let weight = delta_weight(&current, &previous);
        assert_eq!(weight.native_writes, 64);
        assert_eq!(weight.new_instances, 0);
        assert!(weight.heavy());
        // A dropped RenderFidelity override is reset through the native setter.
        let mut current = document(100, "MeshPart");
        let mut previous = document(100, "MeshPart");
        let precise = serde_json::json!({"_type": "EnumItem", "enumType": "Enum.RenderFidelity", "name": "Precise", "value": 1});
        for instance in previous.instances.iter_mut().skip(1).take(63) {
            instance
                .properties
                .insert("RenderFidelity".into(), precise.clone());
        }
        assert_eq!(delta_weight(&current, &previous).native_writes, 63);
        assert!(!delta_weight(&current, &previous).heavy());
        previous.instances[64]
            .properties
            .insert("RenderFidelity".into(), precise.clone());
        assert!(delta_weight(&current, &previous).heavy());
        current.instances[1]
            .properties
            .insert("RenderFidelity".into(), precise);
        assert_eq!(delta_weight(&current, &previous).native_writes, 63);
        // A texture pack on an existing surface is written by the incremental pass.
        let mut current = document(70, "SurfaceAppearance");
        let previous = document(70, "SurfaceAppearance");
        for instance in current.instances.iter_mut().skip(1) {
            instance
                .properties
                .insert("TexturePack".into(), serde_json::json!("rbxassetid://1"));
        }
        assert_eq!(delta_weight(&current, &previous), DeltaWeight::default());
        // The same asset in another spelling is no write.
        let mut current = document(1, "MeshPart");
        let mut previous = document(1, "MeshPart");
        current.instances[1]
            .properties
            .insert("MeshId".into(), serde_json::json!("rbxassetid://12345"));
        previous.instances[1].properties.insert(
            "MeshId".into(),
            serde_json::json!("http://www.roblox.com/asset/?id=12345"),
        );
        assert_eq!(delta_weight(&current, &previous), DeltaWeight::default());
    }
}
