use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ProjectSnapshot, SnapshotEntry};
use crate::system::files::{atomic_write_file, sha256_hex};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredSnapshot {
    entries: BTreeMap<PathBuf, StoredEntry>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
enum StoredEntry {
    Directory,
    File(StoredFile),
    Symlink { target: PathBuf, directory: bool },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredFile {
    pack: String,
    offset: u64,
    length: u64,
    digest: [u8; 32],
}

impl StoredSnapshot {
    pub(super) fn write(root: &Path, key: &str, snapshot: &ProjectSnapshot) -> Result<Self> {
        Ok(Self {
            entries: store_entries(root, key, &snapshot.entries)?,
        })
    }

    pub(super) fn matches(&self, snapshot: &ProjectSnapshot) -> bool {
        self.entries.len() == snapshot.entries.len()
            && self.entries.iter().all(|(path, stored)| {
                snapshot
                    .entries
                    .get(path)
                    .is_some_and(|entry| stored.matches(entry))
            })
    }

    pub(super) fn load(&self, root: &Path, key: &str) -> Result<ProjectSnapshot> {
        self.load_selected(root, key, |_| true)
    }

    pub(super) fn load_scopes(
        &self,
        root: &Path,
        key: &str,
        scopes: &[PathBuf],
    ) -> Result<ProjectSnapshot> {
        self.load_selected(root, key, |path| {
            scopes.iter().any(|scope| path.starts_with(scope))
        })
    }

    pub(super) fn replace_scopes(
        &mut self,
        root: &Path,
        key: &str,
        scopes: &[PathBuf],
        current: &ProjectSnapshot,
    ) -> Result<()> {
        self.entries
            .retain(|path, _| !scopes.iter().any(|scope| path.starts_with(scope)));
        self.entries
            .extend(store_entries(root, key, &current.entries)?);
        Ok(())
    }

    pub(super) fn prune(&self, root: &Path, key: &str) -> Result<()> {
        let directory = pack_directory(root, key);
        let used = self
            .entries
            .values()
            .filter_map(|entry| match entry {
                StoredEntry::File(file) => Some(file.pack.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read {}", directory.display()));
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_file()
                && path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !used.contains(name))
            {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
        }
        Ok(())
    }

    pub(super) fn clear(root: &Path, key: &str) -> Result<()> {
        let directory = pack_directory(root, key)
            .parent()
            .context("Reconciliation pack directory has no parent")?
            .to_path_buf();
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("Failed to remove {}", directory.display()))
            }
        }
    }

    fn load_selected(
        &self,
        root: &Path,
        key: &str,
        selected: impl Fn(&Path) -> bool,
    ) -> Result<ProjectSnapshot> {
        let selected = self
            .entries
            .iter()
            .filter(|(path, _)| selected(path))
            .collect::<Vec<_>>();
        let packs = selected
            .iter()
            .filter_map(|(_, entry)| match entry {
                StoredEntry::File(file) => Some(file.pack.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|name| {
                let path = pack_directory(root, key).join(format!("{name}.bin"));
                let bytes = fs::read(&path).with_context(|| {
                    format!("Failed to read reconciliation pack {}", path.display())
                })?;
                if sha256_hex(&bytes) != name {
                    bail!("Reconciliation pack {} is corrupted", path.display());
                }
                Ok((name, bytes))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let entries = selected
            .into_iter()
            .map(|(path, entry)| Ok((path.clone(), entry.load(&packs)?)))
            .collect::<Result<_>>()?;
        Ok(ProjectSnapshot { entries })
    }
}

impl StoredEntry {
    fn matches(&self, entry: &SnapshotEntry) -> bool {
        match (self, entry) {
            (Self::Directory, SnapshotEntry::Directory) => true,
            (Self::File(stored), SnapshotEntry::File(bytes)) => {
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                usize::try_from(stored.length) == Ok(bytes.len()) && stored.digest == digest
            }
            (
                Self::Symlink {
                    target: left_target,
                    directory: left_directory,
                },
                SnapshotEntry::Symlink {
                    target: right_target,
                    directory: right_directory,
                },
            ) => left_target == right_target && left_directory == right_directory,
            _ => false,
        }
    }

    fn load(&self, packs: &HashMap<&str, Vec<u8>>) -> Result<SnapshotEntry> {
        match self {
            Self::Directory => Ok(SnapshotEntry::Directory),
            Self::Symlink { target, directory } => Ok(SnapshotEntry::Symlink {
                target: target.clone(),
                directory: *directory,
            }),
            Self::File(file) => {
                let pack = packs
                    .get(file.pack.as_str())
                    .context("Reconciliation pack is missing")?;
                let start = usize::try_from(file.offset)
                    .context("Reconciliation pack offset is too large")?;
                let length = usize::try_from(file.length)
                    .context("Reconciliation file length is too large")?;
                let end = start
                    .checked_add(length)
                    .filter(|end| *end <= pack.len())
                    .context("Reconciliation pack entry is out of bounds")?;
                let bytes = &pack[start..end];
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                if file.digest != digest {
                    bail!("Reconciliation file content is corrupted");
                }
                Ok(SnapshotEntry::File(bytes.to_vec()))
            }
        }
    }
}

fn store_entries(
    root: &Path,
    key: &str,
    entries: &BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<BTreeMap<PathBuf, StoredEntry>> {
    let size = entries
        .values()
        .filter_map(|entry| match entry {
            SnapshotEntry::File(bytes) => Some(bytes.len()),
            _ => None,
        })
        .sum();
    let mut pack = Vec::with_capacity(size);
    let mut files = Vec::new();
    let mut stored = BTreeMap::new();
    for (path, entry) in entries {
        match entry {
            SnapshotEntry::Directory => {
                stored.insert(path.clone(), StoredEntry::Directory);
            }
            SnapshotEntry::Symlink { target, directory } => {
                stored.insert(
                    path.clone(),
                    StoredEntry::Symlink {
                        target: target.clone(),
                        directory: *directory,
                    },
                );
            }
            SnapshotEntry::File(bytes) => {
                let offset =
                    u64::try_from(pack.len()).context("Reconciliation pack is too large")?;
                let length =
                    u64::try_from(bytes.len()).context("Reconciliation file is too large")?;
                let digest = Sha256::digest(bytes).into();
                pack.extend_from_slice(bytes);
                files.push((path.clone(), offset, length, digest));
            }
        }
    }
    if !files.is_empty() {
        let name = sha256_hex(&pack);
        let directory = pack_directory(root, key);
        let path = directory.join(format!("{name}.bin"));
        let pack_len = u64::try_from(pack.len()).context("Reconciliation pack is too large")?;
        if fs::metadata(&path).map_or(true, |metadata| metadata.len() != pack_len) {
            atomic_write_file(&path, &pack)?;
        }
        for (path, offset, length, digest) in files {
            stored.insert(
                path,
                StoredEntry::File(StoredFile {
                    pack: name.clone(),
                    offset,
                    length,
                    digest,
                }),
            );
        }
    }
    Ok(stored)
}

fn pack_directory(root: &Path, key: &str) -> PathBuf {
    root.join(".renium")
        .join("reconcile")
        .join(key)
        .join("packs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::temp_dir;

    #[test]
    fn stored_snapshot_round_trips_and_replaces_only_selected_scopes() {
        let root = temp_dir("reconcile-store");
        let key = "pair";
        let initial = ProjectSnapshot {
            entries: [
                (PathBuf::from("src"), SnapshotEntry::Directory),
                (
                    PathBuf::from("src/A.luau"),
                    SnapshotEntry::File(b"return 'a'".to_vec()),
                ),
                (
                    PathBuf::from("src/B.luau"),
                    SnapshotEntry::File(b"return 'b'".to_vec()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let mut stored = StoredSnapshot::write(&root, key, &initial).unwrap();
        assert!(stored.matches(&initial));
        assert!(stored.load(&root, key).unwrap() == initial);

        let scope = PathBuf::from("src/A.luau");
        let changed_scope = ProjectSnapshot {
            entries: [(
                scope.clone(),
                SnapshotEntry::File(b"return 'changed'".to_vec()),
            )]
            .into_iter()
            .collect(),
        };
        stored
            .replace_scopes(&root, key, std::slice::from_ref(&scope), &changed_scope)
            .unwrap();
        let mut expected = initial;
        expected.entries.extend(changed_scope.entries);
        assert!(stored.load(&root, key).unwrap() == expected);
        stored.prune(&root, key).unwrap();
        assert_eq!(fs::read_dir(pack_directory(&root, key)).unwrap().count(), 2);

        StoredSnapshot::clear(&root, key).unwrap();
        assert!(!pack_directory(&root, key).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
