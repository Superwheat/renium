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
    /// The files already hold what the push wrote: a settlement that cannot
    /// finish leaves them so, as a push that reads nothing back does. Such a
    /// record is version 2, which a daemon that would load its absent
    /// snapshots refuses.
    #[serde(default)]
    optional: bool,
    /// The stage whose publication began, which holds under `previous` what
    /// its swaps moved aside.
    #[serde(default)]
    container: Option<PathBuf>,
}

/// The version of a record that keeps only the digests of the files.
const OPTIONAL_VERSION: u8 = 2;

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
            version == Some(1) || version == Some(OPTIONAL_VERSION.into()),
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

    /// Keeps the snapshots recovery loads, which an optional record leaves
    /// out: a settlement that stops after this leaves something to report.
    fn recoverable(&mut self, before: &ProjectSnapshot, expected: &ProjectSnapshot) -> Result<()> {
        if !self.record.optional {
            return Ok(());
        }
        let packs = self.directory.join("packs");
        self.record.before = StoredSnapshot::write_at(&packs, before.entries.iter())?;
        self.record.expected = StoredSnapshot::write_at(&packs, expected.entries.iter())?;
        self.record.optional = false;
        self.record.version = 1;
        Ok(())
    }

    fn accept<'a>(
        &mut self,
        entries: impl Iterator<Item = (&'a PathBuf, &'a SnapshotEntry)>,
    ) -> Result<()> {
        self.record.accepted = Some(StoredSnapshot::write_at(
            &self.directory.join("packs"),
            entries,
        )?);
        Ok(())
    }
}

/// Changes the journal of this request and saves it.
fn amend(change: impl FnOnce(&mut Journal) -> Result<()>) -> Result<()> {
    CURRENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let journal = slot.as_mut().context("Missing import settlement journal")?;
        change(journal)?;
        journal.save()
    })
}

pub(super) struct ImportStart<'a> {
    pub services: &'a [String],
    pub paths: &'a [PathBuf],
    pub verification_paths: &'a HashSet<PathBuf>,
    pub before: &'a ProjectSnapshot,
    pub expected: &'a ProjectSnapshot,
    /// What the push wrote into the files itself, over `expected`.
    pub generated: &'a ProjectSnapshot,
    pub input: Option<&'a Value>,
    pub history_id: String,
    pub optional: bool,
}

pub(super) fn can_continue(stage: &ExportProjectStage, requires_stage: bool) -> bool {
    !requires_stage
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
    let expected = start
        .expected
        .entries
        .iter()
        .filter(|(path, _)| !start.generated.entries.contains_key(*path))
        .chain(start.generated.entries.iter());
    // Nothing loads the snapshots of an optional record until it publishes:
    // it holds only what says whether newer edits came.
    let (version, before, expected) = if start.optional {
        (
            OPTIONAL_VERSION,
            StoredSnapshot::digests(std::iter::empty()),
            StoredSnapshot::digests(expected),
        )
    } else {
        (
            1,
            StoredSnapshot::write_at(&packs, start.before.entries.iter())?,
            StoredSnapshot::write_at(&packs, expected)?,
        )
    };
    let record = Record {
        version,
        journal_id: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
        runtime: context.runtime_id.clone(),
        services: start.services.to_vec(),
        paths: start.paths.to_vec(),
        verification_paths: start.verification_paths.clone(),
        before,
        expected,
        accepted: None,
        input: start.input.cloned(),
        summary: Map::from_iter([("historyId".into(), json!(start.history_id))]),
        error: None,
        optional: start.optional,
        container: None,
    };
    let journal = Journal {
        directory,
        record,
        _ownership: ownership,
    };
    journal.save()?;
    journal.install()
}

/// Journals a verified commit, with what Studio reported of it, before the
/// push answers. A push whose files already hold what it wrote settles
/// before it answers instead where that fails: false.
pub(super) fn journal(
    context: &BoundContext,
    start: ImportStart<'_>,
    summary: &Map<String, Value>,
) -> Result<bool> {
    let optional = start.optional;
    match begin(context, start).and_then(|()| committed(summary)) {
        Ok(()) => Ok(true),
        Err(error) if optional => {
            let _ = finish_request(None);
            log_global(
                3,
                format_args!(
                    "[renium] import settlement runs before the push answers: cannot journal it: {error:#}"
                ),
            );
            Ok(false)
        }
        Err(error) => Err(error),
    }
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

pub(super) fn committed(summary: &Map<String, Value>) -> Result<()> {
    amend(|journal| {
        journal.record.summary.extend(summary.clone());
        Ok(())
    })
}

#[cfg(test)]
fn accepted<'a>(entries: impl Iterator<Item = (&'a PathBuf, &'a SnapshotEntry)>) -> Result<()> {
    amend(|journal| journal.accept(entries))
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
    #[cfg(test)]
    if error.is_none() && CURRENT.with(|slot| slot.borrow().is_some()) {
        crate::snapshot::export::publication_step(
            crate::snapshot::export::PublicationStep::BeforeClear,
        );
    }
    let Some(mut journal) = CURRENT.with(|slot| slot.borrow_mut().take()) else {
        return Ok(());
    };
    let has_commit = journal
        .record
        .summary
        .get("verifiedPushProof")
        .is_some_and(|value| !value.is_null());
    let result = match error.filter(|_| has_commit) {
        Some(error) if !journal.record.optional => {
            journal
                .record
                .error
                .get_or_insert_with(|| error.to_string());
            log_global(
                3,
                format_args!("[renium] import settlement failed: {error}"),
            );
            journal.save()
        }
        Some(error) => {
            log_global(
                3,
                format_args!(
                    "[renium] import settlement left the files as the push wrote them: {error}"
                ),
            );
            resolve_journal(&journal.directory)
        }
        None => resolve_journal(&journal.directory),
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

/// A settlement that reads back only `services`, from a baseline over files
/// a pull wrote whose sourcemap that pull left as `sourcemap`.
pub(super) struct Narrowed<'a> {
    pub services: &'a [String],
    pub sourcemap: Option<&'a SourcemapState>,
}

/// The files a publication left and the sourcemap a baseline of them keeps.
type Published = (ProjectSnapshot, Option<Box<SourcemapState>>);

/// What a settlement left the files as.
pub(super) enum Settlement {
    Published(Published),
    /// As the push left them but for newer edits, or a sourcemap no longer
    /// the pull's, which stopped it before it wrote any.
    Unwritten,
    /// Newer edits stopped it once it had written Studio's copy.
    Stopped,
}

/// The same capture, verification and publication serve a live continuation
/// and recovery after a daemon restart. Recovery never invokes the importer.
pub(super) fn settle(
    context: &BoundContext,
    bridge: &BridgeServer,
    (full, narrowed): (&[String], Option<Narrowed<'_>>),
    work: &mut ImportWork,
    proof: &Value,
) -> Result<Settlement> {
    let root = Path::new(&context.root);
    let source = bound_context::source_dir(context)?;
    let files = match ensure_original_or_accepted_files(root, &work.paths) {
        Ok(files) => files,
        Err(error) if !error.is::<NewerProjectFiles>() => return Err(error),
        // The caller can start editing as soon as its verified reply arrives.
        // Leave those files alone; the next push reconciles the newer work.
        Err(_) => return Ok(Settlement::Unwritten),
    };
    let phase = Instant::now();
    let covered = ScopeSet::new(&work.paths);
    let baseline =
        |paths: &[PathBuf]| super::push_flow::publish_baseline(root, &files, &covered, paths);
    let stage_of = |services| -> Result<ExportProjectStage> {
        let mut stage = ExportProjectStage::create_with_baseline_background(
            root, &source, services, &baseline,
        )?;
        stage.prepare_read_only_links(root, &source, services)?;
        Ok(stage)
    };
    let mut services = narrowed.as_ref().map_or(full, |narrowed| narrowed.services);
    let mut stage = stage_of(services)?;
    // The services it skips keep their entries of the sourcemap it stages.
    if narrowed
        .as_ref()
        .and_then(|narrowed| narrowed.sourcemap)
        .is_some_and(|sourcemap| !holds_sourcemap(&stage.project_root, sourcemap))
    {
        if CURRENT.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|journal| journal.record.optional)
        }) {
            log_global(
                4,
                format_args!(
                    "[renium] import settlement leaves the files as pushed: sourcemap.json is not as the last pull left it"
                ),
            );
            return Ok(Settlement::Unwritten);
        }
        log_global(
            4,
            format_args!(
                "[renium] import settlement reads back all services: sourcemap.json is not as the last pull left it"
            ),
        );
        services = full;
        stage = stage_of(services)?;
    }
    let scopes = ScopeSet::new(stage.publish_paths());
    let staged = files
        .entries
        .into_keys()
        .filter(|path| scopes.contains(path))
        .collect::<Vec<_>>();
    crate::daemon::memory::release_between_phases();
    const LARGE_STORE_BYTES: usize = 4 * 1024 * 1024;
    let has_large_store = work.expected.entries.iter().any(|(path, entry)| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
            && matches!(entry, SnapshotEntry::File(bytes) if bytes.len() >= LARGE_STORE_BYTES)
    });
    let document_below_rows = (!has_large_store).then_some(super::verify::rows::MIN_ROWS);
    let (mut readback, observed_documents) = capture_studio_services_in_memory_options(
        context,
        bridge,
        services,
        &stage,
        true,
        true,
        document_below_rows,
    )?;
    log_reconcile_timing("full push readback", phase);
    crate::daemon::memory::release_between_phases();
    let phase = Instant::now();
    let mut stores = PredecodedStores {
        observed: observed_documents,
    };
    let (mismatches, details) = {
        let _defer_drop =
            (!has_large_store).then(crate::settings::bytecode::DeferredSettingsDropGuard::enter);
        verify_and_settle_native_import(
            (&work.before, &work.expected),
            &mut readback,
            &mut stores,
            &work.verification_paths,
            &mut work.prepared,
            Some(&NewInstanceValues::of(
                bridge,
                context.runtime_id.as_deref(),
            )),
        )?
    };
    log_reconcile_timing("full push verification", phase);
    if !mismatches.is_empty() {
        // Studio does not hold what the push sent: the next command says so.
        amend(|journal| journal.recoverable(&work.before, &work.expected))?;
        return Err(retention_failure(
            "pushed project changes",
            &mismatches,
            details.as_deref(),
        ));
    }
    let phase = Instant::now();
    super::push_flow::stage_verified_import_readback(&mut stage, &readback, &staged)?;
    log_reconcile_timing("full push stage verified readback", phase);
    let stable = || -> Result<()> {
        anyhow::ensure!(
            verified_full_push_proof_matches(context, bridge, proof)?,
            "Studio changed during import settlement; the committed import was not repeated"
        );
        if snapshot_matches_disk(root, &work.paths, &work.expected)? {
            return Ok(());
        }
        ensure_original_or_accepted_files(root, &work.paths)?;
        Ok(())
    };
    let published = publish_checked(
        stage,
        root,
        (&work.paths, &work.before, &work.expected),
        stable,
    );
    let (current, published) = match published {
        Ok(published) => published,
        Err(error) => {
            return match error.downcast_ref::<NewerProjectFiles>() {
                Some(NewerProjectFiles { written: true }) => Ok(Settlement::Stopped),
                Some(_) => Ok(Settlement::Unwritten),
                None => Err(error),
            };
        }
    };
    let id = work.history.commit(&current, &ProjectSnapshot::default())?;
    work.summary.insert("historyId".into(), json!(id));
    let sourcemap = if narrowed
        .as_ref()
        .is_some_and(|narrowed| narrowed.sourcemap.is_none())
    {
        None
    } else {
        pulled_sourcemap(&published)
    };
    Ok(Settlement::Published((current, sourcemap)))
}

/// The sourcemap a baseline of the files `published` left keeps: none where
/// it kept files back from Studio's copy, as a pull then keeps no baseline.
pub(super) fn pulled_sourcemap(
    published: &crate::snapshot::export::PublishedProjectChanges,
) -> Option<Box<SourcemapState>> {
    let kept_back = !published.preserved.is_empty()
        || !published.read_only_links_restored.is_empty()
        || !published.studio_overrides.is_empty();
    if kept_back {
        log_global(
            5,
            format_args!(
                "[renium] baseline: a pull on it captures every service: the settlement kept files back"
            ),
        );
        return None;
    }
    settled_sourcemap(published, None)
}

/// Whether the sourcemap.json under `root` is `sourcemap`.
pub(super) fn holds_sourcemap(root: &Path, sourcemap: &SourcemapState) -> bool {
    crate::snapshot::export::collect_publish_hashes(root, &[PathBuf::from("sourcemap.json")])
        .is_ok_and(|held| held == *sourcemap)
}

/// Files changed since the push, which the settlement leaves as they are:
/// `written` once it had written Studio's copy.
#[derive(Debug)]
struct NewerProjectFiles {
    written: bool,
}

impl std::fmt::Display for NewerProjectFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Project files changed during import settlement; newer files were preserved")
    }
}

impl std::error::Error for NewerProjectFiles {}

fn ensure_original_or_accepted_files(root: &Path, paths: &[PathBuf]) -> Result<ProjectSnapshot> {
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
        return Err(NewerProjectFiles { written: false }.into());
    }
    Ok(current)
}

/// Publishes the stage, outside whose paths the files stay as `held` has
/// them, the files of the push whose Studio held `before`. The files must
/// then be exactly what it staged and those; the returned snapshot covers
/// all of `paths`.
fn publish_checked(
    stage: ExportProjectStage,
    root: &Path,
    (paths, before, held): (&[PathBuf], &ProjectSnapshot, &ProjectSnapshot),
    stable: impl Fn() -> Result<()>,
) -> Result<(
    ProjectSnapshot,
    crate::snapshot::export::PublishedProjectChanges,
)> {
    let scopes = ScopeSet::new(stage.publish_paths());
    let mut staged = capture_snapshot(&stage.project_root, paths)?;
    if let Some(vs) = stage.virtual_scripts() {
        for (rel, state) in vs.staged_entries.lock_recover().iter() {
            if matches!(state, crate::snapshot::export::PublishEntryState::Directory) {
                staged
                    .entries
                    .entry(rel.clone())
                    .or_insert(SnapshotEntry::Directory);
            }
        }
        for rel in vs.skipped_paths.lock_recover().iter() {
            if let Some(entry) = held.entries.get(rel) {
                staged.entries.insert(rel.clone(), entry.clone());
            }
        }
    }
    let accepted_files = || {
        let unsettled = held
            .entries
            .iter()
            .filter(|(path, _)| !scopes.contains(path));
        staged
            .entries
            .iter()
            .filter(|(path, _)| scopes.contains(path))
            .chain(unsettled)
    };
    let container = stage.container().to_path_buf();
    let written = std::cell::Cell::new(false);
    let (published, _) = crate::snapshot::export::finish_export_publication(
        Some(stage),
        root,
        false,
        None,
        || {
            stable()?;
            #[cfg(test)]
            crate::snapshot::export::publication_step(
                crate::snapshot::export::PublicationStep::BeforeRecord,
            );
            // A stop between two of its swaps leaves a file moved aside, which
            // only a restart that loads this record puts back.
            amend(|journal| {
                journal.recoverable(before, held)?;
                journal.accept(accepted_files())?;
                journal.record.container = Some(container);
                Ok(())
            })
        },
        || {
            written.set(true);
            stable()
        },
    )
    .map_err(|error| {
        let newer = error.is::<NewerProjectFiles>()
            || error.is::<crate::snapshot::export::ProjectFilesChanged>();
        if newer {
            NewerProjectFiles {
                written: written.get(),
            }
            .into()
        } else {
            error
        }
    })?;
    let expected_snapshot = ProjectSnapshot {
        entries: accepted_files()
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect(),
    };
    let current = capture_snapshot(root, paths)?;
    if current != expected_snapshot {
        return Err(NewerProjectFiles { written: true }.into());
    }
    history::ensure_published_state(&published, &current)?;
    Ok((current, published))
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
    // An old/new/corrupt record does not stop the very recovery command named
    // in its error. Failed pulls leave its bytes intact.
    if full_pull && pending(context) {
        if let Ok(Some(journal)) = Journal::load(context) {
            if journal.record.optional {
                resolve_journal(&journal.directory)?;
                return Ok(None);
            }
            if let Err(error) = restore_moved_aside(Path::new(&context.root), &journal.record) {
                log_global(3, format_args!("[renium] {error:#}"));
            }
        }
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
    if journal.record.optional
        || journal
            .record
            .summary
            .get("verifiedPushProof")
            .is_none_or(Value::is_null)
    {
        resolve_journal(&journal.directory)?;
        return Ok(None);
    }
    if let Err(error) = restore_moved_aside(Path::new(&context.root), &journal.record) {
        if full_push {
            return Err(error);
        }
        log_global(3, format_args!("[renium] {error:#}"));
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
        // A half-restored project is none to push from: the record stays,
        // with the stage that holds what was moved aside.
        Err(error) if error.is::<crate::snapshot::export::IncompleteRollback>() => {
            finish_request(Some(&format!("{error:#}")))?;
            return Err(error);
        }
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

/// Puts back what a publication that stopped between two of its swaps had
/// moved aside and not replaced. The stage that held it is discarded once
/// the project accounts for every file still moved aside; until then it
/// holds the only copy of one.
fn restore_moved_aside(root: &Path, record: &Record) -> Result<()> {
    let Some(container) = record.container.as_deref() else {
        return Ok(());
    };
    anyhow::ensure!(
        container.parent() == root.parent()
            && container
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".renium-export-")),
        "The import settlement journal names a stage outside the project; run rbx pl to recover"
    );
    let staged = container.join(root.file_name().context("Project root has no name")?);
    let previous = container.join("previous");
    restore_missing(&previous, root, &staged)?;
    if accounted_for(&previous, root, record)? {
        let _ = fs::remove_dir_all(container);
    } else {
        log_global(
            3,
            format_args!(
                "[renium] kept {}: an unfinished import settlement moved files aside into it, and the project no longer holds what replaced them. Delete the folder once none of them is needed.",
                container.display()
            ),
        );
    }
    Ok(())
}

/// Whether the project accounts for each file still under `previous`. One
/// that is what `record` expected at its path is accounted for where the
/// project holds what the publication was to leave there: its accepted
/// files, which hold nothing where it deleted. Any other was saved before
/// the publication moved it aside: it goes back where the project holds
/// nothing, and is unaccounted for where it cannot. No snapshot holds a
/// derived file, which tools rewrite at any time, so none is asked of.
fn accounted_for(previous: &Path, root: &Path, record: &Record) -> Result<bool> {
    if fs::symlink_metadata(previous).is_err() {
        return Ok(true);
    }
    let entries = WalkDir::new(previous)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let mut accounted = true;
    for entry in entries.iter().filter(|entry| !entry.file_type().is_dir()) {
        let relative = as_recorded(&record.paths, entry.path().strip_prefix(previous)?);
        let scope = std::slice::from_ref(&relative);
        let moved_aside = capture_snapshot(previous, scope)?;
        accounted &= if record.expected.matches_at(&relative, &moved_aside) {
            let held = capture_snapshot(root, scope)?;
            let accepted = record.accepted.as_ref();
            accepted.is_some_and(|accepted| accepted.matches_at(&relative, &held))
        } else {
            let target = root.join(&relative);
            target
                .parent()
                .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
                && crate::system::files::move_no_replace(entry.path(), &target).is_ok()
        };
    }
    Ok(accounted)
}

/// `relative` as the path of `paths` it lies under spells it: a source root
/// given as `./src` keeps that spelling in every path of a snapshot.
fn as_recorded(paths: &[PathBuf], relative: &Path) -> PathBuf {
    paths
        .iter()
        .find_map(|scope| {
            let plain = scope
                .components()
                .filter(|part| matches!(part, Component::Normal(_)))
                .collect::<PathBuf>();
            let rest = relative.strip_prefix(plain).ok()?;
            Some(scope.join(rest))
        })
        .unwrap_or_else(|| relative.to_path_buf())
}

/// Moves back each entry under `backup` whose path under `root` is gone while
/// the stage still holds its replacement: a replacement the publication put
/// in place has left the stage. A path it deleted has none, so it stays
/// deleted, and a path saved meanwhile stays as saved. A kill between the
/// link that put a replacement in place and the removal of its staged name
/// leaves both, so a file the user then deletes comes back as the old one.
fn restore_missing(backup: &Path, root: &Path, staged: &Path) -> Result<()> {
    let entries = match fs::read_dir(backup) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let target = root.join(entry.file_name());
        let replacement = staged.join(entry.file_name());
        match fs::symlink_metadata(&target) {
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && fs::symlink_metadata(&replacement).is_ok() =>
            {
                match crate::system::files::move_no_replace(&entry.path(), &target) {
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    outcome => {
                        outcome.with_context(|| format!("Cannot restore {}", target.display()))?
                    }
                }
            }
            Ok(metadata) if metadata.is_dir() && entry.file_type()?.is_dir() => {
                restore_missing(&entry.path(), &target, &replacement)?;
            }
            _ => {}
        }
    }
    Ok(())
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
    mut work: ImportWork,
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
    let mut release =
        StudioTrackingGuardRelease::new(bridge, &guard.runtime_id, guard.tracking_guard_id.clone());
    #[cfg(windows)]
    {
        release.retain_until = attributes.as_ref().map(|relay: &NativeRelay| relay.ends);
    }
    let proof = capture_push_proof(bridge, &guard)?
        .context("Studio cannot prove a stable recovery capture; run rbx pl")?;
    let settlement = settle(context, bridge, (services, None), &mut work, &proof)?;
    release.proof = Some(proof);
    acknowledge_verified_push(bridge, services, &guard, &mut release)?;
    let Settlement::Published(current) = settlement else {
        return Ok(false);
    };
    retain_full_push(
        bridge,
        input.as_ref(),
        &work.paths,
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
            generated: &ProjectSnapshot::default(),
            input: None,
            history_id: "fixture".into(),
            optional: false,
        },
    )?;
    committed(&Map::from_iter([(
        "verifiedPushProof".into(),
        json!({"fixture": true}),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::export::PublicationStep;

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
        begin_test_as(context, false)
    }

    fn begin_test_as(context: &BoundContext, optional: bool) -> Result<ProjectSnapshot> {
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
                generated: &ProjectSnapshot::default(),
                input: None,
                history_id: "test-history".into(),
                optional,
            },
        )?;
        committed(&Map::from_iter([(
            "verifiedPushProof".into(),
            json!({"test": true}),
        )]))?;
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
        let mut work = ImportWork {
            before: ProjectSnapshot::default(),
            expected,
            paths,
            verification_paths,
            prepared: HashMap::new(),
            history,
            summary: Map::new(),
        };
        let settlement = settle(
            &context,
            &bridge,
            (&["Workspace".into()], None),
            &mut work,
            &json!({}),
        )?;
        assert!(matches!(settlement, Settlement::Unwritten));
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
                (&ProjectSnapshot::default(), &expected),
                &mut observed,
                &mut PredecodedStores::default(),
                &HashSet::from(["src/A.luau".into()]),
                &mut HashMap::new(),
                None,
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
                generated: &ProjectSnapshot::default(),
                input: None,
                history_id: history.id()?,
                optional: false,
            },
        )?;
        committed(&Map::from_iter([
            ("historyId".into(), json!(history.id()?)),
            ("verifiedPushProof".into(), json!({"expired":true})),
        ]))?;
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
            super::super::push_flow_tests::capture(context, &[], Path::new("")).is_none(),
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
            rmp_serde::to_vec_named(&json!({"version": 3, "future": "field"}))?,
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
        committed(&Map::from_iter([("verifiedPushProof".into(), json!(3))]))?;
        accepted(expected.entries.iter())?;
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
        accepted(published.entries.iter())?;
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
        accepted(expected.entries.iter())?;
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
        accepted(published.entries.iter())?;
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
        // At 4 a file of a service the stage does not settle is edited once
        // the stage is published; at 5 the check after it finds newer files.
        for failure_at in [0, 1, 2, 3, 4, 5] {
            let root = crate::tests::support::temp_dir("import-publication-fence");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let context = context(&root);
            begin_test(&context)?;
            let relative = Path::new("src/Workspace/A.luau");
            let outside = Path::new("src/ServerStorage/B.luau");
            for (path, text) in [(relative, "original"), (outside, "unsettled")] {
                fs::create_dir_all(root.join(path.parent().unwrap()))?;
                fs::write(root.join(path), text)?;
            }
            let stage = ExportProjectStage::create(&root, Path::new("src"), &["Workspace".into()])?;
            fs::create_dir_all(stage.project_root.join(relative.parent().unwrap()))?;
            fs::write(stage.project_root.join(relative), "accepted")?;
            let mut paths = stage.publish_paths().to_vec();
            paths.push(outside.parent().unwrap().to_path_buf());
            let held = capture_snapshot(&root, &paths)?;
            if failure_at == 3 {
                fs::write(root.join(relative), "newer file edit")?;
            }
            let calls = std::cell::Cell::new(0);
            let stable = || -> Result<()> {
                calls.set(calls.get() + 1);
                if failure_at == 4 && calls.get() == 2 {
                    fs::write(root.join(outside), "newer edit elsewhere")?;
                }
                if failure_at == 5 && calls.get() == 2 {
                    return Err(NewerProjectFiles { written: false }.into());
                }
                anyhow::ensure!(
                    calls.get() != failure_at,
                    "Studio closed or changed at publication boundary"
                );
                Ok(())
            };
            let result = publish_checked(
                stage,
                &root,
                (&paths, &ProjectSnapshot::default(), &held),
                stable,
            );
            let newer = result.as_ref().err().and_then(|error| {
                error
                    .downcast_ref::<NewerProjectFiles>()
                    .map(|newer| newer.written)
            });
            assert_eq!(
                newer,
                match failure_at {
                    3 => Some(false),
                    4 | 5 => Some(true),
                    _ => None,
                },
                "failure at {failure_at}"
            );
            match failure_at {
                0 => {
                    assert!(result.is_ok());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "accepted");
                }
                1 => {
                    assert!(result.is_err());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "original");
                }
                2 | 5 => {
                    assert!(result.is_err());
                    assert_eq!(fs::read_to_string(root.join(relative))?, "accepted");
                }
                4 => {
                    assert!(
                        result
                            .as_ref()
                            .is_err_and(|error| error.is::<NewerProjectFiles>())
                    );
                    assert_eq!(
                        fs::read_to_string(root.join(outside))?,
                        "newer edit elsewhere"
                    );
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

    /// A settlement of files that hold what the push wrote copies none of
    /// them before the push answers, and leaves nothing unsettled and says
    /// nothing when it fails or the daemon stops before it publishes.
    #[test]
    fn an_optional_settlement_that_cannot_finish_leaves_nothing_unsettled() -> Result<()> {
        let bridge = crate::studio::bridge::routing_tests::fixture();
        for next in [None, Some(op::LIVE_START), Some(op::PULL)] {
            let root = crate::tests::support::temp_dir("settle-optional");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_dir_all(&root);
            });
            let context = context(&root);
            begin_test_as(&context, true)?;
            assert!(!directory(&context).join("packs").exists());
            let record: Value =
                rmp_serde::from_slice(&fs::read(directory(&context).join("pending.rmp"))?)?;
            assert_eq!(record["version"], 2);
            let notices = crate::daemon::fixture_request_notices();
            match next {
                Some(operation) => {
                    interrupt_test_request();
                    before_request(&context, &bridge, operation, &json!({}))?;
                }
                None => finish_request(Some(
                    "Studio changed during import settlement; the committed import was not repeated",
                ))?,
            }
            assert_eq!(notices(), Vec::<String>::new(), "{next:?}");
            assert!(!pending(&context), "{next:?}");
            assert!(
                RECOVERY_PULL
                    .with(|slot| slot.borrow_mut().take())
                    .is_none()
            );
        }
        Ok(())
    }

    /// A readback of an optional settlement that finds Studio does not hold
    /// what the push sent leaves a record the next command reports.
    #[test]
    fn a_readback_mismatch_of_an_optional_settlement_stays_unsettled() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-optional-mismatch");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test_as(&context, true)?;
        let verification_paths = HashSet::from([PathBuf::from("src/A.luau")]);
        let history =
            history::SyncHistory::begin(&root, &root.join("src"), &expected, &verification_paths)?;
        let mut work = ImportWork {
            before: ProjectSnapshot::default(),
            expected: expected.clone(),
            paths: vec![PathBuf::from("src")],
            verification_paths,
            prepared: HashMap::new(),
            history,
            summary: Map::new(),
        };
        let mut observed = expected.clone();
        observed.entries.insert(
            "src/A.luau".into(),
            SnapshotEntry::File(b"return 999".to_vec()),
        );
        super::super::push_flow_tests::replace_capture(observed);
        let bridge = crate::studio::bridge::routing_tests::fixture();
        let error = settle(
            &context,
            &bridge,
            (&["Workspace".into()], None),
            &mut work,
            &json!({}),
        )
        .err()
        .context("the readback matched")?;
        finish_request(Some(&format!("{error:#}")))?;
        let journal = Journal::load(&context)?.context("nothing stayed unsettled")?;
        assert!(!journal.record.optional);
        assert!(journal.record.error.is_some());
        let packs = journal.directory.join("packs");
        assert!(journal.record.expected.load_at(&packs, |_| true)? == expected);
        let notices = crate::daemon::fixture_request_notices();
        before_request(&context, &bridge, op::LIVE_START, &json!({}))?;
        let notices = notices();
        assert!(
            notices.len() == 1
                && notices[0].starts_with("The files of the last push were not settled"),
            "{notices:?}"
        );
        Ok(())
    }

    /// An optional settlement narrowed on a pull whose sourcemap the files
    /// no longer hold reads nothing back and leaves the files as pushed.
    #[test]
    fn an_optional_settlement_off_the_pulls_sourcemap_reads_nothing_back() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-optional-sourcemap");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        let expected = begin_test_as(&context, true)?;
        fs::write(root.join("sourcemap.json"), "{}")?;
        let verification_paths = HashSet::from([PathBuf::from("src/A.luau")]);
        let history =
            history::SyncHistory::begin(&root, &root.join("src"), &expected, &verification_paths)?;
        let mut work = ImportWork {
            before: ProjectSnapshot::default(),
            expected,
            paths: vec![PathBuf::from("src")],
            verification_paths,
            prepared: HashMap::new(),
            history,
            summary: Map::new(),
        };
        let bridge = crate::studio::bridge::routing_tests::fixture();
        super::super::push_flow_tests::replace_capture(ProjectSnapshot::default());
        let narrowed = Narrowed {
            services: &["Workspace".into()],
            sourcemap: Some(&SourcemapState::new()),
        };
        let settlement = settle(
            &context,
            &bridge,
            (
                &["Workspace".into(), "ServerStorage".into()],
                Some(narrowed),
            ),
            &mut work,
            &json!({}),
        )?;
        assert!(matches!(settlement, Settlement::Unwritten));
        assert!(
            super::super::push_flow_tests::capture(&context, &[], Path::new("")).is_some(),
            "the settlement read Studio back"
        );
        finish_request(None)?;
        assert!(!pending(&context));
        assert_eq!(fs::read_to_string(root.join("src/A.luau"))?, "return 1");
        Ok(())
    }

    /// The files a killed publication replaces, as the push left them and as
    /// the settlement leaves them. Every file of ReplicatedStorage changes,
    /// so that service is swapped as one directory; the files of Workspace
    /// are swapped one by one.
    const PUSHED: [(&str, &str); 5] = [
        ("src/ReplicatedStorage/Gone.luau", "old gone"),
        ("src/ReplicatedStorage/Mod.luau", "old mod"),
        ("src/Workspace/Kept.luau", "kept"),
        ("src/Workspace/Removed.luau", "old removed"),
        ("src/Workspace/Replaced.luau", "old replaced"),
    ];
    const SETTLED: [(&str, &str); 5] = [
        ("src/ReplicatedStorage/Fresh.luau", "new fresh"),
        ("src/ReplicatedStorage/Mod.luau", "new mod"),
        ("src/Workspace/Added.luau", "new added"),
        ("src/Workspace/Kept.luau", "kept"),
        ("src/Workspace/Replaced.luau", "new replaced"),
    ];
    /// The paths that publication swaps under the source root. It swaps the
    /// sourcemap too, a derived file the record of the import leaves out.
    const SWAPPED: [&str; 4] = [
        "src/ReplicatedStorage",
        "src/Workspace/Added.luau",
        "src/Workspace/Removed.luau",
        "src/Workspace/Replaced.luau",
    ];
    const MAP: &str = "sourcemap.json";
    const SAVED: &str = "saved while down";
    const SAVED_BEFORE: &str = "saved before the kill";

    type Tree = BTreeMap<PathBuf, String>;

    fn tree(files: &[(&str, &str)]) -> Tree {
        files
            .iter()
            .map(|(path, text)| (PathBuf::from(path), (*text).to_string()))
            .collect()
    }

    /// The files under `base/src`, by their path under `base`.
    fn read_tree(base: &Path) -> Result<Tree> {
        let mut files = Tree::new();
        if !base.join("src").exists() {
            return Ok(files);
        }
        for entry in WalkDir::new(base.join("src")) {
            let entry = entry?;
            if entry.file_type().is_file() {
                files.insert(
                    entry.path().strip_prefix(base)?.to_path_buf(),
                    fs::read_to_string(entry.path())?,
                );
            }
        }
        Ok(files)
    }

    /// What the user does to a file while the daemon is down.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum WhileDown {
        Nothing,
        Delete,
        Rename,
        Save,
    }

    enum Report {
        Container(PathBuf),
        Killed(Vec<PublicationStep>, Option<PathBuf>),
        Finished(Result<(), String>),
    }

    /// The file a user acts on at a swapped `path`: that file, or one of
    /// the directory.
    fn file_at(path: &Path) -> PathBuf {
        if path == Path::new(SWAPPED[0]) {
            path.join("Mod.luau")
        } else {
            path.to_path_buf()
        }
    }

    /// A publication stopped as a killed daemon stops it.
    struct Killed {
        _history: history::SyncHistory,
        fixture: super::super::push_flow_tests::Fixture,
        /// The files Studio held before the push, and those the push left.
        before: ProjectSnapshot,
        expected: ProjectSnapshot,
        container: PathBuf,
        /// The steps it reached, the last the one it stopped at.
        steps: Vec<PublicationStep>,
        /// The file saved as SAVED_BEFORE once the publication had checked
        /// the path the kill came at, and before it moved that path aside.
        saved: Option<PathBuf>,
    }

    /// Publishes SETTLED over PUSHED for a journaled import, on a thread that
    /// stops at its `kill_at`th step and never runs again: nothing it holds
    /// is dropped, and the journal and its lock go as with a dead process.
    /// None where the publication ends before that step. With `save_before`
    /// a file is saved at the path under `src` that the step before the
    /// kill is to move aside.
    fn publication_killed_at(
        kill_at: usize,
        optional: bool,
        save_before: bool,
    ) -> Result<Option<Killed>> {
        let fixture = super::super::push_flow_tests::Fixture::new()?;
        let context = fixture.context.clone();
        let root = PathBuf::from(&context.root);
        for (path, text) in PUSHED {
            fs::create_dir_all(root.join(path).parent().unwrap())?;
            fs::write(root.join(path), text)?;
        }
        fs::write(root.join(MAP), "old map")?;
        let paths = vec![PathBuf::from("src"), PathBuf::from("instances")];
        let expected = capture_snapshot(&root, &paths)?;
        let mut before = expected.clone();
        before.entries.insert(
            "src/Workspace/Replaced.luau".into(),
            SnapshotEntry::File(b"before the push".to_vec()),
        );
        let verification_paths = expected.entries.keys().cloned().collect::<HashSet<_>>();
        let history = history::SyncHistory::begin(
            &root,
            Path::new(&context.source),
            &before,
            &verification_paths,
        )?;
        let history_id = history.id()?;
        let (held, pushed_over) = (expected.clone(), before.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::Builder::new().spawn(move || {
            let publish = || -> Result<()> {
                let services = ["ReplicatedStorage".to_string(), "Workspace".to_string()];
                begin(
                    &context,
                    ImportStart {
                        services: &services,
                        paths: &paths,
                        verification_paths: &verification_paths,
                        before: &before,
                        expected: &held,
                        generated: &ProjectSnapshot::default(),
                        input: None,
                        history_id: history_id.clone(),
                        optional,
                    },
                )?;
                committed(&Map::from_iter([
                    ("historyId".into(), json!(history_id)),
                    ("verifiedPushProof".into(), json!({"expired": true})),
                ]))?;
                let stage = ExportProjectStage::create(&root, Path::new("src"), &services)?;
                for (path, _) in PUSHED {
                    fs::remove_file(stage.project_root.join(path))?;
                }
                for (path, text) in SETTLED {
                    fs::write(stage.project_root.join(path), text)?;
                }
                fs::write(stage.project_root.join(MAP), "new map")?;
                let _ = sender.send(Report::Container(stage.container().to_path_buf()));
                let mut steps = Vec::new();
                let killed = sender.clone();
                let (project, mut saved) = (root.clone(), None);
                let _hook = crate::snapshot::export::hook_publication_steps(move |step| {
                    steps.push(step.clone());
                    if let PublicationStep::MoveAside(path) = step
                        && save_before
                        && steps.len() == kill_at
                        && path.starts_with("src")
                    {
                        let file = file_at(path);
                        fs::write(project.join(&file), SAVED_BEFORE).unwrap();
                        saved = Some(file);
                    }
                    if steps.len() > kill_at {
                        interrupt_test_request();
                        let _ = killed.send(Report::Killed(steps.clone(), saved.clone()));
                        loop {
                            thread::park();
                        }
                    }
                });
                let (_, published) =
                    publish_checked(stage, &root, (&paths, &before, &held), || Ok(()))?;
                drop(published);
                finish_request(None)
            };
            let outcome = publish().map_err(|error| format!("{error:#}"));
            let _ = sender.send(Report::Finished(outcome));
        })?;
        let mut container = None;
        loop {
            let report = receiver
                .recv_timeout(Duration::from_secs(60))
                .context("the publication neither stopped nor ended")?;
            match report {
                Report::Container(path) => container = Some(path),
                Report::Finished(outcome) => {
                    outcome.map_err(anyhow::Error::msg)?;
                    return Ok(None);
                }
                Report::Killed(steps, saved) => {
                    return Ok(Some(Killed {
                        _history: history,
                        fixture,
                        before: pushed_over,
                        expected,
                        container: container.context("the publication staged nothing")?,
                        steps,
                        saved,
                    }));
                }
            }
        }
    }

    impl Killed {
        fn reached(&self, step: PublicationStep) -> bool {
            self.steps.contains(&step)
        }

        /// Whether the swap of `path` was done at the kill: its replacement
        /// was in place, or it was moved aside with none to come.
        fn swapped(&self, path: &Path) -> bool {
            let replaced =
                path == Path::new(MAP) || tree(&SETTLED).keys().any(|file| file.starts_with(path));
            self.reached(PublicationStep::PublishedIn(path.into()))
                || !replaced && self.reached(PublicationStep::MovedAside(path.into()))
        }

        /// The files once the next command recovered: each swapped path as
        /// the settlement leaves it, every other as the push left it.
        fn recovered(&self) -> Tree {
            let (pushed, settled) = (tree(&PUSHED), tree(&SETTLED));
            pushed
                .keys()
                .chain(settled.keys())
                .filter_map(|file| {
                    let swap = SWAPPED.iter().find(|swap| file.starts_with(swap));
                    let from = if swap.is_some_and(|swap| self.swapped(Path::new(swap))) {
                        &settled
                    } else {
                        &pushed
                    };
                    Some((file.clone(), from.get(file)?.clone()))
                })
                .collect()
        }

        /// The file the user acts on: the one the kill came at, or one of
        /// the directory it came at.
        fn acted_on(&self) -> PathBuf {
            use PublicationStep::*;
            match self.steps.last() {
                Some(MoveAside(path) | MovedAside(path) | PublishedIn(path))
                    if path.starts_with("src") =>
                {
                    file_at(path)
                }
                _ => PathBuf::from("src/Workspace/Replaced.luau"),
            }
        }

        /// Does `action` to the project and to `recovered`, the files
        /// recovery leaves without it. A save into a directory that was
        /// moved aside makes that directory anew: beside the save come back
        /// the files the stage holds a replacement of, and the others stay
        /// moved aside, which the settlement deletes.
        fn act(&self, action: WhileDown, recovered: &mut Tree) -> Result<()> {
            let root = Path::new(&self.fixture.context.root);
            let file = self.acted_on();
            let on_disk = root.join(&file);
            match action {
                WhileDown::Delete if on_disk.exists() => {
                    fs::remove_file(on_disk)?;
                    recovered.remove(&file);
                }
                WhileDown::Rename if on_disk.exists() => {
                    let renamed = PathBuf::from(format!("{}.renamed", file.display()));
                    fs::rename(on_disk, root.join(&renamed))?;
                    let text = recovered.remove(&file).context("no file to rename")?;
                    recovered.insert(renamed, text);
                }
                WhileDown::Save => {
                    let folder = file.parent().context("the file has no folder")?;
                    if !root.join(folder).exists() {
                        let settled = tree(&SETTLED);
                        recovered.retain(|path, _| {
                            !path.starts_with(folder) || settled.contains_key(path)
                        });
                        fs::create_dir_all(root.join(folder))?;
                    }
                    fs::write(on_disk, SAVED)?;
                    recovered.insert(file, SAVED.to_string());
                }
                _ => {}
            }
            Ok(())
        }

        /// Checks the record a restart finds: until the publication records
        /// itself it is the push's own, and from then on one that an older
        /// daemon reads too, with the stage and the files of the push.
        fn check_record(&self, optional: bool) -> Result<()> {
            let context = &self.fixture.context;
            let journal = Journal::load(context)?.context("the kill left no record")?;
            let saved: Value =
                rmp_serde::from_slice(&fs::read(journal.directory.join("pending.rmp"))?)?;
            if self.steps.len() == 1 {
                assert_eq!(journal.record.optional, optional);
                assert_eq!(journal.record.container, None);
                return Ok(());
            }
            assert_eq!(saved["version"], 1);
            assert!(!journal.record.optional);
            assert_eq!(
                journal.record.container.as_deref(),
                Some(self.container.as_path())
            );
            let packs = journal.directory.join("packs");
            assert!(journal.record.expected.load_at(&packs, |_| true)? == self.expected);
            assert!(journal.record.before.load_at(&packs, |_| true)? == self.before);
            Ok(())
        }

        /// Runs the restart's recovery through `operation` after `action`,
        /// and checks what it leaves.
        fn recover(&self, action: WhileDown, operation: u16, optional: bool) -> Result<()> {
            let context = &self.fixture.context;
            let root = Path::new(&context.root);
            let case = format!("{:?}, {action:?}, op {operation}", self.steps.last());
            let unrecorded = self.steps.len() == 1;
            let moved_aside = read_tree(&self.container.join("previous"))?;
            let mut recovered = self.recovered();
            if let Some(saved) = &self.saved {
                recovered.insert(saved.clone(), SAVED_BEFORE.to_string());
            }
            self.act(action, &mut recovered)?;
            let settles = operation == op::PUSH && !(optional && unrecorded);
            if operation == op::PUSH {
                super::super::push_flow_tests::replace_capture(self.expected.clone());
                recovered = tree(&PUSHED);
            }
            let notices = crate::daemon::fixture_request_notices();
            let bridge = self.fixture.bridge.as_ref().unwrap();
            let answered = before_request(context, bridge, operation, &json!({}))
                .with_context(|| case.clone())?;
            let notices = notices();
            assert!(answered.is_none(), "{case}");
            let files = read_tree(root)?;
            assert_eq!(files, recovered, "{case}");
            let map = if self.swapped(Path::new(MAP)) {
                "new map"
            } else {
                "old map"
            };
            assert_eq!(fs::read_to_string(root.join(MAP))?, map, "{case}");
            let settled = tree(&SETTLED);
            let unaccounted = moved_aside
                .iter()
                .filter(|(path, text)| {
                    files.get(*path) != Some(*text) && files.get(*path) != settled.get(*path)
                })
                .collect::<Vec<_>>();
            for (path, text) in &unaccounted {
                let kept = fs::read_to_string(self.container.join("previous").join(path));
                assert_eq!(
                    kept.ok().as_ref(),
                    Some(*text),
                    "{case}: {}",
                    path.display()
                );
            }
            if unaccounted.is_empty() && !unrecorded {
                assert!(!self.container.exists(), "{case}");
            }
            if operation == op::PUSH || optional && unrecorded {
                assert_eq!(notices, Vec::<String>::new(), "{case}");
                assert!(!pending(context), "{case}");
            } else {
                assert_eq!(
                    notices,
                    [
                        "The files of the last push were not settled: the daemon stopped before settlement finished. Repeat rbx ps to reconcile your files, or run rbx pl to take Studio's state."
                    ],
                    "{case}"
                );
                assert!(pending(context), "{case}");
            }
            let unread =
                super::super::push_flow_tests::capture(context, &[], Path::new("")).is_some();
            assert_eq!(unread, !settles, "{case}: the restart settled the files");
            Ok(())
        }
    }

    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.container);
        }
    }

    /// A daemon killed at any step of a publication leaves each path whole:
    /// as the publication found it or as it was to leave it, with what the
    /// user did to it while the daemon was down. Nothing is truncated or
    /// there twice, the stage stays while it holds the only copy of a file,
    /// and a full push settles the files. A file saved once the publication
    /// had checked its path, and moved aside with the kill before the check
    /// of what was moved, comes back, also where the publication deletes
    /// that path; where the user saved it again while the daemon was down,
    /// the stage keeps the earlier save.
    #[test]
    fn a_kill_at_any_step_of_a_publication_leaves_each_path_whole() -> Result<()> {
        use WhileDown::*;
        let cases = [
            (Nothing, op::LIVE_START),
            (Nothing, op::PUSH),
            (Delete, op::LIVE_START),
            (Rename, op::LIVE_START),
            (Save, op::LIVE_START),
        ];
        for optional in [true, false] {
            let mut walked = Vec::<PublicationStep>::new();
            let mut saved_before = Vec::new();
            'steps: for kill_at in 0.. {
                anyhow::ensure!(kill_at < 64, "the publication does not end");
                let moved_aside = match walked.last() {
                    Some(PublicationStep::MoveAside(path)) if path.starts_with("src") => {
                        Some(path.clone())
                    }
                    _ => None,
                };
                for (action, operation) in cases {
                    let Some(killed) = publication_killed_at(kill_at, optional, false)? else {
                        break 'steps;
                    };
                    if (action, operation) == cases[0] {
                        killed.check_record(optional)?;
                        walked.push(killed.steps.last().cloned().unwrap());
                    }
                    killed.recover(action, operation, optional)?;
                }
                for action in [Nothing, Save] {
                    let Some(path) = &moved_aside else {
                        break;
                    };
                    let killed = publication_killed_at(kill_at, optional, true)?
                        .context("the publication ended before the kill")?;
                    let stopped_at = PublicationStep::MovedAside(path.clone());
                    assert_eq!(killed.steps.last(), Some(&stopped_at));
                    assert_eq!(killed.saved, Some(file_at(path)));
                    killed.recover(action, op::LIVE_START, optional)?;
                    saved_before.push(path.clone());
                }
            }
            for path in SWAPPED {
                assert!(
                    saved_before.contains(&PathBuf::from(path)),
                    "no save before the kill at {path}: {saved_before:?}"
                );
            }
            let at = |path: &str| PathBuf::from(path);
            for step in [
                PublicationStep::BeforeRecord,
                PublicationStep::BeforeConcurrencyCheck,
                PublicationStep::MovedAside(at(MAP)),
                PublicationStep::PublishedIn(at(MAP)),
                PublicationStep::MovedAside(at(SWAPPED[0])),
                PublicationStep::PublishedIn(at(SWAPPED[0])),
                PublicationStep::PublishedIn(at(SWAPPED[1])),
                PublicationStep::MovedAside(at(SWAPPED[2])),
                PublicationStep::MovedAside(at(SWAPPED[3])),
                PublicationStep::PublishedIn(at(SWAPPED[3])),
                PublicationStep::Swapped,
                PublicationStep::BeforeClear,
            ] {
                assert!(walked.contains(&step), "no kill at {step:?}: {walked:?}");
            }
            assert_eq!(walked.len(), 19, "{walked:?}");
        }
        Ok(())
    }

    /// A publication that cannot put back all it moved aside is an error,
    /// not newer files: its record stays, with the stage that holds them.
    /// Here a save stops the publication, which then finds the first file it
    /// moved aside gone.
    #[test]
    fn an_incomplete_rollback_keeps_its_record_and_what_it_moved_aside() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-incomplete-rollback");
        let context = context(&root);
        begin_test_as(&context, true)?;
        let first = Path::new("src/Workspace/A.luau");
        let second = Path::new("src/Workspace/B.luau");
        let kept = Path::new("src/Workspace/Kept.luau");
        fs::create_dir_all(root.join(first.parent().unwrap()))?;
        for path in [first, second, kept] {
            fs::write(root.join(path), "original")?;
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
        let held = capture_snapshot(&root, &paths)?;
        let (saved, moved_aside) = (root.join(second), container.join("previous").join(first));
        let hook = crate::snapshot::export::hook_publication_steps(move |step| {
            if *step == PublicationStep::MoveAside(second.into()) {
                fs::write(&saved, "saved meanwhile").unwrap();
                fs::remove_file(&moved_aside).unwrap();
            }
        });
        let result = publish_checked(
            stage,
            &root,
            (&paths, &ProjectSnapshot::default(), &held),
            || Ok(()),
        );
        drop(hook);
        let error = result.err().context("the publication went through")?;
        assert!(
            error.downcast_ref::<NewerProjectFiles>().is_none()
                && error.to_string().contains("rollback was incomplete"),
            "{error:#}"
        );
        finish_request(Some(&format!("{error:#}")))?;
        let journal = Journal::load(&context)?.context("the record was dropped")?;
        assert!(!journal.record.optional);
        assert!(
            journal
                .record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("rollback was incomplete"))
        );
        assert_eq!(
            journal.record.container.as_deref(),
            Some(container.as_path())
        );
        assert!(container.join("previous").exists());
        assert_eq!(fs::read_to_string(root.join(second))?, "saved meanwhile");
        Ok(())
    }

    /// A recovery push whose publication cannot put back all it moved aside
    /// fails and names the stage that holds the files; its record stays,
    /// with that stage. Here the daemon was killed once every path was
    /// swapped, so the recovery swaps each back: a save stops it, and it
    /// then finds a file it moved aside gone.
    #[test]
    fn a_recovery_push_fails_on_an_incomplete_rollback_and_keeps_its_record() -> Result<()> {
        let mut kill_at = 0;
        let killed = loop {
            let killed = publication_killed_at(kill_at, false, false)?
                .context("no kill once every path was swapped")?;
            if killed.steps.last() == Some(&PublicationStep::Swapped) {
                break killed;
            }
            kill_at += 1;
        };
        let context = &killed.fixture.context;
        let root = Path::new(&context.root);
        let first = Path::new("src/Workspace/Added.luau");
        let second = Path::new("src/Workspace/Replaced.luau");
        super::super::push_flow_tests::replace_capture(killed.expected.clone());
        let stage = std::rc::Rc::new(RefCell::new(None::<PathBuf>));
        let _cleanup = OnDrop::new({
            let stage = std::rc::Rc::clone(&stage);
            move || {
                if let Some(stage) = stage.borrow().as_ref() {
                    let _ = fs::remove_dir_all(stage);
                }
            }
        });
        let (saved, staged) = (root.join(second), std::rc::Rc::clone(&stage));
        let hook = crate::snapshot::export::hook_publication_steps(move |step| {
            if *step == PublicationStep::MoveAside(second.into()) {
                let container = CURRENT
                    .with(|slot| slot.borrow().as_ref()?.record.container.clone())
                    .unwrap();
                fs::write(&saved, "saved meanwhile").unwrap();
                fs::remove_file(container.join("previous").join(first)).unwrap();
                *staged.borrow_mut() = Some(container);
            }
        });
        let bridge = killed.fixture.bridge.as_ref().unwrap();
        let result = before_request(context, bridge, op::PUSH, &json!({}));
        drop(hook);
        let error = result.err().context("the recovery push went on")?;
        assert!(
            error.is::<crate::snapshot::export::IncompleteRollback>(),
            "{error:#}"
        );
        let stage = stage.borrow().clone().context("nothing was moved aside")?;
        assert_ne!(stage, killed.container);
        assert!(
            format!("{error:#}").contains(&stage.display().to_string()),
            "{error:#}"
        );
        let journal = Journal::load(context)?.context("the record was resolved")?;
        assert!(
            journal
                .record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("rollback was incomplete"))
        );
        assert_eq!(journal.record.container.as_deref(), Some(stage.as_path()));
        assert!(stage.join("previous").exists());
        assert_eq!(fs::read_to_string(root.join(second))?, "saved meanwhile");
        Ok(())
    }

    /// A project that names its source root `./src` keeps that spelling in
    /// the paths of its record, which a file moved aside is looked up under.
    #[test]
    fn a_moved_aside_file_is_looked_up_as_its_record_spells_it() {
        let paths = [PathBuf::from("./src"), PathBuf::from("instances")];
        for (found, recorded) in [
            ("src/Workspace/A.luau", "./src/Workspace/A.luau"),
            ("instances/Workspace.renium", "instances/Workspace.renium"),
            ("sourcemap.json", "sourcemap.json"),
        ] {
            // Paths compare by their components, a leading `.` among them.
            assert_eq!(as_recorded(&paths, Path::new(found)), Path::new(recorded));
        }
    }

    /// A journal that names a path outside the project is not followed.
    #[test]
    fn a_journal_path_outside_the_project_is_refused() -> Result<()> {
        let root = crate::tests::support::temp_dir("settle-journal-outside-path");
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let context = context(&root);
        // A drive prefix is a path of its own only on Windows.
        let outside: &[&str] = if cfg!(windows) {
            &["../outside", "C:/outside", "/outside"]
        } else {
            &["../outside", "/outside"]
        };
        for outside in outside {
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
                generated: &ProjectSnapshot::default(),
                input: None,
                history_id: history.id()?,
                optional: false,
            },
        )?;
        committed(&Map::from_iter([
            ("historyId".into(), json!(history.id()?)),
            ("verifiedPushProof".into(), json!({"expired": true})),
        ]))?;
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
                generated: &ProjectSnapshot::default(),
                input: None,
                history_id: "dot-history".into(),
                optional: false,
            },
        )?;
        committed(&Map::from_iter([(
            "verifiedPushProof".into(),
            json!({"test": true}),
        )]))?;
        interrupt_test_request();
        let loaded = Journal::load(&context);
        assert!(loaded.is_ok(), "{:#}", loaded.err().unwrap());
        Ok(())
    }
}
