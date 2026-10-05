//! Live game servers: the newest versions' servers at a glance, a job found by
//! its ID, and filters for server logs.
use std::cmp::Reverse;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde_json::{Map, Value, json};

use super::discovery::place_history;
use super::paging::{MAX_PAGES, PAGE_SIZE, Pager, Plan};
use super::routes::{Access, RouteArgs, resolve_action, write_output};
use super::transport::placeholder_hint;

const RECENT_VERSIONS: usize = 10;
const VERSION_PAGES: usize = 10;
const SCAN_THREADS: usize = 4;
const OPTIONS_PATH: &str =
    "/server-management/v1/universes/{universe}/places/{place}/game-servers:filter-options";
const LIST_PATH: &str =
    "/server-management/v1/universes/{universe}/places/{place}/versions/{version}/game-servers";
/// Log severities follow `Enum.MessageType`.
const SEVERITIES: &[&str] = &["output", "info", "warning", "error"];

#[derive(Args)]
pub(super) struct ServerArgs {
    #[command(flatten)]
    route: RouteArgs,
    #[arg(long, help = "Leave out servers that have shut down (list, find)")]
    active: bool,
    #[arg(
        long,
        value_name = "TEXT",
        help = "Keep log lines whose message or stack contains TEXT, ignoring case (logs)"
    )]
    grep: Option<String>,
    #[arg(
        long,
        value_name = "LEVEL",
        help = "Keep log lines of these comma-separated severities: error, warning, info, output (logs)"
    )]
    severity: Option<String>,
}

pub(super) struct ServerCommand {
    action: &'static str,
    args: ServerArgs,
    severities: Option<Vec<u64>>,
}

/// Checks the action and its flags before any key or network work.
pub(super) fn prepare(mut args: ServerArgs) -> Result<ServerCommand> {
    let action = resolve_action("server", args.route.action.as_deref())?;
    args.route.action = Some(action.to_string());
    if args.active && !matches!(action, "list" | "find") {
        bail!("--active works with list and find");
    }
    if (args.grep.is_some() || args.severity.is_some()) && action != "logs" {
        bail!("--grep and --severity work with logs");
    }
    let scans = action == "find" || (action == "list" && args.route.values.is_empty());
    if scans && (args.route.limit.is_some() || args.route.cursor.is_some()) {
        bail!(
            "-l and --cursor need a VERSION (rbx oc server list VERSION); without one, --pages N sets how many pages of 100 servers are read per version"
        );
    }
    if action == "find" && args.route.values.len() != 1 {
        bail!("Expected: rbx oc server find JOB");
    }
    let severities = args.severity.as_deref().map(parse_severities).transpose()?;
    Ok(ServerCommand {
        action,
        args,
        severities,
    })
}

pub(super) fn run(access: &Access, command: ServerCommand) -> Result<Value> {
    let ServerCommand {
        action,
        args,
        severities,
    } = command;
    let ServerArgs {
        route,
        active,
        grep,
        ..
    } = args;
    let pages = match route.pages {
        Some(pages) => pages.get(),
        None if route.all => MAX_PAGES,
        None => VERSION_PAGES,
    };
    let result = match action {
        "list" if route.values.is_empty() => recent(access, pages)?,
        "find" => find(access, &route.values[0], active, pages)?,
        "logs" => {
            return super::routes::run("server", access, route, |body| {
                filter_logs(body, grep.as_deref(), severities.as_deref());
            });
        }
        "list" => {
            return super::routes::run("server", access, route, |body| {
                if active {
                    keep_active(body);
                }
            });
        }
        _ => return super::routes::run("server", access, route, |_| {}),
    };
    match route.output {
        Some(path) => write_output(&path, &result, false),
        None => Ok(result),
    }
}

/// Versions to look at, newest first, and whether each is known to have
/// servers (true when the filter options listed them).
fn versions(access: &Access) -> Result<(Vec<u64>, bool)> {
    if access.identity.game_id.is_none() {
        bail!(
            "Listing servers needs {{universe}}: {}",
            placeholder_hint("universe")
        );
    }
    let place = access.identity.place_id.with_context(|| {
        format!(
            "Listing servers needs {{place}}: {}",
            placeholder_hint("place")
        )
    })?;
    let options = access.send(json!({ "method": "GET", "path": OPTIONS_PATH }));
    if let Ok(response) = &options {
        let versions = place_versions(&response["body"]);
        if !versions.is_empty() {
            return Ok((versions, true));
        }
    }
    let published = place_history(
        |request| access.send(request).ok(),
        place,
        |versions| versions.iter().filter(|version| version.published).count() >= RECENT_VERSIONS,
    )
    .into_iter()
    .flatten()
    .filter(|version| version.published)
    .map(|version| version.number)
    .collect::<Vec<_>>();
    match options {
        Err(error) if published.is_empty() => Err(error),
        _ => Ok((published, false)),
    }
}

fn place_versions(options: &Value) -> Vec<u64> {
    let mut versions = options
        .pointer("/filters/PlaceVersion/values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .collect::<Vec<_>>();
    versions.sort_unstable_by_key(|version| Reverse(*version));
    versions.dedup();
    versions
}

struct Scan {
    servers: Vec<Value>,
    more: bool,
}

fn scan(
    access: &Access,
    version: u64,
    pages: usize,
    until: impl Fn(&Value) -> bool,
) -> Result<Scan> {
    let pager = Pager {
        size: "MaxPageSize",
        token: "PageToken",
        plan: Plan { limit: None, pages },
    };
    let mut collected = pager.collect(
        json!({
            "method": "GET",
            "path": LIST_PATH,
            "pathParams": { "version": version },
            "query": { "MaxPageSize": PAGE_SIZE },
        }),
        |request| Ok(access.send(request.clone())?["body"].take()),
        until,
    )?;
    let servers = match collected.body["gameServers"].take() {
        Value::Array(servers) => servers,
        _ => Vec::new(),
    };
    Ok(Scan {
        servers,
        more: collected.more,
    })
}

fn recent(access: &Access, pages: usize) -> Result<Value> {
    let (versions, listed) = versions(access)?;
    let (scanned, older) = versions.split_at(versions.len().min(RECENT_VERSIONS));
    let mut scans = Vec::with_capacity(scanned.len());
    for chunk in scanned.chunks(SCAN_THREADS) {
        std::thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|&version| scope.spawn(move || scan(access, version, pages, |_| false)))
                .collect::<Vec<_>>();
            for (handle, &version) in handles.into_iter().zip(chunk) {
                let scan = handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("reading version {version} stopped")));
                scans.push((version, scan));
            }
        });
    }
    if scans.iter().all(|(_, scan)| scan.is_err())
        && let Some((_, Err(error))) = scans.pop()
    {
        return Err(error);
    }
    Ok(summarize(scans, if listed { older } else { &[] }))
}

fn summarize(scans: Vec<(u64, Result<Scan>)>, older: &[u64]) -> Value {
    let mut players = 0;
    let mut servers = 0;
    let mut versions = Vec::new();
    for (version, scan) in scans {
        let scan = match scan {
            Ok(scan) => scan,
            Err(error) => {
                versions.push(json!({ "version": version, "error": format!("{error:#}") }));
                continue;
            }
        };
        if scan.servers.is_empty() {
            continue;
        }
        let (active, ended): (Vec<&Value>, Vec<&Value>) =
            scan.servers.iter().partition(|server| is_active(server));
        let mut listed = active.into_iter().map(compact).collect::<Vec<_>>();
        listed.sort_by_key(|server| Reverse(server["players"].as_u64().unwrap_or(0)));
        let version_players = listed
            .iter()
            .filter_map(|server| server["players"].as_u64())
            .sum::<u64>();
        players += version_players;
        servers += listed.len();
        let mut entry = json!({
            "version": version,
            "players": version_players,
            "servers": listed,
            "ended": ended.len(),
        });
        if scan.more {
            entry["more"] = Value::Bool(true);
        }
        versions.push(entry);
    }
    let mut result = json!({ "players": players, "servers": servers, "versions": versions });
    if !older.is_empty() {
        result["olderVersions"] = json!(older);
    }
    result
}

fn find(access: &Access, job: &str, active: bool, pages: usize) -> Result<Value> {
    let job = job.trim();
    let matches = |server: &Value| {
        server
            .get("jobId")
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq_ignore_ascii_case(job))
            && (!active || is_active(server))
    };
    let (versions, _) = versions(access)?;
    let scanned = &versions[..versions.len().min(RECENT_VERSIONS)];
    for &version in scanned {
        let scan = scan(access, version, pages, |page| {
            page.get("gameServers")
                .and_then(Value::as_array)
                .is_some_and(|servers| servers.iter().any(matches))
        })?;
        if let Some(server) = scan.servers.into_iter().find(|server| matches(server)) {
            return Ok(json!({ "version": version, "server": server }));
        }
    }
    bail!(
        "No {}server with job {job} in versions {}; pass --pages N to read more of each version, or `rbx oc server list VERSION`",
        if active { "active " } else { "" },
        scanned
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn is_active(server: &Value) -> bool {
    server.get("shutDown").and_then(Value::as_bool) != Some(true)
        && server
            .get("status")
            .and_then(Value::as_str)
            .is_none_or(|status| status.eq_ignore_ascii_case("active"))
}

fn compact(server: &Value) -> Value {
    let mut entry = Map::new();
    for (from, to) in [
        ("jobId", "jobId"),
        ("occupancy", "players"),
        ("maxOccupancy", "maxPlayers"),
        ("type", "type"),
    ] {
        if let Some(value) = server.get(from) {
            entry.insert(to.to_string(), value.clone());
        }
    }
    if let Some(uptime) = server.get("uptime").and_then(Value::as_str) {
        entry.insert("uptime".to_string(), Value::String(whole_seconds(uptime)));
    }
    Value::Object(entry)
}

fn whole_seconds(uptime: &str) -> String {
    match uptime.rsplit_once(':') {
        Some((head, seconds)) => {
            format!("{head}:{}", seconds.split('.').next().unwrap_or(seconds))
        }
        None => uptime.to_string(),
    }
}

fn keep_active(body: &mut Value) {
    if let Some(servers) = body.get_mut("gameServers").and_then(Value::as_array_mut) {
        servers.retain(is_active);
    }
}

fn parse_severities(text: &str) -> Result<Vec<u64>> {
    let levels = text
        .split(',')
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(|level| {
            let name = level.to_ascii_lowercase();
            let name = if name == "warn" { "warning" } else { &name };
            SEVERITIES
                .iter()
                .position(|known| *known == name)
                .or_else(|| {
                    level
                        .parse::<usize>()
                        .ok()
                        .filter(|index| *index < SEVERITIES.len())
                })
                .map(|index| index as u64)
                .with_context(|| {
                    format!("Unknown severity '{level}'; use error, warning, info or output")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if levels.is_empty() {
        bail!("--severity needs a level: error, warning, info or output");
    }
    Ok(levels)
}

fn filter_logs(body: &mut Value, grep: Option<&str>, severities: Option<&[u64]>) {
    let needle = grep.map(str::to_lowercase);
    let Some(entries) = body.get_mut("gameServerLogs").and_then(Value::as_array_mut) else {
        return;
    };
    let fetched = entries.len();
    for entry in entries.iter_mut() {
        if let Some(name) = entry
            .get("severity")
            .and_then(Value::as_u64)
            .and_then(|level| SEVERITIES.get(usize::try_from(level).ok()?))
        {
            entry["severityName"] = Value::String((*name).to_string());
        }
    }
    if needle.is_none() && severities.is_none() {
        return;
    }
    entries.retain(|entry| {
        let level = entry.get("severity").and_then(Value::as_u64);
        severities.is_none_or(|levels| level.is_some_and(|level| levels.contains(&level)))
            && needle.as_deref().is_none_or(|needle| {
                ["message", "stackTrace"].iter().any(|field| {
                    entry
                        .get(*field)
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.to_lowercase().contains(needle))
                })
            })
    });
    body["fetched"] = json!(fetched);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(job: &str, players: u64, status: &str) -> Value {
        json!({
            "jobId": job,
            "occupancy": players,
            "maxOccupancy": 10,
            "type": 3,
            "uptime": "05:14:05.3946240",
            "status": status,
            "shutDown": status != "active",
            "playerIds": [1, 2],
        })
    }

    #[test]
    fn severities_accept_names_numbers_and_lists() {
        assert_eq!(parse_severities("error").unwrap(), vec![3]);
        assert_eq!(parse_severities("Warn, error").unwrap(), vec![2, 3]);
        assert_eq!(parse_severities("1").unwrap(), vec![1]);
        assert!(parse_severities("fatal").is_err());
        assert!(parse_severities("7").is_err());
        assert!(parse_severities(" , ").is_err());
    }

    #[test]
    fn log_filters_apply_after_fetching_and_name_the_severity() {
        let mut body = json!({
            "gameServerLogs": [
                {"severity": 3, "message": "Timeout in DataStore", "stackTrace": ""},
                {"severity": 2, "message": "[TAG DIAG] tagged", "stackTrace": ""},
                {"severity": 3, "message": "Failed", "stackTrace": "Script 'ServerScriptService.Shop', Line 4"},
            ],
            "nextPageToken": "next",
        });
        filter_logs(&mut body, Some("shop"), Some(&[3]));
        assert_eq!(body["fetched"], 3);
        assert_eq!(body["gameServerLogs"].as_array().unwrap().len(), 1);
        assert_eq!(body["gameServerLogs"][0]["message"], "Failed");
        assert_eq!(body["gameServerLogs"][0]["severityName"], "error");
        assert_eq!(body["nextPageToken"], "next");

        let mut body = json!({"gameServerLogs": [{"severity": 2, "message": "x"}]});
        filter_logs(&mut body, None, None);
        assert_eq!(body["gameServerLogs"][0]["severityName"], "warning");
        assert!(body.get("fetched").is_none());
    }

    #[test]
    fn filter_options_list_versions_newest_first() {
        let options = json!({"filters": {
            "EngineVersion": {"values": ["0.732.0.7321044"]},
            "PlaceVersion": {"field": "PlaceVersion", "type": "Number", "values": [982, "3991", 3982, 3991]},
        }});
        assert_eq!(place_versions(&options), vec![3991, 3982, 982]);
        assert!(place_versions(&json!({})).is_empty());
    }

    #[test]
    fn summaries_list_active_servers_by_version_with_player_totals() {
        let scans = vec![
            (
                2809,
                Ok(Scan {
                    servers: Vec::new(),
                    more: false,
                }),
            ),
            (
                2797,
                Ok(Scan {
                    servers: vec![
                        server("a", 1, "active"),
                        server("b", 7, "active"),
                        server("c", 0, "shut_down"),
                        server("d", 0, "crashed"),
                    ],
                    more: true,
                }),
            ),
            (2795, Err(anyhow!("HTTP 504"))),
        ];
        let summary = summarize(scans, &[2790]);
        assert_eq!(summary["players"], 8);
        assert_eq!(summary["servers"], 2);
        assert_eq!(summary["olderVersions"], json!([2790]));
        let versions = summary["versions"].as_array().unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(
            versions[0],
            json!({
                "version": 2797,
                "players": 8,
                "ended": 2,
                "more": true,
                "servers": [
                    {"jobId": "b", "players": 7, "maxPlayers": 10, "type": 3, "uptime": "05:14:05"},
                    {"jobId": "a", "players": 1, "maxPlayers": 10, "type": 3, "uptime": "05:14:05"},
                ],
            })
        );
        assert_eq!(versions[1]["version"], 2795);
        assert_eq!(versions[1]["error"], "HTTP 504");
    }

    #[test]
    fn active_means_not_shut_down() {
        assert!(is_active(&server("a", 1, "active")));
        assert!(!is_active(&server("a", 0, "shut_down")));
        assert!(!is_active(&server("a", 0, "restarted")));
        assert!(is_active(&json!({"jobId": "a"})));
        let mut body = json!({"gameServers": [server("a", 1, "active"), server("b", 0, "shut_down")], "totalCount": 2});
        keep_active(&mut body);
        assert_eq!(body["gameServers"].as_array().unwrap().len(), 1);
        assert_eq!(whole_seconds("1.02:03:04.5"), "1.02:03:04");
        assert_eq!(whole_seconds("00:00:07"), "00:00:07");
    }
}
