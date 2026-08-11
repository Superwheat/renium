use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use globset::escape as escape_glob;
use serde_json::{Map, Value, json};

use crate::bytecode_api::apply_file_mutations;
use crate::bytecode_edit::validate_settings_model_internal_references;
use crate::editor_document::read_editor_service_documents;
use crate::editor_paths::{infer_source_script, run_context_name};
use crate::file_io::{
    atomic_write_file, is_service_settings_file_name, path_extension_is, read_file_if_present,
    service_settings_path, sha256_hex,
};
use crate::file_watch::FileWatcher;
use crate::settings_bytecode::{
    SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode, reindex_reference_indices,
};
use crate::settings_tree::settings_children_by_parent;
use crate::snapshot_refs::remap_record_reference_ids;

use super::adapter_format::{
    AdapterFormat, adapter_format, adapter_output_path, compare_or_write, localization_json_to_csv,
    render_adapter, validate_adapter_source,
};
use super::projection::{
    copy_directory_tree, filter_allows_candidate_pair, find_document_target,
    find_document_target_optional, find_document_target_optional_with_ordinals,
    fresh_projection_stage, owned_filter_candidate, projection_field_owners_with_root,
    refresh_stage_settings, stage_adapter, stage_mount, stage_project, target_segments,
    validate_projection_field_ownership,
};
use super::projection_references::{
    canonicalize_projection_document_map, normalize_stage_references,
};
use super::validation::{validate_nested_project, validate_project};
use super::{
    AdapterBaseline, AdapterBaselineEntry, AdapterDirection, AdapterSpec, FilterDirection,
    FilterRule, FilterScope, LoadedProject, MountOwnership, OwnedFilterCandidate,
    PROJECTION_IDENTITY_STACK, PROJECTION_TRANSFORM_STACK, ProjectScriptNaming, ProjectTarget,
    ProjectionStage, ReverseOwner, ReverseSource, absolute_path, active_target_ordinals,
    cache_script_naming, filter_allows_scope, filter_path_segments, load_nested_project,
    load_project, parse_jsonc_value, path_slash, project_script_naming, project_source_roots,
    project_tree_nodes, remove_empty_stage_parents, target_is_within, targets_are_equal,
    with_project_target, with_target_parts,
};

fn adapter_key(adapter: &AdapterSpec) -> String {
    let segments = adapter.target.segments();
    let mut ordinals = adapter.target.ordinals();
    ordinals.resize(segments.len(), 1);
    format!(
        "{}\0{}",
        path_slash(&adapter.source),
        serde_json::to_string(&(segments, ordinals)).unwrap_or_else(|_| adapter.target.to_string())
    )
}

fn adapter_target_bytes_from_root(
    loaded: &LoadedProject,
    root: &Path,
    adapter: &AdapterSpec,
    format: AdapterFormat,
) -> Result<Option<Vec<u8>>> {
    with_project_target(&adapter.target, |target| {
        let service = target
            .first()
            .context("Adapter target must include a service")?;
        let settings = service_settings_path(&root.join(service));
        if !settings.is_file() {
            return Ok(None);
        }
        let document = SettingsBytecode::read_file(&settings)?;
        if find_document_target_optional(&document, target)?.is_none() {
            return Ok(None);
        }
        reversible_adapter_target_bytes(
            adapter,
            format,
            &document,
            target,
            &loaded.root.join(&adapter.source),
            None,
        )
        .map(Some)
    })
}

fn create_adapter_stage(loaded: &LoadedProject, name: &str) -> Result<ProjectionStage> {
    let root = fresh_projection_stage(
        &loaded.root.join(".renium").join("build-staging"),
        &format!("adapter-{name}-"),
    )?;
    if let Some(source_root) = loaded
        .root
        .join(&loaded.project.source_root)
        .is_dir()
        .then(|| loaded.root.join(&loaded.project.source_root))
    {
        copy_directory_tree(&source_root, &root)?;
    }
    cache_script_naming(&root, &loaded.project);
    Ok(ProjectionStage {
        root,
        temporary: true,
        cleanup: true,
        transforms: Vec::new(),
        identities: HashMap::new(),
    })
}

fn load_adapter_baseline(loaded: &LoadedProject) -> Result<(PathBuf, AdapterBaseline)> {
    let path = loaded.root.join(".renium").join("adapter-baseline.json");
    let baseline = if path.is_file() {
        serde_json::from_slice::<AdapterBaseline>(&fs::read(&path)?)
            .with_context(|| format!("Invalid adapter baseline {}", path.display()))?
    } else {
        AdapterBaseline::default()
    };
    Ok((path, baseline))
}

pub(super) fn build_adapters(loaded: &LoadedProject, check: bool, emit: bool) -> Result<()> {
    let mut changed = Vec::new();
    let (baseline_path, mut baseline) = load_adapter_baseline(loaded)?;
    let mut transaction_paths = Vec::new();
    let mut active_baseline_keys = BTreeSet::new();
    let mut active_outputs = BTreeMap::new();
    let mut active_output_owned = BTreeMap::new();
    for adapter in &loaded.project.adapters {
        let key = adapter_key(adapter);
        active_baseline_keys.insert(key.clone());
        let format = adapter_format(adapter)?;
        if adapter.direction == AdapterDirection::FromProject {
            active_outputs.insert(key, None);
            continue;
        }
        let output = adapter_output_path(loaded, adapter, format)?;
        let owned = output.as_deref().is_some_and(|path| {
            baseline.entries.get(&key).is_some_and(|entry| {
                entry.output_owned
                    && entry.output.as_deref().is_some_and(|previous| {
                        absolute_path(&loaded.root.join(previous)) == absolute_path(path)
                    })
            }) || !path.exists()
        });
        if let Some(output) = output.as_ref() {
            transaction_paths.push(output.clone());
        }
        if format.is_reversible() {
            let target = target_segments(&adapter.target)?;
            let service = target
                .first()
                .context("Adapter target must include a service")?;
            transaction_paths.push(service_settings_path(
                &loaded.root.join(&loaded.project.source_root).join(service),
            ));
        }
        active_output_owned.insert(key.clone(), owned);
        active_outputs.insert(key, output);
    }
    for (key, entry) in &baseline.entries {
        if let Some(output) = entry.output.as_deref() {
            transaction_paths.push(loaded.root.join(output));
        }
        if active_baseline_keys.contains(key) {
            continue;
        }
        let Some((_, target)) = key.split_once('\0') else {
            continue;
        };
        let target = serde_json::from_str::<ProjectTarget>(target)
            .unwrap_or_else(|_| ProjectTarget::Shorthand(target.to_string()));
        let target = target_segments(&target)?;
        if let Some(service) = target.first() {
            transaction_paths.push(service_settings_path(
                &loaded.root.join(&loaded.project.source_root).join(service),
            ));
        }
    }
    if !active_baseline_keys.is_empty() || baseline_path.exists() {
        transaction_paths.push(baseline_path.clone());
    }
    transaction_paths.sort();
    transaction_paths.dedup();
    let originals = if check {
        Vec::new()
    } else {
        transaction_paths
            .iter()
            .map(|path| {
                fs::read(path).map(Some).or_else(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        Ok(None)
                    } else {
                        Err(error)
                    }
                })
            })
            .collect::<io::Result<Vec<_>>>()?
    };
    let result = (|| -> Result<()> {
        prune_stale_adapter_targets(
            loaded,
            &mut baseline,
            &active_baseline_keys,
            &active_outputs,
            check,
            &mut changed,
        )?;
        for adapter in &loaded.project.adapters {
            if adapter.direction != AdapterDirection::FromProject {
                continue;
            }
            let key = adapter_key(adapter);
            let Some(entry) = baseline.entries.get_mut(&key) else {
                continue;
            };
            if entry.output.is_none() && entry.output_hash.is_none() && !entry.output_owned {
                continue;
            }
            if check {
                changed.push(format!("adapter baseline {}", adapter.source.display()));
            } else {
                entry.output = None;
                entry.output_hash = None;
                entry.output_owned = false;
            }
        }
        let reversible = loaded
            .project
            .adapters
            .iter()
            .filter_map(|adapter| {
                let format = adapter_format(adapter).ok()?;
                (adapter.direction != AdapterDirection::FromProject && format.is_reversible())
                    .then_some((adapter, format))
            })
            .collect::<Vec<_>>();
        let mut baseline_updates = BTreeMap::<String, (String, String)>::new();
        if !reversible.is_empty() {
            let expected_stage = create_adapter_stage(loaded, "expected")?;
            for (adapter, _) in &reversible {
                stage_adapter(loaded, expected_stage.root(), adapter)?;
            }
            let canonical_root = loaded.root.join(&loaded.project.source_root);
            let mut apply = Vec::new();
            for (adapter, format) in &reversible {
                let key = adapter_key(adapter);
                let source_hash = sha256_hex(&fs::read(loaded.root.join(&adapter.source))?);
                let current =
                    adapter_target_bytes_from_root(loaded, &canonical_root, adapter, *format)?;
                let expected = adapter_target_bytes_from_root(
                    loaded,
                    expected_stage.root(),
                    adapter,
                    *format,
                )?
                .context("Adapter staging did not create its target")?;
                let current_hash = current.as_deref().map(sha256_hex);
                let expected_hash = sha256_hex(&expected);
                let equal = current.as_deref() == Some(expected.as_slice());
                let mut apply_source = adapter.direction == AdapterDirection::ToProject;
                let mut update_baseline = equal;
                if adapter.direction == AdapterDirection::TwoWay && !equal {
                    if let Some(previous) = baseline.entries.get(&key) {
                        if current.is_none() {
                            bail!(
                                "Two-way adapter target '{}' was deleted after its last successful sync; restore it from {} or remove the adapter",
                                adapter.target,
                                adapter.source.display()
                            );
                        }
                        let source_changed = source_hash != previous.source_hash;
                        let target_changed =
                            current_hash.as_deref() != Some(previous.target_hash.as_str());
                        match (source_changed, target_changed) {
                            (true, false) => apply_source = true,
                            (false, true) => {}
                            (true, true) => {
                                bail!(
                                    "Two-way adapter conflict for '{}': both {} and its canonical target changed since the last successful sync",
                                    adapter.target,
                                    adapter.source.display()
                                );
                            }
                            (false, false) => {
                                bail!(
                                    "Two-way adapter '{}' has a divergent baseline; edit one side before building",
                                    adapter.target
                                );
                            }
                        }
                    } else {
                        apply_source = true;
                    }
                }
                if apply_source {
                    apply.push((*adapter, *format));
                    update_baseline = true;
                }
                if update_baseline {
                    baseline_updates.insert(key, (source_hash, expected_hash));
                }
            }
            if !apply.is_empty() {
                let output_stage = create_adapter_stage(loaded, "apply")?;
                let mut services = BTreeSet::new();
                for (adapter, _) in &apply {
                    stage_adapter(loaded, output_stage.root(), adapter)?;
                    let target = target_segments(&adapter.target)?;
                    services.insert(
                        target
                            .first()
                            .context("Adapter target must include a service")?
                            .clone(),
                    );
                }
                for service in services {
                    let staged = service_settings_path(&output_stage.root().join(&service));
                    let canonical = service_settings_path(&canonical_root.join(&service));
                    compare_or_write(&canonical, &fs::read(&staged)?, check, &mut changed)?;
                }
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let source = loaded.root.join(&adapter.source);
            let format = adapter_format(adapter)?;
            validate_adapter_source(&source, format)?;
            if let Some(output) = adapter_output_path(loaded, adapter, format)? {
                let bytes = render_adapter(&source, format)?;
                compare_or_write(&output, &bytes, check, &mut changed)?;
            }
        }
        if active_baseline_keys.is_empty() {
            if baseline_path.is_file() {
                if check {
                    changed.push(path_slash(&baseline_path));
                } else {
                    fs::remove_file(&baseline_path)?;
                }
            }
        } else if !check {
            for adapter in &loaded.project.adapters {
                if adapter.direction == AdapterDirection::FromProject {
                    continue;
                }
                let format = adapter_format(adapter)?;
                let source_bytes = fs::read(loaded.root.join(&adapter.source))?;
                let key = adapter_key(adapter);
                let target_hash = if format.is_reversible() {
                    let Some((_, target_hash)) = baseline_updates.get(&key) else {
                        continue;
                    };
                    target_hash.clone()
                } else {
                    String::new()
                };
                let output = adapter_output_path(loaded, adapter, format)?;
                let output_hash = output
                    .as_deref()
                    .map(fs::read)
                    .transpose()?
                    .map(|bytes| sha256_hex(&bytes));
                baseline.entries.insert(
                    key.clone(),
                    AdapterBaselineEntry {
                        source_hash: sha256_hex(&source_bytes),
                        target_hash,
                        format: Some(format.as_str().to_string()),
                        output: output
                            .as_deref()
                            .and_then(|path| path.strip_prefix(&loaded.root).ok())
                            .map(path_slash),
                        output_hash,
                        output_owned: active_output_owned.get(&key).copied().unwrap_or(false),
                        model_json_hierarchical: if format == AdapterFormat::ModelJson {
                            Some(model_json_source_is_hierarchical(
                                &loaded.root.join(&adapter.source),
                            )?)
                        } else {
                            None
                        },
                    },
                );
            }
            atomic_write_file(&baseline_path, &serde_json::to_vec_pretty(&baseline)?)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if !check {
            restore_file_snapshots(&transaction_paths, &originals)?;
        }
        return Err(error);
    }
    if check && !changed.is_empty() {
        bail!("Generated adapter output is stale: {}", changed.join(", "));
    }
    if !emit {
        return Ok(());
    }
    crate::output::emit_global_output(
        &json!({
            "ok": true,
            "checked": check,
            "adapters": loaded.project.adapters.len(),
            "project": loaded.path,
        }),
        &format!(
            "{} {} adapter output{}",
            if check { "Checked" } else { "Built" },
            loaded.project.adapters.len(),
            if loaded.project.adapters.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
    )
}

fn prune_stale_adapter_targets(
    loaded: &LoadedProject,
    baseline: &mut AdapterBaseline,
    active_keys: &BTreeSet<String>,
    active_outputs: &BTreeMap<String, Option<PathBuf>>,
    check: bool,
    changed: &mut Vec<String>,
) -> Result<()> {
    for (key, entry) in &baseline.entries {
        let Some(previous_output) = entry.output.as_deref() else {
            continue;
        };
        let previous_output = loaded.root.join(previous_output);
        let still_active = active_outputs
            .get(key)
            .and_then(|output| output.as_deref())
            .is_some_and(|output| absolute_path(output) == absolute_path(&previous_output));
        if still_active || !previous_output.is_file() {
            continue;
        }
        let unchanged = entry.output_hash.as_deref().is_some_and(|hash| {
            fs::read(&previous_output).is_ok_and(|bytes| sha256_hex(&bytes) == hash)
        });
        if entry.output_owned && unchanged {
            if check {
                changed.push(path_slash(&previous_output));
            } else {
                fs::remove_file(&previous_output)?;
            }
        }
    }
    let stale = baseline
        .entries
        .keys()
        .filter(|key| !active_keys.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    for key in stale {
        let Some((_source, target_text)) = key.split_once('\0') else {
            if check {
                changed.push("adapter baseline entry".to_string());
            } else {
                baseline.entries.remove(&key);
            }
            continue;
        };
        if check {
            changed.push(format!("removed adapter {target_text}"));
            continue;
        }
        baseline.entries.remove(&key);
    }
    Ok(())
}

fn restore_file_snapshots(paths: &[PathBuf], originals: &[Option<Vec<u8>>]) -> Result<()> {
    let mut errors = Vec::new();
    for (path, original) in paths.iter().zip(originals).rev() {
        if let Err(error) = restore_file_snapshot(path, original.as_deref()) {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("Adapter rollback was incomplete: {}", errors.join("; "))
    }
}

fn restore_file_snapshot(path: &Path, original: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = original {
        atomic_write_file(path, bytes)
    } else if path.is_file() {
        fs::remove_file(path).map_err(anyhow::Error::from)
    } else {
        Ok(())
    }
}

fn validate_read_only_reverse_owners(
    owners: &[ReverseOwner],
    projection_root: &Path,
    baseline_root: &Path,
    documents: &HashMap<String, SettingsBytecode>,
    baseline_documents: &HashMap<String, SettingsBytecode>,
) -> Result<()> {
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::ReadOnly)
    {
        let imported_document = documents
            .get(&owner.target[0])
            .with_context(|| format!("Missing projected service {}", owner.target[0]))?;
        if owner.optional
            && with_target_parts(&owner.target, &owner.ordinals, |target| {
                Ok(find_document_target_optional(imported_document, target)?.is_none())
            })?
        {
            continue;
        }
        let imported = with_target_parts(&owner.target, &owner.ordinals, |target| {
            projection_owner_snapshot(projection_root, imported_document, target)
        })?;
        let original = with_target_parts(&owner.target, &owner.ordinals, |target| {
            projection_owner_snapshot(
                baseline_root,
                baseline_documents
                    .get(&owner.target[0])
                    .with_context(|| format!("Missing baseline service {}", owner.target[0]))?,
                target,
            )
        })?;
        if imported != original {
            bail!(
                "Studio changed read-only mount '{}'; change its source or make the mount writable",
                owner.target.join(".")
            );
        }
    }
    Ok(())
}

fn validate_transformed_reverse_targets(
    targets: &[Vec<String>],
    projection_root: &Path,
    baseline_root: &Path,
    documents: &HashMap<String, SettingsBytecode>,
    baseline_documents: &HashMap<String, SettingsBytecode>,
) -> Result<()> {
    for target in targets {
        let imported_document = documents
            .get(&target[0])
            .with_context(|| format!("Missing projected service {}", target[0]))?;
        let baseline_document = baseline_documents
            .get(&target[0])
            .with_context(|| format!("Missing baseline service {}", target[0]))?;
        let unchanged = projection_owner_snapshot(projection_root, imported_document, target)
            .and_then(|imported| {
                projection_owner_snapshot(baseline_root, baseline_document, target)
                    .map(|original| imported == original)
            })
            .unwrap_or(false);
        if !unchanged {
            bail!(
                "Studio changed sync-rule output '{}'; edit its source file instead",
                target.join(".")
            );
        }
    }
    Ok(())
}

fn collect_reverse_plan(
    plan: ReverseOwnerPlan,
    check: bool,
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut BTreeSet<PathBuf>,
) -> Result<bool> {
    if !reverse_owner_plan_differs(&plan)? {
        return Ok(false);
    }
    if !check {
        for (path, bytes) in plan.writes {
            removals.remove(&path);
            if let Some(previous) = writes.get(&path) {
                if previous != &bytes {
                    bail!(
                        "Reverse projection planned conflicting writes to {}",
                        path.display()
                    );
                }
            } else {
                writes.insert(path, bytes);
            }
        }
        removals.extend(plan.removals);
    }
    Ok(true)
}

pub fn syncback_project_projection(
    loaded: &LoadedProject,
    projection_root: &Path,
    check: bool,
) -> Result<usize> {
    let mut planned_writes = BTreeMap::new();
    let mut planned_removals = BTreeSet::new();
    let changed = syncback_project_projection_into(
        loaded,
        projection_root,
        check,
        &mut planned_writes,
        &mut planned_removals,
    )?;
    if check && changed > 0 {
        bail!("{changed} projected source owner(s) are stale");
    }
    if !check && (!planned_writes.is_empty() || !planned_removals.is_empty()) {
        let removals = planned_removals
            .into_iter()
            .filter(|path| !planned_writes.contains_key(path))
            .collect::<Vec<_>>();
        apply_file_mutations(&planned_writes, &removals)?;
    }
    Ok(changed)
}

fn syncback_project_projection_into(
    loaded: &LoadedProject,
    projection_root: &Path,
    check: bool,
    planned_writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    planned_removals: &mut BTreeSet<PathBuf>,
) -> Result<usize> {
    validate_project(loaded)?;
    let baseline = stage_project(loaded)?;
    if !baseline.is_temporary() && absolute_path(baseline.root()) == absolute_path(projection_root)
    {
        return Ok(0);
    }
    let owners = reverse_owners(loaded)?;
    let naming = project_script_naming(&loaded.project);
    let mut changed = 0usize;
    let mut documents = HashMap::new();
    let mut baseline_documents = HashMap::new();
    for entry in read_editor_service_documents(projection_root)? {
        let service = entry.service;
        documents.insert(service.clone(), entry.document);
        let baseline_settings = service_settings_path(&baseline.root().join(&service));
        if baseline_settings.is_file() {
            baseline_documents.insert(service, SettingsBytecode::read_file(&baseline_settings)?);
        }
    }
    canonicalize_projection_document_map(&mut documents)?;
    canonicalize_projection_document_map(&mut baseline_documents)?;
    validate_projection_field_ownership(loaded, &documents, &baseline_documents)?;
    let projected_sources_by_service = documents
        .iter()
        .map(|(service, document)| {
            Ok((
                service.clone(),
                projection_sources(projection_root, document)?,
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let baseline_sources_by_service = baseline_documents
        .iter()
        .map(|(service, document)| {
            Ok((
                service.clone(),
                projection_sources(baseline.root(), document)?,
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let mut overlay_provenance = HashMap::new();
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::Overlay)
    {
        overlay_provenance.insert(
            reverse_owner_key(owner),
            overlay_owner_provenance(loaded, owner)?,
        );
    }
    validate_read_only_reverse_owners(
        &owners,
        projection_root,
        baseline.root(),
        &documents,
        &baseline_documents,
    )?;

    let mut transformed_targets = baseline
        .transforms
        .iter()
        .map(|transform| transform.target.clone())
        .collect::<Vec<_>>();
    transformed_targets.sort();
    transformed_targets.dedup();
    validate_transformed_reverse_targets(
        &transformed_targets,
        projection_root,
        baseline.root(),
        &documents,
        &baseline_documents,
    )?;

    let mut external_targets = owners
        .iter()
        .map(|owner| ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone()))
        .collect::<Vec<_>>();
    external_targets.extend(
        transformed_targets
            .iter()
            .cloned()
            .map(|target| ProjectTarget::from_parts(target, Vec::new())),
    );
    for adapter in &loaded.project.adapters {
        if adapter.direction != AdapterDirection::FromProject {
            external_targets.push(adapter.target.clone());
        }
    }

    for (service, document) in &documents {
        let target = vec![service.clone()];
        let target_selector = ProjectTarget::from_parts(target.clone(), Vec::new());
        if external_targets
            .iter()
            .any(|owner| targets_are_equal(owner, &target_selector))
        {
            continue;
        }
        let baseline_document = baseline_documents.get(service);
        if loaded
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
            && baseline_document.is_none()
        {
            continue;
        }
        let allowed = if loaded
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
        {
            baseline_document
                .map(|baseline| projection_identity_set(baseline, &target))
                .transpose()?
        } else {
            None
        };
        let mut output =
            extract_projection_document(document, &target, &external_targets, allowed.as_ref())?;
        apply_reverse_filters(
            loaded,
            &target,
            &mut output,
            baseline_document
                .map(|baseline| {
                    extract_projection_document(baseline, &target, &external_targets, None)
                })
                .transpose()?
                .as_ref(),
        )?;
        let destination = loaded.root.join(&loaded.project.source_root).join(service);
        restore_project_owned_fields(loaded, &target, &destination, &mut output)?;
        let plan = plan_reverse_owner(
            &destination,
            &output,
            &projected_sources_by_service[service],
            &naming,
        )?;
        changed += usize::from(collect_reverse_plan(
            plan,
            check,
            planned_writes,
            planned_removals,
        )?);
    }

    for owner in owners {
        if owner.ownership == MountOwnership::ReadOnly {
            continue;
        }
        let document = documents
            .get(&owner.target[0])
            .with_context(|| format!("Missing projected service {}", owner.target[0]))?;
        if owner.optional
            && with_target_parts(&owner.target, &owner.ordinals, |target| {
                Ok(find_document_target_optional(document, target)?.is_none())
            })?
        {
            continue;
        }
        let baseline_document = baseline_documents.get(&owner.target[0]);
        let allowed = if owner.ownership == MountOwnership::Overlay {
            Some(
                overlay_provenance
                    .get(&reverse_owner_key(&owner))
                    .context("Overlay ownership provenance disappeared")?
                    .identities
                    .clone(),
            )
        } else if owner.ignore_unknown_instances {
            baseline_document
                .map(|baseline| {
                    with_target_parts(&owner.target, &owner.ordinals, |target| {
                        projection_identity_set(baseline, target)
                    })
                })
                .transpose()?
        } else {
            None
        };
        let nested_exclusions = external_targets
            .iter()
            .filter(|target| {
                target.segments().len() > owner.target.len()
                    && target_is_within(
                        target,
                        &ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone()),
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut output = with_target_parts(&owner.target, &owner.ordinals, |target| {
            extract_projection_document(document, target, &nested_exclusions, allowed.as_ref())
        })?;
        let baseline_output = baseline_document
            .map(|baseline| {
                with_target_parts(&owner.target, &owner.ordinals, |target| {
                    extract_projection_document(
                        baseline,
                        target,
                        &nested_exclusions,
                        allowed.as_ref(),
                    )
                })
            })
            .transpose()?;
        apply_reverse_filters(loaded, &owner.target, &mut output, baseline_output.as_ref())?;
        if owner.ownership == MountOwnership::Overlay {
            output.instances[0] = overlay_provenance
                .get(&reverse_owner_key(&owner))
                .context("Overlay ownership provenance disappeared")?
                .root
                .clone();
            output.instances[0].parent_index = None;
        }
        restore_project_owned_fields(loaded, &owner.target, &owner.source, &mut output)?;
        if is_nested_project_path(&owner.source) {
            let baseline_output = baseline_output
                .as_ref()
                .context("Nested project mount has no baseline projection")?;
            changed += syncback_nested_owner(
                &owner.source,
                &output,
                baseline_output,
                &projected_sources_by_service[&owner.target[0]],
                baseline_sources_by_service
                    .get(&owner.target[0])
                    .context("Nested project mount has no baseline source map")?,
                check,
                &mut NestedSyncMutations {
                    writes: planned_writes,
                    removals: planned_removals,
                },
            )?;
            continue;
        }
        let plan = plan_reverse_owner(
            &owner.source,
            &output,
            &projected_sources_by_service[&owner.target[0]],
            &naming,
        )?;
        changed += usize::from(collect_reverse_plan(
            plan,
            check,
            planned_writes,
            planned_removals,
        )?);
    }
    Ok(changed)
}

pub(super) fn is_nested_project_path(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".project.json") || name.ends_with(".project.jsonc")
        })
}

struct NestedSyncMutations<'a> {
    writes: &'a mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &'a mut BTreeSet<PathBuf>,
}

fn syncback_nested_owner(
    path: &Path,
    document: &SettingsBytecode,
    baseline_document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
    baseline_sources: &HashMap<String, ReverseSource>,
    check: bool,
    mutations: &mut NestedSyncMutations<'_>,
) -> Result<usize> {
    let nested = load_nested_project(path)?;
    validate_nested_project(&nested)?;
    let root_name = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .context("Nested project mount has no root instance")?;
    let baseline_root_name = baseline_document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.as_str())
        .context("Nested project baseline has no root instance")?;
    if baseline_root_name != root_name {
        bail!(
            "Nested project root changed from '{baseline_root_name}' to '{root_name}'; rename the outer mount instead"
        );
    }
    let root_target = vec![root_name];
    let root_selector = ProjectTarget::from_parts(root_target.clone(), Vec::new());
    let owners = reverse_owners(&nested)?;
    let naming = project_script_naming(&nested.project);
    let filtered = prefixed_nested_filter_project(&nested, &root_target);
    let mut external_targets = owners
        .iter()
        .map(|owner| {
            ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
                .with_prefix(&root_target)
        })
        .collect::<Vec<_>>();
    for adapter in &nested.project.adapters {
        if adapter.direction != AdapterDirection::FromProject {
            external_targets.push(adapter.target.with_prefix(&root_target));
        }
    }
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::ReadOnly)
    {
        let target = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
            .with_prefix(&root_target);
        let current = with_project_target(&target, |target| {
            extract_projection_document(document, target, &[], None)
        })?;
        let baseline = with_project_target(&target, |target| {
            extract_projection_document(baseline_document, target, &[], None)
        })?;
        if projection_document_snapshot(&current, sources)?
            != projection_document_snapshot(&baseline, baseline_sources)?
        {
            bail!(
                "Studio changed read-only nested mount '{}'; change its source or make the mount writable",
                owner.target.join(".")
            );
        }
    }
    let allowed = if nested
        .project
        .root
        .ignore_unknown_instances
        .unwrap_or(false)
    {
        Some(projection_identity_set(baseline_document, &root_target)?)
    } else {
        None
    };
    let mut output = with_project_target(&root_selector, |target| {
        extract_projection_document(document, target, &external_targets, allowed.as_ref())
    })?;
    let baseline_output = with_project_target(&root_selector, |target| {
        extract_projection_document(
            baseline_document,
            target,
            &external_targets,
            allowed.as_ref(),
        )
    })?;
    apply_reverse_filters(&filtered, &root_target, &mut output, Some(&baseline_output))?;
    let destination = nested.root.join(&nested.project.source_root);
    restore_project_owned_fields(&nested, &[], &destination, &mut output)?;
    let mut changed = usize::from(merge_nested_reverse_plan(
        plan_reverse_owner(&destination, &output, sources, &naming)?,
        check,
        &mut *mutations.writes,
        &mut *mutations.removals,
    )?);
    let mut overlay_provenance = HashMap::new();
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::Overlay)
    {
        let target = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
            .with_prefix(&root_target);
        overlay_provenance.insert(
            reverse_owner_key(owner),
            overlay_owner_provenance_at(&nested, owner, &target)?,
        );
    }
    for owner in owners {
        if owner.ownership == MountOwnership::ReadOnly {
            continue;
        }
        let target = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
            .with_prefix(&root_target);
        if owner.optional
            && with_project_target(&target, |target| {
                Ok(find_document_target_optional(document, target)?.is_none())
            })?
        {
            continue;
        }
        let allowed = if owner.ownership == MountOwnership::Overlay {
            Some(
                overlay_provenance
                    .get(&reverse_owner_key(&owner))
                    .context("Nested overlay ownership provenance disappeared")?
                    .identities
                    .clone(),
            )
        } else if owner.ignore_unknown_instances {
            Some(with_project_target(&target, |target| {
                projection_identity_set(baseline_document, target)
            })?)
        } else {
            None
        };
        let exclusions = external_targets
            .iter()
            .filter(|candidate| {
                candidate.segments().len() > target.segments().len()
                    && target_is_within(candidate, &target)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut owner_output = with_project_target(&target, |target| {
            extract_projection_document(document, target, &exclusions, allowed.as_ref())
        })?;
        let owner_baseline = with_project_target(&target, |target| {
            extract_projection_document(baseline_document, target, &exclusions, allowed.as_ref())
        })?;
        apply_reverse_filters(
            &filtered,
            &target.segments(),
            &mut owner_output,
            Some(&owner_baseline),
        )?;
        if owner.ownership == MountOwnership::Overlay {
            owner_output.instances[0] = overlay_provenance
                .get(&reverse_owner_key(&owner))
                .context("Nested overlay ownership provenance disappeared")?
                .root
                .clone();
            owner_output.instances[0].parent_index = None;
        }
        restore_project_owned_fields(&nested, &owner.target, &owner.source, &mut owner_output)?;
        if is_nested_project_path(&owner.source) {
            changed += syncback_nested_owner(
                &owner.source,
                &owner_output,
                &owner_baseline,
                sources,
                baseline_sources,
                check,
                mutations,
            )?;
        } else {
            changed += usize::from(merge_nested_reverse_plan(
                plan_reverse_owner(&owner.source, &owner_output, sources, &naming)?,
                check,
                &mut *mutations.writes,
                &mut *mutations.removals,
            )?);
        }
    }
    if nested
        .project
        .adapters
        .iter()
        .any(|adapter| adapter.direction != AdapterDirection::ToProject)
    {
        let root =
            fresh_projection_stage(&nested.root.join(".renium").join("nested-syncback"), "")?;
        let adapter_result: Result<usize> = (|| {
            let children = settings_children_by_parent(document);
            let root_index = document
                .instances
                .iter()
                .position(|instance| instance.parent_index.is_none())
                .context("Nested project mount has no root instance")?;
            for child_index in children[root_index].iter().copied() {
                let child = extract_projection_subtree(document, child_index)?;
                let child_destination = root.join(&document.instances[child_index].name);
                fs::create_dir_all(&child_destination)?;
                child.write_file(&service_settings_path(&child_destination))?;
                for (source_path, source) in
                    reverse_script_plan(&child_destination, &child, sources, &naming)?
                {
                    if let Some(parent) = source_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    atomic_write_file(&source_path, source.as_bytes())?;
                }
            }
            let adapter_plan = plan_adapter_syncback(&nested, &root)?;
            let adapter_changed =
                adapter_plan.writes.len() + usize::from(adapter_plan.baseline_changed);
            if !check {
                for (path, bytes) in adapter_plan.writes {
                    merge_nested_write(
                        path,
                        bytes,
                        &mut *mutations.writes,
                        &mut *mutations.removals,
                    )?;
                }
                if adapter_plan.baseline_changed {
                    merge_nested_write(
                        adapter_plan.baseline_path,
                        adapter_plan.baseline_bytes,
                        &mut *mutations.writes,
                        &mut *mutations.removals,
                    )?;
                }
            }
            Ok(adapter_changed)
        })();
        let cleanup = fs::remove_dir_all(&root);
        remove_empty_stage_parents(&root);
        if let Err(error) = cleanup {
            eprintln!(
                "[renium] warning: failed to remove nested syncback stage {}: {error}",
                root.display()
            );
        }
        changed += adapter_result?;
    }
    Ok(changed)
}

fn prefixed_nested_filter_project(loaded: &LoadedProject, prefix: &[String]) -> LoadedProject {
    let mut filtered = LoadedProject {
        path: loaded.path.clone(),
        root: loaded.root.clone(),
        project: loaded.project.clone(),
    };
    let escaped_prefix = escape_glob(&filter_path_segments(prefix));
    filtered.project.filters = loaded
        .project
        .filters
        .iter()
        .flat_map(|rule| {
            if let Some(glob) = rule.glob.as_deref() {
                let mut nested = rule.clone();
                nested.glob = Some(format!("{escaped_prefix}/{}", glob.trim_start_matches('/')));
                vec![nested]
            } else {
                let mut root = rule.clone();
                root.glob = Some(escaped_prefix.clone());
                let mut descendants = rule.clone();
                descendants.glob = Some(format!("{escaped_prefix}/**"));
                vec![root, descendants]
            }
        })
        .collect();
    filtered
}

fn projection_document_snapshot(
    document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
) -> Result<Vec<u8>> {
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    serde_json::to_vec(&json!({
        "document": document,
        "sources": sources
            .iter()
            .filter(|(id, _)| ids.contains(id.as_str()))
            .map(|(id, source)| (id, (&source.extension, &source.text)))
            .collect::<BTreeMap<_, _>>(),
    }))
    .context("Failed to encode nested projection snapshot")
}

fn merge_nested_write(
    path: PathBuf,
    bytes: Vec<u8>,
    planned_writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    planned_removals: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    match planned_writes.entry(path) {
        std::collections::btree_map::Entry::Occupied(entry) => {
            if entry.get() != &bytes {
                bail!(
                    "Reverse projection planned conflicting writes to {}",
                    entry.key().display()
                );
            }
            planned_removals.remove(entry.key());
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
            planned_removals.remove(entry.key());
            entry.insert(bytes);
        }
    }
    Ok(())
}

fn merge_nested_reverse_plan(
    plan: ReverseOwnerPlan,
    check: bool,
    planned_writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    planned_removals: &mut BTreeSet<PathBuf>,
) -> Result<bool> {
    if !reverse_owner_plan_differs(&plan)? {
        return Ok(false);
    }
    if !check {
        for (path, bytes) in plan.writes {
            merge_nested_write(path, bytes, planned_writes, planned_removals)?;
        }
        planned_removals.extend(plan.removals);
    }
    Ok(true)
}

fn extract_projection_subtree(
    document: &SettingsBytecode,
    root: usize,
) -> Result<SettingsBytecode> {
    if root >= document.instances.len() {
        bail!("Projected subtree root is outside the settings document");
    }
    let children = settings_children_by_parent(document);
    let mut selected = Vec::new();
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        selected.push(index);
        for child in children[index].iter().rev() {
            stack.push(*child);
        }
    }
    Ok(extracted_settings_document(document, &selected))
}

fn reverse_owners(loaded: &LoadedProject) -> Result<Vec<ReverseOwner>> {
    let mut owners = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(target, node)| {
            node.path.map(|source| ReverseOwner {
                target,
                ordinals: Vec::new(),
                source: loaded.root.join(source),
                ownership: MountOwnership::Exclusive,
                ignore_unknown_instances: node.ignore_unknown_instances.unwrap_or(false),
                optional: false,
            })
        })
        .collect::<Vec<_>>();
    for mount in &loaded.project.mounts {
        let target = target_segments(&mount.target)?;
        let source = loaded.root.join(&mount.source);
        if mount.optional && !source.exists() {
            continue;
        }
        let ignore_unknown_instances = if is_nested_project_path(&source) && source.is_file() {
            load_nested_project(&source)?
                .project
                .root
                .ignore_unknown_instances
                .unwrap_or(false)
        } else {
            false
        };
        owners.push(ReverseOwner {
            target,
            ordinals: mount.target.ordinals(),
            source,
            ownership: mount.ownership,
            ignore_unknown_instances,
            optional: mount.optional,
        });
    }
    owners.sort_by_key(|owner| owner.target.len());
    Ok(owners)
}

fn projection_identity_set(
    document: &SettingsBytecode,
    target: &[String],
) -> Result<HashSet<String>> {
    let root = find_document_target(document, target)?;
    let paths = projection_instance_path_parts(document);
    let mut output = HashSet::new();
    let children = settings_children_by_parent(document);
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        let instance = &document.instances[index];
        output.insert(instance.settings_id.clone());
        output.insert(projection_path_identity(
            &paths[index],
            &instance.class_name,
        ));
        stack.extend(children[index].iter().copied());
    }
    Ok(output)
}

struct OverlayOwnerProvenance {
    identities: HashSet<String>,
    root: SettingsBytecodeInstance,
}

fn overlay_owner_provenance(
    loaded: &LoadedProject,
    owner: &ReverseOwner,
) -> Result<OverlayOwnerProvenance> {
    let selector = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone());
    overlay_owner_provenance_at(loaded, owner, &selector)
}

fn overlay_owner_provenance_at(
    loaded: &LoadedProject,
    owner: &ReverseOwner,
    staged_target: &ProjectTarget,
) -> Result<OverlayOwnerProvenance> {
    let selector = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone());
    let mount = loaded
        .project
        .mounts
        .iter()
        .find(|mount| {
            absolute_path(&loaded.root.join(&mount.source)) == absolute_path(&owner.source)
                && targets_are_equal(&mount.target, &selector)
        })
        .context("Overlay reverse owner no longer matches a project mount")?;
    let root = fresh_projection_stage(&env::temp_dir().join("renium-overlay-owner"), "")?;
    PROJECTION_TRANSFORM_STACK.with(|stack| stack.borrow_mut().push(Vec::new()));
    PROJECTION_IDENTITY_STACK.with(|stack| stack.borrow_mut().push(HashMap::new()));
    let result = (|| {
        cache_script_naming(&root, &loaded.project);
        let mut staged_mount = mount.clone();
        staged_mount.target = staged_target.clone();
        stage_mount(loaded, &root, &staged_mount)?;
        refresh_stage_settings(&root)?;
        normalize_stage_references(&root)?;
        let target = staged_target.segments();
        let ordinals = staged_target.ordinals();
        let service = target.first().context("Overlay target has no service")?;
        let settings = service_settings_path(&root.join(service));
        let document = SettingsBytecode::read_file(&settings)?;
        with_target_parts(&target, &ordinals, |target| {
            let root_index = find_document_target(&document, target)?;
            let paths = projection_instance_path_parts(&document);
            let mut identities = projection_identity_set(&document, target)?;
            let root_instance = &document.instances[root_index];
            identities.remove(&root_instance.settings_id);
            identities.remove(&projection_path_identity(
                &paths[root_index],
                &root_instance.class_name,
            ));
            Ok(OverlayOwnerProvenance {
                identities,
                root: root_instance.clone(),
            })
        })
    })();
    PROJECTION_TRANSFORM_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
    PROJECTION_IDENTITY_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
    let cleanup = fs::remove_dir_all(&root);
    if let Err(error) = cleanup {
        eprintln!(
            "[renium] warning: failed to remove overlay stage {}: {error}",
            root.display()
        );
    }
    result
}

fn reverse_owner_key(owner: &ReverseOwner) -> String {
    serde_json::to_string(&(
        &owner.target,
        &owner.ordinals,
        path_slash(&absolute_path(&owner.source)),
    ))
    .unwrap_or_default()
}

fn projection_path_identity(path: &(Vec<String>, Vec<usize>), class_name: &str) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{class_name}",
        path.0.join("\0"),
        path.1
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn extract_projection_document(
    document: &SettingsBytecode,
    target: &[String],
    exclusions: &[ProjectTarget],
    allowed: Option<&HashSet<String>>,
) -> Result<SettingsBytecode> {
    let root = find_document_target(document, target)?;
    let target_selector =
        ProjectTarget::from_parts(target.to_vec(), active_target_ordinals(target));
    let excluded_roots = exclusions
        .iter()
        .filter(|path| target_is_within(path, &target_selector))
        .map(|path| {
            find_document_target_optional_with_ordinals(
                document,
                &path.segments(),
                &path.ordinals(),
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>();
    let paths = projection_instance_path_parts(document);
    let children = settings_children_by_parent(document);
    let mut selected = Vec::new();
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        if index != root && excluded_roots.contains(&index) {
            continue;
        }
        let instance = &document.instances[index];
        let identity_allowed = allowed.is_none_or(|identities| {
            identities.contains(&instance.settings_id)
                || identities.contains(&projection_path_identity(
                    &paths[index],
                    &instance.class_name,
                ))
        });
        if index != root && !identity_allowed {
            if let Some(identities) = allowed {
                let mut descendants = children[index].clone();
                let mut visited = HashSet::new();
                while let Some(descendant) = descendants.pop() {
                    if !visited.insert(descendant) {
                        continue;
                    }
                    let descendant_instance = &document.instances[descendant];
                    if identities.contains(&descendant_instance.settings_id)
                        || identities.contains(&projection_path_identity(
                            &paths[descendant],
                            &descendant_instance.class_name,
                        ))
                    {
                        bail!(
                            "Owned instance '{}' was moved beneath unknown instance '{}'; move it back into its owned hierarchy before syncing",
                            descendant_instance.name,
                            instance.name
                        );
                    }
                    descendants.extend(children[descendant].iter().copied());
                }
            }
            continue;
        }
        selected.push(index);
        for child in children[index].iter().rev() {
            stack.push(*child);
        }
    }
    Ok(extracted_settings_document(document, &selected))
}

fn extracted_settings_document(
    document: &SettingsBytecode,
    selected: &[usize],
) -> SettingsBytecode {
    let index_map = selected
        .iter()
        .enumerate()
        .map(|(new, old)| (*old, new))
        .collect::<HashMap<_, _>>();
    let ids = selected
        .iter()
        .map(|index| document.instances[*index].settings_id.clone())
        .collect::<HashSet<_>>();
    let mut instances = selected
        .iter()
        .map(|old| {
            let mut instance = document.instances[*old].clone();
            instance.parent_index = instance
                .parent_index
                .and_then(|parent| index_map.get(&parent).copied());
            remap_extracted_references(&mut instance.properties, &index_map, &ids);
            remap_extracted_references(&mut instance.attributes, &index_map, &ids);
            instance
        })
        .collect::<Vec<_>>();
    instances[0].parent_index = None;
    SettingsBytecode {
        version: document.version,
        instances,
    }
}

fn remap_extracted_references(
    record: &mut Map<String, Value>,
    indices: &HashMap<usize, usize>,
    ids: &HashSet<String>,
) {
    fn visit(value: &mut Value, indices: &HashMap<usize, usize>, ids: &HashSet<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, indices, ids);
                }
            }
            Value::Object(object) => {
                let reference_id = object
                    .get("settingsId")
                    .or_else(|| object.get("instanceId"))
                    .and_then(Value::as_str);
                if reference_id.is_some_and(|id| !ids.contains(id)) {
                    object.remove("instanceIndex");
                } else if let Some(old) = object
                    .get("instanceIndex")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| index.checked_sub(1))
                {
                    if let Some(new) = indices.get(&old) {
                        object.insert("instanceIndex".to_string(), json!(new + 1));
                    } else {
                        object.remove("instanceIndex");
                    }
                }
                for value in object.values_mut() {
                    visit(value, indices, ids);
                }
            }
            _ => {}
        }
    }
    for value in record.values_mut() {
        visit(value, indices, ids);
    }
}

pub(super) fn projection_instance_paths(document: &SettingsBytecode) -> Vec<Vec<String>> {
    projection_instance_path_parts(document)
        .into_iter()
        .map(|(segments, _)| segments)
        .collect()
}

pub(super) fn projection_instance_path_parts(
    document: &SettingsBytecode,
) -> Vec<(Vec<String>, Vec<usize>)> {
    projection_instance_path_parts_from_instances(&document.instances)
}

fn projection_instance_path_parts_from_instances(
    instances: &[SettingsBytecodeInstance],
) -> Vec<(Vec<String>, Vec<usize>)> {
    let mut occurrence_by_index = vec![1; instances.len()];
    let mut occurrences = HashMap::<(Option<usize>, String), usize>::new();
    for (index, instance) in instances.iter().enumerate() {
        let occurrence = occurrences
            .entry((instance.parent_index, instance.name.clone()))
            .or_insert(0);
        *occurrence += 1;
        occurrence_by_index[index] = *occurrence;
    }
    let mut paths = vec![(Vec::new(), Vec::new()); instances.len()];
    for (index, (segments, ordinals)) in paths.iter_mut().enumerate() {
        let mut path = Vec::new();
        let mut path_ordinals = Vec::new();
        let mut current = Some(index);
        let mut seen = HashSet::new();
        while let Some(value) = current {
            if value >= instances.len() || !seen.insert(value) {
                break;
            }
            path.push(instances[value].name.clone());
            path_ordinals.push(occurrence_by_index[value]);
            current = instances[value].parent_index;
        }
        path.reverse();
        path_ordinals.reverse();
        *segments = path;
        *ordinals = path_ordinals;
    }
    paths
}

type BaselineFilterEntry = (SettingsBytecodeInstance, Option<String>, String);

fn projection_filter_path(target: &[String], path: &[String]) -> String {
    let segments = if path.starts_with(target) {
        path.to_vec()
    } else {
        target
            .iter()
            .chain(path.iter().skip(1))
            .cloned()
            .collect::<Vec<_>>()
    };
    filter_path_segments(&segments)
}

fn restore_filtered_baseline_deletions(
    rules: &[FilterRule],
    document: &mut SettingsBytecode,
    baseline: Option<&SettingsBytecode>,
    baseline_by_id: &HashMap<String, BaselineFilterEntry>,
    parent_ids: &mut Vec<Option<String>>,
    allowed: &mut Vec<bool>,
) -> Result<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    let present = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut restore = HashSet::new();
    for instance in &baseline.instances {
        if present.contains(&instance.settings_id) {
            continue;
        }
        let Some((baseline_instance, _, path)) = baseline_by_id.get(&instance.settings_id) else {
            continue;
        };
        let candidate = owned_filter_candidate(baseline_instance, path.clone());
        if !filter_allows_scope(
            rules,
            FilterDirection::StudioToFiles,
            &candidate.borrowed(),
            FilterScope::Instance,
        )? {
            restore.insert(instance.settings_id.clone());
        }
    }
    let mut anchors = HashSet::new();
    let mut pending = restore.iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        let Some((_, parent, _)) = baseline_by_id.get(&id) else {
            continue;
        };
        let Some(parent) = parent else {
            continue;
        };
        if present.contains(parent) || restore.contains(parent) || !anchors.insert(parent.clone()) {
            continue;
        }
        pending.push(parent.clone());
    }
    for instance in &baseline.instances {
        let id = &instance.settings_id;
        if !restore.contains(id) && !anchors.contains(id) {
            continue;
        }
        let Some((original, parent_id, _)) = baseline_by_id.get(id) else {
            continue;
        };
        let mut output = original.clone();
        let is_anchor = anchors.contains(id) && !restore.contains(id);
        if is_anchor {
            output.properties.clear();
            output.attributes.clear();
        }
        document.instances.push(output);
        parent_ids.push(parent_id.clone());
        allowed.push(is_anchor);
    }
    Ok(())
}

fn value_references_any_id(value: &Value, ids: &HashSet<String>) -> bool {
    match value {
        Value::Array(values) => values
            .iter()
            .any(|value| value_references_any_id(value, ids)),
        Value::Object(object) => {
            let direct = object
                .get("settingsId")
                .or_else(|| object.get("instanceId"))
                .and_then(Value::as_str)
                .is_some_and(|id| ids.contains(id));
            direct
                || object
                    .values()
                    .any(|value| value_references_any_id(value, ids))
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum FilteredFieldKind {
    Property,
    Attribute,
}

impl FilteredFieldKind {
    fn scope(self, name: &str) -> FilterScope<'_> {
        match self {
            Self::Property => FilterScope::Property(name),
            Self::Attribute => FilterScope::Attribute(name),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Property => "property",
            Self::Attribute => "attribute",
        }
    }
}

fn apply_reverse_field_filters(
    rules: &[FilterRule],
    candidate: &OwnedFilterCandidate,
    baseline_candidate: Option<&OwnedFilterCandidate>,
    fields: &mut Map<String, Value>,
    baseline_fields: Option<&Map<String, Value>>,
    kind: FilteredFieldKind,
) -> Result<()> {
    let names = fields
        .keys()
        .chain(baseline_fields.into_iter().flat_map(Map::keys))
        .cloned()
        .collect::<BTreeSet<_>>();
    for name in names {
        if filter_allows_candidate_pair(
            rules,
            FilterDirection::StudioToFiles,
            candidate,
            baseline_candidate,
            kind.scope(&name),
        )? {
            continue;
        }
        if let Some(value) = baseline_fields.and_then(|fields| fields.get(&name)) {
            fields.insert(name, value.clone());
        } else {
            fields.remove(&name);
        }
    }
    Ok(())
}

fn restore_removed_reference_fields(
    fields: &mut Map<String, Value>,
    baseline_fields: Option<&Map<String, Value>>,
    removed_ids: &HashSet<String>,
    settings_id: &str,
    kind: FilteredFieldKind,
) -> Result<()> {
    let names = fields
        .iter()
        .filter_map(|(name, value)| {
            value_references_any_id(value, removed_ids).then_some(name.clone())
        })
        .collect::<Vec<_>>();
    for name in names {
        let value = baseline_fields
            .and_then(|fields| fields.get(&name))
            .filter(|value| !value_references_any_id(value, removed_ids))
            .cloned()
            .with_context(|| {
                format!(
                    "Cannot filter Studio-only reference from instance '{settings_id}' {} '{name}'; no safe baseline value exists",
                    kind.label()
                )
            })?;
        fields.insert(name, value);
    }
    Ok(())
}

fn apply_reverse_filters(
    loaded: &LoadedProject,
    target: &[String],
    document: &mut SettingsBytecode,
    baseline: Option<&SettingsBytecode>,
) -> Result<()> {
    if loaded.project.filters.is_empty() {
        return Ok(());
    }
    let baseline_by_id = baseline
        .map(|baseline| {
            let paths = projection_instance_paths(baseline);
            let reference_paths = projection_instance_path_parts(baseline);
            baseline
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| {
                    let mut instance = instance.clone();
                    stabilize_instance_references(&mut instance, &reference_paths, |index| {
                        baseline
                            .instances
                            .get(index)
                            .map(|instance| instance.settings_id.as_str())
                    });
                    let parent_id = instance
                        .parent_index
                        .and_then(|parent| baseline.instances.get(parent))
                        .map(|parent| parent.settings_id.clone());
                    let path = projection_filter_path(target, &paths[index]);
                    (instance.settings_id.clone(), (instance, parent_id, path))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut parent_ids = document
        .instances
        .iter()
        .map(|instance| {
            instance
                .parent_index
                .and_then(|parent| document.instances.get(parent))
                .map(|parent| parent.settings_id.clone())
        })
        .collect::<Vec<_>>();
    stabilize_document_references(document);
    let paths = projection_instance_paths(document);
    let mut allowed = vec![true; document.instances.len()];
    for index in 0..document.instances.len() {
        let instance = document.instances[index].clone();
        let candidate =
            owned_filter_candidate(&instance, projection_filter_path(target, &paths[index]));
        let baseline_candidate = baseline_by_id
            .get(&instance.settings_id)
            .map(|(baseline, _, path)| owned_filter_candidate(baseline, path.clone()));
        allowed[index] = filter_allows_candidate_pair(
            &loaded.project.filters,
            FilterDirection::StudioToFiles,
            &candidate,
            baseline_candidate.as_ref(),
            FilterScope::Instance,
        )?;
        if allowed[index] {
            let baseline_instance = baseline_by_id
                .get(&instance.settings_id)
                .map(|(instance, _, _)| instance);
            apply_reverse_field_filters(
                &loaded.project.filters,
                &candidate,
                baseline_candidate.as_ref(),
                &mut document.instances[index].properties,
                baseline_instance.map(|instance| &instance.properties),
                FilteredFieldKind::Property,
            )?;
            apply_reverse_field_filters(
                &loaded.project.filters,
                &candidate,
                baseline_candidate.as_ref(),
                &mut document.instances[index].attributes,
                baseline_instance.map(|instance| &instance.attributes),
                FilteredFieldKind::Attribute,
            )?;
        }
    }
    restore_filtered_baseline_deletions(
        &loaded.project.filters,
        document,
        baseline,
        &baseline_by_id,
        &mut parent_ids,
        &mut allowed,
    )?;
    let mut keep = allowed.clone();
    let indices_by_id = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for index in 0..document.instances.len() {
        if !keep[index] {
            continue;
        }
        let mut parent_id = parent_ids[index].as_deref();
        let mut seen = HashSet::new();
        while let Some(id) = parent_id {
            let Some(parent) = indices_by_id.get(id).copied() else {
                break;
            };
            if !seen.insert(parent) {
                break;
            }
            keep[parent] = true;
            parent_id = parent_ids[parent].as_deref();
        }
    }
    let mut remove = HashSet::new();
    for index in 0..document.instances.len() {
        if allowed[index] {
            continue;
        }
        let instance = &document.instances[index];
        if let Some((original, parent_id, _)) = baseline_by_id.get(&instance.settings_id) {
            document.instances[index].clone_from(original);
            parent_ids[index].clone_from(parent_id);
        } else if keep[index] {
            document.instances[index].properties.clear();
            document.instances[index].attributes.clear();
        } else {
            remove.insert(index);
        }
    }
    let removed_ids = remove
        .iter()
        .map(|index| document.instances[*index].settings_id.clone())
        .collect::<HashSet<_>>();
    if !removed_ids.is_empty() {
        for index in 0..document.instances.len() {
            if remove.contains(&index) {
                continue;
            }
            let settings_id = document.instances[index].settings_id.clone();
            let baseline_instance = baseline_by_id
                .get(&settings_id)
                .map(|(instance, _, _)| instance);
            restore_removed_reference_fields(
                &mut document.instances[index].properties,
                baseline_instance.map(|instance| &instance.properties),
                &removed_ids,
                &settings_id,
                FilteredFieldKind::Property,
            )?;
            restore_removed_reference_fields(
                &mut document.instances[index].attributes,
                baseline_instance.map(|instance| &instance.attributes),
                &removed_ids,
                &settings_id,
                FilteredFieldKind::Attribute,
            )?;
        }
    }
    let kept = (0..document.instances.len())
        .filter(|index| !remove.contains(index))
        .collect::<Vec<_>>();
    let mut instances = kept
        .iter()
        .map(|old| document.instances[*old].clone())
        .collect::<Vec<_>>();
    let kept_parent_ids = kept
        .iter()
        .map(|old| parent_ids[*old].clone())
        .collect::<Vec<_>>();
    let mut indices_by_id = HashMap::new();
    for (index, instance) in instances.iter().enumerate() {
        if indices_by_id
            .insert(instance.settings_id.clone(), index)
            .is_some()
        {
            bail!(
                "Filtered projection contains duplicate settings id '{}'",
                instance.settings_id
            );
        }
    }
    for (index, instance) in instances.iter_mut().enumerate() {
        instance.parent_index = match kept_parent_ids[index].as_deref() {
            Some(parent_id) => Some(*indices_by_id.get(parent_id).with_context(|| {
                format!(
                    "Filtered projection cannot restore parent '{}' for '{}'",
                    parent_id, instance.settings_id
                )
            })?),
            None => None,
        };
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    document.instances = instances;
    Ok(())
}

pub(super) fn stabilize_reference_indices(
    record: &mut Map<String, Value>,
    instances: &[SettingsBytecodeInstance],
) {
    let paths = projection_instance_path_parts_from_instances(instances);
    stabilize_reference_indices_with_paths(record, &paths, |index| {
        instances
            .get(index)
            .map(|instance| instance.settings_id.as_str())
    });
}

pub(super) fn stabilize_reference_indices_with_paths<'a>(
    record: &mut Map<String, Value>,
    paths: &[(Vec<String>, Vec<usize>)],
    settings_id: impl Fn(usize) -> Option<&'a str>,
) {
    crate::settings_bytecode::stabilize_reference_objects(record, |object, index| {
        if let (Some(settings_id), Some((path_segments, path_ordinals))) =
            (settings_id(index), paths.get(index))
        {
            object.insert(
                "settingsId".to_string(),
                Value::String(settings_id.to_string()),
            );
            object.insert(
                "pathSegments".to_string(),
                Value::Array(path_segments.iter().cloned().map(Value::String).collect()),
            );
            object.insert(
                "pathOrdinals".to_string(),
                Value::Array(path_ordinals.iter().map(|value| json!(value)).collect()),
            );
        }
    });
}

fn stabilize_instance_references<'a>(
    instance: &mut SettingsBytecodeInstance,
    paths: &[(Vec<String>, Vec<usize>)],
    settings_id: impl Copy + Fn(usize) -> Option<&'a str>,
) {
    stabilize_reference_indices_with_paths(&mut instance.properties, paths, settings_id);
    stabilize_reference_indices_with_paths(&mut instance.attributes, paths, settings_id);
}

fn stabilize_document_references(document: &mut SettingsBytecode) {
    let paths = projection_instance_path_parts(document);
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    for instance in &mut document.instances {
        stabilize_instance_references(instance, &paths, |index| ids.get(index).map(String::as_str));
    }
}

fn projection_owner_snapshot(
    root: &Path,
    document: &SettingsBytecode,
    target: &[String],
) -> Result<Vec<u8>> {
    let extracted = extract_projection_document(document, target, &[], None)?;
    let sources = projection_sources(root, document)?;
    let mut value = serde_json::to_value(&extracted)?;
    let source_value = extracted
        .instances
        .iter()
        .filter_map(|instance| {
            sources.get(&instance.settings_id).map(|source| {
                (
                    instance.settings_id.clone(),
                    json!([source.extension, source.text]),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    value
        .as_object_mut()
        .context("Settings snapshot must be an object")?
        .insert("sources".to_string(), serde_json::to_value(source_value)?);
    serde_json::to_vec(&value).context("Failed to encode projected owner snapshot")
}

fn projection_sources(
    root: &Path,
    document: &SettingsBytecode,
) -> Result<HashMap<String, ReverseSource>> {
    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.as_str())
        .context("Projected settings have no root")?;
    let service_dir = root.join(service);
    let paths =
        crate::editor_paths::build_editor_source_paths_by_index(document, service, &service_dir);
    let mut output = HashMap::new();
    for (index, path) in paths.into_iter().enumerate() {
        let Some(path) = path else {
            continue;
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read projected source {}", path.display()))?;
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("luau")
            .to_string();
        output.insert(
            document.instances[index].settings_id.clone(),
            ReverseSource { text, extension },
        );
    }
    Ok(output)
}

fn restore_project_owned_fields(
    loaded: &LoadedProject,
    owner_target: &[String],
    destination: &Path,
    output: &mut SettingsBytecode,
) -> Result<()> {
    let mut canonical = if destination.is_dir() {
        let settings = service_settings_path(destination);
        settings
            .is_file()
            .then(|| SettingsBytecode::read_file(&settings))
            .transpose()?
    } else {
        match destination
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("renium") if destination.is_file() => {
                Some(SettingsBytecode::read_file(destination)?)
            }
            Some("rbxm" | "rbxmx") if destination.is_file() => {
                Some(crate::rbx_model::read_settings_model_document(destination)?)
            }
            _ => None,
        }
    };
    if let Some(canonical) = canonical.as_mut() {
        stabilize_document_references(canonical);
    }
    let output_root_name = output
        .instances
        .first()
        .map(|instance| instance.name.clone())
        .context("Projected owner has no root instance")?;
    let mut id_remap = HashMap::new();
    for owner in projection_field_owners_with_root(loaded, owner_target.is_empty())? {
        if !owner.target.starts_with(owner_target) {
            continue;
        }
        let mut local_target = vec![output_root_name.clone()];
        local_target.extend(owner.target.iter().skip(owner_target.len()).cloned());
        let Some(output_index) = find_document_target_optional(output, &local_target)? else {
            continue;
        };
        let canonical_index = canonical
            .as_ref()
            .and_then(|canonical| {
                canonical.instances.iter().position(|instance| {
                    instance.settings_id == output.instances[output_index].settings_id
                })
            })
            .or(canonical
                .as_ref()
                .map(|canonical| find_document_target_optional(canonical, &local_target))
                .transpose()?
                .flatten());
        let canonical_instance = canonical_index.and_then(|index| {
            canonical
                .as_ref()
                .and_then(|canonical| canonical.instances.get(index))
        });
        let projected_id = output.instances[output_index].settings_id.clone();
        if owner.settings_id
            && let Some(canonical_instance) = canonical_instance
            && projected_id != canonical_instance.settings_id
        {
            id_remap.insert(projected_id, canonical_instance.settings_id.clone());
            output.instances[output_index]
                .settings_id
                .clone_from(&canonical_instance.settings_id);
        }
        if owner.class_name
            && let Some(canonical_instance) = canonical_instance
        {
            output.instances[output_index]
                .class_name
                .clone_from(&canonical_instance.class_name);
        }
        for property in &owner.properties {
            if let Some(value) =
                canonical_instance.and_then(|instance| instance.properties.get(property))
            {
                output.instances[output_index]
                    .properties
                    .insert(property.clone(), value.clone());
            } else {
                output.instances[output_index].properties.remove(property);
            }
        }
        for attribute in &owner.attributes {
            if let Some(value) =
                canonical_instance.and_then(|instance| instance.attributes.get(attribute))
            {
                output.instances[output_index]
                    .attributes
                    .insert(attribute.clone(), value.clone());
            } else {
                output.instances[output_index].attributes.remove(attribute);
            }
        }
        if owner.tags {
            if let Some(value) =
                canonical_instance.and_then(|instance| instance.properties.get("Tags"))
            {
                output.instances[output_index]
                    .properties
                    .insert("Tags".to_string(), value.clone());
            } else {
                output.instances[output_index].properties.remove("Tags");
            }
        }
    }
    if !id_remap.is_empty() {
        for instance in &mut output.instances {
            remap_record_reference_ids(&mut instance.properties, &id_remap);
            remap_record_reference_ids(&mut instance.attributes, &id_remap);
        }
    }
    let indices_by_id = output
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for instance in &mut output.instances {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    Ok(())
}

struct ReverseOwnerPlan {
    writes: BTreeMap<PathBuf, Vec<u8>>,
    removals: BTreeSet<PathBuf>,
}

fn plan_reverse_owner(
    destination: &Path,
    document: &SettingsBytecode,
    projected_sources: &HashMap<String, ReverseSource>,
    naming: &ProjectScriptNaming,
) -> Result<ReverseOwnerPlan> {
    let mut writes = BTreeMap::new();
    let mut removals = BTreeSet::new();
    if destination
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            matches!(extension.to_ascii_lowercase().as_str(), "rbxm" | "rbxmx")
        })
    {
        let bytes = reverse_model_bytes(destination, document, projected_sources)?;
        writes.insert(destination.to_path_buf(), bytes);
        return Ok(ReverseOwnerPlan { writes, removals });
    }
    if destination.is_file() || path_extension_is(destination, &["lua", "luau", "renium"]) {
        if path_extension_is(destination, &["renium"]) {
            writes.insert(
                destination.to_path_buf(),
                encode_settings_bytecode(document)?,
            );
            return Ok(ReverseOwnerPlan { writes, removals });
        }
        let expected = document
            .instances
            .first()
            .and_then(|instance| projected_sources.get(&instance.settings_id))
            .map(|source| source.text.as_bytes().to_vec())
            .context("Projected script owner has no source")?;
        writes.insert(destination.to_path_buf(), expected);
        return Ok(ReverseOwnerPlan { writes, removals });
    }
    let settings = service_settings_path(destination);
    writes.insert(settings, encode_settings_bytecode(document)?);
    for (path, source) in reverse_script_plan(destination, document, projected_sources, naming)? {
        writes.insert(path, source.into_bytes());
    }
    if destination.is_dir() {
        for entry in walkdir::WalkDir::new(destination) {
            let entry =
                entry.with_context(|| format!("Failed to scan {}", destination.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if (infer_source_script(&name, naming).is_some()
                || is_service_settings_file_name(&name))
                && !writes.contains_key(entry.path())
            {
                removals.insert(entry.path().to_path_buf());
            }
        }
    }
    Ok(ReverseOwnerPlan { writes, removals })
}

fn reverse_owner_plan_differs(plan: &ReverseOwnerPlan) -> Result<bool> {
    for (path, bytes) in &plan.writes {
        match fs::read(path) {
            Ok(current) if current == *bytes => {}
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()));
            }
        }
    }
    Ok(plan.removals.iter().any(|path| path.is_file()))
}

fn reverse_model_bytes(
    destination: &Path,
    document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
) -> Result<Vec<u8>> {
    let mut model = document.clone();
    restore_reverse_model_topology(destination, &mut model)?;
    for instance in &mut model.instances {
        if let Some(source) = sources.get(&instance.settings_id) {
            instance
                .properties
                .insert("Source".to_string(), Value::String(source.text.clone()));
        }
    }
    validate_settings_model_internal_references(&model, &destination.to_string_lossy())?;
    let binary = path_extension_is(destination, &["rbxm"]);
    crate::rbx_model::encode_settings_model(&model, binary)
}

fn restore_reverse_model_topology(destination: &Path, model: &mut SettingsBytecode) -> Result<()> {
    let canonical = crate::rbx_model::read_settings_model_document(destination)?;
    let canonical_roots = canonical
        .instances
        .iter()
        .filter(|instance| instance.parent_index.is_none())
        .collect::<Vec<_>>();
    if canonical_roots.len() == 1 {
        let canonical_root = canonical_roots[0];
        let root = model
            .instances
            .iter_mut()
            .find(|instance| instance.settings_id == canonical_root.settings_id)
            .context("Projected model no longer contains its canonical root")?;
        if root.parent_index.is_some() {
            bail!("Projected model root moved beneath another instance");
        }
        root.name.clone_from(&canonical_root.name);
        return Ok(());
    }
    if canonical_roots.len() < 2 {
        bail!("Canonical model has no root instances");
    }
    let wrapper = model
        .instances
        .iter()
        .position(|instance| instance.parent_index.is_none())
        .context("Projected multi-root model has no synthetic root")?;
    let canonical_ids = canonical_roots
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    for canonical_root in &canonical_roots {
        let index = model
            .instances
            .iter()
            .position(|instance| instance.settings_id == canonical_root.settings_id)
            .with_context(|| {
                format!(
                    "Projected multi-root model no longer contains root {}",
                    canonical_root.name
                )
            })?;
        if model.instances[index].parent_index != Some(wrapper) {
            bail!(
                "Projected multi-root model root '{}' moved outside its synthetic container",
                canonical_root.name
            );
        }
    }
    if canonical_ids.contains(model.instances[wrapper].settings_id.as_str()) {
        bail!("Projected multi-root model is missing its synthetic container");
    }
    model.instances.remove(wrapper);
    for instance in &mut model.instances {
        instance.parent_index = match instance.parent_index {
            Some(parent) if parent == wrapper => None,
            Some(parent) if parent > wrapper => Some(parent - 1),
            parent => parent,
        };
    }
    let indices_by_id = model
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for instance in &mut model.instances {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    Ok(())
}

fn reverse_script_plan(
    root: &Path,
    document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
    naming: &ProjectScriptNaming,
) -> Result<Vec<(PathBuf, String)>> {
    let children = settings_children_by_parent(document);
    let roots = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut context = ReverseScriptPlanContext {
        document,
        children: &children,
        sources,
        naming,
        output: &mut output,
    };
    let mut used_root_names = HashSet::new();
    let mut next_root_suffix = HashMap::new();
    for index in &roots {
        let directory = if roots.len() == 1 {
            root.to_path_buf()
        } else {
            root.join(crate::file_io::unique_child_stem(
                &document.instances[*index].name,
                &mut used_root_names,
                &mut next_root_suffix,
            ))
        };
        plan_reverse_script_node(&mut context, *index, &directory, true, None)?;
    }
    Ok(output)
}

struct ReverseScriptPlanContext<'a> {
    document: &'a SettingsBytecode,
    children: &'a [Vec<usize>],
    sources: &'a HashMap<String, ReverseSource>,
    naming: &'a ProjectScriptNaming,
    output: &'a mut Vec<(PathBuf, String)>,
}

fn plan_reverse_script_node(
    context: &mut ReverseScriptPlanContext<'_>,
    index: usize,
    parent: &Path,
    is_root: bool,
    stem: Option<&str>,
) -> Result<()> {
    let instance = &context.document.instances[index];
    let has_children = !context.children[index].is_empty();
    let source = context.sources.get(&instance.settings_id);
    let child_root = if let Some(source) = source {
        let suffix = reverse_script_suffix(instance, context.naming, &source.extension)?;
        let path = if is_root {
            parent.join(format!("init{suffix}"))
        } else if has_children {
            let directory = parent.join(stem.unwrap_or(&instance.name));
            directory.join(format!("init{suffix}"))
        } else {
            parent.join(format!("{}{suffix}", stem.unwrap_or(&instance.name)))
        };
        context.output.push((path.clone(), source.text.clone()));
        if has_children {
            path.parent().unwrap_or(parent).to_path_buf()
        } else {
            parent.to_path_buf()
        }
    } else if is_root {
        parent.to_path_buf()
    } else {
        parent.join(stem.unwrap_or(&instance.name))
    };
    let mut used_names = HashSet::new();
    let mut next_suffix = HashMap::new();
    let child_count = context.children[index].len();
    for child_position in 0..child_count {
        let child = context.children[index][child_position];
        let child_stem = crate::file_io::unique_child_stem(
            &context.document.instances[child].name,
            &mut used_names,
            &mut next_suffix,
        );
        plan_reverse_script_node(context, child, &child_root, false, Some(&child_stem))?;
    }
    Ok(())
}

fn reverse_script_suffix(
    instance: &SettingsBytecodeInstance,
    naming: &ProjectScriptNaming,
    extension: &str,
) -> Result<String> {
    let run_context = instance
        .properties
        .get("RunContext")
        .and_then(run_context_name);
    let suffix = if instance.class_name == "Script"
        && run_context.is_some_and(|value| value.eq_ignore_ascii_case("Client"))
    {
        &naming.client_run_context_suffix
    } else if instance.class_name == "Script"
        && run_context.is_some_and(|value| value.eq_ignore_ascii_case("Plugin"))
    {
        &naming.plugin_suffix
    } else {
        match instance.class_name.as_str() {
            "Script" => &naming.server_suffix,
            "LocalScript" => &naming.client_suffix,
            "ModuleScript" => &naming.module_suffix,
            class_name => bail!("{class_name} is not a script class"),
        }
    };
    Ok(format!("{suffix}.{extension}"))
}

pub fn syncback_project_adapters(loaded: &LoadedProject, check: bool) -> Result<usize> {
    let projection = stage_adapter_syncback_projection(loaded)?;
    syncback_project_adapters_from_root(loaded, projection.root(), check)
}

pub(super) fn stage_adapter_syncback_projection(loaded: &LoadedProject) -> Result<ProjectionStage> {
    let mut project = loaded.project.clone();
    project.adapters.clear();
    stage_project(&LoadedProject {
        path: loaded.path.clone(),
        root: loaded.root.clone(),
        project,
    })
}

pub fn syncback_project_adapters_from_root(
    loaded: &LoadedProject,
    source_root: &Path,
    check: bool,
) -> Result<usize> {
    let plan = plan_adapter_syncback(loaded, source_root)?;
    let changed = plan.writes.len() + usize::from(plan.baseline_changed);
    if check && changed > 0 {
        let mut changed_paths = plan
            .writes
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect::<Vec<_>>();
        if plan.baseline_changed {
            changed_paths.push(plan.baseline_path.display().to_string());
        }
        bail!(
            "Adapter source files are stale: {}",
            changed_paths.join(", ")
        );
    }
    if !check {
        let mut writes = plan.writes;
        if plan.baseline_changed {
            writes.push((plan.baseline_path, plan.baseline_bytes));
        }
        if !writes.is_empty() {
            write_file_transaction(&writes)?;
        }
    }
    Ok(changed)
}

pub(super) struct AdapterSyncbackPlan {
    pub(super) writes: Vec<(PathBuf, Vec<u8>)>,
    pub(super) baseline_path: PathBuf,
    baseline_bytes: Vec<u8>,
    pub(super) baseline_changed: bool,
}

pub(super) fn plan_adapter_syncback(
    loaded: &LoadedProject,
    source_root: &Path,
) -> Result<AdapterSyncbackPlan> {
    validate_project(loaded)?;
    let mut writes = Vec::new();
    let (baseline_path, mut baseline) = load_adapter_baseline(loaded)?;
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::ToProject {
            continue;
        }
        with_project_target(&adapter.target, |target| {
            let format = adapter_format(adapter)?;
            let service = target
                .first()
                .context("Adapter target must include a service")?;
            let settings_path = service_settings_path(&source_root.join(service));
            if !settings_path.is_file() {
                bail!(
                    "Cannot sync back adapter {} because {} does not exist",
                    adapter.target,
                    settings_path.display()
                );
            }
            let source = loaded.root.join(&adapter.source);
            let document = SettingsBytecode::read_file(&settings_path)?;
            let current = match fs::read(&source) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to read {}", source.display()));
                }
            };
            let key = adapter_key(adapter);
            let previous = baseline.entries.get(&key);
            let model_json_hierarchical = if format == AdapterFormat::ModelJson {
                Some(match current.as_deref() {
                    Some(bytes) => model_json_bytes_are_hierarchical(bytes)?,
                    None => previous
                        .and_then(|entry| entry.model_json_hierarchical)
                        .unwrap_or(true),
                })
            } else {
                None
            };
            let bytes = reversible_adapter_target_bytes(
                adapter,
                format,
                &document,
                target,
                &source,
                model_json_hierarchical,
            )?;
            let source_hash = current.as_deref().map(sha256_hex);
            let target_hash = sha256_hex(&bytes);
            let mut write_target = adapter.direction == AdapterDirection::FromProject;
            let mut update_baseline = write_target;
            if adapter.direction == AdapterDirection::TwoWay {
                if let Some(previous) = previous {
                    let source_changed = source_hash
                        .as_ref()
                        .is_none_or(|hash| *hash != previous.source_hash);
                    let target_changed = target_hash != previous.target_hash;
                    let values_match = source_hash.as_ref() == Some(&target_hash);
                    if current.is_none() {
                        if target_changed {
                            bail!(
                                "Two-way adapter conflict for '{}': {} was removed and {} changed since the last successful sync",
                                adapter.target,
                                source.display(),
                                settings_path.display()
                            );
                        }
                        write_target = true;
                        update_baseline = true;
                    } else {
                        match (source_changed, target_changed, values_match) {
                            (true, false, _) | (false, false, false) => {}
                            (false, true, _) => {
                                write_target = true;
                                update_baseline = true;
                            }
                            (true, true, false) => {
                                bail!(
                                    "Two-way adapter conflict for '{}': both {} and {} changed since the last successful sync",
                                    adapter.target,
                                    source.display(),
                                    settings_path.display()
                                );
                            }
                            (_, _, true) => update_baseline = true,
                        }
                    }
                } else {
                    write_target = true;
                    update_baseline = true;
                }
            }
            if write_target && current.as_deref() != Some(bytes.as_slice()) {
                writes.push((source, bytes));
            }
            if update_baseline {
                baseline.entries.insert(
                    key,
                    AdapterBaselineEntry {
                        source_hash: if write_target {
                            target_hash.clone()
                        } else {
                            source_hash.context("Adapter source disappeared during syncback")?
                        },
                        target_hash,
                        format: Some(format.as_str().to_string()),
                        output: previous.and_then(|entry| entry.output.clone()),
                        output_hash: previous.and_then(|entry| entry.output_hash.clone()),
                        output_owned: previous.is_some_and(|entry| entry.output_owned),
                        model_json_hierarchical,
                    },
                );
            }
            Ok(())
        })?;
    }
    let baseline_bytes = serde_json::to_vec_pretty(&baseline)?;
    let baseline_changed = read_file_if_present(&baseline_path)
        .with_context(|| format!("Failed to read {}", baseline_path.display()))?
        .as_deref()
        != Some(baseline_bytes.as_slice());
    Ok(AdapterSyncbackPlan {
        writes,
        baseline_path,
        baseline_bytes,
        baseline_changed,
    })
}

fn reversible_adapter_target_bytes(
    adapter: &AdapterSpec,
    format: AdapterFormat,
    document: &SettingsBytecode,
    target: &[String],
    source: &Path,
    model_json_hierarchical: Option<bool>,
) -> Result<Vec<u8>> {
    match format {
        AdapterFormat::Text => {
            let index = find_document_target(document, target)?;
            let instance = &document.instances[index];
            if instance.class_name != "StringValue" {
                bail!(
                    "Adapter {} targets {}, expected StringValue",
                    adapter.source.display(),
                    instance.class_name
                );
            }
            Ok(instance
                .properties
                .get("Value")
                .and_then(Value::as_str)
                .context("StringValue adapter target is missing a string Value")?
                .as_bytes()
                .to_vec())
        }
        AdapterFormat::Csv => {
            let index = find_document_target(document, target)?;
            let instance = &document.instances[index];
            if instance.class_name != "LocalizationTable" {
                bail!(
                    "Adapter {} targets {}, expected LocalizationTable",
                    adapter.source.display(),
                    instance.class_name
                );
            }
            let contents = instance
                .properties
                .get("Contents")
                .and_then(Value::as_str)
                .context("LocalizationTable adapter target is missing string Contents")?;
            localization_json_to_csv(contents)
        }
        AdapterFormat::ModelJson => {
            let hierarchical = match model_json_hierarchical {
                Some(hierarchical) => hierarchical,
                None if source.is_file() => model_json_source_is_hierarchical(source)?,
                None => true,
            };
            export_model_json(document, target, hierarchical)
        }
        _ => unreachable!("validation rejects non-reversible adapter formats"),
    }
}

pub(super) fn write_file_transaction(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    let originals = writes
        .iter()
        .map(|(path, _)| read_file_if_present(path))
        .collect::<io::Result<Vec<_>>>()?;
    for (index, (path, bytes)) in writes.iter().enumerate() {
        if let Err(error) = atomic_write_file(path, bytes) {
            let mut rollback_errors = Vec::new();
            for rollback in (0..index).rev() {
                let rollback_path = &writes[rollback].0;
                if let Err(rollback_error) =
                    restore_file_snapshot(rollback_path, originals[rollback].as_deref())
                {
                    rollback_errors.push(format!("{}: {rollback_error}", rollback_path.display()));
                }
            }
            if rollback_errors.is_empty() {
                return Err(error);
            }
            return Err(error).context(format!(
                "Adapter rollback was incomplete: {}",
                rollback_errors.join("; ")
            ));
        }
    }
    Ok(())
}

fn model_json_source_is_hierarchical(source: &Path) -> Result<bool> {
    model_json_bytes_are_hierarchical(&fs::read(source)?)
}

fn model_json_bytes_are_hierarchical(bytes: &[u8]) -> Result<bool> {
    let text = std::str::from_utf8(bytes).context("Model JSON is not UTF-8")?;
    let value = parse_jsonc_value(text)?;
    let object = value
        .as_object()
        .context("Model JSON root must be an object")?;
    Ok(!object.get("instances").is_some_and(Value::is_array))
}

fn export_model_instance_values(
    document: &SettingsBytecode,
    reference_paths: &[(Vec<String>, Vec<usize>)],
    instance: &SettingsBytecodeInstance,
) -> (Map<String, Value>, Map<String, Value>, Value) {
    let mut properties = instance.properties.clone();
    let mut attributes = instance.attributes.clone();
    let settings_id = |index: usize| {
        document
            .instances
            .get(index)
            .map(|instance| instance.settings_id.as_str())
    };
    stabilize_reference_indices_with_paths(&mut properties, reference_paths, settings_id);
    stabilize_reference_indices_with_paths(&mut attributes, reference_paths, settings_id);
    let tags = properties
        .remove("Tags")
        .unwrap_or(Value::Array(Vec::new()));
    (properties, attributes, tags)
}

fn export_model_json(
    document: &SettingsBytecode,
    target: &[String],
    hierarchical: bool,
) -> Result<Vec<u8>> {
    let target_index = find_document_target(document, target)?;
    let children = settings_children_by_parent(document);
    let reference_paths = projection_instance_path_parts(document);
    if hierarchical {
        let root = export_hierarchical_model_json_node(
            document,
            &children,
            &reference_paths,
            target_index,
            false,
        );
        return Ok((serde_json::to_string_pretty(&root)? + "\n").into_bytes());
    }
    let mut indices = Vec::new();
    let mut stack = children
        .get(target_index)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    while let Some(index) = stack.pop() {
        indices.push(index);
        if let Some(child_indices) = children.get(index) {
            stack.extend(child_indices.iter().rev().copied());
        }
    }
    let included = indices.iter().copied().collect::<BTreeSet<_>>();
    let instances = indices
        .into_iter()
        .map(|index| {
            let instance = &document.instances[index];
            let (properties, attributes, tags) =
                export_model_instance_values(document, &reference_paths, instance);
            json!({
                "id": instance.settings_id,
                "name": instance.name,
                "className": instance.class_name,
                "parentId": instance.parent_index
                    .filter(|parent| included.contains(parent))
                    .map(|parent| document.instances[parent].settings_id.clone()),
                "properties": properties,
                "attributes": attributes,
                "tags": tags,
            })
        })
        .collect::<Vec<_>>();
    Ok((serde_json::to_string_pretty(&json!({
        "schemaVersion": 1,
        "instances": instances,
    }))? + "\n")
        .into_bytes())
}

fn export_hierarchical_model_json_node(
    document: &SettingsBytecode,
    children: &[Vec<usize>],
    reference_paths: &[(Vec<String>, Vec<usize>)],
    index: usize,
    include_name: bool,
) -> Value {
    let instance = &document.instances[index];
    let (properties, attributes, tags) =
        export_model_instance_values(document, reference_paths, instance);
    let child_values = children
        .get(index)
        .into_iter()
        .flatten()
        .map(|child| {
            export_hierarchical_model_json_node(document, children, reference_paths, *child, true)
        })
        .collect::<Vec<_>>();
    let mut output = Map::from_iter([
        (
            "id".to_string(),
            Value::String(instance.settings_id.clone()),
        ),
        (
            "className".to_string(),
            Value::String(instance.class_name.clone()),
        ),
        ("properties".to_string(), Value::Object(properties)),
        ("attributes".to_string(), Value::Object(attributes)),
        ("tags".to_string(), tags),
        ("children".to_string(), Value::Array(child_values)),
    ]);
    if include_name {
        output.insert("name".to_string(), Value::String(instance.name.clone()));
    }
    Value::Object(output)
}

pub(super) fn watch_adapters(loaded: &LoadedProject, interval_ms: u64) -> Result<()> {
    let project_path = loaded.path.clone();
    let mut current = load_project(Some(&project_path), None)?;
    validate_project(&current)?;
    build_adapters(&current, false, true)?;
    let mut announced = false;
    loop {
        let inputs = adapter_watch_inputs(&current)?;
        if !announced {
            println!("Watching {} adapter inputs", inputs.len());
            announced = true;
        }
        let mut files = BTreeSet::new();
        let mut directories = BTreeSet::new();
        for input in &inputs {
            if input.is_dir() {
                directories.insert(input.clone());
            } else {
                files.insert(input.clone());
            }
        }
        let mut watcher = FileWatcher::new(4_096)?;
        watcher.set_inputs(&files, &directories)?;
        let debounce = Duration::from_millis(interval_ms.clamp(25, 60_000));
        let mut relevant = false;
        loop {
            if watcher.take_overflowed() {
                break;
            }
            let event = watcher.receiver().recv()??;
            relevant |= event.paths.iter().any(|path| {
                let path = absolute_path(path);
                inputs.iter().any(|input| {
                    let input = absolute_path(input);
                    path == input || input.is_dir() && path.starts_with(&input)
                })
            });
            if !relevant {
                continue;
            }
            while let Ok(event) = watcher.receiver().recv_timeout(debounce) {
                event?;
            }
            break;
        }
        match load_project(Some(&project_path), None).and_then(|next| {
            validate_project(&next)?;
            build_adapters(&next, false, true)?;
            Ok(next)
        }) {
            Ok(next) => current = next,
            Err(error) => eprintln!("Adapter build failed: {error:#}"),
        }
    }
}

fn adapter_watch_inputs(loaded: &LoadedProject) -> Result<BTreeSet<PathBuf>> {
    let mut inputs = BTreeSet::from([loaded.path.clone()]);
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::FromProject {
            continue;
        }
        let source = loaded.root.join(&adapter.source);
        inputs.insert(source.clone());
        if adapter_format(adapter)? == AdapterFormat::NestedProject && source.is_file() {
            let nested = load_nested_project(&source)?;
            inputs.insert(nested.path.clone());
            inputs.extend(project_source_roots(&nested)?);
        }
    }
    Ok(inputs)
}
