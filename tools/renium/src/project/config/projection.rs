use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use globset::escape as escape_glob;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::editor::paths::infer_source_script;
use crate::settings::bytecode::{
    SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode, is_reference_object,
    reindex_reference_indices,
};
use crate::settings::tree::{editor_child_stems, settings_children_by_parent};
use crate::system::files::{
    atomic_write_file, create_unique_directory, normalized_child_stem_key, path_extension_is,
    sanitize_name, service_settings_path, sha256_hex,
};

use super::adapter_format::{
    AdapterFormat, adapter_format, localization_csv_to_json, render_adapter,
    validate_adapter_source,
};
use super::model_json::{contains_reference_value, stage_model_json};
use super::projection_references::normalize_stage_references;
use super::syncback::{
    is_nested_project_path, projection_instance_path_parts, stabilize_reference_indices,
    stabilize_reference_indices_with_paths, write_file_transaction,
};
use super::validation::{validate_nested_project, validate_project};
use super::{
    AdapterDirection, AdapterSpec, CachedProjection, CompiledProjection, FilterDirection,
    FilterRule, FilterScope, LoadedProject, MetadataSidecar, MountOwnership, NESTED_STAGE_STACK,
    OwnedFilterCandidate, PROJECT_SCHEMA_VERSION, PROJECTION_CACHE, PROJECTION_IDENTITY_STACK,
    PROJECTION_TRANSFORM_STACK, ProjectMount, ProjectNode, ProjectTarget, ProjectionEntry,
    ProjectionFieldOwner, ProjectionIdentity, ProjectionStage, ProjectionTransform,
    ScriptExtensionPolicy, SyncRule, absolute_path, active_target_ordinals, cache_script_naming,
    compile_glob, filter_allows_scope, filter_path_segments, load_nested_project,
    parse_jsonc_value, path_slash, project_script_naming, project_source_roots,
    project_source_to_staged_relative, project_source_to_staged_relatives, project_tree_nodes,
    projection_path_key, record_projection_identity, relocate_cached_script_naming,
    remove_cached_script_naming, remove_empty_stage_parents, resolve_project_write_path,
    validate_instance_target, with_project_target,
};

pub(super) fn compile_projection(loaded: &LoadedProject) -> CompiledProjection {
    let mut entries = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path.as_deref() {
            entries.push(projection_entry(
                "tree",
                &path_slash(path),
                &target.join("."),
                None,
                None,
            ));
        }
    }
    for mount in &loaded.project.mounts {
        let target = mount.target.key();
        entries.push(projection_entry(
            "mount",
            &path_slash(&mount.source),
            &target,
            Some(mount.ownership),
            None,
        ));
    }
    for adapter in &loaded.project.adapters {
        let target = adapter.target.key();
        entries.push(projection_entry(
            "adapter",
            &path_slash(&adapter.source),
            &target,
            None,
            Some(adapter.direction),
        ));
    }
    for rule in &loaded.project.sync_rules {
        entries.push(projection_entry(
            "sync-rule",
            &rule.pattern,
            &rule.middleware,
            None,
            None,
        ));
    }
    for pattern in &loaded.project.glob_ignore_paths {
        entries.push(projection_entry("ignore", pattern, "", None, None));
    }
    entries.sort_by(|a, b| (&a.target, &a.source).cmp(&(&b.target, &b.source)));
    CompiledProjection {
        schema_version: PROJECT_SCHEMA_VERSION,
        project: loaded.path.display().to_string(),
        entries,
    }
}

fn projection_entry(
    kind: &str,
    source: &str,
    target: &str,
    ownership: Option<MountOwnership>,
    direction: Option<AdapterDirection>,
) -> ProjectionEntry {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(source.as_bytes());
    digest.update([0]);
    digest.update(target.as_bytes());
    let id = format!("projection:{:x}", digest.finalize());
    ProjectionEntry {
        id,
        kind: kind.to_string(),
        source: source.to_string(),
        target: target.to_string(),
        ownership,
        direction,
    }
}

pub fn project_requires_temporary_stage(loaded: &LoadedProject) -> Result<bool> {
    if !loaded.project.mounts.is_empty()
        || !loaded.project.adapters.is_empty()
        || project_tree_requires_stage(loaded)
    {
        return Ok(true);
    }
    let source_root = loaded.root.join(&loaded.project.source_root);
    Ok(contains_metadata_sidecars(&source_root)?
        || source_files_require_stage(loaded, &source_root)?)
}

fn project_tree_requires_stage(loaded: &LoadedProject) -> bool {
    loaded.project.tree.iter().any(|(service, node)| {
        let direct_source = loaded.root.join(&loaded.project.source_root).join(service);
        node.path.as_deref().is_some_and(|path| {
            absolute_path(&loaded.root.join(path)) != absolute_path(&direct_source)
        }) || node.id.is_some()
            || node.class_name.is_some()
            || !node.properties.is_empty()
            || !node.attributes.is_empty()
            || node.tags.is_some()
            || node.ignore_unknown_instances.is_some()
            || !node.children.is_empty()
    })
}

fn source_files_require_stage(loaded: &LoadedProject, source_root: &Path) -> Result<bool> {
    if !source_root.is_dir()
        || (loaded.project.sync_rules.is_empty() && loaded.project.glob_ignore_paths.is_empty())
    {
        return Ok(false);
    }
    for entry in walkdir::WalkDir::new(source_root).min_depth(1) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(source_root)?;
        let mut matched_rule = false;
        for rule in &loaded.project.sync_rules {
            if sync_rule_matches(rule, relative)? {
                matched_rule = true;
                break;
            }
        }
        if matched_rule
            || (!loaded.project.glob_ignore_paths.is_empty()
                && path_is_ignored(loaded, entry.path())?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn fresh_projection_stage(parent: &Path, prefix: &str) -> Result<PathBuf> {
    create_unique_directory(parent, prefix)
}

pub fn stage_project(loaded: &LoadedProject) -> Result<ProjectionStage> {
    validate_project(loaded)?;
    if !project_requires_temporary_stage(loaded)? {
        cache_script_naming(
            &loaded.root.join(&loaded.project.source_root),
            &loaded.project,
        );
        return Ok(ProjectionStage {
            root: loaded.root.join(&loaded.project.source_root),
            temporary: false,
            cleanup: false,
            transforms: Vec::new(),
            identities: HashMap::new(),
        });
    }

    let root = fresh_projection_stage(&loaded.root.join(".renium").join("build-staging"), "")?;
    PROJECTION_TRANSFORM_STACK.with(|stack| stack.borrow_mut().push(Vec::new()));
    PROJECTION_IDENTITY_STACK.with(|stack| stack.borrow_mut().push(HashMap::new()));
    let result = (|| {
        cache_script_naming(&root, &loaded.project);
        let source_root = loaded.root.join(&loaded.project.source_root);
        if source_root.is_dir() {
            stage_source_directory(loaded, &root, &source_root, &root, false, None)?;
        }
        for mount in &loaded.project.mounts {
            stage_mount(loaded, &root, mount)?;
        }
        for (service, node) in &loaded.project.tree {
            stage_tree_node(loaded, &root, std::slice::from_ref(service), node)?;
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction != AdapterDirection::FromProject {
                stage_adapter(loaded, &root, adapter)?;
            }
        }
        refresh_stage_settings(&root)?;
        normalize_stage_references(&root)?;
        Ok(())
    })();
    let transforms = PROJECTION_TRANSFORM_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection transform stack is balanced")
    });
    let identities = PROJECTION_IDENTITY_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection identity stack is balanced")
    });
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&root);
        remove_empty_stage_parents(&root);
        return Err(error);
    }
    Ok(ProjectionStage {
        root,
        temporary: true,
        cleanup: true,
        transforms,
        identities,
    })
}

pub fn stage_project_cached(
    loaded: &LoadedProject,
    changed_sources: &[PathBuf],
) -> Result<ProjectionStage> {
    let project_hash = sha256_hex(&serde_json::to_vec(&loaded.project)?);
    let key = fs::canonicalize(&loaded.path).unwrap_or_else(|_| absolute_path(&loaded.path));
    let cache = PROJECTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let reusable = cache
        .get(&key)
        .is_some_and(|entry| entry.project_hash == project_hash && entry.root.is_dir());
    let source_shape_changed = reusable
        && cache.get(&key).is_some_and(|entry| {
            changed_sources.iter().any(|source| {
                entry
                    .source_shape
                    .get(&projection_path_key(source))
                    .copied()
                    != projection_source_kind(source)
            })
        });
    if source_shape_changed {
        validate_project(loaded)?;
    }
    let created = if reusable {
        false
    } else {
        if let Some(previous) = cache.remove(&key) {
            remove_cached_script_naming(&previous.root);
            let _ = fs::remove_dir_all(previous.root);
        }
        let Some(entry) = create_cached_projection(loaded, &key, project_hash.clone())? else {
            drop(cache);
            return stage_project(loaded);
        };
        cache.insert(key.clone(), entry);
        true
    };
    let entry = cache
        .get_mut(&key)
        .context("Projection cache entry disappeared")?;
    if created {
        return Ok(ProjectionStage {
            root: entry.root.clone(),
            temporary: true,
            cleanup: false,
            transforms: entry.transforms.clone(),
            identities: entry.identities.clone(),
        });
    }
    let mut services = BTreeSet::new();
    let mut rebuild_all = false;
    for source in changed_sources {
        if absolute_path(source) == absolute_path(&loaded.path) {
            rebuild_all = true;
            break;
        }
        let relatives = project_source_to_staged_relatives(loaded, source)?;
        if relatives.is_empty() {
            rebuild_all = true;
            break;
        }
        for relative in relatives {
            let Some(Component::Normal(service)) = relative.components().next() else {
                rebuild_all = true;
                break;
            };
            let Some(service) = service.to_str() else {
                rebuild_all = true;
                break;
            };
            services.insert(service.to_string());
        }
        if rebuild_all {
            break;
        }
    }
    if rebuild_all {
        let previous = cache
            .remove(&key)
            .context("Projection cache entry disappeared")?;
        remove_cached_script_naming(&previous.root);
        let _ = fs::remove_dir_all(previous.root);
        let Some(entry) = create_cached_projection(loaded, &key, project_hash)? else {
            drop(cache);
            return stage_project(loaded);
        };
        cache.insert(key.clone(), entry);
    }
    let entry = cache
        .get_mut(&key)
        .context("Projection cache entry disappeared")?;
    if !changed_sources.is_empty()
        && patch_cached_projection_scripts(loaded, &entry.root, changed_sources)?
    {
        if source_shape_changed {
            entry.source_shape = projection_source_shape(loaded)?;
        }
        return Ok(ProjectionStage {
            root: entry.root.clone(),
            temporary: true,
            cleanup: false,
            transforms: entry.transforms.clone(),
            identities: entry.identities.clone(),
        });
    }
    if !rebuild_all && !services.is_empty() {
        match rebuild_cached_projection_services(
            loaded,
            &entry.root,
            &services,
            &entry.transforms,
            &entry.identities,
        ) {
            Ok((transforms, identities)) => {
                entry.transforms = transforms;
                entry.identities = identities;
                if source_shape_changed {
                    entry.source_shape = projection_source_shape(loaded)?;
                }
            }
            Err(error) => {
                let root = entry.root.clone();
                cache.remove(&key);
                remove_cached_script_naming(&root);
                let _ = fs::remove_dir_all(root);
                return Err(error);
            }
        }
    }
    Ok(ProjectionStage {
        root: entry.root.clone(),
        temporary: true,
        cleanup: false,
        transforms: entry.transforms.clone(),
        identities: entry.identities.clone(),
    })
}

fn patch_cached_projection_scripts(
    loaded: &LoadedProject,
    cache_root: &Path,
    changed_sources: &[PathBuf],
) -> Result<bool> {
    let mut writes = Vec::new();
    for source in changed_sources {
        if !source.is_file() || !path_extension_is(source, &["lua", "luau"]) {
            return Ok(false);
        }
        let relatives = project_source_to_staged_relatives(loaded, source)?;
        if relatives.is_empty() {
            return Ok(false);
        }
        let bytes = fs::read(source)?;
        for relative in relatives {
            if !path_extension_is(&relative, &["lua", "luau"]) {
                return Ok(false);
            }
            let destination = cache_root.join(relative);
            if !destination.is_file() {
                return Ok(false);
            }
            writes.push((destination, bytes.clone()));
        }
    }
    write_file_transaction(&writes)?;
    Ok(true)
}

fn create_cached_projection(
    loaded: &LoadedProject,
    key: &Path,
    project_hash: String,
) -> Result<Option<CachedProjection>> {
    let staged = stage_project(loaded)?;
    if !staged.is_temporary() {
        return Ok(None);
    }
    let mut digest = Sha256::new();
    digest.update(key.to_string_lossy().as_bytes());
    let cache_root = env::temp_dir()
        .join("renium-projection-cache")
        .join(format!("{}-{:x}", std::process::id(), digest.finalize()));
    if cache_root.exists() {
        fs::remove_dir_all(&cache_root)?;
    }
    if let Some(parent) = cache_root.parent() {
        fs::create_dir_all(parent)?;
    }
    let staged_root = staged.root().to_path_buf();
    if fs::rename(&staged_root, &cache_root).is_err() {
        copy_directory_tree(&staged_root, &cache_root)?;
    }
    relocate_cached_script_naming(&staged_root, &cache_root);
    Ok(Some(CachedProjection {
        root: cache_root,
        project_hash,
        source_shape: projection_source_shape(loaded)?,
        transforms: staged.transforms.clone(),
        identities: staged.identities.clone(),
    }))
}

fn projection_source_kind(path: &Path) -> Option<u8> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some(if metadata.file_type().is_symlink() {
        3
    } else if metadata.is_dir() {
        2
    } else if metadata.is_file() {
        1
    } else {
        4
    })
}

fn projection_source_shape(loaded: &LoadedProject) -> Result<HashMap<String, u8>> {
    let mut shape = HashMap::new();
    for root in project_source_roots(loaded)? {
        if let Some(kind) = projection_source_kind(&root) {
            shape.insert(projection_path_key(&root), kind);
        }
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .min_depth(1)
            .follow_links(false)
        {
            let entry = entry?;
            let kind = if entry.file_type().is_symlink() {
                3
            } else if entry.file_type().is_dir() {
                2
            } else if entry.file_type().is_file() {
                1
            } else {
                4
            };
            shape.insert(projection_path_key(entry.path()), kind);
        }
    }
    Ok(shape)
}

pub(super) fn copy_directory_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in walkdir::WalkDir::new(source).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn rebuild_cached_projection_services(
    loaded: &LoadedProject,
    root: &Path,
    services: &BTreeSet<String>,
    previous_transforms: &[ProjectionTransform],
    previous_identities: &HashMap<String, ProjectionIdentity>,
) -> Result<(
    Vec<ProjectionTransform>,
    HashMap<String, ProjectionIdentity>,
)> {
    let mut replaced_staged_ids = HashSet::new();
    for service in services {
        let destination = root.join(service);
        let settings = service_settings_path(&destination);
        if settings.is_file() {
            replaced_staged_ids.extend(
                SettingsBytecode::read_file(&settings)?
                    .instances
                    .into_iter()
                    .map(|instance| instance.settings_id),
            );
        }
        if destination.exists() {
            remove_cached_script_naming(&destination);
            fs::remove_dir_all(&destination)?;
        }
    }
    PROJECTION_TRANSFORM_STACK.with(|stack| stack.borrow_mut().push(Vec::new()));
    PROJECTION_IDENTITY_STACK.with(|stack| stack.borrow_mut().push(HashMap::new()));
    let result = (|| {
        for service in services {
            let source = loaded.root.join(&loaded.project.source_root).join(service);
            if source.is_dir() {
                stage_source_directory(
                    loaded,
                    root,
                    &source,
                    &root.join(service),
                    false,
                    Some(Path::new(service)),
                )?;
            }
        }
        for mount in &loaded.project.mounts {
            let target = target_segments(&mount.target)?;
            if target
                .first()
                .is_some_and(|service| services.contains(service))
            {
                stage_mount(loaded, root, mount)?;
            }
        }
        for (service, node) in &loaded.project.tree {
            if services.contains(service) {
                stage_tree_node(loaded, root, std::slice::from_ref(service), node)?;
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let target = target_segments(&adapter.target)?;
            if target
                .first()
                .is_some_and(|service| services.contains(service))
            {
                stage_adapter(loaded, root, adapter)?;
            }
        }
        for service in services {
            let service_dir = root.join(service);
            if service_dir.is_dir() {
                refresh_stage_service_settings(&service_dir)?;
            }
        }
        normalize_stage_references(root)
    })();
    let refreshed = PROJECTION_TRANSFORM_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection transform stack is balanced")
    });
    let refreshed_identities = PROJECTION_IDENTITY_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection identity stack is balanced")
    });
    result?;
    let mut transforms = previous_transforms
        .iter()
        .filter(|transform| {
            transform
                .target
                .first()
                .is_none_or(|service| !services.contains(service))
        })
        .cloned()
        .collect::<Vec<_>>();
    transforms.extend(refreshed);
    transforms.sort_by(|left, right| left.target.cmp(&right.target));
    let mut identities = previous_identities
        .iter()
        .filter(|(staged_id, _)| !replaced_staged_ids.contains(*staged_id))
        .map(|(staged_id, identity)| (staged_id.clone(), identity.clone()))
        .collect::<HashMap<_, _>>();
    identities.extend(refreshed_identities);
    Ok((transforms, identities))
}

fn stage_tree_node(
    loaded: &LoadedProject,
    stage: &Path,
    target: &[String],
    node: &ProjectNode,
) -> Result<()> {
    let target_path = target_fs_path(stage, target);
    if let Some(source) = node.path.as_deref() {
        let source = loaded.root.join(source);
        if source.is_dir() {
            stage_source_directory(loaded, stage, &source, &target_path, true, None)?;
            let settings = service_settings_path(&source);
            if settings.is_file() {
                merge_settings_document_at_target(stage, target, &settings)?;
            }
        } else if source.is_file() {
            if is_nested_project_path(&source) {
                let nested = load_nested_project(&source)?;
                stage_nested_project_at_target(&nested, stage, target)?;
            } else {
                copy_file_to_target(loaded, &source, &target_path)?;
            }
        } else {
            bail!("Project tree source does not exist: {}", source.display());
        }
    } else {
        fs::create_dir_all(&target_path)?;
    }
    if node.id.is_some()
        || node.class_name.is_some()
        || !node.properties.is_empty()
        || !node.attributes.is_empty()
        || node.tags.is_some()
    {
        let inferred_class;
        let class_name = if let Some(class_name) = node.class_name.as_deref() {
            class_name
        } else if target.len() == 1 {
            target[0].as_str()
        } else {
            let service = target
                .first()
                .context("Project tree target must include a service")?;
            let settings = service_settings_path(&stage.join(service));
            inferred_class = if settings.is_file() {
                let document = SettingsBytecode::read_file(&settings)?;
                find_document_target_optional(&document, target)?
                    .map(|index| document.instances[index].class_name.clone())
            } else {
                None
            };
            inferred_class.as_deref().unwrap_or("Folder")
        };
        let properties = normalize_property_map(Some(class_name), &node.properties)
            .with_context(|| format!("Invalid properties on '{}'", target.join(".")))?;
        let attributes = normalize_property_map(None, &node.attributes)
            .with_context(|| format!("Invalid attributes on '{}'", target.join(".")))?;
        override_stage_identity(
            stage,
            target,
            node.class_name.as_deref(),
            node.id.as_deref(),
        )?;
        update_stage_instance(
            stage,
            target,
            class_name,
            node.id.as_deref(),
            &properties,
            &attributes,
            node.tags.as_deref(),
        )?;
    }
    for (name, value) in &node.children {
        if name.starts_with('$') {
            continue;
        }
        let child: ProjectNode = serde_json::from_value(value.clone()).with_context(|| {
            format!("Project tree node '{}' must be an object", target.join("."))
        })?;
        let mut child_target = target.to_vec();
        child_target.push(name.clone());
        stage_tree_node(loaded, stage, &child_target, &child)?;
    }
    Ok(())
}

pub(super) fn stage_mount(
    loaded: &LoadedProject,
    stage: &Path,
    mount: &ProjectMount,
) -> Result<()> {
    with_project_target(&mount.target, |target| {
        stage_mount_target(loaded, stage, mount, target)
    })
}

fn stage_mount_target(
    loaded: &LoadedProject,
    stage: &Path,
    mount: &ProjectMount,
    target: &[String],
) -> Result<()> {
    let source = loaded.root.join(&mount.source);
    if !source.exists() && mount.optional {
        return Ok(());
    }
    if source.is_dir() {
        let destination = target_fs_path(stage, target);
        fs::create_dir_all(&destination)?;
        stage_source_directory(loaded, stage, &source, &destination, true, None)?;
        let settings = service_settings_path(&source);
        if settings.is_file() {
            merge_settings_document_at_target(stage, target, &settings)?;
        } else {
            update_stage_instance(
                stage,
                target,
                "Folder",
                None,
                &Map::new(),
                &Map::new(),
                None,
            )?;
        }
        return Ok(());
    }
    let name = source
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.ends_with(".project.json") || name.ends_with(".project.jsonc") {
        let nested = load_nested_project(&source)?;
        stage_nested_project_at_target(&nested, stage, target)?;
        return Ok(());
    }
    if path_extension_is(&source, &["rbxm", "rbxmx"]) {
        import_model_at_target(stage, target, &source)?;
        return Ok(());
    }
    if path_extension_is(&source, &["renium"]) {
        merge_settings_mount(stage, target, &source)?;
        return Ok(());
    }
    if source.is_file() {
        copy_file_to_target(loaded, &source, &target_fs_path(stage, target))?;
        return Ok(());
    }
    bail!("Mount source does not exist: {}", source.display())
}

pub(super) fn stage_adapter(
    loaded: &LoadedProject,
    stage: &Path,
    adapter: &AdapterSpec,
) -> Result<()> {
    with_project_target(&adapter.target, |target| {
        stage_adapter_target(loaded, stage, adapter, target)
    })
}

fn stage_adapter_target(
    loaded: &LoadedProject,
    stage: &Path,
    adapter: &AdapterSpec,
    target: &[String],
) -> Result<()> {
    let source = loaded.root.join(&adapter.source);
    let format = adapter_format(adapter)?;
    validate_adapter_source(&source, format)?;
    stage_adapter_format(loaded, stage, target, &source, format)
}

fn stage_adapter_format(
    loaded: &LoadedProject,
    stage: &Path,
    target: &[String],
    source: &Path,
    format: AdapterFormat,
) -> Result<()> {
    match format {
        AdapterFormat::Text => stage_text_value(stage, target, source),
        AdapterFormat::Csv => stage_localization_table(stage, target, source),
        AdapterFormat::ModelJson => stage_model_json(stage, target, source),
        AdapterFormat::Rbxm | AdapterFormat::Rbxmx => import_model_at_target(stage, target, source),
        AdapterFormat::NestedProject => {
            let nested = load_nested_project(source)?;
            stage_nested_project_at_target(&nested, stage, target)
        }
        _ => stage_module_data(loaded, stage, target, source, format),
    }
}

fn stage_text_value(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    let value =
        fs::read_to_string(source).with_context(|| format!("{} is not UTF-8", source.display()))?;
    let properties = Map::from_iter([("Value".to_string(), Value::String(value))]);
    update_stage_instance(
        stage,
        target,
        "StringValue",
        None,
        &properties,
        &Map::new(),
        None,
    )
}

fn stage_localization_table(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    let value = localization_csv_to_json(&fs::read_to_string(source)?)?;
    let properties = Map::from_iter([("Contents".to_string(), Value::String(value))]);
    update_stage_instance(
        stage,
        target,
        "LocalizationTable",
        None,
        &properties,
        &Map::new(),
        None,
    )
}

fn stage_module_data(
    loaded: &LoadedProject,
    stage: &Path,
    target: &[String],
    source: &Path,
    format: AdapterFormat,
) -> Result<()> {
    let bytes = render_adapter(source, format)?;
    let output = adapter_target_script_path(loaded, stage, target);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write_file(&output, &bytes)?;
    let source = String::from_utf8(bytes).context("Generated adapter source is not UTF-8")?;
    let properties = Map::from_iter([("Source".to_string(), Value::String(source))]);
    update_stage_instance(
        stage,
        target,
        "ModuleScript",
        None,
        &properties,
        &Map::new(),
        None,
    )
}

fn update_stage_instance(
    stage: &Path,
    target: &[String],
    class_name: &str,
    explicit_id: Option<&str>,
    properties: &Map<String, Value>,
    attributes: &Map<String, Value>,
    tags: Option<&[String]>,
) -> Result<()> {
    let ordinals = active_target_ordinals(target);
    let service = target
        .first()
        .context("Projection target must include a service")?;
    if ordinals.first().copied().unwrap_or(1) != 1 {
        bail!("Projection service roots always have ordinal 1");
    }
    let service_dir = stage.join(service);
    fs::create_dir_all(&service_dir)?;
    let settings_path = service_settings_path(&service_dir);
    let mut document = if settings_path.is_file() {
        SettingsBytecode::read_file(&settings_path)?
    } else {
        crate::rbx::model::source_only_settings_document(&service_dir, service)?
    };
    let mut children_by_parent = settings_children_by_parent(&document);
    let mut parent_index = 0usize;
    for (position, name) in target.iter().enumerate().skip(1) {
        let final_node = position + 1 == target.len();
        let expected_class = if final_node { class_name } else { "Folder" };
        let candidates = children_by_parent
            .get(parent_index)
            .map_or(&[][..], Vec::as_slice);
        let ordinal = ordinals.get(position).copied();
        let selected = ordinal.unwrap_or(1);
        let (matched, match_count) = select_named_child(&document, candidates, name, selected);
        if ordinal.is_none() && match_count > 1 {
            bail!(
                "Projection target '{}' is ambiguous because '{}' has duplicate children",
                target.join("."),
                name
            );
        }
        if selected > match_count + 1 {
            bail!(
                "Projection target '{}' cannot create ordinal {} for '{}' before ordinal {} exists",
                target.join("."),
                selected,
                name,
                selected - 1
            );
        }
        let index = if let Some(index) = matched {
            if final_node && document.instances[index].class_name != expected_class {
                if document.instances[index].class_name == "Folder"
                    && matches!(
                        expected_class,
                        "Script"
                            | "LocalScript"
                            | "ModuleScript"
                            | "StringValue"
                            | "LocalizationTable"
                    )
                {
                    document.instances[index].class_name = expected_class.to_string();
                } else {
                    bail!(
                        "Projection target '{}' already exists as {}, expected {}",
                        target.join("."),
                        document.instances[index].class_name,
                        expected_class
                    );
                }
            }
            index
        } else {
            let identity = target
                .iter()
                .enumerate()
                .take(position + 1)
                .map(|(index, segment)| {
                    format!("{}[{}]", segment, ordinals.get(index).copied().unwrap_or(1))
                })
                .collect::<Vec<_>>()
                .join(".");
            let settings_id = if final_node {
                explicit_id.map_or_else(
                    || projection_settings_id("instance", &identity),
                    str::to_string,
                )
            } else {
                projection_settings_id("folder", &identity)
            };
            let index = document.instances.len();
            document.instances.push(SettingsBytecodeInstance::new(
                settings_id,
                name.clone(),
                expected_class.to_string(),
                Some(parent_index),
            ));
            children_by_parent.push(Vec::new());
            children_by_parent[parent_index].push(index);
            index
        };
        parent_index = index;
    }
    let target_index = if target.len() == 1 { 0 } else { parent_index };
    let instance = &mut document.instances[target_index];
    if target.len() == 1 && instance.class_name != class_name {
        bail!(
            "Projection cannot change service root '{}' from {} to {}",
            target[0],
            instance.class_name,
            class_name
        );
    }
    instance.properties.extend(properties.clone());
    instance.attributes.extend(attributes.clone());
    if let Some(tags) = tags {
        if tags.is_empty() {
            instance.properties.remove("Tags");
        } else {
            instance.properties.insert(
                "Tags".to_string(),
                Value::Array(tags.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    document.write_file(&settings_path)
}

fn ensure_stage_parent<'a>(stage: &Path, target: &'a [String]) -> Result<&'a [String]> {
    let parent = &target[..target.len() - 1];
    update_stage_instance(
        stage,
        parent,
        if parent.len() == 1 {
            parent[0].as_str()
        } else {
            "Folder"
        },
        None,
        &Map::new(),
        &Map::new(),
        None,
    )?;
    Ok(parent)
}

fn import_model_at_target(stage: &Path, target: &[String], model: &Path) -> Result<()> {
    if target.len() < 2 {
        bail!("Model target must include a service and parent path");
    }
    let parent = ensure_stage_parent(stage, target)?;
    let service = &target[0];
    let service_dir = stage.join(service);
    let settings_path = service_settings_path(&service_dir);
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    let parent_index = find_document_target(&document, parent)?;
    let outcome = crate::rbx::model::import_rbx_model_into_document(
        &mut document,
        &settings_path,
        service,
        model,
        Some(parent_index),
    )?;
    if outcome.root_settings_ids.len() == 1
        && let Some(instance) = document
            .instances
            .iter_mut()
            .find(|instance| instance.settings_id == outcome.root_settings_ids[0])
    {
        instance.name.clone_from(&target[target.len() - 1]);
    } else if outcome.root_settings_ids.len() > 1 {
        let mut settings_id = projection_settings_id("model-container", &target.join("."));
        let mut suffix = 2usize;
        while document
            .instances
            .iter()
            .any(|instance| instance.settings_id == settings_id)
        {
            settings_id = projection_settings_id(
                "model-container",
                &format!("{}:{suffix}", target.join(".")),
            );
            suffix += 1;
        }
        let container_index = document.instances.len();
        document.instances.push(SettingsBytecodeInstance::new(
            settings_id,
            target[target.len() - 1].clone(),
            "Folder".to_string(),
            Some(parent_index),
        ));
        let roots = outcome
            .root_settings_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        for instance in &mut document.instances[..container_index] {
            if roots.contains(&instance.settings_id) {
                instance.parent_index = Some(container_index);
            }
        }
    }
    let source_paths =
        crate::editor::paths::build_editor_source_paths_by_index(&document, service, &service_dir);
    let mut writes = Vec::with_capacity(outcome.source_by_settings_id.len() + 1);
    for (settings_id, bytes) in outcome.source_by_settings_id {
        let index = document
            .instances
            .iter()
            .position(|instance| instance.settings_id == settings_id)
            .with_context(|| format!("Imported script id {settings_id} disappeared"))?;
        let source_path = source_paths
            .get(index)
            .and_then(Option::as_ref)
            .with_context(|| format!("Imported script {settings_id} has no source path"))?;
        writes.push((source_path.clone(), bytes));
    }
    writes.push((settings_path.clone(), encode_settings_bytecode(&document)?));
    write_file_transaction(&writes)
}

fn merge_settings_mount(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    if target.len() < 2 {
        bail!("Settings mount target must include a service and parent path");
    }
    let parent = ensure_stage_parent(stage, target)?;
    let mounted = SettingsBytecode::read_file(source)?;
    let roots = mounted
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    if roots.len() > 1 {
        update_stage_instance(
            stage,
            target,
            "Folder",
            None,
            &Map::new(),
            &Map::new(),
            None,
        )?;
    }
    let service = &target[0];
    let settings_path = service_settings_path(&stage.join(service));
    let mut destination = SettingsBytecode::read_file(&settings_path)?;
    let parent_index = if roots.len() > 1 {
        find_document_target(&destination, target)?
    } else {
        find_document_target(&destination, parent)?
    };
    let mut remap = HashMap::new();
    let mut remapped_ids = HashMap::new();
    for (index, instance) in mounted.instances.iter().enumerate() {
        let next = destination.instances.len();
        let parent = if roots.contains(&index) {
            Some(parent_index)
        } else {
            instance
                .parent_index
                .and_then(|value| remap.get(&value).copied())
        };
        let mut instance = instance.clone();
        instance.parent_index = parent;
        if roots.len() == 1 && roots[0] == index {
            instance.name.clone_from(&target[target.len() - 1]);
        }
        let old_settings_id = instance.settings_id.clone();
        if destination
            .instances
            .iter()
            .any(|existing| existing.settings_id == instance.settings_id)
        {
            instance.settings_id =
                projection_settings_id("mounted", &format!("{}:{index}", target.join(".")));
        }
        remapped_ids.insert(old_settings_id, instance.settings_id.clone());
        destination.instances.push(instance);
        remap.insert(index, next);
    }
    remap_mounted_document_references(
        &mounted,
        &mut destination,
        &remap,
        &remapped_ids,
        target,
        roots.len() == 1,
        source,
    )?;
    destination.write_file(&settings_path)
}

fn merge_settings_document_at_target(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    let mounted = SettingsBytecode::read_file(source)?;
    let roots = mounted
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    let root_class = roots
        .first()
        .and_then(|index| mounted.instances.get(*index))
        .map_or("Folder", |instance| instance.class_name.as_str());
    update_stage_instance(
        stage,
        target,
        root_class,
        None,
        &Map::new(),
        &Map::new(),
        None,
    )?;
    let service = target
        .first()
        .context("Settings source target must include a service")?;
    let settings_path = service_settings_path(&stage.join(service));
    let mut destination = SettingsBytecode::read_file(&settings_path)?;
    let target_index = find_document_target(&destination, target)?;
    let single_root = roots.len() == 1;
    let mut index_map = HashMap::new();
    let mut id_map = HashMap::new();
    let mut used_ids = destination
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<BTreeSet<_>>();
    for (source_index, source_instance) in mounted.instances.iter().enumerate() {
        let destination_index = if roots.contains(&source_index) && single_root {
            target_index
        } else {
            let parent = source_instance
                .parent_index
                .and_then(|parent| index_map.get(&parent).copied())
                .unwrap_or(target_index);
            let matches = destination
                .instances
                .iter()
                .enumerate()
                .filter(|(_, instance)| {
                    instance.parent_index == Some(parent) && instance.name == source_instance.name
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [index] => *index,
                [] => {
                    let index = destination.instances.len();
                    destination.instances.push(SettingsBytecodeInstance::new(
                        String::new(),
                        source_instance.name.clone(),
                        source_instance.class_name.clone(),
                        Some(parent),
                    ));
                    index
                }
                _ => bail!(
                    "Settings source {} maps to duplicate target '{}'",
                    source.display(),
                    source_instance.name
                ),
            }
        };
        let mut output_id = source_instance.settings_id.clone();
        let current_id = destination.instances[destination_index].settings_id.clone();
        if output_id != current_id && used_ids.contains(&output_id) {
            output_id = projection_settings_id(
                "settings-source",
                &format!("{}:{source_index}", target.join(".")),
            );
        }
        used_ids.remove(&current_id);
        used_ids.insert(output_id.clone());
        let output = &mut destination.instances[destination_index];
        output.settings_id.clone_from(&output_id);
        output.class_name.clone_from(&source_instance.class_name);
        output.properties.extend(source_instance.properties.clone());
        output.attributes.extend(source_instance.attributes.clone());
        index_map.insert(source_index, destination_index);
        id_map.insert(source_instance.settings_id.clone(), output_id);
    }
    remap_mounted_document_references(
        &mounted,
        &mut destination,
        &index_map,
        &id_map,
        target,
        single_root,
        source,
    )?;
    destination.write_file(&settings_path)
}

fn remap_mounted_document_references(
    mounted: &SettingsBytecode,
    destination: &mut SettingsBytecode,
    indices: &HashMap<usize, usize>,
    ids: &HashMap<String, String>,
    target: &[String],
    single_root: bool,
    source: &Path,
) -> Result<()> {
    let mut mounted_paths = HashMap::<Vec<String>, Vec<Vec<usize>>>::new();
    for (segments, ordinals) in projection_instance_path_parts(mounted) {
        mounted_paths.entry(segments).or_default().push(ordinals);
    }
    let mut target_ordinals = active_target_ordinals(target);
    if target_ordinals.is_empty() {
        target_ordinals.resize(target.len(), 1);
    }
    let remapper = MountedReferenceRemapper {
        ids,
        indices,
        target,
        target_ordinals: &target_ordinals,
        internal_paths: &mounted_paths,
        path_root_components: usize::from(single_root),
    };
    for (source_index, source_instance) in mounted.instances.iter().enumerate() {
        let Some(destination_index) = indices.get(&source_index).copied() else {
            continue;
        };
        let output = &mut destination.instances[destination_index];
        remapper.remap_record(&mut output.properties)?;
        remapper.remap_record(&mut output.attributes)?;
        record_projection_identity(&output.settings_id, source, &source_instance.settings_id);
    }
    Ok(())
}

struct MountedReferenceRemapper<'a> {
    ids: &'a HashMap<String, String>,
    indices: &'a HashMap<usize, usize>,
    target: &'a [String],
    target_ordinals: &'a [usize],
    internal_paths: &'a HashMap<Vec<String>, Vec<Vec<usize>>>,
    path_root_components: usize,
}

impl MountedReferenceRemapper<'_> {
    fn remap_record(&self, record: &mut Map<String, Value>) -> Result<()> {
        for value in record.values_mut() {
            self.remap_value(value)?;
        }
        Ok(())
    }

    fn remap_value(&self, value: &mut Value) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    self.remap_value(value)?;
                }
            }
            Value::Object(object) => {
                if is_reference_object(object) {
                    let mut internal_signal = false;
                    let mut external_signal = false;
                    for key in ["settingsId", "instanceId"] {
                        if let Some(old) = object.get(key).and_then(Value::as_str) {
                            if self.ids.contains_key(old) {
                                internal_signal = true;
                            } else if !old.is_empty() {
                                external_signal = true;
                            }
                        }
                    }
                    let old_index = object
                        .get("instanceIndex")
                        .and_then(Value::as_u64)
                        .and_then(|index| usize::try_from(index).ok())
                        .and_then(|index| index.checked_sub(1));
                    if let Some(old) = old_index {
                        if self.indices.contains_key(&old) {
                            internal_signal = true;
                        } else {
                            external_signal = true;
                        }
                    }
                    let path_segments = object
                        .get("pathSegments")
                        .and_then(Value::as_array)
                        .and_then(|values| {
                            values
                                .iter()
                                .map(Value::as_str)
                                .map(|value| value.map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        });
                    let path_ordinals = object
                        .get("pathOrdinals")
                        .and_then(Value::as_array)
                        .and_then(|values| {
                            values
                                .iter()
                                .map(Value::as_u64)
                                .map(|value| {
                                    value
                                        .filter(|value| *value > 0)
                                        .and_then(|value| usize::try_from(value).ok())
                                })
                                .collect::<Option<Vec<_>>>()
                        });
                    let mut resolved_path_ordinals = None;
                    if let Some(path) = path_segments.as_ref() {
                        if let Some(candidates) = self.internal_paths.get(path) {
                            if let Some(ordinals) = path_ordinals.as_ref() {
                                if ordinals.len() != path.len() {
                                    bail!(
                                        "Mounted settings reference pathOrdinals must contain one value per path segment"
                                    );
                                }
                                if candidates.contains(ordinals) {
                                    internal_signal = true;
                                    resolved_path_ordinals = Some(ordinals.clone());
                                } else {
                                    external_signal = true;
                                }
                            } else if candidates.len() == 1 {
                                internal_signal = true;
                                resolved_path_ordinals = candidates.first().cloned();
                            } else {
                                bail!(
                                    "Mounted settings reference path '{}' is ambiguous; include pathOrdinals",
                                    path.join(".")
                                );
                            }
                        } else if !path.is_empty() {
                            external_signal = true;
                        }
                    }
                    if internal_signal && external_signal {
                        bail!("Mounted settings contain contradictory instance reference fields");
                    }
                    if internal_signal {
                        for key in ["settingsId", "instanceId"] {
                            if let Some(old) = object.get(key).and_then(Value::as_str)
                                && let Some(new) = self.ids.get(old)
                            {
                                object.insert(key.to_string(), Value::String(new.clone()));
                            }
                        }
                        if let Some(old) = object
                            .get("instanceIndex")
                            .and_then(Value::as_u64)
                            .and_then(|index| usize::try_from(index).ok())
                            .and_then(|index| index.checked_sub(1))
                            && let Some(new) = self.indices.get(&old)
                        {
                            object.insert(
                                "instanceIndex".to_string(),
                                Value::Number(serde_json::Number::from((new + 1) as u64)),
                            );
                        }
                        if let Some(paths) =
                            object.get_mut("pathSegments").and_then(Value::as_array_mut)
                        {
                            let tail = paths
                                .iter()
                                .skip(self.path_root_components)
                                .cloned()
                                .collect::<Vec<_>>();
                            *paths = self
                                .target
                                .iter()
                                .cloned()
                                .map(Value::String)
                                .chain(tail)
                                .collect();
                            if let Some(source_ordinals) = resolved_path_ordinals {
                                let tail = source_ordinals
                                    .into_iter()
                                    .skip(self.path_root_components)
                                    .map(|ordinal| {
                                        Value::Number(serde_json::Number::from(ordinal as u64))
                                    })
                                    .collect::<Vec<_>>();
                                object.insert(
                                    "pathOrdinals".to_string(),
                                    Value::Array(
                                        self.target_ordinals
                                            .iter()
                                            .map(|ordinal| {
                                                Value::Number(serde_json::Number::from(
                                                    *ordinal as u64,
                                                ))
                                            })
                                            .chain(tail)
                                            .collect(),
                                    ),
                                );
                            }
                        }
                    }
                }
                for value in object.values_mut() {
                    self.remap_value(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn stage_nested_project_at_target(
    nested: &LoadedProject,
    stage: &Path,
    target: &[String],
) -> Result<()> {
    let path = fs::canonicalize(&nested.path)
        .with_context(|| format!("Failed to resolve nested project {}", nested.path.display()))?;
    let inserted = NESTED_STAGE_STACK.with(|stack| stack.borrow_mut().insert(path.clone()));
    if !inserted {
        bail!("Nested project cycle includes {}", nested.path.display());
    }
    let result = (|| {
        validate_nested_project(nested)?;
        let root_class = nested
            .project
            .root
            .class_name
            .as_deref()
            .unwrap_or("Folder");
        let flattened = root_class == "DataModel";
        cache_script_naming(&target_fs_path(stage, target), &nested.project);
        cache_script_naming(
            &nested.root.join(&nested.project.source_root),
            &nested.project,
        );
        let staged_root_class = if flattened {
            target
                .first()
                .filter(|_| target.len() == 1)
                .map_or("Folder", String::as_str)
        } else {
            root_class
        };
        update_stage_instance(
            stage,
            target,
            staged_root_class,
            None,
            &Map::new(),
            &Map::new(),
            None,
        )?;
        let source = nested.root.join(&nested.project.source_root);
        if source.is_dir() {
            stage_source_directory(
                nested,
                stage,
                &source,
                &target_fs_path(stage, target),
                false,
                None,
            )?;
            let settings = service_settings_path(&source);
            if settings.is_file() {
                merge_settings_document_at_target(stage, target, &settings)?;
            }
        }
        if !flattened
            && (nested.project.root.class_name.is_some()
                || nested.project.root.id.is_some()
                || !nested.project.root.properties.is_empty()
                || !nested.project.root.attributes.is_empty()
                || nested.project.root.tags.is_some())
        {
            let class_name = nested
                .project
                .root
                .class_name
                .clone()
                .or(stage_target_class(stage, target)?)
                .unwrap_or_else(|| root_class.to_string());
            let properties =
                normalize_property_map(Some(&class_name), &nested.project.root.properties)?;
            let attributes = normalize_property_map(None, &nested.project.root.attributes)?;
            override_stage_identity(
                stage,
                target,
                nested.project.root.class_name.as_deref(),
                nested.project.root.id.as_deref(),
            )?;
            update_stage_instance(
                stage,
                target,
                &class_name,
                nested.project.root.id.as_deref(),
                &properties,
                &attributes,
                nested.project.root.tags.as_deref(),
            )?;
        }
        for (name, node) in &nested.project.tree {
            let mut child_target = target.to_vec();
            child_target.push(name.clone());
            stage_tree_node(nested, stage, &child_target, node)?;
        }
        for mount in &nested.project.mounts {
            let mut mounted = mount.clone();
            mounted.target = mount.target.with_prefix(target);
            stage_mount(nested, stage, &mounted)?;
        }
        for adapter in &nested.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let mut nested_adapter = adapter.clone();
            nested_adapter.target = adapter.target.with_prefix(target);
            stage_adapter(nested, stage, &nested_adapter)?;
        }
        Ok(())
    })();
    NESTED_STAGE_STACK.with(|stack| {
        stack.borrow_mut().remove(&path);
    });
    result
}

pub(super) fn find_document_target_optional(
    document: &SettingsBytecode,
    target: &[String],
) -> Result<Option<usize>> {
    find_document_target_optional_with_ordinals(document, target, &active_target_ordinals(target))
}

fn select_named_child(
    document: &SettingsBytecode,
    candidates: &[usize],
    name: &str,
    selected: usize,
) -> (Option<usize>, usize) {
    let mut found = None;
    let mut count = 0;
    for index in candidates.iter().copied() {
        if document.instances[index].name == name {
            count += 1;
            if count == selected {
                found = Some(index);
            }
        }
    }
    (found, count)
}

pub(super) fn find_document_target_optional_with_ordinals(
    document: &SettingsBytecode,
    target: &[String],
    ordinals: &[usize],
) -> Result<Option<usize>> {
    if !ordinals.is_empty() && ordinals.len() != target.len() {
        bail!("Projection target ordinals must contain one value per segment");
    }
    let roots = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    let children_by_parent = settings_children_by_parent(document);
    let mut parent = None;
    let mut found = None;
    for (depth, name) in target.iter().enumerate() {
        let candidates = parent.map_or(roots.as_slice(), |index| {
            children_by_parent.get(index).map_or(&[][..], Vec::as_slice)
        });
        let selected = ordinals.get(depth).copied().unwrap_or(1);
        let (matched, match_count) = select_named_child(document, candidates, name, selected);
        found = matched;
        if match_count == 0 {
            return Ok(None);
        }
        let ordinal = ordinals.get(depth).copied();
        if ordinal.is_none() && match_count > 1 {
            bail!(
                "Projection target '{}' is ambiguous at '{}'",
                target.join("."),
                name
            );
        }
        if selected > match_count {
            return Ok(None);
        }
        parent = found;
    }
    Ok(found)
}

pub(super) fn find_document_target(
    document: &SettingsBytecode,
    target: &[String],
) -> Result<usize> {
    let ordinals = active_target_ordinals(target);
    find_document_target_optional_with_ordinals(document, target, &ordinals)?.with_context(|| {
        if ordinals.is_empty() {
            format!("Projection target '{}' does not exist", target.join("."))
        } else {
            format!(
                "Projection target '{}' with ordinals {:?} does not exist",
                target.join("."),
                ordinals
            )
        }
    })
}

pub(super) fn clear_stage_target_children(stage: &Path, target: &[String]) -> Result<()> {
    let service = target
        .first()
        .context("Projection target must include a service")?;
    let service_dir = stage.join(service);
    let mut settings_path = service_settings_path(&service_dir);
    if !settings_path.is_file()
        || find_document_target_optional(&SettingsBytecode::read_file(&settings_path)?, target)?
            .is_none()
    {
        update_stage_instance(
            stage,
            target,
            "Folder",
            None,
            &Map::new(),
            &Map::new(),
            None,
        )?;
        settings_path = service_settings_path(&service_dir);
    }
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    let target_index = find_document_target(&document, target)?;
    let children = settings_children_by_parent(&document);
    let mut removed = BTreeSet::new();
    let mut pending = children.get(target_index).cloned().unwrap_or_default();
    while let Some(index) = pending.pop() {
        if removed.insert(index) {
            pending.extend(children.get(index).into_iter().flatten().copied());
        }
    }
    if removed.is_empty() {
        return Ok(());
    }
    let paths = projection_instance_path_parts(&document);
    let settings_ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    let parent_ids = document
        .instances
        .iter()
        .map(|instance| {
            instance
                .parent_index
                .and_then(|parent| settings_ids.get(parent))
                .cloned()
        })
        .collect::<Vec<_>>();
    for instance in &mut document.instances {
        stabilize_reference_indices_with_paths(&mut instance.properties, &paths, |index| {
            settings_ids.get(index).map(String::as_str)
        });
        stabilize_reference_indices_with_paths(&mut instance.attributes, &paths, |index| {
            settings_ids.get(index).map(String::as_str)
        });
    }
    let mut retained_parent_ids = Vec::new();
    let mut instances = Vec::with_capacity(document.instances.len() - removed.len());
    for (index, instance) in std::mem::take(&mut document.instances)
        .into_iter()
        .enumerate()
    {
        if !removed.contains(&index) {
            instances.push(instance);
            retained_parent_ids.push(parent_ids[index].clone());
        }
    }
    let indices_by_id = instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for (index, instance) in instances.iter_mut().enumerate() {
        instance.parent_index = retained_parent_ids[index]
            .as_deref()
            .and_then(|parent| indices_by_id.get(parent).copied());
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    document.instances = instances;
    document.write_file(&settings_path)
}

pub(super) fn adapter_target_script_path(
    loaded: &LoadedProject,
    stage: &Path,
    target: &[String],
) -> PathBuf {
    let extension = match loaded.project.script_extension {
        ScriptExtensionPolicy::Lua => "lua",
        ScriptExtensionPolicy::Preserve | ScriptExtensionPolicy::Luau => "luau",
    };
    let leaf = target.last().map_or("Adapter", String::as_str);
    let parent = target_fs_path(stage, &target[..target.len().saturating_sub(1)]);
    parent.join(format!(
        "{}{}.{}",
        leaf, loaded.project.export_naming.module_suffix, extension
    ))
}

pub(super) fn target_segments(target: &ProjectTarget) -> Result<Vec<String>> {
    validate_instance_target(target, "target")?;
    Ok(target.segments())
}

fn target_fs_path(root: &Path, target: &[String]) -> PathBuf {
    target
        .iter()
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

pub(super) fn normalize_sync_middleware(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.replace(['-', '_'], "").as_str() {
        "modulescript" | "module" => "modulescript".to_string(),
        "serverscript" | "server" | "script" => "serverscript".to_string(),
        "clientscript" | "client" | "localscript" => "clientscript".to_string(),
        "pluginscript" | "plugin" => "pluginscript".to_string(),
        "modeljson" => "model-json".to_string(),
        "nestedproject" | "project" => "nested-project".to_string(),
        _ => normalized,
    }
}

pub(super) fn validate_sync_middleware(value: &str) -> Result<()> {
    let normalized = normalize_sync_middleware(value);
    if !matches!(
        normalized.as_str(),
        "ignore" | "modulescript" | "serverscript" | "clientscript" | "pluginscript"
    ) && AdapterFormat::parse(&normalized).is_none()
    {
        bail!("Unsupported sync middleware '{value}'");
    }
    Ok(())
}

pub(super) fn sync_rule_matches(rule: &SyncRule, path: &Path) -> Result<bool> {
    if !compile_glob(&rule.pattern)?.is_match(path) {
        return Ok(false);
    }
    if let Some(exclude) = rule.exclude.as_deref()
        && compile_glob(exclude)?.is_match(path)
    {
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn owned_filter_candidate(
    instance: &SettingsBytecodeInstance,
    path: String,
) -> OwnedFilterCandidate {
    let tags = super::json_string_array(instance.properties.get("Tags"))
        .unwrap_or_default()
        .into_iter()
        .collect();
    OwnedFilterCandidate {
        id: instance.settings_id.clone(),
        path,
        name: instance.name.clone(),
        class: instance.class_name.clone(),
        tags,
        attributes: instance.attributes.keys().cloned().collect(),
        properties: instance.properties.keys().cloned().collect(),
    }
}

pub(super) fn filter_allows_candidate_pair(
    rules: &[FilterRule],
    direction: FilterDirection,
    current: &OwnedFilterCandidate,
    baseline: Option<&OwnedFilterCandidate>,
    scope: FilterScope<'_>,
) -> Result<bool> {
    if !filter_allows_scope(rules, direction, &current.borrowed(), scope)? {
        return Ok(false);
    }
    baseline
        .map(|baseline| filter_allows_scope(rules, direction, &baseline.borrowed(), scope))
        .transpose()
        .map(|allowed| allowed.unwrap_or(true))
}

pub(super) fn sync_rule_instance_name(rule: &SyncRule, path: &Path) -> Result<String> {
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("{} has no UTF-8 file name", path.display()))?;
    let name = if let Some(suffix) = rule.suffix.as_deref() {
        file_name.strip_suffix(suffix).with_context(|| {
            format!(
                "{} matches '{}' but doesn't end with configured suffix '{}'",
                path.display(),
                rule.pattern,
                suffix
            )
        })?
    } else {
        path.file_stem()
            .and_then(OsStr::to_str)
            .with_context(|| format!("{} has no UTF-8 file stem", path.display()))?
    };
    if name.is_empty() {
        bail!(
            "Sync rule '{}' produces an empty instance name",
            rule.pattern
        );
    }
    Ok(name.to_string())
}

pub(super) fn ignore_glob_pattern(raw: &str) -> Result<&str> {
    let pattern = raw
        .strip_prefix("\\!")
        .or_else(|| raw.strip_prefix('!'))
        .unwrap_or(raw);
    if pattern.is_empty() {
        bail!("Ignore glob cannot be empty");
    }
    Ok(pattern)
}

pub(super) fn path_is_ignored(loaded: &LoadedProject, path: &Path) -> Result<bool> {
    let relative = path.strip_prefix(&loaded.root).unwrap_or(path);
    if relative.components().any(|component| {
        matches!(component, Component::Normal(name) if name == ".git" || name == ".renium")
    }) {
        return Ok(true);
    }
    let mut ignored = false;
    for raw in &loaded.project.glob_ignore_paths {
        let escaped = raw.starts_with("\\!");
        let negated = raw.starts_with('!') && !escaped;
        if compile_glob(ignore_glob_pattern(raw)?)?.is_match(relative) {
            ignored = !negated;
        }
    }
    Ok(ignored)
}

pub(super) fn is_metadata_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".meta.json") || lower.ends_with(".meta.jsonc")
        })
}

fn metadata_sidecar_stem(name: &str) -> Option<&str> {
    [".meta.jsonc", ".meta.json"].iter().find_map(|suffix| {
        name.get(name.len().checked_sub(suffix.len())?..)
            .filter(|tail| tail.eq_ignore_ascii_case(suffix))
            .map(|_| &name[..name.len() - suffix.len()])
    })
}

pub(super) fn projection_settings_id(kind: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    format!("projection:{:x}", digest.finalize())
}

fn contains_metadata_sidecars(root: &Path) -> Result<bool> {
    if !root.is_dir() {
        return Ok(false);
    }
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file() && is_metadata_sidecar(entry.path()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn stage_source_directory(
    loaded: &LoadedProject,
    stage: &Path,
    source: &Path,
    destination: &Path,
    owns_source: bool,
    rule_prefix: Option<&Path>,
) -> Result<()> {
    if !source.is_dir() {
        bail!(
            "Projection source directory does not exist: {}",
            source.display()
        );
    }
    fs::create_dir_all(destination)?;
    let source = absolute_path(source);
    let source_key = projection_path_key(&source);
    let claimed_sources = projection_source_owner_paths(loaded)
        .into_iter()
        .map(|path| projection_path_key(&path))
        .collect::<Vec<_>>();
    let mut transformed = Vec::new();
    let mut sidecars = Vec::new();
    let entries = walkdir::WalkDir::new(&source)
        .into_iter()
        .filter_entry(|entry| {
            let entry_key = projection_path_key(entry.path());
            !claimed_sources
                .iter()
                .any(|claim| entry_key == *claim && (!owns_source || entry_key != source_key))
        });
    for entry in entries {
        let entry = entry?;
        let relative = entry.path().strip_prefix(&source)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_symlink() {
            bail!(
                "Projection sources cannot contain symlinks: {}",
                entry.path().display()
            );
        }
        if path_is_ignored(loaded, entry.path())? {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if is_metadata_sidecar(entry.path()) {
            sidecars.push((entry.path().to_path_buf(), target));
            continue;
        }
        let rule_relative =
            rule_prefix.map_or_else(|| relative.to_path_buf(), |prefix| prefix.join(relative));
        let mut rule = None;
        for candidate in &loaded.project.sync_rules {
            if sync_rule_matches(candidate, &rule_relative)? {
                rule = Some(candidate);
            }
        }
        if let Some(rule) = rule {
            transformed.push((entry.path().to_path_buf(), rule_relative, target, rule));
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), target)?;
    }
    for (source_file, relative, target, rule) in transformed {
        stage_sync_rule(loaded, stage, &source_file, &relative, &target, rule)?;
    }
    for (source_file, target) in sidecars {
        apply_metadata_sidecar(loaded, stage, &source_file, &target)?;
    }
    Ok(())
}

pub(super) fn projection_source_owner_paths(loaded: &LoadedProject) -> Vec<PathBuf> {
    let mut paths = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(_, node)| node.path.map(|path| absolute_path(&loaded.root.join(path))))
        .chain(
            loaded
                .project
                .mounts
                .iter()
                .map(|mount| absolute_path(&loaded.root.join(&mount.source))),
        )
        .collect::<Vec<_>>();
    paths.extend(loaded.project.adapters.iter().flat_map(|adapter| {
        std::iter::once(absolute_path(&loaded.root.join(&adapter.source))).chain(
            adapter
                .output
                .as_deref()
                .map(|output| absolute_path(&loaded.root.join(output))),
        )
    }));
    paths.sort_by_key(|path| projection_path_key(path));
    paths.dedup_by(|left, right| projection_path_key(left) == projection_path_key(right));
    paths
}

fn stage_sync_rule(
    loaded: &LoadedProject,
    stage: &Path,
    source: &Path,
    relative: &Path,
    target_file: &Path,
    rule: &SyncRule,
) -> Result<()> {
    let middleware = normalize_sync_middleware(&rule.middleware);
    if middleware == "ignore" {
        return Ok(());
    }
    let mut target = target_file
        .parent()
        .unwrap_or(stage)
        .strip_prefix(stage)
        .with_context(|| format!("{} is outside the projection stage", target_file.display()))?
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    let name = sync_rule_instance_name(rule, relative)?;
    if name != "init" {
        target.push(name);
    }
    if target.is_empty() {
        bail!(
            "Sync rule '{}' maps {} outside a Studio service",
            rule.pattern,
            source.display()
        );
    }
    let script_class_name = match middleware.as_str() {
        "modulescript" => Some("ModuleScript"),
        "serverscript" | "pluginscript" => Some("Script"),
        "clientscript" => Some("LocalScript"),
        _ => None,
    };
    PROJECTION_TRANSFORM_STACK.with(|stack| -> Result<()> {
        if let Some(transforms) = stack.borrow_mut().last_mut() {
            if let Some(existing) = transforms
                .iter()
                .find(|transform| transform.target == target)
            {
                bail!(
                    "Sync-rule sources '{}' and '{}' both map to '{}'",
                    existing.source.display(),
                    source.display(),
                    target.join(".")
                );
            }
            transforms.push(ProjectionTransform {
                target: target.clone(),
                source: source.to_path_buf(),
                script_class_name,
            });
        }
        Ok(())
    })?;
    match middleware.as_str() {
        "modulescript" | "serverscript" | "clientscript" | "pluginscript" => {
            let source_text = fs::read_to_string(source)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            let class_name = script_class_name.expect("script middleware has a class");
            let mut properties =
                Map::from_iter([("Source".to_string(), Value::String(source_text))]);
            if middleware == "pluginscript" {
                properties.insert(
                    "RunContext".to_string(),
                    json!({
                        "_type": "EnumItem",
                        "enumType": "Enum.RunContext",
                        "name": "Plugin",
                    }),
                );
            }
            update_stage_instance(
                stage,
                &target,
                class_name,
                None,
                &properties,
                &Map::new(),
                None,
            )
        }
        format => stage_adapter_format(
            loaded,
            stage,
            &target,
            source,
            AdapterFormat::parse(format)
                .with_context(|| format!("Unsupported sync middleware '{format}'"))?,
        ),
    }
}

fn apply_metadata_sidecar(
    loaded: &LoadedProject,
    stage: &Path,
    source: &Path,
    staged_path: &Path,
) -> Result<()> {
    let text =
        fs::read_to_string(source).with_context(|| format!("{} is not UTF-8", source.display()))?;
    let metadata: MetadataSidecar = serde_json::from_value(parse_jsonc_value(&text)?)
        .with_context(|| format!("Invalid metadata sidecar {}", source.display()))?;
    if metadata.schema_version.is_some_and(|version| version != 1) {
        bail!(
            "{} uses unsupported metadata schema version {}",
            source.display(),
            metadata.schema_version.unwrap_or_default()
        );
    }
    let file_name = staged_path
        .file_name()
        .and_then(OsStr::to_str)
        .context("Metadata sidecar has no UTF-8 file name")?;
    let stem = metadata_sidecar_stem(file_name).context("Invalid metadata sidecar name")?;
    let staged_relative = staged_path.strip_prefix(stage)?;
    let target = metadata_sidecar_target(loaded, staged_relative)?;
    let inferred_class = if stem == "init" {
        None
    } else {
        let synthetic = format!("{stem}.luau");
        let naming = project_script_naming(&loaded.project);
        let (class_name, _, _) =
            infer_source_script(&synthetic, &naming).unwrap_or(("Folder", None, None));
        Some(class_name)
    };
    if target.is_empty() {
        bail!("{} doesn't identify a Studio instance", source.display());
    }
    let existing_class = stage_target_class(stage, &target)?;
    let class_name = metadata
        .class_name
        .as_deref()
        .or(existing_class.as_deref())
        .or(inferred_class)
        .with_context(|| {
            format!(
                "{} has no matching instance; set $className to create one",
                source.display()
            )
        })?;
    let properties = normalize_property_map(Some(class_name), &metadata.properties)
        .with_context(|| format!("Invalid properties in {}", source.display()))?;
    let attributes = normalize_property_map(None, &metadata.attributes)
        .with_context(|| format!("Invalid attributes in {}", source.display()))?;
    override_stage_identity(
        stage,
        &target,
        metadata.class_name.as_deref(),
        metadata.id.as_deref(),
    )?;
    update_stage_instance(
        stage,
        &target,
        class_name,
        metadata.id.as_deref(),
        &properties,
        &attributes,
        metadata.tags.as_deref(),
    )
}

pub(super) fn metadata_sidecar_target(
    loaded: &LoadedProject,
    staged_relative: &Path,
) -> Result<Vec<String>> {
    let file_name = staged_relative
        .file_name()
        .and_then(OsStr::to_str)
        .context("Metadata sidecar has no UTF-8 file name")?;
    let stem = metadata_sidecar_stem(file_name).context("Invalid metadata sidecar name")?;
    let mut target = staged_relative
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    if stem != "init" {
        let synthetic = format!("{stem}.luau");
        let naming = project_script_naming(&loaded.project);
        let (_, leaf, _) =
            infer_source_script(&synthetic, &naming).unwrap_or(("Folder", None, None));
        target.push(leaf.unwrap_or_else(|| stem.to_string()));
    }
    if target.is_empty() {
        bail!(
            "{} doesn't identify a Studio instance",
            staged_relative.display()
        );
    }
    Ok(target)
}

fn metadata_sidecar_ignore_unknown_targets(loaded: &LoadedProject) -> Result<Vec<Vec<String>>> {
    let mut targets = BTreeSet::new();
    for source in metadata_sidecar_files(loaded)? {
        let text = fs::read_to_string(&source)
            .with_context(|| format!("{} is not UTF-8", source.display()))?;
        let metadata: MetadataSidecar = serde_json::from_value(parse_jsonc_value(&text)?)
            .with_context(|| format!("Invalid metadata sidecar {}", source.display()))?;
        if metadata.ignore_unknown_instances != Some(true) {
            continue;
        }
        let staged_relative = project_source_to_staged_relative(loaded, &source)?
            .with_context(|| format!("{} has no projected target", source.display()))?;
        targets.insert(metadata_sidecar_target(loaded, &staged_relative)?);
    }
    Ok(targets.into_iter().collect())
}

pub(super) fn nested_project_targets(
    loaded: &LoadedProject,
    prefix: &[String],
) -> Result<Vec<(Vec<String>, LoadedProject)>> {
    let mut projects = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        let Some(source) = node.path.as_deref() else {
            continue;
        };
        let source = loaded.root.join(source);
        if source.is_file() && is_nested_project_path(&source) {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target);
            projects.push((nested_target, load_nested_project(&source)?));
        }
    }
    for mount in &loaded.project.mounts {
        let source = loaded.root.join(&mount.source);
        if source.is_file() && is_nested_project_path(&source) {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target_segments(&mount.target)?);
            projects.push((nested_target, load_nested_project(&source)?));
        }
    }
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::FromProject {
            continue;
        }
        let source = loaded.root.join(&adapter.source);
        if source.is_file() && adapter_format(adapter)? == AdapterFormat::NestedProject {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target_segments(&adapter.target)?);
            projects.push((nested_target, load_nested_project(&source)?));
        }
    }
    Ok(projects)
}

pub fn compiled_files_to_studio_filters(loaded: &LoadedProject) -> Result<Vec<FilterRule>> {
    fn append(
        loaded: &LoadedProject,
        prefix: &[String],
        visiting: &mut BTreeSet<PathBuf>,
        output: &mut Vec<FilterRule>,
    ) -> Result<()> {
        let project_path = fs::canonicalize(&loaded.path)
            .with_context(|| format!("Failed to resolve {}", loaded.path.display()))?;
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        for rule in &loaded.project.filters {
            if !matches!(
                rule.direction,
                FilterDirection::Both | FilterDirection::FilesToStudio
            ) {
                continue;
            }
            if prefix.is_empty() {
                output.push(rule.clone());
                continue;
            }
            let escaped_prefix = escape_glob(&filter_path_segments(prefix));
            if let Some(glob) = rule.glob.as_deref() {
                let mut nested = rule.clone();
                nested.glob = Some(format!("{escaped_prefix}/{}", glob.trim_start_matches('/')));
                output.push(nested);
            } else {
                let mut root = rule.clone();
                root.glob = Some(escaped_prefix.clone());
                output.push(root);
                let mut descendants = rule.clone();
                descendants.glob = Some(format!("{escaped_prefix}/**"));
                output.push(descendants);
            }
        }
        for (target, nested) in nested_project_targets(loaded, prefix)? {
            append(&nested, &target, visiting, output)?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut output = Vec::new();
    append(loaded, &[], &mut BTreeSet::new(), &mut output)?;
    Ok(output)
}

pub fn compiled_files_to_studio_ignore_unknown_targets(
    loaded: &LoadedProject,
    reconciled_services: &BTreeSet<String>,
) -> Result<Vec<Vec<String>>> {
    fn append(
        loaded: &LoadedProject,
        prefix: &[String],
        reconciled_services: &BTreeSet<String>,
        visiting: &mut BTreeSet<PathBuf>,
        output: &mut BTreeSet<Vec<String>>,
    ) -> Result<()> {
        let project_path = fs::canonicalize(&loaded.path)
            .with_context(|| format!("Failed to resolve {}", loaded.path.display()))?;
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        if loaded
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
        {
            if prefix.is_empty() {
                for service in loaded.project.tree.keys().chain(reconciled_services.iter()) {
                    output.insert(vec![service.clone()]);
                }
            } else {
                output.insert(prefix.to_vec());
            }
        }
        for (target, node) in project_tree_nodes(&loaded.project.tree) {
            if node.ignore_unknown_instances.unwrap_or(false) {
                let mut nested_target = prefix.to_vec();
                nested_target.extend(target);
                output.insert(nested_target);
            }
        }
        for target in metadata_sidecar_ignore_unknown_targets(loaded)? {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target);
            output.insert(nested_target);
        }
        for (target, nested) in nested_project_targets(loaded, prefix)? {
            append(&nested, &target, reconciled_services, visiting, output)?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut output = BTreeSet::new();
    append(
        loaded,
        &[],
        reconciled_services,
        &mut BTreeSet::new(),
        &mut output,
    )?;
    Ok(output.into_iter().collect())
}

fn metadata_sidecar_files(loaded: &LoadedProject) -> Result<Vec<PathBuf>> {
    let mut roots = vec![loaded.root.join(&loaded.project.source_root)];
    roots.extend(
        project_tree_nodes(&loaded.project.tree)
            .into_iter()
            .filter_map(|(_, node)| node.path.map(|path| loaded.root.join(path))),
    );
    roots.extend(
        loaded
            .project
            .mounts
            .iter()
            .map(|mount| loaded.root.join(&mount.source)),
    );
    let mut files = BTreeSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry?;
            if entry.file_type().is_file() && is_metadata_sidecar(entry.path()) {
                files.insert(absolute_path(entry.path()));
            }
        }
    }
    Ok(files.into_iter().collect())
}

pub(super) fn projection_field_owners_with_root(
    loaded: &LoadedProject,
    include_root: bool,
) -> Result<Vec<ProjectionFieldOwner>> {
    fn collect(
        loaded: &LoadedProject,
        prefix: &[String],
        include_root: bool,
        visiting: &mut BTreeSet<PathBuf>,
        owners: &mut Vec<ProjectionFieldOwner>,
    ) -> Result<()> {
        let project_path =
            fs::canonicalize(&loaded.path).unwrap_or_else(|_| absolute_path(&loaded.path));
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        let root = &loaded.project.root;
        if include_root
            && root.class_name.as_deref() != Some("DataModel")
            && (root.class_name.is_some()
                || root.id.is_some()
                || !root.properties.is_empty()
                || !root.attributes.is_empty()
                || root.tags.is_some())
        {
            owners.push(ProjectionFieldOwner {
                target: prefix.to_vec(),
                source: format!("{} root", loaded.path.display()),
                class_name: root.class_name.is_some(),
                settings_id: root.id.is_some(),
                properties: root.properties.keys().cloned().collect(),
                attributes: root.attributes.keys().cloned().collect(),
                tags: root.tags.is_some(),
            });
        }
        let tree_nodes = project_tree_nodes(&loaded.project.tree);
        for (target, node) in &tree_nodes {
            let mut prefixed = prefix.to_vec();
            prefixed.extend(target.iter().cloned());
            owners.push(ProjectionFieldOwner {
                target: prefixed,
                source: format!("{} tree '{}'", loaded.path.display(), target.join(".")),
                class_name: node.class_name.is_some(),
                settings_id: node.id.is_some(),
                properties: node.properties.keys().cloned().collect(),
                attributes: node.attributes.keys().cloned().collect(),
                tags: node.tags.is_some(),
            });
        }
        for mut owner in metadata_sidecar_field_owners(loaded)? {
            let mut prefixed = prefix.to_vec();
            prefixed.append(&mut owner.target);
            owner.target = prefixed;
            owners.push(owner);
        }
        let mut nested = Vec::new();
        for (target, node) in tree_nodes {
            if let Some(source) = node.path {
                let source = loaded.root.join(source);
                if source.is_file() && is_nested_project_path(&source) {
                    nested.push((target, source));
                }
            }
        }
        for mount in &loaded.project.mounts {
            let source = loaded.root.join(&mount.source);
            if source.is_file() && is_nested_project_path(&source) {
                nested.push((target_segments(&mount.target)?, source));
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let source = loaded.root.join(&adapter.source);
            if source.is_file() && adapter_format(adapter)? == AdapterFormat::NestedProject {
                nested.push((target_segments(&adapter.target)?, source));
            }
        }
        for (target, source) in nested {
            let nested_project = load_nested_project(&source)?;
            let mut nested_prefix = prefix.to_vec();
            nested_prefix.extend(target);
            collect(&nested_project, &nested_prefix, true, visiting, owners)?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut owners = Vec::new();
    collect(loaded, &[], include_root, &mut BTreeSet::new(), &mut owners)?;
    owners.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then(left.source.cmp(&right.source))
    });
    Ok(owners)
}

fn projection_field_owners(loaded: &LoadedProject) -> Result<Vec<ProjectionFieldOwner>> {
    projection_field_owners_with_root(loaded, false)
}

fn metadata_sidecar_field_owners(loaded: &LoadedProject) -> Result<Vec<ProjectionFieldOwner>> {
    let mut owners = Vec::new();
    for source in metadata_sidecar_files(loaded)? {
        let text = fs::read_to_string(&source)
            .with_context(|| format!("{} is not UTF-8", source.display()))?;
        let metadata: MetadataSidecar = serde_json::from_value(parse_jsonc_value(&text)?)
            .with_context(|| format!("Invalid metadata sidecar {}", source.display()))?;
        let staged_relative = project_source_to_staged_relative(loaded, &source)?
            .with_context(|| format!("{} has no projected target", source.display()))?;
        owners.push(ProjectionFieldOwner {
            target: metadata_sidecar_target(loaded, &staged_relative)?,
            source: source.display().to_string(),
            class_name: metadata.class_name.is_some(),
            settings_id: metadata.id.is_some(),
            properties: metadata.properties.keys().cloned().collect(),
            attributes: metadata.attributes.keys().cloned().collect(),
            tags: metadata.tags.is_some(),
        });
    }
    Ok(owners)
}

pub fn project_target_is_declarative(loaded: &LoadedProject, target: &[String]) -> Result<bool> {
    Ok(projection_field_owners(loaded)?
        .iter()
        .any(|owner| owner.target == target))
}

pub fn project_structural_store(loaded: &LoadedProject, target: &[String]) -> Result<PathBuf> {
    let relative = target.iter().collect::<PathBuf>();
    let resolution = resolve_project_write_path(loaded, &relative)?;
    if resolution.source_root.is_file() {
        if matches!(
            resolution.source_root.extension().and_then(OsStr::to_str),
            Some("renium")
        ) {
            return Ok(resolution.source_root);
        }
        bail!(
            "Projected path '{}' is owned by file {}; edit that file directly",
            target.join("."),
            resolution.source_root.display()
        );
    }
    let settings = service_settings_path(&resolution.source_root);
    if !settings.is_file() {
        bail!(
            "Projected path '{}' is owned by '{}' but it has no Renium settings store",
            target.join("."),
            resolution.source_root.display()
        );
    }
    Ok(settings)
}

fn canonical_owned_value(value: Option<&Value>, document: &SettingsBytecode) -> Option<Value> {
    let mut record = Map::new();
    if let Some(value) = value {
        record.insert("value".to_string(), value.clone());
    }
    stabilize_reference_indices(&mut record, &document.instances);
    record.remove("value")
}

pub(super) fn validate_projection_field_ownership(
    loaded: &LoadedProject,
    documents: &HashMap<String, SettingsBytecode>,
    baseline_documents: &HashMap<String, SettingsBytecode>,
) -> Result<()> {
    for owner in projection_field_owners(loaded)? {
        let service = owner
            .target
            .first()
            .with_context(|| format!("{} has an empty projected target", owner.source))?;
        let imported = documents
            .get(service)
            .with_context(|| format!("Studio removed declared service '{service}'"))?;
        let baseline = baseline_documents
            .get(service)
            .with_context(|| format!("Baseline is missing declared service '{service}'"))?;
        let baseline_index = find_document_target(baseline, &owner.target).with_context(|| {
            format!(
                "{} does not resolve to '{}'",
                owner.source,
                owner.target.join(".")
            )
        })?;
        let imported_index = find_document_target(imported, &owner.target).map_err(|_| {
            let baseline_id = &baseline.instances[baseline_index].settings_id;
            if let Some(instance) = imported
                .instances
                .iter()
                .find(|instance| &instance.settings_id == baseline_id)
            {
                anyhow::anyhow!(
                    "Studio renamed config-owned instance '{}' to '{}'; rename it in {} instead",
                    owner.target.join("."),
                    instance.name,
                    owner.source
                )
            } else {
                anyhow::anyhow!(
                    "Studio removed config-owned instance '{}'; edit {} instead",
                    owner.target.join("."),
                    owner.source
                )
            }
        })?;
        let original = &baseline.instances[baseline_index];
        let changed = &imported.instances[imported_index];
        if owner.class_name && changed.class_name != original.class_name {
            bail!(
                "Studio changed config-owned ClassName on '{}'; edit {} instead",
                owner.target.join("."),
                owner.source
            );
        }
        if owner.settings_id && changed.settings_id != original.settings_id {
            bail!(
                "Studio changed config-owned id on '{}'; edit {} instead",
                owner.target.join("."),
                owner.source
            );
        }
        for property in &owner.properties {
            if canonical_owned_value(changed.properties.get(property), imported)
                != canonical_owned_value(original.properties.get(property), baseline)
            {
                bail!(
                    "Studio changed config-owned property '{}.{}'; edit {} instead",
                    owner.target.join("."),
                    property,
                    owner.source
                );
            }
        }
        for attribute in &owner.attributes {
            if canonical_owned_value(changed.attributes.get(attribute), imported)
                != canonical_owned_value(original.attributes.get(attribute), baseline)
            {
                bail!(
                    "Studio changed config-owned attribute '{}.{}'; edit {} instead",
                    owner.target.join("."),
                    attribute,
                    owner.source
                );
            }
        }
        if owner.tags
            && canonical_owned_value(changed.properties.get("Tags"), imported)
                != canonical_owned_value(original.properties.get("Tags"), baseline)
        {
            bail!(
                "Studio changed config-owned tags on '{}'; edit {} instead",
                owner.target.join("."),
                owner.source
            );
        }
    }
    Ok(())
}

pub(super) fn normalize_property_map(
    class_name: Option<&str>,
    values: &Map<String, Value>,
) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|(name, value)| {
            if contains_reference_value(value) {
                if class_name.is_none() {
                    bail!("Attributes cannot contain instance references");
                }
                return Ok((name.clone(), value.clone()));
            }
            Ok((
                name.clone(),
                crate::rbx::encode::normalize_project_typed_value(class_name, Some(name), value)
                    .with_context(|| format!("Invalid value for '{name}'"))?,
            ))
        })
        .collect()
}

fn stage_target_class(stage: &Path, target: &[String]) -> Result<Option<String>> {
    let Some(service) = target.first() else {
        return Ok(None);
    };
    let service_dir = stage.join(service);
    if !service_dir.is_dir() {
        return Ok(None);
    }
    let settings_path = service_settings_path(&service_dir);
    let document = if settings_path.is_file() {
        SettingsBytecode::read_file(&settings_path)?
    } else {
        crate::rbx::model::source_only_settings_document(&service_dir, service)?
    };
    Ok(find_document_target_optional(&document, target)?
        .map(|index| document.instances[index].class_name.clone()))
}

fn override_stage_identity(
    stage: &Path,
    target: &[String],
    class_name: Option<&str>,
    settings_id: Option<&str>,
) -> Result<()> {
    if class_name.is_none() && settings_id.is_none() {
        return Ok(());
    }
    let service = target
        .first()
        .context("Projection target must include a service")?;
    let service_dir = stage.join(service);
    if !service_dir.is_dir() {
        return Ok(());
    }
    let settings_path = service_settings_path(&service_dir);
    let mut document = if settings_path.is_file() {
        SettingsBytecode::read_file(&settings_path)?
    } else {
        crate::rbx::model::source_only_settings_document(&service_dir, service)?
    };
    let Some(index) = find_document_target_optional(&document, target)? else {
        return Ok(());
    };
    if let Some(settings_id) = settings_id {
        if document
            .instances
            .iter()
            .enumerate()
            .any(|(other, instance)| other != index && instance.settings_id == settings_id)
        {
            bail!("Projection settings id '{settings_id}' is used more than once");
        }
        document.instances[index].settings_id = settings_id.to_string();
    }
    if let Some(class_name) = class_name {
        document.instances[index].class_name = class_name.to_string();
    }
    document.write_file(&settings_path)
}

pub(super) fn refresh_stage_settings(stage: &Path) -> Result<()> {
    let mut service_dirs = Vec::new();
    for entry in fs::read_dir(stage)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            service_dirs.push(entry.path());
        }
    }
    service_dirs.sort();
    for service_dir in service_dirs {
        refresh_stage_service_settings(&service_dir)?;
    }
    Ok(())
}

fn refresh_stage_service_settings(service_dir: &Path) -> Result<()> {
    let service = service_dir
        .file_name()
        .and_then(OsStr::to_str)
        .context("Projection service name is not UTF-8")?;
    let generated = crate::rbx::model::source_only_settings_document(service_dir, service)?;
    let settings_path = service_settings_path(service_dir);
    if !settings_path.is_file() {
        return generated.write_file(&settings_path);
    }
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    merge_source_only_document(&mut document, &generated)?;
    document.write_file(&settings_path)
}

fn merge_source_only_document(
    destination: &mut SettingsBytecode,
    generated: &SettingsBytecode,
) -> Result<()> {
    if destination.instances.is_empty() {
        destination.instances.clone_from(&generated.instances);
        return Ok(());
    }
    let children = settings_children_by_parent(destination);
    let mut by_parent_stem = HashMap::new();
    for (index, instance) in destination.instances.iter().enumerate() {
        if instance.parent_index.is_none() {
            by_parent_stem.insert(
                (
                    None,
                    normalized_child_stem_key(&sanitize_name(&instance.name)),
                ),
                index,
            );
        }
    }
    for (parent, child_indices) in children.iter().enumerate() {
        for (index, stem, _) in editor_child_stems(destination, child_indices) {
            by_parent_stem.insert((Some(parent), normalized_child_stem_key(&stem)), index);
        }
    }
    let mut remap = BTreeMap::new();
    for (index, instance) in generated.instances.iter().enumerate() {
        let parent = instance
            .parent_index
            .and_then(|parent| remap.get(&parent).copied());
        let stem = if parent.is_none() {
            sanitize_name(&instance.name)
        } else {
            instance.name.clone()
        };
        let key = (parent, normalized_child_stem_key(&stem));
        let mapped = if let Some(mapped) = by_parent_stem.get(&key).copied() {
            mapped
        } else {
            let mapped = destination.instances.len();
            let mut appended = instance.clone();
            appended.parent_index = parent;
            if destination
                .instances
                .iter()
                .any(|existing| existing.settings_id == appended.settings_id)
            {
                appended.settings_id =
                    projection_settings_id("source", &format!("{}:{index}", appended.settings_id));
            }
            destination.instances.push(appended);
            by_parent_stem.insert(key, mapped);
            mapped
        };
        remap.insert(index, mapped);
    }
    Ok(())
}

pub(super) fn file_target_destination(
    loaded: &LoadedProject,
    source: &Path,
    target: &Path,
) -> PathBuf {
    if target.extension().is_some() {
        target.to_path_buf()
    } else {
        let source_name = source.file_name().and_then(OsStr::to_str).unwrap_or("");
        let naming = project_script_naming(&loaded.project);
        if let Some((_, stem, _)) = infer_source_script(source_name, &naming) {
            let prefix_len = stem.as_deref().map_or(4, str::len);
            let suffix = &source_name[prefix_len..];
            target.with_file_name(format!(
                "{}{suffix}",
                target.file_name().and_then(OsStr::to_str).unwrap_or("")
            ))
        } else if let Some(extension) = source.extension().and_then(OsStr::to_str) {
            target.with_extension(extension)
        } else {
            target.to_path_buf()
        }
    }
}

fn copy_file_to_target(loaded: &LoadedProject, source: &Path, target: &Path) -> Result<()> {
    let destination = file_target_destination(loaded, source, target);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(())
}
