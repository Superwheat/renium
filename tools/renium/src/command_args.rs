use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
pub(super) struct ImportSnapshotsArgs {
    #[arg(long, value_name = "PATH")]
    pub(super) snapshot_dir: PathBuf,
    #[arg(long, value_name = "PATH")]
    pub(super) project_root: PathBuf,
    #[arg(long, alias = "src", value_name = "PATH", default_value = "src")]
    pub(super) src_dir: PathBuf,
    #[arg(long, default_value = "")]
    pub(super) services: String,
    #[arg(long)]
    pub(super) no_project_write: bool,
    #[arg(long, default_value_t = 0)]
    pub(super) threads: usize,
}

#[derive(Parser)]
pub(super) struct ImportServiceArgs {
    #[arg(long, value_name = "PATH")]
    pub(super) project_root: PathBuf,
    #[arg(long, alias = "src", value_name = "PATH", default_value = "src")]
    pub(super) src_dir: PathBuf,
    #[arg(long)]
    pub(super) service: String,
    #[arg(long, value_name = "PATH")]
    pub(super) snapshot_file: Option<PathBuf>,
    #[arg(long)]
    pub(super) no_project_write: bool,
}

#[derive(Parser)]
#[command(
    about = "Provision a Renium project for git/GitHub: ignore + attributes policy files and a repo-local diff textconv / merge driver for the binary .renium stores"
)]
pub(super) struct VcInitArgs {
    #[arg(
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Only write the policy files; skip `git init`, git config, and remotes",
        long
    )]
    pub(super) skip_git: bool,
    #[arg(
        help = "Set the `origin` remote to this URL (added or updated)",
        long,
        value_name = "URL"
    )]
    pub(super) remote: Option<String>,
    #[arg(long, value_name = "COMMAND", default_value = "git")]
    pub(super) git_path: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
#[command(
    about = "Render a binary .renium settings store or package as deterministic text. Wired up by `vc-init` as a git textconv so `git diff` shows real changes"
)]
pub(super) struct VcTextconvArgs {
    #[arg(help = "The .renium file to render")]
    pub(super) file: PathBuf,
}

#[derive(Parser)]
#[command(
    about = "Inspect a .renium store: print its instance tree as text, or as a structured JSON tree (`--json`) for the VS Code viewer. Reuses the one decoder, so a dropped file decodes exactly like a synced one"
)]
pub(super) struct ViewArgs {
    #[arg(help = "The .renium file to inspect")]
    pub(super) file: PathBuf,
    #[arg(
        help = "Emit a nested JSON tree (name/class/id/properties/attributes/source) instead of the human-readable text rendering",
        long
    )]
    pub(super) json: bool,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
#[command(
    about = "Three-way merge of .renium settings stores at the instance/property level, using stable settings ids as identity. Wired up by `vc-init` as the git merge driver for *.renium; also usable standalone with --output"
)]
pub(super) struct VcMergeArgs {
    #[arg(help = "Common ancestor version (%O in the git merge driver)")]
    pub(super) base: PathBuf,
    #[arg(help = "Our version (%A); receives the merge result in driver mode")]
    pub(super) ours: PathBuf,
    #[arg(help = "Their version (%B)")]
    pub(super) theirs: PathBuf,
    #[arg(
        help = "Repo-relative path of the file being merged (%P), for messages",
        long
    )]
    pub(super) path: Option<String>,
    #[arg(
        help = "Write the merged store here instead of overwriting OURS",
        short,
        long,
        value_name = "PATH"
    )]
    pub(super) output: Option<PathBuf>,
    #[arg(
        help = "Resolve conflicting edits by taking this side instead of failing",
        long,
        value_name = "ours|theirs"
    )]
    pub(super) prefer: Option<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct GenerateSourcemapArgs {
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub(super) project_root: PathBuf,
    #[arg(long)]
    pub(super) project: Option<PathBuf>,
    #[arg(short, long, value_name = "PATH")]
    pub(super) output: Option<PathBuf>,
    #[arg(long)]
    pub(super) stdout: bool,
    #[arg(long)]
    pub(super) watch: bool,
    #[arg(long, default_value_t = 250)]
    pub(super) interval_ms: u64,
    #[arg(long)]
    pub(super) absolute_paths: bool,
    #[arg(long = "filter", value_name = "GLOB")]
    pub(super) filters: Vec<String>,
}

#[derive(Parser)]
pub(super) struct CursorPollArgs {
    #[arg(long, default_value_t = 16)]
    pub(super) interval_ms: u64,
}
