use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::editor::paths::infer_source_script;
use crate::settings::bytecode::SettingsBytecode;
use crate::system::files::{
    absolutize_for_daemon as absolute_path, atomic_write_file, ends_with_ignore_ascii_case,
    exact_path_key as path_slash, is_windows_reserved_name, path_extension_is,
    service_settings_path,
};

mod adapter_format;
mod jsonc;
mod model_json;
mod projection;
mod projection_references;
mod syncback;
mod validation;

use adapter_format::{AdapterFormat, adapter_format};
use jsonc::format_jsonc;
pub(crate) use jsonc::parse_jsonc_value;

use projection::{
    adapter_target_script_path, compile_projection, file_target_destination, is_metadata_sidecar,
    metadata_sidecar_target, nested_project_targets, path_is_ignored, sync_rule_instance_name,
    sync_rule_matches, target_segments,
};
pub use projection::{
    compiled_files_to_studio_filters, compiled_files_to_studio_ignore_unknown_targets,
    project_requires_temporary_stage, project_structural_store, project_target_is_declarative,
    stage_project, stage_project_cached,
};
use syncback::{
    build_adapters, is_nested_project_path, plan_adapter_syncback, projection_instance_paths,
    stage_adapter_syncback_projection, watch_adapters,
};
pub use syncback::{
    syncback_project_adapters, syncback_project_adapters_from_root, syncback_project_projection,
};
pub(crate) use validation::validate_project;

pub const PROJECT_FILE_NAME: &str = "renium.project.jsonc";
pub const PROJECT_JSON_FILE_NAME: &str = "renium.project.json";
pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const PROJECT_SCHEMA_URL: &str = "https://raw.githubusercontent.com/Superwheat/renium/main/tools/renium/schemas/renium.project.schema.json";
static SCRIPT_NAMING_CACHE: OnceLock<Mutex<HashMap<PathBuf, ProjectScriptNaming>>> =
    OnceLock::new();
static GLOB_MATCHER_CACHE: OnceLock<Mutex<HashMap<String, GlobMatcher>>> = OnceLock::new();
static PROJECTION_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedProjection>>> = OnceLock::new();
thread_local! {
    static NESTED_STAGE_STACK: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
    static PROJECTION_TRANSFORM_STACK: RefCell<Vec<Vec<ProjectionTransform>>> = const { RefCell::new(Vec::new()) };
    static PROJECTION_IDENTITY_STACK: RefCell<Vec<HashMap<String, ProjectionIdentity>>> = const { RefCell::new(Vec::new()) };
    static PROJECT_TARGET_STACK: RefCell<Vec<(Vec<String>, Vec<usize>)>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProjectTarget {
    Shorthand(String),
    Structured {
        segments: Vec<String>,
        #[serde(default)]
        ordinals: Vec<usize>,
    },
}

impl ProjectTarget {
    fn from_parts(segments: Vec<String>, ordinals: Vec<usize>) -> Self {
        Self::Structured { segments, ordinals }
    }

    pub fn segments(&self) -> Vec<String> {
        match self {
            Self::Shorthand(value) => value.split('.').map(str::to_string).collect(),
            Self::Structured { segments, .. } => segments.clone(),
        }
    }

    pub fn ordinals(&self) -> Vec<usize> {
        match self {
            Self::Shorthand(_) => Vec::new(),
            Self::Structured { ordinals, .. } => ordinals.clone(),
        }
    }

    fn key(&self) -> String {
        serde_json::to_string(&(self.segments(), self.ordinals()))
            .expect("project targets are JSON-serializable")
    }

    fn with_prefix(&self, prefix: &[String]) -> Self {
        let mut segments = prefix.to_vec();
        let own_segments = self.segments();
        segments.extend(own_segments.iter().cloned());
        let mut ordinals = active_target_ordinals(prefix);
        if ordinals.is_empty() {
            ordinals.resize(prefix.len(), 1);
        }
        let mut own_ordinals = self.ordinals();
        if own_ordinals.is_empty() {
            own_ordinals.resize(own_segments.len(), 1);
        }
        ordinals.extend(own_ordinals);
        Self::Structured { segments, ordinals }
    }
}

fn target_is_within(target: &ProjectTarget, parent: &ProjectTarget) -> bool {
    let target_segments = target.segments();
    let parent_segments = parent.segments();
    if target_segments.len() < parent_segments.len()
        || target_segments[..parent_segments.len()] != parent_segments
    {
        return false;
    }
    let target_ordinals = target.ordinals();
    let parent_ordinals = parent.ordinals();
    parent_ordinals
        .iter()
        .enumerate()
        .all(|(index, ordinal)| target_ordinals.get(index).copied().unwrap_or(1) == *ordinal)
}

fn targets_are_equal(left: &ProjectTarget, right: &ProjectTarget) -> bool {
    let left_ordinals = left.ordinals();
    let right_ordinals = right.ordinals();
    left.segments() == right.segments()
        && left_ordinals
            .iter()
            .enumerate()
            .all(|(index, ordinal)| right_ordinals.get(index).copied().unwrap_or(1) == *ordinal)
        && right_ordinals
            .iter()
            .enumerate()
            .all(|(index, ordinal)| left_ordinals.get(index).copied().unwrap_or(1) == *ordinal)
}

impl fmt::Display for ProjectTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shorthand(value) => formatter.write_str(value),
            Self::Structured { segments, ordinals } => {
                formatter.write_str(&segments.join("."))?;
                if !ordinals.is_empty() {
                    write!(formatter, "@{ordinals:?}")?;
                }
                Ok(())
            }
        }
    }
}

fn with_project_target<T>(
    target: &ProjectTarget,
    operation: impl FnOnce(&[String]) -> Result<T>,
) -> Result<T> {
    validate_instance_target(target, "target")?;
    let segments = target.segments();
    let ordinals = target.ordinals();
    with_target_parts(&segments, &ordinals, operation)
}

fn with_target_parts<T>(
    segments: &[String],
    ordinals: &[usize],
    operation: impl FnOnce(&[String]) -> Result<T>,
) -> Result<T> {
    PROJECT_TARGET_STACK.with(|stack| {
        stack
            .borrow_mut()
            .push((segments.to_vec(), ordinals.to_vec()))
    });
    let result = operation(segments);
    PROJECT_TARGET_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
    result
}

fn active_target_ordinals(target: &[String]) -> Vec<usize> {
    PROJECT_TARGET_STACK.with(|stack| {
        stack
            .borrow()
            .iter()
            .rev()
            .find(|(segments, _)| target.starts_with(segments))
            .map(|(segments, ordinals)| {
                if segments == target {
                    return ordinals.clone();
                }
                let mut output = ordinals.clone();
                if output.is_empty() && !segments.is_empty() {
                    output.resize(segments.len(), 1);
                }
                output.resize(target.len(), 1);
                output
            })
            .unwrap_or_default()
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ReniumProject {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub name: Option<String>,
    pub source_root: PathBuf,
    pub build_target: Option<ProjectTarget>,
    pub root: ProjectNode,
    pub tree: BTreeMap<String, ProjectNode>,
    pub mounts: Vec<ProjectMount>,
    pub adapters: Vec<AdapterSpec>,
    pub sync_rules: Vec<SyncRule>,
    pub glob_ignore_paths: Vec<String>,
    pub filters: Vec<FilterRule>,
    pub script_extension: ScriptExtensionPolicy,
    pub export_naming: ExportNaming,
    pub settings: Value,
}

impl Default for ReniumProject {
    fn default() -> Self {
        Self {
            schema: Some(PROJECT_SCHEMA_URL.to_string()),
            schema_version: PROJECT_SCHEMA_VERSION,
            name: None,
            source_root: PathBuf::from("src"),
            build_target: None,
            root: ProjectNode::default(),
            tree: BTreeMap::new(),
            mounts: Vec::new(),
            adapters: Vec::new(),
            sync_rules: Vec::new(),
            glob_ignore_paths: Vec::new(),
            filters: Vec::new(),
            script_extension: ScriptExtensionPolicy::default(),
            export_naming: ExportNaming::default(),
            settings: Value::Object(Map::new()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProjectNode {
    #[serde(rename = "$id")]
    pub id: Option<String>,
    #[serde(rename = "$path")]
    pub path: Option<PathBuf>,
    #[serde(rename = "$className")]
    pub class_name: Option<String>,
    #[serde(rename = "$properties")]
    pub properties: Map<String, Value>,
    #[serde(rename = "$attributes")]
    pub attributes: Map<String, Value>,
    #[serde(rename = "$tags")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "$ignoreUnknownInstances")]
    pub ignore_unknown_instances: Option<bool>,
    #[serde(flatten)]
    pub children: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMount {
    pub source: PathBuf,
    pub target: ProjectTarget,
    #[serde(default)]
    pub ownership: MountOwnership,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MountOwnership {
    #[default]
    Exclusive,
    Overlay,
    ReadOnly,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterSpec {
    pub source: PathBuf,
    pub target: ProjectTarget,
    pub output: Option<PathBuf>,
    pub format: Option<String>,
    #[serde(default)]
    pub direction: AdapterDirection,
    #[serde(default)]
    pub generated: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRule {
    pub pattern: String,
    pub exclude: Option<String>,
    #[serde(rename = "use")]
    pub middleware: String,
    pub suffix: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterDirection {
    #[default]
    ToProject,
    FromProject,
    TwoWay,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FilterRule {
    pub action: FilterAction,
    pub direction: FilterDirection,
    pub glob: Option<String>,
    pub name: Option<String>,
    pub class: Option<String>,
    pub tag: Option<String>,
    pub attribute: Option<String>,
    pub property: Option<String>,
    pub id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FilterAction {
    Include,
    #[default]
    Ignore,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FilterDirection {
    #[default]
    Both,
    StudioToFiles,
    FilesToStudio,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScriptExtensionPolicy {
    #[default]
    Preserve,
    Luau,
    Lua,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportNaming {
    pub server_suffix: String,
    pub client_suffix: String,
    pub module_suffix: String,
    pub plugin_suffix: String,
    pub client_run_context_suffix: String,
}

impl Default for ExportNaming {
    fn default() -> Self {
        Self {
            server_suffix: ".server".to_string(),
            client_suffix: ".client".to_string(),
            module_suffix: String::new(),
            plugin_suffix: ".plugin".to_string(),
            client_run_context_suffix: ".run-client".to_string(),
        }
    }
}
#[derive(Clone)]
pub struct ProjectScriptNaming {
    pub extension: ScriptExtensionPolicy,
    pub server_suffix: String,
    pub client_suffix: String,
    pub module_suffix: String,
    pub plugin_suffix: String,
    pub client_run_context_suffix: String,
}

impl ProjectScriptNaming {
    fn from_export(extension: ScriptExtensionPolicy, naming: ExportNaming) -> Self {
        Self {
            extension,
            server_suffix: naming.server_suffix,
            client_suffix: naming.client_suffix,
            module_suffix: naming.module_suffix,
            plugin_suffix: naming.plugin_suffix,
            client_run_context_suffix: naming.client_run_context_suffix,
        }
    }
}

impl Default for ProjectScriptNaming {
    fn default() -> Self {
        Self::from_export(ScriptExtensionPolicy::Preserve, ExportNaming::default())
    }
}

#[derive(Args)]
pub struct FmtProjectArgs {
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub check: bool,
}

#[derive(Args)]
pub struct ExplainPathArgs {
    pub path: PathBuf,
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    Get(ConfigGetArgs),
    Set(ConfigSetArgs),
    Unset(ConfigUnsetArgs),
    Reset(ConfigScopeArgs),
    List(ConfigListArgs),
    Edit(ConfigScopeArgs),
    Path(ConfigScopeArgs),
    Export(ConfigExportArgs),
}

#[derive(Args)]
pub struct ConfigGetArgs {
    pub key: Option<String>,
    #[arg(long, value_enum, default_value_t = ConfigScope::Merged)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Args)]
pub struct ConfigSetArgs {
    pub key: String,
    pub value: String,
    #[arg(long, value_enum, default_value_t = ConfigScope::Place)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long)]
    pub string: bool,
}

#[derive(Args)]
pub struct ConfigUnsetArgs {
    pub key: String,
    #[arg(long, value_enum, default_value_t = ConfigScope::Place)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Args)]
pub struct ConfigScopeArgs {
    #[arg(long, value_enum, default_value_t = ConfigScope::Place)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Args)]
pub struct ConfigListArgs {
    #[arg(long, value_enum, default_value_t = ConfigScope::Merged)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long)]
    pub origins: bool,
}

#[derive(Args)]
pub struct ConfigExportArgs {
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum ConfigScope {
    User,
    Workspace,
    Experience,
    Place,
    Merged,
}

#[derive(Args)]
pub struct AdaptersArgs {
    #[command(subcommand)]
    pub command: AdaptersCommand,
}

#[derive(Subcommand)]
pub enum AdaptersCommand {
    Validate(AdapterProjectArgs),
    Build(AdapterBuildArgs),
    Syncback(AdapterSyncbackArgs),
    Watch(AdapterWatchArgs),
}

#[derive(Args)]
pub struct AdapterProjectArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
}

#[derive(Args)]
pub struct AdapterBuildArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long)]
    pub check: bool,
}

#[derive(Args)]
pub struct AdapterSyncbackArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, conflicts_with = "preview")]
    pub check: bool,
    #[arg(long)]
    pub preview: bool,
}

#[derive(Args)]
pub struct AdapterWatchArgs {
    #[arg(long)]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 250)]
    pub interval_ms: u64,
}

#[derive(Args)]
pub struct ImportRojoArgs {
    #[arg(long, value_name = "PATH")]
    pub project: PathBuf,
    #[arg(long, conflicts_with = "apply")]
    pub preview: bool,
    #[arg(long)]
    pub apply: bool,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub force: bool,
}

#[derive(Serialize)]
struct ProjectionEntry {
    id: String,
    kind: String,
    source: String,
    target: String,
    ownership: Option<MountOwnership>,
    direction: Option<AdapterDirection>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompiledProjection {
    schema_version: u32,
    project: String,
    entries: Vec<ProjectionEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct MetadataSidecar {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    schema_version: Option<u32>,
    #[serde(rename = "$id", alias = "id")]
    id: Option<String>,
    #[serde(rename = "$className", alias = "className")]
    class_name: Option<String>,
    #[serde(rename = "$properties", alias = "properties")]
    properties: Map<String, Value>,
    #[serde(rename = "$attributes", alias = "attributes")]
    attributes: Map<String, Value>,
    #[serde(rename = "$tags", alias = "tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "$ignoreUnknownInstances", alias = "ignoreUnknownInstances")]
    ignore_unknown_instances: Option<bool>,
}

pub struct LoadedProject {
    pub path: PathBuf,
    pub root: PathBuf,
    pub project: ReniumProject,
}

pub struct ProjectionStage {
    root: PathBuf,
    temporary: bool,
    cleanup: bool,
    transforms: Vec<ProjectionTransform>,
    identities: HashMap<String, ProjectionIdentity>,
}
#[derive(Clone)]
struct ProjectionTransform {
    target: Vec<String>,
    source: PathBuf,
    script_class_name: Option<&'static str>,
}
#[derive(Clone)]
struct ProjectionIdentity {
    source: PathBuf,
    settings_id: String,
}

struct CachedProjection {
    root: PathBuf,
    project_hash: String,
    source_shape: HashMap<String, u8>,
    transforms: Vec<ProjectionTransform>,
    identities: HashMap<String, ProjectionIdentity>,
}

impl ProjectionStage {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_temporary(&self) -> bool {
        self.temporary
    }

    pub fn transformed_source_for_target(&self, target: &[String]) -> Option<&Path> {
        self.transforms
            .iter()
            .find(|transform| transform.target == target)
            .map(|transform| transform.source.as_path())
    }

    pub fn transformed_scripts(&self) -> impl Iterator<Item = (&[String], &Path, &'static str)> {
        self.transforms.iter().filter_map(|transform| {
            transform.script_class_name.map(|class_name| {
                (
                    transform.target.as_slice(),
                    transform.source.as_path(),
                    class_name,
                )
            })
        })
    }

    pub fn target_is_transformed(&self, target: &[String]) -> bool {
        self.transforms
            .iter()
            .any(|transform| target.starts_with(&transform.target))
    }

    pub fn canonical_identity(&self, staged_id: &str) -> Option<(&Path, &str)> {
        self.identities
            .get(staged_id)
            .map(|identity| (identity.source.as_path(), identity.settings_id.as_str()))
    }
}

impl Drop for ProjectionStage {
    fn drop(&mut self) {
        if self.cleanup {
            remove_cached_script_naming(&self.root);
            let _ = fs::remove_dir_all(&self.root);
            remove_empty_stage_parents(&self.root);
        }
    }
}

fn record_projection_identity(staged_id: &str, source: &Path, settings_id: &str) {
    PROJECTION_IDENTITY_STACK.with(|stack| {
        if let Some(identities) = stack.borrow_mut().last_mut() {
            identities.insert(
                staged_id.to_string(),
                ProjectionIdentity {
                    source: absolute_path(source),
                    settings_id: settings_id.to_string(),
                },
            );
        }
    });
}

fn remove_empty_stage_parents(path: &Path) {
    if let Some(parent) = path.parent()
        && fs::remove_dir(parent).is_ok()
        && parent
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| matches!(name, "build-staging" | "nested-syncback"))
        && let Some(metadata) = parent.parent()
    {
        let _ = fs::remove_dir(metadata);
    }
}

pub struct FilterCandidate<'a> {
    pub id: &'a str,
    pub path: &'a str,
    pub name: &'a str,
    pub class: &'a str,
    pub tags: &'a BTreeSet<String>,
    pub attributes: &'a BTreeSet<String>,
    pub properties: &'a BTreeSet<String>,
}

pub struct FilterCandidateFields {
    pub tags: BTreeSet<String>,
    pub attributes: BTreeSet<String>,
    pub properties: BTreeSet<String>,
}

impl FilterCandidateFields {
    pub fn candidate<'a>(
        &'a self,
        id: &'a str,
        path: &'a str,
        name: &'a str,
        class: &'a str,
    ) -> FilterCandidate<'a> {
        FilterCandidate {
            id,
            path,
            name,
            class,
            tags: &self.tags,
            attributes: &self.attributes,
            properties: &self.properties,
        }
    }
}

pub fn filter_candidate_fields(
    properties: &Map<String, Value>,
    attributes: &Map<String, Value>,
) -> FilterCandidateFields {
    FilterCandidateFields {
        tags: properties
            .get("Tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        attributes: attributes.keys().cloned().collect(),
        properties: properties.keys().cloned().collect(),
    }
}

pub(crate) fn filter_path_segments(segments: &[String]) -> String {
    segments
        .iter()
        .map(|segment| segment.replace('~', "~0").replace('/', "~1"))
        .collect::<Vec<_>>()
        .join("/")
}

fn json_string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value?.as_array().map(|values| {
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

struct OwnedFilterCandidate {
    id: String,
    path: String,
    name: String,
    class: String,
    tags: BTreeSet<String>,
    attributes: BTreeSet<String>,
    properties: BTreeSet<String>,
}

impl OwnedFilterCandidate {
    fn borrowed(&self) -> FilterCandidate<'_> {
        FilterCandidate {
            id: &self.id,
            path: &self.path,
            name: &self.name,
            class: &self.class,
            tags: &self.tags,
            attributes: &self.attributes,
            properties: &self.properties,
        }
    }
}

struct ReverseOwner {
    target: Vec<String>,
    ordinals: Vec<usize>,
    source: PathBuf,
    ownership: MountOwnership,
    ignore_unknown_instances: bool,
    optional: bool,
}

struct ProjectionFieldOwner {
    target: Vec<String>,
    source: String,
    class_name: bool,
    settings_id: bool,
    properties: BTreeSet<String>,
    attributes: BTreeSet<String>,
    tags: bool,
}

struct ReverseSource {
    text: String,
    extension: String,
}

#[derive(Serialize, Deserialize, Default)]
struct AdapterBaseline {
    #[serde(default)]
    entries: BTreeMap<String, AdapterBaselineEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterBaselineEntry {
    source_hash: String,
    target_hash: String,
    format: Option<String>,
    output: Option<String>,
    output_hash: Option<String>,
    #[serde(default)]
    output_owned: bool,
    model_json_hierarchical: Option<bool>,
}

pub fn project_source_roots(loaded: &LoadedProject) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    let mut visited = BTreeSet::new();
    project_source_roots_into(loaded, &mut roots, &mut visited)?;
    Ok(roots.into_iter().collect())
}

pub fn project_config_paths(loaded: &LoadedProject) -> Result<Vec<PathBuf>> {
    fn collect(
        loaded: &LoadedProject,
        paths: &mut BTreeSet<PathBuf>,
        visited: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        let project_path =
            fs::canonicalize(&loaded.path).unwrap_or_else(|_| absolute_path(&loaded.path));
        if !visited.insert(project_path.clone()) {
            return Ok(());
        }
        paths.insert(project_path);
        for (_, node) in project_tree_nodes(&loaded.project.tree) {
            if let Some(path) = node.path {
                let path = loaded.root.join(path);
                if path.is_file() && is_nested_project_path(&path) {
                    collect(&load_nested_project(&path)?, paths, visited)?;
                }
            }
        }
        for mount in &loaded.project.mounts {
            let path = loaded.root.join(&mount.source);
            if path.is_file() && is_nested_project_path(&path) {
                collect(&load_nested_project(&path)?, paths, visited)?;
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let path = loaded.root.join(&adapter.source);
            if path.is_file() && adapter_format(adapter)? == AdapterFormat::NestedProject {
                collect(&load_nested_project(&path)?, paths, visited)?;
            }
        }
        Ok(())
    }

    let mut paths = BTreeSet::new();
    collect(loaded, &mut paths, &mut BTreeSet::new())?;
    Ok(paths.into_iter().collect())
}

fn project_source_roots_into(
    loaded: &LoadedProject,
    roots: &mut BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let project_path = absolute_path(&loaded.path);
    if !visited.insert(project_path.clone()) {
        bail!("Nested project cycle includes {}", project_path.display());
    }
    roots.insert(loaded.root.join(&loaded.project.source_root));
    let tree_sources = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(_, node)| node.path);
    let mount_sources = loaded
        .project
        .mounts
        .iter()
        .map(|mount| mount.source.clone());
    for source in tree_sources.chain(mount_sources) {
        let path = loaded.root.join(source);
        roots.insert(if path.is_file() {
            path.parent().unwrap_or(&loaded.root).to_path_buf()
        } else {
            path.clone()
        });
        if path.is_file() && is_nested_project_path(&path) {
            project_source_roots_into(&load_nested_project(&path)?, roots, visited)?;
        }
    }
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::FromProject {
            continue;
        }
        let source = loaded.root.join(&adapter.source);
        roots.insert(source.parent().unwrap_or(&loaded.root).to_path_buf());
        if source.is_file() && adapter_format(adapter)? == AdapterFormat::NestedProject {
            project_source_roots_into(&load_nested_project(&source)?, roots, visited)?;
        }
        if let Some(output) = adapter.output.as_deref() {
            let output = loaded.root.join(output);
            roots.insert(output.parent().unwrap_or(&loaded.root).to_path_buf());
        }
    }
    visited.remove(&project_path);
    Ok(())
}

fn path_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect()
}

pub fn staged_path_to_project_source(
    loaded: &LoadedProject,
    staged_relative: &Path,
) -> Result<Option<PathBuf>> {
    let segments = path_segments(staged_relative);
    let mut mappings = project_target_source_mappings(loaded)?;
    mappings.sort_by_key(|mapping| std::cmp::Reverse(mapping.target.len()));
    for mapping in mappings {
        let mut target_segments = segments.clone();
        if let Some((_, leaf, _)) = target_segments
            .last()
            .and_then(|file_name| infer_source_script(file_name, &mapping.naming))
        {
            target_segments.pop();
            if let Some(leaf) = leaf {
                target_segments.push(leaf);
            }
        }
        let target = mapping.target;
        let source = mapping.source;
        if target_segments.starts_with(&target) {
            if source.is_file() {
                if target_segments == target {
                    return Ok(Some(source));
                }
                continue;
            }
            return Ok(Some(
                source.join(segments[target.len()..].iter().collect::<PathBuf>()),
            ));
        }
    }
    Ok(Some(
        loaded
            .root
            .join(&loaded.project.source_root)
            .join(staged_relative),
    ))
}

pub struct ProjectWriteResolution {
    pub path: PathBuf,
    pub source_root: PathBuf,
    pub owner: &'static str,
    pub consumed_segments: usize,
    pub naming: ProjectScriptNaming,
}

pub fn resolve_project_write_path(
    loaded: &LoadedProject,
    staged_relative: &Path,
) -> Result<ProjectWriteResolution> {
    let segments = path_segments(staged_relative);
    resolve_project_write_segments_inner(loaded, &segments, false)
}

pub fn resolve_project_write_segments(
    loaded: &LoadedProject,
    segments: &[String],
) -> Result<ProjectWriteResolution> {
    resolve_project_write_segments_inner(loaded, segments, false)
}

fn resolve_project_write_segments_inner(
    loaded: &LoadedProject,
    segments: &[String],
    mounted_root: bool,
) -> Result<ProjectWriteResolution> {
    let mut candidates = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        if let Some(source) = node.path
            && segments.starts_with(&target)
        {
            let source_root = loaded.root.join(source);
            candidates.push((
                target.len(),
                "tree",
                source_root.join(segments[target.len()..].iter().collect::<PathBuf>()),
                source_root,
                true,
            ));
        }
    }
    for mount in &loaded.project.mounts {
        let target = target_segments(&mount.target)?;
        if segments.starts_with(&target) {
            let source_root = loaded.root.join(&mount.source);
            candidates.push((
                target.len(),
                "mount",
                source_root.join(segments[target.len()..].iter().collect::<PathBuf>()),
                loaded.root.join(&mount.source),
                mount.ownership != MountOwnership::ReadOnly,
            ));
        }
    }
    for adapter in &loaded.project.adapters {
        let target = target_segments(&adapter.target)?;
        if segments.starts_with(&target) {
            let source = loaded.root.join(&adapter.source);
            let writable =
                adapter.direction != AdapterDirection::ToProject && is_nested_project_path(&source);
            candidates.push((target.len(), "adapter", source.clone(), source, writable));
        }
    }
    let longest = candidates.iter().map(|candidate| candidate.0).max();
    if let Some(longest) = longest {
        let mut matches = candidates
            .into_iter()
            .filter(|candidate| candidate.0 == longest);
        let (_, owner, path, source_root, writable) =
            matches.next().expect("longest owner candidate disappeared");
        if matches.next().is_some() {
            bail!(
                "Projected path '{}' has more than one equally specific owner",
                segments.join("/")
            );
        }
        if !writable {
            bail!(
                "Projected path '{}' is owned by a non-writable {owner}",
                segments.join("/")
            );
        }
        if source_root.is_file() && segments.len() > longest {
            if is_nested_project_path(&source_root) {
                let nested = load_nested_project(&source_root)?;
                let mut resolved =
                    resolve_project_write_segments_inner(&nested, &segments[longest..], true)?;
                resolved.consumed_segments += longest;
                return Ok(resolved);
            }
            bail!(
                "Projected path '{}' descends through file owner {}",
                segments.join("/"),
                path.display()
            );
        }
        return Ok(ProjectWriteResolution {
            path,
            source_root,
            owner,
            consumed_segments: longest,
            naming: project_script_naming(&loaded.project),
        });
    }
    let source_root = loaded.root.join(&loaded.project.source_root);
    if mounted_root {
        return Ok(ProjectWriteResolution {
            path: source_root.join(segments.iter().collect::<PathBuf>()),
            source_root,
            owner: "sourceRoot",
            consumed_segments: 0,
            naming: project_script_naming(&loaded.project),
        });
    }
    let service_root = segments
        .first()
        .map_or_else(|| source_root.clone(), |service| source_root.join(service));
    Ok(ProjectWriteResolution {
        path: source_root.join(segments.iter().collect::<PathBuf>()),
        source_root: service_root,
        owner: "sourceRoot",
        consumed_segments: usize::from(!segments.is_empty()),
        naming: project_script_naming(&loaded.project),
    })
}

pub fn project_staged_path_to_source(
    loaded: &LoadedProject,
    staged_relative: &Path,
) -> Result<PathBuf> {
    let segments = path_segments(staged_relative);
    let mut candidates = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        if let Some(source) = node.path
            && segments.starts_with(&target)
        {
            candidates.push((
                target.len(),
                loaded
                    .root
                    .join(source)
                    .join(segments[target.len()..].iter().collect::<PathBuf>()),
            ));
        }
    }
    for mount in &loaded.project.mounts {
        let target = target_segments(&mount.target)?;
        if segments.starts_with(&target) {
            candidates.push((
                target.len(),
                loaded
                    .root
                    .join(&mount.source)
                    .join(segments[target.len()..].iter().collect::<PathBuf>()),
            ));
        }
    }
    for adapter in &loaded.project.adapters {
        let target = target_segments(&adapter.target)?;
        if segments.starts_with(&target) {
            candidates.push((target.len(), loaded.root.join(&adapter.source)));
        }
    }
    let longest = candidates.iter().map(|candidate| candidate.0).max();
    let mut selected = candidates
        .into_iter()
        .filter(|candidate| Some(candidate.0) == longest)
        .map(|candidate| candidate.1)
        .collect::<Vec<_>>();
    selected.sort();
    selected.dedup();
    if selected.len() > 1 {
        bail!(
            "Projected path '{}' has more than one equally specific source",
            path_slash(staged_relative)
        );
    }
    Ok(selected.pop().unwrap_or_else(|| {
        loaded
            .root
            .join(&loaded.project.source_root)
            .join(staged_relative)
    }))
}

pub fn project_source_to_staged_paths(
    loaded: &LoadedProject,
    source: &Path,
    stage_root: &Path,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for relative in project_source_to_staged_relatives(loaded, source)? {
        let mapped = stage_root.join(&relative);
        if mapped.is_file() {
            paths.push(mapped);
        } else if source.is_file() {
            let copied = file_target_destination(loaded, source, &mapped);
            if copied.is_file() {
                paths.push(copied);
            }
        }
        if let Some(Component::Normal(service)) = relative.components().next() {
            let settings = service_settings_path(&stage_root.join(service));
            if settings.is_file() {
                paths.push(settings);
            }
        }
    }
    collect_generated_adapter_staged_paths(
        loaded,
        source,
        stage_root,
        &[],
        &mut BTreeSet::new(),
        &mut paths,
    )?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn collect_generated_adapter_staged_paths(
    loaded: &LoadedProject,
    source: &Path,
    stage_root: &Path,
    prefix: &[String],
    visiting: &mut BTreeSet<PathBuf>,
    output: &mut Vec<PathBuf>,
) -> Result<()> {
    let project_path = fs::canonicalize(&loaded.path)
        .with_context(|| format!("Failed to resolve {}", loaded.path.display()))?;
    if !visiting.insert(project_path.clone()) {
        bail!("Nested project cycle includes {}", loaded.path.display());
    }
    let source = absolute_path(source);
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::FromProject {
            continue;
        }
        let adapter_source = absolute_path(&loaded.root.join(&adapter.source));
        let format = adapter_format(adapter)?;
        if adapter_source == source && format.generates_module() {
            let mut target = prefix.to_vec();
            target.extend(target_segments(&adapter.target)?);
            let path = adapter_target_script_path(loaded, stage_root, &target);
            if path.is_file() {
                output.push(path);
            }
        }
    }
    for (target, nested) in nested_project_targets(loaded, prefix)? {
        collect_generated_adapter_staged_paths(
            &nested, &source, stage_root, &target, visiting, output,
        )?;
    }
    visiting.remove(&project_path);
    Ok(())
}

fn project_source_to_staged_relative(
    loaded: &LoadedProject,
    source: &Path,
) -> Result<Option<PathBuf>> {
    Ok(project_source_to_staged_relatives(loaded, source)?
        .into_iter()
        .next())
}

pub fn project_source_to_staged_relatives(
    loaded: &LoadedProject,
    source: &Path,
) -> Result<Vec<PathBuf>> {
    let source = absolute_path(source);
    let matches = project_target_source_mappings(loaded)?
        .into_iter()
        .filter_map(|mapping| {
            let owner = absolute_path(&mapping.source);
            let relative = if source == owner {
                PathBuf::new()
            } else {
                source.strip_prefix(&owner).ok()?.to_path_buf()
            };
            Some((
                owner.components().count(),
                mapping.target.iter().collect::<PathBuf>().join(relative),
            ))
        })
        .collect::<Vec<_>>();
    if let Some(longest) = matches.iter().map(|(specificity, _)| *specificity).max() {
        let mut mapped = matches
            .into_iter()
            .filter_map(|(specificity, path)| (specificity == longest).then_some(path))
            .collect::<Vec<_>>();
        mapped.sort();
        mapped.dedup();
        return Ok(mapped);
    }
    let owner = absolute_path(&loaded.root.join(&loaded.project.source_root));
    Ok(source
        .strip_prefix(owner)
        .ok()
        .map(Path::to_path_buf)
        .into_iter()
        .collect())
}

fn source_owner_relative_path(loaded: &LoadedProject, source: &Path) -> Option<PathBuf> {
    let source = absolute_path(source);
    let mut roots = vec![absolute_path(
        &loaded.root.join(&loaded.project.source_root),
    )];
    roots.extend(
        project_tree_nodes(&loaded.project.tree)
            .into_iter()
            .filter_map(|(_, node)| node.path.map(|path| absolute_path(&loaded.root.join(path)))),
    );
    roots.extend(
        loaded
            .project
            .mounts
            .iter()
            .map(|mount| absolute_path(&loaded.root.join(&mount.source))),
    );
    roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    roots.into_iter().find_map(|root| {
        if root.is_file() {
            (source == root).then(|| root.file_name().map(PathBuf::from).unwrap_or_default())
        } else {
            source.strip_prefix(root).ok().map(Path::to_path_buf)
        }
    })
}

struct ProjectTargetSourceMapping {
    target: Vec<String>,
    source: PathBuf,
    naming: ProjectScriptNaming,
}

fn project_target_source_mappings(
    loaded: &LoadedProject,
) -> Result<Vec<ProjectTargetSourceMapping>> {
    fn append(
        loaded: &LoadedProject,
        prefix: &[String],
        include_source_root: bool,
        visiting: &mut BTreeSet<PathBuf>,
        mappings: &mut Vec<ProjectTargetSourceMapping>,
    ) -> Result<()> {
        let project_path = fs::canonicalize(&loaded.path)
            .with_context(|| format!("Failed to resolve {}", loaded.path.display()))?;
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        if include_source_root {
            mappings.push(ProjectTargetSourceMapping {
                target: prefix.to_vec(),
                source: loaded.root.join(&loaded.project.source_root),
                naming: project_script_naming(&loaded.project),
            });
        }
        let mut add = |target: Vec<String>, source: PathBuf, nested: bool| -> Result<()> {
            if nested {
                let project = load_nested_project(&source)?;
                append(&project, &target, true, visiting, mappings)
            } else {
                mappings.push(ProjectTargetSourceMapping {
                    target,
                    source,
                    naming: project_script_naming(&loaded.project),
                });
                Ok(())
            }
        };
        for (target, node) in project_tree_nodes(&loaded.project.tree) {
            let Some(source) = node.path.as_deref() else {
                continue;
            };
            let source = loaded.root.join(source);
            let mut target_with_prefix = prefix.to_vec();
            target_with_prefix.extend(target);
            add(
                target_with_prefix,
                source.clone(),
                is_nested_project_path(&source),
            )?;
        }
        for mount in &loaded.project.mounts {
            let source = loaded.root.join(&mount.source);
            let mut target = prefix.to_vec();
            target.extend(target_segments(&mount.target)?);
            add(target, source.clone(), is_nested_project_path(&source))?;
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let source = loaded.root.join(&adapter.source);
            let mut target = prefix.to_vec();
            target.extend(target_segments(&adapter.target)?);
            add(
                target,
                source,
                adapter_format(adapter)? == AdapterFormat::NestedProject,
            )?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut mappings = Vec::new();
    append(loaded, &[], false, &mut BTreeSet::new(), &mut mappings)?;
    Ok(mappings)
}

pub(crate) fn project_tree_nodes(
    tree: &BTreeMap<String, ProjectNode>,
) -> Vec<(Vec<String>, ProjectNode)> {
    fn visit(
        target: &mut Vec<String>,
        node: &ProjectNode,
        output: &mut Vec<(Vec<String>, ProjectNode)>,
    ) {
        output.push((target.clone(), node.clone()));
        for (name, value) in &node.children {
            if name.starts_with('$') {
                continue;
            }
            if let Ok(child) = serde_json::from_value::<ProjectNode>(value.clone()) {
                target.push(name.clone());
                visit(target, &child, output);
                target.pop();
            }
        }
    }
    let mut output = Vec::new();
    for (name, node) in tree {
        let mut target = vec![name.clone()];
        visit(&mut target, node, &mut output);
    }
    output
}

pub fn run_fmt_project(args: FmtProjectArgs, global_project: Option<&Path>) -> Result<()> {
    let project_path = resolve_project_path(args.project.as_deref().or(global_project), None)?;
    let text = fs::read_to_string(&project_path)
        .with_context(|| format!("Failed to read {}", project_path.display()))?;
    let value = parse_jsonc_value(&text)
        .with_context(|| format!("Invalid JSONC in {}", project_path.display()))?;
    let formatted = if text.contains("//") || text.contains("/*") {
        let formatted = format_jsonc(&text)?;
        if parse_jsonc_value(&formatted)? != value {
            bail!(
                "Formatting {} would change its parsed value",
                project_path.display()
            );
        }
        formatted
    } else {
        serde_json::to_string_pretty(&value)? + "\n"
    };
    if args.check {
        if text != formatted {
            bail!("{} is not formatted", project_path.display());
        }
        return crate::app::output::emit_global_output(
            &json!({ "ok": true, "path": project_path, "formatted": true }),
            &format!("{} is formatted", project_path.display()),
        );
    }
    atomic_write_file(&project_path, formatted.as_bytes())?;
    crate::app::output::emit_global_output(
        &json!({ "ok": true, "path": project_path, "formatted": true }),
        &format!("Formatted {}", project_path.display()),
    )
}

pub fn run_explain_path(args: ExplainPathArgs, global_project: Option<&Path>) -> Result<()> {
    let loaded = load_project(args.project.as_deref().or(global_project), None)?;
    validate_project(&loaded)?;
    let absolute = if args.path.is_absolute() {
        args.path
    } else {
        loaded.root.join(args.path)
    };
    let relative = absolute
        .strip_prefix(&loaded.root)
        .with_context(|| {
            format!(
                "{} is outside project {}",
                absolute.display(),
                loaded.root.display()
            )
        })?
        .to_path_buf();
    let relative_text = path_slash(&relative);
    let mut matches = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        if let Some(source) = node.path.as_deref() {
            let source = path_slash(source);
            if relative_text == source || relative_text.starts_with(&(source.clone() + "/")) {
                matches.push(json!({
                    "kind": "tree",
                    "source": source,
                    "target": target.join("."),
                }));
            }
        }
    }
    for mount in &loaded.project.mounts {
        let source = path_slash(&mount.source);
        if relative_text == source || relative_text.starts_with(&(source.clone() + "/")) {
            matches.push(json!({
                "kind": "mount",
                "source": source,
                "target": mount.target,
                "ownership": mount.ownership,
            }));
        }
    }
    for adapter in &loaded.project.adapters {
        let source = path_slash(&adapter.source);
        let output = adapter.output.as_deref().map(path_slash);
        if relative_text == source || output.as_deref() == Some(relative_text.as_str()) {
            matches.push(json!({
                "kind": "adapter",
                "source": source,
                "output": output,
                "target": adapter.target,
                "direction": adapter.direction,
            }));
        }
    }
    let rule_relative = source_owner_relative_path(&loaded, &absolute).unwrap_or(relative);
    let selected_rule = loaded
        .project
        .sync_rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            sync_rule_matches(rule, &rule_relative)
                .ok()?
                .then_some(index)
        })
        .next_back();
    for (index, rule) in loaded.project.sync_rules.iter().enumerate() {
        if sync_rule_matches(rule, &rule_relative)? {
            matches.push(json!({
                "kind": "syncRule",
                "index": index,
                "selected": selected_rule == Some(index),
                "pattern": rule.pattern,
                "exclude": rule.exclude,
                "use": rule.middleware,
                "suffix": rule.suffix,
                "targetName": sync_rule_instance_name(rule, &rule_relative)?,
            }));
        }
    }
    let ignored_by_path = path_is_ignored(&loaded, &absolute)?;
    let projection = stage_project(&loaded)?;
    let staged_absolute = project_source_to_staged_paths(&loaded, &absolute, projection.root())?;
    let staged_paths = staged_absolute
        .iter()
        .map(|path| {
            path.strip_prefix(projection.root())
                .map_or_else(|_| path_slash(path), path_slash)
        })
        .collect::<Vec<_>>();
    let staged_absolute = staged_absolute
        .into_iter()
        .map(|path| absolute_path(&path))
        .collect::<HashSet<_>>();
    let sidecar_target = if is_metadata_sidecar(&absolute) {
        project_source_to_staged_relative(&loaded, &absolute)?
            .map(|relative| metadata_sidecar_target(&loaded, &relative))
            .transpose()?
    } else {
        None
    };
    let mut filter_candidates = Vec::new();
    let mut matching_filters = Vec::new();
    for service in fs::read_dir(projection.root())? {
        let service = service?;
        if !service.file_type()?.is_dir() {
            continue;
        }
        let service_name = service.file_name().to_string_lossy().into_owned();
        let settings = service_settings_path(&service.path());
        if !settings.is_file() {
            continue;
        }
        let document = SettingsBytecode::read_file(&settings)?;
        let source_paths = crate::editor::paths::build_editor_source_paths_by_index(
            &document,
            &service_name,
            &service.path(),
        );
        let paths = projection_instance_paths(&document);
        for (index, source_path) in source_paths.into_iter().enumerate() {
            let source_matches = source_path
                .as_deref()
                .is_some_and(|source_path| staged_absolute.contains(&absolute_path(source_path)));
            if !source_matches && sidecar_target.as_ref() != Some(&paths[index]) {
                continue;
            }
            let instance = &document.instances[index];
            let fields = filter_candidate_fields(&instance.properties, &instance.attributes);
            let candidate_path = filter_path_segments(&paths[index]);
            let candidate = fields.candidate(
                &instance.settings_id,
                &candidate_path,
                &instance.name,
                &instance.class_name,
            );
            let mut candidate_rules = Vec::new();
            for (rule_index, rule) in loaded.project.filters.iter().enumerate() {
                if filter_matches(rule, &candidate, FilterScope::Any)? {
                    let entry = json!({
                        "index": rule_index,
                        "action": rule.action,
                        "direction": rule.direction,
                        "glob": rule.glob,
                        "candidatePath": candidate_path,
                    });
                    candidate_rules.push(entry.clone());
                    matching_filters.push(entry);
                }
            }
            filter_candidates.push(json!({
                "path": candidate_path,
                "id": instance.settings_id,
                "filesToStudio": if filter_allows_instance(
                    &loaded.project.filters,
                    FilterDirection::FilesToStudio,
                    &candidate,
                )? { "include" } else { "ignore" },
                "studioToFiles": if filter_allows_instance(
                    &loaded.project.filters,
                    FilterDirection::StudioToFiles,
                    &candidate,
                )? { "include" } else { "ignore" },
                "matchingFilters": candidate_rules,
            }));
        }
    }
    let result = json!({
        "ok": true,
        "project": loaded.path,
        "path": relative_text,
        "matches": matches,
        "filters": matching_filters,
        "filterCandidates": filter_candidates,
        "selectedSyncRule": selected_rule,
        "stagedPaths": staged_paths,
        "ignored": ignored_by_path,
        "owned": !matches.is_empty() && !ignored_by_path,
    });
    print_json(&result, args.pretty)
}

pub fn run_config(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommand::Get(args) => {
            let value = load_config_scope(args.scope, &args.root)?;
            let selected = match args.key.as_deref() {
                Some(key) => get_dotted(&value, key)
                    .cloned()
                    .with_context(|| format!("Configuration key '{key}' is not set"))?,
                None => value,
            };
            print_json(&selected, true)
        }
        ConfigCommand::Set(args) => {
            ensure_writable_scope(args.scope)?;
            let path = config_scope_path(args.scope, &args.root)?;
            let mut value = read_json_object_or_empty(&path)?;
            let parsed = if args.string {
                Value::String(args.value)
            } else {
                serde_json::from_str(&args.value).unwrap_or(Value::String(args.value))
            };
            set_dotted(&mut value, &args.key, parsed)?;
            validate_config_scope_change(args.scope, &args.root, &path, &value)?;
            write_json(&path, &value)?;
            crate::app::output::emit_global_output(
                &json!({ "ok": true, "action": "set", "key": args.key, "path": path }),
                &format!("Set {} in {}", args.key, path.display()),
            )
        }
        ConfigCommand::Unset(args) => {
            ensure_writable_scope(args.scope)?;
            let path = config_scope_path(args.scope, &args.root)?;
            let mut value = read_json_object_or_empty(&path)?;
            if !remove_dotted(&mut value, &args.key)? {
                bail!(
                    "Configuration key '{}' is not set in {}",
                    args.key,
                    path.display()
                );
            }
            validate_config_scope_change(args.scope, &args.root, &path, &value)?;
            write_json(&path, &value)?;
            crate::app::output::emit_global_output(
                &json!({ "ok": true, "action": "unset", "key": args.key, "path": path }),
                &format!("Removed {} from {}", args.key, path.display()),
            )
        }
        ConfigCommand::Reset(args) => {
            ensure_writable_scope(args.scope)?;
            let path = config_scope_path(args.scope, &args.root)?;
            validate_config_scope_change(args.scope, &args.root, &path, &json!({}))?;
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
            crate::app::output::emit_global_output(
                &json!({ "ok": true, "action": "reset", "path": path }),
                &format!("Reset {}", path.display()),
            )
        }
        ConfigCommand::List(args) => {
            if args.origins {
                let result = config_with_origins(&args.root)?;
                print_json(&result, true)
            } else {
                print_json(&load_config_scope(args.scope, &args.root)?, true)
            }
        }
        ConfigCommand::Edit(args) => {
            ensure_writable_scope(args.scope)?;
            let path = config_scope_path(args.scope, &args.root)?;
            if !path.exists() {
                write_json(&path, &json!({ "schemaVersion": 1 }))?;
            }
            open_in_editor(&path)
        }
        ConfigCommand::Path(args) => {
            ensure_writable_scope(args.scope)?;
            let path = config_scope_path(args.scope, &args.root)?;
            crate::app::output::emit_global_output(
                &json!({ "ok": true, "path": path }),
                &path.display().to_string(),
            )
        }
        ConfigCommand::Export(args) => {
            let value = load_merged_config(&args.root)?;
            let text = serde_json::to_string_pretty(&value)? + "\n";
            if let Some(output) = args.output {
                atomic_write_file(&output, text.as_bytes())?;
                crate::app::output::emit_global_output(
                    &json!({ "ok": true, "path": output }),
                    &format!("Wrote {}", output.display()),
                )
            } else {
                print!("{text}");
                Ok(())
            }
        }
    }
}

pub fn run_adapters(args: AdaptersArgs, global_project: Option<&Path>) -> Result<()> {
    match args.command {
        AdaptersCommand::Validate(args) => {
            let loaded = load_project(args.project.as_deref().or(global_project), None)?;
            validate_project(&loaded)?;
            let projection = compile_projection(&loaded);
            crate::app::output::emit_global_output(
                &json!({
                    "ok": true,
                    "entries": projection.entries.len(),
                    "adapters": loaded.project.adapters.len(),
                    "mounts": loaded.project.mounts.len(),
                    "project": loaded.path,
                }),
                &format!(
                    "Validated {} adapters and {} mounts in {}",
                    loaded.project.adapters.len(),
                    loaded.project.mounts.len(),
                    loaded.path.display()
                ),
            )
        }
        AdaptersCommand::Build(args) => {
            let loaded = load_project(args.project.as_deref().or(global_project), None)?;
            validate_project(&loaded)?;
            build_adapters(&loaded, args.check, true)
        }
        AdaptersCommand::Syncback(args) => {
            let loaded = load_project(args.project.as_deref().or(global_project), None)?;
            validate_project(&loaded)?;
            if args.preview {
                let projection = stage_adapter_syncback_projection(&loaded)?;
                let plan = plan_adapter_syncback(&loaded, projection.root())?;
                let mut operations = plan
                    .writes
                    .iter()
                    .map(|(path, _)| {
                        json!({
                            "action": "write",
                            "path": path,
                            "kind": "adapter-source",
                        })
                    })
                    .collect::<Vec<_>>();
                if plan.baseline_changed {
                    operations.push(json!({
                        "action": "write",
                        "path": plan.baseline_path,
                        "kind": "adapter-baseline",
                    }));
                }
                print_json(
                    &json!({
                        "ok": true,
                        "project": loaded.path,
                        "operations": operations,
                    }),
                    true,
                )?;
                return Ok(());
            }
            let changed = syncback_project_adapters(&loaded, args.check)?;
            crate::app::output::emit_global_output(
                &json!({
                    "ok": true,
                    "checked": args.check,
                    "changed": changed,
                    "project": loaded.path,
                }),
                &format!(
                    "{} {changed} adapter source{}",
                    if args.check { "Checked" } else { "Updated" },
                    if changed == 1 { "" } else { "s" }
                ),
            )
        }
        AdaptersCommand::Watch(args) => {
            let loaded = load_project(args.project.as_deref().or(global_project), None)?;
            validate_project(&loaded)?;
            watch_adapters(&loaded, args.interval_ms)
        }
    }
}

pub fn run_import_rojo(args: ImportRojoArgs) -> Result<()> {
    let source = if args.project.is_dir() {
        let candidates = rojo_project_files(&args.project)?;
        match candidates.as_slice() {
            [only] => only.clone(),
            [] => bail!(
                "No *.project.json file exists in {}",
                args.project.display()
            ),
            many => bail!(
                "Multiple Rojo projects exist in {}: {}",
                args.project.display(),
                many.iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    } else {
        args.project
    };
    let converted = convert_rojo_project(&source)?;
    let value = serde_json::to_value(&converted)?;
    let text = serde_json::to_string_pretty(&value)? + "\n";
    if !args.apply || args.preview {
        print!("{text}");
        return Ok(());
    }
    let output = args.output.unwrap_or_else(|| {
        source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(PROJECT_FILE_NAME)
    });
    if output.exists() && !args.force {
        bail!(
            "{} already exists; use --force to replace it",
            output.display()
        );
    }
    atomic_write_file(&output, text.as_bytes())?;
    crate::app::output::emit_global_output(
        &json!({
            "ok": true,
            "source": source,
            "output": output,
        }),
        &format!("Imported {} into {}", source.display(), output.display()),
    )
}

pub fn load_project(explicit: Option<&Path>, start: Option<&Path>) -> Result<LoadedProject> {
    try_load_project(explicit, start)?.with_context(|| {
        format!(
            "No {PROJECT_FILE_NAME} was found from the current directory upward; pass --project"
        )
    })
}

pub fn try_load_project(
    explicit: Option<&Path>,
    start: Option<&Path>,
) -> Result<Option<LoadedProject>> {
    let Some(path) = try_resolve_project_path(explicit, start)? else {
        return Ok(None);
    };
    let project = load_project_schema(&path)?;
    let root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    Ok(Some(LoadedProject {
        path,
        root,
        project,
    }))
}

fn load_nested_project(path: &Path) -> Result<LoadedProject> {
    let project = load_project_schema(path)?;
    let root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    Ok(LoadedProject {
        path: path.to_path_buf(),
        root,
        project,
    })
}

fn load_project_schema(path: &Path) -> Result<ReniumProject> {
    let text =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let value =
        parse_jsonc_value(&text).with_context(|| format!("Invalid JSONC in {}", path.display()))?;
    let project = if value.get("schemaVersion").is_some() {
        serde_json::from_value(value)
            .with_context(|| format!("Invalid Renium project schema in {}", path.display()))?
    } else {
        convert_rojo_project(path)
            .with_context(|| format!("Invalid Rojo project schema in {}", path.display()))?
    };
    Ok(project)
}

pub fn refresh_script_naming(root: &Path) -> Result<()> {
    let naming = match try_load_project(None, Some(root))? {
        Some(loaded) => project_script_naming(&loaded.project),
        None => ProjectScriptNaming::default(),
    };
    let cache = SCRIPT_NAMING_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(absolute_path(root), naming);
    Ok(())
}

pub fn cache_script_naming(root: &Path, project: &ReniumProject) {
    let naming = project_script_naming(project);
    SCRIPT_NAMING_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(absolute_path(root), naming);
}

fn remove_cached_script_naming(root: &Path) {
    if let Some(cache) = SCRIPT_NAMING_CACHE.get() {
        let root = absolute_path(root);
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|path, _| !path.starts_with(&root));
    }
}

fn relocate_cached_script_naming(source: &Path, destination: &Path) {
    let source = absolute_path(source);
    let destination = absolute_path(destination);
    let cache = SCRIPT_NAMING_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let moved = cache
        .iter()
        .filter_map(|(path, naming)| {
            path.strip_prefix(&source)
                .ok()
                .map(|relative| (destination.join(relative), naming.clone()))
        })
        .collect::<Vec<_>>();
    cache.retain(|path, _| !path.starts_with(&source));
    cache.extend(moved);
}

pub fn project_script_naming(project: &ReniumProject) -> ProjectScriptNaming {
    ProjectScriptNaming::from_export(project.script_extension, project.export_naming.clone())
}

pub fn cached_script_naming(root: &Path) -> ProjectScriptNaming {
    let root = absolute_path(root);
    SCRIPT_NAMING_CACHE
        .get()
        .and_then(|cache| {
            cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|(path, _)| root.starts_with(path))
                .max_by_key(|(path, _)| path.components().count())
                .map(|(_, naming)| naming.clone())
        })
        .unwrap_or_default()
}

pub fn resolve_project_path(explicit: Option<&Path>, start: Option<&Path>) -> Result<PathBuf> {
    try_resolve_project_path(explicit, start)?.with_context(|| {
        format!(
            "No {PROJECT_FILE_NAME} was found from the current directory upward; pass --project"
        )
    })
}

pub fn try_resolve_project_path(
    explicit: Option<&Path>,
    start: Option<&Path>,
) -> Result<Option<PathBuf>> {
    if let Some(explicit) = explicit {
        let path = if explicit.is_dir() {
            project_file_in_directory(explicit)?
        } else {
            explicit.to_path_buf()
        };
        if !path.is_file() {
            bail!("Project file does not exist: {}", path.display());
        }
        return Ok(Some(path));
    }
    let start = match start {
        Some(path) => path.to_path_buf(),
        None => env::current_dir().context("Failed to read the current directory")?,
    };
    let mut current = if start.is_file() {
        start
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        start
    };
    loop {
        let has_renium_project = current.join(PROJECT_FILE_NAME).is_file()
            || current.join(PROJECT_JSON_FILE_NAME).is_file();
        if has_renium_project || !rojo_project_files(&current)?.is_empty() {
            return project_file_in_directory(&current).map(Some);
        }
        if !current.pop() {
            break;
        }
    }
    Ok(None)
}

fn project_file_in_directory(directory: &Path) -> Result<PathBuf> {
    let jsonc = directory.join(PROJECT_FILE_NAME);
    let json = directory.join(PROJECT_JSON_FILE_NAME);
    match (jsonc.is_file(), json.is_file()) {
        (true, false) => Ok(jsonc),
        (false, true) => Ok(json),
        (true, true) => bail!(
            "{} contains both {} and {}; keep one project file",
            directory.display(),
            PROJECT_FILE_NAME,
            PROJECT_JSON_FILE_NAME
        ),
        (false, false) => match rojo_project_files(directory)?.as_slice() {
            [only] => Ok(only.clone()),
            [] => bail!(
                "No Renium or Rojo project exists in {}",
                directory.display()
            ),
            many => bail!(
                "Multiple Rojo projects exist in {}: {}",
                directory.display(),
                many.iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

fn rojo_project_files(directory: &Path) -> Result<Vec<PathBuf>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut projects = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                ends_with_ignore_ascii_case(name, ".project.json")
                    && !name.eq_ignore_ascii_case(PROJECT_JSON_FILE_NAME)
            })
        {
            projects.push(path);
        }
    }
    projects.sort();
    Ok(projects)
}

pub fn load_merged_config(root: &Path) -> Result<Value> {
    load_merged_config_with_override(root, None)
}

fn load_merged_config_with_override(
    root: &Path,
    scope_override: Option<(&Path, &Value)>,
) -> Result<Value> {
    let mut merged = json!({ "schemaVersion": 1 });
    for scope in [
        ConfigScope::User,
        ConfigScope::Workspace,
        ConfigScope::Experience,
        ConfigScope::Place,
    ] {
        let path = config_scope_path(scope, root)?;
        if let Some((override_path, override_value)) = scope_override
            && absolute_path(override_path) == absolute_path(&path)
        {
            merge_json(&mut merged, override_value.clone());
        } else if path.is_file() {
            merge_json(&mut merged, read_json_object_or_empty(&path)?);
        }
    }
    if nearest_project_marker(root).is_some() {
        let project = load_project(None, Some(root))?;
        if project.project.settings.is_object() {
            merge_json(&mut merged, project.project.settings);
        }
    }
    validate_merged_config(&merged)?;
    Ok(merged)
}

fn validate_config_scope_change(
    scope: ConfigScope,
    root: &Path,
    path: &Path,
    value: &Value,
) -> Result<()> {
    if scope == ConfigScope::Merged {
        bail!("Merged configuration has no writable path");
    }
    load_merged_config_with_override(root, Some((path, value))).map(drop)
}

fn validate_merged_config(value: &Value) -> Result<()> {
    fn require_kind(path: &str, expected: &str, valid: bool) -> Result<()> {
        if !valid {
            bail!("Configuration key '{path}' must be {expected}");
        }
        Ok(())
    }

    fn visit(prefix: &str, value: &Value) -> Result<()> {
        let object = value
            .as_object()
            .with_context(|| format!("Configuration key '{prefix}' must be an object"))?;
        for (key, value) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match path.as_str() {
                "gitSync" | "wallySync" | "liveSync" | "link" => visit(&path, value)?,
                "schemaVersion" => require_kind(&path, "the integer 1", value.as_u64() == Some(1))?,
                "services" | "gitSync.stagePaths" => require_kind(
                    &path,
                    "an array of strings",
                    value
                        .as_array()
                        .is_some_and(|values| values.iter().all(Value::is_string)),
                )?,
                "sourceWorkers"
                | "instanceWorkers"
                | "importWorkers"
                | "chunkSize"
                | "autoSyncDebounceMs"
                | "studioLiveSyncPollMs"
                | "liveSync.changesThreshold"
                | "liveSync.diffLinesLimit" => require_kind(
                    &path,
                    "an integer",
                    value.as_i64().is_some() || value.as_u64().is_some(),
                )?,
                "bridgeWaitSeconds" | "progressHeartbeatSeconds" | "gitSync.timeoutSeconds" => {
                    require_kind(&path, "a number", value.is_number())?
                }
                "yes"
                | "backtrace"
                | "verifyEditorPushSources"
                | "adaptiveThrottle"
                | "autoSyncOnSave"
                | "editorLiveSyncEnabled"
                | "studioLiveSyncEnabled"
                | "liveSync.overridePackages"
                | "runImport"
                | "modifiedDefaultBypass"
                | "gitSync.autoFetch"
                | "gitSync.includeUntracked"
                | "gitSync.confirmBeforePush"
                | "gitSync.requireCleanWorktreeBeforePull"
                | "wallySync.runInstall"
                | "link.offline"
                | "link.autoApplyOnManifestChange" => {
                    require_kind(&path, "a boolean", value.is_boolean())?
                }
                "importMode" => require_kind(
                    &path,
                    "'direct' or 'snapshot'",
                    matches!(value.as_str(), Some("direct" | "snapshot")),
                )?,
                "performanceMode" => require_kind(
                    &path,
                    "'throughput', 'balanced', or 'smooth'",
                    matches!(value.as_str(), Some("throughput" | "balanced" | "smooth")),
                )?,
                "logLevel" => require_kind(
                    &path,
                    "off, error, warn, info, debug, or trace",
                    matches!(
                        value.as_str(),
                        Some("off" | "error" | "warn" | "info" | "debug" | "trace")
                    ),
                )?,
                "color" => require_kind(
                    &path,
                    "auto, always, or never",
                    matches!(value.as_str(), Some("auto" | "always" | "never")),
                )?,
                "outputMode" => require_kind(
                    &path,
                    "text, json, or pretty",
                    matches!(value.as_str(), Some("text" | "json" | "pretty")),
                )?,
                "liveSync.initialSyncPriority" => require_kind(
                    &path,
                    "studio, editor, or none",
                    matches!(value.as_str(), Some("studio" | "editor" | "none")),
                )?,
                "liveSync.displayPrompts" => require_kind(
                    &path,
                    "always, initial, or never",
                    matches!(value.as_str(), Some("always" | "initial" | "never")),
                )?,
                "liveSync.conflictResolution" => require_kind(
                    &path,
                    "prompt, filesystem, or studio",
                    matches!(value.as_str(), Some("prompt" | "filesystem" | "studio")),
                )?,
                "gitSync.pullFromStudioBeforePush"
                | "gitSync.applyPulledChangesToStudio"
                | "wallySync.applyToStudio"
                | "link.applyToStudio" => require_kind(
                    &path,
                    "ask, always, or never",
                    matches!(value.as_str(), Some("ask" | "always" | "never")),
                )?,
                "gitSync.stageMode" => require_kind(
                    &path,
                    "tracked or configuredPaths",
                    matches!(value.as_str(), Some("tracked" | "configuredPaths")),
                )?,
                "gitSync.outputBehavior" => require_kind(
                    &path,
                    "onStart, onError, or silent",
                    matches!(value.as_str(), Some("onStart" | "onError" | "silent")),
                )?,
                "projectRoot"
                | "snapshotDir"
                | "cliPath"
                | "bridgePorts"
                | "place"
                | "daemon"
                | "gitSync.gitPath"
                | "gitSync.remote"
                | "gitSync.branch"
                | "gitSync.commitMessageTemplate"
                | "wallySync.wallyPath"
                | "wallySync.packagesDir"
                | "wallySync.targetService"
                | "wallySync.targetName"
                | "wallySync.serverPackagesDir"
                | "wallySync.serverTargetService"
                | "wallySync.serverTargetName"
                | "wallySync.devPackagesDir"
                | "wallySync.devTargetService"
                | "wallySync.devTargetName"
                | "wallySync.realms"
                | "link.manifest"
                | "link.folder"
                | "link.cacheDir"
                | "link.gitPath" => require_kind(&path, "a string", value.is_string())?,
                _ => bail!("Unknown Renium configuration key '{path}'"),
            }
        }
        Ok(())
    }

    visit("", value)
}

pub fn filter_allows(
    rules: &[FilterRule],
    direction: FilterDirection,
    candidate: &FilterCandidate<'_>,
) -> Result<bool> {
    filter_allows_scope(rules, direction, candidate, FilterScope::Any)
}

#[derive(Clone, Copy)]
enum FilterScope<'a> {
    Any,
    Instance,
    Property(&'a str),
    Attribute(&'a str),
}

pub fn filter_allows_instance(
    rules: &[FilterRule],
    direction: FilterDirection,
    candidate: &FilterCandidate<'_>,
) -> Result<bool> {
    filter_allows_scope(rules, direction, candidate, FilterScope::Instance)
}

pub fn filter_allows_property(
    rules: &[FilterRule],
    direction: FilterDirection,
    candidate: &FilterCandidate<'_>,
    property: &str,
) -> Result<bool> {
    filter_allows_scope(rules, direction, candidate, FilterScope::Property(property))
}

pub fn filter_allows_attribute(
    rules: &[FilterRule],
    direction: FilterDirection,
    candidate: &FilterCandidate<'_>,
    attribute: &str,
) -> Result<bool> {
    filter_allows_scope(
        rules,
        direction,
        candidate,
        FilterScope::Attribute(attribute),
    )
}

fn filter_allows_scope(
    rules: &[FilterRule],
    direction: FilterDirection,
    candidate: &FilterCandidate<'_>,
    scope: FilterScope<'_>,
) -> Result<bool> {
    let mut allowed = true;
    for rule in rules {
        if rule.direction != FilterDirection::Both && rule.direction != direction {
            continue;
        }
        let applies = match scope {
            FilterScope::Any => true,
            FilterScope::Instance => rule.property.is_none() && rule.attribute.is_none(),
            FilterScope::Property(property) => {
                rule.attribute.is_none()
                    && rule
                        .property
                        .as_deref()
                        .is_none_or(|expected| expected == property)
            }
            FilterScope::Attribute(attribute) => {
                rule.property.is_none()
                    && rule
                        .attribute
                        .as_deref()
                        .is_none_or(|expected| expected == attribute)
            }
        };
        if !applies {
            continue;
        }
        if filter_matches(rule, candidate, scope)? {
            allowed = matches!(&rule.action, FilterAction::Include);
        }
    }
    Ok(allowed)
}

fn filter_matches(
    rule: &FilterRule,
    candidate: &FilterCandidate<'_>,
    scope: FilterScope<'_>,
) -> Result<bool> {
    if let Some(pattern) = rule.glob.as_deref() {
        let matcher = compile_glob(pattern)?;
        if !matcher.is_match(candidate.path) {
            return Ok(false);
        }
    }
    if let Some(name) = rule.name.as_deref()
        && candidate.name != name
    {
        return Ok(false);
    }
    if let Some(class) = rule.class.as_deref()
        && candidate.class != class
    {
        return Ok(false);
    }
    if let Some(tag) = rule.tag.as_deref()
        && !candidate.tags.contains(tag)
    {
        return Ok(false);
    }
    if !matches!(scope, FilterScope::Attribute(_))
        && let Some(attribute) = rule.attribute.as_deref()
        && !candidate.attributes.contains(attribute)
    {
        return Ok(false);
    }
    if !matches!(scope, FilterScope::Property(_))
        && let Some(property) = rule.property.as_deref()
        && !candidate.properties.contains(property)
    {
        return Ok(false);
    }
    if let Some(id) = rule.id.as_deref()
        && candidate.id != id
    {
        return Ok(false);
    }
    Ok(rule.glob.is_some()
        || rule.name.is_some()
        || rule.class.is_some()
        || rule.tag.is_some()
        || rule.attribute.is_some()
        || rule.property.is_some()
        || rule.id.is_some())
}

fn convert_rojo_project(path: &Path) -> Result<ReniumProject> {
    let text =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let value = parse_jsonc_value(&text)?;
    let object = value
        .as_object()
        .context("Rojo project root must be an object")?;
    let tree_value = object
        .get("tree")
        .and_then(Value::as_object)
        .context("Rojo project is missing an object-valued tree")?;
    let mut project = ReniumProject {
        name: object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        ..ReniumProject::default()
    };
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(source_root) = tree_value.get("$path").and_then(Value::as_str) {
        project.source_root = PathBuf::from(source_root);
    }
    project.root = convert_rojo_node_fields(tree_value, None)?;
    for (name, node) in tree_value {
        if name.starts_with('$') {
            continue;
        }
        let target = name.clone();
        let converted = convert_rojo_node(root, &target, Path::new(name), node, &mut project)?;
        project.tree.insert(name.clone(), converted);
    }
    if let Some(rules) = object.get("syncRules").and_then(Value::as_array) {
        for (index, value) in rules.iter().enumerate() {
            let rule: SyncRule = serde_json::from_value(value.clone())
                .with_context(|| format!("Invalid Rojo syncRules[{index}]"))?;
            project.sync_rules.push(rule);
        }
    } else {
        project.sync_rules = rojo_default_sync_rules();
    }
    project.glob_ignore_paths =
        json_string_array(object.get("globIgnorePaths")).unwrap_or_default();
    Ok(project)
}

fn normalize_rojo_node_value(
    class_name: Option<&str>,
    name: &str,
    value: &Value,
    target: Option<&str>,
    attribute: bool,
) -> Result<Value> {
    let normalized = crate::rbx::encode::normalize_project_typed_value(
        if attribute { None } else { class_name },
        if attribute { None } else { Some(name) },
        value,
    );
    if let Some(target) = target {
        normalized.with_context(|| {
            format!(
                "Invalid {} '{name}' in Rojo node '{target}'",
                if attribute { "attribute" } else { "property" }
            )
        })
    } else {
        normalized
    }
}

fn convert_rojo_node_fields(
    node: &Map<String, Value>,
    target: Option<&str>,
) -> Result<ProjectNode> {
    let mut converted = ProjectNode {
        id: node.get("$id").and_then(Value::as_str).map(str::to_string),
        class_name: node
            .get("$className")
            .and_then(Value::as_str)
            .map(str::to_string),
        tags: json_string_array(node.get("$tags")),
        ignore_unknown_instances: node.get("$ignoreUnknownInstances").and_then(Value::as_bool),
        ..ProjectNode::default()
    };
    let class_name = converted.class_name.as_deref();
    if let Some(properties) = node.get("$properties").and_then(Value::as_object) {
        for (name, value) in properties {
            if name == "Attributes"
                && let Some(attributes) = value.as_object()
            {
                for (attribute, value) in attributes {
                    converted.attributes.insert(
                        attribute.clone(),
                        normalize_rojo_node_value(None, attribute, value, target, true)?,
                    );
                }
            } else {
                converted.properties.insert(
                    name.clone(),
                    normalize_rojo_node_value(class_name, name, value, target, false)?,
                );
            }
        }
    }
    if let Some(attributes) = node.get("$attributes").and_then(Value::as_object) {
        for (name, value) in attributes {
            converted.attributes.insert(
                name.clone(),
                normalize_rojo_node_value(None, name, value, target, true)?,
            );
        }
    }
    Ok(converted)
}

fn convert_rojo_node(
    root: &Path,
    target: &str,
    source_target: &Path,
    value: &Value,
    project: &mut ReniumProject,
) -> Result<ProjectNode> {
    let node = value
        .as_object()
        .with_context(|| format!("Rojo tree node '{target}' must be an object"))?;
    let mut converted = convert_rojo_node_fields(node, Some(target))?;
    if let Some(path) = node.get("$path").and_then(Value::as_str) {
        let relative = PathBuf::from(path);
        let resolved = root.join(&relative);
        let name = resolved
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        let mounted = ends_with_ignore_ascii_case(name, ".project.json")
            || ends_with_ignore_ascii_case(name, ".project.jsonc")
            || path_extension_is(&resolved, &["rbxm", "rbxmx", "renium"]);
        if mounted {
            project.mounts.push(ProjectMount {
                source: relative,
                target: ProjectTarget::Shorthand(target.to_string()),
                ownership: MountOwnership::Exclusive,
                optional: false,
            });
        } else if relative != project.source_root.join(source_target) {
            converted.path = Some(relative);
        }
    }
    for (name, child) in node {
        if name.starts_with('$') {
            continue;
        }
        let child_target = format!("{target}.{name}");
        let child_source_target = source_target.join(name);
        let child = convert_rojo_node(root, &child_target, &child_source_target, child, project)?;
        converted
            .children
            .insert(name.clone(), serde_json::to_value(child)?);
    }
    Ok(converted)
}

fn rojo_default_sync_rules() -> Vec<SyncRule> {
    [
        ("**/*.txt", "txt", ".txt"),
        ("**/*.csv", "csv", ".csv"),
        ("**/*.json", "json", ".json"),
        ("**/*.jsonc", "jsonc", ".jsonc"),
        ("**/*.toml", "toml", ".toml"),
        ("**/*.yaml", "yaml", ".yaml"),
        ("**/*.yml", "yaml", ".yml"),
        ("**/*.msgpack", "msgpack", ".msgpack"),
        ("**/*.md", "markdown", ".md"),
        ("**/*.rbxm", "rbxm", ".rbxm"),
        ("**/*.rbxmx", "rbxmx", ".rbxmx"),
        ("**/*.model.json", "model-json", ".model.json"),
        (
            "**/*.model.renium.jsonc",
            "model-json",
            ".model.renium.jsonc",
        ),
        ("**/*.project.json", "nested-project", ".project.json"),
        ("**/*.project.jsonc", "nested-project", ".project.jsonc"),
    ]
    .into_iter()
    .map(|(pattern, middleware, suffix)| SyncRule {
        pattern: pattern.to_string(),
        exclude: None,
        middleware: middleware.to_string(),
        suffix: Some(suffix.to_string()),
    })
    .collect()
}

fn config_with_origins(root: &Path) -> Result<Value> {
    let mut values = json!({ "schemaVersion": 1 });
    let mut origins = Map::new();
    for scope in [
        ConfigScope::User,
        ConfigScope::Workspace,
        ConfigScope::Experience,
        ConfigScope::Place,
    ] {
        let path = config_scope_path(scope, root)?;
        if !path.is_file() {
            continue;
        }
        let value = read_json_object_or_empty(&path)?;
        merge_json_with_origins(&mut values, value, "", &path, &mut origins);
    }
    if nearest_project_marker(root).is_some() {
        let project = load_project(None, Some(root))?;
        if project.project.settings.is_object() {
            merge_json_with_origins(
                &mut values,
                project.project.settings,
                "",
                &project.path,
                &mut origins,
            );
        }
    }
    Ok(json!({ "values": values, "origins": origins }))
}

fn merge_json_with_origins(
    base: &mut Value,
    overlay: Value,
    prefix: &str,
    path: &Path,
    origins: &mut Map<String, Value>,
) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                let dotted = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                if value.is_null() {
                    base.remove(&key);
                    remove_origin_subtree(origins, &dotted);
                } else {
                    merge_json_with_origins(
                        base.entry(key).or_insert(Value::Null),
                        value,
                        &dotted,
                        path,
                        origins,
                    );
                }
            }
        }
        (base, overlay) => {
            remove_origin_subtree(origins, prefix);
            *base = overlay;
            record_origin_leaves(base, prefix, path, origins);
        }
    }
}

fn record_origin_leaves(
    value: &Value,
    prefix: &str,
    path: &Path,
    origins: &mut Map<String, Value>,
) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let dotted = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            record_origin_leaves(value, &dotted, path, origins);
        }
    } else if !prefix.is_empty() {
        origins.insert(
            prefix.to_string(),
            Value::String(path.display().to_string()),
        );
    }
}

fn remove_origin_subtree(origins: &mut Map<String, Value>, prefix: &str) {
    if prefix.is_empty() {
        origins.clear();
        return;
    }
    let child_prefix = format!("{prefix}.");
    origins.retain(|key, _| key != prefix && !key.starts_with(&child_prefix));
}

fn load_config_scope(scope: ConfigScope, root: &Path) -> Result<Value> {
    if scope == ConfigScope::Merged {
        return load_merged_config(root);
    }
    let path = config_scope_path(scope, root)?;
    read_json_object_or_empty(&path)
}

fn config_scope_path(scope: ConfigScope, root: &Path) -> Result<PathBuf> {
    match scope {
        ConfigScope::User => user_config_path(),
        ConfigScope::Workspace => {
            Ok(config_scope_root(root, ".git").join(".renium/workspace.config.json"))
        }
        ConfigScope::Experience => {
            Ok(config_scope_root(root, "renium.experience.json").join("renium.config.json"))
        }
        ConfigScope::Place => {
            let root = if nearest_project_marker(root).is_some() {
                load_project(None, Some(root))?.root
            } else {
                absolute_path(root)
            };
            Ok(root.join(".renium/config.json"))
        }
        ConfigScope::Merged => bail!("Merged configuration has no writable path"),
    }
}

fn nearest_project_marker(root: &Path) -> Option<PathBuf> {
    let mut current = absolute_path(root);
    if current.is_file() {
        current.pop();
    }
    loop {
        if current.join(PROJECT_FILE_NAME).is_file()
            || current.join(PROJECT_JSON_FILE_NAME).is_file()
        {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn user_config_path() -> Result<PathBuf> {
    if cfg!(windows) {
        let base = env::var_os("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(base).join("Renium/config.json"));
    }
    if cfg!(target_os = "macos") {
        let home = env::var_os("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home).join("Library/Application Support/Renium/config.json"));
    }
    if let Some(base) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(base).join("renium/config.json"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/renium/config.json"))
}

fn config_scope_root(root: &Path, marker: &str) -> PathBuf {
    find_ancestor_with(root, marker).unwrap_or_else(|| absolute_path(root))
}

fn find_ancestor_with(root: &Path, marker: &str) -> Option<PathBuf> {
    let mut current = absolute_path(root);
    loop {
        if current.join(marker).exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn projection_path_key(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| absolute_path(path));
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) || cfg!(target_os = "macos") {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn projection_path_contains(parent: &Path, child: &Path) -> bool {
    let parent = projection_path_key(parent);
    let child = projection_path_key(child);
    child == parent
        || child
            .strip_prefix(&parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn ensure_writable_scope(scope: ConfigScope) -> Result<()> {
    if scope == ConfigScope::Merged {
        bail!("--scope merged is read-only; choose user, workspace, experience, or place");
    }
    Ok(())
}

fn read_json_object_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let value = parse_jsonc_value(&text)?;
    if !value.is_object() {
        bail!("{} must contain a JSON object", path.display());
    }
    Ok(value)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    atomic_write_file(
        path,
        (serde_json::to_string_pretty(value)? + "\n").as_bytes(),
    )
}

fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                if value.is_null() {
                    base.remove(&key);
                } else {
                    merge_json(base.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn get_dotted<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in dotted_segments(key).ok()? {
        current = current.get(segment)?;
    }
    Some(current)
}

fn set_dotted(value: &mut Value, key: &str, new_value: Value) -> Result<()> {
    let segments = dotted_segments(key)?;
    let (leaf, parents) = segments
        .split_last()
        .context("Configuration key is empty")?;
    let mut current = value;
    for segment in parents {
        let object = current
            .as_object_mut()
            .context("A parent configuration key is not an object")?;
        current = object
            .entry((*segment).to_string())
            .or_insert_with(|| json!({}));
    }
    current
        .as_object_mut()
        .context("A parent configuration key is not an object")?
        .insert((*leaf).to_string(), new_value);
    Ok(())
}

fn remove_dotted(value: &mut Value, key: &str) -> Result<bool> {
    let segments = dotted_segments(key)?;
    let (leaf, parents) = segments
        .split_last()
        .context("Configuration key is empty")?;
    let mut current = value;
    for segment in parents {
        let Some(next) = current.get_mut(*segment) else {
            return Ok(false);
        };
        current = next;
    }
    Ok(current
        .as_object_mut()
        .context("A parent configuration key is not an object")?
        .remove(*leaf)
        .is_some())
}

fn dotted_segments(key: &str) -> Result<Vec<&str>> {
    let segments = key.split('.').collect::<Vec<_>>();
    if segments.is_empty() || segments.iter().any(|segment| segment.trim().is_empty()) {
        bail!("Configuration keys use non-empty dot-separated segments");
    }
    Ok(segments)
}

fn open_in_editor(path: &Path) -> Result<()> {
    let editor = env::var_os("VISUAL").or_else(|| env::var_os("EDITOR"));
    if let Some(editor) = editor {
        let status = Command::new(editor)
            .arg(path)
            .status()
            .context("Failed to open the configured editor")?;
        if !status.success() {
            bail!("The configured editor exited with {status}");
        }
        return Ok(());
    }
    println!("{}", path.display());
    Ok(())
}

pub fn validate_relative_portable_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{field} cannot be empty");
    }
    if path.is_absolute() {
        bail!("{field} must be relative: {}", path.display());
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            bail!("{field} cannot leave the project: {}", path.display());
        }
    }
    for segment in path.iter().filter_map(OsStr::to_str) {
        if segment.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        }) {
            bail!("{field} contains a non-portable segment '{segment}'");
        }
        let trimmed = segment.trim_end_matches([' ', '.']);
        if trimmed != segment || is_windows_reserved_name(trimmed) {
            bail!("{field} contains a non-portable segment '{segment}'");
        }
    }
    Ok(())
}

fn validate_instance_target(target: &ProjectTarget, field: &str) -> Result<()> {
    let segments = target.segments();
    let ordinals = target.ordinals();
    if segments.is_empty() || segments.iter().any(|segment| segment.trim().is_empty()) {
        bail!("{field} must contain non-empty instance names");
    }
    if !ordinals.is_empty() && ordinals.len() != segments.len() {
        bail!("{field}.ordinals must contain one value per segment");
    }
    if ordinals.contains(&0) {
        bail!("{field}.ordinals values must be at least 1");
    }
    if ordinals.first().is_some_and(|ordinal| *ordinal != 1) {
        bail!("{field}.ordinals must use 1 for the Studio service root");
    }
    Ok(())
}

fn validate_filesystem_target(target: &ProjectTarget, field: &str) -> Result<()> {
    if target.ordinals().iter().any(|ordinal| *ordinal > 1) {
        bail!("{field} cannot select duplicate sibling ordinals for a filesystem-backed owner");
    }
    for segment in target.segments() {
        if segment == "."
            || segment == ".."
            || segment.ends_with(' ')
            || segment.ends_with('.')
            || segment.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character,
                        '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
                    )
            })
            || is_windows_reserved_name(&segment)
        {
            bail!("{field} contains an instance name that cannot map to a portable file path");
        }
    }
    Ok(())
}

fn validate_direct_owner_source(source: &Path, field: &str) -> Result<()> {
    if source.is_dir() {
        return Ok(());
    }
    let name = source
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if path_extension_is(source, &["lua", "luau", "renium", "rbxm", "rbxmx"])
        || ends_with_ignore_ascii_case(name, ".project.json")
        || ends_with_ignore_ascii_case(name, ".project.jsonc")
    {
        return Ok(());
    }
    bail!("{field} is not a supported direct owner; use an adapter for this file type")
}

fn instance_target_overlaps(left: &ProjectTarget, right: &ProjectTarget) -> bool {
    let left_segments = left.segments();
    let right_segments = right.segments();
    let left_ordinals = left.ordinals();
    let right_ordinals = right.ordinals();
    let shared = left_segments.len().min(right_segments.len());
    if left_segments[..shared] != right_segments[..shared] {
        return false;
    }
    for index in 0..shared {
        let left_ordinal = left_ordinals.get(index).copied().unwrap_or(1);
        let right_ordinal = right_ordinals.get(index).copied().unwrap_or(1);
        if left_ordinal != right_ordinal {
            return false;
        }
    }
    true
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    let cache = GLOB_MATCHER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let existing = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(pattern)
        .cloned();
    if let Some(matcher) = existing {
        return Ok(matcher);
    }
    let matcher = Glob::new(pattern)
        .with_context(|| format!("Invalid glob '{pattern}'"))?
        .compile_matcher();
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(pattern.to_string(), matcher.clone());
    Ok(matcher)
}

fn print_json(value: &Value, pretty: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    if crate::app::output::global_pretty_output(pretty) {
        serde_json::to_writer_pretty(&mut lock, value)?;
    } else {
        serde_json::to_writer(&mut lock, value)?;
    }
    writeln!(lock)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_removes_comments_and_trailing_commas() {
        let value = parse_jsonc_value(
            r#"{
                // line
                "url": "https://example.com//x",
                "items": [1, 2,],
                /* block */
            }"#,
        )
        .unwrap();
        assert_eq!(value["url"], "https://example.com//x");
        assert_eq!(value["items"], json!([1, 2]));
    }

    #[test]
    fn later_filter_rules_override_earlier_rules() {
        let tags = BTreeSet::new();
        let attrs = BTreeSet::new();
        let props = BTreeSet::new();
        let candidate = FilterCandidate {
            id: "editor:1",
            path: "Workspace/Keep/Part",
            name: "Part",
            class: "Part",
            tags: &tags,
            attributes: &attrs,
            properties: &props,
        };
        let rules = vec![
            FilterRule {
                action: FilterAction::Ignore,
                glob: Some("Workspace/**".to_string()),
                ..Default::default()
            },
            FilterRule {
                action: FilterAction::Include,
                glob: Some("Workspace/Keep/**".to_string()),
                ..Default::default()
            },
        ];
        assert!(filter_allows(&rules, FilterDirection::StudioToFiles, &candidate).unwrap());
    }
}
