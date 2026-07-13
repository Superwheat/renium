import * as childProcess from "child_process";
import * as crypto from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { URLSearchParams } from "url";
import * as vscode from "vscode";
import { mergeAndResolve, type ConflictPolicy } from "./conflictMerge";
import {
  FileExplorerController,
  iconAssetNameForClass,
  logPackageDragDebug,
  loadAssetIconNames,
} from "./fileExplorer";
import {
  GitNameStatusEntry,
  GitRunResult,
  GitStatusEntry,
  buildCommitMessage,
  defaultGitSyncScope,
  nameStatusAffectedPaths,
  parseAheadBehind,
  parseNameStatusZ,
  parsePorcelainV1Z,
  redactRemoteUrl,
  remoteUrlToWebUrl,
  renderGitArgs,
  runGit,
  summarizeStatus,
} from "./gitSync";
import { GitViewActions, GitViewState } from "./gitView";
import { pickWorkspaceRoot } from "./utils";
import { DEFAULT_SYNC_SERVICES } from "./serviceDefaults";
import { RbsyncEditorProvider } from "./rbsyncViewer";

const RENIUM_PACKAGE_DRAG_MIME = "application/vnd.renium.package";
const RENIUM_PACKAGE_TEXT_PREFIX = "renium-package:";
const RENIUM_OPEN_PACKAGE_SCRIPT_TABS_STATE_KEY = "renium.openPackageScriptTabs";

type GitSyncConfig = {
  gitPath: string;
  remote: string;
  branch: string;
  autoFetch: boolean;
  runFullSyncBeforePush: "ask" | "always" | "never";
  stageMode: "tracked" | "configuredPaths";
  stagePaths: string[];
  includeUntracked: boolean;
  commitMessageTemplate: string;
  confirmBeforePush: boolean;
  requireCleanWorktreeBeforePull: boolean;
  applyPulledChangesToStudio: "ask" | "always" | "never";
  timeoutSeconds: number;
  outputBehavior: "onStart" | "onError" | "silent";
};

type WallySyncConfig = {
  wallyPath: string;
  rojoPath: string;
  packagesDir: string;
  targetService: string;
  targetName: string;
  realms: string;
  runInstall: boolean;
  applyToStudio: "ask" | "always" | "never";
};

type LinkSyncConfig = {
  manifest: string;
  folder: string;
  cacheDir: string;
  gitPath: string;
  wallyPath: string;
  offline: boolean;
  autoApply: boolean;
  applyToStudio: "ask" | "always" | "never";
};

type ReniumLogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";
type InitialSyncPriority = "studio" | "editor" | "none";
type DisplayPrompts = "always" | "initial" | "never";

type SyncConfig = {
  exportCliPath: string;
  editorSyncCliPath: string;
  rustCliPath: string;
  projectRoot: string;
  snapshotDir: string;
  transport: "ws" | "mcp";
  server: string;
  configTomlPath: string;
  services: string[];
  sourceWorkers: number;
  instanceWorkers: number;
  importWorkers: number;
  chunkSize: number;
  snapshotInstanceChunkSize: number;
  bridgeWaitSeconds: number;
  bridgePorts: string;
  usePersistentBridge: boolean;
  verifyEditorPushSources: boolean;
  adaptiveThrottle: boolean;
  noUpdateEditorIcons: boolean;
  autoSyncOnSave: boolean;
  autoSyncDebounceMs: number;
  editorLiveSyncEnabled: boolean;
  editorLiveSyncOnStartup: boolean;
  studioLiveSyncEnabled: boolean;
  studioLiveSyncPollMs: number;
  initialSyncPriority: InitialSyncPriority;
  changesThreshold: number;
  diffLinesLimit: number;
  displayPrompts: DisplayPrompts;
  logLevel: ReniumLogLevel;
  overridePackages: boolean;
  conflictResolution: ConflictPolicy;
  runImport: boolean;
  importMode: "direct" | "snapshot";
  performanceMode: "throughput" | "balanced" | "smooth";
  modifiedDefaultBypass: boolean;
  watchConfigPath: string;
  wsWaitSeconds: number;
  progressHeartbeatSeconds: number;
  benchmarkRuns: number;
  gitSync: GitSyncConfig;
  wallySync: WallySyncConfig;
  linkSync: LinkSyncConfig;
};

type CommandRunResult = {
  code: number;
  output: string;
};

type RobloxPlaceFormat = "rbxl" | "rbxlx";

type CliExportGameFileResult = {
  ok?: boolean;
  output?: string;
  format?: string;
  services?: string[];
  serviceCount?: number;
  instances?: number;
};

type CliWallySourceWrite = {
  settingsId?: string;
  path?: string;
};

type CliWallyRemovedTarget = {
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
};

type CliWallyRealmResult = {
  realm?: string;
  service?: string;
  targetName?: string;
  settingsFile?: string;
  settingsIds?: string[];
  sourceWrites?: CliWallySourceWrite[];
  removedTarget?: CliWallyRemovedTarget | null;
  skipped?: boolean;
};

type CliSyncWallyPackagesResult = {
  ok?: boolean;
  settingsFile?: string;
  service?: string;
  targetName?: string;
  appliedRealms?: number;
  skippedRealms?: number;
  rootSettingsIds?: string[];
  settingsIds?: string[];
  sourceWrites?: CliWallySourceWrite[];
  changedPaths?: string[];
  targetSettingsIds?: string[];
  removedTarget?: CliWallyRemovedTarget | null;
  removedTargets?: CliWallyRemovedTarget[];
  realms?: CliWallyRealmResult[];
};

type CliLinkApplyResult = {
  ok?: boolean;
  check?: boolean;
  appliedTargets?: number;
  driftedFiles?: number;
  changedPaths?: string[];
  targetSettingsIds?: string[];
  warnings?: string[];
};

type CliLinkStatusMirror = {
  path?: string;
  canonical?: string;
  drift?: boolean;
  exists?: boolean;
};

type CliLinkStatusTarget = {
  linkId?: string;
  service?: string;
  path?: string[];
  pathKey?: string;
  readOnly?: boolean;
  broken?: boolean;
  resolved?: boolean;
  resolvedRef?: string | null;
  drift?: boolean;
  missing?: boolean;
  files?: number;
  mirrors?: CliLinkStatusMirror[];
  reason?: string | null;
};

export type LinkFileInfo = {
  linkId: string;
  service: string;
  pathSegments: string[];
  canonical?: string;
  readOnly: boolean;
  broken: boolean;
  drift: boolean;
};

export type CliLinkStatusLink = {
  id?: string;
  readOnly?: boolean;
  sourceKind?: string;
  source?: string;
  sourcePath?: string;
  targetCount?: number;
  isPackage?: boolean;
  rootClass?: string | null;
  rootName?: string | null;
  instances?: number;
  updatedUnixMs?: number | null;
};

type CliLinkStatusResult = {
  ok?: boolean;
  manifest?: string;
  manifestExists?: boolean;
  linkCount?: number;
  brokenTargets?: number;
  driftedTargets?: number;
  links?: CliLinkStatusLink[];
  targets?: CliLinkStatusTarget[];
};

type CliLinkDeletePackageResult = {
  ok?: boolean;
  id?: string;
  activeUses?: number;
  deletedPackage?: string | null;
  deletedTargets?: unknown[];
  unlinkedTargets?: unknown[];
  missingTargets?: unknown[];
  removedSourcePaths?: string[];
  externalizedSourcePaths?: string[];
  changedPaths?: string[];
  services?: string[];
};

type PackagePreviewNode = {
  settingsId?: string;
  name?: string;
  className?: string;
  parentId?: string;
  childCount?: number;
  pathSegments?: string[];
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
};

type PackagePreviewData = {
  id: string;
  name: string;
  source?: string;
  sourcePath: string;
  rootClass?: string | null;
  rootName?: string | null;
  nodes: PackagePreviewNode[];
  rootIds: string[];
};

type LinkedPackageScriptPreviewRequest = {
  service?: string;
  pathSegments?: string[];
  className?: string;
  name?: string;
};

type OpenPackageScriptTab = {
  linkId: string;
  nodeKey: string;
};

type GitRepoState = {
  view: GitViewState;
  entries: GitStatusEntry[];
  repoRoot?: string;
  branch?: string;
  upstream?: string;
  remote?: string;
  remoteUrl?: string;
  ahead: number;
  behind: number;
};

type StudioChangeState = {
  ok?: boolean;
  tracking?: boolean;
  role?: string;
  seq?: number;
  dirtyServices?: string[];
  fullSyncServices?: string[];
  propertyChanges?: StudioPropertyChange[];
  changes?: StudioChangeLog[];
  trackedServices?: number;
  itemChangedAvailable?: boolean;
  eventDriven?: boolean;
  waitSeconds?: number;
  waitTimedOut?: boolean;
  twoWaySyncEnabled?: boolean;
  runtimeSettings?: Record<string, unknown>;
  conflictResolution?: string;
};

type StudioPropertyChange = {
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  scope?: "metadata" | "property" | "attribute";
  property?: string;
  value?: unknown;
  seq?: number;
};

type StudioChangeLog = {
  service?: string;
  action?: string;
  reason?: string;
  className?: string;
  path?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  property?: string;
  attribute?: string;
  direct?: boolean;
  fullSync?: boolean;
  seq?: number;
};

type StudioSnapshotDiff = {
  changedServices: string[];
  fingerprintsByService: Map<string, string>;
};

type EditorPushOptions = {
  force?: boolean;
  verifySources?: boolean;
  skipChangeFilter?: boolean;
  taskName?: string;
  targetSettingsId?: string;
  targetSettingsIds?: string[];
  targetProperty?: string;
  targetProperties?: string[];
  upsertInstancesOnly?: boolean;
};

type EditorPropertyPushRequest = {
  force?: boolean;
  settingsFile?: string;
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  scope?: "metadata" | "property" | "attribute";
  property?: string;
  value?: unknown;
  allowProtectedMeshIdApply?: boolean;
};

type EditorDeletePushRequest = {
  force?: boolean;
  settingsFile?: string;
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
};

type ProgrammaticEditorWriteRequest = {
  paths?: string[] | string;
  durationMs?: number;
  refreshCache?: boolean;
};

type EditorLiveSyncHashCache = {
  version: number;
  projectRoot: string;
  updatedAtUnixMs: number;
  files: Record<string, string>;
};

type SourcemapNode = {
  name?: string;
  className?: string;
  filePaths?: unknown;
  children?: unknown;
};

type SourcemapCache = {
  path: string;
  mtimeMs: number;
  root: SourcemapNode;
};

type DaemonPendingRequest = {
  id: number;
  label: string;
  launchedAt: number;
  lastOutputAt: number;
  sawOutput: boolean;
  output: string;
  resolve: (result: CommandRunResult) => void;
  reject: (err: Error) => void;
  heartbeatTimer: NodeJS.Timeout | undefined;
  timeoutTimer: NodeJS.Timeout | undefined;
  quiet: boolean;
};

type PluginProfileOperation = {
  calls?: number;
  totalUs?: number;
  avgUs?: number;
  perCallUs?: number;
  p50Us?: number;
  p90Us?: number;
  emptyAvgUs?: number;
  skipped?: boolean;
  error?: string;
  reason?: string;
};

type PluginProfileResult = {
  service?: string;
  profile?: {
    service?: string;
    instanceCount?: number;
    sampleCount?: number;
    iterations?: number;
    projectedServerStoragePropertyReads?: number;
  };
  operations?: Record<string, PluginProfileOperation>;
};

type BenchmarkRunMetrics = {
  totalMs?: number;
  trackedService?: string;
  coreExportMs?: number;
  bridgeStartupMs?: number;
  handshakeMs?: number;
  serviceExportSumMs?: number;
  importCriticalTailMs?: number;
  unmeasuredOrSchedulerGapMs?: number;
  trackedServiceInstanceFetchMs?: number;
  trackedServicePluginServerMs?: number;
  trackedServicePluginEncodeMs?: number;
  trackedServicePayloadBytes?: number;
  trackedServiceChunkCount?: number;
  trackedServiceMaxFrameMs?: number;
  trackedServiceStallCountOver33Ms?: number;
  trackedServiceStallCountOver50Ms?: number;
  trackedServiceStallCountOver100Ms?: number;
  exportFingerprint?: string;
  bridgeFingerprint?: string;
  serviceMetrics?: BenchmarkServiceMetrics[];
};

type BenchmarkServiceMetrics = {
  service: string;
  instanceFetchMs?: number;
  pluginServerMs?: number;
  pluginEncodeMs?: number;
  payloadBytes?: number;
  chunkCount?: number;
  maxFrameMs?: number;
  stallCountOver33Ms?: number;
  stallCountOver50Ms?: number;
  stallCountOver100Ms?: number;
};

const DEFAULT_SERVICES = [...DEFAULT_SYNC_SERVICES];

const DEFAULT_BRIDGE_PORTS = [8781, 8782, 8783];
const PREVIOUS_DEFAULT_BRIDGE_PORTS = [8781, 8782, 8783, 8784];
const LEGACY_BRIDGE_PORTS = [8781, 8782, 8783, 8784, 8785, 8786, 8787, 8788];
const DEFAULT_CHUNK_SIZE = 4 * 1024 * 1024;
const MAX_BRIDGE_CHUNK_SIZE = 8 * 1024 * 1024;
const SETTINGS_FILE_NAME = "__roblox_sync_settings.renium";
const LEGACY_SETTINGS_FILE_NAME = "__roblox_sync_settings.rbsync";

function isReniumSettingsFileName(fileName: string): boolean {
  const normalized = fileName.toLowerCase();
  return normalized === SETTINGS_FILE_NAME || normalized === LEGACY_SETTINGS_FILE_NAME;
}

function isCanonicalReniumSettingsFileName(fileName: string): boolean {
  return fileName.toLowerCase() === SETTINGS_FILE_NAME;
}

function existingReniumSettingsFile(projectRoot: string, service: string): string {
  const serviceDir = path.join(projectRoot, "src", service);
  const canonical = path.join(serviceDir, SETTINGS_FILE_NAME);
  const legacy = path.join(serviceDir, LEGACY_SETTINGS_FILE_NAME);
  return fs.existsSync(canonical) || !fs.existsSync(legacy) ? canonical : legacy;
}
const RUST_CLI_BINARY = process.platform === "win32" ? "renium.exe" : "renium";
const DEFAULT_RUST_CLI_RELATIVE_PATH = RUST_CLI_BINARY;
const RUST_CLI_FALLBACK_RELATIVE_PATHS = [
  DEFAULT_RUST_CLI_RELATIVE_PATH,
  `bin/${RUST_CLI_BINARY}`,
  `tools/renium/target/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-pi-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-drop-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-rename-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target-resave-release/release/${RUST_CLI_BINARY}`,
  `tools/renium/target/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-pi-release/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-drop-release/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-rename-release/debug/${RUST_CLI_BINARY}`,
  `tools/renium/target-resave-release/debug/${RUST_CLI_BINARY}`,
];
const DEFAULT_STUDIO_LIVE_SYNC_POLL_MS = 250;
const MIN_STUDIO_LIVE_SYNC_POLL_MS = 10;
const MAX_STUDIO_LIVE_SYNC_EVENT_WAIT_MS = 150;
const MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS = 2000;
const MAX_STUDIO_LIVE_SYNC_ERROR_POLL_MS = 5000;
const STUDIO_LIVE_SYNC_POLL_BACKOFF_MULTIPLIER = 1.75;
const EDITOR_PUSH_RETRY_BASE_MS = 500;
const EDITOR_PUSH_RETRY_MAX_MS = 10_000;
const DEFAULT_COMMAND_TIMEOUT_MS = 30 * 60 * 1000;
const MAX_COMMAND_TIMEOUT_MS = 30 * 60 * 1000;
const MAX_DAEMON_OUTPUT_BUFFER_BYTES = 1024 * 1024;
const DAEMON_CHANNEL_WAIT_MAX_MS = 30_000;
const rustCliHelpCache = new Map<string, { mtimeMs: number; helpText?: string }>();

const TRANSIENT_SNAPSHOT_PROPERTY_NAMES = new Set([
  "absoluteposition",
  "absoluterotation",
  "absolutesize",
  "absolutecanvassize",
  "absolutewindowsize",
  "absolutecontentsize",
  "absolutecellcount",
  "absolutecellsize",
  "absolutepositionwrite",
  "absolutesizewrite",
  "arehingesdetected",
  "channelcount",
  "datamodelplaceversion",
  "floormaterial",
  "ispaused",
  "issmooth",
  "isspatial",
  "lastusedmodificationmethod",
  "localizedtext",
  "localizationmatchedsourcetext",
  "localizationmatchidentifier",
  "maxextents",
  "movedirection",
  "movedirectioninternal",
  "occupant",
  "opentypefeatureserror",
  "physicsreprrootpart",
  "rolloffgain",
  "rootpart",
  "seatpart",
  "steer",
  "terrain",
  "throttle",
  "timeposition",
  "timepositionreplicating",
  "timepositionreplicator",
  "resolution",
  "walkdirection",
  "weightcurrent",
  "weighttarget",
  "contenttext",
  "textbounds",
  "textfits",
  "assemblyangularvelocity",
  "assemblylinearvelocity",
  "assemblycenterofmass",
  "assemblymass",
  "assemblyrootpart",
  "centerofmass",
  "currentcamera",
  "currentphysicalproperties",
  "distributedgametime",
  "extentscframe",
  "extentssize",
  "isloaded",
  "isplaying",
  "mass",
  "networkissleeping",
  "playbackloudness",
  "receiveage",
  "rotvelocity",
  "timelength",
  "velocity",
]);

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isTransientBridgeFailure(output: string): boolean {
  return [
    "Bridge call failed",
    "Bridge send failed",
    "Bridge read failed",
    "Bridge closed while waiting",
    "closed before hello",
    "failed waiting for hello",
    "No plugin bridge channels connected",
    "Only ",
    "proceeding with",
  ].some((needle) => output.includes(needle));
}

function safeFileComponent(value: unknown): string {
  const cleaned = String(value ?? "item")
    .trim()
    .replace(/[^A-Za-z0-9._-]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 80);
  return cleaned || "item";
}

function safePlaceFileName(name: string, format: RobloxPlaceFormat): string {
  return `${safeFileComponent(name || "Game")}.${format}`;
}

function ensurePlaceFileExtension(filePath: string, format: RobloxPlaceFormat): string {
  const expected = `.${format}`;
  const current = path.extname(filePath).toLowerCase();
  if (current === expected) {
    return filePath;
  }
  if (current === ".rbxl" || current === ".rbxlx") {
    return `${filePath.slice(0, -current.length)}${expected}`;
  }
  return `${filePath}${expected}`;
}

function robloxPlaceFormatFromPath(filePath: string): RobloxPlaceFormat | undefined {
  const extension = path.extname(filePath).toLowerCase();
  if (extension === ".rbxl" || extension === ".rbxlx") {
    return extension.slice(1) as RobloxPlaceFormat;
  }
  return undefined;
}

function rustCliHelpText(cliPath: string): string | undefined {
  try {
    const stat = fs.statSync(cliPath);
    const cached = rustCliHelpCache.get(cliPath);
    if (cached && cached.mtimeMs === stat.mtimeMs) {
      return cached.helpText;
    }
    const result = childProcess.spawnSync(cliPath, ["--help"], {
      cwd: path.dirname(cliPath),
      encoding: "utf8",
      shell: false,
      windowsHide: true,
    });
    const helpText = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
    rustCliHelpCache.set(cliPath, { mtimeMs: stat.mtimeMs, helpText });
    return helpText;
  } catch {
    return undefined;
  }
}

function rustCliSupportsCommand(cliPath: string, command: string): boolean {
  if (!fs.existsSync(cliPath)) {
    return false;
  }
  const helpText = rustCliHelpText(cliPath);
  return helpText !== undefined && helpText.includes(command);
}

function rustCliVersion(cliPath: string): string | undefined {
  try {
    const result = childProcess.spawnSync(cliPath, ["--version"], {
      encoding: "utf8",
      shell: false,
      windowsHide: true,
    });
    const match = `${result.stdout ?? ""}`.match(/(\d+\.\d+\.\d+)/);
    return match ? match[1] : undefined;
  } catch {
    return undefined;
  }
}

function resolveExistingRustCliPath(workspaceRoot: string, projectRoot: string, configuredPath: string): string {
  const roots = Array.from(new Set([workspaceRoot, projectRoot].map((value) => path.normalize(value))));
  const candidates = [
    configuredPath,
    ...roots.flatMap((root) => RUST_CLI_FALLBACK_RELATIVE_PATHS.map((relativePath) => path.normalize(path.join(root, relativePath)))),
  ];
  const uniqueCandidates = Array.from(new Set(candidates.map((candidate) => path.normalize(candidate))));

  if (fs.existsSync(configuredPath)) {
    return configuredPath;
  }

  const existingCandidates = uniqueCandidates.filter((candidate) => fs.existsSync(candidate));
  if (existingCandidates.length === 0) {
    return configuredPath;
  }

  existingCandidates.sort((left, right) => fs.statSync(right).mtimeMs - fs.statSync(left).mtimeMs);
  return existingCandidates[0];
}

class RobloxSyncController {
  private readonly output: vscode.OutputChannel;
  private readonly statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 200);
  private queue: Promise<void> = Promise.resolve();
  private liveSyncWatcher: vscode.FileSystemWatcher | undefined;
  private liveSyncTimer: NodeJS.Timeout | undefined;
  private liveSyncTimerDueAt = 0;
  private studioLiveSyncTimer: NodeJS.Timeout | undefined;
  private studioLiveSyncInFlight = false;
  private changePreviewPanel: vscode.WebviewPanel | undefined;
  private changePreviewResolve: ((decision: "apply" | "full" | "discard") => void) | undefined;
  private changePreviewIconNames: ReadonlySet<string> | undefined;
  private studioLiveSyncStarted = false;
  private studioLiveSyncNextPollMs = DEFAULT_STUDIO_LIVE_SYNC_POLL_MS;
  private studioToEditorImportInProgress = false;
  private studioToEditorImportSuppressUntilMs = 0;
  private studioToEditorLastSyncEndedAt = 0;
  private studioSnapshotFingerprintByService = new Map<string, string>();
  private editorLiveSyncRuntimeEnabled = false;
  private pendingEditorPaths = new Set<string>();
  private editorPushFailureStreak = 0;
  private forcedEditorLiveSyncPathKeys = new Set<string>();
  private suppressedEditorLiveSyncPathUntilByKey = new Map<string, number>();
  private recentDirectSaveAtByPath = new Map<string, number>();
  private studioConflictPolicyOverride: ConflictPolicy | undefined;
  private studioRuntimeSettings: Record<string, unknown> | undefined;
  private conflictMarkerWarnedKeys = new Set<string>();
  private linkStatusCache: { at: number; value: CliLinkStatusResult | undefined } | undefined;
  private linkStatusInflight: Promise<CliLinkStatusResult | undefined> | undefined;
  private linkPackageSourceApplyTimer: NodeJS.Timeout | undefined;
  private readonly pendingLinkPackageSourcePaths = new Set<string>();
  private readonly linkChangeEmitter = new vscode.EventEmitter<void>();
  public readonly onLinksChanged = this.linkChangeEmitter.event;
  private daemonProcess: childProcess.ChildProcessWithoutNullStreams | undefined;
  private daemonKeyValue: string | undefined;
  private daemonRequestId = 1;
  private daemonOutputBuffer = "";
  private daemonReady = false;
  private daemonReadyPromise: Promise<void> | undefined;
  private daemonReadyResolve: (() => void) | undefined;
  private daemonReadyReject: ((err: Error) => void) | undefined;
  private daemonPending = new Map<number, DaemonPendingRequest>();
  private bridgeServeRequested = false;
  private liveSyncOwnsServe = false;
  private liveSyncStartPromise: Promise<void> | undefined;
  private liveSyncStartupInProgress = false;
  private liveSyncStopRequested = false;
  private autoSyncTimer: NodeJS.Timeout | undefined;
  private pendingAutoServices = new Set<string>();
  private activeTaskName: string | undefined;
  private activeTaskStartedAt = 0;
  private activeTaskTicker: NodeJS.Timeout | undefined;
  private warnedLegacyStartupWaitSeconds = false;
  private warnedMultiRootWorkspace = false;
  private warnedLegacyBridgePorts = false;
  private warnedBridgePortLimit = false;
  private warnedLegacyChunkSize = false;
  private warnedChunkSizeCap = false;
  private sourcemapCache: SourcemapCache | undefined;
  private displayedLiveSyncPrompt = false;

  public constructor(private readonly context: vscode.ExtensionContext) {
    const output = vscode.window.createOutputChannel("Renium");
    const appendLine = output.appendLine.bind(output);
    output.appendLine = (value: string): void => {
      if (this.shouldWriteOutput(this.outputLevel(value))) {
        appendLine(value);
      }
    };
    this.output = output;
    this.statusItem.command = "renium.openMenu";
    this.statusItem.show();
    this.updateStatusBar();
  }

  private configuredLogLevel(): ReniumLogLevel {
    const runtimeLevel = this.studioRuntimeSettings?.logLevel;
    const raw = String(
      typeof runtimeLevel === "string"
        ? runtimeLevel
        : vscode.workspace.getConfiguration("renium").get<string>("logLevel", "info") ?? "info",
    ).toLowerCase();
    switch (raw) {
      case "off":
      case "error":
      case "warn":
      case "info":
      case "debug":
      case "trace":
        return raw;
      default:
        return "info";
    }
  }

  private outputLevel(message: string): Exclude<ReniumLogLevel, "off"> {
    const normalized = message.toLowerCase();
    if (normalized.includes("[trace]") || normalized.includes(" trace:")) {
      return "trace";
    }
    if (normalized.includes("[debug]") || normalized.includes(" debug:")) {
      return "debug";
    }
    if (normalized.includes("warning") || normalized.includes(" warn") || normalized.includes("conflict")) {
      return "warn";
    }
    if (normalized.includes(" failed") || normalized.includes(" error") || normalized.includes("rejected") || normalized.includes("could not")) {
      return "error";
    }
    return "info";
  }

  private shouldWriteOutput(level: Exclude<ReniumLogLevel, "off">): boolean {
    const minimum = this.configuredLogLevel();
    if (minimum === "off") {
      return false;
    }
    const rank: Record<Exclude<ReniumLogLevel, "off">, number> = {
      error: 0,
      warn: 1,
      info: 2,
      debug: 3,
      trace: 4,
    };
    return rank[level] <= rank[minimum];
  }

  public gitViewActions(): GitViewActions {
    return {
      refresh: (options) => this.getGitViewState(options),
      runAction: (action) => this.runGitViewAction(action),
      openOutput: () => this.output.show(true),
      openDiff: (filePath) => this.openGitDiff(filePath),
    };
  }

  private gitHeadProviderRegistered = false;

  /** Lazily register a read-only content provider that serves a file's HEAD version. */
  private ensureGitHeadProvider(): void {
    if (this.gitHeadProviderRegistered) {
      return;
    }
    this.gitHeadProviderRegistered = true;
    const provider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent: async (uri) => {
        try {
          const repoRoot = new URLSearchParams(uri.query).get("root") ?? "";
          const relPath = uri.path.replace(/^\/+/, "");
          if (!repoRoot || !relPath) {
            return "";
          }
          return await this.gitOutput(this.getConfig(), repoRoot, ["show", `HEAD:${relPath}`], "read HEAD version");
        } catch {
          return "";
        }
      },
    };
    this.context.subscriptions.push(
      vscode.workspace.registerTextDocumentContentProvider("renium-githead", provider),
    );
  }

  /** Open a working-tree-vs-HEAD diff for a changed file from the Git tab. */
  private async openGitDiff(filePath: string): Promise<void> {
    const requested = String(filePath ?? "").trim();
    if (!requested) {
      return;
    }
    this.ensureGitHeadProvider();
    const cfg = this.getConfig();
    let repoRoot: string;
    try {
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      repoRoot = this.requireGitRepoRoot(state);
    } catch (err) {
      vscode.window.showErrorMessage(`Renium: cannot open diff. ${err instanceof Error ? err.message : String(err)}`);
      return;
    }
    const absFile = path.isAbsolute(requested) ? requested : path.join(repoRoot, requested);
    const relForGit = path.relative(repoRoot, absFile).split(path.sep).join("/");
    const title = `${path.basename(absFile)} (HEAD ↔ Working Tree)`;
    const headUri = vscode.Uri.from({
      scheme: "renium-githead",
      path: `/${relForGit}`,
      query: `root=${encodeURIComponent(repoRoot)}&t=${Date.now()}`,
    });
    if (!fs.existsSync(absFile)) {
      await vscode.window.showTextDocument(headUri, { preview: true });
      return;
    }
    await vscode.commands.executeCommand("vscode.diff", headUri, vscode.Uri.file(absFile), title);
  }

  public dispose(): void {
    if (this.autoSyncTimer) {
      clearTimeout(this.autoSyncTimer);
      this.autoSyncTimer = undefined;
    }
    if (this.activeTaskTicker) {
      clearInterval(this.activeTaskTicker);
      this.activeTaskTicker = undefined;
    }

    if (this.liveSyncTimer) {
      clearTimeout(this.liveSyncTimer);
      this.liveSyncTimer = undefined;
      this.liveSyncTimerDueAt = 0;
    }
    if (this.studioLiveSyncTimer) {
      clearTimeout(this.studioLiveSyncTimer);
      this.studioLiveSyncTimer = undefined;
    }
    if (this.liveSyncWatcher) {
      this.liveSyncWatcher.dispose();
      this.liveSyncWatcher = undefined;
    }

    this.stopBridgeDaemon();

    this.statusItem.dispose();
    this.output.dispose();
  }

  private isEditorLiveSyncActive(): boolean {
    const cfg = this.getConfig();
    return cfg.editorLiveSyncEnabled && this.liveSyncWatcher !== undefined;
  }

  private canUseStudioPushPipeline(): boolean {
    const cfg = this.getConfig();
    if (this.isEditorLiveSyncActive()) {
      return true;
    }
    if (cfg.transport !== "ws") {
      return this.bridgeServeRequested;
    }
    return this.bridgeServeRequested && this.isBridgeDaemonRunning();
  }

  private noteStudioPushSkipped(reason: string): void {
    this.output.appendLine(`[renium] Studio push skipped: ${reason}`);
  }

  public async openMenu(): Promise<void> {
    const cfg = this.getConfig();
    const liveSyncRunning = cfg.editorLiveSyncEnabled || this.liveSyncWatcher !== undefined || this.liveSyncStartPromise !== undefined;
    const serving = this.bridgeServeRequested && this.isBridgeDaemonRunning();
    const items: Array<vscode.QuickPickItem & { action: string }> = [
      {
        label: "$(sync) Full Sync (Studio -> src)",
        description: "Exports from Studio, updates src, writes generated project JSON",
        action: "fullSync",
      },
      {
        label: "$(export) Export Snapshots Only",
        description: "Studio -> snapshots",
        action: "exportOnly",
      },
      {
        label: "$(save) Export Game File...",
        description: "Write a .rbxl/.rbxlx place file from src",
        action: "exportGameFile",
      },
      {
        label: "$(package) Sync Wally Packages",
        description: "Install Wally packages and import them into the configured package target",
        action: "wallyPackages",
      },
      {
        label: "$(link) Sync Link Mirrors",
        description: "Rebuild link targets in src from local, git, Wally, or package sources",
        action: "linkApply",
      },
      {
        label: "$(add) Add Renium Link",
        description: "Control one source script from multiple places",
        action: "linkAdd",
      },
      {
        label: serving ? "$(debug-disconnect) Stop Serve" : "$(radio-tower) Serve",
        description: serving
          ? `Stop bridge server on ${cfg.bridgePorts}`
          : `Open bridge server on ${cfg.bridgePorts}; Studio plugin can connect once and reuse it`,
        action: serving ? "stopServe" : "serve",
      },
      {
        label: liveSyncRunning ? "$(circle-slash) Stop Live Sync" : "$(broadcast) Live Sync",
        description: liveSyncRunning ? "Stop watching src and Studio changes" : "Two-way sync between src and Studio",
        action: liveSyncRunning ? "stopLive" : "startLive",
      },
      {
        label: "$(git) Git",
        description: "Open the Git tab in the main Renium panel",
        action: "gitSync",
      },
      {
        label: cfg.autoSyncOnSave ? "$(circle-slash) Disable Auto Sync On Save" : "$(history) Enable Auto Sync On Save",
        description: `Debounce ${cfg.autoSyncDebounceMs}ms`,
        action: "toggleAuto",
      },
      {
        label: "$(output) Show Output",
        description: "Open extension logs",
        action: "showOutput",
      },
      {
        label: "$(cloud-download) Install Studio Plugin",
        description: "Install or update the Renium plugin in your Roblox Plugins folder",
        action: "installStudioPlugin",
      },
    ];

    const picked = await vscode.window.showQuickPick(items, {
      title: "Renium",
      placeHolder: "Choose an action",
    });

    if (!picked) {
      return;
    }

    switch (picked.action) {
      case "fullSync":
        await this.fullSync();
        return;
      case "exportOnly":
        await this.exportSnapshotsOnly();
        return;
      case "exportGameFile":
        await this.exportGameFile();
        return;
      case "startLive":
        await this.startLiveSync();
        return;
      case "stopLive":
        await this.stopLiveSync();
        return;
      case "gitSync":
        await this.openGitSync();
        return;
      case "wallyPackages":
        await this.syncWallyPackages();
        return;
      case "linkApply":
        await this.linkApply();
        return;
      case "linkAdd":
        await this.addLinkInteractive();
        return;
      case "serve":
        await this.serve();
        return;
      case "stopServe":
        await this.stopServe();
        return;
      case "toggleAuto":
        await this.toggleAutoSyncOnSave();
        return;
      case "showOutput":
        this.output.show(true);
        return;
      case "installStudioPlugin":
        await this.installStudioPlugin();
        return;
      default:
        return;
    }
  }

  public async installStudioPlugin(): Promise<void> {
    const assetName = "Renium.rbxm";
    const releaseUrl = `https://github.com/Superwheat/renium/releases/latest/download/${assetName}`;
    let pluginsDir: string;
    if (process.platform === "win32") {
      const localAppData = process.env.LOCALAPPDATA;
      if (!localAppData) {
        void vscode.window.showErrorMessage("Renium: LOCALAPPDATA is not set; cannot locate the Roblox Plugins folder.");
        return;
      }
      pluginsDir = path.join(localAppData, "Roblox", "Plugins");
    } else if (process.platform === "darwin") {
      pluginsDir = path.join(os.homedir(), "Documents", "Roblox", "Plugins");
    } else {
      void vscode.window.showErrorMessage("Renium: Roblox Studio is only available on Windows and macOS.");
      return;
    }
    const target = path.join(pluginsDir, assetName);

    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: "Renium: installing Studio plugin..." },
      async () => {
        let bytes: Buffer | undefined;
        let source = releaseUrl;
        try {
          const response = await fetch(releaseUrl);
          if (!response.ok) {
            throw new Error(`HTTP ${response.status}`);
          }
          bytes = Buffer.from(await response.arrayBuffer());
        } catch (error) {
          const workspaceRoot = pickWorkspaceRoot();
          const localBundle = workspaceRoot
            ? path.join(workspaceRoot, "tools", "plugin_ws_bridge", assetName)
            : undefined;
          if (localBundle && fs.existsSync(localBundle)) {
            bytes = fs.readFileSync(localBundle);
            source = localBundle;
          } else {
            void vscode.window.showErrorMessage(
              `Renium: downloading the Studio plugin failed (${String(error)}). Check your network or grab ${assetName} from the GitHub release manually.`,
            );
            return;
          }
        }
        if (!bytes.subarray(0, 8).toString("latin1").startsWith("<roblox")) {
          void vscode.window.showErrorMessage("Renium: the downloaded plugin file is not a valid Roblox model.");
          return;
        }
        fs.mkdirSync(pluginsDir, { recursive: true });
        fs.writeFileSync(target, bytes);
        this.output.appendLine(`[plugin-install] ${source} -> ${target} (${bytes.length} bytes)`);
        void vscode.window.showInformationMessage(
          `Renium: Studio plugin installed (${Math.round(bytes.length / 1024)} KB). Restart Roblox Studio to load it.`,
        );
      },
    );
  }

  public async fullSync(): Promise<void> {
    await this.enqueue("Full sync", async () => {
      await this.runExport({
        services: this.getConfig().services,
        runImport: this.getConfig().runImport,
        notifyOnSuccess: true,
        reason: "Full sync completed",
      });
    });
  }

  public async exportSnapshotsOnly(): Promise<void> {
    await this.enqueue("Export snapshots", async () => {
      await this.runExport({
        services: this.getConfig().services,
        runImport: false,
        notifyOnSuccess: true,
        reason: "Snapshot export completed",
      });
    });
  }

  public async exportGameFile(): Promise<void> {
    const cfg = this.getConfig();
    const pickedFormat = await vscode.window.showQuickPick([
      {
        label: "rbxl",
        description: "Binary Roblox place file",
        format: "rbxl" as RobloxPlaceFormat,
      },
      {
        label: "rbxlx",
        description: "XML Roblox place file",
        format: "rbxlx" as RobloxPlaceFormat,
      },
    ], {
      title: "Export Game File",
      placeHolder: "Roblox place format",
    });
    if (!pickedFormat) {
      return;
    }

    const saveUri = await vscode.window.showSaveDialog({
      title: "Export Game File",
      saveLabel: "Export Game",
      defaultUri: vscode.Uri.file(path.join(cfg.projectRoot, safePlaceFileName(path.basename(cfg.projectRoot), pickedFormat.format))),
      filters: {
        "Roblox Place Files": ["rbxl", "rbxlx"],
      },
    });
    if (!saveUri) {
      return;
    }

    const format = robloxPlaceFormatFromPath(saveUri.fsPath) ?? pickedFormat.format;
    const outputPath = ensurePlaceFileExtension(saveUri.fsPath, format);

    await this.enqueue("Export game file", async () => {
      const runCfg = this.getConfig();
      const selectedServices = this.normalizeServices(runCfg.services, runCfg.services);
      const command = this.resolveRustCliPathForCommand(runCfg, "bytecode-export-place");
      this.ensureFileExists(command);
      const args = [
        "bep",
        "-r",
        runCfg.projectRoot,
        "-d",
        "src",
        "-s",
        selectedServices.join(","),
        "-o",
        outputPath,
        "--format",
        format,
      ];

      this.output.show(false);
      this.logResolvedConfig(runCfg);
      if (path.normalize(command) !== path.normalize(runCfg.rustCliPath)) {
        this.output.appendLine(`[renium] export game file: using fallback rustCliPath=${command}`);
      }
      this.output.appendLine(`[renium] export game file command: ${command} ${this.renderArgs(args)}`);
      const result = await this.runCommand(command, args, runCfg.projectRoot, "export-game-file", runCfg.progressHeartbeatSeconds);
      if (result.code !== 0) {
        throw new Error(`Game file export exited with code ${result.code}`);
      }

      const parsed = this.parseExportGameFileResult(result.output);
      const finalOutputPath = typeof parsed?.output === "string" && parsed.output.trim().length > 0
        ? parsed.output
        : outputPath;
      const instanceSummary = typeof parsed?.instances === "number" && Number.isFinite(parsed.instances)
        ? ` (${parsed.instances} instances)`
        : "";
      vscode.window.showInformationMessage(`Renium: exported game file to ${finalOutputPath}${instanceSummary}.`);
    });
  }

  public async importSnapshotsOnly(): Promise<void> {
    await this.enqueue("Import snapshots", async () => {
      const cfg = this.getConfig();
      const snapshotPath = this.resolveSnapshotPath(cfg);
      await this.runRustImport(cfg, snapshotPath, cfg.services);

      vscode.window.showInformationMessage("Renium: snapshot import finished.");
    });
  }

  public async syncWallyPackages(): Promise<void> {
    this.output.appendLine(`[renium] Wally packages: requested at ${new Date().toISOString()}`);
    const manifestReady = await this.ensureWallyManifest(this.getConfig());
    if (!manifestReady) {
      return;
    }

    let syncResult: CliSyncWallyPackagesResult | undefined;
    await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Renium: syncing Wally packages",
        cancellable: false,
      },
      async (progress) => {
        progress.report({ message: "Waiting for Renium task queue..." });
        await this.enqueue("Sync Wally packages", async () => {
          const runCfg = this.getConfig();
          const command = this.resolveRustCliPathForCommand(runCfg, "sync-wally-packages");
          this.ensureFileExists(command);
          const targetSettingsFile = existingReniumSettingsFile(runCfg.projectRoot, runCfg.wallySync.targetService);
          if (!fs.existsSync(targetSettingsFile)) {
            throw new Error(
              `No Renium settings file found for ${runCfg.wallySync.targetService}. Run Full Sync once first. Expected ${targetSettingsFile}`,
            );
          }
          const args = [
            "sync-wally-packages",
            "-r",
            runCfg.projectRoot,
            "-d",
            "src",
            "--wally-path",
            runCfg.wallySync.wallyPath,
            "--packages-dir",
            runCfg.wallySync.packagesDir,
            "--target-service",
            runCfg.wallySync.targetService,
            "--target-name",
            runCfg.wallySync.targetName,
            "--realms",
            runCfg.wallySync.realms,
          ];
          if (!runCfg.wallySync.runInstall) {
            args.push("--skip-install");
          }

          progress.report({ message: "Running wally install and bytecode import..." });
          this.output.show(false);
          this.logResolvedConfig(runCfg);
          if (path.normalize(command) !== path.normalize(runCfg.rustCliPath)) {
            this.output.appendLine(`[renium] Wally packages: using fallback rustCliPath=${command}`);
          }
          this.output.appendLine(`[renium] Wally packages command: ${command} ${this.renderArgs(args)}`);
          const result = await this.runCommand(command, args, runCfg.projectRoot, "wally-packages", runCfg.progressHeartbeatSeconds);
          if (result.code !== 0) {
            throw new Error(this.wallySyncFailureMessage(result));
          }

          const parsed = this.parseCliJsonObject<CliSyncWallyPackagesResult>(result.output);
          if (!parsed || parsed.ok === false) {
            throw new Error("Wally package sync didn't finish. Check the Renium output panel for details.");
          }
          syncResult = parsed;
          const importedCount = Array.isArray(parsed.settingsIds) ? parsed.settingsIds.length : 0;
          progress.report({ message: `Imported ${importedCount} package instance(s).` });
          this.output.appendLine(
            `[renium] Wally packages: imported ${importedCount} instance(s) into ${parsed.service ?? runCfg.wallySync.targetService}.${parsed.targetName ?? runCfg.wallySync.targetName}`,
          );
        });
      },
    );

    if (!syncResult) {
      return;
    }
    await this.applyWallyPackagesToStudio(syncResult);
  }

  private wallySyncFailureMessage(result: CommandRunResult): string {
    const detail = this.compactCommandOutput(result.output, 10, 1200);
    const hint = this.wallySyncFailureHint(result.output);
    const suffix = detail.length > 0 ? ` Details: ${detail}` : " Open the Renium output panel for details.";
    return `Couldn't sync Wally packages.${hint ? ` ${hint}` : ""}${suffix}`;
  }

  private wallySyncFailureHint(output: string): string {
    const lower = output.toLowerCase();
    if (lower.includes("failed to launch wally") || lower.includes("could not find command wally") || lower.includes("program not found")) {
      return "Wally was not found. Install Wally or set renium.wallySync.wallyPath.";
    }
    if (lower.includes("failed to launch rojo") || lower.includes("could not find command rojo")) {
      return "Rojo was not found. Install Rojo or set renium.wallySync.rojoPath.";
    }
    if (lower.includes("no wally manifest") || lower.includes("wally.toml")) {
      return "Check that wally.toml exists at renium.projectRoot.";
    }
    if (lower.includes("no renium bytecode settings file") || lower.includes("run full sync once")) {
      return "Run Full Sync once before syncing packages.";
    }
    if (lower.includes("aftman")) {
      return "If you use Aftman shims, make sure the project has aftman.toml and the tool is trusted/installed.";
    }
    return "";
  }

  private compactCommandOutput(output: string, maxLines: number, maxChars: number): string {
    const lines = output
      .replace(/\r\n/g, "\n")
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
    const text = lines.slice(-maxLines).join(" | ");
    if (text.length <= maxChars) {
      return text;
    }
    return `...${text.slice(text.length - maxChars)}`;
  }

  private async ensureWallyManifest(cfg: SyncConfig): Promise<boolean> {
    const manifestPath = path.join(cfg.projectRoot, "wally.toml");
    if (fs.existsSync(manifestPath)) {
      return true;
    }

    const create = "Create starter wally.toml";
    const picked = await vscode.window.showWarningMessage(
      "Renium: no wally.toml was found at the project root.",
      create,
      "Cancel",
    );
    if (picked !== create) {
      vscode.window.showInformationMessage("Renium: Wally package sync cancelled because no wally.toml was found.");
      return false;
    }

    if (!fs.existsSync(manifestPath)) {
      fs.writeFileSync(manifestPath, this.starterWallyManifest(cfg.projectRoot), "utf8");
      this.output.appendLine(`[renium] Wally packages: created ${manifestPath}`);
    }
    vscode.window.showInformationMessage("Renium: created starter wally.toml. Add dependencies, then run Sync Wally Packages again.");
    return false;
  }

  private starterWallyManifest(projectRoot: string): string {
    const packageName = this.safeWallyPackageName(projectRoot);
    return [
      "[package]",
      `name = "local/${packageName}"`,
      'version = "0.1.0"',
      'registry = "https://github.com/UpliftGames/wally-index"',
      'realm = "shared"',
      "",
      "[dependencies]",
      "",
    ].join("\n");
  }

  private safeWallyPackageName(projectRoot: string): string {
    const base = path.basename(projectRoot).toLowerCase()
      .replace(/[^a-z0-9-]+/g, "-")
      .replace(/-+/g, "-")
      .replace(/^-|-$/g, "");
    return base || "project";
  }

  private async applyWallyPackagesToStudio(result: CliSyncWallyPackagesResult): Promise<void> {
    const cfg = this.getConfig();
    const mode = cfg.wallySync.applyToStudio;

    const settingsFiles: string[] = [];
    const sourceWritePaths: string[] = [];
    const removedTargets: CliWallyRemovedTarget[] = [];
    const collect = (
      settingsFile: string | undefined,
      sourceWrites: CliWallySourceWrite[] | undefined,
      removed: CliWallyRemovedTarget | null | undefined,
    ): void => {
      if (typeof settingsFile === "string" && settingsFile.length > 0) {
        settingsFiles.push(settingsFile);
      }
      for (const write of sourceWrites ?? []) {
        if (typeof write.path === "string" && write.path.length > 0) {
          sourceWritePaths.push(write.path);
        }
      }
      const valid = this.validWallyRemovedTarget(removed);
      if (valid) {
        removedTargets.push(valid);
      }
    };

    const realms = (Array.isArray(result.realms) ? result.realms : []).filter((realm) => realm && realm.skipped !== true);
    let summaryTarget: string;
    if (realms.length > 0) {
      for (const realm of realms) {
        collect(realm.settingsFile, realm.sourceWrites, realm.removedTarget);
      }
      summaryTarget = realms.map((realm) => `${realm.service}/${realm.targetName}`).join(", ");
    } else {
      collect(result.settingsFile, result.sourceWrites, result.removedTarget);
      summaryTarget = `${result.service ?? cfg.wallySync.targetService}/${result.targetName ?? cfg.wallySync.targetName}`;
    }

    const extraChanged = (Array.isArray(result.changedPaths) ? result.changedPaths : [])
      .filter((filePath): filePath is string => typeof filePath === "string" && filePath.length > 0);
    const changedPaths = Array.from(new Set([...settingsFiles, ...sourceWritePaths, ...extraChanged]));

    if (mode === "never" || (changedPaths.length === 0 && removedTargets.length === 0)) {
      vscode.window.showInformationMessage(`Renium: synced Wally packages to ${summaryTarget}.`);
      return;
    }

    let shouldApply = mode === "always";
    if (mode === "ask") {
      const apply = "Apply to Studio";
      const picked = await vscode.window.showInformationMessage(
        `Renium: synced Wally packages to ${summaryTarget}.`,
        apply,
        "Not now",
      );
      shouldApply = picked === apply;
    }
    if (!shouldApply) {
      return;
    }
    if (!this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      vscode.window.showInformationMessage(`Renium: synced Wally packages locally (${summaryTarget}). Start Serve or live sync before applying to Studio.`);
      return;
    }

    try {
      for (const removed of removedTargets) {
        await this.pushEditorDeleteNow({
          force: true,
          service: removed.pathSegments?.[0] ?? "",
          settingsId: removed.settingsId,
          className: removed.className,
          pathSegments: removed.pathSegments,
          pathOrdinals: removed.pathOrdinals,
        });
      }
      if (changedPaths.length > 0) {
        const idSource = Array.isArray(result.targetSettingsIds) ? result.targetSettingsIds : result.settingsIds;
        const targetSettingsIds = Array.isArray(idSource)
          ? idSource.map((value) => String(value).trim()).filter((value) => value.length > 0)
          : [];
        const pushed = await this.pushEditorPathsNow(changedPaths, {
          force: true,
          skipChangeFilter: true,
          taskName: "Wally packages -> Studio",
          targetSettingsIds,
        });
        if (!pushed) {
          vscode.window.showInformationMessage(`Renium: synced Wally packages locally (${summaryTarget}). Start Serve or live sync before applying to Studio.`);
          return;
        }
      }
      vscode.window.showInformationMessage(`Renium: applied Wally packages to Studio (${summaryTarget}).`);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] Wally packages Studio apply failed: ${message}`);
      this.output.show(true);
      vscode.window.showWarningMessage(`Renium: Wally packages synced locally, but Studio apply failed. ${message}`);
    }
  }

  private validWallyRemovedTarget(target: CliWallyRemovedTarget | null | undefined): CliWallyRemovedTarget | undefined {
    if (!target || typeof target !== "object") {
      return undefined;
    }
    const settingsId = typeof target.settingsId === "string" ? target.settingsId.trim() : "";
    const className = typeof target.className === "string" ? target.className.trim() : "";
    const pathSegments = Array.isArray(target.pathSegments)
      ? target.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    const pathOrdinals = Array.isArray(target.pathOrdinals)
      ? target.pathOrdinals.map((ordinal) => Number(ordinal)).filter((ordinal) => Number.isInteger(ordinal) && ordinal > 0)
      : [];
    if (!settingsId || !className || pathSegments.length <= 1) {
      return undefined;
    }
    return { settingsId, className, pathSegments, pathOrdinals };
  }

  private linkManifestPath(cfg: SyncConfig): string {
    return path.isAbsolute(cfg.linkSync.manifest)
      ? cfg.linkSync.manifest
      : path.join(cfg.projectRoot, cfg.linkSync.manifest);
  }

  /** Resolve, materialize, and push every link target to disk and (optionally) Studio. */
  public async linkApply(options: { silent?: boolean; refreshExplorer?: boolean; forceStudio?: boolean; forceTargets?: boolean; forceTargetPaths?: string[][]; taskName?: string; linkId?: string; skipStudio?: boolean } = {}): Promise<CliLinkApplyResult | undefined> {
    let result: CliLinkApplyResult | undefined;
    await this.enqueue("Apply packages", async () => {
      const cfg = this.getConfig();
      const manifestPath = this.linkManifestPath(cfg);
      if (!fs.existsSync(manifestPath)) {
        if (!options.silent) {
          vscode.window.showInformationMessage(
            `Renium: no link manifest found at ${manifestPath}. Use "Renium: Add Link" first.`,
          );
        }
        return;
      }
      const command = this.resolveRustCliPathForCommand(cfg, "link-apply");
      this.ensureFileExists(command);
      const args = [
        "link-apply",
        "-r",
        cfg.projectRoot,
        "-d",
        "src",
        "--manifest",
        cfg.linkSync.manifest,
        "--git-path",
        cfg.linkSync.gitPath,
        "--wally-path",
        cfg.linkSync.wallyPath,
      ];
      if (cfg.linkSync.offline) {
        args.push("--offline");
      }
      const linkId = typeof options.linkId === "string" ? options.linkId.trim() : "";
      if (linkId.length > 0) {
        args.push("--link", linkId);
      }
      if (cfg.linkSync.cacheDir.length > 0) {
        args.push("--cache-dir", cfg.linkSync.cacheDir);
      }
      if (options.forceTargets === true || options.forceStudio === true) {
        args.push("--force-targets");
      }
      for (const targetPath of options.forceTargetPaths ?? []) {
        if (Array.isArray(targetPath) && targetPath.length > 0) {
          args.push("--force-target", JSON.stringify(targetPath));
        }
      }
      const run = await this.runCommand(command, args, cfg.projectRoot, "link-apply", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't apply packages. Check the Renium output panel for details.");
      }
      const parsed = this.parseCliJsonObject<CliLinkApplyResult>(run.output);
      if (!parsed || parsed.ok === false) {
        throw new Error("Applying packages didn't finish. Check the Renium output panel for details.");
      }
      result = parsed;
      for (const warning of Array.isArray(parsed.warnings) ? parsed.warnings : []) {
        this.output.appendLine(`[renium] link warning: ${warning}`);
      }
      const applied = parsed.appliedTargets ?? 0;
      if (!options.silent) {
        const warnCount = Array.isArray(parsed.warnings) ? parsed.warnings.length : 0;
        vscode.window.showInformationMessage(
          `Renium: applied ${applied} link target(s)${warnCount > 0 ? `, ${warnCount} warning(s)` : ""}.`,
        );
      }
    });
    this.invalidateLinkStatusCache();
    const forceStudioAllowed = options.forceStudio === true && this.canUseStudioPushPipeline();
    if (options.forceStudio === true && !forceStudioAllowed) {
      this.noteStudioPushSkipped("serve/live sync is not active");
    }
    if (result && options.skipStudio !== true && (forceStudioAllowed || (this.isEditorLiveSyncActive() && this.getConfig().linkSync.applyToStudio !== "never"))) {
      const changedPaths = (Array.isArray(result.changedPaths) ? result.changedPaths : [])
        .filter((filePath): filePath is string => typeof filePath === "string" && filePath.length > 0);
      if (changedPaths.length > 0) {
        this.noteProgrammaticEditorWrite({ paths: changedPaths, durationMs: 5000 });
        if (forceStudioAllowed) {
          const targetSettingsIds = (Array.isArray(result.targetSettingsIds) ? result.targetSettingsIds : [])
            .map((value) => String(value).trim())
            .filter((value) => value.length > 0);
          await this.pushEditorPathsNow(changedPaths, {
            force: true,
            skipChangeFilter: true,
            taskName: options.taskName ?? "Link -> Studio",
            targetSettingsIds,
          });
        } else {
          await this.applyLinksToStudio(result, { silent: options.silent === true });
        }
      }
    }
    if (options.refreshExplorer !== false) {
      await this.refreshFileExplorerSafe();
    }
    return result;
  }

  private async applyLinksToStudio(result: CliLinkApplyResult, options: { silent?: boolean } = {}): Promise<void> {
    const changedPaths = (Array.isArray(result.changedPaths) ? result.changedPaths : [])
      .filter((filePath): filePath is string => typeof filePath === "string" && filePath.length > 0);
    if (changedPaths.length === 0) {
      return;
    }
    const mode = this.getConfig().linkSync.applyToStudio;
    if (mode === "never") {
      return;
    }
    if (mode === "ask") {
      const apply = "Apply links to Studio";
      const picked = await vscode.window.showWarningMessage(
        "Apply the new package content to Studio now?",
        { modal: true },
        apply,
      );
      if (picked !== apply) {
        return;
      }
    }
    try {
      const targetSettingsIds = (Array.isArray(result.targetSettingsIds) ? result.targetSettingsIds : [])
        .map((value) => String(value).trim())
        .filter((value) => value.length > 0);
      await this.pushEditorPathsNow(changedPaths, {
        force: true,
        skipChangeFilter: true,
        taskName: "Link -> Studio",
        targetSettingsIds,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] link Studio push failed: ${message}`);
      if (!options.silent) {
        vscode.window.showWarningMessage(`Renium: link applied to src/, but the Studio push failed. ${message}`);
      }
    }
  }

  /** Detach a link target so it becomes a normal editable script. */
  public async breakLink(service: string, pathSegments: string[], options: { silent?: boolean; refreshExplorer?: boolean } = {}): Promise<void> {
    await this.enqueue("Break link", async () => {
      const cfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(cfg, "link-break");
      this.ensureFileExists(command);
      const args = [
        "link-break",
        "-r",
        cfg.projectRoot,
        "-d",
        "src",
        "--manifest",
        cfg.linkSync.manifest,
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      ];
      if (cfg.linkSync.cacheDir.length > 0) {
        args.push("--cache-dir", cfg.linkSync.cacheDir);
      }
      const run = await this.runCommand(command, args, cfg.projectRoot, "link-break", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        const detail = run.output.trim();
        throw new Error(
          detail.length > 0
            ? `renium-link break failed (exit ${run.code}). ${detail}`
            : `renium-link break failed (exit ${run.code}). Open the Renium output panel for details.`,
        );
      }
    });
    this.invalidateLinkStatusCache();
    if (options.refreshExplorer !== false) {
      await this.refreshFileExplorerSafe();
    }
    if (!options.silent) {
      vscode.window.showInformationMessage(`Renium: broke link on ${service}.${pathSegments[pathSegments.length - 1] ?? ""}. It is now editable.`);
    }
  }

  /** Guided creation of a new link entry in the manifest. */
  public async addLinkInteractive(seed?: { service?: string; pathSegments?: string[] }): Promise<void> {
    const cfg = this.getConfig();
    const sourceType = await vscode.window.showQuickPick(
      [
        { label: "Local path", value: "local", description: "A file or folder inside this project" },
        { label: "Git repository", value: "git", description: "Public or private git repo (uses your git credentials)" },
        { label: "Wally package", value: "wally", description: "A package installed via wally install" },
      ],
      { title: "Renium Link: source type", placeHolder: "Where does the controlled source live?" },
    );
    if (!sourceType) {
      return;
    }
    let source: string | undefined;
    if (sourceType.value === "local") {
      const picked = await vscode.window.showOpenDialog({
        title: "Renium Link: choose the source file or folder to control",
        canSelectFiles: true,
        canSelectFolders: true,
        canSelectMany: false,
        defaultUri: vscode.Uri.file(cfg.projectRoot),
        openLabel: "Use as link source",
      });
      if (!picked || picked.length === 0) {
        return;
      }
      const abs = picked[0].fsPath;
      source = this.isPathInside(abs, cfg.projectRoot)
        ? path.relative(cfg.projectRoot, abs).split(path.sep).join("/")
        : abs;
    } else {
      const sourcePrompt = sourceType.value === "git"
        ? "Git repository URL (https://... or git@...)"
        : "Wally package (scope/name)";
      source = await vscode.window.showInputBox({ title: "Renium Link: source", prompt: sourcePrompt, ignoreFocusOut: true });
    }
    if (!source) {
      return;
    }
    let sourceRef: string | undefined;
    let subpath: string | undefined;
    if (sourceType.value === "git") {
      sourceRef = await vscode.window.showInputBox({ title: "Renium Link: git ref", prompt: "Branch, tag, or commit (blank = default branch)", ignoreFocusOut: true });
      subpath = await vscode.window.showInputBox({ title: "Renium Link: subpath", prompt: "Path within the repo (blank = repo root)", ignoreFocusOut: true });
    } else if (sourceType.value === "wally") {
      sourceRef = await vscode.window.showInputBox({ title: "Renium Link: version", prompt: "Version requirement (e.g. ^4.0.0, blank = installed)", ignoreFocusOut: true });
    }

    const service = seed?.service ?? await vscode.window.showQuickPick(cfg.services, { title: "Renium Link: target service", placeHolder: "First target service" });
    if (!service) {
      return;
    }
    let pathSegments = seed?.pathSegments;
    if (!pathSegments) {
      const raw = await vscode.window.showInputBox({
        title: "Renium Link: target path",
        prompt: `Instance path under ${service}, dot-separated (e.g. ${service}.Modules.Logger)`,
        value: `${service}.`,
        ignoreFocusOut: true,
      });
      if (!raw) {
        return;
      }
      pathSegments = raw.split(".").map((segment) => segment.trim()).filter((segment) => segment.length > 0);
    }
    if (!pathSegments || pathSegments.length === 0) {
      return;
    }

    await this.enqueue("Add link", async () => {
      const command = this.resolveRustCliPathForCommand(cfg, "link-add");
      this.ensureFileExists(command);
      const args = [
        "link-add",
        "-r",
        cfg.projectRoot,
        "--manifest",
        cfg.linkSync.manifest,
        "--source-type",
        sourceType.value,
        "--source",
        source,
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      ];
      if (sourceRef && sourceRef.length > 0) {
        args.push("--ref", sourceRef);
      }
      if (subpath && subpath.length > 0) {
        args.push("--subpath", subpath);
      }
      const run = await this.runCommand(command, args, cfg.projectRoot, "link-add", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't add the link. Check the Renium output panel for details.");
      }
    });
    const syncNow = await vscode.window.showInformationMessage("Renium: link added. Apply it now?", "Sync now", "Later");
    if (syncNow === "Sync now") {
      await this.linkApply();
    } else {
      this.invalidateLinkStatusCache();
      await this.refreshFileExplorerSafe();
    }
  }

  public invalidateLinkStatusCache(): void {
    this.linkStatusCache = undefined;
    this.linkChangeEmitter.fire();
  }

  /** Push the set of linked/broken target keys to the file explorer so it can
   * show "Create Link" vs "Break Link" appropriately. */
  public async pushLinkStateToExplorer(): Promise<void> {
    const keys: Record<string, string> = {};
    try {
      const status = await this.getLinkStatus(true);
      for (const target of status?.targets ?? []) {
        if (target.missing === true || target.resolved === false) {
          continue;
        }
        if (typeof target.pathKey === "string" && target.pathKey.length > 0) {
          keys[target.pathKey] = target.broken ? "broken" : "linked";
        }
      }
    } catch {
    }
    try {
      await vscode.commands.executeCommand("renium.fileExplorer.setLinkState", keys);
    } catch {
    }
  }

  public normalizeLinkPathKey(fsPath: string): string {
    const stripped = String(fsPath || "").replace(/^[\\/]{2}\?[\\/]/, "");
    const normalized = path.resolve(stripped).replace(/\\/g, "/");
    return process.platform === "win32" ? normalized.toLowerCase() : normalized;
  }

  /** Per-user global package library (mirrors the CLI's resolution). */
  private globalPackagesDir(): string {
    const custom = (process.env.RENIUM_GLOBAL_PACKAGES_DIR ?? "").trim();
    if (custom) {
      return path.normalize(custom);
    }
    const home = process.env.USERPROFILE || process.env.HOME || "";
    return path.normalize(path.join(home, "Documents", "Renium", "Packages"));
  }

  /** In-project package folder (the committed, git-shareable location). */
  private linkPackageFolderPath(cfg: SyncConfig): string {
    const folder = cfg.linkSync.folder || "links";
    return path.isAbsolute(folder)
      ? path.normalize(folder)
      : path.normalize(path.join(cfg.projectRoot, folder));
  }

  /** True when the path is a Renium-managed package location (project or global). */
  private isManagedPackagePath(cfg: SyncConfig, candidate: string): boolean {
    return this.isPathInside(candidate, this.linkPackageFolderPath(cfg))
      || this.isPathInside(candidate, this.globalPackagesDir());
  }

  private absoluteLinkSourcePath(cfg: SyncConfig, sourcePath: string | undefined): string | undefined {
    const trimmed = String(sourcePath ?? "").trim();
    if (!trimmed) {
      return undefined;
    }
    const normalized = trimmed.replace(/\\/g, "/");
    if (normalized.startsWith("~global/")) {
      return path.normalize(path.join(this.globalPackagesDir(), normalized.slice("~global/".length)));
    }
    return path.isAbsolute(trimmed) ? path.normalize(trimmed) : path.normalize(path.join(cfg.projectRoot, trimmed));
  }

  public onLinkPackageSourceChanged(uri: vscode.Uri): void {
    if (uri.scheme !== "file") {
      return;
    }
    const cfg = this.getConfig();
    const ext = path.extname(uri.fsPath).toLowerCase();
    if ((ext !== ".rbsync" && ext !== ".renium") || !this.isManagedPackagePath(cfg, uri.fsPath)) {
      return;
    }
    this.pendingLinkPackageSourcePaths.add(path.normalize(uri.fsPath));
    if (this.linkPackageSourceApplyTimer) {
      clearTimeout(this.linkPackageSourceApplyTimer);
    }
    this.linkPackageSourceApplyTimer = setTimeout(() => {
      this.linkPackageSourceApplyTimer = undefined;
      void this.flushLinkPackageSourceChanges().catch((error) => {
        this.output.appendLine(`[renium] package source auto-apply failed: ${error instanceof Error ? error.message : String(error)}`);
      });
    }, 500);
  }

  private async flushLinkPackageSourceChanges(): Promise<void> {
    const changedPaths = [...this.pendingLinkPackageSourcePaths];
    this.pendingLinkPackageSourcePaths.clear();
    if (changedPaths.length === 0) {
      return;
    }
    const cfg = this.getConfig();
    const changedKeys = new Set(changedPaths.map((filePath) => this.normalizePathForCompare(filePath)));
    this.invalidateLinkStatusCache();
    const status = await this.getLinkStatus(true);
    const linkIds = new Set<string>();
    for (const link of status?.links ?? []) {
      const id = typeof link.id === "string" ? link.id.trim() : "";
      const sourcePath = this.absoluteLinkSourcePath(cfg, link.sourcePath);
      if (!id || !sourcePath || !changedKeys.has(this.normalizePathForCompare(sourcePath))) {
        continue;
      }
      if (link.isPackage !== true && !/\.(rbsync|renium)$/i.test(sourcePath)) {
        continue;
      }
      if (Number(link.targetCount ?? 0) <= 0) {
        continue;
      }
      linkIds.add(id);
    }
    if (linkIds.size === 0) {
      return;
    }
    this.output.appendLine(`[renium] package source changed: applying ${linkIds.size} active link package(s).`);
    for (const linkId of linkIds) {
      await this.linkApply({
        silent: true,
        refreshExplorer: false,
        linkId,
        skipStudio: true,
      });
    }
    await this.refreshFileExplorerSafe();
  }

  public scheduleStartupLinkRefresh(): void {
    setTimeout(() => {
      void this.linkApply({
        silent: true,
        skipStudio: true,
      }).catch((error) => {
        this.output.appendLine(`[renium] startup link refresh failed: ${error instanceof Error ? error.message : String(error)}`);
      });
    }, 750);
  }

  /** Map of normalized mirror file path -> link info, for decorations and break/reveal. */
  public async getLinkFileIndex(force = false): Promise<Map<string, LinkFileInfo>> {
    const status = await this.getLinkStatus(force);
    const index = new Map<string, LinkFileInfo>();
    for (const target of status?.targets ?? []) {
      const base: LinkFileInfo = {
        linkId: target.linkId ?? "",
        service: target.service ?? "",
        pathSegments: Array.isArray(target.path) ? target.path : [],
        readOnly: target.readOnly !== false,
        broken: target.broken === true,
        drift: target.drift === true,
      };
      for (const mirror of target.mirrors ?? []) {
        if (typeof mirror.path === "string" && mirror.path.length > 0) {
          index.set(this.normalizeLinkPathKey(mirror.path), {
            ...base,
            canonical: typeof mirror.canonical === "string" ? mirror.canonical : undefined,
            drift: mirror.drift === true || base.drift,
          });
        }
      }
    }
    return index;
  }

  private async linkInfoForFile(uri: vscode.Uri): Promise<LinkFileInfo | undefined> {
    const index = await this.getLinkFileIndex();
    return index.get(this.normalizeLinkPathKey(uri.fsPath));
  }

  private stripScriptExtension(name: string): string | undefined {
    const lower = name.toLowerCase();
    for (const suffix of [".server.luau", ".server.lua", ".client.luau", ".client.lua", ".luau", ".lua"]) {
      if (lower.endsWith(suffix) && name.length > suffix.length) {
        return name.slice(0, name.length - suffix.length);
      }
    }
    return undefined;
  }

  /** Derive a link target (service + instance path) from a src script file. */
  private linkTargetFromFile(uri: vscode.Uri): { service: string; pathSegments: string[] } | undefined {
    const cfg = this.getConfig();
    const srcRoot = path.join(cfg.projectRoot, "src");
    if (!this.isPathInside(uri.fsPath, srcRoot)) {
      return undefined;
    }
    const parts = path.relative(srcRoot, uri.fsPath).split(path.sep).filter((segment) => segment.length > 0);
    if (parts.length < 2) {
      return undefined;
    }
    const service = parts[0];
    const fileName = parts[parts.length - 1];
    if (/^init(\.server|\.client)?\.(luau|lua)$/i.test(fileName)) {
      const segments = parts.slice(0, parts.length - 1);
      return segments.length >= 2 ? { service, pathSegments: segments } : undefined;
    }
    const leaf = this.stripScriptExtension(fileName);
    if (!leaf) {
      return undefined;
    }
    return { service, pathSegments: [...parts.slice(0, parts.length - 1), leaf] };
  }

  /** Right-click entry: turn the selected script into a link target. */
  public async addLinkFromFile(uri: vscode.Uri | undefined): Promise<void> {
    const target = uri ?? vscode.window.activeTextEditor?.document.uri;
    if (!target) {
      vscode.window.showInformationMessage("Renium: right-click a script under src/ to link it.");
      return;
    }
    const seed = this.linkTargetFromFile(target);
    if (!seed) {
      vscode.window.showInformationMessage("Renium: that file is not a script under src/.");
      return;
    }
    await this.addLinkInteractive(seed);
  }

  /** Pack any instance (subtree) into a bytecode package and link it; optionally mirror elsewhere. */
  public async packInstanceLink(request: { service?: string; pathSegments?: string[]; id?: string; resave?: boolean }): Promise<void> {
    const service = typeof request?.service === "string" ? request.service : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    const requestedLinkId = typeof request?.id === "string" ? request.id.trim() : "";
    const resave = request?.resave === true;
    if (!service || pathSegments.length === 0) {
      vscode.window.showWarningMessage("Renium: select an instance to link.");
      return;
    }

    let packed: { id?: string; source?: string } | undefined;
    await this.enqueue("Create package", async () => {
      const cfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(cfg, "link-pack");
      this.ensureFileExists(command);
      const args = [
        "link-pack",
        "-r",
        cfg.projectRoot,
        "-d",
        "src",
        "--manifest",
        cfg.linkSync.manifest,
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      ];
      if (cfg.linkSync.folder) {
        args.push("--link-folder", cfg.linkSync.folder);
      }
      if (requestedLinkId.length > 0) {
        args.push("--id", requestedLinkId);
      }
      const run = await this.runCommand(command, args, cfg.projectRoot, "link-pack", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't save the package. Check the Renium output panel for details.");
      }
      packed = this.parseCliJsonObject<{ id?: string; source?: string }>(run.output) ?? undefined;
    });
    this.invalidateLinkStatusCache();

    const leaf = pathSegments[pathSegments.length - 1];
    if (!resave && packed?.id && packed?.source) {
      const add = await vscode.window.showInformationMessage(
        `Renium: packaged ${leaf}. Mirror it to another location (read-only copy)?`,
        "Add mirror",
        "Not now",
      );
      if (add === "Add mirror") {
        await this.addPackageMirror(packed.id, packed.source);
      }
    }
    await this.linkApply({ silent: true, linkId: packed?.id ?? requestedLinkId, skipStudio: true });
    await this.refreshFileExplorerSafe();
    vscode.window.showInformationMessage(resave
      ? `Renium: saved new version of ${packed?.id ?? requestedLinkId}.`
      : `Renium: linked ${leaf}.`);
  }

  public async resavePackageLink(request: { service?: string; pathSegments?: string[] }): Promise<void> {
    const service = typeof request?.service === "string" ? request.service.trim() : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length === 0) {
      vscode.window.showWarningMessage("Renium: select a linked package root to resave.");
      return;
    }
    const targetPath = this.normalizeLinkTargetSegments(service, pathSegments);
    const status = await this.getLinkStatus(true);
    const target = (status?.targets ?? []).find((candidate) =>
      candidate.missing !== true &&
      candidate.resolved !== false &&
      String(candidate.service ?? "") === service &&
      this.samePathSegments(
        this.normalizeLinkTargetSegments(service, Array.isArray(candidate.path) ? candidate.path : []),
        targetPath,
      )
    );
    const linkId = typeof target?.linkId === "string" ? target.linkId.trim() : "";
    if (!linkId) {
      vscode.window.showWarningMessage("Renium: selected instance is not a package link target.");
      return;
    }
    const link = (await this.getLinkPackages(true)).find((candidate) => candidate.id === linkId);
    if (!link || link.sourceKind !== "local" || !link.sourcePath) {
      vscode.window.showWarningMessage(`Renium: ${linkId} is not a local Renium package, so it cannot be resaved from Explorer.`);
      return;
    }
    const cfg = this.getConfig();
    const sourcePath = this.absoluteLinkSourcePath(cfg, link.sourcePath);
    if (!sourcePath || !this.isManagedPackagePath(cfg, sourcePath) || !/\.(rbsync|renium)$/i.test(sourcePath)) {
      vscode.window.showWarningMessage(`Renium: ${linkId} is not stored in a Renium packages folder.`);
      return;
    }
    const label = this.linkTargetDisplay(service, pathSegments);
    const relinkNote = target?.broken === true ? " This will relink the broken target." : "";
    const picked = await vscode.window.showWarningMessage(
      `Overwrite package ${linkId} with the current ${label} tree? Active uses will update from this new version.${relinkNote}`,
      { modal: true },
      "Save New Version",
    );
    if (picked !== "Save New Version") {
      return;
    }
    await this.packInstanceLink({
      service,
      pathSegments,
      id: linkId,
      resave: true,
    });
  }

  public async relinkPackageTarget(request: { service?: string; pathSegments?: string[] }): Promise<void> {
    const service = typeof request?.service === "string" ? request.service.trim() : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length === 0) {
      vscode.window.showWarningMessage("Renium: select a broken package root to relink.");
      return;
    }
    const targetPath = this.normalizeLinkTargetSegments(service, pathSegments);
    const status = await this.getLinkStatus(true);
    const target = (status?.targets ?? []).find((candidate) =>
      String(candidate.service ?? "") === service &&
      this.samePathSegments(
        this.normalizeLinkTargetSegments(service, Array.isArray(candidate.path) ? candidate.path : []),
        targetPath,
      )
    );
    const linkId = typeof target?.linkId === "string" ? target.linkId.trim() : "";
    if (!linkId) {
      vscode.window.showWarningMessage("Renium: selected instance is not a package link target.");
      return;
    }
    if (target?.broken !== true) {
      vscode.window.showInformationMessage(`Renium: ${this.linkTargetDisplay(service, pathSegments)} is already linked.`);
      return;
    }
    const picked = await vscode.window.showWarningMessage(
      `Relink ${this.linkTargetDisplay(service, pathSegments)} from package ${linkId}? Local edits in this broken copy will be replaced by the saved package.`,
      { modal: true },
      "Relink Package",
    );
    if (picked !== "Relink Package") {
      return;
    }
    await this.enqueue("Relink package", async () => {
      const runCfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(runCfg, "link-add");
      this.ensureFileExists(command);
      const args = [
        "link-add",
        "-r",
        runCfg.projectRoot,
        "--manifest",
        runCfg.linkSync.manifest,
        "--id",
        linkId,
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      ];
      const run = await this.runCommand(command, args, runCfg.projectRoot, "link-add", runCfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't relink the package. Check the Renium output panel for details.");
      }
    });
    this.invalidateLinkStatusCache();
    await this.linkApply({ silent: true, linkId, skipStudio: true });
    await this.refreshFileExplorerSafe();
    vscode.window.showInformationMessage(`Renium: relinked ${this.linkTargetDisplay(service, pathSegments)}.`);
  }

  private async addPackageMirror(linkId: string, source: string): Promise<void> {
    const cfg = this.getConfig();
    const service = await vscode.window.showQuickPick(cfg.services, {
      title: "Renium Link: mirror target service",
      placeHolder: "Service to receive the read-only copy",
    });
    if (!service) {
      return;
    }
    const raw = await vscode.window.showInputBox({
      title: "Renium Link: mirror target path",
      prompt: `Instance path under ${service}, dot-separated (e.g. ${service}.Shared.Widget)`,
      value: `${service}.`,
      ignoreFocusOut: true,
    });
    if (!raw) {
      return;
    }
    const pathSegments = raw.split(".").map((segment) => segment.trim()).filter((segment) => segment.length > 0);
    if (pathSegments.length === 0) {
      return;
    }
    await this.enqueue("Add link mirror", async () => {
      const runCfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(runCfg, "link-add");
      this.ensureFileExists(command);
      const args = [
        "link-add",
        "-r",
        runCfg.projectRoot,
        "--manifest",
        runCfg.linkSync.manifest,
        "--id",
        linkId,
        "--source-type",
        "local",
        "--source",
        source,
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      ];
      const run = await this.runCommand(command, args, runCfg.projectRoot, "link-add", runCfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't add the mirror. Check the Renium output panel for details.");
      }
    });
  }

  public async breakInstanceLink(request: { service?: string; pathSegments?: string[]; silent?: boolean; refreshExplorer?: boolean }): Promise<void> {
    const service = typeof request?.service === "string" ? request.service : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length === 0) {
      return;
    }
    await this.breakLink(service, pathSegments, {
      silent: request.silent === true,
      refreshExplorer: request.refreshExplorer,
    });
  }

  public async breakLinkForFile(uri: vscode.Uri | undefined): Promise<void> {
    const target = uri ?? vscode.window.activeTextEditor?.document.uri;
    if (!target) {
      vscode.window.showInformationMessage("Renium: open or select a linked file first.");
      return;
    }
    const info = await this.linkInfoForFile(target);
    if (!info || info.broken) {
      vscode.window.showInformationMessage("Renium: that file is not a read-only link target.");
      return;
    }
    const confirm = await vscode.window.showWarningMessage(
      `Break the link on ${info.service}.${info.pathSegments[info.pathSegments.length - 1] ?? ""}? It will become an ordinary editable script.`,
      { modal: true },
      "Break link",
    );
    if (confirm !== "Break link") {
      return;
    }
    await this.breakLink(info.service, info.pathSegments);
  }

  public async revealLinkSourceForFile(uri: vscode.Uri | undefined): Promise<void> {
    const target = uri ?? vscode.window.activeTextEditor?.document.uri;
    if (!target) {
      return;
    }
    const info = await this.linkInfoForFile(target);
    if (!info || !info.canonical) {
      vscode.window.showInformationMessage("Renium: no link source is available for that file.");
      return;
    }
    try {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(info.canonical));
      await vscode.window.showTextDocument(doc, { preview: true });
    } catch (err) {
      vscode.window.showWarningMessage(`Renium: could not open link source. ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  public async showLinkStatus(): Promise<void> {
    const status = await this.getLinkStatus(true);
    if (!status || status.manifestExists === false) {
      vscode.window.showInformationMessage('Renium: no links exist yet. Use "Renium: Add Link" to create one.');
      return;
    }
    const targets = Array.isArray(status.targets) ? status.targets : [];
    this.output.appendLine(
      `[renium] link-status: ${status.linkCount ?? 0} link(s), ${targets.length} target(s), ${status.driftedTargets ?? 0} drifted, ${status.brokenTargets ?? 0} broken.`,
    );
    for (const target of targets) {
      const flags = [
        target.broken ? "broken" : target.readOnly ? "read-only" : "writable",
        target.drift ? "drifted" : undefined,
        target.resolved === false ? `unresolved(${target.reason ?? "?"})` : undefined,
      ].filter(Boolean).join(", ");
      this.output.appendLine(`  ${target.service}.${(target.path ?? []).join(".")} [${target.linkId}] ${flags}`);
    }
    this.output.show(true);
    vscode.window.showInformationMessage(
      `Renium links: ${status.linkCount ?? 0} link(s), ${targets.length} target(s), ${status.driftedTargets ?? 0} drifted, ${status.brokenTargets ?? 0} broken.`,
    );
  }

  /** The available packages (links) for the Packages view. */
  public async getLinkPackages(force = false): Promise<CliLinkStatusLink[]> {
    const status = await this.getLinkStatus(force);
    const links = (Array.isArray(status?.links) ? status!.links! : [])
      .filter((link) => link.isPackage === true || /\.(rbsync|renium)$/i.test(String(link.sourcePath ?? "")));
    if (!Array.isArray(status?.targets)) {
      return links;
    }
    const activeTargetCounts = new Map<string, number>();
    for (const target of status.targets) {
      const linkId = typeof target.linkId === "string" ? target.linkId : "";
      if (!linkId || target.broken === true || target.missing === true || target.resolved === false) {
        continue;
      }
      activeTargetCounts.set(linkId, (activeTargetCounts.get(linkId) ?? 0) + 1);
    }
    return links.map((link) => {
      const id = typeof link.id === "string" ? link.id : "";
      return { ...link, targetCount: activeTargetCounts.get(id) ?? 0 };
    });
  }

  private normalizeLinkTargetSegments(service: string, pathSegments: string[]): string[] {
    const segments = pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0);
    return segments[0] === service ? segments.slice(1) : segments;
  }

  private linkTargetDisplay(service: string, pathSegments: string[]): string {
    const normalized = this.normalizeLinkTargetSegments(service, pathSegments);
    return normalized.length > 0 ? `${service}.${normalized.join(".")}` : service;
  }

  private samePathSegments(left: readonly string[], right: readonly string[]): boolean {
    return left.length === right.length && left.every((segment, index) => segment === right[index]);
  }

  private async uniquePackageInsertPath(service: string, requestedPathSegments: string[]): Promise<string[]> {
    const requested = this.normalizeLinkTargetSegments(service, requestedPathSegments);
    if (requested.length === 0) {
      return requestedPathSegments;
    }
    const parent = requested.slice(0, -1);
    const base = requested[requested.length - 1].trim() || "Instance";
    const existingNames = new Set<string>();
    const status = await this.getLinkStatus();
    for (const target of status?.targets ?? []) {
      if (String(target.service ?? "") !== service || target.broken === true || target.missing === true) {
        continue;
      }
      const targetPath = this.normalizeLinkTargetSegments(service, Array.isArray(target.path) ? target.path : []);
      if (targetPath.length === 0 || !this.samePathSegments(targetPath.slice(0, -1), parent)) {
        continue;
      }
      existingNames.add(targetPath[targetPath.length - 1]);
    }
    if (!existingNames.has(base)) {
      return [service, ...parent, base];
    }
    let index = 2;
    let candidate = `${base} Copy`;
    while (existingNames.has(candidate)) {
      candidate = `${base} Copy ${index}`;
      index += 1;
    }
    return [service, ...parent, candidate];
  }

  /** Insert an existing package as a read-only copy at a concrete Explorer path. */
  public async insertPackageAtPath(request: { linkId?: string; service?: string; pathSegments?: string[] }): Promise<void> {
    const linkId = typeof request?.linkId === "string" ? request.linkId.trim() : "";
    const service = typeof request?.service === "string" ? request.service.trim() : "";
    const requestedPathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0)
      : [];
    if (!linkId || !service || requestedPathSegments.length === 0) {
      return;
    }
    const link = (await this.getLinkPackages()).find((candidate) => candidate.id === linkId);
    const name = (link?.rootName && link.rootName.length > 0 ? link.rootName : linkId) ?? linkId;
    const pathSegments = await this.uniquePackageInsertPath(service, requestedPathSegments);
    const targetLabel = this.linkTargetDisplay(service, pathSegments);
    logPackageDragDebug(`packages.insertAtPath: start link=${linkId} target=${targetLabel}`);
    await this.enqueue("Insert link", async () => {
      const runCfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(runCfg, "link-add");
      this.ensureFileExists(command);
      const args = [
        "link-add",
        "-r",
        runCfg.projectRoot,
        "--manifest",
        runCfg.linkSync.manifest,
        "--id",
        linkId,
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      ];
      const run = await this.runCommand(command, args, runCfg.projectRoot, "link-add", runCfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't insert the package. Check the Renium output panel for details.");
      }
      logPackageDragDebug(`packages.insertAtPath: link-add ok link=${linkId} target=${targetLabel}`);
    });
    this.invalidateLinkStatusCache();
    const applyResult = await this.linkApply({
      silent: true,
      refreshExplorer: false,
      forceTargetPaths: [pathSegments],
      linkId,
    });
    logPackageDragDebug(
      `packages.insertAtPath: link-apply ok changed=${Array.isArray(applyResult?.changedPaths) ? applyResult!.changedPaths!.length : 0} targets=${Array.isArray(applyResult?.targetSettingsIds) ? applyResult!.targetSettingsIds!.length : 0}`,
    );
    const decorationsPush = this.pushLinkStateToExplorer().catch(() => undefined);
    await this.refreshFileExplorerServicesSafe([service]);
    void decorationsPush;
    logPackageDragDebug(`packages.insertAtPath: complete link=${linkId} target=${targetLabel}`);
    const normalizedTarget = this.normalizeLinkTargetSegments(service, pathSegments);
    const leaf = normalizedTarget.length > 0 ? normalizedTarget[normalizedTarget.length - 1] : "";
    vscode.window.showInformationMessage(`Renium: inserted "${name}" at ${service}.${leaf}.`);
  }

  private tabInputUris(input: unknown): vscode.Uri[] {
    const candidate = input as { uri?: unknown; original?: unknown; modified?: unknown };
    const uris: vscode.Uri[] = [];
    for (const value of [candidate.uri, candidate.original, candidate.modified]) {
      if (value instanceof vscode.Uri && value.scheme === "file") {
        uris.push(value);
      }
    }
    return uris;
  }

  private async closeFileTabs(filePaths: readonly string[] | undefined): Promise<void> {
    const pathKeys = new Set(
      (filePaths ?? [])
        .map((filePath) => String(filePath || "").trim())
        .filter((filePath) => filePath.length > 0)
        .map((filePath) => this.normalizeLinkPathKey(filePath)),
    );
    if (pathKeys.size === 0) {
      return;
    }
    const tabs: vscode.Tab[] = [];
    for (const group of vscode.window.tabGroups.all) {
      for (const tab of group.tabs) {
        if (this.tabInputUris(tab.input).some((uri) => pathKeys.has(this.normalizeLinkPathKey(uri.fsPath)))) {
          tabs.push(tab);
        }
      }
    }
    if (tabs.length > 0) {
      await vscode.window.tabGroups.close(tabs, true);
    }
  }

  public async deletePackage(rawLink: CliLinkStatusLink | string | undefined): Promise<void> {
    const link = typeof rawLink === "string"
      ? (await this.getLinkPackages(true)).find((candidate) => candidate.id === rawLink)
      : rawLink;
    if (!link?.id) {
      vscode.window.showInformationMessage("Renium: select a package to delete.");
      return;
    }
    const fresh = (await this.getLinkPackages(true)).find((candidate) => candidate.id === link.id) ?? link;
    const label = (fresh.rootName && fresh.rootName.length > 0 ? fresh.rootName : fresh.id) ?? fresh.id;
    const uses = Math.max(0, Number(fresh.targetCount ?? 0));
    let action = "delete-unused";
    if (uses > 0) {
      const picked = await vscode.window.showWarningMessage(
        `Delete package "${label}" completely? It has ${uses} active use${uses === 1 ? "" : "s"}. What should happen to those uses?`,
        { modal: true },
        "Delete all uses",
        "Unlink uses",
      );
      if (picked === "Delete all uses") {
        action = "delete-uses";
      } else if (picked === "Unlink uses") {
        action = "unlink-uses";
      } else {
        return;
      }
    }

    let result: CliLinkDeletePackageResult | undefined;
    await this.enqueue("Delete link package", async () => {
      const cfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(cfg, "link-delete-package");
      this.ensureFileExists(command);
      const args = [
        "link-delete-package",
        "-r",
        cfg.projectRoot,
        "-d",
        "src",
        "--manifest",
        cfg.linkSync.manifest,
        "--id",
        fresh.id ?? "",
        "--action",
        action,
      ];
      const run = await this.runCommand(command, args, cfg.projectRoot, "link-delete-package", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't delete the package. Check the Renium output panel for details.");
      }
      const parsed = this.parseCliJsonObject<CliLinkDeletePackageResult>(run.output);
      if (!parsed || parsed.ok === false) {
        throw new Error("Package delete did not return a valid Renium result.");
      }
      result = parsed;
    });

    this.invalidateLinkStatusCache();
    const removedSourcePaths = (Array.isArray(result?.removedSourcePaths) ? result!.removedSourcePaths! : [])
      .filter((filePath): filePath is string => typeof filePath === "string" && filePath.length > 0);
    await this.closeFileTabs(removedSourcePaths);
    const changedPaths = (Array.isArray(result?.changedPaths) ? result!.changedPaths! : [])
      .filter((filePath): filePath is string => typeof filePath === "string" && filePath.length > 0);
    if (changedPaths.length > 0 && this.isEditorLiveSyncActive()) {
      this.noteProgrammaticEditorWrite({ paths: changedPaths, durationMs: 5000 });
      await this.pushEditorPathsNow(changedPaths, {
        force: true,
        skipChangeFilter: true,
      });
    }
    const services = (Array.isArray(result?.services) ? result!.services! : [])
      .filter((service): service is string => typeof service === "string" && service.length > 0);
    if (services.length > 0) {
      await this.refreshFileExplorerServicesSafe(services);
    } else {
      await this.refreshFileExplorerSafe();
    }
    await this.pushLinkStateToExplorer();
    void vscode.commands.executeCommand("renium.packages.refresh");
    const deleted = Array.isArray(result?.deletedTargets) ? result!.deletedTargets!.length : 0;
    const unlinked = Array.isArray(result?.unlinkedTargets) ? result!.unlinkedTargets!.length : 0;
    const suffix = deleted > 0 ? ` Deleted ${deleted} use${deleted === 1 ? "" : "s"}.`
      : unlinked > 0 ? ` Unlinked ${unlinked} use${unlinked === 1 ? "" : "s"}.`
        : "";
    vscode.window.showInformationMessage(`Renium: deleted package "${label}".${suffix}`);
  }

  public async viewPackageUses(rawLink: CliLinkStatusLink | string | undefined): Promise<void> {
    const link = typeof rawLink === "string"
      ? (await this.getLinkPackages(true)).find((candidate) => candidate.id === rawLink)
      : rawLink;
    if (!link?.id) {
      vscode.window.showInformationMessage("Renium: select a package to view uses.");
      return;
    }
    const status = await this.getLinkStatus(true);
    const targets = (status?.targets ?? []).filter((target) => target.linkId === link.id);
    const label = (link.rootName && link.rootName.length > 0 ? link.rootName : link.id) ?? link.id;
    if (targets.length === 0) {
      vscode.window.showInformationMessage(`Renium: package "${label}" has no uses.`);
      return;
    }
    const items = targets.map((target) => {
      const path = [target.service ?? "?", ...(target.path ?? [])].join(".");
      const state = target.broken
        ? "broken"
        : target.missing
          ? "missing"
          : target.drift
            ? "drifted"
            : undefined;
      return {
        label: `$(link) ${path}`,
        description: state,
        target,
      };
    });
    await vscode.window.showQuickPick(items, {
      title: `Uses of "${label}" (${targets.length})`,
      placeHolder: "All instances linked to this package",
      matchOnDescription: true,
    });
  }

  public async revealPackageSource(rawLink: CliLinkStatusLink | string | undefined): Promise<void> {
    const link = typeof rawLink === "string"
      ? (await this.getLinkPackages(true)).find((candidate) => candidate.id === rawLink)
      : rawLink;
    if (!link || typeof link.sourcePath !== "string" || link.sourcePath.length === 0) {
      vscode.window.showInformationMessage("Renium: this package has no local source to preview.");
      return;
    }
    if (!/\.(rbsync|renium)$/i.test(link.sourcePath)) {
      try {
        const stat = fs.existsSync(link.sourcePath) ? fs.statSync(link.sourcePath) : undefined;
        if (stat?.isDirectory()) {
          await vscode.commands.executeCommand("revealInExplorer", vscode.Uri.file(link.sourcePath));
          return;
        }
        const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(link.sourcePath));
        await vscode.window.showTextDocument(doc, { preview: true });
        return;
      } catch (err) {
        vscode.window.showWarningMessage(`Renium: could not open link source. ${err instanceof Error ? err.message : String(err)}`);
        return;
      }
    }
    try {
      const preview = await this.loadPackagePreview(link);
      this.showPackagePreview(preview);
    } catch (err) {
      vscode.window.showWarningMessage(`Renium: could not preview package. ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  public async loadPackagePreview(link: CliLinkStatusLink): Promise<PackagePreviewData> {
    if (!link.id || !link.sourcePath) {
      throw new Error("Package link is missing an id or source path.");
    }
    const cfg = this.getConfig();
    const command = this.resolveRustCliPathForCommand(cfg, "bytecode-explorer-batch");
    this.ensureFileExists(command);
    const args = [
      "bytecode-explorer-batch",
      "-f",
      link.sourcePath,
      "-j",
      JSON.stringify([{ type: "service", fields: "brief,parentId,tree,properties,attributes" }]),
      "-o",
      "full",
    ];
    const run = await this.runCommand(command, args, cfg.projectRoot, "package-preview", cfg.progressHeartbeatSeconds, { quietLog: true });
    if (run.code !== 0) {
      throw new Error("Couldn't preview the package. Check the Renium output panel for details.");
    }
    const parsed = this.parseCliJsonObject<{
      results?: Array<{ type?: string; nodes?: PackagePreviewNode[]; rootIds?: string[] }>;
    }>(run.output);
    const service = parsed?.results?.find((result) => result.type === "service") ?? parsed?.results?.[0];
    const nodes = Array.isArray(service?.nodes) ? service.nodes : [];
    const rootIds = Array.isArray(service?.rootIds) ? service.rootIds : [];
    const name = (link.rootName && link.rootName.length > 0 ? link.rootName : link.id) ?? link.id;
    return {
      id: link.id,
      name,
      source: link.source,
      sourcePath: link.sourcePath,
      rootClass: link.rootClass,
      rootName: link.rootName,
      nodes,
      rootIds,
    };
  }

  private showPackagePreview(preview: PackagePreviewData): void {
    const panel = vscode.window.createWebviewPanel(
      "renium.packagePreview",
      `Package: ${preview.name}`,
      vscode.ViewColumn.Active,
      { enableScripts: true },
    );
    panel.webview.html = packagePreviewHtml(preview);
  }

  /** Cached `renium link-status` used by file-explorer decorations. */
  public async getLinkStatus(force = false): Promise<CliLinkStatusResult | undefined> {
    const now = Date.now();
    if (!force && this.linkStatusCache && now - this.linkStatusCache.at < 2000) {
      return this.linkStatusCache.value;
    }
    if (this.linkStatusInflight) {
      return this.linkStatusInflight;
    }
    const inflight = this.fetchLinkStatus(now);
    this.linkStatusInflight = inflight;
    try {
      return await inflight;
    } finally {
      this.linkStatusInflight = undefined;
    }
  }

  private async fetchLinkStatus(now: number): Promise<CliLinkStatusResult | undefined> {
    let value: CliLinkStatusResult | undefined;
    try {
      const cfg = this.getConfig();
      const manifestPath = this.linkManifestPath(cfg);
      if (fs.existsSync(manifestPath)) {
        const command = this.resolveRustCliPathForCommand(cfg, "link-status");
        if (fs.existsSync(command)) {
          const args = ["link-status", "-r", cfg.projectRoot, "-d", "src", "--manifest", cfg.linkSync.manifest];
          if (cfg.linkSync.cacheDir.length > 0) {
            args.push("--cache-dir", cfg.linkSync.cacheDir);
          }
          const run = await this.runCommand(command, args, cfg.projectRoot, "link-status", cfg.progressHeartbeatSeconds, { quietLog: true });
          if (run.code === 0) {
            value = this.parseCliJsonObject<CliLinkStatusResult>(run.output) ?? undefined;
          }
        }
      }
    } catch (err) {
      this.output.appendLine(`[renium] link-status failed: ${err instanceof Error ? err.message : String(err)}`);
    }
    this.linkStatusCache = { at: now, value };
    return value;
  }

  private async refreshFileExplorerSafe(): Promise<void> {
    try {
      await vscode.commands.executeCommand("renium.fileExplorer.refresh");
    } catch {
    }
  }

  private async refreshFileExplorerServicesSafe(services: string[]): Promise<void> {
    try {
      await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", services);
    } catch {
      await this.refreshFileExplorerSafe();
    }
  }

  public async syncActiveService(): Promise<void> {
    await this.enqueue("Sync active service", async () => {
      const cfg = this.getConfig();
      const activePath = vscode.window.activeTextEditor?.document.uri.fsPath;

      let service = activePath ? this.detectServiceForPath(activePath, cfg.projectRoot, cfg.services) : undefined;

      if (!service) {
        service = await vscode.window.showQuickPick(cfg.services, {
          title: "Renium",
          placeHolder: "Select a service to sync",
        });
      }

      if (!service) {
        return;
      }

      await this.runExport({
        services: [service],
        runImport: cfg.runImport,
        notifyOnSuccess: true,
        reason: `Synced ${service}`,
      });
    });
  }

  public async openGitSync(): Promise<void> {
    await vscode.commands.executeCommand("workbench.view.extension.reniumContainer");
    await vscode.commands.executeCommand("renium.fileExplorer.showGit");
  }

  public async gitStatus(): Promise<void> {
    await this.enqueue("Git status", async () => {
      const state = await this.inspectGitRepo(this.getConfig(), { fetch: false });
      this.output.show(false);
      this.logGitState(state);
      await this.refreshGitView();
    });
  }

  public async gitFetch(): Promise<void> {
    await this.enqueue("Git fetch", async () => {
      const cfg = this.getConfig();
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      const remote = state.remote ?? cfg.gitSync.remote;
      const result = await this.runGitCommand(cfg, repoRoot, ["fetch", "--prune", remote], "fetch");
      this.ensureGitSuccess(result, "fetch");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: fetched ${remote}.`);
    });
  }

  public async gitPull(): Promise<void> {
    await this.enqueue("Git pull", async () => {
      const cfg = this.getConfig();
      this.ensureWorkspaceTrustedForGitSync();
      await this.ensureLiveSyncStoppedForGitPull();

      let state = await this.inspectGitRepo(cfg, { fetch: cfg.gitSync.autoFetch, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      this.ensureNoGitConflicts(state);
      if (cfg.gitSync.requireCleanWorktreeBeforePull && state.entries.length > 0) {
        throw new Error("Pull is blocked because the worktree has local changes. Commit, stash, or discard them before pulling.");
      }

      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);
      if (state.upstream && state.behind === 0 && state.ahead === 0) {
        this.output.appendLine(`[git-sync] pull skipped: ${remote}/${branch} is already up to date.`);
        vscode.window.showInformationMessage("Renium: Git pull is already up to date.");
        return;
      }
      if (state.upstream && state.ahead > 0 && state.behind > 0) {
        throw new Error("Pull is blocked because the branch has diverged. Resolve with VS Code Source Control or git manually.");
      }

      const oldHead = await this.gitOutput(cfg, repoRoot, ["rev-parse", "HEAD"], "read HEAD");
      const pullResult = await this.runGitCommand(cfg, repoRoot, ["pull", "--ff-only", remote, branch], "pull --ff-only");
      this.ensureGitSuccess(pullResult, "pull --ff-only");
      const newHead = await this.gitOutput(cfg, repoRoot, ["rev-parse", "HEAD"], "read HEAD after pull");
      const changedFiles = oldHead !== newHead
        ? await this.gitChangedFilesBetween(cfg, repoRoot, oldHead, newHead)
        : [];
      await this.refreshExplorerForGitPaths(repoRoot, changedFiles, cfg);
      await this.maybeApplyPulledPathsToStudio(repoRoot, changedFiles, cfg);
      state = await this.inspectGitRepo(cfg, { fetch: false });
      this.logGitState(state);
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: pulled ${remote}/${branch}.`);
    });
  }

  public async gitCommitAndPush(options: { runFullSyncFirst?: boolean } = {}): Promise<void> {
    await this.enqueue(options.runFullSyncFirst ? "Git full sync + push" : "Git commit & push", async () => {
      const cfg = this.getConfig();
      this.ensureWorkspaceTrustedForGitSync();
      await this.maybeRunFullSyncBeforeGitPush(cfg, options.runFullSyncFirst === true);

      let state = await this.inspectGitRepo(cfg, { fetch: cfg.gitSync.autoFetch, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      this.ensureNoGitConflicts(state);
      if (state.behind > 0) {
        throw new Error("Push is blocked because the remote has new commits. Pull first, then retry.");
      }
      const preexistingStaged = await this.gitStagedChanges(cfg, repoRoot);
      if (preexistingStaged.length > 0) {
        throw new Error(`Push is blocked because ${preexistingStaged.length} file(s) are already staged. Commit or unstage them first so Renium does not publish unintended changes.`);
      }

      const plannedChanges = await this.plannedGitStageChanges(cfg, repoRoot);
      state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);

      if (plannedChanges.length === 0) {
        if (state.ahead > 0) {
          await this.confirmGitPush(`No new files were staged, but ${state.ahead} local commit(s) are ahead of ${state.upstream ?? remote + "/" + branch}. Push them now?`, cfg);
          await this.pushGitBranch(cfg, repoRoot, remote, branch, state.upstream === undefined);
          await this.refreshGitView();
          return;
        }
        throw new Error("No tracked changes are available to commit. Untracked files are excluded unless renium.gitSync.includeUntracked is enabled or stage paths are configured.");
      }

      await this.confirmGitCommitAndPush(plannedChanges, state, cfg);
      await this.stageGitSyncChanges(cfg, repoRoot);
      const staged = await this.gitStagedChanges(cfg, repoRoot);
      if (staged.length === 0) {
        throw new Error("No files were staged after applying the configured Git sync path filters.");
      }
      const commitMessage = await this.gitCommitMessage(cfg, branch);
      const commitResult = await this.runGitCommand(cfg, repoRoot, ["commit", "-m", commitMessage], "commit");
      this.ensureGitSuccess(commitResult, "commit");
      const shortSha = await this.gitOutput(cfg, repoRoot, ["rev-parse", "--short", "HEAD"], "read commit sha");
      await this.pushGitBranch(cfg, repoRoot, remote, branch, state.upstream === undefined);
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: pushed ${shortSha} to ${remote}/${branch}.`);
    });
  }

  public async gitPublishBranch(): Promise<void> {
    await this.enqueue("Git publish branch", async () => {
      const cfg = this.getConfig();
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);
      await this.confirmGitPush(`Publish current branch to ${remote}/${branch}?`, cfg);
      await this.pushGitBranch(cfg, repoRoot, remote, branch, true);
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: published ${remote}/${branch}.`);
    });
  }

  public async gitCreateBranch(): Promise<void> {
    const branchName = await vscode.window.showInputBox({
      title: "Create Git Branch",
      prompt: "New branch name",
      validateInput: (value) => this.validateBranchName(value),
    });
    if (!branchName) {
      return;
    }
    await this.enqueue("Git create branch", async () => {
      const cfg = this.getConfig();
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      const repoRoot = this.requireGitRepoRoot(state);
      const result = await this.runGitCommand(cfg, repoRoot, ["switch", "-c", branchName.trim()], "create branch");
      this.ensureGitSuccess(result, "create branch");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: created branch ${branchName.trim()}.`);
    });
  }

  public async gitCheckoutBranch(): Promise<void> {
    const cfg = this.getConfig();
    const state = await this.inspectGitRepo(cfg, { fetch: false });
    const repoRoot = this.requireGitRepoRoot(state);
    if (state.entries.length > 0) {
      vscode.window.showWarningMessage("Renium: checkout is blocked while local changes are present.");
      return;
    }
    const branchesResult = await this.runGitCommand(cfg, repoRoot, ["branch", "--format=%(refname:short)"], "list branches", { quiet: true });
    this.ensureGitSuccess(branchesResult, "list branches");
    const branches = branchesResult.stdout.split(/\r?\n/).map((line) => line.trim()).filter((line) => line.length > 0);
    const branchName = await vscode.window.showQuickPick(branches, { title: "Checkout Git Branch" });
    if (!branchName) {
      return;
    }
    await this.enqueue("Git checkout branch", async () => {
      const runCfg = this.getConfig();
      const result = await this.runGitCommand(runCfg, repoRoot, ["switch", branchName], "checkout branch");
      this.ensureGitSuccess(result, "checkout branch");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: checked out ${branchName}.`);
    });
  }

  public async gitConnectRepo(): Promise<void> {
    const cfg = this.getConfig();
    this.ensureWorkspaceTrustedForGitSync();
    const state = await this.inspectGitRepo(cfg, { fetch: false, allowMissing: true });
    let repoRoot = state.repoRoot;
    if (!repoRoot) {
      const init = await vscode.window.showWarningMessage(
        `Initialize a Git repository at ${cfg.projectRoot}?`,
        { modal: true },
        "Initialize Repository",
      );
      if (init !== "Initialize Repository") {
        return;
      }
      repoRoot = cfg.projectRoot;
    }
    const remote = await vscode.window.showInputBox({
      title: "Git Remote Name",
      value: cfg.gitSync.remote,
      prompt: "Remote name to connect to Git",
    });
    if (!remote) {
      return;
    }
    const remoteUrl = await vscode.window.showInputBox({
      title: "Git Remote URL",
      value: "",
      placeHolder: state.view.remoteUrl ? `Current: ${state.view.remoteUrl}` : "https://github.com/owner/repo.git or git@github.com:owner/repo.git",
      prompt: "HTTPS or SSH Git repository URL",
      ignoreFocusOut: true,
    });
    if (!remoteUrl) {
      return;
    }

    await this.enqueue("Git connect repo", async () => {
      const runCfg = this.getConfig();
      if (!state.repoRoot) {
        const initResult = await this.runGitCommand(runCfg, runCfg.projectRoot, ["init"], "git init");
        this.ensureGitSuccess(initResult, "git init");
      }
      const targetRoot = repoRoot ?? runCfg.projectRoot;
      const currentRemote = await this.runGitCommand(runCfg, targetRoot, ["remote", "get-url", remote.trim()], "get remote", { quiet: true });
      const args = currentRemote.code === 0
        ? ["remote", "set-url", remote.trim(), remoteUrl.trim()]
        : ["remote", "add", remote.trim(), remoteUrl.trim()];
      const remoteResult = await this.runGitCommand(runCfg, targetRoot, args, currentRemote.code === 0 ? "set remote" : "add remote");
      this.ensureGitSuccess(remoteResult, currentRemote.code === 0 ? "set remote" : "add remote");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Renium: connected ${remote.trim()} to ${redactRemoteUrl(remoteUrl.trim())}.`);
    });
  }

  public async gitOpenRemote(): Promise<void> {
    const state = await this.inspectGitRepo(this.getConfig(), { fetch: false, allowMissing: true });
    const remoteWebUrl = state.view.remoteWebUrl;
    if (!remoteWebUrl) {
      vscode.window.showWarningMessage("Renium: no Git remote URL is configured.");
      return;
    }
    await vscode.env.openExternal(vscode.Uri.parse(remoteWebUrl));
  }

  private async getGitViewState(options: { fetch?: boolean } = {}): Promise<GitViewState> {
    return (await this.inspectGitRepo(this.getConfig(), { fetch: options.fetch === true, allowMissing: true })).view;
  }

  private async refreshGitView(options: { fetch?: boolean } = {}): Promise<void> {
    await vscode.commands.executeCommand("renium.fileExplorer.refreshGit", options);
  }

  private async runGitViewAction(action: string): Promise<void> {
    switch (action) {
      case "connect":
        await this.gitConnectRepo();
        return;
      case "fetch":
        await this.gitFetch();
        return;
      case "pull":
        await this.gitPull();
        return;
      case "commitPush":
        await this.gitCommitAndPush();
        return;
      case "syncCommitPush":
        await this.gitCommitAndPush({ runFullSyncFirst: true });
        return;
      case "publishBranch":
        await this.gitPublishBranch();
        return;
      case "createBranch":
        await this.gitCreateBranch();
        return;
      case "checkoutBranch":
        await this.gitCheckoutBranch();
        return;
      case "openRemote":
        await this.gitOpenRemote();
        return;
      case "status":
        await this.gitStatus();
        return;
      default:
        return;
    }
  }

  private emptyGitViewState(message?: string): GitViewState {
    return {
      ok: false,
      message,
      trusted: vscode.workspace.isTrusted,
      projectRoot: this.getConfig().projectRoot,
      connected: false,
      ahead: 0,
      behind: 0,
      counts: { total: 0, tracked: 0, staged: 0, unstaged: 0, untracked: 0, ignored: 0, conflicted: 0, deleted: 0 },
      entries: [],
      lastUpdated: new Date().toISOString(),
    };
  }

  private async inspectGitRepo(
    cfg: SyncConfig,
    options: { fetch?: boolean; requireRemote?: boolean; allowMissing?: boolean } = {},
  ): Promise<GitRepoState> {
    if (!vscode.workspace.isTrusted) {
      const view = this.emptyGitViewState("Workspace is not trusted. Trust this workspace before using Git sync.");
      if (options.allowMissing) {
        return { view, entries: [], ahead: 0, behind: 0 };
      }
      throw new Error(view.message);
    }

    const repoResult = await this.runGitCommand(cfg, cfg.projectRoot, ["rev-parse", "--show-toplevel"], "repo root", { quiet: true });
    if (repoResult.code !== 0) {
      const view = this.emptyGitViewState("No Git repository is connected. Use Connect Repo to initialize or configure one.");
      if (options.allowMissing) {
        return { view, entries: [], ahead: 0, behind: 0 };
      }
      throw new Error(view.message);
    }

    const repoRoot = path.normalize(repoResult.stdout.trim());
    if (!this.isPathInside(cfg.projectRoot, repoRoot)) {
      throw new Error(`Configured projectRoot is outside the Git repository: ${cfg.projectRoot}`);
    }

    const branchResult = await this.runGitCommand(cfg, repoRoot, ["branch", "--show-current"], "branch", { quiet: true });
    const branch = branchResult.code === 0 ? branchResult.stdout.trim() : "";
    const configuredRemote = cfg.gitSync.remote || "origin";
    const remoteResult = await this.runGitCommand(cfg, repoRoot, ["remote", "get-url", configuredRemote], "remote", { quiet: true });
    const remoteUrl = remoteResult.code === 0 ? remoteResult.stdout.trim() : undefined;
    if (options.requireRemote && !remoteUrl) {
      throw new Error(`Git remote '${configuredRemote}' is not configured. Use the Git tab's Connect Repo action.`);
    }

    if (options.fetch && remoteUrl) {
      const fetchResult = await this.runGitCommand(cfg, repoRoot, ["fetch", "--prune", configuredRemote], "fetch");
      this.ensureGitSuccess(fetchResult, "fetch");
    }

    const upstreamResult = await this.runGitCommand(
      cfg,
      repoRoot,
      ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
      "upstream",
      { quiet: true },
    );
    const upstream = upstreamResult.code === 0 ? upstreamResult.stdout.trim() : undefined;
    let ahead = 0;
    let behind = 0;
    if (upstream) {
      const aheadBehindResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-list", "--left-right", "--count", "HEAD...@{u}"],
        "ahead/behind",
        { quiet: true },
      );
      if (aheadBehindResult.code === 0) {
        ({ ahead, behind } = parseAheadBehind(aheadBehindResult.stdout));
      }
    }

    const statusScope = defaultGitSyncScope(repoRoot, cfg.projectRoot);
    const statusResult = await this.runGitCommand(cfg, repoRoot, ["status", "--porcelain=v1", "-z", "-uall", "--", statusScope], "status", { quiet: true });
    this.ensureGitSuccess(statusResult, "status");
    const entries = parsePorcelainV1Z(statusResult.stdout);
    const counts = summarizeStatus(entries);
    const redactedRemoteUrl = remoteUrl ? redactRemoteUrl(remoteUrl) : undefined;
    const remoteWebUrl = remoteUrlToWebUrl(remoteUrl ?? "");
    const message = !remoteUrl
      ? `Remote '${configuredRemote}' is not configured.`
      : counts.conflicted > 0
        ? `${counts.conflicted} conflicted file(s) need manual resolution.`
        : behind > 0
          ? `${behind} remote commit(s) available to pull.`
          : undefined;
    const view: GitViewState = {
      ok: Boolean(remoteUrl) && counts.conflicted === 0,
      message,
      trusted: true,
      projectRoot: cfg.projectRoot,
      repoRoot,
      connected: Boolean(remoteUrl),
      branch: branch || undefined,
      upstream,
      remote: configuredRemote,
      remoteUrl: redactedRemoteUrl,
      remoteWebUrl,
      ahead,
      behind,
      counts,
      entries: entries.map((entry) => ({
        path: entry.path,
        originalPath: entry.originalPath,
        kind: entry.kind,
        staged: entry.staged,
        unstaged: entry.unstaged,
        untracked: entry.untracked,
        conflicted: entry.conflicted,
        deleted: entry.deleted,
      })),
      lastUpdated: new Date().toISOString(),
    };
    return { view, entries, repoRoot, branch, upstream, remote: configuredRemote, remoteUrl, ahead, behind };
  }

  private ensureWorkspaceTrustedForGitSync(): void {
    if (!vscode.workspace.isTrusted) {
      throw new Error("Workspace is not trusted. Trust this workspace before running Git sync commands.");
    }
  }

  private requireGitRepoRoot(state: GitRepoState): string {
    if (!state.repoRoot) {
      throw new Error(state.view.message || "No Git repository is connected.");
    }
    return state.repoRoot;
  }

  private ensureNoGitConflicts(state: GitRepoState): void {
    const conflicts = state.entries.filter((entry) => entry.conflicted);
    if (conflicts.length > 0) {
      throw new Error(`Git sync is blocked by ${conflicts.length} conflicted file(s). Resolve conflicts before continuing.`);
    }
  }

  private resolveGitBranch(cfg: SyncConfig, state: GitRepoState): string {
    const branch = cfg.gitSync.branch.trim() || state.branch?.trim() || "";
    if (!branch) {
      throw new Error("Current Git HEAD is detached. Checkout or create a branch before using Git sync.");
    }
    return branch;
  }

  private async runGitCommand(
    cfg: SyncConfig,
    cwd: string,
    args: string[],
    label: string,
    options: { quiet?: boolean } = {},
  ): Promise<GitRunResult> {
    const quiet = options.quiet === true || cfg.gitSync.outputBehavior === "silent";
    if (!quiet && cfg.gitSync.outputBehavior === "onStart") {
      this.output.show(false);
    }
    if (!quiet) {
      this.output.appendLine(`[git-sync] ${label}: git ${renderGitArgs(args)}`);
    }
    const result = await runGit(args, {
      cwd,
      gitPath: cfg.gitSync.gitPath,
      timeoutMs: Math.max(10, cfg.gitSync.timeoutSeconds) * 1000,
    });
    if (!quiet) {
      const output = redactRemoteUrl(result.output.trim());
      if (output) {
        for (const line of output.replace(/\r\n/g, "\n").split("\n").slice(-80)) {
          this.output.appendLine(`[git-sync:git] ${line}`);
        }
      }
      this.output.appendLine(`[git-sync] ${label}: exited code=${result.code}${result.timedOut ? " (timed out)" : ""}`);
    }
    return result;
  }

  private ensureGitSuccess(result: GitRunResult, label: string): void {
    if (result.code === 0 && !result.timedOut) {
      return;
    }
    const detail = redactRemoteUrl((result.stderr || result.stdout || result.output || "").trim());
    const timeout = result.timedOut ? " timed out" : "";
    throw new Error(`Git ${label}${timeout} failed with code ${result.code}.${detail ? ` ${detail}` : ""}`);
  }

  private async gitOutput(cfg: SyncConfig, repoRoot: string, args: string[], label: string): Promise<string> {
    const result = await this.runGitCommand(cfg, repoRoot, args, label, { quiet: true });
    this.ensureGitSuccess(result, label);
    return result.stdout.trim();
  }

  private async gitChangedFilesBetween(cfg: SyncConfig, repoRoot: string, oldHead: string, newHead: string): Promise<GitNameStatusEntry[]> {
    const result = await this.runGitCommand(cfg, repoRoot, ["diff", "--name-status", "-z", oldHead, newHead], "changed files", { quiet: true });
    this.ensureGitSuccess(result, "changed files");
    return parseNameStatusZ(result.stdout);
  }

  private async gitStagedChanges(cfg: SyncConfig, repoRoot: string): Promise<GitNameStatusEntry[]> {
    const result = await this.runGitCommand(cfg, repoRoot, ["diff", "--cached", "--name-status", "-z"], "staged changes", { quiet: true });
    this.ensureGitSuccess(result, "staged changes");
    return parseNameStatusZ(result.stdout);
  }

  private async refreshExplorerForGitPaths(repoRoot: string, changedFiles: GitNameStatusEntry[], cfg: SyncConfig): Promise<void> {
    const services = new Set<string>();
    for (const affectedPath of nameStatusAffectedPaths(changedFiles)) {
      const absolutePath = path.join(repoRoot, affectedPath);
      const service = this.detectServiceForPath(absolutePath, cfg.projectRoot, cfg.services);
      if (service) {
        services.add(service);
      }
    }
    if (services.size === 0) {
      return;
    }
    await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", Array.from(services));
  }

  private async maybeApplyPulledPathsToStudio(repoRoot: string, changedFiles: GitNameStatusEntry[], cfg: SyncConfig): Promise<void> {
    const srcPaths = nameStatusAffectedPaths(changedFiles)
      .map((affectedPath) => path.join(repoRoot, affectedPath))
      .filter((filePath) => this.isPathInside(filePath, path.join(cfg.projectRoot, "src")));
    if (srcPaths.length === 0 || cfg.gitSync.applyPulledChangesToStudio === "never") {
      return;
    }
    let apply = cfg.gitSync.applyPulledChangesToStudio === "always";
    if (!apply) {
      const picked = await vscode.window.showInformationMessage(
        `Apply ${srcPaths.length} pulled src file(s) to Studio now?`,
        { modal: true },
        "Apply to Studio",
      );
      apply = picked === "Apply to Studio";
    }
    if (!apply) {
      return;
    }
    const pushed = await this.pushEditorPathsNow(srcPaths, { force: true, skipChangeFilter: true, taskName: "Git pull -> Studio sync" });
    if (!pushed) {
      vscode.window.showInformationMessage("Renium: pulled changes stayed local. Start Serve or live sync before applying to Studio.");
    }
  }

  private async ensureLiveSyncStoppedForGitPull(): Promise<void> {
    if (!this.isEditorLiveSyncActive() && !this.liveSyncStartPromise) {
      return;
    }
    const picked = await vscode.window.showWarningMessage(
      "Git pull can rewrite src files. Stop Renium live sync before pulling?",
      { modal: true },
      "Stop Live Sync",
    );
    if (picked !== "Stop Live Sync") {
      throw new Error("Git pull cancelled because live sync is active.");
    }
    await this.stopLiveSync();
  }

  private async maybeRunFullSyncBeforeGitPush(cfg: SyncConfig, forced: boolean): Promise<void> {
    let shouldRun = forced || cfg.gitSync.runFullSyncBeforePush === "always";
    if (!shouldRun && cfg.gitSync.runFullSyncBeforePush === "ask") {
      const picked = await vscode.window.showInformationMessage(
        "Run Renium Full Sync before committing to Git?",
        { modal: true },
        "Run Full Sync",
        "Commit Current Files",
      );
      if (!picked) {
        throw new Error("Git commit cancelled before full-sync choice.");
      }
      shouldRun = picked === "Run Full Sync";
    }
    if (!shouldRun) {
      return;
    }
    await this.runExport({
      services: cfg.services,
      runImport: cfg.runImport,
      notifyOnSuccess: false,
      reason: "",
      quietTimings: false,
    });
  }

  private async stageGitSyncChanges(cfg: SyncConfig, repoRoot: string): Promise<void> {
    const configuredPaths = cfg.gitSync.stagePaths.map((value) => value.trim()).filter((value) => value.length > 0);
    const hasConfiguredPaths = cfg.gitSync.stageMode === "configuredPaths" && configuredPaths.length > 0;
    const defaultScope = this.defaultGitStageScope(repoRoot, cfg.projectRoot);
    const args = cfg.gitSync.includeUntracked
      ? ["add", "-A", "--", ...(hasConfiguredPaths ? configuredPaths : [defaultScope])]
      : ["add", "-u", "--", ...(hasConfiguredPaths ? configuredPaths : [defaultScope])];
    const result = await this.runGitCommand(cfg, repoRoot, args, "stage changes");
    this.ensureGitSuccess(result, "stage changes");
  }

  private async plannedGitStageChanges(cfg: SyncConfig, repoRoot: string): Promise<GitNameStatusEntry[]> {
    const configuredPaths = cfg.gitSync.stagePaths.map((value) => value.trim()).filter((value) => value.length > 0);
    const scopes = cfg.gitSync.stageMode === "configuredPaths" && configuredPaths.length > 0
      ? configuredPaths
      : [this.defaultGitStageScope(repoRoot, cfg.projectRoot)];
    const result = await this.runGitCommand(
      cfg,
      repoRoot,
      ["status", "--porcelain=v1", "-z", "-uall", "--", ...scopes],
      "stage preview",
      { quiet: true },
    );
    this.ensureGitSuccess(result, "stage preview");
    return parsePorcelainV1Z(result.stdout)
      .filter((entry) => entry.tracked || cfg.gitSync.includeUntracked)
      .map((entry) => ({
        status: this.gitNameStatusForEntry(entry),
        path: entry.path,
        originalPath: entry.originalPath,
      }));
  }

  private gitNameStatusForEntry(entry: GitStatusEntry): string {
    if (entry.conflicted) {
      return "U";
    }
    if (entry.untracked) {
      return "A";
    }
    const status = entry.index.trim() || entry.worktree.trim();
    return status || "M";
  }

  private defaultGitStageScope(repoRoot: string, projectRoot: string): string {
    return defaultGitSyncScope(repoRoot, projectRoot);
  }

  private async confirmGitCommitAndPush(staged: GitNameStatusEntry[], state: GitRepoState, cfg: SyncConfig): Promise<void> {
    if (!cfg.gitSync.confirmBeforePush) {
      return;
    }
    const deleted = staged.filter((entry) => entry.status.includes("D")).length;
    const summary = `${staged.length} staged file(s)${deleted > 0 ? `, including ${deleted} deletion(s)` : ""}. Push target: ${state.remote}/${this.resolveGitBranch(cfg, state)}.`;
    const picked = await vscode.window.showWarningMessage(
      `${summary}\n\nUntracked files are ${cfg.gitSync.includeUntracked ? "included by setting" : "excluded by default"}.`,
      { modal: true },
      "Commit & Push",
    );
    if (picked !== "Commit & Push") {
      throw new Error("Git commit & push cancelled.");
    }
  }

  private async confirmGitPush(message: string, cfg: SyncConfig): Promise<void> {
    if (!cfg.gitSync.confirmBeforePush) {
      return;
    }
    const picked = await vscode.window.showWarningMessage(message, { modal: true }, "Push");
    if (picked !== "Push") {
      throw new Error("Git push cancelled.");
    }
  }

  private async gitCommitMessage(cfg: SyncConfig, branch: string): Promise<string> {
    const value = await vscode.window.showInputBox({
      title: "Git Commit Message",
      value: buildCommitMessage(cfg.gitSync.commitMessageTemplate, branch),
      prompt: "Commit message for the selected Renium changes",
      ignoreFocusOut: true,
      validateInput: (input) => input.trim().length === 0 ? "Commit message is required." : undefined,
    });
    const message = value?.trim() ?? "";
    if (!message) {
      throw new Error("Git commit cancelled because no commit message was provided.");
    }
    return message;
  }

  private async pushGitBranch(cfg: SyncConfig, repoRoot: string, remote: string, branch: string, setUpstream: boolean): Promise<void> {
    const args = setUpstream
      ? ["push", "-u", remote, `HEAD:${branch}`]
      : ["push", remote, `HEAD:${branch}`];
    const result = await this.runGitCommand(cfg, repoRoot, args, "push");
    this.ensureGitSuccess(result, "push");
  }

  private validateBranchName(value: string): string | undefined {
    const branch = value.trim();
    if (!branch) {
      return "Branch name is required.";
    }
    if (/\s/.test(branch) || branch.startsWith("-") || branch.includes("..") || branch.includes("~") || branch.includes("^") || branch.includes(":")) {
      return "Branch name contains invalid characters.";
    }
    return undefined;
  }

  private logGitState(state: GitRepoState): void {
    const counts = state.view.counts;
    this.output.appendLine(`[git-sync] repo=${state.repoRoot ?? "not connected"}`);
    this.output.appendLine(`[git-sync] branch=${state.branch ?? "detached"} upstream=${state.upstream ?? "none"} remote=${state.remote ?? "none"}`);
    if (state.remoteUrl) {
      this.output.appendLine(`[git-sync] remoteUrl=${redactRemoteUrl(state.remoteUrl)}`);
    }
    this.output.appendLine(`[git-sync] ahead=${state.ahead} behind=${state.behind} changed=${counts.total} staged=${counts.staged} unstaged=${counts.unstaged} untracked=${counts.untracked} conflicts=${counts.conflicted}`);
    for (const entry of state.entries.slice(0, 40)) {
      this.output.appendLine(`[git-sync] ${entry.kind.padEnd(10)} ${entry.path}`);
    }
    if (state.entries.length > 40) {
      this.output.appendLine(`[git-sync] ... ${state.entries.length - 40} more file(s)`);
    }
  }

  public async serve(options: { silent?: boolean; bestEffort?: boolean } = {}): Promise<void> {
    const cfg = this.getConfig();
    if (cfg.transport !== "ws") {
      const message = "Renium: serve requires WebSocket bridge transport.";
      if (options.bestEffort) {
        this.output.appendLine(`[renium] serve skipped: ${message}`);
        return;
      }
      throw new Error(message);
    }

    this.bridgeServeRequested = true;
    this.liveSyncOwnsServe = false;
    try {
      await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: true });
    } catch (err) {
      this.bridgeServeRequested = false;
      this.updateStatusBar();
      if (options.bestEffort) {
        this.output.appendLine(`[renium] serve failed: ${err instanceof Error ? err.message : String(err)}`);
        return;
      }
      throw err;
    }

    this.output.appendLine(`[renium] serve ready: plugin can connect on ${cfg.bridgePorts}`);
    this.updateStatusBar();
    if (!options.silent) {
      vscode.window.showInformationMessage(`Renium: Serve started — Studio can now connect (ports ${cfg.bridgePorts}).`);
    }
  }

  public async stopServe(options: { silent?: boolean } = {}): Promise<void> {
    if (this.liveSyncWatcher || this.liveSyncStartPromise) {
      await this.stopLiveSync({ silent: true });
    }
    this.bridgeServeRequested = false;
    this.liveSyncOwnsServe = false;
    this.stopBridgeDaemon();
    this.updateStatusBar();
    if (!options.silent) {
      vscode.window.showInformationMessage("Renium: serve stopped.");
    }
  }

  public async benchmarkFullSync(): Promise<void> {
    await this.enqueue("Benchmark full sync", async () => {
      const cfg = this.getConfig();
      const runCount = Math.max(1, cfg.benchmarkRuns);
      const runs: BenchmarkRunMetrics[] = [];
      this.output.appendLine(`[renium] benchmark: running 1 warm-up + ${runCount} measured full sync iterations`);

      this.output.appendLine("[renium] benchmark: warm-up start (not counted)");
        const warmupResult = await this.runExport({
          services: cfg.services,
          runImport: cfg.runImport,
          notifyOnSuccess: false,
          reason: "",
          quietTimings: false,
        });
      const warmupMetrics = this.parseBenchmarkMetrics(warmupResult.output);
      this.logBenchmarkRun("[renium] benchmark: warm-up", warmupMetrics);

      for (let index = 0; index < runCount; index += 1) {
        this.output.appendLine(`[renium] benchmark: run ${index + 1}/${runCount} start`);
        const result = await this.runExport({
          services: cfg.services,
          runImport: cfg.runImport,
          notifyOnSuccess: false,
          reason: "",
          quietTimings: false,
        });
        const metrics = this.parseBenchmarkMetrics(result.output);
        runs.push(metrics);
        this.logBenchmarkRun(`[renium] benchmark: run ${index + 1}/${runCount}`, metrics);
        if (metrics.exportFingerprint) {
          this.output.appendLine(`[renium] benchmark: run ${index + 1}/${runCount} export=${metrics.exportFingerprint}`);
        }
        if (metrics.bridgeFingerprint) {
          this.output.appendLine(`[renium] benchmark: run ${index + 1}/${runCount} bridge=${metrics.bridgeFingerprint}`);
        }
      }

      this.output.appendLine("[renium] benchmark summary:");
      this.output.appendLine(
        `[renium] benchmark: total ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.totalMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.totalMs), 0.9))} min=${this.formatMetricMs(this.minMetric(runs.map((run) => run.totalMs)))} max=${this.formatMetricMs(this.maxMetric(runs.map((run) => run.totalMs)))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service instance fetch ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceInstanceFetchMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceInstanceFetchMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service plugin server ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginServerMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginServerMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service plugin encode ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginEncodeMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginEncodeMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service payload bytes p50=${this.formatMetricBytes(this.percentile(runs.map((run) => run.trackedServicePayloadBytes), 0.5))} p90=${this.formatMetricBytes(this.percentile(runs.map((run) => run.trackedServicePayloadBytes), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service chunk count p50=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceChunkCount), 0.5))} p90=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceChunkCount), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service max frame ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceMaxFrameMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceMaxFrameMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service stall count >50ms p50=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver50Ms), 0.5))} p90=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver50Ms), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: tracked-service stall count >100ms p50=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver100Ms), 0.5))} p90=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver100Ms), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: core export ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.coreExportMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.coreExportMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: bridge startup ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.bridgeStartupMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.bridgeStartupMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: handshake ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.handshakeMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.handshakeMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: service export sum ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.serviceExportSumMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.serviceExportSumMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: import tail ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.importCriticalTailMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.importCriticalTailMs), 0.9))}`,
      );
      this.output.appendLine(
        `[renium] benchmark: unmeasured/scheduler gap ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.unmeasuredOrSchedulerGapMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.unmeasuredOrSchedulerGapMs), 0.9))}`,
      );
      const lastRun = runs[runs.length - 1];
      if (lastRun?.exportFingerprint) {
        this.output.appendLine(`[renium] benchmark: export fingerprint=${lastRun.exportFingerprint}`);
      }
      if (lastRun?.bridgeFingerprint) {
        this.output.appendLine(`[renium] benchmark: bridge fingerprint=${lastRun.bridgeFingerprint}`);
      }
      const benchmarkPath = path.join(cfg.projectRoot, ".renium", "benchmark-full-sync.latest.json");
      const benchmarkPayload = {
        generatedAt: new Date().toISOString(),
        runCount,
        services: cfg.services,
        runImport: cfg.runImport,
        importMode: cfg.importMode,
        performanceMode: cfg.performanceMode,
        modifiedDefaultBypass: cfg.modifiedDefaultBypass,
        chunkSize: cfg.chunkSize,
        bridgePorts: cfg.bridgePorts,
        warmup: warmupMetrics,
        summary: this.buildBenchmarkSummary(runs),
        runs: runs.map((metrics, index) => ({
          index: index + 1,
          ...metrics,
        })),
      };
      fs.mkdirSync(path.dirname(benchmarkPath), { recursive: true });
      fs.writeFileSync(benchmarkPath, JSON.stringify(benchmarkPayload, null, 2), "utf8");
      this.output.appendLine(`[renium] benchmark: saved metrics JSON to ${benchmarkPath}`);
      vscode.window.showInformationMessage(`Renium: benchmark full sync saved to ${benchmarkPath}.`);
    });
  }

  public async benchmarkModifiedDefaultBypassAB(): Promise<void> {
    await this.enqueue("Benchmark modified-default bypass A/B", async () => {
      const baseCfg = this.getConfig();
      const runCount = Math.max(1, baseCfg.benchmarkRuns);
      const variants = [
        { label: "off", modifiedDefaultBypass: false },
        { label: "on", modifiedDefaultBypass: true },
      ];
      const variantResults: Array<{
        label: string;
        modifiedDefaultBypass: boolean;
        warmup: BenchmarkRunMetrics;
        runs: BenchmarkRunMetrics[];
        summary: Record<string, unknown>;
      }> = [];

      this.output.appendLine(
        `[renium] benchmark-ab: running ${variants.length} variants, each with 1 warm-up + ${runCount} measured runs`,
      );

      for (const variant of variants) {
        const cfg: SyncConfig = {
          ...baseCfg,
          modifiedDefaultBypass: variant.modifiedDefaultBypass,
        };
        const runs: BenchmarkRunMetrics[] = [];
        this.output.appendLine(
          `[renium] benchmark-ab: ${variant.label}: warm-up start (modifiedDefaultBypass=${variant.modifiedDefaultBypass}, not counted)`,
        );
        const warmupResult = await this.runExport({
          services: cfg.services,
          runImport: cfg.runImport,
          notifyOnSuccess: false,
          reason: "",
          quietTimings: false,
          configOverrides: {
            modifiedDefaultBypass: variant.modifiedDefaultBypass,
          },
        });
        const warmup = this.parseBenchmarkMetrics(warmupResult.output);
        this.logBenchmarkRun(`[renium] benchmark-ab: ${variant.label}: warm-up`, warmup);

        for (let index = 0; index < runCount; index += 1) {
          this.output.appendLine(`[renium] benchmark-ab: ${variant.label}: run ${index + 1}/${runCount} start`);
          const result = await this.runExport({
            services: cfg.services,
            runImport: cfg.runImport,
            notifyOnSuccess: false,
            reason: "",
            quietTimings: false,
            configOverrides: {
              modifiedDefaultBypass: variant.modifiedDefaultBypass,
            },
          });
          const metrics = this.parseBenchmarkMetrics(result.output);
          runs.push(metrics);
          this.logBenchmarkRun(`[renium] benchmark-ab: ${variant.label}: run ${index + 1}/${runCount}`, metrics);
        }

        const summary = this.buildBenchmarkSummary(runs);
        variantResults.push({
          label: variant.label,
          modifiedDefaultBypass: variant.modifiedDefaultBypass,
          warmup,
          runs,
          summary,
        });
      }

      const offSummary = variantResults.find((variant) => !variant.modifiedDefaultBypass)?.summary as
        | Record<string, unknown>
        | undefined;
      const onSummary = variantResults.find((variant) => variant.modifiedDefaultBypass)?.summary as
        | Record<string, unknown>
        | undefined;
      const offTotal = this.summaryP50(offSummary, "totalMs");
      const onTotal = this.summaryP50(onSummary, "totalMs");
      const offPlugin = this.summaryP50(offSummary, "trackedServicePluginServerMs");
      const onPlugin = this.summaryP50(onSummary, "trackedServicePluginServerMs");
      const totalDeltaMs = this.metricDelta(offTotal, onTotal);
      const pluginDeltaMs = this.metricDelta(offPlugin, onPlugin);

      this.output.appendLine(
        `[renium] benchmark-ab: total p50 off=${this.formatMetricMs(offTotal)} on=${this.formatMetricMs(onTotal)} delta=${this.formatSignedMetricMs(totalDeltaMs)}`,
      );
      this.output.appendLine(
        `[renium] benchmark-ab: tracked-service plugin server p50 off=${this.formatMetricMs(offPlugin)} on=${this.formatMetricMs(onPlugin)} delta=${this.formatSignedMetricMs(pluginDeltaMs)}`,
      );

      const benchmarkPath = path.join(baseCfg.projectRoot, ".renium", "benchmark-modified-default-bypass-ab.latest.json");
      const payload = {
        generatedAt: new Date().toISOString(),
        runCount,
        warmupRunsPerVariant: 1,
        services: baseCfg.services,
        runImport: baseCfg.runImport,
        importMode: baseCfg.importMode,
        performanceMode: baseCfg.performanceMode,
        chunkSize: baseCfg.chunkSize,
        bridgePorts: baseCfg.bridgePorts,
        comparison: {
          totalP50DeltaMs: totalDeltaMs,
          trackedServicePluginServerP50DeltaMs: pluginDeltaMs,
          totalP50OffMs: offTotal,
          totalP50OnMs: onTotal,
          trackedServicePluginServerP50OffMs: offPlugin,
          trackedServicePluginServerP50OnMs: onPlugin,
        },
        variants: variantResults.map((variant) => ({
          label: variant.label,
          modifiedDefaultBypass: variant.modifiedDefaultBypass,
          warmup: variant.warmup,
          summary: variant.summary,
          runs: variant.runs.map((metrics, index) => ({
            index: index + 1,
            ...metrics,
          })),
        })),
      };
      fs.mkdirSync(path.dirname(benchmarkPath), { recursive: true });
      fs.writeFileSync(benchmarkPath, JSON.stringify(payload, null, 2), "utf8");
      this.output.appendLine(`[renium] benchmark-ab: saved metrics JSON to ${benchmarkPath}`);
      vscode.window.showInformationMessage(`Renium: modified-default A/B benchmark saved to ${benchmarkPath}.`);
    });
  }

  public async profilePluginOperations(): Promise<void> {
    await this.enqueue("Profile plugin operations", async () => {
      const cfg = this.getConfig();
      const service = "ServerStorage";
      const sampleCount = 256;
      const iterations = 11;
      const flags = "luau,instance,serialize";
      const command = cfg.exportCliPath;
      const args = [
        "profile-plugin-ops",
        "--project-root",
        cfg.projectRoot,
        "--snapshot-dir",
        cfg.snapshotDir,
        "--service",
        service,
        "--transport",
        cfg.transport,
        "--source-workers",
        String(Math.max(0, cfg.sourceWorkers)),
        "--instance-workers",
        String(Math.max(0, cfg.instanceWorkers)),
        "--import-workers",
        String(Math.max(0, cfg.importWorkers)),
        "--performance-mode",
        cfg.performanceMode,
        ...(cfg.modifiedDefaultBypass ? ["--modified-default-bypass"] : ["--no-modified-default-bypass"]),
        "--chunk-size",
        String(Math.max(512, cfg.chunkSize)),
        "--snapshot-instance-chunk-size",
        String(Math.max(0, cfg.snapshotInstanceChunkSize)),
        "--bridge-wait-seconds",
        String(Math.max(1, cfg.bridgeWaitSeconds)),
        "--bridge-ports",
        cfg.bridgePorts,
        "--server",
        cfg.server,
        "--config",
        cfg.configTomlPath,
        "--ws-wait-seconds",
        String(Math.max(1, cfg.wsWaitSeconds)),
        cfg.adaptiveThrottle ? "--adaptive-throttle" : "--no-adaptive-throttle",
        cfg.noUpdateEditorIcons ? "--no-update-editor-icons" : "",
        "--sample-count",
        String(sampleCount),
        "--iterations",
        String(iterations),
        "--flags",
        flags,
      ].filter((value) => value.length > 0);

      this.output.show(false);
      this.logResolvedConfig(cfg);
      this.output.appendLine(`[renium] profile command: ${command} ${this.renderArgs(args)}`);
      const result = await this.runCommand(command, args, cfg.projectRoot, "profile-plugin-ops", cfg.progressHeartbeatSeconds);
      if (result.code !== 0) {
        throw new Error(`Plugin op profile exited with code ${result.code}`);
      }

      const profile = this.extractPluginProfile(result.output);
      const profilePath = path.join(cfg.projectRoot, ".renium", "profile-plugin-ops.latest.json");
      fs.mkdirSync(path.dirname(profilePath), { recursive: true });
      fs.writeFileSync(profilePath, JSON.stringify(profile, null, 2), "utf8");
      this.output.appendLine(`[renium] profile: saved raw JSON to ${profilePath}`);
      this.output.appendLine(`[renium] profile: ranked cost per 100k calls for ${service}`);
      for (const line of this.formatPluginProfileRanking(profile, 18)) {
        this.output.appendLine(line);
      }

      vscode.window.showInformationMessage(`Renium: plugin profile saved to ${profilePath}.`);
    });
  }

  public async startLiveSync(options: { silent?: boolean; bestEffort?: boolean } = {}): Promise<void> {
    if (this.liveSyncStartPromise) {
      await this.liveSyncStartPromise;
      return;
    }
    this.liveSyncStopRequested = false;
    const startPromise = this.startLiveSyncInternal(options);
    this.liveSyncStartPromise = startPromise;
    try {
      await startPromise;
    } finally {
      if (this.liveSyncStartPromise === startPromise) {
        this.liveSyncStartPromise = undefined;
      }
    }
  }

  private async startLiveSyncInternal(options: { silent?: boolean; bestEffort?: boolean } = {}): Promise<void> {
    this.liveSyncStartupInProgress = true;
    try {
      if (this.liveSyncWatcher) {
        await this.setEditorLiveSyncEnabled(true);
        const cfg = this.getConfig();
        if (this.liveSyncStopRequested) {
          this.disposeLiveSyncRuntime();
          await this.setEditorLiveSyncEnabled(false);
          return;
        }
        if (cfg.studioLiveSyncEnabled && !this.studioLiveSyncStarted) {
          if (!await this.ensureLiveSyncServeReady(cfg, options)) {
            return;
          }
          if (this.liveSyncStopRequested) {
            this.disposeLiveSyncRuntime();
            await this.setEditorLiveSyncEnabled(false);
            return;
          }
          await this.startStudioLiveSyncRuntime(cfg, options);
        }
        if (!options.silent) {
          vscode.window.showInformationMessage("Renium: live sync is already running.");
        }
        return;
      }

      const cfg = this.getConfig();
      if (cfg.transport !== "ws") {
        if (!options.silent) {
          vscode.window.showErrorMessage('Renium: live sync needs the WebSocket transport. Set "renium.transport" to "ws" in Settings.');
        }
        return;
      }
      try {
        this.ensureFileExists(cfg.exportCliPath);
      } catch (err) {
        if (!options.bestEffort) {
          throw err;
        }
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] editor live sync skipped: ${message}`);
        return;
      }

      const srcRoot = path.join(cfg.projectRoot, "src");
      if (!fs.existsSync(srcRoot)) {
        const message = `src directory not found: ${srcRoot}`;
        if (!options.bestEffort) {
          throw new Error(message);
        }
        this.output.appendLine(`[renium] editor live sync skipped: ${message}`);
        return;
      }

      if (!await this.ensureLiveSyncServeReady(cfg, options)) {
        return;
      }
      if (this.liveSyncStopRequested) {
        return;
      }

      const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(srcRoot, "**/*"));
      this.liveSyncWatcher = watcher;

      const queuePath = (uri: vscode.Uri): void => {
        if (uri.scheme === "file") {
          this.queueEditorChange(uri.fsPath);
        }
      };
      watcher.onDidCreate(queuePath);
      watcher.onDidChange(queuePath);
      watcher.onDidDelete(queuePath);

      await this.setEditorLiveSyncEnabled(true);
      if (this.liveSyncStopRequested) {
        this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        return;
      }
      let liveCfg = this.getConfig();
      this.displayedLiveSyncPrompt = false;
      let initialState: StudioChangeState | undefined;
      if (liveCfg.studioLiveSyncEnabled) {
        initialState = await this.getStudioChangeState(liveCfg, liveCfg.services, { reset: true, start: true });
        liveCfg = this.effectiveLiveSyncConfig(liveCfg);
      }
      if (liveCfg.initialSyncPriority === "editor") {
        await this.runInitialEditorLiveSyncPass(srcRoot, options);
      }
      if (this.liveSyncStopRequested) {
        this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        return;
      }
      await this.startStudioLiveSyncRuntime(liveCfg, {
        ...options,
        initialSync: liveCfg.initialSyncPriority === "studio",
        initialState,
      });
      this.updateStatusBar();
      if (!options.silent) {
        vscode.window.showInformationMessage("Renium: editor -> Studio live sync started.");
      }
    } catch (err) {
      this.disposeLiveSyncRuntime();
      await this.setEditorLiveSyncEnabled(false);
      throw err;
    } finally {
      this.liveSyncStartupInProgress = false;
    }
  }

  private async ensureLiveSyncServeReady(
    cfg: SyncConfig,
    options: { bestEffort?: boolean } = {},
  ): Promise<boolean> {
    if (cfg.transport !== "ws") {
      return true;
    }
    const startedServe = !this.bridgeServeRequested;
    this.bridgeServeRequested = true;
    if (startedServe) {
      this.liveSyncOwnsServe = true;
    }
    try {
      await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: true });
      const result = await this.runDaemonCommand(
        cfg.exportCliPath,
        [],
        cfg,
        "live-sync-wait-for-plugin",
        "wait-for-channels",
        { quietWait: true },
      );
      if (result.code !== 0) {
        const detail = result.output
          .replace(/\r\n/g, "\n")
          .split("\n")
          .map((line) => line.trim())
          .filter((line) => line.length > 0 && !line.startsWith("__ROBLOX_SYNC_DAEMON_RESULT__"))
          .slice(-4)
          .join(" ")
          .slice(-700);
        throw new Error(`Studio plugin bridge did not connect.${detail ? ` ${detail}` : " Check that the Renium Studio plugin is running, then retry."}`);
      }
      return true;
    } catch (err) {
      if (startedServe) {
        this.bridgeServeRequested = false;
        this.liveSyncOwnsServe = false;
        this.stopBridgeDaemon();
      }
      if (!options.bestEffort) {
        throw err;
      }
      this.output.appendLine(`[renium] editor live sync waiting for Studio plugin failed: ${err instanceof Error ? err.message : String(err)}`);
      return false;
    }
  }

  private async runInitialEditorLiveSyncPass(srcRoot: string, options: { bestEffort?: boolean } = {}): Promise<void> {
    const initialPaths = this.collectInitialEditorLiveSyncSettingsPaths(srcRoot);
    const initialTargets = this.collectInitialEditorLiveSyncTargetIds(srcRoot, initialPaths);
    if (initialTargets.paths.length === 0 || initialTargets.targetSettingsIds.length === 0) {
      this.primeEditorLiveSyncCache([], this.getConfig());
      return;
    }
    try {
      await this.pushEditorPathsNow(initialTargets.paths, {
        force: true,
        skipChangeFilter: true,
        targetSettingsIds: initialTargets.targetSettingsIds,
        taskName: "Editor -> Studio initial sync",
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] editor live sync initial pass failed: ${message}`);
      if (!options.bestEffort) {
        throw err;
      }
    }
  }

  public async retryEditorInitialSync(): Promise<void> {
    const cfg = this.getConfig();
    const srcRoot = path.join(cfg.projectRoot, "src");
    if (!fs.existsSync(srcRoot)) {
      throw new Error(`src directory not found: ${srcRoot}`);
    }
    await this.runInitialEditorLiveSyncPass(srcRoot);
    vscode.window.showInformationMessage("Renium: editor -> Studio initial sync finished.");
  }

  private async startStudioLiveSyncRuntime(
    cfg: SyncConfig,
    options: { bestEffort?: boolean; initialSync?: boolean; initialState?: StudioChangeState } = {},
  ): Promise<void> {
    if (!cfg.studioLiveSyncEnabled) {
      this.stopStudioLiveSyncRuntime();
      return;
    }
    try {
      if (this.studioLiveSyncStarted) {
        this.scheduleStudioLiveSyncPoll(cfg, this.resetStudioLiveSyncPollDelay(cfg));
        return;
      }
      const initialState = options.initialState ?? await this.getStudioChangeState(cfg, cfg.services, { reset: true, start: true });
      const runtimeCfg = this.effectiveLiveSyncConfig(cfg);
      if (initialState.twoWaySyncEnabled === false) {
        this.output.appendLine("[renium] Studio -> editor live sync is disabled in the Renium Studio plugin settings.");
        this.stopStudioLiveSyncRuntime();
        return;
      }
      const shouldRunStudioInitialSync = options.initialSync ?? (runtimeCfg.initialSyncPriority === "studio");
      if (shouldRunStudioInitialSync) {
        await this.enqueue("Studio -> Editor initial sync", async () => {
          await this.runStudioToEditorSync(runtimeCfg.services, runtimeCfg);
        });
      }
      await this.getStudioChangeState(runtimeCfg, runtimeCfg.services, { reset: true, start: true });
      this.studioLiveSyncStarted = true;
      this.scheduleStudioLiveSyncPoll(runtimeCfg, this.resetStudioLiveSyncPollDelay(runtimeCfg));
    } catch (err) {
      this.stopStudioLiveSyncRuntime();
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] Studio -> editor live sync start failed: ${message}`);
      if (!options.bestEffort) {
        throw err;
      }
    }
  }

  private stopStudioLiveSyncRuntime(): void {
    if (this.studioLiveSyncTimer) {
      clearTimeout(this.studioLiveSyncTimer);
      this.studioLiveSyncTimer = undefined;
    }
    this.studioLiveSyncInFlight = false;
    this.studioLiveSyncStarted = false;
    this.studioLiveSyncNextPollMs = DEFAULT_STUDIO_LIVE_SYNC_POLL_MS;
    this.studioToEditorImportInProgress = false;
  }

  private studioLiveSyncBasePollDelayMs(cfg: SyncConfig): number {
    return Math.max(MIN_STUDIO_LIVE_SYNC_POLL_MS, cfg.studioLiveSyncPollMs);
  }

  private resetStudioLiveSyncPollDelay(cfg: SyncConfig): number {
    const delayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    this.studioLiveSyncNextPollMs = delayMs;
    return delayMs;
  }

  private nextIdleStudioLiveSyncPollDelay(cfg: SyncConfig): number {
    const baseDelayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    const currentDelayMs = Math.max(baseDelayMs, this.studioLiveSyncNextPollMs || baseDelayMs);
    this.studioLiveSyncNextPollMs = Math.min(
      MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS,
      Math.max(baseDelayMs, Math.ceil(currentDelayMs * STUDIO_LIVE_SYNC_POLL_BACKOFF_MULTIPLIER)),
    );
    return currentDelayMs;
  }

  private nextErrorStudioLiveSyncPollDelay(cfg: SyncConfig): number {
    const baseDelayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    const currentDelayMs = Math.max(baseDelayMs, this.studioLiveSyncNextPollMs || baseDelayMs);
    this.studioLiveSyncNextPollMs = Math.min(
      MAX_STUDIO_LIVE_SYNC_ERROR_POLL_MS,
      Math.max(baseDelayMs, Math.ceil(currentDelayMs * STUDIO_LIVE_SYNC_POLL_BACKOFF_MULTIPLIER)),
    );
    return currentDelayMs;
  }

  private studioLiveSyncWaitSeconds(delayMs: number): number {
    return Math.max(0.05, Math.min(MAX_STUDIO_LIVE_SYNC_EVENT_WAIT_MS / 1000, delayMs / 1000));
  }

  private scheduleStudioLiveSyncPoll(cfg: SyncConfig, delayMs: number): void {
    if (this.studioLiveSyncTimer) {
      clearTimeout(this.studioLiveSyncTimer);
      this.studioLiveSyncTimer = undefined;
    }
    if (!cfg.editorLiveSyncEnabled || !this.liveSyncWatcher || !cfg.studioLiveSyncEnabled) {
      return;
    }
    this.studioLiveSyncTimer = setTimeout(() => {
      this.studioLiveSyncTimer = undefined;
      void this.pollStudioLiveSync().catch((err) => {
        const latestCfg = this.getConfig();
        const nextDelayMs = this.nextErrorStudioLiveSyncPollDelay(latestCfg);
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] Studio -> editor live sync failed: ${message}`);
        this.scheduleStudioLiveSyncPoll(latestCfg, nextDelayMs);
      });
    }, Math.max(MIN_STUDIO_LIVE_SYNC_POLL_MS, delayMs));
  }

  private async pollStudioLiveSync(): Promise<void> {
    const cfg = this.getConfig();
    if (!cfg.editorLiveSyncEnabled || !this.liveSyncWatcher || !cfg.studioLiveSyncEnabled) {
      this.stopStudioLiveSyncRuntime();
      return;
    }
    if (this.studioLiveSyncInFlight) {
      this.scheduleStudioLiveSyncPoll(cfg, this.nextIdleStudioLiveSyncPollDelay(cfg));
      return;
    }
    this.studioLiveSyncInFlight = true;
    let nextDelayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    try {
      const idleWaitMs = this.nextIdleStudioLiveSyncPollDelay(cfg);
      const state = await this.getStudioChangeState(cfg, cfg.services, {
        start: true,
        waitSeconds: this.studioLiveSyncWaitSeconds(idleWaitMs),
      });
      const runtimeCfg = this.effectiveLiveSyncConfig(cfg);
      if (state.twoWaySyncEnabled === false) {
        this.output.appendLine("[renium] Studio -> editor live sync was disabled in the Renium Studio plugin settings.");
        this.stopStudioLiveSyncRuntime();
        return;
      }
      this.studioConflictPolicyOverride = typeof state.conflictResolution === "string" && state.conflictResolution.trim().length > 0
        ? this.normalizeConflictPolicy(state.conflictResolution)
        : undefined;
      const dirtyServices = Array.isArray(state.dirtyServices)
        ? this.normalizeReportedServices(state.dirtyServices, cfg.services)
        : [];
      const observedSeq = this.studioChangeSeq(state);
      if (dirtyServices.length > 0) {
        nextDelayMs = this.resetStudioLiveSyncPollDelay(runtimeCfg);
        const ackObservedDirty = this.studioChangeAckOptions(observedSeq);
        if (this.shouldDropLikelySelfDirtyStudioState(dirtyServices, runtimeCfg)) {
          ackObservedDirty.suppressSeconds = Math.max(1, Math.min(4, runtimeCfg.studioLiveSyncPollMs / 1000 + 1.5));
          await this.getStudioChangeState(runtimeCfg, dirtyServices, ackObservedDirty);
          return;
        }
        let appliedPropertyChanges = false;
        try {
          appliedPropertyChanges = await this.tryApplyStudioPropertyChangesToEditor(state, dirtyServices, runtimeCfg);
        } catch (err) {
          const message = err instanceof Error ? err.message : String(err);
          this.output.appendLine(`[renium] Studio -> editor property fast path failed: ${message}`);
        }
        if (!appliedPropertyChanges) {
          await this.enqueueStudioToEditorSyncIfChanged(dirtyServices, runtimeCfg, state);
        }
        await this.getStudioChangeState(runtimeCfg, dirtyServices, ackObservedDirty);
      } else {
        nextDelayMs = state.eventDriven === true ? MIN_STUDIO_LIVE_SYNC_POLL_MS : this.nextIdleStudioLiveSyncPollDelay(runtimeCfg);
      }
    } catch (err) {
      const latestCfg = this.getConfig();
      nextDelayMs = this.nextErrorStudioLiveSyncPollDelay(latestCfg);
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] Studio -> editor live sync failed: ${message}`);
    } finally {
      this.studioLiveSyncInFlight = false;
      if (this.studioLiveSyncStarted) {
        this.scheduleStudioLiveSyncPoll(this.effectiveLiveSyncConfig(this.getConfig()), nextDelayMs);
      }
    }
  }

  private async getStudioChangeState(
    cfg: SyncConfig,
    services: string[],
    options: { reset?: boolean; ackSeq?: number; start?: boolean; suppressSeconds?: number; waitSeconds?: number } = {},
  ): Promise<StudioChangeState> {
    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const args = [
      "-w",
      String(this.editorBridgeWaitSeconds(cfg)),
      "-P",
      cfg.bridgePorts,
      "-s",
      this.normalizeServices(services, cfg.services).join(","),
    ];
    if (options.reset === true) {
      args.push("--reset");
    }
    if (options.start === false) {
      args.push("--no-start");
    }
    if (typeof options.ackSeq === "number" && Number.isFinite(options.ackSeq)) {
      args.push("--ack-seq", String(Math.max(0, Math.floor(options.ackSeq))));
    }
    if (typeof options.suppressSeconds === "number" && Number.isFinite(options.suppressSeconds) && options.suppressSeconds > 0) {
      args.push("--suppress-seconds", String(Math.max(0.05, options.suppressSeconds)));
    }

    const useEventWait = typeof options.waitSeconds === "number"
      && Number.isFinite(options.waitSeconds)
      && options.waitSeconds > 0;
    if (useEventWait) {
      args.push("--wait-seconds", String(Math.max(0.05, Math.min(25, options.waitSeconds ?? 0))));
    }

    const result = useEventWait
      ? await this.runCommand(
        command,
        ["st", ...args],
        cfg.projectRoot,
        "studio-change-state",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      )
      : await this.runDaemonCommand(
        command,
        args,
        cfg,
        "studio-change-state",
        "st",
        { quietWait: true },
      );
    if (result.code !== 0) {
      throw new Error(`Studio change state exited with code ${result.code}`);
    }
    const state = this.parseStudioChangeState(result.output);
    if (!state) {
      throw new Error("Studio change state did not return a plugin result.");
    }
    if (state.runtimeSettings) {
      this.studioRuntimeSettings = state.runtimeSettings;
    }
    return state;
  }

  private effectiveLiveSyncConfig(cfg: SyncConfig): SyncConfig {
    const settings = this.studioRuntimeSettings;
    if (!settings) {
      return cfg;
    }
    const initialSyncPriority = settings.initialSyncPriority === "editor"
      ? "editor"
      : settings.initialSyncPriority === "none"
        ? "none"
        : settings.initialSyncPriority === "studio"
          ? "studio"
          : cfg.initialSyncPriority;
    const displayPrompts = settings.displayPrompts === "initial"
      ? "initial"
      : settings.displayPrompts === "never"
        ? "never"
        : settings.displayPrompts === "always"
          ? "always"
          : cfg.displayPrompts;
    const boundedInteger = (value: unknown, fallback: number, minimum: number, maximum: number): number => {
      const numeric = typeof value === "number" ? value : Number.NaN;
      return Number.isFinite(numeric)
        ? Math.max(minimum, Math.min(maximum, Math.floor(numeric)))
        : fallback;
    };
    return {
      ...cfg,
      initialSyncPriority,
      changesThreshold: boundedInteger(settings.changesThreshold, cfg.changesThreshold, 0, 100000),
      diffLinesLimit: boundedInteger(settings.diffLinesLimit, cfg.diffLinesLimit, 100, 1_000_000),
      displayPrompts,
      overridePackages: typeof settings.overridePackages === "boolean" ? settings.overridePackages : cfg.overridePackages,
    };
  }

  private studioChangeSeq(state: StudioChangeState): number | undefined {
    if (typeof state.seq !== "number" || !Number.isFinite(state.seq)) {
      return undefined;
    }
    return Math.max(0, Math.floor(state.seq));
  }

  private studioChangeAckOptions(observedSeq: number | undefined): { reset?: boolean; ackSeq?: number; start?: boolean; suppressSeconds?: number } {
    const options: { reset?: boolean; ackSeq?: number; start?: boolean; suppressSeconds?: number } = { start: true };
    if (observedSeq !== undefined) {
      options.ackSeq = observedSeq;
    } else {
      options.reset = true;
    }
    return options;
  }

  private studioChangeLogEntries(state: StudioChangeState | undefined, services?: string[]): StudioChangeLog[] {
    if (!state || !Array.isArray(state.changes)) {
      return [];
    }
    const serviceSet = services ? new Set(services.map((service) => service.trim()).filter(Boolean)) : undefined;
    return state.changes
      .filter((change) => {
        const service = String(change.service ?? "").trim();
        return service.length > 0 && (!serviceSet || serviceSet.has(service));
      })
      .sort((a, b) => (Number(a.seq ?? 0) || 0) - (Number(b.seq ?? 0) || 0));
  }

  private studioChangePath(change: StudioChangeLog): string {
    if (typeof change.path === "string" && change.path.length > 0) {
      return change.path;
    }
    if (Array.isArray(change.pathSegments) && change.pathSegments.length > 0) {
      return change.pathSegments.map((segment) => String(segment)).join(".");
    }
    return String(change.service ?? "").trim() || "<unknown>";
  }

  private formatStudioChange(change: StudioChangeLog, mode: "property" | "full"): string {
    const pathLabel = this.studioChangePath(change);
    const action = String(change.action ?? (mode === "property" ? "property" : "fullSync"));
    const className = String(change.className ?? "").trim();
    const property = String(change.property ?? "").trim();
    const attribute = String(change.attribute ?? "").trim();
    const suffix = property
      ? `.${property}`
      : attribute
        ? `.@${attribute}`
        : "";
    const detailParts = [
      action,
      className.length > 0 ? className : undefined,
      mode === "full" || change.fullSync === true ? "full export" : "property write",
    ].filter((value): value is string => typeof value === "string" && value.length > 0);
    return `${pathLabel}${suffix} (${detailParts.join(", ")})`;
  }

  private logStudioChanges(state: StudioChangeState | undefined, mode: "property" | "full", services: string[]): void {
    void state;
    void mode;
    void services;
  }

  private logEditorChangedPaths(label: string, filePaths: string[], cfg: SyncConfig): void {
    const maxEntries = 25;
    for (const filePath of filePaths.slice(0, maxEntries)) {
      this.output.appendLine(`[renium] ${label}: ${this.normalizePathForCompare(path.relative(cfg.projectRoot, filePath))}`);
    }
    if (filePaths.length > maxEntries) {
      this.output.appendLine(`[renium] ${label}: ${filePaths.length - maxEntries} more file(s)`);
    }
  }

  private async runStudioToEditorSync(services: string[], cfg: SyncConfig): Promise<void> {
    const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg, { quietProbe: true });
    if (diff.changedServices.length === 0) {
      return;
    }
    await this.importStudioLiveSyncSnapshot(diff.changedServices, cfg, diff.fingerprintsByService, { quietLog: true });
  }

  private async enqueueStudioToEditorSyncIfChanged(services: string[], cfg: SyncConfig, state?: StudioChangeState): Promise<void> {
    const run = async (): Promise<void> => {
      let taskStarted = false;
      const taskName = "Studio -> Editor sync";
      try {
        const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg, { quietProbe: true });
        if (diff.changedServices.length === 0) {
          return;
        }
        taskStarted = true;
        this.setActiveTask(taskName);
        this.logStudioChanges(state, "full", diff.changedServices);
        await this.importStudioLiveSyncSnapshot(diff.changedServices, cfg, diff.fingerprintsByService, { quietLog: true });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        if (taskStarted) {
          this.output.appendLine(`[renium] task failed: ${taskName}: ${message}`);
          this.output.show(true);
          vscode.window.showErrorMessage(`Renium: ${taskName} failed. ${message}`);
        } else {
          this.output.appendLine(`[renium] Studio -> editor dirty check failed: ${message}`);
        }
        throw err;
      } finally {
        if (taskStarted) {
          this.setActiveTask(undefined);
        }
      }
    };

    const queued = this.queue.catch(() => undefined).then(run);
    this.queue = queued.catch(() => undefined);
    await queued;
  }

  private async exportStudioLiveSyncSnapshotAndDiff(
    services: string[],
    cfg: SyncConfig,
    options: { quietProbe?: boolean } = {},
  ): Promise<StudioSnapshotDiff> {
    const selectedServices = this.normalizeServices(services, cfg.services);
    await this.getStudioChangeState(cfg, selectedServices, { start: true });
    await this.runExport({
      services: selectedServices,
      runImport: false,
      notifyOnSuccess: false,
      reason: "",
      quietLog: options.quietProbe === true,
    });
    return this.diffServicesBySnapshotFingerprint(selectedServices, cfg);
  }

  private async importStudioLiveSyncSnapshot(
    services: string[],
    cfg: SyncConfig,
    fingerprintsByService?: Map<string, string>,
    options: { quietLog?: boolean } = {},
  ): Promise<void> {
    const selectedServices = this.normalizeServices(services, cfg.services);
    this.studioToEditorImportInProgress = true;
    const capturedLocalEdits = this.captureLocalScriptEditsForServices(selectedServices, cfg);
    try {
      await this.runRustImport(cfg, this.resolveSnapshotPath(cfg), selectedServices, { quietLog: options.quietLog === true });
      const survivingLocalEdits = this.reconcileLocalEditsAfterFullImport(selectedServices, cfg, capturedLocalEdits);
      this.commitStudioSnapshotFingerprints(selectedServices, fingerprintsByService);
      this.replaceEditorLiveSyncCacheForServices(selectedServices, cfg);
      if (survivingLocalEdits.length > 0) {
        this.invalidateEditorLiveSyncCacheEntries(survivingLocalEdits, cfg);
        for (const filePath of survivingLocalEdits) {
          this.pendingEditorPaths.add(filePath);
        }
        this.scheduleEditorLiveSyncFlush(0);
      }
      try {
        await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", selectedServices);
      } catch {
      }
    } finally {
      this.studioToEditorImportSuppressUntilMs = Date.now() + Math.max(1000, Math.min(3000, cfg.studioLiveSyncPollMs * 2));
      this.studioToEditorImportInProgress = false;
      this.studioToEditorLastSyncEndedAt = Date.now();
    }
  }

  private async showStudioChangePreview(
    propertyChanges: StudioPropertyChange[],
    trackedChanges: StudioChangeLog[],
    changeCount: number,
    cfg: SyncConfig,
    structural: boolean,
  ): Promise<"apply" | "full" | "discard"> {
    let oldValues: unknown[] = [];
    try {
      const requests = propertyChanges.map((change) => ({
        service: String(change.service ?? ""),
        settingsId: typeof change.settingsId === "string" ? change.settingsId : undefined,
        scope: change.scope ?? "property",
        property: String(change.property ?? ""),
      }));
      const looked = await vscode.commands.executeCommand<unknown[]>(
        "renium.fileExplorer.lookupPropertyValues",
        requests,
      );
      if (Array.isArray(looked)) {
        oldValues = looked;
      }
    } catch {
      oldValues = [];
    }

    if (!this.changePreviewIconNames) {
      this.changePreviewIconNames = new Set(loadAssetIconNames(this.context.extensionUri));
    }
    const iconNames = this.changePreviewIconNames;
    const rows: Array<{
      service: string;
      path: string;
      leaf: string;
      className: string;
      icon: string;
      scope: string;
      property: string;
      status?: string;
      oldValue?: unknown;
      newValue?: unknown;
    }> = propertyChanges.map((change, index) => {
      const segments = Array.isArray(change.pathSegments)
        ? change.pathSegments.map((segment) => String(segment))
        : [];
      const className = String(change.className ?? "");
      return {
        service: String(change.service ?? ""),
        path: segments.join("."),
        leaf: segments.length > 0 ? segments[segments.length - 1] : String(change.settingsId ?? "instance"),
        className,
        icon: iconAssetNameForClass(className || "Folder", iconNames),
        scope: change.scope ?? "property",
        property: String(change.property ?? ""),
        oldValue: oldValues[index],
        newValue: change.value,
      };
    });
    const seenStatus = new Set<string>();
    for (const change of trackedChanges) {
      const action = String(change.action ?? "");
      if (action !== "added" && action !== "removed") {
        continue;
      }
      const segments = Array.isArray(change.pathSegments)
        ? change.pathSegments.map((segment) => String(segment))
        : [];
      if (segments.length === 0) {
        continue;
      }
      const path = segments.join(".");
      const statusKey = `${action} ${path}`;
      if (seenStatus.has(statusKey)) {
        continue;
      }
      seenStatus.add(statusKey);
      const className = String(change.className ?? "");
      rows.push({
        service: String(change.service ?? segments[0] ?? ""),
        path,
        leaf: segments[segments.length - 1],
        className,
        icon: iconAssetNameForClass(className || "Folder", iconNames),
        scope: "__status",
        property: "",
        status: action,
      });
    }

    if (this.changePreviewResolve) {
      this.changePreviewResolve("full");
      this.changePreviewResolve = undefined;
    }
    this.changePreviewPanel?.dispose();

    const assetsUri = vscode.Uri.joinPath(this.context.extensionUri, "assets");
    const panel = vscode.window.createWebviewPanel(
      "reniumChangePreview",
      `Renium: review ${changeCount} Studio changes`,
      vscode.ViewColumn.Active,
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [assetsUri] },
    );
    this.changePreviewPanel = panel;
    const assetBase = panel.webview.asWebviewUri(assetsUri).toString();
    panel.webview.html = this.buildChangePreviewHtml(rows, changeCount, cfg.changesThreshold, assetBase, structural);

    return await new Promise<"apply" | "full" | "discard">((resolve) => {
      let settled = false;
      const finish = (decision: "apply" | "full" | "discard"): void => {
        if (settled) {
          return;
        }
        settled = true;
        this.changePreviewResolve = undefined;
        this.changePreviewPanel = undefined;
        resolve(decision);
        panel.dispose();
      };
      this.changePreviewResolve = finish;
      panel.webview.onDidReceiveMessage((message: { action?: string }) => {
        const action = message?.action;
        if (action === "apply" || action === "full" || action === "discard") {
          finish(action);
        }
      });
      panel.onDidDispose(() => finish("full"));
    });
  }

  private buildChangePreviewHtml(
    rows: Array<{
      service: string;
      path: string;
      leaf: string;
      className: string;
      icon: string;
      scope: string;
      property: string;
      status?: string;
      oldValue?: unknown;
      newValue?: unknown;
    }>,
    changeCount: number,
    threshold: number,
    assetBase: string,
    structural: boolean,
  ): string {
    const payload = JSON.stringify(rows).replace(/</g, "\\u003c");
    const instanceCount = new Set(rows.map((row) => `${row.service}.${row.path}`)).size;
    const services = [...new Set(rows.map((row) => row.service).filter((service) => service.length > 0))];
    const iconNames = this.changePreviewIconNames ?? new Set<string>();
    const folderIcon = iconAssetNameForClass("Folder", iconNames);
    const serviceIcons = Object.fromEntries(
      services.map((service) => [service, iconAssetNameForClass(service, iconNames)]),
    );
    return `<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<style>
  :root {
    color-scheme: light dark;
    --ink: rgba(255,255,255,0.92);
    --ink-mid: rgba(255,255,255,0.60);
    --ink-dim: rgba(255,255,255,0.38);
    --surface: rgba(255,255,255,0.032);
    --surface-hover: rgba(255,255,255,0.055);
    --edge: rgba(255,255,255,0.085);
    --edge-soft: rgba(255,255,255,0.05);
    --accent: #8b7cf8;
    --accent-2: #5e9bfa;
    --amber: #e8b53f;
    --red: #f47f76;
    --green: #66c88e;
  }
  body.vscode-light, body.vscode-high-contrast-light {
    --ink: rgba(20,22,28,0.92);
    --ink-mid: rgba(20,22,28,0.62);
    --ink-dim: rgba(20,22,28,0.40);
    --surface: rgba(18,20,26,0.035);
    --surface-hover: rgba(18,20,26,0.06);
    --edge: rgba(18,20,26,0.12);
    --edge-soft: rgba(18,20,26,0.07);
    --accent: #6a58e8;
    --accent-2: #3d7ce0;
    --amber: #b8860b;
    --red: #d0453a;
    --green: #1f8a4c;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  ::-webkit-scrollbar { width: 9px; }
  ::-webkit-scrollbar-thumb { background: var(--edge); border-radius: 5px; border: 2px solid transparent; background-clip: padding-box; }
  ::-webkit-scrollbar-thumb:hover { background: var(--ink-dim); border: 2px solid transparent; background-clip: padding-box; }
  body {
    font-family: "Segoe UI Variable Text", "Inter", var(--vscode-font-family, "Segoe UI"), sans-serif;
    -webkit-font-smoothing: antialiased;
    font-size: 13px; line-height: 1.5;
    color: var(--ink);
    background: var(--vscode-editor-background, #17171a);
    display: flex; flex-direction: column; height: 100vh; overflow: hidden;
  }
  .header { padding: 26px 30px 20px; flex: none; }
  .kicker {
    display: flex; align-items: center; gap: 8px;
    font-size: 10px; font-weight: 700; letter-spacing: 0.16em; text-transform: uppercase;
    color: var(--ink-dim);
  }
  .kicker b { color: color-mix(in srgb, var(--accent) 75%, var(--ink)); font-weight: 700; }
  .pulse {
    width: 7px; height: 7px; border-radius: 50%;
    background: var(--amber);
    box-shadow: 0 0 0 0 color-mix(in srgb, var(--amber) 45%, transparent);
    animation: pulse 2.2s cubic-bezier(0.4, 0, 0.6, 1) infinite; flex: none;
  }
  @keyframes pulse {
    0% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--amber) 45%, transparent); }
    70% { box-shadow: 0 0 0 7px transparent; }
    100% { box-shadow: 0 0 0 0 transparent; }
  }
  h1 { font-size: 19px; font-weight: 640; letter-spacing: -0.018em; margin-top: 10px; }
  .subtitle { margin-top: 4px; font-size: 12.5px; color: var(--ink-mid); max-width: 60ch; }
  .subtitle b { color: var(--ink); font-weight: 620; font-variant-numeric: tabular-nums; }
  .toolbar { display: flex; align-items: center; gap: 10px; margin-top: 14px; }
  .filter {
    flex: none; width: 240px; font-family: inherit; font-size: 12px;
    color: var(--ink); background: var(--surface); border: 1px solid var(--edge);
    border-radius: 7px; padding: 5px 11px; outline: none;
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .filter:focus { border-color: color-mix(in srgb, var(--accent) 55%, transparent); background: var(--surface-hover); }
  .filter::placeholder { color: var(--ink-dim); }
  .toolbar-hint { font-size: 11px; color: var(--ink-dim); }
  .list { flex: 1; overflow-y: auto; padding: 6px 22px 26px; position: relative; animation: rise 0.3s cubic-bezier(0.16, 1, 0.3, 1) both; }
  #sizer { position: relative; width: 100%; }
  #viewport { position: absolute; left: 0; right: 0; top: 0; }
  .row {
    display: flex; align-items: center; height: 26px; border-radius: 6px;
    padding-right: 10px; cursor: pointer; user-select: none; min-width: 0;
  }
  .row:hover { background: var(--surface-hover); }
  .twisty {
    width: 17px; flex: none; text-align: center; color: var(--ink-dim);
    font-size: 10px; line-height: 1; transition: transform 0.12s ease;
  }
  .twisty.open { transform: rotate(90deg); }
  .twisty.blank { visibility: hidden; }
  .icon {
    width: 16px; height: 16px; flex: none; margin-right: 6px;
    display: block; object-fit: contain; object-position: center center; image-rendering: pixelated;
  }
  .rname { font-size: 12.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .row.folder .rname { color: var(--ink-mid); }
  .rsep { color: var(--ink-dim); margin: 0 4px; font-size: 11px; }
  .count {
    margin-left: auto; flex: none; font-size: 10px; font-weight: 650;
    padding: 1px 8px; border-radius: 999px;
    background: var(--surface-hover); color: var(--ink-mid);
    font-variant-numeric: tabular-nums;
  }
  .prop-row {
    display: grid; grid-template-columns: minmax(120px, 190px) 1fr;
    gap: 16px; align-items: center; height: 26px; padding: 0 10px; border-radius: 6px;
  }
  .prop-row:hover { background: var(--surface-hover); }
  .prop-name-cell { display: flex; align-items: center; gap: 8px; min-width: 0; }
  .prop-name {
    font-family: "Cascadia Code", "JetBrains Mono", var(--vscode-editor-font-family, Consolas), monospace;
    font-size: 11.5px; font-weight: 450; color: var(--ink-mid);
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .scope-badge {
    font-size: 8px; font-weight: 750; text-transform: uppercase; letter-spacing: 0.1em;
    padding: 1px 6px; border-radius: 4px; flex: none;
    color: var(--ink-dim);
    background: var(--surface-hover);
  }
  .values { display: flex; align-items: center; gap: 8px; min-width: 0; font-variant-numeric: tabular-nums; }
  .val {
    font-family: "Cascadia Code", "JetBrains Mono", var(--vscode-editor-font-family, Consolas), monospace;
    font-size: 11.5px; padding: 2px 9px; border-radius: 6px;
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  }
  .val.old {
    color: color-mix(in srgb, var(--red) 82%, var(--ink));
    background: color-mix(in srgb, var(--red) 9%, transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--red) 22%, transparent);
    text-decoration: line-through; text-decoration-thickness: 1px;
    text-decoration-color: color-mix(in srgb, var(--red) 55%, transparent);
    max-width: 42%; flex: none;
  }
  .val.new {
    color: color-mix(in srgb, var(--green) 85%, var(--ink));
    background: color-mix(in srgb, var(--green) 10%, transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--green) 24%, transparent);
  }
  .val.neutral {
    color: var(--ink-mid);
    background: var(--surface-hover);
    box-shadow: inset 0 0 0 1px var(--edge-soft);
  }
  .row.added .rname { color: color-mix(in srgb, var(--green) 70%, var(--ink)); }
  .row.removed .rname {
    color: color-mix(in srgb, var(--red) 70%, var(--ink));
    text-decoration: line-through;
    text-decoration-color: color-mix(in srgb, var(--red) 50%, transparent);
  }
  .row.removed .icon { opacity: 0.55; }
  .arrow { color: var(--ink-dim); flex: none; font-size: 11px; }
  .swatch { display: inline-block; width: 11px; height: 11px; border-radius: 3.5px; margin-right: 6px; vertical-align: -1px; box-shadow: inset 0 0 0 1px rgba(128,128,128,0.4); }
  .footer {
    flex: none; display: flex; align-items: center; gap: 18px;
    padding: 15px 30px; border-top: 1px solid var(--edge-soft);
    background: color-mix(in srgb, var(--vscode-editor-background, #17171a) 72%, transparent);
    backdrop-filter: blur(14px);
  }
  .countdown { font-size: 11.5px; color: var(--ink-dim); flex: 1; min-width: 0; }
  .countdown b { color: var(--ink-mid); font-weight: 620; font-variant-numeric: tabular-nums; }
  .countdown-bar { height: 2px; border-radius: 2px; background: var(--edge); margin-top: 8px; overflow: hidden; }
  .countdown-fill { height: 100%; width: 100%; background: linear-gradient(90deg, var(--accent), var(--accent-2)); transition: width 1s linear; border-radius: 2px; }
  button {
    font-family: inherit; font-size: 12.5px; font-weight: 590; letter-spacing: 0.005em;
    padding: 8px 18px; border-radius: 8px;
    border: 1px solid transparent; cursor: pointer; flex: none;
    transition: transform 0.1s ease, box-shadow 0.15s ease, background 0.15s ease, color 0.15s ease;
  }
  button:active { transform: translateY(1px) scale(0.98); }
  .apply {
    background: linear-gradient(135deg, var(--accent), var(--accent-2));
    color: #fff;
    box-shadow: 0 2px 8px color-mix(in srgb, var(--accent) 35%, transparent), inset 0 1px 0 rgba(255,255,255,0.18);
  }
  .apply:hover { box-shadow: 0 3px 14px color-mix(in srgb, var(--accent) 50%, transparent), inset 0 1px 0 rgba(255,255,255,0.18); transform: translateY(-1px); }
  .full { background: var(--surface-hover); color: var(--ink); border-color: var(--edge); }
  .full:hover { background: var(--edge); }
  .skip { background: transparent; font-weight: 480; color: var(--ink-dim); }
  .skip:hover { color: var(--red); }
  @keyframes rise { from { opacity: 0; transform: translateY(10px); } to { opacity: 1; transform: none; } }
</style>
</head>
<body>
  <div class="header">
    <div class="kicker"><div class="pulse"></div><span><b>Renium</b>&ensp;&middot;&ensp;Live sync paused</span></div>
    <h1>Studio changes awaiting review</h1>
    <div class="subtitle"><b>${changeCount}</b> change${changeCount === 1 ? "" : "s"} across <b>${instanceCount}</b> instance${instanceCount === 1 ? "" : "s"} in ${services.join(", ") || "your project"} &mdash; this batch is over your review threshold of ${threshold}.${structural ? " It includes added or removed instances, so it can only be applied as a full import." : ""}</div>
    <div class="toolbar">
      <input class="filter" id="filter" type="text" placeholder="Filter by name, class, or property" spellcheck="false">
      <span class="toolbar-hint" id="toolbar-hint"></span>
    </div>
  </div>
  <div class="list" id="list"><div id="sizer"><div id="viewport"></div></div></div>
  <div class="footer">
    <div class="countdown">
      <span id="count-label">Protected full import in <b id="secs">90</b>s &mdash; hover the list to pause</span>
      <div class="countdown-bar"><div class="countdown-fill" id="fill"></div></div>
    </div>
    <button class="skip" id="skip" title="Acknowledge without touching editor files">Skip batch</button>
    ${structural
      ? '<button class="apply" id="full" title="Re-export and import everything that differs">Import</button>'
      : '<button class="full" id="full" title="Safest: re-export and import everything that differs">Full import</button>\n    <button class="apply" id="apply" title="Write exactly these changes to the editor files">Apply changes</button>'}
  </div>
<script>
  const vscode = acquireVsCodeApi();
  const DATA = ${payload};
  const ASSET = ${JSON.stringify(assetBase)};
  const SERVICE_ICONS = ${JSON.stringify(serviceIcons)};
  const FOLDER_ICON = ${JSON.stringify(folderIcon)};

  function esc(text) {
    return String(text).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  }
  function fmtNum(n) {
    if (!isFinite(n)) return String(n);
    return Math.abs(n) >= 1e6 || (Math.abs(n) < 1e-4 && n !== 0) ? n.toExponential(3) : String(Math.round(n * 1000) / 1000);
  }
  function fmt(value) {
    if (value === undefined) return null;
    if (value === null) return '<i>nil</i>';
    const t = typeof value;
    if (t === "boolean") return String(value);
    if (t === "number") return esc(fmtNum(value));
    if (t === "string") return '"' + esc(value.length > 90 ? value.slice(0, 90) + "\\u2026" : value) + '"';
    if (t === "object") {
      const k = value._type;
      if (k === "Color3") {
        const r = Math.round((value.r ?? 0) * 255), g = Math.round((value.g ?? 0) * 255), b = Math.round((value.b ?? 0) * 255);
        return '<span class="swatch" style="background:rgb(' + r + "," + g + "," + b + ')"></span>' + r + ", " + g + ", " + b;
      }
      if (k === "Vector3") return esc(fmtNum(value.x ?? 0) + ", " + fmtNum(value.y ?? 0) + ", " + fmtNum(value.z ?? 0));
      if (k === "Vector2") return esc(fmtNum(value.x ?? 0) + ", " + fmtNum(value.y ?? 0));
      if (k === "EnumItem") return esc(String(value.value ?? value.name ?? "Enum"));
      if (k === "Float") return esc(String(value.value));
      if (k === "CFrame") return "CFrame (" + ((value.components || []).slice(0, 3).map(fmtNum).join(", ") || "\\u2026") + ", \\u2026)";
      if (k === "Ref" || value.Ref) return "\\u2192 " + esc(String((value.Ref || value).settingsId ?? (value.Ref || value).instanceId ?? "instance"));
      const json = JSON.stringify(value);
      return esc(json.length > 90 ? json.slice(0, 90) + "\\u2026" : json);
    }
    return esc(String(value));
  }

  const root = { children: new Map(), changes: null, icon: null, className: "", status: null };
  for (const row of DATA) {
    const segments = row.path.length > 0 ? row.path.split(".") : [row.leaf];
    let node = root;
    for (const segment of segments) {
      if (!node.children.has(segment)) {
        node.children.set(segment, { name: segment, children: new Map(), changes: null, icon: null, className: "", status: null });
      }
      node = node.children.get(segment);
    }
    if (row.scope === "__status") {
      node.status = row.status;
      node.icon = node.icon || row.icon;
      node.className = node.className || row.className;
      if (!node.changes) node.changes = [];
      continue;
    }
    if (!node.changes) {
      node.changes = [];
      node.icon = row.icon;
      node.className = row.className;
    }
    node.changes.push(row);
  }

  const list = document.getElementById("list");
  const sizer = document.getElementById("sizer");
  const viewport = document.getElementById("viewport");
  const filterInput = document.getElementById("filter");
  const hintEl = document.getElementById("toolbar-hint");
  const ROW_HEIGHT = 26;
  const OVERSCAN = 20;
  const instanceTotal = ${instanceCount};
  const collapsed = new Set();
  const propsOpen = new Set();
  const autoOpenProps = instanceTotal <= 12;
  let filterText = "";
  let flat = [];
  let renderFrame = 0;
  let lastStart = -1;
  let lastCount = -1;

  const matchCache = new Map();
  function nodeMatches(node, pathKey) {
    if (!filterText) return true;
    const cached = matchCache.get(pathKey);
    if (cached !== undefined) return cached;
    let out = node.name.toLowerCase().includes(filterText)
      || (node.className && node.className.toLowerCase().includes(filterText))
      || (node.changes && node.changes.some((c) => c.property.toLowerCase().includes(filterText)));
    if (!out) {
      for (const child of node.children.values()) {
        if (nodeMatches(child, pathKey + "." + child.name)) { out = true; break; }
      }
    }
    matchCache.set(pathKey, out);
    return out;
  }

  function flattenNode(node, pathKey, depth) {
    if (!nodeMatches(node, pathKey)) return;
    let chain = [node.name];
    let current = node;
    let key = pathKey;
    while (!current.changes && current.children.size === 1 && !filterText) {
      const child = current.children.values().next().value;
      chain.push(child.name);
      key = key + "." + child.name;
      current = child;
    }
    const isFolder = !current.changes;
    const hasKids = current.children.size > 0;
    const propCount = current.changes ? current.changes.length : 0;
    const isCollapsed = collapsed.has(key);
    const propsShown = propCount > 0 && !isCollapsed && (propsOpen.has(key) || autoOpenProps || !!filterText);
    flat.push({ kind: "node", key, chain, depth, isFolder, hasKids, propCount, isCollapsed, propsShown,
      status: current.status,
      className: current.className, icon: isFolder ? (depth === 0 ? (SERVICE_ICONS[chain[0]] || FOLDER_ICON) : FOLDER_ICON) : current.icon });
    if (isCollapsed) return;
    if (propsShown) {
      for (const change of current.changes) {
        flat.push({ kind: "prop", depth, change, neutral: current.status === "added" });
      }
    }
    const children = [...current.children.values()].sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }));
    for (const child of children) {
      flattenNode(child, key + "." + child.name, depth + 1);
    }
  }

  function rebuildFlat() {
    flat = [];
    matchCache.clear();
    for (const service of root.children.values()) {
      flattenNode(service, service.name, 0);
    }
    sizer.style.height = (flat.length * ROW_HEIGHT) + "px";
    hintEl.textContent = filterText && flat.length === 0 ? "No changes match" : "";
    lastStart = -1;
    renderWindow();
  }

  function nodeRowHtml(item) {
    const expandable = item.hasKids || item.propCount > 0;
    const open = !(item.isCollapsed || (item.propCount > 0 && !item.propsShown && !item.hasKids));
    const statusClass = item.status === "added" ? " added" : item.status === "removed" ? " removed" : "";
    const statusTitle = item.status === "added" ? "Added in Studio" : item.status === "removed" ? "Removed in Studio" : (item.className || "");
    return '<div class="row' + (item.isFolder ? " folder" : "") + statusClass + '" data-key="' + esc(item.key) + '" title="' + esc(statusTitle) + '" style="padding-left:' + (item.depth * 14 + 6) + 'px">' +
      '<span class="twisty' + (open ? " open" : "") + (expandable ? "" : " blank") + '">\\u25B8</span>' +
      '<img class="icon" src="' + ASSET + "/" + esc(item.icon || "Folder") + '.png">' +
      '<span class="rname">' + item.chain.map(esc).join('<span class="rsep">\\u203A</span>') + "</span>" +
      (item.propCount > 0 ? '<span class="count">' + item.propCount + "</span>" : "") +
      "</div>";
  }

  function propRowHtml(item) {
    const row = item.change;
    const oldHtml = item.neutral ? null : fmt(row.oldValue);
    const neutral = item.neutral || oldHtml === null;
    const scopeBadge = row.scope !== "property" ? '<span class="scope-badge">' + esc(row.scope) + "</span>" : "";
    return '<div class="prop-row" style="margin-left:' + (item.depth * 14 + 23) + 'px">' +
      '<span class="prop-name-cell"><span class="prop-name">' + esc(row.property) + "</span>" + scopeBadge + "</span>" +
      '<span class="values">' + (oldHtml !== null ? '<span class="val old">' + oldHtml + '</span><span class="arrow">\\u2192</span>' : "") +
      '<span class="val ' + (neutral ? "neutral" : "new") + '">' + fmt(row.newValue) + "</span></span></div>";
  }

  function renderWindow() {
    const start = Math.max(0, Math.floor(list.scrollTop / ROW_HEIGHT) - OVERSCAN);
    const count = Math.min(flat.length - start, Math.ceil((list.clientHeight || 400) / ROW_HEIGHT) + OVERSCAN * 2);
    if (start === lastStart && count === lastCount) return;
    lastStart = start;
    lastCount = count;
    const parts = [];
    for (let i = start; i < start + count; i++) {
      const item = flat[i];
      parts.push(item.kind === "node" ? nodeRowHtml(item) : propRowHtml(item));
    }
    viewport.style.top = (start * ROW_HEIGHT) + "px";
    viewport.innerHTML = parts.join("");
  }

  list.addEventListener("scroll", () => {
    if (renderFrame) return;
    renderFrame = requestAnimationFrame(() => { renderFrame = 0; renderWindow(); });
  });

  viewport.addEventListener("click", (event) => {
    const row = event.target.closest(".row");
    if (!row) return;
    const key = row.dataset.key;
    const item = flat.find((entry) => entry.kind === "node" && entry.key === key);
    if (!item || !(item.hasKids || item.propCount > 0)) return;
    if (item.isFolder || item.hasKids) {
      if (collapsed.has(key)) collapsed.delete(key); else collapsed.add(key);
    }
    if (item.propCount > 0 && !item.hasKids) {
      if (item.propsShown) { propsOpen.delete(key); if (autoOpenProps || filterText) collapsed.add(key); }
      else { propsOpen.add(key); collapsed.delete(key); }
    }
    rebuildFlat();
  });

  filterInput.addEventListener("input", () => {
    filterText = filterInput.value.trim().toLowerCase();
    rebuildFlat();
  });
  filterInput.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { filterInput.value = ""; filterText = ""; rebuildFlat(); }
  });
  rebuildFlat();

  let secs = 90;
  let paused = false;
  const secsEl = document.getElementById("secs");
  const fillEl = document.getElementById("fill");
  const labelEl = document.getElementById("count-label");
  list.addEventListener("mouseenter", () => { paused = true; labelEl.innerHTML = "Auto import paused while reviewing"; });
  list.addEventListener("mouseleave", () => {
    paused = false;
    labelEl.innerHTML = 'Protected full import in <b id="secs">' + secs + "</b>s &mdash; hover the list to pause";
  });
  const timer = setInterval(() => {
    if (paused) return;
    secs -= 1;
    const liveSecs = document.getElementById("secs");
    if (liveSecs) liveSecs.textContent = String(secs);
    fillEl.style.width = (secs / 90 * 100) + "%";
    if (secs <= 0) { clearInterval(timer); vscode.postMessage({ action: "full" }); }
  }, 1000);

  const applyButton = document.getElementById("apply");
  if (applyButton) applyButton.addEventListener("click", () => vscode.postMessage({ action: "apply" }));
  document.getElementById("full").addEventListener("click", () => vscode.postMessage({ action: "full" }));
  document.getElementById("skip").addEventListener("click", () => vscode.postMessage({ action: "discard" }));
</script>
</body>
</html>`;
  }

  private async tryApplyStudioPropertyChangesToEditor(
    state: StudioChangeState,
    dirtyServices: string[],
    cfg: SyncConfig,
  ): Promise<boolean> {
    const fullSyncServices = Array.isArray(state.fullSyncServices)
      ? state.fullSyncServices.map((service) => service.trim()).filter((service) => service.length > 0)
      : [];
    const propertyChanges = Array.isArray(state.propertyChanges) ? state.propertyChanges : [];
    const trackedChanges = this.studioChangeLogEntries(state, dirtyServices);
    const changeCount = trackedChanges.length > 0 ? trackedChanges.length : propertyChanges.length;
    if (propertyChanges.length === 0 || fullSyncServices.length > 0) {
      if (changeCount > cfg.changesThreshold && trackedChanges.length > 0 && cfg.displayPrompts !== "never") {
        const decision = await this.showStudioChangePreview(propertyChanges, trackedChanges, changeCount, cfg, true);
        if (decision === "discard") {
          this.output.appendLine(
            `[renium] Studio -> editor: ${changeCount} changes skipped from review; editor files were not updated.`,
          );
          return true;
        }
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes reviewed; running protected full import.`,
        );
      }
      return false;
    }

    if (changeCount > cfg.changesThreshold) {
      if (cfg.displayPrompts === "never") {
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes exceed liveSync.changesThreshold=${cfg.changesThreshold}; using protected full import.`,
        );
        return false;
      }
      const decision = await this.showStudioChangePreview(propertyChanges, trackedChanges, changeCount, cfg, false);
      if (decision === "full") {
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes reviewed; running protected full import.`,
        );
        return false;
      }
      if (decision === "discard") {
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes skipped from review; editor files were not updated.`,
        );
        return true;
      }
      this.output.appendLine(`[renium] Studio -> editor: applying ${changeCount} reviewed changes.`);
    }

    const dirtySet = new Set(dirtyServices.map((service) => service.trim()).filter((service) => service.length > 0));
    const changeServices = new Set<string>();
    for (const change of propertyChanges) {
      const service = String(change.service ?? "").trim();
      if (service.length > 0) {
        changeServices.add(service);
      }
    }
    for (const service of dirtySet) {
      if (!changeServices.has(service)) {
        return false;
      }
    }

    this.ensureFileExists(cfg.rustCliPath);
    const changedFiles = new Set<string>();
    const changedSettingsFiles = new Set<string>();
    for (const change of propertyChanges) {
      const service = String(change.service ?? "").trim();
      const property = String(change.property ?? "").trim();
      if (!dirtySet.has(service) || property.length === 0) {
        return false;
      }

      const settingsFile = existingReniumSettingsFile(cfg.projectRoot, service);
      if (!fs.existsSync(settingsFile)) {
        return false;
      }

      const pathSegments = Array.isArray(change.pathSegments) ? change.pathSegments : [];
      if (property === "Source" && (change.scope ?? "property") === "property") {
        if (typeof change.value !== "string") {
          return false;
        }
        this.noteProgrammaticEditorWrite({ paths: [settingsFile], durationMs: 5000 });
        const sourcePath = await this.applyStudioSourceChangeToEditor(cfg, settingsFile, service, change);
        this.noteProgrammaticEditorWrite({ paths: [settingsFile, sourcePath], durationMs: 5000, refreshCache: true });
        changedFiles.add(sourcePath);
        continue;
      }

      const args = [
        "bytecode-set-property",
        "-f",
        settingsFile,
        "-p",
        property,
        "-S",
        change.scope ?? "property",
        `--value-json=${JSON.stringify(change.value ?? null)}`,
      ];
      const settingsId = String(change.settingsId ?? "").trim();
      if (settingsId.length > 0) {
        args.push("-i", settingsId);
      } else if (pathSegments.length >= 1 && pathSegments[0] === service) {
        args.push("--path-segments-json", JSON.stringify(pathSegments));
        args.push("--path-ordinals-json", JSON.stringify(Array.isArray(change.pathOrdinals) ? change.pathOrdinals : []));
      } else {
        return false;
      }

      this.noteProgrammaticEditorWrite({ paths: [settingsFile], durationMs: 5000 });
      const result = await this.runCommand(
        cfg.rustCliPath,
        args,
        cfg.projectRoot,
        "studio-property-import",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
      if (result.code !== 0) {
        throw new Error(`Rust property import exited with code ${result.code}`);
      }
      this.noteProgrammaticEditorWrite({ paths: [settingsFile], durationMs: 5000, refreshCache: true });
      changedFiles.add(settingsFile);
      changedSettingsFiles.add(settingsFile);
    }

    const changedServices = Array.from(dirtySet);
    for (const filePath of changedFiles) {
      this.updateEditorLiveSyncCacheAfterPush([filePath], cfg);
    }
    if (changedSettingsFiles.size > 0) {
      try {
        await vscode.commands.executeCommand("renium.fileExplorer.refreshPropertyChanges", Array.from(changedSettingsFiles));
      } catch {
      }
    }
    this.studioToEditorImportSuppressUntilMs = Date.now() + Math.max(1000, Math.min(3000, cfg.studioLiveSyncPollMs * 2));
    this.studioToEditorLastSyncEndedAt = Date.now();
    this.logStudioChanges(state, "property", changedServices);
    return true;
  }

  private async applyStudioSourceChangeToEditor(
    cfg: SyncConfig,
    settingsFile: string,
    service: string,
    change: StudioPropertyChange,
  ): Promise<string> {
    const directSourcePath = this.tryApplyStudioSourceChangeToEditorFromSourcemap(cfg, change);
    if (directSourcePath) {
      return directSourcePath;
    }

    void settingsFile;
    void service;
    throw new Error("Studio source change could not be mapped via sourcemap; deferring to protected full import.");
  }

  private tryApplyStudioSourceChangeToEditorFromSourcemap(
    cfg: SyncConfig,
    change: StudioPropertyChange,
  ): string | undefined {
    if (typeof change.value !== "string") {
      return undefined;
    }

    const sourcePath = this.resolveStudioSourcePathFromSourcemap(cfg, change);
    if (!sourcePath) {
      return undefined;
    }

    const finalContent = this.reconcileStudioSourceWithLocalEdits(cfg, sourcePath, change.value);

    this.noteProgrammaticEditorWrite({ paths: [sourcePath], durationMs: 5000 });
    this.writeUtf8FileIfChanged(sourcePath, finalContent);
    this.writeSyncBase(cfg, sourcePath, finalContent);
    this.noteProgrammaticEditorWrite({ paths: [sourcePath], durationMs: 5000, refreshCache: true });
    return sourcePath;
  }

  /**
   * Reconcile an incoming Studio script-source value with any concurrent local
   * edits via a 3-way merge against the last-synced base. Non-overlapping edits
   * from both sides always merge automatically. For lines changed on BOTH sides,
   * the configured policy decides: "filesystem" keeps your local line, "studio"
   * keeps Studio's, "prompt" keeps your local line and surfaces a diff so you can
   * review/switch. The written file is always valid Lua (no conflict markers are
   * ever inserted), and both sides are backed up under .renium/conflicts.
   */
  private reconcileStudioSourceWithLocalEdits(cfg: SyncConfig, sourcePath: string, theirs: string): string {
    let ours: string;
    try {
      ours = fs.readFileSync(sourcePath, "utf8");
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== "ENOENT") {
        this.output.appendLine(`[renium] conflict: failed to read ${sourcePath}: ${String(err)}`);
      }
      return theirs;
    }

    if (ours === theirs) {
      return theirs;
    }

    const base = this.readSyncBase(cfg, sourcePath);
    if (base !== undefined && ours === base) {
      return theirs;
    }

    return this.mergeSourceAgainstBase(cfg, sourcePath, ours, theirs, base);
  }

  /**
   * Core 3-way merge + policy resolution for already-read local (`ours`) and
   * incoming (`theirs`) source. `base` may be undefined (no ancestor recorded).
   * Returns the content to write — always valid Lua, never conflict markers.
   */
  private mergeSourceAgainstBase(
    cfg: SyncConfig,
    sourcePath: string,
    ours: string,
    theirs: string,
    base: string | undefined,
  ): string {
    const policy = this.resolveConflictPolicy(cfg);
    const eol: "\n" | "\r\n" = ours.includes("\r\n") ? "\r\n" : "\n";

    let resolvedText: string;
    let conflicted: boolean;
    if (base === undefined) {
      resolvedText = policy === "studio" ? theirs : ours;
      conflicted = true;
    } else {
      const merged = mergeAndResolve(base, ours, theirs, policy, eol);
      resolvedText = merged.text;
      conflicted = merged.hadConflicts;
    }

    if (!conflicted) {
      return resolvedText;
    }

    const localBackup = this.backupConflictCopy(cfg, sourcePath, ours, "local");
    const studioBackup = this.backupConflictCopy(cfg, sourcePath, theirs, "studio");
    if (policy === "filesystem") {
      this.output.appendLine(`[renium] conflict on ${sourcePath}: kept filesystem version (Studio copy backed up to .renium/conflicts).`);
    } else if (policy === "studio") {
      this.output.appendLine(`[renium] conflict on ${sourcePath}: kept Studio version (local copy backed up to .renium/conflicts).`);
    } else {
      this.surfaceConflictChoice(cfg, sourcePath, localBackup, studioBackup);
    }
    return resolvedText;
  }

  /**
   * Surface a non-destructive conflict chooser in VS Code (no markers written).
   * The local version is already on disk; the user can open a side-by-side diff
   * of the two backups or switch the file to Studio's version.
   */
  private surfaceConflictChoice(
    cfg: SyncConfig,
    sourcePath: string,
    localBackup: string | undefined,
    studioBackup: string | undefined,
  ): void {
    const label = path.basename(sourcePath);
    this.output.appendLine(`[renium] conflict on ${sourcePath}: kept your local version; Studio's copy is in .renium/conflicts.`);
    if (cfg.displayPrompts === "never" || (cfg.displayPrompts === "initial" && this.displayedLiveSyncPrompt)) {
      return;
    }
    this.displayedLiveSyncPrompt = true;
    const actions: string[] = [];
    if (localBackup && studioBackup) {
      actions.push("Open Diff");
    }
    if (studioBackup) {
      actions.push("Use Studio's Version");
    }
    void vscode.window
      .showWarningMessage(
        `Renium: ${label} was edited in both Studio and your editor. Kept your version; Studio's copy is saved in .renium/conflicts.`,
        ...actions,
      )
      .then((choice) => {
        if (choice === "Open Diff" && localBackup && studioBackup) {
          const localPreview = this.conflictDiffPreview(cfg, localBackup);
          const studioPreview = this.conflictDiffPreview(cfg, studioBackup);
          void vscode.commands.executeCommand(
            "vscode.diff",
            vscode.Uri.file(localPreview.path),
            vscode.Uri.file(studioPreview.path),
            `${label}: local ↔ Studio`,
          );
        } else if (choice === "Use Studio's Version" && studioBackup) {
          try {
            fs.copyFileSync(studioBackup, sourcePath);
            this.output.appendLine(`[renium] conflict on ${sourcePath}: switched to Studio's version.`);
          } catch (err) {
            this.output.appendLine(`[renium] conflict: failed to apply Studio's version for ${sourcePath}: ${String(err)}`);
          }
        }
      });
  }

  /** Keep very large conflict previews responsive without truncating either
   * authoritative backup file. */
  private conflictDiffPreview(cfg: SyncConfig, backupPath: string): { path: string; truncated: boolean } {
    try {
      const content = fs.readFileSync(backupPath, "utf8");
      const lines = content.split(/\r\n|\r|\n/);
      if (lines.length <= cfg.diffLinesLimit) {
        return { path: backupPath, truncated: false };
      }
      const previewPath = `${backupPath}.preview-${cfg.diffLinesLimit}.txt`;
      const preview = [
        ...lines.slice(0, cfg.diffLinesLimit),
        `-- [Renium truncated this conflict preview after ${cfg.diffLinesLimit.toLocaleString()} lines. The full backup remains beside this file.]`,
        "",
      ].join("\n");
      fs.writeFileSync(previewPath, preview, "utf8");
      return { path: previewPath, truncated: true };
    } catch (err) {
      this.output.appendLine(`[renium] conflict: could not create limited diff preview for ${backupPath}: ${String(err)}`);
      return { path: backupPath, truncated: false };
    }
  }

  private syncBasePathForSource(cfg: SyncConfig, sourcePath: string): string | undefined {
    const srcRoot = path.join(cfg.projectRoot, "src");
    if (!this.isPathInside(sourcePath, srcRoot)) {
      return undefined;
    }
    return path.join(cfg.projectRoot, ".renium", "sync-base", path.relative(srcRoot, sourcePath));
  }

  private readSyncBase(cfg: SyncConfig, sourcePath: string): string | undefined {
    const basePath = this.syncBasePathForSource(cfg, sourcePath);
    if (!basePath) {
      return undefined;
    }
    try {
      return fs.readFileSync(basePath, "utf8");
    } catch {
      return undefined;
    }
  }

  public writeSyncBase(cfg: SyncConfig, sourcePath: string, content: string): void {
    const basePath = this.syncBasePathForSource(cfg, sourcePath);
    if (!basePath) {
      return;
    }
    try {
      fs.mkdirSync(path.dirname(basePath), { recursive: true });
      fs.writeFileSync(basePath, Buffer.from(content, "utf8"));
    } catch (err) {
      this.output.appendLine(`[renium] conflict: failed to update sync base for ${sourcePath}: ${String(err)}`);
    }
  }

  /** Record the current on-disk content of pushed Lua source files as the shared merge base. */
  private refreshSyncBasesForPaths(paths: string[], cfg: SyncConfig): void {
    for (const filePath of paths) {
      const abs = path.isAbsolute(filePath) ? filePath : path.resolve(cfg.projectRoot, filePath);
      if (!this.isLuaSourcePath(abs)) {
        continue;
      }
      try {
        this.writeSyncBase(cfg, abs, fs.readFileSync(abs, "utf8"));
      } catch {
      }
    }
  }

  private syncBaseExists(cfg: SyncConfig, sourcePath: string): boolean {
    const basePath = this.syncBasePathForSource(cfg, sourcePath);
    return basePath !== undefined && fs.existsSync(basePath);
  }

  /**
   * Before a full (Rust) import overwrites the service tree, snapshot the content
   * of scripts that diverge from their recorded sync base (i.e. have unpushed
   * local edits) so they can be merged back afterwards. Files with no base yet
   * are skipped here and seeded after the import to bootstrap protection.
   */
  private captureLocalScriptEditsForServices(services: string[], cfg: SyncConfig): Map<string, string> {
    const captured = new Map<string, string>();
    const srcRoot = path.join(cfg.projectRoot, "src");
    for (const service of this.normalizeServices(services, cfg.services)) {
      const serviceDir = path.join(srcRoot, service);
      if (!fs.existsSync(serviceDir)) {
        continue;
      }
      for (const filePath of this.collectInitialEditorLiveSyncPaths(serviceDir)) {
        if (!this.isLuaSourcePath(filePath)) {
          continue;
        }
        const base = this.readSyncBase(cfg, filePath);
        if (base === undefined) {
          continue;
        }
        let content: string;
        try {
          content = fs.readFileSync(filePath, "utf8");
        } catch {
          continue;
        }
        if (content !== base) {
          captured.set(filePath, content);
        }
      }
    }
    return captured;
  }

  /**
   * After a full import, 3-way merge any captured local edits against the freshly
   * imported Studio content, then seed bases for scripts that lack one. Returns
   * the files whose local edits survived the merge (and must be pushed to Studio).
   */
  private reconcileLocalEditsAfterFullImport(
    services: string[],
    cfg: SyncConfig,
    captured: Map<string, string>,
  ): string[] {
    const surviving: string[] = [];
    for (const [filePath, localContent] of captured) {
      let newDisk: string;
      try {
        newDisk = fs.readFileSync(filePath, "utf8");
      } catch {
        continue;
      }
      if (localContent === newDisk) {
        this.writeSyncBase(cfg, filePath, newDisk);
        continue;
      }
      const base = this.readSyncBase(cfg, filePath);
      const resolved = this.mergeSourceAgainstBase(cfg, filePath, localContent, newDisk, base);
      if (resolved === newDisk) {
        this.writeSyncBase(cfg, filePath, newDisk);
        continue;
      }
      this.writeUtf8FileIfChanged(filePath, resolved);
      surviving.push(filePath);
    }

    const srcRoot = path.join(cfg.projectRoot, "src");
    for (const service of this.normalizeServices(services, cfg.services)) {
      const serviceDir = path.join(srcRoot, service);
      if (!fs.existsSync(serviceDir)) {
        continue;
      }
      for (const filePath of this.collectInitialEditorLiveSyncPaths(serviceDir)) {
        if (!this.isLuaSourcePath(filePath) || captured.has(filePath) || this.syncBaseExists(cfg, filePath)) {
          continue;
        }
        try {
          this.writeSyncBase(cfg, filePath, fs.readFileSync(filePath, "utf8"));
        } catch {
        }
      }
    }
    return surviving;
  }

  private invalidateEditorLiveSyncCacheEntries(paths: string[], cfg: SyncConfig): void {
    const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
    let changed = false;
    for (const filePath of paths) {
      const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
      if (cache.files[key] !== undefined) {
        delete cache.files[key];
        changed = true;
      }
    }
    if (changed) {
      this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
    }
  }

  private backupConflictCopy(cfg: SyncConfig, sourcePath: string, content: string, side: "local" | "studio"): string | undefined {
    try {
      const srcRoot = path.join(cfg.projectRoot, "src");
      const rel = this.isPathInside(sourcePath, srcRoot) ? path.relative(srcRoot, sourcePath) : path.basename(sourcePath);
      const stamp = new Date().toISOString().replace(/[:.]/g, "-");
      const dest = path.join(cfg.projectRoot, ".renium", "conflicts", stamp, `${rel}.${side}`);
      fs.mkdirSync(path.dirname(dest), { recursive: true });
      fs.writeFileSync(dest, Buffer.from(content, "utf8"));
      return dest;
    } catch (err) {
      this.output.appendLine(`[renium] conflict: failed to back up ${side} copy of ${sourcePath}: ${String(err)}`);
      return undefined;
    }
  }

  private notifyConflict(sourcePath: string, policy: ConflictPolicy, manual: boolean, detail: string): void {
    const label = path.basename(sourcePath);
    this.output.appendLine(`[renium] live-sync conflict on ${sourcePath} (policy=${policy}; ${detail})`);
    if (policy === "prompt" || manual) {
      void vscode.window
        .showWarningMessage(
          `Renium: concurrent edits to ${label} — ${manual ? "conflict markers written; resolve manually" : detail}. Backups in .renium/conflicts.`,
          "Open File",
        )
        .then((choice) => {
          if (choice === "Open File") {
            void vscode.window.showTextDocument(vscode.Uri.file(sourcePath));
          }
        });
    }
  }

  private normalizeConflictPolicy(raw: string | undefined): ConflictPolicy {
    switch (String(raw ?? "").trim().toLowerCase()) {
      case "filesystem":
        return "filesystem";
      case "studio":
        return "studio";
      case "prompt":
      case "none":
      default:
        return "prompt";
    }
  }

  private resolveConflictPolicy(cfg: SyncConfig): ConflictPolicy {
    return this.studioConflictPolicyOverride ?? cfg.conflictResolution;
  }

  private resolveStudioSourcePathFromSourcemap(cfg: SyncConfig, change: StudioPropertyChange): string | undefined {
    const pathSegments = Array.isArray(change.pathSegments) ? change.pathSegments.map((segment) => String(segment)) : [];
    if (pathSegments.length === 0) {
      return undefined;
    }

    const root = this.loadSourcemapRoot(cfg);
    if (!root) {
      return undefined;
    }

    const pathOrdinals = Array.isArray(change.pathOrdinals)
      ? change.pathOrdinals.map((ordinal) => Number(ordinal))
      : [];
    let node: SourcemapNode | undefined = root;
    let segmentIndex = String(root.name ?? "") === pathSegments[0] ? 1 : 0;
    for (; segmentIndex < pathSegments.length; segmentIndex += 1) {
      node = this.sourcemapChildForSegment(node, pathSegments[segmentIndex], pathOrdinals[segmentIndex]);
      if (!node) {
        return undefined;
      }
    }

    const expectedClassName = String(change.className ?? "").trim();
    if (expectedClassName.length > 0 && typeof node.className === "string" && node.className !== expectedClassName) {
      return undefined;
    }

    return this.sourcemapNodeSourcePath(cfg, node);
  }

  private loadSourcemapRoot(cfg: SyncConfig): SourcemapNode | undefined {
    const sourcemapPath = path.join(cfg.projectRoot, "sourcemap.json");
    try {
      const stat = fs.statSync(sourcemapPath);
      if (
        this.sourcemapCache &&
        this.sourcemapCache.path === sourcemapPath &&
        this.sourcemapCache.mtimeMs === stat.mtimeMs
      ) {
        return this.sourcemapCache.root;
      }

      const parsed = JSON.parse(fs.readFileSync(sourcemapPath, "utf8")) as unknown;
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
        return undefined;
      }
      const root = parsed as SourcemapNode;
      this.sourcemapCache = {
        path: sourcemapPath,
        mtimeMs: stat.mtimeMs,
        root,
      };
      return root;
    } catch {
      return undefined;
    }
  }

  private sourcemapChildForSegment(
    parent: SourcemapNode | undefined,
    segment: string,
    rawOrdinal: number | undefined,
  ): SourcemapNode | undefined {
    if (!parent || !Array.isArray(parent.children)) {
      return undefined;
    }
    const children = parent.children.filter((child): child is SourcemapNode => (
      !!child && typeof child === "object" && !Array.isArray(child)
    ));
    const matches = children.filter((child) => child.name === segment);
    if (matches.length === 0) {
      return undefined;
    }

    if (typeof rawOrdinal === "number" && Number.isFinite(rawOrdinal)) {
      const ordinal = Math.max(1, Math.floor(rawOrdinal));
      return matches[ordinal - 1] ?? (matches.length === 1 ? matches[0] : undefined);
    }

    return matches.length === 1 ? matches[0] : undefined;
  }

  private sourcemapNodeSourcePath(cfg: SyncConfig, node: SourcemapNode): string | undefined {
    if (!Array.isArray(node.filePaths)) {
      return undefined;
    }
    const rawPath = node.filePaths
      .map((value) => String(value))
      .find((value) => this.isLuaSourcePath(value));
    if (!rawPath) {
      return undefined;
    }

    const sourcePath = path.isAbsolute(rawPath) ? path.resolve(rawPath) : path.resolve(cfg.projectRoot, rawPath);
    const srcRoot = path.join(cfg.projectRoot, "src");
    if (!this.isPathInside(sourcePath, srcRoot)) {
      return undefined;
    }
    return sourcePath;
  }

  private writeUtf8FileIfChanged(filePath: string, content: string): void {
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    const next = Buffer.from(content, "utf8");
    try {
      const current = fs.readFileSync(filePath);
      if (current.length === next.length && current.equals(next)) {
        return;
      }
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== "ENOENT") {
        throw err;
      }
    }
    fs.writeFileSync(filePath, next);
  }

  private shouldDropLikelySelfDirtyStudioState(_dirtyServices: string[], _cfg: SyncConfig): boolean {
    return false;
  }

  private diffServicesBySnapshotFingerprint(services: string[], cfg: SyncConfig): StudioSnapshotDiff {
    const changedServices: string[] = [];
    const fingerprintsByService = new Map<string, string>();
    for (const service of services) {
      const fingerprint = this.snapshotFingerprintForService(service, cfg);
      if (!fingerprint) {
        changedServices.push(service);
        continue;
      }
      fingerprintsByService.set(service, fingerprint);
      const previous = this.studioSnapshotFingerprintByService.get(service);
      if (previous !== fingerprint) {
        changedServices.push(service);
      }
    }
    if (changedServices.length === 0) {
      return { changedServices, fingerprintsByService };
    }
    return { changedServices, fingerprintsByService };
  }

  private commitStudioSnapshotFingerprints(services: string[], fingerprintsByService?: Map<string, string>): void {
    if (!fingerprintsByService) {
      return;
    }
    for (const service of services) {
      const fingerprint = fingerprintsByService.get(service);
      if (fingerprint) {
        this.studioSnapshotFingerprintByService.set(service, fingerprint);
      }
    }
  }

  private snapshotFingerprintForService(service: string, cfg: SyncConfig): string | undefined {
    const snapshotRoot = this.resolveSnapshotPath(cfg);
    const paths = this.collectSnapshotFingerprintPaths(snapshotRoot, service);
    if (paths.length === 0) {
      return undefined;
    }

    const rootFile = path.join(snapshotRoot, service + ".json");
    const hash = crypto.createHash("sha256");
    let hashedAnyFile = false;
    for (const filePath of paths) {
      let stat: fs.Stats;
      try {
        stat = fs.statSync(filePath);
      } catch {
        continue;
      }
      if (!stat.isFile()) {
        continue;
      }
      const relPath = this.normalizePathForCompare(path.relative(snapshotRoot, filePath));
      const content = fs.readFileSync(filePath);
      const fingerprintContent = path.resolve(filePath) === path.resolve(rootFile)
        ? this.normalizeSnapshotRootForFingerprint(content, service)
        : content;
      hash.update(relPath);
      hash.update("\0");
      hash.update(String(fingerprintContent.length));
      hash.update("\0");
      hash.update(fingerprintContent);
      hash.update("\0");
      hashedAnyFile = true;
    }
    return hashedAnyFile ? hash.digest("hex") : undefined;
  }

  private normalizeSnapshotRootForFingerprint(content: Buffer, service: string): Buffer {
    const text = content.toString("utf8");
    try {
      const parsed = JSON.parse(text) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const snapshot = parsed as Record<string, unknown>;
        const filteredInstanceCount = this.normalizeSnapshotInstancesForFingerprint(snapshot, service);
        const metadata = snapshot.metadata;
        if (metadata && typeof metadata === "object" && !Array.isArray(metadata)) {
          const stableMetadata = { ...(metadata as Record<string, unknown>) };
          delete stableMetadata.generatedAtUnix;
          if (filteredInstanceCount !== undefined) {
            stableMetadata.instanceCount = filteredInstanceCount;
          }
          snapshot.metadata = stableMetadata;
        }
        return Buffer.from(this.stableJsonStringify(snapshot), "utf8");
      }
    } catch {
    }

    return Buffer.from(
      text.replace(/(\"generatedAtUnix\"\s*:\s*)-?\d+(\s*,?)/g, (_match, prefix, suffix) => prefix + "0" + suffix),
      "utf8",
    );
  }

  private normalizeSnapshotInstancesForFingerprint(snapshot: Record<string, unknown>, service: string): number | undefined {
    const rawInstances = snapshot.instances;
    if (!Array.isArray(rawInstances)) {
      return undefined;
    }

    const entries = rawInstances.map((entry) => (
      entry && typeof entry === "object" && !Array.isArray(entry)
        ? { ...(entry as Record<string, unknown>) }
        : entry
    ));
    const removedIndices = new Set<number>();
    let changed = false;

    for (let index = 0; index < entries.length; index += 1) {
      const entry = entries[index];
      if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
        continue;
      }
      const instance = entry as Record<string, unknown>;
      if (this.normalizeSnapshotPropertiesForFingerprint(instance)) {
        changed = true;
      }
      if (service === "Workspace" && index === 0) {
        const properties = instance.properties;
        if (properties && typeof properties === "object" && !Array.isArray(properties) && "CurrentCamera" in properties) {
          const stableProperties = { ...(properties as Record<string, unknown>) };
          delete stableProperties.CurrentCamera;
          instance.properties = stableProperties;
          changed = true;
        }
      }
      if (instance.className === "Camera") {
        removedIndices.add(this.snapshotInstanceIndex(instance, index));
        changed = true;
      }
    }

    const filtered = entries.filter((entry, index) => {
      if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
        return true;
      }
      return !removedIndices.has(this.snapshotInstanceIndex(entry as Record<string, unknown>, index));
    });
    if (changed || filtered.length !== rawInstances.length) {
      snapshot.instances = filtered;
    }
    return filtered.length;
  }

  private normalizeSnapshotPropertiesForFingerprint(instance: Record<string, unknown>): boolean {
    const properties = instance.properties;
    if (!properties || typeof properties !== "object" || Array.isArray(properties)) {
      return false;
    }

    const source = properties as Record<string, unknown>;
    let stableProperties: Record<string, unknown> | undefined;
    for (const key of Object.keys(source)) {
      if (TRANSIENT_SNAPSHOT_PROPERTY_NAMES.has(key.toLowerCase())) {
        if (!stableProperties) {
          stableProperties = { ...source };
        }
        delete stableProperties[key];
      }
    }

    if (!stableProperties) {
      return false;
    }
    instance.properties = stableProperties;
    return true;
  }

  private snapshotInstanceIndex(instance: Record<string, unknown>, fallbackIndex: number): number {
    return this.snapshotNumericIndex(instance.instanceIndex) ?? fallbackIndex + 1;
  }

  private snapshotNumericIndex(value: unknown): number | undefined {
    if (typeof value !== "number" || !Number.isFinite(value)) {
      return undefined;
    }
    const index = Math.floor(value);
    return index > 0 ? index : undefined;
  }

  private stableJsonStringify(value: unknown): string {
    if (Array.isArray(value)) {
      return "[" + value.map((entry) => this.stableJsonStringify(entry)).join(",") + "]";
    }
    if (value && typeof value === "object") {
      const record = value as Record<string, unknown>;
      return "{" + Object.keys(record)
        .sort()
        .map((key) => JSON.stringify(key) + ":" + this.stableJsonStringify(record[key]))
        .join(",") + "}";
    }
    const primitive = JSON.stringify(value);
    return primitive === undefined ? "null" : primitive;
  }

  private collectSnapshotFingerprintPaths(snapshotRoot: string, service: string): string[] {
    const paths: string[] = [];
    const rootFile = path.join(snapshotRoot, `${service}.json`);
    if (fs.existsSync(rootFile)) {
      paths.push(rootFile);
    }

    const rootDir = path.join(snapshotRoot, service);
    if (fs.existsSync(rootDir)) {
      const stack = [rootDir];
      while (stack.length > 0) {
        const dir = stack.pop();
        if (!dir) {
          continue;
        }
        let entries: fs.Dirent[];
        try {
          entries = fs.readdirSync(dir, { withFileTypes: true });
        } catch {
          continue;
        }
        for (const entry of entries) {
          const fullPath = path.join(dir, entry.name);
          if (entry.isDirectory()) {
            stack.push(fullPath);
          } else if (entry.isFile()) {
            paths.push(fullPath);
          }
        }
      }
    }

    return paths.sort((a, b) => this.comparePathsForStableOrder(a, b));
  }

  private collectInitialEditorLiveSyncPaths(srcRoot: string): string[] {
    const settingsPathsByDirectory = new Map<string, string>();
    const otherPaths: string[] = [];
    const stack = [srcRoot];
    while (stack.length > 0) {
      const dir = stack.pop();
      if (!dir) {
        continue;
      }
      let entries: fs.Dirent[];
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const entry of entries) {
        const fullPath = path.join(dir, entry.name);
        if (entry.isDirectory()) {
          stack.push(fullPath);
          continue;
        }
        if (!entry.isFile()) {
          continue;
        }
        if (isReniumSettingsFileName(entry.name)) {
          const directory = path.resolve(path.dirname(fullPath));
          const previous = settingsPathsByDirectory.get(directory);
          if (!previous || isCanonicalReniumSettingsFileName(entry.name)) {
            settingsPathsByDirectory.set(directory, fullPath);
          }
        } else {
          otherPaths.push(fullPath);
        }
      }
    }
    return [
      ...Array.from(settingsPathsByDirectory.values()).sort((a, b) => this.comparePathsForStableOrder(a, b)),
      ...otherPaths.sort((a, b) => this.comparePathsForStableOrder(a, b)),
    ];
  }

  private collectInitialEditorLiveSyncSettingsPaths(srcRoot: string): string[] {
    return this.collectInitialEditorLiveSyncPaths(srcRoot)
      .filter((filePath) => isReniumSettingsFileName(path.basename(filePath)));
  }

  private collectEditorLiveSyncPathsForServices(services: string[], cfg: SyncConfig): string[] {
    const srcRoot = path.join(cfg.projectRoot, "src");
    const selectedServices = this.normalizeServices(services, cfg.services);
    const paths: string[] = [];
    for (const service of selectedServices) {
      const serviceDir = path.join(srcRoot, service);
      if (!fs.existsSync(serviceDir)) {
        continue;
      }
      paths.push(...this.collectInitialEditorLiveSyncPaths(serviceDir));
    }
    return [...new Set(paths.map((filePath) => path.resolve(filePath)))].sort((a, b) => this.comparePathsForStableOrder(a, b));
  }

  private collectInitialEditorLiveSyncTargetIds(srcRoot: string, settingsPaths: string[]): { paths: string[]; targetSettingsIds: string[] } {
    const cfg = this.getConfig();
    const result = childProcess.spawnSync(
      cfg.exportCliPath,
      [
        "bt",
        "-d",
        srcRoot,
        "-s",
        cfg.services.join(","),
      ],
      {
        cwd: cfg.projectRoot,
        encoding: "utf8",
        maxBuffer: 16 * 1024 * 1024,
        windowsHide: true,
      },
    );
    if (result.status !== 0) {
      const message = (result.stderr || result.stdout || "").trim();
      this.output.appendLine(`[renium] editor live sync initial target scan failed: ${message || `exit ${result.status}`}`);
      return { paths: [], targetSettingsIds: [] };
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(result.stdout);
    } catch (err) {
      this.output.appendLine(`[renium] editor live sync initial target scan failed: ${err instanceof Error ? err.message : String(err)}`);
      return { paths: [], targetSettingsIds: [] };
    }
    const rawPaths = Array.isArray((parsed as { paths?: unknown }).paths)
      ? (parsed as { paths: unknown[] }).paths
      : [];
    const rawIds = Array.isArray((parsed as { targetSettingsIds?: unknown }).targetSettingsIds)
      ? (parsed as { targetSettingsIds: unknown[] }).targetSettingsIds
      : [];
    const validSettingsPaths = new Set(settingsPaths.map((settingsPath) => this.normalizePathForCompare(settingsPath)));
    const paths = rawPaths
      .map((value) => String(value))
      .filter((value) => validSettingsPaths.has(this.normalizePathForCompare(value)));
    return {
      paths,
      targetSettingsIds: [...new Set(rawIds.map((value) => String(value)).filter((value) => value.startsWith("editor:")))],
    };
  }

  private editorLiveSyncCachePath(projectRoot: string): string {
    return path.join(projectRoot, ".renium", "editor-live-sync-cache.json");
  }

  private emptyEditorLiveSyncCache(projectRoot: string): EditorLiveSyncHashCache {
    return {
      version: 1,
      projectRoot: path.resolve(projectRoot),
      updatedAtUnixMs: Date.now(),
      files: {},
    };
  }

  private loadEditorLiveSyncCache(projectRoot: string): { cache: EditorLiveSyncHashCache; existed: boolean } {
    const cachePath = this.editorLiveSyncCachePath(projectRoot);
    try {
      const parsed = JSON.parse(fs.readFileSync(cachePath, "utf8")) as Partial<EditorLiveSyncHashCache>;
      if (
        parsed &&
        parsed.version === 1 &&
        parsed.files &&
        typeof parsed.files === "object" &&
        !Array.isArray(parsed.files)
      ) {
        return {
          existed: true,
          cache: {
            version: 1,
            projectRoot: typeof parsed.projectRoot === "string" ? parsed.projectRoot : path.resolve(projectRoot),
            updatedAtUnixMs: typeof parsed.updatedAtUnixMs === "number" ? parsed.updatedAtUnixMs : 0,
            files: Object.fromEntries(
              Object.entries(parsed.files).filter((entry): entry is [string, string] => typeof entry[1] === "string"),
            ),
          },
        };
      }
    } catch {
    }
    return { existed: false, cache: this.emptyEditorLiveSyncCache(projectRoot) };
  }

  private saveEditorLiveSyncCache(projectRoot: string, cache: EditorLiveSyncHashCache): void {
    const cachePath = this.editorLiveSyncCachePath(projectRoot);
    fs.mkdirSync(path.dirname(cachePath), { recursive: true });
    const nextCache: EditorLiveSyncHashCache = {
      version: 1,
      projectRoot: path.resolve(projectRoot),
      updatedAtUnixMs: Date.now(),
      files: cache.files,
    };
    fs.writeFileSync(cachePath, `${JSON.stringify(nextCache, null, 2)}${os.EOL}`, "utf8");
  }

  private editorLiveSyncCacheKey(filePath: string, projectRoot: string): string {
    const absolutePath = path.resolve(projectRoot, filePath);
    const relative = path.relative(projectRoot, absolutePath);
    const normalized = relative.split(path.sep).join("/");
    return process.platform === "win32" ? normalized.toLowerCase() : normalized;
  }

  private editorLiveSyncFileHash(filePath: string): string | undefined {
    try {
      const stat = fs.statSync(filePath);
      if (!stat.isFile()) {
        return undefined;
      }
      const hash = crypto.createHash("sha256");
      hash.update(fs.readFileSync(filePath));
      return `sha256:${stat.size}:${hash.digest("hex")}`;
    } catch {
      return undefined;
    }
  }

  private primeEditorLiveSyncCache(paths: string[], cfg: SyncConfig): void {
    const cache = this.emptyEditorLiveSyncCache(cfg.projectRoot);
    for (const filePath of paths) {
      const hash = this.editorLiveSyncFileHash(filePath);
      if (hash) {
        cache.files[this.editorLiveSyncCacheKey(filePath, cfg.projectRoot)] = hash;
      }
    }
    this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
  }

  private filterEditorLiveSyncChangedPaths(paths: string[], cfg: SyncConfig): string[] {
    const { cache, existed } = this.loadEditorLiveSyncCache(cfg.projectRoot);
    const seen = new Set<string>();
    const changed: string[] = [];
    const currentHashes: Record<string, string> = {};

    for (const filePath of paths) {
      const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
      if (!seen.add(key)) {
        continue;
      }
      const hash = this.editorLiveSyncFileHash(filePath);
      if (hash) {
        currentHashes[key] = hash;
      }
      if (!existed) {
        continue;
      }
      if (hash === undefined) {
        if (cache.files[key] !== undefined) {
          changed.push(filePath);
        }
        continue;
      }
      if (cache.files[key] !== hash) {
        changed.push(filePath);
      }
    }

    if (!existed) {
      cache.files = currentHashes;
      this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
      return [];
    }

    return this.excludeUnresolvedConflictMarkerPaths(changed);
  }

  /**
   * Never push a script that still contains unresolved git-style conflict
   * markers — that would send syntactically broken Lua to Studio. Such files are
   * held back (logged once) until the user resolves them, after which they sync
   * normally on the next change.
   */
  private excludeUnresolvedConflictMarkerPaths(paths: string[]): string[] {
    const pushable: string[] = [];
    for (const filePath of paths) {
      const key = this.normalizePathForCompare(filePath);
      if (this.isLuaSourcePath(filePath) && this.fileHasConflictMarkers(filePath)) {
        this.output.appendLine(
          `[renium] live-sync: holding back ${filePath} — unresolved conflict markers present; resolve them to resume syncing this file.`,
        );
        if (!this.conflictMarkerWarnedKeys.has(key)) {
          this.conflictMarkerWarnedKeys.add(key);
          void vscode.window.showWarningMessage(
            `Renium: ${path.basename(filePath)} has unresolved merge conflict markers and won't sync to Studio until resolved.`,
            "Open File",
          ).then((choice) => {
            if (choice === "Open File") {
              void vscode.window.showTextDocument(vscode.Uri.file(filePath));
            }
          });
        }
        continue;
      }
      if (this.conflictMarkerWarnedKeys.delete(key)) {
        this.output.appendLine(`[renium] live-sync: ${filePath} conflict markers resolved; resuming sync.`);
      }
      pushable.push(filePath);
    }
    return pushable;
  }

  private fileHasConflictMarkers(filePath: string): boolean {
    let content: string;
    try {
      content = fs.readFileSync(filePath, "utf8");
    } catch {
      return false;
    }
    return /^<{7} /m.test(content) && /^>{7} /m.test(content);
  }

  private clearExpiredSuppressedEditorLiveSyncPaths(now = Date.now()): void {
    for (const [fileKey, untilMs] of this.suppressedEditorLiveSyncPathUntilByKey) {
      if (untilMs <= now) {
        this.suppressedEditorLiveSyncPathUntilByKey.delete(fileKey);
      }
    }
  }

  private editorLiveSyncSuppressedUntil(filePath: string, now = Date.now()): number {
    this.clearExpiredSuppressedEditorLiveSyncPaths(now);
    const fileKey = this.normalizePathForCompare(filePath);
    const pathUntilMs = this.suppressedEditorLiveSyncPathUntilByKey.get(fileKey) ?? 0;
    const globalUntilMs = this.studioToEditorImportInProgress
      ? Math.max(this.studioToEditorImportSuppressUntilMs, now + 100)
      : this.studioToEditorImportSuppressUntilMs;
    return Math.max(pathUntilMs, globalUntilMs);
  }

  private isEditorLiveSyncPathSuppressed(filePath: string, now = Date.now()): boolean {
    return this.editorLiveSyncSuppressedUntil(filePath, now) > now;
  }

  private scheduleEditorLiveSyncFlush(delayMs: number): void {
    const normalizedDelayMs = Math.max(0, Math.ceil(delayMs));
    const dueAt = Date.now() + normalizedDelayMs;
    if (this.liveSyncTimer && this.liveSyncTimerDueAt > 0 && this.liveSyncTimerDueAt <= dueAt) {
      return;
    }
    if (this.liveSyncTimer) {
      clearTimeout(this.liveSyncTimer);
    }
    this.liveSyncTimerDueAt = dueAt;
    this.liveSyncTimer = setTimeout(() => {
      this.liveSyncTimer = undefined;
      this.liveSyncTimerDueAt = 0;
      void this.flushEditorChanges().catch((err) => {
        this.reportEditorLiveSyncError(err);
      });
    }, normalizedDelayMs);
  }

  private pendingEditorFlushDelayMs(now = Date.now()): number | undefined {
    let earliestSuppressedUntil: number | undefined;
    for (const filePath of this.pendingEditorPaths) {
      const suppressedUntil = this.editorLiveSyncSuppressedUntil(filePath, now);
      if (suppressedUntil <= now) {
        return 0;
      }
      earliestSuppressedUntil = earliestSuppressedUntil === undefined
        ? suppressedUntil
        : Math.min(earliestSuppressedUntil, suppressedUntil);
    }
    if (earliestSuppressedUntil === undefined) {
      return undefined;
    }
    return Math.max(MIN_STUDIO_LIVE_SYNC_POLL_MS, earliestSuppressedUntil - now);
  }

  private schedulePendingEditorFlushIfNeeded(now = Date.now()): void {
    const delayMs = this.pendingEditorFlushDelayMs(now);
    if (delayMs !== undefined) {
      this.scheduleEditorLiveSyncFlush(delayMs);
    }
  }

  public noteProgrammaticEditorWrite(request: ProgrammaticEditorWriteRequest): void {
    const cfg = this.getConfig();
    const srcRoot = path.join(cfg.projectRoot, "src");
    const rawPaths = Array.isArray(request.paths)
      ? request.paths
      : request.paths !== undefined
        ? [request.paths]
        : [];
    const paths = [...new Set(rawPaths
      .map((value) => String(value ?? "").trim())
      .filter((value) => value.length > 0)
      .map((value) => path.isAbsolute(value) ? path.resolve(value) : path.resolve(cfg.projectRoot, value))
      .filter((value) => this.isPathInside(value, srcRoot)))];
    if (paths.length === 0) {
      return;
    }

    const now = Date.now();
    const durationMs = Math.max(250, Math.min(10_000, Number(request.durationMs ?? 2500) || 2500));
    const untilMs = now + durationMs;
    this.clearExpiredSuppressedEditorLiveSyncPaths(now);

    const cachePaths: string[] = [];
    for (const filePath of paths) {
      this.suppressedEditorLiveSyncPathUntilByKey.set(this.normalizePathForCompare(filePath), untilMs);
      if (request.refreshCache === true && fs.existsSync(filePath)) {
        cachePaths.push(filePath);
      }
    }

    if (cachePaths.length > 0) {
      this.updateEditorLiveSyncCacheAfterPush(cachePaths, cfg);
    }
    this.schedulePendingEditorFlushIfNeeded(now);
  }

  private updateEditorLiveSyncCacheAfterPush(paths: string[], cfg: SyncConfig): void {
    const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
    for (const filePath of paths) {
      const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
      const hash = this.editorLiveSyncFileHash(filePath);
      if (hash) {
        cache.files[key] = hash;
      } else {
        delete cache.files[key];
      }
    }
    this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
  }

  private async suppressStudioLiveSyncAfterEditorPush(paths: string[], cfg: SyncConfig): Promise<void> {
    if (!cfg.studioLiveSyncEnabled || !cfg.editorLiveSyncEnabled || !this.liveSyncWatcher) {
      return;
    }

    const services = [...new Set(
      paths
        .map((filePath) => this.detectServiceForPath(filePath, cfg.projectRoot, cfg.services))
        .filter((service): service is string => typeof service === "string" && service.length > 0),
    )];
    if (services.length === 0) {
      return;
    }

    this.scheduleStudioLiveSyncPoll(cfg, this.resetStudioLiveSyncPollDelay(cfg));
  }

  private replaceEditorLiveSyncCacheForServices(services: string[], cfg: SyncConfig): void {
    const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
    const srcRoot = path.join(cfg.projectRoot, "src");
    const selectedServices = this.normalizeServices(services, cfg.services);
    const serviceDirs = selectedServices.map((service) => path.join(srcRoot, service));
    const currentHashes: Record<string, string> = {};

    for (const serviceDir of serviceDirs) {
      if (!fs.existsSync(serviceDir)) {
        continue;
      }
      for (const filePath of this.collectInitialEditorLiveSyncPaths(serviceDir)) {
        const hash = this.editorLiveSyncFileHash(filePath);
        if (hash) {
          currentHashes[this.editorLiveSyncCacheKey(filePath, cfg.projectRoot)] = hash;
        }
      }
    }

    for (const cachedKey of Object.keys(cache.files)) {
      const absolutePath = path.join(cfg.projectRoot, cachedKey);
      if (serviceDirs.some((serviceDir) => this.isPathInside(absolutePath, serviceDir))) {
        delete cache.files[cachedKey];
      }
    }

    for (const [key, hash] of Object.entries(currentHashes)) {
      cache.files[key] = hash;
    }
    this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
  }

  public async stopLiveSync(options: { silent?: boolean } = {}): Promise<void> {
    this.liveSyncStopRequested = true;
    const wasRunning = this.liveSyncWatcher !== undefined || this.liveSyncStartPromise !== undefined || this.editorLiveSyncRuntimeEnabled;
    const startup = this.liveSyncStartPromise;
    this.disposeLiveSyncRuntime();
    await this.setEditorLiveSyncEnabled(false);
    if (startup) {
      try {
        await startup;
      } catch {
      }
      this.disposeLiveSyncRuntime();
      await this.setEditorLiveSyncEnabled(false);
    }
    if (this.liveSyncOwnsServe) {
      this.bridgeServeRequested = false;
      this.liveSyncOwnsServe = false;
      this.stopBridgeDaemon();
    } else if (!this.bridgeServeRequested) {
      this.stopBridgeDaemon();
    }
    this.updateStatusBar();
    if (!options.silent) {
      vscode.window.showInformationMessage(wasRunning
        ? "Renium: editor -> Studio live sync stopped."
        : "Renium: live sync is not running.");
    }
  }

  public async pushEditorPathsNow(paths: string[] | string, options: EditorPushOptions = {}): Promise<boolean> {
    const changedPaths = (Array.isArray(paths) ? paths : [paths])
      .map((value) => String(value))
      .filter((value) => value.length > 0);
    if (changedPaths.length === 0) {
      return false;
    }

    const cfg = this.getConfig();
    if (!options.force && !this.isEditorLiveSyncActive()) {
      this.disposeLiveSyncRuntime();
      this.updateStatusBar();
      return false;
    }
    if (options.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      return false;
    }

    let pushed = false;
    await this.enqueue(options.taskName ?? "Editor -> Studio sync", async () => {
      const runCfg = this.getConfig();
      if (!options.force && !this.isEditorLiveSyncActive()) {
        this.output.appendLine("[renium] editor direct sync cancelled: editor -> Studio live sync is off");
        return;
      }
      if (options.force === true && !this.canUseStudioPushPipeline()) {
        this.noteStudioPushSkipped("serve/live sync is not active");
        return;
      }
      const pathsToPush = options.skipChangeFilter === true
        ? changedPaths
        : this.filterEditorLiveSyncChangedPaths(changedPaths, runCfg);
      if (pathsToPush.length === 0) {
        return;
      }
      this.logEditorChangedPaths("Editor -> Studio", pathsToPush, runCfg);
      await this.runEditorPush(pathsToPush, runCfg, options);
      pushed = true;
    });
    return pushed;
  }

  public async pushEditorPropertyNow(request: EditorPropertyPushRequest): Promise<void> {
    if (!request.force && !this.isEditorLiveSyncActive()) {
      return;
    }
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      return;
    }
    const cfg = this.getConfig();

    const service = String(request.service ?? "").trim();
    const property = String(request.property ?? "").trim();
    const pathSegments = Array.isArray(request.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || !property || pathSegments.length === 0) {
      throw new Error("Editor property push requires service, property, and path segments.");
    }

    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
    const args = [
      "prop",
      "-w",
      String(bridgeWaitSeconds),
      "-P",
      cfg.bridgePorts,
      "-s",
      service,
      "-c",
      String(request.className ?? ""),
      "-p",
      JSON.stringify(pathSegments),
      "-o",
      JSON.stringify(Array.isArray(request.pathOrdinals) ? request.pathOrdinals : []),
      "-S",
      request.scope ?? "property",
      "-n",
      property,
      `--value-json=${JSON.stringify(request.value ?? null)}`,
    ];
    const allowProtectedMeshIdApply = request.allowProtectedMeshIdApply === true
      || (property === "MeshId" && request.className === "MeshPart");
    if (allowProtectedMeshIdApply) {
      args.push("-m");
    }
    const settingsId = String(request.settingsId ?? "").trim();
    if (settingsId.length > 0) {
      args.push("-i", settingsId);
    }

    const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
    let result: CommandRunResult;
    if (usePersistentBridge) {
      result = await this.runDaemonCommand(
        command,
        args.slice(1),
        cfg,
        "editor-property",
        "prop",
        { quietWait: true },
      );
    } else {
      result = await this.runCommand(
        command,
        args,
        cfg.projectRoot,
        "editor-property",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
    }
    if (result.code !== 0) {
      throw new Error(`Editor property push exited with code ${result.code}`);
    }
    const summary = this.parseEditorPushSummary(result.output);
    if (!summary) {
      throw new Error("Editor property push did not return a Studio apply result.");
    }
    const errors = this.summaryNumber(summary, "errors");
    if (summary.ok === false || errors > 0) {
      throw new Error("Studio rejected or failed editor property apply.");
    }

    const settingsFile = String(request.settingsFile ?? "").trim();
    if (settingsFile.length > 0 && fs.existsSync(settingsFile)) {
      this.updateEditorLiveSyncCacheAfterPush([settingsFile], cfg);
      await this.suppressStudioLiveSyncAfterEditorPush([settingsFile], cfg);
    }
  }

  public async pushEditorDeleteNow(request: EditorDeletePushRequest): Promise<void> {
    if (!request.force && !this.isEditorLiveSyncActive()) {
      return;
    }
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      return;
    }
    const cfg = this.getConfig();

    const service = String(request.service ?? "").trim();
    const pathSegments = Array.isArray(request.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length <= 1) {
      throw new Error("Editor delete push requires service and a non-root path.");
    }

    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
    const args = [
      "del",
      "-w",
      String(bridgeWaitSeconds),
      "-P",
      cfg.bridgePorts,
      "-s",
      service,
      "-c",
      String(request.className ?? ""),
      "-p",
      JSON.stringify(pathSegments),
      "-o",
      JSON.stringify(Array.isArray(request.pathOrdinals) ? request.pathOrdinals : []),
    ];
    const settingsId = String(request.settingsId ?? "").trim();
    if (settingsId.length > 0) {
      args.push("-i", settingsId);
    }

    const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
    const result = usePersistentBridge
      ? await this.runDaemonCommand(
        command,
        args.slice(1),
        cfg,
        "editor-delete",
        "del",
        { quietWait: true },
      )
      : await this.runCommand(
        command,
        args,
        cfg.projectRoot,
        "editor-delete",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
    if (result.code !== 0) {
      throw new Error(`Editor delete push exited with code ${result.code}`);
    }
    const summary = this.parseEditorPushSummary(result.output);
    if (!summary) {
      throw new Error("Editor delete push did not return a Studio apply result.");
    }
    const errors = this.summaryNumber(summary, "errors");
    if (summary.ok === false || errors > 0) {
      throw new Error("Studio rejected or failed editor delete apply.");
    }

    const settingsFile = String(request.settingsFile ?? "").trim();
    if (settingsFile.length > 0 && fs.existsSync(settingsFile)) {
      this.updateEditorLiveSyncCacheAfterPush([settingsFile], cfg);
      await this.suppressStudioLiveSyncAfterEditorPush([settingsFile], cfg);
    }
  }

  public async onDocumentSaved(doc: vscode.TextDocument): Promise<void> {
    if (doc.isUntitled || doc.uri.scheme !== "file") {
      return;
    }

    const cfg = this.getConfig();
    if (!cfg.editorLiveSyncEnabled) {
      this.disposeLiveSyncRuntime();
      this.updateStatusBar();
    }

    if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && this.isPathInside(doc.uri.fsPath, path.join(cfg.projectRoot, "src"))) {
      const fileKey = this.normalizePathForCompare(doc.uri.fsPath);
      this.recentDirectSaveAtByPath.set(fileKey, Date.now());
      this.forcedEditorLiveSyncPathKeys.add(fileKey);
      this.pendingEditorPaths.add(doc.uri.fsPath);
      if (this.liveSyncTimer) {
        clearTimeout(this.liveSyncTimer);
        this.liveSyncTimer = undefined;
        this.liveSyncTimerDueAt = 0;
      }
      void this.flushEditorChanges().catch((err) => {
        this.reportEditorLiveSyncError(err);
      });
      return;
    }

    if (!cfg.autoSyncOnSave) {
      return;
    }

    const service = this.detectServiceForPath(doc.uri.fsPath, cfg.projectRoot, cfg.services);
    if (service) {
      this.pendingAutoServices.add(service);
    } else {
      cfg.services.forEach((s) => this.pendingAutoServices.add(s));
    }

    if (this.autoSyncTimer) {
      clearTimeout(this.autoSyncTimer);
    }

    this.autoSyncTimer = setTimeout(() => {
      const services = Array.from(this.pendingAutoServices);
      this.pendingAutoServices.clear();

      void this.enqueue("Auto sync on save", async () => {
        await this.runExport({
          services,
          runImport: cfg.runImport,
          notifyOnSuccess: false,
          reason: "",
        });
      }).catch(() => undefined);
    }, Math.max(100, cfg.autoSyncDebounceMs));
  }

  private queueEditorChange(filePath: string, immediate = false): void {
    const cfg = this.getConfig();
    if (!cfg.editorLiveSyncEnabled) {
      this.disposeLiveSyncRuntime();
      this.updateStatusBar();
      return;
    }

    const srcRoot = path.join(cfg.projectRoot, "src");
    if (!this.isPathInside(filePath, srcRoot)) {
      return;
    }
    const now = Date.now();
    const suppressedUntil = this.editorLiveSyncSuppressedUntil(filePath, now);
    if (suppressedUntil > now) {
      this.pendingEditorPaths.add(filePath);
      this.scheduleEditorLiveSyncFlush(suppressedUntil - now);
      return;
    }

    if (!immediate) {
      const fileKey = this.normalizePathForCompare(filePath);
      const lastDirectSaveAt = this.recentDirectSaveAtByPath.get(fileKey) ?? 0;
      if (now - lastDirectSaveAt < 1000) {
        return;
      }
      if (this.recentDirectSaveAtByPath.size > 256) {
        this.recentDirectSaveAtByPath.clear();
      }
    }

    this.pendingEditorPaths.add(filePath);
    const liveSyncDelayMs = immediate ? 0 : Math.max(50, Math.min(100, cfg.autoSyncDebounceMs));
    this.scheduleEditorLiveSyncFlush(liveSyncDelayMs);
  }

  private reportEditorLiveSyncError(err: unknown): void {
    const message = err instanceof Error ? err.message : String(err);
    this.output.appendLine(`[renium] editor live sync failed: ${message}`);
    if (this.editorPushFailureStreak <= 1) {
      this.output.show(true);
      vscode.window.showErrorMessage(`Renium: editor live sync failed. ${message}`);
    }
  }

  private async flushEditorChanges(): Promise<void> {
    const cfg = this.getConfig();
    if (!cfg.editorLiveSyncEnabled) {
      this.pendingEditorPaths.clear();
      return;
    }

    const now = Date.now();
    const queuedPaths: string[] = [];
    const queuedPathKeys: string[] = [];
    let earliestSuppressedUntil: number | undefined;
    for (const filePath of this.pendingEditorPaths) {
      const fileKey = this.normalizePathForCompare(filePath);
      const forceEditorPush = this.forcedEditorLiveSyncPathKeys.has(fileKey);
      const suppressedUntil = this.editorLiveSyncSuppressedUntil(filePath, now);
      if (!forceEditorPush && suppressedUntil > now) {
        earliestSuppressedUntil = earliestSuppressedUntil === undefined
          ? suppressedUntil
          : Math.min(earliestSuppressedUntil, suppressedUntil);
        continue;
      }
      queuedPaths.push(filePath);
      queuedPathKeys.push(fileKey);
      this.pendingEditorPaths.delete(filePath);
    }
    if (earliestSuppressedUntil !== undefined) {
      this.scheduleEditorLiveSyncFlush(earliestSuppressedUntil - now);
    }
    if (queuedPaths.length === 0) {
      return;
    }
    const changedPaths = this.filterEditorLiveSyncChangedPaths(queuedPaths, cfg);
    if (changedPaths.length === 0) {
      for (const fileKey of queuedPathKeys) {
        this.forcedEditorLiveSyncPathKeys.delete(fileKey);
      }
      return;
    }

    try {
      await this.enqueue("Editor -> Studio sync", async () => {
        this.logEditorChangedPaths("Editor -> Studio", changedPaths, cfg);
        await this.runEditorPush(changedPaths, cfg);
        this.refreshSyncBasesForPaths(changedPaths, cfg);
      });
      if (this.editorPushFailureStreak > 0) {
        this.output.appendLine(
          `[renium] editor live sync recovered after ${this.editorPushFailureStreak} failed attempt(s).`,
        );
      }
      this.editorPushFailureStreak = 0;
    } catch (err) {
      this.editorPushFailureStreak += 1;
      const retryDelayMs = Math.min(
        EDITOR_PUSH_RETRY_BASE_MS * 2 ** Math.min(this.editorPushFailureStreak - 1, 8),
        EDITOR_PUSH_RETRY_MAX_MS,
      );
      for (const filePath of changedPaths) {
        this.pendingEditorPaths.add(filePath);
      }
      this.scheduleEditorLiveSyncFlush(retryDelayMs);
      throw err;
    } finally {
      for (const fileKey of queuedPathKeys) {
        this.forcedEditorLiveSyncPathKeys.delete(fileKey);
      }
    }
  }

  private async runEditorPush(changedPaths: string[], cfg: SyncConfig, options: EditorPushOptions = {}): Promise<void> {
    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
    const args = [
      "push",
      "-r",
      cfg.projectRoot,
      "-d",
      "src",
      "-w",
      String(bridgeWaitSeconds),
      "-P",
      cfg.bridgePorts,
    ];
    const verifySources = options.verifySources === true || cfg.verifyEditorPushSources;
    if (verifySources) {
      args.push("-v");
    }
    if (cfg.linkSync.cacheDir.length > 0) {
      args.push("--link-cache-dir", cfg.linkSync.cacheDir);
    }
    if (this.effectiveLiveSyncConfig(cfg).overridePackages) {
      args.push("--override-packages");
    }
    let changedPathsFile: string | undefined;
    const changedPathArgs = changedPaths.map((changedPath) => this.editorChangedPathArg(changedPath, cfg.projectRoot));
    if (changedPathArgs.length > 32) {
      const listDir = path.join(cfg.projectRoot, ".renium", "editor-push-paths");
      fs.mkdirSync(listDir, { recursive: true });
      changedPathsFile = path.join(
        listDir,
        `paths-${Date.now()}-${Math.random().toString(16).slice(2)}.txt`,
      );
      fs.writeFileSync(changedPathsFile, `${changedPathArgs.join(os.EOL)}${os.EOL}`, "utf8");
      args.push("-f", changedPathsFile);
    } else {
      for (const changedPath of changedPathArgs) {
        args.push("-p", changedPath);
      }
    }
    const targetSettingsId = typeof options.targetSettingsId === "string" ? options.targetSettingsId.trim() : "";
    let targetIdsFile: string | undefined;
    const targetSettingsIds = [
      ...(targetSettingsId.length > 0 ? [targetSettingsId] : []),
      ...(Array.isArray(options.targetSettingsIds) ? options.targetSettingsIds : []),
    ]
      .map((value) => String(value).trim())
      .filter((value) => value.length > 0);
    const uniqueTargetSettingsIds = [...new Set(targetSettingsIds)];
    if (uniqueTargetSettingsIds.length > 128) {
      const listDir = path.join(cfg.projectRoot, ".renium", "editor-push-paths");
      fs.mkdirSync(listDir, { recursive: true });
      targetIdsFile = path.join(
        listDir,
        `target-settings-${Date.now()}-${Math.random().toString(16).slice(2)}.txt`,
      );
      fs.writeFileSync(targetIdsFile, `${uniqueTargetSettingsIds.join(os.EOL)}${os.EOL}`, "utf8");
      args.push("-I", targetIdsFile);
    } else {
      for (const targetId of uniqueTargetSettingsIds) {
        args.push("-i", targetId);
      }
    }
    const targetProperties = [
      ...(typeof options.targetProperty === "string" ? [options.targetProperty] : []),
      ...(Array.isArray(options.targetProperties) ? options.targetProperties : []),
    ]
      .map((value) => String(value).trim())
      .filter((value) => value.length > 0);
    for (const targetProperty of [...new Set(targetProperties)]) {
      args.push("-t", targetProperty);
    }
    if (options.upsertInstancesOnly === true) {
      args.push("-u");
    }

    const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);

    try {
      const maxAttempts = 2;
      let result: CommandRunResult | undefined;
      for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
        if (usePersistentBridge) {
          result = await this.runDaemonCommand(
            command,
            args.slice(1),
            cfg,
            attempt === 1 ? "editor-push" : `editor-push-retry-${attempt}`,
            "push",
            { quietWait: true },
          );
        } else {
          result = await this.runCommand(
            command,
            args,
            cfg.projectRoot,
            attempt === 1 ? "editor-push" : `editor-push-retry-${attempt}`,
            cfg.progressHeartbeatSeconds,
            { quietLog: true },
          );
        }
        if (result.code === 0) {
          break;
        }
        if (attempt >= maxAttempts || !isTransientBridgeFailure(result.output)) {
          break;
        }
        await sleep(250);
      }

      if (!result || result.code !== 0) {
        throw new Error(`Editor push exited with code ${result?.code ?? "unknown"}`);
      }
      const summary = this.parseEditorPushSummary(result.output);
      if (!summary) {
        throw new Error("Editor push did not return a Studio apply result.");
      }
      const sourceVerified = this.summaryNumber(summary, "sourceVerified");
      const sourceVerifyFailed = this.summaryNumber(summary, "sourceVerifyFailed");
      const errors = this.summaryNumber(summary, "errors");
      const sourceQueued = this.summaryNumber(summary, "sourceQueued");
      const sourceUpdated = this.summaryNumber(summary, "sourceUpdated");
      const noops = this.summaryNumber(summary, "noops");
      void sourceQueued;
      void sourceUpdated;
      void noops;
      if (errors > 0) {
        const detail = Array.isArray(summary.sourceVerifyErrors) ? ` ${summary.sourceVerifyErrors.join("; ")}` : "";
        throw new Error(`Studio rejected or failed editor Source verification.${detail}`);
      }
      if (summary.ok === false || sourceVerifyFailed > 0) {
        const detail = Array.isArray(summary.sourceVerifyErrors) ? ` ${summary.sourceVerifyErrors.join("; ")}` : "";
        this.output.appendLine(`[renium] editor push verification warning:${detail || " Studio reported a source verification mismatch after apply."}`);
      }
      if (summary.ok !== false && errors === 0 && sourceVerifyFailed === 0) {
        this.updateEditorLiveSyncCacheAfterPush(changedPaths, cfg);
        await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);
      }

      const existingSourceSaves = changedPaths.filter((changedPath) => this.isLuaSourcePath(changedPath) && fs.existsSync(changedPath)).length;
      if (verifySources && existingSourceSaves > 0 && sourceVerified < existingSourceSaves) {
        this.output.appendLine(
          `[renium] editor push verification warning: verified ${sourceVerified}/${existingSourceSaves} saved Lua source file(s).`,
        );
      }
    } finally {
      if (changedPathsFile) {
        try {
          fs.unlinkSync(changedPathsFile);
        } catch {
        }
      }
      if (targetIdsFile) {
        try {
          fs.unlinkSync(targetIdsFile);
        } catch {
        }
      }
    }
  }

  private parseEditorPushSummary(output: string): Record<string, unknown> | undefined {
    const prefix = "__ROBLOX_SYNC_EDITOR_PUSH_RESULT__ ";
    let found: Record<string, unknown> | undefined;
    for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
      const line = rawLine.trim();
      const index = line.indexOf(prefix);
      if (index < 0) {
        continue;
      }
      try {
        const parsed = JSON.parse(line.slice(index + prefix.length)) as unknown;
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
          found = parsed as Record<string, unknown>;
        }
      } catch {
      }
    }
    if (!found) {
      try {
        const parsed = JSON.parse(output.trim()) as unknown;
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
          return parsed as Record<string, unknown>;
        }
      } catch {
      }
    }
    return found;
  }

  private parseCliJsonObject<T extends object>(output: string): T | undefined {
    const lines = output.replace(/\r\n/g, "\n").split("\n");
    for (let index = lines.length - 1; index >= 0; index -= 1) {
      const line = lines[index].trim();
      if (!line) {
        continue;
      }
      try {
        const parsed = JSON.parse(line) as unknown;
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
          return parsed as T;
        }
      } catch {
      }
    }

    const trimmed = output.trim();
    if (!trimmed) {
      return undefined;
    }
    try {
      const parsed = JSON.parse(trimmed) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        return parsed as T;
      }
    } catch {
    }
    return undefined;
  }

  private parseExportGameFileResult(output: string): CliExportGameFileResult | undefined {
    return this.parseCliJsonObject<CliExportGameFileResult>(output);
  }

  private parseStudioChangeState(output: string): StudioChangeState | undefined {
    const prefix = "__ROBLOX_SYNC_STUDIO_CHANGE_STATE__ ";
    const daemonResultPrefix = "__ROBLOX_SYNC_DAEMON_RESULT__ ";
    let found: StudioChangeState | undefined;
    for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
      const line = rawLine.trim();
      const index = line.indexOf(prefix);
      const daemonIndex = line.indexOf(daemonResultPrefix);
      const payload = index >= 0
        ? line.slice(index + prefix.length)
        : daemonIndex >= 0
          ? line.slice(daemonIndex + daemonResultPrefix.length)
          : undefined;
      if (!payload) {
        continue;
      }
      const state = this.parseStudioChangeStatePayload(payload);
      if (state) {
        found = state;
      }
    }
    if (!found) {
      found = this.parseStudioChangeStatePayload(output.trim());
    }
    return found;
  }

  private parseStudioChangeStatePayload(payload: string): StudioChangeState | undefined {
    if (!payload) {
      return undefined;
    }
    try {
      const parsed = JSON.parse(payload) as unknown;
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
        return undefined;
      }
      const record = parsed as Record<string, unknown>;
      const nestedResult = record.result;
      if (nestedResult && typeof nestedResult === "object" && !Array.isArray(nestedResult)) {
        return this.studioChangeStateFromRecord(nestedResult as Record<string, unknown>);
      }
      if (!this.looksLikeStudioChangeStateRecord(record)) {
        return undefined;
      }
      return this.studioChangeStateFromRecord(record);
    } catch {
      return undefined;
    }
  }

  private looksLikeStudioChangeStateRecord(record: Record<string, unknown>): boolean {
    return Array.isArray(record.dirtyServices)
      || Array.isArray(record.fullSyncServices)
      || Array.isArray(record.propertyChanges)
      || Array.isArray(record.changes)
      || typeof record.tracking === "boolean"
      || typeof record.seq === "number"
      || typeof record.trackedServices === "number"
      || typeof record.itemChangedAvailable === "boolean"
      || typeof record.eventDriven === "boolean"
      || typeof record.waitTimedOut === "boolean";
  }

  private studioChangeStateFromRecord(record: Record<string, unknown>): StudioChangeState {
    return {
      ok: typeof record.ok === "boolean" ? record.ok : undefined,
      tracking: typeof record.tracking === "boolean" ? record.tracking : undefined,
      role: typeof record.role === "string" ? record.role : undefined,
      seq: typeof record.seq === "number" ? record.seq : undefined,
      dirtyServices: Array.isArray(record.dirtyServices)
        ? record.dirtyServices.map((value) => String(value))
        : undefined,
      fullSyncServices: Array.isArray(record.fullSyncServices)
        ? record.fullSyncServices.map((value) => String(value))
        : undefined,
      propertyChanges: Array.isArray(record.propertyChanges)
        ? record.propertyChanges
          .filter((value): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value))
          .map((value) => ({
            service: typeof value.service === "string" ? value.service : undefined,
            settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
            className: typeof value.className === "string" ? value.className : undefined,
            pathSegments: Array.isArray(value.pathSegments) ? value.pathSegments.map((segment) => String(segment)) : undefined,
            pathOrdinals: Array.isArray(value.pathOrdinals)
              ? value.pathOrdinals
                .map((ordinal) => Number(ordinal))
                .filter((ordinal) => Number.isFinite(ordinal))
              : undefined,
            scope: value.scope === "metadata" || value.scope === "attribute" ? value.scope : "property",
            property: typeof value.property === "string" ? value.property : undefined,
            value: value.value,
            seq: typeof value.seq === "number" ? value.seq : undefined,
          }))
        : undefined,
      changes: Array.isArray(record.changes)
        ? record.changes
          .filter((value): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value))
          .map((value) => ({
            service: typeof value.service === "string" ? value.service : undefined,
            action: typeof value.action === "string" ? value.action : undefined,
            reason: typeof value.reason === "string" ? value.reason : undefined,
            className: typeof value.className === "string" ? value.className : undefined,
            path: typeof value.path === "string" ? value.path : undefined,
            pathSegments: Array.isArray(value.pathSegments) ? value.pathSegments.map((segment) => String(segment)) : undefined,
            pathOrdinals: Array.isArray(value.pathOrdinals)
              ? value.pathOrdinals
                .map((ordinal) => Number(ordinal))
                .filter((ordinal) => Number.isFinite(ordinal))
              : undefined,
            property: typeof value.property === "string" ? value.property : undefined,
            attribute: typeof value.attribute === "string" ? value.attribute : undefined,
            direct: typeof value.direct === "boolean" ? value.direct : undefined,
            fullSync: typeof value.fullSync === "boolean" ? value.fullSync : undefined,
            seq: typeof value.seq === "number" ? value.seq : undefined,
          }))
        : undefined,
      trackedServices: typeof record.trackedServices === "number" ? record.trackedServices : undefined,
      itemChangedAvailable: typeof record.itemChangedAvailable === "boolean" ? record.itemChangedAvailable : undefined,
      eventDriven: typeof record.eventDriven === "boolean" ? record.eventDriven : undefined,
      waitSeconds: typeof record.waitSeconds === "number" ? record.waitSeconds : undefined,
      waitTimedOut: typeof record.waitTimedOut === "boolean" ? record.waitTimedOut : undefined,
      twoWaySyncEnabled: typeof record.twoWaySyncEnabled === "boolean" ? record.twoWaySyncEnabled : undefined,
      runtimeSettings: record.runtimeSettings && typeof record.runtimeSettings === "object" && !Array.isArray(record.runtimeSettings)
        ? record.runtimeSettings as Record<string, unknown>
        : undefined,
    };
  }

  private parseBytecodeSetSourceResult(output: string): { sourcePath?: string } | undefined {
    const trimmed = output.trim();
    if (!trimmed) {
      return undefined;
    }
    try {
      const parsed = JSON.parse(trimmed) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const record = parsed as Record<string, unknown>;
        return {
          sourcePath: typeof record.sourcePath === "string" ? record.sourcePath : undefined,
        };
      }
    } catch {
    }
    return undefined;
  }

  private summaryNumber(summary: Record<string, unknown>, key: string): number {
    const value = summary[key];
    return typeof value === "number" && Number.isFinite(value) ? value : 0;
  }

  private isLuaSourcePath(filePath: string): boolean {
    return /\.(lua|luau)$/i.test(filePath);
  }

  public onConfigurationChanged(event?: vscode.ConfigurationChangeEvent): void {
    const cfg = this.getConfig();
    const editorLiveSyncChanged = event?.affectsConfiguration("renium.editorLiveSyncEnabled") === true;
    const bridgeConfigChanged = !event || [
      "renium.exportCliPath",
      "renium.projectRoot",
      "renium.transport",
      "renium.bridgeWaitSeconds",
      "renium.bridgePorts",
    ].some((key) => event.affectsConfiguration(key));
    const persistentBridgeChanged = event?.affectsConfiguration("renium.usePersistentBridge") === true;

    if (bridgeConfigChanged || (persistentBridgeChanged && !this.bridgeServeRequested)) {
      this.stopBridgeDaemon();
      if (this.bridgeServeRequested) {
        void this.serve({ silent: true, bestEffort: true });
      } else if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && this.shouldUsePersistentBridge(cfg)) {
        void this.prewarmPersistentBridgeDaemon("configuration");
      }
    }
    if (!cfg.editorLiveSyncEnabled && this.liveSyncWatcher) {
      this.disposeLiveSyncRuntime();
      if (!this.bridgeServeRequested) {
        this.stopBridgeDaemon();
      }
    }
    if (editorLiveSyncChanged && cfg.editorLiveSyncEnabled && !this.liveSyncWatcher && !this.liveSyncStartPromise) {
      void this.startLiveSync({ silent: true, bestEffort: true });
    }
    if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && !this.liveSyncStartupInProgress) {
      if (cfg.studioLiveSyncEnabled) {
        void this.startStudioLiveSyncRuntime(cfg, { bestEffort: true });
      } else {
        this.stopStudioLiveSyncRuntime();
      }
    }
    if (!event || event.affectsConfiguration("renium.gitSync") || event.affectsConfiguration("renium.projectRoot")) {
      void this.refreshGitView();
    }
    this.updateStatusBar();
  }

  public async prewarmPersistentBridgeDaemon(reason = "activation"): Promise<void> {
    const cfg = this.getConfig();
    if (!this.shouldUsePersistentBridge(cfg)) {
      return;
    }
    if (!this.bridgeServeRequested && !(cfg.editorLiveSyncEnabled && this.liveSyncWatcher)) {
      return;
    }
    if (!fs.existsSync(cfg.exportCliPath)) {
      this.output.appendLine(
        `[renium] bridge daemon prewarm skipped (${reason}): export CLI does not exist yet: ${cfg.exportCliPath}`,
      );
      return;
    }

    try {
      await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: this.bridgeServeRequested });
      this.output.appendLine(`[renium] bridge daemon prewarm ready (${reason})`);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] bridge daemon prewarm skipped (${reason}): ${message}`);
    }
  }

  private async toggleAutoSyncOnSave(): Promise<void> {
    const cfg = vscode.workspace.getConfiguration("renium");
    const enabled = cfg.get<boolean>("autoSyncOnSave", false);
    await cfg.update("autoSyncOnSave", !enabled, vscode.ConfigurationTarget.Workspace);

    this.updateStatusBar();
    vscode.window.showInformationMessage(`Renium: auto sync on save ${!enabled ? "enabled" : "disabled"}.`);
  }

  private async enqueue(taskName: string, task: () => Promise<void>): Promise<void> {
    const run = async (): Promise<void> => {
      try {
        this.setActiveTask(taskName);
        await task();
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] task failed: ${taskName}: ${message}`);
        this.output.show(true);
        vscode.window.showErrorMessage(`Renium: ${taskName} failed. ${message}`);
        throw err;
      } finally {
        this.setActiveTask(undefined);
      }
    };

    const queued = this.queue.catch(() => undefined).then(run);
    this.queue = queued.catch(() => undefined);
    await queued;
  }

  private async runExport(options: {
    services: string[];
    runImport: boolean;
    notifyOnSuccess: boolean;
    reason: string;
    quietTimings?: boolean;
    quietLog?: boolean;
    configOverrides?: Partial<Pick<SyncConfig, "modifiedDefaultBypass">>;
  }): Promise<CommandRunResult> {
    const cfg = {
      ...this.getConfig(),
      ...(options.configOverrides ?? {}),
    };
    const selectedServices = this.normalizeServices(options.services, cfg.services);
    const useRustImportInExporter = options.runImport;
    const { command, args } = this.resolveExportCommand(
      cfg,
      selectedServices,
      options.runImport,
      useRustImportInExporter,
      options.quietTimings !== false,
    );
    const usePersistentBridge = this.shouldUsePersistentBridge(cfg);

    const quietLog = options.quietLog === true;
    if (!quietLog) {
      this.output.show(false);
      this.logResolvedConfig(cfg);
      if (usePersistentBridge) {
        this.output.appendLine(
          `[renium] export daemon command: ${command} bd -w ${Math.max(1, cfg.bridgeWaitSeconds)} -P ${cfg.bridgePorts}`,
        );
        this.output.appendLine(`[renium] export daemon request: x ${this.renderArgs(args.slice(1))}`);
      } else {
        this.output.appendLine(`[renium] export command: ${command} ${this.renderArgs(args)}`);
      }
    }

    const maxAttempts = 3;
    let result: CommandRunResult | undefined;
    for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
      if (usePersistentBridge) {
        result = await this.runDaemonExport(
          command,
          args.slice(1),
          cfg,
          attempt === 1 ? "export" : `export-retry-${attempt}`,
          { quietWait: quietLog },
        );
      } else {
        result = await this.runCommand(
          command,
          args,
          cfg.projectRoot,
          attempt === 1 ? "export" : `export-retry-${attempt}`,
          cfg.progressHeartbeatSeconds,
          { quietLog },
        );
      }
      if (result.code === 0) {
        break;
      }
      if (attempt >= maxAttempts || !isTransientBridgeFailure(result.output)) {
        break;
      }
      const retryDelayMs = attempt === 1 ? 250 : 500;
      if (!quietLog) {
        this.output.appendLine(
          `[renium] export: transient bridge failure; retrying attempt ${attempt + 1}/${maxAttempts} after ${retryDelayMs}ms`,
        );
      }
      await sleep(retryDelayMs);
    }
    if (!result || result.code !== 0) {
      throw new Error(`Export exited with code ${result?.code ?? "unknown"}`);
    }

    if (options.runImport && options.notifyOnSuccess) {
      try {
        await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", selectedServices);
      } catch {
      }
    }

    if (options.notifyOnSuccess && options.reason) {
      vscode.window.showInformationMessage(`Renium: ${options.reason}.`);
    }
    return result;
  }

  private daemonKey(command: string, cfg: SyncConfig, serve: boolean): string {
    let binaryMtimeMs = 0;
    try {
      binaryMtimeMs = Math.floor(fs.statSync(command).mtimeMs);
    } catch {
      binaryMtimeMs = 0;
    }

    return JSON.stringify({
      command,
      binaryMtimeMs,
      projectRoot: cfg.projectRoot,
      bridgePorts: cfg.bridgePorts,
      bridgeWaitSeconds: Math.max(1, cfg.bridgeWaitSeconds),
      serve,
    });
  }

  private async runDaemonExport(
    command: string,
    args: string[],
    cfg: SyncConfig,
    label: string,
    options: { quietWait?: boolean } = {},
  ): Promise<CommandRunResult> {
    return await this.runDaemonCommand(command, args, cfg, label, "x", options);
  }

  private async runDaemonCommand(
    command: string,
    args: string[],
    cfg: SyncConfig,
    label: string,
    daemonCommand: string,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    await this.ensureBridgeDaemon(command, cfg, { serve: this.bridgeServeRequested });

    return await new Promise<CommandRunResult>((resolve, reject) => {
      const proc = this.daemonProcess;
      if (!proc || proc.killed || !proc.stdin?.writable) {
        reject(new Error("Persistent bridge daemon is not running."));
        return;
      }

      const launchedAt = Date.now();
      const id = this.daemonRequestId++;
      const pending: DaemonPendingRequest = {
        id,
        label,
        launchedAt,
        lastOutputAt: launchedAt,
        sawOutput: false,
        output: "",
        resolve,
        reject,
        heartbeatTimer: undefined,
        timeoutTimer: undefined,
        quiet: options.quietWait === true,
      };

      if (!options.quietWait) {
        const heartbeatMs = Math.max(2, Math.round(cfg.progressHeartbeatSeconds)) * 1000;
        pending.heartbeatTimer = setInterval(() => {
          const now = Date.now();
          const elapsedSec = ((now - launchedAt) / 1000).toFixed(1);
          const idleSec = ((now - pending.lastOutputAt) / 1000).toFixed(1);
          if (!pending.sawOutput) {
            this.output.appendLine(`[renium] ${label}: waiting for daemon output (${elapsedSec}s elapsed)`);
          } else {
            this.output.appendLine(`[renium] ${label}: daemon still running (${elapsedSec}s elapsed, idle ${idleSec}s)`);
          }
        }, heartbeatMs);
      }

      const timeoutMs = this.daemonRequestTimeoutMs(cfg, daemonCommand, options.timeoutMs);
      pending.timeoutTimer = setTimeout(() => {
        if (!this.daemonPending.has(id)) {
          return;
        }
        const timeoutMessage = `[renium] ${label}: daemon request timed out after ${Math.round(timeoutMs / 1000)}s; restarting the bridge daemon.\n`;
        this.output.appendLine(timeoutMessage.trim());
        this.finishDaemonRequest(id, { code: 124, output: pending.output + `\n${timeoutMessage}` });
        this.stopBridgeDaemon(new Error(`Persistent bridge daemon request timed out (${label}).`));
      }, timeoutMs);

      this.daemonPending.set(id, pending);
      const request = JSON.stringify({
        id,
        command: daemonCommand,
        args,
      }) + "\n";

      proc.stdin.write(request, "utf8", (err) => {
        if (err) {
          this.finishDaemonRequest(id, {
            code: 1,
            output: pending.output + `\n[renium] daemon request write failed: ${err.message}`,
          });
        }
      });
    });
  }

  private shouldUsePersistentBridge(cfg: SyncConfig): boolean {
    return cfg.transport === "ws" && (cfg.usePersistentBridge || this.bridgeServeRequested);
  }

  private shouldUsePersistentBridgeForEditorPush(cfg: SyncConfig): boolean {
    return cfg.transport === "ws" && (
      this.bridgeServeRequested ||
      (cfg.editorLiveSyncEnabled && this.liveSyncWatcher !== undefined) ||
      (cfg.usePersistentBridge && this.isBridgeDaemonRunning())
    );
  }

  private editorBridgeWaitSeconds(cfg: SyncConfig): number {
    return Math.max(1, Math.min(2, Number(cfg.bridgeWaitSeconds) || 2));
  }

  private daemonRequestTimeoutMs(cfg: SyncConfig, daemonCommand: string, requestedTimeoutMs?: number): number {
    if (daemonCommand === "wait-for-channels") {
      const bridgeWaitMs = (Math.max(1, Number(cfg.bridgeWaitSeconds) || 1) + 3) * 1000;
      return Math.max(5_000, Math.min(DAEMON_CHANNEL_WAIT_MAX_MS, bridgeWaitMs));
    }
    return Math.max(
      1_000,
      Math.min(MAX_COMMAND_TIMEOUT_MS, Math.floor(Number(requestedTimeoutMs) || DEFAULT_COMMAND_TIMEOUT_MS)),
    );
  }

  private isBridgeDaemonRunning(): boolean {
    return !!this.daemonProcess && !this.daemonProcess.killed;
  }

  private async ensureBridgeDaemon(command: string, cfg: SyncConfig, options: { serve?: boolean } = {}): Promise<void> {
    this.ensureFileExists(command);
    const key = this.daemonKey(command, cfg, options.serve === true);
    if (this.daemonProcess && !this.daemonProcess.killed && this.daemonKeyValue === key) {
      await this.awaitBridgeDaemonReady(cfg);
      return;
    }

    this.stopBridgeDaemon();

    const args = [
      "bd",
      "-w",
      String(Math.max(1, cfg.bridgeWaitSeconds)),
      "-P",
      cfg.bridgePorts,
      "--parent-pid",
      String(process.pid),
      options.serve ? "-s" : "",
    ].filter((value) => value.length > 0);

    const child = childProcess.spawn(command, args, {
      cwd: cfg.projectRoot,
      env: process.env,
      shell: false,
      stdio: "pipe",
      windowsHide: true,
    });

    this.daemonProcess = child;
    this.daemonKeyValue = key;
    this.daemonOutputBuffer = "";
    this.daemonReady = false;
    this.daemonReadyPromise = new Promise<void>((resolve, reject) => {
      this.daemonReadyResolve = resolve;
      this.daemonReadyReject = reject;
    });
    child.stdout.on("data", (data: Buffer | string) => {
      this.handleDaemonOutput(command, data, false);
    });
    child.stderr.on("data", (data: Buffer | string) => {
      this.handleDaemonOutput(`${command}:err`, data, true);
    });
    child.on("error", (err: Error) => {
      this.output.appendLine(`[renium] bridge daemon error: ${err.message}`);
      this.daemonReadyReject?.(err);
      this.rejectDaemonPending(err);
    });
    child.on("exit", (code: number | null) => {
      const exitError = new Error(`Persistent bridge daemon exited with code ${code ?? 0}`);
      if (code !== 0 && code !== null) {
        this.output.appendLine(`[renium] bridge daemon exited code=${code}`);
      }
      this.daemonReadyReject?.(exitError);
      this.rejectDaemonPending(exitError);
      if (this.daemonProcess === child) {
        this.daemonProcess = undefined;
        this.daemonKeyValue = undefined;
        this.daemonOutputBuffer = "";
        this.daemonReady = false;
        this.daemonReadyPromise = undefined;
        this.daemonReadyResolve = undefined;
        this.daemonReadyReject = undefined;
      }
    });

    await this.awaitBridgeDaemonReady(cfg);
  }

  private async awaitBridgeDaemonReady(cfg: SyncConfig): Promise<void> {
    if (this.daemonReady) {
      return;
    }
    const readyPromise = this.daemonReadyPromise;
    if (!readyPromise) {
      throw new Error("Persistent bridge daemon was not started.");
    }

    const timeoutMs = Math.max(
      1_000,
      Math.min(DAEMON_CHANNEL_WAIT_MAX_MS, (Math.max(1, Number(cfg.bridgeWaitSeconds) || 1) + 2) * 1000),
    );
    let timeoutHandle: NodeJS.Timeout | undefined;
    try {
      await Promise.race([
        readyPromise,
        new Promise<void>((_resolve, reject) => {
          timeoutHandle = setTimeout(() => {
            reject(new Error(`Persistent bridge daemon did not become ready within ${Math.round(timeoutMs / 1000)}s.`));
          }, timeoutMs);
        }),
      ]);
    } catch (err) {
      const error = err instanceof Error ? err : new Error(String(err));
      this.stopBridgeDaemon(error);
      throw error;
    } finally {
      if (timeoutHandle) {
        clearTimeout(timeoutHandle);
      }
    }
  }

  private handleDaemonOutput(prefix: string, data: Buffer | string, isStderr: boolean): void {
    const text = data.toString();
    const hasQuietPending = Array.from(this.daemonPending.values()).some((pending) => pending.quiet);
    if (!hasQuietPending) {
      this.output.append(this.prefixOutput(prefix, data));
    }

    if (isStderr) {
      this.appendDaemonOutputToActiveRequest(text);
      return;
    }

    this.daemonOutputBuffer += text;
    if (this.daemonOutputBuffer.length > MAX_DAEMON_OUTPUT_BUFFER_BYTES) {
      const error = new Error("Persistent bridge daemon emitted more than 1 MiB without a complete protocol line.");
      this.output.appendLine(`[renium] bridge daemon protocol error: ${error.message}`);
      this.stopBridgeDaemon(error);
      return;
    }
    let newlineIndex = this.daemonOutputBuffer.indexOf("\n");
    while (newlineIndex >= 0) {
      const rawLine = this.daemonOutputBuffer.slice(0, newlineIndex + 1);
      const line = this.daemonOutputBuffer.slice(0, newlineIndex).replace(/\r$/, "");
      this.daemonOutputBuffer = this.daemonOutputBuffer.slice(newlineIndex + 1);
      this.appendDaemonOutputToActiveRequest(rawLine);
      this.processDaemonLine(line);
      newlineIndex = this.daemonOutputBuffer.indexOf("\n");
    }
  }

  private appendDaemonOutputToActiveRequest(text: string): void {
    const active = this.daemonPending.values().next().value as DaemonPendingRequest | undefined;
    if (!active) {
      return;
    }
    active.output += text;
    if (active.output.length > 8_000_000) {
      active.output = active.output.slice(-8_000_000);
    }
    active.sawOutput = true;
    active.lastOutputAt = Date.now();
  }

  private processDaemonLine(line: string): void {
    const readyPrefix = "__ROBLOX_SYNC_DAEMON_READY__ ";
    if (line.startsWith(readyPrefix)) {
      this.daemonReady = true;
      this.daemonReadyResolve?.();
      this.daemonReadyResolve = undefined;
      this.daemonReadyReject = undefined;
      return;
    }

    const resultPrefix = "__ROBLOX_SYNC_DAEMON_RESULT__ ";
    if (!line.startsWith(resultPrefix)) {
      return;
    }

    let payload: unknown;
    try {
      payload = JSON.parse(line.slice(resultPrefix.length));
    } catch (err) {
      this.output.appendLine(
        `[renium] bridge daemon: invalid result sentinel: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }

    const record = payload as Record<string, unknown>;
    const id = Number(record.id ?? 0);
    const code = Number(record.code ?? (record.ok ? 0 : 1));
    const pending = this.daemonPending.get(id);
    if (!pending) {
      return;
    }

    let output = pending.output;
    if (code !== 0 && record.error) {
      output += `\n[renium] daemon request error: ${String(record.error)}\n`;
    }
    this.finishDaemonRequest(id, { code, output });
  }

  private finishDaemonRequest(id: number, result: CommandRunResult): void {
    const pending = this.daemonPending.get(id);
    if (!pending) {
      return;
    }

    if (pending.heartbeatTimer) {
      clearInterval(pending.heartbeatTimer);
    }
    if (pending.timeoutTimer) {
      clearTimeout(pending.timeoutTimer);
    }
    this.daemonPending.delete(id);
    const elapsedSec = ((Date.now() - pending.launchedAt) / 1000).toFixed(1);
    if (!pending.quiet) {
      this.output.appendLine(`[renium] ${pending.label}: daemon result code=${result.code} after ${elapsedSec}s`);
    }
    pending.resolve(result);
  }

  private rejectDaemonPending(err: Error): void {
    for (const [id, pending] of this.daemonPending.entries()) {
      if (pending.heartbeatTimer) {
        clearInterval(pending.heartbeatTimer);
      }
      if (pending.timeoutTimer) {
        clearTimeout(pending.timeoutTimer);
      }
      this.daemonPending.delete(id);
      pending.reject(err);
    }
  }

  private sendDaemonShutdown(): void {
    const proc = this.daemonProcess;
    if (!proc || proc.killed || !proc.stdin?.writable) {
      return;
    }

    try {
      const id = this.daemonRequestId++;
      proc.stdin.write(JSON.stringify({ id, command: "shutdown", args: [] }) + "\n", "utf8");
    } catch {
    }
  }

  private stopBridgeDaemon(reason = new Error("Persistent bridge daemon was stopped.")): void {
    const proc = this.daemonProcess;
    if (!proc) {
      this.rejectDaemonPending(reason);
      this.daemonReadyReject?.(reason);
      this.daemonReady = false;
      this.daemonReadyPromise = undefined;
      this.daemonReadyResolve = undefined;
      this.daemonReadyReject = undefined;
      return;
    }

    this.sendDaemonShutdown();
    if (!proc.killed) {
      proc.kill();
    }
    this.daemonReadyReject?.(reason);
    this.rejectDaemonPending(reason);
    this.daemonProcess = undefined;
    this.daemonKeyValue = undefined;
    this.daemonOutputBuffer = "";
    this.daemonReady = false;
    this.daemonReadyPromise = undefined;
    this.daemonReadyResolve = undefined;
    this.daemonReadyReject = undefined;
  }

  private resolveExportCommand(
    cfg: SyncConfig,
    selectedServices: string[],
    requestedRunImport: boolean,
    useRustImportInExporter: boolean,
    quietTimings: boolean,
  ): { command: string; args: string[] } {
    const runImportFlag = requestedRunImport ? "-i" : "--no-import";
    const extraImportArgs: string[] = [];
    if (useRustImportInExporter) {
      const importCliPath = resolveExistingRustCliPath(this.getWorkspaceRoot(), cfg.projectRoot, cfg.rustCliPath);
      this.ensureFileExists(importCliPath);
      extraImportArgs.push("--import-cli", importCliPath);
    }
    this.ensureFileExists(cfg.exportCliPath);
    return {
      command: cfg.exportCliPath,
      args: [
        "x",
        "-r",
        cfg.projectRoot,
        "-d",
        cfg.snapshotDir,
        "-t",
        cfg.transport,
        "-s",
        selectedServices.join(","),
        "--sw",
        String(Math.max(0, cfg.sourceWorkers)),
        "--iw",
        String(Math.max(0, cfg.instanceWorkers)),
        "--mw",
        String(Math.max(0, cfg.importWorkers)),
        "--perf",
        cfg.performanceMode,
        ...(cfg.modifiedDefaultBypass ? ["--mdb"] : ["--no-mdb"]),
        "-c",
        String(Math.max(512, cfg.chunkSize)),
        "--ic",
        String(Math.max(0, cfg.snapshotInstanceChunkSize)),
        "-w",
        String(Math.max(1, cfg.bridgeWaitSeconds)),
        "-P",
        cfg.bridgePorts,
        "-S",
        cfg.server,
        "-C",
        cfg.configTomlPath,
        "-W",
        String(Math.max(1, cfg.wsWaitSeconds)),
        "-m",
        cfg.importMode,
        runImportFlag,
        quietTimings ? "-q" : "",
        cfg.adaptiveThrottle ? "--adaptive-throttle" : "--no-adaptive-throttle",
        cfg.noUpdateEditorIcons ? "--no-icons" : "",
        ...extraImportArgs,
      ].filter((x) => x.length > 0),
    };
  }

  private resolveRustCliPathForCommand(cfg: SyncConfig, command: string): string {
    const workspaceRoot = this.getWorkspaceRoot();
    const roots = Array.from(new Set([workspaceRoot, cfg.projectRoot].map((value) => path.normalize(value))));
    const fallbackCandidates = roots.flatMap((root) =>
      RUST_CLI_FALLBACK_RELATIVE_PATHS.map((relativePath) => path.normalize(path.join(root, relativePath))),
    );
    const uniqueCandidates = Array.from(
      new Set([cfg.rustCliPath, ...fallbackCandidates].map((candidate) => path.normalize(candidate))),
    );
    const configuredPath = path.normalize(cfg.rustCliPath);
    const configuredIsDefaultCandidate = fallbackCandidates.includes(configuredPath);

    if (!configuredIsDefaultCandidate && rustCliSupportsCommand(configuredPath, command)) {
      return configuredPath;
    }

    const supportedCandidates = uniqueCandidates.filter((candidate) => rustCliSupportsCommand(candidate, command));
    if (supportedCandidates.length === 0) {
      return configuredPath;
    }

    supportedCandidates.sort((left, right) => {
      const leftMtime = fs.existsSync(left) ? fs.statSync(left).mtimeMs : 0;
      const rightMtime = fs.existsSync(right) ? fs.statSync(right).mtimeMs : 0;
      return rightMtime - leftMtime;
    });
    return supportedCandidates[0];
  }

  private async runRustImport(
    cfg: SyncConfig,
    snapshotPath: string,
    services: string[],
    options: { quietLog?: boolean } = {},
  ): Promise<void> {
    const rustCliPath = resolveExistingRustCliPath(this.getWorkspaceRoot(), cfg.projectRoot, cfg.rustCliPath);
    this.ensureFileExists(rustCliPath);
    const selectedServices = this.normalizeServices(services, cfg.services);
    const args = [
      "import-snapshots",
      "--snapshot-dir",
      snapshotPath,
      "--project-root",
      cfg.projectRoot,
      "--services",
      selectedServices.join(","),
      "--compact-meta-json",
    ];
    const quietLog = options.quietLog === true;
    if (!quietLog) {
      this.output.show(false);
      this.logResolvedConfig(cfg);
      if (path.normalize(rustCliPath) !== path.normalize(cfg.rustCliPath)) {
        this.output.appendLine(`[renium] rust import: using fallback rustCliPath=${rustCliPath}`);
      }
      this.output.appendLine(`[renium] rust import command: ${rustCliPath} ${this.renderArgs(args)}`);
    }
    const result = await this.runCommand(
      rustCliPath,
      args,
      cfg.projectRoot,
      "rust-import",
      cfg.progressHeartbeatSeconds,
      { quietLog },
    );
    if (result.code !== 0) {
      throw new Error(`Rust import exited with code ${result.code}`);
    }
  }

  private resolveSnapshotPath(cfg: SyncConfig): string {
    return path.isAbsolute(cfg.snapshotDir) ? cfg.snapshotDir : path.join(cfg.projectRoot, cfg.snapshotDir);
  }

  private async runCommand(
    command: string,
    args: string[],
    cwd: string,
    label: string,
    progressHeartbeatSeconds: number,
    options: { quietLog?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    return await new Promise<CommandRunResult>((resolve, reject) => {
      const quietLog = options.quietLog === true;
      const timeoutMs = Math.max(
        1_000,
        Math.min(MAX_COMMAND_TIMEOUT_MS, Math.floor(Number(options.timeoutMs) || DEFAULT_COMMAND_TIMEOUT_MS)),
      );
      const launchedAt = Date.now();
      let lastOutputAt = launchedAt;
      let sawOutput = false;
      let capturedOutput = "";
      let settled = false;
      let heartbeatTimer: NodeJS.Timeout | undefined;
      let timeoutTimer: NodeJS.Timeout | undefined;
      let child: childProcess.ChildProcess;

      const appendOutput = (text: string): void => {
        capturedOutput += text;
        if (capturedOutput.length > 8_000_000) {
          capturedOutput = capturedOutput.slice(-8_000_000);
        }
      };
      const cleanup = (): void => {
        if (heartbeatTimer) {
          clearInterval(heartbeatTimer);
          heartbeatTimer = undefined;
        }
        if (timeoutTimer) {
          clearTimeout(timeoutTimer);
          timeoutTimer = undefined;
        }
      };
      const finish = (code: number): void => {
        if (settled) {
          return;
        }
        settled = true;
        cleanup();
        const elapsedSec = ((Date.now() - launchedAt) / 1000).toFixed(1);
        if (!quietLog || code !== 0) {
          this.output.appendLine(`[renium] ${label}: exited code=${code} after ${elapsedSec}s`);
        }
        resolve({ code, output: capturedOutput });
      };
      const fail = (err: Error): void => {
        if (settled) {
          return;
        }
        settled = true;
        cleanup();
        reject(err);
      };

      try {
        child = childProcess.spawn(command, args, {
          cwd,
          env: process.env,
          shell: false,
          stdio: "pipe",
          windowsHide: true,
        });
      } catch (err) {
        fail(err instanceof Error ? err : new Error(String(err)));
        return;
      }
      if (!quietLog) {
        this.output.appendLine(
          `[renium] ${label}: spawned pid=${child.pid ?? "unknown"} at ${new Date(launchedAt).toISOString()}`,
        );
      }

      const heartbeatMs = Math.max(2, Math.round(progressHeartbeatSeconds)) * 1000;
      heartbeatTimer = quietLog
        ? undefined
        : setInterval(() => {
          const now = Date.now();
          const elapsedSec = ((now - launchedAt) / 1000).toFixed(1);
          const idleSec = ((now - lastOutputAt) / 1000).toFixed(1);
          if (!sawOutput) {
            this.output.appendLine(`[renium] ${label}: waiting for first output (${elapsedSec}s elapsed)`);
          } else {
            this.output.appendLine(`[renium] ${label}: still running (${elapsedSec}s elapsed, idle ${idleSec}s)`);
          }
        }, heartbeatMs);

      timeoutTimer = setTimeout(() => {
        appendOutput(`\n[renium] ${label}: timed out after ${Math.round(timeoutMs / 1000)}s; terminating the process.\n`);
        try {
          child.kill();
        } catch {
        }
        finish(124);
      }, timeoutMs);

      this.bindProcessOutput(child, command, () => {
        sawOutput = true;
        lastOutputAt = Date.now();
      }, (text) => {
        appendOutput(text);
      }, { quietLog });

      child.on("error", (err) => {
        fail(err);
      });
      child.on("close", (code) => {
        finish(code ?? 0);
      });
    });
  }

  private bindProcessOutput(
    child: childProcess.ChildProcess,
    prefix: string,
    onActivity?: () => void,
    onChunk?: (text: string) => void,
    options: { quietLog?: boolean } = {},
  ): void {
    child.stdout?.on("data", (data: Buffer | string) => {
      onActivity?.();
      onChunk?.(data.toString());
      if (!options.quietLog) {
        this.output.append(this.prefixOutput(prefix, data));
      }
    });

    child.stderr?.on("data", (data: Buffer | string) => {
      onActivity?.();
      onChunk?.(data.toString());
      if (!options.quietLog) {
        this.output.append(this.prefixOutput(`${prefix}:err`, data));
      }
    });
  }

  private prefixOutput(prefix: string, data: Buffer | string): string {
    const text = data.toString();
    const lines = text.replace(/\r\n/g, "\n").split("\n");
    if (lines.length === 1) {
      return `[${prefix}] ${lines[0]}`;
    }

    return lines
      .filter((line, index) => !(line.length === 0 && index === lines.length - 1))
      .map((line) => `[${prefix}] ${line}`)
      .join("\n") + "\n";
  }

  private ensureFileExists(filePath: string): void {
    if (!fs.existsSync(filePath)) {
      throw new Error(`Required file not found: ${filePath}`);
    }
  }

  private normalizeServices(requested: string[], fallback: string[]): string[] {
    const requestedSet = new Set(requested.map((x) => x.trim()).filter((x) => x.length > 0));
    if (requestedSet.size === 0) {
      fallback.forEach((s) => requestedSet.add(s));
    }
    return Array.from(requestedSet);
  }

  private normalizeReportedServices(reported: string[], allowedServices: string[]): string[] {
    const allowed = new Set(allowedServices.map((service) => service.trim()).filter((service) => service.length > 0));
    const services = new Set<string>();
    for (const value of reported) {
      const service = String(value).trim();
      if (service.length > 0 && allowed.has(service)) {
        services.add(service);
      }
    }
    return Array.from(services);
  }

  /** CLI path + cwd for the .renium viewer, reusing the resolved export CLI. */
  public rbsyncViewerCli(): { cliPath: string; cwd: string } | undefined {
    const cfg = this.getConfig();
    if (!cfg.exportCliPath) {
      return undefined;
    }
    return { cliPath: cfg.exportCliPath, cwd: cfg.projectRoot };
  }

  private getConfig(): SyncConfig {
    const root = this.getWorkspaceRoot();
    const cfg = vscode.workspace.getConfiguration("renium");

    const projectRoot = this.resolveConfigPath(cfg.get<string>("projectRoot", "${workspaceFolder}"), root);
    const configTomlPath = this.resolveConfigPath(
      cfg.get<string>("configTomlPath", "${userHome}/.codex/config.toml"),
      root,
    );
    const watchConfigPath = this.resolveConfigPath(
      cfg.get<string>("watchConfigPath", "${workspaceFolder}/tools/editor_to_studio_sync.json"),
      root,
    );
    const configuredExportCliPath = this.resolveConfigPath(
      cfg.get<string>("exportCliPath", "${workspaceFolder}/renium.exe"),
      root,
    );
    const exportCliPath = resolveExistingRustCliPath(root, projectRoot, configuredExportCliPath);
    const editorSyncCliPath = this.resolveConfigPath(
      cfg.get<string>("editorSyncCliPath", "${workspaceFolder}/dist/editor_to_studio_sync.exe"),
      root,
    );

    const servicesRaw = cfg.get<string[]>("services", DEFAULT_SERVICES);
    const services = (Array.isArray(servicesRaw) ? servicesRaw : DEFAULT_SERVICES)
      .map((s) => String(s).trim())
      .filter((s) => s.length > 0);

    const transportRaw = cfg.get<string>("transport", "ws");
    const transport = transportRaw === "mcp" ? "mcp" : "ws";

    const importModeRaw = cfg.get<string>("importMode", "direct");
    const importMode = importModeRaw === "snapshot" ? "snapshot" : "direct";
    const performanceModeRaw = cfg.get<string>("performanceMode", "throughput");
    const performanceMode =
      performanceModeRaw === "smooth"
        ? "smooth"
        : performanceModeRaw === "balanced"
          ? "balanced"
          : "throughput";
    const modifiedDefaultBypass = cfg.get<boolean>("modifiedDefaultBypass", false) === true;
    const wsWaitSeconds = this.getWsWaitSeconds(cfg);
    const chunkSize = this.normalizeChunkSize(cfg);
    const configuredRustCliPath = this.resolveConfigPath(
      cfg.get<string>("rustCliPath", "${workspaceFolder}/renium.exe"),
      root,
    );
    const rustCliPath = resolveExistingRustCliPath(root, projectRoot, configuredRustCliPath);

    const gitStagePathsRaw = cfg.get<string[]>("gitSync.stagePaths", []);
    const gitStagePaths = (Array.isArray(gitStagePathsRaw) ? gitStagePathsRaw : [])
      .map((value) => String(value).trim())
      .filter((value) => value.length > 0);
    const gitRunFullSyncBeforePushRaw = cfg.get<string>("gitSync.runFullSyncBeforePush", "ask");
    const gitRunFullSyncBeforePush = gitRunFullSyncBeforePushRaw === "always"
      ? "always"
      : gitRunFullSyncBeforePushRaw === "never"
        ? "never"
        : "ask";
    const gitStageModeRaw = cfg.get<string>("gitSync.stageMode", "tracked");
    const gitStageMode = gitStageModeRaw === "configuredPaths" ? "configuredPaths" : "tracked";
    const gitApplyPulledChangesRaw = cfg.get<string>("gitSync.applyPulledChangesToStudio", "ask");
    const gitApplyPulledChangesToStudio = gitApplyPulledChangesRaw === "always"
      ? "always"
      : gitApplyPulledChangesRaw === "never"
        ? "never"
        : "ask";
    const gitOutputBehaviorRaw = cfg.get<string>("gitSync.outputBehavior", "onStart");
    const gitOutputBehavior = gitOutputBehaviorRaw === "silent"
      ? "silent"
      : gitOutputBehaviorRaw === "onError"
        ? "onError"
        : "onStart";
    const wallyApplyToStudioRaw = cfg.get<string>("wallySync.applyToStudio", "ask");
    const wallyApplyToStudio = wallyApplyToStudioRaw === "always"
      ? "always"
      : wallyApplyToStudioRaw === "never"
        ? "never"
        : "ask";
    const linkApplyToStudioRaw = cfg.get<string>("link.applyToStudio", "ask");
    const linkApplyToStudio = linkApplyToStudioRaw === "always"
      ? "always"
      : linkApplyToStudioRaw === "never"
        ? "never"
        : "ask";
    const initialSyncPriorityRaw = cfg.get<string>("liveSync.initialSyncPriority", "studio");
    const initialSyncPriority: InitialSyncPriority = initialSyncPriorityRaw === "editor"
      ? "editor"
      : initialSyncPriorityRaw === "none"
        ? "none"
        : "studio";
    const displayPromptsRaw = cfg.get<string>("liveSync.displayPrompts", "always");
    const displayPrompts: DisplayPrompts = displayPromptsRaw === "initial"
      ? "initial"
      : displayPromptsRaw === "never"
        ? "never"
        : "always";
    const logLevel = this.configuredLogLevel();

    return {
      exportCliPath,
      editorSyncCliPath,
      rustCliPath,
      projectRoot,
      snapshotDir: cfg.get<string>("snapshotDir", "snapshots"),
      transport,
      server: cfg.get<string>("server", "Roblox_Studio"),
      configTomlPath,
      services: services.length > 0 ? services : [...DEFAULT_SERVICES],
      sourceWorkers: this.configNumber(cfg, "sourceWorkers", 0, { min: 0, integer: true }),
      instanceWorkers: this.configNumber(cfg, "instanceWorkers", 0, { min: 0, integer: true }),
      importWorkers: this.configNumber(cfg, "importWorkers", 0, { min: 0, integer: true }),
      chunkSize,
      snapshotInstanceChunkSize: this.configNumber(cfg, "snapshotInstanceChunkSize", 5000, { min: 0, integer: true }),
      bridgeWaitSeconds: this.configNumber(cfg, "bridgeWaitSeconds", 8, { min: 1 }),
      bridgePorts: this.normalizeBridgePorts(
        String(
          cfg.get<string>(
            "bridgePorts",
            DEFAULT_BRIDGE_PORTS.join(","),
          ) ?? DEFAULT_BRIDGE_PORTS.join(","),
        ),
      ),
      usePersistentBridge: cfg.get<boolean>("usePersistentBridge", true) !== false,
      verifyEditorPushSources: cfg.get<boolean>("verifyEditorPushSources", false) === true,
      adaptiveThrottle: cfg.get<boolean>("adaptiveThrottle", true),
      noUpdateEditorIcons: cfg.get<boolean>("noUpdateEditorIcons", true),
      autoSyncOnSave: cfg.get<boolean>("autoSyncOnSave", false),
      autoSyncDebounceMs: this.configNumber(cfg, "autoSyncDebounceMs", 800, { min: 100, integer: true }),
      editorLiveSyncEnabled: cfg.get<boolean>("editorLiveSyncEnabled", false) === true,
      editorLiveSyncOnStartup: cfg.get<boolean>("editorLiveSyncOnStartup", false) === true,
      studioLiveSyncEnabled: cfg.get<boolean>("studioLiveSyncEnabled", true) !== false,
      studioLiveSyncPollMs: this.configNumber(
        cfg,
        "studioLiveSyncPollMs",
        DEFAULT_STUDIO_LIVE_SYNC_POLL_MS,
        { min: MIN_STUDIO_LIVE_SYNC_POLL_MS, integer: true },
      ),
      initialSyncPriority,
      changesThreshold: this.configNumber(cfg, "liveSync.changesThreshold", 5, { min: 0, integer: true }),
      diffLinesLimit: this.configNumber(cfg, "liveSync.diffLinesLimit", 3000, { min: 100, integer: true }),
      displayPrompts,
      logLevel,
      overridePackages: cfg.get<boolean>("liveSync.overridePackages", false) === true,
      conflictResolution: this.normalizeConflictPolicy(cfg.get<string>("liveSync.conflictResolution", "prompt")),
      runImport: cfg.get<boolean>("runImport", true),
      importMode,
      performanceMode,
      modifiedDefaultBypass,
      watchConfigPath,
      wsWaitSeconds,
      progressHeartbeatSeconds: this.configNumber(cfg, "progressHeartbeatSeconds", 2, { min: 2 }),
      benchmarkRuns: this.configNumber(cfg, "benchmarkRuns", 5, { min: 1, integer: true }),
      gitSync: {
        gitPath: cfg.get<string>("gitSync.gitPath", "git"),
        remote: cfg.get<string>("gitSync.remote", "origin"),
        branch: cfg.get<string>("gitSync.branch", ""),
        autoFetch: cfg.get<boolean>("gitSync.autoFetch", true) !== false,
        runFullSyncBeforePush: gitRunFullSyncBeforePush,
        stageMode: gitStageMode,
        stagePaths: gitStagePaths.length > 0 ? gitStagePaths : ["src"],
        includeUntracked: cfg.get<boolean>("gitSync.includeUntracked", false) === true,
        commitMessageTemplate: cfg.get<string>("gitSync.commitMessageTemplate", "Renium sync: ${date}"),
        confirmBeforePush: cfg.get<boolean>("gitSync.confirmBeforePush", true) !== false,
        requireCleanWorktreeBeforePull: cfg.get<boolean>("gitSync.requireCleanWorktreeBeforePull", true) !== false,
        applyPulledChangesToStudio: gitApplyPulledChangesToStudio,
        timeoutSeconds: this.configNumber(cfg, "gitSync.timeoutSeconds", 120, { min: 10 }),
        outputBehavior: gitOutputBehavior,
      },
      wallySync: {
        wallyPath: cfg.get<string>("wallySync.wallyPath", "wally"),
        rojoPath: cfg.get<string>("wallySync.rojoPath", "rojo"),
        packagesDir: cfg.get<string>("wallySync.packagesDir", "Packages"),
        targetService: cfg.get<string>("wallySync.targetService", "ReplicatedStorage"),
        targetName: cfg.get<string>("wallySync.targetName", "Packages"),
        realms: cfg.get<string>("wallySync.realms", "shared,server,dev"),
        runInstall: cfg.get<boolean>("wallySync.runInstall", true) !== false,
        applyToStudio: wallyApplyToStudio,
      },
      linkSync: {
        manifest: cfg.get<string>("link.manifest", "renium-link.json"),
        folder: (cfg.get<string>("link.folder", "") ?? "").trim(),
        cacheDir: (cfg.get<string>("link.cacheDir", "") ?? "").trim(),
        gitPath: cfg.get<string>("link.gitPath", "git"),
        wallyPath: cfg.get<string>("wallySync.wallyPath", "wally"),
        offline: cfg.get<boolean>("link.offline", false) === true,
        autoApply: cfg.get<boolean>("link.autoApplyOnManifestChange", false) === true,
        applyToStudio: linkApplyToStudio,
      },
    };
  }

  private normalizeChunkSize(cfg: vscode.WorkspaceConfiguration): number {
    const inspected = cfg.inspect<number>("chunkSize");
    const configuredValue =
      inspected?.workspaceFolderValue ??
      inspected?.workspaceValue ??
      inspected?.globalValue ??
      inspected?.defaultValue;
    const rawValue = Number(configuredValue ?? DEFAULT_CHUNK_SIZE);

    if (!Number.isFinite(rawValue) || rawValue < 512) {
      return DEFAULT_CHUNK_SIZE;
    }

    if (rawValue <= 262144) {
      if (!this.warnedLegacyChunkSize) {
        this.warnedLegacyChunkSize = true;
        this.output.appendLine(`[renium] config: chunkSize 262144 is legacy; using ${DEFAULT_CHUNK_SIZE} for this run.`);
      }
      return DEFAULT_CHUNK_SIZE;
    }

    const normalized = Math.max(512, Math.floor(rawValue));
    if (normalized > MAX_BRIDGE_CHUNK_SIZE) {
      if (!this.warnedChunkSizeCap) {
        this.warnedChunkSizeCap = true;
        this.output.appendLine(
          `[renium] config: chunkSize ${normalized} exceeds the ${MAX_BRIDGE_CHUNK_SIZE}-byte bridge transport limit; using ${MAX_BRIDGE_CHUNK_SIZE} for this run.`,
        );
      }
      return MAX_BRIDGE_CHUNK_SIZE;
    }

    return normalized;
  }

  private configNumber(
    cfg: vscode.WorkspaceConfiguration,
    key: string,
    defaultValue: number,
    options: { min?: number; integer?: boolean } = {},
  ): number {
    return this.normalizeConfigNumber(cfg.get<number>(key, defaultValue), defaultValue, options);
  }

  private normalizeConfigNumber(
    value: unknown,
    defaultValue: number,
    options: { min?: number; integer?: boolean } = {},
  ): number {
    const rawValue = Number(value ?? defaultValue);
    const fallback = this.applyConfigNumberOptions(defaultValue, options);
    if (!Number.isFinite(rawValue)) {
      return fallback;
    }
    return this.applyConfigNumberOptions(rawValue, options);
  }

  private applyConfigNumberOptions(
    value: number,
    options: { min?: number; integer?: boolean },
  ): number {
    const min = typeof options.min === "number" ? options.min : undefined;
    const normalized = options.integer === true ? Math.floor(value) : value;
    return min === undefined ? normalized : Math.max(min, normalized);
  }

  private configOrigin(cfg: vscode.WorkspaceConfiguration, key: string): string {
    const inspected = cfg.inspect(key);
    if (inspected?.workspaceFolderValue !== undefined) {
      return "workspace-folder";
    }
    if (inspected?.workspaceValue !== undefined) {
      return "workspace";
    }
    if (inspected?.globalValue !== undefined) {
      return "user";
    }
    if (inspected?.defaultValue !== undefined) {
      return "default";
    }
    return "unset";
  }

  private configuredValue(cfg: vscode.WorkspaceConfiguration, key: string): unknown {
    const inspected = cfg.inspect(key);
    return (
      inspected?.workspaceFolderValue ??
      inspected?.workspaceValue ??
      inspected?.globalValue ??
      inspected?.defaultValue
    );
  }

  private logResolvedConfig(cfg: SyncConfig): void {
    const workspaceCfg = vscode.workspace.getConfiguration("renium");
    const extensionVersion = String(this.context.extension.packageJSON.version ?? "unknown");
    const extensionEntryPath = path.join(this.context.extensionPath, "out", "extension.js");
    const extensionBuildUnix = fs.existsSync(extensionEntryPath)
      ? Math.floor(fs.statSync(extensionEntryPath).mtimeMs / 1000)
      : 0;
    this.output.appendLine(`[renium] extension version=${extensionVersion}`);
    this.output.appendLine(`[renium] extension build_unix=${extensionBuildUnix}`);
    this.output.appendLine(`[renium] config: exportCliPath=${cfg.exportCliPath}`);
    this.output.appendLine(`[renium] config: rustCliPath=${cfg.rustCliPath}`);
    this.output.appendLine(
      `[renium] config: chunkSize=${cfg.chunkSize} (origin=${this.configOrigin(workspaceCfg, "chunkSize")}, raw=${String(this.configuredValue(workspaceCfg, "chunkSize"))})`,
    );
    this.output.appendLine(
      `[renium] config: bridgePorts=${cfg.bridgePorts} (origin=${this.configOrigin(workspaceCfg, "bridgePorts")})`,
    );
    this.output.appendLine(
      `[renium] config: usePersistentBridge=${cfg.usePersistentBridge} (origin=${this.configOrigin(workspaceCfg, "usePersistentBridge")})`,
    );
    this.output.appendLine(
      `[renium] config: sourceWorkers=${cfg.sourceWorkers} (origin=${this.configOrigin(workspaceCfg, "sourceWorkers")})`,
    );
    this.output.appendLine(
      `[renium] config: instanceWorkers=${cfg.instanceWorkers} (origin=${this.configOrigin(workspaceCfg, "instanceWorkers")})`,
    );
    this.output.appendLine(
      `[renium] config: importWorkers=${cfg.importWorkers} (origin=${this.configOrigin(workspaceCfg, "importWorkers")})`,
    );
    this.output.appendLine(
      `[renium] config: importMode=${cfg.importMode} (origin=${this.configOrigin(workspaceCfg, "importMode")})`,
    );
    this.output.appendLine(
      `[renium] config: performanceMode=${cfg.performanceMode} (origin=${this.configOrigin(workspaceCfg, "performanceMode")})`,
    );
    this.output.appendLine(
      `[renium] config: modifiedDefaultBypass=${cfg.modifiedDefaultBypass} (origin=${this.configOrigin(workspaceCfg, "modifiedDefaultBypass")})`,
    );
    this.output.appendLine(
      `[renium] config: benchmarkRuns=${cfg.benchmarkRuns} (origin=${this.configOrigin(workspaceCfg, "benchmarkRuns")})`,
    );
  }

  private normalizeBridgePorts(raw: string): string {
    const parsed = raw
      .split(",")
      .map((token) => Number.parseInt(token.trim(), 10))
      .filter((value) => Number.isInteger(value) && value > 0 && value <= 65535)
      .filter((value, index, all) => all.indexOf(value) === index);

    let normalized = parsed;
    const matchesPreviousDefault =
      normalized.length === PREVIOUS_DEFAULT_BRIDGE_PORTS.length &&
      normalized.every((value, index) => value === PREVIOUS_DEFAULT_BRIDGE_PORTS[index]);
    const matchesLegacyDefault =
      normalized.length === LEGACY_BRIDGE_PORTS.length &&
      normalized.every((value, index) => value === LEGACY_BRIDGE_PORTS[index]);
    if (matchesPreviousDefault || matchesLegacyDefault) {
      if (!this.warnedLegacyBridgePorts) {
        this.warnedLegacyBridgePorts = true;
        this.output.appendLine(
          `[renium] config: migrating legacy bridge default to ${DEFAULT_BRIDGE_PORTS.join(",")}.`,
        );
      }
      normalized = [...DEFAULT_BRIDGE_PORTS];
    }

    if (normalized.length === 0) {
      normalized = [...DEFAULT_BRIDGE_PORTS];
    }

    if (normalized.length > DEFAULT_BRIDGE_PORTS.length) {
      if (!this.warnedBridgePortLimit) {
        this.warnedBridgePortLimit = true;
        this.output.appendLine(
          `[renium] config: only ${DEFAULT_BRIDGE_PORTS.length} bridge ports are supported; using ${normalized
            .slice(0, DEFAULT_BRIDGE_PORTS.length)
            .join(",")}.`,
        );
      }
      normalized = normalized.slice(0, DEFAULT_BRIDGE_PORTS.length);
    }

    return normalized.join(",");
  }

  private getWsWaitSeconds(cfg: vscode.WorkspaceConfiguration): number {
    const configuredWsWaitSeconds = this.getConfiguredNumber(cfg, "wsWaitSeconds");
    if (configuredWsWaitSeconds !== undefined) {
      return this.normalizeConfigNumber(configuredWsWaitSeconds, 20, { min: 1 });
    }

    const legacyStartupWaitSeconds = this.getConfiguredNumber(cfg, "startupWaitSeconds");
    if (legacyStartupWaitSeconds !== undefined) {
      if (!this.warnedLegacyStartupWaitSeconds) {
        this.warnedLegacyStartupWaitSeconds = true;
        this.output.appendLine(
          "[renium] config: using legacy renium.startupWaitSeconds as renium.wsWaitSeconds; update your settings to renium.wsWaitSeconds.",
        );
      }
      return this.normalizeConfigNumber(legacyStartupWaitSeconds, 20, { min: 1 });
    }

    return this.configNumber(cfg, "wsWaitSeconds", 20, { min: 1 });
  }

  private getConfiguredNumber(
    cfg: vscode.WorkspaceConfiguration,
    key: string,
  ): number | undefined {
    const inspected = cfg.inspect<number>(key);
    const configuredValue =
      inspected?.workspaceFolderValue ??
      inspected?.workspaceValue ??
      inspected?.globalValue;
    const value = Number(configuredValue);
    return Number.isFinite(value) ? value : undefined;
  }

  private resolveConfigPath(raw: string, workspaceRoot: string): string {
    const replaced = raw
      .replaceAll("${workspaceFolder}", workspaceRoot)
      .replaceAll("${userHome}", os.homedir());
    return path.isAbsolute(replaced) ? path.normalize(replaced) : path.normalize(path.join(workspaceRoot, replaced));
  }

  private getWorkspaceRoot(): string {
    const folders = vscode.workspace.workspaceFolders;
    const root = pickWorkspaceRoot();
    if (!root) {
      throw new Error("Open a workspace folder before using Renium.");
    }
    if (folders && folders.length > 1 && root === folders[0].uri.fsPath && !this.warnedMultiRootWorkspace) {
      this.warnedMultiRootWorkspace = true;
      this.output.appendLine(
        `[renium] multi-root workspace: no folder contains renium.exe; using the first folder (${root}). Set renium.projectRoot or renium.exportCliPath if this is wrong.`,
      );
    }
    return root;
  }

  private isPathInside(filePath: string, rootPath: string): boolean {
    const relative = path.relative(this.normalizePathForCompare(rootPath), this.normalizePathForCompare(filePath));
    return relative === "" || (!!relative && !relative.startsWith("..") && !path.isAbsolute(relative));
  }

  private normalizePathForCompare(filePath: string): string {
    const normalized = path.resolve(filePath);
    return process.platform === "win32" ? normalized.toLowerCase() : normalized;
  }

  private comparePathsForStableOrder(a: string, b: string): number {
    const left = this.normalizePathForCompare(a);
    const right = this.normalizePathForCompare(b);
    return left < right ? -1 : left > right ? 1 : 0;
  }

  private editorChangedPathArg(filePath: string, projectRoot: string): string {
    if (!this.isPathInside(filePath, projectRoot)) {
      return filePath;
    }
    return path.relative(projectRoot, filePath);
  }

  private detectServiceForPath(filePath: string, projectRoot: string, services: string[]): string | undefined {
    const srcRoot = path.join(projectRoot, "src");
    if (!this.isPathInside(filePath, srcRoot)) {
      return undefined;
    }

    const relative = path.relative(srcRoot, filePath);
    if (relative.startsWith("..") || path.isAbsolute(relative)) {
      return undefined;
    }

    const firstSegment = relative.split(path.sep)[0];
    const byLower = new Map(services.map((s) => [s.toLowerCase(), s]));
    return byLower.get(firstSegment.toLowerCase());
  }

  private parseBenchmarkMetrics(output: string): BenchmarkRunMetrics {
    const lines = output.replace(/\r\n/g, "\n").split("\n");
    type ServiceBenchmarkMetrics = {
      instanceFetchMs?: number;
      pluginServerMs: number;
      pluginEncodeMs: number;
      payloadBytes: number;
      chunkCount: number;
      sawPerfLine: boolean;
      maxFrameMs?: number;
      stallCountOver33Ms: number;
      stallCountOver50Ms: number;
      stallCountOver100Ms: number;
    };
    const serviceMetrics = new Map<string, ServiceBenchmarkMetrics>();
    const metricsForService = (service: string): ServiceBenchmarkMetrics => {
      let metrics = serviceMetrics.get(service);
      if (!metrics) {
        metrics = {
          pluginServerMs: 0,
          pluginEncodeMs: 0,
          payloadBytes: 0,
          chunkCount: 0,
          sawPerfLine: false,
          stallCountOver33Ms: 0,
          stallCountOver50Ms: 0,
          stallCountOver100Ms: 0,
        };
        serviceMetrics.set(service, metrics);
      }
      return metrics;
    };
    let runTimingSummary: Partial<BenchmarkRunMetrics> = {};

    for (const line of lines) {
      const payloadMatch = /([A-Za-z][A-Za-z0-9_]*): (?:(?:adaptive wave \d+)|instance) payloads chunk metrics -> chunks=(\d+), bytes=(\d+), .*plugin_server_ms=([0-9.]+), plugin_encode_ms=([0-9.]+)/.exec(
        line,
      );
      if (payloadMatch) {
        const metrics = metricsForService(payloadMatch[1]);
        metrics.chunkCount += Number.parseInt(payloadMatch[2], 10);
        metrics.payloadBytes += Number.parseInt(payloadMatch[3], 10);
        metrics.pluginServerMs += Number.parseFloat(payloadMatch[4]);
        metrics.pluginEncodeMs += Number.parseFloat(payloadMatch[5]);
      }

      const perfMatch = /([A-Za-z][A-Za-z0-9_]*): adaptive wave \d+ perf stats -> last_frame_ms=([^,]+), max_frame_ms=([^,]+), stalls33=([^,]+), stalls50=([^,]+), stalls100=([^,]+)/.exec(
        line,
      );
      if (perfMatch) {
        const metrics = metricsForService(perfMatch[1]);
        metrics.sawPerfLine = true;
        const maxFrameMs = Number.parseFloat(perfMatch[3]);
        if (Number.isFinite(maxFrameMs)) {
          metrics.maxFrameMs = metrics.maxFrameMs === undefined ? maxFrameMs : Math.max(metrics.maxFrameMs, maxFrameMs);
        }
        const stalls33 = Number.parseInt(perfMatch[4], 10);
        if (Number.isFinite(stalls33)) {
          metrics.stallCountOver33Ms += stalls33;
        }
        const stalls50 = Number.parseInt(perfMatch[5], 10);
        if (Number.isFinite(stalls50)) {
          metrics.stallCountOver50Ms += stalls50;
        }
        const stalls100 = Number.parseInt(perfMatch[6], 10);
        if (Number.isFinite(stalls100)) {
          metrics.stallCountOver100Ms += stalls100;
        }
      }

      const instanceFetchMatch = /timing: ([A-Za-z][A-Za-z0-9_]*): instance fetch took ([0-9.]+)ms/.exec(line);
      if (instanceFetchMatch) {
        metricsForService(instanceFetchMatch[1]).instanceFetchMs = Number.parseFloat(instanceFetchMatch[2]);
      }

      const runTimingMatch =
        /run timing summary: total_ms=([0-9.]+), core_export_ms=([0-9.]+), bridge_startup_ms=([0-9.]+), handshake_ms=([0-9.]+), service_export_sum_ms=([0-9.]+), import_critical_tail_ms=([0-9.]+), unmeasured_or_scheduler_gap_ms=([0-9.]+)/.exec(
          line,
        );
      if (runTimingMatch) {
        runTimingSummary = {
          totalMs: Number.parseFloat(runTimingMatch[1]),
          coreExportMs: Number.parseFloat(runTimingMatch[2]),
          bridgeStartupMs: Number.parseFloat(runTimingMatch[3]),
          handshakeMs: Number.parseFloat(runTimingMatch[4]),
          serviceExportSumMs: Number.parseFloat(runTimingMatch[5]),
          importCriticalTailMs: Number.parseFloat(runTimingMatch[6]),
          unmeasuredOrSchedulerGapMs: Number.parseFloat(runTimingMatch[7]),
        };
      }
    }

    let trackedService: string | undefined;
    let trackedMetrics: ServiceBenchmarkMetrics | undefined;
    let bestScore = -1;
    for (const [service, metrics] of serviceMetrics.entries()) {
      const score =
        (metrics.instanceFetchMs ?? 0) * 1_000_000 +
        metrics.pluginServerMs * 10_000 +
        metrics.pluginEncodeMs * 1_000 +
        metrics.payloadBytes;
      if (score > bestScore) {
        bestScore = score;
        trackedService = service;
        trackedMetrics = metrics;
      }
    }
    const serviceMetricList = Array.from(serviceMetrics.entries())
      .map(([service, metrics]) => ({
        service,
        instanceFetchMs: metrics.instanceFetchMs,
        pluginServerMs: metrics.chunkCount > 0 ? metrics.pluginServerMs : undefined,
        pluginEncodeMs: metrics.chunkCount > 0 ? metrics.pluginEncodeMs : undefined,
        payloadBytes: metrics.chunkCount > 0 ? metrics.payloadBytes : undefined,
        chunkCount: metrics.chunkCount > 0 ? metrics.chunkCount : undefined,
        maxFrameMs: metrics.maxFrameMs,
        stallCountOver33Ms: metrics.sawPerfLine ? metrics.stallCountOver33Ms : undefined,
        stallCountOver50Ms: metrics.sawPerfLine ? metrics.stallCountOver50Ms : undefined,
        stallCountOver100Ms: metrics.sawPerfLine ? metrics.stallCountOver100Ms : undefined,
      }))
      .sort((a, b) => this.benchmarkServiceScore(b) - this.benchmarkServiceScore(a));

    return {
      totalMs: runTimingSummary.totalMs ?? this.matchLastNumber(output, /full export-snapshots run took ([0-9.]+)ms/g),
      trackedService,
      coreExportMs: runTimingSummary.coreExportMs,
      bridgeStartupMs: runTimingSummary.bridgeStartupMs,
      handshakeMs: runTimingSummary.handshakeMs,
      serviceExportSumMs: runTimingSummary.serviceExportSumMs,
      importCriticalTailMs: runTimingSummary.importCriticalTailMs,
      unmeasuredOrSchedulerGapMs: runTimingSummary.unmeasuredOrSchedulerGapMs,
      trackedServiceInstanceFetchMs: trackedMetrics?.instanceFetchMs,
      trackedServicePluginServerMs: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.pluginServerMs : undefined,
      trackedServicePluginEncodeMs: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.pluginEncodeMs : undefined,
      trackedServicePayloadBytes: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.payloadBytes : undefined,
      trackedServiceChunkCount: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.chunkCount : undefined,
      trackedServiceMaxFrameMs: trackedMetrics?.maxFrameMs,
      trackedServiceStallCountOver33Ms: trackedMetrics?.sawPerfLine ? trackedMetrics.stallCountOver33Ms : undefined,
      trackedServiceStallCountOver50Ms: trackedMetrics?.sawPerfLine ? trackedMetrics.stallCountOver50Ms : undefined,
      trackedServiceStallCountOver100Ms: trackedMetrics?.sawPerfLine ? trackedMetrics.stallCountOver100Ms : undefined,
      exportFingerprint: this.matchLastString(
        output,
        /export start: version=([^,\n]+), git=([^,\n]+), build_ts=([^,\n]+), features=([^,\n]+), protocol=([^,\n]+)/g,
        (match) =>
          `version=${match[1]}, git=${match[2]}, build_ts=${match[3]}, features=${match[4]}, protocol=${match[5]}`,
      ),
      bridgeFingerprint: this.matchLastString(
        output,
        /bridge info: version=([^,\n]+), build_unix=([^,\n]+), protocol=([^,\n]+), codec=([^,\n]+), chunk_frame=([^,\n]+), compact_value=([^,\n]+), warm_mode=([^,\n]+), serializer_mode=([^,\n]+)/g,
        (match) =>
          `version=${match[1]}, build_unix=${match[2]}, protocol=${match[3]}, codec=${match[4]}, chunk_frame=${match[5]}, compact_value=${match[6]}, warm_mode=${match[7]}, serializer_mode=${match[8]}`,
      ),
      serviceMetrics: serviceMetricList,
    };
  }

  private extractPluginProfile(output: string): PluginProfileResult {
    const marker = "[renium] plugin op profile";
    const markerIndex = output.lastIndexOf(marker);
    const jsonStart = output.indexOf("{", markerIndex >= 0 ? markerIndex : 0);
    const jsonEnd = output.lastIndexOf("}");
    if (jsonStart < 0 || jsonEnd <= jsonStart) {
      throw new Error("Plugin profile JSON was not found in CLI output.");
    }

    const rawJson = output.slice(jsonStart, jsonEnd + 1);
    let parsed: unknown;
    try {
      parsed = JSON.parse(rawJson);
    } catch (error) {
      throw new Error(`Failed to parse plugin profile JSON: ${error instanceof Error ? error.message : String(error)}`);
    }

    if (!parsed || typeof parsed !== "object") {
      throw new Error("Plugin profile JSON did not decode to an object.");
    }
    return parsed as PluginProfileResult;
  }

  private formatPluginProfileRanking(profile: PluginProfileResult, limit: number): string[] {
    const projectedCalls =
      typeof profile.profile?.projectedServerStoragePropertyReads === "number" &&
      Number.isFinite(profile.profile.projectedServerStoragePropertyReads)
        ? profile.profile.projectedServerStoragePropertyReads
        : 1_259_770;
    const entries: Array<{
      name: string;
      perCallUs: number;
      p90Us: number | undefined;
      projectedMsPer100k: number;
      projectedServerStorageMs: number;
    }> = [];
    for (const [name, operation] of Object.entries(profile.operations ?? {})) {
      const perCallUs =
        typeof operation?.perCallUs === "number" && Number.isFinite(operation.perCallUs)
          ? operation.perCallUs
          : undefined;
      if (perCallUs === undefined) {
        continue;
      }
      entries.push({
        name,
        perCallUs,
        p90Us: typeof operation?.p90Us === "number" && Number.isFinite(operation.p90Us) ? operation.p90Us : undefined,
        projectedMsPer100k: perCallUs * 100,
        projectedServerStorageMs: (perCallUs * projectedCalls) / 1000,
      });
    }
    entries.sort((a, b) => b.projectedMsPer100k - a.projectedMsPer100k);
    const ranked = entries.slice(0, Math.max(1, limit));

    if (ranked.length === 0) {
      return ["[renium] profile: no per-call operations were available to rank."];
    }

    return ranked.map(
      (entry, index) =>
        `[renium] profile: ${String(index + 1).padStart(2, "0")} ${entry.name} per_call=${entry.perCallUs.toFixed(3)}us p90=${entry.p90Us?.toFixed(1) ?? "n/a"}us per_100k=${entry.projectedMsPer100k.toFixed(1)}ms projected_serverstorage=${entry.projectedServerStorageMs.toFixed(1)}ms`,
    );
  }

  private matchLastNumber(output: string, pattern: RegExp): number | undefined {
    const match = this.matchLastString(output, pattern);
    if (!match) {
      return undefined;
    }
    const value = Number.parseFloat(match);
    return Number.isFinite(value) ? value : undefined;
  }

  private matchLastString(
    output: string,
    pattern: RegExp,
    formatter?: (match: RegExpExecArray) => string,
  ): string | undefined {
    let matched: string | undefined;
    let result: RegExpExecArray | null;
    pattern.lastIndex = 0;
    while ((result = pattern.exec(output)) !== null) {
      matched = formatter ? formatter(result) : result[1];
    }
    return matched;
  }

  private percentile(values: Array<number | undefined>, percentile: number): number | undefined {
    const filtered = values.filter((value): value is number => value !== undefined && Number.isFinite(value)).sort((a, b) => a - b);
    if (filtered.length === 0) {
      return undefined;
    }
    const rank = Math.max(0, Math.ceil(percentile * filtered.length) - 1);
    return filtered[Math.min(filtered.length - 1, rank)];
  }

  private minMetric(values: Array<number | undefined>): number | undefined {
    const filtered = values.filter((value): value is number => value !== undefined && Number.isFinite(value));
    return filtered.length > 0 ? Math.min(...filtered) : undefined;
  }

  private maxMetric(values: Array<number | undefined>): number | undefined {
    const filtered = values.filter((value): value is number => value !== undefined && Number.isFinite(value));
    return filtered.length > 0 ? Math.max(...filtered) : undefined;
  }

  private buildBenchmarkSummary(runs: BenchmarkRunMetrics[]): Record<string, unknown> {
    const lastRun = runs[runs.length - 1];
    return {
      totalMs: this.benchmarkMetricSummary(runs.map((run) => run.totalMs)),
      trackedServiceInstanceFetchMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceInstanceFetchMs)),
      trackedServicePluginServerMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServicePluginServerMs)),
      trackedServicePluginEncodeMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServicePluginEncodeMs)),
      trackedServicePayloadBytes: this.benchmarkMetricSummary(runs.map((run) => run.trackedServicePayloadBytes)),
      trackedServiceChunkCount: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceChunkCount)),
      trackedServiceMaxFrameMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceMaxFrameMs)),
      trackedServiceStallCountOver50Ms: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceStallCountOver50Ms)),
      trackedServiceStallCountOver100Ms: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceStallCountOver100Ms)),
      coreExportMs: this.benchmarkMetricSummary(runs.map((run) => run.coreExportMs)),
      bridgeStartupMs: this.benchmarkMetricSummary(runs.map((run) => run.bridgeStartupMs)),
      handshakeMs: this.benchmarkMetricSummary(runs.map((run) => run.handshakeMs)),
      serviceExportSumMs: this.benchmarkMetricSummary(runs.map((run) => run.serviceExportSumMs)),
      importCriticalTailMs: this.benchmarkMetricSummary(runs.map((run) => run.importCriticalTailMs)),
      unmeasuredOrSchedulerGapMs: this.benchmarkMetricSummary(runs.map((run) => run.unmeasuredOrSchedulerGapMs)),
      perService: this.benchmarkPerServiceSummary(runs),
      exportFingerprint: lastRun?.exportFingerprint,
      bridgeFingerprint: lastRun?.bridgeFingerprint,
    };
  }

  private benchmarkMetricSummary(values: Array<number | undefined>): {
    p50?: number;
    p90?: number;
    min?: number;
    max?: number;
  } {
    return {
      p50: this.percentile(values, 0.5),
      p90: this.percentile(values, 0.9),
      min: this.minMetric(values),
      max: this.maxMetric(values),
    };
  }

  private benchmarkPerServiceSummary(runs: BenchmarkRunMetrics[]): Record<string, Record<string, unknown>> {
    const byService = new Map<string, BenchmarkServiceMetrics[]>();
    for (const run of runs) {
      for (const metrics of run.serviceMetrics ?? []) {
        const serviceRuns = byService.get(metrics.service) ?? [];
        serviceRuns.push(metrics);
        byService.set(metrics.service, serviceRuns);
      }
    }

    const entries = Array.from(byService.entries()).sort((a, b) => {
      const aP50 = this.percentile(a[1].map((metrics) => metrics.instanceFetchMs), 0.5) ?? 0;
      const bP50 = this.percentile(b[1].map((metrics) => metrics.instanceFetchMs), 0.5) ?? 0;
      return bP50 - aP50;
    });
    const out: Record<string, Record<string, unknown>> = {};
    for (const [service, serviceRuns] of entries) {
      out[service] = {
        instanceFetchMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.instanceFetchMs)),
        pluginServerMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.pluginServerMs)),
        pluginEncodeMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.pluginEncodeMs)),
        payloadBytes: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.payloadBytes)),
        chunkCount: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.chunkCount)),
        maxFrameMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.maxFrameMs)),
        stallCountOver33Ms: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.stallCountOver33Ms)),
        stallCountOver50Ms: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.stallCountOver50Ms)),
        stallCountOver100Ms: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.stallCountOver100Ms)),
      };
    }
    return out;
  }

  private benchmarkServiceScore(metrics: BenchmarkServiceMetrics): number {
    return (
      (metrics.instanceFetchMs ?? 0) * 1_000_000 +
      (metrics.pluginServerMs ?? 0) * 10_000 +
      (metrics.pluginEncodeMs ?? 0) * 1_000 +
      (metrics.payloadBytes ?? 0)
    );
  }

  private logBenchmarkRun(prefix: string, metrics: BenchmarkRunMetrics): void {
    this.output.appendLine(
      `${prefix} total=${this.formatMetricMs(metrics.totalMs)} core_export=${this.formatMetricMs(metrics.coreExportMs)} bridge_startup=${this.formatMetricMs(metrics.bridgeStartupMs)} handshake=${this.formatMetricMs(metrics.handshakeMs)} service_export_sum=${this.formatMetricMs(metrics.serviceExportSumMs)} import_tail=${this.formatMetricMs(metrics.importCriticalTailMs)} gap=${this.formatMetricMs(metrics.unmeasuredOrSchedulerGapMs)} trackedService=${metrics.trackedService ?? "n/a"} fetch=${this.formatMetricMs(metrics.trackedServiceInstanceFetchMs)} pluginServer=${this.formatMetricMs(metrics.trackedServicePluginServerMs)} pluginEncode=${this.formatMetricMs(metrics.trackedServicePluginEncodeMs)} payload=${this.formatMetricBytes(metrics.trackedServicePayloadBytes)} chunks=${this.formatMetricInt(metrics.trackedServiceChunkCount)} maxFrame=${this.formatMetricMs(metrics.trackedServiceMaxFrameMs)} stalls50=${this.formatMetricInt(metrics.trackedServiceStallCountOver50Ms)} stalls100=${this.formatMetricInt(metrics.trackedServiceStallCountOver100Ms)}`,
    );
  }

  private summaryP50(summary: Record<string, unknown> | undefined, key: string): number | undefined {
    const metricSummary = summary?.[key];
    if (!metricSummary || typeof metricSummary !== "object" || !("p50" in metricSummary)) {
      return undefined;
    }
    const value = (metricSummary as { p50?: unknown }).p50;
    return typeof value === "number" && Number.isFinite(value) ? value : undefined;
  }

  private metricDelta(before: number | undefined, after: number | undefined): number | undefined {
    return before === undefined || after === undefined ? undefined : after - before;
  }

  private formatMetricMs(value: number | undefined): string {
    return value === undefined ? "n/a" : `${value.toFixed(1)}ms`;
  }

  private formatSignedMetricMs(value: number | undefined): string {
    if (value === undefined) {
      return "n/a";
    }
    return `${value >= 0 ? "+" : ""}${value.toFixed(1)}ms`;
  }

  private formatMetricBytes(value: number | undefined): string {
    return value === undefined ? "n/a" : `${Math.round(value)}B`;
  }

  private formatMetricInt(value: number | undefined): string {
    return value === undefined ? "n/a" : String(Math.round(value));
  }

  private summarizeBenchmarkOutput(output: string): string[] {
    const lines = output
      .replace(/\r\n/g, "\n")
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0);

    const interesting = lines.filter((line) =>
      line.includes("effective chunk size:") ||
      line.includes("prepared bridge_version=") ||
      line.includes("instance fetch") ||
      line.includes("script source fetch") ||
      line.includes("build service state") ||
      line.includes("settings binary collect") ||
      line.includes("settings binary write") ||
      line.includes("direct import worker total") ||
      line.includes("direct import dispatcher drain") ||
      line.includes("export start:")
    );

    return interesting.slice(-16);
  }

  private renderArgs(args: string[]): string {
    return args
      .map((arg) => {
        if (/\s/.test(arg) || arg.includes('"')) {
          return `"${arg.replaceAll('"', '\\"')}"`;
        }
        return arg;
      })
      .join(" ");
  }

  private updateStatusBar(): void {
    if (this.activeTaskName) {
      const elapsedSeconds = Math.max(0, Math.floor((Date.now() - this.activeTaskStartedAt) / 1000));
      this.statusItem.text = `$(sync~spin) Renium ${elapsedSeconds}s`;
      this.statusItem.tooltip = `${this.activeTaskName} in progress`;
      return;
    }

    const config = vscode.workspace.getConfiguration("renium");
    const autoEnabled = config.get<boolean>("autoSyncOnSave", false);
    const liveSyncEnabled = this.editorLiveSyncRuntimeEnabled;

    if (this.bridgeServeRequested && this.isBridgeDaemonRunning()) {
      this.statusItem.text = "$(radio-tower) Renium Serve";
      this.statusItem.tooltip = "Bridge server is running; Studio plugin can connect";
      return;
    }

    if (liveSyncEnabled && this.liveSyncWatcher) {
      this.statusItem.text = "$(sync~spin) Renium Live";
      this.statusItem.tooltip = "Live sync running";
      return;
    }

    if (autoEnabled) {
      this.statusItem.text = "$(sync) Renium Auto";
      this.statusItem.tooltip = "Auto sync on save is enabled";
      return;
    }

    this.statusItem.text = "$(sync) Renium";
    this.statusItem.tooltip = "Open Renium menu";
  }

  private setActiveTask(taskName: string | undefined): void {
    this.activeTaskName = taskName;
    this.activeTaskStartedAt = taskName ? Date.now() : 0;

    if (this.activeTaskTicker) {
      clearInterval(this.activeTaskTicker);
      this.activeTaskTicker = undefined;
    }

    if (taskName) {
      this.activeTaskTicker = setInterval(() => {
        this.updateStatusBar();
      }, 1000);
    }

    this.updateStatusBar();
  }

  private disposeLiveSyncRuntime(): void {
    this.stopStudioLiveSyncRuntime();
    if (this.liveSyncWatcher) {
      this.liveSyncWatcher.dispose();
      this.liveSyncWatcher = undefined;
    }
    if (this.liveSyncTimer) {
      clearTimeout(this.liveSyncTimer);
      this.liveSyncTimer = undefined;
      this.liveSyncTimerDueAt = 0;
    }
    this.pendingEditorPaths.clear();
    this.forcedEditorLiveSyncPathKeys.clear();
    this.suppressedEditorLiveSyncPathUntilByKey.clear();
    this.recentDirectSaveAtByPath.clear();
  }

  private async setEditorLiveSyncEnabled(enabled: boolean): Promise<void> {
    this.editorLiveSyncRuntimeEnabled = enabled;
    const cfg = vscode.workspace.getConfiguration("renium");
    if (cfg.get<boolean>("editorLiveSyncEnabled", false) !== enabled) {
      await cfg.update("editorLiveSyncEnabled", enabled, vscode.ConfigurationTarget.Workspace);
    }
    this.updateStatusBar();
  }
}

/**
 * Decorates renium-link mirror files in the VS Code Explorer and editor tabs.
 * Linked files are also OS read-only, so the editor shows the native lock too.
 */
class LinkDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri[] | undefined>();
  public readonly onDidChangeFileDecorations = this.emitter.event;
  private index = new Map<string, LinkFileInfo>();

  public constructor(private readonly controller: RobloxSyncController) {}

  public async refresh(): Promise<void> {
    try {
      this.index = await this.controller.getLinkFileIndex(true);
    } catch {
      this.index = new Map();
    }
    this.emitter.fire(undefined);
  }

  public provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== "file" || this.index.size === 0) {
      return undefined;
    }
    const info = this.index.get(this.controller.normalizeLinkPathKey(uri.fsPath));
    if (!info || info.broken) {
      return undefined;
    }
    if (info.drift) {
      return new vscode.FileDecoration(
        "L!",
        `Link → ${info.linkId} (edited — reverts to source on sync)`,
        new vscode.ThemeColor("gitDecoration.modifiedResourceForeground"),
      );
    }
    return new vscode.FileDecoration(
      "L",
      `Link → ${info.linkId} (read-only mirror)`,
      new vscode.ThemeColor("gitDecoration.submoduleResourceForeground"),
    );
  }
}

function relativeTimeLabel(unixMs?: number | null): string {
  if (!unixMs || unixMs <= 0) {
    return "unknown";
  }
  const diff = Date.now() - unixMs;
  if (diff < 45_000) {
    return "just now";
  }
  const minutes = Math.floor(diff / 60_000);
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours}h ago`;
  }
  const days = Math.floor(hours / 24);
  if (days < 30) {
    return `${days}d ago`;
  }
  const months = Math.floor(days / 30);
  if (months < 12) {
    return `${months}mo ago`;
  }
  return `${Math.floor(months / 12)}y ago`;
}

function escapeHtml(value: unknown): string {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function scriptJson(value: unknown): string {
  return (JSON.stringify(value) ?? "null")
    .replace(/</g, "\\u003c")
    .replace(/\u2028/g, "\\u2028")
    .replace(/\u2029/g, "\\u2029");
}

function packagePreviewHtml(preview: PackagePreviewData): string {
  return `<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>${escapeHtml(preview.name)}</title>
<style>
body{margin:0;background:var(--vscode-editor-background);color:var(--vscode-editor-foreground);font-family:var(--vscode-font-family);font-size:12px}
.toolbar{display:flex;gap:10px;align-items:center;padding:10px 12px;border-bottom:1px solid var(--vscode-panel-border);background:var(--vscode-sideBar-background)}
.title{font-weight:600;font-size:13px}.muted{color:var(--vscode-descriptionForeground)}
.layout{display:grid;grid-template-columns:minmax(220px,34%) 1fr;height:calc(100vh - 43px)}
.tree{border-right:1px solid var(--vscode-panel-border);overflow:auto;padding:8px}.details{overflow:auto;padding:12px}
.row{display:flex;align-items:center;gap:6px;padding:4px 6px;border-radius:3px;cursor:pointer;white-space:nowrap}
.row:hover{background:var(--vscode-list-hoverBackground)}.row.selected{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground)}
.twisty{width:12px;color:var(--vscode-descriptionForeground)}.name{font-weight:500}.class{color:var(--vscode-descriptionForeground)}
h2{font-size:15px;margin:0 0 4px}h3{font-size:12px;margin:18px 0 8px;text-transform:uppercase;color:var(--vscode-descriptionForeground);letter-spacing:.04em}
table{border-collapse:collapse;width:100%}td{border-top:1px solid var(--vscode-panel-border);padding:5px 6px;vertical-align:top}td:first-child{width:190px;color:var(--vscode-descriptionForeground)}
pre{margin:0;white-space:pre-wrap;word-break:break-word;background:var(--vscode-textCodeBlock-background);padding:10px;border-radius:4px;max-height:45vh;overflow:auto}
.empty{padding:16px;color:var(--vscode-descriptionForeground)}
</style>
</head>
<body>
<div class="toolbar"><span class="title">${escapeHtml(preview.name)}</span><span class="muted">${escapeHtml(preview.source ?? preview.sourcePath)}</span></div>
<div class="layout"><div id="tree" class="tree"></div><div id="details" class="details"></div></div>
<script>
const data=${scriptJson(preview)};
const tree=document.getElementById('tree');
const details=document.getElementById('details');
const byId=new Map();
const children=new Map();
for(const node of data.nodes||[]){
  const id=node.settingsId||node.name||Math.random().toString(36);
  node.__id=id; byId.set(id,node);
  const parent=node.parentId||'';
  if(!children.has(parent))children.set(parent,[]);
  children.get(parent).push(node);
}
let selected=(data.rootIds&&data.rootIds[0])||(data.nodes&&data.nodes[0]&&data.nodes[0].__id)||'';
function esc(value){return String(value??'').replace(/[&<>"]/g,ch=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[ch]))}
function rows(parent,depth){
  const list=children.get(parent)||[];
  let html='';
  for(const node of list){
    const kids=children.get(node.__id)||[];
    html+='<div class="row'+(selected===node.__id?' selected':'')+'" data-id="'+esc(node.__id)+'" style="padding-left:'+(depth*14+4)+'px">';
    html+='<span class="twisty">'+(kids.length?'v':'')+'</span><span class="name">'+esc(node.name||node.__id)+'</span><span class="class">'+esc(node.className||'')+'</span></div>';
    html+=rows(node.__id,depth+1);
  }
  return html;
}
function renderTree(){
  const roots=(data.rootIds||[]).filter(id=>byId.has(id));
  if(roots.length&&!children.has(''))children.set('',roots.map(id=>byId.get(id)));
  tree.innerHTML=rows('',0)||'<div class="empty">Package is empty.</div>';
}
function valueText(value){if(value===undefined)return ''; if(typeof value==='string')return value; try{return JSON.stringify(value,null,2)}catch(_){return String(value)}}
function propTable(title,obj){
  const entries=Object.entries(obj||{});
  if(!entries.length)return '';
  return '<h3>'+title+'</h3><table>'+entries.map(([k,v])=>'<tr><td>'+esc(k)+'</td><td><pre>'+esc(valueText(v))+'</pre></td></tr>').join('')+'</table>';
}
function renderDetails(){
  const node=byId.get(selected);
  if(!node){details.innerHTML='<div class="empty">Select an instance.</div>';return}
  details.innerHTML='<h2>'+esc(node.name||'Instance')+'</h2><div class="muted">'+esc(node.className||'')+' - '+esc(node.settingsId||'')+'</div>'
    +'<h3>Path</h3><pre>'+esc((node.pathSegments||[node.name]).join('.'))+'</pre>'
    +propTable('Properties',node.properties)
    +propTable('Attributes',node.attributes);
}
tree.addEventListener('click',event=>{const row=event.target.closest('.row'); if(!row)return; selected=row.dataset.id; renderTree(); renderDetails();});
renderTree(); renderDetails();
</script>
</body>
</html>`;
}

type PackageLinkElement = {
  kind: "link";
  link: CliLinkStatusLink;
  selectionVersion: number;
};

type PackageNodeElement = {
  kind: "node";
  link: CliLinkStatusLink;
  preview: PackagePreviewData;
  node: PackagePreviewNode;
  nodeKey: string;
  parentKey: string;
  childCount: number;
  selectionVersion: number;
};

type PackageTreeElement = PackageLinkElement | PackageNodeElement;

type PackagePreviewTree = {
  preview: PackagePreviewData;
  nodesByKey: Map<string, PackagePreviewNode>;
  childrenByParent: Map<string, PackageNodeElement[]>;
  roots: PackageNodeElement[];
};

function isPackageScriptClass(className: string | undefined): boolean {
  return className === "Script" || className === "LocalScript" || className === "ModuleScript";
}

function packageNodeKey(node: PackagePreviewNode, index = 0): string {
  const settingsId = String(node.settingsId ?? "").trim();
  if (settingsId.length > 0) {
    return settingsId;
  }
  const pathKey = Array.isArray(node.pathSegments) ? node.pathSegments.join("/") : "";
  return pathKey.length > 0 ? pathKey : `${String(node.name ?? "node")}:${index}`;
}

function packageNodeSource(node: PackagePreviewNode): string | undefined {
  const value = node.properties?.Source ?? node.properties?.source;
  return typeof value === "string" ? value : undefined;
}

function packageScriptFileName(node: PackagePreviewNode): string {
  const name = String(node.name ?? "Script").replace(/[<>:"/\\|?*\x00-\x1f]/g, "_");
  if (/\.(lua|luau)$/i.test(name)) {
    return name;
  }
  switch (node.className) {
    case "Script":
      return `${name}.server.luau`;
    case "LocalScript":
      return `${name}.client.luau`;
    default:
      return `${name}.luau`;
  }
}

function packageScriptUriInfo(uri: vscode.Uri): OpenPackageScriptTab | undefined {
  if (uri.scheme !== "renium-package") {
    return undefined;
  }
  const params = new URLSearchParams(uri.query);
  const linkId = params.get("link")?.trim() ?? "";
  const nodeKey = params.get("node")?.trim() ?? "";
  if (!linkId || !nodeKey) {
    return undefined;
  }
  return { linkId, nodeKey };
}

function packageDisplayPath(node: PackagePreviewNode): string {
  return Array.isArray(node.pathSegments) && node.pathSegments.length > 0
    ? node.pathSegments.join(".")
    : String(node.name ?? "Instance");
}

function packageValueText(value: unknown): string {
  if (value === undefined) {
    return "";
  }
  if (typeof value === "string") {
    return value;
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function packageTableHtml(title: string, object: Record<string, unknown> | undefined, omitKeys = new Set<string>()): string {
  const entries = Object.entries(object ?? {}).filter(([key]) => !omitKeys.has(key));
  if (entries.length === 0) {
    return "";
  }
  return `<h2>${escapeHtml(title)}</h2><table>${entries
    .map(([key, value]) => `<tr><td>${escapeHtml(key)}</td><td><pre>${escapeHtml(packageValueText(value))}</pre></td></tr>`)
    .join("")}</table>`;
}

function packagePropertiesHtml(preview: PackagePreviewData, node: PackagePreviewNode | undefined): string {
  const selected = node ? packageDisplayPath(node) : preview.name;
  const className = node?.className ?? preview.rootClass ?? "Package";
  const omit = new Set<string>(["Source", "source"]);
  return `<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
body{margin:0;background:var(--vscode-editor-background);color:var(--vscode-editor-foreground);font-family:var(--vscode-font-family);font-size:12px}
.head{padding:10px 12px;border-bottom:1px solid var(--vscode-panel-border);background:var(--vscode-sideBar-background)}
.title{font-size:13px;font-weight:700;letter-spacing:.05em}.tag{display:inline-block;margin-left:8px;border:1px solid var(--vscode-focusBorder);border-radius:999px;padding:1px 7px;color:var(--vscode-focusBorder)}
.meta{margin-top:4px;color:var(--vscode-descriptionForeground)}
.body{padding:12px}h2{font-size:12px;margin:16px 0 8px;text-transform:uppercase;color:var(--vscode-descriptionForeground);letter-spacing:.04em}
table{border-collapse:collapse;width:100%}td{border-top:1px solid var(--vscode-panel-border);padding:5px 6px;vertical-align:top}td:first-child{width:190px;color:var(--vscode-descriptionForeground)}
pre{margin:0;white-space:pre-wrap;word-break:break-word;background:var(--vscode-textCodeBlock-background);padding:8px;border-radius:4px}
</style>
</head>
<body>
<div class="head"><span class="title">PROPERTIES</span><span class="tag">${escapeHtml(preview.name)}</span><div class="meta">${escapeHtml(selected)}  -  ${escapeHtml(className)}</div></div>
<div class="body">
<h2>Package</h2>
<table>
<tr><td>Package</td><td><pre>${escapeHtml(preview.name)}</pre></td></tr>
<tr><td>Source</td><td><pre>${escapeHtml(preview.source ?? preview.sourcePath)}</pre></td></tr>
${node ? `<tr><td>Path</td><td><pre>${escapeHtml(packageDisplayPath(node))}</pre></td></tr>` : ""}
</table>
${packageTableHtml("Properties", node?.properties, omit)}
${packageTableHtml("Attributes", node?.attributes)}
</div>
</body>
</html>`;
}

class PackageScriptContentProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly contents = new Map<string, string>();
  private readonly changeEmitter = new vscode.EventEmitter<vscode.Uri>();
  public readonly onDidChange = this.changeEmitter.event;

  public constructor(private readonly resolveContent?: (linkId: string, nodeKey: string) => Promise<string | undefined>) {}

  public provideTextDocumentContent(uri: vscode.Uri): vscode.ProviderResult<string> {
    const key = uri.toString();
    const cached = this.contents.get(key);
    if (cached !== undefined) {
      return cached;
    }
    const info = packageScriptUriInfo(uri);
    if (!info || !this.resolveContent) {
      return "";
    }
    return this.resolveContent(info.linkId, info.nodeKey).then(
      (source) => {
        const text = source ?? "";
        this.contents.set(key, text);
        return text;
      },
      () => "",
    );
  }

  public uriFor(preview: PackagePreviewData, node: PackagePreviewNode): vscode.Uri {
    const source = packageNodeSource(node) ?? "";
    const nodeKey = packageNodeKey(node);
    const hash = crypto.createHash("sha1").update(`${preview.id}\0${nodeKey}\0${source}`).digest("hex").slice(0, 12);
    const packageName = preview.name.replace(/[<>:"/\\|?*\x00-\x1f]/g, "_") || "package";
    const query = new URLSearchParams({
      link: preview.id,
      node: nodeKey,
      v: hash,
    });
    const uri = vscode.Uri.from({
      scheme: "renium-package",
      authority: "preview",
      path: `/${encodeURIComponent(packageName)}/${encodeURIComponent(packageScriptFileName(node))}`,
      query: query.toString(),
    });
    this.contents.set(uri.toString(), source);
    this.changeEmitter.fire(uri);
    return uri;
  }

  public dispose(): void {
    this.contents.clear();
    this.changeEmitter.dispose();
  }
}

class PackageScriptDecorationProvider implements vscode.FileDecorationProvider {
  public provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== "renium-package" && uri.scheme !== "renium-readonly-script") {
      return undefined;
    }
    return new vscode.FileDecoration(
      undefined,
      uri.scheme === "renium-package"
        ? "Renium package script preview (read-only)"
        : "Renium linked script preview (read-only)",
      new vscode.ThemeColor("renium.packagePreviewForeground"),
    );
  }
}

/** A browsable, draggable tree of available renium-link packages. */
class PackagesTreeProvider implements vscode.TreeDataProvider<PackageTreeElement>, vscode.TreeDragAndDropController<PackageTreeElement>, vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<PackageTreeElement | undefined | null | void>();
  public readonly onDidChangeTreeData = this.changeEmitter.event;
  public readonly dragMimeTypes = [RENIUM_PACKAGE_DRAG_MIME, "text/plain"];
  public readonly dropMimeTypes: string[] = [];
  private readonly iconNames: ReadonlySet<string>;
  private readonly previewCache = new Map<string, Promise<PackagePreviewTree>>();
  private readonly expandedLinkIds = new Set<string>();
  private readonly expandedNodeKeys = new Map<string, Set<string>>();
  private clearDragTimer: NodeJS.Timeout | undefined;
  private propertiesPanel: vscode.WebviewPanel | undefined;
  private selectionGeneration = 0;
  private suppressExpansionTracking = false;

  public constructor(
    private readonly controller: RobloxSyncController,
    private readonly extensionUri: vscode.Uri,
    private readonly scriptContentProvider: PackageScriptContentProvider,
    private readonly setPackageDrag: (link?: { id: string; name?: string; mode?: string }) => void,
  ) {
    this.iconNames = new Set(loadAssetIconNames(extensionUri));
  }

  public dispose(): void {
    this.changeEmitter.dispose();
    if (this.clearDragTimer) {
      clearTimeout(this.clearDragTimer);
      this.clearDragTimer = undefined;
    }
    this.setPackageDrag(undefined);
  }

  public refresh(): void {
    this.previewCache.clear();
    this.changeEmitter.fire();
  }

  public clearSelection(_element: PackageTreeElement | undefined): void {
    this.selectionGeneration += 1;
    this.suppressExpansionTracking = true;
    this.changeEmitter.fire();
  }

  public isCurrentElement(element: PackageTreeElement | undefined): element is PackageTreeElement {
    return !!element && element.selectionVersion === this.selectionGeneration;
  }

  private linkSelectionVersion(link: CliLinkStatusLink): number {
    return this.selectionGeneration;
  }

  private nodeSelectionVersion(link: CliLinkStatusLink, nodeKey: string): number {
    return this.selectionGeneration;
  }

  public noteExpansion(element: PackageTreeElement, expanded: boolean): void {
    if (this.suppressExpansionTracking) {
      return;
    }
    const linkId = String(element.link.id ?? "").trim();
    if (!linkId) {
      return;
    }
    if (element.kind === "link") {
      if (expanded) {
        this.expandedLinkIds.add(linkId);
      } else {
        this.expandedLinkIds.delete(linkId);
        this.expandedNodeKeys.delete(linkId);
      }
      return;
    }
    const nodeKeys = this.expandedNodeKeys.get(linkId) ?? new Set<string>();
    if (expanded) {
      nodeKeys.add(element.nodeKey);
      this.expandedNodeKeys.set(linkId, nodeKeys);
    } else {
      nodeKeys.delete(element.nodeKey);
      if (nodeKeys.size === 0) {
        this.expandedNodeKeys.delete(linkId);
      }
    }
  }

  public async restoreExpansion(treeView: vscode.TreeView<PackageTreeElement>): Promise<void> {
    try {
      const linkIds = Array.from(this.expandedLinkIds);
      for (const linkId of linkIds) {
        const linkElement = await this.elementForLinkId(linkId);
        if (linkElement) {
          await treeView.reveal(linkElement, { expand: true, select: false, focus: false });
        }
      }
      const nodeElements: PackageNodeElement[] = [];
      for (const [linkId, nodeKeys] of this.expandedNodeKeys) {
        for (const nodeKey of nodeKeys) {
          const nodeElement = await this.elementForNodeKey(linkId, nodeKey);
          if (nodeElement) {
            nodeElements.push(nodeElement);
          }
        }
      }
      nodeElements.sort((a, b) => (a.node.pathSegments?.length ?? 0) - (b.node.pathSegments?.length ?? 0));
      for (const nodeElement of nodeElements) {
        await treeView.reveal(nodeElement, { expand: true, select: false, focus: false });
      }
    } finally {
      setTimeout(() => {
        this.suppressExpansionTracking = false;
      }, 0);
    }
  }

  public linkFromElement(element: PackageTreeElement | CliLinkStatusLink | string | undefined): CliLinkStatusLink | string | undefined {
    if (!element || typeof element === "string") {
      return element;
    }
    if ("kind" in element) {
      return element.link;
    }
    return element;
  }

  public async handleDrag(source: readonly PackageTreeElement[], dataTransfer: vscode.DataTransfer): Promise<void> {
    const link = source.find((candidate): candidate is PackageLinkElement => candidate.kind === "link")?.link;
    if (!link?.id) {
      logPackageDragDebug("packages.handleDrag: no link id in drag source");
      return;
    }
    const name = (link.rootName && link.rootName.length > 0 ? link.rootName : link.id) ?? link.id;
    const payload = JSON.stringify({ type: "renium-package", id: link.id, name });
    dataTransfer.set(RENIUM_PACKAGE_DRAG_MIME, new vscode.DataTransferItem(payload));
    dataTransfer.set("text/plain", new vscode.DataTransferItem(`${RENIUM_PACKAGE_TEXT_PREFIX}${payload}`));
    logPackageDragDebug(`packages.handleDrag: armed ${link.id} name=${name}`);
    this.setPackageDrag({ id: link.id, name, mode: "drag" });
    if (this.clearDragTimer) {
      clearTimeout(this.clearDragTimer);
    }
    this.clearDragTimer = setTimeout(() => {
      this.clearDragTimer = undefined;
      logPackageDragDebug(`packages.handleDrag: cleared ${link.id} after timeout`);
      this.setPackageDrag(undefined);
    }, 30_000);
  }

  public getTreeItem(element: PackageTreeElement): vscode.TreeItem {
    if (element.kind === "node") {
      return this.getPackageNodeTreeItem(element);
    }
    const link = element.link;
    const name = (link.rootName && link.rootName.length > 0 ? link.rootName : link.id) ?? "link";
    const hasVisibleChildren = typeof link.instances === "number" ? link.instances > 1 : true;
    const item = new vscode.TreeItem(
      name,
      hasVisibleChildren ? vscode.TreeItemCollapsibleState.Collapsed : vscode.TreeItemCollapsibleState.None,
    );
    const uses = link.targetCount ?? 0;
    item.description = `${relativeTimeLabel(link.updatedUnixMs)}${uses > 0 ? `  -  ${uses} use${uses === 1 ? "" : "s"}` : ""}`;

    const rootClass = link.rootClass && link.rootClass.length > 0 ? link.rootClass : "";
    if (rootClass) {
      const iconUri = vscode.Uri.joinPath(this.extensionUri, "assets", `${iconAssetNameForClass(rootClass, this.iconNames)}.png`);
      item.iconPath = fs.existsSync(iconUri.fsPath) ? iconUri : new vscode.ThemeIcon("package");
    } else {
      item.iconPath = new vscode.ThemeIcon("package");
    }

    const tooltip = new vscode.MarkdownString(undefined, true);
    tooltip.appendMarkdown(`**${name}**${rootClass ? `  \`${rootClass}\`` : ""}\n\n`);
    tooltip.appendMarkdown(`- Source: \`${link.source ?? "?"}\`\n`);
    if (typeof link.instances === "number" && link.instances > 0) {
      tooltip.appendMarkdown(`- Instances: ${link.instances}\n`);
    }
    tooltip.appendMarkdown(`- Used in: ${uses} place${uses === 1 ? "" : "s"}\n`);
    tooltip.appendMarkdown(`- ${link.readOnly === false ? "Writable" : "Read-only"}\n`);
    tooltip.appendMarkdown(`- Last edited: ${link.updatedUnixMs ? new Date(link.updatedUnixMs).toLocaleString() : "unknown"}`);
    item.tooltip = tooltip;

    item.contextValue = "reniumPackage";
    item.id = `package:${link.id ?? name}:${this.linkSelectionVersion(link)}`;
    item.command = {
      command: "renium.packages.openItem",
      title: "Open Package",
      arguments: [element],
    };
    return item;
  }

  private getPackageNodeTreeItem(element: PackageNodeElement): vscode.TreeItem {
    const node = element.node;
    const label = String(node.name ?? node.settingsId ?? "Instance");
    const item = new vscode.TreeItem(
      label,
      element.childCount > 0 ? vscode.TreeItemCollapsibleState.Collapsed : vscode.TreeItemCollapsibleState.None,
    );
    const className = String(node.className ?? "");
    item.description = className && className !== label ? className : undefined;
    item.tooltip = new vscode.MarkdownString(
      `**${label}**${className ? `  \`${className}\`` : ""}\n\nPackage: \`${element.preview.name}\`\n\nPath: \`${packageDisplayPath(node)}\``,
      true,
    );
    const iconUri = vscode.Uri.joinPath(this.extensionUri, "assets", `${iconAssetNameForClass(className || "Folder", this.iconNames)}.png`);
    item.iconPath = fs.existsSync(iconUri.fsPath) ? iconUri : new vscode.ThemeIcon(isPackageScriptClass(className) ? "symbol-method" : "symbol-class");
    item.contextValue = isPackageScriptClass(className) ? "reniumPackageNode.script" : "reniumPackageNode";
    item.id = `package-node:${element.link.id}:${element.nodeKey}:${this.nodeSelectionVersion(element.link, element.nodeKey)}`;
    item.command = {
      command: "renium.packages.openItem",
      title: isPackageScriptClass(className) ? "Open Package Script" : "Show Package Properties",
      arguments: [element],
    };
    return item;
  }

  public async getChildren(element?: PackageTreeElement): Promise<PackageTreeElement[]> {
    if (element?.kind === "node") {
      const tree = await this.previewTree(element.link);
      return (tree.childrenByParent.get(element.nodeKey) ?? []).map((child) => this.currentPackageElement(child) as PackageNodeElement);
    }
    if (element?.kind === "link") {
      const tree = await this.previewTree(element.link);
      if (tree.roots.length === 1) {
        return (tree.childrenByParent.get(tree.roots[0].nodeKey) ?? [])
          .map((child) => this.currentPackageElement(child) as PackageNodeElement);
      }
      return tree.roots.map((root) => this.currentPackageElement(root) as PackageNodeElement);
    }
    const links = await this.controller.getLinkPackages(false);
    return [...links]
      .sort((a, b) => (b.updatedUnixMs ?? 0) - (a.updatedUnixMs ?? 0))
      .map((link) => this.linkElement(link));
  }

  public async getParent(element: PackageTreeElement): Promise<PackageTreeElement | undefined> {
    if (element.kind === "link") {
      return undefined;
    }
    if (!element.parentKey) {
      return this.linkElement(element.link);
    }
    const tree = await this.previewTree(element.link);
    if (tree.roots.length === 1 && element.parentKey === tree.roots[0].nodeKey) {
      return this.linkElement(element.link);
    }
    return this.elementForKey(element.link, tree.preview, tree.nodesByKey, tree.childrenByParent, element.parentKey);
  }

  private linkElement(link: CliLinkStatusLink): PackageLinkElement {
    return { kind: "link", link, selectionVersion: this.linkSelectionVersion(link) };
  }

  private currentPackageElement(element: PackageTreeElement): PackageTreeElement {
    if (element.kind === "link") {
      return this.linkElement(element.link);
    }
    return { ...element, selectionVersion: this.nodeSelectionVersion(element.link, element.nodeKey) };
  }

  private async elementForLinkId(linkId: string): Promise<PackageLinkElement | undefined> {
    const links = await this.controller.getLinkPackages(false);
    const link = links.find((candidate) => String(candidate.id ?? "").trim() === linkId);
    return link ? this.linkElement(link) : undefined;
  }

  private async elementForNodeKey(linkId: string, nodeKey: string): Promise<PackageNodeElement | undefined> {
    const linkElement = await this.elementForLinkId(linkId);
    if (!linkElement) {
      return undefined;
    }
    const tree = await this.previewTree(linkElement.link);
    return this.elementForKey(linkElement.link, tree.preview, tree.nodesByKey, tree.childrenByParent, nodeKey);
  }

  public async packageScriptSourceFor(linkId: string, nodeKey: string): Promise<string | undefined> {
    const linkElement = await this.elementForLinkId(linkId);
    if (!linkElement) {
      return undefined;
    }
    const tree = await this.previewTree(linkElement.link);
    const node = tree.nodesByKey.get(nodeKey);
    if (!node || !isPackageScriptClass(node.className)) {
      return undefined;
    }
    return packageNodeSource(node);
  }

  public async openPackageScriptByKey(
    linkId: string,
    nodeKey: string,
    options: { preview?: boolean; preserveFocus?: boolean } = {},
  ): Promise<boolean> {
    const linkElement = await this.elementForLinkId(linkId);
    if (!linkElement) {
      return false;
    }
    const tree = await this.previewTree(linkElement.link);
    const node = tree.nodesByKey.get(nodeKey);
    if (!node) {
      return false;
    }
    return this.openPackageScript(tree.preview, node, options);
  }

  private previewTree(link: CliLinkStatusLink): Promise<PackagePreviewTree> {
    const id = String(link.id ?? "");
    const existing = this.previewCache.get(id);
    if (existing) {
      return existing;
    }
    const loading = this.controller.loadPackagePreview(link).then((preview) => {
      const nodesByKey = new Map<string, PackagePreviewNode>();
      const childrenByParent = new Map<string, PackageNodeElement[]>();
      const keyByRawId = new Map<string, string>();
      preview.nodes.forEach((node, index) => {
        const key = packageNodeKey(node, index);
        nodesByKey.set(key, node);
        if (node.settingsId) {
          keyByRawId.set(node.settingsId, key);
        }
      });
      preview.nodes.forEach((node, index) => {
        const key = packageNodeKey(node, index);
        const parentKey = node.parentId ? (keyByRawId.get(node.parentId) ?? node.parentId) : "";
        const childCount = preview.nodes.filter((candidate) => candidate.parentId === node.settingsId).length;
        const element: PackageNodeElement = {
          kind: "node",
          link,
          preview,
          node,
          nodeKey: key,
          parentKey,
          childCount,
          selectionVersion: this.nodeSelectionVersion(link, key),
        };
        const bucket = childrenByParent.get(parentKey) ?? [];
        bucket.push(element);
        childrenByParent.set(parentKey, bucket);
      });
      const roots = (preview.rootIds.length > 0
        ? preview.rootIds.map((rootId) => keyByRawId.get(rootId) ?? rootId)
        : Array.from(childrenByParent.get("") ?? []).map((root) => root.nodeKey))
        .map((key) => (childrenByParent.get("") ?? []).find((candidate) => candidate.nodeKey === key)
          ?? this.elementForKey(link, preview, nodesByKey, childrenByParent, key))
        .filter((element): element is PackageNodeElement => !!element);
      return { preview, nodesByKey, childrenByParent, roots };
    });
    this.previewCache.set(id, loading);
    return loading;
  }

  private elementForKey(
    link: CliLinkStatusLink,
    preview: PackagePreviewData,
    nodesByKey: Map<string, PackagePreviewNode>,
    childrenByParent: Map<string, PackageNodeElement[]>,
    key: string,
  ): PackageNodeElement | undefined {
    const node = nodesByKey.get(key);
    if (!node) {
      return undefined;
    }
    const childCount = childrenByParent.get(key)?.length ?? 0;
    const parentKey = node.parentId
      ? (Array.from(nodesByKey.entries()).find(([, candidate]) => candidate.settingsId === node.parentId)?.[0] ?? node.parentId)
      : "";
    return {
      kind: "node",
      link,
      preview,
      node,
      nodeKey: key,
      parentKey,
      childCount,
      selectionVersion: this.nodeSelectionVersion(link, key),
    };
  }

  public async openItem(element: PackageTreeElement | undefined): Promise<void> {
    if (!this.isCurrentElement(element)) {
      return;
    }
    if (element.kind === "link") {
      const tree = await this.previewTree(element.link);
      if (!this.isCurrentElement(element)) {
        return;
      }
      const root = tree.roots[0];
      await this.showPackageProperties(tree.preview, root?.node);
      if (root && tree.roots.length === 1 && await this.openPackageScript(tree.preview, root.node)) {
        return;
      }
      return;
    }
    await this.showPackageProperties(element.preview, element.node);
    if (await this.openPackageScript(element.preview, element.node)) {
      return;
    }
  }

  private normalizedTargetPath(target: CliLinkStatusTarget): string[] {
    const service = String(target.service ?? "").trim();
    const pathSegments = Array.isArray(target.path)
      ? target.path.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service) {
      return pathSegments;
    }
    return pathSegments[0] === service ? pathSegments : [service, ...pathSegments];
  }

  private pathStartsWith(pathSegments: readonly string[], prefix: readonly string[]): boolean {
    return pathSegments.length >= prefix.length
      && prefix.every((segment, index) => pathSegments[index] === segment);
  }

  private packageNodeRelativePath(node: PackagePreviewNode): string[] {
    const pathSegments = Array.isArray(node.pathSegments) && node.pathSegments.length > 0
      ? node.pathSegments.map((segment) => String(segment))
      : [String(node.name ?? "Instance")];
    return pathSegments.length > 1 ? pathSegments.slice(1) : [];
  }

  private packageNodeMatchesExplorerRequest(
    node: PackagePreviewNode,
    relativePath: readonly string[],
    request: LinkedPackageScriptPreviewRequest,
  ): boolean {
    if (!isPackageScriptClass(node.className)) {
      return false;
    }
    const nodeRelativePath = this.packageNodeRelativePath(node);
    if (nodeRelativePath.length !== relativePath.length
      || !nodeRelativePath.every((segment, index) => segment === relativePath[index])) {
      return false;
    }
    if (request.className && node.className && request.className !== node.className) {
      return false;
    }
    if (relativePath.length > 0 && request.name && node.name && request.name !== node.name) {
      return false;
    }
    return true;
  }

  public async openLinkedScriptPreview(request: LinkedPackageScriptPreviewRequest | undefined): Promise<boolean> {
    const service = typeof request?.service === "string" ? request.service.trim() : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length < 2) {
      return false;
    }
    const normalizedPath = pathSegments[0] === service ? pathSegments : [service, ...pathSegments];
    const status = await this.controller.getLinkStatus(false);
    const targets = (status?.targets ?? [])
      .filter((target) =>
        target.broken !== true
        && target.missing !== true
        && target.resolved !== false
        && target.service === service
        && typeof target.linkId === "string"
        && target.linkId.length > 0,
      )
      .map((target) => ({ target, targetPath: this.normalizedTargetPath(target) }))
      .filter(({ targetPath }) => this.pathStartsWith(normalizedPath, targetPath))
      .sort((a, b) => b.targetPath.length - a.targetPath.length);
    if (targets.length === 0) {
      return false;
    }
    const packages = await this.controller.getLinkPackages(false);
    for (const { target, targetPath } of targets) {
      const link = packages.find((candidate) => candidate.id === target.linkId);
      if (!link) {
        continue;
      }
      const relativePath = normalizedPath.slice(targetPath.length);
      const tree = await this.previewTree(link);
      const node = Array.from(tree.nodesByKey.values()).find((candidate) =>
        this.packageNodeMatchesExplorerRequest(candidate, relativePath, request ?? {}),
      );
      if (node && await this.openPackageScript(tree.preview, node)) {
        return true;
      }
    }
    return false;
  }

  public async showSelection(element: PackageTreeElement | undefined): Promise<void> {
    if (!this.isCurrentElement(element)) {
      return;
    }
    if (element.kind === "link") {
      const tree = await this.previewTree(element.link);
      if (!this.isCurrentElement(element)) {
        return;
      }
      await this.showPackageProperties(tree.preview, tree.roots[0]?.node);
      return;
    }
    await this.showPackageProperties(element.preview, element.node);
  }

  private async showPackageProperties(preview: PackagePreviewData, node: PackagePreviewNode | undefined): Promise<void> {
    await vscode.commands.executeCommand("renium.properties.showPackageNode", {
      packageId: preview.id,
      packageName: preview.name,
      source: preview.source,
      sourcePath: preview.sourcePath,
      rootClass: preview.rootClass,
      rootName: preview.rootName,
      node,
    });
  }

  private async openPackageScript(
    preview: PackagePreviewData,
    node: PackagePreviewNode,
    options: { preview?: boolean; preserveFocus?: boolean } = {},
  ): Promise<boolean> {
    if (!isPackageScriptClass(node.className)) {
      return false;
    }
    const source = packageNodeSource(node);
    if (source === undefined) {
      return false;
    }
    const uri = this.scriptContentProvider.uriFor(preview, node);
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc, {
      preview: options.preview ?? false,
      preserveFocus: options.preserveFocus,
    });
    try {
      await vscode.languages.setTextDocumentLanguage(doc, "luau");
    } catch {
    }
    return true;
  }
}

export function activate(context: vscode.ExtensionContext): void {
  const controller = new RobloxSyncController(context);
  setTimeout(() => {
    try {
      const cli = controller.rbsyncViewerCli();
      const extensionVersion = String(context.extension.packageJSON.version ?? "");
      if (cli && extensionVersion && fs.existsSync(cli.cliPath)) {
        const cliVersion = rustCliVersion(cli.cliPath);
        if (cliVersion && cliVersion !== extensionVersion) {
          void vscode.window.showWarningMessage(
            `Renium: this extension is v${extensionVersion} but ${RUST_CLI_BINARY} is v${cliVersion}. Update whichever is older so they match — syncing may misbehave until then.`,
          );
        }
      }
    } catch {
    }
  }, 0);
  const fileExplorerController = new FileExplorerController(context, controller.gitViewActions());
  const linkDecorationProvider = new LinkDecorationProvider(controller);
  let packagesProvider: PackagesTreeProvider;
  const packageScriptProvider = new PackageScriptContentProvider((linkId, nodeKey) =>
    packagesProvider.packageScriptSourceFor(linkId, nodeKey),
  );
  const packageScriptDecorationProvider = new PackageScriptDecorationProvider();
  packagesProvider = new PackagesTreeProvider(
    controller,
    context.extensionUri,
    packageScriptProvider,
    (link) => fileExplorerController.setExternalPackageDrag(link),
  );
  const packagesTreeView = vscode.window.createTreeView("renium.packages", {
    treeDataProvider: packagesProvider,
    dragAndDropController: packagesProvider,
    showCollapseAll: false,
  });
  const clearPackageTreeSelection = (): void => {
    const selectedPackage = packagesTreeView.selection[0];
    if (!selectedPackage) {
      return;
    }
    void (async () => {
      try {
        await vscode.commands.executeCommand("list.clear");
      } catch {
      }
      setTimeout(() => {
        if (packagesTreeView.selection.length === 0) {
          return;
        }
        packagesProvider.clearSelection(selectedPackage);
        setTimeout(() => {
          void packagesProvider.restoreExpansion(packagesTreeView);
        }, 0);
      }, 0);
    })();
  };
  const packageTabKey = (tab: OpenPackageScriptTab): string => `${tab.linkId}\u0000${tab.nodeKey}`;
  const tabInputUris = (input: unknown): vscode.Uri[] => {
    const candidate = input as { uri?: unknown; original?: unknown; modified?: unknown };
    const uris: vscode.Uri[] = [];
    for (const value of [candidate.uri, candidate.original, candidate.modified]) {
      if (value instanceof vscode.Uri) {
        uris.push(value);
      }
    }
    return uris;
  };
  const openPackageScriptTabs = (): OpenPackageScriptTab[] => {
    const tabs = new Map<string, OpenPackageScriptTab>();
    for (const group of vscode.window.tabGroups.all) {
      for (const tab of group.tabs) {
        for (const uri of tabInputUris(tab.input)) {
          const info = packageScriptUriInfo(uri);
          if (info) {
            tabs.set(packageTabKey(info), info);
          }
        }
      }
    }
    return Array.from(tabs.values());
  };
  const persistOpenPackageScriptTabs = (): void => {
    void context.workspaceState.update(RENIUM_OPEN_PACKAGE_SCRIPT_TABS_STATE_KEY, openPackageScriptTabs());
  };
  const restoreOpenPackageScriptTabs = async (): Promise<void> => {
    const raw = context.workspaceState.get<unknown>(RENIUM_OPEN_PACKAGE_SCRIPT_TABS_STATE_KEY, []);
    const saved = (Array.isArray(raw) ? raw : [])
      .map((value): OpenPackageScriptTab | undefined => {
        const record = value as { linkId?: unknown; nodeKey?: unknown };
        const linkId = typeof record.linkId === "string" ? record.linkId.trim() : "";
        const nodeKey = typeof record.nodeKey === "string" ? record.nodeKey.trim() : "";
        return linkId && nodeKey ? { linkId, nodeKey } : undefined;
      })
      .filter((value): value is OpenPackageScriptTab => !!value);
    if (saved.length === 0) {
      return;
    }
    const alreadyOpen = new Set(openPackageScriptTabs().map(packageTabKey));
    for (const tab of saved) {
      if (alreadyOpen.has(packageTabKey(tab))) {
        continue;
      }
      await packagesProvider.openPackageScriptByKey(tab.linkId, tab.nodeKey, {
        preview: false,
        preserveFocus: true,
      });
    }
    persistOpenPackageScriptTabs();
  };

  const resolveViewerCli = () => controller.rbsyncViewerCli();
  context.subscriptions.push(
    controller,
    fileExplorerController,
    vscode.window.registerCustomEditorProvider(
      RbsyncEditorProvider.viewType,
      new RbsyncEditorProvider(
        context.extensionUri,
        resolveViewerCli,
        (node) => fileExplorerController.showRbsyncPropertiesReadonly(node),
      ),
      { supportsMultipleEditorsPerDocument: true, webviewOptions: { retainContextWhenHidden: true } },
    ),
    vscode.commands.registerCommand("renium.openMenu", () => controller.openMenu()),
    vscode.commands.registerCommand("renium.installStudioPlugin", () => controller.installStudioPlugin()),
    vscode.commands.registerCommand("renium.openExplorer", () => vscode.commands.executeCommand("workbench.view.extension.reniumContainer")),
    vscode.commands.registerCommand("renium.gitSync", () => controller.openGitSync()),
    vscode.commands.registerCommand("renium.gitSync.status", () => controller.gitStatus()),
    vscode.commands.registerCommand("renium.gitSync.fetch", () => controller.gitFetch()),
    vscode.commands.registerCommand("renium.gitSync.pull", () => controller.gitPull()),
    vscode.commands.registerCommand("renium.gitSync.commitAndPush", () => controller.gitCommitAndPush()),
    vscode.commands.registerCommand("renium.gitSync.fullSyncAndPush", () => controller.gitCommitAndPush({ runFullSyncFirst: true })),
    vscode.commands.registerCommand("renium.gitSync.connectRepo", () => controller.gitConnectRepo()),
    vscode.commands.registerCommand("renium.gitSync.publishBranch", () => controller.gitPublishBranch()),
    vscode.commands.registerCommand("renium.gitSync.createBranch", () => controller.gitCreateBranch()),
    vscode.commands.registerCommand("renium.gitSync.checkoutBranch", () => controller.gitCheckoutBranch()),
    vscode.commands.registerCommand("renium.gitSync.openRemote", () => controller.gitOpenRemote()),
    vscode.commands.registerCommand("renium.fullSync", () => controller.fullSync()),
    vscode.commands.registerCommand("renium.exportSnapshots", () => controller.exportSnapshotsOnly()),
    vscode.commands.registerCommand("renium.exportGameFile", () => controller.exportGameFile()),
    vscode.commands.registerCommand("renium.syncWallyPackages", () => controller.syncWallyPackages()),
    vscode.commands.registerCommand("renium.link.apply", () => controller.linkApply()),
    vscode.commands.registerCommand("renium.link.add", () => controller.addLinkInteractive()),
    vscode.commands.registerCommand("renium.link.status", () => controller.showLinkStatus()),
    vscode.commands.registerCommand("renium.link.break", (uri?: vscode.Uri) => controller.breakLinkForFile(uri)),
    vscode.commands.registerCommand("renium.link.revealSource", (uri?: vscode.Uri) => controller.revealLinkSourceForFile(uri)),
    vscode.commands.registerCommand("renium.link.addFromFile", (uri?: vscode.Uri) => controller.addLinkFromFile(uri)),
    vscode.commands.registerCommand("renium.link.packInstance", (request: { service?: string; pathSegments?: string[]; id?: string; resave?: boolean }) =>
      controller.packInstanceLink(request),
    ),
    vscode.commands.registerCommand("renium.link.resavePackage", (request: { service?: string; pathSegments?: string[] }) =>
      controller.resavePackageLink(request),
    ),
    vscode.commands.registerCommand("renium.link.relinkPackage", (request: { service?: string; pathSegments?: string[] }) =>
      controller.relinkPackageTarget(request),
    ),
    vscode.commands.registerCommand("renium.link.breakInstance", (request: { service?: string; pathSegments?: string[]; silent?: boolean; refreshExplorer?: boolean }) =>
      controller.breakInstanceLink(request),
    ),
    vscode.window.registerFileDecorationProvider(linkDecorationProvider),
    vscode.window.registerFileDecorationProvider(packageScriptDecorationProvider),
    packageScriptProvider,
    vscode.workspace.registerTextDocumentContentProvider("renium-package", packageScriptProvider),
    packagesProvider,
    packagesTreeView,
    packagesTreeView.onDidExpandElement((event) => {
      packagesProvider.noteExpansion(event.element, true);
    }),
    packagesTreeView.onDidCollapseElement((event) => {
      packagesProvider.noteExpansion(event.element, false);
    }),
    packagesTreeView.onDidChangeSelection((event) => {
      const selected = event.selection[0];
      if (!packagesProvider.isCurrentElement(selected)) {
        return;
      }
      fileExplorerController.clearExplorerSelection();
      void packagesProvider.showSelection(selected);
    }),
    fileExplorerController.onDidSelectExplorerNode(() => {
      clearPackageTreeSelection();
    }),
    vscode.commands.registerCommand("renium.packages.refresh", () => packagesProvider.refresh()),
    vscode.commands.registerCommand("renium.packages.openItem", (item?: PackageTreeElement) => packagesProvider.openItem(item)),
    vscode.commands.registerCommand("renium.packages.armInsert", (linkId?: string, name?: string) => {
      const id = String(linkId ?? "").trim();
      if (!id) {
        return;
      }
      fileExplorerController.setExternalPackageDrag({
        id,
        name: typeof name === "string" && name.length > 0 ? name : id,
        mode: "armed",
      });
      logPackageDragDebug(`packages.armInsert: armed ${id} name=${typeof name === "string" ? name : ""}`);
      void vscode.commands.executeCommand("workbench.view.extension.reniumContainer");
    }),
    vscode.commands.registerCommand("renium.packages.insertAtPath", (request?: { linkId?: string; service?: string; pathSegments?: string[] }) =>
      controller.insertPackageAtPath(request ?? {}),
    ),
    vscode.commands.registerCommand("renium.packages.viewUses", (link?: PackageTreeElement | CliLinkStatusLink | string) =>
      controller.viewPackageUses(packagesProvider.linkFromElement(link)),
    ),
    vscode.commands.registerCommand("renium.packages.delete", (link?: PackageTreeElement | CliLinkStatusLink | string) =>
      controller.deletePackage(packagesProvider.linkFromElement(link)),
    ),
    vscode.commands.registerCommand("renium.packages.openLinkedScriptPreview", (request?: LinkedPackageScriptPreviewRequest) =>
      packagesProvider.openLinkedScriptPreview(request),
    ),
    vscode.window.tabGroups.onDidChangeTabs(() => {
      persistOpenPackageScriptTabs();
    }),
    controller.onLinksChanged(() => {
      void linkDecorationProvider.refresh();
      void controller.pushLinkStateToExplorer();
      packagesProvider.refresh();
    }),
    vscode.commands.registerCommand("renium.startLiveSync", () => controller.startLiveSync()),
    vscode.commands.registerCommand("renium.stopLiveSync", () => controller.stopLiveSync()),
    vscode.commands.registerCommand("renium.retryEditorInitialSync", () => controller.retryEditorInitialSync()),
    vscode.commands.registerCommand("renium.serve", () => controller.serve()),
    vscode.commands.registerCommand("renium.stopServe", () => controller.stopServe()),
    vscode.commands.registerCommand("renium.pushEditorPathsNow", (paths: string[] | string, options?: EditorPushOptions) =>
      controller.pushEditorPathsNow(paths, options),
    ),
    vscode.commands.registerCommand("renium.pushEditorPropertyNow", (request: EditorPropertyPushRequest) =>
      controller.pushEditorPropertyNow(request),
    ),
    vscode.commands.registerCommand("renium.pushEditorDeleteNow", (request: EditorDeletePushRequest) =>
      controller.pushEditorDeleteNow(request),
    ),
    vscode.commands.registerCommand("renium.noteProgrammaticEditorWrite", (request: ProgrammaticEditorWriteRequest) =>
      controller.noteProgrammaticEditorWrite(request),
    ),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      void controller.onDocumentSaved(doc);
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("renium")) {
        controller.onConfigurationChanged(event);
      }
    }),
  );

  const linkManifestWatcher = vscode.workspace.createFileSystemWatcher("**/renium-link.json");
  let linkApplyTimer: NodeJS.Timeout | undefined;
  const onLinkManifestChanged = (): void => {
    controller.invalidateLinkStatusCache();
    const linkCfg = vscode.workspace.getConfiguration("renium");
    if (linkCfg.get<boolean>("link.autoApplyOnManifestChange", false) === true) {
      if (linkApplyTimer) {
        clearTimeout(linkApplyTimer);
      }
      linkApplyTimer = setTimeout(() => {
        void controller.linkApply({ silent: true }).catch(() => undefined);
      }, 1500);
    }
  };
  context.subscriptions.push(
    linkManifestWatcher,
    linkManifestWatcher.onDidChange(onLinkManifestChanged),
    linkManifestWatcher.onDidCreate(onLinkManifestChanged),
  );

  const linkPackageWatchers = [
    vscode.workspace.createFileSystemWatcher("**/*.rbsync"),
    vscode.workspace.createFileSystemWatcher("**/*.renium"),
  ];
  for (const watcher of linkPackageWatchers) {
    context.subscriptions.push(
      watcher,
      watcher.onDidChange((uri) => controller.onLinkPackageSourceChanged(uri)),
      watcher.onDidCreate((uri) => controller.onLinkPackageSourceChanged(uri)),
      watcher.onDidDelete((uri) => controller.onLinkPackageSourceChanged(uri)),
    );
  }

  void linkDecorationProvider.refresh();
  void controller.pushLinkStateToExplorer();
  controller.scheduleStartupLinkRefresh();
  setTimeout(() => {
    void restoreOpenPackageScriptTabs().catch(() => undefined);
  }, 500);

  const cfg = vscode.workspace.getConfiguration("renium");
  if (cfg.get<boolean>("editorLiveSyncEnabled", false) === true || cfg.get<boolean>("editorLiveSyncOnStartup", false) === true) {
    void controller.startLiveSync({ silent: true, bestEffort: true });
  }
}

export function deactivate(): void {
}
