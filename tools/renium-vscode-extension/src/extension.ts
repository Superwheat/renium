import * as childProcess from "child_process";
import * as crypto from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { URLSearchParams } from "url";
import * as vscode from "vscode";
import {
  ensureReniumAgentInstructions,
  isReniumProjectRoot,
} from "./agentInstructions";
import {
  bundledReniumCliPath,
  reniumCliCandidates,
} from "./cliResolution";
import { mergeAndResolve, sameSourceText, withLineEnding, type ConflictPolicy } from "./conflictMerge";
import { changedEditorLiveSyncPaths } from "./editorLiveSyncCache";
import {
  projectProcessOwner,
  terminateAllProcesses,
  terminateProcess,
  terminateProcessesForOwner,
  trackProcess,
} from "./processSupervisor";
import {
  ExperienceManifest,
  ExperiencePlace,
  activeExperienceAlias,
  experiencePlaceAliasesInOrder,
  normalizePlaceAlias,
  normalizePublishedPlaceName,
  readExperienceManifest,
  resolveActiveExperiencePlace,
  resolveExperiencePlaceRoot,
  setActiveExperiencePlace,
  uniquePlaceAlias,
  writeExperienceManifest,
} from "./experience";
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
  nameStatusAffectedPaths,
  parseAheadBehind,
  parseNameStatusZ,
  parsePorcelainV1Z,
  redactRemoteUrl,
  remoteUrlToWebUrl,
  renderGitArgs,
  runGit,
  shouldPullFromStudioBeforePush,
  summarizeStatus,
} from "./gitSync";
import { GitViewActions, GitViewState } from "./gitView";
import { pickWorkspaceRoot } from "./utils";
import { DEFAULT_SYNC_SERVICES } from "./serviceDefaults";
import { RbsyncEditorProvider } from "./rbsyncViewer";
import { isRobloxModel, reniumPluginReleaseUrl } from "./pluginDistribution";
import {
  ProjectSourceGraph,
  SharedConfig,
  inferProjectScriptIdentity,
  invalidateProjectSourceGraph,
  loadProjectSourceGraph,
  loadProjectSourceRoot,
  loadProjectSourceLocations,
  loadSharedConfig,
  sharedConfigValue,
} from "./sharedConfig";
import { AUTOMATION_OP } from "./automationProtocol.generated";

const RENIUM_PACKAGE_DRAG_MIME = "application/vnd.renium.package";
const RENIUM_PACKAGE_TEXT_PREFIX = "renium-package:";
const RENIUM_OPEN_PACKAGE_SCRIPT_TABS_STATE_KEY = "renium.openPackageScriptTabs";
const RENIUM_ACTIVE_EXPERIENCE_PLACES_STATE_KEY = "renium.activeExperiencePlaces";
const RENIUM_PLACE_PROJECT_FILES = [
  "renium.project.jsonc",
  "renium.project.json",
  "sourcemap.json",
  "renium-link.json",
  "wally.toml",
  "wally.lock",
  "Packages",
  "snapshots",
  ".renium",
] as const;
const RENIUM_SINGLE_PLACE_FILES = [
  "renium.project.jsonc",
  "renium.project.json",
  "sourcemap.json",
  "renium-link.json",
  ".renium",
] as const;

function reniumPlaceProjectPaths(root: string): string[] {
  return [...new Set([loadProjectSourceRoot(root), ...RENIUM_PLACE_PROJECT_FILES])];
}

function hasSinglePlaceProject(root: string): boolean {
  return [loadProjectSourceRoot(root), ...RENIUM_SINGLE_PLACE_FILES]
    .some((name) => fs.existsSync(path.join(root, name)));
}

type GitSyncConfig = {
  gitPath: string;
  remote: string;
  branch: string;
  autoFetch: boolean;
  pullFromStudioBeforePush: "ask" | "always" | "never";
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
  serverPackagesDir: string;
  serverTargetService: string;
  serverTargetName: string;
  devPackagesDir: string;
  devTargetService: string;
  devTargetName: string;
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
  rustCliPath: string;
  experienceRoot: string;
  projectRoot: string;
  srcDir: string;
  activePlaceAlias?: string;
  activePlace?: ExperiencePlace;
  placeSelector?: string;
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
  result?: unknown;
  automationError?: AutomationError;
};

type CapturedLocalScriptState = {
  content?: string;
  base?: string;
};

type BridgeClientInfo = {
  runtimeId?: string;
  role?: string;
  placeId?: number;
  gameId?: number;
  placeName?: string;
};

type ConnectedStudioPlace = {
  runtimeId?: string;
  placeId: number;
  gameId?: number;
  placeName: string;
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
  projectRoot?: string;
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

type LinkStatusResolution =
  | { kind: "success"; value: CliLinkStatusResult }
  | { kind: "missing" }
  | { kind: "failed"; error: string };

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
  worktreeEntries: GitStatusEntry[];
  repoRoot?: string;
  branch?: string;
  upstream?: string;
  remote?: string;
  remoteUrl?: string;
  ahead: number;
  behind: number;
};

type GitProjectToken = {
  projectRoot: string;
  generation: number;
};

type StudioChangeState = {
  ok?: boolean;
  tracking?: boolean;
  role?: string;
  seq?: number;
  runtimeId?: string;
  dirtyServices?: string[];
  fullSyncServices?: string[];
  propertyChanges?: StudioPropertyChange[];
  editorActions?: StudioEditorAction[];
  changes?: StudioChangeLog[];
  trackedServices?: number;
  itemChangedAvailable?: boolean;
  eventDriven?: boolean;
  waitSeconds?: number;
  waitTimedOut?: boolean;
  twoWaySyncEnabled?: boolean;
  runtimeSettings?: Record<string, unknown>;
  explicitRuntimeSettings?: Record<string, unknown>;
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
  settingsId?: string;
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
  fullSync?: boolean;
  projectRoot?: string;
  pendingServices?: string[];
  verifySources?: boolean;
  skipChangeFilter?: boolean;
  taskName?: string;
  targetSettingsId?: string;
  targetSettingsIds?: string[];
  targetProperty?: string;
  targetProperties?: string[];
  upsertInstancesOnly?: boolean;
};

type ExperienceChangeSnapshot = {
  alias?: string;
  projectRoot?: string;
  pendingEditorPaths: string[];
  blockedStudioImportServices: string[];
  pendingEditorServicesByPath: Array<[string, string[]]>;
  studioSnapshotFingerprintByService: Array<[string, string]>;
  pendingLinkPackageSourcePaths: Array<[string, { projectRoot: string; generation: number }]>;
};

type ProjectRootConfigurationSnapshot = {
  globalValue?: string;
  workspaceValue?: string;
  workspaceFolderValue?: string;
};

type EditorPushOutcome = "applied" | "skipped";

type EditorPropertyPushRequest = {
  force?: boolean;
  projectRoot?: string;
  settingsFile?: string;
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  scope?: "metadata" | "property" | "attribute";
  property?: string;
  value?: unknown;
  changedPaths?: string[];
};

type StudioEditorAction = {
  id?: string;
  type?: string;
  service?: string;
  settingsId?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
};

type EditorDeletePushRequest = {
  force?: boolean;
  projectRoot?: string;
  settingsFile?: string;
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  changedPaths?: string[];
};

type ProgrammaticEditorWriteRequest = {
  paths?: string[] | string;
  durationMs?: number;
  refreshCache?: boolean;
  forcePending?: boolean;
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

type AutomationError = {
  c: string;
  m: string;
  rt: 0 | 1;
  n?: string;
  d?: unknown;
};

type AutomationResponse = {
  v: 1;
  id: number;
  ok: 0 | 1;
  ms: number;
  r?: unknown;
  e?: AutomationError;
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

const DEFAULT_BRIDGE_PORTS = [8781, 8782];
const PREVIOUS_DEFAULT_BRIDGE_PORTS = [8781, 8782, 8783];
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

function existingReniumSettingsFile(projectRoot: string, srcDir: string, service: string): string {
  const serviceDir = path.join(projectRoot, srcDir, service);
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

function resolveExistingRustCliPath(
  workspaceRoot: string,
  projectRoot: string,
  configuredPath: string,
  extensionRoot: string,
): string {
  const roots = Array.from(new Set([workspaceRoot, projectRoot].map((value) => path.normalize(value))));
  const candidates = reniumCliCandidates({
    configuredPath,
    extensionRoot,
    roots,
    fallbackRelativePaths: RUST_CLI_FALLBACK_RELATIVE_PATHS,
  });
  const existing = candidates.find((candidate) => fs.existsSync(candidate));
  return existing ?? (configuredPath || bundledReniumCliPath(extensionRoot));
}

class RobloxSyncController {
  private readonly output: vscode.OutputChannel;
  private readonly statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 200);
  private queue: Promise<void> = Promise.resolve();
  private liveSyncWatcher: vscode.FileSystemWatcher | undefined;
  private liveSyncAdditionalWatchers: vscode.FileSystemWatcher[] = [];
  private liveSyncTimer: NodeJS.Timeout | undefined;
  private liveSyncTimerDueAt = 0;
  private liveSyncGraphRefreshTimer: NodeJS.Timeout | undefined;
  private liveSyncGraphRefreshPending = false;
  private liveSyncGraphRefreshRunning = false;
  private liveSyncProjectRoot: string | undefined;
  private studioLiveSyncTimer: NodeJS.Timeout | undefined;
  private readonly studioLiveSyncInFlightGenerations = new Set<number>();
  private studioLiveSyncGeneration = 0;
  private readonly studioImportTasks = new Map<number, Set<Promise<unknown>>>();
  private studioActionPollTimer: NodeJS.Timeout | undefined;
  private studioActionPollInFlight = false;
  private changePreviewPanel: vscode.WebviewPanel | undefined;
  private changePreviewResolve: ((decision: "apply" | "full" | "discard" | "pending") => void) | undefined;
  private pendingStudioReviewKey: string | undefined;
  private forcedStudioReviewKey: string | undefined;
  private changePreviewIconNames: ReadonlySet<string> | undefined;
  private studioLiveSyncStarted = false;
  private studioLiveSyncNextPollMs = DEFAULT_STUDIO_LIVE_SYNC_POLL_MS;
  private studioToEditorImportInProgress = false;
  private studioToEditorImportSuppressUntilMs = 0;
  private studioToEditorLastSyncEndedAt = 0;
  private studioSnapshotFingerprintByService = new Map<string, string>();
  private editorLiveSyncRuntimeEnabled = false;
  private pendingEditorPaths = new Set<string>();
  private blockedStudioImportServices = new Set<string>();
  private pendingEditorServicesByPath = new Map<string, Set<string>>();
  private pendingEditorPersistence: Promise<void> = Promise.resolve();
  private editorLiveSyncCacheWrites: Promise<void> = Promise.resolve();
  private editorPushFailureStreak = 0;
  private forcedEditorLiveSyncPathKeys = new Set<string>();
  private suppressedEditorLiveSyncPathUntilByKey = new Map<string, number>();
  private recentDirectSaveAtByPath = new Map<string, number>();
  private studioConflictPolicyOverride: ConflictPolicy | undefined;
  private studioRuntimeSettings: Record<string, unknown> | undefined;
  private conflictMarkerWarnedKeys = new Set<string>();
  private linkStatusCache: {
    at: number;
    projectRoot: string;
    generation: number;
    token: number;
    value: CliLinkStatusResult | undefined;
  } | undefined;
  private linkStatusInflight: {
    projectRoot: string;
    generation: number;
    token: number;
    promise: Promise<CliLinkStatusResult | undefined>;
  } | undefined;
  private linkStatusToken = 0;
  private linkPackageSourceApplyTimer: NodeJS.Timeout | undefined;
  private readonly pendingLinkPackageSourcePaths = new Map<string, { projectRoot: string; generation: number }>();
  private linkPackageSourceWatchers: vscode.Disposable[] = [];
  private readonly activeLinkPackageSourceKeys = new Set<string>();
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
  private daemonClosePromise: Promise<void> | undefined;
  private daemonStopPromise: Promise<void> | undefined;
  private daemonPending = new Map<number, DaemonPendingRequest>();
  private daemonContext: { key: string; id: number } | undefined;
  private studioEditorActionRuns = new Map<string, { done: boolean; error?: string }>();
  private publishedPlaceNames = new Map<number, string>();
  private publishedRootPlaceIds = new Map<number, number>();
  private bridgeServeRequested = false;
  private liveSyncOwnsServe = false;
  private liveSyncStartPromise: Promise<void> | undefined;
  private liveSyncStartupInProgress = false;
  private liveSyncStopRequested = false;
  private autoSyncTimer: NodeJS.Timeout | undefined;
  private pendingAutoServices = new Set<string>();
  private activeTaskName: string | undefined;
  private gitViewRefreshSuppression = 0;
  private experienceChangeInProgress = false;
  private experienceGeneration = 0;
  private configuredProjectRoot: string | undefined;
  private projectRootConfigurationSnapshot: ProjectRootConfigurationSnapshot;
  private configurationChangeQueue: Promise<void> = Promise.resolve();
  private warnedLegacyStartupWaitSeconds = false;
  private warnedMultiRootWorkspace = false;
  private warnedLegacyBridgePorts = false;
  private warnedBridgePortLimit = false;
  private warnedLegacyChunkSize = false;
  private warnedChunkSizeCap = false;
  private sourcemapCache: SourcemapCache | undefined;
  private readonly consoleOutput = vscode.window.createOutputChannel("Renium Console");
  private consoleFollowTimer: NodeJS.Timeout | undefined;
  private consoleFollowRunning = false;
  private consoleFollowGeneration = 0;
  private consoleFollowInFlightGeneration: number | undefined;
  private consoleFollowSeq = 0;
  private consoleFollowEpoch: string | undefined;
  private consoleFollowFromOldest = false;
  private consoleFollowOwnsServe = false;
  private displayedLiveSyncPrompt = false;
  private luauSourcemapQueue: Promise<void> = Promise.resolve();
  private sharedConfig: SharedConfig = {};

  public constructor(private readonly context: vscode.ExtensionContext) {
    const output = vscode.window.createOutputChannel("Renium");
    const appendLine = output.appendLine.bind(output);
    output.appendLine = (value: string): void => {
      if (this.shouldWriteOutput(this.outputLevel(value))) {
        appendLine(value);
      }
    };
    this.output = output;
    this.restoreActiveExperiencePlace();
    const initialConfig = this.getConfig();
    this.configuredProjectRoot = initialConfig.projectRoot;
    this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
    this.ensureAgentInstructions(initialConfig.experienceRoot);
    this.restorePendingEditorPaths();
    void this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
    this.statusItem.command = "renium.openMenu";
    this.statusItem.show();
    this.updateStatusBar();
  }

  private ensureAgentInstructions(projectRoot: string): void {
    try {
      for (const filePath of ensureReniumAgentInstructions(
        this.context.extensionPath,
        projectRoot,
      )) {
        this.output.appendLine(`[renium] created ${filePath}`);
      }
    } catch (error) {
      this.output.appendLine(
        `[renium] could not create agent instructions: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  private captureProjectRootConfiguration(): ProjectRootConfigurationSnapshot {
    const root = this.getWorkspaceRoot();
    const inspected = vscode.workspace
      .getConfiguration("renium", vscode.Uri.file(root))
      .inspect<string>("projectRoot");
    return {
      globalValue: inspected?.globalValue,
      workspaceValue: inspected?.workspaceValue,
      workspaceFolderValue: inspected?.workspaceFolderValue,
    };
  }

  private async restoreProjectRootConfiguration(snapshot: ProjectRootConfigurationSnapshot): Promise<void> {
    const root = this.getWorkspaceRoot();
    const configuration = vscode.workspace.getConfiguration("renium", vscode.Uri.file(root));
    const current = this.captureProjectRootConfiguration();
    const updates: Array<[keyof ProjectRootConfigurationSnapshot, vscode.ConfigurationTarget]> = [
      ["globalValue", vscode.ConfigurationTarget.Global],
      ["workspaceValue", vscode.ConfigurationTarget.Workspace],
      ["workspaceFolderValue", vscode.ConfigurationTarget.WorkspaceFolder],
    ];
    for (const [key, target] of updates) {
      if (current[key] !== snapshot[key]) {
        await configuration.update("projectRoot", snapshot[key], target);
      }
    }
    this.projectRootConfigurationSnapshot = snapshot;
  }

  public createAgentInstructions(): void {
    const cfg = this.getConfig();
    if (!isReniumProjectRoot(cfg.experienceRoot)) {
      throw new Error("Open a Renium project before creating agent instructions.");
    }
    const before = ["AGENTS.md", "CLAUDE.md"].filter((name) =>
      fs.existsSync(path.join(cfg.experienceRoot, name))
    ).length;
    this.ensureAgentInstructions(cfg.experienceRoot);
    const after = ["AGENTS.md", "CLAUDE.md"].filter((name) =>
      fs.existsSync(path.join(cfg.experienceRoot, name))
    ).length;
    vscode.window.showInformationMessage(
      after > before
        ? "Created Renium agent instructions."
        : "Renium agent instructions already exist.",
    );
  }

  private isManagedLuauSourcemapSetting(
    value: string,
    workspaceRoot: string,
    experienceRoot: string,
  ): boolean {
    const absolute = path.resolve(workspaceRoot, value);
    const relative = path.relative(experienceRoot, absolute).replaceAll(path.sep, "/");
    return relative === "sourcemap.json" ||
      /^places\/[^/]+\/sourcemap\.json$/i.test(relative);
  }

  private async configureLuauSourcemap(
    cfg: SyncConfig,
    projectRoot: string,
  ): Promise<void> {
    if (
      !vscode.extensions.getExtension("JohnnyMorganz.luau-lsp") ||
      !isReniumProjectRoot(cfg.experienceRoot)
    ) {
      return;
    }
    const folder = vscode.workspace.getWorkspaceFolder(vscode.Uri.file(cfg.experienceRoot));
    if (!folder) {
      return;
    }
    const relative = path.relative(
      folder.uri.fsPath,
      path.join(projectRoot, "sourcemap.json"),
    );
    if (!relative || relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
      return;
    }
    const sourcemapFile = relative.replaceAll(path.sep, "/");
    const luau = vscode.workspace.getConfiguration("luau-lsp.sourcemap", folder.uri);
    const sourcemapInspection = luau.inspect<string>("sourcemapFile");
    const workspaceSourcemap =
      sourcemapInspection?.workspaceFolderValue ??
      sourcemapInspection?.workspaceValue;
    const globalSourcemap = sourcemapInspection?.globalValue;
    if (
      (workspaceSourcemap ?? globalSourcemap) &&
      (workspaceSourcemap ?? globalSourcemap) !== sourcemapFile &&
      !this.isManagedLuauSourcemapSetting(
        workspaceSourcemap ?? globalSourcemap ?? "",
        folder.uri.fsPath,
        cfg.experienceRoot,
      )
    ) {
      this.output.appendLine(
        `[renium] keeping custom Luau sourcemap setting ${workspaceSourcemap ?? globalSourcemap}`,
      );
      return;
    }
    const autogenerateInspection = luau.inspect<boolean>("autogenerate");
    const workspaceAutogenerate =
      autogenerateInspection?.workspaceFolderValue ??
      autogenerateInspection?.workspaceValue;
    const globalAutogenerate = autogenerateInspection?.globalValue;
    const generatorInspection = luau.inspect<string>("generatorCommand");
    const workspaceGenerator =
      generatorInspection?.workspaceFolderValue ??
      generatorInspection?.workspaceValue;
    const globalGenerator = generatorInspection?.globalValue;
    if (
      workspaceAutogenerate === true ||
      globalAutogenerate === true ||
      workspaceGenerator?.trim() ||
      globalGenerator?.trim()
    ) {
      this.output.appendLine("[renium] keeping custom Luau sourcemap generation settings");
      return;
    }
    try {
      const enabledInspection = luau.inspect<boolean>("enabled");
      const workspaceEnabled =
        enabledInspection?.workspaceFolderValue ??
        enabledInspection?.workspaceValue;
      const globalEnabled = enabledInspection?.globalValue;
      const includeNonScriptsInspection = luau.inspect<boolean>("includeNonScripts");
      const workspaceIncludeNonScripts =
        includeNonScriptsInspection?.workspaceFolderValue ??
        includeNonScriptsInspection?.workspaceValue;
      const globalIncludeNonScripts = includeNonScriptsInspection?.globalValue;
      if (workspaceEnabled === undefined && globalEnabled === undefined) {
        await luau.update(
          "enabled",
          true,
          vscode.ConfigurationTarget.WorkspaceFolder,
        );
      }
      if (workspaceAutogenerate === undefined && globalAutogenerate === undefined) {
        await luau.update(
          "autogenerate",
          false,
          vscode.ConfigurationTarget.WorkspaceFolder,
        );
      }
      if (
        workspaceIncludeNonScripts === undefined &&
        globalIncludeNonScripts === undefined
      ) {
        await luau.update(
          "includeNonScripts",
          true,
          vscode.ConfigurationTarget.WorkspaceFolder,
        );
      }
      if (workspaceSourcemap !== sourcemapFile && globalSourcemap === undefined) {
        await luau.update(
          "sourcemapFile",
          sourcemapFile,
          vscode.ConfigurationTarget.WorkspaceFolder,
        );
      }
    } catch (error) {
      this.output.appendLine(
        `[renium] could not configure Luau sourcemap: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  public configureLuauSourcemapForEditor(
    editor: vscode.TextEditor | undefined,
  ): Promise<void> {
    const update = async (): Promise<void> => {
      try {
        const cfg = this.getConfig();
        let projectRoot = cfg.projectRoot;
        if (editor?.document.uri.scheme === "file") {
          const manifest = readExperienceManifest(cfg.experienceRoot);
          if (manifest) {
            for (const place of Object.values(manifest.places)) {
              const placeRoot = resolveExperiencePlaceRoot(cfg.experienceRoot, place.root);
              if (this.isPathInside(editor.document.uri.fsPath, placeRoot)) {
                projectRoot = placeRoot;
                break;
              }
            }
          }
        }
        await this.configureLuauSourcemap(cfg, projectRoot);
      } catch (error) {
        this.output.appendLine(
          `[renium] could not select Luau sourcemap: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    };
    const queued = this.luauSourcemapQueue.then(update, update);
    this.luauSourcemapQueue = queued.catch(() => {});
    return queued;
  }

  private configuredExperienceRoot(): string | undefined {
    const workspaceRoot = pickWorkspaceRoot();
    if (!workspaceRoot) {
      return undefined;
    }
    const cfg = vscode.workspace.getConfiguration("renium", vscode.Uri.file(workspaceRoot));
    return this.resolveConfigPath(cfg.get<string>("projectRoot", "${workspaceFolder}"), workspaceRoot);
  }

  private experienceStateKey(experienceRoot: string): string {
    const resolved = path.resolve(experienceRoot);
    return process.platform === "win32" ? resolved.toLowerCase() : resolved;
  }

  private restoreActiveExperiencePlace(
    experienceRoot = this.configuredExperienceRoot(),
  ): void {
    if (!experienceRoot) {
      return;
    }
    const stored = this.context.workspaceState.get<Record<string, string>>(
      RENIUM_ACTIVE_EXPERIENCE_PLACES_STATE_KEY,
      {},
    );
    setActiveExperiencePlace(experienceRoot, stored[this.experienceStateKey(experienceRoot)]);
    try {
      const active = resolveActiveExperiencePlace(experienceRoot);
      if (active) {
        setActiveExperiencePlace(experienceRoot, active.alias);
      }
    } catch {
    }
  }

  private async persistActiveExperiencePlace(experienceRoot: string, alias: string): Promise<void> {
    setActiveExperiencePlace(experienceRoot, alias);
    const stored = {
      ...this.context.workspaceState.get<Record<string, string>>(
        RENIUM_ACTIVE_EXPERIENCE_PLACES_STATE_KEY,
        {},
      ),
      [this.experienceStateKey(experienceRoot)]: alias,
    };
    try {
      await this.context.workspaceState.update(RENIUM_ACTIVE_EXPERIENCE_PLACES_STATE_KEY, stored);
    } catch (error) {
      throw new Error(
        `Could not save the active place selection. ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  private experiencePlaceByAlias(experienceRoot: string, alias: string) {
    const manifest = readExperienceManifest(experienceRoot);
    const place = manifest?.places[alias];
    if (!manifest || !place) {
      throw new Error(`Place alias '${alias}' is not configured.`);
    }
    const selector = manifest.gameId > 0 && place.placeId > 0
      ? `${manifest.gameId}:${place.placeId}`
      : place.placeId > 0
        ? String(place.placeId)
        : place.name;
    return {
      alias,
      manifest,
      place,
      projectRoot: resolveExperiencePlaceRoot(experienceRoot, place.root),
      selector,
    };
  }

  private captureExperienceChange(experienceRoot: string): ExperienceChangeSnapshot {
    return {
      alias: activeExperienceAlias(experienceRoot),
      projectRoot: this.configuredProjectRoot,
      pendingEditorPaths: [...this.pendingEditorPaths],
      blockedStudioImportServices: [...this.blockedStudioImportServices],
      pendingEditorServicesByPath: [...this.pendingEditorServicesByPath]
        .map(([filePath, services]) => [filePath, [...services]]),
      studioSnapshotFingerprintByService: [...this.studioSnapshotFingerprintByService],
      pendingLinkPackageSourcePaths: [...this.pendingLinkPackageSourcePaths],
    };
  }

  private async rollbackExperienceChange(
    experienceRoot: string,
    snapshot: ExperienceChangeSnapshot,
    resumeLiveSync: boolean,
  ): Promise<void> {
    setActiveExperiencePlace(experienceRoot, snapshot.alias);
    this.configuredProjectRoot = snapshot.projectRoot;
    this.pendingEditorPaths = new Set(snapshot.pendingEditorPaths);
    this.blockedStudioImportServices = new Set(snapshot.blockedStudioImportServices);
    this.pendingEditorServicesByPath = new Map(
      snapshot.pendingEditorServicesByPath.map(([filePath, services]) => [filePath, new Set(services)]),
    );
    this.studioSnapshotFingerprintByService = new Map(snapshot.studioSnapshotFingerprintByService);
    this.pendingLinkPackageSourcePaths.clear();
    for (const [filePath, pending] of snapshot.pendingLinkPackageSourcePaths) {
      this.pendingLinkPackageSourcePaths.set(filePath, {
        projectRoot: snapshot.projectRoot ?? pending.projectRoot,
        generation: this.experienceGeneration,
      });
    }
    this.liveSyncProjectRoot = snapshot.projectRoot;
    this.sourcemapCache = undefined;
    this.studioRuntimeSettings = undefined;
    this.studioConflictPolicyOverride = undefined;
    this.linkStatusCache = undefined;
    this.linkStatusInflight = undefined;
    try {
      await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
      await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
    } finally {
      this.experienceChangeInProgress = false;
    }
    this.linkChangeEmitter.fire();
    this.updateStatusBar();
    if (this.bridgeServeRequested && !this.isBridgeDaemonRunning()) {
      await this.serve({ silent: true, bestEffort: true });
    }
    if (resumeLiveSync) {
      await this.startLiveSync({ silent: true, bestEffort: true });
    }
    if (this.pendingLinkPackageSourcePaths.size > 0 && snapshot.projectRoot) {
      this.scheduleLinkPackageSourceFlush(snapshot.projectRoot, this.experienceGeneration);
    }
  }

  private async prepareExperienceChange(): Promise<boolean> {
    if (this.experienceChangeInProgress) {
      throw new Error("Another place change is already in progress.");
    }
    if (this.activeTaskName) {
      throw new Error(`Wait for ${this.activeTaskName} to finish before changing places.`);
    }
    const resumeLiveSync = !!(
      this.liveSyncWatcher ||
      this.liveSyncStartPromise ||
      this.editorLiveSyncRuntimeEnabled
    );
    const previousProjectRoot = this.configuredProjectRoot;
    this.experienceGeneration += 1;
    this.experienceChangeInProgress = true;
    try {
      await vscode.commands.executeCommand("renium.fileExplorer.prepareProjectSwitch");
      if (resumeLiveSync) {
        await this.stopLiveSync({ silent: true });
      }
      await this.persistPendingEditorPaths();
      this.pendingEditorPaths.clear();
      this.blockedStudioImportServices.clear();
      this.pendingEditorServicesByPath.clear();
      if (this.activeTaskName) {
        throw new Error(`Wait for ${this.activeTaskName} to finish before changing places.`);
      }
      if (this.autoSyncTimer) {
        clearTimeout(this.autoSyncTimer);
        this.autoSyncTimer = undefined;
      }
      if (this.linkPackageSourceApplyTimer) {
        clearTimeout(this.linkPackageSourceApplyTimer);
        this.linkPackageSourceApplyTimer = undefined;
      }
      this.pendingAutoServices.clear();
      await this.stopConsoleFollow();
      if (previousProjectRoot) {
        await terminateProcessesForOwner(projectProcessOwner(previousProjectRoot));
      }
      await this.stopBridgeDaemon();
      return resumeLiveSync;
    } catch (error) {
      await vscode.commands.executeCommand("renium.fileExplorer.cancelProjectSwitch");
      this.restorePendingEditorPaths();
      this.experienceChangeInProgress = false;
      throw error;
    }
  }

  private async finishExperienceChange(experienceRoot: string, alias: string): Promise<void> {
    const active = this.experiencePlaceByAlias(experienceRoot, alias);
    setActiveExperiencePlace(experienceRoot, alias);
    this.configuredProjectRoot = active.projectRoot;
    fs.mkdirSync(active.projectRoot, { recursive: true });
    this.restorePendingEditorPaths();
    this.sourcemapCache = undefined;
    this.studioRuntimeSettings = undefined;
    this.studioConflictPolicyOverride = undefined;
    this.studioSnapshotFingerprintByService.clear();
    this.conflictMarkerWarnedKeys.clear();
    this.recentDirectSaveAtByPath.clear();
    this.editorPushFailureStreak = 0;
    this.linkStatusCache = undefined;
    this.linkStatusInflight = undefined;
    await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
    await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
    this.ensureAgentInstructions(experienceRoot);
    this.linkChangeEmitter.fire();
    this.updateStatusBar();
    if (this.bridgeServeRequested && !this.isBridgeDaemonRunning()) {
      await this.serve({ silent: true, bestEffort: true });
    }
    await this.persistActiveExperiencePlace(experienceRoot, alias);
    let pendingPackageSource = false;
    for (const pending of this.pendingLinkPackageSourcePaths.values()) {
      if (
        this.normalizePathForCompare(pending.projectRoot)
        === this.normalizePathForCompare(active.projectRoot)
      ) {
        pending.generation = this.experienceGeneration;
        pendingPackageSource = true;
      }
    }
    this.experienceChangeInProgress = false;
    if (pendingPackageSource) {
      this.scheduleLinkPackageSourceFlush(active.projectRoot, this.experienceGeneration);
    }
  }

  private async activateExperiencePlace(experienceRoot: string, alias: string): Promise<void> {
    this.experiencePlaceByAlias(experienceRoot, alias);
    const snapshot = this.captureExperienceChange(experienceRoot);
    const resumeLiveSync = await this.prepareExperienceChange();
    try {
      await this.finishExperienceChange(experienceRoot, alias);
    } catch (error) {
      await this.rollbackExperienceChange(experienceRoot, snapshot, resumeLiveSync);
      throw error;
    }
    if (resumeLiveSync) {
      await this.startLiveSync({ silent: true, bestEffort: true });
    }
  }

  private async renameWorkspacePath(source: string, target: string): Promise<void> {
    const edit = new vscode.WorkspaceEdit();
    edit.renameFile(vscode.Uri.file(source), vscode.Uri.file(target), {
      overwrite: false,
      ignoreIfExists: false,
    });
    if (!(await vscode.workspace.applyEdit(edit))) {
      throw new Error(`Could not move ${source} to ${target}.`);
    }
  }

  private configuredLogLevel(): ReniumLogLevel {
    const runtimeLevel = this.studioRuntimeSettings?.logLevel;
    const workspaceConfig = vscode.workspace.getConfiguration("renium");
    const configuredLevel = this.explicitConfigValue<string>(workspaceConfig, "logLevel");
    const sharedLevel = sharedConfigValue<string>(this.sharedConfig, "logLevel");
    const raw = String(
      typeof runtimeLevel === "string"
        ? runtimeLevel
        : configuredLevel ?? sharedLevel ?? "info",
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
      runAction: (action, context) => this.runGitViewAction(action, context.projectRoot),
      openOutput: () => this.output.show(true),
      openDiff: (filePath, context) => this.openGitDiff(filePath, context.projectRoot),
    };
  }

  private gitHeadProviderRegistered = false;


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


  private async openGitDiff(filePath: string, expectedProjectRoot: string): Promise<void> {
    const requested = String(filePath ?? "").trim();
    if (!requested) {
      return;
    }
    this.ensureGitHeadProvider();
    const token = this.captureGitProjectToken(expectedProjectRoot);
    const cfg = this.gitConfigForToken(token);
    let repoRoot: string;
    try {
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      this.gitConfigForToken(token);
      repoRoot = this.requireGitRepoRoot(state);
    } catch (err) {
      vscode.window.showErrorMessage(`Cannot open diff. ${err instanceof Error ? err.message : String(err)}`);
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
    if (this.linkPackageSourceApplyTimer) {
      clearTimeout(this.linkPackageSourceApplyTimer);
      this.linkPackageSourceApplyTimer = undefined;
    }
    for (const watcher of this.linkPackageSourceWatchers) {
      watcher.dispose();
    }
    this.linkPackageSourceWatchers = [];
    this.activeLinkPackageSourceKeys.clear();
    this.disposeLiveSyncRuntime();
    this.stopStudioActionPolling();
    void this.stopConsoleFollow({ releaseServe: false });
    void this.stopBridgeDaemon();

    this.statusItem.dispose();
    this.consoleOutput.dispose();
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
        label: "$(cloud-download) Pull Studio to files",
        description: "Replace project files with the connected Studio place",
        action: "pullFromStudio",
      },
      {
        label: "$(cloud-upload) Push files to Studio",
        description: "Replace the connected Studio place with project files",
        action: "pushToStudio",
      },
      {
        label: "$(list-tree) Manage Places",
        description: "Add, switch, rename, or reorder experience places",
        action: "managePlaces",
      },
      {
        label: "$(export) Export Snapshots Only",
        description: "Studio -> snapshots",
        action: "exportOnly",
      },
      {
        label: "$(save) Export Game File...",
        description: "Write a .rbxl/.rbxlx place file from the project files",
        action: "exportGameFile",
      },
      {
        label: "$(package) Sync Wally Packages",
        description: "Install Wally packages and import them into the configured package target",
        action: "wallyPackages",
      },
      {
        label: "$(link) Sync Link Mirrors",
        description: "Rebuild project link targets from local, Git, Wally, or package sources",
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
        description: liveSyncRunning
          ? "Stop watching project and Studio edits"
          : "Two-way sync between project files and Studio",
        action: liveSyncRunning ? "stopLive" : "startLive",
      },
      {
        label: "$(git) Git",
        description: "Open the Git tab in the main Renium panel",
        action: "gitSync",
      },
      {
        label: "$(tools) Project Tools",
        description: "Build, diagnose, configure, update, or open the console",
        action: "projectTools",
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
      case "pullFromStudio":
        await this.pullFromStudio();
        return;
      case "pushToStudio":
        await this.pushToStudio();
        return;
      case "managePlaces":
        await this.managePlaces();
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
      case "projectTools":
        await this.openProjectTools();
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

  private async connectedStudioPlaces(attempt = 0): Promise<ConnectedStudioPlace[]> {
    const cfg = this.getConfig();
    const result = await this.runAutomationOperation(
      cfg.exportCliPath,
      cfg,
      "current-place",
      AUTOMATION_OP.studios,
      { all: true },
      { timeoutMs: 1_000 },
    );
    if (result.code !== 0) {
      throw new Error("Could not read the connected Studio places.");
    }
    const payload = result.result as { studios?: unknown } | undefined;
    const clients = (Array.isArray(payload?.studios) ? payload.studios : [])
      .map((value): ConnectedStudioPlace | undefined => {
        if (!value || typeof value !== "object" || Array.isArray(value)) {
          return undefined;
        }
        const client = value as BridgeClientInfo;
        const placeId = Number(client.placeId);
        const gameId = Number(client.gameId);
        const placeName = typeof client.placeName === "string" ? client.placeName.trim() : "";
        if (
          String(client.role ?? "").toLowerCase() !== "edit" ||
          !Number.isSafeInteger(placeId) ||
          placeId < 0 ||
          !placeName
        ) {
          return undefined;
        }
        return {
          runtimeId: typeof client.runtimeId === "string" ? client.runtimeId : undefined,
          placeId,
          gameId: Number.isSafeInteger(gameId) && gameId >= 0 ? gameId : undefined,
          placeName,
        };
      })
      .filter((value): value is ConnectedStudioPlace => value !== undefined);
    const unique = Array.from(new Map(
      clients.map((client) => [
        client.runtimeId || `${client.gameId}:${client.placeId}:${client.placeName}`,
        client,
      ]),
    ).values());
    if (unique.length === 0 && attempt < 2) {
      await new Promise<void>((resolve) => {
        setTimeout(resolve, attempt === 0 ? 200 : 800);
      });
      return await this.connectedStudioPlaces(attempt + 1);
    }
    return unique;
  }

  private async currentStudioPlace(
    connected?: ConnectedStudioPlace[],
  ): Promise<ConnectedStudioPlace | undefined> {
    const unique = connected ?? await this.connectedStudioPlaces();
    if (unique.length === 0) {
      void vscode.window.showErrorMessage("No edit-mode Studio place is connected.");
      return undefined;
    }
    const resolved = await Promise.all(unique.map(async (client) => ({
      ...client,
      placeName: await this.publishedPlaceName(client.placeId) ?? client.placeName,
    })));
    if (resolved.length === 1) {
      return resolved[0];
    }
    const picked = await vscode.window.showQuickPick(
      resolved.map((client) => ({
        label: client.placeName,
        description: client.placeId && client.gameId
          ? `PlaceId ${client.placeId}, GameId ${client.gameId}`
          : "Unpublished place",
        client,
      })),
      {
        title: "Choose Studio Place",
        placeHolder: "Choose the Studio place to use",
      },
    );
    return picked?.client;
  }

  private async robloxJson(url: string, label: string): Promise<unknown> {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 1_000);
    try {
      const response = await fetch(url, { signal: controller.signal });
      return response.ok ? await response.json() : undefined;
    } catch (error) {
      this.output.appendLine(
        `[renium] ${label} failed: ${error instanceof Error ? error.message : String(error)}`,
      );
      return undefined;
    } finally {
      clearTimeout(timeout);
    }
  }

  private async publishedPlaceName(placeId: number): Promise<string | undefined> {
    if (placeId <= 0) {
      return undefined;
    }
    const cached = this.publishedPlaceNames.get(placeId);
    if (cached) {
      return cached;
    }
    const raw = await this.robloxJson(
      `https://economy.roblox.com/v2/assets/${placeId}/details`,
      `Place name lookup for ${placeId}`,
    );
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
      return undefined;
    }
    const details = raw as { AssetId?: unknown; AssetTypeId?: unknown; Name?: unknown };
    if (
      Number(details.AssetId) !== placeId ||
      Number(details.AssetTypeId) !== 9 ||
      typeof details.Name !== "string"
    ) {
      return undefined;
    }
    const name = details.Name.trim();
    if (!name) {
      return undefined;
    }
    this.publishedPlaceNames.set(placeId, name);
    return name;
  }

  private async publishedRootPlaceId(gameId: number): Promise<number | undefined> {
    if (gameId <= 0) {
      return undefined;
    }
    const cached = this.publishedRootPlaceIds.get(gameId);
    if (cached) {
      return cached;
    }
    const raw = await this.robloxJson(
      `https://games.roblox.com/v1/games?universeIds=${gameId}`,
      `Starting place lookup for ${gameId}`,
    );
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
      return undefined;
    }
    const data = (raw as { data?: unknown }).data;
    if (!Array.isArray(data)) {
      return undefined;
    }
    const gameDetails = data.find((value) =>
      !!value &&
      typeof value === "object" &&
      !Array.isArray(value) &&
      Number((value as { id?: unknown }).id) === gameId);
    if (!gameDetails) {
      return undefined;
    }
    const rootPlaceId = Number((gameDetails as { rootPlaceId?: unknown }).rootPlaceId);
    if (!Number.isSafeInteger(rootPlaceId) || rootPlaceId <= 0) {
      return undefined;
    }
    this.publishedRootPlaceIds.set(gameId, rootPlaceId);
    return rootPlaceId;
  }

  public async addCurrentPlace(connectedPlace?: ConnectedStudioPlace): Promise<void> {
    let shouldSync = false;
    let resumeLiveSync = false;
    try {
      const current = connectedPlace
        ? {
          ...connectedPlace,
          placeName: await this.publishedPlaceName(connectedPlace.placeId) ?? connectedPlace.placeName,
        }
        : await this.currentStudioPlace();
      if (!current) {
        return;
      }
      if (current.gameId === undefined) {
        void vscode.window.showErrorMessage(
          "The connected Studio plugin did not report GameId. Install the current plugin and try again.",
        );
        return;
      }
      const cfg = this.getConfig();
      const experienceRoot = cfg.experienceRoot;
      this.ensureAgentInstructions(experienceRoot);
      let manifest = readExperienceManifest(experienceRoot);
      const firstPlace = manifest === undefined;
      if (manifest && manifest.gameId > 0 && current.gameId > 0 && manifest.gameId !== current.gameId) {
        await vscode.window.showErrorMessage(
          `This place belongs to GameId ${current.gameId}, but this project belongs to GameId ${manifest.gameId}.`,
          { modal: true },
        );
        return;
      }
      const currentPlaceId = current.placeId;
      const rootPlaceId = await this.publishedRootPlaceId(current.gameId);
      if (manifest && rootPlaceId) {
        const rootEntry = Object.entries(manifest.places)
          .find(([, place]) => place.placeId === rootPlaceId);
        if (rootEntry && manifest.startPlace !== rootEntry[0]) {
          manifest = {
            ...manifest,
            startPlace: rootEntry[0],
          };
          writeExperienceManifest(experienceRoot, manifest);
        }
      }
      const existing = manifest
        ? Object.entries(manifest.places).find(([, place]) =>
          currentPlaceId > 0
            ? place.placeId === currentPlaceId
            : place.placeId === 0 && place.name === current.placeName)
        : undefined;
      if (existing && manifest) {
        const [existingAlias, existingPlace] = existing;
        const correctedAlias = normalizePublishedPlaceName(current.placeName, currentPlaceId);
        const generatedFromOldName =
          existingAlias === normalizePublishedPlaceName(existingPlace.name, currentPlaceId);
        if (
          currentPlaceId > 0 &&
          generatedFromOldName &&
          correctedAlias !== existingAlias &&
          !manifest?.places[correctedAlias] &&
          !fs.existsSync(path.join(experienceRoot, "places", correctedAlias))
        ) {
          await this.renameExperiencePlace(
            experienceRoot,
            {
              alias: existingAlias,
              manifest,
              place: existingPlace,
            },
            correctedAlias,
            current.placeName,
          );
          void vscode.window.showInformationMessage(
            `Updated ${existingAlias} to ${correctedAlias} using the published place name.`,
          );
          return;
        }
        if (existingPlace.name !== current.placeName) {
          manifest.places[existingAlias] = {
            ...existingPlace,
            name: current.placeName,
          };
          writeExperienceManifest(experienceRoot, manifest);
        }
        await this.activateExperiencePlace(experienceRoot, existingAlias);
        void vscode.window.showInformationMessage(`Switched to ${existingAlias}.`);
        return;
      }
      const alias = uniquePlaceAlias(
        experienceRoot,
        manifest,
        current.placeName,
        currentPlaceId,
      );
      const originalManifest = manifest
        ? JSON.parse(JSON.stringify(manifest)) as ExperienceManifest
        : undefined;
      manifest = manifest ?? {
        version: 2,
        gameId: current.gameId,
        startPlace: alias,
        placeOrder: [],
        places: {},
      };
      if (manifest.gameId === 0 && current.gameId > 0) {
        manifest.gameId = current.gameId;
      }
      const placeRoot = path.posix.join("places", alias);
      manifest.places[alias] = {
        placeId: currentPlaceId,
        name: current.placeName,
        root: placeRoot,
      };
      if (currentPlaceId > 0) {
        manifest.placeOrder.push(currentPlaceId);
      }
      if (rootPlaceId === currentPlaceId) {
        manifest.startPlace = alias;
      }
      const absolutePlaceRoot = resolveExperiencePlaceRoot(experienceRoot, placeRoot);
      const createdPlaceRoot = !fs.existsSync(absolutePlaceRoot);
      const migratedPaths: Array<{ source: string; target: string }> = [];
      const experienceSnapshot = this.captureExperienceChange(experienceRoot);
      resumeLiveSync = await this.prepareExperienceChange();
      try {
        fs.mkdirSync(absolutePlaceRoot, { recursive: true });
        if (firstPlace) {
          for (const name of reniumPlaceProjectPaths(experienceRoot)) {
            const source = path.join(experienceRoot, name);
            const target = path.join(absolutePlaceRoot, name);
            if (!fs.existsSync(source)) {
              continue;
            }
            await this.renameWorkspacePath(source, target);
            migratedPaths.push({ source, target });
          }
        }
        writeExperienceManifest(experienceRoot, manifest);
        await this.finishExperienceChange(experienceRoot, alias);
      } catch (error) {
        for (const migrated of migratedPaths.reverse()) {
          if (!fs.existsSync(migrated.target) || fs.existsSync(migrated.source)) {
            continue;
          }
          try {
            await this.renameWorkspacePath(migrated.target, migrated.source);
          } catch (rollbackError) {
            this.output.appendLine(
              `[renium] place migration rollback failed: ${rollbackError instanceof Error ? rollbackError.message : String(rollbackError)}`,
            );
          }
        }
        if (createdPlaceRoot && fs.existsSync(absolutePlaceRoot)) {
          try {
            fs.rmdirSync(absolutePlaceRoot);
          } catch (cleanupError) {
            this.output.appendLine(
              `[renium] place folder cleanup failed: ${cleanupError instanceof Error ? cleanupError.message : String(cleanupError)}`,
            );
          }
        }
        if (originalManifest) {
          writeExperienceManifest(experienceRoot, originalManifest);
        } else {
          fs.rmSync(path.join(experienceRoot, "renium.experience.json"), { force: true });
        }
        await this.rollbackExperienceChange(experienceRoot, experienceSnapshot, resumeLiveSync);
        throw error;
      }
      void vscode.window.showInformationMessage(
        `Added ${current.placeName} as ${alias}. Starting its first pull from Studio.`,
      );
      shouldSync = true;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] add current place failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not add the current place. ${message}`);
      return;
    }
    if (shouldSync) {
      try {
        await this.syncActivePlace();
      } finally {
        if (resumeLiveSync) {
          await this.startLiveSync({ silent: true, bestEffort: true });
        }
      }
    }
  }

  public async managePlaces(): Promise<void> {
    try {
      const cfg = this.getConfig();
      const active = resolveActiveExperiencePlace(cfg.experienceRoot);
      const items: Array<vscode.QuickPickItem & { action: string }> = [
        {
          label: "$(add) Add Current Studio Place",
          description: "Add a connected place to this experience project",
          action: "addCurrentPlace",
        },
      ];
      if (active) {
        items.push(
          {
            label: "$(arrow-swap) Switch Active Place",
            description: `Current: ${active.alias}`,
            action: "switchPlace",
          },
          {
            label: "$(edit) Rename Active Place",
            description: `Current alias: ${active.alias}`,
            action: "renamePlace",
          },
          {
            label: "$(list-ordered) Reorder Places",
            description: "Set the order used when places are displayed",
            action: "reorderPlaces",
          },
        );
      }
      const picked = await vscode.window.showQuickPick(items, {
        title: "Manage Places",
        placeHolder: "Choose an action",
      });
      if (!picked) {
        return;
      }
      switch (picked.action) {
        case "addCurrentPlace":
          await this.addCurrentPlace();
          return;
        case "switchPlace":
          await this.switchPlace();
          return;
        case "renamePlace":
          await this.renamePlace();
          return;
        case "reorderPlaces":
          await this.reorderPlaces();
          return;
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] manage places failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not manage places. ${message}`);
    }
  }

  public async switchPlace(): Promise<void> {
    try {
      const cfg = this.getConfig();
      const manifest = readExperienceManifest(cfg.experienceRoot);
      if (!manifest) {
        void vscode.window.showInformationMessage("Add the current Studio place first.");
        return;
      }
      const rootPlaceId = await this.publishedRootPlaceId(manifest.gameId);
      const currentAlias = resolveActiveExperiencePlace(cfg.experienceRoot)?.alias;
      const picked = await vscode.window.showQuickPick(
        experiencePlaceAliasesInOrder(manifest)
          .map((alias) => {
            const place = manifest.places[alias];
            const startingPlace = rootPlaceId
              ? place.placeId === rootPlaceId
              : alias === manifest.startPlace;
            const status = [
              startingPlace ? "Starting place" : "",
              alias === currentAlias ? "Current" : "",
              place.name,
            ].filter(Boolean);
            return {
              label: alias,
              description: status.join(" | "),
              detail: place.placeId > 0 ? `PlaceId ${place.placeId}` : "Unpublished place",
              alias,
            };
          }),
        {
          title: "Switch Place",
          placeHolder: currentAlias ? `Current: ${currentAlias}` : "Choose a place",
        },
      );
      if (!picked || picked.alias === currentAlias) {
        return;
      }
      await this.activateExperiencePlace(cfg.experienceRoot, picked.alias);
      void vscode.window.showInformationMessage(`Switched to ${picked.alias}.`);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] switch place failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not switch places. ${message}`);
    }
  }

  public async reorderPlaces(): Promise<void> {
    try {
      const cfg = this.getConfig();
      const initialManifest = readExperienceManifest(cfg.experienceRoot);
      if (!initialManifest) {
        void vscode.window.showInformationMessage("Add the current Studio place first.");
        return;
      }
      let manifest: ExperienceManifest = initialManifest;
      if (manifest.placeOrder.length < 2) {
        void vscode.window.showInformationMessage("Add another published place before changing the order.");
        return;
      }
      while (true) {
        const activeAlias = resolveActiveExperiencePlace(cfg.experienceRoot)?.alias;
        const orderedAliases = experiencePlaceAliasesInOrder(manifest)
          .filter((alias) => manifest.places[alias].placeId > 0);
        const selected = await vscode.window.showQuickPick(
          orderedAliases.map((alias, index) => {
            const place = manifest.places[alias];
            const status = [
              alias === manifest.startPlace ? "Starting place" : "",
              alias === activeAlias ? "Current" : "",
              place.name,
            ].filter(Boolean);
            return {
              label: `${index + 1}. ${alias}`,
              description: status.join(" | "),
              detail: place.placeId > 0 ? `PlaceId ${place.placeId}` : "Unpublished place",
              alias,
            };
          }),
          {
            title: "Reorder Places",
            placeHolder: "Choose a place to move; press Escape when finished",
          },
        );
        if (!selected) {
          return;
        }
        const currentIndex = orderedAliases.indexOf(selected.alias);
        const destination = await vscode.window.showQuickPick(
          orderedAliases.map((alias, index) => ({
            label: `Position ${index + 1}`,
            description: index === currentIndex ? "Current position" : `Currently ${alias}`,
            index,
          })),
          {
            title: `Move ${selected.alias}`,
            placeHolder: "Choose its new position",
          },
        );
        if (!destination) {
          return;
        }
        if (destination.index === currentIndex) {
          continue;
        }
        const placeOrder: number[] = [...manifest.placeOrder];
        placeOrder.splice(currentIndex, 1);
        placeOrder.splice(destination.index, 0, manifest.places[selected.alias].placeId);
        manifest = {
          ...manifest,
          placeOrder,
        };
        writeExperienceManifest(cfg.experienceRoot, manifest);
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] reorder places failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not reorder places. ${message}`);
    }
  }

  public async renamePlace(): Promise<void> {
    try {
      const cfg = this.getConfig();
      const active = resolveActiveExperiencePlace(cfg.experienceRoot);
      if (!active) {
        void vscode.window.showInformationMessage("Add the current Studio place first.");
        return;
      }
      const input = await vscode.window.showInputBox({
        title: "Rename Current Place Alias",
        prompt: `Rename ${active.alias}. This does not rename the place in Roblox.`,
        value: active.alias,
        validateInput: (value) =>
          /[a-z0-9]/i.test(value)
            ? undefined
            : "Use at least one English letter or number.",
      });
      if (input === undefined) {
        return;
      }
      const alias = normalizePlaceAlias(input, active.place.placeId);
      if (alias === active.alias) {
        return;
      }
      if (active.manifest.places[alias]) {
        void vscode.window.showErrorMessage(`The alias '${alias}' already exists.`);
        return;
      }
      const nextRoot = path.posix.join("places", alias);
      const targetRoot = resolveExperiencePlaceRoot(cfg.experienceRoot, nextRoot);
      if (fs.existsSync(targetRoot)) {
        void vscode.window.showErrorMessage(`${targetRoot} already exists.`);
        return;
      }
      await this.renameExperiencePlace(cfg.experienceRoot, active, alias, active.place.name);
      void vscode.window.showInformationMessage(`Renamed ${active.alias} to ${alias}.`);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] rename place failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not rename the place alias. ${message}`);
    }
  }

  private async renameExperiencePlace(
    experienceRoot: string,
    active: {
      alias: string;
      manifest: ExperienceManifest;
      place: ExperiencePlace;
    },
    alias: string,
    placeName: string,
  ): Promise<void> {
    const nextRoot = path.posix.join("places", alias);
    const sourceRoot = resolveExperiencePlaceRoot(experienceRoot, active.place.root);
    const targetRoot = resolveExperiencePlaceRoot(experienceRoot, nextRoot);
    const experienceSnapshot = this.captureExperienceChange(experienceRoot);
    const resumeLiveSync = await this.prepareExperienceChange();
    const sourceExists = fs.existsSync(sourceRoot);
    const places = Object.fromEntries(
      Object.entries(active.manifest.places).map(([currentAlias, place]): [string, ExperiencePlace] =>
        currentAlias === active.alias
          ? [alias, { ...place, name: placeName, root: nextRoot }]
          : [currentAlias, place]),
    );
    const renamed: ExperienceManifest = {
      ...active.manifest,
      startPlace: active.manifest.startPlace === active.alias ? alias : active.manifest.startPlace,
      places,
    };
    let sourceMoved = false;
    let targetCreated = false;
    try {
      fs.mkdirSync(path.dirname(targetRoot), { recursive: true });
      if (sourceExists) {
        await this.renameWorkspacePath(sourceRoot, targetRoot);
        sourceMoved = true;
      } else {
        fs.mkdirSync(targetRoot);
        targetCreated = true;
      }
      writeExperienceManifest(experienceRoot, renamed);
      await this.finishExperienceChange(experienceRoot, alias);
    } catch (error) {
      try {
        if (sourceMoved && fs.existsSync(targetRoot) && !fs.existsSync(sourceRoot)) {
          await this.renameWorkspacePath(targetRoot, sourceRoot);
        } else if (targetCreated && fs.existsSync(targetRoot)) {
          fs.rmdirSync(targetRoot);
        }
      } catch (rollbackError) {
        const message = rollbackError instanceof Error ? rollbackError.message : String(rollbackError);
        this.output.appendLine(`[renium] place rename rollback failed: ${message}`);
      }
      try {
        writeExperienceManifest(experienceRoot, active.manifest);
      } catch (rollbackError) {
        const message = rollbackError instanceof Error ? rollbackError.message : String(rollbackError);
        this.output.appendLine(`[renium] place manifest rollback failed: ${message}`);
      }
      await this.rollbackExperienceChange(experienceRoot, experienceSnapshot, resumeLiveSync);
      throw error;
    }
    if (resumeLiveSync) {
      await this.startLiveSync({ silent: true, bestEffort: true });
    }
  }

  public async installStudioPlugin(): Promise<void> {
    const assetName = "Renium.rbxm";
    const extensionVersion = String(this.context.extension.packageJSON.version ?? "");
    const releaseUrl = reniumPluginReleaseUrl(extensionVersion, assetName);
    if (process.platform !== "win32" && process.platform !== "darwin") {
      void vscode.window.showErrorMessage("Roblox Studio is only available on Windows and macOS.");
      return;
    }

    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: "Installing Studio plugin..." },
      async () => {
        let bytes: Buffer | undefined;
        let source = "";
        let sourcePath = "";
        let temporarySource = "";
        const workspaceRoot = pickWorkspaceRoot();
        const localBundles = [
          this.context.asAbsolutePath(path.join("assets", assetName)),
          ...(workspaceRoot ? [path.join(workspaceRoot, "tools", "plugin_ws_bridge", assetName)] : []),
        ];
        for (const localBundle of localBundles) {
          if (!fs.existsSync(localBundle)) {
            continue;
          }
          const candidateBytes = fs.readFileSync(localBundle);
          if (isRobloxModel(candidateBytes)) {
            bytes = candidateBytes;
            source = localBundle;
            sourcePath = localBundle;
            break;
          }
          this.output.appendLine(`[plugin-install] ignored invalid bundled model: ${localBundle}`);
        }
        if (!bytes) {
          source = releaseUrl;
          try {
            const response = await fetch(releaseUrl);
            if (!response.ok) {
              throw new Error(`HTTP ${response.status}`);
            }
            bytes = Buffer.from(await response.arrayBuffer());
          } catch (error) {
            void vscode.window.showErrorMessage(
              `Downloading the matching Studio plugin failed (${String(error)}). Check your network or download ${assetName} from release v${extensionVersion}.`,
            );
            return;
          }
        }
        if (!isRobloxModel(bytes)) {
          void vscode.window.showErrorMessage("The Studio plugin file is not a valid Roblox model.");
          return;
        }
        if (!sourcePath) {
          temporarySource = path.join(os.tmpdir(), `renium-plugin-${process.pid}-${Date.now()}.rbxm`);
          fs.writeFileSync(temporarySource, bytes);
          sourcePath = temporarySource;
        }
        try {
          const cfg = this.getConfig();
          const command = this.resolveRustCliPathForCommand(cfg, "setup");
          const result = await this.runCommand(
            command,
            ["setup", "--file", sourcePath],
            cfg.projectRoot,
            "Studio plugin setup",
            cfg.progressHeartbeatSeconds,
          );
          if (result.code !== 0) {
            void vscode.window.showErrorMessage(
              `Studio plugin setup failed. ${result.output.trim() || `Exit code ${result.code}`}`,
            );
            return;
          }
          this.output.appendLine(`[plugin-install] ${source} (${bytes.length} bytes)`);
        } finally {
          if (temporarySource) {
            fs.rmSync(temporarySource, { force: true });
          }
        }
        void vscode.window.showInformationMessage(
          process.platform === "darwin"
            ? `Studio plugin installed (${Math.round(bytes.length / 1024)} KB). Open Renium Studio from Applications.`
            : `Studio plugin installed (${Math.round(bytes.length / 1024)} KB). Restart Roblox Studio to load it.`,
        );
      },
    );
  }

  private async prepareExplicitSync(): Promise<boolean> {
    const cfg = this.getConfig();
    this.ensureAgentInstructions(cfg.experienceRoot);
    await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
    const manifest = readExperienceManifest(cfg.experienceRoot);
    if (!manifest && !hasSinglePlaceProject(cfg.experienceRoot)) {
      await this.addCurrentPlace();
      return false;
    }
    if (manifest) {
      const connected = await this.connectedStudioPlaces();
      const current = connected.length === 1
        ? connected[0]
        : await this.currentStudioPlace(connected);
      if (!current) {
        return false;
      }
      const matching = Object.entries(manifest.places).find(([, place]) =>
        current.placeId > 0
          ? place.placeId === current.placeId
          : place.placeId === 0 && place.name === current.placeName);
      if (!matching) {
        await this.addCurrentPlace(current);
        return false;
      }
      if (matching[0] !== cfg.activePlaceAlias) {
        await this.activateExperiencePlace(cfg.experienceRoot, matching[0]);
      }
    }
    return true;
  }

  public async pullFromStudio(): Promise<void> {
    if (await this.prepareExplicitSync()) {
      await this.syncActivePlace("studio");
    }
  }

  public async pushToStudio(): Promise<void> {
    if (await this.prepareExplicitSync()) {
      await this.syncActivePlace("editor");
    }
  }

  public async openProjectTools(): Promise<void> {
    const picked = await vscode.window.showQuickPick([
      {
        label: "$(check) Diagnose Project",
        description: "Check project files, tools, plugin installation, and daemon state",
        action: "doctor",
      },
      {
        label: "$(package) Build Project",
        description: "Build the active place using renium.project.jsonc",
        action: "build",
      },
      {
        label: "$(json) Open Project Configuration",
        description: "Edit renium.project.jsonc with schema validation",
        action: "config",
      },
      {
        label: "$(book) Show CLI Documentation",
        description: "Show the bundled Renium command reference",
        action: "docs",
      },
      {
        label: "$(terminal) Follow Studio Console",
        description: "Open or pause the managed Studio console",
        action: "console",
      },
      {
        label: "$(cloud-download) Check for Updates",
        description: "Verify the signed release manifest without installing anything",
        action: "update",
      },
      {
        label: "$(tools) Repair Installation",
        description: "Reinstall the matching Studio plugin",
        action: "repair",
      },
      {
        label: "$(trash) Uninstall Studio Plugin",
        description: "Remove Renium from the Roblox Studio Plugins folder",
        action: "uninstall",
      },
    ], {
      title: "Renium Project Tools",
      placeHolder: "Choose an action",
    });
    switch (picked?.action) {
      case "doctor":
        await this.runProjectDoctor();
        break;
      case "build":
        await this.buildProject();
        break;
      case "config":
        await this.openProjectConfiguration();
        break;
      case "docs":
        await this.openCliDocumentation();
        break;
      case "console":
        this.followStudioConsole();
        break;
      case "update":
        await this.checkForUpdates();
        break;
      case "repair":
        await this.repairInstallation();
        break;
      case "uninstall":
        await this.uninstallStudioPlugin();
        break;
    }
  }

  private projectManifestPath(cfg = this.getConfig()): string {
    const jsonc = path.join(cfg.projectRoot, "renium.project.jsonc");
    const json = path.join(cfg.projectRoot, "renium.project.json");
    return fs.existsSync(json) && !fs.existsSync(jsonc) ? json : jsonc;
  }

  private requireProjectManifest(cfg = this.getConfig()): string {
    const manifest = this.projectManifestPath(cfg);
    if (!fs.existsSync(manifest)) {
      throw new Error(`Project configuration not found: ${manifest}`);
    }
    return manifest;
  }

  private async runProjectCommand(
    taskName: string,
    cliCommand: string,
    args: string[],
  ): Promise<CommandRunResult | undefined> {
    let result: CommandRunResult | undefined;
    await this.enqueue(taskName, async () => {
      const cfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(cfg, cliCommand);
      this.ensureFileExists(command);
      this.output.show(false);
      this.output.appendLine(`[renium] ${taskName}: ${command} ${this.renderArgs(args)}`);
      result = await this.runCommand(
        command,
        args,
        cfg.projectRoot,
        taskName,
        cfg.progressHeartbeatSeconds,
      );
      if (result.code !== 0) {
        throw new Error(`${cliCommand} exited with code ${result.code}`);
      }
    });
    return result;
  }

  public async runProjectDoctor(): Promise<void> {
    const cfg = this.getConfig();
    await this.runProjectCommand("Project diagnosis", "doctor", [
      "doctor",
      "--root",
      cfg.projectRoot,
      "--json",
    ]);
  }

  public async buildProject(): Promise<void> {
    const cfg = this.getConfig();
    const manifest = this.requireProjectManifest(cfg);
    await this.runProjectCommand("Project build", "build", [
      "build",
      "--project",
      manifest,
      "--sourcemap",
    ]);
  }

  public async openProjectConfiguration(): Promise<void> {
    const cfg = this.getConfig();
    const manifest = this.requireProjectManifest(cfg);
    await vscode.window.showTextDocument(vscode.Uri.file(manifest), { preview: false });
  }

  public async openCliDocumentation(): Promise<void> {
    await this.runProjectCommand("CLI documentation", "docs", ["docs"]);
  }

  public async followStudioConsole(): Promise<void> {
    if (this.consoleFollowRunning) {
      await this.stopConsoleFollow();
      this.consoleOutput.appendLine("[Renium console paused]");
      this.consoleOutput.show(false);
      return;
    }
    const cfg = this.getConfig();
    const command = this.resolveRustCliPathForCommand(cfg, "co");
    this.ensureFileExists(command);
    const startedServe = !this.bridgeServeRequested;
    this.bridgeServeRequested = true;
    this.consoleFollowOwnsServe = startedServe;
    try {
      await this.ensureBridgeDaemon(command, cfg, { serve: true });
    } catch (error) {
      if (startedServe) {
        this.bridgeServeRequested = false;
        this.consoleFollowOwnsServe = false;
        await this.stopBridgeDaemon();
      }
      throw error;
    }
    const generation = ++this.consoleFollowGeneration;
    this.consoleFollowRunning = true;
    this.consoleOutput.appendLine("[Renium console resumed]");
    this.consoleOutput.show(false);
    void this.pollStudioConsole(generation);
  }

  private async pollStudioConsole(generation: number): Promise<void> {
    if (
      !this.consoleFollowRunning ||
      generation !== this.consoleFollowGeneration ||
      !this.bridgeServeRequested ||
      this.consoleFollowInFlightGeneration === generation
    ) {
      return;
    }
    this.consoleFollowInFlightGeneration = generation;
    let drainImmediately = false;
    try {
      const cfg = this.getConfig();
      const command = this.resolveRustCliPathForCommand(cfg, "co");
      const result = await this.runAutomationOperation(
        command,
        cfg,
        "console-follow",
        AUTOMATION_OP.console,
        {
          limit: 200,
          sinceSeq: this.consoleFollowSeq,
          fromOldest: this.consoleFollowFromOldest,
        },
        { quietWait: true, timeoutMs: 5000 },
      );
      if (
        !this.consoleFollowRunning
        || generation !== this.consoleFollowGeneration
        || !this.bridgeServeRequested
      ) {
        return;
      }
      if (result.code !== 0) {
        throw new Error(`Console request exited with code ${result.code}`);
      }
      const value = result.result && typeof result.result === "object" && !Array.isArray(result.result)
        ? result.result as Record<string, unknown>
        : this.parseCliJsonObject<Record<string, unknown>>(result.output);
      if (value) {
        const epoch = typeof value.epoch === "string" ? value.epoch : undefined;
        if (this.consoleFollowEpoch && epoch !== this.consoleFollowEpoch) {
          this.consoleFollowSeq = 0;
          this.consoleFollowFromOldest = true;
          this.consoleOutput.appendLine("[Studio console restarted]");
          drainImmediately = true;
        } else {
          const entries = Array.isArray(value.entries) ? value.entries : [];
          for (const entry of entries) {
            if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
              continue;
            }
            const row = entry as Record<string, unknown>;
            this.consoleOutput.appendLine(`[${String(row.type ?? row.level ?? "output")}] ${String(row.message ?? "")}`);
          }
          const nextSeq = Number(value.nextSeq);
          if (Number.isFinite(nextSeq) && nextSeq >= 0) {
            this.consoleFollowSeq = Math.floor(nextSeq);
          }
          this.consoleFollowFromOldest = false;
          drainImmediately = value.hasMore === true;
          if (value.truncated === true) {
            this.consoleOutput.appendLine("[Studio console output was truncated before Renium could read it]");
          }
        }
        this.consoleFollowEpoch = epoch;
      }
    } catch (error) {
      if (this.consoleFollowRunning && generation === this.consoleFollowGeneration) {
        this.consoleOutput.appendLine(`[Renium console error] ${error instanceof Error ? error.message : String(error)}`);
      }
    } finally {
      if (this.consoleFollowInFlightGeneration === generation) {
        this.consoleFollowInFlightGeneration = undefined;
      }
      if (
        this.consoleFollowRunning
        && generation === this.consoleFollowGeneration
        && this.bridgeServeRequested
      ) {
        this.consoleFollowTimer = setTimeout(() => {
          this.consoleFollowTimer = undefined;
          void this.pollStudioConsole(generation);
        }, drainImmediately ? 0 : 250);
      }
    }
  }

  private async stopConsoleFollow(options: { releaseServe?: boolean } = {}): Promise<void> {
    this.consoleFollowRunning = false;
    this.consoleFollowGeneration += 1;
    if (this.consoleFollowTimer) {
      clearTimeout(this.consoleFollowTimer);
      this.consoleFollowTimer = undefined;
    }
    const ownedServe = this.consoleFollowOwnsServe;
    this.consoleFollowOwnsServe = false;
    if (
      options.releaseServe !== false
      && ownedServe
      && !this.liveSyncWatcher
      && !this.liveSyncStartPromise
      && !this.liveSyncStartupInProgress
    ) {
      this.bridgeServeRequested = false;
      this.stopStudioActionPolling();
      await this.stopBridgeDaemon();
      this.updateStatusBar();
    }
  }

  public async checkForUpdates(): Promise<void> {
    await this.runProjectCommand("Update check", "update", ["update", "check"]);
  }

  public async repairInstallation(): Promise<void> {
    await this.runProjectCommand("Installation repair", "setup", ["setup", "--repair"]);
  }

  public async uninstallStudioPlugin(): Promise<void> {
    const choice = await vscode.window.showWarningMessage(
      "Remove the Renium Studio plugin?",
      { modal: true },
      "Uninstall",
    );
    if (choice !== "Uninstall") {
      return;
    }
    await this.runProjectCommand("Plugin uninstall", "setup", ["setup", "--uninstall"]);
  }

  private async syncActivePlace(direction: "studio" | "editor" = "studio"): Promise<void> {
    await this.enqueue(direction === "editor" ? "Push to Studio" : "Pull from Studio", async () => {
      if (direction === "editor") {
        const cfg = this.getConfig();
        const changedPaths = await this.collectInitialEditorLiveSyncPathsAsync(this.sourceRoot(cfg));
        if (changedPaths.length === 0) {
          throw new Error(`No project source files exist under ${this.sourceRoot(cfg)}`);
        }
        const outcome = await this.runEditorPush(changedPaths, cfg, { fullSync: true });
        if (outcome !== "applied") {
          throw new Error("Studio did not apply the project files");
        }
        vscode.window.showInformationMessage("Push to Studio completed.");
        return;
      }
      await this.runExport({
        services: this.getConfig().services,
        runImport: true,
        notifyOnSuccess: true,
        reason: "Pull from Studio completed",
        destructive: true,
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
        runCfg.srcDir,
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
      vscode.window.showInformationMessage(`Exported game file to ${finalOutputPath}${instanceSummary}.`);
    });
  }

  public async importSnapshotsOnly(): Promise<void> {
    await this.enqueue("Import snapshots", async () => {
      const cfg = this.getConfig();
      const snapshotPath = this.resolveSnapshotPath(cfg);
      const serviceLabel = cfg.services.length === 1 ? cfg.services[0] : `${cfg.services.length} services`;
      const confirmed = await vscode.window.showWarningMessage(
        `Import Studio snapshots into ${cfg.srcDir} for ${serviceLabel}? Existing generated files may be replaced; stale files are recoverable from .renium/import-backups.`,
        { modal: true },
        "Import snapshots",
      );
      if (confirmed !== "Import snapshots") {
        return;
      }
      await this.runRustImport(cfg, snapshotPath, cfg.services);

      vscode.window.showInformationMessage("Snapshot import finished.");
    });
  }

  public async syncWallyPackages(): Promise<void> {
    this.output.appendLine(`[renium] Wally packages: requested at ${new Date().toISOString()}`);
    const requestedConfig = this.getConfig();
    const requestedRoot = this.normalizePathForCompare(requestedConfig.projectRoot);
    const requestedGeneration = this.experienceGeneration;
    await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Syncing Wally packages",
        cancellable: false,
      },
      async (progress) => {
        progress.report({ message: "Waiting for Renium task queue..." });
        await this.enqueue("Sync Wally packages", async () => {
          const runCfg = this.getConfig();
          if (
            this.experienceGeneration !== requestedGeneration
            || this.normalizePathForCompare(runCfg.projectRoot) !== requestedRoot
          ) {
            throw new Error("The active Renium place changed. Run Wally package sync again.");
          }
          if (!(await this.ensureWallyManifest(runCfg))) {
            return;
          }
          const command = this.resolveRustCliPathForCommand(runCfg, "sync-wally-packages");
          this.ensureFileExists(command);
          const args = [
            "sync-wally-packages",
            "-r",
            runCfg.projectRoot,
            "-d",
            runCfg.srcDir,
            "--wally-path",
            runCfg.wallySync.wallyPath,
            "--packages-dir",
            runCfg.wallySync.packagesDir,
            "--target-service",
            runCfg.wallySync.targetService,
            "--target-name",
            runCfg.wallySync.targetName,
            "--server-packages-dir",
            runCfg.wallySync.serverPackagesDir,
            "--server-target-service",
            runCfg.wallySync.serverTargetService,
            "--server-target-name",
            runCfg.wallySync.serverTargetName,
            "--dev-packages-dir",
            runCfg.wallySync.devPackagesDir,
            "--dev-target-service",
            runCfg.wallySync.devTargetService,
            "--dev-target-name",
            runCfg.wallySync.devTargetName,
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
          if (
            typeof parsed.projectRoot !== "string"
            || this.normalizePathForCompare(parsed.projectRoot) !== requestedRoot
            || this.experienceGeneration !== requestedGeneration
            || this.normalizePathForCompare(this.getConfig().projectRoot) !== requestedRoot
          ) {
            throw new Error("Wally package sync returned results for a different Renium place.");
          }
          const importedCount = Array.isArray(parsed.settingsIds) ? parsed.settingsIds.length : 0;
          progress.report({ message: `Imported ${importedCount} package instance(s).` });
          this.output.appendLine(
            `[renium] Wally packages: imported ${importedCount} instance(s) into ${parsed.service ?? runCfg.wallySync.targetService}.${parsed.targetName ?? runCfg.wallySync.targetName}`,
          );
          await this.applyWallyPackagesToStudio(parsed, runCfg);
        });
      },
    );
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
      return "Pull from Studio once before syncing packages.";
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
      "No wally.toml was found at the project root.",
      create,
      "Cancel",
    );
    if (picked !== create) {
      vscode.window.showInformationMessage("Wally package sync cancelled because no wally.toml was found.");
      return false;
    }

    if (!fs.existsSync(manifestPath)) {
      fs.writeFileSync(manifestPath, this.starterWallyManifest(cfg.projectRoot), "utf8");
      this.output.appendLine(`[renium] Wally packages: created ${manifestPath}`);
    }
    vscode.window.showInformationMessage("Created starter wally.toml. Add dependencies, then run Sync Wally Packages again.");
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

  private async applyWallyPackagesToStudio(result: CliSyncWallyPackagesResult, cfg: SyncConfig): Promise<void> {
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
      vscode.window.showInformationMessage(`Synced Wally packages to ${summaryTarget}.`);
      return;
    }

    let shouldApply = mode === "always";
    if (mode === "ask") {
      const apply = "Apply to Studio";
      const picked = await vscode.window.showInformationMessage(
        `Synced Wally packages to ${summaryTarget}.`,
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
      vscode.window.showInformationMessage(`Synced Wally packages locally (${summaryTarget}). Start Serve or live sync before applying to Studio.`);
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
          vscode.window.showInformationMessage(`Synced Wally packages locally (${summaryTarget}). Start Serve or live sync before applying to Studio.`);
          return;
        }
      }
      vscode.window.showInformationMessage(`Applied Wally packages to Studio (${summaryTarget}).`);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] Wally packages Studio apply failed: ${message}`);
      this.output.show(true);
      vscode.window.showWarningMessage(`Wally packages synced locally, but Studio apply failed. ${message}`);
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


  public async linkApply(options: { silent?: boolean; refreshExplorer?: boolean; forceStudio?: boolean; forceTargets?: boolean; forceTargetPaths?: string[][]; taskName?: string; linkId?: string; skipStudio?: boolean; expectedProjectRoot?: string; expectedGeneration?: number } = {}): Promise<CliLinkApplyResult | undefined> {
    let result: CliLinkApplyResult | undefined;
    let executed = false;
    await this.enqueue("Apply packages", async () => {
      const cfg = this.getConfig();
      if (
        (options.expectedGeneration !== undefined && options.expectedGeneration !== this.experienceGeneration)
        || (
          options.expectedProjectRoot
          && this.normalizePathForCompare(options.expectedProjectRoot) !== this.normalizePathForCompare(cfg.projectRoot)
        )
      ) {
        throw new Error("The active project changed before package apply.");
      }
      executed = true;
      const manifestPath = this.linkManifestPath(cfg);
      if (!fs.existsSync(manifestPath)) {
        if (!options.silent) {
          vscode.window.showInformationMessage(
            `No link manifest found at ${manifestPath}. Use "Add Link" first.`,
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
        cfg.srcDir,
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
          `Applied ${applied} link target(s)${warnCount > 0 ? `, ${warnCount} warning(s)` : ""}.`,
        );
      }
    });
    if ((options.expectedProjectRoot || options.expectedGeneration !== undefined) && !executed) {
      throw new Error("Package apply was cancelled before it started.");
    }
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
        vscode.window.showWarningMessage(`Link applied to the project files, but the Studio push failed. ${message}`);
      }
    }
  }


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
        cfg.srcDir,
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
      vscode.window.showInformationMessage(`Broke link on ${service}.${pathSegments[pathSegments.length - 1] ?? ""}. It is now editable.`);
    }
  }


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
    const syncNow = await vscode.window.showInformationMessage("Link added. Apply it now?", "Sync now", "Later");
    if (syncNow === "Sync now") {
      await this.linkApply();
    } else {
      this.invalidateLinkStatusCache();
      await this.refreshFileExplorerSafe();
    }
  }

  public invalidateLinkStatusCache(): void {
    this.linkStatusToken += 1;
    this.linkStatusCache = undefined;
    this.linkStatusInflight = undefined;
    this.linkChangeEmitter.fire();
  }



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


  private globalPackagesDir(): string {
    const custom = (process.env.RENIUM_GLOBAL_PACKAGES_DIR ?? "").trim();
    if (custom) {
      return path.normalize(custom);
    }
    const home = process.env.USERPROFILE || process.env.HOME || "";
    return path.normalize(path.join(home, "Documents", "Renium", "Packages"));
  }


  private linkPackageFolderPath(cfg: SyncConfig): string {
    const folder = cfg.linkSync.folder || "links";
    return path.isAbsolute(folder)
      ? path.normalize(folder)
      : path.normalize(path.join(cfg.projectRoot, folder));
  }


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
    if (this.experienceChangeInProgress || uri.scheme !== "file") {
      return;
    }
    const cfg = this.getConfig();
    const ext = path.extname(uri.fsPath).toLowerCase();
    if (
      (ext !== ".rbsync" && ext !== ".renium")
      || !this.activeLinkPackageSourceKeys.has(this.normalizePathForCompare(uri.fsPath))
    ) {
      return;
    }
    const generation = this.experienceGeneration;
    this.pendingLinkPackageSourcePaths.set(path.normalize(uri.fsPath), {
      projectRoot: cfg.projectRoot,
      generation,
    });
    this.scheduleLinkPackageSourceFlush(cfg.projectRoot, generation);
  }

  public async refreshLinkPackageSourceWatchers(): Promise<void> {
    const cfg = this.getConfig();
    const generation = this.experienceGeneration;
    const resolution = await this.resolveLinkStatus(cfg);
    if (resolution.kind === "failed") {
      throw new Error(resolution.error);
    }
    if (generation !== this.experienceGeneration) {
      return;
    }
    const links = resolution.kind === "success" ? resolution.value.links ?? [] : [];
    const sources = Array.from(new Set(links
      .map((link) => this.absoluteLinkSourcePath(cfg, link.sourcePath))
      .filter((sourcePath): sourcePath is string =>
        typeof sourcePath === "string" && /\.(rbsync|renium)$/i.test(sourcePath))
      .map(path.normalize)));
    for (const watcher of this.linkPackageSourceWatchers) {
      watcher.dispose();
    }
    this.linkPackageSourceWatchers = [];
    this.activeLinkPackageSourceKeys.clear();
    for (const sourcePath of sources) {
      this.activeLinkPackageSourceKeys.add(this.normalizePathForCompare(sourcePath));
      const watcher = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(path.dirname(sourcePath), path.basename(sourcePath)),
      );
      watcher.onDidCreate((uri) => this.onLinkPackageSourceChanged(uri));
      watcher.onDidChange((uri) => this.onLinkPackageSourceChanged(uri));
      watcher.onDidDelete((uri) => this.onLinkPackageSourceChanged(uri));
      this.linkPackageSourceWatchers.push(watcher);
    }
  }

  private scheduleLinkPackageSourceFlush(projectRoot: string, generation: number, delayMs = 500): void {
    if (this.linkPackageSourceApplyTimer) {
      clearTimeout(this.linkPackageSourceApplyTimer);
    }
    this.linkPackageSourceApplyTimer = setTimeout(() => {
      this.linkPackageSourceApplyTimer = undefined;
      void this.flushLinkPackageSourceChanges(projectRoot, generation).catch((error) => {
        this.output.appendLine(`[renium] package source auto-apply failed: ${error instanceof Error ? error.message : String(error)}`);
      });
    }, delayMs);
  }

  private async flushLinkPackageSourceChanges(projectRoot: string, generation: number): Promise<void> {
    if (generation !== this.experienceGeneration || this.experienceChangeInProgress) {
      return;
    }
    const cfg = this.getConfig();
    if (this.normalizePathForCompare(projectRoot) !== this.normalizePathForCompare(cfg.projectRoot)) {
      return;
    }
    const changedPaths = [...this.pendingLinkPackageSourcePaths]
      .filter(([, pending]) =>
        pending.generation === generation
        && this.normalizePathForCompare(pending.projectRoot) === this.normalizePathForCompare(projectRoot))
      .map(([filePath]) => filePath);
    if (changedPaths.length === 0) {
      return;
    }
    const changedKeys = new Set(changedPaths.map((filePath) => this.normalizePathForCompare(filePath)));
    this.invalidateLinkStatusCache();
    const resolution = await this.resolveLinkStatus(cfg);
    if (resolution.kind === "failed") {
      this.scheduleLinkPackageSourceFlush(projectRoot, generation, 1000);
      throw new Error(resolution.error);
    }
    const links = resolution.kind === "success" ? resolution.value.links ?? [] : [];
    const linkIdsByPath = new Map<string, Set<string>>();
    for (const link of links) {
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
      const key = this.normalizePathForCompare(sourcePath);
      const ids = linkIdsByPath.get(key) ?? new Set<string>();
      ids.add(id);
      linkIdsByPath.set(key, ids);
    }
    let appliedAny = false;
    let failed = false;
    const linkIds = new Set(Array.from(linkIdsByPath.values()).flatMap((ids) => [...ids]));
    this.output.appendLine(`[renium] package source changed: applying ${linkIds.size} active link package(s).`);
    for (const changedPath of changedPaths) {
      const ids = linkIdsByPath.get(this.normalizePathForCompare(changedPath)) ?? new Set<string>();
      try {
        for (const linkId of ids) {
          await this.linkApply({
            silent: true,
            refreshExplorer: false,
            linkId,
            skipStudio: true,
            expectedProjectRoot: projectRoot,
            expectedGeneration: generation,
          });
          appliedAny = true;
        }
        this.pendingLinkPackageSourcePaths.delete(changedPath);
      } catch (error) {
        failed = true;
        this.output.appendLine(
          `[renium] package source apply retained for retry: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    if (appliedAny) {
      await this.refreshFileExplorerSafe();
    }
    if (failed) {
      this.scheduleLinkPackageSourceFlush(projectRoot, generation, 1000);
    }
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

  private linkTargetFromFile(uri: vscode.Uri): { service: string; pathSegments: string[] } | undefined {
    const cfg = this.getConfig();
    const srcRoot = this.sourceRoot(cfg);
    if (!this.isPathInside(uri.fsPath, srcRoot)) {
      return undefined;
    }
    const parts = path.relative(srcRoot, uri.fsPath).split(path.sep).filter((segment) => segment.length > 0);
    if (parts.length < 2) {
      return undefined;
    }
    const service = parts[0];
    const fileName = parts[parts.length - 1];
    const identity = inferProjectScriptIdentity(cfg.projectRoot, fileName);
    if (!identity) {
      return undefined;
    }
    if (identity.leafName === undefined) {
      const segments = parts.slice(0, parts.length - 1);
      return segments.length >= 2 ? { service, pathSegments: segments } : undefined;
    }
    return {
      service,
      pathSegments: [...parts.slice(0, parts.length - 1), identity.leafName],
    };
  }


  public async addLinkFromFile(uri: vscode.Uri | undefined): Promise<void> {
    const cfg = this.getConfig();
    const target = uri ?? vscode.window.activeTextEditor?.document.uri;
    if (!target) {
      vscode.window.showInformationMessage(`Right-click a script under ${cfg.srcDir}/ to link it.`);
      return;
    }
    const seed = this.linkTargetFromFile(target);
    if (!seed) {
      vscode.window.showInformationMessage(`That file is not a script under ${cfg.srcDir}/.`);
      return;
    }
    await this.addLinkInteractive(seed);
  }


  public async packInstanceLink(request: { service?: string; pathSegments?: string[]; id?: string; resave?: boolean }): Promise<void> {
    const service = typeof request?.service === "string" ? request.service : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    const requestedLinkId = typeof request?.id === "string" ? request.id.trim() : "";
    const resave = request?.resave === true;
    if (!service || pathSegments.length === 0) {
      vscode.window.showWarningMessage("Select an instance to link.");
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
        cfg.srcDir,
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
        `Packaged ${leaf}. Mirror it to another location (read-only copy)?`,
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
      ? `Saved new version of ${packed?.id ?? requestedLinkId}.`
      : `Linked ${leaf}.`);
  }

  public async resavePackageLink(request: { service?: string; pathSegments?: string[] }): Promise<void> {
    const service = typeof request?.service === "string" ? request.service.trim() : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length === 0) {
      vscode.window.showWarningMessage("Select a linked package root to resave.");
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
      vscode.window.showWarningMessage("Selected instance is not a package link target.");
      return;
    }
    const link = (await this.getLinkPackages(true)).find((candidate) => candidate.id === linkId);
    if (!link || link.sourceKind !== "local" || !link.sourcePath) {
      vscode.window.showWarningMessage(`${linkId} is not a local Renium package, so it cannot be resaved from Explorer.`);
      return;
    }
    const cfg = this.getConfig();
    const sourcePath = this.absoluteLinkSourcePath(cfg, link.sourcePath);
    if (!sourcePath || !this.isManagedPackagePath(cfg, sourcePath) || !/\.(rbsync|renium)$/i.test(sourcePath)) {
      vscode.window.showWarningMessage(`${linkId} is not stored in a Renium packages folder.`);
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
      vscode.window.showWarningMessage("Select a broken package root to relink.");
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
      vscode.window.showWarningMessage("Selected instance is not a package link target.");
      return;
    }
    if (target?.broken !== true) {
      vscode.window.showInformationMessage(`${this.linkTargetDisplay(service, pathSegments)} is already linked.`);
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
    vscode.window.showInformationMessage(`Relinked ${this.linkTargetDisplay(service, pathSegments)}.`);
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
      vscode.window.showInformationMessage("Open or select a linked file first.");
      return;
    }
    const info = await this.linkInfoForFile(target);
    if (!info || info.broken) {
      vscode.window.showInformationMessage("That file is not a read-only link target.");
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
      vscode.window.showInformationMessage("No link source is available for that file.");
      return;
    }
    try {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(info.canonical));
      await vscode.window.showTextDocument(doc, { preview: true });
    } catch (err) {
      vscode.window.showWarningMessage(`Could not open link source. ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  public async showLinkStatus(): Promise<void> {
    const status = await this.getLinkStatus(true);
    if (!status || status.manifestExists === false) {
      vscode.window.showInformationMessage('No links exist yet. Use "Add Link" to create one.');
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
    vscode.window.showInformationMessage(`Inserted "${name}" at ${service}.${leaf}.`);
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
      vscode.window.showInformationMessage("Select a package to delete.");
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
        cfg.srcDir,
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
    vscode.window.showInformationMessage(`Deleted package "${label}".${suffix}`);
  }

  public async viewPackageUses(rawLink: CliLinkStatusLink | string | undefined): Promise<void> {
    const link = typeof rawLink === "string"
      ? (await this.getLinkPackages(true)).find((candidate) => candidate.id === rawLink)
      : rawLink;
    if (!link?.id) {
      vscode.window.showInformationMessage("Select a package to view uses.");
      return;
    }
    const status = await this.getLinkStatus(true);
    const targets = (status?.targets ?? []).filter((target) => target.linkId === link.id);
    const label = (link.rootName && link.rootName.length > 0 ? link.rootName : link.id) ?? link.id;
    if (targets.length === 0) {
      vscode.window.showInformationMessage(`Package "${label}" has no uses.`);
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
      vscode.window.showInformationMessage("This package has no local source to preview.");
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
        vscode.window.showWarningMessage(`Could not open link source. ${err instanceof Error ? err.message : String(err)}`);
        return;
      }
    }
    try {
      const preview = await this.loadPackagePreview(link);
      this.showPackagePreview(preview);
    } catch (err) {
      vscode.window.showWarningMessage(`Could not preview package. ${err instanceof Error ? err.message : String(err)}`);
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

  private async resolveLinkStatus(cfg: SyncConfig): Promise<LinkStatusResolution> {
    const manifestPath = this.linkManifestPath(cfg);
    if (!fs.existsSync(manifestPath)) {
      return { kind: "missing" };
    }
    try {
      const command = this.resolveRustCliPathForCommand(cfg, "link-status");
      if (!fs.existsSync(command)) {
        return { kind: "failed", error: `Renium CLI not found: ${command}` };
      }
      const args = [
        "link-status",
        "-r",
        cfg.projectRoot,
        "-d",
        cfg.srcDir,
        "--manifest",
        cfg.linkSync.manifest,
      ];
      if (cfg.linkSync.cacheDir.length > 0) {
        args.push("--cache-dir", cfg.linkSync.cacheDir);
      }
      const run = await this.runCommand(
        command,
        args,
        cfg.projectRoot,
        "link-status",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
      if (run.code !== 0) {
        return {
          kind: "failed",
          error: `link-status exited with code ${run.code}`,
        };
      }
      const value = this.parseCliJsonObject<CliLinkStatusResult>(run.output);
      if (!value) {
        return { kind: "failed", error: "link-status returned invalid JSON" };
      }
      return { kind: "success", value };
    } catch (error) {
      return {
        kind: "failed",
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }


  public async getLinkStatus(force = false): Promise<CliLinkStatusResult | undefined> {
    const now = Date.now();
    const cfg = this.getConfig();
    const projectRoot = this.normalizePathForCompare(cfg.projectRoot);
    const generation = this.experienceGeneration;
    if (
      !force
      && this.linkStatusCache
      && this.linkStatusCache.projectRoot === projectRoot
      && this.linkStatusCache.generation === generation
      && now - this.linkStatusCache.at < 2000
    ) {
      return this.linkStatusCache.value;
    }
    if (
      !force
      && this.linkStatusInflight
      && this.linkStatusInflight.projectRoot === projectRoot
      && this.linkStatusInflight.generation === generation
    ) {
      return this.linkStatusInflight.promise;
    }
    const token = ++this.linkStatusToken;
    const promise = this.fetchLinkStatus(now, cfg, projectRoot, generation, token);
    const inflight = { projectRoot, generation, token, promise };
    this.linkStatusInflight = inflight;
    try {
      return await promise;
    } finally {
      if (this.linkStatusInflight === inflight) {
        this.linkStatusInflight = undefined;
      }
    }
  }

  private async fetchLinkStatus(
    now: number,
    cfg: SyncConfig,
    projectRoot: string,
    generation: number,
    token: number,
  ): Promise<CliLinkStatusResult | undefined> {
    const resolution = await this.resolveLinkStatus(cfg);
    const value = resolution.kind === "success" ? resolution.value : undefined;
    if (resolution.kind === "failed") {
      this.output.appendLine(`[renium] link-status failed: ${resolution.error}`);
    }
    let currentRoot = "";
    try {
      currentRoot = this.normalizePathForCompare(this.getConfig().projectRoot);
    } catch {
    }
    if (
      this.linkStatusToken === token
      && this.experienceGeneration === generation
      && currentRoot === projectRoot
    ) {
      this.linkStatusCache = { at: now, projectRoot, generation, token, value };
    }
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

      const activeServices = activePath ? this.servicesForProjectSourcePath(activePath, cfg) : [];
      let service = activeServices.length === 1 ? activeServices[0] : undefined;

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

  private captureGitProjectToken(expectedProjectRoot?: string): GitProjectToken {
    const cfg = this.getConfig();
    const projectRoot = this.normalizePathForCompare(cfg.projectRoot);
    if (
      this.experienceChangeInProgress
      || (
        expectedProjectRoot !== undefined
        && this.normalizePathForCompare(expectedProjectRoot) !== projectRoot
      )
    ) {
      throw new Error("The active Renium place changed. Run the Git action again.");
    }
    return { projectRoot, generation: this.experienceGeneration };
  }

  private gitConfigForToken(token: GitProjectToken): SyncConfig {
    const cfg = this.getConfig();
    if (
      this.experienceChangeInProgress
      || this.experienceGeneration !== token.generation
      || this.normalizePathForCompare(cfg.projectRoot) !== token.projectRoot
    ) {
      throw new Error("The active Renium place changed. Run the Git action again.");
    }
    return cfg;
  }

  public async gitStatus(token = this.captureGitProjectToken()): Promise<void> {
    await this.enqueue("Git status", async () => {
      const cfg = this.gitConfigForToken(token);
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      this.output.show(false);
      this.logGitState(state);
      await this.refreshGitView();
    });
  }

  public async gitFetch(token = this.captureGitProjectToken()): Promise<void> {
    await this.enqueue("Git fetch", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      const remote = state.remote ?? cfg.gitSync.remote;
      const result = await this.runGitCommand(cfg, repoRoot, ["fetch", "--prune", remote], "fetch");
      this.ensureGitSuccess(result, "fetch");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Fetched ${remote}.`);
    });
  }

  public async gitPull(token = this.captureGitProjectToken()): Promise<void> {
    await this.enqueue("Git pull", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      let state = await this.inspectGitRepo(cfg, { fetch: cfg.gitSync.autoFetch, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      this.ensureNoGitConflicts(state);
      if (cfg.gitSync.requireCleanWorktreeBeforePull && state.worktreeEntries.length > 0) {
        throw new Error("Pull is blocked because the worktree has local changes. Commit, stash, or discard them before pulling.");
      }

      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);
      if (state.upstream && state.behind === 0 && state.ahead === 0) {
        this.output.appendLine(`[git-sync] pull skipped: ${remote}/${branch} is already up to date.`);
        vscode.window.showInformationMessage("Git pull is already up to date.");
        return;
      }
      if (state.upstream && state.ahead > 0 && state.behind > 0) {
        throw new Error("Pull is blocked because the branch has diverged. Resolve with VS Code Source Control or git manually.");
      }

      const resumeLiveSync = await this.ensureLiveSyncStoppedForGitPull();
      try {
        this.gitConfigForToken(token);
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
        vscode.window.showInformationMessage(`Pulled ${remote}/${branch}.`);
      } catch (error) {
        if (
          resumeLiveSync
          && this.experienceGeneration === token.generation
          && this.normalizePathForCompare(this.getConfig().projectRoot) === token.projectRoot
        ) {
          await this.startLiveSync({ silent: true, bestEffort: true });
        }
        throw error;
      }
    });
  }

  public async gitCommitAndPush(
    options: { pullFromStudioFirst?: boolean } = {},
    token = this.captureGitProjectToken(),
  ): Promise<void> {
    await this.enqueue(options.pullFromStudioFirst ? "Pull from Studio, commit and push" : "Git commit & push", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      await this.maybePullFromStudioBeforeGitPush(cfg, options.pullFromStudioFirst === true);
      this.gitConfigForToken(token);

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
      const commitMessage = await this.gitCommitMessage(cfg, branch);
      await this.stageGitSyncChanges(cfg, repoRoot);
      const staged = await this.gitStagedChanges(cfg, repoRoot);
      if (staged.length === 0) {
        throw new Error("No files were staged after applying the configured Git sync path filters.");
      }
      const commitResult = await this.runGitCommand(cfg, repoRoot, ["commit", "-m", commitMessage], "commit");
      this.ensureGitSuccess(commitResult, "commit");
      const shortSha = await this.gitOutput(cfg, repoRoot, ["rev-parse", "--short", "HEAD"], "read commit sha");
      await this.pushGitBranch(cfg, repoRoot, remote, branch, state.upstream === undefined);
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Pushed ${shortSha} to ${remote}/${branch}.`);
    });
  }

  public async gitPublishBranch(token = this.captureGitProjectToken()): Promise<void> {
    await this.enqueue("Git publish branch", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false, requireRemote: true });
      const repoRoot = this.requireGitRepoRoot(state);
      const remote = state.remote ?? cfg.gitSync.remote;
      const branch = this.resolveGitBranch(cfg, state);
      await this.confirmGitPush(`Publish current branch to ${remote}/${branch}?`, cfg);
      await this.pushGitBranch(cfg, repoRoot, remote, branch, true);
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Published ${remote}/${branch}.`);
    });
  }

  public async gitCreateBranch(token = this.captureGitProjectToken()): Promise<void> {
    const branchName = await vscode.window.showInputBox({
      title: "Create Git Branch",
      prompt: "New branch name",
      validateInput: (value) => this.validateBranchName(value),
    });
    if (!branchName) {
      return;
    }
    await this.enqueue("Git create branch", async () => {
      const cfg = this.gitConfigForToken(token);
      this.ensureWorkspaceTrustedForGitSync();
      const state = await this.inspectGitRepo(cfg, { fetch: false });
      const repoRoot = this.requireGitRepoRoot(state);
      const result = await this.runGitCommand(cfg, repoRoot, ["switch", "-c", branchName.trim()], "create branch");
      this.ensureGitSuccess(result, "create branch");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Created branch ${branchName.trim()}.`);
    });
  }

  public async gitCheckoutBranch(token = this.captureGitProjectToken()): Promise<void> {
    const cfg = this.gitConfigForToken(token);
    const state = await this.inspectGitRepo(cfg, { fetch: false });
    const repoRoot = this.requireGitRepoRoot(state);
    if (state.worktreeEntries.length > 0) {
      vscode.window.showWarningMessage("Checkout is blocked while local changes are present.");
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
      const runCfg = this.gitConfigForToken(token);
      const result = await this.runGitCommand(runCfg, repoRoot, ["switch", branchName], "checkout branch");
      this.ensureGitSuccess(result, "checkout branch");
      await this.refreshGitView();
      vscode.window.showInformationMessage(`Checked out ${branchName}.`);
    });
  }

  public async gitConnectRepo(token = this.captureGitProjectToken()): Promise<void> {
    const cfg = this.gitConfigForToken(token);
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
      this.gitConfigForToken(token);
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
    this.gitConfigForToken(token);
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
      const runCfg = this.gitConfigForToken(token);
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
      vscode.window.showInformationMessage(`Connected ${remote.trim()} to ${redactRemoteUrl(remoteUrl.trim())}.`);
    });
  }

  public async gitOpenRemote(token = this.captureGitProjectToken()): Promise<void> {
    const cfg = this.gitConfigForToken(token);
    const state = await this.inspectGitRepo(cfg, { fetch: false, allowMissing: true });
    this.gitConfigForToken(token);
    const remoteWebUrl = state.view.remoteWebUrl;
    if (!remoteWebUrl) {
      vscode.window.showWarningMessage("No Git remote URL is configured.");
      return;
    }
    await vscode.env.openExternal(vscode.Uri.parse(remoteWebUrl));
  }

  private async getGitViewState(options: { fetch?: boolean; projectRoot: string }): Promise<GitViewState> {
    const token = this.captureGitProjectToken(options.projectRoot);
    const state = await this.inspectGitRepo(this.gitConfigForToken(token), {
      fetch: options.fetch === true,
      allowMissing: true,
    });
    this.gitConfigForToken(token);
    return state.view;
  }

  private async refreshGitView(options: { fetch?: boolean } = {}): Promise<void> {
    if (this.gitViewRefreshSuppression > 0) {
      return;
    }
    await vscode.commands.executeCommand("renium.fileExplorer.refreshGit", options);
  }

  private async runGitViewAction(action: string, expectedProjectRoot: string): Promise<void> {
    const token = this.captureGitProjectToken(expectedProjectRoot);
    this.gitViewRefreshSuppression += 1;
    try {
      switch (action) {
        case "connect":
          await this.gitConnectRepo(token);
          return;
        case "fetch":
          await this.gitFetch(token);
          return;
        case "pull":
          await this.gitPull(token);
          return;
        case "commitPush":
          await this.gitCommitAndPush({}, token);
          return;
        case "pullCommitPush":
          await this.gitCommitAndPush({ pullFromStudioFirst: true }, token);
          return;
        case "publishBranch":
          await this.gitPublishBranch(token);
          return;
        case "createBranch":
          await this.gitCreateBranch(token);
          return;
        case "checkoutBranch":
          await this.gitCheckoutBranch(token);
          return;
        case "openRemote":
          await this.gitOpenRemote(token);
          return;
        case "status":
          await this.gitStatus(token);
          return;
        default:
          return;
      }
    } finally {
      this.gitViewRefreshSuppression -= 1;
    }
  }

  private emptyGitViewState(projectRoot: string, message?: string): GitViewState {
    return {
      ok: false,
      message,
      trusted: vscode.workspace.isTrusted,
      projectRoot,
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
      const view = this.emptyGitViewState(cfg.projectRoot, "Workspace is not trusted. Trust this workspace before using Git sync.");
      if (options.allowMissing) {
        return { view, entries: [], worktreeEntries: [], ahead: 0, behind: 0 };
      }
      throw new Error(view.message);
    }

    const repoResult = await this.runGitCommand(cfg, cfg.projectRoot, ["rev-parse", "--show-toplevel"], "repo root", { quiet: true });
    if (repoResult.code !== 0) {
      const view = this.emptyGitViewState(cfg.projectRoot, "No Git repository is connected. Use Connect Repo to initialize or configure one.");
      if (options.allowMissing) {
        return { view, entries: [], worktreeEntries: [], ahead: 0, behind: 0 };
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

    const branchOverride = cfg.gitSync.branch.trim();
    let upstream: string | undefined;
    let comparisonRef: string | undefined;
    if (branchOverride) {
      const remoteRef = `refs/remotes/${configuredRemote}/${branchOverride}`;
      const remoteBranchResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-parse", "--verify", `${remoteRef}^{commit}`],
        "configured branch",
        { quiet: true },
      );
      if (remoteBranchResult.code === 0) {
        upstream = `${configuredRemote}/${branchOverride}`;
        comparisonRef = remoteRef;
      }
    } else {
      const upstreamResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        "upstream",
        { quiet: true },
      );
      if (upstreamResult.code === 0) {
        upstream = upstreamResult.stdout.trim();
        comparisonRef = "@{u}";
      }
    }
    let ahead = 0;
    let behind = 0;
    if (comparisonRef) {
      const aheadBehindResult = await this.runGitCommand(
        cfg,
        repoRoot,
        ["rev-list", "--left-right", "--count", `HEAD...${comparisonRef}`],
        "ahead/behind",
        { quiet: true },
      );
      if (aheadBehindResult.code === 0) {
        ({ ahead, behind } = parseAheadBehind(aheadBehindResult.stdout));
      }
    }

    const statusResult = await this.runGitCommand(
      cfg,
      repoRoot,
      ["status", "--porcelain=v1", "-z", "-uall"],
      "status",
      { quiet: true },
    );
    this.ensureGitSuccess(statusResult, "status");
    const worktreeEntries = parsePorcelainV1Z(statusResult.stdout);
    const statusScopes = this.defaultGitStageScopes(repoRoot, cfg);
    const entries = worktreeEntries.filter((entry) =>
      this.gitEntryMatchesScopes(entry, statusScopes));
    const counts = summarizeStatus(entries);
    const redactedRemoteUrl = remoteUrl ? redactRemoteUrl(remoteUrl) : undefined;
    const remoteWebUrl = remoteUrlToWebUrl(remoteUrl ?? "");
    const worktreeConflicts = worktreeEntries.filter((entry) => entry.conflicted).length;
    const messages: string[] = [];
    if (!remoteUrl) {
      messages.push(`Remote '${configuredRemote}' is not configured.`);
    } else if (worktreeConflicts > 0) {
      messages.push(`${worktreeConflicts} conflicted file(s) need manual resolution.`);
    } else if (behind > 0) {
      messages.push(`${behind} remote commit(s) available to pull.`);
    }
    const hiddenChanges = worktreeEntries.length - entries.length;
    if (hiddenChanges > 0) {
      messages.push(`${hiddenChanges} repository change(s) outside this place's source files are hidden.`);
    }
    const message = messages.length > 0 ? messages.join(" ") : undefined;
    const view: GitViewState = {
      ok: Boolean(remoteUrl) && worktreeConflicts === 0,
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
    return {
      view,
      entries,
      worktreeEntries,
      repoRoot,
      branch,
      upstream,
      remote: configuredRemote,
      remoteUrl,
      ahead,
      behind,
    };
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
    const conflicts = state.worktreeEntries.filter((entry) => entry.conflicted);
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
      owner: projectProcessOwner(cfg.projectRoot),
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
      for (const service of this.servicesForProjectSourcePath(absolutePath, cfg)) {
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
      .filter((filePath) => this.isProjectSourcePath(filePath, cfg));
    if (srcPaths.length === 0 || cfg.gitSync.applyPulledChangesToStudio === "never") {
      return;
    }
    let apply = cfg.gitSync.applyPulledChangesToStudio === "always";
    if (!apply) {
      const picked = await vscode.window.showInformationMessage(
        `Apply ${srcPaths.length} pulled project source file(s) to Studio now?`,
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
      vscode.window.showInformationMessage("Pulled changes stayed local. Start Serve or live sync before applying to Studio.");
    }
  }

  private async ensureLiveSyncStoppedForGitPull(): Promise<boolean> {
    if (!this.isEditorLiveSyncActive() && !this.liveSyncStartPromise) {
      return false;
    }
    const picked = await vscode.window.showWarningMessage(
      "Git pull can rewrite project source files. Stop Renium live sync before pulling?",
      { modal: true },
      "Stop Live Sync",
    );
    if (picked !== "Stop Live Sync") {
      throw new Error("Git pull cancelled because live sync is active.");
    }
    await this.stopLiveSync({ silent: true });
    return true;
  }

  private async maybePullFromStudioBeforeGitPush(cfg: SyncConfig, forced: boolean): Promise<void> {
    let choice: "pull" | "current" | undefined;
    if (!forced && cfg.gitSync.pullFromStudioBeforePush === "ask") {
      const picked = await vscode.window.showInformationMessage(
        "Pull from Studio before committing to Git?",
        { modal: true },
        "Pull from Studio",
        "Commit Current Files",
      );
      if (!picked) {
        throw new Error("Git commit cancelled before the Studio pull choice.");
      }
      choice = picked === "Pull from Studio" ? "pull" : "current";
    }
    if (!shouldPullFromStudioBeforePush(cfg.gitSync.pullFromStudioBeforePush, forced, choice)) {
      return;
    }
    await this.runExport({
      services: cfg.services,
      runImport: true,
      notifyOnSuccess: false,
      reason: "",
      quietTimings: false,
      destructive: true,
    });
  }

  private async stageGitSyncChanges(cfg: SyncConfig, repoRoot: string): Promise<void> {
    const configuredPaths = cfg.gitSync.stagePaths.map((value) => value.trim()).filter((value) => value.length > 0);
    const hasConfiguredPaths = cfg.gitSync.stageMode === "configuredPaths" && configuredPaths.length > 0;
    const defaultScopes = this.defaultGitStageScopes(repoRoot, cfg);
    const args = cfg.gitSync.includeUntracked
      ? ["add", "-A", "--", ...(hasConfiguredPaths ? configuredPaths : defaultScopes)]
      : ["add", "-u", "--", ...(hasConfiguredPaths ? configuredPaths : defaultScopes)];
    const result = await this.runGitCommand(cfg, repoRoot, args, "stage changes");
    this.ensureGitSuccess(result, "stage changes");
  }

  private async plannedGitStageChanges(cfg: SyncConfig, repoRoot: string): Promise<GitNameStatusEntry[]> {
    const configuredPaths = cfg.gitSync.stagePaths.map((value) => value.trim()).filter((value) => value.length > 0);
    const scopes = cfg.gitSync.stageMode === "configuredPaths" && configuredPaths.length > 0
      ? configuredPaths
      : this.defaultGitStageScopes(repoRoot, cfg);
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

  private defaultGitStageScopes(repoRoot: string, cfg: SyncConfig): string[] {
    const scopes = Array.from(new Set(loadProjectSourceGraph(cfg.projectRoot).locations.map((location) => {
      if (!this.isPathInside(location, repoRoot)) {
        throw new Error(`Project source path is outside the Git repository: ${location}`);
      }
      return path.relative(repoRoot, location).split(path.sep).join("/") || ".";
    })));
    scopes.sort((left, right) => left.length - right.length || left.localeCompare(right));
    return scopes.filter((scope, index) => {
      const key = process.platform === "win32" ? scope.toLowerCase() : scope;
      return !scopes.slice(0, index).some((parent) => {
        const parentKey = process.platform === "win32" ? parent.toLowerCase() : parent;
        return parentKey === "." || key === parentKey || key.startsWith(`${parentKey}/`);
      });
    });
  }

  private gitEntryMatchesScopes(entry: GitStatusEntry, scopes: string[]): boolean {
    const matches = (filePath: string | undefined): boolean => {
      if (!filePath) {
        return false;
      }
      const value = process.platform === "win32" ? filePath.toLowerCase() : filePath;
      return scopes.some((scope) => {
        const key = process.platform === "win32" ? scope.toLowerCase() : scope;
        return key === "." || value === key || value.startsWith(`${key}/`);
      });
    };
    return matches(entry.path) || matches(entry.originalPath);
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
      const message = "serve requires WebSocket bridge transport.";
      if (options.bestEffort) {
        this.output.appendLine(`[renium] serve skipped: ${message}`);
        return;
      }
      throw new Error(message);
    }

    this.bridgeServeRequested = true;
    this.liveSyncOwnsServe = false;
    this.consoleFollowOwnsServe = false;
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
    this.scheduleStudioActionPoll(250);
    this.updateStatusBar();
    if (!options.silent) {
      vscode.window.showInformationMessage(`Serve started — Studio can now connect (ports ${cfg.bridgePorts}).`);
    }
  }

  public async stopServe(options: { silent?: boolean } = {}): Promise<void> {
    await this.stopConsoleFollow({ releaseServe: false });
    if (this.liveSyncWatcher || this.liveSyncStartPromise) {
      await this.stopLiveSync({ silent: true });
    }
    this.bridgeServeRequested = false;
    this.liveSyncOwnsServe = false;
    this.stopStudioActionPolling();
    await this.stopBridgeDaemon();
    this.updateStatusBar();
    if (!options.silent) {
      vscode.window.showInformationMessage("Serve stopped.");
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
      vscode.window.showInformationMessage(`Benchmark full sync saved to ${benchmarkPath}.`);
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
      vscode.window.showInformationMessage(`Modified-default A/B benchmark saved to ${benchmarkPath}.`);
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
      const directArgs = this.withPlaceSelector(cfg, args);

      this.output.show(false);
      this.logResolvedConfig(cfg);
      this.output.appendLine(`[renium] profile command: ${command} ${this.renderArgs(directArgs)}`);
      const result = await this.runCommand(command, directArgs, cfg.projectRoot, "profile-plugin-ops", cfg.progressHeartbeatSeconds);
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

      vscode.window.showInformationMessage(`Plugin profile saved to ${profilePath}.`);
    });
  }

  public async startLiveSync(
    options: { silent?: boolean; bestEffort?: boolean; graphRefresh?: boolean } = {},
  ): Promise<void> {
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

  private async startLiveSyncInternal(
    options: { silent?: boolean; bestEffort?: boolean; graphRefresh?: boolean } = {},
  ): Promise<void> {
    this.stopStudioActionPolling();
    this.liveSyncStartupInProgress = true;
    try {
      if (this.liveSyncWatcher) {
        await this.setEditorLiveSyncEnabled(true);
        const cfg = this.getConfig();
        if (this.liveSyncStopRequested) {
          await this.disposeLiveSyncRuntime();
          await this.setEditorLiveSyncEnabled(false);
          return;
        }
        if (cfg.studioLiveSyncEnabled && !this.studioLiveSyncStarted) {
          if (!await this.ensureLiveSyncServeReady(cfg, options)) {
            return;
          }
          if (this.liveSyncStopRequested) {
            await this.disposeLiveSyncRuntime();
            await this.setEditorLiveSyncEnabled(false);
            return;
          }
          await this.startStudioLiveSyncRuntime(cfg, options);
        }
        if (!options.silent) {
          vscode.window.showInformationMessage("Live sync is already running.");
        }
        return;
      }

      const cfg = this.getConfig();
      if (cfg.transport !== "ws") {
        if (!options.silent) {
          vscode.window.showErrorMessage('Live sync needs the WebSocket transport. Set "renium.transport" to "ws" in Settings.');
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

      this.liveSyncProjectRoot = cfg.projectRoot;
      invalidateProjectSourceGraph(cfg.projectRoot);
      const srcRoot = this.sourceRoot(cfg);
      const sourceGraph = loadProjectSourceGraph(cfg.projectRoot);
      if (sourceGraph.locations.length === 0) {
        const message = `No project source directory exists for ${cfg.projectRoot}`;
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

      const queuePath = (uri: vscode.Uri): void => {
        if (uri.scheme !== "file") {
          return;
        }
        const changed = path.resolve(uri.fsPath);
        if (sourceGraph.manifests.some((manifest) =>
          this.normalizePathForCompare(manifest) === this.normalizePathForCompare(changed))) {
          this.scheduleLiveSyncGraphRefresh(cfg.projectRoot);
          return;
        }
        if (sourceGraph.locations.some((location) =>
          this.normalizePathForCompare(location) === this.normalizePathForCompare(changed))) {
          this.scheduleLiveSyncGraphRefresh(cfg.projectRoot);
        }
        if (this.isProjectSourcePath(changed, cfg, sourceGraph)) {
          this.queueEditorChange(changed);
          return;
        }
        if (sourceGraph.locations.some((location) =>
          this.isPathInside(changed, location) || this.isPathInside(location, changed))) {
          this.scheduleLiveSyncGraphRefresh(cfg.projectRoot);
        }
      };
      const directories = [...sourceGraph.directories];
      if (fs.existsSync(srcRoot) && fs.statSync(srcRoot).isDirectory()
        && !directories.some((root) =>
          this.normalizePathForCompare(root) === this.normalizePathForCompare(srcRoot))) {
        directories.unshift(srcRoot);
      }
      const exactFiles = [...sourceGraph.files, ...sourceGraph.manifests];
      const patterns = new Map<string, vscode.RelativePattern>();
      const addPattern = (target: string, recursive: boolean): void => {
        let base = recursive && fs.existsSync(target) && fs.statSync(target).isDirectory()
          ? target
          : path.dirname(target);
        while (!fs.existsSync(base)) {
          const parent = path.dirname(base);
          if (parent === base) {
            return;
          }
          base = parent;
        }
        const relative = path.relative(base, target).split(path.sep).join("/");
        const pattern = recursive
          ? relative === "" ? "**/*" : `${relative}/**/*`
          : relative;
        const key = `${this.normalizePathForCompare(base)}\0${pattern}`;
        patterns.set(key, new vscode.RelativePattern(base, pattern));
      };
      for (const directory of directories) {
        addPattern(directory, true);
      }
      for (const filePath of exactFiles) {
        addPattern(filePath, false);
      }
      for (const location of sourceGraph.locations) {
        addPattern(location, false);
        addPattern(location, true);
      }
      const watchers = Array.from(patterns.values()).map((pattern) =>
        vscode.workspace.createFileSystemWatcher(pattern));
      this.liveSyncWatcher = watchers.shift();
      this.liveSyncAdditionalWatchers = watchers;
      for (const watcher of [this.liveSyncWatcher, ...this.liveSyncAdditionalWatchers]) {
        watcher?.onDidCreate(queuePath);
        watcher?.onDidChange(queuePath);
        watcher?.onDidDelete(queuePath);
      }
      this.schedulePendingEditorFlushIfNeeded();

      await this.setEditorLiveSyncEnabled(true);
      if (this.liveSyncStopRequested) {
        await this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        return;
      }
      let liveCfg = this.getConfig();
      this.displayedLiveSyncPrompt = false;
      let initialState: StudioChangeState | undefined;
      if (liveCfg.studioLiveSyncEnabled) {
        initialState = await this.getStudioChangeState(liveCfg, liveCfg.services, {
          start: true,
          replaceServices: true,
        });
        liveCfg = this.effectiveLiveSyncConfig(liveCfg);
      }
      if (liveCfg.initialSyncPriority === "editor" || options.graphRefresh === true) {
        const outcome = await this.runInitialEditorLiveSyncPass(srcRoot, options);
        if (outcome === "applied" && initialState) {
          initialState = await this.getStudioChangeState(
            liveCfg,
            liveCfg.services,
            this.studioChangeAckOptions(
              this.studioChangeSeq(initialState),
              initialState.runtimeId,
            ),
          );
        }
      }
      if (this.liveSyncStopRequested) {
        await this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        return;
      }
      await this.startStudioLiveSyncRuntime(liveCfg, {
        ...options,
        initialSync: options.graphRefresh === true ? false : liveCfg.initialSyncPriority === "studio",
        initialState,
      });
      this.updateStatusBar();
      if (!options.silent) {
        vscode.window.showInformationMessage("Editor -> Studio live sync started.");
      }
    } catch (err) {
      await this.disposeLiveSyncRuntime();
      await this.setEditorLiveSyncEnabled(false);
      throw err;
    } finally {
      this.liveSyncStartupInProgress = false;
      if (this.liveSyncGraphRefreshPending) {
        this.liveSyncGraphRefreshPending = false;
        let projectRoot = this.liveSyncProjectRoot;
        if (!projectRoot) {
          try {
            projectRoot = this.getConfig().projectRoot;
          } catch {
          }
        }
        if (projectRoot) {
          this.scheduleLiveSyncGraphRefresh(projectRoot);
        }
      }
      if (!this.liveSyncWatcher && this.bridgeServeRequested) {
        this.scheduleStudioActionPoll();
      }
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
    if (this.consoleFollowOwnsServe) {
      this.consoleFollowOwnsServe = false;
      this.liveSyncOwnsServe = true;
    } else if (startedServe) {
      this.liveSyncOwnsServe = true;
    }
    try {
      await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: true });
      const result = await this.runAutomationOperation(
        cfg.exportCliPath,
        cfg,
        "live-sync-wait-for-plugin",
        AUTOMATION_OP.liveStatus,
        {
          services: this.normalizeServices(cfg.services, cfg.services).join(","),
          bridgeWaitSeconds: this.editorBridgeWaitSeconds(cfg),
          bridgePorts: cfg.bridgePorts,
          contextBound: true,
        },
        { quietWait: true },
      );
      if (result.code !== 0) {
        const detail = result.output
          .replace(/\r\n/g, "\n")
          .split("\n")
          .map((line) => line.trim())
          .filter((line) => line.length > 0)
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
        await this.stopBridgeDaemon();
      }
      if (!options.bestEffort) {
        throw err;
      }
      this.output.appendLine(`[renium] editor live sync waiting for Studio plugin failed: ${err instanceof Error ? err.message : String(err)}`);
      return false;
    }
  }

  private async runInitialEditorLiveSyncPass(
    srcRoot: string,
    options: { bestEffort?: boolean } = {},
  ): Promise<EditorPushOutcome> {
    const cfg = this.getConfig();
    const initialPaths = Array.from(new Set([
      ...await this.collectInitialEditorLiveSyncSettingsPaths(srcRoot),
      ...loadProjectSourceLocations(cfg.projectRoot),
    ]));
    const initialTargets = await this.collectInitialEditorLiveSyncTargetIds(srcRoot, initialPaths);
    if (initialTargets.paths.length === 0) {
      await this.primeEditorLiveSyncCache([], cfg);
      return "applied";
    }
    try {
      const applied = await this.pushEditorPathsNow(initialTargets.paths, {
        force: true,
        skipChangeFilter: true,
        targetSettingsIds: initialTargets.targetSettingsIds,
        taskName: "Editor -> Studio initial sync",
      });
      if (!applied) {
        this.markEditorPathsPending(initialTargets.paths, cfg, cfg.services);
      }
      return applied ? "applied" : "skipped";
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] editor live sync initial pass failed: ${message}`);
      this.markEditorPathsPending(initialTargets.paths, cfg, cfg.services);
      if (!options.bestEffort) {
        throw err;
      }
      return "skipped";
    }
  }

  public async retryEditorInitialSync(): Promise<void> {
    const cfg = this.getConfig();
    const srcRoot = this.sourceRoot(cfg);
    if (!loadProjectSourceLocations(cfg.projectRoot).some((root) => fs.existsSync(root))) {
      throw new Error(`No project source directory exists for ${cfg.projectRoot}`);
    }
    const outcome = await this.runInitialEditorLiveSyncPass(srcRoot);
    vscode.window.showInformationMessage(
      outcome === "applied"
        ? "Editor -> Studio initial sync finished."
        : "Editor -> Studio initial sync is still pending.",
    );
  }

  private async startStudioLiveSyncRuntime(
    cfg: SyncConfig,
    options: { bestEffort?: boolean; initialSync?: boolean; initialState?: StudioChangeState } = {},
  ): Promise<void> {
    if (!cfg.studioLiveSyncEnabled) {
      await this.stopStudioLiveSyncRuntime();
      return;
    }
    try {
      if (this.studioLiveSyncStarted) {
        this.scheduleStudioLiveSyncPoll(cfg, this.resetStudioLiveSyncPollDelay(cfg));
        return;
      }
      const generation = ++this.studioLiveSyncGeneration;
      this.studioLiveSyncStarted = true;
      let initialState = options.initialState ?? await this.getStudioChangeState(cfg, cfg.services, {
        start: true,
        replaceServices: true,
      });
      if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
        return;
      }
      const runtimeCfg = this.effectiveLiveSyncConfig(cfg);
      if (initialState.twoWaySyncEnabled === false) {
        this.output.appendLine("[renium] Studio -> editor live sync is disabled in the Renium Studio plugin settings.");
        await this.stopStudioLiveSyncRuntime();
        return;
      }
      const shouldRunStudioInitialSync = options.initialSync ?? (runtimeCfg.initialSyncPriority === "studio");
      if (shouldRunStudioInitialSync) {
        await this.enqueue("Studio -> Editor initial sync", async () => {
          if (generation !== this.studioLiveSyncGeneration) {
            return;
          }
          await this.runStudioToEditorSync(runtimeCfg.services, runtimeCfg, {
            studioAuthoritative: true,
            generation,
          });
        });
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        initialState = await this.getStudioChangeState(
          runtimeCfg,
          runtimeCfg.services,
          this.studioChangeAckOptions(
            this.studioChangeSeq(initialState),
            initialState.runtimeId,
          ),
        );
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
      }
      this.scheduleStudioLiveSyncPoll(
        runtimeCfg,
        this.resetStudioLiveSyncPollDelay(runtimeCfg),
        generation,
      );
    } catch (err) {
      await this.stopStudioLiveSyncRuntime();
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] Studio -> editor live sync start failed: ${message}`);
      if (!options.bestEffort) {
        throw err;
      }
    }
  }

  private stopStudioLiveSyncRuntime(): Promise<void> {
    const stoppedGeneration = this.studioLiveSyncGeneration;
    this.studioLiveSyncGeneration += 1;
    if (this.studioLiveSyncTimer) {
      clearTimeout(this.studioLiveSyncTimer);
      this.studioLiveSyncTimer = undefined;
    }
    this.studioLiveSyncStarted = false;
    this.studioLiveSyncNextPollMs = DEFAULT_STUDIO_LIVE_SYNC_POLL_MS;
    this.studioToEditorImportInProgress = false;
    if (this.changePreviewResolve) {
      this.changePreviewResolve("pending");
      this.changePreviewResolve = undefined;
    }
    this.changePreviewPanel?.dispose();
    this.changePreviewPanel = undefined;
    this.pendingStudioReviewKey = undefined;
    this.forcedStudioReviewKey = undefined;
    return this.waitForStudioImportTasks(stoppedGeneration);
  }

  public activeLinkManifest(): { filePath: string; autoApply: boolean; projectRoot: string; generation: number } {
    const cfg = this.getConfig();
    return {
      filePath: path.normalize(this.linkManifestPath(cfg)),
      autoApply: cfg.linkSync.autoApply,
      projectRoot: cfg.projectRoot,
      generation: this.experienceGeneration,
    };
  }

  private trackStudioImport<T>(generation: number | undefined, task: Promise<T>): Promise<T> {
    if (generation === undefined) {
      return task;
    }
    let tracked: Promise<T>;
    tracked = task.finally(() => {
      const tasks = this.studioImportTasks.get(generation);
      tasks?.delete(tracked);
      if (tasks?.size === 0) {
        this.studioImportTasks.delete(generation);
      }
    });
    const tasks = this.studioImportTasks.get(generation) ?? new Set<Promise<unknown>>();
    tasks.add(tracked);
    this.studioImportTasks.set(generation, tasks);
    return tracked;
  }

  private async waitForStudioImportTasks(maxGeneration: number): Promise<void> {
    for (;;) {
      const tasks = [...this.studioImportTasks]
        .filter(([generation]) => generation <= maxGeneration)
        .flatMap(([, generationTasks]) => [...generationTasks]);
      if (tasks.length === 0) {
        return;
      }
      await Promise.allSettled(tasks);
    }
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

  private scheduleStudioLiveSyncPoll(
    cfg: SyncConfig,
    delayMs: number,
    generation = this.studioLiveSyncGeneration,
  ): void {
    if (this.studioLiveSyncTimer) {
      clearTimeout(this.studioLiveSyncTimer);
      this.studioLiveSyncTimer = undefined;
    }
    if (
      generation !== this.studioLiveSyncGeneration ||
      !this.studioLiveSyncStarted ||
      !cfg.editorLiveSyncEnabled ||
      !this.liveSyncWatcher ||
      !cfg.studioLiveSyncEnabled
    ) {
      return;
    }
    this.studioLiveSyncTimer = setTimeout(() => {
      this.studioLiveSyncTimer = undefined;
      if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
        return;
      }
      void this.pollStudioLiveSync(generation).catch((err) => {
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        const latestCfg = this.getConfig();
        const nextDelayMs = this.nextErrorStudioLiveSyncPollDelay(latestCfg);
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] Studio -> editor live sync failed: ${message}`);
        this.scheduleStudioLiveSyncPoll(latestCfg, nextDelayMs, generation);
      });
    }, Math.max(MIN_STUDIO_LIVE_SYNC_POLL_MS, delayMs));
  }

  private async pollStudioLiveSync(generation: number): Promise<void> {
    const cfg = this.getConfig();
    if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
      return;
    }
    if (!cfg.editorLiveSyncEnabled || !this.liveSyncWatcher || !cfg.studioLiveSyncEnabled) {
      await this.stopStudioLiveSyncRuntime();
      return;
    }
    if (this.studioLiveSyncInFlightGenerations.has(generation)) {
      this.scheduleStudioLiveSyncPoll(
        cfg,
        this.nextIdleStudioLiveSyncPollDelay(cfg),
        generation,
      );
      return;
    }
    this.studioLiveSyncInFlightGenerations.add(generation);
    let nextDelayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    try {
      const idleWaitMs = this.nextIdleStudioLiveSyncPollDelay(cfg);
      const state = await this.getStudioChangeState(cfg, cfg.services, {
        start: true,
        waitSeconds: this.studioLiveSyncWaitSeconds(idleWaitMs),
      });
      if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
        return;
      }
      const runtimeCfg = this.effectiveLiveSyncConfig(cfg);
      if (state.twoWaySyncEnabled === false) {
        this.output.appendLine("[renium] Studio -> editor live sync was disabled in the Renium Studio plugin settings.");
        await this.stopStudioLiveSyncRuntime();
        return;
      }
      this.studioConflictPolicyOverride = typeof state.conflictResolution === "string" && state.conflictResolution.trim().length > 0
        ? this.normalizeConflictPolicy(state.conflictResolution)
        : undefined;
      const dirtyServices = Array.isArray(state.dirtyServices)
        ? this.normalizeReportedServices(state.dirtyServices, cfg.services)
        : [];
      const pendingServices = this.pendingStudioImportServiceSet(runtimeCfg);
      const blockedServices = dirtyServices.filter((service) =>
        pendingServices.has(service.toLowerCase()));
      if (blockedServices.length > 0) {
        this.output.appendLine(
          `[renium] Studio -> editor sync is waiting for pending editor changes in ${blockedServices.join(", ")}`,
        );
      }
      const importableServices = dirtyServices.filter((service) =>
        !pendingServices.has(service.toLowerCase()));
      const observedSeq = this.studioChangeSeq(state);
      const reviewKey = JSON.stringify([
        state.runtimeId ?? "",
        observedSeq,
        (state.editorActions ?? []).map((action) => action.id ?? "").sort(),
      ]);
      if (importableServices.length > 0) {
        nextDelayMs = this.resetStudioLiveSyncPollDelay(runtimeCfg);
        const ackObservedDirty = this.studioChangeAckOptions(observedSeq, state.runtimeId);
        if (this.shouldDropLikelySelfDirtyStudioState(importableServices, runtimeCfg)) {
          ackObservedDirty.suppressSeconds = Math.max(1, Math.min(4, runtimeCfg.studioLiveSyncPollMs / 1000 + 1.5));
          await this.getStudioChangeState(runtimeCfg, importableServices, ackObservedDirty);
          return;
        }
        let propertyImport: "applied" | "fallback" | "pending" = "fallback";
        try {
          propertyImport = await this.tryApplyStudioPropertyChangesToEditor(
            state,
            importableServices,
            runtimeCfg,
            generation,
            reviewKey,
          );
        } catch (err) {
          const message = err instanceof Error ? err.message : String(err);
          this.output.appendLine(`[renium] Studio -> editor property fast path failed: ${message}`);
        }
        if (propertyImport === "pending") {
          return;
        }
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        if (propertyImport === "fallback") {
          await this.enqueueStudioToEditorSyncIfChanged(
            importableServices,
            runtimeCfg,
            state,
            generation,
          );
        }
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        await this.getStudioChangeState(runtimeCfg, importableServices, ackObservedDirty);
      } else if (dirtyServices.length > 0) {
        nextDelayMs = this.nextIdleStudioLiveSyncPollDelay(runtimeCfg);
      } else {
        nextDelayMs = state.eventDriven === true ? MIN_STUDIO_LIVE_SYNC_POLL_MS : this.nextIdleStudioLiveSyncPollDelay(runtimeCfg);
      }
    } catch (err) {
      if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
        return;
      }
      const latestCfg = this.getConfig();
      nextDelayMs = this.nextErrorStudioLiveSyncPollDelay(latestCfg);
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] Studio -> editor live sync failed: ${message}`);
    } finally {
      this.studioLiveSyncInFlightGenerations.delete(generation);
      if (this.studioLiveSyncStarted && generation === this.studioLiveSyncGeneration) {
        this.scheduleStudioLiveSyncPoll(
          this.effectiveLiveSyncConfig(this.getConfig()),
          nextDelayMs,
          generation,
        );
      }
    }
  }

  private async getStudioChangeState(
    cfg: SyncConfig,
    services: string[],
    options: {
      reset?: boolean;
      replaceServices?: boolean;
      clearPending?: boolean;
      ackSeq?: number;
      ackActionIds?: string[];
      ackActionResults?: Record<string, { ok: boolean; error?: string }>;
      runtimeId?: string;
      start?: boolean;
      stop?: boolean;
      suppressSeconds?: number;
      waitSeconds?: number;
    } = {},
  ): Promise<StudioChangeState> {
    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    if (
      typeof options.ackSeq === "number"
      || options.ackActionIds?.length
      || options.ackActionResults
    ) {
      if (typeof options.runtimeId !== "string" || options.runtimeId.length === 0) {
        throw new Error("Studio change acknowledgment is missing its plugin runtime id.");
      }
    }

    const useEventWait = typeof options.waitSeconds === "number"
      && Number.isFinite(options.waitSeconds)
      && options.waitSeconds > 0;
    const operation = options.stop === true
      ? AUTOMATION_OP.liveStop
      : options.start === false
        ? AUTOMATION_OP.liveStatus
        : AUTOMATION_OP.liveStart;
    const result = await this.runAutomationOperation(
      command,
      cfg,
      "studio-change-state",
      operation,
      {
        bridgeWaitSeconds: this.editorBridgeWaitSeconds(cfg),
        bridgePorts: cfg.bridgePorts,
        services: this.normalizeServices(services, cfg.services).join(","),
        reset: options.reset === true,
        replaceServices: options.replaceServices === true,
        clearPending: options.clearPending === true,
        ...(typeof options.ackSeq === "number" && Number.isFinite(options.ackSeq)
          ? { ackSeq: Math.max(0, Math.floor(options.ackSeq)) }
          : {}),
        ...(options.ackActionIds?.length ? { ackActions: options.ackActionIds.join(",") } : {}),
        ...(options.ackActionResults ? { ackActionResults: options.ackActionResults } : {}),
        ...(typeof options.runtimeId === "string" && options.runtimeId.length > 0
          ? { runtimeId: options.runtimeId }
          : {}),
        ...(typeof options.suppressSeconds === "number" && Number.isFinite(options.suppressSeconds) && options.suppressSeconds > 0
          ? { suppressSeconds: Math.max(0.05, options.suppressSeconds) }
          : {}),
        ...(useEventWait ? { waitSeconds: Math.max(0.05, Math.min(25, options.waitSeconds ?? 0)) } : {}),
        contextBound: true,
      },
      { quietWait: true },
    );
    if (result.code !== 0) {
      throw new Error(`Studio change state exited with code ${result.code}`);
    }
    const state = result.result && typeof result.result === "object"
      ? this.parseStudioChangeStatePayload(JSON.stringify(result.result))
      : this.parseStudioChangeState(result.output);
    if (!state) {
      throw new Error("Studio change state did not return a plugin result.");
    }
    if (state.explicitRuntimeSettings) {
      this.studioRuntimeSettings = state.explicitRuntimeSettings;
    }
    const acknowledgedActions = await this.handleStudioEditorActions(state.editorActions, state.runtimeId);
    if (acknowledgedActions.ids.length > 0) {
      await this.getStudioChangeState(cfg, services, {
        start: false,
        ackActionIds: acknowledgedActions.ids,
        ackActionResults: acknowledgedActions.results,
        runtimeId: state.runtimeId,
      });
    }
    return state;
  }

  private async handleStudioEditorActions(
    actions: StudioEditorAction[] | undefined,
    runtimeId: string | undefined,
  ): Promise<{ ids: string[]; results: Record<string, { ok: boolean; error?: string }> }> {
    if (!Array.isArray(actions)) {
      return { ids: [], results: {} };
    }
    const acknowledged: string[] = [];
    const results: Record<string, { ok: boolean; error?: string }> = {};
    const cfg = this.getConfig();
    for (const action of actions) {
      if (action?.type === "revealScript") {
        if (await this.revealStudioScript(action, cfg) && action.id) {
          acknowledged.push(action.id);
        }
        continue;
      }
      if ((action?.type === "pullFromStudio" || action?.type === "pushToStudio") && action.id) {
        const key = `${runtimeId ?? ""}:${action.id}`;
        const running = this.studioEditorActionRuns.get(key);
        if (running?.done) {
          this.studioEditorActionRuns.delete(key);
          acknowledged.push(action.id);
          results[action.id] = running.error
            ? { ok: false, error: running.error }
            : { ok: true };
        } else if (!running) {
          const state: { done: boolean; error?: string } = { done: false };
          this.studioEditorActionRuns.set(key, state);
          const operation = action.type === "pullFromStudio"
            ? this.pullFromStudio()
            : this.pushToStudio();
          void operation
            .catch((error) => {
              state.error = (error instanceof Error ? error.message : String(error)).slice(0, 500);
            })
            .finally(() => {
              state.done = true;
            });
        }
      }
    }
    return { ids: acknowledged, results };
  }

  private async revealStudioScript(action: StudioEditorAction, cfg: SyncConfig): Promise<boolean> {
    const sourcePath = this.resolveStudioSourcePathFromSourcemap(cfg, action);
    if (!sourcePath || !fs.existsSync(sourcePath)) {
      this.output.appendLine("[renium] reveal script is waiting for a matching source file.");
      return false;
    }
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(sourcePath));
    const editor = await vscode.window.showTextDocument(document, { preview: false });
    return this.normalizePathForCompare(editor.document.uri.fsPath) ===
      this.normalizePathForCompare(sourcePath);
  }

  private scheduleStudioActionPoll(delayMs = 750): void {
    this.stopStudioActionPolling();
    if (
      !this.bridgeServeRequested ||
      !this.isBridgeDaemonRunning() ||
      this.liveSyncWatcher ||
      this.liveSyncStartPromise
    ) {
      return;
    }
    this.studioActionPollTimer = setTimeout(() => {
      this.studioActionPollTimer = undefined;
      void this.pollStudioActions();
    }, delayMs);
  }

  private stopStudioActionPolling(): void {
    if (this.studioActionPollTimer) {
      clearTimeout(this.studioActionPollTimer);
      this.studioActionPollTimer = undefined;
    }
  }

  private async pollStudioActions(): Promise<void> {
    if (this.studioActionPollInFlight) {
      this.scheduleStudioActionPoll();
      return;
    }
    const cfg = this.getConfig();
    if (
      !this.bridgeServeRequested ||
      !this.isBridgeDaemonRunning() ||
      this.liveSyncWatcher ||
      this.liveSyncStartPromise
    ) {
      return;
    }
    this.studioActionPollInFlight = true;
    try {
      await this.getStudioChangeState(cfg, cfg.services, { start: false });
    } catch {
    } finally {
      this.studioActionPollInFlight = false;
      this.scheduleStudioActionPoll();
    }
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

  private studioChangeAckOptions(
    observedSeq: number | undefined,
    runtimeId: string | undefined,
  ): { reset?: boolean; ackSeq?: number; runtimeId?: string; start?: boolean; suppressSeconds?: number } {
    const options: {
      reset?: boolean;
      ackSeq?: number;
      runtimeId?: string;
      start?: boolean;
      suppressSeconds?: number;
    } = { start: true };
    if (observedSeq !== undefined) {
      options.ackSeq = observedSeq;
      options.runtimeId = runtimeId;
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

  private async runStudioToEditorSync(
    services: string[],
    cfg: SyncConfig,
    options: { studioAuthoritative?: boolean; generation?: number } = {},
  ): Promise<void> {
    const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg, { quietProbe: true });
    if (diff.changedServices.length === 0) {
      return;
    }
    await this.importStudioLiveSyncSnapshot(diff.changedServices, cfg, diff.fingerprintsByService, {
      quietLog: true,
      studioAuthoritative: options.studioAuthoritative === true,
      generation: options.generation,
    });
  }

  private async enqueueStudioToEditorSyncIfChanged(
    services: string[],
    cfg: SyncConfig,
    state?: StudioChangeState,
    generation?: number,
  ): Promise<void> {
    const run = async (): Promise<void> => {
      if (
        generation !== undefined &&
        (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
      ) {
        return;
      }
      let taskStarted = false;
      const taskName = "Studio -> Editor sync";
      try {
        const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg, { quietProbe: true });
        if (
          generation !== undefined &&
          (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
        ) {
          return;
        }
        if (diff.changedServices.length === 0) {
          return;
        }
        taskStarted = true;
        this.setActiveTask(taskName);
        this.logStudioChanges(state, "full", diff.changedServices);
        await this.importStudioLiveSyncSnapshot(diff.changedServices, cfg, diff.fingerprintsByService, {
          quietLog: true,
          generation,
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        if (taskStarted) {
          this.output.appendLine(`[renium] task failed: ${taskName}: ${message}`);
          this.output.show(true);
          vscode.window.showErrorMessage(`${taskName} failed. ${message}`);
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
    return await this.diffServicesBySnapshotFingerprint(selectedServices, cfg);
  }

  private importStudioLiveSyncSnapshot(
    services: string[],
    cfg: SyncConfig,
    fingerprintsByService?: Map<string, string>,
    options: { quietLog?: boolean; studioAuthoritative?: boolean; generation?: number } = {},
  ): Promise<void> {
    return this.trackStudioImport(
      options.generation,
      this.importStudioLiveSyncSnapshotInner(services, cfg, fingerprintsByService, options),
    );
  }

  private async importStudioLiveSyncSnapshotInner(
    services: string[],
    cfg: SyncConfig,
    fingerprintsByService?: Map<string, string>,
    options: { quietLog?: boolean; studioAuthoritative?: boolean; generation?: number } = {},
  ): Promise<void> {
    if (
      options.generation !== undefined &&
      (options.generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
    ) {
      return;
    }
    const selectedServices = this.normalizeServices(services, cfg.services);
    this.studioToEditorImportInProgress = true;
    const capturedLocalEdits = await this.captureLocalScriptEditsForServices(
      selectedServices,
      cfg,
      options.studioAuthoritative !== true,
    );
    try {
      const changedPaths = await this.runRustImport(
        cfg,
        this.resolveSnapshotPath(cfg),
        selectedServices,
        { quietLog: options.quietLog === true },
      );
      const stillCurrent = options.generation === undefined
        || (options.generation === this.studioLiveSyncGeneration && this.studioLiveSyncStarted);
      const affectedKeys = new Set(
        changedPaths.map((filePath) => this.normalizePathForCompare(filePath)),
      );
      const affectedLocalEdits = new Map(
        [...capturedLocalEdits].filter(([filePath]) =>
          affectedKeys.has(this.normalizePathForCompare(filePath))),
      );
      const survivingLocalEdits = this.reconcileLocalEditsAfterFullImport(
        changedPaths,
        cfg,
        affectedLocalEdits,
      );
      this.commitStudioSnapshotFingerprints(selectedServices, fingerprintsByService);
      await this.updateEditorLiveSyncCacheAfterPush(changedPaths, cfg);
      if (survivingLocalEdits.length > 0) {
        this.invalidateEditorLiveSyncCacheEntries(survivingLocalEdits, cfg);
        for (const filePath of survivingLocalEdits) {
          this.pendingEditorPaths.add(filePath);
        }
        void this.persistPendingEditorPaths(cfg.projectRoot);
        if (stillCurrent) {
          this.scheduleEditorLiveSyncFlush(0);
        }
      }
      if (stillCurrent) {
        try {
          await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", selectedServices);
        } catch {
        }
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
    mode: "property" | "structural",
    generation?: number,
    reviewKey?: string,
  ): Promise<"apply" | "full" | "discard" | "pending"> {
    if (
      generation !== undefined
      && (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
    ) {
      return "pending";
    }
    if (
      reviewKey &&
      this.pendingStudioReviewKey === reviewKey &&
      this.forcedStudioReviewKey !== reviewKey
    ) {
      return "pending";
    }
    if (reviewKey && this.forcedStudioReviewKey === reviewKey) {
      this.forcedStudioReviewKey = undefined;
      this.pendingStudioReviewKey = undefined;
    }
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
      pathSegments: string[];
      pathOrdinals: number[];
      identity: string;
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
      const ordinals = Array.isArray(change.pathOrdinals)
        ? change.pathOrdinals.map((ordinal) => Math.max(1, Math.floor(Number(ordinal) || 1)))
        : [];
      const service = String(change.service ?? "");
      const settingsId = String(change.settingsId ?? "");
      return {
        service,
        path: segments.join("."),
        pathSegments: segments,
        pathOrdinals: ordinals,
        identity: settingsId || JSON.stringify([service, segments, ordinals]),
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
      const ordinals = Array.isArray(change.pathOrdinals)
        ? change.pathOrdinals.map((ordinal) => Math.max(1, Math.floor(Number(ordinal) || 1)))
        : [];
      const service = String(change.service ?? segments[0] ?? "");
      const settingsId = String(change.settingsId ?? "");
      const path = segments.join(".");
      const identity = settingsId || JSON.stringify([service, segments, ordinals]);
      const statusKey = `${action}\0${service}\0${identity}`;
      if (seenStatus.has(statusKey)) {
        continue;
      }
      seenStatus.add(statusKey);
      const className = String(change.className ?? "");
      rows.push({
        service,
        path,
        pathSegments: segments,
        pathOrdinals: ordinals,
        identity,
        leaf: segments[segments.length - 1],
        className,
        icon: iconAssetNameForClass(className || "Folder", iconNames),
        scope: "__status",
        property: "",
        status: action,
      });
    }

    return await this.showChangeReviewPanel(
      rows,
      changeCount,
      cfg.changesThreshold,
      mode,
      `review ${changeCount} Studio changes`,
      generation,
      reviewKey,
    );
  }

  private async showChangeReviewPanel(
    rows: Array<{
      service: string;
      path: string;
      pathSegments: string[];
      pathOrdinals: number[];
      identity: string;
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
    mode: "property" | "structural",
    title: string,
    generation?: number,
    reviewKey?: string,
  ): Promise<"apply" | "full" | "discard" | "pending"> {
    if (
      generation !== undefined &&
      (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
    ) {
      return "pending";
    }
    const defaultDecision = "pending";
    if (this.changePreviewResolve) {
      this.changePreviewResolve(defaultDecision);
      this.changePreviewResolve = undefined;
    }
    this.changePreviewPanel?.dispose();

    const assetsUri = vscode.Uri.joinPath(this.context.extensionUri, "assets");
    const panel = vscode.window.createWebviewPanel(
      "reniumChangePreview",
      `${title}`,
      vscode.ViewColumn.Active,
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [assetsUri] },
    );
    this.changePreviewPanel = panel;
    const assetBase = panel.webview.asWebviewUri(assetsUri).toString();
    panel.webview.html = this.buildChangePreviewHtml(rows, changeCount, threshold, assetBase, mode);

    return await new Promise<"apply" | "full" | "discard" | "pending">((resolve) => {
      let settled = false;
      const finish = (decision: "apply" | "full" | "discard" | "pending"): void => {
        if (settled) {
          return;
        }
        settled = true;
        this.changePreviewResolve = undefined;
        this.changePreviewPanel = undefined;
        if (decision === "pending" && reviewKey) {
          const firstDeferral = this.pendingStudioReviewKey !== reviewKey;
          this.pendingStudioReviewKey = reviewKey;
          if (firstDeferral) {
            void vscode.window.showInformationMessage(
              "Studio changes are waiting for review.",
              "Review changes",
            ).then((choice) => {
              if (choice !== "Review changes" || this.pendingStudioReviewKey !== reviewKey) {
                return;
              }
              this.forcedStudioReviewKey = reviewKey;
              try {
                const cfg = this.effectiveLiveSyncConfig(this.getConfig());
                this.scheduleStudioLiveSyncPoll(cfg, MIN_STUDIO_LIVE_SYNC_POLL_MS);
              } catch {
              }
            });
          }
        } else if (reviewKey && this.pendingStudioReviewKey === reviewKey) {
          this.pendingStudioReviewKey = undefined;
          this.forcedStudioReviewKey = undefined;
        }
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
      panel.onDidDispose(() => finish(defaultDecision));
    });
  }

  private buildChangePreviewHtml(
    rows: Array<{
      service: string;
      path: string;
      pathSegments: string[];
      pathOrdinals: number[];
      identity: string;
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
    mode: "property" | "structural",
  ): string {
    const payload = JSON.stringify(rows).replace(/</g, "\\u003c");
    const instanceCount = new Set(rows.map((row) => `${row.service}\0${row.identity}`)).size;
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
  .kicker b { color: var(--ink-mid); font-weight: 700; }
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
  .subtitle .threshold { color: var(--amber); font-weight: 620; font-variant-numeric: tabular-nums; }
  .toolbar { display: flex; align-items: center; gap: 10px; margin-top: 14px; }
  .filter {
    flex: none; width: 240px; font-family: inherit; font-size: 12px;
    color: var(--ink); background: var(--surface); border: 1px solid var(--edge);
    border-radius: 7px; padding: 5px 11px; outline: none;
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .filter:focus { border-color: var(--ink-dim); background: var(--surface-hover); }
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
  .countdown-fill { height: 100%; width: 100%; background: var(--green); transition: width 1s linear, background 1s linear; border-radius: 2px; }
  button {
    font-family: inherit; font-size: 12.5px; font-weight: 590; letter-spacing: 0.005em;
    padding: 8px 18px; border-radius: 8px;
    border: 1px solid transparent; cursor: pointer; flex: none;
    transition: transform 0.1s ease, box-shadow 0.15s ease, background 0.15s ease, color 0.15s ease;
  }
  button:active { transform: translateY(1px) scale(0.98); }
  .apply { background: #2e9e5b; color: #fff; }
  .apply:hover { background: #35b268; }
  body.vscode-light .apply, body.vscode-high-contrast-light .apply { background: #1f8a4c; }
  body.vscode-light .apply:hover, body.vscode-high-contrast-light .apply:hover { background: #23994f; }
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
    <div class="subtitle"><b>${changeCount}</b> change${changeCount === 1 ? "" : "s"} across <b>${instanceCount}</b> instance${instanceCount === 1 ? "" : "s"} in ${services.join(", ") || "your project"}. This batch is over your review threshold of <span class="threshold">${threshold}</span>.</div>
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
    ${mode === "structural"
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
    const segments = Array.isArray(row.pathSegments) && row.pathSegments.length > 0
      ? row.pathSegments
      : [row.leaf];
    const ordinals = Array.isArray(row.pathOrdinals) ? row.pathOrdinals : [];
    let node = root;
    for (let index = 0; index < segments.length; index += 1) {
      const segment = segments[index];
      const ordinal = Math.max(1, Number(ordinals[index]) || 1);
      const segmentKey = JSON.stringify([segment, ordinal]);
      if (!node.children.has(segmentKey)) {
        node.children.set(segmentKey, {
          name: segment,
          ordinal,
          children: new Map(),
          changes: null,
          icon: null,
          className: "",
          status: null,
        });
      }
      node = node.children.get(segmentKey);
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
  function childKey(child) {
    return JSON.stringify([child.name, Math.max(1, Number(child.ordinal) || 1)]);
  }
  function nodeMatches(node, pathKey) {
    if (!filterText) return true;
    const cached = matchCache.get(pathKey);
    if (cached !== undefined) return cached;
    let out = node.name.toLowerCase().includes(filterText)
      || (node.className && node.className.toLowerCase().includes(filterText))
      || (node.changes && node.changes.some((c) => c.property.toLowerCase().includes(filterText)));
    if (!out) {
      for (const child of node.children.values()) {
        if (nodeMatches(child, pathKey + "/" + childKey(child))) { out = true; break; }
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
      key = key + "/" + childKey(child);
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
      flattenNode(child, key + "/" + childKey(child), depth + 1);
    }
  }

  function rebuildFlat() {
    flat = [];
    matchCache.clear();
    for (const service of root.children.values()) {
      flattenNode(service, childKey(service), 0);
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
    fillEl.style.background = "hsl(" + Math.round(120 * secs / 90) + ", 55%, 45%)";
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

  private tryApplyStudioPropertyChangesToEditor(
    state: StudioChangeState,
    dirtyServices: string[],
    cfg: SyncConfig,
    generation?: number,
    reviewKey?: string,
  ): Promise<"applied" | "fallback" | "pending"> {
    return this.trackStudioImport(
      generation,
      this.tryApplyStudioPropertyChangesToEditorInner(
        state,
        dirtyServices,
        cfg,
        generation,
        reviewKey,
      ),
    );
  }

  private async tryApplyStudioPropertyChangesToEditorInner(
    state: StudioChangeState,
    dirtyServices: string[],
    cfg: SyncConfig,
    generation?: number,
    reviewKey?: string,
  ): Promise<"applied" | "fallback" | "pending"> {
    if (
      generation !== undefined &&
      (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
    ) {
      return "pending";
    }
    const fullSyncServices = Array.isArray(state.fullSyncServices)
      ? state.fullSyncServices.map((service) => service.trim()).filter((service) => service.length > 0)
      : [];
    const propertyChanges = Array.isArray(state.propertyChanges) ? state.propertyChanges : [];
    const trackedChanges = this.studioChangeLogEntries(state, dirtyServices);
    const changeCount = trackedChanges.length > 0 ? trackedChanges.length : propertyChanges.length;
    const reviewStudioBatches = this.canDisplayLiveSyncPrompt(cfg);
    if (propertyChanges.length === 0 || fullSyncServices.length > 0) {
      if (changeCount > cfg.changesThreshold && trackedChanges.length > 0 && reviewStudioBatches) {
        const decision = await this.showStudioChangePreview(
          propertyChanges,
          trackedChanges,
          changeCount,
          cfg,
          "structural",
          generation,
          reviewKey,
        );
        if (
          generation !== undefined &&
          (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
        ) {
          return "pending";
        }
        if (decision === "pending") {
          return "pending";
        }
        this.displayedLiveSyncPrompt = true;
        if (decision === "discard") {
          this.output.appendLine(
            `[renium] Studio -> editor: ${changeCount} changes skipped from review; editor files were not updated.`,
          );
          return "applied";
        }
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes reviewed; running protected full import.`,
        );
      }
      return "fallback";
    }

    if (changeCount > cfg.changesThreshold) {
      if (!reviewStudioBatches) {
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes exceed liveSync.changesThreshold=${cfg.changesThreshold}; using protected full import.`,
        );
        return "fallback";
      }
      const decision = await this.showStudioChangePreview(
        propertyChanges,
        trackedChanges,
        changeCount,
        cfg,
        "property",
        generation,
        reviewKey,
      );
      if (
        generation !== undefined &&
        (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
      ) {
        return "pending";
      }
      if (decision === "pending") {
        return "pending";
      }
      this.displayedLiveSyncPrompt = true;
      if (decision === "full") {
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes reviewed; running protected full import.`,
        );
        return "fallback";
      }
      if (decision === "discard") {
        this.output.appendLine(
          `[renium] Studio -> editor: ${changeCount} changes skipped from review; editor files were not updated.`,
        );
        return "applied";
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
        return "fallback";
      }
    }

    this.ensureFileExists(cfg.rustCliPath);
    const batchEntries: Array<{
      service: string;
      settingsId?: string;
      className: string;
      pathSegments: string[];
      pathOrdinals: number[];
      scope: string;
      property: string;
      value: unknown;
    }> = [];
    const sourceValues = new Map<number, string>();
    const pathsToSuppress = new Set<string>();
    for (const change of propertyChanges) {
      const service = String(change.service ?? "").trim();
      const property = String(change.property ?? "").trim();
      if (!dirtySet.has(service) || property.length === 0) {
        return "fallback";
      }

      const settingsFile = existingReniumSettingsFile(cfg.projectRoot, cfg.srcDir, service);
      if (!fs.existsSync(settingsFile) && !fs.existsSync(this.projectManifestPath(cfg))) {
        return "fallback";
      }

      const settingsId = String(change.settingsId ?? "").trim();
      const pathSegments = Array.isArray(change.pathSegments)
        ? change.pathSegments.map((segment) => String(segment))
        : [];
      if (settingsId.length === 0 && (pathSegments.length < 1 || pathSegments[0] !== service)) {
        return "fallback";
      }
      let value = change.value ?? null;
      if (property === "Source" && (change.scope ?? "property") === "property") {
        if (typeof change.value !== "string") {
          return "fallback";
        }
        const sourcePath = this.resolveStudioSourcePathFromSourcemap(cfg, change);
        if (!sourcePath) {
          return "fallback";
        }
        const finalSource = this.reconcileStudioSourceWithLocalEdits(cfg, sourcePath, change.value);
        value = finalSource;
        sourceValues.set(batchEntries.length, finalSource);
        pathsToSuppress.add(sourcePath);
      }
      pathsToSuppress.add(settingsFile);
      batchEntries.push({
        service,
        settingsId: settingsId.length > 0 ? settingsId : undefined,
        className: String(change.className ?? ""),
        pathSegments,
        pathOrdinals: Array.isArray(change.pathOrdinals)
          ? change.pathOrdinals.map((ordinal) => Number(ordinal))
          : [],
        scope: change.scope ?? "property",
        property,
        value,
      });
    }

    const batchFile = path.join(
      os.tmpdir(),
      `renium-property-batch-${process.pid}-${crypto.randomUUID()}.json`,
    );
    fs.writeFileSync(batchFile, JSON.stringify(batchEntries), "utf8");
    if (
      generation !== undefined &&
      (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted)
    ) {
      fs.rmSync(batchFile, { force: true });
      return "pending";
    }
    this.noteProgrammaticEditorWrite({ paths: Array.from(pathsToSuppress), durationMs: 5000 });
    let result: CommandRunResult;
    try {
      result = await this.runCommand(
        cfg.rustCliPath,
        [
          "bytecode-apply-property-batch",
          "--project-root",
          cfg.projectRoot,
          "--input",
          batchFile,
        ],
        cfg.projectRoot,
        "studio-property-import",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
    } finally {
      try {
        fs.rmSync(batchFile, { force: true });
      } catch {
      }
    }
    if (result.code !== 0) {
      throw new Error(`Rust property import exited with code ${result.code}`);
    }
    const stillCurrent = generation === undefined
      || (generation === this.studioLiveSyncGeneration && this.studioLiveSyncStarted);
    let batchResult: {
      changedPaths?: unknown[];
      sourcePaths?: Array<{ entryIndex?: unknown; path?: unknown }>;
    };
    try {
      batchResult = JSON.parse(result.output.trim()) as typeof batchResult;
    } catch {
      throw new Error("Rust property import returned invalid JSON");
    }
    const changedFiles = new Set(
      (Array.isArray(batchResult.changedPaths) ? batchResult.changedPaths : [])
        .map((filePath) => path.resolve(String(filePath))),
    );
    const changedSettingsFiles = new Set(
      Array.from(changedFiles).filter((filePath) => isReniumSettingsFileName(path.basename(filePath))),
    );
    for (const sourceResult of Array.isArray(batchResult.sourcePaths) ? batchResult.sourcePaths : []) {
      const entryIndex = Number(sourceResult.entryIndex);
      const sourcePath = path.resolve(String(sourceResult.path ?? ""));
      const finalContent = sourceValues.get(entryIndex);
      if (Number.isInteger(entryIndex) && sourcePath.length > 0 && finalContent !== undefined) {
        this.writeSyncBase(cfg, sourcePath, finalContent);
      }
    }
    this.noteProgrammaticEditorWrite({
      paths: Array.from(new Set([...pathsToSuppress, ...changedFiles])),
      durationMs: 5000,
      refreshCache: true,
    });

    const changedServices = Array.from(dirtySet);
    await this.updateEditorLiveSyncCacheAfterPush(Array.from(changedFiles), cfg);
    if (stillCurrent && changedSettingsFiles.size > 0) {
      try {
        await vscode.commands.executeCommand("renium.fileExplorer.refreshPropertyChanges", Array.from(changedSettingsFiles));
      } catch {
      }
    }
    this.studioToEditorImportSuppressUntilMs = Date.now() + Math.max(1000, Math.min(3000, cfg.studioLiveSyncPollMs * 2));
    this.studioToEditorLastSyncEndedAt = Date.now();
    this.logStudioChanges(state, "property", changedServices);
    return stillCurrent ? "applied" : "pending";
  }










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

    if (sameSourceText(ours, theirs)) {
      return ours;
    }

    const base = this.readSyncBase(cfg, sourcePath);
    if (base !== undefined && sameSourceText(ours, base)) {
      return withLineEnding(theirs, ours.includes("\r\n") ? "\r\n" : "\n");
    }

    return this.mergeSourceAgainstBase(cfg, sourcePath, ours, theirs, base);
  }






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






  private canDisplayLiveSyncPrompt(cfg: SyncConfig): boolean {
    return cfg.displayPrompts === "always" ||
      (cfg.displayPrompts === "initial" && !this.displayedLiveSyncPrompt);
  }

  private surfaceConflictChoice(
    cfg: SyncConfig,
    sourcePath: string,
    localBackup: string | undefined,
    studioBackup: string | undefined,
  ): void {
    const label = path.basename(sourcePath);
    this.output.appendLine(`[renium] conflict on ${sourcePath}: kept your local version; Studio's copy is in .renium/conflicts.`);
    if (!this.canDisplayLiveSyncPrompt(cfg)) {
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
        `${label} was edited in both Studio and your editor. Kept your version; Studio's copy is saved in .renium/conflicts.`,
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
    const srcRoot = this.sourceRoot(cfg);
    if (this.isPathInside(sourcePath, srcRoot)) {
      return path.join(cfg.projectRoot, ".renium", "sync-base", path.relative(srcRoot, sourcePath));
    }
    const owner = loadProjectSourceLocations(cfg.projectRoot)
      .filter((location) => {
        try {
          return fs.statSync(location).isFile()
            ? this.normalizePathForCompare(sourcePath) === this.normalizePathForCompare(location)
            : this.isPathInside(sourcePath, location);
        } catch {
          return path.extname(location) !== ""
            ? this.normalizePathForCompare(sourcePath) === this.normalizePathForCompare(location)
            : this.isPathInside(sourcePath, location);
        }
      })
      .sort((left, right) => right.length - left.length)[0];
    if (!owner) {
      return undefined;
    }
    const ownerKey = crypto
      .createHash("sha256")
      .update(this.normalizePathForCompare(owner))
      .digest("hex")
      .slice(0, 20);
    const relative = path.extname(owner) !== "" && !fs.existsSync(owner)
      ? path.basename(sourcePath)
      : fs.existsSync(owner) && fs.statSync(owner).isFile()
        ? path.basename(sourcePath)
        : path.relative(owner, sourcePath);
    return path.join(
      cfg.projectRoot,
      ".renium",
      "sync-base",
      "external",
      ownerKey,
      relative,
    );
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







  private async captureLocalScriptEditsForServices(
    services: string[],
    cfg: SyncConfig,
    captureUnbased: boolean = true,
  ): Promise<Map<string, CapturedLocalScriptState>> {
    const captured = new Map<string, CapturedLocalScriptState>();
    const candidates = new Set(await this.liveSyncLuaSourceFiles(services, cfg));
    for (const filePath of await this.liveSyncBaseSourceFiles(services, cfg)) {
      candidates.add(filePath);
    }
    for (const filePath of candidates) {
      const base = this.readSyncBase(cfg, filePath);
      let content: string | undefined;
      try {
        content = fs.readFileSync(filePath, "utf8");
      } catch {
      }
      if (base === undefined && !captureUnbased) {
        continue;
      }
      if (content === undefined ? base !== undefined : base === undefined || content !== base) {
        captured.set(filePath, { content, base });
      }
    }
    return captured;
  }






  private reconcileLocalEditsAfterFullImport(
    affectedPaths: string[],
    cfg: SyncConfig,
    captured: Map<string, CapturedLocalScriptState>,
  ): string[] {
    const surviving: string[] = [];
    for (const [filePath, local] of captured) {
      let newDisk: string | undefined;
      try {
        newDisk = fs.readFileSync(filePath, "utf8");
      } catch {
      }
      if (local.content === newDisk) {
        if (newDisk === undefined) {
          const basePath = this.syncBasePathForSource(cfg, filePath);
          if (basePath) {
            try {
              fs.rmSync(basePath, { force: true });
            } catch {
            }
          }
          continue;
        }
        this.writeSyncBase(cfg, filePath, newDisk);
        continue;
      }
      if (local.content === undefined) {
        if (newDisk === local.base) {
          fs.rmSync(filePath, { force: true });
          surviving.push(filePath);
          continue;
        }
        const studioBackup = newDisk === undefined
          ? undefined
          : this.backupConflictCopy(cfg, filePath, newDisk, "studio");
        const policy = this.resolveConflictPolicy(cfg);
        if (policy === "studio") {
          if (newDisk !== undefined) {
            this.writeSyncBase(cfg, filePath, newDisk);
          }
        } else {
          fs.rmSync(filePath, { force: true });
          surviving.push(filePath);
        }
        this.surfacePresenceConflict(cfg, filePath, "local deletion", studioBackup);
        continue;
      }
      if (newDisk === undefined) {
        const localBackup = this.backupConflictCopy(cfg, filePath, local.content, "local");
        const policy = this.resolveConflictPolicy(cfg);
        if (policy !== "studio") {
          this.writeUtf8FileIfChanged(filePath, local.content);
          surviving.push(filePath);
        } else {
          const basePath = this.syncBasePathForSource(cfg, filePath);
          if (basePath) {
            try {
              fs.rmSync(basePath, { force: true });
            } catch {
            }
          }
        }
        this.surfacePresenceConflict(cfg, filePath, "Studio deletion", localBackup);
        continue;
      }
      const resolved = this.mergeSourceAgainstBase(
        cfg,
        filePath,
        local.content,
        newDisk,
        local.base,
      );
      if (resolved === newDisk) {
        this.writeSyncBase(cfg, filePath, newDisk);
        continue;
      }
      this.writeUtf8FileIfChanged(filePath, resolved);
      surviving.push(filePath);
    }

    for (const filePath of affectedPaths.filter((filePath) => this.isLuaSourcePath(filePath))) {
      if (captured.has(filePath) || this.syncBaseExists(cfg, filePath)) {
        continue;
      }
      try {
        this.writeSyncBase(cfg, filePath, fs.readFileSync(filePath, "utf8"));
      } catch {
      }
    }
    return surviving;
  }

  private async liveSyncBaseSourceFiles(services: string[], cfg: SyncConfig): Promise<string[]> {
    const baseRoot = path.join(cfg.projectRoot, ".renium", "sync-base");
    if (!fs.existsSync(baseRoot)) {
      return [];
    }
    const selectedServices = this.normalizeServices(services, cfg.services);
    const selected = new Set(selectedServices.map((service) => service.toLowerCase()));
    const sourceGraph = loadProjectSourceGraph(cfg.projectRoot);
    const locations = sourceGraph.locations;
    const externalByKey = new Map<string, string>();
    for (const location of locations) {
      const ownerKey = crypto
        .createHash("sha256")
        .update(this.normalizePathForCompare(location))
        .digest("hex")
        .slice(0, 20);
      externalByKey.set(ownerKey, location);
    }
    const paths = new Set<string>();
    for (const entry of await this.collectInitialEditorLiveSyncPathsAsync(baseRoot)) {
      const relative = path.relative(baseRoot, entry);
      const parts = relative.split(path.sep);
      let sourcePath: string | undefined;
      if (parts[0] === "external" && parts.length >= 3) {
        const owner = externalByKey.get(parts[1]);
        if (owner) {
          const ownerIsFile = fs.existsSync(owner)
            ? fs.statSync(owner).isFile()
            : path.extname(owner) !== "";
          sourcePath = ownerIsFile ? owner : path.join(owner, ...parts.slice(2));
        }
      } else if (parts.length > 0 && selected.has(parts[0].toLowerCase())) {
        sourcePath = path.join(this.sourceRoot(cfg), relative);
      }
      if (
        sourcePath
        && this.isLuaSourcePath(sourcePath)
        && this.projectSourcePathMatchesServices(sourcePath, selected, sourceGraph)
      ) {
        paths.add(path.resolve(sourcePath));
      }
    }
    return Array.from(paths).sort((left, right) => this.comparePathsForStableOrder(left, right));
  }

  private surfacePresenceConflict(
    cfg: SyncConfig,
    sourcePath: string,
    detail: string,
    backupPath: string | undefined,
  ): void {
    const policy = this.resolveConflictPolicy(cfg);
    this.output.appendLine(
      `[renium] conflict on ${sourcePath}: ${detail} conflicted with the other side; policy=${policy}.`,
    );
    if (!this.canDisplayLiveSyncPrompt(cfg)) {
      return;
    }
    this.displayedLiveSyncPrompt = true;
    const action = backupPath ? "Open Backup" : undefined;
    void vscode.window
      .showWarningMessage(
        `${path.basename(sourcePath)} was deleted on one side and edited on the other. Applied the ${policy} conflict policy.`,
        ...(action ? [action] : []),
      )
      .then((choice) => {
        if (choice === action && backupPath) {
          void vscode.window.showTextDocument(vscode.Uri.file(backupPath));
        }
      });
  }

  private async liveSyncLuaSourceFiles(services: string[], cfg: SyncConfig): Promise<string[]> {
    const paths = new Set<string>();
    const srcRoot = this.sourceRoot(cfg);
    const selectedServices = this.normalizeServices(services, cfg.services);
    const selected = new Set(selectedServices.map((service) => service.toLowerCase()));
    for (const service of selectedServices) {
      const serviceDir = path.join(srcRoot, service);
      if (fs.existsSync(serviceDir)) {
        for (const filePath of await this.collectInitialEditorLiveSyncPathsAsync(serviceDir)) {
          if (this.isLuaSourcePath(filePath)) {
            paths.add(path.resolve(filePath));
          }
        }
      }
    }
    const sourceGraph = loadProjectSourceGraph(cfg.projectRoot);
    for (const location of this.projectSourceScanLocations(selectedServices, sourceGraph)) {
      if (this.isPathInside(location, srcRoot)) {
        continue;
      }
      let stat: fs.Stats;
      try {
        stat = fs.statSync(location);
      } catch {
        continue;
      }
      if (stat.isFile()) {
        if (this.isLuaSourcePath(location)) {
          paths.add(path.resolve(location));
        }
        continue;
      }
      if (!stat.isDirectory()) {
        continue;
      }
      for (const filePath of await this.collectInitialEditorLiveSyncPathsAsync(location)) {
        if (this.isLuaSourcePath(filePath)) {
          paths.add(path.resolve(filePath));
        }
      }
    }
    return Array.from(paths).sort((left, right) => this.comparePathsForStableOrder(left, right));
  }

  private projectSourceScanLocations(
    services: string[],
    sourceGraph: ProjectSourceGraph,
  ): string[] {
    const selected = new Set(services.map((service) => service.toLowerCase()));
    const locations = new Set<string>();
    for (const location of sourceGraph.locations) {
      const normalized = this.normalizePathForCompare(location);
      const owners = sourceGraph.owners.filter((owner) =>
        this.normalizePathForCompare(owner.location) === normalized);
      if (owners.some((owner) => owner.target[0] && selected.has(owner.target[0].toLowerCase()))) {
        locations.add(location);
      }
      if (!owners.some((owner) => owner.target.length === 0)) {
        continue;
      }
      let directory = false;
      try {
        directory = fs.statSync(location).isDirectory();
      } catch {
        directory = path.extname(location) === "";
      }
      if (!directory) {
        locations.add(location);
        continue;
      }
      for (const service of services) {
        locations.add(path.join(location, service));
      }
    }
    return Array.from(locations).sort((left, right) => this.comparePathsForStableOrder(left, right));
  }

  private projectSourcePathMatchesServices(
    filePath: string,
    selected: Set<string>,
    sourceGraph: ProjectSourceGraph,
  ): boolean {
    const matches = sourceGraph.owners
      .filter((owner) => {
        let isFile = false;
        try {
          isFile = fs.statSync(owner.location).isFile();
        } catch {
          isFile = path.extname(owner.location) !== "";
        }
        return isFile
          ? this.normalizePathForCompare(filePath) === this.normalizePathForCompare(owner.location)
          : this.isPathInside(filePath, owner.location);
      })
      .sort((left, right) =>
        path.resolve(right.location).split(path.sep).length
        - path.resolve(left.location).split(path.sep).length);
    if (matches.length === 0) {
      return false;
    }
    const specificity = path.resolve(matches[0].location).split(path.sep).length;
    for (const owner of matches) {
      if (path.resolve(owner.location).split(path.sep).length !== specificity) {
        break;
      }
      const fixedService = owner.target[0];
      if (fixedService) {
        if (selected.has(fixedService.toLowerCase())) {
          return true;
        }
        continue;
      }
      const relative = path.relative(owner.location, filePath);
      const service = relative.split(path.sep)[0];
      if (!service || service === "." || selected.has(service.toLowerCase())) {
        return true;
      }
    }
    return false;
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
      const srcRoot = this.sourceRoot(cfg);
      let rel = this.isPathInside(sourcePath, srcRoot) ? path.relative(srcRoot, sourcePath) : undefined;
      if (rel === undefined) {
        const syncBase = this.syncBasePathForSource(cfg, sourcePath);
        if (syncBase) {
          rel = path.relative(path.join(cfg.projectRoot, ".renium", "sync-base"), syncBase);
        }
      }
      if (rel === undefined) {
        const ownerKey = crypto
          .createHash("sha256")
          .update(this.normalizePathForCompare(sourcePath))
          .digest("hex")
          .slice(0, 20);
        rel = path.join("external", ownerKey, path.basename(sourcePath));
      }
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
          `Concurrent edits to ${label} — ${manual ? "conflict markers written; resolve manually" : detail}. Backups in .renium/conflicts.`,
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
    if (!this.isProjectSourcePath(sourcePath, cfg)) {
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

  private async diffServicesBySnapshotFingerprint(
    services: string[],
    cfg: SyncConfig,
  ): Promise<StudioSnapshotDiff> {
    const changedServices: string[] = [];
    const fingerprintsByService = new Map<string, string>();
    for (const service of services) {
      const fingerprint = await this.snapshotFingerprintForService(service, cfg);
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

  private async snapshotFingerprintForService(
    service: string,
    cfg: SyncConfig,
  ): Promise<string | undefined> {
    const snapshotRoot = this.resolveSnapshotPath(cfg);
    const paths = await this.collectSnapshotFingerprintPaths(snapshotRoot, service);
    if (paths.length === 0) {
      return undefined;
    }

    const rootFile = path.join(snapshotRoot, service + ".json");
    const hash = crypto.createHash("sha256");
    let hashedAnyFile = false;
    for (const filePath of paths) {
      let stat: fs.Stats;
      try {
        stat = await fs.promises.stat(filePath);
      } catch {
        continue;
      }
      if (!stat.isFile()) {
        continue;
      }
      const relPath = this.normalizePathForCompare(path.relative(snapshotRoot, filePath));
      const content = await fs.promises.readFile(filePath);
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

  private async collectSnapshotFingerprintPaths(
    snapshotRoot: string,
    service: string,
  ): Promise<string[]> {
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
          entries = await fs.promises.readdir(dir, { withFileTypes: true });
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

  private async collectInitialEditorLiveSyncPathsAsync(srcRoot: string): Promise<string[]> {
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
        entries = await fs.promises.readdir(dir, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const entry of entries) {
        const fullPath = path.join(dir, entry.name);
        if (entry.isDirectory()) {
          stack.push(fullPath);
        } else if (entry.isFile()) {
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
    }
    return [
      ...Array.from(settingsPathsByDirectory.values()).sort((a, b) => this.comparePathsForStableOrder(a, b)),
      ...otherPaths.sort((a, b) => this.comparePathsForStableOrder(a, b)),
    ];
  }

  private async collectInitialEditorLiveSyncSettingsPaths(srcRoot: string): Promise<string[]> {
    return (await this.collectInitialEditorLiveSyncPathsAsync(srcRoot))
      .filter((filePath) => isReniumSettingsFileName(path.basename(filePath)));
  }

  private async collectInitialEditorLiveSyncTargetIds(
    srcRoot: string,
    settingsPaths: string[],
  ): Promise<{ paths: string[]; targetSettingsIds: string[] }> {
    const cfg = this.getConfig();
    const result = await this.runCommand(
      cfg.exportCliPath,
      [
        "bt",
        "-d",
        srcRoot,
        "-s",
        cfg.services.join(","),
      ],
      cfg.projectRoot,
      "editor-live-sync-target-scan",
      cfg.progressHeartbeatSeconds,
      { quietLog: true, timeoutMs: 10_000 },
    );
    if (result.code !== 0) {
      const message = result.output.trim();
      this.output.appendLine(`[renium] editor live sync initial target scan failed: ${message || `exit ${result.code}`}`);
      return { paths: settingsPaths, targetSettingsIds: [] };
    }
    const parsed = this.parseCliJsonObject<{ paths?: unknown; targetSettingsIds?: unknown }>(result.output);
    if (!parsed) {
      this.output.appendLine("[renium] editor live sync initial target scan returned invalid JSON");
      return { paths: settingsPaths, targetSettingsIds: [] };
    }
    const rawPaths = Array.isArray(parsed.paths)
      ? parsed.paths
      : [];
    const rawIds = Array.isArray(parsed.targetSettingsIds)
      ? parsed.targetSettingsIds
      : [];
    const validSettingsPaths = new Set(settingsPaths.map((settingsPath) => this.normalizePathForCompare(settingsPath)));
    const paths = rawPaths
      .map((value) => String(value))
      .filter((value) => validSettingsPaths.has(this.normalizePathForCompare(value)));
    paths.push(...settingsPaths.filter((value) => !isReniumSettingsFileName(path.basename(value))));
    return {
      paths: [...new Set(paths)],
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

  private async editorLiveSyncFileHash(filePath: string): Promise<string | undefined> {
    try {
      const stat = await fs.promises.stat(filePath);
      if (!stat.isFile()) {
        return undefined;
      }
      const hash = crypto.createHash("sha256");
      hash.update(await fs.promises.readFile(filePath));
      return `sha256:${stat.size}:${hash.digest("hex")}`;
    } catch {
      return undefined;
    }
  }

  private async primeEditorLiveSyncCache(paths: string[], cfg: SyncConfig): Promise<void> {
    const cache = this.emptyEditorLiveSyncCache(cfg.projectRoot);
    for (const filePath of paths) {
      const hash = await this.editorLiveSyncFileHash(filePath);
      if (hash) {
        cache.files[this.editorLiveSyncCacheKey(filePath, cfg.projectRoot)] = hash;
      }
    }
    this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
  }

  private async classifyEditorLiveSyncPaths(
    paths: string[],
    cfg: SyncConfig,
  ): Promise<{ pushable: string[]; unchanged: string[]; blocked: string[] }> {
    const { cache, existed } = this.loadEditorLiveSyncCache(cfg.projectRoot);
    const seen = new Set<string>();
    const observations: { path: string; key: string; hash: string | undefined }[] = [];

    for (const filePath of paths) {
      const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
      if (!seen.add(key)) {
        continue;
      }
      const hash = await this.editorLiveSyncFileHash(filePath);
      observations.push({ path: filePath, key, hash });
    }

    const changed = changedEditorLiveSyncPaths(observations, existed, cache.files);
    const changedKeys = new Set(changed.map((filePath) => this.normalizePathForCompare(filePath)));
    const unchanged = observations
      .filter((observation) => !changedKeys.has(this.normalizePathForCompare(observation.path)))
      .map((observation) => observation.path);
    const { pushable, blocked } = await this.partitionUnresolvedConflictMarkerPaths(changed);
    return { pushable, unchanged, blocked };
  }

  private async partitionUnresolvedConflictMarkerPaths(
    paths: string[],
  ): Promise<{ pushable: string[]; blocked: string[] }> {
    const pushable: string[] = [];
    const blocked: string[] = [];
    for (const filePath of paths) {
      const key = this.normalizePathForCompare(filePath);
      if (this.isLuaSourcePath(filePath) && await this.fileHasConflictMarkers(filePath)) {
        this.output.appendLine(
          `[renium] live-sync: holding back ${filePath} — unresolved conflict markers present; resolve them to resume syncing this file.`,
        );
        if (!this.conflictMarkerWarnedKeys.has(key)) {
          this.conflictMarkerWarnedKeys.add(key);
          void vscode.window.showWarningMessage(
            `${path.basename(filePath)} has unresolved merge conflict markers and won't sync to Studio until resolved.`,
            "Open File",
          ).then((choice) => {
            if (choice === "Open File") {
              void vscode.window.showTextDocument(vscode.Uri.file(filePath));
            }
          });
        }
        blocked.push(filePath);
        continue;
      }
      if (this.conflictMarkerWarnedKeys.delete(key)) {
        this.output.appendLine(`[renium] live-sync: ${filePath} conflict markers resolved; resuming sync.`);
      }
      pushable.push(filePath);
    }
    return { pushable, blocked };
  }

  private async fileHasConflictMarkers(filePath: string): Promise<boolean> {
    let content: string;
    try {
      content = await fs.promises.readFile(filePath, "utf8");
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
    this.persistPendingEditorPaths();
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
    const rawPaths = Array.isArray(request.paths)
      ? request.paths
      : request.paths !== undefined
        ? [request.paths]
        : [];
    const paths = [...new Set(rawPaths
      .map((value) => String(value ?? "").trim())
      .filter((value) => value.length > 0)
      .map((value) => path.isAbsolute(value) ? path.resolve(value) : path.resolve(cfg.projectRoot, value))
      .filter((value) => this.isProjectSourcePath(value, cfg)))];
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
      void this.updateEditorLiveSyncCacheAfterPush(cachePaths, cfg);
    }
    if (request.forcePending === true) {
      this.invalidateEditorLiveSyncCacheEntries(paths, cfg);
      this.markEditorPathsPending(paths, cfg);
      this.scheduleEditorLiveSyncFlush(0);
    }
    this.schedulePendingEditorFlushIfNeeded(now);
  }

  private async updateEditorLiveSyncCacheAfterPush(paths: string[], cfg: SyncConfig): Promise<void> {
    const write = this.editorLiveSyncCacheWrites
      .catch(() => undefined)
      .then(async () => {
        const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
        for (const filePath of paths) {
          const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
          const hash = await this.editorLiveSyncFileHash(filePath);
          if (hash) {
            cache.files[key] = hash;
          } else {
            delete cache.files[key];
          }
        }
        this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
      });
    this.editorLiveSyncCacheWrites = write.catch((error) => {
      this.output.appendLine(
        `[renium] editor live sync cache update failed: ${error instanceof Error ? error.message : String(error)}`,
      );
    });
    await write;
  }

  private invalidateEditorLiveSyncCachePaths(paths: string[], cfg: SyncConfig): void {
    if (paths.length === 0) {
      return;
    }
    const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
    for (const filePath of paths) {
      delete cache.files[this.editorLiveSyncCacheKey(filePath, cfg.projectRoot)];
    }
    this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
  }

  private async suppressStudioLiveSyncAfterEditorPush(paths: string[], cfg: SyncConfig): Promise<void> {
    if (!cfg.studioLiveSyncEnabled || !cfg.editorLiveSyncEnabled || !this.liveSyncWatcher) {
      return;
    }

    const services = [...new Set(
      paths.flatMap((filePath) => this.servicesForProjectSourcePath(filePath, cfg)),
    )];
    if (services.length === 0) {
      return;
    }

    this.scheduleStudioLiveSyncPoll(cfg, this.resetStudioLiveSyncPollDelay(cfg));
  }

  public async stopLiveSync(options: { silent?: boolean } = {}): Promise<void> {
    this.liveSyncStopRequested = true;
    const wasRunning = this.liveSyncWatcher !== undefined || this.liveSyncStartPromise !== undefined || this.editorLiveSyncRuntimeEnabled;
    const startup = this.liveSyncStartPromise;
    if (this.studioLiveSyncStarted) {
      const cfg = this.getConfig();
      try {
        await this.getStudioChangeState(cfg, cfg.services, { start: false, stop: true });
      } catch (err) {
        this.output.appendLine(
          `[renium] Studio change tracking stop failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      }
    }
    await this.disposeLiveSyncRuntime();
    await this.setEditorLiveSyncEnabled(false);
    if (startup) {
      try {
        await startup;
      } catch {
      }
      await this.disposeLiveSyncRuntime();
      await this.setEditorLiveSyncEnabled(false);
    }
    if (this.liveSyncOwnsServe) {
      this.liveSyncOwnsServe = false;
      if (this.consoleFollowRunning) {
        this.consoleFollowOwnsServe = true;
        this.scheduleStudioActionPoll();
      } else {
        this.bridgeServeRequested = false;
        await this.stopBridgeDaemon();
      }
    } else if (!this.bridgeServeRequested) {
      await this.stopBridgeDaemon();
    } else {
      this.scheduleStudioActionPoll();
    }
    this.updateStatusBar();
    if (!options.silent) {
      vscode.window.showInformationMessage(wasRunning
        ? "Editor -> Studio live sync stopped."
        : "Live sync is not running.");
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
    const projectRoot = typeof options.projectRoot === "string" && options.projectRoot.length > 0
      ? options.projectRoot
      : cfg.projectRoot;
    const retain = (): void => {
      this.markEditorPathsPendingAtRoot(changedPaths, cfg, projectRoot, options.pendingServices);
    };
    if (this.experienceChangeInProgress) {
      retain();
      void vscode.window.showWarningMessage("Wait for the place change to finish.");
      return false;
    }
    if (this.normalizePathForCompare(projectRoot) !== this.normalizePathForCompare(cfg.projectRoot)) {
      retain();
      return false;
    }
    if (!options.force && !this.isEditorLiveSyncActive()) {
      this.disposeLiveSyncRuntime();
      retain();
      this.updateStatusBar();
      return false;
    }
    if (options.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      retain();
      return false;
    }

    let pushed = false;
    let completed = false;
    try {
      await this.enqueue(options.taskName ?? "Editor -> Studio sync", async () => {
        const runCfg = this.getConfig();
        if (
          this.normalizePathForCompare(projectRoot) !== this.normalizePathForCompare(runCfg.projectRoot)
          || (!options.force && !this.isEditorLiveSyncActive())
        ) {
          this.output.appendLine("[renium] editor direct sync cancelled before apply");
          return;
        }
        if (options.force === true && !this.canUseStudioPushPipeline()) {
          this.noteStudioPushSkipped("serve/live sync is not active");
          return;
        }
        const classified = options.skipChangeFilter === true
          ? {
            ...await this.partitionUnresolvedConflictMarkerPaths(changedPaths),
            unchanged: [] as string[],
          }
          : await this.classifyEditorLiveSyncPaths(changedPaths, runCfg);
        const pathsToPush = classified.pushable;
        if (classified.unchanged.length > 0) {
          this.clearAppliedEditorPaths(classified.unchanged);
        }
        if (classified.blocked.length > 0) {
          this.markEditorPathsPendingAtRoot(
            classified.blocked,
            runCfg,
            projectRoot,
            options.pendingServices,
          );
        }
        if (pathsToPush.length === 0) {
          completed = true;
          pushed = classified.blocked.length === 0;
          return;
        }
        this.logEditorChangedPaths("Editor -> Studio", pathsToPush, runCfg);
        const outcome = await this.runEditorPush(pathsToPush, runCfg, options);
        if (outcome === "skipped") {
          this.markEditorPathsPendingAtRoot(pathsToPush, runCfg, projectRoot, options.pendingServices);
        } else {
          this.clearAppliedEditorPaths(pathsToPush);
        }
        completed = true;
        pushed = outcome === "applied" && classified.blocked.length === 0;
      });
    } catch (error) {
      retain();
      throw error;
    }
    if (!completed) {
      retain();
    }
    return pushed;
  }

  public async pushEditorPropertyNow(request: EditorPropertyPushRequest): Promise<EditorPushOutcome> {
    const cfg = this.getConfig();
    const service = String(request.service ?? "").trim();
    const projectRoot = typeof request.projectRoot === "string" && request.projectRoot.length > 0
      ? request.projectRoot
      : cfg.projectRoot;
    const settingsFile = String(request.settingsFile ?? "").trim();
    const changedPaths = Array.from(new Set([
      ...(Array.isArray(request.changedPaths) ? request.changedPaths : []),
      ...(settingsFile.length > 0 ? [settingsFile] : []),
    ].map((filePath) => String(filePath).trim()).filter((filePath) => filePath.length > 0)));
    const retain = (): void => {
      this.markEditorPathsPendingAtRoot(changedPaths, cfg, projectRoot, service ? [service] : undefined);
    };
    if (this.experienceChangeInProgress) {
      retain();
      void vscode.window.showWarningMessage("Wait for the place change to finish.");
      return "skipped";
    }
    if (!request.force && !this.isEditorLiveSyncActive()) {
      retain();
      return "skipped";
    }
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      retain();
      return "skipped";
    }
    if (
      request.projectRoot
      && this.normalizePathForCompare(request.projectRoot) !== this.normalizePathForCompare(cfg.projectRoot)
    ) {
      retain();
      return "skipped";
    }

    const property = String(request.property ?? "").trim();
    const pathSegments = Array.isArray(request.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || !property || pathSegments.length === 0) {
      retain();
      throw new Error("Editor property push requires service, property, and path segments.");
    }

    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
    const args = [
      "prop",
      "-r",
      projectRoot,
      "-d",
      cfg.srcDir,
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
    const settingsId = String(request.settingsId ?? "").trim();
    if (settingsId.length > 0) {
      args.push("-i", settingsId);
    }

    const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
    const automationParameters = {
      editor: true,
      bridgeWaitSeconds,
      bridgePorts: cfg.bridgePorts,
      service,
      className: String(request.className ?? ""),
      pathSegments,
      pathOrdinals: Array.isArray(request.pathOrdinals) ? request.pathOrdinals : [],
      scope: request.scope ?? "property",
      property,
      value: request.value ?? null,
      ...(settingsId.length > 0 ? { settingsId } : {}),
      overridePackages: this.effectiveLiveSyncConfig(cfg).overridePackages,
    };
    let result: CommandRunResult;
    if (usePersistentBridge) {
      result = await this.runAutomationOperation(
        command,
        cfg,
        "editor-property",
        AUTOMATION_OP.setProperty,
        automationParameters,
        { quietWait: true },
      );
      if (
        result.automationError?.c === "rejected"
        && result.automationError.n === "review-prepare"
      ) {
        result = await this.runReviewedAutomationOperation(
          command,
          cfg,
          "editor-property",
          AUTOMATION_OP.setProperty,
          automationParameters,
          { quietWait: true },
        );
      }
    } else {
      result = await this.runCommand(
        command,
        this.withPlaceSelector(cfg, args),
        cfg.projectRoot,
        "editor-property",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
    }
    if (result.code !== 0) {
      retain();
      throw new Error(`Editor property push exited with code ${result.code}`);
    }
    const summary = this.parseEditorPushSummary(result.output, result.result);
    if (!summary) {
      retain();
      throw new Error("Editor property push did not return a Studio apply result.");
    }
    if (summary.skippedByReview === true) {
      retain();
      return "skipped";
    }
    const errors = this.summaryNumber(summary, "errors");
    if (summary.ok === false || errors > 0) {
      retain();
      throw new Error("Studio rejected or failed editor property apply.");
    }

    if (changedPaths.length > 0) {
      this.clearAppliedEditorPaths(changedPaths);
      await this.updateEditorLiveSyncCacheAfterPush(changedPaths, cfg);
      await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);
    }
    return "applied";
  }

  public async pushEditorDeleteNow(request: EditorDeletePushRequest): Promise<EditorPushOutcome> {
    const cfg = this.getConfig();
    const service = String(request.service ?? "").trim();
    const projectRoot = typeof request.projectRoot === "string" && request.projectRoot.length > 0
      ? request.projectRoot
      : cfg.projectRoot;
    const settingsFile = String(request.settingsFile ?? "").trim();
    const changedPaths = Array.from(new Set([
      ...(Array.isArray(request.changedPaths) ? request.changedPaths : []),
      ...(settingsFile.length > 0 ? [settingsFile] : []),
    ].map((filePath) => String(filePath).trim()).filter((filePath) => filePath.length > 0)));
    const retain = (): void => {
      this.markEditorPathsPendingAtRoot(changedPaths, cfg, projectRoot, service ? [service] : undefined);
    };
    if (this.experienceChangeInProgress) {
      retain();
      void vscode.window.showWarningMessage("Wait for the place change to finish.");
      return "skipped";
    }
    if (!request.force && !this.isEditorLiveSyncActive()) {
      retain();
      return "skipped";
    }
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      retain();
      return "skipped";
    }
    if (
      request.projectRoot
      && this.normalizePathForCompare(request.projectRoot) !== this.normalizePathForCompare(cfg.projectRoot)
    ) {
      retain();
      return "skipped";
    }

    const pathSegments = Array.isArray(request.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length <= 1) {
      retain();
      throw new Error("Editor delete push requires service and a non-root path.");
    }

    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
    const args = [
      "del",
      "-r",
      projectRoot,
      "-d",
      cfg.srcDir,
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
      ? await this.runAutomationOperation(
        command,
        cfg,
        "editor-delete",
        AUTOMATION_OP.remove,
        {
          editor: true,
          bridgeWaitSeconds,
          bridgePorts: cfg.bridgePorts,
          service,
          className: String(request.className ?? ""),
          pathSegments,
          pathOrdinals: Array.isArray(request.pathOrdinals) ? request.pathOrdinals : [],
          ...(settingsId.length > 0 ? { settingsId } : {}),
          overridePackages: this.effectiveLiveSyncConfig(cfg).overridePackages,
        },
        { quietWait: true },
      )
      : await this.runCommand(
        command,
        this.withPlaceSelector(cfg, args),
        cfg.projectRoot,
        "editor-delete",
        cfg.progressHeartbeatSeconds,
        { quietLog: true },
      );
    if (result.code !== 0) {
      retain();
      throw new Error(`Editor delete push exited with code ${result.code}`);
    }
    const summary = this.parseEditorPushSummary(result.output, result.result);
    if (!summary) {
      retain();
      throw new Error("Editor delete push did not return a Studio apply result.");
    }
    if (summary.skippedByReview === true) {
      retain();
      return "skipped";
    }
    const errors = this.summaryNumber(summary, "errors");
    if (summary.ok === false || errors > 0) {
      retain();
      throw new Error("Studio rejected or failed editor delete apply.");
    }

    if (changedPaths.length > 0) {
      this.clearAppliedEditorPaths(changedPaths);
      await this.updateEditorLiveSyncCacheAfterPush(changedPaths, cfg);
      await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);
    }
    return "applied";
  }

  public async onDocumentSaved(doc: vscode.TextDocument): Promise<void> {
    if (this.experienceChangeInProgress || doc.isUntitled || doc.uri.scheme !== "file") {
      return;
    }

    const cfg = this.getConfig();
    if (!cfg.editorLiveSyncEnabled) {
      this.disposeLiveSyncRuntime();
      this.updateStatusBar();
    }

    if (
      cfg.editorLiveSyncEnabled &&
      this.liveSyncWatcher &&
      this.isProjectSourcePath(doc.uri.fsPath, cfg)
    ) {
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

    const services = this.servicesForProjectSourcePath(doc.uri.fsPath, cfg);
    if (services.length === 0) {
      return;
    }
    for (const service of services) {
      this.pendingAutoServices.add(service);
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

    if (!this.isProjectSourcePath(filePath, cfg)) {
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
      vscode.window.showErrorMessage(`Editor live sync failed. ${message}`);
    }
  }

  private async flushEditorChanges(): Promise<void> {
    const cfg = this.getConfig();
    if (!cfg.editorLiveSyncEnabled) {
      this.persistPendingEditorPaths();
      return;
    }

    const now = Date.now();
    const queuedPaths: string[] = [];
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
    }
    this.persistPendingEditorPaths();
    if (earliestSuppressedUntil !== undefined) {
      this.scheduleEditorLiveSyncFlush(earliestSuppressedUntil - now);
    }
    if (queuedPaths.length === 0) {
      return;
    }
    const classified = await this.classifyEditorLiveSyncPaths(queuedPaths, cfg);
    const changedPaths = classified.pushable;
    for (const filePath of classified.unchanged) {
      const key = this.normalizePathForCompare(filePath);
      this.pendingEditorPaths.delete(filePath);
      this.pendingEditorServicesByPath.delete(key);
      this.forcedEditorLiveSyncPathKeys.delete(key);
    }
    if (changedPaths.length === 0) {
      this.recomputeBlockedStudioImportServices(cfg);
      this.persistPendingEditorPaths();
      return;
    }

    const pushedHashes = new Map(
      await Promise.all(changedPaths.map(async (filePath) => [
        this.normalizePathForCompare(filePath),
        await this.editorLiveSyncFileHash(filePath),
      ] as const)),
    );
    const previousCache = this.loadEditorLiveSyncCache(cfg.projectRoot).cache.files;
    const structuralServices = new Set<string>();
    for (const filePath of changedPaths) {
      const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
      const existedBefore = previousCache[key] !== undefined;
      const existsNow = pushedHashes.get(this.normalizePathForCompare(filePath)) !== undefined;
      if (
        existedBefore === existsNow &&
        !isReniumSettingsFileName(path.basename(filePath))
      ) {
        continue;
      }
      for (const service of this.servicesForProjectSourcePath(filePath, cfg)) {
        structuralServices.add(service);
      }
    }
    let applied = false;
    try {
      await this.enqueue("Editor -> Studio sync", async () => {
        this.logEditorChangedPaths("Editor -> Studio", changedPaths, cfg);
        const outcome = await this.runEditorPush(changedPaths, cfg);
        if (outcome === "applied") {
          this.refreshSyncBasesForPaths(changedPaths, cfg);
          applied = true;
        }
      });
      if (!applied) {
        this.persistPendingEditorPaths();
        return;
      }
      const changedDuringPush: string[] = [];
      for (const filePath of changedPaths) {
        const key = this.normalizePathForCompare(filePath);
        if (pushedHashes.get(key) === await this.editorLiveSyncFileHash(filePath)) {
          this.pendingEditorPaths.delete(filePath);
          this.pendingEditorServicesByPath.delete(key);
        } else {
          changedDuringPush.push(filePath);
        }
      }
      this.recomputeBlockedStudioImportServices(cfg);
      this.invalidateEditorLiveSyncCachePaths(changedDuringPush, cfg);
      this.persistPendingEditorPaths();
      if (structuralServices.size > 0) {
        await vscode.commands.executeCommand(
          "renium.fileExplorer.refreshServices",
          [...structuralServices],
        );
      }
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
      this.persistPendingEditorPaths();
      this.scheduleEditorLiveSyncFlush(retryDelayMs);
      throw err;
    } finally {
      if (applied) {
        for (const filePath of changedPaths) {
          this.forcedEditorLiveSyncPathKeys.delete(this.normalizePathForCompare(filePath));
        }
      }
    }
  }

  private async runEditorPush(
    changedPaths: string[],
    cfg: SyncConfig,
    options: EditorPushOptions = {},
  ): Promise<EditorPushOutcome> {
    const command = cfg.exportCliPath;
    this.ensureFileExists(command);
    const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
    const args = [
      "push",
      "-r",
      cfg.projectRoot,
      "-d",
      cfg.srcDir,
      "-w",
      String(bridgeWaitSeconds),
      "-P",
      cfg.bridgePorts,
    ];
    const verifySources = options.fullSync !== true
      && (options.verifySources === true || cfg.verifyEditorPushSources);
    if (verifySources) {
      args.push("-v");
    }
    if (cfg.linkSync.cacheDir.length > 0) {
      args.push("--link-cache-dir", cfg.linkSync.cacheDir);
    }
    if (this.effectiveLiveSyncConfig(cfg).overridePackages) {
      args.push("--override-packages");
    }
    if (options.fullSync === true) {
      args.push("--no-review", "--yes");
    }
    const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
    let changedPathsFile: string | undefined;
    const changedPathArgs = options.fullSync === true
      ? []
      : changedPaths.map((changedPath) => this.editorChangedPathArg(changedPath, cfg.projectRoot));
    if (!usePersistentBridge && changedPathArgs.length > 32) {
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
    if (!usePersistentBridge && uniqueTargetSettingsIds.length > 128) {
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

    try {
      const parameters = {
        bridgeWaitSeconds,
        bridgePorts: cfg.bridgePorts,
        changedPaths: changedPathArgs,
        targetSettingsIds: uniqueTargetSettingsIds,
        targetProperties: [...new Set(targetProperties)],
        verifySources,
        upsertInstancesOnly: options.upsertInstancesOnly === true,
        overridePackages: this.effectiveLiveSyncConfig(cfg).overridePackages,
        ...(cfg.linkSync.cacheDir.length > 0 ? { linkCacheDir: cfg.linkSync.cacheDir } : {}),
        destructive: options.fullSync === true,
      };
      const result = usePersistentBridge
        ? options.fullSync === true
          ? await this.runReviewedAutomationOperation(
            command,
            cfg,
            "editor-push",
            AUTOMATION_OP.push,
            parameters,
            { quietWait: true },
          )
          : await this.runAutomationOperation(
            command,
            cfg,
            "editor-push",
            AUTOMATION_OP.push,
            parameters,
            { quietWait: true },
          )
        : await this.runCommand(
          command,
          this.withPlaceSelector(cfg, args),
          cfg.projectRoot,
          "editor-push",
          cfg.progressHeartbeatSeconds,
          { quietLog: true },
        );

      if (result.code !== 0) {
        throw new Error(result.automationError?.m ?? `Editor push exited with code ${result.code}`);
      }
      const summary = this.parseEditorPushSummary(result.output, result.result);
      if (!summary) {
        throw new Error("Editor push did not return a Studio apply result.");
      }
      if (summary.skippedByReview === true) {
        this.output.appendLine("[renium] editor push was skipped in Studio and remains pending");
        return "skipped";
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
        throw new Error(`Studio reported a failed editor push.${detail}`);
      }
      if (summary.ok !== false && errors === 0 && sourceVerifyFailed === 0) {
        await this.updateEditorLiveSyncCacheAfterPush(changedPaths, cfg);
        await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);
      }

      const existingSourceSaves = changedPaths.filter((changedPath) => this.isLuaSourcePath(changedPath) && fs.existsSync(changedPath)).length;
      if (verifySources && existingSourceSaves > 0 && sourceVerified < existingSourceSaves) {
        this.output.appendLine(
          `[renium] editor push verification warning: verified ${sourceVerified}/${existingSourceSaves} saved Lua source file(s).`,
        );
      }
      return "applied";
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

  private parseEditorPushSummary(output: string, daemonResult?: unknown): Record<string, unknown> | undefined {
    if (daemonResult && typeof daemonResult === "object" && !Array.isArray(daemonResult)) {
      return daemonResult as Record<string, unknown>;
    }
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
    let found: StudioChangeState | undefined;
    for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
      const line = rawLine.trim();
      const index = line.indexOf(prefix);
      const payload = index >= 0 ? line.slice(index + prefix.length) : undefined;
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
      || Array.isArray(record.editorActions)
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
      runtimeId: typeof record.runtimeId === "string" ? record.runtimeId : undefined,
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
      editorActions: Array.isArray(record.editorActions)
        ? record.editorActions
          .filter((value): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value))
          .map((value) => ({
            id: typeof value.id === "string" ? value.id : undefined,
            type: typeof value.type === "string" ? value.type : undefined,
            service: typeof value.service === "string" ? value.service : undefined,
            settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
            pathSegments: Array.isArray(value.pathSegments) ? value.pathSegments.map(String) : undefined,
            pathOrdinals: Array.isArray(value.pathOrdinals)
              ? value.pathOrdinals.map((entry) => Number(entry))
              : undefined,
          }))
        : undefined,
      changes: Array.isArray(record.changes)
        ? record.changes
          .filter((value): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value))
          .map((value) => ({
            service: typeof value.service === "string" ? value.service : undefined,
            settingsId: typeof value.settingsId === "string" ? value.settingsId : undefined,
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
      explicitRuntimeSettings: record.explicitRuntimeSettings && typeof record.explicitRuntimeSettings === "object" && !Array.isArray(record.explicitRuntimeSettings)
        ? record.explicitRuntimeSettings as Record<string, unknown>
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
    const apply = this.configurationChangeQueue.then(() => this.applyConfigurationChanged(event));
    this.configurationChangeQueue = apply.catch(() => undefined);
    void apply.catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] configuration reload failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not reload Renium configuration. ${message}`);
    });
  }

  private async applyConfigurationChanged(event?: vscode.ConfigurationChangeEvent): Promise<void> {
    const cfg = this.getConfig();
    const editorLiveSyncChanged = event?.affectsConfiguration("renium.editorLiveSyncEnabled") === true;
    const projectRootChanged = event?.affectsConfiguration("renium.projectRoot") === true;
    if (projectRootChanged) {
      const previousRoot = this.configuredProjectRoot;
      if (
        previousRoot !== undefined
        && this.normalizePathForCompare(previousRoot) === this.normalizePathForCompare(cfg.projectRoot)
      ) {
        this.configuredProjectRoot = cfg.projectRoot;
        this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
      } else {
        const previousConfiguration = this.projectRootConfigurationSnapshot;
        const previousState: ExperienceChangeSnapshot = {
          projectRoot: previousRoot,
          pendingEditorPaths: [...this.pendingEditorPaths],
          blockedStudioImportServices: [...this.blockedStudioImportServices],
          pendingEditorServicesByPath: [...this.pendingEditorServicesByPath]
            .map(([filePath, services]) => [filePath, [...services]]),
          studioSnapshotFingerprintByService: [...this.studioSnapshotFingerprintByService],
          pendingLinkPackageSourcePaths: [...this.pendingLinkPackageSourcePaths],
        };
        const resumeLiveSync = !!(
          this.liveSyncWatcher
          || this.liveSyncStartPromise
          || this.editorLiveSyncRuntimeEnabled
        );
        this.experienceChangeInProgress = true;
        let explorerPrepared = false;
        try {
          await this.queue.catch(() => undefined);
          this.experienceGeneration += 1;
          await this.stopConsoleFollow();
          await vscode.commands.executeCommand("renium.fileExplorer.prepareProjectSwitch");
          explorerPrepared = true;
          if (previousRoot) {
            await terminateProcessesForOwner(projectProcessOwner(previousRoot));
          }
          await this.persistPendingEditorPaths(previousRoot);
          if (this.liveSyncProjectRoot !== undefined) {
            await this.disposeLiveSyncRuntime();
          }
          this.pendingEditorPaths.clear();
          this.blockedStudioImportServices.clear();
          this.pendingEditorServicesByPath.clear();
          if (previousRoot) {
            invalidateProjectSourceGraph(previousRoot);
          }
          invalidateProjectSourceGraph(cfg.projectRoot);
          this.restorePendingEditorPaths(cfg.projectRoot);
          this.configuredProjectRoot = cfg.projectRoot;
          this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
          this.sourcemapCache = undefined;
          this.studioRuntimeSettings = undefined;
          this.studioConflictPolicyOverride = undefined;
          this.studioSnapshotFingerprintByService.clear();
          this.conflictMarkerWarnedKeys.clear();
          this.recentDirectSaveAtByPath.clear();
          this.editorPushFailureStreak = 0;
          this.linkStatusCache = undefined;
          this.linkStatusInflight = undefined;
          await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
          await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
          explorerPrepared = false;
          this.ensureAgentInstructions(cfg.experienceRoot);
          let pendingPackageSource = false;
          for (const pending of this.pendingLinkPackageSourcePaths.values()) {
            if (
              this.normalizePathForCompare(pending.projectRoot)
              === this.normalizePathForCompare(cfg.projectRoot)
            ) {
              pending.generation = this.experienceGeneration;
              pendingPackageSource = true;
            }
          }
          if (pendingPackageSource) {
            this.scheduleLinkPackageSourceFlush(
              cfg.projectRoot,
              this.experienceGeneration,
              1000,
            );
          }
        } catch (error) {
          await this.restoreProjectRootConfiguration(previousConfiguration);
          this.configuredProjectRoot = previousRoot;
          this.pendingEditorPaths = new Set(previousState.pendingEditorPaths);
          this.blockedStudioImportServices = new Set(previousState.blockedStudioImportServices);
          this.pendingEditorServicesByPath = new Map(
            previousState.pendingEditorServicesByPath
              .map(([filePath, services]) => [filePath, new Set(services)]),
          );
          this.studioSnapshotFingerprintByService = new Map(previousState.studioSnapshotFingerprintByService);
          this.pendingLinkPackageSourcePaths.clear();
          for (const [filePath, pending] of previousState.pendingLinkPackageSourcePaths) {
            this.pendingLinkPackageSourcePaths.set(filePath, {
              projectRoot: previousRoot ?? pending.projectRoot,
              generation: this.experienceGeneration,
            });
          }
          this.liveSyncProjectRoot = previousRoot;
          this.sourcemapCache = undefined;
          this.studioRuntimeSettings = undefined;
          this.studioConflictPolicyOverride = undefined;
          this.linkStatusCache = undefined;
          this.linkStatusInflight = undefined;
          if (previousRoot) {
            invalidateProjectSourceGraph(previousRoot);
          }
          if (explorerPrepared) {
            await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
            explorerPrepared = false;
          } else {
            await vscode.commands.executeCommand("renium.fileExplorer.cancelProjectSwitch");
          }
          await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
          if (this.bridgeServeRequested && !this.isBridgeDaemonRunning()) {
            await this.serve({ silent: true, bestEffort: true });
          }
          if (resumeLiveSync) {
            await this.startLiveSync({ silent: true, bestEffort: true });
          }
          if (this.pendingLinkPackageSourcePaths.size > 0 && previousRoot) {
            this.scheduleLinkPackageSourceFlush(previousRoot, this.experienceGeneration);
          }
          throw error;
        } finally {
          if (explorerPrepared) {
            await vscode.commands.executeCommand("renium.fileExplorer.cancelProjectSwitch");
          }
          this.experienceChangeInProgress = false;
        }
      }
    }
    const bridgeConfigChanged = !event || [
      "renium.exportCliPath",
      "renium.projectRoot",
      "renium.transport",
      "renium.bridgeWaitSeconds",
      "renium.bridgePorts",
    ].some((key) => event.affectsConfiguration(key));
    const persistentBridgeChanged = event?.affectsConfiguration("renium.usePersistentBridge") === true;

    if (bridgeConfigChanged) {
      await this.stopConsoleFollow();
    }
    if (bridgeConfigChanged || (persistentBridgeChanged && !this.bridgeServeRequested)) {
      await this.stopBridgeDaemon();
      if (this.bridgeServeRequested) {
        void this.serve({ silent: true, bestEffort: true });
      } else if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && this.shouldUsePersistentBridge(cfg)) {
        void this.prewarmPersistentBridgeDaemon("configuration");
      }
    }
    if (!cfg.editorLiveSyncEnabled && this.liveSyncWatcher) {
      await this.disposeLiveSyncRuntime();
      if (!this.bridgeServeRequested) {
        await this.stopBridgeDaemon();
      }
    }
    if (
      (editorLiveSyncChanged || projectRootChanged) &&
      cfg.editorLiveSyncEnabled &&
      !this.liveSyncWatcher &&
      !this.liveSyncStartPromise
    ) {
      void this.startLiveSync({ silent: true, bestEffort: true });
    }
    if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && !this.liveSyncStartupInProgress) {
      if (cfg.studioLiveSyncEnabled) {
        void this.startStudioLiveSyncRuntime(cfg, { bestEffort: true });
      } else {
        await this.stopStudioLiveSyncRuntime();
      }
    }
    if (!event || event.affectsConfiguration("renium.gitSync") || event.affectsConfiguration("renium.projectRoot")) {
      void this.refreshGitView();
    }
    if (
      !event
      || event.affectsConfiguration("renium.link")
      || event.affectsConfiguration("renium.projectRoot")
    ) {
      this.invalidateLinkStatusCache();
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
    vscode.window.showInformationMessage(`Auto sync on save ${!enabled ? "enabled" : "disabled"}.`);
  }

  private async enqueue(taskName: string, task: () => Promise<void>): Promise<void> {
    if (this.experienceChangeInProgress) {
      void vscode.window.showWarningMessage("Wait for the place change to finish.");
      return;
    }
    const generation = this.experienceGeneration;
    const run = async (): Promise<void> => {
      if (generation !== this.experienceGeneration || this.experienceChangeInProgress) {
        return;
      }
      try {
        this.setActiveTask(taskName);
        await task();
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] task failed: ${taskName}: ${message}`);
        this.output.show(true);
        vscode.window.showErrorMessage(`${taskName} failed. ${message}`);
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
    destructive?: boolean;
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
    const directArgs = this.withPlaceSelector(cfg, args);

    const quietLog = options.quietLog === true;
    if (!quietLog) {
      this.output.show(false);
      this.logResolvedConfig(cfg);
      if (usePersistentBridge) {
        this.output.appendLine(
          `[renium] export daemon command: ${command} bd -w ${Math.max(1, cfg.bridgeWaitSeconds)} -P ${cfg.bridgePorts}`,
        );
        this.output.appendLine(`[renium] automation operation: ${options.runImport ? "pull" : "export-snapshots"}`);
      } else {
        this.output.appendLine(`[renium] export command: ${command} ${this.renderArgs(directArgs)}`);
      }
    }

    const operation = options.runImport ? AUTOMATION_OP.pull : AUTOMATION_OP.exportSnapshots;
    const parameters = {
      services: selectedServices,
      snapshotDir: cfg.snapshotDir,
      bridgeWaitSeconds: this.editorBridgeWaitSeconds(cfg),
      bridgePorts: cfg.bridgePorts,
      performanceMode: cfg.performanceMode,
      modifiedDefaultBypass: cfg.modifiedDefaultBypass,
      chunkSize: Math.max(512, cfg.chunkSize),
      sourceWorkers: Math.max(0, cfg.sourceWorkers),
      instanceWorkers: Math.max(0, cfg.instanceWorkers),
      importWorkers: Math.max(0, cfg.importWorkers),
      adaptiveThrottle: cfg.adaptiveThrottle,
      noUpdateEditorIcons: cfg.noUpdateEditorIcons,
      destructive: options.destructive === true,
    };
    const result = usePersistentBridge
      ? options.destructive === true
        ? await this.runReviewedAutomationOperation(
          command,
          cfg,
          "pull",
          operation,
          parameters,
          { quietWait: quietLog },
        )
        : await this.runAutomationOperation(
          command,
          cfg,
          "export",
          operation,
          parameters,
          { quietWait: quietLog },
        )
      : await this.runCommand(
        command,
        directArgs,
        cfg.projectRoot,
        "export",
        cfg.progressHeartbeatSeconds,
        { quietLog },
      );
    if (result.code !== 0) {
      throw new Error(result.automationError?.m ?? `Export exited with code ${result.code}`);
    }

    if (options.runImport && options.notifyOnSuccess) {
      try {
        await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", selectedServices);
      } catch {
      }
    }

    if (options.notifyOnSuccess && options.reason) {
      vscode.window.showInformationMessage(`${options.reason}.`);
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

  private async runAutomationOperation(
    command: string,
    cfg: SyncConfig,
    label: string,
    op: number,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    await this.ensureBridgeDaemon(command, cfg, { serve: this.bridgeServeRequested });
    const contextId = await this.ensureDaemonContext(command, cfg);
    return await this.sendAutomationRequest(cfg, label, op, contextId, parameters, options);
  }

  private async runReviewedAutomationOperation(
    command: string,
    cfg: SyncConfig,
    label: string,
    op: number,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    await this.ensureBridgeDaemon(command, cfg, { serve: this.bridgeServeRequested });
    const contextId = await this.ensureDaemonContext(command, cfg);
    const prepared = await this.sendAutomationRequest(
      cfg,
      `${label}-review`,
      AUTOMATION_OP.reviewPrepare,
      contextId,
      { op, p: parameters },
      { ...options, quietWait: true },
    );
    if (prepared.code !== 0) {
      return prepared;
    }
    const result = prepared.result as Record<string, unknown> | undefined;
    const reviewId = typeof result?.reviewId === "string" ? result.reviewId : undefined;
    if (!reviewId) {
      return { code: 1, output: "Review preparation did not return reviewId." };
    }
    return await this.sendAutomationRequest(
      cfg,
      label,
      AUTOMATION_OP.reviewApply,
      contextId,
      { reviewId },
      options,
    );
  }

  private async ensureDaemonContext(command: string, cfg: SyncConfig): Promise<number> {
    const key = JSON.stringify({
      projectRoot: path.resolve(cfg.projectRoot),
      place: cfg.placeSelector ?? "",
      daemon: this.daemonKeyValue ?? "",
    });
    if (this.daemonContext?.key === key) {
      return this.daemonContext.id;
    }
    const bound = await this.sendAutomationRequest(
      cfg,
      "bind",
      AUTOMATION_OP.bind,
      undefined,
      { root: cfg.projectRoot, place: cfg.placeSelector },
      { quietWait: true, timeoutMs: 2_000 },
    );
    if (bound.code !== 0) {
      throw new Error(bound.automationError?.m ?? "Renium could not bind this project.");
    }
    const result = bound.result as Record<string, unknown> | undefined;
    const id = Number(result?.id);
    if (!Number.isSafeInteger(id) || id < 1) {
      throw new Error("Renium bind response omitted the context ID.");
    }
    if (typeof result?.runtimeId === "string" && result.runtimeId.length > 0) {
      this.daemonContext = { key, id };
    }
    void command;
    return id;
  }

  private async sendAutomationRequest(
    cfg: SyncConfig,
    label: string,
    op: number,
    contextId: number | undefined,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {

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

      const timeoutMs = this.daemonRequestTimeoutMs(cfg, op, options.timeoutMs);
      pending.timeoutTimer = setTimeout(() => {
        if (!this.daemonPending.has(id)) {
          return;
        }
        const timeoutMessage = `[renium] ${label}: daemon request timed out after ${Math.round(timeoutMs / 1000)}s; restarting the bridge daemon.\n`;
        this.output.appendLine(timeoutMessage.trim());
        this.finishDaemonRequest(id, { code: 124, output: pending.output + `\n${timeoutMessage}` });
        void this.stopBridgeDaemon(new Error(`Persistent bridge daemon request timed out (${label}).`));
      }, timeoutMs);

      this.daemonPending.set(id, pending);
      const request = JSON.stringify({
        v: 1,
        id,
        op,
        ...(contextId === undefined ? {} : { cx: contextId }),
        p: parameters,
      }) + "\n";

      const writeFailed = (): void => {
        if (this.daemonPending.has(id)) {
          this.finishDaemonRequest(id, {
            code: 1,
            output: pending.output + "\nThe daemon transport closed before the request was written.",
          });
        }
        void this.stopBridgeDaemon(new Error("Persistent bridge daemon transport closed."));
      };
      try {
        proc.stdin.write(request, "utf8", (error) => {
          if (error) {
            writeFailed();
          }
        });
      } catch {
        writeFailed();
      }
    });
  }

  private shouldUsePersistentBridge(cfg: SyncConfig): boolean {
    return cfg.transport === "ws";
  }

  private shouldUsePersistentBridgeForEditorPush(cfg: SyncConfig): boolean {
    return cfg.transport === "ws";
  }

  private editorBridgeWaitSeconds(cfg: SyncConfig): number {
    return Math.max(1, Math.min(2, Number(cfg.bridgeWaitSeconds) || 2));
  }

  private daemonRequestTimeoutMs(cfg: SyncConfig, op: number, requestedTimeoutMs?: number): number {
    if (op === AUTOMATION_OP.liveStatus) {
      const bridgeWaitMs = (Math.max(1, Number(cfg.bridgeWaitSeconds) || 1) + 3) * 1000;
      return Math.max(5_000, Math.min(DAEMON_CHANNEL_WAIT_MAX_MS, bridgeWaitMs));
    }
    return Math.max(
      1_000,
      Math.min(MAX_COMMAND_TIMEOUT_MS, Math.floor(Number(requestedTimeoutMs) || DEFAULT_COMMAND_TIMEOUT_MS)),
    );
  }

  private isBridgeDaemonRunning(): boolean {
    return !!this.daemonProcess
      && !this.daemonProcess.killed
      && this.daemonProcess.exitCode === null
      && this.daemonProcess.signalCode === null;
  }

  private async ensureBridgeDaemon(command: string, cfg: SyncConfig, options: { serve?: boolean } = {}): Promise<void> {
    this.ensureFileExists(command);
    const key = this.daemonKey(command, cfg, options.serve === true);
    if (this.isBridgeDaemonRunning() && this.daemonKeyValue === key) {
      await this.awaitBridgeDaemonReady(cfg);
      return;
    }

    await this.stopBridgeDaemon();

    const args = [
      "bd",
      "-w",
      String(Math.max(1, cfg.bridgeWaitSeconds)),
      "-P",
      cfg.bridgePorts,
      "--parent-pid",
      String(process.pid),
      "--editor-stdio",
    ];

    const child = childProcess.spawn(command, args, {
      cwd: cfg.projectRoot,
      env: process.env,
      detached: process.platform !== "win32",
      shell: false,
      stdio: "pipe",
      windowsHide: true,
    });
    this.daemonClosePromise = trackProcess(child, projectProcessOwner(cfg.projectRoot));

    this.daemonProcess = child;
    this.daemonKeyValue = key;
    this.daemonOutputBuffer = "";
    this.daemonReady = false;
    this.daemonReadyPromise = new Promise<void>((resolve, reject) => {
      this.daemonReadyResolve = resolve;
      this.daemonReadyReject = reject;
    });
    child.once("spawn", () => {
      this.daemonReady = true;
      this.daemonReadyResolve?.();
      this.daemonReadyResolve = undefined;
      this.daemonReadyReject = undefined;
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
    });
    child.on("close", () => this.clearBridgeDaemonProcess(child));

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
      await this.stopBridgeDaemon(error);
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
    if (isStderr && !hasQuietPending) {
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
      void this.stopBridgeDaemon(error);
      return;
    }
    let newlineIndex = this.daemonOutputBuffer.indexOf("\n");
    while (newlineIndex >= 0) {
      const rawLine = this.daemonOutputBuffer.slice(0, newlineIndex + 1);
      const line = this.daemonOutputBuffer.slice(0, newlineIndex).replace(/\r$/, "");
      this.daemonOutputBuffer = this.daemonOutputBuffer.slice(newlineIndex + 1);
      this.processDaemonLine(line);
      void rawLine;
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
    let payload: AutomationResponse;
    try {
      payload = JSON.parse(line) as AutomationResponse;
    } catch (err) {
      this.output.appendLine(
        `[renium] bridge daemon returned invalid protocol JSON: ${err instanceof Error ? err.message : String(err)}`,
      );
      return;
    }
    if (payload.v !== 1 || (payload.ok !== 0 && payload.ok !== 1)) {
      this.output.appendLine("[renium] bridge daemon returned an incompatible protocol response.");
      return;
    }
    const id = Number(payload.id ?? 0);
    const code = payload.ok === 1 ? 0 : 1;
    const pending = this.daemonPending.get(id);
    if (!pending) {
      return;
    }
    if (payload.e?.c === "stale_cx") {
      this.daemonContext = undefined;
    }

    let output = pending.output;
    if (payload.e?.m) {
      output += `\n${payload.e.m}\n`;
    }
    this.finishDaemonRequest(id, {
      code,
      output,
      result: payload.r,
      automationError: payload.e,
    });
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
      proc.stdin.end();
    } catch {
    }
  }

  private clearBridgeDaemonProcess(proc: childProcess.ChildProcess): void {
    if (this.daemonProcess !== proc) {
      return;
    }
    this.daemonProcess = undefined;
    this.daemonKeyValue = undefined;
    this.daemonOutputBuffer = "";
    this.daemonReady = false;
    this.daemonReadyPromise = undefined;
    this.daemonReadyResolve = undefined;
    this.daemonReadyReject = undefined;
    this.daemonClosePromise = undefined;
    this.daemonContext = undefined;
    this.studioEditorActionRuns.clear();
  }

  private stopBridgeDaemon(reason = new Error("Persistent bridge daemon was stopped.")): Promise<void> {
    if (this.daemonStopPromise) {
      return this.daemonStopPromise;
    }
    const stop = this.stopBridgeDaemonProcess(reason);
    const tracked = stop.finally(() => {
      if (this.daemonStopPromise === tracked) {
        this.daemonStopPromise = undefined;
      }
    });
    this.daemonStopPromise = tracked;
    return tracked;
  }

  private async stopBridgeDaemonProcess(reason: Error): Promise<void> {
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
    this.daemonReadyReject?.(reason);
    this.rejectDaemonPending(reason);
    const closed = this.daemonClosePromise ?? trackProcess(
      proc,
      projectProcessOwner(this.configuredProjectRoot ?? this.context.extensionPath),
    );
    let closedGracefully = false;
    await Promise.race([
      closed.then(() => {
        closedGracefully = true;
      }),
      sleep(500),
    ]);
    if (!closedGracefully) {
      await terminateProcess(proc);
    }
    this.clearBridgeDaemonProcess(proc);
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
      const importCliPath = resolveExistingRustCliPath(
        this.getWorkspaceRoot(),
        cfg.projectRoot,
        cfg.rustCliPath,
        this.context.extensionPath,
      );
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
        "--src-dir",
        cfg.srcDir,
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
    const candidates = reniumCliCandidates({
      configuredPath: cfg.rustCliPath,
      extensionRoot: this.context.extensionPath,
      roots,
      fallbackRelativePaths: RUST_CLI_FALLBACK_RELATIVE_PATHS,
    });
    void command;
    return candidates.find((candidate) => fs.existsSync(candidate)) ?? cfg.rustCliPath;
  }

  public async detectedCliVersion(cliPath: string): Promise<string | undefined> {
    const result = await this.runCommand(
      cliPath,
      ["--version"],
      path.dirname(cliPath),
      "CLI version check",
      10,
      { quietLog: true, timeoutMs: 5_000 },
    );
    if (result.code !== 0) {
      return undefined;
    }
    return result.output.match(/(\d+\.\d+\.\d+)/)?.[1];
  }

  private withPlaceSelector(cfg: SyncConfig, args: string[]): string[] {
    return cfg.placeSelector
      ? ["--place", cfg.placeSelector, ...args]
      : args;
  }

  private async runRustImport(
    cfg: SyncConfig,
    snapshotPath: string,
    services: string[],
    options: { quietLog?: boolean } = {},
  ): Promise<string[]> {
    const rustCliPath = resolveExistingRustCliPath(
      this.getWorkspaceRoot(),
      cfg.projectRoot,
      cfg.rustCliPath,
      this.context.extensionPath,
    );
    this.ensureFileExists(rustCliPath);
    const selectedServices = this.normalizeServices(services, cfg.services);
    const args = [
      "--output-mode",
      "json",
      "import-snapshots",
      "--snapshot-dir",
      snapshotPath,
      "--project-root",
      cfg.projectRoot,
      "--src-dir",
      cfg.srcDir,
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
    const parsed = this.parseCliJsonObject<{ changedPaths?: unknown }>(result.output);
    if (!parsed || !Array.isArray(parsed.changedPaths)) {
      throw new Error("Rust import returned invalid structured output.");
    }
    return parsed.changedPaths.map((filePath) => {
      if (typeof filePath !== "string") {
        throw new Error("Rust import returned a non-string changed path.");
      }
      return path.resolve(cfg.projectRoot, filePath);
    });
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
      let timedOut = false;
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
          detached: process.platform !== "win32",
          shell: false,
          stdio: "pipe",
          windowsHide: true,
        });
        trackProcess(child, projectProcessOwner(cwd));
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
        timedOut = true;
        appendOutput(`\n[renium] ${label}: timed out after ${Math.round(timeoutMs / 1000)}s; terminating the process.\n`);
        void terminateProcess(child).finally(() => finish(124));
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
        finish(timedOut ? 124 : code ?? 130);
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


  public rbsyncViewerCli(): { cliPath: string; cwd: string } | undefined {
    const cfg = this.getConfig();
    if (!cfg.exportCliPath) {
      return undefined;
    }
    return { cliPath: cfg.exportCliPath, cwd: cfg.projectRoot };
  }

  private explicitConfigValue<T>(cfg: vscode.WorkspaceConfiguration, key: string): T | undefined {
    const inspected = cfg.inspect<T>(key);
    return inspected?.workspaceFolderValue ?? inspected?.workspaceValue ?? inspected?.globalValue;
  }

  private mergedConfigValue<T>(
    cfg: vscode.WorkspaceConfiguration,
    shared: SharedConfig,
    key: string,
    defaultValue: T,
  ): T {
    return this.explicitConfigValue<T>(cfg, key) ?? sharedConfigValue<T>(shared, key) ?? defaultValue;
  }

  private getConfig(): SyncConfig {
    const root = this.getWorkspaceRoot();
    const cfg = vscode.workspace.getConfiguration("renium", vscode.Uri.file(root));

    const preliminaryShared = loadSharedConfig(root, root);
    const projectRootSetting = this.mergedConfigValue(
      cfg,
      preliminaryShared,
      "projectRoot",
      "${workspaceFolder}",
    );
    const experienceRoot = this.resolveConfigPath(projectRootSetting, root);
    if (!activeExperienceAlias(experienceRoot)) {
      this.restoreActiveExperiencePlace(experienceRoot);
    }
    const activePlace = resolveActiveExperiencePlace(experienceRoot);
    if (activePlace && activeExperienceAlias(experienceRoot) !== activePlace.alias) {
      setActiveExperiencePlace(experienceRoot, activePlace.alias);
    }
    const projectRoot = activePlace?.projectRoot ?? experienceRoot;
    const shared = loadSharedConfig(root, projectRoot);
    const srcDir = loadProjectSourceRoot(projectRoot);
    this.sharedConfig = shared;
    const read = <T>(key: string, defaultValue: T): T =>
      this.mergedConfigValue(cfg, shared, key, defaultValue);
    const configTomlPath = this.resolveConfigPath(
      read("configTomlPath", "${userHome}/.codex/config.toml"),
      root,
    );
    const configuredExportCliPathRaw = read("exportCliPath", "").trim();
    const configuredExportCliPath = configuredExportCliPathRaw.length > 0
      ? this.resolveConfigPath(configuredExportCliPathRaw, root)
      : "";
    const exportCliPath = resolveExistingRustCliPath(
      root,
      projectRoot,
      configuredExportCliPath,
      this.context.extensionPath,
    );

    const servicesRaw = read<string[]>("services", DEFAULT_SERVICES);
    const services = (Array.isArray(servicesRaw) ? servicesRaw : DEFAULT_SERVICES)
      .map((s) => String(s).trim())
      .filter((s) => s.length > 0);

    const transportRaw = read<string>("transport", "ws");
    const transport = transportRaw === "mcp" ? "mcp" : "ws";

    const importModeRaw = read<string>("importMode", "direct");
    const importMode = importModeRaw === "snapshot" ? "snapshot" : "direct";
    const performanceModeRaw = read<string>("performanceMode", "throughput");
    const performanceMode =
      performanceModeRaw === "smooth"
        ? "smooth"
        : performanceModeRaw === "balanced"
          ? "balanced"
          : "throughput";
    const modifiedDefaultBypass = read<boolean>("modifiedDefaultBypass", false) === true;
    const wsWaitSeconds = this.getWsWaitSeconds(cfg, shared);
    const chunkSize = this.normalizeChunkSize(read("chunkSize", DEFAULT_CHUNK_SIZE));
    const configuredRustCliPathRaw = read("rustCliPath", "").trim();
    const configuredRustCliPath = configuredRustCliPathRaw.length > 0
      ? this.resolveConfigPath(configuredRustCliPathRaw, root)
      : "";
    const rustCliPath = resolveExistingRustCliPath(
      root,
      projectRoot,
      configuredRustCliPath,
      this.context.extensionPath,
    );

    const gitStagePathsRaw = read<string[]>("gitSync.stagePaths", []);
    const gitStagePaths = (Array.isArray(gitStagePathsRaw) ? gitStagePathsRaw : [])
      .map((value) => String(value).trim())
      .filter((value) => value.length > 0);
    const gitPullFromStudioBeforePushRaw = read<string>("gitSync.pullFromStudioBeforePush", "ask");
    const gitPullFromStudioBeforePush = gitPullFromStudioBeforePushRaw === "always"
      ? "always"
      : gitPullFromStudioBeforePushRaw === "never"
        ? "never"
        : "ask";
    const gitStageModeRaw = read<string>("gitSync.stageMode", "tracked");
    const gitStageMode = gitStageModeRaw === "configuredPaths" ? "configuredPaths" : "tracked";
    const gitApplyPulledChangesRaw = read<string>("gitSync.applyPulledChangesToStudio", "ask");
    const gitApplyPulledChangesToStudio = gitApplyPulledChangesRaw === "always"
      ? "always"
      : gitApplyPulledChangesRaw === "never"
        ? "never"
        : "ask";
    const gitOutputBehaviorRaw = read<string>("gitSync.outputBehavior", "onStart");
    const gitOutputBehavior = gitOutputBehaviorRaw === "silent"
      ? "silent"
      : gitOutputBehaviorRaw === "onError"
        ? "onError"
        : "onStart";
    const wallyApplyToStudioRaw = read<string>("wallySync.applyToStudio", "ask");
    const wallyApplyToStudio = wallyApplyToStudioRaw === "always"
      ? "always"
      : wallyApplyToStudioRaw === "never"
        ? "never"
        : "ask";
    const linkApplyToStudioRaw = read<string>("link.applyToStudio", "ask");
    const linkApplyToStudio = linkApplyToStudioRaw === "always"
      ? "always"
      : linkApplyToStudioRaw === "never"
        ? "never"
        : "ask";
    const initialSyncPriorityRaw = read<string>("liveSync.initialSyncPriority", "studio");
    const initialSyncPriority: InitialSyncPriority = initialSyncPriorityRaw === "editor"
      ? "editor"
      : initialSyncPriorityRaw === "none"
        ? "none"
        : "studio";
    const displayPromptsRaw = read<string>("liveSync.displayPrompts", "always");
    const displayPrompts: DisplayPrompts = displayPromptsRaw === "initial"
      ? "initial"
      : displayPromptsRaw === "never"
        ? "never"
        : "always";
    const logLevel = this.configuredLogLevel();
    const number = (
      key: string,
      defaultValue: number,
      options: { min?: number; integer?: boolean } = {},
    ): number => this.normalizeConfigNumber(read<unknown>(key, defaultValue), defaultValue, options);

    return {
      exportCliPath,
      rustCliPath,
      experienceRoot,
      projectRoot,
      srcDir,
      activePlaceAlias: activePlace?.alias,
      activePlace: activePlace?.place,
      placeSelector: activePlace?.selector,
      snapshotDir: read("snapshotDir", "snapshots"),
      transport,
      server: read("server", "Roblox_Studio"),
      configTomlPath,
      services: services.length > 0 ? services : [...DEFAULT_SERVICES],
      sourceWorkers: number("sourceWorkers", 0, { min: 0, integer: true }),
      instanceWorkers: number("instanceWorkers", 0, { min: 0, integer: true }),
      importWorkers: number("importWorkers", 0, { min: 0, integer: true }),
      chunkSize,
      snapshotInstanceChunkSize: number("snapshotInstanceChunkSize", 5000, { min: 0, integer: true }),
      bridgeWaitSeconds: number("bridgeWaitSeconds", 8, { min: 1 }),
      bridgePorts: this.normalizeBridgePorts(
        String(
          read("bridgePorts", DEFAULT_BRIDGE_PORTS.join(",")),
        ),
      ),
      usePersistentBridge: read<boolean>("usePersistentBridge", true) !== false,
      verifyEditorPushSources: read<boolean>("verifyEditorPushSources", false) === true,
      adaptiveThrottle: read<boolean>("adaptiveThrottle", true),
      noUpdateEditorIcons: read<boolean>("noUpdateEditorIcons", true),
      autoSyncOnSave: read<boolean>("autoSyncOnSave", false),
      autoSyncDebounceMs: number("autoSyncDebounceMs", 800, { min: 100, integer: true }),
      editorLiveSyncEnabled: read<boolean>("editorLiveSyncEnabled", false) === true,
      editorLiveSyncOnStartup: read<boolean>("editorLiveSyncOnStartup", false) === true,
      studioLiveSyncEnabled: read<boolean>("studioLiveSyncEnabled", true) !== false,
      studioLiveSyncPollMs: number(
        "studioLiveSyncPollMs",
        DEFAULT_STUDIO_LIVE_SYNC_POLL_MS,
        { min: MIN_STUDIO_LIVE_SYNC_POLL_MS, integer: true },
      ),
      initialSyncPriority,
      changesThreshold: number("liveSync.changesThreshold", 5, { min: 0, integer: true }),
      diffLinesLimit: number("liveSync.diffLinesLimit", 3000, { min: 100, integer: true }),
      displayPrompts,
      logLevel,
      overridePackages: read<boolean>("liveSync.overridePackages", false) === true,
      conflictResolution: this.normalizeConflictPolicy(read("liveSync.conflictResolution", "prompt")),
      runImport: read<boolean>("runImport", true),
      importMode,
      performanceMode,
      modifiedDefaultBypass,
      wsWaitSeconds,
      progressHeartbeatSeconds: number("progressHeartbeatSeconds", 2, { min: 2 }),
      benchmarkRuns: number("benchmarkRuns", 5, { min: 1, integer: true }),
      gitSync: {
        gitPath: read("gitSync.gitPath", "git"),
        remote: read("gitSync.remote", "origin"),
        branch: read("gitSync.branch", ""),
        autoFetch: read<boolean>("gitSync.autoFetch", true) !== false,
        pullFromStudioBeforePush: gitPullFromStudioBeforePush,
        stageMode: gitStageMode,
        stagePaths: gitStagePaths.length > 0 ? gitStagePaths : [srcDir],
        includeUntracked: read<boolean>("gitSync.includeUntracked", false) === true,
        commitMessageTemplate: read("gitSync.commitMessageTemplate", "Renium sync: ${date}"),
        confirmBeforePush: read<boolean>("gitSync.confirmBeforePush", true) !== false,
        requireCleanWorktreeBeforePull: read<boolean>("gitSync.requireCleanWorktreeBeforePull", true) !== false,
        applyPulledChangesToStudio: gitApplyPulledChangesToStudio,
        timeoutSeconds: number("gitSync.timeoutSeconds", 120, { min: 10 }),
        outputBehavior: gitOutputBehavior,
      },
      wallySync: {
        wallyPath: read("wallySync.wallyPath", "wally"),
        rojoPath: read("wallySync.rojoPath", "rojo"),
        packagesDir: read("wallySync.packagesDir", "Packages"),
        targetService: read("wallySync.targetService", "ReplicatedStorage"),
        targetName: read("wallySync.targetName", "Packages"),
        serverPackagesDir: read("wallySync.serverPackagesDir", "ServerPackages"),
        serverTargetService: read("wallySync.serverTargetService", "ServerStorage"),
        serverTargetName: read("wallySync.serverTargetName", "ServerPackages"),
        devPackagesDir: read("wallySync.devPackagesDir", "DevPackages"),
        devTargetService: read("wallySync.devTargetService", "ReplicatedStorage"),
        devTargetName: read("wallySync.devTargetName", "DevPackages"),
        realms: read("wallySync.realms", "shared,server,dev"),
        runInstall: read<boolean>("wallySync.runInstall", true) !== false,
        applyToStudio: wallyApplyToStudio,
      },
      linkSync: {
        manifest: read("link.manifest", "renium-link.json"),
        folder: read("link.folder", "").trim(),
        cacheDir: read("link.cacheDir", "").trim(),
        gitPath: read("link.gitPath", "git"),
        wallyPath: read("wallySync.wallyPath", "wally"),
        offline: read<boolean>("link.offline", false) === true,
        autoApply: read<boolean>("link.autoApplyOnManifestChange", false) === true,
        applyToStudio: linkApplyToStudio,
      },
    };
  }

  private normalizeChunkSize(value: unknown): number {
    const rawValue = Number(value ?? DEFAULT_CHUNK_SIZE);

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
    if (sharedConfigValue(this.sharedConfig, key) !== undefined) {
      return "shared";
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
      sharedConfigValue(this.sharedConfig, key) ??
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
      .map((token) => Number(token.trim()))
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

    if (normalized.length !== DEFAULT_BRIDGE_PORTS.length) {
      if (!this.warnedBridgePortLimit) {
        this.warnedBridgePortLimit = true;
      this.output.appendLine(
          `[renium] config: exactly ${DEFAULT_BRIDGE_PORTS.length} bridge ports are required; using ${DEFAULT_BRIDGE_PORTS.join(",")}.`,
        );
      }
      normalized = [...DEFAULT_BRIDGE_PORTS];
    }

    return normalized.join(",");
  }

  private getWsWaitSeconds(cfg: vscode.WorkspaceConfiguration, shared: SharedConfig): number {
    const configuredWsWaitSeconds =
      this.explicitConfigValue<unknown>(cfg, "wsWaitSeconds") ??
      sharedConfigValue<unknown>(shared, "wsWaitSeconds");
    if (configuredWsWaitSeconds !== undefined) {
      return this.normalizeConfigNumber(configuredWsWaitSeconds, 20, { min: 1 });
    }

    const legacyStartupWaitSeconds =
      this.explicitConfigValue<unknown>(cfg, "startupWaitSeconds") ??
      sharedConfigValue<unknown>(shared, "startupWaitSeconds");
    if (legacyStartupWaitSeconds !== undefined) {
      if (!this.warnedLegacyStartupWaitSeconds) {
        this.warnedLegacyStartupWaitSeconds = true;
        this.output.appendLine(
          "[renium] config: using legacy renium.startupWaitSeconds as renium.wsWaitSeconds; update your settings to renium.wsWaitSeconds.",
        );
      }
      return this.normalizeConfigNumber(legacyStartupWaitSeconds, 20, { min: 1 });
    }

    return 20;
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
        `[renium] multi-root workspace: using ${root}. Open a file in another workspace folder or set renium.projectRoot to switch projects.`,
      );
    }
    return root;
  }

  private isPathInside(filePath: string, rootPath: string): boolean {
    const relative = path.relative(this.normalizePathForCompare(rootPath), this.normalizePathForCompare(filePath));
    return relative === "" || (!!relative && !relative.startsWith("..") && !path.isAbsolute(relative));
  }

  private sourceRoot(cfg: SyncConfig): string {
    return path.join(cfg.projectRoot, cfg.srcDir);
  }

  private isProjectSourcePath(
    filePath: string,
    cfg: SyncConfig,
    sourceGraph = loadProjectSourceGraph(cfg.projectRoot),
  ): boolean {
    if (sourceGraph.ignored.some((ignored) => this.isPathInside(filePath, ignored))) {
      return false;
    }
    return sourceGraph.files.some((location) =>
      this.normalizePathForCompare(filePath) === this.normalizePathForCompare(location))
      || sourceGraph.directories.some((location) => this.isPathInside(filePath, location));
  }

  private servicesForProjectSourcePath(
    filePath: string,
    cfg: SyncConfig,
    sourceGraph = loadProjectSourceGraph(cfg.projectRoot),
  ): string[] {
    if (!this.isProjectSourcePath(filePath, cfg, sourceGraph)) {
      return [];
    }
    const matches = sourceGraph.owners
      .filter((owner) => {
        let isFile = false;
        try {
          isFile = fs.statSync(owner.location).isFile();
        } catch {
          isFile = path.extname(owner.location) !== "";
        }
        return isFile
          ? this.normalizePathForCompare(filePath) === this.normalizePathForCompare(owner.location)
          : this.isPathInside(filePath, owner.location);
      })
      .sort((left, right) =>
        path.resolve(right.location).split(path.sep).length
        - path.resolve(left.location).split(path.sep).length);
    if (matches.length === 0) {
      return [];
    }
    const specificity = path.resolve(matches[0].location).split(path.sep).length;
    const byLower = new Map(cfg.services.map((service) => [service.toLowerCase(), service]));
    const services = new Set<string>();
    let ambiguous = false;
    for (const owner of matches) {
      if (path.resolve(owner.location).split(path.sep).length !== specificity) {
        break;
      }
      const fixed = owner.target[0];
      if (fixed) {
        const service = byLower.get(fixed.toLowerCase());
        if (service) {
          services.add(service);
        }
        continue;
      }
      let isFile = false;
      try {
        isFile = fs.statSync(owner.location).isFile();
      } catch {
        isFile = path.extname(owner.location) !== "";
      }
      if (isFile) {
        ambiguous = true;
        continue;
      }
      const relative = path.relative(owner.location, filePath);
      const first = relative.split(path.sep)[0];
      const service = first && first !== "." ? byLower.get(first.toLowerCase()) : undefined;
      if (service) {
        services.add(service);
      } else {
        ambiguous = true;
      }
    }
    return ambiguous && services.size === 0 ? [...cfg.services] : [...services];
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
    const experienceRoot = this.configuredExperienceRoot();
    const placeAlias = experienceRoot ? activeExperienceAlias(experienceRoot) : undefined;
    const statusText = placeAlias ? `Renium - ${placeAlias}` : "Renium";
    const placeTooltip = placeAlias ? ` for ${placeAlias}` : "";
    const pendingCount = this.pendingEditorPaths.size;
    const pendingText = pendingCount > 0 ? ` (${pendingCount})` : "";
    const pendingTooltip = pendingCount > 0
      ? `; ${pendingCount} editor change${pendingCount === 1 ? "" : "s"} waiting`
      : "";
    if (this.activeTaskName) {
      this.statusItem.text = `$(sync~spin) ${statusText}${pendingText}`;
      this.statusItem.tooltip = `${this.activeTaskName} in progress${placeTooltip}${pendingTooltip}`;
      return;
    }

    const config = vscode.workspace.getConfiguration("renium");
    const autoEnabled = config.get<boolean>("autoSyncOnSave", false);
    const liveSyncEnabled = this.editorLiveSyncRuntimeEnabled;

    if (this.bridgeServeRequested && this.isBridgeDaemonRunning()) {
      this.statusItem.text = `$(radio-tower) ${placeAlias ? `Serving - ${placeAlias}` : "Serving"}${pendingText}`;
      this.statusItem.tooltip = `Bridge server is running${placeTooltip}; Studio plugin can connect${pendingTooltip}`;
      return;
    }

    if (liveSyncEnabled && this.liveSyncWatcher) {
      this.statusItem.text = `$(sync~spin) ${statusText}${pendingText}`;
      this.statusItem.tooltip = `Live sync running${placeTooltip}${pendingTooltip}`;
      return;
    }

    if (autoEnabled) {
      this.statusItem.text = `$(sync) ${statusText}${pendingText}`;
      this.statusItem.tooltip = `Auto sync on save is enabled${placeTooltip}${pendingTooltip}`;
      return;
    }

    this.statusItem.text = `$(sync) ${statusText}${pendingText}`;
    this.statusItem.tooltip = `${placeAlias ? `Open Renium menu for ${placeAlias}` : "Open Renium menu"}${pendingTooltip}`;
  }

  private setActiveTask(taskName: string | undefined): void {
    this.activeTaskName = taskName;
    this.updateStatusBar();
  }

  private disposeLiveSyncRuntime(): Promise<void> {
    const importDrain = this.stopStudioLiveSyncRuntime();
    if (this.liveSyncGraphRefreshTimer) {
      clearTimeout(this.liveSyncGraphRefreshTimer);
      this.liveSyncGraphRefreshTimer = undefined;
    }
    if (this.liveSyncWatcher) {
      this.liveSyncWatcher.dispose();
      this.liveSyncWatcher = undefined;
    }
    for (const watcher of this.liveSyncAdditionalWatchers) {
      watcher.dispose();
    }
    this.liveSyncAdditionalWatchers = [];
    if (this.liveSyncTimer) {
      clearTimeout(this.liveSyncTimer);
      this.liveSyncTimer = undefined;
      this.liveSyncTimerDueAt = 0;
    }
    this.forcedEditorLiveSyncPathKeys.clear();
    this.suppressedEditorLiveSyncPathUntilByKey.clear();
    this.recentDirectSaveAtByPath.clear();
    this.persistPendingEditorPaths(this.liveSyncProjectRoot);
    return importDrain;
  }

  private scheduleLiveSyncGraphRefresh(projectRoot: string): void {
    if (this.liveSyncGraphRefreshTimer) {
      clearTimeout(this.liveSyncGraphRefreshTimer);
    }
    this.liveSyncGraphRefreshTimer = setTimeout(() => {
      this.liveSyncGraphRefreshTimer = undefined;
      void this.restartLiveSyncAfterGraphChange(projectRoot);
    }, 100);
  }

  private async restartLiveSyncAfterGraphChange(projectRoot: string): Promise<void> {
    if (this.liveSyncGraphRefreshRunning) {
      this.liveSyncGraphRefreshPending = true;
      return;
    }
    this.liveSyncGraphRefreshRunning = true;
    try {
    let cfg: SyncConfig;
    try {
      cfg = this.getConfig();
    } catch {
      return;
    }
    if (this.normalizePathForCompare(cfg.projectRoot) !== this.normalizePathForCompare(projectRoot)) {
      return;
    }
    invalidateProjectSourceGraph(projectRoot);
    await vscode.commands.executeCommand("renium.fileExplorer.refreshProjectGraph");
    if (this.liveSyncStartupInProgress) {
      this.liveSyncGraphRefreshPending = true;
      return;
    }
    if (!this.liveSyncWatcher) {
      return;
    }
    await this.disposeLiveSyncRuntime();
    if (this.normalizePathForCompare(this.getConfig().projectRoot) !== this.normalizePathForCompare(projectRoot)) {
      return;
    }
    await this.startLiveSync({ silent: true, bestEffort: true, graphRefresh: true });
    } finally {
      this.liveSyncGraphRefreshRunning = false;
      if (this.liveSyncGraphRefreshPending && !this.liveSyncStartupInProgress) {
        this.liveSyncGraphRefreshPending = false;
        this.scheduleLiveSyncGraphRefresh(projectRoot);
      }
    }
  }

  private pendingEditorPathsStorageKey(projectRoot?: string): string {
    let resolvedRoot = projectRoot ?? "";
    if (!resolvedRoot) {
      try {
        resolvedRoot = this.getConfig().projectRoot;
      } catch {
      }
    }
    return `renium.pendingEditorPaths:${this.normalizePathForCompare(resolvedRoot || this.context.extensionPath)}`;
  }

  private restorePendingEditorPaths(projectRoot?: string): void {
    let resolvedRoot = projectRoot;
    if (!resolvedRoot) {
      try {
        resolvedRoot = this.getConfig().projectRoot;
      } catch {
      }
    }
    this.liveSyncProjectRoot = resolvedRoot;
    this.pendingEditorPaths.clear();
    this.blockedStudioImportServices.clear();
    this.pendingEditorServicesByPath.clear();
    const stored = this.context.workspaceState.get<unknown>(
      this.pendingEditorPathsStorageKey(resolvedRoot),
    );
    const paths = Array.isArray(stored)
      ? stored
      : stored && typeof stored === "object" && !Array.isArray(stored)
        ? (stored as { paths?: unknown }).paths
        : undefined;
    if (!Array.isArray(paths)) {
      return;
    }
    for (const value of paths) {
      if (typeof value === "string" && value.length > 0) {
        this.pendingEditorPaths.add(value);
      }
    }
    const pathServices = stored && typeof stored === "object" && !Array.isArray(stored)
      ? (stored as { pathServices?: unknown }).pathServices
      : undefined;
    if (pathServices && typeof pathServices === "object" && !Array.isArray(pathServices)) {
      for (const [filePath, rawServices] of Object.entries(pathServices)) {
        if (!Array.isArray(rawServices)) {
          continue;
        }
        const services = rawServices
          .filter((service): service is string => typeof service === "string" && service.length > 0)
          .map((service) => service.toLowerCase());
        if (services.length > 0) {
          this.pendingEditorServicesByPath.set(
            this.normalizePathForCompare(filePath),
            new Set(services),
          );
        }
      }
    }
    const blockedServices = stored && typeof stored === "object" && !Array.isArray(stored)
      ? (stored as { blockedServices?: unknown }).blockedServices
      : undefined;
    let cfg: SyncConfig | undefined;
    try {
      cfg = this.getConfig();
    } catch {
    }
    for (const filePath of this.pendingEditorPaths) {
      const key = this.normalizePathForCompare(filePath);
      if (this.pendingEditorServicesByPath.has(key)) {
        continue;
      }
      const detected = cfg
        ? this.servicesForProjectSourcePath(filePath, cfg)
        : [];
      const services = detected.length > 0
        ? detected.map((service) => service.toLowerCase())
        : Array.isArray(blockedServices)
          ? blockedServices.filter((service): service is string => typeof service === "string")
            .map((service) => service.toLowerCase())
          : cfg?.services.map((service) => service.toLowerCase()) ?? [];
      if (services.length > 0) {
        this.pendingEditorServicesByPath.set(key, new Set(services));
      }
    }
    if (cfg) {
      this.recomputeBlockedStudioImportServices(cfg);
    }
  }

  private rememberPendingEditorPath(filePath: string, cfg: SyncConfig, services?: string[]): void {
    this.pendingEditorPaths.add(filePath);
    const detected = services ?? this.servicesForProjectSourcePath(filePath, cfg);
    this.pendingEditorServicesByPath.set(
      this.normalizePathForCompare(filePath),
      new Set((detected.length > 0 ? detected : cfg.services).map((service) => service.toLowerCase())),
    );
  }

  private recomputeBlockedStudioImportServices(cfg: SyncConfig): void {
    this.blockedStudioImportServices.clear();
    const pendingKeys = new Set<string>();
    for (const filePath of this.pendingEditorPaths) {
      const key = this.normalizePathForCompare(filePath);
      pendingKeys.add(key);
      let services = this.pendingEditorServicesByPath.get(key);
      if (!services) {
        const detected = this.servicesForProjectSourcePath(filePath, cfg);
        services = new Set(
          (detected.length > 0 ? detected : cfg.services).map((service) => service.toLowerCase()),
        );
        this.pendingEditorServicesByPath.set(key, services);
      }
      for (const service of services) {
        this.blockedStudioImportServices.add(service);
      }
    }
    for (const key of this.pendingEditorServicesByPath.keys()) {
      if (!pendingKeys.has(key)) {
        this.pendingEditorServicesByPath.delete(key);
      }
    }
  }

  public onProjectGraphChanged(projectRoot: string): void {
    const cfg = this.getConfig();
    if (this.normalizePathForCompare(projectRoot) !== this.normalizePathForCompare(cfg.projectRoot)) {
      return;
    }
    invalidateProjectSourceGraph(projectRoot);
    this.pendingEditorServicesByPath.clear();
    for (const filePath of this.pendingEditorPaths) {
      const services = this.servicesForProjectSourcePath(filePath, cfg);
      this.pendingEditorServicesByPath.set(
        this.normalizePathForCompare(filePath),
        new Set((services.length > 0 ? services : cfg.services).map((service) => service.toLowerCase())),
      );
    }
    this.recomputeBlockedStudioImportServices(cfg);
    void this.persistPendingEditorPaths(projectRoot);
    this.invalidateLinkStatusCache();
    if (this.liveSyncWatcher && !this.liveSyncGraphRefreshRunning) {
      this.scheduleLiveSyncGraphRefresh(projectRoot);
    }
  }

  private markEditorPathsPending(paths: string[], cfg: SyncConfig, services?: string[]): void {
    for (const filePath of paths) {
      this.rememberPendingEditorPath(filePath, cfg, services);
    }
    this.recomputeBlockedStudioImportServices(cfg);
    this.persistPendingEditorPaths();
  }

  private markEditorPathsPendingAtRoot(
    paths: string[],
    cfg: SyncConfig,
    projectRoot = cfg.projectRoot,
    services?: string[],
  ): void {
    const uniquePaths = Array.from(new Set(paths
      .map((filePath) => String(filePath).trim())
      .filter((filePath) => filePath.length > 0)));
    if (uniquePaths.length === 0) {
      return;
    }
    if (this.normalizePathForCompare(projectRoot) === this.normalizePathForCompare(cfg.projectRoot)) {
      this.markEditorPathsPending(uniquePaths, cfg, services);
      return;
    }
    const key = this.pendingEditorPathsStorageKey(projectRoot);
    const normalizedServices = (services ?? [])
      .map((service) => String(service).trim().toLowerCase())
      .filter((service) => service.length > 0);
    const update = async (): Promise<void> => {
      const stored = this.context.workspaceState.get<unknown>(key);
      const storedRecord = stored && typeof stored === "object" && !Array.isArray(stored)
        ? stored as { paths?: unknown; blockedServices?: unknown; pathServices?: unknown }
        : {};
      const storedPaths = Array.isArray(storedRecord.paths)
        ? storedRecord.paths.filter((value): value is string => typeof value === "string" && value.length > 0)
        : [];
      const mergedPaths = Array.from(new Set([...storedPaths, ...uniquePaths]));
      const pathServices = storedRecord.pathServices
        && typeof storedRecord.pathServices === "object"
        && !Array.isArray(storedRecord.pathServices)
        ? { ...storedRecord.pathServices as Record<string, unknown> }
        : {};
      for (const filePath of uniquePaths) {
        const existing = Array.isArray(pathServices[filePath])
          ? (pathServices[filePath] as unknown[])
            .filter((service): service is string => typeof service === "string" && service.length > 0)
            .map((service) => service.toLowerCase())
          : [];
        pathServices[filePath] = Array.from(new Set([...existing, ...normalizedServices]));
      }
      const blockedServices = Array.from(new Set([
        ...(Array.isArray(storedRecord.blockedServices)
          ? storedRecord.blockedServices.filter((service): service is string => typeof service === "string")
          : []),
        ...normalizedServices,
      ]));
      await this.context.workspaceState.update(key, {
        paths: mergedPaths,
        blockedServices,
        pathServices,
      });
    };
    this.pendingEditorPersistence = this.pendingEditorPersistence.then(update, update);
  }

  private pendingStudioImportServiceSet(cfg: SyncConfig): Set<string> {
    this.recomputeBlockedStudioImportServices(cfg);
    return new Set(this.blockedStudioImportServices);
  }

  private clearAppliedEditorPaths(paths: string[]): void {
    const applied = new Set(paths.map((filePath) => this.normalizePathForCompare(filePath)));
    for (const filePath of this.pendingEditorPaths) {
      if (applied.has(this.normalizePathForCompare(filePath))) {
        this.pendingEditorPaths.delete(filePath);
        this.pendingEditorServicesByPath.delete(this.normalizePathForCompare(filePath));
      }
    }
    try {
      this.recomputeBlockedStudioImportServices(this.getConfig());
    } catch {
    }
    this.persistPendingEditorPaths();
  }

  private persistPendingEditorPaths(projectRoot = this.liveSyncProjectRoot): Promise<void> {
    const key = this.pendingEditorPathsStorageKey(projectRoot);
    const value = {
      paths: [...this.pendingEditorPaths],
      blockedServices: [...this.blockedStudioImportServices],
      pathServices: Object.fromEntries(
        [...this.pendingEditorPaths].map((filePath) => [
          filePath,
          [...(this.pendingEditorServicesByPath.get(this.normalizePathForCompare(filePath)) ?? [])],
        ]),
      ),
    };
    const update = async (): Promise<void> => {
      await this.context.workspaceState.update(key, value);
    };
    this.pendingEditorPersistence = this.pendingEditorPersistence.then(update, update);
    this.updateStatusBar();
    return this.pendingEditorPersistence;
  }

  public async shutdown(): Promise<void> {
    await this.persistPendingEditorPaths();
    await this.pendingEditorPersistence;
    await this.disposeLiveSyncRuntime();
    await this.stopConsoleFollow({ releaseServe: false });
    await this.stopBridgeDaemon();
    await terminateAllProcesses();
    this.dispose();
  }

  public discardPendingEditorChanges(): void {
    const count = this.pendingEditorPaths.size;
    this.pendingEditorPaths.clear();
    this.blockedStudioImportServices.clear();
    this.pendingEditorServicesByPath.clear();
    this.persistPendingEditorPaths();
    vscode.window.showInformationMessage(
      count === 1
        ? "Discarded one pending editor change."
        : `Discarded ${count} pending editor changes.`,
    );
  }

  public async retryPendingEditorChanges(): Promise<void> {
    if (this.pendingEditorPaths.size === 0) {
      vscode.window.showInformationMessage("No editor changes are pending.");
      return;
    }
    if (!this.liveSyncWatcher) {
      await this.startLiveSync({ silent: true });
    }
    await this.flushEditorChanges();
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





class LinkDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri[] | undefined>();
  public readonly onDidChangeFileDecorations = this.emitter.event;
  private index = new Map<string, LinkFileInfo>();
  private refreshGeneration = 0;

  public constructor(private readonly controller: RobloxSyncController) {}

  public async refresh(): Promise<void> {
    const generation = ++this.refreshGeneration;
    let index: Map<string, LinkFileInfo>;
    try {
      index = await this.controller.getLinkFileIndex(true);
    } catch {
      index = new Map();
    }
    if (generation !== this.refreshGeneration) {
      return;
    }
    this.index = index;
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
  generation: number;
  preview: PackagePreviewData;
  nodesByKey: Map<string, PackagePreviewNode>;
  elementsByKey: Map<string, PackageNodeElement>;
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
  private generation = 0;

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
    const generation = this.generation;
    return this.resolveContent(info.linkId, info.nodeKey).then(
      (source) => {
        const text = source ?? "";
        if (generation === this.generation) {
          this.contents.set(key, text);
        }
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

  public refresh(): void {
    this.generation += 1;
    for (const document of vscode.workspace.textDocuments) {
      if (document.uri.scheme !== "renium-package") {
        continue;
      }
      this.contents.delete(document.uri.toString());
      this.changeEmitter.fire(document.uri);
    }
  }

  public dispose(): void {
    this.generation += 1;
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
  private propertiesUpdateChain: Promise<void> = Promise.resolve();

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
    if (this.clearDragTimer) {
      clearTimeout(this.clearDragTimer);
      this.clearDragTimer = undefined;
    }
    this.setPackageDrag(undefined);
    this.selectionGeneration += 1;
    this.previewCache.clear();
    this.scriptContentProvider.refresh();
    this.queuePackagePropertiesClear();
    this.changeEmitter.fire();
  }

  public clearSelection(_element: PackageTreeElement | undefined): void {
    this.selectionGeneration += 1;
    this.suppressExpansionTracking = true;
    this.queuePackagePropertiesClear();
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
      if (tree.generation !== this.selectionGeneration) {
        return [];
      }
      return (tree.childrenByParent.get(element.nodeKey) ?? []).map((child) => this.currentPackageElement(child) as PackageNodeElement);
    }
    if (element?.kind === "link") {
      const tree = await this.previewTree(element.link);
      if (tree.generation !== this.selectionGeneration) {
        return [];
      }
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
    if (tree.generation !== this.selectionGeneration) {
      return undefined;
    }
    if (tree.roots.length === 1 && element.parentKey === tree.roots[0].nodeKey) {
      return this.linkElement(element.link);
    }
    return tree.elementsByKey.get(element.parentKey);
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
    if (tree.generation !== this.selectionGeneration) {
      return undefined;
    }
    return tree.elementsByKey.get(nodeKey);
  }

  public async packageScriptSourceFor(linkId: string, nodeKey: string): Promise<string | undefined> {
    const linkElement = await this.elementForLinkId(linkId);
    if (!linkElement) {
      return undefined;
    }
    const tree = await this.previewTree(linkElement.link);
    if (tree.generation !== this.selectionGeneration) {
      return undefined;
    }
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
    if (tree.generation !== this.selectionGeneration) {
      return false;
    }
    const node = tree.nodesByKey.get(nodeKey);
    if (!node) {
      return false;
    }
    return this.openPackageScript(tree.preview, node, options);
  }

  private previewTree(link: CliLinkStatusLink): Promise<PackagePreviewTree> {
    const id = String(link.id ?? "");
    const generation = this.selectionGeneration;
    const cacheKey = `${generation}\0${id}`;
    const existing = this.previewCache.get(cacheKey);
    if (existing) {
      return existing;
    }
    const loading = this.controller.loadPackagePreview(link).then((preview) => {
      const nodesByKey = new Map<string, PackagePreviewNode>();
      const elementsByKey = new Map<string, PackageNodeElement>();
      const childrenByParent = new Map<string, PackageNodeElement[]>();
      const keyByRawId = new Map<string, string>();
      const childCounts = new Map<string, number>();
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
        childCounts.set(parentKey, (childCounts.get(parentKey) ?? 0) + 1);
        const element: PackageNodeElement = {
          kind: "node",
          link,
          preview,
          node,
          nodeKey: key,
          parentKey,
          childCount: 0,
          selectionVersion: this.nodeSelectionVersion(link, key),
        };
        elementsByKey.set(key, element);
      });
      for (const element of elementsByKey.values()) {
        element.childCount = childCounts.get(element.nodeKey) ?? 0;
        const bucket = childrenByParent.get(element.parentKey) ?? [];
        bucket.push(element);
        childrenByParent.set(element.parentKey, bucket);
      }
      const roots = (preview.rootIds.length > 0
        ? preview.rootIds.map((rootId) => keyByRawId.get(rootId) ?? rootId)
        : Array.from(childrenByParent.get("") ?? []).map((root) => root.nodeKey))
        .map((key) => elementsByKey.get(key))
        .filter((element): element is PackageNodeElement => !!element);
      return { generation, preview, nodesByKey, elementsByKey, childrenByParent, roots };
    });
    this.previewCache.set(cacheKey, loading);
    return loading;
  }

  public async openItem(element: PackageTreeElement | undefined): Promise<void> {
    if (!this.isCurrentElement(element)) {
      return;
    }
    if (element.kind === "link") {
      const tree = await this.previewTree(element.link);
      if (!this.isCurrentElement(element) || tree.generation !== this.selectionGeneration) {
        return;
      }
      const root = tree.roots[0];
      const generation = this.selectionGeneration;
      await this.showPackageProperties(tree.preview, root?.node, generation);
      if (root && tree.roots.length === 1 && await this.openPackageScript(tree.preview, root.node, {}, generation)) {
        return;
      }
      return;
    }
    const generation = this.selectionGeneration;
    await this.showPackageProperties(element.preview, element.node, generation);
    if (await this.openPackageScript(element.preview, element.node, {}, generation)) {
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
    const generation = this.selectionGeneration;
    const service = typeof request?.service === "string" ? request.service.trim() : "";
    const pathSegments = Array.isArray(request?.pathSegments)
      ? request.pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length < 2) {
      return false;
    }
    const normalizedPath = pathSegments[0] === service ? pathSegments : [service, ...pathSegments];
    const status = await this.controller.getLinkStatus(false);
    if (generation !== this.selectionGeneration) {
      return false;
    }
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
    if (generation !== this.selectionGeneration) {
      return false;
    }
    for (const { target, targetPath } of targets) {
      const link = packages.find((candidate) => candidate.id === target.linkId);
      if (!link) {
        continue;
      }
      const relativePath = normalizedPath.slice(targetPath.length);
      const tree = await this.previewTree(link);
      if (generation !== this.selectionGeneration || tree.generation !== generation) {
        return false;
      }
      const node = Array.from(tree.nodesByKey.values()).find((candidate) =>
        this.packageNodeMatchesExplorerRequest(candidate, relativePath, request ?? {}),
      );
      if (node && await this.openPackageScript(tree.preview, node, {}, generation)) {
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
      if (!this.isCurrentElement(element) || tree.generation !== this.selectionGeneration) {
        return;
      }
      await this.showPackageProperties(tree.preview, tree.roots[0]?.node, this.selectionGeneration);
      return;
    }
    await this.showPackageProperties(element.preview, element.node, this.selectionGeneration);
  }

  private async showPackageProperties(
    preview: PackagePreviewData,
    node: PackagePreviewNode | undefined,
    generation: number,
  ): Promise<void> {
    const payload = {
      packageId: preview.id,
      packageName: preview.name,
      source: preview.source,
      sourcePath: preview.sourcePath,
      rootClass: preview.rootClass,
      rootName: preview.rootName,
      node,
    };
    const update = this.propertiesUpdateChain.then(async () => {
      if (generation === this.selectionGeneration) {
        await vscode.commands.executeCommand("renium.properties.showPackageNode", payload);
      }
    });
    this.propertiesUpdateChain = update.catch(() => undefined);
    await update;
  }

  private queuePackagePropertiesClear(): void {
    const update = this.propertiesUpdateChain.then(async () => {
      await vscode.commands.executeCommand("renium.properties.clearPackageNode");
    });
    this.propertiesUpdateChain = update.catch(() => undefined);
  }

  private async openPackageScript(
    preview: PackagePreviewData,
    node: PackagePreviewNode,
    options: { preview?: boolean; preserveFocus?: boolean } = {},
    generation = this.selectionGeneration,
  ): Promise<boolean> {
    if (generation !== this.selectionGeneration) {
      return false;
    }
    if (!isPackageScriptClass(node.className)) {
      return false;
    }
    const source = packageNodeSource(node);
    if (source === undefined) {
      return false;
    }
    const uri = this.scriptContentProvider.uriFor(preview, node);
    const doc = await vscode.workspace.openTextDocument(uri);
    if (generation !== this.selectionGeneration) {
      return false;
    }
    await vscode.window.showTextDocument(doc, {
      preview: options.preview ?? false,
      preserveFocus: options.preserveFocus,
    });
    try {
      await vscode.languages.setTextDocumentLanguage(doc, "luau");
    } catch {
    }
    return generation === this.selectionGeneration;
  }
}

let activeController: RobloxSyncController | undefined;
let activeFileExplorerController: FileExplorerController | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const bundledCliPath = bundledReniumCliPath(context.extensionPath);
  if (fs.existsSync(bundledCliPath)) {
    context.environmentVariableCollection.prepend("PATH", `${path.dirname(bundledCliPath)}${path.delimiter}`);
  } else {
    context.environmentVariableCollection.delete("PATH");
  }
  const controller = new RobloxSyncController(context);
  activeController = controller;
  setTimeout(() => {
    void (async () => {
    try {
      const cli = controller.rbsyncViewerCli();
      const extensionVersion = String(context.extension.packageJSON.version ?? "");
      if (cli && extensionVersion && fs.existsSync(cli.cliPath)) {
        const cliVersion = await controller.detectedCliVersion(cli.cliPath);
        if (cliVersion && cliVersion !== extensionVersion) {
          void vscode.window.showWarningMessage(
            `This extension is v${extensionVersion} but ${RUST_CLI_BINARY} is v${cliVersion}. Update whichever is older so they match — syncing may misbehave until then.`,
          );
        }
      }
    } catch {
    }
    })();
  }, 0);
  const fileExplorerController = new FileExplorerController(context, controller.gitViewActions());
  activeFileExplorerController = fileExplorerController;
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
        const currentSelection = packagesTreeView.selection[0];
        if (currentSelection && currentSelection !== selectedPackage) {
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
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      void controller.configureLuauSourcemapForEditor(editor);
    }),
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
    vscode.commands.registerCommand("renium.projectTools", () => controller.openProjectTools()),
    vscode.commands.registerCommand("renium.projectDoctor", () => controller.runProjectDoctor()),
    vscode.commands.registerCommand("renium.projectBuild", () => controller.buildProject()),
    vscode.commands.registerCommand("renium.projectConfig", () => controller.openProjectConfiguration()),
    vscode.commands.registerCommand("renium.projectDocs", () => controller.openCliDocumentation()),
    vscode.commands.registerCommand("renium.createAgentInstructions", () => controller.createAgentInstructions()),
    vscode.commands.registerCommand("renium.followConsole", () => controller.followStudioConsole()),
    vscode.commands.registerCommand("renium.checkUpdates", () => controller.checkForUpdates()),
    vscode.commands.registerCommand("renium.repairInstallation", () => controller.repairInstallation()),
    vscode.commands.registerCommand("renium.uninstallStudioPlugin", () => controller.uninstallStudioPlugin()),
    vscode.commands.registerCommand("renium.openExplorer", () => vscode.commands.executeCommand("workbench.view.extension.reniumContainer")),
    vscode.commands.registerCommand("renium.projectGraphChanged", (projectRoot?: string) => {
      if (typeof projectRoot === "string" && projectRoot.length > 0) {
        controller.onProjectGraphChanged(projectRoot);
      }
    }),
    vscode.commands.registerCommand("renium.managePlaces", () => controller.managePlaces()),
    vscode.commands.registerCommand("renium.addCurrentPlace", () => controller.addCurrentPlace()),
    vscode.commands.registerCommand("renium.switchPlace", () => controller.switchPlace()),
    vscode.commands.registerCommand("renium.renamePlace", () => controller.renamePlace()),
    vscode.commands.registerCommand("renium.gitSync", () => controller.openGitSync()),
    vscode.commands.registerCommand("renium.gitSync.status", () => controller.gitStatus()),
    vscode.commands.registerCommand("renium.gitSync.fetch", () => controller.gitFetch()),
    vscode.commands.registerCommand("renium.gitSync.pull", () => controller.gitPull()),
    vscode.commands.registerCommand("renium.gitSync.commitAndPush", () => controller.gitCommitAndPush()),
    vscode.commands.registerCommand("renium.gitSync.pullFromStudioAndPush", () => controller.gitCommitAndPush({ pullFromStudioFirst: true })),
    vscode.commands.registerCommand("renium.gitSync.connectRepo", () => controller.gitConnectRepo()),
    vscode.commands.registerCommand("renium.gitSync.publishBranch", () => controller.gitPublishBranch()),
    vscode.commands.registerCommand("renium.gitSync.createBranch", () => controller.gitCreateBranch()),
    vscode.commands.registerCommand("renium.gitSync.checkoutBranch", () => controller.gitCheckoutBranch()),
    vscode.commands.registerCommand("renium.gitSync.openRemote", () => controller.gitOpenRemote()),
    vscode.commands.registerCommand("renium.pullFromStudio", () => controller.pullFromStudio()),
    vscode.commands.registerCommand("renium.pushToStudio", () => controller.pushToStudio()),
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
      refreshLinkManifestWatcher();
      void controller.refreshLinkPackageSourceWatchers().catch(() => undefined);
      void linkDecorationProvider.refresh();
      void controller.pushLinkStateToExplorer();
      packagesProvider.refresh();
    }),
    vscode.commands.registerCommand("renium.startLiveSync", () => controller.startLiveSync()),
    vscode.commands.registerCommand("renium.stopLiveSync", () => controller.stopLiveSync()),
    vscode.commands.registerCommand("renium.retryEditorInitialSync", () => controller.retryEditorInitialSync()),
    vscode.commands.registerCommand("renium.retryPendingEditorChanges", () => controller.retryPendingEditorChanges()),
    vscode.commands.registerCommand("renium.discardPendingEditorChanges", () => controller.discardPendingEditorChanges()),
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

  let linkManifestWatcherPath = "";
  let linkManifestWatcherGeneration = -1;
  let linkManifestDisposables: vscode.Disposable[] = [];
  let linkApplyTimer: NodeJS.Timeout | undefined;
  const sameLinkManifestPath = (left: string, right: string): boolean => {
    const normalize = (value: string): string => {
      const normalized = path.resolve(value);
      return process.platform === "win32" ? normalized.toLowerCase() : normalized;
    };
    return normalize(left) === normalize(right);
  };
  const onLinkManifestChanged = (uri: vscode.Uri): void => {
    let active: { filePath: string; autoApply: boolean; projectRoot: string; generation: number };
    try {
      active = controller.activeLinkManifest();
    } catch {
      return;
    }
    if (uri.scheme !== "file" || !sameLinkManifestPath(uri.fsPath, active.filePath)) {
      return;
    }
    controller.invalidateLinkStatusCache();
    if (active.autoApply) {
      if (linkApplyTimer) {
        clearTimeout(linkApplyTimer);
      }
      const captured = active;
      linkApplyTimer = setTimeout(() => {
        linkApplyTimer = undefined;
        void controller.linkApply({
          silent: true,
          expectedProjectRoot: captured.projectRoot,
          expectedGeneration: captured.generation,
        }).catch(() => undefined);
      }, 1500);
    }
  };
  const refreshLinkManifestWatcher = (): void => {
    let active: { filePath: string; autoApply: boolean; projectRoot: string; generation: number };
    try {
      active = controller.activeLinkManifest();
    } catch {
      return;
    }
    if (
      linkManifestDisposables.length > 0
      && sameLinkManifestPath(active.filePath, linkManifestWatcherPath)
      && active.generation === linkManifestWatcherGeneration
    ) {
      return;
    }
    for (const disposable of linkManifestDisposables) {
      disposable.dispose();
    }
    if (linkApplyTimer) {
      clearTimeout(linkApplyTimer);
      linkApplyTimer = undefined;
    }
    const watcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(path.dirname(active.filePath), path.basename(active.filePath)),
    );
    linkManifestWatcherPath = active.filePath;
    linkManifestWatcherGeneration = active.generation;
    linkManifestDisposables = [
      watcher,
      watcher.onDidChange(onLinkManifestChanged),
      watcher.onDidCreate(onLinkManifestChanged),
      watcher.onDidDelete(onLinkManifestChanged),
    ];
  };
  refreshLinkManifestWatcher();
  context.subscriptions.push(
    new vscode.Disposable(() => {
      for (const disposable of linkManifestDisposables) {
        disposable.dispose();
      }
      linkManifestDisposables = [];
      if (linkApplyTimer) {
        clearTimeout(linkApplyTimer);
        linkApplyTimer = undefined;
      }
    }),
  );

  void linkDecorationProvider.refresh();
  void controller.pushLinkStateToExplorer();
  void controller.refreshLinkPackageSourceWatchers().catch(() => undefined);
  controller.scheduleStartupLinkRefresh();
  setTimeout(() => {
    void restoreOpenPackageScriptTabs().catch(() => undefined);
  }, 500);

  const cfg = vscode.workspace.getConfiguration("renium");
  if (cfg.get<boolean>("editorLiveSyncEnabled", false) === true || cfg.get<boolean>("editorLiveSyncOnStartup", false) === true) {
    void controller.startLiveSync({ silent: true, bestEffort: true });
  }
}

export async function deactivate(): Promise<void> {
  const controller = activeController;
  const fileExplorerController = activeFileExplorerController;
  activeController = undefined;
  activeFileExplorerController = undefined;
  const results = await Promise.allSettled([
    fileExplorerController?.prepareShutdown(),
    controller?.shutdown(),
  ]);
  const failures = results
    .filter((result): result is PromiseRejectedResult => result.status === "rejected")
    .map((result) => result.reason);
  if (failures.length === 1) {
    throw failures[0];
  }
  if (failures.length > 1) {
    throw new AggregateError(failures, "Renium shutdown failed.");
  }
}
