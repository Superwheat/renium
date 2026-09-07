//! Explicitly installed, process-isolated workflow plugins. Ordinary commands do no discovery.
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use renium_plugin_sdk::{
    Invocation, MANIFEST, Manifest, PROTOCOL, PluginContext, lease, plugin_home,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod manifest;
mod scaffold;
mod snapshot;
#[cfg(test)]
mod tests;

#[derive(Args)]
pub(crate) struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Create a standalone Rust starter, with its SDK included. Does not build it.
    New { name: String, path: Option<PathBuf> },
    /// Register a trusted local plugin. Never builds or runs it during installation.
    Install {
        path: PathBuf,
        /// Allow edits/rebuilds without reinstalling. Only use for code you develop.
        #[arg(long)]
        dev: bool,
    },
    /// List installed plugins without running them.
    List,
    /// Show commands, permissions and agent guidance without running plugin code.
    Info { name: String },
    /// Validate source metadata without requiring an executable.
    Check { path: PathBuf },
    /// Export the selected project's complete projection into a new isolated project.
    Snapshot { destination: PathBuf },
    /// Inspect process liveness without depending on a Studio bridge connection.
    Process { pid: std::num::NonZeroU32 },
    /// Verify that the active daemon can enforce plugin resource ownership.
    RuntimeCheck,
    /// Unregister a plugin; preserve its source, state and resource leases.
    Remove { name: String },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registration {
    directory: PathBuf,
    manifest_hash: String,
    executable_hash: String,
    dev: bool,
}

fn registration_path(home: &Path, name: &str) -> Result<PathBuf> {
    if !renium_plugin_sdk::valid_name(name) {
        bail!("Invalid plugin name: {name}");
    }
    Ok(home.join("installed").join(format!("{name}.json")))
}

fn load(directory: &Path) -> Result<(Manifest, Vec<u8>)> {
    let path = directory.join(MANIFEST);
    let bytes = bounded_read(&path, 1024 * 1024)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("Invalid renium-plugin.json")?;
    manifest::validate(&manifest)?;
    if let Some(guide) = &manifest.guide {
        contained_file(directory, guide)?;
    }
    Ok((manifest, bytes))
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{} exceeds its size limit", path.display());
    }
    Ok(bytes)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_hash(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn contained_file(directory: &Path, path: &Path) -> Result<PathBuf> {
    if path.is_absolute()
        || path.components().any(|p| {
            !matches!(
                p,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        bail!(
            "Plugin paths must stay inside their directory: {}",
            path.display()
        );
    }
    let root = fs::canonicalize(directory)?;
    let file = fs::canonicalize(root.join(path)).with_context(|| {
        format!(
            "Missing {}; build the plugin first if this is its executable",
            path.display()
        )
    })?;
    if !file.starts_with(&root) || !file.is_file() {
        bail!("Plugin file escapes its directory or is not a file");
    }
    Ok(file)
}

fn executable(directory: &Path, manifest: &Manifest) -> Result<(PathBuf, Vec<String>)> {
    let argv = manifest
        .executable
        .get(std::env::consts::OS)
        .or_else(|| manifest.executable.get("default"))
        .with_context(|| {
            format!(
                "Plugin {} does not support {}",
                manifest.name,
                std::env::consts::OS
            )
        })?;
    #[cfg(windows)]
    if !Path::new(&argv[0])
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        bail!("Windows plugin entry points must be .exe files, not shell/batch scripts");
    }
    Ok((
        contained_file(directory, Path::new(&argv[0]))?,
        argv[1..].to_vec(),
    ))
}

fn installed(home: &Path, name: &str) -> Result<(Registration, Manifest)> {
    let path = registration_path(home, name)?;
    let registration: Registration = serde_json::from_slice(&bounded_read(&path, 65536)?)
        .with_context(|| format!("Invalid registration for {name}; reinstall it"))?;
    let (manifest, bytes) = load(&registration.directory)?;
    if manifest.name != name {
        bail!("Plugin was renamed; reinstall it");
    }
    if !registration.dev && hash(&bytes) != registration.manifest_hash {
        bail!("Plugin manifest changed; inspect it and run rbx plugin install PATH again");
    }
    Ok((registration, manifest))
}

pub(crate) fn manage(args: PluginArgs) -> Result<()> {
    let result = match args.command {
        PluginCommand::New { name, path } => scaffold::create(&name, path.as_deref())?,
        PluginCommand::Check { path } => {
            let (manifest, _) = load(&path)?;
            json!({"valid":true,"name":manifest.name,"commands":manifest.commands,"compiled":false})
        }
        PluginCommand::Snapshot { destination } => snapshot::create(&destination)?,
        PluginCommand::Process { pid } => {
            json!({"pid":pid.get(),"alive":renium_plugin_sdk::process::alive(pid.get())?})
        }
        PluginCommand::RuntimeCheck => {
            let capabilities = crate::automation::commands::daemon_result(
                crate::automation::op::CAP,
                None,
                json!({}),
                false,
                None,
            )?;
            if capabilities["pluginResourceLeases"].as_u64() != Some(1) {
                bail!(
                    "The running daemon does not support plugin resource leases. Restart Renium's daemon with this build before using a managed Studio workflow"
                );
            }
            json!({"pluginResourceLeases":1,"ready":true})
        }
        PluginCommand::Install { path, dev } => {
            let directory = fs::canonicalize(path)?;
            let (manifest, bytes) = load(&directory)?;
            let (exe, _) = executable(&directory, &manifest)?;
            let registration = Registration {
                directory,
                manifest_hash: hash(&bytes),
                executable_hash: file_hash(&exe)?,
                dev,
            };
            let path = registration_path(&plugin_home()?, &manifest.name)?;
            fs::create_dir_all(path.parent().context("Missing registration parent")?)?;
            crate::system::files::atomic_write_file(&path, &serde_json::to_vec(&registration)?)?;
            json!({"installed":manifest.name,"dev":dev,"permissions":manifest.permissions,"trust":"Native plugins run with your user privileges. Install only trusted code."})
        }
        PluginCommand::List => {
            let home = plugin_home()?;
            let directory = home.join("installed");
            let mut entries = Vec::new();
            if directory.try_exists()? {
                for entry in fs::read_dir(directory)? {
                    let path = entry?.path();
                    if path.extension().and_then(|v| v.to_str()) != Some("json") {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .and_then(|v| v.to_str())
                        .unwrap_or_default();
                    entries.push(match installed(&home, name) {
                        Ok((r,m)) => json!({"name":m.name,"version":m.version,"description":m.description,"dev":r.dev}),
                        Err(error) => json!({"name":name,"error":format!("{error:#}")}),
                    });
                }
            }
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            json!({"plugins":entries})
        }
        PluginCommand::Info { name } => {
            let (registration, manifest) = installed(&plugin_home()?, &name)?;
            let guide = manifest
                .guide
                .as_ref()
                .map(|path| {
                    String::from_utf8(bounded_read(
                        &contained_file(&registration.directory, path)?,
                        65536,
                    )?)
                    .map_err(anyhow::Error::from)
                })
                .transpose()?;
            json!({"manifest":manifest,"guide":guide,"directory":registration.directory,"dev":registration.dev})
        }
        PluginCommand::Remove { name } => {
            fs::remove_file(registration_path(&plugin_home()?, &name)?)?;
            json!({"removed":name,"statePreserved":true})
        }
    };
    crate::app::output::print_json_output(&result, true)?;
    Ok(())
}

pub(crate) fn run(args: Vec<OsString>, project: Option<&Path>) -> Result<()> {
    let name = args
        .first()
        .and_then(|s| s.to_str())
        .context("Missing plugin command")?;
    let home = plugin_home()?;
    if !registration_path(&home, name)?.try_exists()? {
        bail!("Unknown command '{name}'. Use rbx --help or rbx plugin list");
    }
    let (registration, manifest) = installed(&home, name)?;
    let matches = match manifest::command(&manifest).try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let (name, options) = matches
        .subcommand()
        .context("Choose a plugin command; see --help")?;
    let definition = &manifest.commands[name];
    let arguments = manifest::arguments(definition, options);
    let session = matches
        .get_one::<String>("session")
        .cloned()
        .or_else(|| std::env::var("RENIUM_SESSION_ID").ok())
        .or_else(|| std::env::var("CODEX_THREAD_ID").ok());
    let (exe, argv) = executable(&registration.directory, &manifest)?;
    if !registration.dev && file_hash(&exe)? != registration.executable_hash {
        bail!("Plugin executable changed; inspect/rebuild it and reinstall");
    }
    let state_directory = home.join("state").join(&manifest.name);
    fs::create_dir_all(&state_directory)?;
    let invocation = Invocation {
        protocol: PROTOCOL,
        command: name.into(),
        arguments,
        context: PluginContext {
            plugin: manifest.name,
            directory: registration.directory.clone(),
            state_directory,
            workspace: fs::canonicalize(std::env::current_dir()?)?,
            project: project.map(std::path::absolute).transpose()?,
            place: crate::studio::target::place_filter(),
            session,
            resource_lease: None,
            renium: std::env::current_exe()?,
            permissions: manifest.permissions,
        },
    };
    let mut command = Command::new(exe);
    command
        .args(argv)
        .current_dir(&invocation.context.workspace);
    command.env_remove("RENIUM_RESOURCE_LEASE");
    let mut input = serde_json::to_vec(&invocation)?;
    input.push(b'\n');
    let output = renium_plugin_sdk::process::output(
        command,
        &input,
        Duration::from_secs(definition.timeout_seconds),
    )?;
    if !output.stderr.is_empty() {
        crate::log_global(
            4,
            format_args!(
                "plugin {}: {}",
                invocation.context.plugin,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
    }
    let response: Value = serde_json::from_slice(&output.stdout)
        .context("Plugin returned invalid JSON; stdout is reserved for its protocol")?;
    if response["protocol"].as_u64() != Some(u64::from(PROTOCOL)) {
        bail!("Unsupported plugin response protocol");
    }
    if response["ok"].as_bool() != Some(true) {
        bail!(
            "{}",
            response["error"]
                .as_str()
                .unwrap_or("Plugin failed without an error")
        );
    }
    if !output.status.success() {
        bail!("Plugin reported success but exited with {}", output.status);
    }
    let result = response
        .get("result")
        .context("Plugin response omitted result")?;
    crate::app::output::print_json_output(result, true)?;
    Ok(())
}

pub(crate) fn environment_claim() -> Result<Option<lease::Claim>> {
    std::env::var("RENIUM_RESOURCE_LEASE")
        .ok()
        .map(|raw| serde_json::from_str(&raw).context("Invalid resource lease"))
        .transpose()
}

pub(crate) fn verify_lease_ack(required: bool, response: &Value) -> Result<()> {
    if required && response["resourceLeaseProtected"].as_bool() != Some(true) {
        bail!(
            "The running daemon did not confirm exclusive Studio ownership. Restart Renium's daemon with this build before retrying; no command was sent"
        );
    }
    Ok(())
}

pub(crate) fn verify_place_lease(place: Option<i64>, claim: Option<&lease::Claim>) -> Result<()> {
    match place.filter(|id| *id > 0) {
        Some(id) => lease::Registry::user()?.verify(&format!("studio-place-{id}"), claim),
        None if claim.is_some() => {
            bail!("A leased Studio operation requires an explicit published place ID")
        }
        _ => Ok(()),
    }
}
