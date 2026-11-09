use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
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

macro_rules! print {
    ($($argument:tt)*) => {{
        if $crate::app::context::automation_stdio() {
            $crate::app::output::write_stderr(format_args!($($argument)*));
        } else {
            $crate::app::output::write_stdout_text(format_args!($($argument)*));
        }
    }};
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
            if app::output::global_json_output() || !std::io::stderr().is_terminal() {
                eprintln!(
                    "{}",
                    serde_json::to_string(&structured_error(&error)).unwrap_or_else(|_| {
                        "{\"ok\":false,\"error\":\"Renium failed\"}".to_string()
                    })
                );
            } else {
                eprintln!("Error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

/// The JSON a failed command prints: the message, plus the fields of the
/// structured detail the daemon attached (`consoleErrors` for a Luau run
/// whose module failed to load, `sourceVerifyErrors` for a push).
fn structured_error(error: &anyhow::Error) -> serde_json::Value {
    let mut body = json!({
        "ok": false,
        "error": format!("{error:#}"),
    });
    let detail = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<automation::DaemonFailure>())
        .and_then(|failure| failure.detail.as_ref())
        .and_then(serde_json::Value::as_object);
    for (key, value) in detail.into_iter().flatten() {
        if !matches!(key.as_str(), "ok" | "error") {
            body[key] = value.clone();
        }
    }
    body
}

fn arguments_after_leading_root() -> Result<Vec<std::ffi::OsString>> {
    let mut arguments: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // A `--daemon NAME` ahead of the root keeps the root leading: a wrapper
    // that pins the daemon must not change what `rbx -r NEW status` does.
    let mut start = 1;
    while let Some(argument) = arguments.get(start).and_then(|value| value.to_str()) {
        if argument == "--daemon" && arguments.len() > start + 1 {
            start += 2;
        } else if argument.starts_with("--daemon=") {
            start += 1;
        } else {
            break;
        }
    }
    let Some(first) = arguments.get(start).and_then(|value| value.to_str()) else {
        return Ok(arguments);
    };
    let (root, consumed) = if matches!(first, "-r" | "--root" | "--project-root") {
        match arguments.get(start + 1) {
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
    arguments.drain(start..start + consumed);
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
    if command_projects_its_own_fields(&matches) {
        cli.fields.clear();
    }
    app::output::prime_mode(&cli.output_mode);
    app::output::prime_fields(&cli.fields);
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
            .filter(|value| !value.trim().is_empty()),
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
    if !is_plugin_child() && checks_agent_instructions(&cli.command) {
        refresh_agent_instructions(cli.project.as_deref());
    }

    if daemon::is_default_daemon()
        && resumes_global_audio(&cli.command)
        && let Err(error) = studio::audio::global::resume()
    {
        // Resuming is housekeeping for a Studio that may not be there; a
        // sandboxed or Studio-less run must not see it in the output.
        log_global(
            5,
            format_args!("[renium] Global Studio audio could not resume: {error:#}"),
        );
    }
    cli::dispatch::dispatch(cli.command, cli.project.as_deref())
}

/// Offline commands never touch Studio audio: a syntax check, a bug report
/// or a plugin call has no Studio to resume.
fn resumes_global_audio(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::AudioWorker(_)
            | Commands::AudioGlobalWorker
            | Commands::UpdateHelper(_)
            | Commands::NativeProbe(_)
            | Commands::BridgeDaemon(_)
            | Commands::CheckLuau(_)
            | Commands::Report(_)
            | Commands::Plugin(_)
            | Commands::External(_)
            | Commands::Help(_)
            | Commands::Mcp(_)
    )
}

fn checks_agent_update(command: &Commands) -> bool {
    !matches!(
        command,
        Commands::UpdateHelper(_)
            | Commands::NativeProbe(_)
            | Commands::Mcp(_)
            | Commands::Plugin(_)
            | Commands::External(_)
            | Commands::Help(_)
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

fn refresh_agent_instructions(project: Option<&Path>) {
    match project::workflows::refresh_outdated_agent_instructions(project) {
        Ok(changed) if !changed.is_empty() => eprintln!(
            "[renium] updated {}; reread before the next Renium step",
            changed.join(", ")
        ),
        Ok(_) => {}
        Err(error) => log_global(
            4,
            format_args!("[renium] agent instructions were not refreshed: {error:#}"),
        ),
    }
}

fn is_plugin_child() -> bool {
    std::env::var_os("RENIUM_PLUGIN_CHILD").is_some_and(|value| value == "1")
}

fn is_agent_launcher() -> bool {
    if is_plugin_child() {
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
            | Commands::NativeProbe(_)
            | Commands::ImportRojo(_)
            | Commands::Report(_)
            | Commands::Plugin(_)
            | Commands::External(_)
            | Commands::Help(_)
            | Commands::CheckLuau(_)
            | Commands::RecordEnd(_)
            | Commands::RecordReview(_)
            | Commands::Update(_)
            | Commands::UpdateHelper(_)
            | Commands::Setup(_)
            | Commands::Mcp(_)
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

/// A subcommand with its own `--fields` argument (`in`, `tr`, `f`, `bb`) shares
/// the global option's id, so clap propagates its value, presets included, into
/// the global projection; such a command renders its own fields instead.
fn command_projects_its_own_fields(matches: &clap::ArgMatches) -> bool {
    let mut command = cli::command();
    let mut matches = matches;
    while let Some((name, sub_matches)) = matches.subcommand() {
        let Some(sub_command) = command.find_subcommand(name) else {
            return false;
        };
        if sub_command
            .get_arguments()
            .any(|arg| arg.get_id() == "fields" && !arg.is_global_set())
        {
            return true;
        }
        command = sub_command.clone();
        matches = sub_matches;
    }
    false
}

#[cfg(test)]
mod startup_policy_tests {
    use super::*;

    #[test]
    fn a_daemon_failures_detail_fields_join_the_json_error() {
        let failure = automation::DaemonFailure {
            message: "Requested module experienced an error while loading".into(),
            detail: Some(json!({
                "consoleErrors": ["ReplicatedStorage.Bad:1: bad module"],
                "ok": true,
                "error": "ignored",
            })),
        };
        let error = anyhow::Error::from(failure).context("rbx l failed");
        let body = structured_error(&error);
        assert_eq!(body["ok"], false);
        assert_eq!(
            body["error"],
            "rbx l failed: Requested module experienced an error while loading"
        );
        assert_eq!(
            body["consoleErrors"],
            json!(["ReplicatedStorage.Bad:1: bad module"])
        );
        let plain = structured_error(&anyhow::anyhow!("no detail"));
        assert_eq!(plain, json!({ "ok": false, "error": "no detail" }));
    }

    #[test]
    fn a_command_with_its_own_fields_argument_keeps_the_global_projection_off() {
        for arguments in [
            vec!["rbx", "in", "Workspace", "Baseplate"],
            vec!["rbx", "in", "Workspace", "Baseplate", "-F", "id,n,c"],
            vec!["rbx", "tr", "Workspace", "-F", "all"],
            vec!["rbx", "f", "Workspace", "-n", "Door"],
            vec!["rbx", "bb", "Workspace", "-F", "id,n"],
        ] {
            let matches = cli::command().try_get_matches_from(arguments).unwrap();
            assert!(command_projects_its_own_fields(&matches));
        }
        for arguments in [
            vec!["rbx", "--fields", "ok", "dr"],
            vec!["rbx", "status", "--fields", "ok,playState"],
            vec!["rbx", "lst"],
        ] {
            let matches = cli::command().try_get_matches_from(arguments).unwrap();
            assert!(!command_projects_its_own_fields(&matches));
        }
    }

    #[test]
    fn native_probe_has_no_background_startup_work() {
        let args = ["rbx", "native-probe", "--pid", "123"];
        let matches = cli::command().try_get_matches_from(args).unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert!(!resumes_global_audio(&cli.command));
        assert!(!checks_agent_update(&cli.command));
        assert!(!checks_agent_instructions(&cli.command));
    }

    #[test]
    fn the_mcp_server_leaves_startup_work_to_the_commands_it_runs() {
        let matches = cli::command().try_get_matches_from(["rbx", "mcp"]).unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert!(!resumes_global_audio(&cli.command));
        assert!(!checks_agent_update(&cli.command));
        assert!(!checks_agent_instructions(&cli.command));
    }

    #[test]
    fn offline_commands_leave_studio_audio_alone() {
        for arguments in [
            vec!["rbx", "ck", "src"],
            vec!["rbx", "report", "-m", "what went wrong"],
        ] {
            let matches = cli::command().try_get_matches_from(arguments).unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            assert!(!resumes_global_audio(&cli.command));
        }
    }

    #[test]
    fn daemon_startup_owns_its_background_policy() {
        let matches = cli::command()
            .try_get_matches_from(["rbx", "bd", "--name", "astra"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        assert!(!resumes_global_audio(&cli.command));
    }
}
