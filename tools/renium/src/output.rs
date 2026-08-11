use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::command_line::Cli;
use crate::timing::current_millis;

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
        std::eprintln!("{message}");
    }
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
    if global_pretty_output(pretty) && !STREAM.load(Ordering::Relaxed) {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", serde_json::to_string(value)?);
    }
    Ok(())
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
