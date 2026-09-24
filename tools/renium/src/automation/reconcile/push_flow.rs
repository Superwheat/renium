use super::*;

pub(crate) fn push_project(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: PushEditorChangesArgs,
    guard: Option<&StudioChangeGuard>,
    replace: bool,
) -> Result<Map<String, Value>> {
    #[cfg(any(windows, target_os = "macos"))]
    let mut verified_candidate = if replace && guard.is_none() {
        take_verified_full_push(context, bridge, services, &push_args)?
    } else {
        None
    };
    #[cfg(any(windows, target_os = "macos"))]
    if let Some(candidate) = verified_candidate.take_if(|candidate| candidate.files_unchanged)
        && verified_full_push_proof_matches(context, bridge, &candidate.cached.proof)?
    {
        if let Some(runtime) = context.runtime_id.as_deref() {
            bridge
                .verified_full_pushes
                .lock_recover()
                .insert(runtime.to_string(), candidate.cached);
        }
        return Ok(Map::from_iter([
            ("ok".into(), json!(true)),
            ("unchanged".into(), json!(true)),
        ]));
    }
    #[cfg(any(windows, target_os = "macos"))]
    let cache_input = if replace && guard.is_none() {
        Some(full_push_cache_input(&push_args, services)?)
    } else {
        None
    };
    #[cfg(windows)]
    let mut _native_attributes = None;
    let phase = Instant::now();
    // The previous push's native attribute observer must outlive the proof
    // check inside the lease request, yet stop before the new lease arms its
    // own observer: stopping it earlier retires the proof, stopping it later
    // disarms the fresh observer.
    #[cfg(any(windows, target_os = "macos"))]
    let (candidate_parts, mut previous_attributes) = match verified_candidate {
        Some(candidate) => {
            let VerifiedFullPushCandidate { cached, .. } = candidate;
            #[cfg(windows)]
            let previous = cached._attributes;
            #[cfg(not(windows))]
            let previous = ();
            (Some((cached.proof.clone(), cached.project)), previous)
        }
        None => {
            #[cfg(windows)]
            let previous = None;
            #[cfg(not(windows))]
            let previous = ();
            (None, previous)
        }
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let (candidate_parts, mut previous_attributes): (Option<(Value, ProjectSnapshot)>, ()) =
        (None, ());
    let candidate_proof = candidate_parts.as_ref().map(|(proof, _)| proof.clone());
    let (mut guard, initial_state) = if let Some(guard) = guard {
        (guard.clone(), Value::Null)
    } else {
        current_studio_change_guard_verifying(
            context,
            bridge,
            (cfg!(any(windows, target_os = "macos")) && replace).then_some(services),
            candidate_proof.as_ref(),
            |state| {
                #[cfg(windows)]
                if state["nativeRelayContinued"] == true {
                    return Ok(());
                }
                #[cfg(windows)]
                drop(previous_attributes.take());
                #[cfg(windows)]
                if let Some(path) = state
                    .get("nativeAttributeRelay")
                    .filter(|value| !value.is_null())
                {
                    let path = serde_json::from_value::<Vec<String>>(path.clone())?;
                    let pid = crate::editor::review::studio_pid_for_bridge(bridge)?;
                    let title = crate::editor::review::studio_title_for_bridge(bridge, pid)?;
                    match crate::studio::native::serializer::begin_attribute_relay(
                        pid,
                        &title,
                        services,
                        Duration::from_secs(120),
                        &path,
                    ) {
                        Ok(observation) => _native_attributes = Some(observation),
                        Err(error) => log_global(
                            5,
                            format_args!("[renium] native attribute relay unavailable: {error:#}"),
                        ),
                    }
                }
                #[cfg(not(windows))]
                let _ = state;
                Ok(())
            },
        )?
    };
    log_reconcile_timing("full push guard", phase);
    #[cfg(windows)]
    if initial_state["nativeRelayContinued"] == true {
        _native_attributes = previous_attributes.take();
    } else {
        drop(previous_attributes.take());
    }
    #[cfg(not(windows))]
    let _ = &mut previous_attributes;
    // The proof was checked inside the request that armed tracking, so a match
    // means every later Studio edit is caught by the transaction's generation
    // checks and the last verified files are a complete baseline.
    let verified_baseline = match candidate_parts {
        Some((_, project)) if initial_state["pushProofMatches"] == true => Some(project),
        Some(_) => {
            log_global(
                5,
                format_args!(
                    "[renium] full push cache miss: {}",
                    initial_state["pushProofMismatch"]
                        .as_str()
                        .unwrap_or("Studio proof unavailable")
                ),
            );
            None
        }
        None => None,
    };
    let phase = Instant::now();
    // Declared before the tracking lease so unwinding releases Lua tracking
    // before native observation. On cancellation the native guard can promote
    // surviving listeners before disconnecting its shared signal.
    let mut tracking_release = StudioTrackingGuardRelease {
        bridge,
        runtime_id: guard.runtime_id.clone(),
        guard_id: guard.tracking_guard_id.clone(),
        proof: None,
        retained_proof: None,
        retained_proof_local: false,
    };
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
    // Source capture is read-only and independent of the private Studio stage.
    // Keep Studio calls on this thread, with its selected runtime and lease.
    let (captured, project) = std::thread::scope(|scope| {
        let project = scope.spawn(|| capture_snapshot(&root, &source_paths));
        let captured = if let Some(baseline) = verified_baseline {
            Ok((stage, baseline))
        } else if !requires_stage
            && stage
                .loaded
                .as_ref()
                .is_none_or(|loaded| loaded.project.adapters.is_empty())
        {
            capture_studio_services_in_memory(context, bridge, services, &stage)
                .map(|studio| (stage, studio))
        } else {
            capture_studio_services_with_stage(context, bridge, services, false, stage)
        };
        (
            captured,
            project.join().expect("Source capture worker panicked"),
        )
    });
    let (stage, studio) = captured?;
    let project = project?;
    log_reconcile_timing("full push capture", phase);
    let phase = Instant::now();
    let mut prepared_settings = HashMap::new();
    let differences = if replace {
        let paths = project_replacement_paths(&project, &studio);
        // Replacement writes the complete captured snapshot. Its private staging
        // files do not depend on decoding or matching retained Studio objects.
        let (prepared, staged) = rayon::join(
            || prepare_project_replacement(&project, &studio, &paths, &mut prepared_settings),
            || stage_snapshot_paths(&stage.project_root, &paths, &project),
        );
        let unchanged_settings = prepared?;
        staged?;
        // A full push covers every service, but only replaces services whose
        // authored contents cannot be reused. Unchanged script files still
        // accompany native replacements so their external Source markers resolve.
        let native_services = prepared_settings
            .iter()
            .filter(|(_, change)| {
                !change.current.instances.is_empty()
                    && service_has_only_native_containers(&change.previous)
                    && !settings_documents_equivalent(&change.current, &change.previous)
            })
            .map(|(path, change)| settings_service_name(path, &change.current, &change.previous))
            .collect::<Result<HashSet<_>>>()?;
        let paths = paths
            .into_iter()
            .filter(|path| {
                if unchanged_settings.contains(path) {
                    return false;
                }
                if let Some(change) = prepared_settings.get(path) {
                    return !settings_documents_equivalent(&change.current, &change.previous);
                }
                let native = services_for_snapshot_paths(context, &HashSet::from([path.clone()]))
                    .iter()
                    .any(|service| native_services.contains(service));
                native
                    || !entries_equivalent(
                        path,
                        studio.entries.get(path),
                        project.entries.get(path),
                    )
            })
            .collect::<HashSet<_>>();
        prepared_settings.retain(|path, _| paths.contains(path));
        paths
    } else {
        snapshot_differences_prepared(&project, &studio, Some(&mut prepared_settings))?
    };
    log_reconcile_timing(
        if replace {
            "full replacement preparation"
        } else {
            "full push comparison"
        },
        phase,
    );
    for path in &differences {
        if let Some(change) = prepared_settings.get(path) {
            super::verify::note_saved_field_differences(
                crate::settings::equivalence::saved_field_differences(
                    &change.current,
                    &change.previous,
                ),
            );
        }
    }
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
            &mut _native_attributes,
        );
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let phase = Instant::now();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &project,
        &differences,
        &prepared_settings,
        true,
        false,
    )?;
    log_reconcile_timing("full push plan", phase);
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
            &mut _native_attributes,
        );
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let mutation_paths = differences.clone();
    let phase = Instant::now();
    let history = history::SyncHistory::begin(&root, &src_dir, &studio, &mutation_paths)?;
    if !replace {
        apply_snapshot_paths(&stage.project_root, &mutation_paths, &project)?;
    }
    let mut prepared_documents = HashMap::with_capacity(prepared_settings.len());
    let mut prepared_verification = HashMap::with_capacity(prepared_settings.len());
    for (path, change) in prepared_settings {
        let service = settings_service_name(&path, &change.current, &change.previous)?;
        let desired = Arc::new(change.current);
        let native = plan.initial_native_services.contains(&service);
        prepared_documents.insert(service, Arc::clone(&desired));
        if !native {
            prepared_verification.insert(
                path,
                PreparedPushVerification {
                    previous: change.previous,
                    desired,
                },
            );
        }
    }
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
    pushed.summary.insert(
        "historyId".into(),
        Value::String(history.commit(&project, &pushed.generated)?),
    );
    log_reconcile_timing("full push mutation", phase);

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

    let current = capture_snapshot(&root, stage.publish_paths())?;
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
    if !requires_stage
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
        }
        std::thread::scope(|scope| {
            // No remaining readback consumes these trees. Release their large
            // allocations on the existing parallel path while Studio closes the
            // observation lease; join cleanup before returning to the caller.
            scope.spawn(move || {
                prepared_verification
                    .into_par_iter()
                    .for_each(|(_, verified)| {
                        if let Some(desired) = Arc::into_inner(verified.desired) {
                            drop_settings_documents(verified.previous, desired);
                        } else {
                            drop_settings_document(verified.previous);
                        }
                    });
                drop(stage);
            });
            acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)
        })?;
        #[cfg(any(windows, target_os = "macos"))]
        retain_verified_full_push(
            bridge,
            cache_input.as_ref(),
            &source_paths,
            project,
            &tracking_release,
            #[cfg(windows)]
            &mut _native_attributes,
        );
        return Ok(pushed.summary);
    }
    let verification_services = services_for_snapshot_paths(context, &verification_paths);
    let phase = Instant::now();
    let readback_stage = if requires_stage {
        ExportProjectStage::create(&root, &src_dir, &verification_services)?
    } else {
        ExportProjectStage::create_for_comparison(&root, &src_dir, &verification_services)?
    };
    let readback = if !requires_stage
        && readback_stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty())
    {
        capture_studio_services_in_memory(context, bridge, &verification_services, &readback_stage)?
    } else {
        capture_studio_services_with_stage(
            context,
            bridge,
            &verification_services,
            false,
            readback_stage,
        )?
        .1
    };
    log_reconcile_timing("full push readback", phase);
    let phase = Instant::now();
    // These documents were already aligned to the exact requested file bytes.
    // Generated values or a later file edit invalidate that proof, not merely
    // the observed Studio snapshot (which is always read and aligned anew).
    prepared_verification.retain(|path, _| {
        entries_equivalent(path, project.entries.get(path), current.entries.get(path))
    });
    let (mismatches, details) = snapshot_intended_delta_mismatches(
        &studio,
        &current,
        &readback,
        &verification_paths,
        &mut prepared_verification,
    )?;
    log_reconcile_timing("full push verification", phase);
    if !mismatches.is_empty() {
        return Err(retention_failure(
            "pushed project changes",
            &mismatches,
            details.as_deref(),
        ));
    }
    let mut expected_after_push = project;
    expected_after_push.entries.extend(pushed.generated.entries);
    if expected_after_push == current && !requires_stage && replacement.is_none() {
        // Readback verified the remaining fields. The commit proof predates
        // that readback, so retaining it still requires Studio to have remained
        // unchanged throughout verification (including its asynchronous reads).
        // Accepted generated settings are also verified by readback; only file
        // changes outside that exact result invalidate the source snapshot.
        tracking_release.proof = pushed.summary.remove("verifiedPushProof");
    }
    acknowledge_verified_push(bridge, services, &guard, &mut tracking_release)?;
    #[cfg(any(windows, target_os = "macos"))]
    retain_verified_full_push(
        bridge,
        cache_input.as_ref(),
        &source_paths,
        current,
        &tracking_release,
        #[cfg(windows)]
        &mut _native_attributes,
    );
    Ok(pushed.summary)
}

pub(crate) fn reopened_push_context(
    context: &BoundContext,
    summary: &Map<String, Value>,
) -> Result<Option<BoundContext>> {
    let Some(reopened) = summary.get("protectedOfflineApply") else {
        return Ok(None);
    };
    let previous = reopened["previousRuntimeId"].as_str();
    let runtime = reopened["reopenedRuntimeId"]
        .as_str()
        .filter(|value| !value.is_empty());
    anyhow::ensure!(
        reopened["ok"] == true
            && previous.is_some()
            && previous == context.runtime_id.as_deref()
            && runtime.is_some()
            && runtime != previous,
        "Protected snapshot did not identify its replacement Studio runtime"
    );
    let mut replacement = context.clone();
    replacement.runtime_id = runtime.map(str::to_string);
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
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    export_snapshots_with_warm_bridge(args, bridge, &info, 0.0, false)?;
    let regenerate_sourcemap = generate_sourcemap && stage.capture_sourcemap_needs_regeneration();
    stage.finish_projection(regenerate_sourcemap)?;
    Ok(stage)
}

pub(crate) fn capture_studio_services_in_memory(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    stage: &ExportProjectStage,
) -> Result<ProjectSnapshot> {
    pin_edit_runtime(context, bridge)?;
    let mut args = automation_pull_args(context, &json!({"services": services}))?;
    args.project_root.clone_from(&stage.import_project_root);
    args.src_dir.clone_from(&stage.import_src_dir);
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    let src_root = stage.import_project_root.join(&stage.import_src_dir);
    let projections = capture_exported_services(&args, bridge, &info, |service, state| {
        service_projection_in_memory(&state, &src_root, service)
    })?;
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
