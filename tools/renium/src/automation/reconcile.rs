use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::BoundContext;
use super::context as bound_context;
use super::runtime::{automation_pull_args, automation_push_args};
use crate::app::update::user_data_dir;
use crate::cli::args::ImportSnapshotsArgs;
use crate::editor::review::local_place_path_for_runtime;
use crate::editor::sync::push_editor_changes_with_warm_bridge;
use crate::project::sourcemap::generate_project_sourcemap;
use crate::project::version_control::merge_settings_documents_with_policy;
use crate::project::{config, config::project_watch_inputs};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, decode_settings_bytecode, encode_settings_bytecode,
};
use crate::snapshot::export::{ExportProjectStage, export_snapshots_with_warm_bridge};
use crate::snapshot::import::import_snapshots_quiet;
use crate::snapshot::refs::remap_record_reference_ids;
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::files::{
    OnDrop, atomic_write_file, canonical_path, create_unique_directory,
    is_service_settings_file_name,
};

const RECORD_VERSION: u8 = 1;
const RECORD_DIR: &str = "reconcile";
const OWNERS_FILE: &str = "reconcile-owners.rmp.zst";

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
    baseline: Option<ProjectSnapshot>,
    #[serde(default)]
    head: Option<RecoveryHead>,
    #[serde(default)]
    conflicts: Vec<String>,
    #[serde(default)]
    resolution_required: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct TargetOwners {
    version: u8,
    owners: BTreeMap<String, String>,
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
    owners: Mutex<()>,
}

impl Coordinator {
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
            self.claim_target(&identity)?
        } else {
            None
        };
        if owner_conflict.is_some() {
            mode = PairMode::Verify;
        }
        let path = record_path(context, &pair_key);
        let existing = read_compressed::<PairRecord>(&path)?.filter(|record| {
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
        let requires_reconcile = record_missing
            || configuration_changed
            || record.mode != mode
            || record.conflict_preference != conflict_preference
            || record.head.is_some()
            || !record.conflicts.is_empty();
        if configuration_changed {
            record.baseline = None;
            record.head = None;
            record.conflicts.clear();
            record.resolution_required = false;
        }
        record.identity = identity;
        record.mode = mode;
        record.conflict_preference = conflict_preference;
        record.runtime_settings = runtime_settings;
        write_compressed(&path, &record)?;
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
        let pair_lock = self.pair_lock(&setup.key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let path = record_path(context, &setup.key);
        let mut record = read_compressed::<PairRecord>(&path)?
            .context("Reconciliation state disappeared while starting Live Sync")?;
        let _gate = bridge.acquire_request_gate();
        let _selection = bound_context::select(context);
        let (stage, studio) = capture_studio_project(context, bridge)?;
        let editor = capture_snapshot(Path::new(&context.root), stage.publish_paths())?;
        if let Some(head) = &record.head {
            let editor_hash = snapshot_hash(&editor)?;
            let studio_hash = snapshot_hash(&studio)?;
            let editor_known = editor_hash == head.editor_before || editor_hash == head.intended;
            let studio_known = studio_hash == head.studio_before || studio_hash == head.intended;
            if !editor_known || !studio_known {
                record.conflicts = vec![
                    "An interrupted reconciliation was followed by new edits; review both sides"
                        .to_string(),
                ];
                record.resolution_required = false;
                write_compressed(&path, &record)?;
                setup.mode = PairMode::Verify;
                setup.resolution_required = false;
                setup.error = Some(conflict_message(&record.conflicts));
                return Ok(());
            }
            if editor_hash == head.intended && studio_hash == head.intended {
                record.baseline = Some(editor);
                record.head = None;
                record.conflicts.clear();
                record.resolution_required = false;
                write_compressed(&path, &record)?;
                setup.resolution_required = false;
                return Ok(());
            }
        }
        let (merged, conflicts) = merge_snapshots(
            record.baseline.as_ref(),
            &editor,
            &studio,
            setup
                .resolution_preference
                .unwrap_or(record.conflict_preference),
        )?;

        if !conflicts.is_empty() {
            record.conflicts = conflicts;
            record.resolution_required = setup.resolution_preference.is_none()
                && record.conflict_preference == ConflictPreference::None;
            write_compressed(&path, &record)?;
            setup.mode = PairMode::Verify;
            setup.resolution_required = record.resolution_required;
            setup.error = Some(conflict_message(&record.conflicts));
            return Ok(());
        }

        if setup.mode == PairMode::Verify {
            if snapshots_equivalent(&editor, &studio)? {
                record.baseline = Some(merged);
                record.conflicts.clear();
                record.head = None;
                record.resolution_required = false;
            } else {
                record.conflicts = vec!["Studio and project files differ".to_string()];
                record.resolution_required = false;
                setup.error = Some(conflict_message(&record.conflicts));
            }
            write_compressed(&path, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }

        if snapshots_equivalent(&editor, &merged)? && snapshots_equivalent(&studio, &merged)? {
            record.baseline = Some(merged);
            record.conflicts.clear();
            record.head = None;
            record.resolution_required = false;
            write_compressed(&path, &record)?;
            setup.resolution_required = false;
            return Ok(());
        }

        record.head = Some(RecoveryHead {
            editor_before: snapshot_hash(&editor)?,
            studio_before: snapshot_hash(&studio)?,
            intended: snapshot_hash(&merged)?,
        });
        record.conflicts.clear();
        record.resolution_required = false;
        write_compressed(&path, &record)?;

        apply_snapshot(&stage.project_root, stage.publish_paths(), &merged)?;
        generate_project_sourcemap(&stage.project_root)?;
        push_staged_project(context, &stage, bridge)?;
        let (readback_stage, readback) = capture_studio_project(context, bridge)?;
        if !snapshots_equivalent(&readback, &merged)? {
            bail!("Studio did not retain the reconciled project state");
        }
        readback_stage.publish(Path::new(&context.root))?;

        record.baseline = Some(readback);
        record.head = None;
        setup.resolution_required = false;
        write_compressed(&path, &record)
    }

    pub(crate) fn reconcile_current(
        &self,
        context: &BoundContext,
        bridge: &BridgeServer,
    ) -> Result<PairSetup> {
        let identity = PairIdentity::from_context(context, bridge)?;
        let key = identity.pair_key();
        let record = read_compressed::<PairRecord>(&record_path(context, &key))?
            .context("Reconciliation state is missing")?;
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
        let path = record_path(context, key);
        let mut record =
            read_compressed::<PairRecord>(&path)?.context("Reconciliation state is missing")?;
        if record.mode != PairMode::Reconcile {
            return Ok(());
        }
        if record.head.is_some() || !record.conflicts.is_empty() {
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
            validate_editor_package_links(baseline, &current, &scopes)?;
        }
        baseline
            .entries
            .retain(|path, _| !scopes.iter().any(|scope| path.starts_with(scope)));
        baseline.entries.extend(current.entries);
        write_compressed(&path, &record)
    }

    pub(crate) fn baseline_files(
        &self,
        context: &BoundContext,
        key: &str,
        paths: &[PathBuf],
    ) -> Result<BTreeMap<String, String>> {
        let pair_lock = self.pair_lock(key);
        let _pair = pair_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let record = read_compressed::<PairRecord>(&record_path(context, key))?
            .context("Reconciliation state is missing")?;
        let baseline = record
            .baseline
            .as_ref()
            .context("Reconciliation baseline is missing")?;
        let root = Path::new(&context.root);
        let mut files = BTreeMap::new();
        for requested in paths {
            let absolute = if requested.is_absolute() {
                requested.clone()
            } else {
                root.join(requested)
            };
            let Ok(relative) = absolute.strip_prefix(root) else {
                continue;
            };
            let Some(SnapshotEntry::File(bytes)) = baseline.entries.get(relative) else {
                continue;
            };
            let content = String::from_utf8(bytes.clone())
                .with_context(|| format!("Baseline file {} is not UTF-8", relative.display()))?;
            files.insert(absolute.to_string_lossy().into_owned(), content);
        }
        Ok(files)
    }

    fn claim_target(&self, identity: &PairIdentity) -> Result<Option<String>> {
        let _owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        let path = user_data_dir()?.join(OWNERS_FILE);
        let mut owners = read_compressed::<TargetOwners>(&path)?.unwrap_or(TargetOwners {
            version: RECORD_VERSION,
            owners: BTreeMap::new(),
        });
        if owners.version != RECORD_VERSION {
            owners = TargetOwners {
                version: RECORD_VERSION,
                owners: BTreeMap::new(),
            };
        }
        let target = identity.target_key();
        match owners.owners.get(&target) {
            Some(owner) if owner == &identity.project => Ok(None),
            Some(owner) if Path::new(owner).exists() => Ok(Some(owner.clone())),
            Some(_) => {
                owners.owners.insert(target, identity.project.clone());
                write_compressed(&path, &owners)?;
                Ok(None)
            }
            None => {
                owners.owners.insert(target, identity.project.clone());
                write_compressed(&path, &owners)?;
                Ok(None)
            }
        }
    }
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
        let after = settings_document(current.entries.get(&path))?;
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
        if before_links.len() != after_links.len()
            || before_links.iter().any(|(id, before_index)| {
                after_links.get(id).is_none_or(|after_index| {
                    !settings_instances_equal(&before, *before_index, &after, *after_index)
                })
            })
        {
            bail!(
                "{} changes a PackageLink directly; use the package workflow instead",
                path.display()
            );
        }
    }
    Ok(())
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

fn sync_services() -> Vec<String> {
    DEFAULT_SYNC_SERVICES
        .iter()
        .map(|service| (*service).to_string())
        .collect()
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

fn capture_studio_project(
    context: &BoundContext,
    bridge: &BridgeServer,
) -> Result<(ExportProjectStage, ProjectSnapshot)> {
    let services = sync_services();
    let src_dir = bound_context::source_dir(context)?;
    let stage = ExportProjectStage::create(Path::new(&context.root), &src_dir, &services)?;
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
    let args = automation_pull_args(context, &parameters, false)?;
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Main)?;
    export_snapshots_with_warm_bridge(args, bridge, &info, 0.0, false)?;
    import_snapshots_quiet(ImportSnapshotsArgs {
        snapshot_dir: parameters["snapshotDir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| capture_dir.clone()),
        project_root: stage.import_project_root.clone(),
        src_dir: stage.import_src_dir.clone(),
        services: sync_services().join(","),
        no_project_write: true,
        threads: 0,
    })?;
    stage.finish_projection(true)?;
    let snapshot = capture_snapshot(&stage.project_root, stage.publish_paths())?;
    Ok((stage, snapshot))
}

fn staged_context(context: &BoundContext, stage: &ExportProjectStage) -> Result<BoundContext> {
    let project_relative = Path::new(&context.project).strip_prefix(&context.root)?;
    let source_relative = Path::new(&context.source).strip_prefix(&context.root)?;
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

fn push_staged_project(
    context: &BoundContext,
    stage: &ExportProjectStage,
    bridge: &BridgeServer,
) -> Result<()> {
    let context = staged_context(context, stage)?;
    let _selection = bound_context::select(&context);
    pin_edit_runtime(&context, bridge)?;
    let summary = push_editor_changes_with_warm_bridge(
        automation_push_args(&context, &json!({ "verifySources": true }), false)?,
        bridge,
    )?;
    if summary.get("skippedByReview").and_then(Value::as_bool) == Some(true) {
        bail!("Reconciled changes require review before Studio can be updated");
    }
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

fn merge_snapshots(
    baseline: Option<&ProjectSnapshot>,
    editor: &ProjectSnapshot,
    studio: &ProjectSnapshot,
    preference: ConflictPreference,
) -> Result<(ProjectSnapshot, Vec<String>)> {
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
    for path in paths {
        let base = baseline.and_then(|snapshot| snapshot.entries.get(&path));
        let editor_entry = editor.entries.get(&path);
        let studio_entry = studio.entries.get(&path);
        let value = if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            merge_settings_entry(
                &path,
                base,
                editor_entry,
                studio_entry,
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
        if let Some(value) = value {
            merged.insert(path, value);
        }
    }
    for path in merged.keys() {
        let mut parent = path.parent();
        while let Some(ancestor) = parent.filter(|ancestor| !ancestor.as_os_str().is_empty()) {
            if !merged.contains_key(ancestor)
                && baseline
                    .and_then(|snapshot| snapshot.entries.get(ancestor))
                    .is_some_and(|entry| matches!(entry, SnapshotEntry::Directory))
            {
                conflicts.push(format!(
                    "{} remains under a directory deleted on the other side",
                    path.display()
                ));
                break;
            }
            parent = ancestor.parent();
        }
    }
    conflicts.sort();
    conflicts.dedup();
    Ok((ProjectSnapshot { entries: merged }, conflicts))
}

fn merge_entry(
    path: &Path,
    base: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> Option<SnapshotEntry> {
    if entries_equivalent(path, editor, studio) {
        return editor.cloned();
    }
    if !first_pairing {
        if entries_equivalent(path, editor, base) {
            return studio.cloned();
        }
        if entries_equivalent(path, studio, base) {
            return editor.cloned();
        }
    } else {
        if editor.is_none() {
            return studio.cloned();
        }
        if studio.is_none() {
            return editor.cloned();
        }
    }
    let ordinary_file_conflict = matches!(editor, Some(SnapshotEntry::File(_)))
        && matches!(studio, Some(SnapshotEntry::File(_)))
        && (first_pairing || matches!(base, Some(SnapshotEntry::File(_))));
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

fn normalized_source(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            normalized.push(b'\n');
            index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
        } else {
            normalized.push(bytes[index]);
        }
        index += 1;
    }
    normalized
}

fn entries_equivalent(
    path: &Path,
    left: Option<&SnapshotEntry>,
    right: Option<&SnapshotEntry>,
) -> bool {
    match (left, right) {
        (Some(SnapshotEntry::File(left)), Some(SnapshotEntry::File(right)))
            if is_source_path(path) =>
        {
            left == right || normalized_source(left) == normalized_source(right)
        }
        _ => left == right,
    }
}

fn settings_document(entry: Option<&SnapshotEntry>) -> Result<SettingsBytecode> {
    match entry {
        Some(SnapshotEntry::File(bytes)) => decode_settings_bytecode(bytes),
        None => Ok(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        }),
        Some(_) => bail!("A Renium settings store is not a regular file"),
    }
}

fn merge_settings_entry(
    path: &Path,
    base: Option<&SnapshotEntry>,
    editor: Option<&SnapshotEntry>,
    studio: Option<&SnapshotEntry>,
    preference: ConflictPreference,
    first_pairing: bool,
    conflicts: &mut Vec<String>,
) -> Result<Option<SnapshotEntry>> {
    if editor == studio {
        return Ok(editor.cloned());
    }
    let mut editor_doc = settings_document(editor)?;
    let mut studio_doc = settings_document(studio)?;
    let merged = if first_pairing {
        align_first_pairing(
            path,
            &mut editor_doc,
            &mut studio_doc,
            preference,
            conflicts,
        );
        let empty = SettingsBytecode {
            version: editor_doc.version.max(studio_doc.version),
            instances: Vec::new(),
        };
        let (merged, merge_conflicts) = merge_settings_documents_with_policy(
            &empty,
            &editor_doc,
            &studio_doc,
            preference_bool(preference),
            None,
        );
        conflicts.extend(
            merge_conflicts
                .into_iter()
                .map(|conflict| format!("{}: {}", path.display(), conflict.detail)),
        );
        merged
    } else {
        let mut base_doc = settings_document(base)?;
        align_observation_ids_to_baseline(&base_doc, &mut editor_doc);
        align_observation_ids_to_baseline(&base_doc, &mut studio_doc);
        protect_package_links(
            path,
            Some(&base_doc),
            &mut editor_doc,
            &mut studio_doc,
            conflicts,
        );
        align_transient_script_guids(&mut base_doc, &mut editor_doc, &mut studio_doc);
        let (merged, merge_conflicts) = merge_settings_documents_with_policy(
            &base_doc,
            &editor_doc,
            &studio_doc,
            preference_bool(preference),
            None,
        );
        conflicts.extend(
            merge_conflicts
                .into_iter()
                .map(|conflict| format!("{}: {}", path.display(), conflict.detail)),
        );
        merged
    };
    if merged.instances.is_empty() && editor.is_none() && studio.is_none() {
        Ok(None)
    } else {
        Ok(Some(SnapshotEntry::File(encode_settings_bytecode(
            &merged,
        )?)))
    }
}

fn preference_bool(preference: ConflictPreference) -> Option<bool> {
    match preference {
        ConflictPreference::None => None,
        ConflictPreference::Editor => Some(true),
        ConflictPreference::Studio => Some(false),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct StructuralPart {
    name: String,
    class_name: String,
    ordinal: usize,
}

type StructuralKey = Vec<StructuralPart>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PairingPart {
    name: String,
    ordinal: usize,
}

type PairingKey = Vec<PairingPart>;

fn structural_keys(document: &SettingsBytecode) -> Vec<StructuralKey> {
    let mut keys = Vec::with_capacity(document.instances.len());
    let mut ordinals = HashMap::<(Option<usize>, String, String), usize>::new();
    for instance in &document.instances {
        let ordinal = ordinals
            .entry((
                instance.parent_index,
                instance.name.clone(),
                instance.class_name.clone(),
            ))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let mut key: StructuralKey = instance
            .parent_index
            .and_then(|parent| keys.get(parent).cloned())
            .unwrap_or_default();
        key.push(StructuralPart {
            name: instance.name.clone(),
            class_name: instance.class_name.clone(),
            ordinal: *ordinal,
        });
        keys.push(key);
    }
    keys
}

fn pairing_keys(document: &SettingsBytecode) -> Vec<PairingKey> {
    let mut keys = Vec::with_capacity(document.instances.len());
    let mut ordinals = HashMap::<(Option<usize>, String), usize>::new();
    for instance in &document.instances {
        let ordinal = ordinals
            .entry((instance.parent_index, instance.name.clone()))
            .and_modify(|value| *value += 1)
            .or_insert(1);
        let mut key: PairingKey = instance
            .parent_index
            .and_then(|parent| keys.get(parent).cloned())
            .unwrap_or_default();
        key.push(PairingPart {
            name: instance.name.clone(),
            ordinal: *ordinal,
        });
        keys.push(key);
    }
    keys
}

fn align_observation_ids_to_baseline(baseline: &SettingsBytecode, observed: &mut SettingsBytecode) {
    let baseline_keys = structural_keys(baseline);
    let observed_keys = structural_keys(observed);
    let baseline_by_key = baseline_keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| (key, baseline.instances[index].settings_id.clone()))
        .collect::<HashMap<_, _>>();
    let observed_ids = observed
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let remap = observed
        .instances
        .iter()
        .zip(observed_keys)
        .filter_map(|(instance, key)| {
            let baseline_id = baseline_by_key.get(&key)?;
            (instance.settings_id != *baseline_id && !observed_ids.contains(baseline_id))
                .then(|| (instance.settings_id.clone(), baseline_id.clone()))
        })
        .collect::<HashMap<_, _>>();
    for instance in &mut observed.instances {
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
    let editor_keys = structural_keys(editor);
    let studio_keys = structural_keys(studio);
    let editor_pairing_keys = pairing_keys(editor);
    let studio_pairing_keys = pairing_keys(studio);
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
                render_pairing_key(key)
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
    protect_package_links(path, None, editor, studio, conflicts);

    for (key, editor_index) in &editor_by_key {
        let Some(studio_index) = studio_by_key.get(key).copied() else {
            continue;
        };
        if settings_instances_equal(editor, *editor_index, studio, studio_index) {
            continue;
        }
        let duplicate_slot = if key.last().is_some_and(|part| part.ordinal > 1) {
            true
        } else {
            let mut second = key.clone();
            if let Some(part) = second.last_mut() {
                part.ordinal = 2;
            }
            editor_by_key.contains_key(&second) || studio_by_key.contains_key(&second)
        };
        if duplicate_slot {
            conflicts.push(format!(
                "{} has ambiguous duplicate instances at {}",
                path.display(),
                render_structural_key(key)
            ));
            continue;
        }
        match preference {
            ConflictPreference::None => conflicts.push(format!(
                "{} has different values for {}",
                path.display(),
                render_structural_key(key)
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
        && maps_equal_ignoring_script_guid(&left_instance.properties, &right_instance.properties)
        && left_instance.attributes == right_instance.attributes
}

fn maps_equal_ignoring_script_guid(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    left.iter()
        .filter(|(name, _)| name.as_str() != "ScriptGuid")
        .eq(right
            .iter()
            .filter(|(name, _)| name.as_str() != "ScriptGuid"))
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

fn render_structural_key(key: &[StructuralPart]) -> String {
    if key.is_empty() {
        return "the service root".to_string();
    }
    key.iter()
        .map(|part| {
            if part.ordinal == 1 {
                part.name.clone()
            } else {
                format!("{}[{}]", part.name, part.ordinal)
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn render_pairing_key(key: &[PairingPart]) -> String {
    key.iter()
        .map(|part| {
            if part.ordinal == 1 {
                part.name.clone()
            } else {
                format!("{}[{}]", part.name, part.ordinal)
            }
        })
        .collect::<Vec<_>>()
        .join(".")
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

fn snapshots_equivalent(left: &ProjectSnapshot, right: &ProjectSnapshot) -> Result<bool> {
    if left.entries.len() != right.entries.len() {
        return Ok(false);
    }
    for (path, left_entry) in &left.entries {
        let Some(right_entry) = right.entries.get(path) else {
            return Ok(false);
        };
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            let mut left_doc = settings_document(Some(left_entry))?;
            let mut right_doc = settings_document(Some(right_entry))?;
            let mut conflicts = Vec::new();
            align_first_pairing(
                path,
                &mut left_doc,
                &mut right_doc,
                ConflictPreference::None,
                &mut conflicts,
            );
            if !conflicts.is_empty() || left_doc.instances.len() != right_doc.instances.len() {
                return Ok(false);
            }
            let left_keys = structural_keys(&left_doc);
            let right_keys = structural_keys(&right_doc);
            let right_by_key = right_keys
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, key)| (key, index))
                .collect::<HashMap<_, _>>();
            if left_keys.iter().enumerate().any(|(left_index, key)| {
                right_by_key.get(key).is_none_or(|right_index| {
                    !settings_instances_equal(&left_doc, left_index, &right_doc, *right_index)
                })
            }) {
                return Ok(false);
            }
        } else if !entries_equivalent(path, Some(left_entry), Some(right_entry)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn snapshot_hash(snapshot: &ProjectSnapshot) -> Result<String> {
    let mut normalized = snapshot.clone();
    for (path, entry) in &mut normalized.entries {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            let SnapshotEntry::File(bytes) = entry else {
                bail!("A Renium settings store is not a regular file");
            };
            let mut document = decode_settings_bytecode(bytes)?;
            let keys = structural_keys(&document);
            let mut ordered = keys.iter().cloned().enumerate().collect::<Vec<_>>();
            ordered.sort_by(|(_, left), (_, right)| left.cmp(right));
            let remap = ordered
                .into_iter()
                .enumerate()
                .map(|(canonical, (index, _))| {
                    (
                        document.instances[index].settings_id.clone(),
                        format!("baseline:{canonical}"),
                    )
                })
                .collect::<HashMap<_, _>>();
            for instance in &mut document.instances {
                if let Some(id) = remap.get(&instance.settings_id) {
                    instance.settings_id.clone_from(id);
                }
                instance.properties.remove("ScriptGuid");
                remap_record_reference_ids(&mut instance.properties, &remap);
                remap_record_reference_ids(&mut instance.attributes, &remap);
            }
            *bytes = encode_settings_bytecode(&document)?;
        } else if is_source_path(path)
            && let SnapshotEntry::File(bytes) = entry
            && bytes.contains(&b'\r')
        {
            *bytes = normalized_source(bytes);
        }
    }
    let bytes = rmp_serde::to_vec(&normalized).context("Failed to hash reconciliation state")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn apply_snapshot(root: &Path, roots: &[PathBuf], snapshot: &ProjectSnapshot) -> Result<()> {
    for relative in roots {
        if !derived_project_path(relative) {
            remove_path(&root.join(relative))?;
        }
    }
    for (relative, entry) in &snapshot.entries {
        let path = root.join(relative);
        match entry {
            SnapshotEntry::Directory => fs::create_dir_all(&path)
                .with_context(|| format!("Failed to create {}", path.display()))?,
            SnapshotEntry::File(bytes) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, bytes)
                    .with_context(|| format!("Failed to write {}", path.display()))?;
            }
            SnapshotEntry::Symlink { target, directory } => {
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
        .join(format!("{key}.rmp.zst"))
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

fn write_compressed<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let encoded = rmp_serde::to_vec(value).context("Failed to encode reconciliation state")?;
    let compressed = zstd::stream::encode_all(encoded.as_slice(), 1)
        .context("Failed to compress reconciliation state")?;
    atomic_write_file(path, &compressed)
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
        assert_eq!(
            snapshot_hash(&editor).unwrap(),
            snapshot_hash(&studio).unwrap()
        );

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
        let (_, structural_conflicts) = merge_snapshots(
            Some(&shared),
            &deleted,
            &changed,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert_eq!(structural_conflicts.len(), 1);

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
        let (_, structural_conflicts) = merge_snapshots(
            Some(&baseline),
            &ProjectSnapshot::default(),
            &studio,
            ConflictPreference::Editor,
        )
        .unwrap();
        assert_eq!(structural_conflicts.len(), 1);
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
    fn snapshot_hash_ignores_transient_instance_ids_and_script_guids() {
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
        assert_eq!(
            snapshot_hash(&snapshot(document("root-a", "script-a", "guid-a", "same"))).unwrap(),
            snapshot_hash(&snapshot(document("root-b", "script-b", "guid-b", "same"))).unwrap()
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
    }
}
