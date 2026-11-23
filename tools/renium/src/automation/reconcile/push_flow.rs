use super::*;

/// A push the Studio review held (`heldForReview`) or skipped applied no
/// change, however many it listed.
fn push_applied_nothing(summary: &Map<String, Value>) -> bool {
    ["heldForReview", "skippedByReview"]
        .iter()
        .any(|key| summary.get(*key) == Some(&Value::Bool(true)))
}

/// Whether a push created, replaced or removed an instance: Studio then holds
/// identities the files do not, and the stores no longer stand row for row.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn push_changed_structure(summary: &Map<String, Value>) -> bool {
    [
        "instanceCreated",
        "instanceReplaced",
        "instanceDeleted",
        "sourceCreated",
        "sourceDeleted",
    ]
    .iter()
    .any(|key| {
        summary
            .get(*key)
            .and_then(Value::as_f64)
            .is_some_and(|count| count > 0.0)
    })
}

/// Hands the previous push's native attribute observer over to the fresh
/// lease: the old one stops only once the proof check inside the lease has
/// passed, and the new relay starts from the path the state names.
#[cfg(windows)]
pub(super) fn arm_native_attribute_relay(
    state: &Value,
    bridge: &BridgeServer,
    services: &[String],
    previous: &mut Option<NativeRelay>,
    current: &mut Option<NativeRelay>,
) -> Result<()> {
    if state["nativeRelayContinued"] == true {
        return Ok(());
    }
    drop(previous.take());
    let Some(path) = state
        .get("nativeAttributeRelay")
        .filter(|value| !value.is_null())
    else {
        return Ok(());
    };
    let path = serde_json::from_value::<Vec<String>>(path.clone())?;
    let pid = crate::editor::review::studio_pid_for_bridge(bridge)?;
    let title = crate::editor::review::studio_title_for_bridge(bridge, pid)?;
    let armed = Instant::now();
    match crate::studio::native::serializer::begin_attribute_relay(
        pid,
        &title,
        services,
        NATIVE_RELAY_LIFETIME,
        &path,
    ) {
        Ok(observation) => *current = Some(NativeRelay::armed(observation, armed)),
        Err(error) => log_global(
            5,
            format_args!("[renium] native attribute relay unavailable: {error:#}"),
        ),
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
enum VerifiedFullPushReuse {
    Unchanged(Map<String, Value>),
    Continue(Option<VerifiedFullPushCandidate>),
}

/// A full push whose files match the last verified one answers "unchanged"
/// when Studio still carries that push's proof; otherwise the candidate is
/// handed on so the new push can continue its native observers.
#[cfg(any(windows, target_os = "macos"))]
fn reuse_verified_full_push(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: &PushEditorChangesArgs,
    eligible: bool,
) -> Result<VerifiedFullPushReuse> {
    let candidate = if eligible {
        take_verified_full_push(context, bridge, services, push_args)?
    } else {
        None
    };
    if candidate
        .as_ref()
        .is_some_and(|candidate| candidate.files_unchanged)
        && verified_full_push_proof_matches(
            context,
            bridge,
            &candidate.as_ref().expect("checked above").cached.proof,
        )?
    {
        let mut candidate = candidate.expect("checked above");
        log_global(5, format_args!("[renium] baseline: full push unchanged"));
        if let Some(runtime) = context.runtime_id.as_deref() {
            candidate.cached.renew();
            bridge
                .verified_full_pushes
                .lock_recover()
                .insert(runtime.to_string(), candidate.cached);
        }
        return Ok(VerifiedFullPushReuse::Unchanged(Map::from_iter([
            ("ok".into(), json!(true)),
            ("unchanged".into(), json!(true)),
        ])));
    }
    Ok(VerifiedFullPushReuse::Continue(candidate))
}

/// How a push reads its stores. Where no package is enforced and nothing
/// but stores and script files differs between the project and Studio, it
/// reads each store by its changed instances, and its plan says whether that
/// holds (`plan_reads_changed_alone`). A script is found among them by its
/// name and its place, which every instance keeps, and a script file that
/// adds an instance to its store or takes one away reads that store whole.
pub(super) fn stores_read_of(
    unenforced: bool,
    paths: &HashSet<PathBuf>,
    project: &ProjectSnapshot,
    studio: &ProjectSnapshot,
) -> StoresRead {
    let only_stores_differ = paths.iter().all(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
            || super::merge::is_source_path(path)
            || project.entries.get(path) == studio.entries.get(path)
    });
    if unenforced && only_stores_differ {
        StoresRead::ChangedOfBoth
    } else {
        StoresRead::Changed
    }
}

/// How a push that replaces nothing reads its stores to compare them: by
/// their changed instances under the conditions of a push that replaces
/// (`stores_read_of`), where it takes every instance of a store as the files
/// hold it.
fn stores_compared_of(
    unenforced: bool,
    (project, studio): (&ProjectSnapshot, &ProjectSnapshot),
    args: &crate::cli::PushEditorChangesArgs,
) -> Result<StoresRead> {
    let paths = project_replacement_paths(project, studio);
    Ok(match stores_read_of(unenforced, &paths, project, studio) {
        StoresRead::ChangedOfBoth if crate::editor::sync::project_pushes_stores_as_held(args)? => {
            StoresRead::ChangedOfBoth
        }
        _ => StoresRead::Changed,
    })
}

/// The import carried the instances these documents left in their store
/// files to Studio, and what verifies it reads those files. A document read
/// by its changed instances beside Studio's copy read the same way stays: it
/// is the proof that every other instance is the one Studio held.
fn leave_unread_documents(prepared: &mut HashMap<PathBuf, PreparedPushVerification>) {
    use crate::settings::equivalence::holds_unread_instances;
    for verified in prepared.values_mut() {
        if verified
            .desired
            .as_deref()
            .is_some_and(holds_unread_instances)
            && !holds_unread_instances(&verified.previous)
        {
            verified.desired = None;
        }
    }
}

/// The plan as it is carried out: one made from the changed instances of
/// both copies that the instances that differ do not carry out
/// (`plan_reads_changed_alone`) is made again from the whole copies of its
/// stores: of each of them, or for a move between stores of the ones where
/// an unread instance refers to what moved. `replace` says which comparison
/// prepared them.
pub(super) fn plan_from_whole_stores(
    (stores_read, replace, plan): (StoresRead, bool, ReconcilePushPlan),
    (project, studio): (&ProjectSnapshot, &ProjectSnapshot),
    differences: &HashSet<PathBuf>,
    prepared_settings: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<ReconcilePushPlan> {
    if stores_read != StoresRead::ChangedOfBoth || plan_reads_changed_alone(&plan) {
        return Ok(plan);
    }
    let moved = super::replacement::moved_between_stores(&plan);
    let mut read_whole = Vec::new();
    for (path, change) in prepared_settings.iter() {
        let unread = crate::settings::equivalence::holds_unread_instances(&change.current);
        let whole = match &moved {
            Some(ids) => {
                unread
                    && super::replacement::unread_instance_refers(
                        project.entries.get(path),
                        &change.current,
                        ids,
                    )?
            }
            None if replace => !service_has_only_native_containers(&change.previous),
            None => unread,
        };
        if whole {
            read_whole.push(path.clone());
        }
    }
    if read_whole.is_empty() {
        return Ok(plan);
    }
    if !replace {
        for path in read_whole {
            let mut whole = None;
            let equivalent = super::verify::snapshot_entry_equivalent(
                &path,
                project.entries.get(&path),
                studio.entries.get(&path),
                Some(&mut whole),
            )?;
            let Some(whole) = whole.filter(|_| !equivalent) else {
                bail!("The stores of this push differ another way when read whole; retry the push");
            };
            prepared_settings.insert(path, whole);
        }
        return reconciliation_push_plan_for_paths_with_prepared_settings(
            studio,
            project,
            differences,
            prepared_settings,
            true,
            false,
        );
    }
    // A store that is imported is prepared the same way either time.
    let paths = read_whole.into_iter().collect::<HashSet<_>>();
    let mut whole = HashMap::new();
    prepare_project_replacement_from(project, studio, &paths, &mut whole, StoresRead::Changed)?;
    anyhow::ensure!(
        whole.len() == paths.len(),
        "The stores of this push differ another way when read whole; retry the push"
    );
    prepared_settings.extend(whole);
    reconciliation_push_plan_for_paths_with_prepared_settings(
        studio,
        project,
        differences,
        prepared_settings,
        true,
        false,
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn scoped_push_capture_services(
    context: &BoundContext,
    services: &[String],
    eligible: bool,
    scoped_baseline: Option<&ProjectSnapshot>,
    initial_state: &Value,
) -> Option<Vec<String>> {
    let held_project = scoped_baseline.filter(|_| eligible)?;
    let source = Path::new(&context.source);
    let stores = crate::project::structural::service_store_paths(source)
        .ok()?
        .into_iter()
        .map(|(service, path)| {
            Some((
                service,
                path.strip_prefix(&context.root).ok()?.to_path_buf(),
            ))
        })
        .collect::<Option<BTreeMap<_, _>>>()?;
    let service_set: HashSet<&str> = services.iter().map(String::as_str).collect();
    match super::coordinator::moved_services(
        &ProofScope::parse(&initial_state["pushProofScope"]),
        held_project,
        &stores,
    ) {
        Ok(Ok(moved)) => {
            let moved: Vec<String> = moved
                .into_iter()
                .filter(|service| service_set.contains(service.as_str()))
                .collect();
            (!moved.is_empty() && moved.len() < services.len()).then_some(moved)
        }
        Ok(Err(reason)) => {
            log_global(
                5,
                format_args!("[renium] scoped push capture unavailable: {reason}"),
            );
            None
        }
        Err(error) => {
            log_global(
                5,
                format_args!("[renium] scoped push capture error: {error:#}"),
            );
            None
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn scoped_push_capture_services(
    _context: &BoundContext,
    _services: &[String],
    _eligible: bool,
    _scoped_baseline: Option<&ProjectSnapshot>,
    _initial_state: &Value,
) -> Option<Vec<String>> {
    None
}

fn capture_scoped_push_baseline(
    context: &BoundContext,
    bridge: &BridgeServer,
    root: &Path,
    moved: &[String],
    stage: &ExportProjectStage,
    mut baseline: ProjectSnapshot,
) -> Result<ProjectSnapshot> {
    log_global(
        5,
        format_args!(
            "[renium] scoped push capture: services=[{}]",
            moved.join(", ")
        ),
    );
    let captured_moved =
        capture_studio_services_in_memory_options(context, bridge, moved, stage, false, true)?;
    let source = Path::new(&context.source)
        .strip_prefix(root)
        .context("Project source is outside its root")?;
    let mut scopes = Vec::with_capacity(moved.len() * 2);
    for service in moved {
        scopes.push(source.join(service));
        scopes.push(
            service_settings_path(&Path::new(&context.source).join(service))
                .strip_prefix(root)
                .context("Scoped push capture store is outside its project root")?
                .to_path_buf(),
        );
    }
    let scope_set = ScopeSet::new(&scopes);
    baseline.entries.retain(|path, _| !scope_set.contains(path));
    baseline.entries.extend(captured_moved.entries);
    Ok(baseline)
}

fn collect_delta_bulk_differences(
    project: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    stores_read: StoresRead,
    prepared_settings: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
    bulk_patches: &mut HashMap<PathBuf, crate::editor::types::EditorBulkPropertyPatch>,
) -> Result<HashSet<PathBuf>> {
    use crate::settings::bytecode::blocks::bulk_diff::{
        BulkDiffOutcome, diff_v12_bulk_plain_properties,
    };
    let paths = project_replacement_paths(project, studio);
    let mut non_bulk_project = project.clone();
    let mut non_bulk_studio = studio.clone();
    for path in &paths {
        let is_store = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name);
        if !is_store || project.entries.get(path) == studio.entries.get(path) {
            continue;
        }
        let (Some(SnapshotEntry::File(desired_bytes)), Some(SnapshotEntry::File(observed_bytes))) =
            (project.entries.get(path), studio.entries.get(path))
        else {
            continue;
        };
        let Some(service_name) = super::replacement::settings_path_service_name(path) else {
            continue;
        };
        match diff_v12_bulk_plain_properties(
            desired_bytes,
            observed_bytes,
            service_name,
            super::replacement::BULK_PROPERTY_MIN_INSTANCES,
        )? {
            BulkDiffOutcome::Unchanged => {
                non_bulk_project.entries.remove(path);
                non_bulk_studio.entries.remove(path);
            }
            BulkDiffOutcome::Patch(patch) => {
                bulk_patches.insert(path.clone(), patch);
                non_bulk_project.entries.remove(path);
                non_bulk_studio.entries.remove(path);
            }
            BulkDiffOutcome::Fallback => {}
        }
    }
    let mut diffs = snapshot_differences_prepared(
        &non_bulk_project,
        &non_bulk_studio,
        Some(prepared_settings),
        stores_read == StoresRead::ChangedOfBoth,
    )?;
    diffs.extend(bulk_patches.keys().cloned());
    Ok(diffs)
}

fn bulk_inline_release_tracking(
    plan: &ReconcilePushPlan,
    services: &[String],
    guard: &StudioChangeGuard,
    tracking_release: &StudioTrackingGuardRelease<'_>,
    requires_stage: bool,
    has_link_enforcement: bool,
) -> Option<Value> {
    if !plan.changed_paths.is_empty()
        || !plan.target_settings_ids.is_empty()
        || !plan.initial_native_services.is_empty()
        || plan.bulk_property_patches.is_empty()
        || requires_stage
        || has_link_enforcement
        || tracking_release.guard_id.is_none()
    {
        return None;
    }
    let retain = tracking_release
        .retain_until
        .map_or(BASELINE_IDLE, |until| {
            until.saturating_duration_since(Instant::now())
        });
    Some(json!({
        "services": services,
        "ackSeq": guard.change_seq,
        "runtimeId": guard.runtime_id,
        "start": false,
        "includeGenerations": true,
        "releaseTrackingGuardId": tracking_release.guard_id,
        "continuesPushProof": tracking_release.continues,
        "retainSeconds": retain.as_secs(),
    }))
}

#[cfg(any(windows, target_os = "macos"))]
struct ZeroCaptureContext<'a> {
    context: &'a BoundContext,
    bridge: &'a BridgeServer,
    services: &'a [String],
    root: &'a Path,
    src_dir: &'a Path,
    source_paths: &'a [PathBuf],
    stage: &'a ExportProjectStage,
    guard: &'a StudioChangeGuard,
    push_args: &'a PushEditorChangesArgs,
    initial_state: &'a Value,
    cache_input: Option<&'a Value>,
}

#[cfg(any(windows, target_os = "macos"))]
struct ExtractedZeroCapture {
    plan: ReconcilePushPlan,
    service_rows: HashMap<String, (PathBuf, Vec<usize>)>,
}

#[cfg(any(windows, target_os = "macos"))]
fn plan_zero_capture_bulk_restore(
    zctx: &ZeroCaptureContext<'_>,
    held_project: &ProjectSnapshot,
) -> Result<Option<ExtractedZeroCapture>> {
    use crate::settings::bytecode::blocks::bulk_diff::{
        BulkExtractOutcome, extract_v12_bulk_properties,
    };
    let Some(scope) = ProofBulkScope::parse(&zctx.initial_state["pushProofScope"]) else {
        return Ok(None);
    };
    if scope.services.is_empty() {
        return Ok(None);
    }
    let service_set: HashSet<&str> = zctx.services.iter().map(String::as_str).collect();
    if scope.services.iter().any(|svc| {
        !service_set.contains(svc.as_str())
            || !scope.values_only.contains(svc)
            || !scope.changed_properties.contains_key(svc)
    }) {
        return Ok(None);
    }
    if scope.services.contains("Workspace") && scope.expected_terrain_digest.is_none() {
        return Ok(None);
    }
    let stores = crate::project::structural::service_store_paths(Path::new(&zctx.context.source))?;
    let mut plan = ReconcilePushPlan::default();
    let mut service_rows = HashMap::new();
    for service in &scope.services {
        let Some(store_abs) = stores.get(service) else {
            return Ok(None);
        };
        let Ok(store_rel) = store_abs.strip_prefix(zctx.root) else {
            return Ok(None);
        };
        let Some(SnapshotEntry::File(desired_bytes)) = held_project.entries.get(store_rel) else {
            return Ok(None);
        };
        let props = &scope.changed_properties[service];
        let phase = Instant::now();
        let outcome = extract_v12_bulk_properties(
            desired_bytes,
            service,
            props,
            super::replacement::BULK_PROPERTY_MIN_INSTANCES,
        )?;
        let BulkExtractOutcome::Patch(mut extracted) = outcome else {
            return Ok(None);
        };
        log_reconcile_timing(
            &format!(
                "zero-capture bulk extract ({service}, {} instances, {} fields)",
                extracted.patch.instance_count, extracted.patch.field_count
            ),
            phase,
        );
        if service == "Workspace" {
            extracted.patch.expected_terrain_digest = scope.expected_terrain_digest.clone();
        }
        extracted.patch.return_old_values = true;
        plan.bulk_property_paths.push(store_rel.to_path_buf());
        plan.bulk_property_patches.push(extracted.patch);
        service_rows.insert(
            service.clone(),
            (store_rel.to_path_buf(), extracted.node_to_row),
        );
    }
    Ok(Some(ExtractedZeroCapture { plan, service_rows }))
}

#[cfg(any(windows, target_os = "macos"))]
fn record_zero_capture_history(
    zctx: &ZeroCaptureContext<'_>,
    held_project: &ProjectSnapshot,
    service_rows: &HashMap<String, (PathBuf, Vec<usize>)>,
    old_columns_map: &Map<String, Value>,
) -> Result<String> {
    use crate::settings::bytecode::blocks::bulk_diff::{
        decode_bulk_old_value_columns, patch_v12_bulk_old_values,
    };
    let phase = Instant::now();
    let mut before_entries = BTreeMap::new();
    let mut mutation_paths = HashSet::new();
    for (service, (store_rel, node_to_row)) in service_rows {
        let Some(SnapshotEntry::File(desired_bytes)) = held_project.entries.get(store_rel) else {
            continue;
        };
        let updates = old_columns_map
            .get(service)
            .and_then(Value::as_array)
            .map(|cols| decode_bulk_old_value_columns(cols, node_to_row))
            .transpose()?
            .unwrap_or_default();
        let before_bytes = patch_v12_bulk_old_values(desired_bytes, &updates)?;
        before_entries.insert(store_rel.clone(), SnapshotEntry::File(before_bytes));
        mutation_paths.insert(store_rel.clone());
    }
    log_reconcile_timing("zero-capture history reconstruct", phase);
    let before = ProjectSnapshot {
        entries: before_entries,
    };
    let mut history =
        history::SyncHistory::begin(zctx.root, zctx.src_dir, &before, &mutation_paths)?;
    history.commit(held_project, &ProjectSnapshot::default())
}

#[cfg(any(windows, target_os = "macos"))]
fn try_zero_capture_bulk_restore(
    zctx: &ZeroCaptureContext<'_>,
    held_project: &ProjectSnapshot,
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] native_relay: &mut Option<NativeRelay>,
) -> Result<Option<Map<String, Value>>> {
    if !(zctx.push_args.no_review || zctx.push_args.yes || crate::app::output::global_yes()) {
        return Ok(None);
    }
    let Some(extracted) = plan_zero_capture_bulk_restore(zctx, held_project)? else {
        return Ok(None);
    };
    let phase = Instant::now();
    let mut args = zctx.push_args.clone();
    args.inline_release_tracking = bulk_inline_release_tracking(
        &extracted.plan,
        zctx.services,
        zctx.guard,
        tracking_release,
        false,
        false,
    );
    let mut pushed = push_staged_project(
        zctx.context,
        zctx.stage,
        zctx.bridge,
        StagedPushRequest {
            plan: extracted.plan,
            prepared_documents: HashMap::new(),
            guard: Some(zctx.guard),
            args,
            expected_project: Some(held_project),
            later_edits_follow: false,
        },
    )?;
    if pushed.summary.get("fallbackFullCapture") == Some(&Value::Bool(true)) {
        log_global(
            5,
            format_args!(
                "[renium] zero-capture bulk restore fell back to full capture (Terrain digest mismatch)"
            ),
        );
        return Ok(None);
    }
    if let Some(release_result) = pushed.summary.remove("_inlineReleaseResult") {
        apply_release_tracking_result(zctx.guard, tracking_release, &release_result);
    }
    let old_columns = pushed
        .summary
        .remove("_bulkOldValueColumns")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let updated = pushed
        .summary
        .get("propertyUpdated")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let (current, history_id) = std::thread::scope(|scope| {
        let current = scope.spawn(|| capture_snapshot(zctx.root, zctx.stage.publish_paths()));
        let history_id = if updated > 0.0 {
            Some(record_zero_capture_history(
                zctx,
                held_project,
                &extracted.service_rows,
                &old_columns,
            ))
        } else {
            None
        };
        (
            current.join().expect("Post-push snapshot worker panicked"),
            history_id,
        )
    });
    if let Some(history_id) = history_id {
        pushed
            .summary
            .insert("historyId".into(), Value::String(history_id?));
    }
    log_reconcile_timing("full push mutation", phase);
    let current = current?;
    if *held_project == current {
        tracking_release.proof = pushed.summary.remove("verifiedPushProof");
    } else {
        tracking_release.retained_proof = None;
    }
    if tracking_release.guard_id.is_some() {
        acknowledge_verified_push(zctx.bridge, zctx.services, zctx.guard, tracking_release)?;
    }
    retain_verified_full_push(
        zctx.bridge,
        zctx.cache_input,
        zctx.source_paths,
        held_project.clone(),
        tracking_release,
        #[cfg(windows)]
        native_relay,
    );
    Ok(Some(pushed.summary))
}

fn prepare_replace_push_differences(
    context: &BoundContext,
    root: &Path,
    stage: &ExportProjectStage,
    (project, studio): (&ProjectSnapshot, &ProjectSnapshot),
    (bulk_eligible, unenforced): (bool, bool),
    prepared_settings: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
    bulk_patches: &mut HashMap<PathBuf, crate::editor::types::EditorBulkPropertyPatch>,
) -> Result<(HashSet<PathBuf>, HashSet<String>, StoresRead)> {
    let paths = project_replacement_paths(project, studio);
    let stores_read = stores_read_of(unenforced, &paths, project, studio);
    let imported = ImportedStores::default();
    let collected_bulk = BulkPropertyPatches::default();
    let imports_heavy = !root.join("renium-link.json").exists();
    let unchanged_settings = if bulk_eligible {
        prepare_project_replacement_with_bulk(
            project,
            studio,
            &paths,
            prepared_settings,
            stores_read,
            imports_heavy.then_some(&imported),
            Some(&collected_bulk),
        )?
    } else {
        let (prepared, staged) = rayon::join(
            || {
                prepare_project_replacement_importing(
                    project,
                    studio,
                    &paths,
                    prepared_settings,
                    stores_read,
                    imports_heavy.then_some(&imported),
                )
            },
            || stage_snapshot_paths(&stage.project_root, &paths, project),
        );
        staged?;
        prepared?
    };
    *bulk_patches = collected_bulk
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let native_services = prepared_settings
        .iter()
        .filter(|(_, change)| {
            !change.current.instances.is_empty()
                && service_has_only_native_containers(&change.previous)
                && !settings_documents_equivalent(&change.current, &change.previous)
        })
        .map(|(path, change)| settings_service_name(path, &change.current, &change.previous))
        .collect::<Result<HashSet<_>>>()?;
    let diff_paths = paths
        .iter()
        .filter(|path| {
            if bulk_patches.contains_key(*path) {
                return true;
            }
            if unchanged_settings.contains(*path) {
                return false;
            }
            if let Some(change) = prepared_settings.get(*path) {
                return !settings_documents_equivalent(&change.current, &change.previous);
            }
            let native = services_for_snapshot_paths(context, &HashSet::from([(*path).clone()]))
                .iter()
                .any(|service| native_services.contains(service));
            native
                || !entries_equivalent(path, studio.entries.get(*path), project.entries.get(*path))
        })
        .cloned()
        .collect::<HashSet<_>>();
    if bulk_eligible
        && diff_paths
            .iter()
            .any(|path| !bulk_patches.contains_key(path))
    {
        stage_snapshot_paths(&stage.project_root, &paths, project)?;
    }
    prepared_settings.retain(|path, _| diff_paths.contains(path));
    let promoted_services = imported_services(imported, prepared_settings)?;
    Ok((diff_paths, promoted_services, stores_read))
}

fn collect_saved_field_differences(
    context: &BoundContext,
    guard: &StudioChangeGuard,
    prepared_settings: &HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Option<Arc<crate::editor::types::PushSavedFields>> {
    let mut saved_fields = Vec::new();
    for field in prepared_settings.values().flat_map(|change| {
        crate::settings::equivalence::saved_field_differences(&change.current, &change.previous)
    }) {
        if !saved_fields.contains(&field) {
            saved_fields.push(field);
        }
    }
    (!saved_fields.is_empty()).then(|| {
        Arc::new(crate::editor::types::PushSavedFields {
            project: PathBuf::from(&context.root),
            runtime: guard.runtime_id.clone(),
            fields: saved_fields,
        })
    })
}

fn log_full_push_differences(differences: &HashSet<PathBuf>) {
    if !crate::app::output::global_log_enabled(5) {
        return;
    }
    let mut listed = differences
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    listed.sort();
    log_global(
        5,
        format_args!(
            "[renium] full push differences ({}): {}",
            listed.len(),
            listed.join(", ")
        ),
    );
}

fn log_full_push_plan(plan: &ReconcilePushPlan) {
    if !crate::app::output::global_log_enabled(5) {
        return;
    }
    log_global(
        5,
        format_args!(
            "[renium] full push plan: paths=[{}] targets=[{}] native=[{}]",
            plan.changed_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            plan.target_settings_ids.join(", "),
            plan.initial_native_services
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
}

#[cfg(any(windows, target_os = "macos"))]
fn pre_push_settlement_proof(
    bridge: &BridgeServer,
    guard: &StudioChangeGuard,
    held_pull: Option<(Box<SourcemapState>, Value)>,
    held_sourcemap_fallback: Option<Box<SourcemapState>>,
    eligible: bool,
) -> Option<(Box<SourcemapState>, Value)> {
    if held_pull.is_some() || !eligible {
        return held_pull;
    }
    let sourcemap = held_sourcemap_fallback?;
    let pre_push_proof = capture_push_proof_for_settlement(bridge, guard).ok()??;
    Some((sourcemap, pre_push_proof))
}

#[cfg(any(windows, target_os = "macos"))]
fn collect_prior_differing_services(
    context: &BoundContext,
    scoped_moved: Option<Vec<String>>,
    prior_pull_project: Option<ProjectSnapshot>,
    has_held_pull: bool,
    studio: &ProjectSnapshot,
) -> Vec<String> {
    if let Some(moved) = scoped_moved {
        drop(prior_pull_project);
        return moved;
    }
    let Some(prior) = prior_pull_project else {
        return Vec::new();
    };
    if !has_held_pull {
        drop(prior);
        return Vec::new();
    }
    let paths: BTreeSet<&PathBuf> = prior.entries.keys().chain(studio.entries.keys()).collect();
    let differing: HashSet<PathBuf> = paths
        .into_par_iter()
        .filter(|path| prior.entries.get(*path) != studio.entries.get(*path))
        .cloned()
        .collect();
    let mut changed = services_for_snapshot_paths(context, &differing)
        .into_iter()
        .collect::<Vec<_>>();
    changed.sort();
    drop(prior);
    changed
}

pub(crate) fn push_project(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    mut push_args: PushEditorChangesArgs,
    guard: Option<&StudioChangeGuard>,
    replace: bool,
) -> Result<Map<String, Value>> {
    #[cfg(any(windows, target_os = "macos"))]
    let mut verified_candidate = {
        match reuse_verified_full_push(
            context,
            bridge,
            services,
            &push_args,
            replace && guard.is_none(),
        )? {
            VerifiedFullPushReuse::Unchanged(result) => return Ok(result),
            VerifiedFullPushReuse::Continue(candidate) => candidate,
        }
    };
    #[cfg(any(windows, target_os = "macos"))]
    let cache_input = (replace && guard.is_none())
        .then(|| full_push_cache_input(&push_args, services))
        .transpose()?;
    #[cfg(windows)]
    let mut native_relay = None;
    // A baseline over files a pull wrote holds the sourcemap that pull left.
    #[cfg(any(windows, target_os = "macos"))]
    let held_sourcemap = verified_candidate
        .as_mut()
        .and_then(|candidate| candidate.cached.sourcemap.take());
    let phase = Instant::now();
    // The previous push's native attribute observer must outlive the proof
    // check inside the lease request, yet stop before the new lease arms its
    // own observer: stopping it earlier retires the proof, stopping it later
    // disarms the fresh observer.
    #[cfg(any(windows, target_os = "macos"))]
    let (candidate_parts, candidate_files_unchanged, mut previous_attributes) =
        match verified_candidate {
            Some(candidate) => {
                let VerifiedFullPushCandidate {
                    cached,
                    files_unchanged,
                } = candidate;
                #[cfg(windows)]
                let previous = cached.relay;
                #[cfg(not(windows))]
                let previous = ();
                (
                    Some((cached.proof.clone(), cached.project)),
                    files_unchanged,
                    previous,
                )
            }
            None => {
                #[cfg(windows)]
                let previous = None;
                #[cfg(not(windows))]
                let previous = ();
                (None, false, previous)
            }
        };
    #[cfg(not(any(windows, target_os = "macos")))]
    let (candidate_parts, mut previous_attributes): (Option<(Value, ProjectSnapshot)>, ()) =
        (None, ());
    let candidate_proof = candidate_parts.as_ref().map(|(proof, _)| proof.clone());
    #[cfg(windows)]
    let continue_native_relay = previous_attributes
        .as_ref()
        .is_some_and(|relay| relay.ends > Instant::now() + Duration::from_secs(30));
    #[cfg(not(windows))]
    let continue_native_relay = false;
    let (mut guard, initial_state) = if let Some(guard) = guard {
        (guard.clone(), Value::Null)
    } else {
        current_studio_change_guard_verifying_continuing(
            context,
            bridge,
            (cfg!(any(windows, target_os = "macos")) && replace).then_some(services),
            candidate_proof.as_ref(),
            continue_native_relay,
            |state| {
                #[cfg(windows)]
                arm_native_attribute_relay(
                    state,
                    bridge,
                    services,
                    &mut previous_attributes,
                    &mut native_relay,
                )?;
                #[cfg(not(windows))]
                let _ = state;
                Ok(())
            },
        )?
    };
    log_reconcile_timing("full push guard", phase);
    #[cfg(windows)]
    if initial_state["nativeRelayContinued"] == true {
        native_relay = previous_attributes.take();
    } else {
        drop(previous_attributes.take());
    }
    #[cfg(not(windows))]
    let _ = &mut previous_attributes;
    // The held proof the lease verified as it began, over files a pull wrote:
    // the import settlement may skip services only from such a baseline.
    #[cfg(any(windows, target_os = "macos"))]
    let (held_pull, held_sourcemap_fallback) = if initial_state["pushProofMatches"] == true {
        (held_sourcemap.zip(candidate_proof), None)
    } else {
        (None, held_sourcemap)
    };
    // The proof was checked inside the request that armed tracking, so a match
    // means every later Studio edit is caught by the transaction's generation
    // checks and the last verified files are a complete baseline.
    let (verified_baseline, mut scoped_baseline) = match candidate_parts {
        Some((_, project)) if initial_state["pushProofMatches"] == true => {
            log_global(
                5,
                format_args!("[renium] baseline: full push compared with the held files"),
            );
            (Some(project), None)
        }
        Some((_, project)) => {
            log_global(
                5,
                format_args!(
                    "[renium] full push cache miss: {}",
                    initial_state["pushProofMismatch"]
                        .as_str()
                        .unwrap_or("Studio proof unavailable")
                ),
            );
            (None, Some(project))
        }
        None => (None, None),
    };
    let phase = Instant::now();
    // Declared before the tracking lease so unwinding releases Lua tracking
    // before native observation. On cancellation the native guard can promote
    // surviving listeners before disconnecting its shared signal.
    let mut tracking_release =
        StudioTrackingGuardRelease::new(bridge, &guard.runtime_id, guard.tracking_guard_id.clone());
    #[cfg(windows)]
    {
        tracking_release.retain_until = native_relay.as_ref().map(|relay| relay.ends);
    }
    let src_dir = push_args.project.src_root.clone();
    let root = push_args.project.project_root.clone();
    let requires_stage = config::try_load_project(None, Some(&root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false);
    let stage = if requires_stage {
        ExportProjectStage::create(&root, &src_dir, services)?
    } else {
        ExportProjectStage::create_for_comparison(&root, &src_dir, services)?
    };
    let source_paths = stage.publish_paths().to_vec();
    let can_capture_in_memory = !requires_stage
        && stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty());
    let link_enforcement = super::staging::packing_link_enforcement(
        Path::new(&context.root),
        Path::new(&context.source),
    )?;
    let bulk_eligible = link_enforcement.is_none()
        && !requires_stage
        && crate::editor::sync::files_to_studio_filter_rules(&root)?.is_none()
        && bridge
            .cached_bridge_info_for_target(BridgeTarget::Edit)
            .is_ok_and(|info| info.bulk_property_patch);
    #[cfg(any(windows, target_os = "macos"))]
    if replace
        && bulk_eligible
        && verified_baseline.is_none()
        && candidate_files_unchanged
        && can_capture_in_memory
        && let Some(held_project) = scoped_baseline.as_ref()
    {
        let zctx = ZeroCaptureContext {
            context,
            bridge,
            services,
            root: &root,
            src_dir: &src_dir,
            source_paths: &source_paths,
            stage: &stage,
            guard: &guard,
            push_args: &push_args,
            initial_state: &initial_state,
            cache_input: cache_input.as_ref(),
        };
        if let Some(summary) = try_zero_capture_bulk_restore(
            &zctx,
            held_project,
            &mut tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        )? {
            return Ok(summary);
        }
    }
    let scoped_capture_services = scoped_push_capture_services(
        context,
        services,
        verified_baseline.is_none() && can_capture_in_memory,
        scoped_baseline.as_ref(),
        &initial_state,
    );
    let scoped_moved = scoped_capture_services.clone();
    #[cfg(any(windows, target_os = "macos"))]
    let held_pull = pre_push_settlement_proof(
        bridge,
        &guard,
        held_pull,
        held_sourcemap_fallback,
        replace && verified_baseline.is_none() && scoped_baseline.is_some(),
    );
    // Source capture is read-only and independent of the private Studio stage.
    // Keep Studio calls on this thread, with its selected runtime and lease.
    let (captured, project) = std::thread::scope(|scope| {
        let project = scope.spawn(|| capture_snapshot(&root, &source_paths));
        let captured = if let Some(baseline) = verified_baseline {
            Ok((stage, baseline))
        } else if let (Some(moved), Some(baseline)) =
            (scoped_capture_services, scoped_baseline.take())
        {
            capture_scoped_push_baseline(context, bridge, &root, &moved, &stage, baseline)
                .map(|studio| (stage, studio))
        } else if can_capture_in_memory {
            capture_studio_services_in_memory(context, bridge, services, &stage, false)
                .map(|studio| (stage, studio))
        } else {
            capture_studio_services_with_stage(context, bridge, services, false, stage)
        };
        (
            captured,
            project.join().expect("Source capture worker panicked"),
        )
    });
    let (stage, mut studio) = captured?;
    let project = project?;
    #[cfg(any(windows, target_os = "macos"))]
    let prior_differing_services = collect_prior_differing_services(
        context,
        scoped_moved,
        scoped_baseline,
        held_pull.is_some(),
        &studio,
    );
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        drop(scoped_moved);
        drop(scoped_baseline);
    }
    if let Some(enforcement) = &link_enforcement {
        super::staging::pack_read_only_link_scripts_with(
            enforcement,
            Path::new(&context.root),
            &project,
            &mut studio,
        )?;
    }
    log_reconcile_timing("full push capture", phase);
    let phase = Instant::now();
    let mut prepared_settings = HashMap::new();
    let mut bulk_patches = HashMap::new();
    let promoted_services;
    let stores_read;
    let differences = if replace {
        let (diff_paths, promoted, read_mode) = prepare_replace_push_differences(
            context,
            &root,
            &stage,
            (&project, &studio),
            (bulk_eligible, link_enforcement.is_none()),
            &mut prepared_settings,
            &mut bulk_patches,
        )?;
        stores_read = read_mode;
        promoted_services = promoted;
        diff_paths
    } else {
        stores_read =
            stores_compared_of(link_enforcement.is_none(), (&project, &studio), &push_args)?;
        let mut differences = if bulk_eligible && stores_read != StoresRead::Whole {
            collect_delta_bulk_differences(
                &project,
                &studio,
                stores_read,
                &mut prepared_settings,
                &mut bulk_patches,
            )?
        } else {
            snapshot_differences_prepared(
                &project,
                &studio,
                Some(&mut prepared_settings),
                stores_read == StoresRead::ChangedOfBoth,
            )?
        };
        promoted_services = promote_heavy_deltas_to_replacement(
            context,
            &project,
            &studio,
            &mut differences,
            &mut prepared_settings,
        )?;
        differences
    };
    let differences = differences
        .into_iter()
        .filter(|path| {
            prepared_settings.get(path).is_none_or(|change| {
                !super::verify::differs_only_in_externally_managed_fields(
                    &change.current,
                    &change.previous,
                )
            })
        })
        .collect::<HashSet<_>>();
    prepared_settings.retain(|path, _| differences.contains(path));
    log_reconcile_timing(
        if replace {
            "full replacement preparation"
        } else {
            "full push comparison"
        },
        phase,
    );
    log_full_push_differences(&differences);
    push_args.saved_fields = collect_saved_field_differences(context, &guard, &prepared_settings);
    if differences.is_empty() {
        tracking_release.proof = capture_push_proof(bridge, &guard)?;
        acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)?;
        #[cfg(any(windows, target_os = "macos"))]
        retain_verified_full_push(
            bridge,
            cache_input.as_ref(),
            &source_paths,
            project,
            &tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        );
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let phase = Instant::now();
    let non_bulk_differences = differences
        .iter()
        .filter(|path| !bulk_patches.contains_key(*path))
        .cloned()
        .collect::<HashSet<_>>();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &project,
        &non_bulk_differences,
        &prepared_settings,
        true,
        false,
    )?;
    let mut plan = plan_from_whole_stores(
        (stores_read, replace, plan),
        (&project, &studio),
        &non_bulk_differences,
        &mut prepared_settings,
    )?;
    let mut bulk_entries = bulk_patches.into_iter().collect::<Vec<_>>();
    bulk_entries.sort_by(|left, right| left.0.cmp(&right.0));
    for (path, patch) in bulk_entries {
        plan.bulk_property_paths.push(path);
        plan.bulk_property_patches.push(patch);
    }
    extend_replaced_service_references(
        &project,
        &prepared_settings,
        &promoted_services,
        &mut plan,
    )?;
    log_reconcile_timing("full push plan", phase);
    log_full_push_plan(&plan);
    if plan.is_empty() {
        tracking_release.proof = capture_push_proof(bridge, &guard)?;
        acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)?;
        #[cfg(any(windows, target_os = "macos"))]
        retain_verified_full_push(
            bridge,
            cache_input.as_ref(),
            &source_paths,
            project,
            &tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        );
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let mutation_paths = differences.clone();
    let phase = Instant::now();
    let mut history = history::SyncHistory::begin_push(
        &root,
        &src_dir,
        &studio,
        &mutation_paths,
        &mut push_args,
    )?;
    let settle_native_import = replace && !plan.initial_native_services.is_empty();
    #[cfg(any(windows, target_os = "macos"))]
    let held_pull = held_pull.filter(|_| replace).map(|held| {
        let mut touched = written_services(context, &plan);
        for service in prior_differing_services {
            if !touched.contains(&service) {
                touched.push(service);
            }
        }
        (held, touched)
    });
    if !replace {
        apply_snapshot_paths(&stage.project_root, &mutation_paths, &project)?;
    }
    let mut prepared_documents = HashMap::with_capacity(prepared_settings.len());
    let mut prepared_verification = HashMap::with_capacity(prepared_settings.len());
    for (path, change) in prepared_settings {
        let service = settings_service_name(&path, &change.current, &change.previous)?;
        let desired = Arc::new(change.current);
        prepared_documents.insert(service, Arc::clone(&desired));
        prepared_verification.insert(
            path,
            PreparedPushVerification {
                previous: change.previous,
                desired: Some(desired),
            },
        );
    }
    #[cfg(any(windows, target_os = "macos"))]
    let can_settle_in_background =
        replace && import_settle::can_continue(&stage, requires_stage, link_enforcement.is_some());
    push_args.inline_release_tracking = bulk_inline_release_tracking(
        &plan,
        services,
        &guard,
        &tracking_release,
        requires_stage,
        link_enforcement.is_some(),
    );
    let saved_fields = push_args.saved_fields.clone();
    let mut pushed = push_staged_project(
        context,
        &stage,
        bridge,
        StagedPushRequest {
            plan,
            prepared_documents,
            guard: Some(&guard),
            args: push_args,
            expected_project: Some(&project),
            later_edits_follow: false,
        },
    )?;
    if let Some(release_result) = pushed.summary.remove("_inlineReleaseResult") {
        apply_release_tracking_result(&guard, &mut tracking_release, &release_result);
    }
    if push_applied_nothing(&pushed.summary) {
        // Studio holds or skipped the whole push, so there is nothing to
        // record or read back; the summary carries the hold and its next step.
        return Ok(pushed.summary);
    }
    leave_unread_documents(&mut prepared_verification);
    pushed.summary.insert(
        "historyId".into(),
        Value::String(history.commit(&project, &pushed.generated)?),
    );
    crate::editor::sync::finish_committed_saved_fields(
        saved_fields.as_deref(),
        &root,
        bridge,
        &mut pushed.summary,
    );
    log_reconcile_timing("full push mutation", phase);

    let pending_fields: Vec<crate::editor::types::PendingImportFields> = pushed
        .summary
        .get("supplementalImportPendingFields")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();

    if pushed
        .summary
        .get("protectedOfflineApply")
        .is_some_and(|receipt| receipt["recoveryPending"] == true)
    {
        // The committed snapshot and undo record survive a disconnected
        // successor. Reading the retired runtime cannot verify this push.
        pushed.summary.insert("verified".into(), Value::Bool(false));
        return Ok(pushed.summary);
    }

    let replacement = reopened_push_context(context, &pushed.summary)?;
    let _replacement_selection = replacement.as_ref().map(bound_context::select);
    let context = replacement.as_ref().unwrap_or(context);
    if replacement.is_some() {
        // The old process and its observation epoch ended intentionally. Never
        // route readback or acknowledge its sequence on the replacement runtime.
        tracking_release.guard_id = None;
        pin_edit_runtime(context, bridge)?;
        guard = current_studio_change_guard(context, bridge)?;
        tracking_release.runtime_id.clone_from(&guard.runtime_id);
        tracking_release
            .guard_id
            .clone_from(&guard.tracking_guard_id);
    }

    // A settlement reads back fewer services than a pull only from a held
    // pull, where the push wrote some services alone and no linked package
    // is restored.
    #[cfg(any(windows, target_os = "macos"))]
    let narrowable = held_pull.as_ref().filter(|(_, written)| {
        services.iter().any(|service| !written.contains(service))
            && !Path::new(&context.root).join("renium-link.json").exists()
    });
    // Roots imported beside a service's other instances take values Studio
    // generates for them, which only a pull writes to the files. A service
    // the importer replaced whole carries its loader receipt instead. Their
    // settlement must read back fewer services than a pull, which only a
    // held pull whose sourcemap the files still hold allows.
    #[cfg(any(windows, target_os = "macos"))]
    let added_roots = !settle_native_import
        && pushed
            .summary
            .get("binaryInstances")
            .and_then(Value::as_f64)
            .is_some_and(|count| count > 0.0)
        && pushed
            .summary
            .get("nativeVerifiedServices")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        && narrowable.is_some_and(|((sourcemap, _), _)| {
            import_settle::holds_sourcemap(Path::new(&context.root), sourcemap)
        });
    // Files the push wrote itself are read back to be verified.
    #[cfg(any(windows, target_os = "macos"))]
    let generated_files = !pushed.generated.entries.is_empty();
    #[cfg(any(windows, target_os = "macos"))]
    if let Some((proof, settled)) = (can_settle_in_background
        && (settle_native_import || added_roots))
        .then_some(())
        .filter(|_| replacement.is_none())
        .and_then(|()| {
            committed_import_proof(
                context,
                &pushed.summary,
                &mutation_paths,
                services,
                settle_native_import,
            )
        })
        .map(|proof| {
            // Only a settlement the commit proof fences until publication
            // skips services: the held proof covers what changed before it.
            let settled = narrowable.map_or_else(
                || services.to_vec(),
                |((_, held), written)| {
                    settled_services(
                        context,
                        bridge,
                        (services, written.clone()),
                        held,
                        (&studio, &project, &pushed.generated),
                    )
                },
            );
            (proof, settled)
        })
        // Added roots are settled only where that reads back less than a
        // pull would; their receipt already verified the push.
        .filter(|(_, settled)| settle_native_import || settled.len() < services.len())
        // A Studio that changed since the commit gets the full readback below
        // before the push answers.
        .filter(|(proof, _)| verified_full_push_proof_matches(context, bridge, proof).unwrap_or(false))
        // A journal records only a verified Studio commit whose caller can
        // leave before publication. Failed/review-held pushes and synchronous
        // readback never create recovery obligations.
        && import_settle::journal(
            context,
            import_settle::ImportStart {
                services,
                paths: &source_paths,
                verification_paths: &mutation_paths,
                before: &studio,
                expected: &project,
                generated: &pushed.generated,
                input: cache_input.as_ref(),
                history_id: history.id()?,
                optional: added_roots,
            },
            &pushed.summary,
        )?
    {
        let narrowed =
            narrowable
                .filter(|_| settled.len() < services.len())
                .map(|((sourcemap, _), _)| import_settle::Narrowed {
                    services: &settled,
                    sourcemap,
                });
        let mut expected = project;
        expected.entries.extend(pushed.generated.entries);
        import_settle::continue_request(context, bridge)?;
        crate::automation::runtime::respond_to_verified_import(context, bridge, &pushed.summary);
        let mut work = import_settle::ImportWork {
            before: studio,
            expected,
            paths: source_paths.clone(),
            verification_paths: mutation_paths,
            prepared: prepared_verification,
            history,
            summary: pushed.summary,
        };
        let settlement =
            import_settle::settle(context, bridge, (services, narrowed), &mut work, &proof)?;
        tracking_release.proof = Some(proof);
        acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)?;
        anyhow::ensure!(
            tracking_release.retained_proof.is_some(),
            "Studio changed before import settlement was acknowledged"
        );
        let kept = match settlement {
            import_settle::Settlement::Published(published) => published,
            // Files a settlement of added roots left as the push wrote them
            // stand for the Studio that holds its commit, as after a push
            // that reads nothing back, but for files it wrote itself.
            import_settle::Settlement::Unwritten if added_roots && !generated_files => {
                (work.expected, None)
            }
            _ => return Ok(work.summary),
        };
        retain_full_push(
            bridge,
            cache_input.as_ref(),
            &source_paths,
            kept,
            &tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        );
        return Ok(work.summary);
    }

    let mut current = capture_snapshot(&root, stage.publish_paths())?;
    let mut verification_paths = differences;
    verification_paths.extend(mutation_paths);
    verification_paths.extend(pushed.generated.entries.keys().cloned());
    let verified_services = ["nativeVerifiedServices", "fieldVerifiedServices"]
        .into_iter()
        .filter_map(|name| pushed.summary.get(name).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    // Native replacement validates the loader receipt, retained settings,
    // supplemental writes and script buffers before committing. Re-export only
    // services outside that contract, transformed outputs or newer file edits.
    if !settle_native_import
        && !requires_stage
        && replacement.is_none()
        && stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty())
        && pushed
            .summary
            .get("sourceVerifyFailed")
            .and_then(Value::as_u64)
            == Some(0)
        && !verified_services.is_empty()
    {
        verification_paths.retain(|path| {
            let service = crate::editor::paths::service_from_changed_path(
                Path::new(&context.source),
                &Path::new(&context.root).join(path),
            );
            !service.is_some_and(|service| {
                verified_services
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&service))
            }) || pushed.generated.entries.contains_key(path)
                || !entries_equivalent(path, project.entries.get(path), current.entries.get(path))
        });
    }
    if verification_paths.is_empty()
        || (pushed.generated.entries.is_empty()
            && exact_source_push_verified(&pushed.summary, &verification_paths))
    {
        if project == current && !requires_stage && replacement.is_none() {
            tracking_release.proof = pushed.summary.remove("verifiedPushProof");
        } else {
            tracking_release.retained_proof = None;
        }
        std::thread::scope(|scope| {
            // No remaining readback consumes these trees. Release their large
            // allocations on the existing parallel path while Studio closes the
            // observation lease; join cleanup before returning to the caller.
            scope.spawn(move || {
                prepared_verification
                    .into_par_iter()
                    .for_each(|(_, verified)| {
                        if let Some(desired) = verified.desired.and_then(Arc::into_inner) {
                            drop_settings_documents(verified.previous, desired);
                        } else {
                            drop_settings_document(verified.previous);
                        }
                    });
                drop(stage);
            });
            if tracking_release.guard_id.is_some() {
                acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)
            } else {
                Ok(())
            }
        })?;
        #[cfg(any(windows, target_os = "macos"))]
        retain_verified_full_push(
            bridge,
            cache_input.as_ref(),
            &source_paths,
            project,
            &tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        );
        return Ok(pushed.summary);
    }
    let verification_services = if settle_native_import {
        log_native_settlement_fallbacks(services, &verified_services);
        services.to_vec()
    } else {
        services_for_snapshot_paths(context, &verification_paths)
    };
    // A full import allocates new native identities and settles service state.
    // Reuse its verification capture to publish that accepted representation,
    // with the files read above as its baseline so newer editor work is safe.
    let import_readback = settle_native_import
        .then(|| -> Result<_> {
            let covered = ScopeSet::new(&source_paths);
            let mut stage =
                ExportProjectStage::create_with_baseline(&root, &src_dir, services, &|paths| {
                    publish_baseline(&root, &current, &covered, paths)
                })?;
            stage.prepare_read_only_links(&root, &src_dir, services)?;
            Ok(stage)
        })
        .transpose()?;
    let phase = Instant::now();
    let (mut readback_stage, mut readback, settle_verified_capture) = capture_push_readback(
        context,
        bridge,
        &verification_services,
        &src_dir,
        requires_stage,
        import_readback,
        link_enforcement.is_none(),
    )?;
    if let Some(enforcement) = &link_enforcement {
        super::staging::pack_read_only_link_scripts_with(
            enforcement,
            Path::new(&context.root),
            &current,
            &mut readback,
        )?;
    }
    log_reconcile_timing("full push readback", phase);
    let phase = Instant::now();
    // These documents were already aligned to the exact requested file bytes.
    // Generated values or a later file edit invalidate that proof, not merely
    // the observed Studio snapshot (which is always read and aligned anew).
    // The values this push wrote to a store itself, from what Studio generated,
    // left every instance at its place: Studio's copy from before still says
    // what changed, and the files' is read as it stands now.
    prepared_verification.retain(|path, verified| {
        let now = current.entries.get(path);
        if entries_equivalent(path, project.entries.get(path), now) {
            return true;
        }
        let generated = pushed.generated.entries.get(path);
        verified.desired = None;
        generated.is_some() && generated == now
    });
    let (mismatches, details) = verify::verify_committed_push_readback(
        &studio,
        &current,
        &mut readback,
        &verification_paths,
        &mut prepared_verification,
        verify::Verification::of(
            settle_verified_capture,
            &pending_fields,
            &verify::NewInstanceValues::of(bridge, context.runtime_id.as_deref()),
        ),
    )?;
    log_reconcile_timing("full push verification", phase);
    if !mismatches.is_empty() {
        return Err(retention_failure(
            "pushed project changes",
            &mismatches,
            details.as_deref(),
        ));
    }
    if !pending_fields.is_empty() {
        // Keep desired pending values in the files for the retry. Publishing the
        // actual readback would erase them; caching equality would hide them.
        pushed.summary.insert("verified".into(), Value::Bool(false));
        pushed.summary.remove("verifiedPushProof");
        tracking_release.proof = None;
        acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)?;
        return Ok(pushed.summary);
    }
    if settle_verified_capture {
        let phase = Instant::now();
        stage_verified_import_readback(&mut readback_stage, &readback, current.entries.keys())?;
        log_reconcile_timing("full push stage verified readback", phase);
    }
    let mut expected_after_push = project;
    expected_after_push.entries.extend(pushed.generated.entries);
    let files_held = expected_after_push == current;
    let published = (files_held && settle_native_import)
        .then(|| {
            settle_native_import_files(
                readback_stage,
                (&root, &source_paths),
                &mut history,
                &mut current,
                &mut pushed.summary,
            )
        })
        .transpose()?
        .flatten();
    let files_unchanged = files_held && (!settle_native_import || published.is_some());
    if files_unchanged && !requires_stage && replacement.is_none() {
        // Readback verified the remaining fields. The commit proof predates
        // that readback, so retaining it still requires Studio to have remained
        // unchanged throughout verification (including its asynchronous reads).
        // Accepted generated settings are also verified by readback; only file
        // changes outside that exact result invalidate the source snapshot.
        tracking_release.proof = pushed.summary.remove("verifiedPushProof");
    }
    acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)?;
    #[cfg(any(windows, target_os = "macos"))]
    retain_full_push(
        bridge,
        cache_input.as_ref(),
        &source_paths,
        (
            current,
            published.as_ref().and_then(import_settle::pulled_sourcemap),
        ),
        &tracking_release,
        #[cfg(windows)]
        &mut native_relay,
    );
    Ok(pushed.summary)
}

/// A push of chosen paths or instances. With a held baseline it runs under a
/// lease that continues the baseline's observation, and keeps a baseline of
/// the files where it can say Studio holds them after the push.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn push_targeted(
    context: &BoundContext,
    bridge: &BridgeServer,
    args: PushEditorChangesArgs,
) -> Result<Map<String, Value>> {
    use crate::editor::sync::push_editor_changes_with_warm_bridge;
    let Some(mut held) = take_baseline(context, bridge, &plain_push_input(&args)?) else {
        return push_editor_changes_with_warm_bridge(args, bridge);
    };
    let not_kept = |reason: &str| {
        log_global(
            5,
            format_args!("[renium] baseline: not kept after targeted push: {reason}"),
        );
    };
    let root = args.project.project_root.clone();
    // The push itself needs neither the lease nor the files as they stood:
    // without them it runs as it does with no baseline held.
    let lease = {
        let _span = crate::app::profile::span("held state: files and lease before the push");
        capture_snapshot(&root, &held.paths).and_then(|before| {
            Ok(baseline_lease(context, bridge, &held)?.map(|lease| (before, lease)))
        })
    };
    let (before, (guard, mut release)) = match lease {
        Ok(Some(lease)) => lease,
        Ok(None) => return push_editor_changes_with_warm_bridge(args, bridge),
        Err(error) => {
            not_kept(&format!("{error:#}"));
            return push_editor_changes_with_warm_bridge(args, bridge);
        }
    };
    let pushed = TargetedPush::of(&args);
    let summary = {
        let _span = crate::app::profile::span("push of the chosen paths");
        push_editor_changes_with_warm_bridge(args, bridge)?
    };
    if push_changed_structure(&summary) {
        // No comparison of the stores can keep these files: skip it.
        not_kept("the push added, replaced or removed instances");
        return Ok(summary);
    }
    let _span = crate::app::profile::span("held state: files and lease after the push");
    let after = match capture_snapshot(&root, &held.paths) {
        Ok(after) => after,
        Err(error) => {
            not_kept(&format!("{error:#}"));
            return Ok(summary);
        }
    };
    let service_of = |path: &Path| {
        crate::editor::paths::service_from_changed_path(
            Path::new(&context.source),
            &Path::new(&context.root).join(path),
        )
    };
    let project = match targeted_baseline(
        &held.project,
        (&before, &after),
        &summary,
        &pushed,
        service_of,
    ) {
        Ok(project) => project,
        Err(reason) => {
            not_kept(&reason);
            return Ok(summary);
        }
    };
    let proof = if push_applied_nothing(&summary) {
        Some(held.proof.clone())
    } else {
        summary
            .get("verifiedPushProof")
            .filter(|value| !value.is_null())
            .cloned()
    };
    release.proof.clone_from(&proof);
    if let Err(error) = release_tracking_lease(bridge, &guard, &mut release, json!({})) {
        not_kept(&format!("{error:#}"));
        return Ok(summary);
    }
    let Some(proof) = proof.filter(|proof| release.retained_proof.as_ref() == Some(proof)) else {
        not_kept("Studio holds no proof of the push");
        return Ok(summary);
    };
    if !release.studio_unchanged || !release.proof_continues {
        not_kept("Studio changed during the push");
        return Ok(summary);
    }
    // Studio held the baseline when the lease began, no tracked change but
    // the push's own reached a synced service until the release, and the
    // state no event tracks (Terrain voxels, open script buffers) is what the
    // baseline's proof saw. The push wrote and verified every file that
    // differs from the baseline, so Studio holds these files.
    #[cfg(windows)]
    let relay = held.relay.take();
    let entry = VerifiedFullPush {
        connections: bridge.runtime_connection_signature(&release.runtime_id),
        input: std::mem::take(&mut held.input),
        sourcemap: sourcemap_after_push(&held, &project, &summary),
        project,
        paths: std::mem::take(&mut held.paths),
        proof,
        expires: baseline_expiry(
            #[cfg(windows)]
            relay.as_ref(),
        ),
        #[cfg(windows)]
        relay,
    };
    keep_baseline(bridge, &release.runtime_id, entry, "targeted push");
    Ok(summary)
}

/// A tracking lease that continues the held baseline's observation, which
/// Studio grants only while the baseline's proof holds and tracking runs.
#[cfg(any(windows, target_os = "macos"))]
fn baseline_lease<'a>(
    context: &BoundContext,
    bridge: &'a BridgeServer,
    held: &VerifiedFullPush,
) -> Result<Option<(StudioChangeGuard, StudioTrackingGuardRelease<'a>)>> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    pin_edit_runtime(context, bridge)?;
    let guard_id = format!(
        "push-{}-{}-{}",
        std::process::id(),
        runtime_id,
        crate::app::timing::current_millis()
    );
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "start": true,
            "trackingGuardId": guard_id,
            "includeGenerations": true,
            "services": sync_services(),
            "captureLocalPushProof": true,
            "verifyPushProof": held.proof,
            "requirePushProof": true,
        }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    if state["pushProofMatches"] != true {
        log_global(
            5,
            format_args!(
                "[renium] full push cache miss: {}",
                state["pushProofMismatch"]
                    .as_str()
                    .unwrap_or("Studio proof unavailable")
            ),
        );
        return Ok(None);
    }
    let mut release = StudioTrackingGuardRelease::new(bridge, runtime_id, Some(guard_id.clone()));
    let mut guard = studio_change_guard_from_state(context, &state)?;
    guard.tracking_guard_id = Some(guard_id);
    release.continues = Some(held.proof.clone());
    #[cfg(windows)]
    {
        release.retain_until = held.relay.as_ref().map(|relay| relay.ends);
    }
    Ok(Some((guard, release)))
}

#[cfg(any(windows, target_os = "macos"))]
/// What a push of chosen paths or instances wrote: the project paths it
/// named, and the store instances it wrote whole.
pub(crate) struct TargetedPush {
    pub(crate) paths: HashSet<PathBuf>,
    pub(crate) ids: HashSet<String>,
    /// Whether what stands under the chosen instances went with them.
    pub(crate) exact: bool,
    /// Whether each instance went whole: no property filter, no instance
    /// list or path list file, no upsert of instances alone.
    pub(crate) whole_rows: bool,
}

#[cfg(any(windows, target_os = "macos"))]
impl TargetedPush {
    pub(crate) fn of(args: &PushEditorChangesArgs) -> Self {
        let root = &args.project.project_root;
        Self {
            paths: args
                .changed_paths
                .iter()
                .map(|path| {
                    let absolute = absolutize_under(root, path);
                    absolute
                        .strip_prefix(root)
                        .map_or_else(|_| absolute.clone(), Path::to_path_buf)
                })
                .collect(),
            ids: args.target_settings_ids.iter().cloned().collect(),
            exact: args.exact_target_ids,
            whole_rows: args.target_properties.is_empty()
                && args.target_settings_id_files.is_empty()
                && args.changed_paths_files.is_empty()
                && !args.upsert_instances_only,
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
/// The baseline after a verified push of chosen paths over the files `held`
/// that Studio held: the files as they stand, where every file that differs
/// from `held` is one the push named, wrote and verified. A push that applied
/// nothing leaves `held`. Otherwise says why there is none.
pub(crate) fn targeted_baseline(
    held: &ProjectSnapshot,
    (before, after): (&ProjectSnapshot, &ProjectSnapshot),
    summary: &Map<String, Value>,
    pushed: &TargetedPush,
    service_of: impl Fn(&Path) -> Option<String>,
) -> std::result::Result<ProjectSnapshot, String> {
    if !pushed.whole_rows {
        return Err("the push chose fields or read its selection from a file".into());
    }
    if before != after {
        return Err("the files changed during the push".into());
    }
    if summary.get("ok") == Some(&Value::Bool(false))
        || summary.get("verified") == Some(&Value::Bool(false))
        || [
            "unsupportedProperties",
            "protectedPending",
            "supplementalImportPendingWrites",
            "protectedOfflineApply",
        ]
        .iter()
        .any(|key| summary.contains_key(*key))
    {
        return Err("the push left changes unwritten or unverified".into());
    }
    if push_applied_nothing(summary) {
        return Ok(held.clone());
    }
    let verified = ["nativeVerifiedServices", "fieldVerifiedServices"]
        .into_iter()
        .filter_map(|key| summary.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let is_store = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    };
    let scripts = pushed
        .paths
        .iter()
        .filter(|path| {
            super::merge::is_source_path(path)
                && matches!(after.entries.get(*path), Some(SnapshotEntry::File(_)))
        })
        .count();
    let scripts_verified = summary.get("sourceVerifyFailed").and_then(Value::as_u64) == Some(0)
        && summary.get("sourceVerified").and_then(Value::as_u64) == Some(scripts as u64);
    let paths = held
        .entries
        .keys()
        .chain(after.entries.keys())
        .collect::<BTreeSet<_>>();
    for path in paths {
        let (was, now) = (held.entries.get(path), after.entries.get(path));
        if was == now {
            continue;
        }
        if !pushed.paths.contains(path) {
            return Err(format!(
                "{} differs from the baseline outside the push",
                path.display()
            ));
        }
        let refused = match (was, now) {
            (Some(SnapshotEntry::File(was)), Some(SnapshotEntry::File(now)))
                if is_store(path.as_path()) =>
            {
                if !service_of(path.as_path()).is_some_and(|service| {
                    verified
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&service))
                }) {
                    Some("the push did not verify its service")
                } else if !store_rows_differ_only_at(was, now, &pushed.ids, pushed.exact)
                    .unwrap_or(false)
                {
                    Some("it differs beyond the instances the push wrote")
                } else {
                    None
                }
            }
            (Some(SnapshotEntry::File(_)), Some(SnapshotEntry::File(_)))
                if super::merge::is_source_path(path) =>
            {
                (!scripts_verified).then_some("the push did not verify its source")
            }
            _ => Some("the push does not write such a change"),
        };
        if let Some(reason) = refused {
            return Err(format!("{}: {reason}", path.display()));
        }
    }
    Ok(after.clone())
}

#[cfg(any(windows, target_os = "macos"))]
/// Whether a verified write of the whole instances `ids` of a store (with
/// what stands under them unless `exact`) leaves the store `held` held as
/// `files` holds it. Every other instance must be the same, every instance
/// at the same place, and a written instance must hold every value it held
/// before, each one a push writes: a write sets values and resets none. A
/// push sends no PackageLink and not the viewport's camera, and a script's
/// file is named after its RunContext, so a change there keeps nothing.
pub(crate) fn store_rows_differ_only_at(
    held: &[u8],
    files: &[u8],
    ids: &HashSet<String>,
    exact: bool,
) -> Result<bool> {
    use crate::settings::bytecode::property_edit::{decode_rows, whole_row_digests_of_both};
    let (was, now) = whole_row_digests_of_both(held, files, None);
    let (Some(was), Some(now)) = (was?, now?) else {
        return Ok(false);
    };
    let count = was.instances.len();
    let same_places = now.instances.len() == count
        && was.digests.len() == count
        && now.digests.len() == count
        && was.unique_ids == now.unique_ids
        && was.instances.iter().zip(&now.instances).all(|(was, now)| {
            was.settings_id == now.settings_id
                && was.name == now.name
                && was.class_name == now.class_name
                && was.parent_index == now.parent_index
        });
    if !same_places {
        return Ok(false);
    }
    let mut chosen = was
        .instances
        .iter()
        .map(|instance| ids.contains(&instance.settings_id))
        .collect::<Vec<_>>();
    // A store can hold a parent after its child, so the pass repeats until
    // nothing joins.
    let mut joined = !exact;
    while joined {
        joined = false;
        for index in 0..count {
            if !chosen[index]
                && was.instances[index]
                    .parent_index
                    .is_some_and(|parent| parent < count && chosen[parent])
            {
                chosen[index] = true;
                joined = true;
            }
        }
    }
    let differing = (0..count)
        .filter(|&index| was.digests[index] != now.digests[index])
        .collect::<Vec<_>>();
    if differing.iter().any(|&index| !chosen[index]) {
        return Ok(false);
    }
    if differing.is_empty() {
        return Ok(true);
    }
    let Ok(database) = rbx_reflection_database::get() else {
        return Ok(false);
    };
    let (was_rows, _) = decode_rows(held, &differing)?;
    let (now_rows, _) = decode_rows(files, &differing)?;
    Ok(was_rows.iter().zip(&now_rows).all(|(was, now)| {
        // Studio holds a Decal's ColorMapContent as its TextureContent.
        let mirror = "ColorMapContent";
        let mirrored = !crate::settings::equivalence::reconciliation_property_is_engine_mirror(
            database,
            &now.class_name,
            mirror,
        ) || now
            .properties
            .get(mirror)
            .is_none_or(|map| now.properties.get("TextureContent") == Some(map));
        mirrored
            && !matches!(now.class_name.as_str(), "PackageLink" | "Camera")
            && was
                .properties
                .keys()
                .all(|name| now.properties.contains_key(name))
            && now.properties.iter().all(|(name, value)| {
                was.properties.get(name) == Some(value) || {
                    let logical = crate::rbx::encode::rbx_logical_property_name(
                        database,
                        &now.class_name,
                        name,
                    )
                    .unwrap_or(name);
                    let root = if now.parent_index.is_none() {
                        std::slice::from_ref(&now.name)
                    } else {
                        &[]
                    };
                    !logical.eq_ignore_ascii_case("Source")
                        && !logical.eq_ignore_ascii_case(
                            crate::editor::paths::SOURCE_PATH_CONTEXT_PROPERTY,
                        )
                        && !is_externally_managed_editor_property(
                            &now.name,
                            &now.class_name,
                            root,
                            logical,
                        )
                        && !crate::editor::review::is_engine_managed_editor_property(
                            &now.class_name,
                            logical,
                            database,
                        )
                }
            })
    }))
}

#[cfg(any(windows, target_os = "macos"))]
fn committed_import_proof(
    context: &BoundContext,
    summary: &Map<String, Value>,
    paths: &HashSet<PathBuf>,
    services: &[String],
    native: bool,
) -> Option<Value> {
    let verified = ["nativeVerifiedServices", "fieldVerifiedServices"]
        .into_iter()
        .filter_map(|key| summary.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let covered = services_for_snapshot_paths(context, paths)
        .iter()
        .all(|service| verified.contains(service.as_str()));
    let proof = summary
        .get("verifiedPushProof")
        .filter(|value| !value.is_null())
        .cloned()
        .filter(|_| {
            covered && summary.get("sourceVerifyFailed").and_then(Value::as_u64) == Some(0)
        });
    if !native {
        return proof;
    }
    if proof.is_some() {
        log_native_settlement_fallbacks(services, &verified);
    } else {
        log_global(
            5,
            format_args!(
                "[renium] import settlement stays synchronous: incomplete commit proof or receipt coverage"
            ),
        );
    }
    proof
}

fn log_native_settlement_fallbacks(services: &[String], verified: &HashSet<&str>) {
    for service in services {
        let reason = if verified.contains(service.as_str()) {
            "loader receipt proves creation and applied fields, but does not attest every accepted saved value or generated identity"
        } else {
            "service has no complete native loader verification receipt"
        };
        log_global(
            5,
            format_args!(
                "[renium] native import settlement: service={service} full_readback=true reason={reason}"
            ),
        );
    }
}

fn capture_push_readback(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    src_dir: &Path,
    requires_stage: bool,
    import_readback: Option<ExportProjectStage>,
    allow_memory_settle: bool,
) -> Result<(ExportProjectStage, ProjectSnapshot, bool)> {
    let settle_import = import_readback.is_some();
    let root = Path::new(&context.root);
    let stage = if let Some(stage) = import_readback {
        stage
    } else if requires_stage {
        ExportProjectStage::create(root, src_dir, services)?
    } else {
        ExportProjectStage::create_for_comparison(root, src_dir, services)?
    };
    if (!settle_import || allow_memory_settle)
        && !requires_stage
        && stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty())
    {
        let snapshot =
            capture_studio_services_in_memory(context, bridge, services, &stage, settle_import)?;
        Ok((stage, snapshot, settle_import))
    } else {
        let (stage, snapshot) =
            capture_studio_services_with_stage(context, bridge, services, settle_import, stage)?;
        Ok((stage, snapshot, false))
    }
}

/// Writes `readback` over the stage, whose copy of the project's `files`
/// under its publish paths is all it held besides what the capture staged.
pub(super) fn stage_verified_import_readback<'a>(
    stage: &mut ExportProjectStage,
    readback: &ProjectSnapshot,
    files: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<()> {
    let scopes = ScopeSet::new(stage.publish_paths());
    let mut paths = project_replacement_paths(readback, &ProjectSnapshot::default());
    paths.extend(
        files
            .into_iter()
            .filter(|path| scopes.contains(path) && !derived_project_path(path))
            .cloned(),
    );
    apply_snapshot_paths(&stage.project_root, &paths, readback)?;
    // Capture already built the map from its hierarchy. Rebuilding it from
    // stores here would decode the whole place again.
    stage.finish_projection(false)?;
    stage.mark_settings_aligned();
    Ok(())
}

/// The publication baseline of `paths` from `files`, a read of the paths
/// `covered` holds. Paths that read did not cover, or reached through a
/// link, are hashed from disk.
pub(super) fn publish_baseline(
    root: &Path,
    files: &ProjectSnapshot,
    covered: &ScopeSet,
    paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, PublishEntryState>> {
    let (read, unread): (Vec<_>, Vec<_>) = paths.iter().cloned().partition(|path| {
        !derived_project_path(path)
            && covered.contains(path)
            && path.ancestors().skip(1).all(|ancestor| {
                !matches!(
                    files.entries.get(ancestor),
                    Some(SnapshotEntry::Symlink { .. })
                )
            })
    });
    let mut baseline = crate::snapshot::export::collect_publish_hashes(root, &unread)?;
    let read = ScopeSet::new(&read);
    let hashed = files
        .entries
        .par_iter()
        .filter(|(path, _)| read.contains(path))
        .map(|(path, entry)| (path.clone(), entry_publish_state(entry)))
        .collect::<Vec<_>>();
    baseline.extend(hashed);
    Ok(baseline)
}

pub(super) fn entry_publish_state(entry: &SnapshotEntry) -> PublishEntryState {
    match entry {
        SnapshotEntry::Directory => PublishEntryState::Directory,
        SnapshotEntry::File(bytes) => crate::snapshot::export::file_publish_state(bytes),
        SnapshotEntry::Symlink { target, .. } => PublishEntryState::Symlink(target.clone()),
    }
}

/// Writes a verified import's state to the files, which `current` has as
/// the push left them, and refreshes its undo entry; None when the files
/// are not that state alone. Studio holds the import by now, so a file
/// saved while it was read back or written is newer work and stays: the
/// next pull brings the import's identities into the files. A publication
/// that could not put back all it moved aside is an error.
fn settle_native_import_files(
    stage: ExportProjectStage,
    (root, paths): (&Path, &[PathBuf]),
    history: &mut history::SyncHistory,
    current: &mut ProjectSnapshot,
    summary: &mut Map<String, Value>,
) -> Result<Option<crate::snapshot::export::PublishedProjectChanges>> {
    match publish_native_import_readback(stage, root, (paths, current)) {
        Ok((files, published)) => {
            *current = files;
            let id = history.commit(current, &ProjectSnapshot::default())?;
            summary.insert("historyId".into(), Value::String(id));
            Ok(Some(published))
        }
        Err(error) if error.is::<crate::snapshot::export::IncompleteRollback>() => Err(error),
        Err(error) => {
            log_global(
                3,
                format_args!("[renium] the files are not the import's state alone: {error:#}"),
            );
            Ok(None)
        }
    }
}

/// Publishes the stage over the files `held` has under `paths`. Fails where
/// a file it left alone is no longer as `held` has it: saved meanwhile, it
/// belongs to no baseline of what Studio holds.
fn publish_native_import_readback(
    stage: ExportProjectStage,
    root: &Path,
    (paths, held): (&[PathBuf], &ProjectSnapshot),
) -> Result<(
    ProjectSnapshot,
    crate::snapshot::export::PublishedProjectChanges,
)> {
    let (published, _) = crate::snapshot::export::finish_export_publication(
        Some(stage),
        root,
        false,
        None,
        || Ok(()),
        || Ok(()),
    )
    .context("the import's state was not written")?;
    let current = capture_snapshot(root, paths)?;
    history::ensure_published_state(&published, &current)?;
    let written = ScopeSet::new(&published.changed_roots);
    let saved = held
        .entries
        .keys()
        .chain(current.entries.keys())
        .find(|path| {
            !written.contains(path)
                && !derived_project_path(path)
                && held.entries.get(*path) != current.entries.get(*path)
        });
    if let Some(path) = saved {
        bail!(
            "{} was saved while the import's state was written",
            path.display()
        );
    }
    Ok((current, published))
}

#[test]
fn native_import_readback_publishes_once_and_protects_newer_files() -> Result<()> {
    let root = crate::tests::support::temp_dir("native-import-readback");
    let _cleanup = OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let document = |identity: &str| {
        let mut instance = SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        );
        instance.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": identity}),
        );
        encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![instance],
        })
        .unwrap()
    };
    let original = document("00000000000000000000000000000001");
    let accepted = document("00000000000000000000000000000002");
    fs::create_dir_all(root.join(path.parent().unwrap()))?;
    fs::write(root.join(&path), &original)?;
    let prepare = || -> Result<ExportProjectStage> {
        let stage = ExportProjectStage::create(&root, Path::new("src"), &["Workspace".into()])?;
        fs::write(stage.project_root.join(&path), &accepted)?;
        Ok(stage)
    };
    let mut stage = prepare()?;
    crate::project::sourcemap::generate_project_sourcemap(&stage.project_root)?;
    let paths = stage.publish_paths().to_vec();
    let checked = capture_snapshot(&stage.project_root, &paths)?;
    stage_verified_import_readback(&mut stage, &checked, checked.entries.keys())?;
    let map = fs::read(stage.project_root.join("sourcemap.json"))?;
    assert!(!map.is_empty(), "The verified capture must publish its map");
    let held = capture_snapshot(&root, &paths)?;
    let (after, _) = publish_native_import_readback(stage, &root, (&paths, &held))?;
    assert_eq!(fs::read(root.join("sourcemap.json"))?, map);
    assert!(after.entries[&path] == SnapshotEntry::File(accepted.clone()));
    let modified = fs::metadata(root.join(&path))?.modified()?;
    assert!(publish_native_import_readback(prepare()?, &root, (&paths, &after))?.0 == after);
    assert_eq!(fs::metadata(root.join(&path))?.modified()?, modified);
    let mut stage = prepare()?;
    stage_verified_import_readback(&mut stage, &checked, checked.entries.keys())?;
    fs::write(root.join(&path), &original)?;
    assert!(publish_native_import_readback(stage, &root, (&paths, &after)).is_err());
    assert_eq!(fs::read(root.join(&path))?, original);
    // A file the publication leaves alone, saved once the push had read the
    // files and before the stage did, is in no baseline the stage checks.
    let held = capture_snapshot(&root, &paths)?;
    let outside = PathBuf::from("src/Workspace/Saved.luau");
    fs::write(root.join(&outside), "saved meanwhile")?;
    let mut stage = prepare()?;
    stage_verified_import_readback(&mut stage, &checked, checked.entries.keys())?;
    let error = publish_native_import_readback(stage, &root, (&paths, &held))
        .err()
        .context("a file saved outside the written paths went unseen")?;
    assert!(
        format!("{error:#}").contains("Saved.luau was saved while"),
        "{error:#}"
    );
    assert_eq!(fs::read(root.join(path))?, accepted);
    assert_eq!(fs::read_to_string(root.join(outside))?, "saved meanwhile");
    Ok(())
}

/// A synchronous settlement whose publication cannot put back all it moved
/// aside fails and names the stage that holds the files; one that files
/// saved meanwhile stopped, and that put everything back, settles nothing.
#[test]
fn a_synchronous_settlement_fails_on_an_incomplete_rollback() -> Result<()> {
    use crate::snapshot::export::{IncompleteRollback, PublicationStep, hook_publication_steps};
    for incomplete in [false, true] {
        let root = crate::tests::support::temp_dir("native-import-rollback");
        let first = Path::new("src/Workspace/A.luau");
        let second = Path::new("src/Workspace/B.luau");
        fs::create_dir_all(root.join(first.parent().unwrap()))?;
        for path in [first, second, Path::new("src/Workspace/Kept.luau")] {
            fs::write(root.join(path), "pushed")?;
        }
        let stage = ExportProjectStage::create(&root, Path::new("src"), &["Workspace".into()])?;
        for path in [first, second] {
            fs::write(stage.project_root.join(path), "accepted")?;
        }
        let container = stage.container().to_path_buf();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
            let _ = fs::remove_dir_all(&container);
        });
        let paths = stage.publish_paths().to_vec();
        let mut current = capture_snapshot(&root, &paths)?;
        let mut history =
            history::SyncHistory::begin(&root, &root.join("src"), &current, &HashSet::new())?;
        let (saved, moved_aside) = (root.join(second), container.join("previous").join(first));
        let hook = hook_publication_steps(move |step| {
            if *step == PublicationStep::MoveAside(second.into()) {
                fs::write(&saved, "saved meanwhile").unwrap();
                if incomplete {
                    fs::remove_file(&moved_aside).unwrap();
                }
            }
        });
        let settled = settle_native_import_files(
            stage,
            (&root, &paths),
            &mut history,
            &mut current,
            &mut Map::new(),
        );
        drop(hook);
        assert_eq!(fs::read_to_string(root.join(second))?, "saved meanwhile");
        if !incomplete {
            assert!(settled?.is_none());
            assert_eq!(fs::read_to_string(root.join(first))?, "pushed");
            continue;
        }
        let error = settled.err().context("the push answered success")?;
        assert!(error.is::<IncompleteRollback>(), "{error:#}");
        assert!(
            format!("{error:#}").contains(&container.display().to_string()),
            "{error:#}"
        );
        assert!(container.join("previous").exists());
    }
    Ok(())
}

pub(crate) fn reopened_push_context(
    context: &BoundContext,
    summary: &Map<String, Value>,
) -> Result<Option<BoundContext>> {
    let Some(reopened) = summary.get("protectedOfflineApply") else {
        return Ok(None);
    };
    let runtime = crate::editor::review::snapshot_replacement_runtime(
        context.runtime_id.as_deref(),
        reopened,
    )?;
    let mut replacement = context.clone();
    replacement.runtime_id = Some(runtime.to_owned());
    Ok(Some(replacement))
}

pub(crate) fn exact_source_push_verified(
    summary: &Map<String, Value>,
    verification_paths: &HashSet<PathBuf>,
) -> bool {
    !verification_paths.is_empty()
        && verification_paths.iter().all(|path| is_source_path(path))
        && summary.get("sourceVerifyFailed").and_then(Value::as_u64) == Some(0)
        && summary.get("sourceVerified").and_then(Value::as_u64)
            == u64::try_from(verification_paths.len()).ok()
}

pub(crate) fn service_has_only_native_containers(document: &SettingsBytecode) -> bool {
    document
        .instances
        .iter()
        .enumerate()
        .all(|(index, instance)| {
            let Some(parent) = instance.parent_index else {
                return true;
            };
            if is_reconciliation_protected_workspace_camera(document, index) {
                return true;
            }
            let root = &document.instances[parent];
            root.parent_index.is_none()
                && crate::roblox::services::is_engine_managed_container(
                    &root.class_name,
                    &instance.class_name,
                )
        })
}

pub(crate) fn capture_studio_services(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    generate_sourcemap: bool,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    let src_dir = bound_context::source_dir(context)?;
    let stage = ExportProjectStage::create(Path::new(&context.root), &src_dir, services)?;
    capture_studio_services_with_stage(context, bridge, services, generate_sourcemap, stage)
}

pub(crate) fn capture_studio_services_with_stage(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    generate_sourcemap: bool,
    stage: ExportProjectStage,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    #[cfg(all(test, any(windows, target_os = "macos")))]
    if let Some(snapshot) = super::push_flow_tests::capture(context, services, &stage.project_root)
    {
        return Ok((stage, snapshot?));
    }
    let stage =
        import_studio_services_into_stage(context, bridge, services, generate_sourcemap, stage)?;
    let snapshot = capture_snapshot(&stage.project_root, stage.publish_paths())?;
    Ok((stage, snapshot))
}

pub(crate) fn import_studio_services_into_stage(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    generate_sourcemap: bool,
    stage: ExportProjectStage,
) -> Result<ExportProjectStage> {
    let capture_dir = create_unique_directory(
        &Path::new(&context.root).join(".renium"),
        "reconcile-capture-",
    )?;
    let cleanup_path = capture_dir.clone();
    let _cleanup = OnDrop::new(move || {
        let _ = fs::remove_dir_all(cleanup_path);
    });
    pin_edit_runtime(context, bridge)?;
    let mut args = automation_pull_args(context, &json!({ "services": services }))?;
    args.project_root.clone_from(&stage.import_project_root);
    args.src_dir.clone_from(&stage.import_src_dir);
    args.layout_src_root = stage.layout_src_root(Path::new(&context.root));
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    export_snapshots_with_warm_bridge(
        args,
        bridge,
        &info,
        0.0,
        false,
        crate::snapshot::export::ExportPurpose::Readback,
    )?;
    let regenerate_sourcemap = generate_sourcemap && stage.capture_sourcemap_needs_regeneration();
    stage.finish_projection(regenerate_sourcemap)?;
    Ok(stage)
}

pub(crate) fn capture_studio_services_in_memory(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    stage: &ExportProjectStage,
    preserve_saved_settings: bool,
) -> Result<ProjectSnapshot> {
    capture_studio_services_in_memory_options(
        context,
        bridge,
        services,
        stage,
        preserve_saved_settings,
        false,
    )
}

pub(crate) fn capture_studio_services_in_memory_options(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    stage: &ExportProjectStage,
    preserve_saved_settings: bool,
    known_changed: bool,
) -> Result<ProjectSnapshot> {
    #[cfg(all(test, any(windows, target_os = "macos")))]
    if let Some(result) = super::push_flow_tests::capture(context, services, &stage.project_root) {
        return result;
    }
    pin_edit_runtime(context, bridge)?;
    let mut args = automation_pull_args(context, &json!({"services": services}))?;
    args.project_root.clone_from(&stage.import_project_root);
    args.src_dir.clone_from(&stage.import_src_dir);
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    let src_root = stage.import_project_root.join(&stage.import_src_dir);
    let layout_src_root = stage.layout_src_root(Path::new(&context.root));
    let sourcemap = preserve_saved_settings.then(|| {
        crate::snapshot::import::SourcemapWriter::start(stage.project_root.clone(), false, None)
    });
    let projections = capture_exported_services(&args, bridge, &info, |service, state| {
        let preserved = if preserve_saved_settings {
            crate::snapshot::import::state_with_preserved_material_service_settings(
                service,
                &state,
                &service_settings_path(&src_root.join(service)),
            )?
        } else {
            None
        };
        let state = preserved.as_ref().unwrap_or(&state);
        let projection = crate::snapshot::import::service_projection_in_memory_options(
            state,
            &src_root,
            layout_src_root.as_deref(),
            service,
            known_changed,
        )?;
        if let Some(writer) = &sourcemap {
            stage_captured_service_map(stage, service, state, &projection, writer)?;
        }
        Ok(projection)
    })?;
    if let Some(writer) = sourcemap {
        writer.request_finish();
        writer.join()?;
    }
    let mut entries = BTreeMap::new();
    for projection in projections {
        for (path, bytes) in projection {
            let relative = path.strip_prefix(&stage.project_root)?.to_path_buf();
            if !derived_project_path(&relative) {
                let entry = bytes.map_or(SnapshotEntry::Directory, SnapshotEntry::File);
                anyhow::ensure!(
                    entries.insert(relative, entry).is_none(),
                    "Native verification projected overlapping paths"
                );
            }
        }
    }
    Ok(ProjectSnapshot { entries })
}

fn stage_captured_service_map(
    stage: &ExportProjectStage,
    service: &str,
    state: &crate::snapshot::types::ServiceState,
    projection: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    writer: &crate::snapshot::import::SourcemapWriter,
) -> Result<()> {
    // The existing map builder resolves script paths against files. Stage only
    // the captured sources here; verified stores are published after checking.
    for (path, bytes) in projection {
        if let Some(bytes) = bytes.as_ref().filter(|_| is_source_path(path)) {
            fs::create_dir_all(path.parent().context("Captured source has no parent")?)?;
            fs::write(path, bytes)?;
        }
    }
    let service_dir = stage
        .import_project_root
        .join(&stage.import_src_dir)
        .join(service);
    let node = crate::project::sourcemap::build_service_sourcemap_from_state(
        state,
        &stage.project_root,
        &service_dir,
    );
    writer
        .sender()
        .send(crate::snapshot::import::SourcemapWriterMessage::Service(
            service.to_string(),
            node,
        ))?;
    Ok(())
}

#[test]
fn captured_import_map_includes_new_sources_without_decoding_stores() -> Result<()> {
    use crate::snapshot::import::{SourcemapWriter, build_service_state_from_instances};
    use crate::snapshot::types::SnapshotInstance;
    let root = crate::tests::support::temp_dir("captured-import-map");
    let _cleanup = OnDrop::new(|| {
        let _ = fs::remove_dir_all(&root);
    });
    let service = "ReplicatedStorage";
    let stage = ExportProjectStage::create(&root, Path::new("src"), &[service.into()])?;
    let state = build_service_state_from_instances(
        service,
        None,
        vec![
            SnapshotInstance {
                name: service.into(),
                class_name: service.into(),
                instance_index: Some(1),
                ..Default::default()
            },
            SnapshotInstance {
                name: "Module".into(),
                class_name: "ModuleScript".into(),
                instance_index: Some(2),
                parent_index: Some(1),
                properties: Map::from_iter([("Source".into(), json!("return 7"))]),
                ..Default::default()
            },
        ],
        HashMap::new(),
        true,
    )?;
    let source = stage.import_project_root.join(&stage.import_src_dir);
    let projection = service_projection_in_memory(&state, &source, None, service)?;
    let writer = SourcemapWriter::start(stage.project_root.clone(), false, None);
    stage_captured_service_map(&stage, service, &state, &projection, &writer)?;
    writer.request_finish();
    writer.join()?;
    let map =
        crate::project::sourcemap::load_existing_sourcemap_root(&stage.project_root)?.unwrap();
    assert_eq!(
        map.children[0].children[0].file_paths,
        ["src/ReplicatedStorage/Module.luau"]
    );
    assert!(
        !service_settings_path(&source.join(service)).exists(),
        "The map must not decode a stored service"
    );
    Ok(())
}

pub(crate) fn services_for_snapshot_paths(
    context: &BoundContext,
    paths: &HashSet<PathBuf>,
) -> Vec<String> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source);
    let Some(source) = source.strip_prefix(root).ok() else {
        return sync_services();
    };
    let mut services = HashSet::new();
    let loaded = std::cell::OnceCell::new();
    for path in paths {
        if let Some(service) =
            crate::project::storage::service_for_store(Path::new(&context.source), &root.join(path))
        {
            services.insert(service);
            continue;
        }
        let Some(service) = path
            .strip_prefix(source)
            .ok()
            .and_then(|path| path.components().next())
            .and_then(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .and_then(|name| {
                DEFAULT_SYNC_SERVICES
                    .iter()
                    .find(|service| service.eq_ignore_ascii_case(name))
            })
        else {
            let Some(project) = loaded
                .get_or_init(|| config::load_project(Some(Path::new(&context.project)), None).ok())
            else {
                return sync_services();
            };
            let Ok(mapped) = config::project_source_to_staged_relatives(project, &root.join(path))
            else {
                return sync_services();
            };
            if mapped.is_empty() {
                return sync_services();
            }
            for mapped in mapped {
                let Some(service) = mapped
                    .components()
                    .next()
                    .and_then(|component| component.as_os_str().to_str())
                    .and_then(|name| {
                        DEFAULT_SYNC_SERVICES
                            .iter()
                            .find(|service| service.eq_ignore_ascii_case(name))
                    })
                else {
                    return sync_services();
                };
                services.insert((*service).to_string());
            }
            continue;
        };
        services.insert((*service).to_string());
    }
    let mut services = services.into_iter().collect::<Vec<_>>();
    services.sort();
    services
}

pub(crate) fn staged_context(
    context: &BoundContext,
    stage: &ExportProjectStage,
    source_root: &Path,
) -> Result<BoundContext> {
    let project_relative = Path::new(&context.project).strip_prefix(&context.root)?;
    let source_relative = project_relative_source_root(Path::new(&context.root), source_root)?;
    let mut staged = context.clone();
    staged.root = stage.project_root.display().to_string();
    staged.project = stage
        .project_root
        .join(project_relative)
        .display()
        .to_string();
    staged.source = stage
        .project_root
        .join(source_relative)
        .display()
        .to_string();
    Ok(staged)
}

pub(crate) fn project_relative_source_root(
    project_root: &Path,
    source_root: &Path,
) -> Result<PathBuf> {
    absolutize_under(project_root, source_root)
        .strip_prefix(project_root)
        .with_context(|| {
            format!(
                "Source root {} is outside project {}",
                source_root.display(),
                project_root.display()
            )
        })
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod review_hold_tests {
    use super::push_applied_nothing;
    use serde_json::{Map, Value, json};

    fn summary(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn a_held_or_skipped_push_applied_nothing() {
        assert!(push_applied_nothing(&summary(
            json!({ "ok": true, "heldForReview": true, "effectiveChanges": 8 })
        )));
        assert!(push_applied_nothing(&summary(
            json!({ "ok": true, "skippedByReview": true })
        )));
        assert!(!push_applied_nothing(&summary(
            json!({ "ok": true, "instances": { "created": 8 } })
        )));
        assert!(!push_applied_nothing(&summary(
            json!({ "ok": true, "heldForReview": false })
        )));
    }
}

#[cfg(test)]
mod stores_read_tests {
    use super::*;

    /// A push reads its stores by their changed instances only where no
    /// package is enforced and nothing but stores and scripts differs.
    #[test]
    fn only_a_push_of_stores_and_scripts_reads_stores_by_their_changed_instances() {
        let store = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let script = PathBuf::from("src/Workspace/Terrain.bin");
        let snapshot = |store_bytes: &[u8], source: &[u8]| ProjectSnapshot {
            entries: BTreeMap::from([
                (store.clone(), SnapshotEntry::File(store_bytes.to_vec())),
                (script.clone(), SnapshotEntry::File(source.to_vec())),
            ]),
        };
        let project = snapshot(b"files", b"print(1)");
        let paths = HashSet::from([store.clone(), script.clone()]);
        for (studio, unenforced, expected) in [
            (
                snapshot(b"studio", b"print(1)"),
                true,
                StoresRead::ChangedOfBoth,
            ),
            (snapshot(b"studio", b"print(1)"), false, StoresRead::Changed),
            (snapshot(b"studio", b"print(2)"), true, StoresRead::Changed),
            (snapshot(b"files", b"print(2)"), true, StoresRead::Changed),
        ] {
            assert!(stores_read_of(unenforced, &paths, &project, &studio) == expected);
        }
        // A file only one side holds differs.
        let mut studio = snapshot(b"studio", b"print(1)");
        studio.entries.remove(&script);
        assert!(stores_read_of(true, &paths, &project, &studio) == StoresRead::Changed);
        // A script that differs, or that one side alone holds, does not count.
        let source = PathBuf::from("src/Workspace/Main.server.luau");
        let mut project = snapshot(b"files", b"print(1)");
        let mut studio = snapshot(b"studio", b"print(1)");
        project
            .entries
            .insert(source.clone(), SnapshotEntry::File(b"print(2)".to_vec()));
        let paths = HashSet::from([store, script, source.clone()]);
        assert!(stores_read_of(true, &paths, &project, &studio) == StoresRead::ChangedOfBoth);
        studio
            .entries
            .insert(source, SnapshotEntry::File(b"print(3)".to_vec()));
        assert!(stores_read_of(true, &paths, &project, &studio) == StoresRead::ChangedOfBoth);
    }
}
