use super::*;

/// A failed push can leave the pair record holding its conflicts; the next
/// write names the ways out instead of stopping at the state.
const UNRESOLVED_CHANGES: &str = "Reconciliation state has unresolved changes. `rbx lst` lists them; decide each with `rbx lon --resolve ID=studio|editor`, or all with `rbx lon --prefer studio|editor` (either also pushes the other side's one-sided edits); `rbx lof` then a scoped push (`-p`/`-f` with `-u`) leaves the rest of Studio untouched";

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
    /// The side each conflict decided by id takes in this comparison.
    pub(crate) decisions: BTreeMap<String, ConflictSide>,
    pub(crate) resolution_required: bool,
    pub(crate) error: Option<String>,
    pub(crate) conflicts: Vec<Conflict>,
    /// What verify mode found different, path by path.
    pub(crate) differences: Vec<String>,
    pub(crate) requires_reconcile: bool,
    pub(crate) runtime_id: Option<String>,
    pub(crate) local_file_stamp: Option<LocalFileStamp>,
    pub(crate) local_file_digest: Option<String>,
    pub(crate) bootstrap_studio_from_editor: bool,
    pub(crate) runtime_replacement_unproven: bool,
    /// Project paths edited while the reconcile ran. Studio has not received
    /// them, so Live Sync keeps them pending instead of absorbing them.
    pub(crate) unsynced_paths: Vec<PathBuf>,
    /// The decisions the comparison took without effect.
    pub(crate) moot_decisions: Vec<MootDecision>,
}

/// What a comparison of the captured sides gives.
enum Comparison {
    /// Both sides hold the same project.
    Same,
    Merged {
        merged: ProjectSnapshot,
        conflicts: Vec<Conflict>,
        changes: Box<MergeChanges>,
    },
    /// Decisions the comparison could not take, with the conflicts open
    /// now; nothing is applied.
    Refused {
        message: String,
        open: Vec<Conflict>,
    },
}

#[derive(Default)]
pub(crate) struct Coordinator {
    pub(crate) pairs: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pub(crate) owners: Mutex<TargetOwners>,
}

/// Captures both sides for a comparison: only the services Studio changed
/// since the stored checkpoint when the baseline allows it, else the whole
/// project, with packed link scripts folded into Studio's side.
struct ReconcileCapture {
    stage: ExportProjectStage,
    studio: ProjectSnapshot,
    editor: ProjectSnapshot,
    loaded_baseline: Option<ProjectSnapshot>,
    link_enforcement: Option<crate::project::package_links::LinkEnforcement>,
    publish_paths: Vec<PathBuf>,
}

fn capture_for_reconcile(
    context: &BoundContext,
    bridge: &BridgeServer,
    setup: &PairSetup,
    record: &PairRecord,
    studio_state: &Value,
) -> Result<ReconcileCapture> {
    let phase = Instant::now();
    let selective_services = match (record.studio_checkpoint.as_ref(), record.baseline.as_ref()) {
        (Some(checkpoint), Some(_)) => {
            selective_studio_services(context, checkpoint, studio_state)?
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
        let captured =
            capture_changed_studio_services(context, bridge, &services, &baseline, studio_state)?;
        loaded_baseline = Some(baseline);
        captured
    } else {
        None
    };
    let (stage, mut studio, editor) = if let Some(captured) = selective_capture {
        captured
    } else {
        let (stage, studio) = capture_studio_project_for_comparison(context, bridge)?;
        let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
        (stage, studio, editor)
    };
    let link_enforcement =
        packing_link_enforcement(Path::new(&context.root), Path::new(&context.source))?;
    if let Some(enforcement) = &link_enforcement {
        pack_read_only_link_scripts_with(
            enforcement,
            Path::new(&context.root),
            &editor,
            &mut studio,
        )?;
    }
    let publish_paths = stage.publish_paths().to_vec();
    log_reconcile_timing("capture", phase);
    Ok(ReconcileCapture {
        stage,
        studio,
        editor,
        loaded_baseline,
        link_enforcement,
        publish_paths,
    })
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
        let identity = PairIdentity::from_context(context, bridge)?;
        self.prepare_pair(context, identity, configuration)
    }

    fn prepare_pair(
        &self,
        context: &BoundContext,
        identity: PairIdentity,
        configuration: PairConfiguration,
    ) -> Result<PairSetup> {
        let PairConfiguration {
            mode: requested_mode,
            conflict_preference,
            resolution_preference,
            decisions,
            runtime_settings,
        } = configuration;
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
            differences: Vec::new(),
        });
        refuse_decisions_with_nothing_open(&decisions, &record)?;
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
        let mode_changed = record.mode != mode
            && !(mode == PairMode::Verify && record.mode == PairMode::Reconcile);
        let record_changed = record_missing
            || obsolete_head
            || record.identity != identity
            || mode_changed
            || record.conflict_preference != conflict_preference
            || record.runtime_settings != runtime_settings
            || !runtime_replaced
                && (record.local_file_stamp != current_local_file_stamp
                    || record.local_file_digest != current_local_file_digest);
        let requires_reconcile = record_missing
            || configuration_changed
            || mode_changed
            || record.conflict_preference != conflict_preference
            || record.has_open_review()
            || runtime_replaced;
        if configuration_changed {
            note_configuration_change(&mut record);
        }
        let mode = continued_pair_mode(
            mode,
            (!record_missing).then_some(record.mode),
            requires_reconcile,
            unresolved_local || owner_conflict.is_some(),
        );
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
            decisions,
            conflicts: record.conflicts.clone(),
            differences: record.differences.clone(),
            resolution_required: !unresolved_local
                && owner_conflict.is_none()
                && (record.resolution_required
                    || record.has_open_review()
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
            moot_decisions: Vec::new(),
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
        let _gate = bridge.acquire_edit_gate(context.runtime_id.as_deref(), "live-sync");
        self.reconcile_with_gate_held(context, bridge, setup)
    }

    /// A clean restart whose files match the stored baseline continues the
    /// pair without a comparison, or pushes the file changes made meanwhile;
    /// answers whether the setup was finished that way.
    fn finish_clean_restart(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        setup: &mut PairSetup,
        record: &mut PairRecord,
        studio_guard: &StudioChangeGuard,
        studio_state: &Value,
    ) -> Result<bool> {
        if !setup.requires_reconcile
            && let (Some(checkpoint), Some(baseline)) =
                (record.studio_checkpoint.as_ref(), record.baseline.as_ref())
            && checkpoint.matches_state(context, studio_state)
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
                continue_clean_verify(setup, record);
                record.note_setup(setup);
                write_record(context, &setup.key, record)?;
                setup.resolution_required = false;
                return Ok(true);
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
                    Some(studio_guard),
                    record,
                )?;
                setup.resolution_required = false;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // Lock order everywhere: the bridge request gate first (this Edit
    // runtime's gate, then the process-wide one), then the Live Sync activity,
    // then the pair lock. A request holding the gate may wait for the sync
    // activity to clear, so a sync must never hold the activity while it waits
    // for the gate.
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
        if self.finish_clean_restart(
            context,
            bridge,
            setup,
            &mut record,
            &studio_guard,
            &studio_state,
        )? {
            return Ok(());
        }
        let ReconcileCapture {
            stage,
            studio,
            editor,
            loaded_baseline,
            link_enforcement,
            publish_paths,
        } = capture_for_reconcile(context, bridge, setup, &record, &studio_state)?;
        let phase = Instant::now();
        let side_differences = snapshot_differences(&editor, &studio)?;
        log_reconcile_timing("side comparison", phase);
        let phase = Instant::now();
        let comparison = compare_sides(
            setup,
            &record,
            [&editor, &studio],
            &side_differences,
            || {
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
                Ok(baseline)
            },
        )?;
        let (mut merged, conflicts, mut changes) = match comparison {
            Comparison::Merged {
                merged,
                conflicts,
                changes,
            } => (merged, conflicts, *changes),
            Comparison::Refused { message, open } => {
                return hold_refused(context, setup, &mut record, &studio_state, message, open);
            }
            Comparison::Same => {
                let phase = Instant::now();
                setup.unsynced_paths = unsynced_project_paths(context, &publish_paths, &editor)?.0;
                let baseline_matches = record
                    .baseline
                    .as_ref()
                    .is_some_and(|baseline| baseline.matches(&editor));
                log_reconcile_timing("baseline comparison", phase);
                if !baseline_matches || record.has_open_review() || record.resolution_required {
                    let phase = Instant::now();
                    record.baseline = Some(StoredSnapshot::write(
                        Path::new(&context.root),
                        &setup.key,
                        &editor,
                    )?);
                    record.clear_review();
                    record.resolution_required = false;
                    log_reconcile_timing("baseline write", phase);
                }
                continue_clean_verify(setup, &mut record);
                record.note_setup(setup);
                record.studio_checkpoint =
                    reconciled_studio_checkpoint(context, bridge, &studio_state, &studio_guard);
                write_record(context, &setup.key, &record)?;
                setup.resolution_required = false;
                return Ok(());
            }
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
            return hold_for_conflicts(context, setup, &mut record, &studio_state, conflicts);
        }

        if verify_mode_holds(context, setup, &mut record, &changes)? {
            return Ok(());
        }
        setup.moot_decisions = std::mem::take(&mut changes.moot_decisions);

        let mut sync_history = None;
        let phase = Instant::now();
        let push_plan = if changes.studio.is_empty() {
            ReconcilePushPlan::default()
        } else {
            let prepared = prepared_studio_settings_changes(&merged, &mut changes)?;
            let mut plan = reconciliation_push_plan_for_paths_with_prepared_settings(
                &studio,
                &merged,
                &changes.studio,
                &prepared,
                false,
                false,
            )?;
            attach_capture_identities(&mut plan, &studio, &prepared)?;
            plan
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
            let mut args = automation_push_args(context, &json!({ "yes": true }), false)?;
            sync_history = Some(history::SyncHistory::begin_push(
                Path::new(&context.root),
                &bound_context::source_dir(context)?,
                &studio,
                &changes.studio,
                &mut args,
            )?);
            apply_snapshot_paths(&stage.project_root, &changes.studio, &merged)?;
            log_reconcile_timing("staged project write", phase);
            let phase = Instant::now();
            let pushed = push_staged_project(
                context,
                &stage,
                bridge,
                StagedPushRequest {
                    plan: push_plan,
                    prepared_documents: HashMap::new(),
                    guard: Some(&studio_guard),
                    args,
                    expected_project: Some(&editor),
                    later_edits_follow: true,
                },
            )?;
            let pending: Vec<crate::editor::types::PendingImportFields> = pushed
                .summary
                .get("supplementalImportPendingFields")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            let generated = pushed.generated;
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
                let (readback_stage, mut captured) =
                    capture_studio_services(context, bridge, &services, false)?;
                if let Some(enforcement) = &link_enforcement {
                    pack_read_only_link_scripts_with(
                        enforcement,
                        Path::new(&context.root),
                        &merged,
                        &mut captured,
                    )?;
                }
                let mismatches =
                    reconciled_path_differences(&captured, &merged, &changes.studio, &pending)?;
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
                let (readback_stage, mut readback) = capture_studio_project(context, bridge)?;
                if let Some(enforcement) = &link_enforcement {
                    pack_read_only_link_scripts_with(
                        enforcement,
                        Path::new(&context.root),
                        &merged,
                        &mut readback,
                    )?;
                }
                (readback_stage, readback)
            };
            log_reconcile_timing("readback capture", phase);
            let phase = Instant::now();
            if !changes.editor.is_empty() {
                let mismatches = if pending.is_empty() {
                    snapshot_differences(&readback, &merged)?
                        .into_iter()
                        .collect()
                } else {
                    let compared = readback
                        .entries
                        .keys()
                        .chain(merged.entries.keys())
                        .cloned()
                        .collect();
                    reconciled_path_differences(&readback, &merged, &compared, &pending)?
                };
                let mut paths: Vec<PathBuf> = mismatches;
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
            if !pending.is_empty() {
                // Preserve the actual Studio baseline so the files' pending
                // values remain changes to retry. When both sides changed a
                // store, publish the reviewed merge, which includes Studio's
                // edits and keeps the requested pending values.
                if !changes.editor.is_empty() {
                    let overlap = changes
                        .editor
                        .intersection(&changes.studio)
                        .cloned()
                        .collect();
                    apply_snapshot_paths(&readback_stage.project_root, &overlap, &merged)?;
                    publish_studio_paths(context, readback_stage, &changes.editor, &editor)?;
                }
                let synchronized = changes.studio.union(&changes.editor).cloned().collect();
                if record.baseline.is_none() {
                    // A pairing that had no baseline takes the merged project
                    // with Studio as it stands, so the pending values stay
                    // changes of the files.
                    let mut as_it_stands = merged.clone();
                    overlay_snapshot_paths(&mut as_it_stands, &readback, &synchronized);
                    record.baseline = Some(StoredSnapshot::write(
                        Path::new(&context.root),
                        &setup.key,
                        &as_it_stands,
                    )?);
                } else {
                    record_pushed_baseline(
                        context,
                        &setup.key,
                        &mut record,
                        &synchronized,
                        &readback,
                    )?;
                }
                if let Some(mut history) = sync_history.take() {
                    let files = capture_snapshot(Path::new(&context.root), &publish_paths)?;
                    history.commit(&files, &ProjectSnapshot::default())?;
                }
                // The comparison is settled: what it found before is gone, as
                // after any other reconcile that reached Studio.
                record.clear_review();
                record.resolution_required = false;
                setup.resolution_required = false;
                record.studio_checkpoint = None;
                record.note_setup(setup);
                write_record(context, &setup.key, &record)?;
                setup.unsynced_paths = changes.studio.iter().cloned().collect();
                setup.unsynced_paths.sort();
                setup.error = Some(format!(
                    "Studio committed; {} deferred fields remain pending. Retry the push to write them.",
                    pending
                        .iter()
                        .map(|row| row.properties.len())
                        .sum::<usize>()
                ));
                return Ok(());
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
        if let Some(mut history) = sync_history {
            // Readback publication can normalize generated settings. Guard undo
            // against those accepted file bytes, not the earlier staged bytes.
            history.commit(&baseline, &ProjectSnapshot::default())?;
        }
        record.baseline = Some(StoredSnapshot::write(
            Path::new(&context.root),
            &setup.key,
            &baseline,
        )?);
        record.clear_review();
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
            decisions: BTreeMap::new(),
            resolution_required: record.resolution_required,
            error: None,
            conflicts: Vec::new(),
            differences: Vec::new(),
            requires_reconcile: true,
            runtime_id: context.runtime_id.clone(),
            local_file_stamp: current_local_file_stamp,
            local_file_digest: current_local_file_digest,
            bootstrap_studio_from_editor: false,
            runtime_replacement_unproven: false,
            unsynced_paths: Vec::new(),
            moot_decisions: Vec::new(),
        })
    }

    pub(crate) fn advance_baseline(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
        side: BaselineSide,
    ) -> Result<()> {
        self.advance_baseline_inner(context, key, paths, side, &BTreeMap::new())
    }

    pub(crate) fn advance_pulled_baseline(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
        studio_overrides: &crate::project::package_links::StudioOverrides,
    ) -> Result<()> {
        self.advance_baseline_inner(context, key, paths, BaselineSide::Studio, studio_overrides)
    }

    fn advance_baseline_inner(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
        side: BaselineSide,
        studio_overrides: &crate::project::package_links::StudioOverrides,
    ) -> Result<()> {
        if paths.is_empty() && studio_overrides.is_empty() {
            return Ok(());
        }
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock_recover();
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            return Ok(());
        }
        let paths = paths
            .iter()
            .cloned()
            .chain(studio_overrides.keys().cloned())
            .collect::<Vec<_>>();
        // The advanced paths now agree on both sides, so their recorded
        // conflicts are resolved; the others stay for the next `lon`.
        record
            .conflicts
            .retain(|conflict| !conflict.covered_by_paths(&paths));
        record
            .differences
            .retain(|line| !conflict_covered_by_paths(line, &paths));
        let baseline = record
            .baseline
            .as_mut()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, &paths)?;
        if scopes.is_empty() {
            return Ok(());
        }
        let root = Path::new(&context.root);
        let mut current = capture_snapshot(root, &scopes)?;
        let selected = ScopeSet::new(&scopes);
        for (path, entry) in studio_overrides
            .iter()
            .filter(|(path, _)| selected.contains(path))
        {
            match entry {
                Some(entry) => {
                    current.entries.insert(path.clone(), entry.clone());
                }
                None => {
                    current.entries.remove(path);
                }
            }
        }
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
        if record.baseline.is_none() || record.has_open_review() {
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
        if record.has_open_review() {
            bail!("{UNRESOLVED_CHANGES}");
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
        // A scoped push is the documented way to resolve recorded conflicts
        // toward the files, so they do not block it; the baseline advance
        // drops the ones the push covered.
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
        if record.has_open_review() {
            bail!("{UNRESOLVED_CHANGES}");
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
        let mut inputs = incremental_push_inputs(root, baseline, key, &scopes)?;
        let mut promoted_services = promote_heavy_deltas_to_replacement(
            context,
            &inputs.current,
            &inputs.previous,
            &mut inputs.changed,
            &mut inputs.prepared,
        )?;
        if !promoted_services.is_empty() {
            // A replacement sends the service's unchanged scripts too and
            // rewrites every reference into it from any service, so it plans
            // against the whole project, not the pending scopes alone.
            let stage = project_comparison_stage(context, &sync_services())?;
            let full = stage.publish_paths().to_vec();
            drop(stage);
            inputs = incremental_push_inputs(root, baseline, key, &full)?;
            promoted_services = promote_heavy_deltas_to_replacement(
                context,
                &inputs.current,
                &inputs.previous,
                &mut inputs.changed,
                &mut inputs.prepared,
            )?;
        }
        let IncrementalPushInputs {
            previous,
            current,
            prepared: prepared_settings,
            changed,
        } = inputs;
        let phase = Instant::now();
        let mut plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &previous,
            &current,
            &changed,
            &prepared_settings,
            !promoted_services.is_empty(),
            false,
        )?;
        extend_replaced_service_references(
            &current,
            &prepared_settings,
            &promoted_services,
            &mut plan,
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
        let mut args = automation_push_args(context, &json!({ "yes": true }), false)?;
        let mut history =
            history::SyncHistory::begin_push(root, &source, &previous, &changed, &mut args)?;
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
                args,
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
    let pushed_scopes = ScopeSet::new(&scopes);
    let pushed_content = ProjectSnapshot {
        entries: editor
            .entries
            .iter()
            .filter(|(path, _)| pushed_scopes.contains(path))
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect(),
    };
    baseline.replace_scopes(Path::new(&context.root), key, &scopes, &pushed_content)?;
    write_record(context, key, record)
}

struct IncrementalPushInputs {
    previous: ProjectSnapshot,
    current: ProjectSnapshot,
    prepared: HashMap<PathBuf, PreparedEditorSettingsChange>,
    changed: HashSet<PathBuf>,
}

fn incremental_push_inputs(
    root: &Path,
    baseline: &StoredSnapshot,
    key: &str,
    scopes: &[PathBuf],
) -> Result<IncrementalPushInputs> {
    let phase = Instant::now();
    let previous = baseline.load_scopes(root, key, scopes)?;
    log_reconcile_timing("incremental baseline load", phase);
    let phase = Instant::now();
    let current = capture_snapshot(root, scopes)?;
    log_reconcile_timing("incremental project capture", phase);
    let phase = Instant::now();
    let prepared = prepare_editor_settings_changes(&previous, &current, scopes)?;
    log_reconcile_timing("incremental settings preparation", phase);
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
        .collect();
    Ok(IncrementalPushInputs {
        previous,
        current,
        prepared,
        changed,
    })
}

/// Verify only guards the first comparison; once it is clean the session
/// writes like any other.
fn continue_clean_verify(setup: &mut PairSetup, record: &mut PairRecord) {
    if setup.mode == PairMode::Verify {
        setup.mode = PairMode::Reconcile;
        setup.error = None;
    }
    record.mode = setup.mode;
}

/// A changed project or experience manifest (a shared link applied, a place
/// alias added) forces a full comparison but keeps the baseline: the files
/// the configuration change wrote are one-sided edits against it, whereas a
/// comparison without a baseline reports every such value as a conflict
/// between both sides. The Studio checkpoint goes so no service is skipped.
pub(crate) fn note_configuration_change(record: &mut PairRecord) {
    record.studio_checkpoint = None;
    record.clear_review();
    record.resolution_required = false;
}

/// A verify request on a pair whose record already continues in reconcile,
/// with nothing that calls for a new comparison, keeps reconcile: the pair
/// was verified when its session started, and a `lon` re-run or a daemon
/// restart must not turn a running session read-only without a comparison.
/// Forced verify (unproven local file, foreign owner) stays verify.
pub(crate) fn continued_pair_mode(
    requested: PairMode,
    recorded: Option<PairMode>,
    requires_reconcile: bool,
    forced_verify: bool,
) -> PairMode {
    if requested == PairMode::Verify
        && recorded == Some(PairMode::Reconcile)
        && !requires_reconcile
        && !forced_verify
    {
        PairMode::Reconcile
    } else {
        requested
    }
}

/// A session that starts in verify mode continues as reconcile when the first
/// comparison finds nothing; otherwise it records the differing paths, holds
/// pushes and pulls, and reports them.
fn verify_mode_holds(
    context: &BoundContext,
    setup: &mut PairSetup,
    record: &mut PairRecord,
    changes: &MergeChanges,
) -> Result<bool> {
    if setup.mode != PairMode::Verify {
        return Ok(false);
    }
    if changes.studio.is_empty() && changes.editor.is_empty() {
        continue_clean_verify(setup, record);
        return Ok(false);
    }
    record.differences = verify_difference_lines(&changes.studio, &changes.editor);
    record.conflicts.clear();
    record.resolution_required = false;
    setup.error = Some(format!("Sync needs review: {VERIFY_DIFFERENCE_HEADING}"));
    setup.differences = record.differences.clone();
    setup.conflicts.clear();
    record.note_setup(setup);
    write_record(context, &setup.key, record)?;
    setup.resolution_required = false;
    Ok(true)
}

/// Verify mode records what reconcile would do, path by path, so
/// `lst --details` can show it: `push:` paths differ on the project side,
/// `pull:` paths differ on the Studio side.
pub(crate) fn verify_difference_lines(
    push: &HashSet<PathBuf>,
    pull: &HashSet<PathBuf>,
) -> Vec<String> {
    const LIMIT: usize = 60;
    let mut lines = vec![VERIFY_DIFFERENCE_HEADING.to_string()];
    let mut entries = push
        .iter()
        .map(|path| format!("push: {}", path.display()))
        .chain(pull.iter().map(|path| format!("pull: {}", path.display())))
        .collect::<Vec<_>>();
    entries.sort();
    let total = entries.len();
    lines.extend(entries.into_iter().take(LIMIT));
    if total > LIMIT {
        lines.push(format!("{} more paths", total - LIMIT));
    }
    lines
}

/// With nothing open in the pair, every decision is stale; it is refused
/// before a running session is touched. Open conflicts are checked by the
/// comparison that decides them.
fn refuse_decisions_with_nothing_open(
    decisions: &BTreeMap<String, ConflictSide>,
    record: &PairRecord,
) -> Result<()> {
    if decisions.is_empty() || !record.conflicts.is_empty() {
        return Ok(());
    }
    let ids = decisions.keys().map(String::as_str).collect::<Vec<_>>();
    bail!("{}", unknown_decisions_message(&ids))
}

/// A comparison that meets no conflict at all has nothing a decision could
/// name: the decisions are stale, and nothing is applied.
fn refuse_unmatched_decisions(decisions: &BTreeMap<String, ConflictSide>) -> Result<()> {
    if decisions.is_empty() {
        return Ok(());
    }
    let ids = decisions.keys().map(String::as_str).collect::<Vec<_>>();
    bail!("{}; nothing was applied", unknown_decisions_message(&ids))
}

/// How this comparison settles conflicts: the decisions by id it was given,
/// and the preference of this run, else the pair's.
fn comparison_decisions(setup: &PairSetup, record: &PairRecord) -> ConflictDecisions {
    ConflictDecisions {
        preference: setup
            .resolution_preference
            .unwrap_or(record.conflict_preference),
        by_id: setup.decisions.clone(),
    }
}

/// Compares the captured sides of a pair: the files first, then Studio,
/// with what differs between them. Decisions that name nothing this
/// comparison meets are refused before anything is applied; otherwise the
/// sides merge by the decisions. `baseline` loads the last sync when the
/// sides differ.
fn compare_sides(
    setup: &PairSetup,
    record: &PairRecord,
    sides: [&ProjectSnapshot; 2],
    side_differences: &HashSet<PathBuf>,
    baseline: impl FnOnce() -> Result<Option<ProjectSnapshot>>,
) -> Result<Comparison> {
    let [editor, studio] = sides;
    let refused = |error: anyhow::Error| Comparison::Refused {
        message: error.to_string(),
        open: Vec::new(),
    };
    if side_differences.is_empty() {
        if let Err(error) = refuse_unmatched_decisions(&setup.decisions) {
            return Ok(refused(error));
        }
        return Ok(Comparison::Same);
    }
    let baseline = baseline()?;
    let decisions = comparison_decisions(setup, record);
    log_global(
        5,
        format_args!(
            "[renium] reconcile merge input: baseline={} preference={:?} resolution={:?} decisions={}",
            baseline.is_some(),
            decisions.preference,
            setup.resolution_preference,
            decisions.by_id.len()
        ),
    );
    if setup.bootstrap_studio_from_editor {
        if let Err(error) = refuse_unmatched_decisions(&setup.decisions) {
            return Ok(refused(error));
        }
        return Ok(Comparison::Merged {
            merged: editor.clone(),
            conflicts: Vec::new(),
            changes: Box::new(MergeChanges {
                studio: side_differences.clone(),
                ..MergeChanges::default()
            }),
        });
    }
    let preference = decisions.preference;
    let error = match merge_snapshots_with_changes(
        baseline.as_ref(),
        editor,
        studio,
        decisions,
        Some(side_differences),
    ) {
        Ok((merged, conflicts, changes)) => {
            return Ok(Comparison::Merged {
                merged,
                conflicts,
                changes: Box::new(changes),
            });
        }
        Err(error) => error,
    };
    let Some(refusal) = error.downcast_ref::<RefusedDecisions>() else {
        return Err(error);
    };
    let (_, open, _) = merge_snapshots_with_changes(
        baseline.as_ref(),
        editor,
        studio,
        preference,
        Some(side_differences),
    )?;
    Ok(Comparison::Refused {
        message: refusal_message(refusal, &record.conflicts, &open),
        open,
    })
}

/// Why decisions were refused. A decision for a listed conflict whose
/// values changed since names it, with the id and record it has now; one
/// for a listed conflict nothing matches any more says so.
fn refusal_message(refusal: &RefusedDecisions, listed: &[Conflict], open: &[Conflict]) -> String {
    let mut parts = refusal.reasons.clone();
    let mut unknown = Vec::new();
    for id in &refusal.unknown {
        let Some(before) = listed.iter().find(|conflict| conflict.id == *id) else {
            unknown.push(id.as_str());
            continue;
        };
        let now = open
            .iter()
            .filter(|conflict| conflict.same_subject(before))
            .max_by_key(|conflict| {
                conflict
                    .detail
                    .chars()
                    .zip(before.detail.chars())
                    .take_while(|(now, then)| now == then)
                    .count()
            });
        parts.push(match now {
            Some(now) => format!(
                "Conflict {id} changed since it was listed; it is {} now: {}",
                now.id, now.detail
            ),
            None => format!("Conflict {id} is no longer open: {}", before.detail),
        });
    }
    if !unknown.is_empty() {
        parts.push(unknown_decisions_message(&unknown));
    }
    format!("{}; nothing was applied", parts.join("; "))
}

/// Keeps both sides as they are after refusing decisions: the conflicts
/// open now are recorded and listed, and the refusal is the error.
fn hold_refused(
    context: &BoundContext,
    setup: &mut PairSetup,
    record: &mut PairRecord,
    studio_state: &Value,
    message: String,
    open: Vec<Conflict>,
) -> Result<()> {
    if open.is_empty() {
        setup.mode = PairMode::Verify;
        setup.resolution_required = false;
        setup.conflicts.clear();
        setup.differences.clear();
    } else {
        hold_for_conflicts(context, setup, record, studio_state, open)?;
    }
    setup.error = Some(message);
    Ok(())
}

/// Keeps both sides as they are and records the open conflicts, with the
/// evidence about each, for the caller to decide.
fn hold_for_conflicts(
    context: &BoundContext,
    setup: &mut PairSetup,
    record: &mut PairRecord,
    studio_state: &Value,
    mut conflicts: Vec<Conflict>,
) -> Result<()> {
    attach_conflict_evidence(context, record, studio_state, &mut conflicts);
    record.conflicts = conflicts;
    record.differences.clear();
    record.resolution_required = setup.resolution_preference.is_none()
        && record.conflict_preference == ConflictPreference::None;
    record.note_setup(setup);
    write_record(context, &setup.key, record)?;
    setup.mode = PairMode::Verify;
    setup.resolution_required = record.resolution_required;
    setup.error = Some(conflict_message(&record.conflicts));
    setup.conflicts = record.conflicts.clone();
    setup.differences.clear();
    Ok(())
}

/// Adds what the disk and Studio's change tracker say about each conflict:
/// when its file was last written, and, for a store, whether Studio's
/// service changed since the last sync, when the tracker can tell.
fn attach_conflict_evidence(
    context: &BoundContext,
    record: &PairRecord,
    studio_state: &Value,
    conflicts: &mut [Conflict],
) {
    let root = Path::new(&context.root);
    let changed_services = record
        .studio_checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.changed_services(context, studio_state));
    for conflict in conflicts {
        if conflict.path.is_empty() {
            continue;
        }
        let relative = PathBuf::from(&conflict.path);
        if let Ok(modified) = fs::metadata(root.join(&relative)).and_then(|meta| meta.modified()) {
            conflict.files_changed_at = Some(crate::app::report::utc_stamp(modified));
        }
        let is_store = relative
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name);
        if let Some(changed) = &changed_services
            && is_store
            && let [service] =
                services_for_snapshot_paths(context, &HashSet::from([relative])).as_slice()
        {
            conflict.studio_changed = Some(changed.contains(service));
        }
    }
}

/// Pairs each changed Studio settings document, as the merge aligned it, with
/// the merged document the push must produce. Studio locates the instances a
/// push replaces by the paths they had in its own tree, so those paths have
/// to come from the same pairing that decided which instances changed; a
/// second, independent alignment paired identical same-named siblings across
/// parents and handed Studio colliding previous locations.
fn prepared_studio_settings_changes(
    merged: &ProjectSnapshot,
    changes: &mut MergeChanges,
) -> Result<HashMap<PathBuf, PreparedEditorSettingsChange>> {
    std::mem::take(&mut changes.aligned_studio)
        .into_iter()
        .filter(|(path, _)| changes.studio.contains(path))
        .map(|(path, mut previous)| {
            let current = settings_document(merged.entries.get(&path))?;
            align_equivalent_values(&current, &mut previous);
            Ok((path, PreparedEditorSettingsChange { previous, current }))
        })
        .collect()
}

#[cfg(test)]
mod conflict_cover_tests {
    use super::{Conflict, ConflictKind, conflict_covered_by_paths};
    use std::path::PathBuf;

    #[test]
    fn a_push_of_the_conflicting_path_or_its_directory_covers_the_conflict() {
        let script = vec![PathBuf::from("src/ServerScriptService/Battle.luau")];
        let folder = vec![PathBuf::from(r"src\ServerScriptService")];
        let store = vec![PathBuf::from("instances/ServerStorage.renium")];
        for conflict in [
            "src/ServerScriptService/Battle.luau changed on both sides",
            r"pull: src\ServerScriptService\Battle.luau",
            "push: src/ServerScriptService/Battle.luau",
        ] {
            assert!(conflict_covered_by_paths(conflict, &script), "{conflict}");
            assert!(conflict_covered_by_paths(conflict, &folder), "{conflict}");
            assert!(!conflict_covered_by_paths(conflict, &store), "{conflict}");
        }
        let store_conflict =
            r"instances\ServerStorage.renium: ServerStorage/Maps/A: property Value: absent vs 1";
        assert!(conflict_covered_by_paths(store_conflict, &store));
        assert!(!conflict_covered_by_paths(store_conflict, &script));
        assert!(!conflict_covered_by_paths(
            "src/ServerScriptService/Battle.luau.bak changed on both sides",
            &script
        ));
        assert!(!conflict_covered_by_paths("3 more paths", &script));
        let record = Conflict::new(
            ConflictKind::Property,
            std::path::Path::new(r"instances\ServerStorage.renium"),
            "ServerStorage/Maps/A: property Value".to_string(),
        );
        assert!(record.covered_by_paths(&store));
        assert!(!record.covered_by_paths(&script));
        let sentence = Conflict::legacy(store_conflict.to_string());
        assert!(sentence.covered_by_paths(&store));
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    fn setup(
        decisions: &[(&str, ConflictSide)],
        resolution: Option<ConflictPreference>,
    ) -> PairSetup {
        PairSetup {
            key: "pair".into(),
            identity: PairIdentity {
                experience: String::new(),
                project: String::new(),
                fingerprint: String::new(),
                game_id: None,
                place_id: None,
                local_file: None,
            },
            mode: PairMode::Reconcile,
            resolution_preference: resolution,
            decisions: decisions
                .iter()
                .map(|(id, side)| ((*id).to_string(), *side))
                .collect(),
            resolution_required: false,
            error: None,
            conflicts: Vec::new(),
            differences: Vec::new(),
            requires_reconcile: true,
            runtime_id: Some("runtime".into()),
            local_file_stamp: None,
            local_file_digest: None,
            bootstrap_studio_from_editor: false,
            runtime_replacement_unproven: false,
            unsynced_paths: Vec::new(),
            moot_decisions: Vec::new(),
        }
    }

    fn record(conflicts: Vec<Conflict>, checkpoint: Option<StudioCheckpoint>) -> PairRecord {
        PairRecord {
            version: RECORD_VERSION,
            identity: setup(&[], None).identity,
            mode: PairMode::Reconcile,
            conflict_preference: ConflictPreference::Editor,
            runtime_settings: Map::new(),
            baseline: None,
            head: None,
            conflicts,
            resolution_required: false,
            last_runtime_id: None,
            local_file_stamp: None,
            local_file_digest: None,
            studio_checkpoint: checkpoint,
            differences: Vec::new(),
        }
    }

    #[test]
    fn a_comparison_takes_the_decisions_by_id_and_the_run_s_preference() {
        let decided = comparison_decisions(
            &setup(&[("0a1b2c3d", ConflictSide::Studio)], None),
            &record(Vec::new(), None),
        );
        assert_eq!(decided.preference, ConflictPreference::Editor);
        assert_eq!(
            decided.by_id.into_iter().collect::<Vec<_>>(),
            [("0a1b2c3d".to_string(), ConflictSide::Studio)]
        );
        let preferred = comparison_decisions(
            &setup(&[], Some(ConflictPreference::Studio)),
            &record(Vec::new(), None),
        );
        assert_eq!(preferred.preference, ConflictPreference::Studio);
    }

    #[test]
    fn a_decision_with_nothing_open_is_refused_before_and_after_a_comparison() {
        let decisions = setup(&[("0a1b2c3d", ConflictSide::Studio)], None).decisions;
        let open = vec![Conflict::legacy("src/A.luau changed on both sides".into())];
        let error = refuse_decisions_with_nothing_open(&decisions, &record(Vec::new(), None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("0a1b2c3d"), "{error}");
        refuse_decisions_with_nothing_open(&decisions, &record(open, None)).unwrap();
        refuse_decisions_with_nothing_open(&BTreeMap::new(), &record(Vec::new(), None)).unwrap();
        let error = refuse_unmatched_decisions(&decisions)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("0a1b2c3d") && error.ends_with("nothing was applied"),
            "{error}"
        );
        refuse_unmatched_decisions(&BTreeMap::new()).unwrap();
    }

    #[test]
    fn held_conflicts_carry_the_evidence_of_the_disk_and_the_change_tracker() {
        let root = crate::tests::support::temp_dir("held-conflicts");
        fs::create_dir_all(root.join("src/ReplicatedStorage")).unwrap();
        fs::write(root.join("src/ReplicatedStorage/A.luau"), b"return 1").unwrap();
        let context = BoundContext {
            id: 1,
            initialized: true,
            project: root
                .join("renium.project.jsonc")
                .to_string_lossy()
                .into_owned(),
            root: root.to_string_lossy().into_owned(),
            experience: String::new(),
            source: root.join("src").to_string_lossy().into_owned(),
            resource_lease: None,
            place_id: None,
            game_id: None,
            selector: String::new(),
            runtime_id: Some("runtime".into()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let services = sync_services();
        let generations = |changed: &str| {
            services
                .iter()
                .map(|service| {
                    (
                        service.clone(),
                        json!(if service == changed { 2 } else { 1 }),
                    )
                })
                .collect::<Map<_, _>>()
        };
        let checkpoint = StudioCheckpoint {
            runtime_id: "runtime".into(),
            change_tracker_version: 1,
            seq: 1,
            service_generations: services
                .iter()
                .map(|service| (service.clone(), 1))
                .collect(),
        };
        let state = json!({
            "tracking": true,
            "trackedServices": services.len(),
            "runtimeId": "runtime",
            "changeTrackerVersion": 1,
            "seq": 2,
            "dirtyServices": [],
            "fullSyncServices": [],
            "checkpointGenerations": generations("ReplicatedStorage"),
        });
        let script = Conflict::new(
            ConflictKind::Script,
            Path::new("src/ReplicatedStorage/A.luau"),
            "src/ReplicatedStorage/A.luau changed on both sides".into(),
        );
        let store = |service: &str| {
            Conflict::new(
                ConflictKind::Property,
                &Path::new("src").join(service).join("store.renium"),
                format!("{service}/Door: property Transparency"),
            )
        };
        let mut setup = setup(&[], None);
        let mut record = record(Vec::new(), Some(checkpoint));
        hold_for_conflicts(
            &context,
            &mut setup,
            &mut record,
            &state,
            vec![script, store("ReplicatedStorage"), store("Workspace")],
        )
        .unwrap();
        let written = load_record(&context, "pair").unwrap().unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(setup.mode, PairMode::Verify);
        assert!(
            setup
                .error
                .as_deref()
                .unwrap()
                .starts_with("Sync needs review: ")
        );
        assert_eq!(written.conflicts.len(), 3);
        let [script, replicated, workspace] = written.conflicts.as_slice() else {
            unreachable!()
        };
        let stamp = script.files_changed_at.as_deref().unwrap();
        assert!(stamp.len() == 20 && stamp.ends_with('Z'), "{stamp}");
        assert_eq!(script.studio_changed, None, "a script file is not a store");
        assert_eq!(replicated.studio_changed, Some(true));
        assert_eq!(workspace.studio_changed, Some(false));
        assert_eq!(replicated.files_changed_at, None, "no store file on disk");
    }

    const STORE: &str = "instances/Workspace.renium";

    /// The last sync, the files and Studio of a Door whose Transparency was
    /// 0, the files set to 0.5 and Studio to `studio`.
    fn door(studio: f64) -> [ProjectSnapshot; 3] {
        [0.0, 0.5, studio].map(|transparency| {
            let root = SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            );
            let mut door =
                SettingsBytecodeInstance::new("door".into(), "Door".into(), "Part".into(), Some(0));
            door.properties
                .insert("Transparency".into(), json!(transparency));
            let bytes = encode_settings_bytecode(&SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![root, door],
            })
            .unwrap();
            ProjectSnapshot {
                entries: BTreeMap::from([(PathBuf::from(STORE), SnapshotEntry::File(bytes))]),
            }
        })
    }

    fn undecided_record(conflicts: Vec<Conflict>) -> PairRecord {
        let mut record = record(conflicts, None);
        record.conflict_preference = ConflictPreference::None;
        record
    }

    fn compare(
        setup: &PairSetup,
        record: &PairRecord,
        [base, files, studio]: &[ProjectSnapshot; 3],
    ) -> Comparison {
        let differences = snapshot_differences(files, studio).unwrap();
        compare_sides(setup, record, [files, studio], &differences, || {
            Ok(Some(base.clone()))
        })
        .unwrap()
    }

    fn transparency(merged: &ProjectSnapshot) -> Value {
        settings_document(merged.entries.get(Path::new(STORE)))
            .unwrap()
            .instances[1]
            .properties["Transparency"]
            .clone()
    }

    #[test]
    fn a_comparison_takes_each_decision_by_its_id() {
        let sides = door(0.2);
        let record = undecided_record(Vec::new());
        let Comparison::Merged { conflicts, .. } = compare(&setup(&[], None), &record, &sides)
        else {
            panic!("the sides differ");
        };
        let [conflict] = conflicts.as_slice() else {
            panic!("{conflicts:#?}");
        };
        for (side, value) in [(ConflictSide::Studio, 0.2), (ConflictSide::Editor, 0.5)] {
            let Comparison::Merged {
                merged, conflicts, ..
            } = compare(&setup(&[(&conflict.id, side)], None), &record, &sides)
            else {
                panic!("{side:?} was refused");
            };
            assert!(conflicts.is_empty(), "{conflicts:#?}");
            assert_eq!(transparency(&merged), json!(value), "{side:?}");
        }
    }

    #[test]
    fn decisions_are_refused_where_the_sides_match_or_studio_is_rebuilt() {
        let [base, files, _] = door(0.2);
        let same = [base, files.clone(), files];
        let record = undecided_record(Vec::new());
        let decided = setup(&[("0a1b2c3d", ConflictSide::Studio)], None);
        assert!(matches!(
            compare(&setup(&[], None), &record, &same),
            Comparison::Same
        ));
        let Comparison::Refused { message, open } = compare(&decided, &record, &same) else {
            panic!("a decision with nothing to decide was taken");
        };
        assert!(
            message.contains("0a1b2c3d") && message.ends_with("nothing was applied"),
            "{message}"
        );
        assert!(open.is_empty());

        let sides = door(0.2);
        let mut rebuilt = setup(&[], None);
        rebuilt.bootstrap_studio_from_editor = true;
        let Comparison::Merged { changes, .. } = compare(&rebuilt, &record, &sides) else {
            panic!("rebuilding Studio from the files was refused");
        };
        assert!(changes.studio.contains(Path::new(STORE)));
        let mut rebuilt = decided;
        rebuilt.bootstrap_studio_from_editor = true;
        let Comparison::Refused { message, .. } = compare(&rebuilt, &record, &sides) else {
            panic!("a decision was taken while Studio is rebuilt from the files");
        };
        assert!(message.contains("0a1b2c3d"), "{message}");
    }

    #[test]
    fn a_decision_for_a_conflict_that_changed_names_it_and_its_new_record() {
        let record = undecided_record(Vec::new());
        let Comparison::Merged { conflicts, .. } = compare(&setup(&[], None), &record, &door(0.2))
        else {
            panic!("the sides differ");
        };
        let listed = conflicts[0].id.clone();
        let record = undecided_record(conflicts);
        let Comparison::Refused { message, open } = compare(
            &setup(
                &[
                    (&listed, ConflictSide::Studio),
                    ("0a1b2c3d", ConflictSide::Editor),
                ],
                None,
            ),
            &record,
            &door(0.3),
        ) else {
            panic!("a decision for values that changed was taken");
        };
        let [now] = open.as_slice() else {
            panic!("{open:#?}");
        };
        assert_ne!(now.id, listed);
        assert_eq!(now.studio, Some(json!(0.3)));
        assert!(
            message.contains(&format!(
                "Conflict {listed} changed since it was listed; it is {} now: Workspace/Door: property Transparency: Studio has 0.3",
                now.id
            )) && message.contains("No open conflict has the id 0a1b2c3d")
                && message.ends_with("nothing was applied"),
            "{message}"
        );
    }

    #[test]
    fn a_decision_with_nothing_open_is_refused_before_a_session_is_touched() {
        let root = crate::tests::support::temp_dir("prepare-decisions");
        let context = BoundContext {
            id: 1,
            initialized: true,
            project: root
                .join("renium.project.jsonc")
                .to_string_lossy()
                .into_owned(),
            root: root.to_string_lossy().into_owned(),
            experience: String::new(),
            source: root.join("src").to_string_lossy().into_owned(),
            resource_lease: None,
            place_id: Some(2),
            game_id: Some(1),
            selector: String::new(),
            runtime_id: Some("runtime".into()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let identity = PairIdentity {
            game_id: Some(1),
            place_id: Some(2),
            ..setup(&[], None).identity
        };
        let configuration = |decisions: &[(&str, ConflictSide)]| PairConfiguration {
            mode: PairMode::Reconcile,
            conflict_preference: ConflictPreference::None,
            resolution_preference: None,
            decisions: setup(decisions, None).decisions,
            runtime_settings: Map::new(),
        };
        let coordinator = Coordinator::default();
        let refused = coordinator
            .prepare_pair(
                &context,
                identity.clone(),
                configuration(&[("0a1b2c3d", ConflictSide::Studio)]),
            )
            .map(|_| ());
        let prepared = coordinator.prepare_pair(&context, identity, configuration(&[]));
        fs::remove_dir_all(&root).unwrap();
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("0a1b2c3d"), "{error}");
        assert!(prepared.unwrap().decisions.is_empty());
    }
}
