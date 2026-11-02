use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use yrs::sync::Awareness;
use yrs::types::DeepObservable;
use yrs::types::{Event, PathSegment};
use yrs::{ClientID, MapRef, Subscription, Transact};

use super::document::{self, Content};
use crate::app::output::log_global;
use crate::project::config;
use crate::system::LockRecover;
use crate::system::files::{atomic_write_file, fnv1a};
use crate::system::watch::FileWatcher;

const SETTLE: Duration = Duration::from_millis(60);
const POLL: Duration = Duration::from_millis(100);
const STALE_SAVE_WINDOW: Duration = Duration::from_secs(5);
const REPLACED_TEXTS: usize = 4;
const REMOTE_LABEL: &str = "room";

#[derive(Default)]
pub(crate) struct MirrorStats {
    pub(crate) files: AtomicU64,
    pub(crate) local_changes: AtomicU64,
    pub(crate) remote_changes: AtomicU64,
    pub(crate) error: Mutex<Option<String>>,
}

/// The content last agreed between the file and the document. The text is
/// the base for merging a later local save with remote changes that reached
/// the document meanwhile. `replaced` keeps the texts this mirror overwrote
/// on disk since the last local save: a tool that read the file before such
/// a write and saves right after it saved an edit of the older text.
/// `pending` keeps the last save made here and the shared text it was folded
/// into, so a remote save made from that same text is recognized when it
/// arrives.
struct Synced {
    hash: u64,
    text: Option<String>,
    replaced: Vec<Replaced>,
    pending: Option<Pending>,
}

struct Replaced {
    text: String,
    at: Instant,
    source: Source,
}

struct Pending {
    base: String,
    hash: u64,
    local: String,
}

/// Where a text the mirror writes over a file came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Another participant's change.
    Remote,
    /// The merge of a save made here with the shared text.
    Merge,
}

fn synced(content: &Content) -> Synced {
    Synced {
        hash: fnv1a(&content.bytes()),
        text: match content {
            Content::Text(text) => Some(text.clone()),
            Content::Binary(_) => None,
        },
        replaced: Vec::new(),
        pending: None,
    }
}

impl Synced {
    /// The text a local save was most likely made from: the agreed text, or
    /// a text overwritten moments ago that the save is closer to. A save that
    /// restores exactly what a remote change replaced is the user's choice
    /// and counts as an edit of the agreed text.
    fn base_for(&self, local: &str) -> Option<String> {
        let latest = document::with_line_endings_of(self.text.clone()?, local);
        let mut recent = self
            .replaced
            .iter()
            .rev()
            .filter(|replaced| replaced.at.elapsed() < STALE_SAVE_WINDOW)
            .peekable();
        if recent.peek().is_none() {
            return Some(latest);
        }
        let mut best = (document::changed_lines(&latest, local), None);
        for replaced in recent {
            let text = document::with_line_endings_of(replaced.text.clone(), local);
            let distance = document::changed_lines(&text, local);
            if distance < best.0 {
                best = (distance, Some((text, replaced.source)));
            }
        }
        Some(match best {
            (0, Some((_, Source::Remote))) | (_, None) => latest,
            (_, Some((text, _))) => text,
        })
    }

    fn replaced_by(self, content: &Content, source: Source) -> Synced {
        let mut next = synced(content);
        next.replaced = self.replaced;
        next.pending = self.pending;
        next.replaced
            .retain(|replaced| replaced.at.elapsed() < STALE_SAVE_WINDOW);
        if let Some(text) = self.text
            && next.text.as_ref() != Some(&text)
        {
            next.replaced.push(Replaced {
                text,
                at: Instant::now(),
                source,
            });
        }
        let excess = next.replaced.len().saturating_sub(REPLACED_TEXTS);
        next.replaced.drain(..excess);
        next
    }
}

pub(crate) struct Mirror {
    root: PathBuf,
    name: String,
    awareness: Arc<Mutex<Awareness>>,
    files: MapRef,
    watched_files: BTreeSet<PathBuf>,
    watched_directories: BTreeSet<PathBuf>,
    watcher: FileWatcher,
    ledger: HashMap<String, Synced>,
    dirty: Arc<Mutex<BTreeMap<String, BTreeSet<ClientID>>>>,
    stats: Arc<MirrorStats>,
    _subscription: Subscription,
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | ".renium" | "node_modules" | ".vscode")
        )
    }) || path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".lock") || name.ends_with(".tmp"))
}

pub(crate) fn key_for(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut key = String::new();
    for component in relative.components() {
        let part = component.as_os_str().to_str()?;
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(part);
    }
    (!key.is_empty()).then_some(key)
}

// A key from another participant names a file inside the project, never a
// parent directory, another drive or Renium's own metadata.
fn path_for(root: &Path, key: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in key.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':', '\0']) {
            return None;
        }
        path.push(part);
    }
    (!is_ignored(&path)).then_some(path)
}

fn refuse_key(key: &str) {
    log_global(
        1,
        format_args!("[renium] collaboration refused a file outside the project: {key}"),
    );
}

fn project_file_present(root: &Path) -> bool {
    root.join("renium.project.jsonc").is_file() || root.join("renium.project.json").is_file()
}

impl Mirror {
    pub(crate) fn open(
        root: &Path,
        name: &str,
        awareness: Arc<Mutex<Awareness>>,
        stats: Arc<MirrorStats>,
    ) -> Result<Mirror> {
        let (watched_files, watched_directories) = watch_inputs(root)?;
        let mut watcher = FileWatcher::new(4096)?;
        watcher.set_inputs(&watched_files, &watched_directories)?;
        let dirty = Arc::new(Mutex::new(BTreeMap::<String, BTreeSet<ClientID>>::new()));
        let (files, subscription) = {
            let guard = awareness.lock_recover();
            let files = document::files_map(guard.doc());
            let dirty = dirty.clone();
            let subscription = files.observe_deep(move |txn, events| {
                if txn.origin() == Some(&yrs::Origin::from(document::LOCAL_ORIGIN)) {
                    return;
                }
                let mut keys = Vec::new();
                for event in events.iter() {
                    match event {
                        Event::Map(map) => {
                            if map.path().is_empty() {
                                keys.extend(map.keys(txn).keys().map(|key| key.to_string()));
                            } else if let Some(PathSegment::Key(key)) = map.path().front() {
                                keys.push(key.to_string());
                            }
                        }
                        other => {
                            if let Some(PathSegment::Key(key)) = other.path().front() {
                                keys.push(key.to_string());
                            }
                        }
                    }
                }
                if !keys.is_empty() {
                    let writers = txn.insert_set().client_ids().collect::<Vec<_>>();
                    let mut dirty = dirty.lock_recover();
                    for key in keys {
                        dirty
                            .entry(key)
                            .or_default()
                            .extend(writers.iter().copied());
                    }
                }
            });
            (files, subscription)
        };
        Ok(Mirror {
            root: root.to_path_buf(),
            name: name.to_string(),
            awareness,
            files,
            watched_files,
            watched_directories,
            watcher,
            ledger: HashMap::new(),
            dirty,
            stats,
            _subscription: subscription,
        })
    }

    fn disk_snapshot(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut snapshot = BTreeMap::new();
        for file in &self.watched_files {
            self.collect_file(file, &mut snapshot)?;
        }
        for directory in &self.watched_directories {
            if !directory.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(directory)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| !is_ignored(entry.path()))
            {
                let entry = entry?;
                if entry.file_type().is_file() {
                    self.collect_file(entry.path(), &mut snapshot)?;
                }
            }
        }
        Ok(snapshot)
    }

    fn collect_file(&self, path: &Path, snapshot: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
        if is_ignored(path) || !path.is_file() {
            return Ok(());
        }
        let Some(key) = key_for(&self.root, path) else {
            return Ok(());
        };
        let bytes = read_settled(path)?;
        snapshot.insert(key, bytes);
        Ok(())
    }

    pub(crate) fn seed_from_disk(&mut self) -> Result<usize> {
        let snapshot = self.disk_snapshot()?;
        let mut written = 0;
        {
            let guard = self.awareness.lock_recover();
            let doc = guard.doc();
            let meta = document::meta_map(doc);
            let mut txn = document::transact_local(doc);
            for (key, bytes) in &snapshot {
                let content = Content::from_bytes(Path::new(key), bytes.clone());
                if document::write_entry(&mut txn, &self.files, key, &content) {
                    written += 1;
                }
                self.ledger.insert(key.clone(), synced(&content));
            }
            let name = self
                .root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("project");
            document::set_meta(&mut txn, &meta, "project", name);
            document::set_meta(&mut txn, &meta, "version", "1");
        }
        self.stats
            .files
            .store(snapshot.len() as u64, Ordering::Release);
        Ok(written)
    }

    pub(crate) fn materialize(&mut self) -> Result<usize> {
        let entries = {
            let guard = self.awareness.lock_recover();
            let txn = guard.doc().transact();
            document::snapshot(&txn, &self.files)
        };
        let disk = self.disk_snapshot()?;
        let mut written = 0;
        for (key, content) in &entries {
            let hash = fnv1a(&content.bytes());
            if disk
                .get(key)
                .is_some_and(|existing| fnv1a(existing) == hash)
            {
                self.ledger.insert(key.clone(), synced(content));
                continue;
            }
            if self.write_file(key, content, Source::Remote)? {
                written += 1;
            }
        }
        for key in disk.keys() {
            if !entries.contains_key(key) {
                if let Some(path) = path_for(&self.root, key) {
                    let _ = std::fs::remove_file(&path);
                }
                self.ledger.remove(key);
                written += 1;
            }
        }
        self.stats
            .files
            .store(entries.len() as u64, Ordering::Release);
        self.dirty.lock_recover().clear();
        Ok(written)
    }

    /// Reads the shared inputs again; returns whether they changed.
    pub(crate) fn refresh_inputs(&mut self) -> Result<bool> {
        if !project_file_present(&self.root) {
            return Ok(false);
        }
        let (files, directories) = watch_inputs(&self.root)?;
        if files == self.watched_files && directories == self.watched_directories {
            return Ok(false);
        }
        self.watched_files = files;
        self.watched_directories = directories;
        self.watcher
            .set_inputs(&self.watched_files, &self.watched_directories)?;
        Ok(true)
    }

    fn write_file(&mut self, key: &str, content: &Content, source: Source) -> Result<bool> {
        let Some(path) = path_for(&self.root, key) else {
            refuse_key(key);
            return Ok(false);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write_file(&path, &content.bytes())?;
        let entry = match self.ledger.remove(key) {
            Some(previous) => previous.replaced_by(content, source),
            None => synced(content),
        };
        self.ledger.insert(key.to_string(), entry);
        Ok(true)
    }

    fn is_input(&self, path: &Path) -> bool {
        self.watched_files.contains(path)
            || self
                .watched_directories
                .iter()
                .any(|directory| path.starts_with(directory))
    }

    // Folds a saved file into the document as the change since the content
    // last synchronized, so a remote edit that reached the document meanwhile
    // survives in the merged result. Returns the merged content, whether the
    // document changed and the shared text the save was folded into.
    fn absorb_local(&mut self, key: &str, content: Content) -> (Content, bool, Option<String>) {
        let guard = self.awareness.lock_recover();
        let doc = guard.doc();
        let bases = document::bases_map(doc);
        let mut txn = document::transact_local(doc);
        let (merged, shared) = match (content, document::read_entry(&txn, &self.files, key)) {
            (Content::Text(local), Some(Content::Text(shared))) => {
                let local = document::with_line_endings_of(local, &shared);
                let merged = match self
                    .ledger
                    .get(key)
                    .and_then(|entry| entry.base_for(&local))
                {
                    Some(base) if base != shared => {
                        let labels = document::MergeLabels {
                            local: &self.name,
                            remote: REMOTE_LABEL,
                        };
                        document::merge_lines(&base, &local, &shared, &labels)
                    }
                    _ => local,
                };
                document::record_base(
                    &mut txn,
                    &bases,
                    doc.client_id(),
                    key,
                    fnv1a(shared.as_bytes()),
                );
                (Content::Text(merged), Some(shared))
            }
            (content, _) => (content, None),
        };
        let changed = document::write_entry(&mut txn, &self.files, key, &merged);
        (merged, changed, shared)
    }

    // A saved file whose content is not what was last synchronized becomes a
    // local change first; without this a remote flush would overwrite it.
    // Returns whether the file was rewritten with the merged content.
    fn absorb_pending_save(&mut self, key: &str, path: &Path) -> Result<Option<bool>> {
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = read_settled(path)?;
        if self
            .ledger
            .get(key)
            .is_some_and(|entry| entry.hash == fnv1a(&bytes))
        {
            return Ok(None);
        }
        let (merged, changed, shared) =
            self.absorb_local(key, Content::from_bytes(path, bytes.clone()));
        if changed {
            self.stats.local_changes.fetch_add(1, Ordering::AcqRel);
        }
        self.keep_merged(key, &merged, &bytes, shared).map(Some)
    }

    // A remote save folded into the same shared text as the save pending here
    // was made without knowledge of it; the room kept both sides' lines, and
    // lines both changed are marked. Returns the marked content, written to
    // the document, when this side is the one to mark them.
    fn mark_concurrent(
        &mut self,
        key: &str,
        shared: &Content,
        writers: &BTreeSet<ClientID>,
    ) -> Option<Content> {
        let Content::Text(shared) = shared else {
            return None;
        };
        let pending = self.ledger.get(key)?.pending.as_ref()?;
        let guard = self.awareness.lock_recover();
        let doc = guard.doc();
        let bases = document::bases_map(doc);
        let mut txn = document::transact_local(doc);
        if !writers
            .iter()
            .any(|writer| document::recorded_base(&txn, &bases, *writer, key) == Some(pending.hash))
        {
            return None;
        }
        let labels = document::MergeLabels {
            local: &self.name,
            remote: REMOTE_LABEL,
        };
        let marked = Content::Text(document::mark_merged_conflicts(
            &pending.base,
            &pending.local,
            shared,
            &labels,
        )?);
        document::write_entry(&mut txn, &self.files, key, &marked);
        document::record_base(
            &mut txn,
            &bases,
            doc.client_id(),
            key,
            fnv1a(shared.as_bytes()),
        );
        Some(marked)
    }

    pub(crate) fn run(&mut self, stop: &AtomicBool) {
        let mut pending = BTreeSet::<PathBuf>::new();
        let mut last_event = Instant::now();
        let mut rescan = false;
        while !stop.load(Ordering::Acquire) {
            match self.watcher.receiver().recv_timeout(POLL) {
                Ok(Ok(event)) => {
                    last_event = Instant::now();
                    for path in event.paths {
                        pending.insert(path);
                    }
                }
                Ok(Err(_)) => rescan = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if self.watcher.take_overflowed() {
                rescan = true;
            }
            if let Err(error) = self.flush_remote() {
                *self.stats.error.lock_recover() = Some(format!("{error:#}"));
            }
            if rescan && last_event.elapsed() >= SETTLE {
                rescan = false;
                pending.clear();
                if let Err(error) = self.rescan() {
                    *self.stats.error.lock_recover() = Some(format!("{error:#}"));
                }
            } else if !pending.is_empty() && last_event.elapsed() >= SETTLE {
                let paths = std::mem::take(&mut pending);
                if let Err(error) = self.flush_local(paths) {
                    *self.stats.error.lock_recover() = Some(format!("{error:#}"));
                }
            }
        }
    }

    fn rescan(&mut self) -> Result<()> {
        self.refresh_inputs()?;
        let disk = self.disk_snapshot()?;
        let mut changed = 0;
        for (key, bytes) in &disk {
            if self
                .ledger
                .get(key)
                .is_some_and(|entry| entry.hash == fnv1a(bytes))
            {
                continue;
            }
            let content = Content::from_bytes(Path::new(key), bytes.clone());
            let (merged, wrote, shared) = self.absorb_local(key, content);
            if wrote {
                changed += 1;
            }
            self.keep_merged(key, &merged, bytes, shared)?;
        }
        {
            let guard = self.awareness.lock_recover();
            let doc = guard.doc();
            let mut txn = document::transact_local(doc);
            let existing = document::snapshot(&txn, &self.files);
            for key in existing.keys() {
                if !disk.contains_key(key) && self.ledger.contains_key(key) {
                    document::remove_entry(&mut txn, &self.files, key);
                    self.ledger.remove(key);
                    changed += 1;
                }
            }
        }
        self.stats.files.store(disk.len() as u64, Ordering::Release);
        self.stats
            .local_changes
            .fetch_add(changed, Ordering::AcqRel);
        Ok(())
    }

    // The merged content is what the document holds now; when it differs
    // from the saved bytes the file receives it, and the watcher's later event
    // for that write finds the ledger already matching. Returns whether the
    // file was rewritten.
    fn keep_merged(
        &mut self,
        key: &str,
        merged: &Content,
        saved: &[u8],
        shared: Option<String>,
    ) -> Result<bool> {
        self.ledger.insert(
            key.to_string(),
            synced(&Content::from_bytes(Path::new(key), saved.to_vec())),
        );
        let rewritten = merged.bytes() != saved && self.write_file(key, merged, Source::Merge)?;
        if let (Some(entry), Some(base), Content::Text(local)) =
            (self.ledger.get_mut(key), shared, merged)
        {
            entry.pending = Some(Pending {
                hash: fnv1a(base.as_bytes()),
                base,
                local: local.clone(),
            });
        }
        Ok(rewritten)
    }

    fn flush_local(&mut self, paths: BTreeSet<PathBuf>) -> Result<()> {
        // A saved file that changes which files the project shares (its link
        // manifest, the project file) is read first, so the batch is filtered
        // against the inputs it names and files it adds are published at once.
        if paths
            .iter()
            .any(|path| key_for(&self.root, path).is_some_and(|key| changes_inputs(&key)))
            && self.refresh_inputs()?
        {
            return self.rescan();
        }
        let mut changed = 0u64;
        let mut directories = Vec::new();
        for path in paths {
            if is_ignored(&path) {
                continue;
            }
            if path.is_dir() {
                if self
                    .watched_directories
                    .iter()
                    .any(|directory| directory.starts_with(&path) || path.starts_with(directory))
                {
                    directories.push(path);
                }
                continue;
            }
            let Some(key) = key_for(&self.root, &path) else {
                continue;
            };
            // Only files the project shares travel; a file saved elsewhere in
            // the folder, such as an .env, stays local.
            if path.is_file() && self.is_input(&path) {
                let bytes = read_settled(&path)?;
                if self
                    .ledger
                    .get(&key)
                    .is_some_and(|entry| entry.hash == fnv1a(&bytes))
                {
                    continue;
                }
                let content = Content::from_bytes(&path, bytes.clone());
                let (merged, wrote, shared) = self.absorb_local(&key, content);
                if wrote {
                    changed += 1;
                }
                self.keep_merged(&key, &merged, &bytes, shared)?;
            } else if !path.is_file() && self.ledger.remove(&key).is_some() {
                let guard = self.awareness.lock_recover();
                let mut txn = document::transact_local(guard.doc());
                if document::remove_entry(&mut txn, &self.files, &key) {
                    changed += 1;
                }
            }
        }
        if !directories.is_empty() {
            self.rescan()?;
        }
        self.stats
            .local_changes
            .fetch_add(changed, Ordering::AcqRel);
        if changed > 0 {
            self.stats
                .files
                .store(self.ledger.len() as u64, Ordering::Release);
        }
        Ok(())
    }

    fn flush_remote(&mut self) -> Result<()> {
        let keys = std::mem::take(&mut *self.dirty.lock_recover());
        if keys.is_empty() {
            return Ok(());
        }
        let entries = {
            let guard = self.awareness.lock_recover();
            let txn = guard.doc().transact();
            keys.into_iter()
                .map(|(key, writers)| {
                    let content = document::read_entry(&txn, &self.files, &key);
                    (key, writers, content)
                })
                .collect::<Vec<_>>()
        };
        let mut changed = 0u64;
        let mut project_changed = false;
        for (key, writers, content) in entries {
            let Some(path) = path_for(&self.root, &key) else {
                refuse_key(&key);
                continue;
            };
            match content {
                Some(content) => {
                    if let Some(rewritten) = self.absorb_pending_save(&key, &path)? {
                        changed += u64::from(rewritten);
                    } else if let Some(marked) = self.mark_concurrent(&key, &content, &writers) {
                        if self.write_file(&key, &marked, Source::Merge)? {
                            changed += 1;
                        }
                        if let Some(entry) = self.ledger.get_mut(&key) {
                            entry.pending = None;
                        }
                    } else if self
                        .ledger
                        .get(&key)
                        .is_none_or(|entry| entry.hash != fnv1a(&content.bytes()))
                        && self.write_file(&key, &content, Source::Remote)?
                    {
                        changed += 1;
                    }
                }
                None => {
                    if path.is_file() {
                        std::fs::remove_file(&path)?;
                        changed += 1;
                    }
                    self.ledger.remove(&key);
                }
            }
            if changes_inputs(&key) {
                project_changed = true;
            }
        }
        if project_changed {
            self.refresh_inputs()?;
        }
        self.stats
            .remote_changes
            .fetch_add(changed, Ordering::AcqRel);
        if changed > 0 {
            self.stats
                .files
                .store(self.ledger.len() as u64, Ordering::Release);
        }
        Ok(())
    }
}

fn watch_inputs(root: &Path) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::new();
    if project_file_present(root) {
        let loaded = config::load_project(None, Some(root))
            .with_context(|| format!("Could not load the project at {}", root.display()))?;
        let inputs = config::project_watch_inputs(&loaded)?;
        files.extend(inputs.files.into_iter().map(|path| absolute(path, root)));
        directories.extend(
            inputs
                .directories
                .into_iter()
                .map(|path| absolute(path, root)),
        );
    }
    for name in [
        "renium.project.jsonc",
        ".gitignore",
        ".gitattributes",
        "wally.toml",
        "aftman.toml",
        "rokit.toml",
        "selene.toml",
        ".luaurc",
    ] {
        let path = root.join(name);
        if path.is_file() {
            files.insert(path);
        }
    }
    for path in crate::project::package_links::project_link_inputs(root) {
        if path.is_dir() {
            directories.insert(path);
        } else {
            files.insert(path);
        }
    }
    Ok((files, directories))
}

/// Saved files that change which files the mirror shares.
fn changes_inputs(key: &str) -> bool {
    key == "renium-link.json" || key.rsplit('/').next() == Some("renium.project.jsonc")
}

fn absolute(path: PathBuf, root: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn read_settled(path: &Path) -> Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        match std::fs::read(path) {
            Ok(bytes) => return Ok(bytes),
            Err(error) if attempt < 5 && matches!(error.raw_os_error(), Some(32 | 33)) => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20 * attempt));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("Could not read {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_use_forward_slashes_relative_to_root() {
        let root = Path::new("E:/proj");
        assert_eq!(
            key_for(root, Path::new("E:/proj/src/a/b.luau")),
            Some("src/a/b.luau".into())
        );
        assert_eq!(key_for(root, Path::new("E:/other/x")), None);
        assert_eq!(
            path_for(root, "src/a/b.luau"),
            Some(PathBuf::from("E:/proj/src/a/b.luau"))
        );
    }

    #[test]
    fn remote_keys_stay_inside_the_project() {
        let root = Path::new("E:/proj");
        for key in [
            "../escaped.txt",
            "src/../../escaped.txt",
            "/etc/passwd",
            "C:/Windows/notepad.exe",
            "src\\..\\x.luau",
            "src//a.luau",
            "./a.luau",
            ".git/hooks/pre-commit",
            ".renium/config.json",
            "a\0b",
        ] {
            assert_eq!(path_for(root, key), None, "{key}");
        }
    }

    #[test]
    fn ignores_metadata_folders_and_lock_files() {
        assert!(is_ignored(Path::new("p/.git/HEAD")));
        assert!(is_ignored(Path::new("p/.renium/x")));
        assert!(is_ignored(Path::new("p/src/a.renium.lock")));
        assert!(!is_ignored(Path::new("p/src/a.renium")));
        assert!(!is_ignored(Path::new("p/src/a.luau")));
    }

    use yrs::updates::decoder::Decode;
    use yrs::{ReadTxn, Update};

    const FILE: &str = "selene.toml";
    const FIVE: &str = "line 1\nline 2\nline 3\nline 4\nline 5\n";

    struct Side {
        root: PathBuf,
        awareness: Arc<Mutex<Awareness>>,
        mirror: Option<Mirror>,
    }

    impl Side {
        fn new(tag: &str, client: u64) -> Side {
            let root =
                crate::system::files::create_unique_directory(&std::env::temp_dir(), tag).unwrap();
            std::fs::write(root.join(FILE), FIVE).unwrap();
            Side {
                root,
                awareness: Arc::new(Mutex::new(Awareness::new(document::new_doc_with_client(
                    client,
                )))),
                mirror: None,
            }
        }

        fn with_project(self) -> Side {
            std::fs::write(
                self.root.join("renium.project.jsonc"),
                r#"{"schemaVersion":1,"sourceRoot":"src"}"#,
            )
            .unwrap();
            self
        }

        fn write(&self, relative: &str, content: impl AsRef<[u8]>) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn flush_paths(&mut self, relatives: &[&str]) {
            let paths = relatives
                .iter()
                .map(|relative| self.root.join(relative))
                .collect();
            self.mirror().flush_local(paths).unwrap();
        }

        fn same_file_as(&self, other: &Side, relative: &str) -> bool {
            let mine = std::fs::read(self.root.join(relative)).ok();
            mine.is_some() && mine == std::fs::read(other.root.join(relative)).ok()
        }

        fn open(&mut self, name: &str) -> &mut Mirror {
            let mirror = Mirror::open(
                &self.root,
                name,
                self.awareness.clone(),
                Arc::new(MirrorStats::default()),
            )
            .unwrap();
            self.mirror.insert(mirror)
        }

        fn mirror(&mut self) -> &mut Mirror {
            self.mirror.as_mut().unwrap()
        }

        fn save(&mut self, text: &str) {
            std::fs::write(self.root.join(FILE), text).unwrap();
        }

        fn flush_save(&mut self) {
            let path = self.root.join(FILE);
            self.mirror().flush_local(BTreeSet::from([path])).unwrap();
        }

        fn read(&self) -> String {
            std::fs::read_to_string(self.root.join(FILE)).unwrap()
        }
    }

    impl Drop for Side {
        fn drop(&mut self) {
            self.mirror = None;
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn send(from: &Side, to: &Side) {
        let state = to.awareness.lock_recover().doc().transact().state_vector();
        let update = from
            .awareness
            .lock_recover()
            .doc()
            .transact()
            .encode_state_as_update_v1(&state);
        let guard = to.awareness.lock_recover();
        let mut txn = document::transact_remote(guard.doc());
        txn.apply_update(Update::decode_v1(&update).unwrap())
            .unwrap();
    }

    fn joined_pair() -> (Side, Side) {
        join(Side::new("collab-host", 1), Side::new("collab-guest", 2))
    }

    fn join(mut host: Side, mut guest: Side) -> (Side, Side) {
        host.open("host").seed_from_disk().unwrap();
        send(&host, &guest);
        guest.open("guest").materialize().unwrap();
        (host, guest)
    }

    fn link_manifest(links: &[(&str, &str)]) -> String {
        let links = links
            .iter()
            .map(|(id, path)| {
                serde_json::json!({
                    "id": id, "readOnly": true, "source": {"type": "local", "path": path},
                    "targets": [{"service": "ReplicatedStorage", "path": ["ReplicatedStorage", id]}]
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({"version": 1, "links": links, "broken": []}).to_string()
    }

    fn exchange(host: &mut Side, guest: &mut Side) {
        send(host, guest);
        send(guest, host);
        guest.mirror().flush_remote().unwrap();
        host.mirror().flush_remote().unwrap();
    }

    #[test]
    fn the_link_manifest_and_local_link_sources_reach_a_guest() {
        let mut host = Side::new("collab-link-host", 1);
        let links = host.root.join("links");
        std::fs::create_dir_all(links.join("Folder")).unwrap();
        std::fs::write(links.join("shared.renium"), [0u8, 1, 2]).unwrap();
        std::fs::write(links.join("Logger.luau"), "return \"v1\"\n").unwrap();
        std::fs::write(links.join("Folder").join("Inner.luau"), "return 1\n").unwrap();
        std::fs::write(links.join("unlinked.luau"), "return 0\n").unwrap();
        let link = |id: &str, path: &str| {
            serde_json::json!({
                "id": id, "readOnly": true, "source": {"type": "local", "path": path},
                "targets": [{"service": "ReplicatedStorage", "path": ["ReplicatedStorage", id]}]
            })
        };
        let manifest = serde_json::json!({
            "version": 1,
            "links": [
                link("shared", "links/shared.renium"),
                link("logger", "links/Logger.luau"),
                link("folder", "links/Folder"),
                link("global", "~global/x.renium"),
                link("escape", "../escape.luau"),
            ],
            "broken": [],
        });
        std::fs::write(host.root.join("renium-link.json"), manifest.to_string()).unwrap();
        host.open("host").seed_from_disk().unwrap();
        let mut guest = Side::new("collab-link-guest", 2);
        send(&host, &guest);
        guest.open("guest").materialize().unwrap();
        for file in [
            "renium-link.json",
            "links/shared.renium",
            "links/Logger.luau",
            "links/Folder/Inner.luau",
        ] {
            assert_eq!(
                std::fs::read(guest.root.join(file)).ok(),
                std::fs::read(host.root.join(file)).ok(),
                "{file}"
            );
        }
        assert!(!guest.root.join("links/unlinked.luau").exists());
    }

    #[test]
    fn a_link_manifest_saved_while_the_room_runs_reaches_a_guest_with_its_sources() {
        let (mut host, mut guest) = join(
            Side::new("collab-late-manifest-host", 1).with_project(),
            Side::new("collab-late-manifest-guest", 2),
        );
        host.write("links/New.luau", "return 1\n");
        host.write("links/Folder/Inner.luau", "return 2\n");
        host.write(
            "renium-link.json",
            link_manifest(&[("new", "links/New.luau"), ("folder", "links/Folder")]),
        );
        host.flush_paths(&["renium-link.json"]);
        exchange(&mut host, &mut guest);
        for file in [
            "renium-link.json",
            "links/New.luau",
            "links/Folder/Inner.luau",
        ] {
            assert!(guest.same_file_as(&host, file), "{file}");
        }
    }

    #[test]
    fn a_link_added_to_the_manifest_while_the_room_runs_reaches_a_guest_with_its_source() {
        let host = Side::new("collab-late-link-host", 1).with_project();
        host.write("links/First.luau", "return 1\n");
        host.write(
            "renium-link.json",
            link_manifest(&[("first", "links/First.luau")]),
        );
        let (mut host, mut guest) = join(host, Side::new("collab-late-link-guest", 2));
        assert!(guest.same_file_as(&host, "links/First.luau"));
        host.write("links/Second.luau", "return 2\n");
        host.write(
            "renium-link.json",
            link_manifest(&[
                ("first", "links/First.luau"),
                ("second", "links/Second.luau"),
            ]),
        );
        host.flush_paths(&["renium-link.json"]);
        exchange(&mut host, &mut guest);
        for file in ["renium-link.json", "links/Second.luau"] {
            assert!(guest.same_file_as(&host, file), "{file}");
        }
    }

    #[test]
    fn a_crlf_save_merges_with_a_remote_edit_that_arrived_first() {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1 host\nline 2\nline 3\nline 4\nline 5\n");
        guest.save("line 1\r\nline 2\r\nline 3\r\nline 4\r\nline 5 guest\r\n");
        host.flush_save();
        send(&host, &guest);
        guest.mirror().flush_remote().unwrap();
        exchange(&mut host, &mut guest);
        let merged = "line 1 host\nline 2\nline 3\nline 4\nline 5 guest\n";
        assert_eq!(guest.read(), merged);
        assert_eq!(host.read(), merged);
    }

    #[test]
    fn a_crlf_save_merges_with_a_concurrent_remote_edit() {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1 host\nline 2\nline 3\nline 4\nline 5\n");
        guest.save("line 1\r\nline 2\r\nline 3\r\nline 4\r\nline 5 guest\r\n");
        host.flush_save();
        guest.flush_save();
        exchange(&mut host, &mut guest);
        let merged = "line 1 host\nline 2\nline 3\nline 4\nline 5 guest\n";
        assert_eq!(guest.read(), merged);
        assert_eq!(host.read(), merged);
    }

    #[test]
    fn a_save_made_from_the_text_a_remote_edit_just_replaced_keeps_that_edit() {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1 host\nline 2\nline 3\nline 4\nline 5\n");
        host.flush_save();
        send(&host, &guest);
        guest.mirror().flush_remote().unwrap();
        assert_eq!(
            guest.read(),
            "line 1 host\nline 2\nline 3\nline 4\nline 5\n"
        );
        guest.save("line 1\nline 2\nline 3\nline 4\nline 5 guest\n");
        guest.flush_save();
        exchange(&mut host, &mut guest);
        let merged = "line 1 host\nline 2\nline 3\nline 4\nline 5 guest\n";
        assert_eq!(guest.read(), merged);
        assert_eq!(host.read(), merged);
        guest.save("line 1\nline 2\nline 3\nline 4\nline 5 guest\n");
        guest.flush_save();
        exchange(&mut host, &mut guest);
        assert_eq!(
            guest.read(),
            merged,
            "a repeated stale save keeps the merge"
        );
        assert_eq!(host.read(), merged);
    }

    #[test]
    fn a_save_that_restores_the_text_a_remote_edit_replaced_undoes_that_edit() {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1 host\nline 2\nline 3\nline 4\nline 5\n");
        host.flush_save();
        send(&host, &guest);
        guest.mirror().flush_remote().unwrap();
        assert_eq!(
            guest.read(),
            "line 1 host\nline 2\nline 3\nline 4\nline 5\n"
        );
        guest.save(FIVE);
        guest.flush_save();
        exchange(&mut host, &mut guest);
        assert_eq!(guest.read(), FIVE);
        assert_eq!(host.read(), FIVE);
    }

    #[test]
    fn edits_of_the_same_line_flushed_before_either_arrives_keep_both_versions_between_markers() {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1\nline 2\nline 3 host\nline 4\nline 5\n");
        guest.save("line 1\nline 2\nline 3 guest\nline 4\nline 5\n");
        host.flush_save();
        guest.flush_save();
        exchange(&mut host, &mut guest);
        exchange(&mut host, &mut guest);
        let merged = "line 1\nline 2\n<<<<<<< host\nline 3 host\n=======\nline 3 guest\n>>>>>>> room\nline 4\nline 5\n";
        assert_eq!(host.read(), merged);
        assert_eq!(guest.read(), merged);
    }

    #[test]
    fn crlf_edits_of_the_same_line_flushed_before_either_arrives_keep_both_versions_between_markers()
     {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1\nline 2\nline 3 host\nline 4\nline 5\n");
        guest.save("line 1\r\nline 2\r\nline 3 guest\r\nline 4\r\nline 5\r\n");
        host.flush_save();
        guest.flush_save();
        exchange(&mut host, &mut guest);
        exchange(&mut host, &mut guest);
        let merged = "line 1\nline 2\n<<<<<<< host\nline 3 host\n=======\nline 3 guest\n>>>>>>> room\nline 4\nline 5\n";
        assert_eq!(host.read(), merged);
        assert_eq!(guest.read(), merged);
    }

    #[test]
    fn edits_of_the_same_line_keep_both_versions_between_markers() {
        let (mut host, mut guest) = joined_pair();
        host.save("line 1\nline 2\nline 3 host\nline 4\nline 5\n");
        guest.save("line 1\nline 2\nline 3 guest\nline 4\nline 5\n");
        host.flush_save();
        send(&host, &guest);
        guest.mirror().flush_remote().unwrap();
        exchange(&mut host, &mut guest);
        let merged = "line 1\nline 2\n<<<<<<< guest\nline 3 guest\n=======\nline 3 host\n>>>>>>> room\nline 4\nline 5\n";
        assert_eq!(guest.read(), merged);
        assert_eq!(host.read(), merged);
    }
}
