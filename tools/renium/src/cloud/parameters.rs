use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::system::files::absolutize_for_daemon as absolute_path;

pub(super) fn assignments(values: &[String]) -> Result<Map<String, Value>> {
    let mut result = Map::new();
    merge_assignments(&mut result, values)?;
    Ok(result)
}

pub(super) fn merge_assignments(result: &mut Map<String, Value>, values: &[String]) -> Result<()> {
    for value in values {
        let (name, value) = assignment(value)?;
        match result.get_mut(name) {
            Some(existing) => append(existing, value),
            None => {
                result.insert(name.to_string(), value);
            }
        }
    }
    Ok(())
}

pub(super) fn assignment(value: &str) -> Result<(&str, Value)> {
    let (name, value) = value
        .split_once('=')
        .with_context(|| format!("Expected NAME=VALUE, got '{value}'"))?;
    if name.is_empty() {
        bail!("Assignment names cannot be empty");
    }
    Ok((name, parse_value(value)))
}

pub(super) fn parse_value(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()))
}

pub(super) fn absolutize_files(files: &mut Map<String, Value>) -> Result<()> {
    for (name, value) in files {
        match value {
            Value::String(path) => absolutize_file(path)?,
            Value::Array(paths) => {
                for value in paths {
                    let path = value
                        .as_str()
                        .with_context(|| format!("--file {name} values must be paths"))?;
                    let mut absolute = path.to_string();
                    absolutize_file(&mut absolute)?;
                    *value = Value::String(absolute);
                }
            }
            _ => bail!("--file {name} must be a path or an array of paths"),
        }
    }
    Ok(())
}

fn append(existing: &mut Value, value: Value) {
    if !existing.is_array() {
        *existing = Value::Array(vec![existing.take()]);
    }
    let values = existing
        .as_array_mut()
        .expect("existing was converted to an array");
    match value {
        Value::Array(value) => values.extend(value),
        value => values.push(value),
    }
}

fn absolutize_file(path: &mut String) -> Result<()> {
    let absolute = absolute_path(Path::new(path));
    if !absolute.is_file() {
        bail!("File does not exist: {}", absolute.display());
    }
    *path = absolute.display().to_string();
    Ok(())
}
