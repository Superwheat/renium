use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::automation::{BoundContext, Failure, op};
use crate::cli::{
    AssetInsertArgs, AssetSearchArgs, GenerateModelArgs, ImageStoreArgs, JobStatusArgs,
    ScriptGrepArgs, ScriptReadArgs, ScriptSearchArgs,
};
use crate::daemon::try_daemon_control_request;
use crate::project::config;
use crate::system::files::canonical_path;

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
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn daemon_project_root(project: Option<&Path>) -> Option<&Path> {
    project.map(|path| {
        if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        }
    })
}

fn creator_command(
    operation: u16,
    project: Option<&Path>,
    parameters: Value,
) -> anyhow::Result<()> {
    let result =
        try_daemon_control_request(operation, daemon_project_root(project), parameters, false)?
            .ok_or_else(|| anyhow::anyhow!("Renium daemon is not running"))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(crate) fn asset_insert_command(
    args: AssetInsertArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    creator_command(
        op::ASSET_INSERT,
        project,
        json!({
            "assetId": args.asset_id.get(),
            "parentPath": args.parent,
            "name": args.name,
            "assetType": args.asset_type,
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
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
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
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
    )
}

pub(crate) fn image_store_command(
    args: ImageStoreArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, _) = script_roots(project)?;
    let result = crate::cloud::assets::store_image_at(&root, &json!({ "path": args.path }))
        .map_err(|failure| anyhow::anyhow!(failure.0.m))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn script_roots(project: Option<&Path>) -> anyhow::Result<(PathBuf, PathBuf)> {
    let loaded = config::load_project(project, None)?;
    let source = loaded.root.join(&loaded.project.source_root);
    Ok((loaded.root, source))
}

fn print_script_result(result: Result<Value, Failure>) -> anyhow::Result<()> {
    let value = result.map_err(|failure| anyhow::anyhow!(failure.0.m))?;
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

pub(crate) fn script_search_command(
    args: ScriptSearchArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, source) = script_roots(project)?;
    print_script_result(script_search_at(
        &root,
        &source,
        &json!({ "keywords": args.keywords, "limit": args.limit }),
    ))
}

pub(crate) fn script_grep_command(
    args: ScriptGrepArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, source) = script_roots(project)?;
    print_script_result(script_grep_at(
        &root,
        &source,
        &json!({
            "query": args.query,
            "caseInsensitive": args.case_insensitive,
            "limit": args.limit,
        }),
    ))
}

pub(crate) fn script_read_command(
    args: ScriptReadArgs,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let (root, _) = script_roots(project)?;
    print_script_result(script_read_at(
        &root,
        &json!({
            "path": args.path,
            "startLine": args.start_line,
            "endLine": args.end_line,
        }),
    ))
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
    let returned_matches = results.len();
    Ok(json!({
        "results": results,
        "returnedFiles": returned_matches,
        "totalFiles": total_matches,
        "truncated": returned_matches < total_matches,
    }))
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
    let needle = if request.case_insensitive {
        request.query.to_ascii_lowercase()
    } else {
        request.query
    };
    let limit = request.limit.unwrap_or(100).clamp(1, 1000);
    let mut results = Vec::new();
    let mut total_matches = 0;
    for path in script_files(source) {
        let source = read_script(&path, "script-grep")?;
        for (index, line) in source.lines().enumerate() {
            let matches = if request.case_insensitive {
                line.to_ascii_lowercase().contains(&needle)
            } else {
                line.contains(&needle)
            };
            if matches {
                total_matches += 1;
                if results.len() < limit {
                    results.push(json!({
                        "path": relative(root, &path),
                        "line": index + 1,
                        "text": line,
                    }));
                }
            }
        }
    }
    let returned_matches = results.len();
    Ok(json!({
        "results": results,
        "returnedMatches": returned_matches,
        "totalMatches": total_matches,
        "truncated": returned_matches < total_matches,
    }))
}

pub(crate) fn script_grep(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    script_grep_at(
        Path::new(&context.root),
        Path::new(&context.source),
        parameters,
    )
}
