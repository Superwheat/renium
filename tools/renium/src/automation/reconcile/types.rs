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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StructuralPathChange {
    WholeService,
    Children {
        parent: Vec<String>,
        child_names: Option<BTreeSet<String>>,
    },
}

#[cfg(any(windows, target_os = "macos"))]
impl StructuralPathChange {
    pub(crate) fn matches_ref_path(&self, service: &str, segments: &[String]) -> bool {
        if segments.first().map(String::as_str) != Some(service) {
            return false;
        }
        match self {
            Self::WholeService => true,
            Self::Children {
                parent,
                child_names,
            } => {
                if segments.len() <= parent.len() || segments[..parent.len()] != parent[..] {
                    return false;
                }
                match child_names {
                    None => true,
                    Some(names) => names.contains(&segments[parent.len()]),
                }
            }
        }
    }

    fn append_needles(&self, service: &str, needles: &mut Vec<Vec<String>>) {
        match self {
            Self::WholeService => needles.push(vec![service.to_string()]),
            Self::Children {
                parent,
                child_names,
            } => {
                if parent.is_empty() {
                    needles.push(vec![service.to_string()]);
                    return;
                }
                match child_names {
                    Some(names) if !names.is_empty() && names.len() <= 64 => {
                        for name in names {
                            let mut needle = Vec::with_capacity(parent.len() + 1);
                            needle.extend(parent.iter().cloned());
                            needle.push(name.clone());
                            needles.push(needle);
                        }
                    }
                    _ => needles.push(parent.clone()),
                }
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) type StructuralChanges = BTreeMap<String, Vec<StructuralPathChange>>;

#[cfg(any(windows, target_os = "macos"))]
#[derive(Clone)]
pub(crate) struct PushedFromPull {
    pub(crate) sourcemap: SourcemapState,
    pub(crate) services: BTreeSet<String>,
    pub(crate) paths_moved: BTreeSet<String>,
    pub(crate) structural: StructuralChanges,
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct VerifiedFullPush {
    pub(crate) connections: Vec<usize>,
    pub(crate) input: Value,
    pub(crate) project: ProjectSnapshot,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) proof: Value,
    pub(crate) expires: Instant,
    /// Where the files are what a pull writes from Studio, the sourcemap as
    /// the last pull on them left it: a pull answered from the baseline
    /// writes the services it does not capture into the sourcemap as the
    /// file holds them. A whole push leaves the files only what a push
    /// sends: it sends no Game Settings and not the viewport's camera, so
    /// Studio may hold other values there, unless a settlement published
    /// them from its readback. A push of chosen paths keeps the sourcemap
    /// only where it changed script sources alone, each held by Studio in
    /// its file's bytes.
    pub(crate) sourcemap: Option<Box<SourcemapState>>,
    pub(crate) pushed_from_pull: Option<Box<PushedFromPull>>,
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
    let links = crate::project::package_links::link_cache_fingerprint(
        &args.project.project_root,
        &args.project.src_root,
        args.link_cache_dir.as_deref(),
        args.override_packages,
    )?;
    Ok(json!({
        "root": args.project.project_root, "source": args.project.src_root, "services": services,
        "paths": args.paths, "changedPaths": args.changed_paths, "changedPathFiles": args.changed_paths_files,
        "ids": args.target_settings_ids, "idFiles": args.target_settings_id_files, "properties": args.target_properties,
        "upsert": args.upsert_instances_only,
        "linkCache": args.link_cache_dir, "overridePackages": args.override_packages,
        "links": links,
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
        snapshot_matches_disk(&args.project.project_root, &cached.paths, &cached.project)?;
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
    verified_full_push_proof_matches_with_options(context, bridge, proof, false)
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn verified_full_push_proof_matches_with_options(
    context: &BoundContext,
    bridge: &BridgeServer,
    proof: &Value,
    defer_terrain_proof: bool,
) -> Result<bool> {
    let runtime = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    pin_edit_runtime(context, bridge)?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "start": false,
            "verifyPushProof": proof,
            "deferTerrainProof": defer_terrain_proof,
            "retainSeconds": BASELINE_IDLE.as_secs(),
        }),
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
pub(crate) fn capture_push_proof_for_settlement(
    bridge: &BridgeServer,
    guard: &StudioChangeGuard,
) -> Result<Option<Value>> {
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": false, "capturePushProof": true, "keepForSettlement": true }),
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
    retain_full_push(
        bridge,
        input,
        paths,
        (project, None),
        release,
        #[cfg(windows)]
        relay,
    );
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct RetainedPushScope {
    pub(crate) project: ProjectSnapshot,
    pub(crate) sourcemap: Option<Box<SourcemapState>>,
    pub(crate) pushed_from_pull: Option<Box<PushedFromPull>>,
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn retain_full_push(
    bridge: &BridgeServer,
    input: Option<&Value>,
    paths: &[PathBuf],
    (project, sourcemap): (ProjectSnapshot, Option<Box<SourcemapState>>),
    release: &StudioTrackingGuardRelease<'_>,
    #[cfg(windows)] relay: &mut Option<NativeRelay>,
) {
    retain_full_push_scoped(
        bridge,
        input,
        paths,
        RetainedPushScope {
            project,
            sourcemap,
            pushed_from_pull: None,
        },
        release,
        #[cfg(windows)]
        relay,
    );
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn retain_full_push_scoped(
    bridge: &BridgeServer,
    input: Option<&Value>,
    paths: &[PathBuf],
    retained: RetainedPushScope,
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
    let entry = VerifiedFullPush {
        connections: bridge.runtime_connection_signature(&release.runtime_id),
        input: input.clone(),
        project: retained.project,
        paths: paths.to_vec(),
        proof: proof.clone(),
        expires: baseline_expiry(
            #[cfg(windows)]
            relay.as_ref(),
        ),
        sourcemap: retained.sourcemap,
        pushed_from_pull: retained.pushed_from_pull,
        #[cfg(windows)]
        relay,
    };
    keep_baseline(bridge, &release.runtime_id, entry, "full push");
}

#[cfg(any(windows, target_os = "macos"))]
const SOURCEMAP: &str = "sourcemap.json";

/// What the project's sourcemap path holds, as a publish records it.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) type SourcemapState = BTreeMap<PathBuf, PublishEntryState>;

/// The sourcemap a baseline keeps after `published`: as the publish left
/// it, or `held` where it did not write one.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn settled_sourcemap(
    published: &crate::snapshot::export::PublishedProjectChanges,
    held: Option<Box<SourcemapState>>,
) -> Option<Box<SourcemapState>> {
    let sourcemap = Path::new(SOURCEMAP);
    if !published.publish_paths.iter().any(|path| path == sourcemap) {
        return held;
    }
    Some(Box::new(
        published
            .settled
            .iter()
            .filter(|(path, _)| *path == sourcemap)
            .map(|(path, state)| (path.clone(), state.clone()))
            .collect(),
    ))
}

/// The sourcemap a baseline keeps after a push of chosen paths left its
/// files as `project`. A pull maps stores and file names; a push that
/// changed only script sources changes neither. A pull writes a script as
/// Studio holds it, which its `summary` must say is the file byte for byte:
/// Studio's copy passes for the file with other line breaks or without the
/// file's final one.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn sourcemap_after_push(
    held: &VerifiedFullPush,
    project: &ProjectSnapshot,
    summary: &Map<String, Value>,
) -> Option<Box<SourcemapState>> {
    let sources_only = held.project.entries.len() == project.entries.len()
        && held.project.entries.iter().all(|(path, entry)| {
            project
                .entries
                .get(path)
                .is_some_and(|now| now == entry || super::merge::is_source_path(path))
        });
    let exact =
        summary.get("sourceInexact").and_then(Value::as_u64) == Some(0) || held.project == *project;
    if held.sourcemap.is_some() && !(sources_only && exact) {
        log_global(
            5,
            format_args!(
                "[renium] baseline: a pull on it captures every service: {}",
                if sources_only {
                    "Studio may hold a script the push wrote in other bytes than its file"
                } else {
                    "the push changed more than script sources"
                }
            ),
        );
    }
    held.sourcemap.clone().filter(|_| sources_only && exact)
}

/// Whether the project's sourcemap is as `held` says the last pull on it
/// left it.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn sourcemap_as_left(root: &Path, held: &VerifiedFullPush) -> Result<bool> {
    let Some(left) = held.sourcemap.as_ref() else {
        return Ok(false);
    };
    sourcemap_state_as_left(root, left)
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn sourcemap_state_as_left(root: &Path, left: &SourcemapState) -> Result<bool> {
    Ok(
        crate::snapshot::export::collect_publish_hashes(root, &[PathBuf::from(SOURCEMAP)])?
            == *left,
    )
}

/// A pull's use of the held baseline.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) enum PullBaseline<'a> {
    /// Studio and the files still hold the baseline: the pull writes nothing.
    Unchanged,
    /// Studio moved since the baseline only in services the pull can capture
    /// alone; it falls back to a whole pull where that proves false.
    Scoped(Box<ScopedPull>),
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
pub(crate) fn pull_is_whole(args: &crate::cli::PullArgs) -> Result<bool> {
    let mut services = crate::snapshot::import::parse_services(&args.services)?;
    services.sort();
    let mut synced = sync_services();
    synced.sort();
    if services != synced
        || !args.preserve_paths.is_empty()
        || args.export_all_properties
        || args.keep_local_only_scripts
        || args.layout_src_root.is_some()
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
    // Only files a pull would write, still as they were, answer a pull.
    let (files_unchanged, has_unsettled_push) = match held.as_ref() {
        Some(entry) if entry.sourcemap.is_some() => {
            if sourcemap_as_left(&args.project_root, entry)? {
                (
                    snapshot_matches_disk(&args.project_root, &entry.paths, &entry.project)?,
                    false,
                )
            } else {
                log_global(
                    5,
                    format_args!(
                        "[renium] baseline: pull whole: sourcemap.json is not as the last pull left it"
                    ),
                );
                (false, false)
            }
        }
        Some(entry) if entry.pushed_from_pull.is_some() => {
            let sm_ok = entry.pushed_from_pull.as_ref().is_some_and(|p| {
                sourcemap_state_as_left(&args.project_root, &p.sourcemap).unwrap_or(false)
            });
            if sm_ok {
                (
                    snapshot_matches_disk(&args.project_root, &entry.paths, &entry.project)?,
                    true,
                )
            } else {
                log_global(
                    5,
                    format_args!(
                        "[renium] baseline: pull whole: sourcemap.json is not as the last pull left it"
                    ),
                );
                (false, false)
            }
        }
        _ => (false, false),
    };
    if files_unchanged {
        let unchanged = match held.as_ref() {
            Some(entry) => verified_full_push_proof_matches(context, bridge, &entry.proof)?,
            None => false,
        };
        if unchanged
            && !has_unsettled_push
            && let Some(mut entry) = held.take()
        {
            // Studio still holds the proof it held when it and these files
            // were one, and the files are still those files: a capture would
            // find nothing to write.
            entry.renew();
            log_global(5, format_args!("[renium] baseline: pull unchanged"));
            bridge
                .verified_full_pushes
                .lock_recover()
                .insert(runtime.to_string(), entry);
            return Ok(Some(PullBaseline::Unchanged));
        }
        if unchanged && has_unsettled_push {
            match plan_scoped_pull_after_push(args, &mut held) {
                Ok(Some(scoped)) => return Ok(Some(PullBaseline::Scoped(Box::new(scoped)))),
                Ok(None) => {}
                Err(error) => log_global(
                    5,
                    format_args!("[renium] baseline: pull not scoped: {error:#}"),
                ),
            }
        } else {
            match plan_scoped_pull(context, bridge, args, &mut held) {
                Ok(Some(scoped)) => return Ok(Some(PullBaseline::Scoped(Box::new(scoped)))),
                Ok(None) => {}
                Err(error) => log_global(
                    5,
                    format_args!("[renium] baseline: pull not scoped: {error:#}"),
                ),
            }
        }
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
        let project = if let Some(cached) = published.take_after_snapshot() {
            cached
        } else {
            capture_snapshot(&self.root, &published.publish_paths)?
        };
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
            sourcemap: settled_sourcemap(published, None),
            pushed_from_pull: None,
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

#[cfg(any(windows, target_os = "macos"))]
/// What Studio holds against a held proof, as the plugin answers: the
/// services it moved in, each with the instances touched since the proof,
/// or why it cannot say.
#[derive(Debug, PartialEq)]
pub(crate) enum ProofScope {
    Unknown(String),
    Services {
        services: BTreeSet<String>,
        touched: BTreeMap<String, Option<Vec<TouchedInstance>>>,
        values_only: BTreeSet<String>,
    },
}

#[cfg(any(windows, target_os = "macos"))]
/// An instance touched since a proof, by where it stands now: its `values`,
/// its `subtree`, or the set of its `children`. The daemon logs these.
#[derive(Debug, PartialEq)]
pub(crate) struct TouchedInstance {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<u64>,
    pub(crate) kind: String,
    /// What changed of a `values` instance, where Studio named it.
    pub(crate) fields: Option<TouchedFields>,
}

#[cfg(any(windows, target_os = "macos"))]
/// What changed of an instance whose values alone changed: its properties by
/// the names Studio announced them under, its attributes, its tags; with the
/// instance's debug id.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TouchedFields {
    pub(crate) debug_id: String,
    pub(crate) properties: Vec<String>,
    pub(crate) attributes: bool,
    pub(crate) tags: bool,
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct ProofBulkScope {
    pub(crate) services: BTreeSet<String>,
    pub(crate) values_only: BTreeSet<String>,
    pub(crate) changed_class_properties: BTreeMap<String, Vec<(String, String)>>,
    pub(crate) expected_terrain_digest: Option<String>,
    pub(crate) terrain_differs: bool,
    pub(crate) terrain_only_workspace: bool,
}

#[cfg(any(windows, target_os = "macos"))]
impl ProofBulkScope {
    pub(crate) fn parse(answer: &Value) -> Option<Self> {
        if answer.get("unknown").and_then(Value::as_str).is_some() {
            return None;
        }
        let services = answer["services"]
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<BTreeSet<_>>>()?;
        let values_only = answer["valuesOnly"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, flag)| flag.as_bool() == Some(true))
            .map(|(service, _)| service.clone())
            .collect();
        let mut changed_class_properties = BTreeMap::new();
        if let Some(map) = answer["changedClassProps"].as_object() {
            for (service, class_map) in map {
                let Some(obj) = class_map.as_object() else {
                    continue;
                };
                let mut pairs = Vec::new();
                for (class_name, list) in obj {
                    let Some(arr) = list.as_array() else {
                        continue;
                    };
                    for value in arr {
                        if let Some(prop) = value.as_str() {
                            pairs.push((class_name.clone(), prop.to_string()));
                        }
                    }
                }
                if !pairs.is_empty() {
                    changed_class_properties.insert(service.clone(), pairs);
                }
            }
        }
        let expected_terrain_digest = answer["expectedTerrainDigest"]
            .as_str()
            .filter(|digest| !digest.is_empty())
            .map(str::to_string);
        let terrain_differs = answer["terrainDiffers"].as_bool().unwrap_or(false);
        let terrain_only_workspace = answer["terrainOnlyWorkspace"].as_bool().unwrap_or(false);
        Some(Self {
            services,
            values_only,
            changed_class_properties,
            expected_terrain_digest,
            terrain_differs,
            terrain_only_workspace,
        })
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl ProofScope {
    pub(crate) fn parse(answer: &Value) -> Self {
        if let Some(reason) = answer["unknown"].as_str() {
            return Self::Unknown(reason.to_string());
        }
        let Some(services) = answer["services"].as_array().and_then(|services| {
            services
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()
        }) else {
            return Self::Unknown("Studio gave no scoped answer".to_string());
        };
        let touched = answer["touched"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(service, entries)| (service.clone(), touched_instances(entries)))
            .collect();
        let values_only = answer["valuesOnly"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, flag)| flag.as_bool() == Some(true))
            .map(|(service, _)| service.clone())
            .collect();
        Self::Services {
            services: services.into_iter().map(str::to_string).collect(),
            touched,
            values_only,
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
/// A service's touched instances, or `None` for the whole service.
fn touched_instances(entries: &Value) -> Option<Vec<TouchedInstance>> {
    entries
        .as_array()?
        .iter()
        .map(|entry| {
            Some(TouchedInstance {
                path_segments: serde_json::from_value(entry["pathSegments"].clone()).ok()?,
                path_ordinals: serde_json::from_value(entry["pathOrdinals"].clone()).ok()?,
                kind: entry["kind"].as_str()?.to_string(),
                fields: touched_fields(entry),
            })
        })
        .collect()
}

#[cfg(any(windows, target_os = "macos"))]
fn touched_fields(entry: &Value) -> Option<TouchedFields> {
    let properties = match &entry["properties"] {
        // Studio writes an empty list as an empty table.
        Value::Object(names) if names.is_empty() => Vec::new(),
        names => serde_json::from_value(names.clone()).ok()?,
    };
    Some(TouchedFields {
        debug_id: entry["debugId"].as_str()?.to_string(),
        properties,
        attributes: entry["attributes"].as_bool()?,
        tags: entry["tags"].as_bool()?,
    })
}

#[cfg(any(windows, target_os = "macos"))]
fn log_touched(touched: &BTreeMap<String, Option<Vec<TouchedInstance>>>) {
    for (service, instances) in touched {
        let Some(instances) = instances else {
            log_global(
                5,
                format_args!("[renium] baseline: touched {service}: whole"),
            );
            continue;
        };
        let count = |kind: &str| {
            instances
                .iter()
                .filter(|instance| instance.kind == kind)
                .count()
        };
        log_global(
            5,
            format_args!(
                "[renium] baseline: touched {service}: {} values ({} named), {} subtree, {} children",
                count("values"),
                instances
                    .iter()
                    .filter(|instance| instance.fields.is_some())
                    .count(),
                count("subtree"),
                count("children")
            ),
        );
    }
}

/// The store of each service the project holds, by path from its root.
#[cfg(any(windows, target_os = "macos"))]
fn project_store_paths(root: &Path, src_dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    crate::project::structural::service_store_paths(&root.join(src_dir))?
        .into_iter()
        .map(|(service, path)| Ok((service, path.strip_prefix(root)?.to_path_buf())))
        .collect()
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn structural_changes_from_touched(
    paths_moved: &BTreeSet<String>,
    touched: &BTreeMap<String, Option<Vec<TouchedInstance>>>,
) -> StructuralChanges {
    let mut out = StructuralChanges::new();
    for service in paths_moved {
        let Some(Some(entries)) = touched.get(service) else {
            out.insert(service.clone(), vec![StructuralPathChange::WholeService]);
            continue;
        };
        if entries.is_empty() {
            out.insert(service.clone(), vec![StructuralPathChange::WholeService]);
            continue;
        }
        let mut service_changes = Vec::new();
        let mut whole = false;
        for entry in entries {
            if entry.path_segments.first().map(String::as_str) != Some(service.as_str()) {
                whole = true;
                break;
            }
            match entry.kind.as_str() {
                "values" => {
                    if let Some(pos) = entry
                        .path_segments
                        .iter()
                        .position(|segment| segment == "__Renium_SessionLock")
                    {
                        if pos == 0 {
                            whole = true;
                            break;
                        }
                        service_changes.push(StructuralPathChange::Children {
                            parent: entry.path_segments[..pos].to_vec(),
                            child_names: Some(BTreeSet::from(["__Renium_SessionLock".to_string()])),
                        });
                    }
                }
                "subtree" => {
                    if let Some((last, parent)) = entry.path_segments.split_last()
                        && !parent.is_empty()
                    {
                        service_changes.push(StructuralPathChange::Children {
                            parent: parent.to_vec(),
                            child_names: Some(BTreeSet::from([last.clone()])),
                        });
                    } else {
                        whole = true;
                        break;
                    }
                }
                "children" => {
                    service_changes.push(StructuralPathChange::Children {
                        parent: entry.path_segments.clone(),
                        child_names: None,
                    });
                }
                _ => {
                    whole = true;
                    break;
                }
            }
        }
        if whole || service_changes.is_empty() {
            out.insert(service.clone(), vec![StructuralPathChange::WholeService]);
        } else {
            out.insert(service.clone(), service_changes);
        }
    }
    out
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn merge_structural_changes(dst: &mut StructuralChanges, src: StructuralChanges) {
    for (service, changes) in src {
        let entry = dst.entry(service).or_default();
        if entry.contains(&StructuralPathChange::WholeService) {
            continue;
        }
        if changes.contains(&StructuralPathChange::WholeService) {
            *entry = vec![StructuralPathChange::WholeService];
        } else {
            entry.extend(changes);
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn referring_services_for_changes(
    project: &ProjectSnapshot,
    stores: &BTreeMap<String, PathBuf>,
    changes: &StructuralChanges,
) -> Result<BTreeSet<String>> {
    if changes.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut needles = Vec::new();
    for (service, service_changes) in changes {
        for change in service_changes {
            change.append_needles(service, &mut needles);
        }
    }
    if needles.is_empty() {
        return Ok(BTreeSet::new());
    }
    let referring = stores
        .par_iter()
        .filter(|(service, _)| !changes.contains_key(*service))
        .map(|(service, path)| {
            let Some(SnapshotEntry::File(bytes)) = project.entries.get(path) else {
                return Ok(Some(service.clone()));
            };
            let refers =
                SettingsBytecode::has_reference_path_matching(bytes, &needles, |segments| {
                    let Some(target_service) = segments.first() else {
                        return false;
                    };
                    changes.get(target_service).is_some_and(|service_changes| {
                        service_changes
                            .iter()
                            .any(|change| change.matches_ref_path(target_service, segments))
                    })
                })?;
            Ok(refers.then(|| service.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(referring.into_iter().flatten().collect())
}

#[cfg(any(windows, target_os = "macos"))]
fn build_structure_children(doc: &SettingsBytecode) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut children = vec![Vec::new(); doc.instances.len()];
    let mut roots = Vec::new();
    for (idx, inst) in doc.instances.iter().enumerate() {
        if let Some(parent) = inst.parent_index.filter(|&p| p < doc.instances.len()) {
            children[parent].push(idx);
        } else {
            roots.push(idx);
        }
    }
    (roots, children)
}

#[cfg(any(windows, target_os = "macos"))]
type UniqueIdSlice<'a> = Option<&'a [Option<Box<str>>]>;

#[cfg(any(windows, target_os = "macos"))]
fn diff_structure_children<'a>(
    before: &'a SettingsBytecode,
    after: &'a SettingsBytecode,
    (b_children, a_children): (&[Vec<usize>], &[Vec<usize>]),
    (b_uids, a_uids): (UniqueIdSlice<'_>, UniqueIdSlice<'_>),
    (b_node, a_node): (usize, usize),
    path: &mut Vec<&'a str>,
    out: &mut Vec<StructuralPathChange>,
) -> bool {
    let bc = &b_children[b_node];
    let ac = &a_children[a_node];
    let same_node = |bi: usize, ai: usize| {
        let b = &before.instances[bi];
        let a = &after.instances[ai];
        b.name == a.name
            && b.class_name == a.class_name
            && b.settings_id == a.settings_id
            && match (b_uids, a_uids) {
                (Some(bu), Some(au)) => bu.get(bi) == au.get(ai),
                _ => true,
            }
    };
    if bc.len() == ac.len() && bc.iter().zip(ac.iter()).all(|(&bi, &ai)| same_node(bi, ai)) {
        for (&bi, &ai) in bc.iter().zip(ac.iter()) {
            path.push(before.instances[bi].name.as_str());
            let ok = diff_structure_children(
                before,
                after,
                (b_children, a_children),
                (b_uids, a_uids),
                (bi, ai),
                path,
                out,
            );
            path.pop();
            if !ok {
                return false;
            }
        }
        return true;
    }

    let mut b_by_name: ahash::AHashMap<&'a str, Vec<usize>> = ahash::AHashMap::new();
    for &bi in bc {
        b_by_name
            .entry(before.instances[bi].name.as_str())
            .or_default()
            .push(bi);
    }
    let mut a_by_name: ahash::AHashMap<&'a str, Vec<usize>> = ahash::AHashMap::new();
    for &ai in ac {
        a_by_name
            .entry(after.instances[ai].name.as_str())
            .or_default()
            .push(ai);
    }
    let mut changed_names = BTreeSet::new();
    let mut matched_pairs: Vec<(&'a str, usize, usize)> = Vec::new();
    for (&name, b_list) in &b_by_name {
        let Some(a_list) = a_by_name.get(name) else {
            changed_names.insert(name.to_string());
            continue;
        };
        let dup_unsafe = b_list.len() > 1
            && (b_uids.is_none() || a_uids.is_none())
            && b_list
                .iter()
                .any(|&bi| before.instances[bi].settings_id.is_empty());
        if b_list.len() != a_list.len()
            || dup_unsafe
            || !b_list
                .iter()
                .zip(a_list.iter())
                .all(|(&bi, &ai)| same_node(bi, ai))
        {
            changed_names.insert(name.to_string());
        } else {
            for (&bi, &ai) in b_list.iter().zip(a_list.iter()) {
                matched_pairs.push((name, bi, ai));
            }
        }
    }
    for &name in a_by_name.keys() {
        if !b_by_name.contains_key(name) {
            changed_names.insert(name.to_string());
        }
    }
    if !changed_names.is_empty() {
        out.push(StructuralPathChange::Children {
            parent: path.iter().map(|s| (*s).to_string()).collect(),
            child_names: Some(changed_names),
        });
    }
    for (name, bi, ai) in matched_pairs {
        path.push(name);
        let ok = diff_structure_children(
            before,
            after,
            (b_children, a_children),
            (b_uids, a_uids),
            (bi, ai),
            path,
            out,
        );
        path.pop();
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(any(windows, target_os = "macos"))]
fn diff_store_bytes_structure(
    service: &str,
    before_bytes: &[u8],
    after_bytes: &[u8],
) -> Vec<StructuralPathChange> {
    if before_bytes == after_bytes {
        return Vec::new();
    }
    let (Ok(mut before_doc), Ok(mut after_doc)) = rayon::join(
        || SettingsBytecode::decode_structure(before_bytes),
        || SettingsBytecode::decode_structure(after_bytes),
    ) else {
        return vec![StructuralPathChange::WholeService];
    };
    let (b_uids, a_uids) = rayon::join(
        || {
            crate::settings::bytecode::blocks::BlockStore::open(before_bytes)
                .ok()
                .and_then(|s| {
                    s.extract_unique_ids_and_root_camera(&mut before_doc.instances)
                        .ok()
                })
        },
        || {
            crate::settings::bytecode::blocks::BlockStore::open(after_bytes)
                .ok()
                .and_then(|s| {
                    s.extract_unique_ids_and_root_camera(&mut after_doc.instances)
                        .ok()
                })
        },
    );
    let (b_roots, b_children) = build_structure_children(&before_doc);
    let (a_roots, a_children) = build_structure_children(&after_doc);
    if b_roots.len() != a_roots.len() {
        return vec![StructuralPathChange::WholeService];
    }
    let mut changes = Vec::new();
    for (&br, &ar) in b_roots.iter().zip(a_roots.iter()) {
        let b = &before_doc.instances[br];
        let a = &after_doc.instances[ar];
        if b.name != a.name || b.class_name != a.class_name || b.settings_id != a.settings_id {
            return vec![StructuralPathChange::WholeService];
        }
        let mut path = vec![service];
        if !diff_structure_children(
            &before_doc,
            &after_doc,
            (&b_children, &a_children),
            (b_uids.as_deref(), a_uids.as_deref()),
            (br, ar),
            &mut path,
            &mut changes,
        ) {
            return vec![StructuralPathChange::WholeService];
        }
    }
    changes
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn structural_changes_between_snapshots(
    context: &BoundContext,
    services: &BTreeSet<String>,
    before: &ProjectSnapshot,
    after: &ProjectSnapshot,
    generated: Option<&ProjectSnapshot>,
) -> StructuralChanges {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source);
    let Ok(rel_source) = source.strip_prefix(root) else {
        return services
            .iter()
            .map(|s| (s.clone(), vec![StructuralPathChange::WholeService]))
            .collect();
    };
    let after_entry = |path: &PathBuf| {
        generated
            .and_then(|g| g.entries.get(path))
            .or_else(|| after.entries.get(path))
    };
    let mut out = StructuralChanges::new();
    for service in services {
        let mut service_changes = Vec::new();
        let mut whole = false;
        let folder = source.join(service);
        let Ok(store_rel) = service_settings_path(&folder)
            .strip_prefix(root)
            .map(Path::to_path_buf)
        else {
            out.insert(service.clone(), vec![StructuralPathChange::WholeService]);
            continue;
        };
        let b_store = before.entries.get(&store_rel);
        let a_store = after_entry(&store_rel);
        if b_store != a_store {
            match (b_store, a_store) {
                (Some(SnapshotEntry::File(b_bytes)), Some(SnapshotEntry::File(a_bytes))) => {
                    let store_diff = diff_store_bytes_structure(service, b_bytes, a_bytes);
                    if store_diff.contains(&StructuralPathChange::WholeService) {
                        whole = true;
                    } else {
                        service_changes.extend(store_diff);
                    }
                }
                _ => whole = true,
            }
        }
        if !whole {
            let service_prefix = rel_source.join(service);
            let mut extra_paths = before
                .entries
                .keys()
                .chain(after.entries.keys())
                .chain(generated.into_iter().flat_map(|g| g.entries.keys()))
                .filter(|path| path.starts_with(&service_prefix) && **path != store_rel)
                .collect::<Vec<_>>();
            extra_paths.sort();
            extra_paths.dedup();
            for path in extra_paths {
                let b_has = before.entries.contains_key(path);
                let a_has = after_entry(path).is_some();
                if b_has == a_has {
                    continue;
                }
                let Ok(rel_in_service) = path.strip_prefix(&service_prefix) else {
                    whole = true;
                    break;
                };
                let comps = rel_in_service
                    .components()
                    .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
                    .collect::<Vec<_>>();
                if comps.is_empty() {
                    continue;
                }
                let is_init = comps.last().is_some_and(|name| name.starts_with("init."));
                let mut parent = vec![service.clone()];
                let drop_tail = if is_init && comps.len() >= 2 { 2 } else { 1 };
                parent.extend(comps[..comps.len() - drop_tail].iter().cloned());
                service_changes.push(StructuralPathChange::Children {
                    parent,
                    child_names: None,
                });
            }
        }
        if whole {
            out.insert(service.clone(), vec![StructuralPathChange::WholeService]);
        } else if !service_changes.is_empty() {
            out.insert(service.clone(), service_changes);
        }
    }
    out
}

#[cfg(any(windows, target_os = "macos"))]
/// Puts what a scoped publish left under its paths in place of what the
/// held files hold there.
pub(crate) fn rescope(held: &mut ProjectSnapshot, written: ProjectSnapshot, paths: &[PathBuf]) {
    let scope = ScopeSet::new(paths);
    held.entries.retain(|path, _| !scope.contains(path));
    held.entries.extend(written.entries);
}

#[cfg(any(windows, target_os = "macos"))]
/// What Studio did since a scoped pull's proof, as its publication sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopedPremise {
    /// Studio still holds the proof.
    Held,
    /// Studio moved only where the pull captures, references to it
    /// included: the pull publishes and keeps no baseline.
    Inside,
    /// Studio moved elsewhere, or cannot say where: the pull publishes
    /// nothing and a whole pull runs instead.
    False,
}

#[cfg(any(windows, target_os = "macos"))]
/// The services among `services` where Studio may have moved what another
/// store refers to. A store writes a reference by its target's path, sibling
/// ordinals, export and identity; values alone (properties but a name, a
/// parent or Archivable, attributes, tags, script buffers, voxels) move none,
/// but for the attribute that makes a folder named as the session lock one,
/// which leaves it out of the export.
#[cfg(all(test, any(windows, target_os = "macos")))]
pub(crate) fn paths_moved_in(
    services: &BTreeSet<String>,
    touched: &BTreeMap<String, Option<Vec<TouchedInstance>>>,
) -> BTreeSet<String> {
    paths_moved_in_with(services, touched, &BTreeSet::new())
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn paths_moved_in_with(
    services: &BTreeSet<String>,
    touched: &BTreeMap<String, Option<Vec<TouchedInstance>>>,
    values_only: &BTreeSet<String>,
) -> BTreeSet<String> {
    services
        .iter()
        .filter(|service| {
            !values_only.contains(*service)
                && !touched
                    .get(*service)
                    .and_then(Option::as_ref)
                    .is_some_and(|instances| {
                        !instances.is_empty()
                            && instances.iter().all(|instance| {
                                instance.kind == "values"
                                    && !instance
                                        .path_segments
                                        .iter()
                                        .any(|segment| segment == "__Renium_SessionLock")
                            })
                    })
        })
        .cloned()
        .collect()
}

#[cfg(any(windows, target_os = "macos"))]
/// The premise of a pull scoped to `scope` once Studio moved as `now`
/// says since its proof.
pub(crate) fn scoped_premise(
    now: &ProofScope,
    scope: &BTreeSet<String>,
    project: &ProjectSnapshot,
    stores: &BTreeMap<String, PathBuf>,
) -> Result<ScopedPremise> {
    let ProofScope::Services {
        services,
        touched,
        values_only,
    } = now
    else {
        return Ok(ScopedPremise::False);
    };
    Ok(
        if services.is_subset(scope)
            && referring_services_for_changes(
                project,
                stores,
                &structural_changes_from_touched(
                    &paths_moved_in_with(services, touched, values_only),
                    touched,
                ),
            )?
            .is_subset(scope)
        {
            ScopedPremise::Inside
        } else {
            ScopedPremise::False
        },
    )
}

#[cfg(any(windows, target_os = "macos"))]
/// The baseline a scoped pull keeps, from the files under its publish paths
/// as read back: the held files with those in their place. The capture came
/// after the pull's proof and Studio held that proof until the files were
/// written, so the scoped services' files hold what Studio holds at it; every
/// other service's files hold what Studio held at the old proof, which
/// differs from this one only in the scoped services.
pub(crate) fn scoped_baseline(
    mut held: ProjectSnapshot,
    written: ProjectSnapshot,
    (paths, settled): (&[PathBuf], &BTreeMap<PathBuf, PublishEntryState>),
    premise: ScopedPremise,
) -> std::result::Result<ProjectSnapshot, &'static str> {
    if premise != ScopedPremise::Held {
        return Err("Studio changed during the pull");
    }
    if !publish_settled(&written, settled) {
        return Err("the files changed after the pull wrote them");
    }
    rescope(&mut held, written, paths);
    Ok(held)
}

/// Captures and publishes the services `args` names, running `check` before
/// the publish.
#[cfg(any(windows, target_os = "macos"))]
fn export_scoped(
    args: &crate::cli::PullArgs,
    bridge: &BridgeServer,
    info: &crate::studio::bridge::BridgeInfoPayload,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<crate::snapshot::export::PublishedProjectChanges> {
    #[cfg(test)]
    if let Some(exported) = super::push_flow_tests::export(check) {
        return exported;
    }
    crate::snapshot::export::export_pull_checked(args, bridge, info, check)
}

/// A pull of the services Studio moved in since the held baseline, with
/// those whose stores refer into them.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) struct ScopedPull {
    held: VerifiedFullPush,
    /// The proof Studio took under the held one's observation before the
    /// capture, which the new baseline stands on.
    proof: Value,
    services: BTreeSet<String>,
    stores: BTreeMap<String, PathBuf>,
    /// What Studio touched in each service between the two proofs.
    #[cfg_attr(not(windows), allow(dead_code))]
    touched: BTreeMap<String, Option<Vec<TouchedInstance>>>,
}

#[cfg(any(windows, target_os = "macos"))]
fn plan_scoped_pull_after_push(
    args: &crate::cli::PullArgs,
    held: &mut Option<VerifiedFullPush>,
) -> Result<Option<ScopedPull>> {
    let Some(entry) = held.as_ref() else {
        return Ok(None);
    };
    let Some(pushed) = entry.pushed_from_pull.as_ref() else {
        return Ok(None);
    };
    let stores = project_store_paths(&args.project_root, &args.src_dir)?;
    let referring = referring_services_for_changes(&entry.project, &stores, &pushed.structural)?;
    let services = pushed
        .services
        .union(&referring)
        .cloned()
        .collect::<BTreeSet<_>>();
    if services.len() >= sync_services().len() {
        log_global(
            5,
            format_args!("[renium] baseline: pull not scoped: it covers every service"),
        );
        return Ok(None);
    }
    let list =
        |services: &BTreeSet<String>| services.iter().cloned().collect::<Vec<_>>().join(", ");
    log_global(
        5,
        format_args!(
            "[renium] baseline: pull scoped to [{}]: pushed [{}], stores referring to the rest [{}]",
            list(&services),
            list(&pushed.services),
            list(&referring)
        ),
    );
    Ok(held.take().map(|held| {
        let proof = held.proof.clone();
        ScopedPull {
            held,
            proof,
            services,
            stores,
            touched: BTreeMap::new(),
        }
    }))
}

/// Has Studio take a new proof under the held proof's observation and, where
/// it can say which services moved between the two, plans a pull of those.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn plan_scoped_pull(
    context: &BoundContext,
    bridge: &BridgeServer,
    args: &crate::cli::PullArgs,
    held: &mut Option<VerifiedFullPush>,
) -> Result<Option<ScopedPull>> {
    let (Some(entry), Some(runtime)) = (held.as_ref(), context.runtime_id.as_deref()) else {
        return Ok(None);
    };
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({"start": false, "capturePushProof": true, "scopeFrom": entry.proof}),
        BridgeTarget::Edit,
        runtime,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    let not_scoped = |reason: &str| {
        log_global(
            5,
            format_args!("[renium] baseline: pull not scoped: {reason}"),
        );
    };
    let (mut moved, mut paths_moved, mut touched) =
        match ProofScope::parse(&state["pushProofScope"]) {
            ProofScope::Unknown(reason) => {
                not_scoped(&reason);
                return Ok(None);
            }
            ProofScope::Services {
                services,
                touched,
                values_only,
            } => {
                log_touched(&touched);
                let paths_moved = paths_moved_in_with(&services, &touched, &values_only);
                (services, paths_moved, touched)
            }
        };
    let mut structural_changes = structural_changes_from_touched(&paths_moved, &touched);
    if let Some(pushed) = entry.pushed_from_pull.as_ref() {
        moved.extend(pushed.services.iter().cloned());
        paths_moved.extend(pushed.paths_moved.iter().cloned());
        merge_structural_changes(&mut structural_changes, pushed.structural.clone());
        for service in &pushed.services {
            touched.insert(service.clone(), None);
        }
    }
    let Some(proof) = state
        .get("verifiedPushProof")
        .filter(|value| !value.is_null())
        .cloned()
    else {
        not_scoped("Studio gave no new proof");
        return Ok(None);
    };
    let stores = project_store_paths(&args.project_root, &args.src_dir)?;
    let referring = referring_services_for_changes(&entry.project, &stores, &structural_changes)?;
    let services = moved.union(&referring).cloned().collect::<BTreeSet<_>>();
    if services.len() >= sync_services().len() {
        not_scoped("it covers every service");
        return Ok(None);
    }
    let list =
        |services: &BTreeSet<String>| services.iter().cloned().collect::<Vec<_>>().join(", ");
    log_global(
        5,
        format_args!(
            "[renium] baseline: pull scoped to [{}]: Studio moved [{}], values only in [{}], stores referring to the rest [{}]",
            list(&services),
            list(&moved),
            list(&moved.difference(&paths_moved).cloned().collect()),
            list(&referring)
        ),
    );
    Ok(held.take().map(|held| ScopedPull {
        held,
        proof,
        services,
        stores,
        touched,
    }))
}

#[cfg(any(windows, target_os = "macos"))]
impl ScopedPull {
    /// Whether the held files hold a PackageLink in the store of a service
    /// the pull writes.
    pub(crate) fn holds_package_link(&self) -> Result<bool> {
        for service in &self.services {
            let store =
                (self.stores.get(service)).and_then(|store| self.held.project.entries.get(store));
            if super::merge::holds_package_link(store)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The baseline the pull was planned over, for a pull that does not run.
    pub(crate) fn into_held(self) -> VerifiedFullPush {
        self.held
    }

    /// Captures and publishes the scoped services, checking before the
    /// publish that Studio moved nowhere else since the proof; `None`, having
    /// written nothing, where it did. `files_held` checks the files under
    /// the services' folders and stores before a publish of services too; an
    /// error from it publishes nothing.
    pub(crate) fn run(
        self,
        context: &BoundContext,
        bridge: &BridgeServer,
        info: &crate::studio::bridge::BridgeInfoPayload,
        args: &mut crate::cli::PullArgs,
        files_held: &mut dyn FnMut(&[PathBuf]) -> Result<()>,
    ) -> Result<Option<crate::snapshot::export::PublishedProjectChanges>> {
        let runtime = context
            .runtime_id
            .as_deref()
            .context("Studio context has no edit-mode runtime")?;
        let root = Path::new(&context.root);
        let pulled = (self.services.iter())
            .flat_map(|service| {
                let folder = Path::new(&context.source).join(service);
                [service_settings_path(&folder), folder]
            })
            .map(|path| Ok(path.strip_prefix(root)?.to_path_buf()))
            .collect::<Result<Vec<_>>>()?;
        #[cfg(windows)]
        if let Some((published, premise)) = self.pull_instances(bridge, runtime, args)? {
            if let Err(error) = self.keep(bridge, runtime, &args.project_root, &published, premise)
            {
                log_global(
                    5,
                    format_args!("[renium] baseline: not kept after instance pull: {error:#}"),
                );
            }
            return Ok(Some(published));
        }
        let mut premise = ScopedPremise::False;
        let mut check = || -> Result<()> {
            let state = bridge.call_for_runtime_with_timeout(
                "getStudioChangeState",
                json!({"start": false, "verifyPushProof": self.proof, "scopePushProof": true}),
                BridgeTarget::Edit,
                runtime,
                Some(Duration::from_secs(10)),
            )?;
            ensure_plugin_api_ok(&state)?;
            premise = if state["pushProofMatches"] == true {
                ScopedPremise::Held
            } else {
                scoped_premise(
                    &ProofScope::parse(&state["pushProofScope"]),
                    &self.services,
                    &self.held.project,
                    &self.stores,
                )?
            };
            if premise == ScopedPremise::False {
                bail!("Studio moved outside the scoped services during the pull");
            }
            files_held(&pulled)
        };
        let result = if self.services.is_empty() {
            check().and_then(|()| {
                crate::snapshot::export::record_bridge_sync_completion(bridge)?;
                Ok(crate::snapshot::export::PublishedProjectChanges::default())
            })
        } else {
            let whole = std::mem::replace(
                &mut args.services,
                self.services.iter().cloned().collect::<Vec<_>>().join(","),
            );
            let result = export_scoped(args, bridge, info, &mut check);
            args.services = whole;
            result
        };
        let published = match result {
            Err(error) if premise == ScopedPremise::False => {
                log_global(
                    5,
                    format_args!(
                        "[renium] baseline: scoped pull wrote nothing; a whole pull follows: {error:#}"
                    ),
                );
                return Ok(None);
            }
            result => result?,
        };
        if let Err(error) = self.keep(bridge, runtime, &args.project_root, &published, premise) {
            log_global(
                5,
                format_args!("[renium] baseline: not kept after scoped pull: {error:#}"),
            );
        }
        Ok(Some(published))
    }

    /// Keeps the held files with the scoped services' files as the publish
    /// left them as the baseline, on the proof taken before the capture,
    /// when Studio still holds that proof.
    fn keep(
        mut self,
        bridge: &BridgeServer,
        runtime: &str,
        root: &Path,
        published: &crate::snapshot::export::PublishedProjectChanges,
        premise: ScopedPremise,
    ) -> Result<()> {
        let not_kept = |reason: &str| {
            log_global(
                5,
                format_args!("[renium] baseline: not kept after scoped pull: {reason}"),
            );
        };
        if !published.preserved.is_empty()
            || !published.read_only_links_restored.is_empty()
            || !published.studio_overrides.is_empty()
        {
            not_kept("the pull kept files back");
            return Ok(());
        }
        let held_paths = ScopeSet::new(&self.held.paths);
        if published
            .publish_paths
            .iter()
            .any(|path| !derived_project_path(path) && !held_paths.contains(path))
        {
            not_kept("the pull wrote outside the baseline's paths");
            return Ok(());
        }
        let expires = baseline_expiry(
            #[cfg(windows)]
            self.held.relay.as_ref(),
        );
        let retain = expires
            .saturating_duration_since(Instant::now())
            .min(BASELINE_IDLE);
        let (written, state) = rayon::join(
            || capture_snapshot(root, &published.publish_paths),
            || {
                bridge.call_for_runtime_with_timeout(
                    "getStudioChangeState",
                    json!({"start": false, "retainPushProof": self.proof, "retainSeconds": retain.as_secs()}),
                    BridgeTarget::Edit,
                    runtime,
                    Some(Duration::from_secs(10)),
                )
            },
        );
        let state = state?;
        ensure_plugin_api_ok(&state)?;
        if state.get("retainedPushProof") != Some(&self.proof) {
            not_kept("Studio changed during the pull");
            return Ok(());
        }
        #[cfg(windows)]
        if self.held.relay.is_none() && state["retainedPushProofLocal"] != true {
            not_kept("no native attribute guard");
            return Ok(());
        }
        let written = written?;
        let project = match scoped_baseline(
            std::mem::take(&mut self.held.project),
            written,
            (&published.publish_paths, &published.settled),
            premise,
        ) {
            Ok(project) => project,
            Err(reason) => {
                not_kept(reason);
                return Ok(());
            }
        };
        let prior_sourcemap = self
            .held
            .sourcemap
            .or_else(|| self.held.pushed_from_pull.map(|p| Box::new(p.sourcemap)));
        let entry = VerifiedFullPush {
            connections: bridge.runtime_connection_signature(runtime),
            input: self.held.input,
            project,
            paths: self.held.paths,
            proof: self.proof,
            expires,
            sourcemap: settled_sourcemap(published, prior_sourcemap),
            pushed_from_pull: None,
            #[cfg(windows)]
            relay: self.held.relay,
        };
        keep_baseline(bridge, runtime, entry, "scoped pull");
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos"))]
#[path = "instance_pull.rs"]
mod instance_pull;
#[cfg(all(test, any(windows, target_os = "macos")))]
pub(crate) use instance_pull::*;

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
    pub(crate) bulk_property_patches: Vec<crate::editor::types::EditorBulkPropertyPatch>,
    pub(crate) bulk_property_paths: Vec<PathBuf>,
    pub(crate) extra_property_changes: Vec<EditorPropertyChange>,
    pub(crate) capture_before_terrain: bool,
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
            && self.bulk_property_patches.is_empty()
            && self.extra_property_changes.is_empty()
    }
}

#[cfg(all(test, any(windows, target_os = "macos")))]
mod structural_diff_tests {
    use super::*;
    use crate::settings::bytecode::{
        SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode,
    };

    #[test]
    fn structural_changes_between_snapshots_roots_renamed_service_at_canonical_key() {
        let root = crate::tests::support::temp_dir("r9-renamed-service-diff");
        let source = root.join("src");
        let context = BoundContext {
            id: 1,
            initialized: true,
            root: root.display().to_string(),
            project: root.join("renium.project.json").display().to_string(),
            experience: "test".into(),
            source: source.display().to_string(),
            resource_lease: None,
            place_id: None,
            game_id: None,
            selector: String::new(),
            runtime_id: None,
            plugin_build: None,
            fingerprint: String::new(),
        };
        let before_doc = SettingsBytecode {
            version: 12,
            instances: vec![SettingsBytecodeInstance::new(
                "ws-0".into(),
                "RenamedWorld".into(),
                "Workspace".into(),
                None,
            )],
        };
        let mut after_doc = before_doc.clone();
        after_doc.instances.push(SettingsBytecodeInstance::new(
            "part-1".into(),
            "Spawn".into(),
            "Part".into(),
            Some(0),
        ));
        let store_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let before = ProjectSnapshot {
            entries: BTreeMap::from([(
                store_rel.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&before_doc).unwrap()),
            )]),
        };
        let after = ProjectSnapshot {
            entries: BTreeMap::from([(
                store_rel,
                SnapshotEntry::File(encode_settings_bytecode(&after_doc).unwrap()),
            )]),
        };
        let services = BTreeSet::from(["Workspace".to_string()]);
        let diff = structural_changes_between_snapshots(&context, &services, &before, &after, None);
        let ws_changes = diff
            .get("Workspace")
            .expect("Workspace structural change must be recorded");
        let target_segments = vec!["Workspace".to_string(), "Spawn".to_string()];
        assert!(
            ws_changes
                .iter()
                .any(|change| change.matches_ref_path("Workspace", &target_segments)),
            "R9: renamed service diff must root at canonical service key 'Workspace', got {ws_changes:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn plan_scoped_pull_after_push_scopes_to_pushed_and_referring_services() {
        let root = crate::tests::support::temp_dir("r10-scoped-pull-after-push");
        let src = root.join("src");
        fs::create_dir_all(src.join("Workspace")).unwrap();
        fs::create_dir_all(src.join("ReplicatedStorage")).unwrap();
        fs::create_dir_all(src.join("ServerStorage")).unwrap();

        let ws_doc = SettingsBytecode {
            version: 12,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws-0".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new("p-1".into(), "Spawn".into(), "Part".into(), Some(0)),
            ],
        };
        let mut rep_doc = SettingsBytecode {
            version: 12,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "rs-0".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "ov-1".into(),
                    "Link".into(),
                    "ObjectValue".into(),
                    Some(0),
                ),
            ],
        };
        rep_doc.instances[1].properties.insert(
            "Value".into(),
            json!({"_type": "Ref", "pathSegments": ["Workspace", "Spawn"], "pathOrdinals": [1, 1]}),
        );
        let ss_doc = SettingsBytecode {
            version: 12,
            instances: vec![SettingsBytecodeInstance::new(
                "ss-0".into(),
                "ServerStorage".into(),
                "ServerStorage".into(),
                None,
            )],
        };

        let ws_bytes = encode_settings_bytecode(&ws_doc).unwrap();
        let rep_bytes = encode_settings_bytecode(&rep_doc).unwrap();
        let ss_bytes = encode_settings_bytecode(&ss_doc).unwrap();
        let ws_rel = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let rep_rel = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let ss_rel = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
        fs::write(root.join(&ws_rel), &ws_bytes).unwrap();
        fs::write(root.join(&rep_rel), &rep_bytes).unwrap();
        fs::write(root.join(&ss_rel), &ss_bytes).unwrap();
        let entries = BTreeMap::from([
            (ws_rel, SnapshotEntry::File(ws_bytes)),
            (rep_rel, SnapshotEntry::File(rep_bytes)),
            (ss_rel, SnapshotEntry::File(ss_bytes)),
        ]);
        let mut structural = StructuralChanges::new();
        structural.insert(
            "Workspace".into(),
            vec![StructuralPathChange::Children {
                parent: vec!["Workspace".into()],
                child_names: Some(BTreeSet::from(["Spawn".into()])),
            }],
        );
        let entry = VerifiedFullPush {
            connections: Vec::new(),
            input: json!({}),
            project: ProjectSnapshot { entries },
            paths: vec![PathBuf::from("src")],
            proof: json!({"id": "proof-1"}),
            expires: Instant::now() + Duration::from_secs(60),
            sourcemap: None,
            pushed_from_pull: Some(Box::new(PushedFromPull {
                sourcemap: BTreeMap::new(),
                services: BTreeSet::from(["Workspace".into()]),
                paths_moved: BTreeSet::from(["Workspace".into()]),
                structural,
            })),
            #[cfg(windows)]
            relay: None,
        };
        let mut held = Some(entry);
        let args = crate::cli::PullArgs {
            project_root: root.clone(),
            src_dir: PathBuf::from("src"),
            services: String::new(),
            bridge: crate::cli::BridgeConnectionArgs::default_connection(),
            export_all_properties: false,
            quiet_timings: true,
            preserve_paths: Vec::new(),
            keep_local_only_scripts: false,
            layout_src_root: None,
        };
        let scoped = plan_scoped_pull_after_push(&args, &mut held)
            .unwrap()
            .expect("must return scoped pull plan");
        assert!(
            scoped.services.contains("Workspace"),
            "pushed service must be captured"
        );
        assert!(
            scoped.services.contains("ReplicatedStorage"),
            "service referring to moved Workspace.Spawn must be captured"
        );
        assert!(
            !scoped.services.contains("ServerStorage"),
            "unrelated ServerStorage must NOT be captured"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
