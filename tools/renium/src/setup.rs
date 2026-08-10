use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use serde_json::Value;
use serde_json::json;

use super::build_info::VERSION as BUILD_VERSION;
use super::command_line::SetupArgs;
use super::external_tools::download_to_file;
use super::file_io::sha256_hex;
use super::lifecycle;
use super::output::emit_global_output;
use super::rbx_decode::rbx_variant_to_source_string;
#[cfg(target_os = "macos")]
use super::studio_native_serializer;

const GITHUB_REPO: &str = "Superwheat/renium";
pub(crate) const PLUGIN_ASSET_NAME: &str = "Renium.rbxm";

#[cfg(target_os = "macos")]
fn response_with(mut response: Value, key: &str, value: Value) -> Value {
    response
        .as_object_mut()
        .expect("setup response is an object")
        .insert(key.to_string(), value);
    response
}

pub(crate) fn roblox_plugins_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA")
            .context("LOCALAPPDATA is not set; cannot locate the Roblox Plugins folder")?;
        Ok(PathBuf::from(local).join("Roblox").join("Plugins"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME")
            .context("HOME is not set; cannot locate the Roblox Plugins folder")?;
        Ok(PathBuf::from(home)
            .join("Documents")
            .join("Roblox")
            .join("Plugins"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        bail!("Roblox Studio is only available on Windows and macOS; pass --dir explicitly")
    }
}

fn renium_plugin_source(bytes: &[u8]) -> Result<String> {
    let dom = if bytes.starts_with(b"<roblox!") {
        rbx_binary::from_reader(bytes)
            .context("The plugin file is not a valid binary Roblox model")?
    } else {
        rbx_xml::from_reader_default(bytes)
            .context("The plugin file is not a valid XML Roblox model")?
    };
    let runtime = dom
        .descendants()
        .find(|instance| {
            instance.name == "BridgePluginRuntime" && instance.class.as_str() == "ModuleScript"
        })
        .context("The plugin file is not a Renium Studio plugin")?;
    let source = runtime
        .properties
        .iter()
        .find_map(|(name, value)| (name.as_str() == "Source").then_some(value))
        .and_then(rbx_variant_to_source_string)
        .context("The Renium Studio plugin runtime has no readable source")?;
    if !source.contains("BRIDGE_VERSION") {
        bail!("The plugin file is not a Renium Studio plugin");
    }
    Ok(source)
}

fn renium_plugin_version(bytes: &[u8]) -> Result<String> {
    let source = renium_plugin_source(bytes)?;
    let marker = "local BRIDGE_VERSION = \"";
    let start = source
        .find(marker)
        .map(|index| index + marker.len())
        .context("The Renium Studio plugin has no readable version")?;
    let rest = &source[start..];
    let end = rest
        .find('"')
        .context("The Renium Studio plugin version is malformed")?;
    let version = &rest[..end];
    if version.is_empty() {
        bail!("The Renium Studio plugin version is empty");
    }
    Ok(version.to_string())
}

pub(crate) fn validate_rbxm(bytes: &[u8]) -> Result<()> {
    validate_rbxm_version(bytes, BUILD_VERSION)
}

pub(crate) fn validate_rbxm_version(bytes: &[u8], expected: &str) -> Result<()> {
    let version = renium_plugin_version(bytes)?;
    if version != expected {
        bail!("The Studio plugin version {version} does not match Renium {expected}");
    }
    Ok(())
}

fn download_compatible_plugin(destination: &Path) -> Result<String> {
    let url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/v{BUILD_VERSION}/{PLUGIN_ASSET_NAME}"
    );
    download_to_file(&url, destination).with_context(|| {
        format!(
            "Could not download the Studio plugin; check your network or pass --file with a local {PLUGIN_ASSET_NAME}"
        )
    })?;
    Ok(url)
}

pub(super) fn setup_command(args: SetupArgs) -> Result<()> {
    let selected_actions =
        usize::from(args.status) + usize::from(args.repair) + usize::from(args.uninstall);
    if selected_actions > 1 {
        bail!("Use only one of --status, --repair, or --uninstall");
    }
    let plugins_dir = match args.dir.as_ref() {
        Some(dir) => PathBuf::from(dir),
        None => roblox_plugins_dir()?,
    };
    let target = plugins_dir.join(PLUGIN_ASSET_NAME);

    let exe_sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(PLUGIN_ASSET_NAME)))
        .filter(|path| path.is_file());
    if args.status {
        let installed = target.is_file();
        let installed_bytes = installed.then(|| fs::read(&target)).transpose()?;
        let installed_hash = installed_bytes.as_deref().map(sha256_hex);
        let installed_version = installed_bytes
            .as_deref()
            .and_then(|bytes| renium_plugin_version(bytes).ok());
        let identified = installed_version.is_some();
        let compatible = installed_version.as_deref() == Some(BUILD_VERSION);
        let bundled_hash = exe_sibling
            .as_deref()
            .map(fs::read)
            .transpose()?
            .map(|bytes| sha256_hex(&bytes));
        let response = json!({
            "ok": installed && compatible,
            "action": "status",
            "installed": installed,
            "identified": identified,
            "compatible": compatible,
            "installedAt": target,
            "installedVersion": installed_version,
            "installedSha256": installed_hash,
            "bundledSha256": bundled_hash,
            "matchesBundled": installed_hash.is_some() && installed_hash == bundled_hash,
            "version": BUILD_VERSION,
        });
        let text = if !installed {
            format!(
                "The Renium Studio plugin is not installed at {}",
                target.display()
            )
        } else if !compatible {
            match installed_version {
                Some(version) => format!(
                    "Renium Studio plugin {version} is installed at {} and does not match {BUILD_VERSION}",
                    target.display()
                ),
                None => format!(
                    "The file at {} is not an identifiable Renium Studio plugin",
                    target.display()
                ),
            }
        } else {
            format!(
                "Renium Studio plugin {} is installed at {}",
                BUILD_VERSION,
                target.display()
            )
        };
        return emit_global_output(&response, &text);
    }
    if args.uninstall {
        let _lifecycle_lock = (!args.dry_run)
            .then(lifecycle::acquire_lifecycle_lock)
            .transpose()?;
        let installed_bytes = target.is_file().then(|| fs::read(&target)).transpose()?;
        let installed_version = installed_bytes
            .as_deref()
            .map(renium_plugin_version)
            .transpose()
            .with_context(|| {
                format!(
                    "Refusing to remove {} because it is not an identifiable Renium Studio plugin",
                    target.display()
                )
            })?;
        if args.dry_run {
            #[cfg(target_os = "macos")]
            let managed_studio = studio_native_serializer::managed_studio_path()?;
            let response = json!({
                "ok": true,
                "action": "uninstall",
                "dryRun": true,
                "wouldRemove": target,
                "installed": target.is_file(),
                "installedVersion": installed_version,
            });
            #[cfg(target_os = "macos")]
            let response =
                response_with(response, "wouldRemoveManagedStudio", json!(managed_studio));
            return emit_global_output(
                &response,
                &format!("Would remove the Studio plugin at {}", target.display()),
            );
        }
        #[cfg(target_os = "macos")]
        let managed_removal = studio_native_serializer::begin_managed_studio_removal()?;
        if target.is_file()
            && let Err(error) = fs::remove_file(&target)
        {
            #[cfg(target_os = "macos")]
            if let Err(rollback_error) = managed_removal.rollback() {
                return Err(error).context(format!(
                    "Failed to remove {} and managed Studio rollback failed: {rollback_error:#}",
                    target.display()
                ));
            }
            return Err(error).with_context(|| format!("Failed to remove {}", target.display()));
        }
        #[cfg(target_os = "macos")]
        managed_removal.commit()?;
        let response = json!({
            "ok": true,
            "action": "uninstall",
            "removed": target,
            "removedVersion": installed_version,
        });
        return emit_global_output(
            &response,
            &format!("Removed the Studio plugin from {}", target.display()),
        );
    }
    let staging_download =
        std::env::temp_dir().join(format!("renium-plugin-{}.rbxm", std::process::id()));

    let (source_path, source_label) = if let Some(file) = args.file.as_ref() {
        let path = PathBuf::from(file);
        if !path.is_file() {
            bail!("--file {} does not exist", path.display());
        }
        (path, format!("file {file}"))
    } else if let Some(sibling) = exe_sibling {
        let label = format!("bundled {}", sibling.display());
        (sibling, label)
    } else {
        let url = download_compatible_plugin(&staging_download)?;
        (staging_download.clone(), url)
    };

    let bytes = std::fs::read(&source_path)
        .with_context(|| format!("Failed to read {}", source_path.display()))?;
    validate_rbxm(&bytes)?;

    if args.dry_run {
        #[cfg(target_os = "macos")]
        let managed_studio = studio_native_serializer::setup_managed_studio(true)?;
        let _ = std::fs::remove_file(&staging_download);
        let response = json!({
            "ok": true,
            "action": "setup",
            "dryRun": true,
            "source": source_label,
            "wouldInstallTo": target.display().to_string(),
            "bytes": bytes.len(),
        });
        #[cfg(target_os = "macos")]
        let response = response_with(
            response,
            "wouldPrepareStudioAt",
            json!(managed_studio.display().to_string()),
        );
        return emit_global_output(
            &response,
            &format!(
                "Would install the Studio plugin from {source_label} to {}",
                target.display()
            ),
        );
    }

    let _lifecycle_lock = lifecycle::acquire_lifecycle_lock()?;
    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("Failed to create {}", plugins_dir.display()))?;
    #[cfg(target_os = "macos")]
    let previous_plugin = fs::read(&target).ok();
    lifecycle::install_bytes(&target, &bytes)?;
    let _ = std::fs::remove_file(&staging_download);

    #[cfg(target_os = "macos")]
    let managed_studio = match studio_native_serializer::setup_managed_studio(false) {
        Ok(path) => path,
        Err(error) => {
            let rollback = if let Some(previous) = previous_plugin.as_deref() {
                lifecycle::install_bytes(&target, previous)
            } else if target.is_file() {
                fs::remove_file(&target).map_err(anyhow::Error::from)
            } else {
                Ok(())
            };
            if let Err(rollback_error) = rollback {
                return Err(error).context(format!(
                    "Managed Studio setup failed and plugin rollback failed: {rollback_error:#}"
                ));
            }
            return Err(error);
        }
    };
    let response = json!({
        "ok": true,
        "action": if args.repair { "repair" } else { "setup" },
        "source": source_label,
        "installedTo": target.display().to_string(),
        "bytes": bytes.len(),
        "note": "Restart Roblox Studio (or toggle the plugin) to load the new version",
    });
    #[cfg(target_os = "macos")]
    let response = response_with(
        response_with(
            response,
            "managedStudio",
            json!(managed_studio.display().to_string()),
        ),
        "note",
        json!("Open Renium Studio from Applications to use exact protected-property sync"),
    );
    emit_global_output(
        &response,
        &format!(
            "Installed the Studio plugin from {source_label} to {}",
            target.display()
        ),
    )
}
