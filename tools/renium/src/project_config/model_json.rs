use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::file_io::service_settings_path;
use crate::settings_bytecode::{
    SettingsBytecode, SettingsBytecodeInstance, reindex_reference_indices,
};

use super::parse_jsonc_value;
use super::projection::{
    clear_stage_target_children, find_document_target, projection_settings_id,
    remap_settings_references,
};

fn validate_model_json_hierarchy(instances: &[Value]) -> Result<(Vec<String>, Vec<Option<usize>>)> {
    let mut ids = Vec::with_capacity(instances.len());
    let mut indices = HashMap::with_capacity(instances.len());
    for (index, value) in instances.iter().enumerate() {
        let instance = value
            .as_object()
            .context("Model JSON instances must be objects")?;
        let id = instance
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Model JSON instance id must be a non-empty string")?;
        if indices.insert(id.to_string(), index).is_some() {
            bail!("Model JSON contains duplicate instance id '{id}'");
        }
        ids.push(id.to_string());
    }
    let mut parents = Vec::with_capacity(instances.len());
    for value in instances {
        let instance = value
            .as_object()
            .context("Model JSON instances must be objects")?;
        let parent = match instance.get("parentId") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let id = value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .context("Model JSON parentId must be null or a non-empty string")?;
                Some(
                    indices
                        .get(id)
                        .copied()
                        .with_context(|| format!("Model JSON parent id '{id}' does not exist"))?,
                )
            }
        };
        parents.push(parent);
    }
    let mut states = vec![0_u8; instances.len()];
    for start in 0..instances.len() {
        if states[start] == 2 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(index) = current {
            match states[index] {
                0 => {
                    states[index] = 1;
                    path.push(index);
                    current = parents[index];
                }
                1 => bail!("Model JSON contains a parent cycle at '{}'", ids[index]),
                2 => break,
                _ => unreachable!("model JSON parent state is internal"),
            }
        }
        for index in path {
            states[index] = 2;
        }
    }
    Ok((ids, parents))
}

pub(super) fn stage_model_json(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    if target.len() < 2 {
        bail!("Model JSON target must be below a Studio service");
    }
    let text = fs::read_to_string(source)?;
    let value = parse_jsonc_value(&text)?;
    let object = value
        .as_object()
        .context("Model JSON root must be an object")?;
    let hierarchical_instances;
    let root_input_id;
    let instances = match object.get("instances") {
        Some(value) => {
            root_input_id = None;
            value
                .as_array()
                .context("Model JSON instances must be an array")?
        }
        None => {
            hierarchical_instances = flatten_rojo_model_json(object, target)?;
            root_input_id = hierarchical_instances
                .first()
                .and_then(Value::as_object)
                .and_then(|instance| instance.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            &hierarchical_instances
        }
    };
    let (input_ids, _) = validate_model_json_hierarchy(instances)?;
    clear_stage_target_children(stage, target)?;
    let service = target
        .first()
        .context("Model JSON target must include a service")?;
    let settings_path = service_settings_path(&stage.join(service));
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    let target_index = find_document_target(&document, target)?;
    let mut used_ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<BTreeSet<_>>();
    let mut output_ids = HashMap::new();
    if let Some(root_id) = root_input_id.as_deref() {
        let previous = document.instances[target_index].settings_id.clone();
        used_ids.remove(&previous);
        if !used_ids.insert(root_id.to_string()) {
            bail!(
                "Model JSON root id '{root_id}' collides with an instance outside the adapter target"
            );
        }
        document.instances[target_index].settings_id = root_id.to_string();
        if previous != root_id {
            let remap = HashMap::from([(previous, root_id.to_string())]);
            for instance in &mut document.instances {
                remap_settings_references(&mut instance.properties, &remap);
                remap_settings_references(&mut instance.attributes, &remap);
            }
        }
    }
    for id in &input_ids {
        if root_input_id.as_deref() == Some(id.as_str()) {
            output_ids.insert(id.clone(), id.clone());
            continue;
        }
        if !used_ids.insert(id.clone()) {
            bail!(
                "Model JSON instance id '{id}' collides with an instance outside the adapter target"
            );
        }
        output_ids.insert(id.clone(), id.clone());
    }
    let first_output_index = document.instances.len();
    let mut output_indices = HashMap::new();
    let mut next_output_index = first_output_index;
    for id in &input_ids {
        if root_input_id.as_deref() == Some(id.as_str()) {
            output_indices.insert(id.clone(), target_index);
        } else {
            output_indices.insert(id.clone(), next_output_index);
            next_output_index += 1;
        }
    }
    for (offset, value) in instances.iter().enumerate() {
        let instance = value
            .as_object()
            .context("Model JSON instances must be objects")?;
        let id = &input_ids[offset];
        let name = instance
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Model JSON instance name must be a non-empty string")?;
        let class_name = match instance.get("className") {
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("Model JSON instance '{id}' className must be a non-empty string")
                })?,
            None => "Folder",
        };
        let mut properties = match instance.get("properties") {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{id}' properties must be an object")
            })?,
            None => Map::new(),
        };
        let mut attributes = match instance.get("attributes") {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{id}' attributes must be an object")
            })?,
            None => Map::new(),
        };
        stabilize_model_json_reference_indices(&mut properties, &input_ids)?;
        stabilize_model_json_reference_indices(&mut attributes, &input_ids)?;
        remap_settings_references(&mut properties, &output_ids);
        remap_settings_references(&mut attributes, &output_ids);
        let mut properties = normalize_model_property_map(Some(class_name), &properties)
            .with_context(|| format!("Invalid properties on model JSON instance '{id}'"))?;
        let attributes = normalize_model_property_map(None, &attributes)
            .with_context(|| format!("Invalid attributes on model JSON instance '{id}'"))?;
        let tags = match instance.get("tags") {
            Some(value) => value
                .as_array()
                .with_context(|| format!("Model JSON instance '{id}' tags must be an array"))?
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .with_context(|| {
                            format!(
                                "Model JSON instance '{id}' tag {index} must be a non-empty string"
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        if !tags.is_empty() {
            properties.insert(
                "Tags".to_string(),
                Value::Array(
                    tags.into_iter()
                        .map(|tag| Value::String(tag.to_string()))
                        .collect(),
                ),
            );
        }
        let parent_index = instance
            .get("parentId")
            .and_then(Value::as_str)
            .map(|parent| output_indices[parent])
            .unwrap_or(target_index);
        if root_input_id.as_deref() == Some(id.as_str()) {
            let root = &mut document.instances[target_index];
            root.class_name = class_name.to_string();
            root.properties = properties;
            root.attributes = attributes;
        } else {
            document.instances.push(SettingsBytecodeInstance {
                settings_id: output_ids[id].clone(),
                name: name.to_string(),
                class_name: class_name.to_string(),
                parent_index: Some(parent_index),
                properties,
                attributes,
            });
        }
    }
    let indices_by_id = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    if root_input_id.is_some() {
        reindex_reference_indices(
            &mut document.instances[target_index].properties,
            &indices_by_id,
        );
        reindex_reference_indices(
            &mut document.instances[target_index].attributes,
            &indices_by_id,
        );
    }
    for instance in &mut document.instances[first_output_index..] {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    document.write_file(&settings_path)
}

fn flatten_rojo_model_json(root: &Map<String, Value>, target: &[String]) -> Result<Vec<Value>> {
    fn field<'a>(
        object: &'a Map<String, Value>,
        lower: &str,
        upper: &str,
        path: &str,
    ) -> Result<Option<&'a Value>> {
        if object.contains_key(lower) && object.contains_key(upper) {
            bail!("Model JSON instance '{path}' declares both {lower} and {upper}");
        }
        Ok(object.get(lower).or_else(|| object.get(upper)))
    }

    fn visit(
        object: &Map<String, Value>,
        name: String,
        parent_id: Option<&str>,
        path: &str,
        output: &mut Vec<Value>,
    ) -> Result<()> {
        if object.contains_key("id") && object.contains_key("$id") {
            bail!("Model JSON instance '{path}' declares both id and $id");
        }
        let id = match object.get("id").or_else(|| object.get("$id")) {
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("Model JSON instance '{path}' id must be a non-empty string")
                })?
                .to_string(),
            None => projection_settings_id("rojo-model-json", path),
        };
        let class_name = match field(object, "className", "ClassName", path)? {
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("Model JSON instance '{path}' className must be a non-empty string")
                })?,
            None => "Folder",
        };
        let properties = match field(object, "properties", "Properties", path)? {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{path}' properties must be an object")
            })?,
            None => Map::new(),
        };
        let attributes = match field(object, "attributes", "Attributes", path)? {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{path}' attributes must be an object")
            })?,
            None => Map::new(),
        };
        let tags = match field(object, "tags", "Tags", path)? {
            Some(value) => value
                .as_array()
                .with_context(|| format!("Model JSON instance '{path}' tags must be an array"))?
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .map(|value| Value::String(value.to_string()))
                        .with_context(|| {
                            format!(
                                "Model JSON instance '{path}' tag {index} must be a non-empty string"
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        output.push(json!({
            "id": id,
            "name": name,
            "className": class_name,
            "parentId": parent_id,
            "properties": properties,
            "attributes": attributes,
            "tags": tags,
        }));
        if let Some(children) = field(object, "children", "Children", path)? {
            let children = children
                .as_array()
                .context("Model JSON Children must be an array")?;
            for (index, child) in children.iter().enumerate() {
                let child = child
                    .as_object()
                    .context("Model JSON children must be objects")?;
                let child_path = format!("{path}/child[{index}]");
                let child_name = field(child, "name", "Name", &child_path)?
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .with_context(|| {
                        format!("Model JSON child {index} name must be a non-empty string")
                    })?;
                visit(
                    child,
                    child_name.to_string(),
                    Some(&id),
                    &format!("{path}/{index}:{child_name}"),
                    output,
                )?;
            }
        }
        Ok(())
    }

    let name = target
        .last()
        .cloned()
        .context("Model JSON target has no name")?;
    let mut output = Vec::new();
    visit(root, name, None, &target.join("/"), &mut output)?;
    Ok(output)
}

fn stabilize_model_json_reference_indices(
    record: &mut Map<String, Value>,
    ids: &[String],
) -> Result<()> {
    fn selector(object: &Map<String, Value>, name: &str) -> Result<Option<String>> {
        object
            .get(name)
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .with_context(|| format!("Ref {name} must be a non-empty string"))
            })
            .transpose()
    }

    fn visit(value: &mut Value, ids: &[String]) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, ids)?;
                }
            }
            Value::Object(object) => {
                let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId")
                    || object.contains_key("instanceIndex");
                if is_reference {
                    let mut resolved = selector(object, "settingsId")?;
                    if let Some(instance_id) = selector(object, "instanceId")? {
                        if resolved.as_ref().is_some_and(|id| id != &instance_id) {
                            bail!("Ref settingsId and instanceId identify different instances");
                        }
                        resolved = Some(instance_id);
                    }
                    if let Some(value) = object.get("instanceIndex") {
                        let index = value
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .and_then(|index| index.checked_sub(1))
                            .context("Ref instanceIndex must be a valid 1-based index")?;
                        let id = ids.get(index).with_context(|| {
                            format!("Ref instanceIndex {} is out of range", index + 1)
                        })?;
                        if resolved.as_ref().is_some_and(|resolved| resolved != id) {
                            bail!("Ref stable id and instanceIndex identify different instances");
                        }
                        resolved = Some(id.clone());
                    }
                    if let Some(id) = resolved {
                        object.insert("settingsId".to_string(), Value::String(id));
                        object.remove("instanceId");
                    }
                    object.remove("instanceIndex");
                }
                for value in object.values_mut() {
                    visit(value, ids)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    for value in record.values_mut() {
        visit(value, ids)?;
    }
    Ok(())
}

fn normalize_model_property_map(
    class_name: Option<&str>,
    values: &Map<String, Value>,
) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|(name, value)| {
            let normalized = if contains_reference_value(value) {
                value.clone()
            } else {
                crate::rbx_encode::normalize_project_typed_value(class_name, Some(name), value)
                    .with_context(|| format!("Invalid value for '{name}'"))?
            };
            Ok((name.clone(), normalized))
        })
        .collect()
}

pub(super) fn contains_reference_value(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_reference_value),
        Value::Object(object) => {
            object.get("_type").and_then(Value::as_str) == Some("Ref")
                || object.contains_key("instanceIndex")
                || object.contains_key("settingsId")
                || object.contains_key("instanceId")
                || object.values().any(contains_reference_value)
        }
        _ => false,
    }
}
