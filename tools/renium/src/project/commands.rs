use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::automation::commands::daemon_result;
use crate::automation::op;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::app::output::print_json_output;
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
};
use crate::editor::paths::{build_editor_instance_paths, infer_source_script};
use crate::editor::sync::push_editor_changes;
use crate::project::config;
use crate::project::layout::configured_project_layout;
use crate::project::package_links::build_loaded_project_link_enforcement;
use crate::project::sourcemap::path_to_sourcemap_relative;
use crate::project::structural::move_instance_between_service_stores;
use crate::rbx::model::{bytecode_export_model, bytecode_import_model};
use crate::settings::bytecode::SettingsBytecode;
use crate::settings::tree::editor_service_root_index;
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

/// Returns the target's projected path plus the settings ID it resolved to,
/// so a dotted path given in place of an ID works everywhere an ID does.
fn projected_structural_target(
    stage: &config::ProjectionStage,
    service: &str,
    target: Option<&str>,
) -> Result<(Vec<String>, Vec<usize>, Option<String>)> {
    let settings = service_settings_path(&stage.root().join(service));
    let Some(target) = target else {
        return Ok((vec![service.to_string()], vec![1], None));
    };
    if !settings.is_file() {
        bail!("Projected service '{service}' has no Renium store");
    }
    let document = SettingsBytecode::read_file(&settings)?;
    let index = crate::bytecode::query::resolve_document_id_or_target(&document, target)
        .with_context(|| format!("Projected service '{service}' has no instance '{target}'"))?;
    let settings_id = document.instances[index].settings_id.clone();
    let paths = build_editor_instance_paths(&document, service);
    let path = paths
        .get(index)
        .and_then(Option::as_ref)
        .with_context(|| format!("Cannot resolve the projected path for '{settings_id}'"))?;
    Ok((
        path.path_segments.clone(),
        path.path_ordinals.clone(),
        Some(settings_id),
    ))
}

fn projected_structural_store(
    loaded: &config::LoadedProject,
    stage: &config::ProjectionStage,
    service: &str,
    settings_id: Option<&str>,
    reject_declarative_target: bool,
    override_packages: bool,
) -> Result<(PathBuf, Vec<String>, Option<String>)> {
    let (target, target_ordinals, resolved_id) =
        projected_structural_target(stage, service, settings_id)?;
    let settings_id = resolved_id.as_deref();
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
    let store = if settings_id.is_none() {
        config::project_structural_store_path(loaded, &target)?
    } else {
        config::project_structural_store(loaded, &target)?
    };
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
    let store = if store.exists() {
        canonical_path(&store)?
    } else {
        store
    };
    Ok((store, target, canonical_id))
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

/// `upl` is refused inside a read-only link like any edit, except at the
/// root of a link whose package has no PackageLink there (`lkp --share`
/// strips it and asks the other places to unlink): removing a Roblox link
/// there brings the copy back to its package. That refusal comes back
/// deferred, to stand only when the unlink finds nothing to remove.
fn projected_unlink_store(
    project: Option<&Path>,
    target: &ProjectInstanceArgs,
    override_packages: bool,
) -> Result<(PathBuf, Option<String>, Option<anyhow::Error>)> {
    let loaded = load_structural_project(project, &target.project_root)?;
    let stage = config::stage_project(&loaded)?;
    let mut deferred = None;
    if !override_packages {
        let (path, ordinals, _) =
            projected_structural_target(&stage, &target.service, Some(&target.settings_id))?;
        let enforcement = build_loaded_project_link_enforcement(&loaded, false)?;
        let refusal = enforcement.reject_read_only_package_path(&target.service, &path, &ordinals);
        if enforcement.unlink_matches_package(&target.service, &path, &ordinals) {
            deferred = refusal.err();
        } else {
            refusal?;
        }
    }
    let (settings_file, _, settings_id) = projected_structural_store(
        &loaded,
        &stage,
        &target.service,
        Some(&target.settings_id),
        true,
        true,
    )?;
    Ok((settings_file, settings_id, deferred))
}

/// The projected caller has already applied the read-only link check, with
/// its own `--override-packages`.
fn metadata_property_change(
    settings_file: PathBuf,
    settings_id: Option<String>,
    property: &str,
    value: Option<String>,
    push: bool,
) -> BytecodeSetPropertyArgs {
    BytecodeSetPropertyArgs {
        where_query: None,
        input: BytecodeFileArgs::settings_file(settings_file),
        selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
        property: vec![property.to_string()],
        value_json: None,
        value_str: value,
        value_num: None,
        value_bool: None,
        value_null: false,
        scope: "metadata".to_string(),
        override_packages: true,
        push,
        pretty: false,
    }
}

fn service_root_settings_id(settings_file: &Path, service: &str) -> Result<String> {
    let document = SettingsBytecode::read_file(settings_file)?;
    let root_index = editor_service_root_index(&document, service)
        .with_context(|| format!("Service '{service}' has no root instance"))?;
    Ok(document.instances[root_index].settings_id.clone())
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
            service_hint: Some(args.service),
            name: Some(args.name),
            class_name: Some(args.class_name),
            settings_id: None,
            batch: None,
            push: false,
            parent: BytecodeParentArgs {
                parent_settings_id,
                ..Default::default()
            },
            properties: args.properties,
            attributes: args.attributes,
            pretty: false,
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
        service_hint: Some(args.service),
        name: Some(args.name),
        class_name: Some(args.class_name),
        settings_id: None,
        batch: None,
        push: false,
        parent: BytecodeParentArgs {
            parent_settings_id: args.parent_settings_id,
            ..Default::default()
        },
        properties: args.properties,
        attributes: args.attributes,
        pretty: false,
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
        pretty: false,
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
            args.parent_settings_id.as_deref(),
            false,
            args.override_packages,
        )?;
        if !target_file.is_file() {
            bail!("Target service '{target_service}' has no Renium store");
        }
        let parent_id = parent_id
            .map(Ok)
            .unwrap_or_else(|| service_root_settings_id(&target_file, &target_service))?;
        if source_file != target_file {
            return move_instance_between_service_stores(
                &source_file,
                &target.service,
                source_id
                    .as_deref()
                    .context("Source instance has no canonical id")?,
                &target_file,
                &target_service,
                &parent_id,
            );
        }
        return move_within_store(
            metadata_property_change(source_file, source_id, "Parent", Some(parent_id), false),
            args.push,
        );
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
    if !target_file.is_file() {
        bail!("Target service '{target_service}' has no Renium store");
    }
    let parent_settings_id = args
        .parent_settings_id
        .map(Ok)
        .unwrap_or_else(|| service_root_settings_id(&target_file, &target_service))?;
    if target_service == target.service {
        return move_within_store(
            metadata_property_change(
                source_file,
                Some(target.settings_id),
                "Parent",
                Some(parent_settings_id),
                false,
            ),
            args.push,
        );
    }
    move_instance_between_service_stores(
        &source_file,
        &target.service,
        &target.settings_id,
        &target_file,
        &target_service,
        &parent_settings_id,
    )
}

/// Moves an instance inside one store and, with `--push`, reparents the
/// same instance in the open place before a scoped push syncs it there; a
/// scoped push alone would create a copy under the new parent and leave the
/// original where it was.
fn move_within_store(args: BytecodeSetPropertyArgs, push: bool) -> Result<()> {
    let settings_file = args
        .input
        .settings_file
        .clone()
        .context("A move names its store file")?;
    let settings_id = args.selector.settings_id.clone();
    let before = push
        .then(|| {
            crate::bytecode::edit::stored_path_segments(&settings_file, settings_id.as_deref())
        })
        .transpose()?
        .flatten();
    let mut result = crate::bytecode::bytecode_set_property_result(args)?;
    if push {
        let after =
            crate::bytecode::edit::stored_path_segments(&settings_file, settings_id.as_deref())?;
        if let (Some(before), Some(after)) = (before, after) {
            result["reparented"] = reparent_in_studio(&before, &after)?;
        }
        if result.get("settingsId").is_none() {
            result["settingsId"] = json!(settings_id);
        }
        result["settingsFile"] = json!(settings_file);
        crate::bytecode::edit::push_stored_instance_reading(
            &mut result,
            &["Parent".to_string()],
            &[],
        )?;
    }
    crate::app::output::print_json_output(&result, false)
}

/// Sets the parent of the instance at `before` to the instance at the parent
/// of `after` in the open place, as one undo step, and returns what happened.
fn reparent_in_studio(before: &[String], after: &[String]) -> Result<Value> {
    let Some((_, new_parent)) = after.split_last() else {
        return Ok(json!("unchanged"));
    };
    if before.len() >= 2 && before[..before.len() - 1] == *new_parent {
        return Ok(json!("unchanged"));
    }
    let code = format!(
        "local function at(segments) local inst = game for _, name in segments do inst = inst:FindFirstChild(name) if not inst then return nil end end return inst end\n         local inst = at({}) local parent = at({})\n         if not inst then return 'missing' end if not parent then return 'parent missing' end\n         if inst.Parent == parent then return 'unchanged' end inst.Parent = parent return 'moved'",
        luau_string_list(before),
        luau_string_list(new_parent),
    );
    let result = crate::daemon::daemon_control_request(
        crate::automation::op::LUAU,
        None,
        json!({ "code": code, "edit": true, "bridgeWaitSeconds": 8.0, "bridgePorts": "8781,8782" }),
        false,
    )
    .context("The store was moved but Studio could not reparent the instance; push the store with `rbx ps`")?;
    Ok(result
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .cloned()
        .unwrap_or(result))
}

fn luau_string_list(segments: &[String]) -> String {
    let items = segments
        .iter()
        .map(|segment| format!("{:?}", segment))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{items}}}")
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
            args.push,
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
        args.push,
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
        where_query: None,
        push: false,
        input: BytecodeFileArgs::settings_file(settings_file),
        selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
        no_recursive: args.no_recursive,
        pretty: false,
    })
}

pub(crate) fn desync_package_link_command(
    args: DesyncPackageLinkCommandArgs,
    project: Option<&Path>,
) -> Result<()> {
    let target = args.target;
    let (settings_file, settings_id, read_only_refusal) =
        projected_unlink_store(project, &target, args.override_packages)?;
    let segments =
        stored_instance_segments(&settings_file, &target.service, settings_id.as_deref())?;
    let linked_in_store = store_has_package_link(&settings_file, settings_id.as_deref())?;
    let studio = match unlink_package_in_studio(project, &target.service, &segments) {
        Ok(studio) => studio,
        Err(error) => {
            if let Some(refusal) = read_only_refusal
                && !linked_in_store
                && format!("{error:#}").contains("is missing")
            {
                return Err(refusal);
            }
            return Err(error);
        }
    };
    if let Some(refusal) = read_only_refusal
        && !linked_in_store
        && studio
            .as_deref()
            .is_none_or(|studio| studio == "already unlinked")
    {
        return Err(refusal);
    }
    let live_sync = match &studio {
        Some(_) => live_sync_state(&daemon_result(
            op::STUDIO_STATUS,
            project,
            json!({ "all": false }),
            false,
            Some(&crate::cli::BridgeConnectionArgs::local(8.0)),
        )?),
        None => LiveSyncState::Off,
    };
    if let Some(studio) = &studio
        && live_sync == LiveSyncState::Writing
    {
        return crate::app::output::print_json_output(
            &json!({
                "ok": true,
                "studio": studio,
                "files": "Live Sync pulls the unlinked package into the project",
            }),
            false,
        );
    }
    let mut result = match &studio {
        Some(studio) if !linked_in_store => {
            json!({ "ok": true, "studio": studio, "files": "already unlinked" })
        }
        _ => crate::app::output::capture_json_output(|| {
            bytecode_desync_package_link(BytecodeDesyncPackageLinkArgs {
                input: BytecodeFileArgs::settings_file(settings_file),
                service: target.service,
                selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
                pretty: false,
            })
        })?,
    };
    if let Some(studio) = &studio {
        crate::app::output::log_global(
            3,
            format_args!("[renium] package unlinked in Studio: {studio}"),
        );
        result["studio"] = json!(studio);
    }
    if live_sync == LiveSyncState::Reviewing {
        result["liveSync"] = json!(compared_again_summary(
            crate::studio::automation::compare_live_sync_again(project)
        ));
    }
    crate::app::output::print_json_output(&result, false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveSyncState {
    Off,
    Writing,
    /// Waiting for a decision on differences it found; not writing.
    Reviewing,
    /// Verify-only for another reason; not writing either.
    Holding,
}

fn live_sync_state(status: &Value) -> LiveSyncState {
    let live = &status["liveSync"];
    if live["running"].as_bool() != Some(true) {
        LiveSyncState::Off
    } else if live["resolutionRequired"].as_bool() == Some(true) {
        LiveSyncState::Reviewing
    } else if live["mode"].as_str() == Some("verify") {
        LiveSyncState::Holding
    } else {
        LiveSyncState::Writing
    }
}

/// What a second comparison made of the review the unlink answered.
fn compared_again_summary(compared: Result<Value>) -> String {
    match compared {
        Ok(result) if result.pointer("/daemon/resolutionRequired") == Some(&Value::Bool(true)) => {
            format!(
                "still waiting for a decision: {}",
                result["error"]
                    .as_str()
                    .unwrap_or("Studio and project files differ")
            )
        }
        Ok(result) if result["ok"] == Value::Bool(false) => format!(
            "compared again and stopped: {}",
            result["error"].as_str().unwrap_or("unknown error")
        ),
        Ok(_) => "compared again; the review cleared and Live Sync resumed".to_string(),
        Err(error) => format!("could not compare again: {error:#}; run rbx lon"),
    }
}

fn store_has_package_link(settings_file: &Path, settings_id: Option<&str>) -> Result<bool> {
    let document = SettingsBytecode::read_file(settings_file)?;
    let Some(settings_id) = settings_id else {
        return Ok(false);
    };
    let Some(index) = document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
    else {
        return Ok(false);
    };
    Ok(document.instances.iter().any(|instance| {
        instance.parent_index == Some(index) && instance.class_name == "PackageLink"
    }))
}

fn stored_instance_segments(
    settings_file: &Path,
    service: &str,
    settings_id: Option<&str>,
) -> Result<Vec<String>> {
    let document = SettingsBytecode::read_file(settings_file)?;
    let Some(settings_id) = settings_id else {
        return Ok(Vec::new());
    };
    let index = document
        .instances
        .iter()
        .position(|instance| instance.settings_id == settings_id)
        .with_context(|| format!("{settings_id} is not in the {service} store"))?;
    let paths = build_editor_instance_paths(&document, service);
    let mut segments = paths
        .get(index)
        .and_then(Option::as_ref)
        .map(|path| path.path_segments.clone())
        .unwrap_or_default();
    if segments.first().is_some_and(|first| first == service) {
        segments.remove(0);
    }
    Ok(segments)
}

/// Destroys the PackageLink under the target in the connected edit Studio;
/// Studio does not let a plugin reparent a PackageLink, but destroying it is
/// how Unlink from Package works. None when no Studio holds the place.
fn unlink_package_in_studio(
    project: Option<&Path>,
    service: &str,
    segments: &[String],
) -> Result<Option<String>> {
    let lua_strings = segments
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(", ");
    let code = format!(
        "local inst = game:GetService({service}) for _, name in ipairs({{{lua_strings}}}) do inst = inst:FindFirstChild(name) if not inst then return {{ found = false, missing = name }} end end local link = inst:FindFirstChildOfClass('PackageLink') if not link then return {{ found = true, link = false }} end link:Destroy() return {{ found = true, link = true, removed = inst:FindFirstChildOfClass('PackageLink') == nil, path = inst:GetFullName() }}",
        service = serde_json::to_string(service)?,
    );
    let bridge = crate::cli::BridgeConnectionArgs::local(8.0);
    let result = match daemon_result(
        op::LUAU,
        project,
        json!({
            "code": code,
            "edit": true,
            "timeout": 30.0,
            "bridgeWaitSeconds": bridge.wait_seconds,
            "bridgePorts": bridge.ports,
        }),
        false,
        Some(&bridge),
    ) {
        Ok(result) => result,
        Err(error) => {
            let text = format!("{error:#}");
            if text.contains("No Studio runtime") || text.contains("no_studio") {
                return Ok(None);
            }
            return Err(error);
        }
    };
    let outcome = result
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .cloned()
        .unwrap_or(Value::Null);
    if outcome.get("found") == Some(&Value::Bool(false)) {
        bail!(
            "Studio has no {service}.{} ({} is missing); the files and Studio differ, run `rbx lst --details`",
            segments.join("."),
            outcome
                .get("missing")
                .and_then(Value::as_str)
                .unwrap_or("a segment")
        );
    }
    if outcome.get("link") == Some(&Value::Bool(false)) {
        return Ok(Some("already unlinked".to_string()));
    }
    if outcome.get("removed") == Some(&Value::Bool(true)) {
        return Ok(Some(format!(
            "unlinked {}",
            outcome
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("the package")
        )));
    }
    bail!("Studio kept the PackageLink after Destroy: {outcome}")
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
        pretty: false,
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
    let (_, _, settings_id) =
        projected_structural_target(&stage, &target.service, Some(&target.settings_id))?;
    bytecode_export_model(BytecodeExportModelArgs {
        input: BytecodeFileArgs::settings_file(settings_file),
        service: target.service,
        selector: BytecodeInstanceSelectorArgs::by_settings_id(settings_id),
        output: args.output,
        format: args.format,
        pretty: false,
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
    let mut root = if let Some(loaded) = loaded.as_ref() {
        loaded.root.clone()
    } else if args.project_root.exists() {
        canonical_path(&args.project_root)?
    } else {
        args.project_root.clone()
    };
    let source = canonical_path(&args.source)
        .with_context(|| format!("Failed to resolve {}", args.source.display()))?;
    if args.path_json.is_some() && loaded.is_none() {
        let project = config::load_project(None, Some(&root))?;
        root = project.root.clone();
        loaded = Some(project);
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
            .context("ip imports a script file with --path-json or a directory with --destination; import places with pi and models with mip")?;
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
        mut changed_paths,
    } = prepare_import_path_files(&root, &files)?;
    let mut result = json!({
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
    if let (Some(path_json), Some(project)) = (args.path_json.as_deref(), loaded.as_ref()) {
        match crate::bytecode::edit::register_imported_script(project, path_json, &destination) {
            Ok(Some(instance)) => {
                changed_paths.extend(
                    instance["changedPaths"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from),
                );
                changed_paths.sort();
                changed_paths.dedup();
                result["storeInstance"] = instance;
            }
            Ok(None) => {}
            Err(error) => result["storeInstanceError"] = json!(format!("{error:#}")),
        }
    }
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
    if source.is_file() {
        let naming = config::project_script_naming(&loaded.project);
        let target_name = destination
            .file_name()
            .and_then(|value| value.to_str())
            .context("--path-json target name is not valid UTF-8")?;
        // The last segment is an instance name, which may contain dots.
        if infer_source_script(target_name, &naming).is_none() {
            let suffix = import_source_suffix(source, Some(&naming))?;
            destination.set_file_name(format!("{target_name}{suffix}"));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlink_defers_only_to_a_live_sync_that_writes() {
        let state = |live: Value| live_sync_state(&json!({ "liveSync": live }));
        assert_eq!(state(Value::Null), LiveSyncState::Off);
        assert_eq!(state(json!({"running": false})), LiveSyncState::Off);
        assert_eq!(
            state(json!({"running": true, "pending": 0})),
            LiveSyncState::Writing
        );
        assert_eq!(
            state(json!({"running": true, "resolutionRequired": true, "mode": "verify"})),
            LiveSyncState::Reviewing
        );
        assert_eq!(
            state(json!({"running": true, "mode": "verify", "pushesPaused": true})),
            LiveSyncState::Holding
        );
        assert_eq!(
            compared_again_summary(Ok(json!({"ok": true, "daemon": {"running": true}}))),
            "compared again; the review cleared and Live Sync resumed"
        );
        assert_eq!(
            compared_again_summary(Ok(json!({
                "ok": false,
                "error": "Sync needs review: x",
                "daemon": {"resolutionRequired": true}
            }))),
            "still waiting for a decision: Sync needs review: x"
        );
    }
}
