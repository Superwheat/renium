use super::*;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
    Symlink { target: PathBuf, directory: bool },
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectSnapshot {
    pub(crate) entries: BTreeMap<PathBuf, SnapshotEntry>,
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct VerifiedFullPush {
    pub(crate) connections: Vec<usize>,
    pub(crate) input: Value,
    pub(crate) project: ProjectSnapshot,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) proof: Value,
    pub(crate) created: Instant,
    #[cfg(windows)]
    pub(crate) _attributes: Option<crate::studio::native::serializer::AttributeGuard>,
}

#[cfg(any(windows, target_os = "macos"))]
impl VerifiedFullPush {
    pub(crate) fn expired(&self) -> bool {
        self.created.elapsed() >= Duration::from_secs(60)
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) type VerifiedFullPushCache = Arc<Mutex<HashMap<String, VerifiedFullPush>>>;

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn full_push_cache_input(
    args: &PushEditorChangesArgs,
    services: &[String],
) -> Result<Value> {
    let loaded = config::try_load_project(None, Some(&args.project.project_root))?;
    Ok(json!({
        "root": args.project.project_root, "source": args.project.src_root, "services": services,
        "paths": args.paths, "changedPaths": args.changed_paths, "changedPathFiles": args.changed_paths_files,
        "ids": args.target_settings_ids, "idFiles": args.target_settings_id_files, "properties": args.target_properties,
        "upsert": args.upsert_instances_only, "verify": args.verify_sources,
        "linkCache": args.link_cache_dir, "overridePackages": args.override_packages,
        "configuration": loaded.as_ref().map(|loaded| &loaded.project),
    }))
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct VerifiedFullPushCandidate {
    pub(crate) cached: VerifiedFullPush,
    pub(crate) files_unchanged: bool,
}

// The retained proof says Studio still equals the files of the last verified
// push. Those files are then a complete baseline: an unchanged tree needs no
// push at all, and a changed tree needs only a file diff instead of a fresh
// Studio capture.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn take_verified_full_push(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    args: &PushEditorChangesArgs,
) -> Result<Option<VerifiedFullPushCandidate>> {
    let Some(runtime) = context.runtime_id.as_deref() else {
        return Ok(None);
    };
    // Take ownership before any RPC or native cleanup: another place must not
    // wait behind this place's verification or an expired observer's teardown.
    let cached = bridge.verified_full_pushes.lock_recover().remove(runtime);
    let Some(cached) = cached else {
        return Ok(None);
    };
    let invalidated_by = if cached.expired() {
        Some("proof expired")
    } else if cached.connections != bridge.runtime_connection_signature(runtime) {
        Some("Studio connection changed")
    } else if cached.input != full_push_cache_input(args, services)? {
        Some("push configuration changed")
    } else {
        None
    };
    if let Some(reason) = invalidated_by {
        log_global(5, format_args!("[renium] full push cache miss: {reason}"));
        return Ok(None);
    }
    let files_unchanged =
        cached.project == capture_snapshot(&args.project.project_root, &cached.paths)?;
    Ok(Some(VerifiedFullPushCandidate {
        cached,
        files_unchanged,
    }))
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn verified_full_push_proof_matches(
    context: &BoundContext,
    bridge: &BridgeServer,
    proof: &Value,
) -> Result<bool> {
    let runtime = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    pin_edit_runtime(context, bridge)?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({"start": false, "verifyPushProof": proof}),
        BridgeTarget::Edit,
        runtime,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    if state["pushProofMatches"] == true {
        return Ok(true);
    }
    log_global(
        5,
        format_args!(
            "[renium] full push cache miss: {}",
            state["pushProofMismatch"]
                .as_str()
                .unwrap_or("Studio proof unavailable")
        ),
    );
    Ok(false)
}

pub(crate) fn capture_push_proof(
    bridge: &BridgeServer,
    guard: &StudioChangeGuard,
) -> Result<Option<Value>> {
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": false, "capturePushProof": true }),
        BridgeTarget::Edit,
        &guard.runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    Ok(state
        .get("verifiedPushProof")
        .filter(|value| !value.is_null())
        .cloned())
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn retain_verified_full_push(
    bridge: &BridgeServer,
    input: Option<&Value>,
    paths: &[PathBuf],
    project: ProjectSnapshot,
    release: &StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] attributes: &mut Option<crate::studio::native::serializer::AttributeGuard>,
) {
    let (Some(input), Some(proof)) = (input, release.retained_proof.as_ref()) else {
        log_global(
            5,
            format_args!(
                "[renium] full push proof not retained: input={} proof={}",
                input.is_some(),
                release.retained_proof.is_some()
            ),
        );
        return;
    };
    #[cfg(windows)]
    let attributes = attributes.take();
    #[cfg(windows)]
    if attributes.is_none() && !release.retained_proof_local {
        log_global(
            5,
            format_args!("[renium] full push proof not retained: no native attribute guard"),
        );
        return;
    }
    let cached = VerifiedFullPush {
        connections: bridge.runtime_connection_signature(&release.runtime_id),
        input: input.clone(),
        project,
        paths: paths.to_vec(),
        proof: proof.clone(),
        created: Instant::now(),
        #[cfg(windows)]
        _attributes: attributes,
    };
    let evicted = {
        let mut entries = bridge.verified_full_pushes.lock_recover();
        let expired = entries
            .iter()
            .filter(|(_, value)| value.created.elapsed() >= Duration::from_secs(60))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let mut evicted = expired
            .into_iter()
            .filter_map(|key| entries.remove(&key))
            .collect::<Vec<_>>();
        if entries.len() >= 8
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, value)| value.created)
                .map(|(key, _)| key.clone())
        {
            evicted.extend(entries.remove(&oldest));
        }
        evicted.extend(entries.insert(release.runtime_id.clone(), cached));
        evicted
    };
    drop(evicted);
}

#[derive(Default)]
pub(crate) struct ReconcilePushPlan {
    pub(crate) initial_native_services: HashSet<String>,
    pub(crate) changed_paths: Vec<PathBuf>,
    pub(crate) target_settings_ids: Vec<String>,
    pub(crate) recreated_settings_ids: HashSet<String>,
    pub(crate) previous_class_names: HashMap<(String, String), String>,
    pub(crate) previous_paths: HashMap<(String, String), EditorInstancePath>,
    pub(crate) instance_deletes: Vec<EditorInstanceChange>,
    pub(crate) property_removals: Vec<EditorPropertyChange>,
    pub(crate) geometry_properties: HashMap<(String, String), Vec<String>>,
    pub(crate) attribute_only_instances: HashSet<(String, String)>,
    pub(crate) in_place_instances: HashSet<(String, String)>,
    pub(crate) unchanged_native_root_properties: HashMap<(String, String), Vec<String>>,
}

pub(crate) struct PreparedEditorSettingsChange {
    pub(crate) previous: SettingsBytecode,
    pub(crate) current: SettingsBytecode,
}

pub(crate) struct PreparedPushVerification {
    pub(crate) previous: SettingsBytecode,
    pub(crate) desired: Arc<SettingsBytecode>,
}

#[derive(Default)]
pub(crate) struct MergeChanges {
    pub(crate) editor: HashSet<PathBuf>,
    pub(crate) studio: HashSet<PathBuf>,
}

pub(crate) struct MergedEntry {
    pub(crate) value: Option<SnapshotEntry>,
    pub(crate) editor_changed: bool,
    pub(crate) studio_changed: bool,
}

pub(crate) struct SnapshotSides<'a> {
    pub(crate) baseline: Option<&'a ProjectSnapshot>,
    pub(crate) editor: &'a ProjectSnapshot,
    pub(crate) studio: &'a ProjectSnapshot,
}

impl PairRecord {
    pub(crate) fn note_setup(&mut self, setup: &PairSetup) {
        self.last_runtime_id.clone_from(&setup.runtime_id);
        self.local_file_stamp.clone_from(&setup.local_file_stamp);
        self.local_file_digest.clone_from(&setup.local_file_digest);
    }
}

impl ReconcilePushPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.changed_paths.is_empty()
            && self.target_settings_ids.is_empty()
            && self.recreated_settings_ids.is_empty()
            && self.previous_class_names.is_empty()
            && self.previous_paths.is_empty()
            && self.instance_deletes.is_empty()
            && self.property_removals.is_empty()
    }
}
