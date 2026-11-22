use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::app::output::ensure_plugin_api_ok;
use crate::automation::Failure;
use crate::cloud::{API_ROOT, CloudAuth, agent, read_response};
use crate::studio::automation::studio_login::StudioLogin;
use crate::studio::bridge::{BridgeServer, BridgeTarget};

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
    audio_min_duration: Option<u32>,
    audio_max_duration: Option<u32>,
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
    via: Option<String>,
}

fn default_store_scope() -> String {
    "creator-store".to_string()
}

fn default_key_env() -> String {
    crate::cloud::transport::DEFAULT_KEY_ENV.to_string()
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

fn studio_user_id(
    bridge: Option<&BridgeServer>,
    runtime: Option<&str>,
    next: &str,
) -> Result<u64, Failure> {
    let bridge = bridge.ok_or_else(|| {
        failure(
            "no_studio",
            "User inventory search needs a connected Studio or an explicit userId",
            false,
            "studios",
        )
    })?;
    let result = match runtime {
        Some(runtime) => bridge.call_for_runtime_with_timeout(
            "getCreatorContext",
            json!({}),
            BridgeTarget::Edit,
            runtime,
            None,
        ),
        None => bridge.call_for_target("getCreatorContext", json!({}), BridgeTarget::Edit),
    }
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

pub(crate) fn search(
    parameters: &Value,
    studio: Option<(&BridgeServer, &str)>,
) -> Result<Value, Failure> {
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
                url.query_pairs_mut().append_pair("pageToken", cursor);
            }
            for facet in &request.facets {
                url.query_pairs_mut().append_pair("facets", facet);
            }
            for tag in &request.tags {
                url.query_pairs_mut().append_pair("tags", tag);
            }
            for (name, value) in [
                (
                    "includeOnlyVerifiedCreators",
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
                (
                    "audioMinDurationSeconds",
                    request.audio_min_duration.map(|value| json!(value)),
                ),
                (
                    "audioMaxDurationSeconds",
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
                _ => studio_user_id(
                    studio.map(|(bridge, _)| bridge),
                    studio.map(|(_, runtime)| runtime),
                    "asset-search",
                )?,
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

/// How an upload reaches Roblox: with an Open Cloud credential, or without
/// one under the login of the connected Studio.
enum UploadRoute<'a> {
    OpenCloud(CloudAuth),
    Login(StudioLogin<'a>),
}

/// Whether a request that names no credential may go under the Studio login:
/// it asked for neither Open Cloud nor a key or token of its own, and no
/// default key is stored (`default_stored`), which it would have gone with.
fn login_allowed(request: &ImageUpload, default_stored: bool) -> bool {
    request.via.as_deref() != Some("open-cloud")
        && request.oauth_env.is_none()
        && request.key_env == crate::cloud::transport::DEFAULT_KEY_ENV
        && crate::cloud::keys::selected().is_none()
        && !default_stored
}

impl<'a> UploadRoute<'a> {
    fn choose(request: &ImageUpload, bridge: Option<&'a BridgeServer>) -> Result<Self, Failure> {
        let login = |reason: &str| {
            let refused = |error: String| {
                let text = format!("{reason} the upload goes under the Studio login: {error}");
                failure("cloud_auth", text, false, "image-upload")
            };
            let bridge = bridge.ok_or_else(|| refused("no Studio is connected".into()))?;
            StudioLogin::connect(bridge)
                .map(Self::Login)
                .map_err(|error| refused(format!("{error:#}")))
        };
        if request.via.as_deref() == Some("login") {
            return login("As asked,");
        }
        match CloudAuth::from_env(
            false,
            &request.key_env,
            request.oauth_env.as_deref(),
            "image-upload",
        ) {
            Ok(auth) => Ok(Self::OpenCloud(auth)),
            Err(missing)
                if bridge.is_some()
                    && login_allowed(request, crate::cloud::keys::default_stored()) =>
            {
                login(&format!("{}; without a key", missing.0.m))
            }
            Err(missing) => Err(missing),
        }
    }

    /// Sends one multipart upload and returns what Roblox answered.
    fn create(&self, boundary: &str, body: &[u8]) -> Result<Value, Failure> {
        let content_type = format!("multipart/form-data; boundary={boundary}");
        match self {
            Self::OpenCloud(auth) => checked_response(
                auth.apply(agent().post(&format!("{API_ROOT}/assets/v1/assets")))
                    .set("Content-Type", &content_type)
                    .send_bytes(body),
                "image-upload",
            ),
            Self::Login(login) => login_answer(login.post(
                &format!("{API_ROOT}/assets/user-auth/v1/assets"),
                &content_type,
                body,
            )),
        }
    }

    /// Reads the upload operation `id`.
    fn operation(&self, id: &str) -> Result<Value, Failure> {
        match self {
            Self::OpenCloud(auth) => checked_response(
                auth.apply(agent().get(&format!("{API_ROOT}/assets/v1/operations/{id}")))
                    .call(),
                "image-upload",
            ),
            Self::Login(login) => {
                login_answer(login.get(&format!("{API_ROOT}/assets/user-auth/v1/operations/{id}")))
            }
        }
    }

    /// The wait between two reads of an operation; a read under the Studio
    /// login is a task on Studio's main thread.
    fn poll(&self) -> Duration {
        match self {
            Self::OpenCloud(_) => Duration::from_millis(200),
            Self::Login(_) => Duration::from_secs(1),
        }
    }

    /// Puts the route, the creator and the credential's name in front of
    /// what Roblox answered: a refusal such as "User is moderated" names none
    /// of them.
    fn refusal(&self, mut refusal: Failure, creator: &str) -> Failure {
        refusal.0.m = match self {
            Self::OpenCloud(auth) => format!(
                "Open Cloud upload for {creator} with {}: {}",
                auth.name(),
                refusal.0.m
            ),
            Self::Login(_) => format!(
                "Upload for {creator} under the Studio login: {}",
                refusal.0.m
            ),
        };
        refusal
    }
}

/// The JSON a request under the Studio login was answered with.
fn login_answer(answer: anyhow::Result<String>) -> Result<Value, Failure> {
    let text = answer
        .map_err(|error| failure("cloud_http", format!("{error:#}"), false, "image-upload"))?;
    serde_json::from_str(&text).map_err(|_| {
        failure(
            "cloud_http",
            format!("Roblox answered with text that is not JSON: {text}"),
            false,
            "image-upload",
        )
    })
}

fn wait_for_upload(
    path: &str,
    route: &UploadRoute<'_>,
    wait_seconds: f64,
) -> Result<Value, Failure> {
    let operation_id = path.trim_start_matches("operations/");
    let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds.clamp(0.0, 120.0));
    loop {
        let operation = route.operation(operation_id)?;
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
        thread::sleep(route.poll());
    }
}

pub(crate) fn upload(
    root: &Path,
    parameters: &Value,
    bridge: Option<&BridgeServer>,
) -> Result<Value, Failure> {
    // A request that came through the daemon carries how the command line
    // reaches Studio.
    let mut parameters = parameters.clone();
    if let Some(options) = parameters.as_object_mut() {
        options.remove("bridgePorts");
        options.remove("bridgeWaitSeconds");
    }
    let request: ImageUpload = serde_json::from_value(parameters).map_err(|error| {
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
    let (creator, creator_name) = if let Some(group_id) = request.group_id.filter(|id| *id > 0) {
        (
            json!({ "groupId": group_id.to_string() }),
            format!("group {group_id}"),
        )
    } else {
        let user_id = match request.user_id.filter(|id| *id > 0) {
            Some(id) => id,
            None => studio_user_id(bridge, None, "image-upload")?,
        };
        (
            json!({ "userId": user_id.to_string() }),
            format!("user {user_id}"),
        )
    };
    let route = UploadRoute::choose(&request, bridge)?;
    let refused = |refusal| route.refusal(refusal, &creator_name);
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
        let created = route.create(&boundary, &body).map_err(refused)?;
        let operation = match operation_path(&created) {
            Some(path) => wait_for_upload(path, &route, request.wait_seconds).map_err(refused)?,
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
    // The command line sends every option, the unset ones as null.
    let unset = |name: &str| parameters.get(name).is_none_or(Value::is_null);
    parameters.get("via").and_then(Value::as_str) != Some("open-cloud")
        && unset("userId")
        && unset("groupId")
        && images.iter().all(|source| {
            source.as_str().is_some_and(|source| {
                source.starts_with("https://") || source.starts_with("http://")
            })
        })
}

#[cfg(test)]
#[test]
fn web_images_without_a_creator_or_route_go_through_studio() {
    let request = |user: Value, via: Value, image: &str| json!({ "images": [image], "userId": user, "groupId": null, "via": via });
    let web = "https://example.com/a.png";
    assert!(studio_upload(&request(Value::Null, Value::Null, web)));
    assert!(studio_upload(&json!({ "imagePaths": [web] })));
    assert!(!studio_upload(&request(json!(7), Value::Null, web)));
    assert!(!studio_upload(&request(
        Value::Null,
        json!("open-cloud"),
        web
    )));
    assert!(!studio_upload(&request(Value::Null, Value::Null, "a.png")));
}

#[cfg(test)]
#[test]
fn an_upload_asked_through_the_daemon_is_read_with_its_connection_options() {
    let asked = json!({ "images": [], "bridgePorts": "1-2", "bridgeWaitSeconds": 5.0 });
    let refusal = upload(Path::new("."), &asked, None)
        .err()
        .map(|refusal| refusal.0.m);
    assert_eq!(
        refusal.as_deref(),
        Some("image-upload requires 1 through 20 images")
    );
}

#[cfg(test)]
#[test]
fn only_a_request_that_names_no_credential_may_use_the_studio_login() {
    let request = |options: Value| {
        let mut request = json!({ "images": ["a.png"] });
        request
            .as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        serde_json::from_value::<ImageUpload>(request).unwrap()
    };
    assert!(login_allowed(&request(json!({})), false));
    assert!(login_allowed(
        &request(json!({ "groupId": 7, "keyEnv": "ROBLOX_API_KEY" })),
        false
    ));
    assert!(
        !login_allowed(&request(json!({})), true),
        "a stored default key that could not be read"
    );
    for named in [
        json!({ "via": "open-cloud" }),
        json!({ "oauthEnv": "TOKEN" }),
        json!({ "keyEnv": "OTHER_KEY" }),
    ] {
        assert!(!login_allowed(&request(named.clone()), false), "{named}");
    }
    // With a Studio connection at hand, a request that names its own key
    // fails on that key; only one that asks for the login reaches for it.
    let bridge = crate::studio::bridge::routing_tests::fixture();
    let refused = |options: Value, bridge: Option<&BridgeServer>| {
        UploadRoute::choose(&request(options), bridge)
            .err()
            .map(|refusal| refusal.0.m)
            .unwrap_or_default()
    };
    let own = refused(
        json!({ "keyEnv": "RENIUM_TEST_NO_SUCH_KEY" }),
        Some(&bridge),
    );
    assert!(own.contains("RENIUM_TEST_NO_SUCH_KEY is not set"), "{own}");
    assert!(!own.contains("Studio login"), "{own}");
    let asked = refused(json!({ "via": "login" }), None);
    assert!(
        asked.contains("under the Studio login: no Studio is connected"),
        "{asked}"
    );
    let stores = crate::tests::support::temp_dir("upload-keys");
    let store = stores.join("keys.json");
    assert!(!crate::cloud::keys::default_stored_in(&store));
    std::fs::write(&store, "not a store").unwrap();
    assert!(crate::cloud::keys::default_stored_in(&store));
    let _ = std::fs::remove_dir_all(stores);
    let created = login_answer(Ok(r#"{"path":"operations/1"}"#.into())).ok();
    assert_eq!(created, Some(json!({ "path": "operations/1" })));
    for answer in [
        Ok("<html>".to_string()),
        Err(anyhow::anyhow!("Roblox answered HTTP 400")),
    ] {
        assert_eq!(login_answer(answer).unwrap_err().0.c, "cloud_http");
    }
}

/// Whether an upload request finds an Open Cloud credential where it runs;
/// without one it goes to the daemon, for the login of its Studio.
pub(crate) fn has_credential(key_env: &str, oauth_env: Option<&str>) -> bool {
    CloudAuth::from_env(false, key_env, oauth_env, "image-upload").is_ok()
}

#[cfg(test)]
#[test]
fn a_refused_upload_names_its_route_creator_and_key() {
    let answer =
        r#"Roblox returned HTTP 403: {"errors":[{"code":0,"message":"User is moderated"}]}"#;
    let auth = CloudAuth::ApiKey {
        secret: "rbx-secret".to_string(),
        name: "the stored default key studio".to_string(),
    };
    let refusal = failure("cloud_auth", answer, false, "image-upload");
    let refusal = UploadRoute::OpenCloud(auth)
        .refusal(refusal, "user 123456")
        .0;
    assert_eq!(
        refusal.m,
        format!("Open Cloud upload for user 123456 with the stored default key studio: {answer}")
    );
    assert_eq!(refusal.c, "cloud_auth");
}
