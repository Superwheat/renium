use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ProjectSnapshot, ScopeSet, SnapshotEntry};
use crate::system::files::{atomic_write_parts, sha256_hex};

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

    /// What says whether files are those of `entries`, without their bytes:
    /// nothing loads from it.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pub(super) fn digests<'a>(
        entries: impl Iterator<Item = (&'a PathBuf, &'a SnapshotEntry)>,
    ) -> Self {
        use rayon::prelude::*;
        let entries = entries.collect::<Vec<_>>();
        Self {
            entries: entries
                .into_par_iter()
                .map(|(path, entry)| {
                    let stored = match entry {
                        SnapshotEntry::Directory => StoredEntry::Directory,
                        SnapshotEntry::Symlink { target, directory } => StoredEntry::Symlink {
                            target: target.clone(),
                            directory: *directory,
                        },
                        SnapshotEntry::File(bytes) => StoredEntry::File(StoredFile {
                            pack: String::new(),
                            offset: 0,
                            length: bytes.len() as u64,
                            digest: Sha256::digest(bytes).into(),
                        }),
                    };
                    (path.clone(), stored)
                })
                .collect(),
        }
    }

    pub(super) fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    pub(super) fn matches(&self, snapshot: &ProjectSnapshot) -> bool {
        use rayon::prelude::*;
        let _span = crate::app::profile::span("last-sync copy: compare with a snapshot");
        self.entries.len() == snapshot.entries.len()
            && self.entries.par_iter().all(|(path, stored)| {
                snapshot
                    .entries
                    .get(path)
                    .is_some_and(|entry| stored.matches(entry))
            })
    }

    /// Whether this copy holds what `snapshot` holds under `scopes`.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pub(super) fn holds_within(&self, scopes: &[PathBuf], snapshot: &ProjectSnapshot) -> bool {
        let scopes = ScopeSet::new(scopes);
        let within = |path: &&PathBuf| scopes.contains(path);
        self.entries.keys().filter(within).count() == snapshot.entries.keys().filter(within).count()
            && (self.entries.iter())
                .filter(|(path, _)| within(path))
                .all(|(path, stored)| {
                    (snapshot.entries.get(path)).is_some_and(|entry| stored.matches(entry))
                })
    }

    /// The paths at which `snapshot` holds other than this copy, in order.
    pub(super) fn moved_paths(&self, snapshot: &ProjectSnapshot) -> Vec<PathBuf> {
        use rayon::prelude::*;
        let _span = crate::app::profile::span("last-sync copy: paths moved from it");
        let paths = (self.entries.keys().chain(snapshot.entries.keys()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        paths
            .into_par_iter()
            .filter(
                |path| match (self.entries.get(*path), snapshot.entries.get(*path)) {
                    (Some(stored), Some(entry)) => !stored.matches(entry),
                    (stored, entry) => stored.is_some() || entry.is_some(),
                },
            )
            .cloned()
            .collect()
    }

    /// A process can stop between atomic file replacements. Every path must
    /// still be one of the two journaled states; an unrelated edit never fits.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pub(super) fn matches_partial_publication(
        &self,
        accepted: &Self,
        snapshot: &ProjectSnapshot,
    ) -> bool {
        let paths = self
            .entries
            .keys()
            .chain(accepted.entries.keys())
            .chain(snapshot.entries.keys())
            .collect::<std::collections::BTreeSet<_>>();
        paths.into_iter().all(|path| {
            let current = snapshot.entries.get(path);
            let matches = |stored: Option<&StoredEntry>| match (stored, current) {
                (None, None) => true,
                (Some(stored), Some(current)) => stored.matches(current),
                _ => false,
            };
            matches(self.entries.get(path)) || matches(accepted.entries.get(path))
        })
    }

    /// Whether `snapshot`, a read of `path` alone, holds what this holds at
    /// `path` and under it: nothing where this holds nothing. Paths order by
    /// their components, so those under one follow it.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pub(super) fn matches_at(&self, path: &Path, snapshot: &ProjectSnapshot) -> bool {
        use std::ops::Bound;
        let mut stored = self
            .entries
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .take_while(|(entry, _)| entry.starts_with(path));
        let mut held = 0;
        stored.all(|(entry, stored)| {
            held += 1;
            snapshot
                .entries
                .get(entry)
                .is_some_and(|current| stored.matches(current))
        }) && held == snapshot.entries.len()
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
    use rayon::prelude::*;
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
            SnapshotEntry::File(bytes) => files.push((path, bytes.as_slice())),
        }
    }
    if files.is_empty() {
        return Ok(stored);
    }
    // The pack holds the files one after another and is named by its
    // digest, taken beside the digest of each file.
    let (name, digests) = rayon::join(
        || {
            let mut pack = Sha256::new();
            for (_, bytes) in &files {
                pack.update(bytes);
            }
            format!("{:x}", pack.finalize())
        },
        || {
            (files.par_iter())
                .map(|(_, bytes)| <[u8; 32]>::from(Sha256::digest(bytes)))
                .collect::<Vec<_>>()
        },
    );
    let path = directory.join(format!("{name}.bin"));
    let pack_len = files.iter().map(|(_, bytes)| bytes.len()).sum::<usize>();
    let pack_len = u64::try_from(pack_len).context("Reconciliation pack is too large")?;
    if fs::metadata(&path).map_or(true, |metadata| metadata.len() != pack_len) {
        atomic_write_parts(&path, files.iter().map(|(_, bytes)| *bytes))?;
    }
    let mut offset = 0;
    for ((path, bytes), digest) in files.into_iter().zip(digests) {
        let length = u64::try_from(bytes.len()).context("Reconciliation file is too large")?;
        stored.insert(
            path.clone(),
            StoredEntry::File(StoredFile {
                pack: name.clone(),
                offset,
                length,
                digest,
            }),
        );
        offset += length;
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
