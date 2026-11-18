use super::*;
use crate::editor::types::PendingImportFields;

#[derive(Clone, Copy, Default)]
struct Verification<'a> {
    settle: bool,
    pending: &'a [PendingImportFields],
}

pub(crate) fn verify_committed_push_readback(
    before: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    observed: &mut ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedPushVerification>,
    pending: &[PendingImportFields],
    settle: bool,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    if pending.is_empty() {
        return if settle {
            verify_and_settle_native_import(before, desired, observed, paths, prepared)
        } else {
            snapshot_intended_delta_mismatches(before, desired, observed, paths, prepared)
        };
    }
    let result = verify_snapshot_delta(
        before,
        desired,
        observed,
        paths,
        prepared,
        Verification {
            settle: false,
            pending,
        },
    )?;
    Ok((result.mismatches, result.detail))
}

#[cfg(test)]
pub(crate) fn snapshots_equivalent(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
) -> Result<bool> {
    Ok(snapshot_differences(left, right)?.is_empty())
}

pub(crate) fn snapshot_path_differences(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut differences = Vec::new();
    for path in paths {
        if !snapshot_entry_equivalent(path, left.entries.get(path), right.entries.get(path), None)?
        {
            differences.push(path.clone());
        }
    }
    differences.sort();
    Ok(differences)
}

/// Full reconciliation comparison, with only this commit's reported pending
/// fields masked in a temporary document. Extra instances and unrelated fields
/// still use the ordinary whole-store comparison. Nothing masked is published.
pub(crate) fn reconciled_path_differences(
    observed: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    pending: &[PendingImportFields],
) -> Result<Vec<PathBuf>> {
    if pending.is_empty() {
        return snapshot_path_differences(observed, desired, paths);
    }
    let mut differences = Vec::new();
    for path in paths {
        let actual = observed.entries.get(path);
        let expected = desired.entries.get(path);
        let comparison = if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
            && actual.is_some()
            && expected.is_some()
        {
            let expected = settings_document(expected)?;
            let mut actual = settings_document(actual)?;
            if align_settings_ids_to_reference(&expected, &mut actual) {
                mask_reported_pending_fields(path, &expected, &mut actual, pending)?;
                Some(SnapshotEntry::File(encode_settings_bytecode(&actual)?))
            } else {
                None
            }
        } else {
            None
        };
        if !snapshot_entry_equivalent(path, comparison.as_ref().or(actual), expected, None)? {
            differences.push(path.clone());
        }
    }
    differences.sort();
    Ok(differences)
}

pub(crate) fn snapshot_intended_delta_mismatches(
    before: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedPushVerification>,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    let result = verify_snapshot_delta(
        before,
        desired,
        observed,
        paths,
        prepared,
        Verification::default(),
    )?;
    Ok((result.mismatches, result.detail))
}

/// Keep the exact documents that verification aligned. Native import receipts
/// supply store identities, but do not contain regenerated saved values; those
/// still come from Studio's export. Publication must use this checked capture.
pub(crate) fn verify_and_settle_native_import(
    before: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    observed: &mut ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedPushVerification>,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    let result = verify_snapshot_delta(
        before,
        desired,
        observed,
        paths,
        prepared,
        Verification {
            settle: true,
            ..Default::default()
        },
    )?;
    if result.mismatches.is_empty() {
        observed.entries.extend(result.accepted.entries);
    }
    Ok((result.mismatches, result.detail))
}

struct SnapshotVerification {
    mismatches: Vec<PathBuf>,
    detail: Option<String>,
    accepted: ProjectSnapshot,
}

#[derive(Default)]
struct SettingsVerification {
    mismatch: Option<String>,
    accepted: Option<Vec<u8>>,
}

fn verify_snapshot_delta(
    before: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedPushVerification>,
    policy: Verification<'_>,
) -> Result<SnapshotVerification> {
    let settings_path = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    };
    let mut jobs = paths
        .iter()
        .filter(|path| {
            !entries_equivalent(path, before.entries.get(*path), desired.entries.get(*path))
        })
        .collect::<HashSet<_>>();
    if policy.settle {
        jobs.extend(observed.entries.keys().filter(|path| settings_path(path)));
    }
    let jobs = jobs
        .into_iter()
        .map(|path| (path, prepared.remove(path)))
        .collect::<Vec<_>>();
    // Services have independent identity graphs. Share the captured snapshots,
    // but give each worker ownership of its already-prepared documents.
    let results = jobs
        .into_par_iter()
        .map(|(path, prepared)| {
            let desired_entry = desired.entries.get(path);
            let result = if settings_path(path) {
                settings_delta_verification(
                    path,
                    before.entries.get(path),
                    desired_entry,
                    observed.entries.get(path),
                    prepared,
                    policy,
                )?
            } else if entries_equivalent(path, observed.entries.get(path), desired_entry) {
                SettingsVerification::default()
            } else {
                SettingsVerification {
                    mismatch: Some(format!(
                        "{} differs from the requested value",
                        path.display()
                    )),
                    accepted: None,
                }
            };
            Ok((path, result))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut differences = Vec::new();
    let mut accepted = ProjectSnapshot::default();
    for (path, result) in results {
        if let Some(detail) = result.mismatch {
            differences.push((path.clone(), detail));
        }
        if let Some(bytes) = result.accepted {
            accepted
                .entries
                .insert(path.clone(), SnapshotEntry::File(bytes));
        }
    }
    differences.sort_by(|left, right| left.0.cmp(&right.0));
    let detail = differences.first().map(|(_, detail)| detail.clone());
    Ok(SnapshotVerification {
        mismatches: differences.into_iter().map(|(path, _)| path).collect(),
        detail,
        accepted,
    })
}

#[cfg(test)]
pub(crate) fn settings_delta_mismatch(
    path: &Path,
    before: Option<&SnapshotEntry>,
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    prepared: Option<PreparedPushVerification>,
) -> Result<Option<String>> {
    Ok(settings_delta_verification(
        path,
        before,
        desired,
        observed,
        prepared,
        Verification::default(),
    )?
    .mismatch)
}

fn settings_delta_verification(
    path: &Path,
    before: Option<&SnapshotEntry>,
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    prepared: Option<PreparedPushVerification>,
    policy: Verification<'_>,
) -> Result<SettingsVerification> {
    let before_aligned = prepared.is_some();
    // The three stores decode side by side: on a large service each takes a
    // good part of a second.
    let (requested, observed) = rayon::join(
        || -> Result<_> {
            Ok(match prepared {
                Some(prepared) => (prepared.previous, prepared.desired),
                None => {
                    let (before, desired) =
                        rayon::join(|| settings_document(before), || settings_document(desired));
                    (before?, Arc::new(desired?))
                }
            })
        },
        || settings_document(observed),
    );
    let (mut before, desired) = requested?;
    let mut observed = observed?;
    let viewport = policy
        .settle
        .then(|| crate::settings::equivalence::reference_viewport_camera(&desired))
        .flatten();
    if desired.instances.is_empty()
        && !before.instances.is_empty()
        && !observed.instances.is_empty()
        && !align_settings_ids_to_reference(&before, &mut observed)
    {
        bail!(
            "Could not align the Studio identities in {}",
            path.display()
        );
    }
    if !desired.instances.is_empty() {
        if !before_aligned
            && !before.instances.is_empty()
            && !align_settings_ids_to_reference(&desired, &mut before)
        {
            bail!(
                "Could not align the previous identities in {}",
                path.display()
            );
        }
        if !observed.instances.is_empty()
            && !align_settings_ids_to_reference(&desired, &mut observed)
        {
            bail!(
                "Could not align the Studio identities in {}",
                path.display()
            );
        }
    }
    // This is a local comparison document only. It is never published or kept
    // as an equality proof when the push has pending writes.
    mask_reported_pending_fields(path, &desired, &mut observed, policy.pending)?;
    let before_by_id = before
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let desired_by_id = desired
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let observed_by_id = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<AHashMap<_, _>>();
    let settings_ids = before
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .chain(
            desired
                .instances
                .iter()
                .map(|instance| instance.settings_id.as_str())
                .filter(|id| !before_by_id.contains_key(id)),
        )
        .collect::<Vec<_>>();
    let compare = |settings_id: &str| {
        let before_index = before_by_id.get(settings_id).copied();
        let desired_index = desired_by_id.get(settings_id).copied();
        let observed_index = observed_by_id.get(settings_id).copied();
        if observed_index
            .is_some_and(|index| is_reconciliation_protected_workspace_camera(&observed, index))
        {
            // The active viewport is retained, even if its saved row was
            // removed or edited. Other cameras still receive full verification.
            return None;
        }
        if before_index.is_some_and(|index| before.instances[index].class_name == "PackageLink")
            && desired_index
                .is_some_and(|index| desired.instances[index].class_name == "PackageLink")
        {
            return None;
        }
        let name = desired_index
            .map(|index| desired.instances[index].name.as_str())
            .or_else(|| before_index.map(|index| before.instances[index].name.as_str()))
            .unwrap_or(settings_id);
        match (before_index, desired_index, observed_index) {
            (Some(before_index), None, Some(observed_index)) => {
                // A removed service store deletes its contents, not the engine's
                // service object. Match append_aligned_settings_push_plan.
                let previous = &before.instances[before_index];
                let actual = &observed.instances[observed_index];
                if previous.parent_index.is_none()
                    && actual.parent_index.is_none()
                    && previous.class_name == actual.class_name
                    && previous.name == actual.name
                {
                    return None;
                }
                // The container is engine-owned; omission removes its contents,
                // not the container. Authored containers still verify normally.
                if is_protected_engine_container(&before, before_index)
                    && is_protected_engine_container(&observed, observed_index)
                    && previous.class_name == actual.class_name
                {
                    return None;
                }
                Some(format!("{name} was not deleted from Studio"))
            }
            (None, Some(_), None) => Some(format!("{name} was not created in Studio")),
            (_, None, _) => None,
            (None, Some(desired_index), Some(observed_index)) => {
                added_instance_mismatch(&desired, desired_index, &observed, observed_index)
                    .map(|detail| format!("{name}.{detail}"))
            }
            (Some(before_index), Some(desired_index), Some(observed_index)) => {
                changed_instance_mismatch(
                    &before,
                    before_index,
                    &desired,
                    desired_index,
                    &observed,
                    observed_index,
                )
                .map(|detail| format!("{name}.{detail}"))
            }
            (Some(_), Some(_), None) => Some(format!("{name} disappeared from Studio")),
        }
    };
    // Read-only comparisons are independent once all three identity maps exist.
    // Keep the same first error regardless of which worker finishes first.
    let mismatch = if settings_ids.len() >= 1_024 {
        settings_ids.par_iter().find_map_first(|id| compare(id))
    } else {
        settings_ids.iter().find_map(|id| compare(id))
    };
    drop(before_by_id);
    drop(desired_by_id);
    drop(observed_by_id);
    let accepted = if policy.settle && mismatch.is_none() {
        if let Some(camera) = viewport {
            crate::settings::equivalence::keep_reference_viewport_camera(&camera, &mut observed);
        }
        Some(encode_settings_bytecode(&observed)?)
    } else {
        None
    };
    rayon::join(
        || drop_settings_documents(before, observed),
        || {
            if let Some(desired) = Arc::into_inner(desired) {
                drop_settings_document(desired);
            }
        },
    );
    Ok(SettingsVerification { mismatch, accepted })
}

fn mask_reported_pending_fields(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &mut SettingsBytecode,
    pending: &[PendingImportFields],
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let service = settings_service_name(path, desired, observed)?;
    let rows = pending
        .iter()
        .filter(|row| row.service == service)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Ok(());
    }
    let paths = crate::editor::paths::build_editor_instance_paths(desired, &service);
    let ids = crate::editor::review::EditorReferenceIds::new(
        desired
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str()),
    )
    .in_service(&service);
    let by_path = paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            path.as_ref().map(|path| {
                (
                    (path.path_segments.as_slice(), path.path_ordinals.as_slice()),
                    index,
                )
            })
        })
        .collect::<HashMap<_, _>>();
    let observed_by_id = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for row in rows {
        let Some(&index) =
            by_path.get(&(row.path_segments.as_slice(), row.path_ordinals.as_slice()))
        else {
            continue;
        };
        let expected = &desired.instances[index];
        if expected.class_name != row.class_name
            || row
                .settings_id
                .as_ref()
                .is_some_and(|id| id != &expected.settings_id)
        {
            continue;
        }
        let Some(&actual_index) = observed_by_id.get(&expected.settings_id) else {
            continue;
        };
        let actual = &mut observed.instances[actual_index];
        if actual.class_name != row.class_name {
            continue;
        }
        for (name, value) in &row.properties {
            if let Some(saved) = verification_value(&expected.properties, name, true) {
                let normalized =
                    crate::editor::review::normalize_editor_bridge_value(saved, None, &paths, &ids);
                if reconciliation_property_values_equal(
                    &row.class_name,
                    name,
                    Some(&normalized),
                    Some(value),
                ) {
                    actual.properties.insert(name.clone(), saved.clone());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn reported_pending_values_use_the_push_normalization_and_readback_equivalence() -> Result<()> {
    use crate::editor::review::{EditorReferenceIds, normalize_editor_bridge_value};
    let colors = rbx_dom_weak::types::MaterialColors::default().encode();
    for saved in [
        json!({"_type":"MaterialColors","base64":base64::encode(colors)}),
        json!({"BrickColor":21}),
        json!({"NumberSequence":{"keypoints":[{"time":0,"value":1,"envelope":0},{"time":1,"value":2,"envelope":0}]}}),
        json!({"ColorSequence":{"keypoints":[{"time":0,"color":[1,0,0]},{"time":1,"color":[0,1,0]}]}}),
        json!({"_type":"EnumItem","name":"Plastic"}),
    ] {
        let mut requested =
            normalize_editor_bridge_value(&saved, None, &[], &EditorReferenceIds::default());
        if requested["_type"] == "EnumItem" {
            requested["enumType"] = json!("Material");
        }
        assert_ne!(saved, requested);
        let mut instance =
            SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), Some(0));
        instance
            .properties
            .insert("FutureSavedField".into(), saved.clone());
        let desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                instance,
            ],
        };
        let mut pending = PendingImportFields {
            service: "Workspace".into(),
            settings_id: Some("part".into()),
            class_name: "Part".into(),
            path_segments: vec!["Workspace".into(), "Part".into()],
            path_ordinals: vec![1, 1],
            properties: Map::from_iter([("FutureSavedField".into(), requested)]),
        };
        let path = service_settings_path(Path::new("src/Workspace"));
        for wrong in [false, true] {
            if wrong {
                pending
                    .properties
                    .insert("FutureSavedField".into(), json!("another requested value"));
            }
            let mut observed = desired.clone();
            observed.instances[1]
                .properties
                .insert("FutureSavedField".into(), json!("not written"));
            observed.instances[1]
                .properties
                .insert("Unreported".into(), json!("changed"));
            mask_reported_pending_fields(
                &path,
                &desired,
                &mut observed,
                std::slice::from_ref(&pending),
            )?;
            assert_eq!(
                observed.instances[1].properties["FutureSavedField"],
                if wrong {
                    json!("not written")
                } else {
                    saved.clone()
                }
            );
            assert_eq!(observed.instances[1].properties["Unreported"], "changed");
        }
    }
    Ok(())
}

pub(crate) fn added_instance_mismatch(
    desired: &SettingsBytecode,
    desired_index: usize,
    observed: &SettingsBytecode,
    observed_index: usize,
) -> Option<String> {
    let desired_instance = &desired.instances[desired_index];
    let observed_instance = &observed.instances[observed_index];
    if desired_instance.name != observed_instance.name {
        return Some("Name was not retained".to_string());
    }
    if desired_instance.class_name != observed_instance.class_name {
        return Some("ClassName was not retained".to_string());
    }
    if settings_parent_id(desired, desired_index) != settings_parent_id(observed, observed_index) {
        return Some("Parent was not retained".to_string());
    }
    expected_map_mismatch(
        &desired_instance.properties,
        &observed_instance.properties,
        true,
        desired_instance,
    )
    .or_else(|| {
        expected_map_mismatch(
            &desired_instance.attributes,
            &observed_instance.attributes,
            false,
            desired_instance,
        )
    })
}

pub(crate) fn changed_instance_mismatch(
    before: &SettingsBytecode,
    before_index: usize,
    desired: &SettingsBytecode,
    desired_index: usize,
    observed: &SettingsBytecode,
    observed_index: usize,
) -> Option<String> {
    let before_instance = &before.instances[before_index];
    let desired_instance = &desired.instances[desired_index];
    let observed_instance = &observed.instances[observed_index];
    // The push asked nothing of an instance the files hold as Studio held it.
    if crate::settings::held::stands_unchanged(&before_instance.properties) {
        return None;
    }
    for (label, previous, expected, actual) in [
        (
            "Name",
            before_instance.name.as_str(),
            desired_instance.name.as_str(),
            observed_instance.name.as_str(),
        ),
        (
            "ClassName",
            before_instance.class_name.as_str(),
            desired_instance.class_name.as_str(),
            observed_instance.class_name.as_str(),
        ),
    ] {
        if previous != expected && actual != expected {
            return Some(format!("{label} was not retained"));
        }
    }
    let previous_parent = settings_parent_id(before, before_index);
    let expected_parent = settings_parent_id(desired, desired_index);
    if previous_parent != expected_parent
        && settings_parent_id(observed, observed_index) != expected_parent
    {
        return Some("Parent was not retained".to_string());
    }
    changed_map_mismatch(
        &before_instance.properties,
        &desired_instance.properties,
        &observed_instance.properties,
        true,
        desired_instance,
    )
    .or_else(|| {
        changed_map_mismatch(
            &before_instance.attributes,
            &desired_instance.attributes,
            &observed_instance.attributes,
            false,
            desired_instance,
        )
    })
}

const ASSEMBLY_POSITION_TOLERANCE: f64 = 1e-3;
const ASSEMBLY_ROTATION_TOLERANCE: f64 = 1e-4;

fn cframes_within_assembly_tolerance(left: &Value, right: &Value) -> bool {
    let components = |value: &Value| {
        value
            .get("components")?
            .as_array()
            .filter(|components| components.len() == 12)?
            .iter()
            .map(Value::as_f64)
            .collect::<Option<Vec<f64>>>()
    };
    let (Some(left), Some(right)) = (components(left), components(right)) else {
        return false;
    };
    left.iter().zip(&right).enumerate().all(|(index, (a, b))| {
        let tolerance = if index < 3 {
            ASSEMBLY_POSITION_TOLERANCE
        } else {
            ASSEMBLY_ROTATION_TOLERANCE
        };
        a.is_finite() && b.is_finite() && (a - b).abs() <= tolerance
    })
}

pub(crate) fn verification_values_equal(
    properties: bool,
    class_name: &str,
    name: &str,
    left: Option<&Value>,
    right: Option<&Value>,
) -> bool {
    if properties {
        // Studio recalculates this state when inserting a package or changing
        // its contents. Verify the package identity/content, not that readback.
        if class_name == "PackageLink" && name == "ModifiedState" {
            return true;
        }
        // A part inside a welded assembly gets its CFrame re-derived from the
        // assembly root through the weld offsets once the tree is in the
        // place, which moves it by a few float steps; a lost CFrame is off by
        // whole studs.
        if name == "CFrame"
            && let (Some(left), Some(right)) = (left, right)
            && cframes_within_assembly_tolerance(left, right)
        {
            return true;
        }
        // The physics engine owns a part's velocities: a part inside a welded
        // assembly gets its velocity recomputed from the assembly root once
        // the tree is in the place, so the pushed value cannot be retained.
        if matches!(
            name,
            "Velocity" | "RotVelocity" | "AssemblyLinearVelocity" | "AssemblyAngularVelocity"
        ) && right.is_some()
        {
            return true;
        }
        // Studio issues its own texture pack asset for the maps an appearance
        // carries, so a pushed pack id is a hint and whichever pack Studio
        // holds afterwards is the retained value.
        if crate::roblox::schema::has_protected_texture_pack(class_name)
            && name == crate::roblox::schema::TEXTURE_PACK_PROPERTY
            && right.is_some_and(|pack| pack.as_str().is_none_or(|uri| !uri.is_empty()))
        {
            return true;
        }
        reconciliation_property_values_equal(class_name, name, left, right)
    } else {
        match (left, right) {
            (Some(left), Some(right)) => reconciliation_values_equal(left, right, false),
            (None, None) => true,
            _ => false,
        }
    }
}

/// Whether two stores differ in nothing a push writes: only in the fields of
/// a service root that Roblox takes from elsewhere (the player capacity set in
/// Game Settings). Such a store is not a difference a push can settle.
pub(crate) fn differs_only_in_externally_managed_fields(
    current: &SettingsBytecode,
    previous: &SettingsBytecode,
) -> bool {
    let managed = |instance: &SettingsBytecodeInstance, property: &str| {
        instance.parent_index.is_none()
            && is_externally_managed_editor_property(
                &instance.name,
                &instance.class_name,
                std::slice::from_ref(&instance.name),
                property,
            )
    };
    let holds_managed = |document: &SettingsBytecode| {
        document.instances.iter().any(|instance| {
            instance
                .properties
                .keys()
                .any(|property| managed(instance, property))
        })
    };
    if !holds_managed(current) && !holds_managed(previous) {
        return false;
    }
    let without_managed = |document: &SettingsBytecode| {
        let mut document = document.clone();
        for index in 0..document.instances.len() {
            let kept = document.instances[index]
                .properties
                .iter()
                .filter(|(property, _)| !managed(&document.instances[index], property))
                .map(|(property, value)| (property.clone(), value.clone()))
                .collect();
            document.instances[index].properties = kept;
        }
        document
    };
    settings_documents_equivalent(&without_managed(current), &without_managed(previous))
}

pub(crate) fn verification_skips_property(
    instance: &SettingsBytecodeInstance,
    name: &str,
    forced: &Map<String, Value>,
) -> bool {
    let class_name = &instance.class_name;
    name == "ScriptGuid"
        || (name == "Source" && is_lua_source_class(class_name))
        || (instance.parent_index.is_none()
            && is_externally_managed_editor_property(
                &instance.name,
                class_name,
                std::slice::from_ref(&instance.name),
                name,
            ))
        || reconciliation_property_is_derived(name)
        // A value the engine owns (a weld's State, a collision group's
        // numeric id) is not read back the way a store may carry it.
        || crate::settings::equivalence::reconciliation_property_is_engine_state(
            rbx_reflection_database::get().ok(),
            class_name,
            name,
            forced.get(name).unwrap_or(&Value::Null),
        )
        || crate::settings::equivalence::reconciliation_property_is_forced(name, forced)
        || rbx_reflection_database::get().is_ok_and(|database| {
            crate::editor::review::is_editor_state_property(class_name, name, database)
        })
}

pub(crate) fn verification_value<'a>(
    values: &'a Map<String, Value>,
    name: &str,
    properties: bool,
) -> Option<&'a Value> {
    if properties {
        reconciliation_property_value(values, name)
    } else {
        values.get(name)
    }
}

fn saved_field_without_plugin_api(class_name: &str, name: &str) -> bool {
    rbx_reflection_database::get().is_ok_and(|database| {
        crate::editor::review::plugin_cannot_write_property(database, class_name, name)
    })
}

// A push leaves the engine-managed fields of a service root out (a deprecated
// Lighting.Outlines, a hidden Workspace.Wind), and Studio's record of a root,
// the plugin's reading plus the native unscriptable fields, leaves them out
// too, so a root record without one says nothing about the files' value.
fn root_field_push_never_sends(instance: &SettingsBytecodeInstance, name: &str) -> bool {
    instance.parent_index.is_none()
        && rbx_reflection_database::get().is_ok_and(|database| {
            crate::editor::review::is_engine_managed_editor_property(
                &instance.class_name,
                name,
                database,
            )
        })
}

pub(crate) fn expected_map_mismatch(
    expected: &Map<String, Value>,
    actual: &Map<String, Value>,
    properties: bool,
    instance: &SettingsBytecodeInstance,
) -> Option<String> {
    let class_name = &instance.class_name;
    expected.iter().find_map(|(name, value)| {
        if properties
            && name == "PhysicsGrid"
            && crate::settings::equivalence::terrain_physics_caches_match(
                class_name, expected, actual,
            )
        {
            return None;
        }
        if !properties && crate::settings::equivalence::is_engine_managed_attribute(name) {
            return None;
        }
        if properties
            && (verification_skips_property(instance, name, expected)
                || crate::settings::equivalence::reconciliation_property_is_metadata(name, value))
        {
            return None;
        }
        let actual = verification_value(actual, name, properties);
        if properties
            && actual.is_none()
            && (crate::settings::equivalence::capture_may_omit(class_name, name)
                || root_field_push_never_sends(instance, name))
        {
            return None;
        }
        if properties && saved_field_without_plugin_api(class_name, name) {
            return None;
        }
        (!verification_values_equal(properties, class_name, name, Some(value), actual))
            .then(|| retention_mismatch(name, Some(value), actual))
    })
}

fn is_blank_scalar(value: &Value) -> bool {
    match value {
        Value::Bool(value) => !value,
        Value::Number(value) => value.as_f64() == Some(0.0),
        Value::String(value) => value.is_empty(),
        _ => false,
    }
}

pub(crate) fn retention_mismatch(
    name: &str,
    expected: Option<&Value>,
    actual: Option<&Value>,
) -> String {
    format!(
        "{name} was not retained (expected {}, Studio has {})",
        short_reconciliation_value(expected),
        short_reconciliation_value(actual)
    )
}

pub(crate) fn changed_map_mismatch(
    before: &Map<String, Value>,
    desired: &Map<String, Value>,
    observed: &Map<String, Value>,
    properties: bool,
    instance: &SettingsBytecodeInstance,
) -> Option<String> {
    if before == desired {
        return None;
    }
    let mut names = before.keys().chain(desired.keys()).collect::<Vec<_>>();
    names.sort();
    names.dedup();
    let class_name = &instance.class_name;
    names.into_iter().find_map(|name| {
        // Voxel replacement schedules a collision-cache rebuild. A physics-only
        // write still needs exact readback; matching voxels alone cannot verify it.
        if properties
            && name == "PhysicsGrid"
            && before.get("SmoothGrid") != desired.get("SmoothGrid")
            && crate::settings::equivalence::terrain_physics_caches_match(
                class_name, desired, observed,
            )
        {
            return None;
        }
        if !properties && crate::settings::equivalence::is_engine_managed_attribute(name) {
            return None;
        }
        if properties && verification_skips_property(instance, name, desired) {
            return None;
        }
        let previous = verification_value(before, name, properties);
        let expected = verification_value(desired, name, properties);
        if verification_values_equal(properties, class_name, name, previous, expected) {
            return None;
        }
        let actual = verification_value(observed, name, properties);
        if properties
            && actual.is_none()
            && (crate::settings::equivalence::capture_may_omit(class_name, name)
                || root_field_push_never_sends(instance, name))
        {
            return None;
        }
        if properties && saved_field_without_plugin_api(class_name, name) {
            return None;
        }
        if properties
            && expected.is_none()
            && rbx_reflection_database::get().is_ok_and(|database| {
                crate::editor::review::is_engine_managed_editor_property(class_name, name, database)
            })
        {
            return None;
        }
        // A record that leaves the field out says nothing about it: the files
        // never state a field that does not serialize (HumanoidDescription's
        // HeadColor), and a capture may omit a fidelity at its default.
        if properties
            && expected.is_none()
            && crate::settings::equivalence::reconciliation_property_is_unknown_when_absent(
                rbx_reflection_database::get().ok(),
                class_name,
                name,
            )
        {
            return None;
        }
        // A field the reflection database does not know yet (a Sound's
        // IsMutedForCapture on Studio 0.746) is enumerated by no capture, so
        // when neither capture carried it and the files hold its blank value
        // there is nothing to compare.
        if properties
            && previous.is_none()
            && actual.is_none()
            && expected.is_some_and(is_blank_scalar)
            && rbx_reflection_database::get().is_ok_and(|database| {
                crate::rbx::encode::rbx_property_descriptor(database, class_name, name).is_none()
            })
        {
            return None;
        }
        // Property equivalence already accepts omitted class defaults.
        // Attribute absence still means deletion, including false/zero values.
        (!verification_values_equal(properties, class_name, name, expected, actual))
            .then(|| retention_mismatch(name, expected, actual))
    })
}

pub(crate) fn snapshot_entry_equivalent(
    path: &Path,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
    prepared: Option<&mut Option<PreparedEditorSettingsChange>>,
) -> Result<bool> {
    if entries_equivalent(path, left, right) {
        return Ok(true);
    }
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(false);
    };
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_service_settings_file_name)
    {
        // Whether two store files are equivalent depends on their bytes
        // alone, so a pair found equivalent before is not decoded again.
        let pair = match (left, right) {
            (SnapshotEntry::File(left), SnapshotEntry::File(right)) => Some(rayon::join(
                || <[u8; 32]>::from(Sha256::digest(left)),
                || <[u8; 32]>::from(Sha256::digest(right)),
            )),
            _ => None,
        };
        if pair.is_some_and(|pair| equivalent_stores().lock_recover().contains(&pair)) {
            return Ok(true);
        }
        let (left, right) = rayon::join(
            || settings_document(Some(left)),
            || settings_document(Some(right)),
        );
        let mut left = left?;
        let mut right = right?;
        let phase = Instant::now();
        let positional = settings_documents_positionally_equivalent(&left, &right);
        log_global(
            4,
            format_args!(
                "[renium] reconcile positional {}: {:.1}ms matched={positional}",
                path.display(),
                elapsed_ms(phase)
            ),
        );
        let phase = Instant::now();
        let equivalent = if positional {
            Some(true)
        } else if align_settings_ids_to_reference(&left, &mut right) {
            log_global(
                4,
                format_args!(
                    "[renium] reconcile identity {}: {:.1}ms",
                    path.display(),
                    elapsed_ms(phase)
                ),
            );
            let phase = Instant::now();
            let matched = settings_documents_equivalent(&left, &right);
            log_global(
                4,
                format_args!(
                    "[renium] reconcile values {}: {:.1}ms matched={matched}",
                    path.display(),
                    elapsed_ms(phase)
                ),
            );
            Some(matched)
        } else {
            None
        };
        if equivalent == Some(false)
            && let Some(prepared) = prepared
        {
            // Comparison already resolved duplicate identities. Reuse that exact
            // mapping and the decoded documents when planning the push.
            crate::settings::equivalence::inherit_workspace_viewport_reference(&mut left, &right);
            align_equivalent_values(&left, &mut right);
            *prepared = Some(PreparedEditorSettingsChange {
                previous: right,
                current: left,
            });
            return Ok(false);
        }
        let phase = Instant::now();
        drop_settings_documents(left, right);
        log_global(
            4,
            format_args!(
                "[renium] reconcile release {}: {:.1}ms",
                path.display(),
                elapsed_ms(phase)
            ),
        );
        if let Some(equivalent) = equivalent {
            if equivalent && let Some(pair) = pair {
                let mut pairs = equivalent_stores().lock_recover();
                if pairs.len() >= 4_096 {
                    pairs.clear();
                }
                pairs.insert(pair);
            }
            return Ok(equivalent);
        }
        {
            bail!(
                "Failed to align duplicate instance identities in {}",
                path.display()
            );
        }
    }
    Ok(false)
}

/// The digests of two store files, left then right.
type StorePair = ([u8; 32], [u8; 32]);

/// The pairs of store files a comparison found equivalent, by the digests of
/// their bytes.
pub(super) fn equivalent_stores() -> &'static Mutex<HashSet<StorePair>> {
    static PAIRS: std::sync::OnceLock<Mutex<HashSet<StorePair>>> = std::sync::OnceLock::new();
    PAIRS.get_or_init(Default::default)
}

pub(crate) fn retention_failure(
    what: &str,
    mismatches: &[PathBuf],
    details: Option<&str>,
) -> anyhow::Error {
    anyhow::anyhow!(
        "Studio did not retain {what}: {}{}",
        mismatches
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", "),
        details
            .map(|details| format!(" ({details})"))
            .unwrap_or_default()
    )
}

pub(crate) fn snapshot_mismatch_details(
    observed: &ProjectSnapshot,
    expected: &ProjectSnapshot,
    paths: &[PathBuf],
) -> Result<Option<String>> {
    let Some(path) = paths.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    }) else {
        return Ok(None);
    };
    let database = rbx_reflection_database::get()?;
    let observed_document = settings_document(observed.entries.get(path))?;
    let mut expected_document = settings_document(expected.entries.get(path))?;
    if observed_document.instances.len() != expected_document.instances.len() {
        return Ok(Some(format!(
            "Studio returned {} instances; expected {}",
            observed_document.instances.len(),
            expected_document.instances.len()
        )));
    }
    if !align_settings_ids_to_reference(&observed_document, &mut expected_document) {
        return Ok(Some(
            "Studio returned instance identities that could not be aligned".to_string(),
        ));
    }
    let expected_by_id = expected_document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    for (observed_index, observed) in observed_document.instances.iter().enumerate() {
        let Some(expected_index) = expected_by_id.get(observed.settings_id.as_str()).copied()
        else {
            return Ok(Some(format!(
                "Studio returned an unexpected instance identity at {}",
                observed.name
            )));
        };
        let expected = &expected_document.instances[expected_index];
        if observed.name != expected.name
            || observed.class_name != expected.class_name
            || settings_parent_id(&observed_document, observed_index)
                != settings_parent_id(&expected_document, expected_index)
        {
            return Ok(Some(format!(
                "Studio returned a different structure at {}",
                expected.name
            )));
        }
        if is_reconciliation_protected_workspace_camera(&expected_document, expected_index) {
            continue;
        }
        let instance_name = &expected.name;
        let class_name = &expected_document.instances[expected_index].class_name;
        for (kind, observed, expected) in [
            ("property", &observed.properties, &expected.properties),
            ("attribute", &observed.attributes, &expected.attributes),
        ] {
            let mut names = observed.keys().chain(expected.keys()).collect::<Vec<_>>();
            names.sort();
            names.dedup();
            for name in names {
                if kind == "attribute"
                    && crate::settings::equivalence::is_engine_managed_attribute(name)
                {
                    continue;
                }
                if kind == "property"
                    && !crate::settings::equivalence::reconciliation_property_compares(
                        Some(database),
                        class_name,
                        name,
                        observed
                            .get(name)
                            .or_else(|| expected.get(name))
                            .unwrap_or(&Value::Null),
                        observed.contains_key(name),
                        expected.contains_key(name),
                    )
                {
                    continue;
                }
                let observed_value = if kind == "property" {
                    reconciliation_property_value(observed, name)
                } else {
                    observed.get(name)
                };
                let expected_value = if kind == "property" {
                    reconciliation_property_value(expected, name)
                } else {
                    expected.get(name)
                };
                let retained = match (observed_value, expected_value) {
                    (Some(observed), Some(expected)) if kind == "property" => {
                        reconciliation_property_values_equal(
                            &expected_document.instances[expected_index].class_name,
                            name,
                            Some(observed),
                            Some(expected),
                        )
                    }
                    (Some(observed), Some(expected)) => {
                        reconciliation_values_equal(observed, expected, false)
                    }
                    (None, None) => true,
                    (Some(observed), None) if kind == "property" => {
                        crate::settings::equivalence::reconciliation_property_value_is_default(
                            class_name, name, observed,
                        )
                    }
                    (None, Some(expected)) if kind == "property" => {
                        crate::settings::equivalence::reconciliation_property_value_is_default(
                            class_name, name, expected,
                        )
                    }
                    _ => false,
                };
                if !retained {
                    let detail = match (observed_value, expected_value) {
                        (None, Some(expected)) => format!(
                            "is missing from Studio; the files have {}",
                            reconcile_value_label(expected)
                        ),
                        (Some(observed), None) => {
                            format!("was added by Studio as {}", reconcile_value_label(observed))
                        }
                        (Some(observed), Some(expected)) => format!(
                            "is {}; expected {}",
                            reconcile_value_label(observed),
                            reconcile_value_label(expected)
                        ),
                        (None, None) => continue,
                    };
                    let mut path = Vec::new();
                    let mut current = Some(expected_index);
                    while let Some(index) = current {
                        path.push(expected_document.instances[index].name.as_str());
                        current = expected_document.instances[index].parent_index;
                    }
                    path.reverse();
                    let _ = instance_name;
                    return Ok(Some(format!("{}.{name} {kind} {detail}", path.join("/"))));
                }
            }
        }
        if !reconciliation_maps_equal(class_name, &expected.properties, &observed.properties)
            || !reconciliation_values_map_equal(&expected.attributes, &observed.attributes)
        {
            let mut path = Vec::new();
            let mut current = Some(expected_index);
            while let Some(index) = current {
                path.push(expected_document.instances[index].name.as_str());
                current = expected_document.instances[index].parent_index;
            }
            path.reverse();
            let mut one_sided = expected
                .properties
                .keys()
                .filter(|name| !observed.properties.contains_key(*name))
                .map(|name| format!("{name} (files only)"))
                .chain(
                    observed
                        .properties
                        .keys()
                        .filter(|name| !expected.properties.contains_key(*name))
                        .map(|name| format!("{name} (Studio only)")),
                )
                .collect::<Vec<_>>();
            one_sided.sort();
            return Ok(Some(format!(
                "{} differs from the files: {}",
                path.join("/"),
                if one_sided.is_empty() {
                    "same property set with different values".to_string()
                } else {
                    one_sided.join(", ")
                }
            )));
        }
    }
    Ok(None)
}

pub(crate) fn reconcile_value_label(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => format!("{value:?}"),
        Value::Array(value) => format!("an array of {} values", value.len()),
        Value::Object(value) if value.get("_type").and_then(Value::as_str) == Some("CFrame") => {
            let components = value.get("components").and_then(Value::as_array);
            format!(
                "CFrame({})",
                components
                    .map(|values| values
                        .iter()
                        .map(reconcile_value_label)
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_else(|| "missing components".into())
            )
        }
        Value::Object(value) if value.get("_type").and_then(Value::as_str) == Some("Float") => {
            value
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("invalid Float")
                .to_string()
        }
        Value::Object(value) if value.get("_type").and_then(Value::as_str) == Some("Ref") => {
            let id = value
                .get("settingsId")
                .or_else(|| value.get("instanceId"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            let path = value
                .get("pathSegments")
                .and_then(Value::as_array)
                .map(|segments| {
                    segments
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(".")
                })
                .unwrap_or_default();
            format!("Ref({id}, {path})")
        }
        Value::Object(value) => value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("_type").and_then(Value::as_str))
            .map_or_else(|| "an object".to_string(), |value| value.to_string()),
    }
}

pub(crate) fn snapshot_differences(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
) -> Result<HashSet<PathBuf>> {
    snapshot_differences_prepared(left, right, None)
}

pub(crate) fn retained_settings_document(document: &SettingsBytecode) -> SettingsBytecode {
    let mut retained = BTreeSet::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if instance.parent_index.is_none()
            || is_protected_engine_container(document, index)
            || is_reconciliation_protected_workspace_camera(document, index)
        {
            let mut ancestor = Some(index);
            while let Some(index) = ancestor {
                if !retained.insert(index) {
                    break;
                }
                ancestor = document.instances[index].parent_index;
            }
        }
    }
    let remap = retained
        .iter()
        .enumerate()
        .map(|(new, old)| (*old, new))
        .collect::<HashMap<_, _>>();
    SettingsBytecode {
        version: document.version,
        instances: retained
            .into_iter()
            .map(|index| {
                let mut instance = document.instances[index].clone();
                instance.parent_index = instance.parent_index.map(|parent| remap[&parent]);
                instance
            })
            .collect(),
    }
}
