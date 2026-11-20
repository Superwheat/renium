use super::*;
use crate::settings::bytecode::blocks::store_payload_len;
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

#[cfg(test)]
pub(crate) fn prepare_project_replacement(
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<HashSet<PathBuf>> {
    prepare_project_replacement_from(desired, observed, paths, prepared, StoresRead::Changed)
}

/// How much of the two copies of a store a push reads.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum StoresRead {
    /// Both whole, as a store whose copies differ in more than a few
    /// instances is read.
    Whole,
    /// The files' whole, and of Studio's the instances that differ.
    Changed,
    /// Of both the instances that differ: for a push whose plan sends
    /// nothing but values of existing instances (`plan_reads_changed_alone`).
    ChangedOfBoth,
}

pub(crate) fn prepare_project_replacement_from(
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
    read: StoresRead,
) -> Result<HashSet<PathBuf>> {
    prepare_project_replacement_importing(desired, observed, paths, prepared, read, None)
}

/// The stores a push imports anew although Studio holds their instances
/// (`imported_store`).
pub(crate) type ImportedStores = std::sync::Mutex<HashSet<PathBuf>>;

/// The services of the stores a push imports anew. Every instance of such a
/// service is a new object, as in one `promote_heavy_deltas_to_replacement`
/// replaces.
pub(crate) fn imported_services(
    imported: ImportedStores,
    prepared: &HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<HashSet<String>> {
    imported
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|path| Some((path, prepared.get(path)?)))
        .map(|(path, change)| settings_service_name(path, &change.current, &change.previous))
        .collect()
}

/// `prepare_project_replacement_from` for a project whose stores may be
/// imported anew where most of their instances differ: `imported` takes the
/// paths of those.
pub(crate) fn prepare_project_replacement_importing(
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
    read: StoresRead,
    imported: Option<&ImportedStores>,
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
            let step_started = std::time::Instant::now();
            let changed = if read == StoresRead::Whole {
                ChangedInstances::Whole(None)
            } else {
                changed_instances_alone(
                    desired.entries.get(path),
                    observed.entries.get(path),
                    read == StoresRead::ChangedOfBoth,
                    imported.is_some(),
                )?
            };
            let (decoded, bare) = match changed {
                ChangedInstances::None => return Ok((path.clone(), None)),
                ChangedInstances::Prepared(change) => {
                    return Ok((path.clone(), Some(*change)));
                }
                ChangedInstances::Whole(None) if read != StoresRead::Whole => (
                    None,
                    replaced_store_alone(desired.entries.get(path), observed.entries.get(path))?,
                ),
                ChangedInstances::Whole(current) => (current, None),
                ChangedInstances::Imported(copies) => {
                    if let Some(imported) = imported {
                        imported
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(path.clone());
                    }
                    (None, Some(*copies))
                }
            };
            let (current, previous) = match bare {
                Some((current, previous)) => (anyhow::Ok(current), anyhow::Ok(previous)),
                None => rayon::join(
                    || match decoded {
                        Some(current) => Ok(current),
                        None => settings_document(desired.entries.get(path)),
                    },
                    || settings_document(observed.entries.get(path)),
                ),
            };
            let mut current = current?;
            let previous = previous?;
            let stated = states_viewport(&current);
            let instances = current.instances.len().max(previous.instances.len());
            let step = |name: &str, started: std::time::Instant| {
                if instances >= 4_096 {
                    crate::app::timing::log_timing(
                        &format!("replacement preparation of {instances} instances: {name}"),
                        started,
                    );
                }
                std::time::Instant::now()
            };
            let step_started = step("decode", step_started);
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
                read_where_paired(&mut current, &retained, desired.entries.get(path))?;
                crate::settings::equivalence::inherit_workspace_viewport_reference(
                    &mut current,
                    &retained,
                );
                retained
            } else {
                let mut previous = previous;
                let step_started = step("comparison by position", step_started);
                if !align_settings_ids_to_reference(&current, &mut previous) {
                    bail!(
                        "Could not align existing Studio objects in {}",
                        path.display()
                    );
                }
                read_where_paired(&mut current, &previous, desired.entries.get(path))?;
                let step_started = step("identity matching", step_started);
                crate::settings::equivalence::inherit_workspace_viewport_reference(
                    &mut current,
                    &previous,
                );
                let equivalent = settings_documents_equivalent(&current, &previous);
                let step_started = step("comparison", step_started);
                if equivalent {
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
                step("search for a reusable instance", step_started);
                if reusable || service_has_only_native_containers(&current) {
                    previous
                } else {
                    let retained = retained_settings_document(&previous);
                    drop_settings_document(previous);
                    retained
                }
            };
            // Instances stay in their store file only for the import of a
            // whole service, which reads that file: every other push creates
            // the files' instances one by one from what each holds, and a
            // store that took its viewport camera from Studio is another
            // document than its file gives.
            if crate::settings::equivalence::holds_unread_instances(&current)
                && (!service_has_only_native_containers(&previous)
                    || !stated && states_viewport(&current))
                && let Some(SnapshotEntry::File(bytes)) = desired.entries.get(path)
            {
                crate::settings::equivalence::read_unread_instances(&mut current, bytes)?;
            }
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

/// Whether a Workspace store names the camera of its viewport.
fn states_viewport(document: &SettingsBytecode) -> bool {
    document.instances.iter().any(|instance| {
        instance.parent_index.is_none()
            && instance.class_name == "Workspace"
            && instance.properties.contains_key("CurrentCamera")
    })
}

/// Whether a plan sends nothing but values of instances both copies hold:
/// nothing is created anew, removed or imported, so no instance beside the
/// ones that differ, their ancestors and their namesakes is read to carry
/// it out. Any other plan is made from the files' whole copies.
pub(crate) fn plan_reads_changed_alone(plan: &ReconcilePushPlan) -> bool {
    plan.initial_native_services.is_empty()
        && plan.recreated_settings_ids.is_empty()
        && plan.recreated_instances.is_empty()
        && plan.previous_class_names.is_empty()
        && plan.instance_deletes.is_empty()
}

/// What `changed_instances_alone` makes of two copies of a store.
enum ChangedInstances {
    /// The two hold the same.
    None,
    Prepared(Box<PreparedEditorSettingsChange>),
    /// The whole documents have to say, with the files' where it is decoded.
    Whole(Option<SettingsBytecode>),
    /// The store is imported anew (`imported_store`): the files' copy
    /// beside what Studio keeps of its own.
    Imported(Box<(SettingsBytecode, SettingsBytecode)>),
}

/// A store file smaller than this is read whole: two documents of it are
/// small.
const CHANGED_INSTANCES_MIN_BYTES: usize = if cfg!(test) { 1 } else { 256 * 1024 };

/// The most instances read from the store Studio holds; past that the whole
/// documents are compared, or the store is imported anew.
const MAX_CHANGED_INSTANCES: usize = 16_384;

/// The instances of each copy `told_apart` holds whole at a time: one can
/// hold a mesh's cooked data.
const COMPARED_AT_ONCE: usize = if cfg!(test) { 64 } else { 1_024 };

/// A copy of a store read by some of its instances: every other one bare
/// and standing for its counterpart of the other copy.
fn sparse_document(
    instances: Vec<SettingsBytecodeInstance>,
    read: &[usize],
    rows: Vec<SettingsBytecodeInstance>,
    version: u8,
) -> Result<SettingsBytecode> {
    let mut document = SettingsBytecode { version, instances };
    for instance in &mut document.instances {
        crate::settings::held::stand_unchanged(&mut instance.properties);
    }
    for (index, mut row) in read.iter().zip(rows) {
        for value in row.properties.values_mut() {
            *value = crate::settings::held::held(std::mem::take(value));
        }
        document.instances[*index] = row;
    }
    crate::settings::equivalence::stabilize_settings_reference_ids(&mut document);
    canonicalize_settings_property_names(&mut document)?;
    Ok(document)
}

/// Tells the instances that differ in what they hold from the ones whose
/// rows two copies of a store only write another way: `differs` keeps each
/// of `differing` that holds other values, and their count is the answer. A
/// capture can write a value the other copy leaves out, in every row of a
/// class. Once more than `MAX_CHANGED_INSTANCES` differ the store is not
/// compared instance by instance, so the rest are left as they were, and so
/// are all of a copy that cannot be read by its rows.
fn told_apart(
    (desired_bytes, observed_bytes): (&[u8], &[u8]),
    differing: &[usize],
    differs: &mut [bool],
) -> Result<usize> {
    use crate::settings::bytecode::property_edit::decode_for_property_edit;
    use crate::settings::equivalence::canonicalize_instances;

    let _span = crate::app::profile::span("store rows told apart");
    let (Ok((mut files, mut files_rows)), Ok((mut studio, mut studio_rows))) = rayon::join(
        || decode_for_property_edit(desired_bytes),
        || decode_for_property_edit(observed_bytes),
    ) else {
        return Ok(differing.len());
    };
    let whole = |document: &mut SettingsBytecode, rows: &[usize]| {
        let mut instances = rows
            .iter()
            .map(|row| {
                let instance = &mut document.instances[*row];
                SettingsBytecodeInstance {
                    properties: std::mem::take(&mut instance.properties),
                    attributes: std::mem::take(&mut instance.attributes),
                    ..instance.bare()
                }
            })
            .collect::<Vec<_>>();
        canonicalize_instances(&mut instances)?;
        anyhow::Ok(instances)
    };
    let (mut unlike, mut compared) = (0, 0);
    for rows in differing.chunks(COMPARED_AT_ONCE) {
        if unlike > MAX_CHANGED_INSTANCES {
            break;
        }
        let (read_files, read_studio) = rayon::join(
            || files_rows.materialize(&mut files, rows),
            || studio_rows.materialize(&mut studio, rows),
        );
        if read_files.is_err() || read_studio.is_err() {
            break;
        }
        let (of_files, of_studio) = (whole(&mut files, rows)?, whole(&mut studio, rows)?);
        let differ = of_files
            .par_iter()
            .zip(&of_studio)
            .map(|(files, studio)| !instance_values_equal(None, files, studio))
            .collect::<Vec<_>>();
        for (row, differ) in rows.iter().zip(differ) {
            differs[*row] = differ;
            unlike += usize::from(differ);
        }
        compared += rows.len();
    }
    Ok(unlike + differing.len() - compared)
}

/// The instances of a store a push reads whole, of `instances` at the same
/// places in both copies: the ones that differ or name another by its id,
/// and what a comparison reads of an instance whatever it holds, the roots
/// and the cameras.
fn instances_read(
    instances: &[SettingsBytecodeInstance],
    (differs, named): (&[bool], &[bool]),
    paths_too: bool,
) -> Vec<usize> {
    let count = instances.len();
    let mut read = (0..count)
        .map(|index| {
            differs[index]
                || named[index]
                || instances[index].parent_index.is_none()
                || instances[index].class_name == "Camera"
        })
        .collect::<Vec<_>>();
    if paths_too {
        // A request names an instance by its path, and each instance on that
        // path by what tells it from its namesakes. A store can hold a
        // parent after its child, so each path is walked to its root.
        for index in 0..count {
            if !read[index] {
                continue;
            }
            let mut parent = instances[index].parent_index;
            let mut steps = 0;
            while let Some(at) = parent.filter(|at| *at < count && !read[*at]) {
                read[at] = true;
                parent = instances[at].parent_index;
                steps += 1;
                if steps > count {
                    break;
                }
            }
        }
    }
    // A request that names one of several namesakes tells it from the others
    // by what each holds, so the namesakes of an instance that is read are
    // read with it.
    let namesakes = (0..count)
        .filter(|&index| read[index])
        .map(|index| {
            (
                instances[index].parent_index,
                instances[index].name.as_str(),
            )
        })
        .collect::<HashSet<_>>();
    (0..count)
        .filter(|&index| {
            namesakes.contains(&(
                instances[index].parent_index,
                instances[index].name.as_str(),
            ))
        })
        .collect()
}

/// The two copies of a store that holds the same instances at the same
/// places where more than `MAX_CHANGED_INSTANCES` of them differ in what
/// they hold. Sending that many instances one by one costs Studio and the
/// daemon far more than one import of the store, so the store is prepared
/// as one Studio holds nothing of: the files' copy as `replaced_store_alone`
/// reads it, beside the instances of Studio's copy that an import keeps.
/// `studio` are the instances of that copy, bare. `None` where the import
/// cannot place a camera or a package root, as
/// `promote_heavy_deltas_to_replacement` asks.
fn imported_store(
    (desired_bytes, observed_bytes): (&[u8], &[u8]),
    studio: Vec<SettingsBytecodeInstance>,
) -> Result<Option<(SettingsBytecode, SettingsBytecode)>> {
    let _span = crate::app::profile::span("store prepared for import");
    // What an import keeps of Studio's copy: its roots, the engine's
    // containers under them and the camera of the viewport.
    let kept = (0..studio.len())
        .filter(|&index| {
            studio[index].parent_index.is_none_or(|parent| {
                studio.get(parent).is_some_and(|parent| {
                    parent.parent_index.is_none()
                        && (studio[index].class_name == "Camera"
                            || crate::roblox::services::is_engine_managed_container(
                                &parent.class_name,
                                &studio[index].class_name,
                            ))
                })
            })
        })
        .collect::<Vec<_>>();
    let Ok((rows, _)) =
        crate::settings::bytecode::property_edit::decode_rows(observed_bytes, &kept)
    else {
        return Ok(None);
    };
    let studio = sparse_document(studio, &kept, rows, SETTINGS_BINARY_VERSION)?;
    let retained = retained_settings_document(&studio);
    if !service_has_only_native_containers(&retained)
        || same_named_roots_share_a_package(&studio)
        || retained
            .instances
            .iter()
            .any(|instance| crate::settings::held::stands_unchanged(&instance.properties))
    {
        return Ok(None);
    }
    drop(studio);
    replaced_store_beside(desired_bytes, retained)
}

/// The change of a store whose two copies differ in the values of a few
/// instances, without the whole document of the store Studio holds: the
/// digests of the two files say which instances differ, and those, their
/// ancestors, the namesakes of all of them, the roots and the cameras alone
/// are read from Studio's. Every other instance of that document stands for
/// the files' instance at its place (`held::stand_unchanged`) and keeps its
/// UniqueId alone. This is taken only where the two copies hold the same
/// instances at the same places and the alignment of the whole documents
/// would leave each of them there, whatever ids Studio gave them.
///
/// With `files_too` the files' copy is read the same way: a push that sends
/// nothing but values of existing instances reads no other instance of it.
fn changed_instances_alone(
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    files_too: bool,
    imports_heavy: bool,
) -> Result<ChangedInstances> {
    use crate::settings::bytecode::property_edit::{
        StoreRows, decode_rows, names_an_instance_by_id, whole_row_digests,
        whole_row_digests_of_both,
    };
    use crate::settings::equivalence::{
        align_settings_ids_by_place, inherit_workspace_viewport_reference,
        stabilize_settings_reference_ids,
    };

    let (Some(SnapshotEntry::File(desired_bytes)), Some(SnapshotEntry::File(observed_bytes))) =
        (desired, observed)
    else {
        return Ok(ChangedInstances::Whole(None));
    };
    // A store Studio holds nearly empty is replaced, not compared.
    let (desired_len, observed_len) = (
        store_payload_len(desired_bytes),
        store_payload_len(observed_bytes),
    );
    if desired_bytes.len() < CHANGED_INSTANCES_MIN_BYTES
        || observed_len.saturating_mul(4) < desired_len
        || desired_len.saturating_mul(4) < observed_len
    {
        return Ok(ChangedInstances::Whole(None));
    }
    // What is known of the files' copy: its instances whole, or their
    // digests with the instances bare.
    enum Files {
        Whole(SettingsBytecode, Vec<[u8; 16]>, Vec<bool>),
        Rows(StoreRows),
    }
    let (observed_rows, files) = if files_too {
        let (files, observed_rows) = whole_row_digests_of_both(desired_bytes, observed_bytes);
        (observed_rows, Ok(files.ok().flatten().map(Files::Rows)))
    } else {
        rayon::join(
            || whole_row_digests(observed_bytes, |_| Some((None, None))),
            || -> Result<Option<Files>> {
                // The files' copy as `settings_document` gives it, with the
                // digests of its instances taken as they were decoded.
                let mut current =
                    crate::settings::bytecode::decode_settings_bytecode_holding(desired_bytes)?;
                let digests = crate::settings::bytecode::whole_instance_digests(&current)
                    .into_iter()
                    .map(|(digest, _)| digest)
                    .collect();
                let named = current
                    .instances
                    .par_iter()
                    .map(|instance| {
                        instance
                            .properties
                            .values()
                            .chain(instance.attributes.values())
                            .any(names_an_instance_by_id)
                    })
                    .collect::<Vec<_>>();
                stabilize_settings_reference_ids(&mut current);
                canonicalize_settings_property_names(&mut current)?;
                Ok(Some(Files::Whole(current, digests, named)))
            },
        )
    };
    let Some(files) = files? else {
        return Ok(ChangedInstances::Whole(None));
    };
    // A store the digests cannot read is the whole decode's to refuse.
    let Some(observed_rows) = observed_rows.ok().flatten() else {
        return Ok(ChangedInstances::Whole(match files {
            Files::Whole(current, ..) => Some(current),
            Files::Rows(_) => None,
        }));
    };
    let skeleton = observed_rows.instances;
    let (files_instances, files_digests, files_named): (
        &[SettingsBytecodeInstance],
        &[[u8; 16]],
        Vec<bool>,
    ) = match &files {
        Files::Whole(current, digests, named) => (&current.instances, digests, named.clone()),
        Files::Rows(rows) => {
            let mut named = vec![false; rows.instances.len()];
            for index in &rows.named {
                if let Some(named) = named.get_mut(*index) {
                    *named = true;
                }
            }
            (&rows.instances, &rows.digests, named)
        }
    };
    let count = files_instances.len();
    let same_places = skeleton.len() == count
        && files_digests.len() == count
        && observed_rows.digests.len() == count
        && files_instances
            .iter()
            .zip(&skeleton)
            .all(|(files, studio)| {
                files.name == studio.name
                    && files.class_name == studio.class_name
                    && files.parent_index == studio.parent_index
            });
    let hand_over = |files: Files| {
        Ok(ChangedInstances::Whole(match files {
            Files::Whole(current, ..) => Some(current),
            Files::Rows(_) => None,
        }))
    };
    if !same_places {
        return hand_over(files);
    }
    let mut differs = (0..count)
        .map(|index| files_digests[index] != observed_rows.digests[index])
        .collect::<Vec<_>>();
    let mut read = instances_read(&skeleton, (&differs, &files_named), files_too);
    if read.len() > MAX_CHANGED_INSTANCES {
        // The rows that only write the same values another way are not read.
        let mut names = files_named.clone();
        for index in &observed_rows.named {
            if let Some(names) = names.get_mut(*index) {
                *names = true;
            }
        }
        let differing = (0..count)
            .filter(|&index| differs[index] && !names[index])
            .collect::<Vec<_>>();
        let copies = (desired_bytes.as_slice(), observed_bytes.as_slice());
        let unlike = told_apart(copies, &differing, &mut differs)?;
        if unlike > MAX_CHANGED_INSTANCES && imports_heavy {
            // Neither copy is whole beside the ones the import reads.
            drop(files);
            let copies = imported_store(copies, skeleton)?;
            if copies.is_some() {
                crate::app::output::log_global(
                    3,
                    format_args!(
                        "[renium] push imports a store of {count} instances anew: most of them differ"
                    ),
                );
            }
            return Ok(copies.map_or(ChangedInstances::Whole(None), |copies| {
                ChangedInstances::Imported(Box::new(copies))
            }));
        }
        read = instances_read(&skeleton, (&differs, &files_named), files_too);
    }
    if read.len() > MAX_CHANGED_INSTANCES || read.len() == count {
        return hand_over(files);
    }
    let Ok((rows, _)) = decode_rows(observed_bytes, &read) else {
        return hand_over(files);
    };
    let sparse = |instances, rows, version| sparse_document(instances, &read, rows, version);
    let mut current = match files {
        Files::Whole(current, ..) => current,
        Files::Rows(files) => {
            let Ok((rows, _)) = decode_rows(desired_bytes, &read) else {
                return Ok(ChangedInstances::Whole(None));
            };
            sparse(files.instances, rows, SETTINGS_BINARY_VERSION)?
        }
    };
    let mut previous = sparse(skeleton, rows, current.version)?;
    let hand_over =
        |current: SettingsBytecode| Ok(ChangedInstances::Whole((!files_too).then_some(current)));
    if settings_documents_positionally_equivalent(&current, &previous) {
        return Ok(ChangedInstances::None);
    }
    if !align_settings_ids_by_place(&current, &mut previous) {
        return hand_over(current);
    }
    let reusable = previous
        .instances
        .iter()
        .enumerate()
        .any(|(index, instance)| {
            instance.parent_index.is_some()
                && crate::settings::held::stands_unchanged(&instance.properties)
                && !is_protected_engine_container(&previous, index)
        });
    if !reusable || service_has_only_native_containers(&previous) {
        return hand_over(current);
    }
    inherit_workspace_viewport_reference(&mut current, &previous);
    if settings_documents_equivalent(&current, &previous) {
        return Ok(ChangedInstances::None);
    }
    // A delta heavy enough to replace the service retains what Studio holds.
    if delta_weight(&current, &previous).heavy() {
        return hand_over(current);
    }
    Ok(ChangedInstances::Prepared(Box::new(
        PreparedEditorSettingsChange { previous, current },
    )))
}

/// The two copies of a large store of which Studio holds little: Studio's
/// whole, and the files' with only the instances a push reads beside the
/// import that carries every other one from the store file to Studio. Those
/// are the roots, the instances Studio's can be matched with (by an id, a
/// UniqueId, or a name and class), the engine's containers, the cameras and
/// the scripts, with the instances on their paths and the namesakes of all
/// of them, and the models. Every other instance stands for its row of the store file
/// (`held::stand_unchanged`) with its UniqueId alone, until
/// `equivalence::read_unread_instances` or `read_instances` reads it. `None`
/// where the whole documents are prepared.
fn replaced_store_alone(
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
) -> Result<Option<(SettingsBytecode, SettingsBytecode)>> {
    let Some(SnapshotEntry::File(desired_bytes)) = desired else {
        return Ok(None);
    };
    let observed_bytes = match observed {
        Some(SnapshotEntry::File(bytes)) => store_payload_len(bytes),
        None => 0,
        Some(_) => return Ok(None),
    };
    if desired_bytes.len() < CHANGED_INSTANCES_MIN_BYTES
        || !cfg!(test) && observed_bytes.saturating_mul(4) >= store_payload_len(desired_bytes)
    {
        return Ok(None);
    }
    replaced_store_beside(desired_bytes, settings_document(observed)?)
}

/// `replaced_store_alone` for the files' store beside what Studio holds, or
/// keeps, of its own.
fn replaced_store_beside(
    desired_bytes: &[u8],
    previous: SettingsBytecode,
) -> Result<Option<(SettingsBytecode, SettingsBytecode)>> {
    use crate::settings::bytecode::property_edit::decode_for_row_chunks;
    use crate::settings::equivalence::stabilize_settings_reference_ids;

    // Every instance with its UniqueId alone, which says where Studio's
    // stand among them. A store this reader refuses is the whole decode's to
    // refuse.
    let Ok((structure, columns)) = decode_for_row_chunks(desired_bytes) else {
        return Ok(None);
    };
    let mut current = structure;
    if columns
        .project_properties(
            &mut current,
            |name| name == "UniqueId",
            |_, _, value| Some(value),
        )
        .is_err()
    {
        return Ok(None);
    }
    let skeleton = &current.instances;
    let count = skeleton.len();
    // The identities of a store this much smaller are matched with the
    // instances of the files that share an id or a name and class with one
    // of them, or its UniqueId.
    if previous.instances.len() >= count / 4 {
        return Ok(None);
    }
    let ids = previous
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let named = previous
        .instances
        .iter()
        .map(|instance| (instance.name.as_str(), instance.class_name.as_str()))
        .collect::<HashSet<_>>();
    let unique_ids = previous
        .instances
        .iter()
        .filter_map(|instance| instance.properties.get("UniqueId"))
        .collect::<Vec<_>>();
    let mut read = skeleton
        .iter()
        .map(|instance| {
            let Some(parent) = instance.parent_index else {
                return true;
            };
            ids.contains(instance.settings_id.as_str())
                || named.contains(&(instance.name.as_str(), instance.class_name.as_str()))
                || instance
                    .properties
                    .get("UniqueId")
                    .is_some_and(|unique_id| unique_ids.contains(&unique_id))
                || instance.class_name == "Camera"
                || crate::editor::sync::is_lua_source_class(&instance.class_name)
                || skeleton.get(parent).is_some_and(|parent| {
                    parent.parent_index.is_none()
                        && crate::roblox::services::is_engine_managed_container(
                            &parent.class_name,
                            &instance.class_name,
                        )
                })
        })
        .collect::<Vec<_>>();
    for index in 0..count {
        if !read[index] {
            continue;
        }
        let mut parent = skeleton[index].parent_index;
        let mut steps = 0;
        while let Some(at) = parent.filter(|at| *at < count && !read[*at]) {
            read[at] = true;
            parent = skeleton[at].parent_index;
            steps += 1;
            if steps > count {
                break;
            }
        }
    }
    let namesakes = (0..count)
        .filter(|&index| read[index])
        .map(|index| (skeleton[index].parent_index, skeleton[index].name.as_str()))
        .collect::<HashSet<_>>();
    // The import returns the rows for all post-import fields, including pivots.
    let read = (0..count)
        .filter(|&index| {
            namesakes.contains(&(skeleton[index].parent_index, skeleton[index].name.as_str()))
        })
        .collect::<Vec<_>>();
    drop(namesakes);
    if read.len() == count {
        return Ok(None);
    }
    let mut wanted = vec![false; count];
    for index in read {
        wanted[index] = true;
    }
    if columns.materialize_selected(&mut current, &wanted).is_err() {
        return Ok(None);
    }
    for (instance, wanted) in current.instances.iter_mut().zip(wanted) {
        if !wanted {
            crate::settings::held::stand_unchanged(&mut instance.properties);
        }
    }
    stabilize_settings_reference_ids(&mut current);
    canonicalize_settings_property_names(&mut current)?;
    Ok(Some((current, previous)))
}

/// Reads the instances the files' copy left in its store file where one of
/// them shares its id with an instance of Studio's copy: the two are
/// compared, and an instance that stands for its row compares equal to
/// anything.
fn read_where_paired(
    current: &mut SettingsBytecode,
    previous: &SettingsBytecode,
    store: Option<&SnapshotEntry>,
) -> Result<()> {
    let unread = current
        .instances
        .iter()
        .filter(|instance| crate::settings::held::stands_unchanged(&instance.properties))
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let paired = !unread.is_empty()
        && previous
            .instances
            .iter()
            .any(|instance| unread.contains(instance.settings_id.as_str()));
    drop(unread);
    if paired && let Some(SnapshotEntry::File(bytes)) = store {
        crate::settings::equivalence::read_unread_instances(current, bytes)?;
    }
    Ok(())
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
        if crate::settings::held::stands_unchanged(&other.properties) {
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
        // Studio can reissue a texture pack without an edit; neither two
        // issued IDs nor a capture without the field requires a write.
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
        assert_eq!(weight.native_writes, 0);
        // A replacement would still have to write all 70 packs afterwards.
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
