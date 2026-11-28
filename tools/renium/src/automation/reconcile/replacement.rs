use super::*;
use crate::settings::bytecode::blocks::store_payload_len;
use crate::settings::bytecode::property_edit::Moved;
use crate::settings::equivalence::{ByOrder, run_is_told};
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

/// Stores whose entire difference is eligible plain scriptable properties and
/// can be pushed directly through the compact columnar bulk property patch.
pub(crate) type BulkPropertyPatches =
    std::sync::Mutex<HashMap<PathBuf, crate::editor::types::EditorBulkPropertyPatch>>;

/// Extracts the Roblox service name from a service settings path, supporting both
/// `instances/<Service>.renium` and `src/<Service>/__roblox_sync_settings.renium`.
pub(crate) fn settings_path_service_name(path: &Path) -> Option<&str> {
    let stem = path.file_stem()?.to_str()?;
    if stem != "__roblox_sync_settings" {
        return Some(stem);
    }
    path.parent()?.file_name()?.to_str()
}

/// Minimum number of changed instances before switching from ordinary per-row
/// JSON property edits to the compact columnar bulk plain-property path.
pub(crate) const BULK_PROPERTY_MIN_INSTANCES: usize = 64;

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
    prepare_project_replacement_with_bulk(desired, observed, paths, prepared, read, imported, None)
}

pub(crate) fn prepare_project_replacement_with_bulk(
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
    read: StoresRead,
    imported: Option<&ImportedStores>,
    bulk: Option<&BulkPropertyPatches>,
) -> Result<HashSet<PathBuf>> {
    enum PreparedSlot {
        Unchanged,
        Bulk,
        Prepared(PreparedEditorSettingsChange),
    }

    let has_changed_sources = paths.iter().any(|path| {
        super::merge::is_source_path(path)
            && desired.entries.get(path) != observed.entries.get(path)
    });
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
                return Ok((path.clone(), PreparedSlot::Unchanged));
            }
            if let Some(bulk_patches) = bulk
                && read != StoresRead::Whole
                && let (
                    Some(SnapshotEntry::File(desired_bytes)),
                    Some(SnapshotEntry::File(observed_bytes)),
                ) = (desired.entries.get(path), observed.entries.get(path))
                && let Some(service_name) = settings_path_service_name(path)
            {
                let bulk_started = std::time::Instant::now();
                match crate::settings::bytecode::blocks::bulk_diff::diff_v12_bulk_plain_properties(
                    desired_bytes,
                    observed_bytes,
                    service_name,
                    BULK_PROPERTY_MIN_INSTANCES,
                )? {
                    crate::settings::bytecode::blocks::bulk_diff::BulkDiffOutcome::Unchanged => {
                        return Ok((path.clone(), PreparedSlot::Unchanged));
                    }
                    crate::settings::bytecode::blocks::bulk_diff::BulkDiffOutcome::Patch(patch) => {
                        crate::app::timing::log_timing(
                            &format!(
                                "columnar bulk property diff ({} instances, {} fields, {} tree nodes)",
                                patch.instance_count,
                                patch.field_count,
                                patch.nodes.len()
                            ),
                            bulk_started,
                        );
                        bulk_patches
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(path.clone(), patch);
                        return Ok((path.clone(), PreparedSlot::Bulk));
                    }
                    crate::settings::bytecode::blocks::bulk_diff::BulkDiffOutcome::Hybrid(
                        hybrid,
                    ) => {
                        let patch_instances = hybrid.patch.instance_count;
                        let patch_fields = hybrid.patch.field_count;
                        let patch_nodes = hybrid.patch.nodes.len();
                        let extra_count =
                            hybrid.extra_desired_rows.len() + hybrid.extra_observed_rows.len();
                        let patch = hybrid.patch.clone();
                        if let Some(prepared) = prepare_hybrid_bulk_delta(
                            desired_bytes,
                            observed_bytes,
                            *hybrid,
                            has_changed_sources,
                        )? {
                            crate::app::timing::log_timing(
                                &format!(
                                    "columnar hybrid bulk+delta diff ({patch_instances} bulk instances, {patch_fields} bulk fields, {patch_nodes} tree nodes, {extra_count} delta rows)"
                                ),
                                bulk_started,
                            );
                            bulk_patches
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(path.clone(), patch);
                            if settings_documents_equivalent(&prepared.current, &prepared.previous)
                            {
                                return Ok((path.clone(), PreparedSlot::Bulk));
                            }
                            return Ok((path.clone(), PreparedSlot::Prepared(prepared)));
                        }
                    }
                    crate::settings::bytecode::blocks::bulk_diff::BulkDiffOutcome::Fallback => {}
                }
            }
            let step_started = std::time::Instant::now();
            let replaced_fast = if read != StoresRead::Whole
                && let (
                    Some(SnapshotEntry::File(desired_bytes)),
                    Some(SnapshotEntry::File(observed_bytes)),
                ) = (desired.entries.get(path), observed.entries.get(path))
                && desired_bytes.len() >= CHANGED_INSTANCES_MIN_BYTES
                && store_payload_len(observed_bytes).saturating_mul(4)
                    < store_payload_len(desired_bytes)
                && crate::settings::bytecode::blocks::instance_count(observed_bytes)
                    .zip(crate::settings::bytecode::blocks::instance_count(desired_bytes))
                    .is_some_and(|(obs_rows, des_rows)| obs_rows.saturating_mul(4) < des_rows)
            {
                replaced_store_alone(desired.entries.get(path), observed.entries.get(path))?
            } else {
                None
            };
            let (decoded, bare) = if let Some(bare) = replaced_fast {
                (None, Some(bare))
            } else {
                let changed = if read == StoresRead::Whole {
                    ChangedInstances::Whole(None)
                } else {
                    changed_instances_alone(
                        path,
                        desired.entries.get(path),
                        observed.entries.get(path),
                        read == StoresRead::ChangedOfBoth,
                        imported.is_some(),
                        true,
                    )?
                };
                match changed {
                    ChangedInstances::None => return Ok((path.clone(), PreparedSlot::Unchanged)),
                    ChangedInstances::Prepared(change) => {
                        return Ok((path.clone(), PreparedSlot::Prepared(*change)));
                    }
                    ChangedInstances::Whole(None) if read != StoresRead::Whole => (
                        None,
                        replaced_store_alone(
                            desired.entries.get(path),
                            observed.entries.get(path),
                        )?,
                    ),
                    ChangedInstances::Whole(current) | ChangedInstances::Apart(current) => {
                        (current, None)
                    }
                    ChangedInstances::Imported(copies) => {
                        if let Some(imported) = imported {
                            imported
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(path.clone());
                        }
                        (None, Some(*copies))
                    }
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
                return Ok((path.clone(), PreparedSlot::Unchanged));
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
                    return Ok((path.clone(), PreparedSlot::Unchanged));
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
                PreparedSlot::Prepared(PreparedEditorSettingsChange { previous, current }),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut unchanged = HashSet::new();
    for (path, slot) in documents {
        match slot {
            PreparedSlot::Prepared(change) => {
                prepared.insert(path, change);
            }
            PreparedSlot::Unchanged => {
                unchanged.insert(path);
            }
            PreparedSlot::Bulk => {}
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

/// Whether a plan is carried out from the instances that differ: it sends
/// values of instances both copies hold, creates instances the files alone
/// hold and removes instances Studio alone holds, which are read with their
/// ancestors and their namesakes. An instance of another class, or a solid
/// cut anew, is created in the place of the one it removes and takes what
/// stands under that one with it, and an import reads the store: such a plan
/// is made from the files' whole copies. So is one that moves an instance
/// to another store, where an instance that differs in nothing refers to it
/// (`moved_between_stores`).
pub(crate) fn plan_reads_changed_alone(plan: &ReconcilePushPlan) -> bool {
    plan.initial_native_services.is_empty()
        && plan.previous_class_names.is_empty()
        && !plan
            .instance_deletes
            .iter()
            .flat_map(|change| &change.instances)
            .any(|removed| plan.recreated_settings_ids.contains(&removed.settings_id))
}

/// The ids of the instances a plan removes from one store and creates in
/// another, where those alone keep it from being carried out from the
/// instances that differ. What refers to such an instance names it by its
/// id; of the instances that differ in nothing, only one that does so has
/// to be read (`unread_instance_refers`).
pub(crate) fn moved_between_stores(plan: &ReconcilePushPlan) -> Option<HashSet<String>> {
    if !plan.initial_native_services.is_empty() || !plan.previous_class_names.is_empty() {
        return None;
    }
    let mut moved = HashSet::new();
    for change in &plan.instance_deletes {
        for removed in &change.instances {
            let id = &removed.settings_id;
            if !plan.recreated_settings_ids.contains(id) {
                continue;
            }
            if plan
                .recreated_instances
                .contains(&(change.service.clone(), id.clone()))
            {
                return None;
            }
            moved.insert(id.clone());
        }
    }
    Some(moved)
}

/// Whether an instance of a store that `current` holds unread refers to one
/// of `ids` by its id.
pub(crate) fn unread_instance_refers(
    entry: Option<&SnapshotEntry>,
    current: &SettingsBytecode,
    ids: &HashSet<String>,
) -> Result<bool> {
    use crate::settings::bytecode::property_edit::{decode_rows, whole_row_digests};
    let Some(SnapshotEntry::File(bytes)) = entry else {
        return Ok(true);
    };
    let Some(rows) = whole_row_digests(bytes, None, |_| Some((None, None)))? else {
        return Ok(true);
    };
    let mut unread = rows
        .named
        .into_iter()
        .filter(|row| {
            current.instances.get(*row).is_none_or(|instance| {
                crate::settings::held::stands_unchanged(&instance.properties)
            })
        })
        .collect::<Vec<_>>();
    unread.sort_unstable();
    unread.dedup();
    if unread.is_empty() {
        return Ok(false);
    }
    let (instances, _) = decode_rows(bytes, &unread)?;
    Ok(instances.iter().any(|instance| {
        instance
            .properties
            .values()
            .chain(instance.attributes.values())
            .any(|value| super::push_plan::value_references_any(value, ids))
    }))
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
    /// The two hold their instances at other places, or other instances
    /// than one run: `changed_instances_alone` then pairs each by itself.
    /// With the files' copy where it is decoded.
    Apart(Option<SettingsBytecode>),
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

fn is_native_side_effect_property(class_name: &str, name: &str) -> bool {
    crate::editor::native_roots::is_property(class_name, name)
        || crate::editor::native_geometry::is_mesh_geometry_property(class_name, name)
}

pub(super) fn prepare_hybrid_bulk_delta(
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    hybrid: crate::settings::bytecode::blocks::bulk_diff::BulkHybridDelta,
    has_changed_sources: bool,
) -> Result<Option<PreparedEditorSettingsChange>> {
    use crate::settings::bytecode::property_edit::decode_rows;
    use crate::settings::equivalence::inherit_workspace_viewport_reference;

    let service = hybrid.patch.service.clone();
    let des_count = hybrid.desired_skeleton.len();
    let obs_count = hybrid.observed_skeleton.len();
    if des_count == 0 || obs_count == 0 || hybrid.obs_to_des.len() != obs_count {
        crate::app::output::log_global(
            4,
            format_args!("[renium] hybrid prepare fallback ({service}): invalid skeleton shape"),
        );
        return Ok(None);
    }
    let mut des_to_obs = vec![None; des_count];
    for (o_idx, &d_opt) in hybrid.obs_to_des.iter().enumerate() {
        if let Some(d_idx) = d_opt {
            if d_idx >= des_count || des_to_obs[d_idx].is_some() {
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] hybrid prepare fallback ({service}): non-bijective mapping"
                    ),
                );
                return Ok(None);
            }
            des_to_obs[d_idx] = Some(o_idx);
        }
    }
    let has_structural_changes = des_count != obs_count
        || hybrid.obs_to_des.iter().enumerate().any(|(o_idx, d_opt)| {
            let Some(d_idx) = *d_opt else {
                return true;
            };
            let o_row = &hybrid.observed_skeleton[o_idx];
            let d_row = &hybrid.desired_skeleton[d_idx];
            o_row.name != d_row.name
                || o_row.class_name != d_row.class_name
                || o_row.parent_index.and_then(|p| hybrid.obs_to_des[p]) != d_row.parent_index
        });

    let mut diff_des = vec![false; des_count];
    let mut diff_obs = vec![false; obs_count];
    let no_named_des = vec![false; des_count];
    let no_named_obs = vec![false; obs_count];
    for &d in &hybrid.extra_desired_rows {
        if d < des_count {
            diff_des[d] = true;
            if let Some(o) = des_to_obs[d] {
                diff_obs[o] = true;
            }
        }
    }
    for &o in &hybrid.extra_observed_rows {
        if o < obs_count {
            diff_obs[o] = true;
            if let Some(d) = hybrid.obs_to_des[o] {
                diff_des[d] = true;
            }
        }
    }

    let (read_des_indices, read_obs_indices) = loop {
        let des_indices = instances_read(
            &hybrid.desired_skeleton,
            (&diff_des, &no_named_des),
            (true, has_changed_sources),
        );
        let obs_indices = instances_read(
            &hybrid.observed_skeleton,
            (&diff_obs, &no_named_obs),
            (true, has_changed_sources),
        );
        let mut grew = false;
        for &d in &des_indices {
            if !diff_des[d] {
                diff_des[d] = true;
            }
            if let Some(o) = des_to_obs[d]
                && !diff_obs[o]
            {
                diff_obs[o] = true;
                grew = true;
            }
        }
        for &o in &obs_indices {
            if !diff_obs[o] {
                diff_obs[o] = true;
            }
            if let Some(d) = hybrid.obs_to_des[o]
                && !diff_des[d]
            {
                diff_des[d] = true;
                grew = true;
            }
        }
        if !grew {
            break (des_indices, obs_indices);
        }
    };
    if read_des_indices.len() > MAX_CHANGED_INSTANCES
        || read_obs_indices.len() > MAX_CHANGED_INSTANCES
    {
        crate::app::output::log_global(
            4,
            format_args!(
                "[renium] hybrid prepare fallback ({service}): materialized rows des={} obs={} exceed limit",
                read_des_indices.len(),
                read_obs_indices.len()
            ),
        );
        return Ok(None);
    }

    let (des_decoded, obs_decoded) = rayon::join(
        || decode_rows(desired_bytes, &read_des_indices),
        || decode_rows(observed_bytes, &read_obs_indices),
    );
    let (Ok((des_rows, _)), Ok((obs_rows, _))) = (des_decoded, obs_decoded) else {
        crate::app::output::log_global(
            4,
            format_args!("[renium] hybrid prepare fallback ({service}): row decode failed"),
        );
        return Ok(None);
    };

    let mut current = sparse_document(
        hybrid.desired_skeleton,
        &read_des_indices,
        des_rows,
        SETTINGS_BINARY_VERSION,
        false,
    )?;
    let mut previous = sparse_document(
        hybrid.observed_skeleton,
        &read_obs_indices,
        obs_rows,
        current.version,
        false,
    )?;

    let mut used_ids: HashSet<String> = current
        .instances
        .iter()
        .map(|inst| inst.settings_id.clone())
        .collect();
    let mut id_remap: HashMap<String, String> = HashMap::with_capacity(obs_count);
    for (o_idx, inst) in previous.instances.iter_mut().enumerate() {
        let old_id = inst.settings_id.clone();
        if let Some(d_idx) = hybrid.obs_to_des[o_idx] {
            inst.settings_id
                .clone_from(&current.instances[d_idx].settings_id);
        } else {
            if used_ids.contains(&inst.settings_id) {
                let mut suffix = o_idx;
                let fresh = loop {
                    let candidate = format!("hybrid-obs-{suffix}");
                    if !used_ids.contains(&candidate) {
                        break candidate;
                    }
                    suffix += 1;
                };
                inst.settings_id = fresh;
            }
            used_ids.insert(inst.settings_id.clone());
        }
        id_remap.insert(old_id, inst.settings_id.clone());
    }
    let root_service = previous.instances[0].class_name.clone();
    crate::settings::equivalence::remap_rows(&mut previous, 0..obs_count, &id_remap, &root_service);
    for (o_idx, prop_name) in &hybrid.bulk_handled_on_extra {
        if let Some(d_idx) = hybrid.obs_to_des[*o_idx] {
            match current.instances[d_idx].properties.get(prop_name).cloned() {
                Some(val) => {
                    previous.instances[*o_idx]
                        .properties
                        .insert(prop_name.clone(), val);
                }
                None => {
                    previous.instances[*o_idx].properties.remove(prop_name);
                }
            }
        }
    }
    if has_structural_changes {
        let has_post_delta_native_write = read_des_indices.iter().any(|&d_idx| {
            let d_inst = &current.instances[d_idx];
            let o_inst = des_to_obs[d_idx].map(|o_idx| &previous.instances[o_idx]);
            d_inst.properties.iter().any(|(name, val)| {
                is_native_side_effect_property(&d_inst.class_name, name)
                    && o_inst.and_then(|o| o.properties.get(name)) != Some(val)
            }) || o_inst.is_some_and(|o| {
                o.properties.keys().any(|name| {
                    is_native_side_effect_property(&o.class_name, name)
                        && !d_inst.properties.contains_key(name)
                })
            })
        });
        if has_post_delta_native_write {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] hybrid prepare fallback ({service}): structural edit alongside native root/geometry write"
                ),
            );
            return Ok(None);
        }
    }
    inherit_workspace_viewport_reference(&mut current, &previous);
    if delta_weight(&current, &previous).heavy() {
        crate::app::output::log_global(
            4,
            format_args!("[renium] hybrid prepare fallback ({service}): delta weight heavy"),
        );
        return Ok(None);
    }
    Ok(Some(PreparedEditorSettingsChange { previous, current }))
}

/// A copy of a store read by some of its instances: every other one bare
/// and standing for its counterpart of the other copy. With `as_stored` each
/// property keeps the name its file states, as `editor_settings_document`
/// leaves it.
fn sparse_document(
    instances: Vec<SettingsBytecodeInstance>,
    read: &[usize],
    rows: Vec<SettingsBytecodeInstance>,
    version: u8,
    as_stored: bool,
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
    if !as_stored {
        canonicalize_settings_property_names(&mut document)?;
    }
    Ok(document)
}

/// The instances of a copy that stand under their parent beside a namesake,
/// twins, or under one at any depth.
fn twins_or_under(instances: &[SettingsBytecodeInstance]) -> Vec<bool> {
    let mut first = AHashMap::with_capacity(instances.len());
    let mut twins = vec![false; instances.len()];
    for (index, instance) in instances.iter().enumerate() {
        let key = (
            instance.parent_index,
            instance.name.as_str(),
            instance.class_name.as_str(),
        );
        if let Some(first) = first.insert(key, index) {
            twins[first] = true;
            twins[index] = true;
        }
    }
    for (index, instance) in instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index {
            twins[index] |= parent >= index || twins[parent];
        }
    }
    twins
}

/// Of Studio's instances, by `files_of` each the files' one it stands for,
/// the ones `twins_or_under` gives of either copy.
fn twins_of_both(
    (studio, files): (&[SettingsBytecodeInstance], &[SettingsBytecodeInstance]),
    files_of: impl Fn(usize) -> Option<usize>,
) -> Vec<bool> {
    let (of_studio, of_files) = (twins_or_under(studio), twins_or_under(files));
    (0..studio.len())
        .map(|index| of_studio[index] || files_of(index).is_some_and(|files| of_files[files]))
        .collect()
}

/// Tells the instances that differ in what they hold from the ones whose
/// rows two copies of a store only write another way: `differs` keeps each
/// of `differing`, an instance of the files' copy with Studio's, that holds
/// other values, by where Studio's stands, and their count is the answer. A
/// capture can write a value the other copy leaves out, in every row of a
/// class. Once more than `MAX_CHANGED_INSTANCES` differ the store is not
/// compared instance by instance, so the rest are left as they were, and so
/// are all of a copy that cannot be read by its rows. Of Studio's instances
/// in `twins`, which an alignment by places pairs as they stand, `differs`
/// keeps as well, uncounted, each that holds another value the matching
/// keys an instance by (`holds_exactly`).
fn told_apart(
    (desired_bytes, observed_bytes): (&[u8], &[u8]),
    differing: &[(usize, usize)],
    (differs, twins): (&mut [bool], Option<&[bool]>),
    moved: Option<&Moved>,
) -> Result<usize> {
    use crate::settings::bytecode::property_edit::decode_for_property_edit;
    use crate::settings::equivalence::{canonicalize_instances, holds_exactly};

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
    for pairs in differing.chunks(COMPARED_AT_ONCE) {
        if unlike > MAX_CHANGED_INSTANCES {
            break;
        }
        let (of_files, of_studio): (Vec<usize>, Vec<usize>) = pairs.iter().copied().unzip();
        let read = crate::app::profile::span("store rows read for comparison");
        let (read_files, read_studio) = rayon::join(
            || files_rows.materialize(&mut files, &of_files),
            || studio_rows.materialize(&mut studio, &of_studio),
        );
        drop(read);
        if read_files.is_err() || read_studio.is_err() {
            break;
        }
        let _compared = crate::app::profile::span("store rows compared");
        // The two copies are made whole together; one of the files' that
        // cannot be is the one refused first.
        let (of_files, of_studio) = rayon::join(
            || whole(&mut files, &of_files),
            || whole(&mut studio, &of_studio),
        );
        let (of_files, mut of_studio) = (of_files?, of_studio?);
        // A reference by position is compared as the position its instance
        // has among the files'.
        if let Some(moved) = moved {
            (of_studio.par_iter_mut()).for_each(|instance| moved.in_state_whole(instance));
        }
        // Instances paired each by itself can stand under another name or
        // parent, which a push sends as well.
        let differ = of_files
            .par_iter()
            .zip(&of_studio)
            .zip(pairs)
            .map(|((files, studio), (_, row))| {
                let differ = files.name != studio.name
                    || files.parent_index != studio.parent_index
                    || !instance_values_equal(None, files, studio);
                let twin = twins.is_some_and(|twins| twins[*row]);
                (differ, differ || twin && !holds_exactly(files, studio))
            })
            .collect::<Vec<_>>();
        (of_files.into_par_iter().chain(of_studio)).for_each(drop);
        for ((_, row), (differ, read)) in pairs.iter().zip(differ) {
            differs[*row] = read;
            unlike += usize::from(differ);
        }
        compared += pairs.len();
    }
    rayon::join(
        || drop_settings_documents(files, studio),
        || drop((files_rows, studio_rows)),
    );
    Ok(unlike + differing.len() - compared)
}

/// The instances of a store a push reads whole: the ones that differ or name
/// another by its id, and what a comparison reads of an instance whatever it
/// holds, the roots and the cameras. `walk_ancestors` walks each read row to
/// its root; `scripts_too` also reads every `Script` row because its
/// `RunContext` is part of its file name.
fn instances_read(
    instances: &[SettingsBytecodeInstance],
    (differs, named): (&[bool], &[bool]),
    (walk_ancestors, scripts_too): (bool, bool),
) -> Vec<usize> {
    let count = instances.len();
    let mut read = (0..count)
        .map(|index| {
            differs[index]
                || named[index]
                || instances[index].parent_index.is_none()
                || instances[index].class_name == "Camera"
                || scripts_too && instances[index].class_name == "Script"
        })
        .collect::<Vec<_>>();
    if walk_ancestors {
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
/// `studio` are the instances of that copy, bare, and `files` the files'
/// with their UniqueIds alone where they were read. `None` where the import
/// cannot place a camera or a package root, as
/// `promote_heavy_deltas_to_replacement` asks.
fn imported_store(
    (desired_bytes, observed_bytes): (&[u8], &[u8]),
    (studio, files): (
        Vec<SettingsBytecodeInstance>,
        Option<Vec<SettingsBytecodeInstance>>,
    ),
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
    let studio = sparse_document(studio, &kept, rows, SETTINGS_BINARY_VERSION, false)?;
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
    replaced_store_beside(desired_bytes, retained, files)
}

/// What `changed_instances_alone` answers for a store it has imported anew
/// (`imported_store`), said in the log: the whole documents' where the
/// import cannot take it.
fn imported_anew(
    copies: (&[u8], &[u8]),
    instances: (
        Vec<SettingsBytecodeInstance>,
        Option<Vec<SettingsBytecodeInstance>>,
    ),
    why: &str,
) -> Result<ChangedInstances> {
    let count = instances.0.len();
    let Some(copies) = imported_store(copies, instances)? else {
        return Ok(ChangedInstances::Whole(None));
    };
    log_global(
        3,
        format_args!("[renium] push imports a store of {count} instances anew: {why}"),
    );
    Ok(ChangedInstances::Imported(Box::new(copies)))
}

/// The change of a store whose two copies differ in the values of a few
/// instances, without the whole document of the store Studio holds: the
/// digests of the two files say which instances differ, and those, their
/// ancestors, the namesakes of all of them, the roots and the cameras alone
/// are read from Studio's. Every other instance of that document stands for
/// the files' instance at its place (`held::stand_unchanged`) and keeps its
/// UniqueId alone. This is taken only where the two copies hold the same
/// instances at the same places and the alignment of the whole documents
/// would leave each of them there, whatever ids Studio gave them. One copy
/// may hold one run of instances the other lacks, an instance a store edit
/// added or removed with all under it: every other instance then stands for
/// the one at its place beside the run (`Moved`).
///
/// With `files_too` the files' copy is read the same way: a push that sends
/// nothing but values of existing instances reads no other instance of it.
///
/// Copies that hold their instances at other places, as after an instance
/// was added to the files under a folder and pushed, or more than one run
/// apart, are read the same way where an id, a UniqueId or its place under
/// its parent tells every instance by itself (`told_alone`).
///
/// `viewport_first` is the order of a push that replaces: the files take the
/// camera of the viewport from Studio before the two are compared.
///
/// Whether two copies hold the same depends on their bytes alone, so a pair
/// found to is not read again while neither changes. `path` names the store:
/// one whose copies stood at other places the last time is paired so first
/// (`changed_instances_compared`).
fn changed_instances_alone(
    path: &Path,
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    files_too: bool,
    imports_heavy: bool,
    viewport_first: bool,
) -> Result<ChangedInstances> {
    use sha2::{Digest, Sha256};
    type Pairs = Mutex<HashSet<([u8; 32], [u8; 32], bool)>>;
    static SAME: std::sync::OnceLock<Pairs> = std::sync::OnceLock::new();
    let pair = match (desired, observed) {
        (Some(SnapshotEntry::File(files)), Some(SnapshotEntry::File(studio))) => {
            let (files, studio) = rayon::join(
                || <[u8; 32]>::from(Sha256::digest(files)),
                || <[u8; 32]>::from(Sha256::digest(studio)),
            );
            Some((files, studio, viewport_first))
        }
        _ => None,
    };
    let same = SAME.get_or_init(Default::default);
    if pair.is_some_and(|pair| same.lock_recover().contains(&pair)) {
        return Ok(ChangedInstances::None);
    }
    let how = (files_too, imports_heavy, viewport_first, false);
    let changed = changed_instances_compared(path, desired, observed, how)?;
    if let (ChangedInstances::None, Some(pair)) = (&changed, pair) {
        let mut same = same.lock_recover();
        if same.len() >= 4_096 {
            same.clear();
        }
        same.insert(pair);
    }
    Ok(changed)
}

/// `changed_instances_alone` without what it remembers of equal copies.
/// Copies are compared place by place first and, where they stand apart,
/// paired each instance by itself. A store whose copies held as many
/// instances at other places the last time is paired without the comparison
/// by place, which could only say so again.
fn changed_instances_compared(
    path: &Path,
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    how: (bool, bool, bool, bool),
) -> Result<ChangedInstances> {
    static APART: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    let apart = APART.get_or_init(Default::default);
    let copies = match (desired, observed) {
        (Some(SnapshotEntry::File(files)), Some(SnapshotEntry::File(studio))) if how.0 => {
            Some((files, studio))
        }
        _ => None,
    };
    let paired = |mut told: Told| {
        let structures = told.structures.take();
        let had_by_order = !told.by_order.is_empty();
        Ok(
            match changed_instances_placed(desired, observed, how, Some(told), structures)? {
                ChangedInstances::Apart(current) => {
                    if had_by_order
                        && let Some((files, studio)) = copies
                        && let Some(compact_told) = told_compact(files, studio)?
                    {
                        match changed_instances_placed(
                            desired,
                            observed,
                            how,
                            Some(compact_told),
                            None,
                        )? {
                            ChangedInstances::Apart(compact_current) => {
                                ChangedInstances::Whole(compact_current.or(current))
                            }
                            changed => {
                                #[cfg(test)]
                                if matches!(changed, ChangedInstances::Prepared(_)) {
                                    TOLD_ALONE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                changed
                            }
                        }
                    } else {
                        ChangedInstances::Whole(current)
                    }
                }
                changed => {
                    #[cfg(test)]
                    if matches!(changed, ChangedInstances::Prepared(_)) {
                        TOLD_ALONE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    changed
                }
            },
        )
    };
    if let Some((files, studio)) = copies
        && apart.lock_recover().contains(path)
    {
        match told_alone(files, studio, None) {
            Some((told, true)) => {
                #[cfg(test)]
                PAIRED_FIRST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return paired(told);
            }
            Some(_) | None => {
                apart.lock_recover().remove(path);
            }
        }
    }
    let structures = match (desired, observed) {
        (Some(SnapshotEntry::File(files)), Some(SnapshotEntry::File(studio))) => {
            structures_of(files, studio)
        }
        _ => None,
    };
    let (current, structures) = match structures {
        Some((files, studio))
            if copies.is_some() && stand_apart(&files.instances, &studio.instances) =>
        {
            (None, Some((files, studio)))
        }
        Some((files, studio))
            if copies.is_some()
                && files.instances.len() != studio.instances.len()
                && Moved::between_copies(&studio.instances, &files.instances)
                    .flatten()
                    .is_none() =>
        {
            (None, Some((files, studio)))
        }
        structures => match changed_instances_placed(desired, observed, how, None, structures)? {
            ChangedInstances::Apart(current) => (current, None),
            changed => return Ok(changed),
        },
    };
    if let Some((files, studio)) = copies {
        if let Some((told, as_many_apart)) = told_alone(files, studio, structures) {
            if as_many_apart {
                apart.lock_recover().insert(path.to_path_buf());
            }
            return paired(told);
        }
        if let Some(told) = told_compact(files, studio)? {
            return paired(told);
        }
    }
    Ok(ChangedInstances::Whole(current))
}

/// The stores paired each instance by itself without a comparison by place
/// before.
#[cfg(test)]
pub(crate) static PAIRED_FIRST: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The stores whose namesakes were paired by their order: the ones refused
/// for what such a pair holds, and the ones taken.
#[cfg(test)]
pub(crate) static BY_ORDER: [std::sync::atomic::AtomicUsize; 2] = [
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];

/// The stores prepared from instances paired each by itself.
#[cfg(test)]
pub(crate) static TOLD_ALONE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The stores prepared from instances aligned beside a run of instances one
/// copy alone holds, each by `copies_key` of its two copies.
#[cfg(test)]
pub(crate) static ALIGNED_BESIDE_RUN: std::sync::Mutex<Vec<u64>> =
    std::sync::Mutex::new(Vec::new());

/// What tells the two copies of a store one test prepared from those of
/// another test.
#[cfg(test)]
pub(crate) fn copies_key(desired: &[u8], observed: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (desired, observed).hash(&mut hasher);
    hasher.finish()
}

/// How `told_alone` pairs the instances of two copies of a store.
struct Told {
    /// Studio's instances among the files'.
    moved: Moved,
    /// The id the alignment gives each of Studio's.
    ids: Vec<String>,
    /// Studio's instances taken by their order among their namesakes under
    /// one parent: UniqueIds told none of those apart, as after an import
    /// of the service, which gives every instance another.
    by_order: Vec<(usize, ByOrder)>,
    /// The instances of the files' copy and of Studio's without their
    /// values, which the digests of the two read.
    structures: Option<(SettingsBytecode, SettingsBytecode)>,
}

/// How Studio's instances stand among the files', each paired by itself,
/// and the id the alignment gives each (`settings_ids_told_in_order`). Of
/// each copy only the instances with their UniqueIds, and the camera a root
/// names, are read. `None` where the whole documents have to say. With it,
/// whether the copies hold as many instances and one stands under another
/// name, class or parent than the one at its place: a comparison by place
/// finds such copies apart. `structures` are the instances of the two
/// without their values, where they are read.
fn told_alone(
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    structures: Option<(SettingsBytecode, SettingsBytecode)>,
) -> Option<(Told, bool)> {
    use crate::settings::bytecode::property_edit::{decode_for_row_chunks, retained_row_columns};
    let _span = crate::app::profile::span("store instances told alone");
    let bare = |bytes: &[u8], structure: Option<SettingsBytecode>| {
        let (mut document, columns) = match structure {
            Some(structure) => (structure, retained_row_columns(bytes).ok()?),
            None => decode_for_row_chunks(bytes).ok()?,
        };
        columns
            .project_properties(
                &mut document,
                |name| name == "UniqueId" || name == "CurrentCamera",
                |instance, name, value| {
                    (name == "UniqueId" || instance.parent_index.is_none()).then_some(value)
                },
            )
            .ok()?;
        Some(document)
    };
    let (files, studio) = structures.unzip();
    let read = crate::app::profile::span("store UniqueIds read");
    let (files, studio) = rayon::join(
        || bare(desired_bytes, files),
        || bare(observed_bytes, studio),
    );
    drop(read);
    let (mut files, mut studio) = (files?, studio?);
    let told = crate::app::profile::span("store instances told in order");
    let (ids, places, by_order) =
        crate::settings::equivalence::settings_ids_told_in_order(&files, &studio)?;
    drop(told);
    let as_many_apart = stand_apart(&files.instances, &studio.instances);
    let moved = Moved::paired(places, files.instances.len());
    for instance in files.instances.iter_mut().chain(&mut studio.instances) {
        instance.properties.clear();
    }
    Some((
        Told {
            moved,
            ids,
            by_order,
            structures: Some((files, studio)),
        },
        as_many_apart,
    ))
}

/// Bounded-memory fallback for `told_alone` when UniqueIds do not uniquely pair
/// ambiguous twins (for example after a native import): runs compact streamed
/// identity matching on the v12 block stores without decoding whole documents.
fn told_compact(desired_bytes: &[u8], observed_bytes: &[u8]) -> Result<Option<Told>> {
    if !compared_by_instances(desired_bytes, observed_bytes) {
        return Ok(None);
    }
    let Some(files_count) = crate::settings::bytecode::blocks::instance_count(desired_bytes) else {
        return Ok(None);
    };
    let Some((ids, places)) =
        crate::settings::equivalence::match_block_stores_compact(desired_bytes, observed_bytes)?
    else {
        return Ok(None);
    };
    Ok(Some(Told {
        moved: Moved::paired(places, files_count),
        ids,
        by_order: Vec::new(),
        structures: None,
    }))
}

/// Whether two copies of a store are compared by their instances: a small
/// store file is read whole, and so is a store Studio holds nearly empty, or
/// holds far more of.
fn compared_by_instances(desired_bytes: &[u8], observed_bytes: &[u8]) -> bool {
    let (desired_len, observed_len) = (
        store_payload_len(desired_bytes),
        store_payload_len(observed_bytes),
    );
    desired_bytes.len() >= CHANGED_INSTANCES_MIN_BYTES
        && observed_len.saturating_mul(4) >= desired_len
        && desired_len.saturating_mul(4) >= observed_len
}

/// The instances of two v12 copies of a store, without their values, where the
/// two are compared by their instances. Their digests and pairing reuse them.
fn structures_of(
    desired_bytes: &[u8],
    observed_bytes: &[u8],
) -> Option<(SettingsBytecode, SettingsBytecode)> {
    use crate::settings::bytecode::blocks::instance_count;
    let counts = (
        instance_count(desired_bytes),
        instance_count(observed_bytes),
    );
    if !matches!(counts, (Some(_), Some(_)))
        || !compared_by_instances(desired_bytes, observed_bytes)
    {
        return None;
    }
    let (files, studio) = rayon::join(
        || SettingsBytecode::decode_structure(desired_bytes),
        || SettingsBytecode::decode_structure(observed_bytes),
    );
    Some((files.ok()?, studio.ok()?))
}

/// Whether two copies hold as many instances and one stands under another
/// name, class or parent than the one at its place: a comparison by place
/// finds such copies apart, whatever they hold.
fn stand_apart(files: &[SettingsBytecodeInstance], studio: &[SettingsBytecodeInstance]) -> bool {
    files.len() == studio.len()
        && files.iter().zip(studio).any(|(files, studio)| {
            files.name != studio.name
                || files.class_name != studio.class_name
                || files.parent_index != studio.parent_index
        })
}

/// The instance of the other copy an instance of one stands for, if any.
type Counterpart<'a> = &'a dyn Fn(usize) -> Option<usize>;

/// Studio's instances that a matching took by their order among namesakes
/// (`Told::by_order`), with the ones that stand under such an instance, where
/// they are the instances it took them for; `None` where they may not be.
/// One paired so is its counterpart only where the two and all Studio holds
/// under them hold the same, as a comparison by place asks of namesakes
/// (`positional_identity_preserved`). Where one copy holds more of the
/// namesakes, more could tell which of them an instance is, so none of this
/// may be there: an instance the files alone hold under a paired one, an
/// instance the files hold elsewhere under one Studio holds more. `studio`
/// and `files` are the instances of the two copies, and `differs` says of
/// each of Studio's whether its row differs from its counterpart's. What
/// names one of these instances is `names_one_taken_by_order`'s to find.
fn taken_by_order(
    by_order: &[(usize, ByOrder)],
    (studio, files): (&[SettingsBytecodeInstance], &[SettingsBytecodeInstance]),
    differs: &[bool],
    (files_of, studio_of): (Counterpart<'_>, Counterpart<'_>),
) -> Option<Vec<Option<ByOrder>>> {
    let mut ordered = vec![None; studio.len()];
    for (index, how) in by_order {
        ordered[*index] = Some(*how);
    }
    for index in 0..studio.len() {
        let parent = studio[index].parent_index;
        if parent.is_some_and(|parent| parent >= index) {
            return None;
        }
        let above = parent.and_then(|parent| ordered[parent]);
        if above == Some(ByOrder::More) {
            if files_of(index).is_some() {
                return None;
            }
            ordered[index] = above;
        } else if ordered[index] != Some(ByOrder::More) {
            ordered[index] = ordered[index].max(above);
            if ordered[index].is_some() && differs[index] {
                return None;
            }
        }
    }
    (0..files.len())
        .filter(|index| studio_of(*index).is_none())
        .all(|index| {
            // The nearest instance above it that Studio holds as well.
            let mut above = files[index].parent_index;
            for _ in 0..files.len() {
                let Some(parent) = above else {
                    return true;
                };
                if let Some(studio) = studio_of(parent) {
                    return ordered[studio] != Some(ByOrder::BesideMore);
                }
                above = files[parent].parent_index;
            }
            false
        })
        .then_some(ordered)
}

fn ordered_differing_pairs(
    by_order: &[(usize, ByOrder)],
    studio: &[SettingsBytecodeInstance],
    differs: &[bool],
    files_of: Counterpart<'_>,
) -> Vec<(usize, usize)> {
    let mut ordered = vec![None; studio.len()];
    for &(index, how) in by_order {
        ordered[index] = Some(how);
    }
    let mut pairs = Vec::new();
    for index in 0..studio.len() {
        let parent = studio[index].parent_index;
        if parent.is_some_and(|parent| parent >= index) {
            return Vec::new();
        }
        let above = parent.and_then(|parent| ordered[parent]);
        if above == Some(ByOrder::More) {
            ordered[index] = above;
        } else if ordered[index] != Some(ByOrder::More) {
            ordered[index] = ordered[index].max(above);
            if ordered[index].is_some()
                && differs[index]
                && let Some(files_idx) = files_of(index)
            {
                pairs.push((files_idx, index));
            }
        }
    }
    pairs
}

/// Whether an instance read of either copy refers to one of `taken`
/// (`taken_by_order`, of Studio's instances): namesakes that hold the same
/// are told apart by what names them, and a pairing by their order that took
/// one for the other reads as a reference that changed. `read` and
/// `read_of_files` are the instances read of Studio's copy and of the files'.
fn names_one_taken_by_order(
    taken: &[Option<ByOrder>],
    (studio, files): (&SettingsBytecode, &SettingsBytecode),
    (read, read_of_files): (&[usize], &[usize]),
    studio_of: Counterpart<'_>,
) -> bool {
    use crate::settings::bytecode::for_each_reference;
    use crate::settings::bytecode::property_edit::names_an_instance_by_id;
    let has_ref = |instance: &SettingsBytecodeInstance| {
        instance
            .properties
            .values()
            .chain(instance.attributes.values())
            .any(names_an_instance_by_id)
    };
    if !read.iter().any(|index| has_ref(&studio.instances[*index]))
        && !read_of_files
            .iter()
            .any(|index| has_ref(&files.instances[*index]))
    {
        return false;
    }
    let by_id = |document: &'_ SettingsBytecode| {
        let ids = document.instances.iter().enumerate();
        ids.map(|(index, instance)| (instance.settings_id.clone(), index))
            .collect::<HashMap<_, _>>()
    };
    let names = |instance: &SettingsBytecodeInstance,
                 ids: &HashMap<String, usize>,
                 taken: &dyn Fn(usize) -> bool| {
        let mut found = false;
        let values = instance.properties.values();
        for value in values.chain(instance.attributes.values()) {
            for_each_reference(value, &mut |reference| {
                let id = reference.get("settingsId").and_then(Value::as_str);
                found |= id.and_then(|id| ids.get(id)).is_some_and(|at| taken(*at));
            });
        }
        found
    };
    let (studio_ids, files_ids) = rayon::join(|| by_id(studio), || by_id(files));
    let of_studio = |index: usize| taken[index].is_some();
    let of_files = |index: usize| studio_of(index).is_some_and(|studio| taken[studio].is_some());
    read.iter()
        .any(|index| names(&studio.instances[*index], &studio_ids, &of_studio))
        || read_of_files
            .iter()
            .any(|index| names(&files.instances[*index], &files_ids, &of_files))
}

fn precompute_moved_run(
    told: Option<&Told>,
    structures: Option<&(SettingsBytecode, SettingsBytecode)>,
    as_many: bool,
) -> Option<Option<Moved>> {
    if told.is_some() || as_many {
        return Some(None);
    }
    let Some((files, studio)) = structures else {
        return Some(None);
    };
    let moved = Moved::between_copies(&studio.instances, &files.instances).flatten()?;
    Some(Some(moved))
}

fn moved_for_placed(
    told: Option<&Told>,
    precomputed: Option<&Moved>,
    as_many: bool,
    desired_bytes: &[u8],
    studio: &[SettingsBytecodeInstance],
) -> Option<Option<Moved>> {
    if let Some(told) = told {
        return Some(Some(told.moved.clone()));
    }
    if let Some(moved) = precomputed {
        return Some(Some(moved.clone()));
    }
    if as_many {
        return Some(None);
    }
    let moved = crate::settings::bytecode::property_edit::decode_for_row_chunks(desired_bytes)
        .ok()
        .and_then(|(files, _)| Moved::between_copies(studio, &files.instances))
        .flatten()?;
    Some(Some(moved))
}

/// `changed_instances_alone` for copies whose instances stand at the same
/// places, one run apart, or as `told` pairs them. `as_stored` is for two
/// copies of the files' own (`edited_store_alone`). `structures` are the
/// instances of the files' copy and of Studio's without their values, where
/// they were read before (`structures_of`).
fn changed_instances_placed(
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    (files_too, imports_heavy, viewport_first, as_stored): (bool, bool, bool, bool),
    told: Option<Told>,
    structures: Option<(SettingsBytecode, SettingsBytecode)>,
) -> Result<ChangedInstances> {
    use crate::settings::bytecode::blocks::instance_count;
    use crate::settings::bytecode::property_edit::{
        StoreRows, decode_rows, names_an_instance_by_id, whole_row_digests,
        whole_row_digests_of_both,
    };
    use crate::settings::equivalence::{
        align_settings_ids_beside_run, align_settings_ids_by_place,
        inherit_workspace_viewport_reference, stabilize_settings_reference_ids, take_settings_ids,
    };

    let (Some(SnapshotEntry::File(desired_bytes)), Some(SnapshotEntry::File(observed_bytes))) =
        (desired, observed)
    else {
        return Ok(ChangedInstances::Whole(None));
    };
    // A store Studio holds nearly empty is replaced, not compared.
    if !compared_by_instances(desired_bytes, observed_bytes) {
        return Ok(ChangedInstances::Whole(None));
    }
    // What is known of the files' copy: its instances whole, or their
    // digests with the instances bare.
    enum Files {
        Whole(SettingsBytecode, Vec<[u8; 16]>, Vec<bool>),
        Rows(Box<StoreRows>),
    }
    // Copies that hold as many instances are compared place by place.
    // Where they do not, Studio's instances are digested as they stand among
    // the files', if one run of instances is all that one holds more.
    let as_many = match (
        instance_count(desired_bytes),
        instance_count(observed_bytes),
    ) {
        (Some(files), Some(studio)) => files == studio,
        _ => true,
    };
    // The digests place the parents of Studio's instances among the files';
    // its document holds them where Studio has them.
    let Some(precomputed_moved) = precompute_moved_run(told.as_ref(), structures.as_ref(), as_many)
    else {
        return Ok(ChangedInstances::Apart(None));
    };
    let own_parents = std::sync::OnceLock::new();
    let stand = |studio: &[SettingsBytecodeInstance]| {
        let moved = moved_for_placed(
            told.as_ref(),
            precomputed_moved.as_ref(),
            as_many,
            desired_bytes,
            studio,
        )?;
        if moved.is_some() {
            let parents = studio.iter().map(|instance| instance.parent_index);
            let _ = own_parents.set(parents.collect::<Vec<_>>());
        }
        Some((None, moved))
    };
    // Copies whose structures stand apart are compared whole, and of those
    // the files' document is all that is read here.
    if !files_too
        && told.is_none()
        && let Some((files, studio)) = &structures
        && stand_apart(&files.instances, &studio.instances)
    {
        let mut current =
            crate::settings::bytecode::decode_settings_bytecode_holding(desired_bytes)?;
        stabilize_settings_reference_ids(&mut current);
        canonicalize_settings_property_names(&mut current)?;
        return Ok(ChangedInstances::Apart(Some(current)));
    }
    let (files_structure, studio_structure) = structures
        .map(|(files, studio)| (files.instances, studio.instances))
        .unzip();
    let (observed_rows, files) = if files_too {
        let (files, observed_rows) = if as_many && told.is_none() {
            whole_row_digests_of_both(
                desired_bytes,
                observed_bytes,
                files_structure.zip(studio_structure),
            )
        } else {
            rayon::join(
                || whole_row_digests(desired_bytes, files_structure, |_| Some((None, None))),
                || whole_row_digests(observed_bytes, studio_structure, stand),
            )
        };
        (
            observed_rows,
            Ok(files.ok().flatten().map(Box::new).map(Files::Rows)),
        )
    } else {
        rayon::join(
            || whole_row_digests(observed_bytes, studio_structure, stand),
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
    let mut skeleton = observed_rows.instances;
    if let (Some(_), Some(parents)) = (&observed_rows.moved, own_parents.get()) {
        for (instance, parent) in skeleton.iter_mut().zip(parents) {
            instance.parent_index = *parent;
        }
    }
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
    let studio_count = skeleton.len();
    let moved = observed_rows.moved;
    let same_places = files_digests.len() == count
        && observed_rows.digests.len() == studio_count
        && match moved.as_ref().map(Moved::run) {
            // Each instance beside the run stands under the name, class and
            // parent of its counterpart (`Moved::between_copies`).
            Some(Some((run, _))) => run.len() == count.abs_diff(studio_count),
            // Paired each by itself, under whatever name or parent.
            Some(None) => true,
            None => {
                studio_count == count
                    && files_instances
                        .iter()
                        .zip(&skeleton)
                        .all(|(files, studio)| {
                            files.name == studio.name
                                && files.class_name == studio.class_name
                                && files.parent_index == studio.parent_index
                        })
            }
        };
    let hand_over = |files: Files| {
        Ok(ChangedInstances::Whole(match files {
            Files::Whole(current, ..) => Some(current),
            Files::Rows(_) => None,
        }))
    };
    let run = moved.as_ref().and_then(Moved::run);
    if !same_places || run.is_some_and(|run| !run_is_told(run, files_instances, &skeleton)) {
        return Ok(ChangedInstances::Apart(match files {
            Files::Whole(current, ..) => Some(current),
            Files::Rows(_) => None,
        }));
    }
    let moved = moved.as_ref();
    let files_of = |studio: usize| moved.map_or(Some(studio), |moved| moved.in_state(studio));
    let studio_of = |files: usize| moved.map_or(Some(files), |moved| moved.in_store(files));
    let mut differs = (0..studio_count)
        .map(|index| {
            files_of(index).is_none_or(|files| files_digests[files] != observed_rows.digests[index])
        })
        .collect::<Vec<_>>();
    // Read whatever they hold: the instances that name another by its id.
    let kept = (0..studio_count)
        .map(|index| files_of(index).is_some_and(|files| files_named[files]))
        .collect::<Vec<_>>();
    let mut taken = None;
    if let Some(by_order) = told.as_ref().map(|told| &told.by_order)
        && !by_order.is_empty()
    {
        taken = taken_by_order(
            by_order,
            (&skeleton, files_instances),
            &differs,
            (&files_of, &studio_of),
        );
        if taken.is_none() {
            let ordered_diff = ordered_differing_pairs(by_order, &skeleton, &differs, &files_of);
            if !ordered_diff.is_empty() && ordered_diff.len() <= MAX_CHANGED_INSTANCES {
                let copies = (desired_bytes.as_slice(), observed_bytes.as_slice());
                let unlike = told_apart(copies, &ordered_diff, (&mut differs, None), moved)?;
                if unlike == 0 {
                    taken = taken_by_order(
                        by_order,
                        (&skeleton, files_instances),
                        &differs,
                        (&files_of, &studio_of),
                    );
                }
            }
        }
        if taken.is_none() {
            #[cfg(test)]
            BY_ORDER[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(ChangedInstances::Apart(match files {
                Files::Whole(current, ..) => Some(current),
                Files::Rows(_) => None,
            }));
        }
    }
    let beside_run = told.is_none() && moved.and_then(Moved::run).is_some();
    let no_kept;
    let kept_for_read = if told.is_some() && !as_stored {
        no_kept = vec![false; studio_count];
        &no_kept
    } else {
        &kept
    };
    let mut read = instances_read(&skeleton, (&differs, kept_for_read), (files_too, files_too));
    if read.len() > MAX_CHANGED_INSTANCES {
        // The rows that only write the same values another way are not read.
        let mut names = kept.clone();
        for index in &observed_rows.named {
            if let Some(names) = names.get_mut(*index) {
                *names = true;
            }
        }
        let differing = (0..studio_count)
            .filter(|&index| differs[index] && !names[index])
            .filter_map(|index| Some((files_of(index)?, index)))
            .collect::<Vec<_>>();
        // Twins, which an alignment beside a run pairs by their places, are
        // read where they hold what the matching weighs otherwise.
        let twins = beside_run.then(|| twins_of_both((&skeleton, files_instances), files_of));
        let copies = (desired_bytes.as_slice(), observed_bytes.as_slice());
        // With the instances one copy alone holds.
        let unlike = told_apart(copies, &differing, (&mut differs, twins.as_deref()), moved)?
            + (0..studio_count)
                .filter(|&index| files_of(index).is_none())
                .count()
            + (0..count)
                .filter(|&index| studio_of(index).is_none())
                .count();
        if unlike > MAX_CHANGED_INSTANCES && imports_heavy {
            // Neither copy is whole beside the ones the import reads, which
            // start from the files' instances as their digests left them.
            let files = match files {
                Files::Rows(rows) => Some(rows.instances),
                Files::Whole(..) => None,
            };
            let why = "most of its instances differ";
            return imported_anew(copies, (skeleton, files), why);
        }
        read = instances_read(&skeleton, (&differs, &kept), (files_too, files_too));
    }
    // Of the files' copy read by its instances: the counterparts of those,
    // and the instances of a run it alone holds with what is read beside
    // them, whose counterparts are read from Studio's as well.
    let read_of_files = match (&files, moved) {
        (Files::Rows(files), Some(_)) => {
            let mut of_studio = vec![false; studio_count];
            for index in &read {
                of_studio[*index] = true;
            }
            let wanted = (0..count)
                .map(|index| studio_of(index).is_none_or(|studio| of_studio[studio]))
                .collect::<Vec<_>>();
            let of_files = instances_read(
                &files.instances,
                (&wanted, &vec![false; count]),
                (true, true),
            );
            let known = read.len();
            for studio in of_files.iter().filter_map(|index| studio_of(*index)) {
                if !std::mem::replace(&mut of_studio[studio], true) {
                    read.push(studio);
                }
            }
            if read.len() != known {
                read.sort_unstable();
            }
            Some(of_files)
        }
        _ => None,
    };
    if read.len() > MAX_CHANGED_INSTANCES || read.len() == studio_count {
        return hand_over(files);
    }
    let Ok((rows, _)) = decode_rows(observed_bytes, &read) else {
        return hand_over(files);
    };
    let mut current = match files {
        Files::Whole(current, ..) => current,
        Files::Rows(files) => {
            let read = read_of_files.as_deref().unwrap_or(&read);
            let Ok((rows, _)) = decode_rows(desired_bytes, read) else {
                return Ok(ChangedInstances::Whole(None));
            };
            sparse_document(
                files.instances,
                read,
                rows,
                SETTINGS_BINARY_VERSION,
                as_stored,
            )?
        }
    };
    let mut previous = sparse_document(skeleton, &read, rows, current.version, as_stored)?;
    let hand_over =
        |current: SettingsBytecode| Ok(ChangedInstances::Whole((!files_too).then_some(current)));
    if settings_documents_positionally_equivalent(&current, &previous) {
        return Ok(ChangedInstances::None);
    }
    let stored_ids = as_stored.then(|| {
        let ids = previous.instances.iter();
        ids.map(|instance| instance.settings_id.clone())
            .collect::<Vec<_>>()
    });
    let aligned = match (told, moved.and_then(Moved::run)) {
        (Some(told), _) => {
            take_settings_ids(&mut previous, told.ids);
            true
        }
        (None, None) => align_settings_ids_by_place(&current, &mut previous),
        (None, Some((run, of_files))) => {
            align_settings_ids_beside_run(&current, &mut previous, run, !of_files)
        }
    };
    if let Some(taken) = &taken {
        let props_differ_ignoring_uid = |a: &Map<String, Value>, b: &Map<String, Value>| {
            a.iter()
                .any(|(k, v)| k != "UniqueId" && b.get(k) != Some(v))
                || b.iter()
                    .any(|(k, v)| k != "UniqueId" && a.get(k) != Some(v))
        };
        let mut studio_check = Vec::new();
        let mut files_check = Vec::new();
        for &studio_idx in &read {
            match files_of(studio_idx) {
                Some(files_idx) => {
                    let p = &previous.instances[studio_idx];
                    let c = &current.instances[files_idx];
                    if differs[studio_idx]
                        || p.attributes != c.attributes
                        || props_differ_ignoring_uid(&p.properties, &c.properties)
                    {
                        studio_check.push(studio_idx);
                        files_check.push(files_idx);
                    }
                }
                None => studio_check.push(studio_idx),
            }
        }
        for files_idx in 0..count {
            if studio_of(files_idx).is_none() {
                files_check.push(files_idx);
            }
        }
        let named = names_one_taken_by_order(
            taken,
            (&previous, &current),
            (&studio_check, &files_check),
            &studio_of,
        );
        #[cfg(test)]
        BY_ORDER[usize::from(!named)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if named {
            return Ok(ChangedInstances::Apart((!files_too).then_some(current)));
        }
    }
    // Two copies of the files' own share their ids: an alignment that gives
    // an instance another id took it for one the files hold under that id.
    let kept = stored_ids.is_none_or(|ids| {
        let now = previous.instances.iter();
        ids.iter().eq(now.map(|instance| &instance.settings_id))
    });
    if !aligned || !kept {
        return hand_over(current);
    }
    #[cfg(test)]
    if beside_run {
        let key = copies_key(desired_bytes, observed_bytes);
        ALIGNED_BESIDE_RUN.lock().unwrap().push(key);
        if let (Ok(files), Ok(studio)) = (settings_document(desired), settings_document(observed)) {
            crate::settings::equivalence::paired_as_the_whole_matching_pairs(
                &files, studio, &current, &previous,
            );
        }
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
    if viewport_first {
        inherit_workspace_viewport_reference(&mut current, &previous);
    }
    if settings_documents_equivalent(&current, &previous) {
        return Ok(ChangedInstances::None);
    }
    if !viewport_first {
        inherit_workspace_viewport_reference(&mut current, &previous);
    }
    // A delta heavy enough to replace the service is one import of the
    // store, as `promote_heavy_deltas_to_replacement` makes it; where the
    // push imports none, the whole documents send it.
    if delta_weight(&current, &previous).heavy() {
        if !imports_heavy {
            return hand_over(current);
        }
        drop((current, previous));
        let Some(studio) = whole_row_digests(observed_bytes, None, |_| Some((None, None)))
            .ok()
            .flatten()
        else {
            return Ok(ChangedInstances::Whole(None));
        };
        return imported_anew(
            (desired_bytes, observed_bytes),
            (studio.instances, None),
            "it creates more instances than a push sends one by one",
        );
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
        || observed_bytes.saturating_mul(4) >= store_payload_len(desired_bytes)
    {
        return Ok(None);
    }
    replaced_store_beside(desired_bytes, settings_document(observed)?, None)
}

/// `replaced_store_alone` for the files' store beside what Studio holds, or
/// keeps, of its own. `files` are the instances of a v12 store file, each
/// with its UniqueId alone, where they were read before.
fn replaced_store_beside(
    desired_bytes: &[u8],
    previous: SettingsBytecode,
    files: Option<Vec<SettingsBytecodeInstance>>,
) -> Result<Option<(SettingsBytecode, SettingsBytecode)>> {
    use crate::settings::bytecode::property_edit::{decode_for_row_chunks, retained_row_columns};
    use crate::settings::equivalence::stabilize_settings_reference_ids;

    // Every instance with its UniqueId alone, which says where Studio's
    // stand among them. A store this reader refuses is the whole decode's to
    // refuse. The files' instances give their UniqueIds as it reads them
    // where none is a held payload or stands for a pool entry.
    let as_read = |instances: &[SettingsBytecodeInstance]| {
        use crate::settings::bytecode::blocks::{holds_marker, is_block_store};
        use crate::settings::held::holds_a_handle;
        let plain = |value: &serde_json::Value| !holds_marker(value) && !holds_a_handle(value);
        is_block_store(desired_bytes)
            && (instances.iter()).all(|instance| instance.properties.values().all(plain))
    };
    let unique_ids = previous
        .instances
        .iter()
        .filter_map(|instance| instance.properties.get("UniqueId"))
        .collect::<Vec<_>>();
    let read = match files.filter(|instances| as_read(instances)) {
        Some(instances) => retained_row_columns(desired_bytes).ok().map(|columns| {
            let version = SETTINGS_BINARY_VERSION;
            (SettingsBytecode { version, instances }, columns)
        }),
        None => decode_for_row_chunks(desired_bytes)
            .ok()
            .and_then(|(mut structure, columns)| {
                if unique_ids.is_empty() {
                    return Some((structure, columns));
                }
                let unique = |name: &str| name == "UniqueId";
                let projected =
                    columns.project_properties(&mut structure, unique, |_, _, value| {
                        unique_ids.contains(&&value).then_some(value)
                    });
                projected.ok().map(|()| (structure, columns))
            }),
    };
    let Some((mut current, columns)) = read else {
        return Ok(None);
    };
    let skeleton = &current.instances;
    let count = skeleton.len();
    // The identities of a store this much smaller are matched with the
    // instances of the files that share an id or a name and class with one
    // of them, or its UniqueId.
    if previous.instances.len() >= count / 4 {
        return Ok(None);
    }
    let chosen = crate::app::profile::span("store instances an import reads");
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
    let mut match_read = vec![false; count];
    let mut script_read = vec![false; count];
    for (index, instance) in skeleton.iter().enumerate() {
        let Some(parent) = instance.parent_index else {
            match_read[index] = true;
            continue;
        };
        if ids.contains(instance.settings_id.as_str())
            || named.contains(&(instance.name.as_str(), instance.class_name.as_str()))
            || instance
                .properties
                .get("UniqueId")
                .is_some_and(|unique_id| unique_ids.contains(&unique_id))
            || instance.class_name == "Camera"
            || skeleton.get(parent).is_some_and(|parent| {
                parent.parent_index.is_none()
                    && crate::roblox::services::is_engine_managed_container(
                        &parent.class_name,
                        &instance.class_name,
                    )
            })
        {
            match_read[index] = true;
        }
        if crate::editor::sync::is_lua_source_class(&instance.class_name) {
            script_read[index] = true;
        }
    }
    let mark_ancestors = |flags: &mut [bool]| {
        for index in 0..count {
            if !flags[index] {
                continue;
            }
            let mut parent = skeleton[index].parent_index;
            let mut steps = 0;
            while let Some(at) = parent.filter(|at| *at < count && !flags[*at]) {
                flags[at] = true;
                parent = skeleton[at].parent_index;
                steps += 1;
                if steps > count {
                    break;
                }
            }
        }
    };
    mark_ancestors(&mut match_read);
    mark_ancestors(&mut script_read);
    let namesakes = (0..count)
        .filter(|&index| match_read[index])
        .map(|index| (skeleton[index].parent_index, skeleton[index].name.as_str()))
        .collect::<HashSet<_>>();
    // The import returns the rows for all post-import fields, including pivots.
    let mut wanted = vec![false; count];
    let mut wanted_count = 0usize;
    for index in 0..count {
        if script_read[index]
            || namesakes.contains(&(skeleton[index].parent_index, skeleton[index].name.as_str()))
        {
            wanted[index] = true;
            wanted_count += 1;
        }
    }
    drop(namesakes);
    if wanted_count == count {
        return Ok(None);
    }
    drop(chosen);
    if columns.materialize_selected(&mut current, &wanted).is_err() {
        return Ok(None);
    }
    let _span = crate::app::profile::span("store instances settled for import");
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

/// The change of a store the files hold edited since Live Sync pushed it
/// last, from the instances that differ between the two copies
/// (`changed_instances_alone`, of both). The copies share their ids and Live
/// Sync aligns neither, so the change is taken only where the alignment
/// leaves each instance of the earlier copy the id it had, and each instance
/// read is the one `editor_settings_document` gives. `None` where the whole
/// documents are prepared: for copies that hold the same, which no instance
/// read here tells from copies under other ids, and for what this reading
/// refuses, which is the whole decode's to refuse.
pub(crate) fn edited_store_alone(
    path: &Path,
    current: Option<&SnapshotEntry>,
    previous: Option<&SnapshotEntry>,
) -> Option<PreparedEditorSettingsChange> {
    let how = (true, false, false, true);
    match changed_instances_compared(path, current, previous, how) {
        Ok(ChangedInstances::Prepared(change)) => Some(*change),
        _ => None,
    }
}

fn replaced_store_for_diff(
    (_path, left, right): (&Path, Option<&SnapshotEntry>, Option<&SnapshotEntry>),
    prepared: Option<&mut Option<PreparedEditorSettingsChange>>,
) -> Result<Option<bool>> {
    let Some((mut current, mut previous)) = replaced_store_alone(left, right)? else {
        return Ok(None);
    };
    if !align_settings_ids_to_reference(&current, &mut previous) {
        return Ok(None);
    }
    read_where_paired(&mut current, &previous, left)?;
    if settings_documents_equivalent(&current, &previous) {
        return Ok(Some(true));
    }
    if let Some(prepared) = prepared {
        crate::settings::equivalence::inherit_workspace_viewport_reference(&mut current, &previous);
        align_equivalent_values(&current, &mut previous);
        *prepared = Some(PreparedEditorSettingsChange { previous, current });
    }
    Ok(Some(false))
}

/// Whether two copies of a store are equivalent, by the instances that
/// differ (`changed_instances_alone`), with the change where they are not
/// and `prepared` takes one. `None` where the whole documents have to say
/// (`snapshot_entry_equivalent`).
pub(super) fn store_copies_by_changed(
    (path, left, right): (&Path, Option<&SnapshotEntry>, Option<&SnapshotEntry>),
    prepared: Option<&mut Option<PreparedEditorSettingsChange>>,
) -> Result<Option<bool>> {
    Ok(
        match changed_instances_alone(path, left, right, true, false, false)? {
            ChangedInstances::None => Some(true),
            ChangedInstances::Prepared(mut change) => {
                if let Some(prepared) = prepared {
                    align_equivalent_values(&change.current, &mut change.previous);
                    *prepared = Some(*change);
                }
                Some(false)
            }
            ChangedInstances::Whole(None) => {
                return replaced_store_for_diff((path, left, right), prepared);
            }
            _ => None,
        },
    )
}

/// The paths at which two snapshots differ, with the change of each store
/// where `prepared` takes them. With `by_changed` a store is compared by
/// the instances that differ where that says as much as its whole documents.
pub(crate) fn snapshot_differences_prepared(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
    mut prepared: Option<&mut HashMap<PathBuf, PreparedEditorSettingsChange>>,
    by_changed: bool,
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
            let (ours, theirs) = (left.entries.get(&path), right.entries.get(&path));
            let is_store = ours != theirs
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_service_settings_file_name);
            let told = if by_changed && is_store {
                store_copies_by_changed((&path, ours, theirs), prepare.then_some(&mut settings))?
            } else if !by_changed && prepare && is_store {
                replaced_store_for_diff((&path, ours, theirs), Some(&mut settings))?
            } else {
                None
            };
            let equivalent = match told {
                Some(equivalent) => equivalent,
                None => snapshot_entry_equivalent(
                    &path,
                    ours,
                    theirs,
                    prepare.then_some(&mut settings),
                )?,
            };
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

// One by one a new instance without such a field costs Studio about four
// times what the import spends on an instance of the service, so that many
// new instances are imported only where they are a quarter of it.
const HEAVY_DELTA_NEW_SERVICE_RATIO: usize = 4;

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
        (self.new_instances >= HEAVY_DELTA_NEW_INSTANCES
            && self.new_instances * HEAVY_DELTA_NEW_SERVICE_RATIO >= self.service_instances)
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
    let mut file_writes: Vec<(PathBuf, &[u8])> = Vec::new();
    let mut parents_created = HashSet::<PathBuf>::new();
    for relative in paths {
        if derived_project_path(relative) {
            continue;
        }
        let path = root.join(relative);
        match snapshot.entries.get(relative) {
            None => remove_path(&path)?,
            Some(SnapshotEntry::Directory) => {
                if fs::symlink_metadata(&path)
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
                {
                    continue;
                }
                fs::create_dir_all(&path)
                    .with_context(|| format!("Failed to create {}", path.display()))?;
            }
            Some(SnapshotEntry::File(bytes)) => {
                if let Some(parent) = path.parent()
                    && parents_created.insert(parent.to_path_buf())
                {
                    fs::create_dir_all(parent)?;
                }
                file_writes.push((path, bytes.as_slice()));
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
    file_writes
        .into_par_iter()
        .try_for_each(|(path, bytes)| -> Result<()> {
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                    if meta.len() == bytes.len() as u64
                        && fs::read(&path).ok().as_deref() == Some(bytes)
                    {
                        return Ok(());
                    }
                    fs::write(&path, bytes)
                        .with_context(|| format!("Failed to write {}", path.display()))?;
                    return Ok(());
                }
                Ok(_) => remove_path(&path)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to inspect {}", path.display()));
                }
            }
            fs::write(&path, bytes)
                .with_context(|| format!("Failed to write {}", path.display()))?;
            Ok(())
        })?;
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
#[path = "replacement_tests.rs"]
mod replacement_tests;

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
        let large = document(HEAVY_DELTA_NEW_INSTANCES * 4, "Folder");
        let larger = document(HEAVY_DELTA_NEW_INSTANCES * 5, "Folder");
        assert!(delta_weight(&larger, &large).new_instances == HEAVY_DELTA_NEW_INSTANCES);
        assert!(!delta_weight(&larger, &large).heavy());
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

    #[test]
    fn replaced_store_alone_enforces_size_ratio_in_tests() {
        let large = document(80, "Part");
        let small = document(1, "Part");
        let medium = document(40, "Part");
        let large_bytes = crate::settings::bytecode::encode_settings_bytecode(&large).unwrap();
        let small_bytes = crate::settings::bytecode::encode_settings_bytecode(&small).unwrap();
        let medium_bytes = crate::settings::bytecode::encode_settings_bytecode(&medium).unwrap();

        let desired = SnapshotEntry::File(large_bytes);
        let tiny_obs = SnapshotEntry::File(small_bytes);
        let half_obs = SnapshotEntry::File(medium_bytes);

        let result = replaced_store_alone(Some(&desired), Some(&tiny_obs)).unwrap();
        assert!(result.is_some());
        let refused = replaced_store_alone(Some(&desired), Some(&half_obs)).unwrap();
        assert!(refused.is_none());
    }

    #[test]
    fn prepare_project_replacement_with_bulk_emits_bulk_patch_without_promoting_service() {
        let root = crate::tests::support::temp_dir("r10-replacement-bulk-branch");
        let src = root.join("src");
        let ws_dir = src.join("ReplicatedStorage");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let store_rel = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");

        let count = BULK_PROPERTY_MIN_INSTANCES;
        let mut studio_doc = document(count, "Part");
        let mut desired_doc = document(count, "Part");
        for i in 1..=count {
            studio_doc.instances[i]
                .properties
                .insert("Transparency".into(), serde_json::json!(0.0));
            desired_doc.instances[i]
                .properties
                .insert("Transparency".into(), serde_json::json!(0.5));
        }
        let studio_bytes =
            crate::settings::bytecode::encode_settings_bytecode(&studio_doc).unwrap();
        let desired_bytes =
            crate::settings::bytecode::encode_settings_bytecode(&desired_doc).unwrap();

        let studio = ProjectSnapshot {
            entries: std::collections::BTreeMap::from([(
                store_rel.clone(),
                SnapshotEntry::File(studio_bytes),
            )]),
        };
        let desired = ProjectSnapshot {
            entries: std::collections::BTreeMap::from([(
                store_rel.clone(),
                SnapshotEntry::File(desired_bytes),
            )]),
        };
        let paths = HashSet::from([store_rel.clone()]);
        let mut prepared = HashMap::new();
        let bulk_patches = std::sync::Mutex::new(HashMap::new());
        let unchanged = prepare_project_replacement_with_bulk(
            &desired,
            &studio,
            &paths,
            &mut prepared,
            StoresRead::ChangedOfBoth,
            None,
            Some(&bulk_patches),
        )
        .unwrap();
        assert!(
            prepared.is_empty(),
            "plain property edit must not promote Workspace to prepared/native replacement"
        );
        assert!(
            unchanged.is_empty(),
            "modified Workspace must not be marked unchanged"
        );
        let patches = bulk_patches.into_inner().unwrap();
        assert!(
            patches.contains_key(&store_rel),
            "R10: prepare_project_replacement_with_bulk must populate bulk_patches for Workspace"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hybrid_delta_reads_all_40_same_named_siblings_and_remaps_references() {
        use crate::settings::bytecode::blocks::bulk_diff::{
            BulkDiffOutcome, diff_v12_bulk_plain_properties,
        };
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "cam".into(),
                    "Camera".into(),
                    "Camera".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new(
                    "holder".into(),
                    "Folder".into(),
                    "Folder".into(),
                    Some(0),
                ),
            ],
        };
        desired.instances[0].properties.insert(
            "CurrentCamera".into(),
            serde_json::json!({"_type": "Ref", "settingsId": "cam", "instanceIndex": 2}),
        );
        for i in 0..40 {
            let mut p = SettingsBytecodeInstance::new(
                format!("sib-{i}"),
                "Same".into(),
                "Part".into(),
                Some(2),
            );
            p.properties.insert(
                "UniqueId".into(),
                serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", i + 100)}),
            );
            p.properties
                .insert("Transparency".into(), serde_json::json!(0.0));
            desired.instances.push(p);
        }
        let mut observed = desired.clone();
        for (idx, inst) in observed.instances.iter_mut().enumerate() {
            inst.settings_id = format!("obs-{idx}");
        }
        observed.instances[0].properties.insert(
            "CurrentCamera".into(),
            serde_json::json!({"_type": "Ref", "settingsId": "obs-1", "instanceIndex": 2}),
        );
        for inst in observed.instances.iter_mut().skip(3) {
            inst.properties
                .insert("Transparency".into(), serde_json::json!(0.5));
        }
        observed.instances[3 + 24].name = "SameRenamed".into();
        let des_bytes = crate::settings::bytecode::encode_settings_bytecode(&desired).unwrap();
        let obs_bytes = crate::settings::bytecode::encode_settings_bytecode(&observed).unwrap();
        let BulkDiffOutcome::Hybrid(hybrid) =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes, "Workspace", 8).unwrap()
        else {
            panic!("expected Hybrid outcome for 40-sibling rename + mass Transparency edit");
        };
        let prepared = prepare_hybrid_bulk_delta(&des_bytes, &obs_bytes, *hybrid, false)
            .unwrap()
            .expect("expected prepared hybrid delta");
        for idx in 3..43 {
            assert!(
                !crate::settings::held::stands_unchanged(
                    &prepared.current.instances[idx].properties
                ),
                "sibling {idx} in desired must be materialized"
            );
            assert!(
                !crate::settings::held::stands_unchanged(
                    &prepared.previous.instances[idx].properties
                ),
                "sibling {idx} in observed must be materialized"
            );
        }
        let prev_cam = prepared.previous.instances[0]
            .properties
            .get("CurrentCamera")
            .unwrap();
        assert_eq!(prev_cam["settingsId"], "cam");
    }

    #[test]
    fn by_order_pairing_accepts_unchanged_twin_ref_and_rejects_swapped_twin_ref() {
        let make_doc = |uid_base: u64, obs_ids: bool, swap_ref: bool, change_target: bool| {
            let id = |s: &str| {
                if obs_ids {
                    format!("obs-{s}")
                } else {
                    s.to_string()
                }
            };
            let mut instances = vec![SettingsBytecodeInstance::new(
                id("ws"),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )];
            for t in 0..2 {
                let mut twin = SettingsBytecodeInstance::new(
                    id(&format!("twin-{t}")),
                    "Twin".into(),
                    "Part".into(),
                    Some(0),
                );
                twin.properties.insert(
                    "UniqueId".into(),
                    serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + t)}),
                );
                instances.push(twin);
            }
            let target_ref = if swap_ref { 2 } else { 1 };
            let target_id = id(if swap_ref { "twin-1" } else { "twin-0" });
            let mut weld =
                SettingsBytecodeInstance::new(id("weld"), "Weld".into(), "Weld".into(), Some(0));
            weld.properties.insert(
                "UniqueId".into(),
                serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + 10)}),
            );
            weld.properties.insert(
                "Part0".into(),
                serde_json::json!({"_type": "Ref", "settingsId": target_id, "instanceIndex": target_ref + 1}),
            );
            instances.push(weld);
            let mut changed = SettingsBytecodeInstance::new(
                id("changed"),
                "Changed".into(),
                "Part".into(),
                Some(0),
            );
            changed.properties.insert(
                "UniqueId".into(),
                serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + 20)}),
            );
            changed.properties.insert(
                "Transparency".into(),
                serde_json::json!(if change_target { 0.5 } else { 0.0 }),
            );
            instances.push(changed);
            for i in 0..512 {
                let mut filler = SettingsBytecodeInstance::new(
                    id(&format!("pad-{i}")),
                    format!("Pad_{i}"),
                    "Part".into(),
                    Some(0),
                );
                filler.properties.insert(
                    "UniqueId".into(),
                    serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + 100 + i)}),
                );
                instances.push(filler);
            }
            if obs_ids {
                instances.swap(5, 6);
            }
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            }
        };
        let desired_doc = make_doc(1, false, false, false);
        let obs_unchanged_ref = make_doc(10_000, true, false, true);
        let obs_swapped_ref = make_doc(20_000, true, true, true);
        let desired_entry = SnapshotEntry::File(
            crate::settings::bytecode::encode_settings_bytecode(&desired_doc).unwrap(),
        );
        let obs_same_entry = SnapshotEntry::File(
            crate::settings::bytecode::encode_settings_bytecode(&obs_unchanged_ref).unwrap(),
        );
        let obs_swap_entry = SnapshotEntry::File(
            crate::settings::bytecode::encode_settings_bytecode(&obs_swapped_ref).unwrap(),
        );

        let before_accepted = BY_ORDER[1].load(std::sync::atomic::Ordering::Relaxed);
        let path = Path::new("instances/Workspace.renium");
        let res = changed_instances_alone(
            path,
            Some(&desired_entry),
            Some(&obs_same_entry),
            true,
            false,
            true,
        )
        .unwrap();
        assert!(matches!(res, ChangedInstances::Prepared(_)));
        assert!(
            BY_ORDER[1].load(std::sync::atomic::Ordering::Relaxed) > before_accepted,
            "unchanged twin reference must accept by-order pairing"
        );

        let before_refused = BY_ORDER[0].load(std::sync::atomic::Ordering::Relaxed);
        let res_swap = changed_instances_alone(
            path,
            Some(&desired_entry),
            Some(&obs_swap_entry),
            true,
            false,
            true,
        )
        .unwrap();
        assert!(matches!(
            res_swap,
            ChangedInstances::Prepared(_) | ChangedInstances::Whole(_)
        ));
        assert!(
            BY_ORDER[0].load(std::sync::atomic::Ordering::Relaxed) > before_refused,
            "swapped twin reference must reject by-order pairing and fall back"
        );
    }

    #[test]
    fn by_order_pairing_accepts_reordered_humanoid_description_lists() {
        let make_doc = |uid_base: u64, accessories: &str, changed_val: f64, swap_padding: bool| {
            let id = |s: &str| format!("id-{uid_base}-{s}");
            let mut instances = vec![SettingsBytecodeInstance::new(
                id("ws"),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )];
            for t in 0..2 {
                let model_idx = instances.len();
                let mut model = SettingsBytecodeInstance::new(
                    id(&format!("npc-{t}")),
                    "Model".into(),
                    "Model".into(),
                    Some(0),
                );
                model.properties.insert(
                    "UniqueId".into(),
                    serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + t * 10)}),
                );
                instances.push(model);
                let hum_idx = instances.len();
                let mut hum = SettingsBytecodeInstance::new(
                    id(&format!("hum-{t}")),
                    "Humanoid".into(),
                    "Humanoid".into(),
                    Some(model_idx),
                );
                hum.properties.insert(
                    "UniqueId".into(),
                    serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + t * 10 + 1)}),
                );
                instances.push(hum);
                let mut hd = SettingsBytecodeInstance::new(
                    id(&format!("hd-{t}")),
                    "HumanoidDescription".into(),
                    "HumanoidDescription".into(),
                    Some(hum_idx),
                );
                hd.properties.insert(
                    "UniqueId".into(),
                    serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + t * 10 + 2)}),
                );
                hd.properties.insert(
                    "HatAccessory".into(),
                    serde_json::json!(if t == 0 { accessories } else { "100,200" }),
                );
                instances.push(hd);
            }
            let mut changed = SettingsBytecodeInstance::new(
                id("changed"),
                "Changed".into(),
                "Part".into(),
                Some(0),
            );
            changed.properties.insert(
                "UniqueId".into(),
                serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + 50)}),
            );
            changed
                .properties
                .insert("Transparency".into(), serde_json::json!(changed_val));
            instances.push(changed);
            let pad_start = instances.len();
            for p in 0..320 {
                let mut part = SettingsBytecodeInstance::new(
                    id(&format!("pad-{p}")),
                    format!("Pad{p}"),
                    "Part".into(),
                    Some(0),
                );
                part.properties.insert(
                    "UniqueId".into(),
                    serde_json::json!({"_type": "UniqueId", "value": format!("{:032x}", uid_base + 100 + p as u64)}),
                );
                instances.push(part);
            }
            if swap_padding {
                instances.swap(pad_start, pad_start + 1);
            }
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances,
            }
        };
        let desired_entry = SnapshotEntry::File(
            crate::settings::bytecode::encode_settings_bytecode(&make_doc(
                1, "11,22,33", 0.0, false,
            ))
            .unwrap(),
        );
        let obs_reordered = SnapshotEntry::File(
            crate::settings::bytecode::encode_settings_bytecode(&make_doc(
                10_000, "33,11,22", 0.25, true,
            ))
            .unwrap(),
        );
        let obs_mutated = SnapshotEntry::File(
            crate::settings::bytecode::encode_settings_bytecode(&make_doc(
                20_000, "11,22,99", 0.25, true,
            ))
            .unwrap(),
        );
        let path = Path::new("instances/Workspace.renium");

        let before_accepted = BY_ORDER[1].load(std::sync::atomic::Ordering::Relaxed);
        let before_refused = BY_ORDER[0].load(std::sync::atomic::Ordering::Relaxed);
        let res = changed_instances_alone(
            path,
            Some(&desired_entry),
            Some(&obs_reordered),
            true,
            false,
            true,
        )
        .unwrap();
        assert!(matches!(res, ChangedInstances::Prepared(_)));
        assert!(
            BY_ORDER[1].load(std::sync::atomic::Ordering::Relaxed) > before_accepted,
            "equivalent reordered HumanoidDescription accessory list must stay on by-order fast path"
        );
        assert_eq!(
            BY_ORDER[0].load(std::sync::atomic::Ordering::Relaxed),
            before_refused,
            "equivalent reordered HumanoidDescription accessory list must not reject by-order"
        );

        let res_mutated = changed_instances_alone(
            path,
            Some(&desired_entry),
            Some(&obs_mutated),
            true,
            false,
            true,
        )
        .unwrap();
        assert!(matches!(
            res_mutated,
            ChangedInstances::Prepared(_) | ChangedInstances::Whole(_)
        ));
        assert!(
            BY_ORDER[0].load(std::sync::atomic::Ordering::Relaxed) > before_refused,
            "genuinely changed HumanoidDescription under a twin must reject by-order"
        );
    }
}
