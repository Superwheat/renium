use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result as AnyResult, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::automation::Failure;
use crate::system::files::atomic_write_file;

pub(crate) const API_ROOT: &str = "https://apis.roblox.com";
const DEFAULT_KEY_ENV: &str = "ROBLOX_API_KEY";
const MAX_BODY_BYTES: u64 = 5 * 1024 * 1024;
const MAX_MULTIPART_FILE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_RAW_FILE_BYTES: u64 = 200 * 1024 * 1024;
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

#[derive(Clone, Copy, Default)]
pub(crate) struct CloudIdentity {
    pub(crate) game_id: Option<i64>,
    pub(crate) place_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CloudBatch {
    #[serde(default = "default_key_env")]
    key_env: String,
    oauth_env: Option<String>,
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
    #[serde(default)]
    form: Map<String, Value>,
    #[serde(default)]
    json_parts: Map<String, Value>,
    #[serde(default)]
    files: Map<String, Value>,
    #[serde(default)]
    url_encoded: Map<String, Value>,
    raw_file: Option<String>,
    content_type: Option<String>,
    output_file: Option<String>,
    #[serde(default)]
    headers: Map<String, Value>,
    if_match: Option<String>,
    if_none_match: Option<String>,
}

pub(crate) struct CloudResponse {
    pub(crate) status: u16,
    pub(crate) headers: Map<String, Value>,
    pub(crate) body: Value,
}

pub(crate) enum CloudAuth {
    Anonymous,
    ApiKey(String),
    OAuth(String),
}

impl CloudAuth {
    pub(crate) fn from_env(
        anonymous: bool,
        key_env: &str,
        oauth_env: Option<&str>,
        next: &str,
    ) -> Result<Self, Failure> {
        if anonymous && oauth_env.is_some() {
            return Err(Failure::new(
                "bad_req",
                "Use either anonymous access or OAuth, not both",
                false,
                next,
            ));
        }
        if anonymous {
            Ok(Self::Anonymous)
        } else if let Some(oauth_env) = oauth_env {
            Ok(Self::OAuth(required_secret(oauth_env, next)?))
        } else {
            Ok(Self::ApiKey(required_secret(key_env, next)?))
        }
    }

    pub(crate) fn apply(&self, request: ureq::Request) -> ureq::Request {
        match self {
            Self::Anonymous => request,
            Self::ApiKey(key) => request.set("x-api-key", key),
            Self::OAuth(token) => request.set("Authorization", &format!("Bearer {token}")),
        }
    }
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

fn required_secret(env_name: &str, next: &str) -> Result<String, Failure> {
    let value = environment_value(env_name).ok_or_else(|| {
        Failure::new(
            "cloud_auth",
            format!("{env_name} is not set in this process environment"),
            false,
            next,
        )
    })?;
    if value.trim().is_empty() {
        return Err(Failure::new(
            "cloud_auth",
            format!("{env_name} is empty"),
            false,
            next,
        ));
    }
    Ok(value)
}

fn environment_value(name: &str) -> Option<String> {
    env::var(name).ok().or_else(|| user_environment_value(name))
}

#[cfg(not(windows))]
fn user_environment_value(_name: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn user_environment_value(name: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };

    if name.contains('\0') {
        return None;
    }
    let subkey = std::ffi::OsStr::new("Environment")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let name = std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut bytes = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || bytes < 2 {
        return None;
    }
    let mut value = vec![0_u16; bytes as usize / 2];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16(&value[..end]).ok()
}

pub(crate) fn execute_with_identity(
    identity: CloudIdentity,
    parameters: &Value,
) -> Result<Value, Failure> {
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
    let auth = CloudAuth::from_env(
        batch.anonymous,
        &batch.key_env,
        batch.oauth_env.as_deref(),
        "cloud",
    )?;
    let responses = batch
        .requests
        .iter()
        .enumerate()
        .map(|(index, request)| execute_request(identity, &auth, index, request))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "responses": responses }))
}

pub(crate) fn execute_one(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    anonymous: bool,
    request: Value,
) -> Result<Value, Failure> {
    let result = execute_with_identity(
        identity,
        &json!({
            "keyEnv": key_env,
            "oauthEnv": oauth_env,
            "anonymous": anonymous,
            "requests": [request],
        }),
    )?;
    result
        .get("responses")
        .and_then(Value::as_array)
        .and_then(|responses| responses.first())
        .cloned()
        .ok_or_else(|| {
            Failure::new(
                "cloud_http",
                "Open Cloud returned no response",
                false,
                "cloud",
            )
        })
}

pub(crate) fn upload_file(
    url: &str,
    auth: &CloudAuth,
    content_type: &str,
    path: &Path,
) -> AnyResult<Value> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let size = file.metadata()?.len();
    let result = auth
        .apply(agent().post(url))
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
    identity: CloudIdentity,
    auth: &CloudAuth,
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
    let path = expand_path(identity, &request.path, &request.path_params)
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
    let has_multipart =
        !request.form.is_empty() || !request.json_parts.is_empty() || !request.files.is_empty();
    let body_count = usize::from(request.body.is_some())
        + usize::from(has_multipart)
        + usize::from(!request.url_encoded.is_empty())
        + usize::from(request.raw_file.is_some());
    if body_count > 1 {
        return Err(bad_request(format!(
            "cloud request {index} has more than one body type"
        )));
    }
    if request.content_type.is_some() && request.raw_file.is_none() {
        return Err(bad_request(format!(
            "cloud request {index} uses contentType without rawFile"
        )));
    }
    let multipart =
        if request.form.is_empty() && request.json_parts.is_empty() && request.files.is_empty() {
            None
        } else {
            Some(
                multipart_body(&request.form, &request.json_parts, &request.files)
                    .map_err(|message| bad_request(format!("cloud request {index}: {message}")))?,
            )
        };
    let url_encoded = if request.url_encoded.is_empty() {
        None
    } else {
        Some(
            url_encoded_body(&request.url_encoded)
                .map_err(|message| bad_request(format!("cloud request {index}: {message}")))?,
        )
    };
    let raw = request
        .raw_file
        .as_deref()
        .map(|path| raw_body(path, request.content_type.as_deref()))
        .transpose()
        .map_err(|message| bad_request(format!("cloud request {index}: {message}")))?;
    let headers = request
        .headers
        .iter()
        .map(|(name, value)| {
            let normalized = name.to_ascii_lowercase();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                || matches!(
                    normalized.as_str(),
                    "authorization" | "content-length" | "content-type" | "host" | "x-api-key"
                )
            {
                return Err(bad_request(format!(
                    "cloud request {index} has an invalid or reserved header {name}"
                )));
            }
            let value = scalar(value).ok_or_else(|| {
                bad_request(format!(
                    "cloud request {index} header {name} must be a string, number, or boolean"
                ))
            })?;
            if value
                .chars()
                .any(|character| matches!(character, '\r' | '\n'))
            {
                return Err(bad_request(format!(
                    "cloud request {index} header {name} contains a line break"
                )));
            }
            Ok((name.as_str(), value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let send = || {
        let mut outgoing = auth.apply(agent().request(&method, &url));
        if let Some(value) = request.if_match.as_deref() {
            outgoing = outgoing.set("If-Match", value);
        }
        if let Some(value) = request.if_none_match.as_deref() {
            outgoing = outgoing.set("If-None-Match", value);
        }
        for (name, value) in &headers {
            outgoing = outgoing.set(name, value);
        }
        match (&request.body, &multipart, &url_encoded, &raw) {
            (Some(body), _, _, _) => outgoing.send_json(body.clone()),
            (None, Some((boundary, body)), _, _) => outgoing
                .set(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={boundary}"),
                )
                .send_bytes(body),
            (None, None, Some(body), _) => outgoing
                .set("Content-Type", "application/x-www-form-urlencoded")
                .send_bytes(body),
            (None, None, None, Some((content_type, body))) => {
                outgoing.set("Content-Type", content_type).send_bytes(body)
            }
            (None, None, None, None) => outgoing.call(),
        }
        .map_err(Box::new)
    };
    let mut result = send();
    if matches!(method.as_str(), "GET" | "HEAD") && is_transient(&result) {
        result = send();
    }
    let response = match result {
        Ok(response) => response,
        Err(error) => match *error {
            ureq::Error::Status(_, response) => response,
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
    let success = (200..300).contains(&response.status());
    let response = if success {
        if let Some(path) = request.output_file.as_deref() {
            write_response(response, Path::new(path))
        } else {
            read_response(response)
        }
    } else {
        read_response(response)
    }
    .map_err(|message| {
        Failure::new("cloud_http", message, false, "cloud").detail(json!({ "index": index }))
    })?;
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

fn url_encoded_body(fields: &Map<String, Value>) -> Result<Vec<u8>, String> {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        for value in query_values(name, value)? {
            body.append_pair(name, &value);
        }
    }
    Ok(body.finish().into_bytes())
}

fn raw_body(path: &str, content_type: Option<&str>) -> Result<(String, Vec<u8>), String> {
    let path = Path::new(path);
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if metadata.len() > MAX_RAW_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {} MiB raw body limit",
            path.display(),
            MAX_RAW_FILE_BYTES / 1024 / 1024
        ));
    }
    let content_type = content_type.unwrap_or_else(|| file_content_type(path));
    if content_type.is_empty()
        || content_type
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err("contentType is invalid".to_string());
    }
    let body =
        fs::read(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    Ok((content_type.to_string(), body))
}

fn multipart_body(
    fields: &Map<String, Value>,
    json_parts: &Map<String, Value>,
    files: &Map<String, Value>,
) -> Result<(String, Vec<u8>), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let boundary = format!("renium-{stamp:x}");
    let mut body = Vec::new();
    for (name, value) in fields {
        validate_part_name(name)?;
        let value = scalar(value)
            .ok_or_else(|| format!("form.{name} must be a string, number, or boolean"))?;
        push_part(&mut body, &boundary, name, None, None, value.as_bytes());
    }
    for (name, value) in json_parts {
        validate_part_name(name)?;
        let value = serde_json::to_vec(value)
            .map_err(|error| format!("jsonParts.{name} is invalid: {error}"))?;
        push_part(
            &mut body,
            &boundary,
            name,
            None,
            Some("application/json"),
            &value,
        );
    }
    for (name, value) in files {
        validate_part_name(name)?;
        let path = value
            .as_str()
            .ok_or_else(|| format!("files.{name} must be a file path string"))?;
        let path = Path::new(path);
        let metadata = fs::metadata(path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        if metadata.len() > MAX_MULTIPART_FILE_BYTES {
            return Err(format!(
                "{} exceeds the {} MiB multipart limit",
                path.display(),
                MAX_MULTIPART_FILE_BYTES / 1024 / 1024
            ));
        }
        let bytes = fs::read(path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file");
        if filename
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '"'))
        {
            return Err(format!("{} has an unsupported file name", path.display()));
        }
        push_part(
            &mut body,
            &boundary,
            name,
            Some(filename),
            Some(file_content_type(path)),
            &bytes,
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok((boundary, body))
}

fn validate_part_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '"'))
    {
        return Err(format!("Invalid multipart field name '{name}'"));
    }
    Ok(())
}

fn push_part(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    filename: Option<&str>,
    content_type: Option<&str>,
    value: &[u8],
) {
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"").as_bytes(),
    );
    if let Some(filename) = filename {
        body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    if let Some(content_type) = content_type {
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    }
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

fn file_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("bmp") => "image/bmp",
        Some("json") => "application/json",
        Some("rbxl") => "application/octet-stream",
        Some("rbxlx" | "xml") => "application/xml",
        _ => "application/octet-stream",
    }
}

pub(crate) fn read_response(response: ureq::Response) -> Result<CloudResponse, String> {
    let status = response.status();
    let headers = response_headers(&response);
    let bytes = read_response_bytes(response, MAX_BODY_BYTES)?;
    let body = if bytes.is_empty() {
        Value::Null
    } else if let Ok(value) = serde_json::from_slice(&bytes) {
        value
    } else if let Ok(text) = String::from_utf8(bytes) {
        Value::String(text)
    } else {
        return Err("Open Cloud returned a non-text response; use --output FILE".to_string());
    };
    Ok(CloudResponse {
        status,
        headers,
        body,
    })
}

fn write_response(response: ureq::Response, path: &Path) -> Result<CloudResponse, String> {
    let status = response.status();
    let headers = response_headers(&response);
    let bytes = read_response_bytes(response, MAX_RAW_FILE_BYTES)?;
    atomic_write_file(path, &bytes)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
    Ok(CloudResponse {
        status,
        headers,
        body: json!({
            "file": path,
            "bytes": bytes.len(),
        }),
    })
}

fn response_headers(response: &ureq::Response) -> Map<String, Value> {
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
    headers
}

fn read_response_bytes(response: ureq::Response, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Failed to read Open Cloud response: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "Open Cloud response exceeds the {} MiB limit",
            limit / 1024 / 1024
        ));
    }
    Ok(bytes)
}

fn expand_path(
    identity: CloudIdentity,
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
    if let Some(id) = identity.game_id {
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
    if let Some(id) = identity.place_id {
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

    fn identity() -> CloudIdentity {
        CloudIdentity {
            place_id: Some(456),
            game_id: Some(123),
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
                identity(),
                "/cloud/v2/universes/{universe}/data-stores/{store}/entries/{entry}",
                &parameters,
            )
            .unwrap(),
            "/cloud/v2/universes/123/data-stores/Player%20Data/entries/user%2F1"
        );
    }
}
