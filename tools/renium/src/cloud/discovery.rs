//! What an API key can reach: the experiences it is scoped to plus the public
//! experiences of its user and groups, found by name, and pulled into a
//! project as a place file.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::transport::{
    CloudIdentity, download_to_file, execute_one, fetch_public_json, introspect_key,
};

const GAMES_HOST: &str = "https://games.roblox.com";
const GROUPS_HOST: &str = "https://groups.roblox.com";
const GROUP_LIMIT: usize = 40;
const CACHE_SECONDS: u64 = 15 * 60;
const DEVELOP_HOST: &str = "https://develop.roblox.com";
const PAGE_LIMIT: usize = 10;
const HISTORY_PAGES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Experience {
    pub(crate) universe_id: i64,
    pub(crate) name: String,
    pub(crate) root_place_id: Option<i64>,
    pub(crate) creator: String,
    pub(crate) source: &'static str,
}

impl Experience {
    fn to_value(&self) -> Value {
        json!({
            "universeId": self.universe_id,
            "name": self.name,
            "rootPlaceId": self.root_place_id,
            "creator": self.creator,
            "source": self.source,
        })
    }
}

struct Reach {
    user_id: Option<i64>,
    group_ids: Vec<i64>,
    universe_ids: Vec<i64>,
}

fn ids(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::Number(number) => number.as_i64(),
                    Value::String(text) => text.parse().ok(),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn reach(key_env: &str) -> Result<Reach> {
    let info = introspect_key(key_env).map_err(super::command::cloud_error)?;
    let user_id = info.get("authorizedUserId").and_then(Value::as_i64);
    let mut group_ids = manageable_groups(key_env)?;
    if let Some(user_id) = user_id {
        group_ids.extend(member_groups(user_id)?);
    }
    let mut universe_ids = Vec::new();
    for scope in info
        .get("scopes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        group_ids.extend(ids(scope.get("groupIds")));
        universe_ids.extend(ids(scope.get("universeIds")));
    }
    group_ids.sort_unstable();
    group_ids.dedup();
    universe_ids.sort_unstable();
    universe_ids.dedup();
    group_ids.truncate(GROUP_LIMIT);
    Ok(Reach {
        user_id,
        group_ids,
        universe_ids,
    })
}

// Groups the key's user can manage cover experiences owned by groups the key
// itself is not scoped to, which is where most team games live.
fn manageable_groups(key_env: &str) -> Result<Vec<i64>> {
    // Keys without the legacy group scope get a 403 here; that only narrows
    // the search, so it is not an error.
    let Ok(response) = execute_one(
        CloudIdentity::default(),
        key_env,
        None,
        false,
        json!({ "method": "GET", "path": "/legacy-develop/v1/user/groups/canmanage" }),
    ) else {
        return Ok(Vec::new());
    };
    if response.get("status").and_then(Value::as_u64) != Some(200) {
        return Ok(Vec::new());
    }
    Ok(response
        .pointer("/body/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("id").and_then(Value::as_i64))
        .collect())
}

// Every group the user belongs to, highest role first; the public games of
// those groups are where a team's experiences usually live.
fn member_groups(user_id: i64) -> Result<Vec<i64>> {
    let page = fetch_public_json(
        &format!("{GROUPS_HOST}/v1/users/{user_id}/groups/roles"),
        "cloud games",
    )
    .map_err(super::command::cloud_error)?;
    let mut groups = page
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry
                    .pointer("/role/rank")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                entry.pointer("/group/id").and_then(Value::as_i64)?,
            ))
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(groups
        .into_iter()
        .filter(|(rank, _)| *rank >= 100)
        .map(|(_, id)| id)
        .collect())
}

fn public_pages(url: &str, next: &str) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..PAGE_LIMIT {
        let page_url = match cursor.as_deref() {
            Some(cursor) => format!("{url}&cursor={cursor}"),
            None => url.to_string(),
        };
        let page = fetch_public_json(&page_url, next).map_err(super::command::cloud_error)?;
        items.extend(
            page.get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        cursor = page
            .get("nextPageCursor")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(items)
}

fn game_entry(item: &Value, source: &'static str) -> Option<Experience> {
    Some(Experience {
        universe_id: item.get("id")?.as_i64()?,
        name: item.get("name")?.as_str()?.to_string(),
        root_place_id: item.pointer("/rootPlace/id").and_then(Value::as_i64),
        creator: item
            .pointer("/creator/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        source,
    })
}

fn scoped_universes(universe_ids: &[i64]) -> Result<Vec<Experience>> {
    let mut found = Vec::new();
    for chunk in universe_ids.chunks(50) {
        let list = chunk
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let page = fetch_public_json(
            &format!("{DEVELOP_HOST}/v1/universes/multiget?ids={list}"),
            "cloud games",
        )
        .map_err(super::command::cloud_error)?;
        for item in page
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(universe_id) = item.get("id").and_then(Value::as_i64) else {
                continue;
            };
            found.push(Experience {
                universe_id,
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                root_place_id: item.get("rootPlaceId").and_then(Value::as_i64),
                creator: item
                    .get("creatorName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                source: "key",
            });
        }
    }
    Ok(found)
}

fn cache_path(user_id: i64) -> Option<PathBuf> {
    crate::app::update::user_data_dir().ok().map(|dir| {
        dir.join("cache")
            .join(format!("open-cloud-games-{user_id}.json"))
    })
}

fn cached_experiences(user_id: i64) -> Option<Vec<Experience>> {
    let path = cache_path(user_id)?;
    let age = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()?
        .elapsed()
        .ok()?;
    if age.as_secs() > CACHE_SECONDS {
        return None;
    }
    let items: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    Some(
        items
            .iter()
            .filter_map(|item| {
                Some(Experience {
                    universe_id: item.get("universeId")?.as_i64()?,
                    name: item.get("name")?.as_str()?.to_string(),
                    root_place_id: item.get("rootPlaceId").and_then(Value::as_i64),
                    creator: item
                        .get("creator")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    source: "cache",
                })
            })
            .collect(),
    )
}

fn remember_experiences(user_id: i64, experiences: &[Experience]) {
    let Some(path) = cache_path(user_id) else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        let items = experiences
            .iter()
            .map(Experience::to_value)
            .collect::<Vec<_>>();
        let _ = std::fs::write(&path, serde_json::to_vec(&items).unwrap_or_default());
    }
}

/// Every experience the key can reach, deduplicated by universe. A full
/// listing is cached for a few minutes; with a query and no cache, sources
/// are visited in order of likelihood and the search stops at the first
/// exact name match, so a lookup rarely touches every group.
pub(crate) fn experiences(key_env: &str, query: Option<&str>) -> Result<Vec<Experience>> {
    let reach = reach(key_env)?;
    if let Some(cached) = reach.user_id.and_then(cached_experiences) {
        return Ok(cached);
    }
    let wanted = query.map(normalized).filter(|value| !value.is_empty());
    let mut all = scoped_universes(&reach.universe_ids)?;
    let found = |all: &[Experience]| {
        wanted.as_deref().is_some_and(|wanted| {
            all.iter()
                .any(|experience| normalized(&experience.name) == wanted)
        })
    };
    if let Some(user_id) = reach.user_id
        && !found(&all)
    {
        let url = format!(
            "{GAMES_HOST}/v2/users/{user_id}/games?accessFilter=Public&limit=50&sortOrder=Asc"
        );
        all.extend(
            public_pages(&url, "cloud games")?
                .iter()
                .filter_map(|item| game_entry(item, "user")),
        );
    }
    for group_id in &reach.group_ids {
        if found(&all) {
            break;
        }
        let url = format!(
            "{GAMES_HOST}/v2/groups/{group_id}/gamesV2?accessFilter=Public&limit=50&sortOrder=Asc"
        );
        all.extend(
            public_pages(&url, "cloud games")?
                .iter()
                .filter_map(|item| game_entry(item, "group")),
        );
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let mut unique: Vec<Experience> = Vec::new();
    for experience in all {
        if let Some(existing) = unique
            .iter_mut()
            .find(|existing| existing.universe_id == experience.universe_id)
        {
            if existing.root_place_id.is_none() {
                existing.root_place_id = experience.root_place_id;
            }
            if existing.creator.is_empty() {
                existing.creator = experience.creator;
            }
        } else {
            unique.push(experience);
        }
    }
    unique.sort_by_key(|experience| experience.name.to_lowercase());
    if wanted.is_none()
        && let Some(user_id) = reach.user_id
    {
        remember_experiences(user_id, &unique);
    }
    Ok(unique)
}

/// Letters and digits only, lowercase, so emoji, brackets and spacing in a
/// Roblox title never decide whether a name matches.
pub(crate) fn normalized(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

pub(crate) fn matches<'a>(experiences: &'a [Experience], query: &str) -> Vec<&'a Experience> {
    let wanted = normalized(query);
    if wanted.is_empty() {
        return Vec::new();
    }
    if let Ok(id) = query.trim().parse::<i64>() {
        let by_id = experiences
            .iter()
            .filter(|experience| {
                experience.universe_id == id || experience.root_place_id == Some(id)
            })
            .collect::<Vec<_>>();
        if !by_id.is_empty() {
            return by_id;
        }
    }
    let exact = experiences
        .iter()
        .filter(|experience| normalized(&experience.name) == wanted)
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }
    experiences
        .iter()
        .filter(|experience| normalized(&experience.name).contains(&wanted))
        .collect()
}

pub(crate) fn games_command(key_env: &str, query: Option<&str>) -> Result<Value> {
    let query = query.map(str::trim).filter(|value| !value.is_empty());
    let all = experiences(key_env, query)?;
    let Some(query) = query else {
        return Ok(json!({
            "count": all.len(),
            "experiences": all.iter().map(Experience::to_value).collect::<Vec<_>>(),
        }));
    };
    let found = matches(&all, query);
    Ok(json!({
        "query": query,
        "count": found.len(),
        "matches": found.iter().map(|experience| experience.to_value()).collect::<Vec<_>>(),
        "searched": all.len(),
    }))
}

/// Live player count and visits from the public games endpoint, which needs
/// no key; None when it cannot be read.
pub(crate) fn live_counts(universe_id: i64) -> Option<Map<String, Value>> {
    let page = fetch_public_json(
        &format!("{GAMES_HOST}/v1/games?universeIds={universe_id}"),
        "cloud universe",
    )
    .ok()?;
    let counts = counts_from(&page, universe_id);
    (!counts.is_empty()).then_some(counts)
}

fn counts_from(page: &Value, universe_id: i64) -> Map<String, Value> {
    let game = page
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|game| game.get("id").and_then(Value::as_i64) == Some(universe_id));
    ["playing", "visits"]
        .into_iter()
        .filter_map(|name| Some((name.to_string(), game?.get(name)?.clone())))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaceVersion {
    pub(crate) number: u64,
    pub(crate) published: bool,
}

/// The place's versions, newest first, read page by page until `enough`
/// accepts them or the history ends; None when the history cannot be read.
pub(crate) fn place_history(
    send: impl Fn(Value) -> Option<Value>,
    place_id: i64,
    enough: impl Fn(&[PlaceVersion]) -> bool,
) -> Option<Vec<PlaceVersion>> {
    let mut versions = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..HISTORY_PAGES {
        let mut query = json!({ "pageSize": 20 });
        if let Some(cursor) = &cursor {
            query["cursor"] = json!(cursor);
        }
        let response = send(json!({
            "method": "GET",
            "path": "/place-version-history-api/v1/{place}/history",
            "pathParams": { "place": place_id },
            "query": query,
        }))?;
        let body = response.get("body").unwrap_or(&response);
        versions.extend(
            body.get("placeVersions")?
                .as_array()?
                .iter()
                .filter_map(|entry| {
                    let version = entry.get("version")?;
                    Some(PlaceVersion {
                        number: version
                            .as_u64()
                            .or_else(|| version.as_str()?.parse().ok())?,
                        published: entry
                            .get("isPublished")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                }),
        );
        if enough(&versions) {
            break;
        }
        cursor = body
            .get("hasMore")
            .and_then(Value::as_bool)
            .filter(|more| *more)
            .and_then(|_| body.get("nextCursor")?.as_str().map(str::to_string));
        if cursor.is_none() {
            break;
        }
    }
    Some(versions)
}

/// The version a fetch without `--version` downloads: the newest published
/// one, else the newest saved one.
fn default_version(history: &[PlaceVersion]) -> Option<u64> {
    history
        .iter()
        .find(|version| version.published)
        .or_else(|| history.first())
        .map(|version| version.number)
}

pub(crate) struct FetchRequest {
    pub(crate) name: Option<String>,
    pub(crate) output: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) version: Option<u64>,
}

fn file_name_for(name: &str, place_id: i64) -> String {
    let mut stem = String::new();
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !stem.is_empty() {
                stem.push('-');
            }
            pending_dash = false;
            stem.push(ch);
        } else if ch.is_whitespace() || matches!(ch, '-' | '_' | '.') {
            pending_dash = true;
        }
    }
    if stem.is_empty() {
        stem = format!("place-{place_id}");
    }
    format!("{stem}.rbxl")
}

fn resolve_place(
    key_env: &str,
    identity: CloudIdentity,
    name: Option<&str>,
) -> Result<(i64, String)> {
    if let Some(place_id) = identity.place_id {
        return Ok((place_id, format!("place-{place_id}")));
    }
    if let Some(universe_id) = identity.game_id {
        let found = scoped_universes(&[universe_id])?;
        let experience = found
            .into_iter()
            .next()
            .with_context(|| format!("Universe {universe_id} was not found"))?;
        let root = experience
            .root_place_id
            .with_context(|| format!("Universe {universe_id} reports no root place"))?;
        return Ok((root, experience.name));
    }
    let query = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("Name the experience to fetch, or pass --universe ID or --place-id ID")?;
    let all = experiences(key_env, Some(query))?;
    let found = matches(&all, query);
    match found.as_slice() {
        [] => bail!(
            "No experience named {query:?} among the {} this key can reach; `rbx oc games` lists them",
            all.len()
        ),
        [one] => Ok((
            one.root_place_id
                .with_context(|| format!("{} reports no root place", one.name))?,
            one.name.clone(),
        )),
        many => bail!(
            "{query:?} matches {} experiences: {}; pass --universe ID",
            many.len(),
            many.iter()
                .map(|experience| format!("{} ({})", experience.name, experience.universe_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn fetch_command(
    identity: CloudIdentity,
    key_env: &str,
    oauth_env: Option<&str>,
    request: FetchRequest,
) -> Result<Value> {
    let (place_id, name) = resolve_place(key_env, identity, request.name.as_deref())?;
    // Asset delivery can serve a stale copy of "the current place" for a while
    // after a publish, so the newest published version is fetched by number
    // whenever the key can read the place's history.
    let version = request.version.or_else(|| {
        place_history(
            |request| execute_one(identity, key_env, oauth_env, false, request).ok(),
            place_id,
            |versions| versions.iter().any(|version| version.published),
        )
        .as_deref()
        .and_then(default_version)
    });
    let delivery_request = match version {
        Some(version) => json!({
            "method": "GET",
            "path": "/asset-delivery-api/v1/assetId/{assetId}/version/{versionNumber}",
            "pathParams": { "assetId": place_id, "versionNumber": version },
        }),
        None => json!({
            "method": "GET",
            "path": "/asset-delivery-api/v1/assetId/{assetId}",
            "pathParams": { "assetId": place_id },
        }),
    };
    let delivery = match execute_one(identity, key_env, oauth_env, false, delivery_request) {
        Ok(delivery) => delivery,
        Err(failure) => {
            let status = failure
                .0
                .d
                .as_ref()
                .and_then(|detail| detail.get("status"))
                .and_then(Value::as_u64);
            if matches!(status, Some(401 | 403)) {
                bail!(
                    "This API key may not read place {place_id} ({name}); use a key created for that experience or its group: `rbx oc key add NAME`, then `--key NAME`"
                );
            }
            return Err(super::command::cloud_error(failure));
        }
    };
    let status = delivery
        .get("status")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let location = delivery
        .pointer("/body/location")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let Some(location) = location.filter(|_| (200..300).contains(&status)) else {
        if status == 403 || status == 401 {
            bail!(
                "This API key may not read place {place_id} ({name}); use a key created for that experience or its group: `rbx oc key add NAME`, then `--key NAME`"
            );
        }
        bail!(
            "Asset delivery for place {place_id} returned HTTP {status} without a download location: {}",
            delivery
                .get("body")
                .map(Value::to_string)
                .unwrap_or_default()
        );
    };
    let file_name = match request.version {
        Some(version) => {
            let base = file_name_for(&name, place_id);
            format!("{}-v{version}.rbxl", base.trim_end_matches(".rbxl"))
        }
        None => file_name_for(&name, place_id),
    };
    let output = match (request.output, request.project_root.as_deref()) {
        (Some(output), _) => output,
        (None, Some(root)) => root.join(&file_name),
        (None, None) => PathBuf::from(&file_name),
    };
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let bytes =
        download_to_file(location, &output, "cloud fetch").map_err(super::command::cloud_error)?;
    let mut result = json!({
        "ok": true,
        "placeId": place_id,
        "name": name,
        "file": output,
        "bytes": bytes,
        "version": version,
    });
    if version.is_none() {
        result["versionNote"] = json!(
            "this key cannot read the place's version history, so the number of the current version it downloaded is unknown"
        );
    }
    if let Some(root) = request.project_root {
        import_into(&root, &output)?;
        result["projectRoot"] = json!(root);
        result["imported"] = json!(true);
    }
    Ok(result)
}

fn import_into(root: &Path, place: &Path) -> Result<()> {
    crate::snapshot::place_import::import_place_file(crate::cli::ImportPlaceArgs {
        input: place.to_path_buf(),
        project_root: root.to_path_buf(),
        src_dir: PathBuf::from("src"),
        services: String::new(),
    })?;
    crate::project::workflows::refresh_agent_instructions(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn experience(id: i64, name: &str) -> Experience {
        Experience {
            universe_id: id,
            name: name.to_string(),
            root_place_id: Some(id * 10),
            creator: "Studio".to_string(),
            source: "user",
        }
    }

    #[test]
    fn names_match_without_emoji_brackets_or_case() {
        let list = vec![
            experience(1, "[🏘️] Brainrot Town"),
            experience(2, "Brainrot Town Testing"),
            experience(3, "Drift Tag"),
        ];
        let exact = matches(&list, "brainrot town");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].universe_id, 1);
        assert_eq!(matches(&list, "brainrot").len(), 2);
        assert_eq!(matches(&list, "30")[0].universe_id, 3);
        assert_eq!(matches(&list, "2")[0].universe_id, 2);
        assert!(matches(&list, "").is_empty());
        assert!(matches(&list, "🏘️").is_empty());
    }

    #[test]
    fn live_counts_come_from_the_matching_game_only() {
        let page = json!({"data": [
            {"id": 7, "playing": 1, "visits": 2},
            {"id": 8420907710_i64, "playing": 1234, "visits": 98765, "name": "Drift Tag"},
        ]});
        assert_eq!(
            Value::Object(counts_from(&page, 8420907710)),
            json!({"playing": 1234, "visits": 98765})
        );
        assert!(counts_from(&page, 9).is_empty());
        assert!(counts_from(&json!({"errors": []}), 7).is_empty());
    }

    #[test]
    fn fetch_defaults_to_the_newest_published_version_then_the_newest_saved() {
        let version = |number, published| PlaceVersion { number, published };
        assert_eq!(
            default_version(&[version(12, false), version(11, true), version(10, true)]),
            Some(11)
        );
        assert_eq!(default_version(&[version(12, false)]), Some(12));
        assert_eq!(default_version(&[]), None);
    }

    #[test]
    fn place_history_reads_pages_until_enough() {
        let calls = std::cell::Cell::new(0);
        let versions = place_history(
            |request| {
                calls.set(calls.get() + 1);
                let page = if request["query"].get("cursor").is_some() {
                    json!({"placeVersions": [{"version": 3, "isPublished": true}], "hasMore": false})
                } else {
                    json!({"placeVersions": [{"version": "5"}, {"version": 4, "isPublished": false}], "hasMore": true, "nextCursor": "next"})
                };
                Some(json!({ "status": 200, "body": page }))
            },
            1,
            |versions| versions.iter().any(|version| version.published),
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(
            versions,
            vec![
                PlaceVersion {
                    number: 5,
                    published: false
                },
                PlaceVersion {
                    number: 4,
                    published: false
                },
                PlaceVersion {
                    number: 3,
                    published: true
                },
            ]
        );
        assert!(place_history(|_| None, 1, |_| false).is_none());
    }

    #[test]
    fn place_files_get_plain_names() {
        assert_eq!(file_name_for("[🏘️] Brainrot Town", 5), "Brainrot-Town.rbxl");
        assert_eq!(
            file_name_for("Drift Tag: Reloaded!", 5),
            "Drift-Tag-Reloaded.rbxl"
        );
        assert_eq!(file_name_for("🏘️", 42), "place-42.rbxl");
    }
}
