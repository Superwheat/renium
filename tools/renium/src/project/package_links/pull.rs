use super::*;
use crate::automation::reconcile::{SnapshotEntry, derived_project_path};
use crate::cli::{LinkApplyArgs, ProjectSourceArgs};
use crate::settings::bytecode::decode_settings_bytecode;
use crate::system::files::sha256_hex;

pub(crate) type StudioOverrides = BTreeMap<PathBuf, Option<SnapshotEntry>>;

/// Immutable package inputs for one pull. Applying these in the export stage
/// uses the ordinary identity/reference/placement checks without repacking a
/// source place, fetching a Git ref, or changing another service's targets.
pub(crate) struct PullLinkRestore {
    manifest: LinkManifest,
    src_dir: PathBuf,
    pub(crate) services: Vec<String>,
    inputs: Vec<(PathBuf, Option<String>)>,
    policy: PullRestorePolicy,
}

pub(super) struct PullRestorePolicy {
    shared_links: HashSet<String>,
    backup_stamp: String,
    reference_source: PathBuf,
    services: Vec<String>,
    references: std::sync::OnceLock<Result<OutsideReferences, String>>,
}

struct OutsideReferences {
    ids: HashSet<String>,
    inputs: Vec<(PathBuf, Option<String>)>,
}

impl PullRestorePolicy {
    pub(super) fn outside_references(&self) -> Result<&HashSet<String>> {
        self.references
            .get_or_init(|| {
                capture_outside_references(&self.reference_source, &self.services)
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map(|references| &references.ids)
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    pub(super) fn displaced_directory(&self, target: &ResolvedLinkTarget) -> Option<PathBuf> {
        self.shared_links.contains(&target.link_id).then(|| {
            let key = link_target_key(
                &target.service,
                &target.target_segments,
                &target.target_ordinals,
            );
            PathBuf::from(".renium/link-replaced")
                .join(&target.link_id)
                .join(&self.backup_stamp)
                .join(fnv1a_hex(key.as_bytes()))
        })
    }
}

impl PullLinkRestore {
    pub(crate) fn prepare(
        project_root: &Path,
        src_dir: &Path,
        staged_root: &Path,
        scratch: &Path,
        services: &[String],
    ) -> Result<Option<Self>> {
        let manifest_path = project_root.join("renium-link.json");
        let Some(manifest_bytes) = read_optional_file(&manifest_path)? else {
            return Ok(None);
        };
        let mut manifest = parse_link_manifest(std::str::from_utf8(&manifest_bytes)?)
            .with_context(|| format!("Invalid link manifest {}", manifest_path.display()))?;
        let broken = manifest
            .broken
            .iter()
            .map(link_target_ref_key)
            .collect::<HashSet<_>>();
        for link in &mut manifest.links {
            link.targets.retain(|target| {
                link.read_only
                    && services.contains(&target.service)
                    && !broken.contains(&link_target_ref_key(target))
            });
        }
        manifest.links.retain(|link| !link.targets.is_empty());
        if manifest.links.is_empty() {
            return Ok(None);
        }
        let options = LinkResolveOptions {
            cache_dir: resolve_link_cache_dir(project_root, &manifest, None),
            ..LinkResolveOptions::default()
        };
        let mut inputs = vec![(manifest_path, Some(sha256_hex(&manifest_bytes)))];
        let mut shared_links = HashSet::new();
        let mut packages = Vec::new();
        for mut link in manifest.links {
            let (source, is_dir, _) = resolve_link_source(project_root, &link.source, &options)?;
            // File/directory mirrors have their own watcher enforcement. This
            // restoration concerns serialized package subtrees.
            if is_dir || !is_package_path(&source) {
                continue;
            }
            if shared::experience_package_path(project_root, &source.to_string_lossy()).is_some() {
                shared_links.insert(link.id.clone());
            }
            let bytes = fs::read(&source)?;
            let destination = scratch
                .join("pull-packages")
                .join(format!("{}.renium", packages.len()));
            fs::create_dir_all(
                destination
                    .parent()
                    .context("Package snapshot has no parent")?,
            )?;
            fs::write(&destination, &bytes)?;
            inputs.push((source, Some(sha256_hex(&bytes))));
            link.source = LinkSource::Local {
                path: destination.to_string_lossy().into_owned(),
            };
            packages.push(link);
        }
        if packages.is_empty() {
            return Ok(None);
        }
        manifest.links = packages;
        let lock_path = link_lock_path(project_root);
        let lock_bytes = read_optional_file(&lock_path)?;
        if let Some(bytes) = &lock_bytes {
            let staged_lock = link_lock_path(staged_root);
            fs::create_dir_all(staged_lock.parent().context("Link lock has no parent")?)?;
            fs::write(staged_lock, bytes)?;
        }
        inputs.push((lock_path, lock_bytes.as_deref().map(sha256_hex)));
        Ok(Some(Self {
            manifest,
            src_dir: src_dir.to_path_buf(),
            services: services.to_vec(),
            inputs,
            policy: PullRestorePolicy {
                shared_links,
                backup_stamp: scratch
                    .file_name()
                    .context("Pull stage has no name")?
                    .to_string_lossy()
                    .into_owned(),
                reference_source: project_root.join(src_dir),
                services: services.to_vec(),
                references: std::sync::OnceLock::new(),
            },
        }))
    }

    pub(crate) fn publish_paths(&self) -> Vec<PathBuf> {
        std::iter::once(PathBuf::from(".renium/link.lock.json"))
            .chain(self.policy.shared_links.iter().map(|id| {
                PathBuf::from(".renium/link-replaced")
                    .join(id)
                    .join(&self.policy.backup_stamp)
            }))
            .collect()
    }

    pub(crate) fn apply(self, staged_root: &Path) -> Result<(Vec<Value>, StudioOverrides)> {
        ensure_inputs_unchanged(&self.inputs)?;
        let args = LinkApplyArgs {
            project: ProjectSourceArgs {
                project_root: staged_root.to_path_buf(),
                src_root: self.src_dir.clone(),
            },
            manifest: PathBuf::from("renium-link.json"),
            link: None,
            check: false,
            force_targets: true,
            force_target: Vec::new(),
            offline: true,
            strict: true,
            git_path: "git".into(),
            wally_path: "wally".into(),
            cache_dir: None,
            experience: false,
            shared_consumer: false,
            pretty: false,
        };
        let (result, warnings, overrides) = commands::apply_project_links_inner(
            &args,
            staged_root,
            &staged_root.join(&self.src_dir),
            &staged_root.join("renium-link.json"),
            self.manifest,
            Some(&self.policy),
        )?;
        if warnings != 0 {
            bail!("Could not restore read-only links: {}", result["warnings"]);
        }
        ensure_inputs_unchanged(&self.inputs)?;
        if let Some(Ok(references)) = self.policy.references.get() {
            ensure_inputs_unchanged(&references.inputs)?;
        }
        let restored = result["links"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("pullRestore").cloned())
            .collect();
        Ok((restored, overrides))
    }
}

/// Read and guard the same bytes. Unpulled stores are not published, but their
/// references still protect identities that a package replacement would delete.
fn capture_outside_references(src_root: &Path, services: &[String]) -> Result<OutsideReferences> {
    let mut documents = HashMap::new();
    let mut outside = Vec::new();
    let mut inputs = Vec::new();
    for path in collect_project_settings_files(src_root, std::iter::empty())? {
        let bytes = fs::read(&path)?;
        let document = decode_settings_bytecode(&bytes)?;
        if document
            .instances
            .first()
            .is_some_and(|root| !services.contains(&root.name))
        {
            inputs.push((path.clone(), Some(sha256_hex(&bytes))));
            outside.push(path.clone());
        }
        documents.insert(path, document);
    }
    canonicalize_loaded_settings_documents(&mut documents)?;
    Ok(OutsideReferences {
        ids: outside
            .into_iter()
            .flat_map(|path| referenced_settings_ids(&documents[&path]))
            .collect(),
        inputs,
    })
}

fn ensure_inputs_unchanged(inputs: &[(PathBuf, Option<String>)]) -> Result<()> {
    for (path, captured) in inputs {
        if &read_optional_file(path)?.as_deref().map(sha256_hex) != captured {
            bail!(
                "Link input changed during pull: {}. Retry the pull.",
                path.display()
            );
        }
    }
    Ok(())
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn snapshot_entry(path: &Path) -> Result<Option<SnapshotEntry>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(if metadata.file_type().is_symlink() {
        SnapshotEntry::Symlink {
            target: fs::read_link(path)?,
            directory: fs::metadata(path)?.is_dir(),
        }
    } else if metadata.is_dir() {
        SnapshotEntry::Directory
    } else {
        SnapshotEntry::File(fs::read(path)?)
    }))
}

/// Keep only the bytes restoration actually replaces, plus directory entries
/// it creates/prunes. The watcher accepts the final files; reconciliation must
/// still remember what Studio exported rather than claim it had these bytes.
pub(super) fn capture_restore_baseline(
    root: &Path,
    writes: &BTreeMap<PathBuf, Vec<u8>>,
    removals: &[PathBuf],
) -> Result<StudioOverrides> {
    let mut before = BTreeMap::new();
    for path in writes.keys().chain(removals) {
        let relative = path.strip_prefix(root)?;
        if derived_project_path(relative) {
            continue;
        }
        let entry = snapshot_entry(path)?;
        if writes
            .get(path)
            .is_some_and(|bytes| matches!(&entry, Some(SnapshotEntry::File(old)) if old == bytes))
        {
            continue;
        }
        before.insert(relative.to_path_buf(), entry);
        for parent in relative
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            if !before.contains_key(parent) {
                before.insert(parent.to_path_buf(), snapshot_entry(&root.join(parent))?);
            }
        }
    }
    Ok(before)
}

pub(super) fn changed_restore_baseline(
    root: &Path,
    before: StudioOverrides,
) -> Result<StudioOverrides> {
    let mut changed = BTreeMap::new();
    for (path, entry) in before {
        if snapshot_entry(&root.join(&path))? != entry {
            changed.insert(path, entry);
        }
    }
    Ok(changed)
}
