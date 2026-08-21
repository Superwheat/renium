use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

macro_rules! eprintln {
    ($format:literal $(, $argument:expr)* $(,)?) => {{
        if $format.starts_with("[renium]") {
            $crate::app::output::log_global(
                if $format.starts_with("[renium] warning") {
                    2
                } else if $format.contains("failed") || $format.contains("error") {
                    1
                } else {
                    3
                },
                format_args!($format $(, $argument)*),
            );
        } else {
            std::eprintln!($format $(, $argument)*);
        }
    }};
    ($($argument:tt)*) => {
        std::eprintln!($($argument)*)
    };
}

macro_rules! println {
    ($format:literal $(, $argument:expr)* $(,)?) => {{
        if $format.starts_with("[renium]") {
            $crate::app::output::log_global(
                if $format.starts_with("[renium] warning") { 2 } else { 3 },
                format_args!($format $(, $argument)*),
            );
        } else if $crate::app::context::automation_stdio() {
            std::eprintln!($format $(, $argument)*);
        } else {
            std::println!($format $(, $argument)*);
        }
    }};
    ($($argument:tt)*) => {{
        if $crate::app::context::automation_stdio() {
            std::eprintln!($($argument)*);
        } else {
            std::println!($($argument)*);
        }
    }};
}

mod app;
mod automation;
mod bytecode;
mod cli;
mod cloud;
mod daemon;
mod editor;
mod project;
mod rbx;
mod roblox;
mod settings;
mod snapshot;
mod studio;
mod system;
#[cfg(test)]
mod tests;

pub(crate) use app::output::{emit_global_output, log_global, set_global_stream_output};

use app::{context, update};
use cli::{Cli, Commands};
use studio::target::set_place_filter;

fn main() -> ExitCode {
    match run_cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error
                .downcast_ref::<app::output::ReportedFailure>()
                .is_some()
            {
                return ExitCode::FAILURE;
            }
            if app::output::global_json_output() {
                eprintln!(
                    "{}",
                    serde_json::to_string(&json!({
                        "ok": false,
                        "error": format!("{error:#}"),
                    }))
                    .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"Renium failed\"}".to_string())
                );
            } else {
                eprintln!("Error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run_cli() -> Result<()> {
    app::crash::install_hook();
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    app::output::prime_mode(&cli.output_mode);
    if cli.backtrace {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }
    cli::config::apply_merged(&mut cli, &matches)?;
    if cli.verbose > 0 {
        cli.log_level = if cli.verbose > 1 {
            "trace".to_string()
        } else {
            "debug".to_string()
        };
    }
    app::output::validate_options(&cli)?;
    app::output::configure(&cli);
    set_place_filter(
        cli.place
            .clone()
            .or_else(|| std::env::var("RENIUM_PLACE").ok())
            .or_else(|| std::env::var("PLACE").ok()),
    );
    if let Some(name) = cli.daemon.as_deref() {
        unsafe {
            std::env::set_var("RENIUM_DAEMON_NAME", name);
        }
    }
    context::set_cli_project(cli.project.clone());
    if !matches!(&cli.command, Commands::UpdateHelper(_)) {
        update::report_pending_update_result();
    }
    if is_agent_launcher()
        && checks_agent_instructions(&cli.command)
        && project::workflows::refresh_outdated_agent_instructions(cli.project.as_deref())?
    {
        bail!(
            "Renium instructions were outdated and have been updated. Reread RENIUM.md, then run the command again"
        );
    }

    cli::dispatch::dispatch(cli.command, cli.project.as_deref())
}

fn is_agent_launcher() -> bool {
    if std::env::var_os("RENIUM_AGENT_CLI").is_some_and(|value| value != "0") {
        return true;
    }
    std::env::args_os()
        .next()
        .as_deref()
        .and_then(|argument| Path::new(argument).file_stem())
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("rbx"))
}

fn checks_agent_instructions(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::Init(_)
            | Commands::Update(_)
            | Commands::UpdateHelper(_)
            | Commands::Setup(_)
            | Commands::Daemon(_)
            | Commands::BridgeDaemon(_)
            | Commands::ExplorerDaemon(_)
            | Commands::BridgeGetSource(_)
            | Commands::CursorPoll(_)
    )
}
