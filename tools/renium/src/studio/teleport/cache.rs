//! Immutable saved bytes and reusable preparation; authorization precedes lookup.
use std::fs::{self, File};
use std::path::{Path, PathBuf};
#[cfg(any(windows, test))]
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::payload::{self, Payload};
use crate::cloud::teleport::{Destination, MAX_BYTES, SavedVersion};
use crate::system::files::{OnDrop, atomic_write_file, replace_file_with_backup, sha256_hex};

const CAPACITY: u64 = 1024 * 1024 * 1024;
static PREPARATION: LazyLock<String> = LazyLock::new(|| {
    // Invalidate preparation on importer/dependency changes, retaining downloaded bytes.
    sha256_hex(
        concat!(
            include_str!("payload.rs"),
            include_str!("../../../Cargo.lock")
        )
        .as_bytes(),
    )
});

#[derive(Serialize, Deserialize)]
struct Prepared {
    format: String,
    raw_hash: String,
    payload_hash: String,
    manifest: Value,
}

pub(super) struct CachedPayload {
    pub payload: Payload,
    pub downloaded: bool,
    pub prepared_hit: bool,
}

/// The engine may retain its input after the initiating request has returned.
/// Own the bytes, rather than lending it an evictable cache filename.
#[cfg(any(windows, test))]
pub(super) struct SavedPlace {
    pub bytes: Arc<[u8]>,
    pub downloaded: bool,
}

struct Cache {
    root: PathBuf,
    capacity: u64,
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        path.metadata()?.len() <= MAX_BYTES,
        "Oversized teleport cache file"
    );
    Ok(fs::read(path)?)
}

fn prepared_read(entry: &Path, raw_hash: &str) -> Result<Payload> {
    let record: Prepared = serde_json::from_slice(&bounded_read(&entry.join("prepared.json"))?)?;
    ensure!(
        record.format == *PREPARATION && record.raw_hash == raw_hash,
        "Preparation changed"
    );
    let bytes = bounded_read(&entry.join("payload.rbxm"))?;
    ensure!(
        record.payload_hash == sha256_hex(&bytes),
        "Incomplete cached preparation"
    );
    Ok(Payload {
        bytes,
        manifest: record.manifest,
    })
}

fn place_key(saved: SavedVersion) -> String {
    format!("{}-{}", saved.universe, saved.place)
}

fn entry_key(saved: SavedVersion) -> String {
    format!("{}-{}", place_key(saved), saved.version)
}

impl Cache {
    fn with_saved<T>(
        &self,
        saved: SavedVersion,
        download: impl FnOnce(&Path) -> Result<()>,
        consume: impl FnOnce(&Path, &Path, &[u8], &str, bool) -> Result<T>,
    ) -> Result<T> {
        let locks = self.root.join("locks");
        fs::create_dir_all(&locks)?;
        let key = place_key(saved);
        // Stable files survive eviction. Separate places never wait on each other's downloads.
        let place_lock = open_lock(&locks.join(format!("{key}.lock")))?;
        place_lock.lock()?;
        let entry = self.root.join("entries").join(entry_key(saved));
        fs::create_dir_all(&entry)?;
        let _evict = OnDrop::new(|| {
            if let Err(error) = self.trim(&entry, &key) {
                crate::app::output::log_global(
                    2,
                    format_args!("[renium] teleport cache eviction failed: {error:#}"),
                );
            }
        });
        let raw = entry.join("place.rbxl");
        let expected = fs::read_to_string(entry.join("raw.sha256")).ok();
        let mut bytes = bounded_read(&raw).ok();
        let raw_hash = bytes.as_ref().map(|bytes| sha256_hex(bytes));
        let downloaded = raw_hash.is_none() || raw_hash != expected;
        if downloaded {
            let partial = entry.join("download.part");
            let _cleanup = OnDrop::new(|| {
                let _ = fs::remove_file(&partial);
            });
            download(&partial)?;
            let downloaded_bytes = bounded_read(&partial)?;
            ensure!(!downloaded_bytes.is_empty(), "Empty teleport download");
            replace_file_with_backup(&partial, &raw, "teleport download")?;
            atomic_write_file(
                &entry.join("raw.sha256"),
                sha256_hex(&downloaded_bytes).as_bytes(),
            )?;
            bytes = Some(downloaded_bytes);
        }
        let raw_hash = fs::read_to_string(entry.join("raw.sha256"))?;
        // Only a fully downloaded file becomes reusable; failed preparation retains it.
        File::create(entry.join("used"))?.set_modified(SystemTime::now())?;
        consume(
            &entry,
            &raw,
            bytes
                .as_deref()
                .expect("verified or downloaded saved bytes"),
            &raw_hash,
            downloaded,
        )
    }

    #[cfg(any(windows, test))]
    fn raw(
        &self,
        saved: SavedVersion,
        download: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<SavedPlace> {
        self.with_saved(saved, download, |_, _, bytes, _, downloaded| {
            Ok(SavedPlace {
                bytes: Arc::from(bytes),
                downloaded,
            })
        })
    }

    fn load(
        &self,
        saved: SavedVersion,
        download: impl FnOnce(&Path) -> Result<()>,
        prepare: impl FnOnce(&Path) -> Result<Payload>,
    ) -> Result<CachedPayload> {
        self.with_saved(saved, download, |entry, raw, _, raw_hash, downloaded| {
            let cached = prepared_read(entry, raw_hash).ok();
            let prepared_hit = cached.is_some();
            let payload = match cached {
                Some(payload) => payload,
                None => {
                    let payload = prepare(raw)?;
                    ensure!(
                        payload.bytes.len() as u64 <= MAX_BYTES,
                        "Teleport payload exceeds 128 MiB"
                    );
                    let record = serde_json::to_vec(&Prepared {
                        format: PREPARATION.clone(),
                        raw_hash: raw_hash.to_owned(),
                        payload_hash: sha256_hex(&payload.bytes),
                        manifest: payload.manifest.clone(),
                    })?;
                    ensure!(
                        record.len() as u64 <= MAX_BYTES,
                        "Teleport manifest exceeds 128 MiB"
                    );
                    let mut incomplete = OnDrop::new(|| {
                        let _ = fs::remove_file(entry.join("payload.rbxm"));
                        let _ = fs::remove_file(entry.join("prepared.json"));
                    });
                    atomic_write_file(&entry.join("payload.rbxm"), &payload.bytes)?;
                    atomic_write_file(&entry.join("prepared.json"), &record)?;
                    incomplete.disarm();
                    payload
                }
            };
            Ok(CachedPayload {
                payload,
                downloaded,
                prepared_hit,
            })
        })
    }

    fn trim(&self, current: &Path, held_place: &str) -> Result<()> {
        // This global lock covers directory accounting/removal only, never cloud or preparation.
        let sweep = open_lock(&self.root.join("eviction.lock"))?;
        sweep.lock()?;
        let mut entries = Vec::new();
        let mut total = 0u64;
        for item in fs::read_dir(self.root.join("entries"))? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            let ids = name.split('-').collect::<Vec<_>>();
            if !item.file_type()?.is_dir()
                || ids.len() != 3
                || ids.iter().any(|id| id.parse::<u64>().is_err())
            {
                continue;
            }
            let path = item.path();
            let mut size = 0;
            for file in fs::read_dir(&path)? {
                match file?.metadata() {
                    Ok(metadata) => size += metadata.len(),
                    // Another place can finish an atomic write while accounting runs.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let used = path
                .join("used")
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            total += size;
            entries.push((used, path, size, format!("{}-{}", ids[0], ids[1])));
        }
        entries.sort_by_key(|(used, _, _, _)| *used);
        for (_, path, size, place) in entries {
            if total <= self.capacity {
                break;
            }
            if path == current {
                continue;
            }
            let lock = open_lock(&self.root.join("locks").join(format!("{place}.lock")))?;
            if place != held_place && lock.try_lock().is_err() {
                continue;
            }
            fs::remove_dir_all(&path)?;
            total -= size;
        }
        if total > self.capacity {
            // Payload is already in memory. Do not delay travel behind busy entries;
            // retire this entry if everything older is pinned by another place.
            fs::remove_dir_all(current)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
pub(super) fn saved_place(destination: &Destination) -> Result<SavedPlace> {
    Cache {
        root: crate::app::update::user_data_dir()?
            .join("cache")
            .join("play-teleport"),
        capacity: CAPACITY,
    }
    .raw(destination.saved, |path| destination.download(path))
    .context("Cannot load saved teleport destination")
}

pub(super) fn load(destination: &Destination) -> Result<CachedPayload> {
    let cache = Cache {
        root: crate::app::update::user_data_dir()?
            .join("cache")
            .join("play-teleport"),
        capacity: CAPACITY,
    };
    cache
        .load(
            destination.saved,
            |path| destination.download(path),
            payload::read,
        )
        .context("Cannot prepare cached teleport content")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    fn saved(version: u64) -> SavedVersion {
        SavedVersion {
            universe: 7,
            place: 12,
            version,
        }
    }
    fn payload(_: &Path) -> Result<Payload> {
        Ok(Payload {
            bytes: vec![1; 32],
            manifest: serde_json::json!({"groups":[]}),
        })
    }
    fn directory() -> PathBuf {
        crate::tests::support::temp_dir("teleport-cache")
    }

    #[test]
    fn native_saved_bytes_survive_eviction_and_bypass_transplant_preparation() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let cache = Cache {
            root: root.clone(),
            capacity: CAPACITY,
        };
        assert!(
            cache
                .load(saved(10), download, |_| anyhow::bail!(
                    "Terrain is not a transplant payload"
                ))
                .is_err()
        );
        let first = cache
            .raw(saved(10), |_| panic!("completed download must be retained"))
            .unwrap();
        assert!(!first.downloaded);
        let in_flight = Arc::clone(&first.bytes);
        drop(first);
        let tiny = Cache {
            root: root.clone(),
            capacity: 1,
        };
        let second = tiny.raw(saved(11), download).unwrap();
        assert!(second.downloaded);
        assert!(!root.join("entries/7-12-10").exists());
        assert_eq!(&*in_flight, b"saved bytes");
        assert_eq!(&*second.bytes, b"saved bytes");
    }
    fn download(path: &Path) -> Result<()> {
        Ok(fs::write(path, b"saved bytes")?)
    }

    #[test]
    fn same_version_reuses_download_and_preparation_across_cache_instances() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let downloads = Cell::new(0);
        let preparations = Cell::new(0);
        for version in [10, 10, 11, 10] {
            let cache = Cache {
                root: root.clone(),
                capacity: CAPACITY,
            };
            cache
                .load(
                    saved(version),
                    |path| {
                        downloads.set(downloads.get() + 1);
                        download(path)
                    },
                    |path| {
                        preparations.set(preparations.get() + 1);
                        payload(path)
                    },
                )
                .unwrap();
        }
        assert_eq!(downloads.get(), 2);
        assert_eq!(preparations.get(), 2);
        for identity in [
            SavedVersion {
                universe: 8,
                ..saved(10)
            },
            SavedVersion {
                place: 13,
                ..saved(10)
            },
        ] {
            assert!(
                Cache {
                    root: root.clone(),
                    capacity: CAPACITY
                }
                .load(identity, download, payload)
                .unwrap()
                .downloaded
            );
        }
    }

    #[test]
    fn incomplete_downloads_never_hit_and_preparation_changes_keep_raw_bytes() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let cache = Cache {
            root: root.clone(),
            capacity: CAPACITY,
        };
        assert!(
            cache
                .load(
                    saved(10),
                    |path| {
                        download(path)?;
                        anyhow::bail!("interrupted")
                    },
                    payload
                )
                .is_err()
        );
        assert!(!root.join("entries/7-12-10/download.part").exists());
        assert!(
            cache
                .load(saved(10), download, |_| anyhow::bail!(
                    "unsupported content"
                ))
                .is_err()
        );
        let ready = cache
            .load(
                saved(10),
                |_| panic!("must reuse completed download"),
                payload,
            )
            .unwrap();
        assert!(!ready.downloaded && !ready.prepared_hit);
        let entry = root.join("entries/7-12-10");
        let mut record: Prepared =
            serde_json::from_slice(&fs::read(entry.join("prepared.json")).unwrap()).unwrap();
        record.format = "old importer".into();
        fs::write(
            entry.join("prepared.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        assert!(
            !cache
                .load(saved(10), |_| panic!("must not redownload"), payload)
                .unwrap()
                .prepared_hit
        );
        fs::write(entry.join("payload.rbxm"), b"corrupt").unwrap();
        assert!(
            !cache
                .load(saved(10), |_| panic!("must not redownload"), payload)
                .unwrap()
                .prepared_hit
        );
        fs::write(entry.join("place.rbxl"), b"corrupt").unwrap();
        assert!(cache.load(saved(10), download, payload).unwrap().downloaded);
    }

    #[test]
    fn lru_evicts_older_entries_and_keeps_the_recent_hit() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let cache = Cache {
            root: root.clone(),
            capacity: CAPACITY,
        };
        for version in [10, 11] {
            cache.load(saved(version), download, payload).unwrap();
        }
        File::options()
            .write(true)
            .open(root.join("entries/7-12-10/used"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        cache
            .load(saved(10), |_| panic!("cache hit"), payload)
            .unwrap();
        File::options()
            .write(true)
            .open(root.join("entries/7-12-11/used"))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        let size: u64 = fs::read_dir(root.join("entries/7-12-10"))
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        let cache = Cache {
            root: root.clone(),
            capacity: size * 2,
        };
        cache.load(saved(12), download, payload).unwrap();
        assert!(root.join("entries/7-12-10").exists());
        assert!(!root.join("entries/7-12-11").exists());
        assert!(root.join("entries/7-12-12").exists());
    }

    #[test]
    fn pinned_entries_survive_eviction_without_blocking_another_travel() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let cache = Cache {
            root: root.clone(),
            capacity: CAPACITY,
        };
        cache.load(saved(10), download, payload).unwrap();
        let entry = root.join("entries/7-12-10");
        let size: u64 = fs::read_dir(&entry)
            .unwrap()
            .map(|item| item.unwrap().metadata().unwrap().len())
            .sum();
        let pin = open_lock(&root.join("locks/7-12.lock")).unwrap();
        pin.lock().unwrap();
        let cache = Cache {
            root: root.clone(),
            capacity: size,
        };
        let loaded = cache
            .load(
                SavedVersion {
                    place: 13,
                    ..saved(10)
                },
                download,
                payload,
            )
            .unwrap();
        assert_eq!(loaded.payload.bytes.len(), 32);
        assert!(entry.exists());
        assert!(!root.join("entries/7-13-10").exists());
    }

    #[test]
    fn a_busy_download_does_not_block_another_place() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let (begun, started) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let busy_root = root.clone();
            scope.spawn(move || {
                Cache {
                    root: busy_root,
                    capacity: CAPACITY,
                }
                .load(
                    saved(10),
                    |path| {
                        begun.send(()).unwrap();
                        finished
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        download(path)
                    },
                    payload,
                )
                .unwrap();
            });
            started
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            scope.spawn(|| {
                Cache {
                    root: root.clone(),
                    capacity: CAPACITY,
                }
                .load(
                    SavedVersion {
                        place: 13,
                        ..saved(10)
                    },
                    download,
                    payload,
                )
                .unwrap();
                done.send(()).unwrap();
            });
            let independent = completed.recv_timeout(std::time::Duration::from_secs(3));
            finish.send(()).unwrap();
            assert!(independent.is_ok());
        });
    }

    #[test]
    fn simultaneous_same_version_downloads_are_coalesced() {
        let root = directory();
        let _cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&root);
        });
        let barrier = Arc::new(Barrier::new(2));
        let count = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let (barrier, count) = (Arc::clone(&barrier), Arc::clone(&count));
                let root = root.clone();
                scope.spawn(move || {
                    barrier.wait();
                    Cache {
                        root,
                        capacity: CAPACITY,
                    }
                    .load(
                        saved(10),
                        |path| {
                            count.fetch_add(1, Ordering::SeqCst);
                            download(path)
                        },
                        payload,
                    )
                    .unwrap();
                });
            }
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
