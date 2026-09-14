use std::ffi::OsString;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use serde::Deserialize;

pub(crate) mod args;
pub(crate) mod config;
pub(crate) mod dispatch;
mod examples;
pub(crate) mod performance;
pub(crate) mod syntax;

use crate::app::update;
use crate::automation::commands::{
    ImageUploadArgs, InputArgs, MultiEditArgs, PlaceAddArgs, PlaceRenameArgs, PlaceReorderArgs,
    StudioCloseArgs, StudioReopenArgs, StudioStatusArgs,
};
use crate::cli::args::{
    CursorPollArgs, GenerateSourcemapArgs, VcInitArgs, VcMergeArgs, VcTextconvArgs, ViewArgs,
};
use crate::daemon::transport::DEFAULT_DAEMON_CONTROL_PORT;
use crate::project::config as project_config;
use crate::project::workflows;

pub(crate) fn command() -> clap::Command {
    let mut command = Cli::command();
    for (name, examples) in examples::COMMAND_EXAMPLES {
        let subcommand = command
            .find_subcommand_mut(name)
            .expect("command example must name an existing command");
        *subcommand = std::mem::take(subcommand).after_help(*examples);
    }
    command.term_width(0).after_help(
        "Global options (any command):\n  --place <NAME|ID|GAME:PLACE>  Pin to one Studio place (env: RENIUM_PLACE)\n  --project <PATH>              Use this renium.project.jsonc\n  --output-mode text|json|pretty  --log-level off|error|warn|info|debug|trace  -v\n  --color auto|always|never  --yes  --backtrace  --daemon <NAME>\n\nExamples:\n  rbx f Workspace -n Door\n  rbx pl\n  rbx ps src/StarterGui/Menu.client.luau\n  rbx l \"return game.PlaceId\"\n  rbx sc --studio -o studio.png",
    )
}

#[derive(Parser)]
#[command(author, version, disable_help_subcommand = true)]
pub(super) struct Cli {
    #[arg(
        help = "Pin bridge commands to one Studio place by name, placeId, or gameId:placeId (env: RENIUM_PLACE)",
        long,
        global = true,
        hide = true,
        value_name = "NAME|ID|GAME:PLACE"
    )]
    pub(super) place: Option<String>,
    #[arg(
        long,
        global = true,
        hide = true,
        value_name = "PATH",
        help = "Use this renium.project.jsonc instead of nearest-project discovery"
    )]
    pub(super) project: Option<PathBuf>,
    #[arg(
        help = "Log verbosity",
        long,
        global = true,
        hide = true,
        value_name = "off|error|warn|info|debug|trace",
        default_value = "info"
    )]
    pub(super) log_level: String,
    #[arg(help = "Increase log verbosity", short, long, global = true, hide = true, action = ArgAction::Count)]
    pub(super) verbose: u8,
    #[arg(
        help = "Color output",
        long,
        global = true,
        hide = true,
        value_name = "auto|always|never",
        default_value = "auto"
    )]
    pub(super) color: String,
    #[arg(help = "Skip confirmation prompts", long, global = true, hide = true)]
    pub(super) yes: bool,
    #[arg(
        help = "Include a backtrace in errors",
        long,
        global = true,
        hide = true
    )]
    pub(super) backtrace: bool,
    #[arg(
        help = "Output format",
        long,
        global = true,
        hide = true,
        value_name = "text|json|pretty",
        default_value = "text"
    )]
    pub(super) output_mode: String,
    #[arg(
        long,
        global = true,
        hide = true,
        value_name = "NAME",
        help = "Use a named Renium daemon"
    )]
    pub(super) daemon: Option<String>,
    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Parser)]
pub(super) struct QueryPlaceArgs {
    #[arg(help = "Place file to search", value_name = "PLACE.rbxl|PLACE.rbxlx")]
    pub(super) input: PathBuf,
    #[arg(help = "Text to match in names", value_name = "QUERY")]
    pub(super) query: Option<String>,
    #[arg(help = "Match exact name", short, long)]
    pub(super) name: Option<String>,
    #[arg(help = "Match class name", short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(
        help = "Text to match in script sources",
        short,
        long,
        value_name = "TEXT"
    )]
    pub(super) source: Option<String>,
    #[arg(help = "Maximum matches", long, default_value = "20")]
    pub(super) limit: NonZeroUsize,
    #[arg(help = "Return every match", short, long)]
    pub(super) all: bool,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct ComparePlaceArgs {
    #[arg(
        help = "Place file holding the before state",
        value_name = "PLACE.rbxl|PLACE.rbxlx"
    )]
    pub(super) input: PathBuf,
    #[arg(
        long,
        value_name = "PLACE.rbxl|PLACE.rbxlx",
        help = "Compare against another place file instead of the project"
    )]
    pub(super) against: Option<PathBuf>,
    #[arg(
        long,
        help = "Compare all saved instances and values, not only scripts"
    )]
    pub(super) full: bool,
    #[arg(
        long,
        requires = "full",
        help = "Include before/after values and source (may contain secrets)"
    )]
    pub(super) values: bool,
    #[arg(help = "Maximum differences to return", long, default_value = "50")]
    pub(super) limit: NonZeroUsize,
    #[arg(help = "Return every difference", short, long)]
    pub(super) all: bool,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Subcommand)]
pub(super) enum Commands {
    #[command(
        name = "plugin",
        about = "Create, install and inspect plugins that add commands to Renium"
    )]
    Plugin(crate::plugins::PluginArgs),
    #[command(external_subcommand)]
    External(Vec<OsString>),
    #[command(
        name = "ck",
        alias = "check",
        about = "Check Luau syntax offline without executing code"
    )]
    CheckLuau(syntax::CheckArgs),
    #[command(
        name = "fmt",
        alias = "fmt-project",
        about = "Format renium.project.jsonc"
    )]
    FmtProject(project_config::FmtProjectArgs),
    #[command(
        name = "pv",
        alias = "project-validate",
        alias = "validate-project",
        about = "Validate project configuration offline"
    )]
    ProjectValidate(project_config::AdapterProjectArgs),
    #[command(
        name = "xp",
        alias = "explain-path",
        about = "Explain how a path maps to instances"
    )]
    ExplainPath(project_config::ExplainPathArgs),
    #[command(
        name = "cfg",
        alias = "config",
        about = "Read or change Renium settings"
    )]
    Config(project_config::ConfigArgs),
    #[command(
        name = "ad",
        alias = "adapters",
        about = "Build or sync back project adapters"
    )]
    Adapters(project_config::AdaptersArgs),
    #[command(
        name = "ir",
        alias = "import-rojo",
        about = "Convert a Rojo project to renium.project.jsonc"
    )]
    ImportRojo(project_config::ImportRojoArgs),
    #[command(
        name = "init",
        alias = "project-init",
        about = "Create a Renium project here"
    )]
    Init(workflows::InitArgs),
    #[command(
        name = "build",
        alias = "build-project",
        about = "Build a place file from the project"
    )]
    Build(workflows::BuildArgs),
    #[command(
        name = "q",
        alias = "query-place",
        alias = "place-find",
        about = "Search a place file without Studio"
    )]
    QueryPlace(QueryPlaceArgs),
    #[command(
        name = "cmp",
        alias = "compare-place",
        about = "Compare saved places or a place with this project; --full includes instances and properties"
    )]
    ComparePlace(ComparePlaceArgs),
    #[command(
        name = "dr",
        alias = "doctor",
        about = "Check the installation and project"
    )]
    Doctor(workflows::DoctorArgs),
    #[command(
        name = "docs",
        alias = "open-docs",
        about = "Print a documentation topic"
    )]
    Docs(workflows::DocsArgs),
    #[command(name = "dm", alias = "daemon", about = "Manage Renium daemons")]
    Daemon(workflows::DaemonArgs),
    #[command(
        name = "so",
        alias = "studio",
        alias = "open-studio",
        about = "Open a place in Studio"
    )]
    Studio(workflows::StudioArgs),
    #[command(
        name = "ro",
        alias = "studio-open",
        alias = "reopen-studio",
        about = "Reopen the remembered place in Studio"
    )]
    StudioReopen(StudioReopenArgs),
    #[command(
        name = "sx",
        alias = "studio-close",
        alias = "close-studio",
        about = "Close Studio"
    )]
    StudioClose(StudioCloseArgs),
    #[command(
        name = "status",
        alias = "studio-status",
        about = "Show Studio connection and play state"
    )]
    StudioStatus(StudioStatusArgs),
    #[command(name = "upd", alias = "update", about = "Update Renium")]
    Update(update::UpdateArgs),
    #[command(
        name = "oc",
        alias = "cloud",
        alias = "opencloud",
        about = "Call Roblox Open Cloud APIs"
    )]
    OpenCloud(crate::cloud::command::OpenCloudArgs),
    #[command(hide = true)]
    UpdateHelper(update::UpdateHelperArgs),
    #[command(
        name = "ip",
        alias = "import-path",
        about = "Import a script file or directory into the project"
    )]
    ImportPath(ImportPathArgs),
    #[command(name = "cr", alias = "create", about = "Create an instance")]
    Create(CreateInstanceArgs),
    #[command(name = "cp", alias = "clone", about = "Clone an instance")]
    Clone(CloneInstanceCommandArgs),
    #[command(
        name = "mv",
        alias = "move",
        about = "Move an instance to another parent or service"
    )]
    Move(MoveInstanceArgs),
    #[command(name = "rn", alias = "rename", about = "Rename an instance")]
    Rename(RenameInstanceArgs),
    #[command(name = "rm", alias = "remove", about = "Remove an instance")]
    Remove(RemoveInstanceCommandArgs),
    #[command(
        name = "upl",
        visible_alias = "unlink-package-link",
        aliases = ["dpl", "desync-package-link"]
    )]
    DesyncPackageLink(DesyncPackageLinkCommandArgs),
    #[command(
        name = "pd",
        alias = "package-desync",
        about = "Mark a Roblox package Changed"
    )]
    PackageDesync(PackageActionArgs),
    #[command(
        name = "pp",
        alias = "package-publish",
        about = "Publish a Roblox package (needs user authorization)"
    )]
    PackagePublish(PackageActionArgs),
    #[command(
        name = "pu",
        alias = "package-update",
        alias = "package-revert",
        about = "Revert a Roblox package to its published version"
    )]
    PackageUpdate(PackageActionArgs),
    #[command(
        name = "mip",
        alias = "import-model",
        about = "Import a model file under a parent"
    )]
    ImportModel(ImportModelCommandArgs),
    #[command(
        name = "mep",
        alias = "export-model",
        about = "Export an instance subtree as a model file"
    )]
    ExportModel(ExportModelCommandArgs),
    #[command(name = "pl", alias = "pull", about = "Pull Studio into project files")]
    Pull(PullArgs),
    #[command(
        name = "pi",
        alias = "place-import",
        alias = "import-place",
        about = "Import a saved place file into the project without Studio"
    )]
    ImportPlace(ImportPlaceArgs),
    #[command(
        name = "bd",
        alias = "bridge-daemon",
        about = "Run the bridge daemon",
        hide = true
    )]
    BridgeDaemon(BridgeDaemonArgs),
    #[command(
        name = "ed",
        alias = "explorer-daemon",
        about = "Run the explorer daemon for the editor",
        hide = true
    )]
    ExplorerDaemon(ExplorerDaemonArgs),
    #[command(
        name = "co",
        alias = "get-console-output",
        alias = "console",
        about = "Read Studio or play console output"
    )]
    GetConsoleOutput(PluginConsoleOutputArgs),
    #[command(
        name = "l",
        alias = "lx",
        alias = "execute-luau",
        alias = "luau",
        about = "Run Luau in Studio (Edit, or the server during Play)"
    )]
    ExecuteLuau(ExecuteLuauArgs),
    #[command(
        name = "lc",
        alias = "execute-client-luau",
        about = "Run Luau on a play client"
    )]
    ExecuteClientLuau(ExecuteClientLuauArgs),
    #[command(
        name = "dev",
        alias = "device",
        alias = "studio-device",
        about = "Control Studio's built-in device simulator"
    )]
    StudioDevice(StudioDeviceArgs),
    #[command(
        name = "net",
        alias = "network",
        about = "Inspect or change Studio network simulation, including a live play client"
    )]
    NetworkSimulation(crate::studio::automation::network::NetworkArgs),
    #[command(
        name = "access",
        about = "Control access to security-protected Studio properties"
    )]
    PropertyAccess(crate::studio::automation::property_access::PropertyAccessArgs),
    #[command(
        name = "perf",
        about = "Read runtime performance or capture frame, memory and network measurements"
    )]
    PerformanceMonitor(crate::studio::automation::monitor::MonitorArgs),
    #[command(
        name = "pf",
        alias = "performance-profile",
        alias = "performance",
        about = "Constrain Studio resources for performance testing"
    )]
    PerformanceProfile(performance::PerformanceArgs),
    #[command(hide = true)]
    PerformanceWorker,
    #[command(hide = true)]
    PerformanceHolder(performance::PerformanceHolderArgs),
    #[command(
        name = "as",
        alias = "asset-search",
        about = "Search the Creator Store"
    )]
    AssetSearch(AssetSearchArgs),
    #[command(
        name = "ai",
        alias = "asset-insert",
        about = "Insert a Creator Store asset"
    )]
    AssetInsert(AssetInsertArgs),
    #[command(name = "gm", alias = "generate-model", about = "Generate a model")]
    GenerateModel(GenerateModelArgs),
    #[command(name = "js", alias = "job-status", about = "Read a creator job")]
    JobStatus(JobStatusArgs),
    #[command(name = "iu", alias = "image-upload", about = "Upload images to Roblox")]
    ImageUpload(ImageUploadArgs),
    #[command(
        name = "ss",
        alias = "script-search",
        about = "Find scripts containing keywords"
    )]
    ScriptSearch(ScriptSearchArgs),
    #[command(name = "sg", alias = "script-grep", about = "Find text in scripts")]
    ScriptGrep(ScriptGrepArgs),
    #[command(name = "sr", alias = "script-read", about = "Read a script")]
    ScriptRead(ScriptReadArgs),
    #[command(
        name = "play",
        alias = "playtest",
        alias = "start-stop-play",
        about = "Start or stop a play session"
    )]
    StartStopPlay(StartStopPlayArgs),
    #[command(
        name = "cs",
        alias = "clients",
        alias = "studios",
        alias = "list-clients"
    )]
    ListClients(ListClientsArgs),
    #[command(
        name = "rv",
        alias = "review",
        alias = "editor-review-decision",
        about = "Decide a pending push review"
    )]
    EditorReviewDecision(EditorReviewDecisionArgs),
    #[command(
        name = "pr",
        alias = "press",
        about = "Press a GUI element in a play client"
    )]
    Press(PressArgs),
    #[command(
        name = "clk",
        alias = "click",
        about = "Click viewport coordinates in a play client"
    )]
    Click(ClickArgs),
    #[command(name = "ky", alias = "key", about = "Press a key in a play client")]
    Key(KeyArgs),
    #[command(
        name = "ui",
        alias = "user-interface",
        about = "List visible GUI elements in a play client"
    )]
    Ui(UiArgs),
    #[command(name = "ty", alias = "type", about = "Type text into a text box")]
    Type(TypeArgs),
    #[command(
        name = "wait",
        alias = "wait-until",
        about = "Wait until a Luau expression is true"
    )]
    WaitUntil(WaitUntilArgs),
    #[command(
        name = "go",
        alias = "goto",
        about = "Walk the character to a part or position"
    )]
    Goto(GotoArgs),
    #[command(
        name = "sc",
        alias = "shot",
        alias = "screenshot",
        about = "Screenshot Studio or a play client"
    )]
    Shot(ShotArgs),
    #[command(
        name = "inp",
        alias = "input",
        about = "Run a sequence of input actions"
    )]
    Input(InputArgs),
    #[command(
        name = "rs",
        alias = "record-start",
        about = "Start recording a window"
    )]
    RecordStart(RecordStartArgs),
    #[command(
        name = "re",
        alias = "record-end",
        about = "Stop a recording and render its review image"
    )]
    RecordEnd(RecordEndArgs),
    /// Inspect a saved recording as a timestamped image, without Studio.
    #[command(
        name = "rf",
        alias = "record-review",
        alias = "record-frames",
        about = "Review a recording as frame images"
    )]
    RecordReview(RecordReviewArgs),
    #[command(
        name = "setup",
        alias = "setup-renium",
        about = "Install or repair the CLI, plugin and PATH"
    )]
    Setup(SetupArgs),
    #[command(
        name = "st",
        alias = "studio-change-state",
        about = "Control Studio change tracking (low level)"
    )]
    StudioChangeState(StudioChangeStateArgs),
    #[command(name = "lon", alias = "live-start", about = "Start Live Sync")]
    LiveStart(StudioChangeStateArgs),
    #[command(name = "lof", alias = "live-stop", about = "Stop Live Sync")]
    LiveStop(StudioChangeStateArgs),
    #[command(name = "lst", alias = "live-status", about = "Show Live Sync status")]
    LiveStatus(StudioChangeStateArgs),
    #[command(
        name = "rp",
        alias = "retry-pending",
        about = "Retry pending Live Sync edits"
    )]
    RetryPending(StudioChangeStateArgs),
    #[command(
        name = "dp",
        alias = "discard-pending",
        about = "Discard pending Live Sync edits"
    )]
    DiscardPending(StudioChangeStateArgs),
    #[command(
        name = "ps",
        alias = "push",
        alias = "push-editor-changes",
        about = "Push project files to Studio"
    )]
    PushEditorChanges(PushEditorChangesArgs),
    #[command(
        name = "prop",
        alias = "apply-editor-property",
        about = "Apply a property change to Studio"
    )]
    ApplyEditorProperty(ApplyEditorPropertyArgs),
    #[command(
        name = "del",
        alias = "apply-editor-delete",
        about = "Delete an instance in Studio"
    )]
    ApplyEditorDelete(ApplyEditorDeleteArgs),
    #[command(
        name = "rev",
        alias = "editor-revert",
        about = "Restore files from sync history"
    )]
    EditorRevert(EditorRevertArgs),
    #[command(
        name = "me",
        alias = "multi-edit",
        about = "Replace text in a script file"
    )]
    MultiEdit(MultiEditArgs),
    #[command(name = "f", alias = "find", about = "Find instances in saved data")]
    Find(FindArgs),
    #[command(name = "tr", alias = "tree", about = "Show an instance subtree")]
    Tree(TreeArgs),
    #[command(name = "in", alias = "inspect", about = "Inspect one instance")]
    Inspect(InspectArgs),
    #[command(
        name = "bg",
        alias = "bytecode-get-property",
        alias = "get-property",
        about = "Read a stored property or attribute"
    )]
    BytecodeGetProperty(BytecodeGetPropertyArgs),
    #[command(
        name = "bs",
        alias = "bytecode-set-property",
        alias = "set-property",
        about = "Set a stored property or attribute"
    )]
    BytecodeSetProperty(BytecodeSetPropertyArgs),
    #[command(hide = true)]
    BytecodeApplyPropertyBatch(BytecodeApplyPropertyBatchArgs),
    #[command(
        name = "bss",
        alias = "bytecode-set-source",
        alias = "set-source",
        about = "Set a script source through the store"
    )]
    BytecodeSetSource(BytecodeSetSourceArgs),
    #[command(
        name = "bb",
        alias = "bytecode-explorer-batch",
        alias = "batch",
        about = "Run batched store queries"
    )]
    BytecodeExplorerBatch(BytecodeExplorerBatchArgs),
    #[command(
        name = "ba",
        alias = "bytecode-add-instance",
        alias = "add",
        about = "Add an instance to a store"
    )]
    BytecodeAddInstance(BytecodeAddInstanceArgs),
    #[command(
        name = "bcl",
        alias = "bytecode-clone-instance",
        about = "Clone an instance in a store"
    )]
    BytecodeCloneInstance(BytecodeCloneInstanceArgs),
    #[command(
        name = "br",
        alias = "bytecode-remove-instance",
        about = "Remove an instance from a store"
    )]
    BytecodeRemoveInstance(BytecodeRemoveInstanceArgs),
    #[command(
        name = "bem",
        alias = "bytecode-export-model",
        about = "Export a store subtree as a model file"
    )]
    BytecodeExportModel(BytecodeExportModelArgs),
    #[command(
        name = "bep",
        alias = "bytecode-export-place",
        alias = "export-place",
        about = "Build a place file from the stores"
    )]
    BytecodeExportPlace(BytecodeExportPlaceArgs),
    #[command(
        name = "bim",
        alias = "bytecode-import-model",
        about = "Import a model file into a store"
    )]
    BytecodeImportModel(BytecodeImportModelArgs),
    #[command(
        name = "wally",
        alias = "sync-wally-packages",
        about = "Install Wally packages into the project"
    )]
    SyncWallyPackages(SyncWallyPackagesArgs),
    #[command(name = "lk", alias = "link-apply", about = "Apply link packages")]
    LinkApply(LinkApplyArgs),
    #[command(name = "lkb", alias = "link-break", about = "Detach a link target")]
    LinkBreak(LinkBreakArgs),
    #[command(name = "lks", alias = "link-status", about = "Show link status")]
    LinkStatus(LinkStatusArgs),
    #[command(name = "lka", alias = "link-add", about = "Add a link target")]
    LinkAdd(LinkAddArgs),
    #[command(
        name = "lkm",
        alias = "link-move-target",
        hide = true,
        about = "Move a link target"
    )]
    LinkMoveTarget(LinkMoveTargetArgs),
    #[command(
        name = "lkp",
        alias = "link-pack",
        about = "Pack an instance subtree as a reusable link package"
    )]
    LinkPack(LinkPackArgs),
    #[command(
        name = "lkd",
        alias = "link-delete-package",
        about = "Delete a link package and handle its existing uses"
    )]
    LinkDeletePackage(LinkDeletePackageArgs),
    #[command(
        name = "bpack",
        alias = "bytecode-repack",
        about = "Upgrade old stores"
    )]
    BytecodeRepack(BytecodeRepackArgs),
    #[command(
        name = "sm",
        alias = "generate-sourcemap",
        alias = "sourcemap",
        about = "Generate or query the sourcemap"
    )]
    GenerateSourcemap(GenerateSourcemapArgs),
    #[command(
        name = "vci",
        alias = "vc-init",
        about = "Configure Git for .renium files"
    )]
    VcInit(VcInitArgs),
    #[command(
        name = "vct",
        alias = "vc-textconv",
        about = "Render a .renium file as text"
    )]
    VcTextconv(VcTextconvArgs),
    #[command(name = "v", alias = "view", about = "View a .renium or model file")]
    View(ViewArgs),
    #[command(name = "vcm", alias = "vc-merge", about = "Merge .renium files")]
    VcMerge(VcMergeArgs),
    #[command(
        name = "cpoll",
        alias = "cursor-poll",
        hide = true,
        about = "Poll cursor state for the editor",
        hide = true
    )]
    CursorPoll(CursorPollArgs),
    #[command(
        name = "pa",
        alias = "place-add",
        about = "Add a place to the experience"
    )]
    PlaceAdd(PlaceAddArgs),
    #[command(name = "pn", alias = "place-rename", about = "Set a place alias")]
    PlaceRename(PlaceRenameArgs),
    #[command(name = "po", alias = "place-reorder", about = "Reorder places")]
    PlaceReorder(PlaceReorderArgs),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_visible_command_has_a_short_example() {
        command().debug_assert();
        for subcommand in command()
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
        {
            let examples = subcommand.clone().render_help().to_string();
            assert!(
                examples.contains(&format!("rbx {}", subcommand.get_name())),
                "{} does not use its short name in examples",
                subcommand.get_name()
            );
        }
    }
}

#[derive(Clone, clap::Args)]
pub(super) struct BridgeConnectionArgs {
    #[arg(
        help = "Seconds to wait for a Studio connection",
        short,
        long,
        default_value_t = 8.0
    )]
    pub(super) wait_seconds: f64,
    #[arg(
        help = "Studio bridge ports to try",
        short = 'P',
        long,
        default_value = "8781,8782"
    )]
    pub(super) ports: String,
}

impl BridgeConnectionArgs {
    pub(super) fn local(wait_seconds: f64) -> Self {
        Self {
            wait_seconds,
            ports: "8781,8782".to_string(),
        }
    }
}

#[derive(Parser)]
pub(super) struct BridgeDaemonArgs {
    #[arg(help = "Daemon name", long)]
    pub(super) name: Option<String>,
    #[arg(long = "serve", alias = "keep-alive", hide = true)]
    pub(super) _serve: bool,
    #[arg(help = "Bind address", short = 'H', long, default_value = "127.0.0.1")]
    pub(super) host: String,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Daemon control port", long, alias = "ctl-port", default_value_t = DEFAULT_DAEMON_CONTROL_PORT)]
    pub(super) control_port: u16,
    #[arg(
        long,
        help = "Proxy editor requests to the shared daemon over JSON stdin"
    )]
    pub(super) editor_stdio: bool,
    #[arg(
        help = "Exit the editor proxy when this process dies",
        long,
        value_name = "PID"
    )]
    pub(super) parent_pid: Option<u32>,
}

#[derive(Parser)]
pub(super) struct ExplorerDaemonArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Services to watch (comma-separated)",
        short,
        long,
        default_value = ""
    )]
    pub(super) services: String,
    #[arg(
        help = "Exit automatically when this process dies, even if stdin stays open (prevents orphaned explorer daemons when the editor crashes)",
        long,
        value_name = "PID"
    )]
    pub(super) parent_pid: Option<u32>,
}

#[derive(Args, Clone)]
pub(super) struct ProjectSourceArgs {
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Script source directory",
        short = 'd',
        long = "src",
        alias = "src-dir",
        value_name = "PATH",
        default_value = "src"
    )]
    pub(super) src_root: PathBuf,
}

#[derive(Parser)]
pub(super) struct FindArgs {
    #[arg(help = "Service to search, or text when --service is given")]
    pub(super) query_or_service: Option<String>,
    #[arg(help = "Text to search for")]
    pub(super) query: Option<String>,
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(help = "Service to search", short, long)]
    pub(super) service: Option<String>,
    #[arg(help = "Match exact name", short, long)]
    pub(super) name: Option<String>,
    #[arg(help = "Match class name", short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(
        help = "Search only this parent's subtree",
        short = 'I',
        long,
        alias = "parent-id"
    )]
    pub(super) parent_settings_id: Option<String>,
    #[arg(help = "Match instances with this tag", short, long)]
    pub(super) tag: Option<String>,
    #[arg(
        help = "Property filter NAME=JSON (repeatable)",
        short,
        long = "property"
    )]
    pub(super) properties: Vec<String>,
    #[arg(
        help = "Attribute filter NAME=JSON (repeatable)",
        short,
        long = "attribute"
    )]
    pub(super) attributes: Vec<String>,
    #[arg(help = "Return every match", long)]
    pub(super) all: bool,
    #[arg(help = "Maximum matches", short, long, default_value_t = 20)]
    pub(super) limit: usize,
    #[arg(
        help = "Detail level: compact, summary, detail or full",
        short,
        long,
        default_value = "compact"
    )]
    pub(super) output: String,
    #[arg(
        help = "Fields or preset to return",
        short = 'F',
        long,
        default_value = "lookup,ords"
    )]
    pub(super) fields: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct HighLevelTargetArgs {
    #[arg(help = "Service name, or the target when the service is given with --service")]
    pub(super) service_or_target: Option<String>,
    #[arg(help = "Target name or dotted path")]
    pub(super) target: Option<String>,
    #[arg(help = "Service that owns the target", short, long)]
    pub(super) service: Option<String>,
    #[arg(help = "Select by settings ID", short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[arg(help = "Select by store index", short = 'x', long)]
    pub(super) index: Option<usize>,
    #[arg(help = "Select by exact name", short, long)]
    pub(super) name: Option<String>,
    #[arg(help = "Select by class name", short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(
        help = "Target path, dotted or as a JSON string array",
        long,
        alias = "path-json",
        alias = "path-segments",
        alias = "path-segments-json"
    )]
    pub(super) path: Option<String>,
    #[arg(
        help = "Sibling ordinals (JSON array) for duplicate names",
        long,
        alias = "path-ordinals",
        alias = "path-ordinals-json"
    )]
    pub(super) ords: Option<String>,
}

#[derive(Parser)]
pub(super) struct TreeArgs {
    #[command(flatten)]
    pub(super) target: HighLevelTargetArgs,
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(help = "Levels of children to include", long, default_value_t = 1)]
    pub(super) depth: usize,
    #[arg(help = "Maximum nodes", short, long)]
    pub(super) limit: Option<usize>,
    #[arg(
        help = "Detail level: compact, summary, detail or full",
        short,
        long,
        default_value = "compact"
    )]
    pub(super) output: String,
    #[arg(
        help = "Fields or preset to return",
        short = 'F',
        long,
        default_value = "tree,ords"
    )]
    pub(super) fields: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct InspectArgs {
    #[command(flatten)]
    pub(super) target: HighLevelTargetArgs,
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Detail level: compact, summary, detail or full",
        short,
        long,
        default_value = "compact"
    )]
    pub(super) output: String,
    #[arg(
        help = "Fields or preset to return",
        short = 'F',
        long,
        default_value = "brief,ords"
    )]
    pub(super) fields: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct PluginConsoleOutputArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Maximum entries", short = 'n', long, default_value_t = 200)]
    pub(super) limit: usize,
    #[arg(
        help = "Only entries after this sequence number",
        short,
        long,
        alias = "since",
        default_value_t = 0
    )]
    pub(super) since_seq: u64,
    #[arg(long, hide = true)]
    pub(super) from_oldest: bool,
    #[arg(help = "Clear the buffer after reading", short, long)]
    pub(super) clear: bool,
    #[arg(help = "Read a play client console", long)]
    pub(super) client: bool,
    #[arg(help = "Read the play server console", long, conflicts_with_all = ["client", "player"])]
    pub(super) server: bool,
    #[arg(help = "Play client by name or index", long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(help = "Keep streaming new entries", short, long)]
    pub(super) follow: bool,
    #[arg(help = "Only entries containing TEXT", long, value_name = "TEXT")]
    pub(super) grep: Option<String>,
    #[arg(help = "Only entries of this message type", long, value_name = "TYPE")]
    pub(super) level: Option<String>,
    #[arg(help = "Poll interval while following", long, default_value_t = 200)]
    pub(super) interval_ms: u64,
}

#[derive(Parser)]
pub(super) struct ExecuteLuauArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Luau code (- for stdin)", short = 'e', long)]
    pub(super) code: Option<String>,
    #[arg(help = "Luau code (- for stdin)", value_name = "LUAU", conflicts_with_all = ["code", "file"])]
    pub(super) inline_code: Option<String>,
    #[arg(help = "Luau file to run", short, long, value_name = "PATH")]
    pub(super) file: Option<PathBuf>,
    #[arg(help = "Run on a play client", short, long)]
    pub(super) client: bool,
    #[arg(help = "Play client by name or index", long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(
        help = "Seconds before the run is cancelled",
        short,
        long,
        default_value_t = 10.0
    )]
    pub(super) timeout: f64,
}

#[derive(Parser)]
pub(super) struct ExecuteClientLuauArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Luau code (- for stdin)")]
    pub(super) code: String,
    #[arg(help = "Play client by name or index")]
    pub(super) player: Option<String>,
    #[arg(
        help = "Seconds before the run is cancelled",
        short,
        long,
        default_value_t = 10.0
    )]
    pub(super) timeout: f64,
}

#[derive(Parser)]
pub(super) struct StudioDeviceArgs {
    #[arg(help = "list, set, status or stop", default_value = "status",
        value_parser = ["list", "status", "set", "stop"]
    )]
    pub(super) action: String,
    #[arg(help = "Catalog name or stable device id")]
    pub(super) device: Option<String>,
    #[arg(
        long,
        help = "portrait, landscape, landscape-left, landscape-right, landscape-sensor, or sensor"
    )]
    pub(super) orientation: Option<String>,
    #[arg(
        long = "scaling",
        alias = "scaling-mode",
        value_name = "MODE",
        help = "physical, actual, or fit"
    )]
    pub(super) scaling_mode: Option<String>,
    #[arg(
        long,
        value_name = "WIDTHxHEIGHT",
        help = "Override the simulated resolution"
    )]
    pub(super) resolution: Option<String>,
    #[arg(
        long,
        alias = "density",
        value_name = "DENSITY",
        help = "Override pixels per inch"
    )]
    pub(super) pixel_density: Option<f64>,
    #[arg(long, help = "Include dimensions and density in device listings")]
    pub(super) details: bool,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct AssetSearchArgs {
    #[arg(help = "Search text")]
    pub(super) query: String,
    #[arg(help = "Maximum results", short, long, default_value_t = 5)]
    pub(super) limit: u32,
    #[arg(help = "Asset type to search", long = "type", default_value = "Model")]
    pub(super) asset_type: String,
    #[arg(help = "Continue from a previous result cursor", long)]
    pub(super) cursor: Option<String>,
    #[arg(help = "Include creator, description and price", long)]
    pub(super) details: bool,
}

#[derive(Parser)]
pub(super) struct AssetInsertArgs {
    #[arg(help = "Creator Store asset ID")]
    pub(super) asset_id: NonZeroU64,
    #[arg(help = "Parent path in Studio", long, default_value = "Workspace")]
    pub(super) parent: String,
    #[arg(help = "Name for the inserted instance", long)]
    pub(super) name: Option<String>,
    #[arg(help = "Asset type (Model, Decal, Audio, ...)", long = "type")]
    pub(super) asset_type: Option<String>,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct GenerateModelArgs {
    #[arg(help = "Description of the model to generate")]
    pub(super) prompt: Option<String>,
    #[arg(help = "Reference image asset ID", long)]
    pub(super) image_asset_id: Option<NonZeroU64>,
    #[arg(help = "Parent path in Studio", long, default_value = "Workspace")]
    pub(super) parent: String,
    #[arg(
        help = "Name of the generated model",
        long,
        default_value = "GeneratedModel"
    )]
    pub(super) name: String,
    #[arg(help = "Target size", long, value_name = "X,Y,Z")]
    pub(super) size: Option<String>,
    #[arg(help = "Triangle budget", long)]
    pub(super) max_triangles: Option<u32>,
    #[arg(help = "Generate textures", long)]
    pub(super) generate_textures: Option<bool>,
    #[arg(help = "Part description (repeatable)", long = "part")]
    pub(super) parts: Vec<String>,
    #[arg(help = "Segmentation mode", long)]
    pub(super) segmentation: Option<String>,
    #[arg(help = "Leave parts unanchored", long)]
    pub(super) unanchored: bool,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct JobStatusArgs {
    #[arg(help = "Creator job ID")]
    pub(super) job_id: String,
    #[arg(help = "Seconds to wait for completion", long, default_value_t = 0.0)]
    pub(super) wait_seconds: f64,
}

#[derive(Parser)]
pub(super) struct ScriptSearchArgs {
    #[arg(help = "Keywords every script must contain", required = true, num_args = 1..)]
    pub(super) keywords: Vec<String>,
    #[arg(help = "Maximum scripts", short, long, default_value_t = 25)]
    pub(super) limit: usize,
}

#[derive(Parser)]
pub(super) struct ScriptGrepArgs {
    #[arg(help = "Text to find")]
    pub(super) query: String,
    #[arg(help = "Ignore case", short = 'i', long)]
    pub(super) case_insensitive: bool,
    #[arg(help = "Maximum matching lines", short, long, default_value_t = 100)]
    pub(super) limit: usize,
}

#[derive(Parser)]
pub(super) struct ScriptReadArgs {
    #[arg(help = "Script file path")]
    pub(super) path: PathBuf,
    #[arg(help = "First line to read", long, default_value_t = 1)]
    pub(super) start_line: usize,
    #[arg(help = "Last line to read", long)]
    pub(super) end_line: Option<usize>,
}

#[derive(Parser)]
pub(super) struct StartStopPlayArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Start a session", short, long)]
    pub(super) start: bool,
    #[arg(help = "Stop the session", short = 'x', long)]
    pub(super) stop: bool,
    #[arg(help = "Launch a server and N clients", short, long, value_name = "N")]
    pub(super) players: Option<u32>,
    #[arg(help = "Session kind", long, value_name = "play|run|server")]
    pub(super) mode: Option<String>,
}

#[derive(Parser)]
pub(super) struct ImportPathArgs {
    #[arg(help = "Script file or directory to import (not a place or model file)")]
    pub(super) source: PathBuf,
    #[arg(
        help = "Project path for a directory import",
        long,
        value_name = "PATH"
    )]
    pub(super) destination: Option<PathBuf>,
    #[arg(
        help = "Roblox path for a script import (JSON string array)",
        long,
        value_name = "[\"Service\",\"Parent\",\"Name\"]",
        conflicts_with = "destination"
    )]
    pub(super) path_json: Option<String>,
    #[arg(help = "Use this renium.project.jsonc", long)]
    pub(super) project: Option<PathBuf>,
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(help = "Preview without writing", long)]
    pub(super) dry_run: bool,
    #[arg(help = "Push the imported files to Studio", long)]
    pub(super) push: bool,
}

#[derive(Parser)]
pub(super) struct CreateInstanceArgs {
    #[arg(help = "Service that receives the instance")]
    pub(super) service: String,
    #[arg(help = "Class name of the new instance", short, long, alias = "class")]
    pub(super) class_name: String,
    #[arg(help = "Name of the new instance", short, long)]
    pub(super) name: String,
    #[arg(
        help = "Parent settings ID (default: service root)",
        short = 'I',
        long,
        alias = "parent-id"
    )]
    pub(super) parent_settings_id: Option<String>,
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(help = "Script source directory", short = 'd', long)]
    pub(super) src_root: Option<PathBuf>,
    #[arg(help = "Property as NAME=JSON (repeatable)", short, long = "property")]
    pub(super) properties: Vec<String>,
    #[arg(
        help = "Attribute as NAME=JSON (repeatable)",
        short,
        long = "attribute"
    )]
    pub(super) attributes: Vec<String>,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Args)]
pub(super) struct ProjectInstanceArgs {
    #[arg(help = "Service that owns the target")]
    pub(super) service: String,
    #[arg(help = "Target settings ID", short = 'i', long, alias = "id")]
    pub(super) settings_id: String,
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
}

#[derive(Parser)]
pub(super) struct CloneInstanceCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(
        help = "Destination parent settings ID",
        short = 'I',
        long,
        alias = "parent-id"
    )]
    pub(super) parent_settings_id: String,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct MoveInstanceArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(help = "Move into this service", long = "to-service")]
    pub(super) target_service: Option<String>,
    #[arg(
        help = "New parent settings ID",
        short = 'I',
        long,
        alias = "parent-id"
    )]
    pub(super) parent_settings_id: Option<String>,
    #[arg(help = "Script source directory", short = 'd', long)]
    pub(super) src_root: Option<PathBuf>,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct RenameInstanceArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(help = "New name")]
    pub(super) name: String,
    #[arg(help = "Script source directory", short = 'd', long)]
    pub(super) src_root: Option<PathBuf>,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct RemoveInstanceCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(help = "Keep descendants", short = 'R', long)]
    pub(super) no_recursive: bool,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct DesyncPackageLinkCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct PackageActionArgs {
    #[arg(help = "Package root path; use a JSON string array when names contain dots")]
    pub(super) target: String,
    #[arg(
        long,
        value_delimiter = ',',
        help = "One-based ordinal for each path segment"
    )]
    pub(super) ords: Vec<usize>,
    #[arg(long, help = "Target Studio process when more than one is connected")]
    pub(super) pid: Option<u32>,
    #[arg(
        help = "Seconds to wait for the package update",
        long,
        default_value_t = 20.0
    )]
    pub(super) timeout: f64,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct ImportModelCommandArgs {
    #[arg(help = "Service that owns the parent")]
    pub(super) service: String,
    #[arg(help = "Parent settings ID", short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: String,
    #[arg(help = "Model file to import", short, long, value_name = "PATH")]
    pub(super) model: PathBuf,
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct ExportModelCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(help = "Model file to write", short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(help = "Output format", long, value_name = "rbxm|rbxmx")]
    pub(super) format: Option<String>,
}

#[derive(Parser)]
pub(super) struct ListClientsArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct EditorReviewDecisionArgs {
    #[arg(default_value = "apply", value_parser = ["apply", "skip"])]
    pub(super) decision: String,
    #[arg(help = "Review to decide", short = 'i', long)]
    pub(super) review_id: Option<String>,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct PressArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "GUI path from ui",
        value_name = "GUI_PATH",
        required_unless_present = "id"
    )]
    pub(super) path: Option<String>,
    #[arg(help = "GUI element ID from ui", short, long)]
    pub(super) id: Option<String>,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Right-click", long)]
    pub(super) right: bool,
    #[arg(help = "Click a world part path instead of a GUI element", long)]
    pub(super) world: bool,
    #[arg(
        help = "Milliseconds to hold the button",
        long,
        alias = "hold-ms",
        value_name = "MS",
        default_value_t = 30
    )]
    pub(super) hold: u64,
}

#[derive(Parser)]
pub(super) struct ClickArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Viewport X coordinate")]
    pub(super) x: i32,
    #[arg(help = "Viewport Y coordinate")]
    pub(super) y: i32,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Right-click", long)]
    pub(super) right: bool,
    #[arg(
        help = "Milliseconds to hold the button",
        long,
        alias = "hold-ms",
        value_name = "MS",
        default_value_t = 30
    )]
    pub(super) hold: u64,
}

#[derive(Parser)]
pub(super) struct KeyArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Key code name, such as E or Space")]
    pub(super) key: String,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(
        help = "Milliseconds to hold the key",
        long,
        value_name = "MS",
        default_value_t = 60
    )]
    pub(super) hold_ms: u64,
}

#[derive(Parser)]
pub(super) struct UiArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Maximum elements", short = 'n', long, default_value_t = 200)]
    pub(super) limit: usize,
    #[arg(help = "Include off-screen elements", long, alias = "all")]
    pub(super) include_offscreen: bool,
}

#[derive(Parser)]
pub(super) struct SetupArgs {
    #[arg(
        help = "Install the Studio plugin from this .rbxm file instead of downloading",
        long,
        value_name = "PATH"
    )]
    pub(super) file: Option<String>,
    #[arg(
        help = "Roblox Plugins directory override (default: the local Studio Plugins folder)",
        long
    )]
    pub(super) dir: Option<String>,
    #[arg(
        help = "Only download/copy without installing; print where the plugin would go",
        long
    )]
    pub(super) dry_run: bool,
    #[arg(help = "Report the installation state", long)]
    pub(super) status: bool,
    #[arg(help = "Reinstall the plugin and PATH entries", long)]
    pub(super) repair: bool,
    #[arg(help = "Remove the installation", long)]
    pub(super) uninstall: bool,
}

#[derive(Parser)]
pub(super) struct TypeArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Text to type")]
    pub(super) text: String,
    #[arg(help = "Text box path from ui", long, value_name = "GUI_PATH")]
    pub(super) path: Option<String>,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Press Enter after typing", long)]
    pub(super) enter: bool,
}

#[derive(Parser)]
pub(super) struct WaitUntilArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "Luau expression that must become true",
        value_name = "LUAU_CONDITION"
    )]
    pub(super) condition: String,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Evaluate on a play client", short, long)]
    pub(super) client: bool,
    #[arg(help = "Seconds before giving up", short, long, default_value_t = 10.0)]
    pub(super) timeout: f64,
    #[arg(help = "Seconds between checks", long, default_value_t = 0.25)]
    pub(super) interval: f64,
}

#[derive(Parser)]
pub(super) struct GotoArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "Part path to walk to",
        value_name = "PART_PATH",
        required_unless_present = "pos"
    )]
    pub(super) target: Option<String>,
    #[arg(help = "World position to walk to", long, value_name = "X,Y,Z")]
    pub(super) pos: Option<String>,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Teleport instead of walking", long)]
    pub(super) tp: bool,
    #[arg(help = "Seconds before giving up", short, long, default_value_t = 30.0)]
    pub(super) timeout: f64,
    #[arg(help = "Walk speed multiplier", long, default_value_t = 1.0)]
    pub(super) speed_multiplier: f64,
}

#[derive(Parser)]
pub(super) struct ShotArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "PNG path to write",
        short,
        long,
        value_name = "PATH",
        default_value = "shot.png"
    )]
    pub(super) output: PathBuf,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Capture the Studio Edit window", long, conflicts_with_all = ["client", "player"])]
    pub(super) studio: bool,
    #[arg(help = "Capture a play client", long, conflicts_with = "studio")]
    pub(super) client: bool,
    #[arg(
        help = "Move the camera here before capturing",
        long,
        value_name = "X,Y,Z",
        requires = "look_at"
    )]
    pub(super) camera_position: Option<String>,
    #[arg(
        help = "Point the camera at this position",
        long,
        value_name = "X,Y,Z",
        requires = "camera_position"
    )]
    pub(super) look_at: Option<String>,
}

#[derive(Parser)]
pub(super) struct RecordStartArgs {
    #[arg(help = "MP4 path to write", short, long, value_name = "PATH")]
    pub(super) output: Option<PathBuf>,
    #[arg(
        help = "Play client by name or index",
        short,
        long,
        value_name = "NAME|N"
    )]
    pub(super) player: Option<String>,
    #[arg(help = "Record the Studio Edit window", long, conflicts_with_all = ["client", "player"])]
    pub(super) studio: bool,
    #[arg(help = "Record a play client", long, conflicts_with = "studio")]
    pub(super) client: bool,
    #[arg(help = "Frames per second (1-30)", long, default_value_t = 12.0)]
    pub(super) fps: f64,
    #[arg(
        help = "Stop after this many seconds (1-300)",
        long,
        default_value_t = 60.0
    )]
    pub(super) max_seconds: f64,
    #[arg(help = "Video quality (0-100)", long, default_value_t = 80.0)]
    pub(super) quality: f32,
}

#[derive(Parser)]
pub(super) struct RecordEndArgs {
    #[arg(help = "Recording to stop")]
    pub(super) recording_id: Option<String>,
    /// Finish the video without generating its overview image.
    #[arg(help = "Skip the review image", long)]
    pub(super) no_review: bool,
}

#[derive(Parser)]
pub(super) struct RecordReviewArgs {
    #[arg(help = "Recording to review")]
    pub(super) file: PathBuf,
    /// Show 12 consecutive frames (pages start at 1). Default: sampled overview.
    #[arg(help = "Render this page of 12 frames", long, conflicts_with = "frame", value_parser = clap::value_parser!(u32).range(1..))]
    pub(super) page: Option<u32>,
    /// Extract one full-resolution frame (frames start at 1).
    #[arg(help = "Render one frame at full size", long, value_parser = clap::value_parser!(u32).range(1..))]
    pub(super) frame: Option<u32>,
    /// Write the PNG here instead of beside the recording in its .review folder.
    #[arg(help = "Image path to write", short, long)]
    pub(super) output: Option<PathBuf>,
}

#[derive(Parser)]
pub(super) struct StudioChangeStateArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "Services to track (comma-separated)",
        short,
        long,
        default_value = ""
    )]
    pub(super) services: String,
    #[arg(help = "Reset tracking state", long)]
    pub(super) reset: bool,
    #[arg(help = "Replace the tracked service set", long)]
    pub(super) replace_services: bool,
    #[arg(help = "Discard pending changes", long)]
    pub(super) clear_pending: bool,
    #[arg(help = "Report without starting tracking", long)]
    pub(super) no_start: bool,
    #[arg(help = "Stop tracking", long)]
    pub(super) stop: bool,
    #[arg(
        help = "Acknowledge changes through this sequence",
        long,
        value_name = "SEQ"
    )]
    pub(super) ack_seq: Option<u64>,
    #[arg(
        help = "Acknowledge runtime settings through this sequence",
        long,
        value_name = "SEQ"
    )]
    pub(super) ack_runtime_settings_seq: Option<u64>,
    #[arg(
        help = "Editor action IDs to acknowledge",
        long,
        value_name = "IDS",
        value_delimiter = ','
    )]
    pub(super) ack_actions: Vec<String>,
    #[arg(
        help = "Editor action results to acknowledge (JSON)",
        long,
        value_name = "JSON",
        default_value = "{}"
    )]
    pub(super) ack_action_results: String,
    #[arg(help = "Target this Studio runtime", long)]
    pub(super) runtime_id: Option<String>,
    #[arg(
        help = "Ignore Studio changes for this many seconds",
        long,
        value_name = "SECONDS"
    )]
    pub(super) suppress_seconds: Option<f64>,
    #[arg(
        help = "Wait up to this long for a change event",
        long = "event-wait-seconds",
        value_name = "SECONDS"
    )]
    pub(super) event_wait_seconds: Option<f64>,
    #[arg(
        long = "wait",
        value_name = "SECONDS",
        num_args = 0..=1,
        default_missing_value = "10",
        help = "Wait for watched file changes to finish syncing"
    )]
    pub(super) settle_wait_seconds: Option<f64>,
    #[arg(help = "Bind tracking to the current project context", long)]
    pub(super) context_bound: bool,
    #[arg(help = "Include full change details", long)]
    pub(super) details: bool,
    #[arg(
        help = "Resolve first-connection conflicts toward one side",
        long,
        value_name = "studio|editor"
    )]
    pub(super) prefer: Option<String>,
}

#[derive(Parser)]
pub(super) struct ImportPlaceArgs {
    #[arg(help = "Place file to import (.rbxl or .rbxlx)", value_name = "PLACE")]
    pub(super) input: PathBuf,
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Script source directory",
        long,
        alias = "src",
        value_name = "PATH",
        default_value = "src"
    )]
    pub(super) src_dir: PathBuf,
    #[arg(
        help = "Services to import (comma-separated)",
        short,
        long,
        default_value = ""
    )]
    pub(super) services: String,
}

#[derive(Parser)]
pub(super) struct PullArgs {
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Script source directory",
        long,
        alias = "src",
        value_name = "PATH",
        default_value = "src"
    )]
    pub(super) src_dir: PathBuf,
    #[arg(
        help = "Services to pull (comma-separated)",
        short,
        long,
        default_value = ""
    )]
    pub(super) services: String,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        help = "Store every property, including defaults",
        long,
        alias = "all-props"
    )]
    pub(super) export_all_properties: bool,
    #[arg(help = "Hide timing output", short, long)]
    pub(super) quiet_timings: bool,
}

#[derive(Clone, Parser)]
pub(super) struct PushEditorChangesArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Files or directories to push", value_name = "PATH")]
    pub(super) paths: Vec<PathBuf>,
    #[arg(
        help = "Changed file path (repeatable)",
        short = 'p',
        long = "changed-path",
        alias = "path",
        value_name = "PATH"
    )]
    pub(super) changed_paths: Vec<PathBuf>,
    #[arg(
        help = "File listing changed paths",
        short = 'f',
        long = "changed-paths-file",
        alias = "paths-file",
        value_name = "PATH"
    )]
    pub(super) changed_paths_files: Vec<PathBuf>,
    #[arg(
        help = "Only push these settings IDs",
        short = 'i',
        long = "target-settings-id",
        alias = "id"
    )]
    pub(super) target_settings_ids: Vec<String>,
    #[arg(
        help = "File listing target settings IDs",
        short = 'I',
        long = "target-settings-ids-file",
        alias = "ids-file",
        value_name = "PATH"
    )]
    pub(super) target_settings_id_files: Vec<PathBuf>,
    #[arg(
        help = "Only push these properties",
        short,
        long = "target-property",
        alias = "prop"
    )]
    pub(super) target_properties: Vec<String>,
    #[arg(
        help = "Create or update instances without deleting",
        short,
        long,
        alias = "upsert"
    )]
    pub(super) upsert_instances_only: bool,
    #[arg(help = "Verify pushed script sources", long, alias = "verify")]
    pub(super) verify_sources: bool,
    #[arg(help = "Skip the Studio review", long)]
    pub(super) no_review: bool,
    #[arg(help = "Apply without confirmation", long, alias = "apply")]
    pub(super) yes: bool,
    #[arg(
        help = "Cache dir for renium-link git/wally sources, used when enforcing read-only link mirrors during a push. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) link_cache_dir: Option<PathBuf>,
    #[arg(
        help = "Permit a push to modify mirrors from read-only Renium link packages. Disabled by default so package protection remains the safe behavior",
        long
    )]
    pub(super) override_packages: bool,
}

impl PushEditorChangesArgs {
    pub(super) fn new(project: ProjectSourceArgs, bridge: BridgeConnectionArgs) -> Self {
        Self {
            project,
            bridge,
            paths: Vec::new(),
            changed_paths: Vec::new(),
            changed_paths_files: Vec::new(),
            target_settings_ids: Vec::new(),
            target_settings_id_files: Vec::new(),
            target_properties: Vec::new(),
            upsert_instances_only: false,
            verify_sources: false,
            no_review: false,
            yes: false,
            link_cache_dir: None,
            override_packages: false,
        }
    }
}

#[derive(Args)]
pub(super) struct EditorMutationArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(help = "Service that owns the target", short, long)]
    pub(super) service: String,
    #[arg(help = "Target settings ID", short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[arg(
        help = "Class name of the new instance",
        short,
        long,
        alias = "class",
        default_value = ""
    )]
    pub(super) class_name: String,
    #[arg(
        help = "Target path as a JSON string array",
        short,
        long,
        alias = "path"
    )]
    pub(super) path_segments_json: String,
    #[arg(
        help = "Sibling ordinals (JSON array) for duplicate names",
        short = 'o',
        long,
        alias = "ords",
        default_value = "[]"
    )]
    pub(super) path_ordinals_json: String,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct ApplyEditorPropertyArgs {
    #[command(flatten)]
    pub(super) target: EditorMutationArgs,
    #[arg(
        help = "property or attribute",
        short = 'S',
        long,
        default_value = "property"
    )]
    pub(super) scope: String,
    #[arg(help = "Property or attribute name", short = 'n', long, alias = "prop")]
    pub(super) property: String,
    #[arg(
        help = "Value as JSON",
        short = 'j',
        long,
        alias = "value",
        required_unless_present = "source_file",
        conflicts_with = "source_file"
    )]
    pub(super) value_json: Option<String>,
    #[arg(help = "Read the value from this file", long, value_name = "PATH")]
    pub(super) source_file: Option<PathBuf>,
    #[arg(help = "Skip the Studio review", long)]
    pub(super) no_review: bool,
    #[arg(help = "Apply without confirmation", long, alias = "apply")]
    pub(super) yes: bool,
}

#[derive(Parser)]
pub(super) struct ApplyEditorDeleteArgs {
    #[command(flatten)]
    pub(super) target: EditorMutationArgs,
}

#[derive(Parser)]
pub(super) struct EditorRevertArgs {
    #[arg(
        help = "Project root directory",
        long,
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Script source directory",
        long,
        value_name = "PATH",
        default_value = "src"
    )]
    pub(super) src_dir: PathBuf,
    #[arg(help = "Restore this file", long)]
    pub(super) path: Option<PathBuf>,
    #[arg(help = "Restore this instance", long)]
    pub(super) settings_id: Option<String>,
    #[arg(help = "Restore this service's store", long)]
    pub(super) service: Option<String>,
    /// Restore file-backed sync history (an ID returned by push, or latest).
    #[arg(help = "Restore a sync history entry: latest or a historyId", long, value_name = "ID|latest", conflicts_with_all = ["path", "settings_id", "service"])]
    pub(super) sync: Option<String>,
    /// Include every restored path in sync undo output.
    #[arg(help = "List every restored path", long, requires = "sync")]
    pub(super) details: bool,
    #[arg(help = "Also push the restored files to Studio", long)]
    pub(super) apply_studio: bool,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Args, Default)]
pub(super) struct BytecodeFileArgs {
    #[arg(help = "Service name or store file")]
    pub(super) service_or_file: Option<String>,
    #[arg(
        help = "Store file instead of a service name",
        short = 'f',
        long,
        alias = "file",
        value_name = "PATH"
    )]
    pub(super) settings_file: Option<PathBuf>,
}

impl BytecodeFileArgs {
    pub(super) fn settings_file(settings_file: PathBuf) -> Self {
        Self {
            service_or_file: None,
            settings_file: Some(settings_file),
        }
    }
}

#[derive(Args)]
pub(super) struct BytecodeInstanceSelectorArgs {
    #[arg(help = "Select by settings ID", short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[arg(help = "Select by store index", short = 'x', long)]
    pub(super) index: Option<usize>,
    #[arg(help = "Select by exact name", short, long)]
    pub(super) name: Option<String>,
    #[arg(help = "Select by class name", short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(
        help = "Select by path (JSON string array); add --ords for duplicates",
        long = "path",
        alias = "path-segments",
        alias = "path-segments-json"
    )]
    pub(super) path_segments_json: Option<String>,
    #[arg(
        help = "Sibling ordinals (JSON array) for duplicate names",
        long = "ords",
        alias = "path-ordinals",
        alias = "path-ordinals-json",
        default_value = "[]"
    )]
    pub(super) path_ordinals_json: String,
}

impl Default for BytecodeInstanceSelectorArgs {
    fn default() -> Self {
        Self {
            settings_id: None,
            index: None,
            name: None,
            class_name: None,
            path_segments_json: None,
            path_ordinals_json: "[]".to_string(),
        }
    }
}

impl BytecodeInstanceSelectorArgs {
    pub(super) fn by_settings_id(settings_id: Option<String>) -> Self {
        Self {
            settings_id,
            ..Default::default()
        }
    }
}

#[derive(Parser)]
pub(super) struct BytecodeGetPropertyArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(help = "Property or attribute name", short, long, alias = "prop")]
    pub(super) property: String,
    #[arg(
        help = "auto, property or attribute",
        short = 'S',
        long,
        default_value = "auto"
    )]
    pub(super) scope: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeSetPropertyArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(help = "Property or attribute name", short, long, alias = "prop")]
    pub(super) property: String,
    #[arg(
        short = 'j',
        long,
        alias = "value",
        alias = "json",
        help = "JSON value, or - to read it from stdin"
    )]
    pub(super) value_json: Option<String>,
    #[arg(
        help = "String value",
        long = "str",
        alias = "value-str",
        allow_hyphen_values = true
    )]
    pub(super) value_str: Option<String>,
    #[arg(help = "Number value", long = "num", alias = "value-num")]
    pub(super) value_num: Option<f64>,
    #[arg(help = "Boolean value", long = "bool", alias = "value-bool")]
    pub(super) value_bool: Option<bool>,
    #[arg(help = "Remove the stored value", long = "null", alias = "value-null")]
    pub(super) value_null: bool,
    #[arg(
        help = "auto, property or attribute",
        short = 'S',
        long,
        default_value = "auto"
    )]
    pub(super) scope: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeApplyPropertyBatchArgs {
    #[arg(
        help = "Project root directory",
        long,
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(help = "Property batch JSON file", long, value_name = "PATH")]
    pub(super) input: PathBuf,
    #[arg(
        help = "Direction recorded for the batch",
        long,
        default_value = "studio-to-files"
    )]
    pub(super) direction: String,
    #[arg(help = "Allow edits inside linked packages", long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeSetSourceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(help = "Service that owns the script", short, long)]
    pub(super) service: Option<String>,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(
        short = 'j',
        long,
        alias = "value",
        alias = "json",
        help = "JSON value, or - to read it from stdin"
    )]
    pub(super) value_json: Option<String>,
    #[arg(
        help = "Source text",
        long = "str",
        visible_alias = "source",
        alias = "value-str",
        allow_hyphen_values = true
    )]
    pub(super) value_str: Option<String>,
    #[arg(
        help = "Read the source from a file instead of an argument — use this for large scripts that exceed the OS command-line length limit",
        long,
        alias = "src-file",
        value_name = "PATH"
    )]
    pub(super) source_file: Option<PathBuf>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeExplorerBatchArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(help = "Service that owns the store", short, long, default_value = "")]
    pub(super) service: String,
    #[arg(help = "Project root directory", long)]
    pub(super) project_root: Option<PathBuf>,
    #[arg(
        help = "Operations JSON",
        short = 'j',
        long = "ops",
        alias = "ops-json",
        value_name = "JSON",
        conflicts_with = "ops_file"
    )]
    pub(super) ops_json: Option<String>,
    #[arg(
        help = "Operations JSON file (- for stdin)",
        short = 'J',
        long,
        value_name = "PATH"
    )]
    pub(super) ops_file: Option<PathBuf>,
    #[arg(
        help = "Detail level: compact, summary, detail or full",
        short,
        long,
        alias = "mode"
    )]
    pub(super) output: Option<String>,
    #[arg(
        help = "Fields or preset for search results",
        short = 'F',
        long,
        alias = "fs"
    )]
    pub(super) fields: Option<String>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BytecodeExplorerBatchRequest {
    pub(super) ops: Vec<BytecodeExplorerBatchOp>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum BytecodeBatchFields {
    Csv(String),
    List(Vec<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct BytecodeExplorerBatchOp {
    #[serde(rename = "type", alias = "op", alias = "kind")]
    pub(super) op: String,
    #[serde(alias = "rid", alias = "request_id")]
    pub(super) request_id: Option<String>,
    #[serde(alias = "o")]
    pub(super) output: Option<String>,
    pub(super) fields: Option<BytecodeBatchFields>,
    #[serde(alias = "q")]
    pub(super) query: Option<String>,
    #[serde(alias = "l")]
    pub(super) limit: Option<usize>,
    #[serde(alias = "id", alias = "settings_id")]
    pub(super) settings_id: Option<String>,
    #[serde(alias = "x")]
    pub(super) index: Option<usize>,
    #[serde(alias = "n")]
    pub(super) name: Option<String>,
    #[serde(alias = "class", alias = "c", alias = "class_name")]
    pub(super) class_name: Option<String>,
    #[serde(
        alias = "parentId",
        alias = "pid",
        alias = "parent_settings_id",
        alias = "parent_id"
    )]
    pub(super) parent_settings_id: Option<String>,
    #[serde(
        default,
        alias = "path",
        alias = "path_segments",
        deserialize_with = "deserialize_batch_path"
    )]
    pub(super) path_segments: Option<Vec<String>>,
    #[serde(default, alias = "ords", alias = "path_ordinals")]
    pub(super) path_ordinals: Vec<usize>,
    #[serde(default, alias = "props")]
    pub(super) properties: Vec<String>,
    #[serde(default, alias = "attrs")]
    pub(super) attributes: Vec<String>,
    pub(super) tag: Option<String>,
}

fn deserialize_batch_path<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(serde_json::Value::String(path)) => crate::bytecode::parse_path_segments(&path)
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(value @ serde_json::Value::Array(_)) => {
            serde_json::from_value(value).map(Some).map_err(|error| {
                serde::de::Error::custom(format!("path must contain only string segments: {error}"))
            })
        }
        Some(_) => Err(serde::de::Error::custom(
            "path must be a string or an array of string segments",
        )),
    }
}

#[derive(Parser)]
pub(super) struct BytecodeAddInstanceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(skip)]
    pub(super) service_hint: Option<String>,
    #[arg(help = "Name of the new instance", short, long)]
    pub(super) name: String,
    #[arg(help = "Class name of the new instance", short, long, alias = "class")]
    pub(super) class_name: String,
    #[arg(help = "Settings ID to assign", short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[command(flatten)]
    pub(super) parent: BytecodeParentArgs,
    #[arg(help = "Property as NAME=JSON (repeatable)", short, long = "property")]
    pub(super) properties: Vec<String>,
    #[arg(
        help = "Attribute as NAME=JSON (repeatable)",
        short,
        long = "attribute"
    )]
    pub(super) attributes: Vec<String>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Args, Default)]
pub(super) struct BytecodeParentArgs {
    #[arg(help = "Parent store index", short = 'x', long)]
    pub(super) parent_index: Option<usize>,
    #[arg(help = "Parent settings ID", short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: Option<String>,
    #[arg(help = "Parent exact name", short = 'N', long)]
    pub(super) parent_name: Option<String>,
    #[arg(help = "Parent class name", short = 'C', long, alias = "parent-class")]
    pub(super) parent_class_name: Option<String>,
    #[arg(help = "Place at the service root", long, alias = "root")]
    pub(super) no_parent: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeCloneInstanceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(help = "Service that owns the source", short, long, default_value = "")]
    pub(super) service: String,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(help = "Parent store index", short = 'X', long)]
    pub(super) parent_index: Option<usize>,
    #[arg(help = "Parent settings ID", short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: Option<String>,
    #[arg(help = "Parent exact name", short = 'N', long)]
    pub(super) parent_name: Option<String>,
    #[arg(help = "Parent class name", short = 'C', long, alias = "parent-class")]
    pub(super) parent_class_name: Option<String>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeRemoveInstanceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(help = "Keep descendants", short = 'R', long)]
    pub(super) no_recursive: bool,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeDesyncPackageLinkArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(help = "Service that owns the target", short, long, default_value = "")]
    pub(super) service: String,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeExportModelArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(help = "Service that owns the source", short, long, default_value = "")]
    pub(super) service: String,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(help = "Model file to write", short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(help = "Output format", long, value_name = "rbxm|rbxmx")]
    pub(super) format: Option<String>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeExportPlaceArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Services to include (comma-separated)",
        short,
        long,
        default_value = ""
    )]
    pub(super) services: String,
    #[arg(help = "Place file to write", short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(help = "Output format", long, value_name = "rbxl|rbxlx")]
    pub(super) format: Option<String>,
    #[arg(
        help = "Place file whose unsynced services and root fields are kept",
        long,
        value_name = "PATH"
    )]
    pub(super) base: Option<PathBuf>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeImportModelArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(help = "Service that owns the parent", short, long, default_value = "")]
    pub(super) service: String,
    #[arg(help = "Model file to import", short, long, value_name = "PATH")]
    pub(super) model: PathBuf,
    #[command(flatten)]
    pub(super) parent: BytecodeParentArgs,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct SyncWallyPackagesArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "wally.toml path",
        long,
        value_name = "PATH",
        default_value = "wally.toml"
    )]
    pub(super) manifest: PathBuf,
    #[arg(
        help = "Wally executable",
        long,
        value_name = "COMMAND",
        default_value = "wally"
    )]
    pub(super) wally_path: String,
    #[arg(
        help = "Packages directory",
        long,
        value_name = "PATH",
        default_value = "Packages"
    )]
    pub(super) packages_dir: PathBuf,
    #[arg(
        help = "Service for shared packages",
        long,
        default_value = "ReplicatedStorage"
    )]
    pub(super) target_service: String,
    #[arg(
        help = "Instance name for shared packages",
        long,
        default_value = "Packages"
    )]
    pub(super) target_name: String,
    #[arg(
        help = "Comma list of realms to import: shared, server, dev. Server/dev are imported only when their package directory exists",
        long,
        value_name = "LIST",
        default_value = "shared,server,dev"
    )]
    pub(super) realms: String,
    #[arg(
        help = "ServerPackages directory",
        long,
        value_name = "PATH",
        default_value = "ServerPackages"
    )]
    pub(super) server_packages_dir: PathBuf,
    #[arg(
        help = "Service for server packages",
        long,
        default_value = "ServerStorage"
    )]
    pub(super) server_target_service: String,
    #[arg(
        help = "Instance name for server packages",
        long,
        default_value = "ServerPackages"
    )]
    pub(super) server_target_name: String,
    #[arg(
        help = "DevPackages directory",
        long,
        value_name = "PATH",
        default_value = "DevPackages"
    )]
    pub(super) dev_packages_dir: PathBuf,
    #[arg(
        help = "Service for dev packages",
        long,
        default_value = "ReplicatedStorage"
    )]
    pub(super) dev_target_service: String,
    #[arg(
        help = "Instance name for dev packages",
        long,
        default_value = "DevPackages"
    )]
    pub(super) dev_target_name: String,
    #[arg(
        help = "Re-import even when wally.lock is unchanged since the last sync",
        long
    )]
    pub(super) force: bool,
    #[arg(help = "Link existing packages without installing", long)]
    pub(super) skip_install: bool,
    #[arg(help = "Include full paths and IDs", long)]
    pub(super) details: bool,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkApplyArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(
        help = "Apply only the link with this id (default: all links)",
        long,
        value_name = "ID"
    )]
    pub(super) link: Option<String>,
    #[arg(
        help = "Report drift only; do not write files, settings, or the lockfile",
        long
    )]
    pub(super) check: bool,
    #[arg(
        help = "Include unchanged package targets in changedPaths/targetSettingsIds so explicit Studio pushes can upsert the already-materialized subtree",
        long
    )]
    pub(super) force_targets: bool,
    #[arg(
        help = "Force-apply one specific target as {\"service\":\"...\",\"path\":[...],\"ords\":[...]}. Repeatable.",
        long,
        value_name = "JSON"
    )]
    pub(super) force_target: Vec<String>,
    #[arg(
        help = "Never fetch git/wally sources; fail if a remote source is not cached",
        long
    )]
    pub(super) offline: bool,
    #[arg(
        help = "Exit with an error (ok:false) when any link resolves with a warning. Recommended for CI so unreachable or invalid sources fail the build",
        long
    )]
    pub(super) strict: bool,
    #[arg(
        help = "Git executable",
        long,
        value_name = "COMMAND",
        default_value = "git"
    )]
    pub(super) git_path: String,
    #[arg(
        help = "Wally executable",
        long,
        value_name = "COMMAND",
        default_value = "wally"
    )]
    pub(super) wally_path: String,
    #[arg(
        help = "Where cloned git/wally sources are cached. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) cache_dir: Option<PathBuf>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkBreakArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(help = "Break every target of this link id", long, value_name = "ID")]
    pub(super) link: Option<String>,
    #[arg(help = "Break a single target: the owning service", long)]
    pub(super) service: Option<String>,
    #[arg(
        help = "Break a single target: JSON array of path segments (includes the service root)",
        long = "path",
        value_name = "JSON"
    )]
    pub(super) path_segments_json: Option<String>,
    #[arg(
        help = "Sibling ordinals for --path as a JSON array",
        long = "ords",
        alias = "path-ordinals",
        value_name = "JSON",
        default_value = "[]"
    )]
    pub(super) path_ordinals_json: String,
    #[arg(
        help = "Remove the selected target from the link manifest instead of retaining it as temporarily broken",
        long
    )]
    pub(super) remove: bool,
    #[arg(
        help = "Where cloned git/wally sources are cached. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) cache_dir: Option<PathBuf>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkStatusArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(
        help = "Where cloned git/wally sources are cached. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) cache_dir: Option<PathBuf>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkAddArgs {
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(
        help = "Stable link id; defaults to a slug of the first target name",
        long
    )]
    pub(super) id: Option<String>,
    #[arg(
        help = "Source kind: local | git | wally",
        long,
        value_name = "KIND",
        default_value = "local"
    )]
    pub(super) source_type: String,
    #[arg(
        help = "local: file/dir path. git: repo url. wally: package name (scope/name). Optional when --id refers to an existing link (inserting it elsewhere)",
        long,
        value_name = "VALUE"
    )]
    pub(super) source: Option<String>,
    #[arg(
        help = "git ref (branch/tag/commit) or wally version requirement",
        long = "ref",
        value_name = "REF"
    )]
    pub(super) source_ref: Option<String>,
    #[arg(
        help = "git subpath within the repo",
        long = "subpath",
        value_name = "PATH"
    )]
    pub(super) source_subpath: Option<String>,
    #[command(flatten)]
    pub(super) target: LinkTargetArgs,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Args)]
pub(super) struct LinkTargetArgs {
    #[arg(help = "Target service", long)]
    pub(super) service: String,
    #[arg(
        help = "Target path as a JSON array of segments (includes the service root)",
        long = "path",
        value_name = "JSON"
    )]
    pub(super) path_segments_json: String,
    #[arg(
        help = "Sibling ordinals for --path as a JSON array",
        long = "ords",
        alias = "path-ordinals",
        value_name = "JSON",
        default_value = "[]"
    )]
    pub(super) path_ordinals_json: String,
    #[arg(
        help = "Mark the link writable (targets are editable, not reverted)",
        long
    )]
    pub(super) writable: bool,
}

#[derive(Parser)]
pub(super) struct LinkMoveTargetArgs {
    #[arg(
        help = "Project root directory",
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(help = "Current service", long, value_name = "SERVICE")]
    pub(super) old_service: String,
    #[arg(
        help = "Current target path (JSON string array)",
        long = "old-path",
        value_name = "JSON"
    )]
    pub(super) old_path_segments_json: String,
    #[arg(
        help = "Current sibling ordinals (JSON array)",
        long = "old-ords",
        value_name = "JSON",
        default_value = "[]"
    )]
    pub(super) old_path_ordinals_json: String,
    #[arg(help = "New service", long, value_name = "SERVICE")]
    pub(super) new_service: String,
    #[arg(
        help = "New target path (JSON string array)",
        long = "new-path",
        value_name = "JSON"
    )]
    pub(super) new_path_segments_json: String,
    #[arg(
        help = "New sibling ordinals (JSON array)",
        long = "new-ords",
        value_name = "JSON",
        default_value = "[]"
    )]
    pub(super) new_path_ordinals_json: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
#[command(
    about = "Pack an existing instance subtree into a reusable bytecode package and register it as a link target"
)]
pub(super) struct LinkPackArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(
        help = "Project folder where bytecode packages are stored (commit it to share packages with the repo). Omit to save into the per-user global library (Documents/Renium/Packages), usable from any project on this machine",
        long,
        value_name = "PATH"
    )]
    pub(super) link_folder: Option<PathBuf>,
    #[arg(
        help = "Package / link id; defaults to a slug of the instance name",
        long
    )]
    pub(super) id: Option<String>,
    #[command(flatten)]
    pub(super) target: LinkTargetArgs,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
#[command(about = "Delete a bytecode package link and optionally resolve/delete existing uses")]
pub(super) struct LinkDeletePackageArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(
        help = "Link manifest path",
        long,
        value_name = "PATH",
        default_value = "renium-link.json"
    )]
    pub(super) manifest: PathBuf,
    #[arg(help = "Package / link id to delete", long)]
    pub(super) id: String,
    #[arg(
        help = "delete-unused | delete-uses | unlink-uses",
        long,
        default_value = "delete-unused"
    )]
    pub(super) action: String,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeRepackArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(help = "Stores or services to upgrade", value_name = "SERVICE_OR_FILE")]
    pub(super) paths: Vec<PathBuf>,
    #[arg(help = "Pretty-print the JSON result", long)]
    pub(super) pretty: bool,
}
