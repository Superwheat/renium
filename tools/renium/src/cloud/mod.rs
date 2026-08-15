use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result as AnyResult, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::automation::{BoundContext, Failure};

pub(crate) mod assets;

pub(crate) const API_ROOT: &str = "https://apis.roblox.com";
const DEFAULT_KEY_ENV: &str = "ROBLOX_API_KEY";
const MAX_BODY_BYTES: u64 = 5 * 1024 * 1024;
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CloudBatch {
    #[serde(default = "default_key_env")]
    key_env: String,
    #[serde(default)]
    anonymous: bool,
    requests: Vec<CloudRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CloudRequest {
    id: Option<Value>,
    method: String,
    path: String,
    #[serde(default)]
    path_params: Map<String, Value>,
    #[serde(default)]
    query: Map<String, Value>,
    body: Option<Value>,
    if_match: Option<String>,
    if_none_match: Option<String>,
}

pub(crate) struct CloudResponse {
    pub(crate) status: u16,
    pub(crate) headers: Map<String, Value>,
    pub(crate) body: Value,
}

fn default_key_env() -> String {
    DEFAULT_KEY_ENV.to_string()
}

pub(crate) fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(30))
            .build()
    })
}

pub(crate) fn required_key(key_env: &str, next: &str) -> Result<String, Failure> {
    let value = env::var(key_env).map_err(|_| {
        Failure::new(
            "cloud_auth",
            format!("{key_env} is not set in the Renium daemon environment"),
            false,
            next,
        )
    })?;
    if value.trim().is_empty() {
        return Err(Failure::new(
            "cloud_auth",
            format!("{key_env} is empty"),
            false,
            next,
        ));
    }
    Ok(value)
}

pub fn execute(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    let batch: CloudBatch = serde_json::from_value(parameters.clone()).map_err(|error| {
        Failure::new(
            "bad_req",
            format!("Invalid cloud payload: {error}"),
            false,
            "cloud",
        )
    })?;
    if batch.requests.is_empty() {
        return Err(Failure::new(
            "bad_req",
            "cloud requires at least one request",
            false,
            "cloud",
        ));
    }
    let key = if batch.anonymous {
        None
    } else {
        Some(required_key(&batch.key_env, "cloud")?)
    };
    let responses = batch
        .requests
        .iter()
        .enumerate()
        .map(|(index, request)| execute_request(context, key.as_deref(), index, request))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "responses": responses }))
}

pub fn upload_file(url: &str, key: &str, content_type: &str, path: &Path) -> AnyResult<Value> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let size = file.metadata()?.len();
    let result = agent()
        .post(url)
        .set("x-api-key", key)
        .set("Content-Type", content_type)
        .set("Content-Length", &size.to_string())
        .send(file);
    let response = match result {
        Ok(response) | Err(ureq::Error::Status(_, response)) => {
            read_response(response).map_err(anyhow::Error::msg)?
        }
        Err(ureq::Error::Transport(error)) => bail!("Open Cloud upload failed: {error}"),
    };
    if !(200..300).contains(&response.status) {
        bail!(
            "Open Cloud upload returned HTTP {}: {}",
            response.status,
            response.body
        );
    }
    Ok(response.body)
}

fn execute_request(
    context: &BoundContext,
    key: Option<&str>,
    index: usize,
    request: &CloudRequest,
) -> Result<Value, Failure> {
    let method = request.method.trim().to_ascii_uppercase();
    if !matches!(
        method.as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
    ) {
        return Err(bad_request(format!(
            "cloud request {index} has unsupported method {method}"
        )));
    }
    let path = expand_path(context, &request.path, &request.path_params)
        .map_err(|message| bad_request(format!("cloud request {index}: {message}")))?;
    let mut url = url::Url::parse(&format!("{API_ROOT}{path}")).map_err(|error| {
        bad_request(format!(
            "cloud request {index} has an invalid path: {error}"
        ))
    })?;
    {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in &request.query {
            for value in query_values(name, value)
                .map_err(|message| bad_request(format!("cloud request {index}: {message}")))?
            {
                pairs.append_pair(name, &value);
            }
        }
    }
    let url = url.to_string();
    let send = || {
        let mut outgoing = agent().request(&method, &url);
        if let Some(key) = key {
            outgoing = outgoing.set("x-api-key", key);
        }
        if let Some(value) = request.if_match.as_deref() {
            outgoing = outgoing.set("If-Match", value);
        }
        if let Some(value) = request.if_none_match.as_deref() {
            outgoing = outgoing.set("If-None-Match", value);
        }
        match &request.body {
            Some(body) => outgoing.send_json(body.clone()),
            None => outgoing.call(),
        }
        .map_err(Box::new)
    };
    let mut result = send();
    if matches!(method.as_str(), "GET" | "HEAD") && is_transient(&result) {
        result = send();
    }
    let response = match result {
        Ok(response) => read_response(response).map_err(|message| {
            Failure::new("cloud_http", message, false, "cloud").detail(json!({ "index": index }))
        })?,
        Err(error) => match *error {
            ureq::Error::Status(_, response) => read_response(response).map_err(|message| {
                Failure::new("cloud_http", message, false, "cloud")
                    .detail(json!({ "index": index }))
            })?,
            ureq::Error::Transport(error) => {
                return Err(Failure::new(
                    "cloud_http",
                    format!("Open Cloud request {index} failed: {error}"),
                    true,
                    "cloud",
                ));
            }
        },
    };
    if !(200..300).contains(&response.status) {
        let (code, retry) = match response.status {
            401 | 403 => ("cloud_auth", false),
            409 | 412 => ("conflict", false),
            429 | 500..=599 => ("cloud_http", true),
            _ => ("cloud_http", false),
        };
        return Err(Failure::new(
            code,
            format!(
                "Open Cloud request {index} returned HTTP {}",
                response.status
            ),
            retry,
            "cloud",
        )
        .detail(json!({
            "index": index,
            "status": response.status,
            "headers": response.headers,
            "body": response.body,
        })));
    }
    let mut value = json!({
        "status": response.status,
        "body": response.body,
    });
    if !response.headers.is_empty() {
        value["headers"] = Value::Object(response.headers);
    }
    if let Some(id) = &request.id {
        value["id"] = id.clone();
    }
    Ok(value)
}

fn is_transient(result: &Result<ureq::Response, Box<ureq::Error>>) -> bool {
    result.as_ref().is_err_and(|error| {
        matches!(
            error.as_ref(),
            ureq::Error::Transport(_) | ureq::Error::Status(429 | 502 | 503 | 504, _)
        )
    })
}

pub(crate) fn read_response(response: ureq::Response) -> Result<CloudResponse, String> {
    let status = response.status();
    let mut headers = Map::new();
    for name in [
        "etag",
        "last-modified",
        "retry-after",
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
    ] {
        if let Some(value) = response.header(name) {
            headers.insert(name.to_string(), Value::String(value.to_string()));
        }
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Failed to read Open Cloud response: {error}"))?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(format!(
            "Open Cloud response exceeds the {} MiB automation limit",
            MAX_BODY_BYTES / 1024 / 1024
        ));
    }
    let body = if bytes.is_empty() {
        Value::Null
    } else if let Ok(value) = serde_json::from_slice(&bytes) {
        value
    } else if let Ok(text) = String::from_utf8(bytes) {
        Value::String(text)
    } else {
        return Err("Open Cloud returned a non-text response".to_string());
    };
    Ok(CloudResponse {
        status,
        headers,
        body,
    })
}

fn expand_path(
    context: &BoundContext,
    template: &str,
    parameters: &Map<String, Value>,
) -> Result<String, String> {
    let mut path = template.trim().to_string();
    if !path.starts_with('/') || path.starts_with("//") || path.contains('?') || path.contains('#')
    {
        return Err(
            "path must be an absolute API path without a host, query, or fragment".to_string(),
        );
    }
    if let Some(id) = context.game_id {
        for name in [
            "universe",
            "universe_id",
            "universeId",
            "game",
            "game_id",
            "gameId",
        ] {
            path = path.replace(&format!("{{{name}}}"), &id.to_string());
        }
    }
    if let Some(id) = context.place_id {
        for name in ["place", "place_id", "placeId"] {
            path = path.replace(&format!("{{{name}}}"), &id.to_string());
        }
    }
    for (name, value) in parameters {
        let value = scalar(value)
            .ok_or_else(|| format!("pathParams.{name} must be a string, number, or boolean"))?;
        path = path.replace(&format!("{{{name}}}"), &encode_segment(&value));
    }
    if path.contains('{') || path.contains('}') {
        return Err(format!("path has an unresolved placeholder: {path}"));
    }
    Ok(path)
}

fn query_values(name: &str, value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                scalar(value).ok_or_else(|| {
                    format!("query.{name} must contain only strings, numbers, or booleans")
                })
            })
            .collect(),
        value => scalar(value)
            .map(|value| vec![value])
            .ok_or_else(|| format!("query.{name} must be a string, number, boolean, or array")),
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
            encoded.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn bad_request(message: String) -> Failure {
    Failure::new("bad_req", message, false, "cloud")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> BoundContext {
        BoundContext {
            id: 1,
            initialized: true,
            project: String::new(),
            root: String::new(),
            experience: String::new(),
            source: String::new(),
            place_id: Some(456),
            game_id: Some(123),
            selector: String::new(),
            runtime_id: None,
            plugin_build: None,
            fingerprint: String::new(),
        }
    }

    #[test]
    fn expands_context_and_escaped_path_parameters() {
        let parameters = Map::from_iter([
            ("store".to_string(), json!("Player Data")),
            ("entry".to_string(), json!("user/1")),
        ]);
        assert_eq!(
            expand_path(
                &context(),
                "/cloud/v2/universes/{universe}/data-stores/{store}/entries/{entry}",
                &parameters,
            )
            .unwrap(),
            "/cloud/v2/universes/123/data-stores/Player%20Data/entries/user%2F1"
        );
    }
}
