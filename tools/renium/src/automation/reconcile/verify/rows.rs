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
/// `None` where the whole documents have to say: the two copies hold other
/// instances, the store is small, or an instance does not hold what the
/// files hold for it. The whole documents then name what differs, or match
/// Studio's instances with the files' another way.
pub(super) fn verified_by_rows(
    path: &Path,
    before: &SettingsBytecode,
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
    if count < MIN_ROWS || observed.instances.len() != count {
        return Ok(None);
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
        return Ok(None);
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
    let before_by_id = before
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
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
        // only where the two hold the same in full, as a new one is compared.
        let previous = |index: usize| {
            before_by_id
                .get(desired.instances[index].settings_id.as_str())
                .copied()
                .filter(|_| !by_order[index])
        };
        let compare = |observed: &SettingsBytecode, index: usize| {
            let place = places[index];
            if protected == Some(place) {
                return None;
            }
            let instance = &desired.instances[index];
            match previous(index) {
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
                Some(values) => values
                    .mismatch(
                        previous(index).map(|previous| (before, previous)),
                        (desired, index),
                        (observed, places[index]),
                    )
                    .is_some(),
                None => true,
            })
    };
    if crate::settings::equivalence::holds_unread_instances(before) {
        if policy.settle || pending.is_some() {
            return Ok(None);
        }
        let changed = desired
            .instances
            .iter()
            .map(|instance| {
                before_by_id
                    .get(instance.settings_id.as_str())
                    .is_none_or(|previous| {
                        !crate::settings::held::stands_unchanged(
                            &before.instances[*previous].properties,
                        )
                    })
            })
            .collect::<Vec<_>>();
        let changed_of_studio = origins
            .iter()
            .map(|origin| changed[*origin])
            .collect::<Vec<_>>();
        let (read_desired, read_observed) = rayon::join(
            || desired_rows.materialize_selected(&mut desired, &changed),
            || observed_rows.materialize_selected(&mut observed, &changed_of_studio),
        );
        if read_desired.is_err() || read_observed.is_err() {
            return Ok(None);
        }
        let rows = (0..count)
            .filter(|index| changed[*index])
            .collect::<Vec<_>>();
        for &row in &rows {
            let place = places[row]..places[row] + 1;
            let row = row..row + 1;
            stabilize_rows(&mut desired, row.clone(), &desired_ids);
            stabilize_rows(&mut observed, place.clone(), &studio_ids);
            remap_rows(&mut observed, place.clone(), &remap, &service);
            canonicalize_instances(&mut desired.instances[row])?;
            canonicalize_instances(&mut observed.instances[place])?;
        }
        return Ok((!differ(&desired, &mut observed, &rows)).then(SettingsVerification::default));
    }
    // The instances a settle writes otherwise than Studio's capture holds
    // them.
    let mut settled = BTreeMap::new();
    // The instances of the files that wait for their counterpart's range.
    let mut waiting = 0;
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
            return Ok(None);
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
        if differ(&desired, &mut observed, &both) {
            return Ok(None);
        }
        for place in written {
            settled.insert(place, observed.instances[place].clone());
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
    if desired_rows.finish().is_err() || observed_rows.finish().is_err() {
        return Ok(None);
    }
    drop((desired, observed));
    let accepted = policy
        .settle
        .then(|| settled_store(observed_bytes, &placed_ids, &settled))
        .transpose()?;
    Ok(Some(SettingsVerification {
        mismatch: None,
        accepted,
    }))
}

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
