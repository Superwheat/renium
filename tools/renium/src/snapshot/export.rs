use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::app::build::{
    GIT_HASH as BUILD_GIT_HASH, TIMESTAMP_UNIX as BUILD_TIMESTAMP_UNIX, VERSION as BUILD_VERSION,
};
use crate::app::output::{log_global, print_json_output};
use crate::app::timing::{
    elapsed_ms, log_timing_ms, quiet_timings, set_quiet_timings, verbose_timing_logs,
};
use crate::automation::op;
use crate::cli::PullArgs;
use crate::daemon::daemon_control_request;
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::project::sourcemap::generate_project_sourcemap;
use crate::project::structural::{
    moved_references_between_documents, rewrite_moved_references, service_store_paths,
};
use crate::roblox::schema::{configure_bridge_property_candidates, load_rbx_dom_property_schema};
use crate::settings::bytecode::{SettingsBytecode, encode_settings_bytecode};
use crate::settings::equivalence::{SettingsAlignment, align_settings_bytes_to_reference};
use crate::snapshot::import::{
    DirectImportDispatcher, SourcemapWriter, build_service_state_from_instances,
    direct_import_export_order, normalize_class_defaults, parse_services,
    resolve_direct_import_workers,
};
use crate::snapshot::types::{
    ExportedSnapshotParts, ServiceExecutionSpan, ServiceExportOutput, ServiceState,
};
use crate::studio::bridge::{
    BridgeChunk, BridgeInfoPayload, BridgeServer, BridgeTarget, ChunkFetchMetrics,
    MAX_BRIDGE_CHUNK_BYTES, MAX_BRIDGE_REASSEMBLY_BYTES,
};
use crate::studio::native::editor::{EditorBinaryExportFinishGuard, editor_binary_export_parts};
use crate::system::files::{
    OnDrop, create_unique_directory, fnv1a, is_service_settings_file_name,
    resolve_existing_project_root, sanitize_name, sha256_hex, write_bytes_if_changed,
};

pub(crate) const BRIDGE_PROTOCOL_VERSION: &str = "compact-v5";
pub(crate) const LARGE_SERVICE_DETERMINISTIC_FETCH_MIN_INSTANCES: usize = 20_000;
const BRIDGE_CHUNK_FRAME_PROTOCOL_VERSION: &str = "rbs2";
const BRIDGE_COMPACT_VALUE_PROTOCOL_VERSION: &str = "compact-v5-schema-4";
const BRIDGE_CODEC_VERSION_SCHEMA9: &str = "compact-v5-schema-9";
const BRIDGE_CODEC_VERSION_SCHEMA8: &str = "compact-v5-schema-8";
const BRIDGE_CODEC_VERSION: &str = BRIDGE_CODEC_VERSION_SCHEMA9;
const SUPPORTED_BRIDGE_CODEC_VERSIONS: [&str; 2] =
    [BRIDGE_CODEC_VERSION, BRIDGE_CODEC_VERSION_SCHEMA8];

fn record_bridge_sync_completion(bridge: &BridgeServer) -> Result<()> {
    bridge
        .call("recordSyncCompletion", json!({}))
        .context("Failed to record Studio sync completion after export publication")?;
    Ok(())
}

pub(crate) fn is_transient_bridge_error(err: &anyhow::Error) -> bool {
    let message = err
        .chain()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    [
        "Bridge call failed",
        "Bridge send failed",
        "Bridge read failed",
        "Bridge closed while waiting",
        "closed before hello",
        "failed waiting for hello",
        "No plugin bridge channels connected",
        "Only ",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlaceGuardConfig {
    pub(crate) allowed_place_ids: Vec<i64>,
    allowed_game_ids: Vec<i64>,
}

pub(crate) fn parse_place_guard_config(text: &str, path: &Path) -> Result<PlaceGuardConfig> {
    let config: PlaceGuardConfig = serde_json::from_str(text)
        .with_context(|| format!("Invalid place guard JSON in {}", path.display()))?;
    if config.allowed_place_ids.is_empty() && config.allowed_game_ids.is_empty() {
        bail!(
            "Place guard {} must contain at least one allowedPlaceIds or allowedGameIds entry; remove the file to disable the guard",
            path.display()
        );
    }
    Ok(config)
}

fn place_guard_config_path() -> PathBuf {
    std::env::var_os("RENIUM_CONFIG")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("renium.config.json"), PathBuf::from)
}

fn active_place_guard() -> Result<Option<PlaceGuardConfig>> {
    if std::env::var("RENIUM_ALLOW_ANY_PLACE").is_ok_and(|value| value == "1") {
        return Ok(None);
    }
    let path = place_guard_config_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read place guard {}", path.display()));
        }
    };
    Ok(Some(parse_place_guard_config(&text, &path)?))
}

fn ensure_place_allowed(info: &BridgeInfoPayload) -> Result<()> {
    let Some(guard) = active_place_guard()? else {
        return Ok(());
    };
    let place_allowed = info
        .place_id
        .is_some_and(|id| guard.allowed_place_ids.contains(&id));
    let game_allowed = info
        .game_id
        .is_some_and(|id| guard.allowed_game_ids.contains(&id));
    if place_allowed || game_allowed {
        return Ok(());
    }
    let config_path = place_guard_config_path();
    bail!(
        "Refusing bridge connection from place '{}' (placeId {}, gameId {}): not listed in {} allowedPlaceIds/allowedGameIds. Unsaved local places report placeId 0; add 0 to the allowlist or set RENIUM_ALLOW_ANY_PLACE=1 to override.",
        info.place_name,
        info.place_id
            .map_or_else(|| "none".to_string(), |id| id.to_string()),
        info.game_id
            .map_or_else(|| "none".to_string(), |id| id.to_string()),
        config_path.display()
    )
}

pub(crate) fn validate_bridge_info(info: &BridgeInfoPayload) -> Result<()> {
    ensure_place_allowed(info)?;
    if info.protocol_version != BRIDGE_PROTOCOL_VERSION {
        bail!(
            "Unsupported plugin protocol {} (expected {})",
            info.protocol_version,
            BRIDGE_PROTOCOL_VERSION
        );
    }
    if !is_supported_bridge_codec(&info.codec_version) {
        bail!(
            "Unsupported plugin codec {} (expected one of {})",
            info.codec_version,
            SUPPORTED_BRIDGE_CODEC_VERSIONS.join(", ")
        );
    }
    if info.chunk_frame_protocol_version != BRIDGE_CHUNK_FRAME_PROTOCOL_VERSION {
        bail!(
            "Unsupported plugin chunk frame protocol {} (expected {})",
            info.chunk_frame_protocol_version,
            BRIDGE_CHUNK_FRAME_PROTOCOL_VERSION
        );
    }
    if info.compact_value_protocol_version != BRIDGE_COMPACT_VALUE_PROTOCOL_VERSION {
        bail!(
            "Unsupported plugin compact value protocol {} (expected {})",
            info.compact_value_protocol_version,
            BRIDGE_COMPACT_VALUE_PROTOCOL_VERSION
        );
    }
    Ok(())
}

pub(crate) fn is_supported_bridge_codec(value: &str) -> bool {
    SUPPORTED_BRIDGE_CODEC_VERSIONS.contains(&value)
}

fn finish_service_export_output(
    output: ServiceExportOutput,
    dispatcher: &DirectImportDispatcher,
    service_export_spans: &mut Vec<ServiceExecutionSpan>,
    cumulative_service_latency_ms: &mut f64,
) -> Result<()> {
    *cumulative_service_latency_ms += output.span.export_end_ms - output.span.export_start_ms;
    dispatcher.check_error()?;
    dispatcher.enqueue_parts(&output.span.service, output.parts)?;
    service_export_spans.push(output.span);
    Ok(())
}

struct ExportPrelude {
    total_started: Instant,
    project_root: PathBuf,
    services: Vec<String>,
}

pub(crate) struct ExportProjectStage {
    pub(crate) project_root: PathBuf,
    container: PathBuf,
    pub(crate) import_project_root: PathBuf,
    pub(crate) import_src_dir: PathBuf,
    publish_paths: Vec<PathBuf>,
    publish_baseline: BTreeMap<PathBuf, PublishEntryState>,
    pub(crate) loaded: Option<config::LoadedProject>,
    pub(crate) projection: Option<config::ProjectionStage>,
    settings_already_aligned: bool,
    active: bool,
}

impl ExportProjectStage {
    pub(crate) fn create(project_root: &Path, src_dir: &Path, services: &[String]) -> Result<Self> {
        Self::create_inner(project_root, src_dir, services, true)
    }

    pub(crate) fn create_for_comparison(
        project_root: &Path,
        src_dir: &Path,
        services: &[String],
    ) -> Result<Self> {
        if let Some(project) = config::try_load_project(None, Some(project_root))?
            && config::project_requires_temporary_stage(&project)?
        {
            bail!("Project requires a staged comparison");
        }
        Self::create_inner(project_root, src_dir, services, false)
    }

    fn create_inner(
        project_root: &Path,
        src_dir: &Path,
        services: &[String],
        clone_project_data: bool,
    ) -> Result<Self> {
        let started = Instant::now();
        let parent = project_root
            .parent()
            .context("Project root has no parent directory")?;
        let project_name = project_root
            .file_name()
            .context("Project root has no directory name")?;
        let container = create_unique_directory(parent, ".renium-export-")?;
        let mut cleanup = OnDrop::new(|| {
            let _ = fs::remove_dir_all(&container);
        });
        let staged_root = container.join(project_name);
        fs::create_dir_all(&staged_root)
            .with_context(|| format!("Failed to create {}", staged_root.display()))?;
        let mut loaded = config::try_load_project(None, Some(project_root))?
            .filter(|loaded| loaded.root == project_root);
        if let Some(loaded) = loaded.as_mut() {
            scope_export_project(loaded, services);
        }
        let mut publish_paths = Vec::new();
        let mut clone_paths = Vec::new();
        if let Some(loaded) = loaded.as_ref() {
            collect_configured_export_paths(
                loaded,
                project_root,
                services,
                clone_project_data,
                &mut clone_paths,
                &mut publish_paths,
            )?;
        } else {
            for service in services {
                let path = src_dir.join(sanitize_name(service));
                if clone_project_data {
                    clone_paths.push(path.clone());
                }
                publish_paths.push(path);
            }
        }
        if clone_project_data {
            clone_paths.push(PathBuf::from("sourcemap.json"));
        }
        publish_paths.push(PathBuf::from("sourcemap.json"));
        normalize_owned_paths(&mut clone_paths);
        normalize_owned_paths(&mut publish_paths);
        let publish_baseline = if clone_project_data {
            collect_publish_hashes(project_root, &publish_paths)?
        } else {
            BTreeMap::new()
        };
        for relative in &clone_paths {
            let source = project_root.join(relative);
            if source.exists() {
                copy_isolated_path(&source, &staged_root.join(relative))?;
            }
        }
        let staged_loaded = if let Some(original) = loaded.as_ref() {
            let relative = original.path.strip_prefix(project_root)?;
            // Only the private copy is scoped; the user's configuration is
            // neither rewritten nor included in publication.
            fs::write(
                staged_root.join(relative),
                serde_json::to_vec(&original.project)?,
            )?;
            Some(config::load_project(
                Some(&staged_root.join(relative)),
                None,
            )?)
        } else {
            None
        };
        let projection = staged_loaded
            .as_ref()
            .map(config::stage_project)
            .transpose()?;
        let (import_project_root, import_src_dir) = match projection.as_ref() {
            Some(projection) if projection.is_temporary() => {
                (projection.root().to_path_buf(), PathBuf::from("."))
            }
            Some(projection) => (
                staged_root.clone(),
                projection
                    .root()
                    .strip_prefix(&staged_root)
                    .unwrap_or(src_dir)
                    .to_path_buf(),
            ),
            None => (staged_root.clone(), src_dir.to_path_buf()),
        };
        cleanup.disarm();
        drop(cleanup);
        let stage = Self {
            project_root: staged_root,
            container,
            import_project_root,
            import_src_dir,
            publish_paths,
            publish_baseline,
            loaded: staged_loaded,
            projection,
            settings_already_aligned: false,
            active: true,
        };
        log_global(
            5,
            format_args!(
                "[renium] export project stage copy: {:.1}ms",
                elapsed_ms(started)
            ),
        );
        log_timing_ms("export project stage copy", elapsed_ms(started));
        Ok(stage)
    }

    pub(crate) fn mark_settings_aligned(&mut self) {
        self.settings_already_aligned = self
            .projection
            .as_ref()
            .is_none_or(|projection| !projection.is_temporary())
            && self
                .loaded
                .as_ref()
                .is_none_or(|loaded| loaded.project.adapters.is_empty());
    }

    pub(crate) fn finish_projection(&self, generate_sourcemap: bool) -> Result<()> {
        if let (Some(loaded), Some(projection)) = (&self.loaded, &self.projection)
            && projection.is_temporary()
        {
            config::syncback_project_projection(loaded, projection.root(), false)?;
        }
        if let Some(loaded) = &self.loaded {
            let adapter_root = self.projection.as_ref().map_or_else(
                || loaded.root.join(&loaded.project.source_root),
                |projection| projection.root().to_path_buf(),
            );
            config::syncback_project_adapters_from_root(loaded, &adapter_root, false)?;
        }
        if generate_sourcemap {
            generate_project_sourcemap(&self.project_root)?;
        }
        Ok(())
    }

    pub(crate) fn capture_sourcemap_needs_regeneration(&self) -> bool {
        self.projection
            .as_ref()
            .is_some_and(config::ProjectionStage::is_temporary)
            || self
                .loaded
                .as_ref()
                .is_some_and(|loaded| !loaded.project.adapters.is_empty())
    }

    pub(crate) fn publish_paths(&self) -> &[PathBuf] {
        &self.publish_paths
    }

    pub(crate) fn capture_publish_baseline(&mut self, project_root: &Path) -> Result<()> {
        self.publish_baseline = collect_publish_hashes(project_root, &self.publish_paths)?;
        Ok(())
    }

    pub(crate) fn preview_operations(&self, project_root: &Path) -> Result<Vec<Value>> {
        let staged = collect_publish_hashes(&self.project_root, &self.publish_paths)?;
        let current = collect_publish_hashes(project_root, &self.publish_paths)?;
        let mut adapter_paths = Vec::new();
        if let Some(loaded) = self.loaded.as_ref() {
            for adapter in &loaded.project.adapters {
                adapter_paths.push(adapter.source.clone());
                if let Some(output) = config::project_adapter_output_path(loaded, adapter)? {
                    adapter_paths.push(output.strip_prefix(&loaded.root)?.to_path_buf());
                }
            }
            adapter_paths.push(PathBuf::from(".renium/adapter-baseline.json"));
        }
        let paths = publish_operation_paths(&current, &staged);
        let mut operations = Vec::new();
        for relative in paths {
            let action = if staged.contains_key(&relative) {
                "write"
            } else {
                "delete"
            };
            let kind = if adapter_paths
                .iter()
                .any(|path| relative == *path || relative.starts_with(path))
            {
                "adapter"
            } else {
                "filesystem"
            };
            operations.push(json!({
                "action": action,
                "kind": kind,
                "path": relative,
            }));
        }
        Ok(operations)
    }

    fn source_roots(&self, project_root: &Path) -> Result<Vec<PathBuf>> {
        if self.loaded.is_some() {
            // Optional cross-service reference repair still needs the original
            // owners, not just this export's scoped private projection.
            let loaded = config::try_load_project(None, Some(project_root))?
                .context("Project configuration disappeared during Studio export")?;
            return config::project_source_roots(&loaded);
        }
        Ok(vec![project_root.join(&self.import_src_dir)])
    }

    fn stage_moved_reference_updates(
        &mut self,
        project_root: &Path,
        settings_candidates: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        let mut updated = BTreeSet::new();
        for current_source_root in self.source_roots(project_root)? {
            let Ok(relative_source_root) = current_source_root.strip_prefix(project_root) else {
                continue;
            };
            let mut before = BTreeMap::new();
            let mut after = BTreeMap::new();
            let document_pairs = settings_candidates
                .par_iter()
                .filter(|relative| relative.starts_with(relative_source_root))
                .map(|relative| -> Result<Option<_>> {
                    let current_path = project_root.join(relative);
                    let staged_path = self.project_root.join(relative);
                    let Some(service) = crate::project::storage::store_service_name(relative)
                    else {
                        return Ok(None);
                    };
                    let (current, staged) = rayon::join(
                        || SettingsBytecode::read_structure_file(&current_path),
                        || SettingsBytecode::read_structure_file(&staged_path),
                    );
                    Ok(Some((service, current?, staged?)))
                })
                .collect::<Result<Vec<_>>>()?;
            for (service, current, staged) in document_pairs.into_iter().flatten() {
                before.insert(service.clone(), current);
                after.insert(service, staged);
            }
            let moved = moved_references_between_documents(&before, &after);
            if moved.is_empty() {
                continue;
            }
            let source_path_segments = moved
                .keys()
                .filter_map(|key| key.split_once('\u{1}').map(|(path, _)| path))
                .map(|path| path.split('\0').map(ToString::to_string).collect())
                .collect::<Vec<Vec<String>>>();
            for (_, current_path) in service_store_paths(&current_source_root)? {
                let relative = current_path.strip_prefix(project_root)?.to_path_buf();
                let staged_path = self.project_root.join(&relative);
                let baseline = (!self.publish_baseline.contains_key(&relative))
                    .then(|| collect_publish_hashes(project_root, std::slice::from_ref(&relative)))
                    .transpose()?;
                let source_path = if staged_path.is_file() {
                    &staged_path
                } else {
                    &current_path
                };
                let Some(mut document) = SettingsBytecode::read_file_if_contains_any_string_set(
                    source_path,
                    &source_path_segments,
                )?
                else {
                    continue;
                };
                let mut changed = false;
                for instance in &mut document.instances {
                    let instance_changed =
                        rewrite_moved_references(&mut instance.properties, &moved)
                            | rewrite_moved_references(&mut instance.attributes, &moved);
                    if instance_changed && instance.class_name == "PackageLink" {
                        bail!(
                            "A PackageLink refers to a moved instance and cannot be edited directly"
                        );
                    }
                    changed |= instance_changed;
                }
                if !changed {
                    continue;
                }
                if let Some(baseline) = baseline {
                    self.publish_baseline.extend(baseline);
                }
                if let Some(parent) = staged_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_bytes_if_changed(&staged_path, &encode_settings_bytecode(&document)?)?;
                updated.insert(relative.clone());
                self.publish_paths.push(relative);
            }
        }
        normalize_owned_paths(&mut self.publish_paths);
        Ok(updated.into_iter().collect())
    }

    pub(crate) fn publish(
        mut self,
        project_root: &Path,
        repair_reference_paths: bool,
    ) -> Result<PublishedProjectChanges> {
        let started = Instant::now();
        let phase = Instant::now();
        let mut current = collect_publish_hashes(project_root, &self.publish_paths)?;
        log_global(
            5,
            format_args!(
                "[renium] export publish current hashes: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        ensure_publish_entries_unchanged(&self.publish_baseline, &current, &self.publish_paths)?;
        let backup_root = self.container.join("previous");
        fs::create_dir_all(&backup_root)
            .with_context(|| format!("Failed to create {}", backup_root.display()))?;
        let phase = Instant::now();
        let mut staged = collect_publish_hashes(&self.project_root, &self.publish_paths)?;
        log_global(
            5,
            format_args!(
                "[renium] export publish staged hashes: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        let settings_candidates = current
            .keys()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_service_settings_file_name)
                    && staged.contains_key(*path)
                    && current.get(*path) != staged.get(*path)
            })
            .cloned()
            .collect::<Vec<_>>();
        let phase = Instant::now();
        for relative in &settings_candidates {
            if self.settings_already_aligned {
                continue;
            }
            let current_path = project_root.join(relative);
            let staged_path = self.project_root.join(relative);
            let current_bytes = fs::read(&current_path)
                .with_context(|| format!("Failed to read {}", current_path.display()))?;
            let staged_bytes = fs::read(&staged_path)
                .with_context(|| format!("Failed to read {}", staged_path.display()))?;
            match align_settings_bytes_to_reference(&current_bytes, &staged_bytes)
                .with_context(|| format!("Failed to align {}", relative.display()))?
            {
                SettingsAlignment::Equivalent => {
                    fs::copy(&current_path, &staged_path).with_context(|| {
                        format!("Failed to preserve equivalent {}", relative.display())
                    })?;
                }
                SettingsAlignment::Changed(aligned) => {
                    write_bytes_if_changed(&staged_path, &aligned)?;
                }
            }
        }
        log_global(
            5,
            format_args!(
                "[renium] export publish settings alignment: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        let phase = Instant::now();
        let repaired = if repair_reference_paths {
            self.stage_moved_reference_updates(project_root, &settings_candidates)?
        } else {
            Vec::new()
        };
        refresh_publish_hashes(project_root, &mut current, &repaired)?;
        log_global(
            5,
            format_args!(
                "[renium] export publish reference repair: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        let phase = Instant::now();
        let mut refreshed = settings_candidates;
        refreshed.extend(repaired);
        refresh_publish_hashes(&self.project_root, &mut staged, &refreshed)?;
        let operation_paths = publish_operation_paths(&current, &staged);
        let directory_swaps = publish_directory_swaps(
            project_root,
            &self.project_root,
            &self.publish_paths,
            &current,
            &staged,
            &operation_paths,
        )?;
        let mut write_paths = operation_paths
            .iter()
            .map(|path| {
                directory_swaps
                    .iter()
                    .find(|directory| path.starts_with(directory))
                    .unwrap_or(path)
                    .clone()
            })
            .collect::<Vec<_>>();
        normalize_owned_paths(&mut write_paths);
        // A directory swap also replaces its directory entries. Recheck that
        // entire footprint, including files added since the original snapshot.
        let concurrency_paths = write_paths
            .iter()
            .filter(|path| path.as_path() != Path::new("sourcemap.json"))
            .cloned()
            .collect::<Vec<_>>();
        let latest = collect_publish_hashes(project_root, &concurrency_paths)?;
        ensure_publish_entries_unchanged(&current, &latest, &concurrency_paths)?;
        log_global(
            5,
            format_args!(
                "[renium] export publish concurrency check: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        let phase = Instant::now();
        let expected = current
            .keys()
            .chain(staged.keys())
            .filter(|path| {
                operation_paths
                    .iter()
                    .any(|root| *path == root || path.starts_with(root))
            })
            .map(|path| (path.clone(), staged.get(path).cloned()))
            .collect();
        let changed_roots = operation_paths.clone();
        log_global(
            5,
            format_args!(
                "[renium] export publish final staging plan: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        let mut published = Vec::<(PathBuf, Option<PathBuf>)>::new();
        let phase = Instant::now();
        let publish_result = (|| -> Result<()> {
            for relative in write_paths {
                let staged = self.project_root.join(&relative);
                let destination = project_root.join(&relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create {}", parent.display()))?;
                }
                let backup = if fs::symlink_metadata(&destination).is_ok() {
                    let backup = backup_root.join(&relative);
                    if let Some(parent) = backup.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::rename(&destination, &backup)
                        .with_context(|| format!("Failed to preserve {}", destination.display()))?;
                    Some(backup)
                } else {
                    None
                };
                published.push((destination.clone(), backup));
                if fs::symlink_metadata(&staged).is_ok() {
                    let result = if directory_swaps.contains(&relative) {
                        // This isolated subtree is consumed exactly once. Moving
                        // it avoids both recursive copying and deleting that copy.
                        fs::rename(&staged, &destination).map_err(anyhow::Error::from)
                    } else {
                        copy_isolated_path(&staged, &destination)
                    };
                    result.with_context(|| {
                        format!("Failed to publish staged path {}", relative.display())
                    })?;
                }
            }
            Ok(())
        })();
        if let Err(error) = publish_result {
            let mut rollback_errors = Vec::new();
            for (destination, backup) in published.into_iter().rev() {
                if let Ok(metadata) = fs::symlink_metadata(&destination) {
                    let remove_result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                        fs::remove_dir_all(&destination)
                    } else {
                        fs::remove_file(&destination)
                    };
                    if let Err(remove_error) = remove_result {
                        rollback_errors.push(format!(
                            "could not remove {}: {remove_error}",
                            destination.display()
                        ));
                        continue;
                    }
                }
                if let Some(backup) = backup
                    && let Err(restore_error) = fs::rename(&backup, &destination)
                {
                    rollback_errors.push(format!(
                        "could not restore {} from {}: {restore_error}",
                        destination.display(),
                        backup.display()
                    ));
                }
            }
            if !rollback_errors.is_empty() {
                self.active = false;
                return Err(error).context(format!(
                    "Export rollback was incomplete; recovery data remains in {}: {}",
                    self.container.display(),
                    rollback_errors.join("; ")
                ));
            }
            return Err(error);
        }
        log_global(
            5,
            format_args!(
                "[renium] export publish file swaps: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        self.active = false;
        let phase = Instant::now();
        published
            .par_iter()
            .filter_map(|(_, backup)| backup.as_ref())
            .for_each(|backup| match fs::symlink_metadata(backup) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    let _ = fs::remove_dir_all(backup);
                }
                Ok(_) => {
                    let _ = fs::remove_file(backup);
                }
                Err(_) => {}
            });
        let _ = fs::remove_dir_all(&self.container);
        log_global(
            5,
            format_args!(
                "[renium] export publish cleanup: {:.1}ms",
                elapsed_ms(phase)
            ),
        );
        log_timing_ms("export project stage publish", elapsed_ms(started));
        Ok(PublishedProjectChanges {
            changed_roots,
            expected,
        })
    }
}

impl Drop for ExportProjectStage {
    fn drop(&mut self) {
        let _trace =
            crate::app::timing::trace_scope("snapshot.cleanup", "release private snapshot stage");
        config::remove_cached_script_naming(&self.project_root);
        if self.active {
            let _ = fs::remove_dir_all(&self.container);
        }
    }
}

fn project_path_is_nested(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".project.json") || name.ends_with(".project.jsonc")
        })
}

fn collect_nested_project_paths(
    project_root: &Path,
    project_path: &Path,
    clone_paths: &mut Vec<PathBuf>,
    publish_paths: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    writable: bool,
) -> Result<()> {
    let project_path = if project_path.is_absolute() {
        project_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(project_path)
    };
    if !visited.insert(project_path.clone()) {
        bail!("Nested project cycle includes {}", project_path.display());
    }
    let loaded = config::load_project(Some(&project_path), None)?;
    let relative = |path: &Path| -> Result<PathBuf> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        Ok(path
            .strip_prefix(project_root)
            .with_context(|| {
                format!(
                    "Nested project path {} is outside {}",
                    path.display(),
                    project_root.display()
                )
            })?
            .to_path_buf())
    };
    clone_paths.push(relative(&loaded.path)?);
    let source_root = loaded.root.join(&loaded.project.source_root);
    clone_paths.push(relative(&source_root)?);
    clone_paths.push(relative(&loaded.root.join("instances"))?);
    if writable {
        publish_paths.push(relative(&source_root)?);
        publish_paths.push(relative(&loaded.root.join("instances"))?);
    }
    for (_, node) in config::project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path {
            let path = loaded.root.join(path);
            clone_paths.push(relative(&path)?);
            if writable {
                publish_paths.push(relative(&path)?);
            }
            if project_path_is_nested(&path) && path.is_file() {
                collect_nested_project_paths(
                    project_root,
                    &path,
                    clone_paths,
                    publish_paths,
                    visited,
                    writable,
                )?;
            }
        }
    }
    for mount in &loaded.project.mounts {
        let path = loaded.root.join(&mount.source);
        clone_paths.push(relative(&path)?);
        let mount_writable = writable && mount.ownership != config::MountOwnership::ReadOnly;
        if mount_writable {
            publish_paths.push(relative(&path)?);
        }
        if project_path_is_nested(&path) && path.is_file() {
            collect_nested_project_paths(
                project_root,
                &path,
                clone_paths,
                publish_paths,
                visited,
                mount_writable,
            )?;
        }
    }
    for adapter in &loaded.project.adapters {
        let source = loaded.root.join(&adapter.source);
        clone_paths.push(relative(&source)?);
        if writable && adapter.direction != config::AdapterDirection::ToProject {
            publish_paths.push(relative(&source)?);
        }
        if project_path_is_nested(&source) && source.is_file() {
            collect_nested_project_paths(
                project_root,
                &source,
                clone_paths,
                publish_paths,
                visited,
                writable && adapter.direction != config::AdapterDirection::ToProject,
            )?;
        }
        if let Some(output) = config::project_adapter_output_path(&loaded, adapter)? {
            clone_paths.push(relative(&output)?);
        }
    }
    let baseline = loaded.root.join(".renium/adapter-baseline.json");
    clone_paths.push(relative(&baseline)?);
    if writable {
        publish_paths.push(relative(&baseline)?);
    }
    visited.remove(&project_path);
    Ok(())
}

#[derive(Default)]
pub(crate) struct PublishedProjectChanges {
    pub(crate) changed_roots: Vec<PathBuf>,
    pub(crate) expected: BTreeMap<PathBuf, Option<PublishEntryState>>,
}

#[derive(Clone, PartialEq)]
pub(crate) enum PublishEntryState {
    Directory,
    File {
        sha256: String,
        length: u64,
        hash: u64,
    },
    Symlink(PathBuf),
}

#[derive(Clone, Copy)]
enum PublishEntryKind {
    Directory,
    File,
    Symlink,
}

pub(crate) fn collect_publish_hashes(
    root: &Path,
    publish_paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, PublishEntryState>> {
    let mut candidates = Vec::new();
    for relative in publish_paths {
        let path = root.join(relative);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            candidates.push((relative.clone(), path, PublishEntryKind::Symlink));
            continue;
        }
        if metadata.is_file() {
            candidates.push((relative.clone(), path, PublishEntryKind::File));
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            let entry_relative = entry.path().strip_prefix(root)?.to_path_buf();
            let kind = if entry.file_type().is_symlink() {
                PublishEntryKind::Symlink
            } else if entry.file_type().is_dir() {
                PublishEntryKind::Directory
            } else if entry.file_type().is_file() {
                PublishEntryKind::File
            } else {
                continue;
            };
            candidates.push((entry_relative, entry.path().to_path_buf(), kind));
        }
    }
    let entries = candidates
        .into_par_iter()
        .map(|(relative, path, kind)| {
            let state = match kind {
                PublishEntryKind::Directory => PublishEntryState::Directory,
                PublishEntryKind::Symlink => PublishEntryState::Symlink(fs::read_link(path)?),
                PublishEntryKind::File => {
                    let bytes = fs::read(path)?;
                    PublishEntryState::File {
                        sha256: sha256_hex(&bytes),
                        length: bytes.len() as u64,
                        hash: fnv1a(&bytes),
                    }
                }
            };
            Ok((relative, state))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(entries.into_iter().collect())
}

fn refresh_publish_hashes(
    root: &Path,
    entries: &mut BTreeMap<PathBuf, PublishEntryState>,
    paths: &[PathBuf],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut paths = paths.to_vec();
    normalize_owned_paths(&mut paths);
    entries.retain(|path, _| !paths.iter().any(|changed| path.starts_with(changed)));
    entries.extend(collect_publish_hashes(root, &paths)?);
    Ok(())
}

fn ensure_publish_entries_unchanged(
    before: &BTreeMap<PathBuf, PublishEntryState>,
    after: &BTreeMap<PathBuf, PublishEntryState>,
    scopes: &[PathBuf],
) -> Result<()> {
    let changed = before
        .keys()
        .chain(after.keys())
        .filter(|path| {
            scopes
                .iter()
                .any(|scope| *path == scope || path.starts_with(scope))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| path.as_path() != Path::new("sourcemap.json"))
        .filter(|path| before.get(*path) != after.get(*path))
        .take(10)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return Ok(());
    }
    bail!(
        "Project files changed while Studio export was running; retry without overwriting: {}",
        changed.join(", ")
    )
}

pub(crate) fn publish_operation_paths(
    current: &BTreeMap<PathBuf, PublishEntryState>,
    staged: &BTreeMap<PathBuf, PublishEntryState>,
) -> Vec<PathBuf> {
    let mut candidates = current
        .keys()
        .chain(staged.keys())
        .filter(|path| current.get(*path) != staged.get(*path))
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| path.components().count());
    let mut operations = Vec::<PathBuf>::new();
    for path in candidates {
        if operations.iter().any(|parent| path.starts_with(parent)) {
            continue;
        }
        operations.push(path);
    }
    operations
}

fn publish_directory_swaps(
    project_root: &Path,
    staged_root: &Path,
    owned_paths: &[PathBuf],
    current: &BTreeMap<PathBuf, PublishEntryState>,
    staged: &BTreeMap<PathBuf, PublishEntryState>,
    operations: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for candidate in owned_paths.iter().chain(operations) {
        if staged.get(candidate) != Some(&PublishEntryState::Directory)
            || current
                .get(candidate)
                .is_some_and(|state| *state != PublishEntryState::Directory)
            || !owned_paths.iter().any(|owner| candidate.starts_with(owner))
            || !operations.iter().any(|path| path.starts_with(candidate))
            || directories
                .iter()
                .any(|parent| candidate.starts_with(parent))
        {
            continue;
        }
        // Do not turn a partial update into replacement of unchanged files,
        // links or unrelated empty directories. Ancestor directories alone may
        // be coalesced; configured owners remain bounded by their exact scopes.
        if current.iter().any(|(path, state)| {
            path.starts_with(candidate)
                && !operations
                    .iter()
                    .any(|operation| path.starts_with(operation))
                && (*state != PublishEntryState::Directory
                    || !operations
                        .iter()
                        .any(|operation| operation.starts_with(path)))
        }) {
            continue;
        }
        if publish_directory_can_move(project_root, staged_root, candidate)? {
            directories.push(candidate.clone());
        }
    }
    normalize_owned_paths(&mut directories);
    Ok(directories)
}

fn publish_directory_can_move(
    project_root: &Path,
    staged_root: &Path,
    relative: &Path,
) -> Result<bool> {
    // The private project is a sibling of the destination. Links, mount points
    // or parent traversal can invalidate that same-filesystem guarantee; leave
    // those paths on the existing copy path instead of retrying a failed move.
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Ok(false);
    }
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(staged_root)?.dev()
    };
    for root in [project_root, staged_root] {
        for ancestor in relative.ancestors() {
            let path = root.join(ancestor);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to inspect {}", path.display()));
                }
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Ok(false);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Ok(false); // FILE_ATTRIBUTE_REPARSE_POINT, including junctions.
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.dev() != device {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn normalize_owned_paths(paths: &mut Vec<PathBuf>) {
    paths.retain(|path| !path.as_os_str().is_empty() && path.is_relative());
    paths.sort_by_key(|path| path.components().count());
    let mut output = Vec::<PathBuf>::new();
    for path in paths.drain(..) {
        if !output.iter().any(|parent| path.starts_with(parent)) {
            output.push(path);
        }
    }
    *paths = output;
}

fn copy_isolated_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Failed to inspect {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        return copy_symbolic_link(source, destination);
    }
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_isolated_file(source, destination)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!("Cannot stage unsupported path {}", source.display());
    }
    for entry in WalkDir::new(source).follow_links(false) {
        let entry = entry.with_context(|| format!("Failed to scan {}", source.display()))?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_symbolic_link(entry.path(), &target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_isolated_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy_isolated_file(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if clone_file(source, destination) {
        return Ok(());
    }
    fs::copy(source, destination)
        .with_context(|| format!("Failed to stage {}", source.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_source) = CString::new(source.as_os_str().as_bytes()) else {
        return false;
    };
    let Ok(c_destination) = CString::new(destination.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: both C strings remain alive for the call and contain complete filesystem paths.
    if unsafe { libc::clonefile(c_source.as_ptr(), c_destination.as_ptr(), 0) } == 0 {
        return true;
    }
    let _ = fs::remove_file(destination);
    false
}

#[cfg(unix)]
fn copy_symbolic_link(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)
        .with_context(|| format!("Failed to stage symbolic link {}", source.display()))
}

#[cfg(windows)]
fn copy_symbolic_link(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)?;
    if fs::metadata(source)?.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
    .with_context(|| format!("Failed to stage symbolic link {}", source.display()))
}

fn export_snapshots_prelude(args: &PullArgs) -> Result<ExportPrelude> {
    set_quiet_timings(args.quiet_timings);

    let total_started = Instant::now();
    let project_root = resolve_existing_project_root(&args.project_root)?;
    config::validate_relative_portable_path(&args.src_dir, "srcDir")?;
    let services = parse_services(&args.services)?;
    println!(
        "[renium] export start: version={}, git={}, build_ts={}, protocol={}, services={}",
        BUILD_VERSION,
        BUILD_GIT_HASH,
        BUILD_TIMESTAMP_UNIX,
        BRIDGE_PROTOCOL_VERSION,
        services.len()
    );
    Ok(ExportPrelude {
        total_started,
        project_root,
        services,
    })
}

pub(crate) fn pull_from_studio(mut args: PullArgs) -> Result<()> {
    apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    let parameters = json!({
        "srcDir": args.src_dir,
        "services": args.services,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
        "exportAllProperties": args.export_all_properties,
        "noExportAllProperties": args.no_export_all_properties,
    });
    let result = daemon_control_request(op::PULL, Some(&args.project_root), parameters, false)?;
    print_json_output(&result, false)
}

pub(crate) fn export_snapshots_with_warm_bridge(
    args: PullArgs,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    bridge_info_refresh_ms: f64,
    repair_reference_paths: bool,
) -> Result<PublishedProjectChanges> {
    let _trace = crate::app::timing::trace_scope("sync", "export snapshots");
    let prelude = export_snapshots_prelude(&args)?;
    println!(
        "[renium] persistent warm bridge: channels={}/{}, cached_bridge_info={}, per_export_handshake_ms={:.1}",
        bridge.channel_count(),
        bridge.expected_channel_count(),
        bridge_info_refresh_ms == 0.0,
        bridge_info_refresh_ms
    );
    export_snapshots_core(
        &args,
        prelude,
        bridge,
        bridge_info,
        bridge_info_refresh_ms,
        repair_reference_paths,
    )
}

/// Capture the ordinary native export, including source and property overlays,
/// without publishing a filesystem projection or reporting sync completion.
pub(crate) fn capture_exported_services<T: Send>(
    args: &PullArgs,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    project_service: impl Fn(&str, ServiceState) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let prelude = export_snapshots_prelude(args)?;
    validate_bridge_info(bridge_info)?;
    prepare_export_bridge(
        args,
        bridge,
        bridge_info,
        &prelude.project_root,
        prelude.total_started,
    )?;
    let outputs = Mutex::new(Vec::with_capacity(prelude.services.len()));
    let trace_context = crate::app::timing::trace_context();
    // Consume each exported service immediately. Waiting for the last export
    // before projecting the first would serialize two otherwise parallel stages.
    let mut guard = rayon::scope(|scope| {
        let _trace_context =
            trace_context.map(|context| crate::app::timing::enter_trace_context(Some(context)));
        editor_binary_export_parts(
            bridge,
            &prelude.services,
            prelude.total_started,
            &mut |output| {
                let outputs = &outputs;
                let project_service = &project_service;
                scope.spawn(move |_| {
                    let _trace_context = trace_context
                        .map(|context| crate::app::timing::enter_trace_context(Some(context)));
                    let _trace =
                        crate::app::timing::trace_scope("sync", "project captured service");
                    let service = output.span.service;
                    let result = exported_parts_to_service_state(&service, output.parts)
                        .and_then(|state| project_service(&service, state));
                    outputs
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(result);
                });
                Ok(())
            },
            &mut || Ok(()),
        )
    })?;
    let projected = outputs
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    finish_native_export(&mut guard)?;
    Ok(projected)
}

fn log_export_bridge_connection(
    total_started: Instant,
    bridge_info_ms: f64,
    bridge_info: &BridgeInfoPayload,
) -> f64 {
    let cli_to_listen_ms = elapsed_ms(total_started);
    log_timing_ms("cli start to bridge listen", cli_to_listen_ms);
    log_timing_ms("all channels connected to bridge info", bridge_info_ms);
    println!(
        "[renium] bridge info: version={}, build_unix={}, protocol={}, codec={}, chunk_frame={}, compact_value={}",
        bridge_info.bridge_version,
        bridge_info.bridge_build_unix,
        bridge_info.protocol_version,
        bridge_info.codec_version,
        bridge_info.chunk_frame_protocol_version,
        bridge_info.compact_value_protocol_version
    );
    cli_to_listen_ms
}

struct ExportBridgeSetup {
    property_schema_ready_ms: f64,
    bridge_info_to_property_schema_ready_ms: f64,
}

fn prepare_export_bridge(
    args: &PullArgs,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    project_root: &Path,
    total_started: Instant,
) -> Result<ExportBridgeSetup> {
    let export_all_properties = args.export_all_properties && !args.no_export_all_properties;
    if export_all_properties {
        println!("[renium] full property export requested; default-value elision disabled");
    }
    if bridge_info.export_all_properties != export_all_properties {
        bridge
            .call(
                "setExportOptions",
                json!({ "exportAllProperties": export_all_properties }),
            )
            .context("Failed to apply plugin export options")?;
        bridge.cache_export_options_for_target(BridgeTarget::Main, export_all_properties);
    }
    let bridge_info_done_ms = elapsed_ms(total_started);
    let property_schema_by_class = load_rbx_dom_property_schema(project_root)?.unwrap_or_default();
    if !property_schema_by_class.is_empty() {
        println!("[renium] configuring plugin property candidates from rbx-dom schema");
        configure_bridge_property_candidates(bridge, &property_schema_by_class)
            .context("Failed to configure plugin property candidates")?;
    }
    let property_schema_ready_ms = elapsed_ms(total_started);
    let bridge_info_to_property_schema_ready_ms =
        (property_schema_ready_ms - bridge_info_done_ms).max(0.0);
    log_timing_ms(
        "bridge info to property schema ready",
        bridge_info_to_property_schema_ready_ms,
    );
    Ok(ExportBridgeSetup {
        property_schema_ready_ms,
        bridge_info_to_property_schema_ready_ms,
    })
}

struct ServiceExportRun<'a> {
    spans: Vec<ServiceExecutionSpan>,
    cumulative_latency_ms: f64,
    native_finish_guard: Option<EditorBinaryExportFinishGuard<'a>>,
}

fn run_service_exports<'a>(
    bridge: &'a BridgeServer,
    run_started: Instant,
    services: &[String],
    dispatcher: &DirectImportDispatcher,
) -> Result<ServiceExportRun<'a>> {
    let mut run = ServiceExportRun {
        spans: Vec::with_capacity(services.len()),
        cumulative_latency_ms: 0.0,
        native_finish_guard: None,
    };
    let guard = {
        let mut finish_output = |output| {
            finish_service_export_output(
                output,
                dispatcher,
                &mut run.spans,
                &mut run.cumulative_latency_ms,
            )
        };
        let mut release_import = || {
            dispatcher.activate_workers(1);
            Ok(())
        };
        editor_binary_export_parts(
            bridge,
            services,
            run_started,
            &mut finish_output,
            &mut release_import,
        )?
    };
    run.native_finish_guard = Some(guard);
    run.spans.sort_by(|a, b| {
        a.export_start_ms
            .partial_cmp(&b.export_start_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(run)
}

#[derive(Default)]
struct ImportFinishMetrics {
    dispatcher_drain_ms: f64,
    sourcemap_finalize_ms: f64,
}

fn finish_export_import(
    dispatcher: DirectImportDispatcher,
    sourcemap_writer: SourcemapWriter,
) -> Result<ImportFinishMetrics> {
    let mut metrics = ImportFinishMetrics::default();
    let drain_started = Instant::now();
    dispatcher.finish()?;
    metrics.dispatcher_drain_ms = elapsed_ms(drain_started);
    log_timing_ms(
        "direct import dispatcher drain",
        metrics.dispatcher_drain_ms,
    );
    sourcemap_writer.request_finish();
    let sourcemap_started = Instant::now();
    sourcemap_writer.join()?;
    metrics.sourcemap_finalize_ms = elapsed_ms(sourcemap_started);
    log_timing_ms("sourcemap finalize", metrics.sourcemap_finalize_ms);
    Ok(metrics)
}

struct ExportExecutionSetup {
    project_stage: ExportProjectStage,
    sourcemap_writer: SourcemapWriter,
    direct_import_dispatcher: DirectImportDispatcher,
    export_services: Vec<String>,
}

fn prepare_export_execution(
    args: &PullArgs,
    project_root: &Path,
    services: &[String],
    total_started: Instant,
) -> Result<ExportExecutionSetup> {
    let project_stage = ExportProjectStage::create(project_root, &args.src_dir, services)?;
    let import_project_root = project_stage.import_project_root.clone();
    let import_src_dir = project_stage.import_src_dir.clone();
    log_global(
        5,
        format_args!(
            "[renium] export stage roots: project={} stage={} import={} src={}",
            project_root.display(),
            project_stage.project_root.display(),
            import_project_root.display(),
            import_src_dir.display()
        ),
    );
    let sourcemap_writer = SourcemapWriter::start(import_project_root.clone(), false);
    let workers = resolve_direct_import_workers();
    println!("[renium] direct import workers during export: {workers}");
    let direct_import_dispatcher = DirectImportDispatcher::start(
        import_project_root.clone(),
        import_src_dir,
        workers,
        workers,
        Some(sourcemap_writer.sender()),
        total_started,
    )?;
    let export_services = direct_import_export_order(services);
    if export_services != services {
        println!(
            "[renium] direct import export order: {}",
            export_services.join(",")
        );
    }
    Ok(ExportExecutionSetup {
        project_stage,
        sourcemap_writer,
        direct_import_dispatcher,
        export_services,
    })
}

fn finish_native_export(guard: &mut EditorBinaryExportFinishGuard<'_>) -> Result<()> {
    let result = guard.finish(false);
    // A reported mutation already expires the plugin session. Do not let Drop
    // retry finalization (or mask its first error); an undelivered request still
    // has the existing plugin lease/expiry cleanup.
    guard.export_id = None;
    result.map(|_| ())
}

fn finish_export_publication(
    mut stage: Option<ExportProjectStage>,
    project_root: &Path,
    repair_reference_paths: bool,
    finalize_native: impl FnOnce() -> Result<()>,
    record_completion: impl FnOnce() -> Result<()>,
) -> Result<(PublishedProjectChanges, f64)> {
    if let Some(stage) = stage.as_mut() {
        stage.mark_settings_aligned();
        let started = Instant::now();
        stage.finish_projection(false)?;
        log_global(
            5,
            format_args!(
                "[renium] export project stage projection: {:.1}ms",
                elapsed_ms(started)
            ),
        );
    }
    let started = Instant::now();
    finalize_native()?;
    let mut sync_completion_ms = elapsed_ms(started);
    let published = if let Some(stage) = stage {
        let started = Instant::now();
        let published = stage.publish(project_root, repair_reference_paths)?;
        log_global(
            5,
            format_args!(
                "[renium] export project stage publish: {:.1}ms",
                elapsed_ms(started)
            ),
        );
        published
    } else {
        PublishedProjectChanges::default()
    };
    let started = Instant::now();
    record_completion()?;
    sync_completion_ms += elapsed_ms(started);
    Ok((published, sync_completion_ms))
}

fn export_snapshots_core(
    args: &PullArgs,
    prelude: ExportPrelude,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    all_channels_connected_to_bridge_info_ms: f64,
    repair_reference_paths: bool,
) -> Result<PublishedProjectChanges> {
    let _trace = crate::app::timing::trace_scope("sync", "export core");
    let mut stages = crate::app::timing::trace_stages(
        "export.core",
        "initialize export metrics and service selection",
    );
    let ExportPrelude {
        total_started,
        project_root,
        services,
        ..
    } = prelude;
    let cli_start_to_bridge_listen_ms = log_export_bridge_connection(
        total_started,
        all_channels_connected_to_bridge_info_ms,
        bridge_info,
    );
    stages.next("prepare export bridge and property schema");
    let ExportBridgeSetup {
        property_schema_ready_ms,
        bridge_info_to_property_schema_ready_ms,
    } = prepare_export_bridge(args, bridge, bridge_info, &project_root, total_started)?;
    stages.next("prepare project output workers and export service order");
    let ExportExecutionSetup {
        project_stage,
        sourcemap_writer,
        direct_import_dispatcher,
        export_services,
    } = prepare_export_execution(args, &project_root, &services, total_started)?;
    stages.next("capture services and stream snapshots into output workers");
    let ServiceExportRun {
        spans: service_export_spans,
        cumulative_latency_ms: cumulative_service_latency_ms,
        mut native_finish_guard,
    } = run_service_exports(
        bridge,
        total_started,
        &export_services,
        &direct_import_dispatcher,
    )?;
    stages.next("export timing boundaries");
    let first_service_export_ms = service_export_spans
        .first()
        .map_or(property_schema_ready_ms, |span| span.export_start_ms);
    let last_service_export_ms = service_export_spans
        .last()
        .map_or(property_schema_ready_ms, |span| span.export_end_ms);
    let property_schema_ready_to_first_service_export_ms =
        (first_service_export_ms - property_schema_ready_ms).max(0.0);
    let first_service_export_to_last_service_export_ms =
        (last_service_export_ms - first_service_export_ms).max(0.0);
    log_timing_ms(
        "property schema ready to first service export",
        property_schema_ready_to_first_service_export_ms,
    );
    log_timing_ms(
        "first service export to last service export",
        first_service_export_to_last_service_export_ms,
    );

    let dispatcher_drain_start_ms = elapsed_ms(total_started);
    let last_service_export_to_dispatcher_drain_start_ms =
        (dispatcher_drain_start_ms - last_service_export_ms).max(0.0);
    log_timing_ms(
        "last service export to dispatcher drain start",
        last_service_export_to_dispatcher_drain_start_ms,
    );
    stages.next("join output workers and finalize sourcemap");
    let ImportFinishMetrics {
        dispatcher_drain_ms,
        sourcemap_finalize_ms,
    } = finish_export_import(direct_import_dispatcher, sourcemap_writer)?;
    stages.next("publish exported files and acknowledge Studio snapshot");
    let (published, sync_completion_ms) = finish_export_publication(
        Some(project_stage),
        &project_root,
        repair_reference_paths,
        move || {
            if let Some(mut guard) = native_finish_guard.take() {
                finish_native_export(&mut guard)?;
            }
            Ok(())
        },
        || record_bridge_sync_completion(bridge),
    )?;

    stages.next("format export timing summaries");
    let total_run_ms = elapsed_ms(total_started);
    let handshake_ms = all_channels_connected_to_bridge_info_ms;
    let core_export_ms = property_schema_ready_to_first_service_export_ms
        + first_service_export_to_last_service_export_ms;
    let import_critical_tail_ms = last_service_export_to_dispatcher_drain_start_ms
        + dispatcher_drain_ms
        + sourcemap_finalize_ms
        + sync_completion_ms;
    let unmeasured_or_scheduler_gap_ms = (total_run_ms
        - cli_start_to_bridge_listen_ms
        - handshake_ms
        - bridge_info_to_property_schema_ready_ms
        - core_export_ms
        - import_critical_tail_ms)
        .max(0.0);
    if verbose_timing_logs() {
        for span in &service_export_spans {
            println!(
                "[renium] service export span: service={}, start_ms={:.1}, end_ms={:.1}, duration_ms={:.1}",
                span.service,
                span.export_start_ms,
                span.export_end_ms,
                span.export_end_ms - span.export_start_ms
            );
        }
    }
    println!(
        "[renium] run timing spans: cli_start_to_bridge_listen_ms={cli_start_to_bridge_listen_ms:.1}, all_channels_connected_to_bridge_info_ms={all_channels_connected_to_bridge_info_ms:.1}, bridge_info_to_property_schema_ready_ms={bridge_info_to_property_schema_ready_ms:.1}, property_schema_ready_to_first_service_export_ms={property_schema_ready_to_first_service_export_ms:.1}, first_service_export_to_last_service_export_ms={first_service_export_to_last_service_export_ms:.1}, cumulative_service_latency_ms={cumulative_service_latency_ms:.1}, last_service_export_to_dispatcher_drain_start_ms={last_service_export_to_dispatcher_drain_start_ms:.1}, dispatcher_drain_ms={dispatcher_drain_ms:.1}, sourcemap_finalize_ms={sourcemap_finalize_ms:.1}, sync_completion_ms={sync_completion_ms:.1}, total_run_ms={total_run_ms:.1}"
    );
    println!(
        "[renium] run timing summary: total_ms={total_run_ms:.1}, core_export_ms={core_export_ms:.1}, bridge_startup_ms={cli_start_to_bridge_listen_ms:.1}, handshake_ms={handshake_ms:.1}, cumulative_service_latency_ms={cumulative_service_latency_ms:.1}, import_critical_tail_ms={import_critical_tail_ms:.1}, unmeasured_or_scheduler_gap_ms={unmeasured_or_scheduler_gap_ms:.1}"
    );
    log_timing_ms("full export-snapshots run", total_run_ms);
    println!("[renium] export done");
    stages.next("release completed export buffers and configuration");
    Ok(published)
}

pub(crate) fn parse_bridge_ports(raw: &str) -> Result<Vec<u16>> {
    if raw.trim().eq_ignore_ascii_case("auto") {
        bail!(
            "Automatic bridge ports cannot be coordinated with the Studio plugin; configure the same two ports in Renium and Studio"
        );
    }
    let mut out = Vec::new();
    for token in raw.split(',') {
        let text = token.trim();
        if text.is_empty() {
            continue;
        }
        let value: u16 = text
            .parse()
            .with_context(|| format!("Invalid bridge port: {text}"))?;
        if value == 0 {
            bail!("Invalid bridge port: {text}");
        }
        if !out.contains(&value) {
            out.push(value);
        }
    }
    if out.len() != 2 {
        bail!(
            "Exactly 2 distinct bridge ports are required; got {} in {:?}",
            out.len(),
            out
        );
    }
    Ok(out)
}

pub(crate) fn exported_parts_to_service_state(
    service: &str,
    parts: ExportedSnapshotParts,
) -> Result<ServiceState> {
    let native_properties_by_instance = parts.native_properties_by_instance;
    let class_defaults_by_class = normalize_class_defaults(parts.class_defaults);
    let mut state = build_service_state_from_instances(
        service,
        None,
        parts.instances,
        class_defaults_by_class,
        true,
    )?;
    if let Some(native_properties) = native_properties_by_instance {
        if native_properties.len() != state.instances.len() {
            bail!(
                "Native settings values contain {} {service} instances; expected {}",
                native_properties.len(),
                state.instances.len()
            );
        }
        state.native_properties_by_instance = Some(native_properties);
    }
    Ok(state)
}

pub(crate) fn parse_bridge_chunk(value: Value) -> Result<BridgeChunk> {
    let chunk: BridgeChunk =
        serde_json::from_value(value).context("Invalid bridge chunk payload")?;
    validate_bridge_chunk(&chunk)?;
    Ok(chunk)
}

pub(crate) fn validate_bridge_chunk(chunk: &BridgeChunk) -> Result<()> {
    if chunk.chunk.len() > MAX_BRIDGE_CHUNK_BYTES {
        bail!(
            "Bridge chunk exceeds safe size limit ({} bytes; maximum is {MAX_BRIDGE_CHUNK_BYTES})",
            chunk.chunk.len()
        );
    }
    if chunk.total > MAX_BRIDGE_REASSEMBLY_BYTES {
        bail!(
            "Bridge payload advertises {} bytes, above the safe {MAX_BRIDGE_REASSEMBLY_BYTES}-byte limit",
            chunk.total
        );
    }
    if let Some(payload_hash) = &chunk.payload_hash
        && (payload_hash.is_empty()
            || payload_hash.len() > 128
            || payload_hash.chars().any(char::is_whitespace))
    {
        bail!("Bridge chunk has an invalid payload hash");
    }
    if chunk.payload_cache_hit
        && (chunk.payload_hash.is_none()
            || !chunk.chunk.is_empty()
            || chunk.start != 1
            || chunk.next_start != 1
            || chunk.total == 0)
    {
        bail!("Bridge chunk has an invalid payload cache hit");
    }
    match chunk.compression.as_deref() {
        Some("zstd-base64-v1") => {
            let uncompressed_bytes = chunk
                .uncompressed_bytes
                .context("Compressed bridge chunk omitted its uncompressed size")?;
            if uncompressed_bytes == 0 || uncompressed_bytes > MAX_BRIDGE_REASSEMBLY_BYTES {
                bail!("Compressed bridge chunk has an invalid uncompressed size");
            }
            if chunk.payload_cache_hit {
                bail!("Compressed bridge chunk cannot be a payload cache hit");
            }
        }
        Some(_) => bail!("Bridge chunk uses an unsupported compression format"),
        None if chunk.uncompressed_bytes.is_some() => {
            bail!("Uncompressed bridge chunk reports an uncompressed size")
        }
        None => {}
    }
    if chunk.total == 0 {
        if !chunk.chunk.is_empty() {
            bail!("Bridge chunk has payload bytes but reports a zero total");
        }
        return Ok(());
    }
    if chunk.start == 0 {
        bail!("Bridge chunk is missing a positive start cursor");
    }
    if chunk.start > chunk.total.saturating_add(1) {
        bail!(
            "Bridge chunk start {} is outside its total {}",
            chunk.start,
            chunk.total
        );
    }
    if chunk.next_start < chunk.start.max(1) || chunk.next_start > chunk.total.saturating_add(1) {
        bail!(
            "Bridge chunk cursor {} is invalid for start {} and total {}",
            chunk.next_start,
            chunk.start,
            chunk.total
        );
    }
    if !chunk.chunk.is_empty() && chunk.next_start <= chunk.start {
        bail!(
            "Bridge chunk has payload bytes but does not advance its cursor (start {}, next {})",
            chunk.start,
            chunk.next_start
        );
    }
    Ok(())
}

pub(crate) fn merge_chunk_fetch_metrics(
    target: &mut ChunkFetchMetrics,
    partial: ChunkFetchMetrics,
) {
    target.bytes = target.bytes.saturating_add(partial.bytes);
    target.chunks = target.chunks.saturating_add(partial.chunks);
    target.max_chunk_bytes = target.max_chunk_bytes.max(partial.max_chunk_bytes);
    target.plugin_server_ms += partial.plugin_server_ms;
    target.plugin_encode_ms += partial.plugin_encode_ms;
    target.reassembly_ms += partial.reassembly_ms;
    target.json_parse_ms += partial.json_parse_ms;
}

pub(crate) fn log_chunk_fetch_metrics(label: &str, metrics: ChunkFetchMetrics) {
    if metrics.chunks == 0 || (quiet_timings() && !crate::app::output::global_log_enabled(5)) {
        return;
    }
    let message = format!(
        "[renium] timing: {label} chunk metrics -> chunks={}, bytes={}, max_chunk_bytes={}, plugin_server_ms={:.1}, plugin_encode_ms={:.1}, reassembly_ms={:.1}, json_parse_ms={:.1}",
        metrics.chunks,
        metrics.bytes,
        metrics.max_chunk_bytes,
        metrics.plugin_server_ms,
        metrics.plugin_encode_ms,
        metrics.reassembly_ms,
        metrics.json_parse_ms
    );
    if quiet_timings() {
        log_global(5, format_args!("{message}"));
    } else {
        println!("{message}");
    }
}

pub(crate) fn fetch_text_chunks<F>(
    chunk_size: usize,
    mut fetcher: F,
) -> Result<(String, ChunkFetchMetrics)>
where
    F: FnMut(usize, usize) -> Result<BridgeChunk>,
{
    let reassembly_started = Instant::now();
    let mut metrics = ChunkFetchMetrics::default();
    let max_len = chunk_size.max(256);
    let first = fetcher(1, max_len)?;
    validate_bridge_chunk(&first)?;
    if first.start > 0 && first.start != 1 {
        bail!(
            "Plugin returned an initial chunk starting at {}, expected 1",
            first.start
        );
    }
    metrics.max_chunk_bytes = first.chunk.len();
    metrics.bytes = first.chunk.len();
    metrics.chunks = 1;
    metrics.plugin_server_ms += first.plugin_server_ms.unwrap_or(0.0);
    metrics.plugin_encode_ms += first.plugin_encode_ms.unwrap_or(0.0);
    let compression = first.compression.clone();
    let uncompressed_bytes = first.uncompressed_bytes;
    if first.total > 0 && first.next_start <= 1 {
        bail!(
            "Plugin returned a non-advancing initial payload chunk (next={}, total={})",
            first.next_start,
            first.total
        );
    }
    let first_done = first.total == 0 || first.next_start > first.total;
    let mut output = first.chunk;
    if first_done {
        output = decompress_bridge_text(output, compression.as_deref(), uncompressed_bytes)?;
        metrics.reassembly_ms = elapsed_ms(reassembly_started);
        return Ok((output, metrics));
    }
    if output.len() > first.total && first.total > 0 {
        bail!(
            "Plugin returned {} bytes for a payload declared as {} bytes",
            output.len(),
            first.total
        );
    }
    if first.total > output.len() {
        output
            .try_reserve_exact(first.total - output.len())
            .context("Failed to reserve memory for bridge payload")?;
    }
    let mut start = first.next_start;
    let total = first.total;
    let max_chunks = total.div_ceil(256).saturating_add(2);
    loop {
        if metrics.chunks >= max_chunks {
            bail!("Plugin returned too many chunks for a {total}-byte payload");
        }
        let chunk = fetcher(start, max_len)?;
        validate_bridge_chunk(&chunk)?;
        if chunk.total != total {
            bail!(
                "Plugin changed payload total between chunks (expected {total}, got {})",
                chunk.total
            );
        }
        if chunk.compression != compression || chunk.uncompressed_bytes != uncompressed_bytes {
            bail!("Plugin changed payload compression between chunks");
        }
        if chunk.start > 0 && chunk.start != start {
            bail!(
                "Plugin returned chunk start {} while {} was requested",
                chunk.start,
                start
            );
        }
        if output.len().saturating_add(chunk.chunk.len()) > total {
            bail!("Plugin returned more bytes than its declared payload total");
        }
        metrics.max_chunk_bytes = metrics.max_chunk_bytes.max(chunk.chunk.len());
        output.push_str(&chunk.chunk);
        metrics.bytes = metrics.bytes.saturating_add(chunk.chunk.len());
        metrics.chunks = metrics.chunks.saturating_add(1);
        metrics.plugin_server_ms += chunk.plugin_server_ms.unwrap_or(0.0);
        metrics.plugin_encode_ms += chunk.plugin_encode_ms.unwrap_or(0.0);

        if chunk.total == 0 || chunk.next_start > chunk.total {
            break;
        }
        if chunk.next_start <= start {
            bail!(
                "Plugin returned a non-advancing payload chunk (start={start}, next={}, total={})",
                chunk.next_start,
                chunk.total
            );
        }
        start = chunk.next_start;
    }
    if total > 0 && output.len() != total {
        bail!(
            "Plugin payload ended at {} bytes but declared {total} bytes",
            output.len()
        );
    }
    output = decompress_bridge_text(output, compression.as_deref(), uncompressed_bytes)?;
    metrics.reassembly_ms = elapsed_ms(reassembly_started);
    Ok((output, metrics))
}

const BRIDGE_TEXT_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const BRIDGE_TEXT_CACHE_MAX_ENTRIES: usize = 64;

#[derive(Default)]
struct BridgeTextPayloadCache {
    entries: VecDeque<(String, Arc<str>)>,
    last_hash_by_slot: HashMap<String, String>,
    total_bytes: usize,
}

fn bridge_text_payload_cache() -> &'static Mutex<BridgeTextPayloadCache> {
    static CACHE: OnceLock<Mutex<BridgeTextPayloadCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BridgeTextPayloadCache::default()))
}

fn bridge_text_payload_known(slot: &str) -> Option<(String, Arc<str>)> {
    let cache = bridge_text_payload_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let hash = cache.last_hash_by_slot.get(slot)?;
    cache.entries.iter().find_map(|(entry_hash, text)| {
        (entry_hash == hash).then(|| (hash.clone(), Arc::clone(text)))
    })
}

fn bridge_text_payload_insert(slot: &str, hash: String, text: &str) {
    if text.len() > BRIDGE_TEXT_CACHE_MAX_BYTES {
        return;
    }
    let mut cache = bridge_text_payload_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache
        .last_hash_by_slot
        .insert(slot.to_string(), hash.clone());
    if cache
        .entries
        .iter()
        .any(|(entry_hash, _)| entry_hash == &hash)
    {
        return;
    }
    while cache.entries.len() >= BRIDGE_TEXT_CACHE_MAX_ENTRIES
        || cache.total_bytes.saturating_add(text.len()) > BRIDGE_TEXT_CACHE_MAX_BYTES
    {
        let Some((_, removed)) = cache.entries.pop_front() else {
            break;
        };
        cache.total_bytes = cache.total_bytes.saturating_sub(removed.len());
    }
    let text: Arc<str> = Arc::from(text);
    cache.total_bytes = cache.total_bytes.saturating_add(text.len());
    cache.entries.push_back((hash, text));
}

pub(crate) fn fetch_text_chunks_with_cache<F>(
    chunk_size: usize,
    cache_slot: &str,
    mut fetcher: F,
) -> Result<(String, ChunkFetchMetrics)>
where
    F: FnMut(usize, usize, Option<&str>) -> Result<BridgeChunk>,
{
    // Keep advertised bytes alive across the request. Other service exports can
    // evict the cache entry before Studio returns its hash-only response.
    let known_payload = bridge_text_payload_known(cache_slot);
    let known_hash = known_payload.as_ref().map(|(hash, _)| hash.as_str());
    let mut first = true;
    let mut payload_hash = None;
    let (text, metrics) = fetch_text_chunks(chunk_size, |start, max_len| {
        let chunk = fetcher(start, max_len, first.then_some(known_hash).flatten())?;
        if first {
            first = false;
            payload_hash.clone_from(&chunk.payload_hash);
            if chunk.payload_cache_hit {
                let hash = chunk
                    .payload_hash
                    .as_deref()
                    .context("Bridge text cache hit omitted its payload hash")?;
                let Some((known_hash, text)) = &known_payload else {
                    bail!("Bridge returned an unexpected text payload cache hit");
                };
                if known_hash != hash {
                    bail!("Bridge returned an unexpected text payload cache hit");
                }
                if text.len() != chunk.total {
                    bail!("Bridge text payload cache entry has the wrong size");
                }
                return Ok(BridgeChunk {
                    start: 1,
                    next_start: text.len() + 1,
                    total: text.len(),
                    chunk: text.to_string(),
                    plugin_server_ms: chunk.plugin_server_ms,
                    plugin_encode_ms: chunk.plugin_encode_ms,
                    serialization_complete: chunk.serialization_complete,
                    payload_hash: chunk.payload_hash,
                    payload_cache_hit: false,
                    compression: None,
                    uncompressed_bytes: None,
                });
            }
        } else if chunk.payload_hash != payload_hash {
            bail!("Plugin changed payload hash between chunks");
        }
        Ok(chunk)
    })?;
    if let Some(hash) = payload_hash {
        bridge_text_payload_insert(cache_slot, hash, &text);
    }
    Ok((text, metrics))
}

fn decompress_bridge_text(
    text: String,
    compression: Option<&str>,
    uncompressed_bytes: Option<usize>,
) -> Result<String> {
    if compression.is_none() {
        return Ok(text);
    }
    let expected_len = uncompressed_bytes.context("Compressed bridge payload omitted its size")?;
    let compressed =
        base64::decode(text.as_bytes()).context("Compressed bridge payload is not valid base64")?;
    let decoded = zstd::bulk::decompress(&compressed, expected_len)
        .context("Compressed bridge payload has invalid zstd data")?;
    if decoded.len() != expected_len {
        bail!(
            "Compressed bridge payload has {} bytes; expected {expected_len}",
            decoded.len()
        );
    }
    String::from_utf8(decoded).context("Compressed bridge payload is not valid UTF-8")
}

pub(crate) fn fetch_json_payload<F>(
    chunk_size: usize,
    mut fetcher: F,
) -> Result<(Value, ChunkFetchMetrics)>
where
    F: FnMut(usize, usize) -> Result<BridgeChunk>,
{
    fetch_typed_payload_with_size(chunk_size, &mut fetcher)
}

pub(crate) fn fetch_typed_payload_with_size<T, F>(
    chunk_size: usize,
    fetcher: F,
) -> Result<(T, ChunkFetchMetrics)>
where
    T: DeserializeOwned,
    F: FnMut(usize, usize) -> Result<BridgeChunk>,
{
    let (text, mut metrics) = fetch_text_chunks(chunk_size, fetcher)?;
    metrics.bytes = text.len();
    let parse_started = Instant::now();
    let value = serde_json::from_slice(text.as_bytes()).context("Invalid chunked JSON payload")?;
    metrics.json_parse_ms = elapsed_ms(parse_started);
    Ok((value, metrics))
}

pub(crate) fn fetch_typed_payload_with_size_cached<T, F>(
    chunk_size: usize,
    cache_slot: &str,
    fetcher: F,
) -> Result<(T, ChunkFetchMetrics)>
where
    T: DeserializeOwned,
    F: FnMut(usize, usize, Option<&str>) -> Result<BridgeChunk>,
{
    let (text, mut metrics) = fetch_text_chunks_with_cache(chunk_size, cache_slot, fetcher)?;
    metrics.bytes = text.len();
    let parse_started = Instant::now();
    let value = serde_json::from_slice(text.as_bytes()).context("Invalid chunked JSON payload")?;
    metrics.json_parse_ms = elapsed_ms(parse_started);
    Ok((value, metrics))
}

fn collect_configured_export_paths(
    loaded: &config::LoadedProject,
    project_root: &Path,
    services: &[String],
    clone_project_data: bool,
    clone_paths: &mut Vec<PathBuf>,
    publish_paths: &mut Vec<PathBuf>,
) -> Result<()> {
    let project_file = loaded.path.strip_prefix(project_root)?.to_path_buf();
    clone_paths.push(project_file);
    let adapter_baseline = PathBuf::from(".renium").join("adapter-baseline.json");
    if clone_project_data {
        clone_paths.push(adapter_baseline.clone());
    }
    publish_paths.push(adapter_baseline);
    let source_root = loaded.project.source_root.clone();
    for service in services {
        let path = source_root.join(sanitize_name(service));
        let store = PathBuf::from("instances").join(format!("{}.renium", sanitize_name(service)));
        let mapped_stores = PathBuf::from("instances").join(sanitize_name(service));
        if clone_project_data {
            clone_paths.push(path.clone());
            clone_paths.push(store.clone());
            clone_paths.push(mapped_stores.clone());
        }
        publish_paths.push(path);
        publish_paths.push(store);
        publish_paths.push(mapped_stores);
    }
    let mut nested_projects = HashSet::new();
    for (_, node) in config::project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path {
            if clone_project_data {
                clone_paths.push(path.clone());
            }
            publish_paths.push(path.clone());
            let source = loaded.root.join(&path);
            if clone_project_data && project_path_is_nested(&source) && source.is_file() {
                collect_nested_project_paths(
                    project_root,
                    &source,
                    clone_paths,
                    publish_paths,
                    &mut nested_projects,
                    true,
                )?;
            }
        }
    }
    for mount in &loaded.project.mounts {
        clone_paths.push(mount.source.clone());
        if mount.ownership != config::MountOwnership::ReadOnly {
            publish_paths.push(mount.source.clone());
        }
        let source = loaded.root.join(&mount.source);
        if project_path_is_nested(&source) && source.is_file() {
            collect_nested_project_paths(
                project_root,
                &source,
                clone_paths,
                publish_paths,
                &mut nested_projects,
                mount.ownership != config::MountOwnership::ReadOnly,
            )?;
        }
    }
    for adapter in &loaded.project.adapters {
        clone_paths.push(adapter.source.clone());
        if adapter.direction != config::AdapterDirection::ToProject {
            publish_paths.push(adapter.source.clone());
        }
        let source = loaded.root.join(&adapter.source);
        if project_path_is_nested(&source) && source.is_file() {
            collect_nested_project_paths(
                project_root,
                &source,
                clone_paths,
                publish_paths,
                &mut nested_projects,
                adapter.direction != config::AdapterDirection::ToProject,
            )?;
        }
        if let Some(output) = config::project_adapter_output_path(loaded, adapter)? {
            clone_paths.push(output.strip_prefix(project_root)?.to_path_buf());
        }
    }
    Ok(())
}

fn scope_export_project(loaded: &mut config::LoadedProject, services: &[String]) {
    // The private projection must have the same service scope as the
    // capture, including configured owners outside the ordinary src tree.
    loaded
        .project
        .tree
        .retain(|service, _| services.contains(service));
    loaded.project.mounts.retain(|mount| {
        mount
            .target
            .segments()
            .first()
            .is_none_or(|service| services.contains(service))
    });
    loaded.project.adapters.retain(|adapter| {
        adapter
            .target
            .segments()
            .first()
            .is_none_or(|service| services.contains(service))
    });
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use clap::Parser;
    use std::cell::Cell;

    const SOURCE: &str = "src/ReplicatedStorage/Mod.luau";
    const ORIGINAL_MAP: &str = r#"{"name":"Fixture","className":"DataModel","children":[]}"#;

    struct Fixture {
        container: PathBuf,
        root: PathBuf,
        stage: Option<ExportProjectStage>,
    }

    impl Fixture {
        fn new() -> Self {
            let container = create_unique_directory(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
                "export-publication-test-",
            )
            .unwrap();
            let root = container.join("project");
            fs::create_dir_all(root.join("src/ReplicatedStorage")).unwrap();
            fs::write(
                root.join("renium.project.jsonc"),
                serde_json::to_vec(&config::ReniumProject::default()).unwrap(),
            )
            .unwrap();
            fs::write(root.join(SOURCE), "return 'original'\n").unwrap();
            fs::write(root.join("sourcemap.json"), ORIGINAL_MAP).unwrap();
            let stage =
                ExportProjectStage::create(&root, Path::new("src"), &["ReplicatedStorage".into()])
                    .unwrap();
            fs::write(stage.project_root.join(SOURCE), "return 'captured'\n").unwrap();
            fs::write(
                stage.project_root.join("sourcemap.json"),
                "{\"name\":\"Captured\"}",
            )
            .unwrap();
            Self {
                container,
                root,
                stage: Some(stage),
            }
        }

        fn planned_directory_swaps(&self, stage: &ExportProjectStage) -> Vec<PathBuf> {
            let current = collect_publish_hashes(&self.root, &stage.publish_paths).unwrap();
            let staged = collect_publish_hashes(&stage.project_root, &stage.publish_paths).unwrap();
            publish_directory_swaps(
                &self.root,
                &stage.project_root,
                &stage.publish_paths,
                &current,
                &staged,
                &publish_operation_paths(&current, &staged),
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(self.stage.take());
            let _ = fs::remove_dir_all(&self.container);
        }
    }

    #[test]
    fn export_publication_moves_new_service_directories_without_copying() {
        for initially_present in [false, true] {
            let mut fixture = Fixture::new();
            drop(fixture.stage.take());
            fs::remove_file(fixture.root.join(SOURCE)).unwrap();
            let service = PathBuf::from("src/ReplicatedStorage");
            if !initially_present {
                fs::remove_dir(fixture.root.join(&service)).unwrap();
            }
            let stage = ExportProjectStage::create(
                &fixture.root,
                Path::new("src"),
                &["ReplicatedStorage".into()],
            )
            .unwrap();
            fs::create_dir_all(stage.project_root.join(&service).join("Nested/Empty")).unwrap();
            fs::write(stage.project_root.join(SOURCE), "return 'captured'\n").unwrap();
            fs::write(
                stage.project_root.join(&service).join("Nested/Child.luau"),
                "return 'child'\n",
            )
            .unwrap();
            assert_eq!(fixture.planned_directory_swaps(&stage), [service]);
            let current = collect_publish_hashes(&fixture.root, &stage.publish_paths).unwrap();
            let staged = collect_publish_hashes(&stage.project_root, &stage.publish_paths).unwrap();
            let logical_paths = publish_operation_paths(&current, &staged);
            let expected = current
                .keys()
                .chain(staged.keys())
                .filter(|path| logical_paths.iter().any(|root| path.starts_with(root)))
                .map(|path| (path.clone(), staged.get(path).cloned()))
                .collect::<BTreeMap<_, _>>();
            // An independent link proves the staged file itself was moved,
            // rather than merely verifying equivalent copied bytes.
            let staged_file_link = fixture.container.join("staged-file-link");
            fs::hard_link(stage.project_root.join(SOURCE), &staged_file_link).unwrap();
            let stage_container = stage.container.clone();
            let published = stage.publish(&fixture.root, false).unwrap();
            assert_eq!(published.changed_roots, logical_paths);
            assert!(published.expected == expected);
            assert!(
                fixture
                    .root
                    .join("src/ReplicatedStorage/Nested/Empty")
                    .is_dir()
            );
            assert_eq!(
                fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
                "return 'captured'\n"
            );
            assert!(!stage_container.exists());
            fs::write(staged_file_link, "same file, not a copy").unwrap();
            assert_eq!(
                fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
                "same file, not a copy"
            );
        }
    }

    #[test]
    fn export_publication_swaps_complete_replacements_without_broadening_changed_paths() {
        let mut fixture = Fixture::new();
        drop(fixture.stage.take());
        let deleted = PathBuf::from("src/ReplicatedStorage/Deleted.luau");
        fs::write(fixture.root.join(&deleted), "old").unwrap();
        fs::write(fixture.root.join("outside.txt"), "unrelated").unwrap();
        let stage = ExportProjectStage::create(
            &fixture.root,
            Path::new("src"),
            &["ReplicatedStorage".into()],
        )
        .unwrap();
        fs::remove_file(stage.project_root.join(&deleted)).unwrap();
        fs::write(stage.project_root.join(SOURCE), "new").unwrap();
        assert_eq!(
            fixture.planned_directory_swaps(&stage),
            [PathBuf::from("src/ReplicatedStorage")]
        );
        let current = collect_publish_hashes(&fixture.root, &stage.publish_paths).unwrap();
        let staged = collect_publish_hashes(&stage.project_root, &stage.publish_paths).unwrap();
        let logical_paths = publish_operation_paths(&current, &staged);
        let published = stage.publish(&fixture.root, false).unwrap();
        assert_eq!(published.changed_roots, logical_paths);
        assert!(
            !published
                .expected
                .contains_key(Path::new("src/ReplicatedStorage"))
        );
        assert!(published.expected.get(&deleted) == Some(&None));
        assert!(!fixture.root.join(&deleted).exists());
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("outside.txt")).unwrap(),
            "unrelated"
        );
    }

    #[test]
    fn export_publication_keeps_unchanged_files_and_empty_directories_out_of_swaps() {
        for keep_file in [false, true] {
            let mut fixture = Fixture::new();
            drop(fixture.stage.take());
            let keep = fixture.root.join("src/ReplicatedStorage/Keep");
            let original_file_link = fixture.container.join("original-file-link");
            if keep_file {
                fs::write(&keep, "unchanged").unwrap();
                fs::hard_link(&keep, &original_file_link).unwrap();
            } else {
                fs::create_dir(&keep).unwrap();
            }
            let stage = ExportProjectStage::create(
                &fixture.root,
                Path::new("src"),
                &["ReplicatedStorage".into()],
            )
            .unwrap();
            fs::write(stage.project_root.join(SOURCE), "changed").unwrap();
            let new_subtree = PathBuf::from("src/ReplicatedStorage/New");
            fs::create_dir(stage.project_root.join(&new_subtree)).unwrap();
            fs::write(
                stage.project_root.join(&new_subtree).join("Child.luau"),
                "new",
            )
            .unwrap();
            assert_eq!(fixture.planned_directory_swaps(&stage), [new_subtree]);
            let published = stage.publish(&fixture.root, false).unwrap();
            assert!(
                !published
                    .expected
                    .contains_key(Path::new("src/ReplicatedStorage/Keep"))
            );
            if keep_file {
                fs::write(original_file_link, "outside edit after publication").unwrap();
                assert_eq!(
                    fs::read_to_string(keep).unwrap(),
                    "outside edit after publication"
                );
            } else {
                assert!(keep.is_dir());
            }
        }
    }

    #[test]
    fn export_publication_directory_swaps_stay_inside_exact_owner_scopes() {
        // These are publication scopes after projection/adapter syncback, not
        // permission to replace their parents or unrelated source-root content.
        for owner in ["configured/Partial", "mount/source", "adapter/records.csv"] {
            let mut fixture = Fixture::new();
            let mut stage = fixture.stage.take().unwrap();
            let scope = PathBuf::from(owner);
            let is_file = scope.extension().is_some();
            let file = if is_file {
                scope.clone()
            } else {
                scope.join("Mod.luau")
            };
            for root in [&fixture.root, &stage.project_root] {
                fs::create_dir_all(root.join(file.parent().unwrap())).unwrap();
            }
            fs::write(fixture.root.join(&file), "original").unwrap();
            fs::write(stage.project_root.join(&file), "captured").unwrap();
            let neighbor = fixture
                .root
                .join(scope.parent().unwrap())
                .join("unrelated.txt");
            fs::write(&neighbor, "outside owner").unwrap();
            stage.publish_paths = vec![scope.clone()];
            stage.capture_publish_baseline(&fixture.root).unwrap();
            let swaps = fixture.planned_directory_swaps(&stage);
            if is_file {
                assert!(swaps.is_empty());
            } else {
                assert_eq!(swaps, [scope]);
            }
            let published = stage.publish(&fixture.root, false).unwrap();
            assert_eq!(published.changed_roots, std::slice::from_ref(&file));
            assert_eq!(
                fs::read_to_string(fixture.root.join(file)).unwrap(),
                "captured"
            );
            assert_eq!(fs::read_to_string(neighbor).unwrap(), "outside owner");
            assert_eq!(
                fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
                "return 'original'\n"
            );
        }
    }

    #[test]
    fn export_publication_directory_swap_rechecks_new_entries_in_its_full_footprint() {
        let mut fixture = Fixture::new();
        let stage = fixture.stage.take().unwrap();
        let current = collect_publish_hashes(&fixture.root, &stage.publish_paths).unwrap();
        let staged = collect_publish_hashes(&stage.project_root, &stage.publish_paths).unwrap();
        let operations = publish_operation_paths(&current, &staged);
        let swaps = fixture.planned_directory_swaps(&stage);
        assert_eq!(swaps, [PathBuf::from("src/ReplicatedStorage")]);
        let outside = PathBuf::from("src/ReplicatedStorage/Outside.luau");
        assert!(
            !operations
                .iter()
                .any(|operation| outside.starts_with(operation))
        );
        fs::write(fixture.root.join(&outside), "concurrent new file").unwrap();
        let narrow = collect_publish_hashes(&fixture.root, &operations).unwrap();
        ensure_publish_entries_unchanged(&current, &narrow, &operations).unwrap();
        let entire_swap = collect_publish_hashes(&fixture.root, &swaps).unwrap();
        let error = ensure_publish_entries_unchanged(&current, &entire_swap, &swaps).unwrap_err();
        assert!(error.to_string().contains("Outside.luau"));
        let error = stage.publish(&fixture.root, false).err().unwrap();
        assert!(error.to_string().contains("Project files changed"));
        assert_eq!(
            fs::read_to_string(fixture.root.join(outside)).unwrap(),
            "concurrent new file"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "return 'original'\n"
        );
    }

    #[test]
    fn export_publication_rolls_back_directory_swaps_when_a_later_swap_fails() {
        let mut fixture = Fixture::new();
        drop(fixture.stage.take());
        let second = "src/ServerStorage/Other.luau";
        fs::create_dir_all(fixture.root.join("src/ServerStorage")).unwrap();
        fs::write(fixture.root.join(second), "second original").unwrap();
        let stage = ExportProjectStage::create(
            &fixture.root,
            Path::new("src"),
            &["ReplicatedStorage".into(), "ServerStorage".into()],
        )
        .unwrap();
        fs::write(stage.project_root.join(SOURCE), "first captured").unwrap();
        fs::write(stage.project_root.join(second), "second captured").unwrap();
        assert_eq!(
            fixture.planned_directory_swaps(&stage),
            [
                PathBuf::from("src/ReplicatedStorage"),
                PathBuf::from("src/ServerStorage")
            ]
        );
        // The first service is installed, then the second backup rename must
        // fail on both Windows and Unix (onto a nonempty directory).
        let blocked_backup = stage.container.join("previous/src/ServerStorage");
        fs::create_dir_all(&blocked_backup).unwrap();
        fs::write(blocked_backup.join("blocker"), "block second backup").unwrap();
        let stage_container = stage.container.clone();
        let error = stage.publish(&fixture.root, false).err().unwrap();
        assert!(error.to_string().contains("Failed to preserve"));
        assert!(!error.to_string().contains("rollback was incomplete"));
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "return 'original'\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join(second)).unwrap(),
            "second original"
        );
        assert!(!stage_container.exists());
    }

    #[test]
    fn export_publication_validates_before_writes_and_records_afterward() {
        let mut fixture = Fixture::new();
        let stage = fixture.stage.take().unwrap();
        let stage_container = stage.container.clone();
        let calls = Cell::new(0);
        let (published, _) = finish_export_publication(
            Some(stage),
            &fixture.root,
            false,
            || {
                assert_eq!(calls.replace(1), 0);
                assert_eq!(
                    fs::read_to_string(fixture.root.join(SOURCE))?,
                    "return 'original'\n"
                );
                assert_eq!(
                    fs::read_to_string(fixture.root.join("sourcemap.json"))?,
                    ORIGINAL_MAP
                );
                fs::write(fixture.root.join("outside.txt"), "outside edit")?;
                Ok(())
            },
            || {
                assert_eq!(calls.replace(2), 1);
                assert_eq!(
                    fs::read_to_string(fixture.root.join(SOURCE))?,
                    "return 'captured'\n"
                );
                assert_eq!(
                    fs::read_to_string(fixture.root.join("sourcemap.json"))?,
                    "{\"name\":\"Captured\"}"
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert!(!published.changed_roots.is_empty());
        assert_eq!(
            fs::read_to_string(fixture.root.join("outside.txt")).unwrap(),
            "outside edit"
        );
        assert!(!stage_container.exists());
    }

    #[test]
    fn export_publication_guard_failure_preserves_outside_edits_and_discards_stage() {
        let mut fixture = Fixture::new();
        let stage = fixture.stage.take().unwrap();
        let stage_container = stage.container.clone();
        let calls = Cell::new(0);
        let error = finish_export_publication(
            Some(stage),
            &fixture.root,
            false,
            || {
                calls.set(calls.get() + 1);
                fs::write(fixture.root.join(SOURCE), "return 'outside'\n")?;
                bail!("Studio changed ReplicatedStorage during native export")
            },
            || panic!("failed capture must not record completion"),
        )
        .err()
        .expect("a failed guard must abort publication");
        assert_eq!(
            error.to_string(),
            "Studio changed ReplicatedStorage during native export"
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "return 'outside'\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("sourcemap.json")).unwrap(),
            ORIGINAL_MAP
        );
        assert!(!stage_container.exists());
    }

    #[test]
    fn export_publication_retains_original_file_baseline_after_native_validation() {
        let mut fixture = Fixture::new();
        let stage = fixture.stage.take().unwrap();
        let stage_container = stage.container.clone();
        let calls = Cell::new(0);
        let error = finish_export_publication(
            Some(stage),
            &fixture.root,
            false,
            || {
                calls.set(calls.get() + 1);
                fs::write(fixture.root.join(SOURCE), "return 'outside'\n")?;
                Ok(())
            },
            || panic!("publication conflict must not record completion"),
        )
        .err()
        .expect("a concurrent file edit must abort publication");
        assert!(error.to_string().contains("Project files changed"));
        assert_eq!(calls.get(), 1);
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "return 'outside'\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("sourcemap.json")).unwrap(),
            ORIGINAL_MAP
        );
        assert!(!stage_container.exists());
    }

    #[test]
    fn export_publication_completion_failure_is_not_retried_or_rolled_back() {
        let mut fixture = Fixture::new();
        let stage = fixture.stage.take().unwrap();
        let stage_container = stage.container.clone();
        let calls = Cell::new(0);
        let error = finish_export_publication(
            Some(stage),
            &fixture.root,
            false,
            || {
                assert_eq!(calls.replace(1), 0);
                Ok(())
            },
            || {
                assert_eq!(calls.replace(2), 1);
                assert_eq!(
                    fs::read_to_string(fixture.root.join(SOURCE))?,
                    "return 'captured'\n"
                );
                fs::write(
                    fixture.root.join(SOURCE),
                    "return 'outside after publish'\n",
                )?;
                bail!("completion transport failed")
            },
        )
        .err()
        .expect("completion errors must propagate");
        assert_eq!(error.to_string(), "completion transport failed");
        assert_eq!(calls.get(), 2);
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "return 'outside after publish'\n"
        );
        assert!(!stage_container.exists());
    }

    #[test]
    fn export_publication_import_workers_write_only_to_stage() {
        let mut fixture = Fixture::new();
        drop(fixture.stage.take());
        let args = PullArgs::try_parse_from(["pull"]).unwrap();
        let setup = prepare_export_execution(
            &args,
            &fixture.root,
            &["ReplicatedStorage".into()],
            Instant::now(),
        )
        .unwrap();
        let stage_container = setup.project_stage.container.clone();
        let import_project_root = setup.project_stage.import_project_root.clone();
        let import_src_dir = setup.project_stage.import_src_dir.clone();
        assert!(import_project_root.starts_with(&stage_container));
        assert_ne!(import_project_root, fixture.root);
        fs::write(
            import_project_root
                .join(import_src_dir)
                .join("ReplicatedStorage/Mod.luau"),
            "return 'private'\n",
        )
        .unwrap();
        let ExportExecutionSetup {
            project_stage,
            sourcemap_writer,
            direct_import_dispatcher,
            ..
        } = setup;
        drop(direct_import_dispatcher);
        drop(sourcemap_writer);
        drop(project_stage);
        assert!(!stage_container.exists());
        assert_eq!(
            fs::read_to_string(fixture.root.join(SOURCE)).unwrap(),
            "return 'original'\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("sourcemap.json")).unwrap(),
            ORIGINAL_MAP
        );
    }

    #[test]
    fn export_publication_scopes_configured_owners_to_selected_services() {
        let mut fixture = Fixture::new();
        drop(fixture.stage.take());
        fs::create_dir_all(fixture.root.join("src/ServerStorage")).unwrap();
        fs::write(
            fixture.root.join("src/ServerStorage/Untouched.luau"),
            "return 'unrelated'\n",
        )
        .unwrap();
        let mut project = serde_json::to_value(config::ReniumProject::default()).unwrap();
        // Explicit tree owners must not also be implicit sourceRoot owners.
        project["sourceRoot"] = json!("implicit-src");
        project["tree"] = json!({
            "ReplicatedStorage": { "$path": "src/ReplicatedStorage" },
            "ServerStorage": { "$path": "src/ServerStorage" },
        });
        // Required, absent owners would make an unscoped projection fail.
        project["mounts"] = json!([{
            "source": "unrelated-mount", "target": "Workspace.Mount",
        }]);
        project["adapters"] = json!([{
            "source": "unrelated.csv", "target": "LocalizationService.Table",
        }]);
        let project_bytes = serde_json::to_vec(&project).unwrap();
        fs::write(fixture.root.join("renium.project.jsonc"), &project_bytes).unwrap();
        let stage = ExportProjectStage::create(
            &fixture.root,
            Path::new("src"),
            &["ReplicatedStorage".into()],
        )
        .unwrap();
        assert!(stage.project_root.join(SOURCE).is_file());
        assert!(!stage.project_root.join("src/ServerStorage").exists());
        assert!(!stage.project_root.join("unrelated-mount").exists());
        let scoped = stage.loaded.as_ref().unwrap();
        assert_eq!(scoped.project.tree.len(), 1);
        assert!(scoped.project.tree.contains_key("ReplicatedStorage"));
        assert!(scoped.project.mounts.is_empty());
        assert!(scoped.project.adapters.is_empty());
        assert!(
            !stage
                .publish_paths
                .iter()
                .any(|path| path.starts_with("src/ServerStorage"))
        );
        stage.finish_projection(false).unwrap();
        assert_eq!(
            fs::read(fixture.root.join("renium.project.jsonc")).unwrap(),
            project_bytes
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("src/ServerStorage/Untouched.luau")).unwrap(),
            "return 'unrelated'\n"
        );
        drop(stage);
    }
}
