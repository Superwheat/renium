use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use walkdir::WalkDir;

use super::bridge_server::BridgeServer;
use super::command_line::{EditorRevertArgs, ProjectSourceArgs, PushEditorChangesArgs};
use super::editor_sync::push_editor_changes_with_warm_bridge;
use super::editor_types::{EditorChangeSet, EditorHistoryEntry, EditorRevertManifest};
use super::file_io::{
    absolutize_under, path_key, read_json_file, resolve_project_root_if_present,
    service_settings_path, write_json_file, write_utf8_file,
};
use super::project_layout::apply_configured_project_layout;
use super::snapshot_export::parse_bridge_ports;
use super::sourcemap::path_to_sourcemap_relative;
use super::timing::current_millis;

pub(super) struct EditorHistoryTransaction {
    stage_root: PathBuf,
    history_root: PathBuf,
    published: Vec<(PathBuf, PathBuf)>,
    active: bool,
}

impl EditorHistoryTransaction {
    fn create(project_root: &Path) -> Result<Self> {
        static HISTORY_STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);
        let renium_root = project_root.join(".renium");
        fs::create_dir_all(&renium_root)
            .with_context(|| format!("Failed to create {}", renium_root.display()))?;
        let stage_root = renium_root.join(format!(
            ".editor-history-stage-{}-{}-{}",
            std::process::id(),
            current_millis(),
            HISTORY_STAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&stage_root)
            .with_context(|| format!("Failed to create {}", stage_root.display()))?;
        Ok(Self {
            stage_root,
            history_root: renium_root.join("editor-history"),
            published: Vec::new(),
            active: true,
        })
    }

    pub(super) fn publish(&mut self) -> Result<()> {
        fs::create_dir_all(&self.history_root)
            .with_context(|| format!("Failed to create {}", self.history_root.display()))?;
        let mut entries = fs::read_dir(&self.stage_root)
            .with_context(|| format!("Failed to read {}", self.stage_root.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let source = entry.path();
            let destination = self.history_root.join(entry.file_name());
            if let Err(error) = fs::rename(&source, &destination) {
                let rollback = self.rollback_published();
                if let Err(rollback_error) = rollback {
                    self.active = false;
                    bail!(
                        "Failed to publish editor history: {error}; rollback failed: {rollback_error}; recovery files remain in {}",
                        self.stage_root.display()
                    );
                }
                return Err(error).with_context(|| {
                    format!(
                        "Failed to publish editor history to {}",
                        destination.display()
                    )
                });
            }
            self.published.push((source, destination));
        }
        Ok(())
    }

    fn rollback_published(&mut self) -> Result<()> {
        for (source, destination) in self.published.iter().rev() {
            if destination.exists() {
                fs::rename(destination, source).with_context(|| {
                    format!(
                        "Failed to restore pending editor history from {}",
                        destination.display()
                    )
                })?;
            }
        }
        self.published.clear();
        Ok(())
    }

    pub(super) fn commit(mut self) {
        self.active = false;
        let _ = fs::remove_dir(&self.stage_root);
    }
}

impl Drop for EditorHistoryTransaction {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Err(error) = self.rollback_published() {
            eprintln!(
                "[renium] editor history rollback failed: {error:#}; recovery files remain in {}",
                self.stage_root.display()
            );
            self.active = false;
            return;
        }
        let _ = fs::remove_dir_all(&self.stage_root);
    }
}

#[derive(Deserialize)]
struct EditorHistorySourceBatch {
    rows: Vec<EditorHistorySourceRow>,
}

#[derive(Deserialize)]
struct EditorHistorySourceRow {
    index: usize,
    source: Option<String>,
    error: Option<String>,
}

fn fetch_editor_history_sources(
    bridge: &BridgeServer,
    entries: &[EditorHistoryEntry],
) -> (HashMap<usize, String>, HashMap<usize, String>) {
    let mut indexes_by_service = BTreeMap::<&str, Vec<usize>>::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.source_key.is_some() {
            indexes_by_service
                .entry(&entry.service)
                .or_default()
                .push(index);
        }
    }

    let mut sources = HashMap::new();
    let mut errors = HashMap::new();
    for (service, indexes) in indexes_by_service {
        for batch in indexes.chunks(64) {
            let selectors = batch
                .iter()
                .map(|index| {
                    let entry = &entries[*index];
                    json!({
                        "index": index,
                        "pathSegments": &entry.path_segments,
                        "pathOrdinals": &entry.path_ordinals,
                    })
                })
                .collect::<Vec<_>>();
            for index in batch {
                errors.insert(
                    *index,
                    "Studio did not return the script Source".to_string(),
                );
            }
            let response = bridge
                .call(
                    "getLiveSourceBatch",
                    json!({ "service": service, "selectors": selectors }),
                )
                .and_then(|value| {
                    serde_json::from_value::<EditorHistorySourceBatch>(value)
                        .context("Studio returned an invalid live source batch")
                });
            match response {
                Ok(response) => {
                    for row in response.rows {
                        if let Some(source) = row.source {
                            sources.insert(row.index, source);
                            errors.remove(&row.index);
                        } else if let Some(error) = row.error {
                            errors.insert(row.index, error);
                        }
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    for index in batch {
                        errors.insert(*index, message.clone());
                    }
                }
            }
        }
    }
    (sources, errors)
}

pub(super) fn save_editor_history_entries(
    bridge: &BridgeServer,
    project_root_arg: &Path,
    changes: &EditorChangeSet,
) -> Result<Option<EditorHistoryTransaction>> {
    if changes.history_entries.is_empty() {
        return Ok(None);
    }
    let project_root = resolve_project_root_if_present(project_root_arg)?;
    let transaction = EditorHistoryTransaction::create(&project_root)?;
    let mut seen = HashSet::new();
    let (source_before_by_index, source_errors_by_index) =
        fetch_editor_history_sources(bridge, &changes.history_entries);

    for (sequence, entry) in changes.history_entries.iter().enumerate() {
        let identity = format!(
            "{}:{}:{}",
            entry.service,
            entry.settings_id.as_deref().unwrap_or(""),
            entry
                .source_path
                .as_ref()
                .map(|path| path_key(path))
                .unwrap_or_default()
        );
        if !seen.insert(identity) {
            continue;
        }

        let source_path = entry
            .source_path
            .as_ref()
            .map(|path| absolutize_under(&project_root, path));
        let source_before = source_before_by_index.get(&sequence);
        let source_changed = source_before.is_some_and(|before| {
            source_path
                .as_ref()
                .and_then(|path| fs::read_to_string(path).ok())
                .as_ref()
                != Some(before)
        });
        if entry.settings_before.is_none() && !source_changed {
            if let Some(error) = source_errors_by_index.get(&sequence) {
                eprintln!(
                    "[renium] editor history: failed to save Source for {}: {}",
                    entry.path_segments.join("."),
                    error
                );
            }
            continue;
        }

        let created_unix_ms = current_millis();
        let safe_name = sanitize_history_component(
            entry
                .settings_id
                .as_deref()
                .or_else(|| entry.path_segments.last().map(String::as_str))
                .unwrap_or("item"),
        );
        let entry_dir = transaction.stage_root.join(format!(
            "{created_unix_ms}-{sequence}-{}-{safe_name}",
            entry.service
        ));
        fs::create_dir_all(&entry_dir)
            .with_context(|| format!("Failed to create {}", entry_dir.display()))?;

        let settings_backup = if let Some(document) = entry.settings_before.as_ref() {
            let file_name = "settings.renium";
            document.write_file(&entry_dir.join(file_name))?;
            Some(file_name.to_string())
        } else {
            None
        };

        let source_backup = if source_changed {
            let file_name = "source.luau";
            write_utf8_file(
                &entry_dir.join(file_name),
                source_before.expect("changed Source has a previous value"),
            )?;
            Some(file_name.to_string())
        } else {
            None
        };

        let source_path = source_path
            .as_ref()
            .map(|path| path_to_sourcemap_relative(&project_root, path));
        let manifest = EditorRevertManifest {
            version: 1,
            created_unix_ms,
            service: entry.service.clone(),
            source_path,
            settings_id: entry.settings_id.clone(),
            path_segments: entry.path_segments.clone(),
            class_name: entry.class_name.clone(),
            settings_backup,
            source_backup,
        };
        write_json_file(&entry_dir.join("manifest.json"), &manifest, false)?;
    }

    Ok(Some(transaction))
}

pub(super) fn editor_revert(mut args: EditorRevertArgs) -> Result<()> {
    apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    let project_root = resolve_project_root_if_present(&args.project_root)?;
    let src_root = absolutize_under(&project_root, &args.src_dir);
    let requested_path = args
        .path
        .as_ref()
        .map(|path| path_key(&absolutize_under(&project_root, path)));
    if requested_path.is_none() && args.settings_id.is_none() {
        bail!("Provide --path or --settings-id");
    }

    let history_root = project_root.join(".renium").join("editor-history");
    let mut candidates = Vec::new();
    if history_root.exists() {
        for entry in WalkDir::new(&history_root)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name().to_string_lossy() == "manifest.json")
        {
            let manifest: EditorRevertManifest = read_json_file(entry.path())?;
            if let Some(service) = args.service.as_deref()
                && manifest.service != service
            {
                continue;
            }
            let settings_matches = args
                .settings_id
                .as_deref()
                .is_some_and(|settings_id| manifest.settings_id.as_deref() == Some(settings_id));
            let path_matches = requested_path.as_ref().is_some_and(|requested| {
                manifest
                    .source_path
                    .as_ref()
                    .map(|source_path| {
                        path_key(&absolutize_under(&project_root, Path::new(source_path)))
                    })
                    .as_ref()
                    == Some(requested)
            });
            if settings_matches || path_matches {
                candidates.push((
                    manifest.created_unix_ms,
                    entry.path().to_path_buf(),
                    manifest,
                ));
            }
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let Some((_created, manifest_path, manifest)) = candidates.into_iter().next() else {
        bail!("No editor revert history found for requested path/item");
    };
    let manifest_dir = manifest_path
        .parent()
        .context("Editor revert manifest has no parent directory")?;

    let mut changed_paths = Vec::new();
    if let Some(settings_backup) = manifest.settings_backup.as_deref() {
        let from = manifest_dir.join(settings_backup);
        let to = service_settings_path(&src_root.join(&manifest.service));
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::copy(&from, &to)
            .with_context(|| format!("Failed to restore {} to {}", from.display(), to.display()))?;
        changed_paths.push(to);
    }
    if let (Some(source_backup), Some(source_path)) = (
        manifest.source_backup.as_deref(),
        manifest.source_path.as_deref(),
    ) {
        let source = fs::read_to_string(manifest_dir.join(source_backup)).with_context(|| {
            format!(
                "Failed to read source backup {}",
                manifest_dir.join(source_backup).display()
            )
        })?;
        let to = absolutize_under(&project_root, Path::new(source_path));
        write_utf8_file(&to, &source)?;
        changed_paths.push(to);
    }

    println!(
        "[renium] editor revert restored: service={}, settings_id={}, path={}",
        manifest.service,
        manifest.settings_id.as_deref().unwrap_or(""),
        manifest.source_path.as_deref().unwrap_or("")
    );
    println!(
        "__ROBLOX_SYNC_EDITOR_REVERT_RESULT__ {}",
        json!({
            "ok": true,
            "service": manifest.service,
            "settingsId": manifest.settings_id,
            "sourcePath": manifest.source_path,
            "changedPaths": changed_paths.iter().map(|path| path_to_sourcemap_relative(&project_root, path)).collect::<Vec<_>>(),
        })
    );

    if args.apply_studio && !changed_paths.is_empty() {
        let ports = parse_bridge_ports(&args.bridge.ports)?;
        let (bridge, _) =
            BridgeServer::listen(&args.bridge.host, &ports, args.bridge.wait_seconds)?;
        push_editor_changes_with_warm_bridge(
            PushEditorChangesArgs {
                changed_paths,
                verify_sources: true,
                ..PushEditorChangesArgs::new(
                    ProjectSourceArgs {
                        project_root,
                        src_root: args.src_dir,
                    },
                    args.bridge,
                )
            },
            &bridge,
        )?;
    }

    Ok(())
}

fn sanitize_history_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "item".to_string()
    } else {
        out
    }
}
