use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::app::output::print_json_output;
use crate::bytecode::edit::{
    collect_settings_subtree_preorder, instance_path_parts_key, prune_removed_source_dirs,
};
use crate::bytecode::{acquire_settings_file_lock, apply_file_mutations};
use crate::cli::SyncWallyPackagesArgs;
use crate::editor::document::ensure_editor_source_target_in_bytecode;
use crate::editor::paths::{build_editor_instance_paths, infer_editor_source_path_spec_in_service};
use crate::project::layout::apply_configured_project_layout;
use crate::settings::bytecode::{
    SettingsBytecode, decode_settings_bytecode, encode_settings_bytecode,
};
use crate::settings::instance::{self as instance_api, InstanceSelector};
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::system::files::{
    absolutize_under, ensure_existing_ancestor_inside, fnv1a_hex, path_key, read_file_if_present,
    resolve_existing_project_root, service_settings_path, validate_filesystem_instance_name,
};
use crate::system::tools::run_checked_external_tool;

use super::{
    LinkLock, LinkTargetRef, LinkTargetStorage, RENIUM_DIR_GITIGNORE,
    apply_preserved_subtree_identity, collect_project_settings_files, ensure_editor_container_path,
    ensure_settings_document, link_lock_path, link_manifest_path,
    link_target_document_selector_parts, link_target_file_pairs_at, link_target_ordinals,
    link_target_ref_key, link_target_segments, load_settings_documents, package_target_fingerprint,
    prepare_package_replacement, read_link_lock, read_link_manifest,
    referenced_settings_ids_outside, resolve_editor_instance_by_path_ordinals,
    resolve_link_target_storage, selector_starts_with, serialize_link_lock,
    stage_settings_document_writes, subtree_relative_indices,
};

struct WallyRealm {
    realm: &'static str,
    packages_dir: PathBuf,
    service: String,
    target_name: String,
    required: bool,
}

fn wally_realm_target(realm: &WallyRealm) -> LinkTargetRef {
    LinkTargetRef {
        service: realm.service.clone(),
        path: vec![realm.service.clone(), realm.target_name.clone()],
        ords: Vec::new(),
    }
}

fn wally_realm_lock_key(realm: &WallyRealm, storage: &LinkTargetStorage) -> String {
    format!(
        "wally:{}:{}/{}@{}:{}",
        realm.realm,
        realm.service,
        realm.target_name,
        storage.owner,
        path_key(&storage.source_root)
    )
}

fn lock_has_wally_realm(lock: &LinkLock, realm: &WallyRealm) -> bool {
    let prefix = format!(
        "wally:{}:{}/{}@",
        realm.realm, realm.service, realm.target_name
    );
    lock.entries.keys().any(|key| key.starts_with(&prefix))
}

fn build_wally_realms(
    project_root: &Path,
    args: &SyncWallyPackagesArgs,
) -> Result<Vec<WallyRealm>> {
    let wanted: HashSet<String> = args
        .realms
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();
    if let Some(realm) = wanted
        .iter()
        .find(|realm| !matches!(realm.as_str(), "shared" | "server" | "dev"))
    {
        bail!("Unknown Wally realm '{realm}'; expected shared, server, or dev");
    }
    let mut realms = Vec::new();
    if wanted.contains("shared") {
        realms.push(WallyRealm {
            realm: "shared",
            packages_dir: absolutize_under(project_root, &args.packages_dir),
            service: validate_wally_target_name(&args.target_service, "target service")?,
            target_name: validate_wally_target_name(&args.target_name, "target name")?,
            required: true,
        });
    }
    if wanted.contains("server") {
        realms.push(WallyRealm {
            realm: "server",
            packages_dir: absolutize_under(project_root, &args.server_packages_dir),
            service: validate_wally_target_name(
                &args.server_target_service,
                "server target service",
            )?,
            target_name: validate_wally_target_name(
                &args.server_target_name,
                "server target name",
            )?,
            required: false,
        });
    }
    if wanted.contains("dev") {
        realms.push(WallyRealm {
            realm: "dev",
            packages_dir: absolutize_under(project_root, &args.dev_packages_dir),
            service: validate_wally_target_name(&args.dev_target_service, "dev target service")?,
            target_name: validate_wally_target_name(&args.dev_target_name, "dev target name")?,
            required: false,
        });
    }
    Ok(realms)
}

fn validate_wally_target_overlaps(project_root: &Path, realms: &[WallyRealm]) -> Result<()> {
    let mut targets = Vec::<(String, LinkTargetRef)>::new();
    for realm in realms {
        targets.push((
            format!("Wally {} realm", realm.realm),
            wally_realm_target(realm),
        ));
    }
    let manifest = read_link_manifest(&link_manifest_path(
        project_root,
        Path::new("renium-link.json"),
    ))?;
    let broken = manifest
        .broken
        .iter()
        .map(link_target_ref_key)
        .collect::<HashSet<_>>();
    for link in manifest.links {
        for target in link.targets {
            if !broken.contains(&link_target_ref_key(&target)) {
                targets.push((format!("link {}", link.id), target));
            }
        }
    }
    for index in 0..targets.len() {
        let (left_label, left) = &targets[index];
        let left_segments = link_target_segments(left);
        let left_ordinals = link_target_ordinals(left);
        for (right_label, right) in &targets[index + 1..] {
            if left.service != right.service {
                continue;
            }
            let right_segments = link_target_segments(right);
            let right_ordinals = link_target_ordinals(right);
            if selector_starts_with(
                &left_segments,
                &left_ordinals,
                &right_segments,
                &right_ordinals,
            ) || selector_starts_with(
                &right_segments,
                &right_ordinals,
                &left_segments,
                &left_ordinals,
            ) {
                bail!(
                    "{left_label} target {}.{} overlaps {right_label} target {}.{}",
                    left.service,
                    left_segments.join("."),
                    right.service,
                    right_segments.join(".")
                );
            }
        }
    }
    Ok(())
}

fn wally_inputs_hash(project_root: &Path, manifest: &Path) -> Result<String> {
    let manifest = fs::read(manifest)
        .with_context(|| format!("Failed to read Wally manifest {}", manifest.display()))?;
    let lock_path = project_root.join("wally.lock");
    let lock = read_file_if_present(&lock_path)
        .with_context(|| format!("Failed to read Wally lockfile {}", lock_path.display()))?
        .unwrap_or_default();
    let mut content = Vec::with_capacity(manifest.len() + lock.len() + 16);
    content.extend_from_slice(&(manifest.len() as u64).to_le_bytes());
    content.extend_from_slice(&manifest);
    content.extend_from_slice(&(lock.len() as u64).to_le_bytes());
    content.extend_from_slice(&lock);
    Ok(fnv1a_hex(&content))
}

struct WallyRealmOutcome {
    service: String,
    removed_target: Value,
    removed_settings_ids: Vec<String>,
    settings_ids: Vec<String>,
    source_writes: Vec<Value>,
    changed_paths: Vec<String>,
    writes: BTreeMap<PathBuf, Vec<u8>>,
    removals: Vec<PathBuf>,
}

fn import_wally_realm(
    document: &mut SettingsBytecode,
    realm: &WallyRealm,
    storage: &LinkTargetStorage,
    remove_only: bool,
    external_references: &HashSet<String>,
) -> Result<WallyRealmOutcome> {
    let target = wally_realm_target(realm);
    let segments = link_target_segments(&target);
    let (service, target_segments, target_ordinals) = link_target_document_selector_parts(
        &target.service,
        &segments,
        &link_target_ordinals(&target),
        storage,
        document,
    )?;
    let service_dir = storage.source_root.clone();
    let target_name = realm.target_name.clone();
    let pairs = if realm.packages_dir.is_dir() {
        link_target_file_pairs_at(
            &target,
            &realm.packages_dir,
            true,
            &storage.naming,
            &storage.target_path,
            &service_dir,
            storage.source_is_file,
        )?
    } else {
        Vec::new()
    };
    let target_prefix = std::iter::once(service.clone())
        .chain(target_segments.iter().cloned())
        .collect::<Vec<_>>();
    let root_key = instance_path_parts_key(&[], &[]);
    let mut expected_classes = HashMap::from([(root_key, "Folder".to_string())]);
    let mut explicit_sources = HashMap::<String, PathBuf>::new();
    for pair in &pairs {
        let Some(spec) =
            infer_editor_source_path_spec_in_service(&service_dir, &service, &pair.mirror)
        else {
            continue;
        };
        if !spec.path_segments.starts_with(&target_prefix) {
            bail!(
                "Wally source {} maps outside {}.{}",
                pair.mirror.display(),
                service,
                target_segments.join(".")
            );
        }
        let relative = &spec.path_segments[target_prefix.len()..];
        for end in 1..=relative.len() {
            let segments = relative[..end].to_vec();
            let key = instance_path_parts_key(&segments, &vec![1; segments.len()]);
            if end < relative.len() {
                expected_classes
                    .entry(key)
                    .or_insert_with(|| "Folder".to_string());
                continue;
            }
            if let Some(existing_source) = explicit_sources.get(&key)
                && expected_classes.get(&key) != Some(&spec.class_name)
            {
                let path = target_prefix
                    .iter()
                    .chain(relative.iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(".");
                bail!(
                    "Wally sources {} and {} map {path} to incompatible classes {} and {}",
                    existing_source.display(),
                    pair.canonical.display(),
                    expected_classes[&key],
                    spec.class_name,
                );
            }
            expected_classes.insert(key.clone(), spec.class_name.clone());
            explicit_sources.insert(key, pair.canonical.clone());
        }
    }

    let mut removed_target = Value::Null;
    let mut removed_settings_ids = Vec::new();
    let mut preserved = HashMap::new();
    if let Some(existing_index) = resolve_editor_instance_by_path_ordinals(
        document,
        &service,
        &target_segments,
        &target_ordinals,
    ) {
        let existing = document.instances[existing_index].clone();
        if existing.class_name != "Folder" {
            bail!(
                "Refusing to replace {service}.{target_name} because the existing instance is a {}, not a Folder.",
                existing.class_name
            );
        }
        let children_by_parent = settings_children_by_parent(document);
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children_by_parent, existing_index, &mut subtree);
        let existing_by_key = subtree_relative_indices(document, existing_index);
        let mut preserved_by_index = HashMap::new();
        for (key, class_name) in &expected_classes {
            let Some(index) = existing_by_key.get(key).copied() else {
                continue;
            };
            if document.instances[index].class_name != *class_name {
                continue;
            }
            let settings_id = document.instances[index].settings_id.clone();
            preserved.insert(key.clone(), settings_id.clone());
            preserved_by_index.insert(index, settings_id);
        }
        prepare_package_replacement(
            document,
            &subtree,
            &preserved_by_index,
            external_references,
            "Wally",
        )?;
        let removed_indexes = if target_segments.is_empty() {
            subtree.iter().copied().skip(1).collect::<Vec<_>>()
        } else {
            let paths_by_index = build_editor_instance_paths(document, &service);
            if let Some(path_info) = paths_by_index
                .get(existing_index)
                .and_then(std::clone::Clone::clone)
            {
                removed_target = json!({
                    "settingsId": existing.settings_id,
                    "className": existing.class_name,
                    "pathSegments": path_info.path_segments,
                    "pathOrdinals": path_info.path_ordinals,
                });
            }
            subtree.clone()
        };
        removed_settings_ids = removed_indexes
            .iter()
            .filter_map(|index| document.instances.get(*index))
            .map(|instance| instance.settings_id.clone())
            .collect();
        if target_segments.is_empty() {
            let direct_children = children_by_parent[existing_index].clone();
            for child in direct_children.into_iter().rev() {
                instance_api::remove_instance(document, InstanceSelector::Index(child), true)?;
            }
        } else {
            instance_api::remove_instance(document, InstanceSelector::Index(existing_index), true)?;
        }
    }

    let package_source_dir = storage.target_path.clone();
    let mut removals = Vec::new();
    if package_source_dir.is_dir() {
        ensure_existing_ancestor_inside(
            &service_dir,
            &package_source_dir,
            "Wally package source directory",
        )?;
        for entry in WalkDir::new(&package_source_dir).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                removals.push(entry.path().to_path_buf());
            }
        }
    } else if fs::symlink_metadata(&package_source_dir).is_ok() {
        bail!(
            "Refusing to replace Wally package source path because it is not a directory: {}",
            package_source_dir.display()
        );
    }

    if remove_only {
        return Ok(WallyRealmOutcome {
            service: realm.service.clone(),
            removed_target,
            removed_settings_ids,
            settings_ids: Vec::new(),
            source_writes: Vec::new(),
            changed_paths: removals
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            writes: BTreeMap::new(),
            removals,
        });
    }

    let mut settings_ids = Vec::new();
    let mut ensured_targets = Vec::new();
    let mut source_writes = Vec::new();
    let mut changed_paths = Vec::new();
    let mut writes = BTreeMap::new();
    let mut seen = HashSet::new();
    if pairs.is_empty() && !target_segments.is_empty() {
        ensure_editor_container_path(document, &service, &target_segments)?;
    }
    for pair in &pairs {
        let content = fs::read(&pair.canonical)
            .with_context(|| format!("Failed to read {}", pair.canonical.display()))?;
        writes.insert(pair.mirror.clone(), content);
        let Some(spec) =
            infer_editor_source_path_spec_in_service(&service_dir, &service, &pair.mirror)
        else {
            continue;
        };
        let ensured = ensure_editor_source_target_in_bytecode(document, &spec)?;
        ensured_targets.push((
            ensured.target.path_segments.clone(),
            ensured.target.path_ordinals.clone(),
        ));
        let mirror_str = pair.mirror.to_string_lossy().into_owned();
        if seen.insert(path_key(&pair.mirror)) {
            changed_paths.push(mirror_str.clone());
        }
        source_writes.push(json!({ "path": mirror_str }));
    }
    let rebuilt_root = if target_segments.is_empty() {
        editor_service_root_index(document, &service)
    } else {
        resolve_editor_instance_by_path_ordinals(
            document,
            &service,
            &target_segments,
            &target_ordinals,
        )
    }
    .with_context(|| format!("Wally target {service}.{target_name} was not rebuilt"))?;
    apply_preserved_subtree_identity(document, rebuilt_root, &preserved)?;
    for (segments, ordinals) in ensured_targets {
        let offset = usize::from(segments.first().is_some_and(|value| value == &service));
        if let Some(index) = resolve_editor_instance_by_path_ordinals(
            document,
            &service,
            &segments[offset..],
            &ordinals[offset.min(ordinals.len())..],
        ) {
            settings_ids.push(document.instances[index].settings_id.clone());
        }
    }

    Ok(WallyRealmOutcome {
        service: realm.service.clone(),
        removed_target,
        removed_settings_ids,
        settings_ids,
        source_writes,
        changed_paths,
        writes,
        removals,
    })
}

fn wally_realm_is_fresh(
    document: &SettingsBytecode,
    realm: &WallyRealm,
    storage: &LinkTargetStorage,
) -> Result<bool> {
    if !realm.packages_dir.is_dir() {
        return Ok(false);
    }
    let target = wally_realm_target(realm);
    let segments = link_target_segments(&target);
    let (service, target_segments, target_ordinals) = link_target_document_selector_parts(
        &target.service,
        &segments,
        &link_target_ordinals(&target),
        storage,
        document,
    )?;
    let pairs = link_target_file_pairs_at(
        &target,
        &realm.packages_dir,
        true,
        &storage.naming,
        &storage.target_path,
        &storage.source_root,
        storage.source_is_file,
    )?;
    let target_dir = storage.target_path.clone();
    if !target_dir.is_dir() {
        return Ok(false);
    }

    let mut expected_files = BTreeMap::new();
    for pair in pairs {
        expected_files.insert(
            path_key(&pair.mirror),
            fs::read(&pair.canonical)
                .with_context(|| format!("Failed to read {}", pair.canonical.display()))?,
        );
    }
    let mut actual_files = BTreeMap::new();
    for entry in WalkDir::new(&target_dir).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            return Ok(false);
        }
        if entry.file_type().is_file() {
            if storage
                .settings_file
                .as_ref()
                .is_some_and(|settings| path_key(settings) == path_key(entry.path()))
            {
                continue;
            }
            actual_files.insert(
                path_key(entry.path()),
                fs::read(entry.path())
                    .with_context(|| format!("Failed to read {}", entry.path().display()))?,
            );
        }
    }
    if actual_files != expected_files {
        return Ok(false);
    }

    let current_fingerprint =
        package_target_fingerprint(document, &service, &target_segments, &target_ordinals)?;
    let mut expected_document = document.clone();
    import_wally_realm(
        &mut expected_document,
        realm,
        storage,
        false,
        &HashSet::new(),
    )?;
    let expected_document =
        decode_settings_bytecode(&encode_settings_bytecode(&expected_document)?)?;
    let expected_fingerprint = package_target_fingerprint(
        &expected_document,
        &service,
        &target_segments,
        &target_ordinals,
    )?;
    Ok(current_fingerprint.is_some() && current_fingerprint == expected_fingerprint)
}

fn resolve_wally_realm_storages(
    project_root: &Path,
    src_root: &Path,
    realms: &[WallyRealm],
    lock: &LinkLock,
) -> Result<HashMap<&'static str, LinkTargetStorage>> {
    let mut storages = HashMap::new();
    for realm in realms {
        match resolve_link_target_storage(
            project_root,
            src_root,
            &wally_realm_target(realm),
            true,
            true,
        ) {
            Ok(storage) => {
                storages.insert(realm.realm, storage);
            }
            Err(_)
                if !realm.required
                    && !realm.packages_dir.is_dir()
                    && !lock_has_wally_realm(lock, realm) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(storages)
}

fn initial_wally_realm_freshness(
    force: bool,
    realms: &[WallyRealm],
    storages: &HashMap<&'static str, LinkTargetStorage>,
    documents: &HashMap<PathBuf, SettingsBytecode>,
    lock: &LinkLock,
    input_hash: &str,
) -> Result<(bool, HashMap<String, bool>)> {
    if force {
        return Ok((false, HashMap::new()));
    }
    let mut freshness = HashMap::new();
    for realm in realms {
        let Some(storage) = storages.get(realm.realm) else {
            continue;
        };
        let lock_key = wally_realm_lock_key(realm, storage);
        let settings_file = match (realm.packages_dir.is_dir(), storage.settings_file.as_ref()) {
            (true, Some(settings_file)) => settings_file,
            _ if realm.required || lock.entries.contains_key(&lock_key) => {
                freshness.insert(lock_key, false);
                return Ok((false, freshness));
            }
            _ => continue,
        };
        let previous_hash = lock
            .entries
            .get(&lock_key)
            .and_then(|entry| entry.resolved_ref.as_deref());
        let fresh = previous_hash == Some(input_hash)
            && wally_realm_is_fresh(
                documents
                    .get(settings_file)
                    .context("Wally settings store was not loaded")?,
                realm,
                storage,
            )?;
        freshness.insert(lock_key, fresh);
        if !fresh {
            return Ok((false, freshness));
        }
    }
    Ok((true, freshness))
}

fn run_wally_install(
    skip_install: bool,
    all_realms_fresh: bool,
    wally_path: &str,
    project_root: &Path,
) -> Result<Value> {
    if skip_install {
        return Ok(json!({
            "skipped": true,
            "command": wally_path,
            "args": ["install"],
        }));
    }
    if all_realms_fresh {
        return Ok(json!({
            "skipped": true,
            "reason": "current",
            "command": wally_path,
            "args": ["install"],
        }));
    }
    run_checked_external_tool("wally install", wally_path, &["install"], project_root)
}

fn commit_wally_changes(
    project_root: &Path,
    src_root: &Path,
    documents: &HashMap<PathBuf, SettingsBytecode>,
    settings_outputs: &HashMap<PathBuf, PathBuf>,
    lock: &LinkLock,
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut Vec<PathBuf>,
) -> Result<()> {
    stage_settings_document_writes(documents, settings_outputs, writes, removals)?;
    let gitignore = project_root.join(".renium").join(".gitignore");
    if !gitignore.exists() {
        writes.insert(gitignore, RENIUM_DIR_GITIGNORE.as_bytes().to_vec());
    }
    writes.insert(link_lock_path(project_root), serialize_link_lock(lock)?);
    removals.retain(|path| !writes.keys().any(|write| path_key(write) == path_key(path)));
    removals.sort_by_key(|path| path_key(path));
    removals.dedup_by(|left, right| path_key(left) == path_key(right));
    apply_file_mutations(writes, removals)?;
    prune_removed_source_dirs(src_root, removals);
    Ok(())
}

pub(crate) fn sync_wally_packages(args: SyncWallyPackagesArgs) -> Result<()> {
    let pretty = args.pretty;
    let result = sync_wally_packages_result(args)?;
    print_json_output(&result, pretty)
}

pub(crate) fn sync_wally_packages_result(mut args: SyncWallyPackagesArgs) -> Result<Value> {
    apply_configured_project_layout(&mut args.project.project_root, &mut args.project.src_root)?;
    let project_root = resolve_existing_project_root(&args.project.project_root)?;
    let src_root = absolutize_under(&project_root, &args.project.src_root);
    let manifest = absolutize_under(&project_root, &args.manifest);
    if !manifest.exists() {
        bail!(
            "No Wally manifest found at {}. Create wally.toml first, then run this command again.",
            manifest.display()
        );
    }

    let realms = build_wally_realms(&project_root, &args)?;
    if realms.is_empty() {
        bail!("No Wally realms selected. Use --realms shared,server,dev.");
    }
    validate_wally_target_overlaps(&project_root, &realms)?;
    let initial_lock = read_link_lock(&project_root)?;
    let realm_storages =
        resolve_wally_realm_storages(&project_root, &src_root, &realms, &initial_lock)?;
    let settings_files = collect_project_settings_files(
        &src_root,
        realm_storages
            .values()
            .filter_map(|storage| storage.settings_file.clone()),
    )?;
    let mut guards = Vec::with_capacity(settings_files.len());
    for settings_file in &settings_files {
        guards.push(acquire_settings_file_lock(settings_file)?);
    }
    let existing_settings_files = settings_files
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let (mut documents, mut settings_outputs) = load_settings_documents(&existing_settings_files)?;
    for realm in &realms {
        if let Some(storage) = realm_storages.get(realm.realm) {
            ensure_settings_document(
                &mut documents,
                &mut settings_outputs,
                storage,
                &realm.service,
            );
        }
    }

    let initial_lock_hash = wally_inputs_hash(&project_root, &manifest)?;
    let (all_realms_fresh, initial_realm_fresh) = initial_wally_realm_freshness(
        args.force,
        &realms,
        &realm_storages,
        &documents,
        &initial_lock,
        &initial_lock_hash,
    )?;
    let wally_result = run_wally_install(
        args.skip_install,
        all_realms_fresh,
        &args.wally_path,
        &project_root,
    )?;

    let lock_hash = wally_inputs_hash(&project_root, &manifest)?;
    let mut lock = read_link_lock(&project_root)?;

    let mut realm_results: Vec<Value> = Vec::new();
    let mut changed_paths: Vec<String> = Vec::new();
    let mut changed_seen: HashSet<String> = HashSet::new();
    let mut target_settings_ids: Vec<String> = Vec::new();
    let mut settings_seen: HashSet<String> = HashSet::new();
    let mut removed_targets: Vec<Value> = Vec::new();
    let mut transaction_writes = BTreeMap::new();
    let mut transaction_removals = Vec::new();
    let mut applied = 0usize;
    let mut skipped = 0usize;

    for realm in &realms {
        let storage = realm_storages.get(realm.realm);
        let lock_key = storage.map_or_else(
            || {
                format!(
                    "wally:{}:{}/{}",
                    realm.realm, realm.service, realm.target_name
                )
            },
            |storage| wally_realm_lock_key(realm, storage),
        );
        if !realm.packages_dir.is_dir() {
            if !realm.required {
                if !lock.entries.contains_key(&lock_key) {
                    continue;
                }
                if let Some(storage) = storage
                    && let Some(settings_file) = storage.settings_file.as_ref()
                {
                    let external_references =
                        referenced_settings_ids_outside(&documents, settings_file);
                    let document = documents
                        .get_mut(settings_file)
                        .context("Wally owner settings store was not loaded")?;
                    let outcome =
                        import_wally_realm(document, realm, storage, true, &external_references)?;
                    for path in &outcome.changed_paths {
                        if changed_seen.insert(path_key(Path::new(path))) {
                            changed_paths.push(path.clone());
                        }
                    }
                    if !outcome.removed_target.is_null() {
                        removed_targets.push(outcome.removed_target.clone());
                    }
                    transaction_removals.extend(outcome.removals);
                    realm_results.push(json!({
                        "realm": realm.realm,
                        "service": outcome.service,
                        "targetName": realm.target_name,
                        "settingsFile": settings_file,
                        "settingsIds": [],
                        "sourceWrites": [],
                        "removedSettingsIds": outcome.removed_settings_ids,
                        "removedTarget": outcome.removed_target,
                        "skipped": false,
                        "removed": true,
                    }));
                } else {
                    let target_dir = storage.map_or_else(
                        || src_root.join(&realm.service).join(&realm.target_name),
                        |storage| storage.target_path.clone(),
                    );
                    if target_dir.is_dir() {
                        for entry in WalkDir::new(&target_dir).follow_links(false) {
                            let entry = entry?;
                            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                                transaction_removals.push(entry.path().to_path_buf());
                            }
                        }
                    }
                    realm_results.push(json!({
                        "realm": realm.realm,
                        "service": realm.service,
                        "targetName": realm.target_name,
                        "settingsFile": Value::Null,
                        "settingsIds": [],
                        "sourceWrites": [],
                        "removedSettingsIds": [],
                        "removedTarget": Value::Null,
                        "skipped": false,
                        "removed": true,
                    }));
                }
                lock.entries.remove(&lock_key);
                applied += 1;
                continue;
            }
            if args.skip_install {
                bail!(
                    "Wally packages directory was not found at {}. Run without --skip-install so `wally install` can create it.",
                    realm.packages_dir.display()
                );
            }
            fs::create_dir_all(&realm.packages_dir)
                .with_context(|| format!("Failed to create {}", realm.packages_dir.display()))?;
        }
        let storage = storage.context("Wally realm has no writable project owner")?;
        let Some(settings_file) = storage.settings_file.as_ref() else {
            if realm.required {
                bail!(
                    "No Renium bytecode settings file found at {}. Pull from Studio once before syncing Wally packages.",
                    service_settings_path(&storage.source_root).display()
                );
            }
            continue;
        };

        let previous_hash = lock
            .entries
            .get(&lock_key)
            .and_then(|entry| entry.resolved_ref.clone());
        let realm_is_fresh = if args.force || previous_hash.as_deref() != Some(lock_hash.as_str()) {
            false
        } else if initial_lock_hash == lock_hash
            && initial_realm_fresh.get(&lock_key).copied() == Some(true)
        {
            true
        } else {
            wally_realm_is_fresh(
                documents
                    .get(settings_file)
                    .context("Wally settings store was not loaded")?,
                realm,
                storage,
            )?
        };
        if realm_is_fresh {
            skipped += 1;
            realm_results.push(json!({
                "realm": realm.realm,
                "service": realm.service,
                "targetName": realm.target_name,
                "skipped": true,
            }));
            continue;
        }

        let external_references = referenced_settings_ids_outside(&documents, settings_file);
        let document = documents
            .get_mut(settings_file)
            .context("Wally owner settings store was not loaded")?;
        let outcome = import_wally_realm(document, realm, storage, false, &external_references)?;

        applied += 1;
        for path in &outcome.changed_paths {
            if changed_seen.insert(path_key(Path::new(path))) {
                changed_paths.push(path.clone());
            }
        }
        for id in &outcome.settings_ids {
            if settings_seen.insert(id.clone()) {
                target_settings_ids.push(id.clone());
            }
        }
        if !outcome.removed_target.is_null() {
            removed_targets.push(outcome.removed_target.clone());
        }
        transaction_writes.extend(outcome.writes);
        transaction_removals.extend(outcome.removals);
        lock.entries.entry(lock_key).or_default().resolved_ref = Some(lock_hash.clone());
        realm_results.push(json!({
            "realm": realm.realm,
            "service": outcome.service,
            "targetName": realm.target_name,
            "settingsFile": settings_file,
            "settingsIds": outcome.settings_ids,
            "sourceWrites": outcome.source_writes,
            "removedSettingsIds": outcome.removed_settings_ids,
            "removedTarget": outcome.removed_target,
            "skipped": false,
        }));
    }

    if applied > 0 {
        commit_wally_changes(
            &project_root,
            &src_root,
            &documents,
            &settings_outputs,
            &lock,
            &mut transaction_writes,
            &mut transaction_removals,
        )?;
    }
    drop(guards);

    let primary = realm_results
        .iter()
        .find(|value| value.get("skipped").and_then(Value::as_bool) == Some(false))
        .or_else(|| realm_results.first());
    let compact_realms = realm_results
        .iter()
        .map(|realm| {
            let mut compact = json!({
                "realm": realm.get("realm"),
                "service": realm.get("service"),
                "targetName": realm.get("targetName"),
                "skipped": realm.get("skipped"),
            });
            if let Some(removed) = realm.get("removed") {
                compact["removed"] = removed.clone();
            }
            compact
        })
        .collect::<Vec<_>>();
    let mut result = json!({
        "ok": true,
        "projectRoot": project_root,
        "manifest": manifest,
        "appliedRealms": applied,
        "skippedRealms": skipped,
        "processedPathCount": changed_paths.len(),
        "importedInstanceCount": target_settings_ids.len(),
        "removedTargetCount": removed_targets.len(),
        "realms": compact_realms,
        "wallyInstall": wally_result,
    });
    if args.details {
        let object = result
            .as_object_mut()
            .context("Wally result was not an object")?;
        if let Some(primary) = primary {
            for key in ["service", "targetName", "settingsFile"] {
                if let Some(value) = primary.get(key).filter(|value| !value.is_null()) {
                    object.insert(key.to_string(), value.clone());
                }
            }
        }
        object.insert("changedPaths".to_string(), json!(changed_paths));
        object.insert("targetSettingsIds".to_string(), json!(target_settings_ids));
        object.insert("removedTargets".to_string(), json!(removed_targets));
        object.insert("realms".to_string(), json!(realm_results));
    }
    Ok(result)
}

fn validate_wally_target_name(raw: &str, label: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("Wally {label} cannot be empty");
    }
    validate_filesystem_instance_name(value, &format!("Wally {label}"))?;
    Ok(value.to_string())
}
