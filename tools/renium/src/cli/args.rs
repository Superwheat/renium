use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
pub(crate) struct ImportSnapshotsArgs {
    #[arg(long, value_name = "PATH")]
    pub(crate) snapshot_dir: PathBuf,
    #[arg(long, value_name = "PATH")]
    pub(crate) project_root: PathBuf,
    #[arg(long, alias = "src", value_name = "PATH", default_value = "src")]
    pub(crate) src_dir: PathBuf,
    #[arg(long, default_value = "")]
    pub(crate) services: String,
    #[arg(long)]
    pub(crate) no_project_write: bool,
    #[arg(long, default_value_t = 0)]
    pub(crate) threads: usize,
}

#[derive(Parser)]
pub(crate) struct ImportServiceArgs {
    #[arg(long, value_name = "PATH")]
    pub(crate) project_root: PathBuf,
    #[arg(long, alias = "src", value_name = "PATH", default_value = "src")]
    pub(crate) src_dir: PathBuf,
    #[arg(long)]
    pub(crate) service: String,
    #[arg(long, value_name = "PATH")]
    pub(crate) snapshot_file: Option<PathBuf>,
    #[arg(long)]
    pub(crate) no_project_write: bool,
}

#[derive(Parser)]
#[command(
    about = "Provision a Renium project for git/GitHub: ignore + attributes policy files and a repo-local diff textconv / merge driver for the binary .renium stores"
)]
pub(crate) struct VcInitArgs {
    #[arg(
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(crate) project_root: PathBuf,
    #[arg(
        help = "Only write the policy files; skip `git init`, git config, and remotes",
        long
    )]
    pub(crate) skip_git: bool,
    #[arg(
        help = "Set the `origin` remote to this URL (added or updated)",
        long,
        value_name = "URL"
    )]
    pub(crate) remote: Option<String>,
    #[arg(long, value_name = "COMMAND", default_value = "git")]
    pub(crate) git_path: String,
    #[arg(long)]
    pub(crate) pretty: bool,
}

#[derive(Parser)]
#[command(
    about = "Render a binary .renium settings store or package as deterministic text. Wired up by `vc-init` as a git textconv so `git diff` shows real changes"
)]
pub(crate) struct VcTextconvArgs {
    #[arg(help = "The .renium file to render")]
    pub(crate) file: PathBuf,
}

#[derive(Parser)]
#[command(
    about = "Inspect a .renium store: print its instance tree as text, or as a structured JSON tree (`--json`) for the VS Code viewer. Reuses the one decoder, so a dropped file decodes exactly like a synced one"
)]
pub(crate) struct ViewArgs {
    #[arg(help = "The .renium file to inspect")]
    pub(crate) file: PathBuf,
    #[arg(
        help = "Emit a nested JSON tree (name/class/id/properties/attributes/source) instead of the human-readable text rendering",
        long
    )]
    pub(crate) json: bool,
    #[arg(long)]
    pub(crate) pretty: bool,
}

#[derive(Parser)]
#[command(
    about = "Three-way merge of .renium settings stores at the instance/property level, using stable settings ids as identity. Wired up by `vc-init` as the git merge driver for *.renium; also usable standalone with --output"
)]
pub(crate) struct VcMergeArgs {
    #[arg(help = "Common ancestor version (%O in the git merge driver)")]
    pub(crate) base: PathBuf,
    #[arg(help = "Our version (%A); receives the merge result in driver mode")]
    pub(crate) ours: PathBuf,
    #[arg(help = "Their version (%B)")]
    pub(crate) theirs: PathBuf,
    #[arg(
        help = "Repo-relative path of the file being merged (%P), for messages",
        long
    )]
    pub(crate) path: Option<String>,
    #[arg(
        help = "Write the merged store here instead of overwriting OURS",
        short,
        long,
        value_name = "PATH"
    )]
    pub(crate) output: Option<PathBuf>,
    #[arg(
        help = "Resolve conflicting edits by taking this side instead of failing",
        long,
        value_name = "ours|theirs"
    )]
    pub(crate) prefer: Option<String>,
    #[arg(long)]
    pub(crate) pretty: bool,
}

#[derive(Parser)]
pub(crate) struct GenerateSourcemapArgs {
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub(crate) project_root: PathBuf,
    #[arg(long)]
    pub(crate) project: Option<PathBuf>,
    #[arg(short, long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,
    #[arg(long)]
    pub(crate) stdout: bool,
    #[arg(long)]
    pub(crate) watch: bool,
    #[arg(long, default_value_t = 250)]
    pub(crate) interval_ms: u64,
    #[arg(long)]
    pub(crate) absolute_paths: bool,
    #[arg(long = "filter", value_name = "GLOB")]
    pub(crate) filters: Vec<String>,
}

#[derive(Parser)]
pub(crate) struct CursorPollArgs {
    #[arg(long, default_value_t = 16)]
    pub(crate) interval_ms: u64,
}
