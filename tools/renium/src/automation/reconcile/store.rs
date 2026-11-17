use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ProjectSnapshot, ScopeSet, SnapshotEntry};
use crate::system::files::{atomic_write_file, sha256_hex};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredSnapshot {
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
    pub(super) fn migrate_store_paths(&mut self, root: &Path) -> Result<bool> {
        let moves = self
            .entries
            .keys()
            .filter_map(|old| {
                crate::project::storage::relocated_legacy_path(root, old)
                    .map(|new| (old.clone(), new))
            })
            .collect::<Vec<_>>();
        for (old, new) in &moves {
            if let Some(existing) = self.entries.get(new) {
                let same = match (&self.entries[old], existing) {
                    (StoredEntry::File(left), StoredEntry::File(right)) => {
                        left.length == right.length && left.digest == right.digest
                    }
                    (left, right) => left == right,
                };
                if !same {
                    bail!(
                        "Reconciliation baseline contains conflicting old and new stores: {}",
                        new.display()
                    );
                }
            }
        }
        for (old, new) in &moves {
            let value = self
                .entries
                .remove(old)
                .context("Migrated baseline entry disappeared")?;
            self.entries.insert(new.clone(), value);
        }
        Ok(!moves.is_empty())
    }

    pub(super) fn write(root: &Path, key: &str, snapshot: &ProjectSnapshot) -> Result<Self> {
        Self::write_at(&pack_directory(root, key), snapshot.entries.iter())
    }

    pub(super) fn write_at<'a>(
        directory: &Path,
        entries: impl Iterator<Item = (&'a PathBuf, &'a SnapshotEntry)>,
    ) -> Result<Self> {
        let _span = crate::app::profile::span("last-sync copy: write");
        Ok(Self {
            entries: store_entries(directory, entries)?,
        })
    }

    pub(super) fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    pub(super) fn matches(&self, snapshot: &ProjectSnapshot) -> bool {
        let _span = crate::app::profile::span("last-sync copy: compare with a snapshot");
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
        let scopes = ScopeSet::new(scopes);
        self.load_selected(root, key, |path| scopes.contains(path))
    }

    pub(super) fn replace_scopes(
        &mut self,
        root: &Path,
        key: &str,
        scopes: &[PathBuf],
        current: &ProjectSnapshot,
    ) -> Result<()> {
        let _span = crate::app::profile::span("last-sync copy: replace part");
        let scope_set = ScopeSet::new(scopes);
        self.entries.retain(|path, _| !scope_set.contains(path));
        self.entries.extend(store_entries(
            &pack_directory(root, key),
            current.entries.iter(),
        )?);
        Ok(())
    }

    pub(super) fn packs_present(&self, root: &Path, key: &str) -> bool {
        let directory = pack_directory(root, key);
        self.entries
            .values()
            .filter_map(|entry| match entry {
                StoredEntry::File(file) => Some(file.pack.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .all(|name| directory.join(format!("{name}.bin")).is_file())
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
        let _span = crate::app::profile::span("last-sync copy: read");
        self.load_at(&pack_directory(root, key), selected)
    }

    pub(super) fn load_at(
        &self,
        directory: &Path,
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
                if name.len() != 64 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    bail!("Invalid snapshot pack name");
                }
                let path = directory.join(format!("{name}.bin"));
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

fn store_entries<'a>(
    directory: &Path,
    entries: impl Iterator<Item = (&'a PathBuf, &'a SnapshotEntry)>,
) -> Result<BTreeMap<PathBuf, StoredEntry>> {
    let entries = entries.collect::<Vec<_>>();
    let size = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
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
    fn migrated_baseline_preserves_original_bytes_for_three_way_reconciliation() -> Result<()> {
        let root = temp_dir("baseline-store-migration");
        let _cleanup = crate::system::files::OnDrop::new(|| {
            crate::project::storage::forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        fs::write(
            root.join("renium.project.jsonc"),
            br#"{"schemaVersion":1,"sourceRoot":"code/scripts"}"#,
        )?;
        crate::project::config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let old = PathBuf::from("code/scripts/Workspace/__roblox_sync_settings.renium");
        let new = PathBuf::from("instances/Workspace.renium");
        let mut stored = StoredSnapshot::write(
            &root,
            "pair",
            &ProjectSnapshot {
                entries: BTreeMap::from([(old.clone(), SnapshotEntry::File(b"baseline".to_vec()))]),
            },
        )?;
        assert!(stored.migrate_store_paths(&root)?);
        assert!(!stored.migrate_store_paths(&root)?);
        let loaded = stored.load(&root, "pair")?;
        assert!(!loaded.entries.contains_key(&old));
        assert!(loaded.entries.get(&new) == Some(&SnapshotEntry::File(b"baseline".to_vec())));
        Ok(())
    }

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

    fn snapshot(entries: &[(&str, SnapshotEntry)]) -> ProjectSnapshot {
        ProjectSnapshot {
            entries: entries
                .iter()
                .map(|(path, entry)| (PathBuf::from(path), entry.clone()))
                .collect(),
        }
    }

    fn file(bytes: &[u8]) -> SnapshotEntry {
        SnapshotEntry::File(bytes.to_vec())
    }

    fn link(target: &str, directory: bool) -> SnapshotEntry {
        SnapshotEntry::Symlink {
            target: PathBuf::from(target),
            directory,
        }
    }

    #[test]
    fn a_stored_snapshot_matches_only_the_snapshot_it_holds() {
        let root = temp_dir("reconcile-store-matches");
        let held = [
            ("src", SnapshotEntry::Directory),
            ("src/A.luau", file(b"return 'a'")),
            ("src/Shared", link("../shared", true)),
        ];
        let stored = StoredSnapshot::write(&root, "pair", &snapshot(&held)).unwrap();
        assert!(stored.matches(&snapshot(&held)));
        assert!(stored.contains(Path::new("src/A.luau")));
        assert!(!stored.contains(Path::new("src/B.luau")));
        let with = |index: usize, path: &'static str, entry: SnapshotEntry| {
            let mut other = held.clone();
            other[index] = (path, entry);
            snapshot(&other)
        };
        for (what, other) in [
            (
                "other bytes of the same length",
                with(1, "src/A.luau", file(b"return 'b'")),
            ),
            (
                "the same file under another name",
                with(1, "src/B.luau", file(b"return 'a'")),
            ),
            (
                "a folder for the file",
                with(1, "src/A.luau", SnapshotEntry::Directory),
            ),
            ("a file for the folder", with(0, "src", file(b""))),
            (
                "another link target",
                with(2, "src/Shared", link("../other", true)),
            ),
            (
                "a file link for the folder link",
                with(2, "src/Shared", link("../shared", false)),
            ),
            ("one entry fewer", snapshot(&held[..2])),
            (
                "one entry more",
                snapshot(&[held.as_slice(), &[("src/B.luau", file(b""))]].concat()),
            ),
        ] {
            assert!(!stored.matches(&other), "{what}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pruning_removes_the_packs_no_entry_uses_and_nothing_else() {
        let root = temp_dir("reconcile-store-prune");
        let key = "pair";
        let mut stored =
            StoredSnapshot::write(&root, key, &snapshot(&[("src/A.luau", file(b"first"))]))
                .unwrap();
        // Nothing was written for this pair yet.
        stored.prune(&root, "other").unwrap();
        StoredSnapshot::clear(&root, "other").unwrap();
        let current = snapshot(&[("src/A.luau", file(b"second"))]);
        stored
            .replace_scopes(&root, key, &[PathBuf::from("src")], &current)
            .unwrap();
        assert_eq!(fs::read_dir(pack_directory(&root, key)).unwrap().count(), 2);
        stored.prune(&root, key).unwrap();
        assert_eq!(fs::read_dir(pack_directory(&root, key)).unwrap().count(), 1);
        assert!(stored.load(&root, key).unwrap() == current);
        // A pack folder that cannot be listed is an error, not an empty one.
        let blocked = pack_directory(&root, "blocked");
        fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        fs::write(&blocked, b"").unwrap();
        assert!(stored.prune(&root, "blocked").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_scope_loads_what_lies_under_it_and_needs_its_pack_on_disk() {
        let root = temp_dir("reconcile-store-scopes");
        let held = snapshot(&[
            ("src", SnapshotEntry::Directory),
            ("src/A.luau", file(b"return 'a'")),
            ("src/B.luau", file(b"return 'b'")),
            ("notes.txt", file(b"notes")),
        ]);
        let stored = StoredSnapshot::write(&root, "pair", &held).unwrap();
        let scoped = |scope: &str| {
            stored
                .load_scopes(&root, "pair", &[PathBuf::from(scope)])
                .unwrap()
        };
        assert!(scoped("src/A.luau") == snapshot(&[("src/A.luau", file(b"return 'a'"))]));
        let mut under_src = held.clone();
        under_src.entries.remove(Path::new("notes.txt"));
        assert!(scoped("src") == under_src);
        assert!(stored.packs_present(&root, "pair"));
        assert!(!stored.packs_present(&root, "other"));
        let folders = snapshot(&[("src", SnapshotEntry::Directory)]);
        let folders = StoredSnapshot::write(&root, "folders", &folders).unwrap();
        assert!(folders.packs_present(&root, "folders"));
        for pack in fs::read_dir(pack_directory(&root, "pair")).unwrap() {
            fs::remove_file(pack.unwrap().path()).unwrap();
        }
        assert!(!stored.packs_present(&root, "pair"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pack_cut_short_on_disk_is_written_again() {
        let root = temp_dir("reconcile-store-rewrite");
        let held = snapshot(&[("src/A.luau", file(b"return 'a'"))]);
        StoredSnapshot::write(&root, "pair", &held).unwrap();
        let pack = fs::read_dir(pack_directory(&root, "pair"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(&pack, b"return").unwrap();
        let stored = StoredSnapshot::write(&root, "pair", &held).unwrap();
        assert!(stored.load(&root, "pair").unwrap() == held);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pack_name_that_is_no_digest_is_never_opened() {
        let root = temp_dir("reconcile-store-pack-name");
        for pack in ["ab".to_string(), "z".repeat(64), "../".repeat(21) + "x"] {
            let stored = StoredSnapshot {
                entries: BTreeMap::from([(
                    PathBuf::from("src/A.luau"),
                    StoredEntry::File(StoredFile {
                        pack,
                        offset: 0,
                        length: 0,
                        digest: [0; 32],
                    }),
                )]),
            };
            let error = stored.load(&root, "pair").err().unwrap().to_string();
            assert_eq!(error, "Invalid snapshot pack name");
        }
    }

    #[test]
    fn a_migration_onto_an_existing_store_needs_both_to_hold_the_same() -> Result<()> {
        let root = temp_dir("baseline-store-migration-conflict");
        let _cleanup = crate::system::files::OnDrop::new(|| {
            crate::project::storage::forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        fs::write(
            root.join("renium.project.jsonc"),
            br#"{"schemaVersion":1,"sourceRoot":"code/scripts"}"#,
        )?;
        crate::project::config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let old = "code/scripts/Workspace/__roblox_sync_settings.renium";
        let new = "instances/Workspace.renium";
        let migrate = |old_entry: SnapshotEntry, new_entry: SnapshotEntry| {
            let both = snapshot(&[
                ("pad.txt", file(b"pad")),
                (old, old_entry),
                (new, new_entry),
            ]);
            let mut stored = StoredSnapshot::write(&root, "pair", &both)?;
            stored.migrate_store_paths(&root)?;
            stored.load(&root, "pair")
        };
        let merged = migrate(file(b"baseline"), file(b"baseline"))?;
        assert!(!merged.entries.contains_key(Path::new(old)));
        assert!(merged.entries.get(Path::new(new)) == Some(&file(b"baseline")));
        for (old_entry, new_entry) in [
            (file(b"baseline"), file(b"baselinE")),
            (file(b"baseline"), file(b"longer baseline")),
            (file(b"baseline"), SnapshotEntry::Directory),
            (link("a", false), link("b", false)),
        ] {
            let error = migrate(old_entry, new_entry).err().unwrap().to_string();
            assert!(error.contains("conflicting old and new stores"), "{error}");
        }
        assert!(migrate(link("a", false), link("a", false)).is_ok());
        Ok(())
    }
}
