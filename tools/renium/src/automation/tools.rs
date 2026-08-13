use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::automation::{BoundContext, Failure};
use crate::cloud::{agent, read_response};

const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptSearch {
    keywords: Value,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptRead {
    #[serde(alias = "target_file", alias = "file_path")]
    path: String,
    #[serde(default, alias = "start_line_one_indexed")]
    start_line: Option<usize>,
    #[serde(default, alias = "end_line_one_indexed_inclusive")]
    end_line: Option<usize>,
    #[serde(default, alias = "should_read_entire_file")]
    _entire: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptGrep {
    query: String,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HttpGet {
    url: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default = "default_context_lines", alias = "context_lines")]
    context_lines: usize,
    #[serde(default, alias = "return_full")]
    return_full: bool,
}

fn default_context_lines() -> usize {
    3
}

fn failure(message: impl Into<String>, next: &str) -> Failure {
    Failure::new("bad_req", message.into(), false, next)
}

fn script_files(context: &BoundContext) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(&context.source)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "lua" | "luau"))
        })
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

fn relative(context: &BoundContext, path: &Path) -> String {
    path.strip_prefix(&context.root)
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

pub(crate) fn script_search(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
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
    for path in script_files(context) {
        let source = read_script(&path, "script-search")?;
        let relative = relative(context, &path);
        let searchable = format!("{relative}\n{source}").to_ascii_lowercase();
        if needles.iter().all(|needle| searchable.contains(needle)) {
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
            if results.len() == limit {
                break;
            }
        }
    }
    Ok(json!({ "results": results }))
}

fn resolve_script(context: &BoundContext, requested: &str) -> Result<PathBuf, Failure> {
    let path = PathBuf::from(requested);
    let path = if path.is_absolute() {
        path
    } else {
        Path::new(&context.root).join(path)
    };
    let path = fs::canonicalize(&path).map_err(|error| {
        failure(
            format!("Failed to resolve {}: {error}", path.display()),
            "script-search",
        )
    })?;
    let root = fs::canonicalize(&context.root)
        .map_err(|error| failure(format!("Failed to resolve project root: {error}"), "bind"))?;
    if !path.starts_with(root)
        || !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| matches!(value, "lua" | "luau"))
    {
        return Err(failure(
            "Script path must be a .lua or .luau file inside the bound project",
            "script-search",
        ));
    }
    Ok(path)
}

pub(crate) fn script_read(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    let request: ScriptRead = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            format!("Invalid script-read payload: {error}"),
            "script-read",
        )
    })?;
    let path = resolve_script(context, &request.path)?;
    let source = read_script(&path, "script-read")?;
    let lines = source.lines().collect::<Vec<_>>();
    let start = request.start_line.unwrap_or(1);
    let end = request.end_line.unwrap_or(lines.len());
    if start == 0 || end < start || end > lines.len() {
        return Err(failure(
            format!(
                "Requested lines {start} through {end}, but the script has {} lines",
                lines.len()
            ),
            "script-read",
        ));
    }
    Ok(json!({
        "path": relative(context, &path),
        "startLine": start,
        "endLine": end,
        "totalLines": lines.len(),
        "source": lines[start - 1..end].join("\n"),
    }))
}

pub(crate) fn script_grep(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
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
    for path in script_files(context) {
        let source = read_script(&path, "script-grep")?;
        for (index, line) in source.lines().enumerate() {
            let matches = if request.case_insensitive {
                line.to_ascii_lowercase().contains(&needle)
            } else {
                line.contains(&needle)
            };
            if matches {
                results.push(json!({
                    "path": relative(context, &path),
                    "line": index + 1,
                    "text": line,
                }));
                if results.len() == limit {
                    return Ok(json!({ "results": results, "truncated": true }));
                }
            }
        }
    }
    Ok(json!({ "results": results, "truncated": false }))
}

pub(crate) fn http_get(parameters: &Value) -> Result<Value, Failure> {
    let request: HttpGet = serde_json::from_value(parameters.clone())
        .map_err(|error| failure(format!("Invalid http-get payload: {error}"), "http-get"))?;
    let url = url::Url::parse(&request.url)
        .map_err(|error| failure(format!("Invalid documentation URL: {error}"), "http-get"))?;
    let allowed = url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("create.roblox.com" | "github.com" | "raw.githubusercontent.com")
        )
        && (url.host_str() == Some("create.roblox.com")
            || url.path().contains("/Roblox/creator-docs/"));
    if !allowed {
        return Err(failure(
            "http-get only accepts HTTPS Roblox Creator documentation URLs",
            "http-get",
        ));
    }
    let response = match agent().get(url.as_str()).call() {
        Ok(response) | Err(ureq::Error::Status(_, response)) => read_response(response)
            .map_err(|message| Failure::new("cloud_http", message, false, "http-get"))?,
        Err(ureq::Error::Transport(error)) => {
            return Err(Failure::new(
                "cloud_http",
                format!("Documentation request failed: {error}"),
                true,
                "http-get",
            ));
        }
    };
    if !(200..300).contains(&response.status) {
        return Err(Failure::new(
            "cloud_http",
            format!("Documentation request returned HTTP {}", response.status),
            matches!(response.status, 429 | 500..=599),
            "http-get",
        )
        .detail(json!({ "status": response.status, "body": response.body })));
    }
    let Some(query) = request.query.filter(|query| !query.is_empty()) else {
        return Ok(
            json!({ "url": url.as_str(), "status": response.status, "body": response.body }),
        );
    };
    let text = response
        .body
        .as_str()
        .ok_or_else(|| failure("http-get query requires a text response", "http-get"))?;
    let needle = query.to_lowercase();
    let lines = text.lines().collect::<Vec<_>>();
    let matching = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.to_lowercase().contains(&needle).then_some(index))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Ok(json!({
            "url": url.as_str(),
            "status": response.status,
            "query": query,
            "matches": 0,
            "body": "",
        }));
    }
    if request.return_full {
        return Ok(json!({
            "url": url.as_str(),
            "status": response.status,
            "query": query,
            "matches": matching.len(),
            "body": text,
        }));
    }
    let context = request.context_lines.min(50);
    let mut ranges = Vec::<(usize, usize)>::new();
    for index in matching.iter().copied() {
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(lines.len());
        match ranges.last_mut() {
            Some((_, previous_end)) if start <= *previous_end => *previous_end = end,
            _ => ranges.push((start, end)),
        }
    }
    let body = ranges
        .into_iter()
        .map(|(start, end)| {
            lines[start..end]
                .iter()
                .enumerate()
                .map(|(offset, line)| format!("{}: {line}", start + offset + 1))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n---\n");
    Ok(json!({
        "url": url.as_str(),
        "status": response.status,
        "query": query,
        "matches": matching.len(),
        "body": body,
    }))
}
