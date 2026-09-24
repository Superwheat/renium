use super::*;

pub(crate) fn pin_edit_runtime(context: &BoundContext, bridge: &BridgeServer) -> Result<()> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Live Sync context has no edit-mode Studio runtime")?;
    bridge.clear_runtime_pins();
    bridge.pin_runtime(BridgeTarget::Edit, runtime_id);
    bridge.pin_runtime(BridgeTarget::Main, runtime_id);
    Ok(())
}

pub(crate) fn studio_change_guard_from_state(
    context: &BoundContext,
    state: &Value,
) -> Result<StudioChangeGuard> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    let state_runtime_id = state["runtimeId"]
        .as_str()
        .context("Studio change state did not include its runtime")?;
    if state_runtime_id != runtime_id {
        bail!("Studio change state came from a different runtime");
    }
    let service_generations = state["serviceGenerations"]
        .as_object()
        .context("Studio change state did not include service generations")?
        .iter()
        .map(|(service, generation)| {
            generation
                .as_u64()
                .map(|generation| (service.clone(), generation))
                .with_context(|| format!("Studio generation for {service} is invalid"))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(StudioChangeGuard {
        runtime_id: state_runtime_id.to_string(),
        change_seq: state["seq"]
            .as_u64()
            .context("Studio change state did not include its sequence")?,
        tracking_guard_id: None,
        runtime_bootstrap_safe: studio_runtime_bootstrap_safe(state),
        service_generations,
    })
}

pub(crate) fn studio_runtime_bootstrap_safe(state: &Value) -> bool {
    let restored_pending_services = state["restoredPendingServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let has_fresh_changes = state["dirtyServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|service| !restored_pending_services.contains(service));
    state["tracking"].as_bool() == Some(true) && !has_fresh_changes
}

pub(crate) fn read_studio_change_state(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<Value> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    pin_edit_runtime(context, bridge)?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": false, "includeGenerations": true }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    Ok(state)
}

pub(crate) fn current_studio_checkpoint(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Option<StudioCheckpoint> {
    match read_studio_change_state(context, bridge) {
        Ok(state) => {
            let checkpoint = StudioCheckpoint::from_state(context, &state);
            if checkpoint.is_none() {
                log_global(
                    5,
                    format_args!(
                        "[renium] Studio checkpoint state rejected: tracking={:?} tracked={:?} dirty={} full={} runtime={:?} version={:?} seq={:?} generations={}",
                        state["tracking"].as_bool(),
                        state["trackedServices"].as_u64(),
                        state["dirtyServices"].as_array().map_or(0, Vec::len),
                        state["fullSyncServices"].as_array().map_or(0, Vec::len),
                        state["runtimeId"].as_str(),
                        state["changeTrackerVersion"].as_u64(),
                        state["seq"].as_u64(),
                        checkpoint_generations(&state).map_or(0, Map::len),
                    ),
                );
            }
            checkpoint
        }
        Err(error) => {
            log_global(
                5,
                format_args!("[renium] Studio checkpoint unavailable: {error:#}"),
            );
            None
        }
    }
}

pub(crate) fn reconciled_studio_checkpoint(
    context: &BoundContext,
    bridge: &BridgeServer,
    initial_state: &Value,
    guard: &StudioChangeGuard,
) -> Option<StudioCheckpoint> {
    let services = initial_state["dirtyServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if services.is_empty() {
        return current_studio_checkpoint(context, bridge);
    }
    match acknowledge_pulled_changes(bridge, &services, guard.change_seq, &guard.runtime_id) {
        Ok(state) => StudioCheckpoint::from_state(context, &state),
        Err(error) => {
            log_global(
                5,
                format_args!("[renium] reconciled Studio acknowledgment failed: {error:#}"),
            );
            None
        }
    }
}

pub(crate) struct StudioTrackingGuardRelease<'a> {
    pub(crate) bridge: &'a BridgeServer,
    pub(crate) runtime_id: String,
    pub(crate) guard_id: Option<String>,
    pub(crate) proof: Option<Value>,
    pub(crate) retained_proof: Option<Value>,
    pub(crate) retained_proof_local: bool,
}

impl StudioTrackingGuardRelease<'_> {
    pub(crate) fn finish(&mut self) -> Result<()> {
        let Some(guard_id) = self.guard_id.as_ref() else {
            return Ok(());
        };
        let result = self.bridge.call_for_runtime_with_timeout(
            "getStudioChangeState",
            json!({
                "start": false,
                "releaseTrackingGuardId": guard_id,
            }),
            BridgeTarget::Edit,
            &self.runtime_id,
            Some(Duration::from_secs(10)),
        )?;
        ensure_plugin_api_ok(&result)?;
        self.guard_id = None;
        Ok(())
    }
}

impl Drop for StudioTrackingGuardRelease<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub(crate) fn acknowledge_verified_push(
    bridge: &BridgeServer,
    services: &[String],
    guard: &StudioChangeGuard,
    tracking_release: &mut StudioTrackingGuardRelease<'_>,
) -> Result<()> {
    let result = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "services": services,
            "start": false,
            "ackSeq": guard.change_seq,
            "runtimeId": guard.runtime_id,
            "releaseTrackingGuardId": tracking_release.guard_id,
            "retainPushProof": tracking_release.proof,
        }),
        BridgeTarget::Edit,
        &guard.runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&result)?;
    tracking_release.guard_id = None;
    tracking_release.retained_proof = result
        .get("retainedPushProof")
        .filter(|value| !value.is_null())
        .cloned();
    tracking_release.retained_proof_local = result["retainedPushProofLocal"] == true;
    Ok(())
}

pub(crate) fn current_studio_change_guard_with_state(
    context: &BoundContext,
    bridge: &BridgeServer,
    arm_native: impl FnOnce(&Value) -> Result<()>,
) -> Result<(StudioChangeGuard, Value)> {
    current_studio_change_guard_verifying(context, bridge, None, None, arm_native)
}

pub(crate) fn current_studio_change_guard_verifying(
    context: &BoundContext,
    bridge: &BridgeServer,
    native_attribute_services: Option<&[String]>,
    verify_push_proof: Option<&Value>,
    arm_native: impl FnOnce(&Value) -> Result<()>,
) -> Result<(StudioChangeGuard, Value)> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    // Acquire the lease and return its initial state atomically. A separate
    // read-then-start costs a round trip and races another tracking client.
    pin_edit_runtime(context, bridge)?;
    let guard_id = format!(
        "push-{}-{}-{}",
        std::process::id(),
        runtime_id,
        crate::app::timing::current_millis()
    );
    let mut state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "start": true,
            "trackingGuardId": guard_id,
            "includeGenerations": true,
            "nativeAttributeRelay": cfg!(windows) && native_attribute_services.is_some(),
            "deferNativeTracking": cfg!(windows) && native_attribute_services.is_some(),
            "captureLocalPushProof": native_attribute_services.is_some(),
            "services": native_attribute_services,
            "verifyPushProof": verify_push_proof,
        }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    let mut release = StudioTrackingGuardRelease {
        bridge,
        runtime_id: runtime_id.to_string(),
        guard_id: Some(guard_id.clone()),
        proof: None,
        retained_proof: None,
        retained_proof_local: false,
    };
    ensure_plugin_api_ok(&state)?;
    let tracking_started = state["trackingStarted"].as_bool() != Some(false);
    arm_native(&state)?;
    log_global(
        5,
        format_args!(
            "[renium] tracking lease: deferred={} proofMatches={} mismatch={} verifying={} continued={} started={}",
            state["nativeTrackingDeferred"],
            state["pushProofMatches"],
            state["pushProofMismatch"],
            verify_push_proof.is_some(),
            state["nativeRelayContinued"],
            state["trackingStarted"]
        ),
    );
    if state["nativeTrackingDeferred"] == true {
        // The native handshake starts tracking with attribute observation already
        // armed. If discovery failed, this call starts ordinary local listeners.
        // In either case this is the initial, fully observed generation fence.
        let proof_matches = state.get("pushProofMatches").cloned();
        let proof_mismatch = state.get("pushProofMismatch").cloned();
        state = bridge.call_for_runtime_with_timeout(
            "getStudioChangeState",
            json!({"start": true, "trackingGuardId": guard_id, "includeGenerations": true}),
            BridgeTarget::Edit,
            runtime_id,
            Some(Duration::from_secs(10)),
        )?;
        ensure_plugin_api_ok(&state)?;
        if let Some(matches) = proof_matches {
            state["pushProofMatches"] = matches;
        }
        if let Some(mismatch) = proof_mismatch {
            state["pushProofMismatch"] = mismatch;
        }
    }
    let mut guard = studio_change_guard_from_state(context, &state)?;
    guard.tracking_guard_id = Some(guard_id);
    if tracking_started || state["trackingStarted"].as_bool() != Some(false) {
        // Older plugins do not distinguish pre-existing tracking from startup.
        guard.runtime_bootstrap_safe = false;
    }
    release.guard_id = None;
    Ok((guard, state))
}

pub(crate) fn current_studio_change_guard(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<StudioChangeGuard> {
    current_studio_change_guard_with_state(context, bridge, |_| Ok(())).map(|(guard, _)| guard)
}

pub(crate) fn studio_guard_matches_state(
    context: &BoundContext,
    guard: &StudioChangeGuard,
    state: &Value,
) -> bool {
    studio_change_guard_from_state(context, state).is_ok_and(|current| {
        current.runtime_id == guard.runtime_id
            && current.change_seq == guard.change_seq
            && current.service_generations == guard.service_generations
    })
}

pub(crate) fn studio_states_share_epoch(
    context: &BoundContext,
    left: &Value,
    right: &Value,
) -> bool {
    let services = sync_services();
    let expected_runtime = context.runtime_id.as_deref();
    let tracker_version = left["changeTrackerVersion"].as_u64();
    let seq = left["seq"].as_u64();
    let fields_match = left["tracking"].as_bool() == Some(true)
        && right["tracking"].as_bool() == Some(true)
        && left["trackedServices"].as_u64() == u64::try_from(services.len()).ok()
        && right["trackedServices"].as_u64() == u64::try_from(services.len()).ok()
        && left["runtimeId"].as_str() == expected_runtime
        && right["runtimeId"].as_str() == expected_runtime
        && tracker_version.is_some()
        && tracker_version == right["changeTrackerVersion"].as_u64()
        && seq.is_some()
        && seq == right["seq"].as_u64();
    if !fields_match {
        return false;
    }
    let (Some(left_generations), Some(right_generations)) =
        (checkpoint_generations(left), checkpoint_generations(right))
    else {
        return false;
    };
    if left_generations.len() != services.len() || right_generations.len() != services.len() {
        return false;
    }
    services.iter().all(|service| {
        left_generations.get(service).and_then(Value::as_u64)
            == right_generations.get(service).and_then(Value::as_u64)
    })
}

// Only the paths Studio contributed are published, so a project file edited
// meanwhile elsewhere is neither overwritten nor a reason to fail.
pub(crate) fn publish_captured_studio(
    context: &BoundContext,
    bridge: &BridgeServer,
    stage: ExportProjectStage,
    guard: &StudioChangeGuard,
    studio_paths: &HashSet<PathBuf>,
    captured_project: &ProjectSnapshot,
) -> Result<()> {
    let confirmed = read_studio_change_state(context, bridge)?;
    let stage = if studio_guard_matches_state(context, guard, &confirmed) {
        stage
    } else {
        capture_studio_project(context, bridge)?.0
    };
    publish_studio_paths(context, stage, studio_paths, captured_project)
}

pub(crate) fn publish_studio_paths(
    context: &BoundContext,
    mut stage: ExportProjectStage,
    studio_paths: &HashSet<PathBuf>,
    captured_project: &ProjectSnapshot,
) -> Result<()> {
    stage.restrict_publish_paths(studio_paths);
    carry_project_edits_into_stage(context, &mut stage, studio_paths, captured_project)?;
    stage.publish(Path::new(&context.root), false)?;
    Ok(())
}

// A service store Studio changed may also have been edited in the project
// since the capture, for example another instance in the same service. Both
// sides' instance edits are kept. Any other file edited on both sides waits
// for the next reconcile, which reports a real conflict if one remains.
fn carry_project_edits_into_stage(
    context: &BoundContext,
    stage: &mut ExportProjectStage,
    studio_paths: &HashSet<PathBuf>,
    captured_project: &ProjectSnapshot,
) -> Result<()> {
    let root = Path::new(&context.root);
    let mut paths = studio_paths.iter().cloned().collect::<Vec<_>>();
    paths.sort();
    let current = capture_snapshot(root, &paths)?;
    let mut carried = Vec::new();
    for path in &paths {
        let captured = captured_project.entries.get(path);
        let now = current.entries.get(path);
        if captured == now {
            continue;
        }
        let is_store = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name);
        let (Some(captured), Some(now), true) = (captured, now, is_store) else {
            bail!(
                "Project file {} changed while Studio changes were being applied; retry the sync",
                path.display()
            );
        };
        let single = |entry: &SnapshotEntry| ProjectSnapshot {
            entries: BTreeMap::from([(path.clone(), entry.clone())]),
        };
        let studio = capture_snapshot(&stage.project_root, std::slice::from_ref(path))?;
        let (merged, conflicts, _) = merge_snapshots_with_changes(
            Some(&single(captured)),
            &single(now),
            &studio,
            ConflictPreference::None,
            None,
        )?;
        if !conflicts.is_empty() {
            bail!(
                "Project store {} changed while Studio changes to the same instances were being applied; retry the sync",
                path.display()
            );
        }
        apply_snapshot_paths(&stage.project_root, &HashSet::from([path.clone()]), &merged)?;
        carried.push(path.clone());
    }
    if !carried.is_empty() {
        log_global(
            5,
            format_args!(
                "[renium] reconcile kept project edits in Studio-changed stores: {}",
                carried
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    stage.accept_project_changes(root, &carried)
}

pub(crate) fn capture_studio_project(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    let services = sync_services();
    capture_studio_services(context, bridge, &services, true)
}

pub(crate) fn capture_studio_project_for_comparison(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    retry_transient_studio_capture(|| {
        let services = sync_services();
        let mut stage = project_comparison_stage(context, &services)?;
        stage.capture_publish_baseline(Path::new(&context.root))?;
        capture_studio_services_with_stage(context, bridge, &services, true, stage)
    })
}

pub(crate) fn retry_transient_studio_capture<T>(
    mut capture: impl FnMut() -> Result<T>,
) -> Result<T> {
    match capture() {
        Err(error) if is_transient_studio_capture_change(&error) => {
            log_global(
                5,
                format_args!("[renium] Studio changed during comparison capture; retrying once"),
            );
            capture()
        }
        result => result,
    }
}

pub(crate) fn is_transient_studio_capture_change(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("Studio changed ") && message.contains("retry the sync")
}

pub(crate) fn selective_studio_services(
    context: &BoundContext,
    checkpoint: &StudioCheckpoint,
    state: &Value,
) -> Result<Option<Vec<String>>> {
    let Some(services) = checkpoint.changed_services(context, state) else {
        return Ok(None);
    };
    if services.is_empty() || services.len() == sync_services().len() {
        return Ok(None);
    }
    let root = Path::new(&context.root);
    if config::try_load_project(None, Some(root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false)
    {
        return Ok(None);
    }
    Ok(Some(services))
}

pub(crate) fn capture_changed_studio_services(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    baseline: &ProjectSnapshot,
    initial_state: &Value,
) -> Result<Option<(ExportProjectStage, ProjectSnapshot, ProjectSnapshot)>> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source)
        .strip_prefix(root)
        .context("Project source is outside its root")?;
    let mut scopes = Vec::new();
    for service in services {
        scopes.push(source.join(service));
        scopes.push(
            service_settings_path(&Path::new(&context.source).join(service))
                .strip_prefix(root)
                .context("Selective capture store is outside its project root")?
                .to_path_buf(),
        );
    }
    let all_services = sync_services();
    let src_dir = bound_context::source_dir(context)?;
    let stage = ExportProjectStage::create(root, &src_dir, &all_services)?;
    let editor = capture_snapshot(&stage.project_root, stage.publish_paths())?;
    let stage = import_studio_services_into_stage(context, bridge, services, true, stage)?;
    let captured = capture_snapshot(&stage.project_root, &scopes)?;
    let mut studio = baseline.clone();
    studio
        .entries
        .retain(|path, _| !scopes.iter().any(|scope| path.starts_with(scope)));
    studio.entries.extend(captured.entries);
    let confirmed = read_studio_change_state(context, bridge)?;
    if !studio_states_share_epoch(context, initial_state, &confirmed) {
        return Ok(None);
    }
    // Files edited while Studio was captured are compared at their latest
    // content. The stage keeps Studio's captured scopes and takes the rest
    // from the project, so a busy project does not keep failing this capture.
    let current_editor = capture_snapshot(root, stage.publish_paths())?;
    if editor != current_editor {
        let refreshed = current_editor
            .entries
            .keys()
            .chain(editor.entries.keys())
            .filter(|path| editor.entries.get(*path) != current_editor.entries.get(*path))
            .filter(|path| !scopes.iter().any(|scope| path.starts_with(scope)))
            .cloned()
            .collect::<HashSet<_>>();
        apply_snapshot_paths(&stage.project_root, &refreshed, &current_editor)?;
    }
    log_global(
        5,
        format_args!("[renium] selective Studio capture: {}", services.join(",")),
    );
    Ok(Some((stage, studio, current_editor)))
}

pub(crate) fn project_comparison_stage(
    context: &BoundContext,
    services: &[String],
) -> Result<ExportProjectStage> {
    let root = PathBuf::from(&context.root);
    let src_dir = bound_context::source_dir(context)?;
    let requires_stage = config::try_load_project(None, Some(&root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false);
    if requires_stage {
        ExportProjectStage::create(&root, &src_dir, services)
    } else {
        ExportProjectStage::create_for_comparison(&root, &src_dir, services)
    }
}

pub(crate) fn push_project_delta(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: PushEditorChangesArgs,
    guard: Option<&StudioChangeGuard>,
) -> Result<Map<String, Value>> {
    push_project(context, bridge, services, push_args, guard, false)
}

pub(crate) fn push_project_replacement(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: PushEditorChangesArgs,
) -> Result<Map<String, Value>> {
    let replace = crate::editor::sync::native_editor_full_push_eligible(&push_args)?;
    push_project(context, bridge, services, push_args, None, replace)
}
