use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::app::timing::current_millis;
use crate::system::files::read_file_if_present;

const DEFAULT_UPDATE_MANIFEST: &str =
    "https://github.com/Superwheat/renium/releases/latest/download/update-manifest.json";
const UPDATE_PUBLIC_KEY: &str = "rgtfzbsFaGc3ZiDXdBcZ4KMLhaKcuv1BSD7b8D1lt7I=";
const SHARED_CORE_LAUNCHERS: [&str; 3] = ["rbx", "rbx.cmd", "rbx-run.ps1"];
const AGENT_INSTRUCTIONS_FILE: &str = "renium-agents.md";
const AGENT_GUIDES_DIRECTORY: &str = "renium-guides";

#[path = "update/check.rs"]
mod check;

#[derive(Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: UpdateCommand,
}

#[derive(Subcommand)]
pub enum UpdateCommand {
    Check(UpdateCheckArgs),
    Apply(UpdateApplyArgs),
}

#[derive(Args)]
pub struct UpdateCheckArgs {
    #[arg(long, default_value = DEFAULT_UPDATE_MANIFEST)]
    pub manifest: String,
}

#[derive(Args)]
pub struct UpdateApplyArgs {
    #[arg(long, default_value = DEFAULT_UPDATE_MANIFEST)]
    pub manifest: String,
    #[arg(long, value_delimiter = ',')]
    pub component: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
    #[arg(long, requires = "editor_cli")]
    pub extension_root: Option<PathBuf>,
    #[arg(long, requires = "extension_root")]
    pub editor_cli: Option<PathBuf>,
}

#[derive(Args)]
pub struct UpdateHelperArgs {
    #[arg(long)]
    pub parent_pid: u32,
    #[arg(long)]
    pub plan: PathBuf,
    #[arg(long)]
    pub result: PathBuf,
    #[arg(long)]
    pub fallback_result: PathBuf,
    #[arg(long)]
    pub transaction_id: String,
}

#[derive(Serialize, Deserialize)]
struct SignedUpdateManifest {
    payload: UpdatePayload,
    signature: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePayload {
    schema_version: u32,
    version: String,
    components: BTreeMap<String, PlatformArtifacts>,
}

#[derive(Serialize, Deserialize)]
struct PlatformArtifacts {
    cli: Option<UpdateArtifact>,
    plugin: Option<UpdateArtifact>,
    extension: Option<UpdateArtifact>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum UpdateComponent {
    Cli,
    Plugin,
    Extension,
}

impl UpdateComponent {
    fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "cli" => Ok(Self::Cli),
            "plugin" => Ok(Self::Plugin),
            "extension" => Ok(Self::Extension),
            _ => bail!("Unknown update component '{value}'"),
        }
    }

    fn available(self, artifacts: &PlatformArtifacts) -> bool {
        match self {
            Self::Cli => artifacts.cli.is_some(),
            Self::Plugin => artifacts.plugin.is_some(),
            Self::Extension => artifacts.extension.is_some(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Plugin => "plugin",
            Self::Extension => "extension",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct UpdateArtifact {
    url: String,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
struct DeferredUpdateResult {
    ok: bool,
    version: String,
    target: PathBuf,
    error: Option<String>,
    helper: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeferredUpdatePlan {
    transaction_id: String,
    phase: String,
    originals: Option<DeferredUpdateOriginals>,
    version: String,
    target: PathBuf,
    stage: PathBuf,
    core_stage: Option<PathBuf>,
    managed_studio_core_stage: Option<PathBuf>,
    plugin: Option<DeferredFileInstall>,
    extension_installs: Vec<DeferredExtensionInstall>,
    components: Vec<UpdateComponent>,
}

#[derive(Clone, Serialize, Deserialize)]
struct EditorExtensionInstall {
    cli: PathBuf,
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct DeferredExtensionInstall {
    source: PathBuf,
    editors: Vec<EditorExtensionInstall>,
}

#[derive(Serialize, Deserialize)]
struct DeferredFileInstall {
    source: PathBuf,
    target: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateHelperReservation {
    transaction_id: String,
    helper: PathBuf,
    parent_pid: u32,
    parent_start_identity: String,
    helper_pid: Option<u32>,
    helper_start_identity: Option<String>,
    phase: String,
}

#[derive(Serialize, Deserialize)]
struct DeferredUpdateOriginals {
    file_backups: Vec<PathBackup>,
    extension_backups: Vec<ExtensionRootBackup>,
    core_backups: Vec<PathBackup>,
    managed_studio_backup: Option<ManagedStudioBackup>,
}

#[derive(Serialize, Deserialize)]
struct PathBackup {
    target: PathBuf,
    backup: PathBuf,
    existed: bool,
    directory: bool,
    sha256: Option<String>,
}

pub(crate) struct LifecycleLock {
    path: PathBuf,
    token: String,
    owned: bool,
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        if self.owned
            && fs::read_to_string(&self.path).is_ok_and(|value| value.trim() == self.token)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl LifecycleLock {
    #[cfg(target_os = "macos")]
    fn apply_to_command(&self, command: &mut Command) {
        command.env("RENIUM_LIFECYCLE_LOCK_TOKEN", &self.token);
    }
}

struct LifecycleLockOwner {
    pid: u32,
    start: Option<String>,
}

fn parse_lifecycle_lock_owner(value: &str) -> Option<LifecycleLockOwner> {
    let value = value.trim();
    let mut fields = value.split('\t');
    if let (Some(pid), Some(start), Some(token), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
        && !start.is_empty()
        && !token.is_empty()
        && let Ok(pid) = pid.parse::<u32>()
    {
        return Some(LifecycleLockOwner {
            pid,
            start: Some(start.to_string()),
        });
    }
    let pid = value
        .split_once(':')
        .and_then(|(pid, token)| (!token.is_empty()).then_some(pid))
        .and_then(|pid| pid.parse::<u32>().ok())?;
    Some(LifecycleLockOwner { pid, start: None })
}

fn lifecycle_lock_owner_is_alive(owner: &LifecycleLockOwner) -> bool {
    crate::daemon::is_process_alive(owner.pid)
        && owner
            .start
            .as_ref()
            .is_none_or(|expected| process_start_identity(owner.pid).as_ref() == Some(expected))
}

pub(crate) fn process_start_identity(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        if !ok {
            return None;
        }
        Some(
            ((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
                .to_string(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let tail = stat.rsplit_once(')')?.1.trim();
        tail.split_whitespace().nth(19).map(str::to_string)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let value = String::from_utf8(output.stdout).ok()?;
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    }
}

pub(crate) fn acquire_lifecycle_lock() -> Result<LifecycleLock> {
    let root = lifecycle_lock_dir()?;
    fs::create_dir_all(&root)?;
    let path = root.join("lifecycle.lock");
    if let Ok(token) = env::var("RENIUM_LIFECYCLE_LOCK_TOKEN") {
        let owner = fs::read_to_string(&path)
            .with_context(|| "The inherited Renium lifecycle lock no longer exists")?;
        if owner.trim() != token.trim() {
            bail!("The inherited Renium lifecycle lock is no longer owned by its parent");
        }
        let parsed = parse_lifecycle_lock_owner(&owner)
            .context("The inherited Renium lifecycle lock is malformed")?;
        if !lifecycle_lock_owner_is_alive(&parsed) {
            bail!("The inherited Renium lifecycle lock owner is no longer running");
        }
        return Ok(LifecycleLock {
            path,
            token: owner.trim().to_string(),
            owned: false,
        });
    }
    let cleanup_path = root.join("lifecycle.lock.cleanup");
    let deadline = Instant::now() + Duration::from_secs(1);
    let start = process_start_identity(std::process::id())
        .context("Could not read this process's start identity")?;
    let token = format!("{}\t{}\t{}", std::process::id(), start, current_millis());
    let temporary = root.join(format!(
        ".lifecycle.lock.{}.{}.tmp",
        std::process::id(),
        current_millis()
    ));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    loop {
        if cleanup_path.exists() {
            if Instant::now() >= deadline {
                let _ = fs::remove_file(&temporary);
                bail!("Another Renium lifecycle lock operation is still finishing");
            }
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        match fs::hard_link(&temporary, &path) {
            Ok(()) => {
                let _ = fs::remove_file(&temporary);
                let lifecycle_lock = LifecycleLock {
                    path,
                    token,
                    owned: true,
                };
                validate_update_helper_reservation_for_current_process()?;
                return Ok(lifecycle_lock);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = if path.is_dir() {
                    fs::read_to_string(path.join("owner")).ok()
                } else {
                    fs::read_to_string(&path).ok()
                };
                let Some(holder) = holder else {
                    if Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let _ = fs::remove_file(&temporary);
                    bail!(
                        "The Renium lifecycle lock is incomplete and could not be safely recovered"
                    );
                };
                let Some(owner) = parse_lifecycle_lock_owner(&holder) else {
                    if Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let _ = fs::remove_file(&temporary);
                    bail!("The Renium lifecycle lock is malformed");
                };
                if lifecycle_lock_owner_is_alive(&owner) {
                    let _ = fs::remove_file(&temporary);
                    bail!("Another Renium install, update, or uninstall is running");
                }
                match fs::create_dir(&cleanup_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        if Instant::now() >= deadline {
                            let _ = fs::remove_file(&temporary);
                            bail!("Another Renium lifecycle lock operation is still finishing");
                        }
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    Err(error) => {
                        let _ = fs::remove_file(&temporary);
                        return Err(error)
                            .context("Failed to reserve stale lifecycle lock cleanup");
                    }
                }
                let cleanup = crate::system::files::OnDrop::new(|| {
                    let _ = fs::remove_dir(&cleanup_path);
                });
                let current = if path.is_dir() {
                    fs::read_to_string(path.join("owner")).ok()
                } else {
                    fs::read_to_string(&path).ok()
                };
                if current.as_deref().map(str::trim) != Some(holder.trim()) {
                    drop(cleanup);
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                if lifecycle_lock_owner_is_alive(&owner) {
                    drop(cleanup);
                    let _ = fs::remove_file(&temporary);
                    bail!("Another Renium install, update, or uninstall is running");
                }
                if path.is_dir() {
                    fs::remove_dir_all(&path)
                        .with_context(|| format!("Failed to clear stale {}", path.display()))?;
                } else {
                    fs::remove_file(&path)
                        .with_context(|| format!("Failed to clear stale {}", path.display()))?;
                }
                drop(cleanup);
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error).with_context(|| format!("Failed to create {}", path.display()));
            }
        }
    }
}

fn update_helper_reservation_path() -> Result<PathBuf> {
    Ok(lifecycle_state_dir()?.join("update-helper-reservation.json"))
}

fn read_update_helper_reservation() -> Result<Option<UpdateHelperReservation>> {
    let path = update_helper_reservation_path()?;
    recover_file_install(&path)?;
    if !path.is_file() {
        return Ok(None);
    }
    let reservation = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid update helper reservation {}", path.display()))?;
    Ok(Some(reservation))
}

fn write_update_helper_reservation(reservation: &UpdateHelperReservation) -> Result<()> {
    let path = update_helper_reservation_path()?;
    let mut bytes = serde_json::to_vec_pretty(reservation)?;
    bytes.push(b'\n');
    install_bytes(&path, &bytes)
}

fn clear_update_helper_reservation(transaction_id: &str) -> Result<()> {
    let path = update_helper_reservation_path()?;
    recover_file_install(&path)?;
    if !path.is_file() {
        return Ok(());
    }
    let reservation = read_update_helper_reservation()?
        .context("The update helper reservation disappeared while clearing it")?;
    if reservation.transaction_id != transaction_id {
        bail!("A different Renium update helper reservation is active");
    }
    fs::remove_file(&path).with_context(|| format!("Failed to remove {}", path.display()))
}

fn reservation_process_is_alive(pid: u32, expected_start: &str) -> bool {
    crate::daemon::is_process_alive(pid)
        && process_start_identity(pid).as_deref() == Some(expected_start)
}

fn validate_update_helper_reservation_for_current_process() -> Result<()> {
    let Some(reservation) = read_update_helper_reservation()? else {
        return Ok(());
    };
    let current_pid = std::process::id();
    let current_start = process_start_identity(current_pid);
    if let (Some(helper_pid), Some(helper_start)) = (
        reservation.helper_pid,
        reservation.helper_start_identity.as_deref(),
    ) && reservation_process_is_alive(helper_pid, helper_start)
    {
        if helper_pid == current_pid && current_start.as_deref() == Some(helper_start) {
            return Ok(());
        }
        bail!("A Renium update helper is still running");
    }
    if reservation_process_is_alive(reservation.parent_pid, &reservation.parent_start_identity) {
        bail!("A Renium update is waiting for its helper to take ownership");
    }
    Ok(())
}

pub fn run_update(args: UpdateArgs) -> Result<()> {
    match args.command {
        UpdateCommand::Check(args) => {
            let manifest = check::manifest(&args.manifest)?;
            let current = Version::parse(crate::app::build::VERSION).with_context(|| {
                format!("Invalid current version {}", crate::app::build::VERSION)
            })?;
            let latest = Version::parse(&manifest.payload.version)
                .with_context(|| format!("Invalid release version {}", manifest.payload.version))?;
            let platform = platform_key();
            let components = manifest.payload.components.get(&platform);
            let response = json!({
                "ok": true,
                "currentVersion": crate::app::build::VERSION,
                "latestVersion": manifest.payload.version,
                "updateAvailable": latest > current,
                "downgrade": latest < current,
                "platform": platform,
                "components": components,
                "signature": "verified",
                "signatureError": Option::<String>::None,
            });
            let text = match latest.cmp(&current) {
                std::cmp::Ordering::Greater => format!(
                    "Renium {} is available; installed version is {}",
                    manifest.payload.version,
                    crate::app::build::VERSION
                ),
                std::cmp::Ordering::Equal => {
                    format!("Renium {} is up to date", crate::app::build::VERSION)
                }
                std::cmp::Ordering::Less => format!(
                    "Installed Renium {} is newer than release {}",
                    crate::app::build::VERSION,
                    manifest.payload.version
                ),
            };
            crate::app::output::emit_global_output(&response, &text)
        }
        UpdateCommand::Apply(args) => {
            if delegate_extension_owned_update(&args)? {
                return Ok(());
            }
            let lifecycle_lock = acquire_lifecycle_lock()?;
            recover_running_core_install()?;
            if let Some(version) = recover_pending_update_transaction(&lifecycle_lock)? {
                return crate::app::output::emit_global_output(
                    &json!({
                        "ok": true,
                        "version": version,
                        "recoveryScheduled": true,
                        "restartRequired": true,
                    }),
                    &format!(
                        "Rescheduled the interrupted Renium {version} update; it will finish after this process exits"
                    ),
                );
            }
            cleanup_orphaned_update_stages()?;
            let manifest = fetch_manifest(&args.manifest)?;
            verify_manifest(&manifest)?;
            apply_update(
                manifest,
                &args.component,
                args.dry_run,
                args.force,
                args.extension_root.as_deref(),
                args.editor_cli.as_deref(),
                &lifecycle_lock,
            )
        }
    }
}

fn delegate_extension_owned_update(args: &UpdateApplyArgs) -> Result<bool> {
    let current = env::current_exe().context("Failed to locate the running Renium CLI")?;
    if !cli_is_extension_owned(&current) {
        return Ok(false);
    }
    let Some(cli) = find_user_wide_cli(&current) else {
        return Ok(false);
    };
    let mut command = Command::new(&cli);
    command
        .args(["update", "apply", "--manifest"])
        .arg(&args.manifest);
    for component in &args.component {
        command.args(["--component", component]);
    }
    if args.dry_run {
        command.arg("--dry-run");
    }
    if args.force {
        command.arg("--force");
    }
    if let Some(root) = args.extension_root.as_deref() {
        command.arg("--extension-root").arg(root);
    }
    if let Some(cli) = args.editor_cli.as_deref() {
        command.arg("--editor-cli").arg(cli);
    }
    let status = command
        .status()
        .with_context(|| format!("Failed to start {}", cli.display()))?;
    if !status.success() {
        bail!("User-wide Renium update exited with {status}");
    }
    Ok(true)
}

fn find_user_wide_cli(current: &Path) -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "renium.exe"
    } else {
        "renium"
    };
    let mut candidates = Vec::new();
    if let Ok(root) = user_data_dir() {
        candidates.push(root.join(executable));
        candidates.push(root.join("bin").join(executable));
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/share/renium").join(executable));
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(env::split_paths(&path).map(|root| root.join(executable)));
    }
    let current = fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
    candidates.into_iter().find(|candidate| {
        candidate.is_file()
            && fs::canonicalize(candidate).ok().is_some_and(|candidate| {
                !paths_equal(&candidate, &current) && !cli_is_extension_owned(&candidate)
            })
    })
}

fn fetch_manifest(source: &str) -> Result<SignedUpdateManifest> {
    let bytes = if source.starts_with("https://") {
        download(source, "update-manifest.json")?
    } else if source.starts_with("http://") {
        bail!("Update manifests must use HTTPS or a local file");
    } else {
        fs::read(source).with_context(|| format!("Failed to read {source}"))?
    };
    parse_manifest(&bytes)
}

fn parse_manifest(bytes: &[u8]) -> Result<SignedUpdateManifest> {
    let manifest: SignedUpdateManifest =
        serde_json::from_slice(bytes).context("Invalid update manifest")?;
    if manifest.payload.schema_version != 1 {
        bail!(
            "Unsupported update manifest schema {}",
            manifest.payload.schema_version
        );
    }
    Ok(manifest)
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn latest_release_version() -> Result<String> {
    let manifest = check::manifest(DEFAULT_UPDATE_MANIFEST)?;
    Version::parse(&manifest.payload.version)
        .with_context(|| format!("Invalid release version {}", manifest.payload.version))?;
    Ok(manifest.payload.version)
}

fn verify_manifest(manifest: &SignedUpdateManifest) -> Result<()> {
    let raw_key = env::var("RENIUM_UPDATE_PUBLIC_KEY")
        .ok()
        .or_else(|| option_env!("RENIUM_UPDATE_PUBLIC_KEY").map(str::to_string))
        .unwrap_or_else(|| UPDATE_PUBLIC_KEY.to_string());
    let key_bytes = base64::decode(raw_key.trim()).context("Invalid update public key base64")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Update public key must be 32 bytes"))?;
    let signature_bytes =
        base64::decode(&manifest.signature).context("Invalid manifest signature base64")?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|error| anyhow::anyhow!("Invalid manifest signature: {error}"))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|error| anyhow::anyhow!("Invalid update public key: {error}"))?;
    let payload = serde_json::to_vec(&manifest.payload)?;
    key.verify(&payload, &signature)
        .context("Update manifest signature verification failed")
}

fn apply_update(
    manifest: SignedUpdateManifest,
    requested: &[String],
    dry_run: bool,
    force: bool,
    extension_root: Option<&Path>,
    editor_cli: Option<&Path>,
    _lifecycle_lock: &LifecycleLock,
) -> Result<()> {
    let platform = platform_key();
    let components = manifest
        .payload
        .components
        .get(&platform)
        .with_context(|| {
            format!(
                "Release {} has no {platform} artifacts",
                manifest.payload.version
            )
        })?;
    let mut requested = if requested.is_empty() {
        default_update_components(components)?
    } else if requested
        .iter()
        .any(|name| name.eq_ignore_ascii_case("all"))
    {
        [
            UpdateComponent::Cli,
            UpdateComponent::Plugin,
            UpdateComponent::Extension,
        ]
        .into_iter()
        .filter(|component| component.available(components))
        .collect()
    } else {
        requested
            .iter()
            .map(|name| UpdateComponent::parse(name))
            .collect::<Result<Vec<_>>>()?
    };
    requested.sort();
    requested.dedup();
    if requested.contains(&UpdateComponent::Cli)
        && env::current_exe()
            .ok()
            .is_some_and(|path| cli_is_extension_owned(&path))
    {
        if components.extension.is_none() {
            bail!(
                "This CLI is owned by the editor extension, but release {} has no extension artifact",
                manifest.payload.version
            );
        }
        requested.retain(|component| *component != UpdateComponent::Cli);
        if !requested.contains(&UpdateComponent::Extension) {
            requested.push(UpdateComponent::Extension);
            requested.sort();
        }
    }
    let current = Version::parse(crate::app::build::VERSION)
        .with_context(|| format!("Invalid current version {}", crate::app::build::VERSION))?;
    let release = Version::parse(&manifest.payload.version)
        .with_context(|| format!("Invalid release version {}", manifest.payload.version))?;
    if release < current && !force {
        bail!(
            "Release {} is older than installed version {}; pass --force to downgrade",
            manifest.payload.version,
            crate::app::build::VERSION
        );
    }
    if release != current {
        for component in default_update_components(components)? {
            if !requested.contains(&component) {
                requested.push(component);
            }
        }
        requested.sort();
    }
    for component in &requested {
        if !component.available(components) {
            bail!(
                "Release {} has no {} artifact for {platform}",
                manifest.payload.version,
                component.name()
            );
        }
    }
    let apply_cli = requested.contains(&UpdateComponent::Cli);
    let update_plugin = requested.contains(&UpdateComponent::Plugin);
    let repair_core = cfg!(target_os = "macos") && update_plugin;
    if repair_core && components.cli.is_none() {
        bail!(
            "Release {} has no core artifact required to update managed Studio",
            manifest.payload.version
        );
    }
    let plugin_target = update_plugin
        .then(|| {
            crate::app::setup::roblox_plugins_dir()
                .map(|dir| dir.join(crate::app::setup::PLUGIN_ASSET_NAME))
        })
        .transpose()?;
    let editor_installs = if requested.contains(&UpdateComponent::Extension) {
        match (extension_root, editor_cli) {
            (Some(root), Some(cli)) => vec![selected_extension_editor(root, cli)?],
            (None, None) => find_installed_extension_editors()?,
            _ => unreachable!("clap requires extension update arguments together"),
        }
    } else {
        Vec::new()
    };
    let extension_groups = group_editor_installs_by_platform(&editor_installs)?;
    for target_platform in extension_groups.keys() {
        if manifest
            .payload
            .components
            .get(target_platform)
            .and_then(|target| target.extension.as_ref())
            .is_none()
        {
            bail!(
                "Release {} has no extension artifact for {target_platform}",
                manifest.payload.version
            );
        }
    }
    if apply_cli {
        let target = env::current_exe().context("Failed to locate the running Renium CLI")?;
        target
            .parent()
            .context("The running Renium CLI has no installation directory")?;
    }
    #[cfg(target_os = "macos")]
    let managed_studio_platform = update_plugin
        .then(crate::studio::native::serializer::source_studio_platform_key)
        .transpose()?;
    #[cfg(not(target_os = "macos"))]
    let managed_studio_platform: Option<String> = None;
    #[cfg(target_os = "macos")]
    if let Some(target_platform) = managed_studio_platform.as_deref() {
        crate::studio::native::serializer::managed_studio_path()?;
        if manifest
            .payload
            .components
            .get(target_platform)
            .and_then(|target| target.cli.as_ref())
            .is_none()
        {
            bail!(
                "Release {} has no core artifact for managed Studio on {target_platform}",
                manifest.payload.version
            );
        }
    }
    if dry_run {
        return crate::app::output::emit_global_output(
            &json!({
                "ok": true,
                "version": manifest.payload.version,
                "platform": platform,
                "extensionPlatforms": extension_groups.keys().collect::<Vec<_>>(),
                "managedStudioPlatform": managed_studio_platform,
                "components": requested,
                "dryRun": true,
            }),
            &format!(
                "Would update {} component(s) to {}",
                requested.len(),
                manifest.payload.version
            ),
        );
    }
    let stage = fresh_temp_dir(&format!("renium-update-{}", manifest.payload.version))?;
    let staged: Result<_> = (|| {
        let plugin = if requested.contains(&UpdateComponent::Plugin) {
            Some(fetch_artifact(
                components
                    .plugin
                    .as_ref()
                    .context("This release has no Studio plugin artifact")?,
                &stage,
                "Renium.rbxm",
            )?)
        } else {
            None
        };
        let mut staged_extensions = Vec::new();
        for (index, (target_platform, editors)) in extension_groups.iter().enumerate() {
            let file_name = format!("renium-{index}.vsix");
            fetch_artifact(
                manifest
                    .payload
                    .components
                    .get(target_platform)
                    .and_then(|target| target.extension.as_ref())
                    .with_context(|| {
                        format!(
                            "Release {} has no extension artifact for {target_platform}",
                            manifest.payload.version
                        )
                    })?,
                &stage,
                &file_name,
            )?;
            staged_extensions.push(DeferredExtensionInstall {
                source: stage.join(file_name),
                editors: editors.clone(),
            });
        }
        let core = if apply_cli || managed_studio_platform.as_deref() == Some(platform.as_str()) {
            let bytes = fetch_artifact(
                components
                    .cli
                    .as_ref()
                    .context("This release has no core artifact")?,
                &stage,
                "core.zip",
            )?;
            Some(extract_core_bundle(&bytes, &stage.join("core"))?)
        } else {
            None
        };
        let managed_studio_core = if let Some(target_platform) = managed_studio_platform.as_deref()
        {
            if target_platform == platform {
                core.clone()
            } else {
                let bytes = fetch_artifact(
                    manifest
                        .payload
                        .components
                        .get(target_platform)
                        .and_then(|target| target.cli.as_ref())
                        .with_context(|| {
                            format!(
                                "Release {} has no core artifact for managed Studio on {target_platform}",
                                manifest.payload.version
                            )
                        })?,
                    &stage,
                    "managed-studio-core.zip",
                )?;
                Some(extract_core_bundle(
                    &bytes,
                    &stage.join("managed-studio-core"),
                )?)
            }
        } else {
            None
        };
        Ok((plugin, staged_extensions, core, managed_studio_core))
    })();
    let (plugin_bytes, staged_extensions, core_root, managed_studio_core_root) = match staged {
        Ok(artifacts) => artifacts,
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
    };
    if let Some(bytes) = plugin_bytes.as_deref() {
        crate::app::setup::validate_rbxm_version(bytes, &manifest.payload.version)?;
    }
    if let Err(error) = crate::project::workflows::stop_all_daemons_for_update() {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    let target = env::current_exe().context("Failed to locate the running Renium CLI")?;
    let mut plan = DeferredUpdatePlan {
        transaction_id: format!("{}-{}", std::process::id(), current_millis()),
        phase: "staged".to_string(),
        originals: None,
        version: manifest.payload.version.clone(),
        target,
        stage: stage.clone(),
        core_stage: if apply_cli { core_root } else { None },
        managed_studio_core_stage: managed_studio_core_root,
        plugin: plugin_target.as_ref().map(|target| DeferredFileInstall {
            source: stage.join("Renium.rbxm"),
            target: target.clone(),
        }),
        extension_installs: staged_extensions,
        components: requested.clone(),
    };
    plan.originals = match prepare_update_originals(&plan) {
        Ok(originals) => Some(originals),
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
    };
    plan.phase = "prepared".to_string();
    #[cfg(windows)]
    {
        schedule_windows_update(&plan, plan.core_stage.as_deref())?;
        let result_path = deferred_update_result_path()?;
        crate::app::output::emit_global_output(
            &json!({
                "ok": true,
                "version": manifest.payload.version,
                "platform": platform,
                "applied": [],
                "scheduled": requested,
                "restartRequired": true,
                "resultPath": result_path,
            }),
            &format!(
                "Scheduled the Renium {} update; it will finish after this process exits",
                manifest.payload.version
            ),
        )
    }
    #[cfg(not(windows))]
    {
        write_pending_update_transaction(&plan)?;
        plan.phase = "applying".to_string();
        write_pending_update_transaction(&plan)?;
        let result = (|| -> Result<(Vec<UpdateComponent>, Vec<UpdateComponent>)> {
            apply_staged_update_plan(&plan, _lifecycle_lock)?;
            Ok((plan.components.clone(), Vec::new()))
        })();
        let (applied, scheduled) = match result {
            Ok(value) => value,
            Err(error) => {
                let rollback = plan
                    .originals
                    .as_ref()
                    .context("The update transaction has no original-state baseline")
                    .and_then(restore_update_originals);
                if let Err(rollback_error) = rollback {
                    return Err(error).context(format!(
                        "Update rollback was incomplete: {rollback_error:#}"
                    ));
                }
                clear_pending_update_transaction()?;
                let _ = fs::remove_dir_all(&stage);
                return Err(error);
            }
        };
        plan.phase = "applied".to_string();
        write_pending_update_transaction(&plan)?;
        clear_pending_update_transaction()?;
        if scheduled.is_empty()
            && let Err(error) = fs::remove_dir_all(&stage)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "[renium] warning: failed to remove update stage {}: {error}",
                stage.display()
            );
        }
        let restart_required = !scheduled.is_empty();
        let text = if scheduled.is_empty() {
            format!(
                "Updated {} component(s) to {}",
                applied.len(),
                manifest.payload.version
            )
        } else {
            format!(
                "Scheduled the Renium CLI replacement for {}; it will finish after this process exits",
                manifest.payload.version
            )
        };
        crate::app::output::emit_global_output(
            &json!({
                "ok": true,
                "version": manifest.payload.version,
                "platform": platform,
                "applied": applied,
                "scheduled": scheduled,
                "restartRequired": restart_required,
            }),
            &text,
        )
    }
}

#[derive(Serialize, Deserialize)]
struct ExtensionRootBackup {
    root: PathBuf,
    backup: PathBuf,
    names: Vec<String>,
    hashes: BTreeMap<String, String>,
    obsolete: Option<Vec<u8>>,
    existed: bool,
}

#[derive(Serialize, Deserialize)]
struct ManagedStudioBackup {
    target: PathBuf,
    backup: PathBuf,
    existed: bool,
    sha256: Option<String>,
}

fn extension_roots() -> Vec<PathBuf> {
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from);
    let mut roots = home
        .iter()
        .flat_map(|home| {
            [
                home.join(".cursor/extensions"),
                home.join(".vscode/extensions"),
                home.join(".vscode-insiders/extensions"),
                home.join(".windsurf/extensions"),
            ]
        })
        .collect::<Vec<_>>();
    if let Some(root) = env::var_os("RENIUM_EXTENSION_ROOT") {
        roots.push(PathBuf::from(root));
    }
    if let Ok(executable) = env::current_exe() {
        for ancestor in executable.ancestors() {
            if ancestor
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.eq_ignore_ascii_case("extensions"))
            {
                roots.push(ancestor.to_path_buf());
                break;
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn default_update_components(components: &PlatformArtifacts) -> Result<Vec<UpdateComponent>> {
    let current = env::current_exe().context("Failed to locate the running Renium CLI")?;
    let extension_owned = cli_is_extension_owned(&current);
    let mut requested = Vec::new();
    if extension_owned {
        if components.extension.is_some() {
            requested.push(UpdateComponent::Extension);
        }
    } else if components.cli.is_some() {
        requested.push(UpdateComponent::Cli);
    }
    if components.extension.is_some() && renium_extension_is_installed() {
        requested.push(UpdateComponent::Extension);
    }
    if components.plugin.is_some()
        && !cfg!(target_os = "linux")
        && crate::app::setup::roblox_plugins_dir()
            .ok()
            .is_some_and(|dir| dir.join(crate::app::setup::PLUGIN_ASSET_NAME).is_file())
    {
        requested.push(UpdateComponent::Plugin);
    }
    requested.sort();
    requested.dedup();
    if requested.is_empty() {
        bail!("No installed Renium components match this release");
    }
    Ok(requested)
}

fn renium_extension_is_installed() -> bool {
    extension_roots()
        .into_iter()
        .any(|root| has_renium_extension(&root))
}

fn fresh_temp_dir(prefix: &str) -> Result<PathBuf> {
    crate::system::files::create_unique_directory(
        &lifecycle_state_dir()?.join("update-stages"),
        &format!("{prefix}-"),
    )
}

fn cleanup_orphaned_update_stages() -> Result<()> {
    let root = lifecycle_state_dir()?.join("update-stages");
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let age = entry.metadata()?.modified()?.elapsed().unwrap_or_default();
        if age > Duration::from_secs(24 * 60 * 60) {
            fs::remove_dir_all(entry.path()).with_context(|| {
                format!(
                    "Failed to remove stale update stage {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

fn is_renium_extension_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name == "local.renium" || name.starts_with("local.renium-")
        })
}

fn has_renium_extension(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries.filter_map(std::result::Result::ok).any(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && is_renium_extension_dir(&entry.path())
        })
    })
}

fn plan_editor_installs(plan: &DeferredUpdatePlan) -> Vec<EditorExtensionInstall> {
    let mut editors = plan
        .extension_installs
        .iter()
        .flat_map(|install| install.editors.iter().cloned())
        .collect::<Vec<_>>();
    editors.sort_by(|left, right| left.root.cmp(&right.root));
    editors.dedup_by(|left, right| paths_equal(&left.root, &right.root));
    editors
}

fn snapshot_extension_installation(
    stage: &Path,
    editors: &[EditorExtensionInstall],
) -> Result<Vec<ExtensionRootBackup>> {
    let mut snapshots = Vec::new();
    let mut roots = editors
        .iter()
        .map(|editor| editor.root.clone())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    for (index, root) in roots.into_iter().enumerate() {
        let backup = stage.join("extension-backup").join(index.to_string());
        let mut names = Vec::new();
        let mut hashes = BTreeMap::new();
        let existed = root.is_dir();
        if existed {
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() || !is_renium_extension_dir(&entry.path()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let destination = backup.join(&name);
                copy_directory(&entry.path(), &destination)?;
                hashes.insert(name.clone(), directory_sha256(&destination)?);
                names.push(name);
            }
        }
        let obsolete = fs::read(root.join(".obsolete")).ok();
        snapshots.push(ExtensionRootBackup {
            root,
            backup,
            names,
            hashes,
            obsolete,
            existed,
        });
    }
    Ok(snapshots)
}

fn restore_extension_installation(snapshots: &[ExtensionRootBackup]) -> Result<()> {
    let mut errors = Vec::new();
    for snapshot in snapshots {
        let result = (|| -> Result<()> {
            if !snapshot.root.is_dir() && snapshot.existed {
                fs::create_dir_all(&snapshot.root)?;
            }
            if !snapshot.root.is_dir() {
                return Ok(());
            }
            for entry in fs::read_dir(&snapshot.root)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() && is_renium_extension_dir(&entry.path()) {
                    fs::remove_dir_all(entry.path())?;
                }
            }
            for name in &snapshot.names {
                let backup = snapshot.backup.join(name);
                let expected = snapshot
                    .hashes
                    .get(name)
                    .with_context(|| format!("Missing extension backup hash for {name}"))?;
                let actual = directory_sha256(&backup)?;
                if !actual.eq_ignore_ascii_case(expected) {
                    bail!("Extension backup {name} no longer matches its recorded hash");
                }
                copy_directory(&backup, &snapshot.root.join(name))?;
            }
            let obsolete = snapshot.root.join(".obsolete");
            if let Some(bytes) = snapshot.obsolete.as_deref() {
                install_bytes(&obsolete, bytes)?;
            } else if obsolete.is_file() {
                fs::remove_file(obsolete)?;
            }
            if !snapshot.existed {
                let _ = fs::remove_dir(&snapshot.root);
            }
            Ok(())
        })();
        if let Err(error) = result {
            errors.push(format!("{}: {error}", snapshot.root.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("Extension rollback was incomplete: {}", errors.join("; "))
    }
}

#[cfg(target_os = "macos")]
fn snapshot_managed_studio(stage: &Path) -> Result<ManagedStudioBackup> {
    let target = crate::studio::native::serializer::managed_studio_path()?;
    let backup = stage.join("managed-studio.previous.app");
    let existed = target.is_dir();
    if existed {
        let status = Command::new("ditto")
            .arg(&target)
            .arg(&backup)
            .status()
            .context("Failed to snapshot the managed Studio app")?;
        if !status.success() {
            bail!("Managed Studio snapshot exited with {status}");
        }
    }
    Ok(ManagedStudioBackup {
        target,
        sha256: existed.then(|| directory_sha256(&backup)).transpose()?,
        backup,
        existed,
    })
}

#[cfg(target_os = "macos")]
fn restore_managed_studio(snapshot: &ManagedStudioBackup) -> Result<()> {
    if snapshot.target.exists() {
        fs::remove_dir_all(&snapshot.target)
            .with_context(|| format!("Failed to remove {}", snapshot.target.display()))?;
    }
    if snapshot.existed {
        let expected = snapshot
            .sha256
            .as_deref()
            .context("The managed Studio backup hash is missing")?;
        let actual = directory_sha256(&snapshot.backup)?;
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("The managed Studio backup no longer matches its recorded hash");
        }
        let status = Command::new("ditto")
            .arg(&snapshot.backup)
            .arg(&snapshot.target)
            .status()
            .context("Failed to restore the managed Studio app")?;
        if !status.success() {
            bail!("Managed Studio restore exited with {status}");
        }
    }
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&source, &destination)?;
        } else {
            fs::copy(&source, &destination)?;
        }
    }
    Ok(())
}

fn directory_sha256(path: &Path) -> Result<String> {
    let mut entries = walkdir::WalkDir::new(path)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    let mut digest = Sha256::new();
    for entry in entries {
        let relative = entry.path().strip_prefix(path)?;
        digest.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        digest.update([0]);
        if entry.file_type().is_dir() {
            digest.update(b"d");
        } else {
            digest.update(b"f");
            digest.update(fs::read(entry.path())?);
        }
        digest.update([0]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn snapshot_path(target: &Path, backup: &Path) -> Result<PathBackup> {
    let existed = target.exists();
    let directory = target.is_dir();
    let sha256 = if !existed {
        None
    } else if directory {
        copy_directory(target, backup)?;
        Some(directory_sha256(backup)?)
    } else {
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(target, backup)?;
        Some(format!("{:x}", Sha256::digest(fs::read(backup)?)))
    };
    Ok(PathBackup {
        target: target.to_path_buf(),
        backup: backup.to_path_buf(),
        existed,
        directory,
        sha256,
    })
}

fn verify_path_backup(backup: &PathBackup) -> Result<()> {
    if !backup.existed {
        return Ok(());
    }
    let expected = backup
        .sha256
        .as_deref()
        .context("A transaction backup hash is missing")?;
    let actual = if backup.directory {
        directory_sha256(&backup.backup)?
    } else {
        format!("{:x}", Sha256::digest(fs::read(&backup.backup)?))
    };
    if !actual.eq_ignore_ascii_case(expected) {
        bail!(
            "Transaction backup {} no longer matches its recorded hash",
            backup.backup.display()
        );
    }
    Ok(())
}

fn restore_path_backup(backup: &PathBackup) -> Result<()> {
    verify_path_backup(backup)?;
    if backup.target.is_dir() {
        fs::remove_dir_all(&backup.target)?;
    } else if backup.target.exists() {
        fs::remove_file(&backup.target)?;
    }
    if !backup.existed {
        return Ok(());
    }
    if backup.directory {
        copy_directory(&backup.backup, &backup.target)
    } else {
        install_bytes(&backup.target, &fs::read(&backup.backup)?)
    }
}

fn shared_core_install_paths(target: &Path, core_root: &Path) -> Result<Vec<PathBuf>> {
    let root = target
        .parent()
        .context("The running Renium CLI has no installation directory")?;
    let mut paths = vec![target.to_path_buf()];
    for name in SHARED_CORE_LAUNCHERS {
        let source = core_root.join(name);
        let destination = root.join(name);
        if source.is_file() && destination.is_file() && is_renium_launcher(&destination) {
            paths.push(destination);
        }
    }
    if core_root.join(AGENT_INSTRUCTIONS_FILE).is_file() {
        paths.push(root.join(AGENT_INSTRUCTIONS_FILE));
    }
    if core_root.join(AGENT_GUIDES_DIRECTORY).is_dir() {
        paths.push(root.join(AGENT_GUIDES_DIRECTORY));
    }
    Ok(paths)
}

fn prepare_update_originals(plan: &DeferredUpdatePlan) -> Result<DeferredUpdateOriginals> {
    let root = plan.stage.join("originals");
    fs::create_dir(&root).with_context(|| format!("Failed to create {}", root.display()))?;
    let file_targets = plan
        .plugin
        .as_ref()
        .into_iter()
        .map(|install| install.target.clone())
        .collect::<Vec<_>>();
    let mut file_backups = Vec::new();
    for (index, target) in file_targets.iter().enumerate() {
        file_backups.push(snapshot_path(
            target,
            &root.join("files").join(index.to_string()),
        )?);
    }
    let editors = plan_editor_installs(plan);
    let extension_backups = if editors.is_empty() {
        Vec::new()
    } else {
        snapshot_extension_installation(&root.join("extensions"), &editors)?
    };
    let mut core_backups = Vec::new();
    if plan.components.contains(&UpdateComponent::Cli)
        && let Some(core_root) = plan.core_stage.as_deref()
    {
        if let Some(target_root) = managed_core_root(&plan.target) {
            core_backups.push(snapshot_path(
                &target_root,
                &root.join("core").join("directory"),
            )?);
        } else {
            for (index, target) in shared_core_install_paths(&plan.target, core_root)?
                .into_iter()
                .enumerate()
            {
                core_backups.push(snapshot_path(
                    &target,
                    &root.join("core").join(index.to_string()),
                )?);
            }
        }
    }
    #[cfg(target_os = "macos")]
    let managed_studio_backup = plan
        .plugin
        .is_some()
        .then(|| snapshot_managed_studio(&root))
        .transpose()?;
    #[cfg(not(target_os = "macos"))]
    let managed_studio_backup = None;
    Ok(DeferredUpdateOriginals {
        file_backups,
        extension_backups,
        core_backups,
        managed_studio_backup,
    })
}

fn verify_update_originals(originals: &DeferredUpdateOriginals) -> Result<()> {
    for backup in originals.file_backups.iter().chain(&originals.core_backups) {
        verify_path_backup(backup)?;
    }
    for snapshot in &originals.extension_backups {
        for name in &snapshot.names {
            let expected = snapshot
                .hashes
                .get(name)
                .with_context(|| format!("Missing extension backup hash for {name}"))?;
            let actual = directory_sha256(&snapshot.backup.join(name))?;
            if !actual.eq_ignore_ascii_case(expected) {
                bail!("Extension backup {name} no longer matches its recorded hash");
            }
        }
    }
    if let Some(snapshot) = originals.managed_studio_backup.as_ref()
        && snapshot.existed
    {
        let expected = snapshot
            .sha256
            .as_deref()
            .context("The managed Studio backup hash is missing")?;
        if !directory_sha256(&snapshot.backup)?.eq_ignore_ascii_case(expected) {
            bail!("The managed Studio backup no longer matches its recorded hash");
        }
    }
    Ok(())
}

fn restore_update_originals(originals: &DeferredUpdateOriginals) -> Result<()> {
    let mut errors = Vec::new();
    for backup in originals.core_backups.iter().rev() {
        if let Err(error) = restore_path_backup(backup) {
            errors.push(format!("{}: {error:#}", backup.target.display()));
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(snapshot) = originals.managed_studio_backup.as_ref() {
        if let Err(error) = restore_managed_studio(snapshot) {
            errors.push(format!("{}: {error:#}", snapshot.target.display()));
        }
    }
    if let Err(error) = restore_extension_installation(&originals.extension_backups) {
        errors.push(format!("{error:#}"));
    }
    for backup in originals.file_backups.iter().rev() {
        if let Err(error) = restore_path_backup(backup) {
            errors.push(format!("{}: {error:#}", backup.target.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("Update rollback was incomplete: {}", errors.join("; "))
    }
}

fn extract_core_bundle(bytes: &[u8], destination: &Path) -> Result<PathBuf> {
    fs::create_dir(destination)
        .with_context(|| format!("Failed to create {}", destination.display()))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("Invalid core archive")?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .context("Core archive contains an unsafe path")?;
        let target = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut file)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
    }
    let executable_name = if cfg!(windows) {
        "renium.exe"
    } else {
        "renium"
    };
    let mut executables = Vec::new();
    for entry in walkdir::WalkDir::new(destination).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() == executable_name {
            executables.push(entry.into_path());
        }
    }
    executables.sort();
    if executables.len() != 1 {
        bail!(
            "Core archive must contain exactly one {executable_name}; found {}",
            executables.len()
        );
    }
    let root = executables[0]
        .parent()
        .context("Core executable has no parent")?
        .to_path_buf();
    #[cfg(unix)]
    ensure_core_permissions(&root)?;
    Ok(root)
}

#[cfg(unix)]
fn ensure_core_permissions(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for name in ["renium", "rbx", "install.sh"] {
        let path = root.join(name);
        if path.is_file() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

fn managed_core_root(target: &Path) -> Option<PathBuf> {
    let root = target.parent()?.to_path_buf();
    let expected = installer_core_root()?;
    paths_equal(&root, &expected).then_some(root)
}

fn recover_running_core_install() -> Result<()> {
    let current = env::current_exe().context("Failed to locate the running Renium CLI")?;
    if let Some(target_root) = managed_core_root(&current) {
        recover_core_install(&target_root)?;
    }
    #[cfg(target_os = "macos")]
    crate::studio::native::serializer::recover_managed_studio_install()?;
    Ok(())
}

fn recover_core_install(target_root: &Path) -> Result<()> {
    let parent = target_root
        .parent()
        .context("Core installation directory has no parent")?;
    if !parent.is_dir() {
        return Ok(());
    }
    let executable = if cfg!(windows) {
        "renium.exe"
    } else {
        "renium"
    };
    let mut reserved = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let reserved_name = entry.file_name().to_str().is_some_and(|name| {
            name.starts_with(".renium-previous-")
                || name.starts_with(".renium-core-previous-")
                || name.starts_with(".renium-install-")
                || name.starts_with(".renium-core-next-")
        });
        if reserved_name {
            reserved.push(entry.path());
        }
    }
    reserved.sort();
    let backups = reserved
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name.starts_with(".renium-previous-")
                        || name.starts_with(".renium-core-previous-")
                })
                && path.join(executable).is_file()
        })
        .cloned()
        .collect::<Vec<_>>();
    let stages = reserved
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name.starts_with(".renium-install-") || name.starts_with(".renium-core-next-")
                })
                && path.join(executable).is_file()
        })
        .cloned()
        .collect::<Vec<_>>();
    if !target_root.is_dir() {
        let recovery = if backups.len() > 1 {
            bail!(
                "Multiple interrupted Renium core backups need manual cleanup in {}",
                parent.display()
            );
        } else if let Some(backup) = backups.first() {
            Some(backup)
        } else {
            if stages.len() > 1 {
                bail!(
                    "Multiple interrupted Renium core stages need manual cleanup in {}",
                    parent.display()
                );
            }
            stages.first()
        };
        if let Some(recovery) = recovery {
            fs::rename(recovery, target_root).with_context(|| {
                format!(
                    "Failed to restore Renium core from {} to {}",
                    recovery.display(),
                    target_root.display()
                )
            })?;
        }
    }
    if target_root.is_dir() {
        for path in reserved {
            if path.exists() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn installer_core_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("Renium/bin"))
    }
    #[cfg(not(windows))]
    {
        if let Some(root) = env::var_os("XDG_DATA_HOME") {
            return Some(PathBuf::from(root).join("renium"));
        }
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|root| root.join(".local/share/renium"))
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .replace('\\', "/")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[cfg(windows)]
fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path = path.to_string_lossy().replace('\\', "/");
    let root = root.to_string_lossy().replace('\\', "/");
    path.eq_ignore_ascii_case(&root)
        || path
            .get(root.len()..)
            .is_some_and(|suffix| suffix.starts_with('/'))
            && path[..root.len()].eq_ignore_ascii_case(&root)
}

#[cfg(windows)]
fn plan_mutates_running_executable(plan: &DeferredUpdatePlan) -> Result<bool> {
    let current = env::current_exe().context("Failed to locate the running Renium CLI")?;
    if plan.components.contains(&UpdateComponent::Cli) && paths_equal(&current, &plan.target) {
        return Ok(true);
    }
    Ok(plan_editor_installs(plan)
        .iter()
        .any(|editor| path_is_within(&current, &editor.root)))
}

fn prepare_core_directory(source: &Path, target_root: &Path) -> Result<PathBuf> {
    let parent = target_root
        .parent()
        .context("Core installation directory has no parent")?;
    for attempt in 0..1_000_u32 {
        let candidate = parent.join(format!(
            ".renium-core-next-{}-{}-{attempt}",
            std::process::id(),
            current_millis()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                let result = copy_directory(source, &candidate);
                #[cfg(unix)]
                let result = result.and_then(|()| ensure_core_permissions(&candidate));
                if let Err(error) = result {
                    let _ = fs::remove_dir_all(&candidate);
                    return Err(error);
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    bail!("Could not allocate a fresh core installation stage")
}

fn replace_core_directory(target_root: &Path, prepared: &Path) -> Result<()> {
    let parent = target_root
        .parent()
        .context("Core installation directory has no parent")?;
    let backup = parent.join(format!(
        ".renium-core-previous-{}-{}",
        std::process::id(),
        current_millis()
    ));
    fs::rename(target_root, &backup)
        .with_context(|| format!("Failed to stage {}", target_root.display()))?;
    if let Err(error) = fs::rename(prepared, target_root) {
        let restore = fs::rename(&backup, target_root);
        return Err(error)
            .with_context(|| format!("Failed to install {}", target_root.display()))
            .context(restore.err().map_or_else(
                || "The previous core was restored".to_string(),
                |error| format!("Core rollback failed: {error}"),
            ));
    }
    if let Err(error) = fs::remove_dir_all(&backup) {
        eprintln!(
            "[renium] warning: failed to remove core backup {}: {error}",
            backup.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn install_windows_rbx_aliases(target_root: &Path) -> Result<()> {
    let cli = target_root.join("renium.exe");
    let home = env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .context("USERPROFILE is unavailable")?;
    let stable_root = home.join(".renium").join("bin");
    fs::create_dir_all(&stable_root)?;
    for alias in [target_root.join("rbx.exe"), stable_root.join("rbx.exe")] {
        if alias.is_file() {
            fs::remove_file(&alias)?;
        }
        if fs::hard_link(&cli, &alias).is_err() {
            fs::copy(&cli, &alias)?;
        }
    }
    Ok(())
}

fn restore_installed_files(paths: &[PathBuf], originals: &[Option<Vec<u8>>]) -> Result<()> {
    let mut errors = Vec::new();
    for (path, original) in paths.iter().zip(originals).rev() {
        let result = if let Some(bytes) = original {
            install_bytes(path, bytes)
        } else if path.is_file() {
            fs::remove_file(path).map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("Update rollback was incomplete: {}", errors.join("; "))
    }
}

fn is_renium_launcher(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| {
        let text = text.to_ascii_lowercase();
        text.contains("renium_cli") && text.contains("renium")
    })
}

fn install_shared_core_files(target: &Path, core_root: &Path) -> Result<()> {
    let root = target
        .parent()
        .context("The running Renium CLI has no installation directory")?;
    let executable = if cfg!(windows) {
        "renium.exe"
    } else {
        "renium"
    };
    let mut installs = vec![(core_root.join(executable), target.to_path_buf())];
    for name in SHARED_CORE_LAUNCHERS {
        let source = core_root.join(name);
        let destination = root.join(name);
        if source.is_file() && destination.is_file() && is_renium_launcher(&destination) {
            installs.push((source, destination));
        }
    }
    let agent_instructions = core_root.join(AGENT_INSTRUCTIONS_FILE);
    if agent_instructions.is_file() {
        installs.push((agent_instructions, root.join(AGENT_INSTRUCTIONS_FILE)));
    }
    let originals = installs
        .iter()
        .map(|(_, destination)| read_file_if_present(destination))
        .collect::<std::io::Result<Vec<_>>>()?;
    for (index, (source, destination)) in installs.iter().enumerate() {
        if let Err(error) = fs::read(source)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| install_bytes(destination, &bytes))
        {
            let rollback_paths = installs[..=index]
                .iter()
                .map(|(_, destination)| destination.clone())
                .collect::<Vec<_>>();
            let rollback = restore_installed_files(&rollback_paths, &originals[..=index]);
            return Err(error).context(rollback.err().map_or_else(
                || "The previous shared core files were restored".to_string(),
                |error| format!("Shared core rollback failed: {error:#}"),
            ));
        }
    }
    let agent_guides = core_root.join(AGENT_GUIDES_DIRECTORY);
    if agent_guides.is_dir() {
        let destination = root.join(AGENT_GUIDES_DIRECTORY);
        if destination.is_dir() {
            fs::remove_dir_all(&destination)?;
        } else if destination.exists() {
            fs::remove_file(&destination)?;
        }
        copy_directory(&agent_guides, &destination)?;
    }
    Ok(())
}

fn apply_staged_update_plan(
    plan: &DeferredUpdatePlan,
    _lifecycle_lock: &LifecycleLock,
) -> Result<()> {
    if let Some(plugin) = plan.plugin.as_ref() {
        let bytes = fs::read(&plugin.source)
            .with_context(|| format!("Failed to read {}", plugin.source.display()))?;
        crate::app::setup::validate_rbxm_version(&bytes, &plan.version)?;
        install_bytes(&plugin.target, &bytes)?;
        #[cfg(target_os = "macos")]
        if let Some(core_root) = plan
            .managed_studio_core_stage
            .as_deref()
            .or(plan.core_stage.as_deref())
        {
            let target_dir = plugin
                .target
                .parent()
                .context("Studio plugin target has no parent")?;
            let mut command = Command::new(core_root.join("renium"));
            command
                .arg("setup")
                .arg("--repair")
                .arg("--file")
                .arg(&plugin.source)
                .arg("--dir")
                .arg(target_dir);
            _lifecycle_lock.apply_to_command(&mut command);
            let status = command
                .status()
                .context("Failed to start the updated Renium managed Studio setup")?;
            if !status.success() {
                bail!("Updated Renium managed Studio setup exited with {status}");
            }
        }
    }
    for install in &plan.extension_installs {
        for editor in &install.editors {
            let status = Command::new(&editor.cli)
                .arg("--extensions-dir")
                .arg(&editor.root)
                .arg("--install-extension")
                .arg(&install.source)
                .arg("--force")
                .status()
                .with_context(|| format!("Failed to start {}", editor.cli.display()))?;
            if !status.success() {
                bail!(
                    "Extension installer {} exited with {status}",
                    editor.cli.display()
                );
            }
        }
    }
    if plan.components.contains(&UpdateComponent::Cli)
        && let Some(core_root) = plan.core_stage.as_deref()
    {
        #[cfg(windows)]
        if let Some(target_root) = managed_core_root(&plan.target) {
            let prepared = prepare_core_directory(core_root, &target_root)?;
            replace_core_directory(&target_root, &prepared)?;
            install_windows_rbx_aliases(&target_root)?;
        } else {
            install_shared_core_files(&plan.target, core_root)?;
        }
        #[cfg(not(windows))]
        install_cli_update(&plan.target, core_root)?;
    }
    Ok(())
}

fn pending_update_transaction_path() -> Result<PathBuf> {
    Ok(lifecycle_state_dir()?.join("update-transaction.json"))
}

fn write_pending_update_transaction(plan: &DeferredUpdatePlan) -> Result<()> {
    let path = pending_update_transaction_path()?;
    let mut bytes = serde_json::to_vec_pretty(plan)?;
    bytes.push(b'\n');
    install_bytes(&path, &bytes)
}

fn clear_pending_update_transaction() -> Result<()> {
    let path = pending_update_transaction_path()?;
    recover_file_install(&path)?;
    if path.is_file() {
        fs::remove_file(&path).with_context(|| format!("Failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn recover_pending_update_transaction(lifecycle_lock: &LifecycleLock) -> Result<Option<String>> {
    let path = pending_update_transaction_path()?;
    recover_file_install(&path)?;
    if !path.is_file() {
        return Ok(None);
    }
    let mut plan: DeferredUpdatePlan = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid update transaction journal {}", path.display()))?;
    let originals = plan
        .originals
        .as_ref()
        .context("The interrupted update has no durable original-state baseline")?;
    verify_update_originals(originals)?;
    if plan.phase == "applied" {
        clear_pending_update_transaction()?;
        if let Err(error) = fs::remove_dir_all(&plan.stage)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "[renium] warning: failed to remove completed update stage {}: {error}",
                plan.stage.display()
            );
        }
        return Ok(None);
    }
    crate::project::workflows::stop_all_daemons_for_update()?;
    #[cfg(windows)]
    if plan_mutates_running_executable(&plan)? {
        schedule_windows_update(&plan, plan.core_stage.as_deref())?;
        return Ok(Some(plan.version));
    }
    plan.phase = "applying".to_string();
    write_pending_update_transaction(&plan)?;
    if let Err(error) = apply_staged_update_plan(&plan, lifecycle_lock) {
        if let Err(rollback_error) = restore_update_originals(
            plan.originals
                .as_ref()
                .context("The interrupted update has no original-state baseline")?,
        ) {
            return Err(error).context(format!(
                "Interrupted update rollback was incomplete: {rollback_error:#}"
            ));
        }
        clear_pending_update_transaction()?;
        return Err(error).context(format!(
            "Could not finish the interrupted Renium {} update; the original installation was restored",
            plan.version
        ));
    }
    plan.phase = "applied".to_string();
    write_pending_update_transaction(&plan)?;
    clear_pending_update_transaction()?;
    if let Err(error) = fs::remove_dir_all(&plan.stage)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "[renium] warning: failed to remove recovered update stage {}: {error}",
            plan.stage.display()
        );
    }
    Ok(None)
}

#[cfg(windows)]
fn schedule_windows_update(plan: &DeferredUpdatePlan, core_root: Option<&Path>) -> Result<()> {
    use std::os::windows::process::CommandExt;

    let plan_path = plan.stage.join("update-plan.json");
    fs::write(&plan_path, serde_json::to_vec(plan)?)
        .with_context(|| format!("Failed to write {}", plan_path.display()))?;
    let helper_source = core_root.map_or(env::current_exe()?, |root| root.join("renium.exe"));
    let helper = env::temp_dir().join(format!(
        "renium-update-helper-{}-{}.exe",
        std::process::id(),
        current_millis()
    ));
    fs::copy(&helper_source, &helper)
        .with_context(|| format!("Failed to create update helper {}", helper.display()))?;
    let result = deferred_update_result_path()?;
    let result_parent = result
        .parent()
        .context("Deferred update result has no parent directory")?;
    fs::create_dir_all(result_parent)?;
    let result_probe = result_parent.join(format!(
        ".update-result-probe-{}-{}",
        std::process::id(),
        current_millis()
    ));
    fs::write(&result_probe, b"probe")?;
    fs::remove_file(&result_probe)?;
    if result.is_file() {
        fs::remove_file(&result)?;
    }
    let fallback_result = helper.with_extension("result.json");
    let mut command = Command::new(&helper);
    command
        .arg("update-helper")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--plan")
        .arg(&plan_path)
        .arg("--result")
        .arg(&result)
        .arg("--fallback-result")
        .arg(&fallback_result)
        .arg("--transaction-id")
        .arg(&plan.transaction_id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x08000000);
    write_pending_update_transaction(plan)?;
    let parent_start_identity = process_start_identity(std::process::id())
        .context("Could not read the update parent process start identity")?;
    let mut reservation = UpdateHelperReservation {
        transaction_id: plan.transaction_id.clone(),
        helper: helper.clone(),
        parent_pid: std::process::id(),
        parent_start_identity,
        helper_pid: None,
        helper_start_identity: None,
        phase: "reserved".to_string(),
    };
    write_update_helper_reservation(&reservation)?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = clear_update_helper_reservation(&plan.transaction_id);
            let _ = fs::remove_file(&helper);
            return Err(error).context("Failed to schedule the Renium CLI replacement");
        }
    };
    let helper_pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(1);
    let helper_start_identity = loop {
        if let Some(identity) = process_start_identity(helper_pid) {
            break identity;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = clear_update_helper_reservation(&plan.transaction_id);
            let _ = fs::remove_file(&helper);
            bail!("Could not read the update helper process start identity");
        }
        thread::sleep(Duration::from_millis(10));
    };
    reservation.helper_pid = Some(helper_pid);
    reservation.helper_start_identity = Some(helper_start_identity);
    reservation.phase = "spawned".to_string();
    if let Err(error) = write_update_helper_reservation(&reservation) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = clear_update_helper_reservation(&plan.transaction_id);
        let _ = fs::remove_file(&helper);
        return Err(error).context("Failed to publish update helper ownership");
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_cli_update(target: &Path, core_root: &Path) -> Result<()> {
    if let Some(target_root) = managed_core_root(target) {
        let prepared = prepare_core_directory(core_root, &target_root)?;
        replace_core_directory(&target_root, &prepared)?;
    } else {
        install_shared_core_files(target, core_root)?;
    }
    Ok(())
}

pub fn run_update_helper(args: UpdateHelperArgs) -> Result<()> {
    let helper = env::current_exe().context("Failed to locate the update helper")?;
    let mut plan: DeferredUpdatePlan = serde_json::from_slice(
        &fs::read(&args.plan).with_context(|| format!("Failed to read {}", args.plan.display()))?,
    )
    .context("Invalid deferred update plan")?;
    if plan.transaction_id != args.transaction_id {
        bail!("The deferred update plan does not match this helper reservation");
    }
    let helper_start_identity = process_start_identity(std::process::id())
        .context("Could not read the update helper process start identity")?;
    let reservation_deadline = Instant::now() + Duration::from_secs(1);
    let mut reservation = loop {
        let reservation = read_update_helper_reservation()?
            .context("The update helper reservation is missing")?;
        if reservation.transaction_id != args.transaction_id
            || reservation.parent_pid != args.parent_pid
            || !paths_equal(&reservation.helper, &helper)
        {
            bail!("The update helper reservation belongs to a different transaction");
        }
        if reservation.helper_pid == Some(std::process::id())
            && reservation.helper_start_identity.as_deref() == Some(helper_start_identity.as_str())
            && reservation.phase == "spawned"
        {
            break reservation;
        }
        if Instant::now() >= reservation_deadline {
            bail!("The update helper reservation was not published completely");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let parent_deadline = Instant::now() + Duration::from_secs(30);
    while reservation_process_is_alive(args.parent_pid, &reservation.parent_start_identity) {
        if Instant::now() >= parent_deadline {
            bail!("The previous Renium process did not exit within 30 seconds");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let lifecycle_lock = acquire_lifecycle_lock()?;
    reservation.phase = "claimed".to_string();
    write_update_helper_reservation(&reservation)?;
    let outcome = (|| -> Result<()> {
        crate::project::workflows::stop_all_daemons_for_update()?;
        if let Some(target_root) = managed_core_root(&plan.target) {
            recover_core_install(&target_root)?;
        }
        #[cfg(target_os = "macos")]
        crate::studio::native::serializer::recover_managed_studio_install()?;
        verify_update_originals(
            plan.originals
                .as_ref()
                .context("The deferred update has no original-state baseline")?,
        )?;
        plan.phase = "applying".to_string();
        write_pending_update_transaction(&plan)?;
        let applied = apply_staged_update_plan(&plan, &lifecycle_lock);
        if let Err(error) = applied {
            if let Err(rollback_error) = restore_update_originals(
                plan.originals
                    .as_ref()
                    .context("The deferred update has no original-state baseline")?,
            ) {
                return Err(error).context(format!(
                    "Deferred update rollback was incomplete: {rollback_error:#}"
                ));
            }
            return Err(error);
        }
        plan.phase = "applied".to_string();
        write_pending_update_transaction(&plan)?;
        clear_pending_update_transaction()?;
        Ok(())
    })();
    let errors = outcome
        .as_ref()
        .err()
        .map(|error| vec![format!("{error:#}")])
        .unwrap_or_default();
    if outcome.is_ok()
        && let Err(error) = fs::remove_dir_all(&plan.stage)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "[renium] warning: failed to remove update stage {}: {error}",
            plan.stage.display()
        );
    }
    let record = DeferredUpdateResult {
        ok: errors.is_empty(),
        version: plan.version,
        target: plan.target,
        error: (!errors.is_empty()).then(|| errors.join("; ")),
        helper,
    };
    let result_written = match write_deferred_update_result(&args.result, &record) {
        Ok(()) => true,
        Err(error) => match write_deferred_update_result(&args.fallback_result, &record) {
            Ok(()) => true,
            Err(fallback_error) => {
                eprintln!(
                    "[renium] warning: update completed but neither result file could be written: {error:#}; fallback: {fallback_error:#}"
                );
                false
            }
        },
    };
    if result_written && let Err(error) = clear_update_helper_reservation(&plan.transaction_id) {
        eprintln!(
            "[renium] warning: failed to clear the completed update helper reservation: {error:#}"
        );
    }
    drop(lifecycle_lock);
    if record.ok {
        Ok(())
    } else {
        bail!(
            "{}",
            record.error.as_deref().unwrap_or("Deferred update failed")
        )
    }
}

fn fetch_artifact(artifact: &UpdateArtifact, stage: &Path, file_name: &str) -> Result<Vec<u8>> {
    let bytes = if artifact.url.starts_with("https://") {
        download(&artifact.url, file_name)?
    } else {
        bail!("Update artifacts must use HTTPS");
    };
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(&artifact.sha256) {
        bail!(
            "{} has SHA-256 {}, expected {}",
            artifact.url,
            actual,
            artifact.sha256
        );
    }
    fs::write(stage.join(file_name), &bytes)?;
    Ok(bytes)
}

fn download(url: &str, file_name: &str) -> Result<Vec<u8>> {
    let path = env::temp_dir().join(format!("renium-{}-{}", std::process::id(), file_name));
    let result = crate::system::tools::download_to_file(url, &path).and_then(|_| {
        fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))
    });
    let _ = fs::remove_file(path);
    result
}

pub(crate) fn install_bytes(target: &Path, bytes: &[u8]) -> Result<()> {
    recover_file_install(target)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("renium");
    let temporary = target.with_file_name(format!(".{name}.{}.new", std::process::id()));
    let backup = target.with_file_name(format!(".{name}.previous"));
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    preserve_executable_permissions(target, &temporary)?;
    let had_target = target.exists();
    if had_target {
        if backup.exists() {
            fs::remove_file(&backup)?;
        }
        fs::rename(target, &backup)
            .with_context(|| format!("Failed to stage {}", target.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, target) {
        let rollback = had_target.then(|| {
            fs::rename(&backup, target).with_context(|| {
                format!(
                    "Failed to restore {} from {}",
                    target.display(),
                    backup.display()
                )
            })
        });
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("Failed to install {}", target.display()))
            .context(rollback.and_then(Result::err).map_or_else(
                || "The previous file was restored".to_string(),
                |error| format!("Rollback failed: {error:#}"),
            ));
    }
    if backup.exists()
        && let Err(error) = fs::remove_file(&backup)
    {
        eprintln!(
            "[renium] warning: failed to remove update backup {}: {error}",
            backup.display()
        );
    }
    Ok(())
}

fn recover_file_install(target: &Path) -> Result<()> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("renium");
    let backup = target.with_file_name(format!(".{name}.previous"));
    if target.exists() {
        if backup.is_file() {
            fs::remove_file(&backup)
                .with_context(|| format!("Failed to remove {}", backup.display()))?;
        }
    } else if backup.is_file() {
        fs::rename(&backup, target).with_context(|| {
            format!(
                "Failed to restore {} from {}",
                target.display(),
                backup.display()
            )
        })?;
    }
    if parent.is_dir() {
        let prefix = format!(".{name}.");
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.file_name().to_str().is_some_and(|entry_name| {
                    entry_name.starts_with(&prefix) && entry_name.ends_with(".new")
                })
            {
                fs::remove_file(entry.path())
                    .with_context(|| format!("Failed to remove {}", entry.path().display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn preserve_executable_permissions(target: &Path, temporary: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(target)
        .map(|metadata| metadata.permissions().mode())
        .unwrap_or(0o755);
    fs::set_permissions(temporary, fs::Permissions::from_mode(mode))
        .with_context(|| format!("Failed to set permissions on {}", temporary.display()))
}

fn platform_key() -> String {
    format!("{}-{}", env::consts::OS, env::consts::ARCH)
}

fn cli_is_extension_owned(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized.split("/extensions/").skip(1).any(|suffix| {
        suffix.split_once('/').is_some_and(|(extension, path)| {
            (extension == "local.renium" || extension.starts_with("local.renium-"))
                && path.starts_with("bin/")
        })
    })
}

fn user_data_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        return Ok(
            PathBuf::from(env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?)
                .join("Renium"),
        );
    }
    if cfg!(target_os = "macos") {
        return Ok(
            PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
                .join("Library/Application Support/Renium"),
        );
    }
    if let Some(base) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(base).join("renium"));
    }
    Ok(PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".local/share/renium"))
}

fn physical_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
        }
    }
    let mut existing = normalized.as_path();
    let mut suffix = Vec::<OsString>::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .with_context(|| format!("Could not resolve {}", path.display()))?;
        suffix.push(name.to_os_string());
        existing = existing
            .parent()
            .with_context(|| format!("Could not resolve {}", path.display()))?;
    }
    let mut resolved = fs::canonicalize(existing)
        .with_context(|| format!("Could not resolve {}", existing.display()))?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn paths_overlap_physically(left: &Path, right: &Path) -> Result<bool> {
    let left = physical_path(left)?;
    let right = physical_path(right)?;
    Ok(paths_equal(&left, &right) || left.starts_with(&right) || right.starts_with(&left))
}

fn lifecycle_state_dir() -> Result<PathBuf> {
    if cfg!(windows) || cfg!(target_os = "macos") {
        return user_data_dir();
    }
    let state = if let Some(base) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(base).join("renium")
    } else {
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".local/state/renium")
    };
    let data = user_data_dir()?;
    if paths_overlap_physically(&state, &data)? {
        let name = data.file_name().and_then(OsStr::to_str).unwrap_or("renium");
        return Ok(data.with_file_name(format!("{name}.lifecycle")));
    }
    Ok(state)
}

fn lifecycle_lock_dir() -> Result<PathBuf> {
    if cfg!(windows) || cfg!(target_os = "macos") {
        return user_data_dir();
    }
    let lock = if let Some(base) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(base).join("renium")
    } else {
        PathBuf::from(env::var_os("HOME").context("HOME is not set")?).join(".config/renium")
    };
    let data = user_data_dir()?;
    if paths_overlap_physically(&lock, &data)? {
        return lifecycle_state_dir();
    }
    Ok(lock)
}

fn deferred_update_result_path() -> Result<PathBuf> {
    Ok(lifecycle_state_dir()?.join("update-result.json"))
}

fn write_deferred_update_result(path: &Path, result: &DeferredUpdateResult) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(result)?;
    bytes.push(b'\n');
    install_bytes(path, &bytes)
}

pub(crate) fn report_pending_update_result() {
    let Ok(primary) = deferred_update_result_path() else {
        return;
    };
    let Ok(current) = env::current_exe() else {
        return;
    };
    let mut candidates = vec![(primary, false)];
    let mut fallbacks = fs::read_dir(env::temp_dir())
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    name.starts_with("renium-update-helper-") && name.ends_with(".result.json")
                })
        })
        .collect::<Vec<_>>();
    fallbacks.sort_by_key(|path| {
        std::cmp::Reverse(
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });
    candidates.extend(fallbacks.into_iter().map(|path| (path, true)));
    let mut selected = None;
    for (path, fallback) in candidates {
        if !path.is_file() {
            continue;
        }
        let result = fs::read(&path)
            .context("Failed to read deferred update result")
            .and_then(|bytes| {
                serde_json::from_slice::<DeferredUpdateResult>(&bytes)
                    .context("Invalid deferred update result")
            });
        if result.as_ref().is_ok_and(|result| {
            fs::canonicalize(&result.target)
                .ok()
                .zip(fs::canonicalize(&current).ok())
                .is_some_and(|(target, current)| paths_equal(&target, &current))
        }) {
            selected = Some((path, result));
            break;
        }
        if fallback
            && fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(7 * 24 * 60 * 60))
        {
            let _ = fs::remove_file(path);
        }
    }
    let Some((path, result)) = selected else {
        return;
    };
    let _ = fs::remove_file(&path);
    match result {
        Ok(result) => {
            if result.helper.is_file()
                && let Err(error) = fs::remove_file(&result.helper)
            {
                eprintln!(
                    "[renium] warning: failed to remove update helper {}: {error}",
                    result.helper.display()
                );
            }
            if result.ok {
                eprintln!(
                    "[renium] Renium {} finished updating {}",
                    result.version,
                    result.target.display()
                );
            } else {
                eprintln!(
                    "[renium] Renium {} update failed for {}: {}",
                    result.version,
                    result.target.display(),
                    result.error.as_deref().unwrap_or("unknown error")
                );
            }
        }
        Err(error) => {
            eprintln!("[renium] warning: {error:#}");
        }
    }
}

fn editor_kind_from_extension_root(root: &Path) -> Option<&'static str> {
    let normalized = root
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if normalized.ends_with("/.cursor/extensions") {
        Some("cursor")
    } else if normalized.ends_with("/.vscode/extensions") {
        Some("code")
    } else if normalized.ends_with("/.vscode-insiders/extensions") {
        Some("code-insiders")
    } else if normalized.ends_with("/.windsurf/extensions") {
        Some("windsurf")
    } else {
        None
    }
}

fn editor_kind_from_cli(path: &Path) -> Option<&'static str> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)?
        .trim_end_matches(".cmd")
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    match name.as_str() {
        "cursor" => Some("cursor"),
        "code" => Some("code"),
        "code-insiders" => Some("code-insiders"),
        "windsurf" => Some("windsurf"),
        _ => None,
    }
}

fn find_editor_clis() -> Vec<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &[
            "cursor.cmd",
            "code.cmd",
            "code-insiders.cmd",
            "windsurf.cmd",
            "cursor.exe",
            "code.exe",
            "code-insiders.exe",
            "windsurf.exe",
        ]
    } else {
        &["cursor", "code", "code-insiders", "windsurf"]
    };
    let Some(path) = env::var_os("PATH") else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for directory in env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                let key = fs::canonicalize(&candidate)
                    .unwrap_or_else(|_| candidate.clone())
                    .to_string_lossy()
                    .replace('\\', "/")
                    .to_ascii_lowercase();
                if seen.insert(key) {
                    result.push(candidate);
                }
            }
        }
    }
    result
}

fn find_installed_extension_editors() -> Result<Vec<EditorExtensionInstall>> {
    let installed = extension_roots()
        .into_iter()
        .filter(|root| has_renium_extension(root))
        .collect::<Vec<_>>();
    if installed.is_empty() {
        bail!("No installed Renium editor extension was found");
    }
    let available = find_editor_clis()
        .into_iter()
        .filter_map(|path| editor_kind_from_cli(&path).map(|kind| (kind, path)))
        .collect::<BTreeMap<_, _>>();
    let explicit_cli = env::var_os("RENIUM_EDITOR_CLI")
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let mut result = Vec::new();
    for root in installed {
        let kind = editor_kind_from_extension_root(&root).with_context(|| {
            format!("Could not identify the editor that owns {}", root.display())
        })?;
        let cli = available
            .get(kind)
            .cloned()
            .or_else(|| {
                explicit_cli
                    .as_ref()
                    .filter(|cli| editor_kind_from_cli(cli) == Some(kind))
                    .cloned()
            })
            .with_context(|| {
                format!(
                    "No editor CLI can update the Renium extension in {}; set RENIUM_EDITOR_CLI to that editor's command",
                    root.display()
                )
            })?;
        result.push(EditorExtensionInstall { cli, root });
    }
    Ok(result)
}

fn selected_extension_editor(root: &Path, cli: &Path) -> Result<EditorExtensionInstall> {
    if !has_renium_extension(root) {
        bail!(
            "No installed Renium editor extension was found in {}",
            root.display()
        );
    }
    if !cli.is_file() {
        bail!("Editor CLI does not exist: {}", cli.display());
    }
    let root_kind = editor_kind_from_extension_root(root)
        .with_context(|| format!("Could not identify the editor that owns {}", root.display()))?;
    let cli_kind = editor_kind_from_cli(cli)
        .with_context(|| format!("Could not identify editor CLI {}", cli.display()))?;
    if root_kind != cli_kind {
        bail!(
            "Editor CLI {} does not manage {}",
            cli.display(),
            root.display()
        );
    }
    Ok(EditorExtensionInstall {
        cli: cli.to_path_buf(),
        root: root.to_path_buf(),
    })
}

fn editor_platform(editor: &EditorExtensionInstall) -> Result<String> {
    let output = Command::new(&editor.cli)
        .arg("--version")
        .output()
        .with_context(|| format!("Failed to inspect {}", editor.cli.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            editor.cli.display(),
            output.status
        );
    }
    let mut architecture = None;
    for text in [
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ] {
        for token in
            text.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        {
            let found = if ["x64", "x86_64", "amd64"]
                .iter()
                .any(|value| token.eq_ignore_ascii_case(value))
            {
                Some("x86_64")
            } else if ["arm64", "aarch64"]
                .iter()
                .any(|value| token.eq_ignore_ascii_case(value))
            {
                Some("aarch64")
            } else {
                None
            };
            if let Some(found) = found {
                if architecture.is_some_and(|current| current != found) {
                    bail!(
                        "{} reported multiple supported architectures from --version",
                        editor.cli.display()
                    );
                }
                architecture = Some(found);
            }
        }
    }
    let architecture = architecture.with_context(|| {
        format!(
            "{} did not report one supported architecture from --version",
            editor.cli.display()
        )
    })?;
    Ok(format!("{}-{architecture}", env::consts::OS))
}

fn group_editor_installs_by_platform(
    editors: &[EditorExtensionInstall],
) -> Result<BTreeMap<String, Vec<EditorExtensionInstall>>> {
    let mut groups = BTreeMap::<String, Vec<EditorExtensionInstall>>::new();
    let mut platforms = HashMap::<PathBuf, String>::new();
    for editor in editors {
        let platform = if let Some(platform) = platforms.get(&editor.cli) {
            platform.clone()
        } else {
            let platform = editor_platform(editor)?;
            platforms.insert(editor.cli.clone(), platform.clone());
            platform
        };
        groups.entry(platform).or_default().push(editor.clone());
    }
    Ok(groups)
}
