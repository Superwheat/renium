use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use crate::app;
use crate::automation::{commands::daemon_result, op};

#[derive(Args)]
pub(crate) struct PerformanceArgs {
    #[command(subcommand)]
    command: PerformanceCommand,
}

#[derive(Args)]
pub(crate) struct PerformanceHolderArgs {
    #[arg(long)]
    pub(crate) pid: u32,
    #[arg(long)]
    pub(crate) identity: String,
    #[arg(long)]
    pub(crate) locator: String,
}

#[derive(Subcommand)]
enum PerformanceCommand {
    #[command(name = "ls", alias = "list", about = "List enforceable profiles")]
    List,
    #[command(name = "cal", alias = "calibrate", about = "Calibrate this computer")]
    Calibrate,
    #[command(name = "use", about = "Apply a profile to connected Studio processes")]
    Use { name: String },
    #[command(name = "show", alias = "status", about = "Show applied constraints")]
    Show,
    #[command(name = "off", about = "Remove every performance constraint")]
    Off,
    #[command(name = "adv", alias = "advanced", about = "Apply exact constraints")]
    Advanced {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        values: Vec<String>,
    },
}

pub(crate) fn run(args: PerformanceArgs) -> Result<()> {
    let parameters = match args.command {
        PerformanceCommand::List => json!({ "action": "list" }),
        PerformanceCommand::Calibrate => json!({ "action": "calibrate" }),
        PerformanceCommand::Use { name } => json!({ "action": "use", "name": name }),
        PerformanceCommand::Show => json!({ "action": "show" }),
        PerformanceCommand::Off => json!({ "action": "off" }),
        PerformanceCommand::Advanced { values } => {
            let mut parameters =
                Map::from_iter([("action".to_string(), Value::String("advanced".to_string()))]);
            for value in values {
                let (key, value) = value
                    .split_once('=')
                    .with_context(|| format!("Expected key=value, got '{value}'"))?;
                let key = key.trim().to_ascii_lowercase();
                if key.is_empty() || value.trim().is_empty() {
                    bail!("Expected non-empty key=value");
                }
                if !matches!(
                    key.as_str(),
                    "cpu" | "cores" | "headroom" | "mem" | "prio" | "save" | "risk"
                ) {
                    bail!("Unknown advanced setting '{key}'");
                }
                if parameters
                    .insert(key.clone(), Value::String(value.trim().to_string()))
                    .is_some()
                {
                    bail!("Advanced setting '{key}' was provided twice");
                }
            }
            Value::Object(parameters)
        }
    };
    let result = daemon_result(op::PERFORMANCE_PROFILE, None, parameters, false, None)?;
    app::output::print_json_output(&result, false)
}
