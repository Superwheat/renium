use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::app::output::ensure_plugin_api_ok;
use crate::automation::{BoundContext, Failure};
use crate::cloud::{API_ROOT, CloudAuth, agent, read_response};
use crate::studio::bridge::{BridgeServer, BridgeTarget};

const MAX_STORED_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssetSearch {
    #[serde(default = "default_store_scope")]
    scope: String,
    #[serde(default)]
    query: String,
    asset_type: Option<String>,
    filter: Option<String>,
    user_id: Option<u64>,
    max_results: Option<u32>,
    cursor: Option<String>,
    #[serde(default)]
    facets: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    verified_creators_only: Option<bool>,
    min_price_cents: Option<u64>,
    max_price_cents: Option<u64>,
    price_filter: Option<String>,
    audio_min_duration: Option<f64>,
    audio_max_duration: Option<f64>,
    #[serde(default = "default_key_env")]
    key_env: String,
    oauth_env: Option<String>,
    #[serde(default)]
    options: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImageUpload {
    #[serde(alias = "imagePaths")]
    images: Vec<String>,
    user_id: Option<u64>,
    group_id: Option<u64>,
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default = "default_key_env")]
    key_env: String,
    oauth_env: Option<String>,
    #[serde(default = "default_upload_wait")]
    wait_seconds: f64,
    #[serde(rename = "via")]
    _via: Option<String>,
}

fn default_store_scope() -> String {
    "creator-store".to_string()
}

fn default_key_env() -> String {
    "ROBLOX_API_KEY".to_string()
}

fn default_upload_wait() -> f64 {
    30.0
}

fn failure(code: &str, message: impl Into<String>, retry: bool, next: &str) -> Failure {
    Failure::new(code, message.into(), retry, next)
}

fn checked_response(
    result: Result<ureq::Response, ureq::Error>,
    next: &str,
) -> Result<Value, Failure> {
    let response = match result {
        Ok(response) | Err(ureq::Error::Status(_, response)) => read_response(response)
            .map_err(|message| failure("cloud_http", message, false, next))?,
        Err(ureq::Error::Transport(error)) => {
            return Err(failure(
                "cloud_http",
                format!("Roblox request failed: {error}"),
                next == "asset-search",
                next,
            ));
        }
    };
    if !(200..300).contains(&response.status) {
        let code = if matches!(response.status, 401 | 403) {
            "cloud_auth"
        } else {
            "cloud_http"
        };
        return Err(failure(
            code,
            format!(
                "Roblox returned HTTP {}: {}",
                response.status, response.body
            ),
            next == "asset-search" && matches!(response.status, 429 | 500..=599),
            next,
        ));
    }
    Ok(response.body)
}

fn append_query(url: &mut url::Url, name: &str, value: &Value) -> Result<(), Failure> {
    let mut query = url.query_pairs_mut();
    match value {
        Value::String(value) => {
            query.append_pair(name, value);
        }
        Value::Number(value) => {
            query.append_pair(name, &value.to_string());
        }
        Value::Bool(value) => {
            query.append_pair(name, &value.to_string());
        }
        Value::Array(values) => {
            for value in values {
                let value = value.as_str().ok_or_else(|| {
                    failure(
                        "bad_req",
                        format!("asset-search p.options.{name} arrays must contain strings"),
                        false,
                        "asset-search",
                    )
                })?;
                query.append_pair(name, value);
            }
        }
        _ => {
            return Err(failure(
                "bad_req",
                format!("asset-search p.options.{name} must be scalar or a string array"),
                false,
                "asset-search",
            ));
        }
    };
    Ok(())
}

fn studio_user_id(bridge: Option<&BridgeServer>, next: &str) -> Result<u64, Failure> {
    let bridge = bridge.ok_or_else(|| {
        failure(
            "no_studio",
            "User inventory search needs a connected Studio or an explicit userId",
            false,
            "studios",
        )
    })?;
    let result = bridge
        .call_for_target("getCreatorContext", json!({}), BridgeTarget::Edit)
        .map_err(|error| failure("no_studio", format!("{error:#}"), false, "studios"))?;
    ensure_plugin_api_ok(&result)
        .map_err(|error| failure("unsupported", format!("{error:#}"), false, next))?;
    result
        .get("userId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            failure(
                "unsupported",
                "Studio did not provide the signed-in creator user ID",
                false,
                next,
            )
        })
}

pub(crate) fn search(parameters: &Value, bridge: Option<&BridgeServer>) -> Result<Value, Failure> {
    let request: AssetSearch = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            "bad_req",
            format!("Invalid asset-search payload: {error}"),
            false,
            "asset-search",
        )
    })?;
    let max_results = request.max_results.unwrap_or(25).clamp(1, 100);
    match request.scope.trim().to_ascii_lowercase().as_str() {
        "auto" | "creator-store" | "creator_store" | "store" => {
            let mut url = url::Url::parse(&format!("{API_ROOT}/toolbox-service/v2/assets:search"))
                .expect("static Creator Store URL is valid");
            url.query_pairs_mut()
                .append_pair(
                    "searchCategoryType",
                    request.asset_type.as_deref().unwrap_or("Model"),
                )
                .append_pair("query", &request.query)
                .append_pair("maxPageSize", &max_results.to_string());
            if let Some(cursor) = request.cursor.as_deref() {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            for facet in &request.facets {
                url.query_pairs_mut().append_pair("facets", facet);
            }
            for tag in &request.tags {
                url.query_pairs_mut().append_pair("tags", tag);
            }
            for (name, value) in [
                (
                    "verifiedCreatorsOnly",
                    request.verified_creators_only.map(Value::Bool),
                ),
                (
                    "minPriceCents",
                    request.min_price_cents.map(|value| json!(value)),
                ),
                (
                    "maxPriceCents",
                    request.max_price_cents.map(|value| json!(value)),
                ),
                ("priceFilter", request.price_filter.map(Value::String)),
                (
                    "audioMinDuration",
                    request.audio_min_duration.map(|value| json!(value)),
                ),
                (
                    "audioMaxDuration",
                    request.audio_max_duration.map(|value| json!(value)),
                ),
            ] {
                if let Some(value) = value {
                    append_query(&mut url, name, &value)?;
                }
            }
            for (name, value) in &request.options {
                append_query(&mut url, name, value)?;
            }
            let body = checked_response(agent().get(url.as_str()).call(), "asset-search")?;
            Ok(json!({
                "scope": "creator-store",
                "results": body.get("creatorStoreAssets").cloned().unwrap_or_else(|| body.clone()),
                "nextPageCursor": body.get("nextPageCursor").or_else(|| body.get("nextPageToken")).cloned(),
            }))
        }
        "user" | "inventory" => {
            let user_id = match request.user_id {
                Some(id) if id > 0 => id,
                _ => studio_user_id(bridge, "asset-search")?,
            };
            let auth = CloudAuth::from_env(
                false,
                &request.key_env,
                request.oauth_env.as_deref(),
                "asset-search",
            )?;
            let mut url = url::Url::parse(&format!(
                "{API_ROOT}/cloud/v2/users/{user_id}/inventory-items"
            ))
            .expect("numeric user inventory URL is valid");
            url.query_pairs_mut()
                .append_pair("maxPageSize", &max_results.to_string());
            if let Some(cursor) = request.cursor.as_deref() {
                url.query_pairs_mut().append_pair("pageToken", cursor);
            }
            let filter = request.filter.or_else(|| {
                request
                    .asset_type
                    .map(|kind| format!("inventoryItemAssetTypes={kind}"))
            });
            if let Some(filter) = filter {
                url.query_pairs_mut().append_pair("filter", &filter);
            }
            let body =
                checked_response(auth.apply(agent().get(url.as_str())).call(), "asset-search")?;
            let mut results = body
                .get("inventoryItems")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !request.query.is_empty() {
                let needle = request.query.to_ascii_lowercase();
                results.retain(|item| item.to_string().to_ascii_lowercase().contains(&needle));
            }
            Ok(json!({
                "scope": "user",
                "userId": user_id,
                "results": results,
                "nextPageCursor": body.get("nextPageToken").cloned(),
            }))
        }
        "group" | "universe" => Err(failure(
            "unsupported",
            "Roblox does not expose group or universe Creator Inventory through a plugin-accessible or public Open Cloud endpoint",
            false,
            "asset-search",
        )),
        scope => Err(failure(
            "bad_req",
            format!("Unknown asset-search scope {scope}"),
            false,
            "asset-search",
        )),
    }
}

fn resolve_image_path(root: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn image_type(bytes: &[u8], path: &str) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"BM") {
        Some(("image/bmp", "bmp"))
    } else if path.to_ascii_lowercase().ends_with(".tga") {
        Some(("image/tga", "tga"))
    } else {
        None
    }
}

fn read_bounded(
    mut reader: impl Read,
    limit: u64,
    label: &str,
    next: &str,
) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            failure(
                "bad_req",
                format!("Failed to read {label}: {error}"),
                false,
                next,
            )
        })?;
    if bytes.len() as u64 > limit {
        return Err(failure(
            "bad_req",
            format!("{label} exceeds {} MiB", limit / 1024 / 1024),
            false,
            next,
        ));
    }
    Ok(bytes)
}

fn load_image(root: &Path, source: &str) -> Result<(Vec<u8>, String), Failure> {
    if source.starts_with("https://") || source.starts_with("http://") {
        let response = match agent().get(source).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _)) => {
                return Err(failure(
                    "cloud_http",
                    format!("Image download returned HTTP {status}: {source}"),
                    false,
                    "image-upload",
                ));
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(failure(
                    "cloud_http",
                    format!("Failed to download {source}: {error}"),
                    true,
                    "image-upload",
                ));
            }
        };
        let bytes = read_bounded(
            response.into_reader(),
            MAX_UPLOAD_BYTES,
            source,
            "image-upload",
        )?;
        return Ok((bytes, source.to_string()));
    }
    let path = resolve_image_path(root, source);
    let file = fs::File::open(&path).map_err(|error| {
        failure(
            "bad_req",
            format!("Failed to open {}: {error}", path.display()),
            false,
            "image-upload",
        )
    })?;
    let bytes = read_bounded(
        file,
        MAX_UPLOAD_BYTES,
        &path.display().to_string(),
        "image-upload",
    )?;
    Ok((bytes, path.display().to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImageStore {
    #[serde(alias = "filePath")]
    path: String,
}

pub(crate) fn store_image_at(root: &Path, parameters: &Value) -> Result<Value, Failure> {
    let request: ImageStore = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            "bad_req",
            format!("Invalid image-store payload: {error}"),
            false,
            "image-store",
        )
    })?;
    let path = resolve_image_path(root, &request.path);
    let file = fs::File::open(&path).map_err(|error| {
        failure(
            "bad_req",
            format!("Failed to open {}: {error}", path.display()),
            false,
            "image-store",
        )
    })?;
    let bytes = read_bounded(
        file,
        MAX_STORED_IMAGE_BYTES,
        &path.display().to_string(),
        "image-store",
    )?;
    let (mime, _) = image_type(&bytes, &path.display().to_string()).ok_or_else(|| {
        failure(
            "bad_req",
            "image-store supports PNG, JPEG, BMP, and TGA images",
            false,
            "image-store",
        )
    })?;
    Ok(json!({
        "path": path,
        "mimeType": mime,
        "bytes": bytes.len(),
    }))
}

pub(crate) fn store_image(context: &BoundContext, parameters: &Value) -> Result<Value, Failure> {
    store_image_at(Path::new(&context.root), parameters)
}

fn multipart_body(metadata: &Value, name: &str, mime: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let boundary = format!("renium-{stamp:x}");
    let mut body = Vec::with_capacity(bytes.len() + 1024);
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n{metadata}\r\n").as_bytes());
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"fileContent\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

fn operation_path(body: &Value) -> Option<&str> {
    body.get("path")
        .and_then(Value::as_str)
        .filter(|path| path.starts_with("operations/"))
}

fn wait_for_upload(path: &str, auth: &CloudAuth, wait_seconds: f64) -> Result<Value, Failure> {
    let operation_id = path.trim_start_matches("operations/");
    let url = format!("{API_ROOT}/assets/v1/operations/{operation_id}");
    let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds.clamp(0.0, 120.0));
    loop {
        let operation = checked_response(auth.apply(agent().get(&url)).call(), "image-upload")?;
        if operation.get("done").and_then(Value::as_bool) == Some(true) {
            if let Some(error) = operation.get("error") {
                return Err(failure(
                    "rejected",
                    format!("Roblox rejected the image: {error}"),
                    false,
                    "image-upload",
                ));
            }
            return Ok(operation);
        }
        if Instant::now() >= deadline {
            return Ok(operation);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub(crate) fn upload(
    root: &Path,
    parameters: &Value,
    bridge: Option<&BridgeServer>,
) -> Result<Value, Failure> {
    let request: ImageUpload = serde_json::from_value(parameters.clone()).map_err(|error| {
        failure(
            "bad_req",
            format!("Invalid image-upload payload: {error}"),
            false,
            "image-upload",
        )
    })?;
    if request.images.is_empty() || request.images.len() > 20 {
        return Err(failure(
            "bad_req",
            "image-upload requires 1 through 20 images",
            false,
            "image-upload",
        ));
    }
    if request.user_id.is_some() && request.group_id.is_some() {
        return Err(failure(
            "bad_req",
            "image-upload accepts either userId or groupId, not both",
            false,
            "image-upload",
        ));
    }
    if !request.wait_seconds.is_finite() || request.wait_seconds < 0.0 {
        return Err(failure(
            "bad_req",
            "image-upload waitSeconds must be a non-negative finite number",
            false,
            "image-upload",
        ));
    }
    let creator = if let Some(group_id) = request.group_id.filter(|id| *id > 0) {
        json!({ "groupId": group_id.to_string() })
    } else {
        let user_id = match request.user_id.filter(|id| *id > 0) {
            Some(id) => id,
            None => studio_user_id(bridge, "image-upload")?,
        };
        json!({ "userId": user_id.to_string() })
    };
    let auth = CloudAuth::from_env(
        false,
        &request.key_env,
        request.oauth_env.as_deref(),
        "image-upload",
    )?;
    let mut results = Vec::with_capacity(request.images.len());
    for (index, source) in request.images.iter().enumerate() {
        let (bytes, label) = load_image(root, source)?;
        let (mime, extension) = image_type(&bytes, &label).ok_or_else(|| {
            failure(
                "bad_req",
                format!("{label} is not a supported PNG, JPEG, BMP, or TGA image"),
                false,
                "image-upload",
            )
        })?;
        let display_name = request
            .name
            .as_deref()
            .map(|name| {
                if request.images.len() == 1 {
                    name.to_string()
                } else {
                    format!("{name} {}", index + 1)
                }
            })
            .unwrap_or_else(|| format!("Renium image {}", index + 1));
        let metadata = json!({
            "assetType": "Image",
            "displayName": display_name,
            "description": request.description,
            "creationContext": { "creator": creator },
        });
        let filename = format!("image-{}.{}", index + 1, extension);
        let (boundary, body) = multipart_body(&metadata, &filename, mime, &bytes);
        let created = checked_response(
            auth.apply(agent().post(&format!("{API_ROOT}/assets/v1/assets")))
                .set(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={boundary}"),
                )
                .send_bytes(&body),
            "image-upload",
        )?;
        let operation = match operation_path(&created) {
            Some(path) => wait_for_upload(path, &auth, request.wait_seconds)?,
            None => created,
        };
        let asset_id = operation
            .pointer("/response/assetId")
            .or_else(|| operation.get("assetId"))
            .cloned();
        let asset_id_text = asset_id.as_ref().and_then(|id| {
            id.as_str()
                .map(str::to_string)
                .or_else(|| id.as_u64().map(|id| id.to_string()))
        });
        results.push(json!({
            "source": source,
            "assetId": asset_id,
            "uri": asset_id_text.map(|id| format!("rbxassetid://{id}")),
            "operation": operation,
        }));
    }
    Ok(json!({ "images": results }))
}

pub(crate) fn studio_upload(parameters: &Value) -> bool {
    let Some(images) = parameters
        .get("images")
        .or_else(|| parameters.get("imagePaths"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    parameters.get("via").and_then(Value::as_str) != Some("open-cloud")
        && parameters.get("userId").is_none()
        && parameters.get("groupId").is_none()
        && images.iter().all(|source| {
            source.as_str().is_some_and(|source| {
                source.starts_with("https://") || source.starts_with("http://")
            })
        })
}
