use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::file_io::service_settings_path;
use crate::settings_bytecode::SettingsBytecode;

use super::syncback::projection_instance_path_parts;

pub(super) fn normalize_stage_references(stage: &Path) -> Result<()> {
    let mut paths = Vec::new();
    let mut documents = Vec::new();
    for entry in fs::read_dir(stage)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let settings_path = service_settings_path(&entry.path());
        if settings_path.is_file() {
            documents.push(SettingsBytecode::read_file(&settings_path)?);
            paths.push(settings_path);
        }
    }
    canonicalize_projection_references(&mut documents)?;
    for (settings_path, document) in paths.iter().zip(&documents) {
        document.write_file(settings_path)?;
    }
    Ok(())
}

struct ProjectionReferenceTarget {
    settings_id: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
}

fn canonicalize_projection_references(documents: &mut [SettingsBytecode]) -> Result<()> {
    let mut targets = Vec::<ProjectionReferenceTarget>::new();
    let mut target_by_document_instance = Vec::with_capacity(documents.len());
    let mut by_settings_id = HashMap::<String, Vec<usize>>::new();
    let mut by_path_segments = HashMap::<Vec<String>, Vec<usize>>::new();
    let mut by_path_parts = HashMap::<(Vec<String>, Vec<usize>), usize>::new();
    for document in documents.iter() {
        let parts = projection_instance_path_parts(document);
        let mut target_indices = Vec::with_capacity(document.instances.len());
        for (instance_index, instance) in document.instances.iter().enumerate() {
            let (path_segments, path_ordinals) = parts[instance_index].clone();
            let target_index = targets.len();
            targets.push(ProjectionReferenceTarget {
                settings_id: instance.settings_id.clone(),
                path_segments: path_segments.clone(),
                path_ordinals: path_ordinals.clone(),
            });
            target_indices.push(target_index);
            by_settings_id
                .entry(instance.settings_id.clone())
                .or_default()
                .push(target_index);
            by_path_segments
                .entry(path_segments.clone())
                .or_default()
                .push(target_index);
            if by_path_parts
                .insert((path_segments, path_ordinals), target_index)
                .is_some()
            {
                bail!("Projected DataModel contains duplicate structured instance paths");
            }
        }
        target_by_document_instance.push(target_indices);
    }

    for (document_index, document) in documents.iter_mut().enumerate() {
        for instance in &mut document.instances {
            canonicalize_record_references(
                &mut instance.properties,
                document_index,
                &targets,
                &target_by_document_instance,
                &by_settings_id,
                &by_path_segments,
                &by_path_parts,
            )
            .with_context(|| {
                format!(
                    "Invalid reference on {} ({})",
                    instance.name, instance.settings_id
                )
            })?;
            canonicalize_record_references(
                &mut instance.attributes,
                document_index,
                &targets,
                &target_by_document_instance,
                &by_settings_id,
                &by_path_segments,
                &by_path_parts,
            )
            .with_context(|| {
                format!(
                    "Invalid attribute reference on {} ({})",
                    instance.name, instance.settings_id
                )
            })?;
        }
    }
    Ok(())
}

pub(super) fn canonicalize_projection_document_map(
    documents: &mut HashMap<String, SettingsBytecode>,
) -> Result<()> {
    let mut entries = documents.drain().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let (keys, mut values): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
    canonicalize_projection_references(&mut values)?;
    documents.extend(keys.into_iter().zip(values));
    Ok(())
}

fn canonicalize_record_references(
    record: &mut Map<String, Value>,
    document_index: usize,
    targets: &[ProjectionReferenceTarget],
    target_by_document_instance: &[Vec<usize>],
    by_settings_id: &HashMap<String, Vec<usize>>,
    by_path_segments: &HashMap<Vec<String>, Vec<usize>>,
    by_path_parts: &HashMap<(Vec<String>, Vec<usize>), usize>,
) -> Result<()> {
    struct ReferenceIndex<'a> {
        document_index: usize,
        targets: &'a [ProjectionReferenceTarget],
        target_by_document_instance: &'a [Vec<usize>],
        by_settings_id: &'a HashMap<String, Vec<usize>>,
        by_path_segments: &'a HashMap<Vec<String>, Vec<usize>>,
        by_path_parts: &'a HashMap<(Vec<String>, Vec<usize>), usize>,
    }

    fn visit(value: &mut Value, force_reference: bool, index: &ReferenceIndex<'_>) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, false, index)?;
                }
            }
            Value::Object(object) => {
                let is_reference = force_reference
                    || object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId")
                    || object.contains_key("instanceIndex")
                    || object.contains_key("referent")
                    || object.contains_key("ref")
                    || object.contains_key("debugId")
                    || object.contains_key("pathSegments")
                    || object.contains_key("pathOrdinals")
                    || object.contains_key("path");
                if is_reference {
                    let mut constraints = Vec::<HashSet<usize>>::new();
                    let mut selector_present = false;
                    if let Some(raw_index) = object.get("instanceIndex") {
                        selector_present = true;
                        let instance_index = raw_index
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .and_then(|index| index.checked_sub(1))
                            .context("Reference instanceIndex must be a positive integer")?;
                        let target = index
                            .target_by_document_instance
                            .get(index.document_index)
                            .and_then(|indices| indices.get(instance_index))
                            .copied()
                            .context("Reference instanceIndex does not exist")?;
                        constraints.push(HashSet::from([target]));
                    }
                    if let Some(raw_settings_id) = object.get("settingsId") {
                        selector_present = true;
                        let settings_id = raw_settings_id
                            .as_str()
                            .context("Reference settingsId must be a string")?;
                        constraints.push(
                            index
                                .by_settings_id
                                .get(settings_id)
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect(),
                        );
                    }
                    for key in ["instanceId", "referent", "ref"] {
                        if let Some(raw_id) = object.get(key) {
                            selector_present = true;
                            let id = raw_id
                                .as_str()
                                .with_context(|| format!("Reference {key} must be a string"))?;
                            constraints.push(
                                index
                                    .by_settings_id
                                    .get(id)
                                    .cloned()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .collect(),
                            );
                        }
                    }
                    if let Some(raw_debug_id) = object.get("debugId") {
                        selector_present = true;
                        let debug_id = raw_debug_id
                            .as_str()
                            .context("Reference debugId must be a string")?;
                        constraints.push(
                            index
                                .by_settings_id
                                .get(&format!("debug:{debug_id}"))
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect(),
                        );
                    }
                    if let Some(raw_path_values) = object.get("pathSegments") {
                        selector_present = true;
                        let path_values = raw_path_values
                            .as_array()
                            .context("Reference pathSegments must be an array")?;
                        let segments = path_values
                            .iter()
                            .map(|segment| {
                                segment
                                    .as_str()
                                    .map(str::to_string)
                                    .context("Reference pathSegments must contain strings")
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let candidates = if let Some(raw_ordinal_values) =
                            object.get("pathOrdinals")
                        {
                            let ordinal_values = raw_ordinal_values
                                .as_array()
                                .context("Reference pathOrdinals must be an array")?;
                            let ordinals = ordinal_values
                                .iter()
                                .map(|value| {
                                    value
                                        .as_u64()
                                        .filter(|value| *value > 0)
                                        .and_then(|value| usize::try_from(value).ok())
                                        .context(
                                            "Reference pathOrdinals must contain positive integers",
                                        )
                                })
                                .collect::<Result<Vec<_>>>()?;
                            if ordinals.len() != segments.len() {
                                bail!(
                                    "Reference pathOrdinals must contain one value per path segment"
                                );
                            }
                            index
                                .by_path_parts
                                .get(&(segments, ordinals))
                                .copied()
                                .into_iter()
                                .collect()
                        } else {
                            index
                                .by_path_segments
                                .get(&segments)
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect()
                        };
                        constraints.push(candidates);
                    } else if object.contains_key("pathOrdinals") {
                        bail!("Reference pathOrdinals require pathSegments");
                    }
                    if object.contains_key("path") {
                        bail!("Reference path is unsupported; use pathSegments and pathOrdinals");
                    }
                    if constraints.is_empty() {
                        if selector_present {
                            bail!("Reference target does not exist in the projected DataModel");
                        }
                    } else {
                        let mut candidates = constraints.remove(0);
                        for constraint in constraints {
                            candidates.retain(|candidate| constraint.contains(candidate));
                        }
                        if candidates.is_empty() {
                            bail!("Reference selectors do not identify the same instance");
                        }
                        if candidates.len() != 1 {
                            bail!("Reference target is ambiguous; include pathOrdinals");
                        }
                        let target = &index.targets[*candidates
                            .iter()
                            .next()
                            .expect("reference candidate count was validated")];
                        object.remove("instanceIndex");
                        object.remove("instanceId");
                        object.remove("debugId");
                        object.remove("path");
                        object.remove("referent");
                        object.remove("ref");
                        object.insert(
                            "settingsId".to_string(),
                            Value::String(target.settings_id.clone()),
                        );
                        object.insert(
                            "pathSegments".to_string(),
                            Value::Array(
                                target
                                    .path_segments
                                    .iter()
                                    .cloned()
                                    .map(Value::String)
                                    .collect(),
                            ),
                        );
                        object.insert(
                            "pathOrdinals".to_string(),
                            Value::Array(
                                target
                                    .path_ordinals
                                    .iter()
                                    .map(|value| json!(value))
                                    .collect(),
                            ),
                        );
                    }
                }
                for (key, value) in object.iter_mut() {
                    visit(value, key == "Ref", index)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let index = ReferenceIndex {
        document_index,
        targets,
        target_by_document_instance,
        by_settings_id,
        by_path_segments,
        by_path_parts,
    };
    for value in record.values_mut() {
        visit(value, false, &index)?;
    }
    Ok(())
}
