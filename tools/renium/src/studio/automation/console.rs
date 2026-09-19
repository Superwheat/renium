use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{console_entry_level, wait_for_player_bridge};
use crate::app::output::{ensure_plugin_api_ok, print_json_output};
use crate::automation::op;
use crate::cli::PluginConsoleOutputArgs;
use crate::daemon::{daemon_control_request, try_daemon_control_request};
use crate::snapshot::export::is_transient_bridge_error;
use crate::studio::bridge::{BridgeServer, BridgeTarget};

pub(crate) fn get_console_output_command(args: PluginConsoleOutputArgs) -> Result<()> {
    if args.follow {
        return follow_console_via_daemon(&args);
    }
    let mut result = daemon_control_request(
        op::CONSOLE,
        None,
        console_daemon_parameters(&args, args.since_seq, args.clear, args.from_oldest),
        false,
    )?;
    if let Some(map) = result.as_object_mut() {
        map.remove("count");
    }
    crate::app::output::drop_false(&mut result, &["hasMore", "truncated"]);
    compact_console_entries(&mut result);
    print_json_output(&result, false)
}

/// Studio's MessageType names and a wall-clock stamp repeat on every line;
/// the level word is what a reader filters on and `time` already orders them.
fn compact_console_entries(result: &mut Value) {
    if crate::app::output::global_json_output() {
        return;
    }
    let Some(entries) = result.get_mut("entries").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in entries {
        let level = short_console_level(console_entry_level(entry)).to_string();
        if let Some(map) = entry.as_object_mut() {
            map.remove("unix");
            map.insert("type".to_string(), Value::String(level));
        }
    }
}

/// Studio's MessageType names shortened to the level words `rbx l` prints.
fn short_console_level(raw: &str) -> &str {
    match raw {
        "MessageOutput" => "print",
        "MessageInfo" => "info",
        "MessageWarning" => "warn",
        "MessageError" => "error",
        other => other,
    }
}

fn console_daemon_parameters(
    args: &PluginConsoleOutputArgs,
    since_seq: u64,
    clear: bool,
    from_oldest: bool,
) -> Value {
    json!({
        "limit": args.limit,
        "sinceSeq": since_seq,
        "fromOldest": from_oldest,
        "clear": clear,
        "client": args.client,
        "server": args.server,
        "player": args.player,
        "grep": args.grep,
        "level": args.level,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    })
}

pub(crate) fn get_console_output_result(
    args: &PluginConsoleOutputArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let client = !args.server && (args.client || args.player.is_some());
    let target = BridgeTarget::main_or_client(client);
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
    Ok(filtered_console_result(args, result))
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

fn follow_console_via_daemon(args: &PluginConsoleOutputArgs) -> Result<()> {
    let mut since_seq = args.since_seq;
    let mut from_oldest = false;
    let mut connected = false;
    let mut epoch = None;
    loop {
        let parameters = console_daemon_parameters(
            args,
            since_seq,
            args.clear && !connected,
            args.from_oldest || from_oldest,
        );
        let result = match try_daemon_control_request(op::CONSOLE, parameters) {
            Ok(Some(result)) => result,
            Ok(None) if connected => {
                thread::sleep(console_follow_interval(args));
                continue;
            }
            Ok(None) => bail!("Renium daemon did not accept the command"),
            Err(error) if connected && is_transient_console_follow_error(&error) => {
                thread::sleep(console_follow_interval(args));
                continue;
            }
            Err(error) => return Err(error),
        };
        connected = true;
        handle_console_follow_result(args, &result, &mut epoch, &mut since_seq, &mut from_oldest)?;
    }
}

fn console_follow_interval(args: &PluginConsoleOutputArgs) -> Duration {
    Duration::from_millis(args.interval_ms.clamp(25, 10_000))
}

fn handle_console_follow_result(
    args: &PluginConsoleOutputArgs,
    result: &Value,
    epoch: &mut Option<String>,
    since_seq: &mut u64,
    from_oldest: &mut bool,
) -> Result<()> {
    if update_console_follow_epoch(result, epoch, since_seq, from_oldest) {
        return Ok(());
    }
    print_followed_console_entries(args, result)?;
    *from_oldest = false;
    *since_seq = result
        .get("nextSeq")
        .and_then(Value::as_u64)
        .unwrap_or(*since_seq);
    if result.get("hasMore").and_then(Value::as_bool) != Some(true) {
        thread::sleep(console_follow_interval(args));
    }
    Ok(())
}

fn is_transient_console_follow_error(error: &anyhow::Error) -> bool {
    if is_transient_bridge_error(error) {
        return true;
    }
    let message = error
        .chain()
        .map(std::string::ToString::to_string)
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
        object.insert("count".to_string(), json!(filtered_count));
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
            let level = console_entry_level(entry);
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
        if !entry_level.eq_ignore_ascii_case(level)
            && !short_console_level(entry_level).eq_ignore_ascii_case(level)
        {
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
