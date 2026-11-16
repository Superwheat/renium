//! Which commands of a queue leave nothing behind for the next one.
use clap::Parser;

use super::{Cli, Commands};

/// The arguments without a leading `--daemon NAME` and project root, which
/// the entry point takes off before the command line is parsed.
fn without_leading_root(mut args: &[String]) -> &[String] {
    loop {
        args = match args {
            [flag, _, rest @ ..] if flag == "--daemon" => rest,
            [flag, rest @ ..] if flag.starts_with("--daemon=") => rest,
            [flag, _, rest @ ..] if matches!(flag.as_str(), "-r" | "--root" | "--project-root") => {
                return rest;
            }
            [flag, rest @ ..]
                if flag.starts_with("--root=") || flag.starts_with("--project-root=") =>
            {
                return rest;
            }
            _ => return args,
        };
    }
}

/// Whether a command only reads files, saved data, a console or the client's
/// GUI list. Nothing a later command of a queue does can depend on it, so its
/// failure does not stop the queue. A command that does not parse is not one.
pub(crate) fn is_lookup(args: &[String]) -> bool {
    let args = without_leading_root(args);
    let Ok(cli) =
        Cli::try_parse_from(std::iter::once("rbx").chain(args.iter().map(String::as_str)))
    else {
        return false;
    };
    matches!(
        cli.command,
        Commands::ScriptSearch(_)
            | Commands::ScriptGrep(_)
            | Commands::ScriptRead(_)
            | Commands::Find(_)
            | Commands::Tree(_)
            | Commands::Inspect(_)
            | Commands::BytecodeGetProperty(_)
            | Commands::QueryPlace(_)
            | Commands::ComparePlace(_)
            | Commands::View(_)
            | Commands::ExplainPath(_)
            | Commands::ProjectValidate(_)
            | Commands::Docs(_)
            | Commands::GetConsoleOutput(_)
            | Commands::Ui(_)
            | Commands::ListClients(_)
            | Commands::RecordReview(_)
            | Commands::LiveStatus(_)
            | Commands::StudioStatus(_)
            | Commands::LinkStatus(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(args: &[&str]) -> bool {
        is_lookup(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn only_reads_count_as_lookups() {
        assert!(lookup(&["sg", "Honk", "--in", "src"]));
        assert!(lookup(&["sr", "DataHandler:1-20"]));
        assert!(lookup(&["--output-mode", "json", "tr", "Workspace"]));
        assert!(lookup(&["in", "Workspace", "Lobby.Door", "-F", "props"]));
        assert!(lookup(&["co", "--server", "--level", "error"]));
        assert!(lookup(&["-r", "places/lobby", "sr", "Main"]));
        assert!(lookup(&[
            "--daemon",
            "bench",
            "--root=places/lobby",
            "sg",
            "Honk"
        ]));
        for step in [
            &["l", "return 1"][..],
            &["l", "--server", "return 1"],
            &["lc", "return 1", "1"],
            &["ck", "A.luau"],
            &["wait", "30s"],
            &["play", "-x"],
            &["play", "-s", "--until", "true"],
            &["inp", "-p", "1", "click", "Main.Shop"],
            &["se", "A.luau", "old", "new"],
            &["bs", "Lighting", "-p", "Brightness=2", "--push"],
            &["-r", "places/lobby", "se", "A.luau", "old", "new"],
            &["sg"],
            &["no-such-command"],
        ] {
            assert!(!lookup(step), "{step:?}");
        }
    }
}
