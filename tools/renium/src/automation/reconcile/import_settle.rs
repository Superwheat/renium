//! A committed import keeps its request thread, gate and tracking guard until
//! its accepted state is published. Only the response and request lease end
//! early. The journal precedes mutation and survives process termination.
use super::*;
use std::cell::RefCell;
use std::sync::{Condvar, LazyLock};
use std::thread::{self, ThreadId};

use crate::automation::op;

#[derive(Serialize, Deserialize)]
struct Record {
    version: u8,
    runtime: Option<String>,
    services: Vec<String>,
    paths: Vec<PathBuf>,
    verification_paths: HashSet<PathBuf>,
    before: StoredSnapshot,
    expected: StoredSnapshot,
    accepted: Option<StoredSnapshot>,
    input: Option<Value>,
    summary: Map<String, Value>,
    error: Option<String>,
}

struct Journal {
    directory: PathBuf,
    record: Record,
}

struct Running {
    directory: PathBuf,
    runtime: Option<String>,
    owner: ThreadId,
}

static RUNNING: LazyLock<(Mutex<Vec<Running>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(Vec::new()), Condvar::new()));

thread_local! {
    static CURRENT: RefCell<Option<Journal>> = const { RefCell::new(None) };
}

fn directory(context: &BoundContext) -> PathBuf {
    let key = crate::system::files::exact_path_key(Path::new(&context.source));
    Path::new(&context.root)
        .join(".renium/import-settlement")
        .join(format!("{:016x}", fnv1a(key.as_bytes())))
}

impl Journal {
    fn save(&self) -> Result<()> {
        atomic_write_file(
            &self.directory.join("pending.rmp"),
            &rmp_serde::to_vec(&self.record)?,
        )
    }

    fn load(context: &BoundContext) -> Result<Option<Self>> {
        let directory = directory(context);
        let bytes = match fs::read(directory.join("pending.rmp")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let record: Record = rmp_serde::from_slice(&bytes)?;
        anyhow::ensure!(
            record.version == 1,
            "Unsupported import settlement journal; run a full pull to recover"
        );
        Ok(Some(Self { directory, record }))
    }

    fn install(self) -> Result<()> {
        anyhow::ensure!(
            CURRENT.with(|slot| slot.borrow().is_none()),
            "An import settlement is already active on this request"
        );
        RUNNING.0.lock_recover().push(Running {
            directory: self.directory.clone(),
            runtime: self.record.runtime.clone(),
            owner: thread::current().id(),
        });
        CURRENT.with(|slot| *slot.borrow_mut() = Some(self));
        Ok(())
    }
}

pub(super) struct ImportStart<'a> {
    pub services: &'a [String],
    pub paths: &'a [PathBuf],
    pub verification_paths: &'a HashSet<PathBuf>,
    pub before: &'a ProjectSnapshot,
    pub expected: &'a ProjectSnapshot,
    pub input: Option<&'a Value>,
    pub history_id: String,
}

pub(super) fn can_continue(
    stage: &ExportProjectStage,
    requires_stage: bool,
    has_links: bool,
) -> bool {
    !requires_stage
        && !has_links
        && crate::daemon::can_respond_before_settle()
        && stage
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.project.adapters.is_empty())
}

pub(super) fn begin(context: &BoundContext, start: ImportStart<'_>) -> Result<()> {
    let directory = directory(context);
    anyhow::ensure!(
        !directory.join("pending.rmp").exists(),
        "An earlier import still needs settlement; run rbx pl"
    );
    let packs = directory.join("packs");
    let record = Record {
        version: 1,
        runtime: context.runtime_id.clone(),
        services: start.services.to_vec(),
        paths: start.paths.to_vec(),
        verification_paths: start.verification_paths.clone(),
        before: StoredSnapshot::write_at(&packs, start.before.entries.iter())?,
        expected: StoredSnapshot::write_at(&packs, start.expected.entries.iter())?,
        accepted: None,
        input: start.input.cloned(),
        summary: Map::from_iter([("historyId".into(), json!(start.history_id))]),
        error: None,
    };
    let journal = Journal { directory, record };
    journal.save()?;
    journal.install()
}

pub(crate) fn active() -> bool {
    CURRENT.with(|slot| slot.borrow().is_some())
}

pub(crate) fn pending(context: &BoundContext) -> bool {
    directory(context).join("pending.rmp").exists()
}

pub(crate) struct RequestCleanup;

impl Drop for RequestCleanup {
    fn drop(&mut self) {
        // On ordinary returns finish_request already took the journal. On a
        // panic, release waiters and retain an explicit failure before unwind.
        if let Err(error) = finish_request(Some(
            "The daemon request ended before import settlement completed",
        )) {
            log_global(
                3,
                format_args!("[renium] could not record import settlement failure: {error:#}"),
            );
        }
    }
}

pub(super) fn committed(summary: &Map<String, Value>, expected: &ProjectSnapshot) -> Result<()> {
    CURRENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let journal = slot.as_mut().context("Missing import settlement journal")?;
        journal.record.summary.extend(summary.clone());
        if !journal.record.expected.matches(expected) {
            journal.record.expected = StoredSnapshot::write_at(
                &journal.directory.join("packs"),
                expected.entries.iter(),
            )?;
        }
        journal.save()
    })
}

fn accepted(snapshot: &ProjectSnapshot) -> Result<()> {
    CURRENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let journal = slot.as_mut().context("Missing import settlement journal")?;
        journal.record.accepted = Some(StoredSnapshot::write_at(
            &journal.directory.join("packs"),
            snapshot.entries.iter(),
        )?);
        journal.save()
    })
}

/// Called before the gate is released, after Live Sync's acknowledgment too.
/// A failed continuation never disappears with its already-answered client.
pub(crate) fn finish_request(error: Option<&str>) -> Result<()> {
    let Some(mut journal) = CURRENT.with(|slot| slot.borrow_mut().take()) else {
        return Ok(());
    };
    let result = if let Some(error) = error {
        journal.record.error = Some(error.to_string());
        log_global(
            3,
            format_args!("[renium] import settlement failed: {error}"),
        );
        journal.save()
    } else {
        fs::remove_file(journal.directory.join("pending.rmp")).map_err(anyhow::Error::from)
    };
    if result.is_ok() && error.is_none() {
        let _ = fs::remove_dir_all(&journal.directory);
    }
    RUNNING
        .0
        .lock_recover()
        .retain(|entry| entry.owner != thread::current().id());
    RUNNING.1.notify_all();
    result
}

pub(crate) fn wait(context: &BoundContext, bridge: &BridgeServer) -> Result<()> {
    wait_for_request(context, bridge.active_request_lease().as_deref())
}

pub(crate) fn wait_for_request(
    context: &BoundContext,
    lease: Option<&crate::studio::bridge::BridgeRequestLease>,
) -> Result<()> {
    let directory = directory(context);
    let mut running = RUNNING.0.lock_recover();
    while running.iter().any(|entry| {
        entry.owner != thread::current().id()
            && (entry.directory == directory
                || entry.runtime.is_some() && entry.runtime == context.runtime_id)
    }) {
        if let Some(lease) = lease {
            lease.ensure_active()?;
        }
        running = RUNNING
            .1
            .wait_timeout(running, Duration::from_millis(100))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    Ok(())
}

pub(super) struct ImportWork {
    pub before: ProjectSnapshot,
    pub expected: ProjectSnapshot,
    pub paths: Vec<PathBuf>,
    pub verification_paths: HashSet<PathBuf>,
    pub prepared: HashMap<PathBuf, PreparedPushVerification>,
    pub history: history::SyncHistory,
    pub summary: Map<String, Value>,
}

/// The same capture, verification and publication serve a live continuation
/// and recovery after a daemon restart. Recovery never invokes the importer.
pub(super) fn settle(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    mut work: ImportWork,
    proof: &Value,
) -> Result<(ProjectSnapshot, Map<String, Value>)> {
    let root = Path::new(&context.root);
    let source = bound_context::source_dir(context)?;
    ensure_original_or_accepted_files(root, &work.paths)?;
    let phase = Instant::now();
    let mut stage = ExportProjectStage::create(root, &source, services)?;
    let mut readback = capture_studio_services_in_memory(context, bridge, services, &stage, true)?;
    log_reconcile_timing("full push readback", phase);
    let phase = Instant::now();
    let (mismatches, details) = verify_and_settle_native_import(
        &work.before,
        &work.expected,
        &mut readback,
        &work.verification_paths,
        &mut work.prepared,
    )?;
    log_reconcile_timing("full push verification", phase);
    if !mismatches.is_empty() {
        return Err(retention_failure(
            "pushed project changes",
            &mismatches,
            details.as_deref(),
        ));
    }
    let phase = Instant::now();
    super::push_flow::stage_verified_import_readback(&mut stage, &readback)?;
    log_reconcile_timing("full push stage verified readback", phase);
    let stable = || -> Result<()> {
        anyhow::ensure!(
            verified_full_push_proof_matches(context, bridge, proof)?,
            "Studio changed during import settlement; the committed import was not repeated"
        );
        ensure_original_or_accepted_files(root, &work.paths)
    };
    let current = publish_checked(stage, root, &work.paths, stable)?;
    let id = work.history.commit(&current, &ProjectSnapshot::default())?;
    work.summary.insert("historyId".into(), json!(id));
    Ok((current, work.summary))
}

fn ensure_original_or_accepted_files(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let current = capture_snapshot(root, paths)?;
    let files_match = CURRENT.with(|slot| {
        slot.borrow().as_ref().is_some_and(|journal| {
            journal.record.expected.matches(&current)
                || journal.record.accepted.as_ref().is_some_and(|accepted| {
                    journal
                        .record
                        .expected
                        .matches_partial_publication(accepted, &current)
                })
        })
    });
    anyhow::ensure!(
        files_match,
        "Project files changed during import settlement; newer files were preserved"
    );
    Ok(())
}

fn publish_checked(
    stage: ExportProjectStage,
    root: &Path,
    paths: &[PathBuf],
    stable: impl Fn() -> Result<()>,
) -> Result<ProjectSnapshot> {
    let accepted_files = capture_snapshot(&stage.project_root, paths)?;
    accepted(&accepted_files)?;
    let (published, _) = crate::snapshot::export::finish_export_publication(
        Some(stage),
        root,
        false,
        None,
        &stable,
        &stable,
    )?;
    let current = capture_snapshot(root, paths)?;
    history::ensure_published_state(&published, &current)?;
    anyhow::ensure!(
        current == accepted_files,
        "Project files changed while the import was being published; newer files were preserved"
    );
    Ok(current)
}

pub(crate) fn before_request(
    context: &BoundContext,
    bridge: &BridgeServer,
    operation: u16,
    parameters: &Value,
) -> Result<Option<Value>> {
    wait(context, bridge)?;
    // The request that owns the continuation may make nested managed calls.
    if active() {
        return Ok(None);
    }
    let Some(journal) = Journal::load(context)? else {
        return Ok(None);
    };
    if matches!(operation, op::STUDIO_CLOSE | op::STUDIO_OPEN) {
        crate::daemon::emit_request_notice(
            "The project still has an unsettled import. Opening or closing Studio keeps its recovery record; run rbx pl to recover the accepted state.",
        );
        return Ok(None);
    }
    let full_pull = operation == op::PULL && {
        let args = automation_pull_args(context, parameters)?;
        let selected = crate::snapshot::import::parse_services(&args.services)?;
        journal
            .record
            .services
            .iter()
            .all(|service| selected.contains(service))
    };
    if let Some(error) = &journal.record.error {
        if full_pull {
            crate::daemon::emit_request_notice(&format!(
                "The import committed, but settling its files failed: {error}. This pull recovers its accepted state."
            ));
            journal.install()?;
            return Ok(None);
        }
        bail!(
            "The import committed, but settling its files failed: {error}. Run rbx pl to recover; the import will not be repeated"
        );
    }
    if full_pull {
        journal.install()?;
        return Ok(None);
    }
    anyhow::ensure!(
        operation == op::PUSH
            && parameters.get("verify") != Some(&Value::Bool(true))
            && !super::super::runtime::push_is_filtered(parameters),
        "The last import is unsettled after the daemon stopped. Run rbx pl or repeat the whole push to settle it first; the import will not be repeated"
    );
    anyhow::ensure!(
        context.runtime_id == journal.record.runtime,
        "The unsettled import belongs to another Studio session. Run rbx pl to recover its accepted state"
    );
    let packs = journal.directory.join("packs");
    let expected = journal.record.expected.load_at(&packs, |_| true)?;
    let work = ImportWork {
        before: journal.record.before.load_at(&packs, |_| true)?,
        expected,
        paths: journal.record.paths.clone(),
        verification_paths: journal.record.verification_paths.clone(),
        prepared: HashMap::new(),
        history: history::SyncHistory::open(root_history(context, &journal.record.summary)?)?,
        summary: journal.record.summary.clone(),
    };
    let services = journal.record.services.clone();
    let input = journal.record.input.clone();
    let args = automation_push_args(context, parameters, true)?;
    let same_input = input.as_ref() == Some(&full_push_cache_input(&args, &services)?);
    journal.install()?;
    recover(context, bridge, &services, work, input)?;
    if same_input {
        Ok(Some(json!({"ok": true, "unchanged": true})))
    } else {
        finish_request(None)?;
        Ok(None)
    }
}

fn root_history(context: &BoundContext, summary: &Map<String, Value>) -> Result<PathBuf> {
    let id = summary.get("historyId").and_then(Value::as_str).context("The daemon stopped before the import commit was recorded. Run rbx pl to recover; no import will be repeated")?;
    anyhow::ensure!(
        Path::new(id).components().count() == 1
            && matches!(
                Path::new(id).components().next(),
                Some(Component::Normal(_))
            ),
        "Invalid import history id"
    );
    Ok(Path::new(&context.root)
        .join(".renium/editor-history/sync")
        .join(id))
}

fn recover(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    work: ImportWork,
    input: Option<Value>,
) -> Result<()> {
    #[cfg(windows)]
    let mut attributes = None;
    let (guard, _) =
        current_studio_change_guard_verifying(context, bridge, Some(services), None, |state| {
            #[cfg(windows)]
            super::push_flow::arm_native_attribute_relay(
                state,
                bridge,
                services,
                &mut None,
                &mut attributes,
            )?;
            #[cfg(not(windows))]
            let _ = state;
            Ok(())
        })?;
    let mut release = StudioTrackingGuardRelease {
        bridge,
        runtime_id: guard.runtime_id.clone(),
        guard_id: guard.tracking_guard_id.clone(),
        proof: None,
        retained_proof: None,
        retained_proof_local: false,
    };
    let proof = capture_push_proof(bridge, &guard)?
        .context("Studio cannot prove a stable recovery capture; run rbx pl")?;
    let paths = work.paths.clone();
    let (current, _) = settle(context, bridge, services, work, &proof)?;
    release.proof = Some(proof);
    acknowledge_verified_push(bridge, services, &guard, &mut release)?;
    retain_verified_full_push(
        bridge,
        input.as_ref(),
        &paths,
        current,
        &release,
        #[cfg(windows)]
        &mut attributes,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(root: &Path) -> BoundContext {
        BoundContext {
            id: 1,
            initialized: true,
            project: root.join("renium.project.jsonc").display().to_string(),
            root: root.display().to_string(),
            experience: String::new(),
            source: root.join("src").display().to_string(),
            resource_lease: None,
            place_id: None,
            game_id: None,
            selector: String::new(),
            runtime_id: Some(root.display().to_string()),
            plugin_build: None,
            fingerprint: String::new(),
        }
    }

    fn begin_test(context: &BoundContext) -> Result<ProjectSnapshot> {
        let root = Path::new(&context.root);
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/A.luau"), "return 1")?;
        let expected = capture_snapshot(root, &[PathBuf::from("src")])?;
        begin(
            context,
            ImportStart {
                services: &["Workspace".into()],
                paths: &["src".into()],
                verification_paths: &HashSet::from(["src/A.luau".into()]),
                before: &ProjectSnapshot::default(),
                expected: &expected,
                input: None,
                history_id: "test-history".into(),
            },
        )?;
        Ok(expected)
    }

    #[test]
    fn success_clears_the_journal_only_when_the_whole_request_finishes() -> Result<()> {
        let root = crate::tests::support::temp_dir("import-settle-success");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test(&context)?;
        committed(
            &Map::from_iter([("verifiedPushProof".into(), json!(3))]),
            &expected,
        )?;
        accepted(&expected)?;
        let journal = Journal::load(&context)?.unwrap();
        assert!(journal.record.expected.matches(&expected));
        assert_eq!(journal.record.summary["historyId"], "test-history");
        assert!(journal.record.accepted.unwrap().matches(&expected));
        assert!(journal.record.error.is_none());
        finish_request(None)?;
        assert!(Journal::load(&context)?.is_none());
        Ok(())
    }

    #[test]
    fn interrupted_publication_accepts_only_journaled_file_versions() -> Result<()> {
        let root = crate::tests::support::temp_dir("import-partial-publication");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test(&context)?;
        let mut published = expected.clone();
        published.entries.insert(
            "src/A.luau".into(),
            SnapshotEntry::File(b"accepted".to_vec()),
        );
        published
            .entries
            .insert("src/B.luau".into(), SnapshotEntry::File(b"new".to_vec()));
        accepted(&published)?;
        fs::write(root.join("src/A.luau"), "accepted")?;
        ensure_original_or_accepted_files(&root, &["src".into()])?;
        fs::write(root.join("src/B.luau"), "unexpected edit")?;
        assert!(ensure_original_or_accepted_files(&root, &["src".into()]).is_err());
        finish_request(Some("newer file edit"))?;
        Ok(())
    }

    #[test]
    fn an_edit_before_the_stage_baseline_cannot_become_the_publication_baseline() -> Result<()> {
        let root = crate::tests::support::temp_dir("import-settle-baseline-race");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test(&context)?;
        ensure_original_or_accepted_files(&root, &["src".into()])?;
        fs::write(root.join("src/A.luau"), "newer file edit")?;
        // A stage made after this edit would accept it as its own baseline.
        // Publication must also compare against the journal's earlier files.
        accepted(&expected)?;
        assert!(ensure_original_or_accepted_files(&root, &["src".into()]).is_err());
        finish_request(Some("Project files changed during import settlement"))?;
        assert_eq!(
            fs::read_to_string(root.join("src/A.luau"))?,
            "newer file edit"
        );
        Ok(())
    }

    #[test]
    fn failures_remain_durable_and_never_replace_newer_files() -> Result<()> {
        for reason in [
            "readback mismatch",
            "Studio closed",
            "Studio changed during settlement",
            "files changed",
            "Live Sync acknowledgment failed",
        ] {
            let root = crate::tests::support::temp_dir("import-settle-failure");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let context = context(&root);
            let expected = begin_test(&context)?;
            fs::write(root.join("src/A.luau"), "return 2")?;
            finish_request(Some(reason))?;
            let journal = Journal::load(&context)?.unwrap();
            assert_eq!(journal.record.error.as_deref(), Some(reason));
            assert!(journal.record.expected.matches(&expected));
            assert!(
                !journal
                    .record
                    .expected
                    .matches(&capture_snapshot(&root, &["src".into()])?)
            );
            assert_eq!(fs::read_to_string(root.join("src/A.luau"))?, "return 2");
        }
        Ok(())
    }

    #[test]
    fn interruption_keeps_both_snapshots_and_a_published_candidate() -> Result<()> {
        let root = crate::tests::support::temp_dir("import-settle-interrupt");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test(&context)?;
        let mut published = expected.clone();
        published.entries.insert(
            "src/A.luau".into(),
            SnapshotEntry::File(b"return 3".to_vec()),
        );
        accepted(&published)?;
        // A daemon killed between publication and acknowledgment runs no
        // finalizer. Loading the disk journal must distinguish both states.
        CURRENT.with(|slot| slot.borrow_mut().take());
        RUNNING
            .0
            .lock_recover()
            .retain(|entry| entry.owner != thread::current().id());
        let journal = Journal::load(&context)?.unwrap();
        assert!(journal.record.error.is_none());
        assert!(
            journal
                .record
                .expected
                .load_at(&journal.directory.join("packs"), |_| true)?
                == expected
        );
        assert!(
            journal
                .record
                .accepted
                .as_ref()
                .unwrap()
                .matches(&published)
        );
        journal.install()?;
        finish_request(Some("Recovery interrupted"))?;
        assert!(Journal::load(&context)?.unwrap().record.error.is_some());
        Ok(())
    }

    #[test]
    fn publication_checks_studio_before_and_after_and_preserves_file_edits() -> Result<()> {
        for failure_at in [0, 1, 2, 3] {
            let root = crate::tests::support::temp_dir("import-publication-fence");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let context = context(&root);
            begin_test(&context)?;
            let relative = Path::new("src/Workspace/A.luau");
            fs::create_dir_all(root.join(relative.parent().unwrap()))?;
            fs::write(root.join(relative), "original")?;
            let stage = ExportProjectStage::create(&root, Path::new("src"), &["Workspace".into()])?;
            fs::create_dir_all(stage.project_root.join(relative.parent().unwrap()))?;
            fs::write(stage.project_root.join(relative), "accepted")?;
            let paths = stage.publish_paths().to_vec();
            if failure_at == 3 {
                fs::write(root.join(relative), "newer file edit")?;
            }
            let calls = std::cell::Cell::new(0);
            let stable = || -> Result<()> {
                calls.set(calls.get() + 1);
                anyhow::ensure!(
                    calls.get() != failure_at,
                    "Studio closed or changed at publication boundary"
                );
                Ok(())
            };
            let result = publish_checked(stage, &root, &paths, stable);
            match failure_at {
                0 => {
                    assert!(result.is_ok());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "accepted");
                }
                1 => {
                    assert!(result.is_err());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "original");
                }
                2 => {
                    assert!(result.is_err());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "accepted");
                }
                _ => {
                    assert!(result.is_err());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "newer file edit");
                }
            }
            let error = result.err().map(|error| format!("{error:#}"));
            finish_request(error.as_deref())?;
            assert_eq!(Journal::load(&context)?.is_some(), failure_at != 0);
        }
        Ok(())
    }

    #[test]
    fn unwinding_records_failure_and_releases_waiters() -> Result<()> {
        let root = crate::tests::support::temp_dir("import-settle-unwind");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let outcome = std::panic::catch_unwind(|| {
            let _cleanup = RequestCleanup;
            begin_test(&context).unwrap();
            panic!("simulated continuation panic");
        });
        assert!(outcome.is_err());
        assert!(!active());
        assert!(
            Journal::load(&context)?
                .unwrap()
                .record
                .error
                .unwrap()
                .contains("before import settlement completed")
        );
        assert!(
            !RUNNING
                .0
                .lock_recover()
                .iter()
                .any(|entry| entry.owner == thread::current().id())
        );
        Ok(())
    }

    #[test]
    fn a_later_project_or_studio_request_waits_for_completion() -> Result<()> {
        let root = crate::tests::support::temp_dir("import-settle-wait");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        begin_test(&context)?;
        let (bridge, _) = BridgeServer::listen_with_initial_wait("127.0.0.1", &[0], 0.0, false)?;
        thread::scope(|scope| {
            let (done, received) = std::sync::mpsc::channel();
            let project = &context;
            let mut studio = context.clone();
            studio.source.push_str("/different-project");
            let bridge = &bridge;
            for target in [context.clone(), studio] {
                let done = done.clone();
                scope.spawn(move || {
                    wait(&target, bridge).unwrap();
                    done.send(()).unwrap();
                });
            }
            assert!(received.recv_timeout(Duration::from_millis(50)).is_err());
            let mut unrelated = project.clone();
            unrelated.runtime_id = Some("unrelated-runtime".into());
            unrelated.source.push_str("/unrelated");
            wait(&unrelated, bridge).unwrap();
            finish_request(Some("test completed")).unwrap();
            for _ in 0..2 {
                received.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        });
        Ok(())
    }
}
