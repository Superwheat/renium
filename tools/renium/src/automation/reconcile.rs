use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::BoundContext;
use super::context as bound_context;
use super::runtime::{automation_pull_args, automation_push_args};
use crate::app::output::{ensure_plugin_api_ok, log_global};
use crate::app::timing::elapsed_ms;
use crate::cli::PushEditorChangesArgs;
use crate::editor::diff::editor_instance_descriptor_for_known_path;
use crate::editor::paths::{
    build_editor_instance_paths, build_editor_instance_paths_for_indices,
    build_editor_source_paths_by_index,
};
use crate::editor::review::local_place_path_for_runtime;
use crate::editor::sync::{
    StudioChangeGuard, expand_editor_changed_paths,
    push_reconciled_editor_changes_with_warm_bridge, settings_file_hash,
};
use crate::editor::types::{
    EditorChangeSet, EditorInstanceChange, EditorInstancePath, EditorPropertyChange,
};
use crate::project::version_control::{
    VcMergeConflict, merge_settings_documents_with_policy_and_source_changes,
};
use crate::project::{config, config::project_watch_inputs};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
    encode_settings_bytecode,
};
use crate::settings::equivalence::{
    align_equivalent_values, align_settings_ids_to_reference, canonicalize_settings_property_names,
    drop_settings_documents, reconciliation_maps_equal, reconciliation_property_is_derived,
    reconciliation_property_value, reconciliation_values_equal, reconciliation_values_map_equal,
    remove_reconciliation_derived_properties, settings_documents_equivalent,
    settings_documents_positionally_equivalent, stabilize_settings_reference_ids,
};
use crate::snapshot::export::{ExportProjectStage, export_snapshots_with_warm_bridge};
use crate::snapshot::refs::remap_record_reference_ids;
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::files::{
    OnDrop, absolutize_under, atomic_write_file, canonical_path, create_unique_directory,
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
    previous_class_names: HashMap<String, String>,
    previous_paths: HashMap<(String, String), EditorInstancePath>,
    instance_deletes: Vec<EditorInstanceChange>,
    property_removals: Vec<EditorPropertyChange>,
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

fn saved_local_file_for_context(context: &BoundContext) -> Result<Option<PathBuf>> {
    let record_dir = Path::new(&context.root).join(".renium").join(RECORD_DIR);
    let entries = match fs::read_dir(&record_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect {}", record_dir.display()));
        }
    };
    let project = canonical_string(Path::new(&context.root))?;
    let experience = canonical_string(Path::new(&context.experience))?;
    let mut files = HashSet::new();
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
        if let Some(file) = record.identity.local_file.map(PathBuf::from)
            && file.is_file()
        {
            files.insert(file);
        }
    }
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
    pub(crate) mode: PairMode,
    resolution_preference: Option<ConflictPreference>,
    pub(crate) resolution_required: bool,
    pub(crate) error: Option<String>,
    pub(crate) requires_reconcile: bool,
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
        });
        let configuration_changed = record.identity.fingerprint != identity.fingerprint;
        let obsolete_head = record.head.take().is_some();
        let record_changed = record_missing
            || obsolete_head
            || record.identity != identity
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || record.runtime_settings != runtime_settings;
        let requires_reconcile = record_missing
            || configuration_changed
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || !record.conflicts.is_empty();
        if configuration_changed {
            record.baseline = None;
            record.conflicts.clear();
            record.resolution_required = false;
        }
        record.identity = identity;
        record.mode = mode;
        record.conflict_preference = conflict_preference;
        record.runtime_settings = runtime_settings;
        if record_changed {
            write_record(context, &pair_key, &record)?;
        }
        Ok(PairSetup {
            key: pair_key,
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
        })
    }

    pub(crate) fn reconcile(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
        setup: &mut PairSetup,
    ) -> Result<()> {
        // Every operation that needs both locks takes the bridge gate first.
        // LiveLoop::execute_push already follows this order.
        let _gate = bridge.acquire_request_gate();
        let pair_lock = self.pair_lock(&setup.key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = load_record(context, &setup.key)?
            .context("Reconciliation state disappeared while starting Live Sync")?;
        let _selection = bound_context::select(context);
        let studio_guard = current_studio_change_guard(context, bridge)?;
        let phase = Instant::now();
        let (stage, studio) = capture_studio_project_for_comparison(context, bridge)?;
        let publish_paths = stage.publish_paths().to_vec();
        let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
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
                write_record(context, &setup.key, &record)?;
                log_reconcile_timing("baseline write", phase);
            }
            setup.resolution_required = false;
            return Ok(());
        }
        let phase = Instant::now();
        let baseline = record
            .baseline
            .as_ref()
            .map(|baseline| baseline.load(Path::new(&context.root), &setup.key))
            .transpose()?;
        log_reconcile_timing("baseline load", phase);
        let phase = Instant::now();
        let (mut merged, conflicts, mut changes) = merge_snapshots_with_changes(
            baseline.as_ref(),
            &editor,
            &studio,
            setup
                .resolution_preference
                .unwrap_or(record.conflict_preference),
            Some(&side_differences),
        )?;
        log_reconcile_timing("merge", phase);

        if !conflicts.is_empty() {
            record.conflicts = conflicts;
            record.resolution_required = setup.resolution_preference.is_none()
                && record.conflict_preference == ConflictPreference::None;
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
            write_record(context, &setup.key, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }

        let _readback = if changes.studio.is_empty() {
            if !changes.editor.is_empty() {
                capture_studio_project(context, bridge)?
                    .0
                    .publish(Path::new(&context.root), false)?;
            }
            studio
        } else {
            let phase = Instant::now();
            let push_plan = reconciliation_push_plan_for_paths(&studio, &merged, &changes.studio)?;
            log_reconcile_timing("push plan", phase);
            if push_plan.is_empty() {
                if !changes.editor.is_empty() {
                    capture_studio_project(context, bridge)?
                        .0
                        .publish(Path::new(&context.root), false)?;
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
                    push_plan,
                    Some(&studio_guard),
                    automation_push_args(context, &json!({}), false)?,
                    Some(&editor),
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
        log_global(
            5,
            format_args!("[renium] reconcile current: cx={}", context.id),
        );
        let identity = PairIdentity::from_context(context, bridge)?;
        let key = identity.pair_key();
        let record = load_record(context, &key)?.context("Reconciliation state is missing")?;
        let mut setup = PairSetup {
            key,
            mode: record.mode,
            resolution_preference: None,
            resolution_required: record.resolution_required,
            error: None,
            requires_reconcile: true,
        };
        self.reconcile(context, bridge, &mut setup)?;
        Ok(setup)
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
    ) -> Result<Vec<PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = load_record(context, key)?.context("Reconciliation state is missing")?;
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
            return Ok(Vec::new());
        }
        let root = Path::new(&context.root);
        let previous = baseline.load_scopes(root, key, &scopes)?;
        let current = capture_snapshot(root, &scopes)?;
        validate_editor_package_links(&previous, &current, &scopes)?;
        let changed = previous
            .entries
            .keys()
            .chain(current.entries.keys())
            .cloned()
            .collect::<HashSet<_>>();
        let plan = reconciliation_push_plan_for_paths(&previous, &current, &changed)?;
        let supporting_scopes = supporting_settings_scopes(context, &changed)?;
        let supporting = capture_snapshot(root, &supporting_scopes)?;
        let supporting_paths = supporting.entries.keys().cloned().collect::<HashSet<_>>();
        let source = bound_context::source_dir(context)?;
        let stage = ExportProjectStage::create(root, &source, &[])?;
        apply_snapshot_paths(&stage.project_root, &supporting_paths, &supporting)?;
        apply_snapshot_paths(&stage.project_root, &changed, &current)?;
        let generated = push_staged_project(
            context,
            &stage,
            bridge,
            plan,
            guard,
            automation_push_args(context, &json!({}), false)?,
            None,
        )?
        .generated;
        let baseline = record
            .baseline
            .as_mut()
            .context("Reconciliation baseline is missing")?;
        baseline.replace_scopes(root, key, &scopes, &current)?;
        if !generated.entries.is_empty() {
            let generated_scopes = generated.entries.keys().cloned().collect::<Vec<_>>();
            baseline.replace_scopes(root, key, &generated_scopes, &generated)?;
        }
        let generated_paths = generated
            .entries
            .keys()
            .map(|path| root.join(path))
            .collect();
        write_record(context, key, &record)?;
        Ok(generated_paths)
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
    for path in paths {
        let before = settings_document(baseline.entries.get(&path))?;
        let mut after = settings_document(current.entries.get(&path))?;
        align_observation_ids_to_baseline(&before, &mut after);
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
        let mismatch = if before_links.len() != after_links.len() {
            Some(format!(
                "PackageLink count changed from {} to {}",
                before_links.len(),
                after_links.len()
            ))
        } else {
            before_links.iter().find_map(|(id, before_index)| {
                let after_index = after_links
                    .get(id)
                    .copied()
                    .ok_or_else(|| format!("PackageLink identity {id} is missing"));
                match after_index {
                    Ok(after_index)
                        if package_link_instances_equal(
                            &before,
                            *before_index,
                            &after,
                            after_index,
                        ) =>
                    {
                        None
                    }
                    Ok(after_index) => Some(package_link_mismatch_detail(
                        id,
                        &before,
                        *before_index,
                        &after,
                        after_index,
                    )),
                    Err(detail) => Some(detail),
                }
            })
        };
        if let Some(mismatch) = mismatch {
            bail!(
                "{} changes a PackageLink directly; use the package workflow instead ({mismatch})",
                path.display(),
            );
        }
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
        service_generations,
    })
}

fn current_studio_change_guard(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<StudioChangeGuard> {
    let runtime_id = context
        .runtime_id
        .as_deref()
        .context("Studio context has no edit-mode runtime")?;
    pin_edit_runtime(context, bridge)?;
    let state = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({ "start": true, "includeGenerations": true }),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_secs(10)),
    )?;
    ensure_plugin_api_ok(&state)?;
    studio_change_guard_from_state(context, &state)
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
    let services = sync_services();
    let root = PathBuf::from(&context.root);
    let src_dir = bound_context::source_dir(context)?;
    let requires_stage = config::try_load_project(None, Some(&root))?
        .as_ref()
        .map(config::project_requires_temporary_stage)
        .transpose()?
        .unwrap_or(false);
    let stage = if requires_stage {
        ExportProjectStage::create(&root, &src_dir, &services)?
    } else {
        ExportProjectStage::create_for_comparison(&root, &src_dir, &services)?
    };
    capture_studio_services_with_stage(context, bridge, &services, true, stage)
}

pub(crate) fn push_project_delta(
    context: &BoundContext,
    bridge: &BridgeServer,
    services: &[String],
    push_args: PushEditorChangesArgs,
    guard: Option<&StudioChangeGuard>,
) -> Result<Map<String, Value>> {
    let guard = guard
        .cloned()
        .map_or_else(|| current_studio_change_guard(context, bridge), Ok)?;
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
    let differences = snapshot_differences(&project, &studio)?;
    if differences.is_empty() {
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let plan = reconciliation_push_plan_for_paths(&studio, &project, &differences)?;
    if plan.is_empty() {
        return Ok(Map::from_iter([("ok".to_string(), Value::Bool(true))]));
    }
    let mutation_paths = plan.changed_paths.iter().cloned().collect::<HashSet<_>>();
    apply_snapshot_paths(&stage.project_root, &mutation_paths, &project)?;
    let pushed = push_staged_project(
        context,
        &stage,
        bridge,
        plan,
        Some(&guard),
        push_args,
        Some(&project),
    )?;

    let current = capture_snapshot(&root, stage.publish_paths())?;
    let mut verification_paths = differences;
    verification_paths.extend(mutation_paths);
    verification_paths.extend(pushed.generated.entries.keys().cloned());
    let verification_services = services_for_snapshot_paths(context, &verification_paths);
    let (_readback_stage, readback) =
        capture_studio_services(context, bridge, &verification_services, false)?;
    let mismatches = snapshot_path_differences(&readback, &current, &verification_paths)?;
    if !mismatches.is_empty() {
        let details = snapshot_mismatch_details(&readback, &current, &mismatches)?;
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
    Ok(pushed.summary)
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
    let snapshot = capture_snapshot(&stage.project_root, stage.publish_paths())?;
    Ok((stage, snapshot))
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

fn push_staged_project(
    context: &BoundContext,
    stage: &ExportProjectStage,
    bridge: &BridgeServer,
    plan: ReconcilePushPlan,
    guard: Option<&StudioChangeGuard>,
    mut push_args: PushEditorChangesArgs,
    expected_project: Option<&ProjectSnapshot>,
) -> Result<StagedPushResult> {
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
        let staged = capture_snapshot(&stage.project_root, &supporting_paths)?;
        let current = capture_snapshot(root, &supporting_paths)?;
        let supporting_paths = supporting_paths.into_iter().collect::<HashSet<_>>();
        let differences = snapshot_path_differences(&staged, &current, &supporting_paths)?;
        if !differences.is_empty() {
            bail!(
                "{} changed while its Studio update was being prepared; retry the sync",
                root.join(&differences[0]).display()
            );
        }
        apply_snapshot_paths(&stage.project_root, &supporting_paths, &current)?;
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
                "{} changed while its Studio update was being prepared; retry the sync",
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
        |changes| {
            amend_reconciled_changes(changes, plan)?;
            generated = redirect_staged_settings_writes(
                changes,
                &stage.project_root,
                Path::new(&context.root),
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
        if settings_file_hash(&destination)? != write.expected_hash {
            bail!(
                "{} changed while its Studio update was being prepared; retry the sync",
                destination.display()
            );
        }
        write.path = destination;
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
    let mut recreated = plan
        .instance_deletes
        .iter()
        .flat_map(|change| &change.instances)
        .map(|instance| instance.settings_id.as_str())
        .filter(|settings_id| targeted.contains(settings_id))
        .map(str::to_string)
        .collect::<HashSet<_>>();
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
    let service = desired
        .instances
        .iter()
        .chain(&observed.instances)
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .with_context(|| format!("{} has no service root", path.display()))?;
    let mut observed = observed.clone();
    if !align_settings_ids_to_reference(desired, &mut observed) {
        bail!("Could not align Studio identities in {service}; Studio was not changed");
    }
    align_equivalent_values(desired, &mut observed);
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
    for (index, path) in build_editor_instance_paths(&observed, &service)
        .into_iter()
        .enumerate()
    {
        let Some(path) = path else {
            continue;
        };
        let instance = &observed.instances[index];
        plan.previous_paths
            .insert((service.clone(), instance.settings_id.clone()), path);
    }
    let mut pending_property_removals = Vec::new();

    for (desired_index, instance) in desired.instances.iter().enumerate() {
        if instance.class_name == "PackageLink" {
            continue;
        }
        let observed_index = observed_by_id.get(instance.settings_id.as_str()).copied();
        if observed_index.is_none_or(|observed_index| {
            !settings_instances_equal(desired, desired_index, &observed, observed_index)
        }) {
            plan.target_settings_ids.push(instance.settings_id.clone());
        }
        let Some(observed_index) = observed_index else {
            continue;
        };
        let observed_instance = &observed.instances[observed_index];
        if observed_instance.class_name != instance.class_name {
            plan.previous_class_names.insert(
                instance.settings_id.clone(),
                observed_instance.class_name.clone(),
            );
            continue;
        }
        let reset_properties = observed_instance
            .properties
            .keys()
            .filter(|name| {
                name.as_str() != "ScriptGuid"
                    && !reconciliation_property_is_derived(name)
                    && !instance.properties.contains_key(*name)
            })
            .cloned()
            .collect::<Vec<_>>();
        let deleted_attributes = observed_instance
            .attributes
            .keys()
            .filter(|name| !instance.attributes.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        if reset_properties.is_empty() && deleted_attributes.is_empty() {
            continue;
        }
        pending_property_removals.push((desired_index, reset_properties, deleted_attributes));
    }

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
        return Ok(());
    }
    let package_ancestors = observed
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.class_name == "PackageLink")
        .flat_map(|(index, _)| {
            let mut ancestors = Vec::new();
            let mut current = Some(index);
            while let Some(index) = current {
                ancestors.push(index);
                current = observed.instances[index].parent_index;
            }
            ancestors
        })
        .collect::<HashSet<_>>();
    let observed_paths =
        build_editor_instance_paths_for_indices(&observed, &service, &root_removals);
    let mut descriptors = Vec::with_capacity(root_removals.len());
    for index in root_removals {
        if package_ancestors.contains(&index) {
            bail!(
                "Reconciliation would remove a PackageLink through {}; Studio was not changed",
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
                &observed,
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

fn normalized_source_bytes(bytes: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut index = 0;
    std::iter::from_fn(move || {
        let byte = *bytes.get(index)?;
        if byte == b'\r' {
            index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
        }
        index += 1;
        Some(if byte == b'\r' { b'\n' } else { byte })
    })
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
            left == right || normalized_source_bytes(left).eq(normalized_source_bytes(right))
        }
        _ => left == right,
    }
}

fn settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    let mut document = match entry {
        Some(SnapshotEntry::File(bytes)) => decode_settings_bytecode(bytes),
        None => Ok(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        }),
        Some(_) => bail!("A Renium settings store is not a regular file"),
    }?;
    stabilize_settings_reference_ids(&mut document);
    canonicalize_settings_property_names(&mut document)?;
    Ok(document)
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
        align_equivalent_new_instance_ids(&base_doc, &editor_doc, &mut studio_doc);
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
    merge_settings_documents_with_policy_and_source_changes(
        base,
        studio,
        editor,
        prefer_studio,
        prefer_studio,
        studio_source_changes,
        editor_source_changes,
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

fn align_equivalent_new_instance_ids(
    baseline: &SettingsBytecode,
    editor: &SettingsBytecode,
    studio: &mut SettingsBytecode,
) {
    let baseline_ids = baseline
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let mut interner = PathInterner::new();
    let editor_keys = structural_path_ids(editor, &mut interner);
    let studio_keys = structural_path_ids(studio, &mut interner);
    let editor_by_key = editor_keys
        .into_iter()
        .enumerate()
        .collect::<HashMap<_, _>>();
    let studio_ids = studio
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    let remap = studio_keys
        .into_iter()
        .enumerate()
        .filter_map(|(studio_index, key)| {
            let editor_index = editor_by_key.get(&key).copied()?;
            let editor_id = editor.instances[editor_index].settings_id.as_str();
            let studio_id = studio.instances[studio_index].settings_id.as_str();
            (!baseline_ids.contains(editor_id)
                && !baseline_ids.contains(studio_id)
                && editor_id != studio_id
                && !studio_ids.contains(editor_id)
                && settings_instances_equal(editor, editor_index, studio, studio_index))
            .then(|| (studio_id.to_string(), editor_id.to_string()))
        })
        .collect::<HashMap<_, _>>();
    for instance in &mut studio.instances {
        if let Some(id) = remap.get(&instance.settings_id) {
            instance.settings_id.clone_from(id);
        }
        remap_record_reference_ids(&mut instance.properties, &remap);
        remap_record_reference_ids(&mut instance.attributes, &remap);
    }
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
        .flat_map(|document| &document.instances)
        .filter(|instance| instance.class_name == "PackageLink")
        .map(|instance| (instance.settings_id.clone(), instance))
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

    for (id, base_instance) in &base_by_id {
        if !studio_by_id.contains_key(id) {
            conflicts.push(format!(
                "{} would remove PackageLink {} from Studio",
                path.display(),
                base_instance.name
            ));
        }
        if !editor_by_id.contains_key(id) && studio_by_id.contains_key(id) {
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
        && reconciliation_maps_equal(&left_instance.properties, &right_instance.properties)
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
        if !snapshot_entry_equivalent(path, left.entries.get(path), right.entries.get(path))? {
            differences.push(path.clone());
        }
    }
    differences.sort();
    Ok(differences)
}

fn snapshot_entry_equivalent(
    path: &Path,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
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
        let equivalent = if settings_documents_positionally_equivalent(&left, &right) {
            Some(true)
        } else if align_settings_ids_to_reference(&left, &mut right) {
            Some(settings_documents_equivalent(&left, &right))
        } else {
            None
        };
        drop_settings_documents(left, right);
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
    let observed = settings_document(observed.entries.get(path))?;
    let expected = settings_document(expected.entries.get(path))?;
    if observed.instances.len() != expected.instances.len() {
        return Ok(Some(format!(
            "Studio returned {} instances; expected {}",
            observed.instances.len(),
            expected.instances.len()
        )));
    }
    for (observed, expected) in observed.instances.iter().zip(&expected.instances) {
        if observed.name != expected.name
            || observed.class_name != expected.class_name
            || observed.parent_index != expected.parent_index
        {
            return Ok(Some(format!(
                "Studio returned a different structure at {}",
                expected.name
            )));
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
    let mut paths = left
        .entries
        .keys()
        .chain(right.entries.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
        .into_par_iter()
        .map(|path| {
            let equivalent = snapshot_entry_equivalent(
                &path,
                left.entries.get(&path),
                right.entries.get(&path),
            )?;
            Ok((!equivalent).then_some(path))
        })
        .collect::<Result<Vec<_>>>()
        .map(|paths| paths.into_iter().flatten().collect())
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
    use crate::settings::bytecode::SettingsBytecodeInstance;

    fn file_snapshot(entries: &[(&str, &[u8])]) -> ProjectSnapshot {
        ProjectSnapshot {
            entries: entries
                .iter()
                .map(|(path, bytes)| (PathBuf::from(path), SnapshotEntry::File(bytes.to_vec())))
                .collect(),
        }
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
    fn reconciliation_uses_targeted_deletes_and_blocks_package_subtrees() {
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
        let studio = snapshot(vec![root, folder, package_link]);
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
