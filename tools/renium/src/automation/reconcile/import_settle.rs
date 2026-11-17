//! A committed import keeps its request thread, gate and tracking guard until
//! its accepted state is published. Only the response and request lease end
//! early. Only verified commits are journaled, before the response leaves.
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
    #[serde(default)]
    input: Option<Value>,
    summary: Map<String, Value>,
    error: Option<String>,
    #[serde(default)]
    journal_id: String,
}

struct Journal {
    directory: PathBuf,
    record: Record,
    _ownership: crate::system::project_access::ProjectAccess,
}

struct Running {
    directory: PathBuf,
    runtime: Option<String>,
    owner: ThreadId,
    lease: Option<Arc<crate::studio::bridge::BridgeRequestLease>>,
}

struct RecoveryPull {
    directory: PathBuf,
    _ownership: crate::system::project_access::ProjectAccess,
}

static RUNNING: LazyLock<(Mutex<Vec<Running>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(Vec::new()), Condvar::new()));

thread_local! {
    static CURRENT: RefCell<Option<Journal>> = const { RefCell::new(None) };
    // An explicit full pull can recover even a journal this version cannot
    // decode. Keep its bytes untouched if that pull fails.
    static RECOVERY_PULL: RefCell<Option<RecoveryPull>> = const { RefCell::new(None) };
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
            &rmp_serde::to_vec_named(&self.record)?,
        )
    }

    fn load(context: &BoundContext) -> Result<Option<Self>> {
        let ownership =
            crate::system::project_access::acquire(Path::new(&context.root), || Ok(()))?;
        let directory = directory(context);
        let bytes = match fs::read(directory.join("pending.rmp")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if resolved(&directory, &bytes) {
            return Ok(None);
        }
        // Read the version before interpreting fields. Named fields allow
        // compatible additions; arrays are the original version-one format.
        let header: Value = rmp_serde::from_slice(&bytes)
            .context("Cannot read the import settlement journal; run rbx pl to recover")?;
        let version = header
            .get("version")
            .or_else(|| header.get(0))
            .and_then(Value::as_u64);
        anyhow::ensure!(
            version == Some(1),
            "Unsupported import settlement journal; run a full pull to recover"
        );
        let record: Record = rmp_serde::from_slice(&bytes)
            .context("Cannot decode the import settlement journal; run rbx pl to recover")?;
        anyhow::ensure!(
            record.paths.iter().all(|path| path
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))),
            "The import settlement journal names a path outside the project; run rbx pl to recover"
        );
        Ok(Some(Self {
            directory,
            record,
            _ownership: ownership,
        }))
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
            lease: None,
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
    let ownership = crate::system::project_access::acquire(Path::new(&context.root), || Ok(()))?;
    let directory = directory(context);
    anyhow::ensure!(
        !pending(context),
        "An earlier import still needs settlement; run rbx pl"
    );
    let packs = directory.join("packs");
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("Cannot identify import journal: {error}"))?;
    let record = Record {
        version: 1,
        journal_id: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
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
    let journal = Journal {
        directory,
        record,
        _ownership: ownership,
    };
    journal.save()?;
    journal.install()
}

pub(crate) fn active() -> bool {
    CURRENT.with(|slot| slot.borrow().is_some())
        || RECOVERY_PULL.with(|slot| slot.borrow().is_some())
}

pub(crate) fn pending(context: &BoundContext) -> bool {
    let directory = directory(context);
    match fs::read(directory.join("pending.rmp")) {
        Ok(bytes) => !resolved(&directory, &bytes),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

fn resolved(directory: &Path, pending: &[u8]) -> bool {
    fs::read(directory.join("resolved.sha256"))
        .is_ok_and(|digest| digest.as_slice() == Sha256::digest(pending).as_slice())
}

/// A completed journal stays completed even when Windows temporarily refuses
/// deletion. The marker names these exact bytes, never a later import.
fn resolve_journal(directory: &Path) -> Result<()> {
    let bytes = fs::read(directory.join("pending.rmp"))?;
    atomic_write_file(&directory.join("resolved.sha256"), &Sha256::digest(&bytes))?;
    if fs::remove_file(directory.join("pending.rmp")).is_ok() {
        let _ = fs::remove_dir_all(directory);
    }
    Ok(())
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
    if let Some(recovery) = RECOVERY_PULL.with(|slot| slot.borrow_mut().take()) {
        if error.is_none() {
            resolve_journal(&recovery.directory)?;
        }
        return Ok(());
    }
    let Some(mut journal) = CURRENT.with(|slot| slot.borrow_mut().take()) else {
        return Ok(());
    };
    let has_commit = journal
        .record
        .summary
        .get("verifiedPushProof")
        .is_some_and(|value| !value.is_null());
    let result = if let Some(error) = error.filter(|_| has_commit) {
        journal
            .record
            .error
            .get_or_insert_with(|| error.to_string());
        log_global(
            3,
            format_args!("[renium] import settlement failed: {error}"),
        );
        journal.save()
    } else {
        resolve_journal(&journal.directory)
    };
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

pub(crate) fn continue_request(context: &BoundContext, bridge: &BridgeServer) -> Result<()> {
    bridge.continue_request_lease()?;
    let mut running = RUNNING.0.lock_recover();
    let entry = running
        .iter_mut()
        .find(|entry| {
            entry.owner == thread::current().id() && entry.directory == directory(context)
        })
        .context("Missing running import settlement")?;
    entry.lease = bridge.active_request_lease();
    Ok(())
}

/// How long a close that keeps the place lets a running settlement publish
/// before it cancels it. Tests wait it out, so it is short there.
pub(crate) const CLOSE_WAITS_FOR_SETTLEMENT: Duration =
    Duration::from_secs(if cfg!(test) { 2 } else { 30 });

/// A close that discards the place cancels the read-only continuation of its
/// Studio at once. Any other close lets the settlement publish first and
/// cancels it only when that takes too long: a hung Studio must not keep its
/// own close waiting. A client disconnect alone never cancels the committed
/// tail, and closing another Studio of the project leaves it alone.
pub(crate) fn yield_to_close(
    context: &BoundContext,
    discards_place: bool,
    lease: Option<&crate::studio::bridge::BridgeRequestLease>,
) -> Result<()> {
    if !discards_place {
        wait_while_running(
            context,
            lease,
            Some(Instant::now() + CLOSE_WAITS_FOR_SETTLEMENT),
        )?;
    }
    let directory = directory(context);
    for entry in RUNNING.0.lock_recover().iter() {
        let of_this_studio = match &context.runtime_id {
            Some(runtime) => entry.runtime.as_ref() == Some(runtime),
            None => entry.directory == directory,
        };
        if of_this_studio && let Some(lease) = &entry.lease {
            lease.cancel();
        }
    }
    Ok(())
}

pub(crate) fn wait_for_request(
    context: &BoundContext,
    lease: Option<&crate::studio::bridge::BridgeRequestLease>,
) -> Result<()> {
    wait_while_running(context, lease, None)
}

/// Waits for the settlements of this project or Studio that other requests
/// run, to `deadline` at the latest.
fn wait_while_running(
    context: &BoundContext,
    lease: Option<&crate::studio::bridge::BridgeRequestLease>,
    deadline: Option<Instant>,
) -> Result<()> {
    let directory = directory(context);
    let mut running = RUNNING.0.lock_recover();
    while running.iter().any(|entry| {
        entry.owner != thread::current().id()
            && (entry.directory == directory
                || entry.runtime.is_some() && entry.runtime == context.runtime_id)
    }) && deadline.is_none_or(|deadline| Instant::now() < deadline)
    {
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
) -> Result<(Option<ProjectSnapshot>, Map<String, Value>)> {
    let root = Path::new(&context.root);
    let source = bound_context::source_dir(context)?;
    if let Err(error) = ensure_original_or_accepted_files(root, &work.paths) {
        if !error.is::<NewerProjectFiles>() {
            return Err(error);
        }
        // The caller can start editing as soon as its verified reply arrives.
        // Leave those files alone; the next push reconciles the newer work.
        return Ok((None, work.summary));
    }
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
    let current = match publish_checked(stage, root, &work.paths, stable) {
        Ok(current) => current,
        Err(error) if error.is::<NewerProjectFiles>() => return Ok((None, work.summary)),
        Err(error) => return Err(error),
    };
    let id = work.history.commit(&current, &ProjectSnapshot::default())?;
    work.summary.insert("historyId".into(), json!(id));
    Ok((Some(current), work.summary))
}

#[derive(Debug)]
struct NewerProjectFiles;

impl std::fmt::Display for NewerProjectFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Project files changed during import settlement; newer files were preserved")
    }
}

impl std::error::Error for NewerProjectFiles {}

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
    if !files_match {
        return Err(NewerProjectFiles.into());
    }
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
    if current != accepted_files {
        return Err(NewerProjectFiles.into());
    }
    history::ensure_published_state(&published, &current)?;
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
    // Without a journal there is nothing to recover and no lock to take.
    if !pending(context) {
        return Ok(None);
    }
    // Another daemon that settles this project holds the lock until it is done.
    let ownership = crate::system::project_access::acquire(Path::new(&context.root), || {
        bridge
            .active_request_lease()
            .map_or(Ok(()), |lease| lease.ensure_active())
    })?;
    let full_pull = operation == op::PULL && {
        let args = automation_pull_args(context, parameters)?;
        let selected = crate::snapshot::import::parse_services(&args.services)?;
        DEFAULT_SYNC_SERVICES
            .iter()
            .all(|service| selected.iter().any(|name| name == service))
    };
    // Do not decode an old/new/corrupt record before accepting the very
    // recovery command named in its error. Failed pulls leave its bytes intact.
    if full_pull && pending(context) {
        crate::daemon::emit_request_notice(
            "This full pull recovers the unfinished import from Studio. Project files are replaced only by the requested pull.",
        );
        RECOVERY_PULL.with(|slot| {
            *slot.borrow_mut() = Some(RecoveryPull {
                directory: directory(context),
                _ownership: ownership,
            })
        });
        return Ok(None);
    }
    let full_push = operation == op::PUSH
        && parameters.get("verify") != Some(&Value::Bool(true))
        && !super::super::runtime::push_is_filtered(parameters);
    let journal = match Journal::load(context) {
        Ok(journal) => journal,
        Err(error) if full_push => {
            crate::daemon::emit_request_notice(&format!(
                "Cannot recover the old import record: {error:#}. This push reconciles the current files with Studio."
            ));
            resolve_journal(&directory(context))?;
            return Ok(None);
        }
        Err(error) => {
            unsettled_notice(operation, &format!("{error:#}"));
            return Ok(None);
        }
    };
    let Some(journal) = journal else {
        return Ok(None);
    };
    // Version one's old writer created records before committing. A proof
    // was persisted only after commit and before answering the caller.
    if journal
        .record
        .summary
        .get("verifiedPushProof")
        .is_none_or(Value::is_null)
    {
        resolve_journal(&journal.directory)?;
        return Ok(None);
    }
    // Only a full push or a full pull settles the import. Every other command
    // runs on the files and the Studio as they are, as it did after a push
    // whose readback failed before pushes answered early.
    if !full_push {
        unsettled_notice(
            operation,
            journal
                .record
                .error
                .as_deref()
                .unwrap_or("the daemon stopped before settlement finished"),
        );
        return Ok(None);
    }
    let current = capture_snapshot(Path::new(&context.root), &journal.record.paths)?;
    let files_match = journal.record.expected.matches(&current)
        || journal.record.accepted.as_ref().is_some_and(|accepted| {
            journal
                .record
                .expected
                .matches_partial_publication(accepted, &current)
        });
    let retry = journal.record.error.is_some()
        || context.runtime_id != journal.record.runtime
        || !files_match;
    if retry {
        crate::daemon::emit_request_notice(&format!(
            "The earlier import was not settled: {}. This push reconciles the current files with Studio and preserves newer file edits.",
            journal
                .record
                .error
                .as_deref()
                .unwrap_or("the files or Studio changed")
        ));
        resolve_journal(&journal.directory)?;
        return Ok(None);
    }
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
    // A daemon restart can end the native observer behind the old proof while
    // Studio still holds the committed import. Recover under a fresh guard and
    // verify the complete accepted capture before publishing; treating a lost
    // observer as a new ordinary push would leave regenerated saved identities
    // unsettled even when that push finds no authored differences.
    let settled = match recover(context, bridge, &services, work, input) {
        Ok(settled) => settled,
        // Studio no longer holds what the earlier push left there, or cannot
        // prove it. Nothing of that push is repeated or reported as failed:
        // this push reconciles the files with Studio as it is now.
        Err(error) => {
            crate::daemon::emit_request_notice(&format!(
                "The earlier import could not be settled: {error}. This push reconciles the current files with Studio."
            ));
            finish_request(None)?;
            return Ok(None);
        }
    };
    if same_input && settled {
        Ok(Some(json!({"ok": true, "unchanged": true})))
    } else {
        finish_request(None)?;
        Ok(None)
    }
}

/// Says, on the commands that sync or end the session, that the files of the
/// last push were not settled and what settles them.
fn unsettled_notice(operation: u16, reason: &str) {
    if matches!(
        operation,
        op::PUSH | op::PULL | op::LIVE_START | op::STUDIO_CLOSE | op::STUDIO_OPEN
    ) {
        crate::daemon::emit_request_notice(&format!(
            "The files of the last push were not settled: {reason}. Repeat rbx ps to reconcile your files, or run rbx pl to take Studio's state."
        ));
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
) -> Result<bool> {
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
    let Some(current) = current else {
        return Ok(false);
    };
    retain_verified_full_push(
        bridge,
        input.as_ref(),
        &paths,
        current,
        &release,
        #[cfg(windows)]
        &mut attributes,
    );
    Ok(true)
}

#[cfg(test)]
pub(crate) fn fixture_running_import(context: &BoundContext) -> Result<()> {
    let snapshot = ProjectSnapshot::default();
    begin(
        context,
        ImportStart {
            services: &["Workspace".into()],
            paths: &[],
            verification_paths: &HashSet::new(),
            before: &snapshot,
            expected: &snapshot,
            input: None,
            history_id: "fixture".into(),
        },
    )?;
    committed(
        &Map::from_iter([("verifiedPushProof".into(), json!({"fixture": true}))]),
        &snapshot,
    )
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
        committed(
            &Map::from_iter([("verifiedPushProof".into(), json!({"test": true}))]),
            &expected,
        )?;
        Ok(expected)
    }

    fn interrupt_test_request() {
        CURRENT.with(|slot| slot.borrow_mut().take());
        RUNNING
            .0
            .lock_recover()
            .retain(|entry| entry.owner != thread::current().id());
        RUNNING.1.notify_all();
    }

    // Finding 1: both a normal pre-commit error and a process death in the
    // original writer's pre-commit window must leave the next push usable.
    #[test]
    fn uncommitted_failures_and_restart_records_allow_the_next_push() -> Result<()> {
        let bridge = crate::studio::bridge::routing_tests::fixture();
        for stopped in [false, true] {
            let root = crate::tests::support::temp_dir("uncommitted-import");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let context = context(&root);
            begin_test(&context)?;
            CURRENT.with(|slot| -> Result<()> {
                let mut slot = slot.borrow_mut();
                let journal = slot.as_mut().unwrap();
                journal.record.summary.remove("verifiedPushProof");
                // Reproduce the old array format, including its premature historyId.
                atomic_write_file(
                    &journal.directory.join("pending.rmp"),
                    &rmp_serde::to_vec(&journal.record)?,
                )
            })?;
            if stopped {
                interrupt_test_request();
            } else {
                finish_request(Some("Studio update was rolled back before commit"))?;
            }
            assert!(before_request(&context, &bridge, op::PUSH, &json!({}))?.is_none());
            assert!(!pending(&context));
            assert_eq!(fs::read_to_string(root.join("src/A.luau"))?, "return 1");
        }
        Ok(())
    }

    // Finding 2: no engine call or pull is required to preserve a source edit
    // made immediately after the caller's reply.
    #[test]
    fn newer_files_skip_settlement_and_leave_the_next_push_available() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-newer-source");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test(&context)?;
        let paths = vec![PathBuf::from("src")];
        let verification_paths = HashSet::from([PathBuf::from("src/A.luau")]);
        let history =
            history::SyncHistory::begin(&root, &root.join("src"), &expected, &verification_paths)?;
        fs::write(root.join("src/A.luau"), "return 2")?;
        let bridge = crate::studio::bridge::routing_tests::fixture();
        let work = ImportWork {
            before: ProjectSnapshot::default(),
            expected,
            paths,
            verification_paths,
            prepared: HashMap::new(),
            history,
            summary: Map::new(),
        };
        let (published, _) = settle(&context, &bridge, &["Workspace".into()], work, &json!({}))?;
        assert!(published.is_none());
        finish_request(None)?;
        assert!(before_request(&context, &bridge, op::PUSH, &json!({}))?.is_none());
        assert_eq!(fs::read_to_string(root.join("src/A.luau"))?, "return 2");
        Ok(())
    }

    #[test]
    fn closed_or_changed_studio_and_real_mismatches_allow_repush() -> Result<()> {
        let bridge = crate::studio::bridge::routing_tests::fixture();
        for changed_runtime in [false, true] {
            let root = crate::tests::support::temp_dir("settle-repush");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let mut context = context(&root);
            let expected = begin_test(&context)?;
            let mut observed = expected.clone();
            observed.entries.insert(
                "src/A.luau".into(),
                SnapshotEntry::File(b"return 999".to_vec()),
            );
            let (mismatches, detail) = verify_and_settle_native_import(
                &ProjectSnapshot::default(),
                &expected,
                &mut observed,
                &HashSet::from(["src/A.luau".into()]),
                &mut HashMap::new(),
            )?;
            assert!(!mismatches.is_empty());
            let error = retention_failure("pushed project changes", &mismatches, detail.as_deref());
            finish_request(Some(&format!("{error:#}")))?;
            if changed_runtime {
                context.runtime_id = Some("reopened-studio".into());
            }
            assert!(before_request(&context, &bridge, op::PUSH, &json!({}))?.is_none());
            assert_eq!(fs::read_to_string(root.join("src/A.luau"))?, "return 1");
            assert!(!pending(&context));
        }
        Ok(())
    }

    #[test]
    fn restart_with_a_lost_import_or_newer_files_allows_push_without_pull() -> Result<()> {
        for newer_files in [false, true] {
            let root = crate::tests::support::temp_dir("settle-restart-repush");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let mut context = context(&root);
            begin_test(&context)?;
            interrupt_test_request();
            if newer_files {
                fs::write(root.join("src/A.luau"), "newer source")?;
            } else {
                context.runtime_id = Some("reopened-unsaved-place".into());
            }
            let bridge = crate::studio::bridge::routing_tests::fixture();
            assert!(before_request(&context, &bridge, op::PUSH, &json!({}))?.is_none());
            assert!(!pending(&context));
            assert_eq!(
                fs::read_to_string(root.join("src/A.luau"))?,
                if newer_files {
                    "newer source"
                } else {
                    "return 1"
                }
            );
        }
        Ok(())
    }

    #[test]
    fn restart_with_expired_observer_verifies_and_settles_under_a_fresh_guard() -> Result<()> {
        let fixture = super::super::push_flow_tests::Fixture::new()?;
        let context = &fixture.context;
        let paths = vec![PathBuf::from("src"), PathBuf::from("instances")];
        let verification_paths = fixture.desired.entries.keys().cloned().collect();
        let history = history::SyncHistory::begin(
            Path::new(&context.root),
            Path::new(&context.source),
            &ProjectSnapshot::default(),
            &verification_paths,
        )?;
        begin(
            context,
            ImportStart {
                services: &["ReplicatedStorage".into()],
                paths: &paths,
                verification_paths: &verification_paths,
                before: &ProjectSnapshot::default(),
                expected: &fixture.desired,
                input: None,
                history_id: history.id()?,
            },
        )?;
        committed(
            &Map::from_iter([
                ("historyId".into(), json!(history.id()?)),
                ("verifiedPushProof".into(), json!({"expired":true})),
            ]),
            &fixture.desired,
        )?;
        interrupt_test_request();
        super::super::push_flow_tests::replace_capture(fixture.desired.clone());
        assert!(
            before_request(
                context,
                fixture.bridge.as_ref().unwrap(),
                op::PUSH,
                &json!({})
            )?
            .is_none()
        );
        assert!(!pending(context));
        assert!(
            super::super::push_flow_tests::capture(context).is_none(),
            "recovery must verify its fresh capture, even when the old observer expired"
        );
        Ok(())
    }

    // Findings 3 and 6: recovery does not need a decodable record, and a
    // failed pull cannot replace its original error with its own failure.
    #[test]
    fn full_pull_recovers_newer_or_malformed_journals_and_preserves_failed_attempts() -> Result<()>
    {
        let bridge = crate::studio::bridge::routing_tests::fixture();
        for bytes in [
            rmp_serde::to_vec_named(&json!({"version": 2, "future": "field"}))?,
            vec![0xc1],
        ] {
            let root = crate::tests::support::temp_dir("settle-future-journal");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let context = context(&root);
            begin_test(&context)?;
            interrupt_test_request();
            let file = directory(&context).join("pending.rmp");
            fs::write(&file, &bytes)?;
            assert!(Journal::load(&context).is_err());
            assert!(before_request(&context, &bridge, op::PULL, &json!({}))?.is_none());
            finish_request(Some("Studio closed during the recovery pull"))?;
            assert_eq!(fs::read(&file)?, bytes);
            assert!(before_request(&context, &bridge, op::PULL, &json!({}))?.is_none());
            finish_request(None)?;
            assert!(!pending(&context));
        }
        Ok(())
    }

    #[test]
    fn compatible_journal_fields_and_original_failure_survive_recovery() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-journal-fields");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        begin_test(&context)?;
        finish_request(Some("original verification failure"))?;
        let file = directory(&context).join("pending.rmp");
        let mut map: Value = rmp_serde::from_slice(&fs::read(&file)?)?;
        map.as_object_mut().unwrap().remove("journal_id");
        map["compatibleFutureField"] = json!({"a": 1});
        fs::write(&file, rmp_serde::to_vec_named(&map)?)?;
        assert_eq!(
            Journal::load(&context)?.unwrap().record.error.as_deref(),
            Some("original verification failure")
        );
        let bridge = crate::studio::bridge::routing_tests::fixture();
        before_request(&context, &bridge, op::PULL, &json!({}))?;
        finish_request(Some("second failure while pulling"))?;
        assert_eq!(
            Journal::load(&context)?.unwrap().record.error.as_deref(),
            Some("original verification failure")
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn completed_settlement_stays_complete_when_pending_file_cannot_be_deleted() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let root = crate::tests::support::temp_dir("settle-cleanup-sharing");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        begin_test(&context)?;
        let file = directory(&context).join("pending.rmp");
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 2)
            .open(&file)?;
        finish_request(None)?;
        assert!(file.exists());
        assert!(!pending(&context));
        assert!(Journal::load(&context)?.is_none());
        drop(held);
        // A marker for the prior exact bytes cannot resolve the next import.
        begin_test(&context)?;
        assert!(pending(&context));
        finish_request(None)?;
        Ok(())
    }

    #[test]
    fn a_second_daemon_cannot_recover_a_live_journal_and_process_death_releases_it() -> Result<()> {
        const ENV: &str = "RENIUM_TEST_SETTLE_SECOND_DAEMON";
        if let Some(root) = std::env::var_os(ENV) {
            let root = PathBuf::from(root);
            let context = context(&root);
            begin_test(&context)?;
            fs::write(root.join("owner-ready"), [])?;
            // Parent kills this process: no Rust destructor runs.
            loop {
                thread::park_timeout(Duration::from_secs(1));
            }
        }
        let root = crate::tests::support::temp_dir("settle-two-daemons");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let mut owner = crate::system::tools::hidden_command(std::env::current_exe()?)
            .args(["--exact", "automation::reconcile::import_settle::tests::a_second_daemon_cannot_recover_a_live_journal_and_process_death_releases_it"])
            .env(ENV, &root).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("owner-ready").exists() && Instant::now() < deadline {
            anyhow::ensure!(
                owner.try_wait()?.is_none(),
                "owner process failed before taking its journal"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let context = context(&root);
        let original = fs::read(directory(&context).join("pending.rmp"))?;
        let bridge = crate::studio::bridge::routing_tests::fixture();
        thread::scope(|scope| -> Result<()> {
            let (done, received) = std::sync::mpsc::channel();
            let context = &context;
            let bridge = &bridge;
            scope.spawn(move || {
                let result = before_request(context, bridge, op::PULL, &json!({}));
                let _ = done.send(result.map(|_| ()));
                let _ = finish_request(Some("recovery pull failed"));
            });
            let blocked = received.recv_timeout(Duration::from_millis(150)).is_err();
            let unchanged = fs::read(directory(context).join("pending.rmp"))? == original;
            owner.kill()?;
            owner.wait()?;
            // Always release the child before asserting, including on baseline.
            assert!(blocked, "another daemon recovered a live owner's journal");
            assert!(unchanged);
            received.recv_timeout(Duration::from_secs(5))??;
            Ok(())
        })?;
        assert_eq!(fs::read(directory(&context).join("pending.rmp"))?, original);
        Ok(())
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

    /// A settlement that failed after the push was answered leaves every
    /// command but a full push or pull running as before.
    #[test]
    fn a_settlement_error_does_not_refuse_ordinary_commands() -> Result<()> {
        let root = crate::tests::support::temp_dir("review-settle-error-commands");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        begin_test(&context)?;
        finish_request(Some(
            "Studio changed during import settlement; the committed import was not repeated",
        ))?;
        let bridge = crate::studio::bridge::routing_tests::fixture();
        for operation in [
            op::LUAU,
            op::FIND,
            op::CONSOLE,
            op::PLAY_START,
            op::LIVE_START,
        ] {
            before_request(&context, &bridge, operation, &json!({}))
                .map_err(|error| anyhow::anyhow!("op {operation} refused: {error:#}"))?;
        }
        Ok(())
    }

    /// A journal that names a path outside the project is not followed.
    #[test]
    fn a_journal_path_outside_the_project_is_refused() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-journal-outside-path");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        for outside in ["../outside", "C:/outside", "/outside"] {
            begin_test(&context)?;
            CURRENT.with(|slot| {
                let mut slot = slot.borrow_mut();
                let journal = slot.as_mut().unwrap();
                journal.record.paths.push(PathBuf::from(outside));
                journal.save()
            })?;
            interrupt_test_request();
            let error = Journal::load(&context)
                .err()
                .map(|error| format!("{error:#}"));
            assert!(
                error
                    .as_deref()
                    .is_some_and(|error| error.contains("outside the project")),
                "{outside}: {error:?}"
            );
            resolve_journal(&directory(&context))?;
        }
        Ok(())
    }

    #[test]
    fn a_studio_change_while_unsettled_does_not_fail_the_next_full_push() -> Result<()> {
        let fixture = super::super::push_flow_tests::Fixture::new()?;
        let context = &fixture.context;
        let paths = vec![PathBuf::from("src"), PathBuf::from("instances")];
        let verification_paths = fixture.desired.entries.keys().cloned().collect();
        let history = history::SyncHistory::begin(
            Path::new(&context.root),
            Path::new(&context.source),
            &ProjectSnapshot::default(),
            &verification_paths,
        )?;
        begin(
            context,
            ImportStart {
                services: &["ReplicatedStorage".into()],
                paths: &paths,
                verification_paths: &verification_paths,
                before: &ProjectSnapshot::default(),
                expected: &fixture.desired,
                input: None,
                history_id: history.id()?,
            },
        )?;
        committed(
            &Map::from_iter([
                ("historyId".into(), json!(history.id()?)),
                ("verifiedPushProof".into(), json!({"expired": true})),
            ]),
            &fixture.desired,
        )?;
        interrupt_test_request();
        super::super::push_flow_tests::replace_capture(ProjectSnapshot::default());
        let result = before_request(
            context,
            fixture.bridge.as_ref().unwrap(),
            op::PUSH,
            &json!({}),
        );
        assert!(
            result.is_ok(),
            "the user's full push failed: {:#}",
            result.unwrap_err()
        );
        Ok(())
    }

    #[test]
    fn a_journal_of_a_dot_relative_source_root_stays_recoverable() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-dot-source-root");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"./src","tree":{}}"#,
        )?;
        fs::create_dir_all(root.join("src/Workspace"))?;
        config::load_project(None, Some(&root))?;
        let stage = ExportProjectStage::create_for_comparison(
            &root,
            Path::new("./src"),
            &["Workspace".into()],
        )?;
        let paths = stage.publish_paths().to_vec();
        assert!(
            paths.iter().any(|path| path.starts_with("./src")),
            "{paths:?}"
        );
        let context = context(&root);
        begin(
            &context,
            ImportStart {
                services: &["Workspace".into()],
                paths: &paths,
                verification_paths: &HashSet::new(),
                before: &ProjectSnapshot::default(),
                expected: &ProjectSnapshot::default(),
                input: None,
                history_id: "dot-history".into(),
            },
        )?;
        committed(
            &Map::from_iter([("verifiedPushProof".into(), json!({"test": true}))]),
            &ProjectSnapshot::default(),
        )?;
        interrupt_test_request();
        let loaded = Journal::load(&context);
        assert!(loaded.is_ok(), "{:#}", loaded.err().unwrap());
        Ok(())
    }
}
