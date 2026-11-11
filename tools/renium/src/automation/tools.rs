use std::cell::OnceCell;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::app::output::print_json_output;
use crate::automation::luau_outline::{LuauFunction, luau_functions};
use crate::automation::{BoundContext, Failure, op};
use crate::cli::{
    AssetInsertArgs, AssetSearchArgs, BridgeConnectionArgs, GenerateModelArgs, JobStatusArgs,
    ScriptGrepArgs, ScriptReadArgs, ScriptSearchArgs,
};
use crate::project::config;
use crate::project::sourcemap::{
    SourcemapNode, build_project_sourcemap_with_loaded, load_existing_sourcemap_root,
};
use crate::system::files::canonical_path;

use super::commands::run_daemon;

const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptSearch {
    keywords: Value,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptRead {
    #[serde(alias = "target_file", alias = "file_path")]
    path: String,
    #[serde(alias = "start_line_one_indexed")]
    start_line: Option<usize>,
    #[serde(alias = "end_line_one_indexed_inclusive")]
    end_line: Option<usize>,
    #[serde(alias = "should_read_entire_file")]
    _entire: Option<bool>,
    #[serde(default)]
    outline: bool,
    #[serde(default, rename = "fn")]
    functions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptGrep {
    query: String,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    count: bool,
    context: Option<usize>,
    #[serde(default, deserialize_with = "one_or_several")]
    scope: Vec<String>,
    limit: Option<usize>,
    #[serde(default, rename = "fn")]
    functions: bool,
}

/// A search scope given as one path or as a list of them.
fn one_or_several<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scope {
        One(String),
        Several(Vec<String>),
    }
    Ok(match Option::<Scope>::deserialize(deserializer)? {
        Some(Scope::One(path)) => vec![path],
        Some(Scope::Several(paths)) => paths,
        None => Vec::new(),
    })
}

pub(crate) fn asset_search_command(args: AssetSearchArgs) -> anyhow::Result<()> {
    let mut result = crate::cloud::assets::search(
        &json!({
            "scope": "creator-store",
            "query": args.query,
            "assetType": args.asset_type,
            "maxResults": args.limit,
            "cursor": args.cursor,
        }),
        None,
    )
    .map_err(|failure| anyhow::anyhow!(failure.0.m))?;
    result["assetType"] = json!(args.asset_type);
    if !args.details {
        let compact = result
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| {
                let asset = item.get("asset").unwrap_or(&Value::Null);
                let creator = item.get("creator").unwrap_or(&Value::Null);
                json!({
                    "id": asset.get("id"),
                    "name": asset.get("name"),
                    "assetTypeId": asset.get("assetTypeId"),
                    "creator": creator.get("name"),
                    "creatorId": creator.get("userId"),
                    "verified": creator.get("verified"),
                    "hasScripts": asset.get("hasScripts"),
                })
            })
            .collect::<Vec<_>>();
        result["results"] = json!(compact);
    }
    print_json_output(&result, false)
}

fn creator_command(
    operation: u16,
    project: Option<&Path>,
    parameters: Value,
    bridge: Option<&BridgeConnectionArgs>,
) -> anyhow::Result<()> {
    run_daemon(operation, project, parameters, false, bridge)
}

pub(crate) fn asset_insert_command(
    args: AssetInsertArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let bridge = args.bridge;
    creator_command(
        op::ASSET_INSERT,
        project,
        json!({
            "assetId": args.asset_id.get(),
            "parentPath": args.parent,
            "name": args.name,
            "assetType": args.asset_type,
        }),
        Some(&bridge),
    )
}

fn model_size(value: &str) -> anyhow::Result<[f64; 3]> {
    let values = value
        .split([',', 'x', 'X'])
        .map(str::trim)
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()?;
    let [x, y, z] = values.as_slice() else {
        anyhow::bail!("Model size must contain three numbers: X,Y,Z");
    };
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        anyhow::bail!("Model size must contain finite numbers");
    }
    Ok([*x, *y, *z])
}

pub(crate) fn generate_model_command(
    args: GenerateModelArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    if args.prompt.as_deref().is_none_or(str::is_empty) && args.image_asset_id.is_none() {
        anyhow::bail!("Provide a prompt, --image-asset-id, or both");
    }
    if let Some(max_triangles) = args.max_triangles
        && !(12..=20_000).contains(&max_triangles)
    {
        anyhow::bail!("--max-triangles must be from 12 through 20000");
    }
    let bridge = args.bridge;
    creator_command(
        op::GENERATE_MODEL,
        project,
        json!({
            "prompt": args.prompt,
            "imageAssetId": args.image_asset_id.map(|id| id.get()),
            "parentPath": args.parent,
            "name": args.name,
            "size": args.size.as_deref().map(model_size).transpose()?,
            "maxTriangles": args.max_triangles,
            "generateTextures": args.generate_textures,
            "parts": args.parts,
            "segmentation": args.segmentation,
            "anchored": args.unanchored.then_some(false),
        }),
        Some(&bridge),
    )
}

pub(crate) fn job_status_command(
    args: JobStatusArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    if !args.wait_seconds.is_finite() || !(0.0..=120.0).contains(&args.wait_seconds) {
        anyhow::bail!("--wait-seconds must be from 0 through 120");
    }
    creator_command(
        op::JOB_STATUS,
        project,
        json!({
            "jobId": args.job_id,
            "waitSeconds": args.wait_seconds,
        }),
        None,
    )
}

fn script_roots(project: Option<&Path>) -> anyhow::Result<(PathBuf, PathBuf)> {
    let loaded = config::load_project(project, None)?;
    let source = loaded.root.join(&loaded.project.source_root);
    Ok((loaded.root, source))
}

fn print_script_hits(result: Result<Value, Failure>) -> anyhow::Result<()> {
    let value = result.map_err(|failure| anyhow::anyhow!(failure.0.m))?;
    let text = script_hits_text(&value);
    crate::app::output::emit_global_output(&value, &text)
}

const HIT_TEXT_LIMIT: usize = 240;

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// One hit as a `LINE: text` line, or a context line around a hit as
/// `LINE- text`, without the indentation and with a long line cut short.
fn push_hit_line(text: &mut String, hit: &Value) {
    let line = hit["text"].as_str().unwrap_or_default().trim();
    let mark = if hit["context"] == json!(true) {
        '-'
    } else {
        ':'
    };
    text.push_str(&format!("{}{mark} ", hit["line"]));
    match line.char_indices().nth(HIT_TEXT_LIMIT) {
        Some((cut, _)) => {
            text.push_str(&line[..cut]);
            text.push('\u{2026}');
        }
        None => text.push_str(line),
    }
    text.push('\n');
}

/// One function of a `--fn` grep: a `FIRST-LAST name` line, then its source,
/// or its hit lines when the limit had no room for the source; returns the
/// number of lines after the first.
fn push_function(text: &mut String, function: &Value) -> u64 {
    text.push_str(&format!(
        "{}-{} {}\n",
        function["startLine"],
        function["endLine"],
        function["name"].as_str().unwrap_or_default()
    ));
    if let Some(source) = function.get("source").and_then(Value::as_str) {
        text.push_str(source);
        text.push('\n');
        return source.split('\n').count() as u64;
    }
    let hits = items(function, "hits");
    for hit in hits {
        push_hit_line(text, hit);
    }
    hits.len() as u64
}

/// The lines under one script's path: the parse error of a script whose
/// functions could not be read, then its functions and hits in line order;
/// returns the number of source and hit lines.
fn push_script_lines(text: &mut String, result: &Value) -> u64 {
    if let Some(error) = result.get("parseError").and_then(Value::as_str) {
        text.push_str(&format!("does not parse ({error}); matching lines:\n"));
    }
    let mut lines = 0;
    let mut functions = items(result, "functions").iter().peekable();
    for hit in items(result, "hits") {
        let line = hit["line"].as_u64();
        while let Some(function) = functions.next_if(|next| next["startLine"].as_u64() < line) {
            lines += push_function(text, function);
        }
        push_hit_line(text, hit);
        lines += u64::from(hit["context"] != json!(true));
    }
    for function in functions {
        lines += push_function(text, function);
    }
    lines
}

/// Search hits as text: each script's path on its own line, for a grep
/// followed by its hits as `LINE: text` without the indentation and with a
/// long line cut short, and a last line when the limit left hits out. A
/// script search names the scripts only; `rbx sg` shows the lines, with
/// `--count` each script's path and its number of matching lines, and with
/// `--fn` each function that holds a hit as a `FIRST-LAST name` line and its
/// source (its hit lines when the limit has no room for the source), in line
/// order with the hits outside every function.
fn script_hits_text(value: &Value) -> String {
    let results = items(value, "results");
    if results.is_empty() {
        return "no matches".to_string();
    }
    let mut text = String::new();
    let mut lines = 0u64;
    for result in results {
        text.push_str(result["path"].as_str().unwrap_or_default());
        if let Some(count) = result.get("count") {
            text.push_str(&format!(" {count}"));
        }
        text.push('\n');
        lines += push_script_lines(&mut text, result);
    }
    if value["truncated"] == json!(true) {
        let total_lines = value.get("totalLines").unwrap_or(&value["totalMatches"]);
        let (shown, total, unit) = match value.get("totalFiles") {
            Some(total) => (results.len() as u64, total.as_u64().unwrap_or(0), "scripts"),
            None => (lines, total_lines.as_u64().unwrap_or(0), "lines"),
        };
        text.push_str(&format!(
            "{shown} of {total} {unit} shown; -l N shows more\n"
        ));
    }
    text.truncate(text.trim_end().len());
    text
}

pub(crate) fn script_search_command(
    args: ScriptSearchArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, source) = script_roots(project)?;
    print_script_hits(script_search_at(
        &root,
        &source,
        &json!({ "keywords": args.keywords(), "limit": args.limit }),
    ))
}

pub(crate) fn script_grep_command(
    args: ScriptGrepArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, source) = script_roots(project)?;
    print_script_hits(script_grep_at(
        &root,
        &source,
        &json!({
            "query": args.query,
            "caseInsensitive": args.case_insensitive,
            "regex": args.regex,
            "count": args.count,
            "context": args.context,
            "scope": args.scope,
            "limit": args.limit,
            "fn": args.functions,
        }),
    ))
}

/// A script read as text: a line range as `FIRST-LAST/TOTAL` and its source,
/// an outline as `TOTAL lines` and one `FIRST-LAST name(params)` line per
/// function, indented one space per enclosing function, and functions read by
/// name each as `FIRST-LAST/TOTAL name` and its source.
fn script_read_text(value: &Value) -> String {
    let total = &value["totalLines"];
    if let Some(source) = value.get("source").and_then(Value::as_str) {
        return format!(
            "{}-{}/{total}\n{source}",
            value["startLine"], value["endLine"]
        );
    }
    let functions = items(value, "functions");
    let mut text = String::new();
    if functions
        .iter()
        .all(|function| function.get("source").is_none())
    {
        text.push_str(&format!("{total} lines\n"));
    }
    for function in functions {
        let range = format!("{}-{}", function["startLine"], function["endLine"]);
        let name = function["name"].as_str().unwrap_or_default();
        match function.get("source").and_then(Value::as_str) {
            Some(source) => text.push_str(&format!("{range}/{total} {name}\n{source}\n")),
            None => {
                let depth = function["depth"].as_u64().unwrap_or(0) as usize;
                let parameters = items(function, "parameters")
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                text.push_str(&format!(
                    "{}{range} {name}({parameters})\n",
                    " ".repeat(depth)
                ));
            }
        }
    }
    text.truncate(text.trim_end_matches('\n').len());
    text
}

/// One read of several as text: a `== path FIRST-LAST` line and the source,
/// or a `== path` line and the outline or the functions read by name.
fn script_block_text(value: &Value) -> String {
    let path = value["path"].as_str().unwrap_or_default();
    match value.get("source").and_then(Value::as_str) {
        Some(source) => format!(
            "== {path} {}-{}\n{source}",
            value["startLine"], value["endLine"]
        ),
        None => format!("== {path}\n{}", script_read_text(value)),
    }
}

/// One line range of a read target: `A-B`, `A-` (through the last line) or
/// `A` (that line alone).
fn line_range(text: &str) -> Option<(usize, Option<usize>)> {
    Some(match text.split_once('-') {
        Some((first, "")) => (first.parse().ok()?, None),
        Some((first, last)) => (first.parse().ok()?, Some(last.parse().ok()?)),
        None => {
            let line = text.parse().ok()?;
            (line, Some(line))
        }
    })
}

/// A read target split into its path and the line ranges after its last `:`,
/// separated by commas. A suffix that is not line ranges stays in the path,
/// so `C:/x/y.luau` is all path.
fn split_target(target: &str) -> (&str, Vec<(usize, Option<usize>)>) {
    let ranged = target.rsplit_once(':').and_then(|(path, suffix)| {
        let ranges = suffix.split(',').map(line_range).collect::<Option<_>>()?;
        (!path.is_empty()).then_some((path, ranges))
    });
    ranged.unwrap_or((target, Vec::new()))
}

/// The reads a command line asks for, in order: one per line range of each
/// target, and one for a target without lines, which takes `--start-line` and
/// `--end-line`.
fn read_requests(args: &ScriptReadArgs) -> anyhow::Result<Vec<Value>> {
    let mut requests = Vec::new();
    for target in &args.targets {
        let (path, ranges) = split_target(target);
        let ranges = if ranges.is_empty() {
            vec![(args.start_line, args.end_line)]
        } else if args.start_line.is_some() || args.end_line.is_some() {
            anyhow::bail!(
                "{target} names its lines; give them there or with --start-line/--end-line, not both"
            );
        } else {
            ranges
                .into_iter()
                .map(|(first, last)| (Some(first), last))
                .collect()
        };
        for (first, last) in ranges {
            requests.push(json!({
                "path": path,
                "startLine": first,
                "endLine": last,
                "outline": args.outline,
                "fn": args.functions,
            }));
        }
    }
    Ok(requests)
}

pub(crate) fn script_read_command(
    args: ScriptReadArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, _) = script_roots(project)?;
    let mapping = ScriptMapping::new(&root);
    let values = read_requests(&args)?
        .iter()
        .map(|request| script_read_with(&mapping, request))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|failure| anyhow::anyhow!(failure.0.m))?;
    if let [value] = values.as_slice() {
        return crate::app::output::emit_global_output(value, &script_read_text(value));
    }
    let text = values
        .iter()
        .map(script_block_text)
        .collect::<Vec<_>>()
        .join("\n");
    crate::app::output::emit_global_output(&json!({ "scripts": values }), &text)
}

fn failure(message: impl Into<String>, next: &str) -> Failure {
    Failure::new("bad_req", message.into(), false, next)
}

pub(super) fn has_script_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "lua" | "luau"))
}

fn script_files(source: &Path) -> Vec<PathBuf> {
    let mut paths = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| has_script_extension(path))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

pub(super) fn read_script(path: &Path, next: &str) -> Result<String, Failure> {
    let metadata = fs::metadata(path).map_err(|error| {
        failure(
            format!("Failed to inspect {}: {error}", path.display()),
            next,
        )
    })?;
    if metadata.len() > MAX_SCRIPT_BYTES {
        return Err(failure(
            format!("{} exceeds the 2 MiB script limit", path.display()),
            next,
        ));
    }
    fs::read_to_string(path)
        .map_err(|error| failure(format!("Failed to read {}: {error}", path.display()), next))
}

pub(super) fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn keywords(value: &Value) -> Option<Vec<String>> {
    let values = if let Some(text) = value.as_str() {
        text.split(',').map(str::trim).map(str::to_string).collect()
    } else {
        value
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::trim).map(str::to_string))
            .collect::<Option<Vec<_>>>()?
    };
    Some(
        values
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect(),
    )
}

fn script_search_at(root: &Path, source: &Path, parameters: &Value) -> Result<Value, Failure> {
    let request: ScriptSearch = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            format!("Invalid script-search payload: {error}"),
            "script-search",
        )
    })?;
    let needles = keywords(&request.keywords)
        .filter(|keywords| !keywords.is_empty())
        .ok_or_else(|| {
            failure(
                "script-search requires one or more keywords",
                "script-search",
            )
        })?
        .into_iter()
        .map(|keyword| keyword.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let limit = request.limit.unwrap_or(25).clamp(1, 100);
    let mut results = Vec::new();
    let mut total_matches = 0;
    for path in script_files(source) {
        let source = read_script(&path, "script-search")?;
        let relative = relative(root, &path);
        let searchable = format!("{relative}\n{source}").to_ascii_lowercase();
        if needles.iter().all(|needle| searchable.contains(needle)) {
            total_matches += 1;
            if results.len() < limit {
                let lines = source
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| {
                        let line = line.to_ascii_lowercase();
                        needles.iter().any(|needle| line.contains(needle))
                    })
                    .take(5)
                    .map(|(index, line)| json!({ "line": index + 1, "text": line }))
                    .collect::<Vec<_>>();
                results.push(json!({ "path": relative, "matches": lines }));
            }
        }
    }
    let truncated = results.len() < total_matches;
    let mut result = json!({ "results": results, "totalFiles": total_matches });
    if truncated {
        result["truncated"] = json!(true);
    }
    Ok(result)
}

pub(crate) fn script_search(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    script_search_at(
        Path::new(&context.root),
        Path::new(&context.source),
        parameters,
    )
}

fn resolve_script(root: &Path, requested: &str, next: &str) -> Result<PathBuf, Failure> {
    let path = PathBuf::from(requested);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let path = canonical_path(&path).map_err(|error| {
        failure(
            format!("Failed to resolve {}: {error}", path.display()),
            next,
        )
    })?;
    let root = canonical_path(root)
        .map_err(|error| failure(format!("Failed to resolve project root: {error}"), "bind"))?;
    if !path.starts_with(root) || !has_script_extension(&path) {
        return Err(failure(
            "Script path must be a .lua or .luau file inside the bound project",
            next,
        ));
    }
    Ok(path)
}

/// Adds the files of every instance at the dotted path `rest` below `node` to
/// `files` and returns the number of those instances. A name may hold dots,
/// so every child whose name starts the rest of the path is followed.
fn mapped_files<'a>(node: &'a SourcemapNode, rest: &str, files: &mut Vec<&'a str>) -> usize {
    let mut instances = 0;
    for child in &node.children {
        match rest.strip_prefix(child.name.as_str()) {
            Some("") => {
                instances += 1;
                files.extend(child.file_paths.iter().map(String::as_str));
            }
            Some(tail) => {
                if let Some(tail) = tail.strip_prefix('.') {
                    instances += mapped_files(child, tail, files);
                }
            }
            None => {}
        }
    }
    instances
}

/// The file the sourcemap maps the script at an instance path to. A path
/// that names no instance, an instance without a script file, or several
/// script files is an error.
fn mapped_script<'a>(sourcemap: &'a SourcemapNode, requested: &str) -> Result<&'a str, Failure> {
    let mut files = Vec::new();
    let path = requested.strip_prefix("game.").unwrap_or(requested);
    let instances = mapped_files(sourcemap, path, &mut files);
    let message = match files.as_slice() {
        [file] => return Ok(*file),
        [] if instances == 0 => format!(
            "No script at {requested}: it is not a file, and the project has no instance at that path; rbx ss NAME finds scripts"
        ),
        [] => format!("{requested} has no script file in the project"),
        _ => format!(
            "{requested} names {} script files: {}; pass one of them",
            files.len(),
            files.join(", ")
        ),
    };
    Err(failure(message, "script-read"))
}

/// The project's script mapping for one command: the sourcemap on disk, and
/// the one built from the stores and source files, which answers when the one
/// on disk is missing or does not lead to a script file.
pub(super) struct ScriptMapping<'a> {
    root: &'a Path,
    saved: OnceCell<Option<SourcemapNode>>,
    built: OnceCell<SourcemapNode>,
}

fn loaded_once<T>(
    cell: &OnceCell<T>,
    load: impl FnOnce() -> anyhow::Result<T>,
) -> Result<&T, Failure> {
    if let Some(value) = cell.get() {
        return Ok(value);
    }
    let value = load().map_err(|error| {
        failure(
            format!("Failed to load the project's sourcemap: {error:#}"),
            "script-read",
        )
    })?;
    Ok(cell.get_or_init(|| value))
}

impl<'a> ScriptMapping<'a> {
    pub(super) fn new(root: &'a Path) -> Self {
        Self {
            root,
            saved: OnceCell::new(),
            built: OnceCell::new(),
        }
    }

    /// The file of the script at an instance path.
    fn file(&self, requested: &str) -> Result<&str, Failure> {
        let saved = loaded_once(&self.saved, || load_existing_sourcemap_root(self.root))?;
        if let Some(saved) = saved
            && let Ok(file) = mapped_script(saved, requested)
            && self.root.join(file).is_file()
        {
            return Ok(file);
        }
        let built = loaded_once(&self.built, || {
            let loaded = config::try_load_project(None, Some(self.root))?
                .filter(|loaded| loaded.root == self.root);
            build_project_sourcemap_with_loaded(self.root, loaded.as_ref())
        })?;
        mapped_script(built, requested)
    }
}

/// The script file a read names: a file path, or an instance path
/// (`ServerScriptService.Main`) that the project's script mapping leads to a
/// file.
pub(super) fn resolve_read_target(
    mapping: &ScriptMapping,
    requested: &str,
) -> Result<PathBuf, Failure> {
    let root = mapping.root;
    let names_file = requested.contains(['/', '\\']) || has_script_extension(Path::new(requested));
    if names_file || root.join(requested).is_file() {
        return resolve_script(root, requested, "script-read");
    }
    resolve_script(root, mapping.file(requested)?, "script-read")
}

fn script_read_at(root: &Path, parameters: &Value) -> Result<Value, Failure> {
    script_read_with(&ScriptMapping::new(root), parameters)
}

fn script_read_with(mapping: &ScriptMapping, parameters: &Value) -> Result<Value, Failure> {
    let root = mapping.root;
    let request: ScriptRead = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            format!("Invalid script-read payload: {error}"),
            "script-read",
        )
    })?;
    let path = resolve_read_target(mapping, &request.path)?;
    let source = read_script(&path, "script-read")?;
    let lines = source.lines().collect::<Vec<_>>();
    if request.outline || !request.functions.is_empty() {
        return script_read_functions(&relative(root, &path), &source, &lines, &request);
    }
    let start = request.start_line.unwrap_or(1);
    let requested_end = request.end_line.unwrap_or(lines.len());
    if start == 0 || requested_end < start || start > lines.len() {
        return Err(failure(
            format!(
                "Requested lines {start} through {requested_end}, but {} has {} lines",
                relative(root, &path),
                lines.len()
            ),
            "script-read",
        ));
    }
    let end = requested_end.min(lines.len());
    Ok(json!({
        "path": relative(root, &path),
        "startLine": start,
        "endLine": end,
        "totalLines": lines.len(),
        "source": lines[start - 1..end].join("\n"),
    }))
}

/// One function as a result entry: its name and lines, and under `key` its
/// `source` or the `hits` returned in place of it.
fn function_entry(function: &LuauFunction, key: &str, value: Value) -> Value {
    let mut entry = json!({
        "name": function.name,
        "startLine": function.first_line,
        "endLine": function.last_line,
    });
    entry[key] = value;
    entry
}

/// A function's source: the script's lines from its first through its last.
fn function_source(function: &LuauFunction, lines: &[&str]) -> Value {
    json!(lines[function.first_line - 1..function.last_line].join("\n"))
}

/// The functions `names` ask for, each once and in source order. A name
/// matches a function's printed name or its last segment; a name that matches
/// none is an error that lists the nearest names.
fn named_functions<'a>(
    path: &str,
    functions: &'a [LuauFunction],
    names: &[String],
) -> Result<Vec<&'a LuauFunction>, Failure> {
    let unmatched = names
        .iter()
        .find(|name| !functions.iter().any(|function| function.is_named(name)));
    if let Some(name) = unmatched {
        let qualified = name.contains(['.', ':']);
        let nearest = crate::bytecode::closest_property_names(
            name,
            functions.iter().map(|function| match qualified {
                true => function.name.as_str(),
                false => function.last_segment(),
            }),
        );
        let mut message = format!("No function {name} in {path}");
        if !nearest.is_empty() {
            message.push_str(&format!("; nearest: {}", nearest.join(", ")));
        }
        message.push_str("; --outline lists them all");
        return Err(failure(message, "script-read"));
    }
    Ok(functions
        .iter()
        .filter(|function| names.iter().any(|name| function.is_named(name)))
        .collect())
}

/// A script read by function: with `outline` the name, parameters, lines and
/// depth of every function, with `fn` names the source of each function they
/// match. A script that does not parse is an error with the parser's first
/// error and its line.
fn script_read_functions(
    path: &str,
    source: &str,
    lines: &[&str],
    request: &ScriptRead,
) -> Result<Value, Failure> {
    let ranged = request.start_line.is_some() || request.end_line.is_some();
    if ranged || (request.outline && !request.functions.is_empty()) {
        return Err(failure(
            "script-read takes one of a line range, outline and fn",
            "script-read",
        ));
    }
    let functions = luau_functions(source).map_err(|error| {
        failure(
            format!("{path} does not parse ({error}); read it by line range"),
            "script-read",
        )
    })?;
    let listed = if request.outline {
        functions
            .iter()
            .map(|function| {
                json!({
                    "name": function.name,
                    "parameters": function.parameters,
                    "startLine": function.first_line,
                    "endLine": function.last_line,
                    "depth": function.depth,
                })
            })
            .collect::<Vec<_>>()
    } else {
        named_functions(path, &functions, &request.functions)?
            .into_iter()
            .map(|function| function_entry(function, "source", function_source(function, lines)))
            .collect()
    };
    Ok(json!({ "path": path, "totalLines": lines.len(), "functions": listed }))
}

pub(crate) fn script_read(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    script_read_at(Path::new(&context.root), parameters)
}

fn script_grep_at(root: &Path, source: &Path, parameters: &Value) -> Result<Value, Failure> {
    let request: ScriptGrep = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            format!("Invalid script-grep payload: {error}"),
            "script-grep",
        )
    })?;
    if request.query.is_empty() {
        return Err(failure(
            "script-grep requires a non-empty query",
            "script-grep",
        ));
    }
    let matches = line_matcher(&request)?;
    let scopes = request
        .scope
        .iter()
        .map(|scope| scope_prefix(scope))
        .collect::<Vec<_>>();
    let files = script_files(source).into_iter().filter(|path| {
        let relative = relative(root, path);
        scopes.is_empty() || scopes.iter().any(|scope| in_scope(&relative, scope))
    });
    if request.functions {
        if request.count {
            return Err(failure(
                "script-grep takes one of count and fn",
                "script-grep",
            ));
        }
        let limit = request.limit.unwrap_or(200).clamp(1, 1000);
        return script_grep_functions(root, files, &matches, limit);
    }
    let limit = request.limit.unwrap_or(50).clamp(1, 1000);
    let context = request.context.unwrap_or(0);
    // Matches are grouped by file: one path per script instead of one per line.
    let mut results: Vec<Value> = Vec::new();
    let mut returned = 0;
    let mut total_matches = 0;
    let mut matching_files = 0;
    for path in files {
        let source = read_script(&path, "script-grep")?;
        if request.count {
            let count = source.lines().filter(|line| matches(line)).count();
            if count > 0 {
                total_matches += count;
                matching_files += 1;
                if results.len() < limit {
                    results.push(json!({ "path": relative(root, &path), "count": count }));
                }
            }
            continue;
        }
        let lines = source.lines().collect::<Vec<_>>();
        let around =
            |line: usize| json!({ "line": line, "text": lines[line - 1], "context": true });
        let mut hits = Vec::new();
        let (mut listed, mut after) = (0, 0);
        for (index, line) in lines.iter().enumerate() {
            let matched = matches(line);
            total_matches += usize::from(matched);
            if matched && returned < limit {
                returned += 1;
                let before = (index + 1).saturating_sub(context).max(listed + 1);
                hits.extend((before..=index).map(around));
                hits.push(json!({ "line": index + 1, "text": line }));
                (listed, after) = (index + 1, context);
            } else if after > 0 {
                hits.push(around(index + 1));
                (listed, after) = (index + 1, after - 1);
            }
        }
        if !hits.is_empty() {
            results.push(json!({ "path": relative(root, &path), "hits": hits }));
        }
    }
    let listed = results.len();
    let mut result = json!({ "results": results, "totalMatches": total_matches });
    if request.count {
        if listed < matching_files {
            result["truncated"] = json!(true);
            result["totalFiles"] = json!(matching_files);
        }
    } else if returned < total_matches {
        result["truncated"] = json!(true);
    }
    Ok(result)
}

type FunctionHits<'a> = (Option<&'a LuauFunction>, Vec<usize>);

/// The hit lines of one script grouped for a function grep, in line order: a
/// function that holds hits comes once with all of them, and a hit outside
/// every function on its own. The function is the innermost one around a hit,
/// or the one around that where it holds hits of its own as well.
fn hits_by_function<'a>(functions: &'a [LuauFunction], hits: &[usize]) -> Vec<FunctionHits<'a>> {
    let mut holding = std::collections::BTreeSet::new();
    for &line in hits {
        let innermost = functions
            .iter()
            .enumerate()
            .filter(|(_, function)| function.holds(line))
            .max_by_key(|(_, function)| function.depth);
        holding.extend(innermost.map(|(index, _)| index));
    }
    let mut kept: Vec<&LuauFunction> = Vec::new();
    for function in holding.into_iter().map(|index| &functions[index]) {
        let nested = kept
            .last()
            .is_some_and(|outer| outer.depth < function.depth && outer.holds(function.first_line));
        if !nested {
            kept.push(function);
        }
    }
    let mut groups: Vec<FunctionHits> = Vec::new();
    for &line in hits {
        let function = kept.iter().copied().find(|function| function.holds(line));
        match groups.last_mut() {
            Some((Some(last), group))
                if function.is_some_and(|function| std::ptr::eq(function, *last)) =>
            {
                group.push(line);
            }
            _ => groups.push((function, vec![line])),
        }
    }
    groups
}

/// Spends what is `left` of a function grep's line limit on one script's
/// grouped hits and returns the `functions` and `hits` to show and the lines
/// they take without a limit. A function comes with its source while that
/// fits in what is left; one that does not comes with its hit lines instead,
/// one line of the limit each like a hit outside the functions.
fn spend_lines(
    groups: Vec<FunctionHits>,
    lines: &[&str],
    left: &mut usize,
) -> (Vec<Value>, Vec<Value>, usize) {
    let mut functions = Vec::new();
    let mut outside = Vec::new();
    let mut total = 0;
    for (function, hits) in groups {
        let length = function.map_or(1, LuauFunction::line_count);
        total += length;
        if let Some(function) = function.filter(|_| length <= *left) {
            *left -= length;
            let source = function_source(function, lines);
            functions.push(function_entry(function, "source", source));
            continue;
        }
        let shown = hits.len().min(*left);
        *left -= shown;
        let hits = hits[..shown]
            .iter()
            .map(|&line| json!({ "line": line, "text": lines[line - 1] }))
            .collect::<Vec<_>>();
        match function {
            _ if shown == 0 => {}
            Some(function) => functions.push(function_entry(function, "hits", json!(hits))),
            None => outside.extend(hits),
        }
    }
    (functions, outside, total)
}

/// A grep that returns whole functions: per script the `functions` that hold
/// a hit, each once with its `source`, and the `hits` outside every function.
/// A script that does not parse returns its hits and a `parseError`. `limit`
/// caps the source and hit lines returned across all scripts, a function too
/// long for what is left of it returns its `hits` instead of its `source`,
/// and `totalLines` counts the lines without the limit.
fn script_grep_functions(
    root: &Path,
    files: impl Iterator<Item = PathBuf>,
    matches: &LineMatcher,
    limit: usize,
) -> Result<Value, Failure> {
    let mut results: Vec<Value> = Vec::new();
    let mut left = limit;
    let mut total_matches = 0;
    let mut total_lines = 0;
    for path in files {
        let source = read_script(&path, "script-grep")?;
        let lines = source.lines().collect::<Vec<_>>();
        let hits = (1..=lines.len())
            .filter(|line| matches(lines[line - 1]))
            .collect::<Vec<_>>();
        if hits.is_empty() {
            continue;
        }
        total_matches += hits.len();
        let (functions, parse_error) = match luau_functions(&source) {
            Ok(functions) => (functions, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let groups = hits_by_function(&functions, &hits);
        let (shown_functions, shown_hits, script_lines) = spend_lines(groups, &lines, &mut left);
        total_lines += script_lines;
        if shown_functions.is_empty() && shown_hits.is_empty() {
            continue;
        }
        let mut result = json!({ "path": relative(root, &path) });
        if let Some(error) = parse_error {
            result["parseError"] = json!(error);
        }
        if !shown_functions.is_empty() {
            result["functions"] = json!(shown_functions);
        }
        if !shown_hits.is_empty() {
            result["hits"] = json!(shown_hits);
        }
        results.push(result);
    }
    let mut result = json!({
        "results": results,
        "totalMatches": total_matches,
        "totalLines": total_lines,
    });
    if limit - left < total_lines {
        result["truncated"] = json!(true);
    }
    Ok(result)
}

type LineMatcher = Box<dyn Fn(&str) -> bool>;

/// The test a grep applies to each line: the query as literal text, or as a
/// regular expression when asked, with or without regard to case.
fn line_matcher(request: &ScriptGrep) -> Result<LineMatcher, Failure> {
    if request.regex {
        let pattern = regex::RegexBuilder::new(&request.query)
            .case_insensitive(request.case_insensitive)
            .build()
            .map_err(|error| {
                failure(
                    format!("Invalid regular expression: {error}"),
                    "script-grep",
                )
            })?;
        return Ok(Box::new(move |line| pattern.is_match(line)));
    }
    if request.case_insensitive {
        let needle = request.query.to_ascii_lowercase();
        return Ok(Box::new(move |line| {
            line.to_ascii_lowercase().contains(&needle)
        }));
    }
    let needle = request.query.clone();
    Ok(Box::new(move |line| line.contains(&needle)))
}

/// A `--in` path as the prefix a script's project-relative path is tested
/// against: forward slashes, no leading `./` and no trailing slash.
fn scope_prefix(scope: &str) -> String {
    let scope = scope.replace('\\', "/");
    scope
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn in_scope(relative: &str, scope: &str) -> bool {
    relative == scope
        || relative
            .strip_prefix(scope)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn script_grep(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    script_grep_at(
        Path::new(&context.root),
        Path::new(&context.source),
        parameters,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        HIT_TEXT_LIMIT, script_grep_at, script_hits_text, script_read_at, script_read_text,
    };

    /// Runs `check` on the root and source folder of a project that holds
    /// `files`, given as paths under `src` and their text.
    fn with_scripts(
        files: &[(&str, &str)],
        check: impl FnOnce(&std::path::Path, &std::path::Path),
    ) {
        let root = crate::system::files::create_unique_directory(
            &std::env::temp_dir(),
            "renium-script-fn-",
        )
        .unwrap();
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = std::fs::remove_dir_all(&root);
        });
        let root = crate::system::files::canonical_path(&root).unwrap();
        let source = root.join("src");
        std::fs::create_dir_all(&source).unwrap();
        for (path, text) in files {
            std::fs::write(source.join(path), text).unwrap();
        }
        check(&root, &source);
    }

    const SHOP: &str = "\
local Shop = {}
function Shop.buy(player, id)
\tlocal function price()
\t\treturn 5
\tend
\treturn price()
end
Shop.sell = function(player)
end
return Shop
";

    #[test]
    fn a_script_reads_as_an_outline_and_by_function_name() {
        with_scripts(&[("Shop.luau", SHOP)], |root, _| {
            let read = |mut parameters: serde_json::Value| {
                parameters["path"] = json!("src/Shop.luau");
                script_read_at(root, &parameters)
                    .map(|value| script_read_text(&value))
                    .map_err(|failure| failure.0.m)
            };
            assert_eq!(
                read(json!({ "outline": true })).unwrap(),
                "10 lines\n2-7 Shop.buy(player, id)\n 3-5 price()\n8-9 Shop.sell(player)"
            );
            assert_eq!(
                read(json!({ "fn": ["sell"] })).unwrap(),
                "8-9/10 Shop.sell\nShop.sell = function(player)\nend"
            );
            assert_eq!(
                read(json!({ "fn": ["price", "Shop.buy", "buy"] })).unwrap(),
                "2-7/10 Shop.buy\nfunction Shop.buy(player, id)\n\tlocal function price()\n\t\treturn 5\n\tend\n\treturn price()\nend\n3-5/10 price\n\tlocal function price()\n\t\treturn 5\n\tend"
            );
            assert_eq!(
                read(json!({ "fn": ["sell", "bye"] })).unwrap_err(),
                "No function bye in src/Shop.luau; nearest: buy; --outline lists them all"
            );
            assert_eq!(
                read(json!({ "fn": ["Shop.sel"] })).unwrap_err(),
                "No function Shop.sel in src/Shop.luau; nearest: Shop.sell, Shop.buy; --outline lists them all"
            );
            assert_eq!(
                read(json!({ "startLine": 8, "endLine": 9 })).unwrap(),
                "8-9/10\nShop.sell = function(player)\nend"
            );
            assert!(read(json!({ "outline": true, "endLine": 9 })).is_err());
            assert!(read(json!({ "outline": true, "fn": ["sell"] })).is_err());
        });
    }

    #[test]
    fn a_script_that_does_not_parse_says_so_and_greps_as_plain_lines() {
        let files = [
            (
                "Broken.luau",
                "local function grant()\n\treturn 1\nend\nlocal x = )\n",
            ),
            ("Fine.luau", "local function grant()\n\treturn 2\nend\n"),
        ];
        with_scripts(&files, |root, source| {
            let error =
                script_read_at(root, &json!({ "path": "src/Broken.luau", "outline": true }))
                    .map_err(|failure| failure.0.m)
                    .unwrap_err();
            assert!(
                error.starts_with("src/Broken.luau does not parse (line 4: "),
                "{error}"
            );
            assert!(!error.contains('\n'), "{error}");
            let value = script_grep_at(root, source, &json!({ "query": "return", "fn": true }))
                .map_err(|failure| failure.0.m)
                .unwrap();
            let error = value["results"][0]["parseError"].as_str().unwrap();
            assert!(error.starts_with("line 4: "), "{error}");
            assert_eq!(
                script_hits_text(&value),
                format!(
                    "src/Broken.luau\ndoes not parse ({error}); matching lines:\n2: return 1\nsrc/Fine.luau\n1-3 grant\nlocal function grant()\n\treturn 2\nend"
                )
            );
        });
    }

    const ROUNDS: &str = "\
local total = 0
local function add(value)
\ttotal += value
\treturn total
end
local function outer()
\tlocal function inner()
\t\treturn total
\tend
\treturn inner()
end
local function both()
\tlocal seen = total
\tlocal function nested()
\t\treturn total
\tend
\treturn nested() + seen
end
local function unrelated()
end
";

    #[test]
    fn a_function_grep_prints_each_function_once_and_the_limit_caps_source_lines() {
        with_scripts(&[("Rounds.luau", ROUNDS)], |root, source| {
            let grep = |mut parameters: serde_json::Value| {
                parameters["query"] = json!("total");
                parameters["fn"] = json!(true);
                script_grep_at(root, source, &parameters).map_err(|failure| failure.0.m)
            };
            let add = "2-5 add\nlocal function add(value)\n\ttotal += value\n\treturn total\nend";
            let all = grep(json!({})).unwrap();
            assert_eq!(all["totalMatches"], json!(6));
            assert_eq!(all["totalLines"], json!(15));
            assert_eq!(
                script_hits_text(&all),
                format!(
                    "src/Rounds.luau\n1: local total = 0\n{add}\n7-9 inner\n\tlocal function inner()\n\t\treturn total\n\tend\n12-18 both\nlocal function both()\n\tlocal seen = total\n\tlocal function nested()\n\t\treturn total\n\tend\n\treturn nested() + seen\nend"
                )
            );
            assert_eq!(
                script_hits_text(&grep(json!({ "limit": 9 })).unwrap()),
                format!(
                    "src/Rounds.luau\n1: local total = 0\n{add}\n7-9 inner\n\tlocal function inner()\n\t\treturn total\n\tend\n12-18 both\n13: local seen = total\n9 of 15 lines shown; -l N shows more"
                )
            );
            assert_eq!(
                script_hits_text(&grep(json!({ "limit": 6 })).unwrap()),
                format!(
                    "src/Rounds.luau\n1: local total = 0\n{add}\n7-9 inner\n8: return total\n6 of 15 lines shown; -l N shows more"
                )
            );
            assert_eq!(
                script_hits_text(&grep(json!({ "limit": 5 })).unwrap()),
                format!(
                    "src/Rounds.luau\n1: local total = 0\n{add}\n5 of 15 lines shown; -l N shows more"
                )
            );
            assert!(grep(json!({ "count": true })).is_err());
        });
    }

    #[test]
    fn a_grep_takes_a_regular_expression_and_a_path_scope() {
        let root = crate::system::files::create_unique_directory(
            &std::env::temp_dir(),
            "renium-script-grep-",
        )
        .unwrap();
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = std::fs::remove_dir_all(&root);
        });
        let source = root.join("src");
        std::fs::create_dir_all(source.join("Server")).unwrap();
        std::fs::create_dir_all(source.join("ServerOther")).unwrap();
        std::fs::write(
            source.join("Server/Shop.server.luau"),
            "local function grantSpins()\nend\nGrantSpins = grantSpins\n",
        )
        .unwrap();
        std::fs::write(
            source.join("ServerOther/Wheel.luau"),
            "api.GrantRevive(player)\n",
        )
        .unwrap();
        let grep = |parameters: serde_json::Value| {
            script_grep_at(&root, &source, &parameters).map_err(|failure| failure.0.m)
        };
        let hits = |value: &serde_json::Value| {
            value["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|result| {
                    (
                        result["path"].as_str().unwrap().to_string(),
                        result["hits"].as_array().unwrap().len(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let either = grep(json!({ "query": "GrantSpins|GrantRevive", "regex": true })).unwrap();
        assert_eq!(
            hits(&either),
            vec![
                ("src/Server/Shop.server.luau".to_string(), 1),
                ("src/ServerOther/Wheel.luau".to_string(), 1)
            ]
        );
        let literal = grep(json!({ "query": "GrantSpins|GrantRevive" })).unwrap();
        assert_eq!(literal["totalMatches"], json!(0));
        let folded =
            grep(json!({ "query": "^grant", "regex": true, "caseInsensitive": true })).unwrap();
        assert_eq!(
            hits(&folded),
            vec![("src/Server/Shop.server.luau".to_string(), 1)]
        );
        let scoped = grep(json!({ "query": "Grant", "scope": "src\\Server/" })).unwrap();
        assert_eq!(
            hits(&scoped),
            vec![("src/Server/Shop.server.luau".to_string(), 1)]
        );
        let one_file =
            grep(json!({ "query": "Grant", "scope": "./src/ServerOther/Wheel.luau" })).unwrap();
        assert_eq!(
            hits(&one_file),
            vec![("src/ServerOther/Wheel.luau".to_string(), 1)]
        );
        let around =
            grep(json!({ "query": "Grant", "scope": "src/Server", "context": 1 })).unwrap();
        let listed = around["results"][0]["hits"].as_array().unwrap();
        assert!(listed.len() > 1, "{around}");
        assert_eq!(
            listed.iter().filter(|hit| hit["context"] != true).count(),
            1,
            "{around}"
        );
        assert!(script_hits_text(&around).contains("- "), "{around}");
        let cut = grep(json!({ "query": "rant", "scope": "src/Server", "context": 2, "limit": 1 }))
            .unwrap();
        let lines = cut["results"][0]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| (hit["line"].as_u64().unwrap(), hit["context"] == true))
            .collect::<Vec<_>>();
        assert_eq!(lines, [(1, false), (2, true), (3, true)], "{cut}");
        let both = grep(json!({
            "query": "Grant",
            "scope": ["src/Server", "src/ServerOther/Wheel.luau"],
        }))
        .unwrap();
        assert_eq!(hits(&both).len(), 2);
        let broken = grep(json!({ "query": "(", "regex": true })).err().unwrap();
        assert!(broken.contains("Invalid regular expression"));
        let counted =
            grep(json!({ "query": "grant", "caseInsensitive": true, "count": true })).unwrap();
        assert_eq!(
            script_hits_text(&counted),
            "src/Server/Shop.server.luau 2\nsrc/ServerOther/Wheel.luau 1"
        );
        let first =
            grep(json!({ "query": "grant", "caseInsensitive": true, "count": true, "limit": 1 }))
                .unwrap();
        assert_eq!(
            script_hits_text(&first),
            "src/Server/Shop.server.luau 2\n1 of 2 scripts shown; -l N shows more"
        );
    }

    #[test]
    fn grep_hits_print_one_path_per_script_and_unindented_lines() {
        let value = json!({
            "results": [
                { "path": "src/A.server.luau", "hits": [
                    { "line": 7, "text": "\t\tif api.Grant(player) then" },
                    { "line": 12, "text": "    api.Grant(player, 2)  " },
                ]},
                { "path": "src/B.luau", "hits": [{ "line": 3, "text": "Grant = grant," }] },
            ],
            "totalMatches": 3,
        });
        assert_eq!(
            script_hits_text(&value),
            "src/A.server.luau\n7: if api.Grant(player) then\n12: api.Grant(player, 2)\nsrc/B.luau\n3: Grant = grant,"
        );
    }

    #[test]
    fn hits_the_limit_left_out_are_counted_in_a_last_line() {
        let grep = json!({
            "results": [{ "path": "src/A.luau", "hits": [{ "line": 1, "text": "x" }] }],
            "totalMatches": 40,
            "truncated": true,
        });
        assert_eq!(
            script_hits_text(&grep),
            "src/A.luau\n1: x\n1 of 40 lines shown; -l N shows more"
        );
        let search = json!({
            "results": [{ "path": "src/A.luau", "matches": [{ "line": 1, "text": "x" }] }],
            "totalFiles": 9,
            "truncated": true,
        });
        assert_eq!(
            script_hits_text(&search),
            "src/A.luau\n1 of 9 scripts shown; -l N shows more"
        );
    }

    #[test]
    fn a_long_line_is_cut_and_no_hits_say_so() {
        let long = "\u{e9}".repeat(HIT_TEXT_LIMIT + 10);
        let value = json!({
            "results": [{ "path": "src/A.luau", "hits": [{ "line": 1, "text": long }] }],
            "totalMatches": 1,
        });
        let text = script_hits_text(&value);
        let line = text.lines().nth(1).unwrap();
        assert_eq!(line.chars().count(), "1: ".len() + HIT_TEXT_LIMIT + 1);
        assert!(line.ends_with('\u{2026}'));
        assert_eq!(
            script_hits_text(&json!({ "results": [], "totalMatches": 0 })),
            "no matches"
        );
    }
}
