use std::cell::RefCell;
use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::app::timing::current_millis;
use crate::cli::Cli;

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

thread_local! {
    static CAPTURED_OUTPUT: RefCell<Result<Option<Value>, ()>> = const { RefCell::new(Err(())) };
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
}

pub(crate) fn write_stdout(message: fmt::Arguments<'_>) {
    let _ = write_output_line(io::stdout().lock(), message);
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
    MODE.load(Ordering::Relaxed) != 0
}

pub(crate) fn set_global_stream_output(enabled: bool) {
    STREAM.store(enabled, Ordering::Relaxed);
}

pub(crate) fn emit_global_output(value: &Value, text: &str) -> Result<()> {
    if CAPTURED_OUTPUT.with_borrow(Result::is_ok) || global_json_output() {
        print_json_output(value, false)
    } else {
        println!("{text}");
        Ok(())
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
        if captured.is_empty() {
            bail!("{message}");
        }
        bail!("{message}\nCommand output:\n{captured}");
    }
    Ok(())
}

pub(crate) fn print_json_output(value: &Value, pretty: bool) -> Result<()> {
    let captured = CAPTURED_OUTPUT.with_borrow_mut(|output| match output {
        Ok(output) => {
            *output = Some(value.clone());
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
        relative = text;
        &relative
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
    CAPTURED_OUTPUT.with_borrow_mut(|output| *output = Ok(None));
    let result = run();
    let output = CAPTURED_OUTPUT
        .with_borrow_mut(|output| std::mem::replace(output, Err(())).unwrap_or_default());
    result?;
    Ok(output.unwrap_or_else(|| serde_json::json!({})))
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
mod float_output_tests {
    use super::shorten_single_precision_floats;
    use serde_json::json;

    #[test]
    fn single_precision_expansions_print_short_and_doubles_stay_exact() {
        let mut value = json!({
            "size": [0.20000000298023224, 0.699999988079071, 1.0, 228.31427001953125],
            "count": 3,
            "big": 1789795605,
            "precise": 0.1234567890123,
            "nested": {"x": 0.960784375667572}
        });
        shorten_single_precision_floats(&mut value);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"big":1789795605,"count":3,"nested":{"x":0.9607844},"precise":0.1234567890123,"size":[0.2,0.7,1.0,228.31427]}"#
        );
        let mut value = json!({"size": [0.2, 0.7, 228.31427]});
        let before = serde_json::to_string(&value).unwrap();
        shorten_single_precision_floats(&mut value);
        assert_eq!(serde_json::to_string(&value).unwrap(), before);
    }
}
