//! Durable resource ownership. Expiry never makes an unclean resource reusable.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claim {
    pub resource: String,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lease {
    pub claim: Claim,
    pub plugin: String,
    pub session: String,
    pub workspace: PathBuf,
    pub data: Value,
}

pub struct Registry {
    root: PathBuf,
}

// A forked child can briefly retain the open file description even with CLOEXEC.
// Explicitly unlock when this operation ends, not when its last duplicate closes.
struct LockedFile(File);

impl std::ops::Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}

impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl Registry {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn user() -> Result<Self> {
        Ok(Self::new(super::plugin_home()?.join("leases")))
    }
    fn path(&self, resource: &str) -> Result<PathBuf> {
        if !super::valid_name(resource) {
            bail!("Invalid resource name");
        }
        Ok(self.root.join(format!("{resource}.json")))
    }
    pub fn read(&self, resource: &str) -> Result<Option<Lease>> {
        let path = self.path(resource)?;
        if !path.try_exists()? {
            return Ok(None);
        }
        let file = File::options().read(true).write(true).open(path)?;
        file.try_lock_shared()
            .context("Resource ownership is being updated; retry")?;
        let mut file = LockedFile(file);
        read(&mut file)
    }
    pub fn acquire(
        &self,
        resource: &str,
        plugin: &str,
        session: &str,
        workspace: &Path,
    ) -> Result<Lease> {
        if session.trim().is_empty() {
            bail!("Resource ownership requires a session");
        }
        self.edit(resource, |existing| {
            if let Some(existing) = existing {
                bail!(
                    "{} is held by session {}; release/clean it before reuse",
                    resource,
                    existing.session
                );
            }
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes)
                .map_err(|error| anyhow::anyhow!("Could not generate lease token: {error}"))?;
            let lease = Lease {
                claim: Claim {
                    resource: resource.into(),
                    token: bytes.iter().map(|b| format!("{b:02x}")).collect(),
                },
                plugin: plugin.into(),
                session: session.into(),
                workspace: fs::canonicalize(workspace)?,
                data: Value::Null,
            };
            Ok((Some(lease.clone()), lease))
        })
    }
    pub fn update(&self, claim: &Claim, data: Value) -> Result<()> {
        self.edit(&claim.resource, |existing| {
            let mut lease = owned(existing, claim)?;
            lease.data = data;
            Ok((Some(lease), ()))
        })
    }
    pub fn release(&self, claim: &Claim) -> Result<()> {
        self.edit(&claim.resource, |existing| {
            owned(existing, claim)?;
            Ok((None, ()))
        })
    }
    pub fn verify(&self, resource: &str, claim: Option<&Claim>) -> Result<()> {
        if claim.is_some_and(|claim| claim.resource != resource) {
            bail!("Resource lease cannot target a different resource");
        }
        match self.read(resource)? {
            Some(lease) if Some(&lease.claim) != claim => bail!(
                "Resource {resource} is owned by task {} through plugin {}; only that task may use it",
                lease.session,
                lease.plugin
            ),
            None if claim.is_some_and(|claim| claim.resource == resource) => {
                bail!("Resource lease expired or was released")
            }
            _ => Ok(()),
        }
    }
    fn edit<T>(
        &self,
        resource: &str,
        operation: impl FnOnce(Option<Lease>) -> Result<(Option<Lease>, T)>,
    ) -> Result<T> {
        fs::create_dir_all(&self.root)?;
        // Never unlink this lock file: replacing its inode would split concurrent ownership.
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path(resource)?)?;
        file.try_lock()
            .context("Resource ownership is being updated; retry")?;
        let mut file = LockedFile(file);
        let (lease, result) = operation(read(&mut file)?)?;
        let bytes = serde_json::to_vec(&lease)?;
        if bytes.len() > 1_048_576 {
            bail!("Resource record is too large");
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&bytes)?;
        file.set_len(bytes.len() as u64)?;
        file.sync_all()?;
        Ok(result)
    }
}

fn read(file: &mut File) -> Result<Option<Lease>> {
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > 1_048_576 {
        bail!("Resource record is too large");
    }
    serde_json::from_slice(&bytes)
        .context("Resource record is damaged; do not reuse it before recovery")
}

fn owned(existing: Option<Lease>, claim: &Claim) -> Result<Lease> {
    let lease = existing.context("Resource lease was released")?;
    if lease.claim != *claim {
        bail!("Resource lease is no longer owned by this caller");
    }
    Ok(lease)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn completed_operation_releases_lock_while_duplicate_is_open() -> Result<()> {
        let path = std::env::temp_dir().join(format!("renium-lease-lock-{}", std::process::id()));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.try_lock()?;
        let guard = LockedFile(file);
        let duplicate = guard.try_clone()?;
        drop(guard);
        let next = File::options().read(true).write(true).open(&path)?;
        let result = next.try_lock();
        drop(duplicate);
        drop(next);
        fs::remove_file(path)?;
        result.context("A completed operation retained its lock through a duplicate")
    }
}
