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

/// Paths inside any of a set of scopes. Looking up each ancestor keeps a push
/// that names every file linear instead of scanning all scopes per path.
pub(crate) struct ScopeSet(HashSet<PathBuf>);

impl ScopeSet {
    pub(crate) fn new<'a>(scopes: impl IntoIterator<Item = &'a PathBuf>) -> Self {
        Self(scopes.into_iter().cloned().collect())
    }

    pub(crate) fn contains(&self, path: &Path) -> bool {
        path.ancestors().any(|ancestor| self.0.contains(ancestor))
    }
}

/// How long the native helper observes attributes for a Windows proof: it
/// refuses a longer lifetime, and a proof ends with its observer.
#[cfg(windows)]
pub(crate) const NATIVE_RELAY_LIFETIME: Duration = Duration::from_secs(120);

/// A baseline on a native observer ends this long before the observer, so
/// Studio stops the tracking it kept for the baseline before the observer's
/// end would hand every instance to local listeners.
#[cfg(windows)]
const NATIVE_RELAY_MARGIN: Duration = Duration::from_secs(5);

/// How long a baseline on local observation lasts unused. Its proof does not
/// age; the limit only ends the tracking Studio keeps for a daemon that may
/// have gone, and each use renews it.
pub(crate) const BASELINE_IDLE: Duration = Duration::from_secs(30 * 60);

/// The native attribute observer a Windows proof stands on, and when the
/// baseline on it ends.
#[cfg(windows)]
pub(crate) struct NativeRelay {
    _guard: crate::studio::native::serializer::AttributeGuard,
    pub(crate) ends: Instant,
}

#[cfg(windows)]
impl NativeRelay {
    /// An observer the helper started after `armed`.
    pub(crate) fn armed(
        guard: crate::studio::native::serializer::AttributeGuard,
        armed: Instant,
    ) -> Self {
        Self {
            _guard: guard,
            ends: armed + NATIVE_RELAY_LIFETIME - NATIVE_RELAY_MARGIN,
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct VerifiedFullPush {
    pub(crate) connections: Vec<usize>,
    pub(crate) input: Value,
    pub(crate) project: ProjectSnapshot,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) proof: Value,
    pub(crate) expires: Instant,
    /// Whether the files are what a pull writes from Studio. A whole push
    /// leaves them only what a push sends: it sends no Game Settings and not
    /// the viewport's camera, so Studio may hold other values there.
    pub(crate) exact: bool,
    #[cfg(windows)]
    pub(crate) relay: Option<NativeRelay>,
}

#[cfg(any(windows, target_os = "macos"))]
impl VerifiedFullPush {
    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.expires
    }

    /// A baseline used again lasts as long as a new one on its observer.
    pub(crate) fn renew(&mut self) {
        self.expires = baseline_expiry(
            #[cfg(windows)]
            self.relay.as_ref(),
        );
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn baseline_expiry(#[cfg(windows)] relay: Option<&NativeRelay>) -> Instant {
    #[cfg(windows)]
    if let Some(relay) = relay {
        return relay.ends;
    }
    Instant::now() + BASELINE_IDLE
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
        "upsert": args.upsert_instances_only,
        "linkCache": args.link_cache_dir, "overridePackages": args.override_packages,
        "configuration": loaded.as_ref().map(|loaded| &loaded.project),
    }))
}

/// The input of a whole push of the project `args` reads: a pull and a push
/// of chosen paths keep and use the baseline of `rbx ps`.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn plain_push_input(args: &PushEditorChangesArgs) -> Result<Value> {
    let mut plain = PushEditorChangesArgs::new(args.project.clone(), args.bridge.clone());
    plain.link_cache_dir.clone_from(&args.link_cache_dir);
    plain.override_packages = args.override_packages;
    full_push_cache_input(&plain, &sync_services())
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct VerifiedFullPushCandidate {
    pub(crate) cached: VerifiedFullPush,
    pub(crate) files_unchanged: bool,
}

/// Takes the runtime's baseline when it still applies to a push of `input`.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn take_baseline(
    context: &BoundContext,
    bridge: &BridgeServer,
    input: &Value,
) -> Option<VerifiedFullPush> {
    let runtime = context.runtime_id.as_deref()?;
    // Take ownership before any RPC or native cleanup: another place must not
    // wait behind this place's verification or an expired observer's teardown.
    let cached = bridge.verified_full_pushes.lock_recover().remove(runtime)?;
    let invalidated_by = if cached.expired() {
        Some("proof expired")
    } else if cached.connections != bridge.runtime_connection_signature(runtime) {
        Some("Studio connection changed")
    } else if cached.input != *input {
        Some("push configuration changed")
    } else {
        None
    };
    if let Some(reason) = invalidated_by {
        log_global(5, format_args!("[renium] full push cache miss: {reason}"));
        return None;
    }
    Some(cached)
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
    let input = full_push_cache_input(args, services)?;
    let Some(cached) = take_baseline(context, bridge, &input) else {
        return Ok(None);
    };
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
        json!({"start": false, "verifyPushProof": proof, "retainSeconds": BASELINE_IDLE.as_secs()}),
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

/// Holds `entry` as the runtime's baseline, dropping expired ones and the
/// oldest past eight.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn keep_baseline(
    bridge: &BridgeServer,
    runtime: &str,
    entry: VerifiedFullPush,
    after: &str,
) {
    log_global(
        5,
        format_args!(
            "[renium] baseline: kept after {after} for {}s",
            entry
                .expires
                .saturating_duration_since(Instant::now())
                .as_secs()
        ),
    );
    let evicted = {
        let mut entries = bridge.verified_full_pushes.lock_recover();
        let expired = entries
            .iter()
            .filter(|(_, value)| value.expired())
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let mut evicted = expired
            .into_iter()
            .filter_map(|key| entries.remove(&key))
            .collect::<Vec<_>>();
        if entries.len() >= 8
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, value)| value.expires)
                .map(|(key, _)| key.clone())
        {
            evicted.extend(entries.remove(&oldest));
        }
        evicted.extend(entries.insert(runtime.to_string(), entry));
        evicted
    };
    drop(evicted);
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn retain_verified_full_push(
    bridge: &BridgeServer,
    input: Option<&Value>,
    paths: &[PathBuf],
    project: ProjectSnapshot,
    release: &StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] relay: &mut Option<NativeRelay>,
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
    if !release.studio_unchanged {
        log_global(
            5,
            format_args!("[renium] full push proof not retained: Studio changed during the push"),
        );
        return;
    }
    #[cfg(windows)]
    let relay = relay.take();
    #[cfg(windows)]
    if relay.is_none() && !release.retained_proof_local {
        log_global(
            5,
            format_args!("[renium] full push proof not retained: no native attribute guard"),
        );
        return;
    }
    // Studio held the commit's proof at the release, and no tracked change
    // reached a synced service between the lease and the release: Studio
    // holds what the push verified, which is these files.
    let entry = VerifiedFullPush {
        connections: bridge.runtime_connection_signature(&release.runtime_id),
        input: input.clone(),
        project,
        paths: paths.to_vec(),
        proof: proof.clone(),
        expires: baseline_expiry(
            #[cfg(windows)]
            relay.as_ref(),
        ),
        exact: false,
        #[cfg(windows)]
        relay,
    };
    keep_baseline(bridge, &release.runtime_id, entry, "full push");
}

/// A pull's use of the held baseline.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) enum PullBaseline<'a> {
    /// Studio and the files still hold the baseline: the pull writes nothing.
    Unchanged,
    /// The pull captures under this lease, which may keep a new baseline.
    Capture(Box<PullLease<'a>>),
}

/// The tracking lease of a capturing pull and the proof it took before the
/// capture. Its fields drop in order: Lua tracking ends before native
/// observation.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct PullLease<'a> {
    guard: StudioChangeGuard,
    release: StudioTrackingGuardRelease<'a>,
    root: PathBuf,
    input: Value,
    #[cfg(windows)]
    relay: Option<NativeRelay>,
}

/// Whether a pull writes Studio's whole export as it stands, which only then
/// is a baseline of `rbx ps`: every synced service, no file kept back, no
/// layout that transforms the export (adapters, a temporary stage, linked
/// packages).
#[cfg(any(windows, target_os = "macos"))]
fn pull_is_whole(args: &crate::cli::PullArgs) -> Result<bool> {
    let mut services = crate::snapshot::import::parse_services(&args.services)?;
    services.sort();
    let mut synced = sync_services();
    synced.sort();
    if services != synced
        || !args.preserve_paths.is_empty()
        || args.export_all_properties
        || args.keep_local_only_scripts
        || args.layout_src_root.is_some()
        || args.project_root.join("renium-link.json").exists()
    {
        return Ok(false);
    }
    Ok(
        match config::try_load_project(None, Some(&args.project_root))? {
            Some(loaded) => {
                loaded.project.adapters.is_empty()
                    && !config::project_requires_temporary_stage(&loaded)?
            }
            None => true,
        },
    )
}

/// How a pull uses the held baseline: it answers from one Studio and the
/// files still hold, and otherwise takes a lease and a proof before its
/// capture. `None` where the pull cannot keep one.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn pull_baseline<'a>(
    context: &BoundContext,
    bridge: &'a BridgeServer,
    args: &crate::cli::PullArgs,
) -> Result<Option<PullBaseline<'a>>> {
    let Some(runtime) = context.runtime_id.as_deref() else {
        return Ok(None);
    };
    if !pull_is_whole(args)? {
        return Ok(None);
    }
    let project = crate::cli::ProjectSourceArgs {
        project_root: args.project_root.clone(),
        src_root: args.src_dir.clone(),
    };
    let input = plain_push_input(&PushEditorChangesArgs::new(project, args.bridge.clone()))?;
    let mut held = take_baseline(context, bridge, &input);
    let unchanged = match held.as_ref() {
        Some(entry) => {
            entry.exact
                && capture_snapshot(&args.project_root, &entry.paths)? == entry.project
                && verified_full_push_proof_matches(context, bridge, &entry.proof)?
        }
        None => false,
    };
    if unchanged && let Some(mut entry) = held.take() {
        // Studio still holds the proof it held when it and these files were
        // one, and the files are still those files: a capture would find
        // nothing to write.
        entry.renew();
        log_global(5, format_args!("[renium] baseline: pull unchanged"));
        bridge
            .verified_full_pushes
            .lock_recover()
            .insert(runtime.to_string(), entry);
        return Ok(Some(PullBaseline::Unchanged));
    }
    let services = sync_services();
    #[cfg(windows)]
    let mut previous = held.as_mut().and_then(|entry| entry.relay.take());
    #[cfg(windows)]
    let mut relay = None;
    let (guard, state) = current_studio_change_guard_verifying(
        context,
        bridge,
        Some(services.as_slice()),
        held.as_ref().map(|entry| &entry.proof),
        |state| {
            #[cfg(windows)]
            super::push_flow::arm_native_attribute_relay(
                state,
                bridge,
                &services,
                &mut previous,
                &mut relay,
            )?;
            #[cfg(not(windows))]
            let _ = state;
            Ok(())
        },
    )?;
    #[cfg(windows)]
    if state["nativeRelayContinued"] == true {
        relay = previous.take();
    }
    #[cfg(not(windows))]
    let _ = state;
    let mut release =
        StudioTrackingGuardRelease::new(bridge, &guard.runtime_id, guard.tracking_guard_id.clone());
    #[cfg(windows)]
    {
        release.retain_until = relay.as_ref().map(|relay| relay.ends);
    }
    release.proof = capture_push_proof(bridge, &guard)?;
    Ok(Some(PullBaseline::Capture(Box::new(PullLease {
        guard,
        release,
        root: args.project_root.clone(),
        input,
        #[cfg(windows)]
        relay,
    }))))
}

#[cfg(any(windows, target_os = "macos"))]
impl PullLease<'_> {
    /// Keeps the files `published` wrote as the baseline when Studio held the
    /// proof from before the capture until now.
    pub(crate) fn keep(
        mut self,
        published: &crate::snapshot::export::PublishedProjectChanges,
    ) -> Result<()> {
        let not_kept = |reason: &str| {
            log_global(
                5,
                format_args!("[renium] baseline: not kept after pull: {reason}"),
            );
        };
        let Some(proof) = self.release.proof.clone() else {
            not_kept("Studio gave no proof");
            return Ok(());
        };
        if !published.preserved.is_empty()
            || !published.read_only_links_restored.is_empty()
            || !published.studio_overrides.is_empty()
        {
            not_kept("the pull kept files back");
            return Ok(());
        }
        let project = capture_snapshot(&self.root, &published.publish_paths)?;
        if !publish_settled(&project, &published.settled) {
            not_kept("the files changed after the pull wrote them");
            return Ok(());
        }
        let bridge = self.release.bridge;
        release_tracking_lease(bridge, &self.guard, &mut self.release, json!({}))?;
        if self.release.retained_proof.as_ref() != Some(&proof) || !self.release.studio_unchanged {
            not_kept("Studio changed during the pull");
            return Ok(());
        }
        #[cfg(windows)]
        if self.relay.is_none() && !self.release.retained_proof_local {
            not_kept("no native attribute guard");
            return Ok(());
        }
        // The capture came after the proof, and Studio held that proof with
        // no tracked change until the files were written and read back as the
        // publish left them: the files hold what Studio holds at this proof.
        let entry = VerifiedFullPush {
            connections: bridge.runtime_connection_signature(&self.release.runtime_id),
            input: self.input,
            project,
            paths: published.publish_paths.clone(),
            proof,
            expires: baseline_expiry(
                #[cfg(windows)]
                self.relay.as_ref(),
            ),
            exact: true,
            #[cfg(windows)]
            relay: self.relay,
        };
        keep_baseline(bridge, &self.release.runtime_id, entry, "pull");
        Ok(())
    }
}

/// Whether `files` holds exactly what a publish left at its paths.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn publish_settled(
    files: &ProjectSnapshot,
    settled: &BTreeMap<PathBuf, PublishEntryState>,
) -> bool {
    let settled = settled
        .iter()
        .filter(|(path, _)| !derived_project_path(path))
        .collect::<Vec<_>>();
    settled.len() == files.entries.len()
        && settled.par_iter().all(|(path, state)| {
            files.entries.get(*path).is_some_and(|entry| {
                *state
                    == &match entry {
                        SnapshotEntry::Directory => PublishEntryState::Directory,
                        SnapshotEntry::File(bytes) => {
                            crate::snapshot::export::file_publish_state(bytes)
                        }
                        SnapshotEntry::Symlink { target, .. } => {
                            PublishEntryState::Symlink(target.clone())
                        }
                    }
            })
        })
}

#[derive(Default)]
pub(crate) struct ReconcilePushPlan {
    pub(crate) initial_native_services: HashSet<String>,
    pub(crate) changed_paths: Vec<PathBuf>,
    pub(crate) target_settings_ids: Vec<String>,
    pub(crate) recreated_settings_ids: HashSet<String>,
    /// The recreated identities by service: identities repeat across stores,
    /// so an unchanged instance of another service that shares the string is
    /// not created anew.
    pub(crate) recreated_instances: HashSet<(String, String)>,
    pub(crate) previous_class_names: HashMap<(String, String), String>,
    pub(crate) previous_paths: HashMap<(String, String), EditorInstancePath>,
    /// The identity Studio's capture gave each instance a previous path names,
    /// by service and merged identity. Studio resolves it before the path, so
    /// a sibling group whose capture order differs from Studio's child order
    /// still finds the right instance.
    pub(crate) previous_identities: HashMap<(String, String), String>,
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
    /// The files' document the push was made from. `None` for one that left
    /// instances in its store file, once the import has carried them to
    /// Studio: a verification reads that file.
    pub(crate) desired: Option<Arc<SettingsBytecode>>,
}

#[derive(Default)]
pub(crate) struct MergeChanges {
    pub(crate) editor: HashSet<PathBuf>,
    pub(crate) studio: HashSet<PathBuf>,
    /// Studio's settings documents with the identities the merge paired them
    /// to, for every path whose Studio side changed. The push plan compares
    /// against these so its previous locations follow the merge's pairing.
    pub(crate) aligned_studio: HashMap<PathBuf, SettingsBytecode>,
    /// Where each script of the files' side stands, by store and by the
    /// identity the merge gave it: a store may name another instance by an
    /// identity the merge had to give away.
    pub(crate) editor_script_paths: HashMap<PathBuf, HashMap<String, PathBuf>>,
    /// The decisions the merge took without effect.
    pub(crate) moot_decisions: Vec<MootDecision>,
}

pub(crate) struct MergedEntry {
    pub(crate) value: Option<SnapshotEntry>,
    pub(crate) editor_changed: bool,
    pub(crate) studio_changed: bool,
    pub(crate) aligned_studio: Option<SettingsBytecode>,
    pub(crate) editor_script_paths: Option<HashMap<String, PathBuf>>,
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
