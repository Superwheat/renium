use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};

use super::{
    DEFAULT_UPDATE_MANIFEST, SignedUpdateManifest, fetch_manifest, install_bytes,
    lifecycle_lock_owner_is_alive, parse_lifecycle_lock_owner, parse_manifest,
    process_start_identity, user_data_dir, verify_manifest,
};
use crate::app::timing::current_millis;

const CACHE_INTERVAL_MS: u128 = 5 * 60 * 1000;
const LOCK_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cache {
    schema_version: u32,
    checked_at_unix_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest: Option<SignedUpdateManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

enum ManifestResponse {
    Modified {
        manifest: SignedUpdateManifest,
        etag: Option<String>,
    },
    NotModified,
}

struct Lock {
    path: PathBuf,
    token: String,
}

impl Drop for Lock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path).is_ok_and(|value| value.trim() == self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(10))
            .build()
    })
}

fn read_response(response: ureq::Response, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read {label}"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        bail!("{label} exceeds {MAX_RESPONSE_BYTES} bytes");
    }
    Ok(bytes)
}

fn fetch_https_manifest(source: &str, etag: Option<&str>) -> Result<ManifestResponse> {
    let mut request = agent()
        .get(source)
        .set("User-Agent", concat!("Renium/", env!("CARGO_PKG_VERSION")));
    if let Some(etag) = etag {
        request = request.set("If-None-Match", etag);
    }
    let response = match request.call() {
        Ok(response) if response.status() == 304 => {
            return Ok(ManifestResponse::NotModified);
        }
        Ok(response) => response,
        Err(ureq::Error::Status(304, _)) => return Ok(ManifestResponse::NotModified),
        Err(ureq::Error::Status(status, _)) => {
            bail!("GitHub release check returned HTTP {status}")
        }
        Err(ureq::Error::Transport(error)) => {
            return Err(error).context("GitHub release check failed");
        }
    };
    let etag = response.header("ETag").map(str::to_owned);
    let manifest = parse_manifest(&read_response(response, "update manifest")?)?;
    Ok(ManifestResponse::Modified { manifest, etag })
}

fn cache_path() -> Result<PathBuf> {
    Ok(user_data_dir()?.join("update-check.json"))
}

fn read_cache() -> Option<Cache> {
    let cache: Cache = serde_json::from_slice(&fs::read(cache_path().ok()?).ok()?).ok()?;
    if !matches!(cache.schema_version, 1 | 2)
        || cache
            .manifest
            .as_ref()
            .is_some_and(|manifest| verify_manifest(manifest).is_err())
        || cache.manifest.is_none() && cache.error.is_none()
    {
        return None;
    }
    if let Some(manifest) = cache.manifest.as_ref() {
        Version::parse(&manifest.payload.version).ok()?;
    }
    Some(cache)
}

fn is_fresh(cache: &Cache) -> bool {
    let now = current_millis();
    cache.checked_at_unix_ms <= now && now - cache.checked_at_unix_ms < CACHE_INTERVAL_MS
}

fn write_cache(cache: &Cache) -> Result<()> {
    install_bytes(&cache_path()?, &serde_json::to_vec(cache)?)
}

fn cached_manifest(cache: Cache) -> Result<SignedUpdateManifest> {
    cache.manifest.with_context(|| {
        cache
            .error
            .unwrap_or_else(|| "The last Renium update check failed".to_string())
    })
}

fn acquire_lock() -> Result<Lock> {
    let root = user_data_dir()?;
    fs::create_dir_all(&root)?;
    let path = root.join("update-check.lock");
    let start = process_start_identity(std::process::id())
        .context("Could not read this process's start identity")?;
    let token = format!("{}\t{}\t{}", std::process::id(), start, current_millis());
    let temporary = root.join(format!(
        ".update-check.lock.{}.{}.tmp",
        std::process::id(),
        current_millis()
    ));
    fs::write(&temporary, &token)?;
    let temporary_cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_file(&temporary);
    });
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                drop(temporary_cleanup);
                return Ok(Lock { path, token });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(&path).ok();
                let alive = holder
                    .as_deref()
                    .and_then(parse_lifecycle_lock_owner)
                    .is_some_and(|owner| lifecycle_lock_owner_is_alive(&owner));
                if !alive
                    && holder.as_deref().map(str::trim)
                        == fs::read_to_string(&path).ok().as_deref().map(str::trim)
                {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("Another Renium update check is still running");
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to create {}", path.display()));
            }
        }
    }
}

pub(super) fn manifest(source: &str) -> Result<SignedUpdateManifest> {
    if source != DEFAULT_UPDATE_MANIFEST {
        let manifest = fetch_manifest(source)?;
        verify_manifest(&manifest)?;
        return Ok(manifest);
    }
    if let Some(cache) = read_cache().filter(is_fresh) {
        return cached_manifest(cache);
    }
    let _lock = acquire_lock()?;
    let previous = match read_cache() {
        Some(cache) if is_fresh(&cache) => return cached_manifest(cache),
        cache => cache,
    };
    let response = fetch_https_manifest(
        DEFAULT_UPDATE_MANIFEST,
        previous.as_ref().and_then(|cache| cache.etag.as_deref()),
    );
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let cache = Cache {
                schema_version: 2,
                checked_at_unix_ms: current_millis(),
                etag: previous.as_ref().and_then(|cache| cache.etag.clone()),
                manifest: previous.and_then(|cache| cache.manifest),
                error: Some(format!("{error:#}")),
            };
            if let Err(cache_error) = write_cache(&cache) {
                eprintln!("[renium] warning: failed to cache update check: {cache_error:#}");
            }
            return cached_manifest(cache);
        }
    };
    let (manifest, etag) = match response {
        ManifestResponse::Modified { manifest, etag } => {
            verify_manifest(&manifest)?;
            (manifest, etag)
        }
        ManifestResponse::NotModified => {
            let cache = previous.context("GitHub returned 304 without a cached update manifest")?;
            (
                cache
                    .manifest
                    .context("GitHub returned 304 without a cached update manifest")?,
                cache.etag,
            )
        }
    };
    let cache = Cache {
        schema_version: 2,
        checked_at_unix_ms: current_millis(),
        etag,
        manifest: Some(manifest),
        error: None,
    };
    if let Err(error) = write_cache(&cache) {
        eprintln!("[renium] warning: failed to cache update check: {error:#}");
    }
    cached_manifest(cache)
}
