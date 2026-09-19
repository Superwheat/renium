use super::*;

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

pub(crate) fn snapshot_intended_delta_mismatches(
    before: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared: &mut HashMap<PathBuf, PreparedPushVerification>,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    let jobs = paths
        .iter()
        .filter(|path| {
            !entries_equivalent(path, before.entries.get(*path), desired.entries.get(*path))
        })
        .map(|path| (path, prepared.remove(path)))
        .collect::<Vec<_>>();
    // Services have independent identity graphs. Share the captured snapshots,
    // but give each worker ownership of its already-prepared documents.
    let results = jobs
        .into_par_iter()
        .map(|(path, prepared)| {
            let desired_entry = desired.entries.get(path);
            let mismatch = if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
            {
                settings_delta_mismatch(
                    path,
                    before.entries.get(path),
                    desired_entry,
                    observed.entries.get(path),
                    prepared,
                )?
            } else if entries_equivalent(path, observed.entries.get(path), desired_entry) {
                None
            } else {
                Some(format!(
                    "{} differs from the requested value",
                    path.display()
                ))
            };
            Ok(mismatch.map(|detail| (path.clone(), detail)))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut differences = results.into_iter().flatten().collect::<Vec<_>>();
    differences.sort_by(|left, right| left.0.cmp(&right.0));
    let detail = differences.first().map(|(_, detail)| detail.clone());
    Ok((
        differences.into_iter().map(|(path, _)| path).collect(),
        detail,
    ))
}

pub(crate) fn settings_delta_mismatch(
    path: &Path,
    before: Option<&SnapshotEntry>,
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
    prepared: Option<PreparedPushVerification>,
) -> Result<Option<String>> {
    let before_aligned = prepared.is_some();
    let (mut before, desired) = match prepared {
        Some(prepared) => (prepared.previous, prepared.desired),
        None => (
            settings_document(before)?,
            Arc::new(settings_document(desired)?),
        ),
    };
    let mut observed = settings_document(observed)?;
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
    rayon::join(
        || drop_settings_documents(before, observed),
        || {
            if let Some(desired) = Arc::into_inner(desired) {
                drop_settings_document(desired);
            }
        },
    );
    Ok(mismatch)
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
        reconciliation_property_values_equal(class_name, name, left, right)
    } else {
        match (left, right) {
            (Some(left), Some(right)) => reconciliation_values_equal(left, right, false),
            (None, None) => true,
            _ => false,
        }
    }
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
        || crate::settings::equivalence::reconciliation_property_is_forced(name, forced)
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

pub(crate) fn expected_map_mismatch(
    expected: &Map<String, Value>,
    actual: &Map<String, Value>,
    properties: bool,
    instance: &SettingsBytecodeInstance,
) -> Option<String> {
    let class_name = &instance.class_name;
    expected.iter().find_map(|(name, value)| {
        if properties
            && (verification_skips_property(instance, name, expected)
                || crate::settings::equivalence::reconciliation_property_is_metadata(name, value))
        {
            return None;
        }
        let actual = verification_value(actual, name, properties);
        (!verification_values_equal(properties, class_name, name, Some(value), actual))
            .then(|| retention_mismatch(name, Some(value), actual))
    })
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
        if properties && verification_skips_property(instance, name, desired) {
            return None;
        }
        let previous = verification_value(before, name, properties);
        let expected = verification_value(desired, name, properties);
        if verification_values_equal(properties, class_name, name, previous, expected) {
            return None;
        }
        let actual = verification_value(observed, name, properties);
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
