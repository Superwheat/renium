use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::parser::ValueSource;
use serde_json::{Map, Value};

use crate::command_line::{BridgeConnectionArgs, Cli, Commands};
use crate::project_config;

pub(crate) fn apply_merged(cli: &mut Cli, matches: &clap::ArgMatches) -> Result<()> {
    if matches!(
        &cli.command,
        Commands::Config(_) | Commands::Doctor(_) | Commands::UpdateHelper(_)
    ) {
        return Ok(());
    }
    let command_root = active_command_path(matches, "project")
        .or_else(|| active_command_path(matches, "project_root"))
        .or_else(|| active_command_path(matches, "root"));
    let root = match cli.project.as_deref().or(command_root.as_deref()) {
        Some(project) if project.is_dir() => project.to_path_buf(),
        Some(project) => project
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        None => std::env::current_dir()?,
    };
    let config = project_config::load_merged_config(&root)?;
    let object = config
        .as_object()
        .context("Merged Renium configuration must be an object")?;
    let uses_default = |id: &str| {
        matches!(
            matches.value_source(id),
            None | Some(ValueSource::DefaultValue)
        )
    };
    if uses_default("log_level")
        && let Some(value) = object.get("logLevel").and_then(Value::as_str)
    {
        cli.log_level = value.to_string();
    }
    if uses_default("color")
        && let Some(value) = object.get("color").and_then(Value::as_str)
    {
        cli.color = value.to_string();
    }
    if uses_default("output_mode")
        && let Some(value) = object.get("outputMode").and_then(Value::as_str)
    {
        cli.output_mode = value.to_string();
    }
    if uses_default("yes")
        && let Some(value) = object.get("yes").and_then(Value::as_bool)
    {
        cli.yes = value;
    }
    if uses_default("backtrace")
        && let Some(value) = object.get("backtrace").and_then(Value::as_bool)
    {
        cli.backtrace = value;
    }
    if cli.place.is_none()
        && let Some(value) = object.get("place").and_then(Value::as_str)
    {
        cli.place = Some(value.to_string());
    }
    if cli.daemon.is_none()
        && let Some(value) = object.get("daemon").and_then(Value::as_str)
    {
        cli.daemon = Some(value.to_string());
    }
    apply_command(&mut cli.command, matches, object)
}

fn active_command_path(matches: &clap::ArgMatches, id: &str) -> Option<PathBuf> {
    let mut current = matches;
    loop {
        if current.try_contains_id(id).ok() == Some(true)
            && let Ok(Some(path)) = current.try_get_one::<PathBuf>(id)
        {
            return Some(path.clone());
        }
        let (_, child) = current.subcommand()?;
        current = child;
    }
}

fn command_value_uses_default(matches: &clap::ArgMatches, id: &str) -> bool {
    let mut current = matches;
    loop {
        if current.try_contains_id(id).ok() == Some(true) {
            return matches!(
                current.value_source(id),
                None | Some(ValueSource::DefaultValue)
            );
        }
        let Some((_, child)) = current.subcommand() else {
            return true;
        };
        current = child;
    }
}

fn configured_usize(object: &Map<String, Value>, key: &str) -> Result<Option<usize>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let number = value
        .as_u64()
        .with_context(|| format!("Configuration key '{key}' must be a non-negative integer"))?;
    Ok(Some(usize::try_from(number).with_context(|| {
        format!("Configuration key '{key}' is too large")
    })?))
}

fn configured_services(object: &Map<String, Value>) -> Option<String> {
    object
        .get("services")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
}

fn apply_default<T>(matches: &clap::ArgMatches, id: &str, target: &mut T, value: Option<T>) {
    if command_value_uses_default(matches, id)
        && let Some(value) = value
    {
        *target = value;
    }
}

fn apply_default_usize(
    matches: &clap::ArgMatches,
    id: &str,
    target: &mut usize,
    object: &Map<String, Value>,
    key: &str,
) -> Result<()> {
    if command_value_uses_default(matches, id)
        && let Some(value) = configured_usize(object, key)?
    {
        *target = value;
    }
    Ok(())
}

fn apply_default_path(
    matches: &clap::ArgMatches,
    id: &str,
    target: &mut PathBuf,
    object: &Map<String, Value>,
    key: &str,
) {
    let value = object.get(key).and_then(Value::as_str).map(PathBuf::from);
    apply_default(matches, id, target, value);
}

fn apply_default_services(
    matches: &clap::ArgMatches,
    target: &mut String,
    object: &Map<String, Value>,
) {
    apply_default(matches, "services", target, configured_services(object));
}

fn apply_default_pair(
    matches: &clap::ArgMatches,
    ids: (&str, &str),
    targets: (&mut bool, &mut bool),
    value: Option<bool>,
) {
    if command_value_uses_default(matches, ids.0)
        && command_value_uses_default(matches, ids.1)
        && let Some(value) = value
    {
        *targets.0 = value;
        *targets.1 = !value;
    }
}

fn apply_bridge(
    matches: &clap::ArgMatches,
    object: &Map<String, Value>,
    bridge: &mut BridgeConnectionArgs,
) {
    apply_default(
        matches,
        "bridge_wait_seconds",
        &mut bridge.wait_seconds,
        object.get("bridgeWaitSeconds").and_then(Value::as_f64),
    );
    apply_default(
        matches,
        "bridge_ports",
        &mut bridge.ports,
        object
            .get("bridgePorts")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
}

fn apply_command(
    command: &mut Commands,
    matches: &clap::ArgMatches,
    object: &Map<String, Value>,
) -> Result<()> {
    match command {
        Commands::ExportSnapshots(args) => {
            apply_default_path(
                matches,
                "project_root",
                &mut args.project_root,
                object,
                "projectRoot",
            );
            apply_default_path(
                matches,
                "snapshot_dir",
                &mut args.snapshot_dir,
                object,
                "snapshotDir",
            );
            apply_default_services(matches, &mut args.services, object);
            apply_default_usize(
                matches,
                "chunk_size",
                &mut args.chunk_size,
                object,
                "chunkSize",
            )?;
            apply_default_usize(
                matches,
                "source_workers",
                &mut args.source_workers,
                object,
                "sourceWorkers",
            )?;
            apply_default_usize(
                matches,
                "instance_workers",
                &mut args.instance_workers,
                object,
                "instanceWorkers",
            )?;
            apply_default_usize(
                matches,
                "import_workers",
                &mut args.import_workers,
                object,
                "importWorkers",
            )?;
            for (id, key, target) in [
                ("import_mode", "importMode", &mut args.import_mode),
                (
                    "performance_mode",
                    "performanceMode",
                    &mut args.performance_mode,
                ),
            ] {
                apply_default(
                    matches,
                    id,
                    target,
                    object.get(key).and_then(Value::as_str).map(str::to_owned),
                );
            }
            if command_value_uses_default(matches, "run_import")
                && object.get("runImport").and_then(Value::as_bool) == Some(true)
            {
                args.run_import = true;
                args.no_run_import = false;
            }
            apply_default_pair(
                matches,
                ("modified_default_bypass", "no_modified_default_bypass"),
                (
                    &mut args.modified_default_bypass,
                    &mut args.no_modified_default_bypass,
                ),
                object.get("modifiedDefaultBypass").and_then(Value::as_bool),
            );
            if command_value_uses_default(matches, "no_adaptive_throttle")
                && let Some(enabled) = object.get("adaptiveThrottle").and_then(Value::as_bool)
            {
                args.no_adaptive_throttle = !enabled;
            }
            apply_bridge(matches, object, &mut args.bridge);
        }
        Commands::BridgeDaemon(args) => apply_bridge(matches, object, &mut args.bridge),
        Commands::PushEditorChanges(args) => {
            apply_default_path(
                matches,
                "project_root",
                &mut args.project.project_root,
                object,
                "projectRoot",
            );
            apply_default(
                matches,
                "verify_sources",
                &mut args.verify_sources,
                object
                    .get("verifyEditorPushSources")
                    .and_then(Value::as_bool),
            );
            apply_default(
                matches,
                "override_packages",
                &mut args.override_packages,
                object
                    .get("liveSync")
                    .and_then(Value::as_object)
                    .and_then(|live| live.get("overridePackages"))
                    .and_then(Value::as_bool),
            );
            apply_bridge(matches, object, &mut args.bridge);
        }
        Commands::ExplorerDaemon(args) => {
            apply_default_path(
                matches,
                "project_root",
                &mut args.project.project_root,
                object,
                "projectRoot",
            );
            apply_default_services(matches, &mut args.services, object);
        }
        Commands::Syncback(args) => {
            apply_default_path(matches, "input", &mut args.input, object, "snapshotDir");
            apply_default_services(matches, &mut args.services, object);
        }
        _ => {}
    }
    Ok(())
}
