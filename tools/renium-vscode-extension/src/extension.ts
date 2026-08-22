import * as childProcess from "child_process";
import * as crypto from "crypto";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import {
  AutomationClient,
  editorBridgeWaitSeconds,
  type CommandRunResult,
} from "./automationClient";
import {
  ensureReniumAgentInstructions,
  isReniumProjectRoot,
} from "./agentInstructions";
import {
  bundledReniumCliPath,
  reniumBinaryName,
} from "./cliResolution";
import {
  mergeAndResolve,
  normalizeConflictPolicy,
  sameSourceText,
  withLineEnding,
  type ConflictPolicy,
} from "./conflictMerge";
import { buildChangePreviewHtml, type ChangePreviewRow } from "./changePreviewHtml";
import {
  parseCliJsonObject,
  parseEditorPushSummary,
  parseStudioChangeState,
  studioChangeAckOptions,
  studioChangeLogEntries,
  studioChangeSeq,
  summaryNumber,
  type StudioChangeLog,
  type StudioChangeState,
  type StudioEditorAction,
  type StudioPropertyChange,
} from "./studioSyncProtocol";
import {
  commitStudioSnapshotFingerprints,
  diffStudioSnapshots,
  type StudioSnapshotDiff,
} from "./studioSnapshotFingerprint";
import {
  projectProcessOwner,
  spawnTrackedProcess,
  terminateAllProcesses,
  terminateProcess,
  terminateProcessesForOwner,
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
  resolveExperiencePlaceByAlias,
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
import { GitController } from "./gitController";
import { renderCommandArgs } from "./gitSync";
import {
  PackageSyncController,
  type PendingPackageSource,
} from "./packageSyncController";
import {
  SETTINGS_FILE_NAME,
  collectFilesRecursively,
  ensureFileExists,
  ensurePlaceFileExtension,
  filesystemPathKey,
  isPathInside,
  isLuaSourcePath,
  isReniumSettingsFileName,
  normalizeReportedServices,
  normalizeServices,
  pickWorkspaceRoot,
  recordValue,
  resolveConfigPath,
  robloxPlaceFormatFromPath,
  safeFileComponent,
  tabInputUris,
  writeUtf8FileIfChanged,
  prefixProcessOutput,
  type RobloxPlaceFormat,
} from "./utils";
import { SettingsStoreEditorProvider } from "./settingsStoreViewer";
import { isRobloxModel } from "./pluginDistribution";
import {
  LinkDecorationProvider,
  PackageScriptContentProvider,
  PackageScriptDecorationProvider,
  PackagesTreeProvider,
  packageScriptUriInfo,
  type CliLinkStatusLink,
  type LinkedPackageScriptPreviewRequest,
  type OpenPackageScriptTab,
  type PackageTreeElement,
} from "./packagesView";
import {
  ProjectSourceGraph,
  ProjectSourceOwner,
  invalidateProjectSourceGraph,
  loadProjectSourceGraph,
  loadProjectSourceRoot,
  loadProjectSourceLocations,
} from "./sharedConfig";
import {
  DEFAULT_STUDIO_LIVE_SYNC_POLL_MS,
  MIN_STUDIO_LIVE_SYNC_POLL_MS,
  SyncConfigResolver,
  type ReniumLogLevel,
  type SyncConfig,
} from "./syncConfig";
import { AUTOMATION_OP } from "./automationProtocol.generated";

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
  ".renium",
] as const;
const RENIUM_SINGLE_PLACE_FILES = [
  "renium.project.jsonc",
  "renium.project.json",
  "sourcemap.json",
  "renium-link.json",
  ".renium",
] as const;

type ActionMenuItem = vscode.QuickPickItem & { action: string };

function menuSeparator(label: string): vscode.QuickPickItem {
  return { label, kind: vscode.QuickPickItemKind.Separator };
}

async function pickMenuAction(
  title: string,
  items: Array<ActionMenuItem | vscode.QuickPickItem>,
): Promise<string | undefined> {
  const picked = await vscode.window.showQuickPick(items, {
    title,
    placeHolder: "Choose an action",
  });
  const action = (picked as Partial<ActionMenuItem> | undefined)?.action;
  return typeof action === "string" ? action : undefined;
}

function reniumPlaceProjectPaths(root: string): string[] {
  return [...new Set([loadProjectSourceRoot(root), ...RENIUM_PLACE_PROJECT_FILES])];
}

function hasSinglePlaceProject(root: string): boolean {
  return [loadProjectSourceRoot(root), ...RENIUM_SINGLE_PLACE_FILES]
    .some((name) => fs.existsSync(path.join(root, name)));
}

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
  localFile?: string;
  pid?: number;
};

type LocalPlaceUpdateBehavior = "ask" | "leaveOpen" | "saveAndClose" | "terminate";

type ConnectedStudioPlace = {
  runtimeId?: string;
  placeId: number;
  gameId?: number;
  placeName: string;
};

type CliExportGameFileResult = {
  ok?: boolean;
  output?: string;
  format?: string;
  services?: string[];
  serviceCount?: number;
  instances?: number;
};

type ReniumUpdateStatus = {
  currentVersion?: string;
  latestVersion?: string;
  updateAvailable?: boolean;
  signature?: string;
};

type HandledStudioActions = {
  acknowledged: string[];
  updateVersion?: string;
};

type EditorPushOptions = {
  force?: boolean;
  fullSync?: boolean;
  projectRoot?: string;
  verifySources?: boolean;
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
  studioSnapshotFingerprintByService: Array<[string, string]>;
  pendingLinkPackageSourcePaths: Array<[string, PendingPackageSource]>;
};

type ProjectRootConfigurationSnapshot = {
  globalValue?: string;
  workspaceValue?: string;
  workspaceFolderValue?: string;
};

type EditorPushOutcome = "applied" | "skipped";

type EditorDirectPushRequest = {
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

type EditorPropertyPushRequest = EditorDirectPushRequest & {
  scope?: "metadata" | "property" | "attribute";
  property?: string;
  value?: unknown;
};

type EditorDeletePushRequest = EditorDirectPushRequest;

type EditorDirectPushContext = {
  cfg: SyncConfig;
  service: string;
  projectRoot: string;
  changedPaths: string[];
};

type ProgrammaticEditorWriteRequest = {
  paths?: string[] | string;
  fileWrites?: "pause" | "resume" | "queue";
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

function existingReniumSettingsFile(projectRoot: string, srcDir: string, service: string): string {
  const serviceDir = path.join(projectRoot, srcDir, service);
  return path.join(serviceDir, SETTINGS_FILE_NAME);
}
const CLI_BINARY = reniumBinaryName();
const MAX_STUDIO_LIVE_SYNC_EVENT_WAIT_MS = 25_000;
const MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS = 2000;
const MAX_STUDIO_LIVE_SYNC_ERROR_POLL_MS = 5000;
const STUDIO_LIVE_SYNC_POLL_BACKOFF_MULTIPLIER = 1.75;
const DEFAULT_COMMAND_TIMEOUT_MS = 30 * 60 * 1000;
const MAX_COMMAND_TIMEOUT_MS = 30 * 60 * 1000;

function safePlaceFileName(name: string, format: RobloxPlaceFormat): string {
  return `${safeFileComponent(name || "Game")}.${format}`;
}

async function executeCommandBestEffort(command: string, ...args: unknown[]): Promise<void> {
  try {
    await vscode.commands.executeCommand(command, ...args);
  } catch {
  }
}

class RobloxSyncController {
  private readonly output: vscode.OutputChannel;
  private readonly statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 200);
  private queue: Promise<void> = Promise.resolve();
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
  private studioSnapshotFingerprintByService = new Map<string, string>();
  private editorLiveSyncRuntimeEnabled = false;
  private daemonPendingPaths = new Set<string>();
  private studioConflictPolicyOverride: ConflictPolicy | undefined;
  private studioRuntimeSettings: Record<string, unknown> | undefined;
  private conflictMarkerWarnedKeys = new Set<string>();
  private readonly automationClient: AutomationClient;
  private readonly configResolver: SyncConfigResolver;
  public readonly git: GitController<SyncConfig>;
  public readonly packages: PackageSyncController<SyncConfig>;
  private publishedPlaceNames = new Map<number, string>();
  private publishedRootPlaceIds = new Map<number, number>();
  private bridgeServeRequested = false;
  private liveSyncOwnsServe = false;
  private liveSyncStartPromise: Promise<void> | undefined;
  private liveSyncStartupInProgress = false;
  private liveSyncStopRequested = false;
  private autoSyncTimer: NodeJS.Timeout | undefined;
  private pendingAutoPaths = new Set<string>();
  private activeTaskName: string | undefined;
  private experienceChangeInProgress = false;
  private experienceGeneration = 0;
  private configuredProjectRoot: string | undefined;
  private projectRootConfigurationSnapshot: ProjectRootConfigurationSnapshot = {};
  private configurationChangeQueue: Promise<void> = Promise.resolve();
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
  private updateCheckTimer: NodeJS.Timeout | undefined;
  private updateCheckPromise: Promise<ReniumUpdateStatus> | undefined;
  private updateInstallPromise: Promise<void> | undefined;
  private daemonFileSyncEnabled = false;
  private daemonFileSyncGeneration = 0;
  private daemonFileSyncRestartTimer: NodeJS.Timeout | undefined;
  private daemonFileSyncRestartAttempts = 0;
  private daemonFileSyncStatusTimer: NodeJS.Timeout | undefined;
  private daemonFileSyncStatusInFlight = false;
  private daemonFileSyncError: string | undefined;
  private disposed = false;

  public constructor(private readonly context: vscode.ExtensionContext) {
    const output = vscode.window.createOutputChannel("Renium");
    this.output = output;
    this.configResolver = new SyncConfigResolver(
      this.context,
      output,
      (experienceRoot) => this.restoreActiveExperiencePlace(experienceRoot),
    );
    const appendLine = output.appendLine.bind(output);
    output.appendLine = (value: string): void => {
      if (this.shouldWriteOutput(this.outputLevel(value))) {
        appendLine(value);
      }
    };
    this.automationClient = new AutomationClient(
      output,
      () => this.configuredProjectRoot ?? this.context.extensionPath,
      () => this.scheduleDaemonFileSyncRestart(),
    );
    this.git = new GitController<SyncConfig>({
      context: this.context,
      output: this.output,
      getConfig: () => this.getConfig(),
      enqueue: (taskName, task) => this.enqueue(taskName, task),
      experienceChanging: () => this.experienceChangeInProgress,
      experienceGeneration: () => this.experienceGeneration,
      servicesForProjectSourcePath: (filePath, config) =>
        this.servicesForProjectSourcePath(filePath, config),
      isProjectSourcePath: (filePath, config) => this.isProjectSourcePath(filePath, config),
      pushEditorPathsNow: (paths, options) => this.pushEditorPathsNow(paths, options),
      noteProgrammaticEditorWrite: (request) => this.noteProgrammaticEditorWrite(request),
      pullFromStudio: async (config) => {
        await this.runExport({
          services: config.services,
          runImport: true,
          notifyOnSuccess: false,
          reason: "",
          destructive: true,
        });
      },
    });
    this.packages = new PackageSyncController<SyncConfig>({
      output: this.output,
      getConfig: () => this.getConfig(),
      tryGetConfig: () => this.tryGetConfig(),
      enqueue: (taskName, task) => this.enqueue(taskName, task),
      experienceChanging: () => this.experienceChangeInProgress,
      experienceGeneration: () => this.experienceGeneration,
      logResolvedConfig: (config) => this.logResolvedConfig(config),
      runCommand: (command, args, cwd, label, heartbeat, options) =>
        this.runCommand(command, args, cwd, label, heartbeat, options),
      canUseStudioPushPipeline: () => this.canUseStudioPushPipeline(),
      noteStudioPushSkipped: (reason) => this.noteStudioPushSkipped(reason),
      pushEditorPathsNow: (paths, options) => this.pushEditorPathsNow(paths, options),
      pushEditorDeleteNow: (request) => this.pushEditorDeleteNow(request),
      noteProgrammaticEditorWrite: (request) => this.noteProgrammaticEditorWrite(request),
      isEditorLiveSyncActive: () => this.isEditorLiveSyncActive(),
      executeCommandBestEffort,
    });
    this.restoreActiveExperiencePlace();
    const initialConfig = pickWorkspaceRoot() ? this.getConfig() : undefined;
    if (initialConfig) {
      this.configuredProjectRoot = initialConfig.projectRoot;
      this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
      this.ensureAgentInstructions(initialConfig.experienceRoot);
      void this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
    }
    this.statusItem.command = "renium.openMenu";
    this.statusItem.show();
    this.updateStatusBar();
  }

  private ensureAgentInstructions(projectRoot: string): string[] {
    try {
      const written = ensureReniumAgentInstructions(
        this.context.extensionPath,
        projectRoot,
      );
      for (const filePath of written) {
        this.output.appendLine(`[renium] wrote ${filePath}`);
      }
      return written;
    } catch (error) {
      this.output.appendLine(
        `[renium] could not create agent instructions: ${error instanceof Error ? error.message : String(error)}`,
      );
      return [];
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
    const written = ensureReniumAgentInstructions(
      this.context.extensionPath,
      cfg.experienceRoot,
      true,
    );
    vscode.window.showInformationMessage(
      written.length > 0
        ? "Updated Renium agent instructions."
        : "Renium agent instructions are up to date.",
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
              if (isPathInside(editor.document.uri.fsPath, placeRoot)) {
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
    return resolveConfigPath(cfg.get<string>("projectRoot", "${workspaceFolder}"), workspaceRoot);
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
    setActiveExperiencePlace(experienceRoot, stored[filesystemPathKey(experienceRoot)]);
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
      [filesystemPathKey(experienceRoot)]: alias,
    };
    try {
      await this.context.workspaceState.update(RENIUM_ACTIVE_EXPERIENCE_PLACES_STATE_KEY, stored);
    } catch (error) {
      throw new Error(
        `Could not save the active place selection. ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  private captureExperienceChange(experienceRoot: string): ExperienceChangeSnapshot {
    return {
      ...this.captureProjectSyncState(),
      alias: activeExperienceAlias(experienceRoot),
    };
  }

  private captureProjectSyncState(projectRoot = this.configuredProjectRoot): ExperienceChangeSnapshot {
    return {
      projectRoot,
      studioSnapshotFingerprintByService: [...this.studioSnapshotFingerprintByService],
      pendingLinkPackageSourcePaths: this.packages.pendingSourceEntries(),
    };
  }

  private restoreProjectSyncState(snapshot: ExperienceChangeSnapshot): void {
    this.configuredProjectRoot = snapshot.projectRoot;
    this.daemonPendingPaths.clear();
    this.studioSnapshotFingerprintByService = new Map(snapshot.studioSnapshotFingerprintByService);
    this.packages.restorePendingSources(
      snapshot.pendingLinkPackageSourcePaths,
      snapshot.projectRoot,
      this.experienceGeneration,
    );
    this.liveSyncProjectRoot = snapshot.projectRoot;
    this.sourcemapCache = undefined;
    this.studioRuntimeSettings = undefined;
    this.studioConflictPolicyOverride = undefined;
    this.packages.resetStatusCache();
  }

  private resetProjectScopedCaches(): void {
    this.sourcemapCache = undefined;
    this.studioRuntimeSettings = undefined;
    this.studioConflictPolicyOverride = undefined;
    this.studioSnapshotFingerprintByService.clear();
    this.conflictMarkerWarnedKeys.clear();
    this.packages.resetStatusCache();
  }

  private async rollbackExperienceChange(
    experienceRoot: string,
    snapshot: ExperienceChangeSnapshot,
    resumeLiveSync: boolean,
  ): Promise<void> {
    setActiveExperiencePlace(experienceRoot, snapshot.alias);
    this.restoreProjectSyncState(snapshot);
    try {
      await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
      await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
    } finally {
      this.experienceChangeInProgress = false;
    }
    this.packages.notifyLinksChanged();
    this.updateStatusBar();
    if (this.bridgeServeRequested && !this.isBridgeDaemonRunning()) {
      await this.serve({ silent: true, bestEffort: true });
    }
    if (resumeLiveSync) {
      await this.startLiveSync({ silent: true, bestEffort: true });
    }
    if (snapshot.projectRoot) {
      this.packages.resumePendingSources(
        snapshot.projectRoot,
        this.experienceGeneration,
      );
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
      this.daemonPendingPaths.clear();
      if (this.activeTaskName) {
        throw new Error(`Wait for ${this.activeTaskName} to finish before changing places.`);
      }
      if (this.autoSyncTimer) {
        clearTimeout(this.autoSyncTimer);
        this.autoSyncTimer = undefined;
      }
      this.packages.pausePendingSources();
      this.pendingAutoPaths.clear();
      await this.stopConsoleFollow();
      if (previousProjectRoot) {
        await terminateProcessesForOwner(projectProcessOwner(previousProjectRoot));
      }
      await this.stopBridgeDaemon();
      return resumeLiveSync;
    } catch (error) {
      await vscode.commands.executeCommand("renium.fileExplorer.cancelProjectSwitch");
      this.experienceChangeInProgress = false;
      throw error;
    }
  }

  private async finishExperienceChange(experienceRoot: string, alias: string): Promise<void> {
    const active = resolveExperiencePlaceByAlias(experienceRoot, alias);
    setActiveExperiencePlace(experienceRoot, alias);
    this.configuredProjectRoot = active.projectRoot;
    fs.mkdirSync(active.projectRoot, { recursive: true });
    this.daemonPendingPaths.clear();
    this.resetProjectScopedCaches();
    await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
    await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
    this.ensureAgentInstructions(experienceRoot);
    this.packages.notifyLinksChanged();
    this.updateStatusBar();
    if (this.bridgeServeRequested && !this.isBridgeDaemonRunning()) {
      await this.serve({ silent: true, bestEffort: true });
    }
    await this.persistActiveExperiencePlace(experienceRoot, alias);
    this.experienceChangeInProgress = false;
    this.packages.resumePendingSources(
      active.projectRoot,
      this.experienceGeneration,
    );
  }

  private async activateExperiencePlace(experienceRoot: string, alias: string): Promise<void> {
    resolveExperiencePlaceByAlias(experienceRoot, alias);
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
    return this.configResolver.configuredLogLevel(this.studioRuntimeSettings);
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

  public dispose(): void {
    this.disposed = true;
    this.daemonFileSyncEnabled = false;
    this.daemonFileSyncError = undefined;
    if (this.daemonFileSyncRestartTimer) {
      clearTimeout(this.daemonFileSyncRestartTimer);
      this.daemonFileSyncRestartTimer = undefined;
    }
    if (this.daemonFileSyncStatusTimer) {
      clearTimeout(this.daemonFileSyncStatusTimer);
      this.daemonFileSyncStatusTimer = undefined;
    }
    if (this.autoSyncTimer) {
      clearTimeout(this.autoSyncTimer);
      this.autoSyncTimer = undefined;
    }
    if (this.updateCheckTimer) {
      clearTimeout(this.updateCheckTimer);
      this.updateCheckTimer = undefined;
    }
    this.packages.dispose();
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
    return cfg.editorLiveSyncEnabled && this.editorLiveSyncRuntimeEnabled;
  }

  private canUseStudioPushPipeline(): boolean {
    if (this.isEditorLiveSyncActive()) {
      return true;
    }
    return this.bridgeServeRequested && this.isBridgeDaemonRunning();
  }

  private noteStudioPushSkipped(reason: string): void {
    this.output.appendLine(`[renium] Studio push skipped: ${reason}`);
  }

  public async openMenu(): Promise<void> {
    const cfg = this.getConfig();
    const liveSyncRunning = cfg.editorLiveSyncEnabled || this.editorLiveSyncRuntimeEnabled || this.liveSyncStartPromise !== undefined;
    const placeName = cfg.activePlace?.name ?? cfg.activePlaceAlias;
    const action = await pickMenuAction(placeName ? `Renium — ${placeName}` : "Renium", [
      menuSeparator("Sync"),
      {
        label: "$(cloud-download) Pull from Studio",
        description: "Replace project files with the current Studio place",
        action: "pullFromStudio",
      },
      {
        label: "$(cloud-upload) Push to Studio",
        description: "Replace the current Studio place with project files",
        action: "pushToStudio",
      },
      {
        label: liveSyncRunning ? "$(circle-slash) Stop Live Sync" : "$(broadcast) Start Live Sync",
        description: liveSyncRunning
          ? "Stop synchronizing new Studio and project changes"
          : "Keep Studio and project files synchronized in both directions",
        action: liveSyncRunning ? "stopLive" : "startLive",
      },
      {
        label: cfg.autoSyncOnSave ? "$(circle-slash) Disable Sync on Save" : "$(history) Enable Sync on Save",
        description: cfg.autoSyncOnSave
          ? "Stop sending saved file changes to Studio"
          : "Send saved file changes to Studio when Live Sync is off",
        action: "toggleAuto",
      },
      menuSeparator("Project"),
      {
        label: "$(list-tree) Places...",
        description: "Add, switch, rename, or reorder experience places",
        action: "managePlaces",
      },
      {
        label: "$(git) Git",
        description: "Review changes and pull, commit, or push the project",
        action: "gitSync",
      },
      {
        label: "$(package) Packages & Links...",
        description: "Install packages and update linked content",
        action: "packagesAndLinks",
      },
      {
        label: "$(save) Build & Export...",
        description: "Build the project or write place and snapshot files",
        action: "buildAndExport",
      },
      menuSeparator("Tools"),
      {
        label: "$(radio-tower) Studio Tools...",
        description: "Manage the Studio connection, console, and plugin",
        action: "studioTools",
      },
      {
        label: "$(tools) Settings & Diagnostics...",
        description: "Configure or diagnose Renium and open help or logs",
        action: "settingsAndDiagnostics",
      },
      {
        label: "$(versions) Check for Updates",
        description: "Check for and install a newer Renium version",
        action: "update",
      },
    ]);

    switch (action) {
      case "pullFromStudio":
        await this.pullFromStudio();
        return;
      case "pushToStudio":
        await this.pushToStudio();
        return;
      case "managePlaces":
        await this.managePlaces();
        return;
      case "startLive":
        await this.startLiveSync();
        return;
      case "stopLive":
        await this.stopLiveSync();
        return;
      case "gitSync":
        await this.git.openGitSync();
        return;
      case "packagesAndLinks":
        await this.openPackagesAndLinks();
        return;
      case "buildAndExport":
        await this.openBuildAndExport();
        return;
      case "studioTools":
        await this.openStudioTools();
        return;
      case "settingsAndDiagnostics":
        await this.openProjectTools();
        return;
      case "update":
        await this.checkForUpdates();
        return;
      case "toggleAuto":
        await this.toggleAutoSyncOnSave();
        return;
      default:
        return;
    }
  }

  private async openPackagesAndLinks(): Promise<void> {
    switch (await pickMenuAction("Renium — Packages & Links", [
      {
        label: "$(package) Sync Wally Packages",
        description: "Install dependencies and update their package instances",
        action: "wally",
      },
      {
        label: "$(link) Sync Links",
        description: "Update linked targets from their sources",
        action: "syncLinks",
      },
      {
        label: "$(add) Add Link...",
        description: "Reuse a local, Git, or Wally source elsewhere in the project",
        action: "addLink",
      },
    ])) {
      case "wally":
        await this.packages.syncWallyPackages();
        return;
      case "syncLinks":
        await this.packages.linkApply();
        return;
      case "addLink":
        await this.packages.addLinkInteractive();
        return;
    }
  }

  private async openBuildAndExport(): Promise<void> {
    switch (await pickMenuAction("Renium — Build & Export", [
      {
        label: "$(package) Build Project",
        description: "Build the outputs configured for the active place",
        action: "build",
      },
      {
        label: "$(save) Export Place File...",
        description: "Write a standalone .rbxl or .rbxlx file from project files",
        action: "placeFile",
      },
      {
        label: "$(export) Export Studio Snapshots",
        description: "Save Studio snapshots without changing project files",
        action: "snapshots",
      },
    ])) {
      case "build":
        await this.buildProject();
        return;
      case "placeFile":
        await this.exportGameFile();
        return;
      case "snapshots":
        await this.exportSnapshotsOnly();
        return;
    }
  }

  private async openStudioTools(): Promise<void> {
    const cfg = this.getConfig();
    const liveSyncRunning = cfg.editorLiveSyncEnabled || this.editorLiveSyncRuntimeEnabled || this.liveSyncStartPromise !== undefined;
    const serving = this.bridgeServeRequested && this.isBridgeDaemonRunning();
    const items: Array<ActionMenuItem | vscode.QuickPickItem> = [menuSeparator("Studio")];
    if (!liveSyncRunning) {
      items.push({
        label: serving ? "$(debug-disconnect) Stop Studio Connection" : "$(radio-tower) Start Studio Connection",
        description: serving
          ? "Stop accepting connections from the Studio plugin"
          : "Allow the Studio plugin to connect to this project",
        action: serving ? "stopConnection" : "startConnection",
      });
    }
    items.push(
      {
        label: this.consoleFollowRunning ? "$(debug-pause) Pause Studio Console" : "$(terminal) Follow Studio Console",
        description: this.consoleFollowRunning
          ? "Stop streaming new Studio output"
          : "Stream Studio output into the editor",
        action: "console",
      },
      menuSeparator("Plugin"),
      {
        label: "$(cloud-download) Install or Update Studio Plugin",
        description: "Install this Renium version in Roblox Studio",
        action: "install",
      },
      {
        label: "$(tools) Repair Studio Plugin",
        description: "Reinstall the matching plugin if setup is damaged",
        action: "repair",
      },
      {
        label: "$(trash) Uninstall Studio Plugin...",
        description: "Remove Renium from Roblox Studio",
        action: "uninstall",
      },
    );
    switch (await pickMenuAction("Renium — Studio Tools", items)) {
      case "startConnection":
        await this.serve();
        return;
      case "stopConnection":
        await this.stopServe();
        return;
      case "console":
        await this.followStudioConsole();
        return;
      case "install":
        await this.installStudioPlugin();
        return;
      case "repair":
        await this.repairInstallation();
        return;
      case "uninstall":
        await this.uninstallStudioPlugin();
        return;
    }
  }

  private async connectedStudioPlaces(attempt = 0): Promise<ConnectedStudioPlace[]> {
    const cfg = this.getConfig();
    const result = await this.runAutomationOperation(
      cfg.cliPath,
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
        const client = recordValue(value) as BridgeClientInfo | undefined;
        if (!client) {
          return undefined;
        }
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
      return this.connectedStudioPlaces(attempt + 1);
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
    const details = recordValue(raw);
    if (!details) {
      return undefined;
    }
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
    const data = recordValue(raw)?.data;
    if (!Array.isArray(data)) {
      return undefined;
    }
    const gameDetails = data.map(recordValue).find((value) => Number(value?.id) === gameId);
    if (!gameDetails) {
      return undefined;
    }
    const rootPlaceId = Number(gameDetails.rootPlaceId);
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
      const originalManifest = manifest;
      manifest = manifest
        ? {
          ...manifest,
          placeOrder: [...manifest.placeOrder],
          places: { ...manifest.places },
        }
        : {
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
      const items: ActionMenuItem[] = [
        {
          label: "$(add) Add Connected Place",
          description: "Add the current Studio place to this experience",
          action: "addCurrentPlace",
        },
      ];
      if (active) {
        items.push(
          {
            label: "$(arrow-swap) Switch Place...",
            description: "Choose which experience place is active",
            action: "switchPlace",
          },
          {
            label: "$(edit) Rename Place...",
            description: "Rename the active place and its project folder",
            action: "renamePlace",
          },
          {
            label: "$(list-ordered) Reorder Places...",
            description: "Change the order in which places appear",
            action: "reorderPlaces",
          },
        );
      }
      const picked = await vscode.window.showQuickPick(items, {
        title: "Renium — Places",
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
    if (process.platform !== "win32" && process.platform !== "darwin") {
      void vscode.window.showErrorMessage("Roblox Studio is only available on Windows and macOS.");
      return;
    }

    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: "Installing Studio plugin..." },
      async () => {
        const workspaceRoot = pickWorkspaceRoot();
        const localBundles = [
          this.context.asAbsolutePath(path.join("assets", assetName)),
          ...(workspaceRoot ? [path.join(workspaceRoot, "tools", "plugin_ws_bridge", assetName)] : []),
        ];
        const sourcePath = localBundles.find((localBundle) => {
          if (!fs.existsSync(localBundle)) {
            return false;
          }
          const candidateBytes = fs.readFileSync(localBundle);
          if (isRobloxModel(candidateBytes)) {
            return true;
          }
          this.output.appendLine(`[plugin-install] ignored invalid bundled model: ${localBundle}`);
          return false;
        });
        const cfg = this.getConfig();
        const result = await this.runCommand(
          cfg.cliPath,
          sourcePath ? ["setup", "--file", sourcePath] : ["setup"],
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
        this.output.appendLine(`[plugin-install] ${sourcePath ?? "downloaded by Renium"}`);
        void vscode.window.showInformationMessage(
          process.platform === "darwin"
            ? "Studio plugin installed. Open Renium Studio from Applications."
            : "Studio plugin installed. Restart Roblox Studio to load it.",
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
    switch (await pickMenuAction("Renium — Settings & Diagnostics", [
      menuSeparator("Project"),
      {
        label: "$(check) Diagnose Project",
        description: "Check project files, required tools, and Studio integration",
        action: "doctor",
      },
      {
        label: "$(json) Open Project Configuration",
        description: "Edit settings for the active place",
        action: "config",
      },
      menuSeparator("Help"),
      {
        label: "$(book) Show CLI Documentation",
        description: "Open the Renium command reference",
        action: "docs",
      },
      {
        label: "$(output) Show Output",
        description: "Open Renium extension logs",
        action: "output",
      },
    ])) {
      case "doctor":
        await this.runProjectDoctor();
        break;
      case "config":
        await this.openProjectConfiguration();
        break;
      case "docs":
        await this.openCliDocumentation();
        break;
      case "output":
        this.output.show(true);
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
      const command = cfg.cliPath;
      ensureFileExists(command);
      this.output.show(false);
      this.output.appendLine(`[renium] ${taskName}: ${command} ${renderCommandArgs(args)}`);
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
    const command = cfg.cliPath;
    ensureFileExists(command);
    const startedServe = !this.bridgeServeRequested;
    this.bridgeServeRequested = true;
    this.consoleFollowOwnsServe = startedServe;
    try {
      await this.automationClient.ensure(command, cfg);
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
      const command = cfg.cliPath;
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
      const value = recordValue(result.result);
      if (!value) {
        throw new Error("Console request returned an invalid result.");
      }
      const epoch = typeof value.epoch === "string" ? value.epoch : undefined;
      if (this.consoleFollowEpoch && epoch !== this.consoleFollowEpoch) {
        this.consoleFollowSeq = 0;
        this.consoleFollowFromOldest = true;
        this.consoleOutput.appendLine("[Studio console restarted]");
        drainImmediately = true;
      } else {
        const entries = Array.isArray(value.entries) ? value.entries : [];
        for (const entry of entries) {
          const row = recordValue(entry);
          if (row) {
            this.consoleOutput.appendLine(`[${String(row.type ?? row.level ?? "output")}] ${String(row.message ?? "")}`);
          }
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
      && !this.editorLiveSyncRuntimeEnabled
      && !this.liveSyncStartPromise
      && !this.liveSyncStartupInProgress
    ) {
      this.bridgeServeRequested = false;
      this.stopStudioActionPolling();
      await this.stopBridgeDaemon();
      this.updateStatusBar();
    }
  }

  private updateCli(): string {
    const bundled = bundledReniumCliPath(this.context.extensionPath);
    if (fs.existsSync(bundled)) {
      return bundled;
    }
    const configured = this.tryGetConfig()?.cliPath;
    if (configured && fs.existsSync(configured)) {
      return configured;
    }
    throw new Error("The Renium updater is not installed.");
  }

  private currentEditorCli(): string {
    const extensionRoot = path.dirname(this.context.extensionPath);
    const owner = path.basename(path.dirname(extensionRoot)).toLowerCase();
    const name = owner === ".cursor"
      ? "cursor"
      : owner === ".vscode"
        ? "code"
        : owner === ".vscode-insiders"
          ? "code-insiders"
          : owner === ".windsurf"
            ? "windsurf"
            : "";
    if (!name) {
      throw new Error(`Renium cannot identify the editor that owns ${extensionRoot}.`);
    }
    const fileNames = process.platform === "win32" ? [`${name}.cmd`, `${name}.exe`] : [name];
    const candidates = [
      ...fileNames.map((fileName) => path.join(vscode.env.appRoot, "bin", fileName)),
      ...(process.env.PATH ?? "")
        .split(path.delimiter)
        .filter(Boolean)
        .flatMap((directory) => fileNames.map((fileName) => path.join(directory, fileName))),
    ];
    const found = candidates.find((candidate) => fs.existsSync(candidate));
    if (!found) {
      throw new Error(`Renium cannot locate the ${name} command needed to update this extension.`);
    }
    return found;
  }

  private async readUpdateStatus(): Promise<ReniumUpdateStatus> {
    if (this.updateCheckPromise) {
      return this.updateCheckPromise;
    }
    this.updateCheckPromise = (async () => {
      const result = await this.runCommand(
        this.updateCli(),
        ["--output-mode", "json", "update", "check"],
        this.context.extensionPath,
        "update-check",
        2,
        { quietLog: true, timeoutMs: 30_000 },
      );
      if (result.code !== 0) {
        throw new Error(`Update check exited with code ${result.code}.`);
      }
      const status = parseCliJsonObject<ReniumUpdateStatus>(result.output);
      if (!status || status.signature !== "verified") {
        throw new Error("The release manifest signature could not be verified.");
      }
      return status;
    })();
    try {
      return await this.updateCheckPromise;
    } finally {
      this.updateCheckPromise = undefined;
    }
  }

  private async waitForDeferredUpdate(resultPath: string, version: string): Promise<void> {
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      if (fs.existsSync(resultPath)) {
        const result = JSON.parse(fs.readFileSync(resultPath, "utf8")) as {
          ok?: boolean;
          version?: string;
          error?: string;
          helper?: string;
        };
        if (result.version === version) {
          for (let attempt = 0; attempt < 20; attempt += 1) {
            try {
              if (result.helper && fs.existsSync(result.helper)) {
                fs.rmSync(result.helper);
              }
              fs.rmSync(resultPath);
              break;
            } catch {
              await new Promise((resolve) => setTimeout(resolve, 50));
            }
          }
          if (!result.ok) {
            throw new Error(result.error || `Renium ${version} could not be installed.`);
          }
          return;
        }
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`Renium ${version} did not finish installing within 30 seconds.`);
  }

  private async connectedLocalStudioCount(): Promise<number | undefined> {
    if (!this.isBridgeDaemonRunning()) {
      return 0;
    }
    const result = await this.runCommand(
      this.updateCli(),
      ["a", "studios"],
      this.context.extensionPath,
      "update-studios",
      2,
      { quietLog: true, timeoutMs: 2_000 },
    );
    if (result.code !== 0) {
      return undefined;
    }
    const response = parseCliJsonObject<{
      ok?: number;
      r?: { clients?: BridgeClientInfo[] };
    }>(result.output);
    const clients = response?.r?.clients;
    if (!Array.isArray(clients)) {
      return undefined;
    }
    return clients.filter((client) =>
      client.role === "edit"
      && (Boolean(client.localFile) || !(Number(client.gameId) > 0 && Number(client.placeId) > 0))
    ).length;
  }

  private async chooseLocalPlaceUpdateBehavior(): Promise<{
    behavior: Exclude<LocalPlaceUpdateBehavior, "ask">;
    localCount?: number;
  } | undefined> {
    const configured = vscode.workspace.getConfiguration("renium")
      .get<LocalPlaceUpdateBehavior>("localPlaceUpdateBehavior", "ask");
    const localCount = await this.connectedLocalStudioCount();
    if (localCount === 0) {
      return { behavior: "leaveOpen", localCount };
    }
    if (configured !== "ask") {
      return { behavior: configured, localCount };
    }
    const count = localCount === undefined ? "A connected local Studio place may be open" :
      `${localCount} connected local Studio place${localCount === 1 ? " is" : "s are"} open`;
    const choice = await vscode.window.showWarningMessage(
      `${count}. What should Renium do while updating the Studio plugin?`,
      "Leave Open",
      "Save and Close",
      "Terminate Without Saving",
    );
    const behavior = choice === "Leave Open" ? "leaveOpen" :
      choice === "Save and Close" ? "saveAndClose" :
        choice === "Terminate Without Saving" ? "terminate" : undefined;
    if (!behavior) {
      return undefined;
    }
    const remember = await vscode.window.showInformationMessage(
      "Use this choice automatically for future Renium updates?",
      "Always",
      "Just Once",
    );
    if (remember === "Always") {
      await vscode.workspace.getConfiguration("renium").update(
        "localPlaceUpdateBehavior",
        behavior,
        vscode.ConfigurationTarget.Global,
      );
    }
    return { behavior, localCount };
  }

  private async installUpdate(status: ReniumUpdateStatus, requestedByStudio = false): Promise<void> {
    if (this.updateInstallPromise) {
      return this.updateInstallPromise;
    }
    const version = status.latestVersion;
    if (!version || (!status.updateAvailable && !requestedByStudio)) {
      vscode.window.showInformationMessage("Renium is up to date.");
      return;
    }
    const localPlaces = await this.chooseLocalPlaceUpdateBehavior();
    if (!localPlaces) {
      return;
    }
    this.updateInstallPromise = Promise.resolve(vscode.window.withProgress({
      location: vscode.ProgressLocation.Notification,
      title: `Installing Renium ${version}`,
    }, async () => {
      await this.stopConsoleFollow();
      await this.disposeLiveSyncRuntime();
      this.bridgeServeRequested = false;
      this.stopStudioActionPolling();
      const result = await this.runCommand(
        this.updateCli(),
        [
          "--output-mode", "json",
          "update", "apply",
          "--extension-root", path.dirname(this.context.extensionPath),
          "--editor-cli", this.currentEditorCli(),
          "--local-places", localPlaces.behavior === "leaveOpen" ? "leave-open" :
            localPlaces.behavior === "saveAndClose" ? "save-and-close" : "terminate",
        ],
        this.context.extensionPath,
        "update-apply",
        2,
        { quietLog: true, timeoutMs: 5 * 60 * 1000 },
      );
      if (result.code !== 0) {
        throw new Error(`Update installation exited with code ${result.code}.`);
      }
      const applied = parseCliJsonObject<{ resultPath?: string }>(result.output);
      if (applied?.resultPath) {
        await this.waitForDeferredUpdate(applied.resultPath, version);
      }
    }));
    try {
      await this.updateInstallPromise;
    } finally {
      this.updateInstallPromise = undefined;
    }
    const studioNote = localPlaces.behavior === "leaveOpen" && localPlaces.localCount !== 0
      ? " Local Studio windows were left open; restart them when you want to load the updated plugin."
      : "";
    const choice = await vscode.window.showInformationMessage(
      `Renium ${version} is installed. Reload the editor to use it.${studioNote}`,
      "Reload Editor",
    );
    if (choice === "Reload Editor") {
      await vscode.commands.executeCommand("workbench.action.reloadWindow");
    }
  }

  private async offerUpdate(status: ReniumUpdateStatus): Promise<void> {
    if (!status.updateAvailable || !status.latestVersion) {
      return;
    }
    const choice = await vscode.window.showInformationMessage(
      `Renium ${status.latestVersion} is available.`,
      { modal: true },
      "Update",
      "Later",
    );
    if (choice === "Update") {
      await this.installUpdate(status);
    }
  }

  public scheduleAutomaticUpdateCheck(): void {
    if (this.updateCheckTimer) {
      clearTimeout(this.updateCheckTimer);
      this.updateCheckTimer = undefined;
    }
    if (!vscode.workspace.getConfiguration("renium").get<boolean>("automaticUpdateChecks", true)) {
      return;
    }
    this.updateCheckTimer = setTimeout(() => {
      this.updateCheckTimer = undefined;
      void (async () => {
        try {
          const status = await this.readUpdateStatus();
          await this.offerUpdate(status);
        } catch (error) {
          this.output.appendLine(`[renium] update check skipped: ${error instanceof Error ? error.message : String(error)}`);
        }
      })();
    }, 1500);
  }

  public async checkForUpdates(): Promise<void> {
    if (this.updateCheckTimer) {
      clearTimeout(this.updateCheckTimer);
      this.updateCheckTimer = undefined;
    }
    try {
      const status = await this.readUpdateStatus();
      if (status.updateAvailable) {
        await this.offerUpdate(status);
      } else {
        vscode.window.showInformationMessage(`Renium ${status.currentVersion ?? ""} is up to date.`.trim());
      }
    } catch (error) {
      vscode.window.showErrorMessage(`Could not check for Renium updates. ${error instanceof Error ? error.message : String(error)}`);
    }
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
      const selectedServices = normalizeServices(runCfg.services, runCfg.services);
      const command = runCfg.cliPath;
      ensureFileExists(command);
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
      this.output.appendLine(`[renium] export game file command: ${command} ${renderCommandArgs(args)}`);
      const result = await this.runCommand(command, args, runCfg.projectRoot, "export-game-file", runCfg.progressHeartbeatSeconds);
      if (result.code !== 0) {
        throw new Error(`Game file export exited with code ${result.code}`);
      }

      const parsed = parseCliJsonObject<CliExportGameFileResult>(result.output);
      const finalOutputPath = typeof parsed?.output === "string" && parsed.output.trim().length > 0
        ? parsed.output
        : outputPath;
      const instanceSummary = typeof parsed?.instances === "number" && Number.isFinite(parsed.instances)
        ? ` (${parsed.instances} instances)`
        : "";
      vscode.window.showInformationMessage(`Exported game file to ${finalOutputPath}${instanceSummary}.`);
    });
  }

  public async serve(options: { silent?: boolean; bestEffort?: boolean } = {}): Promise<void> {
    const cfg = this.getConfig();
    this.bridgeServeRequested = true;
    this.liveSyncOwnsServe = false;
    this.consoleFollowOwnsServe = false;
    try {
      await this.automationClient.ensure(cfg.cliPath, cfg);
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
    if (this.editorLiveSyncRuntimeEnabled || this.liveSyncStartPromise) {
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
    let startupFilePauseHeld = false;
    try {
      if (this.editorLiveSyncRuntimeEnabled) {
        await this.setEditorLiveSyncEnabled(true);
        const cfg = this.getConfig();
        if (this.liveSyncStopRequested) {
          await this.disposeLiveSyncRuntime();
          await this.setEditorLiveSyncEnabled(false);
          return;
        }
        let initialState: StudioChangeState | undefined;
        if (cfg.studioLiveSyncEnabled && !this.studioLiveSyncStarted) {
          if (!await this.ensureLiveSyncServeReady(cfg, options)) {
            return;
          }
          if (this.liveSyncStopRequested) {
            await this.disposeLiveSyncRuntime();
            await this.setEditorLiveSyncEnabled(false);
            return;
          }
        }
        if (
          !this.daemonFileSyncEnabled
          || this.daemonFileSyncGeneration !== this.automationClient.processGeneration()
        ) {
          initialState = await this.setDaemonFileSync(cfg, true);
        }
        if (cfg.studioLiveSyncEnabled && !this.studioLiveSyncStarted) {
          await this.startStudioLiveSyncRuntime(cfg, { ...options, initialState });
        }
        if (!options.silent) {
          vscode.window.showInformationMessage("Live sync is already running.");
        }
        return;
      }

      const cfg = this.getConfig();
      try {
        ensureFileExists(cfg.cliPath);
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

      await this.setEditorLiveSyncEnabled(true);
      if (this.liveSyncStopRequested) {
        await this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        return;
      }
      let liveCfg = this.getConfig();
      this.displayedLiveSyncPrompt = false;
      let initialState = await this.setDaemonFileSync(liveCfg, true, true);
      startupFilePauseHeld = true;
      liveCfg = this.effectiveLiveSyncConfig(liveCfg);
      if (liveCfg.initialSyncPriority === "editor" || options.graphRefresh === true) {
        const outcome = await this.runInitialEditorLiveSyncPass(srcRoot, options);
        if (outcome === "applied" && initialState) {
          initialState = await this.getStudioChangeState(
            liveCfg,
            liveCfg.services,
            studioChangeAckOptions(
              studioChangeSeq(initialState),
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
      if (this.liveSyncStopRequested) {
        await this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        return;
      }
      await this.controlDaemonFileWrites(liveCfg, "resume", []);
      startupFilePauseHeld = false;
      this.updateStatusBar();
      if (!options.silent) {
        vscode.window.showInformationMessage("Live sync started.");
      }
    } catch (err) {
      if (this.daemonFileSyncEnabled) {
        try {
          await this.setDaemonFileSync(this.getConfig(), false);
          startupFilePauseHeld = false;
        } catch {
        }
      }
      await this.disposeLiveSyncRuntime();
      await this.setEditorLiveSyncEnabled(false);
      throw err;
    } finally {
      if (startupFilePauseHeld && this.daemonFileSyncEnabled) {
        try {
          if (this.liveSyncStopRequested || !this.editorLiveSyncRuntimeEnabled) {
            await this.setDaemonFileSync(this.getConfig(), false);
          } else {
            await this.controlDaemonFileWrites(this.getConfig(), "resume", []);
          }
        } catch (error) {
          this.output.appendLine(
            `[renium] live sync startup cleanup failed: ${error instanceof Error ? error.message : String(error)}`,
          );
        }
      }
      this.liveSyncStartupInProgress = false;
      if (this.liveSyncGraphRefreshPending) {
        this.liveSyncGraphRefreshPending = false;
        const projectRoot = this.liveSyncProjectRoot ?? this.tryGetConfig()?.projectRoot;
        if (projectRoot) {
          this.scheduleLiveSyncGraphRefresh(projectRoot);
        }
      }
      if (!this.editorLiveSyncRuntimeEnabled && this.bridgeServeRequested) {
        this.scheduleStudioActionPoll();
      }
    }
  }

  private async ensureLiveSyncServeReady(
    cfg: SyncConfig,
    options: { bestEffort?: boolean } = {},
  ): Promise<boolean> {
    const startedServe = !this.bridgeServeRequested;
    this.bridgeServeRequested = true;
    if (this.consoleFollowOwnsServe) {
      this.consoleFollowOwnsServe = false;
      this.liveSyncOwnsServe = true;
    } else if (startedServe) {
      this.liveSyncOwnsServe = true;
    }
    try {
      await this.automationClient.ensure(cfg.cliPath, cfg);
      const result = await this.runAutomationOperation(
        cfg.cliPath,
        cfg,
        "live-sync-wait-for-plugin",
        AUTOMATION_OP.liveStatus,
        {
          services: normalizeServices(cfg.services, cfg.services).join(","),
          bridgeWaitSeconds: editorBridgeWaitSeconds(cfg),
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
      return "applied";
    }
    try {
      const applied = await this.pushEditorPathsNow(initialTargets.paths, {
        force: true,
        targetSettingsIds: initialTargets.targetSettingsIds,
        taskName: "Editor -> Studio initial sync",
      });
      if (!applied) {
        await this.controlDaemonFileWrites(cfg, "queue", initialTargets.paths);
      }
      return applied ? "applied" : "skipped";
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`[renium] editor live sync initial pass failed: ${message}`);
      await this.controlDaemonFileWrites(cfg, "queue", initialTargets.paths);
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
          studioChangeAckOptions(
            studioChangeSeq(initialState),
            initialState.runtimeId,
          ),
        );
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
      }
      this.scheduleStudioLiveSyncPoll(
        runtimeCfg,
        0,
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

  private isStudioLiveSyncCurrent(generation?: number): boolean {
    return generation === undefined
      || (generation === this.studioLiveSyncGeneration && this.studioLiveSyncStarted);
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

  private nextStudioLiveSyncPollDelay(cfg: SyncConfig, maxDelayMs: number): number {
    const baseDelayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    const currentDelayMs = Math.max(baseDelayMs, this.studioLiveSyncNextPollMs);
    this.studioLiveSyncNextPollMs = Math.min(
      maxDelayMs,
      Math.max(baseDelayMs, Math.ceil(currentDelayMs * STUDIO_LIVE_SYNC_POLL_BACKOFF_MULTIPLIER)),
    );
    return currentDelayMs;
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
      !this.editorLiveSyncRuntimeEnabled ||
      !cfg.studioLiveSyncEnabled
    ) {
      return;
    }
    const run = (): void => {
      this.studioLiveSyncTimer = undefined;
      if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
        return;
      }
      void this.pollStudioLiveSync(generation).catch((err) => {
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        const latestCfg = this.getConfig();
        const nextDelayMs = this.nextStudioLiveSyncPollDelay(latestCfg, MAX_STUDIO_LIVE_SYNC_ERROR_POLL_MS);
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] Studio -> editor live sync failed: ${message}`);
        this.scheduleStudioLiveSyncPoll(latestCfg, nextDelayMs, generation);
      });
    };
    if (delayMs <= 0) {
      queueMicrotask(run);
    } else {
      this.studioLiveSyncTimer = setTimeout(run, Math.max(MIN_STUDIO_LIVE_SYNC_POLL_MS, delayMs));
    }
  }

  private async pollStudioLiveSync(generation: number): Promise<void> {
    const cfg = this.getConfig();
    if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
      return;
    }
    if (!cfg.editorLiveSyncEnabled || !this.editorLiveSyncRuntimeEnabled || !cfg.studioLiveSyncEnabled) {
      await this.stopStudioLiveSyncRuntime();
      return;
    }
    if (this.studioLiveSyncInFlightGenerations.has(generation)) {
      this.scheduleStudioLiveSyncPoll(
        cfg,
        this.nextStudioLiveSyncPollDelay(cfg, MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS),
        generation,
      );
      return;
    }
    this.studioLiveSyncInFlightGenerations.add(generation);
    let nextDelayMs = this.studioLiveSyncBasePollDelayMs(cfg);
    try {
      const state = await this.getStudioChangeState(cfg, cfg.services, {
        start: false,
        waitSeconds: MAX_STUDIO_LIVE_SYNC_EVENT_WAIT_MS / 1000,
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
        ? normalizeConflictPolicy(state.conflictResolution)
        : undefined;
      const dirtyServices = Array.isArray(state.dirtyServices)
        ? normalizeReportedServices(state.dirtyServices, cfg.services)
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
      const observedSeq = studioChangeSeq(state);
      const reviewKey = JSON.stringify([
        state.runtimeId ?? "",
        observedSeq,
        (state.editorActions ?? []).map((action) => action.id ?? "").sort(),
      ]);
      if (importableServices.length > 0) {
        nextDelayMs = state.eventDriven === true ? 0 : this.resetStudioLiveSyncPollDelay(runtimeCfg);
        const ackObservedDirty = studioChangeAckOptions(observedSeq, state.runtimeId);
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
          nextDelayMs = this.nextStudioLiveSyncPollDelay(runtimeCfg, MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS);
          return;
        }
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        if (propertyImport === "fallback") {
          await this.enqueueStudioToEditorSyncIfChanged(
            importableServices,
            runtimeCfg,
            generation,
          );
        }
        if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
          return;
        }
        await this.getStudioChangeState(runtimeCfg, importableServices, ackObservedDirty);
      } else if (dirtyServices.length > 0) {
        nextDelayMs = this.nextStudioLiveSyncPollDelay(runtimeCfg, MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS);
      } else {
        nextDelayMs = state.eventDriven === true
          ? state.waitCancelled === true ? 50 : 0
          : this.nextStudioLiveSyncPollDelay(runtimeCfg, MAX_STUDIO_LIVE_SYNC_IDLE_POLL_MS);
      }
    } catch (err) {
      if (generation !== this.studioLiveSyncGeneration || !this.studioLiveSyncStarted) {
        return;
      }
      const latestCfg = this.getConfig();
      nextDelayMs = this.nextStudioLiveSyncPollDelay(latestCfg, MAX_STUDIO_LIVE_SYNC_ERROR_POLL_MS);
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
      ackRuntimeSettingsSeq?: number;
      ackActionIds?: string[];
      runtimeId?: string;
      start?: boolean;
      stop?: boolean;
      suppressSeconds?: number;
      waitSeconds?: number;
    } = {},
  ): Promise<StudioChangeState> {
    const command = cfg.cliPath;
    ensureFileExists(command);
    if (
      typeof options.ackSeq === "number"
      || typeof options.ackRuntimeSettingsSeq === "number"
      || options.ackActionIds?.length
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
        bridgeWaitSeconds: editorBridgeWaitSeconds(cfg),
        bridgePorts: cfg.bridgePorts,
        services: normalizeServices(services, cfg.services).join(","),
        reset: options.reset === true,
        replaceServices: options.replaceServices === true,
        clearPending: options.clearPending === true,
        ...(typeof options.ackSeq === "number" && Number.isFinite(options.ackSeq)
          ? { ackSeq: Math.max(0, Math.floor(options.ackSeq)) }
          : {}),
        ...(typeof options.ackRuntimeSettingsSeq === "number" && Number.isFinite(options.ackRuntimeSettingsSeq)
          ? { ackRuntimeSettingsSeq: Math.max(0, Math.floor(options.ackRuntimeSettingsSeq)) }
          : {}),
        ...(options.ackActionIds?.length ? { ackActions: options.ackActionIds.join(",") } : {}),
        ...(typeof options.runtimeId === "string" && options.runtimeId.length > 0
          ? { runtimeId: options.runtimeId }
          : {}),
        ...(typeof options.suppressSeconds === "number" && Number.isFinite(options.suppressSeconds) && options.suppressSeconds > 0
          ? { suppressSeconds: Math.max(0.05, options.suppressSeconds) }
          : {}),
        ...(useEventWait ? { eventWaitSeconds: Math.max(0.05, Math.min(25, options.waitSeconds ?? 0)) } : {}),
        manageFiles: false,
        contextBound: true,
      },
      {
        quietWait: true,
        ...(useEventWait ? { timeoutMs: Math.ceil((options.waitSeconds ?? 0) * 1000) + 3_000 } : {}),
      },
    );
    if (result.code !== 0) {
      throw new Error(`Studio change state exited with code ${result.code}`);
    }
    return this.consumeStudioChangeState(result, cfg, services);
  }

  private async consumeStudioChangeState(
    result: CommandRunResult,
    cfg: SyncConfig,
    services: string[],
  ): Promise<StudioChangeState> {
    const state = result.result && typeof result.result === "object"
      ? parseStudioChangeState(JSON.stringify(result.result))
      : parseStudioChangeState(result.output);
    if (!state) {
      throw new Error("Studio change state did not return a plugin result.");
    }
    this.updateDaemonFileSyncStatus(state.daemon, cfg);
    if (state.runtimeSettingChanges && Object.keys(state.runtimeSettingChanges).length > 0) {
      this.studioRuntimeSettings = {
        ...this.studioRuntimeSettings,
        ...state.runtimeSettingChanges,
      };
    }
    const handledActions = await this.handleStudioEditorActions(state.editorActions);
    const runtimeSettingsSeq = typeof state.runtimeSettingsSeq === "number"
      && Number.isFinite(state.runtimeSettingsSeq)
      ? Math.max(0, Math.floor(state.runtimeSettingsSeq))
      : undefined;
    if (handledActions.acknowledged.length > 0 || runtimeSettingsSeq !== undefined) {
      await this.getStudioChangeState(cfg, services, {
        start: false,
        ackRuntimeSettingsSeq: runtimeSettingsSeq,
        ackActionIds: handledActions.acknowledged,
        runtimeId: state.runtimeId,
      });
    }
    if (handledActions.updateVersion) {
      void this.readUpdateStatus()
        .then((status) => this.installUpdate(status, true))
        .catch((error) => vscode.window.showErrorMessage(
          `Could not install Renium ${handledActions.updateVersion}. ${error instanceof Error ? error.message : String(error)}`,
        ));
    }
    return state;
  }

  private async setDaemonFileSync(
    cfg: SyncConfig,
    running: boolean,
    filesPaused = false,
  ): Promise<StudioChangeState | undefined> {
    if (!running) {
      this.daemonFileSyncEnabled = false;
      this.daemonFileSyncRestartAttempts = 0;
      this.daemonPendingPaths.clear();
      this.daemonFileSyncError = undefined;
      if (this.daemonFileSyncStatusTimer) {
        clearTimeout(this.daemonFileSyncStatusTimer);
        this.daemonFileSyncStatusTimer = undefined;
      }
    }
    const result = await this.runAutomationOperation(
      cfg.cliPath,
      cfg,
      running ? "live-sync-start" : "live-sync-stop",
      running ? AUTOMATION_OP.liveStart : AUTOMATION_OP.liveStop,
      {
        services: normalizeServices(cfg.services, cfg.services).join(","),
        bridgeWaitSeconds: editorBridgeWaitSeconds(cfg),
        bridgePorts: cfg.bridgePorts,
        contextBound: true,
        manageFiles: true,
        pullChanges: false,
        filesPaused: running && filesPaused,
        resetFilesPaused: running && filesPaused,
        replaceServices: running,
      },
      { quietWait: true },
    );
    if (result.code !== 0) {
      throw new Error(result.automationError?.m ?? `Live sync exited with code ${result.code}`);
    }
    this.updateDaemonFileSyncStatus(recordValue(result.result)?.daemon, cfg);
    this.daemonFileSyncEnabled = running;
    this.daemonFileSyncGeneration = running ? this.automationClient.processGeneration() : 0;
    this.daemonFileSyncRestartAttempts = 0;
    if (this.daemonFileSyncRestartTimer) {
      clearTimeout(this.daemonFileSyncRestartTimer);
      this.daemonFileSyncRestartTimer = undefined;
    }
    if (running) {
      this.scheduleDaemonFileSyncStatusPoll();
    }
    this.updateStatusBar();
    return running ? this.consumeStudioChangeState(result, cfg, cfg.services) : undefined;
  }

  private scheduleDaemonFileSyncRestart(): void {
    if (
      this.disposed
      || !this.daemonFileSyncEnabled
      || this.liveSyncStopRequested
      || this.daemonFileSyncRestartTimer
    ) {
      return;
    }
    const delayMs = this.experienceChangeInProgress
      ? 250
      : Math.min(5000, this.daemonFileSyncRestartAttempts === 0
        ? 100
        : 250 * 2 ** Math.min(this.daemonFileSyncRestartAttempts, 5));
    this.daemonFileSyncRestartTimer = setTimeout(() => {
      this.daemonFileSyncRestartTimer = undefined;
      if (this.disposed || !this.daemonFileSyncEnabled || this.liveSyncStopRequested) {
        return;
      }
      if (this.experienceChangeInProgress) {
        this.scheduleDaemonFileSyncRestart();
        return;
      }
      const cfg = this.tryGetConfig();
      if (!cfg || !this.isEditorLiveSyncActive()) {
        this.scheduleDaemonFileSyncRestart();
        return;
      }
      void this.setDaemonFileSync(cfg, true).catch((error) => {
        this.daemonFileSyncRestartAttempts += 1;
        this.output.appendLine(
          `[renium] live sync file watcher restart failed: ${error instanceof Error ? error.message : String(error)}`,
        );
        this.scheduleDaemonFileSyncRestart();
      });
    }, delayMs);
  }

  private scheduleDaemonFileSyncStatusPoll(delayMs = 750): void {
    if (
      this.disposed
      || !this.daemonFileSyncEnabled
      || this.liveSyncStopRequested
      || this.daemonFileSyncStatusTimer
    ) {
      return;
    }
    this.daemonFileSyncStatusTimer = setTimeout(() => {
      this.daemonFileSyncStatusTimer = undefined;
      void this.pollDaemonFileSyncStatus();
    }, delayMs);
  }

  private async pollDaemonFileSyncStatus(): Promise<void> {
    if (
      this.daemonFileSyncStatusInFlight
      || this.disposed
      || !this.daemonFileSyncEnabled
      || this.liveSyncStopRequested
    ) {
      return;
    }
    if (
      !this.automationClient.isRunning()
      || this.daemonFileSyncGeneration !== this.automationClient.processGeneration()
    ) {
      this.scheduleDaemonFileSyncRestart();
      this.scheduleDaemonFileSyncStatusPoll();
      return;
    }
    const cfg = this.tryGetConfig();
    if (!cfg || this.experienceChangeInProgress) {
      this.scheduleDaemonFileSyncStatusPoll();
      return;
    }
    this.daemonFileSyncStatusInFlight = true;
    try {
      const result = await this.runAutomationOperation(
        cfg.cliPath,
        cfg,
        "live-sync-files-status",
        AUTOMATION_OP.liveStatus,
        {
          contextBound: true,
          manageFiles: true,
          filesOnly: true,
        },
        { quietWait: true },
      );
      if (result.code === 0) {
        const daemon = recordValue(recordValue(result.result)?.daemon);
        this.updateDaemonFileSyncStatus(daemon, cfg);
        if (daemon?.running !== true) {
          this.scheduleDaemonFileSyncRestart();
        }
      }
    } catch (error) {
      this.output.appendLine(
        `[renium] live sync file status failed: ${error instanceof Error ? error.message : String(error)}`,
      );
    } finally {
      this.daemonFileSyncStatusInFlight = false;
      this.scheduleDaemonFileSyncStatusPoll();
    }
  }

  private updateDaemonFileSyncStatus(value: unknown, cfg = this.tryGetConfig()): void {
    const status = recordValue(value);
    if (!status || !cfg) {
      return;
    }
    const pending = Array.isArray(status.pendingPaths)
      ? status.pendingPaths
        .filter((entry): entry is string => typeof entry === "string" && entry.length > 0)
        .map((entry) => path.isAbsolute(entry) ? path.resolve(entry) : path.resolve(cfg.projectRoot, entry))
      : [];
    this.daemonPendingPaths = new Set(pending);
    const error = typeof status.error === "string" && status.error.trim().length > 0
      ? status.error.trim()
      : undefined;
    if (error !== this.daemonFileSyncError) {
      this.daemonFileSyncError = error;
      if (error) {
        this.output.appendLine(`[renium] live sync file watcher failed: ${error}`);
      }
    }
    if (this.daemonFileSyncEnabled && status.running !== true) {
      this.scheduleDaemonFileSyncRestart();
    }
    this.updateStatusBar();
  }

  private async controlDaemonFileWrites(
    cfg: SyncConfig,
    action: "pause" | "resume" | "settle" | "queue",
    paths: string[],
  ): Promise<void> {
    if (!this.isEditorLiveSyncActive()) {
      return;
    }
    let startedPaused = false;
    if (
      !this.automationClient.isRunning()
      || this.daemonFileSyncGeneration !== this.automationClient.processGeneration()
    ) {
      startedPaused = action !== "queue";
      await this.setDaemonFileSync(cfg, true, startedPaused);
      if (action === "pause") {
        return;
      }
    }
    for (let attempt = 0; attempt < 2; attempt += 1) {
      const fileWrites = startedPaused && action === "settle" ? "resume" : action;
      const result = await this.runAutomationOperation(
        cfg.cliPath,
        cfg,
        "live-sync-files",
        AUTOMATION_OP.liveStatus,
        {
          contextBound: true,
          manageFiles: true,
          ...(fileWrites === "pause" || fileWrites === "resume" ? { fileWrites } : {}),
          ...(action === "queue" ? { queuePaths: paths } : {}),
          ...(fileWrites === "resume" || action === "settle" ? { settlePaths: paths } : {}),
        },
        { quietWait: true },
      );
      if (result.code !== 0) {
        throw new Error(result.automationError?.m ?? `Live sync file control exited with code ${result.code}`);
      }
      const daemon = recordValue(recordValue(result.result)?.daemon);
      this.updateDaemonFileSyncStatus(daemon, cfg);
      if (daemon?.running === true) {
        return;
      }
      if (attempt > 0) {
        throw new Error("Live sync file watcher did not start.");
      }
      startedPaused = action !== "queue";
      await this.setDaemonFileSync(cfg, true, startedPaused);
      if (action === "pause") {
        return;
      }
    }
  }

  private async handleStudioEditorActions(
    actions: StudioEditorAction[] | undefined,
  ): Promise<HandledStudioActions> {
    if (!Array.isArray(actions)) {
      return { acknowledged: [] };
    }
    const acknowledged: string[] = [];
    let updateVersion: string | undefined;
    const cfg = this.getConfig();
    for (const action of actions) {
      if (action?.type === "revealScript") {
        if (await this.revealStudioScript(action, cfg) && action.id) {
          acknowledged.push(action.id);
        }
        continue;
      }
      if (action?.type === "installUpdate" && action.id) {
        acknowledged.push(action.id);
        updateVersion = action.version;
      }
    }
    return { acknowledged, updateVersion };
  }

  private async revealStudioScript(action: StudioEditorAction, cfg: SyncConfig): Promise<boolean> {
    const sourcePath = this.resolveStudioSourcePathFromSourcemap(cfg, action);
    if (!sourcePath || !fs.existsSync(sourcePath)) {
      this.output.appendLine("[renium] reveal script is waiting for a matching source file.");
      return false;
    }
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(sourcePath));
    const editor = await vscode.window.showTextDocument(document, { preview: false });
    return filesystemPathKey(editor.document.uri.fsPath) ===
      filesystemPathKey(sourcePath);
  }

  private scheduleStudioActionPoll(delayMs = 750): void {
    this.stopStudioActionPolling();
    if (
      !this.bridgeServeRequested ||
      !this.isBridgeDaemonRunning() ||
      this.editorLiveSyncRuntimeEnabled ||
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
      this.editorLiveSyncRuntimeEnabled ||
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

  private logEditorChangedPaths(label: string, filePaths: string[], cfg: SyncConfig): void {
    const maxEntries = 25;
    for (const filePath of filePaths.slice(0, maxEntries)) {
      this.output.appendLine(`[renium] ${label}: ${filesystemPathKey(path.relative(cfg.projectRoot, filePath))}`);
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
    generation?: number,
  ): Promise<void> {
    const run = async (): Promise<void> => {
      if (!this.isStudioLiveSyncCurrent(generation)) {
        return;
      }
      let taskStarted = false;
      const taskName = "Studio -> Editor sync";
      try {
        const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg, { quietProbe: true });
        if (!this.isStudioLiveSyncCurrent(generation)) {
          return;
        }
        if (diff.changedServices.length === 0) {
          return;
        }
        taskStarted = true;
        this.setActiveTask(taskName);
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
    const selectedServices = normalizeServices(services, cfg.services);
    await this.getStudioChangeState(cfg, selectedServices, { start: true });
    await this.runExport({
      services: selectedServices,
      runImport: false,
      notifyOnSuccess: false,
      reason: "",
      quietLog: options.quietProbe === true,
    });
    return diffStudioSnapshots(
      selectedServices,
      this.resolveSnapshotPath(cfg),
      this.studioSnapshotFingerprintByService,
    );
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
    if (!this.isStudioLiveSyncCurrent(options.generation)) {
      return;
    }
    const selectedServices = normalizeServices(services, cfg.services);
    const capturedLocalEdits = await this.captureLocalScriptEditsForServices(
      selectedServices,
      cfg,
      options.studioAuthoritative !== true,
    );
    const changedPaths = await this.importSnapshots(
      cfg,
      this.resolveSnapshotPath(cfg),
      selectedServices,
      { quietLog: options.quietLog === true },
    );
    const stillCurrent = this.isStudioLiveSyncCurrent(options.generation);
    const affectedKeys = new Set(
      changedPaths.map((filePath) => filesystemPathKey(filePath)),
    );
    const affectedLocalEdits = new Map(
      [...capturedLocalEdits].filter(([filePath]) =>
        affectedKeys.has(filesystemPathKey(filePath))),
    );
    const survivingLocalEdits = this.reconcileLocalEditsAfterFullImport(
      changedPaths,
      cfg,
      affectedLocalEdits,
    );
    commitStudioSnapshotFingerprints(
      selectedServices,
      fingerprintsByService,
      this.studioSnapshotFingerprintByService,
    );
    if (stillCurrent && survivingLocalEdits.length > 0) {
      await this.controlDaemonFileWrites(cfg, "queue", survivingLocalEdits);
    }
    if (stillCurrent) {
      await executeCommandBestEffort("renium.fileExplorer.refreshServices", selectedServices);
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
    if (!this.isStudioLiveSyncCurrent(generation)) {
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
    const rows: ChangePreviewRow[] = propertyChanges.map((change, index) => {
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

    return this.showChangeReviewPanel(
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
    rows: ChangePreviewRow[],
    changeCount: number,
    threshold: number,
    mode: "property" | "structural",
    title: string,
    generation?: number,
    reviewKey?: string,
  ): Promise<"apply" | "full" | "discard" | "pending"> {
    if (!this.isStudioLiveSyncCurrent(generation)) {
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
    panel.webview.html = buildChangePreviewHtml(
      rows,
      changeCount,
      threshold,
      assetBase,
      mode,
      this.changePreviewIconNames ?? new Set<string>(),
    );

    return new Promise<"apply" | "full" | "discard" | "pending">((resolve) => {
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
              const cfg = this.tryGetConfig();
              if (cfg) {
                this.scheduleStudioLiveSyncPoll(
                  this.effectiveLiveSyncConfig(cfg),
                  MIN_STUDIO_LIVE_SYNC_POLL_MS,
                );
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
    if (!this.isStudioLiveSyncCurrent(generation)) {
      return "pending";
    }
    const fullSyncServices = Array.isArray(state.fullSyncServices)
      ? state.fullSyncServices.map((service) => service.trim()).filter((service) => service.length > 0)
      : [];
    const propertyChanges = Array.isArray(state.propertyChanges) ? state.propertyChanges : [];
    const trackedChanges = studioChangeLogEntries(state, dirtyServices);
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
        if (!this.isStudioLiveSyncCurrent(generation)) {
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
      if (!this.isStudioLiveSyncCurrent(generation)) {
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

    ensureFileExists(cfg.cliPath);
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
    if (!this.isStudioLiveSyncCurrent(generation)) {
      fs.rmSync(batchFile, { force: true });
      return "pending";
    }
    const resumePaths = new Set(pathsToSuppress);
    try {
      await this.noteProgrammaticEditorWrite({
        paths: Array.from(pathsToSuppress),
        fileWrites: "pause",
      });
    } catch (error) {
      try {
        await this.noteProgrammaticEditorWrite({
          paths: Array.from(pathsToSuppress),
          fileWrites: "resume",
        });
      } catch (resumeError) {
        const original = error instanceof Error ? error.message : String(error);
        const resume = resumeError instanceof Error ? resumeError.message : String(resumeError);
        throw new Error(`${original}; live sync also failed to resume: ${resume}`);
      }
      throw error;
    }
    let importFailed = false;
    try {
      const result = await this.runCommand(
        cfg.cliPath,
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
      if (result.code !== 0) {
        throw new Error(`Rust property import exited with code ${result.code}`);
      }
      const stillCurrent = this.isStudioLiveSyncCurrent(generation);
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
      for (const filePath of changedFiles) {
        resumePaths.add(filePath);
      }
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

      if (stillCurrent && changedSettingsFiles.size > 0) {
        await executeCommandBestEffort(
          "renium.fileExplorer.refreshPropertyChanges",
          Array.from(changedSettingsFiles),
        );
      }
      return stillCurrent ? "applied" : "pending";
    } catch (error) {
      importFailed = true;
      throw error;
    } finally {
      try {
        fs.rmSync(batchFile, { force: true });
      } catch {
      }
      try {
        await this.noteProgrammaticEditorWrite({
          paths: Array.from(resumePaths),
          fileWrites: "resume",
        });
      } catch (error) {
        if (!importFailed) {
          throw error;
        }
        this.output.appendLine(
          `[renium] live sync resume failed after import failure: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
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
    if (isPathInside(sourcePath, srcRoot)) {
      return path.join(cfg.projectRoot, ".renium", "sync-base", path.relative(srcRoot, sourcePath));
    }
    const owner = loadProjectSourceLocations(cfg.projectRoot)
      .filter((location) => {
        try {
          return fs.statSync(location).isFile()
            ? filesystemPathKey(sourcePath) === filesystemPathKey(location)
            : isPathInside(sourcePath, location);
        } catch {
          return path.extname(location) !== ""
            ? filesystemPathKey(sourcePath) === filesystemPathKey(location)
            : isPathInside(sourcePath, location);
        }
      })
      .sort((left, right) => right.length - left.length)[0];
    if (!owner) {
      return undefined;
    }
    const ownerKey = crypto
      .createHash("sha256")
      .update(filesystemPathKey(owner))
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
      if (!isLuaSourcePath(abs)) {
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
          writeUtf8FileIfChanged(filePath, local.content);
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
      writeUtf8FileIfChanged(filePath, resolved);
      surviving.push(filePath);
    }

    for (const filePath of affectedPaths.filter((filePath) => isLuaSourcePath(filePath))) {
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
    const selectedServices = normalizeServices(services, cfg.services);
    const selected = new Set(selectedServices.map((service) => service.toLowerCase()));
    const sourceGraph = loadProjectSourceGraph(cfg.projectRoot);
    const locations = sourceGraph.locations;
    const externalByKey = new Map<string, string>();
    for (const location of locations) {
      const ownerKey = crypto
        .createHash("sha256")
        .update(filesystemPathKey(location))
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
        && isLuaSourcePath(sourcePath)
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
    const selectedServices = normalizeServices(services, cfg.services);
    for (const service of selectedServices) {
      const serviceDir = path.join(srcRoot, service);
      if (fs.existsSync(serviceDir)) {
        for (const filePath of await this.collectInitialEditorLiveSyncPathsAsync(serviceDir)) {
          if (isLuaSourcePath(filePath)) {
            paths.add(path.resolve(filePath));
          }
        }
      }
    }
    const sourceGraph = loadProjectSourceGraph(cfg.projectRoot);
    for (const location of this.projectSourceScanLocations(selectedServices, sourceGraph)) {
      if (isPathInside(location, srcRoot)) {
        continue;
      }
      let stat: fs.Stats;
      try {
        stat = fs.statSync(location);
      } catch {
        continue;
      }
      if (stat.isFile()) {
        if (isLuaSourcePath(location)) {
          paths.add(path.resolve(location));
        }
        continue;
      }
      if (!stat.isDirectory()) {
        continue;
      }
      for (const filePath of await this.collectInitialEditorLiveSyncPathsAsync(location)) {
        if (isLuaSourcePath(filePath)) {
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
      const normalized = filesystemPathKey(location);
      const owners = sourceGraph.owners.filter((owner) =>
        filesystemPathKey(owner.location) === normalized);
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
    const matches = this.sourceOwnersForPath(filePath, sourceGraph);
    if (matches.length === 0) {
      return false;
    }
    for (const owner of matches) {
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

  private backupConflictCopy(cfg: SyncConfig, sourcePath: string, content: string, side: "local" | "studio"): string | undefined {
    try {
      const srcRoot = this.sourceRoot(cfg);
      let rel = isPathInside(sourcePath, srcRoot) ? path.relative(srcRoot, sourcePath) : undefined;
      if (rel === undefined) {
        const syncBase = this.syncBasePathForSource(cfg, sourcePath);
        if (syncBase) {
          rel = path.relative(path.join(cfg.projectRoot, ".renium", "sync-base"), syncBase);
        }
      }
      if (rel === undefined) {
        const ownerKey = crypto
          .createHash("sha256")
          .update(filesystemPathKey(sourcePath))
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

      const parsed = recordValue(JSON.parse(fs.readFileSync(sourcemapPath, "utf8")));
      if (!parsed) {
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
    const children = parent.children.filter((child): child is SourcemapNode => !!recordValue(child));
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
      .find((value) => isLuaSourcePath(value));
    if (!rawPath) {
      return undefined;
    }

    const sourcePath = path.isAbsolute(rawPath) ? path.resolve(rawPath) : path.resolve(cfg.projectRoot, rawPath);
    if (!this.isProjectSourcePath(sourcePath, cfg)) {
      return undefined;
    }
    return sourcePath;
  }

  private async collectInitialEditorLiveSyncPathsAsync(srcRoot: string): Promise<string[]> {
    const settingsPathsByDirectory = new Map<string, string>();
    const otherPaths: string[] = [];
    for (const filePath of await collectFilesRecursively(srcRoot)) {
      const fileName = path.basename(filePath);
      if (isReniumSettingsFileName(fileName)) {
        const directory = path.resolve(path.dirname(filePath));
        settingsPathsByDirectory.set(directory, filePath);
      } else {
        otherPaths.push(filePath);
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
      cfg.cliPath,
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
    const parsed = parseCliJsonObject<{ paths?: unknown; targetSettingsIds?: unknown }>(result.output);
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
    const validSettingsPaths = new Set(settingsPaths.map((settingsPath) => filesystemPathKey(settingsPath)));
    const paths = rawPaths
      .map((value) => String(value))
      .filter((value) => validSettingsPaths.has(filesystemPathKey(value)));
    paths.push(...settingsPaths.filter((value) => !isReniumSettingsFileName(path.basename(value))));
    return {
      paths: [...new Set(paths)],
      targetSettingsIds: [...new Set(rawIds.map((value) => String(value)).filter((value) => value.startsWith("editor:")))],
    };
  }

  private async partitionUnresolvedConflictMarkerPaths(
    paths: string[],
  ): Promise<{ pushable: string[]; blocked: string[] }> {
    const pushable: string[] = [];
    const blocked: string[] = [];
    for (const filePath of paths) {
      const key = filesystemPathKey(filePath);
      if (isLuaSourcePath(filePath) && await this.fileHasConflictMarkers(filePath)) {
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

  public async noteProgrammaticEditorWrite(request: ProgrammaticEditorWriteRequest): Promise<void> {
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
    if (paths.length === 0 && request.fileWrites === undefined) {
      return;
    }

    if (request.fileWrites === "pause") {
      await this.controlDaemonFileWrites(cfg, "pause", paths);
    } else if (request.fileWrites === "resume") {
      await this.controlDaemonFileWrites(cfg, "resume", paths);
    } else if (request.fileWrites === "queue") {
      await this.controlDaemonFileWrites(cfg, "queue", paths);
    } else {
      await this.controlDaemonFileWrites(cfg, "settle", paths);
    }
  }

  private async suppressStudioLiveSyncAfterEditorPush(paths: string[], cfg: SyncConfig): Promise<void> {
    if (!cfg.studioLiveSyncEnabled || !cfg.editorLiveSyncEnabled || !this.editorLiveSyncRuntimeEnabled) {
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
    const wasRunning = this.liveSyncStartPromise !== undefined || this.editorLiveSyncRuntimeEnabled;
    const startup = this.liveSyncStartPromise;
    if (wasRunning) {
      const cfg = this.getConfig();
      try {
        await this.setDaemonFileSync(cfg, false);
      } catch (err) {
        this.output.appendLine(
          `[renium] live sync stop failed: ${err instanceof Error ? err.message : String(err)}`,
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
        ? "Live sync stopped."
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
    if (this.experienceChangeInProgress) {
      void vscode.window.showWarningMessage("Wait for the place change to finish.");
      return false;
    }
    if (filesystemPathKey(projectRoot) !== filesystemPathKey(cfg.projectRoot)) {
      return false;
    }
    if (options.force === true && !this.canUseStudioPushPipeline()) {
      await this.serve({ silent: true });
    }
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
    let completed = false;
    try {
      await this.enqueue(options.taskName ?? "Editor -> Studio sync", async () => {
        const runCfg = this.getConfig();
        if (
          filesystemPathKey(projectRoot) !== filesystemPathKey(runCfg.projectRoot)
          || (!options.force && !this.isEditorLiveSyncActive())
        ) {
          this.output.appendLine("[renium] editor direct sync cancelled before apply");
          return;
        }
        if (options.force === true && !this.canUseStudioPushPipeline()) {
          this.noteStudioPushSkipped("serve/live sync is not active");
          return;
        }
        const classified = await this.partitionUnresolvedConflictMarkerPaths(changedPaths);
        const pathsToPush = classified.pushable;
        if (classified.blocked.length > 0) {
          await this.controlDaemonFileWrites(runCfg, "queue", classified.blocked);
        }
        if (pathsToPush.length === 0) {
          completed = true;
          pushed = classified.blocked.length === 0;
          return;
        }
        this.logEditorChangedPaths("Editor -> Studio", pathsToPush, runCfg);
        const outcome = await this.runEditorPush(pathsToPush, runCfg, options);
        if (outcome === "skipped") {
          await this.controlDaemonFileWrites(runCfg, "queue", pathsToPush);
        }
        completed = true;
        pushed = outcome === "applied" && classified.blocked.length === 0;
      });
    } catch (error) {
      await this.controlDaemonFileWrites(cfg, "queue", changedPaths);
      throw error;
    }
    if (!completed) {
      await this.controlDaemonFileWrites(cfg, "queue", changedPaths);
    }
    return pushed;
  }

  private prepareDirectEditorPush(
    request: EditorDirectPushRequest,
  ): EditorDirectPushContext | undefined {
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
    if (this.experienceChangeInProgress) {
      void vscode.window.showWarningMessage("Wait for the place change to finish.");
      return undefined;
    }
    if (!request.force && !this.isEditorLiveSyncActive()) {
      return undefined;
    }
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      this.noteStudioPushSkipped("serve/live sync is not active");
      return undefined;
    }
    if (
      request.projectRoot
      && filesystemPathKey(request.projectRoot) !== filesystemPathKey(cfg.projectRoot)
    ) {
      return undefined;
    }
    return { cfg, service, projectRoot, changedPaths };
  }

  private async finishDirectEditorPush(
    kind: "property" | "delete",
    result: CommandRunResult,
    context: EditorDirectPushContext,
  ): Promise<EditorPushOutcome> {
    const { cfg, changedPaths } = context;
    const retain = async (): Promise<void> => {
      await this.controlDaemonFileWrites(cfg, "queue", changedPaths);
    };
    if (result.code !== 0) {
      await retain();
      throw new Error(`Editor ${kind} push exited with code ${result.code}`);
    }
    const summary = parseEditorPushSummary(result.output, result.result);
    if (!summary) {
      await retain();
      throw new Error(`Editor ${kind} push did not return a Studio apply result.`);
    }
    if (summary.skippedByReview === true) {
      await retain();
      return "skipped";
    }
    if (summary.ok === false || summaryNumber(summary, "errors") > 0) {
      await retain();
      throw new Error(`Studio rejected or failed editor ${kind} apply.`);
    }
    if (changedPaths.length > 0) {
      await this.controlDaemonFileWrites(cfg, "settle", changedPaths);
      await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);
    }
    return "applied";
  }

  public async pushEditorPropertyNow(request: EditorPropertyPushRequest): Promise<EditorPushOutcome> {
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      await this.serve({ silent: true });
    }
    const context = this.prepareDirectEditorPush(request);
    if (!context) {
      return "skipped";
    }
    const { cfg, service, changedPaths } = context;

    const property = String(request.property ?? "").trim();
    const pathSegments = Array.isArray(request.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || !property || pathSegments.length === 0) {
      await this.controlDaemonFileWrites(cfg, "queue", changedPaths);
      throw new Error("Editor property push requires service, property, and path segments.");
    }

    const command = cfg.cliPath;
    ensureFileExists(command);
    const bridgeWaitSeconds = editorBridgeWaitSeconds(cfg);
    const settingsId = String(request.settingsId ?? "").trim();
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
    let result = await this.runAutomationOperation(
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
    return this.finishDirectEditorPush("property", result, context);
  }

  public async pushEditorDeleteNow(request: EditorDeletePushRequest): Promise<EditorPushOutcome> {
    if (request.force === true && !this.canUseStudioPushPipeline()) {
      await this.serve({ silent: true });
    }
    const context = this.prepareDirectEditorPush(request);
    if (!context) {
      return "skipped";
    }
    const { cfg, service, changedPaths } = context;

    const pathSegments = Array.isArray(request.pathSegments)
      ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
      : [];
    if (!service || pathSegments.length <= 1) {
      await this.controlDaemonFileWrites(cfg, "queue", changedPaths);
      throw new Error("Editor delete push requires service and a non-root path.");
    }

    const command = cfg.cliPath;
    ensureFileExists(command);
    const bridgeWaitSeconds = editorBridgeWaitSeconds(cfg);
    const settingsId = String(request.settingsId ?? "").trim();
    const result = await this.runAutomationOperation(
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
    );
    return this.finishDirectEditorPush("delete", result, context);
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
      this.editorLiveSyncRuntimeEnabled &&
      this.isProjectSourcePath(doc.uri.fsPath, cfg)
    ) {
      return;
    }

    if (!cfg.autoSyncOnSave) {
      return;
    }

    if (!this.isProjectSourcePath(doc.uri.fsPath, cfg)) {
      return;
    }
    this.pendingAutoPaths.add(path.resolve(doc.uri.fsPath));

    if (this.autoSyncTimer) {
      clearTimeout(this.autoSyncTimer);
    }

    this.autoSyncTimer = setTimeout(() => {
      const paths = Array.from(this.pendingAutoPaths);
      this.pendingAutoPaths.clear();

      void this.syncSavedPaths(paths);
    }, Math.max(100, cfg.autoSyncDebounceMs));
  }

  private async syncSavedPaths(paths: string[]): Promise<void> {
    try {
      if (!this.canUseStudioPushPipeline()) {
        await this.serve({ silent: true });
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] sync on save could not start the Studio connection: ${message}`);
      this.output.show(true);
      vscode.window.showErrorMessage(`Sync on save failed. ${message}`);
      return;
    }
    await this.pushEditorPathsNow(paths, {
      force: true,
      taskName: "Sync on save",
    }).catch(() => undefined);
  }

  private async runEditorPush(
    changedPaths: string[],
    cfg: SyncConfig,
    options: EditorPushOptions = {},
  ): Promise<EditorPushOutcome> {
    const command = cfg.cliPath;
    ensureFileExists(command);
    const bridgeWaitSeconds = editorBridgeWaitSeconds(cfg);
    const verifySources = options.fullSync !== true
      && (options.verifySources === true || cfg.verifyEditorPushSources);
    const changedPathArgs = options.fullSync === true
      ? []
      : changedPaths.map((changedPath) => this.editorChangedPathArg(changedPath, cfg.projectRoot));
    const targetSettingsId = typeof options.targetSettingsId === "string" ? options.targetSettingsId.trim() : "";
    const targetSettingsIds = [
      ...(targetSettingsId.length > 0 ? [targetSettingsId] : []),
      ...(Array.isArray(options.targetSettingsIds) ? options.targetSettingsIds : []),
    ]
      .map((value) => String(value).trim())
      .filter((value) => value.length > 0);
    const uniqueTargetSettingsIds = [...new Set(targetSettingsIds)];
    const targetProperties = [
      ...(typeof options.targetProperty === "string" ? [options.targetProperty] : []),
      ...(Array.isArray(options.targetProperties) ? options.targetProperties : []),
    ]
      .map((value) => String(value).trim())
      .filter((value) => value.length > 0);
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
    const result = options.fullSync === true
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
      );

    if (result.code !== 0) {
      throw new Error(result.automationError?.m ?? `Editor push exited with code ${result.code}`);
    }
    const summary = parseEditorPushSummary(result.output, result.result);
    if (!summary) {
      throw new Error("Editor push did not return a Studio apply result.");
    }
    if (summary.skippedByReview === true) {
      this.output.appendLine("[renium] editor push was skipped in Studio and remains pending");
      return "skipped";
    }
    const sourceVerified = summaryNumber(summary, "sourceVerified");
    const sourceVerifyFailed = summaryNumber(summary, "sourceVerifyFailed");
    const errors = summaryNumber(summary, "errors");
    if (errors > 0) {
      const detail = Array.isArray(summary.sourceVerifyErrors) ? ` ${summary.sourceVerifyErrors.join("; ")}` : "";
      throw new Error(`Studio rejected or failed editor Source verification.${detail}`);
    }
    if (summary.ok === false || sourceVerifyFailed > 0) {
      const detail = Array.isArray(summary.sourceVerifyErrors) ? ` ${summary.sourceVerifyErrors.join("; ")}` : "";
      throw new Error(`Studio reported a failed editor push.${detail}`);
    }
    this.refreshSyncBasesForPaths(changedPaths, cfg);
    await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);

    const existingSourceSaves = changedPaths.filter((changedPath) => isLuaSourcePath(changedPath) && fs.existsSync(changedPath)).length;
    if (verifySources && existingSourceSaves > 0 && sourceVerified < existingSourceSaves) {
      this.output.appendLine(
        `[renium] editor push verification warning: verified ${sourceVerified}/${existingSourceSaves} saved Lua source file(s).`,
      );
    }
    return "applied";
  }

  public onConfigurationChanged(event?: vscode.ConfigurationChangeEvent): Promise<void> {
    if (!event || event.affectsConfiguration("renium.automaticUpdateChecks")) {
      this.scheduleAutomaticUpdateCheck();
    }
    const apply = this.configurationChangeQueue.then(() => this.applyConfigurationChanged(event));
    this.configurationChangeQueue = apply.catch(() => undefined);
    return apply.catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      this.output.appendLine(`[renium] configuration reload failed: ${message}`);
      void vscode.window.showErrorMessage(`Could not reload Renium configuration. ${message}`);
    });
  }

  private async applyConfigurationChanged(event?: vscode.ConfigurationChangeEvent): Promise<void> {
    if (!pickWorkspaceRoot()) {
      this.liveSyncStopRequested = true;
      this.daemonFileSyncEnabled = false;
      this.daemonFileSyncError = undefined;
      if (this.daemonFileSyncRestartTimer) {
        clearTimeout(this.daemonFileSyncRestartTimer);
        this.daemonFileSyncRestartTimer = undefined;
      }
      if (this.daemonFileSyncStatusTimer) {
        clearTimeout(this.daemonFileSyncStatusTimer);
        this.daemonFileSyncStatusTimer = undefined;
      }
      await this.disposeLiveSyncRuntime();
      await this.stopBridgeDaemon();
      this.editorLiveSyncRuntimeEnabled = false;
      this.daemonPendingPaths.clear();
      this.configuredProjectRoot = undefined;
      this.liveSyncProjectRoot = undefined;
      this.projectRootConfigurationSnapshot = {};
      this.updateStatusBar();
      return;
    }
    const cfg = this.getConfig();
    const editorLiveSyncChanged = event?.affectsConfiguration("renium.editorLiveSyncEnabled") === true;
    const projectRootChanged = event?.affectsConfiguration("renium.projectRoot") === true;
    if (!this.configuredProjectRoot) {
      this.configuredProjectRoot = cfg.projectRoot;
      this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
      this.daemonPendingPaths.clear();
      this.resetProjectScopedCaches();
      await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
      await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
      this.ensureAgentInstructions(cfg.experienceRoot);
      this.packages.resumePendingSources(cfg.projectRoot, this.experienceGeneration, 1000);
    } else if (projectRootChanged) {
      const previousRoot = this.configuredProjectRoot;
      if (
        previousRoot !== undefined
        && filesystemPathKey(previousRoot) === filesystemPathKey(cfg.projectRoot)
      ) {
        this.configuredProjectRoot = cfg.projectRoot;
        this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
      } else {
        const previousConfiguration = this.projectRootConfigurationSnapshot;
        const previousState = this.captureProjectSyncState(previousRoot);
        const resumeLiveSync = !!(
          this.liveSyncStartPromise
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
          if (this.liveSyncProjectRoot !== undefined) {
            await this.disposeLiveSyncRuntime();
          }
          this.daemonPendingPaths.clear();
          if (previousRoot) {
            invalidateProjectSourceGraph(previousRoot);
          }
          invalidateProjectSourceGraph(cfg.projectRoot);
          this.configuredProjectRoot = cfg.projectRoot;
          if (resumeLiveSync) {
            this.liveSyncProjectRoot = cfg.projectRoot;
          }
          this.projectRootConfigurationSnapshot = this.captureProjectRootConfiguration();
          this.resetProjectScopedCaches();
          await this.configureLuauSourcemapForEditor(vscode.window.activeTextEditor);
          await vscode.commands.executeCommand("renium.fileExplorer.switchProject");
          explorerPrepared = false;
          this.ensureAgentInstructions(cfg.experienceRoot);
          this.packages.resumePendingSources(
            cfg.projectRoot,
            this.experienceGeneration,
            1000,
          );
        } catch (error) {
          await this.restoreProjectRootConfiguration(previousConfiguration);
          this.restoreProjectSyncState(previousState);
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
          if (previousRoot) {
            this.packages.resumePendingSources(
              previousRoot,
              this.experienceGeneration,
            );
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
      "renium.cliPath",
      "renium.projectRoot",
      "renium.bridgeWaitSeconds",
      "renium.bridgePorts",
    ].some((key) => event.affectsConfiguration(key));
    if (bridgeConfigChanged) {
      await this.stopConsoleFollow();
    }
    if (bridgeConfigChanged) {
      await this.stopBridgeDaemon();
      if (this.bridgeServeRequested) {
        void this.serve({ silent: true, bestEffort: true });
      } else if (cfg.editorLiveSyncEnabled && this.editorLiveSyncRuntimeEnabled) {
        void this.prewarmPersistentBridgeDaemon("configuration");
      }
    }
    if (!cfg.editorLiveSyncEnabled && this.editorLiveSyncRuntimeEnabled) {
      await this.disposeLiveSyncRuntime();
      if (!this.bridgeServeRequested) {
        await this.stopBridgeDaemon();
      }
    }
    if (
      (editorLiveSyncChanged || projectRootChanged) &&
      cfg.editorLiveSyncEnabled &&
      !this.editorLiveSyncRuntimeEnabled &&
      !this.liveSyncStartPromise
    ) {
      void this.startLiveSync({ silent: true, bestEffort: true });
    }
    if (cfg.editorLiveSyncEnabled && this.editorLiveSyncRuntimeEnabled && !this.liveSyncStartupInProgress) {
      if (cfg.studioLiveSyncEnabled) {
        void this.startStudioLiveSyncRuntime(cfg, { bestEffort: true });
      } else {
        await this.stopStudioLiveSyncRuntime();
      }
    }
    if (!event || event.affectsConfiguration("renium.gitSync") || event.affectsConfiguration("renium.projectRoot")) {
      void this.git.refreshView();
    }
    if (
      !event
      || event.affectsConfiguration("renium.link")
      || event.affectsConfiguration("renium.projectRoot")
    ) {
      this.packages.invalidateLinkStatusCache();
    }
    this.updateStatusBar();
  }

  public async prewarmPersistentBridgeDaemon(reason = "activation"): Promise<void> {
    const cfg = this.getConfig();
    if (!this.bridgeServeRequested && !(cfg.editorLiveSyncEnabled && this.editorLiveSyncRuntimeEnabled)) {
      return;
    }
    if (!fs.existsSync(cfg.cliPath)) {
      this.output.appendLine(
        `[renium] bridge daemon prewarm skipped (${reason}): CLI does not exist yet: ${cfg.cliPath}`,
      );
      return;
    }

    try {
      await this.automationClient.ensure(cfg.cliPath, cfg);
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
    quietLog?: boolean;
    destructive?: boolean;
    configOverrides?: Partial<Pick<SyncConfig, "modifiedDefaultBypass">>;
  }): Promise<CommandRunResult> {
    const cfg = {
      ...this.getConfig(),
      ...(options.configOverrides ?? {}),
    };
    const selectedServices = normalizeServices(options.services, cfg.services);
    const command = cfg.cliPath;
    ensureFileExists(command);

    const quietLog = options.quietLog === true;
    if (!quietLog) {
      this.output.show(false);
      this.logResolvedConfig(cfg);
      this.output.appendLine(
        `[renium] export daemon command: ${command} bd -w ${Math.max(1, cfg.bridgeWaitSeconds)} -P ${cfg.bridgePorts}`,
      );
      this.output.appendLine(`[renium] automation operation: ${options.runImport ? "pull" : "export-snapshots"}`);
    }

    const operation = options.runImport ? AUTOMATION_OP.pull : AUTOMATION_OP.exportSnapshots;
    const parameters = {
      services: selectedServices,
      snapshotDir: cfg.snapshotDir,
      bridgeWaitSeconds: editorBridgeWaitSeconds(cfg),
      bridgePorts: cfg.bridgePorts,
      performanceMode: cfg.performanceMode,
      modifiedDefaultBypass: cfg.modifiedDefaultBypass,
      chunkSize: Math.max(512, cfg.chunkSize),
      sourceWorkers: Math.max(0, cfg.sourceWorkers),
      instanceWorkers: Math.max(0, cfg.instanceWorkers),
      importWorkers: Math.max(0, cfg.importWorkers),
      adaptiveThrottle: cfg.adaptiveThrottle,
      destructive: options.destructive === true,
    };
    const result = options.destructive === true
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
      );
    if (result.code !== 0) {
      throw new Error(result.automationError?.m ?? `Export exited with code ${result.code}`);
    }

    if (options.runImport && options.notifyOnSuccess) {
      await executeCommandBestEffort("renium.fileExplorer.refreshServices", selectedServices);
    }

    if (options.notifyOnSuccess && options.reason) {
      vscode.window.showInformationMessage(`${options.reason}.`);
    }
    return result;
  }

  private runAutomationOperation(
    command: string,
    cfg: SyncConfig,
    label: string,
    op: number,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    return this.automationClient.runOperation(command, cfg, label, op, parameters, options);
  }

  private runReviewedAutomationOperation(
    command: string,
    cfg: SyncConfig,
    label: string,
    op: number,
    parameters: Record<string, unknown>,
    options: { quietWait?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    return this.automationClient.runReviewedOperation(command, cfg, label, op, parameters, options);
  }

  private isBridgeDaemonRunning(): boolean {
    return this.automationClient.isRunning();
  }

  private stopBridgeDaemon(reason?: Error): Promise<void> {
    return this.automationClient.stop(reason);
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

  private async importSnapshots(
    cfg: SyncConfig,
    snapshotPath: string,
    services: string[],
    options: { quietLog?: boolean } = {},
  ): Promise<string[]> {
    const selectedServices = normalizeServices(services, cfg.services);
    const quietLog = options.quietLog === true;
    if (!quietLog) {
      this.output.show(false);
      this.logResolvedConfig(cfg);
      this.output.appendLine("[renium] importing Studio snapshots into project files");
    }
    const result = await this.runAutomationOperation(
      cfg.cliPath,
      cfg,
      "snapshot-import",
      AUTOMATION_OP.importSnapshots,
      {
        snapshotDir: snapshotPath,
        services: selectedServices,
        noProjectWrite: true,
      },
      { quietWait: quietLog },
    );
    if (result.code !== 0) {
      throw new Error(result.automationError?.m ?? `Snapshot import exited with code ${result.code}`);
    }
    const parsed = result.result as { changedPaths?: unknown } | undefined;
    if (!parsed || !Array.isArray(parsed.changedPaths)) {
      throw new Error("Snapshot import returned invalid structured output.");
    }
    return parsed.changedPaths.map((filePath) => {
      if (typeof filePath !== "string") {
        throw new Error("Snapshot import returned a non-string changed path.");
      }
      return path.resolve(cfg.projectRoot, filePath);
    });
  }

  private resolveSnapshotPath(cfg: SyncConfig): string {
    return path.isAbsolute(cfg.snapshotDir) ? cfg.snapshotDir : path.join(cfg.projectRoot, cfg.snapshotDir);
  }

  private runCommand(
    command: string,
    args: string[],
    cwd: string,
    label: string,
    progressHeartbeatSeconds: number,
    options: { quietLog?: boolean; timeoutMs?: number } = {},
  ): Promise<CommandRunResult> {
    return new Promise<CommandRunResult>((resolve, reject) => {
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
        child = spawnTrackedProcess(command, args, cwd).child;
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
        this.output.append(prefixProcessOutput(prefix, data));
      }
    });

    child.stderr?.on("data", (data: Buffer | string) => {
      onActivity?.();
      onChunk?.(data.toString());
      if (!options.quietLog) {
        this.output.append(prefixProcessOutput(`${prefix}:err`, data));
      }
    });
  }

  public settingsStoreViewerCli(): { cliPath: string; cwd: string } | undefined {
    if (!pickWorkspaceRoot()) {
      return undefined;
    }
    const cfg = this.getConfig();
    if (!cfg.cliPath) {
      return undefined;
    }
    return { cliPath: cfg.cliPath, cwd: cfg.projectRoot };
  }

  private getConfig(): SyncConfig {
    return this.configResolver.get(this.studioRuntimeSettings);
  }

  private tryGetConfig(): SyncConfig | undefined {
    return this.configResolver.tryGet(this.studioRuntimeSettings);
  }

  private logResolvedConfig(config: SyncConfig): void {
    this.configResolver.log(config);
  }

  private getWorkspaceRoot(): string {
    return this.configResolver.workspaceRoot();
  }

  private sourceRoot(cfg: SyncConfig): string {
    return path.join(cfg.projectRoot, cfg.srcDir);
  }

  private isProjectSourcePath(
    filePath: string,
    cfg: SyncConfig,
    sourceGraph = loadProjectSourceGraph(cfg.projectRoot),
  ): boolean {
    if (sourceGraph.ignored.some((ignored) => isPathInside(filePath, ignored))) {
      return false;
    }
    return sourceGraph.files.some((location) =>
      filesystemPathKey(filePath) === filesystemPathKey(location))
      || sourceGraph.directories.some((location) => isPathInside(filePath, location));
  }

  private sourceLocationIsFile(location: string): boolean {
    try {
      return fs.statSync(location).isFile();
    } catch {
      return path.extname(location) !== "";
    }
  }

  private sourceOwnersForPath(
    filePath: string,
    sourceGraph: ProjectSourceGraph,
  ): ProjectSourceOwner[] {
    const matches = sourceGraph.owners.filter((owner) =>
      this.sourceLocationIsFile(owner.location)
        ? filesystemPathKey(filePath) === filesystemPathKey(owner.location)
        : isPathInside(filePath, owner.location));
    const specificity = matches.reduce(
      (maximum, owner) => Math.max(maximum, path.resolve(owner.location).split(path.sep).length),
      0,
    );
    return matches.filter((owner) =>
      path.resolve(owner.location).split(path.sep).length === specificity);
  }

  private servicesForProjectSourcePath(
    filePath: string,
    cfg: SyncConfig,
    sourceGraph = loadProjectSourceGraph(cfg.projectRoot),
  ): string[] {
    if (!this.isProjectSourcePath(filePath, cfg, sourceGraph)) {
      return [];
    }
    const matches = this.sourceOwnersForPath(filePath, sourceGraph);
    if (matches.length === 0) {
      return [];
    }
    const byLower = new Map(cfg.services.map((service) => [service.toLowerCase(), service]));
    const services = new Set<string>();
    let ambiguous = false;
    for (const owner of matches) {
      const fixed = owner.target[0];
      if (fixed) {
        const service = byLower.get(fixed.toLowerCase());
        if (service) {
          services.add(service);
        }
        continue;
      }
      if (this.sourceLocationIsFile(owner.location)) {
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

  private comparePathsForStableOrder(a: string, b: string): number {
    const left = filesystemPathKey(a);
    const right = filesystemPathKey(b);
    return left < right ? -1 : left > right ? 1 : 0;
  }

  private editorChangedPathArg(filePath: string, projectRoot: string): string {
    if (!isPathInside(filePath, projectRoot)) {
      return filePath;
    }
    return path.relative(projectRoot, filePath);
  }
  private updateStatusBar(): void {
    const experienceRoot = this.configuredExperienceRoot();
    const placeAlias = experienceRoot ? activeExperienceAlias(experienceRoot) : undefined;
    const statusText = placeAlias ? `Renium - ${placeAlias}` : "Renium";
    const placeTooltip = placeAlias ? ` for ${placeAlias}` : "";
    const pendingCount = this.daemonPendingPaths.size;
    const pendingText = pendingCount > 0 ? ` (${pendingCount})` : "";
    const pendingTooltip = pendingCount > 0
      ? `; ${pendingCount} editor change${pendingCount === 1 ? "" : "s"} waiting`
      : "";
    if (this.activeTaskName) {
      this.statusItem.text = `$(sync~spin) ${statusText}${pendingText}`;
      this.statusItem.tooltip = `${this.activeTaskName} in progress${placeTooltip}${pendingTooltip}`;
      return;
    }

    if (this.daemonFileSyncError) {
      this.statusItem.text = `$(error) ${statusText}${pendingText}`;
      this.statusItem.tooltip = `Live sync needs attention${placeTooltip}: ${this.daemonFileSyncError}`;
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

    if (liveSyncEnabled) {
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
      const cfg = this.tryGetConfig();
      if (!cfg || filesystemPathKey(cfg.projectRoot) !== filesystemPathKey(projectRoot)) {
        return;
      }
      invalidateProjectSourceGraph(projectRoot);
      await vscode.commands.executeCommand("renium.fileExplorer.refreshProjectGraph");
      if (this.liveSyncStartupInProgress) {
        this.liveSyncGraphRefreshPending = true;
        return;
      }
      if (this.editorLiveSyncRuntimeEnabled) {
        this.scheduleStudioLiveSyncPoll(cfg, this.resetStudioLiveSyncPollDelay(cfg));
      }
    } finally {
      this.liveSyncGraphRefreshRunning = false;
      if (this.liveSyncGraphRefreshPending && !this.liveSyncStartupInProgress) {
        this.liveSyncGraphRefreshPending = false;
        this.scheduleLiveSyncGraphRefresh(projectRoot);
      }
    }
  }

  public onProjectGraphChanged(projectRoot: string): void {
    const cfg = this.getConfig();
    if (filesystemPathKey(projectRoot) !== filesystemPathKey(cfg.projectRoot)) {
      return;
    }
    invalidateProjectSourceGraph(projectRoot);
    this.packages.invalidateLinkStatusCache();
    if (this.editorLiveSyncRuntimeEnabled && !this.liveSyncGraphRefreshRunning) {
      this.scheduleLiveSyncGraphRefresh(projectRoot);
    }
  }

  private pendingStudioImportServiceSet(cfg: SyncConfig): Set<string> {
    const services = new Set<string>();
    for (const filePath of this.daemonPendingPaths) {
      const detected = this.servicesForProjectSourcePath(filePath, cfg);
      for (const service of detected.length > 0 ? detected : cfg.services) {
        services.add(service.toLowerCase());
      }
    }
    return services;
  }

  public async shutdown(): Promise<void> {
    await this.disposeLiveSyncRuntime();
    await this.stopConsoleFollow({ releaseServe: false });
    await this.stopBridgeDaemon();
    await terminateAllProcesses();
    this.dispose();
  }

  public async discardPendingEditorChanges(): Promise<void> {
    const count = this.daemonPendingPaths.size;
    await this.runDaemonPendingOperation(AUTOMATION_OP.discardPending);
    vscode.window.showInformationMessage(
      count === 1
        ? "Discarded one pending editor change."
        : `Discarded ${count} pending editor changes.`,
    );
  }

  public async retryPendingEditorChanges(): Promise<void> {
    if (this.daemonPendingPaths.size === 0) {
      vscode.window.showInformationMessage("No editor changes are pending.");
      return;
    }
    if (!this.editorLiveSyncRuntimeEnabled) {
      await this.startLiveSync({ silent: true });
    }
    await this.runDaemonPendingOperation(AUTOMATION_OP.retryPending);
  }

  private async runDaemonPendingOperation(operation: number): Promise<void> {
    const cfg = this.getConfig();
    const result = await this.runAutomationOperation(
      cfg.cliPath,
      cfg,
      operation === AUTOMATION_OP.retryPending ? "live-sync-retry" : "live-sync-discard",
      operation,
      {
        bridgeWaitSeconds: editorBridgeWaitSeconds(cfg),
        bridgePorts: cfg.bridgePorts,
        contextBound: true,
        manageFiles: true,
      },
      { quietWait: true },
    );
    if (result.code !== 0) {
      throw new Error(result.automationError?.m ?? "Could not update pending live-sync changes.");
    }
    this.updateDaemonFileSyncStatus(recordValue(result.result)?.daemon, cfg);
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
  controller.scheduleAutomaticUpdateCheck();
  void (async () => {
    const cli = controller.settingsStoreViewerCli();
    const extensionVersion = String(context.extension.packageJSON.version ?? "");
    if (cli && extensionVersion && fs.existsSync(cli.cliPath)) {
      const cliVersion = await controller.detectedCliVersion(cli.cliPath);
      if (cliVersion && cliVersion !== extensionVersion) {
        void vscode.window.showWarningMessage(
            `This extension is v${extensionVersion} but ${CLI_BINARY} is v${cliVersion}. Update whichever is older so they match — syncing may misbehave until then.`,
        );
      }
    }
  })().catch(() => undefined);
  const fileExplorerController = new FileExplorerController(context, controller.git.actions());
  activeFileExplorerController = fileExplorerController;
  const linkDecorationProvider = new LinkDecorationProvider(controller.packages);
  let packagesProvider: PackagesTreeProvider;
  const packageScriptProvider = new PackageScriptContentProvider((linkId, nodeKey) =>
    packagesProvider.packageScriptSourceFor(linkId, nodeKey),
  );
  const packageScriptDecorationProvider = new PackageScriptDecorationProvider();
  packagesProvider = new PackagesTreeProvider(
    controller.packages,
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
      await executeCommandBestEffort("list.clear");
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

  const resolveViewerCli = () => controller.settingsStoreViewerCli();
  context.subscriptions.push(
    controller,
    fileExplorerController,
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      void controller.configureLuauSourcemapForEditor(editor);
    }),
    vscode.window.registerCustomEditorProvider(
      SettingsStoreEditorProvider.viewType,
      new SettingsStoreEditorProvider(
        context.extensionUri,
        resolveViewerCli,
        (node) => fileExplorerController.showSettingsStorePropertiesReadonly(node),
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
    vscode.commands.registerCommand("renium.gitSync", () => controller.git.openGitSync()),
    vscode.commands.registerCommand("renium.gitSync.status", () => controller.git.gitStatus()),
    vscode.commands.registerCommand("renium.gitSync.fetch", () => controller.git.gitFetch()),
    vscode.commands.registerCommand("renium.gitSync.pull", () => controller.git.gitPull()),
    vscode.commands.registerCommand("renium.gitSync.commitAndPush", () => controller.git.gitCommitAndPush()),
    vscode.commands.registerCommand("renium.gitSync.pullFromStudioAndPush", () => controller.git.gitCommitAndPush({ pullFromStudioFirst: true })),
    vscode.commands.registerCommand("renium.gitSync.connectRepo", () => controller.git.gitConnectRepo()),
    vscode.commands.registerCommand("renium.gitSync.publishBranch", () => controller.git.gitPublishBranch()),
    vscode.commands.registerCommand("renium.gitSync.createBranch", () => controller.git.gitCreateBranch()),
    vscode.commands.registerCommand("renium.gitSync.checkoutBranch", () => controller.git.gitCheckoutBranch()),
    vscode.commands.registerCommand("renium.gitSync.openRemote", () => controller.git.gitOpenRemote()),
    vscode.commands.registerCommand("renium.pullFromStudio", () => controller.pullFromStudio()),
    vscode.commands.registerCommand("renium.pushToStudio", () => controller.pushToStudio()),
    vscode.commands.registerCommand("renium.exportSnapshots", () => controller.exportSnapshotsOnly()),
    vscode.commands.registerCommand("renium.exportGameFile", () => controller.exportGameFile()),
    vscode.commands.registerCommand("renium.syncWallyPackages", () => controller.packages.syncWallyPackages()),
    vscode.commands.registerCommand("renium.link.apply", () => controller.packages.linkApply()),
    vscode.commands.registerCommand("renium.link.add", () => controller.packages.addLinkInteractive()),
    vscode.commands.registerCommand("renium.link.status", () => controller.packages.showLinkStatus()),
    vscode.commands.registerCommand("renium.link.break", (uri?: vscode.Uri) => controller.packages.breakLinkForFile(uri)),
    vscode.commands.registerCommand("renium.link.revealSource", (uri?: vscode.Uri) => controller.packages.revealLinkSourceForFile(uri)),
    vscode.commands.registerCommand("renium.link.addFromFile", (uri?: vscode.Uri) => controller.packages.addLinkFromFile(uri)),
    vscode.commands.registerCommand("renium.link.packInstance", (request: { service?: string; pathSegments?: string[]; id?: string; resave?: boolean }) =>
      controller.packages.packInstanceLink(request),
    ),
    vscode.commands.registerCommand("renium.link.resavePackage", (request: { service?: string; pathSegments?: string[] }) =>
      controller.packages.resavePackageLink(request),
    ),
    vscode.commands.registerCommand("renium.link.relinkPackage", (request: { service?: string; pathSegments?: string[] }) =>
      controller.packages.relinkPackageTarget(request),
    ),
    vscode.commands.registerCommand("renium.link.breakInstance", (request: { service?: string; pathSegments?: string[]; silent?: boolean; refreshExplorer?: boolean }) =>
      controller.packages.breakInstanceLink(request),
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
      controller.packages.insertPackageAtPath(request ?? {}),
    ),
    vscode.commands.registerCommand("renium.packages.viewUses", (link?: PackageTreeElement | CliLinkStatusLink | string) =>
      controller.packages.viewPackageUses(packagesProvider.linkFromElement(link)),
    ),
    vscode.commands.registerCommand("renium.packages.delete", (link?: PackageTreeElement | CliLinkStatusLink | string) =>
      controller.packages.deletePackage(packagesProvider.linkFromElement(link)),
    ),
    vscode.commands.registerCommand("renium.packages.openLinkedScriptPreview", (request?: LinkedPackageScriptPreviewRequest) =>
      packagesProvider.openLinkedScriptPreview(request),
    ),
    vscode.window.tabGroups.onDidChangeTabs(() => {
      persistOpenPackageScriptTabs();
    }),
    controller.packages.onLinksChanged(() => {
      refreshLinkManifestWatcher();
      void controller.packages.refreshLinkPackageSourceWatchers().catch(() => undefined);
      void linkDecorationProvider.refresh();
      void controller.packages.pushLinkStateToExplorer();
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
        void controller.onConfigurationChanged(event);
      }
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      void controller.onConfigurationChanged().then(() => {
        if (controller.settingsStoreViewerCli()) {
          controller.packages.scheduleStartupLinkRefresh();
        }
      });
    }),
  );

  let linkManifestWatcherPath = "";
  let linkManifestWatcherGeneration = -1;
  let linkManifestDisposables: vscode.Disposable[] = [];
  let linkApplyTimer: NodeJS.Timeout | undefined;
  const sameLinkManifestPath = (left: string, right: string): boolean => {
    return filesystemPathKey(left) === filesystemPathKey(right);
  };
  const onLinkManifestChanged = (uri: vscode.Uri): void => {
    let active: { filePath: string; autoApply: boolean; projectRoot: string; generation: number };
    try {
      active = controller.packages.activeLinkManifest();
    } catch {
      return;
    }
    if (uri.scheme !== "file" || !sameLinkManifestPath(uri.fsPath, active.filePath)) {
      return;
    }
    controller.packages.invalidateLinkStatusCache();
    if (active.autoApply) {
      if (linkApplyTimer) {
        clearTimeout(linkApplyTimer);
      }
      const captured = active;
      linkApplyTimer = setTimeout(() => {
        linkApplyTimer = undefined;
        void controller.packages.linkApply({
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
      active = controller.packages.activeLinkManifest();
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

  if (controller.settingsStoreViewerCli()) {
    void linkDecorationProvider.refresh();
    void controller.packages.pushLinkStateToExplorer();
    void controller.packages.refreshLinkPackageSourceWatchers().catch(() => undefined);
    controller.packages.scheduleStartupLinkRefresh();
  }
  setTimeout(() => {
    void restoreOpenPackageScriptTabs().catch(() => undefined);
  }, 500);

  const cfg = vscode.workspace.getConfiguration("renium");
  if (cfg.get<boolean>("editorLiveSyncEnabled", false) === true) {
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
