use std::cell::OnceCell;
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
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

/// In text mode a script of this many lines or more, read without lines,
/// prints as its outline.
const OUTLINE_FROM_LINES: usize = 120;

/// In text mode a function grep prints a function longer than this many lines
/// as its hits with `EXCERPT_CONTEXT` lines around each.
const EXCERPT_FROM_LINES: usize = 60;
const EXCERPT_CONTEXT: usize = 3;

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
    offset: Option<usize>,
    #[serde(default, deserialize_with = "one_or_several")]
    not: Vec<String>,
}

/// A search scope or an excluded text given as one string or as a list of
/// them.
fn one_or_several<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Texts {
        One(String),
        Several(Vec<String>),
    }
    Ok(match Option::<Texts>::deserialize(deserializer)? {
        Some(Texts::One(text)) => vec![text],
        Some(Texts::Several(texts)) => texts,
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
/// `range` stands in for the line number of a hit on the first line of the
/// function it gives the lines of: `FIRST-LAST: text`.
fn push_hit_line(text: &mut String, hit: &Value, range: Option<&str>) {
    let line = hit["text"].as_str().unwrap_or_default().trim();
    let mark = if hit["context"] == json!(true) {
        '-'
    } else {
        ':'
    };
    match range {
        Some(range) => text.push_str(&format!("{range}{mark} ")),
        None => text.push_str(&format!("{}{mark} ", hit["line"])),
    }
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
/// or its hit lines, with the lines around them for a long function, when
/// they come in place of the source; returns the number of lines after the
/// first.
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
        push_hit_line(text, hit, None);
    }
    hits.len() as u64
}

/// One line of a grep whose `around` is the function it is the first shown
/// line of: a `FIRST-LAST name` line and then the line, or for a hit on the
/// function's own first line, which names it, the hit as `FIRST-LAST: text`.
fn push_enclosed_line(text: &mut String, hit: &Value, around: Option<&Value>) {
    let Some(function) = around else {
        return push_hit_line(text, hit, None);
    };
    let range = format!("{}-{}", function["startLine"], function["endLine"]);
    if hit["context"] != json!(true) && hit["line"] == function["startLine"] {
        return push_hit_line(text, hit, Some(&range));
    }
    text.push_str(&format!(
        "{range} {}\n",
        function["name"].as_str().unwrap_or_default()
    ));
    push_hit_line(text, hit, None);
}

/// The lines under one script's path: the parse error of a script whose
/// functions could not be read, then its functions and hits in line order,
/// the hits of a function under the lines and name of that function; returns
/// the number of source and hit lines.
fn push_script_lines(text: &mut String, result: &Value) -> u64 {
    if let Some(error) = result.get("parseError").and_then(Value::as_str) {
        text.push_str(&format!("does not parse ({error}); matching lines:\n"));
    }
    let mut lines = 0;
    let mut functions = items(result, "functions").iter().peekable();
    let mut enclosing = items(result, "enclosing").iter().peekable();
    for hit in items(result, "hits") {
        let line = hit["line"].as_u64();
        while let Some(function) = functions.next_if(|next| next["startLine"].as_u64() < line) {
            lines += push_function(text, function);
        }
        let around = enclosing.next_if(|next| next["startLine"].as_u64() <= line);
        push_enclosed_line(text, hit, around);
        lines += u64::from(hit["context"] != json!(true));
    }
    for function in functions {
        lines += push_function(text, function);
    }
    lines
}

/// A script's row: its path, then where the result carries them its number
/// of matching lines and its length, `path 3/210 lines`.
fn push_script_row(text: &mut String, result: &Value) {
    text.push_str(result["path"].as_str().unwrap_or_default());
    match (result.get("count"), result.get("lines")) {
        (Some(count), Some(lines)) => text.push_str(&format!(" {count}/{lines} lines")),
        (Some(count), None) => text.push_str(&format!(" {count}")),
        (None, Some(lines)) => text.push_str(&format!(" {lines} lines")),
        (None, None) => {}
    }
    text.push('\n');
}

/// The last line of a search whose rows are not all of its matches: how many
/// of them are shown, as a range of the matching lines after `--offset`.
fn shown_line(value: &Value, scripts: usize, lines: u64) -> Option<String> {
    let truncated = value["truncated"] == json!(true);
    let more = if truncated { "; -l N shows more" } else { "" };
    let total_lines = value.get("totalLines").unwrap_or(&value["totalMatches"]);
    let total_lines = total_lines.as_u64().unwrap_or(0);
    let offset = value.get("offset").and_then(Value::as_u64);
    if let Some(offset) = offset.filter(|_| total_lines > 0) {
        return Some(match lines {
            0 => format!("--offset {offset} skips all {total_lines} matching lines"),
            _ => format!(
                "{}-{} of {total_lines} lines shown{more}",
                offset + 1,
                offset + lines
            ),
        });
    }
    let (shown, total, unit) = match value.get("totalFiles") {
        Some(total) => (scripts as u64, total.as_u64().unwrap_or(0), "scripts"),
        None => (lines, total_lines, "lines"),
    };
    truncated.then(|| format!("{shown} of {total} {unit} shown{more}"))
}

/// Search hits as text: each script's path on its own line, for a grep
/// followed by its hits as `LINE: text` without the indentation and with a
/// long line cut short, and a last line when the limit or `--offset` left
/// hits out. A script search names the scripts and their lengths; `rbx sg`
/// shows the lines, the hits inside a function under a `FIRST-LAST name` line
/// (a hit on the function's own first line as `FIRST-LAST: text` instead),
/// with `--count` each script's path, its number of matching lines and its
/// length, and with `--fn` each function that holds a hit as a `FIRST-LAST
/// name` line and its source (its hit lines when they come in place of the
/// source), in line order with the hits outside every function. A `note`
/// comes first, alone when nothing matched.
fn script_hits_text(value: &Value) -> String {
    let results = items(value, "results");
    let note = value.get("note").and_then(Value::as_str);
    let mut text = String::new();
    let mut lines = 0u64;
    for result in results {
        push_script_row(&mut text, result);
        lines += push_script_lines(&mut text, result);
    }
    if let Some(shown) = shown_line(value, results.len(), lines) {
        text.push_str(&shown);
    }
    text.truncate(text.trim_end().len());
    match (note, text.is_empty()) {
        (Some(note), true) => note.to_string(),
        (Some(note), false) => format!("{note}\n{text}"),
        (None, true) => "no matches".to_string(),
        (None, false) => text,
    }
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
        crate::app::output::compact_text_output(),
    ))
}

/// The result of a literal query that holds `|` and matched nothing, after it
/// ran again as a regular expression: those matches under a first line that
/// says so, or the note that neither reading matched.
fn retried_as_pattern(literal: Value, pattern: Result<Value, Failure>) -> Value {
    let (mut value, note) = match pattern {
        Ok(matched) if matched["totalMatches"] != json!(0) => {
            (matched, "no literal match; as a pattern (-E):")
        }
        Ok(_) => (literal, "no matches, also none as a pattern (-E)"),
        Err(_) => return literal,
    };
    value["note"] = json!(note);
    value
}

pub(crate) fn script_grep_command(
    args: ScriptGrepArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, source) = script_roots(project)?;
    let scope = args.scope.iter().chain(&args.paths).collect::<Vec<_>>();
    let request = json!({
        "query": args.query,
        "caseInsensitive": args.case_insensitive,
        "regex": args.regex,
        "count": args.count,
        "context": args.context,
        "scope": scope,
        "limit": args.limit,
        "fn": args.functions,
        "offset": args.offset,
        "not": args.not,
    });
    let text = crate::app::output::compact_text_output();
    print_script_hits(script_grep_printed(&root, &source, request, text))
}

/// The grep a command line prints. In text mode a literal query that holds
/// `|` and matches nothing runs again as a regular expression, and the result
/// says so in its `note`.
fn script_grep_printed(
    root: &Path,
    source: &Path,
    mut request: Value,
    text: bool,
) -> Result<Value, Failure> {
    let literal = script_grep_shaped(root, source, &request, text)?;
    let alternatives = request["query"]
        .as_str()
        .is_some_and(|query| query.contains('|'));
    let unmatched = literal["totalMatches"] == json!(0);
    if !text || !alternatives || !unmatched || request["regex"] == json!(true) {
        return Ok(literal);
    }
    request["regex"] = json!(true);
    let pattern = script_grep_shaped(root, source, &request, text);
    Ok(retried_as_pattern(literal, pattern))
}

/// The first line of a long script that prints as its outline in place of its
/// source: its length, why its functions cannot be listed where it does not
/// parse, and how to read it.
fn outline_only_line(value: &Value) -> String {
    let total = &value["totalLines"];
    match value.get("parseError").and_then(Value::as_str) {
        Some(error) => format!(
            "{total} lines, does not parse ({error}): FILE:A-B reads a part, --all everything\n"
        ),
        None => format!(
            "{total} lines, outline only: FILE:A-B or --fn NAME reads a part, --all everything\n"
        ),
    }
}

/// A script read as text: a line range as `FIRST-LAST/TOTAL` and its source,
/// an outline as `TOTAL lines` and one `FIRST-LAST name(params)` line per
/// function, indented one space per enclosing function, and functions read by
/// name each as `FIRST-LAST/TOTAL name` and its source. The outline that
/// stands in for a long script's source says so in its first line.
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
    if value["outlineOnly"] == json!(true) {
        text.push_str(&outline_only_line(value));
    } else if functions
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
/// target, after the target's outline where `--outline` comes with ranges,
/// and one for a target without lines, which takes `--start-line` and
/// `--end-line`.
fn read_requests(args: &ScriptReadArgs) -> anyhow::Result<Vec<Value>> {
    let mut requests = Vec::new();
    for target in &args.targets {
        let (path, ranges) = split_target(target);
        if ranges.is_empty() {
            requests.push(json!({
                "path": path,
                "startLine": args.start_line,
                "endLine": args.end_line,
                "outline": args.outline,
                "fn": args.functions,
            }));
            continue;
        }
        if args.start_line.is_some() || args.end_line.is_some() {
            anyhow::bail!(
                "{target} names its lines; give them there or with --start-line/--end-line, not both"
            );
        }
        if args.outline {
            requests.push(json!({ "path": path, "outline": true }));
        }
        for (first, last) in ranges {
            requests.push(json!({
                "path": path,
                "startLine": first,
                "endLine": last,
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
    let (root, source) = script_roots(project)?;
    let mapping = ScriptMapping::new(&root, &source);
    let outline_from =
        (crate::app::output::compact_text_output() && !args.all).then_some(OUTLINE_FROM_LINES);
    let values = read_requests(&args)?
        .iter()
        .map(|request| script_read_with(&mapping, request, outline_from))
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

/// The name a script file goes by without its folder: its file name without
/// the role suffix and the extension (`Shop` for `Shop.server.luau`), and for
/// an `init` file the name of its folder.
fn script_name(path: &Path, naming: &config::ProjectScriptNaming) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    match crate::editor::paths::infer_source_script(file_name, naming) {
        Some((_, Some(name), _)) => Some(name),
        Some((_, None, _)) => Some(path.parent()?.file_name()?.to_str()?.to_string()),
        None => Some(path.file_stem()?.to_str()?.to_string()),
    }
}

/// The role suffixes and extensions the project at `root` names its script
/// files with; the default ones where no project file is there.
fn script_naming(root: &Path) -> config::ProjectScriptNaming {
    let same_folder = |found: &Path| {
        canonical_path(found).is_ok_and(|found| canonical_path(root).ok() == Some(found))
    };
    config::try_load_project(None, Some(root))
        .ok()
        .flatten()
        .filter(|loaded| same_folder(&loaded.root))
        .map(|loaded| config::project_script_naming(&loaded.project))
        .unwrap_or_default()
}

const NAMED_SCRIPTS_LISTED: usize = 6;

/// The one script file under `source` that goes by `name`. No script by that
/// name is `None`; a name several scripts share is an error that lists their
/// paths.
pub(crate) fn script_by_name(
    root: &Path,
    source: &Path,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    if name.is_empty() || name.contains(['/', '\\']) {
        return Ok(None);
    }
    let naming = script_naming(root);
    let mut named = script_files(source)
        .into_iter()
        .filter(|path| script_name(path, &naming).as_deref() == Some(name))
        .collect::<Vec<_>>();
    if named.len() < 2 {
        return Ok(named.pop());
    }
    let mut listed = named
        .iter()
        .take(NAMED_SCRIPTS_LISTED)
        .map(|path| relative(root, path))
        .collect::<Vec<_>>()
        .join(", ");
    if named.len() > NAMED_SCRIPTS_LISTED {
        listed.push_str(&format!(" and {} more", named.len() - NAMED_SCRIPTS_LISTED));
    }
    Err(format!(
        "{name} names {} scripts: {listed}; pass one by its path",
        named.len()
    ))
}

/// The script file a command line names by a script's name alone, in the
/// project found from the working directory or given with `--project`. A
/// path with a folder, and a name no script goes by, is `None`.
pub(crate) fn project_script_by_name(
    project: Option<&Path>,
    name: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(name) = name.to_str() else {
        return Ok(None);
    };
    let Some(loaded) = config::try_load_project(project, None)? else {
        return Ok(None);
    };
    let source = loaded.root.join(&loaded.project.source_root);
    script_by_name(&loaded.root, &source, name).map_err(anyhow::Error::msg)
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

/// The scripts that hold every keyword, each with its first matching lines.
/// A result made for `text` also carries each script's number of `lines`.
fn script_search_at(
    root: &Path,
    source: &Path,
    parameters: &Value,
    text: bool,
) -> Result<Value, Failure> {
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
                let mut result = json!({ "path": relative, "matches": lines });
                if text {
                    result["lines"] = json!(source.lines().count());
                }
                results.push(result);
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
        false,
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
            "No script at {requested}: it is not a file or a script's name, and the project has no instance at that path; rbx ss NAME finds scripts"
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
    source: &'a Path,
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
    pub(super) fn new(root: &'a Path, source: &'a Path) -> Self {
        Self {
            root,
            source,
            saved: OnceCell::new(),
            built: OnceCell::new(),
        }
    }

    /// Whether the project places files in the tree itself (tree nodes with
    /// a path, or mounts), which can put a script at the top of the place
    /// under a name its file does not have.
    fn maps_its_own_tree(&self) -> bool {
        config::try_load_project(None, Some(self.root))
            .ok()
            .flatten()
            .filter(|loaded| loaded.root == self.root)
            .is_some_and(|loaded| {
                !loaded.project.mounts.is_empty()
                    || config::project_tree_nodes(&loaded.project.tree)
                        .iter()
                        .any(|(_, node)| node.path.is_some())
            })
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

/// The script file a read names, tried in this order: a file path, an
/// instance path (`ServerScriptService.Main`) that the project's script
/// mapping leads to a file, and the name one script alone goes by (`Main`).
/// A request without a dot could only be an instance at the top of the place.
/// No script is there unless the project maps its tree itself, so without
/// such a mapping the name is looked up first and the script mapping is not
/// loaded for it.
pub(super) fn resolve_read_target(
    mapping: &ScriptMapping,
    requested: &str,
) -> Result<PathBuf, Failure> {
    let root = mapping.root;
    let names_file = requested.contains(['/', '\\']) || has_script_extension(Path::new(requested));
    if names_file || root.join(requested).is_file() {
        return resolve_script(root, requested, "script-read");
    }
    let named = || {
        script_by_name(root, mapping.source, requested)
            .map_err(|shared| failure(shared, "script-read"))
    };
    let name_only = !requested.contains('.') && !mapping.maps_its_own_tree();
    if name_only && let Some(path) = named()? {
        return resolve_script(root, &path.to_string_lossy(), "script-read");
    }
    let unmapped = match mapping.file(requested) {
        Ok(file) => return resolve_script(root, file, "script-read"),
        Err(unmapped) => unmapped,
    };
    if name_only {
        return Err(unmapped);
    }
    match named()? {
        Some(path) => resolve_script(root, &path.to_string_lossy(), "script-read"),
        None => Err(unmapped),
    }
}

fn script_read_at(root: &Path, source: &Path, parameters: &Value) -> Result<Value, Failure> {
    script_read_with(&ScriptMapping::new(root, source), parameters, None)
}

/// One read of a script. With `outline_from`, a script of that many lines or
/// more that is asked for without lines, outline or function names returns
/// its outline, marked `outlineOnly`, in place of its source.
fn script_read_with(
    mapping: &ScriptMapping,
    parameters: &Value,
    outline_from: Option<usize>,
) -> Result<Value, Failure> {
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
    let ranged = request.start_line.is_some() || request.end_line.is_some();
    if !ranged && outline_from.is_some_and(|from| lines.len() >= from) {
        return Ok(outline_in_place_of_source(
            &relative(root, &path),
            &source,
            lines.len(),
        ));
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

/// The functions of an outline: the name, parameters, lines and depth of
/// each.
fn outline_entries(functions: &[LuauFunction]) -> Vec<Value> {
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
        .collect()
}

/// The outline a long script returns in place of its source, marked
/// `outlineOnly`. A script that does not parse returns its `parseError` and
/// no functions.
fn outline_in_place_of_source(path: &str, source: &str, total_lines: usize) -> Value {
    let mut value = json!({ "path": path, "totalLines": total_lines, "outlineOnly": true });
    match luau_functions(source) {
        Ok(functions) => value["functions"] = json!(outline_entries(&functions)),
        Err(error) => value["parseError"] = json!(error),
    }
    value
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
        outline_entries(&functions)
    } else {
        named_functions(path, &functions, &request.functions)?
            .into_iter()
            .map(|function| function_entry(function, "source", function_source(function, lines)))
            .collect()
    };
    Ok(json!({ "path": path, "totalLines": lines.len(), "functions": listed }))
}

pub(crate) fn script_read(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    script_read_at(
        Path::new(&context.root),
        Path::new(&context.source),
        parameters,
    )
}

fn script_grep_at(root: &Path, source: &Path, parameters: &Value) -> Result<Value, Failure> {
    script_grep_shaped(root, source, parameters, false)
}

/// A grep of the project's scripts. A result made for `text` carries what
/// only the text form shows: each counted script's number of `lines`, the
/// `enclosing` functions of a script's hits, and with `fn` a long function's
/// hits and the lines around them in place of its source.
fn script_grep_shaped(
    root: &Path,
    source: &Path,
    parameters: &Value,
    text: bool,
) -> Result<Value, Failure> {
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
    let scopes = scope_prefixes(root, source, &request.scope)?;
    let excluded = excluded_text(&request);
    let files = script_files(source).into_iter().filter(|path| {
        let relative = relative(root, path);
        let scoped = scopes.is_empty() || scopes.iter().any(|scope| in_scope(&relative, scope));
        scoped && !excluded(&relative)
    });
    if request.functions {
        if request.count || request.offset.is_some() {
            return Err(failure(
                "script-grep takes fn without count and offset",
                "script-grep",
            ));
        }
        let limit = request.limit.unwrap_or(200).clamp(1, 1000);
        return script_grep_functions(root, files, &matches, limit, text);
    }
    let limit = request.limit.unwrap_or(50).clamp(1, 1000);
    if request.count {
        if request.offset.is_some() {
            return Err(failure(
                "script-grep takes one of count and offset",
                "script-grep",
            ));
        }
        return script_grep_counts(root, files, &matches, limit, text);
    }
    let window = HitWindow {
        offset: request.offset.unwrap_or(0),
        limit,
        context: request.context.unwrap_or(0),
    };
    script_grep_lines(root, files, &matches, window, text)
}

/// A grep that counts: per script its number of matching lines, for `limit`
/// scripts.
fn script_grep_counts(
    root: &Path,
    files: impl Iterator<Item = PathBuf>,
    matches: &LineMatcher,
    limit: usize,
    text: bool,
) -> Result<Value, Failure> {
    let mut results: Vec<Value> = Vec::new();
    let mut total_matches = 0;
    let mut matching_files = 0;
    for path in files {
        let source = read_script(&path, "script-grep")?;
        let count = source.lines().filter(|line| matches(line)).count();
        if count == 0 {
            continue;
        }
        total_matches += count;
        matching_files += 1;
        if results.len() < limit {
            let mut result = json!({ "path": relative(root, &path), "count": count });
            if text {
                result["lines"] = json!(source.lines().count());
            }
            results.push(result);
        }
    }
    let listed = results.len();
    let mut result = json!({ "results": results, "totalMatches": total_matches });
    if listed < matching_files {
        result["truncated"] = json!(true);
        result["totalFiles"] = json!(matching_files);
    }
    Ok(result)
}

/// The matching lines a grep shows: `limit` of them after the first `offset`,
/// each with `context` lines around it.
#[derive(Clone, Copy)]
struct HitWindow {
    offset: usize,
    limit: usize,
    context: usize,
}

/// A grep that returns lines: per script its `hits` inside the window, in
/// line order with the context lines around them. `totalMatches` counts every
/// matching line, and a window that starts after the first carries its
/// `offset`.
fn script_grep_lines(
    root: &Path,
    files: impl Iterator<Item = PathBuf>,
    matches: &LineMatcher,
    window: HitWindow,
    text: bool,
) -> Result<Value, Failure> {
    // Matches are grouped by file: one path per script instead of one per line.
    let mut results: Vec<Value> = Vec::new();
    let mut scripts: Vec<(String, Vec<usize>)> = Vec::new();
    let mut returned = 0;
    let mut total_matches = 0;
    for path in files {
        let source = read_script(&path, "script-grep")?;
        let lines = source.lines().collect::<Vec<_>>();
        let around =
            |line: usize| json!({ "line": line, "text": lines[line - 1], "context": true });
        let mut hits = Vec::new();
        let mut shown = Vec::new();
        let (mut listed, mut after) = (0, 0);
        for (index, line) in lines.iter().enumerate() {
            let matched = matches(line);
            total_matches += usize::from(matched);
            if matched && total_matches > window.offset && returned < window.limit {
                returned += 1;
                let before = (index + 1).saturating_sub(window.context).max(listed + 1);
                hits.extend((before..=index).map(around));
                hits.push(json!({ "line": index + 1, "text": line }));
                shown.push(index + 1);
                (listed, after) = (index + 1, window.context);
            } else if after > 0 {
                hits.push(around(index + 1));
                (listed, after) = (index + 1, after - 1);
            }
        }
        if hits.is_empty() {
            continue;
        }
        results.push(json!({ "path": relative(root, &path), "hits": hits }));
        if text {
            scripts.push((source, shown));
        }
    }
    add_enclosing_functions(&mut results, &scripts);
    let mut result = json!({ "results": results, "totalMatches": total_matches });
    if window.offset.min(total_matches) + returned < total_matches {
        result["truncated"] = json!(true);
    }
    if window.offset > 0 {
        result["offset"] = json!(window.offset);
    }
    Ok(result)
}

/// Adds to each script's result the functions around its shown hits, from
/// that script's source and hit lines. Every script with a hit is parsed for
/// this, so they are parsed side by side.
fn add_enclosing_functions(results: &mut [Value], scripts: &[(String, Vec<usize>)]) {
    let enclosing = scripts
        .par_iter()
        .map(|(source, hits)| enclosing_functions(source, hits))
        .collect::<Vec<_>>();
    for (result, enclosing) in results.iter_mut().zip(enclosing) {
        if let Some(enclosing) = enclosing {
            result["enclosing"] = enclosing;
        }
    }
}

/// The shortest name `--fn` takes for this function alone: the last segment
/// of its printed name, or the whole name where another function of the
/// script answers to that segment too.
fn function_handle<'a>(function: &'a LuauFunction, functions: &[LuauFunction]) -> &'a str {
    let last = function.last_segment();
    let answering = functions
        .iter()
        .filter(|other| other.is_named(last))
        .count();
    if answering == 1 { last } else { &function.name }
}

/// The functions around a script's shown hits, in line order, each once with
/// the name `--fn` takes and its lines. A script that does not parse, and one
/// whose hits are all outside its functions, has none.
fn enclosing_functions(source: &str, hits: &[usize]) -> Option<Value> {
    let functions = luau_functions(source).ok()?;
    let enclosing = hits_by_function(&functions, hits)
        .into_iter()
        .filter_map(|(function, _)| function)
        .map(|function| {
            json!({
                "name": function_handle(function, &functions),
                "startLine": function.first_line,
                "endLine": function.last_line,
            })
        })
        .collect::<Vec<_>>();
    (!enclosing.is_empty()).then(|| json!(enclosing))
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

/// The lines a long function shows in place of its source: its hits and the
/// `EXCERPT_CONTEXT` lines around each inside the function, in line order,
/// the lines that are no hit marked as `context`.
fn excerpt(function: &LuauFunction, hits: &[usize], lines: &[&str]) -> Vec<Value> {
    let mut shown = std::collections::BTreeSet::new();
    for &hit in hits {
        let first = hit.saturating_sub(EXCERPT_CONTEXT).max(function.first_line);
        let last = (hit + EXCERPT_CONTEXT).min(function.last_line);
        shown.extend(first..=last);
    }
    shown
        .into_iter()
        .map(|line| match hits.binary_search(&line) {
            Ok(_) => json!({ "line": line, "text": lines[line - 1] }),
            Err(_) => json!({ "line": line, "text": lines[line - 1], "context": true }),
        })
        .collect()
}

/// Spends what is `left` of a function grep's line limit on one script's
/// grouped hits and returns the `functions` and `hits` to show and the lines
/// they take without a limit. A function comes with its source while that
/// fits in what is left; one that does not comes with its hit lines instead,
/// one line of the limit each like a hit outside the functions. With
/// `excerpts`, a function longer than `EXCERPT_FROM_LINES` whose first line
/// is no hit comes with its hits and the lines around them, which count
/// toward the limit in place of its source.
fn spend_lines(
    groups: Vec<FunctionHits>,
    lines: &[&str],
    left: &mut usize,
    excerpts: bool,
) -> (Vec<Value>, Vec<Value>, usize) {
    let mut functions = Vec::new();
    let mut outside = Vec::new();
    let mut total = 0;
    for (function, hits) in groups {
        let excerpt = function
            .filter(|function| {
                excerpts
                    && function.line_count() > EXCERPT_FROM_LINES
                    && hits[0] != function.first_line
            })
            .map(|function| excerpt(function, &hits, lines));
        let length = match &excerpt {
            Some(excerpt) => excerpt.len(),
            None => function.map_or(1, LuauFunction::line_count),
        };
        total += length;
        if let Some(function) = function.filter(|_| length <= *left) {
            *left -= length;
            functions.push(match excerpt {
                Some(excerpt) => function_entry(function, "hits", json!(excerpt)),
                None => function_entry(function, "source", function_source(function, lines)),
            });
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
/// and `totalLines` counts the lines without the limit. A result made for
/// `text` returns a long function as an excerpt, as `spend_lines` describes.
fn script_grep_functions(
    root: &Path,
    files: impl Iterator<Item = PathBuf>,
    matches: &LineMatcher,
    limit: usize,
    text: bool,
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
        let (shown_functions, shown_hits, script_lines) =
            spend_lines(groups, &lines, &mut left, text);
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

/// The test a grep applies to each line: the query matches it and it holds
/// none of the excluded texts.
fn line_matcher(request: &ScriptGrep) -> Result<LineMatcher, Failure> {
    let matches = query_matcher(request)?;
    if request.not.is_empty() {
        return Ok(matches);
    }
    let excluded = excluded_text(request);
    Ok(Box::new(move |line| matches(line) && !excluded(line)))
}

/// Whether a line, or a script's path, holds one of the texts a grep leaves
/// out, with or without regard to case as the query is read.
fn excluded_text(request: &ScriptGrep) -> LineMatcher {
    let fold = request.case_insensitive;
    let texts = request
        .not
        .iter()
        .filter(|text| !text.is_empty())
        .map(|text| match fold {
            true => text.to_ascii_lowercase(),
            false => text.clone(),
        })
        .collect::<Vec<_>>();
    Box::new(move |line| {
        let folded = fold.then(|| line.to_ascii_lowercase());
        let line = folded.as_deref().unwrap_or(line);
        texts.iter().any(|text| line.contains(text.as_str()))
    })
}

/// The query as a test of one line: literal text, or a regular expression
/// when asked, with or without regard to case.
fn query_matcher(request: &ScriptGrep) -> Result<LineMatcher, Failure> {
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

/// The `--in` scopes as path prefixes: a file or folder under the project as
/// it is given, anything else as the file of the one script that goes by that
/// name. A path that is not there matches nothing, as a folder that may be
/// absent always did; a bare word that is neither a path nor a script's name
/// is an error, since it was meant as one of the two.
fn scope_prefixes(root: &Path, source: &Path, scopes: &[String]) -> Result<Vec<String>, Failure> {
    scopes
        .iter()
        .map(|scope| {
            let prefix = scope_prefix(scope);
            if root.join(&prefix).exists() {
                return Ok(prefix);
            }
            match script_by_name(root, source, &prefix) {
                Ok(Some(path)) => Ok(relative(root, &path)),
                Ok(None)
                    if prefix.contains('/') || has_script_extension(Path::new(&prefix)) =>
                {
                    Ok(prefix)
                }
                Ok(None) => Err(failure(
                    format!(
                        "--in {scope}: the project has no file or folder at that path and no script by that name"
                    ),
                    "script-grep",
                )),
                Err(shared) => Err(failure(shared, "script-grep")),
            }
        })
        .collect()
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
    use std::path::Path;

    use clap::Parser;
    use serde_json::{Value, json};

    use super::{
        HIT_TEXT_LIMIT, OUTLINE_FROM_LINES, ScriptMapping, read_requests, script_block_text,
        script_by_name, script_grep_at, script_grep_printed, script_grep_shaped, script_hits_text,
        script_read_at, script_read_text, script_read_with, script_search_at,
    };
    use crate::cli::ScriptReadArgs;

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
            let path = source.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
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
        with_scripts(&[("Shop.luau", SHOP)], |root, source| {
            let read = |mut parameters: serde_json::Value| {
                parameters["path"] = json!("src/Shop.luau");
                script_read_at(root, source, &parameters)
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
            let broken = json!({ "path": "src/Broken.luau", "outline": true });
            let error = script_read_at(root, source, &broken)
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

    type Printed = Result<String, String>;

    /// A grep as the command line prints it in text mode.
    fn grep_text(root: &Path, source: &Path, parameters: Value) -> Printed {
        script_grep_printed(root, source, parameters, true)
            .map(|value| script_hits_text(&value))
            .map_err(|failure| failure.0.m)
    }

    #[test]
    fn search_and_count_rows_carry_each_scripts_length_in_text_only() {
        with_scripts(&[("Rounds.luau", ROUNDS)], |root, source| {
            let search = |text: bool| {
                script_search_at(root, source, &json!({ "keywords": "total" }), text)
                    .map_err(|failure| failure.0.m)
                    .unwrap()
            };
            assert_eq!(script_hits_text(&search(true)), "src/Rounds.luau 20 lines");
            assert_eq!(search(false)["results"][0].get("lines"), None);
            assert_eq!(script_hits_text(&search(false)), "src/Rounds.luau");
            let counted = json!({ "query": "total", "count": true });
            assert_eq!(
                grep_text(root, source, counted.clone()).unwrap(),
                "src/Rounds.luau 6/20 lines"
            );
            let value = script_grep_at(root, source, &counted)
                .map_err(|failure| failure.0.m)
                .unwrap();
            assert_eq!(
                value,
                json!({ "results": [{ "path": "src/Rounds.luau", "count": 6 }], "totalMatches": 6 })
            );
        });
    }

    #[test]
    fn grep_hits_come_under_the_lines_and_name_of_their_function() {
        let twins = "function A.run()\n\treturn 1\nend\nfunction B.run()\n\treturn 2\nend\n";
        let files = [
            ("Rounds.luau", ROUNDS),
            ("Shop.luau", SHOP),
            ("Twins.luau", twins),
        ];
        with_scripts(&files, |root, source| {
            let grep = |query: &str, scope: &str| {
                let request = json!({ "query": query, "scope": format!("src/{scope}.luau") });
                grep_text(root, source, request).unwrap()
            };
            assert_eq!(
                grep("total", "Rounds"),
                "src/Rounds.luau\n1: local total = 0\n2-5 add\n3: total += value\n4: return total\n7-9 inner\n8: return total\n12-18 both\n13: local seen = total\n15: return total"
            );
            assert_eq!(
                grep("return", "Shop"),
                "src/Shop.luau\n2-7 buy\n4: return 5\n6: return price()\n10: return Shop"
            );
            assert_eq!(
                grep("function Shop.buy", "Shop"),
                "src/Shop.luau\n2-7: function Shop.buy(player, id)"
            );
            assert_eq!(
                grep("return", "Twins"),
                "src/Twins.luau\n1-3 A.run\n2: return 1\n4-6 B.run\n5: return 2"
            );
            let around = json!({ "query": "seen", "context": 1 });
            assert_eq!(
                grep_text(root, source, around).unwrap(),
                "src/Rounds.luau\n12-18 both\n12- local function both()\n13: local seen = total\n14- local function nested()\n16- end\n17: return nested() + seen\n18- end"
            );
            let value = script_grep_at(root, source, &json!({ "query": "seen" }))
                .map_err(|failure| failure.0.m)
                .unwrap();
            assert_eq!(
                value,
                json!({
                    "results": [{ "path": "src/Rounds.luau", "hits": [
                        { "line": 13, "text": "\tlocal seen = total" },
                        { "line": 17, "text": "\treturn nested() + seen" },
                    ]}],
                    "totalMatches": 2,
                })
            );
        });
    }

    /// A script of `body + 4` lines whose function `long` runs from line 2
    /// through line `body + 3`, with `local vN = N` on line `N + 3`.
    fn long_script(body: usize) -> String {
        let mut text = String::from("local total = 0\nlocal function long()\n");
        for index in 0..body {
            text.push_str(&format!("\tlocal v{index} = {index}\n"));
        }
        text.push_str("end\nreturn long\n");
        text
    }

    #[test]
    fn a_long_script_read_without_lines_prints_its_outline() {
        let long = long_script(116);
        let short = long_script(115);
        let broken = format!("{long}local x = )\n");
        let files = [
            ("Long.luau", long.as_str()),
            ("Short.luau", short.as_str()),
            ("Broken.luau", broken.as_str()),
        ];
        with_scripts(&files, |root, source| {
            let mapping = ScriptMapping::new(root, source);
            let read = |parameters: Value, outline_from: Option<usize>| {
                script_read_with(&mapping, &parameters, outline_from)
                    .map_err(|failure| failure.0.m)
                    .unwrap()
            };
            let text_mode = Some(OUTLINE_FROM_LINES);
            let outline = read(json!({ "path": "src/Long.luau" }), text_mode);
            assert_eq!(
                script_read_text(&outline),
                "120 lines, outline only: FILE:A-B or --fn NAME reads a part, --all everything\n2-119 long()"
            );
            let whole = read(json!({ "path": "src/Short.luau" }), text_mode);
            assert_eq!(
                script_read_text(&whole),
                format!("1-119/119\n{}", short.trim_end())
            );
            let ranged = json!({ "path": "src/Long.luau", "startLine": 2, "endLine": 2 });
            assert_eq!(
                script_read_text(&read(ranged, text_mode)),
                "2-2/120\nlocal function long()"
            );
            let all = read(json!({ "path": "src/Long.luau" }), None);
            assert_eq!(
                script_read_text(&all),
                format!("1-120/120\n{}", long.trim_end())
            );
            let named = json!({ "path": "src/Long.luau", "fn": ["long"] });
            assert!(script_read_text(&read(named, text_mode)).starts_with("2-119/120 long\n"));
            let unparsed = script_read_text(&read(json!({ "path": "src/Broken.luau" }), text_mode));
            assert!(
                unparsed.starts_with("121 lines, does not parse (line 121: "),
                "{unparsed}"
            );
            assert!(
                unparsed.ends_with("): FILE:A-B reads a part, --all everything"),
                "{unparsed}"
            );
            assert_eq!(
                script_block_text(&outline),
                "== src/Long.luau\n120 lines, outline only: FILE:A-B or --fn NAME reads a part, --all everything\n2-119 long()"
            );
            assert!(
                script_block_text(&whole).starts_with("== src/Short.luau 1-119\nlocal total = 0\n")
            );
        });
    }

    #[test]
    fn an_outline_with_line_ranges_reads_the_outline_and_then_the_ranges() {
        let parse = |arguments: &[&str]| {
            ScriptReadArgs::try_parse_from(std::iter::once("sr").chain(arguments.iter().copied()))
        };
        let args = parse(&["src/Shop.luau:2-3,8", "src/Rounds.luau", "--outline"]).unwrap();
        assert_eq!(
            read_requests(&args).unwrap(),
            vec![
                json!({ "path": "src/Shop.luau", "outline": true }),
                json!({ "path": "src/Shop.luau", "startLine": 2, "endLine": 3, "fn": [] }),
                json!({ "path": "src/Shop.luau", "startLine": 8, "endLine": 8, "fn": [] }),
                json!({ "path": "src/Rounds.luau", "startLine": null, "endLine": null, "outline": true, "fn": [] }),
            ]
        );
        assert!(parse(&["src/Shop.luau", "--all"]).unwrap().all);
        assert!(parse(&["src/Shop.luau", "--all", "--outline"]).is_err());
        with_scripts(&[("Shop.luau", SHOP)], |root, source| {
            let mapping = ScriptMapping::new(root, source);
            let args = parse(&["src/Shop.luau:8-9", "--outline"]).unwrap();
            let blocks = read_requests(&args)
                .unwrap()
                .iter()
                .map(|request| script_read_with(&mapping, request, Some(OUTLINE_FROM_LINES)))
                .map(|value| script_block_text(&value.map_err(|failure| failure.0.m).unwrap()))
                .collect::<Vec<_>>();
            assert_eq!(
                blocks,
                [
                    "== src/Shop.luau\n10 lines\n2-7 Shop.buy(player, id)\n 3-5 price()\n8-9 Shop.sell(player)",
                    "== src/Shop.luau 8-9\nShop.sell = function(player)\nend",
                ]
            );
        });
    }

    #[test]
    fn a_function_grep_prints_a_long_function_as_its_hits_with_the_lines_around_them() {
        let long = long_script(70);
        let files = [("Long.luau", long.as_str())];
        with_scripts(&files, |root, source| {
            let grep = |query: &str, text: bool| {
                script_grep_shaped(root, source, &json!({ "query": query, "fn": true }), text)
                    .map_err(|failure| failure.0.m)
                    .unwrap()
            };
            let excerpt = grep("v40 =", true);
            assert_eq!(excerpt["totalLines"], json!(7));
            assert_eq!(
                script_hits_text(&excerpt),
                "src/Long.luau\n2-73 long\n40- local v37 = 37\n41- local v38 = 38\n42- local v39 = 39\n43: local v40 = 40\n44- local v41 = 41\n45- local v42 = 42\n46- local v43 = 43"
            );
            assert_eq!(
                script_hits_text(&grep("v0 =", true)),
                "src/Long.luau\n2-73 long\n2- local function long()\n3: local v0 = 0\n4- local v1 = 1\n5- local v2 = 2\n6- local v3 = 3"
            );
            let whole = |value: &Value| value["results"][0]["functions"][0]["source"].is_string();
            assert!(whole(&grep("local function long", true)));
            assert!(whole(&grep("v40 =", false)));
            assert_eq!(grep("v40 =", false)["totalLines"], json!(72));
        });
        let fits = long_script(58);
        with_scripts(&[("Fits.luau", fits.as_str())], |root, source| {
            let request = json!({ "query": "v40 =", "fn": true });
            let value = script_grep_shaped(root, source, &request, true)
                .map_err(|failure| failure.0.m)
                .unwrap();
            assert_eq!(value["totalLines"], json!(60));
            assert!(value["results"][0]["functions"][0]["source"].is_string());
        });
    }

    #[test]
    fn a_script_is_named_by_the_name_it_alone_goes_by() {
        let files = [
            ("Server/DataHandler.server.luau", "local a = 1\nreturn a\n"),
            ("Client/Shop/init.client.luau", "print('shop')\n"),
            ("A/Util.luau", "return 1\n"),
            ("B/Util.luau", "return 2\n"),
        ];
        with_scripts(&files, |root, source| {
            let read = |target: &str| {
                script_read_at(root, source, &json!({ "path": target }))
                    .map(|value| value["path"].as_str().unwrap().to_string())
                    .map_err(|failure| failure.0.m)
            };
            assert_eq!(
                read("DataHandler").unwrap(),
                "src/Server/DataHandler.server.luau"
            );
            assert_eq!(read("Shop").unwrap(), "src/Client/Shop/init.client.luau");
            assert_eq!(read("src/A/Util.luau").unwrap(), "src/A/Util.luau");
            let shared =
                "Util names 2 scripts: src/A/Util.luau, src/B/Util.luau; pass one by its path";
            assert_eq!(read("Util").unwrap_err(), shared);
            let unknown = read("Nope").unwrap_err();
            assert!(
                unknown.starts_with("No script at Nope: it is not a file or a script's name"),
                "{unknown}"
            );
            assert_eq!(script_by_name(root, source, "A/Util"), Ok(None));

            let grep = |scope: Value| {
                grep_text(root, source, json!({ "query": "return", "scope": scope }))
            };
            assert_eq!(
                grep(json!("DataHandler")).unwrap(),
                "src/Server/DataHandler.server.luau\n2: return a"
            );
            assert_eq!(
                grep(json!(["src/A", "DataHandler"])).unwrap(),
                "src/A/Util.luau\n1: return 1\nsrc/Server/DataHandler.server.luau\n2: return a"
            );
            assert_eq!(grep(json!("Util")).unwrap_err(), shared);
            assert_eq!(
                grep(json!("Nope")).unwrap_err(),
                "--in Nope: the project has no file or folder at that path and no script by that name"
            );
            assert_eq!(
                grep(json!(["src/Missing", "src/A"])).unwrap(),
                "src/A/Util.luau
1: return 1",
                "a folder that is not there matches nothing and the other scope still answers"
            );
        });
    }

    #[test]
    fn a_grep_skips_the_first_hits_and_leaves_out_excluded_text() {
        with_scripts(&[("Rounds.luau", ROUNDS)], |root, source| {
            let grep = |mut parameters: Value| {
                parameters["query"] = json!("total");
                grep_text(root, source, parameters)
            };
            assert_eq!(
                grep(json!({ "offset": 2, "limit": 2 })).unwrap(),
                "src/Rounds.luau\n2-5 add\n4: return total\n7-9 inner\n8: return total\n3-4 of 6 lines shown; -l N shows more"
            );
            assert_eq!(
                grep(json!({ "offset": 4 })).unwrap(),
                "src/Rounds.luau\n12-18 both\n13: local seen = total\n15: return total\n5-6 of 6 lines shown"
            );
            assert_eq!(
                grep(json!({ "offset": 9 })).unwrap(),
                "--offset 9 skips all 6 matching lines"
            );
            assert!(grep(json!({ "offset": 1, "count": true })).is_err());
            assert!(grep(json!({ "offset": 1, "fn": true })).is_err());
            assert_eq!(
                grep(json!({ "not": ["return"] })).unwrap(),
                "src/Rounds.luau\n1: local total = 0\n2-5 add\n3: total += value\n12-18 both\n13: local seen = total"
            );
            assert_eq!(
                grep(json!({ "not": ["return", "seen", "+="] })).unwrap(),
                "src/Rounds.luau\n1: local total = 0"
            );
            assert_eq!(
                grep(json!({ "not": "RETURN", "caseInsensitive": true, "count": true })).unwrap(),
                "src/Rounds.luau 3/20 lines"
            );
            assert_eq!(
                grep(json!({ "not": "RETURN", "count": true })).unwrap(),
                "src/Rounds.luau 6/20 lines"
            );
            assert_eq!(
                grep(json!({ "not": ["src/Rounds"] })).unwrap(),
                "no matches"
            );
            assert_eq!(
                grep(json!({ "not": ["return"], "fn": true, "scope": "Rounds" })).unwrap(),
                "src/Rounds.luau\n1: local total = 0\n2-5 add\nlocal function add(value)\n\ttotal += value\n\treturn total\nend\n12-18 both\nlocal function both()\n\tlocal seen = total\n\tlocal function nested()\n\t\treturn total\n\tend\n\treturn nested() + seen\nend"
            );
        });
    }

    #[test]
    fn a_literal_query_with_a_bar_that_matches_nothing_runs_as_a_pattern() {
        let events =
            "remote:FireAllClients(1)\nremote:FireClient(player)\ntype Side = \"Left\"|\"Right\"\n";
        with_scripts(&[("Events.luau", events)], |root, source| {
            let grep = |parameters: Value| grep_text(root, source, parameters).unwrap();
            assert_eq!(
                grep(json!({ "query": "FireAllClients|FireClient" })),
                "no literal match; as a pattern (-E):\nsrc/Events.luau\n1: remote:FireAllClients(1)\n2: remote:FireClient(player)"
            );
            assert_eq!(
                grep(json!({ "query": "FireAllClients|FireClient", "count": true })),
                "no literal match; as a pattern (-E):\nsrc/Events.luau 2/3 lines"
            );
            assert_eq!(
                grep(json!({ "query": "\"Left\"|\"Right\"" })),
                "src/Events.luau\n3: type Side = \"Left\"|\"Right\""
            );
            assert_eq!(
                grep(json!({ "query": "Nope|Nada" })),
                "no matches, also none as a pattern (-E)"
            );
            assert_eq!(grep(json!({ "query": "Fire(|x" })), "no matches");
            assert_eq!(
                grep(json!({ "query": "Nope|Nada", "regex": true })),
                "no matches"
            );
            let request = json!({ "query": "FireAllClients|FireClient" });
            let value = script_grep_printed(root, source, request, false)
                .map_err(|failure| failure.0.m)
                .unwrap();
            assert_eq!(value, json!({ "results": [], "totalMatches": 0 }));
        });
    }
}
