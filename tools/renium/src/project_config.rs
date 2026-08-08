use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use globset::{Glob, GlobMatcher, escape as escape_glob};
use notify::{RecursiveMode, Watcher};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::settings_bytecode::{SettingsBytecode, SettingsBytecodeInstance};

pub const PROJECT_FILE_NAME: &str = "renium.project.jsonc";
pub const PROJECT_JSON_FILE_NAME: &str = "renium.project.json";
pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const PROJECT_SCHEMA_URL: &str = "https://raw.githubusercontent.com/Superwheat/renium/main/tools/renium/schemas/renium.project.schema.json";
static PROJECTION_STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
        serde_json::to_string(&(self.segments(), self.ordinals())).unwrap_or_default()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReniumProject {
    #[serde(default, rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default = "project_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_source_root")]
    pub source_root: PathBuf,
    #[serde(default)]
    pub build_target: Option<ProjectTarget>,
    #[serde(default)]
    pub root: ProjectNode,
    #[serde(default)]
    pub tree: BTreeMap<String, ProjectNode>,
    #[serde(default)]
    pub mounts: Vec<ProjectMount>,
    #[serde(default)]
    pub adapters: Vec<AdapterSpec>,
    #[serde(default)]
    pub sync_rules: Vec<SyncRule>,
    #[serde(default)]
    pub glob_ignore_paths: Vec<String>,
    #[serde(default)]
    pub filters: Vec<FilterRule>,
    #[serde(default)]
    pub script_extension: ScriptExtensionPolicy,
    #[serde(default)]
    pub export_naming: ExportNaming,
    #[serde(default)]
    pub settings: Value,
}

impl Default for ReniumProject {
    fn default() -> Self {
        Self {
            schema: Some(PROJECT_SCHEMA_URL.to_string()),
            schema_version: PROJECT_SCHEMA_VERSION,
            name: None,
            source_root: default_source_root(),
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProjectNode {
    #[serde(default, rename = "$id")]
    pub id: Option<String>,
    #[serde(default, rename = "$path")]
    pub path: Option<PathBuf>,
    #[serde(default, rename = "$className")]
    pub class_name: Option<String>,
    #[serde(default, rename = "$properties")]
    pub properties: Map<String, Value>,
    #[serde(default, rename = "$attributes")]
    pub attributes: Map<String, Value>,
    #[serde(default, rename = "$tags")]
    pub tags: Option<Vec<String>>,
    #[serde(default, rename = "$ignoreUnknownInstances")]
    pub ignore_unknown_instances: Option<bool>,
    #[serde(default, flatten)]
    pub children: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectMount {
    pub source: PathBuf,
    pub target: ProjectTarget,
    #[serde(default)]
    pub ownership: MountOwnership,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MountOwnership {
    #[default]
    Exclusive,
    Overlay,
    ReadOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterSpec {
    pub source: PathBuf,
    pub target: ProjectTarget,
    #[serde(default)]
    pub output: Option<PathBuf>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub direction: AdapterDirection,
    #[serde(default)]
    pub generated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncRule {
    pub pattern: String,
    #[serde(default)]
    pub exclude: Option<String>,
    #[serde(rename = "use")]
    pub middleware: String,
    #[serde(default)]
    pub suffix: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterDirection {
    #[default]
    ToProject,
    FromProject,
    TwoWay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilterRule {
    #[serde(default)]
    pub action: FilterAction,
    #[serde(default)]
    pub direction: FilterDirection,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub property: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FilterAction {
    Include,
    #[default]
    Ignore,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FilterDirection {
    #[default]
    Both,
    StudioToFiles,
    FilesToStudio,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScriptExtensionPolicy {
    #[default]
    Preserve,
    Luau,
    Lua,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportNaming {
    #[serde(default = "default_server_suffix")]
    pub server_suffix: String,
    #[serde(default = "default_client_suffix")]
    pub client_suffix: String,
    #[serde(default = "default_module_suffix")]
    pub module_suffix: String,
    #[serde(default = "default_plugin_suffix")]
    pub plugin_suffix: String,
    #[serde(default = "default_client_run_context_suffix")]
    pub client_run_context_suffix: String,
}

impl Default for ExportNaming {
    fn default() -> Self {
        Self {
            server_suffix: default_server_suffix(),
            client_suffix: default_client_suffix(),
            module_suffix: default_module_suffix(),
            plugin_suffix: default_plugin_suffix(),
            client_run_context_suffix: default_client_run_context_suffix(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectScriptNaming {
    pub extension: ScriptExtensionPolicy,
    pub server_suffix: String,
    pub client_suffix: String,
    pub module_suffix: String,
    pub plugin_suffix: String,
    pub client_run_context_suffix: String,
}

impl Default for ProjectScriptNaming {
    fn default() -> Self {
        Self {
            extension: ScriptExtensionPolicy::Preserve,
            server_suffix: default_server_suffix(),
            client_suffix: default_client_suffix(),
            module_suffix: default_module_suffix(),
            plugin_suffix: default_plugin_suffix(),
            client_run_context_suffix: default_client_run_context_suffix(),
        }
    }
}

fn project_schema_version() -> u32 {
    PROJECT_SCHEMA_VERSION
}

fn default_source_root() -> PathBuf {
    PathBuf::from("src")
}

fn default_server_suffix() -> String {
    ".server".to_string()
}

fn default_client_suffix() -> String {
    ".client".to_string()
}

fn default_module_suffix() -> String {
    String::new()
}

fn default_plugin_suffix() -> String {
    ".plugin".to_string()
}

fn default_client_run_context_suffix() -> String {
    ".run-client".to_string()
}

#[derive(Args, Debug)]
pub struct FmtProjectArgs {
    #[arg(value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub check: bool,
}

#[derive(Args, Debug)]
pub struct ExplainPathArgs {
    pub path: PathBuf,
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub pretty: bool,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
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

#[derive(Args, Debug)]
pub struct ConfigGetArgs {
    pub key: Option<String>,
    #[arg(long, value_enum, default_value_t = ConfigScope::Merged)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Args, Debug)]
pub struct ConfigSetArgs {
    pub key: String,
    pub value: String,
    #[arg(long, value_enum, default_value_t = ConfigScope::Place)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub string: bool,
}

#[derive(Args, Debug)]
pub struct ConfigUnsetArgs {
    pub key: String,
    #[arg(long, value_enum, default_value_t = ConfigScope::Place)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Args, Debug)]
pub struct ConfigScopeArgs {
    #[arg(long, value_enum, default_value_t = ConfigScope::Place)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(Args, Debug)]
pub struct ConfigListArgs {
    #[arg(long, value_enum, default_value_t = ConfigScope::Merged)]
    pub scope: ConfigScope,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub origins: bool,
}

#[derive(Args, Debug)]
pub struct ConfigExportArgs {
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    User,
    Workspace,
    Experience,
    Place,
    Merged,
}

#[derive(Args, Debug)]
pub struct AdaptersArgs {
    #[command(subcommand)]
    pub command: AdaptersCommand,
}

#[derive(Subcommand, Debug)]
pub enum AdaptersCommand {
    Validate(AdapterProjectArgs),
    Build(AdapterBuildArgs),
    Syncback(AdapterSyncbackArgs),
    Watch(AdapterWatchArgs),
}

#[derive(Args, Debug)]
pub struct AdapterProjectArgs {
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct AdapterBuildArgs {
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub check: bool,
}

#[derive(Args, Debug)]
pub struct AdapterSyncbackArgs {
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue, conflicts_with = "preview")]
    pub check: bool,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub preview: bool,
}

#[derive(Args, Debug)]
pub struct AdapterWatchArgs {
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<PathBuf>,
    #[arg(long, default_value_t = 250)]
    pub interval_ms: u64,
}

#[derive(Args, Debug)]
pub struct ImportRojoArgs {
    #[arg(long, value_name = "PATH")]
    pub project: PathBuf,
    #[arg(long, action = clap::ArgAction::SetTrue, conflicts_with = "apply")]
    pub preview: bool,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub apply: bool,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectionEntry {
    id: String,
    kind: String,
    source: String,
    target: String,
    ownership: Option<MountOwnership>,
    direction: Option<AdapterDirection>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompiledProjection {
    schema_version: u32,
    project: String,
    entries: Vec<ProjectionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataSidecar {
    #[serde(default, rename = "$schema")]
    _schema: Option<String>,
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default, rename = "$id", alias = "id")]
    id: Option<String>,
    #[serde(default, rename = "$className", alias = "className")]
    class_name: Option<String>,
    #[serde(default, rename = "$properties", alias = "properties")]
    properties: Map<String, Value>,
    #[serde(default, rename = "$attributes", alias = "attributes")]
    attributes: Map<String, Value>,
    #[serde(default, rename = "$tags", alias = "tags")]
    tags: Option<Vec<String>>,
    #[serde(
        default,
        rename = "$ignoreUnknownInstances",
        alias = "ignoreUnknownInstances"
    )]
    _ignore_unknown_instances: Option<bool>,
}

#[derive(Debug)]
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

    fn transform_targets(&self) -> impl Iterator<Item = &Vec<String>> {
        self.transforms.iter().map(|transform| &transform.target)
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

#[derive(Debug, Clone)]
pub struct FilterCandidate<'a> {
    pub id: &'a str,
    pub path: &'a str,
    pub name: &'a str,
    pub class: &'a str,
    pub tags: &'a BTreeSet<String>,
    pub attributes: &'a BTreeSet<String>,
    pub properties: &'a BTreeSet<String>,
}

pub(crate) fn filter_path_segments(segments: &[String]) -> String {
    segments
        .iter()
        .map(|segment| segment.replace('~', "~0").replace('/', "~1"))
        .collect::<Vec<_>>()
        .join("/")
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

#[derive(Clone)]
struct ReverseOwner {
    target: Vec<String>,
    ordinals: Vec<usize>,
    source: PathBuf,
    ownership: MountOwnership,
    ignore_unknown_instances: bool,
    optional: bool,
}

#[derive(Clone)]
struct ProjectionFieldOwner {
    target: Vec<String>,
    source: String,
    class_name: bool,
    settings_id: bool,
    properties: BTreeSet<String>,
    attributes: BTreeSet<String>,
    tags: bool,
}

#[derive(Clone)]
struct ReverseSource {
    text: String,
    extension: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterBaseline {
    #[serde(default)]
    entries: BTreeMap<String, AdapterBaselineEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterBaselineEntry {
    source_hash: String,
    target_hash: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    output_hash: Option<String>,
    #[serde(default)]
    output_owned: bool,
    #[serde(default)]
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
            if path.is_file() && adapter_format(adapter)? == "nested-project" {
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
    for (_, node) in project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path.as_deref() {
            let path = loaded.root.join(path);
            roots.insert(if path.is_file() {
                path.parent().unwrap_or(&loaded.root).to_path_buf()
            } else {
                path.clone()
            });
            if path.is_file() && is_nested_project_path(&path) {
                project_source_roots_into(&load_nested_project(&path)?, roots, visited)?;
            }
        }
    }
    for mount in &loaded.project.mounts {
        let path = loaded.root.join(&mount.source);
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
        if source.is_file() && adapter_format(adapter)? == "nested-project" {
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

pub fn staged_path_to_project_source(
    loaded: &LoadedProject,
    staged_relative: &Path,
) -> Result<Option<PathBuf>> {
    let segments = staged_relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut mappings = project_target_source_mappings(loaded)?;
    mappings.sort_by_key(|mapping| std::cmp::Reverse(mapping.target.len()));
    for mapping in mappings {
        let mut target_segments = segments.clone();
        if let Some(file_name) = target_segments.last().cloned()
            && let Some((_, leaf, _)) = crate::infer_source_script(&file_name, &mapping.naming)
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
    let segments = staged_relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
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
    let selected = longest.and_then(|length| {
        let mut matches = candidates
            .iter()
            .filter(|candidate| candidate.0 == length)
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.1.cmp(right.1).then(left.2.cmp(&right.2)));
        if matches.len() == 1 {
            matches.pop()
        } else {
            None
        }
    });
    if longest.is_some() && selected.is_none() {
        bail!(
            "Projected path '{}' has more than one equally specific owner",
            segments.join("/")
        );
    }
    if let Some((_, owner, path, source_root, writable)) = selected {
        if !writable {
            bail!(
                "Projected path '{}' is owned by a non-writable {owner}",
                segments.join("/")
            );
        }
        if source_root.is_file() && segments.len() > longest.unwrap_or_default() {
            if is_nested_project_path(source_root) {
                let nested = load_nested_project(source_root)?;
                let mut resolved = resolve_project_write_segments_inner(
                    &nested,
                    &segments[longest.unwrap_or_default()..],
                    true,
                )?;
                resolved.consumed_segments += longest.unwrap_or_default();
                return Ok(resolved);
            }
            bail!(
                "Projected path '{}' descends through file owner {}",
                segments.join("/"),
                path.display()
            );
        }
        return Ok(ProjectWriteResolution {
            path: path.clone(),
            source_root: source_root.clone(),
            owner,
            consumed_segments: longest.unwrap_or_default(),
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
        .map(|service| source_root.join(service))
        .unwrap_or_else(|| source_root.clone());
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
    let segments = staged_relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
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
            let settings = crate::existing_service_settings_path(&stage_root.join(service));
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
        if adapter_source == source
            && !matches!(
                format.as_str(),
                "txt" | "csv" | "model-json" | "rbxm" | "rbxmx" | "nested-project"
            )
        {
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
    let mut matches = project_target_source_mappings(loaded)?
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
            .drain(..)
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
            (source == root).then(|| {
                root.file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(PathBuf::new)
            })
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
                normalize_sync_middleware(&adapter_format(adapter)?) == "nested-project",
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
        return crate::emit_global_output(
            &json!({ "ok": true, "path": project_path, "formatted": true }),
            &format!("{} is formatted", project_path.display()),
        );
    }
    atomic_write(&project_path, formatted.as_bytes())?;
    crate::emit_global_output(
        &json!({ "ok": true, "path": project_path, "formatted": true }),
        &format!("Formatted {}", project_path.display()),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsoncTokenKind {
    Open,
    Close,
    Comma,
    Colon,
    Value,
    LineComment,
    BlockComment,
}

struct JsoncToken {
    kind: JsoncTokenKind,
    text: String,
}

fn format_jsonc(text: &str) -> Result<String> {
    let tokens = tokenize_jsonc(text)?;
    let mut output = String::new();
    let mut depth = 0usize;
    let mut line_start = true;
    let mut previous = None;
    for (index, token) in tokens.iter().enumerate() {
        let next = tokens.get(index + 1).map(|token| token.kind);
        match token.kind {
            JsoncTokenKind::Open => {
                write_jsonc_indent(&mut output, depth, &mut line_start);
                output.push_str(&token.text);
                depth += 1;
                if next != Some(JsoncTokenKind::Close) {
                    output.push('\n');
                    line_start = true;
                }
            }
            JsoncTokenKind::Close => {
                depth = depth.saturating_sub(1);
                if !line_start {
                    output.push('\n');
                    line_start = true;
                }
                write_jsonc_indent(&mut output, depth, &mut line_start);
                output.push_str(&token.text);
            }
            JsoncTokenKind::Comma => {
                output.push(',');
                output.push('\n');
                line_start = true;
            }
            JsoncTokenKind::Colon => {
                output.push_str(": ");
                line_start = false;
            }
            JsoncTokenKind::Value => {
                write_jsonc_indent(&mut output, depth, &mut line_start);
                if matches!(
                    previous,
                    Some(JsoncTokenKind::Value | JsoncTokenKind::BlockComment)
                ) {
                    output.push(' ');
                }
                output.push_str(&token.text);
            }
            JsoncTokenKind::LineComment => {
                write_jsonc_indent(&mut output, depth, &mut line_start);
                if !output.ends_with([' ', '\n']) {
                    output.push(' ');
                }
                output.push_str(token.text.trim_end());
                output.push('\n');
                line_start = true;
            }
            JsoncTokenKind::BlockComment => {
                write_jsonc_indent(&mut output, depth, &mut line_start);
                if !output.ends_with([' ', '\n']) {
                    output.push(' ');
                }
                output.push_str(token.text.trim());
                if matches!(
                    next,
                    Some(
                        JsoncTokenKind::Value
                            | JsoncTokenKind::Open
                            | JsoncTokenKind::LineComment
                            | JsoncTokenKind::BlockComment
                    )
                ) {
                    output.push('\n');
                    line_start = true;
                }
            }
        }
        previous = Some(token.kind);
    }
    while output.ends_with([' ', '\t', '\r', '\n']) {
        output.pop();
    }
    output.push('\n');
    Ok(output)
}

fn write_jsonc_indent(output: &mut String, depth: usize, line_start: &mut bool) {
    if *line_start {
        output.push_str(&"  ".repeat(depth));
        *line_start = false;
    }
}

fn tokenize_jsonc(text: &str) -> Result<Vec<JsoncToken>> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        let kind = match chars[index] {
            '{' | '[' => {
                index += 1;
                JsoncTokenKind::Open
            }
            '}' | ']' => {
                index += 1;
                JsoncTokenKind::Close
            }
            ',' => {
                index += 1;
                JsoncTokenKind::Comma
            }
            ':' => {
                index += 1;
                JsoncTokenKind::Colon
            }
            '"' => {
                index += 1;
                let mut escaped = false;
                while index < chars.len() {
                    let character = chars[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        break;
                    }
                }
                if chars.get(index.saturating_sub(1)) != Some(&'"') {
                    bail!("Unterminated JSON string");
                }
                JsoncTokenKind::Value
            }
            '/' if chars.get(index + 1) == Some(&'/') => {
                index += 2;
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
                JsoncTokenKind::LineComment
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                    index += 1;
                }
                if index + 1 >= chars.len() {
                    bail!("Unterminated JSON block comment");
                }
                index += 2;
                JsoncTokenKind::BlockComment
            }
            _ => {
                index += 1;
                while index < chars.len() {
                    let character = chars[index];
                    if character.is_whitespace()
                        || matches!(character, '{' | '}' | '[' | ']' | ',' | ':')
                        || (character == '/' && matches!(chars.get(index + 1), Some('/' | '*')))
                    {
                        break;
                    }
                    index += 1;
                }
                JsoncTokenKind::Value
            }
        };
        tokens.push(JsoncToken {
            kind,
            text: chars[start..index].iter().collect(),
        });
    }
    Ok(tokens)
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
    let rule_relative = source_owner_relative_path(&loaded, &absolute).unwrap_or(relative.clone());
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
                .map(path_slash)
                .unwrap_or_else(|_| path_slash(path))
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
        let service_name = service.file_name().to_string_lossy().to_string();
        let settings = crate::existing_service_settings_path(&service.path());
        if !settings.is_file() {
            continue;
        }
        let document = SettingsBytecode::read_file(&settings)?;
        let source_paths =
            crate::build_editor_source_paths_by_index(&document, &service_name, &service.path());
        let paths = projection_instance_paths(&document);
        for (index, source_path) in source_paths.into_iter().enumerate() {
            let source_matches = source_path
                .as_deref()
                .is_some_and(|source_path| staged_absolute.contains(&absolute_path(source_path)));
            if !source_matches && sidecar_target.as_ref() != Some(&paths[index]) {
                continue;
            }
            let instance = &document.instances[index];
            let tags = instance
                .properties
                .get("Tags")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let attributes = instance.attributes.keys().cloned().collect::<BTreeSet<_>>();
            let properties = instance.properties.keys().cloned().collect::<BTreeSet<_>>();
            let candidate_path = filter_path_segments(&paths[index]);
            let candidate = FilterCandidate {
                id: &instance.settings_id,
                path: &candidate_path,
                name: &instance.name,
                class: &instance.class_name,
                tags: &tags,
                attributes: &attributes,
                properties: &properties,
            };
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
            crate::emit_global_output(
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
            crate::emit_global_output(
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
            crate::emit_global_output(
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
            crate::emit_global_output(
                &json!({ "ok": true, "path": path }),
                &path.display().to_string(),
            )
        }
        ConfigCommand::Export(args) => {
            let value = load_merged_config(&args.root)?;
            let text = serde_json::to_string_pretty(&value)? + "\n";
            if let Some(output) = args.output {
                atomic_write(&output, text.as_bytes())?;
                crate::emit_global_output(
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
            let projection = compile_projection(&loaded)?;
            crate::emit_global_output(
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
            crate::emit_global_output(
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
        let mut candidates = fs::read_dir(&args.project)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.ends_with(".project.json"))
            })
            .collect::<Vec<_>>();
        candidates.sort();
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
    atomic_write(&output, text.as_bytes())?;
    crate::emit_global_output(
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
            "No {} was found from the current directory upward; pass --project",
            PROJECT_FILE_NAME
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
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(absolute_path(root), naming);
    Ok(())
}

pub fn cache_script_naming(root: &Path, project: &ReniumProject) {
    let naming = project_script_naming(project);
    SCRIPT_NAMING_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(absolute_path(root), naming);
}

fn remove_cached_script_naming(root: &Path) {
    if let Some(cache) = SCRIPT_NAMING_CACHE.get() {
        let root = absolute_path(root);
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|path, _| !path.starts_with(&root));
    }
}

fn relocate_cached_script_naming(source: &Path, destination: &Path) {
    let source = absolute_path(source);
    let destination = absolute_path(destination);
    let cache = SCRIPT_NAMING_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    ProjectScriptNaming {
        extension: project.script_extension,
        server_suffix: project.export_naming.server_suffix.clone(),
        client_suffix: project.export_naming.client_suffix.clone(),
        module_suffix: project.export_naming.module_suffix.clone(),
        plugin_suffix: project.export_naming.plugin_suffix.clone(),
        client_run_context_suffix: project.export_naming.client_run_context_suffix.clone(),
    }
}

pub fn cached_script_naming(root: &Path) -> ProjectScriptNaming {
    let root = absolute_path(root);
    SCRIPT_NAMING_CACHE
        .get()
        .and_then(|cache| {
            cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            "No {} was found from the current directory upward; pass --project",
            PROJECT_FILE_NAME
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
    let mut projects = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| {
                    let name = name.to_ascii_lowercase();
                    name.ends_with(".project.json") && name != PROJECT_JSON_FILE_NAME
                })
        })
        .collect::<Vec<_>>();
    projects.sort();
    Ok(projects)
}

pub fn parse_jsonc_value(text: &str) -> Result<Value> {
    let stripped = strip_jsonc_comments(text)?;
    let without_trailing = strip_json_trailing_commas(&stripped);
    serde_json::from_str(&without_trailing).context("Invalid JSON")
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
        if scope_override
            .is_some_and(|(override_path, _)| absolute_path(override_path) == absolute_path(&path))
        {
            merge_json(&mut merged, scope_override.unwrap().1.clone());
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
    fn require_kind(path: &str, _value: &Value, expected: &str, valid: bool) -> Result<()> {
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
                "schemaVersion" => {
                    require_kind(&path, value, "the integer 1", value.as_u64() == Some(1))?
                }
                "services" | "gitSync.stagePaths" => require_kind(
                    &path,
                    value,
                    "an array of strings",
                    value
                        .as_array()
                        .is_some_and(|values| values.iter().all(Value::is_string)),
                )?,
                "sourceWorkers"
                | "instanceWorkers"
                | "importWorkers"
                | "chunkSize"
                | "snapshotInstanceChunkSize"
                | "autoSyncDebounceMs"
                | "studioLiveSyncPollMs"
                | "liveSync.changesThreshold"
                | "liveSync.diffLinesLimit"
                | "benchmarkRuns" => require_kind(
                    &path,
                    value,
                    "an integer",
                    value.as_i64().is_some() || value.as_u64().is_some(),
                )?,
                "bridgeWaitSeconds"
                | "wsWaitSeconds"
                | "startupWaitSeconds"
                | "progressHeartbeatSeconds"
                | "gitSync.timeoutSeconds" => {
                    require_kind(&path, value, "a number", value.is_number())?
                }
                "yes"
                | "backtrace"
                | "usePersistentBridge"
                | "verifyEditorPushSources"
                | "adaptiveThrottle"
                | "noUpdateEditorIcons"
                | "autoSyncOnSave"
                | "editorLiveSyncEnabled"
                | "editorLiveSyncOnStartup"
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
                    require_kind(&path, value, "a boolean", value.is_boolean())?
                }
                "transport" => require_kind(
                    &path,
                    value,
                    "'ws' or 'mcp'",
                    matches!(value.as_str(), Some("ws" | "mcp")),
                )?,
                "importMode" => require_kind(
                    &path,
                    value,
                    "'direct' or 'snapshot'",
                    matches!(value.as_str(), Some("direct" | "snapshot")),
                )?,
                "performanceMode" => require_kind(
                    &path,
                    value,
                    "'throughput', 'balanced', or 'smooth'",
                    matches!(value.as_str(), Some("throughput" | "balanced" | "smooth")),
                )?,
                "logLevel" => require_kind(
                    &path,
                    value,
                    "off, error, warn, info, debug, or trace",
                    matches!(
                        value.as_str(),
                        Some("off" | "error" | "warn" | "info" | "debug" | "trace")
                    ),
                )?,
                "color" => require_kind(
                    &path,
                    value,
                    "auto, always, or never",
                    matches!(value.as_str(), Some("auto" | "always" | "never")),
                )?,
                "outputMode" => require_kind(
                    &path,
                    value,
                    "text, json, or pretty",
                    matches!(value.as_str(), Some("text" | "json" | "pretty")),
                )?,
                "liveSync.initialSyncPriority" => require_kind(
                    &path,
                    value,
                    "studio, editor, or none",
                    matches!(value.as_str(), Some("studio" | "editor" | "none")),
                )?,
                "liveSync.displayPrompts" => require_kind(
                    &path,
                    value,
                    "always, initial, or never",
                    matches!(value.as_str(), Some("always" | "initial" | "never")),
                )?,
                "liveSync.conflictResolution" => require_kind(
                    &path,
                    value,
                    "prompt, filesystem, or studio",
                    matches!(value.as_str(), Some("prompt" | "filesystem" | "studio")),
                )?,
                "gitSync.runFullSyncBeforePush"
                | "gitSync.applyPulledChangesToStudio"
                | "wallySync.applyToStudio"
                | "link.applyToStudio" => require_kind(
                    &path,
                    value,
                    "ask, always, or never",
                    matches!(value.as_str(), Some("ask" | "always" | "never")),
                )?,
                "gitSync.stageMode" => require_kind(
                    &path,
                    value,
                    "tracked or configuredPaths",
                    matches!(value.as_str(), Some("tracked" | "configuredPaths")),
                )?,
                "gitSync.outputBehavior" => require_kind(
                    &path,
                    value,
                    "onStart, onError, or silent",
                    matches!(value.as_str(), Some("onStart" | "onError" | "silent")),
                )?,
                "projectRoot"
                | "snapshotDir"
                | "server"
                | "configTomlPath"
                | "exportCliPath"
                | "rustCliPath"
                | "bridgePorts"
                | "place"
                | "daemon"
                | "gitSync.gitPath"
                | "gitSync.remote"
                | "gitSync.branch"
                | "gitSync.commitMessageTemplate"
                | "wallySync.wallyPath"
                | "wallySync.rojoPath"
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
                | "link.gitPath" => require_kind(&path, value, "a string", value.is_string())?,
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
            allowed = rule.action == FilterAction::Include;
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

pub(crate) fn validate_project(loaded: &LoadedProject) -> Result<()> {
    if loaded.project.schema_version != PROJECT_SCHEMA_VERSION {
        bail!(
            "{} uses schema version {}; this Renium build supports {}",
            loaded.path.display(),
            loaded.project.schema_version,
            PROJECT_SCHEMA_VERSION
        );
    }
    validate_relative_portable_path(&loaded.project.source_root, "sourceRoot")?;
    if let Some(target) = loaded.project.build_target.as_ref() {
        validate_instance_target(target, "buildTarget")?;
    }
    validate_project_node_reserved_keys(&loaded.project.root, "root")?;
    let root = &loaded.project.root;
    if root
        .class_name
        .as_deref()
        .is_some_and(|class_name| class_name != "DataModel")
    {
        bail!("Top-level root $className must be DataModel");
    }
    if root.id.is_some()
        || root.path.is_some()
        || !root.properties.is_empty()
        || !root.attributes.is_empty()
        || root.tags.is_some()
        || !root.children.is_empty()
    {
        bail!(
            "Top-level DataModel root metadata is not supported; configure services under tree instead"
        );
    }
    for (name, node) in &loaded.project.tree {
        validate_project_node_reserved_keys(node, name)?;
    }
    for (field, suffix) in [
        (
            "exportNaming.serverSuffix",
            loaded.project.export_naming.server_suffix.as_str(),
        ),
        (
            "exportNaming.clientSuffix",
            loaded.project.export_naming.client_suffix.as_str(),
        ),
        (
            "exportNaming.moduleSuffix",
            loaded.project.export_naming.module_suffix.as_str(),
        ),
        (
            "exportNaming.pluginSuffix",
            loaded.project.export_naming.plugin_suffix.as_str(),
        ),
        (
            "exportNaming.clientRunContextSuffix",
            loaded
                .project
                .export_naming
                .client_run_context_suffix
                .as_str(),
        ),
    ] {
        if suffix.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
                )
        }) {
            bail!("{field} contains characters that are invalid in portable file names");
        }
    }
    if loaded.project.export_naming.server_suffix.is_empty()
        || loaded.project.export_naming.client_suffix.is_empty()
        || loaded.project.export_naming.plugin_suffix.is_empty()
        || loaded
            .project
            .export_naming
            .client_run_context_suffix
            .is_empty()
    {
        bail!("exportNaming script suffixes other than moduleSuffix cannot be empty");
    }
    let suffixes = [
        loaded.project.export_naming.server_suffix.as_str(),
        loaded.project.export_naming.client_suffix.as_str(),
        loaded.project.export_naming.module_suffix.as_str(),
        loaded.project.export_naming.plugin_suffix.as_str(),
        loaded
            .project
            .export_naming
            .client_run_context_suffix
            .as_str(),
    ];
    for left in 0..suffixes.len() {
        for right in left + 1..suffixes.len() {
            if suffixes[left].eq_ignore_ascii_case(suffixes[right]) {
                bail!("exportNaming suffixes must be distinct");
            }
        }
    }
    let mut mount_targets = Vec::<(&ProjectTarget, &Path, MountOwnership)>::new();
    for (index, mount) in loaded.project.mounts.iter().enumerate() {
        validate_relative_portable_path(&mount.source, &format!("mounts[{index}].source"))?;
        let mount_source = loaded.root.join(&mount.source);
        if mount_source.exists() || !mount.optional {
            validate_direct_owner_source(&mount_source, &format!("mounts[{index}].source"))?;
        }
        validate_instance_target(&mount.target, &format!("mounts[{index}].target"))?;
        validate_filesystem_target(&mount.target, &format!("mounts[{index}].target"))?;
        for (target, source, _) in &mount_targets {
            if instance_target_overlaps(target, &mount.target) {
                bail!(
                    "Mounts {} and {} overlap at Studio targets '{}' and '{}'; nested mount targets require explicit ownership provenance and are not supported",
                    source.display(),
                    mount.source.display(),
                    target,
                    mount.target
                );
            }
        }
        mount_targets.push((&mount.target, &mount.source, mount.ownership));
    }
    let mut outputs = BTreeSet::new();
    let mut adapter_targets = Vec::<(&ProjectTarget, &Path)>::new();
    let mut reverse_adapter_sources = BTreeMap::<PathBuf, ProjectTarget>::new();
    let tree_nodes = project_tree_nodes(&loaded.project.tree);
    for (target, node) in &tree_nodes {
        if let Some(source) = node.path.as_deref() {
            validate_direct_owner_source(
                &loaded.root.join(source),
                &format!("tree target {}", target.join(".")),
            )?;
        }
    }
    let writable_tree_targets = tree_nodes
        .into_iter()
        .filter_map(|(target, node)| {
            node.path.map(|_| ProjectTarget::Structured {
                segments: target,
                ordinals: Vec::new(),
            })
        })
        .collect::<Vec<_>>();
    for (index, target) in writable_tree_targets.iter().enumerate() {
        validate_filesystem_target(target, &format!("tree writable target {index}"))?;
    }
    for left in 0..writable_tree_targets.len() {
        for right in left + 1..writable_tree_targets.len() {
            if instance_target_overlaps(&writable_tree_targets[left], &writable_tree_targets[right])
            {
                bail!(
                    "Writable tree targets '{}' and '{}' overlap",
                    writable_tree_targets[left],
                    writable_tree_targets[right]
                );
            }
        }
    }
    for tree_target in &writable_tree_targets {
        for (mount_target, mount_source, _) in &mount_targets {
            if instance_target_overlaps(tree_target, mount_target) {
                bail!(
                    "Writable tree target '{}' overlaps mount {} at '{}'",
                    tree_target,
                    mount_source.display(),
                    mount_target
                );
            }
        }
    }
    for (index, adapter) in loaded.project.adapters.iter().enumerate() {
        validate_relative_portable_path(&adapter.source, &format!("adapters[{index}].source"))?;
        validate_instance_target(&adapter.target, &format!("adapters[{index}].target"))?;
        validate_filesystem_target(&adapter.target, &format!("adapters[{index}].target"))?;
        for (target, source) in &adapter_targets {
            if instance_target_overlaps(target, &adapter.target) {
                bail!(
                    "Adapters {} and {} have overlapping Studio targets '{}' and '{}'",
                    source.display(),
                    adapter.source.display(),
                    target,
                    adapter.target
                );
            }
        }
        if writable_tree_targets
            .iter()
            .any(|target| instance_target_overlaps(target, &adapter.target))
            || mount_targets
                .iter()
                .any(|(target, _, _)| instance_target_overlaps(target, &adapter.target))
        {
            bail!(
                "Adapter {} overlaps a tree or mount owner at '{}'",
                adapter.source.display(),
                adapter.target
            );
        }
        adapter_targets.push((&adapter.target, &adapter.source));
        if let Some(output) = adapter.output.as_deref() {
            validate_relative_portable_path(output, &format!("adapters[{index}].output"))?;
            if !outputs.insert(path_slash(output).to_ascii_lowercase()) {
                bail!("More than one adapter writes {}", output.display());
            }
            if loaded.project.adapters.iter().any(|other| {
                projection_path_key(&loaded.root.join(&other.source))
                    == projection_path_key(&loaded.root.join(output))
            }) {
                bail!(
                    "Adapter output {} collides with an adapter source",
                    output.display()
                );
            }
        }
        let format = adapter_format(adapter)?;
        if adapter.direction != AdapterDirection::ToProject
            && !matches!(format.as_str(), "txt" | "csv" | "model-json")
        {
            bail!(
                "Adapter {} uses {} in {:?}; this format is not reversible and must use to-project",
                adapter.source.display(),
                format,
                adapter.direction
            );
        }
        if format == "model-json" && target_segments(&adapter.target)?.len() < 2 {
            bail!(
                "Model JSON adapter {} must target a child below a Studio service",
                adapter.source.display()
            );
        }
        if adapter.direction == AdapterDirection::ToProject
            && matches!(
                format.as_str(),
                "json" | "jsonc" | "toml" | "yaml" | "msgpack" | "markdown"
            )
            && !adapter.generated
        {
            bail!(
                "Adapter {} uses a generated {} projection; set generated to true",
                adapter.source.display(),
                format
            );
        }
        if adapter.direction != AdapterDirection::ToProject
            && mount_targets.iter().any(|(target, _, ownership)| {
                *ownership == MountOwnership::ReadOnly
                    && instance_target_overlaps(target, &adapter.target)
            })
        {
            bail!(
                "Adapter {} cannot sync back through a read-only mount",
                adapter.source.display()
            );
        }
        if adapter.direction != AdapterDirection::ToProject {
            let source = absolute_path(&loaded.root.join(&adapter.source));
            if let Some(previous) = reverse_adapter_sources.insert(source, adapter.target.clone()) {
                bail!(
                    "Adapters '{}' and '{}' both sync back to {}",
                    previous,
                    adapter.target,
                    adapter.source.display()
                );
            }
        }
    }
    for (index, rule) in loaded.project.sync_rules.iter().enumerate() {
        compile_glob(&rule.pattern)
            .with_context(|| format!("Invalid syncRules[{index}].pattern '{}'", rule.pattern))?;
        if let Some(exclude) = rule.exclude.as_deref() {
            compile_glob(exclude)
                .with_context(|| format!("Invalid syncRules[{index}].exclude '{exclude}'"))?;
        }
        validate_sync_middleware(&rule.middleware)
            .with_context(|| format!("Invalid syncRules[{index}].use"))?;
        if let Some(suffix) = rule.suffix.as_deref()
            && (suffix.is_empty()
                || suffix.contains('/')
                || suffix.contains('\\')
                || suffix.chars().any(char::is_control))
        {
            bail!("syncRules[{index}].suffix must be a non-empty file-name suffix");
        }
    }
    for (index, pattern) in loaded.project.glob_ignore_paths.iter().enumerate() {
        let pattern = ignore_glob_pattern(pattern)?;
        compile_glob(pattern)
            .with_context(|| format!("Invalid globIgnorePaths[{index}] '{pattern}'"))?;
    }
    for (index, rule) in loaded.project.filters.iter().enumerate() {
        if let Some(pattern) = rule.glob.as_deref() {
            compile_glob(pattern)
                .with_context(|| format!("Invalid filters[{index}].glob '{pattern}'"))?;
        }
        if rule.glob.is_none()
            && rule.name.is_none()
            && rule.class.is_none()
            && rule.tag.is_none()
            && rule.attribute.is_none()
            && rule.property.is_none()
            && rule.id.is_none()
        {
            bail!("filters[{index}] has no matching condition");
        }
    }
    let mut explicit_targets = writable_tree_targets
        .iter()
        .map(ProjectTarget::segments)
        .chain(mount_targets.iter().map(|(target, _, _)| target.segments()))
        .chain(
            loaded
                .project
                .adapters
                .iter()
                .filter(|adapter| adapter.direction != AdapterDirection::FromProject)
                .map(|adapter| adapter.target.segments()),
        )
        .collect::<Vec<_>>();
    explicit_targets.sort();
    explicit_targets.dedup();
    let source_root = loaded.root.join(&loaded.project.source_root);
    let source_naming = project_script_naming(&loaded.project);
    let claimed_sources = projection_source_owner_paths(loaded);
    let mut source_documents = HashMap::new();
    for target in explicit_targets {
        let Some(service) = target.first() else {
            continue;
        };
        let service_dir = source_root.join(service);
        if !service_dir.is_dir() {
            continue;
        }
        if !source_documents.contains_key(service) {
            source_documents.insert(
                service.clone(),
                crate::source_structure_settings_document(
                    &service_dir,
                    service,
                    &source_naming,
                    &claimed_sources,
                )?,
            );
        }
        if find_document_target_optional(&source_documents[service], &target)?.is_some() {
            bail!(
                "Explicit project owner '{}' overlaps content already projected from sourceRoot; move or remove one owner",
                target.join(".")
            );
        }
    }
    let mut reverse_sources = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(target, node)| {
            node.path.map(|source| {
                (
                    format!("tree {}", target.join(".")),
                    absolute_path(&loaded.root.join(source)),
                )
            })
        })
        .chain(
            loaded
                .project
                .mounts
                .iter()
                .filter(|mount| mount.ownership != MountOwnership::ReadOnly)
                .map(|mount| {
                    (
                        format!("mount {}", mount.target),
                        absolute_path(&loaded.root.join(&mount.source)),
                    )
                }),
        )
        .chain(
            loaded
                .project
                .adapters
                .iter()
                .filter(|adapter| adapter.direction != AdapterDirection::ToProject)
                .map(|adapter| {
                    (
                        format!("adapter {}", adapter.target),
                        absolute_path(&loaded.root.join(&adapter.source)),
                    )
                }),
        )
        .collect::<Vec<_>>();
    reverse_sources.sort_by_key(|entry| projection_path_key(&entry.1));
    for left in 0..reverse_sources.len() {
        for right in left + 1..reverse_sources.len() {
            let (left_name, left_path) = &reverse_sources[left];
            let (right_name, right_path) = &reverse_sources[right];
            if projection_path_contains(left_path, right_path)
                || projection_path_contains(right_path, left_path)
            {
                bail!(
                    "{left_name} and {right_name} have overlapping writable source paths '{}' and '{}'",
                    left_path.display(),
                    right_path.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_nested_project(loaded: &LoadedProject) -> Result<()> {
    if loaded.project.root.path.is_some() || !loaded.project.root.children.is_empty() {
        bail!(
            "Nested project root $path and child nodes are unsupported; put mounted content in sourceRoot or tree"
        );
    }
    let root_class = loaded
        .project
        .root
        .class_name
        .as_deref()
        .unwrap_or("Folder");
    normalize_property_map(Some(root_class), &loaded.project.root.properties).with_context(
        || {
            format!(
                "Invalid nested root properties in {}",
                loaded.path.display()
            )
        },
    )?;
    normalize_property_map(None, &loaded.project.root.attributes).with_context(|| {
        format!(
            "Invalid nested root attributes in {}",
            loaded.path.display()
        )
    })?;
    let mut top_level = LoadedProject {
        path: loaded.path.clone(),
        root: loaded.root.clone(),
        project: loaded.project.clone(),
    };
    top_level.project.root = ProjectNode::default();
    validate_project(&top_level)
}

fn validate_project_node_reserved_keys(node: &ProjectNode, path: &str) -> Result<()> {
    for (name, value) in &node.children {
        if name.starts_with('$') {
            bail!("Unknown reserved project-node key '{name}' at '{path}'");
        }
        let child: ProjectNode = serde_json::from_value(value.clone())
            .with_context(|| format!("Project tree node '{path}.{name}' must be an object"))?;
        validate_project_node_reserved_keys(&child, &format!("{path}.{name}"))?;
    }
    Ok(())
}

fn compile_projection(loaded: &LoadedProject) -> Result<CompiledProjection> {
    let mut entries = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        if let Some(path) = node.path.as_deref() {
            entries.push(projection_entry(
                "tree",
                &path_slash(path),
                &target.join("."),
                None,
                None,
            ));
        }
    }
    for mount in &loaded.project.mounts {
        let target = mount.target.key();
        entries.push(projection_entry(
            "mount",
            &path_slash(&mount.source),
            &target,
            Some(mount.ownership),
            None,
        ));
    }
    for adapter in &loaded.project.adapters {
        let target = adapter.target.key();
        entries.push(projection_entry(
            "adapter",
            &path_slash(&adapter.source),
            &target,
            None,
            Some(adapter.direction),
        ));
    }
    for rule in &loaded.project.sync_rules {
        entries.push(projection_entry(
            "sync-rule",
            &rule.pattern,
            &rule.middleware,
            None,
            None,
        ));
    }
    for pattern in &loaded.project.glob_ignore_paths {
        entries.push(projection_entry("ignore", pattern, "", None, None));
    }
    entries.sort_by(|a, b| (&a.target, &a.source).cmp(&(&b.target, &b.source)));
    Ok(CompiledProjection {
        schema_version: PROJECT_SCHEMA_VERSION,
        project: loaded.path.display().to_string(),
        entries,
    })
}

fn projection_entry(
    kind: &str,
    source: &str,
    target: &str,
    ownership: Option<MountOwnership>,
    direction: Option<AdapterDirection>,
) -> ProjectionEntry {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(source.as_bytes());
    digest.update([0]);
    digest.update(target.as_bytes());
    let id = format!("projection:{:x}", digest.finalize());
    ProjectionEntry {
        id,
        kind: kind.to_string(),
        source: source.to_string(),
        target: target.to_string(),
        ownership,
        direction,
    }
}

pub fn project_requires_temporary_stage(loaded: &LoadedProject) -> Result<bool> {
    Ok(!(loaded.project.tree.is_empty()
        && loaded.project.mounts.is_empty()
        && loaded.project.adapters.is_empty()
        && loaded.project.sync_rules.is_empty()
        && loaded.project.glob_ignore_paths.is_empty()
        && !contains_metadata_sidecars(&loaded.root.join(&loaded.project.source_root))?))
}

fn fresh_projection_stage(parent: &Path, prefix: &str) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    for _ in 0..1_000 {
        let sequence = PROJECTION_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = parent.join(format!(
            "{prefix}{}-{}-{sequence}",
            std::process::id(),
            crate::current_millis()
        ));
        match fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to create projection stage {}", root.display())
                });
            }
        }
    }
    bail!("Could not allocate a fresh projection stage")
}

pub fn stage_project(loaded: &LoadedProject) -> Result<ProjectionStage> {
    validate_project(loaded)?;
    if !project_requires_temporary_stage(loaded)? {
        cache_script_naming(
            &loaded.root.join(&loaded.project.source_root),
            &loaded.project,
        );
        return Ok(ProjectionStage {
            root: loaded.root.join(&loaded.project.source_root),
            temporary: false,
            cleanup: false,
            transforms: Vec::new(),
            identities: HashMap::new(),
        });
    }

    let root = fresh_projection_stage(&loaded.root.join(".renium").join("build-staging"), "")?;
    PROJECTION_TRANSFORM_STACK.with(|stack| stack.borrow_mut().push(Vec::new()));
    PROJECTION_IDENTITY_STACK.with(|stack| stack.borrow_mut().push(HashMap::new()));
    let result = (|| {
        cache_script_naming(&root, &loaded.project);
        let source_root = loaded.root.join(&loaded.project.source_root);
        if source_root.is_dir() {
            stage_source_directory(loaded, &root, &source_root, &root, false, None)?;
        }
        for mount in &loaded.project.mounts {
            stage_mount(loaded, &root, mount)?;
        }
        for (service, node) in &loaded.project.tree {
            stage_tree_node(loaded, &root, std::slice::from_ref(service), node)?;
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction != AdapterDirection::FromProject {
                stage_adapter(loaded, &root, adapter)?;
            }
        }
        refresh_stage_settings(&root)?;
        normalize_stage_references(&root)?;
        Ok(())
    })();
    let transforms = PROJECTION_TRANSFORM_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection transform stack is balanced")
    });
    let identities = PROJECTION_IDENTITY_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection identity stack is balanced")
    });
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&root);
        remove_empty_stage_parents(&root);
        return Err(error);
    }
    Ok(ProjectionStage {
        root,
        temporary: true,
        cleanup: true,
        transforms,
        identities,
    })
}

pub fn stage_project_cached(
    loaded: &LoadedProject,
    changed_sources: &[PathBuf],
) -> Result<ProjectionStage> {
    let project_hash = bytes_hash(&serde_json::to_vec(&loaded.project)?);
    let key = fs::canonicalize(&loaded.path).unwrap_or_else(|_| absolute_path(&loaded.path));
    let cache = PROJECTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reusable = cache
        .get(&key)
        .is_some_and(|entry| entry.project_hash == project_hash && entry.root.is_dir());
    let source_shape_changed = reusable
        && cache.get(&key).is_some_and(|entry| {
            changed_sources.iter().any(|source| {
                entry
                    .source_shape
                    .get(&projection_path_key(source))
                    .copied()
                    != projection_source_kind(source)
            })
        });
    if source_shape_changed {
        validate_project(loaded)?;
    }
    let mut created = false;
    if !reusable {
        if let Some(previous) = cache.remove(&key) {
            remove_cached_script_naming(&previous.root);
            let _ = fs::remove_dir_all(previous.root);
        }
        let Some(entry) = create_cached_projection(loaded, &key, project_hash.clone())? else {
            drop(cache);
            return stage_project(loaded);
        };
        cache.insert(key.clone(), entry);
        created = true;
    }
    let entry = cache
        .get_mut(&key)
        .context("Projection cache entry disappeared")?;
    if created {
        return Ok(ProjectionStage {
            root: entry.root.clone(),
            temporary: true,
            cleanup: false,
            transforms: entry.transforms.clone(),
            identities: entry.identities.clone(),
        });
    }
    let mut services = BTreeSet::new();
    let mut rebuild_all = false;
    for source in changed_sources {
        if absolute_path(source) == absolute_path(&loaded.path) {
            rebuild_all = true;
            break;
        }
        let relatives = project_source_to_staged_relatives(loaded, source)?;
        if relatives.is_empty() {
            rebuild_all = true;
            break;
        }
        for relative in relatives {
            let Some(Component::Normal(service)) = relative.components().next() else {
                rebuild_all = true;
                break;
            };
            let Some(service) = service.to_str() else {
                rebuild_all = true;
                break;
            };
            services.insert(service.to_string());
        }
        if rebuild_all {
            break;
        }
    }
    if rebuild_all {
        let previous = cache
            .remove(&key)
            .context("Projection cache entry disappeared")?;
        remove_cached_script_naming(&previous.root);
        let _ = fs::remove_dir_all(previous.root);
        let Some(entry) = create_cached_projection(loaded, &key, project_hash)? else {
            drop(cache);
            return stage_project(loaded);
        };
        cache.insert(key.clone(), entry);
    }
    let entry = cache
        .get_mut(&key)
        .context("Projection cache entry disappeared")?;
    if !changed_sources.is_empty()
        && patch_cached_projection_scripts(loaded, &entry.root, changed_sources)?
    {
        if source_shape_changed {
            entry.source_shape = projection_source_shape(loaded)?;
        }
        return Ok(ProjectionStage {
            root: entry.root.clone(),
            temporary: true,
            cleanup: false,
            transforms: entry.transforms.clone(),
            identities: entry.identities.clone(),
        });
    }
    if !rebuild_all && !services.is_empty() {
        match rebuild_cached_projection_services(
            loaded,
            &entry.root,
            &services,
            &entry.transforms,
            &entry.identities,
        ) {
            Ok((transforms, identities)) => {
                entry.transforms = transforms;
                entry.identities = identities;
                if source_shape_changed {
                    entry.source_shape = projection_source_shape(loaded)?;
                }
            }
            Err(error) => {
                let root = entry.root.clone();
                cache.remove(&key);
                remove_cached_script_naming(&root);
                let _ = fs::remove_dir_all(root);
                return Err(error);
            }
        }
    }
    Ok(ProjectionStage {
        root: entry.root.clone(),
        temporary: true,
        cleanup: false,
        transforms: entry.transforms.clone(),
        identities: entry.identities.clone(),
    })
}

fn patch_cached_projection_scripts(
    loaded: &LoadedProject,
    cache_root: &Path,
    changed_sources: &[PathBuf],
) -> Result<bool> {
    let mut writes = Vec::new();
    for source in changed_sources {
        if !source.is_file()
            || !source
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| matches!(extension, "lua" | "luau"))
        {
            return Ok(false);
        }
        let relatives = project_source_to_staged_relatives(loaded, source)?;
        if relatives.is_empty() {
            return Ok(false);
        }
        let bytes = fs::read(source)?;
        for relative in relatives {
            if !relative
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| matches!(extension, "lua" | "luau"))
            {
                return Ok(false);
            }
            let destination = cache_root.join(relative);
            if !destination.is_file() {
                return Ok(false);
            }
            writes.push((destination, bytes.clone()));
        }
    }
    write_file_transaction(&writes)?;
    Ok(true)
}

fn create_cached_projection(
    loaded: &LoadedProject,
    key: &Path,
    project_hash: String,
) -> Result<Option<CachedProjection>> {
    let staged = stage_project(loaded)?;
    if !staged.is_temporary() {
        return Ok(None);
    }
    let mut digest = Sha256::new();
    digest.update(key.to_string_lossy().as_bytes());
    let cache_root = env::temp_dir()
        .join("renium-projection-cache")
        .join(format!("{}-{:x}", std::process::id(), digest.finalize()));
    if cache_root.exists() {
        fs::remove_dir_all(&cache_root)?;
    }
    if let Some(parent) = cache_root.parent() {
        fs::create_dir_all(parent)?;
    }
    let staged_root = staged.root().to_path_buf();
    if fs::rename(&staged_root, &cache_root).is_err() {
        copy_directory_tree(&staged_root, &cache_root)?;
    }
    relocate_cached_script_naming(&staged_root, &cache_root);
    Ok(Some(CachedProjection {
        root: cache_root,
        project_hash,
        source_shape: projection_source_shape(loaded)?,
        transforms: staged.transforms.clone(),
        identities: staged.identities.clone(),
    }))
}

fn projection_source_kind(path: &Path) -> Option<u8> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some(if metadata.file_type().is_symlink() {
        3
    } else if metadata.is_dir() {
        2
    } else if metadata.is_file() {
        1
    } else {
        4
    })
}

fn projection_source_shape(loaded: &LoadedProject) -> Result<HashMap<String, u8>> {
    let mut shape = HashMap::new();
    for root in project_source_roots(loaded)? {
        if let Some(kind) = projection_source_kind(&root) {
            shape.insert(projection_path_key(&root), kind);
        }
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .min_depth(1)
            .follow_links(false)
        {
            let entry = entry?;
            let kind = if entry.file_type().is_symlink() {
                3
            } else if entry.file_type().is_dir() {
                2
            } else if entry.file_type().is_file() {
                1
            } else {
                4
            };
            shape.insert(projection_path_key(entry.path()), kind);
        }
    }
    Ok(shape)
}

fn copy_directory_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in walkdir::WalkDir::new(source).min_depth(1) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn rebuild_cached_projection_services(
    loaded: &LoadedProject,
    root: &Path,
    services: &BTreeSet<String>,
    previous_transforms: &[ProjectionTransform],
    previous_identities: &HashMap<String, ProjectionIdentity>,
) -> Result<(
    Vec<ProjectionTransform>,
    HashMap<String, ProjectionIdentity>,
)> {
    let mut replaced_staged_ids = HashSet::new();
    for service in services {
        let destination = root.join(service);
        let settings = crate::existing_service_settings_path(&destination);
        if settings.is_file() {
            replaced_staged_ids.extend(
                SettingsBytecode::read_file(&settings)?
                    .instances
                    .into_iter()
                    .map(|instance| instance.settings_id),
            );
        }
        if destination.exists() {
            remove_cached_script_naming(&destination);
            fs::remove_dir_all(&destination)?;
        }
    }
    PROJECTION_TRANSFORM_STACK.with(|stack| stack.borrow_mut().push(Vec::new()));
    PROJECTION_IDENTITY_STACK.with(|stack| stack.borrow_mut().push(HashMap::new()));
    let result = (|| {
        for service in services {
            let source = loaded.root.join(&loaded.project.source_root).join(service);
            if source.is_dir() {
                stage_source_directory(
                    loaded,
                    root,
                    &source,
                    &root.join(service),
                    false,
                    Some(Path::new(service)),
                )?;
            }
        }
        for mount in &loaded.project.mounts {
            let target = target_segments(&mount.target)?;
            if target
                .first()
                .is_some_and(|service| services.contains(service))
            {
                stage_mount(loaded, root, mount)?;
            }
        }
        for (service, node) in &loaded.project.tree {
            if services.contains(service) {
                stage_tree_node(loaded, root, std::slice::from_ref(service), node)?;
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let target = target_segments(&adapter.target)?;
            if target
                .first()
                .is_some_and(|service| services.contains(service))
            {
                stage_adapter(loaded, root, adapter)?;
            }
        }
        for service in services {
            let service_dir = root.join(service);
            if service_dir.is_dir() {
                refresh_stage_service_settings(&service_dir)?;
            }
        }
        normalize_stage_references(root)
    })();
    let refreshed = PROJECTION_TRANSFORM_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection transform stack is balanced")
    });
    let refreshed_identities = PROJECTION_IDENTITY_STACK.with(|stack| {
        stack
            .borrow_mut()
            .pop()
            .expect("projection identity stack is balanced")
    });
    result?;
    let mut transforms = previous_transforms
        .iter()
        .filter(|transform| {
            transform
                .target
                .first()
                .is_none_or(|service| !services.contains(service))
        })
        .cloned()
        .collect::<Vec<_>>();
    transforms.extend(refreshed);
    transforms.sort_by(|left, right| left.target.cmp(&right.target));
    let mut identities = previous_identities
        .iter()
        .filter(|(staged_id, _)| !replaced_staged_ids.contains(*staged_id))
        .map(|(staged_id, identity)| (staged_id.clone(), identity.clone()))
        .collect::<HashMap<_, _>>();
    identities.extend(refreshed_identities);
    Ok((transforms, identities))
}

fn stage_tree_node(
    loaded: &LoadedProject,
    stage: &Path,
    target: &[String],
    node: &ProjectNode,
) -> Result<()> {
    let target_path = target_fs_path(stage, target);
    if let Some(source) = node.path.as_deref() {
        let source = loaded.root.join(source);
        if source.is_dir() {
            stage_source_directory(loaded, stage, &source, &target_path, true, None)?;
            let settings = crate::existing_service_settings_path(&source);
            if settings.is_file() {
                merge_settings_document_at_target(stage, target, &settings)?;
            }
        } else if source.is_file() {
            if is_nested_project_path(&source) {
                let nested = load_nested_project(&source)?;
                stage_nested_project_at_target(&nested, stage, target)?;
            } else {
                copy_file_to_target(loaded, &source, &target_path)?;
            }
        } else {
            bail!("Project tree source does not exist: {}", source.display());
        }
    } else {
        fs::create_dir_all(&target_path)?;
    }
    if node.id.is_some()
        || node.class_name.is_some()
        || !node.properties.is_empty()
        || !node.attributes.is_empty()
        || node.tags.is_some()
    {
        let inferred_class;
        let class_name = if let Some(class_name) = node.class_name.as_deref() {
            class_name
        } else if target.len() == 1 {
            target[0].as_str()
        } else {
            let service = target
                .first()
                .context("Project tree target must include a service")?;
            let settings = crate::existing_service_settings_path(&stage.join(service));
            inferred_class = if settings.is_file() {
                let document = SettingsBytecode::read_file(&settings)?;
                find_document_target_optional(&document, target)?
                    .map(|index| document.instances[index].class_name.clone())
            } else {
                None
            };
            inferred_class.as_deref().unwrap_or("Folder")
        };
        let properties = normalize_property_map(Some(class_name), &node.properties)
            .with_context(|| format!("Invalid properties on '{}'", target.join(".")))?;
        let attributes = normalize_property_map(None, &node.attributes)
            .with_context(|| format!("Invalid attributes on '{}'", target.join(".")))?;
        override_stage_identity(
            stage,
            target,
            node.class_name.as_deref(),
            node.id.as_deref(),
        )?;
        update_stage_instance(
            stage,
            target,
            class_name,
            node.id.as_deref(),
            &properties,
            &attributes,
            node.tags.as_deref(),
        )?;
    }
    for (name, value) in &node.children {
        if name.starts_with('$') {
            continue;
        }
        let child: ProjectNode = serde_json::from_value(value.clone()).with_context(|| {
            format!("Project tree node '{}' must be an object", target.join("."))
        })?;
        let mut child_target = target.to_vec();
        child_target.push(name.clone());
        stage_tree_node(loaded, stage, &child_target, &child)?;
    }
    Ok(())
}

fn stage_mount(loaded: &LoadedProject, stage: &Path, mount: &ProjectMount) -> Result<()> {
    with_project_target(&mount.target, |target| {
        stage_mount_target(loaded, stage, mount, target.to_vec())
    })
}

fn stage_mount_target(
    loaded: &LoadedProject,
    stage: &Path,
    mount: &ProjectMount,
    target: Vec<String>,
) -> Result<()> {
    let source = loaded.root.join(&mount.source);
    if !source.exists() && mount.optional {
        return Ok(());
    }
    if source.is_dir() {
        let destination = target_fs_path(stage, &target);
        fs::create_dir_all(&destination)?;
        stage_source_directory(loaded, stage, &source, &destination, true, None)?;
        let settings = crate::existing_service_settings_path(&source);
        if settings.is_file() {
            merge_settings_document_at_target(stage, &target, &settings)?;
        } else {
            update_stage_instance(
                stage,
                &target,
                "Folder",
                None,
                &Map::new(),
                &Map::new(),
                None,
            )?;
        }
        return Ok(());
    }
    let name = source
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.ends_with(".project.json") || name.ends_with(".project.jsonc") {
        let nested = load_nested_project(&source)?;
        stage_nested_project_at_target(&nested, stage, &target)?;
        return Ok(());
    }
    if matches!(
        source.extension().and_then(OsStr::to_str),
        Some("rbxm" | "rbxmx")
    ) {
        import_model_at_target(stage, &target, &source)?;
        return Ok(());
    }
    if source
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "renium" | "rbsync"))
    {
        merge_settings_mount(stage, &target, &source)?;
        return Ok(());
    }
    if source.is_file() {
        copy_file_to_target(loaded, &source, &target_fs_path(stage, &target))?;
        return Ok(());
    }
    bail!("Mount source does not exist: {}", source.display())
}

fn stage_adapter(loaded: &LoadedProject, stage: &Path, adapter: &AdapterSpec) -> Result<()> {
    with_project_target(&adapter.target, |target| {
        stage_adapter_target(loaded, stage, adapter, target)
    })
}

fn stage_adapter_target(
    loaded: &LoadedProject,
    stage: &Path,
    adapter: &AdapterSpec,
    target: &[String],
) -> Result<()> {
    let source = loaded.root.join(&adapter.source);
    let format = adapter_format(adapter)?;
    validate_adapter_source(&source, &format)?;
    match format.as_str() {
        "txt" => stage_text_value(stage, target, &source),
        "csv" => stage_localization_table(stage, target, &source),
        "model-json" => stage_model_json(stage, target, &source),
        "rbxm" | "rbxmx" => import_model_at_target(stage, target, &source),
        "nested-project" => {
            let nested = load_nested_project(&source)?;
            stage_nested_project_at_target(&nested, stage, target)
        }
        _ => stage_module_data(loaded, stage, target, &source, &format),
    }
}

fn stage_text_value(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    let value =
        fs::read_to_string(source).with_context(|| format!("{} is not UTF-8", source.display()))?;
    let properties = Map::from_iter([("Value".to_string(), Value::String(value))]);
    update_stage_instance(
        stage,
        target,
        "StringValue",
        None,
        &properties,
        &Map::new(),
        None,
    )
}

fn stage_localization_table(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    let value = localization_csv_to_json(&fs::read_to_string(source)?)?;
    let properties = Map::from_iter([("Contents".to_string(), Value::String(value))]);
    update_stage_instance(
        stage,
        target,
        "LocalizationTable",
        None,
        &properties,
        &Map::new(),
        None,
    )
}

fn stage_module_data(
    loaded: &LoadedProject,
    stage: &Path,
    target: &[String],
    source: &Path,
    format: &str,
) -> Result<()> {
    let bytes = render_adapter(source, format)?;
    let output = adapter_target_script_path(loaded, stage, target);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&output, &bytes)?;
    let source = String::from_utf8(bytes).context("Generated adapter source is not UTF-8")?;
    let properties = Map::from_iter([("Source".to_string(), Value::String(source))]);
    update_stage_instance(
        stage,
        target,
        "ModuleScript",
        None,
        &properties,
        &Map::new(),
        None,
    )
}

fn update_stage_instance(
    stage: &Path,
    target: &[String],
    class_name: &str,
    explicit_id: Option<&str>,
    properties: &Map<String, Value>,
    attributes: &Map<String, Value>,
    tags: Option<&[String]>,
) -> Result<()> {
    let ordinals = active_target_ordinals(target);
    let service = target
        .first()
        .context("Projection target must include a service")?;
    if ordinals.first().copied().unwrap_or(1) != 1 {
        bail!("Projection service roots always have ordinal 1");
    }
    let service_dir = stage.join(service);
    fs::create_dir_all(&service_dir)?;
    let settings_path = crate::writable_service_settings_path(&service_dir)?;
    let mut document = if settings_path.is_file() {
        SettingsBytecode::read_file(&settings_path)?
    } else {
        crate::source_only_settings_document(&service_dir, service)?
    };
    let mut parent_index = 0usize;
    for (position, name) in target.iter().enumerate().skip(1) {
        let final_node = position + 1 == target.len();
        let expected_class = if final_node { class_name } else { "Folder" };
        let matches = document
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| {
                instance.parent_index == Some(parent_index) && instance.name == *name
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let ordinal = ordinals.get(position).copied();
        if ordinal.is_none() && matches.len() > 1 {
            bail!(
                "Projection target '{}' is ambiguous because '{}' has duplicate children",
                target.join("."),
                name
            );
        }
        let selected = ordinal.unwrap_or(1);
        if selected > matches.len() + 1 {
            bail!(
                "Projection target '{}' cannot create ordinal {} for '{}' before ordinal {} exists",
                target.join("."),
                selected,
                name,
                selected - 1
            );
        }
        let index = if let Some(index) = matches.get(selected - 1).copied() {
            if final_node && document.instances[index].class_name != expected_class {
                if document.instances[index].class_name == "Folder"
                    && matches!(
                        expected_class,
                        "Script"
                            | "LocalScript"
                            | "ModuleScript"
                            | "StringValue"
                            | "LocalizationTable"
                    )
                {
                    document.instances[index].class_name = expected_class.to_string();
                } else {
                    bail!(
                        "Projection target '{}' already exists as {}, expected {}",
                        target.join("."),
                        document.instances[index].class_name,
                        expected_class
                    );
                }
            }
            index
        } else {
            let identity = target
                .iter()
                .enumerate()
                .take(position + 1)
                .map(|(index, segment)| {
                    format!("{}[{}]", segment, ordinals.get(index).copied().unwrap_or(1))
                })
                .collect::<Vec<_>>()
                .join(".");
            let settings_id = if final_node {
                explicit_id
                    .map(str::to_string)
                    .unwrap_or_else(|| projection_settings_id("instance", &identity))
            } else {
                projection_settings_id("folder", &identity)
            };
            let index = document.instances.len();
            document.instances.push(SettingsBytecodeInstance {
                settings_id,
                name: name.clone(),
                class_name: expected_class.to_string(),
                parent_index: Some(parent_index),
                properties: Map::new(),
                attributes: Map::new(),
            });
            index
        };
        parent_index = index;
    }
    let target_index = if target.len() == 1 { 0 } else { parent_index };
    let instance = &mut document.instances[target_index];
    if target.len() == 1 && instance.class_name != class_name {
        bail!(
            "Projection cannot change service root '{}' from {} to {}",
            target[0],
            instance.class_name,
            class_name
        );
    }
    instance.properties.extend(properties.clone());
    instance.attributes.extend(attributes.clone());
    if let Some(tags) = tags {
        if tags.is_empty() {
            instance.properties.remove("Tags");
        } else {
            instance.properties.insert(
                "Tags".to_string(),
                Value::Array(tags.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    document.write_file(&settings_path)
}

fn import_model_at_target(stage: &Path, target: &[String], model: &Path) -> Result<()> {
    if target.len() < 2 {
        bail!("Model target must include a service and parent path");
    }
    let parent = &target[..target.len() - 1];
    update_stage_instance(
        stage,
        parent,
        if parent.len() == 1 {
            parent[0].as_str()
        } else {
            "Folder"
        },
        None,
        &Map::new(),
        &Map::new(),
        None,
    )?;
    let service = &target[0];
    let service_dir = stage.join(service);
    let settings_path = crate::writable_service_settings_path(&service_dir)?;
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    let parent_index = find_document_target(&document, parent)?;
    let outcome = crate::import_rbx_model_into_document(
        &mut document,
        &settings_path,
        service,
        model,
        Some(parent_index),
    )?;
    if outcome.root_settings_ids.len() == 1
        && let Some(instance) = document
            .instances
            .iter_mut()
            .find(|instance| instance.settings_id == outcome.root_settings_ids[0])
    {
        instance.name = target[target.len() - 1].clone();
    } else if outcome.root_settings_ids.len() > 1 {
        let mut settings_id = projection_settings_id("model-container", &target.join("."));
        let mut suffix = 2usize;
        while document
            .instances
            .iter()
            .any(|instance| instance.settings_id == settings_id)
        {
            settings_id = projection_settings_id(
                "model-container",
                &format!("{}:{suffix}", target.join(".")),
            );
            suffix += 1;
        }
        let container_index = document.instances.len();
        document.instances.push(SettingsBytecodeInstance {
            settings_id,
            name: target[target.len() - 1].clone(),
            class_name: "Folder".to_string(),
            parent_index: Some(parent_index),
            properties: Map::new(),
            attributes: Map::new(),
        });
        let roots = outcome
            .root_settings_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        for instance in &mut document.instances[..container_index] {
            if roots.contains(&instance.settings_id) {
                instance.parent_index = Some(container_index);
            }
        }
    }
    let source_paths = crate::build_editor_source_paths_by_index(&document, service, &service_dir);
    let mut writes = Vec::with_capacity(outcome.source_by_settings_id.len() + 1);
    for (settings_id, bytes) in outcome.source_by_settings_id {
        let index = document
            .instances
            .iter()
            .position(|instance| instance.settings_id == settings_id)
            .with_context(|| format!("Imported script id {settings_id} disappeared"))?;
        let source_path = source_paths
            .get(index)
            .and_then(Option::as_ref)
            .with_context(|| format!("Imported script {settings_id} has no source path"))?;
        writes.push((source_path.clone(), bytes));
    }
    writes.push((
        settings_path.clone(),
        crate::settings_document_bytes(&document, &settings_path)?,
    ));
    write_file_transaction(&writes)
}

fn merge_settings_mount(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    if target.len() < 2 {
        bail!("Settings mount target must include a service and parent path");
    }
    let parent = &target[..target.len() - 1];
    update_stage_instance(
        stage,
        parent,
        if parent.len() == 1 {
            parent[0].as_str()
        } else {
            "Folder"
        },
        None,
        &Map::new(),
        &Map::new(),
        None,
    )?;
    let mounted = SettingsBytecode::read_file(source)?;
    let roots = mounted
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    if roots.len() > 1 {
        update_stage_instance(
            stage,
            target,
            "Folder",
            None,
            &Map::new(),
            &Map::new(),
            None,
        )?;
    }
    let service = &target[0];
    let settings_path = crate::writable_service_settings_path(&stage.join(service))?;
    let mut destination = SettingsBytecode::read_file(&settings_path)?;
    let parent_index = if roots.len() > 1 {
        find_document_target(&destination, target)?
    } else {
        find_document_target(&destination, parent)?
    };
    let mut remap = BTreeMap::new();
    let mut remapped_ids = HashMap::new();
    for (index, instance) in mounted.instances.iter().enumerate() {
        let next = destination.instances.len();
        let parent = if roots.contains(&index) {
            Some(parent_index)
        } else {
            instance
                .parent_index
                .and_then(|value| remap.get(&value).copied())
        };
        let mut instance = instance.clone();
        instance.parent_index = parent;
        if roots.len() == 1 && roots[0] == index {
            instance.name = target[target.len() - 1].clone();
        }
        let old_settings_id = instance.settings_id.clone();
        if destination
            .instances
            .iter()
            .any(|existing| existing.settings_id == instance.settings_id)
        {
            instance.settings_id =
                projection_settings_id("mounted", &format!("{}:{index}", target.join(".")));
        }
        remapped_ids.insert(old_settings_id, instance.settings_id.clone());
        destination.instances.push(instance);
        remap.insert(index, next);
    }
    let remapped_indices = remap
        .iter()
        .map(|(old, new)| (*old, *new))
        .collect::<HashMap<_, _>>();
    let mut mounted_paths = HashMap::<Vec<String>, Vec<Vec<usize>>>::new();
    for (segments, ordinals) in projection_instance_path_parts(&mounted) {
        mounted_paths.entry(segments).or_default().push(ordinals);
    }
    let mut target_ordinals = active_target_ordinals(target);
    if target_ordinals.is_empty() {
        target_ordinals.resize(target.len(), 1);
    }
    let path_root_components = usize::from(roots.len() == 1);
    for source_index in 0..mounted.instances.len() {
        let Some(destination_index) = remap.get(&source_index).copied() else {
            continue;
        };
        remap_settings_document_references(
            &mut destination.instances[destination_index].properties,
            &remapped_ids,
            &remapped_indices,
            target,
            &target_ordinals,
            &mounted_paths,
            path_root_components,
        )?;
        remap_settings_document_references(
            &mut destination.instances[destination_index].attributes,
            &remapped_ids,
            &remapped_indices,
            target,
            &target_ordinals,
            &mounted_paths,
            path_root_components,
        )?;
        record_projection_identity(
            &destination.instances[destination_index].settings_id,
            source,
            &mounted.instances[source_index].settings_id,
        );
    }
    destination.write_file(&settings_path)
}

fn merge_settings_document_at_target(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    let mounted = SettingsBytecode::read_file(source)?;
    let roots = mounted
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    let root_class = roots
        .first()
        .and_then(|index| mounted.instances.get(*index))
        .map(|instance| instance.class_name.as_str())
        .unwrap_or("Folder");
    update_stage_instance(
        stage,
        target,
        root_class,
        None,
        &Map::new(),
        &Map::new(),
        None,
    )?;
    let service = target
        .first()
        .context("Settings source target must include a service")?;
    let settings_path = crate::writable_service_settings_path(&stage.join(service))?;
    let mut destination = SettingsBytecode::read_file(&settings_path)?;
    let target_index = find_document_target(&destination, target)?;
    let single_root = roots.len() == 1;
    let mut index_map = HashMap::new();
    let mut id_map = HashMap::new();
    let mut used_ids = destination
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<BTreeSet<_>>();
    for (source_index, source_instance) in mounted.instances.iter().enumerate() {
        let destination_index = if roots.contains(&source_index) && single_root {
            target_index
        } else {
            let parent = source_instance
                .parent_index
                .and_then(|parent| index_map.get(&parent).copied())
                .unwrap_or(target_index);
            let matches = destination
                .instances
                .iter()
                .enumerate()
                .filter(|(_, instance)| {
                    instance.parent_index == Some(parent) && instance.name == source_instance.name
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [index] => *index,
                [] => {
                    let index = destination.instances.len();
                    destination.instances.push(SettingsBytecodeInstance {
                        settings_id: String::new(),
                        name: source_instance.name.clone(),
                        class_name: source_instance.class_name.clone(),
                        parent_index: Some(parent),
                        properties: Map::new(),
                        attributes: Map::new(),
                    });
                    index
                }
                _ => bail!(
                    "Settings source {} maps to duplicate target '{}'",
                    source.display(),
                    source_instance.name
                ),
            }
        };
        let mut output_id = source_instance.settings_id.clone();
        let current_id = destination.instances[destination_index].settings_id.clone();
        if output_id != current_id && used_ids.contains(&output_id) {
            output_id = projection_settings_id(
                "settings-source",
                &format!("{}:{source_index}", target.join(".")),
            );
        }
        used_ids.remove(&current_id);
        used_ids.insert(output_id.clone());
        let output = &mut destination.instances[destination_index];
        output.settings_id = output_id.clone();
        output.class_name = source_instance.class_name.clone();
        output.properties.extend(source_instance.properties.clone());
        output.attributes.extend(source_instance.attributes.clone());
        index_map.insert(source_index, destination_index);
        id_map.insert(source_instance.settings_id.clone(), output_id);
    }
    let target_prefix = target.to_vec();
    let mut mounted_paths = HashMap::<Vec<String>, Vec<Vec<usize>>>::new();
    for (segments, ordinals) in projection_instance_path_parts(&mounted) {
        mounted_paths.entry(segments).or_default().push(ordinals);
    }
    let mut target_ordinals = active_target_ordinals(target);
    if target_ordinals.is_empty() {
        target_ordinals.resize(target.len(), 1);
    }
    let path_root_components = usize::from(single_root);
    for source_index in 0..mounted.instances.len() {
        let Some(destination_index) = index_map.get(&source_index).copied() else {
            continue;
        };
        remap_settings_document_references(
            &mut destination.instances[destination_index].properties,
            &id_map,
            &index_map,
            &target_prefix,
            &target_ordinals,
            &mounted_paths,
            path_root_components,
        )?;
        remap_settings_document_references(
            &mut destination.instances[destination_index].attributes,
            &id_map,
            &index_map,
            &target_prefix,
            &target_ordinals,
            &mounted_paths,
            path_root_components,
        )?;
        record_projection_identity(
            &destination.instances[destination_index].settings_id,
            source,
            &mounted.instances[source_index].settings_id,
        );
    }
    destination.write_file(&settings_path)
}

fn remap_settings_document_references(
    record: &mut Map<String, Value>,
    ids: &HashMap<String, String>,
    indices: &HashMap<usize, usize>,
    target: &[String],
    target_ordinals: &[usize],
    internal_paths: &HashMap<Vec<String>, Vec<Vec<usize>>>,
    path_root_components: usize,
) -> Result<()> {
    for value in record.values_mut() {
        remap_settings_document_reference_value(
            value,
            ids,
            indices,
            target,
            target_ordinals,
            internal_paths,
            path_root_components,
        )?;
    }
    Ok(())
}

fn remap_settings_document_reference_value(
    value: &mut Value,
    ids: &HashMap<String, String>,
    indices: &HashMap<usize, usize>,
    target: &[String],
    target_ordinals: &[usize],
    internal_paths: &HashMap<Vec<String>, Vec<Vec<usize>>>,
    path_root_components: usize,
) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                remap_settings_document_reference_value(
                    value,
                    ids,
                    indices,
                    target,
                    target_ordinals,
                    internal_paths,
                    path_root_components,
                )?;
            }
        }
        Value::Object(object) => {
            let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                || object.contains_key("settingsId")
                || object.contains_key("instanceId")
                || object.contains_key("instanceIndex");
            if is_reference {
                let mut internal_signal = false;
                let mut external_signal = false;
                for key in ["settingsId", "instanceId"] {
                    if let Some(old) = object.get(key).and_then(Value::as_str) {
                        if ids.contains_key(old) {
                            internal_signal = true;
                        } else if !old.is_empty() {
                            external_signal = true;
                        }
                    }
                }
                let old_index = object
                    .get("instanceIndex")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| index.checked_sub(1));
                if let Some(old) = old_index {
                    if indices.contains_key(&old) {
                        internal_signal = true;
                    } else {
                        external_signal = true;
                    }
                }
                let path_segments = object
                    .get("pathSegments")
                    .and_then(Value::as_array)
                    .and_then(|values| {
                        values
                            .iter()
                            .map(Value::as_str)
                            .map(|value| value.map(str::to_string))
                            .collect::<Option<Vec<_>>>()
                    });
                let path_ordinals = object
                    .get("pathOrdinals")
                    .and_then(Value::as_array)
                    .and_then(|values| {
                        values
                            .iter()
                            .map(Value::as_u64)
                            .map(|value| {
                                value
                                    .filter(|value| *value > 0)
                                    .and_then(|value| usize::try_from(value).ok())
                            })
                            .collect::<Option<Vec<_>>>()
                    });
                let mut resolved_path_ordinals = None;
                if let Some(path) = path_segments.as_ref() {
                    if let Some(candidates) = internal_paths.get(path) {
                        if let Some(ordinals) = path_ordinals.as_ref() {
                            if ordinals.len() != path.len() {
                                bail!(
                                    "Mounted settings reference pathOrdinals must contain one value per path segment"
                                );
                            }
                            if candidates.contains(ordinals) {
                                internal_signal = true;
                                resolved_path_ordinals = Some(ordinals.clone());
                            } else {
                                external_signal = true;
                            }
                        } else if candidates.len() == 1 {
                            internal_signal = true;
                            resolved_path_ordinals = candidates.first().cloned();
                        } else {
                            bail!(
                                "Mounted settings reference path '{}' is ambiguous; include pathOrdinals",
                                path.join(".")
                            );
                        }
                    } else if !path.is_empty() {
                        external_signal = true;
                    }
                }
                if internal_signal && external_signal {
                    bail!("Mounted settings contain contradictory instance reference fields");
                }
                if !internal_signal {
                    for value in object.values_mut() {
                        remap_settings_document_reference_value(
                            value,
                            ids,
                            indices,
                            target,
                            target_ordinals,
                            internal_paths,
                            path_root_components,
                        )?;
                    }
                    return Ok(());
                }
                for key in ["settingsId", "instanceId"] {
                    if let Some(old) = object.get(key).and_then(Value::as_str)
                        && let Some(new) = ids.get(old)
                    {
                        object.insert(key.to_string(), Value::String(new.clone()));
                    }
                }
                if let Some(old) = object
                    .get("instanceIndex")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| index.checked_sub(1))
                    && let Some(new) = indices.get(&old)
                {
                    object.insert(
                        "instanceIndex".to_string(),
                        Value::Number(serde_json::Number::from((new + 1) as u64)),
                    );
                }
                if let Some(paths) = object.get_mut("pathSegments").and_then(Value::as_array_mut) {
                    let tail = paths
                        .iter()
                        .skip(path_root_components)
                        .cloned()
                        .collect::<Vec<_>>();
                    *paths = target
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .chain(tail)
                        .collect();
                    if let Some(source_ordinals) = resolved_path_ordinals {
                        let tail = source_ordinals
                            .into_iter()
                            .skip(path_root_components)
                            .map(|ordinal| Value::Number(serde_json::Number::from(ordinal as u64)))
                            .collect::<Vec<_>>();
                        object.insert(
                            "pathOrdinals".to_string(),
                            Value::Array(
                                target_ordinals
                                    .iter()
                                    .map(|ordinal| {
                                        Value::Number(serde_json::Number::from(*ordinal as u64))
                                    })
                                    .chain(tail)
                                    .collect(),
                            ),
                        );
                    }
                }
            }
            for value in object.values_mut() {
                remap_settings_document_reference_value(
                    value,
                    ids,
                    indices,
                    target,
                    target_ordinals,
                    internal_paths,
                    path_root_components,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn stage_nested_project_at_target(
    nested: &LoadedProject,
    stage: &Path,
    target: &[String],
) -> Result<()> {
    let path = fs::canonicalize(&nested.path)
        .with_context(|| format!("Failed to resolve nested project {}", nested.path.display()))?;
    let inserted = NESTED_STAGE_STACK.with(|stack| stack.borrow_mut().insert(path.clone()));
    if !inserted {
        bail!("Nested project cycle includes {}", nested.path.display());
    }
    let result = (|| {
        validate_nested_project(nested)?;
        let root_class = nested
            .project
            .root
            .class_name
            .as_deref()
            .unwrap_or("Folder");
        let flattened = root_class == "DataModel";
        cache_script_naming(&target_fs_path(stage, target), &nested.project);
        cache_script_naming(
            &nested.root.join(&nested.project.source_root),
            &nested.project,
        );
        let staged_root_class = if flattened {
            target
                .first()
                .filter(|_| target.len() == 1)
                .map_or("Folder", String::as_str)
        } else {
            root_class
        };
        update_stage_instance(
            stage,
            target,
            staged_root_class,
            None,
            &Map::new(),
            &Map::new(),
            None,
        )?;
        let source = nested.root.join(&nested.project.source_root);
        if source.is_dir() {
            stage_source_directory(
                nested,
                stage,
                &source,
                &target_fs_path(stage, target),
                false,
                None,
            )?;
            let settings = crate::existing_service_settings_path(&source);
            if settings.is_file() {
                merge_settings_document_at_target(stage, target, &settings)?;
            }
        }
        if !flattened
            && (nested.project.root.class_name.is_some()
                || nested.project.root.id.is_some()
                || !nested.project.root.properties.is_empty()
                || !nested.project.root.attributes.is_empty()
                || nested.project.root.tags.is_some())
        {
            let class_name = nested
                .project
                .root
                .class_name
                .clone()
                .or(stage_target_class(stage, target)?)
                .unwrap_or_else(|| root_class.to_string());
            let properties =
                normalize_property_map(Some(&class_name), &nested.project.root.properties)?;
            let attributes = normalize_property_map(None, &nested.project.root.attributes)?;
            override_stage_identity(
                stage,
                target,
                nested.project.root.class_name.as_deref(),
                nested.project.root.id.as_deref(),
            )?;
            update_stage_instance(
                stage,
                target,
                &class_name,
                nested.project.root.id.as_deref(),
                &properties,
                &attributes,
                nested.project.root.tags.as_deref(),
            )?;
        }
        for (name, node) in &nested.project.tree {
            let mut child_target = target.to_vec();
            child_target.push(name.clone());
            stage_tree_node(nested, stage, &child_target, node)?;
        }
        for mount in &nested.project.mounts {
            let mut mounted = mount.clone();
            mounted.target = mount.target.with_prefix(target);
            stage_mount(nested, stage, &mounted)?;
        }
        for adapter in &nested.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let mut nested_adapter = adapter.clone();
            nested_adapter.target = adapter.target.with_prefix(target);
            stage_adapter(nested, stage, &nested_adapter)?;
        }
        Ok(())
    })();
    NESTED_STAGE_STACK.with(|stack| {
        stack.borrow_mut().remove(&path);
    });
    result
}

fn remap_settings_references(record: &mut Map<String, Value>, ids: &HashMap<String, String>) {
    for value in record.values_mut() {
        remap_settings_reference_value(value, ids);
    }
}

fn remap_settings_reference_value(value: &mut Value, ids: &HashMap<String, String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                remap_settings_reference_value(value, ids);
            }
        }
        Value::Object(object) => {
            for key in ["settingsId", "instanceId"] {
                if let Some(old) = object.get(key).and_then(Value::as_str)
                    && let Some(new) = ids.get(old)
                {
                    object.insert(key.to_string(), Value::String(new.clone()));
                }
            }
            for value in object.values_mut() {
                remap_settings_reference_value(value, ids);
            }
        }
        _ => {}
    }
}

fn find_document_target_optional(
    document: &SettingsBytecode,
    target: &[String],
) -> Result<Option<usize>> {
    find_document_target_optional_with_ordinals(document, target, &active_target_ordinals(target))
}

fn find_document_target_optional_with_ordinals(
    document: &SettingsBytecode,
    target: &[String],
    ordinals: &[usize],
) -> Result<Option<usize>> {
    if !ordinals.is_empty() && ordinals.len() != target.len() {
        bail!("Projection target ordinals must contain one value per segment");
    }
    let mut parent = None;
    let mut found = None;
    for (depth, name) in target.iter().enumerate() {
        let matches = document
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| instance.parent_index == parent && instance.name == *name)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Ok(None);
        }
        let ordinal = ordinals.get(depth).copied();
        if ordinal.is_none() && matches.len() > 1 {
            bail!(
                "Projection target '{}' is ambiguous at '{}'",
                target.join("."),
                name
            );
        }
        let selected = ordinal.unwrap_or(1);
        if selected > matches.len() {
            return Ok(None);
        }
        found = matches.get(selected - 1).copied();
        parent = found;
    }
    Ok(found)
}

fn find_document_target(document: &SettingsBytecode, target: &[String]) -> Result<usize> {
    let ordinals = active_target_ordinals(target);
    find_document_target_optional_with_ordinals(document, target, &ordinals)?.with_context(|| {
        if ordinals.is_empty() {
            format!("Projection target '{}' does not exist", target.join("."))
        } else {
            format!(
                "Projection target '{}' with ordinals {:?} does not exist",
                target.join("."),
                ordinals
            )
        }
    })
}

fn clear_stage_target_children(stage: &Path, target: &[String]) -> Result<()> {
    let service = target
        .first()
        .context("Projection target must include a service")?;
    let service_dir = stage.join(service);
    let mut settings_path = crate::existing_service_settings_path(&service_dir);
    if !settings_path.is_file()
        || find_document_target_optional(&SettingsBytecode::read_file(&settings_path)?, target)?
            .is_none()
    {
        update_stage_instance(
            stage,
            target,
            "Folder",
            None,
            &Map::new(),
            &Map::new(),
            None,
        )?;
        settings_path = crate::existing_service_settings_path(&service_dir);
    }
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    let target_index = find_document_target(&document, target)?;
    let children = crate::settings_children_by_parent(&document);
    let mut removed = BTreeSet::new();
    let mut pending = children.get(target_index).cloned().unwrap_or_default();
    while let Some(index) = pending.pop() {
        if removed.insert(index) {
            pending.extend(children.get(index).into_iter().flatten().copied());
        }
    }
    if removed.is_empty() {
        return Ok(());
    }
    let original_instances = document.instances.clone();
    let parent_ids = original_instances
        .iter()
        .map(|instance| {
            instance
                .parent_index
                .and_then(|parent| original_instances.get(parent))
                .map(|parent| parent.settings_id.clone())
        })
        .collect::<Vec<_>>();
    for instance in &mut document.instances {
        stabilize_reference_indices(&mut instance.properties, &original_instances);
        stabilize_reference_indices(&mut instance.attributes, &original_instances);
    }
    let mut retained_parent_ids = Vec::new();
    let mut instances = Vec::with_capacity(document.instances.len() - removed.len());
    for (index, instance) in document.instances.iter().enumerate() {
        if !removed.contains(&index) {
            instances.push(instance.clone());
            retained_parent_ids.push(parent_ids[index].clone());
        }
    }
    let indices_by_id = instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for (index, instance) in instances.iter_mut().enumerate() {
        instance.parent_index = retained_parent_ids[index]
            .as_deref()
            .and_then(|parent| indices_by_id.get(parent).copied());
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    document.instances = instances;
    document.write_file(&settings_path)
}

fn stage_model_json(stage: &Path, target: &[String], source: &Path) -> Result<()> {
    if target.len() < 2 {
        bail!("Model JSON target must be below a Studio service");
    }
    let text = fs::read_to_string(source)?;
    let value = parse_jsonc_value(&text)?;
    let object = value
        .as_object()
        .context("Model JSON root must be an object")?;
    let hierarchical_instances;
    let root_input_id;
    let instances = match object.get("instances") {
        Some(value) => {
            root_input_id = None;
            value
                .as_array()
                .context("Model JSON instances must be an array")?
        }
        None => {
            hierarchical_instances = flatten_rojo_model_json(object, target)?;
            root_input_id = hierarchical_instances
                .first()
                .and_then(Value::as_object)
                .and_then(|instance| instance.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            &hierarchical_instances
        }
    };
    let mut input_ids = Vec::with_capacity(instances.len());
    let mut input_indices = HashMap::with_capacity(instances.len());
    for (index, value) in instances.iter().enumerate() {
        let instance = value
            .as_object()
            .context("Model JSON instances must be objects")?;
        let id = instance
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Model JSON instance id must be a non-empty string")?;
        if input_indices.insert(id.to_string(), index).is_some() {
            bail!("Model JSON contains duplicate instance id '{id}'");
        }
        input_ids.push(id.to_string());
    }
    let mut parent_indices = Vec::with_capacity(instances.len());
    for value in instances {
        let instance = value
            .as_object()
            .context("Model JSON instances must be objects")?;
        if let Some(parent) = instance.get("parentId") {
            if parent.is_null() {
                parent_indices.push(None);
                continue;
            }
            let parent_id = parent
                .as_str()
                .filter(|value| !value.is_empty())
                .context("Model JSON parentId must be null or a non-empty string")?;
            let parent_index = input_indices
                .get(parent_id)
                .copied()
                .with_context(|| format!("Model JSON parent id '{parent_id}' does not exist"))?;
            parent_indices.push(Some(parent_index));
        } else {
            parent_indices.push(None);
        }
    }
    let mut parent_states = vec![0_u8; instances.len()];
    for start in 0..instances.len() {
        if parent_states[start] == 2 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(index) = current {
            match parent_states[index] {
                0 => {
                    parent_states[index] = 1;
                    path.push(index);
                    current = parent_indices[index];
                }
                1 => bail!(
                    "Model JSON contains a parent cycle at '{}'",
                    input_ids[index]
                ),
                2 => break,
                _ => unreachable!("model JSON parent state is internal"),
            }
        }
        for index in path {
            parent_states[index] = 2;
        }
    }
    clear_stage_target_children(stage, target)?;
    let service = target
        .first()
        .context("Model JSON target must include a service")?;
    let settings_path = crate::existing_service_settings_path(&stage.join(service));
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    let target_index = find_document_target(&document, target)?;
    let mut used_ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<BTreeSet<_>>();
    let mut output_ids = HashMap::new();
    if let Some(root_id) = root_input_id.as_deref() {
        let previous = document.instances[target_index].settings_id.clone();
        used_ids.remove(&previous);
        if !used_ids.insert(root_id.to_string()) {
            bail!(
                "Model JSON root id '{root_id}' collides with an instance outside the adapter target"
            );
        }
        document.instances[target_index].settings_id = root_id.to_string();
        if previous != root_id {
            let remap = HashMap::from([(previous, root_id.to_string())]);
            for instance in &mut document.instances {
                remap_settings_references(&mut instance.properties, &remap);
                remap_settings_references(&mut instance.attributes, &remap);
            }
        }
    }
    for id in &input_ids {
        if root_input_id.as_deref() == Some(id.as_str()) {
            output_ids.insert(id.clone(), id.clone());
            continue;
        }
        if !used_ids.insert(id.clone()) {
            bail!(
                "Model JSON instance id '{id}' collides with an instance outside the adapter target"
            );
        }
        output_ids.insert(id.clone(), id.clone());
    }
    let first_output_index = document.instances.len();
    let mut output_indices = HashMap::new();
    let mut next_output_index = first_output_index;
    for id in &input_ids {
        if root_input_id.as_deref() == Some(id.as_str()) {
            output_indices.insert(id.clone(), target_index);
        } else {
            output_indices.insert(id.clone(), next_output_index);
            next_output_index += 1;
        }
    }
    for (offset, value) in instances.iter().enumerate() {
        let instance = value
            .as_object()
            .context("Model JSON instances must be objects")?;
        let id = &input_ids[offset];
        let name = instance
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("Model JSON instance name must be a non-empty string")?;
        let class_name = match instance.get("className") {
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("Model JSON instance '{id}' className must be a non-empty string")
                })?,
            None => "Folder",
        };
        let mut properties = match instance.get("properties") {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{id}' properties must be an object")
            })?,
            None => Map::new(),
        };
        let mut attributes = match instance.get("attributes") {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{id}' attributes must be an object")
            })?,
            None => Map::new(),
        };
        stabilize_model_json_reference_indices(&mut properties, &input_ids)?;
        stabilize_model_json_reference_indices(&mut attributes, &input_ids)?;
        remap_settings_references(&mut properties, &output_ids);
        remap_settings_references(&mut attributes, &output_ids);
        let mut properties = normalize_model_property_map(Some(class_name), &properties)
            .with_context(|| format!("Invalid properties on model JSON instance '{id}'"))?;
        let attributes = normalize_model_property_map(None, &attributes)
            .with_context(|| format!("Invalid attributes on model JSON instance '{id}'"))?;
        let tags = match instance.get("tags") {
            Some(value) => value
                .as_array()
                .with_context(|| format!("Model JSON instance '{id}' tags must be an array"))?
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .with_context(|| {
                            format!(
                                "Model JSON instance '{id}' tag {index} must be a non-empty string"
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        if !tags.is_empty() {
            properties.insert(
                "Tags".to_string(),
                Value::Array(
                    tags.into_iter()
                        .map(|tag| Value::String(tag.to_string()))
                        .collect(),
                ),
            );
        }
        let parent_index = instance
            .get("parentId")
            .and_then(Value::as_str)
            .map(|parent| output_indices[parent])
            .unwrap_or(target_index);
        if root_input_id.as_deref() == Some(id.as_str()) {
            let root = &mut document.instances[target_index];
            root.class_name = class_name.to_string();
            root.properties = properties;
            root.attributes = attributes;
        } else {
            document.instances.push(SettingsBytecodeInstance {
                settings_id: output_ids[id].clone(),
                name: name.to_string(),
                class_name: class_name.to_string(),
                parent_index: Some(parent_index),
                properties,
                attributes,
            });
        }
    }
    let indices_by_id = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    if root_input_id.is_some() {
        reindex_reference_indices(
            &mut document.instances[target_index].properties,
            &indices_by_id,
        );
        reindex_reference_indices(
            &mut document.instances[target_index].attributes,
            &indices_by_id,
        );
    }
    for instance in &mut document.instances[first_output_index..] {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    document.write_file(&settings_path)
}

fn flatten_rojo_model_json(root: &Map<String, Value>, target: &[String]) -> Result<Vec<Value>> {
    fn field<'a>(
        object: &'a Map<String, Value>,
        lower: &str,
        upper: &str,
        path: &str,
    ) -> Result<Option<&'a Value>> {
        if object.contains_key(lower) && object.contains_key(upper) {
            bail!("Model JSON instance '{path}' declares both {lower} and {upper}");
        }
        Ok(object.get(lower).or_else(|| object.get(upper)))
    }

    fn visit(
        object: &Map<String, Value>,
        name: String,
        parent_id: Option<&str>,
        path: &str,
        output: &mut Vec<Value>,
    ) -> Result<()> {
        if object.contains_key("id") && object.contains_key("$id") {
            bail!("Model JSON instance '{path}' declares both id and $id");
        }
        let id = match object.get("id").or_else(|| object.get("$id")) {
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("Model JSON instance '{path}' id must be a non-empty string")
                })?
                .to_string(),
            None => projection_settings_id("rojo-model-json", path),
        };
        let class_name = match field(object, "className", "ClassName", path)? {
            Some(value) => value
                .as_str()
                .filter(|value| !value.is_empty())
                .with_context(|| {
                    format!("Model JSON instance '{path}' className must be a non-empty string")
                })?,
            None => "Folder",
        };
        let properties = match field(object, "properties", "Properties", path)? {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{path}' properties must be an object")
            })?,
            None => Map::new(),
        };
        let attributes = match field(object, "attributes", "Attributes", path)? {
            Some(value) => value.as_object().cloned().with_context(|| {
                format!("Model JSON instance '{path}' attributes must be an object")
            })?,
            None => Map::new(),
        };
        let tags = match field(object, "tags", "Tags", path)? {
            Some(value) => value
                .as_array()
                .with_context(|| format!("Model JSON instance '{path}' tags must be an array"))?
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .map(|value| Value::String(value.to_string()))
                        .with_context(|| {
                            format!(
                                "Model JSON instance '{path}' tag {index} must be a non-empty string"
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        output.push(json!({
            "id": id,
            "name": name,
            "className": class_name,
            "parentId": parent_id,
            "properties": properties,
            "attributes": attributes,
            "tags": tags,
        }));
        if let Some(children) = field(object, "children", "Children", path)? {
            let children = children
                .as_array()
                .context("Model JSON Children must be an array")?;
            for (index, child) in children.iter().enumerate() {
                let child = child
                    .as_object()
                    .context("Model JSON children must be objects")?;
                let child_path = format!("{path}/child[{index}]");
                let child_name = field(child, "name", "Name", &child_path)?
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .with_context(|| {
                        format!("Model JSON child {index} name must be a non-empty string")
                    })?;
                visit(
                    child,
                    child_name.to_string(),
                    Some(&id),
                    &format!("{path}/{index}:{child_name}"),
                    output,
                )?;
            }
        }
        Ok(())
    }

    let name = target
        .last()
        .cloned()
        .context("Model JSON target has no name")?;
    let mut output = Vec::new();
    visit(root, name, None, &target.join("/"), &mut output)?;
    Ok(output)
}

fn stabilize_model_json_reference_indices(
    record: &mut Map<String, Value>,
    ids: &[String],
) -> Result<()> {
    fn selector(object: &Map<String, Value>, name: &str) -> Result<Option<String>> {
        object
            .get(name)
            .map(|value| {
                value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .with_context(|| format!("Ref {name} must be a non-empty string"))
            })
            .transpose()
    }

    fn visit(value: &mut Value, ids: &[String]) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, ids)?;
                }
            }
            Value::Object(object) => {
                let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId")
                    || object.contains_key("instanceIndex");
                if is_reference {
                    let mut resolved = selector(object, "settingsId")?;
                    if let Some(instance_id) = selector(object, "instanceId")? {
                        if resolved.as_ref().is_some_and(|id| id != &instance_id) {
                            bail!("Ref settingsId and instanceId identify different instances");
                        }
                        resolved = Some(instance_id);
                    }
                    if let Some(value) = object.get("instanceIndex") {
                        let index = value
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .and_then(|index| index.checked_sub(1))
                            .context("Ref instanceIndex must be a valid 1-based index")?;
                        let id = ids.get(index).with_context(|| {
                            format!("Ref instanceIndex {} is out of range", index + 1)
                        })?;
                        if resolved.as_ref().is_some_and(|resolved| resolved != id) {
                            bail!("Ref stable id and instanceIndex identify different instances");
                        }
                        resolved = Some(id.clone());
                    }
                    if let Some(id) = resolved {
                        object.insert("settingsId".to_string(), Value::String(id));
                        object.remove("instanceId");
                    }
                    object.remove("instanceIndex");
                }
                for value in object.values_mut() {
                    visit(value, ids)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    for value in record.values_mut() {
        visit(value, ids)?;
    }
    Ok(())
}

fn normalize_model_property_map(
    class_name: Option<&str>,
    values: &Map<String, Value>,
) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|(name, value)| {
            let normalized = if contains_reference_value(value) {
                value.clone()
            } else {
                crate::normalize_project_typed_value(class_name, Some(name), value)
                    .with_context(|| format!("Invalid value for '{name}'"))?
            };
            Ok((name.clone(), normalized))
        })
        .collect()
}

fn contains_reference_value(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_reference_value),
        Value::Object(object) => {
            object.get("_type").and_then(Value::as_str) == Some("Ref")
                || object.contains_key("instanceIndex")
                || object.contains_key("settingsId")
                || object.contains_key("instanceId")
                || object.values().any(contains_reference_value)
        }
        _ => false,
    }
}

fn adapter_target_script_path(loaded: &LoadedProject, stage: &Path, target: &[String]) -> PathBuf {
    let extension = match loaded.project.script_extension {
        ScriptExtensionPolicy::Lua => "lua",
        ScriptExtensionPolicy::Preserve | ScriptExtensionPolicy::Luau => "luau",
    };
    let leaf = target.last().map(String::as_str).unwrap_or("Adapter");
    let parent = target_fs_path(stage, &target[..target.len().saturating_sub(1)]);
    parent.join(format!(
        "{}{}.{}",
        leaf, loaded.project.export_naming.module_suffix, extension
    ))
}

fn target_segments(target: &ProjectTarget) -> Result<Vec<String>> {
    validate_instance_target(target, "target")?;
    Ok(target.segments())
}

fn target_fs_path(root: &Path, target: &[String]) -> PathBuf {
    target
        .iter()
        .fold(root.to_path_buf(), |path, segment| path.join(segment))
}

fn normalize_sync_middleware(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.replace(['-', '_'], "").as_str() {
        "modulescript" | "module" => "modulescript".to_string(),
        "serverscript" | "server" | "script" => "serverscript".to_string(),
        "clientscript" | "client" | "localscript" => "clientscript".to_string(),
        "pluginscript" | "plugin" => "pluginscript".to_string(),
        "modeljson" => "model-json".to_string(),
        "nestedproject" | "project" => "nested-project".to_string(),
        _ => normalized,
    }
}

fn validate_sync_middleware(value: &str) -> Result<()> {
    let normalized = normalize_sync_middleware(value);
    if !matches!(
        normalized.as_str(),
        "ignore"
            | "modulescript"
            | "serverscript"
            | "clientscript"
            | "pluginscript"
            | "txt"
            | "csv"
            | "json"
            | "jsonc"
            | "toml"
            | "yaml"
            | "msgpack"
            | "markdown"
            | "model-json"
            | "rbxm"
            | "rbxmx"
            | "nested-project"
    ) {
        bail!("Unsupported sync middleware '{value}'");
    }
    Ok(())
}

fn sync_rule_matches(rule: &SyncRule, path: &Path) -> Result<bool> {
    if !compile_glob(&rule.pattern)?.is_match(path) {
        return Ok(false);
    }
    if let Some(exclude) = rule.exclude.as_deref()
        && compile_glob(exclude)?.is_match(path)
    {
        return Ok(false);
    }
    Ok(true)
}

fn owned_filter_candidate(
    instance: &SettingsBytecodeInstance,
    path: String,
) -> OwnedFilterCandidate {
    let tags = instance
        .properties
        .get("Tags")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    OwnedFilterCandidate {
        id: instance.settings_id.clone(),
        path,
        name: instance.name.clone(),
        class: instance.class_name.to_string(),
        tags,
        attributes: instance.attributes.keys().cloned().collect(),
        properties: instance.properties.keys().cloned().collect(),
    }
}

fn filter_allows_candidate_pair(
    rules: &[FilterRule],
    direction: FilterDirection,
    current: &OwnedFilterCandidate,
    baseline: Option<&OwnedFilterCandidate>,
    scope: FilterScope<'_>,
) -> Result<bool> {
    if !filter_allows_scope(rules, direction, &current.borrowed(), scope)? {
        return Ok(false);
    }
    baseline
        .map(|baseline| filter_allows_scope(rules, direction, &baseline.borrowed(), scope))
        .transpose()
        .map(|allowed| allowed.unwrap_or(true))
}

fn sync_rule_instance_name(rule: &SyncRule, path: &Path) -> Result<String> {
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("{} has no UTF-8 file name", path.display()))?;
    let name = if let Some(suffix) = rule.suffix.as_deref() {
        file_name.strip_suffix(suffix).with_context(|| {
            format!(
                "{} matches '{}' but doesn't end with configured suffix '{}'",
                path.display(),
                rule.pattern,
                suffix
            )
        })?
    } else {
        path.file_stem()
            .and_then(OsStr::to_str)
            .with_context(|| format!("{} has no UTF-8 file stem", path.display()))?
    };
    if name.is_empty() {
        bail!(
            "Sync rule '{}' produces an empty instance name",
            rule.pattern
        );
    }
    Ok(name.to_string())
}

fn ignore_glob_pattern(raw: &str) -> Result<&str> {
    let pattern = raw
        .strip_prefix("\\!")
        .or_else(|| raw.strip_prefix('!'))
        .unwrap_or(raw);
    if pattern.is_empty() {
        bail!("Ignore glob cannot be empty");
    }
    Ok(pattern)
}

fn path_is_ignored(loaded: &LoadedProject, path: &Path) -> Result<bool> {
    let relative = path.strip_prefix(&loaded.root).unwrap_or(path);
    if relative.components().any(|component| {
        matches!(component, Component::Normal(name) if name == ".git" || name == ".renium")
    }) {
        return Ok(true);
    }
    let mut ignored = false;
    for raw in &loaded.project.glob_ignore_paths {
        let escaped = raw.starts_with("\\!");
        let negated = raw.starts_with('!') && !escaped;
        if compile_glob(ignore_glob_pattern(raw)?)?.is_match(relative) {
            ignored = !negated;
        }
    }
    Ok(ignored)
}

fn is_metadata_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".meta.json") || lower.ends_with(".meta.jsonc")
        })
}

fn metadata_sidecar_stem(name: &str) -> Option<&str> {
    name.strip_suffix(".meta.jsonc")
        .or_else(|| name.strip_suffix(".meta.json"))
}

fn projection_settings_id(kind: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    format!("projection:{:x}", digest.finalize())
}

fn contains_metadata_sidecars(root: &Path) -> Result<bool> {
    if !root.is_dir() {
        return Ok(false);
    }
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file() && is_metadata_sidecar(entry.path()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn stage_source_directory(
    loaded: &LoadedProject,
    stage: &Path,
    source: &Path,
    destination: &Path,
    owns_source: bool,
    rule_prefix: Option<&Path>,
) -> Result<()> {
    if !source.is_dir() {
        bail!(
            "Projection source directory does not exist: {}",
            source.display()
        );
    }
    fs::create_dir_all(destination)?;
    let source = absolute_path(source);
    let claimed_sources = projection_source_owner_paths(loaded);
    let mut transformed = Vec::new();
    let mut sidecars = Vec::new();
    let entries = walkdir::WalkDir::new(&source)
        .into_iter()
        .filter_entry(|entry| {
            !claimed_sources.iter().any(|claim| {
                projection_path_key(entry.path()) == projection_path_key(claim)
                    && (!owns_source
                        || projection_path_key(entry.path()) != projection_path_key(&source))
            })
        });
    for entry in entries {
        let entry = entry?;
        let relative = entry.path().strip_prefix(&source)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if entry.file_type().is_symlink() {
            bail!(
                "Projection sources cannot contain symlinks: {}",
                entry.path().display()
            );
        }
        if path_is_ignored(loaded, entry.path())? {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if is_metadata_sidecar(entry.path()) {
            sidecars.push((entry.path().to_path_buf(), target));
            continue;
        }
        let rule_relative = rule_prefix
            .map(|prefix| prefix.join(relative))
            .unwrap_or_else(|| relative.to_path_buf());
        let mut rule = None;
        for candidate in &loaded.project.sync_rules {
            if sync_rule_matches(candidate, &rule_relative)? {
                rule = Some(candidate);
            }
        }
        if let Some(rule) = rule {
            transformed.push((entry.path().to_path_buf(), rule_relative, target, rule));
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(entry.path(), target)?;
    }
    for (source_file, relative, target, rule) in transformed {
        stage_sync_rule(loaded, stage, &source_file, &relative, &target, rule)?;
    }
    for (source_file, target) in sidecars {
        apply_metadata_sidecar(loaded, stage, &source_file, &target)?;
    }
    Ok(())
}

fn projection_source_owner_paths(loaded: &LoadedProject) -> Vec<PathBuf> {
    let mut paths = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(_, node)| {
            node.path.map(|path| {
                fs::canonicalize(loaded.root.join(&path))
                    .unwrap_or_else(|_| absolute_path(&loaded.root.join(&path)))
            })
        })
        .chain(loaded.project.mounts.iter().map(|mount| {
            fs::canonicalize(loaded.root.join(&mount.source))
                .unwrap_or_else(|_| absolute_path(&loaded.root.join(&mount.source)))
        }))
        .chain(loaded.project.adapters.iter().flat_map(|adapter| {
            std::iter::once(
                fs::canonicalize(loaded.root.join(&adapter.source))
                    .unwrap_or_else(|_| absolute_path(&loaded.root.join(&adapter.source))),
            )
            .chain(adapter.output.as_deref().map(|output| {
                fs::canonicalize(loaded.root.join(output))
                    .unwrap_or_else(|_| absolute_path(&loaded.root.join(output)))
            }))
        }))
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| projection_path_key(path));
    paths.dedup_by(|left, right| projection_path_key(left) == projection_path_key(right));
    paths
}

fn stage_sync_rule(
    loaded: &LoadedProject,
    stage: &Path,
    source: &Path,
    relative: &Path,
    target_file: &Path,
    rule: &SyncRule,
) -> Result<()> {
    let middleware = normalize_sync_middleware(&rule.middleware);
    if middleware == "ignore" {
        return Ok(());
    }
    let mut target = target_file
        .parent()
        .unwrap_or(stage)
        .strip_prefix(stage)
        .with_context(|| format!("{} is outside the projection stage", target_file.display()))?
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    let name = sync_rule_instance_name(rule, relative)?;
    if name != "init" {
        target.push(name);
    }
    if target.is_empty() {
        bail!(
            "Sync rule '{}' maps {} outside a Studio service",
            rule.pattern,
            source.display()
        );
    }
    let script_class_name = match middleware.as_str() {
        "modulescript" => Some("ModuleScript"),
        "serverscript" | "pluginscript" => Some("Script"),
        "clientscript" => Some("LocalScript"),
        _ => None,
    };
    PROJECTION_TRANSFORM_STACK.with(|stack| -> Result<()> {
        if let Some(transforms) = stack.borrow_mut().last_mut() {
            if let Some(existing) = transforms
                .iter()
                .find(|transform| transform.target == target)
            {
                bail!(
                    "Sync-rule sources '{}' and '{}' both map to '{}'",
                    existing.source.display(),
                    source.display(),
                    target.join(".")
                );
            }
            transforms.push(ProjectionTransform {
                target: target.clone(),
                source: source.to_path_buf(),
                script_class_name,
            });
        }
        Ok(())
    })?;
    match middleware.as_str() {
        "modulescript" | "serverscript" | "clientscript" | "pluginscript" => {
            let source_text = fs::read_to_string(source)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            let class_name = script_class_name.expect("script middleware has a class");
            let mut properties =
                Map::from_iter([("Source".to_string(), Value::String(source_text))]);
            if middleware == "pluginscript" {
                properties.insert(
                    "RunContext".to_string(),
                    json!({
                        "_type": "EnumItem",
                        "enumType": "Enum.RunContext",
                        "name": "Plugin",
                    }),
                );
            }
            update_stage_instance(
                stage,
                &target,
                class_name,
                None,
                &properties,
                &Map::new(),
                None,
            )
        }
        "txt" => stage_text_value(stage, &target, source),
        "csv" => stage_localization_table(stage, &target, source),
        "model-json" => stage_model_json(stage, &target, source),
        "rbxm" | "rbxmx" => import_model_at_target(stage, &target, source),
        "nested-project" => {
            let nested = load_nested_project(source)?;
            stage_nested_project_at_target(&nested, stage, &target)
        }
        format => stage_module_data(loaded, stage, &target, source, format),
    }
}

fn apply_metadata_sidecar(
    loaded: &LoadedProject,
    stage: &Path,
    source: &Path,
    staged_path: &Path,
) -> Result<()> {
    let text =
        fs::read_to_string(source).with_context(|| format!("{} is not UTF-8", source.display()))?;
    let metadata: MetadataSidecar = serde_json::from_value(parse_jsonc_value(&text)?)
        .with_context(|| format!("Invalid metadata sidecar {}", source.display()))?;
    if metadata.schema_version.is_some_and(|version| version != 1) {
        bail!(
            "{} uses unsupported metadata schema version {}",
            source.display(),
            metadata.schema_version.unwrap_or_default()
        );
    }
    let file_name = staged_path
        .file_name()
        .and_then(OsStr::to_str)
        .context("Metadata sidecar has no UTF-8 file name")?;
    let stem = metadata_sidecar_stem(file_name).context("Invalid metadata sidecar name")?;
    let staged_relative = staged_path.strip_prefix(stage)?;
    let target = metadata_sidecar_target(loaded, staged_relative)?;
    let inferred_class = if stem == "init" {
        None
    } else {
        let synthetic = format!("{stem}.luau");
        let naming = project_script_naming(&loaded.project);
        let (class_name, _, _) =
            crate::infer_source_script(&synthetic, &naming).unwrap_or(("Folder", None, None));
        Some(class_name)
    };
    if target.is_empty() {
        bail!("{} doesn't identify a Studio instance", source.display());
    }
    let existing_class = stage_target_class(stage, &target)?;
    let class_name = metadata
        .class_name
        .as_deref()
        .or(existing_class.as_deref())
        .or(inferred_class)
        .with_context(|| {
            format!(
                "{} has no matching instance; set $className to create one",
                source.display()
            )
        })?;
    let properties = normalize_property_map(Some(class_name), &metadata.properties)
        .with_context(|| format!("Invalid properties in {}", source.display()))?;
    let attributes = normalize_property_map(None, &metadata.attributes)
        .with_context(|| format!("Invalid attributes in {}", source.display()))?;
    override_stage_identity(
        stage,
        &target,
        metadata.class_name.as_deref(),
        metadata.id.as_deref(),
    )?;
    update_stage_instance(
        stage,
        &target,
        class_name,
        metadata.id.as_deref(),
        &properties,
        &attributes,
        metadata.tags.as_deref(),
    )
}

fn metadata_sidecar_target(loaded: &LoadedProject, staged_relative: &Path) -> Result<Vec<String>> {
    let file_name = staged_relative
        .file_name()
        .and_then(OsStr::to_str)
        .context("Metadata sidecar has no UTF-8 file name")?;
    let stem = metadata_sidecar_stem(file_name).context("Invalid metadata sidecar name")?;
    let mut target = staged_relative
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    if stem != "init" {
        let synthetic = format!("{stem}.luau");
        let naming = project_script_naming(&loaded.project);
        let (_, leaf, _) =
            crate::infer_source_script(&synthetic, &naming).unwrap_or(("Folder", None, None));
        target.push(leaf.unwrap_or_else(|| stem.to_string()));
    }
    if target.is_empty() {
        bail!(
            "{} doesn't identify a Studio instance",
            staged_relative.display()
        );
    }
    Ok(target)
}

pub fn metadata_sidecar_ignore_unknown_targets(loaded: &LoadedProject) -> Result<Vec<Vec<String>>> {
    let mut targets = BTreeSet::new();
    for source in metadata_sidecar_files(loaded)? {
        let text = fs::read_to_string(&source)
            .with_context(|| format!("{} is not UTF-8", source.display()))?;
        let metadata: MetadataSidecar = serde_json::from_value(parse_jsonc_value(&text)?)
            .with_context(|| format!("Invalid metadata sidecar {}", source.display()))?;
        if metadata._ignore_unknown_instances != Some(true) {
            continue;
        }
        let staged_relative = project_source_to_staged_relative(loaded, &source)?
            .with_context(|| format!("{} has no projected target", source.display()))?;
        targets.insert(metadata_sidecar_target(loaded, &staged_relative)?);
    }
    Ok(targets.into_iter().collect())
}

fn nested_project_targets(
    loaded: &LoadedProject,
    prefix: &[String],
) -> Result<Vec<(Vec<String>, LoadedProject)>> {
    let mut projects = Vec::new();
    for (target, node) in project_tree_nodes(&loaded.project.tree) {
        let Some(source) = node.path.as_deref() else {
            continue;
        };
        let source = loaded.root.join(source);
        if source.is_file() && is_nested_project_path(&source) {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target);
            projects.push((nested_target, load_nested_project(&source)?));
        }
    }
    for mount in &loaded.project.mounts {
        let source = loaded.root.join(&mount.source);
        if source.is_file() && is_nested_project_path(&source) {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target_segments(&mount.target)?);
            projects.push((nested_target, load_nested_project(&source)?));
        }
    }
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::FromProject {
            continue;
        }
        let source = loaded.root.join(&adapter.source);
        if source.is_file()
            && normalize_sync_middleware(&adapter_format(adapter)?) == "nested-project"
        {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target_segments(&adapter.target)?);
            projects.push((nested_target, load_nested_project(&source)?));
        }
    }
    Ok(projects)
}

pub fn compiled_files_to_studio_filters(loaded: &LoadedProject) -> Result<Vec<FilterRule>> {
    fn append(
        loaded: &LoadedProject,
        prefix: &[String],
        visiting: &mut BTreeSet<PathBuf>,
        output: &mut Vec<FilterRule>,
    ) -> Result<()> {
        let project_path = fs::canonicalize(&loaded.path)
            .with_context(|| format!("Failed to resolve {}", loaded.path.display()))?;
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        for rule in &loaded.project.filters {
            if !matches!(
                rule.direction,
                FilterDirection::Both | FilterDirection::FilesToStudio
            ) {
                continue;
            }
            if prefix.is_empty() {
                output.push(rule.clone());
                continue;
            }
            let escaped_prefix = escape_glob(&filter_path_segments(prefix));
            if let Some(glob) = rule.glob.as_deref() {
                let mut nested = rule.clone();
                nested.glob = Some(format!("{escaped_prefix}/{}", glob.trim_start_matches('/')));
                output.push(nested);
            } else {
                let mut root = rule.clone();
                root.glob = Some(escaped_prefix.clone());
                output.push(root);
                let mut descendants = rule.clone();
                descendants.glob = Some(format!("{escaped_prefix}/**"));
                output.push(descendants);
            }
        }
        for (target, nested) in nested_project_targets(loaded, prefix)? {
            append(&nested, &target, visiting, output)?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut output = Vec::new();
    append(loaded, &[], &mut BTreeSet::new(), &mut output)?;
    Ok(output)
}

pub fn compiled_files_to_studio_ignore_unknown_targets(
    loaded: &LoadedProject,
    reconciled_services: &BTreeSet<String>,
) -> Result<Vec<Vec<String>>> {
    fn append(
        loaded: &LoadedProject,
        prefix: &[String],
        reconciled_services: &BTreeSet<String>,
        visiting: &mut BTreeSet<PathBuf>,
        output: &mut BTreeSet<Vec<String>>,
    ) -> Result<()> {
        let project_path = fs::canonicalize(&loaded.path)
            .with_context(|| format!("Failed to resolve {}", loaded.path.display()))?;
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        if loaded
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
        {
            if prefix.is_empty() {
                for service in loaded.project.tree.keys().chain(reconciled_services.iter()) {
                    output.insert(vec![service.clone()]);
                }
            } else {
                output.insert(prefix.to_vec());
            }
        }
        for (target, node) in project_tree_nodes(&loaded.project.tree) {
            if node.ignore_unknown_instances.unwrap_or(false) {
                let mut nested_target = prefix.to_vec();
                nested_target.extend(target);
                output.insert(nested_target);
            }
        }
        for target in metadata_sidecar_ignore_unknown_targets(loaded)? {
            let mut nested_target = prefix.to_vec();
            nested_target.extend(target);
            output.insert(nested_target);
        }
        for (target, nested) in nested_project_targets(loaded, prefix)? {
            append(&nested, &target, reconciled_services, visiting, output)?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut output = BTreeSet::new();
    append(
        loaded,
        &[],
        reconciled_services,
        &mut BTreeSet::new(),
        &mut output,
    )?;
    Ok(output.into_iter().collect())
}

fn metadata_sidecar_files(loaded: &LoadedProject) -> Result<Vec<PathBuf>> {
    let mut roots = vec![loaded.root.join(&loaded.project.source_root)];
    roots.extend(
        project_tree_nodes(&loaded.project.tree)
            .into_iter()
            .filter_map(|(_, node)| node.path.map(|path| loaded.root.join(path))),
    );
    roots.extend(
        loaded
            .project
            .mounts
            .iter()
            .map(|mount| loaded.root.join(&mount.source)),
    );
    let mut files = BTreeSet::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry?;
            if entry.file_type().is_file() && is_metadata_sidecar(entry.path()) {
                files.insert(absolute_path(entry.path()));
            }
        }
    }
    Ok(files.into_iter().collect())
}

fn projection_field_owners_with_root(
    loaded: &LoadedProject,
    include_root: bool,
) -> Result<Vec<ProjectionFieldOwner>> {
    fn collect(
        loaded: &LoadedProject,
        prefix: &[String],
        include_root: bool,
        visiting: &mut BTreeSet<PathBuf>,
        owners: &mut Vec<ProjectionFieldOwner>,
    ) -> Result<()> {
        let project_path =
            fs::canonicalize(&loaded.path).unwrap_or_else(|_| absolute_path(&loaded.path));
        if !visiting.insert(project_path.clone()) {
            bail!("Nested project cycle includes {}", loaded.path.display());
        }
        let root = &loaded.project.root;
        if include_root
            && root.class_name.as_deref() != Some("DataModel")
            && (root.class_name.is_some()
                || root.id.is_some()
                || !root.properties.is_empty()
                || !root.attributes.is_empty()
                || root.tags.is_some())
        {
            owners.push(ProjectionFieldOwner {
                target: prefix.to_vec(),
                source: format!("{} root", loaded.path.display()),
                class_name: root.class_name.is_some(),
                settings_id: root.id.is_some(),
                properties: root.properties.keys().cloned().collect(),
                attributes: root.attributes.keys().cloned().collect(),
                tags: root.tags.is_some(),
            });
        }
        let tree_nodes = project_tree_nodes(&loaded.project.tree);
        for (target, node) in &tree_nodes {
            let mut prefixed = prefix.to_vec();
            prefixed.extend(target.iter().cloned());
            owners.push(ProjectionFieldOwner {
                target: prefixed,
                source: format!("{} tree '{}'", loaded.path.display(), target.join(".")),
                class_name: node.class_name.is_some(),
                settings_id: node.id.is_some(),
                properties: node.properties.keys().cloned().collect(),
                attributes: node.attributes.keys().cloned().collect(),
                tags: node.tags.is_some(),
            });
        }
        for mut owner in metadata_sidecar_field_owners(loaded)? {
            let mut prefixed = prefix.to_vec();
            prefixed.append(&mut owner.target);
            owner.target = prefixed;
            owners.push(owner);
        }
        let mut nested = Vec::new();
        for (target, node) in tree_nodes {
            if let Some(source) = node.path {
                let source = loaded.root.join(source);
                if source.is_file() && is_nested_project_path(&source) {
                    nested.push((target, source));
                }
            }
        }
        for mount in &loaded.project.mounts {
            let source = loaded.root.join(&mount.source);
            if source.is_file() && is_nested_project_path(&source) {
                nested.push((target_segments(&mount.target)?, source));
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let source = loaded.root.join(&adapter.source);
            if source.is_file() && adapter_format(adapter)? == "nested-project" {
                nested.push((target_segments(&adapter.target)?, source));
            }
        }
        for (target, source) in nested {
            let nested_project = load_nested_project(&source)?;
            let mut nested_prefix = prefix.to_vec();
            nested_prefix.extend(target);
            collect(&nested_project, &nested_prefix, true, visiting, owners)?;
        }
        visiting.remove(&project_path);
        Ok(())
    }

    let mut owners = Vec::new();
    collect(loaded, &[], include_root, &mut BTreeSet::new(), &mut owners)?;
    owners.sort_by(|left, right| {
        left.target
            .cmp(&right.target)
            .then(left.source.cmp(&right.source))
    });
    Ok(owners)
}

fn projection_field_owners(loaded: &LoadedProject) -> Result<Vec<ProjectionFieldOwner>> {
    projection_field_owners_with_root(loaded, false)
}

fn metadata_sidecar_field_owners(loaded: &LoadedProject) -> Result<Vec<ProjectionFieldOwner>> {
    let mut owners = Vec::new();
    for source in metadata_sidecar_files(loaded)? {
        let text = fs::read_to_string(&source)
            .with_context(|| format!("{} is not UTF-8", source.display()))?;
        let metadata: MetadataSidecar = serde_json::from_value(parse_jsonc_value(&text)?)
            .with_context(|| format!("Invalid metadata sidecar {}", source.display()))?;
        let staged_relative = project_source_to_staged_relative(loaded, &source)?
            .with_context(|| format!("{} has no projected target", source.display()))?;
        owners.push(ProjectionFieldOwner {
            target: metadata_sidecar_target(loaded, &staged_relative)?,
            source: source.display().to_string(),
            class_name: metadata.class_name.is_some(),
            settings_id: metadata.id.is_some(),
            properties: metadata.properties.keys().cloned().collect(),
            attributes: metadata.attributes.keys().cloned().collect(),
            tags: metadata.tags.is_some(),
        });
    }
    Ok(owners)
}

pub fn project_target_is_declarative(loaded: &LoadedProject, target: &[String]) -> Result<bool> {
    Ok(projection_field_owners(loaded)?
        .iter()
        .any(|owner| owner.target == target))
}

pub fn project_structural_store(loaded: &LoadedProject, target: &[String]) -> Result<PathBuf> {
    let relative = target.iter().collect::<PathBuf>();
    let resolution = resolve_project_write_path(loaded, &relative)?;
    if resolution.source_root.is_file() {
        if matches!(
            resolution.source_root.extension().and_then(OsStr::to_str),
            Some("renium" | "rbsync")
        ) {
            return Ok(resolution.source_root);
        }
        bail!(
            "Projected path '{}' is owned by file {}; edit that file directly",
            target.join("."),
            resolution.source_root.display()
        );
    }
    let settings = crate::existing_service_settings_path(&resolution.source_root);
    if !settings.is_file() {
        bail!(
            "Projected path '{}' is owned by '{}' but it has no Renium settings store",
            target.join("."),
            resolution.source_root.display()
        );
    }
    Ok(settings)
}

fn canonical_owned_value(value: Option<&Value>, document: &SettingsBytecode) -> Option<Value> {
    let mut record = Map::new();
    if let Some(value) = value {
        record.insert("value".to_string(), value.clone());
    }
    stabilize_reference_indices(&mut record, &document.instances);
    record.remove("value")
}

fn validate_projection_field_ownership(
    loaded: &LoadedProject,
    documents: &HashMap<String, SettingsBytecode>,
    baseline_documents: &HashMap<String, SettingsBytecode>,
) -> Result<()> {
    for owner in projection_field_owners(loaded)? {
        let service = owner
            .target
            .first()
            .with_context(|| format!("{} has an empty projected target", owner.source))?;
        let imported = documents
            .get(service)
            .with_context(|| format!("Studio removed declared service '{service}'"))?;
        let baseline = baseline_documents
            .get(service)
            .with_context(|| format!("Baseline is missing declared service '{service}'"))?;
        let baseline_index = find_document_target(baseline, &owner.target).with_context(|| {
            format!(
                "{} does not resolve to '{}'",
                owner.source,
                owner.target.join(".")
            )
        })?;
        let imported_index = find_document_target(imported, &owner.target).map_err(|_| {
            let baseline_id = &baseline.instances[baseline_index].settings_id;
            if let Some(instance) = imported
                .instances
                .iter()
                .find(|instance| &instance.settings_id == baseline_id)
            {
                anyhow::anyhow!(
                    "Studio renamed config-owned instance '{}' to '{}'; rename it in {} instead",
                    owner.target.join("."),
                    instance.name,
                    owner.source
                )
            } else {
                anyhow::anyhow!(
                    "Studio removed config-owned instance '{}'; edit {} instead",
                    owner.target.join("."),
                    owner.source
                )
            }
        })?;
        let original = &baseline.instances[baseline_index];
        let changed = &imported.instances[imported_index];
        if owner.class_name && changed.class_name != original.class_name {
            bail!(
                "Studio changed config-owned ClassName on '{}'; edit {} instead",
                owner.target.join("."),
                owner.source
            );
        }
        if owner.settings_id && changed.settings_id != original.settings_id {
            bail!(
                "Studio changed config-owned id on '{}'; edit {} instead",
                owner.target.join("."),
                owner.source
            );
        }
        for property in &owner.properties {
            if canonical_owned_value(changed.properties.get(property), imported)
                != canonical_owned_value(original.properties.get(property), baseline)
            {
                bail!(
                    "Studio changed config-owned property '{}.{}'; edit {} instead",
                    owner.target.join("."),
                    property,
                    owner.source
                );
            }
        }
        for attribute in &owner.attributes {
            if canonical_owned_value(changed.attributes.get(attribute), imported)
                != canonical_owned_value(original.attributes.get(attribute), baseline)
            {
                bail!(
                    "Studio changed config-owned attribute '{}.{}'; edit {} instead",
                    owner.target.join("."),
                    attribute,
                    owner.source
                );
            }
        }
        if owner.tags
            && canonical_owned_value(changed.properties.get("Tags"), imported)
                != canonical_owned_value(original.properties.get("Tags"), baseline)
        {
            bail!(
                "Studio changed config-owned tags on '{}'; edit {} instead",
                owner.target.join("."),
                owner.source
            );
        }
    }
    Ok(())
}

fn normalize_property_map(
    class_name: Option<&str>,
    values: &Map<String, Value>,
) -> Result<Map<String, Value>> {
    values
        .iter()
        .map(|(name, value)| {
            if contains_reference_value(value) {
                if class_name.is_none() {
                    bail!("Attributes cannot contain instance references");
                }
                return Ok((name.clone(), value.clone()));
            }
            Ok((
                name.clone(),
                crate::normalize_project_typed_value(class_name, Some(name), value)
                    .with_context(|| format!("Invalid value for '{name}'"))?,
            ))
        })
        .collect()
}

fn normalize_stage_references(stage: &Path) -> Result<()> {
    let mut paths = Vec::new();
    let mut documents = Vec::new();
    for entry in fs::read_dir(stage)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let settings_path = crate::existing_service_settings_path(&entry.path());
        if settings_path.is_file() {
            paths.push(settings_path.clone());
            documents.push(SettingsBytecode::read_file(&settings_path)?);
        }
    }
    let originals = documents.clone();
    canonicalize_projection_references(&mut documents)?;
    for ((settings_path, document), original) in paths.iter().zip(&documents).zip(&originals) {
        if document != original {
            document.write_file(settings_path)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct ProjectionReferenceTarget {
    settings_id: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
}

fn canonicalize_projection_references(documents: &mut [SettingsBytecode]) -> Result<()> {
    let mut targets = Vec::<ProjectionReferenceTarget>::new();
    let mut target_by_document_instance = Vec::with_capacity(documents.len());
    let mut by_settings_id = HashMap::<String, Vec<usize>>::new();
    let mut by_path_segments = HashMap::<Vec<String>, Vec<usize>>::new();
    let mut by_path_parts = HashMap::<(Vec<String>, Vec<usize>), usize>::new();
    for document in documents.iter() {
        let parts = projection_instance_path_parts(document);
        let mut target_indices = Vec::with_capacity(document.instances.len());
        for (instance_index, instance) in document.instances.iter().enumerate() {
            let (path_segments, path_ordinals) = parts[instance_index].clone();
            let target_index = targets.len();
            targets.push(ProjectionReferenceTarget {
                settings_id: instance.settings_id.clone(),
                path_segments: path_segments.clone(),
                path_ordinals: path_ordinals.clone(),
            });
            target_indices.push(target_index);
            by_settings_id
                .entry(instance.settings_id.clone())
                .or_default()
                .push(target_index);
            by_path_segments
                .entry(path_segments.clone())
                .or_default()
                .push(target_index);
            if by_path_parts
                .insert((path_segments, path_ordinals), target_index)
                .is_some()
            {
                bail!("Projected DataModel contains duplicate structured instance paths");
            }
        }
        target_by_document_instance.push(target_indices);
    }

    for (document_index, document) in documents.iter_mut().enumerate() {
        for instance in &mut document.instances {
            canonicalize_record_references(
                &mut instance.properties,
                document_index,
                &targets,
                &target_by_document_instance,
                &by_settings_id,
                &by_path_segments,
                &by_path_parts,
            )
            .with_context(|| {
                format!(
                    "Invalid reference on {} ({})",
                    instance.name, instance.settings_id
                )
            })?;
            canonicalize_record_references(
                &mut instance.attributes,
                document_index,
                &targets,
                &target_by_document_instance,
                &by_settings_id,
                &by_path_segments,
                &by_path_parts,
            )
            .with_context(|| {
                format!(
                    "Invalid attribute reference on {} ({})",
                    instance.name, instance.settings_id
                )
            })?;
        }
    }
    Ok(())
}

fn canonicalize_projection_document_map(
    documents: &mut HashMap<String, SettingsBytecode>,
) -> Result<()> {
    let mut keys = documents.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    let mut values = keys
        .iter()
        .filter_map(|key| documents.get(key).cloned())
        .collect::<Vec<_>>();
    canonicalize_projection_references(&mut values)?;
    for (key, value) in keys.into_iter().zip(values) {
        documents.insert(key, value);
    }
    Ok(())
}

fn canonicalize_record_references(
    record: &mut Map<String, Value>,
    document_index: usize,
    targets: &[ProjectionReferenceTarget],
    target_by_document_instance: &[Vec<usize>],
    by_settings_id: &HashMap<String, Vec<usize>>,
    by_path_segments: &HashMap<Vec<String>, Vec<usize>>,
    by_path_parts: &HashMap<(Vec<String>, Vec<usize>), usize>,
) -> Result<()> {
    struct ReferenceIndex<'a> {
        document_index: usize,
        targets: &'a [ProjectionReferenceTarget],
        target_by_document_instance: &'a [Vec<usize>],
        by_settings_id: &'a HashMap<String, Vec<usize>>,
        by_path_segments: &'a HashMap<Vec<String>, Vec<usize>>,
        by_path_parts: &'a HashMap<(Vec<String>, Vec<usize>), usize>,
    }

    fn visit(value: &mut Value, force_reference: bool, index: &ReferenceIndex<'_>) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, false, index)?;
                }
            }
            Value::Object(object) => {
                let is_reference = force_reference
                    || object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId")
                    || object.contains_key("instanceIndex")
                    || object.contains_key("referent")
                    || object.contains_key("ref")
                    || object.contains_key("debugId")
                    || object.contains_key("pathSegments")
                    || object.contains_key("pathOrdinals")
                    || object.contains_key("path");
                if is_reference {
                    let mut constraints = Vec::<HashSet<usize>>::new();
                    let mut selector_present = false;
                    if let Some(raw_index) = object.get("instanceIndex") {
                        selector_present = true;
                        let instance_index = raw_index
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .and_then(|index| index.checked_sub(1))
                            .context("Reference instanceIndex must be a positive integer")?;
                        let target = index
                            .target_by_document_instance
                            .get(index.document_index)
                            .and_then(|indices| indices.get(instance_index))
                            .copied()
                            .context("Reference instanceIndex does not exist")?;
                        constraints.push(HashSet::from([target]));
                    }
                    if let Some(raw_settings_id) = object.get("settingsId") {
                        selector_present = true;
                        let settings_id = raw_settings_id
                            .as_str()
                            .context("Reference settingsId must be a string")?;
                        constraints.push(
                            index
                                .by_settings_id
                                .get(settings_id)
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect(),
                        );
                    }
                    for key in ["instanceId", "referent", "ref"] {
                        if let Some(raw_id) = object.get(key) {
                            selector_present = true;
                            let id = raw_id
                                .as_str()
                                .with_context(|| format!("Reference {key} must be a string"))?;
                            constraints.push(
                                index
                                    .by_settings_id
                                    .get(id)
                                    .cloned()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .collect(),
                            );
                        }
                    }
                    if let Some(raw_debug_id) = object.get("debugId") {
                        selector_present = true;
                        let debug_id = raw_debug_id
                            .as_str()
                            .context("Reference debugId must be a string")?;
                        constraints.push(
                            index
                                .by_settings_id
                                .get(&format!("debug:{debug_id}"))
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect(),
                        );
                    }
                    if let Some(raw_path_values) = object.get("pathSegments") {
                        selector_present = true;
                        let path_values = raw_path_values
                            .as_array()
                            .context("Reference pathSegments must be an array")?;
                        let segments = path_values
                            .iter()
                            .map(|segment| {
                                segment
                                    .as_str()
                                    .map(str::to_string)
                                    .context("Reference pathSegments must contain strings")
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let candidates = if let Some(raw_ordinal_values) =
                            object.get("pathOrdinals")
                        {
                            let ordinal_values = raw_ordinal_values
                                .as_array()
                                .context("Reference pathOrdinals must be an array")?;
                            let ordinals = ordinal_values
                                .iter()
                                .map(|value| {
                                    value
                                        .as_u64()
                                        .filter(|value| *value > 0)
                                        .and_then(|value| usize::try_from(value).ok())
                                        .context(
                                            "Reference pathOrdinals must contain positive integers",
                                        )
                                })
                                .collect::<Result<Vec<_>>>()?;
                            if ordinals.len() != segments.len() {
                                bail!(
                                    "Reference pathOrdinals must contain one value per path segment"
                                );
                            }
                            index
                                .by_path_parts
                                .get(&(segments, ordinals))
                                .copied()
                                .into_iter()
                                .collect()
                        } else {
                            index
                                .by_path_segments
                                .get(&segments)
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect()
                        };
                        constraints.push(candidates);
                    } else if object.contains_key("pathOrdinals") {
                        bail!("Reference pathOrdinals require pathSegments");
                    }
                    if object.contains_key("path") {
                        bail!("Reference path is unsupported; use pathSegments and pathOrdinals");
                    }
                    if constraints.is_empty() {
                        if selector_present {
                            bail!("Reference target does not exist in the projected DataModel");
                        }
                    } else {
                        let mut candidates = constraints.remove(0);
                        for constraint in constraints {
                            candidates.retain(|candidate| constraint.contains(candidate));
                        }
                        if candidates.is_empty() {
                            bail!("Reference selectors do not identify the same instance");
                        }
                        if candidates.len() != 1 {
                            bail!("Reference target is ambiguous; include pathOrdinals");
                        }
                        let target = &index.targets[*candidates.iter().next().unwrap()];
                        object.remove("instanceIndex");
                        object.remove("instanceId");
                        object.remove("debugId");
                        object.remove("path");
                        object.remove("referent");
                        object.remove("ref");
                        object.insert(
                            "settingsId".to_string(),
                            Value::String(target.settings_id.clone()),
                        );
                        object.insert(
                            "pathSegments".to_string(),
                            Value::Array(
                                target
                                    .path_segments
                                    .iter()
                                    .cloned()
                                    .map(Value::String)
                                    .collect(),
                            ),
                        );
                        object.insert(
                            "pathOrdinals".to_string(),
                            Value::Array(
                                target
                                    .path_ordinals
                                    .iter()
                                    .map(|value| json!(value))
                                    .collect(),
                            ),
                        );
                    }
                }
                let keys = object.keys().cloned().collect::<Vec<_>>();
                for key in keys {
                    if let Some(value) = object.get_mut(&key) {
                        visit(value, key == "Ref", index)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    let index = ReferenceIndex {
        document_index,
        targets,
        target_by_document_instance,
        by_settings_id,
        by_path_segments,
        by_path_parts,
    };
    for value in record.values_mut() {
        visit(value, false, &index)?;
    }
    Ok(())
}

fn stage_target_class(stage: &Path, target: &[String]) -> Result<Option<String>> {
    let Some(service) = target.first() else {
        return Ok(None);
    };
    let service_dir = stage.join(service);
    if !service_dir.is_dir() {
        return Ok(None);
    }
    let settings_path = crate::writable_service_settings_path(&service_dir)?;
    let document = if settings_path.is_file() {
        SettingsBytecode::read_file(&settings_path)?
    } else {
        crate::source_only_settings_document(&service_dir, service)?
    };
    Ok(find_document_target(&document, target)
        .ok()
        .map(|index| document.instances[index].class_name.clone()))
}

fn override_stage_identity(
    stage: &Path,
    target: &[String],
    class_name: Option<&str>,
    settings_id: Option<&str>,
) -> Result<()> {
    if class_name.is_none() && settings_id.is_none() {
        return Ok(());
    }
    let service = target
        .first()
        .context("Projection target must include a service")?;
    let service_dir = stage.join(service);
    if !service_dir.is_dir() {
        return Ok(());
    }
    let settings_path = crate::writable_service_settings_path(&service_dir)?;
    let mut document = if settings_path.is_file() {
        SettingsBytecode::read_file(&settings_path)?
    } else {
        crate::source_only_settings_document(&service_dir, service)?
    };
    let Ok(index) = find_document_target(&document, target) else {
        return Ok(());
    };
    if let Some(settings_id) = settings_id {
        if document
            .instances
            .iter()
            .enumerate()
            .any(|(other, instance)| other != index && instance.settings_id == settings_id)
        {
            bail!("Projection settings id '{settings_id}' is used more than once");
        }
        document.instances[index].settings_id = settings_id.to_string();
    }
    if let Some(class_name) = class_name {
        document.instances[index].class_name = class_name.to_string();
    }
    document.write_file(&settings_path)
}

fn refresh_stage_settings(stage: &Path) -> Result<()> {
    let mut service_dirs = fs::read_dir(stage)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    service_dirs.sort();
    for service_dir in service_dirs {
        refresh_stage_service_settings(&service_dir)?;
    }
    Ok(())
}

fn refresh_stage_service_settings(service_dir: &Path) -> Result<()> {
    let service = service_dir
        .file_name()
        .and_then(OsStr::to_str)
        .context("Projection service name is not UTF-8")?;
    let generated = crate::source_only_settings_document(service_dir, service)?;
    let settings_path = crate::writable_service_settings_path(service_dir)?;
    if !settings_path.is_file() {
        return generated.write_file(&settings_path);
    }
    let mut document = SettingsBytecode::read_file(&settings_path)?;
    merge_source_only_document(&mut document, &generated)?;
    document.write_file(&settings_path)
}

fn merge_source_only_document(
    destination: &mut SettingsBytecode,
    generated: &SettingsBytecode,
) -> Result<()> {
    if destination.instances.is_empty() {
        destination.instances = generated.instances.clone();
        return Ok(());
    }
    let mut remap = BTreeMap::new();
    for (index, instance) in generated.instances.iter().enumerate() {
        let parent = instance
            .parent_index
            .and_then(|parent| remap.get(&parent).copied());
        let matches = destination
            .instances
            .iter()
            .enumerate()
            .filter(|(_, existing)| {
                existing.parent_index == parent
                    && existing.name == instance.name
                    && existing.class_name == instance.class_name
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let mapped = match matches.as_slice() {
            [only] => *only,
            [] => {
                let mapped = destination.instances.len();
                let mut appended = instance.clone();
                appended.parent_index = parent;
                if destination
                    .instances
                    .iter()
                    .any(|existing| existing.settings_id == appended.settings_id)
                {
                    appended.settings_id = projection_settings_id(
                        "source",
                        &format!("{}:{index}", appended.settings_id),
                    );
                }
                destination.instances.push(appended);
                mapped
            }
            _ if !generated
                .instances
                .iter()
                .any(|child| child.parent_index == Some(index)) =>
            {
                matches[0]
            }
            _ => {
                bail!(
                    "Source-only projection is ambiguous at '{}' ({})",
                    instance.name,
                    instance.class_name
                )
            }
        };
        remap.insert(index, mapped);
    }
    Ok(())
}

fn file_target_destination(loaded: &LoadedProject, source: &Path, target: &Path) -> PathBuf {
    if target.extension().is_some() {
        target.to_path_buf()
    } else {
        let source_name = source.file_name().and_then(OsStr::to_str).unwrap_or("");
        let naming = project_script_naming(&loaded.project);
        if let Some((_, stem, _)) = crate::infer_source_script(source_name, &naming) {
            let prefix_len = stem.as_deref().map(str::len).unwrap_or(4);
            let suffix = &source_name[prefix_len..];
            target.with_file_name(format!(
                "{}{suffix}",
                target.file_name().and_then(OsStr::to_str).unwrap_or("")
            ))
        } else if let Some(extension) = source.extension().and_then(OsStr::to_str) {
            target.with_extension(extension)
        } else {
            target.to_path_buf()
        }
    }
}

fn copy_file_to_target(loaded: &LoadedProject, source: &Path, target: &Path) -> Result<()> {
    let destination = file_target_destination(loaded, source, target);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(())
}

fn adapter_key(adapter: &AdapterSpec) -> String {
    let segments = adapter.target.segments();
    let mut ordinals = adapter.target.ordinals();
    ordinals.resize(segments.len(), 1);
    format!(
        "{}\0{}",
        path_slash(&adapter.source),
        serde_json::to_string(&(segments, ordinals)).unwrap_or_else(|_| adapter.target.to_string())
    )
}

fn migrate_adapter_baseline_entries(project: &ReniumProject, baseline: &mut AdapterBaseline) {
    for adapter in &project.adapters {
        let canonical = adapter_key(adapter);
        if baseline.entries.contains_key(&canonical) {
            continue;
        }
        let source = path_slash(&adapter.source);
        let legacy = baseline.entries.keys().find_map(|key| {
            let (key_source, raw_target) = key.split_once('\0')?;
            if key_source != source {
                return None;
            }
            let target = serde_json::from_str::<ProjectTarget>(raw_target).ok()?;
            targets_are_equal(&target, &adapter.target).then(|| key.clone())
        });
        if let Some(legacy) = legacy
            && let Some(entry) = baseline.entries.remove(&legacy)
        {
            baseline.entries.insert(canonical, entry);
        }
    }
}

fn reversible_adapter_format(format: &str) -> bool {
    matches!(format, "txt" | "csv" | "model-json")
}

fn adapter_target_bytes_from_root(
    loaded: &LoadedProject,
    root: &Path,
    adapter: &AdapterSpec,
    format: &str,
) -> Result<Option<Vec<u8>>> {
    with_project_target(&adapter.target, |target| {
        let service = target
            .first()
            .context("Adapter target must include a service")?;
        let settings = crate::existing_service_settings_path(&root.join(service));
        if !settings.is_file() {
            return Ok(None);
        }
        let document = SettingsBytecode::read_file(&settings)?;
        if find_document_target_optional(&document, target)?.is_none() {
            return Ok(None);
        }
        reversible_adapter_target_bytes(
            adapter,
            format,
            &document,
            target,
            &loaded.root.join(&adapter.source),
            None,
        )
        .map(Some)
    })
}

fn create_adapter_stage(loaded: &LoadedProject, name: &str) -> Result<ProjectionStage> {
    let root = fresh_projection_stage(
        &loaded.root.join(".renium").join("build-staging"),
        &format!("adapter-{name}-"),
    )?;
    if let Some(source_root) = loaded
        .root
        .join(&loaded.project.source_root)
        .is_dir()
        .then(|| loaded.root.join(&loaded.project.source_root))
    {
        copy_directory_tree(&source_root, &root)?;
    }
    cache_script_naming(&root, &loaded.project);
    Ok(ProjectionStage {
        root,
        temporary: true,
        cleanup: true,
        transforms: Vec::new(),
        identities: HashMap::new(),
    })
}

fn build_adapters(loaded: &LoadedProject, check: bool, emit: bool) -> Result<()> {
    compile_projection(loaded)?;
    let mut changed = Vec::new();
    let baseline_path = loaded.root.join(".renium").join("adapter-baseline.json");
    let mut baseline = if baseline_path.is_file() {
        serde_json::from_slice::<AdapterBaseline>(&fs::read(&baseline_path)?)
            .with_context(|| format!("Invalid adapter baseline {}", baseline_path.display()))?
    } else {
        AdapterBaseline::default()
    };
    migrate_adapter_baseline_entries(&loaded.project, &mut baseline);
    let mut transaction_paths = Vec::new();
    let mut active_baseline_keys = BTreeSet::new();
    let mut active_outputs = BTreeMap::new();
    let mut active_output_owned = BTreeMap::new();
    for adapter in &loaded.project.adapters {
        let key = adapter_key(adapter);
        active_baseline_keys.insert(key.clone());
        let format = adapter_format(adapter)?;
        if adapter.direction == AdapterDirection::FromProject {
            active_outputs.insert(key, None);
            continue;
        }
        let output = adapter_output_path(loaded, adapter, &format)?;
        let owned = output.as_deref().is_some_and(|path| {
            baseline.entries.get(&key).is_some_and(|entry| {
                entry.output_owned
                    && entry.output.as_deref().is_some_and(|previous| {
                        absolute_path(&loaded.root.join(previous)) == absolute_path(path)
                    })
            }) || !path.exists()
        });
        if let Some(output) = output.as_ref() {
            transaction_paths.push(output.clone());
        }
        if reversible_adapter_format(&format) {
            let target = target_segments(&adapter.target)?;
            let service = target
                .first()
                .context("Adapter target must include a service")?;
            transaction_paths.push(crate::writable_service_settings_path(
                &loaded.root.join(&loaded.project.source_root).join(service),
            )?);
        }
        active_output_owned.insert(key.clone(), owned);
        active_outputs.insert(key, output);
    }
    for (key, entry) in &baseline.entries {
        if let Some(output) = entry.output.as_deref() {
            transaction_paths.push(loaded.root.join(output));
        }
        if active_baseline_keys.contains(key) {
            continue;
        }
        let Some((_, target)) = key.split_once('\0') else {
            continue;
        };
        let target = serde_json::from_str::<ProjectTarget>(target)
            .unwrap_or_else(|_| ProjectTarget::Shorthand(target.to_string()));
        let target = target_segments(&target)?;
        if let Some(service) = target.first() {
            transaction_paths.push(crate::writable_service_settings_path(
                &loaded.root.join(&loaded.project.source_root).join(service),
            )?);
        }
    }
    if !active_baseline_keys.is_empty() || baseline_path.exists() {
        transaction_paths.push(baseline_path.clone());
    }
    transaction_paths.sort();
    transaction_paths.dedup();
    let originals = if check {
        Vec::new()
    } else {
        transaction_paths
            .iter()
            .map(|path| {
                fs::read(path).map(Some).or_else(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        Ok(None)
                    } else {
                        Err(error)
                    }
                })
            })
            .collect::<io::Result<Vec<_>>>()?
    };
    let result = (|| -> Result<()> {
        prune_stale_adapter_targets(
            loaded,
            &mut baseline,
            &active_baseline_keys,
            &active_outputs,
            check,
            &mut changed,
        )?;
        for adapter in &loaded.project.adapters {
            if adapter.direction != AdapterDirection::FromProject {
                continue;
            }
            let key = adapter_key(adapter);
            let Some(entry) = baseline.entries.get_mut(&key) else {
                continue;
            };
            if entry.output.is_none() && entry.output_hash.is_none() && !entry.output_owned {
                continue;
            }
            if check {
                changed.push(format!("adapter baseline {}", adapter.source.display()));
            } else {
                entry.output = None;
                entry.output_hash = None;
                entry.output_owned = false;
            }
        }
        let reversible = loaded
            .project
            .adapters
            .iter()
            .filter_map(|adapter| {
                let format = adapter_format(adapter).ok()?;
                (adapter.direction != AdapterDirection::FromProject
                    && reversible_adapter_format(&format))
                .then_some((adapter, format))
            })
            .collect::<Vec<_>>();
        let mut baseline_updates = BTreeMap::<String, (String, String)>::new();
        if !reversible.is_empty() {
            let expected_stage = create_adapter_stage(loaded, "expected")?;
            for (adapter, _) in &reversible {
                stage_adapter(loaded, expected_stage.root(), adapter)?;
            }
            let canonical_root = loaded.root.join(&loaded.project.source_root);
            let mut apply = Vec::new();
            for (adapter, format) in &reversible {
                let key = adapter_key(adapter);
                let source_hash = bytes_hash(&fs::read(loaded.root.join(&adapter.source))?);
                let current =
                    adapter_target_bytes_from_root(loaded, &canonical_root, adapter, format)?;
                let expected =
                    adapter_target_bytes_from_root(loaded, expected_stage.root(), adapter, format)?
                        .context("Adapter staging did not create its target")?;
                let current_hash = current.as_deref().map(bytes_hash);
                let expected_hash = bytes_hash(&expected);
                let equal = current.as_deref() == Some(expected.as_slice());
                let mut apply_source = adapter.direction == AdapterDirection::ToProject;
                let mut update_baseline = equal;
                if adapter.direction == AdapterDirection::TwoWay && !equal {
                    if let Some(previous) = baseline.entries.get(&key) {
                        if current.is_none() {
                            bail!(
                                "Two-way adapter target '{}' was deleted after its last successful sync; restore it from {} or remove the adapter",
                                adapter.target,
                                adapter.source.display()
                            );
                        }
                        let source_changed = source_hash != previous.source_hash;
                        let target_changed =
                            current_hash.as_deref() != Some(previous.target_hash.as_str());
                        match (source_changed, target_changed) {
                            (true, false) => apply_source = true,
                            (false, true) => {}
                            (true, true) => {
                                bail!(
                                    "Two-way adapter conflict for '{}': both {} and its canonical target changed since the last successful sync",
                                    adapter.target,
                                    adapter.source.display()
                                );
                            }
                            (false, false) => {
                                bail!(
                                    "Two-way adapter '{}' has a divergent baseline; edit one side before building",
                                    adapter.target
                                );
                            }
                        }
                    } else {
                        apply_source = true;
                    }
                }
                if apply_source {
                    apply.push((*adapter, format.clone()));
                    update_baseline = true;
                }
                if update_baseline {
                    baseline_updates.insert(key, (source_hash, expected_hash));
                }
            }
            if !apply.is_empty() {
                let output_stage = create_adapter_stage(loaded, "apply")?;
                let mut services = BTreeSet::new();
                for (adapter, _) in &apply {
                    stage_adapter(loaded, output_stage.root(), adapter)?;
                    let target = target_segments(&adapter.target)?;
                    services.insert(
                        target
                            .first()
                            .context("Adapter target must include a service")?
                            .clone(),
                    );
                }
                for service in services {
                    let staged =
                        crate::existing_service_settings_path(&output_stage.root().join(&service));
                    let canonical =
                        crate::writable_service_settings_path(&canonical_root.join(&service))?;
                    compare_or_write(&canonical, &fs::read(&staged)?, check, &mut changed)?;
                }
            }
        }
        for adapter in &loaded.project.adapters {
            if adapter.direction == AdapterDirection::FromProject {
                continue;
            }
            let source = loaded.root.join(&adapter.source);
            let format = adapter_format(adapter)?;
            validate_adapter_source(&source, &format)?;
            if let Some(output) = adapter_output_path(loaded, adapter, &format)? {
                let bytes = render_adapter(&source, &format)?;
                compare_or_write(&output, &bytes, check, &mut changed)?;
            }
        }
        if active_baseline_keys.is_empty() {
            if baseline_path.is_file() {
                if check {
                    changed.push(path_slash(&baseline_path));
                } else {
                    fs::remove_file(&baseline_path)?;
                }
            }
        } else if !check {
            for adapter in &loaded.project.adapters {
                if adapter.direction == AdapterDirection::FromProject {
                    continue;
                }
                let format = adapter_format(adapter)?;
                let source_bytes = fs::read(loaded.root.join(&adapter.source))?;
                let key = adapter_key(adapter);
                let target_hash = if reversible_adapter_format(&format) {
                    let Some((_, target_hash)) = baseline_updates.get(&key) else {
                        continue;
                    };
                    target_hash.clone()
                } else {
                    String::new()
                };
                let output = adapter_output_path(loaded, adapter, &format)?;
                let output_hash = output
                    .as_deref()
                    .map(fs::read)
                    .transpose()?
                    .map(|bytes| bytes_hash(&bytes));
                baseline.entries.insert(
                    key.clone(),
                    AdapterBaselineEntry {
                        source_hash: bytes_hash(&source_bytes),
                        target_hash,
                        format: Some(format.clone()),
                        output: output
                            .as_deref()
                            .and_then(|path| path.strip_prefix(&loaded.root).ok())
                            .map(path_slash),
                        output_hash,
                        output_owned: active_output_owned.get(&key).copied().unwrap_or(false),
                        model_json_hierarchical: if format == "model-json" {
                            Some(model_json_source_is_hierarchical(
                                &loaded.root.join(&adapter.source),
                            )?)
                        } else {
                            None
                        },
                    },
                );
            }
            atomic_write(&baseline_path, &serde_json::to_vec_pretty(&baseline)?)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if !check {
            restore_file_snapshots(&transaction_paths, &originals)?;
        }
        return Err(error);
    }
    if check && !changed.is_empty() {
        bail!("Generated adapter output is stale: {}", changed.join(", "));
    }
    if !emit {
        return Ok(());
    }
    crate::emit_global_output(
        &json!({
            "ok": true,
            "checked": check,
            "adapters": loaded.project.adapters.len(),
            "project": loaded.path,
        }),
        &format!(
            "{} {} adapter output{}",
            if check { "Checked" } else { "Built" },
            loaded.project.adapters.len(),
            if loaded.project.adapters.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
    )
}

fn prune_stale_adapter_targets(
    loaded: &LoadedProject,
    baseline: &mut AdapterBaseline,
    active_keys: &BTreeSet<String>,
    active_outputs: &BTreeMap<String, Option<PathBuf>>,
    check: bool,
    changed: &mut Vec<String>,
) -> Result<()> {
    for (key, entry) in &baseline.entries {
        let Some(previous_output) = entry.output.as_deref() else {
            continue;
        };
        let previous_output = loaded.root.join(previous_output);
        let still_active = active_outputs
            .get(key)
            .and_then(|output| output.as_deref())
            .is_some_and(|output| absolute_path(output) == absolute_path(&previous_output));
        if still_active || !previous_output.is_file() {
            continue;
        }
        let unchanged = entry.output_hash.as_deref().is_some_and(|hash| {
            fs::read(&previous_output).is_ok_and(|bytes| bytes_hash(&bytes) == hash)
        });
        if entry.output_owned && unchanged {
            if check {
                changed.push(path_slash(&previous_output));
            } else {
                fs::remove_file(&previous_output)?;
            }
        }
    }
    let stale = baseline
        .entries
        .keys()
        .filter(|key| !active_keys.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    for key in stale {
        let Some((_source, target_text)) = key.split_once('\0') else {
            if check {
                changed.push("adapter baseline entry".to_string());
            } else {
                baseline.entries.remove(&key);
            }
            continue;
        };
        if check {
            changed.push(format!("removed adapter {target_text}"));
            continue;
        }
        baseline.entries.remove(&key);
    }
    Ok(())
}

fn restore_file_snapshots(paths: &[PathBuf], originals: &[Option<Vec<u8>>]) -> Result<()> {
    let mut errors = Vec::new();
    for (path, original) in paths.iter().zip(originals).rev() {
        let result = if let Some(bytes) = original {
            atomic_write(path, bytes)
        } else if path.is_file() {
            fs::remove_file(path).map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("Adapter rollback was incomplete: {}", errors.join("; "))
    }
}

pub fn syncback_project_projection(
    loaded: &LoadedProject,
    projection_root: &Path,
    check: bool,
) -> Result<usize> {
    let mut planned_writes = BTreeMap::new();
    let mut planned_removals = BTreeSet::new();
    let changed = syncback_project_projection_into(
        loaded,
        projection_root,
        check,
        &mut planned_writes,
        &mut planned_removals,
    )?;
    if check && changed > 0 {
        bail!("{changed} projected source owner(s) are stale");
    }
    if !check && (!planned_writes.is_empty() || !planned_removals.is_empty()) {
        let removals = planned_removals
            .into_iter()
            .filter(|path| !planned_writes.contains_key(path))
            .collect::<Vec<_>>();
        crate::apply_file_mutations(&planned_writes, &removals)?;
    }
    Ok(changed)
}

fn syncback_project_projection_into(
    loaded: &LoadedProject,
    projection_root: &Path,
    check: bool,
    planned_writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    planned_removals: &mut BTreeSet<PathBuf>,
) -> Result<usize> {
    validate_project(loaded)?;
    let baseline = stage_project(loaded)?;
    if !baseline.is_temporary() && absolute_path(baseline.root()) == absolute_path(projection_root)
    {
        return Ok(0);
    }
    let owners = reverse_owners(loaded)?;
    let naming = project_script_naming(&loaded.project);
    let mut changed = 0usize;
    let mut documents = HashMap::new();
    let mut baseline_documents = HashMap::new();
    for entry in fs::read_dir(projection_root)
        .with_context(|| format!("Failed to read {}", projection_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let service = entry.file_name().to_string_lossy().to_string();
        let settings = crate::existing_service_settings_path(&entry.path());
        if !settings.is_file() {
            continue;
        }
        documents.insert(service.clone(), SettingsBytecode::read_file(&settings)?);
        let baseline_settings =
            crate::existing_service_settings_path(&baseline.root().join(&service));
        if baseline_settings.is_file() {
            baseline_documents.insert(service, SettingsBytecode::read_file(&baseline_settings)?);
        }
    }
    canonicalize_projection_document_map(&mut documents)?;
    canonicalize_projection_document_map(&mut baseline_documents)?;
    validate_projection_field_ownership(loaded, &documents, &baseline_documents)?;
    let projected_sources_by_service = documents
        .iter()
        .map(|(service, document)| {
            Ok((
                service.clone(),
                projection_sources(projection_root, document)?,
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let baseline_sources_by_service = baseline_documents
        .iter()
        .map(|(service, document)| {
            Ok((
                service.clone(),
                projection_sources(baseline.root(), document)?,
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let mut overlay_provenance = HashMap::new();
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::Overlay)
    {
        overlay_provenance.insert(
            reverse_owner_key(owner),
            overlay_owner_provenance(loaded, owner)?,
        );
    }
    for owner in &owners {
        if owner.ownership != MountOwnership::ReadOnly {
            continue;
        }
        let imported_document = documents
            .get(&owner.target[0])
            .with_context(|| format!("Missing projected service {}", owner.target[0]))?;
        if owner.optional
            && with_target_parts(&owner.target, &owner.ordinals, |target| {
                find_document_target(imported_document, target).map(drop)
            })
            .is_err()
        {
            continue;
        }
        let imported = with_target_parts(&owner.target, &owner.ordinals, |target| {
            projection_owner_snapshot(projection_root, imported_document, target)
        })?;
        let original = with_target_parts(&owner.target, &owner.ordinals, |target| {
            projection_owner_snapshot(
                baseline.root(),
                baseline_documents
                    .get(&owner.target[0])
                    .with_context(|| format!("Missing baseline service {}", owner.target[0]))?,
                target,
            )
        })?;
        if imported != original {
            bail!(
                "Studio changed read-only mount '{}'; change its source or make the mount writable",
                owner.target.join(".")
            );
        }
    }

    let mut transformed_targets = baseline.transform_targets().cloned().collect::<Vec<_>>();
    transformed_targets.sort();
    transformed_targets.dedup();
    for target in &transformed_targets {
        let imported_document = documents
            .get(&target[0])
            .with_context(|| format!("Missing projected service {}", target[0]))?;
        let baseline_document = baseline_documents
            .get(&target[0])
            .with_context(|| format!("Missing baseline service {}", target[0]))?;
        let unchanged = projection_owner_snapshot(projection_root, imported_document, target)
            .and_then(|imported| {
                projection_owner_snapshot(baseline.root(), baseline_document, target)
                    .map(|original| imported == original)
            })
            .unwrap_or(false);
        if !unchanged {
            bail!(
                "Studio changed sync-rule output '{}'; edit its source file instead",
                target.join(".")
            );
        }
    }

    let mut external_targets = owners
        .iter()
        .map(|owner| ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone()))
        .collect::<Vec<_>>();
    external_targets.extend(
        transformed_targets
            .iter()
            .cloned()
            .map(|target| ProjectTarget::from_parts(target, Vec::new())),
    );
    for adapter in &loaded.project.adapters {
        if adapter.direction != AdapterDirection::FromProject {
            external_targets.push(adapter.target.clone());
        }
    }

    for (service, document) in &documents {
        let target = vec![service.clone()];
        let target_selector = ProjectTarget::from_parts(target.clone(), Vec::new());
        if external_targets
            .iter()
            .any(|owner| targets_are_equal(owner, &target_selector))
        {
            continue;
        }
        let baseline_document = baseline_documents.get(service);
        if loaded
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
            && baseline_document.is_none()
        {
            continue;
        }
        let allowed = if loaded
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
        {
            baseline_document
                .map(|baseline| projection_identity_set(baseline, &target))
                .transpose()?
        } else {
            None
        };
        let mut output =
            extract_projection_document(document, &target, &external_targets, allowed.as_ref())?;
        apply_reverse_filters(
            loaded,
            &target,
            &mut output,
            baseline_document
                .map(|baseline| {
                    extract_projection_document(baseline, &target, &external_targets, None)
                })
                .transpose()?
                .as_ref(),
        )?;
        let destination = loaded.root.join(&loaded.project.source_root).join(service);
        restore_project_owned_fields(loaded, &target, &destination, &mut output)?;
        let plan = plan_reverse_owner(
            &destination,
            &output,
            &projected_sources_by_service[service],
            &naming,
        )?;
        if reverse_owner_plan_differs(&plan)? {
            changed += 1;
            if !check {
                for (path, bytes) in plan.writes {
                    if let Some(previous) = planned_writes.insert(path.clone(), bytes.clone())
                        && previous != bytes
                    {
                        bail!(
                            "Reverse projection planned conflicting writes to {}",
                            path.display()
                        );
                    }
                    planned_removals.remove(&path);
                }
                planned_removals.extend(plan.removals);
            }
        }
    }

    for owner in owners {
        if owner.ownership == MountOwnership::ReadOnly {
            continue;
        }
        let document = documents
            .get(&owner.target[0])
            .with_context(|| format!("Missing projected service {}", owner.target[0]))?;
        if owner.optional
            && with_target_parts(&owner.target, &owner.ordinals, |target| {
                find_document_target(document, target).map(drop)
            })
            .is_err()
        {
            continue;
        }
        let baseline_document = baseline_documents.get(&owner.target[0]);
        let allowed = if owner.ownership == MountOwnership::Overlay {
            Some(
                overlay_provenance
                    .get(&reverse_owner_key(&owner))
                    .context("Overlay ownership provenance disappeared")?
                    .identities
                    .clone(),
            )
        } else if owner.ignore_unknown_instances {
            baseline_document
                .map(|baseline| {
                    with_target_parts(&owner.target, &owner.ordinals, |target| {
                        projection_identity_set(baseline, target)
                    })
                })
                .transpose()?
        } else {
            None
        };
        let nested_exclusions = external_targets
            .iter()
            .filter(|target| {
                target.segments().len() > owner.target.len()
                    && target_is_within(
                        target,
                        &ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone()),
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut output = with_target_parts(&owner.target, &owner.ordinals, |target| {
            extract_projection_document(document, target, &nested_exclusions, allowed.as_ref())
        })?;
        let baseline_output = baseline_document
            .map(|baseline| {
                with_target_parts(&owner.target, &owner.ordinals, |target| {
                    extract_projection_document(
                        baseline,
                        target,
                        &nested_exclusions,
                        allowed.as_ref(),
                    )
                })
            })
            .transpose()?;
        apply_reverse_filters(loaded, &owner.target, &mut output, baseline_output.as_ref())?;
        if owner.ownership == MountOwnership::Overlay {
            output.instances[0] = overlay_provenance
                .get(&reverse_owner_key(&owner))
                .context("Overlay ownership provenance disappeared")?
                .root
                .clone();
            output.instances[0].parent_index = None;
        }
        restore_project_owned_fields(loaded, &owner.target, &owner.source, &mut output)?;
        if is_nested_project_path(&owner.source) {
            let baseline_output = baseline_output
                .as_ref()
                .context("Nested project mount has no baseline projection")?;
            changed += syncback_nested_owner(
                &owner.source,
                &output,
                baseline_output,
                &projected_sources_by_service[&owner.target[0]],
                baseline_sources_by_service
                    .get(&owner.target[0])
                    .context("Nested project mount has no baseline source map")?,
                check,
                &mut NestedSyncMutations {
                    writes: planned_writes,
                    removals: planned_removals,
                },
            )?;
            continue;
        }
        let plan = plan_reverse_owner(
            &owner.source,
            &output,
            &projected_sources_by_service[&owner.target[0]],
            &naming,
        )?;
        if reverse_owner_plan_differs(&plan)? {
            changed += 1;
            if !check {
                for (path, bytes) in plan.writes {
                    if let Some(previous) = planned_writes.insert(path.clone(), bytes.clone())
                        && previous != bytes
                    {
                        bail!(
                            "Reverse projection planned conflicting writes to {}",
                            path.display()
                        );
                    }
                    planned_removals.remove(&path);
                }
                planned_removals.extend(plan.removals);
            }
        }
    }
    Ok(changed)
}

fn is_nested_project_path(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".project.json") || name.ends_with(".project.jsonc")
        })
}

struct NestedSyncMutations<'a> {
    writes: &'a mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &'a mut BTreeSet<PathBuf>,
}

fn syncback_nested_owner(
    path: &Path,
    document: &SettingsBytecode,
    baseline_document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
    baseline_sources: &HashMap<String, ReverseSource>,
    check: bool,
    mutations: &mut NestedSyncMutations<'_>,
) -> Result<usize> {
    let nested = load_nested_project(path)?;
    validate_nested_project(&nested)?;
    let root_name = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .context("Nested project mount has no root instance")?;
    let baseline_root_name = baseline_document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.as_str())
        .context("Nested project baseline has no root instance")?;
    if baseline_root_name != root_name {
        bail!(
            "Nested project root changed from '{}' to '{}'; rename the outer mount instead",
            baseline_root_name,
            root_name
        );
    }
    let root_target = vec![root_name.clone()];
    let root_selector = ProjectTarget::from_parts(root_target.clone(), Vec::new());
    let owners = reverse_owners(&nested)?;
    let naming = project_script_naming(&nested.project);
    let filtered = prefixed_nested_filter_project(&nested, &root_target);
    let mut external_targets = owners
        .iter()
        .map(|owner| {
            ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
                .with_prefix(&root_target)
        })
        .collect::<Vec<_>>();
    for adapter in &nested.project.adapters {
        if adapter.direction != AdapterDirection::FromProject {
            external_targets.push(adapter.target.with_prefix(&root_target));
        }
    }
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::ReadOnly)
    {
        let target = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
            .with_prefix(&root_target);
        let current = with_project_target(&target, |target| {
            extract_projection_document(document, target, &[], None)
        })?;
        let baseline = with_project_target(&target, |target| {
            extract_projection_document(baseline_document, target, &[], None)
        })?;
        if projection_document_snapshot(&current, sources)?
            != projection_document_snapshot(&baseline, baseline_sources)?
        {
            bail!(
                "Studio changed read-only nested mount '{}'; change its source or make the mount writable",
                owner.target.join(".")
            );
        }
    }
    let allowed = if nested
        .project
        .root
        .ignore_unknown_instances
        .unwrap_or(false)
    {
        Some(projection_identity_set(baseline_document, &root_target)?)
    } else {
        None
    };
    let mut output = with_project_target(&root_selector, |target| {
        extract_projection_document(document, target, &external_targets, allowed.as_ref())
    })?;
    let baseline_output = with_project_target(&root_selector, |target| {
        extract_projection_document(
            baseline_document,
            target,
            &external_targets,
            allowed.as_ref(),
        )
    })?;
    apply_reverse_filters(&filtered, &root_target, &mut output, Some(&baseline_output))?;
    let destination = nested.root.join(&nested.project.source_root);
    restore_project_owned_fields(&nested, &[], &destination, &mut output)?;
    let mut changed = usize::from(merge_nested_reverse_plan(
        plan_reverse_owner(&destination, &output, sources, &naming)?,
        check,
        &mut *mutations.writes,
        &mut *mutations.removals,
    )?);
    let mut overlay_provenance = HashMap::new();
    for owner in owners
        .iter()
        .filter(|owner| owner.ownership == MountOwnership::Overlay)
    {
        let target = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
            .with_prefix(&root_target);
        overlay_provenance.insert(
            reverse_owner_key(owner),
            overlay_owner_provenance_at(&nested, owner, &target)?,
        );
    }
    for owner in owners {
        if owner.ownership == MountOwnership::ReadOnly {
            continue;
        }
        let target = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone())
            .with_prefix(&root_target);
        if owner.optional
            && with_project_target(&target, |target| {
                find_document_target(document, target).map(drop)
            })
            .is_err()
        {
            continue;
        }
        let allowed = if owner.ownership == MountOwnership::Overlay {
            Some(
                overlay_provenance
                    .get(&reverse_owner_key(&owner))
                    .context("Nested overlay ownership provenance disappeared")?
                    .identities
                    .clone(),
            )
        } else if owner.ignore_unknown_instances {
            Some(with_project_target(&target, |target| {
                projection_identity_set(baseline_document, target)
            })?)
        } else {
            None
        };
        let exclusions = external_targets
            .iter()
            .filter(|candidate| {
                candidate.segments().len() > target.segments().len()
                    && target_is_within(candidate, &target)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut owner_output = with_project_target(&target, |target| {
            extract_projection_document(document, target, &exclusions, allowed.as_ref())
        })?;
        let owner_baseline = with_project_target(&target, |target| {
            extract_projection_document(baseline_document, target, &exclusions, allowed.as_ref())
        })?;
        apply_reverse_filters(
            &filtered,
            &target.segments(),
            &mut owner_output,
            Some(&owner_baseline),
        )?;
        if owner.ownership == MountOwnership::Overlay {
            owner_output.instances[0] = overlay_provenance
                .get(&reverse_owner_key(&owner))
                .context("Nested overlay ownership provenance disappeared")?
                .root
                .clone();
            owner_output.instances[0].parent_index = None;
        }
        restore_project_owned_fields(&nested, &owner.target, &owner.source, &mut owner_output)?;
        if is_nested_project_path(&owner.source) {
            changed += syncback_nested_owner(
                &owner.source,
                &owner_output,
                &owner_baseline,
                sources,
                baseline_sources,
                check,
                mutations,
            )?;
        } else {
            changed += usize::from(merge_nested_reverse_plan(
                plan_reverse_owner(&owner.source, &owner_output, sources, &naming)?,
                check,
                &mut *mutations.writes,
                &mut *mutations.removals,
            )?);
        }
    }
    if nested
        .project
        .adapters
        .iter()
        .any(|adapter| adapter.direction != AdapterDirection::ToProject)
    {
        let root =
            fresh_projection_stage(&nested.root.join(".renium").join("nested-syncback"), "")?;
        let adapter_result: Result<usize> = (|| {
            let children = crate::settings_children_by_parent(document);
            let root_index = document
                .instances
                .iter()
                .position(|instance| instance.parent_index.is_none())
                .context("Nested project mount has no root instance")?;
            for child_index in children[root_index].iter().copied() {
                let child = extract_projection_subtree(document, child_index)?;
                let child_destination = root.join(&document.instances[child_index].name);
                fs::create_dir_all(&child_destination)?;
                child.write_file(&crate::writable_service_settings_path(&child_destination)?)?;
                for (source_path, source) in
                    reverse_script_plan(&child_destination, &child, sources, &naming)?
                {
                    if let Some(parent) = source_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    atomic_write(&source_path, source.as_bytes())?;
                }
            }
            let adapter_plan = plan_adapter_syncback(&nested, &root)?;
            let adapter_changed =
                adapter_plan.writes.len() + usize::from(adapter_plan.baseline_changed);
            if !check {
                for (path, bytes) in adapter_plan.writes {
                    merge_nested_write(
                        path,
                        bytes,
                        &mut *mutations.writes,
                        &mut *mutations.removals,
                    )?;
                }
                if adapter_plan.baseline_changed {
                    merge_nested_write(
                        adapter_plan.baseline_path,
                        adapter_plan.baseline_bytes,
                        &mut *mutations.writes,
                        &mut *mutations.removals,
                    )?;
                }
            }
            Ok(adapter_changed)
        })();
        let cleanup = fs::remove_dir_all(&root);
        remove_empty_stage_parents(&root);
        if let Err(error) = cleanup {
            eprintln!(
                "[renium] warning: failed to remove nested syncback stage {}: {error}",
                root.display()
            );
        }
        changed += adapter_result?;
    }
    Ok(changed)
}

fn prefixed_nested_filter_project(loaded: &LoadedProject, prefix: &[String]) -> LoadedProject {
    let mut filtered = LoadedProject {
        path: loaded.path.clone(),
        root: loaded.root.clone(),
        project: loaded.project.clone(),
    };
    let escaped_prefix = escape_glob(&filter_path_segments(prefix));
    filtered.project.filters = loaded
        .project
        .filters
        .iter()
        .flat_map(|rule| {
            if let Some(glob) = rule.glob.as_deref() {
                let mut nested = rule.clone();
                nested.glob = Some(format!("{escaped_prefix}/{}", glob.trim_start_matches('/')));
                vec![nested]
            } else {
                let mut root = rule.clone();
                root.glob = Some(escaped_prefix.clone());
                let mut descendants = rule.clone();
                descendants.glob = Some(format!("{escaped_prefix}/**"));
                vec![root, descendants]
            }
        })
        .collect();
    filtered
}

fn projection_document_snapshot(
    document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
) -> Result<Vec<u8>> {
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    serde_json::to_vec(&json!({
        "document": document,
        "sources": sources
            .iter()
            .filter(|(id, _)| ids.contains(id.as_str()))
            .map(|(id, source)| (id, (&source.extension, &source.text)))
            .collect::<BTreeMap<_, _>>(),
    }))
    .context("Failed to encode nested projection snapshot")
}

fn merge_nested_write(
    path: PathBuf,
    bytes: Vec<u8>,
    planned_writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    planned_removals: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if let Some(previous) = planned_writes.insert(path.clone(), bytes.clone())
        && previous != bytes
    {
        bail!(
            "Reverse projection planned conflicting writes to {}",
            path.display()
        );
    }
    planned_removals.remove(&path);
    Ok(())
}

fn merge_nested_reverse_plan(
    plan: ReverseOwnerPlan,
    check: bool,
    planned_writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    planned_removals: &mut BTreeSet<PathBuf>,
) -> Result<bool> {
    if !reverse_owner_plan_differs(&plan)? {
        return Ok(false);
    }
    if !check {
        for (path, bytes) in plan.writes {
            merge_nested_write(path, bytes, planned_writes, planned_removals)?;
        }
        planned_removals.extend(plan.removals);
    }
    Ok(true)
}

fn extract_projection_subtree(
    document: &SettingsBytecode,
    root: usize,
) -> Result<SettingsBytecode> {
    if root >= document.instances.len() {
        bail!("Projected subtree root is outside the settings document");
    }
    let children = crate::settings_children_by_parent(document);
    let mut selected = Vec::new();
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        selected.push(index);
        for child in children[index].iter().rev() {
            stack.push(*child);
        }
    }
    let index_map = selected
        .iter()
        .enumerate()
        .map(|(new, old)| (*old, new))
        .collect::<HashMap<_, _>>();
    let id_set = selected
        .iter()
        .map(|index| document.instances[*index].settings_id.clone())
        .collect::<HashSet<_>>();
    let mut instances = selected
        .iter()
        .map(|old| {
            let mut instance = document.instances[*old].clone();
            instance.parent_index = instance
                .parent_index
                .and_then(|parent| index_map.get(&parent).copied());
            remap_extracted_references(&mut instance.properties, &index_map, &id_set);
            remap_extracted_references(&mut instance.attributes, &index_map, &id_set);
            instance
        })
        .collect::<Vec<_>>();
    instances[0].parent_index = None;
    Ok(SettingsBytecode {
        version: document.version,
        instances,
    })
}

fn reverse_owners(loaded: &LoadedProject) -> Result<Vec<ReverseOwner>> {
    let mut owners = project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(target, node)| {
            node.path.map(|source| ReverseOwner {
                target,
                ordinals: Vec::new(),
                source: loaded.root.join(source),
                ownership: MountOwnership::Exclusive,
                ignore_unknown_instances: node.ignore_unknown_instances.unwrap_or(false),
                optional: false,
            })
        })
        .collect::<Vec<_>>();
    for mount in &loaded.project.mounts {
        let target = target_segments(&mount.target)?;
        let source = loaded.root.join(&mount.source);
        if mount.optional && !source.exists() {
            continue;
        }
        let ignore_unknown_instances = if is_nested_project_path(&source) && source.is_file() {
            load_nested_project(&source)?
                .project
                .root
                .ignore_unknown_instances
                .unwrap_or(false)
        } else {
            false
        };
        owners.push(ReverseOwner {
            target,
            ordinals: mount.target.ordinals(),
            source,
            ownership: mount.ownership,
            ignore_unknown_instances,
            optional: mount.optional,
        });
    }
    owners.sort_by_key(|owner| owner.target.len());
    Ok(owners)
}

fn projection_identity_set(
    document: &SettingsBytecode,
    target: &[String],
) -> Result<HashSet<String>> {
    let root = find_document_target(document, target)?;
    let paths = projection_instance_path_parts(document);
    let mut output = HashSet::new();
    let children = crate::settings_children_by_parent(document);
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        let instance = document.instances[index].clone();
        output.insert(instance.settings_id.clone());
        output.insert(projection_path_identity(
            &paths[index],
            &instance.class_name,
        ));
        stack.extend(children[index].iter().copied());
    }
    Ok(output)
}

struct OverlayOwnerProvenance {
    identities: HashSet<String>,
    root: SettingsBytecodeInstance,
}

fn overlay_owner_provenance(
    loaded: &LoadedProject,
    owner: &ReverseOwner,
) -> Result<OverlayOwnerProvenance> {
    let selector = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone());
    overlay_owner_provenance_at(loaded, owner, &selector)
}

fn overlay_owner_provenance_at(
    loaded: &LoadedProject,
    owner: &ReverseOwner,
    staged_target: &ProjectTarget,
) -> Result<OverlayOwnerProvenance> {
    let selector = ProjectTarget::from_parts(owner.target.clone(), owner.ordinals.clone());
    let mount = loaded
        .project
        .mounts
        .iter()
        .find(|mount| {
            absolute_path(&loaded.root.join(&mount.source)) == absolute_path(&owner.source)
                && targets_are_equal(&mount.target, &selector)
        })
        .context("Overlay reverse owner no longer matches a project mount")?;
    let root = fresh_projection_stage(&env::temp_dir().join("renium-overlay-owner"), "")?;
    PROJECTION_TRANSFORM_STACK.with(|stack| stack.borrow_mut().push(Vec::new()));
    PROJECTION_IDENTITY_STACK.with(|stack| stack.borrow_mut().push(HashMap::new()));
    let result = (|| {
        cache_script_naming(&root, &loaded.project);
        let mut staged_mount = mount.clone();
        staged_mount.target = staged_target.clone();
        stage_mount(loaded, &root, &staged_mount)?;
        refresh_stage_settings(&root)?;
        normalize_stage_references(&root)?;
        let target = staged_target.segments();
        let ordinals = staged_target.ordinals();
        let service = target.first().context("Overlay target has no service")?;
        let settings = crate::existing_service_settings_path(&root.join(service));
        let document = SettingsBytecode::read_file(&settings)?;
        with_target_parts(&target, &ordinals, |target| {
            let root_index = find_document_target(&document, target)?;
            let paths = projection_instance_path_parts(&document);
            let mut identities = projection_identity_set(&document, target)?;
            let root_instance = &document.instances[root_index];
            identities.remove(&root_instance.settings_id);
            identities.remove(&projection_path_identity(
                &paths[root_index],
                &root_instance.class_name,
            ));
            Ok(OverlayOwnerProvenance {
                identities,
                root: root_instance.clone(),
            })
        })
    })();
    PROJECTION_TRANSFORM_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
    PROJECTION_IDENTITY_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
    let cleanup = fs::remove_dir_all(&root);
    if let Err(error) = cleanup {
        eprintln!(
            "[renium] warning: failed to remove overlay stage {}: {error}",
            root.display()
        );
    }
    result
}

fn reverse_owner_key(owner: &ReverseOwner) -> String {
    serde_json::to_string(&(
        &owner.target,
        &owner.ordinals,
        path_slash(&absolute_path(&owner.source)),
    ))
    .unwrap_or_default()
}

fn projection_path_identity(path: &(Vec<String>, Vec<usize>), class_name: &str) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{class_name}",
        path.0.join("\0"),
        path.1
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn extract_projection_document(
    document: &SettingsBytecode,
    target: &[String],
    exclusions: &[ProjectTarget],
    allowed: Option<&HashSet<String>>,
) -> Result<SettingsBytecode> {
    let root = find_document_target(document, target)?;
    let target_selector =
        ProjectTarget::from_parts(target.to_vec(), active_target_ordinals(target));
    let excluded_roots = exclusions
        .iter()
        .filter(|path| target_is_within(path, &target_selector))
        .map(|path| {
            find_document_target_optional_with_ordinals(
                document,
                &path.segments(),
                &path.ordinals(),
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>();
    let paths = projection_instance_path_parts(document);
    let children = crate::settings_children_by_parent(document);
    let mut selected = Vec::new();
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        if index != root && excluded_roots.contains(&index) {
            continue;
        }
        let instance = &document.instances[index];
        let identity_allowed = allowed.is_none_or(|identities| {
            identities.contains(&instance.settings_id)
                || identities.contains(&projection_path_identity(
                    &paths[index],
                    &instance.class_name,
                ))
        });
        if index != root && !identity_allowed {
            if let Some(identities) = allowed {
                let mut descendants = children[index].clone();
                let mut visited = HashSet::new();
                while let Some(descendant) = descendants.pop() {
                    if !visited.insert(descendant) {
                        continue;
                    }
                    let descendant_instance = &document.instances[descendant];
                    if identities.contains(&descendant_instance.settings_id)
                        || identities.contains(&projection_path_identity(
                            &paths[descendant],
                            &descendant_instance.class_name,
                        ))
                    {
                        bail!(
                            "Owned instance '{}' was moved beneath unknown instance '{}'; move it back into its owned hierarchy before syncing",
                            descendant_instance.name,
                            instance.name
                        );
                    }
                    descendants.extend(children[descendant].iter().copied());
                }
            }
            continue;
        }
        selected.push(index);
        for child in children[index].iter().rev() {
            stack.push(*child);
        }
    }
    let index_map = selected
        .iter()
        .enumerate()
        .map(|(new, old)| (*old, new))
        .collect::<HashMap<_, _>>();
    let id_set = selected
        .iter()
        .map(|index| document.instances[*index].settings_id.clone())
        .collect::<HashSet<_>>();
    let mut instances = selected
        .iter()
        .map(|old| {
            let mut instance = document.instances[*old].clone();
            instance.parent_index = instance
                .parent_index
                .and_then(|parent| index_map.get(&parent).copied());
            remap_extracted_references(&mut instance.properties, &index_map, &id_set);
            remap_extracted_references(&mut instance.attributes, &index_map, &id_set);
            instance
        })
        .collect::<Vec<_>>();
    if let Some(root) = instances.first_mut() {
        root.parent_index = None;
    }
    Ok(SettingsBytecode {
        version: document.version,
        instances,
    })
}

fn remap_extracted_references(
    record: &mut Map<String, Value>,
    indices: &HashMap<usize, usize>,
    ids: &HashSet<String>,
) {
    fn visit(value: &mut Value, indices: &HashMap<usize, usize>, ids: &HashSet<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, indices, ids);
                }
            }
            Value::Object(object) => {
                let reference_id = object
                    .get("settingsId")
                    .or_else(|| object.get("instanceId"))
                    .and_then(Value::as_str);
                if reference_id.is_some_and(|id| !ids.contains(id)) {
                    object.remove("instanceIndex");
                } else if let Some(old) = object
                    .get("instanceIndex")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| index.checked_sub(1))
                {
                    if let Some(new) = indices.get(&old) {
                        object.insert("instanceIndex".to_string(), json!(new + 1));
                    } else {
                        object.remove("instanceIndex");
                    }
                }
                for value in object.values_mut() {
                    visit(value, indices, ids);
                }
            }
            _ => {}
        }
    }
    for value in record.values_mut() {
        visit(value, indices, ids);
    }
}

fn projection_instance_paths(document: &SettingsBytecode) -> Vec<Vec<String>> {
    projection_instance_path_parts(document)
        .into_iter()
        .map(|(segments, _)| segments)
        .collect()
}

fn projection_instance_path_parts(document: &SettingsBytecode) -> Vec<(Vec<String>, Vec<usize>)> {
    projection_instance_path_parts_from_instances(&document.instances)
}

fn projection_instance_path_parts_from_instances(
    instances: &[SettingsBytecodeInstance],
) -> Vec<(Vec<String>, Vec<usize>)> {
    let mut occurrence_by_index = vec![1; instances.len()];
    let mut occurrences = HashMap::<(Option<usize>, String), usize>::new();
    for (index, instance) in instances.iter().enumerate() {
        let occurrence = occurrences
            .entry((instance.parent_index, instance.name.clone()))
            .or_insert(0);
        *occurrence += 1;
        occurrence_by_index[index] = *occurrence;
    }
    let mut paths = vec![(Vec::new(), Vec::new()); instances.len()];
    for (index, (segments, ordinals)) in paths.iter_mut().enumerate() {
        let mut path = Vec::new();
        let mut path_ordinals = Vec::new();
        let mut current = Some(index);
        let mut seen = HashSet::new();
        while let Some(value) = current {
            if value >= instances.len() || !seen.insert(value) {
                break;
            }
            path.push(instances[value].name.clone());
            path_ordinals.push(occurrence_by_index[value]);
            current = instances[value].parent_index;
        }
        path.reverse();
        path_ordinals.reverse();
        *segments = path;
        *ordinals = path_ordinals;
    }
    paths
}

type BaselineFilterEntry = (SettingsBytecodeInstance, Option<String>, String);

fn projection_filter_path(target: &[String], path: &[String]) -> String {
    let segments = if path.starts_with(target) {
        path.to_vec()
    } else {
        target
            .iter()
            .chain(path.iter().skip(1))
            .cloned()
            .collect::<Vec<_>>()
    };
    filter_path_segments(&segments)
}

fn restore_filtered_baseline_deletions(
    rules: &[FilterRule],
    document: &mut SettingsBytecode,
    baseline: Option<&SettingsBytecode>,
    baseline_by_id: &HashMap<String, BaselineFilterEntry>,
    parent_ids: &mut Vec<Option<String>>,
    allowed: &mut Vec<bool>,
) -> Result<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    let present = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut restore = HashSet::new();
    for instance in &baseline.instances {
        if present.contains(&instance.settings_id) {
            continue;
        }
        let Some((baseline_instance, _, path)) = baseline_by_id.get(&instance.settings_id) else {
            continue;
        };
        let candidate = owned_filter_candidate(baseline_instance, path.clone());
        if !filter_allows_scope(
            rules,
            FilterDirection::StudioToFiles,
            &candidate.borrowed(),
            FilterScope::Instance,
        )? {
            restore.insert(instance.settings_id.clone());
        }
    }
    let mut anchors = HashSet::new();
    let mut pending = restore.iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        let Some((_, parent, _)) = baseline_by_id.get(&id) else {
            continue;
        };
        let Some(parent) = parent else {
            continue;
        };
        if present.contains(parent) || restore.contains(parent) || !anchors.insert(parent.clone()) {
            continue;
        }
        pending.push(parent.clone());
    }
    for instance in &baseline.instances {
        let id = &instance.settings_id;
        if !restore.contains(id) && !anchors.contains(id) {
            continue;
        }
        let Some((original, parent_id, _)) = baseline_by_id.get(id) else {
            continue;
        };
        let mut output = original.clone();
        let is_anchor = anchors.contains(id) && !restore.contains(id);
        if is_anchor {
            output.properties.clear();
            output.attributes.clear();
        }
        document.instances.push(output);
        parent_ids.push(parent_id.clone());
        allowed.push(is_anchor);
    }
    Ok(())
}

fn value_references_any_id(value: &Value, ids: &HashSet<String>) -> bool {
    match value {
        Value::Array(values) => values
            .iter()
            .any(|value| value_references_any_id(value, ids)),
        Value::Object(object) => {
            let direct = object
                .get("settingsId")
                .or_else(|| object.get("instanceId"))
                .and_then(Value::as_str)
                .is_some_and(|id| ids.contains(id));
            direct
                || object
                    .values()
                    .any(|value| value_references_any_id(value, ids))
        }
        _ => false,
    }
}

fn apply_reverse_filters(
    loaded: &LoadedProject,
    target: &[String],
    document: &mut SettingsBytecode,
    baseline: Option<&SettingsBytecode>,
) -> Result<()> {
    if loaded.project.filters.is_empty() {
        return Ok(());
    }
    let baseline_by_id = baseline
        .map(|baseline| {
            let paths = projection_instance_paths(baseline);
            baseline
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| {
                    let mut instance = instance.clone();
                    stabilize_reference_indices(&mut instance.properties, &baseline.instances);
                    stabilize_reference_indices(&mut instance.attributes, &baseline.instances);
                    let parent_id = instance
                        .parent_index
                        .and_then(|parent| baseline.instances.get(parent))
                        .map(|parent| parent.settings_id.clone());
                    let path = projection_filter_path(target, &paths[index]);
                    (instance.settings_id.clone(), (instance, parent_id, path))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut parent_ids = document
        .instances
        .iter()
        .map(|instance| {
            instance
                .parent_index
                .and_then(|parent| document.instances.get(parent))
                .map(|parent| parent.settings_id.clone())
        })
        .collect::<Vec<_>>();
    let current_instances = document.instances.clone();
    for instance in &mut document.instances {
        stabilize_reference_indices(&mut instance.properties, &current_instances);
        stabilize_reference_indices(&mut instance.attributes, &current_instances);
    }
    let paths = projection_instance_paths(document);
    let mut allowed = vec![true; document.instances.len()];
    for index in 0..document.instances.len() {
        let instance = document.instances[index].clone();
        let candidate =
            owned_filter_candidate(&instance, projection_filter_path(target, &paths[index]));
        let baseline_candidate = baseline_by_id
            .get(&instance.settings_id)
            .map(|(baseline, _, path)| owned_filter_candidate(baseline, path.clone()));
        allowed[index] = filter_allows_candidate_pair(
            &loaded.project.filters,
            FilterDirection::StudioToFiles,
            &candidate,
            baseline_candidate.as_ref(),
            FilterScope::Instance,
        )?;
        if allowed[index] {
            let baseline_instance = baseline_by_id
                .get(&instance.settings_id)
                .map(|(instance, _, _)| instance);
            let property_names = instance
                .properties
                .keys()
                .chain(
                    baseline_instance
                        .into_iter()
                        .flat_map(|instance| instance.properties.keys()),
                )
                .cloned()
                .collect::<BTreeSet<_>>();
            for property in property_names {
                if !filter_allows_candidate_pair(
                    &loaded.project.filters,
                    FilterDirection::StudioToFiles,
                    &candidate,
                    baseline_candidate.as_ref(),
                    FilterScope::Property(&property),
                )? {
                    if let Some(value) =
                        baseline_instance.and_then(|instance| instance.properties.get(&property))
                    {
                        document.instances[index]
                            .properties
                            .insert(property, value.clone());
                    } else {
                        document.instances[index].properties.remove(&property);
                    }
                }
            }
            let attribute_names = instance
                .attributes
                .keys()
                .chain(
                    baseline_instance
                        .into_iter()
                        .flat_map(|instance| instance.attributes.keys()),
                )
                .cloned()
                .collect::<BTreeSet<_>>();
            for attribute in attribute_names {
                if !filter_allows_candidate_pair(
                    &loaded.project.filters,
                    FilterDirection::StudioToFiles,
                    &candidate,
                    baseline_candidate.as_ref(),
                    FilterScope::Attribute(&attribute),
                )? {
                    if let Some(value) =
                        baseline_instance.and_then(|instance| instance.attributes.get(&attribute))
                    {
                        document.instances[index]
                            .attributes
                            .insert(attribute, value.clone());
                    } else {
                        document.instances[index].attributes.remove(&attribute);
                    }
                }
            }
        }
    }
    restore_filtered_baseline_deletions(
        &loaded.project.filters,
        document,
        baseline,
        &baseline_by_id,
        &mut parent_ids,
        &mut allowed,
    )?;
    let mut keep = allowed.clone();
    let indices_by_id = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for index in 0..document.instances.len() {
        if !keep[index] {
            continue;
        }
        let mut parent_id = parent_ids[index].as_deref();
        let mut seen = HashSet::new();
        while let Some(id) = parent_id {
            let Some(parent) = indices_by_id.get(id).copied() else {
                break;
            };
            if !seen.insert(parent) {
                break;
            }
            keep[parent] = true;
            parent_id = parent_ids[parent].as_deref();
        }
    }
    let mut remove = HashSet::new();
    for index in 0..document.instances.len() {
        if allowed[index] {
            continue;
        }
        let instance = &document.instances[index];
        if let Some((original, parent_id, _)) = baseline_by_id.get(&instance.settings_id) {
            document.instances[index] = original.clone();
            parent_ids[index] = parent_id.clone();
        } else if keep[index] {
            document.instances[index].properties.clear();
            document.instances[index].attributes.clear();
        } else {
            remove.insert(index);
        }
    }
    let removed_ids = remove
        .iter()
        .map(|index| document.instances[*index].settings_id.clone())
        .collect::<HashSet<_>>();
    if !removed_ids.is_empty() {
        for index in 0..document.instances.len() {
            if remove.contains(&index) {
                continue;
            }
            let settings_id = document.instances[index].settings_id.clone();
            let baseline_instance = baseline_by_id
                .get(&settings_id)
                .map(|(instance, _, _)| instance);
            let property_names = document.instances[index]
                .properties
                .iter()
                .filter_map(|(name, value)| {
                    value_references_any_id(value, &removed_ids).then_some(name.clone())
                })
                .collect::<Vec<_>>();
            for name in property_names {
                let value = baseline_instance
                    .and_then(|instance| instance.properties.get(&name))
                    .filter(|value| !value_references_any_id(value, &removed_ids))
                    .cloned()
                    .with_context(|| {
                        format!(
                            "Cannot filter Studio-only reference from instance '{}' property '{}'; no safe baseline value exists",
                            settings_id, name
                        )
                    })?;
                document.instances[index].properties.insert(name, value);
            }
            let attribute_names = document.instances[index]
                .attributes
                .iter()
                .filter_map(|(name, value)| {
                    value_references_any_id(value, &removed_ids).then_some(name.clone())
                })
                .collect::<Vec<_>>();
            for name in attribute_names {
                let value = baseline_instance
                    .and_then(|instance| instance.attributes.get(&name))
                    .filter(|value| !value_references_any_id(value, &removed_ids))
                    .cloned()
                    .with_context(|| {
                        format!(
                            "Cannot filter Studio-only reference from instance '{}' attribute '{}'; no safe baseline value exists",
                            settings_id, name
                        )
                    })?;
                document.instances[index].attributes.insert(name, value);
            }
        }
    }
    let kept = (0..document.instances.len())
        .filter(|index| !remove.contains(index))
        .collect::<Vec<_>>();
    let mut instances = kept
        .iter()
        .map(|old| document.instances[*old].clone())
        .collect::<Vec<_>>();
    let kept_parent_ids = kept
        .iter()
        .map(|old| parent_ids[*old].clone())
        .collect::<Vec<_>>();
    let mut indices_by_id = HashMap::new();
    for (index, instance) in instances.iter().enumerate() {
        if indices_by_id
            .insert(instance.settings_id.clone(), index)
            .is_some()
        {
            bail!(
                "Filtered projection contains duplicate settings id '{}'",
                instance.settings_id
            );
        }
    }
    for (index, instance) in instances.iter_mut().enumerate() {
        instance.parent_index = match kept_parent_ids[index].as_deref() {
            Some(parent_id) => Some(*indices_by_id.get(parent_id).with_context(|| {
                format!(
                    "Filtered projection cannot restore parent '{}' for '{}'",
                    parent_id, instance.settings_id
                )
            })?),
            None => None,
        };
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    document.instances = instances;
    Ok(())
}

fn stabilize_reference_indices(
    record: &mut Map<String, Value>,
    instances: &[SettingsBytecodeInstance],
) {
    fn visit(
        value: &mut Value,
        instances: &[SettingsBytecodeInstance],
        paths: &[(Vec<String>, Vec<usize>)],
    ) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, instances, paths);
                }
            }
            Value::Object(object) => {
                let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId")
                    || object.contains_key("instanceIndex");
                if is_reference {
                    if object.get("settingsId").and_then(Value::as_str).is_none()
                        && object.get("instanceId").and_then(Value::as_str).is_none()
                        && let Some(index) = object
                            .get("instanceIndex")
                            .and_then(Value::as_u64)
                            .and_then(|index| usize::try_from(index).ok())
                            .and_then(|index| index.checked_sub(1))
                        && let Some(instance) = instances.get(index)
                    {
                        let (path_segments, path_ordinals) = &paths[index];
                        object.insert(
                            "settingsId".to_string(),
                            Value::String(instance.settings_id.clone()),
                        );
                        object.insert(
                            "pathSegments".to_string(),
                            Value::Array(
                                path_segments.iter().cloned().map(Value::String).collect(),
                            ),
                        );
                        object.insert(
                            "pathOrdinals".to_string(),
                            Value::Array(path_ordinals.iter().map(|value| json!(value)).collect()),
                        );
                    }
                    object.remove("instanceIndex");
                }
                for value in object.values_mut() {
                    visit(value, instances, paths);
                }
            }
            _ => {}
        }
    }
    let paths = projection_instance_path_parts_from_instances(instances);
    for value in record.values_mut() {
        visit(value, instances, &paths);
    }
}

fn reindex_reference_indices(record: &mut Map<String, Value>, indices: &HashMap<String, usize>) {
    fn visit(value: &mut Value, indices: &HashMap<String, usize>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    visit(value, indices);
                }
            }
            Value::Object(object) => {
                let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId")
                    || object.contains_key("instanceIndex");
                if is_reference {
                    let target = object
                        .get("settingsId")
                        .or_else(|| object.get("instanceId"))
                        .and_then(Value::as_str)
                        .and_then(|id| indices.get(id))
                        .copied();
                    if let Some(index) = target {
                        object.insert("instanceIndex".to_string(), json!(index + 1));
                    } else {
                        object.remove("instanceIndex");
                    }
                }
                for value in object.values_mut() {
                    visit(value, indices);
                }
            }
            _ => {}
        }
    }
    for value in record.values_mut() {
        visit(value, indices);
    }
}

fn projection_owner_snapshot(
    root: &Path,
    document: &SettingsBytecode,
    target: &[String],
) -> Result<Vec<u8>> {
    let extracted = extract_projection_document(document, target, &[], None)?;
    let sources = projection_sources(root, document)?;
    let mut value = serde_json::to_value(&extracted)?;
    let source_value = extracted
        .instances
        .iter()
        .filter_map(|instance| {
            sources.get(&instance.settings_id).map(|source| {
                (
                    instance.settings_id.clone(),
                    json!([source.extension, source.text]),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    value
        .as_object_mut()
        .context("Settings snapshot must be an object")?
        .insert("sources".to_string(), serde_json::to_value(source_value)?);
    serde_json::to_vec(&value).context("Failed to encode projected owner snapshot")
}

fn projection_sources(
    root: &Path,
    document: &SettingsBytecode,
) -> Result<HashMap<String, ReverseSource>> {
    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.as_str())
        .context("Projected settings have no root")?;
    let service_dir = root.join(service);
    let paths = crate::build_editor_source_paths_by_index(document, service, &service_dir);
    let mut output = HashMap::new();
    for (index, path) in paths.into_iter().enumerate() {
        let Some(path) = path else {
            continue;
        };
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read projected source {}", path.display()))?;
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("luau")
            .to_string();
        output.insert(
            document.instances[index].settings_id.clone(),
            ReverseSource { text, extension },
        );
    }
    Ok(output)
}

fn restore_project_owned_fields(
    loaded: &LoadedProject,
    owner_target: &[String],
    destination: &Path,
    output: &mut SettingsBytecode,
) -> Result<()> {
    let mut canonical = if destination.is_dir() {
        let settings = crate::existing_service_settings_path(destination);
        settings
            .is_file()
            .then(|| SettingsBytecode::read_file(&settings))
            .transpose()?
    } else {
        match destination
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("renium" | "rbsync") if destination.is_file() => {
                Some(SettingsBytecode::read_file(destination)?)
            }
            Some("rbxm" | "rbxmx") if destination.is_file() => {
                Some(crate::read_settings_model_document(destination)?)
            }
            _ => None,
        }
    };
    if let Some(canonical) = canonical.as_mut() {
        let canonical_instances = canonical.instances.clone();
        for instance in &mut canonical.instances {
            stabilize_reference_indices(&mut instance.properties, &canonical_instances);
            stabilize_reference_indices(&mut instance.attributes, &canonical_instances);
        }
    }
    let output_root_name = output
        .instances
        .first()
        .map(|instance| instance.name.clone())
        .context("Projected owner has no root instance")?;
    let mut id_remap = HashMap::new();
    for owner in projection_field_owners_with_root(loaded, owner_target.is_empty())? {
        if !owner.target.starts_with(owner_target) {
            continue;
        }
        let mut local_target = vec![output_root_name.clone()];
        local_target.extend(owner.target.iter().skip(owner_target.len()).cloned());
        let Some(output_index) = find_document_target_optional(output, &local_target)? else {
            continue;
        };
        let canonical_index = canonical
            .as_ref()
            .and_then(|canonical| {
                canonical.instances.iter().position(|instance| {
                    instance.settings_id == output.instances[output_index].settings_id
                })
            })
            .or(canonical
                .as_ref()
                .map(|canonical| find_document_target_optional(canonical, &local_target))
                .transpose()?
                .flatten());
        let canonical_instance = canonical_index.and_then(|index| {
            canonical
                .as_ref()
                .and_then(|canonical| canonical.instances.get(index))
        });
        let projected_id = output.instances[output_index].settings_id.clone();
        if owner.settings_id
            && let Some(canonical_instance) = canonical_instance
            && projected_id != canonical_instance.settings_id
        {
            id_remap.insert(projected_id, canonical_instance.settings_id.clone());
            output.instances[output_index].settings_id = canonical_instance.settings_id.clone();
        }
        if owner.class_name
            && let Some(canonical_instance) = canonical_instance
        {
            output.instances[output_index].class_name = canonical_instance.class_name.clone();
        }
        for property in &owner.properties {
            if let Some(value) =
                canonical_instance.and_then(|instance| instance.properties.get(property))
            {
                output.instances[output_index]
                    .properties
                    .insert(property.clone(), value.clone());
            } else {
                output.instances[output_index].properties.remove(property);
            }
        }
        for attribute in &owner.attributes {
            if let Some(value) =
                canonical_instance.and_then(|instance| instance.attributes.get(attribute))
            {
                output.instances[output_index]
                    .attributes
                    .insert(attribute.clone(), value.clone());
            } else {
                output.instances[output_index].attributes.remove(attribute);
            }
        }
        if owner.tags {
            if let Some(value) =
                canonical_instance.and_then(|instance| instance.properties.get("Tags"))
            {
                output.instances[output_index]
                    .properties
                    .insert("Tags".to_string(), value.clone());
            } else {
                output.instances[output_index].properties.remove("Tags");
            }
        }
    }
    if !id_remap.is_empty() {
        for instance in &mut output.instances {
            remap_settings_references(&mut instance.properties, &id_remap);
            remap_settings_references(&mut instance.attributes, &id_remap);
        }
    }
    let indices_by_id = output
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for instance in &mut output.instances {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    Ok(())
}

struct ReverseOwnerPlan {
    writes: BTreeMap<PathBuf, Vec<u8>>,
    removals: BTreeSet<PathBuf>,
}

fn plan_reverse_owner(
    destination: &Path,
    document: &SettingsBytecode,
    projected_sources: &HashMap<String, ReverseSource>,
    naming: &ProjectScriptNaming,
) -> Result<ReverseOwnerPlan> {
    let mut writes = BTreeMap::new();
    let mut removals = BTreeSet::new();
    if destination
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            matches!(extension.to_ascii_lowercase().as_str(), "rbxm" | "rbxmx")
        })
    {
        let bytes = reverse_model_bytes(destination, document, projected_sources)?;
        writes.insert(destination.to_path_buf(), bytes);
        return Ok(ReverseOwnerPlan { writes, removals });
    }
    if destination.is_file()
        || destination
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| matches!(extension, "lua" | "luau" | "renium" | "rbsync"))
    {
        if matches!(
            destination.extension().and_then(OsStr::to_str),
            Some("renium" | "rbsync")
        ) {
            writes.insert(
                destination.to_path_buf(),
                crate::settings_document_bytes(document, destination)?,
            );
            return Ok(ReverseOwnerPlan { writes, removals });
        }
        let expected = document
            .instances
            .first()
            .and_then(|instance| projected_sources.get(&instance.settings_id))
            .map(|source| source.text.as_bytes().to_vec())
            .context("Projected script owner has no source")?;
        writes.insert(destination.to_path_buf(), expected);
        return Ok(ReverseOwnerPlan { writes, removals });
    }
    let settings = crate::writable_service_settings_path(destination)?;
    writes.insert(
        settings.clone(),
        crate::settings_document_bytes(document, &settings)?,
    );
    for (path, source) in reverse_script_plan(destination, document, projected_sources, naming)? {
        writes.insert(path, source.into_bytes());
    }
    if destination.is_dir() {
        for entry in walkdir::WalkDir::new(destination) {
            let entry =
                entry.with_context(|| format!("Failed to scan {}", destination.display()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if (crate::infer_source_script(&name, naming).is_some()
                || crate::is_service_settings_file_name(&name))
                && !writes.contains_key(entry.path())
            {
                removals.insert(entry.path().to_path_buf());
            }
        }
    }
    Ok(ReverseOwnerPlan { writes, removals })
}

fn reverse_owner_plan_differs(plan: &ReverseOwnerPlan) -> Result<bool> {
    for (path, bytes) in &plan.writes {
        match fs::read(path) {
            Ok(current) if current == *bytes => {}
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()));
            }
        }
    }
    Ok(plan.removals.iter().any(|path| path.is_file()))
}

fn reverse_model_bytes(
    destination: &Path,
    document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
) -> Result<Vec<u8>> {
    let mut model = document.clone();
    restore_reverse_model_topology(destination, &mut model)?;
    for instance in &mut model.instances {
        if let Some(source) = sources.get(&instance.settings_id) {
            instance
                .properties
                .insert("Source".to_string(), Value::String(source.text.clone()));
        }
    }
    crate::validate_settings_model_internal_references(&model, &destination.to_string_lossy())?;
    let binary = destination
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rbxm"));
    crate::encode_settings_model(&model, binary)
}

fn restore_reverse_model_topology(destination: &Path, model: &mut SettingsBytecode) -> Result<()> {
    let canonical = crate::read_settings_model_document(destination)?;
    let canonical_roots = canonical
        .instances
        .iter()
        .filter(|instance| instance.parent_index.is_none())
        .collect::<Vec<_>>();
    if canonical_roots.len() == 1 {
        let canonical_root = canonical_roots[0];
        let root = model
            .instances
            .iter_mut()
            .find(|instance| instance.settings_id == canonical_root.settings_id)
            .context("Projected model no longer contains its canonical root")?;
        if root.parent_index.is_some() {
            bail!("Projected model root moved beneath another instance");
        }
        root.name = canonical_root.name.clone();
        return Ok(());
    }
    if canonical_roots.len() < 2 {
        bail!("Canonical model has no root instances");
    }
    let wrapper = model
        .instances
        .iter()
        .position(|instance| instance.parent_index.is_none())
        .context("Projected multi-root model has no synthetic root")?;
    let canonical_ids = canonical_roots
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    for canonical_root in &canonical_roots {
        let index = model
            .instances
            .iter()
            .position(|instance| instance.settings_id == canonical_root.settings_id)
            .with_context(|| {
                format!(
                    "Projected multi-root model no longer contains root {}",
                    canonical_root.name
                )
            })?;
        if model.instances[index].parent_index != Some(wrapper) {
            bail!(
                "Projected multi-root model root '{}' moved outside its synthetic container",
                canonical_root.name
            );
        }
    }
    if canonical_ids.contains(model.instances[wrapper].settings_id.as_str()) {
        bail!("Projected multi-root model is missing its synthetic container");
    }
    model.instances.remove(wrapper);
    for instance in &mut model.instances {
        instance.parent_index = match instance.parent_index {
            Some(parent) if parent == wrapper => None,
            Some(parent) if parent > wrapper => Some(parent - 1),
            parent => parent,
        };
    }
    let indices_by_id = model
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for instance in &mut model.instances {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    Ok(())
}

fn reverse_script_plan(
    root: &Path,
    document: &SettingsBytecode,
    sources: &HashMap<String, ReverseSource>,
    naming: &ProjectScriptNaming,
) -> Result<Vec<(PathBuf, String)>> {
    let children = crate::settings_children_by_parent(document);
    let roots = document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| instance.parent_index.is_none().then_some(index))
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut context = ReverseScriptPlanContext {
        document,
        children: &children,
        sources,
        naming,
        output: &mut output,
    };
    let mut used_root_names = HashSet::new();
    let mut next_root_suffix = HashMap::new();
    for index in &roots {
        let directory = if roots.len() == 1 {
            root.to_path_buf()
        } else {
            root.join(crate::unique_child_stem(
                &document.instances[*index].name,
                &mut used_root_names,
                &mut next_root_suffix,
            ))
        };
        plan_reverse_script_node(&mut context, *index, &directory, true, None)?;
    }
    Ok(output)
}

struct ReverseScriptPlanContext<'a> {
    document: &'a SettingsBytecode,
    children: &'a [Vec<usize>],
    sources: &'a HashMap<String, ReverseSource>,
    naming: &'a ProjectScriptNaming,
    output: &'a mut Vec<(PathBuf, String)>,
}

fn plan_reverse_script_node(
    context: &mut ReverseScriptPlanContext<'_>,
    index: usize,
    parent: &Path,
    is_root: bool,
    stem: Option<&str>,
) -> Result<()> {
    let instance = &context.document.instances[index];
    let has_children = !context.children[index].is_empty();
    let source = context.sources.get(&instance.settings_id);
    let child_root = if let Some(source) = source {
        let suffix = reverse_script_suffix(instance, context.naming, &source.extension)?;
        let path = if is_root {
            parent.join(format!("init{suffix}"))
        } else if has_children {
            let directory = parent.join(stem.unwrap_or(&instance.name));
            directory.join(format!("init{suffix}"))
        } else {
            parent.join(format!("{}{suffix}", stem.unwrap_or(&instance.name)))
        };
        context.output.push((path.clone(), source.text.clone()));
        if has_children {
            path.parent().unwrap_or(parent).to_path_buf()
        } else {
            parent.to_path_buf()
        }
    } else if is_root {
        parent.to_path_buf()
    } else {
        parent.join(stem.unwrap_or(&instance.name))
    };
    let mut used_names = HashSet::new();
    let mut next_suffix = HashMap::new();
    let child_count = context.children[index].len();
    for child_position in 0..child_count {
        let child = context.children[index][child_position];
        let child_stem = crate::unique_child_stem(
            &context.document.instances[child].name,
            &mut used_names,
            &mut next_suffix,
        );
        plan_reverse_script_node(context, child, &child_root, false, Some(&child_stem))?;
    }
    Ok(())
}

fn reverse_script_suffix(
    instance: &SettingsBytecodeInstance,
    naming: &ProjectScriptNaming,
    extension: &str,
) -> Result<String> {
    let run_context = instance
        .properties
        .get("RunContext")
        .and_then(crate::run_context_name);
    let suffix = if instance.class_name == "Script"
        && run_context.is_some_and(|value| value.eq_ignore_ascii_case("Client"))
    {
        &naming.client_run_context_suffix
    } else if instance.class_name == "Script"
        && run_context.is_some_and(|value| value.eq_ignore_ascii_case("Plugin"))
    {
        &naming.plugin_suffix
    } else {
        match instance.class_name.as_str() {
            "Script" => &naming.server_suffix,
            "LocalScript" => &naming.client_suffix,
            "ModuleScript" => &naming.module_suffix,
            class_name => bail!("{class_name} is not a script class"),
        }
    };
    Ok(format!("{suffix}.{extension}"))
}

pub fn syncback_project_adapters(loaded: &LoadedProject, check: bool) -> Result<usize> {
    let projection = stage_adapter_syncback_projection(loaded)?;
    syncback_project_adapters_from_root(loaded, projection.root(), check)
}

fn stage_adapter_syncback_projection(loaded: &LoadedProject) -> Result<ProjectionStage> {
    let mut project = loaded.project.clone();
    project.adapters.clear();
    stage_project(&LoadedProject {
        path: loaded.path.clone(),
        root: loaded.root.clone(),
        project,
    })
}

pub fn syncback_project_adapters_from_root(
    loaded: &LoadedProject,
    source_root: &Path,
    check: bool,
) -> Result<usize> {
    let plan = plan_adapter_syncback(loaded, source_root)?;
    let changed = plan.writes.len() + usize::from(plan.baseline_changed);
    if check && changed > 0 {
        let mut changed_paths = plan
            .writes
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect::<Vec<_>>();
        if plan.baseline_changed {
            changed_paths.push(plan.baseline_path.display().to_string());
        }
        bail!(
            "Adapter source files are stale: {}",
            changed_paths.join(", ")
        );
    }
    if !check {
        let mut writes = plan.writes;
        if plan.baseline_changed {
            writes.push((plan.baseline_path, plan.baseline_bytes));
        }
        if !writes.is_empty() {
            write_file_transaction(&writes)?;
        }
    }
    Ok(changed)
}

struct AdapterSyncbackPlan {
    writes: Vec<(PathBuf, Vec<u8>)>,
    baseline_path: PathBuf,
    baseline_bytes: Vec<u8>,
    baseline_changed: bool,
}

fn plan_adapter_syncback(
    loaded: &LoadedProject,
    source_root: &Path,
) -> Result<AdapterSyncbackPlan> {
    validate_project(loaded)?;
    let mut writes = Vec::new();
    let baseline_path = loaded.root.join(".renium").join("adapter-baseline.json");
    let mut baseline = if baseline_path.is_file() {
        serde_json::from_slice::<AdapterBaseline>(&fs::read(&baseline_path)?)
            .with_context(|| format!("Invalid adapter baseline {}", baseline_path.display()))?
    } else {
        AdapterBaseline::default()
    };
    migrate_adapter_baseline_entries(&loaded.project, &mut baseline);
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::ToProject {
            continue;
        }
        with_project_target(&adapter.target, |target| {
            let format = adapter_format(adapter)?;
            let service = target
                .first()
                .context("Adapter target must include a service")?;
            let settings_path = crate::existing_service_settings_path(&source_root.join(service));
            if !settings_path.is_file() {
                bail!(
                    "Cannot sync back adapter {} because {} does not exist",
                    adapter.target,
                    settings_path.display()
                );
            }
            let source = loaded.root.join(&adapter.source);
            let document = SettingsBytecode::read_file(&settings_path)?;
            let current = match fs::read(&source) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to read {}", source.display()));
                }
            };
            let key = adapter_key(adapter);
            let previous = baseline.entries.get(&key).cloned();
            let model_json_hierarchical = if format == "model-json" {
                Some(
                    current
                        .as_deref()
                        .and_then(|bytes| model_json_bytes_are_hierarchical(bytes).ok())
                        .or_else(|| {
                            previous
                                .as_ref()
                                .and_then(|entry| entry.model_json_hierarchical)
                        })
                        .unwrap_or(true),
                )
            } else {
                None
            };
            let bytes = reversible_adapter_target_bytes(
                adapter,
                &format,
                &document,
                target,
                &source,
                model_json_hierarchical,
            )?;
            let source_hash = current.as_deref().map(bytes_hash);
            let target_hash = bytes_hash(&bytes);
            let mut write_target = adapter.direction == AdapterDirection::FromProject;
            let mut update_baseline = write_target;
            if adapter.direction == AdapterDirection::TwoWay {
                if let Some(previous) = previous.as_ref() {
                    let source_changed = source_hash
                        .as_ref()
                        .is_none_or(|hash| *hash != previous.source_hash);
                    let target_changed = target_hash != previous.target_hash;
                    let values_match = source_hash.as_ref() == Some(&target_hash);
                    if current.is_none() {
                        if target_changed {
                            bail!(
                                "Two-way adapter conflict for '{}': {} was removed and {} changed since the last successful sync",
                                adapter.target,
                                source.display(),
                                settings_path.display()
                            );
                        }
                        write_target = true;
                        update_baseline = true;
                    } else {
                        match (source_changed, target_changed, values_match) {
                            (true, false, _) => {}
                            (false, true, _) => {
                                write_target = true;
                                update_baseline = true;
                            }
                            (true, true, false) => {
                                bail!(
                                    "Two-way adapter conflict for '{}': both {} and {} changed since the last successful sync",
                                    adapter.target,
                                    source.display(),
                                    settings_path.display()
                                );
                            }
                            (_, _, true) => update_baseline = true,
                            (false, false, false) => {}
                        }
                    }
                } else {
                    write_target = true;
                    update_baseline = true;
                }
            }
            if write_target && current.as_deref() != Some(bytes.as_slice()) {
                writes.push((source, bytes.clone()));
            }
            if update_baseline {
                baseline.entries.insert(
                    key,
                    AdapterBaselineEntry {
                        source_hash: if write_target {
                            target_hash.clone()
                        } else {
                            source_hash
                                .clone()
                                .context("Adapter source disappeared during syncback")?
                        },
                        target_hash,
                        format: Some(format.clone()),
                        output: previous.as_ref().and_then(|entry| entry.output.clone()),
                        output_hash: previous
                            .as_ref()
                            .and_then(|entry| entry.output_hash.clone()),
                        output_owned: previous.as_ref().is_some_and(|entry| entry.output_owned),
                        model_json_hierarchical,
                    },
                );
            }
            Ok(())
        })?;
    }
    let baseline_bytes = serde_json::to_vec_pretty(&baseline)?;
    let baseline_changed =
        fs::read(&baseline_path).ok().as_deref() != Some(baseline_bytes.as_slice());
    Ok(AdapterSyncbackPlan {
        writes,
        baseline_path,
        baseline_bytes,
        baseline_changed,
    })
}

fn reversible_adapter_target_bytes(
    adapter: &AdapterSpec,
    format: &str,
    document: &SettingsBytecode,
    target: &[String],
    source: &Path,
    model_json_hierarchical: Option<bool>,
) -> Result<Vec<u8>> {
    match format {
        "txt" => {
            let index = find_document_target(document, target)?;
            let instance = &document.instances[index];
            if instance.class_name != "StringValue" {
                bail!(
                    "Adapter {} targets {}, expected StringValue",
                    adapter.source.display(),
                    instance.class_name
                );
            }
            Ok(instance
                .properties
                .get("Value")
                .and_then(Value::as_str)
                .context("StringValue adapter target is missing a string Value")?
                .as_bytes()
                .to_vec())
        }
        "csv" => {
            let index = find_document_target(document, target)?;
            let instance = &document.instances[index];
            if instance.class_name != "LocalizationTable" {
                bail!(
                    "Adapter {} targets {}, expected LocalizationTable",
                    adapter.source.display(),
                    instance.class_name
                );
            }
            let contents = instance
                .properties
                .get("Contents")
                .and_then(Value::as_str)
                .context("LocalizationTable adapter target is missing string Contents")?;
            localization_json_to_csv(contents)
        }
        "model-json" => {
            let hierarchical = model_json_hierarchical
                .or_else(|| model_json_source_is_hierarchical(source).ok())
                .unwrap_or(true);
            export_model_json(document, target, hierarchical)
        }
        _ => unreachable!("validation rejects non-reversible adapter formats"),
    }
}

fn bytes_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_file_transaction(writes: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    let originals = writes
        .iter()
        .map(|(path, _)| {
            if path.is_file() {
                fs::read(path).map(Some)
            } else {
                Ok(None)
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    for (index, (path, bytes)) in writes.iter().enumerate() {
        if let Err(error) = atomic_write(path, bytes) {
            let mut rollback_errors = Vec::new();
            for rollback in (0..index).rev() {
                let rollback_path = &writes[rollback].0;
                let result = if let Some(original) = &originals[rollback] {
                    atomic_write(rollback_path, original)
                } else if rollback_path.is_file() {
                    fs::remove_file(rollback_path).map_err(anyhow::Error::from)
                } else {
                    Ok(())
                };
                if let Err(rollback_error) = result {
                    rollback_errors.push(format!("{}: {rollback_error}", rollback_path.display()));
                }
            }
            if rollback_errors.is_empty() {
                return Err(error);
            }
            return Err(error).context(format!(
                "Adapter rollback was incomplete: {}",
                rollback_errors.join("; ")
            ));
        }
    }
    Ok(())
}

fn model_json_source_is_hierarchical(source: &Path) -> Result<bool> {
    model_json_bytes_are_hierarchical(&fs::read(source)?)
}

fn model_json_bytes_are_hierarchical(bytes: &[u8]) -> Result<bool> {
    let text = std::str::from_utf8(bytes).context("Model JSON is not UTF-8")?;
    let value = parse_jsonc_value(text)?;
    let object = value
        .as_object()
        .context("Model JSON root must be an object")?;
    Ok(!object.get("instances").is_some_and(Value::is_array))
}

fn export_model_json(
    document: &SettingsBytecode,
    target: &[String],
    hierarchical: bool,
) -> Result<Vec<u8>> {
    let target_index = find_document_target(document, target)?;
    let children = crate::settings_children_by_parent(document);
    if hierarchical {
        let included = projection_subtree_indices(&children, target_index)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let root = export_hierarchical_model_json_node(
            document,
            &children,
            target_index,
            &included,
            false,
        );
        return Ok((serde_json::to_string_pretty(&root)? + "\n").into_bytes());
    }
    let mut indices = Vec::new();
    let mut stack = children
        .get(target_index)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    while let Some(index) = stack.pop() {
        indices.push(index);
        if let Some(child_indices) = children.get(index) {
            stack.extend(child_indices.iter().rev().copied());
        }
    }
    let included = indices.iter().copied().collect::<BTreeSet<_>>();
    let instances = indices
        .into_iter()
        .map(|index| {
            let instance = &document.instances[index];
            let mut properties = instance.properties.clone();
            let mut attributes = instance.attributes.clone();
            stabilize_reference_indices(&mut properties, &document.instances);
            stabilize_reference_indices(&mut attributes, &document.instances);
            let tags = properties
                .remove("Tags")
                .unwrap_or(Value::Array(Vec::new()));
            json!({
                "id": instance.settings_id,
                "name": instance.name,
                "className": instance.class_name,
                "parentId": instance.parent_index
                    .filter(|parent| included.contains(parent))
                    .map(|parent| document.instances[parent].settings_id.clone()),
                "properties": properties,
                "attributes": attributes,
                "tags": tags,
            })
        })
        .collect::<Vec<_>>();
    Ok((serde_json::to_string_pretty(&json!({
        "schemaVersion": 1,
        "instances": instances,
    }))? + "\n")
        .into_bytes())
}

fn projection_subtree_indices(children: &[Vec<usize>], root: usize) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        indices.push(index);
        if let Some(child_indices) = children.get(index) {
            stack.extend(child_indices.iter().rev().copied());
        }
    }
    indices
}

fn export_hierarchical_model_json_node(
    document: &SettingsBytecode,
    children: &[Vec<usize>],
    index: usize,
    included: &BTreeSet<usize>,
    include_name: bool,
) -> Value {
    let instance = &document.instances[index];
    let mut properties = instance.properties.clone();
    let mut attributes = instance.attributes.clone();
    stabilize_reference_indices(&mut properties, &document.instances);
    stabilize_reference_indices(&mut attributes, &document.instances);
    let tags = properties
        .remove("Tags")
        .unwrap_or(Value::Array(Vec::new()));
    let child_values = children
        .get(index)
        .into_iter()
        .flatten()
        .map(|child| {
            export_hierarchical_model_json_node(document, children, *child, included, true)
        })
        .collect::<Vec<_>>();
    let mut output = Map::from_iter([
        (
            "id".to_string(),
            Value::String(instance.settings_id.clone()),
        ),
        (
            "className".to_string(),
            Value::String(instance.class_name.to_string()),
        ),
        ("properties".to_string(), Value::Object(properties)),
        ("attributes".to_string(), Value::Object(attributes)),
        ("tags".to_string(), tags),
        ("children".to_string(), Value::Array(child_values)),
    ]);
    if include_name {
        output.insert("name".to_string(), Value::String(instance.name.clone()));
    }
    if let Some(parent) = instance
        .parent_index
        .filter(|parent| included.contains(parent))
    {
        output.insert(
            "parentId".to_string(),
            Value::String(document.instances[parent].settings_id.clone()),
        );
    }
    Value::Object(output)
}

fn watch_adapters(loaded: &LoadedProject, interval_ms: u64) -> Result<()> {
    let project_path = loaded.path.clone();
    let mut current = load_project(Some(&project_path), None)?;
    validate_project(&current)?;
    build_adapters(&current, false, true)?;
    let mut announced = false;
    loop {
        let inputs = adapter_watch_inputs(&current)?;
        if !announced {
            println!("Watching {} adapter inputs", inputs.len());
            announced = true;
        }
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })?;
        let mut watched = BTreeSet::new();
        for input in &inputs {
            let (path, mode) = if input.is_dir() {
                (input.clone(), RecursiveMode::Recursive)
            } else {
                (
                    input
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| current.root.clone()),
                    RecursiveMode::NonRecursive,
                )
            };
            let key = (
                absolute_path(&path),
                matches!(mode, RecursiveMode::Recursive),
            );
            if watched.insert(key) {
                watcher.watch(&path, mode)?;
            }
        }
        let debounce = Duration::from_millis(interval_ms.clamp(25, 60_000));
        let mut relevant = false;
        loop {
            let event = receiver.recv()??;
            relevant |= event.paths.iter().any(|path| {
                let path = absolute_path(path);
                inputs.iter().any(|input| {
                    let input = absolute_path(input);
                    path == input || input.is_dir() && path.starts_with(&input)
                })
            });
            if !relevant {
                continue;
            }
            while let Ok(event) = receiver.recv_timeout(debounce) {
                event?;
            }
            break;
        }
        match load_project(Some(&project_path), None).and_then(|next| {
            validate_project(&next)?;
            build_adapters(&next, false, true)?;
            Ok(next)
        }) {
            Ok(next) => current = next,
            Err(error) => eprintln!("Adapter build failed: {error:#}"),
        }
    }
}

fn adapter_watch_inputs(loaded: &LoadedProject) -> Result<BTreeSet<PathBuf>> {
    let mut inputs = BTreeSet::from([loaded.path.clone()]);
    for adapter in &loaded.project.adapters {
        if adapter.direction == AdapterDirection::FromProject {
            continue;
        }
        let source = loaded.root.join(&adapter.source);
        inputs.insert(source.clone());
        if adapter_format(adapter)? == "nested-project" && source.is_file() {
            let nested = load_nested_project(&source)?;
            inputs.insert(nested.path.clone());
            inputs.extend(project_source_roots(&nested)?);
        }
    }
    Ok(inputs)
}

fn render_adapter(source: &Path, format: &str) -> Result<Vec<u8>> {
    let source_bytes =
        fs::read(source).with_context(|| format!("Failed to read {}", source.display()))?;
    let value = match format {
        "txt" => {
            let text = String::from_utf8(source_bytes)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            return Ok(format!("return {}\n", luau_string(&text)).into_bytes());
        }
        "markdown" => {
            let text = String::from_utf8(source_bytes)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            let rich_text = markdown_to_roblox_rich_text(&text);
            return Ok(format!("return {}\n", luau_string(&rich_text)).into_bytes());
        }
        "csv" => {
            let text = String::from_utf8(source_bytes)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            csv_to_value(&text)?
        }
        "json" => serde_json::from_slice(&source_bytes)
            .with_context(|| format!("Invalid JSON in {}", source.display()))?,
        "jsonc" | "model-json" => {
            let text = String::from_utf8(source_bytes)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            parse_jsonc_value(&text)
                .with_context(|| format!("Invalid JSONC in {}", source.display()))?
        }
        "toml" => {
            let text = String::from_utf8(source_bytes)
                .with_context(|| format!("{} is not UTF-8", source.display()))?;
            let parsed: toml::Value = toml::from_str(&text)
                .with_context(|| format!("Invalid TOML in {}", source.display()))?;
            serde_json::to_value(parsed)?
        }
        "yaml" => serde_yaml::from_slice(&source_bytes)
            .with_context(|| format!("Invalid YAML in {}", source.display()))?,
        "msgpack" => rmp_serde::from_slice(&source_bytes)
            .with_context(|| format!("Invalid MessagePack in {}", source.display()))?,
        other => bail!("Unsupported adapter format '{other}'"),
    };
    if format == "model-json" {
        return Ok((serde_json::to_string_pretty(&value)? + "\n").into_bytes());
    }
    let rendered = value_to_luau(&value, 0)?;
    if value_contains_null(&value) {
        return Ok(format!("local null = table.freeze({{}})\nreturn {rendered}\n").into_bytes());
    }
    Ok(format!("return {rendered}\n").into_bytes())
}

fn markdown_to_roblox_rich_text(markdown: &str) -> String {
    fn escape(output: &mut String, text: &str) {
        for character in text.chars() {
            match character {
                '&' => output.push_str("&amp;"),
                '<' => output.push_str("&lt;"),
                '>' => output.push_str("&gt;"),
                '"' => output.push_str("&quot;"),
                '\'' => output.push_str("&apos;"),
                _ => output.push(character),
            }
        }
    }

    fn block_break(output: &mut String, lines: usize) {
        let existing = output
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count();
        for _ in existing..lines {
            output.push('\n');
        }
    }

    fn heading_size(level: HeadingLevel) -> u8 {
        match level {
            HeadingLevel::H1 => 28,
            HeadingLevel::H2 => 24,
            HeadingLevel::H3 => 21,
            HeadingLevel::H4 => 18,
            HeadingLevel::H5 => 16,
            HeadingLevel::H6 => 14,
        }
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let mut output = String::new();
    let mut links = Vec::new();
    let mut lists = Vec::<Option<u64>>::new();
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::Paragraph) => block_break(&mut output, 1),
            Event::End(TagEnd::Paragraph) => block_break(&mut output, 2),
            Event::Start(Tag::Heading { level, .. }) => {
                block_break(&mut output, 1);
                output.push_str(&format!("<font size=\"{}\"><b>", heading_size(level)));
            }
            Event::End(TagEnd::Heading(_)) => {
                output.push_str("</b></font>");
                block_break(&mut output, 2);
            }
            Event::Start(Tag::Emphasis) => output.push_str("<i>"),
            Event::End(TagEnd::Emphasis) => output.push_str("</i>"),
            Event::Start(Tag::Strong) => output.push_str("<b>"),
            Event::End(TagEnd::Strong) => output.push_str("</b>"),
            Event::Start(Tag::Strikethrough) => output.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => output.push_str("</s>"),
            Event::Start(Tag::CodeBlock(kind)) => {
                block_break(&mut output, 1);
                output.push_str("<font face=\"RobotoMono\">");
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    output.push('[');
                    escape(&mut output, &language);
                    output.push_str("]\n");
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                output.push_str("</font>");
                block_break(&mut output, 2);
            }
            Event::Start(Tag::BlockQuote(_)) => {
                block_break(&mut output, 1);
                output.push_str("&gt; ");
            }
            Event::End(TagEnd::BlockQuote(_)) => block_break(&mut output, 2),
            Event::Start(Tag::List(start)) => {
                block_break(&mut output, 1);
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                block_break(&mut output, 1);
            }
            Event::Start(Tag::Item) => {
                block_break(&mut output, 1);
                for _ in 1..lists.len() {
                    output.push_str("  ");
                }
                if let Some(Some(next)) = lists.last_mut() {
                    output.push_str(&format!("{next}. "));
                    *next += 1;
                } else {
                    output.push_str("• ");
                }
            }
            Event::End(TagEnd::Item) => block_break(&mut output, 1),
            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. }) => {
                links.push(dest_url.into_string());
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                if let Some(url) = links.pop() {
                    output.push_str(" (");
                    escape(&mut output, &url);
                    output.push(')');
                }
            }
            Event::Start(Tag::Table(_))
            | Event::Start(Tag::TableHead)
            | Event::Start(Tag::TableRow) => block_break(&mut output, 1),
            Event::End(TagEnd::Table)
            | Event::End(TagEnd::TableHead)
            | Event::End(TagEnd::TableRow) => block_break(&mut output, 1),
            Event::Start(Tag::TableCell) if !output.ends_with('\n') && !output.is_empty() => {
                output.push_str(" | ");
            }
            Event::Start(Tag::TableCell) => {}
            Event::End(TagEnd::TableCell) => {}
            Event::Text(text) => escape(&mut output, &text),
            Event::Code(text) => {
                output.push_str("<font face=\"RobotoMono\">");
                escape(&mut output, &text);
                output.push_str("</font>");
            }
            Event::Html(html) | Event::InlineHtml(html) => escape(&mut output, &html),
            Event::SoftBreak => output.push('\n'),
            Event::HardBreak => output.push_str("<br />"),
            Event::Rule => {
                block_break(&mut output, 1);
                output.push_str("────────");
                block_break(&mut output, 2);
            }
            Event::FootnoteReference(label) => {
                output.push('[');
                escape(&mut output, &label);
                output.push(']');
            }
            Event::TaskListMarker(checked) => {
                output.push_str(if checked { "☑ " } else { "☐ " });
            }
            Event::InlineMath(value) | Event::DisplayMath(value) => escape(&mut output, &value),
            _ => {}
        }
    }
    output.trim_end_matches('\n').to_string()
}

fn validate_adapter_source(source: &Path, format: &str) -> Result<()> {
    if !source.is_file() {
        bail!("Adapter input does not exist: {}", source.display());
    }
    if format == "rbxm" || format == "rbxmx" {
        let bytes = fs::read(source)?;
        if format == "rbxm" && !bytes.starts_with(b"<roblox") {
            bail!("{} is not a recognized Roblox model", source.display());
        }
        if format == "rbxmx" && !bytes.starts_with(b"<roblox") {
            bail!("{} is not a Roblox XML model", source.display());
        }
    }
    if format == "nested-project" {
        let _ = load_nested_project(source)?;
    }
    Ok(())
}

fn adapter_format(adapter: &AdapterSpec) -> Result<String> {
    let format = adapter
        .format
        .as_deref()
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            adapter
                .source
                .file_name()
                .and_then(OsStr::to_str)
                .map(|name| {
                    if name.ends_with(".project.json") || name.ends_with(".project.jsonc") {
                        "nested-project".to_string()
                    } else if name.ends_with(".model.json")
                        || name.ends_with(".model.jsonc")
                        || name.ends_with(".model.renium.jsonc")
                    {
                        "model-json".to_string()
                    } else {
                        adapter
                            .source
                            .extension()
                            .and_then(OsStr::to_str)
                            .unwrap_or("")
                            .to_ascii_lowercase()
                    }
                })
        })
        .unwrap_or_default();
    let normalized = match format.as_str() {
        "md" => "markdown",
        "yml" => "yaml",
        "mpk" | "mpack" => "msgpack",
        other => other,
    }
    .to_string();
    if !matches!(
        normalized.as_str(),
        "txt"
            | "csv"
            | "json"
            | "jsonc"
            | "toml"
            | "yaml"
            | "msgpack"
            | "markdown"
            | "model-json"
            | "rbxm"
            | "rbxmx"
            | "nested-project"
    ) {
        bail!(
            "Could not infer a supported format for {}; set format explicitly",
            adapter.source.display()
        );
    }
    Ok(normalized)
}

fn adapter_output_path(
    loaded: &LoadedProject,
    adapter: &AdapterSpec,
    format: &str,
) -> Result<Option<PathBuf>> {
    if let Some(output) = adapter.output.as_deref() {
        return Ok(Some(loaded.root.join(output)));
    }
    if !matches!(
        format,
        "json" | "jsonc" | "toml" | "yaml" | "msgpack" | "markdown"
    ) {
        return Ok(None);
    }
    let target = target_segments(&adapter.target)?;
    let extension = match loaded.project.script_extension {
        ScriptExtensionPolicy::Lua => "lua",
        ScriptExtensionPolicy::Preserve | ScriptExtensionPolicy::Luau => "luau",
    };
    let leaf = target
        .last()
        .context("Adapter target must include an instance name")?;
    let parent = target[..target.len().saturating_sub(1)].iter().fold(
        loaded.root.join(&loaded.project.source_root),
        |path, segment| path.join(segment),
    );
    Ok(Some(parent.join(format!(
        "{}{}.{}",
        leaf, loaded.project.export_naming.module_suffix, extension
    ))))
}

fn compare_or_write(
    path: &Path,
    bytes: &[u8],
    check: bool,
    changed: &mut Vec<String>,
) -> Result<()> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    changed.push(path.display().to_string());
    if !check {
        atomic_write(path, bytes)?;
    }
    Ok(())
}

fn value_to_luau(value: &Value, depth: usize) -> Result<String> {
    if depth > 128 {
        bail!("Adapter data is nested too deeply");
    }
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => {
            const MAX_EXACT_INTEGER: u64 = 9_007_199_254_740_992;
            if value
                .as_u64()
                .is_some_and(|integer| integer > MAX_EXACT_INTEGER)
                || value
                    .as_i64()
                    .is_some_and(|integer| integer.unsigned_abs() > MAX_EXACT_INTEGER)
            {
                bail!("Adapter integer {value} cannot be represented exactly by Luau");
            }
            Ok(value.to_string())
        }
        Value::String(value) => Ok(luau_string(value)),
        Value::Array(values) => {
            let items = values
                .iter()
                .map(|value| value_to_luau(value, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("{{{}}}", items.join(", ")))
        }
        Value::Object(values) => {
            let items = values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "[{}] = {}",
                        luau_string(key),
                        value_to_luau(value, depth + 1)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("{{{}}}", items.join(", ")))
        }
    }
}

fn value_contains_null(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(values) => values.iter().any(value_contains_null),
        Value::Object(values) => values.values().any(value_contains_null),
        _ => false,
    }
}

fn luau_string(value: &str) -> String {
    let mut equals = String::new();
    while value.contains(&format!("]{}]", equals)) {
        equals.push('=');
    }
    format!("[{equals}[{value}]{equals}]")
}

fn csv_to_value(text: &str) -> Result<Value> {
    let rows = parse_csv(text)?;
    let Some(headers) = rows.first() else {
        return Ok(Value::Array(Vec::new()));
    };
    let mut output = Vec::new();
    for (row_index, row) in rows.iter().enumerate().skip(1) {
        if row.len() != headers.len() {
            bail!(
                "CSV row {} has {} columns; expected {}",
                row_index + 1,
                row.len(),
                headers.len()
            );
        }
        let object = headers
            .iter()
            .cloned()
            .zip(row.iter().cloned().map(Value::String))
            .collect::<Map<_, _>>();
        output.push(Value::Object(object));
    }
    Ok(Value::Array(output))
}

fn localization_csv_to_json(text: &str) -> Result<String> {
    let rows = parse_csv(text)?;
    let Some(headers) = rows.first() else {
        return Ok("[]".to_string());
    };
    let mut entries = Vec::new();
    for row in rows.iter().skip(1) {
        let mut entry = Map::new();
        let mut values = Map::new();
        for (index, header) in headers.iter().enumerate() {
            let value = row.get(index).map(String::as_str).unwrap_or("");
            if header.is_empty() || value.is_empty() {
                continue;
            }
            match header.as_str() {
                "Key" => {
                    entry.insert("key".to_string(), Value::String(value.to_string()));
                }
                "Source" => {
                    entry.insert("source".to_string(), Value::String(value.to_string()));
                }
                "Context" => {
                    entry.insert("context".to_string(), Value::String(value.to_string()));
                }
                "Example" | "Examples" => {
                    entry.insert("example".to_string(), Value::String(value.to_string()));
                }
                _ => {
                    values.insert(header.clone(), Value::String(value.to_string()));
                }
            }
        }
        if !entry.contains_key("key") && !entry.contains_key("source") {
            continue;
        }
        entry.insert("values".to_string(), Value::Object(values));
        entries.push(Value::Object(entry));
    }
    serde_json::to_string(&entries).context("Failed to encode LocalizationTable contents")
}

fn localization_json_to_csv(contents: &str) -> Result<Vec<u8>> {
    let entries = serde_json::from_str::<Vec<Map<String, Value>>>(contents)
        .context("LocalizationTable Contents is not valid JSON")?;
    let mut languages = BTreeSet::new();
    for entry in &entries {
        if let Some(values) = entry.get("values").and_then(Value::as_object) {
            languages.extend(values.keys().cloned());
        }
    }
    let mut headers = vec![
        "Key".to_string(),
        "Source".to_string(),
        "Context".to_string(),
        "Example".to_string(),
    ];
    headers.extend(languages.iter().cloned());
    let mut output = String::new();
    write_csv_row(&mut output, headers.iter().map(String::as_str));
    for entry in entries {
        let values = entry.get("values").and_then(Value::as_object);
        let mut row = vec![
            entry.get("key").and_then(Value::as_str).unwrap_or(""),
            entry.get("source").and_then(Value::as_str).unwrap_or(""),
            entry.get("context").and_then(Value::as_str).unwrap_or(""),
            entry
                .get("example")
                .or_else(|| entry.get("examples"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        ];
        for language in &languages {
            row.push(
                values
                    .and_then(|values| values.get(language))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
        }
        write_csv_row(&mut output, row);
    }
    Ok(output.into_bytes())
}

fn write_csv_row<'a>(output: &mut String, values: impl IntoIterator<Item = &'a str>) {
    let mut first = true;
    for value in values {
        if !first {
            output.push(',');
        }
        first = false;
        if value.contains(',')
            || value.contains('"')
            || value.contains('\n')
            || value.contains('\r')
        {
            output.push('"');
            output.push_str(&value.replace('"', "\"\""));
            output.push('"');
        } else {
            output.push_str(value);
        }
    }
    output.push('\n');
}

fn parse_csv(text: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            other => field.push(other),
        }
    }
    if quoted {
        bail!("CSV ends inside a quoted field");
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
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
    project.root = convert_rojo_root_node(tree_value)?;
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
    project.glob_ignore_paths = object
        .get("globIgnorePaths")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(project)
}

fn convert_rojo_root_node(node: &Map<String, Value>) -> Result<ProjectNode> {
    let mut converted = ProjectNode {
        id: node.get("$id").and_then(Value::as_str).map(str::to_string),
        class_name: node
            .get("$className")
            .and_then(Value::as_str)
            .map(str::to_string),
        tags: node.get("$tags").and_then(Value::as_array).map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
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
                        crate::normalize_project_typed_value(None, None, value)?,
                    );
                }
            } else {
                converted.properties.insert(
                    name.clone(),
                    crate::normalize_project_typed_value(class_name, Some(name), value)?,
                );
            }
        }
    }
    if let Some(attributes) = node.get("$attributes").and_then(Value::as_object) {
        for (name, value) in attributes {
            converted.attributes.insert(
                name.clone(),
                crate::normalize_project_typed_value(None, None, value)?,
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
    let mut converted = ProjectNode {
        id: node.get("$id").and_then(Value::as_str).map(str::to_string),
        class_name: node
            .get("$className")
            .and_then(Value::as_str)
            .map(str::to_string),
        tags: node.get("$tags").and_then(Value::as_array).map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
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
                        crate::normalize_project_typed_value(None, None, value).with_context(
                            || format!("Invalid attribute '{attribute}' in Rojo node '{target}'"),
                        )?,
                    );
                }
                continue;
            }
            converted.properties.insert(
                name.clone(),
                crate::normalize_project_typed_value(class_name, Some(name), value).with_context(
                    || format!("Invalid property '{name}' in Rojo node '{target}'"),
                )?,
            );
        }
    }
    if let Some(attributes) = node.get("$attributes").and_then(Value::as_object) {
        for (name, value) in attributes {
            converted.attributes.insert(
                name.clone(),
                crate::normalize_project_typed_value(None, None, value).with_context(|| {
                    format!("Invalid attribute '{name}' in Rojo node '{target}'")
                })?,
            );
        }
    }
    if let Some(path) = node.get("$path").and_then(Value::as_str) {
        let relative = PathBuf::from(path);
        let resolved = root.join(&relative);
        let lower_name = resolved
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mounted = lower_name.ends_with(".project.json")
            || lower_name.ends_with(".project.jsonc")
            || matches!(
                resolved.extension().and_then(OsStr::to_str),
                Some("rbxm" | "rbxmx" | "renium" | "rbsync")
            );
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
            Ok(find_workspace_root(root).join(".renium/workspace.config.json"))
        }
        ConfigScope::Experience => Ok(find_experience_root(root).join("renium.config.json")),
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

fn find_workspace_root(root: &Path) -> PathBuf {
    find_ancestor_with(root, ".git").unwrap_or_else(|| absolute_path(root))
}

fn find_experience_root(root: &Path) -> PathBuf {
    find_ancestor_with(root, "renium.experience.json").unwrap_or_else(|| absolute_path(root))
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

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
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
    atomic_write(
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

fn strip_jsonc_comments(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let current = bytes[index];
        if in_string {
            output.push(current);
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if current == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if current == b'"' {
            in_string = true;
            output.push(current);
            index += 1;
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'/') {
            output.extend_from_slice(b"  ");
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\r' | b'\n') {
                output.push(b' ');
                index += 1;
            }
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'*') {
            output.extend_from_slice(b"  ");
            index += 2;
            let mut closed = false;
            while index < bytes.len() {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    output.extend_from_slice(b"  ");
                    index += 2;
                    closed = true;
                    break;
                }
                output.push(if matches!(bytes[index], b'\r' | b'\n') {
                    bytes[index]
                } else {
                    b' '
                });
                index += 1;
            }
            if !closed {
                bail!("Unterminated block comment");
            }
            continue;
        }
        output.push(current);
        index += 1;
    }
    if in_string {
        bail!("Unterminated JSON string");
    }
    String::from_utf8(output).context("JSONC is not UTF-8")
}

fn strip_json_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let current = bytes[index];
        if in_string {
            output.push(current);
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if current == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if current == b'"' {
            in_string = true;
            output.push(current);
            index += 1;
            continue;
        }
        if current == b',' {
            let mut next = index + 1;
            while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                next += 1;
            }
            if next < bytes.len() && matches!(bytes[next], b'}' | b']') {
                output.push(b' ');
                index += 1;
                continue;
            }
        }
        output.push(current);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| text.to_string())
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
        if segment.contains(['<', '>', ':', '"', '|', '?', '*']) {
            bail!("{field} contains a non-portable segment '{segment}'");
        }
        let trimmed = segment.trim_end_matches([' ', '.']);
        if trimmed != segment || is_windows_reserved_name(trimmed) {
            bail!("{field} contains a non-portable segment '{segment}'");
        }
    }
    Ok(())
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
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
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension = source
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "lua" | "luau" | "renium" | "rbsync" | "rbxm" | "rbxmx"
    ) || name.ends_with(".project.json")
        || name.ends_with(".project.jsonc")
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
    if let Some(matcher) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(pattern)
        .cloned()
    {
        return Ok(matcher);
    }
    let matcher = Glob::new(pattern)
        .with_context(|| format!("Invalid glob '{pattern}'"))?
        .compile_matcher();
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(pattern.to_string(), matcher.clone());
    Ok(matcher)
}

fn path_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or("renium");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("Failed to create {}", temporary.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(first) = fs::rename(&temporary, path) {
        if !path.exists() {
            let _ = fs::remove_file(&temporary);
            return Err(first).with_context(|| format!("Failed to replace {}", path.display()));
        }
        let backup = path.with_file_name(format!(".{file_name}.{}.backup", std::process::id()));
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup)
            .with_context(|| format!("Failed to preserve {}", path.display()))?;
        if let Err(error) = fs::rename(&temporary, path) {
            let restore = fs::rename(&backup, path);
            let _ = fs::remove_file(&temporary);
            restore.with_context(|| {
                format!(
                    "Failed to restore {} after replacement failed: {error}",
                    path.display()
                )
            })?;
            return Err(error).with_context(|| format!("Failed to replace {}", path.display()));
        }
        if let Err(error) = fs::remove_file(&backup) {
            eprintln!(
                "[renium] warning: failed to remove write backup {}: {error}",
                backup.display()
            );
        }
    }
    Ok(())
}

fn print_json(value: &Value, pretty: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    if crate::global_pretty_output(pretty) {
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
                direction: FilterDirection::Both,
                glob: Some("Workspace/**".to_string()),
                name: None,
                class: None,
                tag: None,
                attribute: None,
                property: None,
                id: None,
            },
            FilterRule {
                action: FilterAction::Include,
                direction: FilterDirection::Both,
                glob: Some("Workspace/Keep/**".to_string()),
                name: None,
                class: None,
                tag: None,
                attribute: None,
                property: None,
                id: None,
            },
        ];
        assert!(filter_allows(&rules, FilterDirection::StudioToFiles, &candidate).unwrap());
    }
}
