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
    Continue(Option<Box<VerifiedFullPushCandidate>>),
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn is_whole_project_push(
    services: &[String],
    push_args: &PushEditorChangesArgs,
    guard: Option<&StudioChangeGuard>,
    replace: bool,
) -> Result<bool> {
    if guard.is_some() {
        return Ok(false);
    }
    if replace {
        return Ok(true);
    }
    let all_services = sync_services();
    if services.len() != all_services.len()
        || !services
            .iter()
            .all(|service| all_services.contains(service))
        || !push_args.changed_paths.is_empty()
        || !push_args.changed_paths_files.is_empty()
        || !push_args.target_settings_ids.is_empty()
        || !push_args.target_settings_id_files.is_empty()
        || !push_args.target_properties.is_empty()
        || push_args.upsert_instances_only
    {
        return Ok(false);
    }
    crate::editor::sync::project_pushes_stores_as_held(push_args)
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn reuse_verified_subset_push(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: &PushEditorChangesArgs,
) -> Result<Option<Map<String, Value>>> {
    if services.is_empty()
        || !push_args.target_settings_ids.is_empty()
        || !push_args.target_settings_id_files.is_empty()
        || !push_args.target_properties.is_empty()
        || push_args.upsert_instances_only
    {
        return Ok(None);
    }
    let all_services = sync_services();
    if !services
        .iter()
        .all(|service| all_services.contains(service))
    {
        return Ok(None);
    }
    if !crate::editor::sync::project_pushes_stores_as_held(push_args).unwrap_or(false) {
        return Ok(None);
    }
    let Ok(plain_input) = plain_push_input(push_args) else {
        return Ok(None);
    };
    let Some(mut cached) = take_baseline(context, bridge, &plain_input) else {
        return Ok(None);
    };
    let check = (|| -> Result<bool> {
        let stage = ExportProjectStage::create_for_comparison(
            &push_args.project.project_root,
            &push_args.project.src_root,
            services,
        )?;
        let files_unchanged = snapshot_matches_disk_subset(
            &push_args.project.project_root,
            stage.publish_paths(),
            &cached.project,
        )?;
        Ok(files_unchanged && verified_full_push_proof_matches(context, bridge, &cached.proof)?)
    })();
    let reinsert_baseline = |cached: VerifiedFullPush| {
        if let Some(runtime) = context.runtime_id.as_deref() {
            bridge
                .verified_full_pushes
                .lock_recover()
                .insert(runtime.to_string(), cached);
        }
    };
    match check {
        Ok(true) => {
            log_global(
                5,
                format_args!("[renium] baseline: scoped service push unchanged"),
            );
            cached.renew();
            reinsert_baseline(cached);
            Ok(Some(Map::from_iter([
                ("ok".into(), json!(true)),
                ("unchanged".into(), json!(true)),
            ])))
        }
        Ok(false) => {
            reinsert_baseline(cached);
            Ok(None)
        }
        Err(error) => {
            log_global(
                5,
                format_args!("[renium] baseline: scoped service pre-check skipped: {error:#}"),
            );
            reinsert_baseline(cached);
            Ok(None)
        }
    }
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
    Ok(VerifiedFullPushReuse::Continue(candidate.map(Box::new)))
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

/// Splits prepared settings changes into documents for the staged push and
/// entries retained for post-push verification. Large v12 block stores that
/// are not sparse on both sides are verified row-by-row from raw bytes, so
/// their `previous` documents are dropped immediately and `desired` is not
/// pinned across mutation and readback.
#[allow(clippy::type_complexity)]
pub(super) fn split_prepared_settings_for_push(
    prepared_settings: HashMap<PathBuf, PreparedEditorSettingsChange>,
    project: &ProjectSnapshot,
) -> Result<(
    HashMap<String, Arc<SettingsBytecode>>,
    HashMap<PathBuf, PreparedPushVerification>,
)> {
    let mut prepared_documents = HashMap::with_capacity(prepared_settings.len());
    let mut prepared_verification = HashMap::with_capacity(prepared_settings.len());
    for (path, change) in prepared_settings {
        let service = settings_service_name(&path, &change.current, &change.previous)?;
        let desired = Arc::new(change.current);
        prepared_documents.insert(service, Arc::clone(&desired));
        let large_block_store = matches!(
            project.entries.get(&path),
            Some(SnapshotEntry::File(bytes))
                if crate::settings::bytecode::blocks::is_block_store(bytes)
                    && desired.instances.len() >= super::verify::rows::MIN_ROWS
        );
        let both_sparse = crate::settings::equivalence::holds_unread_instances(&desired)
            && crate::settings::equivalence::holds_unread_instances(&change.previous);
        let small_previous = change.previous.instances.len() < super::verify::rows::MIN_ROWS;
        if large_block_store && !both_sparse {
            if small_previous {
                prepared_verification.insert(
                    path,
                    PreparedPushVerification {
                        previous: change.previous,
                        desired: None,
                    },
                );
            } else {
                drop_settings_document(change.previous);
            }
        } else {
            prepared_verification.insert(
                path,
                PreparedPushVerification {
                    previous: change.previous,
                    desired: Some(desired),
                },
            );
        }
    }
    Ok((prepared_documents, prepared_verification))
}

/// The plan as it is carried out: one made from the changed instances of
/// both copies that the instances that differ do not carry out
/// (`plan_reads_changed_alone`) is made again from the whole copies of its
/// stores: of each of them, or for a move between stores of the ones where
/// an unread instance refers to what moved. `replace` says which comparison
/// prepared them.
pub(super) fn plan_from_whole_stores(
    (stores_read, replace, plan): (StoresRead, bool, ReconcilePushPlan),
    snapshots: (&ProjectSnapshot, &ProjectSnapshot),
    differences: &HashSet<PathBuf>,
    prepared_settings: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<ReconcilePushPlan> {
    plan_from_whole_stores_with(
        (stores_read, replace, true, plan),
        snapshots,
        differences,
        prepared_settings,
    )
}

pub(super) fn plan_from_whole_stores_with(
    (stores_read, replace, include_reconciled_services, plan): (
        StoresRead,
        bool,
        bool,
        ReconcilePushPlan,
    ),
    (project, studio): (&ProjectSnapshot, &ProjectSnapshot),
    differences: &HashSet<PathBuf>,
    prepared_settings: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<ReconcilePushPlan> {
    let has_unread_replacement = prepared_settings.iter().any(|(path, change)| {
        crate::settings::equivalence::holds_unread_instances(&change.current)
            && !crate::settings::equivalence::holds_unread_instances(&change.previous)
            && (!service_has_only_native_containers(&change.previous)
                || !settings_service_name(path, &change.current, &change.previous)
                    .ok()
                    .is_some_and(|service| plan.initial_native_services.contains(&service)))
    });
    let has_sparse_current = prepared_settings
        .values()
        .any(|change| crate::settings::equivalence::holds_unread_instances(&change.current));
    if ((stores_read != StoresRead::ChangedOfBoth && !has_sparse_current)
        || plan_reads_changed_alone(&plan))
        && !has_unread_replacement
    {
        return Ok(plan);
    }
    let moved = super::replacement::moved_between_stores(&plan);
    let mut read_whole = Vec::new();
    for (path, change) in prepared_settings.iter() {
        let unread = crate::settings::equivalence::holds_unread_instances(&change.current);
        let in_native = settings_service_name(path, &change.current, &change.previous)
            .ok()
            .is_some_and(|service| plan.initial_native_services.contains(&service));
        let whole = match &moved {
            Some(ids) => {
                unread
                    && super::replacement::unread_instance_refers(
                        project.entries.get(path),
                        &change.current,
                        ids,
                    )?
            }
            None if replace || in_native => !service_has_only_native_containers(&change.previous),
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
            include_reconciled_services,
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
        include_reconciled_services,
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
    let (captured_moved, _) = capture_studio_services_in_memory_options(
        context, bridge, moved, stage, false, true, None,
    )?;
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
    let has_changed_sources = paths.iter().any(|path| {
        super::merge::is_source_path(path) && project.entries.get(path) != studio.entries.get(path)
    });
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
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] delta bulk route ({service_name}): unchanged ({})",
                        path.display()
                    ),
                );
                non_bulk_project.entries.remove(path);
                non_bulk_studio.entries.remove(path);
            }
            BulkDiffOutcome::Patch(patch) => {
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] delta bulk route ({service_name}): bulk patch instances={} fields={}",
                        patch.instance_count, patch.field_count
                    ),
                );
                bulk_patches.insert(path.clone(), patch);
                non_bulk_project.entries.remove(path);
                non_bulk_studio.entries.remove(path);
            }
            BulkDiffOutcome::Hybrid(hybrid) => {
                let patch = hybrid.patch.clone();
                if let Some(prepared) = super::replacement::prepare_hybrid_bulk_delta(
                    desired_bytes,
                    observed_bytes,
                    *hybrid,
                    has_changed_sources,
                )? {
                    let has_delta =
                        !settings_documents_equivalent(&prepared.current, &prepared.previous);
                    crate::app::output::log_global(
                        4,
                        format_args!(
                            "[renium] delta bulk route ({service_name}): hybrid bulk_instances={} delta={has_delta}",
                            patch.instance_count
                        ),
                    );
                    bulk_patches.insert(path.clone(), patch);
                    if has_delta {
                        prepared_settings.insert(path.clone(), prepared);
                    }
                    non_bulk_project.entries.remove(path);
                    non_bulk_studio.entries.remove(path);
                } else {
                    crate::app::output::log_global(
                        4,
                        format_args!(
                            "[renium] delta bulk route ({service_name}): hybrid rejected, falling back to full diff"
                        ),
                    );
                }
            }
            BulkDiffOutcome::Fallback => {
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] delta bulk route ({service_name}): fallback to full diff"
                    ),
                );
            }
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

fn drop_promoted_bulk_patches(
    bulk_patches: &mut HashMap<PathBuf, crate::editor::types::EditorBulkPropertyPatch>,
    hybrid_sparse_paths: &HashSet<PathBuf>,
    prepared_settings: &HashMap<PathBuf, PreparedEditorSettingsChange>,
) {
    bulk_patches.retain(|path, patch| {
        if hybrid_sparse_paths.contains(path)
            && prepared_settings.get(path).is_some_and(|change| {
                !crate::settings::equivalence::holds_unread_instances(&change.current)
            })
        {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] delta bulk route ({}): dropped bulk patch after whole-store re-plan",
                    patch.service
                ),
            );
            return false;
        }
        true
    });
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
        || !plan.extra_property_changes.is_empty()
        || plan.bulk_property_patches.is_empty()
        || plan
            .bulk_property_patches
            .iter()
            .any(|patch| !patch.package_targets.is_empty())
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
pub(super) struct ZeroCaptureContext<'a> {
    pub(super) context: &'a BoundContext,
    pub(super) bridge: &'a BridgeServer,
    pub(super) services: &'a [String],
    pub(super) root: &'a Path,
    pub(super) src_dir: &'a Path,
    pub(super) source_paths: &'a [PathBuf],
    pub(super) stage: &'a ExportProjectStage,
    pub(super) guard: &'a StudioChangeGuard,
    pub(super) push_args: &'a PushEditorChangesArgs,
    pub(super) initial_state: &'a Value,
    pub(super) cache_input: Option<&'a Value>,
}

#[cfg(any(windows, target_os = "macos"))]
struct ExtractedZeroCapture {
    plan: ReconcilePushPlan,
    prepared_documents: HashMap<String, Arc<SettingsBytecode>>,
    service_rows: HashMap<String, (PathBuf, Vec<usize>)>,
    terrain_restore: Option<(PathBuf, usize)>,
    extra_before: BTreeMap<PathBuf, SnapshotEntry>,
    extra_mutation_paths: HashSet<PathBuf>,
}

#[cfg(any(windows, target_os = "macos"))]
fn merge_zero_capture_scoped_services(
    zctx: &ZeroCaptureContext<'_>,
    held_project: &ProjectSnapshot,
    capture_services: &[String],
    plan: &mut ReconcilePushPlan,
    prepared_documents: &mut HashMap<String, Arc<SettingsBytecode>>,
    extra_before: &mut BTreeMap<PathBuf, SnapshotEntry>,
    extra_mutation_paths: &mut HashSet<PathBuf>,
) -> Result<bool> {
    let source = Path::new(&zctx.context.source)
        .strip_prefix(zctx.root)
        .context("Project source is outside its root")?;
    let mut scopes = Vec::with_capacity(capture_services.len() * 2);
    for service in capture_services {
        scopes.push(source.join(service));
        scopes.push(
            service_settings_path(&Path::new(&zctx.context.source).join(service))
                .strip_prefix(zctx.root)
                .context("Scoped push capture store is outside its project root")?
                .to_path_buf(),
        );
    }
    let scope_set = ScopeSet::new(&scopes);
    let scoped_desired = ProjectSnapshot {
        entries: held_project
            .entries
            .iter()
            .filter(|(path, _)| scope_set.contains(path))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect(),
    };
    let (scoped_studio, _) = capture_studio_services_in_memory_options(
        zctx.context,
        zctx.bridge,
        capture_services,
        zctx.stage,
        false,
        true,
        None,
    )?;
    let mut prepared_settings = HashMap::new();
    let (diff_paths, promoted, stores_read) = prepare_replace_push_differences(
        zctx.context,
        zctx.root,
        (&scoped_desired, &scoped_studio),
        (false, true),
        &mut prepared_settings,
        &mut HashMap::new(),
    )?;
    if !promoted.is_empty() {
        return Ok(false);
    }
    if diff_paths.is_empty() {
        return Ok(true);
    }
    let mut scoped_plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &scoped_studio,
        &scoped_desired,
        &diff_paths,
        &prepared_settings,
        false,
        true,
    )?;
    scoped_plan = plan_from_whole_stores(
        (stores_read, true, scoped_plan),
        (&scoped_desired, &scoped_studio),
        &diff_paths,
        &mut prepared_settings,
    )?;
    if !scoped_plan.initial_native_services.is_empty() {
        return Ok(false);
    }
    let _ = stage_replace_push_paths(
        zctx.root,
        &zctx.stage.project_root,
        Path::new(&zctx.context.source),
        &scoped_plan,
        &prepared_settings,
        &scoped_desired,
        &scoped_studio,
    )?;
    for (path, change) in prepared_settings {
        let svc = settings_service_name(&path, &change.current, &change.previous)?;
        prepared_documents.insert(svc, Arc::new(change.current));
    }
    for path in &diff_paths {
        if let Some(entry) = scoped_studio.entries.get(path) {
            extra_before.insert(path.clone(), entry.clone());
        }
        extra_mutation_paths.insert(path.clone());
    }
    let bulk_paths = std::mem::take(&mut plan.bulk_property_paths);
    let bulk_patches = std::mem::take(&mut plan.bulk_property_patches);
    let extra_props = std::mem::take(&mut plan.extra_property_changes);
    *plan = scoped_plan;
    plan.bulk_property_paths = bulk_paths;
    plan.bulk_property_patches = bulk_patches;
    plan.extra_property_changes.extend(extra_props);
    Ok(true)
}

#[cfg(any(windows, target_os = "macos"))]
fn plan_zero_capture_bulk_restore(
    zctx: &ZeroCaptureContext<'_>,
    held_project: &ProjectSnapshot,
) -> Result<Option<ExtractedZeroCapture>> {
    use crate::settings::bytecode::blocks::bulk_diff::{
        BulkExtractOutcome, extract_v12_bulk_properties_for_classes,
        extract_v12_workspace_terrain_voxels,
    };
    let Some(scope) = ProofBulkScope::parse(&zctx.initial_state["pushProofScope"]) else {
        log_global(
            5,
            format_args!("[renium] zero-capture skipped: pushProofScope unparseable or unknown"),
        );
        return Ok(None);
    };
    if scope.services.is_empty() {
        return Ok(None);
    }
    let service_set: HashSet<&str> = zctx.services.iter().map(String::as_str).collect();
    if scope
        .services
        .iter()
        .any(|svc| !service_set.contains(svc.as_str()))
    {
        return Ok(None);
    }
    if scope.services.contains("Workspace")
        && !scope.terrain_differs
        && scope.expected_terrain_digest.is_none()
    {
        log_global(
            5,
            format_args!("[renium] zero-capture skipped: missing expectedTerrainDigest"),
        );
        return Ok(None);
    }
    let stores = crate::project::structural::service_store_paths(Path::new(&zctx.context.source))?;
    let mut plan = ReconcilePushPlan::default();
    let mut service_rows = HashMap::new();
    let mut terrain_restore = None;
    let mut capture_services = Vec::new();
    let mut total_instances = 0_usize;
    for service in &scope.services {
        let can_try_bulk = scope.values_only.contains(service)
            && scope.changed_class_properties.contains_key(service);
        let has_terrain_class_prop = service == "Workspace"
            && scope
                .changed_class_properties
                .get(service)
                .is_some_and(|pairs| pairs.iter().any(|(cls, _)| cls == "Terrain"));
        let restore_terrain = service == "Workspace"
            && scope.terrain_differs
            && scope.values_only.contains(service)
            && !has_terrain_class_prop
            && (can_try_bulk || scope.terrain_only_workspace);
        let store_entry = stores
            .get(service)
            .and_then(|store_abs| store_abs.strip_prefix(zctx.root).ok())
            .and_then(|store_rel| {
                if let Some(SnapshotEntry::File(desired_bytes)) =
                    held_project.entries.get(store_rel)
                {
                    Some((store_rel, desired_bytes))
                } else {
                    None
                }
            });
        let is_heavy = store_entry.is_some_and(|(_, bytes)| bytes.len() >= 256 * 1024);
        if (can_try_bulk || restore_terrain)
            && let Some((store_rel, desired_bytes)) = store_entry
        {
            let extracted_terrain = if restore_terrain {
                match extract_v12_workspace_terrain_voxels(desired_bytes)? {
                    Some(terrain) => Some(terrain),
                    None => {
                        log_global(
                            5,
                            format_args!(
                                "[renium] zero-capture skipped (Workspace): terrain row unavailable"
                            ),
                        );
                        return Ok(None);
                    }
                }
            } else {
                None
            };
            let mut handled = false;
            if can_try_bulk {
                let class_props = &scope.changed_class_properties[service];
                let phase = Instant::now();
                let outcome = extract_v12_bulk_properties_for_classes(
                    desired_bytes,
                    service,
                    class_props,
                    1,
                )?;
                if let BulkExtractOutcome::Patch(mut extracted) = outcome {
                    log_reconcile_timing(
                        &format!(
                            "zero-capture bulk extract ({service}, {} instances, {} fields)",
                            extracted.patch.instance_count, extracted.patch.field_count
                        ),
                        phase,
                    );
                    total_instances += extracted.patch.instance_count;
                    if service == "Workspace" && extracted_terrain.is_none() {
                        extracted.patch.expected_terrain_digest =
                            scope.expected_terrain_digest.clone();
                    }
                    extracted.patch.return_old_values = true;
                    plan.bulk_property_paths.push(store_rel.to_path_buf());
                    plan.bulk_property_patches.push(extracted.patch);
                    service_rows.insert(
                        service.clone(),
                        (store_rel.to_path_buf(), extracted.node_to_row),
                    );
                    handled = true;
                }
            } else if extracted_terrain.is_some() {
                handled = true;
            }
            if handled {
                if let Some(terrain) = extracted_terrain {
                    let mut properties = Map::new();
                    properties.insert("SmoothGrid".to_string(), terrain.smooth_grid);
                    properties.insert("PhysicsGrid".to_string(), terrain.physics_grid);
                    plan.extra_property_changes.push(EditorPropertyChange {
                        service: "Workspace".to_string(),
                        settings_id: terrain.settings_id,
                        path_segments: terrain.path_segments,
                        path_ordinals: terrain.path_ordinals,
                        class_name: "Terrain".to_string(),
                        properties,
                        ..EditorPropertyChange::default()
                    });
                    plan.capture_before_terrain = true;
                    terrain_restore = Some((store_rel.to_path_buf(), terrain.row_index));
                    total_instances += 1;
                }
                continue;
            }
        }
        if is_heavy {
            log_global(
                5,
                format_args!(
                    "[renium] zero-capture skipped ({service}): values_only={} has_class_props={} terrain_differs={} terrain_only={}",
                    scope.values_only.contains(service),
                    scope.changed_class_properties.contains_key(service),
                    scope.terrain_differs,
                    scope.terrain_only_workspace,
                ),
            );
            return Ok(None);
        }
        capture_services.push(service.clone());
    }
    if total_instances == 0 {
        return Ok(None);
    }
    let mut prepared_documents = HashMap::new();
    let mut extra_before = BTreeMap::new();
    let mut extra_mutation_paths = HashSet::new();
    if !capture_services.is_empty()
        && !merge_zero_capture_scoped_services(
            zctx,
            held_project,
            &capture_services,
            &mut plan,
            &mut prepared_documents,
            &mut extra_before,
            &mut extra_mutation_paths,
        )?
    {
        return Ok(None);
    }
    Ok(Some(ExtractedZeroCapture {
        plan,
        prepared_documents,
        service_rows,
        terrain_restore,
        extra_before,
        extra_mutation_paths,
    }))
}

#[cfg(any(windows, target_os = "macos"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkSourcemapPolicy {
    Deny,
    MetadataOnly,
    RequireExactRetained,
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn bulk_patches_sourcemap_policy(
    patches: &[crate::editor::types::EditorBulkPropertyPatch],
) -> BulkSourcemapPolicy {
    if patches.is_empty() {
        return BulkSourcemapPolicy::Deny;
    }
    let mut has_property = false;
    for patch in patches {
        for col in &patch.columns {
            match col.kind {
                crate::editor::types::BulkColumnKind::Attribute
                | crate::editor::types::BulkColumnKind::AttributesReplace
                | crate::editor::types::BulkColumnKind::Tag
                | crate::editor::types::BulkColumnKind::AssertModelScale
                | crate::editor::types::BulkColumnKind::AssertModelPivot => {}
                crate::editor::types::BulkColumnKind::Property => {
                    if matches!(
                        col.property_name.as_str(),
                        "CFrame" | "Size" | "Position" | "Orientation" | "Rotation"
                    ) {
                        return BulkSourcemapPolicy::Deny;
                    }
                    has_property = true;
                }
            }
        }
    }
    if has_property {
        BulkSourcemapPolicy::RequireExactRetained
    } else {
        BulkSourcemapPolicy::MetadataOnly
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn bulk_patches_allow_preserved_sourcemap(
    policy: BulkSourcemapPolicy,
    summary: &Map<String, Value>,
) -> bool {
    match policy {
        BulkSourcemapPolicy::Deny => false,
        BulkSourcemapPolicy::MetadataOnly => {
            summary.get("exactRetained") != Some(&Value::Bool(false))
        }
        BulkSourcemapPolicy::RequireExactRetained => {
            summary.get("exactRetained") == Some(&Value::Bool(true))
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn is_bulk_only_values_plan(plan: &ReconcilePushPlan) -> bool {
    !plan.bulk_property_patches.is_empty()
        && plan.changed_paths.is_empty()
        && plan.target_settings_ids.is_empty()
        && plan.initial_native_services.is_empty()
        && plan.extra_property_changes.is_empty()
        && plan.instance_deletes.is_empty()
        && plan.property_removals.is_empty()
}

#[cfg(any(windows, target_os = "macos"))]
fn summary_has_pending_or_unsupported(summary: &Map<String, Value>) -> bool {
    summary.contains_key("protectedPending")
        || summary.contains_key("supplementalImportPendingWrites")
        || summary.contains_key("unsupportedProperties")
}

#[cfg(any(windows, target_os = "macos"))]
fn reconstruct_zero_capture_before(
    held_project: &ProjectSnapshot,
    service_rows: &HashMap<String, (PathBuf, Vec<usize>)>,
    old_columns_map: &Map<String, Value>,
    terrain_restore: Option<&(PathBuf, usize)>,
    before_terrain_grids: Option<&Value>,
    extra_before: BTreeMap<PathBuf, SnapshotEntry>,
    extra_mutation_paths: HashSet<PathBuf>,
) -> Result<(ProjectSnapshot, HashSet<PathBuf>)> {
    use crate::settings::bytecode::blocks::bulk_diff::{
        patch_v12_bulk_old_value_columns, patch_v12_terrain_voxels,
    };
    let phase = Instant::now();
    let mut before_entries = extra_before;
    let mut mutation_paths = extra_mutation_paths;
    for (service, (store_rel, node_to_row)) in service_rows {
        let Some(SnapshotEntry::File(desired_bytes)) = held_project.entries.get(store_rel) else {
            continue;
        };
        let before_bytes = match old_columns_map.get(service).and_then(Value::as_array) {
            Some(cols) if !cols.is_empty() => {
                patch_v12_bulk_old_value_columns(desired_bytes, cols, node_to_row)?
            }
            _ => desired_bytes.clone(),
        };
        before_entries.insert(store_rel.clone(), SnapshotEntry::File(before_bytes));
        mutation_paths.insert(store_rel.clone());
    }
    if let Some((store_rel, terrain_row)) = terrain_restore {
        let base_bytes = match before_entries.get(store_rel) {
            Some(SnapshotEntry::File(bytes)) => Some(bytes.as_slice()),
            _ => match held_project.entries.get(store_rel) {
                Some(SnapshotEntry::File(bytes)) => Some(bytes.as_slice()),
                _ => None,
            },
        };
        if let Some(base_bytes) = base_bytes {
            let smooth = before_terrain_grids
                .and_then(|grids| grids.get("SmoothGrid"))
                .cloned();
            let physics = before_terrain_grids
                .and_then(|grids| grids.get("PhysicsGrid"))
                .cloned();
            let patched = patch_v12_terrain_voxels(base_bytes, *terrain_row, smooth, physics)?;
            before_entries.insert(store_rel.clone(), SnapshotEntry::File(patched));
            mutation_paths.insert(store_rel.clone());
        }
    }
    log_reconcile_timing("zero-capture history reconstruct", phase);
    Ok((
        ProjectSnapshot {
            entries: before_entries,
        },
        mutation_paths,
    ))
}

pub(super) fn verified_push_services(summary: &Map<String, Value>) -> Option<HashSet<&str>> {
    if summary.get("ok") == Some(&Value::Bool(false))
        || summary.get("verified") == Some(&Value::Bool(false))
        || summary.get("sourceVerifyFailed").and_then(Value::as_u64) != Some(0)
        || summary_has_pending_or_unsupported(summary)
        || summary
            .get("supplementalImportPendingFields")
            .and_then(Value::as_array)
            .is_some_and(|arr| !arr.is_empty())
    {
        return None;
    }
    Some(
        ["nativeVerifiedServices", "fieldVerifiedServices"]
            .into_iter()
            .filter_map(|name| summary.get(name).and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str)
            .collect(),
    )
}

#[cfg(any(windows, target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn zero_capture_verification_paths(
    context: &BoundContext,
    summary: &Map<String, Value>,
    generated_paths: &HashSet<PathBuf>,
    mutation_paths: &HashSet<PathBuf>,
    current: Option<&ProjectSnapshot>,
    expected: Option<&ProjectSnapshot>,
    has_adapters: bool,
    replacement: bool,
) -> HashSet<PathBuf> {
    let mut verification_paths = mutation_paths.clone();
    verification_paths.extend(generated_paths.iter().cloned());
    if verification_paths.is_empty() || replacement || has_adapters {
        return verification_paths;
    }
    let Some(verified_services) = verified_push_services(summary) else {
        return verification_paths;
    };
    if !verified_services.is_empty() {
        verification_paths.retain(|path| {
            let service = crate::editor::paths::service_from_changed_path(
                Path::new(&context.source),
                &Path::new(&context.root).join(path),
            );
            let service_verified = service.is_some_and(|service| {
                verified_services
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&service))
            });
            let disk_diverged = match (current, expected) {
                (Some(cur), Some(exp)) => {
                    !entries_equivalent(path, exp.entries.get(path), cur.entries.get(path))
                }
                _ => false,
            };
            !service_verified || generated_paths.contains(path) || disk_diverged
        });
    }
    if !verification_paths.is_empty()
        && generated_paths.is_empty()
        && exact_source_push_verified(summary, &verification_paths)
    {
        verification_paths.clear();
    }
    verification_paths
}

#[cfg(any(windows, target_os = "macos"))]
fn try_zero_capture_if_eligible(
    flags: [bool; 5],
    zctx: &ZeroCaptureContext<'_>,
    scoped_baseline: &mut Option<ProjectSnapshot>,
    held_sourcemap_fallback: &mut Option<Box<SourcemapState>>,
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] native_relay: &mut Option<NativeRelay>,
) -> Result<Option<Map<String, Value>>> {
    if !flags.into_iter().all(|flag| flag) || scoped_baseline.is_none() {
        return Ok(None);
    }
    try_zero_capture_bulk_restore(
        zctx,
        scoped_baseline,
        held_sourcemap_fallback,
        tracking_release,
        #[cfg(windows)]
        native_relay,
    )
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn try_zero_capture_bulk_restore(
    zctx: &ZeroCaptureContext<'_>,
    scoped_baseline: &mut Option<ProjectSnapshot>,
    held_sourcemap_fallback: &mut Option<Box<SourcemapState>>,
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] native_relay: &mut Option<NativeRelay>,
) -> Result<Option<Map<String, Value>>> {
    if !(zctx.push_args.no_review || zctx.push_args.yes || crate::app::output::global_yes()) {
        return Ok(None);
    }
    let Some(held_project) = scoped_baseline.as_ref() else {
        return Ok(None);
    };
    let Some(extracted) = plan_zero_capture_bulk_restore(zctx, held_project)? else {
        return Ok(None);
    };
    let sourcemap_policy =
        if extracted.plan.bulk_property_patches.is_empty() && extracted.terrain_restore.is_some() {
            BulkSourcemapPolicy::MetadataOnly
        } else {
            bulk_patches_sourcemap_policy(&extracted.plan.bulk_property_patches)
        };
    let phase = Instant::now();
    let mut args = zctx.push_args.clone();
    args.inline_release_tracking = if extracted.extra_mutation_paths.is_empty() {
        bulk_inline_release_tracking(
            &extracted.plan,
            zctx.services,
            zctx.guard,
            tracking_release,
            false,
            false,
        )
    } else {
        None
    };
    let mut pushed = push_staged_project(
        zctx.context,
        zctx.stage,
        zctx.bridge,
        StagedPushRequest {
            plan: extracted.plan,
            prepared_documents: extracted.prepared_documents,
            virtual_files: None,
            guard: Some(zctx.guard),
            args,
            expected_project: Some(held_project),
            later_edits_follow: false,
        },
    )?;
    if pushed.summary.get("fallbackFullCapture") == Some(&Value::Bool(true)) {
        let reason = pushed
            .summary
            .get("fallbackReason")
            .and_then(Value::as_str)
            .unwrap_or("");
        log_global(
            5,
            format_args!(
                "[renium] zero-capture bulk restore fell back to full capture (reason={reason})"
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
    let before_terrain_grids = pushed.summary.remove("_beforeTerrainGrids");
    let updated = pushed
        .summary
        .get("propertyUpdated")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let any_mutated = updated > 0.0
        || !extracted.extra_mutation_paths.is_empty()
        || extracted.terrain_restore.is_some();
    let has_adapters = zctx
        .stage
        .loaded
        .as_ref()
        .is_some_and(|loaded| !loaded.project.adapters.is_empty());
    let pending_fields: Vec<crate::editor::types::PendingImportFields> = pushed
        .summary
        .get("supplementalImportPendingFields")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let has_pending_or_unsupported = !pending_fields.is_empty()
        || pushed.summary.contains_key("protectedPending")
        || pushed
            .summary
            .contains_key("supplementalImportPendingWrites")
        || pushed.summary.contains_key("unsupportedProperties");
    let generated_paths: HashSet<PathBuf> = pushed.generated.entries.keys().cloned().collect();
    let has_extra_mutations = !extracted.extra_mutation_paths.is_empty();
    let (disk_check, before_and_history) = std::thread::scope(|scope| {
        let disk_check = scope
            .spawn(|| snapshot_matches_disk(zctx.root, zctx.stage.publish_paths(), held_project));
        let before_and_history = reconstruct_zero_capture_before(
            held_project,
            &extracted.service_rows,
            &old_columns,
            extracted.terrain_restore.as_ref(),
            before_terrain_grids.as_ref(),
            extracted.extra_before,
            extracted.extra_mutation_paths,
        )
        .and_then(|(before, mutation_paths)| {
            let history_id = if any_mutated {
                let mut history =
                    history::SyncHistory::begin(zctx.root, zctx.src_dir, &before, &mutation_paths)?;
                Some(history.commit(held_project, &pushed.generated)?)
            } else {
                None
            };
            Ok((before, mutation_paths, history_id))
        });
        (
            disk_check
                .join()
                .expect("Post-push snapshot check worker panicked"),
            before_and_history,
        )
    });
    let (before_snapshot, all_mutation_paths, history_id) = before_and_history?;
    if let Some(history_id) = history_id {
        pushed
            .summary
            .insert("historyId".into(), Value::String(history_id));
    }
    log_reconcile_timing("full push mutation", phase);
    if pushed
        .summary
        .get("protectedOfflineApply")
        .is_some_and(|receipt| receipt["recoveryPending"] == true)
    {
        pushed.summary.insert("verified".into(), Value::Bool(false));
        return Ok(Some(pushed.summary));
    }
    let replacement = reopened_push_context(zctx.context, &pushed.summary)?;
    let _replacement_selection = replacement.as_ref().map(bound_context::select);
    let active_context = replacement.as_ref().unwrap_or(zctx.context);
    let mut active_guard = zctx.guard.clone();
    if replacement.is_some() {
        tracking_release.guard_id = None;
        pin_edit_runtime(active_context, zctx.bridge)?;
        active_guard = current_studio_change_guard(active_context, zctx.bridge)?;
        tracking_release
            .runtime_id
            .clone_from(&active_guard.runtime_id);
        tracking_release
            .guard_id
            .clone_from(&active_guard.tracking_guard_id);
    }
    let disk_matches = disk_check?;
    let initial_verification_paths = zero_capture_verification_paths(
        active_context,
        &pushed.summary,
        &generated_paths,
        &all_mutation_paths,
        None,
        None,
        has_adapters,
        replacement.is_some(),
    );
    if !initial_verification_paths.is_empty()
        || !generated_paths.is_empty()
        || (!disk_matches && (has_extra_mutations || has_pending_or_unsupported))
    {
        let current = capture_snapshot(zctx.root, zctx.stage.publish_paths())?;
        let verification_paths = zero_capture_verification_paths(
            active_context,
            &pushed.summary,
            &generated_paths,
            &all_mutation_paths,
            Some(&current),
            Some(held_project),
            has_adapters,
            replacement.is_some(),
        );
        if !verification_paths.is_empty() {
            let verification_services =
                services_for_snapshot_paths(active_context, &verification_paths);
            let (_readback_stage, mut readback, _) = capture_push_readback(
                active_context,
                zctx.bridge,
                &verification_services,
                zctx.src_dir,
                false,
                None,
                true,
            )?;
            let mut prepared_verification = HashMap::new();
            let (mismatches, details) = verify::verify_committed_push_readback(
                &before_snapshot,
                &current,
                &mut readback,
                &verification_paths,
                &mut prepared_verification,
                verify::Verification::of(
                    false,
                    &pending_fields,
                    &verify::NewInstanceValues::of(
                        zctx.bridge,
                        active_context.runtime_id.as_deref(),
                    ),
                ),
            )?;
            if !mismatches.is_empty() {
                return Err(retention_failure(
                    "pushed project changes",
                    &mismatches,
                    details.as_deref(),
                ));
            }
        }
        if has_pending_or_unsupported {
            pushed.summary.insert("verified".into(), Value::Bool(false));
            pushed.summary.remove("verifiedPushProof");
            tracking_release.proof = None;
            if tracking_release.guard_id.is_some() {
                acknowledge_verified_push(
                    zctx.bridge,
                    zctx.services,
                    &active_guard,
                    tracking_release,
                )?;
            }
            return Ok(Some(pushed.summary));
        }
        let mut expected_after_push = held_project.clone();
        expected_after_push
            .entries
            .extend(pushed.generated.entries.clone());
        let files_held = expected_after_push == current;
        if files_held && replacement.is_none() {
            tracking_release.proof = pushed.summary.remove("verifiedPushProof");
        } else {
            tracking_release.retained_proof = None;
        }
        if tracking_release.guard_id.is_some() {
            acknowledge_verified_push(zctx.bridge, zctx.services, &active_guard, tracking_release)?;
        }
        retain_verified_full_push(
            zctx.bridge,
            zctx.cache_input,
            zctx.source_paths,
            current,
            tracking_release,
            #[cfg(windows)]
            native_relay,
        );
        return Ok(Some(pushed.summary));
    }
    if disk_matches {
        tracking_release.proof = pushed.summary.remove("verifiedPushProof");
    } else {
        tracking_release.retained_proof = None;
    }
    if tracking_release.guard_id.is_some() {
        acknowledge_verified_push(zctx.bridge, zctx.services, &active_guard, tracking_release)?;
    }
    let retained_project = scoped_baseline.take().expect("scoped baseline held");
    let can_keep_pull_scope = zero_capture_can_keep_pull_scope(
        disk_matches,
        has_extra_mutations,
        generated_paths.is_empty(),
        replacement.is_none(),
        has_pending_or_unsupported,
        &pushed.summary,
    );
    let preserved_sourcemap = preserved_sourcemap_for_bulk_push(
        can_keep_pull_scope,
        sourcemap_policy,
        &pushed.summary,
        held_sourcemap_fallback,
    );
    let mut pushed_services: BTreeSet<String> = extracted.service_rows.keys().cloned().collect();
    if extracted.terrain_restore.is_some() {
        pushed_services.insert("Workspace".to_string());
    }
    let retained_pushed_from_pull =
        if preserved_sourcemap.is_none() && can_keep_pull_scope && !pushed_services.is_empty() {
            held_sourcemap_fallback.take().map(|sm| {
                Box::new(PushedFromPull {
                    sourcemap: *sm,
                    services: pushed_services,
                    paths_moved: BTreeSet::new(),
                    structural: BTreeMap::new(),
                })
            })
        } else {
            None
        };
    retain_full_push_scoped(
        zctx.bridge,
        zctx.cache_input,
        zctx.source_paths,
        RetainedPushScope {
            project: retained_project,
            sourcemap: preserved_sourcemap,
            pushed_from_pull: retained_pushed_from_pull,
        },
        tracking_release,
        #[cfg(windows)]
        native_relay,
    );
    Ok(Some(pushed.summary))
}

fn prepare_replace_push_differences(
    context: &BoundContext,
    root: &Path,
    (project, studio): (&ProjectSnapshot, &ProjectSnapshot),
    (bulk_eligible, unenforced): (bool, bool),
    prepared_settings: &mut HashMap<PathBuf, PreparedEditorSettingsChange>,
    bulk_patches: &mut HashMap<PathBuf, crate::editor::types::EditorBulkPropertyPatch>,
) -> Result<(HashSet<PathBuf>, HashSet<String>, StoresRead)> {
    let paths = project_replacement_paths(project, studio);
    let stores_read = stores_read_of(unenforced, &paths, project, studio);
    let imported = ImportedStores::default();
    let collected_bulk = BulkPropertyPatches::default();
    let imports_heavy =
        super::staging::packing_link_enforcement(root, Path::new(&context.source))?.is_none();
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
        prepare_project_replacement_importing(
            project,
            studio,
            &paths,
            prepared_settings,
            stores_read,
            imports_heavy.then_some(&imported),
        )?
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
    prepared_settings.retain(|path, _| diff_paths.contains(path));
    let promoted_services = imported_services(imported, prepared_settings)?;
    Ok((diff_paths, promoted_services, stores_read))
}

fn target_subtrees_contain_scripts(
    document: &SettingsBytecode,
    target_ids: &HashSet<String>,
) -> bool {
    if target_ids.is_empty() {
        return document
            .instances
            .iter()
            .any(|inst| crate::editor::paths::script_file_names(&inst.class_name).is_some());
    }
    let mut children_by_parent: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent) = instance.parent_index {
            children_by_parent.entry(parent).or_default().push(index);
        }
        if target_ids.contains(&instance.settings_id) {
            roots.push(index);
        }
    }
    let mut stack = roots;
    let mut visited = vec![false; document.instances.len()];
    while let Some(index) = stack.pop() {
        if visited[index] {
            continue;
        }
        visited[index] = true;
        if crate::editor::paths::script_file_names(&document.instances[index].class_name).is_some()
        {
            return true;
        }
        if let Some(children) = children_by_parent.get(&index) {
            stack.extend(children.iter().copied());
        }
    }
    false
}

fn stage_replace_push_paths(
    root: &Path,
    stage_root: &Path,
    src_dir: &Path,
    plan: &ReconcilePushPlan,
    prepared_settings: &HashMap<PathBuf, PreparedEditorSettingsChange>,
    project: &ProjectSnapshot,
    studio: &ProjectSnapshot,
) -> Result<Option<Arc<crate::editor::types::VirtualStageFiles>>> {
    if plan.changed_paths.is_empty() {
        return Ok(None);
    }
    let has_symlinks = project
        .entries
        .values()
        .any(|entry| matches!(entry, SnapshotEntry::Symlink { .. }));
    let link_enforcement = if root.join("renium-link.json").exists() {
        Some(crate::project::package_links::build_link_enforcement(
            root, src_dir, None,
        )?)
    } else {
        None
    };
    if has_symlinks
        || crate::editor::sync::files_to_studio_filter_rules(root)?.is_some()
        || link_enforcement
            .as_ref()
            .is_some_and(|e| e.has_read_only_packages() || e.has_read_only_mirrors())
    {
        let all = project_replacement_paths(project, studio);
        stage_snapshot_paths(stage_root, &all, project)?;
        return Ok(None);
    }
    let src_rel = project_relative_source_root(root, src_dir)?;
    let mut needed: HashSet<PathBuf> = HashSet::new();
    for path in project.entries.keys() {
        let is_store = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(crate::system::files::is_service_settings_file_name);
        if is_store || !path.starts_with(&src_rel) {
            needed.insert(path.clone());
        }
    }
    let mut changed_dirs = Vec::new();
    for path in &plan.changed_paths {
        needed.insert(path.clone());
        if matches!(project.entries.get(path), Some(SnapshotEntry::Directory))
            || matches!(studio.entries.get(path), Some(SnapshotEntry::Directory))
        {
            changed_dirs.push(path.as_path());
        }
    }
    let mut full_script_services: HashSet<String> = plan.initial_native_services.clone();
    let changed_set: HashSet<&Path> = plan.changed_paths.iter().map(PathBuf::as_path).collect();
    let target_ids_set: HashSet<String> = plan.target_settings_ids.iter().cloned().collect();
    for (path, change) in prepared_settings {
        if !changed_set.contains(path.as_path()) {
            continue;
        }
        let service = settings_service_name(path, &change.current, &change.previous)?;
        if full_script_services.contains(&service) {
            continue;
        }
        if target_subtrees_contain_scripts(&change.current, &target_ids_set) {
            full_script_services.insert(service);
        }
    }
    if !changed_dirs.is_empty() || !full_script_services.is_empty() {
        let service_prefixes: Vec<PathBuf> = full_script_services
            .iter()
            .map(|service| src_rel.join(service))
            .collect();
        for path in project.entries.keys() {
            if changed_dirs.iter().any(|dir| path.starts_with(dir))
                || service_prefixes
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
            {
                needed.insert(path.clone());
            }
        }
    }
    if !plan.initial_native_services.is_empty() {
        let native_prefixes: Vec<PathBuf> = plan
            .initial_native_services
            .iter()
            .map(|service| src_rel.join(service))
            .collect();
        let mut disk_needed: HashSet<PathBuf> = HashSet::new();
        let mut virtual_files = crate::editor::types::VirtualStageFiles::new(stage_root);
        for path in needed {
            let is_store = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(crate::system::files::is_service_settings_file_name);
            let is_virtual_native = !is_store
                && native_prefixes
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
                && !matches!(
                    project.entries.get(&path),
                    Some(SnapshotEntry::Symlink { .. })
                );
            if is_virtual_native {
                match project.entries.get(&path) {
                    Some(SnapshotEntry::File(bytes)) => {
                        virtual_files.insert_file(&path, bytes.clone());
                    }
                    Some(SnapshotEntry::Directory) => {
                        virtual_files.insert_dir(&path);
                    }
                    _ => {
                        disk_needed.insert(path);
                    }
                }
            } else {
                disk_needed.insert(path);
            }
        }
        stage_snapshot_paths(stage_root, &disk_needed, project)?;
        if !virtual_files.is_empty() {
            return Ok(Some(Arc::new(virtual_files)));
        }
        return Ok(None);
    }
    stage_snapshot_paths(stage_root, &needed, project)?;
    Ok(None)
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
type HeldPullProof = (Box<SourcemapState>, Value);

#[cfg(any(windows, target_os = "macos"))]
fn merge_held_pull_touched(
    context: &BoundContext,
    held_pull: Option<HeldPullProof>,
    replace: bool,
    plan: &ReconcilePushPlan,
    prior_differing_services: Vec<String>,
) -> Option<(HeldPullProof, Vec<String>)> {
    held_pull.filter(|_| replace).map(|held| {
        let mut touched = written_services(context, plan);
        for service in prior_differing_services {
            if !touched.contains(&service) {
                touched.push(service);
            }
        }
        (held, touched)
    })
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

#[cfg(any(windows, target_os = "macos"))]
fn all_paths_in_verified_services(
    context: &BoundContext,
    differences: &HashSet<PathBuf>,
    mutation_paths: &HashSet<PathBuf>,
    verified_services: &HashSet<&str>,
) -> bool {
    !verified_services.is_empty()
        && differences.iter().chain(mutation_paths.iter()).all(|path| {
            crate::editor::paths::service_from_changed_path(
                Path::new(&context.source),
                &Path::new(&context.root).join(path),
            )
            .is_some_and(|service| {
                verified_services
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&service))
            })
        })
}

#[cfg(any(windows, target_os = "macos"))]
fn finish_fast_verified_push(
    bridge: &BridgeServer,
    services: &[String],
    guard: &StudioChangeGuard,
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
    prepared_verification: HashMap<PathBuf, PreparedPushVerification>,
    stage: ExportProjectStage,
) -> Result<()> {
    std::thread::scope(|scope| {
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
            acknowledge_verified_push(bridge, services, guard, tracking_release)
        } else {
            Ok(())
        }
    })
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) type HeldPullTuple = ((Box<SourcemapState>, Value), Vec<String>);

#[cfg(any(windows, target_os = "macos"))]
fn take_candidate_pull_scope(
    candidate: Option<&mut Box<VerifiedFullPushCandidate>>,
) -> (Option<Box<SourcemapState>>, Option<Box<PushedFromPull>>) {
    match candidate {
        Some(candidate) => (
            candidate.cached.sourcemap.take(),
            candidate.cached.pushed_from_pull.take(),
        ),
        None => (None, None),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn prior_scope_structural_services(
    scoped_moved: Option<&Vec<String>>,
    initial_state: &Value,
) -> (bool, BTreeSet<String>) {
    let Some(moved) = scoped_moved else {
        return (false, BTreeSet::new());
    };
    let structural = match ProofScope::parse(&initial_state["pushProofScope"]) {
        ProofScope::Services {
            services,
            touched,
            values_only,
        } => paths_moved_in_with(&services, &touched, &values_only),
        ProofScope::Unknown(_) => moved.iter().cloned().collect(),
    };
    (true, structural)
}

#[cfg(any(windows, target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_pushed_from_pull(
    context: &BoundContext,
    matched_prior: bool,
    pushed_services: BTreeSet<String>,
    prior_structural: BTreeSet<String>,
    studio: &ProjectSnapshot,
    project: &ProjectSnapshot,
    held_pull: Option<HeldPullTuple>,
    held_pushed_from_pull: Option<Box<PushedFromPull>>,
) -> Option<Box<PushedFromPull>> {
    if !matched_prior || pushed_services.is_empty() {
        return None;
    }
    let mut structural =
        structural_changes_between_snapshots(context, &pushed_services, studio, project, None);
    for service in prior_structural {
        structural.insert(service, vec![StructuralPathChange::WholeService]);
    }
    let paths_moved: BTreeSet<String> = structural.keys().cloned().collect();
    if let Some(((sm, _), _)) = held_pull {
        Some(Box::new(PushedFromPull {
            sourcemap: *sm,
            services: pushed_services,
            paths_moved,
            structural,
        }))
    } else if let Some(mut prior) = held_pushed_from_pull {
        prior.services.extend(pushed_services);
        prior.paths_moved.extend(paths_moved);
        merge_structural_changes(&mut prior.structural, structural);
        Some(prior)
    } else {
        None
    }
}

#[cfg(any(windows, target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn retain_push_project_scope(
    context: &BoundContext,
    bridge: &BridgeServer,
    cache_input: Option<&Value>,
    source_paths: &[PathBuf],
    (project, studio): (ProjectSnapshot, &ProjectSnapshot),
    (kept_sourcemap, matched_prior, pushed_services, prior_structural): (
        Option<Box<SourcemapState>>,
        bool,
        BTreeSet<String>,
        BTreeSet<String>,
    ),
    (held_pull, held_pushed_from_pull): (Option<HeldPullTuple>, Option<Box<PushedFromPull>>),
    tracking_release: &StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] native_relay: &mut Option<NativeRelay>,
) {
    let kept_pushed = if kept_sourcemap.is_some() {
        None
    } else {
        build_pushed_from_pull(
            context,
            matched_prior,
            pushed_services,
            prior_structural,
            studio,
            &project,
            held_pull,
            held_pushed_from_pull,
        )
    };
    retain_full_push_scoped(
        bridge,
        cache_input,
        source_paths,
        RetainedPushScope {
            project,
            sourcemap: kept_sourcemap,
            pushed_from_pull: kept_pushed,
        },
        tracking_release,
        #[cfg(windows)]
        native_relay,
    );
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn restore_held_baseline_on_noop(
    (bridge, services, guard): (&BridgeServer, &[String], &StudioChangeGuard),
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
    (initial_state, prior_proof, cache_input): (&Value, Option<Value>, Option<&Value>),
    source_paths: &[PathBuf],
    retained: RetainedPushScope,
    #[cfg(windows)] native_relay: &mut Option<NativeRelay>,
) {
    if initial_state["pushProofMatches"] == true
        && let Some(prior_proof) = prior_proof
        && cache_input.is_some()
    {
        tracking_release.proof = Some(prior_proof);
        if tracking_release.guard_id.is_some() {
            let _ = acknowledge_verified_push(bridge, services, guard, tracking_release);
        }
        retain_full_push_scoped(
            bridge,
            cache_input,
            source_paths,
            retained,
            tracking_release,
            #[cfg(windows)]
            native_relay,
        );
    }
}

struct BulkPushFallback<'a> {
    context: &'a BoundContext,
    stage: &'a ExportProjectStage,
    bridge: &'a BridgeServer,
    root: &'a Path,
    src_dir: &'a Path,
    project: &'a ProjectSnapshot,
    studio: &'a ProjectSnapshot,
    differences: &'a HashSet<PathBuf>,
    mutation_paths: &'a HashSet<PathBuf>,
    stores_read: StoresRead,
    replace: bool,
    guard: &'a StudioChangeGuard,
}

fn retry_full_push_without_bulk(
    input: BulkPushFallback<'_>,
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
    mut retry_args: PushEditorChangesArgs,
    reason: &str,
) -> Result<(
    history::SyncHistory,
    HashMap<PathBuf, PreparedPushVerification>,
    StagedPushResult,
)> {
    log_global(
        4,
        format_args!("[renium] full push bulk patch fell back to standard push (reason={reason})"),
    );
    let mut fallback_prepared = HashMap::new();
    let _ = prepare_project_replacement_from(
        input.project,
        input.studio,
        input.differences,
        &mut fallback_prepared,
        input.stores_read,
    )?;
    let fallback_plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        input.studio,
        input.project,
        input.differences,
        &fallback_prepared,
        true,
        false,
    )?;
    let fallback_plan = plan_from_whole_stores(
        (input.stores_read, input.replace, fallback_plan),
        (input.project, input.studio),
        input.differences,
        &mut fallback_prepared,
    )?;
    let history = history::SyncHistory::begin_push(
        input.root,
        input.src_dir,
        input.studio,
        input.mutation_paths,
        &mut retry_args,
    )?;
    let fallback_virtual_files = if input.replace {
        stage_replace_push_paths(
            input.root,
            &input.stage.project_root,
            input.src_dir,
            &fallback_plan,
            &fallback_prepared,
            input.project,
            input.studio,
        )?
    } else {
        apply_snapshot_paths(
            &input.stage.project_root,
            input.mutation_paths,
            input.project,
        )?;
        None
    };
    let (fallback_documents, fallback_verification) =
        split_prepared_settings_for_push(fallback_prepared, input.project)?;
    let mut pushed = push_staged_project(
        input.context,
        input.stage,
        input.bridge,
        StagedPushRequest {
            plan: fallback_plan,
            prepared_documents: fallback_documents,
            virtual_files: fallback_virtual_files,
            guard: Some(input.guard),
            args: retry_args,
            expected_project: Some(input.project),
            later_edits_follow: false,
        },
    )?;
    if let Some(release_result) = pushed.summary.remove("_inlineReleaseResult") {
        apply_release_tracking_result(input.guard, tracking_release, &release_result);
    }
    Ok((history, fallback_verification, pushed))
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
    let whole_project_push = is_whole_project_push(services, &push_args, guard, replace)?;
    #[cfg(any(windows, target_os = "macos"))]
    if !whole_project_push
        && guard.is_none()
        && let Some(result) = reuse_verified_subset_push(context, bridge, services, &push_args)?
    {
        return Ok(result);
    }
    #[cfg(any(windows, target_os = "macos"))]
    let mut verified_candidate = {
        match reuse_verified_full_push(context, bridge, services, &push_args, whole_project_push)? {
            VerifiedFullPushReuse::Unchanged(result) => return Ok(result),
            VerifiedFullPushReuse::Continue(candidate) => candidate,
        }
    };
    #[cfg(any(windows, target_os = "macos"))]
    let cache_input = whole_project_push
        .then(|| full_push_cache_input(&push_args, services))
        .transpose()?;
    #[cfg(windows)]
    let mut native_relay = None;
    // A baseline over files a pull wrote holds the sourcemap that pull left.
    #[cfg(any(windows, target_os = "macos"))]
    let (held_sourcemap, held_pushed_from_pull) =
        take_candidate_pull_scope(verified_candidate.as_mut());
    let phase = Instant::now();
    // The previous push's native attribute observer must outlive the proof
    // check inside the lease request, yet stop before the new lease arms its
    // own observer: stopping it earlier retires the proof, stopping it later
    // disarms the fresh observer.
    #[cfg(any(windows, target_os = "macos"))]
    let (candidate_parts, candidate_files_unchanged, mut previous_attributes) =
        match verified_candidate.map(|c| *c) {
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
    #[cfg(not(any(windows, target_os = "macos")))]
    let whole_project_push = replace;
    let (mut guard, initial_state) = if let Some(guard) = guard {
        (guard.clone(), Value::Null)
    } else {
        current_studio_change_guard_verifying_continuing(
            context,
            bridge,
            (cfg!(any(windows, target_os = "macos")) && whole_project_push).then_some(services),
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
    let prior_proof = candidate_proof.clone();
    #[cfg(any(windows, target_os = "macos"))]
    let (held_pull, mut held_sourcemap_fallback) = if initial_state["pushProofMatches"] == true {
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
        if let Some(summary) = try_zero_capture_if_eligible(
            [
                replace,
                bulk_eligible,
                verified_baseline.is_none(),
                candidate_files_unchanged,
                can_capture_in_memory,
            ],
            &zctx,
            &mut scoped_baseline,
            &mut held_sourcemap_fallback,
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
    let (had_scoped_moved, prior_structural_services) =
        prior_scope_structural_services(scoped_moved.as_ref(), &initial_state);
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
    let (captured, project) = if let Some(baseline) = verified_baseline {
        (
            Ok((stage, baseline)),
            capture_snapshot(&root, &source_paths),
        )
    } else {
        std::thread::scope(|scope| {
            let project = scope.spawn(|| capture_snapshot(&root, &source_paths));
            let captured = if let (Some(moved), Some(baseline)) =
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
        })
    };
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
    let externally_managed_only = prepared_settings
        .iter()
        .filter_map(|(path, change)| {
            super::verify::differs_only_in_externally_managed_fields(
                &change.current,
                &change.previous,
            )
            .then_some(path.clone())
        })
        .collect::<HashSet<_>>();
    prepared_settings.retain(|path, _| !externally_managed_only.contains(path));
    let differences = differences
        .into_iter()
        .filter(|path| bulk_patches.contains_key(path) || !externally_managed_only.contains(path))
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
        .filter(|path| !bulk_patches.contains_key(*path) || prepared_settings.contains_key(*path))
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
    let hybrid_sparse_paths = bulk_patches
        .keys()
        .filter(|path| {
            prepared_settings.get(*path).is_some_and(|change| {
                crate::settings::equivalence::holds_unread_instances(&change.current)
            })
        })
        .cloned()
        .collect::<HashSet<_>>();
    let mut plan = plan_from_whole_stores(
        (stores_read, replace, plan),
        (&project, &studio),
        &non_bulk_differences,
        &mut prepared_settings,
    )?;
    drop_promoted_bulk_patches(&mut bulk_patches, &hybrid_sparse_paths, &prepared_settings);
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
    #[cfg(any(windows, target_os = "macos"))]
    let pushed_services_for_scope: BTreeSet<String> =
        services_for_snapshot_paths(context, &differences)
            .into_iter()
            .chain(
                plan.bulk_property_patches
                    .iter()
                    .map(|patch| patch.service.clone()),
            )
            .chain(prior_differing_services.iter().cloned())
            .collect();
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
    let held_pull =
        merge_held_pull_touched(context, held_pull, replace, &plan, prior_differing_services);
    let virtual_files = if replace {
        stage_replace_push_paths(
            &root,
            &stage.project_root,
            &src_dir,
            &plan,
            &prepared_settings,
            &project,
            &studio,
        )?
    } else {
        apply_snapshot_paths(&stage.project_root, &mutation_paths, &project)?;
        None
    };
    let (prepared_documents, mut prepared_verification) =
        split_prepared_settings_for_push(prepared_settings, &project)?;
    if !plan.bulk_property_paths.is_empty() {
        let bulk_paths = plan.bulk_property_paths.iter().collect::<HashSet<_>>();
        prepared_verification.retain(|path, _| !bulk_paths.contains(path));
    }
    #[cfg(any(windows, target_os = "macos"))]
    let can_settle_in_background = replace && import_settle::can_continue(&stage, requires_stage);
    push_args.inline_release_tracking = bulk_inline_release_tracking(
        &plan,
        services,
        &guard,
        &tracking_release,
        requires_stage,
        link_enforcement.is_some(),
    );
    let saved_fields = push_args.saved_fields.clone();
    let had_bulk_patches = !plan.bulk_property_patches.is_empty();
    let mut fallback_push_args = had_bulk_patches.then(|| {
        let mut fallback = push_args.clone();
        fallback.inline_release_tracking = None;
        fallback
    });
    #[cfg(any(windows, target_os = "macos"))]
    let mut is_bulk_only_values_push = is_bulk_only_values_plan(&plan);
    #[cfg(any(windows, target_os = "macos"))]
    let sourcemap_policy = bulk_patches_sourcemap_policy(&plan.bulk_property_patches);
    let mut pushed = push_staged_project(
        context,
        &stage,
        bridge,
        StagedPushRequest {
            plan,
            prepared_documents,
            virtual_files,
            guard: Some(&guard),
            args: push_args,
            expected_project: Some(&project),
            later_edits_follow: false,
        },
    )?;
    if let Some(release_result) = pushed.summary.remove("_inlineReleaseResult") {
        apply_release_tracking_result(&guard, &mut tracking_release, &release_result);
    }
    if should_retry_full_push_without_bulk(&pushed.summary, had_bulk_patches)
        && let Some(retry_args) = fallback_push_args.take()
    {
        let reason = pushed
            .summary
            .get("fallbackReason")
            .and_then(Value::as_str)
            .unwrap_or("");
        #[cfg(any(windows, target_os = "macos"))]
        {
            is_bulk_only_values_push = false;
        }
        drop(history);
        (history, prepared_verification, pushed) = retry_full_push_without_bulk(
            BulkPushFallback {
                context,
                stage: &stage,
                bridge,
                root: &root,
                src_dir: &src_dir,
                project: &project,
                studio: &studio,
                differences: &differences,
                mutation_paths: &mutation_paths,
                stores_read,
                replace,
                guard: &guard,
            },
            &mut tracking_release,
            retry_args,
            reason,
        )?;
    }
    if push_applied_nothing(&pushed.summary) {
        // Studio holds or skipped the whole push, so there is nothing to
        // record or read back; the summary carries the hold and its next step.
        #[cfg(any(windows, target_os = "macos"))]
        restore_held_baseline_on_noop(
            (bridge, services, &guard),
            &mut tracking_release,
            (&initial_state, prior_proof, cache_input.as_ref()),
            &source_paths,
            RetainedPushScope {
                project: studio,
                sourcemap: held_pull.map(|((sm, _), _)| sm),
                pushed_from_pull: held_pushed_from_pull,
            },
            #[cfg(windows)]
            &mut native_relay,
        );
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

    #[cfg(any(windows, target_os = "macos"))]
    let narrowable = held_pull
        .as_ref()
        .filter(|(_, written)| services.iter().any(|service| !written.contains(service)));
    #[cfg(any(windows, target_os = "macos"))]
    let has_added_roots = !settle_native_import
        && pushed
            .summary
            .get("binaryInstances")
            .and_then(Value::as_f64)
            .is_some_and(|count| count > 0.0)
        && pushed
            .summary
            .get("nativeVerifiedServices")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
    #[cfg(any(windows, target_os = "macos"))]
    let narrow_from_pull = narrowable.filter(|((sourcemap, _), _)| {
        import_settle::holds_sourcemap(Path::new(&context.root), sourcemap)
    });
    #[cfg(any(windows, target_os = "macos"))]
    let settle_linked_added_roots =
        has_added_roots && link_enforcement.is_some() && !pushed_services_for_scope.is_empty();
    #[cfg(any(windows, target_os = "macos"))]
    let added_roots = (has_added_roots && narrow_from_pull.is_some()) || settle_linked_added_roots;
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
            let settled = if let Some(((_, held), written)) =
                narrowable.filter(|_| settle_native_import || narrow_from_pull.is_some())
            {
                settled_services(
                    context,
                    bridge,
                    (services, written.clone()),
                    held,
                    (&studio, &project, &pushed.generated),
                )
            } else if settle_linked_added_roots {
                pushed_services_for_scope.iter().cloned().collect()
            } else {
                services.to_vec()
            };
            (proof, settled)
        })
        // Added roots are settled only where that reads back less than a
        // pull would; their receipt already verified the push.
        .filter(|(_, settled)| {
            settle_native_import || settle_linked_added_roots || settled.len() < services.len()
        })
        // A Studio that changed since the commit gets the full readback below
        // before the push answers.
        .filter(|(proof, _)| {
            verified_full_push_proof_matches_with_options(context, bridge, proof, true)
                .unwrap_or(false)
        })
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
        let narrowed = (settled.len() < services.len()).then(|| import_settle::Narrowed {
            services: &settled,
            sourcemap: narrowable
                .filter(|_| settle_native_import || narrow_from_pull.is_some())
                .map(|((sourcemap, _), _)| &**sourcemap),
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

    let verified_services = ["nativeVerifiedServices", "fieldVerifiedServices"]
        .into_iter()
        .filter_map(|name| pushed.summary.get(name).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let allow_fast_verify_skip = !settle_native_import
        && pushed
            .summary
            .get("sourceVerifyFailed")
            .and_then(Value::as_u64)
            == Some(0);
    #[cfg(any(windows, target_os = "macos"))]
    let fast_verified_paths = allow_fast_verify_skip
        && !requires_stage
        && replacement.is_none()
        && pushed.generated.entries.is_empty()
        && pending_fields.is_empty()
        && !summary_has_pending_or_unsupported(&pushed.summary)
        && stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty())
        && all_paths_in_verified_services(
            context,
            &differences,
            &mutation_paths,
            &verified_services,
        );
    #[cfg(any(windows, target_os = "macos"))]
    if fast_verified_paths && snapshot_matches_disk(&root, stage.publish_paths(), &project)? {
        tracking_release.proof = pushed.summary.remove("verifiedPushProof");
        finish_fast_verified_push(
            bridge,
            services,
            &guard,
            &mut tracking_release,
            prepared_verification,
            stage,
        )?;
        let mut candidate_sourcemap = held_pull
            .as_ref()
            .map(|((sourcemap, _), _)| sourcemap.clone());
        let preserved_sourcemap = preserved_sourcemap_for_bulk_push(
            is_bulk_only_values_push,
            sourcemap_policy,
            &pushed.summary,
            &mut candidate_sourcemap,
        );
        let matched_prior = (initial_state["pushProofMatches"] == true || had_scoped_moved)
            && pushed.generated.entries.is_empty();
        retain_push_project_scope(
            context,
            bridge,
            cache_input.as_ref(),
            &source_paths,
            (project, &studio),
            (
                preserved_sourcemap,
                matched_prior,
                pushed_services_for_scope,
                prior_structural_services,
            ),
            (held_pull, held_pushed_from_pull),
            &tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        );
        return Ok(pushed.summary);
    }
    let mut current = capture_snapshot(&root, stage.publish_paths())?;
    let mut verification_paths = differences;
    verification_paths.extend(mutation_paths);
    verification_paths.extend(pushed.generated.entries.keys().cloned());
    // Native replacement validates the loader receipt, retained settings,
    // supplemental writes and script buffers before committing. Re-export only
    // services outside that contract, transformed outputs or newer file edits.
    if allow_fast_verify_skip
        && !requires_stage
        && replacement.is_none()
        && stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty())
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
        {
            let matched_prior = (initial_state["pushProofMatches"] == true || had_scoped_moved)
                && pushed.generated.entries.is_empty();
            retain_push_project_scope(
                context,
                bridge,
                cache_input.as_ref(),
                &source_paths,
                (project, &studio),
                (
                    None,
                    matched_prior,
                    pushed_services_for_scope,
                    prior_structural_services,
                ),
                (held_pull, held_pushed_from_pull),
                &tracking_release,
                #[cfg(windows)]
                &mut native_relay,
            );
        }
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
            let mut stage = ExportProjectStage::create_with_baseline_background(
                &root,
                &src_dir,
                services,
                &|paths| publish_baseline(&root, &current, &covered, paths),
            )?;
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
    #[cfg(any(windows, target_os = "macos"))]
    let no_generated = pushed.generated.entries.is_empty();
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
    {
        let kept_sourcemap = published.as_ref().and_then(import_settle::pulled_sourcemap);
        let matched_prior =
            (initial_state["pushProofMatches"] == true || had_scoped_moved) && no_generated;
        retain_push_project_scope(
            context,
            bridge,
            cache_input.as_ref(),
            &source_paths,
            (current, &studio),
            (
                kept_sourcemap,
                matched_prior,
                pushed_services_for_scope,
                prior_structural_services,
            ),
            (held_pull, held_pushed_from_pull),
            &tracking_release,
            #[cfg(windows)]
            &mut native_relay,
        );
    }
    Ok(pushed.summary)
}

/// A push of chosen paths or instances. With a held baseline it runs under a
/// lease that continues the baseline's observation, and keeps a baseline of
/// the files where it can say Studio holds them after the push.
#[cfg(any(windows, target_os = "macos"))]
pub(super) fn retain_targeted_baseline_scope(
    context: &BoundContext,
    held: &mut VerifiedFullPush,
    project: &ProjectSnapshot,
    summary: &Map<String, Value>,
) -> (Option<Box<SourcemapState>>, Option<Box<PushedFromPull>>) {
    let kept_sm = sourcemap_after_push(held, project, summary);
    let kept_pushed = if kept_sm.is_some() {
        None
    } else {
        let changed_paths: HashSet<PathBuf> = held
            .project
            .entries
            .keys()
            .chain(project.entries.keys())
            .filter(|path| held.project.entries.get(*path) != project.entries.get(*path))
            .cloned()
            .collect();
        let pushed_services: BTreeSet<String> =
            services_for_snapshot_paths(context, &changed_paths)
                .into_iter()
                .collect();
        if pushed_services.is_empty() {
            held.pushed_from_pull.take()
        } else {
            let structural = structural_changes_between_snapshots(
                context,
                &pushed_services,
                &held.project,
                project,
                None,
            );
            let paths_moved: BTreeSet<String> = structural.keys().cloned().collect();
            if let Some(sm) = held.sourcemap.take() {
                Some(Box::new(PushedFromPull {
                    sourcemap: *sm,
                    services: pushed_services,
                    paths_moved,
                    structural,
                }))
            } else if let Some(mut prior) = held.pushed_from_pull.take() {
                prior.services.extend(pushed_services);
                prior.paths_moved.extend(paths_moved);
                merge_structural_changes(&mut prior.structural, structural);
                Some(prior)
            } else {
                None
            }
        }
    };
    (kept_sm, kept_pushed)
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn preserved_sourcemap_for_bulk_push(
    allow: bool,
    policy: BulkSourcemapPolicy,
    summary: &Map<String, Value>,
    held: &mut Option<Box<SourcemapState>>,
) -> Option<Box<SourcemapState>> {
    if allow && bulk_patches_allow_preserved_sourcemap(policy, summary) {
        held.take()
    } else {
        None
    }
}

pub(super) fn should_retry_full_push_without_bulk(
    summary: &Map<String, Value>,
    had_bulk_patches: bool,
) -> bool {
    had_bulk_patches && summary.get("fallbackFullCapture") == Some(&Value::Bool(true))
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn zero_capture_can_keep_pull_scope(
    disk_matches: bool,
    has_extra_mutations: bool,
    generated_empty: bool,
    replacement_none: bool,
    has_pending_or_unsupported: bool,
    summary: &Map<String, Value>,
) -> bool {
    disk_matches
        && !has_extra_mutations
        && generated_empty
        && replacement_none
        && !has_pending_or_unsupported
        && summary.get("exactRetained") != Some(&Value::Bool(false))
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) fn targeted_push_keeps_baseline_on_structure_change(
    summary: &Map<String, Value>,
    pushed: &TargetedPush,
    held: &VerifiedFullPush,
) -> bool {
    !push_changed_structure(summary)
        || (push_only_created_instances(summary)
            && !pushed.ids.is_empty()
            && (held.sourcemap.is_some() || held.pushed_from_pull.is_some()))
}

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
    if !targeted_push_keeps_baseline_on_structure_change(&summary, &pushed, &held) {
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
    let (kept_sm, kept_pushed) =
        retain_targeted_baseline_scope(context, &mut held, &project, &summary);
    let entry = VerifiedFullPush {
        connections: bridge.runtime_connection_signature(&release.runtime_id),
        input: std::mem::take(&mut held.input),
        sourcemap: kept_sm,
        pushed_from_pull: kept_pushed,
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
            "lazyTracking": cfg!(windows),
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
pub(super) fn push_only_created_instances(summary: &Map<String, Value>) -> bool {
    let count = |top: &str, nested: &str| {
        summary
            .get(top)
            .and_then(Value::as_f64)
            .or_else(|| {
                summary
                    .get("instances")
                    .and_then(|v| v.get(nested))
                    .and_then(Value::as_f64)
            })
            .unwrap_or(0.0)
    };
    count("instanceCreated", "created") > 0.0
        && count("instanceDeleted", "deleted") == 0.0
        && count("instanceReplaced", "replaced") == 0.0
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
    if now.instances.len() > count
        && was.digests.len() == count
        && now.digests.len() == now.instances.len()
        && was.unique_ids.len() == count
        && now.unique_ids.len() == now.instances.len()
        && was.unique_ids[..] == now.unique_ids[..count]
        && was.digests[..] == now.digests[..count]
        && was
            .instances
            .iter()
            .zip(&now.instances[..count])
            .all(|(w, n)| {
                w.settings_id == n.settings_id
                    && w.name == n.name
                    && w.class_name == n.class_name
                    && w.parent_index == n.parent_index
            })
    {
        let total = now.instances.len();
        let mut chosen = now
            .instances
            .iter()
            .map(|instance| ids.contains(&instance.settings_id))
            .collect::<Vec<_>>();
        let mut joined = !exact;
        while joined {
            joined = false;
            for index in 0..total {
                if !chosen[index]
                    && now.instances[index]
                        .parent_index
                        .is_some_and(|parent| parent < total && chosen[parent])
                {
                    chosen[index] = true;
                    joined = true;
                }
            }
        }
        let added_ok = (count..total).all(|index| {
            let inst = &now.instances[index];
            chosen[index]
                && !matches!(inst.class_name.as_str(), "PackageLink" | "Camera")
                && crate::editor::paths::script_file_names(&inst.class_name).is_none()
        });
        return Ok(added_ok);
    }
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
    stage.wait_for_copy()?;
    let scopes = ScopeSet::new(stage.publish_paths());
    let mut paths = project_replacement_paths(readback, &ProjectSnapshot::default());
    paths.extend(
        files
            .into_iter()
            .filter(|path| scopes.contains(path) && !derived_project_path(path))
            .cloned(),
    );
    if let Some(vs) = stage.virtual_scripts().cloned() {
        let src_prefix = stage.import_src_dir.clone();
        let mut non_virtual_paths = HashSet::new();
        let mut virtual_dirs = BTreeSet::new();
        let mut virtual_files = Vec::new();
        for path in &paths {
            let is_store = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name);
            if !is_store
                && scopes.contains(path)
                && path != &src_prefix
                && path.starts_with(&src_prefix)
            {
                if let Ok(after_src) = path.strip_prefix(&src_prefix)
                    && let Some(first) = after_src.components().next()
                {
                    let service_root = src_prefix.join(first.as_os_str());
                    virtual_dirs.insert(service_root);
                }
                match readback.entries.get(path) {
                    Some(SnapshotEntry::Directory) => {
                        virtual_dirs.insert(path.clone());
                    }
                    Some(SnapshotEntry::File(bytes)) => {
                        virtual_files.push((path.clone(), bytes.as_slice()));
                    }
                    Some(SnapshotEntry::Symlink { .. }) => {
                        non_virtual_paths.insert(path.clone());
                    }
                    None => {}
                }
            } else {
                non_virtual_paths.insert(path.clone());
            }
        }
        {
            let mut roots = vs.virtual_roots.lock_recover();
            let mut staged_entries = vs.staged_entries.lock_recover();
            for dir in &virtual_dirs {
                if dir.parent() == Some(src_prefix.as_path()) {
                    roots.insert(dir.clone());
                }
                if vs.baseline.get(dir) != Some(&PublishEntryState::Directory) {
                    fs::create_dir_all(stage.project_root.join(dir))?;
                }
                staged_entries.insert(dir.clone(), PublishEntryState::Directory);
            }
        }
        let processed: Vec<(PathBuf, PublishEntryState, bool)> = virtual_files
            .par_iter()
            .map(
                |(rel, bytes)| -> Result<(PathBuf, PublishEntryState, bool)> {
                    let state = crate::snapshot::export::file_publish_state(bytes);
                    if vs.baseline.get(rel) == Some(&state) {
                        return Ok((rel.clone(), state, true));
                    }
                    let full = stage.project_root.join(rel);
                    if let Some(parent) = full.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&full, bytes)?;
                    Ok((rel.clone(), state, false))
                },
            )
            .collect::<Result<Vec<_>>>()?;
        {
            let mut staged_entries = vs.staged_entries.lock_recover();
            let mut skipped = vs.skipped_paths.lock_recover();
            for (rel, state, is_skipped) in processed {
                if is_skipped {
                    skipped.insert(rel.clone());
                }
                staged_entries.insert(rel, state);
            }
        }
        apply_snapshot_paths(&stage.project_root, &non_virtual_paths, readback)?;
    } else {
        apply_snapshot_paths(&stage.project_root, &paths, readback)?;
    }
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
        None,
    )
    .map(|(snapshot, _)| snapshot)
}

pub(crate) fn capture_studio_services_in_memory_options(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    stage: &ExportProjectStage,
    preserve_saved_settings: bool,
    known_changed: bool,
    document_below_rows: Option<usize>,
) -> Result<(ProjectSnapshot, HashMap<PathBuf, SettingsBytecode>)> {
    #[cfg(all(test, any(windows, target_os = "macos")))]
    if let Some(result) = super::push_flow_tests::capture(context, services, &stage.project_root) {
        return result.map(|snapshot| (snapshot, HashMap::new()));
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
        let (projection, settings_path, store) =
            crate::snapshot::import::service_projection_in_memory_options(
                state,
                &src_root,
                layout_src_root.as_deref(),
                service,
                known_changed,
                document_below_rows,
            )?;
        if let Some(writer) = &sourcemap {
            stage_captured_service_map(stage, service, state, &projection, writer)?;
        }
        Ok((projection, settings_path, store))
    })?;
    if let Some(writer) = sourcemap {
        writer.request_finish();
        writer.join()?;
    }
    let mut entries = BTreeMap::new();
    let mut documents = HashMap::new();
    for (projection, settings_path, store) in projections {
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
        let relative = settings_path
            .strip_prefix(&stage.project_root)?
            .to_path_buf();
        if !derived_project_path(&relative) {
            match store {
                crate::snapshot::import::CapturedStore::Bytes(bytes) => {
                    anyhow::ensure!(
                        entries
                            .insert(relative, SnapshotEntry::File(bytes))
                            .is_none(),
                        "Native verification projected overlapping paths"
                    );
                }
                crate::snapshot::import::CapturedStore::Document(document) => {
                    anyhow::ensure!(
                        !entries.contains_key(&relative)
                            && documents.insert(relative, document).is_none(),
                        "Native verification projected overlapping paths"
                    );
                }
            }
        }
    }
    Ok((ProjectSnapshot { entries }, documents))
}

fn stage_captured_service_map(
    stage: &ExportProjectStage,
    service: &str,
    state: &crate::snapshot::types::ServiceState,
    projection: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    writer: &crate::snapshot::import::SourcemapWriter,
) -> Result<()> {
    let existing_files: HashSet<PathBuf> = projection
        .iter()
        .filter_map(|(path, bytes)| {
            bytes
                .as_ref()
                .filter(|_| is_source_path(path))
                .and_then(|_| path.strip_prefix(&stage.project_root).ok())
                .map(Path::to_path_buf)
        })
        .collect();
    let service_dir = stage
        .import_project_root
        .join(&stage.import_src_dir)
        .join(service);
    let node = crate::project::sourcemap::build_service_sourcemap_from_state_with_files(
        state,
        &stage.project_root,
        &service_dir,
        &existing_files,
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

    #[test]
    fn target_subtrees_contain_scripts_uses_zero_based_parent_index_and_stages_scripts() {
        use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};
        let doc = SettingsBytecode {
            version: 12,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root-0".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "folder-1".into(),
                    "Sub".into(),
                    "Folder".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new(
                    "script-2".into(),
                    "Mod".into(),
                    "ModuleScript".into(),
                    Some(1),
                ),
            ],
        };
        assert!(
            target_subtrees_contain_scripts(&doc, &HashSet::from(["folder-1".to_string()])),
            "R2: 0-based parent_index=Some(1) script under folder-1 must be detected"
        );
        assert!(
            !target_subtrees_contain_scripts(&doc, &HashSet::from(["nonexistent".to_string()])),
            "unrelated target id should not match"
        );

        let root = crate::tests::support::temp_dir("r2-stage-replace-scripts");
        let stage_root = crate::tests::support::temp_dir("r2-stage-replace-out");
        let store_rel = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let script_rel = PathBuf::from("src/ReplicatedStorage/Sub/Mod.luau");
        let bytes = crate::settings::bytecode::encode_settings_bytecode(&doc).unwrap();
        let project = ProjectSnapshot {
            entries: BTreeMap::from([
                (store_rel.clone(), SnapshotEntry::File(bytes)),
                (
                    script_rel.clone(),
                    SnapshotEntry::File(b"return 1\n".to_vec()),
                ),
            ]),
        };
        let studio = ProjectSnapshot::default();
        let mut plan = ReconcilePushPlan::default();
        plan.changed_paths.push(store_rel.clone());
        plan.target_settings_ids.push("folder-1".into());
        let mut prepared = HashMap::new();
        prepared.insert(
            store_rel,
            PreparedEditorSettingsChange {
                previous: SettingsBytecode {
                    version: 12,
                    instances: vec![doc.instances[0].clone()],
                },
                current: doc,
            },
        );
        stage_replace_push_paths(
            &root,
            &stage_root,
            &root.join("src"),
            &plan,
            &prepared,
            &project,
            &studio,
        )
        .unwrap();
        assert!(
            stage_root.join(&script_rel).is_file(),
            "R2: stage_replace_push_paths must stage unchanged script under targeted subtree"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&stage_root);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn same_size_and_mtime_replacement_is_detected_by_snapshot_checks() {
        let root = crate::tests::support::temp_dir("reconcile1-same-mtime-size");
        let script_rel = PathBuf::from("src/ServerScriptService/Main.server.luau");
        let script_abs = root.join(&script_rel);
        fs::create_dir_all(script_abs.parent().unwrap()).unwrap();
        fs::write(&script_abs, "return 1\n").unwrap();
        let fixed_time = std::time::SystemTime::now() - Duration::from_secs(10);
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&script_abs)
            .unwrap();
        f.set_modified(fixed_time).unwrap();
        drop(f);

        let publish_paths = vec![PathBuf::from("src")];
        let held = capture_snapshot(&root, &publish_paths).unwrap();
        assert!(snapshot_matches_disk(&root, &publish_paths, &held).unwrap());
        assert!(
            snapshot_matches_disk_subset(&root, std::slice::from_ref(&script_rel), &held).unwrap()
        );
        assert!(
            snapshot_paths_match_disk(&root, std::slice::from_ref(&script_rel), &held)
                .unwrap()
                .is_none()
        );

        fs::write(&script_abs, "return 2\n").unwrap();
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&script_abs)
            .unwrap();
        f.set_modified(fixed_time).unwrap();
        drop(f);

        assert!(
            !snapshot_matches_disk(&root, &publish_paths, &held).unwrap(),
            "same-size same-mtime replacement must not match disk"
        );
        assert!(
            !snapshot_matches_disk_subset(&root, std::slice::from_ref(&script_rel), &held).unwrap(),
            "subset check must detect same-size same-mtime replacement"
        );
        assert_eq!(
            snapshot_paths_match_disk(&root, std::slice::from_ref(&script_rel), &held).unwrap(),
            Some(script_rel.clone()),
            "path check must report same-size same-mtime replacement"
        );
        let recaptured = capture_snapshot(&root, &publish_paths).unwrap();
        assert!(
            matches!(
                recaptured.entries.get(&script_rel),
                Some(SnapshotEntry::File(bytes)) if bytes == b"return 2\n"
            ),
            "capture_snapshot must read replaced bytes"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn bulk_patches_allow_preserved_sourcemap_fails_closed_on_spatial_or_unproved_properties() {
        use crate::editor::types::{
            BulkColumnKind, BulkPropertyColumn, BulkPropertyValues, EditorBulkPropertyPatch,
        };
        let make_patch = |kind: BulkColumnKind, prop: &str| EditorBulkPropertyPatch {
            service: "Workspace".into(),
            strings: Vec::new(),
            nodes: Vec::new(),
            columns: vec![BulkPropertyColumn {
                kind,
                property_name: prop.into(),
                rows: vec![1],
                values: BulkPropertyValues::F32Const(0.5),
            }],
            field_count: 1,
            instance_count: 1,
            expected_terrain_digest: None,
            return_old_values: false,
            package_targets: Vec::new(),
        };
        let mut proved = Map::new();
        proved.insert("exactRetained".into(), Value::Bool(true));
        let mut failed = Map::new();
        failed.insert("exactRetained".into(), Value::Bool(false));
        let empty = Map::new();

        for prop in ["CFrame", "Position", "Size", "Orientation", "Rotation"] {
            let policy =
                bulk_patches_sourcemap_policy(&[make_patch(BulkColumnKind::Property, prop)]);
            assert_eq!(policy, BulkSourcemapPolicy::Deny);
            assert!(
                !bulk_patches_allow_preserved_sourcemap(policy, &proved),
                "R8/Reconcile-6: spatial property {prop} must never preserve prior pull sourcemap"
            );
        }
        for prop in [
            "MaxValue",
            "MinValue",
            "MaxHealth",
            "CanvasSize",
            "Transparency",
            "Color",
        ] {
            let policy =
                bulk_patches_sourcemap_policy(&[make_patch(BulkColumnKind::Property, prop)]);
            assert_eq!(policy, BulkSourcemapPolicy::RequireExactRetained);
            assert!(
                !bulk_patches_allow_preserved_sourcemap(policy, &empty),
                "R8/Reconcile-6: {prop} must fail closed when exactRetained is absent"
            );
            assert!(
                !bulk_patches_allow_preserved_sourcemap(policy, &failed),
                "R8/Reconcile-6: {prop} must fail closed when Studio observed a side effect"
            );
            assert!(
                bulk_patches_allow_preserved_sourcemap(policy, &proved),
                "R8/Reconcile-6: {prop} may preserve sourcemap when Studio proved exactRetained=true"
            );
        }
        let attr_policy =
            bulk_patches_sourcemap_policy(&[make_patch(BulkColumnKind::Attribute, "@attributes")]);
        assert_eq!(attr_policy, BulkSourcemapPolicy::MetadataOnly);
        assert!(bulk_patches_allow_preserved_sourcemap(attr_policy, &empty));
        assert!(!bulk_patches_allow_preserved_sourcemap(
            attr_policy,
            &failed
        ));
        let tag_policy = bulk_patches_sourcemap_policy(&[make_patch(BulkColumnKind::Tag, "Tags")]);
        assert_eq!(tag_policy, BulkSourcemapPolicy::MetadataOnly);
        assert!(bulk_patches_allow_preserved_sourcemap(tag_policy, &empty));
        let mut with_assertions = make_patch(BulkColumnKind::Attribute, "@attributes");
        with_assertions.columns.insert(
            0,
            BulkPropertyColumn {
                kind: BulkColumnKind::AssertModelScale,
                property_name: "Scale".into(),
                rows: vec![1],
                values: BulkPropertyValues::F32Const(1.0),
            },
        );
        with_assertions.columns.insert(
            1,
            BulkPropertyColumn {
                kind: BulkColumnKind::AssertModelPivot,
                property_name: "WorldPivot".into(),
                rows: vec![1],
                values: BulkPropertyValues::CFrame(vec![[
                    0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
                ]]),
            },
        );
        assert!(
            bulk_patches_allow_preserved_sourcemap(
                bulk_patches_sourcemap_policy(&[with_assertions]),
                &empty
            ),
            "metadata and read-only model assertion columns may preserve sourcemap"
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn store_rows_differ_only_at_pure_creation_branch_checks_targets_and_classes() {
        use crate::settings::bytecode::{
            SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode,
        };
        let base_doc = SettingsBytecode {
            version: 12,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws-0".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "folder-1".into(),
                    "Holder".into(),
                    "Folder".into(),
                    Some(0),
                ),
            ],
        };
        let mut added_part_doc = base_doc.clone();
        added_part_doc.instances.push(SettingsBytecodeInstance::new(
            "part-2".into(),
            "NewPart".into(),
            "Part".into(),
            Some(1),
        ));
        let held = encode_settings_bytecode(&base_doc).unwrap();
        let files_part = encode_settings_bytecode(&added_part_doc).unwrap();
        assert!(
            store_rows_differ_only_at(
                &held,
                &files_part,
                &HashSet::from(["folder-1".to_string()]),
                false
            )
            .unwrap()
        );
        assert!(
            !store_rows_differ_only_at(
                &held,
                &files_part,
                &HashSet::from(["folder-1".to_string()]),
                true
            )
            .unwrap()
        );
        let mut added_script_doc = base_doc;
        added_script_doc
            .instances
            .push(SettingsBytecodeInstance::new(
                "script-2".into(),
                "NewScript".into(),
                "Script".into(),
                Some(1),
            ));
        let files_script = encode_settings_bytecode(&added_script_doc).unwrap();
        assert!(
            !store_rows_differ_only_at(
                &held,
                &files_script,
                &HashSet::from(["folder-1".to_string()]),
                false
            )
            .unwrap()
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn zero_capture_plan_and_before_reconstruction_roundtrip_without_terrain_retry() {
        use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};
        use crate::settings::bytecode::{
            SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
            encode_settings_bytecode,
        };
        let root = crate::tests::support::temp_dir("e2-zero-capture-roundtrip");
        let src_dir = root.join("src");
        let store_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        fs::create_dir_all(root.join("src/Workspace")).unwrap();
        let mut part =
            SettingsBytecodeInstance::new("part-1".into(), "Block".into(), "Part".into(), Some(0));
        part.properties.insert("Transparency".into(), json!(0.25));
        let doc = SettingsBytecode {
            version: 12,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws-0".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                part,
            ],
        };
        let desired_bytes = encode_settings_bytecode(&doc).unwrap();
        fs::write(root.join(&store_rel), &desired_bytes).unwrap();
        let held_project = ProjectSnapshot {
            entries: BTreeMap::from([(store_rel.clone(), SnapshotEntry::File(desired_bytes))]),
        };
        let context = BoundContext {
            id: 1,
            initialized: true,
            root: root.display().to_string(),
            project: root.join("renium.project.json").display().to_string(),
            experience: "test".into(),
            source: src_dir.display().to_string(),
            resource_lease: None,
            place_id: None,
            game_id: None,
            selector: String::new(),
            runtime_id: Some("rt-test".into()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let stage =
            ExportProjectStage::create(&root, Path::new("src"), &["Workspace".into()]).unwrap();
        let bridge = crate::studio::bridge::routing_tests::fixture();
        let guard = StudioChangeGuard {
            runtime_id: "rt-test".into(),
            change_seq: 0,
            tracking_guard_id: None,
            runtime_bootstrap_safe: true,
            service_generations: BTreeMap::new(),
        };
        let initial_state = json!({
            "pushProofScope": {
                "services": ["Workspace"],
                "valuesOnly": { "Workspace": true },
                "changedClassProps": {
                    "Workspace": { "Part": ["Transparency"] }
                },
                "expectedTerrainDigest": "digest-1"
            }
        });
        let push_args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::default_connection(),
        );
        let services = vec!["Workspace".to_string()];
        let source_paths = vec![PathBuf::from("src")];
        let zctx = ZeroCaptureContext {
            context: &context,
            bridge: &bridge,
            root: &root,
            src_dir: &src_dir,
            services: &services,
            stage: &stage,
            guard: &guard,
            initial_state: &initial_state,
            push_args: &push_args,
            cache_input: None,
            source_paths: &source_paths,
        };
        let extracted = plan_zero_capture_bulk_restore(&zctx, &held_project)
            .unwrap()
            .expect("zero-capture bulk restore plan must build for Workspace Part.Transparency");
        assert_eq!(extracted.plan.bulk_property_patches.len(), 1);
        assert_eq!(
            extracted.plan.bulk_property_patches[0]
                .expected_terrain_digest
                .as_deref(),
            Some("digest-1")
        );
        let rows_b64 = "AQAAAA==";
        let vals_b64 = "AABAPw==";
        let mut old_cols = Map::new();
        old_cols.insert(
            "Workspace".into(),
            json!([{
                "propertyName": "Transparency",
                "encoding": "f32",
                "count": 1,
                "rows": rows_b64,
                "values": vals_b64
            }]),
        );
        let (before, mutation_paths) = reconstruct_zero_capture_before(
            &held_project,
            &extracted.service_rows,
            &old_cols,
            None,
            None,
            BTreeMap::new(),
            HashSet::new(),
        )
        .unwrap();
        assert!(mutation_paths.contains(&store_rel));
        let SnapshotEntry::File(before_bytes) = before.entries.get(&store_rel).unwrap() else {
            panic!("before store must be a file");
        };
        let before_doc = decode_settings_bytecode(before_bytes).unwrap();
        let before_t = before_doc.instances[1]
            .properties
            .get("Transparency")
            .and_then(Value::as_f64)
            .unwrap();
        assert!(
            (before_t - 0.75).abs() < 1e-5,
            "before state must hold Studio's 0.75, got {before_t}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn zero_capture_plans_terrain_voxels_alone_and_beside_bulk_and_reconstructs_history() {
        use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};
        use crate::settings::bytecode::{
            SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
            decode_settings_bytecode, encode_settings_bytecode,
        };
        let root = crate::tests::support::temp_dir("zero-capture-terrain");
        let src_dir = root.join("src");
        let store_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        fs::create_dir_all(root.join("src/Workspace")).unwrap();
        let mut terrain = SettingsBytecodeInstance::new(
            "terrain-1".into(),
            "Terrain".into(),
            "Terrain".into(),
            Some(0),
        );
        let desired_smooth = json!({ "_type": "BinaryString", "base64": "AQU=" });
        let desired_physics = json!({ "_type": "BinaryString", "base64": "AQ==" });
        terrain
            .properties
            .insert("SmoothGrid".into(), desired_smooth.clone());
        terrain
            .properties
            .insert("PhysicsGrid".into(), desired_physics.clone());
        let mut part =
            SettingsBytecodeInstance::new("part-1".into(), "Block".into(), "Part".into(), Some(0));
        part.properties.insert("Transparency".into(), json!(0.25));
        let doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws-0".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                terrain,
                part,
            ],
        };
        let desired_bytes = encode_settings_bytecode(&doc).unwrap();
        fs::write(root.join(&store_rel), &desired_bytes).unwrap();
        let held_project = ProjectSnapshot {
            entries: BTreeMap::from([(store_rel.clone(), SnapshotEntry::File(desired_bytes))]),
        };
        let context = BoundContext {
            id: 1,
            initialized: true,
            root: root.display().to_string(),
            project: root.join("renium.project.json").display().to_string(),
            experience: "test".into(),
            source: src_dir.display().to_string(),
            resource_lease: None,
            place_id: None,
            game_id: None,
            selector: String::new(),
            runtime_id: Some("rt-terrain".into()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let stage =
            ExportProjectStage::create(&root, Path::new("src"), &["Workspace".into()]).unwrap();
        let bridge = crate::studio::bridge::routing_tests::fixture();
        let guard = StudioChangeGuard {
            runtime_id: "rt-terrain".into(),
            change_seq: 0,
            tracking_guard_id: None,
            runtime_bootstrap_safe: true,
            service_generations: BTreeMap::new(),
        };
        let push_args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::default_connection(),
        );
        let services = vec!["Workspace".to_string()];
        let source_paths = vec![PathBuf::from("src")];
        let plan_with = |initial_state: &Value| {
            let zctx = ZeroCaptureContext {
                context: &context,
                bridge: &bridge,
                root: &root,
                src_dir: &src_dir,
                services: &services,
                stage: &stage,
                guard: &guard,
                initial_state,
                push_args: &push_args,
                cache_input: None,
                source_paths: &source_paths,
            };
            plan_zero_capture_bulk_restore(&zctx, &held_project)
        };

        let alone_state = json!({
            "pushProofScope": {
                "services": ["Workspace"],
                "valuesOnly": { "Workspace": true },
                "terrainDiffers": true,
                "terrainOnlyWorkspace": true,
                "expectedTerrainDigest": "digest-1"
            }
        });
        let alone = plan_with(&alone_state)
            .unwrap()
            .expect("terrain-only Workspace scope must plan zero-capture restore");
        assert!(alone.plan.bulk_property_patches.is_empty());
        assert_eq!(alone.plan.extra_property_changes.len(), 1);
        assert_eq!(alone.plan.extra_property_changes[0].class_name, "Terrain");
        assert!(alone.plan.capture_before_terrain);
        assert_eq!(alone.terrain_restore, Some((store_rel.clone(), 1)));

        let studio_smooth = json!({ "_type": "BinaryString", "base64": "AQV4" });
        let before_grids = json!({
            "SmoothGrid": studio_smooth,
            "PhysicsGrid": desired_physics,
        });
        let (before, mutation_paths) = reconstruct_zero_capture_before(
            &held_project,
            &alone.service_rows,
            &Map::new(),
            alone.terrain_restore.as_ref(),
            Some(&before_grids),
            BTreeMap::new(),
            HashSet::new(),
        )
        .unwrap();
        assert!(mutation_paths.contains(&store_rel));
        let SnapshotEntry::File(before_bytes) = before.entries.get(&store_rel).unwrap() else {
            panic!("before store must be a file");
        };
        let before_doc = decode_settings_bytecode(before_bytes).unwrap();
        assert_eq!(
            before_doc.instances[1].properties.get("SmoothGrid"),
            Some(&studio_smooth)
        );

        let mixed_state = json!({
            "pushProofScope": {
                "services": ["Workspace"],
                "valuesOnly": { "Workspace": true },
                "changedClassProps": {
                    "Workspace": { "Part": ["Transparency"] }
                },
                "terrainDiffers": true,
                "terrainOnlyWorkspace": false,
                "expectedTerrainDigest": "digest-1"
            }
        });
        let mixed = plan_with(&mixed_state)
            .unwrap()
            .expect("mass edit + terrain voxels must plan combined zero-capture restore");
        assert_eq!(mixed.plan.bulk_property_patches.len(), 1);
        assert!(
            mixed.plan.bulk_property_patches[0]
                .expected_terrain_digest
                .is_none()
        );
        assert_eq!(mixed.plan.extra_property_changes.len(), 1);

        let unsafe_state = json!({
            "pushProofScope": {
                "services": ["Workspace"],
                "valuesOnly": { "Workspace": true },
                "terrainDiffers": true,
                "terrainOnlyWorkspace": false,
                "expectedTerrainDigest": "digest-1"
            }
        });
        assert!(plan_with(&unsafe_state).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hybrid_route_plans_delta_beside_bulk_and_drops_bulk_on_whole_replan() {
        use crate::settings::bytecode::{
            SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
            encode_settings_bytecode,
        };

        let store_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let part_count = super::super::replacement::BULK_PROPERTY_MIN_INSTANCES + 8;
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )],
        };
        for i in 0..part_count {
            let mut p = SettingsBytecodeInstance::new(
                format!("p-{i}"),
                format!("Part{i}"),
                "Part".into(),
                Some(0),
            );
            p.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:032x}", i + 10)}),
            );
            p.properties.insert("Transparency".into(), json!(0.0));
            desired.instances.push(p);
        }
        let mut observed = desired.clone();
        for inst in observed.instances.iter_mut().skip(1) {
            inst.properties.insert("Transparency".into(), json!(0.5));
        }
        observed.instances[1].name = "Part0Renamed".into();
        let project = ProjectSnapshot {
            entries: BTreeMap::from([(
                store_rel.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&desired).unwrap()),
            )]),
        };
        let studio = ProjectSnapshot {
            entries: BTreeMap::from([(
                store_rel.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&observed).unwrap()),
            )]),
        };

        let mut prepared_settings = HashMap::new();
        let mut bulk_patches = HashMap::new();
        let differences = collect_delta_bulk_differences(
            &project,
            &studio,
            StoresRead::ChangedOfBoth,
            &mut prepared_settings,
            &mut bulk_patches,
        )
        .unwrap();
        assert!(differences.contains(&store_rel));
        assert!(bulk_patches.contains_key(&store_rel));
        assert!(prepared_settings.contains_key(&store_rel));
        assert!(crate::settings::equivalence::holds_unread_instances(
            &prepared_settings[&store_rel].current
        ));

        let non_bulk_differences: HashSet<PathBuf> = differences
            .iter()
            .filter(|path| {
                !bulk_patches.contains_key(*path) || prepared_settings.contains_key(*path)
            })
            .cloned()
            .collect();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &studio,
            &project,
            &non_bulk_differences,
            &prepared_settings,
            true,
            false,
        )
        .unwrap();
        assert!(plan.target_settings_ids.contains(&"p-0".to_string()));
        assert!(
            plan.target_settings_ids.len() < part_count / 2,
            "hybrid plan should target only delta rows, got {}",
            plan.target_settings_ids.len()
        );

        let hybrid_sparse: HashSet<PathBuf> = bulk_patches
            .keys()
            .filter(|path| {
                prepared_settings.get(*path).is_some_and(|change| {
                    crate::settings::equivalence::holds_unread_instances(&change.current)
                })
            })
            .cloned()
            .collect();
        let mut bulk_kept = bulk_patches.clone();
        let kept_plan = plan_from_whole_stores(
            (StoresRead::ChangedOfBoth, false, plan),
            (&project, &studio),
            &non_bulk_differences,
            &mut prepared_settings,
        )
        .unwrap();
        drop_promoted_bulk_patches(&mut bulk_kept, &hybrid_sparse, &prepared_settings);
        assert!(bulk_kept.contains_key(&store_rel));

        let mut forced_plan = kept_plan;
        forced_plan
            .initial_native_services
            .insert("ReplicatedStorage".into());
        let mut bulk_dropped = bulk_patches;
        let _replanned = plan_from_whole_stores(
            (StoresRead::ChangedOfBoth, false, forced_plan),
            (&project, &studio),
            &non_bulk_differences,
            &mut prepared_settings,
        )
        .unwrap();
        drop_promoted_bulk_patches(&mut bulk_dropped, &hybrid_sparse, &prepared_settings);
        assert!(!crate::settings::equivalence::holds_unread_instances(
            &prepared_settings[&store_rel].current
        ));
        assert!(
            !bulk_dropped.contains_key(&store_rel),
            "bulk patch must be dropped when hybrid store is swapped for whole documents"
        );
    }

    #[test]
    fn changed_mode_hybrid_replans_whole_store_for_cross_service_class_change_referrer() {
        use crate::settings::bytecode::{
            SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
            encode_settings_bytecode,
        };

        let ws_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let rs_rel = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let extra_rel = PathBuf::from("renium.project.jsonc");
        let part_count = super::super::replacement::BULK_PROPERTY_MIN_INSTANCES + 8;

        let mut ws_desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )],
        };
        for i in 0..part_count {
            let mut p = SettingsBytecodeInstance::new(
                format!("p-{i}"),
                format!("Part{i}"),
                "Part".into(),
                Some(0),
            );
            p.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:032x}", i + 10)}),
            );
            p.properties.insert("Transparency".into(), json!(0.0));
            ws_desired.instances.push(p);
        }
        let mut referrer = SettingsBytecodeInstance::new(
            "ws-ref".into(),
            "RefHolder".into(),
            "ObjectValue".into(),
            Some(0),
        );
        referrer.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": format!("{:032x}", 9999)}),
        );
        referrer.properties.insert(
            "Value".into(),
            json!({"_type": "Ref", "settingsId": "rs-target", "service": "ReplicatedStorage"}),
        );
        ws_desired.instances.push(referrer);

        let mut ws_observed = ws_desired.clone();
        for inst in ws_observed.instances.iter_mut().skip(1).take(part_count) {
            inst.properties.insert("Transparency".into(), json!(0.5));
        }
        ws_observed.instances[1].name = "Part0Renamed".into();

        let rs_desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "rs".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "rs-target".into(),
                    "Target".into(),
                    "Model".into(),
                    Some(0),
                ),
            ],
        };
        let rs_observed = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "rs".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "rs-target".into(),
                    "Target".into(),
                    "Folder".into(),
                    Some(0),
                ),
            ],
        };

        let project = ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    ws_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&ws_desired).unwrap()),
                ),
                (
                    rs_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&rs_desired).unwrap()),
                ),
                (extra_rel.clone(), SnapshotEntry::File(b"{}\n".to_vec())),
            ]),
        };
        let studio = ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    ws_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&ws_observed).unwrap()),
                ),
                (
                    rs_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&rs_observed).unwrap()),
                ),
            ]),
        };

        let all_paths = project_replacement_paths(&project, &studio);
        let stores_read = stores_read_of(true, &all_paths, &project, &studio);
        assert!(stores_read == StoresRead::Changed);

        let mut prepared_settings = HashMap::new();
        let mut bulk_patches = HashMap::new();
        let differences = collect_delta_bulk_differences(
            &project,
            &studio,
            stores_read,
            &mut prepared_settings,
            &mut bulk_patches,
        )
        .unwrap();
        assert!(bulk_patches.contains_key(&ws_rel));
        assert!(crate::settings::equivalence::holds_unread_instances(
            &prepared_settings[&ws_rel].current
        ));

        let non_bulk_differences: HashSet<PathBuf> = differences
            .iter()
            .filter(|path| {
                !bulk_patches.contains_key(*path) || prepared_settings.contains_key(*path)
            })
            .cloned()
            .collect();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &studio,
            &project,
            &non_bulk_differences,
            &prepared_settings,
            true,
            false,
        )
        .unwrap();
        let hybrid_sparse: HashSet<PathBuf> = bulk_patches
            .keys()
            .filter(|path| {
                prepared_settings.get(*path).is_some_and(|change| {
                    crate::settings::equivalence::holds_unread_instances(&change.current)
                })
            })
            .cloned()
            .collect();
        let replanned = plan_from_whole_stores(
            (stores_read, false, plan),
            (&project, &studio),
            &non_bulk_differences,
            &mut prepared_settings,
        )
        .unwrap();
        drop_promoted_bulk_patches(&mut bulk_patches, &hybrid_sparse, &prepared_settings);

        assert!(!crate::settings::equivalence::holds_unread_instances(
            &prepared_settings[&ws_rel].current
        ));
        assert!(!bulk_patches.contains_key(&ws_rel));
        assert!(
            replanned
                .target_settings_ids
                .iter()
                .any(|id| id == "ws-ref")
                || replanned
                    .extra_property_changes
                    .iter()
                    .any(|row| row.settings_id.as_deref() == Some("ws-ref")),
            "unchanged Workspace ObjectValue referring to class-changed ReplicatedStorage target must be resent"
        );
    }

    #[test]
    fn hybrid_store_reads_run_context_for_script_whose_source_this_push_carries() {
        use crate::settings::bytecode::{
            SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
            encode_settings_bytecode,
        };

        let ws_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let edited_src = PathBuf::from("src/Workspace/ClientRunner.run-client.luau");
        let part_count = super::super::replacement::BULK_PROPERTY_MIN_INSTANCES + 8;

        let mut ws_desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )],
        };
        for i in 0..part_count {
            let mut p = SettingsBytecodeInstance::new(
                format!("p-{i}"),
                format!("Part{i}"),
                "Part".into(),
                Some(0),
            );
            p.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:032x}", i + 10)}),
            );
            p.properties.insert("Transparency".into(), json!(0.0));
            ws_desired.instances.push(p);
        }
        let mut client_script = SettingsBytecodeInstance::new(
            "client-script".into(),
            "ClientRunner".into(),
            "Script".into(),
            Some(0),
        );
        client_script.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": format!("{:032x}", 8001)}),
        );
        client_script.properties.insert(
            "RunContext".into(),
            crate::editor::paths::editor_run_context_value("Client"),
        );
        let client_script_index = ws_desired.instances.len();
        ws_desired.instances.push(client_script);

        let mut untouched_script = SettingsBytecodeInstance::new(
            "other-script".into(),
            "UntouchedRunner".into(),
            "Script".into(),
            Some(0),
        );
        untouched_script.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": format!("{:032x}", 8002)}),
        );
        untouched_script.properties.insert(
            "RunContext".into(),
            crate::editor::paths::editor_run_context_value("Client"),
        );
        let untouched_script_index = ws_desired.instances.len();
        ws_desired.instances.push(untouched_script);

        let mut ws_observed = ws_desired.clone();
        for inst in ws_observed.instances.iter_mut().skip(1).take(part_count) {
            inst.properties.insert("Transparency".into(), json!(0.5));
        }
        ws_observed.instances[1].name = "Part0Renamed".into();

        let project = ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    ws_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&ws_desired).unwrap()),
                ),
                (
                    edited_src.clone(),
                    SnapshotEntry::File(b"print('after')\n".to_vec()),
                ),
            ]),
        };
        let studio = ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    ws_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&ws_observed).unwrap()),
                ),
                (
                    edited_src.clone(),
                    SnapshotEntry::File(b"print('before')\n".to_vec()),
                ),
            ]),
        };

        let mut prepared_settings = HashMap::new();
        let mut bulk_patches = HashMap::new();
        let _differences = collect_delta_bulk_differences(
            &project,
            &studio,
            StoresRead::ChangedOfBoth,
            &mut prepared_settings,
            &mut bulk_patches,
        )
        .unwrap();
        assert!(bulk_patches.contains_key(&ws_rel));
        let prepared = &prepared_settings[&ws_rel];
        assert!(!crate::settings::held::stands_unchanged(
            &prepared.current.instances[client_script_index].properties
        ));
        assert!(!crate::settings::held::stands_unchanged(
            &prepared.current.instances[untouched_script_index].properties
        ));

        let children = crate::settings::tree::settings_children_by_parent(&prepared.current);
        let resolved = crate::editor::paths::editor_source_target_with_children(
            &prepared.current,
            "Workspace",
            Path::new("src/Workspace"),
            &edited_src,
            &children,
        )
        .expect("edited Client RunContext Script source must resolve in hybrid store");
        assert_eq!(resolved.class_name, "Script");
        assert_eq!(resolved.settings_id.as_deref(), Some("client-script"));
    }

    #[test]
    fn hybrid_store_leaves_script_rows_unread_when_push_carries_no_changed_sources() {
        use crate::settings::bytecode::{
            SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
            encode_settings_bytecode,
        };

        let ws_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let unchanged_src = PathBuf::from("src/Workspace/ClientRunner.run-client.luau");
        let part_count = super::super::replacement::BULK_PROPERTY_MIN_INSTANCES + 8;

        let mut ws_desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )],
        };
        for i in 0..part_count {
            let mut p = SettingsBytecodeInstance::new(
                format!("p-{i}"),
                format!("Part{i}"),
                "Part".into(),
                Some(0),
            );
            p.properties.insert(
                "UniqueId".into(),
                json!({"_type": "UniqueId", "value": format!("{:032x}", i + 10)}),
            );
            p.properties.insert("Transparency".into(), json!(0.0));
            ws_desired.instances.push(p);
        }
        let mut client_script = SettingsBytecodeInstance::new(
            "client-script".into(),
            "ClientRunner".into(),
            "Script".into(),
            Some(0),
        );
        client_script.properties.insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": format!("{:032x}", 8001)}),
        );
        client_script.properties.insert(
            "RunContext".into(),
            crate::editor::paths::editor_run_context_value("Client"),
        );
        let client_script_index = ws_desired.instances.len();
        ws_desired.instances.push(client_script);

        let mut ws_observed = ws_desired.clone();
        for inst in ws_observed.instances.iter_mut().skip(1).take(part_count) {
            inst.properties.insert("Transparency".into(), json!(0.5));
        }
        ws_observed.instances[1].name = "Part0Renamed".into();

        let project = ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    ws_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&ws_desired).unwrap()),
                ),
                (
                    unchanged_src.clone(),
                    SnapshotEntry::File(b"print('same')\n".to_vec()),
                ),
            ]),
        };
        let studio = ProjectSnapshot {
            entries: BTreeMap::from([
                (
                    ws_rel.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&ws_observed).unwrap()),
                ),
                (
                    unchanged_src,
                    SnapshotEntry::File(b"print('same')\n".to_vec()),
                ),
            ]),
        };

        let mut prepared_settings = HashMap::new();
        let mut bulk_patches = HashMap::new();
        let _differences = collect_delta_bulk_differences(
            &project,
            &studio,
            StoresRead::ChangedOfBoth,
            &mut prepared_settings,
            &mut bulk_patches,
        )
        .unwrap();
        assert!(bulk_patches.contains_key(&ws_rel));
        let prepared = &prepared_settings[&ws_rel];
        assert!(crate::settings::held::stands_unchanged(
            &prepared.current.instances[client_script_index].properties
        ));
    }
}
