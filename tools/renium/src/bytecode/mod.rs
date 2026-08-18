use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub(crate) mod edit;
pub(crate) mod explorer;
pub(crate) mod query;

use crate::app::output::{OutputMode, print_json_output};
use crate::app::timing::current_millis;
use crate::bytecode::edit::bytecode_service_name;
use crate::bytecode::explorer::{
    BytecodeNodeProjection, explorer_search_groups, explorer_search_instance_matches,
    insert_top_field, parse_requested_fields,
};
use crate::bytecode::query::{
    BytecodeInstanceTarget, parse_property_predicates, parse_property_scope,
    resolve_bytecode_instance_index,
};
use crate::cli::{
    BytecodeApplyPropertyBatchArgs, BytecodeGetPropertyArgs, BytecodeInstanceSelectorArgs,
    BytecodeSetPropertyArgs, BytecodeSetSourceArgs, FindArgs, HighLevelTargetArgs, InspectArgs,
    TreeArgs,
};
use crate::daemon::is_process_alive;
use crate::editor::document::is_protected_starter_player_container;
use crate::editor::paths::{
    build_editor_instance_path_parts, build_editor_instance_paths,
    build_editor_source_paths_by_index, document_instance_index_by_path_unique,
    path_ordinals_match, script_file_names,
};
use crate::editor::sync::is_lua_source_class;
use crate::project::commands::load_structural_project;
use crate::project::config;
use crate::project::layout::configured_project_layout;
use crate::project::package_links::{LinkEnforcement, build_loaded_project_link_enforcement};
use crate::rbx::decode::rbx_variant_to_settings_json;
use crate::rbx::encode::{rbx_model_property_descriptor, rbx_property_descriptor};
use crate::rbx::model::{
    BytecodeModelImportRefs, canonicalize_settings_reference_documents,
    source_structure_settings_document,
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
    if scope != PropertyScope::Auto
        || matches!(property, "Name" | "Parent")
        || document.instances[index].properties.contains_key(property)
        || document.instances[index].attributes.contains_key(property)
    {
        return Ok(());
    }
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let class_name = document.instances[index].class_name.as_str();
    if rbx_model_property_descriptor(database, class_name, property).is_some()
        || rbx_property_descriptor(database, class_name, property).is_some()
    {
        return Ok(());
    }
    bail!(
        "Property {property:?} does not exist on {class_name}; use --scope property only for an unlisted Roblox property"
    )
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

fn high_level_service_and_value(
    explicit_service: Option<&str>,
    first: Option<&str>,
    second: Option<&str>,
    command: &str,
) -> Result<(String, Option<String>)> {
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
        return Ok((service.to_string(), value));
    }

    let service = first
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("Provide a service: {command} <SERVICE> [TARGET_OR_QUERY]"))?;
    let value = second
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string);
    Ok((service.to_string(), value))
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
        return Ok((settings_file.to_path_buf(), explicit_service));
    }

    if let Some(service_or_file) = service_or_file {
        if bytecode_input_looks_like_settings_file(service_or_file) {
            return Ok((
                absolutize_under(project_root, Path::new(service_or_file)),
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
    insert_top_field(
        &mut response,
        mode,
        "count",
        Value::Number(serde_json::Number::from(indices.len() as u64)),
    );
    if indices.len() > HIGH_LEVEL_AMBIGUITY_LIMIT {
        insert_top_field(&mut response, mode, "truncated", Value::Bool(true));
    }
    insert_top_field(
        &mut response,
        mode,
        "matches",
        Value::Array(high_level_ambiguity_nodes(ctx, indices, mode)),
    );
    print_json_output(&Value::Object(response), pretty)
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
            0 => Err(anyhow::anyhow!("No matching instance path: {path_label}")),
            1 => Ok(HighLevelTargetResolution::Found(matched[0])),
            _ => Ok(HighLevelTargetResolution::Ambiguous(matched)),
        };
    }

    match candidates.len() {
        0 => bail!("No matching instance path: {path_label}"),
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
        0 => bail!("No matching instance: {raw_target}"),
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
            || raw_target.chars().any(|ch| matches!(ch, '/' | '\\' | '.'))
        {
            let segments = high_level_path_segments(raw_target, &ctx.service)?;
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
    let (service, query_text) = high_level_service_and_value(
        args.service.as_deref(),
        args.query_or_service.as_deref(),
        args.query.as_deref(),
        "find",
    )?;
    let ctx = high_level_context(&args.project.project_root, &args.project.src_root, &service)?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(Some(args.fields.as_str()));
    let properties = parse_property_predicates(&args.properties)?;
    let attributes = parse_property_predicates(&args.attributes)?;
    let has_structured_filters = args.name.as_deref().is_some_and(|value| !value.is_empty())
        || args
            .class_name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || args
            .parent_settings_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
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

    let structured_matches = has_structured_filters.then(|| {
        let query = InstanceQuery {
            name: args.name,
            class_name: args.class_name,
            parent_settings_id: args.parent_settings_id,
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
    let (service, positional) = high_level_service_and_value(
        args.target.service.as_deref(),
        args.target.service_or_target.as_deref(),
        args.target.target.as_deref(),
        "tree",
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

pub(super) fn inspect_command(args: InspectArgs) -> Result<()> {
    let (service, positional) = high_level_service_and_value(
        args.target.service.as_deref(),
        args.target.service_or_target.as_deref(),
        args.target.target.as_deref(),
        "inspect",
    )?;
    let ctx = high_level_context(&args.project.project_root, &args.project.src_root, &service)?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(Some(args.fields.as_str()));
    let index = match high_level_target_resolution(
        &ctx,
        high_level_target(&args.target, positional.as_deref()),
    )? {
        HighLevelTargetResolution::Found(index) => index,
        HighLevelTargetResolution::Ambiguous(indices) => {
            return high_level_print_ambiguity(&ctx, mode, &indices, args.pretty);
        }
    };
    let node = ctx.projection(mode, fields.as_ref()).node(index, true);
    print_json_output(&node, args.pretty)
}

pub(super) fn bytecode_get_property(args: BytecodeGetPropertyArgs) -> Result<()> {
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
            resolve_bytecode_selector(document, service, &args.selector, "No matching instance")
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
    let index =
        resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?
            .index;
    if args.property.eq_ignore_ascii_case("source")
        && is_lua_source_class(&document.instances[index].class_name)
    {
        let direct_source_paths;
        let source_paths = if let Some(source_paths) = source_paths.as_ref() {
            source_paths
        } else {
            let service_dir = settings_file.parent().unwrap_or_else(|| Path::new("."));
            direct_source_paths =
                build_editor_source_paths_by_index(&document, &service, service_dir);
            &direct_source_paths
        };
        if let Some(Some(path)) = source_paths.get(index)
            && path.exists()
        {
            let source = fs::read_to_string(path)
                .with_context(|| format!("Failed to read source mirror {}", path.display()))?;
            return print_json_output(&json!(source), args.pretty);
        }
    }
    if let Some(value) =
        instance_api::get_instance_property(&document, index, &args.property, scope)
    {
        let (path_segments, path_ordinals) = build_editor_instance_path_parts(&document, &service);
        let fallback_settings_ids;
        let canonical_settings_ids = if let Some(settings_ids) = canonical_settings_ids.as_ref() {
            settings_ids
        } else {
            fallback_settings_ids = document
                .instances
                .iter()
                .map(|instance| Some(instance.settings_id.clone()))
                .collect::<Vec<_>>();
            &fallback_settings_ids
        };
        let mut record = Map::from_iter([("value".to_string(), value)]);
        stabilize_reference_output(
            &document,
            &path_segments,
            &path_ordinals,
            canonical_settings_ids,
            &mut record,
        );
        let value = record
            .remove("value")
            .context("Property output is missing")?;
        return print_json_output(&value, args.pretty);
    }
    if matches!(scope, PropertyScope::Auto | PropertyScope::Property)
        && !args.property.eq_ignore_ascii_case("source")
    {
        let database =
            rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
        let class_name = document.instances[index].class_name.as_str();
        if let Some(default) = database
            .classes
            .get(class_name)
            .and_then(|class| database.find_default_property(class, &args.property))
        {
            let descriptor = rbx_model_property_descriptor(database, class_name, &args.property)
                .or_else(|| rbx_property_descriptor(database, class_name, &args.property));
            if let Some(value) = rbx_variant_to_settings_json(
                default,
                descriptor,
                database,
                &BytecodeModelImportRefs::default(),
            ) {
                return print_json_output(&value, args.pretty);
            }
        }
    }
    bail!("Property not found: {}", args.property)
}

pub(super) fn bytecode_set_property(args: BytecodeSetPropertyArgs) -> Result<()> {
    if args.property.eq_ignore_ascii_case("classname") {
        bail!("ClassName is read-only");
    }
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
    let mut settings_files_to_lock = BTreeSet::from([settings_file.clone()]);
    if structural_reference_update
        && let Some(src_root) = settings_file.parent().and_then(Path::parent)
    {
        for entry in fs::read_dir(src_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let path = service_settings_path(&entry.path());
                if path.is_file() {
                    settings_files_to_lock.insert(path);
                }
            }
        }
    }
    let _locks = settings_files_to_lock
        .iter()
        .map(|path| lock_existing_service_store(path))
        .collect::<Result<Vec<_>>>()?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let resolved =
        resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?;
    let value = parse_cli_value(
        args.value_json.as_deref(),
        args.value_str.as_deref(),
        args.value_num,
        args.value_bool,
        args.value_null,
    )?;
    let index = resolved.index;
    validate_auto_property_name(&document, index, &args.property, scope)?;
    if matches!(scope, PropertyScope::Auto | PropertyScope::Metadata)
        && matches!(args.property.as_str(), "ClassName" | "Parent")
        && is_protected_starter_player_container(&document, index)
    {
        bail!("{} metadata is read-only", document.instances[index].name);
    }
    let service_dir = settings_file.parent().unwrap_or_else(|| Path::new("."));
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
        return print_json_output(
            &json!({
                "ok": true,
                "settingsFile": settings_file,
                "property": args.property,
                "sourcePath": source_path,
                "changedPaths": changed_paths,
            }),
            args.pretty,
        );
    }
    let affects_source_path = property_affects_source_path(scope, &args.property);
    let before_document = affects_source_path.then(|| document.clone());
    let source_paths_before = before_document
        .as_ref()
        .map(|document| build_editor_source_paths_by_index(document, &service, service_dir));
    instance_api::set_instance_property(&mut document, index, &args.property, value, scope)?;
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
    if structural_reference_update {
        let mut reference_documents = BTreeMap::new();
        let mut reference_files = BTreeMap::new();
        for path in &settings_files_to_lock {
            let service_name = path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .context("Service settings path has no service directory")?;
            let value = if *path == settings_file {
                document.clone()
            } else {
                SettingsBytecode::read_file(path)?
            };
            reference_files.insert(service_name.clone(), path.clone());
            reference_documents.insert(service_name, value);
        }
        let changed_services = canonicalize_settings_reference_documents(&mut reference_documents);
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
        "property": args.property,
        "changedPaths": changed_paths,
    });
    if structural_reference_update {
        let instance = &document.instances[index];
        let (path_segments, path_ordinals) = build_editor_instance_path_parts(&document, &service);
        result["settingsId"] = json!(instance.settings_id);
        result["name"] = json!(instance.name);
        result["className"] = json!(instance.class_name);
        result["pathSegments"] = json!(path_segments.get(index).and_then(Clone::clone));
        result["pathOrdinals"] = json!(path_ordinals.get(index).and_then(Clone::clone));
    }
    print_json_output(&result, args.pretty)
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
        stage.publish(&loaded.root)?;
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
            && is_protected_starter_player_container(&state.document, instance_index)
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
            let service_dir = state
                .settings_file
                .parent()
                .context("Settings file has no parent directory")?;
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
    let refresh_references = entries.iter().any(|entry| {
        matches!(
            entry.property.to_ascii_lowercase().as_str(),
            "name" | "parent"
        ) && matches!(entry.scope, PropertyScope::Auto | PropertyScope::Metadata)
    });
    if !refresh_references {
        return Ok(());
    }
    let mut reference_documents = documents
        .iter()
        .map(|(service, state)| (service.clone(), state.document.clone()))
        .collect::<BTreeMap<_, _>>();
    for service in canonicalize_settings_reference_documents(&mut reference_documents) {
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
            let service_dir = state
                .settings_file
                .parent()
                .context("Settings file has no parent directory")?;
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
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && service_settings_path(&entry.path()).is_file() {
                loaded_services.insert(entry.file_name().to_string_lossy().into_owned());
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

pub(super) fn bytecode_set_source(args: BytecodeSetSourceArgs) -> Result<()> {
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
    let service_dir = settings_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Settings file has no parent directory"))?;
    let source_path = build_editor_source_paths_by_index(document, service, service_dir)
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

pub(super) fn missing_service_store_error(settings_file: &Path) -> anyhow::Error {
    let service = settings_file
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty());
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
    if settings_file.exists() {
        return Ok(());
    }
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    let service = service_hint.trim();
    if !service.is_empty() {
        document.instances.push(SettingsBytecodeInstance::new(
            "editor:0".to_string(),
            service.to_string(),
            service.to_string(),
            None,
        ));
    }
    document
        .write_file(settings_file)
        .with_context(|| format!("Failed to create {}", settings_file.display()))
}

pub(super) fn lock_existing_service_store(settings_file: &Path) -> Result<SettingsFileLock> {
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
                if attempt > 60
                    && let Ok(metadata) = fs::metadata(&lock_path)
                    && metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(30))
                {
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
