use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use serde_json::Value;
use serde_json::json;

use crate::app::build::VERSION as BUILD_VERSION;
use crate::app::mcp::register;
use crate::app::output::emit_global_output;
use crate::app::update;
use crate::cli::SetupArgs;
use crate::rbx::decode::rbx_variant_to_source_string;
#[cfg(target_os = "macos")]
use crate::studio::native::serializer;
use crate::system::files::{resolved_current_executable, sha256_hex};
use crate::system::tools::download_to_file;

const GITHUB_REPO: &str = "renium-rbx/renium";
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

struct PluginSource {
    bytes: Vec<u8>,
    label: String,
    /// The copy beside the executable that these downloaded bytes can replace.
    replaces: Option<PathBuf>,
}

/// The plugin to install: `--file`, else the copy beside the executable when it is the plugin of `version`
/// (an installer writes that copy, and it can be from another release than the executable), else a download.
fn plugin_source(
    file: Option<&str>,
    bundled: Option<&Path>,
    version: &str,
    download: impl FnOnce(&Path) -> Result<String>,
) -> Result<PluginSource> {
    if let Some(file) = file {
        let path = Path::new(file);
        if !path.is_file() {
            bail!("--file {} does not exist", path.display());
        }
        return Ok(PluginSource {
            bytes: fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?,
            label: format!("file {file}"),
            replaces: None,
        });
    }
    if let Some(path) = bundled
        && let Ok(bytes) = fs::read(path)
        && validate_rbxm_version(&bytes, version).is_ok()
    {
        return Ok(PluginSource {
            bytes,
            label: format!("bundled {}", path.display()),
            replaces: None,
        });
    }
    let staging = std::env::temp_dir().join(format!("renium-plugin-{}.rbxm", std::process::id()));
    let fetched = download(&staging).and_then(|label| {
        let bytes =
            fs::read(&staging).with_context(|| format!("Failed to read {}", staging.display()))?;
        Ok((bytes, label))
    });
    let _ = fs::remove_file(&staging);
    match (fetched, bundled) {
        (Ok((bytes, label)), _) => Ok(PluginSource {
            bytes,
            label,
            replaces: bundled.map(Path::to_path_buf),
        }),
        (Err(error), Some(path)) => Err(error.context(format!(
            "{} is not the plugin of Renium {version}",
            path.display()
        ))),
        (Err(error), None) => Err(error),
    }
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

pub(crate) fn setup_command(args: SetupArgs) -> Result<()> {
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

    let exe_sibling = resolved_current_executable()
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
            "agentHints": crate::app::agent_hints::status(),
        });
        #[cfg(target_os = "macos")]
        let response = if args.dir.is_none() {
            response_with(
                response,
                "studioPatches",
                json!(serializer::studio_patch_status()?),
            )
        } else {
            response
        };
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
            .then(update::acquire_lifecycle_lock)
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
            let studio = args
                .dir
                .is_none()
                .then(serializer::patched_studio_path)
                .transpose()?;
            let response = json!({
                "ok": true,
                "action": "uninstall",
                "dryRun": true,
                "wouldRemove": target,
                "installed": target.is_file(),
                "installedVersion": installed_version,
            });
            #[cfg(target_os = "macos")]
            let response = if let Some(studio) = studio {
                response_with(response, "wouldRestoreStudio", json!(studio))
            } else {
                response
            };
            return emit_global_output(
                &response,
                &format!("Would remove the Studio plugin at {}", target.display()),
            );
        }
        #[cfg(target_os = "macos")]
        let studio_patch_removal = args
            .dir
            .is_none()
            .then(serializer::begin_studio_patch_removal)
            .transpose()?;
        if target.is_file()
            && let Err(error) = fs::remove_file(&target)
        {
            #[cfg(target_os = "macos")]
            if let Some(removal) = studio_patch_removal
                && let Err(rollback_error) = removal.rollback()
            {
                return Err(error).context(format!(
                    "Failed to remove {} and Studio patch rollback failed: {rollback_error:#}",
                    target.display()
                ));
            }
            return Err(error).with_context(|| format!("Failed to remove {}", target.display()));
        }
        #[cfg(target_os = "macos")]
        if let Some(removal) = studio_patch_removal {
            removal.commit()?;
        }
        let agent_hints = crate::app::agent_hints::remove()?;
        let mcp = register::remove();
        let mut response = json!({
            "ok": true,
            "action": "uninstall",
            "removed": target,
            "removedVersion": installed_version,
            "agentHintsRemoved": agent_hints,
            "mcpRemoved": mcp.changed,
        });
        if !mcp.failed.is_empty() {
            response["mcpFailed"] = mcp.failures();
        }
        let mut text = format!("Removed the Studio plugin from {}", target.display());
        if let Some(note) = mcp.describe("removed from") {
            text = format!("{text}; {note}");
        }
        return emit_global_output(&response, &text);
    }
    let PluginSource {
        bytes,
        label: source_label,
        replaces,
    } = plugin_source(
        args.file.as_deref(),
        exe_sibling.as_deref(),
        BUILD_VERSION,
        download_compatible_plugin,
    )?;
    validate_rbxm(&bytes)?;

    if args.dry_run {
        #[cfg(target_os = "macos")]
        let studio = args
            .dir
            .is_none()
            .then(|| serializer::setup_studio_patch(true))
            .transpose()?;
        let response = json!({
            "ok": true,
            "action": "setup",
            "dryRun": true,
            "source": source_label,
            "wouldInstallTo": target.display().to_string(),
            "bytes": bytes.len(),
        });
        #[cfg(target_os = "macos")]
        let response = if let Some(studio) = studio {
            response_with(
                response,
                "wouldPatchStudioAt",
                json!(studio.display().to_string()),
            )
        } else {
            response
        };
        return emit_global_output(
            &response,
            &format!(
                "Would install the Studio plugin from {source_label} to {}",
                target.display()
            ),
        );
    }

    let _lifecycle_lock = update::acquire_lifecycle_lock()?;
    std::fs::create_dir_all(&plugins_dir)
        .with_context(|| format!("Failed to create {}", plugins_dir.display()))?;
    #[cfg(target_os = "macos")]
    let previous_plugin = fs::read(&target).ok();
    update::install_bytes(&target, &bytes)?;
    if args.dir.is_none()
        && let Some(outdated) = replaces.as_deref()
    {
        let _ = update::install_bytes(outdated, &bytes);
    }

    #[cfg(target_os = "macos")]
    // An explicit plugin destination is a scoped plugin installation, including
    // repair/uninstall. It must not inspect or mutate machine-wide Studio apps.
    let studio = match args
        .dir
        .is_none()
        .then(|| serializer::setup_studio_patch(false))
        .transpose()
    {
        Ok(path) => path,
        Err(error) => {
            let rollback = if let Some(previous) = previous_plugin.as_deref() {
                update::install_bytes(&target, previous)
            } else if target.is_file() {
                fs::remove_file(&target).map_err(anyhow::Error::from)
            } else {
                Ok(())
            };
            if let Err(rollback_error) = rollback {
                return Err(error).context(format!(
                    "Studio patch setup failed and plugin rollback failed: {rollback_error:#}"
                ));
            }
            return Err(error);
        }
    };
    let agent_hints = crate::app::agent_hints::install(BUILD_VERSION)?;
    let mcp = register::install(&register::setup_executable()?);
    let mut response = json!({
        "ok": true,
        "action": if args.repair { "repair" } else { "setup" },
        "source": source_label,
        "installedTo": target.display().to_string(),
        "bytes": bytes.len(),
        "agentHints": crate::app::agent_hints::status(),
        "agentHintsWritten": agent_hints,
        "mcpRegistered": mcp.changed,
        "note": "Restart Roblox Studio (or toggle the plugin) to load the new version",
    });
    if !mcp.failed.is_empty() {
        response["mcpFailed"] = mcp.failures();
    }
    #[cfg(target_os = "macos")]
    let response = if let Some(studio) = studio {
        response_with(
            response_with(response, "studio", json!(studio.display().to_string())),
            "note",
            json!("Restart Roblox Studio to load the updated Renium plugin and native helper"),
        )
    } else {
        response
    };
    let mut text = format!(
        "Installed the Studio plugin from {source_label} to {}; {}",
        target.display(),
        crate::app::agent_hints::describe(&agent_hints)
    );
    if let Some(note) = mcp.describe("added to") {
        text = format!("{text}; {note}");
    }
    emit_global_output(&response, &text)
}

#[cfg(test)]
mod tests {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    use super::*;

    fn plugin_model(version: &str) -> Vec<u8> {
        let runtime = InstanceBuilder::new("ModuleScript")
            .with_name("BridgePluginRuntime")
            .with_property(
                "Source",
                format!("local BRIDGE_VERSION = \"{version}\"\nreturn {{}}\n"),
            );
        let dom = WeakDom::new(InstanceBuilder::new("Folder").with_child(runtime));
        let mut bytes = Vec::new();
        rbx_binary::to_writer(&mut bytes, &dom, &[dom.root_ref()]).unwrap();
        bytes
    }

    fn released(staging: &Path) -> Result<String> {
        fs::write(staging, plugin_model("0.6.4"))?;
        Ok("release".to_string())
    }

    fn unreachable(_: &Path) -> Result<String> {
        bail!("no network")
    }

    #[test]
    fn a_bundled_plugin_of_another_release_is_replaced_by_a_download() {
        let folder = std::env::temp_dir().join(format!("renium-setup-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let bundled = folder.join(PLUGIN_ASSET_NAME);
        let given = folder.join("given.rbxm");
        fs::write(&given, plugin_model("0.6.3")).unwrap();

        fs::write(&bundled, plugin_model("0.6.4")).unwrap();
        let current = plugin_source(None, Some(&bundled), "0.6.4", unreachable).unwrap();
        assert_eq!(current.bytes, plugin_model("0.6.4"));
        assert_eq!(current.label, format!("bundled {}", bundled.display()));
        assert_eq!(current.replaces, None);

        for outdated in [plugin_model("0.5.8"), b"not a model".to_vec()] {
            fs::write(&bundled, outdated).unwrap();
            let downloaded = plugin_source(None, Some(&bundled), "0.6.4", released).unwrap();
            assert_eq!(downloaded.bytes, plugin_model("0.6.4"));
            assert_eq!(downloaded.label, "release");
            assert_eq!(downloaded.replaces.as_deref(), Some(bundled.as_path()));
        }

        let chosen = plugin_source(given.to_str(), Some(&bundled), "0.6.4", unreachable).unwrap();
        assert_eq!(chosen.bytes, plugin_model("0.6.3"));
        assert_eq!(chosen.replaces, None);

        let offline = plugin_source(None, Some(&bundled), "0.6.4", unreachable)
            .err()
            .map(|error| format!("{error:#}"));
        assert_eq!(
            offline,
            Some(format!(
                "{} is not the plugin of Renium 0.6.4: no network",
                bundled.display()
            ))
        );

        let alone = plugin_source(None, None, "0.6.4", released).unwrap();
        assert_eq!(alone.bytes, plugin_model("0.6.4"));
        assert_eq!(alone.replaces, None);

        fs::remove_dir_all(&folder).unwrap();
    }
}
