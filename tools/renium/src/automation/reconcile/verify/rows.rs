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
const MIN_ROWS: usize = if cfg!(test) { 2 } else { 16_384 };

/// Verifies a store whose push left every instance where the files hold it,
/// or among its namesakes where they hold it (`instance_places`): Studio's
/// instances take the files' ids by their places, and each is compared with
/// the files' instance of its place as the whole documents compare the two.
/// With `policy.settle` the store to publish is Studio's copy as it was
/// captured, under the files' ids (`settled_store`).
///
/// Where Studio holds the instances in another order, an instance read
/// with one range waits for the range that reads its counterpart; a store
/// whose two orders keep more than a range waiting is the whole documents'.
///
/// A push that read Studio's copy by the instances it changed (`before`
/// holds every other one bare) asked nothing of the others: those instances
/// alone are read of either copy, in one pass.
///
/// Without `before`, for a store the push asked nothing of, every instance
/// is compared in full as a new one is: Studio holding what the files hold
/// is more than such a store's whole documents ask.
///
/// `None` where the whole documents have to say: the two copies hold other
/// instances, the store is small, or an instance does not hold what the
/// files hold for it. The whole documents then name what differs, or match
/// Studio's instances with the files' another way.
pub(super) fn verified_by_rows(
    path: &Path,
    before: Option<&SettingsBytecode>,
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    policy: Verification<'_>,
) -> Result<Option<SettingsVerification>> {
    let (desired, observed) = rayon::join(
        || decode_for_row_chunks(desired_bytes),
        || decode_for_row_chunks(observed_bytes),
    );
    // A store this reader refuses is the whole decode's to refuse.
    let (Ok((mut desired, mut desired_rows)), Ok((mut observed, mut observed_rows))) =
        (desired, observed)
    else {
        return Ok(None);
    };
    let count = desired.instances.len();
    if count < MIN_ROWS {
        return Ok(None);
    }
    // Why a large store is left to its whole documents, for the log.
    let whole = |why: &str| {
        log_global(
            4,
            format_args!(
                "[renium] a store of {count} instances is verified from its whole documents: {why}"
            ),
        );
        Ok(None)
    };
    if observed.instances.len() != count {
        return whole("Studio holds another count of instances");
    }
    // What says where the instances stand, before any of them is read: the
    // UniqueId of each and the camera its root names.
    let stands = |name: &str| name == "UniqueId" || name == "CurrentCamera";
    let of_place = |instance: &SettingsBytecodeInstance, name: &str, value: Value| {
        (name == "UniqueId" || instance.parent_index.is_none()).then_some(value)
    };
    if desired_rows
        .project_properties(&mut desired, stands, of_place)
        .is_err()
        || observed_rows
            .project_properties(&mut observed, stands, of_place)
            .is_err()
    {
        return Ok(None);
    }
    let ids_of = |document: &SettingsBytecode| {
        document
            .instances
            .iter()
            .map(|instance| instance.settings_id.clone())
            .collect::<Vec<_>>()
    };
    let (desired_ids, studio_ids) = (ids_of(&desired), ids_of(&observed));
    let roots_of = |document: &SettingsBytecode| {
        (0..count)
            .filter(|index| document.instances[*index].parent_index.is_none())
            .collect::<Vec<_>>()
    };
    let studio_roots = roots_of(&observed);
    for root in roots_of(&desired) {
        stabilize_rows(&mut desired, root..root + 1, &desired_ids);
    }
    for &root in &studio_roots {
        stabilize_rows(&mut observed, root..root + 1, &studio_ids);
    }
    // Where Studio holds each instance of the files, and the files each of
    // Studio's.
    let Some((places, by_order)) = instance_places(&desired, &observed) else {
        return whole("nothing says where Studio holds each instance");
    };
    let mut origins = vec![0; count];
    for (index, place) in places.iter().enumerate() {
        origins[*place] = index;
    }
    // The UniqueIds have said what they say of the places.
    for document in [&mut desired, &mut observed] {
        for instance in &mut document.instances {
            if instance.parent_index.is_some() {
                instance.properties = Map::new();
            }
        }
    }
    // Studio's instances take the files' ids by their places.
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
    let service = settings_service_name(path, &desired, &observed)?;
    for &root in &studio_roots {
        remap_rows(&mut observed, root..root + 1, &remap, &service);
    }
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
            by_order[index]
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
                None => added_instance_mismatch(desired, index, observed, place)
                    .or_else(|| holds_more(observed, index).then(String::new)),
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
    if let Some((before, before_by_id)) = &before
        && crate::settings::equivalence::holds_unread_instances(before)
    {
        if pending.is_some() {
            return whole(
                "the push read Studio's copy by its changed instances and left fields pending",
            );
        }
        let in_place = !policy.settle || (0..count).all(|index| places[index] == index);
        if !in_place {
            return whole(
                "the push read Studio's copy by its changed instances and settles a reordered store",
            );
        }
        let rows = (0..count)
            .filter(|&index| {
                before_by_id
                    .get(desired.instances[index].settings_id.as_str())
                    .is_none_or(|previous| {
                        !crate::settings::held::stands_unchanged(
                            &before.instances[*previous].properties,
                        )
                    })
                    || (policy.settle && by_order[index])
                    || (keeps_camera && protected == Some(places[index]))
            })
            .collect::<Vec<_>>();
        if policy.settle && rows.len() > ROWS_AT_A_TIME {
            return whole(
                "the push read Studio's copy by its changed instances with too many rows to settle",
            );
        }
        let Some(written) = materialize_unresolved_rows(
            &mut desired,
            &mut observed,
            (desired_rows, observed_rows),
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
                (before, before_by_id),
                (&desired, &mut observed),
                (&rows, &places, &origins, &desired_ids),
                (protected, policy.new_instance_values),
            )? {
                return Ok(Some(SettingsVerification {
                    mismatch: Some(mismatch),
                    accepted: None,
                }));
            }
            return Ok(None);
        }
        let settled: BTreeMap<usize, SettingsBytecodeInstance> = written
            .into_iter()
            .map(|place| (place, observed.instances[place].clone()))
            .collect();
        drop((desired, observed));
        let accepted = policy
            .settle
            .then(|| settled_store(observed_bytes, &placed_ids, &settled))
            .transpose()?;
        return Ok(Some(SettingsVerification {
            mismatch: None,
            accepted,
        }));
    }
    let skip = fast_verify_skip_mask(&desired.instances, protected, &origins, pending.as_ref());
    if let Some(verified) = crate::settings::bytecode::blocks::fast_verify_aligned_v12_rows(
        crate::settings::bytecode::blocks::FastVerifyInput {
            desired_bytes,
            observed_bytes,
            desired_instances: &desired.instances,
            observed_instances: &observed.instances,
            places: &places,
            by_order: &by_order,
            skip: &skip,
            has_before: before.is_some(),
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
                (desired_rows, observed_rows),
                (&unresolved, &places, &origins),
                (&desired_ids, &studio_ids, &remap, &service),
                (pending.as_ref(), &reference_ids, keeps_camera, protected),
            )?
            else {
                return Ok(None);
            };
            if differ(&desired, &mut observed, &unresolved) {
                if let Some((before_doc, before_by_id)) = &before
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
                return whole("an instance does not hold what the files hold for it");
            }
            let settled: BTreeMap<usize, SettingsBytecodeInstance> = written
                .into_iter()
                .map(|place| (place, observed.instances[place].clone()))
                .collect();
            drop((desired, observed));
            #[cfg(test)]
            if before.is_none() {
                UNPREPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let accepted = policy
                .settle
                .then(|| settled_store(observed_bytes, &placed_ids, &settled))
                .transpose()?;
            return Ok(Some(SettingsVerification {
                mismatch: None,
                accepted,
            }));
        }
    }
    // The instances a settle writes otherwise than Studio's capture holds
    // them.
    let mut settled = BTreeMap::new();
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
        if keeps_camera && let Some(camera) = protected.filter(|camera| read(origins[*camera])) {
            let saved = &desired.instances[origins[camera]];
            let held = &mut observed.instances[camera];
            if saved.class_name == "Camera"
                && (held.properties != saved.properties || held.attributes != saved.attributes)
            {
                held.properties.clone_from(&saved.properties);
                held.attributes.clone_from(&saved.attributes);
                written.push(camera);
            }
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

fn materialize_unresolved_rows(
    desired: &mut SettingsBytecode,
    observed: &mut SettingsBytecode,
    (desired_rows, observed_rows): (
        crate::settings::bytecode::property_edit::RetainedRowColumns,
        crate::settings::bytecode::property_edit::RetainedRowColumns,
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
        || desired_rows.materialize_selected(desired, &need_des),
        || observed_rows.materialize_selected(observed, &need_obs),
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
    if keeps_camera && let Some(camera) = protected {
        let saved = &desired.instances[origins[camera]];
        let held = &mut observed.instances[camera];
        if saved.class_name == "Camera"
            && (held.properties != saved.properties || held.attributes != saved.attributes)
        {
            held.properties.clone_from(&saved.properties);
            held.attributes.clone_from(&saved.attributes);
            written.push(camera);
        }
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
    let stands = |name: &str| name == "UniqueId" || name == "CurrentCamera";
    let of_place = |instance: &SettingsBytecodeInstance, name: &str, value: Value| {
        (name == "UniqueId" || instance.parent_index.is_none()).then_some(value)
    };
    if desired_rows
        .project_properties(&mut desired, stands, of_place)
        .is_err()
        || observed_rows
            .project_properties(&mut observed, stands, of_place)
            .is_err()
    {
        return Ok(false);
    }
    let studio_ids = observed
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    let desired_roots = (0..count)
        .filter(|index| desired.instances[*index].parent_index.is_none())
        .collect::<Vec<_>>();
    let studio_roots = (0..count)
        .filter(|index| observed.instances[*index].parent_index.is_none())
        .collect::<Vec<_>>();
    for root in desired_roots {
        stabilize_rows(&mut desired, root..root + 1, desired_ids);
    }
    for root in studio_roots {
        stabilize_rows(&mut observed, root..root + 1, &studio_ids);
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
