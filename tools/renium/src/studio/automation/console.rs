use std::collections::HashMap;
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
    if crate::app::output::compact_text_output() {
        return print_console_text(&result, args.since_seq);
    }
    print_json_output(&result, false)
}

/// Text mode prints one header line (`nextSeq`, which `--since-seq` continues
/// from, and the filter counts) and then a line per distinct message as
/// `[level] message`, repeats collapsed as a Play start prints them; json
/// mode keeps the entry objects with `seq` and `time`.
fn print_console_text(result: &Value, since_seq: u64) -> Result<()> {
    print_json_output(&console_text_header(result, since_seq), false)?;
    let entries = result
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for line in play_console_lines(entries) {
        println!("{line}");
    }
    io::stdout().flush()?;
    Ok(())
}

/// The header of a text-mode read. `ok` is always true there and the lines
/// carry their own repeat counts, so both go, as does an `epoch` the caller
/// did not continue from.
fn console_text_header(result: &Value, since_seq: u64) -> Value {
    let mut header = result.clone();
    if let Some(map) = header.as_object_mut() {
        map.remove("entries");
        map.remove("ok");
        map.remove("folded");
        if since_seq == 0 {
            map.remove("epoch");
        }
    }
    header
}

/// `[level] message`, with a repeat count when consecutive lines were folded
/// and continuation lines indented so one entry stays one block.
pub(super) fn console_entry_line(entry: &Value) -> String {
    let mut line = console_line_start(entry);
    if let Some(repeat) = entry.get("repeat").and_then(Value::as_u64)
        && repeat > 1
    {
        line.push_str(&format!(" (x{repeat})"));
    }
    line
}

fn console_line_start(entry: &Value) -> String {
    let level = console_entry_level(entry);
    let message = entry
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end()
        .replace('\n', "\n  ");
    format!("[{level}] {message}")
}

/// The text a Play start prints for one console: a line per distinct message
/// in the `rbx co` shape, without times. Identical messages collapse into the
/// first with ` (xN)`, messages that differ only in a long number (an asset
/// or user id) into the first with ` (+N similar)`, and a folded stack frame
/// follows its error as an indented line; the same message from another
/// location keeps its own line. Counts, coordinates and line
/// numbers are short and keep their lines apart.
pub(super) fn play_console_lines(entries: &[Value]) -> Vec<String> {
    struct Group<'a> {
        entry: &'a Value,
        same: u64,
        similar: u64,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut positions: HashMap<(&str, String, &str), usize> = HashMap::new();
    for entry in entries {
        let message = entry
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let repeat = entry
            .get("repeat")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let place = entry
            .get("where")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = (console_entry_level(entry), digit_pattern(message), place);
        match positions.get(&key) {
            Some(&position) => {
                let group = &mut groups[position];
                if group.entry.get("message").and_then(Value::as_str) == Some(message) {
                    group.same += repeat;
                } else {
                    group.similar += repeat;
                }
            }
            None => {
                positions.insert(key, groups.len());
                groups.push(Group {
                    entry,
                    same: repeat,
                    similar: 0,
                });
            }
        }
    }
    groups
        .iter()
        .map(|group| {
            let mut line = console_line_start(group.entry);
            if group.same > 1 {
                line.push_str(&format!(" (x{})", group.same));
            }
            if group.similar > 0 {
                line.push_str(&format!(" (+{} similar)", group.similar));
            }
            if let Some(place) = group.entry.get("where").and_then(Value::as_str) {
                line.push_str("\n  ");
                line.push_str(place);
            }
            line
        })
        .collect()
}

/// The shortest run of digits that reads as an id rather than a value.
const ID_DIGITS: usize = 6;

/// A message with every run of `ID_DIGITS` or more digits replaced by one
/// `0`, so two messages that differ only in an id compare equal while a
/// counter or a coordinate keeps them apart.
fn digit_pattern(message: &str) -> String {
    let mut pattern = String::with_capacity(message.len());
    let mut run = String::new();
    let flush = |run: &mut String, pattern: &mut String| {
        if run.len() >= ID_DIGITS {
            pattern.push('0');
        } else {
            pattern.push_str(run);
        }
        run.clear();
    };
    for character in message.chars() {
        if character.is_ascii_digit() {
            run.push(character);
        } else {
            flush(&mut run, &mut pattern);
            pattern.push(character);
        }
    }
    flush(&mut run, &mut pattern);
    pattern
}

/// Studio's MessageType names and a wall-clock stamp repeat on every line;
/// the level word is what a reader filters on and `time` already orders them.
pub(crate) fn compact_console_entries(result: &mut Value) {
    if crate::app::output::global_json_output() {
        return;
    }
    if let Some(map) = result.as_object_mut() {
        map.remove("epoch");
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
        "fixed": args.fixed,
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

/// The console a Play start returns: Studio's stack-trace framing (`Stack
/// Begin`, `Script '...', Line N`, `Stack End`) folds into a `where` on the
/// error or warning it follows, consecutive repeats collapse into `repeat`,
/// and the sequence numbers go; `time` keeps the order.
pub(crate) fn fold_play_console(entries: &mut Vec<Value>) {
    let mut kept: Vec<Value> = Vec::with_capacity(entries.len());
    for mut entry in entries.drain(..) {
        let message = entry
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let info = entry.get("type").and_then(Value::as_str) == Some("info");
        if info && (message == "Stack Begin" || message == "Stack End") {
            continue;
        }
        if info
            && message.starts_with("Script '")
            && let Some(previous) = kept.last_mut()
            && matches!(
                previous.get("type").and_then(Value::as_str),
                Some("error") | Some("warn")
            )
        {
            if previous.get("where").is_none() {
                previous["where"] = Value::String(message);
            }
            continue;
        }
        if let Some(map) = entry.as_object_mut() {
            map.remove("seq");
            map.remove("unix");
        }
        kept.push(entry);
    }
    fold_repeated_entries(&mut kept);
    *entries = kept;
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
            if let Some(seq) = entry.get("seq").cloned() {
                last["seq"] = seq;
            }
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
            // `output` is the level word the help lists for ordinary prints.
            let wanted = if level.eq_ignore_ascii_case("output") {
                "print"
            } else {
                level
            };
            if !entry_level.eq_ignore_ascii_case(wanted)
                && !short_console_level(entry_level).eq_ignore_ascii_case(wanted)
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
            println!("{}", console_entry_line(entry));
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

    #[test]
    fn a_play_console_folds_stack_framing_into_its_error() {
        let mut entries = vec![
            json!({"type": "print", "message": "ready", "seq": 1, "time": 1.0}),
            json!({"type": "error", "message": "x is not a valid member", "seq": 2, "time": 1.5}),
            json!({"type": "info", "message": "Stack Begin", "seq": 3, "time": 1.5}),
            json!({"type": "info", "message": "Script 'ServerScriptService.Scoreboard', Line 7", "seq": 4, "time": 1.5}),
            json!({"type": "info", "message": "Stack End", "seq": 5, "time": 1.5}),
            json!({"type": "error", "message": "x is not a valid member", "seq": 6, "time": 1.6}),
            json!({"type": "info", "message": "Stack Begin", "seq": 7, "time": 1.6}),
            json!({"type": "info", "message": "Script 'ServerScriptService.Scoreboard', Line 7", "seq": 8, "time": 1.6}),
            json!({"type": "info", "message": "Stack End", "seq": 9, "time": 1.6}),
            json!({"type": "info", "message": "plain info", "seq": 10, "time": 2.0}),
        ];
        fold_play_console(&mut entries);
        assert_eq!(
            entries,
            vec![
                json!({"type": "print", "message": "ready", "time": 1.0}),
                json!({"type": "error", "message": "x is not a valid member", "time": 1.5, "where": "Script 'ServerScriptService.Scoreboard', Line 7", "repeat": 2}),
                json!({"type": "info", "message": "plain info", "time": 2.0}),
            ]
        );
    }
}

#[cfg(test)]
mod line_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn console_lines_carry_level_message_and_repeats() {
        assert_eq!(
            console_entry_line(
                &json!({"message": "Requiring asset 1.\nCallstack:\nMain, line 5\n", "type": "print", "time": 6.7, "seq": 3})
            ),
            "[print] Requiring asset 1.\n  Callstack:\n  Main, line 5"
        );
        assert_eq!(
            console_entry_line(&json!({"message": "same", "type": "warn", "repeat": 4})),
            "[warn] same (x4)"
        );
    }

    #[test]
    fn a_text_header_keeps_the_cursor_and_the_filter_counts() {
        let result = json!({
            "ok": true, "nextSeq": 40, "matched": 3, "scanned": 20, "folded": 2,
            "hasMore": true, "epoch": "e1", "entries": [{"message": "a", "type": "print"}]
        });
        assert_eq!(
            console_text_header(&result, 0),
            json!({"nextSeq": 40, "matched": 3, "scanned": 20, "hasMore": true})
        );
        assert_eq!(console_text_header(&result, 12)["epoch"], "e1");
        assert_eq!(
            console_text_header(&json!({"ok": true, "nextSeq": 7, "entries": []}), 0),
            json!({"nextSeq": 7})
        );
    }

    #[test]
    fn a_play_console_collapses_repeats_and_messages_that_differ_in_an_id() {
        let entries = vec![
            json!({"message": "ContentId: rbxassetid://128044540936968 failed", "time": 1.0, "type": "warn"}),
            json!({"message": "[Shop] ready", "time": 1.0, "type": "print"}),
            json!({"message": "ContentId: rbxassetid://977129176443663 failed", "time": 1.1, "type": "warn"}),
            json!({"message": "event fired.", "repeat": 4, "time": 1.2, "type": "info"}),
            json!({"message": "ContentId: rbxassetid://128044540936968 failed", "time": 1.3, "type": "warn"}),
            json!({"message": "ContentId: rbxassetid://500112822177229 failed", "repeat": 2, "time": 1.4, "type": "warn"}),
            json!({"message": "event fired.", "repeat": 2, "time": 1.5, "type": "info"}),
            json!({"message": "ContentId: rbxassetid://128044540936968 failed", "time": 1.6, "type": "error"}),
            json!({"message": "x is not a valid member\nof Workspace", "time": 2.0, "type": "error",
                "where": "Script 'ServerScriptService.Scoreboard', Line 7", "repeat": 2}),
            json!({"message": "t+1: 5 of 12", "time": 2.1, "type": "print"}),
            json!({"message": "t+2: 7 of 12", "time": 2.2, "type": "print"}),
            json!({"message": "x is not a valid member\nof Workspace", "time": 2.3, "type": "error",
                "where": "Script 'ServerScriptService.Shop', Line 41"}),
        ];
        assert_eq!(
            play_console_lines(&entries),
            vec![
                "[warn] ContentId: rbxassetid://128044540936968 failed (x2) (+3 similar)",
                "[print] [Shop] ready",
                "[info] event fired. (x6)",
                "[error] ContentId: rbxassetid://128044540936968 failed",
                "[error] x is not a valid member\n  of Workspace (x2)\n  Script 'ServerScriptService.Scoreboard', Line 7",
                "[print] t+1: 5 of 12",
                "[print] t+2: 7 of 12",
                "[error] x is not a valid member\n  of Workspace\n  Script 'ServerScriptService.Shop', Line 41",
            ]
        );
        assert!(play_console_lines(&[]).is_empty());
    }

    #[test]
    fn only_a_long_run_of_digits_compares_as_an_id() {
        assert_eq!(
            digit_pattern("asset 128044540936968 of user 4953300458 failed"),
            "asset 0 of user 0 failed"
        );
        assert_eq!(digit_pattern("id 123456"), digit_pattern("id 9876543210"));
        assert_eq!(
            digit_pattern("centered 104 of 7 (v3.15.0)"),
            "centered 104 of 7 (v3.15.0)"
        );
        assert_ne!(digit_pattern("t+1: 5"), digit_pattern("t+2: 7"));
        assert_ne!(digit_pattern("line 12345"), digit_pattern("line 12346"));
    }
}
