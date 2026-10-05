use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
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
    let target = if args.server {
        BridgeTarget::Server
    } else {
        BridgeTarget::main_or_client(args.client || args.player.is_some())
    };
    if let Some(player) = args.player.as_deref() {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    // A filter reads the whole retained buffer and applies the limit to the
    // matches; otherwise the limit would be spent on lines the filter drops.
    let filtering = args.grep.is_some() || args.level.is_some();
    let result = bridge.call_for_selector(
        "getConsoleOutput",
        json!({
            "limit": if filtering { CONSOLE_FILTER_SCAN_LIMIT } else { args.limit },
            "sinceSeq": args.since_seq,
            "fromOldest": args.from_oldest,
            "clear": args.clear,
        }),
        target,
        args.player.as_deref(),
    )?;
    ensure_plugin_api_ok(&result)?;
    filtered_console_result(args, result)
}

const CONSOLE_FILTER_SCAN_LIMIT: usize = 1000;

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

fn filtered_console_result(args: &PluginConsoleOutputArgs, mut result: Value) -> Result<Value> {
    let matcher = ConsoleMatcher::new(args)?;
    let Some(entries) = result.get_mut("entries").and_then(Value::as_array_mut) else {
        return Ok(result);
    };
    let scanned = entries.len();
    entries.retain(|entry| matcher.matches(entry));
    let matched = entries.len();
    if matched > args.limit {
        entries.drain(..matched - args.limit);
    }
    let folded = fold_repeated_entries(entries);
    let count = entries.len();
    if let Some(object) = result.as_object_mut() {
        object.insert("count".to_string(), json!(count));
        if matcher.active() {
            object.insert("scanned".to_string(), json!(scanned));
            object.insert("matched".to_string(), json!(matched));
        }
        if folded > 0 {
            object.insert("folded".to_string(), json!(folded));
        }
    }
    Ok(result)
}

/// Consecutive identical messages collapse into one entry with `repeat`.
fn fold_repeated_entries(entries: &mut Vec<Value>) -> usize {
    let mut folded = Vec::with_capacity(entries.len());
    let mut removed = 0;
    for entry in entries.drain(..) {
        let same = folded.last().is_some_and(|previous: &Value| {
            previous.get("message") == entry.get("message")
                && previous.get("type") == entry.get("type")
        });
        if same {
            let last = folded.last_mut().unwrap();
            let repeat = last.get("repeat").and_then(Value::as_u64).unwrap_or(1) + 1;
            last["repeat"] = json!(repeat);
            last["seq"] = entry.get("seq").cloned().unwrap_or(Value::Null);
            removed += 1;
        } else {
            folded.push(entry);
        }
    }
    *entries = folded;
    removed
}

struct ConsoleMatcher {
    level: Option<String>,
    pattern: Option<regex::Regex>,
}

impl ConsoleMatcher {
    fn new(args: &PluginConsoleOutputArgs) -> Result<Self> {
        let pattern = match args.grep.as_deref() {
            None => None,
            Some(text) if args.fixed => Some(
                regex::RegexBuilder::new(&regex::escape(text))
                    .case_insensitive(true)
                    .build()?,
            ),
            Some(text) => Some(
                regex::RegexBuilder::new(text)
                    .case_insensitive(true)
                    .build()
                    .with_context(|| {
                        format!("--grep takes a regex; pass -F for the plain text '{text}'")
                    })?,
            ),
        };
        Ok(Self {
            level: args.level.clone(),
            pattern,
        })
    }

    fn active(&self) -> bool {
        self.level.is_some() || self.pattern.is_some()
    }

    fn matches(&self, entry: &Value) -> bool {
        if let Some(level) = self.level.as_deref() {
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
        if let Some(pattern) = &self.pattern {
            let message = entry
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !pattern.is_match(message) {
                return false;
            }
        }
        true
    }
}

fn print_followed_console_entries(args: &PluginConsoleOutputArgs, result: &Value) -> Result<()> {
    if result.get("truncated").and_then(Value::as_bool) == Some(true) {
        eprintln!(
            "[renium] console history was truncated; continuing from the oldest retained line"
        );
    }
    let matcher = ConsoleMatcher::new(args)?;
    if let Some(entries) = result.get("entries").and_then(Value::as_array) {
        for entry in entries.iter().filter(|entry| matcher.matches(entry)) {
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

#[cfg(test)]
mod filter_tests {
    use super::*;
    use crate::cli::BridgeConnectionArgs;

    fn args(
        grep: Option<&str>,
        fixed: bool,
        level: Option<&str>,
        limit: usize,
    ) -> PluginConsoleOutputArgs {
        PluginConsoleOutputArgs {
            bridge: BridgeConnectionArgs::local(1.0),
            limit,
            since_seq: 0,
            from_oldest: false,
            clear: false,
            client: false,
            server: false,
            player: None,
            follow: false,
            grep: grep.map(str::to_string),
            fixed,
            level: level.map(str::to_string),
            interval_ms: 200,
        }
    }

    fn entry(seq: u64, kind: &str, message: &str) -> Value {
        json!({ "seq": seq, "type": kind, "message": message })
    }

    #[test]
    fn filters_apply_before_the_limit_and_report_counts() {
        let entries = (1..=50)
            .map(|seq| {
                entry(
                    seq,
                    if seq % 10 == 0 { "error" } else { "output" },
                    &format!("line {seq}"),
                )
            })
            .collect::<Vec<_>>();
        let result = filtered_console_result(
            &args(None, false, Some("error"), 3),
            json!({ "entries": entries, "ok": true }),
        )
        .unwrap();
        let kept = result["entries"].as_array().unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0]["seq"], 30);
        assert_eq!(result["scanned"], 50);
        assert_eq!(result["matched"], 5);
        assert_eq!(result["count"], 3);
    }

    #[test]
    fn grep_is_a_regex_unless_fixed() {
        let entries = vec![
            entry(1, "output", "Round started"),
            entry(2, "error", "Script Error: boom"),
            entry(3, "warn", r"ranked\|warn literal"),
        ];
        let result = filtered_console_result(
            &args(Some("rror|ranked"), false, None, 10),
            json!({ "entries": entries.clone() }),
        )
        .unwrap();
        assert_eq!(result["matched"], 2);
        let fixed = filtered_console_result(
            &args(Some("ranked\\|warn"), true, None, 10),
            json!({ "entries": entries }),
        )
        .unwrap();
        assert_eq!(fixed["matched"], 1);
        assert!(
            filtered_console_result(&args(Some("("), false, None, 10), json!({ "entries": [] }))
                .is_err()
        );
    }

    #[test]
    fn consecutive_duplicates_fold_with_a_repeat_count() {
        let entries = vec![
            entry(1, "output", "tick"),
            entry(2, "output", "tick"),
            entry(3, "output", "tick"),
            entry(4, "output", "other"),
            entry(5, "output", "tick"),
        ];
        let result =
            filtered_console_result(&args(None, false, None, 10), json!({ "entries": entries }))
                .unwrap();
        let kept = result["entries"].as_array().unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0]["repeat"], 3);
        assert_eq!(kept[0]["seq"], 3);
        assert_eq!(result["folded"], 2);
        assert!(result.get("scanned").is_none());
    }
}
