use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::app::timing::current_millis;
use crate::daemon::is_process_alive;

pub(crate) const SERVICE_SETTINGS_FILE_NAME: &str = "__roblox_sync_settings.renium";

pub(crate) struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    pub(crate) fn new(action: F) -> Self {
        Self(Some(action))
    }

    pub(crate) fn run(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.run();
    }
}

pub(crate) fn current_unix_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs() as i64)
}

pub(crate) fn service_settings_path(service_dir: &Path) -> PathBuf {
    service_dir.join(SERVICE_SETTINGS_FILE_NAME)
}

pub(crate) fn is_service_settings_file_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(SERVICE_SETTINGS_FILE_NAME)
}

pub(crate) fn path_extension_is(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|expected| extension.eq_ignore_ascii_case(expected))
        })
}

pub(crate) fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
    value
        .get(value.len().saturating_sub(suffix.len())..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

pub(crate) fn path_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character == '\\' && cfg!(windows) {
            out.push('/');
        } else if cfg!(windows) {
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

pub(crate) fn exact_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub(crate) fn create_unique_directory(parent: &Path, prefix: &str) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    let stamp = current_millis();
    for _ in 0..1_000 {
        let path = parent.join(format!(
            "{prefix}{}-{stamp}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to create {}", path.display()));
            }
        }
    }
    bail!("Could not allocate a fresh temporary directory")
}

pub(crate) fn case_folded_path_key(path: &Path) -> String {
    exact_path_key(path).to_ascii_lowercase()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    if matches!(reader.fill_buf(), Ok(buffer) if buffer.starts_with(&[0xEF, 0xBB, 0xBF])) {
        reader.consume(3);
    }
    serde_json::from_reader(reader).with_context(|| format!("Invalid JSON in {}", path.display()))
}

pub(crate) fn read_file_if_present(path: &Path) -> io::Result<Option<Vec<u8>>> {
    if path.is_file() {
        fs::read(path).map(Some)
    } else {
        Ok(None)
    }
}

pub(crate) fn create_output_writer(path: &Path) -> Result<BufWriter<File>> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    File::create(path)
        .map(BufWriter::new)
        .with_context(|| format!("Failed to write {}", path.display()))
}

pub(crate) fn write_json_file<T: Serialize>(path: &Path, value: &T, compact: bool) -> Result<()> {
    if compact {
        write_json_streaming(path, value)
    } else {
        let value =
            serde_json::to_value(value).context("Failed to convert JSON value for formatting")?;
        let mut serialized = String::new();
        write_pretty_json_value(&value, 0, &mut serialized)?;
        serialized.push('\n');
        write_utf8_file(path, &serialized)
    }
}

fn write_pretty_json_value(value: &Value, indent: usize, out: &mut String) -> Result<()> {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }

            out.push('{');
            out.push('\n');
            for (index, (key, child)) in map.iter().enumerate() {
                push_json_indent(out, indent + 1);
                out.push_str(&serde_json::to_string(key).context("Failed to serialize JSON key")?);
                out.push_str(": ");
                write_pretty_json_value(child, indent + 1, out)?;
                if index + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_json_indent(out, indent);
            out.push('}');
            Ok(())
        }
        Value::Array(array) => {
            if array.is_empty() {
                out.push_str("[]");
                return Ok(());
            }

            if is_inline_numeric_array(array) {
                out.push('[');
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(
                        &serde_json::to_string(item)
                            .context("Failed to serialize numeric array value")?,
                    );
                }
                out.push(']');
                return Ok(());
            }

            out.push('[');
            out.push('\n');
            for (index, item) in array.iter().enumerate() {
                push_json_indent(out, indent + 1);
                write_pretty_json_value(item, indent + 1, out)?;
                if index + 1 < array.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_json_indent(out, indent);
            out.push(']');
            Ok(())
        }
        _ => {
            out.push_str(&serde_json::to_string(value).context("Failed to serialize JSON value")?);
            Ok(())
        }
    }
}

fn is_inline_numeric_array(array: &[Value]) -> bool {
    !array.is_empty()
        && array.iter().all(|value| match value {
            Value::Number(_) => true,
            Value::Array(child) => is_inline_numeric_array(child),
            _ => false,
        })
}

fn push_json_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

pub(crate) fn write_utf8_file(path: &Path, content: &str) -> Result<()> {
    write_bytes_if_changed(path, content.as_bytes())
}

pub(crate) fn write_bytes_if_changed(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    write_bytes_if_changed_in_existing_dir(path, content)
}

pub(crate) fn write_bytes_if_changed_in_existing_dir(path: &Path, content: &[u8]) -> Result<()> {
    cleanup_stale_sibling_temps(path);
    let (contents_match, was_readonly) = file_contents_match(path, content)?;
    if contents_match {
        return Ok(());
    }

    let temp_path = sibling_temp_path(path);
    if let Err(error) = fs::write(&temp_path, content) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("Failed to write {}", temp_path.display()));
    }

    publish_sibling_temp_with_permissions(&temp_path, path, was_readonly)
}

fn cleanup_stale_sibling_temps(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return;
    };
    let prefix = format!("{file_name}.");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let current_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(owner) = name
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(".renium-tmp"))
        else {
            continue;
        };
        let Some(pid) = owner
            .split_once('-')
            .and_then(|(value, _)| value.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != current_pid && !is_process_alive(pid) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn publish_sibling_temp(temp_path: &Path, path: &Path) -> Result<()> {
    let was_readonly = fs::metadata(path).is_ok_and(|meta| meta.permissions().readonly());
    publish_sibling_temp_with_permissions(temp_path, path, was_readonly)
}

fn publish_sibling_temp_with_permissions(
    temp_path: &Path,
    path: &Path,
    was_readonly: bool,
) -> Result<()> {
    if was_readonly {
        set_path_readonly(path, false)?;
    }
    let renamed = fs::rename(temp_path, path);
    if was_readonly {
        let _ = set_path_readonly(path, true);
    }
    if let Err(error) = renamed {
        let _ = fs::remove_file(temp_path);
        return Err(error).with_context(|| format!("Failed to write {}", path.display()));
    }
    Ok(())
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(".{}-{sequence}.renium-tmp", std::process::id()));
    path.with_file_name(name)
}

fn file_contents_match(path: &Path, content: &[u8]) -> Result<(bool, bool)> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((false, false)),
        Err(err) => return Err(err).with_context(|| format!("Failed to stat {}", path.display())),
    };
    let was_readonly = metadata.permissions().readonly();

    if metadata.len() != content.len() as u64 {
        return Ok((false, was_readonly));
    }

    const COMPARE_BUF_SIZE: usize = 1024 * 1024;
    let file = File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut reader = BufReader::with_capacity(COMPARE_BUF_SIZE, file);
    let mut offset = 0usize;
    let mut buffer = vec![0u8; COMPARE_BUF_SIZE.min(content.len())];
    while offset < content.len() {
        let want = (content.len() - offset).min(buffer.len());
        reader
            .read_exact(&mut buffer[..want])
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if buffer[..want] != content[offset..offset + want] {
            return Ok((false, was_readonly));
        }
        offset += want;
    }

    Ok((true, was_readonly))
}

pub(crate) fn write_json_streaming<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    cleanup_stale_sibling_temps(path);
    let temp_path = sibling_temp_path(path);
    let result = (|| -> Result<()> {
        let file = File::create(&temp_path)
            .with_context(|| format!("Failed to write {}", temp_path.display()))?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        serde_json::to_writer(&mut writer, value).context("Failed to serialize JSON")?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("Failed to write {}", temp_path.display()))?;
        writer
            .flush()
            .with_context(|| format!("Failed to write {}", temp_path.display()))?;
        writer
            .get_ref()
            .sync_all()
            .with_context(|| format!("Failed to write {}", temp_path.display()))
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    let published = publish_sibling_temp(&temp_path, path);
    if published.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    published
}

pub(crate) fn atomic_write_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    cleanup_stale_sibling_temps(path);
    let temp_path = sibling_temp_path(path);
    let result = (|| -> Result<()> {
        let mut file = File::create(&temp_path)
            .with_context(|| format!("Failed to create {}", temp_path.display()))?;
        file.write_all(content)
            .with_context(|| format!("Failed to write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to write {}", temp_path.display()))
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    let published = replace_file_with_backup(&temp_path, path, "file");
    if published.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    published
}

pub(crate) fn replace_file_with_backup(
    source: &Path,
    destination: &Path,
    label: &str,
) -> Result<()> {
    if !destination.exists() {
        fs::rename(source, destination).with_context(|| {
            format!(
                "Failed to publish {label} {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }
    let file_name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(label);
    let backup = destination.with_file_name(format!(
        ".{file_name}.renium-replace-{}-{}",
        std::process::id(),
        current_millis()
    ));
    fs::rename(destination, &backup)
        .with_context(|| format!("Failed to preserve existing {}", destination.display()))?;
    if let Err(error) = fs::rename(source, destination) {
        let restore = fs::rename(&backup, destination);
        if let Err(restore_error) = restore {
            return Err(error).with_context(|| {
                format!(
                    "Failed to publish {label} and failed to restore {}: {restore_error}",
                    destination.display()
                )
            });
        }
        return Err(error).with_context(|| {
            format!(
                "Failed to publish {label} {} -> {}",
                source.display(),
                destination.display()
            )
        });
    }
    if let Err(error) = fs::remove_file(&backup) {
        eprintln!(
            "[renium] warning: failed to remove replacement backup {}: {error}",
            backup.display()
        );
    }
    Ok(())
}

pub(crate) fn sanitize_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let invalid = matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*');
        if invalid || ch.is_control() {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim_end_matches([' ', '.']);
    let capped: String = trimmed.chars().take(100).collect();
    let capped = capped.trim_end_matches([' ', '.']);
    let mut final_name = if capped.is_empty() {
        "_".to_string()
    } else {
        capped.to_string()
    };

    if is_windows_reserved_name(&final_name) {
        final_name.insert(0, '_');
    }
    final_name
}

pub(crate) fn sanitize_ascii_identifier(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

pub(crate) fn normalized_child_stem_key(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == '\\' {
            out.push('/');
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

pub(crate) fn unique_child_stem(
    raw_name: &str,
    used_stem_keys: &mut HashSet<String>,
    next_suffix_by_base: &mut HashMap<String, usize>,
) -> String {
    let base = sanitize_name(raw_name);
    let base_key = normalized_child_stem_key(&base);
    if used_stem_keys.insert(base_key.clone()) {
        next_suffix_by_base.entry(base_key).or_insert(2);
        return base;
    }

    let next_suffix = next_suffix_by_base.entry(base_key).or_insert(2);
    loop {
        let candidate = format!("{}_{}", base, *next_suffix);
        *next_suffix += 1;
        if used_stem_keys.insert(normalized_child_stem_key(&candidate)) {
            return candidate;
        }
    }
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn fnv1a_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a(bytes))
}

pub(crate) fn absolutize_under(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub(crate) fn absolutize_for_daemon(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

pub(crate) fn validate_filesystem_instance_name(raw: &str, label: &str) -> Result<()> {
    if raw.is_empty() {
        bail!("{label} cannot be empty");
    }
    if sanitize_name(raw) != raw {
        bail!(
            "{label} must be one filesystem-safe Roblox instance name, not a path or reserved name: {raw:?}"
        );
    }
    Ok(())
}

pub(crate) fn set_path_readonly(path: &Path, readonly: bool) -> Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    let mut perms = meta.permissions();
    if perms.readonly() == readonly {
        return Ok(());
    }
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to set permissions on {}", path.display()))
}

pub(crate) fn ensure_existing_ancestor_inside(
    root: &Path,
    target: &Path,
    label: &str,
) -> Result<()> {
    if !target.starts_with(root) {
        bail!(
            "{label} must stay inside {}: {}",
            root.display(),
            target.display()
        );
    }
    if fs::symlink_metadata(root).is_err() {
        return Ok(());
    }

    let root =
        canonical_path(root).with_context(|| format!("Failed to resolve {}", root.display()))?;
    let mut ancestor = target;
    while fs::symlink_metadata(ancestor).is_err() {
        ancestor = ancestor
            .parent()
            .with_context(|| format!("{label} has no existing parent: {}", target.display()))?;
    }
    let ancestor = canonical_path(ancestor)
        .with_context(|| format!("Failed to resolve {}", ancestor.display()))?;
    if ancestor != root && !ancestor.starts_with(&root) {
        bail!(
            "{label} resolves outside {} through an existing symlink or junction: {}",
            root.display(),
            ancestor.display()
        );
    }
    Ok(())
}

pub(crate) fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    if cfg!(windows)
        && let Some(text) = path.to_str()
    {
        if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{stripped}"));
        }
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            let bytes = stripped.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' {
                return PathBuf::from(stripped);
            }
        }
    }
    path
}

pub(crate) fn canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    Ok(strip_extended_prefix(fs::canonicalize(path)?))
}

pub(crate) fn resolve_existing_project_root(path: &Path) -> Result<PathBuf> {
    canonical_path(path)
        .with_context(|| format!("Failed to resolve project root: {}", path.display()))
}

pub(crate) fn resolve_project_root_if_present(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        return Ok(path.to_path_buf());
    }
    resolve_existing_project_root(path)
}

pub(crate) fn resolve_link_project_root(raw: &Path) -> Result<PathBuf> {
    Ok(strip_extended_prefix(resolve_project_root_if_present(raw)?))
}
