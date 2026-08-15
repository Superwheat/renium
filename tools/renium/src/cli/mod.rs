use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use serde::Deserialize;

pub(crate) mod args;
pub(crate) mod config;
pub(crate) mod dispatch;

use crate::app::update;
use crate::cli::args::{
    CursorPollArgs, GenerateSourcemapArgs, ImportServiceArgs, ImportSnapshotsArgs, VcInitArgs,
    VcMergeArgs, VcTextconvArgs, ViewArgs,
};
use crate::daemon::transport::DEFAULT_DAEMON_CONTROL_PORT;
use crate::project::config as project_config;
use crate::project::workflows;
use crate::studio::bridge::DEFAULT_EXPORT_CHUNK_SIZE;

#[derive(Parser)]
#[command(author, version)]
pub(super) struct Cli {
    #[arg(
        help = "Pin bridge commands to one Studio place by name, placeId, or gameId:placeId (env: RENIUM_PLACE)",
        long,
        global = true,
        value_name = "NAME|ID|GAME:PLACE"
    )]
    pub(super) place: Option<String>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Use this renium.project.jsonc instead of nearest-project discovery"
    )]
    pub(super) project: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "off|error|warn|info|debug|trace",
        default_value = "info"
    )]
    pub(super) log_level: String,
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub(super) verbose: u8,
    #[arg(
        long,
        global = true,
        value_name = "auto|always|never",
        default_value = "auto"
    )]
    pub(super) color: String,
    #[arg(long, global = true)]
    pub(super) yes: bool,
    #[arg(long, global = true)]
    pub(super) backtrace: bool,
    #[arg(
        long,
        global = true,
        value_name = "text|json|pretty",
        default_value = "text"
    )]
    pub(super) output_mode: String,
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help = "Use a named Renium daemon"
    )]
    pub(super) daemon: Option<String>,
    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Subcommand)]
pub(super) enum Commands {
    #[command(name = "a", alias = "automation")]
    Automation(AutomationArgs),
    FmtProject(project_config::FmtProjectArgs),
    ExplainPath(project_config::ExplainPathArgs),
    Config(project_config::ConfigArgs),
    Adapters(project_config::AdaptersArgs),
    ImportRojo(project_config::ImportRojoArgs),
    Init(workflows::InitArgs),
    Build(workflows::BuildArgs),
    Doctor(workflows::DoctorArgs),
    Docs(workflows::DocsArgs),
    Daemon(workflows::DaemonArgs),
    Studio(workflows::StudioArgs),
    #[command(name = "upload-place", alias = "upload")]
    Upload(workflows::UploadArgs),
    Update(update::UpdateArgs),
    #[command(hide = true)]
    UpdateHelper(update::UpdateHelperArgs),
    Syncback(SyncbackArgs),
    ImportPath(ImportPathArgs),
    Create(CreateInstanceArgs),
    Clone(CloneInstanceCommandArgs),
    Move(MoveInstanceArgs),
    Rename(RenameInstanceArgs),
    Remove(RemoveInstanceCommandArgs),
    DesyncPackageLink(DesyncPackageLinkCommandArgs),
    ImportModel(ImportModelCommandArgs),
    ExportModel(ExportModelCommandArgs),
    Test(TestArgs),
    #[command(alias = "x")]
    ExportSnapshots(ExportSnapshotsArgs),
    #[command(alias = "bd")]
    BridgeDaemon(BridgeDaemonArgs),
    #[command(alias = "ed")]
    ExplorerDaemon(ExplorerDaemonArgs),
    #[command(alias = "src")]
    BridgeGetSource(BridgeGetSourceArgs),
    #[command(alias = "co")]
    GetConsoleOutput(PluginConsoleOutputArgs),
    #[command(alias = "lx")]
    ExecuteLuau(ExecuteLuauArgs),
    #[command(
        alias = "device",
        alias = "dev",
        about = "Control Studio's built-in device simulator"
    )]
    StudioDevice(StudioDeviceArgs),
    #[command(alias = "play")]
    StartStopPlay(StartStopPlayArgs),
    #[command(alias = "clients")]
    ListClients(ListClientsArgs),
    #[command(alias = "review")]
    EditorReviewDecision(EditorReviewDecisionArgs),
    #[command(alias = "pr")]
    Press(PressArgs),
    #[command(alias = "clk")]
    Click(ClickArgs),
    #[command(alias = "ky")]
    Key(KeyArgs),
    Ui(UiArgs),
    #[command(alias = "ty")]
    Type(TypeArgs),
    #[command(alias = "wait")]
    WaitUntil(WaitUntilArgs),
    #[command(alias = "go")]
    Goto(GotoArgs),
    #[command(alias = "sc")]
    Shot(ShotArgs),
    Setup(SetupArgs),
    #[command(alias = "st")]
    StudioChangeState(StudioChangeStateArgs),
    #[command(alias = "push")]
    PushEditorChanges(PushEditorChangesArgs),
    #[command(alias = "prop")]
    ApplyEditorProperty(ApplyEditorPropertyArgs),
    #[command(alias = "del")]
    ApplyEditorDelete(ApplyEditorDeleteArgs),
    #[command(alias = "rev")]
    EditorRevert(EditorRevertArgs),
    Find(FindArgs),
    Tree(TreeArgs),
    Inspect(InspectArgs),
    #[command(alias = "bg")]
    BytecodeGetProperty(BytecodeGetPropertyArgs),
    #[command(alias = "bs")]
    BytecodeSetProperty(BytecodeSetPropertyArgs),
    #[command(hide = true)]
    BytecodeApplyPropertyBatch(BytecodeApplyPropertyBatchArgs),
    #[command(alias = "bss")]
    BytecodeSetSource(BytecodeSetSourceArgs),
    #[command(alias = "bb")]
    BytecodeExplorerBatch(BytecodeExplorerBatchArgs),
    #[command(alias = "bt")]
    BytecodeEditorTargets(BytecodeEditorTargetsArgs),
    #[command(alias = "ba")]
    BytecodeAddInstance(BytecodeAddInstanceArgs),
    #[command(alias = "bcl")]
    BytecodeCloneInstance(BytecodeCloneInstanceArgs),
    #[command(alias = "br")]
    BytecodeRemoveInstance(BytecodeRemoveInstanceArgs),
    #[command(alias = "bdp")]
    BytecodeDesyncPackageLink(BytecodeDesyncPackageLinkArgs),
    #[command(alias = "bem")]
    BytecodeExportModel(BytecodeExportModelArgs),
    #[command(alias = "bep")]
    BytecodeExportPlace(BytecodeExportPlaceArgs),
    #[command(alias = "pdp")]
    PlaceDesyncPackageLink(PlaceDesyncPackageLinkArgs),
    #[command(alias = "bim")]
    BytecodeImportModel(BytecodeImportModelArgs),
    #[command(alias = "wally")]
    SyncWallyPackages(SyncWallyPackagesArgs),
    #[command(alias = "lk")]
    LinkApply(LinkApplyArgs),
    #[command(alias = "lkb")]
    LinkBreak(LinkBreakArgs),
    #[command(alias = "lks")]
    LinkStatus(LinkStatusArgs),
    #[command(alias = "lka")]
    LinkAdd(LinkAddArgs),
    #[command(alias = "lkm", hide = true)]
    LinkMoveTarget(LinkMoveTargetArgs),
    #[command(alias = "lkp")]
    LinkPack(LinkPackArgs),
    #[command(alias = "lkd")]
    LinkDeletePackage(LinkDeletePackageArgs),
    #[command(alias = "bpack")]
    BytecodeRepack(BytecodeRepackArgs),
    #[command(alias = "im")]
    ImportSnapshots(ImportSnapshotsArgs),
    #[command(alias = "ims")]
    ImportService(ImportServiceArgs),
    #[command(alias = "sm")]
    GenerateSourcemap(GenerateSourcemapArgs),
    #[command(alias = "vci")]
    VcInit(VcInitArgs),
    #[command(alias = "vct")]
    VcTextconv(VcTextconvArgs),
    #[command(alias = "v")]
    View(ViewArgs),
    #[command(alias = "vcm")]
    VcMerge(VcMergeArgs),
    #[command(alias = "cpoll", hide = true)]
    CursorPoll(CursorPollArgs),
}

#[derive(Parser)]
pub(super) struct AutomationArgs {
    #[arg(value_name = "OP")]
    pub(super) operation: String,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub(super) args: Vec<String>,
}

#[derive(Clone, clap::Args)]
pub(super) struct BridgeConnectionArgs {
    #[arg(short, long, alias = "wait", default_value_t = 8.0)]
    pub(super) wait_seconds: f64,
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub(super) host: String,
    #[arg(short = 'P', long, default_value = "8781,8782")]
    pub(super) ports: String,
}

impl BridgeConnectionArgs {
    pub(super) fn local(wait_seconds: f64) -> Self {
        Self {
            wait_seconds,
            host: "127.0.0.1".to_string(),
            ports: "8781,8782".to_string(),
        }
    }
}

#[derive(Parser)]
pub(super) struct BridgeDaemonArgs {
    #[arg(long)]
    pub(super) name: Option<String>,
    #[arg(long = "serve", alias = "keep-alive", hide = true)]
    pub(super) _serve: bool,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(long, alias = "ctl-port", default_value_t = DEFAULT_DAEMON_CONTROL_PORT)]
    pub(super) control_port: u16,
    #[arg(
        long,
        help = "Use the editor-owned JSON stdin protocol and exit when stdin closes"
    )]
    pub(super) editor_stdio: bool,
    #[arg(
        help = "Exit automatically when this process dies. Passed by the editor so an editor-owned daemon can't outlive its window; omit for a shared daemon",
        long,
        value_name = "PID"
    )]
    pub(super) parent_pid: Option<u32>,
}

#[derive(Parser)]
pub(super) struct ExplorerDaemonArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(short, long, default_value = "")]
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
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(
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
    pub(super) query_or_service: Option<String>,
    pub(super) query: Option<String>,
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(short, long)]
    pub(super) service: Option<String>,
    #[arg(short, long)]
    pub(super) name: Option<String>,
    #[arg(short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: Option<String>,
    #[arg(short, long)]
    pub(super) tag: Option<String>,
    #[arg(short, long = "property")]
    pub(super) properties: Vec<String>,
    #[arg(short, long = "attribute")]
    pub(super) attributes: Vec<String>,
    #[arg(long)]
    pub(super) all: bool,
    #[arg(short, long, default_value_t = 20)]
    pub(super) limit: usize,
    #[arg(short, long, default_value = "compact")]
    pub(super) output: String,
    #[arg(short = 'F', long, default_value = "lookup,ords")]
    pub(super) fields: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct HighLevelTargetArgs {
    pub(super) service_or_target: Option<String>,
    pub(super) target: Option<String>,
    #[arg(short, long)]
    pub(super) service: Option<String>,
    #[arg(short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[arg(short = 'x', long)]
    pub(super) index: Option<usize>,
    #[arg(short, long)]
    pub(super) name: Option<String>,
    #[arg(short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(
        long,
        alias = "path-json",
        alias = "path-segments",
        alias = "path-segments-json"
    )]
    pub(super) path: Option<String>,
    #[arg(long, alias = "path-ordinals", alias = "path-ordinals-json")]
    pub(super) ords: Option<String>,
}

#[derive(Parser)]
pub(super) struct TreeArgs {
    #[command(flatten)]
    pub(super) target: HighLevelTargetArgs,
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, default_value_t = 1)]
    pub(super) depth: usize,
    #[arg(short, long)]
    pub(super) limit: Option<usize>,
    #[arg(short, long, default_value = "compact")]
    pub(super) output: String,
    #[arg(short = 'F', long, default_value = "tree,ords")]
    pub(super) fields: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct InspectArgs {
    #[command(flatten)]
    pub(super) target: HighLevelTargetArgs,
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(short, long, default_value = "compact")]
    pub(super) output: String,
    #[arg(short = 'F', long, default_value = "brief,ords")]
    pub(super) fields: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BridgeGetSourceArgs {
    #[arg(short, long)]
    pub(super) service: String,
    #[arg(short = 'k', long, value_name = "KEY")]
    pub(super) source_key: String,
    #[arg(short, long, value_name = "PATH")]
    pub(super) expect_file: Option<PathBuf>,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short, long, default_value_t = DEFAULT_EXPORT_CHUNK_SIZE)]
    pub(super) chunk_size: usize,
}

#[derive(Parser)]
pub(super) struct PluginConsoleOutputArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short = 'n', long, default_value_t = 200)]
    pub(super) limit: usize,
    #[arg(short, long, alias = "since", default_value_t = 0)]
    pub(super) since_seq: u64,
    #[arg(long, hide = true)]
    pub(super) from_oldest: bool,
    #[arg(short, long)]
    pub(super) clear: bool,
    #[arg(long)]
    pub(super) client: bool,
    #[arg(long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(short, long)]
    pub(super) follow: bool,
    #[arg(long, value_name = "TEXT")]
    pub(super) grep: Option<String>,
    #[arg(long, value_name = "TYPE")]
    pub(super) level: Option<String>,
    #[arg(long, default_value_t = 200)]
    pub(super) interval_ms: u64,
}

#[derive(Parser)]
pub(super) struct ExecuteLuauArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short = 'e', long)]
    pub(super) code: Option<String>,
    #[arg(short, long, value_name = "PATH")]
    pub(super) file: Option<PathBuf>,
    #[arg(short, long)]
    pub(super) client: bool,
    #[arg(long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(short, long, default_value_t = 10.0)]
    pub(super) timeout: f64,
}

#[derive(Parser)]
pub(super) struct StudioDeviceArgs {
    #[arg(default_value = "status",
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
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct StartStopPlayArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short, long)]
    pub(super) start: bool,
    #[arg(short = 'x', long)]
    pub(super) stop: bool,
    #[arg(short, long, value_name = "N")]
    pub(super) players: Option<u32>,
    #[arg(long, value_name = "play|run|server")]
    pub(super) mode: Option<String>,
}

#[derive(Parser)]
pub(super) struct TestArgs {
    #[arg(long, value_name = "play|run|server", default_value = "play")]
    pub(super) mode: String,
    #[arg(short, long, value_name = "N")]
    pub(super) players: Option<u32>,
    #[arg(short, long, default_value_t = 30.0)]
    pub(super) timeout: f64,
    #[arg(long)]
    pub(super) fail_on_error: bool,
    #[arg(long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
}

#[derive(Parser)]
pub(super) struct SyncbackArgs {
    #[arg(long, value_name = "PATH", default_value = "snapshots")]
    pub(super) input: PathBuf,
    #[arg(long)]
    pub(super) project: Option<PathBuf>,
    #[arg(long)]
    pub(super) list: bool,
    #[arg(long)]
    pub(super) dry_run: bool,
    #[arg(short, long)]
    pub(super) yes: bool,
    #[arg(short, long, default_value = "")]
    pub(super) services: String,
}

#[derive(Parser)]
pub(super) struct ImportPathArgs {
    pub(super) source: PathBuf,
    #[arg(long, value_name = "PATH", required_unless_present = "path_json")]
    pub(super) destination: Option<PathBuf>,
    #[arg(
        long,
        value_name = "[\"Service\",\"Parent\",\"Name\"]",
        conflicts_with = "destination"
    )]
    pub(super) path_json: Option<String>,
    #[arg(long)]
    pub(super) project: Option<PathBuf>,
    #[arg(short = 'r', long, default_value = ".")]
    pub(super) project_root: PathBuf,
    #[arg(long)]
    pub(super) dry_run: bool,
    #[arg(long)]
    pub(super) push: bool,
}

#[derive(Parser)]
pub(super) struct CreateInstanceArgs {
    pub(super) service: String,
    #[arg(short, long, alias = "class")]
    pub(super) class_name: String,
    #[arg(short, long)]
    pub(super) name: String,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: Option<String>,
    #[arg(short = 'r', long, default_value = ".")]
    pub(super) project_root: PathBuf,
    #[arg(short = 'd', long)]
    pub(super) src_root: Option<PathBuf>,
    #[arg(short, long = "property")]
    pub(super) properties: Vec<String>,
    #[arg(short, long = "attribute")]
    pub(super) attributes: Vec<String>,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Args)]
pub(super) struct ProjectInstanceArgs {
    pub(super) service: String,
    #[arg(short = 'i', long, alias = "id")]
    pub(super) settings_id: String,
    #[arg(short = 'r', long, default_value = ".")]
    pub(super) project_root: PathBuf,
}

#[derive(Parser)]
pub(super) struct CloneInstanceCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: String,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct MoveInstanceArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(long = "to-service")]
    pub(super) target_service: Option<String>,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: String,
    #[arg(short = 'd', long)]
    pub(super) src_root: Option<PathBuf>,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct RenameInstanceArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    pub(super) name: String,
    #[arg(short = 'd', long)]
    pub(super) src_root: Option<PathBuf>,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct RemoveInstanceCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(short = 'R', long)]
    pub(super) no_recursive: bool,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct DesyncPackageLinkCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct ImportModelCommandArgs {
    pub(super) service: String,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: String,
    #[arg(short, long, value_name = "PATH")]
    pub(super) model: PathBuf,
    #[arg(short = 'r', long, default_value = ".")]
    pub(super) project_root: PathBuf,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct ExportModelCommandArgs {
    #[command(flatten)]
    pub(super) target: ProjectInstanceArgs,
    #[arg(short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(long, value_name = "rbxm|rbxmx")]
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
    #[arg(short = 'i', long)]
    pub(super) review_id: Option<String>,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Parser)]
pub(super) struct PressArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(value_name = "GUI_PATH", required_unless_present = "id")]
    pub(super) path: Option<String>,
    #[arg(short, long)]
    pub(super) id: Option<String>,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(long)]
    pub(super) right: bool,
    #[arg(long)]
    pub(super) world: bool,
    #[arg(long, alias = "hold-ms", value_name = "MS", default_value_t = 30)]
    pub(super) hold: u64,
}

#[derive(Parser)]
pub(super) struct ClickArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    pub(super) x: i32,
    pub(super) y: i32,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(long)]
    pub(super) right: bool,
    #[arg(long, alias = "hold-ms", value_name = "MS", default_value_t = 30)]
    pub(super) hold: u64,
}

#[derive(Parser)]
pub(super) struct KeyArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    pub(super) key: String,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(long, value_name = "MS", default_value_t = 60)]
    pub(super) hold_ms: u64,
}

#[derive(Parser)]
pub(super) struct UiArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(short = 'n', long, default_value_t = 200)]
    pub(super) limit: usize,
    #[arg(long, alias = "all")]
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
    #[arg(long)]
    pub(super) status: bool,
    #[arg(long)]
    pub(super) repair: bool,
    #[arg(long)]
    pub(super) uninstall: bool,
}

#[derive(Parser)]
pub(super) struct TypeArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    pub(super) text: String,
    #[arg(long, value_name = "GUI_PATH")]
    pub(super) path: Option<String>,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(long)]
    pub(super) enter: bool,
}

#[derive(Parser)]
pub(super) struct WaitUntilArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(value_name = "LUAU_CONDITION")]
    pub(super) condition: String,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(short, long)]
    pub(super) client: bool,
    #[arg(short, long, default_value_t = 10.0)]
    pub(super) timeout: f64,
    #[arg(long, default_value_t = 0.25)]
    pub(super) interval: f64,
}

#[derive(Parser)]
pub(super) struct GotoArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(value_name = "PART_PATH", required_unless_present = "pos")]
    pub(super) target: Option<String>,
    #[arg(long, value_name = "X,Y,Z")]
    pub(super) pos: Option<String>,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(long)]
    pub(super) tp: bool,
    #[arg(short, long, default_value_t = 30.0)]
    pub(super) timeout: f64,
    #[arg(long, default_value_t = 1.0)]
    pub(super) speed_multiplier: f64,
}

#[derive(Parser)]
pub(super) struct ShotArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short, long, value_name = "PATH", default_value = "shot.png")]
    pub(super) output: PathBuf,
    #[arg(short, long, value_name = "NAME|N")]
    pub(super) player: Option<String>,
    #[arg(long, conflicts_with_all = ["client", "player"])]
    pub(super) studio: bool,
    #[arg(long, conflicts_with = "studio")]
    pub(super) client: bool,
    #[arg(long, value_name = "X,Y,Z", requires = "look_at")]
    pub(super) camera_position: Option<String>,
    #[arg(long, value_name = "X,Y,Z", requires = "camera_position")]
    pub(super) look_at: Option<String>,
}

#[derive(Parser)]
pub(super) struct StudioChangeStateArgs {
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short, long, default_value = "")]
    pub(super) services: String,
    #[arg(long)]
    pub(super) reset: bool,
    #[arg(long)]
    pub(super) replace_services: bool,
    #[arg(long)]
    pub(super) clear_pending: bool,
    #[arg(long)]
    pub(super) no_start: bool,
    #[arg(long)]
    pub(super) stop: bool,
    #[arg(long, value_name = "SEQ")]
    pub(super) ack_seq: Option<u64>,
    #[arg(long, value_name = "IDS", value_delimiter = ',')]
    pub(super) ack_actions: Vec<String>,
    #[arg(long, value_name = "JSON", default_value = "{}")]
    pub(super) ack_action_results: String,
    #[arg(long)]
    pub(super) runtime_id: Option<String>,
    #[arg(long, value_name = "SECONDS")]
    pub(super) suppress_seconds: Option<f64>,
    #[arg(long = "event-wait-seconds", value_name = "SECONDS")]
    pub(super) event_wait_seconds: Option<f64>,
    #[arg(long)]
    pub(super) context_bound: bool,
}

#[derive(Parser)]
pub(super) struct ExportSnapshotsArgs {
    #[arg(
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(long, alias = "src", value_name = "PATH", default_value = "src")]
    pub(super) src_dir: PathBuf,
    #[arg(
        short = 'd',
        long,
        alias = "out",
        value_name = "PATH",
        default_value = "snapshots"
    )]
    pub(super) snapshot_dir: PathBuf,
    #[arg(short, long, default_value = "")]
    pub(super) services: String,
    #[arg(short, long, default_value_t = DEFAULT_EXPORT_CHUNK_SIZE)]
    pub(super) chunk_size: usize,
    #[arg(short, long, alias = "seed", default_value_t = 0)]
    pub(super) adaptive_seed_batch: usize,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(short = 'i', long)]
    pub(super) run_import: bool,
    #[arg(long, alias = "no-import")]
    pub(super) no_run_import: bool,
    #[arg(short = 'm', long, alias = "mode", default_value = "direct")]
    pub(super) import_mode: String,
    #[arg(long, alias = "sw", default_value_t = 0)]
    pub(super) source_workers: usize,
    #[arg(long, alias = "iw", default_value_t = 0)]
    pub(super) instance_workers: usize,
    #[arg(long, alias = "mw", default_value_t = 0)]
    pub(super) import_workers: usize,
    #[arg(long, alias = "perf", default_value = "throughput")]
    pub(super) performance_mode: String,
    #[arg(long, alias = "mdb")]
    pub(super) modified_default_bypass: bool,
    #[arg(long, alias = "no-mdb")]
    pub(super) no_modified_default_bypass: bool,
    #[arg(long)]
    pub(super) no_adaptive_throttle: bool,
    #[arg(long, alias = "all-props")]
    pub(super) export_all_properties: bool,
    #[arg(long, alias = "no-props")]
    pub(super) no_export_all_properties: bool,
    #[arg(short, long)]
    pub(super) quiet_timings: bool,
}

#[derive(Clone, Parser)]
pub(super) struct PushEditorChangesArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
    #[arg(
        short = 'p',
        long = "changed-path",
        alias = "path",
        value_name = "PATH"
    )]
    pub(super) changed_paths: Vec<PathBuf>,
    #[arg(
        short = 'f',
        long = "changed-paths-file",
        alias = "paths-file",
        value_name = "PATH"
    )]
    pub(super) changed_paths_files: Vec<PathBuf>,
    #[arg(short = 'i', long = "target-settings-id", alias = "id")]
    pub(super) target_settings_ids: Vec<String>,
    #[arg(
        short = 'I',
        long = "target-settings-ids-file",
        alias = "ids-file",
        value_name = "PATH"
    )]
    pub(super) target_settings_id_files: Vec<PathBuf>,
    #[arg(short, long = "target-property", alias = "prop")]
    pub(super) target_properties: Vec<String>,
    #[arg(short, long, alias = "upsert")]
    pub(super) upsert_instances_only: bool,
    #[arg(short = 'e', long, alias = "probe")]
    pub(super) probe_events: bool,
    #[arg(long, alias = "verify")]
    pub(super) verify_sources: bool,
    #[arg(long)]
    pub(super) no_review: bool,
    #[arg(long, alias = "apply")]
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
            changed_paths: Vec::new(),
            changed_paths_files: Vec::new(),
            target_settings_ids: Vec::new(),
            target_settings_id_files: Vec::new(),
            target_properties: Vec::new(),
            upsert_instances_only: false,
            probe_events: false,
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
    #[arg(short, long)]
    pub(super) service: String,
    #[arg(short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[arg(short, long, alias = "class", default_value = "")]
    pub(super) class_name: String,
    #[arg(short, long, alias = "path")]
    pub(super) path_segments_json: String,
    #[arg(short = 'o', long, alias = "ords", default_value = "[]")]
    pub(super) path_ordinals_json: String,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct ApplyEditorPropertyArgs {
    #[command(flatten)]
    pub(super) target: EditorMutationArgs,
    #[arg(short = 'S', long, default_value = "property")]
    pub(super) scope: String,
    #[arg(short = 'n', long, alias = "prop")]
    pub(super) property: String,
    #[arg(short = 'j', long, alias = "value")]
    pub(super) value_json: String,
    #[arg(long)]
    pub(super) no_review: bool,
    #[arg(long, alias = "apply")]
    pub(super) yes: bool,
}

#[derive(Parser)]
pub(super) struct ApplyEditorDeleteArgs {
    #[command(flatten)]
    pub(super) target: EditorMutationArgs,
}

#[derive(Parser)]
pub(super) struct EditorRevertArgs {
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub(super) project_root: PathBuf,
    #[arg(long, value_name = "PATH", default_value = "src")]
    pub(super) src_dir: PathBuf,
    #[arg(long)]
    pub(super) path: Option<PathBuf>,
    #[arg(long)]
    pub(super) settings_id: Option<String>,
    #[arg(long)]
    pub(super) service: Option<String>,
    #[arg(long)]
    pub(super) apply_studio: bool,
    #[command(flatten)]
    pub(super) bridge: BridgeConnectionArgs,
}

#[derive(Args, Default)]
pub(super) struct BytecodeFileArgs {
    pub(super) service_or_file: Option<String>,
    #[arg(short = 'f', long, alias = "file", value_name = "PATH")]
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
    #[arg(short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[arg(short = 'x', long)]
    pub(super) index: Option<usize>,
    #[arg(short, long)]
    pub(super) name: Option<String>,
    #[arg(short, long, alias = "class")]
    pub(super) class_name: Option<String>,
    #[arg(long = "path", alias = "path-segments", alias = "path-segments-json")]
    pub(super) path_segments_json: Option<String>,
    #[arg(
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
    #[arg(short, long, alias = "prop")]
    pub(super) property: String,
    #[arg(short = 'S', long, default_value = "auto")]
    pub(super) scope: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeSetPropertyArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(short, long, alias = "prop")]
    pub(super) property: String,
    #[arg(short = 'j', long, alias = "value", alias = "json")]
    pub(super) value_json: Option<String>,
    #[arg(long = "str", alias = "value-str")]
    pub(super) value_str: Option<String>,
    #[arg(long = "num", alias = "value-num")]
    pub(super) value_num: Option<f64>,
    #[arg(long = "bool", alias = "value-bool")]
    pub(super) value_bool: Option<bool>,
    #[arg(long = "null", alias = "value-null")]
    pub(super) value_null: bool,
    #[arg(short = 'S', long, default_value = "auto")]
    pub(super) scope: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeApplyPropertyBatchArgs {
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub(super) project_root: PathBuf,
    #[arg(long, value_name = "PATH")]
    pub(super) input: PathBuf,
    #[arg(long, default_value = "studio-to-files")]
    pub(super) direction: String,
    #[arg(long)]
    pub(super) override_packages: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeSetSourceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long)]
    pub(super) service: Option<String>,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(short = 'j', long, alias = "value", alias = "json")]
    pub(super) value_json: Option<String>,
    #[arg(long = "str", visible_alias = "source", alias = "value-str")]
    pub(super) value_str: Option<String>,
    #[arg(
        help = "Read the source from a file instead of an argument — use this for large scripts that exceed the OS command-line length limit",
        long,
        alias = "src-file",
        value_name = "PATH"
    )]
    pub(super) source_file: Option<PathBuf>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeEditorTargetsArgs {
    #[arg(
        short = 'd',
        long,
        alias = "src",
        value_name = "PATH",
        default_value = "src"
    )]
    pub(super) src_root: PathBuf,
    #[arg(short, long, default_value = "")]
    pub(super) services: String,
    #[arg(short = 'p', long, default_value = "editor:")]
    pub(super) id_prefix: String,
}

#[derive(Parser)]
pub(super) struct BytecodeExplorerBatchArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long, default_value = "")]
    pub(super) service: String,
    #[arg(long)]
    pub(super) project_root: Option<PathBuf>,
    #[arg(
        short = 'j',
        long = "ops",
        alias = "ops-json",
        value_name = "JSON",
        conflicts_with = "ops_file"
    )]
    pub(super) ops_json: Option<String>,
    #[arg(short = 'J', long, value_name = "PATH")]
    pub(super) ops_file: Option<PathBuf>,
    #[arg(short, long, alias = "mode")]
    pub(super) output: Option<String>,
    #[arg(short = 'F', long, alias = "fs")]
    pub(super) fields: Option<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Deserialize)]
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
#[serde(rename_all = "camelCase")]
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
    #[serde(alias = "path", alias = "path_segments")]
    pub(super) path_segments: Option<Vec<String>>,
    #[serde(default, alias = "ords", alias = "path_ordinals")]
    pub(super) path_ordinals: Vec<usize>,
    #[serde(default, alias = "props")]
    pub(super) properties: Vec<String>,
    #[serde(default, alias = "attrs")]
    pub(super) attributes: Vec<String>,
    pub(super) tag: Option<String>,
}

#[derive(Parser)]
pub(super) struct BytecodeAddInstanceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long)]
    pub(super) name: String,
    #[arg(short, long, alias = "class")]
    pub(super) class_name: String,
    #[arg(short = 'i', long, alias = "id")]
    pub(super) settings_id: Option<String>,
    #[command(flatten)]
    pub(super) parent: BytecodeParentArgs,
    #[arg(short, long = "property")]
    pub(super) properties: Vec<String>,
    #[arg(short, long = "attribute")]
    pub(super) attributes: Vec<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Args, Default)]
pub(super) struct BytecodeParentArgs {
    #[arg(short = 'x', long)]
    pub(super) parent_index: Option<usize>,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: Option<String>,
    #[arg(short = 'N', long)]
    pub(super) parent_name: Option<String>,
    #[arg(short = 'C', long, alias = "parent-class")]
    pub(super) parent_class_name: Option<String>,
    #[arg(long, alias = "root")]
    pub(super) no_parent: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeCloneInstanceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long, default_value = "")]
    pub(super) service: String,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(short = 'X', long)]
    pub(super) parent_index: Option<usize>,
    #[arg(short = 'I', long, alias = "parent-id")]
    pub(super) parent_settings_id: Option<String>,
    #[arg(short = 'N', long)]
    pub(super) parent_name: Option<String>,
    #[arg(short = 'C', long, alias = "parent-class")]
    pub(super) parent_class_name: Option<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeRemoveInstanceArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(short = 'R', long)]
    pub(super) no_recursive: bool,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeDesyncPackageLinkArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long, default_value = "")]
    pub(super) service: String,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeExportModelArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long, default_value = "")]
    pub(super) service: String,
    #[command(flatten)]
    pub(super) selector: BytecodeInstanceSelectorArgs,
    #[arg(short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(long, value_name = "rbxm|rbxmx")]
    pub(super) format: Option<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeExportPlaceArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(short, long, default_value = "")]
    pub(super) services: String,
    #[arg(short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(long, value_name = "rbxl|rbxlx")]
    pub(super) format: Option<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct PlaceDesyncPackageLinkArgs {
    #[arg(short, long, value_name = "PATH")]
    pub(super) input: PathBuf,
    #[arg(short, long, value_name = "PATH")]
    pub(super) output: PathBuf,
    #[arg(long = "path", alias = "path-segments", alias = "path-segments-json")]
    pub(super) path_segments_json: String,
    #[arg(
        long = "ords",
        alias = "path-ordinals",
        alias = "path-ordinals-json",
        default_value = "[]"
    )]
    pub(super) path_ordinals_json: String,
    #[arg(long, value_name = "rbxl|rbxlx")]
    pub(super) output_format: Option<String>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeImportModelArgs {
    #[command(flatten)]
    pub(super) input: BytecodeFileArgs,
    #[arg(short, long, default_value = "")]
    pub(super) service: String,
    #[arg(short, long, value_name = "PATH")]
    pub(super) model: PathBuf,
    #[command(flatten)]
    pub(super) parent: BytecodeParentArgs,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct SyncWallyPackagesArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, value_name = "PATH", default_value = "wally.toml")]
    pub(super) manifest: PathBuf,
    #[arg(long, value_name = "COMMAND", default_value = "wally")]
    pub(super) wally_path: String,
    #[arg(long, value_name = "PATH", default_value = "Packages")]
    pub(super) packages_dir: PathBuf,
    #[arg(long, default_value = "ReplicatedStorage")]
    pub(super) target_service: String,
    #[arg(long, default_value = "Packages")]
    pub(super) target_name: String,
    #[arg(
        help = "Comma list of realms to import: shared, server, dev. Server/dev are imported only when their package directory exists",
        long,
        value_name = "LIST",
        default_value = "shared,server,dev"
    )]
    pub(super) realms: String,
    #[arg(long, value_name = "PATH", default_value = "ServerPackages")]
    pub(super) server_packages_dir: PathBuf,
    #[arg(long, default_value = "ServerStorage")]
    pub(super) server_target_service: String,
    #[arg(long, default_value = "ServerPackages")]
    pub(super) server_target_name: String,
    #[arg(long, value_name = "PATH", default_value = "DevPackages")]
    pub(super) dev_packages_dir: PathBuf,
    #[arg(long, default_value = "ReplicatedStorage")]
    pub(super) dev_target_service: String,
    #[arg(long, default_value = "DevPackages")]
    pub(super) dev_target_name: String,
    #[arg(
        help = "Re-import even when wally.lock is unchanged since the last sync",
        long
    )]
    pub(super) force: bool,
    #[arg(long)]
    pub(super) skip_install: bool,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkApplyArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
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
    #[arg(long, value_name = "COMMAND", default_value = "git")]
    pub(super) git_path: String,
    #[arg(long, value_name = "COMMAND", default_value = "wally")]
    pub(super) wally_path: String,
    #[arg(
        help = "Where cloned git/wally sources are cached. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) cache_dir: Option<PathBuf>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkBreakArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
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
        help = "Where cloned git/wally sources are cached. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) cache_dir: Option<PathBuf>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkStatusArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
    pub(super) manifest: PathBuf,
    #[arg(
        help = "Where cloned git/wally sources are cached. Overrides the manifest cacheDir",
        long,
        value_name = "PATH"
    )]
    pub(super) cache_dir: Option<PathBuf>,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct LinkAddArgs {
    #[arg(
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
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
    #[arg(long)]
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
        short = 'r',
        long,
        alias = "root",
        value_name = "PATH",
        default_value = "."
    )]
    pub(super) project_root: PathBuf,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
    pub(super) manifest: PathBuf,
    #[arg(long, value_name = "SERVICE")]
    pub(super) old_service: String,
    #[arg(long = "old-path", value_name = "JSON")]
    pub(super) old_path_segments_json: String,
    #[arg(long = "old-ords", value_name = "JSON", default_value = "[]")]
    pub(super) old_path_ordinals_json: String,
    #[arg(long, value_name = "SERVICE")]
    pub(super) new_service: String,
    #[arg(long = "new-path", value_name = "JSON")]
    pub(super) new_path_segments_json: String,
    #[arg(long = "new-ords", value_name = "JSON", default_value = "[]")]
    pub(super) new_path_ordinals_json: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
#[command(
    about = "Pack an existing instance subtree into a reusable bytecode package and register it as a link target"
)]
pub(super) struct LinkPackArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
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
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
#[command(about = "Delete a bytecode package link and optionally resolve/delete existing uses")]
pub(super) struct LinkDeletePackageArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(long, value_name = "PATH", default_value = "renium-link.json")]
    pub(super) manifest: PathBuf,
    #[arg(help = "Package / link id to delete", long)]
    pub(super) id: String,
    #[arg(
        help = "delete-unused | delete-uses | unlink-uses",
        long,
        default_value = "delete-unused"
    )]
    pub(super) action: String,
    #[arg(long)]
    pub(super) pretty: bool,
}

#[derive(Parser)]
pub(super) struct BytecodeRepackArgs {
    #[command(flatten)]
    pub(super) project: ProjectSourceArgs,
    #[arg(value_name = "SERVICE_OR_FILE")]
    pub(super) paths: Vec<PathBuf>,
    #[arg(long)]
    pub(super) pretty: bool,
}
