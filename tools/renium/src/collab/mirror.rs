use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use yrs::sync::Awareness;
use yrs::types::DeepObservable;
use yrs::types::{Event, PathSegment};
use yrs::{MapRef, Subscription, Transact};

use super::document::{self, Content};
use crate::project::config;
use crate::system::LockRecover;
use crate::system::files::{atomic_write_file, fnv1a};
use crate::system::watch::FileWatcher;

const SETTLE: Duration = Duration::from_millis(60);
const POLL: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct MirrorStats {
    pub(crate) files: AtomicU64,
    pub(crate) local_changes: AtomicU64,
    pub(crate) remote_changes: AtomicU64,
    pub(crate) error: Mutex<Option<String>>,
}

pub(crate) struct Mirror {
    root: PathBuf,
    awareness: Arc<Mutex<Awareness>>,
    files: MapRef,
    watched_files: BTreeSet<PathBuf>,
    watched_directories: BTreeSet<PathBuf>,
    watcher: FileWatcher,
    ledger: HashMap<String, u64>,
    dirty: Arc<Mutex<BTreeSet<String>>>,
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

fn path_for(root: &Path, key: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in key.split('/') {
        path.push(part);
    }
    path
}

fn project_file_present(root: &Path) -> bool {
    root.join("renium.project.jsonc").is_file() || root.join("renium.project.json").is_file()
}

impl Mirror {
    pub(crate) fn open(
        root: &Path,
        awareness: Arc<Mutex<Awareness>>,
        stats: Arc<MirrorStats>,
    ) -> Result<Mirror> {
        let (watched_files, watched_directories) = watch_inputs(root)?;
        let mut watcher = FileWatcher::new(4096)?;
        watcher.set_inputs(&watched_files, &watched_directories)?;
        let dirty = Arc::new(Mutex::new(BTreeSet::new()));
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
                    dirty.lock_recover().extend(keys);
                }
            });
            (files, subscription)
        };
        Ok(Mirror {
            root: root.to_path_buf(),
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
                self.ledger.insert(key.clone(), fnv1a(&content.bytes()));
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
            let bytes = content.bytes();
            let hash = fnv1a(&bytes);
            if disk
                .get(key)
                .is_some_and(|existing| fnv1a(existing) == hash)
            {
                self.ledger.insert(key.clone(), hash);
                continue;
            }
            self.write_file(key, &bytes)?;
            written += 1;
        }
        for key in disk.keys() {
            if !entries.contains_key(key) {
                let path = path_for(&self.root, key);
                let _ = std::fs::remove_file(&path);
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

    pub(crate) fn refresh_inputs(&mut self) -> Result<()> {
        if !project_file_present(&self.root) {
            return Ok(());
        }
        let (files, directories) = watch_inputs(&self.root)?;
        if files != self.watched_files || directories != self.watched_directories {
            self.watched_files = files;
            self.watched_directories = directories;
            self.watcher
                .set_inputs(&self.watched_files, &self.watched_directories)?;
        }
        Ok(())
    }

    fn write_file(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = path_for(&self.root, key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write_file(&path, bytes)?;
        self.ledger.insert(key.to_string(), fnv1a(bytes));
        Ok(())
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
        {
            let guard = self.awareness.lock_recover();
            let doc = guard.doc();
            let mut txn = document::transact_local(doc);
            let existing = document::snapshot(&txn, &self.files);
            for (key, bytes) in &disk {
                let hash = fnv1a(bytes);
                if self.ledger.get(key) == Some(&hash) {
                    continue;
                }
                let content = Content::from_bytes(Path::new(key), bytes.clone());
                if document::write_entry(&mut txn, &self.files, key, &content) {
                    changed += 1;
                }
                self.ledger.insert(key.clone(), hash);
            }
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

    fn flush_local(&mut self, paths: BTreeSet<PathBuf>) -> Result<()> {
        let mut project_changed = false;
        let mut changed = 0u64;
        let mut directories = Vec::new();
        for path in paths {
            if is_ignored(&path) {
                continue;
            }
            if path.is_dir() {
                directories.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) == Some("renium.project.jsonc") {
                project_changed = true;
            }
            let Some(key) = key_for(&self.root, &path) else {
                continue;
            };
            if path.is_file() {
                let bytes = read_settled(&path)?;
                let hash = fnv1a(&bytes);
                if self.ledger.get(&key) == Some(&hash) {
                    continue;
                }
                let content = Content::from_bytes(&path, bytes);
                let guard = self.awareness.lock_recover();
                let mut txn = document::transact_local(guard.doc());
                if document::write_entry(&mut txn, &self.files, &key, &content) {
                    changed += 1;
                }
                self.ledger.insert(key, hash);
            } else if self.ledger.remove(&key).is_some() {
                let guard = self.awareness.lock_recover();
                let mut txn = document::transact_local(guard.doc());
                if document::remove_entry(&mut txn, &self.files, &key) {
                    changed += 1;
                }
            }
        }
        if project_changed {
            self.refresh_inputs()?;
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
            keys.iter()
                .map(|key| (key.clone(), document::read_entry(&txn, &self.files, key)))
                .collect::<Vec<_>>()
        };
        let mut changed = 0u64;
        let mut project_changed = false;
        for (key, content) in entries {
            match content {
                Some(content) => {
                    let bytes = content.bytes();
                    let hash = fnv1a(&bytes);
                    if self.ledger.get(&key) == Some(&hash) {
                        continue;
                    }
                    self.write_file(&key, &bytes)?;
                    changed += 1;
                }
                None => {
                    let path = path_for(&self.root, &key);
                    if path.is_file() {
                        std::fs::remove_file(&path)?;
                        changed += 1;
                    }
                    self.ledger.remove(&key);
                }
            }
            if key == "renium.project.jsonc" {
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
    Ok((files, directories))
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
            PathBuf::from("E:/proj/src/a/b.luau")
        );
    }

    #[test]
    fn ignores_metadata_folders_and_lock_files() {
        assert!(is_ignored(Path::new("p/.git/HEAD")));
        assert!(is_ignored(Path::new("p/.renium/x")));
        assert!(is_ignored(Path::new("p/src/a.renium.lock")));
        assert!(!is_ignored(Path::new("p/src/a.renium")));
        assert!(!is_ignored(Path::new("p/src/a.luau")));
    }
}
