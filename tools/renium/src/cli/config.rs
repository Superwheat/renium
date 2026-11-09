use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::parser::ValueSource;
use serde_json::{Map, Value};

use crate::cli::{BridgeConnectionArgs, Cli, Commands};
use crate::project::config;

pub(crate) fn apply_merged(cli: &mut Cli, matches: &clap::ArgMatches) -> Result<()> {
    if matches!(
        &cli.command,
        Commands::Config(_)
            | Commands::Doctor(_)
            | Commands::UpdateHelper(_)
            | Commands::Plugin(_)
            | Commands::External(_)
            | Commands::Help(_)
    ) {
        return Ok(());
    }
    let command_root = active_command_path(matches, "project")
        .or_else(|| active_command_path(matches, "project_root"))
        .or_else(|| active_command_path(matches, "root"));
    let root = match cli.project.as_deref().or(command_root.as_deref()) {
        Some(project) if project.is_dir() => project.to_path_buf(),
        Some(project) => match project.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => std::env::current_dir()?,
        },
        None => std::env::current_dir()?,
    };
    let config = config::load_merged_config(&root)?;
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
    if let Some(place) = stored_unless_explicit(
        cli.place.as_deref(),
        std::env::var_os("RENIUM_PLACE"),
        object.get("place"),
    ) {
        cli.place = Some(place);
    }
    if let Some(daemon) = stored_unless_explicit(
        cli.daemon.as_deref(),
        std::env::var_os("RENIUM_DAEMON_NAME"),
        object.get("daemon"),
    ) {
        cli.daemon = Some(daemon);
    }
    apply_command(&mut cli.command, matches, object)
}

/// The stored value of a setting that a flag or an environment variable can
/// also give (`--place` and RENIUM_PLACE, `--daemon` and RENIUM_DAEMON_NAME).
/// Either of those is the choice for this run, so the stored value applies
/// only when both are absent.
fn stored_unless_explicit(
    flag: Option<&str>,
    variable: Option<OsString>,
    stored: Option<&Value>,
) -> Option<String> {
    if flag.is_some() || variable.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    stored.and_then(Value::as_str).map(str::to_string)
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

fn apply_live_sync_defaults(
    matches: &clap::ArgMatches,
    object: &Map<String, Value>,
    initial_sync_priority: &mut Option<String>,
    initial_conflict_preference: &mut Option<String>,
) {
    let live = object.get("liveSync").and_then(Value::as_object);
    let configured = |key: &str| {
        live.and_then(|live| live.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    apply_default(
        matches,
        "initial_sync_priority",
        initial_sync_priority,
        configured("initialSyncPriority").map(Some),
    );
    apply_default(
        matches,
        "initial_conflict_preference",
        initial_conflict_preference,
        configured("initialConflictPreference").map(Some),
    );
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
        Commands::Pull(args) => {
            apply_default_path(
                matches,
                "project_root",
                &mut args.project_root,
                object,
                "projectRoot",
            );
            apply_default_services(matches, &mut args.services, object);
            apply_bridge(matches, object, &mut args.bridge);
        }
        Commands::BridgeDaemon(args) => apply_bridge(matches, object, &mut args.bridge),
        Commands::StudioChangeState(args) | Commands::LiveStart(args) => {
            apply_live_sync_defaults(
                matches,
                object,
                &mut args.initial_sync_priority,
                &mut args.initial_conflict_preference,
            );
            apply_bridge(matches, object, &mut args.bridge);
        }
        Commands::LiveStatus(args) => {
            apply_live_sync_defaults(
                matches,
                object,
                &mut args.initial_sync_priority,
                &mut args.initial_conflict_preference,
            );
            apply_bridge(matches, object, &mut args.bridge);
        }
        Commands::LiveStop(args)
        | Commands::RetryPending(args)
        | Commands::DiscardPending(args) => {
            apply_bridge(matches, object, &mut args.bridge);
        }
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
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_flag_or_an_environment_variable_beats_the_stored_setting() {
        let stored = json!("stored");
        let resolve = |flag: Option<&str>, variable: Option<&str>| {
            stored_unless_explicit(flag, variable.map(OsString::from), Some(&stored))
        };
        assert_eq!(resolve(None, None).as_deref(), Some("stored"));
        assert_eq!(resolve(None, Some("")).as_deref(), Some("stored"));
        assert_eq!(resolve(None, Some("fromenv")), None);
        assert_eq!(resolve(Some("flag"), None), None);
        assert_eq!(stored_unless_explicit(None, None, None), None);
    }
}
