use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Number, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::edit::{
    BytecodeCloneRefMap, CloneRefMapInput, build_clone_ref_map, collect_settings_subtree_preorder,
    plan_editor_source_file_removals, prune_empty_source_dirs, prune_removed_source_dirs,
    strict_ref_old_index,
};
use crate::bytecode::{
    acquire_settings_file_lock, apply_file_mutations, apply_file_mutations_with_permissions,
};
use crate::cli::{
    LinkAddArgs, LinkApplyArgs, LinkBreakArgs, LinkDeletePackageArgs, LinkMoveTargetArgs,
    LinkPackArgs, LinkStatusArgs, ProjectSourceArgs,
};
use crate::editor::document::{editor_child_by_stem, ensure_editor_source_target_in_bytecode};
use crate::editor::paths::{
    build_editor_instance_path_parts, build_editor_source_paths_by_index,
    infer_editor_source_path_spec_in_service,
};
use crate::editor::sync::is_lua_source_class;
use crate::project::layout::apply_configured_project_layout;
use crate::rbx::encode::bytecode_export_script_source;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode,
    SettingsBytecodeInstance, encode_settings_bytecode,
};
use crate::settings::instance::{self as instance_api, InstanceSelector};
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::system::files::{
    absolutize_under, ensure_existing_ancestor_inside, exact_path_key, fnv1a_hex, path_key,
    resolve_link_project_root, validate_filesystem_instance_name,
};

use super::{
    GLOBAL_LINK_PREFIX, LinkEntry, LinkLockEntry, LinkManifest, LinkResolveOptions, LinkSource,
    LinkSourceMeta, LinkTargetRef, LinkTargetStorage, PackageMaterialization, RENIUM_DIR_GITIGNORE,
    RENIUM_STORE_EXTENSION, ResolvedLinkTarget, collect_project_settings_files,
    ensure_settings_document, is_global_link_path, is_package_path, link_lock_path,
    link_manifest_path, link_mirror_lock_key, link_slug, link_target_document_selector,
    link_target_document_selector_parts, link_target_from_settings_id, link_target_key,
    link_target_ordinals, link_target_ref_key, link_target_segments, load_settings_documents,
    mark_manifest_target_broken, materialize_package_target, package_document_fingerprint,
    package_lock_key, package_target_fingerprint, package_target_matches,
    package_target_settings_ids, read_link_lock, read_link_manifest, read_link_source_meta,
    referenced_settings_ids_outside, renium_global_packages_dir, resolve_link_cache_dir,
    resolve_link_target_storage, resolve_link_targets, resolve_local_link_path,
    selector_starts_with, serialize_link_lock, serialize_link_manifest,
    stage_settings_document_writes, validate_link_target_ref, write_link_manifest,
};

fn load_link_project(
    project: &mut ProjectSourceArgs,
    manifest: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf, LinkManifest)> {
    apply_configured_project_layout(&mut project.project_root, &mut project.src_root)?;
    let project_root = resolve_link_project_root(&project.project_root)?;
    let src_root = absolutize_under(&project_root, &project.src_root);
    let manifest_path = link_manifest_path(&project_root, manifest);
    let manifest = read_link_manifest(&manifest_path)?;
    Ok((project_root, src_root, manifest_path, manifest))
}

fn migrate_link_target_lock(
    entry: &mut LinkLockEntry,
    old_service: &str,
    old_segments: &[String],
    old_ordinals: &[usize],
    new_service: &str,
    new_segments: &[String],
    new_ordinals: &[usize],
) -> bool {
    let mut changed = false;
    for prefix in ["package", "package-target"] {
        let old_key = package_lock_key(prefix, old_service, old_segments, old_ordinals);
        let new_key = package_lock_key(prefix, new_service, new_segments, new_ordinals);
        if let Some(value) = entry.files.remove(&old_key) {
            entry.files.insert(new_key, value);
            changed = true;
        }
    }
    let old_key = link_target_key(old_service, old_segments, old_ordinals);
    let new_key = link_target_key(new_service, new_segments, new_ordinals);
    if let Some(target) = entry.targets.remove(&old_key) {
        entry.targets.insert(new_key, target);
        changed = true;
    }
    changed
}

fn reconcile_package_target_names(
    project_root: &Path,
    src_root: &Path,
    targets: &mut [ResolvedLinkTarget],
    documents: &HashMap<PathBuf, SettingsBytecode>,
    manifest: &mut LinkManifest,
    lock: &mut super::LinkLock,
) -> Result<bool> {
    let mut changed = false;
    for target in targets {
        if target.package_source.is_none() || !target.resolved {
            continue;
        }
        let Some(storage) = target.storage.as_ref() else {
            continue;
        };
        let Some(settings_file) = storage.settings_file.as_ref() else {
            continue;
        };
        let old_segments = target.target_segments.clone();
        let old_ordinals = target.target_ordinals.clone();
        let old_key = link_target_key(&target.service, &old_segments, &old_ordinals);
        let Some(settings_id) = lock
            .entries
            .get(&target.link_id)
            .and_then(|entry| entry.targets.get(&old_key))
            .and_then(|target| target.settings_id.as_deref())
        else {
            continue;
        };
        let Some(document) = documents.get(settings_file) else {
            continue;
        };
        let Some(actual) = link_target_from_settings_id(target, document, settings_id) else {
            continue;
        };
        let new_segments = link_target_segments(&actual);
        let new_ordinals = link_target_ordinals(&actual);
        if old_segments == new_segments && old_ordinals == new_ordinals {
            continue;
        }
        let new_ref_key = link_target_ref_key(&actual);
        if manifest.links.iter().any(|link| {
            link.targets
                .iter()
                .any(|candidate| link_target_ref_key(candidate) == new_ref_key)
        }) {
            bail!(
                "Renamed link target {}.{} conflicts with another link target",
                target.service,
                new_segments.join(".")
            );
        }
        let new_storage = resolve_link_target_storage(project_root, src_root, &actual, true, true)?;
        let old_ref_key = link_target_key(&target.service, &old_segments, &old_ordinals);
        let manifest_target = manifest
            .links
            .iter_mut()
            .find(|link| link.id == target.link_id)
            .and_then(|link| {
                link.targets
                    .iter_mut()
                    .find(|candidate| link_target_ref_key(candidate) == old_ref_key)
            })
            .context("Renium link target disappeared while updating its name")?;
        *manifest_target = actual;
        manifest.broken.retain(|candidate| {
            let key = link_target_ref_key(candidate);
            key != old_ref_key && key != new_ref_key
        });
        if let Some(entry) = lock.entries.get_mut(&target.link_id) {
            migrate_link_target_lock(
                entry,
                &target.service,
                &old_segments,
                &old_ordinals,
                &target.service,
                &new_segments,
                &new_ordinals,
            );
        }
        target.target_segments = new_segments;
        target.target_ordinals = new_ordinals;
        target.storage = Some(new_storage);
        target.broken = false;
        changed = true;
    }
    Ok(changed)
}

fn parse_link_target(service: &str, path: &str, ordinals: &str) -> Result<LinkTargetRef> {
    let target = LinkTargetRef {
        service: service.to_string(),
        path: serde_json::from_str(path).context("Failed to parse link target path JSON")?,
        ords: serde_json::from_str(ordinals)
            .context("Failed to parse link target ordinals JSON")?,
    };
    validate_link_target_ref(&target)?;
    Ok(target)
}

fn link_target_output_ordinals(ordinals: Vec<usize>) -> Vec<usize> {
    if ordinals.iter().all(|ordinal| *ordinal == 1) {
        Vec::new()
    } else {
        ordinals
    }
}

#[derive(Default)]
struct LinkApplyChanges {
    changed_paths: Vec<String>,
    changed_seen: HashSet<String>,
    target_settings_ids: Vec<String>,
    settings_seen: HashSet<String>,
    link_results: Vec<Value>,
    warnings: Vec<String>,
    processed_targets: usize,
    differences: usize,
    manifest_changed: bool,
    transaction_writes: BTreeMap<PathBuf, Vec<u8>>,
    transaction_removals: Vec<PathBuf>,
    transaction_prune_dirs: Vec<(PathBuf, PathBuf)>,
    mirror_permissions: BTreeMap<PathBuf, bool>,
}

impl LinkApplyChanges {
    fn mark_path(&mut self, path: &Path) {
        if self.changed_seen.insert(path_key(path)) {
            self.changed_paths.push(path.to_string_lossy().into_owned());
        }
    }

    fn mark_settings_ids(&mut self, ids: &[String]) {
        for id in ids {
            if self.settings_seen.insert(id.clone()) {
                self.target_settings_ids.push(id.clone());
            }
        }
    }

    fn remove_package_path(&mut self, source_root: &Path, path: PathBuf) {
        if let Some(parent) = path.parent() {
            self.transaction_prune_dirs
                .push((source_root.to_path_buf(), parent.to_path_buf()));
        }
        self.mark_path(&path);
        self.transaction_removals.push(path);
    }

    fn mark_deleted_target(
        &mut self,
        manifest: &mut LinkManifest,
        target: &ResolvedLinkTarget,
        check: bool,
        package: bool,
    ) {
        if !check {
            self.manifest_changed |= mark_manifest_target_broken(manifest, target);
        }
        self.differences += 1;
        self.processed_targets += 1;
        let mut result = json!({
            "id": target.link_id,
            "service": target.service,
            "path": target.target_segments,
            "readOnly": target.read_only,
            "resolvedRef": target.resolved_ref,
            "skipped": true,
            "broken": true,
            "deletedTarget": true,
        });
        if package {
            result["package"] = Value::Bool(true);
        }
        self.link_results.push(result);
    }
}

struct PackageLinkApply<'a> {
    args: &'a LinkApplyArgs,
    target: &'a ResolvedLinkTarget,
    target_forced: bool,
    storage: &'a LinkTargetStorage,
    settings_file: &'a Path,
    document_selector: &'a (String, Vec<String>, Vec<usize>),
    external_references: &'a HashSet<String>,
    document: &'a mut SettingsBytecode,
    lock_entry: &'a mut LinkLockEntry,
    manifest: &'a mut LinkManifest,
    changes: &'a mut LinkApplyChanges,
}

impl PackageLinkApply<'_> {
    fn apply(self) -> Result<()> {
        let Self {
            args,
            target,
            target_forced,
            storage,
            settings_file,
            document_selector,
            external_references,
            document,
            lock_entry,
            manifest,
            changes,
        } = self;
        let package_path = target
            .package_source
            .as_ref()
            .context("Package link target has no package source")?;
        let (document_service, document_segments, document_ordinals) = document_selector;
        let lock_key = package_lock_key(
            "package",
            &target.service,
            &target.target_segments,
            &target.target_ordinals,
        );
        let target_fingerprint_key = package_lock_key(
            "package-target",
            &target.service,
            &target.target_segments,
            &target.target_ordinals,
        );
        let package_doc = SettingsBytecode::read_file(package_path)?;
        let package_fingerprint = package_document_fingerprint(&package_doc)?;
        let package_hash = fs::read(package_path).map(|bytes| fnv1a_hex(&bytes)).ok();
        let target_exists = resolve_editor_instance_by_path_ordinals(
            document,
            document_service,
            document_segments,
            document_ordinals,
        )
        .is_some();
        let target_fingerprint = if target_exists {
            package_target_fingerprint(
                document,
                document_service,
                document_segments,
                document_ordinals,
            )?
        } else {
            None
        };
        let target_matches_package = target_exists
            && package_target_matches(
                document,
                document_service,
                document_segments,
                document_ordinals,
                &package_doc,
                &package_fingerprint,
            )?;
        if !target_exists && lock_entry.files.contains_key(&lock_key) && !target_forced {
            changes.mark_deleted_target(manifest, target, args.check, true);
            return Ok(());
        }
        let previous_package_hash = lock_entry.files.get(&lock_key).cloned();
        let previous_target_fingerprint = lock_entry.files.get(&target_fingerprint_key).cloned();
        let has_three_way_baseline =
            previous_package_hash.is_some() && previous_target_fingerprint.is_some();
        let package_changed_from_baseline = previous_package_hash
            .as_ref()
            .zip(package_hash.as_ref())
            .is_some_and(|(previous, current)| previous != current);
        let target_changed_from_baseline = previous_target_fingerprint
            .as_ref()
            .is_some_and(|previous| target_fingerprint.as_ref() != Some(previous));
        let unchanged = target_exists
            && target_matches_package
            && package_hash.is_some()
            && lock_entry.files.get(&lock_key) == package_hash.as_ref();
        let package_conflict = !target.read_only
            && has_three_way_baseline
            && package_changed_from_baseline
            && target_changed_from_baseline
            && !target_matches_package;
        let preserved_target_edits = !target.read_only
            && target_exists
            && !target_matches_package
            && (!has_three_way_baseline
                || target_changed_from_baseline && !package_changed_from_baseline
                || package_conflict);

        if args.check {
            if !unchanged {
                changes.differences += 1;
            }
        } else if !unchanged && (!preserved_target_edits || target_forced) {
            if !target_matches_package {
                let (removals, settings_ids, _) = materialize_package_target(
                    document,
                    PackageMaterialization {
                        service_dir: &storage.source_root,
                        service: document_service,
                        target_segments: document_segments,
                        target_ordinals: document_ordinals,
                        package_path,
                        filesystem_target: storage.filesystem_target,
                        external_references,
                    },
                )?;
                for path in removals {
                    changes.remove_package_path(&storage.source_root, path);
                }
                changes.mark_settings_ids(&settings_ids);
                changes.mark_path(settings_file);
                changes.differences += 1;
            }
            if let Some(hash) = package_hash {
                lock_entry.files.insert(lock_key, hash);
            }
            lock_entry
                .files
                .insert(target_fingerprint_key, package_fingerprint);
        } else if target_forced {
            let settings_ids = package_target_settings_ids(
                document,
                document_service,
                document_segments,
                document_ordinals,
            );
            changes.mark_settings_ids(&settings_ids);
            if !settings_ids.is_empty() {
                changes.mark_path(settings_file);
            }
        } else if !preserved_target_edits
            && lock_entry.files.get(&target_fingerprint_key) != Some(&package_fingerprint)
        {
            lock_entry
                .files
                .insert(target_fingerprint_key, package_fingerprint);
        } else if preserved_target_edits {
            changes.differences += 1;
            if package_conflict {
                changes.warnings.push(format!(
                    "{}: both the package and writable target {}.{} changed since the last apply",
                    target.link_id,
                    target.service,
                    target.target_segments.join(".")
                ));
            }
        }
        changes.processed_targets += 1;
        let settings_ids = package_target_settings_ids(
            document,
            document_service,
            document_segments,
            document_ordinals,
        );
        let root_settings_id = settings_ids.first().cloned();
        lock_entry
            .targets
            .entry(link_target_key(
                &target.service,
                &target.target_segments,
                &target.target_ordinals,
            ))
            .or_default()
            .settings_id
            .clone_from(&root_settings_id);
        changes.link_results.push(json!({
            "id": target.link_id,
            "service": target.service,
            "path": target.target_segments,
            "rootSettingsId": root_settings_id,
            "readOnly": target.read_only,
            "resolvedRef": target.resolved_ref,
            "settingsIds": settings_ids,
            "package": true,
            "skipped": unchanged || preserved_target_edits,
            "forced": target_forced && (unchanged || preserved_target_edits),
            "targetDrift": target_exists && !target_matches_package,
            "preservedEdits": preserved_target_edits,
            "conflict": package_conflict,
        }));
        Ok(())
    }
}

struct MirrorLinkApply<'a> {
    args: &'a LinkApplyArgs,
    project_root: &'a Path,
    target: &'a ResolvedLinkTarget,
    target_forced: bool,
    storage: &'a LinkTargetStorage,
    document_selector: Option<&'a (String, Vec<String>, Vec<usize>)>,
    settings_file: Option<&'a Path>,
    documents: &'a mut HashMap<PathBuf, SettingsBytecode>,
    lock_entry: &'a mut LinkLockEntry,
    manifest: &'a mut LinkManifest,
    changes: &'a mut LinkApplyChanges,
}

impl MirrorLinkApply<'_> {
    fn apply(self) -> Result<()> {
        let Self {
            args,
            project_root,
            target,
            target_forced,
            storage,
            document_selector,
            settings_file,
            documents,
            lock_entry,
            manifest,
            changes,
        } = self;
        let mut settings_ids = Vec::new();
        let mut source_writes = Vec::new();
        let mut preserved_edits = Vec::new();
        let target_lock_key = link_target_key(
            &target.service,
            &target.target_segments,
            &target.target_ordinals,
        );
        let target_lock = lock_entry.targets.entry(target_lock_key).or_default();
        let locked_mirror_keys = target
            .files
            .iter()
            .map(|pair| link_mirror_lock_key(project_root, &pair.mirror))
            .collect::<Vec<_>>();
        let target_was_previously_applied = locked_mirror_keys
            .iter()
            .any(|key| target_lock.files.contains_key(key));
        let target_exists = if let Some((service, segments, ordinals)) = document_selector
            && let Some(settings_file) = settings_file
            && let Some(document) = documents.get(settings_file)
        {
            resolve_editor_instance_by_path_ordinals(document, service, segments, ordinals)
                .is_some()
        } else {
            target.files.iter().any(|pair| pair.mirror.exists())
        };
        let missing_locked_mirror = target
            .files
            .iter()
            .zip(&locked_mirror_keys)
            .any(|(pair, key)| target_lock.files.contains_key(key) && !pair.mirror.exists());
        if target_was_previously_applied
            && (!target_exists || missing_locked_mirror)
            && !target_forced
        {
            changes.mark_deleted_target(manifest, target, args.check, false);
            return Ok(());
        }
        for (pair, lock_key) in target.files.iter().zip(&locked_mirror_keys) {
            let content = fs::read_to_string(&pair.canonical).with_context(|| {
                format!("Failed to read link source {}", pair.canonical.display())
            })?;
            let mirror_current = fs::read_to_string(&pair.mirror).ok();
            let locally_edited = !target_forced
                && !target.read_only
                && mirror_current.as_ref().is_some_and(|current| {
                    target_lock
                        .files
                        .get(lock_key)
                        .is_some_and(|synced| *synced != fnv1a_hex(current.as_bytes()))
                });
            let mirror_changed = mirror_current.as_deref() != Some(content.as_str());
            if !args.check && !locally_edited && mirror_changed {
                changes
                    .transaction_writes
                    .insert(pair.mirror.clone(), content.as_bytes().to_vec());
            }
            if mirror_changed {
                changes.differences += 1;
            }
            if locally_edited {
                preserved_edits.push(pair.mirror.to_string_lossy().into_owned());
            }
            let mut structure_changed = false;
            let mut target_settings_id = None;
            if let Some((service, _, _)) = document_selector
                && let Some(settings_file) = settings_file
                && let Some(document) = documents.get_mut(settings_file)
                && let Some(spec) = infer_editor_source_path_spec_in_service(
                    &storage.source_root,
                    service,
                    &pair.mirror,
                )
            {
                let ensured = ensure_editor_source_target_in_bytecode(document, &spec)?;
                structure_changed = ensured.changed;
                if let Some(id) = ensured.target.settings_id {
                    settings_ids.push(id.clone());
                    target_settings_id = Some(id);
                }
                if structure_changed {
                    changes.mark_path(settings_file);
                }
            }
            let target_changed = structure_changed || mirror_changed && !locally_edited;
            if target_changed {
                if let Some(id) = target_settings_id {
                    changes.mark_settings_ids(&[id]);
                }
                changes.mark_path(&pair.mirror);
                source_writes.push(json!({
                    "path": pair.mirror.to_string_lossy(),
                }));
            }
            if !locally_edited {
                target_lock
                    .files
                    .insert(lock_key.clone(), fnv1a_hex(content.as_bytes()));
            }
            if !args.check {
                changes
                    .mirror_permissions
                    .insert(pair.mirror.clone(), target.read_only);
            }
        }
        let current_mirror_keys = locked_mirror_keys.into_iter().collect::<HashSet<_>>();
        let stale_keys = target_lock
            .files
            .keys()
            .filter(|key| !current_mirror_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in stale_keys {
            let mirror = project_root.join(key.replace('/', std::path::MAIN_SEPARATOR_STR));
            let locally_edited = !target_forced
                && !target.read_only
                && fs::read_to_string(&mirror).ok().is_some_and(|current| {
                    target_lock
                        .files
                        .get(&key)
                        .is_some_and(|synced| *synced != fnv1a_hex(current.as_bytes()))
                });
            if args.check {
                if mirror.exists() {
                    changes.differences += 1;
                }
                continue;
            }
            if locally_edited {
                preserved_edits.push(mirror.to_string_lossy().into_owned());
            } else if mirror.exists() {
                changes.transaction_removals.push(mirror.clone());
                changes.differences += 1;
                changes.mark_path(&mirror);
            }
            target_lock.files.remove(&key);
        }
        changes.processed_targets += 1;
        let root_settings_id = document_selector.zip(settings_file).and_then(
            |((service, segments, ordinals), settings_file)| {
                let document = documents.get(settings_file)?;
                let index = resolve_editor_instance_by_path_ordinals(
                    document, service, segments, ordinals,
                )?;
                Some(document.instances[index].settings_id.clone())
            },
        );
        changes.link_results.push(json!({
            "id": target.link_id,
            "service": target.service,
            "path": target.target_segments,
            "rootSettingsId": root_settings_id,
            "readOnly": target.read_only,
            "resolvedRef": target.resolved_ref,
            "settingsIds": settings_ids,
            "sourceWrites": source_writes,
            "preservedEdits": preserved_edits,
        }));
        Ok(())
    }
}

pub(crate) fn link_apply(mut args: LinkApplyArgs) -> Result<()> {
    let (project_root, src_root, manifest_path, mut manifest) =
        load_link_project(&mut args.project, &args.manifest)?;
    let options = LinkResolveOptions {
        only_link: args.link.clone(),
        offline: args.offline || args.check,
        fetch: !args.offline && !args.check,
        read_only: args.check,
        allow_missing_store: true,
        git_path: args.git_path.clone(),
        wally_path: args.wally_path.clone(),
        cache_dir: resolve_link_cache_dir(&project_root, &manifest, args.cache_dir.as_deref()),
    };
    let mut targets = resolve_link_targets(&project_root, &src_root, &manifest, &options);
    let settings_files = collect_project_settings_files(
        &src_root,
        targets
            .iter()
            .filter(|target| target.resolved && !target.broken)
            .filter_map(|target| target.storage.as_ref()?.settings_file.clone()),
    )?;
    let mut settings_guards = Vec::with_capacity(settings_files.len());
    for settings_file in settings_files
        .iter()
        .filter(|path| path.is_file() || !args.check)
    {
        settings_guards.push(acquire_settings_file_lock(settings_file)?);
    }

    let existing_settings_files = settings_files
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let (mut documents, mut settings_outputs) = load_settings_documents(&existing_settings_files)?;
    for target in targets
        .iter()
        .filter(|target| target.resolved && !target.broken)
    {
        let Some(storage) = target.storage.as_ref() else {
            continue;
        };
        ensure_settings_document(
            &mut documents,
            &mut settings_outputs,
            storage,
            &target.service,
        );
    }
    let mut lock = read_link_lock(&project_root)?;
    let manifest_changed = reconcile_package_target_names(
        &project_root,
        &src_root,
        &mut targets,
        &documents,
        &mut manifest,
        &mut lock,
    )?;
    let mut changes = LinkApplyChanges {
        manifest_changed,
        ..Default::default()
    };

    let forced_targets: Vec<LinkTargetRef> = args
        .force_target
        .iter()
        .map(|raw| {
            let target: LinkTargetRef =
                serde_json::from_str(raw).context("Failed to parse --force-target selector")?;
            validate_link_target_ref(&target)?;
            Ok(target)
        })
        .collect::<Result<_>>()?;

    for target in &targets {
        if target.broken {
            continue;
        }
        let target_forced = args.force_targets
            || forced_targets.iter().any(|forced| {
                forced.service == target.service
                    && link_target_segments(forced) == target.target_segments
                    && link_target_ordinals(forced) == target.target_ordinals
            });
        if !target.resolved {
            if target.storage.is_none() {
                bail!(
                    "{}: {}",
                    target.link_id,
                    target.unresolved_reason.clone().unwrap_or_default()
                );
            }
            changes.warnings.push(format!(
                "{}: {}",
                target.link_id,
                target.unresolved_reason.clone().unwrap_or_default()
            ));
            continue;
        }
        let storage = target
            .storage
            .as_ref()
            .context("Resolved link target has no ownership information")?;
        if let Some(settings_file) = storage.settings_file.as_ref()
            && !documents.contains_key(settings_file)
        {
            documents.insert(
                settings_file.clone(),
                SettingsBytecode::read_file(settings_file).with_context(|| {
                    format!(
                        "Failed to read {} link owner settings at {}",
                        storage.owner,
                        settings_file.display()
                    )
                })?,
            );
        }
        if let Some(settings_file) = storage.settings_file.as_ref() {
            settings_outputs.insert(
                settings_file.clone(),
                storage
                    .settings_output_file
                    .clone()
                    .unwrap_or_else(|| settings_file.clone()),
            );
        }
        let document_key = storage.settings_file.as_ref();
        let document_selector = if let Some(settings_file) = document_key
            && let Some(document) = documents.get(settings_file)
        {
            Some(link_target_document_selector(target, document)?)
        } else {
            None
        };
        let lock_entry = lock.entries.entry(target.link_id.clone()).or_default();
        lock_entry.resolved_ref.clone_from(&target.resolved_ref);

        if target.package_source.is_some() {
            let settings_file = document_key
                .context("Package link target has no writable bytecode settings store")?;
            let document_selector = document_selector
                .as_ref()
                .context("Package link target has no bytecode selector")?;
            let external_references = referenced_settings_ids_outside(&documents, settings_file);
            let document = documents
                .get_mut(settings_file)
                .context("Package link target settings were not loaded")?;
            PackageLinkApply {
                args: &args,
                target,
                target_forced,
                storage,
                settings_file,
                document_selector,
                external_references: &external_references,
                document,
                lock_entry,
                manifest: &mut manifest,
                changes: &mut changes,
            }
            .apply()?;
            continue;
        }

        MirrorLinkApply {
            args: &args,
            project_root: &project_root,
            target,
            target_forced,
            storage,
            document_selector: document_selector.as_ref(),
            settings_file: document_key.map(PathBuf::as_path),
            documents: &mut documents,
            lock_entry,
            manifest: &mut manifest,
            changes: &mut changes,
        }
        .apply()?;
    }

    if !args.check {
        stage_settings_document_writes(
            &documents,
            &settings_outputs,
            &mut changes.transaction_writes,
            &mut changes.transaction_removals,
        )?;
        changes
            .transaction_writes
            .insert(link_lock_path(&project_root), serialize_link_lock(&lock)?);
        if changes.manifest_changed {
            changes.transaction_writes.insert(
                manifest_path.clone(),
                serialize_link_manifest(&manifest)?.into_bytes(),
            );
        }
        let gitignore = project_root.join(".renium").join(".gitignore");
        if !gitignore.exists() {
            changes
                .transaction_writes
                .insert(gitignore, RENIUM_DIR_GITIGNORE.as_bytes().to_vec());
        }
        changes
            .transaction_removals
            .sort_by_key(|path| exact_path_key(path));
        changes
            .transaction_removals
            .dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
        apply_file_mutations_with_permissions(
            &changes.transaction_writes,
            &changes.transaction_removals,
            &changes.mirror_permissions,
        )?;
        changes
            .transaction_prune_dirs
            .sort_by_key(|(_, directory)| std::cmp::Reverse(directory.components().count()));
        changes.transaction_prune_dirs.dedup_by(|left, right| {
            exact_path_key(&left.0) == exact_path_key(&right.0)
                && exact_path_key(&left.1) == exact_path_key(&right.1)
        });
        for (root, directory) in changes.transaction_prune_dirs.drain(..) {
            let _ = prune_empty_source_dirs(&root, &directory);
        }
        drop(settings_guards);
    }

    let strict_failure = args.strict && !changes.warnings.is_empty();
    print_json_output(
        &json!({
            "ok": !strict_failure,
            "check": args.check,
            "manifest": manifest_path,
            "processedTargets": changes.processed_targets,
            "differenceCount": changes.differences,
            "changedPaths": changes.changed_paths,
            "changedSettingsIds": changes.target_settings_ids,
            "links": changes.link_results,
            "warnings": changes.warnings,
        }),
        args.pretty,
    )?;
    if strict_failure {
        bail!(
            "link-apply finished with {} warning(s) and --strict is set",
            changes.warnings.len()
        );
    }
    Ok(())
}

pub(crate) fn link_break(mut args: LinkBreakArgs) -> Result<()> {
    let (project_root, src_root, manifest_path, mut manifest) =
        load_link_project(&mut args.project, &args.manifest)?;

    let known_target_keys: HashSet<String> = manifest
        .links
        .iter()
        .flat_map(|link| link.targets.iter())
        .map(link_target_ref_key)
        .collect();

    let mut to_break: Vec<LinkTargetRef> = Vec::new();
    if let Some(link_id) = &args.link {
        let link = manifest
            .links
            .iter()
            .find(|link| &link.id == link_id)
            .ok_or_else(|| {
                anyhow::anyhow!("No link with id {link_id} in {}", manifest_path.display())
            })?;
        to_break.extend(link.targets.iter().cloned());
    } else if let (Some(service), Some(path_json)) = (&args.service, &args.path_segments_json) {
        let target = parse_link_target(service, path_json, &args.path_ordinals_json)?;
        validate_link_target_ref(&target)?;
        if !known_target_keys.contains(&link_target_ref_key(&target)) {
            bail!(
                "{}.{} is not a renium-link target; nothing to break.",
                target.service,
                target.path.last().map_or("", String::as_str)
            );
        }
        to_break.push(target);
    } else {
        bail!(
            "Specify --link <id> to break a whole link, or --service and --path to break one target."
        );
    }

    let mut broken_keys: HashSet<String> = HashSet::new();
    let mut broken_out: Vec<Value> = Vec::new();
    for target in &to_break {
        validate_link_target_ref(target)?;
        let key = link_target_ref_key(target);
        broken_keys.insert(key);
        broken_out.push(json!({
            "service": target.service,
            "path": link_target_segments(target),
            "ords": link_target_output_ordinals(link_target_ordinals(target)),
        }));
    }

    let options = LinkResolveOptions {
        cache_dir: resolve_link_cache_dir(&project_root, &manifest, args.cache_dir.as_deref()),
        ..LinkResolveOptions::default()
    };
    let resolved = resolve_link_targets(&project_root, &src_root, &manifest, &options);
    let mut mirrors_to_unlock = Vec::new();
    let mut package_target_keys = HashSet::new();
    let mut package_settings_files = Vec::new();
    for target in &resolved {
        let key = link_target_key(
            &target.service,
            &target.target_segments,
            &target.target_ordinals,
        );
        if !broken_keys.contains(&key) {
            continue;
        }
        for pair in &target.files {
            mirrors_to_unlock.push(pair.mirror.clone());
        }
        if target.package_source.is_some() {
            package_target_keys.insert(key);
            if let Some(settings_file) = target
                .storage
                .as_ref()
                .and_then(|storage| storage.settings_file.clone())
            {
                package_settings_files.push(settings_file);
            }
        }
    }

    let package_targets = to_break
        .iter()
        .filter(|target| package_target_keys.contains(&link_target_ref_key(target)))
        .cloned()
        .collect::<Vec<_>>();
    let mut settings_guards = Vec::new();
    let mut documents = HashMap::new();
    let mut settings_outputs = HashMap::new();
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let mut externalized_source_paths = Vec::new();
    if !package_targets.is_empty() {
        let settings_files = collect_project_settings_files(&src_root, package_settings_files)?;
        settings_guards.reserve(settings_files.len());
        for settings_file in &settings_files {
            settings_guards.push(acquire_settings_file_lock(settings_file)?);
        }
        for target in &package_targets {
            let (_, paths) = plan_unlink_link_target_instance(
                &project_root,
                &src_root,
                target,
                &mut documents,
                &mut settings_outputs,
                &mut writes,
            )?;
            externalized_source_paths.extend(paths);
        }
        stage_settings_document_writes(&documents, &settings_outputs, &mut writes, &mut removals)?;
    }

    if args.remove {
        manifest
            .broken
            .retain(|target| !broken_keys.contains(&link_target_ref_key(target)));
        for link in &mut manifest.links {
            link.targets
                .retain(|target| !broken_keys.contains(&link_target_ref_key(target)));
        }
        manifest.links.retain(|link| !link.targets.is_empty());
    } else {
        let existing: HashSet<String> = manifest.broken.iter().map(link_target_ref_key).collect();
        for target in to_break {
            let key = link_target_ref_key(&target);
            if !existing.contains(&key) {
                manifest.broken.push(target);
            }
        }
    }

    let mut mirror_permissions = mirrors_to_unlock
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| (path, false))
        .collect::<BTreeMap<_, _>>();
    mirror_permissions.extend(
        externalized_source_paths
            .iter()
            .map(|path| (PathBuf::from(path), false)),
    );
    let remove_manifest = args.remove && manifest.links.is_empty() && manifest.broken.is_empty();
    if remove_manifest {
        removals.push(manifest_path.clone());
    } else {
        writes.insert(
            manifest_path.clone(),
            serialize_link_manifest(&manifest)?.into_bytes(),
        );
    }
    let mut lock_removed = None;
    if args.remove {
        let active_links = manifest
            .links
            .iter()
            .map(|link| link.id.as_str())
            .collect::<HashSet<_>>();
        let mut lock = read_link_lock(&project_root)?;
        lock.entries
            .retain(|link_id, _| active_links.contains(link_id.as_str()));
        let lock_path = link_lock_path(&project_root);
        let removed = lock.entries.is_empty();
        lock_removed = Some(removed);
        if removed {
            removals.push(lock_path);
        } else {
            writes.insert(lock_path, serialize_link_lock(&lock)?);
        }
    }
    apply_file_mutations_with_permissions(&writes, &removals, &mirror_permissions)?;
    let mut result = json!({
        "ok": true,
        "manifest": manifest_path,
        "manifestRemoved": remove_manifest,
        "targetsRetained": true,
        "unlockedFiles": mirror_permissions.len(),
        "externalizedSourcePaths": externalized_source_paths,
    });
    if let Some(removed) = lock_removed {
        result["lockRemoved"] = json!(removed);
    }
    result[if args.remove { "removed" } else { "broken" }] = Value::Array(broken_out);
    print_json_output(&result, args.pretty)
}

fn link_target_source_instance_count(target: &ResolvedLinkTarget) -> usize {
    let Some(storage) = target.storage.as_ref() else {
        return 0;
    };
    let mut target_path = Vec::with_capacity(target.target_segments.len() + 1);
    target_path.push(target.service.clone());
    target_path.extend(target.target_segments.iter().cloned());
    let mut instances = HashSet::new();
    for pair in &target.files {
        let Some(spec) = infer_editor_source_path_spec_in_service(
            &storage.source_root,
            &target.service,
            &pair.mirror,
        ) else {
            continue;
        };
        if !spec.path_segments.starts_with(&target_path) {
            continue;
        }
        for depth in target_path.len()..=spec.path_segments.len() {
            instances.insert(spec.path_segments[..depth].to_vec());
        }
    }
    instances.len()
}

pub(crate) fn link_status(mut args: LinkStatusArgs) -> Result<()> {
    let (project_root, src_root, manifest_path, manifest) =
        load_link_project(&mut args.project, &args.manifest)?;
    let options = LinkResolveOptions {
        cache_dir: resolve_link_cache_dir(&project_root, &manifest, args.cache_dir.as_deref()),
        ..LinkResolveOptions::default()
    };
    let resolved = resolve_link_targets(&project_root, &src_root, &manifest, &options);

    let mut meta_by_link: HashMap<String, LinkSourceMeta> = HashMap::new();
    let mut source_instances_by_link: HashMap<String, usize> = HashMap::new();
    let mut source_path_by_link: HashMap<String, PathBuf> = HashMap::new();
    let mut package_fingerprints = HashMap::<PathBuf, Option<String>>::new();
    let mut settings_documents = HashMap::<PathBuf, Option<SettingsBytecode>>::new();
    let mut file_contents = HashMap::<PathBuf, Option<String>>::new();
    for target in &resolved {
        let source_instances = link_target_source_instance_count(target);
        source_instances_by_link
            .entry(target.link_id.clone())
            .and_modify(|count| *count = (*count).max(source_instances))
            .or_insert(source_instances);
        if let Some(source_path) = &target.source_path {
            meta_by_link
                .entry(target.link_id.clone())
                .or_insert_with(|| read_link_source_meta(source_path));
            source_path_by_link
                .entry(target.link_id.clone())
                .or_insert_with(|| source_path.clone());
        }
    }

    let mut targets_out: Vec<Value> = Vec::new();
    let mut drifted = 0usize;
    let mut broken = 0usize;
    let mut active_target_count_by_link: HashMap<String, usize> = HashMap::new();
    for target in &resolved {
        let mut drift = false;
        let mut missing = false;
        let mut mirrors: Vec<Value> = Vec::new();
        if let Some(package_path) = &target.package_source
            && target.resolved
        {
            let expected_fingerprint = package_fingerprints
                .entry(package_path.clone())
                .or_insert_with(|| {
                    SettingsBytecode::read_file(package_path)
                        .and_then(|package| package_document_fingerprint(&package))
                        .ok()
                })
                .as_deref();
            let mut package_root_found = false;
            if let Some(storage) = target.storage.as_ref()
                && let Some(settings_file) = storage.settings_file.as_ref()
                && let Some(doc) = settings_documents
                    .entry(settings_file.clone())
                    .or_insert_with(|| SettingsBytecode::read_file(settings_file).ok())
                    .as_ref()
                && let Ok((document_service, document_segments, document_ordinals)) =
                    link_target_document_selector(target, doc)
                && let Some(root_index) = resolve_editor_instance_by_path_ordinals(
                    doc,
                    &document_service,
                    &document_segments,
                    &document_ordinals,
                )
            {
                package_root_found = true;
                let paths = build_editor_source_paths_by_index(
                    doc,
                    &document_service,
                    &storage.source_root,
                );
                let children_by_parent = settings_children_by_parent(doc);
                let mut subtree = Vec::new();
                collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);
                for index in &subtree {
                    if let Some(Some(source_path)) = paths.get(*index) {
                        mirrors.push(json!({
                            "path": source_path.to_string_lossy(),
                            "drift": false,
                            "exists": source_path.exists(),
                        }));
                    }
                }
                match (
                    expected_fingerprint,
                    package_target_fingerprint(
                        doc,
                        &document_service,
                        &document_segments,
                        &document_ordinals,
                    )
                    .ok()
                    .flatten()
                    .as_deref(),
                ) {
                    (Some(expected), Some(actual)) if expected == actual => {}
                    _ => drift = true,
                }
            }
            if !target.broken && !package_root_found {
                missing = true;
                drift = true;
            }
        }
        for pair in &target.files {
            for path in [&pair.mirror, &pair.canonical] {
                file_contents
                    .entry(path.clone())
                    .or_insert_with(|| fs::read_to_string(path).ok());
            }
            let mirror = file_contents.get(&pair.mirror).and_then(Option::as_ref);
            let mut file_drift = false;
            let exists = mirror.is_some();
            if target.resolved && !target.broken {
                if mirror.is_none() {
                    missing = true;
                    drift = true;
                    file_drift = true;
                } else {
                    let canonical = file_contents.get(&pair.canonical).and_then(Option::as_ref);
                    if mirror != canonical {
                        drift = true;
                        file_drift = true;
                    }
                }
            }
            mirrors.push(json!({
                "path": pair.mirror.to_string_lossy(),
                "canonical": pair.canonical.to_string_lossy(),
                "drift": file_drift,
                "exists": exists,
            }));
        }
        if target.broken {
            broken += 1;
        }
        if drift {
            drifted += 1;
        }
        if target.resolved && !target.broken && !missing {
            *active_target_count_by_link
                .entry(target.link_id.clone())
                .or_default() += 1;
        }
        let meta = meta_by_link.get(&target.link_id);
        targets_out.push(json!({
            "linkId": target.link_id,
            "service": target.service,
            "path": target.target_segments,
            "ords": link_target_output_ordinals(target.target_ordinals.clone()),
            "pathKey": link_target_key(
                &target.service,
                &target.target_segments,
                &target.target_ordinals,
            ),
            "readOnly": target.read_only && !target.broken,
            "broken": target.broken,
            "resolved": target.resolved,
            "resolvedRef": target.resolved_ref,
            "drift": drift,
            "missing": missing,
            "files": target.files.len(),
            "mirrors": mirrors,
            "reason": target.unresolved_reason,
            "isPackage": meta.is_some_and(|meta| meta.is_package),
            "rootClass": meta.and_then(|meta| meta.root_class.clone()),
            "rootName": meta.and_then(|meta| meta.root_name.clone()),
            "sourceInstances": if meta.is_some_and(|meta| meta.is_package) {
                meta.map_or(0, |meta| meta.instances)
            } else {
                link_target_source_instance_count(target)
            },
            "updatedUnixMs": meta.and_then(|meta| meta.updated_unix_ms).map(|value| value as u64),
        }));
    }

    let links_out: Vec<Value> = manifest
        .links
        .iter()
        .map(|link| {
            let meta = meta_by_link.get(&link.id);
            json!({
                "id": link.id,
                "readOnly": link.read_only,
                "sourceKind": link.source.kind(),
                "source": link.source.summary(),
                "sourcePath": source_path_by_link.get(&link.id).map(|path| path.to_string_lossy().into_owned()),
                "targetCount": link.targets.len(),
                "activeTargetCount": active_target_count_by_link.get(&link.id).copied().unwrap_or(0),
                "isPackage": meta.is_some_and(|meta| meta.is_package),
                "rootClass": meta.and_then(|meta| meta.root_class.clone()),
                "rootName": meta.and_then(|meta| meta.root_name.clone()),
                "instances": if meta.is_some_and(|meta| meta.is_package) {
                    meta.map_or(0, |meta| meta.instances)
                } else {
                    source_instances_by_link.get(&link.id).copied().unwrap_or(0)
                },
                "updatedUnixMs": meta.and_then(|meta| meta.updated_unix_ms).map(|value| value as u64),
            })
        })
        .collect();

    print_json_output(
        &json!({
            "ok": true,
            "manifest": manifest_path,
            "manifestExists": manifest_path.exists(),
            "lockExists": link_lock_path(&project_root).exists(),
            "linkCount": manifest.links.len(),
            "brokenTargets": broken,
            "driftedTargets": drifted,
            "links": links_out,
            "targets": targets_out,
        }),
        args.pretty,
    )
}

pub(crate) fn link_add(args: LinkAddArgs) -> Result<()> {
    let project_root = resolve_link_project_root(&args.project_root)?;
    let manifest_path = link_manifest_path(&project_root, &args.manifest);
    let mut manifest = read_link_manifest(&manifest_path)?;

    let target = parse_link_target(
        &args.target.service,
        &args.target.path_segments_json,
        &args.target.path_ordinals_json,
    )?;
    let path = &target.path;
    let target_key = link_target_ref_key(&target);
    let target_out = json!({
        "service": &target.service,
        "path": link_target_segments(&target),
        "ords": link_target_output_ordinals(link_target_ordinals(&target)),
    });
    let id = args
        .id
        .unwrap_or_else(|| link_slug(path.last().map_or("link", String::as_str)));
    validate_filesystem_instance_name(&id, "link id")?;
    if let Some(existing_link) = manifest.links.iter().find(|link| {
        link.id != id
            && link
                .targets
                .iter()
                .any(|existing| link_target_ref_key(existing) == target_key)
    }) {
        bail!(
            "{}.{} is already a renium-link target owned by link {}.",
            target.service,
            link_target_segments(&target).join("."),
            existing_link.id
        );
    }

    if let Some(link) = manifest.links.iter_mut().find(|link| link.id == id) {
        if !link
            .targets
            .iter()
            .any(|existing| link_target_ref_key(existing) == target_key)
        {
            link.targets.push(target);
        }
    } else {
        let source_value = args.source.ok_or_else(|| {
            anyhow::anyhow!(
                "--source is required when creating a new link (id {id} does not exist yet)"
            )
        })?;
        let source = match args.source_type.to_ascii_lowercase().as_str() {
            "local" => LinkSource::Local { path: source_value },
            "git" => LinkSource::Git {
                url: source_value,
                git_ref: args.source_ref,
                subpath: args.source_subpath,
            },
            "wally" => LinkSource::Wally {
                package: source_value,
                version: args.source_ref,
            },
            other => bail!("Unknown --source-type {other}. Use local, git, or wally."),
        };
        manifest.links.push(LinkEntry {
            id: id.clone(),
            read_only: !args.target.writable,
            source,
            targets: vec![target],
        });
    }
    manifest
        .broken
        .retain(|broken_target| link_target_ref_key(broken_target) != target_key);
    let target_count = manifest
        .links
        .iter()
        .find(|link| link.id == id)
        .map_or(0, |link| link.targets.len());

    write_link_manifest(&manifest_path, &manifest)?;
    print_json_output(
        &json!({
            "ok": true,
            "id": id,
            "manifest": manifest_path,
            "linkCount": manifest.links.len(),
            "targetCount": target_count,
            "target": target_out,
        }),
        args.pretty,
    )
}

pub(crate) fn link_move_target(args: LinkMoveTargetArgs) -> Result<()> {
    let project_root = resolve_link_project_root(&args.project_root)?;
    let manifest_path = link_manifest_path(&project_root, &args.manifest);
    let mut manifest = read_link_manifest(&manifest_path)?;

    let old_target = parse_link_target(
        &args.old_service,
        &args.old_path_segments_json,
        &args.old_path_ordinals_json,
    )?;
    let new_target = parse_link_target(
        &args.new_service,
        &args.new_path_segments_json,
        &args.new_path_ordinals_json,
    )?;
    let old_key = link_target_ref_key(&old_target);
    let new_key = link_target_ref_key(&new_target);
    if old_key != new_key
        && let Some(existing_link) = manifest.links.iter().find(|link| {
            link.targets
                .iter()
                .any(|target| link_target_ref_key(target) == new_key)
        })
    {
        bail!(
            "{}.{} is already a renium-link target owned by link {}.",
            new_target.service,
            link_target_segments(&new_target).join("."),
            existing_link.id
        );
    }

    let mut moved_link_ids = Vec::new();
    for link in &mut manifest.links {
        for target in &mut link.targets {
            if link_target_ref_key(target) == old_key {
                *target = new_target.clone();
                moved_link_ids.push(link.id.clone());
            }
        }
    }
    if moved_link_ids.is_empty() {
        bail!(
            "{}.{} is not a renium-link target; nothing to move.",
            old_target.service,
            link_target_segments(&old_target).join(".")
        );
    }
    manifest.broken.retain(|target| {
        let key = link_target_ref_key(target);
        key != old_key && key != new_key
    });
    let old_segments = link_target_segments(&old_target);
    let new_segments = link_target_segments(&new_target);
    let mut lock = read_link_lock(&project_root)?;
    let mut lock_changed = false;
    for link_id in &moved_link_ids {
        if let Some(entry) = lock.entries.get_mut(link_id) {
            lock_changed |= migrate_link_target_lock(
                entry,
                &old_target.service,
                &old_segments,
                &link_target_ordinals(&old_target),
                &new_target.service,
                &new_segments,
                &link_target_ordinals(&new_target),
            );
        }
    }
    let mut writes = BTreeMap::from([(
        manifest_path.clone(),
        serialize_link_manifest(&manifest)?.into_bytes(),
    )]);
    if lock_changed {
        writes.insert(link_lock_path(&project_root), serialize_link_lock(&lock)?);
    }
    apply_file_mutations(&writes, &[])?;

    print_json_output(
        &json!({
            "ok": true,
            "manifest": manifest_path,
            "links": moved_link_ids,
            "old": { "service": old_target.service, "path": link_target_segments(&old_target) },
            "new": { "service": new_target.service, "path": link_target_segments(&new_target) },
        }),
        args.pretty,
    )
}

pub(super) fn resolve_editor_instance_by_path_ordinals(
    document: &SettingsBytecode,
    service: &str,
    segments_after_service: &[String],
    ordinals_after_service: &[usize],
) -> Option<usize> {
    let mut current = editor_service_root_index(document, service)?;
    for (index, segment) in segments_after_service.iter().enumerate() {
        let ordinal = ordinals_after_service.get(index).copied().unwrap_or(1);
        if ordinals_after_service.is_empty() {
            current = editor_child_by_stem(document, current, segment)?;
        } else {
            let ordinal_index = ordinal.checked_sub(1)?;
            current = document
                .instances
                .iter()
                .enumerate()
                .filter(|(_, instance)| {
                    instance.parent_index == Some(current) && instance.name == *segment
                })
                .nth(ordinal_index)
                .map(|(index, _)| index)?;
        }
    }
    Some(current)
}

fn remap_package_ref_object(
    object: &mut Map<String, Value>,
    full_refs: &BytecodeCloneRefMap,
    package_refs: &BytecodeCloneRefMap,
    document: &SettingsBytecode,
    location: &str,
) -> Result<()> {
    let old_index = strict_ref_old_index(object, full_refs)
        .with_context(|| format!("Invalid instance reference at {location}"))?;
    let Some(old_index) = old_index else {
        return Ok(());
    };
    let Some(new_index) = package_refs.new_index_by_old.get(&old_index).copied() else {
        let target = &document.instances[old_index];
        bail!(
            "Cannot pack {location}: it references external instance {} ({}, {})",
            target.name,
            target.class_name,
            target.settings_id
        );
    };
    for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
        object.remove(selector);
    }
    object.insert(
        "instanceIndex".to_string(),
        Value::Number(Number::from((new_index + 1) as u64)),
    );
    Ok(())
}

fn remap_package_reference_value(
    value: &mut Value,
    full_refs: &BytecodeCloneRefMap,
    package_refs: &BytecodeCloneRefMap,
    document: &SettingsBytecode,
    location: &str,
) -> Result<()> {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                remap_package_reference_value(
                    item,
                    full_refs,
                    package_refs,
                    document,
                    &format!("{location}[{}]", index + 1),
                )?;
            }
        }
        Value::Object(object) => {
            if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                remap_package_ref_object(object, full_refs, package_refs, document, location)?;
                return Ok(());
            }
            if let Some(reference) = object.get_mut("Ref").and_then(Value::as_object_mut) {
                remap_package_ref_object(
                    reference,
                    full_refs,
                    package_refs,
                    document,
                    &format!("{location}.Ref"),
                )?;
            }
            for (key, nested) in object.iter_mut() {
                if key == "Ref" {
                    continue;
                }
                remap_package_reference_value(
                    nested,
                    full_refs,
                    package_refs,
                    document,
                    &format!("{location}.{key}"),
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn pack_subtree_to_bytecode(
    document: &SettingsBytecode,
    root_index: usize,
    source_paths: &[Option<PathBuf>],
) -> Result<(SettingsBytecode, HashMap<usize, String>)> {
    let children_by_parent = settings_children_by_parent(document);
    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map_or("", |instance| instance.name.as_str());
    let (source_path_segments, source_path_ordinals) =
        build_editor_instance_path_parts(document, service);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);
    let mut package = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let mut source_by_index = HashMap::new();
    for old in &subtree {
        let instance = &document.instances[*old];
        if is_lua_source_class(&instance.class_name) {
            let source = bytecode_export_script_source(
                instance,
                source_paths.get(*old).and_then(Option::as_deref),
            )?;
            source_by_index.insert(*old, source);
        }
    }
    let mut new_by_old: HashMap<usize, usize> = HashMap::new();
    for old in &subtree {
        let instance = &document.instances[*old];
        let parent = if *old == root_index {
            None
        } else {
            instance
                .parent_index
                .and_then(|parent| new_by_old.get(&parent).copied())
        };
        let mut properties = instance.properties.clone();
        if let Some(source) = source_by_index.get(old) {
            properties.insert("Source".to_string(), Value::String(source.clone()));
        }
        let new_index = package.instances.len();
        package.instances.push(SettingsBytecodeInstance {
            settings_id: format!("pkg:{new_index}"),
            name: instance.name.clone(),
            class_name: instance.class_name.clone(),
            parent_index: parent,
            properties,
            attributes: instance.attributes.clone(),
        });
        new_by_old.insert(*old, new_index);
    }
    let package_refs = build_clone_ref_map(
        document,
        CloneRefMapInput {
            source_subtree: &subtree,
            old_to_new_index: &new_by_old,
            path_segments_before: &source_path_segments,
            path_ordinals_before: &source_path_ordinals,
        },
    );
    let all_indexes = (0..document.instances.len()).collect::<Vec<_>>();
    let identity_indexes = all_indexes
        .iter()
        .copied()
        .map(|index| (index, index))
        .collect::<HashMap<_, _>>();
    let full_refs = build_clone_ref_map(
        document,
        CloneRefMapInput {
            source_subtree: &all_indexes,
            old_to_new_index: &identity_indexes,
            path_segments_before: &source_path_segments,
            path_ordinals_before: &source_path_ordinals,
        },
    );
    for (index, instance) in package.instances.iter_mut().enumerate() {
        let source = &document.instances[subtree[index]];
        for (name, value) in &mut instance.properties {
            remap_package_reference_value(
                value,
                &full_refs,
                &package_refs,
                document,
                &format!("{} ({}) property {name}", source.name, source.class_name),
            )?;
        }
        for (name, value) in &mut instance.attributes {
            remap_package_reference_value(
                value,
                &full_refs,
                &package_refs,
                document,
                &format!("{} ({}) attribute {name}", source.name, source.class_name),
            )?;
        }
    }
    Ok((package, source_by_index))
}

fn inline_editor_source_files_for_indexes(
    document: &mut SettingsBytecode,
    service_dir: &Path,
    source_paths_by_index: &[Option<PathBuf>],
    indexes: &[usize],
    source_by_index: &HashMap<usize, String>,
) -> Result<Vec<String>> {
    let mut inlined_paths = Vec::new();
    for index in indexes {
        let Some(instance) = document.instances.get_mut(*index) else {
            continue;
        };
        if !is_lua_source_class(&instance.class_name) {
            continue;
        }
        let Some(Some(source_path)) = source_paths_by_index.get(*index) else {
            continue;
        };
        if fs::symlink_metadata(source_path).is_err() || source_path.is_dir() {
            continue;
        }
        ensure_existing_ancestor_inside(service_dir, source_path, "source file to inline")?;
        let source = source_by_index
            .get(index)
            .with_context(|| format!("Source snapshot is missing for {}", source_path.display()))?;
        instance
            .properties
            .insert("Source".to_string(), Value::String(source.clone()));
        inlined_paths.push(source_path.to_string_lossy().into_owned());
    }
    Ok(inlined_paths)
}

pub(crate) fn link_pack(mut args: LinkPackArgs) -> Result<()> {
    let (project_root, src_root, manifest_path, mut manifest) =
        load_link_project(&mut args.project, &args.manifest)?;

    let target = parse_link_target(
        &args.target.service,
        &args.target.path_segments_json,
        &args.target.path_ordinals_json,
    )?;
    let segments_after_service = link_target_segments(&target);
    let target_ordinals = link_target_ordinals(&target);
    if let Some(id) = &args.id {
        validate_filesystem_instance_name(id, "link id")?;
    }

    for link in &manifest.links {
        for existing in &link.targets {
            if existing.service != args.target.service {
                continue;
            }
            let existing_segments = link_target_segments(existing);
            let existing_ordinals = link_target_ordinals(existing);
            if existing_segments == segments_after_service && existing_ordinals == target_ordinals {
                continue;
            }
            let inside_existing = segments_after_service.len() > existing_segments.len()
                && selector_starts_with(
                    &segments_after_service,
                    &target_ordinals,
                    &existing_segments,
                    &existing_ordinals,
                );
            if inside_existing {
                bail!(
                    "{}.{} is inside the existing link \"{}\" ({}). Break that link first, or pack its root instead.",
                    args.target.service,
                    segments_after_service.join("."),
                    link.id,
                    existing_segments.join(".")
                );
            }
            let contains_existing = existing_segments.len() > segments_after_service.len()
                && selector_starts_with(
                    &existing_segments,
                    &existing_ordinals,
                    &segments_after_service,
                    &target_ordinals,
                );
            if contains_existing {
                bail!(
                    "{}.{} contains the existing link \"{}\" ({}). Break that link first.",
                    args.target.service,
                    segments_after_service.join("."),
                    link.id,
                    existing_segments.join(".")
                );
            }
        }
    }

    let storage = resolve_link_target_storage(&project_root, &src_root, &target, true, false)?;
    let settings_file = storage
        .settings_file
        .as_ref()
        .context("The selected link target has no writable bytecode settings store")?;
    let settings_output_file = storage
        .settings_output_file
        .as_ref()
        .context("The selected link target has no settings output path")?;
    let _settings_guard = acquire_settings_file_lock(settings_file)?;
    let mut document = SettingsBytecode::read_file(settings_file)?;
    let (document_service, document_segments, document_ordinals) =
        link_target_document_selector_parts(
            &target.service,
            &segments_after_service,
            &target_ordinals,
            &storage,
            &document,
        )?;
    let root_index = resolve_editor_instance_by_path_ordinals(
        &document,
        &document_service,
        &document_segments,
        &document_ordinals,
    )
    .ok_or_else(|| anyhow::anyhow!("Instance not found: {}", target.path.join(".")))?;

    let leaf_name = document.instances[root_index].name.clone();
    let service_dir = storage.source_root.clone();
    let source_paths =
        build_editor_source_paths_by_index(&document, &document_service, &service_dir);
    let children_by_parent = settings_children_by_parent(&document);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);
    let (package, source_by_index) =
        pack_subtree_to_bytecode(&document, root_index, &source_paths)?;

    let target_key = link_target_ref_key(&target);
    let existing_target_link_id = manifest
        .links
        .iter()
        .find(|link| {
            link.targets
                .iter()
                .any(|existing| link_target_ref_key(existing) == target_key)
        })
        .map(|link| link.id.clone());

    let explicit_id = args.id.is_some();
    let id = if let Some(explicit) = args.id {
        explicit
    } else {
        let base = link_slug(&leaf_name);
        let mut candidate = base.clone();
        let mut suffix = 2;
        loop {
            match manifest.links.iter().find(|link| link.id == candidate) {
                None => break,
                Some(link)
                    if link
                        .targets
                        .iter()
                        .any(|existing| link_target_ref_key(existing) == target_key) =>
                {
                    break;
                }
                Some(_) => {
                    candidate = format!("{base}-{suffix}");
                    suffix += 1;
                }
            }
        }
        candidate
    };
    validate_filesystem_instance_name(&id, "link id")?;
    if let Some(owner_id) = existing_target_link_id.as_deref()
        && owner_id != id
    {
        bail!(
            "{}.{} is already a renium-link target owned by link {}.",
            target.service,
            link_target_segments(&target).join("."),
            owner_id
        );
    }
    let existing_link = manifest.links.iter().find(|link| link.id == id);
    if explicit_id
        && let Some(link) = existing_link
        && !link
            .targets
            .iter()
            .any(|existing| link_target_ref_key(existing) == target_key)
    {
        bail!(
            "Link {id} does not target {}.{}; refusing to overwrite its package from an unrelated instance.",
            target.service,
            link_target_segments(&target).join(".")
        );
    }
    let (package_file, source_rel) = if let Some(link) = existing_link {
        match &link.source {
            LinkSource::Local { path } if is_package_path(Path::new(path)) => {
                let package_file = resolve_local_link_path(&project_root, path);
                (package_file, path.replace('\\', "/"))
            }
            _ => bail!("Link {id} is not a local bytecode package and cannot be resaved."),
        }
    } else if let Some(link_folder) = &args.link_folder {
        let link_folder_abs = absolutize_under(&project_root, link_folder);
        let package_file = link_folder_abs.join(format!("{id}.{RENIUM_STORE_EXTENSION}"));
        let source_rel = link_folder
            .join(format!("{id}.{RENIUM_STORE_EXTENSION}"))
            .to_string_lossy()
            .replace('\\', "/");
        (package_file, source_rel)
    } else {
        let global_dir = renium_global_packages_dir();
        (
            global_dir.join(format!("{id}.{RENIUM_STORE_EXTENSION}")),
            format!("{GLOBAL_LINK_PREFIX}{id}.{RENIUM_STORE_EXTENSION}"),
        )
    };
    let package_bytes = encode_settings_bytecode(&package)?;
    let mut lock = read_link_lock(&project_root)?;
    let default_ordinals = vec![1; segments_after_service.len()];
    let default_target_exists = manifest
        .links
        .iter()
        .find(|link| link.id == id)
        .is_some_and(|link| {
            link.targets.iter().any(|existing| {
                existing.service == args.target.service
                    && link_target_segments(existing) == segments_after_service
                    && link_target_ordinals(existing) == default_ordinals
            })
        });
    let lock_entry = lock.entries.entry(id.clone()).or_default();
    lock_entry
        .targets
        .entry(link_target_key(
            &args.target.service,
            &segments_after_service,
            &target_ordinals,
        ))
        .or_default()
        .settings_id = Some(document.instances[root_index].settings_id.clone());
    let files = &mut lock_entry.files;
    if target_ordinals != default_ordinals && !default_target_exists {
        files.remove(&package_lock_key(
            "package",
            &args.target.service,
            &segments_after_service,
            &default_ordinals,
        ));
        files.remove(&package_lock_key(
            "package-target",
            &args.target.service,
            &segments_after_service,
            &default_ordinals,
        ));
    }
    files.insert(
        package_lock_key(
            "package",
            &args.target.service,
            &segments_after_service,
            &target_ordinals,
        ),
        fnv1a_hex(&package_bytes),
    );
    files.insert(
        package_lock_key(
            "package-target",
            &args.target.service,
            &segments_after_service,
            &target_ordinals,
        ),
        package_document_fingerprint(&package)?,
    );

    if let Some(link) = manifest.links.iter_mut().find(|link| link.id == id) {
        if !link
            .targets
            .iter()
            .any(|existing| link_target_ref_key(existing) == target_key)
        {
            link.targets.push(target);
        }
    } else {
        manifest.links.push(LinkEntry {
            id: id.clone(),
            read_only: !args.target.writable,
            source: LinkSource::Local {
                path: source_rel.clone(),
            },
            targets: vec![target],
        });
    }
    manifest
        .broken
        .retain(|broken_target| link_target_ref_key(broken_target) != target_key);
    let inlined_source_paths = inline_editor_source_files_for_indexes(
        &mut document,
        &service_dir,
        &source_paths,
        &subtree,
        &source_by_index,
    )?;
    let mut source_removals =
        plan_editor_source_file_removals(&service_dir, &source_paths, &subtree)?;
    let removed_source_paths = source_removals
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut transaction_writes = BTreeMap::new();
    transaction_writes.insert(package_file.clone(), package_bytes);
    transaction_writes.insert(link_lock_path(&project_root), serialize_link_lock(&lock)?);
    transaction_writes.insert(
        manifest_path.clone(),
        serialize_link_manifest(&manifest)?.into_bytes(),
    );
    if !inlined_source_paths.is_empty() {
        transaction_writes.insert(
            settings_output_file.clone(),
            encode_settings_bytecode(&document)?,
        );
        if exact_path_key(settings_output_file) != exact_path_key(settings_file) {
            source_removals.push(settings_file.clone());
        }
    }
    let gitignore = project_root.join(".renium").join(".gitignore");
    if !gitignore.exists() {
        transaction_writes.insert(gitignore, RENIUM_DIR_GITIGNORE.as_bytes().to_vec());
    }
    apply_file_mutations(&transaction_writes, &source_removals)?;
    prune_removed_source_dirs(&service_dir, &source_removals);

    print_json_output(
        &json!({
            "ok": true,
            "id": id,
            "manifest": manifest_path,
            "package": package_file,
            "source": source_rel,
            "instances": package.instances.len(),
            "service": args.target.service,
            "path": segments_after_service,
            "embeddedSourceFiles": inlined_source_paths,
            "deletedSourceFiles": removed_source_paths,
        }),
        args.pretty,
    )
}

enum LinkDeletePackageAction {
    DeleteUnused,
    DeleteUses,
    UnlinkUses,
}

fn parse_link_delete_package_action(raw: &str) -> Result<LinkDeletePackageAction> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "delete-unused" | "unused" | "refuse" => Ok(LinkDeletePackageAction::DeleteUnused),
        "delete-uses" | "delete-usage" | "delete-targets" | "delete-all" => {
            Ok(LinkDeletePackageAction::DeleteUses)
        }
        "unlink-uses" | "unlink" | "desync" | "keep-uses" => {
            Ok(LinkDeletePackageAction::UnlinkUses)
        }
        other => {
            bail!("Invalid --action {other:?}. Use delete-unused, delete-uses, or unlink-uses.")
        }
    }
}

fn active_link_targets(manifest: &LinkManifest, link: &LinkEntry) -> Vec<LinkTargetRef> {
    let broken = manifest
        .broken
        .iter()
        .map(link_target_ref_key)
        .collect::<HashSet<_>>();
    link.targets
        .iter()
        .filter(|target| !broken.contains(&link_target_ref_key(target)))
        .cloned()
        .collect()
}

fn plan_externalize_editor_source_files_for_indexes(
    document: &mut SettingsBytecode,
    service_dir: &Path,
    source_paths_by_index: &[Option<PathBuf>],
    indexes: &[usize],
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Vec<String>> {
    let mut written_paths = Vec::new();
    for index in indexes {
        let Some(instance) = document.instances.get_mut(*index) else {
            continue;
        };
        if !is_lua_source_class(&instance.class_name) {
            continue;
        }
        let Some(Some(source_path)) = source_paths_by_index.get(*index) else {
            continue;
        };
        ensure_existing_ancestor_inside(service_dir, source_path, "unlinked source file")?;
        let source = bytecode_export_script_source(instance, Some(source_path))?;
        writes.insert(source_path.clone(), source.into_bytes());
        instance.properties.insert(
            "Source".to_string(),
            Value::String("__SOURCE_EXTERNAL__".to_string()),
        );
        written_paths.push(source_path.to_string_lossy().into_owned());
    }
    Ok(written_paths)
}

struct PreparedLinkTarget {
    storage: LinkTargetStorage,
    settings_file: PathBuf,
    document_service: String,
    index: usize,
}

fn prepare_link_target_document(
    project_root: &Path,
    src_root: &Path,
    target: &LinkTargetRef,
    documents: &mut HashMap<PathBuf, SettingsBytecode>,
    settings_outputs: &mut HashMap<PathBuf, PathBuf>,
) -> Result<Option<PreparedLinkTarget>> {
    validate_link_target_ref(target)?;
    let segments = link_target_segments(target);
    let storage = resolve_link_target_storage(project_root, src_root, target, true, false)?;
    let Some(settings_file) = storage.settings_file.clone() else {
        return Ok(None);
    };
    if !documents.contains_key(&settings_file) {
        documents.insert(
            settings_file.clone(),
            SettingsBytecode::read_file(&settings_file)?,
        );
    }
    settings_outputs
        .entry(settings_file.clone())
        .or_insert_with(|| {
            storage
                .settings_output_file
                .clone()
                .unwrap_or_else(|| settings_file.clone())
        });
    let document = documents
        .get(&settings_file)
        .context("Link target settings were not loaded")?;
    let (document_service, document_segments, document_ordinals) =
        link_target_document_selector_parts(
            &target.service,
            &segments,
            &link_target_ordinals(target),
            &storage,
            document,
        )?;
    let Some(index) = resolve_editor_instance_by_path_ordinals(
        document,
        &document_service,
        &document_segments,
        &document_ordinals,
    ) else {
        return Ok(None);
    };
    Ok(Some(PreparedLinkTarget {
        storage,
        settings_file,
        document_service,
        index,
    }))
}

fn plan_delete_link_target_instance(
    project_root: &Path,
    src_root: &Path,
    target: &LinkTargetRef,
    documents: &mut HashMap<PathBuf, SettingsBytecode>,
    settings_outputs: &mut HashMap<PathBuf, PathBuf>,
    removals: &mut Vec<PathBuf>,
) -> Result<(bool, Vec<String>)> {
    let Some(prepared) =
        prepare_link_target_document(project_root, src_root, target, documents, settings_outputs)?
    else {
        return Ok((false, Vec::new()));
    };
    let document = documents
        .get_mut(&prepared.settings_file)
        .context("Link target settings were not loaded")?;
    let source_paths_before = build_editor_source_paths_by_index(
        document,
        &prepared.document_service,
        &prepared.storage.source_root,
    );
    let removed =
        instance_api::remove_instance(document, InstanceSelector::Index(prepared.index), true)?;
    let source_removals = plan_editor_source_file_removals(
        &prepared.storage.source_root,
        &source_paths_before,
        &removed,
    )?;
    let removed_source_paths = source_removals
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    removals.extend(source_removals);
    Ok((true, removed_source_paths))
}

fn plan_unlink_link_target_instance(
    project_root: &Path,
    src_root: &Path,
    target: &LinkTargetRef,
    documents: &mut HashMap<PathBuf, SettingsBytecode>,
    settings_outputs: &mut HashMap<PathBuf, PathBuf>,
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> Result<(bool, Vec<String>)> {
    let Some(prepared) =
        prepare_link_target_document(project_root, src_root, target, documents, settings_outputs)?
    else {
        return Ok((false, Vec::new()));
    };
    let document = documents
        .get_mut(&prepared.settings_file)
        .context("Link target settings were not loaded")?;
    let children_by_parent = settings_children_by_parent(document);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children_by_parent, prepared.index, &mut subtree);
    let source_paths = build_editor_source_paths_by_index(
        document,
        &prepared.document_service,
        &prepared.storage.source_root,
    );
    let written_source_paths = plan_externalize_editor_source_files_for_indexes(
        document,
        &prepared.storage.source_root,
        &source_paths,
        &subtree,
        writes,
    )?;
    Ok((true, written_source_paths))
}

fn local_package_source_path(project_root: &Path, link: &LinkEntry) -> Result<PathBuf> {
    let LinkSource::Local { path } = &link.source else {
        bail!("Link {} is not a local package source.", link.id);
    };
    let package_path = resolve_local_link_path(project_root, path);
    if !is_package_path(&package_path) {
        bail!(
            "Link {} source is not a renium package: {}",
            link.id,
            package_path.display()
        );
    }
    if is_global_link_path(path) {
        ensure_existing_ancestor_inside(
            &renium_global_packages_dir(),
            &package_path,
            "link package source",
        )?;
    } else {
        ensure_existing_ancestor_inside(project_root, &package_path, "link package source")?;
    }
    Ok(package_path)
}

pub(crate) fn link_delete_package(mut args: LinkDeletePackageArgs) -> Result<()> {
    let (project_root, src_root, manifest_path, mut manifest) =
        load_link_project(&mut args.project, &args.manifest)?;
    let action = parse_link_delete_package_action(&args.action)?;
    let link_index = manifest
        .links
        .iter()
        .position(|link| link.id == args.id)
        .ok_or_else(|| anyhow::anyhow!("No link package with id {}.", args.id))?;
    let (package_path, active_targets) = {
        let link = &manifest.links[link_index];
        (
            local_package_source_path(&project_root, link)?,
            active_link_targets(&manifest, link),
        )
    };
    if !active_targets.is_empty() && matches!(&action, LinkDeletePackageAction::DeleteUnused) {
        bail!(
            "Package {} has {} active use(s). Choose delete-uses or unlink-uses.",
            args.id,
            active_targets.len()
        );
    }
    let mut target_settings_files = Vec::new();
    for target in &active_targets {
        if let Some(settings_file) =
            resolve_link_target_storage(&project_root, &src_root, target, true, false)?
                .settings_file
        {
            target_settings_files.push(settings_file);
        }
    }
    let settings_files = collect_project_settings_files(&src_root, target_settings_files)?;
    let mut _settings_guards = Vec::with_capacity(settings_files.len());
    for settings_file in &settings_files {
        _settings_guards.push(acquire_settings_file_lock(settings_file)?);
    }
    let link = manifest.links.remove(link_index);

    let mut touched_services = HashSet::new();
    let mut documents = HashMap::<PathBuf, SettingsBytecode>::new();
    let mut settings_outputs = HashMap::<PathBuf, PathBuf>::new();
    let mut transaction_writes = BTreeMap::<PathBuf, Vec<u8>>::new();
    let mut transaction_removals = Vec::<PathBuf>::new();
    let mut removed_source_paths = Vec::new();
    let mut externalized_source_paths = Vec::new();
    let mut deleted_targets = Vec::new();
    let mut unlinked_targets = Vec::new();
    let mut missing_targets = Vec::new();

    match action {
        LinkDeletePackageAction::DeleteUnused => {}
        LinkDeletePackageAction::DeleteUses => {
            for target in &active_targets {
                let (deleted, paths) = plan_delete_link_target_instance(
                    &project_root,
                    &src_root,
                    target,
                    &mut documents,
                    &mut settings_outputs,
                    &mut transaction_removals,
                )?;
                touched_services.insert(target.service.clone());
                removed_source_paths.extend(paths);
                let out =
                    json!({ "service": target.service, "path": link_target_segments(target) });
                if deleted {
                    deleted_targets.push(out);
                } else {
                    missing_targets.push(out);
                }
            }
        }
        LinkDeletePackageAction::UnlinkUses => {
            for target in &active_targets {
                let (unlinked, paths) = plan_unlink_link_target_instance(
                    &project_root,
                    &src_root,
                    target,
                    &mut documents,
                    &mut settings_outputs,
                    &mut transaction_writes,
                )?;
                touched_services.insert(target.service.clone());
                externalized_source_paths.extend(paths);
                let out =
                    json!({ "service": target.service, "path": link_target_segments(target) });
                if unlinked {
                    unlinked_targets.push(out);
                } else {
                    missing_targets.push(out);
                }
            }
        }
    }

    let target_keys = link
        .targets
        .iter()
        .map(link_target_ref_key)
        .collect::<HashSet<_>>();
    manifest
        .broken
        .retain(|target| !target_keys.contains(&link_target_ref_key(target)));
    let mut lock = read_link_lock(&project_root)?;
    lock.entries.remove(&link.id);
    stage_settings_document_writes(
        &documents,
        &settings_outputs,
        &mut transaction_writes,
        &mut transaction_removals,
    )?;
    let manifest_removed = manifest.links.is_empty() && manifest.broken.is_empty();
    if manifest_removed {
        transaction_removals.push(manifest_path.clone());
    } else {
        transaction_writes.insert(
            manifest_path.clone(),
            serialize_link_manifest(&manifest)?.into_bytes(),
        );
    }
    let lock_path = link_lock_path(&project_root);
    let lock_removed = lock.entries.is_empty();
    if lock_removed {
        transaction_removals.push(lock_path);
    } else {
        transaction_writes.insert(lock_path, serialize_link_lock(&lock)?);
    }
    let deleted_package_path = package_path.is_file().then(|| package_path.clone());
    let deleted_package_output = deleted_package_path.as_ref().map(|path| {
        path.to_string_lossy()
            .replace('/', std::path::MAIN_SEPARATOR_STR)
    });
    if deleted_package_path.is_some() {
        transaction_removals.push(package_path);
    }
    transaction_removals.retain(|path| {
        !transaction_writes
            .keys()
            .any(|write| exact_path_key(write) == exact_path_key(path))
    });
    transaction_removals.sort_by_key(|path| exact_path_key(path));
    transaction_removals.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    apply_file_mutations(&transaction_writes, &transaction_removals)?;
    prune_removed_source_dirs(&src_root, &transaction_removals);
    let mut changed_paths = documents
        .keys()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    changed_paths.extend(externalized_source_paths.iter().cloned());
    changed_paths.extend(removed_source_paths.iter().cloned());

    print_json_output(
        &json!({
            "ok": true,
            "id": link.id,
            "manifest": manifest_path,
            "manifestRemoved": manifest_removed,
            "lockRemoved": lock_removed,
            "deletedPackage": deleted_package_output,
            "activeUses": active_targets.len(),
            "deletedTargets": deleted_targets,
            "unlinkedTargets": unlinked_targets,
            "missingTargets": missing_targets,
            "removedSourcePaths": removed_source_paths,
            "externalizedSourcePaths": externalized_source_paths,
            "changedPaths": changed_paths,
            "services": touched_services.into_iter().collect::<Vec<_>>(),
        }),
        args.pretty,
    )
}
