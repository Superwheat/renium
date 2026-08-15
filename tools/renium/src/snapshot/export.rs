use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::app::build::{
    GIT_HASH as BUILD_GIT_HASH, TIMESTAMP_UNIX as BUILD_TIMESTAMP_UNIX, VERSION as BUILD_VERSION,
};
use crate::app::timing::{
    elapsed_ms, log_timing, log_timing_ms, quiet_timings, set_quiet_timings, verbose_timing_logs,
};
use crate::automation::op;
use crate::cli::ExportSnapshotsArgs;
use crate::cli::args::ImportSnapshotsArgs;
use crate::daemon::try_daemon_control_request;
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::project::sourcemap::{
    generate_project_sourcemap, write_project_sourcemap_from_service_nodes,
};
use crate::rbx::model::canonicalize_settings_reference_stores;
use crate::roblox::schema::{
    EnumValueNameMap, PropertySchemaMap, configure_bridge_property_candidates,
    load_rbx_dom_property_schema, parse_enum_value_name_map, parse_property_schema_map,
    parse_string_list,
};
use crate::snapshot::codec::{
    apply_compact_batch_debug_ids, decode_compact_batch_debug_ids, parse_compact_v5_instance_items,
    parse_compact_v5_shape_instance_items,
};
use crate::snapshot::import::{
    DirectImportDispatcher, SourcemapWriter, build_service_state_from_instances,
    direct_import_export_order, fetch_script_sources, import_snapshots, normalize_class_defaults,
    parse_services, resolve_direct_import_drain_workers, resolve_direct_import_workers,
    resolve_source_worker_count,
};
use crate::snapshot::types::{
    AdaptiveTuneCache, AdaptiveTuneEntry, CompactBatchPayload, ExportedSnapshotParts,
    InstanceBatchFetch, InstanceFetchResult, ServiceExecutionSpan, ServiceExportOutput,
    ServiceState, SnapshotInstance,
};
use crate::studio::bridge::{
    BridgeChunk, BridgeInfoPayload, BridgeListenMetrics, BridgePerformanceStats, BridgeServer,
    BridgeTarget, ChunkFetchMetrics, MAX_BRIDGE_CHUNK_BYTES, MAX_BRIDGE_REASSEMBLY_BYTES,
    SourceBatchMap, clamp_bridge_chunk_size,
};
use crate::studio::native::editor::{EditorBinaryExportFinishGuard, editor_binary_export_parts};
use crate::system::files::{
    OnDrop, create_unique_directory, current_unix_ts, read_json_file,
    resolve_existing_project_root, sanitize_name, sha256_hex, write_json_file,
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
const ADAPTIVE_TUNE_CACHE_VERSION: u32 = 3;
const SAFE_CACHED_TUNE_FRAME_MS: f64 = 20.0;
const STALE_CACHED_TUNE_MAX_AGE_SECS: i64 = 24 * 60 * 60;
const LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES: usize = 5_000;
const ADAPTIVE_LAG_FRAME_MS: f64 = 33.3;
const INITIAL_SEED_CHUNKS_PER_BRIDGE_MIN: usize = 4;
const INITIAL_SEED_CHUNKS_PER_BRIDGE_MAX: usize = 5;
const ADAPTIVE_BATCH_GROWTH_DIVISOR: usize = 8;
const ADAPTIVE_WORKER_GROWTH_WAVE_INTERVAL: usize = 2;
const DYNAMIC_RANGES_PER_WORKER: usize = 2;
const DYNAMIC_RANGE_MIN_INSTANCES: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum PerformanceMode {
    Throughput,
    Balanced,
    Smooth,
}

impl PerformanceMode {
    fn parse(raw: &str) -> Self {
        match raw {
            "smooth" => Self::Smooth,
            "balanced" => Self::Balanced,
            _ => Self::Throughput,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Throughput => "throughput",
            Self::Balanced => "balanced",
            Self::Smooth => "smooth",
        }
    }

    fn min_large_service_batch_size(
        self,
        instance_count: usize,
        bridge_concurrency: usize,
    ) -> usize {
        match self {
            Self::Throughput => {
                if instance_count < LARGE_SERVICE_DETERMINISTIC_FETCH_MIN_INSTANCES {
                    return instance_count.max(1);
                }
                let target_ranges = bridge_concurrency.max(1).min(instance_count);
                instance_count.div_ceil(target_ranges)
            }
            Self::Balanced => {
                if instance_count < LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES {
                    return 0;
                }
                instance_count.div_ceil(8)
            }
            Self::Smooth => 0,
        }
    }

    fn large_service_worker_floor(self, instance_count: usize, bridge_concurrency: usize) -> usize {
        if instance_count < LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES {
            return 0;
        }
        match self {
            Self::Throughput | Self::Balanced => bridge_concurrency.max(1),
            Self::Smooth => 0,
        }
    }
}

fn adaptive_tune_cache_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".renium")
        .join("cache")
        .join("adaptive.json")
}

fn adaptive_tune_cache_key(
    bridge_info: &BridgeInfoPayload,
    chunk_size: usize,
    performance_mode: PerformanceMode,
    modified_default_bypass: bool,
) -> String {
    format!(
        "bridge={}:{};protocol={};codec={};chunk_frame={};compact_value={};chunk_size={};performance={};modified_default_bypass={}",
        bridge_info.bridge_version,
        bridge_info.bridge_build_unix,
        bridge_info.protocol_version,
        bridge_info.codec_version,
        bridge_info.chunk_frame_protocol_version,
        bridge_info.compact_value_protocol_version,
        chunk_size,
        performance_mode.as_str(),
        modified_default_bypass
    )
}

fn empty_adaptive_tune_cache(cache_key: &str) -> AdaptiveTuneCache {
    AdaptiveTuneCache {
        version: ADAPTIVE_TUNE_CACHE_VERSION,
        cache_key: cache_key.to_string(),
        ..AdaptiveTuneCache::default()
    }
}

fn load_adaptive_tune_cache(project_root: &Path, expected_cache_key: &str) -> AdaptiveTuneCache {
    let path = adaptive_tune_cache_path(project_root);
    let Ok(cache) = read_json_file::<AdaptiveTuneCache>(&path) else {
        return empty_adaptive_tune_cache(expected_cache_key);
    };
    if cache.version != ADAPTIVE_TUNE_CACHE_VERSION {
        println!(
            "[renium] adaptive tune cache version mismatch at {} (found {}, expected {}); ignoring cached tunes",
            path.display(),
            cache.version,
            ADAPTIVE_TUNE_CACHE_VERSION
        );
        let _ = fs::remove_file(&path);
        return empty_adaptive_tune_cache(expected_cache_key);
    }
    if cache.cache_key != expected_cache_key {
        println!(
            "[renium] adaptive tune cache key mismatch at {}; ignoring cached tunes",
            path.display()
        );
        let _ = fs::remove_file(&path);
        return empty_adaptive_tune_cache(expected_cache_key);
    }
    cache
}

fn write_adaptive_tune_cache(project_root: &Path, cache: &AdaptiveTuneCache) {
    let path = adaptive_tune_cache_path(project_root);
    if let Err(err) = write_json_file(&path, cache, true) {
        println!(
            "[renium] warning: failed to write adaptive tuning cache {}: {err:#}",
            path.display()
        );
    }
}

fn prepare_bridge_for_next_run(bridge: &BridgeServer) {
    let started = Instant::now();
    match bridge.call("prepareForNextRun", json!({})) {
        Ok(_) => log_timing_ms("bridge prepare next run", elapsed_ms(started)),
        Err(err) => {
            println!("[renium] warning: failed to prepare bridge for next run: {err:#}");
        }
    }
}

fn record_bridge_sync_completion(bridge: &BridgeServer) {
    if let Err(err) = bridge.call("recordSyncCompletion", json!({})) {
        println!("[renium] warning: failed to update the Studio sync timestamp: {err:#}");
    }
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

struct ServiceExportContext<'a> {
    bridge: &'a BridgeServer,
    chunk_size: usize,
    adaptive_instance_batches: bool,
    performance_mode: PerformanceMode,
    fetch_class_defaults: bool,
    source_workers: usize,
    instance_workers: usize,
    adaptive_seed_batch: usize,
    property_schema_by_class: &'a PropertySchemaMap,
    run_started: Instant,
}

impl ServiceExportContext<'_> {
    fn export_with_span(
        &self,
        service: &str,
        cached_tune: Option<&AdaptiveTuneEntry>,
    ) -> Result<ServiceExportOutput> {
        if verbose_timing_logs() {
            println!("[renium] exporting {service}");
        }
        let service_export_started_ms = elapsed_ms(self.run_started);
        let (parts, tune) = self.export_parts(service, cached_tune)?;
        let service_export_end_ms = elapsed_ms(self.run_started);
        Ok(ServiceExportOutput {
            parts,
            span: ServiceExecutionSpan {
                service: service.to_string(),
                export_start_ms: service_export_started_ms,
                export_end_ms: service_export_end_ms,
            },
            tune,
        })
    }
}

fn finish_service_export_output(
    output: ServiceExportOutput,
    direct_import_dispatcher: Option<&DirectImportDispatcher>,
    direct_import_mode: bool,
    snapshot_dir: &Path,
    tune_updates: &mut Vec<(String, AdaptiveTuneEntry)>,
    service_export_spans: &mut Vec<ServiceExecutionSpan>,
    cumulative_service_latency_ms: &mut f64,
) -> Result<()> {
    let service = output.span.service.clone();
    *cumulative_service_latency_ms += output.span.export_end_ms - output.span.export_start_ms;
    if let Some(tune) = output.tune {
        tune_updates.push((service.clone(), tune));
    }
    if direct_import_mode {
        let dispatcher = direct_import_dispatcher
            .with_context(|| "Direct import dispatcher is not available")?;
        dispatcher.check_error()?;
        dispatcher.enqueue_parts(&service, output.parts)?;
    } else {
        let path = snapshot_dir.join(format!("{service}.json"));
        write_json_file(
            &path,
            &json!({
                "classDefaults": output.parts.class_defaults,
                "instances": output.parts.instances,
            }),
            true,
        )?;
        println!("[renium] wrote {}", path.display());
    }
    service_export_spans.push(output.span);
    Ok(())
}

struct ExportPrelude {
    total_started: Instant,
    performance_mode: PerformanceMode,
    modified_default_bypass: bool,
    project_root: PathBuf,
    services: Vec<String>,
    snapshot_dir: PathBuf,
    ports: Vec<u16>,
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
    active: bool,
}

impl ExportProjectStage {
    pub(crate) fn create(project_root: &Path, src_dir: &Path, services: &[String]) -> Result<Self> {
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
        let loaded = config::try_load_project(None, Some(project_root))?
            .filter(|loaded| loaded.root == project_root);
        let mut publish_paths = Vec::new();
        let mut clone_paths = Vec::new();
        if let Some(loaded) = loaded.as_ref() {
            let project_file = loaded.path.strip_prefix(project_root)?.to_path_buf();
            clone_paths.push(project_file);
            let adapter_baseline = PathBuf::from(".renium").join("adapter-baseline.json");
            clone_paths.push(adapter_baseline.clone());
            publish_paths.push(adapter_baseline);
            let source_root = loaded.project.source_root.clone();
            for service in services {
                publish_paths.push(source_root.join(sanitize_name(service)));
            }
            clone_paths.push(source_root);
            let mut nested_projects = HashSet::new();
            for (_, node) in config::project_tree_nodes(&loaded.project.tree) {
                if let Some(path) = node.path {
                    clone_paths.push(path.clone());
                    publish_paths.push(path.clone());
                    let source = loaded.root.join(&path);
                    if project_path_is_nested(&source) && source.is_file() {
                        collect_nested_project_paths(
                            project_root,
                            &source,
                            &mut clone_paths,
                            &mut publish_paths,
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
                        &mut clone_paths,
                        &mut publish_paths,
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
                        &mut clone_paths,
                        &mut publish_paths,
                        &mut nested_projects,
                        adapter.direction != config::AdapterDirection::ToProject,
                    )?;
                }
                if let Some(output) = adapter.output.as_ref() {
                    clone_paths.push(output.clone());
                }
            }
        } else {
            for service in services {
                let path = src_dir.join(sanitize_name(service));
                clone_paths.push(path.clone());
                publish_paths.push(path);
            }
        }
        clone_paths.push(PathBuf::from("sourcemap.json"));
        publish_paths.push(PathBuf::from("sourcemap.json"));
        normalize_owned_paths(&mut clone_paths);
        normalize_owned_paths(&mut publish_paths);
        let publish_baseline = collect_publish_hashes(project_root, &publish_paths)?;
        for relative in &clone_paths {
            let source = project_root.join(relative);
            if source.exists() {
                copy_isolated_path(&source, &staged_root.join(relative))?;
            }
        }
        let staged_loaded = if let Some(original) = loaded.as_ref() {
            let relative = original.path.strip_prefix(project_root)?;
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
            active: true,
        };
        log_timing_ms("export project stage copy", elapsed_ms(started));
        Ok(stage)
    }

    pub(crate) fn finish_projection(&self) -> Result<()> {
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
        generate_project_sourcemap(&self.project_root)
    }

    pub(crate) fn preview_operations(&self, project_root: &Path) -> Result<Vec<Value>> {
        let staged = collect_publish_hashes(&self.project_root, &self.publish_paths)?;
        let current = collect_publish_hashes(project_root, &self.publish_paths)?;
        let adapter_paths = self
            .loaded
            .as_ref()
            .map(|loaded| {
                let mut paths = loaded
                    .project
                    .adapters
                    .iter()
                    .flat_map(|adapter| {
                        [Some(adapter.source.clone()), adapter.output.clone()]
                            .into_iter()
                            .flatten()
                    })
                    .collect::<Vec<_>>();
                paths.push(PathBuf::from(".renium/adapter-baseline.json"));
                paths
            })
            .unwrap_or_default();
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

    pub(crate) fn publish(mut self, project_root: &Path) -> Result<Vec<PathBuf>> {
        let started = Instant::now();
        let current = collect_publish_hashes(project_root, &self.publish_paths)?;
        if current != self.publish_baseline {
            let changed = current
                .keys()
                .chain(self.publish_baseline.keys())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|path| current.get(*path) != self.publish_baseline.get(*path))
                .take(10)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            bail!(
                "Project files changed while Studio export was running; retry without overwriting: {}",
                changed.join(", ")
            );
        }
        let backup_root = self.container.join("previous");
        fs::create_dir_all(&backup_root)
            .with_context(|| format!("Failed to create {}", backup_root.display()))?;
        let staged_state = collect_publish_hashes(&self.project_root, &self.publish_paths)?;
        let operation_paths = publish_operation_paths(&current, &staged_state);
        let changed_paths = operation_paths.clone();
        let mut published = Vec::<(PathBuf, Option<PathBuf>)>::new();
        let publish_result = (|| -> Result<()> {
            for relative in operation_paths {
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
                    copy_isolated_path(&staged, &destination).with_context(|| {
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
        self.active = false;
        let _ = fs::remove_dir_all(&self.container);
        log_timing_ms("export project stage publish", elapsed_ms(started));
        Ok(changed_paths)
    }
}

impl Drop for ExportProjectStage {
    fn drop(&mut self) {
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
    if writable {
        publish_paths.push(relative(&source_root)?);
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
        if let Some(output) = adapter.output.as_ref() {
            clone_paths.push(relative(&loaded.root.join(output))?);
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

#[derive(PartialEq)]
pub(crate) enum PublishEntryState {
    Directory,
    File(String),
    Symlink(PathBuf),
}

pub(crate) fn collect_publish_hashes(
    root: &Path,
    publish_paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, PublishEntryState>> {
    let mut entries = BTreeMap::new();
    for relative in publish_paths {
        let path = root.join(relative);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            entries.insert(
                relative.clone(),
                PublishEntryState::Symlink(fs::read_link(&path)?),
            );
            continue;
        }
        if metadata.is_file() {
            entries.insert(
                relative.clone(),
                PublishEntryState::File(sha256_hex(&fs::read(&path)?)),
            );
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            let entry_relative = entry.path().strip_prefix(root)?.to_path_buf();
            let state = if entry.file_type().is_symlink() {
                PublishEntryState::Symlink(fs::read_link(entry.path())?)
            } else if entry.file_type().is_dir() {
                PublishEntryState::Directory
            } else if entry.file_type().is_file() {
                PublishEntryState::File(sha256_hex(&fs::read(entry.path())?))
            } else {
                continue;
            };
            entries.insert(entry_relative, state);
        }
    }
    Ok(entries)
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
        fs::copy(source, destination)
            .with_context(|| format!("Failed to stage {}", source.display()))?;
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
            fs::copy(entry.path(), &target)
                .with_context(|| format!("Failed to stage {}", entry.path().display()))?;
        }
    }
    Ok(())
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

#[derive(Clone, Copy)]
enum ExportBridgeMode {
    Cold,
    Warm { prepare_next_run: bool },
}

fn export_snapshots_prelude(args: &ExportSnapshotsArgs) -> Result<ExportPrelude> {
    set_quiet_timings(args.quiet_timings);

    let total_started = Instant::now();
    let performance_mode = PerformanceMode::parse(&args.performance_mode);
    let modified_default_bypass = if args.no_modified_default_bypass {
        false
    } else {
        args.modified_default_bypass
    };

    let project_root = resolve_existing_project_root(&args.project_root)?;
    config::validate_relative_portable_path(&args.src_dir, "srcDir")?;
    let services = parse_services(&args.services)?;
    let snapshot_dir = if args.snapshot_dir.is_absolute() {
        args.snapshot_dir.clone()
    } else {
        project_root.join(&args.snapshot_dir)
    };

    let ports = parse_bridge_ports(&args.bridge.ports)?;
    println!(
        "[renium] export start: version={}, git={}, build_ts={}, protocol={}, services={}, chunk_size={}, import_mode={}, performance_mode={}, modified_default_bypass={}",
        BUILD_VERSION,
        BUILD_GIT_HASH,
        BUILD_TIMESTAMP_UNIX,
        BRIDGE_PROTOCOL_VERSION,
        services.len(),
        args.chunk_size,
        args.import_mode,
        performance_mode.as_str(),
        modified_default_bypass
    );
    println!("[renium] effective chunk size: {} bytes", args.chunk_size);
    println!("[renium] modified default bypass: {modified_default_bypass}");
    Ok(ExportPrelude {
        total_started,
        performance_mode,
        modified_default_bypass,
        project_root,
        services,
        snapshot_dir,
        ports,
    })
}

pub(crate) fn export_snapshots(mut args: ExportSnapshotsArgs) -> Result<()> {
    apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
    let operation = if args.run_import {
        op::PULL
    } else {
        op::EXPORT_SNAPSHOTS
    };
    let parameters = json!({
        "srcDir": args.src_dir,
        "snapshotDir": args.snapshot_dir,
        "services": args.services,
        "chunkSize": args.chunk_size,
        "adaptiveSeedBatch": args.adaptive_seed_batch,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
        "importMode": args.import_mode,
        "sourceWorkers": args.source_workers,
        "instanceWorkers": args.instance_workers,
        "importWorkers": args.import_workers,
        "performanceMode": args.performance_mode,
        "modifiedDefaultBypass": args.modified_default_bypass,
        "noModifiedDefaultBypass": args.no_modified_default_bypass,
        "adaptiveThrottle": !args.no_adaptive_throttle,
        "exportAllProperties": args.export_all_properties,
        "noExportAllProperties": args.no_export_all_properties,
    });
    if let Some(result) =
        try_daemon_control_request(operation, Some(&args.project_root), parameters, false)?
    {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let prelude = export_snapshots_prelude(&args)?;
    let max_bridge_connect_attempts = 3usize;
    let mut bridge_connect_attempt = 0usize;
    let (bridge, bridge_listen_metrics, bridge_info, all_channels_connected_to_bridge_info_ms) = loop {
        bridge_connect_attempt += 1;
        let (candidate_bridge, candidate_listen_metrics) = match BridgeServer::listen(
            &args.bridge.host,
            &prelude.ports,
            args.bridge.wait_seconds,
        ) {
            Ok(result) => result,
            Err(err)
                if bridge_connect_attempt < max_bridge_connect_attempts
                    && is_transient_bridge_error(&err) =>
            {
                println!(
                    "[renium] warning: bridge listen failed on attempt {bridge_connect_attempt}/{max_bridge_connect_attempts}; retrying: {err}"
                );
                thread::sleep(Duration::from_millis(83 * bridge_connect_attempt as u64));
                continue;
            }
            Err(err) => return Err(err),
        };

        let bridge_info_started = Instant::now();
        match candidate_bridge
            .cached_bridge_info_for_target(BridgeTarget::Main)
            .and_then(|info| {
                validate_bridge_info(&info)?;
                Ok(info)
            }) {
            Ok(info) => {
                break (
                    candidate_bridge,
                    candidate_listen_metrics,
                    info,
                    elapsed_ms(bridge_info_started),
                );
            }
            Err(err)
                if bridge_connect_attempt < max_bridge_connect_attempts
                    && is_transient_bridge_error(&err) =>
            {
                println!(
                    "[renium] warning: bridge info handshake failed on attempt {bridge_connect_attempt}/{max_bridge_connect_attempts}; retrying: {err}"
                );
                drop(candidate_bridge);
                thread::sleep(Duration::from_millis(83 * bridge_connect_attempt as u64));
            }
            Err(err) => return Err(err),
        }
    };
    export_snapshots_core(
        &args,
        prelude,
        &bridge,
        &bridge_info,
        bridge_listen_metrics,
        all_channels_connected_to_bridge_info_ms,
        ExportBridgeMode::Cold,
    )
}

pub(crate) fn export_snapshots_with_warm_bridge(
    args: ExportSnapshotsArgs,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    bridge_info_refresh_ms: f64,
    prepare_next_run: bool,
) -> Result<()> {
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
        BridgeListenMetrics {
            bind_ms: 0.0,
            wait_for_channels_ms: 0.0,
        },
        bridge_info_refresh_ms,
        ExportBridgeMode::Warm { prepare_next_run },
    )
}

fn log_export_bridge_connection(
    total_started: Instant,
    metrics: BridgeListenMetrics,
    bridge_info_ms: f64,
    bridge_info: &BridgeInfoPayload,
) -> (f64, f64) {
    let cli_to_listen_ms = (elapsed_ms(total_started) - metrics.wait_for_channels_ms).max(0.0);
    let channel_wait_ms = metrics.wait_for_channels_ms;
    log_timing_ms("cli start to bridge listen", cli_to_listen_ms);
    log_timing_ms("bridge listen to all channels connected", channel_wait_ms);
    log_timing_ms("bridge bind/listen setup", metrics.bind_ms);
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
    (cli_to_listen_ms, channel_wait_ms)
}

struct ExportBridgeSetup {
    property_schema_by_class: PropertySchemaMap,
    property_schema_ready_ms: f64,
    bridge_info_to_property_schema_ready_ms: f64,
}

fn prepare_export_bridge(
    args: &ExportSnapshotsArgs,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    project_root: &Path,
    performance_mode: PerformanceMode,
    modified_default_bypass: bool,
    total_started: Instant,
) -> Result<ExportBridgeSetup> {
    let export_all_properties = args.export_all_properties && !args.no_export_all_properties;
    if export_all_properties {
        println!("[renium] full property export requested; default-value elision disabled");
    }
    let bridge_options_match = bridge_info.performance_mode == performance_mode.as_str()
        && bridge_info.modified_default_bypass == modified_default_bypass
        && bridge_info.export_all_properties == export_all_properties;
    if bridge_options_match {
        println!("[renium] plugin export options already match requested configuration");
    } else {
        bridge
            .call(
                "setExportOptions",
                json!({
                    "exportAllProperties": export_all_properties,
                    "performanceMode": performance_mode.as_str(),
                    "modifiedDefaultBypass": modified_default_bypass,
                }),
            )
            .context("Failed to apply plugin export options")?;
        bridge.cache_export_options_for_target(
            BridgeTarget::Main,
            performance_mode.as_str(),
            modified_default_bypass,
            export_all_properties,
        );
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
        property_schema_by_class,
        property_schema_ready_ms,
        bridge_info_to_property_schema_ready_ms,
    })
}

struct ServiceExportRun<'a> {
    spans: Vec<ServiceExecutionSpan>,
    cumulative_latency_ms: f64,
    tune_updates: Vec<(String, AdaptiveTuneEntry)>,
    native_finish_guard: Option<EditorBinaryExportFinishGuard<'a>>,
}

impl ServiceExportRun<'_> {
    fn finish_output(
        &mut self,
        output: ServiceExportOutput,
        dispatcher: Option<&DirectImportDispatcher>,
        direct_import_mode: bool,
        snapshot_dir: &Path,
    ) -> Result<()> {
        finish_service_export_output(
            output,
            dispatcher,
            direct_import_mode,
            snapshot_dir,
            &mut self.tune_updates,
            &mut self.spans,
            &mut self.cumulative_latency_ms,
        )
    }
}

fn run_service_exports<'a>(
    context: &ServiceExportContext<'a>,
    services: &[String],
    tune_cache: &AdaptiveTuneCache,
    dispatcher: Option<&DirectImportDispatcher>,
    direct_import_mode: bool,
    snapshot_dir: &Path,
) -> Result<ServiceExportRun<'a>> {
    let mut run = ServiceExportRun {
        spans: Vec::with_capacity(services.len()),
        cumulative_latency_ms: 0.0,
        tune_updates: Vec::new(),
        native_finish_guard: None,
    };
    if direct_import_mode {
        println!("[renium] native full export enabled");
        let guard = {
            let mut finish_output =
                |output| run.finish_output(output, dispatcher, true, snapshot_dir);
            let mut release_import = || {
                if let Some(dispatcher) = dispatcher {
                    dispatcher.activate_workers(1);
                }
                Ok(())
            };
            editor_binary_export_parts(
                context.bridge,
                services,
                context.run_started,
                &mut finish_output,
                &mut release_import,
            )?
        };
        run.native_finish_guard = Some(guard);
    } else {
        for service in services {
            let output = context.export_with_span(service, tune_cache.services.get(service))?;
            run.finish_output(output, None, false, snapshot_dir)?;
        }
    }
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
    args: &ExportSnapshotsArgs,
    services: &[String],
    snapshot_dir: &Path,
    import_project_root: &Path,
    import_src_dir: &Path,
    dispatcher: &mut Option<DirectImportDispatcher>,
    sourcemap_writer: Option<SourcemapWriter>,
) -> Result<ImportFinishMetrics> {
    if args.no_run_import {
        return Ok(ImportFinishMetrics::default());
    }
    let mut metrics = ImportFinishMetrics::default();
    if args.import_mode == "direct" {
        let mut sourcemap_nodes = HashMap::new();
        if let Some(dispatcher) = dispatcher.take() {
            let drain_started = Instant::now();
            sourcemap_nodes = dispatcher.finish()?;
            metrics.dispatcher_drain_ms = elapsed_ms(drain_started);
            log_timing_ms(
                "direct import dispatcher drain",
                metrics.dispatcher_drain_ms,
            );
        }
        if let Some(writer) = sourcemap_writer.as_ref() {
            writer.request_finish();
        }
        let sourcemap_started = Instant::now();
        if let Some(writer) = sourcemap_writer {
            writer.join()?;
        } else {
            write_project_sourcemap_from_service_nodes(import_project_root, &sourcemap_nodes)?;
        }
        metrics.sourcemap_finalize_ms = elapsed_ms(sourcemap_started);
        log_timing_ms("sourcemap finalize", metrics.sourcemap_finalize_ms);
    } else {
        import_snapshots(ImportSnapshotsArgs {
            snapshot_dir: snapshot_dir.to_path_buf(),
            project_root: import_project_root.to_path_buf(),
            src_dir: import_src_dir.to_path_buf(),
            services: services.join(","),
            no_project_write: false,
            threads: 0,
        })?;
    }
    let changed =
        canonicalize_settings_reference_stores(&import_project_root.join(import_src_dir))?;
    if changed > 0 {
        println!("[renium] refreshed reference identities in {changed} service store(s)");
    }
    Ok(metrics)
}

struct ExportExecutionSetup {
    project_stage: Option<ExportProjectStage>,
    import_project_root: PathBuf,
    import_src_dir: PathBuf,
    adaptive_instance_batches: bool,
    direct_import_mode: bool,
    effective_chunk: usize,
    adaptive_tune_cache: AdaptiveTuneCache,
    sourcemap_writer: Option<SourcemapWriter>,
    direct_import_dispatcher: Option<DirectImportDispatcher>,
    export_services: Vec<String>,
}

fn start_direct_import(
    enabled: bool,
    args: &ExportSnapshotsArgs,
    project_root: &Path,
    src_dir: &Path,
    sourcemap_writer: Option<&SourcemapWriter>,
    total_started: Instant,
) -> Result<Option<DirectImportDispatcher>> {
    if !enabled {
        return Ok(None);
    }
    let default_workers = resolve_direct_import_workers(args.import_workers);
    let drain_workers = resolve_direct_import_drain_workers(args.import_workers, default_workers);
    println!("[renium] direct import workers during export: 0, after export: {drain_workers}");
    DirectImportDispatcher::start(
        project_root.to_path_buf(),
        src_dir.to_path_buf(),
        0,
        drain_workers,
        sourcemap_writer.map(SourcemapWriter::sender),
        total_started,
    )
    .map(Some)
}

fn ordered_export_services(
    direct_import_mode: bool,
    services: &[String],
    adaptive_tune_cache: &AdaptiveTuneCache,
) -> Vec<String> {
    if !direct_import_mode {
        return services.to_vec();
    }
    let ordered = direct_import_export_order(services, adaptive_tune_cache);
    if ordered != services {
        println!("[renium] direct import export order: {}", ordered.join(","));
    }
    ordered
}

fn prepare_export_execution(
    args: &ExportSnapshotsArgs,
    project_root: &Path,
    services: &[String],
    bridge_info: &BridgeInfoPayload,
    performance_mode: PerformanceMode,
    modified_default_bypass: bool,
    total_started: Instant,
) -> Result<ExportExecutionSetup> {
    let run_import = !args.no_run_import;
    let project_stage = run_import
        .then(|| ExportProjectStage::create(project_root, &args.src_dir, services))
        .transpose()?;
    let import_project_root = project_stage.as_ref().map_or_else(
        || project_root.to_path_buf(),
        |stage| stage.import_project_root.clone(),
    );
    let import_src_dir = project_stage.as_ref().map_or_else(
        || args.src_dir.clone(),
        |stage| stage.import_src_dir.clone(),
    );
    let adaptive_instance_batches = !args.no_adaptive_throttle;
    println!(
        "[renium] adaptive instance batching: {}",
        if adaptive_instance_batches {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("[renium] performance mode: {}", performance_mode.as_str());
    if args.adaptive_seed_batch > 0 {
        println!(
            "[renium] adaptive seed batch override: {}",
            args.adaptive_seed_batch
        );
    }
    let direct_import_mode = run_import && args.import_mode == "direct";
    let effective_chunk = clamp_bridge_chunk_size(args.chunk_size);
    if effective_chunk != args.chunk_size {
        println!(
            "[renium] warning: clamped requested chunk size {} to {} bytes",
            args.chunk_size, effective_chunk
        );
    }
    let cache_key = adaptive_tune_cache_key(
        bridge_info,
        effective_chunk,
        performance_mode,
        modified_default_bypass,
    );
    let adaptive_tune_cache = load_adaptive_tune_cache(project_root, &cache_key);
    let sourcemap_writer =
        direct_import_mode.then(|| SourcemapWriter::start(import_project_root.clone()));
    let direct_import_dispatcher = start_direct_import(
        direct_import_mode,
        args,
        &import_project_root,
        &import_src_dir,
        sourcemap_writer.as_ref(),
        total_started,
    )?;
    let export_services =
        ordered_export_services(direct_import_mode, services, &adaptive_tune_cache);
    Ok(ExportExecutionSetup {
        project_stage,
        import_project_root,
        import_src_dir,
        adaptive_instance_batches,
        direct_import_mode,
        effective_chunk,
        adaptive_tune_cache,
        sourcemap_writer,
        direct_import_dispatcher,
        export_services,
    })
}

fn finish_sync_completion(
    bridge: &BridgeServer,
    native_finish_guard: Option<EditorBinaryExportFinishGuard<'_>>,
) -> f64 {
    let started = Instant::now();
    let recorded = native_finish_guard.is_some_and(|mut guard| match guard.finish(true) {
        Ok(recorded) => recorded,
        Err(err) => {
            println!("[renium] warning: failed to finish the native export session: {err:#}");
            false
        }
    });
    if !recorded {
        record_bridge_sync_completion(bridge);
    }
    elapsed_ms(started)
}

fn export_snapshots_core(
    args: &ExportSnapshotsArgs,
    prelude: ExportPrelude,
    bridge: &BridgeServer,
    bridge_info: &BridgeInfoPayload,
    bridge_listen_metrics: BridgeListenMetrics,
    all_channels_connected_to_bridge_info_ms: f64,
    mode: ExportBridgeMode,
) -> Result<()> {
    let ExportPrelude {
        total_started,
        performance_mode,
        modified_default_bypass,
        project_root,
        services,
        snapshot_dir,
        ..
    } = prelude;
    let (cli_start_to_bridge_listen_ms, bridge_listen_to_all_channels_connected_ms) =
        log_export_bridge_connection(
            total_started,
            bridge_listen_metrics,
            all_channels_connected_to_bridge_info_ms,
            bridge_info,
        );
    let ExportBridgeSetup {
        property_schema_by_class,
        property_schema_ready_ms,
        bridge_info_to_property_schema_ready_ms,
    } = prepare_export_bridge(
        args,
        bridge,
        bridge_info,
        &project_root,
        performance_mode,
        modified_default_bypass,
        total_started,
    )?;
    let ExportExecutionSetup {
        mut project_stage,
        import_project_root,
        import_src_dir,
        adaptive_instance_batches,
        direct_import_mode,
        effective_chunk,
        mut adaptive_tune_cache,
        sourcemap_writer,
        mut direct_import_dispatcher,
        export_services,
    } = prepare_export_execution(
        args,
        &project_root,
        &services,
        bridge_info,
        performance_mode,
        modified_default_bypass,
        total_started,
    )?;
    let export_context = ServiceExportContext {
        bridge,
        chunk_size: effective_chunk,
        adaptive_instance_batches,
        performance_mode,
        fetch_class_defaults: true,
        source_workers: args.source_workers,
        instance_workers: args.instance_workers,
        adaptive_seed_batch: args.adaptive_seed_batch,
        property_schema_by_class: &property_schema_by_class,
        run_started: total_started,
    };
    let ServiceExportRun {
        spans: service_export_spans,
        cumulative_latency_ms: cumulative_service_latency_ms,
        tune_updates,
        mut native_finish_guard,
    } = run_service_exports(
        &export_context,
        &export_services,
        &adaptive_tune_cache,
        direct_import_dispatcher.as_ref(),
        direct_import_mode,
        &snapshot_dir,
    )?;
    let tune_updated = !tune_updates.is_empty();
    for (service, tune) in tune_updates {
        adaptive_tune_cache.services.insert(service, tune);
    }
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
    let ImportFinishMetrics {
        dispatcher_drain_ms,
        sourcemap_finalize_ms,
    } = finish_export_import(
        args,
        &services,
        &snapshot_dir,
        &import_project_root,
        &import_src_dir,
        &mut direct_import_dispatcher,
        sourcemap_writer,
    )?;
    if let Some(stage) = project_stage.take() {
        stage.finish_projection()?;
        stage.publish(&project_root)?;
    }
    if tune_updated {
        write_adaptive_tune_cache(&project_root, &adaptive_tune_cache);
    }

    let sync_completion_ms = finish_sync_completion(bridge, native_finish_guard.take());
    let total_run_ms = elapsed_ms(total_started);
    let handshake_ms =
        bridge_listen_to_all_channels_connected_ms + all_channels_connected_to_bridge_info_ms;
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
        "[renium] run timing spans: cli_start_to_bridge_listen_ms={cli_start_to_bridge_listen_ms:.1}, bridge_listen_to_all_channels_connected_ms={bridge_listen_to_all_channels_connected_ms:.1}, all_channels_connected_to_bridge_info_ms={all_channels_connected_to_bridge_info_ms:.1}, bridge_info_to_property_schema_ready_ms={bridge_info_to_property_schema_ready_ms:.1}, property_schema_ready_to_first_service_export_ms={property_schema_ready_to_first_service_export_ms:.1}, first_service_export_to_last_service_export_ms={first_service_export_to_last_service_export_ms:.1}, cumulative_service_latency_ms={cumulative_service_latency_ms:.1}, last_service_export_to_dispatcher_drain_start_ms={last_service_export_to_dispatcher_drain_start_ms:.1}, dispatcher_drain_ms={dispatcher_drain_ms:.1}, sourcemap_finalize_ms={sourcemap_finalize_ms:.1}, sync_completion_ms={sync_completion_ms:.1}, total_run_ms={total_run_ms:.1}"
    );
    println!(
        "[renium] run timing summary: total_ms={total_run_ms:.1}, core_export_ms={core_export_ms:.1}, bridge_startup_ms={cli_start_to_bridge_listen_ms:.1}, handshake_ms={handshake_ms:.1}, cumulative_service_latency_ms={cumulative_service_latency_ms:.1}, import_critical_tail_ms={import_critical_tail_ms:.1}, unmeasured_or_scheduler_gap_ms={unmeasured_or_scheduler_gap_ms:.1}"
    );
    log_timing_ms("full export-snapshots run", total_run_ms);
    if matches!(
        mode,
        ExportBridgeMode::Warm {
            prepare_next_run: true
        }
    ) {
        prepare_bridge_for_next_run(bridge);
    }
    println!("[renium] export done");
    Ok(())
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

impl ServiceExportContext<'_> {
    fn export_parts(
        &self,
        service: &str,
        cached_tune: Option<&AdaptiveTuneEntry>,
    ) -> Result<(ExportedSnapshotParts, Option<AdaptiveTuneEntry>)> {
        let bridge = self.bridge;
        let chunk_size = self.chunk_size;
        let adaptive_instance_batches = self.adaptive_instance_batches;
        let performance_mode = self.performance_mode;
        let fetch_class_defaults = self.fetch_class_defaults;
        let source_workers = self.source_workers;
        let instance_workers = self.instance_workers;
        let adaptive_seed_batch = self.adaptive_seed_batch;
        let default_property_schema_by_class = self.property_schema_by_class;
        let service_started = Instant::now();
        let prepare_started = Instant::now();
        let mut prepare = bridge.call("prepare", json!({ "service": service }))?;
        let release = || {
            OnDrop::new(|| {
                let _ = bridge.call("release", json!({ "service": service }));
            })
        };
        let mut prepared_service = release();
        log_timing(&format!("{service}: prepare"), prepare_started);
        let instance_count = prepare
            .get("instanceCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let script_count = prepare
            .get("scriptCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let prepare_class_names = parse_string_list(prepare.get("classNames"))
            .context("prepare.classNames must be an array of strings")?;
        if verbose_timing_logs() {
            println!(
                "[renium] {service}: prepared instances={instance_count}, scripts={script_count}"
            );
        }
        let mut service_property_schema_by_class =
            parse_property_schema_map(prepare.get("propertySchemaByClass"))?;
        if service_property_schema_by_class.is_empty()
            && !default_property_schema_by_class.is_empty()
        {
            println!(
                "[renium] {service}: plugin property schema cache is empty; configuring rbx-dom candidates and retrying prepare"
            );
            prepared_service.run();
            configure_bridge_property_candidates(bridge, default_property_schema_by_class)
                .context("Failed to configure plugin property candidates")?;
            let retry_prepare_started = Instant::now();
            prepare = bridge.call("prepare", json!({ "service": service }))?;
            prepared_service = release();
            log_timing(
                &format!("{service}: prepare after schema configure"),
                retry_prepare_started,
            );
            service_property_schema_by_class =
                parse_property_schema_map(prepare.get("propertySchemaByClass"))?;
        }
        let enum_value_names_by_type =
            parse_enum_value_name_map(prepare.get("enumValueNamesByType"))?;
        let effective_property_schema_by_class = if service_property_schema_by_class.is_empty() {
            default_property_schema_by_class
        } else {
            &service_property_schema_by_class
        };
        let instance_batches = InstanceBatchContext {
            bridge,
            service,
            chunk_size,
            instance_count,
            property_schema_by_class: effective_property_schema_by_class,
            enum_value_names_by_type: &enum_value_names_by_type,
            class_names: &prepare_class_names,
        };
        let source_worker_count = resolve_source_worker_count(
            source_workers,
            bridge.channel_count(),
            script_count,
            instance_count,
        );

        if verbose_timing_logs() {
            println!(
                "[renium] {service}: script sources={script_count}, workers={source_worker_count}"
            );
        }

        let fetch_class_defaults_for_service = || -> Result<Value> {
            if !fetch_class_defaults {
                return Ok(Value::Object(Map::new()));
            }
            let started = Instant::now();
            let (value, metrics) = fetch_json_payload(chunk_size, |chunk_start, max_len| {
                bridge.call_chunk(
                    "getClassDefaultsChunk",
                    json!({
                        "service": service,
                        "startIndex": chunk_start,
                        "maxLen": max_len,
                    }),
                )
            })?;
            log_chunk_fetch_metrics(&format!("{service}: class defaults"), metrics);
            log_timing(&format!("{service}: class defaults fetch"), started);
            Ok(value)
        };
        let fetch_source_map_for_service = || -> Result<SourceBatchMap> {
            let started = Instant::now();
            let value = fetch_script_sources(
                bridge,
                service,
                chunk_size,
                script_count,
                source_worker_count,
            )?;
            log_timing(&format!("{service}: script source fetch"), started);
            Ok(value)
        };
        let fetch_instance_payload_for_service = || -> Result<InstanceFetchResult> {
            let instance_fetch_started = Instant::now();
            let instance_fetch = if adaptive_instance_batches {
                instance_batches.fetch_adaptive(
                    instance_workers,
                    adaptive_seed_batch,
                    performance_mode,
                    cached_tune,
                )?
            } else {
                let fixed_batch_floor = performance_mode
                    .min_large_service_batch_size(instance_count, bridge.channel_count());
                let base_batch_size = instance_batch_defaults(instance_count)
                    .fixed
                    .max(fixed_batch_floor);
                let instance_worker_count = resolve_instance_worker_count(
                    instance_workers,
                    bridge.channel_count(),
                    instance_count,
                    base_batch_size,
                );
                let instance_batch_size = if instance_workers > 0 {
                    instance_count.div_ceil(instance_worker_count).max(1)
                } else {
                    base_batch_size
                };
                if verbose_timing_logs() {
                    println!(
                        "[renium] {service}: instance batch size {instance_batch_size} (fixed, workers={instance_worker_count}, min_batch_floor={fixed_batch_floor})"
                    );
                }
                InstanceFetchResult {
                    instances: instance_batches
                        .fetch_fixed(instance_batch_size, instance_worker_count)?,
                    tune: None,
                }
            };
            log_timing(
                &format!("{service}: instance fetch"),
                instance_fetch_started,
            );
            Ok(instance_fetch)
        };
        let deterministic_large_service =
            instance_count >= LARGE_SERVICE_DETERMINISTIC_FETCH_MIN_INSTANCES;
        let (class_defaults, mut instance_fetch, source_by_key) = if deterministic_large_service {
            if verbose_timing_logs() {
                println!("[renium] {service}: coordinated large-service fetch mode enabled");
            }
            thread::scope(|scope| -> Result<_> {
                let class_defaults_task = scope.spawn(fetch_class_defaults_for_service);
                let instance_fetch = fetch_instance_payload_for_service()?;
                let class_defaults = match class_defaults_task.join() {
                    Ok(value) => value?,
                    Err(_) => bail!("Class defaults worker panicked for {service}"),
                };
                let source_by_key = fetch_source_map_for_service()?;
                Ok((class_defaults, instance_fetch, source_by_key))
            })?
        } else {
            thread::scope(|scope| -> Result<_> {
                let class_defaults_task = scope.spawn(fetch_class_defaults_for_service);
                let source_fetch_task = scope.spawn(fetch_source_map_for_service);
                let instance_fetch = fetch_instance_payload_for_service()?;

                let class_defaults = match class_defaults_task.join() {
                    Ok(value) => value?,
                    Err(_) => bail!("Class defaults worker panicked for {service}"),
                };
                let source_by_key = match source_fetch_task.join() {
                    Ok(value) => value?,
                    Err(_) => bail!("Script source worker panicked for {service}"),
                };

                Ok((class_defaults, instance_fetch, source_by_key))
            })?
        };

        let merge_started = Instant::now();
        merge_script_sources(&mut instance_fetch.instances, &source_by_key);
        log_timing(&format!("{service}: merge script sources"), merge_started);

        let release_started = Instant::now();
        prepared_service.run();
        log_timing(&format!("{service}: release"), release_started);
        log_timing(
            &format!("{service}: export assembly total"),
            service_started,
        );
        Ok((
            ExportedSnapshotParts {
                class_defaults,
                instances: instance_fetch.instances,
                native_properties_by_instance: None,
            },
            instance_fetch.tune,
        ))
    }
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

struct InstanceBatchDefaults {
    fixed: usize,
    adaptive: usize,
}

fn instance_batch_defaults(instance_count: usize) -> InstanceBatchDefaults {
    let (fixed, adaptive) = if instance_count >= 150_000 {
        (1800, 1800)
    } else if instance_count >= 100_000 {
        (1400, 1600)
    } else if instance_count >= 50_000 {
        (1000, 1400)
    } else if instance_count >= 20_000 {
        (800, 1100)
    } else {
        (500, 800)
    };
    InstanceBatchDefaults { fixed, adaptive }
}

fn auto_instance_worker_target(instance_count: usize, concurrency_cap: usize) -> usize {
    let concurrency_cap = concurrency_cap.max(1);
    let desired = if instance_count >= 20_000 {
        4
    } else if instance_count >= 5_000 {
        3
    } else if instance_count >= 250 {
        2
    } else {
        1
    };
    desired.min(concurrency_cap)
}

fn resolve_instance_worker_count(
    requested_instance_workers: usize,
    channel_count: usize,
    instance_count: usize,
    batch_size: usize,
) -> usize {
    let batch_size = batch_size.max(1);
    let batch_count = instance_count.div_ceil(batch_size).max(1);
    if batch_count <= 1 {
        return 1;
    }

    let channel_count = channel_count.max(1);
    let mut soft_target = auto_instance_worker_target(instance_count, channel_count);
    if instance_count >= LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES {
        soft_target = soft_target.max(channel_count.min(4));
    }
    let hard_cap = channel_count.saturating_mul(2).min(64);
    let cpu_cap = std::thread::available_parallelism()
        .map_or(8, |v| v.get().saturating_mul(2))
        .max(4);
    let effective_cap = hard_cap.min(cpu_cap);

    if requested_instance_workers > 0 {
        return requested_instance_workers
            .min(effective_cap)
            .min(batch_count);
    }

    soft_target.min(effective_cap).min(batch_count)
}

pub(crate) fn adaptive_tune_estimated_total_ms(tune: &AdaptiveTuneEntry) -> Option<f64> {
    let items_fetched = if tune.items_fetched > 0 {
        tune.items_fetched
    } else {
        tune.batch_size
            .saturating_mul(tune.request_count.max(1))
            .min(tune.instance_count.max(1))
    };
    let wave_ms = tune.wave_ms?;
    if items_fetched == 0 || wave_ms <= 0.0 || tune.instance_count == 0 {
        return None;
    }
    let instances_per_ms = items_fetched as f64 / wave_ms;
    if instances_per_ms <= 0.0 {
        return None;
    }
    Some(tune.instance_count as f64 / instances_per_ms)
}

fn adaptive_tune_is_better(
    candidate: &AdaptiveTuneEntry,
    current: &AdaptiveTuneEntry,
    performance_mode: PerformanceMode,
) -> bool {
    let candidate_safe = adaptive_tune_is_safe(candidate, performance_mode);
    let current_safe = adaptive_tune_is_safe(current, performance_mode);
    if candidate_safe != current_safe {
        return candidate_safe;
    }

    match (
        adaptive_tune_estimated_total_ms(candidate),
        adaptive_tune_estimated_total_ms(current),
    ) {
        (Some(candidate_total_ms), Some(current_total_ms)) => {
            if (candidate_total_ms - current_total_ms).abs() > f64::EPSILON {
                return candidate_total_ms < current_total_ms;
            }
        }
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        (None, None) => {}
    }

    match (candidate.wave_ms, current.wave_ms) {
        (Some(candidate_wave_ms), Some(current_wave_ms)) => {
            if (candidate_wave_ms - current_wave_ms).abs() > f64::EPSILON {
                return candidate_wave_ms < current_wave_ms;
            }
        }
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        (None, None) => {}
    }

    if candidate.request_count != current.request_count {
        return candidate.request_count > current.request_count;
    }

    candidate.batch_size < current.batch_size
}

fn adaptive_tune_is_safe(tune: &AdaptiveTuneEntry, performance_mode: PerformanceMode) -> bool {
    let (max_frame_ms, max_stall_count) = match performance_mode {
        PerformanceMode::Throughput => (100.0, 1),
        PerformanceMode::Balanced => (50.0, 0),
        PerformanceMode::Smooth => (33.0, 0),
    };
    tune.frame_ms
        .is_some_and(|value| value < SAFE_CACHED_TUNE_FRAME_MS)
        && tune.max_frame_ms.unwrap_or(SAFE_CACHED_TUNE_FRAME_MS - 0.1) < max_frame_ms
        && tune.stall_count_over_50_ms <= max_stall_count
}

#[derive(Clone, Copy)]
struct AdaptiveFetchConfig<'a> {
    service: &'a str,
    instance_count: usize,
    requested_workers: usize,
    requested_batch: usize,
    performance_mode: PerformanceMode,
    cached_tune: Option<&'a AdaptiveTuneEntry>,
    bridge_concurrency: usize,
}

struct AdaptiveSeed {
    manual_batch: bool,
    bridge_concurrency: usize,
    min_batch_size: usize,
    batch_size: usize,
    workers: usize,
    reason: &'static str,
    trusted_cache: bool,
}

fn adaptive_default_workers(config: AdaptiveFetchConfig<'_>) -> usize {
    let mut workers = auto_instance_worker_target(config.instance_count, config.bridge_concurrency);
    if config.requested_workers == 0 {
        workers = workers.max(
            config
                .performance_mode
                .large_service_worker_floor(config.instance_count, config.bridge_concurrency),
        );
    }
    workers
}

fn adaptive_cached_seed_is_trusted(
    config: AdaptiveFetchConfig<'_>,
    min_batch_size: usize,
    now_unix: i64,
) -> bool {
    config.requested_batch == 0
        && config.cached_tune.is_some_and(|tune| {
            tune.batch_size > 0
                && tune.batch_size >= min_batch_size
                && adaptive_tune_is_safe(tune, config.performance_mode)
                && now_unix.saturating_sub(tune.updated_at_unix) <= STALE_CACHED_TUNE_MAX_AGE_SECS
        })
}

fn log_adaptive_seed(config: AdaptiveFetchConfig<'_>, seed: &AdaptiveSeed) {
    if !verbose_timing_logs() {
        return;
    }
    let default_batch_size = instance_batch_defaults(config.instance_count).adaptive;
    let cached_batch_size = config
        .cached_tune
        .map(|tune| tune.batch_size)
        .filter(|value| *value > 0);
    let cached_or_default_batch_size = cached_batch_size.unwrap_or(default_batch_size);
    let cached_workers = config
        .cached_tune
        .map(|tune| tune.workers)
        .filter(|value| *value > 0);
    if seed.batch_size < cached_or_default_batch_size {
        println!(
            "[renium] {}: clamped adaptive seed batch from {} to {} using {} bridge channels",
            config.service, cached_or_default_batch_size, seed.batch_size, seed.bridge_concurrency
        );
    } else if seed.batch_size > cached_or_default_batch_size {
        println!(
            "[renium] {}: raised adaptive seed batch from {} to {} using {} bridge channels",
            config.service, cached_or_default_batch_size, seed.batch_size, seed.bridge_concurrency
        );
    }
    let source = if seed.manual_batch {
        "manual-batch"
    } else if config.requested_workers > 0 {
        "manual"
    } else if config.cached_tune.is_some() {
        "cached"
    } else {
        "default"
    };
    println!(
        "[renium] {}: adaptive seed source={} cached_batch={} cached_workers={} default_batch={} auto_workers={} final_batch={} final_workers={} min_batch_floor={} reason={} lag_frame_ms={:.1}",
        config.service,
        source,
        cached_batch_size.map_or_else(|| "n/a".to_string(), |value| value.to_string()),
        cached_workers.map_or_else(|| "n/a".to_string(), |value| value.to_string()),
        default_batch_size,
        adaptive_default_workers(config),
        seed.batch_size,
        seed.workers,
        seed.min_batch_size,
        seed.reason,
        ADAPTIVE_LAG_FRAME_MS
    );
    if let Some(tune) = config.cached_tune {
        println!(
            "[renium] {}: cached adaptive tune frame_ms={} max_frame_ms={} stalls50={} age_s={} trusted={}",
            config.service,
            format_frame_ms(tune.frame_ms),
            format_frame_ms(tune.max_frame_ms),
            tune.stall_count_over_50_ms,
            current_unix_ts()
                .saturating_sub(tune.updated_at_unix)
                .to_string(),
            seed.trusted_cache
        );
    }
}

fn adaptive_fetch_seed(config: AdaptiveFetchConfig<'_>) -> AdaptiveSeed {
    let AdaptiveFetchConfig {
        instance_count,
        requested_workers,
        requested_batch,
        performance_mode,
        cached_tune,
        bridge_concurrency,
        ..
    } = config;
    let now_unix = current_unix_ts();
    let manual_batch = requested_batch > 0;
    let default_batch_size = instance_batch_defaults(instance_count).adaptive;
    let cached_batch_size = cached_tune
        .map(|tune| tune.batch_size)
        .filter(|value| *value > 0);
    let cached_or_default_batch_size = cached_batch_size.unwrap_or(default_batch_size);
    let min_batch_size =
        performance_mode.min_large_service_batch_size(instance_count, bridge_concurrency);
    let default_workers = adaptive_default_workers(config);
    let cached_workers = cached_tune
        .map(|tune| tune.workers)
        .filter(|value| *value > 0);
    let workers = if requested_workers > 0 {
        requested_workers
    } else if let Some(workers) = cached_workers {
        workers.max(default_workers)
    } else {
        default_workers
    }
    .min(bridge_concurrency);
    let mut batch_size = if manual_batch {
        requested_batch
    } else {
        cached_or_default_batch_size
    }
    .min(instance_count.max(1));
    let trust_cached_seed = adaptive_cached_seed_is_trusted(config, min_batch_size, now_unix);
    let mut seed_reason = if manual_batch {
        "manual seed override"
    } else if requested_workers > 0 {
        "manual workers"
    } else if cached_tune.is_some() {
        "cached tune"
    } else {
        "default sizing"
    };
    if !manual_batch
        && instance_count >= LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES
        && !trust_cached_seed
    {
        let max_total_chunks = bridge_concurrency
            .saturating_mul(INITIAL_SEED_CHUNKS_PER_BRIDGE_MAX)
            .min(16);
        let min_total_chunks = bridge_concurrency
            .saturating_mul(INITIAL_SEED_CHUNKS_PER_BRIDGE_MIN)
            .min(12);
        let min_seed_batch = instance_count.div_ceil(max_total_chunks);
        let max_seed_batch = instance_count.div_ceil(min_total_chunks);
        batch_size = batch_size.clamp(min_seed_batch, max_seed_batch);
        if batch_size != cached_or_default_batch_size {
            seed_reason = "bridge seed window";
        }
    } else if trust_cached_seed {
        seed_reason = "healthy cached tune";
    }
    if min_batch_size > 0 && batch_size < min_batch_size {
        batch_size = min_batch_size;
        seed_reason = match performance_mode {
            PerformanceMode::Throughput => "throughput floor",
            PerformanceMode::Balanced => "balanced floor",
            PerformanceMode::Smooth => seed_reason,
        };
    }
    let seed = AdaptiveSeed {
        manual_batch,
        bridge_concurrency,
        min_batch_size,
        batch_size,
        workers,
        reason: seed_reason,
        trusted_cache: trust_cached_seed,
    };
    log_adaptive_seed(config, &seed);
    seed
}

struct AdaptiveWavePlan {
    remaining: usize,
    logical_batch_size: usize,
    batch_size: usize,
    workers: usize,
    ranges: Vec<(usize, usize, usize)>,
}

fn plan_adaptive_wave(
    config: AdaptiveFetchConfig<'_>,
    seed: &AdaptiveSeed,
    total_hint: usize,
    next_start: &mut usize,
    batch_size: usize,
    worker_target: usize,
) -> AdaptiveWavePlan {
    let remaining = total_hint - *next_start + 1;
    let enforce_full_channel_use = !seed.manual_batch
        && config.requested_workers == 0
        && config.instance_count >= LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES
        && remaining >= seed.bridge_concurrency;
    let mut wave_batch_size = batch_size.min(remaining);
    if enforce_full_channel_use {
        let target_ranges = remaining.min(seed.bridge_concurrency);
        wave_batch_size = wave_batch_size.min(remaining.div_ceil(target_ranges));
    }
    let logical_batch_size = wave_batch_size;
    let max_wave_workers = remaining.div_ceil(wave_batch_size);
    let mut workers = worker_target.min(max_wave_workers);
    if enforce_full_channel_use {
        workers = workers.max(seed.bridge_concurrency.min(max_wave_workers));
    }
    let dynamic_ranges = config.instance_count >= LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES
        && workers > 1
        && remaining > wave_batch_size;
    let mut item_budget = remaining.min(wave_batch_size.saturating_mul(workers));
    if remaining > item_budget {
        let leftover = remaining - item_budget;
        let fold_tail_threshold = (wave_batch_size / 2).max(256).min(wave_batch_size);
        if leftover <= fold_tail_threshold {
            item_budget = remaining;
        }
    }
    if dynamic_ranges {
        let target_ranges = workers
            .saturating_mul(DYNAMIC_RANGES_PER_WORKER)
            .clamp(workers, item_budget);
        wave_batch_size = item_budget
            .div_ceil(target_ranges)
            .max(DYNAMIC_RANGE_MIN_INSTANCES.min(item_budget));
    }
    let range_count = item_budget.div_ceil(wave_batch_size);
    workers = workers.min(range_count);
    let mut ranges = Vec::with_capacity(range_count);
    let mut scheduled_items = 0;
    while scheduled_items < item_budget && *next_start <= total_hint {
        let remaining_budget = item_budget - scheduled_items;
        let take = (total_hint - *next_start + 1)
            .min(wave_batch_size)
            .min(remaining_budget);
        ranges.push((ranges.len(), *next_start, take));
        *next_start += take;
        scheduled_items += take;
    }
    AdaptiveWavePlan {
        remaining,
        logical_batch_size,
        batch_size: wave_batch_size,
        workers,
        ranges,
    }
}

struct AdaptiveWaveMetrics {
    bytes: usize,
    requests: usize,
    avg_request_ms: f64,
    max_request_ms: f64,
    avg_request_bytes: f64,
    max_request_bytes: usize,
    chunks: ChunkFetchMetrics,
    expand_ms: f64,
    items_fetched: usize,
}

fn merge_adaptive_wave(
    fetched: Vec<(usize, InstanceBatchFetch)>,
    total_hint: &mut usize,
    instances: &mut Vec<SnapshotInstance>,
) -> AdaptiveWaveMetrics {
    debug_assert!(
        fetched.windows(2).all(|items| items[0].0 <= items[1].0),
        "parallel adaptive instance batches must preserve range order"
    );
    let bytes = fetched.iter().map(|(_, batch)| batch.metrics.bytes).sum();
    let requests = fetched.len();
    let total_request_ms = fetched
        .iter()
        .map(|(_, batch)| batch.request_ms)
        .sum::<f64>();
    let max_request_ms = fetched
        .iter()
        .map(|(_, batch)| batch.request_ms)
        .fold(0.0, f64::max);
    let avg_request_ms = if requests > 0 {
        total_request_ms / requests as f64
    } else {
        0.0
    };
    let max_request_bytes = fetched
        .iter()
        .map(|(_, batch)| batch.metrics.bytes)
        .max()
        .unwrap_or(0);
    let avg_request_bytes = if requests > 0 {
        bytes as f64 / requests as f64
    } else {
        0.0
    };
    let mut chunks = ChunkFetchMetrics::default();
    let mut expand_ms = 0.0;
    let mut items_fetched = 0usize;
    for (_, batch) in fetched {
        *total_hint = (*total_hint).max(batch.total_hint);
        merge_chunk_fetch_metrics(&mut chunks, batch.metrics);
        expand_ms += batch.compact_expand_ms;
        let mut items = batch.items;
        items_fetched = items_fetched.saturating_add(items.len());
        instances.append(&mut items);
    }
    AdaptiveWaveMetrics {
        bytes,
        requests,
        avg_request_ms,
        max_request_ms,
        avg_request_bytes,
        max_request_bytes,
        chunks,
        expand_ms,
        items_fetched,
    }
}

impl AdaptiveWaveMetrics {
    fn log(
        &self,
        config: AdaptiveFetchConfig<'_>,
        wave_index: usize,
        progress: (usize, usize),
        plan: &AdaptiveWavePlan,
        wave_ms: f64,
        perf_stats: Option<&BridgePerformanceStats>,
    ) {
        let service = config.service;
        if verbose_timing_logs() {
            let frame_ms = perf_stats.and_then(|stats| stats.frame_ms);
            println!(
                "[renium] {service}: adaptive wave {} -> instances {}/{} (batch={}, workers={}, wave_ms={:.0}, bytes={:.1}MB, frame_ms={})",
                wave_index,
                progress.0,
                progress.1,
                plan.batch_size,
                plan.workers,
                wave_ms,
                self.bytes as f64 / (1024.0 * 1024.0),
                format_frame_ms(frame_ms)
            );
            println!(
                "[renium] {service}: adaptive wave {} perf stats -> last_frame_ms={}, max_frame_ms={}, stalls33={}, stalls50={}, stalls100={}",
                wave_index,
                format_frame_ms(perf_stats.and_then(|stats| stats.last_frame_ms)),
                format_frame_ms(perf_stats.and_then(|stats| stats.max_frame_ms)),
                format_stall_count(perf_stats.and_then(|stats| stats.stall_count_over_33_ms)),
                format_stall_count(perf_stats.and_then(|stats| stats.stall_count_over_50_ms)),
                format_stall_count(perf_stats.and_then(|stats| stats.stall_count_over_100_ms))
            );
            println!(
                "[renium] {service}: adaptive wave {} export metrics -> modified_checks={}, modified_elided={}, modified_validation_reads={}, modified_denylist={}, properties_read={}, properties_encoded={}, properties_default_skipped={}, pcall_class_fallbacks={}, pcall_property_fallbacks={}",
                wave_index,
                perf_stats
                    .and_then(|stats| stats.modified_default_checks)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.modified_default_elided)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.modified_default_validation_reads)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.modified_default_runtime_denylist_count)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.properties_read)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.properties_encoded)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.properties_default_skipped)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.safe_read_class_fallback_count)
                    .unwrap_or(0),
                perf_stats
                    .and_then(|stats| stats.safe_read_property_fallback_count)
                    .unwrap_or(0),
            );
            println!(
                "[renium] {service}: adaptive wave {} request stats -> requests={}, avg_req_ms={:.1}, max_req_ms={:.1}, avg_req_mb={:.1}, max_req_mb={:.1}",
                wave_index,
                self.requests,
                self.avg_request_ms,
                self.max_request_ms,
                self.avg_request_bytes / (1024.0 * 1024.0),
                self.max_request_bytes as f64 / (1024.0 * 1024.0)
            );
        }
        log_chunk_fetch_metrics(
            &format!("{service}: adaptive wave {wave_index} payloads"),
            self.chunks,
        );
        log_timing_ms(
            &format!("{service}: adaptive wave {wave_index} compact expansion"),
            self.expand_ms,
        );
    }
}

fn log_adaptive_batch_reduction(
    service: &str,
    wave_index: usize,
    underused_bridge: bool,
    imbalanced_requests: bool,
    slow_requests: bool,
) {
    if verbose_timing_logs() {
        println!(
            "[renium] {service}: adaptive wave {} reducing next batch due to{}{}{}",
            wave_index,
            if underused_bridge {
                " underused bridge"
            } else {
                ""
            },
            if imbalanced_requests {
                " oversized request skew"
            } else {
                ""
            },
            if slow_requests {
                " slow max request"
            } else {
                ""
            }
        );
    }
}

struct InstanceBatchContext<'a> {
    bridge: &'a BridgeServer,
    service: &'a str,
    chunk_size: usize,
    instance_count: usize,
    property_schema_by_class: &'a PropertySchemaMap,
    enum_value_names_by_type: &'a EnumValueNameMap,
    class_names: &'a [String],
}

impl InstanceBatchContext<'_> {
    fn fetch_fixed(
        &self,
        instance_batch_size: usize,
        instance_worker_count: usize,
    ) -> Result<Vec<SnapshotInstance>> {
        let service = self.service;
        let instance_count = self.instance_count;
        let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
        let mut range_index = 0usize;
        let mut start = 1usize;
        while start <= instance_count {
            let take = (instance_count - start + 1).min(instance_batch_size);
            ranges.push((range_index, start, take));
            range_index += 1;
            start += take;
        }

        let mut instances: Vec<SnapshotInstance> = Vec::with_capacity(instance_count);
        if ranges.is_empty() {
            if verbose_timing_logs() {
                println!("[renium] {service}: instances 0/0");
            }
            return Ok(instances);
        }

        let mut total_hint = instance_count;
        let mut chunk_metrics = ChunkFetchMetrics::default();
        let mut compact_expand_ms = 0.0;
        if instance_worker_count <= 1 || ranges.len() <= 1 {
            for (range_idx, start_index, take_count) in ranges {
                let batch = self.fetch(start_index, take_count)?;
                total_hint = total_hint.max(batch.total_hint);
                compact_expand_ms += batch.compact_expand_ms;
                merge_chunk_fetch_metrics(&mut chunk_metrics, batch.metrics);
                let mut items = batch.items;
                instances.append(&mut items);

                if (range_idx + 1) % 4 == 0
                    && range_idx + 1 < total_hint.div_ceil(instance_batch_size)
                    && verbose_timing_logs()
                {
                    println!(
                        "[renium] {service}: instances {}/{}",
                        instances.len(),
                        total_hint
                    );
                }
            }
        } else {
            let total_ranges = ranges.len();
            let progress_batches = std::sync::atomic::AtomicUsize::new(0);
            let progress_instances = std::sync::atomic::AtomicUsize::new(0);
            let progress_stride = (total_ranges / 12).max(1);

            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(instance_worker_count)
                .build()
                .context("Failed to create instance batch worker pool")?;
            let fetched = pool.install(|| {
            ranges
                .par_iter()
                .map(
                    |(range_index, start_index, take_count)| -> Result<(usize, InstanceBatchFetch)> {
                        let batch = self.fetch(*start_index, *take_count)?;

                        let done_batches =
                            progress_batches.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let done_instances = progress_instances
                            .fetch_add(batch.items.len(), std::sync::atomic::Ordering::Relaxed)
                            + batch.items.len();
                        if verbose_timing_logs()
                            && (done_batches.is_multiple_of(progress_stride) || done_batches == total_ranges)
                        {
                            println!(
                                "[renium] {service}: instances {}/{} (batches {}/{})",
                                done_instances,
                                batch.total_hint.max(instance_count),
                                done_batches,
                                total_ranges
                            );
                        }

                        Ok((*range_index, batch))
                    },
                )
                .collect::<Result<Vec<_>>>()
        })?;

            debug_assert!(
                fetched.windows(2).all(|items| items[0].0 <= items[1].0),
                "parallel fixed instance batches must preserve range order"
            );
            for (_, batch) in fetched {
                total_hint = total_hint.max(batch.total_hint);
                compact_expand_ms += batch.compact_expand_ms;
                merge_chunk_fetch_metrics(&mut chunk_metrics, batch.metrics);
                let mut items = batch.items;
                instances.append(&mut items);
            }
        }

        if verbose_timing_logs() {
            println!(
                "[renium] {service}: instances {}/{}",
                instances.len(),
                total_hint
            );
        }
        log_chunk_fetch_metrics(&format!("{service}: instance payloads"), chunk_metrics);
        log_timing_ms(
            &format!("{service}: compact instance expansion"),
            compact_expand_ms,
        );

        Ok(instances)
    }
}

impl InstanceBatchContext<'_> {
    fn fetch_adaptive_wave(
        &self,
        ranges: &[(usize, usize, usize)],
        workers: usize,
        pool: Option<&rayon::ThreadPool>,
    ) -> Result<Vec<(usize, InstanceBatchFetch)>> {
        if ranges.len() <= 1 || workers <= 1 {
            return ranges
                .iter()
                .map(|(range_index, start_index, take_count)| {
                    Ok((*range_index, self.fetch(*start_index, *take_count)?))
                })
                .collect();
        }
        let pool = pool.context("Adaptive instance batch worker pool was not initialized")?;
        pool.install(|| {
            ranges
                .par_iter()
                .map(|(range_index, start_index, take_count)| {
                    Ok((*range_index, self.fetch(*start_index, *take_count)?))
                })
                .collect()
        })
    }

    fn fetch_adaptive(
        &self,
        requested_instance_workers: usize,
        adaptive_seed_batch: usize,
        performance_mode: PerformanceMode,
        cached_tune: Option<&AdaptiveTuneEntry>,
    ) -> Result<InstanceFetchResult> {
        let bridge = self.bridge;
        let service = self.service;
        let instance_count = self.instance_count;
        if instance_count == 0 {
            if verbose_timing_logs() {
                println!("[renium] {service}: instances 0/0");
            }
            return Ok(InstanceFetchResult {
                instances: Vec::new(),
                tune: None,
            });
        }

        let fetch_config = AdaptiveFetchConfig {
            service,
            instance_count,
            requested_workers: requested_instance_workers,
            requested_batch: adaptive_seed_batch,
            performance_mode,
            cached_tune,
            bridge_concurrency: bridge.channel_count().max(1),
        };
        let seed = adaptive_fetch_seed(fetch_config);
        let manual_seed_batch = seed.manual_batch;
        let bridge_concurrency = seed.bridge_concurrency;
        let min_large_service_batch_size = seed.min_batch_size;
        let mut batch_size = seed.batch_size;
        let mut worker_target = seed.workers;
        let mut next_start = 1usize;
        let mut total_hint = instance_count;
        let mut wave_index = 0usize;
        let mut instances = Vec::with_capacity(instance_count);
        let mut last_perf_stats: Option<BridgePerformanceStats> = None;
        let mut best_measured_tune: Option<AdaptiveTuneEntry> = None;
        let skip_tune_cache = manual_seed_batch || requested_instance_workers > 0;
        let collect_wave_perf_stats = performance_mode != PerformanceMode::Throughput;
        let adaptive_pool = if bridge_concurrency > 1 {
            Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(bridge_concurrency)
                    .build()
                    .context("Failed to create adaptive instance batch worker pool")?,
            )
        } else {
            None
        };

        while next_start <= total_hint {
            let plan = plan_adaptive_wave(
                fetch_config,
                &seed,
                total_hint,
                &mut next_start,
                batch_size,
                worker_target,
            );
            if collect_wave_perf_stats {
                let _ = read_bridge_performance_stats(bridge);
            }
            let wave_started = Instant::now();
            let fetched =
                self.fetch_adaptive_wave(&plan.ranges, plan.workers, adaptive_pool.as_ref())?;

            let wave_metrics = merge_adaptive_wave(fetched, &mut total_hint, &mut instances);

            wave_index += 1;
            let wave_ms = wave_started.elapsed().as_secs_f64() * 1000.0;
            let perf_stats = if collect_wave_perf_stats {
                read_bridge_performance_stats(bridge)
            } else {
                None
            };
            let frame_ms = perf_stats.as_ref().and_then(|stats| stats.frame_ms);
            let max_frame_ms = perf_stats.as_ref().and_then(|stats| stats.max_frame_ms);
            let stall_count_over_50_ms = perf_stats
                .as_ref()
                .and_then(|stats| stats.stall_count_over_50_ms);
            let lagging = perf_stats.as_ref().is_some_and(|stats| {
                stats
                    .max_frame_ms
                    .or(stats.frame_ms)
                    .is_some_and(|ms| ms >= ADAPTIVE_LAG_FRAME_MS)
                    || stats.stall_count_over_50_ms.unwrap_or(0) > 0
            });
            wave_metrics.log(
                fetch_config,
                wave_index,
                (instances.len(), total_hint),
                &plan,
                wave_ms,
                perf_stats.as_ref(),
            );
            let measured_tune = AdaptiveTuneEntry {
                batch_size: plan.logical_batch_size,
                workers: plan.workers,
                instance_count: total_hint,
                frame_ms,
                max_frame_ms,
                wave_ms: Some(wave_ms),
                payload_bytes: wave_metrics.chunks.bytes,
                request_count: wave_metrics.requests,
                items_fetched: wave_metrics.items_fetched,
                stall_count_over_50_ms: stall_count_over_50_ms.unwrap_or(0),
                updated_at_unix: current_unix_ts(),
            };
            if best_measured_tune.as_ref().is_none_or(|current| {
                adaptive_tune_is_better(&measured_tune, current, performance_mode)
            }) {
                best_measured_tune = Some(measured_tune);
            }

            let underused_bridge = !manual_seed_batch
                && requested_instance_workers == 0
                && instance_count >= LARGE_SERVICE_SINGLE_WAVE_MIN_INSTANCES
                && wave_metrics.requests < bridge_concurrency
                && plan.remaining >= bridge_concurrency;
            let imbalanced_requests = wave_metrics.requests > 1
                && wave_metrics.max_request_bytes as f64 > wave_metrics.avg_request_bytes * 1.35;
            let slow_requests = !lagging && wave_metrics.max_request_ms >= 1000.0;
            if lagging {
                batch_size = plan.batch_size.saturating_mul(3).div_ceil(4);
                worker_target = worker_target.saturating_mul(3).div_ceil(4);
            } else if underused_bridge || imbalanced_requests || slow_requests {
                batch_size = plan.batch_size.saturating_mul(3).div_ceil(4);
                if requested_instance_workers == 0 {
                    worker_target = worker_target.max(bridge_concurrency);
                }
                log_adaptive_batch_reduction(
                    service,
                    wave_index,
                    underused_bridge,
                    imbalanced_requests,
                    slow_requests,
                );
            } else {
                let batch_step = (plan.batch_size / ADAPTIVE_BATCH_GROWTH_DIVISOR).max(1);
                batch_size = plan.batch_size.saturating_add(batch_step).min(total_hint);
                if wave_index.is_multiple_of(ADAPTIVE_WORKER_GROWTH_WAVE_INTERVAL) {
                    worker_target = worker_target.saturating_add(1);
                }
            }
            if min_large_service_batch_size > 0 {
                batch_size = batch_size.max(min_large_service_batch_size);
            }
            worker_target = worker_target.min(bridge_concurrency);
            last_perf_stats = perf_stats;
        }

        if verbose_timing_logs() {
            println!(
                "[renium] {service}: instances {}/{}",
                instances.len(),
                total_hint
            );
        }
        Ok(InstanceFetchResult {
            instances,
            tune: if skip_tune_cache {
                None
            } else {
                best_measured_tune.map(|mut tune| {
                    tune.instance_count = total_hint;
                    if let Some(perf_stats) = last_perf_stats.as_ref() {
                        tune.frame_ms = perf_stats.frame_ms.or(tune.frame_ms);
                        tune.max_frame_ms = perf_stats.max_frame_ms.or(tune.max_frame_ms);
                        tune.stall_count_over_50_ms = perf_stats
                            .stall_count_over_50_ms
                            .unwrap_or(tune.stall_count_over_50_ms);
                    }
                    tune.updated_at_unix = current_unix_ts();
                    tune
                })
            },
        })
    }
}

impl InstanceBatchContext<'_> {
    fn fetch(&self, start_index: usize, take_count: usize) -> Result<InstanceBatchFetch> {
        let mut fetch = self.fetch_once(start_index, take_count)?;
        loop {
            let fetched = fetch.items.len();
            if fetched == 0 || fetched >= take_count {
                break;
            }
            let next_start = start_index + fetched;
            let range_end = (start_index + take_count - 1).min(fetch.total_hint);
            if next_start > range_end {
                break;
            }
            let remainder = self.fetch_once(next_start, range_end - next_start + 1)?;
            if remainder.items.is_empty() {
                break;
            }
            fetch.total_hint = fetch.total_hint.max(remainder.total_hint);
            merge_chunk_fetch_metrics(&mut fetch.metrics, remainder.metrics);
            fetch.compact_expand_ms += remainder.compact_expand_ms;
            fetch.request_ms += remainder.request_ms;
            let mut items = remainder.items;
            fetch.items.append(&mut items);
        }
        Ok(fetch)
    }
}

impl InstanceBatchContext<'_> {
    fn fetch_once(&self, start_index: usize, take_count: usize) -> Result<InstanceBatchFetch> {
        let bridge = self.bridge;
        let service = self.service;
        let started = Instant::now();
        let (mut batch, metrics) = fetch_typed_payload_with_size::<CompactBatchPayload, _>(
            self.chunk_size,
            |chunk_start, max_len| {
                bridge.call_chunk(
                    "getInstanceBatchCompactChunk",
                    json!({
                        "service": service,
                        "startIndex": start_index,
                        "maxCount": take_count,
                        "chunkStart": chunk_start,
                        "maxLen": max_len,
                    }),
                )
            },
        )?;

        let shape_batch = batch.format == "compact-v5-shape";
        let debug_ids =
            decode_compact_batch_debug_ids(std::mem::take(&mut batch.debug_ids), &batch.strings)
                .with_context(|| {
                    format!("Invalid compact debug id batch item schema for {service}")
                })?;
        let raw_items = Value::Array(batch.items);
        if !shape_batch && batch.format != BRIDGE_PROTOCOL_VERSION {
            bail!(
                "Invalid instance batch format {} for {service}",
                batch.format
            );
        }
        if !is_supported_bridge_codec(&batch.codec_version) {
            bail!(
                "Invalid {} codec {} for {} (expected {} or {})",
                batch.format,
                if batch.codec_version.is_empty() {
                    "missing"
                } else {
                    batch.codec_version.as_str()
                },
                service,
                BRIDGE_CODEC_VERSION,
                BRIDGE_CODEC_VERSION_SCHEMA8
            );
        }
        let compact_expand_started = Instant::now();
        let mut out = if shape_batch {
            parse_compact_v5_shape_instance_items(
                raw_items,
                &batch.strings,
                batch.shapes,
                start_index,
                self.property_schema_by_class,
                self.enum_value_names_by_type,
                self.class_names,
            )
            .with_context(|| {
                format!("Invalid compact-v5 shape instance batch item schema for {service}")
            })?
        } else {
            parse_compact_v5_instance_items(
                raw_items,
                &batch.strings,
                start_index,
                self.property_schema_by_class,
                self.enum_value_names_by_type,
                self.class_names,
            )
            .with_context(|| {
                format!("Invalid compact-v5 instance batch item schema for {service}")
            })?
        };
        apply_compact_batch_debug_ids(&mut out, debug_ids);

        let total_hint = batch.total.max(self.instance_count);

        Ok(InstanceBatchFetch {
            total_hint,
            metrics,
            compact_expand_ms: elapsed_ms(compact_expand_started),
            request_ms: elapsed_ms(started),
            items: out,
        })
    }
}

fn read_bridge_performance_stats(bridge: &BridgeServer) -> Option<BridgePerformanceStats> {
    bridge
        .call("getPerformanceStats", json!({}))
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn format_frame_ms(frame_ms: Option<f64>) -> String {
    frame_ms.map_or_else(|| "n/a".to_string(), |value| format!("{value:.1}"))
}

fn format_stall_count(stall_count: Option<u64>) -> String {
    stall_count.map_or_else(|| "n/a".to_string(), |value| value.to_string())
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
    if metrics.chunks == 0 || quiet_timings() {
        return;
    }
    println!(
        "[renium] timing: {label} chunk metrics -> chunks={}, bytes={}, max_chunk_bytes={}, plugin_server_ms={:.1}, plugin_encode_ms={:.1}, reassembly_ms={:.1}, json_parse_ms={:.1}",
        metrics.chunks,
        metrics.bytes,
        metrics.max_chunk_bytes,
        metrics.plugin_server_ms,
        metrics.plugin_encode_ms,
        metrics.reassembly_ms,
        metrics.json_parse_ms
    );
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
    metrics.reassembly_ms = elapsed_ms(reassembly_started);
    Ok((output, metrics))
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

fn merge_script_sources(instances: &mut [SnapshotInstance], source_map: &SourceBatchMap) {
    for instance in instances.iter_mut() {
        let is_script_class = matches!(
            instance.class_name.as_str(),
            "Script" | "LocalScript" | "ModuleScript"
        );
        if !is_script_class {
            instance.source_key = None;
            continue;
        }

        let source = instance
            .instance_index
            .and_then(|instance_index| source_map.by_index.get(&instance_index))
            .or_else(|| {
                instance
                    .source_key
                    .as_deref()
                    .and_then(|source_key| source_map.by_key.get(source_key))
            })
            .or_else(|| {
                instance
                    .instance_id
                    .as_deref()
                    .and_then(|instance_id| source_map.by_key.get(&format!("id:{instance_id}")))
            })
            .or_else(|| {
                instance.instance_index.and_then(|instance_index| {
                    source_map.by_key.get(&format!("id:{instance_index:x}"))
                })
            })
            .or_else(|| {
                instance
                    .debug_id
                    .as_deref()
                    .and_then(|debug_id| source_map.by_key.get(&format!("debug:{debug_id}")))
            })
            .or_else(|| source_map.by_key.get(&instance.path));
        if let Some(source) = source {
            instance
                .properties
                .insert("Source".to_string(), Value::String(source.clone()));
        } else if is_script_class && instance.source_key.is_some() {
            instance
                .properties
                .entry("Source".to_string())
                .or_insert_with(|| Value::String("__SOURCE_EXTERNAL__".to_string()));
        }
        instance.source_key = None;
    }
}
