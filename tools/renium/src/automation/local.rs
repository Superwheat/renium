use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::Value;

use super::{BoundContext, context, op};
use crate::app::output::capture_json_output;
use crate::cli::{Cli, dispatch};

fn payload_args(parameters: &Value) -> Result<Vec<String>> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut arguments = Vec::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "reviewId" | "op" | "p" | "service" | "bridgePorts" | "bridgeWaitSeconds"
        ) {
            continue;
        }
        let mut flag = String::from("--");
        for ch in key.chars() {
            if ch.is_ascii_uppercase() {
                flag.push('-');
                flag.push(ch.to_ascii_lowercase());
            } else {
                flag.push(ch);
            }
        }
        match value {
            Value::Bool(true) => arguments.push(flag),
            Value::Bool(false) | Value::Null => {}
            Value::Array(values) => {
                for value in values {
                    arguments.push(flag.clone());
                    arguments.push(
                        value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_string),
                    );
                }
            }
            Value::String(value) => {
                arguments.push(flag);
                arguments.push(value.clone());
            }
            Value::Number(value) => {
                arguments.push(flag);
                arguments.push(value.to_string());
            }
            Value::Object(_) => {
                arguments.push(format!("{flag}={}", serde_json::to_string(value)?));
            }
        }
    }
    Ok(arguments)
}

fn string_list(value: &Value) -> Option<String> {
    value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .or_else(|| value.as_str().map(str::to_string))
}

fn cli_args(operation: u16, context: &BoundContext, parameters: &Value) -> Result<Vec<String>> {
    let object = parameters.as_object().context("p must be an object")?;
    let mut flags = object.clone();
    for key in [
        "service",
        "editor",
        "destructive",
        "bootstrap",
        "bridgeWaitSeconds",
        "bridgePorts",
        "root",
        "project",
        "projectRoot",
        "srcDir",
        "srcRoot",
    ] {
        flags.remove(key);
    }
    if operation == op::PROJECT_INIT {
        flags.remove("path");
    }
    let mut arguments = Vec::new();
    let path_fields: &[(&str, &str)] = match operation {
        op::GET_PROPERTY | op::SET_PROPERTY => &[("settingsFile", "--settings-file")],
        op::SET_SOURCE => &[
            ("settingsFile", "--settings-file"),
            ("sourceFile", "--source-file"),
        ],
        op::IMPORT_MODEL => &[("model", "--model")],
        op::EXPORT_MODEL | op::EXPORT_PLACE | op::SOURCEMAP => &[("output", "--output")],
        _ => &[],
    };
    for (key, flag) in path_fields {
        if let Some(value) = flags.remove(*key) {
            let path = value
                .as_str()
                .with_context(|| format!("p.{key} must be a path string"))?;
            arguments.extend([
                (*flag).to_string(),
                context::path(context, PathBuf::from(path))
                    .display()
                    .to_string(),
            ]);
        }
    }
    for (key, flag) in [("pathSegments", "--path"), ("pathOrdinals", "--ords")] {
        if let Some(value) = flags.remove(key) {
            arguments.push(format!("{flag}={}", serde_json::to_string(&value)?));
        }
    }
    if operation == op::SET_PROPERTY
        && let Some(value) = flags.remove("value")
    {
        arguments.push(format!("--value={}", serde_json::to_string(&value)?));
    }
    if operation == op::SET_SOURCE
        && let Some(source) = flags
            .remove("source")
            .and_then(|value| value.as_str().map(str::to_string))
    {
        arguments.extend(["--str".to_string(), source]);
    }
    if operation == op::MOVE
        && let Some(target) = flags
            .remove("targetService")
            .and_then(|value| value.as_str().map(str::to_string))
    {
        arguments.extend(["--to-service".to_string(), target]);
    }
    if operation == op::ADD {
        for (key, flag) in [("properties", "--property"), ("attributes", "--attribute")] {
            let Some(values) = flags.remove(key) else {
                continue;
            };
            let values = values
                .as_array()
                .with_context(|| format!("p.{key} must be a string array"))?;
            for value in values {
                arguments.extend([
                    flag.to_string(),
                    value
                        .as_str()
                        .with_context(|| format!("p.{key} must contain strings"))?
                        .to_string(),
                ]);
            }
        }
    }
    if operation == op::IMPORT_SNAPSHOTS {
        if let Some(snapshot_dir) = flags
            .remove("snapshotDir")
            .and_then(|value| value.as_str().map(str::to_string))
        {
            arguments.extend([
                "--snapshot-dir".to_string(),
                context::path(context, PathBuf::from(snapshot_dir))
                    .display()
                    .to_string(),
            ]);
        }
        if let Some(services) = flags.remove("services") {
            let services = string_list(&services)
                .context("import-snapshots p.services must be a string or string array")?;
            arguments.extend(["--services".to_string(), services]);
        }
    }
    arguments.extend(payload_args(&Value::Object(flags))?);
    Ok(arguments)
}

pub(super) fn execute(operation: u16, context: &BoundContext, parameters: &Value) -> Result<Value> {
    if operation == op::REVERT
        && parameters.get("applyStudio").and_then(Value::as_bool) == Some(true)
    {
        bail!("Revert with applyStudio is unsupported; use push after reverting the files");
    }
    let command = match operation {
        op::FIND => "find",
        op::TREE => "tree",
        op::INSPECT => "inspect",
        op::GET_PROPERTY => "bg",
        op::SET_PROPERTY => "bs",
        op::SET_SOURCE => "bss",
        op::ADD => "create",
        op::CLONE => "clone",
        op::MOVE => "move",
        op::REMOVE => "remove",
        op::REVERT => "rev",
        op::IMPORT_MODEL => "import-model",
        op::EXPORT_MODEL => "export-model",
        op::EXPORT_PLACE => "bep",
        op::IMPORT_SNAPSHOTS => "im",
        op::SOURCEMAP => "sm",
        op::PROJECT_INIT => "init",
        op::PROJECT_VALIDATE => "doctor",
        _ => bail!("Unsupported local automation opcode {operation}"),
    };
    let mut arguments = vec!["renium".to_string()];
    if operation != op::PROJECT_INIT {
        arguments.extend(["--project".to_string(), context.project.clone()]);
    }
    arguments.push(command.to_string());
    if matches!(
        operation,
        op::FIND
            | op::TREE
            | op::INSPECT
            | op::GET_PROPERTY
            | op::SET_PROPERTY
            | op::SET_SOURCE
            | op::ADD
            | op::CLONE
            | op::MOVE
            | op::REMOVE
            | op::IMPORT_MODEL
            | op::EXPORT_MODEL
    ) && let Some(service) = parameters.get("service").and_then(Value::as_str)
    {
        arguments.push(service.to_string());
    }
    if operation == op::PROJECT_INIT {
        arguments.push(context.root.clone());
    } else if operation == op::PROJECT_VALIDATE {
        arguments.extend(["--root".to_string(), context.root.clone()]);
    } else if matches!(operation, op::REVERT | op::IMPORT_SNAPSHOTS) {
        arguments.extend([
            "--project-root".to_string(),
            context.root.clone(),
            "--src-dir".to_string(),
            context::source_dir(context)?.display().to_string(),
        ]);
    }
    arguments.extend(cli_args(operation, context, parameters)?);
    let cli = Cli::try_parse_from(arguments).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    capture_json_output(|| dispatch::dispatch(cli.command, cli.project.as_deref()))
}
