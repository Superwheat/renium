//! Renium plugins receive one JSON invocation and return one JSON result.
//! stdout belongs to the protocol; diagnostic messages belong on stderr.
use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
pub use serde_json::{Value, json};

pub mod lease;
pub mod process;

pub const PROTOCOL: u32 = 1;
pub const MANIFEST: &str = "renium-plugin.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    pub description: String,
    /// OS names: windows, macos, linux; default is an optional portable fallback.
    pub executable: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub guide: Option<PathBuf>,
    pub commands: BTreeMap<String, PluginCommand>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginCommand {
    pub description: String,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    20
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Argument {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: ArgumentType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgumentType {
    String,
    Integer,
    Boolean,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Invocation {
    pub protocol: u32,
    pub command: String,
    pub arguments: Value,
    pub context: PluginContext,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginContext {
    pub plugin: String,
    pub directory: PathBuf,
    pub state_directory: PathBuf,
    pub workspace: PathBuf,
    pub project: Option<PathBuf>,
    pub place: Option<String>,
    pub session: Option<String>,
    #[serde(default)]
    pub resource_lease: Option<lease::Claim>,
    pub renium: PathBuf,
    pub permissions: Vec<String>,
}

impl PluginContext {
    /// Calls Renium without a shell. Uses the host's exact binary and project binding.
    pub fn renium(&self, args: &[&str]) -> Result<Value> {
        self.renium_with_input(args, None, Duration::from_secs(20))
    }

    pub fn renium_with_input(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Value> {
        if !self.permissions.iter().any(|p| p == "renium") {
            bail!("Plugin does not declare the renium permission");
        }
        let mut command = Command::new(&self.renium);
        command
            .current_dir(&self.workspace)
            .args(["--output-mode", "json"]);
        command.env("RENIUM_PLUGIN_CHILD", "1");
        command.env_remove("RENIUM_RESOURCE_LEASE");
        if let Some(claim) = &self.resource_lease {
            command.env("RENIUM_RESOURCE_LEASE", serde_json::to_string(claim)?);
        }
        if let Some(project) = &self.project {
            command.arg("--project").arg(project);
        }
        if let Some(place) = &self.place {
            command.arg("--place").arg(place);
        }
        command.args(args);
        let output = process::output(command, input.unwrap_or_default(), timeout)?;
        if !output.status.success() {
            bail!(
                "Renium command failed ({}): {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        serde_json::from_slice(&output.stdout).context("Renium did not return JSON")
    }

    /// A different binding affects only this value; never rewrites the production project.
    pub fn target(&self, project: &Path, place: &str) -> Self {
        let mut target = self.clone();
        target.project = Some(project.to_path_buf());
        target.place = Some(place.to_owned());
        target
    }

    pub fn session(&self) -> Result<&str> {
        self.session.as_deref().context(
            "This workflow needs a session identity: use --session ID or RENIUM_SESSION_ID",
        )
    }

    /// A fresh, flattened projection with separate sync metadata. No Studio or cloud writes.
    pub fn snapshot(&self, destination: &Path) -> Result<Value> {
        self.renium(&[
            "plugin",
            "snapshot",
            destination.to_str().context("Snapshot path is not UTF-8")?,
        ])
    }
}

/// Implement one handler. Renium generates arguments and help from the manifest.
pub fn serve(handler: impl FnOnce(Invocation) -> Result<Value>) -> std::process::ExitCode {
    let result = (|| {
        let mut line = String::new();
        io::stdin().lock().take(1_048_577).read_line(&mut line)?;
        if line.len() > 1_048_576 {
            bail!("Plugin invocation exceeds 1 MiB");
        }
        let invocation: Invocation =
            serde_json::from_str(&line).context("Invalid plugin invocation")?;
        if invocation.protocol != PROTOCOL {
            bail!("Unsupported plugin protocol {}", invocation.protocol);
        }
        handler(invocation)
    })();
    let (response, status) = match result {
        Ok(value) => (
            json!({"protocol":PROTOCOL,"ok":true,"result":value}),
            std::process::ExitCode::SUCCESS,
        ),
        Err(error) => (
            json!({"protocol":PROTOCOL,"ok":false,"error":format!("{error:#}")}),
            std::process::ExitCode::FAILURE,
        ),
    };
    let mut stdout = io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &response).is_err() || writeln!(stdout).is_err() {
        return std::process::ExitCode::FAILURE;
    }
    status
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn plugin_home() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("RENIUM_PLUGIN_HOME") {
        return std::path::absolute(root).map_err(Into::into);
    }
    let root = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?)
            .join("Renium")
    } else {
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".renium")
    };
    Ok(root.join("plugins"))
}
