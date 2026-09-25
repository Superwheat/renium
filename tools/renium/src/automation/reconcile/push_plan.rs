use super::*;

pub(crate) struct StagedPushResult {
    pub(crate) generated: ProjectSnapshot,
    pub(crate) summary: Map<String, Value>,
}

#[derive(Default)]
pub(crate) struct AppliedEditorChanges {
    pub(crate) accepted: BTreeMap<PathBuf, Option<PublishEntryState>>,
    pub(crate) summary: Map<String, Value>,
}

pub(crate) fn accepted_editor_entries(
    root: &Path,
    previous: &ProjectSnapshot,
    current: &ProjectSnapshot,
    generated: &ProjectSnapshot,
) -> BTreeMap<PathBuf, Option<PublishEntryState>> {
    previous
        .entries
        .keys()
        .chain(current.entries.keys())
        .chain(generated.entries.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| {
            let entry = generated
                .entries
                .get(path)
                .or_else(|| current.entries.get(path));
            let state = entry.map(|entry| match entry {
                SnapshotEntry::Directory => PublishEntryState::Directory,
                SnapshotEntry::File(bytes) => PublishEntryState::File {
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                    length: bytes.len() as u64,
                    hash: fnv1a(bytes),
                },
                SnapshotEntry::Symlink { target, .. } => PublishEntryState::Symlink(target.clone()),
            });
            (root.join(path), state)
        })
        .collect()
}

#[test]
pub(crate) fn editor_acknowledgment_uses_captured_and_generated_bytes_including_deletions() {
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let removed = PathBuf::from("src/removed.luau");
    let previous = ProjectSnapshot {
        entries: BTreeMap::from([
            (path.clone(), SnapshotEntry::File(b"old".to_vec())),
            (removed.clone(), SnapshotEntry::File(b"removed".to_vec())),
        ]),
    };
    let current = ProjectSnapshot {
        entries: BTreeMap::from([(path.clone(), SnapshotEntry::File(b"captured".to_vec()))]),
    };
    let generated = ProjectSnapshot {
        entries: BTreeMap::from([(path.clone(), SnapshotEntry::File(b"generated".to_vec()))]),
    };
    let root = Path::new("project");
    let accepted = accepted_editor_entries(root, &previous, &current, &generated);
    assert!(matches!(accepted.get(&root.join(removed)), Some(None)));
    assert!(
        matches!(accepted.get(&root.join(path)), Some(Some(PublishEntryState::File { length: 9, hash, .. })) if *hash == fnv1a(b"generated"))
    );
}

pub(crate) struct StagedPushRequest<'a> {
    pub(crate) plan: ReconcilePushPlan,
    pub(crate) prepared_documents: HashMap<String, Arc<SettingsBytecode>>,
    pub(crate) guard: Option<&'a StudioChangeGuard>,
    pub(crate) args: PushEditorChangesArgs,
    pub(crate) expected_project: Option<&'a ProjectSnapshot>,
    /// Live Sync keeps later edits pending and pushes them next, so a file
    /// edited during this push is not a reason to refuse it.
    pub(crate) later_edits_follow: bool,
}

pub(crate) fn push_staged_project(
    context: &BoundContext,
    stage: &ExportProjectStage,
    bridge: &BridgeServer,
    request: StagedPushRequest<'_>,
) -> Result<StagedPushResult> {
    let StagedPushRequest {
        plan,
        prepared_documents,
        guard,
        args: mut push_args,
        expected_project,
        later_edits_follow,
    } = request;
    if plan.is_empty() {
        return Ok(StagedPushResult {
            generated: ProjectSnapshot::default(),
            summary: Map::from_iter([("ok".to_string(), Value::Bool(true))]),
        });
    }
    let changed_paths = plan.changed_paths.iter().cloned().collect::<HashSet<_>>();
    let supporting_paths = supporting_settings_scopes(context, &changed_paths)?
        .into_iter()
        .filter(|path| !changed_paths.contains(path))
        .collect::<Vec<_>>();
    // A reconcile pushes the project as it captured it; the stage already
    // holds that supporting data, and later edits stay pending in Live Sync.
    if !supporting_paths.is_empty() && !later_edits_follow {
        let root = Path::new(&context.root);
        let staged = expected_project
            .is_none()
            .then(|| capture_snapshot(&stage.project_root, &supporting_paths))
            .transpose()?;
        let current = capture_snapshot(root, &supporting_paths)?;
        let supporting_paths = supporting_paths.into_iter().collect::<HashSet<_>>();
        let reference = expected_project
            .or(staged.as_ref())
            .expect("reference snapshot");
        let differences = snapshot_path_differences(reference, &current, &supporting_paths)?;
        if !differences.is_empty() {
            bail!(
                "Supporting project data {} changed while its Studio update was being prepared; retry the sync",
                root.join(&differences[0]).display()
            );
        }
        if expected_project.is_none() {
            apply_snapshot_paths(&stage.project_root, &supporting_paths, &current)?;
        }
    }
    let project_root = push_args.project.project_root.clone();
    let planned_paths = plan.changed_paths.clone();
    // Only the files this push sends matter here. Edits elsewhere in the
    // project stay pending in Live Sync and follow once this push lands.
    let validate_project = || -> Result<()> {
        let Some(expected_project) = expected_project.filter(|_| !later_edits_follow) else {
            return Ok(());
        };
        let current = capture_snapshot(&project_root, &planned_paths)?;
        let planned = planned_paths.iter().cloned().collect::<HashSet<_>>();
        let expected = ProjectSnapshot {
            entries: expected_project
                .entries
                .iter()
                .filter(|(path, _)| planned.iter().any(|scope| path.starts_with(scope)))
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .collect(),
        };
        let differences = snapshot_differences(&expected, &current)?;
        if let Some(changed) = differences.iter().next() {
            bail!(
                "Project file {} changed while its Studio update was being prepared; retry the sync",
                project_root.join(changed).display()
            );
        }
        Ok(())
    };
    let staged = staged_context(context, stage, &push_args.project.src_root)?;
    let _selection = bound_context::select(&staged);
    pin_edit_runtime(&staged, bridge)?;
    let source_relative =
        project_relative_source_root(&push_args.project.project_root, &push_args.project.src_root)?;
    push_args
        .project
        .project_root
        .clone_from(&stage.project_root);
    push_args.project.src_root = stage.project_root.join(source_relative);
    push_args.paths.clear();
    push_args.changed_paths.clone_from(&plan.changed_paths);
    push_args.changed_paths_files.clear();
    push_args
        .target_settings_ids
        .clone_from(&plan.target_settings_ids);
    push_args.target_settings_id_files.clear();
    push_args.target_properties.clear();
    push_args.verify_sources = true;
    push_args.upsert_instances_only = false;
    let initial_documents = prepared_documents
        .iter()
        .filter(|(service, _)| plan.initial_native_services.contains(*service))
        .map(|(service, document)| (service.clone(), Arc::clone(document)))
        .collect::<HashMap<_, _>>();
    let mut generated = ProjectSnapshot::default();
    let summary = push_reconciled_editor_changes_with_warm_bridge(
        push_args,
        bridge,
        guard,
        PreparedEditorDocuments {
            documents: prepared_documents,
            native_services: plan.initial_native_services.clone(),
        },
        |changes| {
            amend_reconciled_changes(changes, plan)?;
            stage_native_bootstrap_services(changes, &initial_documents);
            Ok(())
        },
        |changes| {
            generated = redirect_staged_settings_writes(
                changes,
                &stage.project_root,
                Path::new(&context.root),
                expected_project,
            )?;
            Ok(())
        },
        validate_project,
    )?;
    if summary.get("skippedByReview").and_then(Value::as_bool) == Some(true) {
        bail!("Reconciled changes require review before Studio can be updated");
    }
    for document in initial_documents.into_values() {
        if let Some(document) = Arc::into_inner(document) {
            drop_settings_document(document);
        }
    }
    Ok(StagedPushResult { generated, summary })
}

pub(crate) fn stage_native_bootstrap_services(
    changes: &mut EditorChangeSet,
    documents: &HashMap<String, Arc<SettingsBytecode>>,
) {
    changes
        .instance_changes
        .retain(|change| !documents.contains_key(&change.service));
    for (service, document) in documents {
        if changes.native_property_documents.contains_key(service) {
            changes.instance_changes.push(EditorInstanceChange {
                service: service.clone(),
                mode: "reconcileService".into(),
                allow_deletes: true,
                instances: Vec::new(),
                preserve_instances: Vec::new(),
            });
        } else {
            crate::editor::diff::append_editor_instance_reconcile(changes, document, service);
        }
    }
}

pub(crate) fn redirect_staged_settings_writes(
    changes: &mut EditorChangeSet,
    staged_root: &Path,
    project_root: &Path,
    expected_project: Option<&ProjectSnapshot>,
) -> Result<ProjectSnapshot> {
    let mut generated = ProjectSnapshot::default();
    for write in &mut changes.settings_writes {
        let relative = write
            .path
            .strip_prefix(staged_root)
            .with_context(|| {
                format!(
                    "Generated settings path {} is outside its reconciliation stage",
                    write.path.display()
                )
            })?
            .to_path_buf();
        let destination = project_root.join(&relative);
        // The staged document may already contain merged Studio data or aligned IDs.
        // Its hash guards the stage, not the original project we will publish into.
        let expected_hash = if let Some(project) = expected_project {
            match project.entries.get(&relative) {
                Some(SnapshotEntry::File(bytes)) => Some(Sha256::digest(bytes).into()),
                None => None,
                Some(_) => bail!("Settings path {} was not a file", destination.display()),
            }
        } else {
            write.expected_hash
        };
        if settings_file_hash(&destination)? != expected_hash {
            bail!(
                "Settings file {} changed while its Studio update was being prepared; retry the sync",
                destination.display()
            );
        }
        write.path = destination;
        write.expected_hash = expected_hash;
        generated.entries.insert(
            relative,
            SnapshotEntry::File(encode_settings_bytecode(&write.document)?),
        );
    }
    Ok(generated)
}

pub(crate) fn amend_reconciled_changes(
    changes: &mut EditorChangeSet,
    mut plan: ReconcilePushPlan,
) -> Result<()> {
    if changes
        .instance_changes
        .iter()
        .any(|change| change.mode == "reconcileService")
    {
        bail!("Reconciliation cannot replace an entire Studio service");
    }
    plan.instance_deletes.append(&mut changes.instance_changes);
    changes.instance_changes = plan.instance_deletes;
    for change in &mut changes.instance_changes {
        for instance in &mut change.instances {
            if plan
                .in_place_instances
                .contains(&(change.service.clone(), instance.settings_id.clone()))
            {
                // This identity, class, name and parent already matched the
                // captured target. Keep its lookup anchor without an upsert.
                instance.anchor_only = true;
            }
            if let Some(previous) = plan
                .previous_class_names
                .get(&(change.service.clone(), instance.settings_id.clone()))
            {
                instance.previous_class_name = Some(previous.clone());
            }
            if let Some(previous) = plan
                .previous_paths
                .get(&(change.service.clone(), instance.settings_id.clone()))
            {
                instance.previous_path_segments = previous.path_segments.clone();
                instance.previous_path_ordinals = previous.path_ordinals.clone();
            }
        }
    }
    move_escaping_instances_before_deletes(&mut changes.instance_changes);
    for removal in plan.property_removals {
        if let Some(change) = changes.property_changes.iter_mut().find(|change| {
            change.service == removal.service
                && change.settings_id == removal.settings_id
                && change.path_segments == removal.path_segments
                && change.path_ordinals == removal.path_ordinals
        }) {
            change.reset_properties.extend(removal.reset_properties);
            change.deleted_attributes.extend(removal.deleted_attributes);
            change.reset_properties.sort();
            change.reset_properties.dedup();
            change.deleted_attributes.sort();
            change.deleted_attributes.dedup();
        } else {
            changes.property_changes.push(removal);
        }
    }
    for change in &mut changes.property_changes {
        if change.settings_id.as_ref().is_some_and(|id| {
            plan.attribute_only_instances
                .contains(&(change.service.clone(), id.clone()))
        }) {
            // These values already matched the captured Studio instance. An
            // attribute edit must not replay every property setter on that object.
            change.properties.clear();
            change.reset_properties.clear();
        }
    }
    for change in &mut changes.property_changes {
        if let Some(id) = &change.settings_id
            && let Some(names) = plan
                .unchanged_native_root_properties
                .get(&(change.service.clone(), id.clone()))
        {
            for name in names {
                change.properties.remove(name);
            }
        }
    }
    for change in &changes.property_changes {
        if let Some(id) = &change.settings_id
            && let Some(properties) = plan
                .geometry_properties
                .remove(&(change.service.clone(), id.clone()))
        {
            changes
                .geometry_readbacks
                .push(crate::editor::native_geometry::GeometryReadback {
                    service: change.service.clone(),
                    settings_id: id.clone(),
                    path_segments: change.path_segments.clone(),
                    path_ordinals: change.path_ordinals.clone(),
                    properties,
                });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn reconciliation_push_plan(
    studio: &ProjectSnapshot,
    merged: &ProjectSnapshot,
) -> Result<ReconcilePushPlan> {
    let mut paths = studio
        .entries
        .keys()
        .chain(merged.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();

    let paths = paths.into_iter().collect::<HashSet<_>>();
    reconciliation_push_plan_for_paths(studio, merged, &paths)
}

pub(crate) fn reconciliation_push_plan_for_paths(
    studio: &ProjectSnapshot,
    merged: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
) -> Result<ReconcilePushPlan> {
    reconciliation_push_plan_for_paths_with_prepared_settings(
        studio,
        merged,
        paths,
        &HashMap::new(),
        false,
        false,
    )
}

pub(crate) fn reconciliation_push_plan_for_paths_with_prepared_settings(
    studio: &ProjectSnapshot,
    merged: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared_settings: &HashMap<PathBuf, PreparedEditorSettingsChange>,
    bootstrap: bool,
    replace: bool,
) -> Result<ReconcilePushPlan> {
    let mut paths = paths.iter().cloned().collect::<Vec<_>>();
    paths.sort();

    // Select full-service imports before constructing individual insertion and
    // recreated-reference records that the import would immediately discard.
    let mut plan = ReconcilePushPlan {
        initial_native_services: if bootstrap {
            prepared_settings
                .iter()
                .filter(|(_, change)| {
                    !change.current.instances.is_empty()
                        && (replace || service_has_only_native_containers(&change.previous))
                })
                .map(|(path, change)| {
                    settings_service_name(path, &change.current, &change.previous)
                })
                .collect::<Result<_>>()?
        } else {
            HashSet::new()
        },
        ..Default::default()
    };
    for path in paths {
        let studio_entry = studio.entries.get(&path);
        let merged_entry = merged.entries.get(&path);
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            if let Some(prepared) = prepared_settings.get(&path) {
                let targets_before = plan.target_settings_ids.len();
                let phase = Instant::now();
                append_aligned_settings_push_plan(
                    &path,
                    &prepared.current,
                    &prepared.previous,
                    &mut plan,
                )?;
                log_reconcile_timing("incremental settings delta", phase);
                if plan.target_settings_ids.len() > targets_before {
                    plan.changed_paths.push(path);
                }
                continue;
            }
            let desired = settings_document(merged_entry)?;
            let observed = settings_document(studio_entry)?;
            if settings_documents_equivalent(&desired, &observed) {
                continue;
            }
            let targets_before = plan.target_settings_ids.len();
            append_settings_push_plan(&path, &desired, &observed, &mut plan)?;
            if plan.target_settings_ids.len() > targets_before {
                plan.changed_paths.push(path);
            }
        } else if !matches!(merged_entry, Some(SnapshotEntry::Directory))
            && (replace || !entries_equivalent(&path, studio_entry, merged_entry))
        {
            plan.changed_paths.push(path);
        }
    }
    append_recreated_reference_pushes(merged, prepared_settings, &mut plan)?;
    plan.changed_paths.sort();
    plan.changed_paths.dedup();
    plan.target_settings_ids.sort();
    plan.target_settings_ids.dedup();
    Ok(plan)
}

pub(crate) fn append_recreated_reference_pushes(
    desired: &ProjectSnapshot,
    prepared_settings: &HashMap<PathBuf, PreparedEditorSettingsChange>,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    let targeted = plan
        .target_settings_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut recreated = plan.recreated_settings_ids.clone();
    recreated.extend(
        plan.instance_deletes
            .iter()
            .flat_map(|change| &change.instances)
            .map(|instance| instance.settings_id.as_str())
            .filter(|settings_id| targeted.contains(settings_id))
            .map(str::to_string),
    );
    recreated.extend(
        plan.previous_class_names
            .keys()
            .map(|(_, settings_id)| settings_id.clone()),
    );
    if recreated.is_empty() {
        return Ok(());
    }

    for (path, entry) in &desired.entries {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            continue;
        }
        let decoded;
        let document = if let Some(prepared) = prepared_settings.get(path) {
            &prepared.current
        } else {
            decoded = settings_document(Some(entry))?;
            &decoded
        };
        // Full-service payloads already carry every outgoing reference, including
        // the serializer's post-apply exceptions. Only incremental services need
        // additional referrer selections when an identity is recreated elsewhere.
        if plan
            .initial_native_services
            .contains(&settings_service_name(path, document, document)?)
        {
            continue;
        }
        for instance in &document.instances {
            if instance.class_name == "PackageLink"
                || !instance
                    .properties
                    .values()
                    .chain(instance.attributes.values())
                    .any(|value| value_references_any(value, &recreated))
            {
                continue;
            }
            plan.changed_paths.push(path.clone());
            plan.target_settings_ids.push(instance.settings_id.clone());
            plan.attribute_only_instances.remove(&(
                settings_service_name(path, document, document)?,
                instance.settings_id.clone(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn value_references_any(value: &Value, settings_ids: &HashSet<String>) -> bool {
    match value {
        Value::Array(values) => values
            .iter()
            .any(|value| value_references_any(value, settings_ids)),
        Value::Object(object) => {
            let direct = object.get("_type").and_then(Value::as_str) == Some("Ref")
                && object
                    .get("settingsId")
                    .or_else(|| object.get("instanceId"))
                    .and_then(Value::as_str)
                    .is_some_and(|settings_id| settings_ids.contains(settings_id));
            direct
                || object
                    .values()
                    .any(|value| value_references_any(value, settings_ids))
        }
        _ => false,
    }
}

pub(crate) fn append_settings_push_plan(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    let mut observed = observed.clone();
    let service = settings_service_name(path, desired, &observed)?;
    if !align_settings_ids_to_reference(desired, &mut observed) {
        bail!("Could not align Studio identities in {service}; Studio was not changed");
    }
    align_equivalent_values(desired, &mut observed);
    append_aligned_settings_push_plan(path, desired, &observed, plan)
}

pub(crate) fn settings_service_name(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> Result<String> {
    desired
        .instances
        .iter()
        .chain(&observed.instances)
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .with_context(|| format!("{} has no service root", path.display()))
}

pub(crate) fn append_aligned_settings_push_plan(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    let service = settings_service_name(path, desired, observed)?;
    let bootstrap = plan.initial_native_services.contains(&service);
    if bootstrap
        && let Some(root) = desired
            .instances
            .iter()
            .find(|instance| instance.parent_index.is_none())
    {
        // Keep collection explicitly targeted even when no retained field differs.
        // Newly inserted descendants are already covered by the binary import.
        plan.target_settings_ids.push(root.settings_id.clone());
    }
    let database = rbx_reflection_database::get()?;
    let (desired_by_id, observed_by_id) = rayon::join(
        || {
            desired
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.as_str(), index))
                .collect::<HashMap<_, _>>()
        },
        || {
            observed
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.as_str(), index))
                .collect::<HashMap<_, _>>()
        },
    );
    let mut pending_property_removals = Vec::new();

    struct InstanceDelta {
        desired_index: usize,
        observed_index: Option<usize>,
        reset_properties: Vec<String>,
        deleted_attributes: Vec<String>,
    }

    let instance_deltas = desired
        .instances
        .par_iter()
        .enumerate()
        .filter_map(|(desired_index, instance)| {
            if instance.class_name == "PackageLink" {
                return None;
            }
            let observed_index = observed_by_id.get(instance.settings_id.as_str()).copied();
            if observed_index.is_some_and(|observed_index| {
                is_reconciliation_protected_workspace_camera(observed, observed_index)
                    || settings_instances_equal(desired, desired_index, observed, observed_index)
            }) {
                return None;
            }
            let Some(observed_index) = observed_index else {
                if bootstrap {
                    return None;
                }
                return Some(InstanceDelta {
                    desired_index,
                    observed_index: None,
                    reset_properties: Vec::new(),
                    deleted_attributes: Vec::new(),
                });
            };
            let observed_instance = &observed.instances[observed_index];
            let (reset_properties, deleted_attributes) =
                if observed_instance.class_name == instance.class_name {
                    (
                        observed_instance
                            .properties
                            .keys()
                            .filter(|name| {
                                name.as_str() != "ScriptGuid"
                                    && instance.parent_index.is_some()
                                    && crate::rbx::decode::property_has_serialized_form(
                                        database,
                                        &instance.class_name,
                                        name,
                                    )
                                    && !reconciliation_property_is_derived(name)
                                    // Resets obey the same capability rules as writes.
                                    // Engine-derived fields (for example cooked mesh
                                    // data after switching to Box) still participate
                                    // in the complete post-push verification.
                                    && !crate::editor::review::is_engine_managed_editor_property(
                                        &instance.class_name, name, database,
                                    )
                                    && !instance.properties.contains_key(*name)
                            })
                            .cloned()
                            .collect(),
                        observed_instance
                            .attributes
                            .keys()
                            .filter(|name| !instance.attributes.contains_key(*name))
                            .cloned()
                            .collect(),
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
            Some(InstanceDelta {
                desired_index,
                observed_index: Some(observed_index),
                reset_properties,
                deleted_attributes,
            })
        })
        .collect::<Vec<_>>();
    // The aligned identity can have a different ordinal, even when its own
    // name and parent identity are unchanged. Preserve the observed paths for
    // exactly the upsert selection, including lookup-only ancestors/siblings.
    let previous_indices = if instance_deltas.is_empty() {
        Vec::new()
    } else {
        let filter = crate::editor::types::EditorPropertyFilter {
            settings_ids: instance_deltas
                .iter()
                .map(|delta| desired.instances[delta.desired_index].settings_id.clone())
                .collect(),
            ..Default::default()
        };
        let (_, mut selected) = crate::editor::diff::editor_target_indices(desired, &filter);
        crate::editor::diff::expand_ambiguous_editor_siblings(desired, &mut selected);
        selected
            .iter()
            .filter_map(|index| {
                observed_by_id
                    .get(desired.instances[*index].settings_id.as_str())
                    .copied()
            })
            .collect()
    };
    for delta in instance_deltas {
        let instance = &desired.instances[delta.desired_index];
        plan.target_settings_ids.push(instance.settings_id.clone());
        let Some(observed_index) = delta.observed_index else {
            plan.recreated_settings_ids
                .insert(instance.settings_id.clone());
            continue;
        };
        let observed_instance = &observed.instances[observed_index];
        if !bootstrap
            && instance.name == observed_instance.name
            && instance.class_name == observed_instance.class_name
            && settings_parent_id(desired, delta.desired_index)
                == settings_parent_id(observed, observed_index)
        {
            plan.in_place_instances
                .insert((service.clone(), instance.settings_id.clone()));
        }
        if plan
            .in_place_instances
            .contains(&(service.clone(), instance.settings_id.clone()))
            && reconciliation_maps_equal(
                &instance.class_name,
                &instance.properties,
                &observed_instance.properties,
            )
        {
            plan.attribute_only_instances
                .insert((service.clone(), instance.settings_id.clone()));
        } else if plan
            .in_place_instances
            .contains(&(service.clone(), instance.settings_id.clone()))
        {
            let unchanged = instance
                .properties
                .iter()
                .filter(|(name, value)| {
                    crate::editor::native_roots::is_property(&instance.class_name, name)
                        && reconciliation_property_values_equal(
                            &instance.class_name,
                            name,
                            Some(value),
                            observed_instance.properties.get(name.as_str()),
                        )
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            if !unchanged.is_empty() {
                plan.unchanged_native_root_properties
                    .insert((service.clone(), instance.settings_id.clone()), unchanged);
            }
        }
        let geometry =
            crate::editor::native_geometry::generated_properties(observed_instance, instance);
        if !geometry.is_empty() {
            plan.geometry_properties
                .insert((service.clone(), instance.settings_id.clone()), geometry);
        }
        if observed_instance.class_name != instance.class_name {
            plan.previous_class_names.insert(
                (service.clone(), instance.settings_id.clone()),
                observed_instance.class_name.clone(),
            );
        } else if !delta.reset_properties.is_empty() || !delta.deleted_attributes.is_empty() {
            pending_property_removals.push((
                delta.desired_index,
                delta.reset_properties,
                delta.deleted_attributes,
            ));
        }
    }

    if !previous_indices.is_empty() {
        let previous_paths =
            build_editor_instance_paths_for_indices(observed, &service, &previous_indices);
        for index in previous_indices {
            if let Some(path) = previous_paths.get(&index).cloned() {
                plan.previous_paths.insert(
                    (
                        service.clone(),
                        observed.instances[index].settings_id.clone(),
                    ),
                    path,
                );
            }
        }
    }

    if !pending_property_removals.is_empty() {
        let removal_indices = pending_property_removals
            .iter()
            .map(|(index, _, _)| *index)
            .collect::<Vec<_>>();
        let desired_paths =
            build_editor_instance_paths_for_indices(desired, &service, &removal_indices);
        for (desired_index, reset_properties, deleted_attributes) in pending_property_removals {
            let instance = &desired.instances[desired_index];
            let path_info = desired_paths
                .get(&desired_index)
                .cloned()
                .with_context(|| format!("Could not locate {} in {}", instance.name, service))?;
            plan.property_removals.push(EditorPropertyChange {
                service: service.clone(),
                settings_id: Some(instance.settings_id.clone()),
                path_segments: path_info.path_segments,
                path_ordinals: path_info.path_ordinals,
                class_name: instance.class_name.clone(),
                properties: Map::new(),
                reset_properties,
                attributes: Map::new(),
                deleted_attributes,
                attributes_complete: false,
            });
        }
    }

    let removed = observed
        .instances
        .iter()
        .enumerate()
        .filter(|(index, instance)| {
            instance.parent_index.is_some()
                && !desired_by_id.contains_key(instance.settings_id.as_str())
                && !is_reconciliation_protected_workspace_camera(observed, *index)
                && !is_protected_engine_container(observed, *index)
        })
        .map(|(index, _)| index)
        .collect::<HashSet<_>>();
    if removed.is_empty() {
        return Ok(());
    }
    let root_removals = removed
        .iter()
        .copied()
        .filter(|index| {
            observed.instances[*index]
                .parent_index
                .is_none_or(|parent| !removed.contains(&parent))
        })
        .collect::<Vec<_>>();
    if root_removals.is_empty() {
        return Ok(());
    }
    let observed_paths =
        build_editor_instance_paths_for_indices(observed, &service, &root_removals);
    let mut descriptors = Vec::with_capacity(root_removals.len());
    for index in root_removals {
        if observed.instances[index].class_name == "PackageLink" {
            bail!(
                "Reconciliation would remove PackageLink {} directly; Studio was not changed",
                observed.instances[index].name
            );
        }
        let path_info = observed_paths.get(&index).cloned().with_context(|| {
            format!(
                "Could not locate removed instance {} in {}",
                observed.instances[index].name, service
            )
        })?;
        descriptors.push(
            editor_instance_descriptor_for_known_path(
                observed,
                index,
                path_info.path_segments,
                path_info.path_ordinals,
            )
            .context("Failed to describe a reconciled instance removal")?,
        );
    }
    plan.instance_deletes.push(EditorInstanceChange {
        mode: "deleteInstances".to_string(),
        service,
        allow_deletes: false,
        instances: descriptors,
        preserve_instances: Vec::new(),
    });
    Ok(())
}

/// A delete detaches the whole subtree, so an instance moving out of a
/// deleted subtree must move first or Studio can no longer identify it.
pub(crate) fn move_escaping_instances_before_deletes(
    instance_changes: &mut Vec<EditorInstanceChange>,
) {
    let deleted = instance_changes
        .iter()
        .filter(|change| change.mode == "deleteInstances")
        .flat_map(|change| {
            change
                .instances
                .iter()
                .map(move |instance| (change.service.as_str(), instance.path_segments.as_slice()))
        })
        .map(|(service, path)| (service.to_string(), path.to_vec()))
        .collect::<Vec<_>>();
    if deleted.is_empty() {
        return;
    }
    let mut escapes = Vec::new();
    for change in instance_changes.iter_mut() {
        if change.mode != "upsertInstances" {
            continue;
        }
        let leaving = change
            .instances
            .iter()
            .filter(|instance| {
                !instance.previous_path_segments.is_empty()
                    && deleted.iter().any(|(service, path)| {
                        *service == change.service
                            && instance.previous_path_segments.len() > path.len()
                            && instance.previous_path_segments.starts_with(path)
                    })
            })
            .map(|instance| instance.settings_id.clone())
            .collect::<HashSet<_>>();
        if leaving.is_empty() {
            continue;
        }
        let ancestors = change
            .instances
            .iter()
            .filter(|instance| leaving.contains(&instance.settings_id))
            .flat_map(|instance| {
                change.instances.iter().filter(|candidate| {
                    candidate.path_segments.len() < instance.path_segments.len()
                        && instance.path_segments.starts_with(&candidate.path_segments)
                })
            })
            .map(|ancestor| ancestor.settings_id.clone())
            .collect::<HashSet<_>>();
        let (escaping, staying): (Vec<_>, Vec<_>) =
            change.instances.drain(..).partition(|instance| {
                leaving.contains(&instance.settings_id) || ancestors.contains(&instance.settings_id)
            });
        change.instances = staying;
        escapes.push(EditorInstanceChange {
            mode: "upsertInstances".to_string(),
            service: change.service.clone(),
            allow_deletes: false,
            instances: escaping,
            preserve_instances: Vec::new(),
        });
    }
    if escapes.is_empty() {
        return;
    }
    instance_changes
        .retain(|change| !change.instances.is_empty() || !change.preserve_instances.is_empty());
    escapes.append(instance_changes);
    *instance_changes = escapes;
}

pub(crate) fn capture_snapshot(root: &Path, roots: &[PathBuf]) -> Result<ProjectSnapshot> {
    let mut entries = BTreeMap::new();
    for relative_root in roots {
        if derived_project_path(relative_root) {
            continue;
        }
        let path = root.join(relative_root);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            entries.insert(
                relative_root.clone(),
                SnapshotEntry::Symlink {
                    target: fs::read_link(&path)?,
                    directory: fs::metadata(&path).is_ok_and(|target| target.is_dir()),
                },
            );
            continue;
        }
        if metadata.is_file() {
            entries.insert(relative_root.clone(), SnapshotEntry::File(fs::read(&path)?));
            continue;
        }
        if !metadata.is_dir() {
            bail!("Unsupported project entry {}", path.display());
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(root)?.to_path_buf();
            if derived_project_path(&relative) {
                continue;
            }
            let value = if entry.file_type().is_dir() {
                SnapshotEntry::Directory
            } else if entry.file_type().is_file() {
                SnapshotEntry::File(fs::read(entry.path())?)
            } else if entry.file_type().is_symlink() {
                SnapshotEntry::Symlink {
                    target: fs::read_link(entry.path())?,
                    directory: fs::metadata(entry.path()).is_ok_and(|target| target.is_dir()),
                }
            } else {
                continue;
            };
            entries.insert(relative, value);
        }
    }
    Ok(ProjectSnapshot { entries })
}
