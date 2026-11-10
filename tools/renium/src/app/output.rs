use std::cell::{Cell, RefCell};
use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::app::timing::current_millis;
use crate::cli::Cli;
use crate::system::LockRecover;

static LOG_FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
const LOG_FILE_LIMIT: u64 = 8 * 1024 * 1024;
const LOG_FILE_LEVEL: u8 = 3;

pub(crate) fn daemon_log_path() -> Option<std::path::PathBuf> {
    crate::daemon::local_app_data_daemon_path().and_then(|path| {
        path.parent()
            .map(|parent| parent.join("logs").join("daemon.log"))
    })
}

pub(crate) fn open_daemon_log() {
    let Some(path) = daemon_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_if_large(&path);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        *LOG_FILE.lock_recover() = Some(file);
        log_global(
            3,
            format_args!(
                "[renium] daemon {} ({}) started, pid {}",
                crate::app::build::VERSION,
                crate::app::build::GIT_HASH,
                std::process::id()
            ),
        );
    }
}

fn rotate_if_large(path: &std::path::Path) {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > LOG_FILE_LIMIT) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

fn append_log_file(message: fmt::Arguments<'_>) {
    let mut guard = LOG_FILE.lock_recover();
    let Some(file) = guard.as_mut() else {
        return;
    };
    let line = format!(
        "{} {}\n",
        crate::app::report::utc_stamp(std::time::SystemTime::now()),
        message
    );
    let _ = file.write_all(line.as_bytes());
    if file
        .metadata()
        .is_ok_and(|metadata| metadata.len() > LOG_FILE_LIMIT)
        && let Some(path) = daemon_log_path()
    {
        *guard = None;
        rotate_if_large(&path);
        *guard = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
    }
}

#[derive(Debug)]
pub(crate) struct ReportedFailure;

impl fmt::Display for ReportedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("command reported a structured failure")
    }
}

impl std::error::Error for ReportedFailure {}

#[derive(Clone, Copy)]
pub(crate) enum OutputMode {
    Compact,
    Summary,
    Detail,
    Full,
}

impl OutputMode {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "compact" | "comp" | "min" | "c" => Ok(Self::Compact),
            "summary" | "sum" | "s" => Ok(Self::Summary),
            "detail" | "details" | "d" => Ok(Self::Detail),
            "full" | "f" => Ok(Self::Full),
            other => bail!("Invalid output mode: {other}. Use compact, summary, detail, or full."),
        }
    }

    pub(crate) fn uses_short_keys(self) -> bool {
        matches!(self, Self::Compact)
    }
}

static LOG_LEVEL: AtomicU8 = AtomicU8::new(3);
static YES: AtomicBool = AtomicBool::new(false);
static MODE: AtomicU8 = AtomicU8::new(0);
static STREAM: AtomicBool = AtomicBool::new(false);
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// How captured output is shaped. `Structured` keeps the value as the
/// extension's in-process calls expect; `Text` and `Json` reproduce what
/// `--output-mode text` and `--output-mode json` print.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CaptureMode {
    Structured,
    Text,
    Json,
}

thread_local! {
    static CAPTURED_OUTPUT: RefCell<Result<Option<Value>, ()>> = const { RefCell::new(Err(())) };
    static CAPTURE_MODE: Cell<CaptureMode> = const { Cell::new(CaptureMode::Structured) };
    static CAPTURED_STDOUT: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn output_is_captured() -> bool {
    CAPTURED_OUTPUT.with_borrow(Result::is_ok)
}

static FIELDS: OnceLock<Vec<String>> = OnceLock::new();

/// `--fields KEY,KEY` keeps only those keys of a printed result and of each
/// record inside it; `error` always stays.
pub(crate) fn prime_fields(fields: &[String]) {
    let fields = fields
        .iter()
        .map(|field| field.trim().to_string())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if !fields.is_empty() {
        let _ = FIELDS.set(fields);
    }
}

fn project_record(value: &Value, fields: &[String]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| fields.iter().any(|field| field == *key) || *key == "error")
                .map(|(key, inner)| (key.clone(), inner.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub(crate) fn project_fields(value: &Value, fields: &[String]) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, inner) in map {
        if fields.iter().any(|field| field == key) || key == "error" {
            out.insert(key.clone(), inner.clone());
            continue;
        }
        if let Value::Array(items) = inner
            && !items.is_empty()
            && items.iter().all(Value::is_object)
        {
            let projected = items
                .iter()
                .map(|item| project_record(item, fields))
                .collect::<Vec<_>>();
            if projected
                .iter()
                .any(|item| item.as_object().is_some_and(|member| !member.is_empty()))
            {
                out.insert(key.clone(), Value::Array(projected));
            }
        }
    }
    Value::Object(out)
}

pub(crate) fn prime_mode(mode: &str) {
    MODE.store(
        match mode {
            "json" => 1,
            "pretty" => 2,
            _ => 0,
        },
        Ordering::Relaxed,
    );
}

pub(crate) fn validate_options(cli: &Cli) -> Result<()> {
    if !matches!(
        cli.log_level.as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    ) {
        bail!(
            "Invalid --log-level '{}'; use off, error, warn, info, debug, or trace",
            cli.log_level
        );
    }
    if !matches!(cli.color.as_str(), "auto" | "always" | "never") {
        bail!(
            "Invalid --color '{}'; use auto, always, or never",
            cli.color
        );
    }
    if !matches!(cli.output_mode.as_str(), "text" | "json" | "pretty") {
        bail!(
            "Invalid --output-mode '{}'; use text, json, or pretty",
            cli.output_mode
        );
    }
    Ok(())
}

pub(crate) fn configure(cli: &Cli) {
    LOG_LEVEL.store(
        match cli.log_level.as_str() {
            "off" => 0,
            "error" => 1,
            "warn" => 2,
            "info" => 3,
            "debug" => 4,
            "trace" => 5,
            _ => unreachable!("global CLI options are validated first"),
        },
        Ordering::Relaxed,
    );
    YES.store(cli.yes, Ordering::Relaxed);
    prime_mode(&cli.output_mode);
    prime_fields(&cli.fields);
    unsafe {
        match cli.color.as_str() {
            "always" => {
                std::env::remove_var("NO_COLOR");
                std::env::set_var("CLICOLOR_FORCE", "1");
            }
            "never" => {
                std::env::set_var("NO_COLOR", "1");
                std::env::remove_var("CLICOLOR_FORCE");
            }
            _ => {}
        }
    }
}

pub(crate) fn global_log_enabled(level: u8) -> bool {
    LOG_LEVEL.load(Ordering::Relaxed) >= level
}

pub(crate) fn log_global(level: u8, message: std::fmt::Arguments<'_>) {
    if global_log_enabled(level) {
        write_stderr(message);
    }
    if level <= LOG_FILE_LEVEL || global_log_enabled(level) {
        append_log_file(message);
    }
}

pub(crate) fn write_stdout(message: fmt::Arguments<'_>) {
    if CAPTURED_STDOUT.with_borrow_mut(|captured| {
        captured.as_mut().is_some_and(|buffer| {
            buffer.push_str(&fmt::format(message));
            buffer.push('\n');
            true
        })
    }) {
        return;
    }
    let _ = write_output_line(io::stdout().lock(), message);
}

/// Text written to stdout without a trailing newline (a rendered store, a
/// sourcemap), captured like a printed line.
pub(crate) fn write_stdout_text(message: fmt::Arguments<'_>) {
    if CAPTURED_STDOUT.with_borrow_mut(|captured| {
        captured.as_mut().is_some_and(|buffer| {
            buffer.push_str(&fmt::format(message));
            true
        })
    }) {
        return;
    }
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_fmt(message);
    let _ = stdout.flush();
}

pub(crate) fn write_stderr(message: fmt::Arguments<'_>) {
    let _ = write_output_line(io::stderr().lock(), message);
}

fn write_output_line(mut writer: impl Write, message: fmt::Arguments<'_>) -> io::Result<()> {
    // Display implementations such as serde_json::Value emit many fragments.
    // Buffer the complete line so unbuffered stderr does not write each one.
    let mut line = fmt::format(message);
    line.push('\n');
    writer.write_all(line.as_bytes())
}

pub(crate) fn global_yes() -> bool {
    YES.load(Ordering::Relaxed)
}

pub(crate) fn global_pretty_output(local: bool) -> bool {
    local || MODE.load(Ordering::Relaxed) == 2
}

pub(crate) fn global_json_output() -> bool {
    (output_is_captured() && CAPTURE_MODE.get() == CaptureMode::Json)
        || MODE.load(Ordering::Relaxed) != 0
}

/// Text mode for a reader at a terminal or an agent: compact shapes apply.
/// Captured output (tests, the extension's in-process calls) keeps the
/// structured value, like `--output-mode json`, unless it is captured as
/// text.
pub(crate) fn compact_text_output() -> bool {
    if output_is_captured() {
        return CAPTURE_MODE.get() == CaptureMode::Text;
    }
    !global_json_output()
}

/// Text mode with nothing capturing the output: a command may print a form
/// that is not JSON, which a capturing caller could not take as a value.
pub(crate) fn prints_text() -> bool {
    !output_is_captured() && !global_json_output()
}

pub(crate) fn set_global_stream_output(enabled: bool) {
    STREAM.store(enabled, Ordering::Relaxed);
}

pub(crate) fn emit_global_output(value: &Value, text: &str) -> Result<()> {
    if compact_text_output() {
        println!("{text}");
        Ok(())
    } else {
        print_json_output(value, false)
    }
}

pub(crate) fn ensure_plugin_api_ok(result: &Value) -> Result<()> {
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Plugin API returned ok=false");
        bail!("{message}");
    }
    Ok(())
}

const LUAU_ERROR_HINTS: [(&str, &str); 4] = [
    (
        "cannot read 'Source'",
        "a runner cannot read Script.Source; `rbx sr PATH` reads a script and `rbx sg TEXT` searches them",
    ),
    (
        "lacking capability Plugin",
        "profiling inside runners is blocked; use `rbx perf start --player N` or `rbx perf micro-start`",
    ),
    (
        "Luau runner timed out after",
        "pass -t SECONDS, or use --detach NAME for samplers and read them with --collect NAME",
    ),
    (
        "is not a valid member of",
        "`rbx tr SERVICE.PATH` lists an instance's children and `rbx l \"return X.ClassName\"` its class",
    ),
];

fn luau_error_hint(text: &str) -> Option<&'static str> {
    LUAU_ERROR_HINTS
        .iter()
        .find(|(pattern, _)| text.contains(pattern))
        .map(|(_, hint)| *hint)
}

/// A failed Luau run as the plugin reported it: the folded message, plus the
/// console lines Studio logged during the run on their own.
#[derive(Debug)]
pub(crate) struct LuauRunFailure {
    message: String,
    pub(crate) console_errors: Vec<String>,
}

impl LuauRunFailure {
    /// The same failure named after the runtime it came from, keeping the
    /// console lines a JSON caller reads as `consoleErrors`.
    pub(crate) fn in_context(mut self, label: &str) -> Self {
        self.message = format!("[{label}] {}", self.message);
        self
    }
}

impl fmt::Display for LuauRunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LuauRunFailure {}

pub(crate) fn ensure_luau_api_ok(result: &Value) -> Result<()> {
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Luau command failed");
        let captured = result
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let message = entry.get("message").and_then(Value::as_str)?;
                let kind = entry
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("output");
                Some(format!("[{kind}] {message}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let console_errors = result
            .get("consoleErrors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let console = console_errors.join(
            "
",
        );
        let mut text = message.to_string();
        if !captured.is_empty() {
            text.push_str(
                "
Command output:
",
            );
            text.push_str(&captured);
        }
        if !console.is_empty() {
            text.push_str(
                "
Studio logged during the run:
",
            );
            text.push_str(&console);
        }
        if let Some(hint) = luau_error_hint(&text) {
            text.push_str(
                "
Hint: ",
            );
            text.push_str(hint);
        }
        return Err(LuauRunFailure {
            message: text,
            console_errors,
        }
        .into());
    }
    Ok(())
}

pub(crate) fn print_json_output(value: &Value, pretty: bool) -> Result<()> {
    let captured = CAPTURED_OUTPUT.with_borrow_mut(|output| match output {
        Ok(output) => {
            *output = Some(if CAPTURE_MODE.get() == CaptureMode::Text {
                let mut text = relativize_project_paths(value.clone());
                shorten_single_precision_floats(&mut text);
                text
            } else {
                value.clone()
            });
            true
        }
        Err(()) => false,
    });
    if captured {
        return Ok(());
    }
    let relative;
    let value = if MODE.load(Ordering::Relaxed) == 0 {
        let mut text = relativize_project_paths(value.clone());
        shorten_single_precision_floats(&mut text);
        trim_store_edit_result(&mut text);
        crate::editor::review::compact_held_review(&mut text);
        relative = text;
        &relative
    } else {
        value
    };
    let projected;
    let value = if let Some(fields) = FIELDS.get() {
        projected = project_fields(value, fields);
        &projected
    } else {
        value
    };
    if global_pretty_output(pretty) && !STREAM.load(Ordering::Relaxed) {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
}

/// What the result of a store edit repeats, which its text form leaves out:
/// the store file that `changedPaths` already names, the store index and the
/// split path (printed as one dotted `path`), the push summary's own `ok`
/// and its `persistence`, and the read-back's path and class once `verified`
/// is true. A result without `changedPaths` is not a store edit and stays,
/// and so does a captured result, which keeps every field for its reader.
fn trim_store_edit_result(value: &mut Value) {
    let Some(result) = value.as_object_mut() else {
        return;
    };
    let Some(changed) = result.get("changedPaths").and_then(Value::as_array) else {
        return;
    };
    if result
        .get("settingsFile")
        .is_some_and(|file| changed.contains(file))
    {
        result.remove("settingsFile");
    }
    let segments = result
        .get("pathSegments")
        .and_then(Value::as_array)
        .and_then(|segments| {
            segments
                .iter()
                .map(|segment| segment.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        });
    let ordinals = result
        .get("pathOrdinals")
        .and_then(Value::as_array)
        .and_then(|ordinals| {
            ordinals
                .iter()
                .map(|ordinal| ordinal.as_u64().map(|ordinal| ordinal as usize))
                .collect::<Option<Vec<_>>>()
        });
    if !result.contains_key("path")
        && let Some(segments) = segments
        && let Some(path) =
            crate::bytecode::explorer::compact_path_string(&segments, ordinals.as_deref())
    {
        for key in ["pathSegments", "pathOrdinals", "index"] {
            result.remove(key);
        }
        result.insert("path".to_string(), Value::String(path));
    }
    if let Some(push) = result.get_mut("push").and_then(Value::as_object_mut) {
        if push.get("ok") == Some(&Value::Bool(true)) {
            push.remove("ok");
        }
        push.remove("persistence");
        if push.is_empty() {
            result.remove("push");
        }
    }
    if result.get("verified") == Some(&Value::Bool(true))
        && let Some(studio) = result.get_mut("studio").and_then(Value::as_object_mut)
    {
        studio.remove("path");
        studio.remove("class");
        if studio.is_empty() {
            result.remove("studio");
        }
    }
}

static PROJECT_PREFIXES: OnceLock<Vec<String>> = OnceLock::new();

fn project_prefixes() -> &'static [String] {
    PROJECT_PREFIXES.get_or_init(|| {
        let Ok(dir) = std::env::current_dir() else {
            return Vec::new();
        };
        let text = dir
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_string();
        let mut prefixes = vec![format!("{text}\\"), format!("{text}/")];
        let forward = text.replace('\\', "/");
        if forward != text {
            prefixes.push(format!("{forward}/"));
        }
        prefixes
    })
}

/// Text-mode output prints paths inside the working directory relative to it.
fn relativize_project_paths(mut value: Value) -> Value {
    fn visit(value: &mut Value) {
        match value {
            Value::String(text) => {
                for prefix in project_prefixes() {
                    if text.len() > prefix.len()
                        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
                    {
                        text.drain(..prefix.len());
                        if text.contains('\\') {
                            *text = text.replace('\\', "/");
                        }
                        break;
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(visit),
            Value::Object(map) => map.values_mut().for_each(visit),
            _ => {}
        }
    }
    visit(&mut value);
    value
}

/// Studio stores most numbers as single precision; their exact double
/// expansion (0.20000000298023224) costs tokens without adding information.
/// Text output prints the shortest decimal that round-trips the same f32.
pub(crate) fn shorten_single_precision_floats(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if let Some(double) = number.as_f64()
                && number.as_i64().is_none()
                && number.as_u64().is_none()
                && double.is_finite()
            {
                let single = double as f32;
                if f64::from(single) == double
                    && let Ok(parsed) = single.to_string().parse::<f64>()
                    && let Some(shortened) = serde_json::Number::from_f64(parsed)
                {
                    *number = shortened;
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(shorten_single_precision_floats),
        Value::Object(map) => map.values_mut().for_each(shorten_single_precision_floats),
        _ => {}
    }
}

/// Removes null, empty-string, empty-array and empty-object members.
pub(crate) fn strip_empty(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.values_mut().for_each(strip_empty);
            map.retain(|_, entry| match entry {
                Value::Null => false,
                Value::String(text) => !text.is_empty(),
                Value::Array(items) => !items.is_empty(),
                Value::Object(members) => !members.is_empty(),
                _ => true,
            });
        }
        Value::Array(items) => items.iter_mut().for_each(strip_empty),
        _ => {}
    }
}

/// Removes the named members when they are empty arrays.
pub(crate) fn drop_empty(value: &mut Value, keys: &[&str]) {
    if let Some(map) = value.as_object_mut() {
        for key in keys {
            if map
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                map.remove(*key);
            }
        }
    }
}

/// Removes the named members when they are `false`.
pub(crate) fn drop_false(value: &mut Value, keys: &[&str]) {
    if let Some(map) = value.as_object_mut() {
        for key in keys {
            if map.get(*key) == Some(&Value::Bool(false)) {
                map.remove(*key);
            }
        }
    }
}

pub(crate) fn capture_json_output(run: impl FnOnce() -> Result<()>) -> Result<Value> {
    let (result, output, _) = capture_output(CaptureMode::Structured, run);
    result?;
    Ok(output.unwrap_or_else(|| serde_json::json!({})))
}

/// Runs a command with its printed result and stdout text captured instead
/// of written, shaped as `mode` would print them.
pub(crate) fn capture_output(
    mode: CaptureMode,
    run: impl FnOnce() -> Result<()>,
) -> (Result<()>, Option<Value>, String) {
    CAPTURED_OUTPUT.with_borrow_mut(|output| *output = Ok(None));
    CAPTURE_MODE.set(mode);
    CAPTURED_STDOUT.with_borrow_mut(|captured| {
        *captured = (mode != CaptureMode::Structured).then(String::new)
    });
    let result = run();
    let output = CAPTURED_OUTPUT
        .with_borrow_mut(|output| std::mem::replace(output, Err(())).unwrap_or_default());
    CAPTURE_MODE.set(CaptureMode::Structured);
    let stdout = CAPTURED_STDOUT
        .with_borrow_mut(Option::take)
        .unwrap_or_default();
    (result, output, stdout)
}

pub(crate) fn automation_token(prefix: &str) -> String {
    let sequence = TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}_{:x}_{:x}_{:x}",
        std::process::id(),
        current_millis(),
        sequence
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_log_line_is_one_write_and_preserves_output() {
        #[derive(Default)]
        struct Writer {
            bytes: Vec<u8>,
            writes: usize,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let value = serde_json::json!({"groups": (0..100).map(|index| {
            serde_json::json!({"name": format!("service {index}"), "ms": 0.25})
        }).collect::<Vec<_>>()});
        let mut writer = Writer::default();
        write_output_line(&mut writer, format_args!("[renium] profile {value}")).unwrap();
        assert_eq!(writer.writes, 1);
        assert_eq!(
            writer.bytes,
            format!("[renium] profile {value}\n").as_bytes()
        );
    }
}

#[cfg(test)]
mod luau_hint_tests {
    use super::ensure_luau_api_ok;
    use serde_json::json;

    #[test]
    fn luau_failures_name_the_next_step() {
        let error = |result| format!("{:#}", ensure_luau_api_ok(&result).unwrap_err());
        let profiler = error(json!({
            "ok": false,
            "error": "ReniumRunner:2: The current thread cannot call 'Start' (lacking capability Plugin)",
        }));
        assert!(profiler.ends_with(
            "\nHint: profiling inside runners is blocked; use `rbx perf start --player N` or `rbx perf micro-start`"
        ));
        let captured = error(json!({
            "ok": false,
            "error": "Luau command failed",
            "output": [{"type": "error", "message": "cannot call ScriptProfilerService (lacking capability Plugin)"}],
        }));
        assert!(
            captured.contains("Command output:\n[error]") && captured.contains("Hint: profiling")
        );
        let timeout = error(
            json!({"ok": false, "error": "Luau runner timed out after 10.0s and was stopped"}),
        );
        assert_eq!(
            timeout,
            "Luau runner timed out after 10.0s and was stopped\nHint: pass -t SECONDS, or use --detach NAME for samplers and read them with --collect NAME"
        );
        let edit = error(
            json!({"ok": false, "error": "Luau execution timed out after 10.0s and was stopped"}),
        );
        assert!(!edit.contains("Hint:"));
        assert!(ensure_luau_api_ok(&json!({"ok": true})).is_ok());
    }
}

#[cfg(test)]
mod store_edit_result_tests {
    use super::trim_store_edit_result;
    use serde_json::json;

    #[test]
    fn a_pushed_edit_keeps_its_verdict_and_values_without_the_repeats() {
        let mut set = json!({
            "changedPaths": ["instances/StarterGui.renium"],
            "ok": true,
            "push": {"ok": true, "persistence": "unsaved", "properties": 2},
            "set": ["Text"],
            "settingsFile": "instances/StarterGui.renium",
            "settingsId": "editor:88d",
            "studio": {"Text": "Redeem", "class": "TextLabel", "path": "StarterGui.Main.Title"},
            "verified": true,
        });
        trim_store_edit_result(&mut set);
        assert_eq!(
            set,
            json!({
                "changedPaths": ["instances/StarterGui.renium"],
                "ok": true,
                "push": {"properties": 2},
                "set": ["Text"],
                "settingsId": "editor:88d",
                "studio": {"Text": "Redeem"},
                "verified": true,
            })
        );
    }

    #[test]
    fn an_added_instance_prints_one_dotted_path() {
        let mut added = json!({
            "changedPaths": ["instances/Workspace.renium"],
            "index": 41,
            "ok": true,
            "pathOrdinals": [1, 1, 2],
            "pathSegments": ["Workspace", "Lobby", "Pad"],
            "push": {"ok": true, "persistence": "unsaved"},
            "settingsFile": "instances/Workspace.renium",
            "settingsId": "editor:6c71",
            "studio": {"class": "Part", "path": "Workspace.Lobby.Pad[2]"},
            "verified": true,
        });
        trim_store_edit_result(&mut added);
        assert_eq!(
            added,
            json!({
                "changedPaths": ["instances/Workspace.renium"],
                "ok": true,
                "path": "Workspace.Lobby.Pad[2]",
                "settingsId": "editor:6c71",
                "verified": true,
            })
        );
    }
}

#[cfg(test)]
mod field_projection_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_keep_listed_keys_at_the_top_and_in_records() {
        let value = json!({
            "ok": true, "count": 2, "error": "none",
            "versions": [{"version": 3, "createdBy": 1, "title": null}, {"version": 2, "createdBy": 1}],
            "flat": [1, 2], "meta": {"version": 9}
        });
        let fields = ["version".to_string(), "count".to_string()];
        assert_eq!(
            project_fields(&value, &fields),
            json!({"count": 2, "error": "none", "versions": [{"version": 3}, {"version": 2}]})
        );
        assert_eq!(project_fields(&json!([1]), &fields), json!([1]));
    }
}
