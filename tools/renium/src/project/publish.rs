use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(any(windows, target_os = "macos", test))]
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde_json::{Value, json};

use super::{config, experience, workflows};
use crate::app;
use crate::automation::{BoundContext, commands::daemon_result, live, op};
use crate::cli::BridgeConnectionArgs;
use crate::cloud;
use crate::rbx::model::RbxPlaceFormat;
use crate::studio::bridge::{BridgeApplicationError, BridgeServer, BridgeTarget};
use crate::system::files::{absolutize_for_daemon, create_unique_directory};

const MAX_PLACE_BYTES: u64 = 100 * 1024 * 1024;
const PUBLISH_SECONDS: u64 = 120;
const LIVE_SYNC_SETTLE_SECONDS: f64 = 20.0;
#[cfg(any(windows, target_os = "macos"))]
const STUDIO_PUBLISH_ACTION: &str = "publishToRobloxAction";
#[cfg(any(windows, target_os = "macos"))]
const STUDIO_PUBLISH_WAIT: Duration = Duration::from_secs(600);

#[derive(Args)]
#[command(group = clap::ArgGroup::new("cloud_mode").args(["open_cloud", "publish_as"]))]
pub(crate) struct PublishArgs {
    #[arg(
        long,
        help = "Build and upload project files using an Open Cloud API key instead of Studio"
    )]
    open_cloud: bool,
    #[arg(
        long,
        requires = "open_cloud",
        value_name = "PLACE.rbxl|PLACE.rbxlx",
        help = "Upload this place file instead of building the project"
    )]
    file: Option<PathBuf>,
    #[arg(
        long = "as",
        value_name = "PLACE_ID",
        conflicts_with_all = ["open_cloud", "file", "place_id"],
        value_parser = clap::value_parser!(i64).range(1..),
        help = "Publish the open Studio place to this place through Open Cloud, as Studio's Publish As does, without a file round trip"
    )]
    publish_as: Option<i64>,
    #[arg(
        long,
        requires = "publish_as",
        help = "With --as, save a version without publishing it"
    )]
    saved: bool,
    #[arg(
        long,
        requires = "cloud_mode",
        value_parser = clap::value_parser!(i64).range(1..),
        help = "Universe of the destination place; defaults to the project's experience"
    )]
    universe: Option<i64>,
    #[arg(long, requires = "open_cloud", value_parser = clap::value_parser!(i64).range(1..))]
    place_id: Option<i64>,
    #[arg(
        long,
        requires = "cloud_mode",
        value_name = "ENV",
        help = "API-key environment variable (default ROBLOX_API_KEY)"
    )]
    key_env: Option<String>,
    #[arg(
        long,
        requires = "cloud_mode",
        value_name = "NAME",
        help = "Use this stored API key"
    )]
    key: Option<String>,
    #[arg(
        long,
        help = "Validate and show the source and destination without publishing or checking cloud permissions"
    )]
    dry_run: bool,
    #[arg(
        long,
        conflicts_with = "open_cloud",
        help = "Publish Studio as it is even when Live Sync has file changes pending or a conflict to resolve"
    )]
    allow_pending: bool,
    #[arg(
        long,
        requires = "publish_as",
        help = "With --as, publish while a Play session runs in the selected Studio"
    )]
    allow_play: bool,
    #[command(flatten)]
    bridge: BridgeConnectionArgs,
}

pub(crate) fn run(args: PublishArgs, project: Option<&Path>) -> Result<()> {
    let result = if args.open_cloud {
        open_cloud(&args, project)?
    } else if args.publish_as.is_some() {
        publish_as(&args, project)?
    } else {
        publish_from_studio(&args, project)?
    };
    app::output::print_json_output(&result, false)
}

struct StudioPreflight {
    live_sync: Value,
    place_version: Option<u64>,
}

/// Checks the selected Studio before its open place is published: no Play
/// session, and Live Sync, when running, settled so every file edit is in Studio.
fn studio_preflight(args: &PublishArgs, project: Option<&Path>) -> Result<StudioPreflight> {
    let status = daemon_result(
        op::STUDIO_STATUS,
        project,
        json!({ "all": false }),
        false,
        Some(&args.bridge),
    )?;
    ensure_play_stopped(&status, args.allow_play, args.publish_as.is_some())?;
    let mut live_status = daemon_result(
        op::LIVE_STATUS,
        project,
        json!({ "manageFiles": true, "filesOnly": true, "compact": true }),
        false,
        Some(&args.bridge),
    )?;
    if live_status
        .pointer("/daemon/running")
        .and_then(Value::as_bool)
        == Some(true)
    {
        live_status = daemon_result(
            op::LIVE_STATUS,
            project,
            json!({
                "manageFiles": true,
                "compact": true,
                "settleWaitSeconds": LIVE_SYNC_SETTLE_SECONDS,
            }),
            false,
            Some(&args.bridge),
        )?;
    }
    Ok(StudioPreflight {
        live_sync: live_sync_preflight(&live_status["daemon"], args.allow_pending)?,
        place_version: studio_place_version(&status),
    })
}

fn studio_place_version(status: &Value) -> Option<u64> {
    status["placeVersion"]
        .as_u64()
        .filter(|version| *version > 0)
}

fn ensure_play_stopped(status: &Value, allow_play: bool, publish_as: bool) -> Result<()> {
    let playing = matches!(status["playState"].as_str(), Some("running" | "starting"))
        || status["clients"].as_array().is_some_and(|clients| {
            clients.iter().any(|client| {
                matches!(client["role"].as_str(), Some("play-server" | "play-client"))
            })
        });
    if !playing || allow_play {
        return Ok(());
    }
    if publish_as {
        bail!(
            "A Play session is running in the selected Studio; stop Play first (rbx play -x) or pass --allow-play"
        );
    }
    bail!(
        "A Play session is running in the selected Studio; stop Play first (rbx play -x). Studio publishes its open place only from Edit"
    )
}

fn live_sync_preflight(daemon: &Value, allow_pending: bool) -> Result<Value> {
    if daemon["running"].as_bool() != Some(true) {
        return Ok(json!({ "running": false }));
    }
    let unsettled = live::unsettled_reason(daemon, &Value::Null);
    if let Some(reason) = &unsettled
        && !allow_pending
    {
        bail!(
            "Live Sync is not settled: {reason}. Let it finish (rbx lst --wait) or pass --allow-pending to publish Studio as it is"
        );
    }
    let mut result = json!({
        "pending": daemon["pendingCount"].as_u64().unwrap_or_default(),
        "settled": unsettled.is_none(),
    });
    if let Some(reason) = unsettled {
        result["unsettled"] = json!(reason);
    }
    Ok(result)
}

fn publish_from_studio(args: &PublishArgs, project: Option<&Path>) -> Result<Value> {
    let preflight = studio_preflight(args, project)?;
    let mut result = daemon_result(
        op::PLACE_PUBLISH,
        project,
        json!({ "dryRun": args.dry_run, "allowPending": args.allow_pending }),
        !args.dry_run,
        Some(&args.bridge),
    )?;
    if result["published"] == true
        && result["versionNumber"].is_null()
        && let Some(previous) = preflight.place_version
        && let Some(current) = daemon_result(
            op::STUDIO_STATUS,
            project,
            json!({ "all": false }),
            false,
            Some(&args.bridge),
        )
        .ok()
        .as_ref()
        .and_then(studio_place_version)
        .filter(|current| *current > previous)
    {
        result["versionNumber"] = json!(current);
    }
    result["previousVersion"] = json!(preflight.place_version);
    result["liveSync"] = preflight.live_sync;
    Ok(result)
}

pub(crate) fn studio_result(
    context: &BoundContext,
    parameters: &Value,
    bridge: &BridgeServer,
) -> Result<Value> {
    let runtime = context
        .runtime_id
        .as_deref()
        .context("No bound Edit runtime")?;
    if let Some(expected) = parameters.get("runtimeId").and_then(Value::as_str) {
        ensure!(
            expected == runtime,
            "Studio changed after publish review; review the selected place again"
        );
    }
    let dry_run = parameters
        .get("dryRun")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = bridge
        .call_for_runtime_with_timeout(
            "publishPlace",
            json!({
                "dryRun": dry_run,
                "runtimeId": runtime,
                "gameId": context.game_id,
                "placeId": context.place_id,
            }),
            BridgeTarget::Edit,
            runtime,
            Some(Duration::from_secs(PUBLISH_SECONDS)),
        )
        .map_err(studio_failure)?;
    match crate::app::output::ensure_plugin_api_ok(&result) {
        Ok(()) => Ok(result),
        Err(error) if !dry_run && save_place_api_refused(&error.to_string()) => {
            publish_with_studio_action(context, bridge, runtime, &result)
        }
        Err(error) => Err(error),
    }
}

// AssetService:SavePlaceAsync only works where the Save Place API is enabled.
// Studio's own Publish to Roblox command has no such gate, so run that command
// and read its outcome from the Studio log.
/// Writes the open place of the bound Edit runtime, as Studio serializes it,
/// to the path the caller chose.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn snapshot_result(
    context: &BoundContext,
    parameters: &Value,
    bridge: &BridgeServer,
) -> Result<Value> {
    let runtime = context
        .runtime_id
        .as_deref()
        .context("No bound Edit runtime")?;
    let output = parameters
        .get("output")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context("A snapshot needs an output path")?;
    ensure!(
        output.is_absolute() && output.extension().is_some_and(|ext| ext == "rbxl"),
        "The snapshot output must be an absolute .rbxl path"
    );
    let instances =
        crate::studio::native::editor::write_edit_place_snapshot(bridge, runtime, &output)
            .map_err(studio_failure)?;
    Ok(json!({
        "ok": true,
        "runtimeId": runtime,
        "file": output,
        "instances": instances,
        "gameId": context.game_id,
        "placeId": context.place_id,
    }))
}

fn publish_as(args: &PublishArgs, project: Option<&Path>) -> Result<Value> {
    cloud::keys::select(args.key.clone());
    let target = args.publish_as.context("--as needs a place id")?;
    let identity = cloud::command::discover_identity(project, args.universe, None)?;
    let game_id = identity.game_id.context(
        "Publishing as another place requires its universe; pass --universe ID when it is not the project's experience",
    )?;
    let identity = cloud::CloudIdentity {
        game_id: Some(game_id),
        place_id: Some(target),
    };
    let key_env = args.key_env.as_deref().unwrap_or("ROBLOX_API_KEY");
    if !args.dry_run {
        cloud::CloudAuth::from_env(false, key_env, None, "publish")
            .map_err(cloud::command::cloud_error)?;
    }
    let preflight = studio_preflight(args, project)?;
    let previous_version = cloud::place_history_page(identity, key_env, target)
        .and_then(|history| cloud::place_versions(&history).first().map(|entry| entry.0));
    let file = std::env::temp_dir().join(format!(
        "renium-publish-as-{}-{}.rbxl",
        std::process::id(),
        crate::app::timing::current_millis()
    ));
    let result = (|| -> Result<Value> {
        let snapshot = daemon_result(
            op::PLACE_SNAPSHOT,
            project,
            json!({ "output": file }),
            false,
            Some(&args.bridge),
        )?;
        // Studio serialized this place itself, so unions, appearances and every
        // engine-owned field are intact; only the upload size limit applies.
        let bytes = fs::metadata(&file)
            .with_context(|| format!("Snapshot {} was not written", file.display()))?
            .len();
        validate_size(bytes)?;
        let mut result = json!({
            "ok": true, "published": false, "dryRun": args.dry_run,
            "source": "studio", "sourcePlaceId": snapshot["placeId"],
            "sourceRuntimeId": snapshot["runtimeId"], "instances": snapshot["instances"],
            "gameId": game_id, "placeId": target, "bytes": bytes,
            "versionType": if args.saved { "Saved" } else { "Published" },
            "url": format!("https://www.roblox.com/games/{target}"),
            "previousVersion": previous_version, "liveSync": preflight.live_sync,
        });
        if !args.dry_run {
            let response = upload(
                identity,
                key_env,
                target,
                cloud_request_with_type(&file, !args.saved),
            )?;
            result["versionNumber"] = json!(published_version(&response)?);
            result["published"] = json!(!args.saved);
        }
        Ok(result)
    })();
    let _ = fs::remove_file(&file);
    result
}

fn save_place_api_refused(message: &str) -> bool {
    message.contains("Save Place API") || message.contains("SavePlace")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn publish_with_studio_action(
    _context: &BoundContext,
    _bridge: &BridgeServer,
    _runtime: &str,
    plugin_result: &Value,
) -> Result<Value> {
    bail!(
        "Studio refused SavePlaceAsync ({plugin_result}) and its Publish command cannot be run on this platform"
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn publish_with_studio_action(
    context: &BoundContext,
    bridge: &BridgeServer,
    runtime: &str,
    plugin_result: &Value,
) -> Result<Value> {
    let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
    let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, runtime)?;
    let title = crate::studio::native::serializer::target_name(pid, &info.place_name)?;
    // Only this Studio's own log can report this publish; another open Studio
    // publishing at the same time must not be mistaken for it.
    let log = crate::studio::diagnosis::studio_log_for_process(
        pid,
        crate::studio::diagnosis::studio_process_started_unix(pid),
    )
    .with_context(|| {
        format!(
            "Studio refused SavePlaceAsync and the log of Studio process {pid} could not be found to confirm its Publish command, so it was not run"
        )
    })?;
    let started = SystemTime::now();
    let outcome = crate::studio::native::serializer::trigger_studio_action(
        pid,
        &title,
        STUDIO_PUBLISH_ACTION,
    )
    .with_context(|| {
        format!(
            "Studio refused SavePlaceAsync ({}) and its Publish command could not be run",
            plugin_result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
        )
    })?;
    let published = wait_for_studio_publish(&log, started, STUDIO_PUBLISH_WAIT)?;
    Ok(json!({
        "ok": true,
        "published": true,
        "dryRun": false,
        "source": "studio",
        "method": "studioPublishCommand",
        "runtimeId": runtime,
        "gameId": context.game_id,
        "placeId": context.place_id,
        "window": outcome.window_title,
        "actionMatches": outcome.found,
        "versionNumber": published.version,
        "url": format!("https://www.roblox.com/games/{}", context.place_id.unwrap_or_default()),
    }))
}

#[cfg(any(windows, target_os = "macos", test))]
struct StudioPublishReport {
    version: Option<u64>,
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum StudioPublishEvent {
    Succeeded { version: Option<u64> },
    Failed(String),
}

#[cfg(any(windows, target_os = "macos", test))]
// Studio writes `2026-09-22T13:15:11.403Z,...,Info [FLog::CreatorOutput] Place published.`
// lines; only lines stamped after the command was triggered count.
fn studio_publish_event(line: &str, since: SystemTime) -> Option<StudioPublishEvent> {
    let (stamp, rest) = line.split_once(',')?;
    let stamped = humantime_parse(stamp)?;
    if stamped < since {
        return None;
    }
    if rest.contains("[FLog::PublishSessionStateController] Go to PublishSuccessful")
        || rest.contains("[FLog::CreatorOutput] Place published.")
    {
        return Some(StudioPublishEvent::Succeeded { version: None });
    }
    if let Some(index) = rest.find("Add publish notes to v") {
        let digits = rest[index + "Add publish notes to v".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        return Some(StudioPublishEvent::Succeeded {
            version: digits.parse().ok(),
        });
    }
    if rest.contains("[FLog::PublishSessionStateController] Go to PublishFailed")
        || rest.contains("[FLog::PublishSessionStateController] Go to PublishCanceled")
        || rest.contains("[FLog::PublishSessionStateController] Go to PublishCancelled")
        || rest.contains("PublishPlaceToRobloxIsCanceled")
    {
        return Some(StudioPublishEvent::Failed(rest.trim().to_string()));
    }
    None
}

#[cfg(any(windows, target_os = "macos", test))]
fn humantime_parse(stamp: &str) -> Option<SystemTime> {
    let stamp = stamp.strip_suffix('Z')?;
    let (date, time) = stamp.split_once('T')?;
    let mut date_parts = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (
        date_parts.next()??,
        date_parts.next()??,
        date_parts.next()??,
    );
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let second = time_parts.next()?.parse::<f64>().ok()?;
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60;
    let total = seconds as f64 + second;
    if total < 0.0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs_f64(total))
}

#[cfg(test)]
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(any(windows, target_os = "macos", test))]
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(any(windows, target_os = "macos", test))]
// Windows leaves a log's modified time stale while Studio holds it open, so
// the log is re-read and only the timestamps inside the lines decide.
fn wait_for_studio_publish(
    log: &Path,
    since: SystemTime,
    limit: Duration,
) -> Result<StudioPublishReport> {
    let deadline = Instant::now() + limit;
    let mut version = None;
    loop {
        let mut succeeded = false;
        let bytes = fs::read(log).with_context(|| format!("Could not read {}", log.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines().rev().take(4000) {
            match studio_publish_event(line, since) {
                Some(StudioPublishEvent::Succeeded { version: found }) => {
                    succeeded = true;
                    version = version.or(found);
                }
                Some(StudioPublishEvent::Failed(detail)) => {
                    bail!("Studio reported the publish did not complete: {detail}");
                }
                None => {}
            }
        }
        if succeeded {
            return Ok(StudioPublishReport { version });
        }
        ensure!(
            Instant::now() < deadline,
            "Studio's Publish command was triggered but its log reported no result within {} seconds; check the place's Version History before retrying",
            limit.as_secs()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn studio_failure(error: anyhow::Error) -> anyhow::Error {
    if let Some(application) = error.downcast_ref::<BridgeApplicationError>() {
        if application.message.contains("Unknown method: publishPlace") {
            return anyhow::anyhow!(
                "The selected Studio is running an older Renium plugin; update the plugin and reopen that Studio before publishing"
            );
        }
        return error;
    }
    error.context("Studio publish response was not confirmed. Check the place's Version History before trying again; the request is not automatically repeated")
}

fn selected_project(
    project: Option<&Path>,
    selector: Option<&str>,
) -> Result<Option<config::LoadedProject>> {
    let start = match project {
        Some(path) if path.is_file() => path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let place = experience::resolve_experience_place(&start, selector)?;
    match place {
        Some(place) => {
            let explicit = project
                .filter(|path| path.is_file() && path.parent() == Some(place.root.as_path()));
            config::try_load_project(explicit.or(Some(&place.root)), None)
        }
        None => config::try_load_project(project, None),
    }
}

fn open_cloud(args: &PublishArgs, project: Option<&Path>) -> Result<Value> {
    cloud::keys::select(args.key.clone());
    let loaded = if args.file.is_none() || args.universe.is_none() || args.place_id.is_none() {
        selected_project(project, app::context::place_selector().as_deref())?
    } else {
        None
    };
    if args.file.is_none() && loaded.is_none() {
        bail!("No Renium project found to build; select a place project or pass --file PLACE.rbxl");
    }
    let identity = cloud::command::discover_identity(
        loaded
            .as_ref()
            .map(|loaded| loaded.path.as_path())
            .or(project),
        args.universe,
        args.place_id,
    )?;
    let game_id = identity.game_id.context(
        "Publishing requires a universe ID; configure the experience or pass --universe ID",
    )?;
    let place_id = identity
        .place_id
        .context("Publishing requires a place ID; select --place ALIAS or pass --place-id ID")?;
    if !args.dry_run {
        cloud::CloudAuth::from_env(
            false,
            args.key_env.as_deref().unwrap_or("ROBLOX_API_KEY"),
            None,
            "publish",
        )
        .map_err(cloud::command::cloud_error)?;
    }
    let root = loaded
        .as_ref()
        .map(|loaded| loaded.root.clone())
        .or_else(|| {
            project.map(|path| {
                if path.is_dir() {
                    path.to_path_buf()
                } else {
                    path.parent().unwrap_or(Path::new(".")).to_path_buf()
                }
            })
        })
        .unwrap_or(std::env::current_dir()?);
    super::version_control::ensure_renium_local_state_ignored(&root)?;
    let directory = create_unique_directory(&root.join(".renium"), "publish-")?;
    let result = (|| {
        let (file, source) = if let Some(file) = &args.file {
            let format = RbxPlaceFormat::from_path(file)?;
            validate_size(fs::metadata(file)?.len())?;
            let snapshot = directory.join(format!("place.{}", format.label()));
            fs::copy(file, &snapshot)
                .with_context(|| format!("Could not snapshot {}", file.display()))?;
            (snapshot, absolutize_for_daemon(file))
        } else {
            let loaded = loaded.as_ref().expect("project checked above");
            let output = directory.join("place.rbxl");
            workflows::build_once(
                loaded,
                &workflows::BuildArgs {
                    output: Some(output.clone()),
                    project: Some(loaded.path.clone()),
                    watch: false,
                    sourcemap: false,
                    plugin: false,
                    target: None,
                    wally: workflows::ToolPolicy::Auto,
                    typescript: workflows::ToolPolicy::Auto,
                },
                &output,
                false,
                None,
            )?;
            (output, loaded.path.clone())
        };
        let bytes = validate_file(&file)?;
        let mut result = json!({
            "ok": true, "published": false, "dryRun": args.dry_run,
            "source": "open-cloud", "input": source, "gameId": game_id,
            "placeId": place_id, "bytes": bytes,
            "url": format!("https://www.roblox.com/games/{place_id}"),
        });
        if !args.dry_run {
            let key_env = args.key_env.as_deref().unwrap_or("ROBLOX_API_KEY");
            let response = upload(identity, key_env, place_id, cloud_request(&file))?;
            result["versionNumber"] = json!(published_version(&response)?);
            result["published"] = json!(true);
        }
        Ok(result)
    })();
    let cleanup = fs::remove_dir_all(&directory);
    if let Err(error) = cleanup {
        crate::log_global(
            2,
            format_args!(
                "Could not remove publish staging {}: {error}",
                directory.display()
            ),
        );
    }
    result
}

fn validate_size(bytes: u64) -> Result<()> {
    ensure!(
        bytes > 0 && bytes <= MAX_PLACE_BYTES,
        "Place file must be nonempty and no larger than 100 MiB"
    );
    Ok(())
}

fn validate_file(path: &Path) -> Result<u64> {
    let bytes = fs::metadata(path)?.len();
    validate_size(bytes)?;
    let dom = RbxPlaceFormat::from_path(path)?.read(path)?;
    let database = rbx_reflection_database::get()?;
    let mut unsupported = BTreeMap::<String, usize>::new();
    for instance in dom.descendants() {
        let mut class = instance.class.as_str();
        loop {
            if matches!(
                class,
                "EditableImage"
                    | "EditableMesh"
                    | "PartOperation"
                    | "SurfaceAppearance"
                    | "BaseWrap"
            ) {
                *unsupported.entry(instance.class.to_string()).or_default() += 1;
                break;
            }
            let Some(parent) = database
                .classes
                .get(class)
                .and_then(|descriptor| descriptor.superclass)
            else {
                break;
            };
            class = parent;
        }
    }
    ensure!(
        unsupported.is_empty(),
        "Open Cloud cannot reliably update these instances: {}. Publish from Studio instead",
        unsupported
            .iter()
            .map(|(class, count)| format!("{class} ({count})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(bytes)
}

fn cloud_request(file: &Path) -> Value {
    cloud_request_with_type(file, true)
}

fn cloud_request_with_type(file: &Path, published: bool) -> Value {
    json!({
        "method": "POST", "path": "/universes/v1/{universe}/places/{place}/versions",
        "query": { "versionType": if published { "Published" } else { "Saved" } }, "rawFile": file,
        "contentType": if file.extension().is_some_and(|ext| ext == "rbxlx") { "application/xml" } else { "application/octet-stream" },
        "timeoutSeconds": PUBLISH_SECONDS,
    })
}

fn published_version(response: &Value) -> Result<u64> {
    response.pointer("/body/versionNumber").and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|version| *version > 0)
        .context("Roblox did not return a published version number. Check Version History before retrying")
}

fn upload(
    identity: cloud::CloudIdentity,
    key_env: &str,
    place_id: i64,
    request: Value,
) -> Result<Value> {
    cloud::execute_one(identity, key_env, None, false, request).map_err(|failure| {
        let detail = failure.0.d.clone().unwrap_or_default();
        let error = cloud::command::cloud_error(failure);
        if detail["status"].as_u64() != Some(409) {
            return error.context("Publish was not confirmed. Check Version History before retrying; the upload is not automatically repeated");
        }
        let report = busy_upload_report(
            cloud::team_create_members(identity, key_env, place_id).as_ref(),
            cloud::place_history_page(identity, key_env, place_id).as_ref(),
        );
        busy_upload_error(&error.to_string(), &detail["body"], report)
    })
}

// Roblox answers 409 "Server is busy" while the destination is open in a Team
// Create session; who is in it and which saves are unpublished says what blocks it.
fn busy_upload_report(members: Option<&Value>, history: Option<&Value>) -> Option<Value> {
    if members.is_none() && history.is_none() {
        return None;
    }
    let members = members.map(team_members);
    let newer_saves = history.map(|history| {
        cloud::place_versions(history)
            .into_iter()
            .take_while(|(_, published)| !published)
            .take(10)
            .map(|(version, _)| json!({ "version": version, "published": false }))
            .collect::<Vec<_>>()
    });
    let names = members
        .iter()
        .flatten()
        .filter_map(|member| member["name"].as_str())
        .collect::<Vec<_>>();
    let (code, hint) = if members.as_ref().is_some_and(|members| !members.is_empty()) {
        let who = match names.as_slice() {
            [] => "Someone has".to_string(),
            [one] => format!("{one} has"),
            [rest @ .., last] => format!("{} and {last} have", rest.join(", ")),
        };
        (
            "team_create_active",
            format!(
                "{who} this place open in Team Create, and Roblox refuses uploads while that session is open. Ask them to close it or publish from that Studio, then publish again"
            ),
        )
    } else if let Some(version) = newer_saves.iter().flatten().next() {
        (
            "unpublished_newer_save",
            format!(
                "Version {} was saved after the last published version and is not published, which happens while a Team Create session edits this place. Publish from that session or close it, then publish again",
                version["version"]
            ),
        )
    } else {
        (
            "place_busy",
            "Roblox reported the place busy; a Team Create session open on it causes this. Close that session, then publish again".to_string(),
        )
    };
    let mut report = json!({ "code": code, "hint": hint });
    if let Some(members) = members {
        report["members"] = json!(members);
    }
    if let Some(newer_saves) = newer_saves {
        report["newerSaves"] = json!(newer_saves);
    }
    Some(report)
}

fn team_members(body: &Value) -> Vec<Value> {
    body.get("data")
        .unwrap_or(body)
        .as_array()
        .into_iter()
        .flatten()
        .map(|member| {
            let mut entry = json!({});
            if let Some(id) = member["id"].as_i64().or_else(|| member["userId"].as_i64()) {
                entry["id"] = json!(id);
            }
            if let Some(name) = ["name", "username", "displayName"]
                .iter()
                .find_map(|key| member[key].as_str())
            {
                entry["name"] = json!(name);
            }
            entry
        })
        .collect()
}

fn busy_upload_error(original: &str, body: &Value, report: Option<Value>) -> anyhow::Error {
    let Some(report) = report else {
        return anyhow::anyhow!(
            "{original}\nA Team Create session open on the destination place causes this; close it, then publish again"
        );
    };
    let message = body
        .as_str()
        .or_else(|| body["message"].as_str())
        .or_else(|| body.pointer("/errors/0/message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or("the place is busy");
    anyhow::anyhow!("Roblox refused the upload with HTTP 409 ({message}): {report}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    fn args(arguments: &[&str]) -> PublishArgs {
        let command = crate::cli::Cli::try_parse_from(arguments).unwrap().command;
        let crate::cli::Commands::Publish(args) = command else {
            panic!("publish not parsed")
        };
        args
    }

    fn write_place(path: &Path, classes: &[&str]) {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let service = dom.insert(dom.root_ref(), InstanceBuilder::new("Workspace"));
        for class in classes {
            dom.insert(service, InstanceBuilder::new(*class));
        }
        let file = fs::File::create(path).unwrap();
        if path.extension().is_some_and(|ext| ext == "rbxlx") {
            rbx_xml::to_writer_default(file, &dom, &[service]).unwrap();
        } else {
            rbx_binary::to_writer(file, &dom, &[service]).unwrap();
        }
    }

    #[test]
    fn studio_publish_watcher_reads_logs_studio_still_holds_open() {
        let directory =
            create_unique_directory(&std::env::temp_dir(), "renium-publish-log-").unwrap();
        let since = SystemTime::now() - Duration::from_secs(5);
        let stamp = |offset: i64| {
            let seconds = since
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + offset;
            let days = seconds.div_euclid(86_400);
            let rest = seconds.rem_euclid(86_400);
            let (year, month, day) = civil_from_days(days);
            format!(
                "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
                rest / 3600,
                rest % 3600 / 60,
                rest % 60
            )
        };
        let log = directory.join("0.739.0.7390687_20260922T130705Z_Studio_35E2B_last.log");
        let other = directory.join("0.739.0.7390687_20260922T130800Z_Studio_9A1F0_last.log");
        std::fs::write(
            &log,
            format!(
                "{},1.0,a8fc,6,Debug [FLog::PublishSessionStateController] Go to PublishSuccessful\n{},2.0,0b4c,6,Info [FLog::CreatorOutput] Add publish notes to v2746\n",
                stamp(1),
                stamp(2)
            ),
        )
        .unwrap();
        std::fs::write(
            &other,
            format!(
                "{},1.0,a8fc,6,Debug [FLog::PublishSessionStateController] Go to PublishFailed\n",
                stamp(1)
            ),
        )
        .unwrap();
        let report = wait_for_studio_publish(&log, since, Duration::from_secs(5)).unwrap();
        assert_eq!(report.version, Some(2746));
        std::fs::write(
            &log,
            format!(
                "{},3.0,a8fc,6,Debug [FLog::PublishSessionStateController] Go to PublishFailed\n",
                stamp(3)
            ),
        )
        .unwrap();
        std::fs::write(
            &other,
            format!(
                "{},4.0,0b4c,6,Info [FLog::CreatorOutput] Add publish notes to v900001\n",
                stamp(4)
            ),
        )
        .unwrap();
        let failed = wait_for_studio_publish(&log, since, Duration::from_secs(2));
        assert!(failed.is_err());
        std::fs::write(&log, "").unwrap();
        let silent = wait_for_studio_publish(&log, since, Duration::from_secs(1));
        assert!(
            silent
                .err()
                .is_some_and(|error| error.to_string().contains("reported no result"))
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn studio_publish_log_lines_report_success_failure_and_version() {
        let since = humantime_parse("2026-09-22T13:15:00.000Z").unwrap();
        assert_eq!(
            studio_publish_event(
                "2026-09-22T13:15:11.403Z,486.403290,a8fc,6,Debug [FLog::PublishSessionStateController] Go to PublishSuccessful",
                since
            ),
            Some(StudioPublishEvent::Succeeded { version: None })
        );
        assert_eq!(
            studio_publish_event(
                "2026-09-22T13:15:11.414Z,486.414337,0b4c,6,Info [FLog::CreatorOutput] \u{2192} Add publish notes to v2745",
                since
            ),
            Some(StudioPublishEvent::Succeeded {
                version: Some(2745)
            })
        );
        assert_eq!(
            studio_publish_event(
                "2026-09-22T13:14:59.000Z,480.0,a8fc,6,Debug [FLog::PublishSessionStateController] Go to PublishSuccessful",
                since
            ),
            None
        );
        assert!(matches!(
            studio_publish_event(
                "2026-09-22T13:16:00.000Z,500.0,a8fc,6,Debug [FLog::PublishSessionStateController] Go to PublishFailed",
                since
            ),
            Some(StudioPublishEvent::Failed(_))
        ));
        assert!(save_place_api_refused(
            "Studio publish failed: Game:SavePlace can only be called from a server script. Save Place API must be enabled for this place"
        ));
        assert!(!save_place_api_refused("connection closed"));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn publish_defaults_to_studio_and_requires_explicit_cloud_options() {
        assert!(!args(&["rbx", "publish"]).open_cloud);
        let publish_as = args(&[
            "rbx",
            "publish",
            "--as",
            "112966546347918",
            "--universe",
            "8420907710",
            "--key",
            "live",
        ]);
        assert_eq!(publish_as.publish_as, Some(112966546347918));
        assert_eq!(publish_as.universe, Some(8420907710));
        assert_eq!(publish_as.key.as_deref(), Some("live"));
        assert!(!publish_as.saved);
        assert!(
            crate::cli::Cli::try_parse_from(["rbx", "publish", "--as", "5", "--open-cloud"])
                .is_err()
        );
        assert!(crate::cli::Cli::try_parse_from(["rbx", "publish", "--saved"]).is_err());
        assert!(args(&["rbx", "publish", "--allow-pending"]).allow_pending);
        assert!(
            args(&[
                "rbx",
                "publish",
                "--as",
                "5",
                "--allow-play",
                "--allow-pending"
            ])
            .allow_play
        );
        assert!(crate::cli::Cli::try_parse_from(["rbx", "publish", "--allow-play"]).is_err());
        assert!(
            crate::cli::Cli::try_parse_from(["rbx", "publish", "--open-cloud", "--allow-pending"])
                .is_err()
        );
        assert_eq!(
            cloud_request_with_type(Path::new("a.rbxl"), false)["query"]["versionType"],
            "Saved"
        );
        assert!(args(&["rbx", "publish", "--dry-run"]).dry_run);
        assert!(args(&["rbx", "publish", "--open-cloud"]).open_cloud);
        for options in [
            vec!["--file", "place.rbxl"],
            vec!["--universe", "123"],
            vec!["--key-env", "KEY"],
            vec!["--place-id", "456"],
        ] {
            let mut input = vec!["rbx", "publish"];
            input.extend(options);
            assert!(crate::cli::Cli::try_parse_from(input).is_err());
        }
        assert!(
            crate::cli::Cli::try_parse_from(["rbx", "publish", "--open-cloud", "--place-id", "0"])
                .is_err()
        );
    }

    #[test]
    fn studio_preflight_blocks_play_and_unsettled_live_sync_unless_allowed() {
        let stopped = json!({"playState": "stopped", "clients": [{"role": "edit"}]});
        assert!(ensure_play_stopped(&stopped, false, false).is_ok());
        for status in [
            json!({"playState": "running"}),
            json!({"playState": "starting"}),
            json!({"playState": "unknown", "clients": [{"role": "edit"}, {"role": "play-server"}]}),
        ] {
            let studio = ensure_play_stopped(&status, false, false)
                .unwrap_err()
                .to_string();
            assert!(
                studio.contains("stop Play first (rbx play -x)")
                    && !studio.contains("--allow-play"),
                "{studio}"
            );
            let publish_as = ensure_play_stopped(&status, false, true)
                .unwrap_err()
                .to_string();
            assert!(
                publish_as.ends_with("stop Play first (rbx play -x) or pass --allow-play"),
                "{publish_as}"
            );
            assert!(ensure_play_stopped(&status, true, true).is_ok());
        }
        assert_eq!(
            live_sync_preflight(&json!({"running": false}), false).unwrap(),
            json!({"running": false})
        );
        assert_eq!(
            live_sync_preflight(
                &json!({"running": true, "settled": true, "pendingCount": 0}),
                false
            )
            .unwrap(),
            json!({"pending": 0, "settled": true})
        );
        let pending = json!({
            "running": true, "settled": false, "pendingCount": 2,
            "pendingPaths": ["src/A.luau", "src/B.luau"],
        });
        let error = live_sync_preflight(&pending, false)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("2 file changes not yet in Studio: src/A.luau, src/B.luau")
                && error.contains("--allow-pending"),
            "{error}"
        );
        let allowed = live_sync_preflight(&pending, true).unwrap();
        assert_eq!(allowed["pending"], 2);
        assert_eq!(allowed["settled"], false);
        assert!(
            live_sync_preflight(
                &json!({"running": true, "resolutionRequired": true, "error": "Size differs"}),
                false
            )
            .unwrap_err()
            .to_string()
            .contains("conflict needs resolution (Size differs)")
        );
        assert_eq!(studio_place_version(&json!({"placeVersion": 0})), None);
        assert_eq!(
            studio_place_version(&json!({"placeVersion": 2848})),
            Some(2848)
        );
    }

    #[test]
    fn busy_upload_names_team_create_members_and_unpublished_saves() {
        let members = json!({
            "previousPageCursor": null, "nextPageCursor": null,
            "data": [
                {"id": 1, "name": "Builder", "displayName": "B"},
                {"userId": 2, "displayName": "Scripter"},
            ],
        });
        let history = json!({
            "placeVersions": [
                {"version": 2849, "isPublished": false},
                {"version": 2848, "isPublished": true},
                {"version": 2847, "isPublished": false},
            ],
            "hasMore": true,
        });
        let report = busy_upload_report(Some(&members), Some(&history)).unwrap();
        assert_eq!(report["code"], "team_create_active");
        assert_eq!(
            report["members"],
            json!([{"id": 1, "name": "Builder"}, {"id": 2, "name": "Scripter"}])
        );
        assert_eq!(
            report["newerSaves"],
            json!([{"version": 2849, "published": false}])
        );
        assert!(
            report["hint"]
                .as_str()
                .unwrap()
                .starts_with("Builder and Scripter have this place open in Team Create")
        );
        let saves_only = busy_upload_report(None, Some(&history)).unwrap();
        assert_eq!(saves_only["code"], "unpublished_newer_save");
        assert!(saves_only.get("members").is_none());
        assert!(
            saves_only["hint"]
                .as_str()
                .unwrap()
                .starts_with("Version 2849 was saved after the last published version")
        );
        let quiet = busy_upload_report(
            Some(&json!({"data": []})),
            Some(&json!({"placeVersions": [{"version": 7, "isPublished": true}]})),
        )
        .unwrap();
        assert_eq!(quiet["code"], "place_busy");
        assert_eq!(quiet["members"], json!([]));
        assert_eq!(quiet["newerSaves"], json!([]));
        assert_eq!(busy_upload_report(None, None), None);

        let structured = busy_upload_error(
            "Open Cloud request 0 returned HTTP 409",
            &json!({"message": "Server is busy and unable to process your upload request"}),
            Some(report),
        )
        .to_string();
        assert!(
            structured.starts_with(
                "Roblox refused the upload with HTTP 409 (Server is busy and unable to process your upload request): {"
            ),
            "{structured}"
        );
        assert!(structured.contains(r#""code":"team_create_active""#));
        let fallback = busy_upload_error(
            "Open Cloud request 0 returned HTTP 409\n{\"status\":409}",
            &Value::Null,
            None,
        )
        .to_string();
        assert!(
            fallback.starts_with(
                "Open Cloud request 0 returned HTTP 409\n{\"status\":409}\nA Team Create session open on the destination place causes this"
            ),
            "{fallback}"
        );
    }

    #[test]
    fn publish_upload_shape_and_completion_are_explicit() {
        for (extension, mime) in [
            ("rbxl", "application/octet-stream"),
            ("rbxlx", "application/xml"),
        ] {
            let file = PathBuf::from(format!("place.{extension}"));
            let request = cloud_request(&file);
            assert_eq!(request["method"], "POST");
            assert_eq!(request["query"]["versionType"], "Published");
            assert_eq!(request["rawFile"], json!(file));
            assert_eq!(request["contentType"], mime);
        }
        assert_eq!(
            published_version(&json!({"body":{"versionNumber":12}})).unwrap(),
            12
        );
        assert_eq!(
            published_version(&json!({"body":{"versionNumber":"13"}})).unwrap(),
            13
        );
        for response in [
            json!({}),
            json!({"body":{"versionNumber":0}}),
            json!({"body":{"versionNumber":-1}}),
        ] {
            assert!(published_version(&response).is_err());
        }
        assert!(validate_size(0).is_err());
        assert!(validate_size(MAX_PLACE_BYTES + 1).is_err());
        assert!(validate_size(MAX_PLACE_BYTES).is_ok());
        let rejected = studio_failure(
            BridgeApplicationError {
                method: "publishPlace".into(),
                message: "This Studio place is unpublished".into(),
            }
            .into(),
        );
        assert!(!rejected.to_string().contains("Version History"));
        let old = studio_failure(
            BridgeApplicationError {
                method: "publishPlace".into(),
                message: "Unknown method: publishPlace".into(),
            }
            .into(),
        );
        assert!(old.to_string().contains("reopen"));
        assert!(
            studio_failure(anyhow::anyhow!("connection closed"))
                .to_string()
                .contains("Version History")
        );
    }

    #[test]
    fn publish_cloud_validates_binary_xml_and_unsupported_subclasses() {
        let root = crate::tests::support::temp_dir("publish-fidelity");
        for extension in ["rbxl", "rbxlx"] {
            let file = root.join(format!("place.{extension}"));
            write_place(&file, &["Part"]);
            assert!(validate_file(&file).is_ok());
            for class in [
                "SurfaceAppearance",
                "UnionOperation",
                "WrapLayer",
                "WrapTarget",
            ] {
                write_place(&file, &[class]);
                let error = validate_file(&file).unwrap_err().to_string();
                assert!(error.contains(class), "{error}");
            }
            fs::write(&file, "not a place").unwrap();
            assert!(validate_file(&file).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_cloud_dry_run_needs_no_credentials_and_removes_staging() {
        let root = crate::tests::support::temp_dir("publish-dry-run");
        let project = root.join("renium.project.jsonc");
        fs::write(
            &project,
            r#"{"schemaVersion":1,"name":"Publish test","sourceRoot":"src"}"#,
        )
        .unwrap();
        fs::create_dir(root.join("src")).unwrap();
        crate::bytecode::ensure_service_store_exists(
            &root.join("instances/Workspace.renium"),
            "Workspace",
        )
        .unwrap();
        let mut options = args(&[
            "rbx",
            "publish",
            "--open-cloud",
            "--dry-run",
            "--universe",
            "123",
            "--place-id",
            "456",
            "--key-env",
            "RENIUM_PUBLISH_TEST_UNSET",
        ]);
        let result = open_cloud(&options, Some(&project)).unwrap();
        assert_eq!(result["published"], false);
        assert_eq!(result["placeId"], 456);
        let file = root.join("place.rbxl");
        write_place(&file, &["Part"]);
        options.file = Some(file.clone());
        assert!(
            !open_cloud(&options, Some(&project)).unwrap()["published"]
                .as_bool()
                .unwrap()
        );
        write_place(&file, &["UnionOperation"]);
        assert!(
            open_cloud(&options, Some(&project))
                .unwrap_err()
                .to_string()
                .contains("UnionOperation")
        );
        assert!(!fs::read_dir(root.join(".renium")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("publish-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_selects_one_place_and_never_builds_the_experience_root() {
        let root = crate::tests::support::temp_dir("publish-places");
        fs::write(root.join("renium.experience.json"), r#"{"gameId":123,"places":{"lobby":{"placeId":456,"root":"places/lobby"},"race":{"placeId":789,"root":"places/race"}}}"#).unwrap();
        for name in ["lobby", "race"] {
            let place = root.join("places").join(name);
            fs::create_dir_all(place.join("src")).unwrap();
            fs::write(
                place.join("renium.project.jsonc"),
                format!(r#"{{"schemaVersion":1,"name":"{name}","sourceRoot":"src"}}"#),
            )
            .unwrap();
        }
        assert!(selected_project(Some(&root), None).is_err());
        for selector in ["race", "789", "123:789"] {
            let project = selected_project(Some(&root), Some(selector))
                .unwrap()
                .unwrap();
            assert_eq!(project.project.name.as_deref(), Some("race"));
        }
        assert!(selected_project(Some(&root), Some("unknown")).is_err());
        let race = root.join("places/race");
        assert_eq!(
            selected_project(Some(&race), None)
                .unwrap()
                .unwrap()
                .project
                .name
                .as_deref(),
            Some("race")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
