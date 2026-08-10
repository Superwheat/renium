use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::bytecode_edit::{
    BytecodeCloneRefMap, CloneRefMapInput, build_clone_ref_map, collect_settings_subtree_preorder,
    instance_path_parts_key, next_editor_settings_id_fast, plan_editor_source_file_removals,
    ref_old_index, remap_internal_clone_refs_in_record,
};
use crate::editor_document::editor_child_by_stem;
use crate::editor_paths::{
    build_editor_instance_path_parts, build_editor_instance_paths,
    build_editor_source_paths_by_index, infer_source_script,
};
use crate::external_tools::{run_checked_external_tool, run_git_checked};
use crate::file_io::{
    absolutize_under, canonical_path, ensure_existing_ancestor_inside, exact_path_key, fnv1a_hex,
    is_service_settings_file_name, path_key, service_settings_path, set_path_readonly,
    strip_extended_prefix, validate_filesystem_instance_name, write_utf8_file,
};
use crate::instance_api::{self, AddInstanceSpec, InstanceSelector};
use crate::project_config;
use crate::rbx_encode::settings_root_indices;
use crate::rbx_model::canonicalize_settings_reference_documents;
use crate::settings_bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, SettingsBytecodeInstance,
    encode_settings_bytecode, reindex_reference_indices,
};
use crate::settings_tree::{editor_service_root_index, settings_children_by_parent};
use crate::snapshot_refs::remap_record_reference_ids;

pub(super) const RENIUM_STORE_EXTENSION: &str = "renium";
pub(super) const RENIUM_DIR_GITIGNORE: &str = "# Renium local state. Configuration and link.lock.json remain tracked.\ncache/\ndiagnostics/\neditor-history/\neditor-property-batches/\nimport-backups/\nsnapshots/\nsync-base/\nconflicts/\nbuild/\nbuild-staging/\nnested-syncback/\nadapter-baseline.json\nlink-cache/\n";

mod commands;
mod wally;
#[cfg(test)]
use commands::pack_subtree_to_bytecode;
use commands::resolve_editor_instance_by_path_ordinals;
pub(super) use commands::{
    link_add, link_apply, link_break, link_delete_package, link_move_target, link_pack, link_status,
};
pub(super) use wally::sync_wally_packages;
pub(crate) use wally::sync_wally_packages_result;

const LINK_MANIFEST_VERSION: u32 = 1;
const LINK_GIT_CACHE_REF: &str = "refs/renium/cache";

#[cfg(test)]
#[path = "package_links_tests.rs"]
mod tests;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkManifest {
    version: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    cache_dir: Option<String>,
    links: Vec<LinkEntry>,
    broken: Vec<LinkTargetRef>,
}

impl Default for LinkManifest {
    fn default() -> Self {
        Self {
            version: LINK_MANIFEST_VERSION,
            cache_dir: None,
            links: Vec::new(),
            broken: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkEntry {
    id: String,
    #[serde(default = "default_true")]
    read_only: bool,
    source: LinkSource,
    targets: Vec<LinkTargetRef>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LinkSource {
    Local {
        path: String,
    },
    Git {
        url: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
    Wally {
        package: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

impl LinkSource {
    fn kind(&self) -> &'static str {
        match self {
            LinkSource::Local { .. } => "local",
            LinkSource::Git { .. } => "git",
            LinkSource::Wally { .. } => "wally",
        }
    }

    fn summary(&self) -> String {
        match self {
            LinkSource::Local { path } => format!("local:{path}"),
            LinkSource::Git {
                url,
                git_ref,
                subpath,
            } => {
                let mut out = format!("git:{url}");
                if let Some(git_ref) = git_ref {
                    out.push('#');
                    out.push_str(git_ref);
                }
                if let Some(subpath) = subpath {
                    out.push_str("//");
                    out.push_str(subpath);
                }
                out
            }
            LinkSource::Wally { package, version } => version.as_ref().map_or_else(
                || format!("wally:{package}"),
                |version| format!("wally:{package}@{version}"),
            ),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct LinkTargetRef {
    service: String,
    path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ords: Vec<usize>,
}

#[derive(Serialize, Deserialize, Default)]
struct LinkLock {
    version: u32,
    entries: BTreeMap<String, LinkLockEntry>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LinkLockEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_ref: Option<String>,
    files: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    targets: BTreeMap<String, LinkTargetLockEntry>,
}

#[derive(Serialize, Deserialize, Default)]
struct LinkTargetLockEntry {
    files: BTreeMap<String, String>,
}

struct LinkFilePair {
    mirror: PathBuf,
    canonical: PathBuf,
}

struct ResolvedLinkTarget {
    link_id: String,
    read_only: bool,
    source_is_local: bool,
    resolved_ref: Option<String>,
    service: String,
    target_segments: Vec<String>,
    target_ordinals: Vec<usize>,
    broken: bool,
    resolved: bool,
    unresolved_reason: Option<String>,

    files: Vec<LinkFilePair>,

    package_source: Option<PathBuf>,

    source_path: Option<PathBuf>,

    storage: Option<LinkTargetStorage>,
}

struct LinkTargetStorage {
    target_path: PathBuf,
    source_root: PathBuf,
    settings_file: Option<PathBuf>,
    settings_output_file: Option<PathBuf>,
    consumed_segments: usize,
    owner: String,
    naming: project_config::ProjectScriptNaming,
    source_is_file: bool,
    filesystem_target: bool,
}

struct LinkSourceMeta {
    is_package: bool,
    root_class: Option<String>,
    root_name: Option<String>,
    instances: usize,
    updated_unix_ms: Option<u128>,
}

fn file_modified_unix_ms(path: &Path) -> Option<u128> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis())
}

fn read_link_source_meta(source_path: &Path) -> LinkSourceMeta {
    let updated_unix_ms = file_modified_unix_ms(source_path);
    if is_package_path(source_path)
        && let Ok(package) = SettingsBytecode::read_file(source_path)
    {
        let root = package
            .instances
            .iter()
            .find(|instance| instance.parent_index.is_none());
        return LinkSourceMeta {
            is_package: true,
            root_class: root.map(|instance| instance.class_name.clone()),
            root_name: root.map(|instance| instance.name.clone()),
            instances: package.instances.len(),
            updated_unix_ms,
        };
    }
    if source_path.is_file() {
        let root_class = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                infer_source_script(name, &project_config::ProjectScriptNaming::default())
            })
            .map(|(class, _, _)| class.to_string());
        return LinkSourceMeta {
            is_package: false,
            root_class,
            root_name: None,
            instances: 1,
            updated_unix_ms,
        };
    }
    LinkSourceMeta {
        is_package: false,
        root_class: Some("Folder".to_string()),
        root_name: None,
        instances: 0,
        updated_unix_ms,
    }
}

fn is_package_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(RENIUM_STORE_EXTENSION))
}

struct LinkResolveOptions {
    only_link: Option<String>,
    offline: bool,
    fetch: bool,
    read_only: bool,
    git_path: String,
    wally_path: String,
    cache_dir: PathBuf,
}

impl Default for LinkResolveOptions {
    fn default() -> Self {
        Self {
            only_link: None,
            offline: true,
            fetch: false,
            read_only: true,
            git_path: "git".to_string(),
            wally_path: "wally".to_string(),
            cache_dir: PathBuf::from(".renium").join("link-cache"),
        }
    }
}

fn link_manifest_path(project_root: &Path, manifest: &Path) -> PathBuf {
    absolutize_under(project_root, manifest)
}

fn link_lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".renium").join("link.lock.json")
}

fn link_cache_dir(project_root: &Path, manifest: &LinkManifest) -> PathBuf {
    match manifest.cache_dir.as_deref() {
        Some(dir) if !dir.trim().is_empty() => {
            absolutize_under(project_root, Path::new(dir.trim()))
        }
        _ => project_root.join(".renium").join("link-cache"),
    }
}

fn resolve_link_cache_dir(
    project_root: &Path,
    manifest: &LinkManifest,
    override_dir: Option<&Path>,
) -> PathBuf {
    if let Some(dir) = override_dir
        && !dir.as_os_str().is_empty()
    {
        return absolutize_under(project_root, dir);
    }
    link_cache_dir(project_root, manifest)
}

fn ensure_renium_gitignore(project_root: &Path) {
    let dir = project_root.join(".renium");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = fs::write(&gitignore, RENIUM_DIR_GITIGNORE);
    }
}

fn read_link_manifest(path: &Path) -> Result<LinkManifest> {
    if !path.exists() {
        return Ok(LinkManifest::default());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("Failed to read link manifest {}", path.display()))?;
    let raw = raw.trim_start_matches('\u{feff}');
    if raw.trim().is_empty() {
        bail!("Link manifest {} is empty", path.display());
    }
    let manifest: LinkManifest = serde_json::from_str(raw)
        .with_context(|| format!("Failed to parse link manifest {}", path.display()))?;
    validate_link_manifest(&manifest)
        .with_context(|| format!("Invalid link manifest {}", path.display()))?;
    Ok(manifest)
}

fn validate_link_manifest(manifest: &LinkManifest) -> Result<()> {
    if manifest.version != LINK_MANIFEST_VERSION {
        bail!(
            "Unsupported Renium link manifest version {}; expected {}",
            manifest.version,
            LINK_MANIFEST_VERSION
        );
    }
    let mut ids = HashSet::new();
    let mut targets = HashSet::new();
    let mut broken_targets = HashSet::new();
    for target in &manifest.broken {
        validate_link_target_ref(target).context("invalid broken renium-link target")?;
        broken_targets.insert(link_target_ref_key(target));
    }
    let mut active_targets: Vec<(&str, &LinkTargetRef, Vec<String>, Vec<usize>)> = Vec::new();
    for link in &manifest.links {
        if link.id.trim().is_empty() {
            bail!("renium-link id cannot be empty");
        }
        if !ids.insert(link.id.clone()) {
            bail!(
                "duplicate renium-link id {:?}; link ids must be unique",
                link.id
            );
        }
        for target in &link.targets {
            validate_link_target_ref(target)
                .with_context(|| format!("invalid target for link {}", link.id))?;
            let key = link_target_ref_key(target);
            if !targets.insert(key.clone()) {
                bail!("duplicate renium-link target {key:?}");
            }
            if broken_targets.contains(&key) {
                continue;
            }
            let segments = link_target_segments(target);
            let ordinals = link_target_ordinals(target);
            for (existing_link, existing_target, existing_segments, existing_ordinals) in
                &active_targets
            {
                if existing_target.service != target.service {
                    continue;
                }
                if selector_starts_with(&segments, &ordinals, existing_segments, existing_ordinals)
                    || selector_starts_with(
                        existing_segments,
                        existing_ordinals,
                        &segments,
                        &ordinals,
                    )
                {
                    bail!(
                        "renium-link targets {}.{} ({}) and {}.{} ({}) overlap",
                        existing_target.service,
                        existing_segments.join("."),
                        existing_link,
                        target.service,
                        segments.join("."),
                        link.id
                    );
                }
            }
            active_targets.push((link.id.as_str(), target, segments, ordinals));
        }
    }
    Ok(())
}

fn serialize_link_manifest(manifest: &LinkManifest) -> Result<String> {
    validate_link_manifest(manifest)?;
    Ok(serde_json::to_string_pretty(manifest)? + "\n")
}

fn serialize_link_lock(lock: &LinkLock) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(lock)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_link_manifest(path: &Path, manifest: &LinkManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let content = serialize_link_manifest(manifest)?;
    write_utf8_file(path, &content)
}

fn read_link_lock(project_root: &Path) -> Result<LinkLock> {
    let path = link_lock_path(project_root);
    if !path.exists() {
        return Ok(LinkLock::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read Renium link lock {}", path.display()))?;
    let lock: LinkLock = serde_json::from_str(&raw)
        .with_context(|| format!("Invalid Renium link lock {}", path.display()))?;
    if lock.version != LINK_MANIFEST_VERSION {
        bail!(
            "Unsupported Renium link lock version {} in {}; expected {}",
            lock.version,
            path.display(),
            LINK_MANIFEST_VERSION
        );
    }
    Ok(lock)
}

fn link_target_segments(target: &LinkTargetRef) -> Vec<String> {
    if target.path.first().map(String::as_str) == Some(target.service.as_str()) {
        target.path[1..].to_vec()
    } else {
        target.path.clone()
    }
}

fn validate_link_target_ref(target: &LinkTargetRef) -> Result<Vec<String>> {
    validate_filesystem_instance_name(&target.service, "link target service")?;
    let segments = link_target_segments(target);
    if segments.is_empty() {
        bail!(
            "link target for service {} must include at least one path segment under the service",
            target.service
        );
    }
    for segment in &segments {
        if segment.is_empty() {
            bail!("link target path segment cannot be empty");
        }
    }
    if !target.ords.is_empty() {
        let includes_service =
            target.path.first().map(String::as_str) == Some(target.service.as_str());
        let expected = if includes_service {
            [target.path.len(), segments.len()]
        } else {
            [segments.len(), segments.len()]
        };
        if !expected.contains(&target.ords.len()) {
            bail!("link target ords must match the target path length");
        }
        if target.ords.contains(&0) {
            bail!("link target ords must contain positive integers");
        }
        if includes_service
            && target.ords.len() == target.path.len()
            && target.ords.first() != Some(&1)
        {
            bail!("link target service ordinal must be 1");
        }
    }
    Ok(segments)
}

fn validate_filesystem_link_target_ref(target: &LinkTargetRef) -> Result<Vec<String>> {
    let segments = validate_link_target_ref(target)?;
    for segment in &segments {
        validate_filesystem_instance_name(segment, "link target path segment")?;
    }
    Ok(segments)
}

fn link_target_ordinals(target: &LinkTargetRef) -> Vec<usize> {
    let segments = link_target_segments(target);
    if target.ords.is_empty() {
        return vec![1; segments.len()];
    }
    if target.path.first().map(String::as_str) == Some(target.service.as_str())
        && target.ords.len() == target.path.len()
    {
        return target.ords[1..].to_vec();
    }
    target.ords.clone()
}

fn selector_starts_with(
    path_segments: &[String],
    path_ordinals: &[usize],
    prefix_segments: &[String],
    prefix_ordinals: &[usize],
) -> bool {
    path_segments.starts_with(prefix_segments)
        && prefix_ordinals
            .iter()
            .enumerate()
            .all(|(index, ordinal)| path_ordinals.get(index).copied().unwrap_or(1) == *ordinal)
}

fn link_target_key(service: &str, segments: &[String], ordinals: &[usize]) -> String {
    if validate_filesystem_instance_name(service, "service").is_err()
        || segments
            .iter()
            .any(|segment| validate_filesystem_instance_name(segment, "segment").is_err())
    {
        let encoded = serde_json::to_string(&(service, segments, ordinals))
            .expect("link target selectors always serialize");
        return format!("\u{2}{encoded}");
    }
    format!(
        "{service}\u{1}{}\u{1}{}",
        segments.join("/"),
        ordinals
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn package_lock_key(
    prefix: &str,
    service: &str,
    segments: &[String],
    ordinals: &[usize],
) -> String {
    format!("{prefix}:{}", link_target_key(service, segments, ordinals))
}

fn link_target_ref_key(target: &LinkTargetRef) -> String {
    link_target_key(
        &target.service,
        &link_target_segments(target),
        &link_target_ordinals(target),
    )
}

fn mark_manifest_target_broken(manifest: &mut LinkManifest, target: &ResolvedLinkTarget) -> bool {
    let broken_target = LinkTargetRef {
        service: target.service.clone(),
        path: target.target_segments.clone(),
        ords: target.target_ordinals.clone(),
    };
    let key = link_target_ref_key(&broken_target);
    if manifest
        .broken
        .iter()
        .any(|existing| link_target_ref_key(existing) == key)
    {
        return false;
    }
    manifest.broken.push(broken_target);
    true
}

fn link_mirror_lock_key(src_root: &Path, mirror: &Path) -> String {
    mirror
        .strip_prefix(src_root)
        .unwrap_or(mirror)
        .to_string_lossy()
        .replace('\\', "/")
}

fn write_mirror_file(path: &Path, content: &str) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let existing = fs::read_to_string(path).ok();
    if existing.as_deref() == Some(content) {
        return Ok(false);
    }
    set_path_readonly(path, false)?;
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(true)
}

fn ensure_git_source(
    cache_root: &Path,
    url: &str,
    git_ref: Option<&str>,
    git_path: &str,
    offline: bool,
    read_only: bool,
) -> Result<(PathBuf, Option<String>)> {
    let key = fnv1a_hex(format!("{url}\u{1}{}", git_ref.unwrap_or("")).as_bytes());
    let dir = cache_root.join(key);
    let git = |args: &[&str], cwd: &Path| -> Result<String> {
        let owned = args
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        run_git_checked(git_path, &owned, cwd)
    };

    if !dir.join(".git").exists() {
        if offline || read_only {
            bail!(
                "git source {url} is not cached yet. Run `renium link-apply` online once before using --offline or --check."
            );
        }
        fs::create_dir_all(cache_root)
            .with_context(|| format!("Failed to create {}", cache_root.display()))?;
        let dir_str = dir.to_string_lossy().into_owned();
        git(
            &["-c", "core.longpaths=true", "clone", url, &dir_str],
            cache_root,
        )?;
    }

    let dir_str = dir.to_string_lossy().into_owned();
    if !read_only {
        git(&["-C", &dir_str, "config", "core.longpaths", "true"], &dir)?;
    }
    if let Some(git_ref) = git_ref {
        let cache_commit = format!("{LINK_GIT_CACHE_REF}^{{commit}}");
        let cached_commit = || {
            git(
                &["-C", &dir_str, "rev-parse", "--verify", &cache_commit],
                &dir,
            )
            .or_else(|_| git(&["-C", &dir_str, "rev-parse", "--verify", "HEAD"], &dir))
        };
        if read_only {
            let head = git(&["-C", &dir_str, "rev-parse", "HEAD"], &dir)?;
            let requested = cached_commit()?;
            if head != requested {
                bail!(
                    "Cached git source {url} is at {head}, not its recorded {git_ref} commit ({requested}); run link-apply without --check to update it"
                );
            }
        } else {
            let requested = if offline {
                cached_commit()?
            } else {
                git(
                    &[
                        "-C", &dir_str, "fetch", "origin", git_ref, "--tags", "--force",
                    ],
                    &dir,
                )?;
                git(
                    &[
                        "-C",
                        &dir_str,
                        "rev-parse",
                        "--verify",
                        "FETCH_HEAD^{commit}",
                    ],
                    &dir,
                )?
            };
            if !offline {
                git(
                    &["-C", &dir_str, "update-ref", LINK_GIT_CACHE_REF, &requested],
                    &dir,
                )?;
            }
            git(
                &[
                    "-C", &dir_str, "checkout", "--detach", "--force", &requested,
                ],
                &dir,
            )?;
            git(&["-C", &dir_str, "reset", "--hard", &requested], &dir)?;
        }
    } else if !offline && !read_only {
        git(
            &[
                "-C", &dir_str, "fetch", "--all", "--tags", "--force", "--prune",
            ],
            &dir,
        )?;
        let requested = git(
            &[
                "-C",
                &dir_str,
                "rev-parse",
                "--verify",
                "@{upstream}^{commit}",
            ],
            &dir,
        )
        .or_else(|_| {
            git(
                &[
                    "-C",
                    &dir_str,
                    "rev-parse",
                    "--verify",
                    "refs/remotes/origin/HEAD^{commit}",
                ],
                &dir,
            )
        })?;
        git(
            &[
                "-C", &dir_str, "checkout", "--detach", "--force", &requested,
            ],
            &dir,
        )?;
        git(&["-C", &dir_str, "reset", "--hard", &requested], &dir)?;
    }

    let head = git(&["-C", &dir_str, "rev-parse", "HEAD"], &dir)?;
    Ok((dir, Some(head)))
}

fn resolve_wally_source(
    project_root: &Path,
    package: &str,
    version_requirement: Option<&str>,
    wally_path: &str,
    offline: bool,
) -> Result<(PathBuf, Option<String>)> {
    let packages_dir = project_root.join("Packages");
    let index_dir = packages_dir.join("_Index");
    if !index_dir.exists() && !offline {
        run_checked_external_tool("wally install", wally_path, &["install"], project_root)?;
    }
    if !index_dir.exists() {
        bail!(
            "Wally package index not found at {}. Run `wally install` (with a wally.toml dependency on {package}) first.",
            index_dir.display()
        );
    }
    let wanted = package.replace('/', "_").to_ascii_lowercase();
    let leaf = package
        .rsplit('/')
        .next()
        .unwrap_or(package)
        .to_ascii_lowercase();
    let requirement = version_requirement
        .map(semver::VersionReq::parse)
        .transpose()
        .with_context(|| {
            format!(
                "Invalid Wally version requirement for {package}: {}",
                version_requirement.unwrap_or_default()
            )
        })?;
    let mut best: Option<(PathBuf, semver::Version)> = None;
    let wanted_prefix = format!("{wanted}@");
    for entry in fs::read_dir(&index_dir)
        .with_context(|| format!("Failed to read {}", index_dir.display()))?
    {
        let entry = entry?;
        let dir_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !dir_name.starts_with(&wanted_prefix) {
            continue;
        }
        let Some(raw_version) = dir_name.strip_prefix(&wanted_prefix) else {
            continue;
        };
        let Ok(version) = semver::Version::parse(raw_version) else {
            continue;
        };
        if requirement
            .as_ref()
            .is_some_and(|requirement| !requirement.matches(&version))
        {
            continue;
        }
        let inner = entry.path().join(&leaf);
        let chosen = if inner.is_dir() { inner } else { entry.path() };
        if best.as_ref().is_none_or(|(_, current)| version > *current) {
            best = Some((chosen, version));
        }
    }
    match best {
        Some((dir, version)) => Ok((dir, Some(version.to_string()))),
        None => bail!(
            "No installed Wally package {package} satisfies {} under {}. Check wally.toml and run `wally install`.",
            version_requirement.unwrap_or("the requested version"),
            index_dir.display()
        ),
    }
}

const GLOBAL_LINK_PREFIX: &str = "~global/";

fn renium_global_packages_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("RENIUM_GLOBAL_PACKAGES_DIR") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(home)
        .join("Documents")
        .join("Renium")
        .join("Packages")
}

fn resolve_local_link_path(project_root: &Path, raw: &str) -> PathBuf {
    let normalized = raw.replace('\\', "/");
    if let Some(rest) = normalized.strip_prefix(GLOBAL_LINK_PREFIX) {
        return renium_global_packages_dir().join(rest);
    }
    absolutize_under(project_root, Path::new(raw))
}

fn is_global_link_path(raw: &str) -> bool {
    raw.replace('\\', "/").starts_with(GLOBAL_LINK_PREFIX)
}

fn resolve_link_source(
    project_root: &Path,
    source: &LinkSource,
    options: &LinkResolveOptions,
) -> Result<(PathBuf, bool, Option<String>)> {
    match source {
        LinkSource::Local { path } => {
            let resolved = resolve_local_link_path(project_root, path);
            if !resolved.exists() {
                bail!("local link source not found: {}", resolved.display());
            }
            let is_dir = resolved.is_dir();
            Ok((resolved, is_dir, None))
        }
        LinkSource::Git {
            url,
            git_ref,
            subpath,
        } => {
            let cache_root = options.cache_dir.clone();
            let (dir, head) = ensure_git_source(
                &cache_root,
                url,
                git_ref.as_deref(),
                &options.git_path,
                options.offline || !options.fetch,
                options.read_only,
            )?;
            let root = match subpath {
                Some(subpath) => {
                    canonical_existing_descendant(&dir, &dir.join(subpath), "git link subpath")?
                }
                None => dir,
            };
            let is_dir = root.is_dir();
            Ok((root, is_dir, head))
        }
        LinkSource::Wally { package, version } => {
            let (dir, resolved_version) = resolve_wally_source(
                project_root,
                package,
                version.as_deref(),
                &options.wally_path,
                options.offline || !options.fetch,
            )?;
            let resolved_ref = resolved_version.map(|value| format!("{package}@{value}"));
            let is_dir = dir.is_dir();
            Ok((dir, is_dir, resolved_ref))
        }
    }
}

fn canonical_existing_descendant(root: &Path, candidate: &Path, label: &str) -> Result<PathBuf> {
    let root =
        canonical_path(root).with_context(|| format!("Failed to resolve {}", root.display()))?;
    let candidate = canonical_path(candidate)
        .with_context(|| format!("{label} not found: {}", candidate.display()))?;
    if candidate != root && !candidate.starts_with(&root) {
        bail!(
            "{label} must stay inside {}: {}",
            root.display(),
            candidate.display()
        );
    }
    Ok(candidate)
}

fn resolve_link_target_storage(
    project_root: &Path,
    src_root: &Path,
    target: &LinkTargetRef,
    enforce_projection_ownership: bool,
) -> Result<LinkTargetStorage> {
    let segments = validate_link_target_ref(target)?;
    let mut staged_segments = vec![target.service.clone()];
    staged_segments.extend(segments.clone());
    let filesystem_target = validate_filesystem_link_target_ref(target).is_ok();
    let loaded = project_config::try_load_project(None, Some(project_root))?.filter(|loaded| {
        canonical_path(&loaded.root).is_ok_and(|root| root == project_root)
            && path_key(&loaded.root.join(&loaded.project.source_root)) == path_key(src_root)
    });
    let (target_path, source_root, owner, consumed_segments, naming) = if let Some(loaded) =
        loaded.as_ref()
    {
        if enforce_projection_ownership {
            let stage = project_config::stage_project(loaded)?;
            if stage.target_is_transformed(&staged_segments) {
                bail!(
                    "Link target {} is generated by a sync rule; edit its source file instead",
                    staged_segments.join(".")
                );
            }
            if project_config::project_target_is_declarative(loaded, &staged_segments)? {
                bail!(
                    "Link target {} is declared by project configuration and cannot be replaced by a package",
                    staged_segments.join(".")
                );
            }
        }
        let resolved = project_config::resolve_project_write_segments(loaded, &staged_segments)?;
        let target_path = if filesystem_target {
            resolved.path
        } else {
            resolved.source_root.clone()
        };
        if resolved.owner == "adapter" {
            bail!(
                "Link target {} is adapter-owned and cannot be edited by package links",
                staged_segments.join(".")
            );
        }
        (
            target_path,
            resolved.source_root,
            resolved.owner.to_string(),
            resolved.consumed_segments,
            resolved.naming,
        )
    } else {
        let source_root = src_root.join(&target.service);
        (
            if filesystem_target {
                source_root.join(segments.iter().collect::<PathBuf>())
            } else {
                source_root.clone()
            },
            source_root,
            "sourceRoot".to_string(),
            1,
            link_project_naming(project_root)?,
        )
    };
    let source_is_file = source_root.is_file()
        || source_root
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "lua" | "luau" | "renium" | "rbxm" | "rbxmx" | "json" | "jsonc"
                )
            });
    if source_is_file
        && !(target_path == source_root
            && source_root
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| {
                    matches!(extension.to_ascii_lowercase().as_str(), "lua" | "luau")
                }))
    {
        bail!(
            "Link target {} is owned by file {} and has no writable settings store",
            staged_segments.join("."),
            source_root.display()
        );
    }
    let settings_file = (!source_is_file)
        .then(|| service_settings_path(&source_root))
        .filter(|path| path.is_file());
    let settings_output_file = if source_is_file {
        None
    } else {
        Some(service_settings_path(&source_root))
    };
    if !source_is_file && settings_file.is_none() {
        bail!(
            "Link target service {} has no Renium bytecode settings file for owner {} at {}. Pull from Studio once before applying links.",
            target.service,
            owner,
            service_settings_path(&source_root).display()
        );
    }
    Ok(LinkTargetStorage {
        target_path,
        source_root,
        settings_file,
        settings_output_file,
        consumed_segments,
        owner,
        naming,
        source_is_file,
        filesystem_target,
    })
}

fn link_target_document_selector(
    target: &ResolvedLinkTarget,
    document: &SettingsBytecode,
) -> Result<(String, Vec<String>, Vec<usize>)> {
    let storage = target
        .storage
        .as_ref()
        .context("Resolved link target has no ownership information")?;
    link_target_document_selector_parts(
        &target.service,
        &target.target_segments,
        &target.target_ordinals,
        storage,
        document,
    )
}

fn link_target_document_selector_parts(
    service: &str,
    target_segments: &[String],
    target_ordinals: &[usize],
    storage: &LinkTargetStorage,
    document: &SettingsBytecode,
) -> Result<(String, Vec<String>, Vec<usize>)> {
    let root = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .context("Link target settings store has no root instance")?;
    let mut global = vec![service.to_string()];
    global.extend(target_segments.iter().cloned());
    let mut global_ordinals = vec![1];
    global_ordinals.extend(target_ordinals.iter().copied());
    if storage.consumed_segments > global.len() {
        bail!(
            "Link ownership consumed more path segments than target {}",
            global.join(".")
        );
    }
    Ok((
        root,
        global[storage.consumed_segments..].to_vec(),
        global_ordinals[storage.consumed_segments..].to_vec(),
    ))
}

fn link_target_file_pairs_at(
    target: &LinkTargetRef,
    source_root: &Path,
    source_is_dir: bool,
    naming: &project_config::ProjectScriptNaming,
    target_path: &Path,
    containment_root: &Path,
    target_is_file: bool,
) -> Result<Vec<LinkFilePair>> {
    validate_filesystem_link_target_ref(target)?;
    ensure_existing_ancestor_inside(containment_root, target_path, "link target")?;
    if target_is_file {
        if source_is_dir {
            bail!(
                "A directory link source cannot replace direct file owner {}",
                target_path.display()
            );
        }
        let file_name = source_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        infer_source_script(&file_name, naming).ok_or_else(|| {
            anyhow::anyhow!("link source is not a Lua script: {}", source_root.display())
        })?;
        return Ok(vec![LinkFilePair {
            mirror: target_path.to_path_buf(),
            canonical: source_root.to_path_buf(),
        }]);
    }
    if !source_is_dir {
        let file_name = source_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (class_name, _, run_context) =
            infer_source_script(&file_name, naming).ok_or_else(|| {
                anyhow::anyhow!("link source is not a Lua script: {}", source_root.display())
            })?;
        let (_, leaf_suffix) = link_script_file_names(naming, class_name, run_context, &file_name)
            .ok_or_else(|| anyhow::anyhow!("unsupported script class {class_name}"))?;
        let leaf = target_path
            .file_name()
            .map(|name| name.to_string_lossy())
            .filter(|name| !name.is_empty())
            .context("Link target has no file name")?;
        let mirror = target_path.with_file_name(format!("{leaf}{leaf_suffix}"));
        ensure_existing_ancestor_inside(containment_root, &mirror, "link target file")?;
        return Ok(vec![LinkFilePair {
            mirror,
            canonical: source_root.to_path_buf(),
        }]);
    }

    let target_root = target_path.to_path_buf();
    let mut pairs = Vec::new();
    let mut mirror_sources = HashMap::<String, PathBuf>::new();
    for entry in WalkDir::new(source_root) {
        let entry = entry
            .with_context(|| format!("Failed to read link source {}", source_root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some((class_name, leaf_name, run_context)) = infer_source_script(&file_name, naming)
        else {
            continue;
        };
        let Some((init_name, leaf_suffix)) =
            link_script_file_names(naming, class_name, run_context, &file_name)
        else {
            continue;
        };
        let renium_name =
            leaf_name.map_or(init_name, |leaf_name| format!("{leaf_name}{leaf_suffix}"));
        let relative = path.strip_prefix(source_root).unwrap_or(path);
        let mut mirror = target_root.clone();
        if let Some(parent) = relative.parent()
            && !parent.as_os_str().is_empty()
        {
            mirror.push(parent);
        }
        mirror.push(renium_name);
        ensure_existing_ancestor_inside(containment_root, &mirror, "link target file")?;
        let mirror_key = path_key(&mirror);
        if let Some(previous) = mirror_sources.insert(mirror_key, path.to_path_buf()) {
            bail!(
                "Link source files {} and {} both map to {}",
                previous.display(),
                path.display(),
                mirror.display()
            );
        }
        pairs.push(LinkFilePair {
            mirror,
            canonical: path.to_path_buf(),
        });
    }
    if pairs.is_empty() {
        bail!(
            "Link source directory contains no scripts recognized by this project's script naming: {}",
            source_root.display()
        );
    }
    Ok(pairs)
}

fn link_script_file_names(
    naming: &project_config::ProjectScriptNaming,
    class_name: &str,
    run_context: Option<&str>,
    source_file_name: &str,
) -> Option<(String, String)> {
    let suffix = match class_name {
        "Script" if run_context.is_some_and(|value| value.eq_ignore_ascii_case("Client")) => {
            &naming.client_run_context_suffix
        }
        "Script" if run_context.is_some_and(|value| value.eq_ignore_ascii_case("Plugin")) => {
            &naming.plugin_suffix
        }
        "Script" => &naming.server_suffix,
        "LocalScript" => &naming.client_suffix,
        "ModuleScript" => &naming.module_suffix,
        _ => return None,
    };
    let extension = match naming.extension {
        project_config::ScriptExtensionPolicy::Lua => "lua",
        project_config::ScriptExtensionPolicy::Luau => "luau",
        project_config::ScriptExtensionPolicy::Preserve => source_file_name
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .filter(|extension| matches!(extension.to_ascii_lowercase().as_str(), "lua" | "luau"))
            .unwrap_or("luau"),
    };
    Some((
        format!("init{suffix}.{extension}"),
        format!("{suffix}.{extension}"),
    ))
}

fn resolve_link_targets(
    project_root: &Path,
    src_root: &Path,
    manifest: &LinkManifest,
    options: &LinkResolveOptions,
) -> Vec<ResolvedLinkTarget> {
    let broken: HashSet<String> = manifest.broken.iter().map(link_target_ref_key).collect();
    let mut resolved = Vec::new();
    for link in &manifest.links {
        if let Some(only) = &options.only_link
            && &link.id != only
        {
            continue;
        }
        let source = resolve_link_source(project_root, &link.source, options);
        for target in &link.targets {
            let key = link_target_ref_key(target);
            let segments = link_target_segments(target);
            let is_broken = broken.contains(&key);
            let mut entry = ResolvedLinkTarget {
                link_id: link.id.clone(),
                read_only: link.read_only,
                source_is_local: matches!(link.source, LinkSource::Local { .. }),
                resolved_ref: None,
                service: target.service.clone(),
                target_segments: segments,
                target_ordinals: link_target_ordinals(target),
                broken: is_broken,
                resolved: false,
                unresolved_reason: None,
                files: Vec::new(),
                package_source: None,
                source_path: None,
                storage: None,
            };
            let storage = match resolve_link_target_storage(
                project_root,
                src_root,
                target,
                !options.read_only,
            ) {
                Ok(storage) => storage,
                Err(error) => {
                    entry.unresolved_reason = Some(error.to_string());
                    resolved.push(entry);
                    continue;
                }
            };
            match &source {
                Ok((source_root, is_dir, resolved_ref)) => {
                    entry.resolved_ref.clone_from(resolved_ref);
                    entry.source_path = Some(source_root.clone());
                    if !is_dir && is_package_path(source_root) {
                        if storage.settings_file.is_some() {
                            entry.resolved = true;
                            entry.package_source = Some(source_root.clone());
                        } else {
                            entry.unresolved_reason = Some(format!(
                                "Package link target {}.{} has no writable bytecode settings store",
                                target.service,
                                link_target_segments(target).join(".")
                            ));
                        }
                    } else {
                        if link_target_ordinals(target)
                            .iter()
                            .any(|ordinal| *ordinal != 1)
                        {
                            entry.unresolved_reason = Some(format!(
                                "File and directory link target {}.{} cannot select duplicate sibling ordinals",
                                target.service,
                                link_target_segments(target).join(".")
                            ));
                        } else {
                            let containment_root = if storage.source_root.is_file() {
                                storage.source_root.parent().unwrap_or(project_root)
                            } else {
                                &storage.source_root
                            };
                            match link_target_file_pairs_at(
                                target,
                                source_root,
                                *is_dir,
                                &storage.naming,
                                &storage.target_path,
                                containment_root,
                                storage.source_is_file,
                            ) {
                                Ok(files) => {
                                    entry.resolved = true;
                                    entry.files = files;
                                }
                                Err(error) => entry.unresolved_reason = Some(error.to_string()),
                            }
                        }
                    }
                }
                Err(error) => entry.unresolved_reason = Some(error.to_string()),
            }
            entry.storage = Some(storage);
            resolved.push(entry);
        }
    }
    resolved
}

fn ensure_editor_container_path(
    document: &mut SettingsBytecode,
    service: &str,
    segments_after_service: &[String],
) -> Result<usize> {
    let root_index = editor_service_root_index(document, service).unwrap_or_else(|| {
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            service.to_string(),
            service.into(),
            None,
        ));
        document.instances.len() - 1
    });
    let mut current = root_index;
    for component in segments_after_service {
        if let Some(child) = editor_child_by_stem(document, current, component) {
            current = child;
            continue;
        }
        let added = instance_api::add_instance(
            document,
            AddInstanceSpec::new(None, component.clone(), "Folder".to_string(), Some(current)),
        )?;
        current = added.index;
    }
    Ok(current)
}

#[derive(Default)]
struct PackageIdentityMatches {
    by_package_index: HashMap<usize, String>,
    by_existing_index: HashMap<usize, String>,
}

fn subtree_relative_indices(document: &SettingsBytecode, root: usize) -> HashMap<String, usize> {
    fn visit(
        document: &SettingsBytecode,
        children: &[Vec<usize>],
        index: usize,
        segments: &mut Vec<String>,
        ordinals: &mut Vec<usize>,
        output: &mut HashMap<String, usize>,
    ) {
        output.insert(instance_path_parts_key(segments, ordinals), index);
        let mut name_counts = HashMap::<String, usize>::new();
        for child in &children[index] {
            let name = document.instances[*child].name.clone();
            let ordinal = name_counts.entry(name.clone()).or_default();
            *ordinal += 1;
            segments.push(name);
            ordinals.push(*ordinal);
            visit(document, children, *child, segments, ordinals, output);
            segments.pop();
            ordinals.pop();
        }
    }

    let children = settings_children_by_parent(document);
    let mut output = HashMap::new();
    visit(
        document,
        &children,
        root,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut output,
    );
    output
}

fn package_identity_matches(
    document: &SettingsBytecode,
    existing_root: usize,
    package: &SettingsBytecode,
    package_root: usize,
    preserve_root: bool,
) -> PackageIdentityMatches {
    let existing = subtree_relative_indices(document, existing_root);
    let incoming = subtree_relative_indices(package, package_root);
    let mut by_package_index = HashMap::new();
    let mut by_existing_index = HashMap::new();
    for (key, package_index) in incoming {
        let Some(existing_index) = existing.get(&key).copied() else {
            continue;
        };
        if key != instance_path_parts_key(&[], &[])
            && document.instances[existing_index].class_name
                != package.instances[package_index].class_name
        {
            continue;
        }
        if key == instance_path_parts_key(&[], &[])
            && !preserve_root
            && document.instances[existing_index].class_name
                != package.instances[package_index].class_name
        {
            continue;
        }
        let settings_id = document.instances[existing_index].settings_id.clone();
        by_package_index.insert(package_index, settings_id.clone());
        by_existing_index.insert(existing_index, settings_id);
    }
    PackageIdentityMatches {
        by_package_index,
        by_existing_index,
    }
}

fn prepare_external_package_references(
    document: &mut SettingsBytecode,
    removed: &HashSet<usize>,
    preserved: &HashMap<usize, String>,
    label: &str,
) -> Result<()> {
    fn normalize_ref(
        object: &mut Map<String, Value>,
        refs: &BytecodeCloneRefMap,
        removed: &HashSet<usize>,
        preserved: &HashMap<usize, String>,
        label: &str,
    ) -> Result<()> {
        let Some(index) = ref_old_index(object, refs) else {
            return Ok(());
        };
        if !removed.contains(&index) {
            return Ok(());
        }
        let settings_id = preserved.get(&index).with_context(|| {
            format!(
                "{label} replacement would remove externally referenced instance {}",
                refs.old_index_by_settings_id
                    .iter()
                    .find_map(|(settings_id, candidate)| {
                        (*candidate == index).then_some(settings_id.as_str())
                    })
                    .unwrap_or("unknown")
            )
        })?;
        for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
            object.remove(selector);
        }
        object.insert("settingsId".to_string(), Value::String(settings_id.clone()));
        Ok(())
    }

    fn visit(
        value: &mut Value,
        refs: &BytecodeCloneRefMap,
        removed: &HashSet<usize>,
        preserved: &HashMap<usize, String>,
        label: &str,
    ) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, refs, removed, preserved, label)?;
                }
            }
            Value::Object(object) => {
                if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                    normalize_ref(object, refs, removed, preserved, label)?;
                    return Ok(());
                }
                if let Some(Value::Object(reference)) = object.get_mut("Ref") {
                    normalize_ref(reference, refs, removed, preserved, label)?;
                }
                for (key, value) in object.iter_mut() {
                    if key != "Ref" {
                        visit(value, refs, removed, preserved, label)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    let all_indices = (0..document.instances.len()).collect::<Vec<_>>();
    let identity = all_indices
        .iter()
        .copied()
        .map(|index| (index, index))
        .collect::<HashMap<_, _>>();
    let (segments, ordinals) = build_editor_instance_path_parts(document, "");
    let refs = build_clone_ref_map(
        document,
        CloneRefMapInput {
            source_subtree: &all_indices,
            old_to_new_index: &identity,
            path_segments_before: &segments,
            path_ordinals_before: &ordinals,
        },
    );
    for (index, instance) in document.instances.iter_mut().enumerate() {
        if removed.contains(&index) {
            continue;
        }
        for value in instance.properties.values_mut() {
            visit(value, &refs, removed, preserved, label)?;
        }
        for value in instance.attributes.values_mut() {
            visit(value, &refs, removed, preserved, label)?;
        }
    }
    Ok(())
}

fn prepare_package_replacement(
    document: &mut SettingsBytecode,
    subtree: &[usize],
    preserved: &HashMap<usize, String>,
    external_references: &HashSet<String>,
    label: &str,
) -> Result<()> {
    let preserved_ids = preserved
        .values()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if let Some(removed_id) = subtree
        .iter()
        .map(|index| document.instances[*index].settings_id.as_str())
        .find(|settings_id| {
            !preserved_ids.contains(*settings_id) && external_references.contains(*settings_id)
        })
    {
        bail!("{label} replacement would remove externally referenced instance {removed_id}");
    }
    prepare_external_package_references(
        document,
        &subtree.iter().copied().collect(),
        preserved,
        label,
    )
}

fn referenced_settings_ids(document: &SettingsBytecode) -> HashSet<String> {
    fn collect(value: &Value, output: &mut HashSet<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    collect(value, output);
                }
            }
            Value::Object(object) => {
                if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                    for selector in ["settingsId", "instanceId"] {
                        if let Some(value) = object.get(selector).and_then(Value::as_str) {
                            output.insert(value.to_string());
                        }
                    }
                    return;
                }
                if let Some(Value::Object(reference)) = object.get("Ref") {
                    for selector in ["settingsId", "instanceId"] {
                        if let Some(value) = reference.get(selector).and_then(Value::as_str) {
                            output.insert(value.to_string());
                        }
                    }
                }
                for (key, value) in object {
                    if key != "Ref" {
                        collect(value, output);
                    }
                }
            }
            _ => {}
        }
    }

    let mut output = HashSet::new();
    for instance in &document.instances {
        for value in instance.properties.values() {
            collect(value, &mut output);
        }
        for value in instance.attributes.values() {
            collect(value, &mut output);
        }
    }
    output
}

fn referenced_settings_ids_outside(
    documents: &HashMap<PathBuf, SettingsBytecode>,
    excluded: &Path,
) -> HashSet<String> {
    let excluded = exact_path_key(excluded);
    documents
        .iter()
        .filter(|(path, _)| exact_path_key(path) != excluded)
        .flat_map(|(_, document)| referenced_settings_ids(document))
        .collect()
}

fn collect_project_settings_files(
    src_root: &Path,
    extras: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut files = extras.into_iter().collect::<Vec<_>>();
    if src_root.is_dir() {
        for entry in WalkDir::new(src_root).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_service_settings_file_name)
            {
                files.push(entry.into_path());
            }
        }
    }
    files.sort_by_key(|path| exact_path_key(path));
    files.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    Ok(files)
}

fn canonicalize_loaded_settings_documents(
    documents: &mut HashMap<PathBuf, SettingsBytecode>,
) -> Result<()> {
    let mut paths_by_service = BTreeMap::new();
    let mut by_service = BTreeMap::new();
    for (path, document) in documents.iter() {
        let service = document
            .instances
            .iter()
            .find(|instance| instance.parent_index.is_none())
            .map(|instance| instance.name.clone())
            .with_context(|| format!("Settings store has no root: {}", path.display()))?;
        if let Some(previous) = paths_by_service.insert(service.clone(), path.clone()) {
            bail!(
                "Multiple settings stores claim service {service}: {} and {}",
                previous.display(),
                path.display()
            );
        }
        by_service.insert(service, document.clone());
    }
    canonicalize_settings_reference_documents(&mut by_service);
    for (service, document) in by_service {
        let path = paths_by_service
            .get(&service)
            .context("Canonical settings store path disappeared")?;
        documents.insert(path.clone(), document);
    }
    Ok(())
}

fn load_settings_documents(
    settings_files: &[PathBuf],
) -> Result<(
    HashMap<PathBuf, SettingsBytecode>,
    HashMap<PathBuf, PathBuf>,
)> {
    let mut documents = HashMap::with_capacity(settings_files.len());
    let mut outputs = HashMap::with_capacity(settings_files.len());
    for path in settings_files {
        documents.insert(
            path.clone(),
            SettingsBytecode::read_file(path)
                .with_context(|| format!("Failed to read {}", path.display()))?,
        );
        outputs.insert(path.clone(), path.clone());
    }
    canonicalize_loaded_settings_documents(&mut documents)?;
    Ok((documents, outputs))
}

fn stage_settings_document_writes(
    documents: &HashMap<PathBuf, SettingsBytecode>,
    outputs: &HashMap<PathBuf, PathBuf>,
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut settings_files = documents.keys().collect::<Vec<_>>();
    settings_files.sort_unstable();
    for settings_file in settings_files {
        let output_file = &outputs[settings_file];
        writes.insert(
            output_file.clone(),
            encode_settings_bytecode(&documents[settings_file])?,
        );
        if exact_path_key(output_file) != exact_path_key(settings_file) {
            removals.push(settings_file.clone());
        }
    }
    Ok(())
}

fn apply_preserved_subtree_identity(
    document: &mut SettingsBytecode,
    root: usize,
    preserved: &HashMap<String, String>,
) -> Result<()> {
    let by_key = subtree_relative_indices(document, root);
    let subtree = by_key.values().copied().collect::<HashSet<_>>();
    let desired = preserved.values().cloned().collect::<HashSet<_>>();
    let mut used = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            (!subtree.contains(&index)).then_some(instance.settings_id.clone())
        })
        .collect::<HashSet<_>>();
    let mut remapped = HashMap::new();
    let mut seed = document.instances.len();
    let mut entries = by_key.into_iter().collect::<Vec<_>>();
    entries.sort_by_key(|(_, index)| *index);
    for (key, index) in entries {
        let old = document.instances[index].settings_id.clone();
        let next = if let Some(settings_id) = preserved.get(&key) {
            if used.contains(settings_id) {
                bail!("Preserved package settings id {settings_id} is already in use");
            }
            settings_id.clone()
        } else if !used.contains(&old) && !desired.contains(&old) {
            old.clone()
        } else {
            next_editor_settings_id_fast(&mut used, &mut seed)
        };
        used.insert(next.clone());
        if old != next {
            remapped.insert(old, next.clone());
            document.instances[index].settings_id = next;
        }
    }
    if !remapped.is_empty() {
        for instance in &mut document.instances {
            remap_record_reference_ids(&mut instance.properties, &remapped);
            remap_record_reference_ids(&mut instance.attributes, &remapped);
        }
    }
    let indices = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for instance in &mut document.instances {
        reindex_reference_indices(&mut instance.properties, &indices);
        reindex_reference_indices(&mut instance.attributes, &indices);
    }
    Ok(())
}

fn materialize_package_root(
    document: &mut SettingsBytecode,
    service_dir: &Path,
    service: &str,
    package: &SettingsBytecode,
) -> Result<(Vec<PathBuf>, Vec<String>, Value)> {
    let roots = settings_root_indices(package);
    if roots.len() != 1 {
        bail!("link package must contain exactly one root instance");
    }
    let document_roots = settings_root_indices(document);
    if document_roots.len() != 1 {
        bail!("link target settings store must contain exactly one root instance");
    }
    let document_root = document_roots[0];
    let package_root = roots[0];
    let source_paths_before = build_editor_source_paths_by_index(document, service, service_dir);
    let removed = (0..document.instances.len()).collect::<Vec<_>>();
    let planned_removals =
        plan_editor_source_file_removals(service_dir, &source_paths_before, &removed)?;
    let path = build_editor_instance_paths(document, service)
        .get(document_root)
        .and_then(Option::as_ref)
        .cloned();
    let removed_target = json!({
        "settingsId": document.instances[document_root].settings_id,
        "className": document.instances[document_root].class_name,
        "pathSegments": path.as_ref().map(|path| path.path_segments.clone()).unwrap_or_default(),
        "pathOrdinals": path.map(|path| path.path_ordinals).unwrap_or_default(),
    });
    let root_name = document.instances[document_root].name.clone();
    let identity = package_identity_matches(document, document_root, package, package_root, true);
    let package_indices = (0..package.instances.len()).collect::<Vec<_>>();
    let (package_path_segments, package_path_ordinals) =
        build_editor_instance_path_parts(package, "");
    let mut existing_ids = HashSet::new();
    let mut seed = document.instances.len();
    let mut new_index_by_pkg = HashMap::new();
    let mut new_settings_ids = Vec::with_capacity(package.instances.len());
    let mut instances = Vec::with_capacity(package.instances.len());
    for (package_index, instance) in package.instances.iter().enumerate() {
        let is_root = package_index == package_root;
        let parent_index = if is_root {
            None
        } else {
            let package_parent = instance
                .parent_index
                .context("link package contains another root instance")?;
            Some(
                *new_index_by_pkg
                    .get(&package_parent)
                    .context("link package is not in preorder (child precedes parent)")?,
            )
        };
        let settings_id =
            if let Some(settings_id) = identity.by_package_index.get(&package_index).cloned() {
                if !existing_ids.insert(settings_id.clone()) {
                    bail!("Package identity mapping produced duplicate settings id {settings_id}");
                }
                settings_id
            } else {
                next_editor_settings_id_fast(&mut existing_ids, &mut seed)
            };
        let new_index = instances.len();
        instances.push(SettingsBytecodeInstance {
            settings_id: settings_id.clone(),
            name: if is_root {
                root_name.clone()
            } else {
                instance.name.clone()
            },
            class_name: instance.class_name.clone(),
            parent_index,
            properties: instance.properties.clone(),
            attributes: instance.attributes.clone(),
        });
        new_index_by_pkg.insert(package_index, new_index);
        new_settings_ids.push(settings_id);
    }
    let refs = build_clone_ref_map(
        package,
        CloneRefMapInput {
            source_subtree: &package_indices,
            old_to_new_index: &new_index_by_pkg,
            path_segments_before: &package_path_segments,
            path_ordinals_before: &package_path_ordinals,
        },
    );
    for instance in &mut instances {
        remap_internal_clone_refs_in_record(&mut instance.properties, &refs);
        remap_internal_clone_refs_in_record(&mut instance.attributes, &refs);
    }
    document.instances = instances;
    Ok((planned_removals, new_settings_ids, removed_target))
}

#[derive(Clone, Copy)]
struct PackageMaterialization<'a> {
    service_dir: &'a Path,
    service: &'a str,
    target_segments: &'a [String],
    target_ordinals: &'a [usize],
    package_path: &'a Path,
    filesystem_target: bool,
    external_references: &'a HashSet<String>,
}

fn materialize_package_target(
    document: &mut SettingsBytecode,
    request: PackageMaterialization<'_>,
) -> Result<(Vec<PathBuf>, Vec<String>, Value)> {
    let PackageMaterialization {
        service_dir,
        service,
        target_segments,
        target_ordinals,
        package_path,
        filesystem_target,
        external_references,
    } = request;
    let package = SettingsBytecode::read_file(package_path)?;
    if package.instances.is_empty() {
        bail!("link package is empty: {}", package_path.display());
    }
    let package_roots = settings_root_indices(&package);
    if package_roots.len() != 1 {
        bail!("link package must contain exactly one root instance");
    }
    let package_root = package_roots[0];
    if target_segments.is_empty() {
        return materialize_package_root(document, service_dir, service, &package);
    }
    let (leaf, parent_segments) = target_segments
        .split_last()
        .context("link target is missing its leaf")?;
    let leaf = leaf.clone();
    let parent_ordinals = &target_ordinals[..target_ordinals.len().saturating_sub(1)];
    let parent_index = if parent_segments.is_empty() {
        editor_service_root_index(document, service)
            .with_context(|| format!("Link target service {service} was not found"))?
    } else if let Some(parent) = resolve_editor_instance_by_path_ordinals(
        document,
        service,
        parent_segments,
        parent_ordinals,
    ) {
        parent
    } else if parent_ordinals.iter().all(|ordinal| *ordinal == 1) {
        ensure_editor_container_path(document, service, parent_segments)?
    } else {
        bail!("Link target parent with duplicate-name ordinals was not found");
    };
    let leaf_ordinal = target_ordinals.last().copied().unwrap_or(1);
    let package_indices = (0..package.instances.len()).collect::<Vec<_>>();
    let (package_path_segments, package_path_ordinals) =
        build_editor_instance_path_parts(&package, "");

    let mut removed_target = Value::Null;
    let mut planned_removals = Vec::new();
    let mut identity = PackageIdentityMatches::default();
    let same_name_children = document
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| {
            instance.parent_index == Some(parent_index) && instance.name == leaf
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if leaf_ordinal > same_name_children.len() + 1 {
        bail!("Link target ordinal {leaf_ordinal} does not exist and cannot be created");
    }
    let existing = same_name_children.get(leaf_ordinal - 1).copied();
    let insertion_anchor = existing.and_then(|existing| {
        document
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| instance.parent_index == Some(parent_index))
            .skip_while(|(index, _)| *index != existing)
            .nth(1)
            .map(|(_, instance)| instance.settings_id.clone())
    });
    if let Some(existing) = existing {
        let source_paths_before =
            build_editor_source_paths_by_index(document, service, service_dir);
        let paths_by_index = build_editor_instance_paths(document, service);
        if let Some(info) = paths_by_index
            .get(existing)
            .and_then(std::clone::Clone::clone)
        {
            removed_target = json!({
                "settingsId": document.instances[existing].settings_id.clone(),
                "className": document.instances[existing].class_name.clone(),
                "pathSegments": info.path_segments,
                "pathOrdinals": info.path_ordinals,
            });
        }
        let children = settings_children_by_parent(document);
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children, existing, &mut subtree);
        identity = package_identity_matches(document, existing, &package, package_root, false);
        prepare_package_replacement(
            document,
            &subtree,
            &identity.by_existing_index,
            external_references,
            "Package",
        )?;
        let removed =
            instance_api::remove_instance(document, InstanceSelector::Index(existing), true)?;
        planned_removals.extend(plan_editor_source_file_removals(
            service_dir,
            &source_paths_before,
            &removed,
        )?);
    }

    let mut leaf_dir = service_dir.to_path_buf();
    for segment in parent_segments {
        leaf_dir.push(segment);
    }
    leaf_dir.push(&leaf);
    if filesystem_target && leaf_ordinal == 1 && leaf_dir.exists() {
        ensure_existing_ancestor_inside(service_dir, &leaf_dir, "package target directory")?;
        if fs::symlink_metadata(&leaf_dir)?.file_type().is_symlink() {
            bail!(
                "Refusing to replace package target through directory symlink {}",
                leaf_dir.display()
            );
        }
        let planned = planned_removals
            .iter()
            .map(|path| exact_path_key(path))
            .collect::<HashSet<_>>();
        for entry in WalkDir::new(&leaf_dir).follow_links(false).min_depth(1) {
            let entry = entry.with_context(|| {
                format!("Failed to inspect package target {}", leaf_dir.display())
            })?;
            if entry.file_type().is_symlink() {
                bail!(
                    "Refusing to replace package target containing symlink {}",
                    entry.path().display()
                );
            }
            if entry.file_type().is_file() && !planned.contains(&exact_path_key(entry.path())) {
                bail!(
                    "Package target contains an unowned file that Renium will not delete: {}",
                    entry.path().display()
                );
            }
        }
    }

    let parent_index = resolve_editor_instance_by_path_ordinals(
        document,
        service,
        parent_segments,
        parent_ordinals,
    )
    .with_context(|| {
        format!(
            "Link target parent {} was not found",
            parent_segments.join(".")
        )
    })?;

    let mut existing_ids: HashSet<String> = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect();
    let mut seed = document.instances.len();
    let mut new_index_by_pkg: HashMap<usize, usize> = HashMap::new();
    let mut new_settings_ids = Vec::new();

    for (pkg_index, instance) in package.instances.iter().enumerate() {
        let is_root = instance.parent_index.is_none();
        let parent_in_doc = match instance.parent_index {
            None => Some(parent_index),
            Some(pkg_parent) => Some(*new_index_by_pkg.get(&pkg_parent).ok_or_else(|| {
                anyhow::anyhow!("link package is not in preorder (child precedes parent)")
            })?),
        };
        let name = if is_root {
            leaf.clone()
        } else {
            instance.name.clone()
        };
        let settings_id =
            if let Some(settings_id) = identity.by_package_index.get(&pkg_index).cloned() {
                if !existing_ids.insert(settings_id.clone()) {
                    bail!("Package identity mapping produced duplicate settings id {settings_id}");
                }
                settings_id
            } else {
                next_editor_settings_id_fast(&mut existing_ids, &mut seed)
            };
        let new_index = document.instances.len();
        document.instances.push(SettingsBytecodeInstance {
            settings_id: settings_id.clone(),
            name,
            class_name: instance.class_name.clone(),
            parent_index: parent_in_doc,
            properties: instance.properties.clone(),
            attributes: instance.attributes.clone(),
        });
        new_index_by_pkg.insert(pkg_index, new_index);
        new_settings_ids.push(settings_id);
    }

    let refs = build_clone_ref_map(
        &package,
        CloneRefMapInput {
            source_subtree: &package_indices,
            old_to_new_index: &new_index_by_pkg,
            path_segments_before: &package_path_segments,
            path_ordinals_before: &package_path_ordinals,
        },
    );
    for doc_index in new_index_by_pkg.values() {
        let instance = &mut document.instances[*doc_index];
        remap_internal_clone_refs_in_record(&mut instance.properties, &refs);
        remap_internal_clone_refs_in_record(&mut instance.attributes, &refs);
    }

    if let Some(anchor) = insertion_anchor {
        let inserted = new_settings_ids.iter().cloned().collect::<HashSet<_>>();
        let parent_ids = document
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.settings_id.clone(),
                    instance.parent_index.and_then(|index| {
                        document
                            .instances
                            .get(index)
                            .map(|parent| parent.settings_id.clone())
                    }),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut block = Vec::new();
        let mut remaining = Vec::new();
        for instance in std::mem::take(&mut document.instances) {
            let parent_id = parent_ids.get(&instance.settings_id).cloned().flatten();
            let entry = (instance, parent_id);
            if inserted.contains(&entry.0.settings_id) {
                block.push(entry);
            } else {
                remaining.push(entry);
            }
        }
        let position = remaining
            .iter()
            .position(|(instance, _)| instance.settings_id == anchor)
            .unwrap_or(remaining.len());
        remaining.splice(position..position, block);
        let indices = remaining
            .iter()
            .enumerate()
            .map(|(index, (instance, _))| (instance.settings_id.clone(), index))
            .collect::<HashMap<_, _>>();
        for (mut instance, parent_id) in remaining {
            instance.parent_index = parent_id.and_then(|id| indices.get(&id).copied());
            reindex_reference_indices(&mut instance.properties, &indices);
            reindex_reference_indices(&mut instance.attributes, &indices);
            document.instances.push(instance);
        }
    }

    Ok((planned_removals, new_settings_ids, removed_target))
}

struct PackageFingerprintRefs {
    lookup: BytecodeCloneRefMap,
    ordinal_by_index: HashMap<usize, usize>,
}

fn package_fingerprint_refs(
    document: &SettingsBytecode,
    subtree: &[usize],
) -> PackageFingerprintRefs {
    let all_indices = (0..document.instances.len()).collect::<Vec<_>>();
    let identity = all_indices
        .iter()
        .copied()
        .map(|index| (index, index))
        .collect::<HashMap<_, _>>();
    let (path_segments, path_ordinals) = build_editor_instance_path_parts(document, "");
    PackageFingerprintRefs {
        lookup: build_clone_ref_map(
            document,
            CloneRefMapInput {
                source_subtree: &all_indices,
                old_to_new_index: &identity,
                path_segments_before: &path_segments,
                path_ordinals_before: &path_ordinals,
            },
        ),
        ordinal_by_index: subtree
            .iter()
            .copied()
            .enumerate()
            .map(|(ordinal, index)| (index, ordinal))
            .collect(),
    }
}

fn push_package_fingerprint_json(
    value: &Value,
    refs: &PackageFingerprintRefs,
    out: &mut String,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            if map.get("_type").and_then(Value::as_str) == Some("Ref")
                && let Some(index) = ref_old_index(map, &refs.lookup)
                && let Some(ordinal) = refs.ordinal_by_index.get(&index)
            {
                out.push_str("{\"_type\":\"Ref\",\"packageOrdinal\":");
                out.push_str(&ordinal.to_string());
                out.push('}');
                return Ok(());
            }
            out.push('{');
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            for (position, key) in keys.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                if let Some(child) = map.get(*key) {
                    if *key == "Ref"
                        && let Value::Object(reference) = child
                        && let Some(index) = ref_old_index(reference, &refs.lookup)
                        && let Some(ordinal) = refs.ordinal_by_index.get(&index)
                    {
                        out.push_str("{\"_type\":\"Ref\",\"packageOrdinal\":");
                        out.push_str(&ordinal.to_string());
                        out.push('}');
                    } else {
                        push_package_fingerprint_json(child, refs, out)?;
                    }
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                push_package_fingerprint_json(item, refs, out)?;
            }
            out.push(']');
        }
        _ => out.push_str(&serde_json::to_string(value)?),
    }
    Ok(())
}

fn push_instance_subtree_fingerprint(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    root_index: usize,
    root_marker: &str,
    refs: &PackageFingerprintRefs,
    out: &mut String,
) -> Result<()> {
    let mut stack = vec![(root_index, vec![root_marker.to_string()])];
    while let Some((index, relative_path)) = stack.pop() {
        let Some(instance) = document.instances.get(index) else {
            continue;
        };
        out.push_str("path=");
        out.push_str(&serde_json::to_string(&relative_path)?);
        out.push_str(";class=");
        out.push_str(&serde_json::to_string(&instance.class_name)?);
        out.push_str(";properties=");
        push_package_fingerprint_json(&Value::Object(instance.properties.clone()), refs, out)?;
        out.push_str(";attributes=");
        push_package_fingerprint_json(&Value::Object(instance.attributes.clone()), refs, out)?;
        out.push('\n');

        if let Some(children) = children_by_parent.get(index) {
            for child_index in children.iter().rev().copied() {
                let Some(child) = document.instances.get(child_index) else {
                    continue;
                };
                let mut child_path = relative_path.clone();
                child_path.push(child.name.clone());
                stack.push((child_index, child_path));
            }
        }
    }
    Ok(())
}

fn package_document_fingerprint(package: &SettingsBytecode) -> Result<String> {
    let children_by_parent = settings_children_by_parent(package);
    let mut out = String::new();
    let roots = package
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    let mut subtree = Vec::new();
    for root_index in &roots {
        collect_settings_subtree_preorder(&children_by_parent, *root_index, &mut subtree);
    }
    let refs = package_fingerprint_refs(package, &subtree);
    for (ordinal, root_index) in roots.into_iter().enumerate() {
        push_instance_subtree_fingerprint(
            package,
            &children_by_parent,
            root_index,
            &format!("#root{ordinal}"),
            &refs,
            &mut out,
        )?;
    }
    Ok(fnv1a_hex(out.as_bytes()))
}

fn package_target_fingerprint(
    document: &SettingsBytecode,
    service: &str,
    target_segments: &[String],
    target_ordinals: &[usize],
) -> Result<Option<String>> {
    let Some(root_index) = resolve_editor_instance_by_path_ordinals(
        document,
        service,
        target_segments,
        target_ordinals,
    ) else {
        return Ok(None);
    };
    let children_by_parent = settings_children_by_parent(document);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);
    let refs = package_fingerprint_refs(document, &subtree);
    let mut out = String::new();
    push_instance_subtree_fingerprint(
        document,
        &children_by_parent,
        root_index,
        "#root0",
        &refs,
        &mut out,
    )?;
    Ok(Some(fnv1a_hex(out.as_bytes())))
}

pub(super) fn package_target_fingerprint_with_external_sources(
    document: &SettingsBytecode,
    service: &str,
    service_dir: &Path,
    target_segments: &[String],
    target_ordinals: &[usize],
) -> Result<Option<String>> {
    let mut normalized = document.clone();
    let source_paths = build_editor_source_paths_by_index(&normalized, service, service_dir);
    for (index, source_path) in source_paths.into_iter().enumerate() {
        let Some(source_path) = source_path.filter(|path| path.is_file()) else {
            continue;
        };
        let source = fs::read_to_string(&source_path)
            .with_context(|| format!("Failed to read {}", source_path.display()))?;
        if let Some(instance) = normalized.instances.get_mut(index) {
            instance
                .properties
                .insert("Source".to_string(), Value::String(source));
        }
    }
    package_target_fingerprint(&normalized, service, target_segments, target_ordinals)
}

fn package_target_settings_ids(
    document: &SettingsBytecode,
    service: &str,
    target_segments: &[String],
    target_ordinals: &[usize],
) -> Vec<String> {
    let Some(root_index) = resolve_editor_instance_by_path_ordinals(
        document,
        service,
        target_segments,
        target_ordinals,
    ) else {
        return Vec::new();
    };
    let children_by_parent = settings_children_by_parent(document);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);
    subtree
        .into_iter()
        .filter_map(|index| document.instances.get(index))
        .map(|instance| instance.settings_id.clone())
        .collect()
}

#[derive(Default)]
pub(super) struct LinkEnforcement {
    mirror_to_canonical: HashMap<String, (PathBuf, bool)>,
    canonical_to_mirrors: HashMap<String, Vec<PathBuf>>,
    pub(super) read_only_packages: Vec<ReadOnlyPackageEnforcement>,
    active: bool,
}

pub(super) struct ReadOnlyPackageEnforcement {
    pub(super) link_id: String,
    pub(super) service: String,
    pub(super) target_segments: Vec<String>,
    pub(super) target_ordinals: Vec<usize>,
    pub(super) expected_fingerprint: String,
}

impl LinkEnforcement {
    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    pub(super) fn read_only_package_for_path(
        &self,
        service: &str,
        path_segments: &[String],
        path_ordinals: &[usize],
    ) -> Option<&ReadOnlyPackageEnforcement> {
        let service_prefixed = path_segments.first().map(String::as_str) == Some(service);
        let relative = if service_prefixed {
            &path_segments[1..]
        } else {
            path_segments
        };
        let ordinal_offset =
            usize::from(service_prefixed && path_ordinals.len() == path_segments.len());
        self.read_only_packages.iter().find(|target| {
            target.service == service
                && selector_starts_with(
                    relative,
                    &path_ordinals[ordinal_offset.min(path_ordinals.len())..],
                    &target.target_segments,
                    &target.target_ordinals,
                )
        })
    }

    pub(super) fn reject_read_only_package_path(
        &self,
        service: &str,
        path_segments: &[String],
        path_ordinals: &[usize],
    ) -> Result<()> {
        if let Some(target) = self.read_only_package_for_path(service, path_segments, path_ordinals)
        {
            bail!(
                "Cannot edit read-only link \"{}\" at {}.{}. Use --override-packages to replace it intentionally.",
                target.link_id,
                target.service,
                target.target_segments.join(".")
            );
        }
        Ok(())
    }
}

pub(super) fn build_loaded_project_link_enforcement(
    loaded: &project_config::LoadedProject,
    override_packages: bool,
) -> Result<LinkEnforcement> {
    if override_packages {
        return Ok(LinkEnforcement::default());
    }
    build_link_enforcement(
        &loaded.root,
        &absolutize_under(&loaded.root, &loaded.project.source_root),
        None,
    )
}

fn link_path_key(path: &Path) -> String {
    path_key(&strip_extended_prefix(path.to_path_buf()))
}

pub(super) fn build_link_enforcement(
    project_root: &Path,
    src_root: &Path,
    cache_override: Option<&Path>,
) -> Result<LinkEnforcement> {
    let manifest_path = link_manifest_path(project_root, Path::new("renium-link.json"));
    if !manifest_path.exists() {
        return Ok(LinkEnforcement::default());
    }
    let manifest = read_link_manifest(&manifest_path)?;
    if manifest.links.is_empty() {
        return Ok(LinkEnforcement::default());
    }
    let options = LinkResolveOptions {
        cache_dir: resolve_link_cache_dir(project_root, &manifest, cache_override),
        ..LinkResolveOptions::default()
    };
    let targets = resolve_link_targets(project_root, src_root, &manifest, &options);
    let mut enforcement = LinkEnforcement {
        active: true,
        ..LinkEnforcement::default()
    };
    for target in targets {
        if target.broken {
            continue;
        }
        if !target.resolved {
            bail!(
                "Could not enforce active Renium link {} at {}: {}",
                target.link_id,
                std::iter::once(target.service.as_str())
                    .chain(target.target_segments.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join("."),
                target
                    .unresolved_reason
                    .as_deref()
                    .unwrap_or("target resolution failed")
            );
        }
        if target.read_only
            && let Some(package_path) = &target.package_source
        {
            let package = SettingsBytecode::read_file(package_path).with_context(|| {
                format!(
                    "Failed to read read-only link package {}",
                    package_path.display()
                )
            })?;
            enforcement
                .read_only_packages
                .push(ReadOnlyPackageEnforcement {
                    link_id: target.link_id.clone(),
                    service: target.service.clone(),
                    target_segments: target.target_segments.clone(),
                    target_ordinals: target.target_ordinals.clone(),
                    expected_fingerprint: package_document_fingerprint(&package)?,
                });
        }
        for pair in &target.files {
            let mirror_key = link_path_key(&pair.mirror);
            enforcement
                .mirror_to_canonical
                .insert(mirror_key, (pair.canonical.clone(), target.read_only));
            if target.source_is_local {
                enforcement
                    .canonical_to_mirrors
                    .entry(link_path_key(&pair.canonical))
                    .or_default()
                    .push(pair.mirror.clone());
            }
        }
    }
    Ok(enforcement)
}

pub(super) fn apply_link_enforcement_to_changed_paths(
    project_root: &Path,
    enforcement: &LinkEnforcement,
    changed_paths: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    if !enforcement.is_active() {
        return Ok(changed_paths);
    }
    let mut seen = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    let push = |path: PathBuf, out: &mut Vec<PathBuf>, seen: &mut HashSet<String>| {
        let key = link_path_key(&path);
        if seen.insert(key) {
            out.push(path);
        }
    };

    for path in changed_paths {
        let absolute = absolutize_under(project_root, &path);
        let key = link_path_key(&absolute);

        if let Some((canonical, read_only)) = enforcement.mirror_to_canonical.get(&key)
            && *read_only
        {
            let content = fs::read_to_string(canonical).with_context(|| {
                format!(
                    "Failed to restore protected Renium link mirror {} from {}",
                    absolute.display(),
                    canonical.display()
                )
            })?;
            write_mirror_file(&absolute, &content)?;
            set_path_readonly(&absolute, true)?;
        }

        if let Some(mirrors) = enforcement.canonical_to_mirrors.get(&key) {
            let content = fs::read_to_string(&absolute).with_context(|| {
                format!(
                    "Failed to read canonical Renium link source {}",
                    absolute.display()
                )
            })?;
            for mirror in mirrors {
                write_mirror_file(mirror, &content)?;
                let read_only = enforcement
                    .mirror_to_canonical
                    .get(&link_path_key(mirror))
                    .is_none_or(|(_, read_only)| *read_only);
                if read_only {
                    set_path_readonly(mirror, true)?;
                }
                push(mirror.clone(), &mut out, &mut seen);
            }
        }

        push(path, &mut out, &mut seen);
    }
    Ok(out)
}

fn link_project_naming(project_root: &Path) -> Result<project_config::ProjectScriptNaming> {
    Ok(project_config::try_load_project(None, Some(project_root))?
        .filter(|loaded| loaded.root == project_root)
        .map(|loaded| project_config::project_script_naming(&loaded.project))
        .unwrap_or_default())
}

fn link_slug(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "link".to_string()
    } else {
        out
    }
}
