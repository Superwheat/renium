use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Number, Value, json};
use walkdir::WalkDir;

use crate::app::output::{global_yes, print_json_output};
use crate::bytecode::edit::{
    bytecode_add_instance, bytecode_clone_instance, bytecode_desync_package_link,
    bytecode_remove_instance,
};
use crate::bytecode::parse_bracket_path_segments;
use crate::bytecode::{apply_file_mutations, bytecode_set_property};
use crate::cli::{
    BridgeConnectionArgs, BytecodeAddInstanceArgs, BytecodeCloneInstanceArgs,
    BytecodeDesyncPackageLinkArgs, BytecodeExportModelArgs, BytecodeFileArgs,
    BytecodeImportModelArgs, BytecodeInstanceSelectorArgs, BytecodeParentArgs,
    BytecodeRemoveInstanceArgs, BytecodeSetPropertyArgs, CloneInstanceCommandArgs,
    CreateInstanceArgs, DesyncPackageLinkCommandArgs, ExportModelCommandArgs,
    ImportModelCommandArgs, ImportPathArgs, MoveInstanceArgs, ProjectInstanceArgs,
    ProjectSourceArgs, PushEditorChangesArgs, RemoveInstanceCommandArgs, RenameInstanceArgs,
    SyncbackArgs,
};
use crate::editor::paths::{build_editor_instance_paths, infer_source_script};
use crate::editor::sync::push_editor_changes;
use crate::project::config;
use crate::project::layout::configured_project_layout;
use crate::project::package_links::build_loaded_project_link_enforcement;
use crate::project::sourcemap::{path_to_sourcemap_relative, write_project_sourcemap_with_updates};
use crate::project::structural::move_instance_between_service_stores;
use crate::rbx::model::{bytecode_export_model, bytecode_import_model};
use crate::settings::bytecode::{SettingsBytecode, instance_settings_id};
use crate::snapshot::export::ExportProjectStage;
use crate::snapshot::import::{
    build_service_state_from_instances, import_service_state_with_sourcemap, load_service_state,
    parse_services,
};
use crate::snapshot::refs::{
    merge_syncback_instance_fields, reindex_snapshot_references,
    settings_document_as_snapshot_instances, snapshot_service_exists,
    stabilize_snapshot_references, syncback_filter_allows_instance,
};
use crate::snapshot::types::{ServiceState, SnapshotInstance};
use crate::system::files::{
    absolutize_under, canonical_path, path_key, service_settings_path, strip_extended_prefix,
};
fn structural_source_root(
    project: Option<&Path>,
    project_root: &Path,
    src_root: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(project) = project {
        let loaded = config::load_project(Some(project), None)?;
        return Ok(loaded
            .root
            .join(src_root.unwrap_or(&loaded.project.source_root)));
    }
    let source_root = src_root.unwrap_or_else(|| Path::new("src"));
    let (project_root, source_root) = configured_project_layout(project_root, source_root)?;
    Ok(absolutize_under(&project_root, &source_root))
}

fn structural_project(
    project: Option<&Path>,
    project_root: &Path,
    src_root: Option<&Path>,
) -> Result<Option<config::LoadedProject>> {
    if src_root.is_some() {
        return Ok(None);
    }
    load_structural_project(project, project_root).map(Some)
}

pub(crate) fn load_structural_project(
    project: Option<&Path>,
    project_root: &Path,
) -> Result<config::LoadedProject> {
    if let Some(project) = project {
        return config::load_project(Some(project), None);
    }
    if let Some(loaded) = config::try_load_project(None, Some(project_root))? {
        return Ok(loaded);
    }
    let (root, source_root) = configured_project_layout(project_root, Path::new("src"))?;
    let project = config::ReniumProject {
        source_root,
        ..config::ReniumProject::default()
    };
    Ok(config::LoadedProject {
        path: root.join(config::PROJECT_FILE_NAME),
        root,
        project,
    })
}

fn projected_structural_target(
    stage: &config::ProjectionStage,
    service: &str,
    settings_id: Option<&str>,
) -> Result<(Vec<String>, Vec<usize>)> {
    let settings = service_settings_path(&stage.root().join(service));
    if !settings.is_file() {
        bail!("Projected service '{service}' has no Renium store");
    }
    if settings_id.is_none() {
        return Ok((vec![service.to_string()], vec![1]));
    }
    let document = SettingsBytecode::read_file(&settings)?;
    let settings_id = settings_id.unwrap_or_default();
    let index = document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
        .with_context(|| {
            format!("Projected service '{service}' has no instance id '{settings_id}'")
        })?;
    let paths = build_editor_instance_paths(&document, service);
    let path = paths
        .get(index)
        .and_then(Option::as_ref)
        .with_context(|| format!("Cannot resolve the projected path for '{settings_id}'"))?;
    Ok((path.path_segments.clone(), path.path_ordinals.clone()))
}

fn projected_structural_store(
    loaded: &config::LoadedProject,
    stage: &config::ProjectionStage,
    service: &str,
    settings_id: Option<&str>,
    reject_declarative_target: bool,
    override_packages: bool,
) -> Result<(PathBuf, Vec<String>, Option<String>)> {
    let (target, target_ordinals) = projected_structural_target(stage, service, settings_id)?;
    build_loaded_project_link_enforcement(loaded, override_packages)?
        .reject_read_only_package_path(service, &target, &target_ordinals)?;
    if settings_id.is_none() && target_ordinals.iter().any(|ordinal| *ordinal > 1) {
        bail!(
            "Projected path '{}' selects a duplicate sibling that cannot map to one filesystem owner",
            target.join(".")
        );
    }
    if stage.target_is_transformed(&target) {
        bail!(
            "Projected path '{}' is generated by a sync rule; edit its source file instead",
            target.join(".")
        );
    }
    if reject_declarative_target && config::project_target_is_declarative(loaded, &target)? {
        bail!(
            "Projected path '{}' is named by project configuration; edit the project or metadata sidecar instead",
            target.join(".")
        );
    }
    let store = config::project_structural_store(loaded, &target)?;
    let canonical_id = settings_id
        .map(|settings_id| {
            if let Some((source, canonical_id)) = stage.canonical_identity(settings_id) {
                if path_key(source) != path_key(&store) {
                    bail!(
                        "Projected instance '{}' resolves to a different canonical store",
                        target.join(".")
                    );
                }
                Ok(canonical_id.to_string())
            } else {
                let document = SettingsBytecode::read_file(&store)?;
                if document
                    .instances
                    .iter()
                    .any(|instance| instance.settings_id == settings_id)
                {
                    Ok(settings_id.to_string())
                } else {
                    bail!(
                        "Projected instance '{}' exists only in project configuration and is not structurally writable",
                        target.join(".")
                    )
                }
            }
        })
        .transpose()?;
    Ok((canonical_path(&store)?, target, canonical_id))
}

fn projected_instance_store(
    project: Option<&Path>,
    target: &ProjectInstanceArgs,
    override_packages: bool,
) -> Result<(PathBuf, Option<String>)> {
    let loaded = load_structural_project(project, &target.project_root)?;
    let stage = config::stage_project(&loaded)?;
    let (settings_file, _, settings_id) = projected_structural_store(
        &loaded,
        &stage,
        &target.service,
        Some(&target.settings_id),
        true,
        override_packages,
    )?;
    Ok((settings_file, settings_id))
}

fn metadata_property_change(
    settings_file: PathBuf,
    settings_id: Option<String>,
    property: &str,
    value: Option<String>,
) -> BytecodeSetPropertyArgs {
    BytecodeSetPropertyArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
        property: property.to_string(),
        value_json: None,
        value_str: value,
        value_num: None,
        value_bool: None,
        value_null: false,
        scope: "metadata".to_string(),
        pretty: false,
    }
}

pub(crate) fn create_instance_command(
    args: CreateInstanceArgs,
    project: Option<&Path>,
) -> Result<()> {
    let loaded_project = structural_project(project, &args.project_root, args.src_root.as_deref())?;
    let settings_file = if let Some(loaded) = loaded_project.as_ref() {
        let stage = config::stage_project(loaded)?;
        let (settings_file, _, parent_settings_id) = projected_structural_store(
            loaded,
            &stage,
            &args.service,
            args.parent_settings_id.as_deref(),
            false,
            args.override_packages,
        )?;
        return bytecode_add_instance(BytecodeAddInstanceArgs {
            input: BytecodeFileArgs::settings_file(settings_file),
            name: args.name,
            class_name: args.class_name,
            settings_id: None,
            parent: BytecodeParentArgs {
                parent_settings_id,
                ..Default::default()
            },
            properties: args.properties,
            attributes: args.attributes,
            pretty: true,
        });
    } else {
        let src_root =
            structural_source_root(project, &args.project_root, args.src_root.as_deref())?;
        service_settings_path(&src_root.join(&args.service))
    };
    if !settings_file.is_file() {
        bail!(
            "Service '{}' has no Renium store at {}",
            args.service,
            settings_file.display()
        );
    }
    bytecode_add_instance(BytecodeAddInstanceArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        name: args.name,
        class_name: args.class_name,
        settings_id: None,
        parent: BytecodeParentArgs {
            parent_settings_id: args.parent_settings_id,
            ..Default::default()
        },
        properties: args.properties,
        attributes: args.attributes,
        pretty: true,
    })
}

pub(crate) fn clone_instance_command(
    args: CloneInstanceCommandArgs,
    project: Option<&Path>,
) -> Result<()> {
    let target = args.target;
    let loaded = load_structural_project(project, &target.project_root)?;
    let stage = config::stage_project(&loaded)?;
    let (source_file, source_target, source_id) = projected_structural_store(
        &loaded,
        &stage,
        &target.service,
        Some(&target.settings_id),
        true,
        args.override_packages,
    )?;
    let (parent_file, parent_target, parent_id) = projected_structural_store(
        &loaded,
        &stage,
        &target.service,
        Some(&args.parent_settings_id),
        false,
        args.override_packages,
    )?;
    if source_file != parent_file {
        bail!(
            "Copying '{}' to '{}' crosses projected owners",
            source_target.join("."),
            parent_target.join(".")
        );
    }
    bytecode_clone_instance(BytecodeCloneInstanceArgs {
        input: BytecodeFileArgs::settings_file(source_file),
        service: target.service,
        selector: BytecodeInstanceSelectorArgs::by_settings_id(source_id),
        parent_index: None,
        parent_settings_id: parent_id,
        parent_name: None,
        parent_class_name: None,
        pretty: true,
    })
}

pub(crate) fn move_instance_command(args: MoveInstanceArgs, project: Option<&Path>) -> Result<()> {
    let target = args.target;
    let target_service = args
        .target_service
        .clone()
        .unwrap_or_else(|| target.service.clone());
    let loaded_project =
        structural_project(project, &target.project_root, args.src_root.as_deref())?;
    let (source_file, target_file) = if let Some(loaded) = loaded_project.as_ref() {
        let stage = config::stage_project(loaded)?;
        let (source_file, _, source_id) = projected_structural_store(
            loaded,
            &stage,
            &target.service,
            Some(&target.settings_id),
            true,
            args.override_packages,
        )?;
        let (target_file, _, parent_id) = projected_structural_store(
            loaded,
            &stage,
            &target_service,
            Some(&args.parent_settings_id),
            false,
            args.override_packages,
        )?;
        if source_file != target_file {
            return move_instance_between_service_stores(
                &source_file,
                &target.service,
                source_id
                    .as_deref()
                    .context("Source instance has no canonical id")?,
                &target_file,
                &target_service,
                parent_id
                    .as_deref()
                    .context("Target parent has no canonical id")?,
            );
        }
        return bytecode_set_property(metadata_property_change(
            source_file,
            source_id,
            "Parent",
            parent_id,
        ));
    } else {
        let src_root =
            structural_source_root(project, &target.project_root, args.src_root.as_deref())?;
        (
            service_settings_path(&src_root.join(&target.service)),
            service_settings_path(&src_root.join(&target_service)),
        )
    };
    if !source_file.is_file() {
        bail!("Source service '{}' has no Renium store", target.service);
    }
    if target_service == target.service {
        return bytecode_set_property(metadata_property_change(
            source_file,
            Some(target.settings_id),
            "Parent",
            Some(args.parent_settings_id),
        ));
    }
    if !target_file.is_file() {
        bail!("Target service '{target_service}' has no Renium store");
    }
    move_instance_between_service_stores(
        &source_file,
        &target.service,
        &target.settings_id,
        &target_file,
        &target_service,
        &args.parent_settings_id,
    )
}

pub(crate) fn rename_instance_command(
    args: RenameInstanceArgs,
    project: Option<&Path>,
) -> Result<()> {
    let target = args.target;
    let loaded_project =
        structural_project(project, &target.project_root, args.src_root.as_deref())?;
    let settings_file = if let Some(loaded) = loaded_project.as_ref() {
        let stage = config::stage_project(loaded)?;
        let (settings_file, _, settings_id) = projected_structural_store(
            loaded,
            &stage,
            &target.service,
            Some(&target.settings_id),
            true,
            args.override_packages,
        )?;
        return bytecode_set_property(metadata_property_change(
            settings_file,
            settings_id,
            "Name",
            Some(args.name),
        ));
    } else {
        let src_root =
            structural_source_root(project, &target.project_root, args.src_root.as_deref())?;
        service_settings_path(&src_root.join(&target.service))
    };
    if !settings_file.is_file() {
        bail!("Service '{}' has no Renium store", target.service);
    }
    bytecode_set_property(metadata_property_change(
        settings_file,
        Some(target.settings_id),
        "Name",
        Some(args.name),
    ))
}

pub(crate) fn remove_instance_command(
    args: RemoveInstanceCommandArgs,
    project: Option<&Path>,
) -> Result<()> {
    let target = args.target;
    let (settings_file, settings_id) =
        projected_instance_store(project, &target, args.override_packages)?;
    bytecode_remove_instance(BytecodeRemoveInstanceArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
        no_recursive: args.no_recursive,
        pretty: true,
    })
}

pub(crate) fn desync_package_link_command(
    args: DesyncPackageLinkCommandArgs,
    project: Option<&Path>,
) -> Result<()> {
    let target = args.target;
    let (settings_file, settings_id) =
        projected_instance_store(project, &target, args.override_packages)?;
    bytecode_desync_package_link(BytecodeDesyncPackageLinkArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        service: target.service,
        selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
        pretty: true,
    })
}

pub(crate) fn import_model_command(
    args: ImportModelCommandArgs,
    project: Option<&Path>,
) -> Result<()> {
    let loaded = load_structural_project(project, &args.project_root)?;
    let stage = config::stage_project(&loaded)?;
    let (settings_file, _, parent_settings_id) = projected_structural_store(
        &loaded,
        &stage,
        &args.service,
        Some(&args.parent_settings_id),
        false,
        args.override_packages,
    )?;
    bytecode_import_model(BytecodeImportModelArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        service: args.service,
        model: args.model,
        parent: BytecodeParentArgs {
            parent_settings_id,
            ..Default::default()
        },
        pretty: true,
    })
}

pub(crate) fn export_model_command(
    args: ExportModelCommandArgs,
    project: Option<&Path>,
) -> Result<()> {
    let target = args.target;
    let loaded = load_structural_project(project, &target.project_root)?;
    let stage = config::stage_project(&loaded)?;
    let settings_file = service_settings_path(&stage.root().join(&target.service));
    if !settings_file.is_file() {
        bail!("Projected service '{}' has no Renium store", target.service);
    }
    bytecode_export_model(BytecodeExportModelArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        service: target.service,
        selector: BytecodeInstanceSelectorArgs::by_settings_id(Some(target.settings_id)),
        output: args.output,
        format: args.format,
        pretty: true,
    })
}

pub(crate) fn import_path_command(
    args: ImportPathArgs,
    global_project: Option<&Path>,
) -> Result<()> {
    let explicit_project = args.project.as_deref().or(global_project);
    let mut loaded = explicit_project
        .map(|project| config::load_project(Some(project), None))
        .transpose()?;
    let root = if let Some(loaded) = loaded.as_ref() {
        loaded.root.clone()
    } else if args.project_root.exists() {
        canonical_path(&args.project_root)?
    } else {
        args.project_root.clone()
    };
    let source = canonical_path(&args.source)
        .with_context(|| format!("Failed to resolve {}", args.source.display()))?;
    if args.path_json.is_some() && loaded.is_none() {
        loaded = Some(config::load_project(None, Some(&root))?);
    }
    let configured_src_dir = loaded.as_ref().map_or_else(
        || PathBuf::from("src"),
        |project| project.project.source_root.clone(),
    );
    let (destination, src_dir) = if let Some(path_json) = args.path_json.as_deref() {
        let loaded = loaded.as_ref().context("No Renium project was found")?;
        let destination = import_path_json_destination(&source, loaded, path_json)?;
        (destination, configured_src_dir)
    } else {
        let destination = args
            .destination
            .as_deref()
            .context("--destination or --path-json is required")?;
        config::validate_relative_portable_path(destination, "destination")?;
        (root.join(destination), configured_src_dir)
    };
    let files = collect_import_path_files(&source, &destination)?;
    if args.push {
        if loaded.is_none() {
            loaded = Some(config::load_project(None, Some(&root))?);
        }
        let project = loaded.as_ref().context("No Renium project was found")?;
        let projection = config::stage_project(project)?;
        for (_, target) in &files {
            validate_import_destination_ownership(project, &projection, target)?;
        }
    }
    let PreparedImportPaths {
        writes,
        results: file_results,
        changed_paths,
    } = prepare_import_path_files(&root, &files)?;
    let result = json!({
        "ok": true,
        "source": source,
        "destination": destination,
        "files": file_results,
        "dryRun": args.dry_run,
    });
    if args.dry_run {
        return print_json_output(&result, false);
    }
    apply_file_mutations(&writes, &[])?;
    if args.push && !changed_paths.is_empty() {
        push_editor_changes(PushEditorChangesArgs {
            changed_paths,
            yes: true,
            ..PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root,
                    src_root: src_dir,
                },
                BridgeConnectionArgs::local(8.0),
            )
        })?;
    }
    print_json_output(&result, false)
}

fn validate_import_destination_ownership(
    loaded: &config::LoadedProject,
    projection: &config::ProjectionStage,
    destination: &Path,
) -> Result<()> {
    let relatives = config::project_source_to_staged_relatives(loaded, destination)?;
    if relatives.is_empty() {
        bail!(
            "{} is not owned by the active Renium project; use --path-json or choose a managed destination before --push",
            destination.display()
        );
    }
    let naming = config::project_script_naming(&loaded.project);
    for relative in relatives {
        let mut target = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => value.to_str().map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some((_, leaf, _)) = target
            .last()
            .and_then(|file_name| infer_source_script(file_name, &naming))
        {
            target.pop();
            if let Some(leaf) = leaf {
                target.push(leaf);
            }
        }
        if target.is_empty() {
            bail!(
                "{} does not map to a Roblox instance",
                destination.display()
            );
        }
        if projection.target_is_transformed(&target) {
            bail!(
                "{} is shadowed by a sync rule at {}; import the rule source instead",
                destination.display(),
                target.join(".")
            );
        }
        if config::project_target_is_declarative(loaded, &target)? {
            bail!(
                "{} is shadowed by project configuration at {}; use --path-json or edit the project",
                destination.display(),
                target.join(".")
            );
        }
        let resolved =
            config::resolve_project_write_path(loaded, &target.iter().collect::<PathBuf>())?;
        if resolved.owner == "adapter" {
            bail!(
                "{} maps to an adapter; import its canonical source instead",
                destination.display()
            );
        }
        let source_root = strip_extended_prefix(resolved.source_root);
        let destination = strip_extended_prefix(destination.to_path_buf());
        if destination != source_root && !destination.starts_with(&source_root) {
            bail!(
                "{} is shadowed by the projected {} owner at {}",
                destination.display(),
                resolved.owner,
                source_root.display()
            );
        }
    }
    Ok(())
}

fn import_path_json_destination(
    source: &Path,
    loaded: &config::LoadedProject,
    raw: &str,
) -> Result<PathBuf> {
    let segments = parse_bracket_path_segments(raw)
        .context("--path-json must be a JSON string array or bracketed comma list")?;
    if segments.len() < 2 {
        bail!("--path-json must include a service and target name");
    }
    let mut relative = PathBuf::new();
    for segment in &segments {
        if segment.trim().is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains(['/', '\\'])
        {
            bail!("--path-json contains an invalid path segment: {segment:?}");
        }
        relative.push(segment);
    }
    config::validate_relative_portable_path(&relative, "--path-json")?;
    let resolution = config::resolve_project_write_path(loaded, &relative)?;
    if resolution.owner == "adapter" {
        bail!("--path-json targets an adapter; edit or import its canonical source file instead");
    }
    let mut destination = resolution.path;
    if source.is_file() && destination.extension().is_none() {
        let naming = config::project_script_naming(&loaded.project);
        let suffix = import_source_suffix(source, Some(&naming))?;
        let target_name = destination
            .file_name()
            .and_then(|value| value.to_str())
            .context("--path-json target name is not valid UTF-8")?;
        destination.set_file_name(format!("{target_name}{suffix}"));
    }
    Ok(destination)
}

struct PreparedImportPaths {
    writes: BTreeMap<PathBuf, Vec<u8>>,
    results: Vec<Value>,
    changed_paths: Vec<PathBuf>,
}

fn prepare_import_path_files(
    root: &Path,
    files: &[(PathBuf, PathBuf)],
) -> Result<PreparedImportPaths> {
    let mut writes = BTreeMap::new();
    let mut results = Vec::with_capacity(files.len());
    let mut changed_paths = Vec::with_capacity(files.len());
    for (source, destination) in files {
        let bytes =
            fs::read(source).with_context(|| format!("Failed to read {}", source.display()))?;
        let action = if destination.exists() {
            if fs::read(destination)
                .with_context(|| format!("Failed to read {}", destination.display()))?
                == bytes
            {
                "unchanged"
            } else {
                "overwrite"
            }
        } else {
            "create"
        };
        results.push(json!({
            "path": path_to_sourcemap_relative(root, destination),
            "action": action,
        }));
        if action != "unchanged" {
            writes.insert(destination.clone(), bytes);
            changed_paths.push(destination.clone());
        }
    }
    Ok(PreparedImportPaths {
        writes,
        results,
        changed_paths,
    })
}

fn import_source_suffix(
    source: &Path,
    naming: Option<&config::ProjectScriptNaming>,
) -> Result<String> {
    let name = source
        .file_name()
        .and_then(|value| value.to_str())
        .context("Import source file name is not valid UTF-8")?;
    let lower = name.to_ascii_lowercase();
    if let Some(naming) = naming
        && let Some((_, stem, _)) = infer_source_script(name, naming)
    {
        let prefix_len = stem.as_deref().map_or(4, str::len);
        return Ok(name[prefix_len..].to_string());
    }
    for suffix in [
        ".server.luau",
        ".server.lua",
        ".client.luau",
        ".client.lua",
        ".plugin.luau",
        ".plugin.lua",
        ".model.renium.jsonc",
        ".model.json",
        ".project.jsonc",
        ".project.json",
    ] {
        if lower.ends_with(suffix) {
            return Ok(suffix.to_string());
        }
    }
    source
        .extension()
        .and_then(|value| value.to_str())
        .map(|extension| format!(".{extension}"))
        .context("Import source file has no extension")
}

fn collect_import_path_files(source: &Path, destination: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    if source.is_file() {
        let target = if destination.is_dir() {
            destination.join(
                source
                    .file_name()
                    .context("Import source file has no file name")?,
            )
        } else {
            destination.to_path_buf()
        };
        return Ok(vec![(source.to_path_buf(), target)]);
    }
    if !source.is_dir() {
        bail!(
            "Import source is not a file or directory: {}",
            source.display()
        );
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(source) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push((
                entry.path().to_path_buf(),
                destination.join(entry.path().strip_prefix(source)?),
            ));
        }
    }
    Ok(files)
}

pub(crate) fn syncback_command(args: SyncbackArgs, global_project: Option<&Path>) -> Result<()> {
    let loaded = config::load_project(args.project.as_deref().or(global_project), None)?;
    let snapshot_dir = canonical_path(&args.input)
        .with_context(|| format!("Failed to resolve {}", args.input.display()))?;
    let services = parse_services(&args.services)?;
    let mut projected = Vec::new();
    let mut ignored = Vec::new();
    for service in &services {
        let state = match load_service_state(&snapshot_dir, service) {
            Ok(state) => state,
            Err(_) if !snapshot_service_exists(&snapshot_dir, service) => continue,
            Err(error) => return Err(error),
        };
        for (index, instance) in state.instances.iter().enumerate() {
            let fields =
                config::filter_candidate_fields(&instance.properties, &instance.attributes);
            let settings_id = instance_settings_id(index, instance);
            let candidate = fields.candidate(
                &settings_id,
                &instance.path,
                &instance.name,
                &instance.class_name,
            );
            let entry = json!({
                "service": service,
                "path": instance.path,
                "name": instance.name,
                "className": instance.class_name,
            });
            if config::filter_allows(
                &loaded.project.filters,
                config::FilterDirection::StudioToFiles,
                &candidate,
            )? {
                projected.push(entry);
            } else {
                ignored.push(entry);
            }
        }
    }
    let mut result = json!({
        "ok": true,
        "input": snapshot_dir,
        "project": loaded.path,
        "projectedCount": projected.len(),
        "ignoredCount": ignored.len(),
        "projected": if args.list { Value::Array(projected) } else { Value::Null },
        "ignored": if args.list { Value::Array(ignored) } else { Value::Null },
        "dryRun": args.dry_run || args.list,
        "canonicalImportPreservesIgnoredInstances": true,
    });
    if !args.dry_run && !args.list && !args.yes && !global_yes() {
        bail!("Review with --list or --dry-run, then pass -y to apply syncback");
    }
    let stage = ExportProjectStage::create(&loaded.root, &loaded.project.source_root, &services)?;
    let stage_loaded = stage
        .loaded
        .as_ref()
        .context("Staged syncback project is missing its project configuration")?;
    let import_root = stage.import_project_root.clone();
    let import_source_root = import_root.join(&stage.import_src_dir);
    let imported_services = import_filtered_syncback(
        stage_loaded,
        &import_root,
        &import_source_root,
        &snapshot_dir,
        &services,
        &loaded.project.filters,
    )?;
    stage.finish_projection(true)?;
    let operations = stage.preview_operations(&loaded.root)?;
    result["operations"] = Value::Array(operations.clone());
    result["operationCount"] = Value::Number(Number::from(operations.len() as u64));
    result["servicesUpdated"] = Value::Number(Number::from(imported_services as u64));
    if args.dry_run || args.list {
        return print_json_output(&result, true);
    }
    stage.publish(&loaded.root)?;
    print_json_output(&result, true)
}

#[derive(Clone, Copy)]
enum SyncbackInstanceSource {
    Incoming,
    Existing,
}

#[derive(Clone, Copy)]
struct SyncbackInstanceChoice {
    source: SyncbackInstanceSource,
    index: usize,
}

fn import_filtered_syncback(
    loaded: &config::LoadedProject,
    project_root: &Path,
    source_root: &Path,
    snapshot_dir: &Path,
    services: &[String],
    filters: &[config::FilterRule],
) -> Result<usize> {
    config::refresh_script_naming(&loaded.root)?;
    fs::create_dir_all(source_root)
        .with_context(|| format!("Failed to create {}", source_root.display()))?;
    let mut sourcemap_nodes = HashMap::new();
    for service in services {
        if !snapshot_service_exists(snapshot_dir, service) {
            continue;
        }
        let incoming = load_service_state(snapshot_dir, service)?;
        let state = if filters.is_empty() {
            incoming
        } else {
            merge_filtered_syncback_state(source_root, service, incoming, filters)?
        };
        let node = import_service_state_with_sourcemap(&state, project_root, source_root, service)?;
        sourcemap_nodes.insert(service.clone(), node);
    }
    let updated = sourcemap_nodes.len();
    write_project_sourcemap_with_updates(project_root, sourcemap_nodes)?;
    Ok(updated)
}

fn merge_filtered_syncback_state(
    source_root: &Path,
    service: &str,
    mut incoming: ServiceState,
    filters: &[config::FilterRule],
) -> Result<ServiceState> {
    let settings_path = service_settings_path(&source_root.join(service));
    let existing = settings_path
        .is_file()
        .then(|| SettingsBytecode::read_file(&settings_path))
        .transpose()?;
    let mut existing_instances = existing
        .as_ref()
        .map(settings_document_as_snapshot_instances)
        .unwrap_or_default();
    let incoming_ids = incoming
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| instance_settings_id(index, instance))
        .collect::<Vec<_>>();
    let existing_ids = existing_instances
        .iter()
        .enumerate()
        .map(|(index, instance)| instance_settings_id(index, instance))
        .collect::<Vec<_>>();
    stabilize_snapshot_references(&mut incoming.instances, &incoming_ids);
    stabilize_snapshot_references(&mut existing_instances, &existing_ids);
    let incoming_by_id = incoming_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect::<HashMap<_, _>>();
    let existing_by_id = existing_ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect::<HashMap<_, _>>();
    let incoming_parent_ids = snapshot_parent_ids(
        &incoming.instances,
        &incoming.children_by_index,
        &incoming_ids,
    );
    let existing_parent_ids = existing
        .as_ref()
        .map(|document| {
            document
                .instances
                .iter()
                .map(|instance| {
                    instance
                        .parent_index
                        .and_then(|parent| existing_ids.get(parent))
                        .cloned()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut selected = HashMap::<String, SyncbackInstanceChoice>::new();
    for (index, instance) in incoming.instances.iter().enumerate() {
        let id = &incoming_ids[index];
        let existing_instance = existing_by_id
            .get(id)
            .and_then(|index| existing_instances.get(*index));
        if index == incoming.service_root_index
            || syncback_filter_allows_instance(filters, Some(instance), existing_instance)?
        {
            selected.insert(
                id.clone(),
                SyncbackInstanceChoice {
                    source: SyncbackInstanceSource::Incoming,
                    index,
                },
            );
        } else if let Some(existing_index) = existing_by_id.get(id).copied() {
            selected.insert(
                id.clone(),
                SyncbackInstanceChoice {
                    source: SyncbackInstanceSource::Existing,
                    index: existing_index,
                },
            );
        }
    }
    for (index, instance) in existing_instances.iter().enumerate() {
        let id = &existing_ids[index];
        if incoming_by_id.contains_key(id)
            || syncback_filter_allows_instance(filters, None, Some(instance))?
        {
            continue;
        }
        selected.insert(
            id.clone(),
            SyncbackInstanceChoice {
                source: SyncbackInstanceSource::Existing,
                index,
            },
        );
    }
    let mut pending = selected.values().copied().collect::<Vec<_>>();
    while let Some(choice) = pending.pop() {
        let parent_id =
            syncback_choice_parent_id(choice, &incoming_parent_ids, &existing_parent_ids);
        let Some(parent_id) = parent_id else {
            continue;
        };
        if selected.contains_key(parent_id) {
            continue;
        }
        let parent = match choice.source {
            SyncbackInstanceSource::Incoming => {
                incoming_by_id
                    .get(parent_id)
                    .map(|index| SyncbackInstanceChoice {
                        source: SyncbackInstanceSource::Incoming,
                        index: *index,
                    })
            }
            SyncbackInstanceSource::Existing => {
                existing_by_id
                    .get(parent_id)
                    .map(|index| SyncbackInstanceChoice {
                        source: SyncbackInstanceSource::Existing,
                        index: *index,
                    })
            }
        }
        .with_context(|| {
            format!("Filtered syncback instance has a missing parent id {parent_id}")
        })?;
        selected.insert(parent_id.clone(), parent);
        pending.push(parent);
    }
    let mut choices = selected
        .into_iter()
        .map(|(id, choice)| {
            let parent_id =
                syncback_choice_parent_id(choice, &incoming_parent_ids, &existing_parent_ids)
                    .cloned();
            (id, parent_id, choice)
        })
        .collect::<Vec<_>>();
    choices.sort_by_key(|(_, _, choice)| match choice.source {
        SyncbackInstanceSource::Incoming => (0usize, choice.index),
        SyncbackInstanceSource::Existing => (1usize, choice.index),
    });
    let mut merged = Vec::with_capacity(choices.len());
    let mut output_by_id = HashMap::<String, usize>::new();
    while !choices.is_empty() {
        let before = choices.len();
        let mut deferred = Vec::new();
        for (id, parent_id, choice) in choices {
            let parent_index = match parent_id.as_ref() {
                Some(parent_key) => {
                    let Some(index) = output_by_id.get(parent_key).copied() else {
                        deferred.push((id, parent_id, choice));
                        continue;
                    };
                    Some(index)
                }
                None => None,
            };
            let source = match choice.source {
                SyncbackInstanceSource::Incoming => &incoming.instances[choice.index],
                SyncbackInstanceSource::Existing => &existing_instances[choice.index],
            };
            let mut instance = source.clone();
            if matches!(choice.source, SyncbackInstanceSource::Incoming)
                && let Some(existing_index) = existing_by_id.get(&id).copied()
            {
                merge_syncback_instance_fields(
                    filters,
                    &mut instance,
                    &incoming.instances[choice.index],
                    &existing_instances[existing_index],
                )?;
            }
            instance.instance_id = Some(id.clone());
            instance.parent_instance_id = parent_id;
            instance.debug_id = None;
            instance.parent_debug_id = None;
            instance.instance_index = parent_index.is_none().then_some(1);
            instance.parent_index = None;
            instance.source_key = None;
            let output_index = merged.len();
            output_by_id.insert(id, output_index);
            merged.push(instance);
        }
        if deferred.len() == before {
            bail!("Filtered syncback hierarchy contains a cycle or orphan");
        }
        choices = deferred;
    }
    for instance in &mut merged {
        reindex_snapshot_references(instance, &output_by_id);
    }
    build_service_state_from_instances(
        service,
        Some(service),
        merged,
        incoming.class_defaults_by_class,
        false,
    )
}

fn syncback_choice_parent_id<'a>(
    choice: SyncbackInstanceChoice,
    incoming_parent_ids: &'a [Option<String>],
    existing_parent_ids: &'a [Option<String>],
) -> Option<&'a String> {
    match choice.source {
        SyncbackInstanceSource::Incoming => incoming_parent_ids.get(choice.index),
        SyncbackInstanceSource::Existing => existing_parent_ids.get(choice.index),
    }
    .and_then(Option::as_ref)
}

fn snapshot_parent_ids(
    instances: &[SnapshotInstance],
    children_by_index: &[Vec<usize>],
    ids: &[String],
) -> Vec<Option<String>> {
    let mut parent_ids = vec![None; instances.len()];
    for (parent_index, children) in children_by_index.iter().enumerate() {
        let Some(parent_id) = ids.get(parent_index) else {
            continue;
        };
        for child_index in children {
            if let Some(slot) = parent_ids.get_mut(*child_index) {
                *slot = Some(parent_id.clone());
            }
        }
    }
    parent_ids
}
