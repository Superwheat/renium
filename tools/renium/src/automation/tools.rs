use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::app::output::print_json_output;
use crate::automation::{BoundContext, Failure, op};
use crate::cli::{
    AssetInsertArgs, AssetSearchArgs, BridgeConnectionArgs, GenerateModelArgs, JobStatusArgs,
    ScriptGrepArgs, ScriptReadArgs, ScriptSearchArgs,
};
use crate::project::config;
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
    scope: Option<String>,
    limit: Option<usize>,
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

/// Search hits as text: each script's path on its own line, for a grep
/// followed by its hits as `LINE: text` without the indentation and with a
/// long line cut short, and a last line when the limit left hits out. A
/// script search names the scripts only; `rbx sg` shows the lines, and with
/// `--count` each script's path and its number of matching lines.
fn script_hits_text(value: &Value) -> String {
    let results = value["results"].as_array().map_or(&[][..], Vec::as_slice);
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
        for hit in result
            .get("hits")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            lines += 1;
            let line = hit["text"].as_str().unwrap_or_default().trim();
            text.push_str(&format!("{}: ", hit["line"]));
            match line.char_indices().nth(HIT_TEXT_LIMIT) {
                Some((cut, _)) => {
                    text.push_str(&line[..cut]);
                    text.push('\u{2026}');
                }
                None => text.push_str(line),
            }
            text.push('\n');
        }
    }
    if value["truncated"] == json!(true) {
        let (shown, total, unit) = match value.get("totalFiles") {
            Some(total) => (results.len() as u64, total.as_u64().unwrap_or(0), "scripts"),
            None => (lines, value["totalMatches"].as_u64().unwrap_or(0), "lines"),
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
            "scope": args.scope,
            "limit": args.limit,
        }),
    ))
}

pub(crate) fn script_read_command(
    args: ScriptReadArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, _) = script_roots(project)?;
    let value = script_read_at(
        &root,
        &json!({
            "path": args.path,
            "startLine": args.start_line,
            "endLine": args.end_line,
        }),
    )
    .map_err(|failure| anyhow::anyhow!(failure.0.m))?;
    let text = format!(
        "{}-{}/{}\n{}",
        value["startLine"],
        value["endLine"],
        value["totalLines"],
        value["source"].as_str().unwrap_or_default()
    );
    crate::app::output::emit_global_output(&value, &text)
}

fn failure(message: impl Into<String>, next: &str) -> Failure {
    Failure::new("bad_req", message.into(), false, next)
}

fn script_files(source: &Path) -> Vec<PathBuf> {
    let mut paths = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "lua" | "luau"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn read_script(path: &Path, next: &str) -> Result<String, Failure> {
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

fn relative(root: &Path, path: &Path) -> String {
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
    if !path.starts_with(root)
        || !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| matches!(value, "lua" | "luau"))
    {
        return Err(failure(
            "Script path must be a .lua or .luau file inside the bound project",
            next,
        ));
    }
    Ok(path)
}

fn script_read_at(root: &Path, parameters: &Value) -> Result<Value, Failure> {
    let request: ScriptRead = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            format!("Invalid script-read payload: {error}"),
            "script-read",
        )
    })?;
    let path = resolve_script(root, &request.path, "script-read")?;
    let source = read_script(&path, "script-read")?;
    let lines = source.lines().collect::<Vec<_>>();
    let start = request.start_line.unwrap_or(1);
    let requested_end = request.end_line.unwrap_or(lines.len());
    if start == 0 || requested_end < start || start > lines.len() {
        return Err(failure(
            format!(
                "Requested lines {start} through {requested_end}, but the script has {} lines",
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
    let scope = request.scope.as_deref().map(scope_prefix);
    let limit = request.limit.unwrap_or(50).clamp(1, 1000);
    // Matches are grouped by file: one path per script instead of one per line.
    let mut results: Vec<Value> = Vec::new();
    let mut returned = 0;
    let mut total_matches = 0;
    let mut matching_files = 0;
    for path in script_files(source) {
        if let Some(scope) = scope.as_deref()
            && !in_scope(&relative(root, &path), scope)
        {
            continue;
        }
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
        let mut hits = Vec::new();
        for (index, line) in source.lines().enumerate() {
            if matches(line) {
                total_matches += 1;
                if returned < limit {
                    returned += 1;
                    hits.push(json!({ "line": index + 1, "text": line }));
                }
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

    use super::{HIT_TEXT_LIMIT, script_grep_at, script_hits_text};

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
