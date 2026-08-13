use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::editor::paths::document_instance_index_by_path_unique;
use crate::settings::bytecode::SettingsBytecode;
use crate::settings::instance::{
    self as instance_api, InstanceSelector, PropertyPredicate, PropertyScope,
};

pub(crate) fn bytecode_selector_specified(
    index: Option<usize>,
    settings_id: Option<&str>,
    name: Option<&str>,
    class_name: Option<&str>,
) -> bool {
    index.is_some()
        || settings_id.is_some_and(|value| !value.is_empty())
        || name.is_some_and(|value| !value.is_empty())
        || class_name.is_some_and(|value| !value.is_empty())
}

pub(crate) fn bytecode_selector<'a>(
    index: Option<usize>,
    settings_id: Option<&'a str>,
    name: Option<&'a str>,
    class_name: Option<&'a str>,
) -> Result<InstanceSelector<'a>> {
    let mut selector = None;
    let mut count = 0;
    if let Some(index) = index {
        selector = Some(InstanceSelector::Index(index));
        count += 1;
    }
    if let Some(settings_id) = settings_id.filter(|value| !value.is_empty()) {
        selector = Some(InstanceSelector::SettingsId(settings_id));
        count += 1;
    }
    if let Some(name) = name.filter(|value| !value.is_empty()) {
        selector = Some(InstanceSelector::Name(name));
        count += 1;
    }
    if let Some(class_name) = class_name.filter(|value| !value.is_empty()) {
        selector = Some(InstanceSelector::ClassName(class_name));
        count += 1;
    }
    match (count, selector) {
        (1, Some(selector)) => Ok(selector),
        (0, _) => bail!("Provide one selector: --index, --settings-id, --name, or --class-name"),
        _ => bail!("Provide only one selector"),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct BytecodeInstanceTarget<'a> {
    pub(crate) path_segments: Option<&'a [String]>,
    pub(crate) path_ordinals: &'a [usize],
    pub(crate) index: Option<usize>,
    pub(crate) settings_id: Option<&'a str>,
    pub(crate) name: Option<&'a str>,
    pub(crate) class_name: Option<&'a str>,
}

pub(crate) fn resolve_bytecode_instance_index(
    document: &SettingsBytecode,
    target: BytecodeInstanceTarget<'_>,
    not_found: &str,
) -> Result<usize> {
    if let Some(path_segments) = target.path_segments {
        if bytecode_selector_specified(
            target.index,
            target.settings_id,
            target.name,
            target.class_name,
        ) {
            bail!("--path-segments-json cannot be combined with another selector");
        }
        return document_instance_index_by_path_unique(
            document,
            path_segments,
            target.path_ordinals,
        );
    }
    let selector = bytecode_selector(
        target.index,
        target.settings_id,
        target.name,
        target.class_name,
    )?;
    instance_api::find_unique_instance_index(document, selector)?
        .ok_or_else(|| anyhow::anyhow!(not_found.to_string()))
}

pub(crate) fn bytecode_parent_index(
    document: &SettingsBytecode,
    no_parent: bool,
    index: Option<usize>,
    settings_id: Option<&str>,
    name: Option<&str>,
    class_name: Option<&str>,
) -> Result<Option<usize>> {
    if no_parent {
        let specified = bytecode_selector_specified(index, settings_id, name, class_name);
        if specified {
            bail!("--no-parent cannot be combined with a parent selector");
        }
        if !document.instances.is_empty() {
            bail!("--no-parent is only valid when creating the root of an empty settings store");
        }
        return Ok(None);
    }

    let specified = bytecode_selector_specified(index, settings_id, name, class_name);
    if specified {
        let selector = bytecode_selector(index, settings_id, name, class_name)?;
        return instance_api::find_unique_instance_index(document, selector)?
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Parent instance was not found"));
    }

    document
        .instances
        .iter()
        .position(|instance| instance.parent_index.is_none())
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("Cannot infer parent for empty settings bytecode"))
}

pub(crate) fn parse_property_scope(raw: &str) -> Result<PropertyScope> {
    match raw.to_ascii_lowercase().as_str() {
        "auto" => Ok(PropertyScope::Auto),
        "metadata" | "meta" => Ok(PropertyScope::Metadata),
        "property" | "prop" => Ok(PropertyScope::Property),
        "attribute" | "attr" => Ok(PropertyScope::Attribute),
        other => bail!("Invalid property scope: {other}"),
    }
}

pub(crate) fn parse_property_predicates(raw: &[String]) -> Result<Vec<PropertyPredicate>> {
    raw.iter()
        .map(|item| {
            if let Some((name, value)) = item.split_once('=') {
                let value: Value = serde_json::from_str(value)
                    .with_context(|| format!("Invalid predicate JSON value in {item}"))?;
                Ok(PropertyPredicate::equals(name.trim(), value))
            } else {
                Ok(PropertyPredicate::exists(item.trim()))
            }
        })
        .collect()
}

pub(crate) fn parse_property_assignments(raw: &[String]) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for item in raw {
        let Some((name, value)) = item.split_once('=') else {
            bail!("Expected NAME=JSON assignment: {item}");
        };
        let name = name.trim();
        if name.is_empty() {
            bail!("Property assignment name cannot be empty: {item}");
        }
        let value: Value = serde_json::from_str(value)
            .with_context(|| format!("Invalid assignment JSON value in {item}"))?;
        out.insert(name.to_string(), value);
    }
    Ok(out)
}
