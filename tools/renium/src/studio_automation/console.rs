use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::wait_for_player_bridge;
use crate::bridge_server::{BridgeServer, BridgeTarget};
use crate::command_line::PluginConsoleOutputArgs;
use crate::daemon_control::{get_console_output_daemon_args, try_daemon_control_request};
use crate::output::ensure_plugin_api_ok;
use crate::snapshot_export::{is_transient_bridge_error, parse_bridge_ports};

pub(crate) fn get_console_output_command(args: PluginConsoleOutputArgs) -> Result<()> {
    if args.follow {
        if follow_console_via_daemon(&args)? {
            return Ok(());
        }
    } else if let Some(result) =
        try_daemon_control_request("co", get_console_output_daemon_args(&args))?
    {
        println!(
            "{}",
            serde_json::to_string_pretty(&filtered_console_result(&args, result))?
        );
        return Ok(());
    }
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, _listen_metrics) =
        BridgeServer::listen(&args.bridge.host, &ports, args.bridge.wait_seconds)?;
    get_console_output_with_warm_bridge(args, &bridge)
}

fn get_console_output_with_warm_bridge(
    args: PluginConsoleOutputArgs,
    bridge: &BridgeServer,
) -> Result<()> {
    if args.follow {
        return follow_console_with_bridge(&args, bridge);
    }
    let result = get_console_output_result(&args, bridge)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&filtered_console_result(&args, result))?
    );
    Ok(())
}

pub(crate) fn get_console_output_result(
    args: &PluginConsoleOutputArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let client = args.client || args.player.is_some();
    let target = if client {
        BridgeTarget::Client
    } else {
        BridgeTarget::Main
    };
    if let Some(player) = args.player.as_deref() {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let result = bridge.call_for_selector(
        "getConsoleOutput",
        json!({
            "limit": args.limit,
            "sinceSeq": args.since_seq,
            "fromOldest": args.from_oldest,
            "clear": args.clear,
        }),
        target,
        args.player.as_deref(),
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

fn update_console_follow_epoch(
    result: &Value,
    epoch: &mut Option<String>,
    since_seq: &mut u64,
    from_oldest: &mut bool,
) -> bool {
    let next_epoch = result.get("epoch").and_then(Value::as_str);
    let changed = epoch.is_some() && epoch.as_deref() != next_epoch;
    if changed {
        *since_seq = 0;
        *from_oldest = true;
    }
    if changed || epoch.is_none() {
        *epoch = next_epoch.map(str::to_string);
    }
    changed
}

fn follow_console_via_daemon(args: &PluginConsoleOutputArgs) -> Result<bool> {
    let mut since_seq = args.since_seq;
    let mut from_oldest = false;
    let mut connected = false;
    let mut epoch = None;
    loop {
        let mut request = get_console_output_daemon_args(args);
        if let Some(position) = request.iter().position(|arg| arg == "-s")
            && let Some(value) = request.get_mut(position + 1)
        {
            *value = since_seq.to_string();
        }
        if connected && let Some(position) = request.iter().position(|arg| arg == "-c") {
            request.remove(position);
        }
        if from_oldest {
            request.push("--from-oldest".to_string());
        }
        let result = match try_daemon_control_request("co", request) {
            Ok(Some(result)) => result,
            Ok(None) if connected => {
                thread::sleep(Duration::from_millis(args.interval_ms.clamp(25, 10_000)));
                continue;
            }
            Ok(None) => return Ok(false),
            Err(error) if connected && is_transient_console_follow_error(&error) => {
                thread::sleep(Duration::from_millis(args.interval_ms.clamp(25, 10_000)));
                continue;
            }
            Err(error) => return Err(error),
        };
        connected = true;
        if update_console_follow_epoch(&result, &mut epoch, &mut since_seq, &mut from_oldest) {
            continue;
        }
        print_followed_console_entries(args, &result)?;
        from_oldest = false;
        since_seq = result
            .get("nextSeq")
            .and_then(Value::as_u64)
            .unwrap_or(since_seq);
        if result.get("hasMore").and_then(Value::as_bool) != Some(true) {
            thread::sleep(Duration::from_millis(args.interval_ms.clamp(25, 10_000)));
        }
    }
}

fn follow_console_with_bridge(args: &PluginConsoleOutputArgs, bridge: &BridgeServer) -> Result<()> {
    let mut since_seq = args.since_seq;
    let mut from_oldest = false;
    let mut epoch = None;
    let mut clear_pending = args.clear;
    loop {
        let request = PluginConsoleOutputArgs {
            bridge: args.bridge.clone(),
            limit: args.limit,
            since_seq,
            from_oldest: args.from_oldest || from_oldest,
            clear: clear_pending,
            client: args.client,
            player: args.player.clone(),
            follow: false,
            grep: args.grep.clone(),
            level: args.level.clone(),
            interval_ms: args.interval_ms,
        };
        let result = match get_console_output_result(&request, bridge) {
            Ok(result) => result,
            Err(error) if is_transient_console_follow_error(&error) => {
                thread::sleep(Duration::from_millis(args.interval_ms.clamp(25, 10_000)));
                continue;
            }
            Err(error) => return Err(error),
        };
        clear_pending = false;
        if update_console_follow_epoch(&result, &mut epoch, &mut since_seq, &mut from_oldest) {
            continue;
        }
        print_followed_console_entries(args, &result)?;
        from_oldest = false;
        since_seq = result
            .get("nextSeq")
            .and_then(Value::as_u64)
            .unwrap_or(since_seq);
        if result.get("hasMore").and_then(Value::as_bool) != Some(true) {
            thread::sleep(Duration::from_millis(args.interval_ms.clamp(25, 10_000)));
        }
    }
}

fn is_transient_console_follow_error(error: &anyhow::Error) -> bool {
    if is_transient_bridge_error(error) {
        return true;
    }
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
        .to_ascii_lowercase();
    [
        "connection reset",
        "connection refused",
        "broken pipe",
        "unexpected eof",
        "timed out",
        "daemon control endpoint is unavailable",
        "closed the control connection",
        "did not finish co before the control timeout",
    ]
    .iter()
    .any(|needle| message.contains(needle))
        && ![
            "multiple studio",
            "match this command",
            "player selector",
            "no player matched",
            "ambiguous",
        ]
        .iter()
        .any(|needle| message.contains(needle))
}

fn filtered_console_result(args: &PluginConsoleOutputArgs, mut result: Value) -> Value {
    let filtered_count =
        if let Some(entries) = result.get_mut("entries").and_then(Value::as_array_mut) {
            entries.retain(|entry| console_entry_matches(args, entry));
            entries.len()
        } else {
            return result;
        };
    if let Some(object) = result.as_object_mut() {
        object.insert("filteredCount".to_string(), json!(filtered_count));
    }
    result
}

fn print_followed_console_entries(args: &PluginConsoleOutputArgs, result: &Value) -> Result<()> {
    if result.get("truncated").and_then(Value::as_bool) == Some(true) {
        eprintln!(
            "[renium] console history was truncated; continuing from the oldest retained line"
        );
    }
    if let Some(entries) = result.get("entries").and_then(Value::as_array) {
        for entry in entries
            .iter()
            .filter(|entry| console_entry_matches(args, entry))
        {
            let level = entry
                .get("type")
                .or_else(|| entry.get("level"))
                .and_then(Value::as_str)
                .unwrap_or("output");
            let message = entry
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            println!("[{level}] {message}");
        }
    }
    io::stdout().flush()?;
    Ok(())
}

fn console_entry_matches(args: &PluginConsoleOutputArgs, entry: &Value) -> bool {
    if let Some(level) = args.level.as_deref() {
        let entry_level = entry
            .get("type")
            .or_else(|| entry.get("level"))
            .and_then(Value::as_str)
            .unwrap_or("output");
        if !entry_level.eq_ignore_ascii_case(level) {
            return false;
        }
    }
    if let Some(needle) = args.grep.as_deref() {
        let message = entry
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !message
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
        {
            return false;
        }
    }
    true
}
