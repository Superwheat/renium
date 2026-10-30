use super::*;
use crate::settings::tree::settings_children_by_parent;

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

// A replacement grows with the service: its serialize, import and reference
// scan touch every instance, so a few dozen writes in a huge service stay
// incremental unless they are at least this share of it.
const HEAVY_DELTA_SERVICE_RATIO: usize = 100;

#[derive(Debug, Default, PartialEq, Eq)]
struct DeltaWeight {
    new_instances: usize,
    native_writes: usize,
    /// Engine-set fields the replacement itself must write afterwards, because
    /// Studio's reader does not take them from the payload.
    replacement_writes: usize,
    service_instances: usize,
    fields: BTreeMap<String, usize>,
}

impl DeltaWeight {
    fn heavy(&self) -> bool {
        self.new_instances >= HEAVY_DELTA_NEW_INSTANCES
            || (self.native_writes >= HEAVY_DELTA_NATIVE_WRITES
                && self.native_writes > self.replacement_writes
                && self.native_writes * HEAVY_DELTA_SERVICE_RATIO >= self.service_instances)
    }
}

/// Returns the services it promoted.
/// Every instance of a replaced service is a new object, so references from
/// other services into it must be written again, as after any recreation.
pub(crate) fn extend_replaced_service_references(
    desired: &ProjectSnapshot,
    prepared: &HashMap<PathBuf, PreparedEditorSettingsChange>,
    promoted: &HashSet<String>,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    if promoted.is_empty() {
        return Ok(());
    }
    for (path, change) in prepared {
        if promoted.contains(&settings_service_name(
            path,
            &change.current,
            &change.previous,
        )?) {
            plan.recreated_settings_ids.extend(
                change
                    .current
                    .instances
                    .iter()
                    .filter(|instance| instance.parent_index.is_some())
                    .map(|instance| instance.settings_id.clone()),
            );
        }
    }
    append_recreated_reference_pushes(desired, prepared, plan)?;
    plan.changed_paths.sort();
    plan.changed_paths.dedup();
    plan.target_settings_ids.sort();
    plan.target_settings_ids.dedup();
    Ok(())
}

pub(crate) fn promote_heavy_deltas_to_replacement(
    context: &BoundContext,
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    differences: &mut HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<HashSet<String>> {
    let mut promoted = HashSet::new();
    let mut eligible = None;
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
        let allowed = match eligible {
            Some(allowed) => allowed,
            None => {
                let allowed = native_replacement_eligible(context)?;
                eligible = Some(allowed);
                allowed
            }
        };
        if !allowed {
            break;
        }
        // Package roots at the top of a service are matched by name during the
        // import, which two same-named roots defeat; the incremental path never
        // touches roots it does not change.
        if same_named_roots_share_a_package(&change.current)
            || same_named_roots_share_a_package(&change.previous)
        {
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
        promoted.insert(service);
    }
    if promoted.is_empty() {
        return Ok(promoted);
    }
    // Unchanged script files accompany a native replacement so their external
    // Source markers resolve, as in a full replacement push.
    let root = Path::new(&context.root);
    let source = Path::new(&context.source)
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    for path in project_replacement_paths(desired, observed) {
        let service = crate::project::storage::service_for_store(
            Path::new(&context.source),
            &root.join(&path),
        )
        .or_else(|| {
            path.strip_prefix(&source).ok().and_then(|relative| {
                relative
                    .components()
                    .next()
                    .and_then(|component| match component {
                        Component::Normal(name) => name.to_str().map(str::to_string),
                        _ => None,
                    })
            })
        });
        let in_promoted = match service {
            Some(service) => promoted.contains(&service),
            None => services_for_snapshot_paths(context, &HashSet::from([path.clone()]))
                .iter()
                .any(|service| promoted.contains(service)),
        };
        if in_promoted {
            differences.insert(path);
        }
    }
    Ok(promoted)
}

/// Two top-level roots of one name and class where one holds a package link.
fn same_named_roots_share_a_package(document: &SettingsBytecode) -> bool {
    let Some(root) = document
        .instances
        .iter()
        .position(|instance| instance.parent_index.is_none())
    else {
        return false;
    };
    let children = settings_children_by_parent(document);
    let mut groups: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for &index in children.get(root).map(Vec::as_slice).unwrap_or_default() {
        let instance = &document.instances[index];
        groups
            .entry((instance.name.as_str(), instance.class_name.as_str()))
            .or_default()
            .push(index);
    }
    groups.values().any(|members| {
        members.len() > 1
            && members.iter().any(|&member| {
                children.get(member).is_some_and(|kids| {
                    kids.iter()
                        .any(|&kid| document.instances[kid].class_name == "PackageLink")
                })
            })
    })
}

/// Files-to-Studio rules, a root `$ignoreUnknownInstances`, mounts that need
/// a stage and linked packages all keep the native import off, and a
/// bootstrap row without one fails the push; those projects stay incremental.
fn native_replacement_eligible(context: &BoundContext) -> Result<bool> {
    let root = Path::new(&context.root);
    if crate::editor::sync::files_to_studio_filter_rules(root)?.is_some()
        || root.join("renium-link.json").exists()
    {
        return Ok(false);
    }
    let Some(project) = config::try_load_project(None, Some(root))? else {
        return Ok(true);
    };
    Ok(!project
        .project
        .root
        .ignore_unknown_instances
        .unwrap_or(false)
        && !config::project_requires_temporary_stage(&project)?)
}

fn native_writes_for_new_instance(instance: &SettingsBytecodeInstance) -> usize {
    let saved = instance
        .properties
        .keys()
        .filter(|name| crate::editor::native_roots::is_property(&instance.class_name, name))
        .count();
    let class = match instance.class_name.as_str() {
        "MeshPart" => 3,
        "SurfaceAppearance" => 1,
        _ => 0,
    };
    saved.max(class)
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
        weight.service_instances += 1;
        if instance.class_name == "SurfaceAppearance"
            && instance
                .properties
                .contains_key(crate::roblox::schema::TEXTURE_PACK_PROPERTY)
        {
            weight.replacement_writes += 1;
        }
        let Some(&other) = previous_by_id.get(instance.settings_id.as_str()) else {
            weight.new_instances += 1;
            let writes = native_writes_for_new_instance(instance);
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
    fn only_same_named_package_roots_block_a_promotion() {
        let mut document = document(3, "Model");
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:link".into(),
            "PackageLink".into(),
            "PackageLink".into(),
            Some(2),
        ));
        assert!(!same_named_roots_share_a_package(&document));
        document.instances[3].name = "Item1".into();
        assert!(same_named_roots_share_a_package(&document));
        document.instances.pop();
        assert!(!same_named_roots_share_a_package(&document));
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
        assert_eq!(delta_weight(&current, &previous).native_writes, 0);
        for instance in current.instances.iter_mut().skip(1).take(64) {
            instance.properties.insert(
                "CollisionFidelity".into(),
                serde_json::json!({"_type": "EnumItem", "enumType": "Enum.CollisionFidelity", "name": "Hull", "value": 1}),
            );
        }
        // A fidelity the capture does not carry says nothing about Studio.
        assert_eq!(delta_weight(&current, &previous).native_writes, 0);
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
        // A changed texture pack on an existing surface is one native write; a
        // capture without the field says nothing about it.
        let mut current = document(70, "SurfaceAppearance");
        let mut previous = document(70, "SurfaceAppearance");
        for instance in current.instances.iter_mut().skip(1) {
            instance
                .properties
                .insert("TexturePack".into(), serde_json::json!("rbxassetid://1"));
        }
        assert_eq!(delta_weight(&current, &previous).native_writes, 0);
        for instance in previous.instances.iter_mut().skip(1) {
            instance
                .properties
                .insert("TexturePack".into(), serde_json::json!("rbxassetid://2"));
        }
        let weight = delta_weight(&current, &previous);
        assert_eq!(weight.native_writes, 70);
        // A replacement would write the same 70 packs afterwards, so it buys nothing.
        assert_eq!(weight.replacement_writes, 70);
        assert!(!weight.heavy());
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
        assert_eq!(delta_weight(&current, &previous).native_writes, 0);
    }
}
