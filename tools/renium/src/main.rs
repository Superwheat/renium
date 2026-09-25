use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::FromArgMatches;
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
            $crate::app::output::write_stderr(format_args!($format $(, $argument)*));
        }
    }};
    ($($argument:tt)*) => {
        $crate::app::output::write_stderr(format_args!($($argument)*))
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
            $crate::app::output::write_stderr(format_args!($format $(, $argument)*));
        } else {
            $crate::app::output::write_stdout(format_args!($format $(, $argument)*));
        }
    }};
    ($($argument:tt)*) => {{
        if $crate::app::context::automation_stdio() {
            $crate::app::output::write_stderr(format_args!($($argument)*));
        } else {
            $crate::app::output::write_stdout(format_args!($($argument)*));
        }
    }};
}

mod app;
mod automation;
mod bytecode;
mod cli;
mod cloud;
mod collab;
mod daemon;
mod editor;
mod plugins;
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
    main_result()
}

fn main_result() -> ExitCode {
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

fn arguments_after_leading_root() -> Result<Vec<std::ffi::OsString>> {
    let mut arguments: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(first) = arguments.get(1).and_then(|value| value.to_str()) else {
        return Ok(arguments);
    };
    let (root, consumed) = if matches!(first, "-r" | "--root" | "--project-root") {
        match arguments.get(2) {
            Some(path) => (std::path::PathBuf::from(path), 2),
            None => return Ok(arguments),
        }
    } else if let Some(path) = first
        .strip_prefix("--root=")
        .or_else(|| first.strip_prefix("--project-root="))
    {
        (std::path::PathBuf::from(path), 1)
    } else {
        return Ok(arguments);
    };
    arguments.drain(1..=consumed);
    if arguments.len() > 1 {
        let mut with_flag = arguments.clone();
        with_flag.push("-r".into());
        with_flag.push(root.as_os_str().to_owned());
        if cli::command().try_get_matches_from(&with_flag).is_ok() {
            return Ok(with_flag);
        }
    }
    std::fs::create_dir_all(&root)
        .with_context(|| format!("Could not create {}", root.display()))?;
    std::env::set_current_dir(&root)
        .with_context(|| format!("Could not enter {}", root.display()))?;
    Ok(arguments)
}

fn run_cli() -> Result<()> {
    app::crash::install_hook();
    let matches = cli::command().get_matches_from(arguments_after_leading_root()?);
    if let Ok(cwd) = std::env::current_dir() {
        let long = system::files::expand_short_names(cwd.clone());
        if long != cwd {
            let _ = std::env::set_current_dir(long);
        }
    }
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
    if is_agent_launcher() && checks_agent_update(&cli.command) {
        update::check_agent_update();
    }
    if is_agent_launcher()
        && checks_agent_instructions(&cli.command)
        && project::workflows::refresh_outdated_agent_instructions(cli.project.as_deref())?
    {
        bail!(
            "Renium instructions were outdated and have been updated. Reread RENIUM.md, then run the command again"
        );
    }

    if !matches!(
        &cli.command,
        Commands::AudioWorker(_) | Commands::AudioGlobalWorker | Commands::UpdateHelper(_)
    ) && let Err(error) = studio::audio::global::resume()
    {
        eprintln!("[renium] Global Studio audio could not resume: {error:#}");
    }
    cli::dispatch::dispatch(cli.command, cli.project.as_deref())
}

fn checks_agent_update(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::UpdateHelper(_)
            | Commands::Plugin(_)
            | Commands::External(_)
            | Commands::CheckLuau(_)
            | Commands::RecordEnd(_)
            | Commands::RecordReview(_)
            | Commands::BridgeDaemon(_)
            | Commands::ExplorerDaemon(_)
            | Commands::PerformanceWorker
            | Commands::AudioWorker(_)
            | Commands::AudioGlobalWorker
            | Commands::PerformanceHolder(_)
            | Commands::CursorPoll(_)
    )
}

fn is_agent_launcher() -> bool {
    if std::env::var_os("RENIUM_PLUGIN_CHILD").is_some_and(|value| value == "1") {
        return false;
    }
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
            | Commands::Plugin(_)
            | Commands::External(_)
            | Commands::CheckLuau(_)
            | Commands::RecordEnd(_)
            | Commands::RecordReview(_)
            | Commands::Update(_)
            | Commands::UpdateHelper(_)
            | Commands::Setup(_)
            | Commands::Daemon(_)
            | Commands::BridgeDaemon(_)
            | Commands::ExplorerDaemon(_)
            | Commands::PerformanceProfile(_)
            | Commands::PerformanceWorker
            | Commands::AudioWorker(_)
            | Commands::AudioGlobalWorker
            | Commands::PerformanceHolder(_)
            | Commands::CursorPoll(_)
    )
}
