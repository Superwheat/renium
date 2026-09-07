use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::BoundContext;
use super::context as bound_context;
use super::runtime::{acknowledge_pulled_changes, automation_pull_args, automation_push_args};
use crate::app::output::{ensure_plugin_api_ok, global_log_enabled, log_global};
use crate::app::timing::elapsed_ms;
use crate::cli::PushEditorChangesArgs;
use crate::editor::diff::editor_instance_descriptor_for_known_path;
use crate::editor::paths::{
    build_editor_instance_paths_for_indices, build_editor_source_paths_by_index,
};
use crate::editor::review::local_place_path_for_runtime;
use crate::editor::sync::{
    StudioChangeGuard, expand_editor_changed_paths, is_lua_source_class,
    push_reconciled_editor_changes_with_warm_bridge, settings_file_hash,
};
use crate::editor::types::{
    EditorChangeSet, EditorInstanceChange, EditorInstancePath, EditorPropertyChange,
};
use crate::project::version_control::{VcMergeConflict, merge_aligned_settings_documents};
use crate::project::{config, config::project_watch_inputs};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
    encode_settings_bytecode,
};
use crate::settings::equivalence::{
    align_equivalent_values, align_reconciliation_protected_workspace_cameras,
    align_settings_ids_to_reference, canonicalize_settings_property_names, drop_settings_document,
    drop_settings_documents, is_reconciliation_protected_workspace_camera,
    reconciliation_maps_equal, reconciliation_property_is_derived, reconciliation_property_value,
    reconciliation_property_values_equal, reconciliation_values_equal,
    reconciliation_values_map_equal, remove_reconciliation_derived_properties,
    settings_documents_equivalent, settings_documents_positionally_equivalent,
    stabilize_settings_reference_ids,
};
use crate::snapshot::export::{
    ExportProjectStage, PublishEntryState, export_snapshots_with_warm_bridge,
};
use crate::snapshot::refs::remap_record_reference_ids;
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::files::{
    OnDrop, absolutize_under, atomic_write_file, canonical_path, create_unique_directory, fnv1a,
    is_service_settings_file_name, service_settings_path,
};

mod store;

use store::StoredSnapshot;

const RECORD_VERSION: u8 = 2;
const RECORD_DIR: &str = "reconcile";

fn log_reconcile_timing(label: &str, started: Instant) {
    log_global(
        4,
        format_args!("[renium] reconcile {label}: {:.1}ms", elapsed_ms(started)),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PairMode {
    Reconcile,
    Verify,
}

impl PairMode {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        if matches!(value, Some("none" | "verify")) {
            Self::Verify
        } else {
            Self::Reconcile
        }
    }

    pub(crate) fn writes(self) -> bool {
        self == Self::Reconcile
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Verify => "verify",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConflictPreference {
    None,
    Studio,
    Editor,
}

impl ConflictPreference {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            Some("studio") => Self::Studio,
            Some("editor") => Self::Editor,
            _ => Self::None,
        }
    }
}

pub(crate) struct PairConfiguration {
    pub(crate) mode: PairMode,
    pub(crate) conflict_preference: ConflictPreference,
    pub(crate) resolution_preference: Option<ConflictPreference>,
    pub(crate) runtime_settings: Map<String, Value>,
}

#[derive(Clone, Copy)]
pub(crate) enum BaselineSide {
    Editor,
    Studio,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PairIdentity {
    experience: String,
    project: String,
    fingerprint: String,
    game_id: Option<i64>,
    place_id: Option<i64>,
    #[serde(default)]
    local_file: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LocalFileStamp {
    length: u64,
    modified_seconds: u64,
    modified_nanos: u32,
}

fn local_file_stamp(path: Option<&str>) -> Result<Option<LocalFileStamp>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect local place file {path}"));
        }
    };
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read local place timestamp for {path}"))?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Ok(Some(LocalFileStamp {
        length: metadata.len(),
        modified_seconds: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
    }))
}

fn local_file_digest(path: Option<&str>) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to open local place file {path}"));
        }
    };
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read local place file {path}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

fn should_bootstrap_studio_from_editor(
    mode: PairMode,
    previous_runtime_id: Option<&str>,
    current_runtime_id: Option<&str>,
    previous_local_file_digest: Option<&str>,
    current_local_file_digest: Option<&str>,
) -> bool {
    mode == PairMode::Reconcile
        && previous_runtime_id.is_some()
        && previous_runtime_id != current_runtime_id
        && previous_local_file_digest.is_some()
        && previous_local_file_digest == current_local_file_digest
}

fn legacy_local_file_stamp_matches(
    mode: PairMode,
    runtime_replaced: bool,
    previous_local_file_digest: Option<&str>,
    previous_local_file_stamp: Option<&LocalFileStamp>,
    current_local_file_stamp: Option<&LocalFileStamp>,
) -> bool {
    mode == PairMode::Reconcile
        && runtime_replaced
        && previous_local_file_digest.is_none()
        && previous_local_file_stamp.is_some()
        && previous_local_file_stamp == current_local_file_stamp
}

impl PairIdentity {
    fn from_context(context: &BoundContext, bridge: &BridgeServer) -> Result<Self> {
        let published = context.game_id.is_some_and(|value| value > 0)
            && context.place_id.is_some_and(|value| value > 0);
        Ok(Self {
            experience: canonical_string(Path::new(&context.experience))?,
            project: canonical_string(Path::new(&context.root))?,
            fingerprint: context.fingerprint.clone(),
            game_id: context.game_id.filter(|value| *value > 0),
            place_id: context.place_id.filter(|value| *value > 0),
            local_file: if published {
                None
            } else {
                context
                    .runtime_id
                    .as_deref()
                    .and_then(|runtime_id| local_place_path_for_runtime(bridge, runtime_id))
                    .or(saved_local_file_for_context(context)?)
                    .map(|path| canonical_string(&path))
                    .transpose()?
            },
        })
    }

    fn pair_key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.experience.as_bytes());
        hash.update([0]);
        hash.update(self.project.as_bytes());
        hash.update([0]);
        hash.update(self.game_id.unwrap_or_default().to_le_bytes());
        hash.update(self.place_id.unwrap_or_default().to_le_bytes());
        if let Some(local_file) = &self.local_file {
            hash.update([0]);
            hash.update(local_file.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    fn same_pair(&self, other: &Self) -> bool {
        self.experience == other.experience
            && self.project == other.project
            && self.game_id == other.game_id
            && self.place_id == other.place_id
            && self.local_file == other.local_file
    }

    fn target_key(&self) -> String {
        match (self.game_id, self.place_id) {
            (Some(game_id), Some(place_id)) => format!("published:{game_id}:{place_id}"),
            _ => self.local_file.as_ref().map_or_else(
                || format!("local-unresolved:{}", self.project),
                |path| format!("local-file:{path}"),
            ),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
    Symlink { target: PathBuf, directory: bool },
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectSnapshot {
    entries: BTreeMap<PathBuf, SnapshotEntry>,
}

#[derive(Default)]
struct ReconcilePushPlan {
    changed_paths: Vec<PathBuf>,
    target_settings_ids: Vec<String>,
    recreated_settings_ids: HashSet<String>,
    previous_class_names: HashMap<String, String>,
    previous_paths: HashMap<(String, String), EditorInstancePath>,
    instance_deletes: Vec<EditorInstanceChange>,
    property_removals: Vec<EditorPropertyChange>,
    geometry_properties: HashMap<(String, String), Vec<String>>,
}

struct PreparedEditorSettingsChange {
    previous: SettingsBytecode,
    current: SettingsBytecode,
}

#[derive(Default)]
struct MergeChanges {
    editor: HashSet<PathBuf>,
    studio: HashSet<PathBuf>,
}

struct MergedEntry {
    value: Option<SnapshotEntry>,
    editor_changed: bool,
    studio_changed: bool,
}

struct SnapshotSides<'a> {
    baseline: Option<&'a ProjectSnapshot>,
    editor: &'a ProjectSnapshot,
    studio: &'a ProjectSnapshot,
}

impl ReconcilePushPlan {
    fn is_empty(&self) -> bool {
        self.changed_paths.is_empty()
            && self.target_settings_ids.is_empty()
            && self.recreated_settings_ids.is_empty()
            && self.previous_class_names.is_empty()
            && self.previous_paths.is_empty()
            && self.instance_deletes.is_empty()
            && self.property_removals.is_empty()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct RecoveryHead {
    editor_before: String,
    studio_before: String,
    intended: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StudioCheckpoint {
    runtime_id: String,
    change_tracker_version: u64,
    seq: u64,
    service_generations: BTreeMap<String, u64>,
}

fn checkpoint_generations(state: &Value) -> Option<&Map<String, Value>> {
    state["checkpointGenerations"]
        .as_object()
        .or_else(|| state["serviceGenerations"].as_object())
}

impl StudioCheckpoint {
    fn from_state(context: &BoundContext, state: &Value) -> Option<Self> {
        let services = sync_services();
        if state["tracking"].as_bool() != Some(true)
            || state["trackedServices"].as_u64() != u64::try_from(services.len()).ok()
            || !state["dirtyServices"].as_array().is_some_and(Vec::is_empty)
            || !state["fullSyncServices"]
                .as_array()
                .is_some_and(Vec::is_empty)
        {
            return None;
        }
        let runtime_id = state["runtimeId"].as_str()?;
        if context.runtime_id.as_deref() != Some(runtime_id) {
            return None;
        }
        let generations = checkpoint_generations(state)?;
        if generations.len() != services.len() {
            return None;
        }
        let service_generations = services
            .into_iter()
            .map(|service| Some((service.clone(), generations.get(&service)?.as_u64()?)))
            .collect::<Option<_>>()?;
        Some(Self {
            runtime_id: runtime_id.to_string(),
            change_tracker_version: state["changeTrackerVersion"].as_u64()?,
            seq: state["seq"].as_u64()?,
            service_generations,
        })
    }

    fn matches_state(&self, context: &BoundContext, state: &Value) -> bool {
        Self::from_state(context, state).as_ref() == Some(self)
    }

    fn changed_services(&self, context: &BoundContext, state: &Value) -> Option<Vec<String>> {
        let services = sync_services();
        if state["tracking"].as_bool() != Some(true)
            || state["trackedServices"].as_u64() != u64::try_from(services.len()).ok()
            || state["runtimeId"].as_str()? != self.runtime_id
            || context.runtime_id.as_deref() != Some(self.runtime_id.as_str())
            || state["changeTrackerVersion"].as_u64()? != self.change_tracker_version
            || state["seq"].as_u64()? < self.seq
            || state["referencePathsMayChange"].as_bool() == Some(true)
        {
            return None;
        }
        let generations = checkpoint_generations(state)?;
        if generations.len() != services.len() {
            return None;
        }
        let allowed = services.iter().map(String::as_str).collect::<HashSet<_>>();
        let mut changed = BTreeSet::new();
        for key in ["dirtyServices", "fullSyncServices"] {
            for service in state[key].as_array()?.iter().map(Value::as_str) {
                let service = service?;
                if !allowed.contains(service) {
                    return None;
                }
                changed.insert(service.to_string());
            }
        }
        for service in &services {
            let current = generations.get(service)?.as_u64()?;
            if self.service_generations.get(service) != Some(&current) {
                changed.insert(service.clone());
            }
        }
        if state["seq"].as_u64()? != self.seq && changed.is_empty() {
            return None;
        }
        Some(changed.into_iter().collect())
    }
}

#[derive(Serialize, Deserialize)]
struct PairRecord {
    version: u8,
    identity: PairIdentity,
    mode: PairMode,
    conflict_preference: ConflictPreference,
    runtime_settings: Map<String, Value>,
    #[serde(default)]
    baseline: Option<StoredSnapshot>,
    #[serde(default)]
    head: Option<RecoveryHead>,
    #[serde(default)]
    conflicts: Vec<String>,
    #[serde(default)]
    resolution_required: bool,
    #[serde(default)]
    last_runtime_id: Option<String>,
    #[serde(default)]
    local_file_stamp: Option<LocalFileStamp>,
    #[serde(default)]
    local_file_digest: Option<String>,
    #[serde(default)]
    studio_checkpoint: Option<StudioCheckpoint>,
}

#[derive(Deserialize)]
struct LegacyPairRecord {
    version: u8,
    identity: PairIdentity,
    mode: PairMode,
    conflict_preference: ConflictPreference,
    runtime_settings: Map<String, Value>,
    #[serde(default)]
    baseline: Option<ProjectSnapshot>,
    #[serde(default, rename = "head")]
    _head: Option<RecoveryHead>,
    #[serde(default)]
    conflicts: Vec<String>,
    #[serde(default)]
    resolution_required: bool,
}

fn saved_pair_identities(root: &Path, experience: &Path) -> Result<Vec<PairIdentity>> {
    let record_dir = root.join(".renium").join(RECORD_DIR);
    let entries = match fs::read_dir(&record_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect {}", record_dir.display()));
        }
    };
    let project = canonical_string(root)?;
    let experience = canonical_string(experience)?;
    let mut identities = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("rmp")
        {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        let Ok(record) = rmp_serde::from_slice::<PairRecord>(&bytes) else {
            continue;
        };
        if record.version != RECORD_VERSION
            || record.identity.project != project
            || record.identity.experience != experience
        {
            continue;
        }
        identities.push(record.identity);
    }
    Ok(identities)
}

pub(super) fn saved_studio_target_for_root(
    root: &Path,
    experience: &Path,
) -> Result<Option<super::StudioReopenTarget>> {
    let mut targets = Vec::new();
    for identity in saved_pair_identities(root, experience)? {
        let file = identity
            .local_file
            .map(PathBuf::from)
            .filter(|path| path.is_file());
        let game_id = identity.game_id.filter(|id| *id > 0);
        let place_id = identity.place_id.filter(|id| *id > 0);
        if file.is_none() && place_id.is_none() {
            continue;
        }
        let target = super::StudioReopenTarget {
            file,
            game_id,
            place_id,
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    Ok((targets.len() == 1).then(|| targets.pop().unwrap()))
}

fn saved_local_file_for_context(context: &BoundContext) -> Result<Option<PathBuf>> {
    let files = saved_pair_identities(Path::new(&context.root), Path::new(&context.experience))?
        .into_iter()
        .filter_map(|identity| identity.local_file.map(PathBuf::from))
        .filter(|file| file.is_file())
        .collect::<HashSet<_>>();
    if files.len() > 1 {
        bail!(
            "More than one local Studio file is paired with this project; pass the file to rbx ro"
        );
    }
    Ok(files.into_iter().next())
}

#[derive(Default)]
struct TargetOwners {
    by_target: HashMap<String, (String, String)>,
    target_by_pair: HashMap<String, String>,
}

pub(crate) struct PairSetup {
    pub(crate) key: String,
    identity: PairIdentity,
    pub(crate) mode: PairMode,
    resolution_preference: Option<ConflictPreference>,
    pub(crate) resolution_required: bool,
    pub(crate) error: Option<String>,
    pub(crate) requires_reconcile: bool,
    runtime_id: Option<String>,
    local_file_stamp: Option<LocalFileStamp>,
    local_file_digest: Option<String>,
    bootstrap_studio_from_editor: bool,
    runtime_replacement_unproven: bool,
}

#[derive(Default)]
pub(crate) struct Coordinator {
    pairs: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    owners: Mutex<TargetOwners>,
}

impl Coordinator {
    pub(crate) fn saved_local_file(&self, context: &BoundContext) -> Result<Option<PathBuf>> {
        saved_local_file_for_context(context)
    }

    pub(crate) fn pair_key(&self, context: &BoundContext, bridge: &BridgeServer) -> Result<String> {
        Ok(PairIdentity::from_context(context, bridge)?.pair_key())
    }

    pub(crate) fn target_key(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<String> {
        Ok(PairIdentity::from_context(context, bridge)?.target_key())
    }

    pub(crate) fn target_owner(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<Option<String>> {
        let identity = PairIdentity::from_context(context, bridge)?;
        let pair = identity.pair_key();
        Ok(self
            .owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_target
            .get(&identity.target_key())
            .filter(|(owner_pair, _)| owner_pair != &pair)
            .map(|(_, owner_project)| owner_project.clone()))
    }

    fn pair_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut pairs = self.pairs.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            pairs
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub(crate) fn prepare(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        configuration: PairConfiguration,
    ) -> Result<PairSetup> {
        let PairConfiguration {
            mode: requested_mode,
            conflict_preference,
            resolution_preference,
            runtime_settings,
        } = configuration;
        let identity = PairIdentity::from_context(context, bridge)?;
        let current_runtime_id = context.runtime_id.clone();
        let current_local_file_stamp = local_file_stamp(identity.local_file.as_deref())?;
        let pair_key = identity.pair_key();
        let pair_lock = self.pair_lock(&pair_key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut mode = requested_mode;
        let unresolved_local = identity.game_id.is_none()
            && identity.place_id.is_none()
            && identity.local_file.is_none();
        if unresolved_local {
            mode = PairMode::Verify;
        }
        let owner_conflict = if mode.writes() {
            self.claim_target(&identity)
        } else {
            None
        };
        if owner_conflict.is_some() {
            mode = PairMode::Verify;
        }
        let existing = load_record(context, &pair_key)?.filter(|record| {
            record.version == RECORD_VERSION && record.identity.same_pair(&identity)
        });
        let record_missing = existing.is_none();
        let mut record = existing.unwrap_or(PairRecord {
            version: RECORD_VERSION,
            identity: identity.clone(),
            mode,
            conflict_preference,
            runtime_settings: Map::new(),
            baseline: None,
            head: None,
            conflicts: Vec::new(),
            resolution_required: false,
            last_runtime_id: None,
            local_file_stamp: None,
            local_file_digest: None,
            studio_checkpoint: None,
        });
        let runtime_replaced =
            record.last_runtime_id.is_some() && record.last_runtime_id != current_runtime_id;
        let current_local_file_digest = if runtime_replaced
            || record.local_file_digest.is_none()
            || record.local_file_stamp != current_local_file_stamp
        {
            local_file_digest(identity.local_file.as_deref())?
        } else {
            record.local_file_digest.clone()
        };
        let legacy_stamp_matches = legacy_local_file_stamp_matches(
            mode,
            runtime_replaced,
            record.local_file_digest.as_deref(),
            record.local_file_stamp.as_ref(),
            current_local_file_stamp.as_ref(),
        );
        let runtime_replacement_unproven = runtime_replaced
            && identity.local_file.is_some()
            && record.local_file_digest.is_none()
            && !legacy_stamp_matches;
        let bootstrap_studio_from_editor = legacy_stamp_matches
            || should_bootstrap_studio_from_editor(
                mode,
                record.last_runtime_id.as_deref(),
                current_runtime_id.as_deref(),
                record.local_file_digest.as_deref(),
                current_local_file_digest.as_deref(),
            );
        let configuration_changed = record.identity.fingerprint != identity.fingerprint;
        let obsolete_head = record.head.take().is_some();
        let record_changed = record_missing
            || obsolete_head
            || record.identity != identity
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || record.runtime_settings != runtime_settings
            || !runtime_replaced
                && (record.local_file_stamp != current_local_file_stamp
                    || record.local_file_digest != current_local_file_digest);
        let requires_reconcile = record_missing
            || configuration_changed
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || !record.conflicts.is_empty()
            || runtime_replaced;
        if configuration_changed {
            record.baseline = None;
            record.studio_checkpoint = None;
            record.conflicts.clear();
            record.resolution_required = false;
        }
        record.identity = identity.clone();
        record.mode = mode;
        record.conflict_preference = conflict_preference;
        record.runtime_settings = runtime_settings;
        if !runtime_replaced {
            record.local_file_stamp = current_local_file_stamp.clone();
            record.local_file_digest = current_local_file_digest.clone();
        }
        if record_changed {
            write_record(context, &pair_key, &record)?;
        }
        Ok(PairSetup {
            key: pair_key,
            identity,
            mode,
            resolution_preference,
            resolution_required: !unresolved_local
                && owner_conflict.is_none()
                && (record.resolution_required
                    || !record.conflicts.is_empty()
                        && conflict_preference == ConflictPreference::None),
            error: unresolved_local
                .then(|| {
                    "Renium could not prove which local place file is open, so this pair is verify-only"
                        .to_string()
                })
                .or_else(|| {
                    owner_conflict.map(|owner| {
                        format!("This Studio place is already owned by {owner}; this project is verify-only")
                    })
                }),
            requires_reconcile,
            runtime_id: current_runtime_id,
            local_file_stamp: current_local_file_stamp,
            local_file_digest: current_local_file_digest,
            bootstrap_studio_from_editor,
            runtime_replacement_unproven,
        })
    }

    pub(crate) fn claim_setup_target(&self, setup: &PairSetup) -> Option<String> {
        self.claim_target(&setup.identity)
    }

    pub(crate) fn reconcile(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        setup: &mut PairSetup,
    ) -> Result<()> {
        let _gate = bridge.acquire_request_gate();
        // Every operation that needs both locks takes the bridge gate first.
        // LiveLoop::execute_push already follows this order.
        let pair_lock = self.pair_lock(&setup.key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = load_record(context, &setup.key)?
            .context("Reconciliation state disappeared while starting Live Sync")?;
        let _selection = bound_context::select(context);
        let (studio_guard, studio_state) = current_studio_change_guard_with_state(context, bridge)?;
        if setup.runtime_replacement_unproven && setup.resolution_preference.is_none() {
            setup.mode = PairMode::Verify;
            setup.error = Some(
                "Renium cannot prove whether the local place file changed before this Studio restart; choose Studio or project files"
                    .to_string(),
            );
            setup.resolution_required = true;
            return Ok(());
        }
        if setup.bootstrap_studio_from_editor && !studio_guard.runtime_bootstrap_safe {
            setup.mode = PairMode::Verify;
            setup.error = Some(
                "Studio restarted before change tracking was active; Renium left both sides unchanged"
                    .to_string(),
            );
            setup.resolution_required = false;
            return Ok(());
        }
        log_global(
            5,
            format_args!(
                "[renium] clean restart checkpoint: stored={} matches={}",
                record.studio_checkpoint.is_some(),
                record
                    .studio_checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.matches_state(context, &studio_state))
            ),
        );
        if !setup.requires_reconcile
            && let (Some(checkpoint), Some(baseline)) =
                (record.studio_checkpoint.as_ref(), record.baseline.as_ref())
            && checkpoint.matches_state(context, &studio_state)
        {
            let phase = Instant::now();
            let stage = project_comparison_stage(context, &sync_services())?;
            let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
            drop(stage);
            log_reconcile_timing("clean restart comparison", phase);
            let differences = if baseline.matches(&editor) {
                HashSet::new()
            } else {
                let previous = baseline.load(Path::new(&context.root), &setup.key)?;
                snapshot_differences(&previous, &editor)?
            };
            let confirmed = read_studio_change_state(context, bridge)?;
            if checkpoint.matches_state(context, &confirmed) && differences.is_empty() {
                record.last_runtime_id = setup.runtime_id.clone();
                record.local_file_stamp = setup.local_file_stamp.clone();
                record.local_file_digest = setup.local_file_digest.clone();
                write_record(context, &setup.key, &record)?;
                setup.resolution_required = false;
                return Ok(());
            }
            if checkpoint.matches_state(context, &confirmed) && setup.mode.writes() {
                let mut paths = differences.into_iter().collect::<Vec<_>>();
                paths.sort();
                record.last_runtime_id = setup.runtime_id.clone();
                record.local_file_stamp = setup.local_file_stamp.clone();
                record.local_file_digest = setup.local_file_digest.clone();
                self.push_editor_changes_locked(
                    context,
                    &setup.key,
                    bridge,
                    &paths,
                    Some(&studio_guard),
                    &mut record,
                )?;
                setup.resolution_required = false;
                return Ok(());
            }
        }
        let phase = Instant::now();
        let selective_services = match (record.studio_checkpoint.as_ref(), record.baseline.as_ref())
        {
            (Some(checkpoint), Some(_)) => {
                selective_studio_services(context, checkpoint, &studio_state)?
            }
            _ => None,
        };
        let mut loaded_baseline = None;
        let selective_capture = if let Some(services) = selective_services {
            let baseline = record
                .baseline
                .as_ref()
                .context("Reconciliation baseline disappeared")?
                .load(Path::new(&context.root), &setup.key)?;
            let captured = capture_changed_studio_services(
                context,
                bridge,
                &services,
                &baseline,
                &studio_state,
            )?;
            loaded_baseline = Some(baseline);
            captured
        } else {
            None
        };
        let (stage, studio, editor) = if let Some(captured) = selective_capture {
            captured
        } else {
            let (stage, studio) = capture_studio_project_for_comparison(context, bridge)?;
            let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
            (stage, studio, editor)
        };
        let publish_paths = stage.publish_paths().to_vec();
        log_reconcile_timing("capture", phase);
        let phase = Instant::now();
        let side_differences = snapshot_differences(&editor, &studio)?;
        let sides_match = side_differences.is_empty();
        log_reconcile_timing("side comparison", phase);
        if sides_match {
            let phase = Instant::now();
            let baseline_matches = record
                .baseline
                .as_ref()
                .is_some_and(|baseline| baseline.matches(&editor));
            log_reconcile_timing("baseline comparison", phase);
            if !baseline_matches || !record.conflicts.is_empty() || record.resolution_required {
                let phase = Instant::now();
                record.baseline = Some(StoredSnapshot::write(
                    Path::new(&context.root),
                    &setup.key,
                    &editor,
                )?);
                record.conflicts.clear();
                record.resolution_required = false;
                log_reconcile_timing("baseline write", phase);
            }
            record.last_runtime_id = setup.runtime_id.clone();
            record.local_file_stamp = setup.local_file_stamp.clone();
            record.local_file_digest = setup.local_file_digest.clone();
            record.studio_checkpoint =
                reconciled_studio_checkpoint(context, bridge, &studio_state, &studio_guard);
            write_record(context, &setup.key, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }
        let phase = Instant::now();
        let baseline = match loaded_baseline {
            Some(baseline) => Some(baseline),
            None => record
                .baseline
                .as_ref()
                .map(|baseline| baseline.load(Path::new(&context.root), &setup.key))
                .transpose()?,
        };
        log_reconcile_timing("baseline load", phase);
        let phase = Instant::now();
        let merge_preference = setup
            .resolution_preference
            .unwrap_or(record.conflict_preference);
        log_global(
            5,
            format_args!(
                "[renium] reconcile merge input: baseline={} preference={merge_preference:?} resolution={:?}",
                baseline.is_some(),
                setup.resolution_preference
            ),
        );
        let (mut merged, conflicts, mut changes) = if setup.bootstrap_studio_from_editor {
            (
                editor.clone(),
                Vec::new(),
                MergeChanges {
                    editor: HashSet::new(),
                    studio: side_differences.clone(),
                },
            )
        } else {
            merge_snapshots_with_changes(
                baseline.as_ref(),
                &editor,
                &studio,
                merge_preference,
                Some(&side_differences),
            )?
        };
        log_global(
            5,
            format_args!(
                "[renium] reconcile merge result: conflicts={} editor_paths={} studio_paths={}",
                conflicts.len(),
                changes.editor.len(),
                changes.studio.len()
            ),
        );
        if !changes.editor.is_empty() {
            let mut paths = changes
                .editor
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            paths.sort();
            log_global(
                5,
                format_args!("[renium] reconcile project paths: {}", paths.join(", ")),
            );
        }
        if !changes.studio.is_empty() {
            let mut paths = changes
                .studio
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            paths.sort();
            log_global(
                5,
                format_args!("[renium] reconcile Studio paths: {}", paths.join(", ")),
            );
        }
        log_reconcile_timing("merge", phase);

        if !conflicts.is_empty() {
            record.conflicts = conflicts;
            record.resolution_required = setup.resolution_preference.is_none()
                && record.conflict_preference == ConflictPreference::None;
            record.last_runtime_id = setup.runtime_id.clone();
            record.local_file_stamp = setup.local_file_stamp.clone();
            record.local_file_digest = setup.local_file_digest.clone();
            write_record(context, &setup.key, &record)?;
            setup.mode = PairMode::Verify;
            setup.resolution_required = record.resolution_required;
            setup.error = Some(conflict_message(&record.conflicts));
            return Ok(());
        }

        if setup.mode == PairMode::Verify {
            record.conflicts = vec!["Studio and project files differ".to_string()];
            record.resolution_required = false;
            setup.error = Some(conflict_message(&record.conflicts));
            record.last_runtime_id = setup.runtime_id.clone();
            record.local_file_stamp = setup.local_file_stamp.clone();
            record.local_file_digest = setup.local_file_digest.clone();
            write_record(context, &setup.key, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }

        let _readback = if changes.studio.is_empty() {
            if !changes.editor.is_empty() {
                publish_captured_studio(context, bridge, stage, &studio_guard)?;
            }
            studio
        } else {
            let phase = Instant::now();
            let push_plan = reconciliation_push_plan_for_paths(&studio, &merged, &changes.studio)?;
            log_reconcile_timing("push plan", phase);
            if push_plan.is_empty() {
                if !changes.editor.is_empty() {
                    publish_captured_studio(context, bridge, stage, &studio_guard)?;
                }
                studio
            } else {
                let phase = Instant::now();
                apply_snapshot_paths(&stage.project_root, &changes.studio, &merged)?;
                log_reconcile_timing("staged project write", phase);
                let phase = Instant::now();
                let generated = push_staged_project(
                    context,
                    &stage,
                    bridge,
                    StagedPushRequest {
                        plan: push_plan,
                        prepared_documents: HashMap::new(),
                        guard: Some(&studio_guard),
                        args: automation_push_args(context, &json!({}), false)?,
                        expected_project: Some(&editor),
                    },
                )?
                .generated;
                let generated_paths = generated.entries.keys().cloned().collect::<HashSet<_>>();
                if !generated_paths.is_empty() {
                    apply_snapshot_paths(&stage.project_root, &generated_paths, &generated)?;
                    for (path, entry) in generated.entries {
                        merged.entries.insert(path.clone(), entry);
                        changes.studio.insert(path);
                    }
                }
                log_reconcile_timing("Studio push", phase);
                let phase = Instant::now();
                let (readback_stage, readback) = if changes.editor.is_empty() {
                    let services = services_for_snapshot_paths(context, &changes.studio);
                    let (readback_stage, captured) =
                        capture_studio_services(context, bridge, &services, false)?;
                    let mismatches =
                        snapshot_path_differences(&captured, &merged, &changes.studio)?;
                    if !mismatches.is_empty() {
                        let details = snapshot_mismatch_details(&captured, &merged, &mismatches)?;
                        bail!(
                            "Studio did not retain reconciled paths: {}{}",
                            mismatches
                                .iter()
                                .map(|path| path.to_string_lossy())
                                .collect::<Vec<_>>()
                                .join(", "),
                            details
                                .as_deref()
                                .map(|details| format!(" ({details})"))
                                .unwrap_or_default()
                        );
                    }
                    let mut readback = studio;
                    for path in &changes.studio {
                        match captured.entries.get(path) {
                            Some(entry) => {
                                readback.entries.insert(path.clone(), entry.clone());
                            }
                            None => {
                                readback.entries.remove(path);
                            }
                        }
                    }
                    (readback_stage, readback)
                } else {
                    capture_studio_project(context, bridge)?
                };
                log_reconcile_timing("readback capture", phase);
                let phase = Instant::now();
                if !changes.editor.is_empty() {
                    let mismatches = snapshot_differences(&readback, &merged)?;
                    if !mismatches.is_empty() {
                        let mut paths = mismatches.into_iter().collect::<Vec<_>>();
                        paths.sort();
                        let detail = snapshot_mismatch_details(&readback, &merged, &paths)?
                            .unwrap_or_else(|| paths[0].display().to_string());
                        bail!("Studio did not retain the reconciled project state: {detail}");
                    }
                }
                if !changes.editor.is_empty() {
                    readback_stage.publish(Path::new(&context.root), false)?;
                }
                log_reconcile_timing("readback verification", phase);
                readback
            }
        };

        let phase = Instant::now();
        let baseline = capture_snapshot(Path::new(&context.root), &publish_paths)?;
        record.baseline = Some(StoredSnapshot::write(
            Path::new(&context.root),
            &setup.key,
            &baseline,
        )?);
        record.conflicts.clear();
        record.resolution_required = false;
        record.last_runtime_id = setup.runtime_id.clone();
        record.local_file_stamp = setup.local_file_stamp.clone();
        record.local_file_digest = setup.local_file_digest.clone();
        record.studio_checkpoint =
            reconciled_studio_checkpoint(context, bridge, &studio_state, &studio_guard);
        setup.resolution_required = false;
        write_record(context, &setup.key, &record)?;
        log_reconcile_timing("final baseline write", phase);
        Ok(())
    }

    pub(crate) fn reconcile_current(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<PairSetup> {
        let mut setup = self.current_setup(context, bridge)?;
        self.reconcile(context, bridge, &mut setup)?;
        Ok(setup)
    }

    fn current_setup(&self, context: &BoundContext, bridge: &BridgeServer) -> Result<PairSetup> {
        log_global(
            5,
            format_args!("[renium] reconcile current: cx={}", context.id),
        );
        let identity = PairIdentity::from_context(context, bridge)?;
        let key = identity.pair_key();
        let record = load_record(context, &key)?.context("Reconciliation state is missing")?;
        let current_local_file_stamp = local_file_stamp(identity.local_file.as_deref())?;
        let current_local_file_digest = if record.local_file_digest.is_none()
            || record.local_file_stamp != current_local_file_stamp
        {
            local_file_digest(identity.local_file.as_deref())?
        } else {
            record.local_file_digest.clone()
        };
        Ok(PairSetup {
            key,
            identity,
            mode: record.mode,
            resolution_preference: None,
            resolution_required: record.resolution_required,
            error: None,
            requires_reconcile: true,
            runtime_id: context.runtime_id.clone(),
            local_file_stamp: current_local_file_stamp,
            local_file_digest: current_local_file_digest,
            bootstrap_studio_from_editor: false,
            runtime_replacement_unproven: false,
        })
    }

    pub(crate) fn advance_baseline(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
        side: BaselineSide,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            return Ok(());
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let baseline = record
            .baseline
            .as_mut()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, paths)?;
        if scopes.is_empty() {
            return Ok(());
        }
        let root = Path::new(&context.root);
        let current = capture_snapshot(root, &scopes)?;
        if matches!(side, BaselineSide::Editor) {
            let previous = baseline.load_scopes(root, key, &scopes)?;
            validate_editor_package_links(&previous, &current, &scopes)?;
        }
        baseline.replace_scopes(root, key, &scopes, &current)?;
        write_record(context, key, &record)
    }

    pub(crate) fn record_studio_checkpoint(
        &self,
        context: &BoundContext,
        key: &str,
        state: &Value,
    ) -> Result<()> {
        let Some(checkpoint) = StudioCheckpoint::from_state(context, state) else {
            return Ok(());
        };
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.baseline.is_none() || !record.conflicts.is_empty() {
            return Ok(());
        }
        record.studio_checkpoint = Some(checkpoint);
        write_record(context, key, &record)
    }

    pub(crate) fn record_full_editor_push_with_gate_held(
        &self,
        context: &BoundContext,
        key: &str,
        bridge: &BridgeServer,
    ) -> Result<()> {
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            bail!("Live Sync is not allowed to write in verify mode");
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let root = Path::new(&context.root);
        let stage = project_comparison_stage(context, &sync_services())?;
        let current = capture_snapshot(root, stage.publish_paths())?;
        drop(stage);
        if !record
            .baseline
            .as_ref()
            .is_some_and(|baseline| baseline.matches(&current))
        {
            record.baseline = Some(StoredSnapshot::write(root, key, &current)?);
        }
        record.studio_checkpoint = current_studio_checkpoint(context, bridge);
        write_record(context, key, &record)
    }

    pub(crate) fn validate_editor_changes(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let record = load_record(context, key)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            return Ok(());
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, paths)?;
        if scopes.is_empty() {
            return Ok(());
        }
        let root = Path::new(&context.root);
        let previous = baseline.load_scopes(root, key, &scopes)?;
        let current = capture_snapshot(root, &scopes)?;
        validate_editor_package_links(&previous, &current, &scopes)
    }

    pub(crate) fn push_editor_changes(
        &self,
        context: &BoundContext,
        key: &str,
        bridge: &BridgeServer,
        paths: &[PathBuf],
        guard: Option<&StudioChangeGuard>,
    ) -> Result<AppliedEditorChanges> {
        if paths.is_empty() {
            return Ok(AppliedEditorChanges::default());
        }
        let pair_lock = self.pair_lock(key);
        let phase = Instant::now();
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        log_reconcile_timing("incremental pair lock", phase);
        let phase = Instant::now();
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
        log_reconcile_timing("incremental record load", phase);
        self.push_editor_changes_locked(context, key, bridge, paths, guard, &mut record)
    }

    fn push_editor_changes_locked(
        &self,
        context: &BoundContext,
        key: &str,
        bridge: &BridgeServer,
        paths: &[PathBuf],
        guard: Option<&StudioChangeGuard>,
        record: &mut PairRecord,
    ) -> Result<AppliedEditorChanges> {
        if record.mode != PairMode::Reconcile {
            bail!("Live Sync is not allowed to write in verify mode");
        }
        if !record.conflicts.is_empty() {
            bail!("Reconciliation state has unresolved changes");
        }
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let scopes = baseline_scopes(context, paths)?;
        if scopes.is_empty() {
            return Ok(AppliedEditorChanges::default());
        }
        let root = Path::new(&context.root);
        let phase = Instant::now();
        let previous = baseline.load_scopes(root, key, &scopes)?;
        log_reconcile_timing("incremental baseline load", phase);
        let phase = Instant::now();
        let current = capture_snapshot(root, &scopes)?;
        log_reconcile_timing("incremental project capture", phase);
        let phase = Instant::now();
        let prepared_settings = prepare_editor_settings_changes(&previous, &current, &scopes)?;
        log_reconcile_timing("incremental settings preparation", phase);
        let phase = Instant::now();
        let changed = previous
            .entries
            .keys()
            .chain(current.entries.keys())
            .filter(|path| {
                !entries_equivalent(
                    path,
                    previous.entries.get(*path),
                    current.entries.get(*path),
                )
            })
            .cloned()
            .collect::<HashSet<_>>();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &previous,
            &current,
            &changed,
            &prepared_settings,
        )?;
        log_reconcile_timing("incremental push plan", phase);
        let phase = Instant::now();
        let mut prepared_documents = HashMap::with_capacity(prepared_settings.len());
        let mut previous_documents = Vec::with_capacity(prepared_settings.len());
        for (path, change) in prepared_settings {
            let service = settings_service_name(&path, &change.current, &change.previous)?;
            previous_documents.push(change.previous);
            prepared_documents.insert(service, change.current);
        }
        for document in previous_documents {
            drop_settings_document(document);
        }
        log_reconcile_timing("incremental settings release", phase);
        let phase = Instant::now();
        let supporting_scopes = supporting_settings_scopes(context, &changed)?;
        let supporting = capture_snapshot(root, &supporting_scopes)?;
        let supporting_paths = supporting.entries.keys().cloned().collect::<HashSet<_>>();
        log_reconcile_timing("incremental supporting capture", phase);
        let phase = Instant::now();
        let source = bound_context::source_dir(context)?;
        let requires_stage = config::try_load_project(None, Some(root))?
            .as_ref()
            .map(config::project_requires_temporary_stage)
            .transpose()?
            .unwrap_or(false);
        let stage = if requires_stage {
            ExportProjectStage::create(root, &source, &[])?
        } else {
            ExportProjectStage::create_for_comparison(root, &source, &[])?
        };
        log_reconcile_timing("incremental stage", phase);
        let phase = Instant::now();
        apply_snapshot_paths(&stage.project_root, &supporting_paths, &supporting)?;
        apply_snapshot_paths(&stage.project_root, &changed, &current)?;
        log_reconcile_timing("incremental staged write", phase);
        let phase = Instant::now();
        let StagedPushResult { generated, summary } = push_staged_project(
            context,
            &stage,
            bridge,
            StagedPushRequest {
                plan,
                prepared_documents,
                guard,
                args: automation_push_args(context, &json!({}), false)?,
                expected_project: None,
            },
        )?;
        log_reconcile_timing("incremental Studio push", phase);
        let phase = Instant::now();
        drop(stage);
        log_reconcile_timing("incremental stage cleanup", phase);
        let phase = Instant::now();
        let baseline = record
            .baseline
            .as_mut()
            .context("Reconciliation baseline is missing")?;
        let mut changed_scopes = changed.iter().cloned().collect::<Vec<_>>();
        changed_scopes.sort();
        baseline.replace_scopes(root, key, &changed_scopes, &current)?;
        if !generated.entries.is_empty() {
            let generated_scopes = generated.entries.keys().cloned().collect::<Vec<_>>();
            baseline.replace_scopes(root, key, &generated_scopes, &generated)?;
        }
        log_reconcile_timing("incremental baseline update", phase);
        let accepted = accepted_editor_entries(root, &previous, &current, &generated);
        let phase = Instant::now();
        record.studio_checkpoint = current_studio_checkpoint(context, bridge);
        write_record(context, key, record)?;
        log_reconcile_timing("incremental record write", phase);
        Ok(AppliedEditorChanges { accepted, summary })
    }

    pub(crate) fn baseline_files(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
    ) -> Result<BTreeMap<String, String>> {
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let record = load_record(context, key)?.context("Reconciliation state is missing")?;
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let root = Path::new(&context.root);
        let requested = paths
            .iter()
            .filter_map(|requested| {
                let absolute = if requested.is_absolute() {
                    requested.clone()
                } else {
                    root.join(requested)
                };
                let relative = absolute.strip_prefix(root).ok()?.to_path_buf();
                Some((absolute, relative))
            })
            .collect::<Vec<_>>();
        let scopes = requested
            .iter()
            .map(|(_, relative)| relative.clone())
            .collect::<Vec<_>>();
        let baseline = baseline.load_scopes(root, key, &scopes)?;
        let mut files = BTreeMap::new();
        for (absolute, relative) in requested {
            let Some(SnapshotEntry::File(bytes)) = baseline.entries.get(&relative) else {
                continue;
            };
            let content = String::from_utf8(bytes.clone())
                .with_context(|| format!("Baseline file {} is not UTF-8", relative.display()))?;
            files.insert(absolute.to_string_lossy().into_owned(), content);
        }
        Ok(files)
    }

    fn claim_target(&self, identity: &PairIdentity) -> Option<String> {
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        let target = identity.target_key();
        let pair = identity.pair_key();
        match owners.by_target.get(&target) {
            Some((owner_pair, _)) if owner_pair == &pair => None,
            Some((_, owner_project)) => Some(owner_project.clone()),
            None => {
                owners
                    .by_target
                    .insert(target.clone(), (pair.clone(), identity.project.clone()));
                owners.target_by_pair.insert(pair, target);
                None
            }
        }
    }

    pub(crate) fn release_target(&self, pair: &str) {
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(target) = owners.target_by_pair.remove(pair) else {
            return;
        };
        if owners
            .by_target
            .get(&target)
            .is_some_and(|(owner_pair, _)| owner_pair == pair)
        {
            owners.by_target.remove(&target);
        }
    }
}

fn supporting_settings_scopes(
    context: &BoundContext,
    paths: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source)
        .strip_prefix(root)
        .context("Project source is outside its root")?;
    Ok(services_for_snapshot_paths(context, paths)
        .into_iter()
        .map(|service| service_settings_path(&source.join(service)))
        .collect())
}

fn baseline_scopes(context: &BoundContext, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source);
    let loaded = config::load_project(Some(Path::new(&context.project)), None)?;
    let inputs = project_watch_inputs(&loaded)?;
    let project = Path::new(&context.project);
    let mut scopes = Vec::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        let relative = absolute.strip_prefix(root).with_context(|| {
            format!(
                "Acknowledged path {} is outside {}",
                absolute.display(),
                root.display()
            )
        })?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
            || derived_project_path(relative)
            || absolute == project
        {
            continue;
        }
        let represented = absolute.starts_with(source)
            || inputs.files.contains(&absolute)
            || inputs
                .directories
                .iter()
                .any(|directory| absolute.starts_with(directory));
        if represented {
            scopes.push(relative.to_path_buf());
        }
    }
    scopes.sort_by_key(|path| path.components().count());
    let mut collapsed = Vec::<PathBuf>::new();
    for scope in scopes {
        if !collapsed.iter().any(|parent| scope.starts_with(parent)) {
            collapsed.push(scope);
        }
    }
    Ok(collapsed)
}

fn validate_editor_package_links(
    baseline: &ProjectSnapshot,
    current: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> Result<()> {
    for path in service_settings_paths(baseline, current, scopes) {
        if baseline.entries.get(&path) == current.entries.get(&path) {
            continue;
        }
        let before = settings_document(baseline.entries.get(&path))?;
        let mut after = settings_document(current.entries.get(&path))?;
        align_observation_ids_to_baseline(&before, &mut after);
        validate_package_link_documents(&path, &before, &after)?;
    }
    Ok(())
}

fn service_settings_paths(
    baseline: &ProjectSnapshot,
    current: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = baseline
        .entries
        .keys()
        .chain(current.entries.keys())
        .filter(|path| {
            scopes.iter().any(|scope| path.starts_with(scope))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_service_settings_file_name)
        })
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn prepare_editor_settings_changes(
    previous: &ProjectSnapshot,
    current: &ProjectSnapshot,
    scopes: &[PathBuf],
) -> Result<HashMap<PathBuf, PreparedEditorSettingsChange>> {
    service_settings_paths(previous, current, scopes)
        .into_iter()
        .filter(|path| previous.entries.get(path) != current.entries.get(path))
        .map(|path| {
            let phase = Instant::now();
            let (previous_document, current_document) = rayon::join(
                || editor_settings_document(previous.entries.get(&path)),
                || editor_settings_document(current.entries.get(&path)),
            );
            let change = PreparedEditorSettingsChange {
                previous: previous_document?,
                current: current_document?,
            };
            log_reconcile_timing("incremental settings decode", phase);
            let phase = Instant::now();
            validate_package_link_documents(&path, &change.previous, &change.current)?;
            log_reconcile_timing("incremental PackageLink validation", phase);
            Ok((path, change))
        })
        .collect()
}

fn validate_package_link_documents(
    path: &Path,
    before: &SettingsBytecode,
    after: &SettingsBytecode,
) -> Result<()> {
    let before_links = before
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let after_links = after
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mismatch = before_links
        .iter()
        .find_map(|(id, before_index)| match after_links.get(id).copied() {
            Some(after_index)
                if package_link_instances_equal(before, *before_index, after, after_index) =>
            {
                None
            }
            Some(after_index) => Some(package_link_mismatch_detail(
                id,
                before,
                *before_index,
                after,
                after_index,
            )),
            None if package_parent_is_missing(before, *before_index, after) => None,
            None => Some(format!("PackageLink identity {id} is missing")),
        })
        .or_else(|| {
            after_links
                .keys()
                .find(|id| !before_links.contains_key(*id))
                .map(|id| format!("PackageLink identity {id} was created"))
        });
    if let Some(mismatch) = mismatch {
        bail!(
            "{} changes a PackageLink directly; use the package workflow instead ({mismatch})",
            path.display(),
        );
    }
    Ok(())
}

fn package_link_mismatch_detail(
    id: &str,
    left: &SettingsBytecode,
    left_index: usize,
    right: &SettingsBytecode,
    right_index: usize,
) -> String {
    let left_instance = &left.instances[left_index];
    let right_instance = &right.instances[right_index];
    if left_instance.name != right_instance.name {
        return format!("PackageLink {id} was renamed");
    }
    if left_instance.class_name != right_instance.class_name {
        return format!("PackageLink {id} changed class");
    }
    if settings_parent_id(left, left_index) != settings_parent_id(right, right_index) {
        return format!("PackageLink {id} changed parent");
    }
    if !reconciliation_values_map_equal(&left_instance.attributes, &right_instance.attributes) {
        return format!("PackageLink {id} changed attributes");
    }
    format!("PackageLink {id} changed properties")
}

fn package_link_instances_equal(
    left: &SettingsBytecode,
    left_index: usize,
    right: &SettingsBytecode,
    right_index: usize,
) -> bool {
    let left_instance = &left.instances[left_index];
    let right_instance = &right.instances[right_index];
    let stable_properties_equal = || {
        let stable = |name: &str| name != "ModifiedState";
        left_instance
            .properties
            .iter()
            .filter(|(name, _)| stable(name))
            .all(|(name, value)| {
                right_instance
                    .properties
                    .get(name)
                    .is_none_or(|other| reconciliation_values_equal(value, other, false))
            })
            && right_instance
                .properties
                .iter()
                .filter(|(name, _)| stable(name))
                .all(|(name, value)| {
                    left_instance
                        .properties
                        .get(name)
                        .is_none_or(|other| reconciliation_values_equal(value, other, false))
                })
    };
    left_instance.name == right_instance.name
        && left_instance.class_name == right_instance.class_name
        && settings_parent_id(left, left_index) == settings_parent_id(right, right_index)
        && stable_properties_equal()
        && reconciliation_values_map_equal(&left_instance.attributes, &right_instance.attributes)
}

#[cfg(test)]
fn align_snapshot_ids(
    reference: &ProjectSnapshot,
    observed: &ProjectSnapshot,
) -> Result<ProjectSnapshot> {
    let mut aligned = observed.clone();
    for (path, entry) in &mut aligned.entries {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            continue;
        }
        let Some(reference_entry) = reference.entries.get(path) else {
            continue;
        };
        let reference = settings_document(Some(reference_entry))?;
        let mut observed = settings_document(Some(entry))?;
        if !align_settings_ids_to_reference(&reference, &mut observed) {
            bail!(
                "Failed to align duplicate instance identities in {}",
                path.display()
            );
        }
        *entry = SnapshotEntry::File(encode_settings_bytecode(&observed)?);
    }
    Ok(aligned)
}

fn conflict_message(conflicts: &[String]) -> String {
    let shown = conflicts.iter().take(3).cloned().collect::<Vec<_>>();
    let remainder = conflicts.len().saturating_sub(shown.len());
    if remainder == 0 {
        format!("Sync needs review: {}", shown.join("; "))
    } else {
        format!(
            "Sync needs review: {}; and {remainder} more",
            shown.join("; ")
        )
    }
}

pub(crate) fn sync_services() -> Vec<String> {
    DEFAULT_SYNC_SERVICES
        .iter()
        .map(|service| (*service).to_string())
        .collect()
}

pub(crate) fn selected_push_delta_services(
    context: &BoundContext,
    args: &PushEditorChangesArgs,
) -> Result<Option<Vec<String>>> {
    if !args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || !args.target_properties.is_empty()
        || args.upsert_instances_only
    {
        return Ok(None);
    }
    let changed_paths = expand_editor_changed_paths(args)?;
    if !changed_paths.iter().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    }) {
        return Ok(None);
    }
    let root = Path::new(&context.root);
    let mut relative_paths = HashSet::with_capacity(changed_paths.len());
    for path in changed_paths {
        let absolute = absolutize_under(root, &path);
        let Ok(relative) = absolute.strip_prefix(root) else {
            return Ok(Some(sync_services()));
        };
        relative_paths.insert(relative.to_path_buf());
    }
    Ok(Some(services_for_snapshot_paths(context, &relative_paths)))
}

fn pin_edit_runtime(context: &BoundContext, bridge: &BridgeServer) -> Result<()> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Live Sync context has no edit-mode Studio runtime")?;
    bridge.clear_runtime_pins();
    bridge.pin_runtime(BridgeTarget::Edit, runtime_id);
    bridge.pin_runtime(BridgeTarget::Main, runtime_id);
    Ok(())
}

pub(crate) fn studio_change_guard_from_state(
    context: &BoundContext,
    state: &Value,
) -> Result<StudioChangeGuard> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    let state_runtime_id = state["runtimeId"]
        .as_str()
        .context("Studio change state did not include its runtime")?;
    if state_runtime_id != runtime_id {
        bail!("Studio change state came from a different runtime");
    }
    let service_generations = state["serviceGenerations"]
        .as_object()
        .context("Studio change state did not include service generations")?
        .iter()
        .map(|(service, generation)| {
            generation
                .as_u64()
                .map(|generation| (service.clone(), generation))
                .with_context(|| format!("Studio generation for {service} is invalid"))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(StudioChangeGuard {
        runtime_id: state_runtime_id.to_string(),
        change_seq: state["seq"]
            .as_u64()
            .context("Studio change state did not include its sequence")?,
        tracking_guard_id: None,
        runtime_bootstrap_safe: studio_runtime_bootstrap_safe(state),
        service_generations,
    })
}

fn studio_runtime_bootstrap_safe(state: &Value) -> bool {
    let restored_pending_services = state["restoredPendingServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let has_fresh_changes = state["dirtyServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|service| !restored_pending_services.contains(service));
    state["tracking"].as_bool() == Some(true) && !has_fresh_changes
}

fn read_studio_change_state(context: &BoundContext, bridge: &BridgeServer) -> Result<Value> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    pin_edit_runtime(context, bridge)?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": false, "includeGenerations": true }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    Ok(state)
}

fn current_studio_checkpoint(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Option<StudioCheckpoint> {
    match read_studio_change_state(context, bridge) {
        Ok(state) => {
            let checkpoint = StudioCheckpoint::from_state(context, &state);
            if checkpoint.is_none() {
                log_global(
                    5,
                    format_args!(
                        "[renium] Studio checkpoint state rejected: tracking={:?} tracked={:?} dirty={} full={} runtime={:?} version={:?} seq={:?} generations={}",
                        state["tracking"].as_bool(),
                        state["trackedServices"].as_u64(),
                        state["dirtyServices"].as_array().map_or(0, Vec::len),
                        state["fullSyncServices"].as_array().map_or(0, Vec::len),
                        state["runtimeId"].as_str(),
                        state["changeTrackerVersion"].as_u64(),
                        state["seq"].as_u64(),
                        checkpoint_generations(&state).map_or(0, Map::len),
                    ),
                );
            }
            checkpoint
        }
        Err(error) => {
            log_global(
                5,
                format_args!("[renium] Studio checkpoint unavailable: {error:#}"),
            );
            None
        }
    }
}

fn reconciled_studio_checkpoint(
    context: &BoundContext,
    bridge: &BridgeServer,
    initial_state: &Value,
    guard: &StudioChangeGuard,
) -> Option<StudioCheckpoint> {
    let services = initial_state["dirtyServices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if services.is_empty() {
        return current_studio_checkpoint(context, bridge);
    }
    match acknowledge_pulled_changes(bridge, &services, guard.change_seq, &guard.runtime_id) {
        Ok(state) => StudioCheckpoint::from_state(context, &state),
        Err(error) => {
            log_global(
                5,
                format_args!("[renium] reconciled Studio acknowledgment failed: {error:#}"),
            );
            None
        }
    }
}

struct StudioTrackingGuardRelease<'a> {
    bridge: &'a BridgeServer,
    runtime_id: String,
    guard_id: Option<String>,
}

impl StudioTrackingGuardRelease<'_> {
    fn finish(&mut self) -> Result<()> {
        let Some(guard_id) = self.guard_id.as_ref() else {
            return Ok(());
        };
        let result = self.bridge.call_for_runtime_with_timeout(
            "getStudioChangeState",
            json!({
                "start": false,
                "releaseTrackingGuardId": guard_id,
            }),
            BridgeTarget::Edit,
            &self.runtime_id,
            Some(Duration::from_secs(10)),
        )?;
        ensure_plugin_api_ok(&result)?;
        self.guard_id = None;
        Ok(())
    }
}

impl Drop for StudioTrackingGuardRelease<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn acknowledge_verified_push(
    bridge: &BridgeServer,
    services: &[String],
    guard: &StudioChangeGuard,
) -> Result<()> {
    let result = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "services": services,
            "start": false,
            "ackSeq": guard.change_seq,
            "runtimeId": guard.runtime_id,
        }),
        BridgeTarget::Edit,
        &guard.runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&result)
}

fn current_studio_change_guard_with_state(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<(StudioChangeGuard, Value)> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    let initial = read_studio_change_state(context, bridge)?;
    if initial["tracking"].as_bool() == Some(true) {
        let guard = studio_change_guard_from_state(context, &initial)?;
        return Ok((guard, initial));
    }
    let guard_id = format!(
        "push-{}-{}-{}",
        std::process::id(),
        runtime_id,
        crate::app::timing::current_millis()
    );
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "start": true,
            "trackingGuardId": guard_id,
            "includeGenerations": true,
        }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    let mut guard = studio_change_guard_from_state(context, &state)?;
    guard.tracking_guard_id = Some(guard_id);
    guard.runtime_bootstrap_safe = false;
    Ok((guard, state))
}

fn current_studio_change_guard(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<StudioChangeGuard> {
    current_studio_change_guard_with_state(context, bridge).map(|(guard, _)| guard)
}

fn studio_guard_matches_state(
    context: &BoundContext,
    guard: &StudioChangeGuard,
    state: &Value,
) -> bool {
    studio_change_guard_from_state(context, state).is_ok_and(|current| {
        current.runtime_id == guard.runtime_id
            && current.change_seq == guard.change_seq
            && current.service_generations == guard.service_generations
    })
}

fn studio_states_share_epoch(context: &BoundContext, left: &Value, right: &Value) -> bool {
    let services = sync_services();
    let expected_runtime = context.runtime_id.as_deref();
    let tracker_version = left["changeTrackerVersion"].as_u64();
    let seq = left["seq"].as_u64();
    let fields_match = left["tracking"].as_bool() == Some(true)
        && right["tracking"].as_bool() == Some(true)
        && left["trackedServices"].as_u64() == u64::try_from(services.len()).ok()
        && right["trackedServices"].as_u64() == u64::try_from(services.len()).ok()
        && left["runtimeId"].as_str() == expected_runtime
        && right["runtimeId"].as_str() == expected_runtime
        && tracker_version.is_some()
        && tracker_version == right["changeTrackerVersion"].as_u64()
        && seq.is_some()
        && seq == right["seq"].as_u64();
    if !fields_match {
        return false;
    }
    let (Some(left_generations), Some(right_generations)) =
        (checkpoint_generations(left), checkpoint_generations(right))
    else {
        return false;
    };
    if left_generations.len() != services.len() || right_generations.len() != services.len() {
        return false;
    }
    services.iter().all(|service| {
        left_generations.get(service).and_then(Value::as_u64)
            == right_generations.get(service).and_then(Value::as_u64)
    })
}

fn publish_captured_studio(
    context: &BoundContext,
    bridge: &BridgeServer,
    stage: ExportProjectStage,
    guard: &StudioChangeGuard,
) -> Result<()> {
    let confirmed = read_studio_change_state(context, bridge)?;
    if studio_guard_matches_state(context, guard, &confirmed) {
        stage.publish(Path::new(&context.root), false)?;
    } else {
        capture_studio_project(context, bridge)?
            .0
            .publish(Path::new(&context.root), false)?;
    }
    Ok(())
}

fn capture_studio_project(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    let services = sync_services();
    capture_studio_services(context, bridge, &services, true)
}

fn capture_studio_project_for_comparison(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    retry_transient_studio_capture(|| {
        let services = sync_services();
        let mut stage = project_comparison_stage(context, &services)?;
        stage.capture_publish_baseline(Path::new(&context.root))?;
        capture_studio_services_with_stage(context, bridge, &services, true, stage)
    })
}

fn retry_transient_studio_capture<T>(mut capture: impl FnMut() -> Result<T>) -> Result<T> {
    match capture() {
        Err(error) if is_transient_studio_capture_change(&error) => {
            log_global(
                5,
                format_args!("[renium] Studio changed during comparison capture; retrying once"),
            );
            capture()
        }
        result => result,
    }
}

fn is_transient_studio_capture_change(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("Studio changed ") && message.contains("retry the sync")
}

fn selective_studio_services(
    context: &BoundContext,
    checkpoint: &StudioCheckpoint,
    state: &Value,
) -> Result<Option<Vec<String>>> {
    let Some(services) = checkpoint.changed_services(context, state) else {
        return Ok(None);
    };
    if services.is_empty() || services.len() == sync_services().len() {
        return Ok(None);
    }
    let root = Path::new(&context.root);
    if config::try_load_project(None, Some(root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false)
    {
        return Ok(None);
    }
    Ok(Some(services))
}

fn capture_changed_studio_services(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    baseline: &ProjectSnapshot,
    initial_state: &Value,
) -> Result<Option<(ExportProjectStage, ProjectSnapshot, ProjectSnapshot)>> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source)
        .strip_prefix(root)
        .context("Project source is outside its root")?;
    let scopes = services
        .iter()
        .map(|service| source.join(service))
        .collect::<Vec<_>>();
    let all_services = sync_services();
    let src_dir = bound_context::source_dir(context)?;
    let stage = ExportProjectStage::create(root, &src_dir, &all_services)?;
    let editor = capture_snapshot(&stage.project_root, stage.publish_paths())?;
    let stage = import_studio_services_into_stage(context, bridge, services, true, stage)?;
    let captured = capture_snapshot(&stage.project_root, &scopes)?;
    let mut studio = baseline.clone();
    studio
        .entries
        .retain(|path, _| !scopes.iter().any(|scope| path.starts_with(scope)));
    studio.entries.extend(captured.entries);
    let confirmed = read_studio_change_state(context, bridge)?;
    if !studio_states_share_epoch(context, initial_state, &confirmed) {
        return Ok(None);
    }
    let current_editor = capture_snapshot(root, stage.publish_paths())?;
    if editor != current_editor {
        bail!("Project files changed while Studio recovery was being captured; retry the sync");
    }
    log_global(
        5,
        format_args!("[renium] selective Studio capture: {}", services.join(",")),
    );
    Ok(Some((stage, studio, editor)))
}

fn project_comparison_stage(
    context: &BoundContext,
    services: &[String],
) -> Result<ExportProjectStage> {
    let root = PathBuf::from(&context.root);
    let src_dir = bound_context::source_dir(context)?;
    let requires_stage = config::try_load_project(None, Some(&root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false);
    if requires_stage {
        ExportProjectStage::create(&root, &src_dir, services)
    } else {
        ExportProjectStage::create_for_comparison(&root, &src_dir, services)
    }
}

pub(crate) fn push_project_delta(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: PushEditorChangesArgs,
    guard: Option<&StudioChangeGuard>,
) -> Result<Map<String, Value>> {
    let phase = Instant::now();
    let guard = guard
        .cloned()
        .map_or_else(|| current_studio_change_guard(context, bridge), Ok)?;
    log_reconcile_timing("full push guard", phase);
    let phase = Instant::now();
    let mut tracking_release = StudioTrackingGuardRelease {
        bridge,
        runtime_id: guard.runtime_id.clone(),
        guard_id: guard.tracking_guard_id.clone(),
    };
    let src_dir = push_args.project.src_root.clone();
    let root = push_args.project.project_root.clone();
    let requires_stage = config::try_load_project(None, Some(&root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false);
    let stage = if requires_stage {
        ExportProjectStage::create(&root, &src_dir, services)?
    } else {
        ExportProjectStage::create_for_comparison(&root, &src_dir, services)?
    };
    let (stage, studio) =
        capture_studio_services_with_stage(context, bridge, services, false, stage)?;
    let project = capture_snapshot(&root, stage.publish_paths())?;
    log_reconcile_timing("full push capture", phase);
    let phase = Instant::now();
    let mut prepared_settings = HashMap::new();
    let differences =
        snapshot_differences_prepared(&project, &studio, Some(&mut prepared_settings))?;
    log_reconcile_timing("full push comparison", phase);
    if differences.is_empty() {
        acknowledge_verified_push(bridge, services, &guard)?;
        tracking_release.finish()?;
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let phase = Instant::now();
    let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
        &studio,
        &project,
        &differences,
        &prepared_settings,
    )?;
    log_reconcile_timing("full push plan", phase);
    if plan.is_empty() {
        acknowledge_verified_push(bridge, services, &guard)?;
        tracking_release.finish()?;
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let mutation_paths = differences.clone();
    let phase = Instant::now();
    apply_snapshot_paths(&stage.project_root, &mutation_paths, &project)?;
    let mut prepared_documents = HashMap::with_capacity(prepared_settings.len());
    for (path, change) in prepared_settings {
        let service = settings_service_name(&path, &change.current, &change.previous)?;
        drop_settings_document(change.previous);
        prepared_documents.insert(service, change.current);
    }
    let pushed = push_staged_project(
        context,
        &stage,
        bridge,
        StagedPushRequest {
            plan,
            prepared_documents,
            guard: Some(&guard),
            args: push_args,
            expected_project: Some(&project),
        },
    )?;
    log_reconcile_timing("full push mutation", phase);

    let current = capture_snapshot(&root, stage.publish_paths())?;
    let mut verification_paths = differences;
    verification_paths.extend(mutation_paths);
    verification_paths.extend(pushed.generated.entries.keys().cloned());
    if pushed.generated.entries.is_empty()
        && exact_source_push_verified(&pushed.summary, &verification_paths)
    {
        acknowledge_verified_push(bridge, services, &guard)?;
        tracking_release.finish()?;
        return Ok(pushed.summary);
    }
    let verification_services = services_for_snapshot_paths(context, &verification_paths);
    let phase = Instant::now();
    let (_readback_stage, readback) =
        capture_studio_services(context, bridge, &verification_services, false)?;
    log_reconcile_timing("full push readback", phase);
    let phase = Instant::now();
    let (mismatches, details) =
        snapshot_intended_delta_mismatches(&studio, &current, &readback, &verification_paths)?;
    log_reconcile_timing("full push verification", phase);
    if !mismatches.is_empty() {
        bail!(
            "Studio did not retain pushed project changes: {}{}",
            mismatches
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", "),
            details
                .as_deref()
                .map(|details| format!(" ({details})"))
                .unwrap_or_default()
        );
    }
    acknowledge_verified_push(bridge, services, &guard)?;
    tracking_release.finish()?;
    Ok(pushed.summary)
}

fn exact_source_push_verified(
    summary: &Map<String, Value>,
    verification_paths: &HashSet<PathBuf>,
) -> bool {
    !verification_paths.is_empty()
        && verification_paths.iter().all(|path| is_source_path(path))
        && summary.get("sourceVerifyFailed").and_then(Value::as_u64) == Some(0)
        && summary.get("sourceVerified").and_then(Value::as_u64)
            == u64::try_from(verification_paths.len()).ok()
}

fn capture_studio_services(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    generate_sourcemap: bool,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    let src_dir = bound_context::source_dir(context)?;
    let stage = ExportProjectStage::create(Path::new(&context.root), &src_dir, services)?;
    capture_studio_services_with_stage(context, bridge, services, generate_sourcemap, stage)
}

fn capture_studio_services_with_stage(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    generate_sourcemap: bool,
    stage: ExportProjectStage,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    let stage =
        import_studio_services_into_stage(context, bridge, services, generate_sourcemap, stage)?;
    let snapshot = capture_snapshot(&stage.project_root, stage.publish_paths())?;
    Ok((stage, snapshot))
}

fn import_studio_services_into_stage(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    generate_sourcemap: bool,
    stage: ExportProjectStage,
) -> Result<ExportProjectStage> {
    let capture_dir = create_unique_directory(
        &Path::new(&context.root).join(".renium"),
        "reconcile-capture-",
    )?;
    let cleanup_path = capture_dir.clone();
    let _cleanup = OnDrop::new(move || {
        let _ = fs::remove_dir_all(cleanup_path);
    });
    pin_edit_runtime(context, bridge)?;
    let parameters = json!({
        "snapshotDir": capture_dir,
        "services": services,
    });
    let mut args = automation_pull_args(context, &parameters, true)?;
    args.project_root.clone_from(&stage.import_project_root);
    args.src_dir.clone_from(&stage.import_src_dir);
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    export_snapshots_with_warm_bridge(args, bridge, &info, 0.0, false, false)?;
    let regenerate_sourcemap = generate_sourcemap && stage.capture_sourcemap_needs_regeneration();
    stage.finish_projection(regenerate_sourcemap)?;
    Ok(stage)
}

fn services_for_snapshot_paths(context: &BoundContext, paths: &HashSet<PathBuf>) -> Vec<String> {
    let root = Path::new(&context.root);
    let source = Path::new(&context.source);
    let Some(source) = source.strip_prefix(root).ok() else {
        return sync_services();
    };
    let mut services = HashSet::new();
    for path in paths {
        let Some(service) = path
            .strip_prefix(source)
            .ok()
            .and_then(|path| path.components().next())
            .and_then(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .and_then(|name| {
                DEFAULT_SYNC_SERVICES
                    .iter()
                    .find(|service| service.eq_ignore_ascii_case(name))
            })
        else {
            return sync_services();
        };
        services.insert((*service).to_string());
    }
    let mut services = services.into_iter().collect::<Vec<_>>();
    services.sort();
    services
}

fn staged_context(
    context: &BoundContext,
    stage: &ExportProjectStage,
    source_root: &Path,
) -> Result<BoundContext> {
    let project_relative = Path::new(&context.project).strip_prefix(&context.root)?;
    let source_relative = project_relative_source_root(Path::new(&context.root), source_root)?;
    let mut staged = context.clone();
    staged.root = stage.project_root.display().to_string();
    staged.project = stage
        .project_root
        .join(project_relative)
        .display()
        .to_string();
    staged.source = stage
        .project_root
        .join(source_relative)
        .display()
        .to_string();
    Ok(staged)
}

fn project_relative_source_root(project_root: &Path, source_root: &Path) -> Result<PathBuf> {
    absolutize_under(project_root, source_root)
        .strip_prefix(project_root)
        .with_context(|| {
            format!(
                "Source root {} is outside project {}",
                source_root.display(),
                project_root.display()
            )
        })
        .map(Path::to_path_buf)
}

struct StagedPushResult {
    generated: ProjectSnapshot,
    summary: Map<String, Value>,
}

#[derive(Default)]
pub(crate) struct AppliedEditorChanges {
    pub(crate) accepted: BTreeMap<PathBuf, Option<PublishEntryState>>,
    pub(crate) summary: Map<String, Value>,
}

fn accepted_editor_entries(
    root: &Path,
    previous: &ProjectSnapshot,
    current: &ProjectSnapshot,
    generated: &ProjectSnapshot,
) -> BTreeMap<PathBuf, Option<PublishEntryState>> {
    previous
        .entries
        .keys()
        .chain(current.entries.keys())
        .chain(generated.entries.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| {
            let entry = generated
                .entries
                .get(path)
                .or_else(|| current.entries.get(path));
            let state = entry.map(|entry| match entry {
                SnapshotEntry::Directory => PublishEntryState::Directory,
                SnapshotEntry::File(bytes) => PublishEntryState::File {
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                    length: bytes.len() as u64,
                    hash: fnv1a(bytes),
                },
                SnapshotEntry::Symlink { target, .. } => PublishEntryState::Symlink(target.clone()),
            });
            (root.join(path), state)
        })
        .collect()
}

#[test]
fn editor_acknowledgment_uses_captured_and_generated_bytes_including_deletions() {
    let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
    let removed = PathBuf::from("src/removed.luau");
    let previous = ProjectSnapshot {
        entries: BTreeMap::from([
            (path.clone(), SnapshotEntry::File(b"old".to_vec())),
            (removed.clone(), SnapshotEntry::File(b"removed".to_vec())),
        ]),
    };
    let current = ProjectSnapshot {
        entries: BTreeMap::from([(path.clone(), SnapshotEntry::File(b"captured".to_vec()))]),
    };
    let generated = ProjectSnapshot {
        entries: BTreeMap::from([(path.clone(), SnapshotEntry::File(b"generated".to_vec()))]),
    };
    let root = Path::new("project");
    let accepted = accepted_editor_entries(root, &previous, &current, &generated);
    assert!(matches!(accepted.get(&root.join(removed)), Some(None)));
    assert!(
        matches!(accepted.get(&root.join(path)), Some(Some(PublishEntryState::File { length: 9, hash, .. })) if *hash == fnv1a(b"generated"))
    );
}

struct StagedPushRequest<'a> {
    plan: ReconcilePushPlan,
    prepared_documents: HashMap<String, SettingsBytecode>,
    guard: Option<&'a StudioChangeGuard>,
    args: PushEditorChangesArgs,
    expected_project: Option<&'a ProjectSnapshot>,
}

fn push_staged_project(
    context: &BoundContext,
    stage: &ExportProjectStage,
    bridge: &BridgeServer,
    request: StagedPushRequest<'_>,
) -> Result<StagedPushResult> {
    let StagedPushRequest {
        plan,
        prepared_documents,
        guard,
        args: mut push_args,
        expected_project,
    } = request;
    if plan.is_empty() {
        return Ok(StagedPushResult {
            generated: ProjectSnapshot::default(),
            summary: Map::from_iter([("ok".to_string(), Value::Bool(true))]),
        });
    }
    let changed_paths = plan.changed_paths.iter().cloned().collect::<HashSet<_>>();
    let supporting_paths = supporting_settings_scopes(context, &changed_paths)?
        .into_iter()
        .filter(|path| !changed_paths.contains(path))
        .collect::<Vec<_>>();
    if !supporting_paths.is_empty() {
        let root = Path::new(&context.root);
        let staged = expected_project
            .is_none()
            .then(|| capture_snapshot(&stage.project_root, &supporting_paths))
            .transpose()?;
        let current = capture_snapshot(root, &supporting_paths)?;
        let supporting_paths = supporting_paths.into_iter().collect::<HashSet<_>>();
        let differences = if let Some(expected_project) = expected_project {
            snapshot_path_differences(expected_project, &current, &supporting_paths)?
        } else {
            snapshot_path_differences(
                staged.as_ref().expect("staged snapshot exists"),
                &current,
                &supporting_paths,
            )?
        };
        if !differences.is_empty() {
            bail!(
                "Supporting project data {} changed while its Studio update was being prepared; retry the sync",
                root.join(&differences[0]).display()
            );
        }
        if expected_project.is_none() {
            apply_snapshot_paths(&stage.project_root, &supporting_paths, &current)?;
        }
    }
    let project_root = push_args.project.project_root.clone();
    let validate_project = || -> Result<()> {
        let Some(expected_project) = expected_project else {
            return Ok(());
        };
        let current = capture_snapshot(&project_root, stage.publish_paths())?;
        let differences = snapshot_differences(expected_project, &current)?;
        if let Some(changed) = differences.iter().next() {
            bail!(
                "Project file {} changed while its Studio update was being prepared; retry the sync",
                project_root.join(changed).display()
            );
        }
        Ok(())
    };
    let staged = staged_context(context, stage, &push_args.project.src_root)?;
    let _selection = bound_context::select(&staged);
    pin_edit_runtime(&staged, bridge)?;
    let source_relative =
        project_relative_source_root(&push_args.project.project_root, &push_args.project.src_root)?;
    push_args
        .project
        .project_root
        .clone_from(&stage.project_root);
    push_args.project.src_root = stage.project_root.join(source_relative);
    push_args.paths.clear();
    push_args.changed_paths.clone_from(&plan.changed_paths);
    push_args.changed_paths_files.clear();
    push_args
        .target_settings_ids
        .clone_from(&plan.target_settings_ids);
    push_args.target_settings_id_files.clear();
    push_args.target_properties.clear();
    push_args.verify_sources = true;
    push_args.upsert_instances_only = false;
    let mut generated = ProjectSnapshot::default();
    let summary = push_reconciled_editor_changes_with_warm_bridge(
        push_args,
        bridge,
        guard,
        prepared_documents,
        |changes| amend_reconciled_changes(changes, plan),
        |changes| {
            generated = redirect_staged_settings_writes(
                changes,
                &stage.project_root,
                Path::new(&context.root),
                expected_project,
            )?;
            Ok(())
        },
        validate_project,
    )?;
    if summary.get("skippedByReview").and_then(Value::as_bool) == Some(true) {
        bail!("Reconciled changes require review before Studio can be updated");
    }
    Ok(StagedPushResult { generated, summary })
}

fn redirect_staged_settings_writes(
    changes: &mut EditorChangeSet,
    staged_root: &Path,
    project_root: &Path,
    expected_project: Option<&ProjectSnapshot>,
) -> Result<ProjectSnapshot> {
    let mut generated = ProjectSnapshot::default();
    for write in &mut changes.settings_writes {
        let relative = write
            .path
            .strip_prefix(staged_root)
            .with_context(|| {
                format!(
                    "Generated settings path {} is outside its reconciliation stage",
                    write.path.display()
                )
            })?
            .to_path_buf();
        let destination = project_root.join(&relative);
        // The staged document may already contain merged Studio data or aligned IDs.
        // Its hash guards the stage, not the original project we will publish into.
        let expected_hash = if let Some(project) = expected_project {
            match project.entries.get(&relative) {
                Some(SnapshotEntry::File(bytes)) => Some(Sha256::digest(bytes).into()),
                None => None,
                Some(_) => bail!("Settings path {} was not a file", destination.display()),
            }
        } else {
            write.expected_hash
        };
        if settings_file_hash(&destination)? != expected_hash {
            bail!(
                "Settings file {} changed while its Studio update was being prepared; retry the sync",
                destination.display()
            );
        }
        write.path = destination;
        write.expected_hash = expected_hash;
        generated.entries.insert(
            relative,
            SnapshotEntry::File(encode_settings_bytecode(&write.document)?),
        );
    }
    Ok(generated)
}

fn amend_reconciled_changes(
    changes: &mut EditorChangeSet,
    mut plan: ReconcilePushPlan,
) -> Result<()> {
    if changes
        .instance_changes
        .iter()
        .any(|change| change.mode == "reconcileService")
    {
        bail!("Reconciliation cannot replace an entire Studio service");
    }
    plan.instance_deletes.append(&mut changes.instance_changes);
    changes.instance_changes = plan.instance_deletes;
    for change in &mut changes.instance_changes {
        for instance in &mut change.instances {
            if let Some(previous) = plan.previous_class_names.get(&instance.settings_id) {
                instance.previous_class_name = Some(previous.clone());
            }
            if let Some(previous) = plan
                .previous_paths
                .get(&(change.service.clone(), instance.settings_id.clone()))
            {
                instance.previous_path_segments = previous.path_segments.clone();
                instance.previous_path_ordinals = previous.path_ordinals.clone();
            }
        }
    }
    for removal in plan.property_removals {
        if let Some(change) = changes.property_changes.iter_mut().find(|change| {
            change.service == removal.service
                && change.settings_id == removal.settings_id
                && change.path_segments == removal.path_segments
                && change.path_ordinals == removal.path_ordinals
        }) {
            change.reset_properties.extend(removal.reset_properties);
            change.deleted_attributes.extend(removal.deleted_attributes);
            change.reset_properties.sort();
            change.reset_properties.dedup();
            change.deleted_attributes.sort();
            change.deleted_attributes.dedup();
        } else {
            changes.property_changes.push(removal);
        }
    }
    for change in &changes.property_changes {
        if let Some(id) = &change.settings_id
            && let Some(properties) = plan
                .geometry_properties
                .remove(&(change.service.clone(), id.clone()))
        {
            changes
                .geometry_readbacks
                .push(crate::editor::native_geometry::GeometryReadback {
                    service: change.service.clone(),
                    settings_id: id.clone(),
                    path_segments: change.path_segments.clone(),
                    path_ordinals: change.path_ordinals.clone(),
                    properties,
                });
        }
    }
    Ok(())
}

#[cfg(test)]
fn reconciliation_push_plan(
    studio: &ProjectSnapshot,
    merged: &ProjectSnapshot,
) -> Result<ReconcilePushPlan> {
    let mut paths = studio
        .entries
        .keys()
        .chain(merged.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();

    let paths = paths.into_iter().collect::<HashSet<_>>();
    reconciliation_push_plan_for_paths(studio, merged, &paths)
}

fn reconciliation_push_plan_for_paths(
    studio: &ProjectSnapshot,
    merged: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
) -> Result<ReconcilePushPlan> {
    reconciliation_push_plan_for_paths_with_prepared_settings(
        studio,
        merged,
        paths,
        &HashMap::new(),
    )
}

fn reconciliation_push_plan_for_paths_with_prepared_settings(
    studio: &ProjectSnapshot,
    merged: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
    prepared_settings: &HashMap<PathBuf, PreparedEditorSettingsChange>,
) -> Result<ReconcilePushPlan> {
    let mut paths = paths.iter().cloned().collect::<Vec<_>>();
    paths.sort();

    let mut plan = ReconcilePushPlan::default();
    for path in paths {
        let studio_entry = studio.entries.get(&path);
        let merged_entry = merged.entries.get(&path);
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            if let Some(prepared) = prepared_settings.get(&path) {
                let targets_before = plan.target_settings_ids.len();
                let phase = Instant::now();
                append_aligned_settings_push_plan(
                    &path,
                    &prepared.current,
                    &prepared.previous,
                    &mut plan,
                )?;
                log_reconcile_timing("incremental settings delta", phase);
                if plan.target_settings_ids.len() > targets_before {
                    plan.changed_paths.push(path);
                }
                continue;
            }
            let desired = settings_document(merged_entry)?;
            let observed = settings_document(studio_entry)?;
            if settings_documents_equivalent(&desired, &observed) {
                continue;
            }
            let targets_before = plan.target_settings_ids.len();
            append_settings_push_plan(&path, &desired, &observed, &mut plan)?;
            if plan.target_settings_ids.len() > targets_before {
                plan.changed_paths.push(path);
            }
        } else if !matches!(merged_entry, Some(SnapshotEntry::Directory))
            && !entries_equivalent(&path, studio_entry, merged_entry)
        {
            plan.changed_paths.push(path);
        }
    }
    append_recreated_reference_pushes(merged, &mut plan)?;
    plan.changed_paths.sort();
    plan.changed_paths.dedup();
    plan.target_settings_ids.sort();
    plan.target_settings_ids.dedup();
    Ok(plan)
}

fn append_recreated_reference_pushes(
    desired: &ProjectSnapshot,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    let targeted = plan
        .target_settings_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut recreated = plan.recreated_settings_ids.clone();
    recreated.extend(
        plan.instance_deletes
            .iter()
            .flat_map(|change| &change.instances)
            .map(|instance| instance.settings_id.as_str())
            .filter(|settings_id| targeted.contains(settings_id))
            .map(str::to_string),
    );
    recreated.extend(plan.previous_class_names.keys().cloned());
    if recreated.is_empty() {
        return Ok(());
    }

    for (path, entry) in &desired.entries {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            continue;
        }
        let document = settings_document(Some(entry))?;
        for instance in &document.instances {
            if instance.class_name == "PackageLink"
                || !instance
                    .properties
                    .values()
                    .chain(instance.attributes.values())
                    .any(|value| value_references_any(value, &recreated))
            {
                continue;
            }
            plan.changed_paths.push(path.clone());
            plan.target_settings_ids.push(instance.settings_id.clone());
        }
    }
    Ok(())
}

fn value_references_any(value: &Value, settings_ids: &HashSet<String>) -> bool {
    match value {
        Value::Array(values) => values
            .iter()
            .any(|value| value_references_any(value, settings_ids)),
        Value::Object(object) => {
            let direct = object.get("_type").and_then(Value::as_str) == Some("Ref")
                && object
                    .get("settingsId")
                    .or_else(|| object.get("instanceId"))
                    .and_then(Value::as_str)
                    .is_some_and(|settings_id| settings_ids.contains(settings_id));
            direct
                || object
                    .values()
                    .any(|value| value_references_any(value, settings_ids))
        }
        _ => false,
    }
}

fn append_settings_push_plan(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    let mut observed = observed.clone();
    let service = settings_service_name(path, desired, &observed)?;
    if !align_settings_ids_to_reference(desired, &mut observed) {
        bail!("Could not align Studio identities in {service}; Studio was not changed");
    }
    align_equivalent_values(desired, &mut observed);
    append_aligned_settings_push_plan(path, desired, &observed, plan)
}

fn settings_service_name(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
) -> Result<String> {
    desired
        .instances
        .iter()
        .chain(&observed.instances)
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .with_context(|| format!("{} has no service root", path.display()))
}

fn append_aligned_settings_push_plan(
    path: &Path,
    desired: &SettingsBytecode,
    observed: &SettingsBytecode,
    plan: &mut ReconcilePushPlan,
) -> Result<()> {
    let service = settings_service_name(path, desired, observed)?;
    let database = rbx_reflection_database::get()?;
    let phase = Instant::now();
    let (desired_by_id, observed_by_id) = rayon::join(
        || {
            desired
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.as_str(), index))
                .collect::<HashMap<_, _>>()
        },
        || {
            observed
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (instance.settings_id.as_str(), index))
                .collect::<HashMap<_, _>>()
        },
    );
    log_reconcile_timing("settings delta identity maps", phase);
    let phase = Instant::now();
    let mut previous_path_settings_ids = Vec::new();
    let mut pending_property_removals = Vec::new();

    struct InstanceDelta {
        desired_index: usize,
        observed_index: Option<usize>,
        requires_previous_path: bool,
        reset_properties: Vec<String>,
        deleted_attributes: Vec<String>,
    }

    let instance_deltas = desired
        .instances
        .par_iter()
        .enumerate()
        .filter_map(|(desired_index, instance)| {
            if instance.class_name == "PackageLink" {
                return None;
            }
            let observed_index = observed_by_id.get(instance.settings_id.as_str()).copied();
            if observed_index.is_some_and(|observed_index| {
                settings_instances_equal(desired, desired_index, observed, observed_index)
            }) {
                return None;
            }
            let Some(observed_index) = observed_index else {
                return Some(InstanceDelta {
                    desired_index,
                    observed_index: None,
                    requires_previous_path: false,
                    reset_properties: Vec::new(),
                    deleted_attributes: Vec::new(),
                });
            };
            let observed_instance = &observed.instances[observed_index];
            let (reset_properties, deleted_attributes) =
                if observed_instance.class_name == instance.class_name {
                    (
                        observed_instance
                            .properties
                            .keys()
                            .filter(|name| {
                                name.as_str() != "ScriptGuid"
                                    && !reconciliation_property_is_derived(name)
                                    // Resets obey the same capability rules as writes.
                                    // Engine-derived fields (for example cooked mesh
                                    // data after switching to Box) still participate
                                    // in the complete post-push verification.
                                    && !crate::editor::review::is_engine_managed_editor_property(
                                        &instance.class_name, name, database,
                                    )
                                    && !instance.properties.contains_key(*name)
                            })
                            .cloned()
                            .collect(),
                        observed_instance
                            .attributes
                            .keys()
                            .filter(|name| !instance.attributes.contains_key(*name))
                            .cloned()
                            .collect(),
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
            Some(InstanceDelta {
                desired_index,
                observed_index: Some(observed_index),
                requires_previous_path: observed_instance.name != instance.name
                    || observed_instance.class_name != instance.class_name
                    || settings_parent_id(observed, observed_index)
                        != settings_parent_id(desired, desired_index),
                reset_properties,
                deleted_attributes,
            })
        })
        .collect::<Vec<_>>();
    for delta in instance_deltas {
        let instance = &desired.instances[delta.desired_index];
        plan.target_settings_ids.push(instance.settings_id.clone());
        let Some(observed_index) = delta.observed_index else {
            plan.recreated_settings_ids
                .insert(instance.settings_id.clone());
            continue;
        };
        let observed_instance = &observed.instances[observed_index];
        let geometry =
            crate::editor::native_geometry::generated_properties(observed_instance, instance);
        if !geometry.is_empty() {
            plan.geometry_properties
                .insert((service.clone(), instance.settings_id.clone()), geometry);
        }
        if delta.requires_previous_path {
            previous_path_settings_ids.push(instance.settings_id.as_str());
        }
        if observed_instance.class_name != instance.class_name {
            plan.previous_class_names.insert(
                instance.settings_id.clone(),
                observed_instance.class_name.clone(),
            );
        } else if !delta.reset_properties.is_empty() || !delta.deleted_attributes.is_empty() {
            pending_property_removals.push((
                delta.desired_index,
                delta.reset_properties,
                delta.deleted_attributes,
            ));
        }
    }
    log_reconcile_timing("settings delta changed instances", phase);

    let phase = Instant::now();
    let previous_indices = previous_path_settings_ids
        .iter()
        .filter_map(|settings_id| observed_by_id.get(*settings_id).copied())
        .collect::<Vec<_>>();
    if !previous_indices.is_empty() {
        let previous_paths =
            build_editor_instance_paths_for_indices(observed, &service, &previous_indices);
        for index in previous_indices {
            if let Some(path) = previous_paths.get(&index).cloned() {
                plan.previous_paths.insert(
                    (
                        service.clone(),
                        observed.instances[index].settings_id.clone(),
                    ),
                    path,
                );
            }
        }
    }
    log_reconcile_timing("settings delta previous paths", phase);

    let phase = Instant::now();
    if !pending_property_removals.is_empty() {
        let removal_indices = pending_property_removals
            .iter()
            .map(|(index, _, _)| *index)
            .collect::<Vec<_>>();
        let desired_paths =
            build_editor_instance_paths_for_indices(desired, &service, &removal_indices);
        for (desired_index, reset_properties, deleted_attributes) in pending_property_removals {
            let instance = &desired.instances[desired_index];
            let path_info = desired_paths
                .get(&desired_index)
                .cloned()
                .with_context(|| format!("Could not locate {} in {}", instance.name, service))?;
            plan.property_removals.push(EditorPropertyChange {
                service: service.clone(),
                settings_id: Some(instance.settings_id.clone()),
                path_segments: path_info.path_segments,
                path_ordinals: path_info.path_ordinals,
                class_name: instance.class_name.clone(),
                properties: Map::new(),
                reset_properties,
                attributes: Map::new(),
                deleted_attributes,
            });
        }
    }
    log_reconcile_timing("settings delta property removals", phase);

    let phase = Instant::now();
    let removed = observed
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| {
            instance.parent_index.is_some()
                && !desired_by_id.contains_key(instance.settings_id.as_str())
        })
        .map(|(index, _)| index)
        .collect::<HashSet<_>>();
    if removed.is_empty() {
        log_reconcile_timing("settings delta instance removals", phase);
        return Ok(());
    }
    let root_removals = removed
        .iter()
        .copied()
        .filter(|index| {
            observed.instances[*index]
                .parent_index
                .is_none_or(|parent| !removed.contains(&parent))
        })
        .collect::<Vec<_>>();
    if root_removals.is_empty() {
        log_reconcile_timing("settings delta instance removals", phase);
        return Ok(());
    }
    let observed_paths =
        build_editor_instance_paths_for_indices(observed, &service, &root_removals);
    let mut descriptors = Vec::with_capacity(root_removals.len());
    for index in root_removals {
        if observed.instances[index].class_name == "PackageLink" {
            bail!(
                "Reconciliation would remove PackageLink {} directly; Studio was not changed",
                observed.instances[index].name
            );
        }
        let path_info = observed_paths.get(&index).cloned().with_context(|| {
            format!(
                "Could not locate removed instance {} in {}",
                observed.instances[index].name, service
            )
        })?;
        descriptors.push(
            editor_instance_descriptor_for_known_path(
                observed,
                index,
                path_info.path_segments,
                path_info.path_ordinals,
            )
            .context("Failed to describe a reconciled instance removal")?,
        );
    }
    plan.instance_deletes.push(EditorInstanceChange {
        mode: "deleteInstances".to_string(),
        service,
        allow_deletes: false,
        instances: descriptors,
        preserve_instances: Vec::new(),
    });
    log_reconcile_timing("settings delta instance removals", phase);
    Ok(())
}

fn capture_snapshot(root: &Path, roots: &[PathBuf]) -> Result<ProjectSnapshot> {
    let mut entries = BTreeMap::new();
    for relative_root in roots {
        if derived_project_path(relative_root) {
            continue;
        }
        let path = root.join(relative_root);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            entries.insert(
                relative_root.clone(),
                SnapshotEntry::Symlink {
                    target: fs::read_link(&path)?,
                    directory: fs::metadata(&path).is_ok_and(|target| target.is_dir()),
                },
            );
            continue;
        }
        if metadata.is_file() {
            entries.insert(relative_root.clone(), SnapshotEntry::File(fs::read(&path)?));
            continue;
        }
        if !metadata.is_dir() {
            bail!("Unsupported project entry {}", path.display());
        }
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(root)?.to_path_buf();
            if derived_project_path(&relative) {
                continue;
            }
            let value = if entry.file_type().is_dir() {
                SnapshotEntry::Directory
            } else if entry.file_type().is_file() {
                SnapshotEntry::File(fs::read(entry.path())?)
            } else if entry.file_type().is_symlink() {
                SnapshotEntry::Symlink {
                    target: fs::read_link(entry.path())?,
                    directory: fs::metadata(entry.path()).is_ok_and(|target| target.is_dir()),
                }
            } else {
                continue;
            };
            entries.insert(relative, value);
        }
    }
    Ok(ProjectSnapshot { entries })
}

#[cfg(test)]
fn merge_snapshots(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    preference: ConflictPreference,
) -> Result<(ProjectSnapshot, Vec<String>)> {
    let (merged, conflicts, _) =
        merge_snapshots_with_changes(baseline, editor, studio, preference, None)?;
    Ok((merged, conflicts))
}

fn merge_snapshots_with_changes(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    preference: ConflictPreference,
    known_differences: Option<&HashSet<PathBuf>>,
) -> Result<(ProjectSnapshot, Vec<String>, MergeChanges)> {
    let sides = SnapshotSides {
        baseline,
        editor,
        studio,
    };
    let mut paths = baseline
        .into_iter()
        .flat_map(|snapshot| snapshot.entries.keys())
        .chain(editor.entries.keys())
        .chain(studio.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut changes = MergeChanges::default();
    for path in paths {
        let base = baseline.and_then(|snapshot| snapshot.entries.get(&path));
        let editor_entry = editor.entries.get(&path);
        let studio_entry = studio.entries.get(&path);
        let result = if known_differences.is_some_and(|paths| !paths.contains(&path)) {
            MergedEntry {
                value: editor_entry.cloned(),
                editor_changed: false,
                studio_changed: false,
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            merge_settings_entry(
                &path,
                &sides,
                preference,
                baseline.is_none(),
                &mut conflicts,
            )?
        } else {
            merge_entry(
                &path,
                base,
                editor_entry,
                studio_entry,
                preference,
                baseline.is_none(),
                &mut conflicts,
            )
        };
        if result.editor_changed {
            changes.editor.insert(path.clone());
        }
        if result.studio_changed {
            changes.studio.insert(path.clone());
        }
        if let Some(value) = result.value {
            merged.insert(path, value);
        }
    }
    remove_superseded_script_paths(baseline, editor, studio, &mut merged, &mut changes)?;
    conflicts.sort();
    conflicts.dedup();
    Ok((ProjectSnapshot { entries: merged }, conflicts, changes))
}

fn script_paths_by_settings_id(
    entries: &BTreeMap<PathBuf, SnapshotEntry>,
    settings_path: &Path,
) -> Result<HashMap<String, PathBuf>> {
    let Some(entry) = entries.get(settings_path) else {
        return Ok(HashMap::new());
    };
    let document = settings_document(Some(entry))?;
    let source_paths = source_paths_by_settings_index(&document, settings_path)?;
    Ok(document
        .instances
        .iter()
        .zip(source_paths)
        .filter_map(|(instance, path)| path.map(|path| (instance.settings_id.clone(), path)))
        .collect())
}

fn source_paths_by_settings_index(
    document: &SettingsBytecode,
    settings_path: &Path,
) -> Result<Vec<Option<PathBuf>>> {
    let service_dir = settings_path
        .parent()
        .context("A service settings path has no parent")?;
    let service = service_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("A service settings path has no service name")?;
    Ok(build_editor_source_paths_by_index(
        document,
        service,
        service_dir,
    ))
}

fn changed_script_source_ids(
    settings_path: &Path,
    baseline: &SettingsBytecode,
    baseline_entries: &BTreeMap<PathBuf, SnapshotEntry>,
    observed: &SettingsBytecode,
    observed_entries: &BTreeMap<PathBuf, SnapshotEntry>,
) -> Result<HashSet<String>> {
    let baseline_indices = baseline
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let baseline_paths = source_paths_by_settings_index(baseline, settings_path)?;
    let observed_paths = source_paths_by_settings_index(observed, settings_path)?;
    Ok(observed
        .instances
        .iter()
        .zip(observed_paths)
        .filter_map(|(instance, observed_path)| {
            let observed_path = observed_path?;
            let baseline_entry = baseline_indices
                .get(instance.settings_id.as_str())
                .and_then(|index| baseline_paths[*index].as_ref())
                .and_then(|path| baseline_entries.get(path));
            let observed_entry = observed_entries.get(&observed_path);
            (!entries_equivalent(&observed_path, baseline_entry, observed_entry))
                .then(|| instance.settings_id.clone())
        })
        .collect())
}

fn remove_superseded_script_paths(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    merged: &mut BTreeMap<PathBuf, SnapshotEntry>,
    changes: &mut MergeChanges,
) -> Result<()> {
    let settings_paths = changes
        .editor
        .iter()
        .chain(changes.studio.iter())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_service_settings_file_name)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut touched = BTreeSet::new();
    for settings_path in settings_paths {
        let baseline_paths = baseline
            .map(|snapshot| script_paths_by_settings_id(&snapshot.entries, &settings_path))
            .transpose()?
            .unwrap_or_default();
        let editor_paths = script_paths_by_settings_id(&editor.entries, &settings_path)?;
        let studio_paths = script_paths_by_settings_id(&studio.entries, &settings_path)?;
        let merged_paths = script_paths_by_settings_id(merged, &settings_path)?;
        let desired_paths = merged_paths.values().collect::<HashSet<_>>();
        let ids = baseline_paths
            .keys()
            .chain(editor_paths.keys())
            .chain(studio_paths.keys())
            .cloned()
            .collect::<HashSet<_>>();
        for id in ids {
            let desired = merged_paths.get(&id);
            for path in [
                baseline_paths.get(&id),
                editor_paths.get(&id),
                studio_paths.get(&id),
            ]
            .into_iter()
            .flatten()
            {
                if desired == Some(path) || desired_paths.contains(path) {
                    continue;
                }
                merged.remove(path);
                touched.insert(path.clone());
            }
        }
    }
    for path in touched {
        let merged_entry = merged.get(&path);
        if entries_equivalent(&path, editor.entries.get(&path), merged_entry) {
            changes.editor.remove(&path);
        } else {
            changes.editor.insert(path.clone());
        }
        if entries_equivalent(&path, studio.entries.get(&path), merged_entry) {
            changes.studio.remove(&path);
        } else {
            changes.studio.insert(path);
        }
    }
    Ok(())
}

fn merge_entry(
    path: &Path,
    base: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> MergedEntry {
    let value = if entries_equivalent(path, editor, studio) {
        editor.cloned()
    } else {
        if !first_pairing {
            if entries_equivalent(path, editor, base) {
                studio.cloned()
            } else if entries_equivalent(path, studio, base) {
                editor.cloned()
            } else {
                merge_conflicting_entry(
                    path,
                    base,
                    editor,
                    studio,
                    preference,
                    first_pairing,
                    conflicts,
                )
            }
        } else if editor.is_none() {
            studio.cloned()
        } else if studio.is_none() {
            editor.cloned()
        } else {
            merge_conflicting_entry(
                path,
                base,
                editor,
                studio,
                preference,
                first_pairing,
                conflicts,
            )
        }
    };
    MergedEntry {
        editor_changed: !entries_equivalent(path, editor, value.as_ref()),
        studio_changed: !entries_equivalent(path, studio, value.as_ref()),
        value,
    }
}

fn merge_conflicting_entry(
    path: &Path,
    base: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> Option<SnapshotEntry> {
    let ordinary_file_conflict = (first_pairing
        && matches!(editor, Some(SnapshotEntry::File(_)))
        && matches!(studio, Some(SnapshotEntry::File(_))))
        || (matches!(base, Some(SnapshotEntry::File(_)))
            && matches!(editor, None | Some(SnapshotEntry::File(_)))
            && matches!(studio, None | Some(SnapshotEntry::File(_))));
    if ordinary_file_conflict {
        match preference {
            ConflictPreference::Editor => return editor.cloned(),
            ConflictPreference::Studio => return studio.cloned(),
            ConflictPreference::None => {}
        }
    }
    conflicts.push(format!("{} changed on both sides", path.display()));
    editor.cloned()
}

fn is_source_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "lua" | "luau"))
}

fn entries_equivalent(
    path: &Path,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
) -> bool {
    match (left, right) {
        (None | Some(SnapshotEntry::Directory), None | Some(SnapshotEntry::Directory)) => true,
        (Some(SnapshotEntry::File(left)), Some(SnapshotEntry::File(right)))
            if is_source_path(path) =>
        {
            left == right
                || crate::system::text::normalized_source_bytes(left)
                    .eq(crate::system::text::normalized_source_bytes(right))
        }
        _ => left == right,
    }
}

fn decoded_settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    let phase = Instant::now();
    let mut document = match entry {
        Some(SnapshotEntry::File(bytes)) => decode_settings_bytecode(bytes),
        None => Ok(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        }),
        Some(_) => bail!("A Renium settings store is not a regular file"),
    }?;
    log_reconcile_timing("settings document decode", phase);
    let phase = Instant::now();
    stabilize_settings_reference_ids(&mut document);
    log_reconcile_timing("settings document reference stabilization", phase);
    Ok(document)
}

fn editor_settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    decoded_settings_document(entry)
}

fn settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    let mut document = decoded_settings_document(entry)?;
    let phase = Instant::now();
    canonicalize_settings_property_names(&mut document)?;
    log_reconcile_timing("settings document property canonicalization", phase);
    Ok(document)
}

fn short_reconciliation_value(value: Option<&Value>) -> String {
    let text = value.map_or_else(|| "<absent>".to_string(), Value::to_string);
    if text.len() <= 96 {
        text
    } else {
        format!("{}...", text.chars().take(93).collect::<String>())
    }
}

fn first_three_way_property_difference(
    baseline: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &SettingsBytecode,
) -> Option<String> {
    let editor_by_id = editor
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance))
        .collect::<HashMap<_, _>>();
    let studio_by_id = studio
        .instances
        .iter()
        .map(|instance| (instance.settings_id.as_str(), instance))
        .collect::<HashMap<_, _>>();
    for base in &baseline.instances {
        let (Some(editor), Some(studio)) = (
            editor_by_id.get(base.settings_id.as_str()).copied(),
            studio_by_id.get(base.settings_id.as_str()).copied(),
        ) else {
            continue;
        };
        let keys = base
            .properties
            .keys()
            .chain(editor.properties.keys())
            .chain(studio.properties.keys())
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for name in keys {
            let baseline_value = reconciliation_property_value(&base.properties, name);
            let editor_value = reconciliation_property_value(&editor.properties, name);
            let studio_value = reconciliation_property_value(&studio.properties, name);
            let editor_changed = !reconciliation_property_values_equal(
                &base.class_name,
                name,
                baseline_value,
                editor_value,
            );
            let studio_changed = !reconciliation_property_values_equal(
                &base.class_name,
                name,
                baseline_value,
                studio_value,
            );
            if editor_changed || studio_changed {
                return Some(format!(
                    "{} ({}) {name}: baseline={} editor={} studio={} editor_changed={editor_changed} studio_changed={studio_changed}",
                    base.name,
                    base.settings_id,
                    short_reconciliation_value(baseline_value),
                    short_reconciliation_value(editor_value),
                    short_reconciliation_value(studio_value),
                ));
            }
        }
    }
    None
}

fn merge_settings_entry(
    path: &Path,
    sides: &SnapshotSides<'_>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> Result<MergedEntry> {
    let base = sides
        .baseline
        .and_then(|snapshot| snapshot.entries.get(path));
    let editor = sides.editor.entries.get(path);
    let studio = sides.studio.entries.get(path);
    if editor == studio {
        return Ok(MergedEntry {
            value: editor.cloned(),
            editor_changed: false,
            studio_changed: false,
        });
    }
    let mut editor_doc = settings_document(editor)?;
    let mut studio_doc = settings_document(studio)?;
    if settings_documents_equivalent(&editor_doc, &studio_doc) {
        return Ok(MergedEntry {
            value: editor.cloned(),
            editor_changed: false,
            studio_changed: false,
        });
    }
    remove_reconciliation_derived_properties(&mut editor_doc);
    remove_reconciliation_derived_properties(&mut studio_doc);
    let (merged, editor_equivalent, studio_equivalent) = if first_pairing {
        let previous_conflicts = conflicts.len();
        align_first_pairing(
            path,
            &mut editor_doc,
            &mut studio_doc,
            preference,
            conflicts,
        );
        align_reconciliation_protected_workspace_cameras(&editor_doc, &mut studio_doc);
        if conflicts.len() > previous_conflicts {
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &editor_doc);
            (editor_doc, true, studio_equivalent)
        } else {
            let no_source_changes = HashSet::new();
            let empty = SettingsBytecode {
                version: editor_doc.version.max(studio_doc.version),
                instances: Vec::new(),
            };
            let (merged, merge_conflicts) = merge_reconciliation_settings_documents(
                &empty,
                &editor_doc,
                &studio_doc,
                preference,
                &no_source_changes,
                &no_source_changes,
            );
            conflicts.extend(
                merge_conflicts
                    .into_iter()
                    .map(|conflict| format!("{}: {}", path.display(), conflict.detail)),
            );
            let editor_equivalent = settings_documents_equivalent(&editor_doc, &merged);
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &merged);
            (merged, editor_equivalent, studio_equivalent)
        }
    } else {
        let mut base_doc = settings_document(base)?;
        remove_reconciliation_derived_properties(&mut base_doc);
        align_observation_ids_to_baseline(&base_doc, &mut editor_doc);
        align_observation_ids_to_baseline(&base_doc, &mut studio_doc);
        align_new_instance_ids(&base_doc, &editor_doc, &mut studio_doc)?;
        align_reconciliation_protected_workspace_cameras(&editor_doc, &mut studio_doc);
        align_equivalent_values(&base_doc, &mut editor_doc);
        align_equivalent_values(&base_doc, &mut studio_doc);
        align_equivalent_values(&editor_doc, &mut studio_doc);
        protect_package_links(
            path,
            Some(&base_doc),
            &mut editor_doc,
            &mut studio_doc,
            conflicts,
        );
        align_transient_script_guids(&mut base_doc, &mut editor_doc, &mut studio_doc);
        if global_log_enabled(5) {
            log_global(
                5,
                format_args!(
                    "[renium] reconcile first property difference in {}: {}",
                    path.display(),
                    first_three_way_property_difference(&base_doc, &editor_doc, &studio_doc)
                        .unwrap_or_else(|| "none".to_string())
                ),
            );
        }
        let baseline_entries = &sides
            .baseline
            .context("A reconciliation baseline is missing")?
            .entries;
        let editor_source_changes = changed_script_source_ids(
            path,
            &base_doc,
            baseline_entries,
            &editor_doc,
            &sides.editor.entries,
        )?;
        let studio_source_changes = changed_script_source_ids(
            path,
            &base_doc,
            baseline_entries,
            &studio_doc,
            &sides.studio.entries,
        )?;
        if settings_documents_equivalent(&editor_doc, &studio_doc) {
            (editor_doc, true, true)
        } else if editor_source_changes.is_empty()
            && settings_documents_equivalent(&editor_doc, &base_doc)
        {
            (studio_doc, false, true)
        } else if studio_source_changes.is_empty()
            && settings_documents_equivalent(&studio_doc, &base_doc)
        {
            (editor_doc, true, false)
        } else {
            let (merged, merge_conflicts) = merge_reconciliation_settings_documents(
                &base_doc,
                &editor_doc,
                &studio_doc,
                preference,
                &editor_source_changes,
                &studio_source_changes,
            );
            conflicts.extend(
                merge_conflicts
                    .into_iter()
                    .map(|conflict| format!("{}: {}", path.display(), conflict.detail)),
            );
            let editor_equivalent = settings_documents_equivalent(&editor_doc, &merged);
            let studio_equivalent = settings_documents_equivalent(&studio_doc, &merged);
            (merged, editor_equivalent, studio_equivalent)
        }
    };
    let value = if merged.instances.is_empty() && editor.is_none() && studio.is_none() {
        None
    } else {
        Some(SnapshotEntry::File(
            encode_settings_bytecode(&merged)
                .with_context(|| format!("Failed to merge {}", path.display()))?,
        ))
    };
    let editor_changed = match editor {
        Some(_) => !editor_equivalent,
        None => value.is_some(),
    };
    let studio_changed = match studio {
        Some(_) => !studio_equivalent,
        None => value.is_some(),
    };
    Ok(MergedEntry {
        value,
        editor_changed,
        studio_changed,
    })
}

fn merge_reconciliation_settings_documents(
    base: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &SettingsBytecode,
    preference: ConflictPreference,
    editor_source_changes: &HashSet<String>,
    studio_source_changes: &HashSet<String>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    let prefer_studio = preference_bool(preference).map(|prefer_editor| !prefer_editor);
    let mut interner = PathInterner::new();
    let editor_paths = structural_path_ids(editor, &mut interner);
    let studio_paths = structural_path_ids(studio, &mut interner);
    let editor_by_id = editor
        .instances
        .iter()
        .zip(editor_paths)
        .map(|(instance, path)| (instance.settings_id.as_str(), path))
        .collect::<HashMap<_, _>>();
    let aligned_additions = studio
        .instances
        .iter()
        .zip(studio_paths)
        .filter(|(instance, path)| editor_by_id.get(instance.settings_id.as_str()) == Some(path))
        .map(|(instance, _)| instance.settings_id.clone())
        .collect();
    merge_aligned_settings_documents(
        base,
        studio,
        editor,
        prefer_studio,
        studio_source_changes,
        editor_source_changes,
        &aligned_additions,
    )
}

fn preference_bool(preference: ConflictPreference) -> Option<bool> {
    match preference {
        ConflictPreference::None => None,
        ConflictPreference::Editor => Some(true),
        ConflictPreference::Studio => Some(false),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct StructuralPart {
    name: String,
    class_name: String,
    ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PairingPart {
    name: String,
    ordinal: usize,
}

type PathId = usize;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PathNode<P> {
    parent: Option<PathId>,
    part: P,
}

struct PathInterner<P> {
    nodes: Vec<PathNode<P>>,
    ids: HashMap<PathNode<P>, PathId>,
}

impl<P> PathInterner<P>
where
    P: Clone + Eq + std::hash::Hash,
{
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            ids: HashMap::new(),
        }
    }

    fn intern(&mut self, parent: Option<PathId>, part: P) -> PathId {
        let node = PathNode { parent, part };
        if let Some(id) = self.ids.get(&node) {
            return *id;
        }
        let id = self.nodes.len();
        self.nodes.push(node.clone());
        self.ids.insert(node, id);
        id
    }

    fn find(&self, parent: Option<PathId>, part: &P) -> Option<PathId> {
        self.ids
            .get(&PathNode {
                parent,
                part: part.clone(),
            })
            .copied()
    }

    fn node(&self, id: PathId) -> &PathNode<P> {
        &self.nodes[id]
    }
}

fn intern_paths<P>(
    document: &SettingsBytecode,
    ordinals: &[usize],
    interner: &mut PathInterner<P>,
    make_part: impl Fn(&SettingsBytecodeInstance, usize) -> P,
) -> Vec<PathId>
where
    P: Clone + Eq + std::hash::Hash,
{
    let mut ids = vec![None; document.instances.len()];
    let mut chain = Vec::new();
    for start in 0..document.instances.len() {
        if ids[start].is_some() {
            continue;
        }
        chain.clear();
        let mut current = start;
        let mut parent_id = loop {
            if let Some(id) = ids[current] {
                break Some(id);
            }
            chain.push(current);
            let Some(parent) = document.instances[current].parent_index else {
                break None;
            };
            current = parent;
        };
        while let Some(index) = chain.pop() {
            let id = interner.intern(
                parent_id,
                make_part(&document.instances[index], ordinals[index]),
            );
            ids[index] = Some(id);
            parent_id = Some(id);
        }
    }
    ids.into_iter().map(Option::unwrap).collect()
}

fn structural_path_ids(
    document: &SettingsBytecode,
    interner: &mut PathInterner<StructuralPart>,
) -> Vec<PathId> {
    let mut ordinals = Vec::with_capacity(document.instances.len());
    let mut counts = HashMap::<(Option<usize>, &str, &str), usize>::new();
    for instance in &document.instances {
        let count = counts
            .entry((
                instance.parent_index,
                instance.name.as_str(),
                instance.class_name.as_str(),
            ))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        ordinals.push(*count);
    }
    intern_paths(document, &ordinals, interner, |instance, ordinal| {
        StructuralPart {
            name: instance.name.clone(),
            class_name: instance.class_name.clone(),
            ordinal,
        }
    })
}

fn pairing_path_ids(
    document: &SettingsBytecode,
    interner: &mut PathInterner<PairingPart>,
) -> Vec<PathId> {
    let mut ordinals = Vec::with_capacity(document.instances.len());
    let mut counts = HashMap::<(Option<usize>, &str), usize>::new();
    for instance in &document.instances {
        let ordinal = counts
            .entry((instance.parent_index, instance.name.as_str()))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        ordinals.push(*ordinal);
    }
    intern_paths(document, &ordinals, interner, |instance, ordinal| {
        PairingPart {
            name: instance.name.clone(),
            ordinal,
        }
    })
}

fn align_observation_ids_to_baseline(baseline: &SettingsBytecode, observed: &mut SettingsBytecode) {
    align_settings_ids_to_reference(baseline, observed);
}

fn align_new_instance_ids(
    baseline: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &mut SettingsBytecode,
) -> Result<()> {
    let baseline_ids = baseline
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let mut interner = PathInterner::new();
    let editor_keys = structural_path_ids(editor, &mut interner);
    let studio_keys = structural_path_ids(studio, &mut interner);
    let editor_by_key = editor_keys
        .iter()
        .copied()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let studio_by_key = studio_keys
        .iter()
        .copied()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let candidates = studio_keys
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(studio_index, key)| {
            let editor_index = editor_by_key.get(&key).copied()?;
            let editor_id = editor.instances[editor_index].settings_id.as_str();
            let studio_id = studio.instances[studio_index].settings_id.as_str();
            (!baseline_ids.contains(editor_id)
                && !baseline_ids.contains(studio_id)
                && editor_id != studio_id)
                .then_some((studio_index, editor_index))
        })
        .collect::<Vec<_>>();
    let mut remap = candidates
        .iter()
        .map(|(studio_index, editor_index)| {
            (
                studio.instances[*studio_index].settings_id.clone(),
                editor.instances[*editor_index].settings_id.clone(),
            )
        })
        .collect::<HashMap<_, _>>();
    let targets = remap.values().cloned().collect::<HashSet<_>>();
    let mut all_ids = editor
        .instances
        .iter()
        .chain(&studio.instances)
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut seed = all_ids.len();
    for instance in &studio.instances {
        let id = &instance.settings_id;
        if targets.contains(id) && !remap.contains_key(id) {
            remap.insert(
                id.clone(),
                crate::bytecode::edit::next_editor_settings_id_fast(&mut all_ids, &mut seed),
            );
        }
    }
    // New identities are paired by their unique structural location, not by value.
    // Requiring equal values turns a legitimate property difference into two instances.
    // Duplicate-name slots still require equivalent data: their ordinal alone is not identity.
    for (studio_index, editor_index) in candidates {
        let key = studio_keys[studio_index];
        let node = interner.node(key);
        let mut second = node.part.clone();
        second.ordinal = 2;
        let duplicate_slot = node.part.ordinal > 1
            || interner.find(node.parent, &second).is_some_and(|second| {
                editor_by_key.contains_key(&second) || studio_by_key.contains_key(&second)
            });
        if !duplicate_slot {
            continue;
        }
        let observed = &studio.instances[studio_index];
        let desired = &editor.instances[editor_index];
        let mut properties = observed.properties.clone();
        let mut attributes = observed.attributes.clone();
        remap_record_reference_ids(&mut properties, &remap);
        remap_record_reference_ids(&mut attributes, &remap);
        if !reconciliation_maps_equal(&desired.class_name, &desired.properties, &properties)
            || !reconciliation_values_map_equal(&desired.attributes, &attributes)
        {
            bail!(
                "Ambiguous new duplicate instances at {}; Studio was not changed",
                render_structural_key(&interner, key)
            );
        }
    }
    for instance in &mut studio.instances {
        if let Some(id) = remap.get(&instance.settings_id) {
            instance.settings_id.clone_from(id);
        }
        remap_record_reference_ids(&mut instance.properties, &remap);
        remap_record_reference_ids(&mut instance.attributes, &remap);
    }
    Ok(())
}

fn align_first_pairing(
    path: &Path,
    editor: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
    preference: ConflictPreference,
    conflicts: &mut Vec<String>,
) {
    let mut structural_interner = PathInterner::new();
    let editor_keys = structural_path_ids(editor, &mut structural_interner);
    let studio_keys = structural_path_ids(studio, &mut structural_interner);
    let mut pairing_interner = PathInterner::new();
    let editor_pairing_keys = pairing_path_ids(editor, &mut pairing_interner);
    let studio_pairing_keys = pairing_path_ids(studio, &mut pairing_interner);
    let editor_by_key = editor_keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let studio_by_key = studio_keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    let stable_pairs = editor_by_key
        .iter()
        .filter_map(|(key, editor_index)| {
            studio_by_key.get(key).and_then(|studio_index| {
                (editor.instances[*editor_index].settings_id
                    == studio.instances[*studio_index].settings_id)
                    .then_some(*key)
            })
        })
        .collect::<HashSet<_>>();
    let editor_by_pairing_key = editor_pairing_keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, key)| (key, index))
        .collect::<HashMap<_, _>>();
    for (studio_index, key) in studio_pairing_keys.iter().enumerate() {
        if let Some(editor_index) = editor_by_pairing_key.get(key).copied()
            && editor.instances[editor_index].class_name
                != studio.instances[studio_index].class_name
        {
            conflicts.push(format!(
                "{} has different classes for {}",
                path.display(),
                render_pairing_key(&pairing_interner, *key)
            ));
        }
    }
    let editor_key_by_id = editor
        .instances
        .iter()
        .zip(&editor_keys)
        .map(|(instance, key)| (instance.settings_id.clone(), key))
        .collect::<HashMap<_, _>>();
    for (instance, key) in studio.instances.iter().zip(&studio_keys) {
        if editor_key_by_id
            .get(&instance.settings_id)
            .is_some_and(|editor_key| *editor_key != key)
        {
            conflicts.push(format!(
                "{} has the same instance identity at different paths",
                path.display()
            ));
        }
    }

    let id_remap = editor_by_key
        .iter()
        .filter_map(|(key, editor_index)| {
            studio_by_key.get(key).map(|studio_index| {
                (
                    studio.instances[*studio_index].settings_id.clone(),
                    editor.instances[*editor_index].settings_id.clone(),
                )
            })
        })
        .collect::<HashMap<_, _>>();
    for instance in &mut studio.instances {
        if let Some(id) = id_remap.get(&instance.settings_id) {
            instance.settings_id.clone_from(id);
        }
        remap_record_reference_ids(&mut instance.properties, &id_remap);
        remap_record_reference_ids(&mut instance.attributes, &id_remap);
    }
    align_equivalent_values(editor, studio);
    protect_package_links(path, None, editor, studio, conflicts);

    for (key, editor_index) in &editor_by_key {
        let Some(studio_index) = studio_by_key.get(key).copied() else {
            continue;
        };
        if settings_instances_equal(editor, *editor_index, studio, studio_index) {
            continue;
        }
        let node = structural_interner.node(*key);
        let duplicate_slot = if node.part.ordinal > 1 {
            true
        } else {
            let mut second = node.part.clone();
            second.ordinal = 2;
            structural_interner
                .find(node.parent, &second)
                .is_some_and(|second| {
                    editor_by_key.contains_key(&second) || studio_by_key.contains_key(&second)
                })
        };
        if duplicate_slot && !stable_pairs.contains(key) {
            conflicts.push(format!(
                "{} has ambiguous duplicate instances at {}",
                path.display(),
                render_structural_key(&structural_interner, *key)
            ));
            continue;
        }
        match preference {
            ConflictPreference::None => conflicts.push(format!(
                "{} has different values for {}",
                path.display(),
                render_structural_key(&structural_interner, *key)
            )),
            ConflictPreference::Editor => {
                copy_settings_instance(editor, *editor_index, studio, studio_index)
            }
            ConflictPreference::Studio => {
                copy_settings_instance(studio, studio_index, editor, *editor_index)
            }
        }
    }
}

fn protect_package_links(
    path: &Path,
    base: Option<&SettingsBytecode>,
    editor: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
    conflicts: &mut Vec<String>,
) {
    let base_by_id = base
        .into_iter()
        .flat_map(|document| document.instances.iter().enumerate())
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.clone(), (index, instance)))
        .collect::<HashMap<_, _>>();
    let studio_by_id = studio
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let editor_by_id = editor
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();

    for (id, (base_index, base_instance)) in &base_by_id {
        if !studio_by_id.contains_key(id)
            && !base
                .is_some_and(|document| package_parent_is_missing(document, *base_index, studio))
        {
            let parent = base
                .and_then(|document| settings_parent_id(document, *base_index))
                .unwrap_or("unknown");
            conflicts.push(format!(
                "{} removes PackageLink {} ({id}) directly while parent {parent} remains in Studio",
                path.display(),
                base_instance.name
            ));
        }
        if !editor_by_id.contains_key(id)
            && studio_by_id.contains_key(id)
            && !base
                .is_some_and(|document| package_parent_is_missing(document, *base_index, editor))
        {
            conflicts.push(format!(
                "{} omits PackageLink {}; ordinary sync preserves package relationships",
                path.display(),
                base_instance.name
            ));
        }
    }

    for (id, editor_index) in editor_by_id {
        let Some(studio_index) = studio_by_id.get(&id).copied() else {
            if !base_by_id.contains_key(&id) {
                conflicts.push(format!(
                    "{} cannot create a PackageLink from project files",
                    path.display()
                ));
            }
            continue;
        };
        let editor_parent = settings_parent_id(editor, editor_index).map(str::to_string);
        let studio_parent = settings_parent_id(studio, studio_index).map(str::to_string);
        let editor_instance = &editor.instances[editor_index];
        let studio_instance = &studio.instances[studio_index];
        if editor_instance.name != studio_instance.name
            || editor_instance.class_name != studio_instance.class_name
            || editor_parent != studio_parent
        {
            conflicts.push(format!(
                "{} directly renames or reparents PackageLink {}",
                path.display(),
                studio_instance.name
            ));
            continue;
        }
        editor.instances[editor_index]
            .properties
            .clone_from(&studio_instance.properties);
        editor.instances[editor_index]
            .attributes
            .clone_from(&studio_instance.attributes);
    }
}

fn settings_parent_id(document: &SettingsBytecode, index: usize) -> Option<&str> {
    document.instances[index]
        .parent_index
        .and_then(|parent| document.instances.get(parent))
        .map(|parent| parent.settings_id.as_str())
}

fn package_parent_is_missing(
    source: &SettingsBytecode,
    package_link_index: usize,
    target: &SettingsBytecode,
) -> bool {
    settings_parent_id(source, package_link_index).is_some_and(|parent_id| {
        !target
            .instances
            .iter()
            .any(|instance| instance.settings_id == parent_id)
    })
}

fn settings_instances_equal(
    left: &SettingsBytecode,
    left_index: usize,
    right: &SettingsBytecode,
    right_index: usize,
) -> bool {
    let left_instance = &left.instances[left_index];
    let right_instance = &right.instances[right_index];
    left_instance.name == right_instance.name
        && left_instance.class_name == right_instance.class_name
        && settings_parent_id(left, left_index) == settings_parent_id(right, right_index)
        && reconciliation_maps_equal(
            &left_instance.class_name,
            &left_instance.properties,
            &right_instance.properties,
        )
        && reconciliation_values_map_equal(&left_instance.attributes, &right_instance.attributes)
}

fn copy_settings_instance(
    source: &SettingsBytecode,
    source_index: usize,
    target: &mut SettingsBytecode,
    target_index: usize,
) {
    let source = &source.instances[source_index];
    let target = &mut target.instances[target_index];
    target.name.clone_from(&source.name);
    target.class_name.clone_from(&source.class_name);
    target.properties.clone_from(&source.properties);
    target.attributes.clone_from(&source.attributes);
}

fn render_structural_key(interner: &PathInterner<StructuralPart>, id: PathId) -> String {
    render_path(interner, id, |part| {
        if part.ordinal == 1 {
            part.name.clone()
        } else {
            format!("{}[{}]", part.name, part.ordinal)
        }
    })
}

fn render_pairing_key(interner: &PathInterner<PairingPart>, id: PathId) -> String {
    render_path(interner, id, |part| {
        if part.ordinal == 1 {
            part.name.clone()
        } else {
            format!("{}[{}]", part.name, part.ordinal)
        }
    })
}

fn render_path<P>(
    interner: &PathInterner<P>,
    mut id: PathId,
    render: impl Fn(&P) -> String,
) -> String
where
    P: Clone + Eq + std::hash::Hash,
{
    let mut parts = Vec::new();
    loop {
        let node = interner.node(id);
        parts.push(render(&node.part));
        let Some(parent) = node.parent else {
            break;
        };
        id = parent;
    }
    parts.reverse();
    parts.join(".")
}

fn align_transient_script_guids(
    base: &mut SettingsBytecode,
    editor: &mut SettingsBytecode,
    studio: &mut SettingsBytecode,
) {
    let editor_by_id = editor
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let studio_by_id = studio
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for base_instance in &mut base.instances {
        let Some(editor_index) = editor_by_id.get(&base_instance.settings_id).copied() else {
            continue;
        };
        let Some(studio_index) = studio_by_id.get(&base_instance.settings_id).copied() else {
            continue;
        };
        let value = editor.instances[editor_index]
            .properties
            .get("ScriptGuid")
            .cloned()
            .or_else(|| {
                studio.instances[studio_index]
                    .properties
                    .get("ScriptGuid")
                    .cloned()
            });
        if let Some(value) = value {
            base_instance
                .properties
                .insert("ScriptGuid".to_string(), value.clone());
            editor.instances[editor_index]
                .properties
                .insert("ScriptGuid".to_string(), value.clone());
            studio.instances[studio_index]
                .properties
                .insert("ScriptGuid".to_string(), value);
        }
    }
}

#[cfg(test)]
fn snapshots_equivalent(left: &ProjectSnapshot, right: &ProjectSnapshot) -> Result<bool> {
    Ok(snapshot_differences(left, right)?.is_empty())
}

fn snapshot_path_differences(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut differences = Vec::new();
    for path in paths {
        if !snapshot_entry_equivalent(path, left.entries.get(path), right.entries.get(path), None)?
        {
            differences.push(path.clone());
        }
    }
    differences.sort();
    Ok(differences)
}

fn snapshot_intended_delta_mismatches(
    before: &ProjectSnapshot,
    desired: &ProjectSnapshot,
    observed: &ProjectSnapshot,
    paths: &HashSet<PathBuf>,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    let mut differences = Vec::new();
    let mut detail = None;
    for path in paths {
        let before_entry = before.entries.get(path);
        let desired_entry = desired.entries.get(path);
        if entries_equivalent(path, before_entry, desired_entry) {
            continue;
        }
        let mismatch = if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            settings_delta_mismatch(
                path,
                before_entry,
                desired_entry,
                observed.entries.get(path),
            )?
        } else if entries_equivalent(path, observed.entries.get(path), desired_entry) {
            None
        } else {
            Some(format!(
                "{} differs from the requested value",
                path.display()
            ))
        };
        if let Some(mismatch) = mismatch {
            differences.push(path.clone());
            if detail.is_none() {
                detail = Some(mismatch);
            }
        }
    }
    differences.sort();
    Ok((differences, detail))
}

fn settings_delta_mismatch(
    path: &Path,
    before: Option<&SnapshotEntry>,
    desired: Option<&SnapshotEntry>,
    observed: Option<&SnapshotEntry>,
) -> Result<Option<String>> {
    let mut before = settings_document(before)?;
    let desired = settings_document(desired)?;
    let mut observed = settings_document(observed)?;
    if desired.instances.is_empty()
        && !before.instances.is_empty()
        && !observed.instances.is_empty()
        && !align_settings_ids_to_reference(&before, &mut observed)
    {
        bail!(
            "Could not align the Studio identities in {}",
            path.display()
        );
    }
    if !desired.instances.is_empty() {
        if !before.instances.is_empty() && !align_settings_ids_to_reference(&desired, &mut before) {
            bail!(
                "Could not align the previous identities in {}",
                path.display()
            );
        }
        if !observed.instances.is_empty()
            && !align_settings_ids_to_reference(&desired, &mut observed)
        {
            bail!(
                "Could not align the Studio identities in {}",
                path.display()
            );
        }
    }
    let before_by_id = before
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let desired_by_id = desired
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let observed_by_id = observed
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut settings_ids = before_by_id
        .keys()
        .chain(desired_by_id.keys())
        .copied()
        .collect::<Vec<_>>();
    settings_ids.sort_unstable();
    settings_ids.dedup();
    for settings_id in settings_ids {
        let before_index = before_by_id.get(settings_id).copied();
        let desired_index = desired_by_id.get(settings_id).copied();
        let observed_index = observed_by_id.get(settings_id).copied();
        if before_index.is_some_and(|index| before.instances[index].class_name == "PackageLink")
            && desired_index
                .is_some_and(|index| desired.instances[index].class_name == "PackageLink")
        {
            continue;
        }
        let name = desired_index
            .map(|index| desired.instances[index].name.as_str())
            .or_else(|| before_index.map(|index| before.instances[index].name.as_str()))
            .unwrap_or(settings_id);
        match (before_index, desired_index, observed_index) {
            (Some(before_index), None, Some(observed_index)) => {
                // A removed service store deletes its contents, not the engine's
                // service object. Match append_aligned_settings_push_plan.
                let previous = &before.instances[before_index];
                let actual = &observed.instances[observed_index];
                if previous.parent_index.is_none()
                    && actual.parent_index.is_none()
                    && previous.class_name == actual.class_name
                    && previous.name == actual.name
                {
                    continue;
                }
                return Ok(Some(format!("{name} was not deleted from Studio")));
            }
            (None, Some(_), None) => {
                return Ok(Some(format!("{name} was not created in Studio")));
            }
            (_, None, _) => continue,
            (None, Some(desired_index), Some(observed_index)) => {
                if let Some(detail) =
                    added_instance_mismatch(&desired, desired_index, &observed, observed_index)
                {
                    return Ok(Some(format!("{name}.{detail}")));
                }
            }
            (Some(before_index), Some(desired_index), Some(observed_index)) => {
                if let Some(detail) = changed_instance_mismatch(
                    &before,
                    before_index,
                    &desired,
                    desired_index,
                    &observed,
                    observed_index,
                ) {
                    return Ok(Some(format!("{name}.{detail}")));
                }
            }
            (Some(_), Some(_), None) => {
                return Ok(Some(format!("{name} disappeared from Studio")));
            }
        }
    }
    Ok(None)
}

fn added_instance_mismatch(
    desired: &SettingsBytecode,
    desired_index: usize,
    observed: &SettingsBytecode,
    observed_index: usize,
) -> Option<String> {
    let desired_instance = &desired.instances[desired_index];
    let observed_instance = &observed.instances[observed_index];
    if desired_instance.name != observed_instance.name {
        return Some("Name was not retained".to_string());
    }
    if desired_instance.class_name != observed_instance.class_name {
        return Some("ClassName was not retained".to_string());
    }
    if settings_parent_id(desired, desired_index) != settings_parent_id(observed, observed_index) {
        return Some("Parent was not retained".to_string());
    }
    expected_map_mismatch(
        &desired_instance.properties,
        &observed_instance.properties,
        true,
        &desired_instance.class_name,
    )
    .or_else(|| {
        expected_map_mismatch(
            &desired_instance.attributes,
            &observed_instance.attributes,
            false,
            &desired_instance.class_name,
        )
    })
}

fn changed_instance_mismatch(
    before: &SettingsBytecode,
    before_index: usize,
    desired: &SettingsBytecode,
    desired_index: usize,
    observed: &SettingsBytecode,
    observed_index: usize,
) -> Option<String> {
    let before_instance = &before.instances[before_index];
    let desired_instance = &desired.instances[desired_index];
    let observed_instance = &observed.instances[observed_index];
    for (label, previous, expected, actual) in [
        (
            "Name",
            before_instance.name.as_str(),
            desired_instance.name.as_str(),
            observed_instance.name.as_str(),
        ),
        (
            "ClassName",
            before_instance.class_name.as_str(),
            desired_instance.class_name.as_str(),
            observed_instance.class_name.as_str(),
        ),
    ] {
        if previous != expected && actual != expected {
            return Some(format!("{label} was not retained"));
        }
    }
    let previous_parent = settings_parent_id(before, before_index);
    let expected_parent = settings_parent_id(desired, desired_index);
    if previous_parent != expected_parent
        && settings_parent_id(observed, observed_index) != expected_parent
    {
        return Some("Parent was not retained".to_string());
    }
    changed_map_mismatch(
        &before_instance.properties,
        &desired_instance.properties,
        &observed_instance.properties,
        true,
        &desired_instance.class_name,
    )
    .or_else(|| {
        changed_map_mismatch(
            &before_instance.attributes,
            &desired_instance.attributes,
            &observed_instance.attributes,
            false,
            &desired_instance.class_name,
        )
    })
}

fn expected_map_mismatch(
    expected: &Map<String, Value>,
    actual: &Map<String, Value>,
    properties: bool,
    class_name: &str,
) -> Option<String> {
    expected.iter().find_map(|(name, expected)| {
        if properties
            && (name == "ScriptGuid"
                || (name == "Source" && is_lua_source_class(class_name))
                || reconciliation_property_is_derived(name))
        {
            return None;
        }
        let actual = if properties {
            reconciliation_property_value(actual, name)
        } else {
            actual.get(name)
        };
        (!reconciliation_property_values_equal(class_name, name, Some(expected), actual))
            .then(|| format!("{name} was not retained"))
    })
}

fn changed_map_mismatch(
    before: &Map<String, Value>,
    desired: &Map<String, Value>,
    observed: &Map<String, Value>,
    properties: bool,
    class_name: &str,
) -> Option<String> {
    let mut names = before.keys().chain(desired.keys()).collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names.into_iter().find_map(|name| {
        if properties
            && (name == "ScriptGuid"
                || (name == "Source" && is_lua_source_class(class_name))
                || reconciliation_property_is_derived(name))
        {
            return None;
        }
        let previous = if properties {
            reconciliation_property_value(before, name)
        } else {
            before.get(name)
        };
        let expected = if properties {
            reconciliation_property_value(desired, name)
        } else {
            desired.get(name)
        };
        if reconciliation_property_values_equal(class_name, name, previous, expected) {
            return None;
        }
        let actual = if properties {
            reconciliation_property_value(observed, name)
        } else {
            observed.get(name)
        };
        (!reconciliation_property_values_equal(class_name, name, expected, actual))
            .then(|| format!("{name} was not retained"))
    })
}

fn snapshot_entry_equivalent(
    path: &Path,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
    prepared: Option<&mut Option<PreparedEditorSettingsChange>>,
) -> Result<bool> {
    if entries_equivalent(path, left, right) {
        return Ok(true);
    }
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(false);
    };
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_service_settings_file_name)
    {
        let (left, right) = rayon::join(
            || settings_document(Some(left)),
            || settings_document(Some(right)),
        );
        let left = left?;
        let mut right = right?;
        let phase = Instant::now();
        let positional = settings_documents_positionally_equivalent(&left, &right);
        log_global(
            4,
            format_args!(
                "[renium] reconcile positional {}: {:.1}ms matched={positional}",
                path.display(),
                elapsed_ms(phase)
            ),
        );
        let phase = Instant::now();
        let equivalent = if positional {
            Some(true)
        } else if align_settings_ids_to_reference(&left, &mut right) {
            log_global(
                4,
                format_args!(
                    "[renium] reconcile identity {}: {:.1}ms",
                    path.display(),
                    elapsed_ms(phase)
                ),
            );
            let phase = Instant::now();
            let matched = settings_documents_equivalent(&left, &right);
            log_global(
                4,
                format_args!(
                    "[renium] reconcile values {}: {:.1}ms matched={matched}",
                    path.display(),
                    elapsed_ms(phase)
                ),
            );
            Some(matched)
        } else {
            None
        };
        if equivalent == Some(false)
            && let Some(prepared) = prepared
        {
            // Comparison already resolved duplicate identities. Reuse that exact
            // mapping and the decoded documents when planning the push.
            align_equivalent_values(&left, &mut right);
            *prepared = Some(PreparedEditorSettingsChange {
                previous: right,
                current: left,
            });
            return Ok(false);
        }
        let phase = Instant::now();
        drop_settings_documents(left, right);
        log_global(
            4,
            format_args!(
                "[renium] reconcile release {}: {:.1}ms",
                path.display(),
                elapsed_ms(phase)
            ),
        );
        if let Some(equivalent) = equivalent {
            return Ok(equivalent);
        }
        {
            bail!(
                "Failed to align duplicate instance identities in {}",
                path.display()
            );
        }
    }
    Ok(false)
}

fn snapshot_mismatch_details(
    observed: &ProjectSnapshot,
    expected: &ProjectSnapshot,
    paths: &[PathBuf],
) -> Result<Option<String>> {
    let Some(path) = paths.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
    }) else {
        return Ok(None);
    };
    let observed_document = settings_document(observed.entries.get(path))?;
    let mut expected_document = settings_document(expected.entries.get(path))?;
    if observed_document.instances.len() != expected_document.instances.len() {
        return Ok(Some(format!(
            "Studio returned {} instances; expected {}",
            observed_document.instances.len(),
            expected_document.instances.len()
        )));
    }
    if !align_settings_ids_to_reference(&observed_document, &mut expected_document) {
        return Ok(Some(
            "Studio returned instance identities that could not be aligned".to_string(),
        ));
    }
    let expected_by_id = expected_document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    for (observed_index, observed) in observed_document.instances.iter().enumerate() {
        let Some(expected_index) = expected_by_id.get(observed.settings_id.as_str()).copied()
        else {
            return Ok(Some(format!(
                "Studio returned an unexpected instance identity at {}",
                observed.name
            )));
        };
        let expected = &expected_document.instances[expected_index];
        if observed.name != expected.name
            || observed.class_name != expected.class_name
            || settings_parent_id(&observed_document, observed_index)
                != settings_parent_id(&expected_document, expected_index)
        {
            return Ok(Some(format!(
                "Studio returned a different structure at {}",
                expected.name
            )));
        }
        if is_reconciliation_protected_workspace_camera(&expected_document, expected_index) {
            continue;
        }
        let instance_name = &expected.name;
        for (kind, observed, expected) in [
            ("property", &observed.properties, &expected.properties),
            ("attribute", &observed.attributes, &expected.attributes),
        ] {
            let mut names = observed.keys().chain(expected.keys()).collect::<Vec<_>>();
            names.sort();
            names.dedup();
            for name in names {
                if kind == "property"
                    && (name == "ScriptGuid" || reconciliation_property_is_derived(name))
                {
                    continue;
                }
                let observed_value = if kind == "property" {
                    reconciliation_property_value(observed, name)
                } else {
                    observed.get(name)
                };
                let expected_value = if kind == "property" {
                    reconciliation_property_value(expected, name)
                } else {
                    expected.get(name)
                };
                let retained = match (observed_value, expected_value) {
                    (Some(observed), Some(expected)) if kind == "property" => {
                        reconciliation_property_values_equal(
                            &expected_document.instances[expected_index].class_name,
                            name,
                            Some(observed),
                            Some(expected),
                        )
                    }
                    (Some(observed), Some(expected)) => {
                        reconciliation_values_equal(observed, expected, false)
                    }
                    (None, None) => true,
                    _ => false,
                };
                if !retained {
                    let detail = match (observed_value, expected_value) {
                        (None, Some(_)) => "is missing from Studio".to_string(),
                        (Some(_), None) => "was added by Studio".to_string(),
                        (Some(observed), Some(expected)) => format!(
                            "is {}; expected {}",
                            reconcile_value_label(observed),
                            reconcile_value_label(expected)
                        ),
                        (None, None) => continue,
                    };
                    return Ok(Some(format!("{instance_name}.{name} {kind} {detail}")));
                }
            }
        }
    }
    Ok(None)
}

fn reconcile_value_label(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => format!("{value:?}"),
        Value::Array(value) => format!("an array of {} values", value.len()),
        Value::Object(value) if value.get("_type").and_then(Value::as_str) == Some("Ref") => {
            let id = value
                .get("settingsId")
                .or_else(|| value.get("instanceId"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            let path = value
                .get("pathSegments")
                .and_then(Value::as_array)
                .map(|segments| {
                    segments
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(".")
                })
                .unwrap_or_default();
            format!("Ref({id}, {path})")
        }
        Value::Object(value) => value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("_type").and_then(Value::as_str))
            .map_or_else(|| "an object".to_string(), |value| value.to_string()),
    }
}

fn snapshot_differences(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
) -> Result<HashSet<PathBuf>> {
    snapshot_differences_prepared(left, right, None)
}

fn snapshot_differences_prepared(
    left: &ProjectSnapshot,
    right: &ProjectSnapshot,
    mut prepared: Option<&mut HashMap<PathBuf, PreparedEditorSettingsChange>>,
) -> Result<HashSet<PathBuf>> {
    let mut paths = left
        .entries
        .keys()
        .chain(right.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    let prepare = prepared.is_some();
    let compared = paths
        .into_par_iter()
        .map(|path| {
            let mut settings = None;
            let equivalent = snapshot_entry_equivalent(
                &path,
                left.entries.get(&path),
                right.entries.get(&path),
                prepare.then_some(&mut settings),
            )?;
            Ok((!equivalent).then_some((path, settings)))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut differences = HashSet::new();
    for (path, settings) in compared.into_iter().flatten() {
        if let Some(prepared) = prepared.as_mut()
            && let Some(settings) = settings
        {
            prepared.insert(path.clone(), settings);
        }
        differences.insert(path);
    }
    Ok(differences)
}

fn apply_snapshot_paths(
    root: &Path,
    paths: &HashSet<PathBuf>,
    snapshot: &ProjectSnapshot,
) -> Result<()> {
    let mut paths = paths.iter().collect::<Vec<_>>();
    paths.sort();
    for relative in paths {
        if derived_project_path(relative) {
            continue;
        }
        let path = root.join(relative);
        match snapshot.entries.get(relative) {
            None => remove_path(&path)?,
            Some(SnapshotEntry::Directory) => fs::create_dir_all(&path)
                .with_context(|| format!("Failed to create {}", path.display()))?,
            Some(SnapshotEntry::File(bytes)) => {
                remove_path(&path)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, bytes)
                    .with_context(|| format!("Failed to write {}", path.display()))?;
            }
            Some(SnapshotEntry::Symlink { target, directory }) => {
                remove_path(&path)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                create_symlink(target, &path, *directory)?;
            }
        }
    }
    Ok(())
}

fn derived_project_path(path: &Path) -> bool {
    path == Path::new("sourcemap.json") || path.starts_with(".renium")
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("Failed to remove {}", path.display()))
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path, _directory: bool) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("Failed to create symbolic link {}", link.display()))
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path, directory: bool) -> Result<()> {
    if directory {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
    .with_context(|| format!("Failed to create symbolic link {}", link.display()))
}

fn canonical_string(path: &Path) -> Result<String> {
    Ok(canonical_path(path)?.to_string_lossy().into_owned())
}

fn record_path(context: &BoundContext, key: &str) -> PathBuf {
    Path::new(&context.root)
        .join(".renium")
        .join(RECORD_DIR)
        .join(format!("{key}.rmp"))
}

fn legacy_record_path(context: &BoundContext, key: &str) -> PathBuf {
    Path::new(&context.root)
        .join(".renium")
        .join(RECORD_DIR)
        .join(format!("{key}.rmp.zst"))
}

fn load_record(context: &BoundContext, key: &str) -> Result<Option<PairRecord>> {
    let path = record_path(context, key);
    match fs::read(&path) {
        Ok(bytes) => {
            return Ok(Some(rmp_serde::from_slice(&bytes).with_context(|| {
                format!("Failed to decode {}", path.display())
            })?));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    }

    let legacy_path = legacy_record_path(context, key);
    let Some(legacy) = read_compressed::<LegacyPairRecord>(&legacy_path)? else {
        return Ok(None);
    };
    if legacy.version != 1 {
        return Ok(None);
    }
    let baseline = legacy
        .baseline
        .as_ref()
        .map(|baseline| StoredSnapshot::write(Path::new(&context.root), key, baseline))
        .transpose()?;
    let record = PairRecord {
        version: RECORD_VERSION,
        identity: legacy.identity,
        mode: legacy.mode,
        conflict_preference: legacy.conflict_preference,
        runtime_settings: legacy.runtime_settings,
        baseline,
        head: None,
        conflicts: legacy.conflicts,
        resolution_required: legacy.resolution_required,
        last_runtime_id: None,
        local_file_stamp: None,
        local_file_digest: None,
        studio_checkpoint: None,
    };
    write_record(context, key, &record)?;
    fs::remove_file(&legacy_path)
        .with_context(|| format!("Failed to remove {}", legacy_path.display()))?;
    Ok(Some(record))
}

fn write_record(context: &BoundContext, key: &str, record: &PairRecord) -> Result<()> {
    let path = record_path(context, key);
    let encoded = rmp_serde::to_vec(record).context("Failed to encode reconciliation state")?;
    atomic_write_file(&path, &encoded)?;
    if let Some(baseline) = &record.baseline {
        let _ = baseline.prune(Path::new(&context.root), key);
    } else {
        let _ = StoredSnapshot::clear(Path::new(&context.root), key);
    }
    Ok(())
}

fn read_compressed<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    let decoded = zstd::stream::decode_all(bytes.as_slice())
        .with_context(|| format!("Failed to decompress {}", path.display()))?;
    Ok(Some(rmp_serde::from_slice(&decoded).with_context(
        || format!("Failed to decode {}", path.display()),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::types::EditorSettingsWrite;
    use crate::settings::bytecode::SettingsBytecodeInstance;

    #[test]
    fn staged_settings_redirect_uses_original_project_not_merged_stage() {
        let root = crate::tests::support::temp_dir("staged-settings-hash");
        let stage = root.join("stage");
        let relative = PathBuf::from("src/ServerScriptService/__roblox_sync_settings.renium");
        let destination = root.join(&relative);
        let staged_path = stage.join(&relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::create_dir_all(staged_path.parent().unwrap()).unwrap();
        let document = |id: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance {
                settings_id: id.into(),
                name: "ServerScriptService".into(),
                class_name: "ServerScriptService".into(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            }],
        };
        let original = encode_settings_bytecode(&document("editor-id")).unwrap();
        let merged = encode_settings_bytecode(&document("aligned-id")).unwrap();
        fs::write(&destination, &original).unwrap();
        fs::write(&staged_path, &merged).unwrap();
        let mut changes = EditorChangeSet {
            settings_writes: vec![EditorSettingsWrite {
                path: staged_path.clone(),
                expected_hash: settings_file_hash(&staged_path).unwrap(),
                document: document("aligned-id"),
            }],
            ..Default::default()
        };
        let project = file_snapshot(&[(relative.to_str().unwrap(), &original)]);
        let generated =
            redirect_staged_settings_writes(&mut changes, &stage, &root, Some(&project)).unwrap();
        let write = &changes.settings_writes[0];
        assert_eq!(write.path, destination);
        assert_eq!(
            write.expected_hash,
            settings_file_hash(&destination).unwrap()
        );
        assert!(generated.entries.get(&relative) == Some(&SnapshotEntry::File(merged)));
        assert_eq!(fs::read(&destination).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_settings_redirect_keeps_concurrent_write_protection() {
        let root = crate::tests::support::temp_dir("staged-settings-concurrency");
        let stage = root.join("stage");
        let relative = Path::new("settings.renium");
        let destination = root.join(relative);
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        let bytes = encode_settings_bytecode(&document).unwrap();
        for existed in [false, true] {
            for with_snapshot in [false, true] {
                let project = if existed {
                    file_snapshot(&[("settings.renium", &bytes)])
                } else {
                    ProjectSnapshot::default()
                };
                let expected = with_snapshot.then_some(&project);
                for concurrent_edit in [false, true] {
                    if existed || concurrent_edit {
                        fs::write(
                            &destination,
                            if concurrent_edit {
                                b"newer editor data".as_slice()
                            } else {
                                bytes.as_slice()
                            },
                        )
                        .unwrap();
                    } else if destination.exists() {
                        fs::remove_file(&destination).unwrap();
                    }
                    let hash = existed.then(|| Sha256::digest(&bytes).into());
                    let mut changes = EditorChangeSet {
                        settings_writes: vec![EditorSettingsWrite {
                            path: stage.join(relative),
                            expected_hash: hash,
                            document: document.clone(),
                        }],
                        ..Default::default()
                    };
                    let result =
                        redirect_staged_settings_writes(&mut changes, &stage, &root, expected);
                    if concurrent_edit {
                        let message = result.err().unwrap().to_string();
                        assert!(
                            message.contains("changed while its Studio update was being prepared")
                        );
                        assert_eq!(fs::read(&destination).unwrap(), b"newer editor data");
                    } else {
                        assert!(result.is_ok());
                        assert_eq!(changes.settings_writes[0].expected_hash, hash);
                        assert_eq!(changes.settings_writes[0].path, destination);
                    }
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_binding_uses_saved_pairing_without_enabling_live_sync() {
        let root = crate::tests::support::temp_dir("saved-studio-pairing");
        let records = root.join(".renium").join(RECORD_DIR);
        fs::create_dir_all(&records).unwrap();
        let mut record = PairRecord {
            version: RECORD_VERSION,
            identity: PairIdentity {
                experience: canonical_string(&root).unwrap(),
                project: canonical_string(&root).unwrap(),
                fingerprint: "prior-configuration".into(),
                game_id: Some(10),
                place_id: Some(20),
                local_file: None,
            },
            mode: PairMode::Reconcile,
            conflict_preference: ConflictPreference::None,
            runtime_settings: Map::new(),
            baseline: None,
            head: None,
            conflicts: Vec::new(),
            resolution_required: false,
            last_runtime_id: None,
            local_file_stamp: None,
            local_file_digest: None,
            studio_checkpoint: None,
        };
        let write = |name: &str, record: &PairRecord| {
            fs::write(records.join(name), rmp_serde::to_vec(record).unwrap()).unwrap();
        };
        assert_eq!(saved_studio_target_for_root(&root, &root).unwrap(), None);
        write("first.rmp", &record);
        write("duplicate.rmp", &record);
        assert_eq!(
            saved_studio_target_for_root(&root, &root).unwrap(),
            Some(super::super::StudioReopenTarget {
                file: None,
                game_id: Some(10),
                place_id: Some(20),
            })
        );
        assert!(!root.join(".renium/live-watch-state.enabled").exists());
        record.identity.project = "a different project".into();
        write("foreign.rmp", &record);
        assert!(
            saved_studio_target_for_root(&root, &root)
                .unwrap()
                .is_some()
        );
        record.identity.project = canonical_string(&root).unwrap();
        record.identity.place_id = Some(30);
        write("different-place.rmp", &record);
        assert_eq!(saved_studio_target_for_root(&root, &root).unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    fn file_snapshot(entries: &[(&str, &[u8])]) -> ProjectSnapshot {
        ProjectSnapshot {
            entries: entries
                .iter()
                .map(|(path, bytes)| (PathBuf::from(path), SnapshotEntry::File(bytes.to_vec())))
                .collect(),
        }
    }

    #[test]
    fn comparison_capture_retries_one_transient_studio_change() {
        let mut attempts = 0;
        let value = retry_transient_studio_capture(|| {
            attempts += 1;
            if attempts == 1 {
                anyhow::bail!(
                    "Studio changed Workspace while native import was staged; retry the sync"
                );
            }
            Ok(42)
        })
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts, 2);

        let mut persistent_attempts = 0;
        let error = retry_transient_studio_capture::<()>(|| {
            persistent_attempts += 1;
            anyhow::bail!("Studio changed Workspace while native import was staged; retry the sync")
        })
        .unwrap_err();
        assert!(error.to_string().contains("retry the sync"));
        assert_eq!(persistent_attempts, 2);

        let mut ordinary_attempts = 0;
        let error = retry_transient_studio_capture::<()>(|| {
            ordinary_attempts += 1;
            anyhow::bail!("invalid snapshot")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "invalid snapshot");
        assert_eq!(ordinary_attempts, 1);
    }

    #[test]
    fn studio_checkpoint_requires_uninterrupted_clean_tracking() {
        let context = BoundContext {
            id: 1,
            initialized: true,
            project: String::new(),
            root: String::new(),
            experience: String::new(),
            source: String::new(),
            resource_lease: None,
            place_id: None,
            game_id: None,
            selector: String::new(),
            runtime_id: Some("runtime".to_string()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let services = sync_services();
        let generations = services
            .iter()
            .map(|service| (service.clone(), Value::from(7)))
            .collect::<Map<_, _>>();
        let state = json!({
            "tracking": true,
            "trackedServices": services.len(),
            "dirtyServices": [],
            "fullSyncServices": [],
            "runtimeId": "runtime",
            "changeTrackerVersion": 4,
            "seq": 9,
            "serviceGenerations": generations.clone(),
            "checkpointGenerations": generations,
        });
        let checkpoint = StudioCheckpoint::from_state(&context, &state).unwrap();
        assert!(checkpoint.matches_state(&context, &state));
        assert_eq!(
            checkpoint.changed_services(&context, &state),
            Some(Vec::new())
        );

        let mut transaction_generation = state.clone();
        transaction_generation["serviceGenerations"][&services[0]] = Value::from(8);
        assert!(checkpoint.matches_state(&context, &transaction_generation));

        let mut interrupted = state.clone();
        interrupted["checkpointGenerations"][&services[0]] = Value::from(8);
        assert!(!checkpoint.matches_state(&context, &interrupted));
        assert_eq!(
            checkpoint.changed_services(&context, &interrupted),
            Some(vec![services[0].clone()])
        );

        let mut pending = state.clone();
        pending["dirtyServices"] = json!([&services[0]]);
        assert!(!checkpoint.matches_state(&context, &pending));
        assert_eq!(
            checkpoint.changed_services(&context, &pending),
            Some(vec![services[0].clone()])
        );

        let mut moved_references = state;
        moved_references["referencePathsMayChange"] = Value::Bool(true);
        assert_eq!(
            checkpoint.changed_services(&context, &moved_references),
            None
        );

        let mut unexpected_service = moved_references;
        unexpected_service["referencePathsMayChange"] = Value::Bool(false);
        unexpected_service["checkpointGenerations"]["UnexpectedService"] = Value::from(7);
        assert!(StudioCheckpoint::from_state(&context, &unexpected_service).is_none());
        assert_eq!(
            checkpoint.changed_services(&context, &unexpected_service),
            None
        );
    }

    #[test]
    fn source_only_push_skips_service_readback_only_after_exact_verification() {
        let paths = HashSet::from([PathBuf::from(
            "src/ServerScriptService/Verified.server.luau",
        )]);
        let verified = Map::from_iter([
            ("sourceVerified".to_string(), Value::from(1)),
            ("sourceVerifyFailed".to_string(), Value::from(0)),
        ]);
        assert!(exact_source_push_verified(&verified, &paths));

        let unverified = Map::from_iter([
            ("sourceVerified".to_string(), Value::from(0)),
            ("sourceVerifyFailed".to_string(), Value::from(0)),
        ]);
        assert!(!exact_source_push_verified(&unverified, &paths));
    }

    #[test]
    fn targeted_push_verification_ignores_unrelated_studio_edits() {
        let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let snapshot = |anchored: bool, concurrent: Option<&str>| {
            let mut part = SettingsBytecodeInstance {
                settings_id: "part".to_string(),
                name: "Part".to_string(),
                class_name: "Part".to_string(),
                parent_index: Some(0),
                properties: Map::from_iter([("Anchored".to_string(), Value::Bool(anchored))]),
                attributes: Map::new(),
            };
            if let Some(value) = concurrent {
                part.attributes.insert(
                    "ConcurrentProbe".to_string(),
                    Value::String(value.to_string()),
                );
            }
            let document = SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    SettingsBytecodeInstance {
                        settings_id: "root".to_string(),
                        name: "ReplicatedStorage".to_string(),
                        class_name: "ReplicatedStorage".to_string(),
                        parent_index: None,
                        properties: Map::new(),
                        attributes: Map::new(),
                    },
                    part,
                ],
            };
            ProjectSnapshot {
                entries: [(
                    path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
                )]
                .into_iter()
                .collect(),
            }
        };
        let before = snapshot(true, None);
        let desired = snapshot(false, None);
        let observed = snapshot(false, Some("kept"));
        let paths = HashSet::from([path.clone()]);
        let (mismatches, _) =
            snapshot_intended_delta_mismatches(&before, &desired, &observed, &paths).unwrap();
        assert!(mismatches.is_empty());

        let overwritten = snapshot(true, Some("kept"));
        let (mismatches, detail) =
            snapshot_intended_delta_mismatches(&before, &desired, &overwritten, &paths).unwrap();
        assert_eq!(mismatches, vec![path]);
        assert!(detail.is_some_and(|detail| detail.contains("Anchored")));
    }

    #[test]
    fn targeted_push_verification_ignores_package_modified_state() {
        let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let instance = |id: &str, name: &str, class_name: &str, parent_index: Option<usize>| {
            SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: name.to_string(),
                class_name: class_name.to_string(),
                parent_index,
                properties: Map::new(),
                attributes: Map::new(),
            }
        };
        let snapshot = |modified_state: i64, archivable: Option<bool>| {
            let event_properties = archivable
                .map(|archivable| {
                    Map::from_iter([("Archivable".to_string(), Value::Bool(archivable))])
                })
                .unwrap_or_default();
            let document = SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
                    instance("package", "testPackage", "Folder", Some(0)),
                    SettingsBytecodeInstance {
                        settings_id: "link".to_string(),
                        name: "PackageLink".to_string(),
                        class_name: "PackageLink".to_string(),
                        parent_index: Some(1),
                        properties: Map::from_iter([(
                            "ModifiedState".to_string(),
                            Value::from(modified_state),
                        )]),
                        attributes: Map::new(),
                    },
                    SettingsBytecodeInstance {
                        settings_id: "event".to_string(),
                        name: "RemoteEvent".to_string(),
                        class_name: "RemoteEvent".to_string(),
                        parent_index: Some(1),
                        properties: event_properties,
                        attributes: Map::new(),
                    },
                ],
            };
            ProjectSnapshot {
                entries: [(
                    path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
                )]
                .into_iter()
                .collect(),
            }
        };
        let before = snapshot(1, Some(false));
        let desired = snapshot(-1, Some(true));
        let observed = snapshot(1, None);
        let paths = HashSet::from([path.clone()]);
        let (mismatches, _) =
            snapshot_intended_delta_mismatches(&before, &desired, &observed, &paths).unwrap();
        assert!(mismatches.is_empty());

        let observed = snapshot(1, Some(false));
        let (mismatches, detail) =
            snapshot_intended_delta_mismatches(&before, &desired, &observed, &paths).unwrap();
        assert_eq!(mismatches, vec![path]);
        assert!(detail.is_some_and(|detail| detail.contains("Archivable")));
    }

    #[test]
    fn targeted_push_verification_checks_script_files_not_settings_source_metadata() {
        let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let source_path = PathBuf::from("src/ReplicatedStorage/ProbeModule.luau");
        let snapshot = |settings_source: &str, file_source: &str| {
            let document = SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    SettingsBytecodeInstance {
                        settings_id: "root".to_string(),
                        name: "ReplicatedStorage".to_string(),
                        class_name: "ReplicatedStorage".to_string(),
                        parent_index: None,
                        properties: Map::new(),
                        attributes: Map::new(),
                    },
                    SettingsBytecodeInstance {
                        settings_id: "script".to_string(),
                        name: "ProbeModule".to_string(),
                        class_name: "ModuleScript".to_string(),
                        parent_index: Some(0),
                        properties: Map::from_iter([(
                            "Source".to_string(),
                            Value::String(settings_source.to_string()),
                        )]),
                        attributes: Map::new(),
                    },
                ],
            };
            ProjectSnapshot {
                entries: [
                    (
                        settings_path.clone(),
                        SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
                    ),
                    (
                        source_path.clone(),
                        SnapshotEntry::File(file_source.as_bytes().to_vec()),
                    ),
                ]
                .into_iter()
                .collect(),
            }
        };
        let before = ProjectSnapshot::default();
        let desired = snapshot("return 'desired'", "return 'desired'\n");
        let observed = snapshot("external", "return 'desired'\n");
        let paths = HashSet::from([settings_path.clone(), source_path.clone()]);
        let (mismatches, _) =
            snapshot_intended_delta_mismatches(&before, &desired, &observed, &paths).unwrap();
        assert!(mismatches.is_empty());

        let observed = snapshot("return 'desired'", "return 'wrong'\n");
        let (mismatches, _) =
            snapshot_intended_delta_mismatches(&before, &desired, &observed, &paths).unwrap();
        assert_eq!(mismatches, vec![source_path]);
    }

    #[test]
    fn reconciliation_treats_elided_class_defaults_as_equal() {
        let omitted = Map::new();
        let default = Map::from_iter([("CanCollide".to_string(), Value::Bool(true))]);
        let changed = Map::from_iter([("CanCollide".to_string(), Value::Bool(false))]);
        assert!(reconciliation_maps_equal("Part", &default, &omitted));
        assert!(reconciliation_maps_equal("Part", &omitted, &default));
        assert!(!reconciliation_maps_equal("Part", &changed, &omitted));
    }

    #[test]
    fn unchanged_local_file_bootstraps_a_replacement_runtime() {
        let digest = "same-digest";

        assert!(should_bootstrap_studio_from_editor(
            PairMode::Reconcile,
            Some("old-runtime"),
            Some("new-runtime"),
            Some(digest),
            Some(digest),
        ));
        assert!(!should_bootstrap_studio_from_editor(
            PairMode::Verify,
            Some("old-runtime"),
            Some("new-runtime"),
            Some(digest),
            Some(digest),
        ));
    }

    #[test]
    fn changed_or_unknown_local_file_uses_normal_reconciliation() {
        assert!(!should_bootstrap_studio_from_editor(
            PairMode::Reconcile,
            Some("old-runtime"),
            Some("new-runtime"),
            Some("previous-digest"),
            Some("changed-digest"),
        ));
        assert!(!should_bootstrap_studio_from_editor(
            PairMode::Reconcile,
            None,
            Some("new-runtime"),
            Some("digest"),
            Some("digest"),
        ));
        assert!(!should_bootstrap_studio_from_editor(
            PairMode::Reconcile,
            Some("old-runtime"),
            Some("new-runtime"),
            None,
            Some("digest"),
        ));
    }

    #[test]
    fn matching_legacy_stamp_migrates_once_to_content_identity() {
        let stamp = LocalFileStamp {
            length: 42,
            modified_seconds: 123,
            modified_nanos: 456,
        };

        assert!(legacy_local_file_stamp_matches(
            PairMode::Reconcile,
            true,
            None,
            Some(&stamp),
            Some(&stamp),
        ));
        assert!(!legacy_local_file_stamp_matches(
            PairMode::Verify,
            true,
            None,
            Some(&stamp),
            Some(&stamp),
        ));
        assert!(!legacy_local_file_stamp_matches(
            PairMode::Reconcile,
            true,
            Some("already-migrated"),
            Some(&stamp),
            Some(&stamp),
        ));
    }

    #[test]
    fn runtime_bootstrap_requires_tracking_without_fresh_edits() {
        assert!(studio_runtime_bootstrap_safe(&json!({
            "tracking": true,
            "dirtyServices": ["ReplicatedStorage"],
            "restoredPendingServices": ["ReplicatedStorage"],
        })));
        assert!(!studio_runtime_bootstrap_safe(&json!({
            "tracking": false,
            "dirtyServices": [],
            "restoredPendingServices": [],
        })));
        assert!(!studio_runtime_bootstrap_safe(&json!({
            "tracking": true,
            "dirtyServices": ["ReplicatedStorage", "StarterGui"],
            "restoredPendingServices": ["ReplicatedStorage"],
        })));
    }

    #[test]
    fn source_equivalence_normalizes_crlf_and_lone_cr_without_allocating() {
        let path = Path::new("script.luau");
        let left = SnapshotEntry::File(b"first\r\nsecond\rthird\n".to_vec());
        let right = SnapshotEntry::File(b"first\nsecond\nthird\n".to_vec());
        assert!(entries_equivalent(path, Some(&left), Some(&right)));
    }

    #[test]
    fn staged_source_root_accepts_cli_relative_and_bound_absolute_paths() {
        let root = if cfg!(windows) {
            Path::new("C:/project")
        } else {
            Path::new("/project")
        };

        assert_eq!(
            project_relative_source_root(root, Path::new("src")).unwrap(),
            Path::new("src")
        );
        assert_eq!(
            project_relative_source_root(root, &root.join("src")).unwrap(),
            Path::new("src")
        );
    }

    #[test]
    fn target_ownership_ends_with_the_live_pair() {
        let coordinator = Coordinator::default();
        let first = PairIdentity {
            experience: "experience".to_string(),
            project: "first".to_string(),
            fingerprint: "first-fingerprint".to_string(),
            game_id: Some(1),
            place_id: Some(2),
            local_file: None,
        };
        let second = PairIdentity {
            project: "second".to_string(),
            fingerprint: "second-fingerprint".to_string(),
            ..first.clone()
        };

        assert_eq!(coordinator.claim_target(&first), None);
        assert_eq!(coordinator.claim_target(&second), Some("first".to_string()));

        coordinator.release_target(&first.pair_key());

        assert_eq!(coordinator.claim_target(&second), None);
    }

    #[test]
    fn first_pairing_unions_separate_files_and_blocks_same_file_conflicts() {
        let editor = file_snapshot(&[("src/A.luau", b"return 'editor'")]);
        let studio = file_snapshot(&[("src/B.luau", b"return 'studio'")]);
        let (merged, conflicts) =
            merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.entries.len(), 2);

        let studio = file_snapshot(&[("src/A.luau", b"return 'studio'")]);
        let (_, conflicts) =
            merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
        assert_eq!(conflicts.len(), 1);

        let editor = file_snapshot(&[("src/A.luau", b"local a = 1\r\nreturn a\r\n")]);
        let studio = file_snapshot(&[("src/A.luau", b"local a = 1\nreturn a\n")]);
        let (merged, conflicts) =
            merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        assert!(merged == editor);
        assert!(snapshots_equivalent(&editor, &studio).unwrap());

        let service_root = |id: &str| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: "ReplicatedStorage".to_string(),
            class_name: "ReplicatedStorage".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let child = |id: &str, name: &str| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: "Folder".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let settings_snapshot = |document: SettingsBytecode| ProjectSnapshot {
            entries: [(
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let editor = settings_snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                service_root("editor-root"),
                child("editor-child", "FromEditor"),
            ],
        });
        let studio = settings_snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                service_root("studio-root"),
                child("studio-child", "FromStudio"),
            ],
        });
        let (merged, conflicts) =
            merge_snapshots(None, &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        let merged = settings_document(merged.entries.get(Path::new(
            "src/ReplicatedStorage/__roblox_sync_settings.renium",
        )))
        .unwrap();
        assert_eq!(merged.instances.len(), 3);

        let duplicate = |id: &str, value: &str| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: "Duplicate".to_string(),
            class_name: "StringValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([("Value".to_string(), Value::String(value.to_string()))]),
            attributes: Map::new(),
        };
        let editor = settings_snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                service_root("editor-root"),
                duplicate("editor-a", "A"),
                duplicate("editor-b", "B"),
            ],
        });
        let studio = settings_snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                service_root("studio-root"),
                duplicate("studio-a", "B"),
                duplicate("studio-b", "A"),
            ],
        });
        let (_, conflicts) =
            merge_snapshots(None, &editor, &studio, ConflictPreference::Editor).unwrap();
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("ambiguous duplicate"))
        );

        let editor = settings_snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![service_root("editor-root"), child("editor-child", "Same")],
        });
        let mut studio_child = child("studio-child", "Same");
        studio_child.class_name = "Model".to_string();
        let studio = settings_snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![service_root("studio-root"), studio_child],
        });
        let (_, conflicts) =
            merge_snapshots(None, &editor, &studio, ConflictPreference::Editor).unwrap();
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("different classes"))
        );
    }

    #[test]
    fn mismatch_details_ignore_instance_serialization_order() {
        let instance = |id: &str,
                        name: &str,
                        class_name: &str,
                        parent_index: Option<usize>,
                        marker: Option<f64>,
                        value: Option<&str>| {
            let mut instance = SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: name.to_string(),
                class_name: class_name.to_string(),
                parent_index,
                properties: Map::new(),
                attributes: Map::new(),
            };
            if let Some(marker) = marker {
                instance
                    .attributes
                    .insert("Marker".to_string(), json!(marker));
            }
            if let Some(value) = value {
                instance
                    .properties
                    .insert("Value".to_string(), json!(value));
            }
            instance
        };
        let snapshot = |document: SettingsBytecode| ProjectSnapshot {
            entries: [(
                PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let expected = snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "ServerStorage", "ServerStorage", None, None, None),
                instance("burst", "Burst", "Folder", Some(0), None, None),
                instance("one", "Node", "Folder", Some(1), Some(1.0), None),
                instance("two", "Node", "Folder", Some(1), Some(2.0), None),
                instance(
                    "holder",
                    "Holder",
                    "StringValue",
                    Some(0),
                    None,
                    Some("editor"),
                ),
            ],
        });
        let observed = snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "ServerStorage", "ServerStorage", None, None, None),
                instance(
                    "holder",
                    "Holder",
                    "StringValue",
                    Some(0),
                    None,
                    Some("studio"),
                ),
                instance("burst", "Burst", "Folder", Some(0), None, None),
                instance("two", "Node", "Folder", Some(2), Some(2.0), None),
                instance("one", "Node", "Folder", Some(2), Some(1.0), None),
            ],
        });
        let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");

        let detail = snapshot_mismatch_details(&observed, &expected, &[path])
            .unwrap()
            .unwrap();
        assert!(detail.contains("Holder.Value property"), "{detail}");
        assert!(!detail.contains("different structure"), "{detail}");
    }

    #[test]
    fn three_way_merge_keeps_independent_changes() {
        let baseline = file_snapshot(&[
            ("src/A.luau", b"return 'base-a'"),
            ("src/B.luau", b"return 'base-b'"),
        ]);
        let editor = file_snapshot(&[
            ("src/A.luau", b"return 'editor'"),
            ("src/B.luau", b"return 'base-b'"),
        ]);
        let studio = file_snapshot(&[
            ("src/A.luau", b"return 'base-a'"),
            ("src/B.luau", b"return 'studio'"),
        ]);
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        assert!(
            merged.entries.get(Path::new("src/A.luau"))
                == editor.entries.get(Path::new("src/A.luau"))
        );
        assert!(
            merged.entries.get(Path::new("src/B.luau"))
                == studio.entries.get(Path::new("src/B.luau"))
        );

        let shared = file_snapshot(&[("src/A.luau", b"return 'shared'")]);
        let reverted = file_snapshot(&[("src/A.luau", b"return 'base-a'")]);
        let changed = file_snapshot(&[("src/A.luau", b"return 'studio-again'")]);
        let (_, stale_conflicts) = merge_snapshots(
            Some(&baseline),
            &reverted,
            &changed,
            ConflictPreference::None,
        )
        .unwrap();
        assert!(stale_conflicts.is_empty());
        let (_, current_conflicts) =
            merge_snapshots(Some(&shared), &reverted, &changed, ConflictPreference::None).unwrap();
        assert_eq!(current_conflicts.len(), 1);

        let deleted = ProjectSnapshot::default();
        let (merged, structural_conflicts) = merge_snapshots(
            Some(&shared),
            &deleted,
            &changed,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert!(structural_conflicts.is_empty());
        assert!(!merged.entries.contains_key(Path::new("src/A.luau")));
        let (merged, structural_conflicts) = merge_snapshots(
            Some(&shared),
            &deleted,
            &changed,
            ConflictPreference::Studio,
        )
        .unwrap();
        assert!(structural_conflicts.is_empty());
        assert!(
            merged.entries.get(Path::new("src/A.luau"))
                == changed.entries.get(Path::new("src/A.luau"))
        );

        let parent = PathBuf::from("src/Folder");
        let baseline = ProjectSnapshot {
            entries: [(parent.clone(), SnapshotEntry::Directory)]
                .into_iter()
                .collect(),
        };
        let studio = ProjectSnapshot {
            entries: [
                (parent, SnapshotEntry::Directory),
                (
                    PathBuf::from("src/Folder/New.luau"),
                    SnapshotEntry::File(b"return true".to_vec()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let (merged, structural_conflicts) = merge_snapshots(
            Some(&baseline),
            &ProjectSnapshot::default(),
            &studio,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert!(structural_conflicts.is_empty());
        assert!(
            merged.entries.get(Path::new("src/Folder/New.luau"))
                == studio.entries.get(Path::new("src/Folder/New.luau"))
        );

        let instance =
            |id: &str, name: &str, parent_index: Option<usize>| SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: name.to_string(),
                class_name: "Folder".to_string(),
                parent_index,
                properties: Map::new(),
                attributes: Map::new(),
            };
        let settings_snapshot = |instances| ProjectSnapshot {
            entries: [(
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(
                    encode_settings_bytecode(&SettingsBytecode {
                        version: SETTINGS_BINARY_VERSION,
                        instances,
                    })
                    .unwrap(),
                ),
            )]
            .into_iter()
            .collect(),
        };
        let baseline = settings_snapshot(vec![
            instance("root", "ReplicatedStorage", None),
            instance("a", "A", Some(0)),
            instance("b", "B", Some(0)),
        ]);
        let editor = settings_snapshot(vec![
            instance("root", "ReplicatedStorage", None),
            instance("a", "A", Some(0)),
            instance("b", "B", Some(1)),
        ]);
        let studio = settings_snapshot(vec![
            instance("root", "ReplicatedStorage", None),
            instance("b", "B", Some(0)),
            instance("a", "A", Some(1)),
        ]);
        let (merged, cycle_conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
        assert!(!cycle_conflicts.is_empty());
        assert_eq!(
            settings_document(merged.entries.values().next())
                .unwrap()
                .instances
                .len(),
            3
        );
        for (preference, expected_a, expected_b) in [
            (ConflictPreference::Studio, Some("b"), Some("root")),
            (ConflictPreference::Editor, Some("root"), Some("a")),
        ] {
            let (merged, conflicts) =
                merge_snapshots(Some(&baseline), &editor, &studio, preference).unwrap();
            assert!(conflicts.is_empty());
            let document = settings_document(merged.entries.values().next()).unwrap();
            let parents = document
                .instances
                .iter()
                .map(|instance| {
                    (
                        instance.settings_id.as_str(),
                        instance
                            .parent_index
                            .map(|parent| document.instances[parent].settings_id.as_str()),
                    )
                })
                .collect::<HashMap<_, _>>();
            assert_eq!(parents["a"], expected_a);
            assert_eq!(parents["b"], expected_b);
        }

        let script_snapshot = |name: &str, source: &[u8]| ProjectSnapshot {
            entries: [
                (
                    PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                    SnapshotEntry::File(
                        encode_settings_bytecode(&SettingsBytecode {
                            version: SETTINGS_BINARY_VERSION,
                            instances: vec![
                                instance("root", "ReplicatedStorage", None),
                                SettingsBytecodeInstance {
                                    settings_id: "script".to_string(),
                                    name: name.to_string(),
                                    class_name: "ModuleScript".to_string(),
                                    parent_index: Some(0),
                                    properties: Map::new(),
                                    attributes: Map::new(),
                                },
                            ],
                        })
                        .unwrap(),
                    ),
                ),
                (
                    PathBuf::from(format!("src/ReplicatedStorage/{name}.luau")),
                    SnapshotEntry::File(source.to_vec()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let baseline = script_snapshot("Original", b"return 'base'");
        let editor = script_snapshot("EditorName", b"return 'editor'");
        let studio = script_snapshot("StudioName", b"return 'studio'");
        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &editor,
            &studio,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert!(conflicts.is_empty());
        assert!(
            merged
                .entries
                .contains_key(Path::new("src/ReplicatedStorage/EditorName.luau"))
        );
        assert!(
            !merged
                .entries
                .contains_key(Path::new("src/ReplicatedStorage/StudioName.luau"))
        );
        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &editor,
            &studio,
            ConflictPreference::Studio,
        )
        .unwrap();
        assert!(conflicts.is_empty());
        assert!(
            merged
                .entries
                .contains_key(Path::new("src/ReplicatedStorage/StudioName.luau"))
        );
        assert!(
            !merged
                .entries
                .contains_key(Path::new("src/ReplicatedStorage/EditorName.luau"))
        );
    }

    #[test]
    fn source_edit_conflicts_with_script_deletion_before_source_enters_baseline() {
        let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let source_path = PathBuf::from("src/ReplicatedStorage/Logic.luau");
        let settings = |include_script| {
            let mut instances = vec![SettingsBytecodeInstance {
                settings_id: "root".to_string(),
                name: "ReplicatedStorage".to_string(),
                class_name: "ReplicatedStorage".to_string(),
                parent_index: None,
                properties: Map::new(),
                attributes: Map::new(),
            }];
            if include_script {
                instances.push(SettingsBytecodeInstance {
                    settings_id: "script".to_string(),
                    name: "Logic".to_string(),
                    class_name: "ModuleScript".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                });
            }
            SnapshotEntry::File(
                encode_settings_bytecode(&SettingsBytecode {
                    version: SETTINGS_BINARY_VERSION,
                    instances,
                })
                .unwrap(),
            )
        };
        let baseline = ProjectSnapshot {
            entries: [(settings_path.clone(), settings(true))]
                .into_iter()
                .collect(),
        };
        let editor = ProjectSnapshot {
            entries: [
                (settings_path.clone(), settings(true)),
                (
                    source_path.clone(),
                    SnapshotEntry::File(b"return 'edited before baseline'".to_vec()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let studio = ProjectSnapshot {
            entries: [(settings_path.clone(), settings(false))]
                .into_iter()
                .collect(),
        };

        let (_, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("deleted here but modified on the other side"));

        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &editor,
            &studio,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert!(conflicts.is_empty());
        assert!(merged.entries.contains_key(&source_path));
        assert_eq!(
            settings_document(merged.entries.get(&settings_path))
                .unwrap()
                .instances
                .len(),
            2
        );

        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &editor,
            &studio,
            ConflictPreference::Studio,
        )
        .unwrap();
        assert!(conflicts.is_empty());
        assert!(!merged.entries.contains_key(&source_path));
        assert_eq!(
            settings_document(merged.entries.get(&settings_path))
                .unwrap()
                .instances
                .len(),
            1
        );

        let (_, conflicts) =
            merge_snapshots(Some(&baseline), &studio, &editor, ConflictPreference::None).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("modified here but deleted on the other side"));

        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &studio,
            &editor,
            ConflictPreference::Studio,
        )
        .unwrap();
        assert!(conflicts.is_empty());
        assert!(merged.entries.contains_key(&source_path));

        let (merged, conflicts) = merge_snapshots(
            Some(&baseline),
            &studio,
            &editor,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert!(conflicts.is_empty());
        assert!(!merged.entries.contains_key(&source_path));
    }

    #[test]
    fn three_way_settings_merge_keeps_concurrent_additions() {
        let instance =
            |id: &str, name: &str, parent_index: Option<usize>| SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: name.to_string(),
                class_name: "Folder".to_string(),
                parent_index,
                properties: Map::new(),
                attributes: Map::new(),
            };
        let snapshot = |document: SettingsBytecode| ProjectSnapshot {
            entries: [(
                PathBuf::from("src/StarterGui/__roblox_sync_settings.renium"),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let baseline = snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "StarterGui", None),
                instance("existing", "Existing", Some(0)),
            ],
        });
        let editor = snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "StarterGui", None),
                instance("existing", "Existing", Some(0)),
                instance("editor", "EditorIndependent", Some(0)),
            ],
        });
        let studio = snapshot(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                instance("root", "StarterGui", None),
                instance("existing", "Existing", Some(0)),
                instance("studio", "StudioIndependent", Some(0)),
                instance("studio-retry", "EditorIndependent", Some(0)),
            ],
        });

        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        let merged = settings_document(
            merged
                .entries
                .get(Path::new("src/StarterGui/__roblox_sync_settings.renium")),
        )
        .unwrap();
        let mut names = merged
            .instances
            .iter()
            .map(|instance| instance.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "EditorIndependent",
                "Existing",
                "StarterGui",
                "StudioIndependent",
            ]
        );
    }

    fn new_branch_fixture() -> (SettingsBytecode, SettingsBytecode, SettingsBytecode) {
        let instance = |id: &str, name: &str, class: &str, parent| {
            SettingsBytecodeInstance::new(
                id.to_string(),
                name.to_string(),
                class.to_string(),
                parent,
            )
        };
        let base = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![instance(
                "root",
                "ReplicatedStorage",
                "ReplicatedStorage",
                None,
            )],
        };
        let mut editor = base.clone();
        editor.instances.extend([
            instance("button", "ShiftLockButton", "TextButton", Some(0)),
            instance("corner", "UICorner", "UICorner", Some(1)),
            instance("ref", "Reference", "ObjectValue", Some(1)),
        ]);
        for name in [
            "TopLeftRadius",
            "TopRightRadius",
            "BottomLeftRadius",
            "BottomRightRadius",
        ] {
            editor.instances[2].properties.insert(
                name.to_string(),
                json!({"_type":"UDim","scale":1.0,"offset":0.0}),
            );
        }
        editor.instances[3].properties.insert(
            "Value".to_string(),
            json!({"_type":"Ref","settingsId":"corner"}),
        );
        let mut studio = editor.clone();
        // New instance IDs can collide across observations, including cycles and
        // an unrelated addition occupying the ID wanted by a matching node.
        studio.instances[1].settings_id = "corner".to_string();
        studio.instances[2].settings_id = "button".to_string();
        studio.instances[3].settings_id = "studio-ref".to_string();
        studio.instances[3].properties.insert(
            "Value".to_string(),
            json!({"_type":"Ref","settingsId":"button"}),
        );
        studio.instances[1]
            .properties
            .insert("Sink".to_string(), json!({"_type":"EnumItem","name":"1"}));
        studio
            .instances
            .push(instance("ref", "StudioOnly", "Folder", Some(0)));
        (base, editor, studio)
    }

    #[test]
    fn reconciliation_matches_new_branches_without_duplicating_property_differences() {
        let path = Path::new("src/ReplicatedStorage/__roblox_sync_settings.renium");
        for iteration in 0..32 {
            let (base, mut editor, mut studio) = new_branch_fixture();
            // Vary traversal indices independently of structural path IDs.
            for i in 0..iteration {
                studio.instances.push(SettingsBytecodeInstance::new(
                    format!("unrelated-{i}"),
                    format!("Unrelated{i}"),
                    "Folder".to_string(),
                    Some(0),
                ));
            }
            if iteration % 2 == 0 {
                std::mem::swap(&mut editor, &mut studio);
            }
            let snapshot = |doc: &SettingsBytecode| {
                file_snapshot(&[(
                    path.to_str().unwrap(),
                    &encode_settings_bytecode(doc).unwrap(),
                )])
            };
            let (merged, conflicts) = merge_snapshots(
                Some(&snapshot(&base)),
                &snapshot(&editor),
                &snapshot(&studio),
                ConflictPreference::None,
            )
            .unwrap();
            assert!(conflicts.is_empty(), "{conflicts:?}");
            let merged = settings_document(merged.entries.get(path)).unwrap();
            for name in ["ShiftLockButton", "UICorner", "Reference", "StudioOnly"] {
                assert_eq!(
                    merged.instances.iter().filter(|i| i.name == name).count(),
                    1,
                    "{name}, iteration {iteration}"
                );
            }
            let corner = merged
                .instances
                .iter()
                .find(|i| i.name == "UICorner")
                .unwrap();
            assert_eq!(
                corner.properties,
                new_branch_fixture().1.instances[2].properties
            );
            let holder = merged
                .instances
                .iter()
                .find(|i| i.name == "Reference")
                .unwrap();
            assert_eq!(holder.properties["Value"]["settingsId"], corner.settings_id);
            assert_eq!(
                merged
                    .instances
                    .iter()
                    .map(|i| &i.settings_id)
                    .collect::<HashSet<_>>()
                    .len(),
                merged.instances.len()
            );
            let mut plan = ReconcilePushPlan::default();
            append_settings_push_plan(path, &merged, &studio, &mut plan).unwrap();
            assert!(
                !plan.recreated_settings_ids.contains(&corner.settings_id),
                "untouched corner scheduled for recreation"
            );
            assert!(
                plan.property_removals
                    .iter()
                    .all(|change| change.settings_id.as_deref() != Some(&corner.settings_id)),
                "unchanged radii scheduled for reset"
            );
        }
    }

    #[test]
    fn matched_new_property_conflicts_do_not_become_copies() {
        let (base, mut editor, mut studio) = new_branch_fixture();
        editor.instances[1]
            .properties
            .insert("Text".to_string(), json!("editor"));
        studio.instances[1]
            .properties
            .insert("Text".to_string(), json!("studio"));
        align_new_instance_ids(&base, &editor, &mut studio).unwrap();
        let (merged, conflicts) = merge_reconciliation_settings_documents(
            &base,
            &editor,
            &studio,
            ConflictPreference::None,
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(
            merged
                .instances
                .iter()
                .filter(|i| i.name == "ShiftLockButton")
                .count(),
            1
        );
        assert!(conflicts.iter().any(|c| c.detail.contains("Text")));
    }

    #[test]
    fn new_duplicate_names_with_different_values_are_not_guessed() {
        let (base, editor, mut studio) = new_branch_fixture();
        let mut duplicate = studio.instances[1].clone();
        duplicate.settings_id = "duplicate".to_string();
        studio.instances.push(duplicate);
        let before = encode_settings_bytecode(&studio).unwrap();
        let error = align_new_instance_ids(&base, &editor, &mut studio).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Ambiguous new duplicate instances")
        );
        assert_eq!(encode_settings_bytecode(&studio).unwrap(), before);
    }

    #[test]
    fn prepared_full_push_reuses_the_same_duplicate_and_reference_mapping() {
        let root = SettingsBytecodeInstance::new(
            "root".into(),
            "Workspace".into(),
            "Workspace".into(),
            None,
        );
        let mut first = SettingsBytecodeInstance::new(
            "first".into(),
            "Duplicate".into(),
            "StringValue".into(),
            Some(0),
        );
        first.properties.insert("Value".into(), json!("first"));
        let mut second = first.clone();
        second.settings_id = "second".into();
        second.properties.insert("Value".into(), json!("second"));
        let mut pointer = SettingsBytecodeInstance::new(
            "pointer".into(),
            "Pointer".into(),
            "ObjectValue".into(),
            Some(0),
        );
        pointer
            .properties
            .insert("Value".into(), json!({"_type":"Ref","settingsId":"first"}));
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root, first, second, pointer],
        };
        let mut observed = desired.clone();
        observed.instances.swap(1, 2);
        observed.instances[1].settings_id = "debug:second".into();
        observed.instances[2].settings_id = "debug:first".into();
        observed.instances[3].properties.insert(
            "Value".into(),
            json!({"_type":"Ref","settingsId":"debug:first"}),
        );
        desired.instances[0]
            .attributes
            .insert("Revision".into(), json!(1));
        let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let snapshot = |doc: &SettingsBytecode| ProjectSnapshot {
            entries: BTreeMap::from([(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(doc).unwrap()),
            )]),
        };
        let studio = snapshot(&observed);
        let project = snapshot(&desired);
        let mut prepared = HashMap::new();
        let paths = snapshot_differences_prepared(&project, &studio, Some(&mut prepared)).unwrap();
        assert_eq!(paths, snapshot_differences(&project, &studio).unwrap());
        assert_eq!(prepared.len(), 1);
        let change = &prepared[&path];
        assert_eq!(change.previous.instances[1].settings_id, "second");
        assert_eq!(change.previous.instances[2].settings_id, "first");
        assert_eq!(
            change.previous.instances[3].properties["Value"]["settingsId"],
            "first"
        );
        let expected = reconciliation_push_plan_for_paths(&studio, &project, &paths).unwrap();
        let actual = reconciliation_push_plan_for_paths_with_prepared_settings(
            &studio, &project, &paths, &prepared,
        )
        .unwrap();
        assert_eq!(actual.changed_paths, expected.changed_paths);
        assert_eq!(actual.target_settings_ids, expected.target_settings_ids);
        assert_eq!(
            actual.recreated_settings_ids,
            expected.recreated_settings_ids
        );
        assert_eq!(
            serde_json::to_value(&actual.instance_deletes).unwrap(),
            serde_json::to_value(&expected.instance_deletes).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&actual.property_removals).unwrap(),
            serde_json::to_value(&expected.property_removals).unwrap()
        );
    }

    #[test]
    fn removed_service_store_keeps_only_the_engine_service() {
        let path = Path::new("src/ServerStorage/__roblox_sync_settings.renium");
        let root = SettingsBytecodeInstance {
            settings_id: "root".into(),
            name: "ServerStorage".into(),
            class_name: "ServerStorage".into(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::from_iter([("Preserved".into(), json!(true))]),
        };
        let child = SettingsBytecodeInstance {
            settings_id: "child".into(),
            name: "MovedPointer".into(),
            class_name: "ObjectValue".into(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let document = |instances| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let entry =
            |doc: &SettingsBytecode| SnapshotEntry::File(encode_settings_bytecode(doc).unwrap());
        let before = document(vec![root.clone(), child.clone()]);
        let before_entry = entry(&before);
        let empty = document(Vec::new());
        let mut plan = ReconcilePushPlan::default();
        append_aligned_settings_push_plan(path, &empty, &before, &mut plan).unwrap();
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances.len(), 1);
        assert!(plan.property_removals.is_empty());
        let remaining = entry(&document(vec![root.clone()]));
        assert_eq!(
            settings_delta_mismatch(path, Some(&before_entry), None, Some(&remaining)).unwrap(),
            None
        );
        for changed_id in [false, true] {
            let mut retained = document(vec![root.clone(), child.clone()]);
            if changed_id {
                retained.instances[0].settings_id = "export-root".into();
                retained.instances[1].settings_id = "export-child".into();
            }
            assert!(
                settings_delta_mismatch(path, Some(&before_entry), None, Some(&entry(&retained)))
                    .unwrap()
                    .is_some_and(|message| message.contains("MovedPointer was not deleted"))
            );
        }
    }

    #[test]
    fn ordinary_reconciliation_never_deletes_a_package_link() {
        let root = SettingsBytecodeInstance {
            settings_id: "root".to_string(),
            name: "ReplicatedStorage".to_string(),
            class_name: "ReplicatedStorage".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let package_link = SettingsBytecodeInstance {
            settings_id: "link".to_string(),
            name: "PackageLink".to_string(),
            class_name: "PackageLink".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let baseline_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), package_link],
        };
        let editor_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root],
        };
        let settings_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
            entries: [(
                settings_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let baseline = snapshot(&baseline_doc);
        let editor = snapshot(&editor_doc);
        let studio = snapshot(&baseline_doc);
        let (_, conflicts) = merge_snapshots(
            Some(&baseline),
            &editor,
            &studio,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.contains("PackageLink"))
        );
        assert!(
            validate_editor_package_links(&baseline, &editor, std::slice::from_ref(&settings_path))
                .is_err()
        );
    }

    #[test]
    fn reconciliation_allows_replacing_an_entire_package_root() {
        let instance =
            |id: &str, name: &str, class_name: &str, parent_index| SettingsBytecodeInstance {
                settings_id: id.to_string(),
                name: name.to_string(),
                class_name: class_name.to_string(),
                parent_index,
                properties: Map::new(),
                attributes: Map::new(),
            };
        let root = instance("root", "Workspace", "Workspace", None);
        let old_package = instance("old-package", "Old car", "Model", Some(0));
        let old_link = instance("old-link", "PackageLink", "PackageLink", Some(1));
        let new_package = instance("new-package", "New car", "Model", Some(0));
        let new_link = instance("new-link", "PackageLink", "PackageLink", Some(1));
        let document = |instances| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let settings_path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let snapshot = |document: SettingsBytecode| ProjectSnapshot {
            entries: [(
                settings_path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let baseline = snapshot(document(vec![
            root.clone(),
            old_package.clone(),
            old_link.clone(),
        ]));
        let editor = baseline.clone();
        let studio = snapshot(document(vec![root.clone(), new_package, new_link]));

        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        let merged = settings_document(merged.entries.get(&settings_path)).unwrap();
        assert!(
            merged
                .instances
                .iter()
                .any(|instance| instance.name == "New car")
        );
        assert!(
            !merged
                .instances
                .iter()
                .any(|instance| instance.name == "Old car")
        );

        let removed_package = snapshot(document(vec![root]));
        assert!(
            validate_editor_package_links(
                &baseline,
                &removed_package,
                std::slice::from_ref(&settings_path),
            )
            .is_ok()
        );
        let direct_link_removal = snapshot(document(vec![
            instance("root", "Workspace", "Workspace", None),
            old_package,
        ]));
        assert!(
            validate_editor_package_links(
                &baseline,
                &direct_link_removal,
                std::slice::from_ref(&settings_path),
            )
            .is_err()
        );
    }

    #[test]
    fn package_link_guard_uses_project_identity_not_export_ids() {
        let document = |ids: [&str; 4], moved: bool, package_content: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: ids[0].to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: ids[1].to_string(),
                    name: "Container".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: ids[2].to_string(),
                    name: "Package".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(usize::from(moved)),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: ids[3].to_string(),
                    name: "PackageLink".to_string(),
                    class_name: "PackageLink".to_string(),
                    parent_index: Some(2),
                    properties: Map::from_iter([(
                        "PackageContent".to_string(),
                        Value::String(package_content.to_string()),
                    )]),
                    attributes: Map::new(),
                },
            ],
        };
        let path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
            entries: [(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let baseline = snapshot(&document(
            ["root", "container", "package", "link"],
            false,
            "rbxassetid://1",
        ));
        let exported = snapshot(&document(
            ["debug:1", "debug:2", "debug:3", "debug:4"],
            false,
            "rbxassetid://1",
        ));
        assert!(
            validate_editor_package_links(&baseline, &exported, std::slice::from_ref(&path))
                .is_ok()
        );
        let mut transport_omission = document(
            ["debug:1", "debug:2", "debug:3", "debug:4"],
            false,
            "rbxassetid://1",
        );
        transport_omission.instances[3]
            .properties
            .remove("PackageContent");
        transport_omission.instances[3]
            .properties
            .insert("Archivable".to_string(), Value::Bool(true));
        assert!(
            validate_editor_package_links(
                &baseline,
                &snapshot(&transport_omission),
                std::slice::from_ref(&path),
            )
            .is_ok()
        );
        let mut runtime_state = document(
            ["debug:1", "debug:2", "debug:3", "debug:4"],
            false,
            "rbxassetid://1",
        );
        runtime_state.instances[3]
            .properties
            .insert("ModifiedState".to_string(), Value::Number(1.into()));
        assert!(
            validate_editor_package_links(
                &baseline,
                &snapshot(&runtime_state),
                std::slice::from_ref(&path),
            )
            .is_ok()
        );
        let aligned = align_snapshot_ids(&baseline, &exported).unwrap();
        let aligned = settings_document(aligned.entries.get(&path)).unwrap();
        assert_eq!(aligned.instances[3].settings_id, "link");
        assert_eq!(
            aligned.instances[3].properties.get("PackageContent"),
            Some(&Value::String("rbxassetid://1".to_string()))
        );

        let moved = snapshot(&document(
            ["root", "container", "package", "link"],
            true,
            "rbxassetid://1",
        ));
        assert!(
            validate_editor_package_links(&baseline, &moved, std::slice::from_ref(&path)).is_ok()
        );

        let edited = snapshot(&document(
            ["debug:1", "debug:2", "debug:3", "debug:4"],
            false,
            "rbxassetid://2",
        ));
        assert!(
            validate_editor_package_links(&baseline, &edited, std::slice::from_ref(&path)).is_err()
        );
    }

    #[test]
    fn reconciliation_pushes_only_semantically_changed_instances() {
        let settings_snapshot = |service: &str, document: SettingsBytecode| {
            (
                PathBuf::from(format!("src/{service}/__roblox_sync_settings.renium")),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )
        };
        let root = |service: &str| SettingsBytecodeInstance {
            settings_id: format!("{service}-root"),
            name: service.to_string(),
            class_name: service.to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let replicated = |value: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                root("ReplicatedStorage"),
                SettingsBytecodeInstance {
                    settings_id: "changed-folder".to_string(),
                    name: "Changed".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::from_iter([(
                        "Archivable".to_string(),
                        Value::String(value.to_string()),
                    )]),
                    attributes: Map::new(),
                },
            ],
        };
        let server = |asset_id: &str, replacement_class: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                root("ServerStorage"),
                SettingsBytecodeInstance {
                    settings_id: "replacement".to_string(),
                    name: "Replacement".to_string(),
                    class_name: replacement_class.to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "public".to_string(),
                    name: "Public".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "package-link".to_string(),
                    name: "PackageLink".to_string(),
                    class_name: "PackageLink".to_string(),
                    parent_index: Some(2),
                    properties: Map::from_iter([(
                        "PackageId".to_string(),
                        Value::String(asset_id.to_string()),
                    )]),
                    attributes: Map::new(),
                },
            ],
        };
        let studio = ProjectSnapshot {
            entries: [
                settings_snapshot("ReplicatedStorage", replicated("before")),
                settings_snapshot("ServerStorage", server("old", "Folder")),
            ]
            .into_iter()
            .collect(),
        };
        let merged = ProjectSnapshot {
            entries: [
                settings_snapshot("ReplicatedStorage", replicated("after")),
                settings_snapshot("ServerStorage", server("new", "Model")),
            ]
            .into_iter()
            .collect(),
        };

        let plan = reconciliation_push_plan(&studio, &merged).unwrap();
        assert_eq!(
            plan.changed_paths,
            vec![
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
            ]
        );
        assert_eq!(
            plan.target_settings_ids,
            vec!["changed-folder", "replacement"]
        );
        assert_eq!(
            plan.previous_class_names
                .get("replacement")
                .map(String::as_str),
            Some("Folder")
        );
        assert!(
            !plan
                .target_settings_ids
                .iter()
                .any(|id| id == "package-link")
        );
        assert!(plan.instance_deletes.is_empty());

        let ordered = |ids: [&str; 3]| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: std::iter::once(root("ReplicatedStorage"))
                .chain(ids.into_iter().map(|id| SettingsBytecodeInstance {
                    settings_id: id.to_string(),
                    name: id.to_uppercase(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                }))
                .collect(),
        };
        let mut order_plan = ReconcilePushPlan::default();
        append_settings_push_plan(
            Path::new("src/ReplicatedStorage/__roblox_sync_settings.renium"),
            &ordered(["a", "c", "b"]),
            &ordered(["a", "b", "c"]),
            &mut order_plan,
        )
        .unwrap();
        assert!(order_plan.target_settings_ids.is_empty());
    }

    #[test]
    fn incremental_editor_plan_uses_persisted_ids_for_duplicate_siblings() {
        let path = PathBuf::from("src/Workspace/__roblox_sync_settings.renium");
        let root = SettingsBytecodeInstance {
            settings_id: "1".to_string(),
            name: "Workspace".to_string(),
            class_name: "Workspace".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let duplicate = |index: usize| SettingsBytecodeInstance {
            settings_id: format!("debug:{index}"),
            name: "Duplicate".to_string(),
            class_name: "StringValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([("Value".to_string(), json!(index.to_string()))]),
            attributes: Map::new(),
        };
        let package_link = SettingsBytecodeInstance {
            settings_id: "debug:package-link".to_string(),
            name: "PackageLink".to_string(),
            class_name: "PackageLink".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([("PackageContent".to_string(), json!("rbxassetid://1"))]),
            attributes: Map::new(),
        };
        let mut previous_instances = Vec::with_capacity(4_098);
        previous_instances.push(root.clone());
        previous_instances.extend((0..4_096).map(duplicate));
        previous_instances.push(package_link.clone());
        let previous_document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: previous_instances,
        };
        let mut current_document = previous_document.clone();
        current_document.instances[1_001]
            .properties
            .insert("Value".to_string(), json!("changed"));
        current_document.instances.remove(2_001);
        current_document.instances.push(SettingsBytecodeInstance {
            settings_id: "debug:new".to_string(),
            ..duplicate(4_096)
        });

        let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
            entries: [(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let previous = snapshot(&previous_document);
        let current = snapshot(&current_document);
        let prepared =
            prepare_editor_settings_changes(&previous, &current, std::slice::from_ref(&path))
                .unwrap();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &previous,
            &current,
            &HashSet::from_iter([path.clone()]),
            &prepared,
        )
        .unwrap();

        assert_eq!(plan.changed_paths, vec![path]);
        assert_eq!(
            plan.target_settings_ids,
            vec!["debug:1000".to_string(), "debug:new".to_string()]
        );
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances.len(), 1);
        assert_eq!(
            plan.instance_deletes[0].instances[0].settings_id,
            "debug:2000"
        );
    }

    #[test]
    fn cross_service_recreation_reapplies_external_referrers() {
        let root = |service: &str| SettingsBytecodeInstance {
            settings_id: format!("{service}-root"),
            name: service.to_string(),
            class_name: service.to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let target = |parent_index| SettingsBytecodeInstance {
            settings_id: "target".to_string(),
            name: "Target".to_string(),
            class_name: "StringValue".to_string(),
            parent_index: Some(parent_index),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let holder = SettingsBytecodeInstance {
            settings_id: "holder".to_string(),
            name: "Holder".to_string(),
            class_name: "ObjectValue".to_string(),
            parent_index: Some(1),
            properties: Map::from_iter([(
                "Value".to_string(),
                json!({"_type": "Ref", "settingsId": "target"}),
            )]),
            attributes: Map::new(),
        };
        let container = SettingsBytecodeInstance {
            settings_id: "container".to_string(),
            name: "Container".to_string(),
            class_name: "Folder".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let snapshot =
            |workspace: SettingsBytecode, replicated: SettingsBytecode| ProjectSnapshot {
                entries: [
                    (
                        PathBuf::from("src/Workspace/__roblox_sync_settings.renium"),
                        SnapshotEntry::File(encode_settings_bytecode(&workspace).unwrap()),
                    ),
                    (
                        PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                        SnapshotEntry::File(encode_settings_bytecode(&replicated).unwrap()),
                    ),
                ]
                .into_iter()
                .collect(),
            };
        let studio = snapshot(
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    root("Workspace"),
                    container.clone(),
                    target(1),
                    holder.clone(),
                ],
            },
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![root("ReplicatedStorage")],
            },
        );
        let desired = snapshot(
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![root("Workspace"), container, holder],
            },
            SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![root("ReplicatedStorage"), target(0)],
            },
        );

        let plan = reconciliation_push_plan(&studio, &desired).unwrap();

        assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
        assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
    }

    #[test]
    fn incremental_cross_service_recreation_reapplies_target_service_referrers() {
        let root = |service: &str| SettingsBytecodeInstance {
            settings_id: format!("{service}-root"),
            name: service.to_string(),
            class_name: service.to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let target = SettingsBytecodeInstance {
            settings_id: "target".to_string(),
            name: "Target".to_string(),
            class_name: "Folder".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let holder = |service: &str| SettingsBytecodeInstance {
            settings_id: "holder".to_string(),
            name: "Holder".to_string(),
            class_name: "ObjectValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([(
                "Value".to_string(),
                json!({
                    "_type": "Ref",
                    "settingsId": "target",
                    "pathSegments": [service, "Target"],
                    "pathOrdinals": [1, 1]
                }),
            )]),
            attributes: Map::new(),
        };
        let replicated_path = PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium");
        let storage_path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
        let previous_replicated = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("ReplicatedStorage"), target.clone()],
        };
        let previous_storage = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("ServerStorage"), holder("ReplicatedStorage")],
        };
        let current_replicated = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("ReplicatedStorage")],
        };
        let current_storage = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root("ServerStorage"), holder("ServerStorage"), target],
        };
        let previous = ProjectSnapshot {
            entries: [
                (
                    replicated_path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&previous_replicated).unwrap()),
                ),
                (
                    storage_path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&previous_storage).unwrap()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let current = ProjectSnapshot {
            entries: [
                (
                    replicated_path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&current_replicated).unwrap()),
                ),
                (
                    storage_path.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(&current_storage).unwrap()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let scopes = vec![replicated_path.clone(), storage_path.clone()];
        let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &previous,
            &current,
            &scopes.into_iter().collect(),
            &prepared,
        )
        .unwrap();

        assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
        assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
    }

    #[test]
    fn incremental_added_target_reapplies_existing_same_service_referrer() {
        let root = SettingsBytecodeInstance {
            settings_id: "storage-root".to_string(),
            name: "ServerStorage".to_string(),
            class_name: "ServerStorage".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let holder = |service: &str| SettingsBytecodeInstance {
            settings_id: "holder".to_string(),
            name: "Holder".to_string(),
            class_name: "ObjectValue".to_string(),
            parent_index: Some(0),
            properties: Map::from_iter([(
                "Value".to_string(),
                json!({
                    "_type": "Ref",
                    "settingsId": "target",
                    "pathSegments": [service, "Target"],
                    "pathOrdinals": [1, 1]
                }),
            )]),
            attributes: Map::new(),
        };
        let target = SettingsBytecodeInstance {
            settings_id: "target".to_string(),
            name: "Target".to_string(),
            class_name: "Folder".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let path = PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium");
        let previous_document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root.clone(), holder("ReplicatedStorage")],
        };
        let current_document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![root, holder("ServerStorage"), target],
        };
        let snapshot = |document: &SettingsBytecode| ProjectSnapshot {
            entries: [(
                path.clone(),
                SnapshotEntry::File(encode_settings_bytecode(document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        let previous = snapshot(&previous_document);
        let current = snapshot(&current_document);
        let scopes = vec![path.clone()];
        let prepared = prepare_editor_settings_changes(&previous, &current, &scopes).unwrap();
        let plan = reconciliation_push_plan_for_paths_with_prepared_settings(
            &previous,
            &current,
            &HashSet::from([path]),
            &prepared,
        )
        .unwrap();

        assert!(plan.target_settings_ids.iter().any(|id| id == "target"));
        assert!(plan.target_settings_ids.iter().any(|id| id == "holder"));
    }

    #[test]
    fn reconciliation_uses_targeted_deletes_and_blocks_direct_package_link_deletes() {
        let root = SettingsBytecodeInstance {
            settings_id: "root".to_string(),
            name: "ServerStorage".to_string(),
            class_name: "ServerStorage".to_string(),
            parent_index: None,
            properties: Map::new(),
            attributes: Map::new(),
        };
        let folder = SettingsBytecodeInstance {
            settings_id: "folder".to_string(),
            name: "Removed".to_string(),
            class_name: "Folder".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let snapshot = |instances| ProjectSnapshot {
            entries: [(
                PathBuf::from("src/ServerStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(
                    encode_settings_bytecode(&SettingsBytecode {
                        version: SETTINGS_BINARY_VERSION,
                        instances,
                    })
                    .unwrap(),
                ),
            )]
            .into_iter()
            .collect(),
        };
        let desired = snapshot(vec![root.clone()]);
        let studio = snapshot(vec![root.clone(), folder.clone()]);
        let plan = reconciliation_push_plan(&studio, &desired).unwrap();
        assert!(plan.changed_paths.is_empty());
        assert!(plan.target_settings_ids.is_empty());
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances.len(), 1);

        let plan = reconciliation_push_plan(&studio, &ProjectSnapshot::default()).unwrap();
        assert!(plan.changed_paths.is_empty());
        assert!(plan.target_settings_ids.is_empty());
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "folder");

        let mut changes = EditorChangeSet::default();
        changes.instance_changes.push(EditorInstanceChange {
            mode: "upsertInstances".to_string(),
            service: "ReplicatedStorage".to_string(),
            allow_deletes: false,
            instances: Vec::new(),
            preserve_instances: Vec::new(),
        });
        amend_reconciled_changes(&mut changes, plan).unwrap();
        assert_eq!(changes.instance_changes[0].mode, "deleteInstances");
        assert_eq!(changes.instance_changes[1].mode, "upsertInstances");

        let package_link = SettingsBytecodeInstance {
            settings_id: "link".to_string(),
            name: "PackageLink".to_string(),
            class_name: "PackageLink".to_string(),
            parent_index: Some(1),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let studio = snapshot(vec![root.clone(), folder.clone(), package_link]);
        let plan = reconciliation_push_plan(&studio, &desired).unwrap();
        assert_eq!(plan.instance_deletes.len(), 1);
        assert_eq!(plan.instance_deletes[0].instances[0].settings_id, "folder");

        let package_link = SettingsBytecodeInstance {
            settings_id: "link".to_string(),
            name: "PackageLink".to_string(),
            class_name: "PackageLink".to_string(),
            parent_index: Some(1),
            properties: Map::new(),
            attributes: Map::new(),
        };
        let studio = snapshot(vec![root.clone(), folder.clone(), package_link]);
        let desired = snapshot(vec![root, folder]);
        assert!(reconciliation_push_plan(&studio, &desired).is_err());
    }

    #[test]
    fn reconciliation_ignores_transient_instance_ids_and_script_guids() {
        let document =
            |root_id: &str, script_id: &str, script_guid: &str, value: &str| SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    SettingsBytecodeInstance {
                        settings_id: root_id.to_string(),
                        name: "ReplicatedStorage".to_string(),
                        class_name: "ReplicatedStorage".to_string(),
                        parent_index: None,
                        properties: Map::new(),
                        attributes: Map::new(),
                    },
                    SettingsBytecodeInstance {
                        settings_id: script_id.to_string(),
                        name: "Module".to_string(),
                        class_name: "ModuleScript".to_string(),
                        parent_index: Some(0),
                        properties: Map::from_iter([
                            (
                                "ScriptGuid".to_string(),
                                Value::String(script_guid.to_string()),
                            ),
                            ("Value".to_string(), Value::String(value.to_string())),
                        ]),
                        attributes: Map::new(),
                    },
                ],
            };
        let snapshot = |document: SettingsBytecode| ProjectSnapshot {
            entries: [(
                PathBuf::from("src/ReplicatedStorage/__roblox_sync_settings.renium"),
                SnapshotEntry::File(encode_settings_bytecode(&document).unwrap()),
            )]
            .into_iter()
            .collect(),
        };
        assert!(
            snapshots_equivalent(
                &snapshot(document("root-a", "script-a", "guid-a", "same")),
                &snapshot(document("root-b", "script-b", "guid-b", "same")),
            )
            .unwrap()
        );

        let baseline = snapshot(document("root-a", "script-a", "guid-a", "base"));
        let editor = snapshot(document("root-a", "script-a", "guid-a", "editor"));
        let studio = snapshot(document("root-b", "script-b", "guid-b", "base"));
        let (merged, conflicts) =
            merge_snapshots(Some(&baseline), &editor, &studio, ConflictPreference::None).unwrap();
        assert!(conflicts.is_empty());
        let settings = settings_document(merged.entries.values().next()).unwrap();
        assert_eq!(settings.instances.len(), 2);
        assert_eq!(
            settings.instances[1].properties.get("Value"),
            Some(&Value::String("editor".to_string()))
        );

        let mut editor = document("root-a", "script-a", "guid-a", "same");
        let mut studio = document("root-b", "script-b", "guid-b", "same");
        editor.instances[1].properties.extend([
            (
                "Position".to_string(),
                json!({"_type":"Vector3","x":0.10000000149011612,"y":2.0,"z":3.0}),
            ),
            (
                "WorldPosition".to_string(),
                json!({"_type":"Vector3","x":1.0,"y":2.0,"z":3.0}),
            ),
        ]);
        studio.instances[1].properties.extend([
            (
                "Position".to_string(),
                json!({"_type":"Vector3","x":0.10000000149011613,"y":2.0,"z":3.0}),
            ),
            (
                "WorldPosition".to_string(),
                json!({"_type":"Vector3","x":999.0,"y":2.0,"z":3.0}),
            ),
        ]);
        align_observation_ids_to_baseline(&editor, &mut studio);
        assert!(settings_documents_equivalent(&editor, &studio));

        let reference_document =
            |target_id: &str, holder_id: &str, reference_id: &str| SettingsBytecode {
                version: SETTINGS_BINARY_VERSION,
                instances: vec![
                    SettingsBytecodeInstance {
                        settings_id: "root".to_string(),
                        name: "ReplicatedStorage".to_string(),
                        class_name: "ReplicatedStorage".to_string(),
                        parent_index: None,
                        properties: Map::new(),
                        attributes: Map::new(),
                    },
                    SettingsBytecodeInstance {
                        settings_id: target_id.to_string(),
                        name: "Target".to_string(),
                        class_name: "Attachment".to_string(),
                        parent_index: Some(0),
                        properties: Map::new(),
                        attributes: Map::new(),
                    },
                    SettingsBytecodeInstance {
                        settings_id: holder_id.to_string(),
                        name: "Holder".to_string(),
                        class_name: "WeldConstraint".to_string(),
                        parent_index: Some(0),
                        properties: Map::from_iter([(
                            "Attachment0".to_string(),
                            json!({"_type":"Ref","settingsId":reference_id}),
                        )]),
                        attributes: Map::new(),
                    },
                ],
            };
        let baseline = reference_document("target", "holder", "target");
        let mut observed = reference_document("debug:target", "debug:holder", "debug:target");
        align_observation_ids_to_baseline(&baseline, &mut observed);
        assert_eq!(observed.instances[1].settings_id, "target");
        assert_eq!(observed.instances[2].settings_id, "holder");
        assert_eq!(
            observed.instances[2]
                .properties
                .get("Attachment0")
                .and_then(|value| value.get("settingsId")),
            Some(&Value::String("target".to_string()))
        );

        let duplicate = |id: &str, marker: &str| SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: "Duplicate".to_string(),
            class_name: "Folder".to_string(),
            parent_index: Some(0),
            properties: Map::new(),
            attributes: Map::from_iter([("Marker".to_string(), json!(marker))]),
        };
        let baseline = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                duplicate("a", "A"),
                duplicate("b", "B"),
            ],
        };
        let mut observed = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "observed-root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                duplicate("observed-b", "B"),
            ],
        };
        align_observation_ids_to_baseline(&baseline, &mut observed);
        assert_eq!(observed.instances[1].settings_id, "b");

        let baseline = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "debug:0_reserved".to_string(),
                    name: "Existing".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
            ],
        };
        let mut observed = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance {
                    settings_id: "observed-root".to_string(),
                    name: "ReplicatedStorage".to_string(),
                    class_name: "ReplicatedStorage".to_string(),
                    parent_index: None,
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "debug:0_reserved".to_string(),
                    name: "New".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
                SettingsBytecodeInstance {
                    settings_id: "observed-existing".to_string(),
                    name: "Existing".to_string(),
                    class_name: "Folder".to_string(),
                    parent_index: Some(0),
                    properties: Map::new(),
                    attributes: Map::new(),
                },
            ],
        };
        align_observation_ids_to_baseline(&baseline, &mut observed);
        assert_eq!(observed.instances[2].settings_id, "debug:0_reserved");
        assert_ne!(observed.instances[1].settings_id, "debug:0_reserved");
        encode_settings_bytecode(&observed).unwrap();
    }
}
