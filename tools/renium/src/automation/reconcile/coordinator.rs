use super::*;

#[derive(Default)]
pub(crate) struct TargetOwners {
    pub(crate) by_target: HashMap<String, (String, String)>,
    pub(crate) target_by_pair: HashMap<String, String>,
}

pub(crate) struct PairSetup {
    pub(crate) key: String,
    pub(crate) identity: PairIdentity,
    pub(crate) mode: PairMode,
    pub(crate) resolution_preference: Option<ConflictPreference>,
    pub(crate) resolution_required: bool,
    pub(crate) error: Option<String>,
    pub(crate) conflicts: Vec<String>,
    pub(crate) requires_reconcile: bool,
    pub(crate) runtime_id: Option<String>,
    pub(crate) local_file_stamp: Option<LocalFileStamp>,
    pub(crate) local_file_digest: Option<String>,
    pub(crate) bootstrap_studio_from_editor: bool,
    pub(crate) runtime_replacement_unproven: bool,
    /// Project paths edited while the reconcile ran. Studio has not received
    /// them, so Live Sync keeps them pending instead of absorbing them.
    pub(crate) unsynced_paths: Vec<PathBuf>,
}

#[derive(Default)]
pub(crate) struct Coordinator {
    pub(crate) pairs: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub(crate) owners: Mutex<TargetOwners>,
}

impl Coordinator {
    pub(crate) fn saved_local_file(&self, context: &BoundContext) -> Result<Option<PathBuf>> {
        saved_local_file_for_context(context)
    }

    pub(crate) fn pair_key(&self, context: &BoundContext, bridge: &BridgeServer) -> Result<String> {
        Ok(PairIdentity::from_context(context, bridge)?.pair_key())
    }

    pub(crate) fn target_key(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<String> {
        Ok(PairIdentity::from_context(context, bridge)?.target_key())
    }

    pub(crate) fn target_owner(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<Option<String>> {
        let identity = PairIdentity::from_context(context, bridge)?;
        let pair = identity.pair_key();
        Ok(self
            .owners
            .lock_recover()
            .by_target
            .get(&identity.target_key())
            .filter(|(owner_pair, _)| owner_pair != &pair)
            .map(|(_, owner_project)| owner_project.clone()))
    }

    pub(crate) fn pair_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut pairs = self.pairs.lock_recover();
        Arc::clone(
            pairs
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub(crate) fn prepare(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        configuration: PairConfiguration,
    ) -> Result<PairSetup> {
        let PairConfiguration {
            mode: requested_mode,
            conflict_preference,
            resolution_preference,
            runtime_settings,
        } = configuration;
        let identity = PairIdentity::from_context(context, bridge)?;
        let current_runtime_id = context.runtime_id.clone();
        let current_local_file_stamp = local_file_stamp(identity.local_file.as_deref())?;
        let pair_key = identity.pair_key();
        let pair_lock = self.pair_lock(&pair_key);
        let _pair = pair_lock.lock_recover();
        let mut mode = requested_mode;
        let unresolved_local = identity.game_id.is_none()
            && identity.place_id.is_none()
            && identity.local_file.is_none();
        if unresolved_local {
            mode = PairMode::Verify;
        }
        let owner_conflict = if mode.writes() {
            self.claim_target(&identity)
        } else {
            None
        };
        if owner_conflict.is_some() {
            mode = PairMode::Verify;
        }
        let existing = load_record(context, &pair_key)?.filter(|record| {
            record.version == RECORD_VERSION && record.identity.same_pair(&identity)
        });
        let record_missing = existing.is_none();
        let mut record = existing.unwrap_or(PairRecord {
            version: RECORD_VERSION,
            identity: identity.clone(),
            mode,
            conflict_preference,
            runtime_settings: Map::new(),
            baseline: None,
            head: None,
            conflicts: Vec::new(),
            resolution_required: false,
            last_runtime_id: None,
            local_file_stamp: None,
            local_file_digest: None,
            studio_checkpoint: None,
        });
        let runtime_replaced =
            record.last_runtime_id.is_some() && record.last_runtime_id != current_runtime_id;
        let current_local_file_digest = if runtime_replaced
            || record.local_file_digest.is_none()
            || record.local_file_stamp != current_local_file_stamp
        {
            local_file_digest(identity.local_file.as_deref())?
        } else {
            record.local_file_digest.clone()
        };
        let runtime_replacement_unproven =
            runtime_replaced && identity.local_file.is_some() && record.local_file_digest.is_none();
        let bootstrap_studio_from_editor = should_bootstrap_studio_from_editor(
            mode,
            record.last_runtime_id.as_deref(),
            current_runtime_id.as_deref(),
            record.local_file_digest.as_deref(),
            current_local_file_digest.as_deref(),
        );
        let configuration_changed = record.identity.fingerprint != identity.fingerprint;
        let obsolete_head = record.head.take().is_some();
        let record_changed = record_missing
            || obsolete_head
            || record.identity != identity
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || record.runtime_settings != runtime_settings
            || !runtime_replaced
                && (record.local_file_stamp != current_local_file_stamp
                    || record.local_file_digest != current_local_file_digest);
        let requires_reconcile = record_missing
            || configuration_changed
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || !record.conflicts.is_empty()
            || runtime_replaced;
        if configuration_changed {
            record.baseline = None;
            record.studio_checkpoint = None;
            record.conflicts.clear();
            record.resolution_required = false;
        }
        record.identity = identity.clone();
        record.mode = mode;
        record.conflict_preference = conflict_preference;
        record.runtime_settings = runtime_settings;
        if !runtime_replaced {
            record.local_file_stamp = current_local_file_stamp.clone();
            record.local_file_digest = current_local_file_digest.clone();
        }
        if record_changed {
            write_record(context, &pair_key, &record)?;
        }
        Ok(PairSetup {
            key: pair_key,
            identity,
            mode,
            resolution_preference,
            conflicts: record.conflicts.clone(),
            resolution_required: !unresolved_local
                && owner_conflict.is_none()
                && (record.resolution_required
                    || !record.conflicts.is_empty()
                        && conflict_preference == ConflictPreference::None),
            error: unresolved_local
                .then(|| {
                    "Renium could not prove which local place file is open, so this pair is verify-only"
                        .to_string()
                })
                .or_else(|| {
                    owner_conflict.map(|owner| {
                        format!("This Studio place is already owned by {owner}; this project is verify-only")
                    })
                }),
            requires_reconcile,
            runtime_id: current_runtime_id,
            local_file_stamp: current_local_file_stamp,
            local_file_digest: current_local_file_digest,
            bootstrap_studio_from_editor,
            runtime_replacement_unproven,
            unsynced_paths: Vec::new(),
        })
    }

    pub(crate) fn claim_setup_target(&self, setup: &PairSetup) -> Option<String> {
        self.claim_target(&setup.identity)
    }

    pub(crate) fn reconcile(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        setup: &mut PairSetup,
    ) -> Result<()> {
        let _gate = bridge.acquire_request_gate();
        self.reconcile_with_gate_held(context, bridge, setup)
    }

    // Lock order everywhere: the bridge request gate first, then the Live Sync
    // activity, then the pair lock. A request holding the gate may wait for
    // the sync activity to clear, so a sync must never hold the activity
    // while it waits for the gate.
    pub(crate) fn reconcile_with_gate_held(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        setup: &mut PairSetup,
    ) -> Result<()> {
        let pair_lock = self.pair_lock(&setup.key);
        let _pair = pair_lock.lock_recover();
        let mut record = load_record(context, &setup.key)?
            .context("Reconciliation state disappeared while starting Live Sync")?;
        let _selection = bound_context::select(context);
        let (studio_guard, studio_state) =
            current_studio_change_guard_with_state(context, bridge, |_| Ok(()))?;
        if setup.runtime_replacement_unproven && setup.resolution_preference.is_none() {
            setup.mode = PairMode::Verify;
            setup.error = Some(
                "Renium cannot prove whether the local place file changed before this Studio restart; choose Studio or project files"
                    .to_string(),
            );
            setup.resolution_required = true;
            return Ok(());
        }
        if setup.bootstrap_studio_from_editor && !studio_guard.runtime_bootstrap_safe {
            setup.mode = PairMode::Verify;
            setup.error = Some(
                "Studio restarted before change tracking was active; Renium left both sides unchanged"
                    .to_string(),
            );
            setup.resolution_required = false;
            return Ok(());
        }
        log_global(
            5,
            format_args!(
                "[renium] clean restart checkpoint: stored={} matches={}",
                record.studio_checkpoint.is_some(),
                record
                    .studio_checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.matches_state(context, &studio_state))
            ),
        );
        if !setup.requires_reconcile
            && let (Some(checkpoint), Some(baseline)) =
                (record.studio_checkpoint.as_ref(), record.baseline.as_ref())
            && checkpoint.matches_state(context, &studio_state)
        {
            let phase = Instant::now();
            let stage = project_comparison_stage(context, &sync_services())?;
            let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
            drop(stage);
            log_reconcile_timing("clean restart comparison", phase);
            let differences = if baseline.matches(&editor) {
                HashSet::new()
            } else {
                let previous = baseline.load(Path::new(&context.root), &setup.key)?;
                snapshot_differences(&previous, &editor)?
            };
            let confirmed = read_studio_change_state(context, bridge)?;
            if checkpoint.matches_state(context, &confirmed) && differences.is_empty() {
                record.note_setup(setup);
                write_record(context, &setup.key, &record)?;
                setup.resolution_required = false;
                return Ok(());
            }
            if checkpoint.matches_state(context, &confirmed) && setup.mode.writes() {
                let mut paths = differences.into_iter().collect::<Vec<_>>();
                paths.sort();
                record.note_setup(setup);
                self.push_editor_changes_locked(
                    context,
                    &setup.key,
                    bridge,
                    &paths,
                    Some(&studio_guard),
                    &mut record,
                )?;
                setup.resolution_required = false;
                return Ok(());
            }
        }
        let phase = Instant::now();
        let selective_services = match (record.studio_checkpoint.as_ref(), record.baseline.as_ref())
        {
            (Some(checkpoint), Some(_)) => {
                selective_studio_services(context, checkpoint, &studio_state)?
            }
            _ => None,
        };
        let mut loaded_baseline = None;
        let selective_capture = if let Some(services) = selective_services {
            let baseline = record
                .baseline
                .as_ref()
                .context("Reconciliation baseline disappeared")?
                .load(Path::new(&context.root), &setup.key)?;
            let captured = capture_changed_studio_services(
                context,
                bridge,
                &services,
                &baseline,
                &studio_state,
            )?;
            loaded_baseline = Some(baseline);
            captured
        } else {
            None
        };
        let (stage, studio, editor) = if let Some(captured) = selective_capture {
            captured
        } else {
            let (stage, studio) = capture_studio_project_for_comparison(context, bridge)?;
            let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
            (stage, studio, editor)
        };
        let publish_paths = stage.publish_paths().to_vec();
        log_reconcile_timing("capture", phase);
        let phase = Instant::now();
        let side_differences = snapshot_differences(&editor, &studio)?;
        let sides_match = side_differences.is_empty();
        log_reconcile_timing("side comparison", phase);
        if sides_match {
            let phase = Instant::now();
            setup.unsynced_paths = unsynced_project_paths(context, &publish_paths, &editor)?.0;
            let baseline_matches = record
                .baseline
                .as_ref()
                .is_some_and(|baseline| baseline.matches(&editor));
            log_reconcile_timing("baseline comparison", phase);
            if !baseline_matches || !record.conflicts.is_empty() || record.resolution_required {
                let phase = Instant::now();
                record.baseline = Some(StoredSnapshot::write(
                    Path::new(&context.root),
                    &setup.key,
                    &editor,
                )?);
                record.conflicts.clear();
                record.resolution_required = false;
                log_reconcile_timing("baseline write", phase);
            }
            record.note_setup(setup);
            record.studio_checkpoint =
                reconciled_studio_checkpoint(context, bridge, &studio_state, &studio_guard);
            write_record(context, &setup.key, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }
        let phase = Instant::now();
        let baseline = match loaded_baseline {
            Some(baseline) => Some(baseline),
            None => record
                .baseline
                .as_ref()
                .map(|baseline| baseline.load(Path::new(&context.root), &setup.key))
                .transpose()?,
        };
        log_reconcile_timing("baseline load", phase);
        let phase = Instant::now();
        let merge_preference = setup
            .resolution_preference
            .unwrap_or(record.conflict_preference);
        log_global(
            5,
            format_args!(
                "[renium] reconcile merge input: baseline={} preference={merge_preference:?} resolution={:?}",
                baseline.is_some(),
                setup.resolution_preference
            ),
        );
        let (mut merged, conflicts, mut changes) = if setup.bootstrap_studio_from_editor {
            (
                editor.clone(),
                Vec::new(),
                MergeChanges {
                    editor: HashSet::new(),
                    studio: side_differences.clone(),
                },
            )
        } else {
            merge_snapshots_with_changes(
                baseline.as_ref(),
                &editor,
                &studio,
                merge_preference,
                Some(&side_differences),
            )?
        };
        log_global(
            5,
            format_args!(
                "[renium] reconcile merge result: conflicts={} editor_paths={} studio_paths={}",
                conflicts.len(),
                changes.editor.len(),
                changes.studio.len()
            ),
        );
        if !changes.editor.is_empty() {
            let mut paths = changes
                .editor
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            paths.sort();
            log_global(
                5,
                format_args!("[renium] reconcile project paths: {}", paths.join(", ")),
            );
        }
        if !changes.studio.is_empty() {
            let mut paths = changes
                .studio
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            paths.sort();
            log_global(
                5,
                format_args!("[renium] reconcile Studio paths: {}", paths.join(", ")),
            );
        }
        log_reconcile_timing("merge", phase);

        if !conflicts.is_empty() {
            record.conflicts = conflicts;
            record.resolution_required = setup.resolution_preference.is_none()
                && record.conflict_preference == ConflictPreference::None;
            record.note_setup(setup);
            write_record(context, &setup.key, &record)?;
            setup.mode = PairMode::Verify;
            setup.resolution_required = record.resolution_required;
            setup.error = Some(conflict_message(&record.conflicts));
            setup.conflicts = record.conflicts.clone();
            return Ok(());
        }

        if setup.mode == PairMode::Verify {
            record.conflicts = vec!["Studio and project files differ".to_string()];
            record.resolution_required = false;
            setup.error = Some(conflict_message(&record.conflicts));
            record.note_setup(setup);
            write_record(context, &setup.key, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }

        let mut sync_history = None;
        let phase = Instant::now();
        let push_plan = if changes.studio.is_empty() {
            ReconcilePushPlan::default()
        } else {
            reconciliation_push_plan_for_paths(&studio, &merged, &changes.studio)?
        };
        log_reconcile_timing("push plan", phase);
        let readback = if push_plan.is_empty() {
            if changes.editor.is_empty() {
                studio
            } else {
                publish_captured_studio(
                    context,
                    bridge,
                    stage,
                    studio,
                    &studio_guard,
                    &changes.editor,
                    &editor,
                )?
            }
        } else {
            let phase = Instant::now();
            sync_history = Some(history::SyncHistory::begin(
                Path::new(&context.root),
                &bound_context::source_dir(context)?,
                &studio,
                &changes.studio,
            )?);
            apply_snapshot_paths(&stage.project_root, &changes.studio, &merged)?;
            log_reconcile_timing("staged project write", phase);
            let phase = Instant::now();
            let generated = push_staged_project(
                context,
                &stage,
                bridge,
                StagedPushRequest {
                    plan: push_plan,
                    prepared_documents: HashMap::new(),
                    guard: Some(&studio_guard),
                    args: automation_push_args(context, &json!({}), false)?,
                    expected_project: Some(&editor),
                    later_edits_follow: true,
                },
            )?
            .generated;
            // Studio now holds the project's content for these paths. Record
            // that before anything else can fail, or the next reconcile would
            // mistake Renium's own push for a Studio edit and report a conflict.
            record_pushed_baseline(context, &setup.key, &mut record, &changes.studio, &editor)?;
            let generated_paths = generated.entries.keys().cloned().collect::<HashSet<_>>();
            if !generated_paths.is_empty() {
                apply_snapshot_paths(&stage.project_root, &generated_paths, &generated)?;
                for (path, entry) in generated.entries {
                    merged.entries.insert(path.clone(), entry);
                    changes.studio.insert(path);
                }
            }
            log_reconcile_timing("Studio push", phase);
            let phase = Instant::now();
            let (readback_stage, readback) = if changes.editor.is_empty() {
                let services = services_for_snapshot_paths(context, &changes.studio);
                let (readback_stage, captured) =
                    capture_studio_services(context, bridge, &services, false)?;
                let mismatches = snapshot_path_differences(&captured, &merged, &changes.studio)?;
                if !mismatches.is_empty() {
                    let details = snapshot_mismatch_details(&captured, &merged, &mismatches)?;
                    return Err(retention_failure(
                        "reconciled paths",
                        &mismatches,
                        details.as_deref(),
                    ));
                }
                let mut readback = studio;
                for path in &changes.studio {
                    match captured.entries.get(path) {
                        Some(entry) => {
                            readback.entries.insert(path.clone(), entry.clone());
                        }
                        None => {
                            readback.entries.remove(path);
                        }
                    }
                }
                (readback_stage, readback)
            } else {
                capture_studio_project(context, bridge)?
            };
            log_reconcile_timing("readback capture", phase);
            let phase = Instant::now();
            if !changes.editor.is_empty() {
                let mismatches = snapshot_differences(&readback, &merged)?;
                let mut paths = mismatches.into_iter().collect::<Vec<_>>();
                paths.sort();
                let mut detail = None;
                for path in &paths {
                    let is_settings = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(is_service_settings_file_name);
                    if !is_settings {
                        detail = Some(path.display().to_string());
                        break;
                    }
                    if let Some(found) =
                        snapshot_mismatch_details(&readback, &merged, std::slice::from_ref(path))?
                    {
                        detail = Some(found);
                        break;
                    }
                }
                if let Some(detail) = detail {
                    bail!("Studio did not retain the reconciled project state: {detail}");
                }
            }
            if !changes.editor.is_empty() {
                publish_studio_paths(context, readback_stage, &changes.editor, &editor)?;
            }
            log_reconcile_timing("readback verification", phase);
            readback
        };

        let phase = Instant::now();
        overlay_snapshot_paths(&mut merged, &readback, &changes.editor);
        let (baseline, unsynced) = synchronized_baseline(context, &publish_paths, &merged)?;
        setup.unsynced_paths = unsynced;
        if let Some(history) = sync_history {
            // Readback publication can normalize generated settings. Guard undo
            // against those accepted file bytes, not the earlier staged bytes.
            history.commit(&baseline, &ProjectSnapshot::default())?;
        }
        record.baseline = Some(StoredSnapshot::write(
            Path::new(&context.root),
            &setup.key,
            &baseline,
        )?);
        record.conflicts.clear();
        record.resolution_required = false;
        record.note_setup(setup);
        record.studio_checkpoint =
            reconciled_studio_checkpoint(context, bridge, &studio_state, &studio_guard);
        setup.resolution_required = false;
        write_record(context, &setup.key, &record)?;
        log_reconcile_timing("final baseline write", phase);
        Ok(())
    }

    pub(crate) fn reconcile_current(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<PairSetup> {
        let mut setup = self.current_setup(context, bridge)?;
        self.reconcile(context, bridge, &mut setup)?;
        Ok(setup)
    }

    pub(crate) fn reconcile_current_with_gate_held(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<PairSetup> {
        let mut setup = self.current_setup(context, bridge)?;
        self.reconcile_with_gate_held(context, bridge, &mut setup)?;
        Ok(setup)
    }

    pub(crate) fn current_setup(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<PairSetup> {
        log_global(
            5,
            format_args!("[renium] reconcile current: cx={}", context.id),
        );
        let identity = PairIdentity::from_context(context, bridge)?;
        let key = identity.pair_key();
        let record = load_record(context, &key)?.context("Reconciliation state is missing")?;
        let current_local_file_stamp = local_file_stamp(identity.local_file.as_deref())?;
        let current_local_file_digest = if record.local_file_digest.is_none()
            || record.local_file_stamp != current_local_file_stamp
        {
            local_file_digest(identity.local_file.as_deref())?
        } else {
            record.local_file_digest.clone()
        };
        Ok(PairSetup {
            key,
            identity,
            mode: record.mode,
            resolution_preference: None,
            resolution_required: record.resolution_required,
            error: None,
            conflicts: Vec::new(),
            requires_reconcile: true,
            runtime_id: context.runtime_id.clone(),
            local_file_stamp: current_local_file_stamp,
            local_file_digest: current_local_file_digest,
            bootstrap_studio_from_editor: false,
            runtime_replacement_unproven: false,
            unsynced_paths: Vec::new(),
        })
    }

    pub(crate) fn advance_baseline(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
        side: BaselineSide,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock_recover();
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            return Ok(());
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let baseline = record
            .baseline
            .as_mut()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, paths)?;
        if scopes.is_empty() {
            return Ok(());
        }
        let root = Path::new(&context.root);
        let current = capture_snapshot(root, &scopes)?;
        if matches!(side, BaselineSide::Editor) {
            let previous = baseline.load_scopes(root, key, &scopes)?;
            validate_editor_package_links(&previous, &current, &scopes)?;
        }
        baseline.replace_scopes(root, key, &scopes, &current)?;
        write_record(context, key, &record)
    }

    pub(crate) fn record_studio_checkpoint(
        &self,
        context: &BoundContext,
        key: &str,
        state: &Value,
    ) -> Result<()> {
        let Some(checkpoint) = StudioCheckpoint::from_state(context, state) else {
            return Ok(());
        };
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock_recover();
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.baseline.is_none() || !record.conflicts.is_empty() {
            return Ok(());
        }
        record.studio_checkpoint = Some(checkpoint);
        write_record(context, key, &record)
    }

    pub(crate) fn record_full_editor_push_with_gate_held(
        &self,
        context: &BoundContext,
        key: &str,
        bridge: &BridgeServer,
    ) -> Result<()> {
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock_recover();
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            bail!("Live Sync is not allowed to write in verify mode");
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let root = Path::new(&context.root);
        let stage = project_comparison_stage(context, &sync_services())?;
        let current = capture_snapshot(root, stage.publish_paths())?;
        drop(stage);
        if !record
            .baseline
            .as_ref()
            .is_some_and(|baseline| baseline.matches(&current))
        {
            record.baseline = Some(StoredSnapshot::write(root, key, &current)?);
        }
        record.studio_checkpoint = current_studio_checkpoint(context, bridge);
        write_record(context, key, &record)
    }

    pub(crate) fn validate_editor_changes(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock_recover();
        let record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            return Ok(());
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, paths)?;
        if scopes.is_empty() {
            return Ok(());
        }
        let root = Path::new(&context.root);
        let previous = baseline.load_scopes(root, key, &scopes)?;
        let current = capture_snapshot(root, &scopes)?;
        validate_editor_package_links(&previous, &current, &scopes)
    }

    pub(crate) fn push_editor_changes(
        &self,
        context: &BoundContext,
        key: &str,
        bridge: &BridgeServer,
        paths: &[PathBuf],
        guard: Option<&StudioChangeGuard>,
    ) -> Result<AppliedEditorChanges> {
        if paths.is_empty() {
            return Ok(AppliedEditorChanges::default());
        }
        let pair_lock = self.pair_lock(key);
        let phase = Instant::now();
        let _pair = pair_lock.lock_recover();
        log_reconcile_timing("incremental pair lock", phase);
        let phase = Instant::now();
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        log_reconcile_timing("incremental record load", phase);
        self.push_editor_changes_locked(context, key, bridge, paths, guard, &mut record)
    }

    pub(crate) fn push_editor_changes_locked(
        &self,
        context: &BoundContext,
        key: &str,
        bridge: &BridgeServer,
        paths: &[PathBuf],
        guard: Option<&StudioChangeGuard>,
        record: &mut PairRecord,
    ) -> Result<AppliedEditorChanges> {
        if record.mode != PairMode::Reconcile {
            bail!("Live Sync is not allowed to write in verify mode");
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, paths)?;
        if scopes.is_empty() {
            return Ok(AppliedEditorChanges::default());
        }
        let root = Path::new(&context.root);
        let phase = Instant::now();
        let previous = baseline.load_scopes(root, key, &scopes)?;
        log_reconcile_timing("incremental baseline load", phase);
        let phase = Instant::now();
        let current = capture_snapshot(root, &scopes)?;
        log_reconcile_timing("incremental project capture", phase);
        let phase = Instant::now();
        let prepared_settings = prepare_editor_settings_changes(&previous, &current, &scopes)?;
        log_reconcile_timing("incremental settings preparation", phase);
        let phase = Instant::now();
        let changed = previous
            .entries
            .keys()
            .chain(current.entries.keys())
            .filter(|path| {
                !entries_equivalent(
                    path,
                    previous.entries.get(*path),
                    current.entries.get(*path),
                )
            })
            .cloned()
            .collect::<HashSet<_>>();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &previous,
            &current,
            &changed,
            &prepared_settings,
            false,
            false,
        )?;
        log_reconcile_timing("incremental push plan", phase);
        let phase = Instant::now();
        let mut prepared_documents = HashMap::with_capacity(prepared_settings.len());
        let mut previous_documents = Vec::with_capacity(prepared_settings.len());
        for (path, change) in prepared_settings {
            let service = settings_service_name(&path, &change.current, &change.previous)?;
            previous_documents.push(change.previous);
            prepared_documents.insert(service, Arc::new(change.current));
        }
        for document in previous_documents {
            drop_settings_document(document);
        }
        log_reconcile_timing("incremental settings release", phase);
        let phase = Instant::now();
        let supporting_scopes = supporting_settings_scopes(context, &changed)?;
        let supporting = capture_snapshot(root, &supporting_scopes)?;
        let supporting_paths = supporting.entries.keys().cloned().collect::<HashSet<_>>();
        log_reconcile_timing("incremental supporting capture", phase);
        let phase = Instant::now();
        let source = bound_context::source_dir(context)?;
        let requires_stage = config::try_load_project(None, Some(root))?
            .as_ref()
            .map(config::project_requires_temporary_stage)
            .transpose()?
            .unwrap_or(false);
        let stage = if requires_stage {
            ExportProjectStage::create(
                root,
                &source,
                &services_for_snapshot_paths(context, &changed),
            )?
        } else {
            ExportProjectStage::create_for_comparison(root, &source, &[])?
        };
        log_reconcile_timing("incremental stage", phase);
        let phase = Instant::now();
        apply_snapshot_paths(&stage.project_root, &supporting_paths, &supporting)?;
        apply_snapshot_paths(&stage.project_root, &changed, &current)?;
        log_reconcile_timing("incremental staged write", phase);
        let phase = Instant::now();
        let history = history::SyncHistory::begin(root, &source, &previous, &changed)?;
        let StagedPushResult {
            generated,
            mut summary,
        } = push_staged_project(
            context,
            &stage,
            bridge,
            StagedPushRequest {
                plan,
                prepared_documents,
                guard,
                args: automation_push_args(context, &json!({}), false)?,
                expected_project: None,
                later_edits_follow: false,
            },
        )?;
        summary.insert(
            "historyId".into(),
            Value::String(history.commit(&current, &generated)?),
        );
        log_reconcile_timing("incremental Studio push", phase);
        let phase = Instant::now();
        drop(stage);
        log_reconcile_timing("incremental stage cleanup", phase);
        let phase = Instant::now();
        let baseline = record
            .baseline
            .as_mut()
            .context("Reconciliation baseline is missing")?;
        let mut changed_scopes = changed.iter().cloned().collect::<Vec<_>>();
        changed_scopes.sort();
        baseline.replace_scopes(root, key, &changed_scopes, &current)?;
        if !generated.entries.is_empty() {
            let generated_scopes = generated.entries.keys().cloned().collect::<Vec<_>>();
            baseline.replace_scopes(root, key, &generated_scopes, &generated)?;
        }
        log_reconcile_timing("incremental baseline update", phase);
        let accepted = accepted_editor_entries(root, &previous, &current, &generated);
        let phase = Instant::now();
        record.studio_checkpoint = current_studio_checkpoint(context, bridge);
        write_record(context, key, record)?;
        log_reconcile_timing("incremental record write", phase);
        Ok(AppliedEditorChanges { accepted, summary })
    }

    pub(crate) fn baseline_files(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
    ) -> Result<BTreeMap<String, String>> {
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock_recover();
        let record = load_record(context, key)?.context("Reconciliation state is missing")?;
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let root = Path::new(&context.root);
        let requested = paths
            .iter()
            .filter_map(|requested| {
                let absolute = if requested.is_absolute() {
                    requested.clone()
                } else {
                    root.join(requested)
                };
                let relative = absolute.strip_prefix(root).ok()?.to_path_buf();
                Some((absolute, relative))
            })
            .collect::<Vec<_>>();
        let scopes = requested
            .iter()
            .map(|(_, relative)| relative.clone())
            .collect::<Vec<_>>();
        let baseline = baseline.load_scopes(root, key, &scopes)?;
        let mut files = BTreeMap::new();
        for (absolute, relative) in requested {
            let Some(SnapshotEntry::File(bytes)) = baseline.entries.get(&relative) else {
                continue;
            };
            let content = String::from_utf8(bytes.clone())
                .with_context(|| format!("Baseline file {} is not UTF-8", relative.display()))?;
            files.insert(absolute.to_string_lossy().into_owned(), content);
        }
        Ok(files)
    }

    pub(crate) fn claim_target(&self, identity: &PairIdentity) -> Option<String> {
        let mut owners = self.owners.lock_recover();
        let target = identity.target_key();
        let pair = identity.pair_key();
        match owners.by_target.get(&target) {
            Some((owner_pair, _)) if owner_pair == &pair => None,
            Some((_, owner_project)) => Some(owner_project.clone()),
            None => {
                owners
                    .by_target
                    .insert(target.clone(), (pair.clone(), identity.project.clone()));
                owners.target_by_pair.insert(pair, target);
                None
            }
        }
    }

    pub(crate) fn release_target(&self, pair: &str) {
        let mut owners = self.owners.lock_recover();
        let Some(target) = owners.target_by_pair.remove(pair) else {
            return;
        };
        if owners
            .by_target
            .get(&target)
            .is_some_and(|(owner_pair, _)| owner_pair == pair)
        {
            owners.by_target.remove(&target);
        }
    }
}

// Files edited while a reconcile ran differ from the state it synchronized.
// Returns those paths and the project as it is now.
fn unsynced_project_paths(
    context: &BoundContext,
    publish_paths: &[PathBuf],
    synchronized: &ProjectSnapshot,
) -> Result<(Vec<PathBuf>, ProjectSnapshot)> {
    let current = capture_snapshot(Path::new(&context.root), publish_paths)?;
    let mut paths = snapshot_differences(synchronized, &current)?
        .into_iter()
        .collect::<Vec<_>>();
    paths.sort();
    if !paths.is_empty() {
        log_global(
            5,
            format_args!(
                "[renium] reconcile kept pending edits made during it: {}",
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    Ok((paths, current))
}

// The files hold what the readback published for Studio's paths, which can
// differ from the merge by properties the engine recomputes.
fn overlay_snapshot_paths(
    target: &mut ProjectSnapshot,
    source: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
) {
    for path in paths {
        match source.entries.get(path) {
            Some(entry) => {
                target.entries.insert(path.clone(), entry.clone());
            }
            None => {
                target.entries.remove(path);
            }
        }
    }
}

// The project as it is now, except that files edited during the reconcile
// keep the content it synchronized, so Live Sync still pushes those edits.
fn synchronized_baseline(
    context: &BoundContext,
    publish_paths: &[PathBuf],
    synchronized: &ProjectSnapshot,
) -> Result<(ProjectSnapshot, Vec<PathBuf>)> {
    let (unsynced, mut baseline) = unsynced_project_paths(context, publish_paths, synchronized)?;
    for path in &unsynced {
        match synchronized.entries.get(path) {
            Some(entry) => {
                baseline.entries.insert(path.clone(), entry.clone());
            }
            None => {
                baseline.entries.remove(path);
            }
        }
    }
    Ok((baseline, unsynced))
}

fn record_pushed_baseline(
    context: &BoundContext,
    key: &str,
    record: &mut PairRecord,
    pushed: &HashSet<PathBuf>,
    editor: &ProjectSnapshot,
) -> Result<()> {
    let Some(baseline) = record.baseline.as_mut() else {
        return Ok(());
    };
    if pushed.is_empty() {
        return Ok(());
    }
    let scopes = pushed.iter().cloned().collect::<Vec<_>>();
    let pushed_content = ProjectSnapshot {
        entries: editor
            .entries
            .iter()
            .filter(|(path, _)| scopes.iter().any(|scope| path.starts_with(scope)))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect(),
    };
    baseline.replace_scopes(Path::new(&context.root), key, &scopes, &pushed_content)?;
    write_record(context, key, record)
}
