use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use full_moon::ast;
use full_moon::node::Node;
use full_moon::visitors::Visitor;
use serde_json::{Map, Value, json};

use crate::app::output::{
    ReportedFailure, ensure_luau_api_ok, ensure_plugin_api_ok, log_global, print_json_output,
};
use crate::app::timing::current_millis;
use crate::automation::{commands::daemon_result, op};
use crate::cli::{
    BridgeConnectionArgs, EditorReviewDecisionArgs, ExecuteLuauArgs, GotoArgs, ListClientsArgs,
    PackageActionArgs, PressArgs, RecordEndArgs, RecordStartArgs, ShotArgs, StartStopPlayArgs,
    StudioChangeStateArgs, StudioDeviceArgs, TypeArgs, UiArgs, WaitUntilArgs,
};
use crate::daemon::{daemon_control_request, try_daemon_control_request};
use crate::snapshot::import::parse_services;
use crate::studio::bridge::{
    BRIDGE_DEFAULT_RESPONSE_TIMEOUT, BRIDGE_ROLE_EDIT, BRIDGE_ROLE_PLAY_CLIENT,
    BRIDGE_ROLE_PLAY_SERVER, BridgeServer, BridgeTarget,
};
use crate::studio::input as input_inject;

mod console;
mod heap;
mod input;
mod microprofiler;
pub(crate) mod monitor;
pub(crate) mod network;
#[cfg(any(windows, target_os = "macos"))]
mod process_exit;
pub(crate) mod property_access;
mod recording;
mod recording_review;
pub(crate) mod render_quality;
pub(crate) mod result_text;
pub(crate) mod studio_login;
pub(crate) mod test_processes;
mod wait_parts;

pub(crate) use console::{get_console_output_command, get_console_output_result};
pub(crate) use input::input_result;
pub(crate) use recording::{end as record_end_result, start as record_start_result};
pub(crate) use recording_review::command as record_review_command;

fn console_entry_level(entry: &Value) -> &str {
    entry
        .get("type")
        .or_else(|| entry.get("level"))
        .and_then(Value::as_str)
        .unwrap_or("output")
}

pub(crate) fn execute_luau_command(mut args: ExecuteLuauArgs) -> Result<()> {
    if args.code.is_none() {
        args.code = args.inline_code.take();
    }
    if args.runner.collect.is_some() && (args.code.is_some() || args.file.is_some()) {
        bail!("--collect reads a detached runner and takes no code");
    }
    if args.code.as_deref() == Some("-") || args.file.as_deref() == Some(std::path::Path::new("-"))
    {
        let mut code = String::new();
        io::stdin().read_to_string(&mut code)?;
        args.code = Some(code.trim_start_matches('\u{feff}').to_string());
        args.file = None;
    }
    let timeout = args.timeout.unwrap_or_else(|| {
        let code = args.code.clone().or_else(|| {
            args.file
                .as_deref()
                .and_then(|path| fs::read_to_string(path).ok())
        });
        default_luau_run_timeout(code.as_deref().unwrap_or(""))
    });
    let parameters = luau_command_parameters(&args, timeout);
    let compact = crate::app::output::compact_text_output();
    if args.out.is_some() && !compact {
        bail!("--out writes the text form of a result and does not combine with --output-mode");
    }
    let text = args.runner.detach.is_none() && args.runner.collect.is_none() && compact;
    let mut result = match daemon_result(op::LUAU, None, parameters, false, Some(&args.bridge)) {
        Err(error) if text => {
            let error = format!("{error:#}");
            crate::app::output::write_stderr(format_args!("error: {error}"));
            // The read that would follow a missing member comes with it.
            if let Some(holds) = crate::bytecode::saved_children_line(&error) {
                crate::app::output::write_stderr(format_args!("{holds}"));
            }
            return Err(ReportedFailure.into());
        }
        result => result?,
    };
    if args.timeout.is_none()
        && timeout > DEFAULT_LUAU_TIMEOUT_SECONDS
        && let Some(map) = result.as_object_mut()
    {
        map.insert("timeoutSeconds".to_string(), json!(timeout));
    }
    if let Some(map) = result.as_object_mut() {
        if map.get("background") == Some(&Value::Bool(false)) {
            map.remove("background");
        }
        if map.get("ok") == Some(&Value::Bool(true)) {
            map.remove("path");
            map.remove("runner");
            if args.edit || args.server || args.client {
                map.remove("context");
            }
        }
        for key in ["output", "results"] {
            if map
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                map.remove(key);
            }
        }
    }
    if text {
        return print_luau_text(
            &luau_body(&result),
            &luau_trailer(&result),
            args.out.as_deref(),
        );
    }
    if compact && args.runner.collect.is_some() {
        let short = result_text::compact_value(&result);
        // A runner's state is the caller's own data, often a long log: one
        // that passes the limit prints as lines, so the cut falls between two.
        if args.out.is_some() || short.to_string().chars().count() > LUAU_TEXT_LIMIT {
            return print_luau_text(&collected_text(&result), &[], args.out.as_deref());
        }
        return print_json_output(&short, false);
    }
    print_json_output(&result, false)
}

/// The daemon request of `rbx l` and `rbx lc`. `keepAlive` marks the run as
/// the user's own, which no chunk Renium composes itself carries.
fn luau_command_parameters(args: &ExecuteLuauArgs, timeout: f64) -> Value {
    json!({
        "code": args.code,
        "file": args.file.clone().map(absolute_from_cwd),
        "client": args.client,
        "player": args.player,
        "server": args.server,
        "edit": args.edit,
        "timeout": timeout,
        "args": args.arg,
        "detach": args.runner.detach,
        "collect": args.runner.collect,
        "stop": args.runner.stop,
        "lifetime": args.runner.lifetime,
        "keepAlive": true,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    })
}

/// What a plain `rbx l` or `rbx lc` run prints in text mode before its
/// closing lines: the lines the code printed as `[level] message`, then each
/// returned value (`result_text::value_text`), or `ok` when it returned
/// nothing. Where the run landed and how it persists stay in json mode.
fn luau_body(result: &Value) -> String {
    let mut lines = result
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(console::console_entry_line)
        .collect::<Vec<_>>();
    let values = result
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let alone = values.len() == 1;
    lines.extend(
        values
            .iter()
            .map(|value| result_text::value_text(value, alone)),
    );
    if values.is_empty() {
        lines.push("ok".to_string());
    }
    lines.join("\n")
}

/// The closing lines of a plain run: whatever else the result carries for
/// the reader to act on (`syncReview`, `listeners`) as one `note:` line, and
/// the daemon's advice on spending fewer tokens as a `hint:` line. They are
/// printed after a cut, so a long result does not lose them.
fn luau_trailer(result: &Value) -> Vec<String> {
    let notes = result
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "ok" | "context"
                    | "persistence"
                    | "results"
                    | "output"
                    | "timeoutSeconds"
                    | "hint"
                    | "nativeProfile"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<String, Value>>();
    let mut lines = Vec::new();
    if !notes.is_empty() {
        lines.push(format!("note: {}", Value::Object(notes)));
    }
    if let Some(hint) = result.get("hint").and_then(Value::as_str) {
        lines.push(format!("hint: {hint}"));
    }
    lines
}

/// A plain run's whole text when nothing is cut.
#[cfg(test)]
fn luau_text(result: &Value) -> String {
    shown_luau_text(&luau_body(result), &luau_trailer(result), |_| None)
}

/// A collected runner in lines: its status as one JSON line, what it printed,
/// then `state:` and `results:` with their values in the text form.
fn collected_text(result: &Value) -> String {
    let status = result
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !matches!(key.as_str(), "state" | "results" | "output"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<String, Value>>();
    let mut lines = vec![Value::Object(status).to_string()];
    lines.extend(
        result
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(console::console_entry_line),
    );
    if let Some(state) = result.get("state") {
        lines.push(format!("state:\n{}", result_text::value_text(state, true)));
    }
    if let Some(values) = result.get("results").and_then(Value::as_array) {
        lines.push("results:".to_string());
        lines.extend(
            values
                .iter()
                .map(|value| result_text::value_text(value, values.len() == 1)),
        );
    }
    lines.join("\n")
}

/// The most characters a Luau run prints in text mode. Results past it are
/// rare (2 of 735 measured agent calls) and cost a reader thousands of
/// tokens each.
const LUAU_TEXT_LIMIT: usize = 12_000;

/// The text cut at the limit, on a line end when one is near, and how many
/// characters it holds in all; None for text within the limit.
fn cut_luau_text(text: &str) -> Option<(&str, usize)> {
    let (limit, _) = text.char_indices().nth(LUAU_TEXT_LIMIT)?;
    let shown = &text[..limit];
    let shown = match shown.rfind('\n') {
        Some(line_end) if line_end >= limit - limit / 4 => &shown[..line_end],
        _ => shown,
    };
    Some((shown, text.chars().count()))
}

/// What a run's text prints as: the body, cut past the limit with a line
/// that says how much was left out and where `saved` put all of it, then the
/// closing lines.
fn shown_luau_text(
    body: &str,
    trailer: &[String],
    saved: impl FnOnce(&str) -> Option<String>,
) -> String {
    let mut lines = match cut_luau_text(body) {
        None => vec![body.to_string()],
        Some((shown, total)) => {
            let rest = match saved(body) {
                Some(file) => format!("all of it is in {file}"),
                None => "return less, or write all of it with --out FILE".to_string(),
            };
            vec![
                shown.to_string(),
                format!(
                    "... (cut: {} of {total} characters shown; {rest})",
                    shown.chars().count()
                ),
            ]
        }
    };
    lines.extend(trailer.iter().cloned());
    lines.join("\n")
}

/// The files of cut results a project keeps; older ones are removed.
const LUAU_OVERFLOW_FILES: usize = 8;

/// Saves a cut result in the cache of the project the command runs in (the
/// nearest folder up from the current one that holds a project) and names the
/// file as the caller can open it; None outside a project. Each run gets a
/// file of its own, so runs side by side do not point at each other's text.
fn save_luau_overflow(text: &str) -> Option<String> {
    let current = std::env::current_dir().ok()?;
    let project = current.ancestors().find(|folder| {
        folder.join(".renium").is_dir()
            || folder
                .join(crate::project::config::PROJECT_FILE_NAME)
                .is_file()
    })?;
    let cache = project.join(".renium").join("cache");
    fs::create_dir_all(&cache).ok()?;
    let file = cache.join(format!(
        "luau-output-{}-{}.txt",
        current_millis(),
        std::process::id()
    ));
    fs::write(&file, format!("{text}\n")).ok()?;
    let mut kept = fs::read_dir(&cache)
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("luau-output-") && name.ends_with(".txt"))
        })
        .collect::<Vec<_>>();
    kept.sort();
    for old in kept.iter().rev().skip(LUAU_OVERFLOW_FILES) {
        let _ = fs::remove_file(old);
    }
    let shown = file.strip_prefix(&current).unwrap_or(&file);
    Some(shown.to_string_lossy().replace('\\', "/"))
}

/// Prints a run's text. With `--out FILE` the body goes to the file and one
/// line says so; otherwise a body past the limit is cut and kept whole in the
/// project's cache, so one oversized return does not fill a reader's context.
fn print_luau_text(body: &str, trailer: &[String], out: Option<&Path>) -> Result<()> {
    let text = match out {
        Some(out) => {
            fs::write(out, format!("{body}\n"))
                .with_context(|| format!("Failed to write {}", out.display()))?;
            let mut lines = vec![format!(
                "{} lines ({} characters) written to {}",
                body.lines().count(),
                body.chars().count(),
                out.display()
            )];
            lines.extend(trailer.iter().cloned());
            lines.join("\n")
        }
        None => shown_luau_text(body, trailer, save_luau_overflow),
    };
    crate::app::output::write_stdout(format_args!("{text}"));
    Ok(())
}

pub(crate) fn validate_luau_syntax(code: &str) -> Result<()> {
    if code.starts_with('\u{feff}') {
        bail!(
            "The source starts with a UTF-8 byte order mark; strip it (the rest was not checked)"
        );
    }
    // Use Studio's actual language compiler, without loading or executing code.
    // Its recursion limit also rejects excessive nesting before AST traversal.
    mlua::Compiler::new()
        .compile(code)
        .context("Invalid Luau syntax")?;
    Ok(())
}

#[derive(Default)]
struct LuauLoopCheckpoints {
    offsets: Vec<usize>,
}

impl Visitor for LuauLoopCheckpoints {
    fn visit_generic_for(&mut self, node: &ast::GenericFor) {
        self.offsets
            .push(node.do_token().token().end_position().bytes());
    }
    fn visit_numeric_for(&mut self, node: &ast::NumericFor) {
        self.offsets
            .push(node.do_token().token().end_position().bytes());
    }
    fn visit_repeat(&mut self, node: &ast::Repeat) {
        self.offsets
            .push(node.repeat_token().token().end_position().bytes());
    }
    fn visit_while(&mut self, node: &ast::While) {
        self.offsets
            .push(node.do_token().token().end_position().bytes());
    }
}

/// Parses code the Luau compiler has accepted. Luau takes a type suffix after
/// a parenthesized return list that ends in a variadic, as in
/// `(boolean, ...any)?`, and full_moon does not. Code it refuses is parsed
/// once more with each `?` after a closing parenthesis blanked, which moves no
/// byte and takes nothing but an optional mark from a type.
pub(crate) fn parse_compiled_luau(code: &str) -> ast::AstResult {
    let luau = full_moon::LuaVersion::luau();
    let parsed = full_moon::parse_fallible(code, luau);
    if parsed.errors().is_empty() {
        return parsed;
    }
    without_optional_marks_after_parentheses(code)
        .map(|blanked| full_moon::parse_fallible(&blanked, luau))
        .filter(|retried| retried.errors().is_empty())
        .unwrap_or(parsed)
}

fn without_optional_marks_after_parentheses(code: &str) -> Option<String> {
    use full_moon::tokenizer::{Lexer, LexerResult, Symbol, TokenType};
    let LexerResult::Ok(tokens) = Lexer::new(code, full_moon::LuaVersion::luau()).collect() else {
        return None;
    };
    let mut blanked = code.as_bytes().to_vec();
    let mut after_parenthesis = false;
    for token in &tokens {
        let is = |symbol| *token.token_type() == TokenType::Symbol { symbol };
        if token.token_type().is_trivia() {
            continue;
        }
        if after_parenthesis && is(Symbol::QuestionMark) {
            blanked[token.start_position().bytes()] = b' ';
        } else {
            after_parenthesis = is(Symbol::RightParen);
        }
    }
    String::from_utf8(blanked).ok().filter(|text| text != code)
}

fn instrument_luau(code: &str) -> Result<String> {
    let parsed = parse_compiled_luau(code);
    if let Some(error) = parsed.errors().first() {
        let (start, _) = error.range();
        bail!(
            "Invalid Luau syntax at {}:{}: {}",
            start.line(),
            start.character(),
            error.error_message()
        );
    }
    let parsed = parsed.into_ast();
    let mut checkpoints = LuauLoopCheckpoints::default();
    checkpoints.visit_ast(&parsed);
    if checkpoints.offsets.is_empty() {
        return Ok(code.to_string());
    }
    let mut name = "__reniumCooperate".to_string();
    while code.contains(&name) {
        name.push('_');
    }
    let count = format!("{name}Count");
    let started = format!("{name}Started");
    let preamble = format!(
        "local {count}=64;local {started}=os.clock();local function {name}(){count}-=1;if {count}>0 then return end;{count}=64;local now=os.clock();if now-{started}>=0.004166666666666667 then task.wait();{started}=os.clock() end end;"
    );
    // Keep comments, directives and the user's formatting intact.
    // Collect byte offsets instead of repeatedly cloning each nested loop AST.
    let first = parsed
        .tokens()
        .next()
        .context("Missing Luau statement")?
        .token()
        .start_position()
        .bytes();
    let mut output = String::with_capacity(
        code.len() + preamble.len() + checkpoints.offsets.len() * (name.len() + 4),
    );
    output.push_str(&code[..first]);
    output.push_str(&preamble);
    let mut previous = first;
    checkpoints.offsets.sort_unstable();
    for offset in checkpoints.offsets {
        output.push_str(&code[previous..offset]);
        output.push(' ');
        output.push_str(&name);
        output.push_str("();");
        previous = offset;
    }
    output.push_str(&code[previous..]);
    validate_luau_syntax(&output)?;
    Ok(output)
}

pub(crate) fn cooperative_luau(code: &str) -> Result<String> {
    validate_luau_syntax(code)?;
    if !["for", "while", "repeat"]
        .iter()
        .any(|word| code.contains(word))
    {
        return Ok(code.to_string());
    }
    on_luau_parser_stack(|| instrument_luau(code))?
}

/// Runs `work` on one joined worker with its own 64 MiB stack. full_moon's
/// recursive parser can exhaust an ordinary daemon request thread even with a
/// few nested loops in a debug build, so parsing, traversal and AST
/// destruction all belong inside `work`.
pub(crate) fn on_luau_parser_stack<T: Send>(work: impl FnOnce() -> T + Send) -> Result<T> {
    thread::scope(|scope| {
        thread::Builder::new()
            .name("renium-luau-parser".into())
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, work)
            .context("Could not start the Luau parser")?
            .join()
            .map_err(|_| anyhow::anyhow!("The Luau parser failed"))
    })
}

fn luau_request(code: &str, chunk_name: &str, target: BridgeTarget, timeout: f64) -> Result<Value> {
    let code = cooperative_luau(code)?;
    Ok(json!({
        "code": code,
        "chunkName": chunk_name,
        "context": if target == BridgeTarget::Client { "client" } else { "plugin" },
        "timeoutSeconds": timeout,
    }))
}

fn call_execute_luau(
    bridge: &BridgeServer,
    target: BridgeTarget,
    player: Option<&str>,
    code: &str,
    chunk_name: &str,
    timeout: f64,
    response_padding: f64,
) -> Result<Value> {
    bridge.call_for_selector_with_timeout(
        "executeLuau",
        luau_request(code, chunk_name, target, timeout)?,
        target,
        player,
        Some(Duration::from_secs_f64(timeout + response_padding)),
    )
}

fn runner_name(name: &str) -> Result<&str> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        bail!("Runner names use up to 64 letters, digits, '_' or '-'");
    }
    Ok(name)
}

/// Where a Luau run landed, for error messages: an error from the Edit window
/// reads differently from one on the play server or a client.
fn luau_context_label(result: &Value, player: Option<&str>) -> String {
    match result.get("context").and_then(Value::as_str) {
        Some("client") => match player {
            Some(player) => format!("client {player}"),
            None => "play client".to_string(),
        },
        Some("server") => "play server".to_string(),
        Some("edit") | Some("plugin") => "Edit window".to_string(),
        _ => "Studio".to_string(),
    }
}

/// Fails for a run the plugin reported as failed, named after the runtime it
/// ran in. `code` is the chunk the user wrote, when its text can explain the
/// failure.
fn ensure_luau_ok_in_context(
    result: &Value,
    player: Option<&str>,
    code: Option<&str>,
) -> Result<()> {
    ensure_luau_api_ok(result).map_err(|error| {
        let label = luau_context_label(result, player);
        match error.downcast::<crate::app::output::LuauRunFailure>() {
            Ok(run) => run
                .with_event_wait_hint(code)
                .with_hint(code.and_then(|code| crate::app::output::luau_code_hint(code, result)))
                .in_context(&label)
                .into(),
            Err(error) => anyhow::anyhow!("[{label}] {error:#}"),
        }
    })
}

/// How long a Play runner of a user-issued chunk stays after the chunk
/// returns, so a connection or `task.delay` made in one call is still there
/// for the next.
const LUAU_KEEP_ALIVE_SECONDS: f64 = 120.0;

/// The `executeLuau` request of a user's chunk. A detached runner carries its
/// own lifetime; any other run that may land in a Play runtime asks the
/// plugin to keep its runner under `keepAliveSeconds`, a parameter the Edit
/// window's path and older plugins do not read.
fn user_luau_request(
    code: &str,
    target: BridgeTarget,
    timeout: f64,
    runner: &crate::cli::LuauRunnerArgs,
) -> Result<Value> {
    let mut request = luau_request(code, "Renium", target, timeout)?;
    if let Some(name) = runner.detach.as_deref() {
        request["detach"] = json!(runner_name(name)?);
        request["lifetimeSeconds"] = json!(runner.lifetime.clamp(1.0, 3600.0));
    } else if runner.keep_alive && target != BridgeTarget::Edit {
        request["keepAliveSeconds"] = json!(LUAU_KEEP_ALIVE_SECONDS);
    }
    Ok(request)
}

/// What a Play run says when its Studio's plugin predates kept runners.
const LISTENERS_ENDED: &str = "ended with the chunk: this Studio runs an older Renium plugin; restart Studio to load the installed one";

/// Drops what the plugin says about a kept runner (`background`, its
/// `executionId`): the caller has nothing to do with either, and a text
/// result would print them as a note on every Play run. A Play runner that
/// was asked to stay and did not says so as `listeners`, since whatever the
/// chunk connected is gone.
fn strip_kept_runner_fields(result: &mut Value) {
    let Some(map) = result.as_object_mut() else {
        return;
    };
    let kept = map.remove("background") == Some(Value::Bool(true));
    map.remove("executionId");
    let ran_in_play =
        map.get("runner") == Some(&Value::Bool(true)) && map.get("ok") == Some(&Value::Bool(true));
    if ran_in_play && !kept {
        map.insert("listeners".to_string(), json!(LISTENERS_ENDED));
    }
}

/// The run limit `rbx l` and `rbx lc` use when `-t` is not given.
pub(crate) const DEFAULT_LUAU_TIMEOUT_SECONDS: f64 = 10.0;

/// The run limit when `-t` is not given: it grows to cover the waits written
/// into the code, so a probe with `task.wait(25)`, `WaitForChild(name, 15)`
/// or a loop bounded by `os.clock() - started < 40` is not stopped at 10 s.
/// Studio stops a runner at 120 s regardless.
fn default_luau_run_timeout(code: &str) -> f64 {
    let code = luau_without_strings_and_comments(code);
    let code = code.as_str();
    let sleeps = literal_sleep_seconds(code);
    if waits_in_a_loop(code) {
        return (sleeps + DEFAULT_LUAU_TIMEOUT_SECONDS).clamp(LUAU_LOOP_TIMEOUT_SECONDS, 120.0);
    }
    if sleeps + LUAU_SLEEP_MARGIN_SECONDS <= DEFAULT_LUAU_TIMEOUT_SECONDS {
        return DEFAULT_LUAU_TIMEOUT_SECONDS;
    }
    (sleeps + DEFAULT_LUAU_TIMEOUT_SECONDS).min(120.0)
}

/// The code with every string and comment blanked, so a `wait(90)` quoted in
/// a message or left in a comment is not read as a call. Text the tokenizer
/// cannot read is scanned as it is.
fn luau_without_strings_and_comments(code: &str) -> String {
    use full_moon::tokenizer::{Lexer, LexerResult, TokenType};
    let tokens = match Lexer::new(code, full_moon::LuaVersion::luau()).collect() {
        LexerResult::Ok(tokens) | LexerResult::Recovered(tokens, _) => tokens,
        LexerResult::Fatal(_) => return code.to_string(),
    };
    let mut text = String::with_capacity(code.len());
    for token in &tokens {
        match token.token_type() {
            TokenType::StringLiteral { .. }
            | TokenType::InterpolatedString { .. }
            | TokenType::SingleLineComment { .. }
            | TokenType::MultiLineComment { .. } => text.push(' '),
            _ => text.push_str(&token.to_string()),
        }
    }
    text
}

/// The default limit of code that waits inside a loop: a polling or
/// step-by-step probe runs for the loop's count, which its literal sleeps
/// do not show.
const LUAU_LOOP_TIMEOUT_SECONDS: f64 = 30.0;

fn waits_in_a_loop(code: &str) -> bool {
    static LOOP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(?:for|while|repeat)\b").expect("valid loop pattern")
    });
    static WAIT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\bwait\s*\(").expect("valid wait pattern")
    });
    LOOP.is_match(code) && WAIT.is_match(code)
}

/// Room the default limit leaves after the code's literal sleeps before it
/// grows; shorter sleeps fit the limit as it is.
const LUAU_SLEEP_MARGIN_SECONDS: f64 = 5.0;

/// Seconds the code may wait by its own number literals: `wait(N)` and
/// `task.wait(N)`, the timeout of `WaitForChild(name, N)` (the name is a
/// blanked string here), and a deadline against a clock, `os.clock() - t < N`
/// or `os.clock() + N`.
fn literal_sleep_seconds(code: &str) -> f64 {
    const NUMBER: &str = r"((?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?)";
    const CLOCK: &str = r"\b(?:os\.clock|os\.time|tick|time)\s*\(\s*\)";
    static WAITS: std::sync::LazyLock<[regex::Regex; 4]> = std::sync::LazyLock::new(|| {
        [
            format!(r"\bwait\s*\(\s*{NUMBER}\s*\)"),
            format!(r"\bWaitForChild\s*\([^(),]*,\s*{NUMBER}\s*\)"),
            format!(r"{CLOCK}\s*-\s*[\w.]+\s*<=?\s*{NUMBER}"),
            format!(r"{CLOCK}\s*\+\s*{NUMBER}"),
        ]
        .map(|pattern| regex::Regex::new(&pattern).expect("valid wait pattern"))
    });
    WAITS
        .iter()
        .flat_map(|pattern| pattern.captures_iter(code))
        .filter_map(|capture| capture[1].parse::<f64>().ok())
        .sum()
}

/// The text of a Luau file to run. A file saved with a UTF-8 byte order mark
/// loses it, as code read from stdin does; Luau takes the mark for a syntax
/// error.
fn read_luau_file(path: &Path) -> Result<String> {
    let code =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(code.trim_start_matches('\u{feff}').to_string())
}

pub(crate) fn execute_luau_result(args: ExecuteLuauArgs, bridge: &BridgeServer) -> Result<Value> {
    let client = args.client || args.player.is_some();
    let target = if args.server {
        BridgeTarget::Server
    } else if args.edit {
        BridgeTarget::Edit
    } else {
        BridgeTarget::main_or_client(client)
    };
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    if let Some(name) = args.runner.collect.as_deref() {
        let result = bridge.call_for_selector_with_timeout(
            "collectLuauRunner",
            json!({ "name": runner_name(name)?, "stop": args.runner.stop }),
            target,
            player,
            Some(Duration::from_secs(30)),
        )?;
        ensure_luau_ok_in_context(&result, player, None)?;
        return Ok(result);
    }
    let (written, sent_whole) = if let Some(code) = args.code.or(args.inline_code) {
        (code, true)
    } else if let Some(path) = args.file {
        (read_luau_file(&path)?, false)
    } else {
        bail!("Missing Luau code. Use -e <code> or -f <file>.");
    };
    let code = if args.arg.is_empty() {
        written.clone()
    } else {
        // Checked as written, so a syntax error names the caller's own lines.
        validate_luau_syntax(&written)?;
        luau_with_arguments(&written, &args.arg)?
    };
    let timeout = args
        .timeout
        .unwrap_or(DEFAULT_LUAU_TIMEOUT_SECONDS)
        .clamp(0.1, 120.0);
    let request = user_luau_request(&code, target, timeout, &args.runner)?;
    let kept = request.get("keepAliveSeconds").is_some();
    let mut result = bridge.call_for_selector_with_timeout(
        "executeLuau",
        request,
        target,
        player,
        Some(Duration::from_secs_f64(timeout + 10.0)),
    )?;
    ensure_luau_ok_in_context(&result, player, Some(&code))?;
    if args.runner.detach.is_some() && result.get("detached").is_none() {
        bail!("The Studio plugin predates detached runners; run rbx setup and restart Studio");
    }
    if kept {
        strip_kept_runner_fields(&mut result);
    }
    // Advice goes to the runs a caller wrote, not to chunks Renium composes.
    if args.runner.keep_alive && args.runner.detach.is_none() {
        let resent = sent_whole
            .then(|| lock_token_advice().resent(&written))
            .flatten()
            .map(|runs| resent_chunk_hint(runs, client));
        let hint = resent.or_else(|| {
            lock_token_advice()
                .long_floats(&result)
                .then(|| LONG_FLOATS_HINT.to_string())
        });
        if let Some(hint) = hint {
            result["hint"] = json!(hint);
        }
    }
    Ok(result)
}

/// A `-a` value: JSON when it is JSON, any other text as a string. Text that
/// opens like a JSON table or string and is neither is an error, since a
/// shell that ate the quotes would otherwise hand the chunk a string where it
/// expects a table.
fn luau_argument(value: &str) -> Result<Value> {
    match serde_json::from_str(value) {
        Ok(parsed) => Ok(parsed),
        Err(error) if value.starts_with(['{', '[', '"']) => bail!(
            "-a {value}: not valid JSON ({error}); a table is written as {{\"key\":1}} or [1,2], and text that starts with {{, [ or a quote as a JSON string"
        ),
        Err(_) => Ok(Value::String(value.to_string())),
    }
}

/// The chunk with `values` as its `...`. The wrapper opens on the chunk's
/// first line and closes on a line of its own, so error lines keep their
/// numbers and a comment on the last line ends before `end`.
fn luau_with_arguments(code: &str, values: &[String]) -> Result<String> {
    let literals = values
        .iter()
        .map(|value| Ok(luau_literal(&luau_argument(value)?)))
        .collect::<Result<Vec<_>>>()?
        .join(",");
    Ok(format!("return (function(...) {code}\nend)({literals})"))
}

/// A JSON value written as Luau.
fn luau_literal(value: &Value) -> String {
    match value {
        Value::Null => "nil".to_string(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::String(text) => {
            let mut literal = String::with_capacity(text.len() + 2);
            literal.push('"');
            for character in text.chars() {
                match character {
                    '"' | '\\' => {
                        literal.push('\\');
                        literal.push(character);
                    }
                    '\n' => literal.push_str("\\n"),
                    '\r' => literal.push_str("\\r"),
                    control if control.is_control() => {
                        literal.push_str(&format!("\\u{{{:X}}}", control as u32));
                    }
                    other => literal.push(other),
                }
            }
            literal.push('"');
            literal
        }
        Value::Array(items) => format!(
            "{{{}}}",
            items.iter().map(luau_literal).collect::<Vec<_>>().join(",")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, item)| format!(
                    "[{}]={}",
                    luau_literal(&Value::String(key.clone())),
                    luau_literal(item)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

/// The chunk with every string, number, comment and space taken out: what
/// two runs share when only their values differ.
fn luau_skeleton(code: &str) -> String {
    use full_moon::tokenizer::{Lexer, LexerResult, TokenType};
    let tokens = match Lexer::new(code, full_moon::LuaVersion::luau()).collect() {
        LexerResult::Ok(tokens) | LexerResult::Recovered(tokens, _) => tokens,
        LexerResult::Fatal(_) => return code.to_string(),
    };
    let mut skeleton = String::with_capacity(code.len());
    for token in &tokens {
        match token.token_type() {
            TokenType::StringLiteral { .. }
            | TokenType::InterpolatedString { .. }
            | TokenType::Number { .. } => skeleton.push('0'),
            TokenType::SingleLineComment { .. }
            | TokenType::MultiLineComment { .. }
            | TokenType::Whitespace { .. } => {}
            _ => {
                skeleton.push_str(&token.to_string());
                skeleton.push(' ');
            }
        }
    }
    skeleton
}

/// A chunk shorter than this costs about as much as the call that would
/// replace it.
const RESENT_CHUNK_MIN_LENGTH: usize = 200;

/// How many chunk shapes the daemon remembers before it starts over.
const RESENT_CHUNK_SHAPES: usize = 1024;

/// Unrounded numbers in the text a run built before the run counts as one
/// that spends tokens on digits.
const LONG_FLOATS_PER_RUN: usize = 4;

const LONG_FLOATS_HINT: &str = "the text this code builds carries every digit of its numbers; return the values themselves (a table for several) and they print to 3 decimals";

/// What the daemon has seen callers spend tokens on, for the `hint:` line of
/// a Luau run. Agents resend one probe a dozen times with other values and
/// format numbers into text digit by digit; a hint now and then costs less
/// than either habit.
#[derive(Default)]
struct TokenAdvice {
    chunk_runs: std::collections::HashMap<u64, u32>,
    long_float_runs: u32,
}

impl TokenAdvice {
    /// How often this chunk has been sent whole with only its values
    /// changed, when that is worth saying: the third time, and again each
    /// time the count doubles.
    fn resent(&mut self, code: &str) -> Option<u32> {
        use std::hash::{Hash, Hasher};
        let skeleton = luau_skeleton(code);
        if skeleton.len() < RESENT_CHUNK_MIN_LENGTH {
            return None;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        skeleton.hash(&mut hasher);
        if self.chunk_runs.len() >= RESENT_CHUNK_SHAPES {
            self.chunk_runs.clear();
        }
        let runs = self.chunk_runs.entry(hasher.finish()).or_insert(0);
        *runs += 1;
        (runs.is_multiple_of(3) && (*runs / 3).is_power_of_two()).then_some(*runs)
    }

    /// Whether to say that this run's own text is full of unrounded numbers:
    /// the second run that is, and again each time their count doubles.
    fn long_floats(&mut self, result: &Value) -> bool {
        static LONG: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\d\.\d{6,}").expect("valid float pattern")
        });
        let texts = result
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            // A JSON table in a string prints as the table, rounded.
            .filter(|text| !text.starts_with(['{', '[']))
            .chain(
                result
                    .get("output")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.get("message").and_then(Value::as_str)),
            );
        let found = texts
            .map(|text| LONG.find_iter(text).count())
            .sum::<usize>();
        if found < LONG_FLOATS_PER_RUN {
            return false;
        }
        self.long_float_runs += 1;
        self.long_float_runs >= 2 && self.long_float_runs.is_power_of_two()
    }
}

fn lock_token_advice() -> std::sync::MutexGuard<'static, TokenAdvice> {
    static ADVICE: std::sync::LazyLock<std::sync::Mutex<TokenAdvice>> =
        std::sync::LazyLock::new(Default::default);
    ADVICE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn resent_chunk_hint(runs: u32, client: bool) -> String {
    let command = if client {
        "rbx lc -f FILE PLAYER -a VALUE"
    } else {
        "rbx l -f FILE -a VALUE"
    };
    format!(
        "this chunk has now been sent {runs} times with only its values changed; put it in a file once and send the values: `{command}`"
    )
}

pub(crate) fn studio_device_command(args: StudioDeviceArgs) -> Result<()> {
    let parameters = json!({
        "action": args.action,
        "device": args.device,
        "orientation": args.orientation,
        "scalingMode": args.scaling_mode,
        "resolution": args.resolution,
        "pixelDensity": args.pixel_density,
        "details": args.details,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let result = daemon_result(op::DEVICE, None, parameters, false, Some(&args.bridge))?;
    print_json_output(&result, false)
}

pub(crate) fn package_action_command(
    args: PackageActionArgs,
    project: Option<&Path>,
    operation: u16,
) -> Result<()> {
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout > 600.0 {
        bail!("Package timeout must be >0 and <=600s");
    }
    let result = daemon_result(
        operation,
        project,
        json!({
            "target": args.target,
            "ords": args.ords,
            "pid": args.pid,
            "timeout": args.timeout,
        }),
        false,
        Some(&args.bridge),
    )?;
    print_json_output(&result, false)
}

pub(crate) fn studio_device_resolution(raw: &str) -> Result<(u32, u32)> {
    let normalized = raw.trim().to_ascii_lowercase().replace('×', "x");
    let (width, height) = normalized
        .split_once('x')
        .with_context(|| format!("Invalid resolution '{raw}'. Use WIDTHxHEIGHT."))?;
    let width = width
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid resolution width in '{raw}'"))?;
    let height = height
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid resolution height in '{raw}'"))?;
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        bail!("Resolution must use positive 32-bit dimensions");
    }
    Ok((width, height))
}

pub(crate) fn studio_device_result(
    args: &StudioDeviceArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let mut params = Map::new();
    params.insert("action".to_string(), Value::String(args.action.clone()));
    if let Some(device) = args.device.as_ref() {
        params.insert("device".to_string(), Value::String(device.clone()));
    }
    if let Some(orientation) = args.orientation.as_ref() {
        params.insert(
            "orientation".to_string(),
            Value::String(orientation.clone()),
        );
    }
    if let Some(scaling_mode) = args.scaling_mode.as_ref() {
        params.insert(
            "scalingMode".to_string(),
            Value::String(scaling_mode.clone()),
        );
    }
    if let Some(resolution) = args.resolution.as_deref() {
        let (width, height) = studio_device_resolution(resolution)?;
        params.insert("width".to_string(), json!(width));
        params.insert("height".to_string(), json!(height));
    }
    if let Some(pixel_density) = args.pixel_density {
        if !pixel_density.is_finite() || pixel_density <= 0.0 {
            bail!("Pixel density must be a finite number greater than zero");
        }
        params.insert("pixelDensity".to_string(), json!(pixel_density));
    }
    if args.details {
        params.insert("details".to_string(), Value::Bool(true));
    }
    let requested_params = params.clone();
    let action = args.action.to_ascii_lowercase();
    let mut result =
        bridge.call_for_target("deviceSimulator", Value::Object(params), BridgeTarget::Edit)?;
    if result.get("timedOut") == Some(&Value::Bool(true))
        && matches!(action.as_str(), "set" | "select" | "apply")
        && let Some(settled) = settled_device_status(bridge, &requested_params)
    {
        result = settled;
    }
    ensure_plugin_api_ok(&result)?;
    match action.as_str() {
        "stop" | "reset" => {
            #[cfg(any(windows, target_os = "macos"))]
            {
                let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
                input_inject::close_device_emulator_toolbar(pid)?;
            }
            bridge.set_desired_device_request(json!({ "action": "stop" }));
        }
        "set" | "select" | "apply" => {
            let mut request = Map::new();
            request.insert("action".to_string(), Value::String("set".to_string()));
            if let Some(device) = result["device"]["id"].as_str() {
                request.insert("device".to_string(), Value::String(device.to_string()));
            }
            if let Some(orientation) = result["orientation"].as_str() {
                request.insert(
                    "orientation".to_string(),
                    Value::String(orientation.to_string()),
                );
            }
            if let Some(scaling_mode) = result["scalingMode"].as_str() {
                request.insert(
                    "scalingMode".to_string(),
                    Value::String(scaling_mode.to_string()),
                );
            }
            for key in [
                "device",
                "orientation",
                "scalingMode",
                "width",
                "height",
                "pixelDensity",
            ] {
                if let Some(value) = requested_params.get(key) {
                    request.insert(key.to_string(), value.clone());
                }
            }
            if args.device.is_some() {
                bridge.set_desired_device_request(Value::Object(request));
            } else {
                bridge.merge_desired_device_request(Value::Object(request));
            }
        }
        _ => {}
    }
    Ok(result)
}

/// Studio's device setters can outlive the plugin's reply window and still
/// finish; the status read afterwards says whether the requested device is
/// now simulated, in which case that status stands in for the set's result.
fn settled_device_status(bridge: &BridgeServer, requested: &Map<String, Value>) -> Option<Value> {
    let wanted = requested
        .get("device")
        .and_then(Value::as_str)
        .map(device_name_key);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = bridge
            .call_for_target(
                "deviceSimulator",
                json!({ "action": "status" }),
                BridgeTarget::Edit,
            )
            .ok()?;
        let simulating = status.get("simulating") == Some(&Value::Bool(true));
        let device_matches = wanted.as_deref().is_none_or(|wanted| {
            ["name", "id"].iter().any(|key| {
                status["device"][key]
                    .as_str()
                    .is_some_and(|value| device_name_key(value) == wanted)
            })
        });
        if simulating && device_matches {
            let mut settled = status;
            settled["action"] = json!("set");
            settled["settledAfterTimeout"] = json!(true);
            return Some(settled);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// `iPhone 16 Pro` and `iphone_16_pro` name the same device.
fn device_name_key(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn wait_for_player_bridge(bridge: &BridgeServer, player: &str, wait_seconds: f64) -> Result<()> {
    // Without a Play session nothing can join, and a client index above the
    // number of clients the running session has can never match; say so now
    // instead of after the whole wait.
    let runtimes = bridge.list_bridge_clients();
    let servers = runtimes
        .iter()
        .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_SERVER)
        .count();
    let clients = runtimes
        .iter()
        .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
        .count();
    if servers == 0 && clients == 0 {
        bail!(
            "No Play session is running, so no client matches player selector '{player}'; `rbx play -s` starts one"
        );
    }
    if let Ok(index) = player.parse::<usize>()
        && clients > 0
        && index > clients
    {
        bail!(
            "Player selector '{player}' asks for client {index}, but the session has {clients}; ordinary Play has one client, `rbx play --add-players N` adds more"
        );
    }
    if bridge.wait_for_ready_player(player, Duration::from_secs_f64(wait_seconds.max(1.0))) {
        return Ok(());
    }
    let connected = bridge
        .list_bridge_clients()
        .iter()
        .map(|entry| {
            format!(
                "{} {} pid {}",
                entry["role"].as_str().unwrap_or("?"),
                entry
                    .get("playerName")
                    .or_else(|| entry.get("placeName"))
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                entry["pid"]
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    bail!(
        "No connected play client matches player selector '{player}'. Connected: {connected}; `rbx play --add-players 1` adds a client, `rbx play -s` starts a session"
    )
}

pub(crate) fn start_stop_play_command(args: StartStopPlayArgs) -> Result<()> {
    validate_play_args(&args)?;
    if args.kill_orphans {
        return kill_orphans_command(&args);
    }
    let operation = if args.stop {
        op::PLAY_STOP
    } else {
        op::PLAY_START
    };
    let parameters = json!({
        "players": args.players,
        "addPlayers": args.add_players,
        "leave": args.leave,
        "mode": args.mode,
        "restart": args.restart,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    validate_play_waits(&args)?;
    // A stop names no players, so a session started with -p N stops too.
    let stop_parameters = json!({
        "mode": args.mode,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let mut result = if args.restart {
        // Edits saved during Play wait for Edit mode; stopping first and
        // letting Live Sync settle makes the restarted session run them.
        // A stop with nothing running answers with the Edit window's status;
        // only a session that was stopped counts as restarted.
        let stopped = daemon_control_request(op::PLAY_STOP, None, stop_parameters.clone(), false)
            .map(|stop| {
                stop.get("ok") == Some(&Value::Bool(true))
                    && stop.get("action") == Some(&json!("stop"))
            })
            .unwrap_or(false);
        let settled = settle_live_sync_before_restart(&args);
        let mut result = daemon_control_request(operation, None, parameters, false)?;
        if result.get("ok") != Some(&Value::Bool(false)) {
            result["restarted"] = json!(stopped);
            if let Some(settled) = settled {
                result["appliedPending"] = settled;
            }
        }
        result
    } else {
        daemon_control_request(operation, None, parameters, false)?
    };
    if args.until.is_none()
        && !args.no_ready
        && !args.stop
        && args.add_players.is_none()
        && !args.leave
        && result.get("ok") != Some(&Value::Bool(false))
    {
        attach_player_readiness(&mut result, &args);
    }
    if let Some(target) = args.at.as_deref()
        && result.get("ok").and_then(Value::as_bool) != Some(false)
    {
        result["at"] = stand_player_at(target, &result, &args);
    }
    let started = result.get("ok").and_then(Value::as_bool) != Some(false);
    let named_players = args.players.is_some();
    let waits_began = Instant::now();
    if started {
        let held = await_play_conditions(&mut result, &args);
        if !matches!(held, Ok(true)) {
            // A wait that failed leaves the session up for a look at it, so
            // a requested stop is reported as not done.
            if args.stop_after {
                result["stopped"] = json!(false);
            }
            print_play_result(result, named_players)?;
            return Err(held.err().unwrap_or_else(|| ReportedFailure.into()));
        }
    }
    // Ordinary Play has one client; its console answers the client-side
    // half of the same check without a second session.
    let has_client = result
        .get("clients")
        .and_then(Value::as_array)
        .is_some_and(|clients| !clients.is_empty())
        && args.mode.as_deref().is_none_or(|mode| mode == "play");
    let mut waits_hold = started;
    if let Some(text) = args.until_console.as_deref()
        && started
    {
        let record = await_console_text(text, has_client, &args);
        waits_hold = record.get("matched") == Some(&Value::Bool(true));
        result["untilConsole"] = record;
    }
    if waits_hold {
        let limit = || result_run_seconds(args.until_timeout, waits_began.elapsed().as_secs_f64());
        // With --until alone the server result was read in the wait's own
        // runner, at the moment the condition held; a client condition after
        // it moves the reading behind that one.
        if (args.until.is_none() || args.client_until.is_some())
            && let Some(code) = args.result.as_deref()
        {
            result["result"] = evaluate_after_wait(code, "server", limit(), &args);
        }
        if let Some(code) = args.client_result.as_deref() {
            result["clientResult"] = evaluate_after_wait(code, "client", limit(), &args);
        }
    }
    if let Some(limit) = args.console
        && started
    {
        result["console"] = play_console_entries("server", limit, &args);
        if has_client {
            result["clientConsole"] = play_console_entries("client", limit, &args);
        }
    }
    if args.stop_after && started {
        result["stopped"] =
            match daemon_control_request(op::PLAY_STOP, None, stop_parameters, false) {
                Ok(stop) => json!(stop.get("ok") == Some(&Value::Bool(true))),
                Err(error) => json!({ "error": format!("{error:#}") }),
            };
    }
    print_play_result(result, named_players)
}

/// Checks the wait and result options of a play start before anything runs:
/// each is a Luau expression or chunk, a result has a wait to follow, and a
/// client condition has a client to run on.
fn validate_play_waits(args: &StartStopPlayArgs) -> Result<()> {
    for condition in [args.until.as_deref(), args.client_until.as_deref()]
        .into_iter()
        .flatten()
    {
        wait_condition_body(condition)?;
    }
    for result in [args.result.as_deref(), args.client_result.as_deref()]
        .into_iter()
        .flatten()
    {
        wait_result_body(result)?;
    }
    if (args.result.is_some() || args.client_result.is_some())
        && args.until.is_none()
        && args.client_until.is_none()
        && args.until_console.is_none()
    {
        bail!("--result and --client-result need --until, --client-until or --until-console");
    }
    if args.client_until.is_some() && matches!(args.mode.as_deref(), Some("run" | "server")) {
        bail!("--client-until waits on a client, and --mode run and server have none");
    }
    Ok(())
}

/// Runs the `--until` wait on the server, then the `--client-until` wait on
/// the first client, each for up to `-t` seconds, and records them on the
/// start result as `until` and `clientUntil`. `Ok(false)` means a wait did
/// not hold; its record says why.
fn await_play_conditions(result: &mut Value, args: &StartStopPlayArgs) -> Result<bool> {
    let edit_runtime_id = result.get("editRuntimeId").cloned().unwrap_or(Value::Null);
    let wait = |mut parameters: Value| {
        parameters["timeout"] = json!(args.until_timeout);
        parameters["editRuntimeId"] = edit_runtime_id.clone();
        parameters["bridgeWaitSeconds"] = json!(args.bridge.wait_seconds);
        parameters["bridgePorts"] = json!(args.bridge.ports);
        daemon_control_request(op::WAIT, None, parameters, false)
    };
    if let Some(condition) = args.until.as_deref() {
        let result_in_wait = args
            .result
            .as_deref()
            .filter(|_| args.client_until.is_none());
        let mut wait = wait(json!({
            "condition": condition,
            "result": result_in_wait,
            "server": true,
        }))
        .map_err(|error| result_chunk_timeout(error, result_in_wait.is_some()))?;
        name_failed_result_chunk(&mut wait);
        if !attach_awaited_result(result, wait) {
            return Ok(false);
        }
    }
    if let Some(condition) = args.client_until.as_deref() {
        let wait = wait(json!({
            "condition": condition,
            "client": true,
            "player": 1,
        }))?;
        let held = wait.get("ok") == Some(&Value::Bool(true));
        result["clientUntil"] = wait;
        return Ok(held);
    }
    Ok(true)
}

/// A wait whose condition was met and whose `--result` chunk then failed
/// says what to do about the chunk: the session it ran in is still up.
fn name_failed_result_chunk(wait: &mut Value) {
    if wait.get("conditionMet") != Some(&Value::Bool(true)) {
        return;
    }
    if let Some(error) = wait.get("error").and_then(Value::as_str) {
        wait["error"] = json!(crate::app::output::failed_play_result_error(error, false));
    }
}

/// A wait with a `--result` chunk runs that chunk in the wait's runner, so a
/// runner that ran out of time there is the chunk's failure and gets the
/// chunk's hint. Any other error passes through.
fn result_chunk_timeout(error: anyhow::Error, has_result: bool) -> anyhow::Error {
    let text = format!("{error:#}");
    if !has_result || !text.contains("Luau runner timed out after") {
        return error;
    }
    let message = crate::app::output::failed_play_result_error(&text, false);
    match error.downcast::<crate::automation::DaemonFailure>() {
        Ok(mut failure) => {
            failure.message = message;
            failure.into()
        }
        Err(_) => anyhow::anyhow!(message),
    }
}

/// Seconds a `--result` or `--client-result` chunk may run: what the waits
/// left of `-t`, never less than a plain Luau run gets and at most the 120 s
/// Studio gives a runner.
fn result_run_seconds(budget: f64, waited: f64) -> f64 {
    (budget - waited).clamp(DEFAULT_LUAU_TIMEOUT_SECONDS, 120.0)
}

/// The last `limit` entries of the play server's or the first client's
/// console, folded; `{ "error" }` when the console could not be read.
fn play_console_entries(side: &str, limit: u64, args: &StartStopPlayArgs) -> Value {
    let console = daemon_control_request(
        op::CONSOLE,
        None,
        json!({
            "limit": limit,
            "sinceSeq": 0,
            "fromOldest": false,
            "clear": false,
            side: true,
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
        false,
    );
    match console {
        Ok(mut console) => {
            console::compact_console_entries(&mut console);
            let mut entries = console
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            console::fold_play_console(&mut entries);
            Value::Array(entries)
        }
        Err(error) => json!({ "error": format!("{error:#}") }),
    }
}

/// Polls the server console, and the client's when the session has one,
/// until a line contains `text` or `-t` seconds pass; the `untilConsole`
/// record of the start result.
fn await_console_text(text: &str, has_client: bool, args: &StartStopPlayArgs) -> Value {
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(args.until_timeout);
    let sides: &[&str] = if has_client {
        &["server", "client"]
    } else {
        &["server"]
    };
    let matched = loop {
        let found = sides.iter().find_map(|side| {
            let entries = play_console_entries(side, 200, args);
            let entry = entries.as_array()?.iter().find(|entry| {
                entry
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.contains(text))
            })?;
            Some((*side, entry.clone()))
        });
        if found.is_some() || Instant::now() >= deadline {
            break found;
        }
        thread::sleep(Duration::from_millis(400));
    };
    let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    match matched {
        Some((side, entry)) => json!({
            "text": text,
            "matched": true,
            "side": side,
            "seconds": seconds,
            "entry": entry,
        }),
        None => json!({ "text": text, "matched": false, "timedOut": true, "seconds": seconds }),
    }
}

/// Prints a Play result. Text mode keeps the decision fields on one JSON line
/// and prints the consoles the result carries as text after it.
fn print_play_result(mut result: Value, named_players: bool) -> Result<()> {
    if !crate::app::output::compact_text_output() {
        return print_json_output(&result, false);
    }
    compact_play_result(&mut result, named_players);
    let consoles = take_play_consoles(&mut result);
    print_json_output(&result, false)?;
    if !consoles.is_empty() {
        crate::app::output::write_stdout(format_args!("{}", consoles.join("\n")));
    }
    Ok(())
}

/// Moves the `console` and `clientConsole` entries out of a Play result and
/// returns them as text: a `server console:` or `client console:` label, then
/// one line per distinct message. A console that could not be read stays in
/// the result with its error.
fn take_play_consoles(result: &mut Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(map) = result.as_object_mut() else {
        return lines;
    };
    for (key, label) in [
        ("console", "server console:"),
        ("clientConsole", "client console:"),
    ] {
        if !map.get(key).is_some_and(Value::is_array) {
            continue;
        }
        let Some(Value::Array(entries)) = map.remove(key) else {
            continue;
        };
        if entries.is_empty() {
            lines.push(format!("{label} empty"));
        } else {
            lines.push(label.to_string());
            lines.extend(console::play_console_lines(&entries));
        }
    }
    lines
}

/// Evaluates `--result` or `--client-result` once the waits hold, on the
/// server or on the first client, for up to `timeout` seconds; the value, or
/// `{ "error" }` with what to do about the failed chunk.
fn evaluate_after_wait(code: &str, side: &str, timeout: f64, args: &StartStopPlayArgs) -> Value {
    let code = match wait_result_body(code) {
        Ok(code) => code,
        Err(error) => return json!({ "error": format!("{error:#}") }),
    };
    let parameters = result_chunk_parameters(&code, side, timeout, args);
    match daemon_control_request(op::LUAU, None, parameters, false) {
        Ok(run) => run
            .get("results")
            .and_then(Value::as_array)
            .and_then(|results| results.first())
            .cloned()
            .unwrap_or(Value::Null),
        Err(error) => json!({
            "error": crate::app::output::failed_play_result_error(
                &format!("{error:#}"),
                side == "client",
            )
        }),
    }
}

/// The Luau request of a `--result` or `--client-result` chunk. It is not a
/// user-issued run: its runner ends with the chunk.
fn result_chunk_parameters(
    code: &str,
    side: &str,
    timeout: f64,
    args: &StartStopPlayArgs,
) -> Value {
    let mut parameters = json!({
        "code": code,
        "timeout": timeout,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    if side == "client" {
        parameters["client"] = json!(true);
        parameters["player"] = json!(1);
    } else {
        parameters["server"] = json!(true);
    }
    parameters
}

/// Runs `--at` on the play server: the first player's character is stood on
/// the target once it exists, and the outcome (`moved`, `player`, `position`
/// or `error`) is the `at` field of the start result.
fn stand_player_at(target: &str, result: &Value, args: &StartStopPlayArgs) -> Value {
    let code = stand_player_code(target);
    let run = daemon_control_request(
        op::LUAU,
        None,
        json!({
            "code": code,
            "server": true,
            "timeout": 45.0,
            "editRuntimeId": result.get("editRuntimeId"),
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
        false,
    );
    match run {
        Ok(run) if run.get("ok") == Some(&Value::Bool(true)) => run
            .get("results")
            .and_then(Value::as_array)
            .and_then(|results| results.first())
            .cloned()
            .unwrap_or_else(|| json!({ "moved": false, "error": "the move returned nothing" })),
        Ok(run) => json!({
            "moved": false,
            "error": run.get("error").cloned().unwrap_or_else(|| json!("the move failed"))
        }),
        Err(error) => json!({ "moved": false, "error": format!("{error:#}") }),
    }
}

/// Server Luau for `--at`: waits up to 30 s for the first player's character
/// and HumanoidRootPart, pivots the character so its feet rest on the target
/// (the top face of a part or model, or the given position), zeroes its
/// velocity and gives the physics 1.5 s to register the contact.
fn stand_player_code(target: &str) -> String {
    let position = target
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()
        .filter(|parts| parts.len() == 3);
    let goal = match position {
        Some(parts) => format!("CFrame.new({}, {}, {})", parts[0], parts[1], parts[2]),
        None => {
            let mut segments = target
                .split('.')
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            if segments
                .first()
                .is_some_and(|first| first.eq_ignore_ascii_case("game"))
            {
                segments.remove(0);
            }
            if segments.first().is_some_and(|first| first == "workspace") {
                segments[0] = "Workspace".to_string();
            }
            format!(
                "(function() local inst = game for _, name in {} do inst = inst:FindFirstChild(name) if not inst then error('no instance {}') end end local size = if inst:IsA('BasePart') then inst.Size elseif inst:IsA('Model') then inst:GetExtentsSize() else Vector3.zero return inst:GetPivot() * CFrame.new(0, size.Y / 2, 0) end)()",
                crate::bytecode::edit::luau_string_list(&segments),
                target.replace(['\\', '\''], ""),
            )
        }
    };
    format!(
        "local deadline = os.clock() + 30 local player, character, root repeat task.wait(0.1) player = game.Players:GetPlayers()[1] character = player and player.Character root = character and character:FindFirstChild('HumanoidRootPart') until root or os.clock() > deadline if not root then return {{ moved = false, error = 'no player character within 30 s' }} end local goal = {goal} character:PivotTo(goal * CFrame.new(0, 3, 0)) root.AssemblyLinearVelocity = Vector3.zero task.wait(1.5) return {{ moved = true, player = player.Name, position = {{ root.Position.X, root.Position.Y, root.Position.Z }} }}"
    )
}

/// The server-side condition a started session is ready for its first test
/// step: every requested client has a Player, or the one player of ordinary
/// Play. Run and server sessions have no players to wait for.
fn player_readiness_condition(players: Option<u32>, mode: Option<&str>) -> Option<String> {
    match (players, mode.unwrap_or("play")) {
        (_, "run" | "server") => None,
        (Some(count), _) => Some(format!("#game.Players:GetPlayers() >= {count}")),
        (None, _) => Some("#game.Players:GetPlayers() >= 1".to_string()),
    }
}

const PLAYER_READY_SECONDS: f64 = 60.0;

/// Waits for the session's players and records `playersReady` (and how long
/// it took) on the start result; a timeout is reported, not failed, so the
/// start itself still counts.
fn attach_player_readiness(result: &mut Value, args: &StartStopPlayArgs) {
    let Some(condition) = player_readiness_condition(args.players, args.mode.as_deref()) else {
        return;
    };
    let wait = daemon_control_request(
        op::WAIT,
        None,
        json!({
            "condition": condition,
            "server": true,
            "timeout": PLAYER_READY_SECONDS,
            "editRuntimeId": result.get("editRuntimeId"),
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
        false,
    );
    match wait {
        Ok(wait) if wait.get("ok") == Some(&Value::Bool(true)) => {
            result["playersReady"] = json!(true);
            if let Some(elapsed) = wait.get("elapsedSeconds") {
                result["readySeconds"] = elapsed.clone();
            }
        }
        Ok(wait) => {
            result["playersReady"] = json!(false);
            result["readyError"] = wait
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!("the players did not join in time"));
        }
        Err(error) => {
            result["playersReady"] = json!(false);
            result["readyError"] = json!(format!("{error:#}"));
        }
    }
}

/// Before a restart, gives Live Sync up to 30 s to apply the file edits it
/// deferred while Play ran; `None` when Live Sync is not running.
fn settle_live_sync_before_restart(args: &StartStopPlayArgs) -> Option<Value> {
    let status = daemon_control_request(
        op::LIVE_STATUS,
        None,
        json!({
            "settleWaitSeconds": 30.0,
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
        false,
    )
    .ok()?;
    let running = status
        .get("daemon")
        .and_then(|daemon| daemon.get("running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !running {
        return None;
    }
    Some(json!({
        "pendingChanges": status.get("pendingChanges").cloned().unwrap_or(json!(0)),
        "settled": status
            .get("daemon")
            .and_then(|daemon| daemon.get("settled"))
            .cloned()
            .unwrap_or(json!(true)),
    }))
}

/// Adds the `--until` outcome to a Play start result: the wait record under
/// `until`, and the typed `--result` observation under `result` so callers
/// read it beside the launch fields. Returns whether the wait succeeded.
fn attach_awaited_result(result: &mut Value, mut wait: Value) -> bool {
    let succeeded = wait.get("ok") == Some(&Value::Bool(true));
    if succeeded && let Some(observed) = wait.as_object_mut().and_then(|map| map.remove("result")) {
        result["result"] = observed;
    }
    result["until"] = wait;
    succeeded
}

/// Text mode answers a Play start with the decision fields: whether the
/// server is ready, how many clients joined and what `--until` returned. The
/// launch identifiers and per-client bridge records stay in json mode.
fn compact_play_result(result: &mut Value, named_players: bool) {
    let Some(map) = result.as_object_mut() else {
        return;
    };
    if map.get("ok") != Some(&Value::Bool(true)) {
        return;
    }
    map.remove("readySeconds");
    if let Some(clients) = map.get("clients").and_then(Value::as_array) {
        let value = if named_players {
            json!(
                clients
                    .iter()
                    .filter_map(|client| client.get("playerName").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            )
        } else {
            // Ordinary Play lists the server and the one client; the count
            // that matters is the players.
            let players = clients
                .iter()
                .filter(|client| {
                    client.get("role").and_then(Value::as_str) != Some("server")
                        && client.get("playerName").is_some()
                })
                .count();
            json!(if players > 0 { players } else { clients.len() })
        };
        map.insert("clients".to_string(), value);
    }
    // A stop answers with the fact; the engine flags, the attempt count and
    // the teardown wait behind it stay in json mode.
    if map.get("action") == Some(&json!("stop")) {
        map.remove("status");
        map.remove("waitedForTeardownMs");
        map.insert("stopped".to_string(), Value::Bool(true));
    }
    // Ready players imply a ready server, and Play is the mode unless asked.
    if map.get("playersReady") == Some(&Value::Bool(true)) {
        map.remove("serverReady");
    }
    if map.get("mode") == Some(&json!("play")) {
        map.remove("mode");
    }
    for key in [
        "action",
        "editPid",
        "editRuntimeId",
        "launchNonce",
        "deviceSimulation",
        "method",
        "attempts",
    ] {
        map.remove(key);
    }
    // A start that found a session running answers with the Edit window's
    // controller status; `alreadyRunning` and `startNote` say what it means.
    if map.get("alreadyRunning") == Some(&Value::Bool(true)) {
        for key in [
            "starting",
            "readyForStart",
            "engineRunning",
            "engineStarted",
            "executing",
            "runState",
            "studioTest",
            "lastResult",
        ] {
            map.remove(key);
        }
    }
    for (key, seconds_key) in [
        ("until", "untilSeconds"),
        ("clientUntil", "clientUntilSeconds"),
    ] {
        compact_play_wait(map, key, seconds_key);
    }
    for key in ["result", "clientResult"] {
        if let Some(value) = map.get_mut(key) {
            *value = result_text::compact_value(value);
        }
    }
    if let Some(status) = map.get_mut("status").and_then(Value::as_object_mut) {
        status.remove("lastResult");
        status.remove("studioTest");
    }
}

/// A wait of a play start in text mode: one that held becomes its value
/// under `key` and its seconds under `seconds_key`; one that failed keeps its
/// record without the echoed condition.
fn compact_play_wait(map: &mut Map<String, Value>, key: &str, seconds_key: &str) {
    if let Some(wait) = map.get(key)
        && wait.get("ok") == Some(&Value::Bool(true))
        && let Some(value) = wait.get("value").cloned()
    {
        let elapsed = wait.get("elapsedSeconds").and_then(Value::as_f64);
        map.insert(key.to_string(), value);
        if let Some(elapsed) = elapsed {
            map.insert(
                seconds_key.to_string(),
                json!((elapsed * 10.0).round() / 10.0),
            );
        }
    } else if let Some(wait) = map.get_mut(key).and_then(Value::as_object_mut) {
        compact_wait_record(wait);
    }
}

fn kill_orphans_command(args: &StartStopPlayArgs) -> Result<()> {
    let result = daemon_control_request(
        op::STUDIOS,
        None,
        json!({
            "killOrphans": true,
            "place": crate::studio::target::place_filter(),
            "bridgeWaitSeconds": args.bridge.wait_seconds,
            "bridgePorts": args.bridge.ports,
        }),
        false,
    )?;
    print_json_output(&result, false)?;
    if let Some(failed) = result["failed"]
        .as_array()
        .filter(|failed| !failed.is_empty())
    {
        bail!(
            "{} orphaned Studio test process(es) could not be closed",
            failed.len()
        );
    }
    Ok(())
}

pub(crate) fn studio_change_state_command(
    args: StudioChangeStateArgs,
    project: Option<&Path>,
) -> Result<()> {
    let operation = if args.stop {
        op::LIVE_STOP
    } else if args.no_start {
        op::LIVE_STATUS
    } else if args.clear_pending {
        op::DISCARD_PENDING
    } else {
        op::LIVE_START
    };
    studio_change_state_operation_command(args, operation, project)
}

pub(crate) fn studio_change_state_operation_command(
    mut args: StudioChangeStateArgs,
    operation: u16,
    project: Option<&Path>,
) -> Result<()> {
    args.stop = operation == op::LIVE_STOP;
    args.no_start = operation == op::LIVE_STATUS;
    args.clear_pending = operation == op::DISCARD_PENDING;
    if operation == op::LIVE_START {
        args.reset = true;
        args.replace_services = true;
    }
    let has_preference = args.prefer.is_some();
    let details = args.details;
    let settle_after_start = args
        .settle_wait_seconds
        .filter(|seconds| operation == op::LIVE_START && *seconds > 0.0);
    let parameters = live_operation_parameters(&args);
    let mut result = daemon_result(operation, project, parameters, false, Some(&args.bridge))?;
    if let Some(seconds) = settle_after_start
        && result.get("ok").and_then(Value::as_bool) != Some(false)
    {
        let status = StudioChangeStateArgs {
            no_start: true,
            settle_wait_seconds: Some(seconds),
            ..StudioChangeStateArgs::plain(args.bridge.clone(), details)
        };
        let settled = daemon_result(
            op::LIVE_STATUS,
            project,
            live_operation_parameters(&status),
            false,
            Some(&status.bridge),
        )?;
        merge_settled_status(&mut result, settled);
    }
    finish_studio_change_state_command(operation, has_preference, result, details)
}

/// Runs Live Sync's comparison again, as a plain `rbx lon` does: a review
/// whose differences are gone clears and Live Sync resumes.
pub(crate) fn compare_live_sync_again(project: Option<&Path>) -> Result<Value> {
    let args = StudioChangeStateArgs {
        reset: true,
        replace_services: true,
        ..StudioChangeStateArgs::plain(crate::cli::BridgeConnectionArgs::local(8.0), false)
    };
    daemon_result(
        op::LIVE_START,
        project,
        live_operation_parameters(&args),
        false,
        Some(&args.bridge),
    )
}

fn live_operation_parameters(args: &StudioChangeStateArgs) -> Value {
    json!({
        "services": args.services,
        "reset": args.reset,
        "replaceServices": args.replace_services,
        "ackSeq": args.ack_seq,
        "ackRuntimeSettingsSeq": args.ack_runtime_settings_seq,
        "ackActions": args.ack_actions,
        "ackActionResults": args.ack_action_results,
        "runtimeId": args.runtime_id,
        "suppressSeconds": args.suppress_seconds,
        "eventWaitSeconds": args.event_wait_seconds,
        "settleWaitSeconds": args.settle_wait_seconds.map(|seconds| seconds.clamp(0.0, 120.0)),
        "contextBound": args.context_bound,
        "resolveConflictPreference": args.prefer,
        "resolveConflicts": args.resolve,
        "initialSyncPriority": args.initial_sync_priority,
        "initialConflictPreference": args.initial_conflict_preference,
        "compact": !args.details,
        "manageFiles": true,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    })
}

/// A `lon --wait` result carries the daemon status observed after the settle
/// wait, and fails the way `lst --wait` does when the sync did not settle.
fn merge_settled_status(result: &mut Value, settled: Value) {
    let Some(target) = result.as_object_mut() else {
        return;
    };
    if let Some(daemon) = settled.get("daemon") {
        target.insert("daemon".to_string(), daemon.clone());
    }
    if settled.get("ok") == Some(&Value::Bool(false)) {
        target.insert("ok".to_string(), Value::Bool(false));
        if let Some(error) = settled.get("error") {
            target.insert("error".to_string(), error.clone());
        }
    }
}

/// How many conflicts a Live Sync status shows without `--details`.
const SHOWN_CONFLICTS: usize = 20;

/// What a caller does with open conflicts.
const DECIDE_CONFLICTS: &str = "Decide each conflict from its values: `rbx lon --resolve ID=studio|editor` (once per conflict; files = editor), or `rbx lon --prefer studio|editor` for all of them. A run that leaves any conflict open applies nothing and keeps no decision, so repeat earlier --resolve values with new ones. Ask the user only where both sides hold work you cannot account for. `rbx lof`, then a scoped push (-p/-f with -u), leaves the rest of Studio untouched.";

/// The open conflicts a Live Sync result carries, with how many more the
/// compact status left out.
fn open_conflicts(result: &Value) -> Option<(Vec<Value>, u64)> {
    let conflicts = result
        .pointer("/daemon/conflicts")
        .and_then(Value::as_array)
        .filter(|conflicts| !conflicts.is_empty())?;
    let more = result
        .pointer("/daemon/moreConflicts")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some((conflicts.clone(), more))
}

/// Prints the conflicts that wait for a decision, each as its record, with
/// what to do next, and fails.
fn report_open_conflicts(error: &str, conflicts: Vec<Value>, more: u64) -> Result<()> {
    let mut next = DECIDE_CONFLICTS.to_string();
    if more > 0 {
        next.push_str(&format!(
            " {more} more conflicts are not shown; `rbx lst --details` lists all."
        ));
    }
    let mut report = json!({
        "ok": false,
        "error": error,
        "next": next,
        "conflicts": conflicts,
    });
    if more > 0 {
        report["moreConflicts"] = json!(more);
    }
    print_json_output(&report, false)?;
    Err(ReportedFailure.into())
}

fn finish_studio_change_state_command(
    operation: u16,
    has_preference: bool,
    mut result: Value,
    details: bool,
) -> Result<()> {
    let failed = result.get("ok").and_then(Value::as_bool) == Some(false);
    let resolution_required = result
        .pointer("/daemon/resolutionRequired")
        .and_then(Value::as_bool)
        == Some(true);
    if failed && resolution_required {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Studio and project files changed the same content");
        if operation == op::LIVE_START && has_preference {
            bail!(error.to_string());
        }
        if open_conflicts(&result).is_none() {
            bail!(
                "{error}\nResolve with one:\nrbx lon --prefer studio\nrbx lon --prefer editor\nrbx lof, then push only the paths you need (-p/-f with -u)"
            );
        }
    }
    if failed && let Some((conflicts, more)) = open_conflicts(&result) {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Studio and project files changed the same content");
        return report_open_conflicts(error, conflicts, more);
    }
    if failed {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Live Sync could not complete the requested operation");
        let differences = result
            .pointer("/daemon/differences")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|line| line.starts_with("push: ") || line.starts_with("pull: "))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if differences.is_empty() {
            if !details && error.ends_with("Studio and project files differ") {
                bail!("{error}; `rbx lst --details` lists the paths as push:/pull:");
            }
            bail!(error.to_string());
        }
        if details {
            let pulls = differences
                .iter()
                .filter(|line| line.starts_with("pull: "))
                .count();
            bail!(
                "{error}\n{}\nStudio differs on {pulls} pull: path(s), the files on {} push: path(s); `rbx lon --prefer studio` keeps Studio's version, `rbx lon --prefer editor` keeps the files",
                differences.join("\n"),
                differences.len() - pulls
            );
        }
        bail!(
            "{error} ({} paths; `rbx lst --details` lists them as push:/pull:)",
            differences.len()
        );
    }
    if !details {
        if let Some(map) = result.as_object_mut() {
            for key in ["runtimeId", "seq", "snapshotSeq", "runtimeSettingsSeq"] {
                map.remove(key);
            }
        }
        crate::app::output::strip_empty(&mut result);
    }
    trim_quiet_live_status(&mut result);
    print_json_output(&result, false)
}

pub(crate) fn studio_change_state_result(
    args: StudioChangeStateArgs,
    bridge: &BridgeServer,
    runtime_id: &str,
) -> Result<Value> {
    let details = args.details;
    let services = parse_services(&args.services)?;
    let action_results: Value = serde_json::from_str(&args.ack_action_results)
        .context("--ack-action-results must be a JSON object")?;
    if !action_results.is_object() {
        bail!("--ack-action-results must be a JSON object");
    }
    let result = bridge.call_for_runtime_with_timeout(
        "getStudioChangeState",
        json!({
            "services": services,
            "reset": args.reset,
            "replaceServices": args.replace_services,
            // Discarding pending file edits must not forget Studio's own
            // pending changes: they are still pulled afterwards.
            "clearPending": false,
            "start": !args.no_start,
            "stop": args.stop,
            "ackSeq": args.ack_seq,
            "ackRuntimeSettingsSeq": args.ack_runtime_settings_seq,
            "ackEditorActions": args.ack_actions,
            "ackEditorActionResults": action_results,
            "runtimeId": args.runtime_id,
            "suppressSeconds": args.suppress_seconds,
            "waitSeconds": args.event_wait_seconds,
            "contextBound": args.context_bound,
            "compact": !details,
        }),
        BridgeTarget::Edit,
        runtime_id,
        None,
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(if details {
        result
    } else {
        compact_live_status(result)
    })
}

pub(crate) fn normalize_live_status(mut value: Value) -> Value {
    let error = value
        .pointer("/daemon/error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            (value.pointer("/daemon/settled").and_then(Value::as_bool) == Some(false)).then(|| {
                match value.pointer("/daemon/unsettled").and_then(Value::as_str) {
                    Some(reason) => {
                        format!("Live Sync did not settle before the wait ended: {reason}")
                    }
                    None => {
                        "Live Sync did not finish before the wait ended; inspect rbx lst --details"
                            .to_string()
                    }
                }
            })
        });
    if let Some(error) = error
        && let Some(result) = value.as_object_mut()
    {
        result.insert("ok".into(), Value::Bool(false));
        result.insert("error".into(), Value::String(error));
    }
    value
}

pub(crate) fn compact_live_status(value: Value) -> Value {
    let value = normalize_live_status(value);
    let Some(source) = value.as_object() else {
        return value;
    };
    let mut result = Map::new();
    for key in [
        "busy",
        "role",
        "runtimeId",
        "twoWaySyncEnabled",
        "tracking",
        "trackedServices",
        "connectedInstances",
        "onlyCodeMode",
        "seq",
        "snapshotSeq",
        "runtimeSettingsSeq",
        "conflictResolution",
        "dirtyServices",
        "fullSyncServices",
    ] {
        if let Some(value) = source.get(key) {
            result.insert(key.to_string(), value.clone());
        }
    }

    let mut pending_changes = 0usize;
    for (source_key, count_key) in [
        ("changes", "changeCount"),
        ("propertyChanges", "propertyChangeCount"),
        ("editorActions", "editorActionCount"),
        ("runtimeSettingChanges", "runtimeSettingChangeCount"),
    ] {
        let count = source
            .get(count_key)
            .and_then(Value::as_u64)
            .map(|count| count as usize)
            .or_else(|| {
                source
                    .get(source_key)
                    .and_then(Value::as_array)
                    .map(Vec::len)
            })
            .unwrap_or_default();
        pending_changes += count;
        if count > 0 {
            result.insert(count_key.to_string(), json!(count));
        }
    }
    result.insert("pendingChanges".to_string(), json!(pending_changes));

    let daemon = source.get("daemon").map(compact_live_daemon_status);
    let mut ok = source.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if let Some(error) = source.get("error") {
        ok = false;
        result.insert("error".to_string(), error.clone());
    }
    result.insert("ok".to_string(), Value::Bool(ok));
    if let Some(daemon) = daemon {
        result.insert("daemon".to_string(), daemon);
    }
    Value::Object(result)
}

fn compact_live_daemon_status(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return value.clone();
    };
    let mut result = Map::new();
    for key in [
        "running",
        "mode",
        "resolutionRequired",
        "pullChanges",
        "paused",
        "syncing",
        "settled",
        "pushes",
        "pulls",
        "autoDesyncedPackages",
        "autoDesyncedAtPush",
        "error",
        "previousError",
        "terrainObservation",
        "nativeCapture",
        "mootDecisions",
    ] {
        if let Some(value) = source.get(key) {
            result.insert(key.to_string(), value.clone());
        }
    }
    let pending = source
        .get("pendingCount")
        .and_then(Value::as_u64)
        .map(|count| count as usize)
        .or_else(|| {
            source
                .get("pendingPaths")
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or_default();
    result.insert("pendingCount".to_string(), json!(pending));
    if pending > 0
        && pending <= 8
        && let Some(paths) = source.get("pendingPaths")
    {
        result.insert("pendingPaths".to_string(), paths.clone());
    }
    if let Some(conflicts) = source
        .get("conflicts")
        .and_then(Value::as_array)
        .filter(|conflicts| !conflicts.is_empty())
    {
        result.insert(
            "conflicts".to_string(),
            Value::Array(conflicts.iter().take(SHOWN_CONFLICTS).cloned().collect()),
        );
        if conflicts.len() > SHOWN_CONFLICTS {
            result.insert(
                "moreConflicts".to_string(),
                json!(conflicts.len() - SHOWN_CONFLICTS),
            );
        }
    }
    Value::Object(result)
}

/// Text mode drops the Live Sync members at their quiet value: a reader
/// looks for the others, and json mode keeps the full shape.
pub(crate) fn trim_quiet_live_status(result: &mut Value) {
    if crate::app::output::global_json_output() {
        return;
    }
    if let Some(map) = result.as_object_mut() {
        map.remove("role");
        if map.get("twoWaySyncEnabled") == Some(&Value::Bool(true)) {
            map.remove("twoWaySyncEnabled");
        }
        if map.get("onlyCodeMode") == Some(&Value::Bool(false)) {
            map.remove("onlyCodeMode");
        }
    }
    let Some(daemon) = result.get_mut("daemon").and_then(Value::as_object_mut) else {
        return;
    };
    for (key, quiet) in [
        ("paused", json!(false)),
        ("syncing", json!(false)),
        ("resolutionRequired", json!(false)),
        ("pullChanges", json!(true)),
        ("settled", json!(true)),
        ("pushes", json!(0)),
        ("pulls", json!(0)),
    ] {
        if daemon.get(key) == Some(&quiet) {
            daemon.remove(key);
        }
    }
}

fn validate_play_args(args: &StartStopPlayArgs) -> Result<()> {
    if args.start && args.stop {
        bail!("Use either --start or --stop, not both");
    }
    if args.players.is_some() && args.stop {
        bail!("--players cannot be combined with --stop");
    }
    let mode = args.mode.as_deref().unwrap_or("play");
    if !matches!(mode, "play" | "run" | "server") {
        bail!("Invalid play mode '{mode}'; use play, run, or server");
    }
    if args.players.is_some() && mode != "play" {
        bail!("--players can only be used with --mode play");
    }
    if args.players == Some(1) && !args.leave && args.mode.is_none() {
        bail!(
            "--players 1 launches a separate local server plus one client window; a one-client check is ordinary Play: rbx play -s. If the test really needs a separate server with one client, pass --mode play as well"
        );
    }
    if (args.add_players.is_some() || args.leave) && (args.start || args.stop) {
        bail!("--add-players and --leave change a running test; drop --start and --stop");
    }
    if let Some(count) = args.add_players {
        if args.leave || args.players.is_some() {
            bail!("Use --add-players N on its own; remove a client separately with --leave -p N");
        }
        if !(1..=8).contains(&count) {
            bail!("--add-players takes 1 through 8; Studio runs at most 8 clients");
        }
    }
    if args.leave {
        match args.players {
            None => bail!("Name the client to remove: rbx play --leave -p N (rbx cs lists them)"),
            Some(0) => bail!("Client indexes start at 1: rbx play --leave -p 1"),
            Some(_) => {}
        }
    }
    Ok(())
}

/// How a play start waits for Studio between two polls. The daemon's wait
/// hands the process-wide request gate back for the duration and selects the
/// request's context again before it returns, so requests of other Studios
/// run meanwhile and re-pin the shared runtime pins. Every bridge call after
/// a wait therefore names its runtime, or runs on that fresh selection.
pub(crate) type PollWait<'a> = &'a dyn Fn(Duration);

pub(crate) fn start_stop_play_result(
    args: StartStopPlayArgs,
    bridge: &BridgeServer,
    wait: PollWait<'_>,
) -> Result<Value> {
    validate_play_args(&args)?;
    let mode = args.mode.as_deref().unwrap_or("play");
    if args.stop {
        return stop_studio_play_with_bridge_result(bridge);
    }
    if args.restart {
        return restart_play_result(args, bridge, wait);
    }
    if let Some(count) = args.add_players {
        return add_test_players_result(bridge, count, args.bridge.wait_seconds);
    }
    if let Some(index) = args.players.filter(|_| args.leave) {
        return leave_test_result(bridge, index, args.bridge.wait_seconds);
    }
    if args.start || args.players.is_some() {
        let plan = match args.players {
            Some(players) => PlayLaunchPlan::Multi(players),
            None if matches!(mode, "run" | "server") => PlayLaunchPlan::Run,
            None => PlayLaunchPlan::Play,
        };
        let mut result = start_play_with_retries(bridge, plan, wait)?;
        result["serverReady"] = json!(server_answers_luau(bridge, wait));
        return Ok(result);
    }
    let result = bridge.call_for_target("startStopPlay", json!({}), BridgeTarget::Edit)?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);
const OLDER_PLUGIN: &str = "The Renium plugin in this test is older than rbx and cannot add or remove clients; update the plugin and restart Studio";

fn session_play_clients(bridge: &BridgeServer, server_runtime_id: &str) -> Vec<Value> {
    let owner = bridge
        .list_bridge_clients()
        .into_iter()
        .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(server_runtime_id))
        .and_then(|entry| {
            entry
                .get("launchEditRuntimeId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            bridge
                .runtime_pin_for_selector(BridgeTarget::Edit, None)
                .ok()
                .map(|pin| pin.runtime_id)
        });
    let Some(owner) = owner else {
        return Vec::new();
    };
    studio_play_clients(bridge, &owner)
        .into_iter()
        .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
        .collect()
}

fn runtime_ids(clients: &[Value]) -> HashSet<String> {
    clients
        .iter()
        .filter_map(|entry| entry.get("runtimeId").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn added_clients_deadline(started: Instant, last_progress: Instant) -> Instant {
    (last_progress + Duration::from_secs(30)).min(started + Duration::from_secs(120))
}

fn add_test_players_result(bridge: &BridgeServer, count: u32, wait_seconds: f64) -> Result<Value> {
    if bridge
        .wait_for_target(wait_seconds, BridgeTarget::Server)
        .is_err()
    {
        bail!("No play test is running; start one with rbx play -s -p N, then add players");
    }
    let server = bridge
        .runtime_pin_for_selector(BridgeTarget::Server, None)?
        .runtime_id;
    let before = runtime_ids(&session_play_clients(bridge, &server));
    let requested = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({ "addPlayers": count }),
        BridgeTarget::Server,
        &server,
        None,
    )?;
    ensure_plugin_api_ok(&requested)?;
    if requested["action"] != "addPlayers" {
        bail!(OLDER_PLUGIN);
    }
    let started = Instant::now();
    let mut last_progress = started;
    let mut last_seen = 0;
    loop {
        let clients = session_play_clients(bridge, &server);
        let added = clients
            .iter()
            .filter(|entry| {
                entry
                    .get("runtimeId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !before.contains(id))
            })
            .collect::<Vec<_>>();
        let names = added
            .iter()
            .filter_map(|entry| entry.get("playerName").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if added.len() >= count as usize && names.len() == added.len() {
            return Ok(json!({
                "ok": true,
                "action": "addPlayers",
                "added": names,
                "clients": server_player_names(bridge, &server).map_or(clients.len(), |names| names.len()),
            }));
        }
        if added.len() != last_seen {
            last_seen = added.len();
            last_progress = Instant::now();
        }
        if Instant::now() >= added_clients_deadline(started, last_progress) {
            bail!(
                "Studio accepted --add-players {count}, but {} new client(s) connected in {} s; the rest may still be joining (rbx cs lists them)",
                added.len(),
                started.elapsed().as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn leave_test_result(bridge: &BridgeServer, index: u32, wait_seconds: f64) -> Result<Value> {
    let selector = index.to_string();
    if !bridge.wait_for_ready_player(&selector, Duration::from_secs_f64(wait_seconds.max(1.0))) {
        bail!("No play client {index} is connected; rbx cs lists the clients of the running test");
    }
    let runtime_id = bridge
        .runtime_pin_for_selector(BridgeTarget::Client, Some(&selector))?
        .runtime_id;
    let Some(server) = bridge.play_runtime_for_selector(BridgeTarget::Server, None) else {
        bail!("No play server is connected; --leave needs a running test (rbx play -s -p N)");
    };
    let player = client_player_name(bridge, &runtime_id).with_context(|| {
        format!("Client {index} has not reported its player name yet; retry in a moment")
    })?;
    let kick = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({ "kickPlayer": player, "reason": "rbx play --leave" }),
        BridgeTarget::Server,
        &server,
        Some(Duration::from_secs(2)),
    )?;
    ensure_plugin_api_ok(&kick)?;
    if kick["action"] != "kick" {
        bail!(OLDER_PLUGIN);
    }
    let kicked = kick["kicked"].as_bool() == Some(true);
    let leave_requested = bridge
        .call_for_runtime_with_timeout(
            "startStopPlay",
            json!({ "leave": true }),
            BridgeTarget::Client,
            &runtime_id,
            Some(Duration::from_secs(2)),
        )
        .is_ok_and(|reply| reply["ok"] == true && reply["action"] == "leave");
    let remaining =
        wait_for_server_to_drop(bridge, &server, &player).map_err(
            |error| match kick["kickError"].as_str() {
                Some(kick_error) => anyhow::anyhow!("{error} (Kick failed: {kick_error})"),
                None => error,
            },
        )?;
    let window_closed = leave_requested && wait_for_channel_to_close(bridge, &runtime_id);
    Ok(json!({
        "ok": true,
        "action": "leave",
        "player": player,
        "kicked": kicked,
        "serverListsPlayer": false,
        "windowClosed": window_closed,
        "clients": remaining.len(),
    }))
}

fn client_player_name(bridge: &BridgeServer, runtime_id: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let name = bridge
            .list_bridge_clients()
            .into_iter()
            .find(|entry| entry.get("runtimeId").and_then(Value::as_str) == Some(runtime_id))
            .and_then(|entry| {
                entry
                    .get("playerName")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if name.is_some() || Instant::now() >= deadline {
            return name;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn server_player_names(bridge: &BridgeServer, server: &str) -> Result<Vec<String>> {
    let listed = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({ "listPlayers": true }),
        BridgeTarget::Server,
        server,
        Some(Duration::from_secs(2)),
    )?;
    ensure_plugin_api_ok(&listed)?;
    if listed["action"] != "players" {
        bail!(OLDER_PLUGIN);
    }
    Ok(listed["players"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
}

fn wait_for_server_to_drop(
    bridge: &BridgeServer,
    server: &str,
    player: &str,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + LEAVE_TIMEOUT;
    loop {
        let names = server_player_names(bridge, server)?;
        if !names.iter().any(|name| name == player) {
            return Ok(names);
        }
        if Instant::now() >= deadline {
            bail!(
                "{player} is still in the server's Players {} s after the kick; end the test with rbx play -x",
                LEAVE_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn wait_for_channel_to_close(bridge: &BridgeServer, runtime_id: &str) -> bool {
    let deadline = Instant::now() + LEAVE_TIMEOUT;
    while runtime_ids(&bridge.list_bridge_clients()).contains(runtime_id) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
    true
}

// A freshly launched server bridge connects before its scripts can run; the
// first Luau call would otherwise be the one that discovers that.
fn server_answers_luau(bridge: &BridgeServer, wait: PollWait<'_>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ready = call_execute_luau(
            bridge,
            BridgeTarget::Main,
            None,
            "return true",
            "ReniumReady",
            5.0,
            2.0,
        )
        .is_ok_and(|result| result.get("ok").and_then(Value::as_bool) == Some(true));
        if ready {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        wait(Duration::from_millis(500));
    }
}

fn new_play_launch(
    bridge: &BridgeServer,
    label: &str,
    edit_runtime_id: &str,
) -> Result<TestLaunch> {
    #[cfg(windows)]
    crate::project::workflows::windows_launch::protect_process(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id)?,
    )?;
    #[cfg(target_os = "macos")]
    crate::studio::native::serializer::protect_studio_launch(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id)?,
    )?;
    let sequence = bridge
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(TestLaunch {
        nonce: format!(
            "{label}-{}-{}-{sequence}",
            std::process::id(),
            current_millis()
        ),
        edit_runtime_id: edit_runtime_id.to_string(),
    })
}

fn cancel_test_launch_best_effort(bridge: &BridgeServer, launch: &TestLaunch, wait: PollWait<'_>) {
    request_play_runtimes_to_stop(
        bridge,
        &launch.edit_runtime_id,
        &test_launch_clients(bridge, launch),
        Some(&launch.nonce),
    );
    let _ = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({
            "stop": true,
            "launchNonce": launch.nonce,
            "waitForStopped": false,
        }),
        BridgeTarget::Edit,
        &launch.edit_runtime_id,
        Some(Duration::from_secs(2)),
    );
    close_leftover_test_processes(
        bridge,
        &launch.edit_runtime_id,
        Duration::from_secs(15),
        wait,
    );
}

fn multiplayer_start_deadline(started: Instant, last_progress: Instant) -> Instant {
    (last_progress + Duration::from_secs(60))
        .max(started + Duration::from_secs(90))
        .min(started + Duration::from_secs(240))
}

fn leftover_test_processes(edit_pid: u32) -> Vec<u32> {
    test_processes::test_descendants(&test_processes::studio_process_table(), edit_pid)
}

#[cfg(any(windows, target_os = "macos"))]
fn terminate_test_process(pid: u32) -> Result<()> {
    crate::studio::input::terminate_studio_process(pid)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn terminate_test_process(_pid: u32) -> Result<()> {
    Ok(())
}

fn close_leftover_test_processes(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    grace: Duration,
    wait: PollWait<'_>,
) -> Vec<u32> {
    let Ok(edit_pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id) else {
        return Vec::new();
    };
    let deadline = Instant::now() + grace;
    let mut leftover = leftover_test_processes(edit_pid);
    while !leftover.is_empty() && Instant::now() < deadline {
        wait(Duration::from_millis(250));
        leftover = leftover_test_processes(edit_pid);
    }
    let mut closed = Vec::new();
    for pid in leftover {
        match terminate_test_process(pid) {
            Ok(()) => closed.push(pid),
            Err(error) => log_global(
                5,
                format_args!("[renium] Studio test process {pid} could not be closed: {error:#}"),
            ),
        }
    }
    if !closed.is_empty() {
        log_global(
            5,
            format_args!("[renium] closed leftover Studio test processes: {closed:?}"),
        );
    }
    closed
}

fn edit_client<'a>(clients: &'a [Value], runtime_id: &str) -> Option<&'a Value> {
    clients.iter().find(|client| {
        client["role"] == BRIDGE_ROLE_EDIT
            && client.get("runtimeId").and_then(Value::as_str) == Some(runtime_id)
    })
}

/// Orphaned Studio test processes for `rbx cs` and `rbx status`. Given an Edit
/// runtime, connected orphans of other places are left out.
pub(crate) fn orphan_summaries(clients: &[Value], edit_runtime_id: Option<&str>) -> Vec<Value> {
    let edit = edit_runtime_id.and_then(|runtime_id| edit_client(clients, runtime_id));
    test_processes::orphan_test_processes(&test_processes::studio_process_table(), clients)
        .iter()
        .filter(|orphan| match (&orphan.client, edit) {
            (Some(client), Some(edit)) => test_processes::same_place(client, edit),
            _ => true,
        })
        .map(test_processes::Orphan::summary)
        .collect()
}

fn close_orphans(
    bridge: &BridgeServer,
    orphans: &[test_processes::Orphan],
) -> (Vec<u32>, Vec<Value>) {
    let mut closed = Vec::new();
    let mut failed = Vec::new();
    for orphan in orphans {
        match terminate_test_process(orphan.pid) {
            Ok(()) => {
                closed.push(orphan.pid);
                if let Some(runtime_id) = orphan.runtime_id() {
                    bridge.retire_runtime(runtime_id);
                }
            }
            Err(error) => {
                failed.push(json!({ "pid": orphan.pid, "error": format!("{error:#}") }));
            }
        }
    }
    if !closed.is_empty() {
        log_global(
            5,
            format_args!("[renium] closed orphaned Studio test processes: {closed:?}"),
        );
    }
    (closed, failed)
}

/// An orphaned test process knows its place only by the file name its Edit
/// window stamped on it, so a place path (`RENIUM_PLACE=<dir>\Scenario.rbxl`)
/// matches it by that name, as the Edit window's own orphan sweep does.
fn client_place_matches(client: &Value, selector: &str) -> bool {
    let place_name = client
        .get("placeName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let wanted = selector.trim().replace('\\', "/");
    let file_selector = wanted
        .rsplit_once('/')
        .map(|(_, name)| name)
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            !place_name.contains(['/', '\\'])
                && (lower.ends_with(".rbxl") || lower.ends_with(".rbxlx"))
        });
    crate::studio::target::place_matches(
        &crate::studio::bridge::BridgeInfoPayload {
            place_id: client.get("placeId").and_then(Value::as_i64),
            game_id: client.get("gameId").and_then(Value::as_i64),
            place_name,
            ..Default::default()
        },
        file_selector.unwrap_or(selector),
    )
}

/// `rbx play --kill-orphans`: closes every orphaned test process, or with a
/// place selector the connected ones of matching places.
pub(crate) fn kill_orphans_result(bridge: &BridgeServer, place: Option<&str>) -> Value {
    let clients = bridge.list_bridge_clients();
    let orphans =
        test_processes::orphan_test_processes(&test_processes::studio_process_table(), &clients)
            .into_iter()
            .filter(|orphan| {
                place.is_none_or(|selector| {
                    orphan
                        .client
                        .as_ref()
                        .is_some_and(|client| client_place_matches(client, selector))
                })
            })
            .collect::<Vec<_>>();
    let (closed, failed) = close_orphans(bridge, &orphans);
    let mut result = json!({ "ok": failed.is_empty(), "action": "killOrphans", "closed": closed });
    if !failed.is_empty() {
        result["failed"] = json!(failed);
    }
    result
}

fn close_same_place_orphans(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
) -> (Vec<u32>, Vec<Value>) {
    let clients = bridge.list_bridge_clients();
    let Some(edit) = edit_client(&clients, edit_runtime_id) else {
        return Default::default();
    };
    let orphans =
        test_processes::orphan_test_processes(&test_processes::studio_process_table(), &clients)
            .into_iter()
            .filter(|orphan| {
                orphan
                    .client
                    .as_ref()
                    .is_some_and(|client| test_processes::same_place(client, edit))
            })
            .collect::<Vec<_>>();
    close_orphans(bridge, &orphans)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayLaunchPlan {
    Play,
    Run,
    Multi(u32),
}

impl PlayLaunchPlan {
    fn label(self) -> &'static str {
        match self {
            Self::Multi(_) => "multi",
            Self::Play | Self::Run => "play",
        }
    }

    fn plugin_mode(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Run => "run",
            Self::Multi(_) => "multi",
        }
    }

    fn session_kind(self) -> &'static str {
        match self {
            Self::Multi(_) => "multiplayer",
            Self::Play | Self::Run => "play",
        }
    }

    fn players(self) -> Option<u32> {
        match self {
            Self::Multi(players) => Some(players),
            Self::Play | Self::Run => None,
        }
    }

    fn clients_needed(self) -> usize {
        match self {
            Self::Play => 1,
            Self::Run => 0,
            Self::Multi(players) => players as usize,
        }
    }

    fn request(self, launch_nonce: &str) -> Value {
        match self {
            Self::Multi(players) => {
                json!({ "start": true, "players": players, "launchNonce": launch_nonce })
            }
            Self::Play | Self::Run => {
                json!({ "start": true, "mode": self.plugin_mode(), "launchNonce": launch_nonce })
            }
        }
    }

    fn start_deadline(self, started: Instant, last_progress: Instant) -> Instant {
        match self {
            Self::Multi(_) => multiplayer_start_deadline(started, last_progress),
            Self::Play | Self::Run => started + SINGLE_START_TIMEOUT,
        }
    }

    /// How long a start may show no sign of the test at all.
    fn sign_timeout(self) -> Duration {
        match self {
            Self::Multi(_) => MULTI_START_SIGN_TIMEOUT,
            Self::Play | Self::Run => SINGLE_START_TIMEOUT,
        }
    }

    /// Whether Studio dropped the start: the call returned without the test
    /// appearing, or nothing showed up at all.
    fn start_was_ignored(self, elapsed: Duration, begun: bool, executing: Option<bool>) -> bool {
        !begun
            && (elapsed >= self.sign_timeout()
                || executing == Some(false) && elapsed >= START_RETURN_GRACE)
    }
}

// Studio drops a test start while the previous test is still ending or a place
// it just opened is still settling: the start call returns at once, or no test
// ever appears. Such a start is cancelled and requested again.
const START_ATTEMPTS: u32 = 3;
const START_RETRY_WINDOW: Duration = Duration::from_secs(60);
const START_RETURN_GRACE: Duration = Duration::from_secs(5);
const SINGLE_START_TIMEOUT: Duration = Duration::from_secs(20);
const MULTI_START_SIGN_TIMEOUT: Duration = Duration::from_secs(30);
const LEFTOVER_GRACE: Duration = Duration::from_secs(10);

enum LaunchOutcome {
    Started(Value),
    Ignored(String),
}

enum AttemptOutcome {
    AlreadyRunning(Value),
    Started { result: Value, requested: Instant },
    Ignored { reason: String, requested: Instant },
}

static LAUNCHED_PLANS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PlayLaunchPlan>>,
> = std::sync::LazyLock::new(Default::default);

fn remember_launch(edit_runtime_id: &str, plan: PlayLaunchPlan) {
    use crate::system::LockRecover;
    LAUNCHED_PLANS
        .lock_recover()
        .insert(edit_runtime_id.to_string(), plan);
}

fn remembered_launch(edit_runtime_id: &str) -> Option<PlayLaunchPlan> {
    use crate::system::LockRecover;
    LAUNCHED_PLANS.lock_recover().get(edit_runtime_id).copied()
}

/// Starts a test in the Edit window the request selected. That window is
/// resolved once, before the first wait; everything after it addresses the
/// window by that runtime id.
fn start_play_with_retries(
    bridge: &BridgeServer,
    plan: PlayLaunchPlan,
    wait: PollWait<'_>,
) -> Result<Value> {
    if plan
        .players()
        .is_some_and(|players| !(1..=8).contains(&players))
    {
        bail!("Multiplayer tests require between 1 and 8 players");
    }
    let edit_runtime_id = bridge
        .runtime_pin_for_selector(BridgeTarget::Edit, None)?
        .runtime_id;
    let (closed_orphans, orphan_errors) = close_same_place_orphans(bridge, &edit_runtime_id);
    let mut first_request: Option<Instant> = None;
    let mut last_reason = String::new();
    let mut attempts = 0;
    while attempts < START_ATTEMPTS
        && first_request.is_none_or(|first| first.elapsed() < START_RETRY_WINDOW)
    {
        attempts += 1;
        if attempts > 1 {
            wait(Duration::from_secs(u64::from(attempts - 1)));
        }
        let outcome = new_play_launch(bridge, plan.label(), &edit_runtime_id)
            .and_then(|launch| start_attempt(bridge, &launch, plan, wait))
            .map_err(|error| with_orphan_hint(bridge, error))?;
        let mut result = match outcome {
            AttemptOutcome::AlreadyRunning(existing) => already_running_result(existing),
            AttemptOutcome::Started {
                mut result,
                requested,
            } => {
                if let Some(first) = first_request {
                    result["retriedAfterMs"] = json!(requested.duration_since(first).as_millis());
                    result["startAttempts"] = json!(attempts);
                    result["retryReason"] = json!(last_reason);
                }
                result
            }
            AttemptOutcome::Ignored { reason, requested } => {
                log_global(
                    5,
                    format_args!(
                        "[renium] Studio did not start the test (attempt {attempts}): {reason}"
                    ),
                );
                first_request.get_or_insert(requested);
                last_reason = reason;
                continue;
            }
        };
        if !closed_orphans.is_empty() {
            result["closedOrphans"] = json!(closed_orphans);
        }
        if !orphan_errors.is_empty() {
            result["orphanErrors"] = json!(orphan_errors);
        }
        return Ok(result);
    }
    Err(with_orphan_hint(
        bridge,
        anyhow::anyhow!(
            "Studio did not start the {} session after {attempts} attempts: {last_reason}. Check the Edit window for a dialog, or run rbx status",
            plan.session_kind(),
        ),
    ))
}

const ALREADY_RUNNING_NOTE: &str =
    "nothing was started: state from earlier calls remains; rbx play -r starts fresh";

/// The answer of a start that found a session running: the controller's
/// status, marked so the caller does not take what follows for a fresh
/// session.
fn already_running_result(mut status: Value) -> Value {
    status["alreadyRunning"] = json!(true);
    status["startNote"] = json!(ALREADY_RUNNING_NOTE);
    status
}

fn with_orphan_hint(bridge: &BridgeServer, error: anyhow::Error) -> anyhow::Error {
    let orphans = test_processes::orphan_test_processes(
        &test_processes::studio_process_table(),
        &bridge.list_bridge_clients(),
    );
    if orphans.is_empty() {
        return error;
    }
    anyhow::anyhow!(
        "{error:#}. {} orphaned Studio test window(s) whose Edit window has exited are still open; rbx play --kill-orphans closes them",
        orphans.len()
    )
}

/// One start of `launch` in its Edit window, addressed by the launch's
/// runtime id throughout.
fn start_attempt(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
    wait: PollWait<'_>,
) -> Result<AttemptOutcome> {
    // Device status decorates the result; a stalled device service must not
    // stop Play from starting.
    let mut device_simulation = studio_device_status(bridge, Some(&launch.edit_runtime_id))
        .unwrap_or_else(|error| json!({ "error": format!("{error:#}") }));
    let waited = Instant::now();
    let mut existing = wait_for_studio_play_ready(bridge, &launch.edit_runtime_id, wait)?;
    if play_status_is_running(&existing) {
        if plan.players().is_some() {
            bail!("A Studio play session is already active in the selected window");
        }
        existing["deviceSimulation"] = device_simulation;
        return Ok(AttemptOutcome::AlreadyRunning(existing));
    }
    let closed =
        close_leftover_test_processes(bridge, &launch.edit_runtime_id, LEFTOVER_GRACE, wait);
    let waited_ms = waited.elapsed().as_millis();
    let requested = Instant::now();
    match launch_play_session(bridge, launch, plan, wait) {
        Ok(LaunchOutcome::Started(mut result)) => {
            remember_launch(&launch.edit_runtime_id, plan);
            device_simulation["playRunning"] = Value::Bool(true);
            result["deviceSimulation"] = device_simulation;
            if waited_ms >= 500 {
                result["waitedForStudioMs"] = json!(waited_ms);
            }
            if !closed.is_empty() {
                result["closedProcesses"] = json!(closed);
            }
            Ok(AttemptOutcome::Started { result, requested })
        }
        Ok(LaunchOutcome::Ignored(reason)) => {
            cancel_test_launch_best_effort(bridge, launch, wait);
            Ok(AttemptOutcome::Ignored { reason, requested })
        }
        Err(error) => {
            cancel_test_launch_best_effort(bridge, launch, wait);
            Err(error)
        }
    }
}

/// One run-mode start attempt against fixture bridges, which have no Studio
/// process whose launch could be protected.
#[cfg(test)]
pub(crate) fn start_fixture_launch(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    wait: PollWait<'_>,
) -> Result<Value> {
    match start_attempt(bridge, launch, PlayLaunchPlan::Run, wait)? {
        AttemptOutcome::Started { result, .. } => Ok(result),
        AttemptOutcome::AlreadyRunning(status) => bail!("a session already runs: {status}"),
        AttemptOutcome::Ignored { reason, .. } => bail!("{reason}"),
    }
}

/// Watches for any sign that Studio took a start: the Edit controller entering
/// the test, a new Studio process under the Edit window, or a test bridge.
struct LaunchWatch {
    edit_pid: Option<u32>,
    baseline: HashSet<u32>,
    begun: bool,
}

fn studio_child_pids(edit_pid: u32) -> HashSet<u32> {
    test_processes::studio_descendants(&test_processes::studio_process_table(), edit_pid)
        .iter()
        .map(|process| process.pid)
        .collect()
}

impl LaunchWatch {
    fn new(bridge: &BridgeServer, launch: &TestLaunch) -> Self {
        let edit_pid = bridge
            .studio_pid_for_runtime(BridgeTarget::Edit, &launch.edit_runtime_id)
            .ok();
        Self {
            edit_pid,
            baseline: edit_pid.map(studio_child_pids).unwrap_or_default(),
            begun: false,
        }
    }

    fn observe(&mut self, status: Option<&Value>, clients: &[Value]) {
        self.begun = self.begun
            || !clients.is_empty()
            || status.is_some_and(|status| {
                status["engineRunning"] == true || status["engineStarted"] == true
            })
            || self
                .edit_pid
                .is_some_and(|pid| !studio_child_pids(pid).is_subset(&self.baseline));
    }
}

fn launch_status_outcome(
    status: &Value,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
    begun: bool,
) -> Result<Option<LaunchOutcome>> {
    if let Some(error) = status
        .get("lastError")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    {
        if !begun {
            return Ok(Some(LaunchOutcome::Ignored(format!(
                "Studio refused the start: {error}"
            ))));
        }
        bail!(
            "Studio could not start the {} session: {error}",
            plan.session_kind()
        );
    }
    if status.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
        bail!(
            "Studio switched to a different {} session while starting",
            plan.session_kind()
        );
    }
    ensure_plugin_api_ok(status)?;
    Ok(None)
}

fn started_result(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
    clients: Vec<Value>,
) -> Value {
    let mut result = json!({
        "ok": true,
        "action": "start",
        "mode": plan.plugin_mode(),
        "launchNonce": launch.nonce,
        "editRuntimeId": launch.edit_runtime_id,
        "editPid": bridge.studio_pid_for_runtime(BridgeTarget::Edit, &launch.edit_runtime_id).ok(),
        "clients": clients,
    });
    if let Some(players) = plan.players() {
        result["players"] = json!(players);
        result["note"] = json!(multi_client_note(players));
    }
    result
}

fn multi_client_note(players: u32) -> String {
    format!(
        "local server plus {players} client windows are running; one-client checks use ordinary Play (rbx play -s), and rbx play -x ends this session"
    )
}

fn start_timeout_error(
    bridge: &BridgeServer,
    plan: PlayLaunchPlan,
    progress: (bool, usize),
    start_result: &Value,
    last_status: &Value,
    clients: &[Value],
) -> anyhow::Error {
    let (server_ready, client_count) = progress;
    let text = |value: &Value| serde_json::to_string(value).unwrap_or_default();
    match plan {
        PlayLaunchPlan::Multi(players) => anyhow::anyhow!(
            "Timed out waiting for the multiplayer test instances to connect \
             (server ready: {server_ready}, clients connected: {client_count}/{players}). \
             Start request result: {start_result}; connected bridges: {}",
            text(&json!(clients))
        ),
        PlayLaunchPlan::Play | PlayLaunchPlan::Run => anyhow::anyhow!(
            "Timed out waiting for the play session to start; last status: {}, connected bridges: {}",
            text(last_status),
            text(&json!(bridge.list_bridge_clients()))
        ),
    }
}

fn launch_play_session(
    bridge: &BridgeServer,
    launch: &TestLaunch,
    plan: PlayLaunchPlan,
    wait: PollWait<'_>,
) -> Result<LaunchOutcome> {
    let mut watch = LaunchWatch::new(bridge, launch);
    let start_result = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        plan.request(&launch.nonce),
        BridgeTarget::Edit,
        &launch.edit_runtime_id,
        None,
    )?;
    if start_result.get("launchNonce").and_then(Value::as_str) != Some(launch.nonce.as_str()) {
        bail!("Studio started a different {} session", plan.session_kind());
    }
    if start_result.get("ok").and_then(Value::as_bool) == Some(false)
        && start_result.get("starting").and_then(Value::as_bool) != Some(true)
    {
        ensure_plugin_api_ok(&start_result)?;
    }
    let started = Instant::now();
    let mut last_progress = started;
    let mut last_seen = (false, 0usize);
    loop {
        let status = studio_play_status_for_runtime(bridge, &launch.edit_runtime_id);
        let last_status = match &status {
            Ok(status) => {
                if let Some(outcome) = launch_status_outcome(status, launch, plan, watch.begun)? {
                    return Ok(outcome);
                }
                status.clone()
            }
            Err(error) => json!({ "error": format!("{error:#}") }),
        };
        let clients = test_launch_clients(bridge, launch);
        let server_ready = clients
            .iter()
            .any(|entry| entry["role"] == BRIDGE_ROLE_PLAY_SERVER);
        let client_count = clients
            .iter()
            .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
            .count();
        if server_ready && client_count >= plan.clients_needed() {
            return Ok(LaunchOutcome::Started(started_result(
                bridge, launch, plan, clients,
            )));
        }
        watch.observe(status.as_ref().ok(), &clients);
        let executing = status
            .as_ref()
            .ok()
            .and_then(|status| status.get("executing"))
            .and_then(Value::as_bool);
        if plan.start_was_ignored(started.elapsed(), watch.begun, executing) {
            return Ok(LaunchOutcome::Ignored(if executing == Some(false) {
                "Studio returned from the start without opening the test".to_string()
            } else {
                format!(
                    "no sign of the test {} s after the start request",
                    plan.sign_timeout().as_secs()
                )
            }));
        }
        if (server_ready, client_count) != last_seen {
            last_seen = (server_ready, client_count);
            last_progress = Instant::now();
        }
        if Instant::now() >= plan.start_deadline(started, last_progress) {
            return Err(start_timeout_error(
                bridge,
                plan,
                last_seen,
                &start_result,
                &last_status,
                &clients,
            ));
        }
        wait(Duration::from_millis(250));
    }
}

/// The way a running session was launched, from the Edit window's play
/// controller; None when Renium did not launch it.
fn running_session_plan(
    status: &Value,
    clients: &[Value],
    remembered: Option<PlayLaunchPlan>,
) -> Option<PlayLaunchPlan> {
    status
        .get("launchNonce")
        .and_then(Value::as_str)
        .filter(|nonce| !nonce.is_empty())?;
    match status.get("mode").and_then(Value::as_str)? {
        "play" => Some(PlayLaunchPlan::Play),
        "run" => Some(PlayLaunchPlan::Run),
        "multi" => status
            .get("players")
            .and_then(Value::as_u64)
            .and_then(|players| u32::try_from(players).ok())
            .or_else(|| remembered.and_then(PlayLaunchPlan::players))
            .or_else(|| {
                let connected = clients
                    .iter()
                    .filter(|entry| entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
                    .count();
                u32::try_from(connected).ok().filter(|count| *count > 0)
            })
            .map(PlayLaunchPlan::Multi),
        _ => None,
    }
}

const RESTART_OUTSIDE_RENIUM: &str = "The running session was not started by Renium, so its mode and player count are unknown; it restarted as ordinary Play (add -p N or --mode to choose)";
const RESTART_NOTHING_KNOWN: &str = "No session was running and none started by this Renium daemon is known for this Edit window; it started ordinary Play (add -p N or --mode to choose)";

fn restart_plan(
    args: &StartStopPlayArgs,
    previous: Option<PlayLaunchPlan>,
    running: bool,
) -> (PlayLaunchPlan, Option<&'static str>) {
    if let Some(players) = args.players {
        return (PlayLaunchPlan::Multi(players), None);
    }
    if let Some(mode) = args.mode.as_deref() {
        let plan = if matches!(mode, "run" | "server") {
            PlayLaunchPlan::Run
        } else {
            PlayLaunchPlan::Play
        };
        return (plan, None);
    }
    match previous {
        Some(plan) => (plan, None),
        None if running => (PlayLaunchPlan::Play, Some(RESTART_OUTSIDE_RENIUM)),
        None => (PlayLaunchPlan::Play, Some(RESTART_NOTHING_KNOWN)),
    }
}

fn restart_play_result(
    args: StartStopPlayArgs,
    bridge: &BridgeServer,
    wait: PollWait<'_>,
) -> Result<Value> {
    let edit_runtime_id = bridge
        .runtime_pin_for_selector(BridgeTarget::Edit, None)?
        .runtime_id;
    let status = studio_play_status_for_runtime(bridge, &edit_runtime_id)?;
    let (clients, _) = play_clients_by_state(bridge, &edit_runtime_id);
    let running = play_status_is_running(&status) || !clients.is_empty();
    let remembered = remembered_launch(&edit_runtime_id);
    let previous = if running {
        running_session_plan(&status, &clients, remembered)
    } else {
        remembered
    };
    let (plan, note) = restart_plan(&args, previous, running);
    let restarted = running
        && stop_studio_play_with_bridge_result(bridge)?
            .get("ok")
            .and_then(Value::as_bool)
            == Some(true);
    let mut result = start_play_with_retries(bridge, plan, wait)?;
    result["serverReady"] = json!(server_answers_luau(bridge, wait));
    result["restarted"] = json!(restarted);
    if let Some(note) = note {
        result["restartNote"] = json!(note);
    } else if let Some(players) = plan.players() {
        result["restartNote"] = json!(format!(
            "restarted the {players}-client session: {}",
            multi_client_note(players)
        ));
    }
    Ok(result)
}

#[cfg(windows)]
fn resolve_player_window(
    bridge: &BridgeServer,
    player: Option<&str>,
    viewport: Option<(i32, i32)>,
) -> Result<(input_inject::StudioWindow, i32, i32)> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
    let viewport = recover_client_viewport(bridge, player, pid, viewport)?;
    let window = input_inject::window_for_pid(pid, viewport)?;
    Ok((window, 0, 0))
}

fn client_viewport_size(bridge: &BridgeServer, player: Option<&str>) -> Option<(i32, i32)> {
    let result = bridge
        .call_for_selector("getMouseLocation", json!({}), BridgeTarget::Client, player)
        .ok()?;
    let width = result.get("viewportWidth").and_then(Value::as_f64)?;
    let height = result.get("viewportHeight").and_then(Value::as_f64)?;
    Some((width.round() as i32, height.round() as i32))
}

#[cfg(any(windows, target_os = "macos"))]
fn result_viewport_size(result: &Value) -> Option<(i32, i32)> {
    let width = result.get("viewportWidth").and_then(Value::as_f64)?;
    let height = result.get("viewportHeight").and_then(Value::as_f64)?;
    Some((width.round() as i32, height.round() as i32))
}

#[cfg(windows)]
fn recover_client_viewport(
    bridge: &BridgeServer,
    player: Option<&str>,
    pid: u32,
    mut viewport: Option<(i32, i32)>,
) -> Result<Option<(i32, i32)>> {
    if !viewport.is_some_and(|(width, height)| width <= 1 || height <= 1) {
        return Ok(viewport);
    }
    input_inject::recover_stalled_window_for_pid(pid)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
        viewport = client_viewport_size(bridge, player);
        if viewport.is_some_and(|(width, height)| width > 1 && height > 1) {
            break;
        }
    }
    Ok(viewport)
}

#[cfg(windows)]
fn input_delta(
    _bridge: &BridgeServer,
    _player: Option<&str>,
    _window: &input_inject::StudioWindow,
    _x: i32,
    _y: i32,
) -> (i32, i32) {
    (0, 0)
}

#[cfg(target_os = "macos")]
fn recover_client_viewport(
    _bridge: &BridgeServer,
    _player: Option<&str>,
    _pid: u32,
    viewport: Option<(i32, i32)>,
) -> Result<Option<(i32, i32)>> {
    Ok(viewport)
}

#[cfg(any(windows, target_os = "macos"))]
fn resolve_client_capture_window(
    bridge: &BridgeServer,
    player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
    let edit_pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    if pid == edit_pid {
        return resolve_edit_window(bridge, BridgeTarget::Client);
    }
    let viewport = client_viewport_size(bridge, player);
    let viewport = recover_client_viewport(bridge, player, pid, viewport)?;
    input_inject::window_for_pid(pid, viewport)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn resolve_client_capture_window(
    _bridge: &BridgeServer,
    _player: Option<&str>,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio screenshots are only supported on Windows and macOS")
}

/// Pointer input goes through the game's own virtual input: it moves the
/// game's mouse, so hover, `Mouse.Target` and ClickDetector events follow,
/// and the cursor and window focus stay untouched. RENIUM_INPUT_OS restores
/// window-level injection on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn os_input_preferred() -> bool {
    cfg!(windows) && std::env::var_os("RENIUM_INPUT_OS").is_some_and(|value| value != "0")
}

/// Copies the plugin's note that a pointer event was delivered through a
/// hidden system element, where a player could not have clicked.
fn note_system_ui(result: &mut Value, response: &Value) {
    if let Some(through) = response
        .get("throughSystemUi")
        .filter(|value| !value.is_null())
    {
        result["throughSystemUi"] = through.clone();
    }
}

fn send_virtual_input(
    bridge: &BridgeServer,
    player: Option<&str>,
    actions: Vec<Value>,
    expect_activated_id: Option<&str>,
) -> Result<Value> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _shield = {
        let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
        let window = input_inject::window_for_pid(pid, client_viewport_size(bridge, player))?;
        input_inject::input_shield(&window)?
    };
    let batches = if expect_activated_id.is_some() {
        vec![actions]
    } else {
        split_virtual_input(actions)
    };
    let mut merged = None;
    let mut held = HashSet::new();
    for batch in batches {
        // A click that follows another action is asked again until its
        // target is up: a panel the earlier action opened needs no wait
        // written before it. The first action has nothing to wait for, and
        // a refusal lets go of every held key, so nothing may be held: not by
        // this request, nor by an earlier one, which the answers so far name.
        let keys_held = merged
            .as_ref()
            .and_then(|answer: &Value| answer.get("heldKeys"))
            .and_then(Value::as_array)
            .is_some_and(|keys| !keys.is_empty());
        let awaited = (expect_activated_id.is_none() && held.is_empty() && !keys_held)
            .then(|| batch.first().filter(|action| action["type"] == "click"))
            .flatten()
            .map(|click| click["step"].as_u64())
            .filter(|step| step.is_some_and(|step| step > 1))
            .map(|step| (step, Instant::now() + CLICK_TARGET_WAIT));
        batch
            .iter()
            .for_each(|action| note_held_input(&mut held, action));
        let mut params = json!({ "actions": batch });
        if let Some(id) = expect_activated_id {
            params["expectActivatedId"] = Value::String(id.to_string());
        }
        let result = loop {
            let result = bridge.call_for_selector(
                "sendVirtualInput",
                params.clone(),
                BridgeTarget::Client,
                player,
            )?;
            let Some(failure) = virtual_input_failure(&result) else {
                break result;
            };
            let waits = awaited.is_some_and(|(step, deadline)| {
                result.get("failedStep").and_then(Value::as_u64) == step
                    && click_target_may_appear(&failure.message)
                    && Instant::now() < deadline
            });
            if !waits {
                return Err(failure.into());
            }
            thread::sleep(CLICK_TARGET_POLL);
        };
        merged = Some(match merged {
            None => result,
            Some(previous) => merge_virtual_input_results(previous, result),
        });
    }
    merged.context("Virtual input needs at least one action")
}

/// Virtual input the client refused: why, with what can be clicked instead
/// when a click target was hidden or covered, and the `step` that click's
/// command carried.
#[derive(Debug)]
pub(super) struct VirtualInputFailure {
    pub(super) message: String,
    pub(super) step: Option<usize>,
}

impl std::fmt::Display for VirtualInputFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for VirtualInputFailure {}

fn virtual_input_failure(result: &Value) -> Option<VirtualInputFailure> {
    if result.get("ok").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let message = result
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("Plugin API returned ok=false");
    Some(VirtualInputFailure {
        message: with_clickable_now(message, result),
        step: result
            .get("failedStep")
            .and_then(Value::as_u64)
            .map(|step| step as usize),
    })
}

const CLICKABLE_NOW_PATHS: usize = 12;
const CLICKABLE_NOW_CHARS: usize = 600;

/// `message` followed by the GUI paths the client says take a click now
/// (`source.clickable`, nearest to the refused target first), so a retry
/// needs no `ui` call: at most twelve paths and about 600 characters. The
/// message is returned as it is when nothing is clickable.
fn with_clickable_now(message: &str, source: &Value) -> String {
    let mut listed = String::new();
    let paths = source
        .get("clickable")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(CLICKABLE_NOW_PATHS);
    for path in paths {
        let separator = if listed.is_empty() { "" } else { ", " };
        if listed.len() + separator.len() + path.len() > CLICKABLE_NOW_CHARS {
            break;
        }
        listed.push_str(separator);
        listed.push_str(path);
    }
    if listed.is_empty() {
        return message.to_string();
    }
    format!("{message}. Clickable now: {listed}")
}

/// How long a click waits for its GUI target to exist, show and be reachable.
const CLICK_TARGET_WAIT: Duration = Duration::from_secs(2);
const CLICK_TARGET_POLL: Duration = Duration::from_millis(150);

/// Whether a click was refused before it was sent for a reason that time can
/// mend: the element is not there yet, not shown yet or still covered.
fn click_target_may_appear(message: &str) -> bool {
    [
        " is not visible: ",
        " is covered by ",
        " not found under ",
        "PlayerGui is not available",
        "LocalPlayer is not available",
    ]
    .iter()
    .any(|reason| message.contains(reason))
}

/// Keeps `held` as the keys and buttons a batch leaves down.
fn note_held_input(held: &mut HashSet<String>, action: &Value) {
    let name = match action["type"].as_str() {
        Some("key") => format!("key {}", action["key"]),
        Some("button") => format!("button {}", action["button"]),
        _ => return,
    };
    if action["down"] == true {
        held.insert(name);
    } else {
        held.remove(&name);
    }
}

/// One bridge call answers within thirty seconds, so a batch whose waits
/// add up to more than this goes to the client as consecutive calls; keys
/// and buttons held at the end of a call stay held into the next one. A
/// verified click starts a call of its own while nothing is held, so a click
/// that has to wait for its target is asked again with nothing run twice.
const MAX_VIRTUAL_CALL_WAIT_MS: u64 = 25_000;

fn split_virtual_input(actions: Vec<Value>) -> Vec<Vec<Value>> {
    let mut batches: Vec<Vec<Value>> = vec![Vec::new()];
    let mut waited = 0;
    let mut held = HashSet::new();
    for action in actions {
        let wait = if action["type"] == "wait" {
            action["ms"].as_u64().unwrap_or(0)
        } else {
            action["holdMs"].as_u64().unwrap_or(0)
        };
        let awaited_click = action["type"] == "click" && held.is_empty();
        if (awaited_click || waited + wait > MAX_VIRTUAL_CALL_WAIT_MS)
            && !batches.last().is_some_and(Vec::is_empty)
        {
            batches.push(Vec::new());
            waited = 0;
        }
        note_held_input(&mut held, &action);
        waited += wait;
        if let Some(batch) = batches.last_mut() {
            batch.push(action);
        }
    }
    batches
}

/// The answer for a batch sent as several calls: counts add up, the observed
/// keys accumulate and the last call reports what is still held.
fn merge_virtual_input_results(previous: Value, last: Value) -> Value {
    let mut merged = last;
    let count = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64);
    merged["actions"] =
        json!(count(&previous, "actions").unwrap_or(0) + count(&merged, "actions").unwrap_or(0));
    for key in ["verifiedClicks", "unconfirmedClicks"] {
        if let Some(clicks) = count(&previous, key) {
            merged[key] = json!(clicks + count(&merged, key).unwrap_or(0));
        }
    }
    if merged.get("coreGui").is_none_or(Value::is_null)
        && let Some(report) = previous.get("coreGui").filter(|report| !report.is_null())
    {
        merged["coreGui"] = report.clone();
    }
    if previous.get("textObserved") == Some(&Value::Bool(false)) {
        merged["textObserved"] = json!(false);
    }
    if let Some(observed) = previous.get("keysObserved").and_then(Value::as_object) {
        let current = merged["keysObserved"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut all = observed.clone();
        all.extend(current);
        merged["keysObserved"] = Value::Object(all);
    }
    if let Some(through) = previous.get("throughSystemUi").and_then(Value::as_array) {
        let mut all = through.clone();
        all.extend(
            merged["throughSystemUi"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
        merged["throughSystemUi"] = Value::Array(all);
    }
    merged
}

fn virtual_click_actions(
    x: i32,
    y: i32,
    right: bool,
    hold_ms: u64,
    move_pointer: bool,
) -> Vec<Value> {
    let button = if right { "right" } else { "left" };
    let mut actions = Vec::with_capacity(if move_pointer { 6 } else { 4 });
    if move_pointer {
        actions.extend([
            json!({ "type": "move", "x": x, "y": y }),
            json!({ "type": "wait", "ms": 0 }),
        ]);
    }
    actions.extend([
        json!({ "type": "button", "x": x, "y": y, "button": button, "down": true }),
        json!({ "type": "wait", "ms": hold_ms.min(10_000) }),
        json!({ "type": "button", "x": x, "y": y, "button": button, "down": false }),
        json!({ "type": "wait", "ms": 0 }),
    ]);
    actions
}

/// The device simulation state of `edit_runtime_id`'s Edit window, or of the
/// selected one when none is named.
fn studio_device_status(bridge: &BridgeServer, edit_runtime_id: Option<&str>) -> Result<Value> {
    let result = bridge.call_for_selector_runtime_with_timeout(
        "deviceSimulator",
        json!({ "action": "capture-status", "details": true, "includeSettle": true }),
        BridgeTarget::Edit,
        None,
        edit_runtime_id,
        None,
    )?;
    ensure_plugin_api_ok(&result)?;
    Ok(result)
}

fn studio_capture_status(bridge: &BridgeServer) -> Option<Value> {
    studio_device_status(bridge, None).ok()
}

#[cfg(windows)]
fn set_capture_probe_phase(
    bridge: &BridgeServer,
    target: BridgeTarget,
    phase: u8,
    colors: &[u32],
) -> Result<()> {
    let action = match phase {
        0 => "start",
        1 => "phase",
        2 => "stop",
        _ => bail!("Invalid capture probe phase {phase}"),
    };
    let result = bridge.call_for_target(
        "captureViewportProbe",
        json!({ "action": action, "colors": colors }),
        target,
    )?;
    ensure_plugin_api_ok(&result)
}

#[cfg(windows)]
fn resolve_edit_window(
    bridge: &BridgeServer,
    probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::verified_studio_window_for_pid(pid, |phase, colors| {
        set_capture_probe_phase(bridge, probe_target, phase, colors)
    })
}

#[cfg(target_os = "macos")]
fn resolve_edit_window(
    bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    let pid = bridge.studio_pid_for_selector(BridgeTarget::Edit, None)?;
    input_inject::window_for_pid(pid, None)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn resolve_edit_window(
    _bridge: &BridgeServer,
    _probe_target: BridgeTarget,
) -> Result<input_inject::StudioWindow> {
    bail!("Studio screenshots are only supported on Windows and macOS")
}

/// Why the element `bounds` describes cannot take a click, or None when it
/// can: off screen, not visible (with the plugin's reason), or covered.
fn gui_click_refusal(bounds: &Value, requested: &str) -> Option<String> {
    let flag = |key: &str| bounds.get(key).and_then(Value::as_bool);
    let subject = bounds
        .get("fullName")
        .and_then(Value::as_str)
        .unwrap_or(requested);
    if flag("onScreen") == Some(false) {
        return Some(format!(
            "{subject} could not be brought on screen (auto-scroll was attempted; it is clipped \
             by a non-scrolling container or positioned outside the viewport)"
        ));
    }
    if let Some(reason) = bounds.get("notVisible").and_then(Value::as_str) {
        let path = bounds.get("ordinalPath").and_then(Value::as_str);
        return Some(format!(
            "GUI element {} is not visible: {reason}",
            path.unwrap_or(subject)
        ));
    }
    if flag("visible") == Some(false) {
        return Some(format!("GUI element {subject} is not visible"));
    }
    if flag("hitTest") == Some(false) {
        return Some(match bounds.get("blockedBy").and_then(Value::as_str) {
            Some(blocker) => format!("GUI element {subject} is covered by {blocker}"),
            None => {
                format!("GUI element {subject} is not receiving pointer input at its center")
            }
        });
    }
    None
}

fn gui_input_bounds(
    bridge: &BridgeServer,
    player: Option<&str>,
    path: Option<&str>,
    id: Option<&str>,
    requested: &str,
) -> Result<(Value, f64, f64)> {
    let read = || {
        bridge.call_for_selector(
            "getGuiBounds",
            json!({ "path": path, "id": id, "scroll": true }),
            BridgeTarget::Client,
            player,
        )
    };
    let bounds = read()?;
    ensure_plugin_api_ok(&bounds)?;
    #[cfg(any(windows, target_os = "macos"))]
    let bounds =
        if result_viewport_size(&bounds).is_some_and(|(width, height)| width <= 1 || height <= 1) {
            let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
            recover_client_viewport(bridge, player, pid, result_viewport_size(&bounds))?;
            let bounds = read()?;
            ensure_plugin_api_ok(&bounds)?;
            bounds
        } else {
            bounds
        };
    if let Some(refusal) = gui_click_refusal(&bounds, requested) {
        bail!("{}", with_clickable_now(&refusal, &bounds));
    }
    let x = bounds
        .get("x")
        .and_then(Value::as_f64)
        .context("getGuiBounds returned no x")?;
    let y = bounds
        .get("y")
        .and_then(Value::as_f64)
        .context("getGuiBounds returned no y")?;
    Ok((bounds, x, y))
}

/// How long a GUI path may stay unresolved on a client that just joined.
const GUI_PATH_SETTLE: Duration = Duration::from_secs(8);

/// `gui_input_bounds` for a client that may have just joined: it builds its
/// PlayerGui a moment after it connects, so a path that matches nothing yet
/// is retried for a short while.
fn gui_input_bounds_settled(
    bridge: &BridgeServer,
    player: Option<&str>,
    path: Option<&str>,
    id: Option<&str>,
    requested: &str,
) -> Result<(Value, f64, f64)> {
    let started = Instant::now();
    loop {
        match gui_input_bounds(bridge, player, path, id, requested) {
            Err(error)
                if error.to_string().contains("matched nothing under")
                    && started.elapsed() < GUI_PATH_SETTLE =>
            {
                std::thread::sleep(Duration::from_millis(500));
            }
            outcome => return outcome,
        }
    }
}

pub(crate) fn press_result(args: &PressArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let requested = args
        .path
        .as_deref()
        .or(args.id.as_deref())
        .context("Provide a GUI path or --id")?;
    let (bounds, x, y) = if args.world {
        let path = args.path.as_deref().context("--world requires a path")?;
        let read = || {
            bridge.call_for_selector(
                "getWorldPoint",
                json!({ "path": path }),
                BridgeTarget::Client,
                player,
            )
        };
        let bounds = read()?;
        ensure_plugin_api_ok(&bounds)?;
        #[cfg(any(windows, target_os = "macos"))]
        let bounds = if result_viewport_size(&bounds)
            .is_some_and(|(width, height)| width <= 1 || height <= 1)
        {
            let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
            recover_client_viewport(bridge, player, pid, result_viewport_size(&bounds))?;
            let bounds = read()?;
            ensure_plugin_api_ok(&bounds)?;
            bounds
        } else {
            bounds
        };
        if bounds.get("onScreen").and_then(Value::as_bool) == Some(false) {
            let subject = bounds
                .get("fullName")
                .and_then(Value::as_str)
                .unwrap_or(requested);
            bail!(
                "{subject} is not on screen (behind the camera or outside the viewport); move \
                 the character or camera first (rbx goto)"
            );
        }
        let x = bounds
            .get("x")
            .and_then(Value::as_f64)
            .context("getWorldPoint returned no x")?;
        let y = bounds
            .get("y")
            .and_then(Value::as_f64)
            .context("getWorldPoint returned no y")?;
        (bounds, x, y)
    } else {
        gui_input_bounds_settled(
            bridge,
            player,
            args.path.as_deref(),
            args.id.as_deref(),
            requested,
        )?
    };
    let mut result = json!({
        "ok": true,
        "action": "press",
        "target": bounds
            .get("fullName")
            .cloned()
            .unwrap_or_else(|| Value::String(requested.to_string())),
        "ordinalPath": bounds.get("ordinalPath").cloned().unwrap_or(Value::Null),
        "id": bounds.get("id").cloned().unwrap_or(Value::Null),
        "matchedCount": bounds.get("matchedCount").cloned().unwrap_or(Value::Null),
        "viewportX": x,
        "viewportY": y,
    });
    if !args.world && !args.right {
        let id = bounds
            .get("id")
            .and_then(Value::as_str)
            .context("The target GuiButton has no stable id")?;
        let response = send_virtual_input(
            bridge,
            player,
            virtual_click_actions(x.round() as i32, y.round() as i32, false, args.hold, false),
            Some(id),
        )?;
        result["inputMethod"] = json!("virtual");
        note_system_ui(&mut result, &response);
        return Ok(result);
    }
    #[cfg(windows)]
    if os_input_preferred() {
        let viewport = match (
            bounds.get("viewportWidth").and_then(Value::as_f64),
            bounds.get("viewportHeight").and_then(Value::as_f64),
        ) {
            (Some(width), Some(height)) if width >= 1.0 && height >= 1.0 => {
                Some((width.round() as i32, height.round() as i32))
            }
            _ => None,
        };
        let (window, offset_x, offset_y) = resolve_player_window(bridge, player, viewport)?;
        let _shield = input_inject::input_shield(&window)?;
        let (delta_x, delta_y) =
            input_delta(bridge, player, &window, x.round() as i32, y.round() as i32);
        input_inject::post_mouse_click(
            &window,
            x.round() as i32 + offset_x + delta_x,
            y.round() as i32 + offset_y + delta_y,
            args.right,
            args.hold,
        )?;
        result["inputMethod"] = json!("os");
        result["window"] = json!(window.label);
        return Ok(result);
    }
    // A world part answers a click only once the pointer has hovered it for a
    // frame or two; a GUI click lands on its element without that hover.
    let mut actions = vec![
        json!({ "type": "move", "x": x.round() as i32, "y": y.round() as i32 }),
        json!({ "type": "wait", "ms": WORLD_HOVER_SETTLE_MS }),
    ];
    actions.extend(virtual_click_actions(
        x.round() as i32,
        y.round() as i32,
        args.right,
        args.hold,
        false,
    ));
    let response = send_virtual_input(bridge, player, actions, None)?;
    result["inputMethod"] = json!("virtual");
    note_system_ui(&mut result, &response);
    Ok(result)
}

/// A key can be held for a minute; the plugin runs each wait for at most
/// ten seconds, so longer holds are several waits.
const MAX_KEY_HOLD_MS: u64 = 60_000;

/// The wait actions that pause a client for `ms` (a minute at most), each
/// within the plugin's ten-second wait.
pub(super) fn virtual_wait_actions(ms: u64) -> Vec<Value> {
    let mut actions = Vec::new();
    let mut remaining = ms.min(MAX_KEY_HOLD_MS);
    while remaining > 0 {
        let step = remaining.min(MAX_VIRTUAL_WAIT_MS);
        actions.push(json!({ "type": "wait", "ms": step }));
        remaining -= step;
    }
    actions
}
const MAX_VIRTUAL_WAIT_MS: u64 = 10_000;

pub(crate) fn ui_result(args: &UiArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let read = || {
        bridge.call_for_selector(
            "getGuiInventory",
            json!({ "limit": args.limit, "includeOffscreen": args.include_offscreen }),
            BridgeTarget::Client,
            player,
        )
    };
    let result = read()?;
    ensure_plugin_api_ok(&result)?;
    #[cfg(any(windows, target_os = "macos"))]
    let result =
        if !args.include_offscreen && result.get("count").and_then(Value::as_u64) == Some(0) {
            let pid = bridge.studio_pid_for_selector(BridgeTarget::Client, player)?;
            let viewport = client_viewport_size(bridge, player);
            if viewport.is_some_and(|(width, height)| width <= 1 || height <= 1) {
                recover_client_viewport(bridge, player, pid, viewport)?;
                let result = read()?;
                ensure_plugin_api_ok(&result)?;
                result
            } else {
                result
            }
        } else {
            result
        };
    Ok(result)
}

pub(crate) fn type_result(args: &TypeArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    #[cfg(windows)]
    if os_input_preferred() {
        let (pressed, click, viewport) = if let Some(path) = args.path.as_ref() {
            let (bounds, x, y) = gui_input_bounds_settled(bridge, player, Some(path), None, path)?;
            if bounds.get("className").and_then(Value::as_str) != Some("TextBox") {
                bail!("{path} is not a TextBox");
            }
            (
                bounds
                    .get("fullName")
                    .cloned()
                    .unwrap_or_else(|| Value::String(path.clone())),
                Some((x.round() as i32, y.round() as i32)),
                result_viewport_size(&bounds),
            )
        } else {
            (Value::Null, None, client_viewport_size(bridge, player))
        };
        let (window, offset_x, offset_y) = resolve_player_window(bridge, player, viewport)?;
        let _shield = input_inject::input_shield(&window)?;
        if let Some((x, y)) = click {
            let (delta_x, delta_y) = input_delta(bridge, player, &window, x, y);
            input_inject::post_mouse_click(
                &window,
                x + offset_x + delta_x,
                y + offset_y + delta_y,
                false,
                30,
            )?;
            thread::sleep(Duration::from_millis(250));
        }
        input_inject::post_text(&window, &args.text)?;
        if args.enter {
            let enter = input_inject::resolve_key("Enter")?;
            input_inject::post_key(&window, &enter, 40)?;
        }
        return Ok(json!({
            "ok": true,
            "action": "type",
            "chars": args.text.chars().count(),
            "focused": pressed,
            "enter": args.enter,
            "inputMethod": "os",
            "window": window.label,
        }));
    }
    let mut pressed = Value::Null;
    let mut actions = Vec::new();
    if let Some(path) = args.path.as_ref() {
        let (bounds, _, _) = gui_input_bounds_settled(bridge, player, Some(path), None, path)?;
        if bounds.get("className").and_then(Value::as_str) != Some("TextBox") {
            bail!("{path} is not a TextBox");
        }
        pressed = bounds
            .get("fullName")
            .cloned()
            .unwrap_or_else(|| Value::String(path.clone()));
        actions.push(json!({
            "type": "focus",
            "id": bounds.get("id").context("The target TextBox has no stable id")?,
        }));
    }
    actions.push(json!({ "type": "text", "text": args.text }));
    if args.enter {
        actions.extend([
            json!({ "type": "wait", "ms": 0 }),
            json!({ "type": "key", "key": "Return", "down": true }),
            json!({ "type": "wait", "ms": 40 }),
            json!({ "type": "key", "key": "Return", "down": false }),
        ]);
    }
    let response = send_virtual_input(bridge, player, actions, None)?;
    let mut result = json!({
        "ok": true,
        "action": "type",
        "chars": args.text.chars().count(),
        "focused": pressed,
        "enter": args.enter,
        "inputMethod": "virtual",
    });
    if let Some(observed) = response.get("textObserved").and_then(Value::as_bool) {
        result["textObserved"] = json!(observed);
    }
    Ok(result)
}

/// The body of the probe function a wait runs each check: a single expression
/// is returned from it, and a chunk (statements with their own `return`) runs
/// as is.
fn wait_condition_body(condition: &str) -> Result<String> {
    returning_body(condition, "condition")
}

/// A Luau expression or chunk made into a chunk that returns its value, for
/// callers outside this module that compose a read after another operation.
pub(crate) fn returning_luau_body(code: &str, what: &str) -> Result<String> {
    returning_body(code, what)
}

/// The body of the one-shot observation a wait evaluates once its condition
/// holds; it takes the same expression or chunk forms as the condition.
fn wait_result_body(result: &str) -> Result<String> {
    returning_body(result, "result expression")
}

fn returning_body(source: &str, label: &str) -> Result<String> {
    let expression = format!("return ({source}\n)");
    match validate_luau_syntax(&expression) {
        Ok(()) => Ok(expression),
        Err(expression_error) => {
            if validate_luau_syntax(source).is_ok() {
                return Ok(source.to_string());
            }
            Err(expression_error.context(format!(
                "The {label} is neither a Luau expression nor a chunk that returns one"
            )))
        }
    }
}

/// One bounded runner slice of a wait: polls the condition until it holds or
/// the slice ends. A met condition returns `true, tostring(value)`, and with a
/// result body also whether that body ran cleanly plus its value or error, so
/// the observation reaches the daemon through the runner's own serialization.
fn wait_probe_code(body: &str, result_body: Option<&str>, slice: f64, interval: f64) -> String {
    let met = match result_body {
        Some(result_body) => format!(
            "\t\t\tlocal resultOk, result = pcall(function()\n\
             {result_body}\n\
             \t\t\tend)\n\
             \t\t\treturn true, (type(value) == 'table' and game:GetService('HttpService'):JSONEncode(value) or tostring(value)), resultOk, result"
        ),
        None => "\t\t\treturn true, (type(value) == 'table' and game:GetService('HttpService'):JSONEncode(value) or tostring(value))".to_string(),
    };
    format!(
        "local deadline = os.clock() + {slice}\n\
         local detail = nil\n\
         \twhile true do\n\
         \t\tlocal ok, value = pcall(function()\n\
         {body}\n\
         \t\tend)\n\
         \t\tdetail = value\n\
         \t\tif ok and value then\n\
         {met}\n\
         \t\tend\n\
         \t\tif os.clock() >= deadline then return false, tostring(detail) end\n\
         \t\ttask.wait({interval})\n\
         \tend",
    )
}

/// The wait record for a met condition. With a result expression, a clean
/// evaluation adds `result`; a failing one turns the record into a failure
/// that still says the condition was met, so the caller does not wait again.
fn met_condition_result(args: &WaitUntilArgs, outcome: &LuauTaskOutcome, elapsed: f64) -> Value {
    let mut record = json!({
        "ok": true,
        "action": "wait",
        "condition": args.condition,
        "value": outcome.detail,
        "elapsedSeconds": elapsed,
    });
    if args.result.is_none() {
        return record;
    }
    let observed = outcome.results.get(3).cloned().unwrap_or(Value::Null);
    if outcome.results.get(2).and_then(Value::as_bool) == Some(true) {
        record["result"] = observed;
    } else {
        record["ok"] = json!(false);
        record["conditionMet"] = json!(true);
        let error = match &observed {
            Value::String(message) => message.clone(),
            other => other.to_string(),
        };
        record["error"] = json!(format!(
            "The condition was met, but the result expression failed: {error}"
        ));
    }
    record
}

/// A duration given in place of a condition: `30`, `30s`, `500ms` or `2m`.
fn fixed_wait_seconds(condition: &str) -> Option<f64> {
    let text = condition.trim();
    let unit_start = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(unit_start);
    let value: f64 = number.parse().ok()?;
    let seconds = match unit.trim() {
        "" | "s" => value,
        "ms" => value / 1000.0,
        "m" => value * 60.0,
        _ => return None,
    };
    (seconds.is_finite() && seconds > 0.0).then(|| seconds.min(3600.0))
}

/// `rbx wait 30s` waits that long and nothing else: a sampler's window, or
/// time for the game to move on, without a condition that never holds.
fn fixed_wait_result(condition: &str) -> Option<Value> {
    let seconds = fixed_wait_seconds(condition)?;
    let started = Instant::now();
    thread::sleep(Duration::from_secs_f64(seconds));
    Some(json!({
        "ok": true,
        "action": "wait",
        "waitedSeconds": started.elapsed().as_secs_f64(),
    }))
}

pub(crate) fn wait_until_result(args: &WaitUntilArgs, bridge: &BridgeServer) -> Result<Value> {
    let client = args.client || args.player.is_some();
    let player = args.player.as_deref();
    let target = if args.server {
        BridgeTarget::Server
    } else {
        BridgeTarget::main_or_client(client)
    };
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    if !args.timeout.is_finite() || !args.interval.is_finite() {
        bail!("--timeout and --interval must be finite numbers");
    }
    let body = wait_condition_body(&args.condition)?;
    let result_body = args.result.as_deref().map(wait_result_body).transpose()?;
    let timeout = args.timeout.clamp(0.1, 3600.0);
    let interval = args.interval.clamp(0.05, 10.0);
    let started = Instant::now();
    let mut detail = Value::Null;
    loop {
        let remaining = timeout - started.elapsed().as_secs_f64();
        if remaining <= 0.0 {
            let mut timed_out = wait_timed_out_result(
                &args.condition,
                timeout,
                &detail,
                started.elapsed().as_secs_f64(),
            );
            // The look that would follow a wait that ran out comes with it:
            // which part of the condition does not hold, and on what value.
            let stopped = (!args.slice)
                .then(|| wait_parts::ConditionParts::of(&body))
                .flatten()
                .and_then(|parts| {
                    let looked = run_luau_task(bridge, target, player, &parts.probe(), 3.0).ok()?;
                    parts.stopped_at(&looked.results)
                });
            if let Some(stopped) = stopped {
                let error = timed_out["error"].as_str().unwrap_or_default().replacen(
                    "; read the state it depends on before waiting again, and raise",
                    &format!("; {stopped}; raise"),
                    1,
                );
                timed_out["error"] = json!(error);
            }
            return Ok(timed_out);
        }
        let slice = remaining.min(WAIT_PROBE_SLICE_SECONDS);
        let code = wait_probe_code(&body, result_body.as_deref(), slice, interval);
        let limit = wait_probe_run_limit(slice, remaining, result_body.is_some());
        let outcome = run_luau_task(bridge, target, player, &code, limit)?;
        if outcome.success {
            return Ok(met_condition_result(
                args,
                &outcome,
                started.elapsed().as_secs_f64(),
            ));
        }
        detail = outcome.detail;
    }
}

/// The run limit of one probe slice. The condition is polled for `slice`
/// seconds and gets a margin on top. A result body runs in the same runner
/// once the condition holds, so with one the runner may use what is left of
/// the wait's timeout (`remaining`), up to the 120 s Studio gives a runner.
fn wait_probe_run_limit(slice: f64, remaining: f64, has_result: bool) -> f64 {
    let polled = slice + 5.0;
    if has_result {
        remaining.clamp(polled, 120.0)
    } else {
        polled
    }
}

/// The answer a wait gives when its condition never held within `timeout`.
pub(crate) fn wait_timed_out_result(
    condition: &str,
    timeout: f64,
    detail: &Value,
    elapsed: f64,
) -> Value {
    json!({
        "ok": false,
        "action": "wait",
        "condition": condition,
        "timedOut": true,
        "value": detail,
        "elapsedSeconds": elapsed,
        "error": format!(
            "Timed out after {timeout}s waiting for condition (last value: {detail}); \
             read the state it depends on before waiting again, and raise -t only when \
             the event is still on its way; rbx wait 30s waits a fixed time, \
             rbx l --detach runs a sampler"
        ),
    })
}

// Studio stops a runner after 120 s; each slice stays under that so a long
// wait keeps checking instead of dying with the runner.
const WAIT_RUNNER_SLICE_SECONDS: f64 = 100.0;
/// How long a world press lets the pointer rest on its part before the button.
const WORLD_HOVER_SETTLE_MS: u64 = 120;
/// A wait polls its condition in short runs so a `rbx play -x` issued
/// meanwhile reaches the runtime between them instead of after the timeout.
const WAIT_PROBE_SLICE_SECONDS: f64 = 5.0;

struct LuauTaskOutcome {
    success: bool,
    detail: Value,
    results: Vec<Value>,
    elapsed: f64,
}

fn run_luau_task(
    bridge: &BridgeServer,
    target: BridgeTarget,
    player: Option<&str>,
    code: &str,
    timeout: f64,
) -> Result<LuauTaskOutcome> {
    let started = Instant::now();
    let result = call_execute_luau(bridge, target, player, code, "ReniumTask", timeout, 2.0)?;
    ensure_luau_api_ok(&result)?;
    let results = result
        .get("results")
        .and_then(Value::as_array)
        .context("Luau task returned no results")?;
    let success = results
        .first()
        .and_then(Value::as_bool)
        .context("Luau task returned an invalid status")?;
    Ok(LuauTaskOutcome {
        success,
        detail: results.get(1).cloned().unwrap_or(Value::Null),
        results: results.clone(),
        elapsed: started.elapsed().as_secs_f64(),
    })
}

pub(crate) fn goto_result(args: &GotoArgs, bridge: &BridgeServer) -> Result<Value> {
    const ARRIVAL_RADIUS: f64 = 8.0;

    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let (x, y, z, label) = if let Some(pos) = args.pos.as_ref() {
        let mut parts = pos.split(',');
        let parse = |part: Option<&str>| {
            part.and_then(|value| value.trim().parse::<f64>().ok())
                .with_context(|| format!("--pos must be X,Y,Z numbers, got '{pos}'"))
        };
        let x = parse(parts.next())?;
        let y = parse(parts.next())?;
        let z = parse(parts.next())?;
        if parts.next().is_some() {
            bail!("--pos must have exactly three components, got '{pos}'");
        }
        (x, y, z, pos.clone())
    } else {
        let path = args
            .target
            .as_deref()
            .context("Provide a part path or --pos")?;
        let point = bridge.call_for_selector(
            "getWorldPoint",
            json!({ "path": path, "approach": true }),
            BridgeTarget::Client,
            player,
        )?;
        ensure_plugin_api_ok(&point)?;
        let position = point
            .get("worldPosition")
            .and_then(Value::as_array)
            .context("getWorldPoint returned no worldPosition")?;
        let [x, y, z] = position.as_slice() else {
            bail!("getWorldPoint returned malformed worldPosition");
        };
        let x = x
            .as_f64()
            .context("getWorldPoint returned malformed worldPosition")?;
        let y = y
            .as_f64()
            .context("getWorldPoint returned malformed worldPosition")?;
        let z = z
            .as_f64()
            .context("getWorldPoint returned malformed worldPosition")?;
        let label = point
            .get("fullName")
            .and_then(Value::as_str)
            .unwrap_or(path)
            .to_string();
        (x, y, z, label)
    };
    if !x.is_finite()
        || !y.is_finite()
        || !z.is_finite()
        || !args.timeout.is_finite()
        || !args.speed_multiplier.is_finite()
    {
        bail!("Position coordinates, --timeout, and --speed-multiplier must be finite numbers");
    }
    let timeout = args.timeout.clamp(1.0, 300.0);
    let speed_multiplier = args.speed_multiplier.clamp(0.1, 10.0);
    let movement = |slice: f64| {
        if args.tp {
            format!(
                "\tch:PivotTo(CFrame.new(targetPos + Vector3.new(0, 4, 0)))\n\
             \tlocal dist = (ch:GetPivot().Position - targetPos).Magnitude\n\
             \treturn dist < {ARRIVAL_RADIUS}, dist"
            )
        } else {
            format!(
                "\tlocal deadline = os.clock() + {slice}\n\
             \tlocal function distance()\n\
             \t\treturn (ch:GetPivot().Position - targetPos).Magnitude\n\
             \tend\n\
             \tlocal PathfindingService = game:GetService('PathfindingService')\n\
             \twhile os.clock() < deadline do\n\
             \t\tlocal dist = distance()\n\
             \t\tif dist < {ARRIVAL_RADIUS} then return true, dist end\n\
             \t\tlocal path = PathfindingService:CreatePath()\n\
             \t\tlocal okCompute = pcall(function()\n\
             \t\t\tpath:ComputeAsync(ch:GetPivot().Position, targetPos)\n\
             \t\tend)\n\
             \t\tif okCompute and path.Status == Enum.PathStatus.Success then\n\
             \t\t\tfor _, waypoint in ipairs(path:GetWaypoints()) do\n\
             \t\t\t\tif os.clock() >= deadline or distance() < {ARRIVAL_RADIUS} then break end\n\
             \t\t\t\tif waypoint.Action == Enum.PathWaypointAction.Jump then hum.Jump = true end\n\
             \t\t\t\thum:MoveTo(waypoint.Position)\n\
             \t\t\t\tlocal reached = false\n\
             \t\t\t\tlocal conn = hum.MoveToFinished:Connect(function() reached = true end)\n\
             \t\t\t\tlocal waitDeadline = os.clock() + 4\n\
             \t\t\t\twhile not reached and os.clock() < waitDeadline do task.wait(0.1) end\n\
             \t\t\t\tconn:Disconnect()\n\
             \t\t\tend\n\
             \t\telse\n\
             \t\t\thum:MoveTo(targetPos)\n\
             \t\t\ttask.wait(1)\n\
             \t\tend\n\
             \t\ttask.wait(0.2)\n\
             \tend\n\
             \tlocal dist = distance()\n\
             \treturn dist < {ARRIVAL_RADIUS}, dist"
            )
        }
    };
    let code = |slice: f64| {
        format!(
            "local targetPos = Vector3.new({x}, {y}, {z})\n\
             \tlocal lp = game:GetService('Players').LocalPlayer\n\
             \tlocal ch = lp.Character or lp.CharacterAdded:Wait()\n\
             \tlocal hum = ch:FindFirstChildOfClass('Humanoid')\n\
             \tlocal humanoidDeadline = os.clock() + 10\n\
             \twhile hum == nil and os.clock() < humanoidDeadline do task.wait(0.1) hum = ch:FindFirstChildOfClass('Humanoid') end\n\
             \tif hum == nil then return false, 'no humanoid' end\n\
             \tlocal originalSpeed = hum.WalkSpeed\n\
             \thum.WalkSpeed = originalSpeed * {speed_multiplier}\n\
             \tlocal ok, reached, detail = pcall(function()\n\
             {}\n\
             \tend)\n\
             \thum.WalkSpeed = originalSpeed\n\
             \tif not ok then error(reached) end\n\
             \treturn reached, detail",
            movement(slice)
        )
    };
    // Studio stops a runner after 120 s, so a long walk runs as slices that
    // each resume from where the character stands.
    let started = Instant::now();
    let outcome = loop {
        let remaining = timeout - started.elapsed().as_secs_f64();
        let slice = remaining.min(WAIT_RUNNER_SLICE_SECONDS);
        let outcome = run_luau_task(
            bridge,
            BridgeTarget::Client,
            player,
            &code(slice),
            slice + 5.0,
        )?;
        let gave_up_early = outcome.elapsed + 1.0 < slice;
        if outcome.success || args.tp || gave_up_early || remaining <= slice {
            break outcome;
        }
    };
    if outcome.success {
        let final_distance = outcome
            .detail
            .as_f64()
            .context("Goto returned a malformed final distance")?;
        Ok(json!({
            "ok": true,
            "action": if args.tp { "teleport" } else { "goto" },
            "target": label,
            "position": [x, y, z],
            "arrivalRadius": ARRIVAL_RADIUS,
            "finalDistance": final_distance,
            "speedMultiplier": speed_multiplier,
            "elapsedSeconds": started.elapsed().as_secs_f64(),
        }))
    } else {
        bail!(
            "Character did not reach {label} within {timeout}s ({})",
            outcome.detail
        )
    }
}

pub(crate) fn goto_command(args: GotoArgs) -> Result<()> {
    run_input_command(
        op::GOTO,
        json!({
            "target": args.target,
            "pos": args.pos,
            "player": args.player,
            "tp": args.tp,
            "timeout": args.timeout,
            "speedMultiplier": args.speed_multiplier,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn shot_result(args: &ShotArgs, bridge: &BridgeServer) -> Result<Value> {
    let player = args.player.as_deref();
    if let Some(player) = player {
        wait_for_player_bridge(bridge, player, args.bridge.wait_seconds)?;
    }
    let studio_status = studio_capture_status(bridge);
    let simulated = studio_status
        .as_ref()
        .and_then(|status| status.get("simulating"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let play_running = studio_status
        .as_ref()
        .and_then(|status| status.get("playRunning"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    #[cfg(any(windows, target_os = "macos"))]
    let selected_client_is_studio = player.is_none()
        || bridge
            .studio_pid_for_selector(BridgeTarget::Client, player)
            .ok()
            == bridge
                .studio_pid_for_selector(BridgeTarget::Edit, None)
                .ok();
    #[cfg(not(any(windows, target_os = "macos")))]
    let selected_client_is_studio = false;
    let use_simulator = simulated && !args.client && selected_client_is_studio;
    if use_simulator {
        let settle_seconds = studio_status
            .as_ref()
            .and_then(|status| status.get("settleSeconds"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .clamp(0.0, 5.0);
        if settle_seconds > 0.0 {
            thread::sleep(Duration::from_secs_f64(settle_seconds));
        }
    }
    let client_ready =
        bridge.channel_count_for_target(BridgeTarget::Client) >= bridge.expected_channel_count();
    let use_studio =
        args.studio || use_simulator || (player.is_none() && !args.client && !client_ready);
    let probe_target = if play_running {
        BridgeTarget::Client
    } else {
        BridgeTarget::Edit
    };
    let (window, target, bridge_target) = if use_studio {
        (
            resolve_edit_window(bridge, probe_target)?,
            "studio",
            BridgeTarget::Edit,
        )
    } else {
        (
            resolve_client_capture_window(bridge, player)?,
            "play-client",
            BridgeTarget::Client,
        )
    };
    let output = if args.output.is_absolute() {
        args.output.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&args.output)
    };
    let camera = match (&args.camera_position, &args.look_at) {
        (Some(position), Some(look_at)) => Some((
            parse_vector3(position, "--camera-position")?,
            parse_vector3(look_at, "--look-at")?,
        )),
        (None, None) => None,
        _ => bail!("--camera-position and --look-at must be used together"),
    };
    let token = if let Some((position, look_at)) = camera {
        let result = bridge.call_for_selector(
            "cameraCapture",
            json!({ "action": "prepare", "position": position, "lookAt": look_at }),
            bridge_target,
            player,
        )?;
        ensure_plugin_api_ok(&result)?;
        Some(
            result
                .get("token")
                .and_then(Value::as_str)
                .context("cameraCapture returned no token")?
                .to_string(),
        )
    } else {
        None
    };
    let mut camera_report = None;
    let capture = (|| {
        if let Some((position, look_at)) = camera {
            // The camera setter acknowledges the property change before rendering.
            // Cross two real frame boundaries while the camera lease is still held,
            // and read the camera back so the result says whether Studio moved it.
            let result = call_execute_luau(
                bridge,
                bridge_target,
                player,
                "local runService = game:GetService(\"RunService\")\nrunService.Heartbeat:Wait()\nrunService.Heartbeat:Wait()\nlocal camera = workspace.CurrentCamera\nlocal p = camera.CFrame.Position\nlocal look = camera.CFrame.LookVector\nreturn { p.X, p.Y, p.Z, tostring(camera.CameraType), look.X, look.Y, look.Z }",
                "ReniumCaptureFrame",
                3.0,
                1.0,
            )?;
            ensure_luau_api_ok(&result)?;
            camera_report = Some(camera_readback(&result, position, look_at));
        }
        input_inject::capture_window_png(&window, &output)
    })();
    let restore = token.map(|token| {
        bridge.call_for_selector(
            "cameraCapture",
            json!({ "action": "restore", "token": token }),
            bridge_target,
            player,
        )
    });
    let (width, height) = capture?;
    if let Some(result) = restore {
        ensure_plugin_api_ok(&result?)?;
    }
    let mut result = json!({
        "ok": true,
        "action": "shot",
        "path": output.display().to_string(),
        "width": width,
        "height": height,
        "window": window.label,
        "target": target,
        "deviceSimulation": use_simulator,
    });
    if let Some(report) = camera_report {
        result["camera"] = report;
    }
    Ok(result)
}

/// What the camera held after the move: position and direction are checked
/// before capture while the temporary camera lease is still active.
fn camera_readback(result: &Value, requested: [f64; 3], look_at: [f64; 3]) -> Value {
    let values = result
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .and_then(Value::as_array);
    let actual = values.and_then(|values| {
        Some([
            values.first()?.as_f64()?,
            values.get(1)?.as_f64()?,
            values.get(2)?.as_f64()?,
        ])
    });
    let camera_type = values
        .and_then(|values| values.get(3))
        .and_then(Value::as_str)
        .map(str::to_string);
    let actual_look = values.and_then(|values| {
        Some([
            values.get(4)?.as_f64()?,
            values.get(5)?.as_f64()?,
            values.get(6)?.as_f64()?,
        ])
    });
    let direction = std::array::from_fn::<_, 3, _>(|index| look_at[index] - requested[index]);
    let length = direction
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    let facing = actual_look.is_some_and(|actual| {
        length > 0.0
            && direction
                .iter()
                .zip(actual)
                .all(|(wanted, got)| (wanted / length - got).abs() < 0.001)
    });
    let moved = facing
        && actual.is_some_and(|actual| {
            requested
                .iter()
                .zip(actual)
                .all(|(wanted, got)| (wanted - got).abs() < 1.0)
        });
    let mut report = json!({ "requested": requested, "requestedLookAt": look_at, "actual": actual, "actualLookVector": actual_look, "moved": moved });
    if let Some(camera_type) = camera_type {
        report["cameraType"] = json!(camera_type);
    }
    if !moved {
        report["note"] = json!(
            "Studio did not hold the requested camera position and direction; inspect the capture before using it. Make the 3D view the active tab and retry"
        );
    }
    report
}

fn test_launch_clients(bridge: &BridgeServer, launch: &TestLaunch) -> Vec<Value> {
    bridge
        .list_bridge_clients()
        .into_iter()
        .filter(|entry| {
            entry.get("launchNonce").and_then(Value::as_str) == Some(launch.nonce.as_str())
                && entry.get("launchEditRuntimeId").and_then(Value::as_str)
                    == Some(launch.edit_runtime_id.as_str())
                && (entry["role"] == BRIDGE_ROLE_PLAY_SERVER
                    || entry["role"] == BRIDGE_ROLE_PLAY_CLIENT)
        })
        .collect()
}

fn parse_vector3(text: &str, label: &str) -> Result<[f64; 3]> {
    let values = text
        .split(',')
        .map(|value| value.trim().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("{label} must contain three comma-separated numbers"))?;
    let [x, y, z] = values.as_slice() else {
        bail!("{label} must contain exactly three numbers");
    };
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        bail!("{label} values must be finite");
    }
    Ok([*x, *y, *z])
}

fn compact_json(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, entry| !entry.is_null());
    }
    value
}

fn run_input_command(
    operation: u16,
    mut parameters: Value,
    bridge_args: &BridgeConnectionArgs,
    compact: bool,
) -> Result<()> {
    let object = parameters
        .as_object_mut()
        .context("Input operation parameters must be an object")?;
    object.insert(
        "bridgeWaitSeconds".to_string(),
        json!(bridge_args.wait_seconds),
    );
    object.insert("bridgePorts".to_string(), json!(bridge_args.ports));
    let requested_output = parameters.get("output").cloned();
    let result = daemon_control_request(operation, None, parameters, false)?;
    let mut result = if compact {
        compact_json(result)
    } else {
        result
    };
    if crate::app::output::compact_text_output() {
        compact_input_result(operation, &mut result, requested_output.as_ref());
    }
    print_json_output(&result, false)?;
    if result.get("ok") == Some(&Value::Bool(false)) {
        return Err(ReportedFailure.into());
    }
    Ok(())
}

/// A wait's record without the echo of its request, its seconds rounded to
/// one decimal and its `--result` value in the short form of a Luau result; a
/// timed-out wait keeps its last value and error.
fn compact_wait_record(record: &mut Map<String, Value>) {
    record.remove("action");
    record.remove("condition");
    if let Some(value) = record.get_mut("result") {
        *value = result_text::compact_value(value);
    }
    for key in ["elapsedSeconds", "waitedSeconds"] {
        if let Some(seconds) = record.get(key).and_then(Value::as_f64) {
            record.insert(key.to_string(), json!((seconds * 10.0).round() / 10.0));
        }
    }
}

/// Text mode keeps a capture's verdict (`moved`, a note when the camera did
/// not hold), a wait's value and what an input sequence observed; the window,
/// device and echoed arguments stay in json mode.
pub(crate) fn compact_input_result(
    operation: u16,
    result: &mut Value,
    requested_output: Option<&Value>,
) {
    let Some(map) = result.as_object_mut() else {
        return;
    };
    if operation == op::WAIT {
        compact_wait_record(map);
        return;
    }
    if map.get("ok") == Some(&Value::Bool(false)) {
        return;
    }
    match operation {
        op::SHOT => {
            if let Some(camera) = map.remove("camera") {
                if let Some(moved) = camera.get("moved").cloned() {
                    map.insert("moved".to_string(), moved);
                }
                if let Some(note) = camera.get("note").cloned() {
                    map.insert("note".to_string(), note);
                }
            }
            map.remove("window");
            map.remove("deviceSimulation");
            if requested_output.is_some_and(|output| map.get("path") == Some(output)) {
                map.remove("path");
            }
        }
        op::INPUT => {
            // Virtual input is the default method; a null click count means
            // no click was sent; a screenshot written where it was asked for
            // is confirmed without repeating the path.
            map.remove("action");
            map.remove("ok");
            if map.get("inputMethod") == Some(&json!("virtual")) {
                map.remove("inputMethod");
            }
            if map.get("verifiedClicks") == Some(&Value::Null) {
                map.remove("verifiedClicks");
            }
            if requested_output.is_some_and(|output| map.get("shot") == Some(output)) {
                map.insert("shot".to_string(), Value::Bool(true));
            }
        }
        _ => {}
    }
}

pub(crate) fn press_command(args: PressArgs) -> Result<()> {
    run_input_command(
        op::PRESS,
        json!({
            "path": args.path,
            "id": args.id,
            "player": args.player,
            "right": args.right,
            "world": args.world,
            "hold": args.hold,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn ui_command(args: UiArgs) -> Result<()> {
    run_input_command(
        op::UI,
        json!({
            "player": args.player,
            "limit": args.limit,
            "includeOffscreen": args.include_offscreen,
        }),
        &args.bridge,
        false,
    )
}

pub(crate) fn type_command(args: TypeArgs) -> Result<()> {
    run_input_command(
        op::TYPE,
        json!({
            "text": args.text,
            "path": args.path,
            "player": args.player,
            "enter": args.enter,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn wait_until_command(args: WaitUntilArgs) -> Result<()> {
    // A fixed wait is this process sleeping. Sent to the daemon it would hold
    // its Studio's request gate, and every other command for that Studio
    // would answer "Studio is busy" until the wait ended.
    if let Some(mut result) = fixed_wait_result(&args.condition) {
        if crate::app::output::compact_text_output() {
            compact_input_result(op::WAIT, &mut result, None);
        }
        return print_json_output(&result, false);
    }
    run_input_command(
        op::WAIT,
        json!({
            "condition": args.condition,
            "result": args.result,
            "player": args.player,
            "client": args.client,
            "server": args.server,
            "timeout": args.timeout,
            "interval": args.interval,
        }),
        &args.bridge,
        true,
    )
}

/// A path argument names a file relative to where the command runs, like
/// every other path a command takes; the daemon has no cwd of its own.
pub(crate) fn absolute_from_cwd(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

pub(crate) fn shot_command(args: ShotArgs) -> Result<()> {
    let output = absolute_from_cwd(args.output.clone());
    run_input_command(
        op::SHOT,
        json!({
            "output": output,
            "player": args.player,
            "studio": args.studio,
            "client": args.client,
            "cameraPosition": args.camera_position,
            "lookAt": args.look_at,
        }),
        &args.bridge,
        true,
    )
}

pub(crate) fn record_start_command(args: RecordStartArgs) -> Result<()> {
    let result = daemon_result(
        op::RECORD_START,
        None,
        json!({
            "output": args.output.map(absolute_from_cwd),
            "directory": std::env::current_dir().ok(),
            "player": args.player,
            "studio": args.studio,
            "client": args.client,
            "fps": args.fps,
            "maxSeconds": args.max_seconds,
            "quality": args.quality,
        }),
        false,
        None,
    )?;
    print_json_output(&result, false)
}

pub(crate) fn record_end_command(args: RecordEndArgs) -> Result<()> {
    let mut result = try_daemon_control_request(
        op::RECORD_END,
        json!({
            "recordingId": args.recording_id,
        }),
    )?
    .context("No Renium recording is active")?;
    if !args.no_review {
        recording_review::attach_overview(&mut result);
    }
    print_json_output(&result, false)
}

pub(crate) fn list_clients_command(args: ListClientsArgs) -> Result<()> {
    let deadline = args
        .wait
        .map(|seconds| Instant::now() + Duration::from_secs_f64(seconds.clamp(0.0, 3600.0)));
    let result = loop {
        let result = daemon_result(op::STUDIOS, None, json!({}), false, Some(&args.bridge))?;
        let connected = result
            .get("clients")
            .and_then(Value::as_array)
            .is_some_and(|clients| !clients.is_empty());
        match deadline {
            Some(deadline) if !connected && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(500));
            }
            _ => break result,
        }
    };
    print_json_output(&connected_clients_output(&result), false)
}

/// Text mode lists the runtimes of one Studio without what they share or
/// only the daemon uses: the runtime and launch ids, the user id, a
/// `localFile` that is the place's own name, and the pid and place of the
/// play runtimes, which are the Edit window's. Returns whether the listing is
/// one Studio's; several Studios keep their records whole.
pub(crate) fn compact_one_studio_clients(clients: &mut [Value]) -> bool {
    let mut pids = clients.iter().map(|client| client.get("pid").cloned());
    let Some(first) = pids.next().flatten() else {
        return false;
    };
    if pids.any(|pid| pid.as_ref() != Some(&first)) {
        return false;
    }
    for client in clients.iter_mut().filter_map(Value::as_object_mut) {
        for key in [
            "runtimeId",
            "launchEditRuntimeId",
            "launchNonce",
            "playerUserId",
        ] {
            client.remove(key);
        }
        let named_like_the_place = client
            .get("localFile")
            .and_then(Value::as_str)
            .and_then(|file| Path::new(file).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| client.get("placeName").and_then(Value::as_str) == Some(name));
        if named_like_the_place {
            client.remove("localFile");
        }
        if client.get("role").and_then(Value::as_str) != Some("edit") {
            for key in ["pid", "placeName", "localFile"] {
                client.remove(key);
            }
        }
    }
    true
}

/// The answer `rbx cs` prints for a daemon listing of connected runtimes:
/// each client without its bridge plumbing, and the orphaned test windows.
pub(crate) fn connected_clients_output(result: &Value) -> Value {
    let mut clients = result
        .get("clients")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Ports and channels are daemon plumbing; a runtime id only selects
    // between several Studios.
    let single = clients.len() == 1;
    let text = !crate::app::output::global_json_output();
    for client in clients.iter_mut().filter_map(Value::as_object_mut) {
        client.remove("channels");
        client.remove("ports");
        if text {
            for key in ["bridgeBuildUnix", "launchNonce", "launchEditRuntimeId"] {
                client.remove(key);
            }
            for key in ["gameId", "placeId"] {
                if client.get(key).and_then(Value::as_i64) == Some(0) {
                    client.remove(key);
                }
            }
        }
        if single {
            client.remove("runtimeId");
        }
    }
    if text {
        compact_one_studio_clients(&mut clients);
    }
    let mut output = json!({ "clients": clients });
    if let Some(orphans) = result.get("orphans") {
        output["orphans"] = orphans.clone();
    }
    output
}

pub(crate) fn editor_review_decision_result(
    args: &EditorReviewDecisionArgs,
    bridge: &BridgeServer,
) -> Result<Value> {
    let result = bridge.call(
        "setEditorPushReviewDecision",
        json!({
            "reviewId": args.review_id,
            "decision": args.decision,
        }),
    )?;
    if result.get("accepted").and_then(Value::as_bool) != Some(true) {
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Studio has no matching review awaiting a decision");
        bail!("{error}");
    }
    Ok(result)
}

pub(crate) fn editor_review_decision_command(args: EditorReviewDecisionArgs) -> Result<()> {
    let parameters = json!({
        "studioDecision": true,
        "decision": args.decision,
        "reviewId": args.review_id,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
    });
    let result = try_daemon_control_request(op::REVIEW_APPLY, parameters)?
        .context("No Renium review is active")?;
    print_json_output(&result, false)
}

fn single_play_launch_nonce(clients: &[Value]) -> Option<String> {
    let mut nonces = clients
        .iter()
        .filter_map(|entry| {
            entry
                .get("launchNonce")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    nonces.sort_unstable();
    nonces.dedup();
    (nonces.len() == 1).then(|| nonces.remove(0))
}

fn studio_play_clients(bridge: &BridgeServer, edit_runtime_id: &str) -> Vec<Value> {
    let clients = bridge.list_bridge_clients();
    #[cfg(any(windows, target_os = "macos"))]
    let same_process_runtime_ids = bridge
        .studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id)
        .map(|edit_pid| {
            clients
                .iter()
                .filter_map(|entry| {
                    let role = entry.get("role").and_then(Value::as_str)?;
                    let target = match role {
                        BRIDGE_ROLE_PLAY_SERVER => BridgeTarget::Main,
                        BRIDGE_ROLE_PLAY_CLIENT => BridgeTarget::Client,
                        _ => return None,
                    };
                    let runtime_id = entry.get("runtimeId").and_then(Value::as_str)?;
                    (bridge.studio_pid_for_runtime(target, runtime_id).ok() == Some(edit_pid))
                        .then(|| runtime_id.to_string())
                })
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    #[cfg(not(any(windows, target_os = "macos")))]
    let same_process_runtime_ids = HashSet::<String>::new();

    clients
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.get("role").and_then(Value::as_str),
                Some(BRIDGE_ROLE_PLAY_SERVER | BRIDGE_ROLE_PLAY_CLIENT)
            ) && (entry.get("launchEditRuntimeId").and_then(Value::as_str) == Some(edit_runtime_id)
                || entry
                    .get("runtimeId")
                    .and_then(Value::as_str)
                    .is_some_and(|runtime_id| same_process_runtime_ids.contains(runtime_id)))
        })
        .collect()
}

fn runtime_stopped_state(
    bridge: &BridgeServer,
    target: BridgeTarget,
    runtime_id: &str,
) -> Option<bool> {
    let status = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({}),
        target,
        runtime_id,
        Some(Duration::from_millis(500)),
    );
    log_global(
        5,
        format_args!(
            "[renium] runtime {target:?} {runtime_id} status: {}",
            match &status {
                Ok(value) => serde_json::to_string(value).unwrap_or_default(),
                Err(error) => format!("error {error:#}"),
            }
        ),
    );
    status.ok().map(|status| play_status_is_stopped(&status))
}

fn play_client_stopped_state(bridge: &BridgeServer, client: &Value) -> Option<bool> {
    let runtime_id = client.get("runtimeId").and_then(Value::as_str)?;
    let target = match client.get("role").and_then(Value::as_str) {
        Some(BRIDGE_ROLE_PLAY_SERVER) => BridgeTarget::Main,
        Some(BRIDGE_ROLE_PLAY_CLIENT) => BridgeTarget::Client,
        _ => return None,
    };
    runtime_stopped_state(bridge, target, runtime_id)
}

fn play_runtime_is_active(edit_stopped: Option<bool>, play_stopped: Option<bool>) -> bool {
    match play_stopped {
        Some(stopped) => !stopped,
        None => edit_stopped != Some(true),
    }
}

fn play_status_is_running(status: &Value) -> bool {
    status.get("running").and_then(Value::as_bool) == Some(true)
        || status.get("starting").and_then(Value::as_bool) == Some(true)
}

/// A session is stopped once the engine and the controller report no run and
/// no pending execution. `readyForStart` stays false for the controller's
/// restart settle after that; `wait_for_studio_play_ready` waits for it before
/// the next start, so a stop does not.
fn play_status_is_stopped(status: &Value) -> bool {
    !play_status_is_running(status)
        && status.get("running").and_then(Value::as_bool) == Some(false)
        && status.get("executing").and_then(Value::as_bool) != Some(true)
}

fn retire_play_clients(bridge: &BridgeServer, clients: &[Value]) {
    for client in clients {
        if let Some(runtime_id) = client.get("runtimeId").and_then(Value::as_str) {
            bridge.retire_runtime(runtime_id);
        }
    }
}

fn play_stop_runtimes<'a>(clients: &'a [Value], edit: Option<&Value>) -> Vec<&'a Value> {
    let source = edit
        .and_then(|edit| {
            clients.iter().find(|client| {
                client["role"] == BRIDGE_ROLE_PLAY_SERVER
                    && client["placeId"] == edit["placeId"]
                    && client["gameId"] == edit["gameId"]
            })
        })
        .or_else(|| {
            let mut servers = clients
                .iter()
                .filter(|client| client["role"] == BRIDGE_ROLE_PLAY_SERVER);
            let only = servers.next()?;
            servers.next().is_none().then_some(only)
        });
    // Only the server that matches the Edit place receives EndTest. Sending it
    // from several servers concurrently completes the shared executor twice
    // and can crash Edit; a server that does not match is not this session's.
    clients
        .iter()
        .filter(|client| {
            client["role"] != BRIDGE_ROLE_PLAY_SERVER
                || source.is_some_and(|source| std::ptr::eq(*client, source))
        })
        .collect()
}

fn request_play_runtimes_to_stop(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    clients: &[Value],
    launch_nonce: Option<&str>,
) {
    let mut params = Map::new();
    params.insert("stop".to_string(), Value::Bool(true));
    params.insert("waitForStopped".to_string(), Value::Bool(false));
    if let Some(launch_nonce) = launch_nonce {
        params.insert(
            "launchNonce".to_string(),
            Value::String(launch_nonce.to_string()),
        );
    }
    let inventory = bridge.list_bridge_clients();
    let edit = inventory
        .iter()
        .find(|row| row["role"] == BRIDGE_ROLE_EDIT && row["runtimeId"] == edit_runtime_id);
    // The runtimes are told at the same time, with a short wait: a runtime
    // that is already tearing down never answers, and the Edit window's own
    // stop call is what ends the session.
    let targets = play_stop_runtimes(clients, edit)
        .into_iter()
        .filter_map(|client| {
            let target = match client.get("role").and_then(Value::as_str) {
                Some(BRIDGE_ROLE_PLAY_SERVER) => BridgeTarget::Main,
                Some(BRIDGE_ROLE_PLAY_CLIENT) => BridgeTarget::Client,
                _ => return None,
            };
            let runtime_id = client.get("runtimeId").and_then(Value::as_str)?.to_string();
            Some((target, runtime_id))
        })
        .collect::<Vec<_>>();
    thread::scope(|scope| {
        for (target, runtime_id) in &targets {
            let params = params.clone();
            scope.spawn(move || {
                let answer = bridge.call_for_runtime_with_timeout(
                    "startStopPlay",
                    Value::Object(params),
                    *target,
                    runtime_id,
                    Some(Duration::from_millis(600)),
                );
                log_global(
                    5,
                    format_args!(
                        "[renium] play stop request to {target:?} {runtime_id}: {}",
                        match &answer {
                            Ok(value) => serde_json::to_string(value).unwrap_or_default(),
                            Err(error) => format!("error {error:#}"),
                        }
                    ),
                );
            });
        }
    });
}

/// This Edit window's play runtimes, split into running ones and ones whose
/// test has stopped while their DataModel is still open.
fn play_clients_by_state(bridge: &BridgeServer, edit_runtime_id: &str) -> (Vec<Value>, Vec<Value>) {
    let clients = studio_play_clients(bridge, edit_runtime_id);
    if clients.is_empty() {
        return (clients, Vec::new());
    }
    let edit_stopped = runtime_stopped_state(bridge, BridgeTarget::Edit, edit_runtime_id);
    clients.into_iter().partition(|client| {
        play_runtime_is_active(edit_stopped, play_client_stopped_state(bridge, client))
    })
}

#[cfg(any(windows, target_os = "macos"))]
fn separate_play_processes(bridge: &BridgeServer, edit_runtime: &str) -> Vec<u32> {
    let Ok(edit_pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime) else {
        return Vec::new();
    };
    studio_play_clients(bridge, edit_runtime)
        .iter()
        .filter_map(|client| {
            let target = if client["role"] == BRIDGE_ROLE_PLAY_CLIENT {
                BridgeTarget::Client
            } else {
                BridgeTarget::Main
            };
            let pid = bridge
                .studio_pid_for_runtime(target, client["runtimeId"].as_str()?)
                .ok()?;
            (pid != edit_pid).then_some(pid)
        })
        .collect()
}

#[cfg(any(windows, target_os = "macos"))]
type PlayProcessExits = Vec<process_exit::ProcessExit>;
#[cfg(not(any(windows, target_os = "macos")))]
type PlayProcessExits = Vec<std::convert::Infallible>;

#[cfg(any(windows, target_os = "macos"))]
fn watch_play_processes(bridge: &BridgeServer, edit_runtime: &str) -> Result<PlayProcessExits> {
    separate_play_processes(bridge, edit_runtime)
        .into_iter()
        .map(process_exit::ProcessExit::watch)
        .filter_map(Result::transpose)
        .collect()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn watch_play_processes(_bridge: &BridgeServer, _edit_runtime: &str) -> Result<PlayProcessExits> {
    Ok(Vec::new())
}

// A stopped test DataModel closes its bridge channels when Studio tears it down
// and its plugin unloads. A test started before that is dropped by Studio.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Splits play runtimes into those hosted by the Edit window's own process,
/// whose DataModels close without a process exit to wait for, and the rest.
fn split_in_process(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    clients: Vec<Value>,
) -> (Vec<Value>, Vec<Value>) {
    let Ok(edit_pid) = bridge.studio_pid_for_runtime(BridgeTarget::Edit, edit_runtime_id) else {
        return (clients, Vec::new());
    };
    clients.into_iter().partition(|client| {
        client
            .get("pid")
            .and_then(Value::as_u64)
            .is_none_or(|pid| pid == u64::from(edit_pid))
    })
}

fn wait_for_test_datamodels_to_close(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    deadline: Instant,
) -> Duration {
    let started = Instant::now();
    let deadline = deadline.min(started + TEARDOWN_TIMEOUT);
    while Instant::now() < deadline
        && !split_in_process(
            bridge,
            edit_runtime_id,
            studio_play_clients(bridge, edit_runtime_id),
        )
        .0
        .is_empty()
    {
        thread::sleep(Duration::from_millis(100));
    }
    started.elapsed()
}

fn finish_stop(
    bridge: &BridgeServer,
    edit_runtime_id: &str,
    processes: &PlayProcessExits,
    shutdown_deadline: Instant,
    result: &mut Value,
) -> Result<()> {
    let teardown = wait_for_test_datamodels_to_close(bridge, edit_runtime_id, shutdown_deadline);
    #[cfg(any(windows, target_os = "macos"))]
    process_exit::wait(processes, shutdown_deadline)?;
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = processes;
    let closed = close_leftover_test_processes(
        bridge,
        edit_runtime_id,
        Duration::from_secs(5),
        &thread::sleep,
    );
    retire_play_clients(bridge, &studio_play_clients(bridge, edit_runtime_id));
    bridge.clear_runtime_pins();
    if teardown >= Duration::from_millis(250) {
        result["waitedForTeardownMs"] = json!(teardown.as_millis());
    }
    if !closed.is_empty() {
        result["closedProcesses"] = json!(closed);
    }
    Ok(())
}

fn stop_studio_play_with_bridge_result(bridge: &BridgeServer) -> Result<Value> {
    let edit_pin = bridge.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
    let edit_runtime_id = edit_pin.runtime_id;
    #[cfg(windows)]
    crate::project::workflows::windows_launch::protect_process(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, &edit_runtime_id)?,
    )?;
    #[cfg(target_os = "macos")]
    crate::studio::native::serializer::protect_studio_launch(
        bridge.studio_pid_for_runtime(BridgeTarget::Edit, &edit_runtime_id)?,
    )?;
    // Closing a test DataModel precedes process exit on Windows and macOS. Its later exit
    // notification can end the next multiplayer launch if stop returns early.
    let processes = watch_play_processes(bridge, &edit_runtime_id)?;
    #[cfg(any(windows, target_os = "macos"))]
    let shutdown_deadline = Instant::now() + process_exit::STOP_TIMEOUT;
    #[cfg(not(any(windows, target_os = "macos")))]
    let shutdown_deadline = Instant::now() + Duration::from_secs(40);
    let mut initial = studio_play_status_for_runtime(bridge, &edit_runtime_id)?;
    let (mut active_clients, _) = play_clients_by_state(bridge, &edit_runtime_id);
    if play_status_is_stopped(&initial) && active_clients.is_empty() {
        finish_stop(
            bridge,
            &edit_runtime_id,
            &processes,
            shutdown_deadline,
            &mut initial,
        )?;
        return Ok(initial);
    }
    let launch_nonce = initial
        .get("launchNonce")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let launch_nonce = launch_nonce.or_else(|| single_play_launch_nonce(&active_clients));
    let mut last_status = Value::Null;
    for attempt in 1..=3 {
        let mut params = Map::new();
        params.insert("stop".to_string(), Value::Bool(true));
        if let Some(launch_nonce) = launch_nonce.as_ref() {
            params.insert(
                "launchNonce".to_string(),
                Value::String(launch_nonce.clone()),
            );
        }
        params.insert("waitForStopped".to_string(), Value::Bool(false));
        request_play_runtimes_to_stop(
            bridge,
            &edit_runtime_id,
            &active_clients,
            launch_nonce.as_deref(),
        );
        let stop_result = bridge.call_for_runtime_with_timeout(
            "startStopPlay",
            Value::Object(params),
            BridgeTarget::Edit,
            &edit_runtime_id,
            Some(Duration::from_secs(2)),
        );
        let stop_result = match stop_result {
            Ok(result) => result,
            Err(error) => {
                last_status = json!({ "error": format!("{error:#}") });
                continue;
            }
        };
        ensure_plugin_api_ok(&stop_result)?;
        let mut stopped = false;
        while Instant::now() < shutdown_deadline {
            let status_stopped = match studio_play_status_for_runtime(bridge, &edit_runtime_id) {
                Ok(status) => {
                    last_status = status.clone();
                    active_clients = play_clients_by_state(bridge, &edit_runtime_id).0;
                    log_global(
                        5,
                        format_args!(
                            "[renium] play stop wait: edit stopped={} active={:?}",
                            play_status_is_stopped(&status),
                            active_clients
                                .iter()
                                .map(|client| {
                                    format!(
                                        "{}:{}",
                                        client["role"].as_str().unwrap_or("?"),
                                        client["runtimeId"].as_str().unwrap_or("?")
                                    )
                                })
                                .collect::<Vec<_>>()
                        ),
                    );
                    play_status_is_stopped(&status) && active_clients.is_empty()
                }
                Err(err) => {
                    last_status = json!({ "error": format!("{:#}", err) });
                    log_global(
                        5,
                        format_args!("[renium] play stop wait: status error {err:#}"),
                    );
                    false
                }
            };
            stopped = status_stopped;
            if stopped {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if stopped {
            let mut result = json!({
                "ok": true,
                "action": "stop",
                "method": "pluginApi",
                "attempts": attempt,
                "status": last_status,
            });
            finish_stop(
                bridge,
                &edit_runtime_id,
                &processes,
                shutdown_deadline,
                &mut result,
            )?;
            return Ok(result);
        }
        if Instant::now() >= shutdown_deadline {
            break;
        }
    }
    bail!(
        "Studio did not finish stopping after the plugin stop request; last status: {}",
        serde_json::to_string(&last_status)?
    )
}

fn studio_play_status_for_runtime(bridge: &BridgeServer, runtime_id: &str) -> Result<Value> {
    let status = bridge.call_for_runtime_with_timeout(
        "startStopPlay",
        json!({}),
        BridgeTarget::Edit,
        runtime_id,
        Some(Duration::from_millis(1_100)),
    )?;
    ensure_plugin_api_ok(&status)?;
    Ok(status)
}

/// Waits until the Edit window can start a test: its controller is ready and
/// the previous test's DataModels are gone. Returns the status, which reports
/// a running session when one is still active.
fn wait_for_studio_play_ready(
    bridge: &BridgeServer,
    runtime_id: &str,
    wait: PollWait<'_>,
) -> Result<Value> {
    let deadline = Instant::now() + BRIDGE_DEFAULT_RESPONSE_TIMEOUT;
    loop {
        let mut status = studio_play_status_for_runtime(bridge, runtime_id)?;
        let (clients, closing) = play_clients_by_state(bridge, runtime_id);
        if !clients.is_empty() {
            retire_play_clients(bridge, &closing);
            let launch_nonce = single_play_launch_nonce(&clients);
            status["running"] = Value::Bool(true);
            status["starting"] = Value::Bool(false);
            status["clients"] = json!(clients);
            if let Some(launch_nonce) = launch_nonce {
                status["launchNonce"] = Value::String(launch_nonce);
            }
            return Ok(status);
        }
        let (closing, separate) = split_in_process(bridge, runtime_id, closing);
        retire_play_clients(bridge, &separate);
        if play_status_is_running(&status) {
            return Ok(status);
        }
        let controller_ready = status.get("readyForStart").and_then(Value::as_bool) != Some(false);
        if controller_ready && closing.is_empty() {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            if controller_ready {
                retire_play_clients(bridge, &closing);
                return Ok(status);
            }
            bail!("Studio did not finish the previous play session before the next start");
        }
        wait(Duration::from_millis(100));
    }
}

pub(crate) struct TestLaunch {
    pub(crate) nonce: String,
    pub(crate) edit_runtime_id: String,
}

#[cfg(test)]
mod play_state_tests {
    use super::*;

    #[test]
    fn a_play_target_luau_failure_keeps_its_console_lines() {
        let error = ensure_luau_ok_in_context(
            &json!({
                "ok": false,
                "context": "server",
                "error": "Requested module experienced an error while loading",
                "consoleErrors": ["ReplicatedStorage.Bad:1: bad module"],
            }),
            None,
            Some("return require(game.ReplicatedStorage.Bad)"),
        )
        .unwrap_err();
        let run = error
            .downcast_ref::<crate::app::output::LuauRunFailure>()
            .expect("the typed failure survives the context label");
        assert_eq!(
            run.console_errors,
            vec!["ReplicatedStorage.Bad:1: bad module"]
        );
        assert!(
            error
                .to_string()
                .starts_with("[play server] Requested module")
        );
        assert!(error.to_string().contains("Studio logged during the run:"));
    }

    #[test]
    fn an_edit_run_that_requires_a_clone_is_told_why_it_indexed_nil() {
        let failed = json!({
            "ok": false,
            "context": "edit",
            "error": "UIEffects:11: attempt to index nil with 'NumberFormat'",
        });
        let code =
            "local U = require(game.ReplicatedStorage.Misc.UIEffects:Clone()) return U.Cash(1)";
        let error = ensure_luau_ok_in_context(&failed, None, Some(code))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("[Edit window] UIEffects:11:"), "{error}");
        assert!(
            error.contains("\nHint: a cloned ModuleScript has no Parent"),
            "{error}"
        );
        let unexplained = ensure_luau_ok_in_context(&failed, None, None)
            .unwrap_err()
            .to_string();
        assert!(!unexplained.contains("Hint:"), "{unexplained}");
    }

    #[test]
    fn play_stop_completes_only_the_source_executor_and_still_stops_orphaned_servers() {
        let edit = json!({"placeId":11,"gameId":7});
        let source = json!({"role":"play-server","placeId":11,"gameId":7});
        let destination = json!({"role":"play-server","placeId":12,"gameId":7});
        let client = json!({"role":"play-client","placeId":12,"gameId":7});
        let multi = [destination.clone(), client.clone(), source.clone()];
        assert_eq!(play_stop_runtimes(&multi, Some(&edit)), [&client, &source]);
        let solo = [destination.clone(), client.clone()];
        assert_eq!(
            play_stop_runtimes(&solo, Some(&edit)),
            [&destination, &client]
        );
        let third = json!({"role":"play-server","placeId":13,"gameId":7});
        let disconnected_source = [destination, client.clone(), third];
        assert_eq!(
            play_stop_runtimes(&disconnected_source, Some(&edit)),
            [&client]
        );
    }

    #[test]
    fn camera_readback_checks_direction_as_well_as_position() {
        let result = json!({"results":[[1.0,2.0,3.0,"Enum.CameraType.Scriptable",0.0,0.0,-1.0]]});
        assert_eq!(
            camera_readback(&result, [1.0, 2.0, 3.0], [1.0, 2.0, 0.0])["moved"],
            true
        );
        let turned = camera_readback(&result, [1.0, 2.0, 3.0], [4.0, 2.0, 3.0]);
        assert_eq!(turned["moved"], false);
        assert!(turned["note"].is_string());
        assert_eq!(
            camera_readback(&result, [-10.0, 2.0, 3.0], [1.0, 2.0, 0.0])["moved"],
            false
        );
        assert_eq!(
            camera_readback(&json!({"results":[]}), [1.0, 2.0, 3.0], [1.0, 2.0, 0.0])["moved"],
            false
        );
    }

    #[test]
    fn runner_names_are_short_identifiers() {
        assert!(runner_name("jump-sampler_2").is_ok());
        assert!(runner_name("").is_err());
        assert!(runner_name("has space").is_err());
        assert!(runner_name(&"x".repeat(65)).is_err());
    }

    fn play_args(parts: &[&str]) -> std::result::Result<StartStopPlayArgs, clap::Error> {
        use clap::Parser;
        match crate::cli::Cli::try_parse_from(parts)?.command {
            crate::cli::Commands::StartStopPlay(args) => Ok(args),
            _ => panic!("expected play"),
        }
    }

    #[test]
    fn play_at_takes_a_path_or_a_position() {
        let args = play_args(&["rbx", "play", "-s", "--at", "Workspace.Lobby.SpawnPad"]).unwrap();
        assert_eq!(args.at.as_deref(), Some("Workspace.Lobby.SpawnPad"));
        let code = stand_player_code("game.workspace.Lobby.SpawnPad");
        assert!(
            code.contains(r#"{"Workspace", "Lobby", "SpawnPad"}"#),
            "{code}"
        );
        assert!(code.contains("inst:GetPivot() * CFrame.new(0, size.Y / 2, 0)"));
        assert!(code.contains("character:PivotTo(goal * CFrame.new(0, 3, 0))"));
        let code = stand_player_code("30, 4, 0");
        assert!(code.contains("local goal = CFrame.new(30, 4, 0)"), "{code}");
        assert!(play_args(&["rbx", "play", "-x", "--at", "Workspace.Pad"]).is_err());
    }

    #[test]
    fn one_client_requires_explicit_mode_except_when_leaving() {
        for action in ["-s", "-r"] {
            let args = play_args(&["rbx", "play", action, "--players", "1"]).unwrap();
            assert!(
                validate_play_args(&args)
                    .unwrap_err()
                    .to_string()
                    .contains("ordinary Play")
            );
            let args =
                play_args(&["rbx", "play", action, "--players", "1", "--mode", "play"]).unwrap();
            validate_play_args(&args).unwrap();
        }
        validate_play_args(&play_args(&["rbx", "play", "--leave", "-p", "1"]).unwrap()).unwrap();
        validate_play_args(&play_args(&["rbx", "play", "-s", "--players", "2"]).unwrap()).unwrap();
    }

    #[test]
    fn add_players_parses_a_count_from_one_to_eight_on_its_own() {
        let args = play_args(&["rbx", "play", "--add-players", "2"]).unwrap();
        assert_eq!(args.add_players, Some(2));
        assert!(!args.start && !args.leave);
        validate_play_args(&args).unwrap();
        for parts in [
            &["rbx", "play", "--add-players", "0"][..],
            &["rbx", "play", "--add-players", "9"],
            &["rbx", "play", "--add-players", "2", "-s"],
            &["rbx", "play", "--add-players", "2", "-x"],
            &["rbx", "play", "--add-players", "2", "-p", "3"],
            &["rbx", "play", "--add-players", "2", "--leave"],
            &["rbx", "play", "--add-players", "2", "--until", "true"],
        ] {
            assert!(play_args(parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn leave_needs_a_client_index_and_no_session_flags() {
        let args = play_args(&["rbx", "play", "--leave", "-p", "2"]).unwrap();
        assert!(args.leave && !args.start);
        assert_eq!(args.players, Some(2));
        validate_play_args(&args).unwrap();
        let missing = validate_play_args(&play_args(&["rbx", "play", "--leave"]).unwrap());
        assert!(missing.unwrap_err().to_string().contains("--leave -p N"));
        let zero = validate_play_args(&play_args(&["rbx", "play", "--leave", "-p", "0"]).unwrap());
        assert!(zero.unwrap_err().to_string().contains("start at 1"));
        for parts in [
            &["rbx", "play", "--leave", "-p", "1", "-x"][..],
            &["rbx", "play", "--leave", "-p", "1", "-s"],
            &["rbx", "play", "--leave", "-p", "1", "-r"],
            &["rbx", "play", "--leave", "-p", "1", "--mode", "run"],
        ] {
            assert!(play_args(parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn added_clients_wait_while_new_clients_keep_arriving() {
        let started = Instant::now();
        assert_eq!(
            added_clients_deadline(started, started),
            started + Duration::from_secs(30)
        );
        assert_eq!(
            added_clients_deadline(started, started + Duration::from_secs(25)),
            started + Duration::from_secs(55)
        );
        assert_eq!(
            added_clients_deadline(started, started + Duration::from_secs(110)),
            started + Duration::from_secs(120)
        );
    }

    #[test]
    fn multiplayer_start_waits_while_instances_keep_arriving() {
        let started = Instant::now();
        assert_eq!(
            multiplayer_start_deadline(started, started),
            started + Duration::from_secs(90)
        );
        assert_eq!(
            multiplayer_start_deadline(started, started + Duration::from_secs(80)),
            started + Duration::from_secs(140)
        );
        assert_eq!(
            multiplayer_start_deadline(started, started + Duration::from_secs(600)),
            started + Duration::from_secs(240)
        );
    }

    #[test]
    fn a_settling_controller_counts_as_stopped_but_a_pending_execution_does_not() {
        assert!(play_status_is_stopped(&json!({
            "running": false, "starting": false, "readyForStart": true
        })));
        assert!(play_status_is_stopped(&json!({
            "running": false, "starting": false, "readyForStart": false
        })));
        assert!(!play_status_is_stopped(&json!({
            "running": false, "starting": false, "readyForStart": false, "executing": true
        })));
        assert!(!play_status_is_stopped(&json!({
            "running": false, "starting": true, "readyForStart": false
        })));
    }

    #[test]
    fn play_runtime_activity_prefers_the_runtime_over_a_stale_editor_state() {
        assert!(play_runtime_is_active(Some(true), Some(false)));
        assert!(!play_runtime_is_active(Some(true), None));
        assert!(play_runtime_is_active(Some(false), None));
        assert!(play_runtime_is_active(None, None));
    }

    #[test]
    fn stale_bridge_failure_does_not_count_as_running() {
        assert!(!play_status_is_running(&json!({
            "ok": false,
            "error": "DataModel stopped",
        })));
        assert!(play_status_is_running(&json!({ "running": true })));
    }

    #[test]
    fn orphans_match_a_place_path_by_its_file_name() {
        let orphan = json!({ "placeName": "Scenario.rbxl", "placeId": 0 });
        assert!(client_place_matches(
            &orphan,
            r"E:\runs\small-1\Scenario.rbxl"
        ));
        assert!(client_place_matches(
            &orphan,
            "E:/runs/small-1/scenario.RBXL"
        ));
        assert!(client_place_matches(&orphan, "Scenario.rbxl"));
        assert!(!client_place_matches(
            &orphan,
            r"E:\runs\small-1\Other.rbxl"
        ));
        let documented = json!({ "placeName": r"E:\runs\small-1\Scenario.rbxl", "placeId": 0 });
        assert!(client_place_matches(
            &documented,
            r"E:\runs\small-1\Scenario.rbxl"
        ));
        assert!(!client_place_matches(
            &documented,
            r"E:\runs\small-2\Scenario.rbxl"
        ));
        let published = json!({ "placeName": "Scenario", "placeId": 20, "gameId": 10 });
        assert!(client_place_matches(&published, "10:20"));
        assert!(!client_place_matches(
            &published,
            r"E:\runs\small-1\Scenario.rbxl"
        ));
    }

    #[test]
    fn restart_reuses_the_launch_unless_flags_override_it() {
        let restart = play_args(&["rbx", "play", "-r"]).unwrap();
        assert!(restart.restart && restart.players.is_none());
        assert!(play_args(&["rbx", "play", "-r", "-x"]).is_err());
        let multi = Some(PlayLaunchPlan::Multi(2));
        assert_eq!(
            restart_plan(&restart, multi, true),
            (PlayLaunchPlan::Multi(2), None)
        );
        assert_eq!(
            restart_plan(&restart, Some(PlayLaunchPlan::Run), true),
            (PlayLaunchPlan::Run, None)
        );
        assert_eq!(
            restart_plan(&restart, multi, false),
            (PlayLaunchPlan::Multi(2), None)
        );
        assert_eq!(
            restart_plan(&restart, None, true),
            (PlayLaunchPlan::Play, Some(RESTART_OUTSIDE_RENIUM))
        );
        assert_eq!(
            restart_plan(&restart, None, false),
            (PlayLaunchPlan::Play, Some(RESTART_NOTHING_KNOWN))
        );
        let three = play_args(&["rbx", "play", "-r", "-p", "3"]).unwrap();
        assert_eq!(
            restart_plan(&three, multi, true),
            (PlayLaunchPlan::Multi(3), None)
        );
        let solo = play_args(&["rbx", "play", "-r", "--mode", "play"]).unwrap();
        assert_eq!(
            restart_plan(&solo, multi, true),
            (PlayLaunchPlan::Play, None)
        );
        let server = play_args(&["rbx", "play", "-r", "--mode", "server"]).unwrap();
        assert_eq!(
            restart_plan(&server, None, true),
            (PlayLaunchPlan::Run, None)
        );
    }

    #[test]
    fn the_edit_controller_names_how_its_session_was_launched() {
        let status = |mode: &str, players: Value| json!({ "running": true, "launchNonce": "multi-1", "mode": mode, "players": players });
        let clients = [
            json!({ "role": BRIDGE_ROLE_PLAY_SERVER }),
            json!({ "role": BRIDGE_ROLE_PLAY_CLIENT }),
            json!({ "role": BRIDGE_ROLE_PLAY_CLIENT }),
        ];
        assert_eq!(
            running_session_plan(&status("multi", json!(3)), &clients, None),
            Some(PlayLaunchPlan::Multi(3))
        );
        assert_eq!(
            running_session_plan(
                &status("multi", Value::Null),
                &clients,
                Some(PlayLaunchPlan::Multi(4))
            ),
            Some(PlayLaunchPlan::Multi(4))
        );
        assert_eq!(
            running_session_plan(&status("multi", Value::Null), &clients, None),
            Some(PlayLaunchPlan::Multi(2))
        );
        assert_eq!(
            running_session_plan(&status("multi", Value::Null), &[], None),
            None
        );
        assert_eq!(
            running_session_plan(&status("play", Value::Null), &[], None),
            Some(PlayLaunchPlan::Play)
        );
        assert_eq!(
            running_session_plan(&status("run", Value::Null), &[], None),
            Some(PlayLaunchPlan::Run)
        );
        let manual = json!({ "running": true, "mode": Value::Null });
        assert_eq!(
            running_session_plan(&manual, &clients, Some(PlayLaunchPlan::Multi(2))),
            None
        );
        let unowned = json!({ "running": true, "launchNonce": "", "mode": "play" });
        assert_eq!(running_session_plan(&unowned, &[], None), None);
    }

    #[test]
    fn an_ignored_start_is_told_apart_from_a_slow_one() {
        let second = Duration::from_secs(1);
        for plan in [PlayLaunchPlan::Play, PlayLaunchPlan::Multi(2)] {
            let silent = plan.sign_timeout();
            assert!(!plan.start_was_ignored(second, false, Some(true)));
            assert!(!plan.start_was_ignored(second, false, Some(false)));
            assert!(plan.start_was_ignored(START_RETURN_GRACE, false, Some(false)));
            assert!(!plan.start_was_ignored(START_RETURN_GRACE, false, Some(true)));
            assert!(!plan.start_was_ignored(START_RETURN_GRACE, false, None));
            assert!(!plan.start_was_ignored(silent - second, false, Some(true)));
            assert!(plan.start_was_ignored(silent, false, Some(true)));
            assert!(plan.start_was_ignored(silent, false, None));
            assert!(!plan.start_was_ignored(silent * 10, true, Some(false)));
        }
        assert_eq!(PlayLaunchPlan::Run.sign_timeout(), SINGLE_START_TIMEOUT);
        assert!(PlayLaunchPlan::Multi(1).sign_timeout() > SINGLE_START_TIMEOUT);
    }

    #[test]
    fn launch_plans_ask_studio_for_their_own_session() {
        assert_eq!(
            PlayLaunchPlan::Multi(2).request("n"),
            json!({ "start": true, "players": 2, "launchNonce": "n" })
        );
        assert_eq!(
            PlayLaunchPlan::Run.request("n"),
            json!({ "start": true, "mode": "run", "launchNonce": "n" })
        );
        assert_eq!(PlayLaunchPlan::Play.clients_needed(), 1);
        assert_eq!(PlayLaunchPlan::Run.clients_needed(), 0);
        assert_eq!(PlayLaunchPlan::Multi(3).clients_needed(), 3);
        let started = Instant::now();
        assert_eq!(
            PlayLaunchPlan::Play.start_deadline(started, started + Duration::from_secs(5)),
            started + SINGLE_START_TIMEOUT
        );
        assert_eq!(
            PlayLaunchPlan::Multi(2).start_deadline(started, started),
            multiplayer_start_deadline(started, started)
        );
    }

    #[test]
    fn a_start_error_before_the_test_appears_is_retried() {
        let launch = TestLaunch {
            nonce: "multi-7".into(),
            edit_runtime_id: "edit".into(),
        };
        let plan = PlayLaunchPlan::Multi(2);
        let refused = json!({ "ok": true, "lastError": "Studio is busy" });
        match launch_status_outcome(&refused, &launch, plan, false).unwrap() {
            Some(LaunchOutcome::Ignored(reason)) => assert!(reason.contains("Studio is busy")),
            _ => panic!("a refusal before the test began must be retried"),
        }
        let failed = launch_status_outcome(&refused, &launch, plan, true)
            .err()
            .unwrap();
        assert!(
            failed
                .to_string()
                .contains("could not start the multiplayer session")
        );
        let other = json!({ "ok": true, "launchNonce": "multi-8" });
        assert!(launch_status_outcome(&other, &launch, plan, false).is_err());
        let current = json!({ "ok": true, "launchNonce": "multi-7", "starting": true });
        assert!(
            launch_status_outcome(&current, &launch, plan, false)
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod settled_start_tests {
    use super::*;

    #[test]
    fn live_start_with_wait_reports_the_settled_daemon_status() {
        let mut result =
            json!({"ok": true, "tracking": true, "daemon": {"running": true, "settled": false}});
        merge_settled_status(
            &mut result,
            json!({"ok": true, "daemon": {"running": true, "settled": true, "pushes": 2}}),
        );
        assert_eq!(result["ok"], true);
        assert_eq!(result["tracking"], true);
        assert_eq!(
            result["daemon"],
            json!({"running": true, "settled": true, "pushes": 2})
        );

        let mut result = json!({"ok": true, "daemon": {"running": true}});
        merge_settled_status(
            &mut result,
            json!({"ok": false, "error": "Live Sync did not settle before the wait ended: a sync pass is still running", "daemon": {"running": true, "settled": false}}),
        );
        assert_eq!(result["ok"], false);
        assert_eq!(result["daemon"]["settled"], false);
        assert_eq!(
            result["error"],
            "Live Sync did not settle before the wait ended: a sync pass is still running"
        );
    }

    #[test]
    fn live_status_parameters_only_carry_the_plain_status_fields() {
        let status = StudioChangeStateArgs {
            no_start: true,
            settle_wait_seconds: Some(200.0),
            ..StudioChangeStateArgs::plain(BridgeConnectionArgs::local(3.0), false)
        };
        let parameters = live_operation_parameters(&status);
        assert_eq!(parameters["settleWaitSeconds"], 120.0);
        assert_eq!(parameters["compact"], true);
        assert_eq!(parameters["manageFiles"], true);
        assert_eq!(parameters["bridgeWaitSeconds"], 3.0);
        assert_eq!(parameters["reset"], false);
        assert!(parameters["resolveConflictPreference"].is_null());
        assert_eq!(parameters["resolveConflicts"], json!([]));
        let decided = StudioChangeStateArgs {
            resolve: vec!["0a1b2c3d=studio".into()],
            ..StudioChangeStateArgs::plain(BridgeConnectionArgs::local(3.0), false)
        };
        assert_eq!(
            live_operation_parameters(&decided)["resolveConflicts"],
            json!(["0a1b2c3d=studio"])
        );
    }

    #[test]
    fn open_conflicts_print_as_records_with_what_to_do_next() {
        let conflict = json!({"id": "0a1b2c3d", "kind": "property", "field": "Transparency"});
        let result = json!({
            "ok": false,
            "error": "Sync needs review: Workspace/Door: property Transparency",
            "daemon": {
                "running": true,
                "resolutionRequired": true,
                "conflicts": [conflict],
                "moreConflicts": 3,
            },
        });
        let (outcome, printed, _) =
            crate::app::output::capture_output(crate::app::output::CaptureMode::Structured, || {
                finish_studio_change_state_command(op::LIVE_STATUS, false, result.clone(), false)
            });
        assert!(outcome.unwrap_err().is::<ReportedFailure>());
        let printed = printed.unwrap();
        assert_eq!(printed["ok"], false);
        assert_eq!(printed["conflicts"], json!([conflict]));
        assert_eq!(printed["moreConflicts"], 3);
        let next = printed["next"].as_str().unwrap();
        assert!(
            next.contains("rbx lon --resolve ID=studio|editor")
                && next.contains("3 more conflicts are not shown"),
            "{next}"
        );
        // A comparison that held without a record keeps the plain text.
        let mut bare = result;
        bare["daemon"]["conflicts"] = json!([]);
        let error = finish_studio_change_state_command(op::LIVE_STATUS, false, bare, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rbx lon --prefer studio"), "{error}");
    }

    #[test]
    fn a_compact_status_shows_twenty_conflicts_and_counts_the_rest() {
        let conflict = |index: usize| json!({"id": format!("{index:08x}"), "kind": "property"});
        let daemon = json!({
            "running": true,
            "resolutionRequired": true,
            "error": "Sync needs review",
            "conflicts": (0..25).map(conflict).collect::<Vec<_>>(),
            "differences": ["push: src/A.luau"],
        });
        let compact = compact_live_daemon_status(&daemon);
        assert_eq!(compact["conflicts"].as_array().unwrap().len(), 20);
        assert_eq!(compact["conflicts"][19], conflict(19));
        assert_eq!(compact["moreConflicts"], 5);
        assert!(compact.get("differences").is_none());
        let (shown, more) = open_conflicts(&json!({"ok": false, "daemon": compact})).unwrap();
        assert_eq!((shown.len(), more), (20, 5));

        let few = json!({"running": true, "conflicts": [conflict(1)]});
        let compact = compact_live_daemon_status(&few);
        assert!(compact.get("moreConflicts").is_none());
        assert!(open_conflicts(&json!({"daemon": {"running": true, "conflicts": []}})).is_none());
    }

    #[test]
    fn a_started_session_shows_the_decisions_it_took_without_effect() {
        let moot = json!([{"id": "0a1b2c3d", "decision": "studio", "by": ["4e5f6a7b"]}]);
        let daemon = json!({"running": true, "pushes": 0, "mootDecisions": moot});
        assert_eq!(compact_live_daemon_status(&daemon)["mootDecisions"], moot);
    }
}

#[cfg(test)]
mod luau_code_tests {
    use super::*;

    #[test]
    fn a_luau_file_loses_its_byte_order_mark() {
        let root = crate::tests::support::temp_dir("luau-file-bom");
        let path = root.join("probe.luau");
        fs::write(&path, "\u{feff}return 1\n").unwrap();
        let code = read_luau_file(&path).unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(code, "return 1\n");
        validate_luau_syntax(&code).unwrap();
    }
}

#[cfg(test)]
mod compact_result_tests {
    use super::*;

    #[test]
    fn a_duration_in_place_of_a_condition_is_a_fixed_wait() {
        assert_eq!(fixed_wait_seconds("30s"), Some(30.0));
        assert_eq!(fixed_wait_seconds("500ms"), Some(0.5));
        assert_eq!(fixed_wait_seconds(" 2m "), Some(120.0));
        assert_eq!(fixed_wait_seconds("45"), Some(45.0));
        assert_eq!(fixed_wait_seconds("7200s"), Some(3600.0));
        assert_eq!(fixed_wait_seconds("0"), None);
        assert_eq!(fixed_wait_seconds("30x"), None);
        assert_eq!(
            fixed_wait_seconds("workspace.DistributedGameTime > 8"),
            None
        );
        assert_eq!(fixed_wait_seconds("#game.Players:GetPlayers() >= 2"), None);
        assert_eq!(fixed_wait_seconds(""), None);
    }

    #[test]
    fn the_default_run_limit_covers_literal_sleeps() {
        assert_eq!(default_luau_run_timeout("return 1"), 10.0);
        assert_eq!(default_luau_run_timeout("task.wait(25) return 1"), 35.0);
        assert_eq!(
            default_luau_run_timeout("wait(4) task.wait(2.5) return 1"),
            16.5
        );
        assert_eq!(default_luau_run_timeout("task.wait(500)"), 120.0);
        assert_eq!(
            default_luau_run_timeout("task.wait(dt) task.wait() await(9)"),
            10.0
        );
        assert_eq!(default_luau_run_timeout("task.wait(4)"), 10.0);
        assert_eq!(default_luau_run_timeout("task.wait(6)"), 16.0);
        assert_eq!(default_luau_run_timeout("task.wait (25)"), 35.0);
        assert_eq!(default_luau_run_timeout("task.wait(2e1)"), 30.0);
        assert_eq!(default_luau_run_timeout("task.wait(.5) wait(19.)"), 29.5);
        assert_eq!(
            default_luau_run_timeout("repeat task.wait () until done"),
            30.0
        );
    }

    #[test]
    fn the_default_run_limit_covers_bounded_waits() {
        assert_eq!(
            default_luau_run_timeout(
                "local a = workspace.Lobby:WaitForChild('Ads', 10) return a ~= nil"
            ),
            20.0
        );
        assert_eq!(
            default_luau_run_timeout("return workspace:WaitForChild('A', 3)"),
            10.0
        );
        assert_eq!(
            default_luau_run_timeout("return workspace:WaitForChild('A')"),
            10.0
        );
        assert_eq!(
            default_luau_run_timeout(
                "local t = os.clock() while os.clock() - t < 40 and not done do task.wait(0.5) end return 1"
            ),
            50.5
        );
        assert_eq!(
            default_luau_run_timeout(
                "local deadline = os.clock() + 25 repeat task.wait() until os.clock() > deadline"
            ),
            35.0
        );
    }

    #[test]
    fn sleeps_named_in_strings_and_comments_do_not_grow_the_run_limit() {
        assert_eq!(
            default_luau_run_timeout("return \"please wait(90) here\""),
            10.0
        );
        assert_eq!(default_luau_run_timeout("-- task.wait(60)\nreturn 1"), 10.0);
        assert_eq!(
            default_luau_run_timeout("--[[ for i = 1, 3 do task.wait(1) end ]] return 1"),
            10.0
        );
        assert_eq!(
            default_luau_run_timeout("print(`while {n} wait(50)`) return [[repeat wait(70)]]"),
            10.0
        );
        assert_eq!(
            default_luau_run_timeout("print(\"wait(90)\") task.wait(25) -- wait(30)"),
            35.0
        );
    }

    #[test]
    fn the_default_run_limit_gives_a_waiting_loop_more_time() {
        let poll = "for _, part in parts do move(part) task.wait(0.6) end return out";
        assert_eq!(default_luau_run_timeout(poll), 30.0);
        assert_eq!(
            default_luau_run_timeout("repeat task.wait() until done"),
            30.0
        );
        assert_eq!(
            default_luau_run_timeout("while busy do changed:Wait() end"),
            30.0
        );
        assert_eq!(
            default_luau_run_timeout("while true do task.wait(25) end"),
            35.0
        );
        assert_eq!(
            default_luau_run_timeout("for _, part in parts do move(part) end"),
            10.0
        );
    }

    #[test]
    fn a_started_session_waits_for_its_players_unless_it_has_none() {
        assert_eq!(
            player_readiness_condition(Some(2), Some("play")).as_deref(),
            Some("#game.Players:GetPlayers() >= 2")
        );
        assert_eq!(
            player_readiness_condition(None, None).as_deref(),
            Some("#game.Players:GetPlayers() >= 1")
        );
        assert_eq!(player_readiness_condition(None, Some("run")), None);
        assert_eq!(player_readiness_condition(Some(3), Some("server")), None);
    }

    #[test]
    fn play_start_keeps_the_decision_fields() {
        let mut result = json!({
            "ok": true, "action": "start", "mode": "play", "serverReady": true,
            "clients": [{"playerName": "A", "pid": 1, "runtimeId": "x"}, {"playerName": "B", "pid": 2}],
            "editPid": 7, "editRuntimeId": "e", "launchNonce": "n", "deviceSimulation": {"ok": true},
            "until": {"action": "wait", "condition": "return true", "ok": true, "value": "true", "elapsedSeconds": 1.5}
        });
        compact_play_result(&mut result, false);
        assert_eq!(
            result,
            json!({"ok": true, "serverReady": true, "clients": 2, "until": "true", "untilSeconds": 1.5})
        );
        let mut named = json!({"ok": true, "clients": [{"playerName": "A"}, {"playerName": "B"}]});
        compact_play_result(&mut named, true);
        assert_eq!(named["clients"], json!(["A", "B"]));
        let mut failed = json!({"ok": false, "error": "no", "clients": []});
        compact_play_result(&mut failed, false);
        assert_eq!(failed["clients"], json!([]));
    }

    #[test]
    fn play_start_lifts_the_awaited_result_beside_the_launch_fields() {
        let mut result =
            json!({"ok": true, "action": "start", "editRuntimeId": "e", "clients": []});
        let snapshot = json!({"hp": 100, "state": "InRound", "zones": ["A", "B"]});
        let wait = json!({
            "ok": true, "action": "wait", "condition": "ready", "value": "true",
            "result": snapshot, "elapsedSeconds": 2.5
        });
        assert!(attach_awaited_result(&mut result, wait));
        assert_eq!(result["result"], snapshot);
        assert_eq!(
            result["until"],
            json!({"ok": true, "action": "wait", "condition": "ready", "value": "true", "elapsedSeconds": 2.5})
        );
        compact_play_result(&mut result, false);
        assert_eq!(
            result,
            json!({"ok": true, "clients": 0, "until": "true", "untilSeconds": 2.5, "result": snapshot})
        );

        let mut plain = json!({"ok": true, "action": "start"});
        assert!(attach_awaited_result(
            &mut plain,
            json!({"ok": true, "action": "wait", "value": "true"})
        ));
        assert!(plain.get("result").is_none());

        let mut failed = json!({"ok": true, "action": "start"});
        let error = json!({
            "ok": false, "action": "wait", "conditionMet": true, "value": "true",
            "error": "The condition was met, but the result expression failed: attempt to index nil"
        });
        assert!(!attach_awaited_result(&mut failed, error.clone()));
        assert_eq!(failed["until"], error);
        assert!(failed.get("result").is_none());

        let mut timed_out = json!({"ok": true, "action": "start"});
        assert!(!attach_awaited_result(
            &mut timed_out,
            json!({"ok": false, "timedOut": true, "value": "false"})
        ));
        assert_eq!(timed_out["until"]["timedOut"], true);
    }

    #[test]
    fn capture_and_wait_results_drop_their_echoes() {
        let output = json!("C:/shots/a.png");
        let mut shot = json!({"ok": true, "path": output, "width": 10, "height": 5, "window": "Studio",
            "deviceSimulation": {"ok": true}, "camera": {"moved": false, "note": "did not hold", "actual": [1, 2, 3]}});
        compact_input_result(op::SHOT, &mut shot, Some(&output));
        assert_eq!(
            shot,
            json!({"ok": true, "width": 10, "height": 5, "moved": false, "note": "did not hold"})
        );
        let mut elsewhere = json!({"ok": true, "path": "C:/other.png"});
        compact_input_result(op::SHOT, &mut elsewhere, Some(&output));
        assert_eq!(elsewhere["path"], json!("C:/other.png"));
        let mut wait = json!({"ok": true, "action": "wait", "condition": "x", "value": "true", "elapsedSeconds": 0.2518773});
        compact_input_result(op::WAIT, &mut wait, None);
        assert_eq!(
            wait,
            json!({"ok": true, "value": "true", "elapsedSeconds": 0.3})
        );
    }

    #[test]
    fn a_wait_drops_its_echo_and_rounds_its_seconds_when_it_fails_too() {
        let mut timed_out =
            wait_timed_out_result("workspace.Ready", 42.0, &json!("false"), 42.2518773);
        let error = timed_out["error"].clone();
        compact_input_result(op::WAIT, &mut timed_out, None);
        assert_eq!(
            timed_out,
            json!({"ok": false, "timedOut": true, "value": "false", "elapsedSeconds": 42.3, "error": error})
        );
        let mut fixed = json!({"ok": true, "action": "wait", "waitedSeconds": 30.0004});
        compact_input_result(op::WAIT, &mut fixed, None);
        assert_eq!(
            serde_json::to_string(&fixed).unwrap(),
            r#"{"ok":true,"waitedSeconds":30.0}"#
        );
    }

    #[test]
    fn an_input_result_keeps_what_the_client_observed() {
        let mut input = json!({
            "ok": true, "action": "input", "actions": 7, "inputMethod": "virtual",
            "keysObserved": {"Return": true}, "verifiedClicks": 2, "result": {"open": true}, "shot": "a.png"
        });
        let mut elsewhere = input.clone();
        compact_input_result(op::INPUT, &mut input, Some(&json!("a.png")));
        assert_eq!(
            input,
            json!({"actions": 7, "keysObserved": {"Return": true}, "verifiedClicks": 2, "result": {"open": true}, "shot": true})
        );
        compact_input_result(op::INPUT, &mut elsewhere, Some(&json!("b.png")));
        assert_eq!(elsewhere["shot"], "a.png");
        let mut mixed = json!({"ok": true, "action": "input", "actions": 2, "inputMethod": "mixed", "window": "Studio", "verifiedClicks": null});
        compact_input_result(op::INPUT, &mut mixed, None);
        assert_eq!(
            mixed,
            json!({"actions": 2, "inputMethod": "mixed", "window": "Studio"})
        );
        let failed = json!({"ok": false, "action": "input", "error": "no client"});
        let mut kept = failed.clone();
        compact_input_result(op::INPUT, &mut kept, None);
        assert_eq!(kept, failed);
    }

    #[test]
    fn a_plain_luau_run_prints_its_values_raw() {
        assert_eq!(
            luau_text(&json!({"ok": true, "context": "client", "results": ["Player1"]})),
            "Player1"
        );
        assert_eq!(
            luau_text(&json!({
                "ok": true, "context": "edit", "persistence": "unsaved",
                "results": ["say \"hi\"\nthen leave", 3, true, {"_type": "Nil"}, null,
                    {"abs": 228.31427001953125, "list": [1, "a"]}, []]
            })),
            "say \"hi\"\nthen leave\n3\ntrue\nnil\nnil\n{\"abs\":228.314,\"list\":[1,\"a\"]}\n[]"
        );
        assert_eq!(luau_text(&json!({"ok": true, "context": "server"})), "ok");
        assert_eq!(
            luau_text(
                &json!({"ok": true, "results": ["nil", null, "12", 12, "", "ok", "[print] done", "[Shop] ready", "plain text"]})
            ),
            "\"nil\"\nnil\n\"12\"\n12\n\"\"\n\"ok\"\n\"[print] done\"\n[Shop] ready\nplain text"
        );
    }

    #[test]
    fn a_plain_luau_run_prints_captured_lines_first_and_notes_last() {
        assert_eq!(
            luau_text(&json!({
                "ok": true,
                "output": [
                    {"type": "print", "message": "step 1\nstep 2"},
                    {"type": "warn", "message": "[Renium stopped capturing command output at its size limit.]"}
                ],
                "results": [{"a": 1, "_truncated": {"kept": 128, "total": 300}}],
                "timeoutSeconds": 35.0,
                "syncReview": "Live Sync is waiting"
            })),
            "[print] step 1\n  step 2\n[warn] [Renium stopped capturing command output at its size limit.]\n{\"_truncated\":{\"kept\":128,\"total\":300},\"a\":1}\nnote: {\"syncReview\":\"Live Sync is waiting\"}"
        );
        assert_eq!(
            luau_text(&json!({"ok": true, "output": [{"type": "print", "message": "done"}]})),
            "[print] done\nok"
        );
    }

    #[test]
    fn a_long_luau_result_is_cut_at_the_limit_or_the_line_before_it() {
        assert_eq!(cut_luau_text(&"a".repeat(LUAU_TEXT_LIMIT)), None);
        let unbroken = "\u{e9}".repeat(LUAU_TEXT_LIMIT + 7);
        let (shown, total) = cut_luau_text(&unbroken).unwrap();
        assert_eq!(shown.chars().count(), LUAU_TEXT_LIMIT);
        assert_eq!(total, LUAU_TEXT_LIMIT + 7);
        let lines = "0123456789\n".repeat(LUAU_TEXT_LIMIT / 10);
        let (shown, total) = cut_luau_text(&lines).unwrap();
        assert!(shown.ends_with("\n0123456789"));
        assert!(shown.len() >= LUAU_TEXT_LIMIT - 11);
        assert_eq!(total, lines.len());
        // A line end far before the limit is not worth the text after it.
        let late = format!("first\n{}", "b".repeat(LUAU_TEXT_LIMIT * 2));
        assert_eq!(
            cut_luau_text(&late).unwrap().0.chars().count(),
            LUAU_TEXT_LIMIT
        );
    }

    #[test]
    fn a_lone_value_spreads_over_lines_and_several_values_stay_one_line_each() {
        let list = json!([{"x": 1.23456}, {"x": 2}]);
        assert_eq!(
            luau_text(&json!({"ok": true, "results": [list]})),
            "x\n1.235\n2"
        );
        assert_eq!(
            luau_text(&json!({"ok": true, "results": [list, 5, ["a b", "c d"]]})),
            "[{\"x\":1.235},{\"x\":2}]\n5\n[\"a b\",\"c d\"]"
        );
        // Text the code built keeps its digits; returned numbers are rounded.
        assert_eq!(
            luau_text(&json!({
                "ok": true,
                "output": [{"type": "print", "message": "dt 0.016666668"}],
                "results": ["at 846.5567626953125", 846.5567626953125],
            })),
            "[print] dt 0.016666668\nat 846.5567626953125\n846.557"
        );
    }

    #[test]
    fn a_cut_result_keeps_its_note_and_hint_and_names_the_saved_file() {
        let result = json!({
            "ok": true,
            "results": [(1..=2000).map(|n| format!("line {n} of a listing")).collect::<Vec<_>>()],
            "hint": "store it", "syncReview": "waiting",
            "nativeProfile": {"version": 1, "elapsedMs": 3.0}
        });
        let body = luau_body(&result);
        let trailer = luau_trailer(&result);
        assert_eq!(
            trailer,
            ["note: {\"syncReview\":\"waiting\"}", "hint: store it"]
        );
        let mut saved = None;
        let shown = shown_luau_text(&body, &trailer, |whole| {
            saved = Some(whole.to_string());
            Some(".renium/cache/luau-output-1.txt".to_string())
        });
        assert_eq!(saved.as_deref(), Some(body.as_str()));
        let lines = shown.lines().collect::<Vec<_>>();
        let (cut, closing) = lines.split_at(lines.len() - 3);
        assert!(cut.len() < 2000 && cut.last().unwrap().starts_with("line "));
        let shown_characters = cut.join("\n").chars().count();
        assert!(shown_characters <= LUAU_TEXT_LIMIT && shown_characters > LUAU_TEXT_LIMIT - 40);
        assert_eq!(
            closing,
            [
                format!(
                    "... (cut: {shown_characters} of {} characters shown; all of it is in .renium/cache/luau-output-1.txt)",
                    body.chars().count()
                )
                .as_str(),
                "note: {\"syncReview\":\"waiting\"}",
                "hint: store it",
            ]
        );
        let unsaved = shown_luau_text(&body, &[], |_| None);
        assert!(
            unsaved.ends_with("characters shown; return less, or write all of it with --out FILE)")
        );
        assert_eq!(
            shown_luau_text("short", &trailer, |_| panic!("nothing to save")),
            "short\nnote: {\"syncReview\":\"waiting\"}\nhint: store it"
        );
    }

    #[test]
    fn a_collected_runner_prints_its_state_in_lines() {
        assert_eq!(
            collected_text(&json!({
                "ok": true, "context": "server", "name": "probe", "done": true, "elapsedSeconds": 1.062,
                "output": [{"type": "print", "message": "armed"}],
                "state": {"phase": "done", "log": ["0.4 track", "0.8 drift"], "at": {"_type": "Vector3", "x": 1.23456, "y": 2, "z": 3}},
                "results": [true]
            })),
            "{\"context\":\"server\",\"done\":true,\"elapsedSeconds\":1.062,\"name\":\"probe\",\"ok\":true}\n[print] armed\nstate:\n{\"at\":\"Vector3(1.235, 2, 3)\",\"phase\":\"done\"}\nlog:\n0.4 track\n0.8 drift\nresults:\ntrue"
        );
    }

    /// What the chunk returns when it runs with these `-a` values.
    fn run_with_arguments(code: &str, values: &[&str]) -> Value {
        use mlua::LuaSerdeExt;
        let values = values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        let wrapped = luau_with_arguments(code, &values).unwrap();
        let lua = mlua::Lua::new();
        let returned: mlua::MultiValue = lua.load(wrapped).eval().unwrap();
        Value::Array(
            returned
                .into_iter()
                .map(|value| lua.from_value(value).unwrap())
                .collect(),
        )
    }

    #[test]
    fn arguments_reach_a_chunk_as_its_varargs() {
        assert_eq!(
            run_with_arguments(
                "local name, n, text, none, table, odd = ...\nreturn name, n * 2, text, none == nil, table.at[2], table.on, odd, select(\"#\", ...) -- done",
                &[
                    "Door",
                    "5",
                    "\"5\"",
                    "null",
                    "{\"at\":[1,2.5],\"on\":true}",
                    "say \"hi\"\\\n\t\u{7}"
                ]
            ),
            json!(["Door", 10, "5", true, 2.5, true, "say \"hi\"\\\n\t\u{7}", 6])
        );
        // A chunk that returns nothing, one that ends in a comment, and a
        // value that is not JSON and is taken as text.
        assert_eq!(run_with_arguments("local a = ...", &["1"]), json!([]));
        assert_eq!(
            run_with_arguments("return ... -- all", &["-7", "a b"]),
            json!([-7, "a b"])
        );
        assert_eq!(
            run_with_arguments("return ...", &["1e400", "nil", "Car 2"]),
            json!(["1e400", "nil", "Car 2"])
        );
        // An error keeps the line of the chunk as it was written.
        let wrapped =
            luau_with_arguments("local a = ...\nerror(\"boom \" .. a)", &["x".to_string()])
                .unwrap();
        let error = mlua::Lua::new()
            .load(wrapped)
            .set_name("Renium")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(error.contains(":2: boom x"), "{error}");
        // Text that opens like a table or a string must be one.
        for broken in ["{y:2.5}", "{\"y\":2.5", "[1,", "\"open"] {
            let error = luau_with_arguments("return ...", &[broken.to_string()])
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with(&format!("-a {broken}: not valid JSON")),
                "{error}"
            );
        }
    }

    #[test]
    fn a_chunk_resent_with_other_values_is_told_to_keep_the_code() {
        let chunk = |name: &str, n: u32| {
            format!(
                "local car = workspace.Vehicles:FindFirstChild(\"{name}\") -- probe {n}\nlocal seat = car and car:FindFirstChild(\"DriverSeat\")\nif not seat then return \"none\" end\nlocal velocity = seat.AssemblyLinearVelocity\nreturn {{speed = velocity.Magnitude, at = seat.Position, limit = {n}.5, throttle = seat.ThrottleFloat}}"
            )
        };
        assert_eq!(
            luau_skeleton(&chunk("A", 1)),
            luau_skeleton(&chunk("Other Car", 22))
        );
        assert_ne!(
            luau_skeleton(&chunk("A", 1)),
            luau_skeleton(&chunk("A", 1).replace("Position", "CFrame"))
        );
        let mut advice = TokenAdvice::default();
        let runs = (1..=12)
            .map(|n| advice.resent(&chunk("Car", n)))
            .collect::<Vec<_>>();
        assert_eq!(
            runs,
            [
                None,
                None,
                Some(3),
                None,
                None,
                Some(6),
                None,
                None,
                None,
                None,
                None,
                Some(12)
            ]
        );
        assert_eq!(
            advice.resent(&chunk("Car", 13).replace("Position", "Orientation")),
            None
        );
        // A short chunk is not worth a file.
        assert!((0..6).all(|n| {
            advice
                .resent(&format!("return workspace.Part{n}.Position"))
                .is_none()
        }));
        assert_eq!(
            resent_chunk_hint(3, false),
            "this chunk has now been sent 3 times with only its values changed; put it in a file once and send the values: `rbx l -f FILE -a VALUE`"
        );
        assert!(resent_chunk_hint(6, true).ends_with("`rbx lc -f FILE PLAYER -a VALUE`"));
    }

    #[test]
    fn runs_that_build_text_from_unrounded_numbers_are_told_now_and_then() {
        let digits = json!({"ok": true, "results": ["at 846.5567626953125, 119.03035736083984 size 2.970987319946289, 3.362443208694458"]});
        let printed = json!({"ok": true, "output": [
            {"type": "print", "message": "846.5567626953125 119.03035736083984"},
            {"type": "print", "message": "2.970987319946289 3.362443208694458"}
        ]});
        let mut advice = TokenAdvice::default();
        let told = [
            &digits, &printed, &digits, &digits, &digits, &digits, &digits, &printed,
        ]
        .map(|result| advice.long_floats(result));
        assert_eq!(told, [false, true, false, true, false, false, false, true]);
        // Returned numbers, a few digits and JSON in a string are not counted.
        for quiet in [
            json!({"ok": true, "results": [[846.5567626953125, 119.03035736083984, 2.970987319946289, 3.362443208694458]]}),
            json!({"ok": true, "results": ["at 846.5567626953125, 119.03035736083984"]}),
            json!({"ok": true, "results": ["[846.5567626953125,119.03035736083984,2.970987319946289,3.362443208694458]"]}),
        ] {
            assert!(!advice.long_floats(&quiet));
        }
        assert_eq!(advice.long_float_runs, 8);
    }

    #[test]
    fn a_play_or_wait_result_prints_its_values_in_the_short_form() {
        let value =
            json!({"at": {"_type": "Vector3", "x": 1.23456789, "y": 2, "z": 3}, "t": 0.123456789});
        let short = json!({"at": "Vector3(1.235, 2, 3)", "t": 0.123});
        let mut play = json!({"ok": true, "clients": [], "result": value, "clientResult": value});
        compact_play_result(&mut play, false);
        assert_eq!(play["result"], short);
        assert_eq!(play["clientResult"], short);
        let mut wait = json!({"ok": true, "action": "wait", "condition": "x", "value": "true", "result": value});
        compact_wait_record(wait.as_object_mut().unwrap());
        assert_eq!(wait, json!({"ok": true, "value": "true", "result": short}));
    }

    #[test]
    fn a_play_start_whose_wait_failed_keeps_the_wait_without_its_echo() {
        let mut result = json!({
            "ok": true, "action": "start", "mode": "play", "serverReady": true, "editRuntimeId": "e",
            "clients": [{"playerName": "A", "role": "client"}]
        });
        let wait =
            wait_timed_out_result("shared.Ready == true", 120.0, &json!("false"), 120.2518773);
        let error = wait["error"].clone();
        assert!(!attach_awaited_result(&mut result, wait));
        compact_play_result(&mut result, false);
        assert_eq!(
            result,
            json!({
                "ok": true, "serverReady": true, "clients": 1,
                "until": {"ok": false, "timedOut": true, "value": "false", "elapsedSeconds": 120.3, "error": error}
            })
        );
    }
}

#[cfg(test)]
mod play_check_tests {
    use super::*;

    fn play_args(parts: &[&str]) -> std::result::Result<StartStopPlayArgs, clap::Error> {
        use clap::Parser;
        match crate::cli::Cli::try_parse_from(parts)?.command {
            crate::cli::Commands::StartStopPlay(args) => Ok(args),
            _ => panic!("expected play"),
        }
    }

    fn luau_args(parts: &[&str]) -> ExecuteLuauArgs {
        use clap::Parser;
        match crate::cli::Cli::try_parse_from(parts).unwrap().command {
            crate::cli::Commands::ExecuteLuau(args) => args,
            crate::cli::Commands::ExecuteClientLuau(args) => args.into_luau_args().unwrap(),
            _ => panic!("expected l or lc"),
        }
    }

    #[test]
    fn l_and_lc_take_arguments_and_an_output_file() {
        let args = luau_args(&[
            "rbx",
            "l",
            "-f",
            "probe.luau",
            "-a",
            "Door",
            "-a",
            "-5",
            "-o",
            "out.txt",
        ]);
        assert_eq!(args.arg, ["Door", "-5"]);
        assert_eq!(args.out.as_deref(), Some(Path::new("out.txt")));
        let client = luau_args(&["rbx", "lc", "return ...", "2", "-a", "x"]);
        assert_eq!(client.arg, ["x"]);
        assert_eq!(client.player.as_deref(), Some("2"));
    }

    #[test]
    fn lc_runs_a_file_on_the_named_player_or_the_first_client() {
        let named = luau_args(&["rbx", "lc", "-f", "probe.luau", "2"]);
        assert_eq!(named.file.as_deref(), Some(Path::new("probe.luau")));
        assert_eq!(named.player.as_deref(), Some("2"));
        assert!(named.code.is_none() && !named.client);
        let first = luau_args(&["rbx", "lc", "--file", "probe.luau"]);
        assert!(first.client && first.player.is_none());
        for parts in [
            &["rbx", "lc", "-f", "probe.luau", "return 1", "1"][..],
            &["rbx", "lc", "return 1", "-f", "probe.luau"][..],
            &["rbx", "lc", "-f", "probe.luau", "-"][..],
        ] {
            use clap::Parser;
            let crate::cli::Commands::ExecuteClientLuau(args) =
                crate::cli::Cli::try_parse_from(parts).unwrap().command
            else {
                panic!("expected lc");
            };
            let error = args.into_luau_args().err().unwrap().to_string();
            assert!(error.starts_with("-f takes the file"), "{error}");
        }
    }

    fn runner(keep_alive: bool, detach: Option<&str>) -> crate::cli::LuauRunnerArgs {
        crate::cli::LuauRunnerArgs {
            detach: detach.map(str::to_string),
            lifetime: 600.0,
            keep_alive,
            ..Default::default()
        }
    }

    #[test]
    fn a_users_play_chunk_asks_the_plugin_to_keep_its_runner() {
        for target in [
            BridgeTarget::Main,
            BridgeTarget::Server,
            BridgeTarget::Client,
        ] {
            let request = user_luau_request("return 1", target, 10.0, &runner(true, None)).unwrap();
            assert_eq!(request["keepAliveSeconds"], 120.0, "{target:?}");
            assert!(request.get("backgroundLifetimeSeconds").is_none());
            assert!(request.get("detach").is_none());
        }
        let edit =
            user_luau_request("return 1", BridgeTarget::Edit, 10.0, &runner(true, None)).unwrap();
        assert!(edit.get("keepAliveSeconds").is_none());
        let internal =
            user_luau_request("return 1", BridgeTarget::Server, 10.0, &runner(false, None))
                .unwrap();
        assert_eq!(
            internal,
            luau_request("return 1", "Renium", BridgeTarget::Server, 10.0).unwrap()
        );
        let detached = user_luau_request(
            "return 1",
            BridgeTarget::Server,
            10.0,
            &runner(true, Some("sampler")),
        )
        .unwrap();
        assert_eq!(detached["detach"], "sampler");
        assert_eq!(detached["lifetimeSeconds"], 600.0);
        assert!(detached.get("keepAliveSeconds").is_none());
        assert!(
            user_luau_request(
                "return 1",
                BridgeTarget::Server,
                10.0,
                &runner(true, Some("no good"))
            )
            .is_err()
        );
    }

    #[test]
    fn only_the_l_and_lc_commands_mark_a_run_as_the_users() {
        for parts in [
            &["rbx", "l", "--server", "return 1"][..],
            &["rbx", "l", "return 1"],
            &["rbx", "lc", "return 1", "2"],
        ] {
            let args = luau_args(parts);
            assert!(
                !args.runner.keep_alive,
                "the flag is no command-line option"
            );
            assert_eq!(
                luau_command_parameters(&args, 10.0)["keepAlive"],
                true,
                "{parts:?}"
            );
        }
        let play = play_args(&["rbx", "play", "-s", "--until-console", "x"]).unwrap();
        for side in ["server", "client"] {
            let chunk = result_chunk_parameters("return 1", side, 42.0, &play);
            assert!(chunk.get("keepAlive").is_none(), "{chunk}");
            assert_eq!(chunk["timeout"], 42.0);
            assert_eq!(chunk[side], true);
        }
        assert_eq!(
            result_chunk_parameters("return 1", "client", 42.0, &play)["player"],
            1
        );
    }

    #[test]
    fn a_kept_run_prints_what_a_plain_run_prints() {
        let mut kept = json!({
            "ok": true, "context": "server", "results": [3], "output": [],
            "background": true, "executionId": "runtime:7", "runner": true
        });
        strip_kept_runner_fields(&mut kept);
        assert_eq!(
            kept,
            json!({"ok": true, "context": "server", "results": [3], "output": [], "runner": true})
        );
        kept.as_object_mut().unwrap().remove("runner");
        assert_eq!(luau_text(&kept), "3");
        let mut edit_window = json!({"ok": true, "results": [], "background": false});
        strip_kept_runner_fields(&mut edit_window);
        assert_eq!(luau_text(&edit_window), "ok");
        let mut older_plugin =
            json!({"ok": true, "results": [], "background": false, "runner": true});
        strip_kept_runner_fields(&mut older_plugin);
        assert_eq!(older_plugin["listeners"], LISTENERS_ENDED);
    }

    #[test]
    fn result_chunks_get_what_the_waits_left_of_the_play_timeout() {
        assert_eq!(result_run_seconds(120.0, 0.0), 120.0);
        assert_eq!(result_run_seconds(120.0, 7.5), 112.5);
        assert_eq!(result_run_seconds(60.0, 20.0), 40.0);
        assert_eq!(result_run_seconds(60.0, 58.0), 10.0);
        assert_eq!(result_run_seconds(5.0, 0.0), 10.0);
        assert_eq!(result_run_seconds(600.0, 1.0), 120.0);

        assert_eq!(wait_probe_run_limit(5.0, 120.0, false), 10.0);
        assert_eq!(wait_probe_run_limit(5.0, 120.0, true), 120.0);
        assert_eq!(wait_probe_run_limit(5.0, 47.0, true), 47.0);
        assert_eq!(wait_probe_run_limit(5.0, 8.0, true), 10.0);
        assert_eq!(wait_probe_run_limit(1.5, 1.5, true), 6.5);
        assert_eq!(wait_probe_run_limit(5.0, 3000.0, true), 120.0);
    }

    #[test]
    fn a_failed_result_chunk_of_a_wait_names_the_running_session() {
        let mut failed = json!({
            "ok": false, "conditionMet": true, "value": "true",
            "error": "The condition was met, but the result expression failed: attempt to index nil with 'Floor'"
        });
        name_failed_result_chunk(&mut failed);
        assert_eq!(
            failed["error"],
            "The condition was met, but the result expression failed: attempt to index nil with 'Floor'\nHint: the session is still running; `rbx l --server 'LUAU'` reruns the chunk"
        );
        let mut timed_out = wait_timed_out_result("shared.Ready", 30.0, &json!("false"), 30.1);
        let unchanged = timed_out.clone();
        name_failed_result_chunk(&mut timed_out);
        assert_eq!(timed_out, unchanged);

        let runner = || {
            anyhow::Error::from(crate::automation::DaemonFailure {
                message: "Luau runner timed out after 118.0s and was stopped\nHint: pass -t SECONDS, or use --detach NAME for samplers and read them with --collect NAME".to_string(),
                detail: Some(json!({"consoleErrors": ["late"]})),
            })
        };
        let named = result_chunk_timeout(runner(), true);
        assert_eq!(
            named.to_string(),
            "Luau runner timed out after 118.0s and was stopped\nHint: raise this command's -t SECONDS, or rerun the chunk with `rbx l --server 'LUAU'` -t SECONDS; the session is still running"
        );
        let detail = named
            .downcast_ref::<crate::automation::DaemonFailure>()
            .and_then(|failure| failure.detail.clone());
        assert_eq!(detail, Some(json!({"consoleErrors": ["late"]})));
        assert!(
            result_chunk_timeout(runner(), false)
                .to_string()
                .contains("--detach NAME")
        );
        let other = result_chunk_timeout(anyhow::anyhow!("The bound Studio changed"), true);
        assert_eq!(other.to_string(), "The bound Studio changed");
    }

    #[test]
    fn client_until_is_a_wait_of_its_own_on_the_first_client() {
        let args = play_args(&[
            "rbx",
            "play",
            "-s",
            "--client-until",
            "game.Players.LocalPlayer.PlayerGui.Main.Shop.Visible",
            "--client-result",
            "#game.Players.LocalPlayer.PlayerGui:GetChildren()",
            "--result",
            "workspace.DistributedGameTime",
            "-t",
            "30",
        ])
        .unwrap();
        assert_eq!(
            args.client_until.as_deref(),
            Some("game.Players.LocalPlayer.PlayerGui.Main.Shop.Visible")
        );
        assert_eq!(args.until_timeout, 30.0);
        assert!(args.until.is_none());
        validate_play_waits(&args).unwrap();

        let both = play_args(&[
            "rbx",
            "play",
            "-r",
            "--until",
            "shared.Ready",
            "--client-until",
            "-1 < 0",
        ])
        .unwrap();
        assert_eq!(both.until.as_deref(), Some("shared.Ready"));
        assert_eq!(both.client_until.as_deref(), Some("-1 < 0"));
        validate_play_waits(&both).unwrap();

        let unparsable =
            play_args(&["rbx", "play", "-s", "--client-until", "workspace.("]).unwrap();
        let error = format!("{:#}", validate_play_waits(&unparsable).unwrap_err());
        assert!(
            error.starts_with("The condition is neither a Luau expression nor a chunk"),
            "{error}"
        );
        for mode in ["run", "server"] {
            let headless = play_args(&[
                "rbx",
                "play",
                "-s",
                "--mode",
                mode,
                "--client-until",
                "true",
            ])
            .unwrap();
            let error = validate_play_waits(&headless).unwrap_err().to_string();
            assert!(error.contains("have none"), "{error}");
        }
        let lone = play_args(&["rbx", "play", "-s", "--client-result", "1"]).unwrap();
        let error = validate_play_waits(&lone).unwrap_err().to_string();
        assert_eq!(
            error,
            "--result and --client-result need --until, --client-until or --until-console"
        );
        for parts in [
            &["rbx", "play", "-x", "--client-until", "true"][..],
            &[
                "rbx",
                "play",
                "--add-players",
                "1",
                "--client-until",
                "true",
            ],
            &[
                "rbx",
                "play",
                "--leave",
                "-p",
                "1",
                "--client-until",
                "true",
            ],
            &["rbx", "play", "--kill-orphans", "--client-until", "true"],
        ] {
            assert!(play_args(parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn a_play_start_reports_the_client_wait_beside_the_server_wait() {
        let mut result = json!({
            "ok": true, "action": "start", "mode": "play", "serverReady": true,
            "clients": [{"playerName": "A", "role": "client"}],
            "until": {"action": "wait", "condition": "a", "ok": true, "value": "true", "elapsedSeconds": 2.04},
            "clientUntil": {"action": "wait", "condition": "b", "ok": true, "value": "true", "elapsedSeconds": 0.26}
        });
        compact_play_result(&mut result, false);
        assert_eq!(
            result,
            json!({
                "ok": true, "serverReady": true, "clients": 1,
                "until": "true", "untilSeconds": 2.0,
                "clientUntil": "true", "clientUntilSeconds": 0.3
            })
        );

        let mut failed = json!({"ok": true, "action": "start", "clients": []});
        let wait = wait_timed_out_result("panel.Visible", 30.0, &json!("false"), 30.26);
        let error = wait["error"].clone();
        failed["clientUntil"] = wait;
        compact_play_result(&mut failed, false);
        assert_eq!(
            failed,
            json!({
                "ok": true, "clients": 0,
                "clientUntil": {"ok": false, "timedOut": true, "value": "false", "elapsedSeconds": 30.3, "error": error}
            })
        );
    }

    #[test]
    fn a_start_into_a_running_session_says_nothing_was_started() {
        let status = json!({
            "ok": true, "running": true, "starting": false, "readyForStart": false,
            "engineRunning": true, "engineStarted": true, "executing": true, "mode": "play",
            "launchNonce": "play-1", "runState": "Enum.RunState.Stopped",
            "studioTest": {"editModeActive": false},
            "clients": [{"role": "server"}, {"role": "client", "playerName": "A"}]
        });
        let mut result = already_running_result(status.clone());
        assert_eq!(result["alreadyRunning"], true);
        assert_eq!(result["startNote"], ALREADY_RUNNING_NOTE);
        assert!(ALREADY_RUNNING_NOTE.contains("rbx play -r"));
        assert_eq!(result["running"], true, "json mode keeps the status");
        assert_eq!(result["launchNonce"], "play-1");

        result["serverReady"] = json!(true);
        result["until"] = json!({"ok": true, "value": "true", "elapsedSeconds": 0.0});
        result["result"] = json!("0/10");
        compact_play_result(&mut result, false);
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            format!(
                r#"{{"alreadyRunning":true,"clients":1,"ok":true,"result":"0/10","running":true,"serverReady":true,"startNote":"{ALREADY_RUNNING_NOTE}","until":"true","untilSeconds":0.0}}"#
            )
        );

        let mut fresh = json!({"ok": true, "action": "start", "running": true, "clients": []});
        compact_play_result(&mut fresh, false);
        assert_eq!(fresh["running"], true);
    }
}

#[cfg(test)]
mod wait_condition_tests {
    use super::*;

    fn wait_args(condition: &str, result: Option<&str>) -> WaitUntilArgs {
        WaitUntilArgs {
            slice: false,
            bridge: BridgeConnectionArgs::local(3.0),
            condition: condition.to_string(),
            player: None,
            client: false,
            server: false,
            timeout: 60.0,
            interval: 0.25,
            result: result.map(str::to_string),
        }
    }

    fn outcome(results: Vec<Value>) -> LuauTaskOutcome {
        LuauTaskOutcome {
            success: results.first() == Some(&json!(true)),
            detail: results.get(1).cloned().unwrap_or(Value::Null),
            results,
            elapsed: 1.0,
        }
    }

    #[test]
    fn result_expressions_take_the_condition_forms() {
        assert_eq!(
            wait_result_body("shared.RoundHandler:Snapshot()").unwrap(),
            "return (shared.RoundHandler:Snapshot()\n)"
        );
        assert_eq!(
            wait_result_body("{hp = car.HP, state = shared.RoundHandler.GameState}").unwrap(),
            "return ({hp = car.HP, state = shared.RoundHandler.GameState}\n)"
        );
        let chunk = "local car = workspace.Car; return {hp = car.HP}";
        assert_eq!(wait_result_body(chunk).unwrap(), chunk);
        let error = format!("{:#}", wait_result_body("{hp =").unwrap_err());
        assert!(
            error.starts_with("The result expression is neither a Luau expression nor a chunk"),
            "{error}"
        );
    }

    #[test]
    fn probe_code_returns_the_observation_only_after_the_condition_holds() {
        let plain = wait_probe_code("return (ready\n)", None, 100.0, 0.25);
        validate_luau_syntax(&plain).unwrap();
        assert!(plain.contains("return true, (type(value) == 'table' and game:GetService('HttpService'):JSONEncode(value) or tostring(value))"));
        assert!(!plain.contains("resultOk"));

        let typed = wait_probe_code(
            "return (ready\n)",
            Some("return (shared.RoundHandler:Snapshot()\n)"),
            100.0,
            0.25,
        );
        validate_luau_syntax(&typed).unwrap();
        let condition = typed.find("return (ready").unwrap();
        let observation = typed
            .find("return (shared.RoundHandler:Snapshot()")
            .unwrap();
        assert!(condition < observation);
        assert!(typed.contains("local resultOk, result = pcall(function()"));
        assert!(typed.contains("return true, (type(value) == 'table' and game:GetService('HttpService'):JSONEncode(value) or tostring(value)), resultOk, result"));
        assert!(
            typed.contains("if os.clock() >= deadline then return false, tostring(detail) end")
        );
        assert!(typed.contains("task.wait(0.25)"));
    }

    #[test]
    fn met_conditions_carry_the_typed_result_or_its_failure() {
        let args = wait_args("ready", None);
        let plain = met_condition_result(&args, &outcome(vec![json!(true), json!("true")]), 1.5);
        assert_eq!(
            plain,
            json!({"ok": true, "action": "wait", "condition": "ready", "value": "true", "elapsedSeconds": 1.5})
        );

        let args = wait_args("ready", Some("shared.RoundHandler:Snapshot()"));
        let snapshot = json!({"hp": 100, "state": "InRound", "zones": ["A", "B"]});
        let typed = met_condition_result(
            &args,
            &outcome(vec![
                json!(true),
                json!("true"),
                json!(true),
                snapshot.clone(),
            ]),
            1.5,
        );
        assert_eq!(typed["ok"], true);
        assert_eq!(typed["value"], "true");
        assert_eq!(typed["result"], snapshot);

        let failed = met_condition_result(
            &args,
            &outcome(vec![
                json!(true),
                json!("true"),
                json!(false),
                json!("attempt to index nil with 'Snapshot'"),
            ]),
            1.5,
        );
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["conditionMet"], true);
        assert_eq!(failed["value"], "true");
        assert!(failed.get("result").is_none());
        assert_eq!(
            failed["error"],
            "The condition was met, but the result expression failed: attempt to index nil with 'Snapshot'"
        );
    }

    #[test]
    fn single_expressions_are_returned_and_chunks_run_as_written() {
        let expression = wait_condition_body("workspace:FindFirstChild('P') ~= nil").unwrap();
        assert_eq!(
            expression,
            "return (workspace:FindFirstChild('P') ~= nil\n)"
        );
        assert_eq!(
            wait_condition_body("#game.Players:GetPlayers() == 2 -- both joined").unwrap(),
            "return (#game.Players:GetPlayers() == 2 -- both joined\n)"
        );
        let chunk = "local p = workspace:FindFirstChild('P'); return p ~= nil";
        assert_eq!(wait_condition_body(chunk).unwrap(), chunk);
        let looped = "for _, part in workspace:GetChildren() do\n\tif part.Name == 'P' then return true end\nend\nreturn false";
        assert_eq!(wait_condition_body(looped).unwrap(), looped);
        assert_eq!(wait_condition_body("return true").unwrap(), "return true");
        let error = format!("{:#}", wait_condition_body("workspace.P ==").unwrap_err());
        assert!(
            error.starts_with("The condition is neither a Luau expression nor a chunk"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod virtual_input_tests {
    use super::*;

    #[test]
    fn long_holds_go_to_the_client_as_consecutive_calls() {
        let mut actions = vec![json!({ "type": "key", "key": "W", "down": true })];
        actions.extend(virtual_wait_actions(45_000));
        actions.push(json!({ "type": "key", "key": "W", "down": false }));
        let batches = split_virtual_input(actions);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 3, "key down plus two ten-second waits");
        assert_eq!(batches[1].len(), 4, "the rest of the hold and the key up");
        let waited =
            |batch: &[Value]| -> u64 { batch.iter().filter_map(|a| a["ms"].as_u64()).sum() };
        assert_eq!(waited(&batches[0]), 20_000);
        assert_eq!(waited(&batches[1]), 25_000);

        let short = split_virtual_input(virtual_click_actions(1, 2, false, 30, true));
        assert_eq!(short.len(), 1);
        let clicks = split_virtual_input(vec![
            json!({ "type": "click", "path": "A", "holdMs": 10_000 }),
            json!({ "type": "click", "path": "B", "holdMs": 10_000 }),
            json!({ "type": "click", "path": "C", "holdMs": 10_000 }),
        ]);
        assert_eq!(clicks.len(), 3);
    }

    #[test]
    fn a_click_starts_its_own_call_unless_something_is_held() {
        let click = |path: &str, step: u64| json!({ "type": "click", "path": path, "step": step });
        let wait = json!({ "type": "wait", "ms": 300 });
        let text = json!({ "type": "text", "text": "abc" });
        let batches = split_virtual_input(vec![
            wait.clone(),
            click("Open", 2),
            wait.clone(),
            click("Box", 4),
            text.clone(),
            click("Redeem", 6),
        ]);
        let shape = |batches: &[Vec<Value>]| batches.iter().map(Vec::len).collect::<Vec<_>>();
        assert_eq!(shape(&batches), [1, 2, 2, 1]);
        assert!(batches[1..].iter().all(|batch| batch[0]["type"] == "click"));

        let key = |down: bool| json!({ "type": "key", "key": "W", "down": down });
        let held = split_virtual_input(vec![key(true), click("A", 2), key(false), click("B", 4)]);
        assert_eq!(
            shape(&held),
            [3, 1],
            "a click under a held key stays in its call"
        );

        assert!(click_target_may_appear(
            "GUI element Main.Shop.Redeem is not visible: Main.Shop has Visible=false"
        ));
        assert!(click_target_may_appear(
            "GUI element A.B is covered by A.Popup"
        ));
        assert!(click_target_may_appear(
            "Path segment 'Shop' not found under Players.A.PlayerGui.Main"
        ));
        assert!(!click_target_may_appear(
            "Expected one Activated and MouseButton1Click event, got 0 and 0"
        ));
    }

    #[test]
    fn consecutive_call_answers_add_up() {
        let first = json!({
            "ok": true, "actions": 3, "verifiedClicks": 1,
            "keysObserved": { "W": true }, "throughSystemUi": ["chat"], "heldKeys": ["W"],
        });
        let last = json!({
            "ok": true, "actions": 4, "verifiedClicks": 0,
            "keysObserved": { "E": false }, "coreGui": { "pressed": true },
        });
        let merged = merge_virtual_input_results(first, last);
        assert_eq!(merged["actions"], 7);
        let counted = merge_virtual_input_results(
            json!({ "ok": true, "actions": 1, "verifiedClicks": 0, "unconfirmedClicks": 1 }),
            json!({ "ok": true, "actions": 1, "verifiedClicks": 1 }),
        );
        assert_eq!(counted["unconfirmedClicks"], 1);
        assert_eq!(counted["verifiedClicks"], 1);
        let dismissed = merge_virtual_input_results(
            json!({ "ok": true, "actions": 1, "coreGui": { "pressed": true } }),
            json!({ "ok": true, "actions": 1, "verifiedClicks": 1 }),
        );
        assert_eq!(dismissed["coreGui"]["pressed"], true);
        assert_eq!(merged["verifiedClicks"], 1);
        assert_eq!(merged["keysObserved"], json!({ "W": true, "E": false }));
        assert_eq!(merged["throughSystemUi"], json!(["chat"]));
        assert!(
            merged.get("heldKeys").is_none(),
            "the last call reports what is still held"
        );
        assert_eq!(merged["coreGui"]["pressed"], true);
    }

    #[test]
    fn a_refused_click_lists_what_is_clickable_now() {
        let hidden = "GUI element Main.HUD.Config is not visible: Main.HUD has Visible=false";
        let listed = with_clickable_now(
            hidden,
            &json!({ "clickable": ["Main.DailyRewards.Header.Exit", "Main.DailyRewards.Claim"] }),
        );
        assert_eq!(
            listed,
            format!(
                "{hidden}. Clickable now: Main.DailyRewards.Header.Exit, Main.DailyRewards.Claim"
            )
        );
        for nothing in [
            json!({}),
            json!({ "clickable": [] }),
            json!({ "clickable": null }),
        ] {
            assert_eq!(with_clickable_now(hidden, &nothing), hidden);
        }

        let many: Vec<String> = (1..=20).map(|n| format!("Shop.Item{n}")).collect();
        let capped = with_clickable_now(hidden, &json!({ "clickable": many }));
        let list = capped.split_once("Clickable now: ").unwrap().1;
        assert_eq!(list.split(", ").count(), 12);
        assert!(list.ends_with("Shop.Item12"));

        let long: Vec<String> = (1..=12)
            .map(|n| format!("{}.B{n}", "Deep".repeat(30)))
            .collect();
        let budgeted = with_clickable_now(hidden, &json!({ "clickable": long }));
        let list = budgeted.split_once("Clickable now: ").unwrap().1;
        assert!(list.len() <= 600, "{}", list.len());
        assert_eq!(list.split(", ").count(), 4, "whole paths only");
    }

    #[test]
    fn a_failed_virtual_input_call_keeps_its_step_and_list() {
        assert!(virtual_input_failure(&json!({ "ok": true, "actions": 2 })).is_none());
        let failure = virtual_input_failure(&json!({
            "ok": false,
            "error": "GUI element Players.A.PlayerGui.Shop.Buy is covered by Players.A.PlayerGui.Popup.Frame",
            "failedStep": 3,
            "clickable": ["Popup.Close"],
        }))
        .unwrap();
        assert_eq!(failure.step, Some(3));
        assert_eq!(
            failure.to_string(),
            "GUI element Players.A.PlayerGui.Shop.Buy is covered by Players.A.PlayerGui.Popup.Frame. Clickable now: Popup.Close"
        );
        let plain = virtual_input_failure(
            &json!({ "ok": false, "error": "Virtual input requires a Play client" }),
        )
        .unwrap();
        assert_eq!(plain.step, None);
        assert_eq!(plain.to_string(), "Virtual input requires a Play client");
    }

    #[test]
    fn a_press_target_is_refused_with_its_reason() {
        let refusal = |bounds: Value| gui_click_refusal(&bounds, "Shop.Buy");
        assert_eq!(
            refusal(json!({ "onScreen": true, "visible": true, "hitTest": true })),
            None
        );
        assert_eq!(
            refusal(json!({
                "fullName": "Players.A.PlayerGui.Main.HUD.Config", "ordinalPath": "Main.HUD.Config",
                "onScreen": true, "visible": false, "notVisible": "Main.HUD has Visible=false",
            }))
            .unwrap(),
            "GUI element Main.HUD.Config is not visible: Main.HUD has Visible=false"
        );
        assert_eq!(
            refusal(json!({
                "fullName": "Players.A.PlayerGui.Shop.Buy", "onScreen": true, "visible": true,
                "hitTest": false, "blockedBy": "Players.A.PlayerGui.Popup.Frame",
            }))
            .unwrap(),
            "GUI element Players.A.PlayerGui.Shop.Buy is covered by Players.A.PlayerGui.Popup.Frame"
        );
        assert_eq!(
            refusal(json!({ "onScreen": true, "visible": true, "hitTest": false })).unwrap(),
            "GUI element Shop.Buy is not receiving pointer input at its center"
        );
        assert!(
            refusal(json!({ "onScreen": false }))
                .unwrap()
                .starts_with("Shop.Buy could not be brought on screen")
        );
    }
}
