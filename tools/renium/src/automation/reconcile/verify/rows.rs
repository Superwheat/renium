//! Verification of a pushed store a range of rows at a time. Neither copy of
//! the store is whole in memory: each is read as its encoded columns, and a
//! few thousand instances of both are decoded, compared and dropped.
use super::*;
use crate::editor::review::EditorReferenceIds;
use crate::settings::bytecode::property_edit::{decode_for_property_edit, decode_for_row_chunks};
use crate::settings::equivalence::{
    canonicalize_instances, instance_places, remap_rows, stabilize_rows,
    workspace_current_camera_index,
};
use std::collections::BTreeMap;

/// The instances of each copy that are decoded at one time.
const ROWS_AT_A_TIME: usize = if cfg!(test) { 3 } else { 8_192 };

/// A store with fewer instances is verified from its whole documents, which
/// are small.
pub(in crate::automation::reconcile) const MIN_ROWS: usize = if cfg!(test) { 2 } else { 16_384 };

fn project_uid_and_camera(
    document: &mut SettingsBytecode,
    rows: &crate::settings::bytecode::property_edit::RetainedRowColumns,
) -> Option<(Vec<String>, Vec<usize>)> {
    let stands = |name: &str| name == "UniqueId" || name == "CurrentCamera";
    let of_place = |instance: &SettingsBytecodeInstance, name: &str, value: Value| {
        (name == "UniqueId" || instance.parent_index.is_none()).then_some(value)
    };
    rows.project_properties(document, stands, of_place).ok()?;
    let ids: Vec<String> = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect();
    let roots: Vec<usize> = (0..document.instances.len())
        .filter(|&index| document.instances[index].parent_index.is_none())
        .collect();
    for &root in &roots {
        stabilize_rows(document, root..root + 1, &ids);
    }
    Some((ids, roots))
}

fn apply_viewport_camera_override(
    desired: &SettingsBytecode,
    observed: &mut SettingsBytecode,
    cam_d: usize,
    cam_o: usize,
) -> bool {
    let saved = &desired.instances[cam_d];
    let held = &mut observed.instances[cam_o];
    if saved.class_name == "Camera"
        && (held.properties != saved.properties || held.attributes != saved.attributes)
    {
        held.properties.clone_from(&saved.properties);
        held.attributes.clone_from(&saved.attributes);
        return true;
    }
    false
}

fn settled_bytes_from_observed(
    observed: &SettingsBytecode,
    observed_rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
    observed_bytes: &[u8],
    studio_ids: &[String],
    placed_ids: &[String],
    settled: &BTreeMap<usize, SettingsBytecodeInstance>,
    settle: bool,
) -> Result<Option<Vec<u8>>> {
    if !settle {
        return Ok(None);
    }
    let bare_records: Vec<SettingsBytecodeInstance> = observed
        .instances
        .iter()
        .zip(studio_ids)
        .map(|(instance, id)| {
            SettingsBytecodeInstance::new(
                id.clone(),
                instance.name.clone(),
                instance.class_name.clone(),
                instance.parent_index,
            )
        })
        .collect();
    Ok(Some(settle_opened_store(
        observed_rows,
        observed_bytes,
        bare_records,
        placed_ids,
        settled,
    )?))
}

fn finish_settled_rows(
    desired: SettingsBytecode,
    observed: SettingsBytecode,
    desired_rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
    observed_rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
    lazy_before: Option<LazyBefore>,
    (observed_bytes, studio_ids, placed_ids, settled): (
        &[u8],
        &[String],
        &[String],
        &BTreeMap<usize, SettingsBytecodeInstance>,
    ),
    settle: bool,
) -> Result<Option<SettingsVerification>> {
    drop((desired_rows, lazy_before));
    let accepted = settled_bytes_from_observed(
        &observed,
        observed_rows,
        observed_bytes,
        studio_ids,
        placed_ids,
        settled,
        settle,
    )?;
    rayon::spawn(move || drop((desired, observed)));
    Ok(Some(SettingsVerification {
        mismatch: None,
        accepted,
    }))
}

fn clear_non_root_fields(document: &mut SettingsBytecode, indices: &[usize]) {
    let mut released = Vec::with_capacity(indices.len());
    for &index in indices {
        let instance = &mut document.instances[index];
        if instance.parent_index.is_some() {
            released.push((
                std::mem::take(&mut instance.properties),
                std::mem::take(&mut instance.attributes),
            ));
        }
    }
    if released.len() >= 1_024 {
        rayon::spawn(move || released.into_par_iter().for_each(drop));
    }
}

/// Lazily opens `before_bytes` as a lightweight skeleton and materializes only
/// the specific rows that differ or need checking, so verification never decodes
/// three whole 200k-instance documents side by side.
struct LazyBefore {
    doc: SettingsBytecode,
    rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
    raw_ids: Vec<String>,
    remap: HashMap<String, String>,
    by_id: AHashMap<String, usize>,
    service: String,
}

impl LazyBefore {
    fn open(
        path: &Path,
        before_bytes: &[u8],
        desired_with_uids: &SettingsBytecode,
        desired_rows: &crate::settings::bytecode::property_edit::RetainedRowColumns,
        service: &str,
    ) -> Result<Option<Self>> {
        let Ok((mut doc, rows)) = decode_for_row_chunks(before_bytes) else {
            return Ok(None);
        };
        if rows.block_store().is_none() && !doc.instances.is_empty() {
            return Ok(None);
        }
        if !doc.instances.is_empty() {
            let Some((raw_ids, roots)) = project_uid_and_camera(&mut doc, &rows) else {
                return Ok(None);
            };
            let Some(aligned_ids) =
                align_skeleton_to_reference(desired_with_uids, desired_rows, &doc, &rows)?
            else {
                bail!(
                    "Could not align the previous identities in {}",
                    path.display()
                );
            };
            let remap: HashMap<String, String> = raw_ids
                .iter()
                .cloned()
                .zip(aligned_ids.iter().cloned())
                .collect();
            for (instance, id) in doc.instances.iter_mut().zip(&aligned_ids) {
                instance.settings_id.clone_from(id);
                if instance.parent_index.is_some() {
                    instance.properties = Map::new();
                }
            }
            for &root in &roots {
                remap_rows(&mut doc, root..root + 1, &remap, service);
            }
            let by_id: AHashMap<String, usize> = doc
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.clone(), index))
                .collect();
            return Ok(Some(Self {
                doc,
                rows,
                raw_ids,
                remap,
                by_id,
                service: service.to_string(),
            }));
        }
        Ok(Some(Self {
            doc,
            rows,
            raw_ids: Vec::new(),
            remap: HashMap::new(),
            by_id: AHashMap::new(),
            service: service.to_string(),
        }))
    }

    fn materialize_indices(&mut self, indices: &[usize]) -> Result<()> {
        if indices.is_empty() || self.doc.instances.is_empty() {
            return Ok(());
        }
        let mut wanted = vec![false; self.doc.instances.len()];
        for &index in indices {
            wanted[index] = true;
        }
        self.rows
            .materialize_selected_in_place(&mut self.doc, &wanted)?;
        for &index in indices {
            let range = index..index + 1;
            stabilize_rows(&mut self.doc, range.clone(), &self.raw_ids);
            remap_rows(&mut self.doc, range.clone(), &self.remap, &self.service);
            canonicalize_instances(&mut self.doc.instances[range])?;
        }
        Ok(())
    }

    fn release_indices(&mut self, indices: &[usize]) {
        clear_non_root_fields(&mut self.doc, indices);
    }
}

/// Aligns a skeleton `observed` (holding only `UniqueId` + root `CurrentCamera`)
/// against `reference` (likewise). Uses `settings_ids_told_alone` first; when
/// namesakes or refs require full property/reference tie-breaking, delegates to
/// streamed compact block-store matching so every row's properties and
/// references participate without decoding whole documents.
fn align_skeleton_to_reference(
    reference: &SettingsBytecode,
    reference_rows: &crate::settings::bytecode::property_edit::RetainedRowColumns,
    observed: &SettingsBytecode,
    observed_rows: &crate::settings::bytecode::property_edit::RetainedRowColumns,
) -> Result<Option<Vec<String>>> {
    if observed.instances.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if let Some((ids, _)) =
        crate::settings::equivalence::settings_ids_told_alone(reference, observed)
    {
        return Ok(Some(ids));
    }
    let (Some(ref_store), Some(obs_store)) =
        (reference_rows.block_store(), observed_rows.block_store())
    else {
        return Ok(None);
    };
    let matched =
        crate::settings::equivalence::match_opened_block_stores_compact(ref_store, obs_store)?;
    Ok(matched.map(|(ids, _)| ids))
}

fn build_uid_skeleton(document: &SettingsBytecode, uids: &[Option<Value>]) -> SettingsBytecode {
    SettingsBytecode {
        version: document.version,
        instances: document
            .instances
            .iter()
            .zip(uids)
            .map(|(inst, uid)| {
                let mut copy = SettingsBytecodeInstance::new(
                    inst.settings_id.clone(),
                    inst.name.clone(),
                    inst.class_name.clone(),
                    inst.parent_index,
                );
                if inst.parent_index.is_none() {
                    copy.properties.clone_from(&inst.properties);
                } else if let Some(val) = uid {
                    copy.properties.insert("UniqueId".into(), val.clone());
                }
                copy
            })
            .collect(),
    }
}

/// Evaluates a materialized slice of `desired` rows against `observed` (and
/// lazily-materialized `before_bytes` when `before` is `None`), returning the
/// earliest mismatch in `settings_ids` order without decoding whole documents.
#[allow(clippy::too_many_arguments)]
fn check_materialized_rows_with_lazy_before(
    path: &Path,
    before: Option<(&SettingsBytecode, &AHashMap<&str, usize>)>,
    lazy_before: &mut Option<LazyBefore>,
    before_bytes: Option<&[u8]>,
    (desired, observed): (&SettingsBytecode, &mut SettingsBytecode),
    desired_rows: &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
    desired_uids: &[Option<Value>],
    (rows, places, desired_ids, service): (&[usize], &[usize], &[String], &str),
    (protected, policy, by_order): (Option<usize>, Verification<'_>, &[bool]),
) -> Result<Option<Option<String>>> {
    if rows.is_empty() {
        return Ok(Some(None));
    }
    if rows.iter().any(|&index| by_order[index]) {
        return Ok(None);
    }
    if let Some((before_doc, before_by_id)) = before {
        let ranks = row_settings_ranks(before_doc, before_by_id, desired_ids);
        let found = best_mismatch_in_rows(
            (before_doc, before_by_id),
            (desired, observed),
            (rows, places, &ranks),
            (protected, policy.new_instance_values, usize::MAX),
        );
        return Ok(Some(found.map(|(_, detail)| detail)));
    }
    if let Some(bytes) = before_bytes {
        if lazy_before.is_none() {
            let uid_skel = build_uid_skeleton(desired, desired_uids);
            let Some(opened) = LazyBefore::open(path, bytes, &uid_skel, desired_rows, service)?
            else {
                return Ok(None);
            };
            *lazy_before = Some(opened);
        }
        let lb = lazy_before.as_mut().expect("opened above");
        let before_indices: Vec<usize> = rows
            .iter()
            .filter_map(|&d| lb.by_id.get(desired_ids[d].as_str()).copied())
            .collect();
        lb.materialize_indices(&before_indices)?;
        let before_by_id_ref: AHashMap<&str, usize> =
            lb.by_id.iter().map(|(k, &v)| (k.as_str(), v)).collect();
        let ranks = row_settings_ranks(&lb.doc, &before_by_id_ref, desired_ids);
        let found = best_mismatch_in_rows(
            (&lb.doc, &before_by_id_ref),
            (desired, observed),
            (rows, places, &ranks),
            (protected, policy.new_instance_values, usize::MAX),
        );
        lb.release_indices(&before_indices);
        return Ok(Some(found.map(|(_, detail)| detail)));
    }
    let mut sorted_rows = rows.to_vec();
    sorted_rows.sort_unstable();
    for index in sorted_rows {
        let place = places[index];
        if protected == Some(place) {
            continue;
        }
        let Some(detail) = added_instance_mismatch(desired, index, observed, place) else {
            continue;
        };
        let name = &desired.instances[index].name;
        let final_detail = match policy.new_instance_values {
            Some(values) => match values.mismatch(None, (desired, index), (observed, place)) {
                Some(detail) => detail,
                None => continue,
            },
            None => detail,
        };
        return Ok(Some(Some(format!("{name}.{final_detail}"))));
    }
    Ok(Some(None))
}

type RowIdIndex = AHashMap<String, usize>;
type StructuralScanResult = (
    Option<(usize, String)>,
    Vec<(usize, Option<usize>, usize, usize)>,
);

fn index_rows_by_id(document: &SettingsBytecode) -> RowIdIndex {
    document
        .instances
        .iter()
        .enumerate()
        .map(|(i, inst)| (inst.settings_id.clone(), i))
        .collect()
}

fn collect_ordered_settings_ids(
    before: Option<&SettingsBytecode>,
    before_by_id: &RowIdIndex,
    desired: &SettingsBytecode,
) -> Vec<String> {
    let mut settings_ids =
        Vec::with_capacity(before.map_or(0, |d| d.instances.len()) + desired.instances.len());
    if let Some(doc) = before {
        settings_ids.extend(doc.instances.iter().map(|inst| inst.settings_id.clone()));
    }
    for inst in &desired.instances {
        if !before_by_id.contains_key(&inst.settings_id) {
            settings_ids.push(inst.settings_id.clone());
        }
    }
    settings_ids
}

fn scan_structural_mismatches_and_pairs(
    settings_ids: &[String],
    before_doc_opt: Option<&SettingsBytecode>,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
    maps: (&RowIdIndex, &RowIdIndex, &RowIdIndex),
) -> StructuralScanResult {
    let (before_by_id, desired_by_id, observed_by_id) = maps;
    let mut earliest_structural: Option<(usize, String)> = None;
    let mut matched_pairs: Vec<(usize, Option<usize>, usize, usize)> = Vec::new();
    for (rank, id) in settings_ids.iter().enumerate() {
        let b_idx = before_by_id.get(id).copied();
        let d_idx = desired_by_id.get(id).copied();
        let o_idx = observed_by_id.get(id).copied();
        if o_idx.is_some_and(|idx| is_reconciliation_protected_workspace_camera(observed, idx)) {
            continue;
        }
        if let (Some(b), Some(d)) = (b_idx, d_idx)
            && before_doc_opt.is_some_and(|doc| doc.instances[b].class_name == "PackageLink")
            && desired.instances[d].class_name == "PackageLink"
        {
            continue;
        }
        let name = d_idx
            .map(|idx| desired.instances[idx].name.as_str())
            .or_else(|| {
                b_idx.and_then(|idx| before_doc_opt.map(|doc| doc.instances[idx].name.as_str()))
            })
            .unwrap_or(id.as_str());
        match (b_idx, d_idx, o_idx) {
            (Some(b), None, Some(o)) if earliest_structural.is_none() => {
                let prev = &before_doc_opt.expect("present").instances[b];
                let actual = &observed.instances[o];
                let keep_root = prev.parent_index.is_none()
                    && actual.parent_index.is_none()
                    && prev.class_name == actual.class_name
                    && prev.name == actual.name;
                let keep_container =
                    is_protected_engine_container(before_doc_opt.expect("present"), b)
                        && is_protected_engine_container(observed, o)
                        && prev.class_name == actual.class_name;
                if !keep_root && !keep_container {
                    earliest_structural =
                        Some((rank, format!("{name} was not deleted from Studio")));
                }
            }
            (None, Some(_), None) if earliest_structural.is_none() => {
                earliest_structural = Some((rank, format!("{name} was not created in Studio")));
            }
            (Some(_), Some(_), None) if earliest_structural.is_none() => {
                earliest_structural = Some((rank, format!("{name} disappeared from Studio")));
            }
            (b_opt, Some(d), Some(o))
                if earliest_structural
                    .as_ref()
                    .is_none_or(|(s_rank, _)| rank < *s_rank) =>
            {
                matched_pairs.push((rank, b_opt, d, o));
            }
            _ => {}
        }
    }
    (earliest_structural, matched_pairs)
}

/// Bounded-memory verifier for large v12 block stores whose row counts differ
/// or whose places could not be paired bijectively. Keeps peak RAM bounded and
/// bails directly on v12 alignment or read errors rather than falling back to
/// 3-way whole-document decoding; returns `None` only for legacy non-v12 stores.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn verify_general_by_rows(
    path: &Path,
    before: Option<&SettingsBytecode>,
    before_bytes: Option<&[u8]>,
    (desired, desired_rows, desired_ids): (
        &mut SettingsBytecode,
        &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
        &[String],
    ),
    (observed, mut observed_rows, studio_ids): (
        &mut SettingsBytecode,
        crate::settings::bytecode::property_edit::RetainedRowColumns,
        &[String],
    ),
    observed_bytes: &[u8],
    policy: Verification<'_>,
) -> Result<Option<SettingsVerification>> {
    if desired_rows.block_store().is_none() || observed_rows.block_store().is_none() {
        return Ok(None);
    }
    let service = settings_service_name(path, desired, observed)?;
    let studio_roots: Vec<usize> = (0..observed.instances.len())
        .filter(|&index| observed.instances[index].parent_index.is_none())
        .collect();
    let Some(aligned_obs_ids) =
        align_skeleton_to_reference(desired, desired_rows, observed, &observed_rows)?
    else {
        bail!(
            "Could not align the Studio identities in {}",
            path.display()
        );
    };
    let mut lazy_before = if before.is_none()
        && let Some(bytes) = before_bytes
    {
        let Some(opened) = LazyBefore::open(path, bytes, desired, desired_rows, &service)? else {
            return Ok(None);
        };
        Some(opened)
    } else {
        None
    };

    for document in [&mut *desired, &mut *observed] {
        for instance in &mut document.instances {
            if instance.parent_index.is_some() {
                instance.properties = Map::new();
            }
        }
    }
    let obs_remap: HashMap<String, String> = studio_ids
        .iter()
        .cloned()
        .zip(aligned_obs_ids.iter().cloned())
        .collect();
    for (instance, id) in observed.instances.iter_mut().zip(&aligned_obs_ids) {
        instance.settings_id.clone_from(id);
    }
    for &root in &studio_roots {
        remap_rows(observed, root..root + 1, &obs_remap, &service);
    }

    let before_doc_opt: Option<&SettingsBytecode> =
        before.or_else(|| lazy_before.as_ref().map(|lb| &lb.doc));
    let before_by_id = before_doc_opt.map_or_else(AHashMap::new, index_rows_by_id);
    let desired_by_id = index_rows_by_id(desired);
    let observed_by_id = index_rows_by_id(observed);
    let settings_ids = collect_ordered_settings_ids(before_doc_opt, &before_by_id, desired);

    let protected = workspace_current_camera_index(observed);
    let viewport_root = desired
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none() && instance.class_name == "Workspace");
    let keeps_camera = policy.settle
        && viewport_root.is_some_and(|root| {
            !root.properties.contains_key("CurrentCamera")
                || workspace_current_camera_index(desired).and_then(|cam| {
                    observed_by_id
                        .get(&desired.instances[cam].settings_id)
                        .copied()
                }) == protected
        });

    let (earliest_structural, matched_pairs) = scan_structural_mismatches_and_pairs(
        &settings_ids,
        before_doc_opt,
        desired,
        observed,
        (&before_by_id, &desired_by_id, &observed_by_id),
    );

    let pending = pending_fields(&service, desired, policy.pending);
    let reference_ids =
        EditorReferenceIds::new(desired_ids.iter().map(String::as_str)).in_service(&service);
    let mut settled = BTreeMap::new();
    let mut best_mismatch = earliest_structural;

    for chunk in matched_pairs.chunks(ROWS_AT_A_TIME) {
        if let Some((best_rank, _)) = &best_mismatch
            && chunk[0].0 >= *best_rank
        {
            break;
        }
        let mut need_des = vec![false; desired.instances.len()];
        let mut need_obs = vec![false; observed.instances.len()];
        let mut d_indices = Vec::with_capacity(chunk.len());
        let mut o_indices = Vec::with_capacity(chunk.len());
        let mut b_indices = Vec::new();
        for &(_, b_opt, d, o) in chunk {
            need_des[d] = true;
            need_obs[o] = true;
            d_indices.push(d);
            o_indices.push(o);
            if let Some(b) = b_opt {
                b_indices.push(b);
            }
        }
        let camera_d = keeps_camera
            .then(|| {
                protected.and_then(|cam| {
                    desired_by_id
                        .get(&observed.instances[cam].settings_id)
                        .copied()
                })
            })
            .flatten();
        if let (Some(cam_d), Some(cam_o)) = (camera_d, protected)
            && !settled.contains_key(&cam_o)
        {
            need_des[cam_d] = true;
            need_obs[cam_o] = true;
            d_indices.push(cam_d);
            o_indices.push(cam_o);
        }

        let (read_d, read_o) = rayon::join(
            || desired_rows.materialize_selected_in_place(desired, &need_des),
            || observed_rows.materialize_selected_in_place(observed, &need_obs),
        );
        read_d?;
        read_o?;
        if let Some(lb) = lazy_before.as_mut() {
            lb.materialize_indices(&b_indices)?;
        }
        for &d in &d_indices {
            let r = d..d + 1;
            stabilize_rows(desired, r.clone(), desired_ids);
            canonicalize_instances(&mut desired.instances[r])?;
        }
        for &o in &o_indices {
            let r = o..o + 1;
            stabilize_rows(observed, r.clone(), studio_ids);
            remap_rows(observed, r.clone(), &obs_remap, &service);
            canonicalize_instances(&mut observed.instances[r])?;
        }
        if let Some(pending) = &pending {
            for (d, row) in &pending.rows {
                if need_des[*d]
                    && let Some(&o) = observed_by_id.get(&desired.instances[*d].settings_id)
                    && mask_pending_fields(
                        &pending.paths,
                        &reference_ids,
                        row,
                        &desired.instances[*d],
                        &mut observed.instances[o],
                    )
                {
                    settled.insert(o, observed.instances[o].clone());
                }
            }
        }
        if let (Some(cam_d), Some(cam_o)) = (camera_d, protected)
            && !settled.contains_key(&cam_o)
            && apply_viewport_camera_override(desired, observed, cam_d, cam_o)
        {
            settled.insert(cam_o, observed.instances[cam_o].clone());
        }

        let b_doc_ref = before.or_else(|| lazy_before.as_ref().map(|lb| &lb.doc));
        let cutoff = best_mismatch.as_ref().map_or(usize::MAX, |(r, _)| *r);
        let mut raw_mismatches: Vec<(usize, usize, Option<usize>, usize, String)> = {
            let obs_ref = &*observed;
            chunk
                .par_iter()
                .copied()
                .filter(|&(rank, _, _, _)| rank < cutoff)
                .filter_map(|(rank, b_opt, d, o)| {
                    let detail = match (b_doc_ref, b_opt) {
                        (Some(b_doc), Some(b)) => {
                            changed_instance_mismatch(b_doc, b, desired, d, obs_ref, o)
                        }
                        _ => added_instance_mismatch(desired, d, obs_ref, o),
                    }?;
                    Some((rank, d, b_opt, o, detail))
                })
                .collect()
        };
        raw_mismatches.sort_unstable_by_key(|&(rank, ..)| rank);
        for (rank, d, b_opt, o, detail) in raw_mismatches {
            let name = &desired.instances[d].name;
            let final_detail = match policy.new_instance_values {
                Some(values) => {
                    let prev = b_doc_ref.zip(b_opt);
                    values.mismatch(prev, (desired, d), (observed, o))
                }
                None => Some(detail),
            };
            if let Some(final_detail) = final_detail {
                best_mismatch = Some((rank, format!("{name}.{final_detail}")));
                break;
            }
        }

        clear_non_root_fields(desired, &d_indices);
        clear_non_root_fields(observed, &o_indices);
        if let Some(lb) = lazy_before.as_mut() {
            lb.release_indices(&b_indices);
        }
    }

    if let Some((_, mismatch)) = best_mismatch {
        return Ok(Some(SettingsVerification {
            mismatch: Some(mismatch),
            accepted: None,
        }));
    }
    let accepted = settled_bytes_from_observed(
        observed,
        observed_rows,
        observed_bytes,
        studio_ids,
        &aligned_obs_ids,
        &settled,
        policy.settle,
    )?;
    Ok(Some(SettingsVerification {
        mismatch: None,
        accepted,
    }))
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn materialize_and_check_columnar_rows(
    path: &Path,
    (desired, observed): (&mut SettingsBytecode, &mut SettingsBytecode),
    (desired_rows, observed_rows): (
        &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
        &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
    ),
    (need_desired, places, origins, desired_ids, studio_ids, remap, service): (
        &[bool],
        &[usize],
        &[usize],
        &[String],
        &[String],
        &HashMap<String, String>,
        &str,
    ),
    (pending, reference_ids, keeps_camera, protected): (
        Option<&PendingFields<'_>>,
        &EditorReferenceIds<'_>,
        bool,
        Option<usize>,
    ),
    settled: &mut BTreeMap<usize, SettingsBytecodeInstance>,
    before: Option<(&SettingsBytecode, &AHashMap<&str, usize>)>,
    lazy_before: &mut Option<LazyBefore>,
    before_bytes: Option<&[u8]>,
    desired_uids: &[Option<Value>],
    (policy, by_order): (Verification<'_>, &[bool]),
) -> Result<Option<Option<String>>> {
    let count = need_desired.len();
    let all_rows: Vec<usize> = (0..count).filter(|&index| need_desired[index]).collect();
    if all_rows.is_empty() {
        return Ok(Some(None));
    }
    let use_chunks = desired_rows.block_store().is_some() && observed_rows.block_store().is_some();
    let chunk_size = if use_chunks {
        ROWS_AT_A_TIME
    } else {
        all_rows.len()
    };
    for chunk in all_rows.chunks(chunk_size) {
        let mut need_d = vec![false; count];
        let mut need_o = vec![false; count];
        for &row in chunk {
            need_d[row] = true;
            need_o[places[row]] = true;
        }
        let (read_desired, read_observed) = rayon::join(
            || desired_rows.materialize_selected_in_place(desired, &need_d),
            || observed_rows.materialize_selected_in_place(observed, &need_o),
        );
        if use_chunks {
            read_desired?;
            read_observed?;
        } else if read_desired.is_err() || read_observed.is_err() {
            return Ok(None);
        }
        for &row in chunk {
            let place = places[row]..places[row] + 1;
            let range = row..row + 1;
            stabilize_rows(desired, range.clone(), desired_ids);
            stabilize_rows(observed, place.clone(), studio_ids);
            remap_rows(observed, place.clone(), remap, service);
            canonicalize_instances(&mut desired.instances[range])?;
            canonicalize_instances(&mut observed.instances[place])?;
        }
        if let Some(pending) = pending {
            for (index, row) in &pending.rows {
                if need_d[*index]
                    && mask_pending_fields(
                        &pending.paths,
                        reference_ids,
                        row,
                        &desired.instances[*index],
                        &mut observed.instances[places[*index]],
                    )
                {
                    settled.insert(places[*index], observed.instances[places[*index]].clone());
                }
            }
        }
        if keeps_camera
            && let Some(camera) = protected
            && need_o[camera]
            && !settled.contains_key(&camera)
            && apply_viewport_camera_override(desired, observed, origins[camera], camera)
        {
            settled.insert(camera, observed.instances[camera].clone());
        }
        let result = check_materialized_rows_with_lazy_before(
            path,
            before,
            lazy_before,
            before_bytes,
            (desired, observed),
            desired_rows,
            desired_uids,
            (chunk, places, desired_ids, service),
            (protected, policy, by_order),
        )?;
        let obs_chunk: Vec<usize> = chunk.iter().map(|&r| places[r]).collect();
        clear_non_root_fields(desired, chunk);
        clear_non_root_fields(observed, &obs_chunk);
        match result {
            None => return Ok(None),
            Some(Some(mismatch)) => return Ok(Some(Some(mismatch))),
            Some(None) => {}
        }
    }
    Ok(Some(None))
}

struct PlacedRowIds {
    origins: Vec<usize>,
    desired_uids: Vec<Option<Value>>,
    placed_ids: Vec<String>,
    remap: HashMap<String, String>,
    service: String,
}

fn assign_placed_ids_and_remap(
    path: &Path,
    desired: &mut SettingsBytecode,
    observed: &mut SettingsBytecode,
    desired_ids: &[String],
    studio_ids: &[String],
    studio_roots: &[usize],
    places: &[usize],
) -> Result<PlacedRowIds> {
    let mut origins = vec![0; desired.instances.len()];
    for (index, place) in places.iter().enumerate() {
        origins[*place] = index;
    }
    let desired_uids: Vec<Option<Value>> = desired
        .instances
        .iter()
        .map(|inst| inst.properties.get("UniqueId").cloned())
        .collect();
    for document in [&mut *desired, &mut *observed] {
        for instance in &mut document.instances {
            if instance.parent_index.is_some() {
                instance.properties = Map::new();
            }
        }
    }
    let placed_ids = origins
        .iter()
        .map(|origin| desired_ids[*origin].clone())
        .collect::<Vec<_>>();
    let remap = studio_ids
        .iter()
        .cloned()
        .zip(placed_ids.iter().cloned())
        .collect::<HashMap<_, _>>();
    for (instance, id) in observed.instances.iter_mut().zip(&placed_ids) {
        instance.settings_id.clone_from(id);
    }
    let service = settings_service_name(path, desired, observed)?;
    for &root in studio_roots {
        remap_rows(observed, root..root + 1, &remap, &service);
    }
    Ok(PlacedRowIds {
        origins,
        desired_uids,
        placed_ids,
        remap,
        service,
    })
}

fn project_and_stabilize_rows(
    desired: &mut SettingsBytecode,
    desired_rows: &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
    observed: &mut SettingsBytecode,
    observed_rows: &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
) -> Option<(Vec<String>, Vec<String>, Vec<usize>)> {
    let (desired_proj, observed_proj) = rayon::join(
        || project_uid_and_camera(desired, desired_rows),
        || project_uid_and_camera(observed, observed_rows),
    );
    let ((desired_ids, _), (studio_ids, studio_roots)) = (desired_proj?, observed_proj?);
    Some((desired_ids, studio_ids, studio_roots))
}

fn retry_general_by_rows(
    path: &Path,
    before: Option<&SettingsBytecode>,
    before_bytes: Option<&[u8]>,
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    policy: Verification<'_>,
) -> Result<Option<SettingsVerification>> {
    let (desired, observed) = rayon::join(
        || decode_for_row_chunks(desired_bytes),
        || decode_for_row_chunks(observed_bytes),
    );
    let (Ok((mut desired, mut desired_rows)), Ok((mut observed, mut observed_rows))) =
        (desired, observed)
    else {
        return Ok(None);
    };
    let Some((desired_ids, studio_ids, _)) = project_and_stabilize_rows(
        &mut desired,
        &mut desired_rows,
        &mut observed,
        &mut observed_rows,
    ) else {
        return Ok(None);
    };
    verify_general_by_rows(
        path,
        before,
        before_bytes,
        (&mut desired, &mut desired_rows, &desired_ids),
        (&mut observed, observed_rows, &studio_ids),
        observed_bytes,
        policy,
    )
}

/// Verifies a large store (`>= MIN_ROWS`) a range of rows at a time without
/// decoding whole documents side by side, settling Studio's spelling on
/// verified v12 stores. Returns `None` for small stores (`< MIN_ROWS`), legacy
/// non-v12 formats, or when `decode_for_row_chunks` / projection cannot open
/// a store by rows.
pub(super) fn verified_by_rows(
    path: &Path,
    before: Option<&SettingsBytecode>,
    before_bytes: Option<&[u8]>,
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    policy: Verification<'_>,
    check_no_extra: bool,
) -> Result<Option<SettingsVerification>> {
    let (desired, observed) = rayon::join(
        || decode_for_row_chunks(desired_bytes),
        || decode_for_row_chunks(observed_bytes),
    );
    let (Ok((mut desired, mut desired_rows)), Ok((mut observed, mut observed_rows))) =
        (desired, observed)
    else {
        return Ok(None);
    };
    let count = desired.instances.len();
    if count < MIN_ROWS {
        return Ok(None);
    }
    let whole = |why: &str| {
        log_global(
            4,
            format_args!(
                "[renium] a store of {count} instances is verified from its whole documents: {why}"
            ),
        );
        Ok(None)
    };
    let Some((desired_ids, studio_ids, studio_roots)) = project_and_stabilize_rows(
        &mut desired,
        &mut desired_rows,
        &mut observed,
        &mut observed_rows,
    ) else {
        return Ok(None);
    };
    let places_and_order = if observed.instances.len() == count {
        instance_places(&desired, &observed)
    } else {
        None
    };
    let Some((places, by_order)) = places_and_order else {
        let why = if observed.instances.len() != count {
            "Studio holds another count of instances"
        } else {
            "nothing says where Studio holds each instance"
        };
        if let Some(verified) = verify_general_by_rows(
            path,
            before,
            before_bytes,
            (&mut desired, &mut desired_rows, &desired_ids),
            (&mut observed, observed_rows, &studio_ids),
            observed_bytes,
            policy,
        )? {
            return Ok(Some(verified));
        }
        return whole(why);
    };
    let PlacedRowIds {
        origins,
        desired_uids,
        placed_ids,
        remap,
        service,
    } = assign_placed_ids_and_remap(
        path,
        &mut desired,
        &mut observed,
        &desired_ids,
        &studio_ids,
        &studio_roots,
        &places,
    )?;
    let pending = pending_fields(&service, &desired, policy.pending);
    let reference_ids =
        EditorReferenceIds::new(desired_ids.iter().map(String::as_str)).in_service(&service);
    let before = before.map(|before| {
        let ids = before.instances.iter().enumerate();
        let by_id = ids
            .map(|(index, instance)| (instance.settings_id.as_str(), index))
            .collect::<AHashMap<_, _>>();
        (before, by_id)
    });
    let mut lazy_before: Option<LazyBefore> = None;
    // The viewport camera Studio holds is never compared, and a settle gives
    // it the values the files hold for it (`keep_reference_viewport_camera`).
    let protected = workspace_current_camera_index(&observed);
    let viewport_root = desired
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none() && instance.class_name == "Workspace");
    let keeps_camera = policy.settle
        && viewport_root.is_some_and(|root| {
            !root.properties.contains_key("CurrentCamera")
                || workspace_current_camera_index(&desired).map(|camera| places[camera])
                    == protected
        });
    // Whether Studio's instance for one of `rows` of the files does not hold
    // what the push asked of it.
    let differ = |desired: &SettingsBytecode, observed: &mut SettingsBytecode, rows: &[usize]| {
        // An instance its order among namesakes alone tells is Studio's
        // only where the two hold the same in full: compared as a new one
        // is, and with nothing in Studio's that the files do not state, which
        // would tell it from a namesake.
        let holds_more = |observed: &SettingsBytecode, index: usize| {
            let (wanted, held) = (
                &desired.instances[index],
                &observed.instances[places[index]],
            );
            let more = |held: &Map<String, Value>, wanted: &Map<String, Value>| {
                held.keys().any(|name| !wanted.contains_key(name))
            };
            (by_order[index] || check_no_extra)
                && (more(&held.properties, &wanted.properties)
                    || more(&held.attributes, &wanted.attributes))
        };
        let previous = |index: usize| {
            let (before, by_id) = before.as_ref()?;
            by_id
                .get(desired.instances[index].settings_id.as_str())
                .filter(|_| !by_order[index])
                .map(|previous| (*before, *previous))
        };
        let compare = |observed: &SettingsBytecode, index: usize| {
            let place = places[index];
            if protected == Some(place) {
                return None;
            }
            let instance = &desired.instances[index];
            match previous(index) {
                Some((before, previous))
                    if before.instances[previous].class_name == "PackageLink"
                        && instance.class_name == "PackageLink" =>
                {
                    None
                }
                Some((before, previous)) => {
                    changed_instance_mismatch(before, previous, desired, index, observed, place)
                }
                None => {
                    let deferred_physics = before_bytes.is_some()
                        && instance.class_name == "Terrain"
                        && instance.properties.get("PhysicsGrid")
                            != observed.instances[place].properties.get("PhysicsGrid");
                    if deferred_physics {
                        Some(String::new())
                    } else {
                        added_instance_mismatch(desired, index, observed, place)
                            .or_else(|| holds_more(observed, index).then(String::new))
                    }
                }
            }
        };
        let unlike = {
            let observed = &*observed;
            rows.par_iter()
                .copied()
                .filter(|index| compare(observed, *index).is_some())
                .collect::<Vec<_>>()
        };
        unlike
            .into_iter()
            .any(|index| match policy.new_instance_values {
                Some(_) if holds_more(observed, index) => true,
                Some(values) => values
                    .mismatch(previous(index), (desired, index), (observed, places[index]))
                    .is_some(),
                None => true,
            })
    };
    if let Some((before_doc, before_by_id)) = &before
        && crate::settings::equivalence::holds_unread_instances(before_doc)
    {
        let fallback_general =
            |desired: SettingsBytecode,
             observed: SettingsBytecode,
             desired_rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
             observed_rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
             lazy_before: Option<LazyBefore>,
             why: &str|
             -> Result<Option<SettingsVerification>> {
                let can_retry = before_bytes.is_some()
                    && desired_rows.block_store().is_some()
                    && observed_rows.block_store().is_some();
                drop((desired, observed, desired_rows, observed_rows, lazy_before));
                if can_retry
                    && let Some(verified) = retry_general_by_rows(
                        path,
                        None,
                        before_bytes,
                        desired_bytes,
                        observed_bytes,
                        policy,
                    )?
                {
                    return Ok(Some(verified));
                }
                whole(why)
            };
        if pending.is_some() {
            return fallback_general(
                desired,
                observed,
                desired_rows,
                observed_rows,
                lazy_before,
                "the push read Studio's copy by its changed instances and left fields pending",
            );
        }
        let in_place = !policy.settle || (0..count).all(|index| places[index] == index);
        if !in_place {
            return fallback_general(
                desired,
                observed,
                desired_rows,
                observed_rows,
                lazy_before,
                "the push read Studio's copy by its changed instances and settles a reordered store",
            );
        }
        let rows = (0..count)
            .filter(|&index| {
                before_by_id
                    .get(desired.instances[index].settings_id.as_str())
                    .is_none_or(|previous| {
                        !crate::settings::held::stands_unchanged(
                            &before_doc.instances[*previous].properties,
                        )
                    })
                    || (policy.settle && by_order[index])
                    || (keeps_camera && protected == Some(places[index]))
            })
            .collect::<Vec<_>>();
        if policy.settle && rows.len() > ROWS_AT_A_TIME {
            return fallback_general(
                desired,
                observed,
                desired_rows,
                observed_rows,
                lazy_before,
                "the push read Studio's copy by its changed instances with too many rows to settle",
            );
        }
        let Some(written) = materialize_unresolved_rows(
            &mut desired,
            &mut observed,
            (&mut desired_rows, &mut observed_rows),
            (&rows, &places, &origins),
            (&desired_ids, &studio_ids, &remap, &service),
            (None, &reference_ids, keeps_camera, protected),
        )?
        else {
            return Ok(None);
        };
        if differ(&desired, &mut observed, &rows) {
            if let Some(mismatch) = direct_mismatch_in_rows(
                (desired_bytes, observed_bytes),
                (before_doc, before_by_id),
                (&desired, &mut observed),
                (&rows, &places, &origins, &desired_ids),
                (protected, policy.new_instance_values),
            )? {
                return Ok(Some(SettingsVerification {
                    mismatch: Some(mismatch),
                    accepted: None,
                }));
            }
            return fallback_general(
                desired,
                observed,
                desired_rows,
                observed_rows,
                lazy_before,
                "an instance does not hold what the files hold for it",
            );
        }
        let settled: BTreeMap<usize, SettingsBytecodeInstance> = written
            .into_iter()
            .map(|place| (place, observed.instances[place].clone()))
            .collect();
        return finish_settled_rows(
            desired,
            observed,
            desired_rows,
            observed_rows,
            lazy_before,
            (observed_bytes, &studio_ids, &placed_ids, &settled),
            policy.settle,
        );
    }
    let skip = fast_verify_skip_mask(&desired.instances, protected, &origins, pending.as_ref());
    let fast_has_before = before.is_some() || before_bytes.is_some() || check_no_extra;
    if let Some(verified) = crate::settings::bytecode::blocks::fast_verify_aligned_v12_rows(
        crate::settings::bytecode::blocks::FastVerifyInput {
            desired_bytes,
            observed_bytes,
            desired_instances: &desired.instances,
            observed_instances: &observed.instances,
            places: &places,
            by_order: &by_order,
            skip: &skip,
            has_before: fast_has_before,
        },
    )? {
        let unresolved: Vec<usize> = (0..count).filter(|&index| !verified[index]).collect();
        log_global(
            5,
            format_args!(
                "[renium] fast verify aligned v12 rows ({service}): resolved={}/{count} unresolved={}",
                count - unresolved.len(),
                unresolved.len()
            ),
        );
        if unresolved.len() <= ROWS_AT_A_TIME {
            let Some(written) = materialize_unresolved_rows(
                &mut desired,
                &mut observed,
                (&mut desired_rows, &mut observed_rows),
                (&unresolved, &places, &origins),
                (&desired_ids, &studio_ids, &remap, &service),
                (pending.as_ref(), &reference_ids, keeps_camera, protected),
            )?
            else {
                return Ok(None);
            };
            if differ(&desired, &mut observed, &unresolved) {
                if let Some((before_doc, before_by_id)) = &before {
                    if !unresolved.iter().any(|&i| by_order[i])
                        && let Some(mismatch) = direct_mismatch_in_rows(
                            (desired_bytes, observed_bytes),
                            (before_doc, before_by_id),
                            (&desired, &mut observed),
                            (&unresolved, &places, &origins, &desired_ids),
                            (protected, policy.new_instance_values),
                        )?
                    {
                        return Ok(Some(SettingsVerification {
                            mismatch: Some(mismatch),
                            accepted: None,
                        }));
                    }
                    let before_doc = *before_doc;
                    drop((desired, observed, desired_rows, observed_rows, lazy_before));
                    if let Some(verified) = retry_general_by_rows(
                        path,
                        Some(before_doc),
                        before_bytes,
                        desired_bytes,
                        observed_bytes,
                        policy,
                    )? {
                        return Ok(Some(verified));
                    }
                    return whole("an instance does not hold what the files hold for it");
                }
                match check_materialized_rows_with_lazy_before(
                    path,
                    None,
                    &mut lazy_before,
                    before_bytes,
                    (&desired, &mut observed),
                    &mut desired_rows,
                    &desired_uids,
                    (&unresolved, &places, &desired_ids, &service),
                    (protected, policy, &by_order),
                )? {
                    Some(Some(mismatch)) => {
                        return Ok(Some(SettingsVerification {
                            mismatch: Some(mismatch),
                            accepted: None,
                        }));
                    }
                    Some(None) => {}
                    None => {
                        drop((desired, observed, desired_rows, observed_rows, lazy_before));
                        if let Some(verified) = retry_general_by_rows(
                            path,
                            None,
                            before_bytes,
                            desired_bytes,
                            observed_bytes,
                            policy,
                        )? {
                            return Ok(Some(verified));
                        }
                        return whole("an instance does not hold what the files hold for it");
                    }
                }
            }
            let settled: BTreeMap<usize, SettingsBytecodeInstance> = written
                .into_iter()
                .map(|place| (place, observed.instances[place].clone()))
                .collect();
            #[cfg(test)]
            if before.is_none() {
                UNPREPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return finish_settled_rows(
                desired,
                observed,
                desired_rows,
                observed_rows,
                lazy_before,
                (observed_bytes, &studio_ids, &placed_ids, &settled),
                policy.settle,
            );
        }
    }
    // The instances a settle writes otherwise than Studio's capture holds
    // them.
    let mut settled = BTreeMap::new();
    let pending_indices = pending
        .as_ref()
        .map(|p| p.rows.iter().map(|(index, _)| *index).collect::<Vec<_>>())
        .unwrap_or_default();
    if let (Some(d_store), Some(o_store)) =
        (desired_rows.block_store(), observed_rows.block_store())
    {
        let need_desired =
            match crate::settings::bytecode::blocks::bulk_diff::matching_rows_by_column_hash_stores(
                (d_store, o_store),
                (&desired.instances, &observed.instances),
                (&places, &origins, &by_order),
                protected,
                &pending_indices,
            )? {
                Some(matched) => {
                    let need = matched.iter().map(|&ok| !ok).collect::<Vec<_>>();
                    let needed_count = need.iter().filter(|&&n| n).count();
                    log_global(
                        5,
                        format_args!(
                            "[renium] columnar row verification ({service}): {needed_count} of {count} rows materialized"
                        ),
                    );
                    need
                }
                None => vec![true; count],
            };
        let before_ref = before.as_ref().map(|(doc, by_id)| (*doc, by_id));
        match materialize_and_check_columnar_rows(
            path,
            (&mut desired, &mut observed),
            (&mut desired_rows, &mut observed_rows),
            (
                &need_desired,
                &places,
                &origins,
                &desired_ids,
                &studio_ids,
                &remap,
                &service,
            ),
            (pending.as_ref(), &reference_ids, keeps_camera, protected),
            &mut settled,
            before_ref,
            &mut lazy_before,
            before_bytes,
            &desired_uids,
            (policy, &by_order),
        )? {
            None => {
                let before_doc = before.map(|(doc, _)| doc);
                drop((desired, observed, desired_rows, observed_rows, lazy_before));
                return retry_general_by_rows(
                    path,
                    before_doc,
                    before_bytes,
                    desired_bytes,
                    observed_bytes,
                    policy,
                );
            }
            Some(Some(mismatch)) => {
                return Ok(Some(SettingsVerification {
                    mismatch: Some(mismatch),
                    accepted: None,
                }));
            }
            Some(None) => {
                #[cfg(test)]
                if before.is_none() {
                    UNPREPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                return finish_settled_rows(
                    desired,
                    observed,
                    desired_rows,
                    observed_rows,
                    lazy_before,
                    (observed_bytes, &studio_ids, &placed_ids, &settled),
                    policy.settle,
                );
            }
        }
    }
    // The instances of the files that wait for their counterpart's range.
    let mut waiting = 0;
    let mut mismatch_state: Option<ChunkMismatchState> = None;
    let mut start = 0;
    while start < count {
        let rows = start..(start + ROWS_AT_A_TIME).min(count);
        let (read_desired, read_observed) = rayon::join(
            || desired_rows.materialize(&mut desired, rows.clone()),
            || observed_rows.materialize(&mut observed, rows.clone()),
        );
        if read_desired.is_err() || read_observed.is_err() {
            return Ok(None);
        }
        let mut written = Vec::new();
        let (placed_desired, placed_observed) = rayon::join(
            || {
                stabilize_rows(&mut desired, rows.clone(), &desired_ids);
                desired.instances[rows.clone()]
                    .par_chunks_mut(512)
                    .try_for_each(canonicalize_instances)
            },
            || {
                stabilize_rows(&mut observed, rows.clone(), &studio_ids);
                remap_rows(&mut observed, rows.clone(), &remap, &service);
                observed.instances[rows.clone()]
                    .par_chunks_mut(512)
                    .try_for_each(canonicalize_instances)
            },
        );
        placed_desired?;
        placed_observed?;
        // The instances both copies hold read by now, whichever of the two
        // this range read last.
        let read = |index: usize| rows.contains(&index.max(places[index]));
        let both = rows
            .clone()
            .filter(|index| places[*index] < rows.end)
            .chain(
                rows.clone()
                    .map(|place| origins[place])
                    .filter(|index| *index < rows.start),
            )
            .collect::<Vec<_>>();
        waiting = waiting + rows.len() - both.len();
        if waiting > ROWS_AT_A_TIME {
            return whole("Studio holds the instances in another order");
        }
        if let Some(pending) = &pending {
            for (index, row) in pending.rows.iter().filter(|(index, _)| read(*index)) {
                if mask_pending_fields(
                    &pending.paths,
                    &reference_ids,
                    row,
                    &desired.instances[*index],
                    &mut observed.instances[places[*index]],
                ) {
                    written.push(places[*index]);
                }
            }
        }
        if keeps_camera
            && let Some(camera) = protected.filter(|camera| read(origins[*camera]))
            && apply_viewport_camera_override(&desired, &mut observed, origins[camera], camera)
        {
            written.push(camera);
        }
        if mismatch_state.is_some() || differ(&desired, &mut observed, &both) {
            if mismatch_state.is_none() {
                let Some((before_doc, before_by_id)) = &before else {
                    return whole("an instance does not hold what the files hold for it");
                };
                let Some(state) = ChunkMismatchState::try_new(
                    (desired_bytes, observed_bytes),
                    (before_doc, before_by_id),
                    (&places, &origins, &desired_ids),
                )?
                else {
                    return whole("an instance does not hold what the files hold for it");
                };
                mismatch_state = Some(state);
            }
            let (before_doc, before_by_id) = before.as_ref().expect("checked above");
            let state = mismatch_state.as_mut().expect("initialized above");
            state.record_chunk(
                (before_doc, before_by_id),
                (&desired, &mut observed),
                (&both, &places),
                (protected, policy.new_instance_values),
            );
            if let Some(mismatch) = state.finished_mismatch(rows.end, &places) {
                return Ok(Some(SettingsVerification {
                    mismatch: Some(mismatch),
                    accepted: None,
                }));
            }
        } else {
            for place in written {
                settled.insert(place, observed.instances[place].clone());
            }
        }
        // The roots stay: they say which camera is the viewport's. The
        // values of the others are freed beside the next range's read.
        let mut released = Vec::with_capacity(both.len() * 2);
        for &index in &both {
            for instance in [
                &mut desired.instances[index],
                &mut observed.instances[places[index]],
            ] {
                if instance.parent_index.is_some() {
                    released.push((
                        std::mem::take(&mut instance.properties),
                        std::mem::take(&mut instance.attributes),
                    ));
                }
            }
        }
        rayon::spawn(move || released.into_par_iter().for_each(drop));
        start = rows.end;
    }
    if let Some(state) = mismatch_state {
        if let Some((_, mismatch)) = state.best {
            return Ok(Some(SettingsVerification {
                mismatch: Some(mismatch),
                accepted: None,
            }));
        }
        return whole("an instance does not hold what the files hold for it");
    }
    if desired_rows.finish().is_err() || observed_rows.finish().is_err() {
        return Ok(None);
    }
    drop((desired, observed));
    #[cfg(test)]
    if before.is_none() {
        UNPREPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let accepted = policy
        .settle
        .then(|| settled_store(observed_bytes, &placed_ids, &settled))
        .transpose()?;
    Ok(Some(SettingsVerification {
        mismatch: None,
        accepted,
    }))
}

/// The stores verified without a copy from before the push.
#[cfg(test)]
pub(crate) static UNPREPARED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The store a settle publishes: Studio's copy as it was captured, each
/// instance under the files' id for its place, and the instances of
/// `written` as the verification left them (the files' value of a field the
/// push left pending, the viewport camera). A reference names its instance
/// by its place, whatever id that instance has. Every other value stays as
/// captured, in Studio's spelling, so that
/// the next capture of an instance nobody changed digests as its stored row.
fn settle_opened_store(
    observed_rows: crate::settings::bytecode::property_edit::RetainedRowColumns,
    captured: &[u8],
    records: Vec<SettingsBytecodeInstance>,
    ids: &[String],
    written: &BTreeMap<usize, SettingsBytecodeInstance>,
) -> Result<Vec<u8>> {
    if let Some(store) = observed_rows.into_block_store() {
        return store.settle_ids_and_rows(records, ids, written);
    }
    settled_store(captured, ids, written)
}

fn settled_store(
    captured: &[u8],
    ids: &[String],
    written: &BTreeMap<usize, SettingsBytecodeInstance>,
) -> Result<Vec<u8>> {
    let (mut document, mut retained) = decode_for_property_edit(captured)?;
    anyhow::ensure!(
        document.instances.len() == ids.len(),
        "A captured store changed while it was verified; retry the sync"
    );
    retained.materialize(&mut document, &written.keys().copied().collect::<Vec<_>>())?;
    for (instance, id) in document.instances.iter_mut().zip(ids) {
        instance.settings_id.clone_from(id);
    }
    for (index, verified) in written {
        let instance = &mut document.instances[*index];
        instance.properties.clone_from(&verified.properties);
        instance.attributes.clone_from(&verified.attributes);
    }
    let store = retained.encode(&document);
    rayon::spawn(move || drop((document, retained)));
    store
}

fn fast_verify_skip_mask(
    instances: &[SettingsBytecodeInstance],
    protected: Option<usize>,
    origins: &[usize],
    pending: Option<&PendingFields<'_>>,
) -> Vec<bool> {
    let mut skip = vec![false; instances.len()];
    for (index, instance) in instances.iter().enumerate() {
        if instance.parent_index.is_none() {
            skip[index] = true;
        }
    }
    if let Some(camera) = protected {
        skip[origins[camera]] = true;
    }
    if let Some(pending) = pending {
        for &(index, _) in &pending.rows {
            skip[index] = true;
        }
    }
    skip
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn materialize_unresolved_rows(
    desired: &mut SettingsBytecode,
    observed: &mut SettingsBytecode,
    (desired_rows, observed_rows): (
        &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
        &mut crate::settings::bytecode::property_edit::RetainedRowColumns,
    ),
    (unresolved, places, origins): (&[usize], &[usize], &[usize]),
    (desired_ids, studio_ids, remap, service): (
        &[String],
        &[String],
        &HashMap<String, String>,
        &str,
    ),
    (pending, reference_ids, keeps_camera, protected): (
        Option<&PendingFields<'_>>,
        &EditorReferenceIds<'_>,
        bool,
        Option<usize>,
    ),
) -> Result<Option<Vec<usize>>> {
    let count = desired.instances.len();
    let mut need_des = vec![false; count];
    let mut need_obs = vec![false; count];
    for &index in unresolved {
        need_des[index] = true;
        need_obs[places[index]] = true;
    }
    let (read_desired, read_observed) = rayon::join(
        || desired_rows.materialize_selected_in_place(desired, &need_des),
        || observed_rows.materialize_selected_in_place(observed, &need_obs),
    );
    if read_desired.is_err() || read_observed.is_err() {
        return Ok(None);
    }
    for &index in unresolved {
        let des_row = index..index + 1;
        let obs_row = places[index]..places[index] + 1;
        stabilize_rows(desired, des_row.clone(), desired_ids);
        stabilize_rows(observed, obs_row.clone(), studio_ids);
        remap_rows(observed, obs_row.clone(), remap, service);
        canonicalize_instances(&mut desired.instances[des_row])?;
        canonicalize_instances(&mut observed.instances[obs_row])?;
    }
    let mut written = Vec::new();
    if let Some(pending) = pending {
        for (index, row) in &pending.rows {
            if mask_pending_fields(
                &pending.paths,
                reference_ids,
                row,
                &desired.instances[*index],
                &mut observed.instances[places[*index]],
            ) {
                written.push(places[*index]);
            }
        }
    }
    if keeps_camera
        && let Some(camera) = protected
        && apply_viewport_camera_override(desired, observed, origins[camera], camera)
    {
        written.push(camera);
    }
    Ok(Some(written))
}

struct ChunkMismatchState {
    ranks: Vec<usize>,
    suffix_min_rank: Vec<usize>,
    best: Option<(usize, String)>,
}

impl ChunkMismatchState {
    fn try_new(
        (desired_bytes, observed_bytes): (&[u8], &[u8]),
        (before, before_by_id): (&SettingsBytecode, &AHashMap<&str, usize>),
        (places, origins, desired_ids): (&[usize], &[usize], &[String]),
    ) -> Result<Option<Self>> {
        if !can_report_direct_mismatch(
            (desired_bytes, observed_bytes),
            (before, before_by_id),
            (places, origins, desired_ids),
        )? {
            return Ok(None);
        }
        let ranks = row_settings_ranks(before, before_by_id, desired_ids);
        let mut suffix_min_rank = vec![usize::MAX; ranks.len() + 1];
        for index in (0..ranks.len()).rev() {
            suffix_min_rank[index] = ranks[index].min(suffix_min_rank[index + 1]);
        }
        Ok(Some(Self {
            ranks,
            suffix_min_rank,
            best: None,
        }))
    }

    fn record_chunk(
        &mut self,
        (before, before_by_id): (&SettingsBytecode, &AHashMap<&str, usize>),
        (desired, observed): (&SettingsBytecode, &mut SettingsBytecode),
        (rows, places): (&[usize], &[usize]),
        (protected, new_instance_values): (Option<usize>, Option<&NewInstanceValues<'_>>),
    ) {
        let cutoff = self.best.as_ref().map_or(usize::MAX, |(rank, _)| *rank);
        if let Some((rank, mismatch)) = best_mismatch_in_rows(
            (before, before_by_id),
            (desired, observed),
            (rows, places, &self.ranks),
            (protected, new_instance_values, cutoff),
        ) {
            self.best = Some((rank, mismatch));
        }
    }

    fn finished_mismatch(&mut self, rows_end: usize, places: &[usize]) -> Option<String> {
        let best_rank = self.best.as_ref()?.0;
        if best_rank >= self.suffix_min_rank[rows_end] {
            return None;
        }
        let waiting_has_earlier =
            (0..rows_end).any(|index| places[index] >= rows_end && self.ranks[index] <= best_rank);
        if waiting_has_earlier {
            return None;
        }
        self.best.take().map(|(_, mismatch)| mismatch)
    }
}

fn direct_mismatch_in_rows(
    (desired_bytes, observed_bytes): (&[u8], &[u8]),
    (before, before_by_id): (&SettingsBytecode, &AHashMap<&str, usize>),
    (desired, observed): (&SettingsBytecode, &mut SettingsBytecode),
    (rows, places, origins, desired_ids): (&[usize], &[usize], &[usize], &[String]),
    (protected, new_instance_values): (Option<usize>, Option<&NewInstanceValues<'_>>),
) -> Result<Option<String>> {
    if !can_report_direct_mismatch(
        (desired_bytes, observed_bytes),
        (before, before_by_id),
        (places, origins, desired_ids),
    )? {
        return Ok(None);
    }
    let ranks = row_settings_ranks(before, before_by_id, desired_ids);
    Ok(best_mismatch_in_rows(
        (before, before_by_id),
        (desired, observed),
        (rows, places, &ranks),
        (protected, new_instance_values, usize::MAX),
    )
    .map(|(_, mismatch)| mismatch))
}

fn can_report_direct_mismatch(
    (desired_bytes, observed_bytes): (&[u8], &[u8]),
    (before, before_by_id): (&SettingsBytecode, &AHashMap<&str, usize>),
    (places, origins, desired_ids): (&[usize], &[usize], &[String]),
) -> Result<bool> {
    let count = desired_ids.len();
    if crate::settings::equivalence::holds_unread_instances(before)
        && !places
            .iter()
            .enumerate()
            .all(|(index, place)| *place == index)
    {
        return Ok(false);
    }
    if before_by_id.len() != before.instances.len() {
        return Ok(false);
    }
    let unique_desired = desired_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>()
        .len()
        == count;
    if !unique_desired {
        return Ok(false);
    }
    alignment_told_alone(desired_bytes, observed_bytes, origins, desired_ids)
}

fn alignment_told_alone(
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    origins: &[usize],
    desired_ids: &[String],
) -> Result<bool> {
    let (desired, observed) = rayon::join(
        || decode_for_row_chunks(desired_bytes),
        || decode_for_row_chunks(observed_bytes),
    );
    let (Ok((mut desired, desired_rows)), Ok((mut observed, observed_rows))) = (desired, observed)
    else {
        return Ok(false);
    };
    let count = desired.instances.len();
    if observed.instances.len() != count || origins.len() != count || desired_ids.len() != count {
        return Ok(false);
    }
    if project_uid_and_camera(&mut desired, &desired_rows).is_none()
        || project_uid_and_camera(&mut observed, &observed_rows).is_none()
    {
        return Ok(false);
    }
    let Some((ids, matched_ref)) =
        crate::settings::equivalence::settings_ids_told_alone(&desired, &observed)
    else {
        return Ok(false);
    };
    Ok(ids.len() == count
        && matched_ref.len() == count
        && matched_ref.iter().enumerate().all(|(place, origin)| {
            *origin == Some(origins[place]) && ids[place] == desired_ids[origins[place]]
        }))
}

fn row_settings_ranks(
    before: &SettingsBytecode,
    before_by_id: &AHashMap<&str, usize>,
    desired_ids: &[String],
) -> Vec<usize> {
    let before_len = before.instances.len();
    desired_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            before_by_id
                .get(id.as_str())
                .copied()
                .unwrap_or(before_len + index)
        })
        .collect()
}

fn best_mismatch_in_rows(
    (before, before_by_id): (&SettingsBytecode, &AHashMap<&str, usize>),
    (desired, observed): (&SettingsBytecode, &mut SettingsBytecode),
    (rows, places, ranks): (&[usize], &[usize], &[usize]),
    (protected, new_instance_values, cutoff): (
        Option<usize>,
        Option<&NewInstanceValues<'_>>,
        usize,
    ),
) -> Option<(usize, String)> {
    let raw_mismatch = |observed: &SettingsBytecode, index: usize| -> Option<String> {
        let place = places[index];
        if protected == Some(place) {
            return None;
        }
        let instance = &desired.instances[index];
        match before_by_id.get(instance.settings_id.as_str()).copied() {
            Some(previous)
                if before.instances[previous].class_name == "PackageLink"
                    && instance.class_name == "PackageLink" =>
            {
                None
            }
            Some(previous) => {
                changed_instance_mismatch(before, previous, desired, index, observed, place)
            }
            None => added_instance_mismatch(desired, index, observed, place),
        }
    };
    let mut candidates = {
        let observed = &*observed;
        rows.par_iter()
            .copied()
            .filter(|&index| ranks[index] < cutoff)
            .filter_map(|index| {
                raw_mismatch(observed, index).map(|detail| (ranks[index], index, detail))
            })
            .collect::<Vec<_>>()
    };
    candidates.sort_unstable_by_key(|(rank, _, _)| *rank);
    for (rank, index, detail) in candidates {
        let name = &desired.instances[index].name;
        let final_detail = match new_instance_values {
            Some(values) => {
                let previous = before_by_id
                    .get(desired.instances[index].settings_id.as_str())
                    .copied()
                    .map(|previous| (before, previous));
                match values.mismatch(previous, (desired, index), (observed, places[index])) {
                    Some(detail) => detail,
                    None => continue,
                }
            }
            None => detail,
        };
        return Some((rank, format!("{name}.{final_detail}")));
    }
    None
}
