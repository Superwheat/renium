//! Open Cloud API keys stored once per user, outside every project and
//! shell profile. At rest they are DPAPI-protected on Windows, kept in the
//! login Keychain on macOS, and in a 0600 file on Linux.
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(target_os = "macos")]
const SERVICE: &str = "renium.open-cloud";
static SELECTED: OnceLock<Option<String>> = OnceLock::new();

/// The `--key NAME` choice for this process; read wherever an API key is needed.
pub(crate) fn select(name: Option<String>) {
    let _ = SELECTED.set(name.filter(|value| !value.trim().is_empty()));
}

pub(crate) fn selected() -> Option<&'static str> {
    SELECTED.get().and_then(|value| value.as_deref())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Store {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<String>,
    #[serde(default)]
    keys: BTreeMap<String, StoredKey>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredKey {
    added_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protected: Option<String>,
}

fn storage_kind() -> &'static str {
    if cfg!(windows) {
        "dpapi"
    } else if cfg!(target_os = "macos") {
        "keychain"
    } else {
        "file"
    }
}

fn store_path() -> Result<PathBuf> {
    Ok(crate::app::update::user_data_dir()?
        .join("private")
        .join("api-keys.json"))
}

fn read_store(path: &Path) -> Result<Store> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a valid key store", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Store::default()),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn write_store(path: &Path, store: &Store) -> Result<()> {
    let parent = path.parent().context("Key store has no parent directory")?;
    create_private_dir(parent)?;
    let bytes = serde_json::to_vec_pretty(store)?;
    let temporary = parent.join(format!(".api-keys-{}.tmp", std::process::id()));
    write_private_file(&temporary, &bytes)?;
    fs::rename(&temporary, path).with_context(|| format!("Failed to write {}", path.display()))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.is_dir() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("Failed to create {}", path.display()))
}

fn validate_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("Key names use 1–64 letters, digits, '-', '_' or '.'");
    }
    Ok(name)
}

fn validate_secret(secret: &str) -> Result<&str> {
    let secret = secret.trim();
    if secret.is_empty() || secret.len() > 4096 || secret.chars().any(char::is_control) {
        bail!("The API key is empty or malformed");
    }
    Ok(secret)
}

#[cfg(windows)]
fn protect(secret: &str) -> Result<Option<String>> {
    let blob = crate::automation::authorization::crypt(secret.as_bytes(), true)?;
    Ok(Some(base64::encode_config(blob, base64::STANDARD)))
}

#[cfg(windows)]
fn unprotect(entry: &StoredKey, _name: &str) -> Result<String> {
    let encoded = entry
        .protected
        .as_deref()
        .context("Stored key has no protected value")?;
    let blob = base64::decode_config(encoded, base64::STANDARD)
        .context("Stored key is not valid base64")?;
    let bytes = crate::automation::authorization::crypt(&blob, false)
        .context("Stored key belongs to another Windows user or is damaged")?;
    String::from_utf8(bytes).context("Stored key is not UTF-8")
}

#[cfg(windows)]
fn forget(_name: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn security(input: &str, args: &[&str]) -> Result<std::process::Output> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("security")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start the macOS security tool")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }
    Ok(child.wait_with_output()?)
}

#[cfg(target_os = "macos")]
fn protect_named(name: &str, secret: &str) -> Result<Option<String>> {
    // The secret travels over stdin in interactive mode, never in argv.
    let command = format!(
        "add-generic-password -U -a {name} -s {SERVICE} -l \"Renium Open Cloud key {name}\" -w {secret}\n"
    );
    let output = security(&command, &["-i"])?;
    if !output.status.success() {
        bail!(
            "Keychain refused the key: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn unprotect(_entry: &StoredKey, name: &str) -> Result<String> {
    let output = security(
        "",
        &["find-generic-password", "-a", name, "-s", SERVICE, "-w"],
    )?;
    if !output.status.success() {
        bail!("Keychain has no Renium key named {name}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "macos")]
fn forget(name: &str) -> Result<()> {
    let _ = security("", &["delete-generic-password", "-a", name, "-s", SERVICE]);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn protect(secret: &str) -> Result<Option<String>> {
    Ok(Some(base64::encode_config(
        secret.as_bytes(),
        base64::STANDARD,
    )))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unprotect(entry: &StoredKey, _name: &str) -> Result<String> {
    let encoded = entry
        .protected
        .as_deref()
        .context("Stored key has no value")?;
    let bytes = base64::decode_config(encoded, base64::STANDARD)
        .context("Stored key is not valid base64")?;
    String::from_utf8(bytes).context("Stored key is not UTF-8")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn forget(_name: &str) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn protect_named(_name: &str, secret: &str) -> Result<Option<String>> {
    protect(secret)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

pub(crate) fn add_in(path: &Path, name: &str, secret: &str) -> Result<Value> {
    let name = validate_name(name)?;
    let secret = validate_secret(secret)?;
    let mut store = read_store(path)?;
    let replaced = store.keys.contains_key(name);
    let protected = protect_named(name, secret)?;
    store.keys.insert(
        name.to_string(),
        StoredKey {
            added_unix: now_unix(),
            protected,
        },
    );
    let became_default = store.default.is_none();
    if became_default {
        store.default = Some(name.to_string());
    }
    write_store(path, &store)?;
    Ok(json!({
        "ok": true,
        "action": "add",
        "name": name,
        "replaced": replaced,
        "default": store.default,
        "storage": storage_kind(),
        "path": path,
    }))
}

pub(crate) fn add(name: &str, secret: &str) -> Result<Value> {
    add_in(&store_path()?, name, secret)
}

pub(crate) fn remove_in(path: &Path, name: &str) -> Result<Value> {
    let name = validate_name(name)?;
    let mut store = read_store(path)?;
    if store.keys.remove(name).is_none() {
        bail!("No stored key named {name}");
    }
    forget(name)?;
    if store.default.as_deref() == Some(name) {
        store.default = store.keys.keys().next().cloned();
    }
    write_store(path, &store)?;
    Ok(json!({"ok": true, "action": "remove", "name": name, "default": store.default}))
}

pub(crate) fn remove(name: &str) -> Result<Value> {
    remove_in(&store_path()?, name)
}

pub(crate) fn set_default_in(path: &Path, name: &str) -> Result<Value> {
    let name = validate_name(name)?;
    let mut store = read_store(path)?;
    if !store.keys.contains_key(name) {
        bail!("No stored key named {name}");
    }
    store.default = Some(name.to_string());
    write_store(path, &store)?;
    Ok(json!({"ok": true, "action": "use", "default": name}))
}

pub(crate) fn set_default(name: &str) -> Result<Value> {
    set_default_in(&store_path()?, name)
}

pub(crate) fn list_in(path: &Path) -> Result<Value> {
    let store = read_store(path)?;
    Ok(json!({
        "default": store.default,
        "keys": store.keys.iter().map(|(name, entry)| json!({
            "name": name,
            "addedUnix": entry.added_unix,
        })).collect::<Vec<_>>(),
        "storage": storage_kind(),
        "path": path,
    }))
}

pub(crate) fn list() -> Result<Value> {
    list_in(&store_path()?)
}

/// The secret for a stored key, or for the default key when no name is given.
pub(crate) fn secret_in(path: &Path, name: Option<&str>) -> Result<Option<String>> {
    let store = read_store(path)?;
    let name = match name {
        Some(name) => validate_name(name)?.to_string(),
        None => match store.default.clone() {
            Some(name) => name,
            None => return Ok(None),
        },
    };
    let Some(entry) = store.keys.get(&name) else {
        bail!("No stored key named {name}; `rbx oc key list` shows the stored keys");
    };
    unprotect(entry, &name).map(Some)
}

pub(crate) fn secret(name: Option<&str>) -> Result<Option<String>> {
    secret_in(&store_path()?, name)
}

/// Reads one line from stdin without echoing it on a terminal, so the key
/// never appears in a command line, a shell history or a transcript.
pub(crate) fn read_secret_from_stdin() -> Result<String> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        eprint!("Paste the Open Cloud API key and press Enter (input hidden): ");
        io::stderr().flush()?;
    }
    let _echo = interactive.then(EchoOff::new);
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    if interactive {
        io::stderr().write_all(
            b"
",
        )?;
    }
    Ok(line.trim().to_string())
}

struct EchoOff {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    mode: u32,
}

impl EchoOff {
    #[cfg(windows)]
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        };
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return None;
        }
        if unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } == 0 {
            return None;
        }
        Some(Self { handle, mode })
    }

    #[cfg(not(windows))]
    fn new() -> Option<Self> {
        std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status()
            .ok()
            .filter(|status| status.success())
            .map(|_| Self {})
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(self.handle, self.mode);
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("stty")
                .arg("echo")
                .stdin(std::process::Stdio::inherit())
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_store() -> (PathBuf, PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("renium-keys-{}-{stamp}", std::process::id()));
        (dir.clone(), dir.join("api-keys.json"))
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn keys_round_trip_without_the_secret_in_the_listing() -> Result<()> {
        let (dir, path) = scratch_store();
        assert!(secret_in(&path, None)?.is_none());
        let added = add_in(&path, "studio", "rbx-secret-1")?;
        assert_eq!(added["default"], "studio");
        add_in(&path, "backup", "rbx-secret-2")?;
        assert_eq!(secret_in(&path, None)?.as_deref(), Some("rbx-secret-1"));
        assert_eq!(
            secret_in(&path, Some("backup"))?.as_deref(),
            Some("rbx-secret-2")
        );
        let listing = list_in(&path)?;
        assert!(!serde_json::to_string(&listing)?.contains("rbx-secret"));
        assert_eq!(listing["keys"].as_array().map(Vec::len), Some(2));
        let raw = fs::read_to_string(&path)?;
        if cfg!(windows) {
            assert!(!raw.contains("rbx-secret"));
        }
        set_default_in(&path, "backup")?;
        assert_eq!(secret_in(&path, None)?.as_deref(), Some("rbx-secret-2"));
        remove_in(&path, "backup")?;
        assert_eq!(secret_in(&path, None)?.as_deref(), Some("rbx-secret-1"));
        assert!(secret_in(&path, Some("backup")).is_err());
        assert!(add_in(&path, "bad name", "x").is_err());
        assert!(add_in(&path, "ok", " ").is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
