use anyhow::{Result, bail};
use std::collections::HashSet;

use serde_json::{Map, Number, Value};

use crate::settings_bytecode::{SettingsBytecode, SettingsBytecodeInstance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyScope {
    Auto,
    Metadata,
    Property,
    Attribute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstanceSelector<'a> {
    Index(usize),
    SettingsId(&'a str),
    Name(&'a str),
    ClassName(&'a str),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct InstanceQuery {
    pub(crate) name: Option<String>,
    pub(crate) class_name: Option<String>,
    pub(crate) parent_settings_id: Option<String>,
    pub(crate) tag: Option<String>,
    pub(crate) properties: Vec<PropertyPredicate>,
    pub(crate) attributes: Vec<PropertyPredicate>,
}

#[derive(Debug, Clone)]
pub(crate) struct PropertyPredicate {
    pub(crate) name: String,
    pub(crate) value: Option<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct AddInstanceSpec {
    pub(crate) settings_id: Option<String>,
    pub(crate) name: String,
    pub(crate) class_name: String,
    pub(crate) parent_index: Option<usize>,
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct AddedInstance {
    pub(crate) index: usize,
    pub(crate) settings_id: String,
}

impl PropertyPredicate {
    pub(crate) fn exists(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
        }
    }

    pub(crate) fn equals(name: impl Into<String>, value: Value) -> Self {
        Self {
            name: name.into(),
            value: Some(value),
        }
    }
}

#[cfg(test)]
fn set_property(
    document: &mut SettingsBytecode,
    selector: InstanceSelector<'_>,
    property_name: &str,
    value: Value,
    scope: PropertyScope,
) -> Result<()> {
    let index = find_unique_instance_index(document, selector)?
        .ok_or_else(|| anyhow::anyhow!("No matching instance"))?;
    set_instance_property(document, index, property_name, value, scope)
}

pub(crate) fn find_instances(document: &SettingsBytecode, query: &InstanceQuery) -> Vec<usize> {
    document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| matches_query(document, instance, query).then_some(index))
        .collect()
}

pub(crate) fn add_instance(
    document: &mut SettingsBytecode,
    spec: AddInstanceSpec,
) -> Result<AddedInstance> {
    if spec.name.is_empty() {
        bail!("Instance name cannot be empty");
    }
    if spec.class_name.is_empty() {
        bail!("ClassName cannot be empty");
    }
    if let Some(parent_index) = spec.parent_index
        && parent_index >= document.instances.len()
    {
        bail!("Parent index {parent_index} is out of range");
    }
    if document.instances.is_empty() {
        if spec.parent_index.is_some() {
            bail!("The first instance in a settings store must be its root");
        }
    } else if spec.parent_index.is_none() {
        bail!("Only an empty settings store can accept an instance without a parent");
    }

    let settings_id = match spec.settings_id {
        Some(settings_id) if !settings_id.is_empty() => {
            if document
                .instances
                .iter()
                .any(|instance| instance.settings_id == settings_id)
            {
                bail!("settingsId already exists: {settings_id}");
            }
            settings_id
        }
        _ => next_editor_settings_id(document),
    };

    let index = document.instances.len();
    document.instances.push(SettingsBytecodeInstance {
        settings_id: settings_id.clone(),
        name: spec.name,
        class_name: spec.class_name,
        parent_index: spec.parent_index,
        properties: spec.properties,
        attributes: spec.attributes,
    });
    Ok(AddedInstance { index, settings_id })
}

pub(crate) fn remove_instance(
    document: &mut SettingsBytecode,
    selector: InstanceSelector<'_>,
    recursive: bool,
) -> Result<Vec<usize>> {
    let target_index = find_unique_instance_index(document, selector)?
        .ok_or_else(|| anyhow::anyhow!("No matching instance"))?;
    remove_instances_at_indices(document, &[target_index], recursive)
}

pub(crate) fn remove_instances_at_indices(
    document: &mut SettingsBytecode,
    target_indices: &[usize],
    recursive: bool,
) -> Result<Vec<usize>> {
    if target_indices.is_empty() {
        bail!("No matching instances");
    }
    for target_index in target_indices {
        let Some(instance) = document.instances.get(*target_index) else {
            bail!("Instance index {target_index} is out of range");
        };
        if instance.parent_index.is_none() {
            bail!("Refusing to remove root instance");
        }
    }

    let mut children_by_parent = vec![Vec::new(); document.instances.len()];
    for (index, instance) in document.instances.iter().enumerate() {
        if let Some(parent_index) = instance.parent_index
            && let Some(children) = children_by_parent.get_mut(parent_index)
        {
            children.push(index);
        }
    }

    let mut remove_set = target_indices.iter().copied().collect::<HashSet<_>>();
    let mut stack = target_indices.to_vec();
    while let Some(parent_index) = stack.pop() {
        for child_index in children_by_parent
            .get(parent_index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if !recursive {
                bail!("Instance has descendants; pass recursive removal");
            }
            if remove_set.insert(*child_index) {
                stack.push(*child_index);
            }
        }
    }

    let mut old_to_new = vec![None; document.instances.len()];
    let mut next_index = 0usize;
    for (index, mapped_index) in old_to_new.iter_mut().enumerate() {
        if remove_set.contains(&index) {
            continue;
        }
        *mapped_index = Some(next_index);
        next_index += 1;
    }

    let mut next_instances = Vec::with_capacity(next_index);
    for (index, mut instance) in document.instances.drain(..).enumerate() {
        if remove_set.contains(&index) {
            continue;
        }
        instance.parent_index = instance
            .parent_index
            .and_then(|parent_index| old_to_new.get(parent_index).copied().flatten());
        next_instances.push(instance);
    }
    document.instances = next_instances;
    remap_ref_indices(document, &old_to_new);

    let mut removed = remove_set.into_iter().collect::<Vec<_>>();
    removed.sort_unstable();
    Ok(removed)
}

pub(crate) fn remap_ref_indices(
    document: &mut SettingsBytecode,
    old_to_new: &[Option<usize>],
) -> usize {
    let mut changed = 0usize;
    for instance in &mut document.instances {
        for value in instance.properties.values_mut() {
            changed += remap_ref_indices_in_value(value, old_to_new);
        }
        for value in instance.attributes.values_mut() {
            changed += remap_ref_indices_in_value(value, old_to_new);
        }
    }
    changed
}

fn remap_ref_indices_in_value(value: &mut Value, old_to_new: &[Option<usize>]) -> usize {
    let mut changed = 0usize;
    match value {
        Value::Array(items) => {
            for item in items {
                changed += remap_ref_indices_in_value(item, old_to_new);
            }
        }
        Value::Object(object) => {
            if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                changed += remap_ref_object_index(object, old_to_new) as usize;
            }
            if let Some(Value::Object(ref_object)) = object.get_mut("Ref") {
                changed += remap_ref_object_index(ref_object, old_to_new) as usize;
            }
            for item in object.values_mut() {
                changed += remap_ref_indices_in_value(item, old_to_new);
            }
        }
        _ => {}
    }
    changed
}

fn remap_ref_object_index(object: &mut Map<String, Value>, old_to_new: &[Option<usize>]) -> bool {
    let Some(old_index) = object
        .get("instanceIndex")
        .and_then(settings_ref_index_to_document_index)
    else {
        return false;
    };

    match old_to_new.get(old_index).copied().flatten() {
        Some(new_index) => {
            if old_index == new_index {
                return false;
            }
            object.insert(
                "instanceIndex".to_string(),
                Value::Number(Number::from((new_index + 1) as u64)),
            );
            true
        }
        None => {
            let mut removed = false;
            for selector in [
                "instanceIndex",
                "settingsId",
                "instanceId",
                "pathSegments",
                "pathOrdinals",
                "debugId",
                "path",
                "referent",
                "ref",
            ] {
                removed = object.remove(selector).is_some() || removed;
            }
            removed
        }
    }
}

fn settings_ref_index_to_document_index(value: &Value) -> Option<usize> {
    let one_based = value
        .as_u64()
        .or_else(|| {
            value
                .as_i64()
                .and_then(|number| (number >= 0).then_some(number as u64))
        })
        .or_else(|| {
            value.as_f64().and_then(|number| {
                number
                    .is_finite()
                    .then_some(number.trunc())
                    .filter(|truncated| (*truncated - number).abs() < f64::EPSILON)
                    .and_then(|truncated| (truncated >= 0.0).then_some(truncated as u64))
            })
        })?;
    usize::try_from(one_based).ok()?.checked_sub(1)
}

pub(crate) fn find_unique_instance_index(
    document: &SettingsBytecode,
    selector: InstanceSelector<'_>,
) -> Result<Option<usize>> {
    match selector {
        InstanceSelector::Index(index) => Ok((index < document.instances.len()).then_some(index)),
        InstanceSelector::SettingsId(settings_id) => Ok(document
            .instances
            .iter()
            .position(|instance| instance.settings_id == settings_id)),
        InstanceSelector::Name(name) => unique_position(
            document,
            |instance| instance.name == name,
            &format!("name {name:?}"),
        ),
        InstanceSelector::ClassName(class_name) => unique_position(
            document,
            |instance| instance.class_name == class_name,
            &format!("className {class_name:?}"),
        ),
    }
}

fn unique_position(
    document: &SettingsBytecode,
    mut matches: impl FnMut(&SettingsBytecodeInstance) -> bool,
    label: &str,
) -> Result<Option<usize>> {
    let mut found = None;
    let mut duplicates = Vec::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if !matches(instance) {
            continue;
        }
        duplicates.push(index);
        if found.is_none() {
            found = Some(index);
        } else if duplicates.len() >= 6 {
            break;
        }
    }
    if duplicates.len() > 1 {
        bail!(
            "Ambiguous selector {label}; matched multiple instances at indexes {:?}. Use --index/-x or --id/-i.",
            duplicates
        );
    }
    Ok(found)
}

pub(crate) fn get_instance_property(
    document: &SettingsBytecode,
    index: usize,
    property_name: &str,
    scope: PropertyScope,
) -> Option<Value> {
    let instance = document.instances.get(index)?;
    match scope {
        PropertyScope::Metadata => metadata_value(document, instance, property_name),
        PropertyScope::Property => instance.properties.get(property_name).cloned(),
        PropertyScope::Attribute => instance.attributes.get(property_name).cloned(),
        PropertyScope::Auto => metadata_value(document, instance, property_name)
            .or_else(|| instance.properties.get(property_name).cloned())
            .or_else(|| instance.attributes.get(property_name).cloned()),
    }
}

pub(crate) fn set_instance_property(
    document: &mut SettingsBytecode,
    index: usize,
    property_name: &str,
    value: Value,
    scope: PropertyScope,
) -> Result<()> {
    if index >= document.instances.len() {
        bail!("Invalid instance index {index}");
    }

    let resolved_parent_index = if matches!(scope, PropertyScope::Auto | PropertyScope::Metadata)
        && property_name == "Parent"
    {
        let resolved = resolve_parent_index(document, &value)?;
        let is_root = document.instances[index].parent_index.is_none();
        if is_root && resolved.is_some() {
            bail!("The service root cannot be moved under another instance");
        }
        if !is_root && resolved.is_none() {
            bail!("Only the service root can have no parent");
        }
        if let Some(parent_index) = resolved {
            ensure_parent_is_not_descendant(document, index, parent_index)?;
        }
        Some(resolved)
    } else {
        None
    };

    let instance = &mut document.instances[index];
    match scope {
        PropertyScope::Metadata => {
            set_metadata_value(instance, property_name, value, resolved_parent_index)
        }
        PropertyScope::Property => {
            set_map_value(&mut instance.properties, property_name, value);
            Ok(())
        }
        PropertyScope::Attribute => {
            set_map_value(&mut instance.attributes, property_name, value);
            Ok(())
        }
        PropertyScope::Auto => {
            if is_metadata_property(property_name) {
                return set_metadata_value(instance, property_name, value, resolved_parent_index);
            }
            if instance.attributes.contains_key(property_name) {
                set_map_value(&mut instance.attributes, property_name, value);
            } else {
                set_map_value(&mut instance.properties, property_name, value);
            }
            Ok(())
        }
    }
}

fn matches_query(
    document: &SettingsBytecode,
    instance: &SettingsBytecodeInstance,
    query: &InstanceQuery,
) -> bool {
    if query
        .name
        .as_deref()
        .is_some_and(|name| instance.name != name)
    {
        return false;
    }
    if query
        .class_name
        .as_deref()
        .is_some_and(|class_name| instance.class_name != class_name)
    {
        return false;
    }
    if query
        .parent_settings_id
        .as_deref()
        .is_some_and(|parent| parent_settings_id(document, instance) != Some(parent))
    {
        return false;
    }
    if query
        .tag
        .as_deref()
        .is_some_and(|tag| !has_tag(instance, tag))
    {
        return false;
    }
    query
        .properties
        .iter()
        .all(|predicate| matches_property(&instance.properties, predicate))
        && query
            .attributes
            .iter()
            .all(|predicate| matches_property(&instance.attributes, predicate))
}

fn matches_property(map: &serde_json::Map<String, Value>, predicate: &PropertyPredicate) -> bool {
    let Some(actual) = map.get(&predicate.name) else {
        return false;
    };
    predicate
        .value
        .as_ref()
        .is_none_or(|expected| actual == expected)
}

fn metadata_value(
    document: &SettingsBytecode,
    instance: &SettingsBytecodeInstance,
    property_name: &str,
) -> Option<Value> {
    match property_name {
        "Name" => Some(Value::String(instance.name.clone())),
        "ClassName" => Some(Value::String(instance.class_name.clone())),
        "Parent" => Some(
            parent_settings_id(document, instance)
                .map(|parent| Value::String(parent.to_string()))
                .unwrap_or(Value::Null),
        ),
        _ => None,
    }
}

fn set_metadata_value(
    instance: &mut SettingsBytecodeInstance,
    property_name: &str,
    value: Value,
    resolved_parent_index: Option<Option<usize>>,
) -> Result<()> {
    match property_name {
        "Name" => {
            instance.name = value_string(value, "Name")?;
            Ok(())
        }
        "ClassName" => {
            instance.class_name = value_string(value, "ClassName")?;
            Ok(())
        }
        "Parent" => {
            instance.parent_index = resolved_parent_index.unwrap_or(None);
            Ok(())
        }
        _ => bail!("{property_name} is not metadata"),
    }
}

fn set_map_value(map: &mut serde_json::Map<String, Value>, property_name: &str, value: Value) {
    if value.is_null() {
        map.remove(property_name);
    } else {
        map.insert(property_name.to_string(), value);
    }
}

fn is_metadata_property(property_name: &str) -> bool {
    matches!(property_name, "Name" | "ClassName" | "Parent")
}

fn value_string(value: Value, label: &str) -> Result<String> {
    match value {
        Value::String(text) => Ok(text),
        _ => bail!("{label} must be a string"),
    }
}

fn ensure_parent_is_not_descendant(
    document: &SettingsBytecode,
    index: usize,
    parent_index: usize,
) -> Result<()> {
    let mut current = Some(parent_index);
    let mut steps = 0usize;
    while let Some(current_index) = current {
        if current_index == index {
            bail!("Cannot set Parent to the instance itself or one of its descendants");
        }
        steps += 1;
        if steps > document.instances.len() {
            break;
        }
        current = document
            .instances
            .get(current_index)
            .and_then(|instance| instance.parent_index);
    }
    Ok(())
}

fn resolve_parent_index(document: &SettingsBytecode, value: &Value) -> Result<Option<usize>> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => number
            .as_u64()
            .and_then(|raw| usize::try_from(raw).ok())
            .filter(|index| *index < document.instances.len())
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Parent index is out of range")),
        Value::String(settings_id) => document
            .instances
            .iter()
            .position(|instance| instance.settings_id == *settings_id)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Parent settings id was not found")),
        _ => bail!("Parent must be null, an index, or a settings id"),
    }
}

fn parent_settings_id<'a>(
    document: &'a SettingsBytecode,
    instance: &SettingsBytecodeInstance,
) -> Option<&'a str> {
    instance
        .parent_index
        .and_then(|index| document.instances.get(index))
        .map(|parent| parent.settings_id.as_str())
}

fn has_tag(instance: &SettingsBytecodeInstance, tag: &str) -> bool {
    instance
        .properties
        .get("Tags")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().any(|value| value.as_str() == Some(tag)))
}

fn next_editor_settings_id(document: &SettingsBytecode) -> String {
    let existing = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    for index in document.instances.len()..usize::MAX {
        let candidate = format!("editor:{index:x}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    "editor:new".to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_document() -> SettingsBytecode {
        SettingsBytecode {
            version: 2,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "Workspace".to_string(),
                    class_name: "Workspace".to_string(),
                    parent_index: None,
                    properties: serde_json::Map::new(),
                    attributes: serde_json::Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "part".to_string(),
                    name: "Target".to_string(),
                    class_name: "Part".to_string(),
                    parent_index: Some(0),
                    properties: serde_json::Map::from_iter([
                        ("Tags".to_string(), json!(["Enemy"])),
                        ("Transparency".to_string(), json!(0.25)),
                    ]),
                    attributes: serde_json::Map::from_iter([("Health".to_string(), json!(100))]),
                },
            ],
        }
    }

    #[test]
    fn finds_instances_by_class_tag_property_and_attribute() {
        let document = sample_document();
        let indices = find_instances(
            &document,
            &InstanceQuery {
                class_name: Some("Part".to_string()),
                tag: Some("Enemy".to_string()),
                properties: vec![PropertyPredicate::equals("Transparency", json!(0.25))],
                attributes: vec![PropertyPredicate::exists("Health")],
                ..Default::default()
            },
        );

        assert_eq!(indices, vec![1]);
    }

    #[test]
    fn gets_and_sets_metadata_properties_and_attributes() {
        let mut document = sample_document();

        assert_eq!(
            find_unique_instance_index(&document, InstanceSelector::SettingsId("part"))
                .unwrap()
                .and_then(|index| get_instance_property(
                    &document,
                    index,
                    "Parent",
                    PropertyScope::Auto,
                )),
            Some(Value::String("root".to_string()))
        );

        set_property(
            &mut document,
            InstanceSelector::SettingsId("part"),
            "Health",
            json!(50),
            PropertyScope::Attribute,
        )
        .unwrap();

        assert_eq!(
            document.instances[1].attributes.get("Health"),
            Some(&json!(50))
        );
    }

    #[test]
    fn adds_instances_with_unique_editor_id() {
        let mut document = sample_document();
        let added = add_instance(
            &mut document,
            AddInstanceSpec {
                settings_id: None,
                name: "Child".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
        )
        .unwrap();

        assert_eq!(added.index, 2);
        assert_eq!(added.settings_id, "editor:2");
        assert_eq!(document.instances[2].parent_index, Some(0));
    }

    #[test]
    fn removes_descendants_and_reindexes_parents() {
        let mut document = sample_document();
        add_instance(
            &mut document,
            AddInstanceSpec {
                settings_id: Some("grandchild".to_string()),
                name: "Grandchild".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(1),
                properties: Map::new(),
                attributes: Map::new(),
            },
        )
        .unwrap();
        add_instance(
            &mut document,
            AddInstanceSpec {
                settings_id: Some("sibling".to_string()),
                name: "Sibling".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(0),
                properties: Map::new(),
                attributes: Map::new(),
            },
        )
        .unwrap();

        let removed =
            remove_instance(&mut document, InstanceSelector::SettingsId("part"), true).unwrap();

        assert_eq!(removed, vec![1, 2]);
        assert_eq!(document.instances.len(), 2);
        assert_eq!(document.instances[1].settings_id, "sibling");
        assert_eq!(document.instances[1].parent_index, Some(0));
    }

    #[test]
    fn remove_instance_remaps_ref_properties() {
        let mut document = SettingsBytecode {
            version: 2,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "Workspace".to_string(),
                    class_name: "Workspace".to_string(),
                    parent_index: None,
                    properties: Map::from_iter([
                        (
                            "KeptRef".to_string(),
                            json!({ "_type": "Ref", "instanceIndex": 3 }),
                        ),
                        (
                            "RemovedRef".to_string(),
                            json!({
                                "_type": "Ref",
                                "instanceIndex": 2,
                                "settingsId": "remove",
                                "instanceId": "remove",
                            }),
                        ),
                    ]),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "remove".to_string(),
                    name: "Remove".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "keep".to_string(),
                    name: "Keep".to_string(),
                    class_name: "Part".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
            ],
        };

        let removed =
            remove_instance(&mut document, InstanceSelector::SettingsId("remove"), true).unwrap();

        assert_eq!(removed, vec![1]);
        assert_eq!(
            document.instances[0]
                .properties
                .get("KeptRef")
                .and_then(Value::as_object)
                .and_then(|object| object.get("instanceIndex"))
                .and_then(Value::as_u64),
            Some(2)
        );
        assert!(
            document.instances[0]
                .properties
                .get("RemovedRef")
                .and_then(Value::as_object)
                .is_some_and(|object| !object.contains_key("instanceIndex")
                    && !object.contains_key("settingsId")
                    && !object.contains_key("instanceId"))
        );
    }

    #[test]
    fn remove_instance_non_recursive_bails_without_mutating_document() {
        let mut document = sample_document();
        add_instance(
            &mut document,
            AddInstanceSpec {
                settings_id: Some("grandchild".to_string()),
                name: "Grandchild".to_string(),
                class_name: "Folder".to_string(),
                parent_index: Some(1),
                properties: Map::new(),
                attributes: Map::new(),
            },
        )
        .unwrap();
        let before = document.instances.clone();

        let err = remove_instance(&mut document, InstanceSelector::SettingsId("part"), false)
            .unwrap_err();

        assert!(err.to_string().contains("Instance has descendants"));
        assert_eq!(
            document
                .instances
                .iter()
                .map(|instance| (&instance.settings_id, instance.parent_index))
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|instance| (&instance.settings_id, instance.parent_index))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn remove_instance_refuses_root_without_mutating_document() {
        let mut document = sample_document();
        let before = document.instances.clone();

        let err =
            remove_instance(&mut document, InstanceSelector::SettingsId("root"), true).unwrap_err();

        assert!(err.to_string().contains("Refusing to remove root"));
        assert_eq!(
            document
                .instances
                .iter()
                .map(|instance| (&instance.settings_id, instance.parent_index))
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|instance| (&instance.settings_id, instance.parent_index))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn remove_instance_recursively_handles_out_of_order_descendants() {
        let mut document = SettingsBytecode {
            version: 2,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "Workspace".to_string(),
                    class_name: "Workspace".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "leaf".to_string(),
                    name: "Leaf".to_string(),
                    class_name: "Part".to_string(),
                    parent_index: Some(2),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "child".to_string(),
                    name: "Child".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(3),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "target".to_string(),
                    name: "Target".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "sibling".to_string(),
                    name: "Sibling".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "sibling_child".to_string(),
                    name: "SiblingChild".to_string(),
                    class_name: "Part".to_string(),
                    parent_index: Some(4),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
            ],
        };

        let removed =
            remove_instance(&mut document, InstanceSelector::SettingsId("target"), true).unwrap();

        assert_eq!(removed, vec![1, 2, 3]);
        assert_eq!(
            document
                .instances
                .iter()
                .map(|instance| (instance.settings_id.as_str(), instance.parent_index))
                .collect::<Vec<_>>(),
            vec![
                ("root", None),
                ("sibling", Some(0)),
                ("sibling_child", Some(1))
            ]
        );
    }
}
