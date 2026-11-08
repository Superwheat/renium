use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rbx_dom_weak::types::VariantType as RbxVariantType;
use rbx_reflection::DataType as RbxDataType;
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub(crate) mod add_batch;
pub(crate) mod edit;
pub(crate) mod explorer;
pub(crate) mod query;

use crate::app::output::{OutputMode, ReportedFailure, print_json_output};
use crate::app::timing::current_millis;
use crate::bytecode::edit::{
    bytecode_service_name, collect_settings_subtree_preorder, reject_package_link_instance_mutation,
};
use crate::bytecode::explorer::{
    BytecodeNodeProjection, explorer_search_groups, explorer_search_instance_matches,
    insert_top_field, parse_requested_fields,
};
use crate::bytecode::query::{
    BytecodeInstanceTarget, parse_property_predicates, parse_property_scope,
    resolve_bytecode_instance_index,
};
use crate::cli::{
    BytecodeApplyPropertyBatchArgs, BytecodeFileArgs, BytecodeGetPropertyArgs,
    BytecodeInstanceSelectorArgs, BytecodeSetPropertyArgs, BytecodeSetSourceArgs, FindArgs,
    HighLevelTargetArgs, InspectArgs, TreeArgs,
};
use crate::daemon::is_process_alive;
use crate::editor::document::is_protected_engine_container;
use crate::editor::paths::{
    build_editor_instance_path_parts, build_editor_instance_paths,
    build_editor_instance_paths_for_indices, build_editor_source_paths_by_index,
    document_instance_index_by_path_unique, path_ordinals_match, script_file_names,
};
use crate::editor::sync::is_lua_source_class;
use crate::project::commands::load_structural_project;
use crate::project::config;
use crate::project::layout::configured_project_layout;
use crate::project::package_links::{LinkEnforcement, build_loaded_project_link_enforcement};
use crate::rbx::decode::rbx_variant_to_settings_json;
use crate::rbx::encode::{
    rbx_logical_property_name, rbx_model_property_descriptor, rbx_property_descriptor,
};
use crate::rbx::model::{
    BytecodeModelImportRefs, canonicalize_settings_references_for_move,
    canonicalize_settings_references_for_moves, source_structure_settings_document,
};
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode,
    stabilize_reference_objects,
};
use crate::settings::instance::{self as instance_api, InstanceQuery, PropertyScope};
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::snapshot::export::ExportProjectStage;
use crate::system::files::{
    absolutize_under, canonical_path, case_folded_path_key, exact_path_key, read_file_if_present,
    service_settings_path, set_path_readonly, validate_filesystem_instance_name,
    write_bytes_if_changed, write_utf8_file,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BytecodePropertyBatchEntry {
    service: String,
    settings_id: Option<String>,
    #[serde(default)]
    class_name: String,
    path_segments: Vec<String>,
    #[serde(default)]
    path_ordinals: Vec<usize>,
    #[serde(default = "default_property_scope_name")]
    scope: String,
    property: String,
    value: Value,
}

struct ResolvedBytecodePropertyBatchEntry {
    entry_index: usize,
    service: String,
    instance_index: usize,
    scope: PropertyScope,
    property: String,
    value: Value,
}

struct BytecodePropertyBatchDocument {
    settings_file: PathBuf,
    document: SettingsBytecode,
    settings_id_indices: HashMap<String, usize>,
    source_paths_before: Option<Vec<Option<PathBuf>>>,
    settings_changed: bool,
}

struct BytecodePropertyBatchResult {
    applied: usize,
    filtered: usize,
    changed_paths: Vec<PathBuf>,
    source_paths: Vec<(usize, PathBuf)>,
}

fn default_property_scope_name() -> String {
    "property".to_string()
}

fn property_affects_source_path(scope: PropertyScope, property: &str) -> bool {
    match scope {
        PropertyScope::Auto => ["name", "parent", "runcontext"]
            .iter()
            .any(|candidate| property.eq_ignore_ascii_case(candidate)),
        PropertyScope::Metadata => ["name", "parent"]
            .iter()
            .any(|candidate| property.eq_ignore_ascii_case(candidate)),
        PropertyScope::Property => property.eq_ignore_ascii_case("runcontext"),
        PropertyScope::Attribute => false,
    }
}

fn parse_cli_value(
    value_json: Option<&str>,
    value_str: Option<&str>,
    value_num: Option<f64>,
    value_bool: Option<bool>,
    value_null: bool,
) -> Result<Value> {
    let specified = value_json.is_some() as usize
        + value_str.is_some() as usize
        + value_num.is_some() as usize
        + value_bool.is_some() as usize
        + value_null as usize;
    if specified != 1 {
        bail!("Provide exactly one value: --value-json/--json, --str, --num, --bool, or --null");
    }
    if let Some(value_json) = value_json {
        if value_json == "-" {
            return serde_json::from_reader(io::stdin().lock())
                .context("Invalid --value-json on stdin");
        }
        return serde_json::from_str(value_json)
            .with_context(|| format!("Invalid --value-json: {value_json}"));
    }
    if let Some(value_str) = value_str {
        return Ok(Value::String(value_str.to_string()));
    }
    if let Some(value_num) = value_num {
        if !value_num.is_finite() {
            bail!("--num must be finite");
        }
        return Ok(json!(value_num));
    }
    if let Some(value_bool) = value_bool {
        return Ok(Value::Bool(value_bool));
    }
    Ok(Value::Null)
}

fn unqualified_reference_ids(value: &Value, settings_ids: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                unqualified_reference_ids(value, settings_ids);
            }
        }
        Value::Object(object) => {
            let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                || object.contains_key("settingsId")
                || object.contains_key("instanceId");
            let has_path = object
                .get("pathSegments")
                .and_then(Value::as_array)
                .is_some_and(|segments| !segments.is_empty());
            if is_reference
                && !has_path
                && let Some(settings_id) = object
                    .get("settingsId")
                    .or_else(|| object.get("instanceId"))
                    .and_then(Value::as_str)
            {
                settings_ids.insert(settings_id.to_string());
            }
            for nested in object.values() {
                unqualified_reference_ids(nested, settings_ids);
            }
        }
        _ => {}
    }
}

#[derive(Clone)]
struct ReferenceTargetPath {
    service: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
}

/// Cross-service references written by hand carry only a settings id; a pull
/// stores the target path. Attach the path so both sides compare equal.
pub(crate) fn qualify_value_references(
    value: &mut Value,
    settings_file: &Path,
    own_document: &SettingsBytecode,
) -> Result<()> {
    let mut unqualified = BTreeSet::new();
    unqualified_reference_ids(value, &mut unqualified);
    if unqualified.is_empty() {
        return Ok(());
    }
    let mut documents = BTreeMap::new();
    let own_service = crate::project::storage::store_service_name(settings_file)
        .context("Service settings path has no service name")?;
    documents.insert(own_service, own_document.clone());
    if let Some(src_root) = crate::project::storage::source_directory(settings_file).parent() {
        for service_dir in crate::project::storage::service_directories(src_root)? {
            let path = service_settings_path(&service_dir);
            if path == settings_file || !path.is_file() {
                continue;
            }
            let service_name = crate::project::storage::store_service_name(&path)
                .context("Service settings path has no service name")?;
            documents.insert(service_name, SettingsBytecode::read_file(&path)?);
        }
    }
    qualify_reference_targets(value, &documents, &unqualified)
}

fn qualify_reference_targets(
    value: &mut Value,
    documents: &BTreeMap<String, SettingsBytecode>,
    requested_settings_ids: &BTreeSet<String>,
) -> Result<()> {
    let mut targets_by_settings_id = HashMap::<String, Vec<ReferenceTargetPath>>::new();
    for (service, document) in documents {
        let matches = document
            .instances
            .iter()
            .enumerate()
            .filter(|(_, instance)| requested_settings_ids.contains(&instance.settings_id))
            .collect::<Vec<_>>();
        let indices = matches.iter().map(|(index, _)| *index).collect::<Vec<_>>();
        let paths = build_editor_instance_paths_for_indices(document, service, &indices);
        for (index, instance) in matches {
            let Some(path) = paths.get(&index) else {
                continue;
            };
            targets_by_settings_id
                .entry(instance.settings_id.clone())
                .or_default()
                .push(ReferenceTargetPath {
                    service: service.clone(),
                    path_segments: path.path_segments.clone(),
                    path_ordinals: path.path_ordinals.clone(),
                });
        }
    }

    fn qualify(
        value: &mut Value,
        targets_by_settings_id: &HashMap<String, Vec<ReferenceTargetPath>>,
    ) -> Result<()> {
        match value {
            Value::Array(values) => {
                for value in values {
                    qualify(value, targets_by_settings_id)?;
                }
            }
            Value::Object(object) => {
                let is_reference = object.get("_type").and_then(Value::as_str) == Some("Ref")
                    || object.contains_key("settingsId")
                    || object.contains_key("instanceId");
                let has_path = object
                    .get("pathSegments")
                    .and_then(Value::as_array)
                    .is_some_and(|segments| !segments.is_empty());
                if is_reference && !has_path {
                    let settings_id = object
                        .get("settingsId")
                        .or_else(|| object.get("instanceId"))
                        .and_then(Value::as_str);
                    if let Some(settings_id) = settings_id {
                        let targets = targets_by_settings_id
                            .get(settings_id)
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        let [target] = targets else {
                            if targets.is_empty() {
                                bail!("Reference target was not found: {settings_id}");
                            }
                            let services = targets
                                .iter()
                                .map(|target| target.service.as_str())
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect::<Vec<_>>()
                                .join(", ");
                            bail!(
                                "Reference target is ambiguous: {settings_id} exists in {services}; include pathSegments and pathOrdinals"
                            );
                        };
                        object.insert("pathSegments".to_string(), json!(target.path_segments));
                        object.insert("pathOrdinals".to_string(), json!(target.path_ordinals));
                    }
                }
                for nested in object.values_mut() {
                    qualify(nested, targets_by_settings_id)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    qualify(value, &targets_by_settings_id)
}

fn parse_cli_source_text(value_json: Option<&str>, value_str: Option<&str>) -> Result<String> {
    let specified = value_json.is_some() as usize + value_str.is_some() as usize;
    if specified != 1 {
        bail!("Provide exactly one source value: --value-json/--json or --str");
    }
    if let Some(value_json) = value_json {
        return serde_json::from_str(value_json)
            .with_context(|| format!("Invalid --value-json: {value_json}"));
    }
    Ok(value_str.unwrap_or_default().to_string())
}

fn validate_auto_property_name(
    document: &SettingsBytecode,
    index: usize,
    property: &str,
    scope: PropertyScope,
) -> Result<()> {
    let instance = &document.instances[index];
    let stored =
        |names: &Map<String, Value>| names.keys().any(|name| name.eq_ignore_ascii_case(property));
    if scope != PropertyScope::Auto
        || matches!(property, "Name" | "Parent")
        || stored(&instance.properties)
        || stored(&instance.attributes)
    {
        return Ok(());
    }
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let class_name = instance.class_name.as_str();
    if rbx_model_property_descriptor(database, class_name, property).is_some()
        || rbx_logical_property_name(database, class_name, property).is_some()
    {
        return Ok(());
    }
    bail!(
        "Property {property:?} does not exist on {class_name}; use --scope property only for an unlisted Roblox property"
    )
}

fn canonical_stored_property_name(
    document: &SettingsBytecode,
    index: usize,
    property: &str,
    scope: PropertyScope,
) -> Result<Option<String>> {
    if !matches!(scope, PropertyScope::Auto | PropertyScope::Property)
        || property.eq_ignore_ascii_case("source")
    {
        return Ok(None);
    }
    let instance = &document.instances[index];
    if instance.properties.contains_key(property)
        || (scope == PropertyScope::Auto && instance.attributes.contains_key(property))
    {
        return Ok(None);
    }
    if let Some(existing) = instance
        .properties
        .keys()
        .find(|name| name.eq_ignore_ascii_case(property))
        .or_else(|| {
            (scope == PropertyScope::Auto)
                .then(|| {
                    instance
                        .attributes
                        .keys()
                        .find(|name| name.eq_ignore_ascii_case(property))
                })
                .flatten()
        })
    {
        return Ok(Some(existing.clone()));
    }
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let logical = rbx_logical_property_name(database, &instance.class_name, property);
    // Studio stores a migrated property under its successor's name (an
    // ImageButton's Image as ImageContent); a request by the old name reads it.
    if let Some(descriptor) =
        rbx_property_descriptor(database, &instance.class_name, logical.unwrap_or(property))
        && let rbx_reflection::PropertyKind::Canonical {
            serialization: rbx_reflection::PropertySerialization::Migrate(migration),
        } = &descriptor.kind
        && let Some(saved) = migration
            .new_property_names()
            .iter()
            .find(|name| instance.properties.contains_key(**name))
    {
        return Ok(Some((*saved).to_string()));
    }
    Ok(logical.map(str::to_string).filter(|name| name != property))
}

struct HighLevelBytecodeContext {
    service: String,
    settings_file: PathBuf,
    document: SettingsBytecode,
    children_by_parent: Vec<Vec<usize>>,
    path_segments_by_index: Vec<Option<Vec<String>>>,
    path_ordinals_by_index: Vec<Option<Vec<usize>>>,
    canonical_settings_ids_by_index: Vec<Option<String>>,
    source_paths_by_index: Vec<Option<PathBuf>>,
    _projection: Option<config::ProjectionStage>,
}

impl HighLevelBytecodeContext {
    fn projection<'a>(
        &'a self,
        mode: OutputMode,
        fields: Option<&'a HashSet<String>>,
    ) -> BytecodeNodeProjection<'a> {
        BytecodeNodeProjection {
            document: &self.document,
            children_by_parent: &self.children_by_parent,
            path_segments_by_index: &self.path_segments_by_index,
            path_ordinals_by_index: &self.path_ordinals_by_index,
            canonical_settings_ids_by_index: &self.canonical_settings_ids_by_index,
            source_paths_by_index: &self.source_paths_by_index,
            mode,
            fields,
        }
    }
}

fn high_level_context(
    project_root: &Path,
    src_root: &Path,
    service: &str,
) -> Result<HighLevelBytecodeContext> {
    validate_filesystem_instance_name(service, "service")?;
    let (project_root, src_root) = configured_project_layout(project_root, src_root)?;
    let src_root = absolutize_under(&project_root, &src_root);
    let canonical_service_dir = src_root.join(service);
    let settings_file = service_settings_path(&canonical_service_dir);
    pull_missing_service_store(&settings_file);
    let loaded = config::try_load_project(None, Some(&project_root))?
        .filter(|loaded| absolutize_under(&loaded.root, &loaded.project.source_root) == src_root);
    let projection = loaded.as_ref().map(config::stage_project).transpose()?;
    let service_dir = projection
        .as_ref()
        .map_or(canonical_service_dir, |stage| stage.root().join(service));
    let projected_settings = service_settings_path(&service_dir);
    let document = if projected_settings.exists() {
        SettingsBytecode::read_file(&projected_settings)
            .with_context(|| format!("Failed to read {}", projected_settings.display()))?
    } else if let Some(loaded) = loaded.as_ref()
        && service_dir.is_dir()
    {
        source_structure_settings_document(
            &service_dir,
            service,
            &config::project_script_naming(&loaded.project),
            &[],
        )?
    } else if loaded.is_none() {
        bail!(
            "No Renium project in {} or its parents; run the command from the project folder, pass -r DIR, or create one with `rbx init`",
            project_root.display()
        );
    } else {
        return Err(missing_service_store_error(&settings_file));
    };
    let children_by_parent = settings_children_by_parent(&document);
    let (path_segments_by_index, path_ordinals_by_index) =
        build_editor_instance_path_parts(&document, service);
    let canonical_settings_ids_by_index = document
        .instances
        .iter()
        .map(|instance| {
            projection
                .as_ref()
                .and_then(|stage| stage.canonical_identity(&instance.settings_id))
                .map_or_else(
                    || Some(instance.settings_id.clone()),
                    |(_, settings_id)| Some(settings_id.to_string()),
                )
        })
        .collect();
    let mut source_paths_by_index =
        build_editor_source_paths_by_index(&document, service, &service_dir);
    if let (Some(loaded), Some(projection)) = (loaded.as_ref(), projection.as_ref()) {
        for source in &mut source_paths_by_index {
            let Some(path) = source.as_ref() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(projection.root()) else {
                *source = None;
                continue;
            };
            *source = config::staged_path_to_project_source(loaded, relative)?
                .map(|path| canonical_path(&path).unwrap_or(path));
        }
    }

    Ok(HighLevelBytecodeContext {
        service: service.to_string(),
        settings_file,
        document,
        children_by_parent,
        path_segments_by_index,
        path_ordinals_by_index,
        canonical_settings_ids_by_index,
        source_paths_by_index,
        _projection: projection,
    })
}

pub(crate) fn stabilize_reference_output(
    document: &SettingsBytecode,
    path_segments_by_index: &[Option<Vec<String>>],
    path_ordinals_by_index: &[Option<Vec<usize>>],
    canonical_settings_ids_by_index: &[Option<String>],
    record: &mut Map<String, Value>,
) {
    stabilize_reference_objects(record, |object, index| {
        let Some(instance) = document.instances.get(index) else {
            return;
        };
        let settings_id = canonical_settings_ids_by_index
            .get(index)
            .and_then(Option::as_deref)
            .unwrap_or(&instance.settings_id);
        object.insert(
            "settingsId".to_string(),
            Value::String(settings_id.to_string()),
        );
        if let (Some(Some(path_segments)), Some(Some(path_ordinals))) = (
            path_segments_by_index.get(index),
            path_ordinals_by_index.get(index),
        ) {
            object.insert("pathSegments".to_string(), json!(path_segments));
            object.insert("pathOrdinals".to_string(), json!(path_ordinals));
        }
    });
}

struct HighLevelSelection {
    service: String,
    value: Option<String>,
    /// The value is the dotted path that followed the service in one positional.
    path_from_service: bool,
}

fn high_level_service_and_value(
    explicit_service: Option<&str>,
    first: Option<&str>,
    second: Option<&str>,
    command: &str,
    known_service: impl Fn(&str) -> bool,
) -> Result<HighLevelSelection> {
    if let Some(service) = explicit_service.filter(|value| !value.trim().is_empty()) {
        let value = match (first, second) {
            (Some(first), Some(second))
                if !first.trim().is_empty() && !second.trim().is_empty() =>
            {
                Some(format!("{} {}", first.trim(), second.trim()))
            }
            (Some(first), _) if !first.trim().is_empty() => Some(first.to_string()),
            (_, Some(second)) if !second.trim().is_empty() => Some(second.to_string()),
            _ => None,
        };
        return Ok(HighLevelSelection {
            service: service.to_string(),
            value,
            path_from_service: false,
        });
    }

    let service = first
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("Provide a service: {command} <SERVICE> [TARGET_OR_QUERY]"))?;
    let value = second
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string);
    // The whole dotted string stays the target: the service becomes the
    // root segment, so a child that carries the service's own name still
    // resolves as a child.
    if value.is_none()
        && let Some((split_service, _)) = split_leading_service(service, known_service)
    {
        return Ok(HighLevelSelection {
            service: split_service,
            value: Some(service.to_string()),
            path_from_service: true,
        });
    }
    Ok(HighLevelSelection {
        service: service.to_string(),
        value,
        path_from_service: false,
    })
}

/// `StarterGui.Main.HUD.Timer` given as the only positional names the service
/// first; the rest is the dotted path inside it.
fn split_leading_service(
    raw: &str,
    known_service: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    let (service, path) = raw.trim().split_once(['.', '/', '\\'])?;
    let service = service.trim();
    let path = path.trim();
    if service.is_empty() || path.is_empty() || !known_service(service) {
        return None;
    }
    Some((service.to_string(), path.to_string()))
}

/// A sync service class, or any service whose store the project holds.
fn project_knows_service(project_root: &Path, src_root: &Path, name: &str) -> bool {
    if crate::roblox::services::DEFAULT_SYNC_SERVICES.contains(&name) {
        return true;
    }
    if validate_filesystem_instance_name(name, "service").is_err() {
        return false;
    }
    let Ok((project_root, src_root)) = configured_project_layout(project_root, src_root) else {
        return false;
    };
    service_settings_path(&absolutize_under(&project_root, &src_root).join(name)).is_file()
}

pub(super) fn high_level_split_path(raw: &str) -> Vec<String> {
    raw.split(['/', '\\', '.'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub(super) fn parse_path_segments(raw: &str) -> Result<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("Path target cannot be empty");
    }

    let segments = if raw.starts_with('[') {
        parse_bracket_path_segments(raw).with_context(|| format!("Invalid path JSON: {raw}"))?
    } else {
        high_level_split_path(raw)
    };
    if segments.is_empty() {
        bail!("Path target cannot be empty");
    }
    Ok(segments)
}

pub(super) fn parse_bracket_path_segments(raw: &str) -> Result<Vec<String>> {
    if let Ok(segments) = serde_json::from_str::<Vec<String>>(raw) {
        return Ok(segments);
    }
    let inner = raw
        .trim()
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .context("Path must start with '[' and end with ']'")?;
    let segments = inner
        .split(',')
        .map(|segment| segment.trim().trim_matches(['"', '\'']))
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if segments.is_empty() {
        bail!("Path cannot be empty");
    }
    Ok(segments)
}

pub(super) fn high_level_path_segments(raw: &str, service: &str) -> Result<Vec<String>> {
    let mut segments = parse_path_segments(raw)?;
    if segments
        .first()
        .is_none_or(|segment| segment.as_str() != service)
    {
        segments.insert(0, service.to_string());
    }
    Ok(segments)
}

/// A dotted path may carry `[n]` after a segment, as compact output prints
/// duplicates. Returns the bare segments and, when any ordinal was given,
/// one ordinal per segment.
pub(super) fn split_inline_ordinals(segments: Vec<String>) -> (Vec<String>, Vec<usize>) {
    let mut bare = Vec::with_capacity(segments.len());
    let mut ordinals = Vec::with_capacity(segments.len());
    let mut any = false;
    for segment in segments {
        let ordinal = segment
            .strip_suffix(']')
            .and_then(|rest| rest.rsplit_once('['))
            .filter(|(name, digits)| {
                !name.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
            .and_then(|(name, digits)| digits.parse::<usize>().ok().map(|n| (name.to_string(), n)))
            .filter(|(_, n)| *n >= 1);
        match ordinal {
            Some((name, n)) => {
                any = true;
                bare.push(name);
                ordinals.push(n);
            }
            None => {
                bare.push(segment);
                ordinals.push(1);
            }
        }
    }
    if !any {
        ordinals.clear();
    }
    (bare, ordinals)
}

const HIGH_LEVEL_AMBIGUITY_LIMIT: usize = 20;

enum HighLevelTargetResolution {
    Found(usize),
    Ambiguous(Vec<usize>),
}

pub(super) fn high_level_path_ordinals(raw: Option<&str>) -> Result<Vec<usize>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) if raw.starts_with('[') => serde_json::from_str::<Vec<usize>>(raw)
            .with_context(|| format!("Invalid ordinals JSON: {raw}")),
        Some(raw) => {
            let ordinals = raw
                .split([',', '.', '/', '\\', ' ', '\t', '\n', '\r'])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| {
                    part.parse::<usize>()
                        .with_context(|| format!("Invalid ordinal: {part}"))
                })
                .collect::<Result<Vec<_>>>()?;
            if ordinals.is_empty() {
                bail!("Path ordinals cannot be empty: {raw}");
            }
            Ok(ordinals)
        }
        None => Ok(Vec::new()),
    }
}

pub(super) fn bytecode_input_looks_like_settings_file(raw: &str) -> bool {
    let raw = raw.trim();
    !raw.is_empty()
        && (raw.contains(['/', '\\'])
            || raw
                .get(raw.len().saturating_sub(7)..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".renium")))
}

pub(super) fn resolve_bytecode_settings_file(
    settings_file: Option<&Path>,
    service_or_file: Option<&str>,
    explicit_service: Option<&str>,
    project_root: &Path,
    src_root: &Path,
) -> Result<(PathBuf, String)> {
    let service_or_file = service_or_file
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let explicit_service = explicit_service
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string();
    if !explicit_service.is_empty() {
        validate_filesystem_instance_name(&explicit_service, "service")?;
    }

    if let Some(settings_file) = settings_file {
        if service_or_file.is_some() {
            bail!("Provide either SERVICE_OR_FILE or --file, not both");
        }
        return Ok((
            crate::project::storage::resolve_explicit_file(settings_file)?,
            explicit_service,
        ));
    }

    if let Some(service_or_file) = service_or_file {
        if bytecode_input_looks_like_settings_file(service_or_file) {
            return Ok((
                crate::project::storage::resolve_explicit_file(&absolutize_under(
                    project_root,
                    Path::new(service_or_file),
                ))?,
                explicit_service,
            ));
        }
        let (project_root, src_root) = configured_project_layout(project_root, src_root)?;
        let src_root = absolutize_under(&project_root, &src_root);
        let service = if explicit_service.is_empty() {
            service_or_file.to_string()
        } else {
            explicit_service
        };
        validate_filesystem_instance_name(&service, "service")?;
        return Ok((service_settings_path(&src_root.join(&service)), service));
    }

    if !explicit_service.is_empty() {
        let (project_root, src_root) = configured_project_layout(project_root, src_root)?;
        let src_root = absolutize_under(&project_root, &src_root);
        validate_filesystem_instance_name(&explicit_service, "service")?;
        return Ok((
            service_settings_path(&src_root.join(&explicit_service)),
            explicit_service,
        ));
    }

    bail!("Provide a settings file or service")
}

/// With `-f STORE` or `-s SERVICE` the store is named, so the first
/// positional is the target, not a service; clap still parses it into the
/// service slot. Returns the positional the target displaced, if any.
pub(super) fn adopt_positional_target(
    input: &mut BytecodeFileArgs,
    explicit_service: Option<&str>,
    selector: &mut BytecodeInstanceSelectorArgs,
) -> Option<String> {
    let store_named = input.settings_file.is_some()
        || explicit_service.is_some_and(|service| !service.trim().is_empty());
    if !store_named
        || input
            .service_or_file
            .as_deref()
            .is_none_or(bytecode_input_looks_like_settings_file)
    {
        return None;
    }
    let target = input.service_or_file.take();
    std::mem::replace(&mut selector.target, target)
}

/// Commands with one target refuse a second positional after it.
pub(crate) fn adopt_single_positional_target(
    input: &mut BytecodeFileArgs,
    explicit_service: Option<&str>,
    selector: &mut BytecodeInstanceSelectorArgs,
) -> Result<()> {
    if adopt_positional_target(input, explicit_service, selector).is_some() {
        bail!("Give the target once: as the positional argument, --path or one selector option");
    }
    Ok(())
}

pub(super) fn resolve_bytecode_cli_settings_file(
    settings_file: Option<&Path>,
    service_or_file: Option<&str>,
    explicit_service: Option<&str>,
) -> Result<(PathBuf, String)> {
    resolve_bytecode_settings_file(
        settings_file,
        service_or_file,
        explicit_service,
        Path::new("."),
        Path::new("src"),
    )
}

pub(super) fn resolve_bytecode_read_input(
    settings_file: Option<&Path>,
    service_or_file: Option<&str>,
    explicit_service: Option<&str>,
) -> Result<(PathBuf, SettingsBytecode, String)> {
    let (settings_file, service_hint) =
        resolve_bytecode_cli_settings_file(settings_file, service_or_file, explicit_service)?;
    pull_missing_service_store(&settings_file);
    if !settings_file.exists() {
        return Err(missing_service_store_error(&settings_file));
    }
    let document = SettingsBytecode::read_file(&settings_file)
        .with_context(|| format!("Failed to read {}", settings_file.display()))?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    Ok((settings_file, document, service))
}

pub(super) fn parse_bytecode_path_segments(
    raw: Option<&str>,
    service: &str,
) -> Result<Option<Vec<String>>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) if service.is_empty() && raw.starts_with('[') => Ok(Some(
            serde_json::from_str::<Vec<String>>(raw)
                .with_context(|| format!("Invalid path JSON: {raw}"))?,
        )),
        Some(raw) if service.is_empty() => {
            let segments = high_level_split_path(raw);
            if segments.is_empty() {
                bail!("Path target cannot be empty");
            }
            Ok(Some(segments))
        }
        Some(raw) => Ok(Some(high_level_path_segments(raw, service)?)),
        None => Ok(None),
    }
}

pub(super) struct ResolvedBytecodeSelector {
    pub index: usize,
    pub path_segments: Option<Vec<String>>,
}

pub(super) fn resolve_bytecode_selector(
    document: &SettingsBytecode,
    service: &str,
    selector: &BytecodeInstanceSelectorArgs,
    not_found: &str,
) -> Result<ResolvedBytecodeSelector> {
    if let Some(target) = selector
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if bytecode_selector_args_specified(selector) {
            bail!(
                "Give the target once: as the positional argument, --path or one selector option"
            );
        }
        return Ok(ResolvedBytecodeSelector {
            index: query::resolve_document_target(document, target)?,
            path_segments: None,
        });
    }
    let path_segments =
        parse_bytecode_path_segments(selector.path_segments_json.as_deref(), service)?;
    let path_ordinals = if path_segments.is_some() {
        high_level_path_ordinals(Some(&selector.path_ordinals_json))?
    } else {
        Vec::new()
    };
    let index = resolve_bytecode_instance_index(
        document,
        BytecodeInstanceTarget {
            path_segments: path_segments.as_deref(),
            path_ordinals: &path_ordinals,
            index: selector.index,
            settings_id: selector.settings_id.as_deref(),
            name: selector.name.as_deref(),
            class_name: selector.class_name.as_deref(),
        },
        not_found,
    )?;
    Ok(ResolvedBytecodeSelector {
        index,
        path_segments,
    })
}

/// Property reads and writes without a selector address the service itself,
/// so `bg Workspace -p Gravity` works like `in Workspace`.
pub(super) fn resolve_bytecode_selector_or_service_root(
    document: &SettingsBytecode,
    service: &str,
    selector: &BytecodeInstanceSelectorArgs,
    not_found: &str,
) -> Result<ResolvedBytecodeSelector> {
    let unselected = !service.is_empty()
        && selector
            .target
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        && !bytecode_selector_args_specified(selector);
    if unselected && let Some(index) = editor_service_root_index(document, service) {
        return Ok(ResolvedBytecodeSelector {
            index,
            path_segments: None,
        });
    }
    resolve_bytecode_selector(document, service, selector, not_found)
}

fn bytecode_selector_args_specified(selector: &BytecodeInstanceSelectorArgs) -> bool {
    selector
        .path_segments_json
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || selector.index.is_some()
        || selector
            .settings_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || selector
            .name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || selector
            .class_name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
}

pub(super) fn ensure_bytecode_service_path_segments(
    path_segments: &[String],
    service: &str,
) -> Vec<String> {
    if path_segments
        .first()
        .is_some_and(|segment| segment == service)
    {
        path_segments.to_vec()
    } else {
        let mut out = Vec::with_capacity(path_segments.len() + 1);
        out.push(service.to_string());
        out.extend(path_segments.iter().cloned());
        out
    }
}

fn high_level_match_path_candidates(
    ctx: &HighLevelBytecodeContext,
    segments: &[String],
) -> Vec<usize> {
    ctx.path_segments_by_index
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            candidate
                .as_ref()
                .is_some_and(|path_segments| path_segments == segments)
                .then_some(index)
        })
        .collect()
}

fn high_level_match_name_candidates(ctx: &HighLevelBytecodeContext, name: &str) -> Vec<usize> {
    ctx.document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| (instance.name == name).then_some(index))
        .collect()
}

fn high_level_match_class_candidates(
    ctx: &HighLevelBytecodeContext,
    class_name: &str,
) -> Vec<usize> {
    ctx.document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| (instance.class_name == class_name).then_some(index))
        .collect()
}

fn high_level_candidate_resolution(candidates: Vec<usize>) -> Result<HighLevelTargetResolution> {
    match candidates.len() {
        0 => bail!("No matching instance"),
        1 => Ok(HighLevelTargetResolution::Found(candidates[0])),
        _ => Ok(HighLevelTargetResolution::Ambiguous(candidates)),
    }
}

fn high_level_ambiguity_nodes(
    ctx: &HighLevelBytecodeContext,
    indices: &[usize],
    mode: OutputMode,
) -> Vec<Value> {
    let fields = parse_requested_fields(Some("lookup,ords"));
    let projection = ctx.projection(mode, fields.as_ref());
    indices
        .iter()
        .take(HIGH_LEVEL_AMBIGUITY_LIMIT)
        .map(|index| projection.node(*index, false))
        .collect()
}

/// Same-named matches under one parent share their path; compact output
/// prints it once and lists each match by id and ordinal.
fn high_level_shared_ambiguity(
    ctx: &HighLevelBytecodeContext,
    indices: &[usize],
) -> Option<(Value, Option<String>, Vec<Value>)> {
    let shown = &indices[..indices.len().min(HIGH_LEVEL_AMBIGUITY_LIMIT)];
    let first = ctx.path_segments_by_index.get(*shown.first()?)?.as_ref()?;
    if !shown.iter().all(|index| {
        ctx.path_segments_by_index
            .get(*index)
            .and_then(|segments| segments.as_ref())
            == Some(first)
    }) {
        return None;
    }
    let path = match crate::bytecode::explorer::compact_path_string(first, None) {
        Some(path) => Value::String(path),
        None => Value::Array(
            first
                .iter()
                .map(|segment| Value::String(segment.clone()))
                .collect(),
        ),
    };
    let class_name = &ctx.document.instances[shown[0]].class_name;
    let same_class = shown
        .iter()
        .all(|index| &ctx.document.instances[*index].class_name == class_name);
    let matches = shown
        .iter()
        .map(|index| {
            let instance = &ctx.document.instances[*index];
            let ordinal = ctx
                .path_ordinals_by_index
                .get(*index)
                .and_then(|ordinals| ordinals.as_ref())
                .and_then(|ordinals| ordinals.last().copied())
                .unwrap_or(1);
            let mut node = Map::new();
            node.insert(
                "id".to_string(),
                Value::String(instance.settings_id.clone()),
            );
            node.insert("ord".to_string(), json!(ordinal as u64));
            if !same_class {
                node.insert("c".to_string(), Value::String(instance.class_name.clone()));
            }
            Value::Object(node)
        })
        .collect();
    Some((path, same_class.then(|| class_name.clone()), matches))
}

fn high_level_print_ambiguity(
    ctx: &HighLevelBytecodeContext,
    mode: OutputMode,
    indices: &[usize],
    pretty: bool,
) -> Result<()> {
    let mut response = Map::new();
    insert_top_field(
        &mut response,
        mode,
        "error",
        Value::String("ambiguous".to_string()),
    );
    insert_top_field(&mut response, mode, "count", json!(indices.len() as u64));
    if indices.len() > HIGH_LEVEL_AMBIGUITY_LIMIT {
        insert_top_field(&mut response, mode, "truncated", Value::Bool(true));
    }
    let shared = if mode.uses_short_keys() {
        high_level_shared_ambiguity(ctx, indices)
    } else {
        None
    };
    match shared {
        Some((path, class_name, matches)) => {
            response.insert("path".to_string(), path);
            if let Some(class_name) = class_name {
                response.insert("c".to_string(), Value::String(class_name));
            }
            insert_top_field(&mut response, mode, "matches", Value::Array(matches));
        }
        None => {
            let mut nodes = high_level_ambiguity_nodes(ctx, indices, mode);
            if mode.uses_short_keys() {
                // A field every match shares is printed once at the top.
                for key in ["n", "c"] {
                    let shared = nodes
                        .first()
                        .and_then(|node| node.get(key))
                        .cloned()
                        .filter(|value| nodes.iter().all(|node| node.get(key) == Some(value)));
                    if let Some(value) = shared {
                        for node in nodes.iter_mut().filter_map(Value::as_object_mut) {
                            node.remove(key);
                        }
                        response.insert(key.to_string(), value);
                    }
                }
            }
            insert_top_field(&mut response, mode, "matches", Value::Array(nodes));
        }
    }
    print_json_output(&Value::Object(response), pretty)?;
    Err(ReportedFailure.into())
}

fn high_level_resolve_path_candidates(
    ctx: &HighLevelBytecodeContext,
    segments: &[String],
    ordinals: &[usize],
) -> Result<HighLevelTargetResolution> {
    let path_label = segments.join(".");
    let candidates = high_level_match_path_candidates(ctx, segments);
    if !ordinals.is_empty() {
        let matched = candidates
            .into_iter()
            .filter(|index| {
                ctx.path_ordinals_by_index
                    .get(*index)
                    .and_then(|candidate| candidate.as_ref())
                    .is_some_and(|candidate| path_ordinals_match(candidate, ordinals))
            })
            .collect::<Vec<_>>();
        return match matched.len() {
            0 => Err(anyhow::anyhow!(
                "No matching instance path: {path_label}; `rbx f SERVICE NAME` searches by name, `rbx tr SERVICE.PATH` lists children, `rbx pl` refreshes the store from Studio"
            )),
            1 => Ok(HighLevelTargetResolution::Found(matched[0])),
            _ => Ok(HighLevelTargetResolution::Ambiguous(matched)),
        };
    }

    match candidates.len() {
        0 => bail!(
            "No matching instance path: {path_label}; `rbx f SERVICE NAME` searches by name, `rbx tr SERVICE.PATH` lists children, `rbx pl` refreshes the store from Studio"
        ),
        1 => Ok(HighLevelTargetResolution::Found(candidates[0])),
        _ => Ok(HighLevelTargetResolution::Ambiguous(candidates)),
    }
}

fn high_level_resolve_simple_target(
    ctx: &HighLevelBytecodeContext,
    raw_target: &str,
) -> Result<HighLevelTargetResolution> {
    let name_candidates = high_level_match_name_candidates(ctx, raw_target);
    match name_candidates.len() {
        1 => return Ok(HighLevelTargetResolution::Found(name_candidates[0])),
        count if count > 1 => return Ok(HighLevelTargetResolution::Ambiguous(name_candidates)),
        _ => {}
    }

    let segments = high_level_path_segments(raw_target, &ctx.service)?;
    let direct_candidates = high_level_match_path_candidates(ctx, &segments);
    match direct_candidates.len() {
        0 => bail!(
            "No matching instance: {raw_target}; `rbx f SERVICE NAME` searches by name, `rbx tr SERVICE.PATH` lists children, `rbx pl` refreshes the store from Studio"
        ),
        1 => Ok(HighLevelTargetResolution::Found(direct_candidates[0])),
        _ => Ok(HighLevelTargetResolution::Ambiguous(direct_candidates)),
    }
}

#[derive(Clone, Copy)]
struct HighLevelTarget<'a> {
    index: Option<usize>,
    settings_id: Option<&'a str>,
    name: Option<&'a str>,
    class_name: Option<&'a str>,
    path: Option<&'a str>,
    ordinals: Option<&'a str>,
    positional: Option<&'a str>,
}

fn high_level_target<'a>(
    args: &'a HighLevelTargetArgs,
    positional: Option<&'a str>,
) -> HighLevelTarget<'a> {
    HighLevelTarget {
        index: args.index,
        settings_id: args.settings_id.as_deref(),
        name: args.name.as_deref(),
        class_name: args.class_name.as_deref(),
        path: args.path.as_deref(),
        ordinals: args.ords.as_deref(),
        positional,
    }
}

impl HighLevelTarget<'_> {
    fn has_selector(&self) -> bool {
        self.index.is_some()
            || self.settings_id.is_some_and(|value| !value.is_empty())
            || self.name.is_some_and(|value| !value.is_empty())
            || self.class_name.is_some_and(|value| !value.is_empty())
    }
}

fn high_level_target_resolution(
    ctx: &HighLevelBytecodeContext,
    target: HighLevelTarget<'_>,
) -> Result<HighLevelTargetResolution> {
    let has_selector = target.has_selector();
    let path = target.path.filter(|value| !value.trim().is_empty());
    let positional_target = target.positional.filter(|value| !value.trim().is_empty());

    if path.is_some() && positional_target.is_some() {
        bail!("Provide either --path or a positional target, not both");
    }

    if let Some(raw_target) = path.or(positional_target) {
        if has_selector {
            bail!("Path target cannot be combined with another selector");
        }
        let ordinals = high_level_path_ordinals(target.ordinals)?;
        if path.is_some()
            || !ordinals.is_empty()
            || raw_target.starts_with('[')
            || raw_target.ends_with(']')
            || raw_target.chars().any(|ch| matches!(ch, '/' | '\\' | '.'))
        {
            let segments = high_level_path_segments(raw_target, &ctx.service)?;
            let (segments, inline) = if raw_target.starts_with('[') {
                (segments, Vec::new())
            } else {
                split_inline_ordinals(segments)
            };
            if !inline.is_empty() && !ordinals.is_empty() {
                bail!("Give duplicate ordinals inline (Name[2]) or with --ords, not both");
            }
            let ordinals = if inline.is_empty() { ordinals } else { inline };
            return high_level_resolve_path_candidates(ctx, &segments, &ordinals);
        }
        return high_level_resolve_simple_target(ctx, raw_target);
    }

    if target
        .ordinals
        .is_some_and(|value| !value.trim().is_empty())
    {
        bail!("--ords requires --path or a positional target");
    }

    if !has_selector {
        return editor_service_root_index(&ctx.document, &ctx.service)
            .map(HighLevelTargetResolution::Found)
            .ok_or_else(|| anyhow::anyhow!("No service root in settings bytecode"));
    }

    if let Some(index) = target.index {
        if index < ctx.document.instances.len() {
            return Ok(HighLevelTargetResolution::Found(index));
        }
        bail!("No matching instance index {index}");
    }

    if let Some(settings_id) = target.settings_id.filter(|value| !value.is_empty()) {
        return ctx
            .document
            .instances
            .iter()
            .position(|instance| instance.settings_id == settings_id)
            .map(HighLevelTargetResolution::Found)
            .ok_or_else(|| anyhow::anyhow!("No matching instance"));
    }

    if let Some(name) = target.name.filter(|value| !value.is_empty()) {
        return high_level_candidate_resolution(high_level_match_name_candidates(ctx, name));
    }

    if let Some(class_name) = target.class_name.filter(|value| !value.is_empty()) {
        return high_level_candidate_resolution(high_level_match_class_candidates(ctx, class_name));
    }

    bail!("Provide a target, --path, --index, --settings-id, --name, or --class-name");
}

fn high_level_visible_tree(
    children_by_parent: &[Vec<usize>],
    root_index: usize,
    depth: usize,
) -> HashSet<usize> {
    let mut visible = HashSet::new();
    let mut queue = VecDeque::from([(root_index, 0usize)]);
    while let Some((index, current_depth)) = queue.pop_front() {
        if !visible.insert(index) || current_depth >= depth {
            continue;
        }
        if let Some(children) = children_by_parent.get(index) {
            for child_index in children {
                queue.push_back((*child_index, current_depth + 1));
            }
        }
    }
    visible
}

fn high_level_response(ctx: HighLevelBytecodeContext, mode: OutputMode) -> Map<String, Value> {
    let mut response = Map::new();
    insert_top_field(
        &mut response,
        mode,
        "settingsFile",
        json!(ctx.settings_file),
    );
    insert_top_field(&mut response, mode, "service", Value::String(ctx.service));
    response
}

pub(super) fn find_command(args: FindArgs) -> Result<()> {
    let selection = high_level_service_and_value(
        args.service.as_deref(),
        args.query_or_service.as_deref(),
        args.query.as_deref(),
        "find",
        |name| project_knows_service(&args.project.project_root, &args.project.src_root, name),
    )?;
    let service = selection.service;
    let (query_text, path_from_service) = if selection.path_from_service {
        (None, selection.value)
    } else {
        (selection.value, None)
    };
    let ctx = high_level_context(&args.project.project_root, &args.project.src_root, &service)?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(Some(args.fields.as_str()));
    let properties = parse_property_predicates(&args.properties)?;
    let attributes = parse_property_predicates(&args.attributes)?;
    let scope_path = args
        .path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or(path_from_service.as_deref());
    let has_structured_filters = args.name.as_deref().is_some_and(|value| !value.is_empty())
        || args
            .class_name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || args
            .parent_settings_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || scope_path.is_some()
        || args.tag.as_deref().is_some_and(|value| !value.is_empty())
        || !properties.is_empty()
        || !attributes.is_empty();
    let groups = query_text
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(explorer_search_groups)
        .unwrap_or_default();
    if groups.is_empty() && !has_structured_filters {
        bail!("Provide a query or filter: find <SERVICE> <QUERY> or find <SERVICE> --class Script");
    }

    let scope_parent = args
        .parent_settings_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if scope_path.is_some() && scope_parent.is_some() {
        bail!("Scope the search with --path or -I, not both");
    }
    let scope = match scope_path {
        Some(path) => {
            let target = HighLevelTarget {
                index: None,
                settings_id: None,
                name: None,
                class_name: None,
                path: Some(path),
                ordinals: args.ords.as_deref(),
                positional: None,
            };
            match high_level_target_resolution(&ctx, target)? {
                HighLevelTargetResolution::Found(root_index) => Some(high_level_visible_tree(
                    &ctx.children_by_parent,
                    root_index,
                    usize::MAX,
                )),
                HighLevelTargetResolution::Ambiguous(indices) => {
                    return high_level_print_ambiguity(&ctx, mode, &indices, args.pretty);
                }
            }
        }
        None => scope_parent
            .map(|raw| {
                query::resolve_document_id_or_target(&ctx.document, raw).map(|root_index| {
                    high_level_visible_tree(&ctx.children_by_parent, root_index, usize::MAX)
                })
            })
            .transpose()?,
    };

    let structured_matches = has_structured_filters.then(|| {
        let query = InstanceQuery {
            name: args.name,
            class_name: args.class_name,
            parent_settings_id: None,
            tag: args.tag,
            properties,
            attributes,
        };
        instance_api::find_instances(&ctx.document, &query)
            .into_iter()
            .collect::<HashSet<_>>()
    });

    let limit = if args.all { 0 } else { args.limit };
    let mut match_indices = Vec::new();
    for index in 0..ctx.document.instances.len() {
        if let Some(scope) = scope.as_ref()
            && !scope.contains(&index)
        {
            continue;
        }
        if let Some(structured_matches) = structured_matches.as_ref()
            && !structured_matches.contains(&index)
        {
            continue;
        }
        if !groups.is_empty()
            && !explorer_search_instance_matches(
                &ctx.document,
                &ctx.path_segments_by_index,
                index,
                &groups,
            )
        {
            continue;
        }
        match_indices.push(index);
        if limit > 0 && match_indices.len() >= limit {
            break;
        }
    }

    let projection = ctx.projection(mode, fields.as_ref());
    let matches = match_indices
        .into_iter()
        .map(|index| projection.node(index, false))
        .collect::<Vec<_>>();

    if mode.uses_short_keys() {
        return print_json_output(&Value::Array(matches), args.pretty);
    }

    let mut response = high_level_response(ctx, mode);
    insert_top_field(&mut response, mode, "matches", Value::Array(matches));
    print_json_output(&Value::Object(response), args.pretty)
}

pub(super) fn tree_command(args: TreeArgs) -> Result<()> {
    let HighLevelSelection {
        service,
        value: positional,
        ..
    } = high_level_service_and_value(
        args.target.service.as_deref(),
        args.target.service_or_target.as_deref(),
        args.target.target.as_deref(),
        "tree",
        |name| project_knows_service(&args.project.project_root, &args.project.src_root, name),
    )?;
    let ctx = high_level_context(&args.project.project_root, &args.project.src_root, &service)?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(Some(args.fields.as_str()));
    let root_index = match high_level_target_resolution(
        &ctx,
        high_level_target(&args.target, positional.as_deref()),
    )? {
        HighLevelTargetResolution::Found(index) => index,
        HighLevelTargetResolution::Ambiguous(indices) => {
            return high_level_print_ambiguity(&ctx, mode, &indices, args.pretty);
        }
    };
    let visible_indices = high_level_visible_tree(&ctx.children_by_parent, root_index, args.depth);
    let node_indices = (0..ctx.document.instances.len())
        .filter(|index| visible_indices.contains(index))
        .take(args.limit.unwrap_or(usize::MAX))
        .collect::<Vec<_>>();
    let visible_indices = node_indices.iter().copied().collect::<HashSet<_>>();
    let projection = ctx.projection(mode, fields.as_ref());
    let nodes = node_indices
        .into_iter()
        .map(|index| projection.search_node(index, &visible_indices))
        .collect::<Vec<_>>();
    if mode.uses_short_keys() {
        return print_json_output(&Value::Array(nodes), args.pretty);
    }

    let root_ids = ctx
        .document
        .instances
        .get(root_index)
        .map(|instance| vec![Value::String(instance.settings_id.clone())])
        .unwrap_or_default();

    let mut response = high_level_response(ctx, mode);
    insert_top_field(&mut response, mode, "rootIds", Value::Array(root_ids));
    insert_top_field(&mut response, mode, "nodes", Value::Array(nodes));
    print_json_output(&Value::Object(response), args.pretty)
}

pub(crate) fn inspect_command(args: InspectArgs) -> Result<()> {
    let HighLevelSelection {
        service,
        value: positional,
        ..
    } = high_level_service_and_value(
        args.target.service.as_deref(),
        args.target.service_or_target.as_deref(),
        args.target.target.as_deref(),
        "inspect",
        |name| project_knows_service(&args.project.project_root, &args.project.src_root, name),
    )?;
    let ctx = high_level_context(&args.project.project_root, &args.project.src_root, &service)?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(Some(args.fields.as_str()));
    let projection = ctx.projection(mode, fields.as_ref());
    // The first target may carry flag selectors; the extra positionals are
    // names or dotted paths in the same store, resolved on the same decode.
    let mut nodes = Vec::new();
    let mut targets = vec![high_level_target(&args.target, positional.as_deref())];
    targets.extend(args.more_targets.iter().map(|target| HighLevelTarget {
        index: None,
        settings_id: None,
        name: None,
        class_name: None,
        path: None,
        ordinals: None,
        positional: Some(target.as_str()),
    }));
    for target in targets {
        let index = match high_level_target_resolution(&ctx, target)? {
            HighLevelTargetResolution::Found(index) => index,
            HighLevelTargetResolution::Ambiguous(indices) => {
                return high_level_print_ambiguity(&ctx, mode, &indices, args.pretty);
            }
        };
        nodes.push(projection.node(index, true));
    }
    if args.more_targets.is_empty() {
        return print_json_output(&nodes.remove(0), args.pretty);
    }
    print_json_output(&Value::Array(nodes), args.pretty)
}

pub(super) fn bytecode_get_property(mut args: BytecodeGetPropertyArgs) -> Result<()> {
    if let Some(property) = adopt_positional_target(&mut args.input, None, &mut args.selector) {
        // The store file took the first positional; the word that was parsed
        // as the target is really the first property name.
        args.property_after_target.insert(0, property);
    }
    let properties = get_property_names(&mut args)?;
    let projected_service = project_service_input(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    );
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    let direct = read_bytecode_document_if_present(&settings_file, &service_hint)?;
    let use_project = projected_service.is_some()
        && direct.as_ref().is_none_or(|(document, service)| {
            resolve_bytecode_selector_or_service_root(
                document,
                service,
                &args.selector,
                "No matching instance",
            )
            .is_err()
        });
    let (document, service, source_paths, canonical_settings_ids) = if use_project {
        let service = projected_service.context("Project service is missing")?;
        let context = high_level_context(Path::new("."), Path::new("src"), service)?;
        (
            context.document,
            context.service,
            Some(context.source_paths_by_index),
            Some(context.canonical_settings_ids_by_index),
        )
    } else {
        let (document, service) =
            direct.ok_or_else(|| missing_service_store_error(&settings_file))?;
        (document, service, None, None)
    };
    let scope = parse_property_scope(&args.scope)?;
    let index = resolve_bytecode_selector_or_service_root(
        &document,
        &service,
        &args.selector,
        "No matching instance",
    )?
    .index;
    let (path_segments, path_ordinals) = build_editor_instance_path_parts(&document, &service);
    let mut values = Map::new();
    for property in &properties {
        let value = stored_property_value(
            &document,
            &service,
            &settings_file,
            index,
            property,
            scope,
            source_paths.as_deref(),
            canonical_settings_ids.as_deref().unwrap_or(&[]),
            (&path_segments, &path_ordinals),
        )?;
        values.insert(property.clone(), value);
    }
    // One property prints its value alone, as before; several print one object
    // keyed by the names as given, so one call reads a whole set.
    if let [property] = properties.as_slice() {
        let value = values
            .remove(property)
            .context("Property output is missing")?;
        return print_json_output(&value, args.pretty);
    }
    print_json_output(&Value::Object(values), args.pretty)
}

type EditorPathParts<'a> = (&'a [Option<Vec<String>>], &'a [Option<Vec<usize>>]);

#[allow(clippy::too_many_arguments)]
fn stored_property_value(
    document: &SettingsBytecode,
    service: &str,
    settings_file: &Path,
    index: usize,
    property: &str,
    scope: PropertyScope,
    source_paths: Option<&[Option<PathBuf>]>,
    canonical_settings_ids: &[Option<String>],
    (path_segments, path_ordinals): EditorPathParts<'_>,
) -> Result<Value> {
    if property.eq_ignore_ascii_case("source")
        && is_lua_source_class(&document.instances[index].class_name)
    {
        let direct_source_paths;
        let source_paths = if let Some(source_paths) = source_paths {
            source_paths
        } else {
            let service_directory = crate::project::storage::source_directory(settings_file);
            let service_dir = service_directory.as_path();
            direct_source_paths =
                build_editor_source_paths_by_index(document, service, service_dir);
            &direct_source_paths
        };
        if let Some(Some(path)) = source_paths.get(index)
            && path.exists()
        {
            let source = fs::read_to_string(path)
                .with_context(|| format!("Failed to read source mirror {}", path.display()))?;
            return Ok(json!(source));
        }
    }
    let canonical_property = canonical_stored_property_name(document, index, property, scope)?;
    let value =
        instance_api::get_instance_property(document, index, property, scope).or_else(|| {
            canonical_property.as_deref().and_then(|property| {
                instance_api::get_instance_property(document, index, property, scope)
            })
        });
    if let Some(value) = value {
        let mut record = Map::from_iter([("value".to_string(), value)]);
        stabilize_reference_output(
            document,
            path_segments,
            path_ordinals,
            canonical_settings_ids,
            &mut record,
        );
        return record.remove("value").context("Property output is missing");
    }
    if matches!(scope, PropertyScope::Auto | PropertyScope::Property)
        && let Some((_, value)) =
            reflected_property_default(&document.instances[index].class_name, property)
    {
        return Ok(value);
    }
    bail!(
        "{}",
        property_not_found_message(&document.instances[index], service, property, scope)
    )
}

const PROPERTY_CANDIDATE_LIMIT: usize = 5;

fn property_not_found_message(
    instance: &SettingsBytecodeInstance,
    service: &str,
    property: &str,
    scope: PropertyScope,
) -> String {
    let (label, stored): (&str, Vec<&str>) = match scope {
        PropertyScope::Attribute => (
            "stored attributes include",
            instance.attributes.keys().map(String::as_str).collect(),
        ),
        PropertyScope::Auto => (
            "stored properties include",
            instance
                .properties
                .keys()
                .chain(instance.attributes.keys())
                .map(String::as_str)
                .collect(),
        ),
        _ => (
            "stored properties include",
            instance.properties.keys().map(String::as_str).collect(),
        ),
    };
    let stored = closest_property_names(property, stored);
    let known = if matches!(scope, PropertyScope::Auto | PropertyScope::Property) {
        closest_property_names(property, reflected_property_names(&instance.class_name))
            .into_iter()
            .filter(|name| !stored.iter().any(|seen| seen.eq_ignore_ascii_case(name)))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut message = format!("Property not found: {property}");
    if !stored.is_empty() {
        message.push_str(&format!("; {label} {}", stored.join(", ")));
    }
    if !known.is_empty() {
        message.push_str(&format!(
            "; {} properties include {}",
            instance.class_name,
            known.join(", ")
        ));
    }
    message.push_str(&format!(
        " (rbx in {service} {} -F props lists all)",
        instance.name
    ));
    message
}

fn reflected_property_names(class_name: &str) -> Vec<&'static str> {
    let Ok(database) = rbx_reflection_database::get() else {
        return Vec::new();
    };
    let Some(class) = database.classes.get(class_name) else {
        return Vec::new();
    };
    database
        .superclasses_iter(class)
        .flat_map(|class| class.properties.values())
        .filter(|property| {
            matches!(
                property.kind,
                rbx_reflection::PropertyKind::Canonical { .. }
            )
        })
        .map(|property| property.name)
        .collect()
}

/// Up to five names close to `wanted`: case-insensitive substring matches
/// first, then the smallest edit distance, ties by name. A name that shares
/// a long prefix (AssetType for AssetId) counts as well.
fn closest_property_names<'a>(
    wanted: &str,
    names: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let wanted_lower = wanted.to_ascii_lowercase();
    let distance_limit = (wanted_lower.len() / 2).max(2);
    let prefix_limit = (wanted_lower.len() / 2).max(3);
    let mut seen = HashSet::new();
    let mut ranked = Vec::new();
    for name in names {
        let lower = name.to_ascii_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        let substring = lower.contains(&wanted_lower) || wanted_lower.contains(&lower);
        let distance = edit_distance(&wanted_lower, &lower);
        let shared_prefix = wanted_lower
            .bytes()
            .zip(lower.bytes())
            .take_while(|(left, right)| left == right)
            .count();
        if substring || distance <= distance_limit || shared_prefix >= prefix_limit {
            ranked.push((!substring, distance, name));
        }
    }
    ranked.sort_unstable();
    ranked
        .into_iter()
        .take(PROPERTY_CANDIDATE_LIMIT)
        .map(|(_, _, name)| name.to_string())
        .collect()
}

fn edit_distance(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (row, left_char) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != right_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// The property names a `bg` call asks for, from `-p` flags and the words
/// after the target, in the order given and without repeats.
pub(crate) fn get_property_names(args: &mut BytecodeGetPropertyArgs) -> Result<Vec<String>> {
    let mut properties = Vec::new();
    for property in args
        .property
        .drain(..)
        .chain(args.property_after_target.drain(..))
    {
        if !properties.contains(&property) {
            properties.push(property);
        }
    }
    // With a flag selector (-i, -x, -n, -c, --path) the first positional word
    // is a property name, not a target.
    let selector = &mut args.selector;
    let flagged = selector.settings_id.is_some()
        || selector.index.is_some()
        || selector.name.is_some()
        || selector.class_name.is_some()
        || selector.path_segments_json.is_some();
    if flagged
        && let Some(property) = selector.target.take()
        && !properties.contains(&property)
    {
        properties.insert(0, property);
    }
    if !properties.is_empty() {
        return Ok(properties);
    }
    bail!(
        "Name the property: rbx bg SERVICE TARGET PROPERTY [PROPERTY...], or -p PROPERTY for the service itself"
    )
}

fn reflected_property_default(class_name: &str, name: &str) -> Option<(String, Value)> {
    if ["Source", "Name", "ClassName", "Parent"]
        .iter()
        .any(|key| name.eq_ignore_ascii_case(key))
    {
        return None;
    }
    let database = rbx_reflection_database::get().ok()?;
    let class = database.classes.get(class_name)?;
    let logical_name = rbx_logical_property_name(database, class_name, name)?;
    let property = rbx_property_descriptor(database, class_name, logical_name)?;
    let descriptor =
        rbx_model_property_descriptor(database, class_name, property.name).unwrap_or(property);
    let value = database
        .find_default_property(class, descriptor.name)
        .or_else(|| database.find_default_property(class, property.name))
        .and_then(|default| {
            rbx_variant_to_settings_json(
                default,
                Some(descriptor),
                database,
                &BytecodeModelImportRefs::default(),
            )
        })
        .or_else(|| {
            matches!(
                descriptor.data_type,
                RbxDataType::Value(RbxVariantType::Ref)
            )
            .then_some(Value::Null)
        })?;
    Some((logical_name.to_string(), value))
}

/// The Workspace's viewport camera belongs to Studio: every comparison and
/// pull leaves it alone, so an edit stored on it would never reach Studio.
fn reject_viewport_camera_edit(document: &SettingsBytecode, index: usize) -> Result<()> {
    let instance = &document.instances[index];
    if instance.class_name != "Camera" {
        return Ok(());
    }
    let marked =
        crate::settings::equivalence::is_reconciliation_protected_workspace_camera(document, index);
    let unmarked_root_camera = instance.parent_index.is_some_and(|parent| {
        let root = &document.instances[parent];
        root.parent_index.is_none()
            && root.class_name == "Workspace"
            && !root.properties.contains_key("CurrentCamera")
    });
    if marked || unmarked_root_camera {
        bail!(
            "{} is the Workspace viewport camera, which Studio owns; Renium does not sync it, so its properties cannot be set in the store",
            instance.name
        );
    }
    Ok(())
}

/// Refuses a store edit inside a read-only link target of the store's
/// project, as the projected commands and pushes do.
fn reject_read_only_link_store_edit(
    settings_file: &Path,
    document: &SettingsBytecode,
    service: &str,
    index: usize,
) -> Result<()> {
    let Some(loaded) = config::try_load_project(None, settings_file.parent())? else {
        return Ok(());
    };
    let enforcement = build_loaded_project_link_enforcement(&loaded, false)?;
    if !enforcement.is_active() {
        return Ok(());
    }
    match build_editor_instance_paths_for_indices(document, service, &[index]).get(&index) {
        Some(path) => enforcement.reject_read_only_package_path(
            service,
            &path.path_segments,
            &path.path_ordinals,
        ),
        None => Ok(()),
    }
}

/// Refuses a store edit of an instance that Roblox or a package owns: a
/// PackageLink, the Workspace viewport camera and, unless packages are
/// overridden, anything inside a read-only link.
fn reject_owned_instance_edit(
    settings_file: &Path,
    document: &SettingsBytecode,
    service: &str,
    index: usize,
    override_packages: bool,
) -> Result<()> {
    reject_package_link_instance_mutation(document, index, "edited")?;
    reject_viewport_camera_edit(document, index)?;
    if !override_packages {
        reject_read_only_link_store_edit(settings_file, document, service, index)?;
    }
    Ok(())
}

/// The name one property of the instance at `index` is stored under, with
/// `value` turned into its stored form: a Roblox property takes its logical
/// name and typed value, and one a push cannot set is refused. The flag says
/// the stored name differs from the requested one.
fn stored_property_edit(
    document: &mut SettingsBytecode,
    index: usize,
    property: &str,
    value: &mut Value,
    scope: PropertyScope,
) -> Result<(String, bool)> {
    validate_auto_property_name(document, index, property, scope)?;
    let canonical_property = canonical_stored_property_name(document, index, property, scope)?;
    let logical_property = if matches!(scope, PropertyScope::Auto | PropertyScope::Property)
        && !property.eq_ignore_ascii_case("source")
        && !(scope == PropertyScope::Auto
            && document.instances[index]
                .attributes
                .keys()
                .any(|name| name.eq_ignore_ascii_case(property)))
    {
        let database =
            rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
        rbx_logical_property_name(database, &document.instances[index].class_name, property)
            .map(str::to_string)
    } else {
        None
    };
    if let Some(logical_property) = logical_property.as_deref() {
        document.instances[index].properties.retain(|name, _| {
            name == logical_property || !name.eq_ignore_ascii_case(logical_property)
        });
        *value = crate::rbx::encode::typed_store_property_value(
            &document.instances[index].class_name,
            logical_property,
            std::mem::take(value),
        )?;
    }
    let requested_differs = canonical_property.is_some();
    let stored = logical_property
        .or(canonical_property)
        .unwrap_or_else(|| property.to_string());
    Ok((stored, requested_differs))
}

pub(super) fn bytecode_set_property(mut args: BytecodeSetPropertyArgs) -> Result<()> {
    if let Some(query) = args.where_query.take() {
        return bytecode_set_property_where(args, &query);
    }
    // A push pairs the store with Studio by path, so a renamed instance is
    // renamed in the place first; otherwise the push would create a copy
    // under the new name beside the old one.
    let renames = args.push
        && args.property.iter().any(|name| {
            name.eq_ignore_ascii_case("name") || name.to_ascii_lowercase().starts_with("name=")
        });
    let before = if renames {
        selected_store_path(&args)?
    } else {
        None
    };
    let mut result = set_properties_on_target(args.clone())?;
    let new_name = result
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .get("pathSegments")
                .and_then(Value::as_array)
                .and_then(|segments| segments.last())
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    if let Some(before) = before
        && let Some(new_name) = new_name
    {
        result["studioRenamed"] = crate::bytecode::edit::rename_in_studio(&[(before, new_name)])?;
    }
    finish_set_property(args, result)
}

/// The full path of the instance a set-property call selects, before the
/// edit; `None` when the selector names the service root.
fn selected_store_path(args: &BytecodeSetPropertyArgs) -> Result<Option<Vec<String>>> {
    let mut args = args.clone();
    adopt_single_positional_target(&mut args.input, None, &mut args.selector)?;
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    let document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let resolved = resolve_bytecode_selector_or_service_root(
        &document,
        &service,
        &args.selector,
        "No matching instance",
    )?;
    let (segments, _) = build_editor_instance_path_parts(&document, &service);
    Ok(segments.get(resolved.index).cloned().flatten())
}

/// The store edit itself: one `-p NAME` with a value flag, or several
/// `-p NAME=JSON`, applied to the selected instance.
fn set_properties_on_target(mut args: BytecodeSetPropertyArgs) -> Result<Value> {
    let names = std::mem::take(&mut args.property);
    let inline_values = names.iter().any(|name| name.contains('='));
    let result = if inline_values {
        if args.value_json.is_some()
            || args.value_str.is_some()
            || args.value_num.is_some()
            || args.value_bool.is_some()
            || args.value_null
        {
            bail!("Give values as -p NAME=JSON or with one -p NAME and a value flag, not both");
        }
        let mut combined = json!({ "ok": true, "set": [], "changedPaths": [] });
        for assignment in &names {
            let (name, value) = assignment
                .split_once('=')
                .with_context(|| format!("-p {assignment} needs NAME=JSON"))?;
            let mut one = args.clone();
            one.property = vec![name.to_string()];
            one.value_json = Some(value.to_string());
            let outcome = bytecode_set_property_result(one)?;
            combined["set"].as_array_mut().unwrap().push(json!(name));
            for path in outcome
                .get("changedPaths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let paths = combined["changedPaths"].as_array_mut().unwrap();
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
            for key in ["settingsFile", "settingsId", "pathSegments", "pathOrdinals"] {
                if let Some(value) = outcome.get(key) {
                    combined[key] = value.clone();
                }
            }
        }
        combined
    } else {
        if names.len() != 1 {
            bail!("Give one -p NAME with a value flag, or -p NAME=JSON for each property");
        }
        args.property = names;
        bytecode_set_property_result(args)?
    };
    Ok(result)
}

/// Pushes the edited instance when asked, with its read-back, and prints.
fn finish_set_property(args: BytecodeSetPropertyArgs, mut result: Value) -> Result<()> {
    if args.push {
        if result.get("settingsId").is_none() {
            let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
                args.input.settings_file.as_deref(),
                args.input.service_or_file.as_deref(),
                None,
            )?;
            let document = SettingsBytecode::read_file(&settings_file)?;
            let service = bytecode_service_name(&document, &settings_file, &service_hint);
            let resolved = resolve_bytecode_selector_or_service_root(
                &document,
                &service,
                &args.selector,
                "No matching instance",
            )?;
            result["settingsId"] = json!(document.instances[resolved.index].settings_id);
        }
        let names = result
            .get("set")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .or_else(|| {
                result
                    .get("property")
                    .and_then(Value::as_str)
                    .map(|name| vec![name.to_string()])
            })
            .unwrap_or_default();
        let attribute_scope = args.scope.eq_ignore_ascii_case("attribute");
        let (properties, attributes) = if attribute_scope {
            (Vec::new(), names)
        } else {
            (names, Vec::new())
        };
        crate::bytecode::edit::push_stored_instance_reading(&mut result, &properties, &attributes)?;
    }
    print_json_output(&result, args.pretty)
}

const WHERE_LIMIT: usize = 500;
const WHERE_READ_BACK_LIMIT: usize = 64;

/// `bs --where`: the assignments apply to every instance under the selected
/// target (the whole service without one) that matches the query; `--push`
/// sends the matches together and `studio` reads them back.
fn bytecode_set_property_where(mut args: BytecodeSetPropertyArgs, query: &str) -> Result<()> {
    adopt_single_positional_target(&mut args.input, None, &mut args.selector)?;
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    let document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let scope = resolve_bytecode_selector_or_service_root(
        &document,
        &service,
        &args.selector,
        "No matching instance",
    )?;
    let matched = where_matches(&document, &service, scope.index, query)?;
    let (segments, _) = crate::editor::paths::build_editor_instance_path_parts(&document, &service);
    let path_of = |index: usize| {
        segments
            .get(index)
            .cloned()
            .flatten()
            .map(|parts| parts.join("."))
            .unwrap_or_else(|| document.instances[index].name.clone())
    };
    if matched.is_empty() {
        bail!(
            "No instance under {} matches '{query}'",
            path_of(scope.index)
        );
    }
    if matched.len() > WHERE_LIMIT {
        bail!(
            "{} instances under {} match '{query}'; narrow the query ({WHERE_LIMIT} at most per call)",
            matched.len(),
            path_of(scope.index)
        );
    }
    let ids = matched
        .iter()
        .map(|&index| document.instances[index].settings_id.clone())
        .collect::<Vec<_>>();
    let push = args.push;
    args.push = false;
    let mut set_names: Vec<String> = Vec::new();
    let mut changed_paths = Vec::new();
    for id in &ids {
        let mut one = args.clone();
        one.selector = BytecodeInstanceSelectorArgs::by_settings_id(Some(id.clone()));
        let outcome = set_properties_on_target(one)?;
        if set_names.is_empty() {
            set_names = outcome
                .get("set")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .or_else(|| {
                    outcome
                        .get("property")
                        .and_then(Value::as_str)
                        .map(|name| vec![name.to_string()])
                })
                .unwrap_or_default();
        }
        for path in outcome
            .get("changedPaths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !changed_paths.contains(path) {
                changed_paths.push(path.clone());
            }
        }
    }
    let mut result = json!({
        "ok": true,
        "where": query,
        "scope": path_of(scope.index),
        "matched": ids.len(),
        "instances": matched.iter().map(|&index| path_of(index)).collect::<Vec<_>>(),
        "set": set_names,
        "changedPaths": changed_paths,
        "settingsFile": settings_file,
    });
    if push {
        result["push"] = crate::bytecode::edit::push_stored_instances(settings_file.clone(), ids)?;
        let (properties, attributes) = if args.scope.eq_ignore_ascii_case("attribute") {
            (Vec::new(), set_names)
        } else {
            (set_names, Vec::new())
        };
        let paths = matched
            .iter()
            .filter_map(|&index| segments.get(index).cloned().flatten())
            .collect::<Vec<_>>();
        if paths.len() > WHERE_READ_BACK_LIMIT {
            result["studioLimit"] = json!(WHERE_READ_BACK_LIMIT);
        }
        let read = &paths[..paths.len().min(WHERE_READ_BACK_LIMIT)];
        if let Ok(studio) =
            crate::bytecode::edit::read_back_many_from_studio(read, &properties, &attributes)
        {
            let rows = studio.as_array().cloned().unwrap_or_default();
            let mut verified = Some(rows.len() == read.len());
            for ((row, path), &index) in rows.iter().zip(read).zip(matched.iter()) {
                match crate::bytecode::edit::read_back_verifies(
                    row,
                    path,
                    &document.instances[index],
                    &properties,
                    &attributes,
                ) {
                    Some(true) => {}
                    Some(false) => verified = Some(false),
                    None => {
                        if verified == Some(true) {
                            verified = None;
                        }
                    }
                }
            }
            if let Some(verified) = verified {
                result["verified"] = json!(verified);
                if let Some(push) = result.get_mut("push").and_then(Value::as_object_mut) {
                    push.remove("verified");
                }
            }
            result["studio"] = studio;
        }
    }
    print_json_output(&result, args.pretty)
}

/// Renames every instance under the scope that matches `query` and whose name
/// contains `from`, replacing `from` with `to`; one push covers them all and
/// the read-back judges `verified`.
pub(crate) fn bytecode_rename_where(
    settings_file: PathBuf,
    scope_settings_id: Option<String>,
    query: &str,
    from: &str,
    to: &str,
    push: bool,
) -> Result<()> {
    if from.is_empty() {
        bail!("--replace FROM must not be empty");
    }
    let document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, "");
    let selector = BytecodeInstanceSelectorArgs::by_settings_id(scope_settings_id);
    let scope = resolve_bytecode_selector_or_service_root(
        &document,
        &service,
        &selector,
        "No matching instance",
    )?;
    let (segments, _) = build_editor_instance_path_parts(&document, &service);
    let path_of = |index: usize| {
        segments
            .get(index)
            .cloned()
            .flatten()
            .unwrap_or_else(|| vec![document.instances[index].name.clone()])
            .join(".")
    };
    let matched = where_matches(&document, &service, scope.index, query)?;
    let renames = matched
        .iter()
        .filter_map(|&index| {
            let instance = &document.instances[index];
            let name = instance.name.replace(from, to);
            (name != instance.name).then(|| (instance.settings_id.clone(), path_of(index), name))
        })
        .collect::<Vec<_>>();
    if renames.is_empty() {
        bail!(
            "No instance under {} matching '{query}' has '{from}' in its name",
            path_of(scope.index)
        );
    }
    if renames.len() > WHERE_LIMIT {
        bail!(
            "{} instances under {} match '{query}'; narrow the query ({WHERE_LIMIT} at most per call)",
            renames.len(),
            path_of(scope.index)
        );
    }
    let mut changed_paths = BTreeSet::new();
    for (settings_id, _, name) in &renames {
        let result = bytecode_set_property_result(BytecodeSetPropertyArgs {
            where_query: None,
            input: BytecodeFileArgs::settings_file(settings_file.clone()),
            selector: BytecodeInstanceSelectorArgs::by_settings_id(Some(settings_id.clone())),
            property: vec!["Name".to_string()],
            value_json: None,
            value_str: Some(name.clone()),
            value_num: None,
            value_bool: None,
            value_null: false,
            scope: "metadata".to_string(),
            override_packages: true,
            push: false,
            pretty: false,
        })?;
        if let Some(paths) = result.get("changedPaths").and_then(Value::as_array) {
            changed_paths.extend(paths.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    let ids = renames
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect::<Vec<_>>();
    let mut result = json!({
        "ok": true,
        "where": query,
        "scope": path_of(scope.index),
        "matched": matched.len(),
        "renamed": renames.iter().map(|(_, path, name)| json!({ "from": path, "to": name })).collect::<Vec<_>>(),
        "set": ["Name"],
        "changedPaths": changed_paths,
        "settingsFile": settings_file,
    });
    if push {
        let pairs = renames
            .iter()
            .map(|(_, path, name)| {
                (
                    path.split('.').map(str::to_string).collect::<Vec<_>>(),
                    name.clone(),
                )
            })
            .collect::<Vec<_>>();
        result["studioRenamed"] = crate::bytecode::edit::rename_in_studio(&pairs)?;
        result["push"] =
            crate::bytecode::edit::push_stored_instances(settings_file.clone(), ids.clone())?;
        let after = SettingsBytecode::read_file(&settings_file)?;
        let (segments, _) = build_editor_instance_path_parts(&after, &service);
        let indices = ids
            .iter()
            .filter_map(|id| {
                after
                    .instances
                    .iter()
                    .position(|instance| instance.settings_id == *id)
            })
            .collect::<Vec<_>>();
        let paths = indices
            .iter()
            .filter_map(|&index| segments.get(index).cloned().flatten())
            .collect::<Vec<_>>();
        let read = &paths[..paths.len().min(WHERE_READ_BACK_LIMIT)];
        if paths.len() > WHERE_READ_BACK_LIMIT {
            result["studioLimit"] = json!(WHERE_READ_BACK_LIMIT);
        }
        let names = ["Name".to_string()];
        if let Ok(studio) = crate::bytecode::edit::read_back_many_from_studio(read, &names, &[]) {
            let rows = studio.as_array().cloned().unwrap_or_default();
            let mut verified = Some(rows.len() == read.len());
            for ((row, path), &index) in rows.iter().zip(read).zip(indices.iter()) {
                match crate::bytecode::edit::read_back_verifies(
                    row,
                    path,
                    &after.instances[index],
                    &names,
                    &[],
                ) {
                    Some(true) => {}
                    Some(false) => verified = Some(false),
                    None => {
                        if verified == Some(true) {
                            verified = None;
                        }
                    }
                }
            }
            if let Some(verified) = verified {
                result["verified"] = json!(verified);
                if let Some(push) = result.get_mut("push").and_then(Value::as_object_mut) {
                    push.remove("verified");
                }
            }
            result["studio"] = studio;
        }
    }
    print_json_output(&result, false)
}

/// Properties Studio derives rather than saves become the saved property:
/// Position and Orientation fold into the CFrame of anything that has one,
/// and Lighting's ClockTime becomes TimeOfDay.
fn fold_derived_property(
    document: &mut SettingsBytecode,
    index: usize,
    property: &mut String,
    value: &mut Value,
) -> Result<()> {
    let instance = &mut document.instances[index];
    let has_cframe = crate::bytecode::edit::has_cframe(&instance.class_name);
    if has_cframe && property.eq_ignore_ascii_case("position") {
        *value = crate::bytecode::edit::cframe_from_parts(
            instance.properties.get("CFrame"),
            Some(value),
            None,
        )?;
        *property = "CFrame".to_string();
    } else if has_cframe && property.eq_ignore_ascii_case("orientation") {
        *value = crate::bytecode::edit::cframe_from_parts(
            instance.properties.get("CFrame"),
            None,
            Some(value),
        )?;
        *property = "CFrame".to_string();
    } else if instance.class_name == "Lighting" && property.eq_ignore_ascii_case("clocktime") {
        *value = crate::bytecode::edit::time_of_day_from_hours(value)?;
        *property = "TimeOfDay".to_string();
        instance.properties.remove("ClockTime");
    }
    Ok(())
}

/// The store indices under `scope_index` (every instance when the scope is
/// the service root) that match a bulk-read query.
pub(crate) fn where_matches(
    document: &SettingsBytecode,
    service: &str,
    scope_index: usize,
    query: &str,
) -> Result<Vec<usize>> {
    let groups = crate::bytecode::explorer::explorer_search_groups(query);
    if groups.is_empty() {
        bail!("--where needs a query: words match names, is:Class, tag:Name, Prop=value, Prop>0.5");
    }
    let (segments, _) = crate::editor::paths::build_editor_instance_path_parts(document, service);
    let whole_service = editor_service_root_index(document, service) == Some(scope_index);
    let under_scope = |index: usize| {
        let mut cursor = document.instances[index].parent_index;
        while let Some(parent) = cursor {
            if parent == scope_index {
                return true;
            }
            cursor = document.instances[parent].parent_index;
        }
        false
    };
    Ok((0..document.instances.len())
        .filter(|&index| index != scope_index && (whole_service || under_scope(index)))
        .filter(|&index| {
            crate::bytecode::explorer::explorer_search_instance_matches(
                document, &segments, index, &groups,
            )
        })
        .collect())
}

pub(crate) fn bytecode_set_property_result(mut args: BytecodeSetPropertyArgs) -> Result<Value> {
    let property_name = args.property.remove(0);
    let args = SetPropertyCall {
        args,
        property: property_name,
    };
    if args.property.eq_ignore_ascii_case("classname") {
        bail!("ClassName is read-only");
    }
    let mut args = args;
    let inner = &mut args.args;
    adopt_single_positional_target(&mut inner.input, None, &mut inner.selector)?;
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    let scope = parse_property_scope(&args.scope)?;
    let structural_reference_update =
        matches!(
            args.property.to_ascii_lowercase().as_str(),
            "name" | "parent"
        ) && matches!(scope, PropertyScope::Auto | PropertyScope::Metadata);
    let mut value = parse_cli_value(
        args.value_json.as_deref(),
        args.value_str.as_deref(),
        args.value_num,
        args.value_bool,
        args.value_null,
    )?;
    let mut unqualified_settings_ids = BTreeSet::new();
    unqualified_reference_ids(&value, &mut unqualified_settings_ids);
    let qualify_references = !unqualified_settings_ids.is_empty();
    let mut settings_files_to_lock = BTreeSet::from([settings_file.clone()]);
    if (structural_reference_update || qualify_references)
        && let Some(src_root) = crate::project::storage::source_directory(&settings_file).parent()
    {
        for service_dir in crate::project::storage::service_directories(src_root)? {
            let path = service_settings_path(&service_dir);
            if path.is_file() {
                settings_files_to_lock.insert(path);
            }
        }
    }
    let _locks = settings_files_to_lock
        .iter()
        .map(|path| lock_existing_service_store(path))
        .collect::<Result<Vec<_>>>()?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    if qualify_references {
        let mut documents = BTreeMap::new();
        for path in &settings_files_to_lock {
            let service_name = crate::project::storage::store_service_name(path)
                .context("Service settings path has no service name")?;
            let candidate = if *path == settings_file {
                document.clone()
            } else {
                SettingsBytecode::read_file(path)?
            };
            documents.insert(service_name, candidate);
        }
        qualify_reference_targets(&mut value, &documents, &unqualified_settings_ids)?;
    }
    let resolved = resolve_bytecode_selector_or_service_root(
        &document,
        &service,
        &args.selector,
        "No matching instance",
    )?;
    let index = resolved.index;
    fold_derived_property(&mut document, index, &mut args.property, &mut value)?;
    reject_owned_instance_edit(
        &settings_file,
        &document,
        &service,
        index,
        args.override_packages,
    )?;
    let (stored_property, requested_differs) =
        stored_property_edit(&mut document, index, &args.property, &mut value, scope)?;
    let property = stored_property.as_str();
    let moved_indices = structural_reference_update.then(|| {
        let children = settings_children_by_parent(&document);
        let mut subtree = Vec::new();
        collect_settings_subtree_preorder(&children, index, &mut subtree);
        subtree.into_iter().collect::<HashSet<_>>()
    });
    if matches!(scope, PropertyScope::Auto | PropertyScope::Metadata)
        && matches!(args.property.as_str(), "Name" | "ClassName" | "Parent")
        && is_protected_engine_container(&document, index)
    {
        bail!("{} metadata is read-only", document.instances[index].name);
    }
    let service_directory = crate::project::storage::source_directory(&settings_file);
    let service_dir = service_directory.as_path();
    if args.property.eq_ignore_ascii_case("source")
        && matches!(scope, PropertyScope::Auto | PropertyScope::Property)
    {
        let source = value.as_str().context("Source must be a string")?;
        let (source_path, changed) =
            write_bytecode_source_file(&settings_file, &document, &service, index, source)?;
        let changed_paths = changed
            .then(|| source_path.clone())
            .into_iter()
            .collect::<Vec<_>>();
        return Ok(json!({
            "ok": true,
            "settingsFile": settings_file,
            "property": args.property,
            "sourcePath": source_path,
            "changedPaths": changed_paths,
        }));
    }
    let affects_source_path = property_affects_source_path(scope, property);
    let before_document = affects_source_path.then(|| document.clone());
    let source_paths_before = before_document
        .as_ref()
        .map(|document| build_editor_source_paths_by_index(document, &service, service_dir));
    instance_api::set_instance_property(&mut document, index, property, value, scope)?;
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    if let (Some(before_document), Some(source_paths_before)) =
        (before_document.as_ref(), source_paths_before.as_ref())
    {
        collect_source_path_updates(
            before_document,
            source_paths_before,
            &document,
            &service,
            service_dir,
            &mut writes,
            &mut removals,
        )?;
    }
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    if let Some(moved_indices) = moved_indices.as_ref() {
        let mut reference_documents = BTreeMap::new();
        let mut reference_files = BTreeMap::new();
        for path in &settings_files_to_lock {
            let service_name = crate::project::storage::store_service_name(path)
                .context("Service settings path has no service name")?;
            let value = if *path == settings_file {
                document.clone()
            } else {
                SettingsBytecode::read_file(path)?
            };
            reference_files.insert(service_name.clone(), path.clone());
            reference_documents.insert(service_name, value);
        }
        let changed_services = canonicalize_settings_references_for_move(
            &mut reference_documents,
            &service,
            moved_indices,
        );
        for changed_service in changed_services {
            let path = &reference_files[&changed_service];
            writes.insert(
                path.clone(),
                encode_settings_bytecode(&reference_documents[&changed_service])?,
            );
        }
    }
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    let mut result = json!({
        "ok": true,
        "settingsFile": settings_file,
        "property": property,
        "changedPaths": changed_paths,
    });
    if requested_differs {
        result["requestedProperty"] = json!(args.property);
    }
    if structural_reference_update {
        let instance = &document.instances[index];
        let (path_segments, path_ordinals) = build_editor_instance_path_parts(&document, &service);
        result["settingsId"] = json!(instance.settings_id);
        result["name"] = json!(instance.name);
        result["className"] = json!(instance.class_name);
        result["pathSegments"] = json!(path_segments.get(index).and_then(Clone::clone));
        result["pathOrdinals"] = json!(path_ordinals.get(index).and_then(Clone::clone));
    }
    Ok(result)
}

/// One property of a `bs` call: the parsed arguments plus the property name
/// that call sets, so the setter reads `args.property` as before.
struct SetPropertyCall {
    args: BytecodeSetPropertyArgs,
    property: String,
}

impl std::ops::Deref for SetPropertyCall {
    type Target = BytecodeSetPropertyArgs;

    fn deref(&self) -> &BytecodeSetPropertyArgs {
        &self.args
    }
}

impl std::ops::DerefMut for SetPropertyCall {
    fn deref_mut(&mut self) -> &mut BytecodeSetPropertyArgs {
        &mut self.args
    }
}

pub(super) fn bytecode_apply_property_batch(args: BytecodeApplyPropertyBatchArgs) -> Result<()> {
    let loaded = load_structural_project(None, &args.project_root)?;
    let link_enforcement = build_loaded_project_link_enforcement(&loaded, args.override_packages)?;
    let input = fs::read_to_string(&args.input)
        .with_context(|| format!("Failed to read {}", args.input.display()))?;
    let entries = serde_json::from_str::<Vec<BytecodePropertyBatchEntry>>(&input)
        .with_context(|| format!("Invalid property batch {}", args.input.display()))?;
    if entries.is_empty() {
        return print_json_output(
            &json!({"ok": true, "applied": 0, "filtered": 0, "changedPaths": [], "sourcePaths": []}),
            false,
        );
    }
    let filter_direction = match args.direction.trim().to_ascii_lowercase().as_str() {
        "studio-to-files" | "studio" => config::FilterDirection::StudioToFiles,
        "files-to-studio" | "files" => config::FilterDirection::FilesToStudio,
        value => bail!(
            "Unknown property batch direction '{value}'; expected studio-to-files or files-to-studio"
        ),
    };

    let mut services = entries
        .iter()
        .map(|entry| entry.service.trim().to_string())
        .collect::<BTreeSet<_>>();
    if services.remove("") {
        bail!("Property batch service cannot be empty");
    }
    for service in &services {
        validate_filesystem_instance_name(service, "service")?;
    }
    let mut result = if config::project_requires_temporary_stage(&loaded)? {
        let service_list = services.iter().cloned().collect::<Vec<_>>();
        let stage =
            ExportProjectStage::create(&loaded.root, &loaded.project.source_root, &service_list)?;
        let mut result = apply_property_batch_to_root(
            &loaded,
            &stage.import_project_root,
            entries,
            &services,
            filter_direction,
            &link_enforcement,
        )?;
        if let (Some(stage_loaded), Some(stage_projection)) =
            (stage.loaded.as_ref(), stage.projection.as_ref())
        {
            for (_, source_path) in &mut result.source_paths {
                let relative = source_path
                    .strip_prefix(stage_projection.root())
                    .with_context(|| {
                        format!(
                            "Projected source path {} is outside {}",
                            source_path.display(),
                            stage_projection.root().display()
                        )
                    })?;
                let canonical = config::staged_path_to_project_source(stage_loaded, relative)?
                    .context("Could not resolve the projected source owner")?;
                let canonical_relative = canonical.strip_prefix(&stage.project_root)?;
                *source_path = loaded.root.join(canonical_relative);
            }
        }
        stage.finish_projection(true)?;
        let operations = stage.preview_operations(&loaded.root)?;
        result.changed_paths = operations
            .iter()
            .filter_map(|operation| operation.get("path").and_then(Value::as_str))
            .map(|path| loaded.root.join(path))
            .collect();
        stage.publish(&loaded.root, false)?;
        result
    } else {
        let projection = config::stage_project(&loaded)?;
        apply_property_batch_to_root(
            &loaded,
            projection.root(),
            entries,
            &services,
            filter_direction,
            &link_enforcement,
        )?
    };
    result.changed_paths.sort();
    result.changed_paths.dedup();
    result.source_paths.sort_by_key(|entry| entry.0);
    print_json_output(
        &json!({
            "ok": true,
            "applied": result.applied,
            "filtered": result.filtered,
            "changedPaths": result.changed_paths,
            "sourcePaths": result.source_paths.into_iter().map(|(entry_index, path)| {
                json!({
                    "entryIndex": entry_index,
                    "path": path,
                })
            }).collect::<Vec<_>>(),
        }),
        false,
    )
}

fn resolve_property_batch_entries(
    entries: Vec<BytecodePropertyBatchEntry>,
    documents: &BTreeMap<String, BytecodePropertyBatchDocument>,
) -> Result<Vec<ResolvedBytecodePropertyBatchEntry>> {
    let mut resolved = Vec::with_capacity(entries.len());
    for (entry_index, entry) in entries.into_iter().enumerate() {
        let service = entry.service.trim().to_string();
        let state = documents
            .get(&service)
            .with_context(|| format!("Missing service settings for {service}"))?;
        if entry.property.eq_ignore_ascii_case("classname") {
            bail!("ClassName is read-only");
        }
        let scope = parse_property_scope(&entry.scope)?;
        let settings_id = entry
            .settings_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let instance_index = if let Some(settings_id) = settings_id {
            state
                .settings_id_indices
                .get(settings_id)
                .copied()
                .with_context(|| format!("No matching instance id: {settings_id}"))?
        } else {
            if entry.path_segments.first().map(String::as_str) != Some(service.as_str()) {
                bail!("Property batch path must start with service {service}");
            }
            document_instance_index_by_path_unique(
                &state.document,
                &entry.path_segments,
                &entry.path_ordinals,
            )?
        };
        let instance = &state.document.instances[instance_index];
        reject_package_link_instance_mutation(&state.document, instance_index, "edited")?;
        if !entry.class_name.trim().is_empty() && instance.class_name != entry.class_name {
            bail!(
                "Class mismatch for {}: expected {}, found {}",
                instance.settings_id,
                entry.class_name,
                instance.class_name
            );
        }
        if entry.property == "Source" && !is_lua_source_class(&instance.class_name) {
            bail!("Source is only valid for Lua source containers");
        }
        if matches!(scope, PropertyScope::Auto | PropertyScope::Metadata)
            && matches!(entry.property.as_str(), "ClassName" | "Parent")
            && is_protected_engine_container(&state.document, instance_index)
        {
            bail!("{} metadata is read-only", instance.name);
        }
        resolved.push(ResolvedBytecodePropertyBatchEntry {
            entry_index,
            service,
            instance_index,
            scope,
            property: entry.property,
            value: entry.value,
        });
    }
    Ok(resolved)
}

fn apply_non_source_batch_entries(
    documents: &mut BTreeMap<String, SettingsBytecode>,
    entries: &[ResolvedBytecodePropertyBatchEntry],
) -> Result<()> {
    for entry in entries {
        if entry.property == "Source" {
            continue;
        }
        let document = documents
            .get_mut(&entry.service)
            .context("Property batch service disappeared")?;
        instance_api::set_instance_property(
            document,
            entry.instance_index,
            &entry.property,
            entry.value.clone(),
            entry.scope,
        )?;
    }
    Ok(())
}

fn validate_property_batch_entries(
    documents: &BTreeMap<String, BytecodePropertyBatchDocument>,
    entries: &[ResolvedBytecodePropertyBatchEntry],
) -> Result<()> {
    for entry in entries {
        if entry.property == "Source" && !entry.value.is_string() {
            bail!("Source must be a string");
        }
    }
    let mut validation_documents = documents
        .iter()
        .map(|(service, state)| (service.clone(), state.document.clone()))
        .collect();
    apply_non_source_batch_entries(&mut validation_documents, entries)
}

fn evaluate_property_batch_filters(
    loaded: &config::LoadedProject,
    filter_direction: config::FilterDirection,
    document: &SettingsBytecode,
    service: &str,
    instance_index: usize,
    entries: &[ResolvedBytecodePropertyBatchEntry],
) -> Result<(bool, Vec<bool>)> {
    let instance = document
        .instances
        .get(instance_index)
        .context("Property batch candidate instance disappeared")?;
    let paths = build_editor_instance_paths(document, service);
    let path_segments = &paths
        .get(instance_index)
        .and_then(Option::as_ref)
        .context("Could not resolve final instance path")?
        .path_segments;
    let path = config::filter_path_segments(path_segments);
    let fields = config::filter_candidate_fields(&instance.properties, &instance.attributes);
    let candidate = fields.candidate(
        &instance.settings_id,
        &path,
        &instance.name,
        &instance.class_name,
    );
    let instance_allowed =
        config::filter_allows_instance(&loaded.project.filters, filter_direction, &candidate)?;
    let field_allowed = entries
        .iter()
        .map(|entry| {
            if entry.scope == PropertyScope::Attribute {
                config::filter_allows_attribute(
                    &loaded.project.filters,
                    filter_direction,
                    &candidate,
                    &entry.property,
                )
            } else {
                config::filter_allows_property(
                    &loaded.project.filters,
                    filter_direction,
                    &candidate,
                    &entry.property,
                )
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((instance_allowed, field_allowed))
}

fn apply_permitted_entries(
    document: &mut SettingsBytecode,
    entries: &[ResolvedBytecodePropertyBatchEntry],
    permitted: &[bool],
) -> Result<()> {
    for (entry, allowed) in entries.iter().zip(permitted) {
        if *allowed && entry.property != "Source" {
            instance_api::set_instance_property(
                document,
                entry.instance_index,
                &entry.property,
                entry.value.clone(),
                entry.scope,
            )?;
        }
    }
    Ok(())
}

fn filter_property_batch_entries(
    loaded: &config::LoadedProject,
    documents: &BTreeMap<String, BytecodePropertyBatchDocument>,
    resolved: Vec<ResolvedBytecodePropertyBatchEntry>,
    filter_direction: config::FilterDirection,
) -> Result<(Vec<ResolvedBytecodePropertyBatchEntry>, usize)> {
    if loaded.project.filters.is_empty() {
        return Ok((resolved, 0));
    }
    let mut entries_by_instance = BTreeMap::new();
    for entry in resolved {
        entries_by_instance
            .entry((entry.service.clone(), entry.instance_index))
            .or_insert_with(Vec::new)
            .push(entry);
    }
    let mut allowed_entries = Vec::new();
    let mut filtered = 0;
    for entries in entries_by_instance.into_values() {
        let first = entries
            .first()
            .context("Property batch instance group is empty")?;
        let original = &documents
            .get(&first.service)
            .context("Property batch source service disappeared")?
            .document;
        let (original_allowed, _) = evaluate_property_batch_filters(
            loaded,
            filter_direction,
            original,
            &first.service,
            first.instance_index,
            &entries,
        )?;
        if !original_allowed {
            filtered += entries.len();
            continue;
        }
        let mut permitted = vec![true; entries.len()];
        let mut seen = HashSet::new();
        for _ in 0..=entries.len() + 1 {
            if !seen.insert(permitted.clone()) {
                for index in 0..permitted.len() {
                    permitted[index] = seen.iter().all(|mask| mask[index]);
                }
                break;
            }
            let mut candidate = original.clone();
            apply_permitted_entries(&mut candidate, &entries, &permitted)?;
            let (_, next) = evaluate_property_batch_filters(
                loaded,
                filter_direction,
                &candidate,
                &first.service,
                first.instance_index,
                &entries,
            )?;
            if next == permitted {
                break;
            }
            permitted = next;
        }
        let mut candidate = original.clone();
        apply_permitted_entries(&mut candidate, &entries, &permitted)?;
        let (instance_allowed, final_fields) = evaluate_property_batch_filters(
            loaded,
            filter_direction,
            &candidate,
            &first.service,
            first.instance_index,
            &entries,
        )?;
        if instance_allowed {
            for ((entry, allowed), field_allowed) in
                entries.into_iter().zip(permitted).zip(final_fields)
            {
                if allowed && field_allowed {
                    allowed_entries.push(entry);
                } else {
                    filtered += 1;
                }
            }
        } else {
            filtered += entries.len();
        }
    }
    allowed_entries.sort_by_key(|entry| entry.entry_index);
    Ok((allowed_entries, filtered))
}

fn reject_read_only_package_changes(
    documents: &BTreeMap<String, BytecodePropertyBatchDocument>,
    entries: &[ResolvedBytecodePropertyBatchEntry],
    link_enforcement: &LinkEnforcement,
) -> Result<()> {
    if link_enforcement.read_only_packages.is_empty() {
        return Ok(());
    }
    let mut final_documents = documents
        .iter()
        .map(|(service, state)| (service.clone(), state.document.clone()))
        .collect();
    apply_non_source_batch_entries(&mut final_documents, entries)?;
    let original_paths = documents
        .iter()
        .map(|(service, state)| {
            (
                service.clone(),
                build_editor_instance_paths(&state.document, service),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let final_paths = final_documents
        .iter()
        .map(|(service, document)| {
            (
                service.clone(),
                build_editor_instance_paths(document, service),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut checked = HashSet::new();
    for entry in entries {
        if !checked.insert((entry.service.clone(), entry.instance_index)) {
            continue;
        }
        for paths in [&original_paths, &final_paths] {
            let path = paths
                .get(&entry.service)
                .and_then(|paths| paths.get(entry.instance_index))
                .and_then(Option::as_ref)
                .context("Could not resolve package-protected instance path")?;
            link_enforcement.reject_read_only_package_path(
                &entry.service,
                &path.path_segments,
                &path.path_ordinals,
            )?;
        }
    }
    Ok(())
}

fn apply_property_batch_entries(
    documents: &mut BTreeMap<String, BytecodePropertyBatchDocument>,
    entries: &[ResolvedBytecodePropertyBatchEntry],
) -> Result<()> {
    for entry in entries {
        if entry.property == "Source" {
            continue;
        }
        let state = documents
            .get_mut(&entry.service)
            .context("Property batch service disappeared")?;
        if property_affects_source_path(entry.scope, &entry.property)
            && state.source_paths_before.is_none()
        {
            let service_directory = crate::project::storage::source_directory(&state.settings_file);
            let service_dir = service_directory.as_path();
            state.source_paths_before = Some(build_editor_source_paths_by_index(
                &state.document,
                &entry.service,
                service_dir,
            ));
        }
        instance_api::set_instance_property(
            &mut state.document,
            entry.instance_index,
            &entry.property,
            entry.value.clone(),
            entry.scope,
        )?;
        state.settings_changed = true;
    }
    Ok(())
}

fn canonicalize_property_batch_references(
    documents: &mut BTreeMap<String, BytecodePropertyBatchDocument>,
    entries: &[ResolvedBytecodePropertyBatchEntry],
) -> Result<()> {
    let mut roots_by_service = HashMap::<String, HashSet<usize>>::new();
    for entry in entries {
        if matches!(
            entry.property.to_ascii_lowercase().as_str(),
            "name" | "parent"
        ) && matches!(entry.scope, PropertyScope::Auto | PropertyScope::Metadata)
        {
            roots_by_service
                .entry(entry.service.clone())
                .or_default()
                .insert(entry.instance_index);
        }
    }
    if roots_by_service.is_empty() {
        return Ok(());
    }
    let mut moved_indices = HashMap::<String, HashSet<usize>>::new();
    for (service, roots) in roots_by_service {
        let document = &documents
            .get(&service)
            .context("Property batch service disappeared")?
            .document;
        let children = settings_children_by_parent(document);
        let indices = moved_indices.entry(service).or_default();
        for root in roots {
            let mut subtree = Vec::new();
            collect_settings_subtree_preorder(&children, root, &mut subtree);
            indices.extend(subtree);
        }
    }
    let mut reference_documents = documents
        .iter()
        .map(|(service, state)| (service.clone(), state.document.clone()))
        .collect::<BTreeMap<_, _>>();
    for service in
        canonicalize_settings_references_for_moves(&mut reference_documents, &moved_indices)
    {
        let state = documents
            .get_mut(&service)
            .context("Reference owner service disappeared")?;
        state.document = reference_documents
            .remove(&service)
            .context("Canonical reference document disappeared")?;
        state.settings_changed = true;
    }
    Ok(())
}

type PropertyBatchMutations = (
    BTreeMap<PathBuf, Vec<u8>>,
    Vec<PathBuf>,
    Vec<(usize, PathBuf)>,
);

fn property_batch_file_mutations(
    documents: &BTreeMap<String, BytecodePropertyBatchDocument>,
    entries: &[ResolvedBytecodePropertyBatchEntry],
) -> Result<PropertyBatchMutations> {
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let mut source_paths = Vec::new();
    for (service, state) in documents {
        let source_entries = entries
            .iter()
            .filter(|entry| entry.service == *service && entry.property == "Source")
            .collect::<Vec<_>>();
        if state.source_paths_before.is_some() || !source_entries.is_empty() {
            let service_directory = crate::project::storage::source_directory(&state.settings_file);
            let service_dir = service_directory.as_path();
            let mut source_paths_after =
                build_editor_source_paths_by_index(&state.document, service, service_dir);
            if let Some(source_paths_before) = state.source_paths_before.as_ref() {
                preserve_source_extensions_by_index(source_paths_before, &mut source_paths_after);
                collect_source_path_moves(
                    source_paths_before,
                    &source_paths_after,
                    &mut writes,
                    &mut removals,
                )?;
            }
            for entry in source_entries {
                let source_path = source_paths_after
                    .get(entry.instance_index)
                    .and_then(Option::as_ref)
                    .context("Could not resolve source file path")?;
                let source = entry.value.as_str().context("Source must be a string")?;
                writes.insert(source_path.clone(), source.as_bytes().to_vec());
                source_paths.push((entry.entry_index, source_path.clone()));
            }
        }
        if state.settings_changed {
            writes.insert(
                state.settings_file.clone(),
                encode_settings_bytecode(&state.document)?,
            );
        }
    }
    Ok((writes, removals, source_paths))
}

fn apply_property_batch_to_root(
    loaded: &config::LoadedProject,
    root: &Path,
    entries: Vec<BytecodePropertyBatchEntry>,
    services: &BTreeSet<String>,
    filter_direction: config::FilterDirection,
    link_enforcement: &LinkEnforcement,
) -> Result<BytecodePropertyBatchResult> {
    let structural_reference_update = entries.iter().any(|entry| {
        matches!(
            entry.property.to_ascii_lowercase().as_str(),
            "name" | "parent"
        ) && matches!(
            entry.scope.trim().to_ascii_lowercase().as_str(),
            "auto" | "metadata" | "meta"
        )
    });
    let mut loaded_services = services.clone();
    if structural_reference_update {
        for service in crate::project::storage::service_directories(root)? {
            if service_settings_path(&service).is_file() {
                loaded_services.insert(service.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let settings_files = loaded_services
        .iter()
        .map(|service| (service.clone(), service_settings_path(&root.join(service))))
        .collect::<BTreeMap<_, _>>();
    let _locks = settings_files
        .values()
        .map(|settings_file| lock_existing_service_store(settings_file))
        .collect::<Result<Vec<_>>>()?;
    let mut documents = BTreeMap::new();
    for (service, settings_file) in &settings_files {
        let document = SettingsBytecode::read_file(settings_file)?;
        let settings_id_indices = document
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| (instance.settings_id.clone(), index))
            .collect();
        documents.insert(
            service.clone(),
            BytecodePropertyBatchDocument {
                settings_file: settings_file.clone(),
                document,
                settings_id_indices,
                source_paths_before: None,
                settings_changed: false,
            },
        );
    }

    let resolved = resolve_property_batch_entries(entries, &documents)?;
    validate_property_batch_entries(&documents, &resolved)?;
    let (allowed_entries, filtered) =
        filter_property_batch_entries(loaded, &documents, resolved, filter_direction)?;
    reject_read_only_package_changes(&documents, &allowed_entries, link_enforcement)?;
    apply_property_batch_entries(&mut documents, &allowed_entries)?;
    canonicalize_property_batch_references(&mut documents, &allowed_entries)?;
    let (writes, removals, source_paths) =
        property_batch_file_mutations(&documents, &allowed_entries)?;
    apply_file_mutations(&writes, &removals)?;
    let changed_paths = file_mutation_paths(&writes, &removals);
    Ok(BytecodePropertyBatchResult {
        applied: allowed_entries.len(),
        filtered,
        changed_paths,
        source_paths,
    })
}

pub(super) fn bytecode_set_source(mut args: BytecodeSetSourceArgs) -> Result<()> {
    adopt_single_positional_target(&mut args.input, args.service.as_deref(), &mut args.selector)?;
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        args.service.as_deref(),
    )?;
    let project_service = project_service_input(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        args.service.as_deref(),
    );
    let _lock = settings_file
        .exists()
        .then(|| lock_existing_service_store(&settings_file))
        .transpose()?;
    let direct = read_bytecode_document_if_present(&settings_file, &service_hint)?;
    let use_project = project_service.is_some()
        && direct.as_ref().is_none_or(|(document, service)| {
            resolve_bytecode_selector(document, service, &args.selector, "No matching instance")
                .is_err()
        });
    let (document, inferred_service, source_paths, path_segments) = if use_project {
        let service = project_service.context("Project service is missing")?;
        let context = high_level_context(Path::new("."), Path::new("src"), service)?;
        (
            context.document,
            context.service,
            Some(context.source_paths_by_index),
            Some(context.path_segments_by_index),
        )
    } else {
        let (document, service) =
            direct.ok_or_else(|| missing_service_store_error(&settings_file))?;
        (document, service, None, None)
    };
    let resolved = resolve_bytecode_selector(
        &document,
        &inferred_service,
        &args.selector,
        "No matching instance",
    )?;
    let index = resolved.index;

    let source = match &args.source_file {
        Some(path) => {
            if args.value_json.is_some() || args.value_str.is_some() {
                bail!("Provide either --source-file or --str/--value-json, not both");
            }
            fs::read_to_string(path)
                .with_context(|| format!("Failed to read source file {}", path.display()))?
        }
        None => parse_cli_source_text(args.value_json.as_deref(), args.value_str.as_deref())?,
    };
    let service = if inferred_service.trim().is_empty() {
        resolved
            .path_segments
            .as_ref()
            .and_then(|segments| segments.first().cloned())
            .ok_or_else(|| anyhow::anyhow!("Could not infer service for source path"))?
    } else {
        inferred_service
    };
    let (source_path, changed) = if let Some(source_paths) = source_paths {
        let path_segments = path_segments
            .as_ref()
            .and_then(|paths| paths.get(index))
            .and_then(Option::as_ref)
            .context("Could not resolve projected instance path")?;
        if let Some(loaded) = config::try_load_project(None, Some(Path::new(".")))? {
            config::resolve_project_write_segments(&loaded, path_segments)?;
        }
        let source_path = source_paths
            .get(index)
            .and_then(Option::as_ref)
            .context("Could not resolve projected source file path")?;
        let changed = file_contents_differ(source_path, source.as_bytes())?;
        write_utf8_file(source_path, &source)?;
        (source_path.clone(), changed)
    } else {
        write_bytecode_source_file(&settings_file, &document, &service, index, &source)?
    };
    let instance = &document.instances[index];
    let mut result = Map::from_iter([
        ("ok".to_string(), Value::Bool(true)),
        ("sourcePath".to_string(), json!(&source_path)),
        ("service".to_string(), Value::String(service)),
        (
            "settingsId".to_string(),
            Value::String(instance.settings_id.clone()),
        ),
        (
            "changedPaths".to_string(),
            Value::Array(changed.then(|| json!(&source_path)).into_iter().collect()),
        ),
    ]);
    if settings_file.exists() {
        result.insert("settingsFile".to_string(), json!(settings_file));
    }
    print_json_output(&Value::Object(result), args.pretty)
}

fn read_bytecode_document_if_present(
    settings_file: &Path,
    service_hint: &str,
) -> Result<Option<(SettingsBytecode, String)>> {
    if !settings_file.exists() {
        return Ok(None);
    }
    let document = SettingsBytecode::read_file(settings_file)
        .with_context(|| format!("Failed to read {}", settings_file.display()))?;
    let service = bytecode_service_name(&document, settings_file, service_hint);
    Ok(Some((document, service)))
}

fn project_service_input<'a>(
    settings_file: Option<&Path>,
    service_or_file: Option<&'a str>,
    explicit_service: Option<&'a str>,
) -> Option<&'a str> {
    if settings_file.is_some()
        || service_or_file.is_some_and(bytecode_input_looks_like_settings_file)
    {
        return None;
    }
    explicit_service
        .or(service_or_file)
        .map(str::trim)
        .filter(|service| !service.is_empty())
}

fn write_bytecode_source_file(
    settings_file: &Path,
    document: &SettingsBytecode,
    service: &str,
    index: usize,
    source: &str,
) -> Result<(PathBuf, bool)> {
    let instance = document
        .instances
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("Invalid instance index {index}"))?;
    if script_file_names(&instance.class_name).is_none() {
        bail!("{} is not a Lua source container", instance.class_name);
    }
    let service_dir = crate::project::storage::source_directory(settings_file);
    let source_path = build_editor_source_paths_by_index(document, service, &service_dir)
        .get(index)
        .and_then(Clone::clone)
        .ok_or_else(|| {
            anyhow::anyhow!("Could not resolve source file path for {}", instance.name)
        })?;
    let changed = file_contents_differ(&source_path, source.as_bytes())?;
    write_utf8_file(&source_path, source)?;
    Ok((source_path, changed))
}

fn file_contents_differ(path: &Path, expected: &[u8]) -> Result<bool> {
    match fs::read(path) {
        Ok(current) => Ok(current != expected),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

pub(crate) fn check_new_source_file(path: &Path, expected: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(current) if current != expected => bail!(
            "{} already exists with different contents; move or rename it before creating this script",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn collect_source_path_moves(
    before: &[Option<PathBuf>],
    after: &[Option<PathBuf>],
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut Vec<PathBuf>,
) -> Result<()> {
    collect_source_path_move_pairs(
        before
            .iter()
            .zip(after)
            .filter_map(|(from, to)| Some((from.as_deref()?, to.as_deref()?))),
        writes,
        removals,
    )
}

fn collect_source_path_move_pairs<'a>(
    pairs: impl IntoIterator<Item = (&'a Path, &'a Path)>,
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut Vec<PathBuf>,
) -> Result<()> {
    for (from, to) in pairs {
        if exact_path_key(from) == exact_path_key(to) || !from.is_file() {
            continue;
        }
        writes.insert(
            to.to_path_buf(),
            fs::read(from).with_context(|| format!("Failed to read {}", from.display()))?,
        );
        removals.push(from.to_path_buf());
    }
    removals.retain(|path| {
        !writes
            .keys()
            .any(|write| exact_path_key(write) == exact_path_key(path))
    });
    removals.sort_by_key(|path| exact_path_key(path));
    removals.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    Ok(())
}

pub(super) fn preserve_source_path_extension(from: &Path, to: &mut PathBuf) {
    let Some(extension) = from.extension().and_then(|value| value.to_str()) else {
        return;
    };
    if extension.eq_ignore_ascii_case("lua") || extension.eq_ignore_ascii_case("luau") {
        to.set_extension(extension);
    }
}

fn preserve_source_extensions_by_index(before: &[Option<PathBuf>], after: &mut [Option<PathBuf>]) {
    for (from, to) in before.iter().zip(after) {
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        preserve_source_path_extension(from, to);
    }
}

fn source_paths_by_settings_id<'a>(
    document: &'a SettingsBytecode,
    paths: &'a [Option<PathBuf>],
) -> HashMap<&'a str, &'a Path> {
    document
        .instances
        .iter()
        .zip(paths)
        .filter_map(|(instance, path)| {
            path.as_deref()
                .map(|path| (instance.settings_id.as_str(), path))
        })
        .collect()
}

pub(super) fn preserve_source_extensions(
    before_document: &SettingsBytecode,
    before: &[Option<PathBuf>],
    after_document: &SettingsBytecode,
    after: &mut [Option<PathBuf>],
) {
    let before_by_id = source_paths_by_settings_id(before_document, before);
    for (instance, path) in after_document.instances.iter().zip(after) {
        let (Some(from), Some(to)) = (
            before_by_id.get(instance.settings_id.as_str()).copied(),
            path,
        ) else {
            continue;
        };
        preserve_source_path_extension(from, to);
    }
}

pub(super) fn collect_source_path_updates(
    before_document: &SettingsBytecode,
    before: &[Option<PathBuf>],
    after_document: &SettingsBytecode,
    service: &str,
    service_dir: &Path,
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut Vec<PathBuf>,
) -> Result<Vec<Option<PathBuf>>> {
    let mut after = build_editor_source_paths_by_index(after_document, service, service_dir);
    preserve_source_extensions(before_document, before, after_document, &mut after);
    collect_source_path_moves_by_settings_id(
        before_document,
        before,
        after_document,
        &after,
        writes,
        removals,
    )?;
    Ok(after)
}

pub(super) fn collect_source_path_moves_by_settings_id(
    before_document: &SettingsBytecode,
    before: &[Option<PathBuf>],
    after_document: &SettingsBytecode,
    after: &[Option<PathBuf>],
    writes: &mut BTreeMap<PathBuf, Vec<u8>>,
    removals: &mut Vec<PathBuf>,
) -> Result<()> {
    let before_by_id = source_paths_by_settings_id(before_document, before);
    collect_source_path_move_pairs(
        after_document
            .instances
            .iter()
            .zip(after)
            .filter_map(|(instance, to)| {
                Some((
                    before_by_id.get(instance.settings_id.as_str()).copied()?,
                    to.as_deref()?,
                ))
            }),
        writes,
        removals,
    )
}

pub(super) fn apply_file_mutations(
    writes: &BTreeMap<PathBuf, Vec<u8>>,
    removals: &[PathBuf],
) -> Result<()> {
    apply_file_mutations_with_permissions(writes, removals, &BTreeMap::new())
}

pub(super) fn file_mutation_paths(
    writes: &BTreeMap<PathBuf, Vec<u8>>,
    removals: &[PathBuf],
) -> Vec<PathBuf> {
    let mut changed = BTreeSet::new();
    for (path, bytes) in writes {
        let case_move = removals.iter().any(|from| {
            exact_path_key(from) != exact_path_key(path)
                && case_folded_path_key(from) == case_folded_path_key(path)
        });
        if case_move || fs::read(path).map_or(true, |current| current != *bytes) {
            changed.insert(path.clone());
        }
    }
    for path in removals {
        if path.is_file() {
            changed.insert(path.clone());
        }
    }
    changed.into_iter().collect()
}

pub(super) fn apply_file_mutations_with_permissions(
    writes: &BTreeMap<PathBuf, Vec<u8>>,
    removals: &[PathBuf],
    permissions: &BTreeMap<PathBuf, bool>,
) -> Result<()> {
    let mut case_moves = Vec::new();
    for from in removals {
        let Some(to) = writes.keys().find(|to| {
            exact_path_key(from) != exact_path_key(to)
                && case_folded_path_key(from) == case_folded_path_key(to)
        }) else {
            continue;
        };
        let parent = from
            .parent()
            .context("Source path has no parent directory")?;
        let mut attempt = 0u32;
        let temporary = loop {
            let candidate = parent.join(format!(
                ".renium-case-move-{}-{}-{attempt}",
                std::process::id(),
                current_millis()
            ));
            if !candidate.exists() {
                break candidate;
            }
            attempt = attempt.saturating_add(1);
        };
        case_moves.push((
            from.clone(),
            to.clone(),
            temporary,
            fs::read(from).with_context(|| format!("Failed to read {}", from.display()))?,
        ));
    }
    let case_paths = case_moves
        .iter()
        .flat_map(|(from, to, _, _)| [exact_path_key(from), exact_path_key(to)])
        .collect::<BTreeSet<_>>();
    let mut paths = writes.keys().cloned().collect::<Vec<_>>();
    paths.extend(removals.iter().cloned());
    paths.extend(permissions.keys().cloned());
    paths.retain(|path| !case_paths.contains(&exact_path_key(path)));
    paths.sort_by_key(|path| exact_path_key(path));
    paths.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    let originals = paths
        .iter()
        .map(|path| read_file_if_present(path))
        .collect::<io::Result<Vec<_>>>()?;
    let mut permission_paths = paths.clone();
    for (from, to, _, _) in &case_moves {
        permission_paths.push(from.clone());
        permission_paths.push(to.clone());
    }
    permission_paths.sort_by_key(|path| exact_path_key(path));
    permission_paths.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    let original_permissions = permission_paths
        .iter()
        .map(|path| {
            fs::metadata(path)
                .ok()
                .map(|metadata| metadata.permissions())
        })
        .collect::<Vec<_>>();
    let mut created_directories = Vec::new();
    let apply = (|| -> Result<()> {
        for (from, _, temporary, _) in &case_moves {
            fs::rename(from, temporary).with_context(|| {
                format!("Failed to stage case-only source move {}", from.display())
            })?;
        }
        for (path, bytes) in writes {
            if let Some(parent) = path.parent() {
                let mut missing = Vec::new();
                let mut current = parent;
                while !current.exists() {
                    missing.push(current.to_path_buf());
                    current = current
                        .parent()
                        .context("Source path has no existing ancestor")?;
                }
                fs::create_dir_all(parent)?;
                missing.reverse();
                created_directories.extend(missing);
            }
            write_bytes_if_changed(path, bytes)?;
        }
        for path in removals {
            if case_paths.contains(&exact_path_key(path)) {
                continue;
            }
            if path.is_file() {
                set_path_readonly(path, false)?;
                fs::remove_file(path)
                    .with_context(|| format!("Failed to remove {}", path.display()))?;
            }
        }
        for (_, _, temporary, _) in &case_moves {
            fs::remove_file(temporary).with_context(|| {
                format!(
                    "Failed to finish case-only source move {}",
                    temporary.display()
                )
            })?;
        }
        for (path, readonly) in permissions {
            if !path.is_file() {
                bail!(
                    "Linked mirror does not exist while setting permissions: {}",
                    path.display()
                );
            }
            set_path_readonly(path, *readonly)?;
        }
        Ok(())
    })();
    let Err(error) = apply else {
        return Ok(());
    };
    let mut rollback_errors = Vec::new();
    for (path, original) in paths.iter().zip(originals).rev() {
        let restore = if let Some(original) = original {
            if let Some(parent) = path.parent()
                && let Err(create_error) = fs::create_dir_all(parent)
            {
                rollback_errors.push(format!("{}: {create_error}", parent.display()));
                continue;
            }
            write_bytes_if_changed(path, &original)
        } else if path.is_file() {
            fs::remove_file(path).map_err(anyhow::Error::from)
        } else {
            Ok(())
        };
        if let Err(restore_error) = restore {
            rollback_errors.push(format!("{}: {restore_error}", path.display()));
        }
    }
    for (from, to, temporary, original) in case_moves.iter().rev() {
        let restore = (|| -> Result<()> {
            if to.is_file() {
                fs::remove_file(to)?;
            }
            if temporary.is_file() {
                fs::rename(temporary, from)?;
            } else {
                if let Some(parent) = from.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_bytes_if_changed(from, original)?;
            }
            Ok(())
        })();
        if let Err(restore_error) = restore {
            rollback_errors.push(format!("{}: {restore_error}", from.display()));
        }
    }
    for (path, original) in permission_paths.iter().zip(original_permissions).rev() {
        let Some(original) = original else {
            continue;
        };
        if path.exists()
            && let Err(permission_error) = fs::set_permissions(path, original)
        {
            rollback_errors.push(format!("{}: {permission_error}", path.display()));
        }
    }
    created_directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    created_directories.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    for directory in created_directories {
        match fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(remove_error)
                if matches!(
                    remove_error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(remove_error) => {
                rollback_errors.push(format!("{}: {remove_error}", directory.display()));
            }
        }
    }
    if rollback_errors.is_empty() {
        Err(error)
    } else {
        Err(error).context(format!(
            "Filesystem rollback was incomplete: {}",
            rollback_errors.join("; ")
        ))
    }
}

pub(super) struct SettingsFileLock {
    path: PathBuf,
    token: String,
}

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        let still_ours =
            fs::read_to_string(&self.path).is_ok_and(|content| content.trim() == self.token);
        if still_ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Pulls a missing service store from Studio before a command reads or edits
/// it, so the first `bs`, `tr`, `in` or batch op on a service goes on instead
/// of stopping at "pull from Studio". Only a daemon that is already running is
/// asked; any failure leaves the usual missing-store error to the caller.
pub(crate) fn pull_missing_service_store(settings_file: &Path) {
    if settings_file.exists() || !crate::automation::client::shared_daemon_available() {
        return;
    }
    let Some(service) =
        crate::project::storage::store_service_name(settings_file).filter(|name| !name.is_empty())
    else {
        return;
    };
    let mut project_root = PathBuf::from(".");
    let mut src_dir = PathBuf::from("src");
    if crate::project::layout::ensure_explicit_project_root(&project_root).is_err()
        || crate::project::layout::apply_configured_project_layout(&mut project_root, &mut src_dir)
            .is_err()
    {
        return;
    }
    let parameters = json!({
        "srcDir": src_dir,
        "services": service,
        "bridgeWaitSeconds": 1.0,
        "exportAllProperties": false,
    });
    match crate::daemon::daemon_control_request(
        crate::automation::op::PULL,
        Some(&project_root),
        parameters,
        false,
    ) {
        Ok(_) if settings_file.exists() => {
            eprintln!("[renium] {service} had no store; pulled it from Studio first");
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[renium] {service} has no store and the pull from Studio failed: {error:#}");
        }
    }
}

pub(super) fn missing_service_store_error(settings_file: &Path) -> anyhow::Error {
    let service =
        crate::project::storage::store_service_name(settings_file).filter(|name| !name.is_empty());
    match service {
        Some(service) => anyhow::anyhow!(
            "No synced Renium store for service '{service}' at {}.\n       \
             Check the service name, pull from Studio, or create it by adding an instance:\n         \
             rbx ba {service} --name <Name> --class-name <Class>",
            settings_file.display()
        ),
        None => anyhow::anyhow!(
            "No Renium settings store at {}. Pass a synced service name or a .renium file.",
            settings_file.display()
        ),
    }
}

pub(super) fn ensure_service_store_exists(settings_file: &Path, service_hint: &str) -> Result<()> {
    pull_missing_service_store(settings_file);
    if settings_file.exists() {
        return Ok(());
    }
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let service = Some(service_hint.trim().to_string())
        .filter(|service| !service.is_empty())
        .or_else(|| crate::project::storage::store_service_name(settings_file))
        .unwrap_or_default();
    if !service.is_empty() {
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            service.clone(),
            service,
            None,
        ));
    }
    document
        .write_file(settings_file)
        .with_context(|| format!("Failed to create {}", settings_file.display()))
}

pub(super) fn lock_existing_service_store(settings_file: &Path) -> Result<SettingsFileLock> {
    pull_missing_service_store(settings_file);
    if !settings_file.exists() {
        return Err(missing_service_store_error(settings_file));
    }
    acquire_settings_file_lock(settings_file)
}

pub(super) fn acquire_settings_file_lock(settings_file: &Path) -> Result<SettingsFileLock> {
    if let Some(parent) = settings_file.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let lock_path = PathBuf::from(format!("{}.lock", settings_file.display()));
    for attempt in 0..240 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let token = format!("{} {}", std::process::id(), current_millis());
                let _ = writeln!(file, "{token}");
                return Ok(SettingsFileLock {
                    path: lock_path,
                    token,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if attempt > 10 {
                    let owner_alive = fs::read_to_string(&lock_path)
                        .ok()
                        .and_then(|content| {
                            content
                                .split_whitespace()
                                .next()
                                .and_then(|pid| pid.parse::<u32>().ok())
                        })
                        .is_some_and(is_process_alive);
                    if !owner_alive {
                        let _ = fs::remove_file(&lock_path);
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to lock {}", settings_file.display()));
            }
        }
    }
    bail!(
        "Timed out waiting for settings file lock: {}",
        settings_file.display()
    )
}

#[cfg(test)]
mod service_split_tests {
    use super::{high_level_service_and_value, split_leading_service};

    fn known(name: &str) -> bool {
        matches!(name, "StarterGui" | "Workspace" | "Chat")
    }

    #[test]
    fn one_dotted_positional_starting_with_a_service_splits_there() {
        assert_eq!(
            split_leading_service("StarterGui.Main.HUD.Timer", known),
            Some(("StarterGui".to_string(), "Main.HUD.Timer".to_string()))
        );
        assert_eq!(
            split_leading_service("Chat/Channels", known),
            Some(("Chat".to_string(), "Channels".to_string()))
        );
        assert_eq!(split_leading_service("Lobby.Door", known), None);
        assert_eq!(split_leading_service("Workspace", known), None);
        assert_eq!(split_leading_service("Workspace.", known), None);

        let selection = high_level_service_and_value(
            None,
            Some("StarterGui.Main.HUD.Timer"),
            None,
            "tree",
            known,
        )
        .unwrap();
        assert_eq!(selection.service, "StarterGui");
        assert_eq!(
            selection.value.as_deref(),
            Some("StarterGui.Main.HUD.Timer")
        );
        assert!(selection.path_from_service);

        let explicit = high_level_service_and_value(
            None,
            Some("StarterGui"),
            Some("Main.HUD.Timer"),
            "tree",
            known,
        )
        .unwrap();
        assert_eq!(explicit.service, "StarterGui");
        assert_eq!(explicit.value.as_deref(), Some("Main.HUD.Timer"));
        assert!(!explicit.path_from_service);

        let flagged = high_level_service_and_value(
            Some("StarterGui"),
            Some("Main.HUD.Timer"),
            None,
            "tree",
            known,
        )
        .unwrap();
        assert_eq!(flagged.service, "StarterGui");
        assert_eq!(flagged.value.as_deref(), Some("Main.HUD.Timer"));
        assert!(!flagged.path_from_service);

        let unknown =
            high_level_service_and_value(None, Some("Lobby.Door"), None, "tree", known).unwrap();
        assert_eq!(unknown.service, "Lobby.Door");
        assert_eq!(unknown.value, None);
    }
}

#[cfg(test)]
mod property_candidate_tests {
    use super::{closest_property_names, edit_distance};

    #[test]
    fn candidates_rank_substrings_before_near_misses_and_stop_at_five() {
        assert_eq!(edit_distance("assetid", "asset"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(
            closest_property_names(
                "AssetId",
                ["Transparency", "AssetType", "Asset", "MeshId", "asset"]
            ),
            ["Asset", "AssetType"]
        );
        let names = [
            "AssetType",
            "Asset",
            "AssetIds",
            "AssetIdx",
            "AssetIdy",
            "AssetIdz",
        ];
        assert_eq!(
            closest_property_names("AssetId", names),
            ["AssetIds", "AssetIdx", "AssetIdy", "AssetIdz", "Asset"]
        );
        assert_eq!(
            closest_property_names("Colour", ["Color", "Size", "Anchored"]),
            ["Color"]
        );
        assert!(closest_property_names("Velocity", ["Size", "Anchored"]).is_empty());
    }
}

#[cfg(test)]
mod inline_ordinal_tests {
    use super::split_inline_ordinals;

    fn segments(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn inline_ordinals_split_from_segments_and_default_to_one() {
        let (bare, ordinals) =
            split_inline_ordinals(segments(&["Workspace", "Lobby", "Border[4]"]));
        assert_eq!(bare, segments(&["Workspace", "Lobby", "Border"]));
        assert_eq!(ordinals, vec![1, 1, 4]);
        let (bare, ordinals) =
            split_inline_ordinals(segments(&["Workspace", "Lobby[2]", "Border"]));
        assert_eq!(bare, segments(&["Workspace", "Lobby", "Border"]));
        assert_eq!(ordinals, vec![1, 2, 1]);
        let (bare, ordinals) = split_inline_ordinals(segments(&["Workspace", "Border"]));
        assert_eq!(bare, segments(&["Workspace", "Border"]));
        assert!(ordinals.is_empty());
        let (bare, ordinals) = split_inline_ordinals(segments(&["[4]", "Item[0]", "Item[x]"]));
        assert_eq!(bare, segments(&["[4]", "Item[0]", "Item[x]"]));
        assert!(ordinals.is_empty());
    }
}

#[cfg(test)]
mod project_discovery_tests {
    use super::*;

    #[test]
    fn store_commands_outside_any_project_say_so() {
        let root = crate::tests::support::temp_dir("store-without-project");
        fs::create_dir_all(root.join("src")).unwrap();
        let message = match high_level_context(&root, Path::new("src"), "Workspace") {
            Ok(_) => panic!("a directory without a project resolved a store"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            message.starts_with(&format!(
                "No Renium project in {} or its parents",
                root.display()
            )),
            "{message}"
        );
        let (settings_file, _) =
            resolve_bytecode_settings_file(None, Some("Workspace"), None, &root, Path::new("src"))
                .unwrap();
        assert!(!settings_file.exists());
        assert!(
            missing_service_store_error(&settings_file)
                .to_string()
                .contains("No synced Renium store for service 'Workspace'")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
