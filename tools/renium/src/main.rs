use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

macro_rules! eprintln {
    ($format:literal $(, $argument:expr)* $(,)?) => {{
        if $format.starts_with("[renium]") {
            $crate::output::log_global(
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
            $crate::output::log_global(
                if $format.starts_with("[renium] warning") { 2 } else { 3 },
                format_args!($format $(, $argument)*),
            );
        } else if $crate::runtime_context::automation_stdio() {
            std::eprintln!($format $(, $argument)*);
        } else {
            std::println!($format $(, $argument)*);
        }
    }};
    ($($argument:tt)*) => {{
        if $crate::runtime_context::automation_stdio() {
            std::eprintln!($($argument)*);
        } else {
            std::println!($($argument)*);
        }
    }};
}

mod automation;
mod automation_runtime;
mod bridge_server;
mod build_info;
mod bytecode_api;
mod bytecode_edit;
mod bytecode_explorer;
mod bytecode_query;
mod cli_config;
mod command_args;
mod command_dispatch;
mod command_line;
mod crash_reporting;
mod daemon_control;
mod editor_diff;
mod editor_document;
mod editor_history;
mod editor_paths;
mod editor_review;
mod editor_sync;
mod editor_types;
mod external_tools;
mod file_io;
mod input_inject;
mod instance_api;
mod lifecycle;
mod local_transport;
mod native_editor;
mod native_import;
#[cfg(any(windows, target_os = "macos"))]
mod native_snapshot;
mod output;
pub(crate) use output::{emit_global_output, log_global, set_global_stream_output};
mod package_links;
mod place_packages;
mod place_target;
mod project_commands;
mod project_config;
mod project_layout;
mod property_schema;
mod rbx_decode;
mod rbx_encode;
mod rbx_model;
mod runtime_context;
mod services;
mod settings_bytecode;
mod settings_tree;
mod setup;
mod snapshot_codec;
mod snapshot_export;
mod snapshot_import;
mod snapshot_refs;
mod snapshot_types;
mod sourcemap;
mod studio_automation;
#[cfg(windows)]
mod studio_native_serializer;
#[cfg(target_os = "macos")]
mod studio_native_serializer_macos;
#[cfg(test)]
mod test_support;
mod timing;
mod version_control;
mod workflows;
#[cfg(target_os = "macos")]
use studio_native_serializer_macos as studio_native_serializer;

use command_line::{Cli, Commands};
use place_target::set_place_filter;

fn main() -> ExitCode {
    match run_cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if output::global_json_output() {
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
    crash_reporting::install_hook();
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    output::prime_mode(&cli.output_mode);
    if cli.backtrace {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }
    cli_config::apply_merged(&mut cli, &matches)?;
    if cli.verbose > 0 {
        cli.log_level = if cli.verbose > 1 {
            "trace".to_string()
        } else {
            "debug".to_string()
        };
    }
    output::validate_options(&cli)?;
    output::configure(&cli);
    if cli.backtrace {
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }
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
    runtime_context::set_cli_project(cli.project.clone());
    if !matches!(&cli.command, Commands::UpdateHelper(_)) {
        lifecycle::report_pending_update_result();
    }

    command_dispatch::dispatch(cli.command, cli.project.as_deref())
}

#[cfg(test)]
mod main_tests;
