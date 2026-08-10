use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::bytecode_api::{
    ensure_bytecode_service_path_segments, parse_bytecode_path_ordinals,
    parse_bytecode_path_segments, resolve_bytecode_read_input, resolve_bytecode_selector,
};
use super::bytecode_edit::has_direct_package_link_child;
use super::bytecode_query::{
    bytecode_selector, bytecode_selector_specified, parse_property_predicates,
};
use super::command_line::{
    BytecodeBatchFields, BytecodeEditorTargetsArgs, BytecodeExplorerBatchArgs,
    BytecodeExplorerBatchOp, BytecodeExplorerBatchRequest, BytecodeExplorerChildrenArgs,
    BytecodeExplorerCountsArgs, BytecodeExplorerInstanceArgs, BytecodeExplorerSearchArgs,
    BytecodeExplorerServiceArgs, BytecodeFindInstancesArgs, ExplorerDaemonArgs,
};
use super::editor_document::is_protected_starter_player_container;
use super::editor_paths::{
    build_editor_instance_path_parts, build_editor_instance_paths,
    build_editor_source_paths_by_index, document_instance_index_by_path_unique,
};
use super::file_io::{absolutize_under, service_settings_path, validate_filesystem_instance_name};
use super::instance_api::{self, InstanceQuery};
use super::local_transport::{BoundedLineRead, MAX_DAEMON_LINE_BYTES, read_bounded_line};
use super::output::{OutputMode, print_json_output};
use super::project_commands::load_structural_project;
use super::project_config;
use super::project_layout::apply_configured_project_layout;
use super::services::{DEFAULT_SYNC_SERVICES, EXTRA_EXPLORER_SERVICES, explorer_service_order};
use super::settings_bytecode::{SettingsBytecode, SettingsBytecodeInstance};
use super::settings_tree::{editor_service_root_index, settings_children_by_parent};
use super::snapshot_import::parse_services;
use super::timing::elapsed_ms;

pub(super) fn parse_requested_fields(raw: Option<&str>) -> Option<HashSet<String>> {
    let mut fields = HashSet::new();
    for field in raw
        .unwrap_or("")
        .split(',')
        .map(|field| field.trim().to_ascii_lowercase())
        .filter(|field| !field.is_empty())
    {
        match field.as_str() {
            "lookup" | "select" | "sel" => {
                fields.extend(
                    ["id", "n", "c", "path"]
                        .into_iter()
                        .map(|field| field.to_string()),
                );
            }
            "tree" | "expand" | "kids" => {
                fields.extend(
                    ["id", "n", "c", "cc", "ch"]
                        .into_iter()
                        .map(|field| field.to_string()),
                );
            }
            "brief" | "node" => {
                fields.extend(
                    ["id", "n", "c", "path", "cc"]
                        .into_iter()
                        .map(|field| field.to_string()),
                );
            }
            _ => {
                fields.insert(field);
            }
        }
    }
    (!fields.is_empty()).then_some(fields)
}

struct BytecodeProjectionData {
    children_by_parent: Vec<Vec<usize>>,
    path_segments_by_index: Vec<Option<Vec<String>>>,
    path_ordinals_by_index: Vec<Option<Vec<usize>>>,
    source_paths_by_index: Vec<Option<PathBuf>>,
}

impl BytecodeProjectionData {
    fn new(document: &SettingsBytecode, service: &str, settings_file: &Path) -> Self {
        let (path_segments_by_index, path_ordinals_by_index) =
            build_editor_instance_path_parts(document, service);
        let source_paths_by_index = settings_file
            .parent()
            .map(|service_dir| build_editor_source_paths_by_index(document, service, service_dir))
            .unwrap_or_else(|| vec![None; document.instances.len()]);
        Self {
            children_by_parent: settings_children_by_parent(document),
            path_segments_by_index,
            path_ordinals_by_index,
            source_paths_by_index,
        }
    }

    fn projection<'a>(
        &'a self,
        document: &'a SettingsBytecode,
        mode: OutputMode,
        fields: Option<&'a HashSet<String>>,
    ) -> BytecodeNodeProjection<'a> {
        BytecodeNodeProjection {
            document,
            children_by_parent: &self.children_by_parent,
            path_segments_by_index: &self.path_segments_by_index,
            path_ordinals_by_index: &self.path_ordinals_by_index,
            source_paths_by_index: &self.source_paths_by_index,
            mode,
            fields,
        }
    }
}

struct BytecodeExplorerBatchContext<'a> {
    document: &'a SettingsBytecode,
    service: &'a str,
    children_by_parent: &'a [Vec<usize>],
    service_path_segments_by_index: &'a [Option<Vec<String>>],
    service_path_ordinals_by_index: &'a [Option<Vec<usize>>],
    service_source_paths_by_index: &'a [Option<PathBuf>],
    service_settings_files_by_index: &'a [Option<PathBuf>],
    service_canonical_settings_ids_by_index: &'a [Option<String>],
    global_path_segments_by_index: &'a [Option<Vec<String>>],
    global_path_ordinals_by_index: &'a [Option<Vec<usize>>],
    global_source_paths_by_index: &'a [Option<PathBuf>],
    default_output: Option<&'a str>,
    default_fields: Option<&'a str>,
}

impl<'a> BytecodeExplorerBatchContext<'a> {
    fn service_projection(
        &'a self,
        mode: OutputMode,
        fields: Option<&'a HashSet<String>>,
    ) -> BytecodeNodeProjection<'a> {
        BytecodeNodeProjection {
            document: self.document,
            children_by_parent: self.children_by_parent,
            path_segments_by_index: self.service_path_segments_by_index,
            path_ordinals_by_index: self.service_path_ordinals_by_index,
            source_paths_by_index: self.service_source_paths_by_index,
            mode,
            fields,
        }
    }

    fn global_projection(
        &'a self,
        mode: OutputMode,
        fields: Option<&'a HashSet<String>>,
    ) -> BytecodeNodeProjection<'a> {
        BytecodeNodeProjection {
            document: self.document,
            children_by_parent: self.children_by_parent,
            path_segments_by_index: self.global_path_segments_by_index,
            path_ordinals_by_index: self.global_path_ordinals_by_index,
            source_paths_by_index: self.global_source_paths_by_index,
            mode,
            fields,
        }
    }
}

fn bytecode_project_explorer_node_json(
    ctx: &BytecodeExplorerBatchContext<'_>,
    index: usize,
    include_children: bool,
    mode: OutputMode,
    fields: Option<&HashSet<String>>,
) -> Value {
    let mut node = ctx
        .service_projection(mode, fields)
        .node(index, include_children);
    if let Some(map) = node.as_object_mut() {
        map.insert(
            "settingsFile".to_string(),
            json!(
                ctx.service_settings_files_by_index
                    .get(index)
                    .and_then(|path| path.as_ref())
            ),
        );
        map.insert(
            "canonicalSettingsId".to_string(),
            json!(
                ctx.service_canonical_settings_ids_by_index
                    .get(index)
                    .and_then(|settings_id| settings_id.as_deref())
            ),
        );
    }
    node
}

fn read_bytecode_explorer_batch_ops(
    args: &BytecodeExplorerBatchArgs,
) -> Result<Vec<BytecodeExplorerBatchOp>> {
    let raw = if let Some(raw) = args.ops_json.as_deref() {
        raw.to_string()
    } else if let Some(path) = args.ops_file.as_deref() {
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?
    } else {
        bail!("Provide -j/--ops, --ops-json, or -J/--ops-file")
    };
    parse_bytecode_explorer_batch_ops(&raw)
}

fn parse_bytecode_explorer_batch_ops(raw: &str) -> Result<Vec<BytecodeExplorerBatchOp>> {
    if let Ok(request) = serde_json::from_str::<BytecodeExplorerBatchRequest>(raw) {
        return Ok(request.ops);
    }
    serde_json::from_str::<Vec<BytecodeExplorerBatchOp>>(raw)
        .context("Invalid batch ops JSON; expected {\"ops\":[...]} or [...]")
}

fn parse_batch_requested_fields(
    fields: Option<&BytecodeBatchFields>,
    fallback: Option<&str>,
) -> Option<HashSet<String>> {
    match fields {
        Some(BytecodeBatchFields::Csv(raw)) => parse_requested_fields(Some(raw.as_str())),
        Some(BytecodeBatchFields::List(items)) => {
            let raw = items.join(",");
            parse_requested_fields(Some(raw.as_str()))
        }
        None => parse_requested_fields(fallback),
    }
}

fn batch_output_mode(
    op_output: Option<&str>,
    batch_output: Option<&str>,
    default_output: &str,
) -> Result<OutputMode> {
    OutputMode::parse(op_output.or(batch_output).unwrap_or(default_output))
}

fn normalize_bytecode_batch_op(raw: &str) -> Result<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        "counts" | "count" | "bc" => Ok("counts"),
        "children" | "child" | "bch" => Ok("children"),
        "service" | "svc" | "bsvc" => Ok("service"),
        "search" | "query" | "bq" => Ok("search"),
        "instance" | "node" | "bi" => Ok("instance"),
        "find" | "match" | "bf" => Ok("find"),
        other => bail!(
            "Unsupported batch op type: {other}. Use counts, children, service, search, instance, or find."
        ),
    }
}

fn bytecode_batch_selector_specified(op: &BytecodeExplorerBatchOp) -> bool {
    op.index.is_some()
        || op
            .settings_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || op.name.as_deref().is_some_and(|value| !value.is_empty())
        || op
            .class_name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
}

fn bytecode_batch_instance_index(
    document: &SettingsBytecode,
    service: &str,
    op: &BytecodeExplorerBatchOp,
    default_to_service_root: bool,
    missing_message: &str,
) -> Result<usize> {
    if let Some(path_segments) = op.path_segments.as_deref() {
        if bytecode_batch_selector_specified(op) {
            bail!("pathSegments cannot be combined with another selector");
        }
        let path_segments = ensure_bytecode_service_path_segments(path_segments, service);
        return document_instance_index_by_path_unique(document, &path_segments, &op.path_ordinals);
    }
    if !bytecode_batch_selector_specified(op) {
        if default_to_service_root {
            return editor_service_root_index(document, service)
                .ok_or_else(|| anyhow::anyhow!("No service root in settings bytecode"));
        }
        bail!("Provide one selector: index, settingsId, name, className, or pathSegments")
    }
    let selector = bytecode_selector(
        op.index,
        op.settings_id.as_deref(),
        op.name.as_deref(),
        op.class_name.as_deref(),
    )?;
    instance_api::find_unique_instance_index(document, selector)?
        .ok_or_else(|| anyhow::anyhow!("{missing_message}"))
}

fn field_matches(fields: &HashSet<String>, key: &str, aliases: &[&str]) -> bool {
    let key = key.to_ascii_lowercase();
    fields.contains(&key) || aliases.iter().any(|alias| fields.contains(*alias))
}

fn node_field_aliases(key: &str) -> &'static [&'static str] {
    match key {
        "settingsId" => &["id", "settingsid"],
        "index" => &["x"],
        "name" => &["n"],
        "className" => &["c", "class", "classname"],
        "parentId" => &["pid", "parentid"],
        "parentIndex" => &["px", "parentindex"],
        "childCount" => &["cc", "childcount"],
        "hasPackageLink" => &["hpl", "package", "packagelink"],
        "children" => &["ch"],
        "pathSegments" => &["path", "segments", "pathsegments"],
        "pathOrdinals" => &["ords", "ordinals", "pathordinals"],
        "sourcePath" => &["src", "source", "sourcepath"],
        "properties" => &["props", "p", "properties"],
        "attributes" => &["attrs", "a", "attributes"],
        _ => &[],
    }
}

fn node_output_key(mode: OutputMode, key: &str) -> &str {
    if !mode.uses_short_keys() {
        return key;
    }
    match key {
        "settingsId" => "id",
        "index" => "x",
        "name" => "n",
        "className" => "c",
        "parentId" => "pid",
        "parentIndex" => "px",
        "childCount" => "cc",
        "hasPackageLink" => "hpl",
        "children" => "ch",
        "pathSegments" => "path",
        "pathOrdinals" => "ords",
        "sourcePath" => "src",
        "properties" => "props",
        "attributes" => "attrs",
        _ => key,
    }
}

fn top_output_key(mode: OutputMode, key: &str) -> &str {
    if !mode.uses_short_keys() {
        return key;
    }
    match key {
        "settingsFile" => "f",
        "service" => "s",
        "rootId" | "rootIds" => "r",
        "rootChildren" => "rc",
        "descendants" => "d",
        "instances" => "n",
        "matches" | "matchIds" => "m",
        "visibleIds" => "v",
        "nodes" => "ns",
        "results" => "rs",
        "requestId" => "q",
        "type" => "t",
        "parent" | "property" => "p",
        "children" => "ch",
        "removedIndexes" => "rm",
        "settingsIds" => "ids",
        "rootSettingsId" | "settingsId" => "id",
        "sourceCopies" | "sourcePath" => "src",
        "index" => "x",
        "error" => "e",
        "count" => "ct",
        "truncated" => "tr",
        "ok" => "ok",
        _ => key,
    }
}

pub(super) fn insert_top_field(
    map: &mut Map<String, Value>,
    mode: OutputMode,
    key: &str,
    value: Value,
) {
    map.insert(top_output_key(mode, key).to_string(), value);
}

fn requested_field(fields: Option<&HashSet<String>>, key: &str, aliases: &[&str]) -> bool {
    fields.is_some_and(|fields| field_matches(fields, key, aliases))
}

fn requested_property_field(fields: Option<&HashSet<String>>, key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    fields.is_some_and(|fields| {
        fields.contains(&key)
            || fields.contains(&format!("p:{key}"))
            || fields.contains(&format!("prop:{key}"))
            || fields.contains(&format!("property:{key}"))
    })
}

fn requested_attribute_field(fields: Option<&HashSet<String>>, key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    fields.is_some_and(|fields| {
        fields.contains(&key)
            || fields.contains(&format!("a:{key}"))
            || fields.contains(&format!("attr:{key}"))
            || fields.contains(&format!("attribute:{key}"))
    })
}

fn include_default_node_field(mode: OutputMode, key: &str) -> bool {
    match key {
        "index" | "parentId" | "parentIndex" | "pathSegments" | "pathOrdinals" | "sourcePath" => {
            matches!(mode, OutputMode::Detail | OutputMode::Full)
        }
        "properties" | "attributes" => matches!(mode, OutputMode::Full),
        _ => true,
    }
}

fn should_include_node_field(
    mode: OutputMode,
    fields: Option<&HashSet<String>>,
    key: &str,
) -> bool {
    if fields.is_some() {
        return requested_field(fields, key, node_field_aliases(key));
    }
    include_default_node_field(mode, key)
}

fn filtered_record(
    record: &Map<String, Value>,
    fields: Option<&HashSet<String>>,
    include_all: bool,
    attribute_record: bool,
) -> Option<Value> {
    if include_all {
        return Some(Value::Object(record.clone()));
    }
    let fields = fields?;
    let include_all_key = if attribute_record {
        requested_field(Some(fields), "attributes", node_field_aliases("attributes"))
    } else {
        requested_field(Some(fields), "properties", node_field_aliases("properties"))
    };
    if include_all_key {
        return Some(Value::Object(record.clone()));
    }
    let filtered = record
        .iter()
        .filter(|(name, _)| {
            if attribute_record {
                requested_attribute_field(Some(fields), name)
            } else {
                requested_property_field(Some(fields), name)
            }
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Map<String, Value>>();
    (!filtered.is_empty()).then_some(Value::Object(filtered))
}

pub(super) fn bytecode_explorer_batch(args: BytecodeExplorerBatchArgs) -> Result<()> {
    let pretty = args.pretty;
    print_json_output(&bytecode_explorer_batch_result(args)?, pretty)
}

pub(super) fn bytecode_explorer_batch_result(args: BytecodeExplorerBatchArgs) -> Result<Value> {
    let ops = read_bytecode_explorer_batch_ops(&args)?;
    if ops.is_empty() {
        bail!("Batch request is empty")
    }

    let mut loaded_project = None;
    let mut projection = None;
    let (settings_file, document, service) = if let Some(project_root) =
        args.project_root.as_deref()
    {
        if args.settings_file.is_some() || args.service_or_file.is_some() {
            bail!("--project-root cannot be combined with a settings file");
        }
        if args.service.trim().is_empty() {
            bail!("--project-root requires --service");
        }
        let loaded = load_structural_project(None, project_root)?;
        let staged = project_config::stage_project(&loaded)?;
        let service = canonical_explorer_service_name(&args.service);
        let staged_settings = service_settings_path(&staged.root().join(&service));
        if !staged_settings.is_file() {
            bail!("Projected service '{service}' has no Renium store");
        }
        let document = SettingsBytecode::read_file(&staged_settings)?;
        let display_settings =
            service_settings_path(&loaded.root.join(&loaded.project.source_root).join(&service));
        loaded_project = Some(loaded);
        projection = Some(staged);
        (display_settings, document, service)
    } else {
        resolve_bytecode_read_input(
            args.settings_file.as_deref(),
            args.service_or_file.as_deref(),
            Some(args.service.as_str()),
        )?
    };
    let children_by_parent = settings_children_by_parent(&document);
    let (service_path_segments_by_index, service_path_ordinals_by_index) =
        build_editor_instance_path_parts(&document, &service);
    let staged_settings_file = projection
        .as_ref()
        .map(|stage| service_settings_path(&stage.root().join(&service)))
        .unwrap_or_else(|| settings_file.clone());
    let mut service_source_paths_by_index = staged_settings_file
        .parent()
        .map(|service_dir| build_editor_source_paths_by_index(&document, &service, service_dir))
        .unwrap_or_else(|| vec![None; document.instances.len()]);
    if let (Some(loaded), Some(stage)) = (loaded_project.as_ref(), projection.as_ref()) {
        for source in &mut service_source_paths_by_index {
            let Some(path) = source.as_ref() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(stage.root()) else {
                *source = None;
                continue;
            };
            *source = project_config::staged_path_to_project_source(loaded, relative)
                .ok()
                .flatten();
        }
    }
    let mut service_settings_files_by_index =
        vec![Some(staged_settings_file); document.instances.len()];
    let mut service_canonical_settings_ids_by_index = document
        .instances
        .iter()
        .map(|instance| Some(instance.settings_id.clone()))
        .collect::<Vec<_>>();
    if let (Some(loaded), Some(stage)) = (loaded_project.as_ref(), projection.as_ref()) {
        for settings_file in &mut service_settings_files_by_index {
            let Some(path) = settings_file.as_ref() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(stage.root()) else {
                *settings_file = None;
                continue;
            };
            *settings_file = project_config::project_staged_path_to_source(loaded, relative).ok();
        }
        for (index, instance) in document.instances.iter().enumerate() {
            if let Some((settings_file, settings_id)) =
                stage.canonical_identity(&instance.settings_id)
            {
                service_settings_files_by_index[index] = Some(settings_file.to_path_buf());
                service_canonical_settings_ids_by_index[index] = Some(settings_id.to_string());
            }
        }
    }
    let (global_path_segments_by_index, global_path_ordinals_by_index) =
        build_editor_instance_path_parts(&document, &service);
    let global_source_paths_by_index = vec![None; document.instances.len()];
    let ctx = BytecodeExplorerBatchContext {
        document: &document,
        service: &service,
        children_by_parent: &children_by_parent,
        service_path_segments_by_index: &service_path_segments_by_index,
        service_path_ordinals_by_index: &service_path_ordinals_by_index,
        service_source_paths_by_index: &service_source_paths_by_index,
        service_settings_files_by_index: &service_settings_files_by_index,
        service_canonical_settings_ids_by_index: &service_canonical_settings_ids_by_index,
        global_path_segments_by_index: &global_path_segments_by_index,
        global_path_ordinals_by_index: &global_path_ordinals_by_index,
        global_source_paths_by_index: &global_source_paths_by_index,
        default_output: args.output.as_deref(),
        default_fields: args.fields.as_deref(),
    };

    let top_mode = OutputMode::parse(args.output.as_deref().unwrap_or("compact"))?;
    let results = ops
        .iter()
        .map(|op| bytecode_explorer_batch_op_json(&ctx, op))
        .collect::<Result<Vec<_>>>()?;
    let response_settings_file = editor_service_root_index(&document, &service)
        .and_then(|index| service_settings_files_by_index.get(index))
        .and_then(|path| path.as_ref())
        .unwrap_or(&settings_file);

    let mut response = Map::new();
    insert_top_field(
        &mut response,
        top_mode,
        "settingsFile",
        json!(response_settings_file),
    );
    insert_top_field(&mut response, top_mode, "service", Value::String(service));
    insert_top_field(&mut response, top_mode, "results", Value::Array(results));
    Ok(Value::Object(response))
}

fn bytecode_explorer_batch_op_json(
    ctx: &BytecodeExplorerBatchContext<'_>,
    op: &BytecodeExplorerBatchOp,
) -> Result<Value> {
    let kind = normalize_bytecode_batch_op(&op.op)?;
    let mode = batch_output_mode(op.output.as_deref(), ctx.default_output, "compact")?;
    let fields = parse_batch_requested_fields(op.fields.as_ref(), ctx.default_fields);
    let mut response = Map::new();
    insert_top_field(&mut response, mode, "type", Value::String(kind.to_string()));
    if let Some(request_id) = op.request_id.as_deref() {
        insert_top_field(
            &mut response,
            mode,
            "requestId",
            Value::String(request_id.to_string()),
        );
    }

    match kind {
        "counts" => {
            let root_index = editor_service_root_index(ctx.document, ctx.service);
            let (root_id, root_children, descendants) = if let Some(index) = root_index {
                let root_children = ctx
                    .children_by_parent
                    .get(index)
                    .map(Vec::len)
                    .unwrap_or_default();
                (
                    ctx.document
                        .instances
                        .get(index)
                        .map(|instance| instance.settings_id.clone()),
                    root_children,
                    count_settings_descendants(ctx.children_by_parent, index),
                )
            } else {
                (None, 0, 0)
            };
            insert_top_field(&mut response, mode, "rootId", json!(root_id));
            insert_top_field(&mut response, mode, "rootChildren", json!(root_children));
            insert_top_field(&mut response, mode, "descendants", json!(descendants));
            insert_top_field(
                &mut response,
                mode,
                "instances",
                json!(ctx.document.instances.len()),
            );
        }
        "children" => {
            let parent_index = bytecode_batch_instance_index(
                ctx.document,
                ctx.service,
                op,
                true,
                "No matching parent instance",
            )?;
            let child_nodes = ctx
                .children_by_parent
                .get(parent_index)
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .filter_map(|child_index| {
                    ctx.document.instances.get(*child_index)?;
                    Some(bytecode_project_explorer_node_json(
                        ctx,
                        *child_index,
                        false,
                        mode,
                        fields.as_ref(),
                    ))
                })
                .collect::<Vec<_>>();
            insert_top_field(
                &mut response,
                mode,
                "parent",
                bytecode_project_explorer_node_json(ctx, parent_index, true, mode, fields.as_ref()),
            );
            insert_top_field(&mut response, mode, "children", Value::Array(child_nodes));
        }
        "service" => {
            let nodes = ctx
                .document
                .instances
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    bytecode_project_explorer_node_json(ctx, index, true, mode, fields.as_ref())
                })
                .collect::<Vec<_>>();
            let root_ids = ctx
                .document
                .instances
                .iter()
                .filter(|instance| instance.parent_index.is_none())
                .map(|instance| Value::String(instance.settings_id.clone()))
                .collect::<Vec<_>>();
            insert_top_field(&mut response, mode, "rootIds", Value::Array(root_ids));
            insert_top_field(&mut response, mode, "nodes", Value::Array(nodes));
        }
        "search" => {
            let query = op.query.as_deref().unwrap_or_default();
            let limit = op.limit.unwrap_or(20);
            let root_index = editor_service_root_index(ctx.document, ctx.service);
            let groups = explorer_search_groups(query);
            let mut match_indices = Vec::new();
            let mut visible_indices = HashSet::new();
            if !groups.is_empty() {
                for index in 0..ctx.document.instances.len() {
                    if explorer_search_instance_matches(
                        ctx.document,
                        ctx.service_path_segments_by_index,
                        index,
                        &groups,
                    ) {
                        if limit > 0 && match_indices.len() >= limit {
                            break;
                        }
                        match_indices.push(index);
                        let mut current = Some(index);
                        while let Some(ancestor_index) = current {
                            if !visible_indices.insert(ancestor_index) {
                                break;
                            }
                            current = ctx.document.instances[ancestor_index].parent_index;
                        }
                    }
                }
            }
            if let Some(index) = root_index {
                visible_indices.insert(index);
            }
            let projection = ctx.service_projection(mode, fields.as_ref());
            let nodes = (0..ctx.document.instances.len())
                .filter(|index| visible_indices.contains(index))
                .map(|index| {
                    let mut node = projection.search_node(index, &visible_indices);
                    if let Some(map) = node.as_object_mut() {
                        map.insert(
                            "settingsFile".to_string(),
                            json!(
                                ctx.service_settings_files_by_index
                                    .get(index)
                                    .and_then(|path| path.as_ref())
                            ),
                        );
                        map.insert(
                            "canonicalSettingsId".to_string(),
                            json!(
                                ctx.service_canonical_settings_ids_by_index
                                    .get(index)
                                    .and_then(|settings_id| settings_id.as_deref())
                            ),
                        );
                    }
                    node
                })
                .collect::<Vec<_>>();
            let root_ids = root_index
                .and_then(|index| ctx.document.instances.get(index))
                .map(|instance| vec![Value::String(instance.settings_id.clone())])
                .unwrap_or_default();
            let match_ids = match_indices
                .iter()
                .filter_map(|index| ctx.document.instances.get(*index))
                .map(|instance| Value::String(instance.settings_id.clone()))
                .collect::<Vec<_>>();
            let visible_ids = visible_indices
                .iter()
                .filter_map(|index| ctx.document.instances.get(*index))
                .map(|instance| Value::String(instance.settings_id.clone()))
                .collect::<Vec<_>>();
            insert_top_field(&mut response, mode, "rootIds", Value::Array(root_ids));
            insert_top_field(&mut response, mode, "matchIds", Value::Array(match_ids));
            insert_top_field(&mut response, mode, "visibleIds", Value::Array(visible_ids));
            insert_top_field(&mut response, mode, "nodes", Value::Array(nodes));
        }
        "instance" => {
            let index = bytecode_batch_instance_index(
                ctx.document,
                ctx.service,
                op,
                false,
                "No matching instance",
            )?;
            let node = bytecode_project_explorer_node_json(ctx, index, true, mode, fields.as_ref());
            if let Some(node) = node.as_object() {
                response.extend(node.clone());
            }
        }
        "find" => {
            let query = InstanceQuery {
                name: op.name.clone(),
                class_name: op.class_name.clone(),
                parent_settings_id: op.parent_settings_id.clone(),
                tag: op.tag.clone(),
                properties: parse_property_predicates(&op.properties)?,
                attributes: parse_property_predicates(&op.attributes)?,
            };
            let limit = op.limit.unwrap_or(20);
            let mut match_indices = instance_api::find_instances(ctx.document, &query);
            if limit > 0 {
                match_indices.truncate(limit);
            }
            let projection = ctx.global_projection(mode, fields.as_ref());
            let matches = match_indices
                .into_iter()
                .map(|index| projection.node(index, false))
                .collect::<Vec<_>>();
            insert_top_field(&mut response, mode, "matches", Value::Array(matches));
        }
        _ => unreachable!("normalized batch op is exhaustive"),
    }

    Ok(Value::Object(response))
}

pub(super) fn bytecode_find_instances(args: BytecodeFindInstancesArgs) -> Result<()> {
    let (settings_file, document, service) = resolve_bytecode_read_input(
        args.settings_file.as_deref(),
        args.service_or_file.as_deref(),
        None,
    )?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(args.fields.as_deref());
    let query = InstanceQuery {
        name: args.name,
        class_name: args.class_name,
        parent_settings_id: args.parent_settings_id,
        tag: args.tag,
        properties: parse_property_predicates(&args.properties)?,
        attributes: parse_property_predicates(&args.attributes)?,
    };
    let projection_data = BytecodeProjectionData::new(&document, &service, &settings_file);
    let projection = projection_data.projection(&document, mode, fields.as_ref());
    let mut match_indices = instance_api::find_instances(&document, &query);
    if args.limit > 0 {
        match_indices.truncate(args.limit);
    }
    let matches = match_indices
        .into_iter()
        .map(|index| projection.node(index, false))
        .collect::<Vec<_>>();
    let mut response = Map::new();
    insert_top_field(&mut response, mode, "settingsFile", json!(settings_file));
    insert_top_field(&mut response, mode, "service", Value::String(service));
    insert_top_field(&mut response, mode, "matches", Value::Array(matches));
    print_json_output(&Value::Object(response), args.pretty)
}

pub(super) fn bytecode_explorer_counts(args: BytecodeExplorerCountsArgs) -> Result<()> {
    let (settings_file, document, service) = resolve_bytecode_read_input(
        args.settings_file.as_deref(),
        args.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let mode = OutputMode::parse(&args.output)?;
    let children_by_parent = settings_children_by_parent(&document);
    let root_index = editor_service_root_index(&document, &service);
    let (root_id, root_children, descendants) = if let Some(index) = root_index {
        let root_children = children_by_parent
            .get(index)
            .map(Vec::len)
            .unwrap_or_default();
        (
            document
                .instances
                .get(index)
                .map(|instance| instance.settings_id.clone()),
            root_children,
            count_settings_descendants(&children_by_parent, index),
        )
    } else {
        (None, 0, 0)
    };

    let mut response = Map::new();
    insert_top_field(&mut response, mode, "settingsFile", json!(settings_file));
    insert_top_field(&mut response, mode, "service", Value::String(service));
    insert_top_field(&mut response, mode, "rootId", json!(root_id));
    insert_top_field(&mut response, mode, "rootChildren", json!(root_children));
    insert_top_field(&mut response, mode, "descendants", json!(descendants));
    insert_top_field(
        &mut response,
        mode,
        "instances",
        json!(document.instances.len()),
    );
    print_json_output(&Value::Object(response), args.pretty)
}

fn count_settings_descendants(children_by_parent: &[Vec<usize>], root_index: usize) -> usize {
    let mut count = 0usize;
    let mut stack = children_by_parent
        .get(root_index)
        .cloned()
        .unwrap_or_default();
    while let Some(index) = stack.pop() {
        count += 1;
        if let Some(children) = children_by_parent.get(index) {
            stack.extend(children.iter().copied());
        }
    }
    count
}

pub(super) fn bytecode_explorer_children(args: BytecodeExplorerChildrenArgs) -> Result<()> {
    let (settings_file, document, service) = resolve_bytecode_read_input(
        args.settings_file.as_deref(),
        args.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(args.fields.as_deref());
    let projection_data = BytecodeProjectionData::new(&document, &service, &settings_file);
    let path_segments =
        parse_bytecode_path_segments(args.selector.path_segments_json.as_deref(), &service)?;
    let path_ordinals =
        parse_bytecode_path_ordinals(&args.selector.path_ordinals_json, path_segments.is_some())?;
    let has_selector = bytecode_selector_specified(
        args.selector.index,
        args.selector.settings_id.as_deref(),
        args.selector.name.as_deref(),
        args.selector.class_name.as_deref(),
    );
    let parent_index = if let Some(path_segments) = path_segments.as_deref() {
        if has_selector {
            bail!("--path-segments-json cannot be combined with another selector");
        }
        document_instance_index_by_path_unique(&document, path_segments, &path_ordinals)?
    } else if !has_selector {
        editor_service_root_index(&document, &service)
            .ok_or_else(|| anyhow::anyhow!("No service root in settings bytecode"))?
    } else {
        let selector = bytecode_selector(
            args.selector.index,
            args.selector.settings_id.as_deref(),
            args.selector.name.as_deref(),
            args.selector.class_name.as_deref(),
        )?;
        instance_api::find_unique_instance_index(&document, selector)?
            .ok_or_else(|| anyhow::anyhow!("No matching parent instance"))?
    };
    let projection = projection_data.projection(&document, mode, fields.as_ref());
    let child_nodes = projection_data
        .children_by_parent
        .get(parent_index)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|child_index| {
            document.instances.get(*child_index)?;
            Some(projection.node(*child_index, false))
        })
        .collect::<Vec<_>>();

    let mut response = Map::new();
    insert_top_field(&mut response, mode, "settingsFile", json!(settings_file));
    insert_top_field(&mut response, mode, "service", Value::String(service));
    insert_top_field(
        &mut response,
        mode,
        "parent",
        projection.node(parent_index, true),
    );
    insert_top_field(&mut response, mode, "children", Value::Array(child_nodes));
    print_json_output(&Value::Object(response), args.pretty)
}

pub(super) struct BytecodeNodeProjection<'a> {
    pub document: &'a SettingsBytecode,
    pub children_by_parent: &'a [Vec<usize>],
    pub path_segments_by_index: &'a [Option<Vec<String>>],
    pub path_ordinals_by_index: &'a [Option<Vec<usize>>],
    pub source_paths_by_index: &'a [Option<PathBuf>],
    pub mode: OutputMode,
    pub fields: Option<&'a HashSet<String>>,
}

impl BytecodeNodeProjection<'_> {
    pub fn node(&self, index: usize, include_children: bool) -> Value {
        let instance = &self.document.instances[index];
        let mut node = Map::new();

        if should_include_node_field(self.mode, self.fields, "settingsId") {
            node.insert(
                node_output_key(self.mode, "settingsId").to_string(),
                Value::String(instance.settings_id.clone()),
            );
        }
        if should_include_node_field(self.mode, self.fields, "index") {
            node.insert(
                node_output_key(self.mode, "index").to_string(),
                Value::Number(serde_json::Number::from(index as u64)),
            );
        }
        if should_include_node_field(self.mode, self.fields, "name") {
            node.insert(
                node_output_key(self.mode, "name").to_string(),
                Value::String(instance.name.clone()),
            );
        }
        if should_include_node_field(self.mode, self.fields, "className") {
            node.insert(
                node_output_key(self.mode, "className").to_string(),
                Value::String(instance.class_name.clone()),
            );
        }
        if should_include_node_field(self.mode, self.fields, "parentId")
            && let Some(parent_id) = instance.parent_index.and_then(|parent_index| {
                self.document
                    .instances
                    .get(parent_index)
                    .map(|parent| parent.settings_id.clone())
            })
        {
            node.insert(
                node_output_key(self.mode, "parentId").to_string(),
                Value::String(parent_id),
            );
        }
        if should_include_node_field(self.mode, self.fields, "parentIndex") {
            node.insert(
                node_output_key(self.mode, "parentIndex").to_string(),
                instance
                    .parent_index
                    .map(|parent_index| {
                        Value::Number(serde_json::Number::from(parent_index as u64))
                    })
                    .unwrap_or(Value::Null),
            );
        }

        let children = self
            .children_by_parent
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if should_include_node_field(self.mode, self.fields, "childCount") {
            node.insert(
                node_output_key(self.mode, "childCount").to_string(),
                Value::Number(serde_json::Number::from(children.len() as u64)),
            );
        }
        if should_include_node_field(self.mode, self.fields, "hasPackageLink") {
            node.insert(
                node_output_key(self.mode, "hasPackageLink").to_string(),
                Value::Bool(has_direct_package_link_child(
                    self.document,
                    self.children_by_parent,
                    index,
                )),
            );
        }
        if include_children
            && !children.is_empty()
            && should_include_node_field(self.mode, self.fields, "children")
        {
            node.insert(
                node_output_key(self.mode, "children").to_string(),
                Value::Array(
                    children
                        .iter()
                        .filter_map(|child_index| {
                            self.document
                                .instances
                                .get(*child_index)
                                .map(|child| Value::String(child.settings_id.clone()))
                        })
                        .collect(),
                ),
            );
        }
        if should_include_node_field(self.mode, self.fields, "pathSegments")
            && let Some(Some(path_segments)) = self.path_segments_by_index.get(index)
        {
            node.insert(
                node_output_key(self.mode, "pathSegments").to_string(),
                Value::Array(
                    path_segments
                        .iter()
                        .map(|segment| Value::String(segment.clone()))
                        .collect(),
                ),
            );
        }
        if should_include_node_field(self.mode, self.fields, "pathOrdinals")
            && let Some(Some(path_ordinals)) = self.path_ordinals_by_index.get(index)
        {
            node.insert(
                node_output_key(self.mode, "pathOrdinals").to_string(),
                Value::Array(
                    path_ordinals
                        .iter()
                        .map(|ordinal| Value::Number(serde_json::Number::from(*ordinal as u64)))
                        .collect(),
                ),
            );
        }
        if should_include_node_field(self.mode, self.fields, "sourcePath")
            && let Some(Some(source_path)) = self.source_paths_by_index.get(index)
        {
            node.insert(
                node_output_key(self.mode, "sourcePath").to_string(),
                Value::String(source_path.to_string_lossy().to_string()),
            );
        }
        if let Some(properties) = filtered_record(
            &instance.properties,
            self.fields,
            matches!(self.mode, OutputMode::Full),
            false,
        ) {
            node.insert(
                node_output_key(self.mode, "properties").to_string(),
                properties,
            );
        }
        if let Some(attributes) = filtered_record(
            &instance.attributes,
            self.fields,
            matches!(self.mode, OutputMode::Full),
            true,
        ) {
            node.insert(
                node_output_key(self.mode, "attributes").to_string(),
                attributes,
            );
        }

        Value::Object(node)
    }
}

pub(super) fn bytecode_explorer_service(args: BytecodeExplorerServiceArgs) -> Result<()> {
    let (settings_file, document, service) = resolve_bytecode_read_input(
        args.settings_file.as_deref(),
        args.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(args.fields.as_deref());
    let projection_data = BytecodeProjectionData::new(&document, &service, &settings_file);
    let projection = projection_data.projection(&document, mode, fields.as_ref());

    let nodes = document
        .instances
        .iter()
        .enumerate()
        .map(|(index, _)| projection.node(index, true))
        .collect::<Vec<_>>();

    let root_ids = document
        .instances
        .iter()
        .filter(|instance| instance.parent_index.is_none())
        .map(|instance| Value::String(instance.settings_id.clone()))
        .collect::<Vec<_>>();

    let mut response = Map::new();
    insert_top_field(&mut response, mode, "settingsFile", json!(settings_file));
    insert_top_field(&mut response, mode, "service", Value::String(service));
    insert_top_field(&mut response, mode, "rootIds", Value::Array(root_ids));
    insert_top_field(&mut response, mode, "nodes", Value::Array(nodes));
    print_json_output(&Value::Object(response), args.pretty)
}

pub(super) fn editor_target_settings_ids(
    document: &SettingsBytecode,
    service: &str,
    prefix: &str,
) -> Vec<String> {
    let paths_by_index = build_editor_instance_paths(document, service);
    document
        .instances
        .iter()
        .enumerate()
        .filter_map(|(index, instance)| {
            if instance.parent_index.is_none() || !instance.settings_id.starts_with(prefix) {
                return None;
            }
            let path_info = paths_by_index.get(index)?.as_ref()?;
            if path_info.path_segments.len() <= 1
                || path_info
                    .path_segments
                    .first()
                    .is_none_or(|segment| segment != service)
            {
                return None;
            }
            Some(instance.settings_id.clone())
        })
        .collect()
}

pub(super) fn bytecode_editor_targets(args: BytecodeEditorTargetsArgs) -> Result<()> {
    let services = explorer_daemon_services(&args.src_root, &args.services)?;
    let mut paths = Vec::new();
    let mut target_ids = Vec::new();
    let mut services_out = Vec::new();
    let prefix = args.id_prefix;

    for service in services {
        let settings_file = service_settings_path(&args.src_root.join(&service));
        if !settings_file.exists() {
            continue;
        }
        let document = SettingsBytecode::read_file(&settings_file)
            .with_context(|| format!("Failed to read {}", settings_file.display()))?;
        let ids = editor_target_settings_ids(&document, &service, &prefix);
        if ids.is_empty() {
            continue;
        }

        paths.push(settings_file.to_string_lossy().to_string());
        target_ids.extend(ids.iter().cloned());
        services_out.push(json!({
            "service": service,
            "settingsFile": settings_file,
            "targetSettingsIds": ids,
        }));
    }

    println!(
        "{}",
        serde_json::to_string(&json!({
            "paths": paths,
            "targetSettingsIds": target_ids,
            "services": services_out,
        }))?
    );
    Ok(())
}

pub(super) fn bytecode_explorer_search(args: BytecodeExplorerSearchArgs) -> Result<()> {
    let (settings_file, document, service) = resolve_bytecode_read_input(
        args.settings_file.as_deref(),
        args.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(args.fields.as_deref());
    let projection_data = BytecodeProjectionData::new(&document, &service, &settings_file);
    let root_index = editor_service_root_index(&document, &service);
    let groups = explorer_search_groups(&args.query);
    let mut match_indices = Vec::new();
    let mut visible_indices = HashSet::new();

    if !groups.is_empty() {
        for index in 0..document.instances.len() {
            if explorer_search_instance_matches(
                &document,
                &projection_data.path_segments_by_index,
                index,
                &groups,
            ) {
                if args.limit > 0 && match_indices.len() >= args.limit {
                    break;
                }
                match_indices.push(index);
                let mut current = Some(index);
                while let Some(ancestor_index) = current {
                    if !visible_indices.insert(ancestor_index) {
                        break;
                    }
                    current = document.instances[ancestor_index].parent_index;
                }
            }
        }
    }
    if let Some(index) = root_index {
        visible_indices.insert(index);
    }
    let projection = projection_data.projection(&document, mode, fields.as_ref());

    let nodes = (0..document.instances.len())
        .filter(|index| visible_indices.contains(index))
        .map(|index| projection.search_node(index, &visible_indices))
        .collect::<Vec<_>>();
    let root_ids = root_index
        .and_then(|index| document.instances.get(index))
        .map(|instance| vec![Value::String(instance.settings_id.clone())])
        .unwrap_or_default();
    let match_ids = match_indices
        .iter()
        .filter_map(|index| document.instances.get(*index))
        .map(|instance| Value::String(instance.settings_id.clone()))
        .collect::<Vec<_>>();
    let visible_ids = visible_indices
        .iter()
        .filter_map(|index| document.instances.get(*index))
        .map(|instance| Value::String(instance.settings_id.clone()))
        .collect::<Vec<_>>();

    let mut response = Map::new();
    insert_top_field(&mut response, mode, "settingsFile", json!(settings_file));
    insert_top_field(&mut response, mode, "service", Value::String(service));
    insert_top_field(&mut response, mode, "rootIds", Value::Array(root_ids));
    insert_top_field(&mut response, mode, "matchIds", Value::Array(match_ids));
    insert_top_field(&mut response, mode, "visibleIds", Value::Array(visible_ids));
    insert_top_field(&mut response, mode, "nodes", Value::Array(nodes));
    print_json_output(&Value::Object(response), args.pretty)
}

impl BytecodeNodeProjection<'_> {
    pub fn search_node(&self, index: usize, visible_indices: &HashSet<usize>) -> Value {
        let mut node = self.node(index, false);
        let Some(map) = node.as_object_mut() else {
            return node;
        };
        let visible_children = self
            .children_by_parent
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(|child_index| visible_indices.contains(child_index))
            .filter_map(|child_index| {
                self.document
                    .instances
                    .get(*child_index)
                    .map(|child| Value::String(child.settings_id.clone()))
            })
            .collect::<Vec<_>>();
        if !visible_children.is_empty()
            && should_include_node_field(self.mode, self.fields, "children")
        {
            map.insert(
                node_output_key(self.mode, "children").to_string(),
                Value::Array(visible_children),
            );
        }
        node
    }
}

pub(super) fn explorer_search_groups(query: &str) -> Vec<Vec<String>> {
    let mut groups = vec![Vec::new()];
    for token in query.split_whitespace() {
        if token.eq_ignore_ascii_case("or") {
            groups.push(Vec::new());
        } else if !token.eq_ignore_ascii_case("and") {
            let trimmed = token.trim_matches(|c| c == '(' || c == ')');
            if !trimmed.is_empty() {
                groups
                    .last_mut()
                    .expect("search groups starts with one group")
                    .push(trimmed.to_string());
            }
        }
    }
    groups
        .into_iter()
        .filter(|group| !group.is_empty())
        .collect()
}

pub(super) fn explorer_search_instance_matches(
    document: &SettingsBytecode,
    path_segments_by_index: &[Option<Vec<String>>],
    index: usize,
    groups: &[Vec<String>],
) -> bool {
    groups.iter().any(|group| {
        let mut token_index = 0usize;
        while token_index < group.len() {
            let token = &group[token_index];
            let next = group.get(token_index + 1).map(String::as_str);
            let next_value = group.get(token_index + 2).map(String::as_str).unwrap_or("");
            if let Some(operator @ ("=" | "==" | "!=" | "~=" | "<" | ">" | "<=" | ">=")) = next {
                if !explorer_search_property_matches(document, index, token, operator, next_value) {
                    return false;
                }
                token_index += 3;
                continue;
            }
            if !explorer_search_token_matches(document, path_segments_by_index, index, token) {
                return false;
            }
            token_index += 1;
        }
        true
    })
}

fn explorer_search_token_matches(
    document: &SettingsBytecode,
    path_segments_by_index: &[Option<Vec<String>>],
    index: usize,
    token: &str,
) -> bool {
    if let Some((prefix, value)) = token.split_once(':') {
        if prefix.eq_ignore_ascii_case("is") {
            return explorer_search_compact(&document.instances[index].class_name)
                == explorer_search_compact(value);
        }
        if prefix.eq_ignore_ascii_case("tag") {
            return explorer_search_tags(&document.instances[index])
                .iter()
                .any(|tag| tag.contains(&explorer_search_compact(value)));
        }
    }
    for operator in ["==", "!=", "~=", "<=", ">=", "=", "<", ">"] {
        if let Some((property_name, expected)) = token.split_once(operator) {
            return explorer_search_property_matches(
                document,
                index,
                property_name,
                operator,
                expected,
            );
        }
    }
    if token.contains('.') || token == "*" || token == "**" {
        return explorer_search_path_matches(path_segments_by_index, index, token);
    }
    explorer_search_compact(&document.instances[index].name)
        .contains(&explorer_search_compact(token))
}

fn explorer_search_path_matches(
    path_segments_by_index: &[Option<Vec<String>>],
    index: usize,
    pattern: &str,
) -> bool {
    let Some(Some(path_segments)) = path_segments_by_index.get(index) else {
        return false;
    };
    let path = path_segments
        .iter()
        .map(|segment| explorer_search_compact(segment))
        .collect::<Vec<_>>();
    let parts = pattern
        .split('.')
        .filter(|part| !part.is_empty())
        .map(explorer_search_compact)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return false;
    }
    fn at(parts: &[String], path: &[String], part_index: usize, path_index: usize) -> bool {
        if part_index == parts.len() {
            return path_index == path.len();
        }
        if parts[part_index] == "**" {
            return true;
        }
        if path_index >= path.len() {
            return false;
        }
        if parts[part_index] == "*" || parts[part_index] == path[path_index] {
            return at(parts, path, part_index + 1, path_index + 1);
        }
        false
    }
    (0..path.len()).any(|start| at(&parts, &path, 0, start))
}

fn explorer_search_property_matches(
    document: &SettingsBytecode,
    index: usize,
    property_name: &str,
    operator: &str,
    expected: &str,
) -> bool {
    let Some(actual) = explorer_search_property_value(document, index, property_name) else {
        return false;
    };
    let actual_text = explorer_search_value_text(&actual);
    let actual_number = actual_text.parse::<f64>().ok();
    let expected_number = expected.parse::<f64>().ok();
    if matches!(operator, "<" | ">" | "<=" | ">=") {
        let (Some(actual_number), Some(expected_number)) = (actual_number, expected_number) else {
            return false;
        };
        return match operator {
            "<" => actual_number < expected_number,
            ">" => actual_number > expected_number,
            "<=" => actual_number <= expected_number,
            _ => actual_number >= expected_number,
        };
    }
    let actual_compact = explorer_search_compact(&actual_text);
    let expected_compact = explorer_search_compact(expected);
    if operator == "!=" || operator == "~=" {
        !actual_compact.contains(&expected_compact)
    } else {
        actual_compact.contains(&expected_compact)
    }
}

fn explorer_search_property_value(
    document: &SettingsBytecode,
    index: usize,
    property_name: &str,
) -> Option<Value> {
    let instance = document.instances.get(index)?;
    let wanted = explorer_search_compact(property_name);
    match wanted.as_str() {
        "name" => return Some(Value::String(instance.name.clone())),
        "classname" | "class" => return Some(Value::String(instance.class_name.clone())),
        "parent" => {
            return Some(
                instance
                    .parent_index
                    .and_then(|parent_index| document.instances.get(parent_index))
                    .map(|parent| Value::String(parent.name.clone()))
                    .unwrap_or(Value::Null),
            );
        }
        _ => {}
    }
    instance
        .properties
        .iter()
        .chain(instance.attributes.iter())
        .find_map(|(name, value)| (explorer_search_compact(name) == wanted).then(|| value.clone()))
}

fn explorer_search_tags(instance: &SettingsBytecodeInstance) -> Vec<String> {
    ["Tags", "tags"]
        .iter()
        .find_map(|name| {
            instance
                .properties
                .get(*name)
                .or_else(|| instance.attributes.get(*name))
        })
        .map(|value| match value {
            Value::Array(values) => values
                .iter()
                .map(explorer_search_value_text)
                .map(|text| explorer_search_compact(&text))
                .filter(|text| !text.is_empty())
                .collect(),
            _ => {
                let text = explorer_search_compact(&explorer_search_value_text(value));
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![text]
                }
            }
        })
        .unwrap_or_default()
}

fn explorer_search_value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Object(map) => map
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default()),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn explorer_search_compact(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplorerViewMode {
    Normal,
    Search,
}

struct ExplorerRowWindow {
    mode: ExplorerViewMode,
    start: usize,
    end: usize,
    rows: Vec<Value>,
    index: usize,
}

impl ExplorerRowWindow {
    fn new(mode: ExplorerViewMode, start: usize, count: usize) -> Self {
        Self {
            mode,
            start,
            end: start.saturating_add(count),
            rows: Vec::with_capacity(count.min(512)),
            index: 0,
        }
    }

    fn includes_current(&self) -> bool {
        self.index >= self.start && self.index < self.end
    }
}

impl ExplorerViewMode {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("search") => Self::Search,
            _ => Self::Normal,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Search => "search",
        }
    }
}

#[derive(Debug, Clone)]
struct ExplorerSearchView {
    search_id: u64,
    query: String,
    visible_by_service: HashMap<String, HashSet<usize>>,
    matches_by_service: HashMap<String, HashSet<usize>>,
    match_ids: Vec<String>,
    match_count: usize,
}

#[derive(Debug, Clone)]
struct ExplorerServiceState {
    service: String,
    document: Option<SettingsBytecode>,
    children_by_parent: Vec<Vec<usize>>,
    path_segments_by_index: Vec<Option<Vec<String>>>,
    path_ordinals_by_index: Vec<Option<Vec<usize>>>,
    source_paths_by_index: Vec<Option<PathBuf>>,
    settings_files_by_index: Vec<Option<PathBuf>>,
    canonical_settings_ids_by_index: Vec<Option<String>>,
    index_by_settings_id: HashMap<String, usize>,
    root_index: Option<usize>,
    name_search_text: Vec<String>,
}

#[derive(Debug)]
struct ExplorerDaemonState {
    project_root: PathBuf,
    src_root: PathBuf,
    services: Vec<String>,
    service_states: HashMap<String, ExplorerServiceState>,
    expanded: HashSet<String>,
    search_collapsed: HashSet<String>,
    search: Option<ExplorerSearchView>,
    snapshot_version: u64,
    view_version: u64,
}

impl ExplorerDaemonState {
    fn new(mut args: ExplorerDaemonArgs) -> Result<Self> {
        apply_configured_project_layout(&mut args.project_root, &mut args.src_dir)?;
        let project_root = args.project_root;
        let src_root = absolutize_under(&project_root, &args.src_dir);
        let services = explorer_daemon_services(&src_root, &args.services)?;
        Ok(Self {
            project_root,
            src_root,
            services,
            service_states: HashMap::new(),
            expanded: HashSet::new(),
            search_collapsed: HashSet::new(),
            search: None,
            snapshot_version: 0,
            view_version: 0,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        let services = self.services.clone();
        self.reload_services(&services)?;
        Ok(())
    }

    fn reload_services(&mut self, services: &[String]) -> Result<()> {
        let loaded = project_config::try_load_project(None, Some(&self.project_root))?;
        let changed_sources = loaded
            .as_ref()
            .filter(|_| !services.is_empty())
            .map(|loaded| {
                let mut paths = Vec::new();
                for requested in services {
                    if let Some((_, state)) = self
                        .service_states
                        .iter()
                        .find(|(service, _)| service.eq_ignore_ascii_case(requested))
                    {
                        paths.extend(state.settings_files_by_index.iter().flatten().cloned());
                        paths.extend(state.source_paths_by_index.iter().flatten().cloned());
                    }
                    paths.push(
                        loaded
                            .root
                            .join(&loaded.project.source_root)
                            .join(canonical_explorer_service_name(requested)),
                    );
                }
                paths.sort();
                paths.dedup();
                paths
            })
            .unwrap_or_default();
        let projection = loaded
            .as_ref()
            .map(|loaded| project_config::stage_project_cached(loaded, &changed_sources))
            .transpose()?;
        let active_root = projection
            .as_ref()
            .map(project_config::ProjectionStage::root)
            .unwrap_or(&self.src_root);
        let requested = if services.is_empty() {
            explorer_daemon_services(active_root, "")?
        } else {
            services.to_vec()
        };
        let mut reload_list = Vec::with_capacity(requested.len());
        for service in requested {
            let canonical = self
                .services
                .iter()
                .find(|existing| existing.eq_ignore_ascii_case(&service))
                .cloned()
                .unwrap_or_else(|| canonical_explorer_service_name(&service));
            if !reload_list
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(&canonical))
            {
                reload_list.push(canonical);
            }
        }
        for service in &reload_list {
            if !self
                .services
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(service))
            {
                self.services.push(service.clone());
            }
            match ExplorerServiceState::load(active_root, service) {
                Ok(mut state) => {
                    if let (Some(loaded), Some(projection)) = (loaded.as_ref(), projection.as_ref())
                    {
                        for source in &mut state.source_paths_by_index {
                            let Some(path) = source.as_ref() else {
                                continue;
                            };
                            let Ok(relative) = path.strip_prefix(projection.root()) else {
                                *source = None;
                                continue;
                            };
                            *source =
                                project_config::staged_path_to_project_source(loaded, relative)
                                    .ok()
                                    .flatten();
                        }
                        for settings_file in &mut state.settings_files_by_index {
                            let Some(path) = settings_file.as_ref() else {
                                continue;
                            };
                            let Ok(relative) = path.strip_prefix(projection.root()) else {
                                *settings_file = None;
                                continue;
                            };
                            *settings_file =
                                project_config::project_staged_path_to_source(loaded, relative)
                                    .ok();
                        }
                        if let Some(document) = state.document.as_ref() {
                            for (index, instance) in document.instances.iter().enumerate() {
                                if let Some((settings_file, settings_id)) =
                                    projection.canonical_identity(&instance.settings_id)
                                {
                                    state.settings_files_by_index[index] =
                                        Some(settings_file.to_path_buf());
                                    state.canonical_settings_ids_by_index[index] =
                                        Some(settings_id.to_string());
                                }
                            }
                        }
                    }
                    self.service_states.insert(service.clone(), state);
                }
                Err(error) => {
                    eprintln!("[renium] explorer-daemon: failed to load {service}: {error:?}");
                    self.service_states.insert(
                        service.clone(),
                        ExplorerServiceState::empty(&self.src_root, service),
                    );
                }
            }
        }
        self.sort_services();
        self.snapshot_version += 1;
        self.view_version += 1;
        if let Some(search) = self.search.clone() {
            self.search = Some(self.build_search_view(search.search_id, &search.query));
        }
        Ok(())
    }

    fn sort_services(&mut self) {
        self.services
            .sort_by(|a, b| explorer_compare_nodes(a, a, b, b));
        let mut seen = HashSet::new();
        self.services
            .retain(|service| seen.insert(service.to_ascii_lowercase()));
    }

    fn total_rows(&self, mode: ExplorerViewMode) -> usize {
        self.collect_rows_window(mode, 0, 0).1
    }

    fn rows_window(
        &self,
        request_id: u64,
        mode: ExplorerViewMode,
        start: usize,
        count: usize,
        include_match_ids: bool,
    ) -> Value {
        let started = Instant::now();
        let (window, total_rows) = self.collect_rows_window(mode, start, count);
        let safe_start = start.min(total_rows);
        let match_ids = if mode == ExplorerViewMode::Search && include_match_ids {
            self.search
                .as_ref()
                .map(|search| json!(&search.match_ids))
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let match_count = if mode == ExplorerViewMode::Search {
            self.search
                .as_ref()
                .map(|search| search.match_count)
                .unwrap_or_default()
        } else {
            0
        };
        json!({
            "type": "rowsWindow",
            "requestId": request_id,
            "snapshotVersion": self.snapshot_version,
            "viewVersion": self.view_version,
            "mode": mode.as_str(),
            "start": safe_start,
            "totalRows": total_rows,
            "rows": window,
            "matchIds": match_ids,
            "matchCount": match_count,
            "metrics": {
                "backendMs": elapsed_ms(started),
                "rowCount": window.len(),
            },
        })
    }

    fn expand(&mut self, node_id: &str, mode: ExplorerViewMode) {
        match mode {
            ExplorerViewMode::Search => {
                self.search_collapsed.remove(node_id);
            }
            ExplorerViewMode::Normal => {
                self.expanded.insert(node_id.to_string());
            }
        }
        self.view_version += 1;
    }

    fn collapse(&mut self, node_id: &str, mode: ExplorerViewMode) {
        match mode {
            ExplorerViewMode::Search => {
                self.search_collapsed.insert(node_id.to_string());
            }
            ExplorerViewMode::Normal => {
                self.expanded.remove(node_id);
            }
        }
        self.view_version += 1;
    }

    fn clear_search(&mut self) {
        self.search = None;
        self.search_collapsed.clear();
        self.view_version += 1;
    }

    fn start_search(&mut self, request_id: u64, search_id: u64, query: &str) -> Value {
        let started = Instant::now();
        self.search = Some(self.build_search_view(search_id, query));
        self.search_collapsed.clear();
        self.view_version += 1;
        let match_count = self
            .search
            .as_ref()
            .map(|search| search.match_count)
            .unwrap_or(0);
        json!({
            "type": "searchStatus",
            "requestId": request_id,
            "searchId": search_id,
            "state": "complete",
            "loaded": self.services.len(),
            "total": self.services.len(),
            "matchCount": match_count,
            "metrics": {
                "backendMs": elapsed_ms(started),
            },
        })
    }

    fn build_search_view(&self, search_id: u64, query: &str) -> ExplorerSearchView {
        let groups = explorer_search_groups(query);
        let fast_name_groups = explorer_search_fast_name_groups(&groups);
        let mut visible_by_service: HashMap<String, HashSet<usize>> = HashMap::new();
        let mut matches_by_service: HashMap<String, HashSet<usize>> = HashMap::new();
        let mut match_ids = Vec::new();
        if !groups.is_empty() {
            for service in &self.services {
                let Some(state) = self.service_states.get(service) else {
                    continue;
                };
                let Some(document) = state.document.as_ref() else {
                    continue;
                };
                for index in 0..document.instances.len() {
                    let matches = if let Some(fast_groups) = fast_name_groups.as_ref() {
                        explorer_search_fast_name_matches(
                            state
                                .name_search_text
                                .get(index)
                                .map(String::as_str)
                                .unwrap_or(""),
                            fast_groups,
                        )
                    } else {
                        explorer_search_instance_matches(
                            document,
                            &state.path_segments_by_index,
                            index,
                            &groups,
                        )
                    };
                    if !matches {
                        continue;
                    }
                    matches_by_service
                        .entry(service.clone())
                        .or_default()
                        .insert(index);
                    if let Some(instance) = document.instances.get(index) {
                        match_ids.push(explorer_instance_tree_id(service, &instance.settings_id));
                    }
                    let mut current = Some(index);
                    while let Some(ancestor_index) = current {
                        visible_by_service
                            .entry(service.clone())
                            .or_default()
                            .insert(ancestor_index);
                        current = document.instances[ancestor_index].parent_index;
                    }
                }
            }
        }
        let match_count = match_ids.len();
        ExplorerSearchView {
            search_id,
            query: query.to_string(),
            visible_by_service,
            matches_by_service,
            match_ids,
            match_count,
        }
    }

    fn details(&self, request_id: u64, node_id: &str) -> Value {
        let Some((service, index)) = self.resolve_node_index(node_id) else {
            return explorer_error(request_id, "not_found", "Node not found");
        };
        let Some(state) = self.service_states.get(&service) else {
            return explorer_error(request_id, "not_found", "Service not found");
        };
        let Some(document) = state.document.as_ref() else {
            return explorer_error(request_id, "not_found", "Service settings are not loaded");
        };
        let Some(instance) = document.instances.get(index) else {
            return explorer_error(request_id, "not_found", "Instance not found");
        };
        let parent_id = explorer_parent_tree_id(&service, document, state.root_index, index);
        let path_segments = state
            .path_segments_by_index
            .get(index)
            .and_then(|segments| segments.clone())
            .unwrap_or_else(|| vec![instance.name.clone()]);
        let path_ordinals = state
            .path_ordinals_by_index
            .get(index)
            .and_then(|ordinals| ordinals.clone())
            .unwrap_or_default();
        let source_path = state
            .source_paths_by_index
            .get(index)
            .and_then(|path| path.as_ref())
            .map(|path| path.to_string_lossy().to_string());
        let settings_file = state
            .settings_files_by_index
            .get(index)
            .and_then(|path| path.as_ref())
            .map(|path| path.to_string_lossy().to_string());
        let settings_id = state
            .canonical_settings_ids_by_index
            .get(index)
            .and_then(|settings_id| settings_id.as_deref())
            .unwrap_or(&instance.settings_id);
        json!({
            "type": "details",
            "requestId": request_id,
            "snapshotVersion": self.snapshot_version,
            "nodeId": node_id,
            "details": {
                "id": node_id,
                "settingsId": settings_id,
                "settingsFile": settings_file,
                "index": index,
                "kind": if Some(index) == state.root_index { "service" } else { "instance" },
                "service": service,
                "name": instance.name,
                "className": instance.class_name,
                "parentId": parent_id,
                "pathSegments": path_segments,
                "pathOrdinals": path_ordinals,
                "sourcePath": source_path,
                "properties": instance.properties,
                "attributes": instance.attributes,
            },
        })
    }

    fn resolve_node_index(&self, node_id: &str) -> Option<(String, usize)> {
        if let Some(service) = node_id.strip_prefix("service:") {
            let state = self.service_states.get(service)?;
            return state.root_index.map(|index| (service.to_string(), index));
        }
        let (service, settings_id) = node_id.split_once(':')?;
        let state = self.service_states.get(service)?;
        state
            .index_by_settings_id
            .get(settings_id)
            .copied()
            .map(|index| (service.to_string(), index))
    }

    fn collect_rows_window(
        &self,
        mode: ExplorerViewMode,
        start: usize,
        count: usize,
    ) -> (Vec<Value>, usize) {
        let mut window = ExplorerRowWindow::new(mode, start, count);
        for service in &self.services {
            let state = self.service_states.get(service);
            match mode {
                ExplorerViewMode::Search => {
                    let Some(search) = self.search.as_ref() else {
                        continue;
                    };
                    if !search.visible_by_service.contains_key(service) {
                        continue;
                    }
                    if window.includes_current() {
                        self.push_service_row(&mut window.rows, service, state, 0, mode);
                    }
                    window.index += 1;
                    let service_id = explorer_service_tree_id(service);
                    if !self.search_collapsed.contains(&service_id)
                        && let Some(state) = state
                        && let Some(root_index) = state.root_index
                    {
                        self.collect_instance_rows_window(state, root_index, 1, &mut window);
                    }
                }
                ExplorerViewMode::Normal => {
                    if window.includes_current() {
                        self.push_service_row(&mut window.rows, service, state, 0, mode);
                    }
                    window.index += 1;
                    let service_id = explorer_service_tree_id(service);
                    if self.expanded.contains(&service_id)
                        && let Some(state) = state
                        && let Some(root_index) = state.root_index
                    {
                        self.collect_instance_rows_window(state, root_index, 1, &mut window);
                    }
                }
            }
        }
        (window.rows, window.index)
    }

    fn collect_instance_rows_window(
        &self,
        state: &ExplorerServiceState,
        parent_index: usize,
        depth: usize,
        window: &mut ExplorerRowWindow,
    ) {
        let children = state
            .children_by_parent
            .get(parent_index)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for child_index in children {
            if window.mode == ExplorerViewMode::Search
                && !self.search_contains(&state.service, *child_index)
            {
                continue;
            }
            if window.includes_current() {
                self.push_instance_row(&mut window.rows, state, *child_index, depth, window.mode);
            }
            window.index += 1;
            let node_id = state
                .document
                .as_ref()
                .and_then(|document| document.instances.get(*child_index))
                .map(|instance| explorer_instance_tree_id(&state.service, &instance.settings_id));
            let is_open = match (window.mode, node_id.as_deref()) {
                (ExplorerViewMode::Search, Some(node_id)) => {
                    !self.search_collapsed.contains(node_id)
                }
                (ExplorerViewMode::Normal, Some(node_id)) => self.expanded.contains(node_id),
                _ => false,
            };
            if is_open {
                self.collect_instance_rows_window(state, *child_index, depth + 1, window);
            }
        }
    }

    fn collect_rows(&self, mode: ExplorerViewMode) -> Vec<Value> {
        let mut rows = Vec::new();
        for service in &self.services {
            let state = self.service_states.get(service);
            match mode {
                ExplorerViewMode::Search => {
                    let Some(search) = self.search.as_ref() else {
                        continue;
                    };
                    if !search.visible_by_service.contains_key(service) {
                        continue;
                    }
                    self.push_service_row(&mut rows, service, state, 0, mode);
                    let service_id = explorer_service_tree_id(service);
                    if !self.search_collapsed.contains(&service_id)
                        && let Some(state) = state
                        && let Some(root_index) = state.root_index
                    {
                        self.collect_instance_rows(state, root_index, 1, mode, &mut rows);
                    }
                }
                ExplorerViewMode::Normal => {
                    self.push_service_row(&mut rows, service, state, 0, mode);
                    let service_id = explorer_service_tree_id(service);
                    if self.expanded.contains(&service_id)
                        && let Some(state) = state
                        && let Some(root_index) = state.root_index
                    {
                        self.collect_instance_rows(state, root_index, 1, mode, &mut rows);
                    }
                }
            }
        }
        rows
    }

    fn push_service_row(
        &self,
        rows: &mut Vec<Value>,
        service: &str,
        state: Option<&ExplorerServiceState>,
        depth: usize,
        mode: ExplorerViewMode,
    ) {
        let id = explorer_service_tree_id(service);
        let (settings_id, settings_file, index, child_count, matched) = if let Some(state) = state {
            let root_index = state.root_index;
            let settings_id = root_index.and_then(|root_index| {
                state
                    .canonical_settings_ids_by_index
                    .get(root_index)
                    .and_then(|settings_id| settings_id.clone())
            });
            let settings_file = root_index.and_then(|root_index| {
                state
                    .settings_files_by_index
                    .get(root_index)
                    .and_then(|path| path.as_ref())
                    .map(|path| path.to_string_lossy().to_string())
            });
            let child_count = root_index
                .map(|root_index| self.visible_child_count(state, root_index, mode))
                .unwrap_or(0);
            let matched = self
                .search
                .as_ref()
                .and_then(|search| search.matches_by_service.get(service))
                .zip(root_index)
                .map(|(matches, root_index)| matches.contains(&root_index))
                .unwrap_or(false);
            (settings_id, settings_file, root_index, child_count, matched)
        } else {
            (None, None, None, 0, false)
        };
        let expanded = match mode {
            ExplorerViewMode::Search => child_count > 0 && !self.search_collapsed.contains(&id),
            ExplorerViewMode::Normal => child_count > 0 && self.expanded.contains(&id),
        };
        rows.push(json!({
            "id": id,
            "settingsId": settings_id,
            "settingsFile": settings_file,
            "index": index,
            "kind": "service",
            "service": service,
            "name": service,
            "className": service,
            "parentId": Value::Null,
            "pathSegments": [service],
            "pathOrdinals": [1],
            "depth": depth,
            "hasChildren": child_count > 0,
            "childCount": child_count,
            "expanded": expanded,
            "matched": matched,
            "iconName": icon_asset_name_for_class_rs(service),
            "isScript": false,
            "disabled": false,
            "locked": true,
            "canRename": true,
            "canMove": false,
            "canDelete": false,
        }));
    }

    fn collect_instance_rows(
        &self,
        state: &ExplorerServiceState,
        parent_index: usize,
        depth: usize,
        mode: ExplorerViewMode,
        rows: &mut Vec<Value>,
    ) {
        let children = state
            .children_by_parent
            .get(parent_index)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for child_index in children {
            if mode == ExplorerViewMode::Search
                && !self.search_contains(&state.service, *child_index)
            {
                continue;
            }
            self.push_instance_row(rows, state, *child_index, depth, mode);
            let node_id = state
                .document
                .as_ref()
                .and_then(|document| document.instances.get(*child_index))
                .map(|instance| explorer_instance_tree_id(&state.service, &instance.settings_id));
            let is_open = match (mode, node_id.as_deref()) {
                (ExplorerViewMode::Search, Some(node_id)) => {
                    !self.search_collapsed.contains(node_id)
                }
                (ExplorerViewMode::Normal, Some(node_id)) => self.expanded.contains(node_id),
                _ => false,
            };
            if is_open {
                self.collect_instance_rows(state, *child_index, depth + 1, mode, rows);
            }
        }
    }

    fn push_instance_row(
        &self,
        rows: &mut Vec<Value>,
        state: &ExplorerServiceState,
        index: usize,
        depth: usize,
        mode: ExplorerViewMode,
    ) {
        let Some(document) = state.document.as_ref() else {
            return;
        };
        let Some(instance) = document.instances.get(index) else {
            return;
        };
        let id = explorer_instance_tree_id(&state.service, &instance.settings_id);
        let child_count = self.visible_child_count(state, index, mode);
        let expanded = match mode {
            ExplorerViewMode::Search => child_count > 0 && !self.search_collapsed.contains(&id),
            ExplorerViewMode::Normal => child_count > 0 && self.expanded.contains(&id),
        };
        let matched = self
            .search
            .as_ref()
            .and_then(|search| search.matches_by_service.get(&state.service))
            .map(|matches| matches.contains(&index))
            .unwrap_or(false);
        let disabled = matches!(instance.class_name.as_str(), "Script" | "LocalScript")
            && instance
                .properties
                .get("Disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let parent_id = explorer_parent_tree_id(&state.service, document, state.root_index, index);
        let path_segments = state
            .path_segments_by_index
            .get(index)
            .and_then(|segments| segments.clone())
            .unwrap_or_else(|| vec![state.service.clone(), instance.name.clone()]);
        let path_ordinals = state
            .path_ordinals_by_index
            .get(index)
            .and_then(|ordinals| ordinals.clone())
            .unwrap_or_default();
        let has_package_link =
            has_direct_package_link_child(document, &state.children_by_parent, index);
        let settings_id = state
            .canonical_settings_ids_by_index
            .get(index)
            .and_then(|settings_id| settings_id.as_deref())
            .unwrap_or(&instance.settings_id);
        let settings_file = state
            .settings_files_by_index
            .get(index)
            .and_then(|path| path.as_ref())
            .map(|path| path.to_string_lossy().to_string());
        rows.push(json!({
            "id": id,
            "settingsId": settings_id,
            "settingsFile": settings_file,
            "index": index,
            "kind": "instance",
            "service": state.service,
            "name": instance.name,
            "className": instance.class_name,
            "parentId": parent_id,
            "pathSegments": path_segments,
            "pathOrdinals": path_ordinals,
            "depth": depth,
            "hasChildren": child_count > 0,
            "childCount": child_count,
            "hasPackageLink": has_package_link,
            "expanded": expanded,
            "matched": matched,
            "iconName": icon_asset_name_for_class_rs(&instance.class_name),
            "isScript": matches!(instance.class_name.as_str(), "Script" | "LocalScript" | "ModuleScript"),
            "disabled": disabled,
            "locked": false,
            "canRename": true,
            "canMove": !is_protected_starter_player_container(document, index),
            "canDelete": !is_protected_starter_player_container(document, index),
        }));
    }

    fn search_contains(&self, service: &str, index: usize) -> bool {
        self.search
            .as_ref()
            .and_then(|search| search.visible_by_service.get(service))
            .map(|visible| visible.contains(&index))
            .unwrap_or(false)
    }

    fn visible_child_count(
        &self,
        state: &ExplorerServiceState,
        index: usize,
        mode: ExplorerViewMode,
    ) -> usize {
        let children = state
            .children_by_parent
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match mode {
            ExplorerViewMode::Normal => children.len(),
            ExplorerViewMode::Search => children
                .iter()
                .filter(|child_index| self.search_contains(&state.service, **child_index))
                .count(),
        }
    }
}

impl ExplorerServiceState {
    fn empty(_src_root: &Path, service: &str) -> Self {
        Self {
            service: service.to_string(),
            document: None,
            children_by_parent: Vec::new(),
            path_segments_by_index: Vec::new(),
            path_ordinals_by_index: Vec::new(),
            source_paths_by_index: Vec::new(),
            settings_files_by_index: Vec::new(),
            canonical_settings_ids_by_index: Vec::new(),
            index_by_settings_id: HashMap::new(),
            root_index: None,
            name_search_text: Vec::new(),
        }
    }

    fn load(src_root: &Path, service: &str) -> Result<Self> {
        validate_filesystem_instance_name(service, "service")?;
        let service_dir = src_root.join(service);
        let settings_file = service_settings_path(&service_dir);
        if !settings_file.exists() {
            return Ok(Self::empty(src_root, service));
        }
        let document = SettingsBytecode::read_file(&settings_file)?;
        let mut children_by_parent = settings_children_by_parent(&document);
        let lower_names = document
            .instances
            .iter()
            .map(|instance| instance.name.to_lowercase())
            .collect::<Vec<_>>();
        for children in &mut children_by_parent {
            children.sort_unstable_by(|a, b| {
                explorer_compare_ranked(
                    &document.instances[*a].class_name,
                    &lower_names[*a],
                    &document.instances[*b].class_name,
                    &lower_names[*b],
                )
                .then_with(|| a.cmp(b))
            });
        }
        let paths_by_index = build_editor_instance_paths(&document, service);
        let path_segments_by_index = paths_by_index
            .iter()
            .map(|path| path.as_ref().map(|path| path.path_segments.clone()))
            .collect::<Vec<_>>();
        let path_ordinals_by_index = paths_by_index
            .iter()
            .map(|path| path.as_ref().map(|path| path.path_ordinals.clone()))
            .collect::<Vec<_>>();
        let source_paths_by_index =
            build_editor_source_paths_by_index(&document, service, &service_dir);
        let settings_files_by_index = vec![Some(settings_file); document.instances.len()];
        let canonical_settings_ids_by_index = document
            .instances
            .iter()
            .map(|instance| Some(instance.settings_id.clone()))
            .collect();
        let index_by_settings_id = document
            .instances
            .iter()
            .enumerate()
            .map(|(index, instance)| (instance.settings_id.clone(), index))
            .collect::<HashMap<_, _>>();
        let root_index = editor_service_root_index(&document, service);
        let name_search_text = document
            .instances
            .iter()
            .map(|instance| explorer_search_compact(&instance.name))
            .collect();
        Ok(Self {
            service: service.to_string(),
            document: Some(document),
            children_by_parent,
            path_segments_by_index,
            path_ordinals_by_index,
            source_paths_by_index,
            settings_files_by_index,
            canonical_settings_ids_by_index,
            index_by_settings_id,
            root_index,
            name_search_text,
        })
    }
}

pub(super) fn explorer_daemon_services(src_root: &Path, raw: &str) -> Result<Vec<String>> {
    let requested = if raw.trim().is_empty() {
        DEFAULT_SYNC_SERVICES
            .iter()
            .map(|&service| service.to_owned())
            .collect::<Vec<_>>()
    } else {
        parse_services(raw)?
    };
    let mut services = Vec::with_capacity(requested.len());
    for service in requested {
        push_explorer_service(&mut services, &service);
    }
    if src_root.exists() {
        for entry in fs::read_dir(src_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let service = entry.file_name().to_string_lossy().to_string();
                push_explorer_service(&mut services, &service);
            }
        }
    }
    for service in &services {
        validate_filesystem_instance_name(service, "service")?;
    }
    services.sort_by(|a, b| explorer_compare_nodes(a, a, b, b));
    services.dedup();
    Ok(services)
}

fn canonical_explorer_service_name(service: &str) -> String {
    DEFAULT_SYNC_SERVICES
        .iter()
        .chain(EXTRA_EXPLORER_SERVICES.iter())
        .find(|candidate| candidate.eq_ignore_ascii_case(service))
        .copied()
        .unwrap_or(service)
        .to_string()
}

fn push_explorer_service(services: &mut Vec<String>, service: &str) {
    let canonical = canonical_explorer_service_name(service);
    if !services
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&canonical))
    {
        services.push(canonical);
    }
}

fn explorer_compare_nodes(
    a_class: &str,
    a_name: &str,
    b_class: &str,
    b_name: &str,
) -> std::cmp::Ordering {
    let a_service_order = explorer_service_order(a_class);
    let b_service_order = explorer_service_order(b_class);
    match (a_service_order, b_service_order) {
        (Some(a), Some(b)) => return a.cmp(&b),
        (Some(_), None) => return std::cmp::Ordering::Less,
        (None, Some(_)) => return std::cmp::Ordering::Greater,
        _ => {}
    }
    explorer_class_rank(a_class)
        .cmp(&explorer_class_rank(b_class))
        .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
}

fn explorer_compare_ranked(
    a_class: &str,
    a_name_lower: &str,
    b_class: &str,
    b_name_lower: &str,
) -> std::cmp::Ordering {
    let a_service_order = explorer_service_order(a_class);
    let b_service_order = explorer_service_order(b_class);
    match (a_service_order, b_service_order) {
        (Some(a), Some(b)) => return a.cmp(&b),
        (Some(_), None) => return std::cmp::Ordering::Less,
        (None, Some(_)) => return std::cmp::Ordering::Greater,
        _ => {}
    }
    explorer_class_rank(a_class)
        .cmp(&explorer_class_rank(b_class))
        .then_with(|| a_name_lower.cmp(b_name_lower))
}

fn explorer_class_rank(class_name: &str) -> usize {
    match class_name {
        "PackageLink" => 0,
        "Camera" => 1,
        "Terrain" => 2,
        "Folder" => 3,
        "SpawnLocation" => 4,
        _ => 5,
    }
}

fn explorer_service_tree_id(service: &str) -> String {
    format!("service:{service}")
}

fn explorer_instance_tree_id(service: &str, settings_id: &str) -> String {
    format!("{service}:{settings_id}")
}

fn explorer_parent_tree_id(
    service: &str,
    document: &SettingsBytecode,
    root_index: Option<usize>,
    index: usize,
) -> Value {
    let Some(instance) = document.instances.get(index) else {
        return Value::Null;
    };
    let Some(parent_index) = instance.parent_index else {
        return Value::String(explorer_service_tree_id(service));
    };
    if Some(parent_index) == root_index {
        return Value::String(explorer_service_tree_id(service));
    }
    document
        .instances
        .get(parent_index)
        .map(|parent| Value::String(explorer_instance_tree_id(service, &parent.settings_id)))
        .unwrap_or_else(|| Value::String(explorer_service_tree_id(service)))
}

fn icon_asset_name_for_class_rs(class_name: &str) -> &str {
    match class_name {
        "BinaryStringValue"
        | "Color3Value"
        | "DoubleConstrainedValue"
        | "IntConstrainedValue"
        | "IntValue"
        | "NumberValue"
        | "ObjectValue"
        | "StringValue"
        | "Vector3Value" => "Value",
        _ => class_name,
    }
}

fn explorer_search_fast_name_groups(groups: &[Vec<String>]) -> Option<Vec<Vec<String>>> {
    if groups.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for group in groups {
        let mut terms = Vec::new();
        for token in group {
            if token.contains(':')
                || token.contains('.')
                || matches!(token.as_str(), "*" | "**")
                || token
                    .chars()
                    .any(|ch| matches!(ch, '=' | '!' | '<' | '>' | '~'))
            {
                return None;
            }
            terms.push(explorer_search_compact(token));
        }
        if terms.is_empty() {
            return None;
        }
        out.push(terms);
    }
    Some(out)
}

fn explorer_search_fast_name_matches(name: &str, groups: &[Vec<String>]) -> bool {
    groups
        .iter()
        .any(|group| group.iter().all(|term| name.contains(term)))
}

fn explorer_error(request_id: u64, code: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "requestId": request_id,
        "code": code,
        "message": message,
    })
}

fn explorer_request_id(request: &Value) -> u64 {
    request
        .get("requestId")
        .or_else(|| request.get("id"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn normalize_explorer_request_type(raw: &str) -> &str {
    match raw {
        "init" => "initialize",
        "rows" => "getRows",
        "exp" => "expand",
        "col" => "collapse",
        "det" => "selectDetails",
        "ss" => "searchStart",
        "sr" => "searchRows",
        "cs" => "clearSearch",
        "rl" | "reload" => "reloadServices",
        "rv" | "reveal" => "revealNode",
        "quit" => "shutdown",
        other => other,
    }
}

fn explorer_request_type(request: &Value) -> &str {
    normalize_explorer_request_type(
        request
            .get("type")
            .or_else(|| request.get("command"))
            .or_else(|| request.get("t"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    )
}

fn explorer_str<'a>(request: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| request.get(*key).and_then(Value::as_str))
}

fn explorer_u64(request: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| request.get(*key).and_then(Value::as_u64))
}

fn explorer_bool(request: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| request.get(*key).and_then(Value::as_bool))
}

fn explorer_array<'a>(request: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    keys.iter()
        .find_map(|key| request.get(*key).and_then(Value::as_array))
}

pub(super) fn watch_parent_and_exit(parent_pid: u32) {
    std::thread::spawn(move || {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
            };
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_pid);
            if !handle.is_null() {
                WaitForSingleObject(handle, INFINITE);
                CloseHandle(handle);
            }
            std::process::exit(0);
        }
        #[cfg(not(windows))]
        loop {
            let alive = unsafe { libc::kill(parent_pid as libc::pid_t, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !alive {
                std::process::exit(0);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

pub(super) fn explorer_daemon(args: ExplorerDaemonArgs) -> Result<()> {
    if let Some(parent_pid) = args.parent_pid {
        watch_parent_and_exit(parent_pid);
    }
    let mut state = ExplorerDaemonState::new(args)?;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = BufWriter::new(io::stdout());
    let mut line = String::new();
    loop {
        match read_bounded_line(&mut stdin, &mut line, MAX_DAEMON_LINE_BYTES)? {
            BoundedLineRead::Eof => break,
            BoundedLineRead::Line => {}
            BoundedLineRead::TooLong => {
                writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&explorer_error(
                        0,
                        "request_too_large",
                        &format!("Explorer request exceeds {MAX_DAEMON_LINE_BYTES} bytes"),
                    ))?
                )?;
                stdout.flush()?;
                continue;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&explorer_error(0, "bad_json", &error.to_string()))?
                )?;
                stdout.flush()?;
                continue;
            }
        };
        let request_id = explorer_request_id(&request);
        let response = match explorer_request_type(&request) {
            "initialize" => match state.initialize() {
                Ok(()) => json!({
                    "type": "ready",
                    "requestId": request_id,
                    "snapshotVersion": state.snapshot_version,
                    "viewVersion": state.view_version,
                    "totalRows": state.total_rows(ExplorerViewMode::Normal),
                    "projectRoot": state.project_root,
                }),
                Err(error) => explorer_error(request_id, "initialize_failed", &error.to_string()),
            },
            "getRows" => {
                let mode = ExplorerViewMode::parse(explorer_str(&request, &["mode", "m"]));
                let start = explorer_u64(&request, &["start", "a"]).unwrap_or(0) as usize;
                let count = explorer_u64(&request, &["count", "c"])
                    .unwrap_or(80)
                    .min(2500) as usize;
                state.rows_window(request_id, mode, start, count, false)
            }
            "expand" => {
                let mode = ExplorerViewMode::parse(explorer_str(&request, &["mode", "m"]));
                if let Some(node_id) = explorer_str(&request, &["nodeId", "n"]) {
                    state.expand(node_id, mode);
                    json!({
                        "type": "invalidateRows",
                        "requestId": request_id,
                        "snapshotVersion": state.snapshot_version,
                        "viewVersion": state.view_version,
                        "start": 0,
                        "end": state.total_rows(mode),
                        "totalRows": state.total_rows(mode),
                    })
                } else {
                    explorer_error(request_id, "bad_request", "expand requires nodeId")
                }
            }
            "collapse" => {
                let mode = ExplorerViewMode::parse(explorer_str(&request, &["mode", "m"]));
                if let Some(node_id) = explorer_str(&request, &["nodeId", "n"]) {
                    state.collapse(node_id, mode);
                    json!({
                        "type": "invalidateRows",
                        "requestId": request_id,
                        "snapshotVersion": state.snapshot_version,
                        "viewVersion": state.view_version,
                        "start": 0,
                        "end": state.total_rows(mode),
                        "totalRows": state.total_rows(mode),
                    })
                } else {
                    explorer_error(request_id, "bad_request", "collapse requires nodeId")
                }
            }
            "selectDetails" => {
                let node_id = explorer_str(&request, &["nodeId", "n"]).unwrap_or("");
                state.details(request_id, node_id)
            }
            "searchStart" => {
                let query = explorer_str(&request, &["query", "q"]).unwrap_or("");
                let search_id = explorer_u64(&request, &["searchId", "sid"]).unwrap_or(request_id);
                state.start_search(request_id, search_id, query)
            }
            "searchRows" => {
                let start = explorer_u64(&request, &["start", "a"]).unwrap_or(0) as usize;
                let count = explorer_u64(&request, &["count", "c"])
                    .unwrap_or(80)
                    .min(25000) as usize;
                let include_match_ids =
                    explorer_bool(&request, &["includeMatchIds", "ids"]).unwrap_or(false);
                state.rows_window(
                    request_id,
                    ExplorerViewMode::Search,
                    start,
                    count,
                    include_match_ids,
                )
            }
            "clearSearch" => {
                state.clear_search();
                json!({
                    "type": "ready",
                    "requestId": request_id,
                    "snapshotVersion": state.snapshot_version,
                    "viewVersion": state.view_version,
                    "totalRows": state.total_rows(ExplorerViewMode::Normal),
                })
            }
            "reloadServices" => {
                let services = explorer_array(&request, &["services", "s"])
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                match state.reload_services(&services) {
                    Ok(()) => json!({
                        "type": "ready",
                        "requestId": request_id,
                        "snapshotVersion": state.snapshot_version,
                        "viewVersion": state.view_version,
                        "totalRows": state.total_rows(if state.search.is_some() { ExplorerViewMode::Search } else { ExplorerViewMode::Normal }),
                    }),
                    Err(error) => explorer_error(request_id, "reload_failed", &error.to_string()),
                }
            }
            "revealNode" => {
                let revealed_node_id = if let Some(node_id) =
                    explorer_str(&request, &["nodeId", "n"])
                    && let Some((service, index)) = state.resolve_node_index(node_id)
                    && let Some(service_state) = state.service_states.get(&service)
                {
                    let mut current = Some(index);
                    while let Some(current_index) = current {
                        let Some(instance) = service_state
                            .document
                            .as_ref()
                            .and_then(|document| document.instances.get(current_index))
                        else {
                            break;
                        };
                        state
                            .expanded
                            .insert(explorer_instance_tree_id(&service, &instance.settings_id));
                        current = instance.parent_index;
                    }
                    state.expanded.insert(explorer_service_tree_id(&service));
                    state.view_version += 1;
                    Some(node_id.to_string())
                } else {
                    None
                };
                let rows = state.collect_rows(ExplorerViewMode::Normal);
                let row_index = revealed_node_id.as_deref().and_then(|node_id| {
                    rows.iter().position(|row| {
                        row.get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id == node_id)
                    })
                });
                json!({
                    "type": "invalidateRows",
                    "requestId": request_id,
                    "snapshotVersion": state.snapshot_version,
                    "viewVersion": state.view_version,
                    "start": 0,
                    "end": rows.len(),
                    "totalRows": rows.len(),
                    "rowIndex": row_index,
                })
            }
            "cancel" => json!({
                "type": "error",
                "requestId": explorer_u64(&request, &["cancelRequestId", "cid"]).unwrap_or(request_id),
                "code": "cancelled",
                "message": "Request cancelled",
                "stale": true,
            }),
            "shutdown" => {
                writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&json!({
                        "type": "ready",
                        "requestId": request_id,
                        "snapshotVersion": state.snapshot_version,
                        "viewVersion": state.view_version,
                        "totalRows": state.total_rows(ExplorerViewMode::Normal),
                    }))?
                )?;
                stdout.flush()?;
                break;
            }
            other => explorer_error(
                request_id,
                "unknown_request",
                &format!("Unknown request: {other}"),
            ),
        };
        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }
    Ok(())
}

pub(super) fn bytecode_explorer_instance(args: BytecodeExplorerInstanceArgs) -> Result<()> {
    let (settings_file, document, service) = resolve_bytecode_read_input(
        args.settings_file.as_deref(),
        args.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let mode = OutputMode::parse(&args.output)?;
    let fields = parse_requested_fields(args.fields.as_deref());
    let index =
        resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?
            .index;
    let instance = document
        .instances
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("Invalid instance index {index}"))?;
    let projection_data = BytecodeProjectionData::new(&document, &service, &settings_file);
    let mut node = projection_data
        .projection(&document, mode, fields.as_ref())
        .node(index, true);
    let include_id_alias = fields.as_ref().is_some_and(|fields| fields.contains("id"))
        || (fields.is_none() && mode.includes_full_ids());
    if include_id_alias && let Some(map) = node.as_object_mut() {
        map.entry("id".to_string())
            .or_insert_with(|| Value::String(instance.settings_id.clone()));
    }
    print_json_output(&node, args.pretty)
}
