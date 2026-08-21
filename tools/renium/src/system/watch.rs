use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

pub(crate) struct FileWatcher {
    watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Result<Event>>,
    overflowed: Arc<AtomicBool>,
    roots: BTreeMap<PathBuf, bool>,
}

impl FileWatcher {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = Arc::clone(&overflowed);
        let watcher = notify::recommended_watcher(move |event| match sender.try_send(event) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                callback_overflowed.store(true, Ordering::Release);
            }
        })?;
        Ok(Self {
            watcher,
            receiver,
            overflowed,
            roots: BTreeMap::new(),
        })
    }

    pub(crate) fn set_inputs(
        &mut self,
        files: &BTreeSet<PathBuf>,
        directories: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        let mut roots = BTreeMap::new();
        for (input, recursive) in files
            .iter()
            .map(|path| (path.parent().unwrap_or(path.as_path()), false))
            .chain(directories.iter().map(|path| (path.as_path(), true)))
        {
            let mut root = input.to_path_buf();
            while !root.exists() {
                let Some(parent) = root.parent() else {
                    break;
                };
                root = parent.to_path_buf();
            }
            if !root.exists() {
                continue;
            }
            let recursive = recursive || root != input;
            roots
                .entry(root)
                .and_modify(|current| *current |= recursive)
                .or_insert(recursive);
        }
        let covered = roots
            .keys()
            .filter(|root| {
                roots.iter().any(|(ancestor, recursive)| {
                    *recursive && ancestor != *root && root.starts_with(ancestor)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        for root in covered {
            roots.remove(&root);
        }
        for (root, recursive) in &self.roots {
            if roots.get(root) != Some(recursive) {
                let _ = self.watcher.unwatch(root);
            }
        }
        self.roots
            .retain(|root, recursive| roots.get(root) == Some(recursive));
        for (root, recursive) in &roots {
            if self.roots.get(root) == Some(recursive) {
                continue;
            }
            self.watcher
                .watch(
                    root,
                    if *recursive {
                        RecursiveMode::Recursive
                    } else {
                        RecursiveMode::NonRecursive
                    },
                )
                .with_context(|| format!("Failed to watch {}", root.display()))?;
            self.roots.insert(root.clone(), *recursive);
        }
        Ok(())
    }

    pub(crate) fn receiver(&self) -> &mpsc::Receiver<notify::Result<Event>> {
        &self.receiver
    }

    pub(crate) fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }
}
