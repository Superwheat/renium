use anyhow::{Result, bail};
use clap::{Arg, ArgAction, Command};
use renium_plugin_sdk::{ArgumentType, Manifest, PROTOCOL, PluginCommand, valid_name};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub(super) fn validate(manifest: &Manifest) -> Result<()> {
    if manifest.schema_version != PROTOCOL {
        bail!(
            "Unsupported plugin schemaVersion {}",
            manifest.schema_version
        );
    }
    if !valid_name(&manifest.name) {
        bail!(
            "Plugin name must start with a lowercase letter and contain only lowercase letters, digits or hyphens (max 64)"
        );
    }
    let core = crate::cli::command();
    if core.get_subcommands().any(|command| {
        command.get_name() == manifest.name
            || command
                .get_all_aliases()
                .any(|alias| alias == manifest.name)
    }) || manifest.name == "help"
    {
        bail!(
            "Plugin name '{}' collides with a Renium command",
            manifest.name
        );
    }
    if manifest.version.trim().is_empty() || manifest.description.trim().is_empty() {
        bail!("Plugin version and description are required");
    }
    semver::Version::parse(&manifest.version)?;
    if manifest.commands.is_empty() || manifest.commands.len() > 100 {
        bail!("Plugins need 1–100 commands");
    }
    if manifest
        .permissions
        .iter()
        .any(|p| !matches!(p.as_str(), "renium" | "network" | "filesystem" | "studio"))
    {
        bail!("Unknown plugin permission (supported: renium, network, filesystem, studio)");
    }
    if manifest.executable.is_empty() {
        bail!("Plugin has no executable targets");
    }
    for (platform, argv) in &manifest.executable {
        if !matches!(platform.as_str(), "windows" | "macos" | "linux" | "default")
            || argv.is_empty()
            || argv.iter().any(|s| s.is_empty() || s.contains('\0'))
        {
            bail!("Invalid executable target {platform}");
        }
        let path = std::path::Path::new(&argv[0]);
        if path.is_absolute()
            || path.components().any(|p| {
                !matches!(
                    p,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            bail!("Executable must be a relative path inside the plugin");
        }
    }
    for (name, command) in &manifest.commands {
        if !valid_name(name) || name == "help" || command.description.trim().is_empty() {
            bail!("Invalid plugin command {name}");
        }
        if !(1..=3600).contains(&command.timeout_seconds) {
            bail!("Command timeoutSeconds must be 1–3600");
        }
        if command.arguments.len() > 100 {
            bail!("Too many arguments for {name}");
        }
        let mut seen = HashSet::new();
        for arg in &command.arguments {
            if !valid_name(&arg.name)
                || matches!(arg.name.as_str(), "help" | "version" | "session")
                || !seen.insert(&arg.name)
            {
                bail!("Invalid, reserved or duplicate argument {}", arg.name);
            }
            if arg.required && arg.default.is_some() {
                bail!(
                    "Argument {} cannot be required and have a default",
                    arg.name
                );
            }
            if let Some(value) = &arg.default {
                let valid = match arg.kind {
                    ArgumentType::String => value.is_string(),
                    ArgumentType::Integer => value.is_i64(),
                    ArgumentType::Boolean => value.is_boolean(),
                };
                if !valid {
                    bail!("Invalid default type for {}", arg.name);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn command(manifest: &Manifest) -> Command {
    let mut root = Command::new(manifest.name.clone())
        .about(manifest.description.clone())
        .version(manifest.version.clone())
        .subcommand_required(true)
        .arg(
            Arg::new("session")
                .long("session")
                .global(true)
                .help("Stable task identity (or RENIUM_SESSION_ID)"),
        );
    for (name, definition) in &manifest.commands {
        let mut command = Command::new(name.clone()).about(definition.description.clone());
        for arg in &definition.arguments {
            let mut option = Arg::new(arg.name.clone())
                .long(arg.name.clone())
                .help(arg.description.clone())
                .required(arg.required);
            option = match arg.kind {
                ArgumentType::String => option,
                ArgumentType::Integer => option.value_parser(clap::value_parser!(i64)),
                ArgumentType::Boolean => option
                    .action(ArgAction::Set)
                    .num_args(0..=1)
                    .default_missing_value("true")
                    .value_parser(clap::value_parser!(bool)),
            };
            if let Some(value) = &arg.default {
                option = option.default_value(
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string()),
                );
            }
            command = command.arg(option);
        }
        root = root.subcommand(command);
    }
    root
}

pub(super) fn arguments(definition: &PluginCommand, matches: &clap::ArgMatches) -> Value {
    let mut values = Map::new();
    for arg in &definition.arguments {
        let value = match arg.kind {
            ArgumentType::String => matches
                .get_one::<String>(&arg.name)
                .map(|v| Value::from(v.clone())),
            ArgumentType::Integer => matches.get_one::<i64>(&arg.name).copied().map(Value::from),
            ArgumentType::Boolean => matches.get_one::<bool>(&arg.name).copied().map(Value::from),
        };
        if let Some(value) = value {
            values.insert(arg.name.clone(), value);
        }
    }
    Value::Object(values)
}
