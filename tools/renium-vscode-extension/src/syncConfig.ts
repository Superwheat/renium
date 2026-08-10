import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import { resolveReniumCliPath } from "./cliResolution";
import { normalizeConflictPolicy, type ConflictPolicy } from "./conflictMerge";
import {
  activeExperienceAlias,
  resolveActiveExperiencePlace,
  setActiveExperiencePlace,
  type ExperiencePlace,
} from "./experience";
import type { GitSyncConfig } from "./gitController";
import type { LinkSyncConfig, WallySyncConfig } from "./packageSyncController";
import { DEFAULT_SYNC_SERVICES } from "./serviceDefaults";
import {
  loadProjectSourceRoot,
  loadSharedConfig,
  sharedConfigValue,
  type SharedConfig,
} from "./sharedConfig";
import { pickWorkspaceRoot, resolveConfigPath } from "./utils";

const DEFAULT_BRIDGE_PORTS = [8781, 8782];
const DEFAULT_CHUNK_SIZE = 4 * 1024 * 1024;
const MAX_BRIDGE_CHUNK_SIZE = 8 * 1024 * 1024;

export const DEFAULT_STUDIO_LIVE_SYNC_POLL_MS = 250;
export const MIN_STUDIO_LIVE_SYNC_POLL_MS = 10;

export type ReniumLogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

export type SyncConfig = {
  cliPath: string;
  experienceRoot: string;
  projectRoot: string;
  srcDir: string;
  activePlaceAlias?: string;
  activePlace?: ExperiencePlace;
  placeSelector?: string;
  snapshotDir: string;
  services: string[];
  sourceWorkers: number;
  instanceWorkers: number;
  importWorkers: number;
  chunkSize: number;
  bridgeWaitSeconds: number;
  bridgePorts: string;
  verifyEditorPushSources: boolean;
  adaptiveThrottle: boolean;
  autoSyncOnSave: boolean;
  autoSyncDebounceMs: number;
  editorLiveSyncEnabled: boolean;
  studioLiveSyncEnabled: boolean;
  studioLiveSyncPollMs: number;
  initialSyncPriority: "studio" | "editor" | "none";
  changesThreshold: number;
  diffLinesLimit: number;
  displayPrompts: "always" | "initial" | "never";
  logLevel: ReniumLogLevel;
  overridePackages: boolean;
  conflictResolution: ConflictPolicy;
  runImport: boolean;
  importMode: "direct" | "snapshot";
  performanceMode: "throughput" | "balanced" | "smooth";
  modifiedDefaultBypass: boolean;
  progressHeartbeatSeconds: number;
  gitSync: GitSyncConfig;
  wallySync: WallySyncConfig;
  linkSync: LinkSyncConfig;
};

type ConfigNumberOptions = { min?: number; integer?: boolean };
type StudioPolicy = "ask" | "always" | "never";

function studioPolicy(value: string): StudioPolicy {
  return value === "always" || value === "never" ? value : "ask";
}

function existingCliPath(
  workspaceRoot: string,
  projectRoot: string,
  configuredPath: string,
  extensionRoot: string,
): string {
  const roots = [...new Set([workspaceRoot, projectRoot].map((value) => path.normalize(value)))];
  return resolveReniumCliPath({ configuredPath, extensionRoot, roots });
}

export class SyncConfigResolver {
  private warnedMultiRootWorkspace = false;
  private warnedBridgePortLimit = false;
  private warnedChunkSizeCap = false;
  private sharedConfig: SharedConfig = {};

  public constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly output: vscode.OutputChannel,
    private readonly restoreActivePlace: (experienceRoot: string) => void,
  ) {}

  public get(studioRuntimeSettings?: Record<string, unknown>): SyncConfig {
    const root = this.workspaceRoot();
    const workspaceConfig = vscode.workspace.getConfiguration("renium", vscode.Uri.file(root));
    const preliminaryShared = loadSharedConfig(root, root);
    const projectRootSetting = this.mergedValue(
      workspaceConfig,
      preliminaryShared,
      "projectRoot",
      "${workspaceFolder}",
    );
    const experienceRoot = resolveConfigPath(projectRootSetting, root);
    if (!activeExperienceAlias(experienceRoot)) {
      this.restoreActivePlace(experienceRoot);
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
      this.mergedValue(workspaceConfig, shared, key, defaultValue);
    const number = (
      key: string,
      defaultValue: number,
      options: ConfigNumberOptions = {},
    ): number => this.normalizedNumber(read<unknown>(key, defaultValue), defaultValue, options);
    const boolean = (key: string, defaultValue: boolean): boolean => {
      const value = read<unknown>(key, defaultValue);
      return typeof value === "boolean" ? value : defaultValue;
    };

    const servicesRaw = read<string[]>("services", [...DEFAULT_SYNC_SERVICES]);
    const services = (Array.isArray(servicesRaw) ? servicesRaw : DEFAULT_SYNC_SERVICES)
      .map((service) => String(service).trim())
      .filter(Boolean);
    const configuredCliPathRaw = read("cliPath", "").trim();
    const configuredCliPath = configuredCliPathRaw
      ? resolveConfigPath(configuredCliPathRaw, root)
      : "";
    const gitStagePathsRaw = read<string[]>("gitSync.stagePaths", []);
    const gitStagePaths = (Array.isArray(gitStagePathsRaw) ? gitStagePathsRaw : [])
      .map((value) => String(value).trim())
      .filter(Boolean);
    const importMode = read<string>("importMode", "direct") === "snapshot" ? "snapshot" : "direct";
    const performanceModeRaw = read<string>("performanceMode", "throughput");
    const performanceMode = performanceModeRaw === "smooth" || performanceModeRaw === "balanced"
      ? performanceModeRaw
      : "throughput";
    const initialSyncPriorityRaw = read<string>("liveSync.initialSyncPriority", "studio");
    const initialSyncPriority = initialSyncPriorityRaw === "editor" || initialSyncPriorityRaw === "none"
      ? initialSyncPriorityRaw
      : "studio";
    const displayPromptsRaw = read<string>("liveSync.displayPrompts", "always");
    const displayPrompts = displayPromptsRaw === "initial" || displayPromptsRaw === "never"
      ? displayPromptsRaw
      : "always";
    const gitStageMode = read<string>("gitSync.stageMode", "tracked") === "configuredPaths"
      ? "configuredPaths"
      : "tracked";
    const gitOutputBehaviorRaw = read<string>("gitSync.outputBehavior", "onStart");
    const gitOutputBehavior = gitOutputBehaviorRaw === "silent" || gitOutputBehaviorRaw === "onError"
      ? gitOutputBehaviorRaw
      : "onStart";

    return {
      cliPath: existingCliPath(root, projectRoot, configuredCliPath, this.context.extensionPath),
      experienceRoot,
      projectRoot,
      srcDir,
      activePlaceAlias: activePlace?.alias,
      activePlace: activePlace?.place,
      placeSelector: activePlace?.selector,
      snapshotDir: read("snapshotDir", ".renium/snapshots"),
      services: services.length > 0 ? services : [...DEFAULT_SYNC_SERVICES],
      sourceWorkers: number("sourceWorkers", 0, { min: 0, integer: true }),
      instanceWorkers: number("instanceWorkers", 0, { min: 0, integer: true }),
      importWorkers: number("importWorkers", 0, { min: 0, integer: true }),
      chunkSize: this.normalizedChunkSize(read("chunkSize", DEFAULT_CHUNK_SIZE)),
      bridgeWaitSeconds: number("bridgeWaitSeconds", 8, { min: 1 }),
      bridgePorts: this.normalizedBridgePorts(String(read("bridgePorts", DEFAULT_BRIDGE_PORTS.join(",")))),
      verifyEditorPushSources: boolean("verifyEditorPushSources", false),
      adaptiveThrottle: boolean("adaptiveThrottle", true),
      autoSyncOnSave: boolean("autoSyncOnSave", false),
      autoSyncDebounceMs: number("autoSyncDebounceMs", 800, { min: 100, integer: true }),
      editorLiveSyncEnabled: boolean("editorLiveSyncEnabled", false),
      studioLiveSyncEnabled: boolean("studioLiveSyncEnabled", true),
      studioLiveSyncPollMs: number(
        "studioLiveSyncPollMs",
        DEFAULT_STUDIO_LIVE_SYNC_POLL_MS,
        { min: MIN_STUDIO_LIVE_SYNC_POLL_MS, integer: true },
      ),
      initialSyncPriority,
      changesThreshold: number("liveSync.changesThreshold", 5, { min: 0, integer: true }),
      diffLinesLimit: number("liveSync.diffLinesLimit", 3000, { min: 100, integer: true }),
      displayPrompts,
      logLevel: this.configuredLogLevel(studioRuntimeSettings),
      overridePackages: boolean("liveSync.overridePackages", false),
      conflictResolution: normalizeConflictPolicy(read("liveSync.conflictResolution", "prompt")),
      runImport: boolean("runImport", true),
      importMode,
      performanceMode,
      modifiedDefaultBypass: boolean("modifiedDefaultBypass", false),
      progressHeartbeatSeconds: number("progressHeartbeatSeconds", 2, { min: 2 }),
      gitSync: {
        gitPath: read("gitSync.gitPath", "git"),
        remote: read("gitSync.remote", "origin"),
        branch: read("gitSync.branch", ""),
        autoFetch: boolean("gitSync.autoFetch", true),
        pullFromStudioBeforePush: studioPolicy(read("gitSync.pullFromStudioBeforePush", "ask")),
        stageMode: gitStageMode,
        stagePaths: gitStagePaths.length > 0 ? gitStagePaths : [srcDir],
        includeUntracked: boolean("gitSync.includeUntracked", false),
        commitMessageTemplate: read("gitSync.commitMessageTemplate", "Renium sync: ${date}"),
        confirmBeforePush: boolean("gitSync.confirmBeforePush", true),
        requireCleanWorktreeBeforePull: boolean("gitSync.requireCleanWorktreeBeforePull", true),
        applyPulledChangesToStudio: studioPolicy(read("gitSync.applyPulledChangesToStudio", "ask")),
        timeoutSeconds: number("gitSync.timeoutSeconds", 120, { min: 10 }),
        outputBehavior: gitOutputBehavior,
      },
      wallySync: {
        wallyPath: read("wallySync.wallyPath", "wally"),
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
        runInstall: boolean("wallySync.runInstall", true),
        applyToStudio: studioPolicy(read("wallySync.applyToStudio", "ask")),
      },
      linkSync: {
        manifest: read("link.manifest", "renium-link.json"),
        folder: read("link.folder", "").trim(),
        cacheDir: read("link.cacheDir", "").trim(),
        gitPath: read("link.gitPath", "git"),
        wallyPath: read("wallySync.wallyPath", "wally"),
        offline: boolean("link.offline", false),
        autoApply: boolean("link.autoApplyOnManifestChange", false),
        applyToStudio: studioPolicy(read("link.applyToStudio", "ask")),
      },
    };
  }

  public tryGet(studioRuntimeSettings?: Record<string, unknown>): SyncConfig | undefined {
    try {
      return this.get(studioRuntimeSettings);
    } catch {
      return undefined;
    }
  }

  public workspaceRoot(): string {
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

  public configuredLogLevel(studioRuntimeSettings?: Record<string, unknown>): ReniumLogLevel {
    const runtimeLevel = studioRuntimeSettings?.logLevel;
    const workspaceConfig = vscode.workspace.getConfiguration("renium");
    const raw = String(
      typeof runtimeLevel === "string"
        ? runtimeLevel
        : this.explicitValue<string>(workspaceConfig, "logLevel")
          ?? sharedConfigValue<string>(this.sharedConfig, "logLevel")
          ?? "info",
    ).toLowerCase();
    return raw === "off"
      || raw === "error"
      || raw === "warn"
      || raw === "debug"
      || raw === "trace"
      ? raw
      : "info";
  }

  public log(config: SyncConfig): void {
    const workspaceConfig = vscode.workspace.getConfiguration("renium");
    const extensionVersion = String(this.context.extension.packageJSON.version ?? "unknown");
    const extensionEntryPath = path.join(this.context.extensionPath, "out", "extension.js");
    const extensionBuildUnix = fs.existsSync(extensionEntryPath)
      ? Math.floor(fs.statSync(extensionEntryPath).mtimeMs / 1000)
      : 0;
    this.output.appendLine(`[renium] extension version=${extensionVersion}`);
    this.output.appendLine(`[renium] extension build_unix=${extensionBuildUnix}`);
    this.output.appendLine(`[renium] config: cliPath=${config.cliPath}`);
    const values: Array<[string, unknown]> = [
      ["chunkSize", config.chunkSize],
      ["bridgePorts", config.bridgePorts],
      ["sourceWorkers", config.sourceWorkers],
      ["instanceWorkers", config.instanceWorkers],
      ["importWorkers", config.importWorkers],
      ["importMode", config.importMode],
      ["performanceMode", config.performanceMode],
      ["modifiedDefaultBypass", config.modifiedDefaultBypass],
    ];
    for (const [key, value] of values) {
      const raw = key === "chunkSize"
        ? `, raw=${String(this.configuredValue(workspaceConfig, key))}`
        : "";
      this.output.appendLine(
        `[renium] config: ${key}=${String(value)} (origin=${this.configOrigin(workspaceConfig, key)}${raw})`,
      );
    }
  }

  private explicitValue<T>(config: vscode.WorkspaceConfiguration, key: string): T | undefined {
    const inspected = config.inspect<T>(key);
    return inspected?.workspaceFolderValue ?? inspected?.workspaceValue ?? inspected?.globalValue;
  }

  private mergedValue<T>(
    config: vscode.WorkspaceConfiguration,
    shared: SharedConfig,
    key: string,
    defaultValue: T,
  ): T {
    return this.explicitValue<T>(config, key) ?? sharedConfigValue<T>(shared, key) ?? defaultValue;
  }

  private normalizedChunkSize(value: unknown): number {
    const raw = Number(value ?? DEFAULT_CHUNK_SIZE);
    if (!Number.isFinite(raw) || raw < 512) {
      return DEFAULT_CHUNK_SIZE;
    }
    const normalized = Math.floor(raw);
    if (normalized <= MAX_BRIDGE_CHUNK_SIZE) {
      return normalized;
    }
    if (!this.warnedChunkSizeCap) {
      this.warnedChunkSizeCap = true;
      this.output.appendLine(
        `[renium] config: chunkSize ${normalized} exceeds the ${MAX_BRIDGE_CHUNK_SIZE}-byte bridge transport limit; using ${MAX_BRIDGE_CHUNK_SIZE} for this run.`,
      );
    }
    return MAX_BRIDGE_CHUNK_SIZE;
  }

  private normalizedNumber(value: unknown, defaultValue: number, options: ConfigNumberOptions): number {
    const raw = Number(value ?? defaultValue);
    const normalized = Number.isFinite(raw) ? raw : defaultValue;
    const integer = options.integer ? Math.floor(normalized) : normalized;
    return options.min === undefined ? integer : Math.max(options.min, integer);
  }

  private normalizedBridgePorts(raw: string): string {
    const parsed = raw
      .split(",")
      .map((token) => Number(token.trim()))
      .filter((value) => Number.isInteger(value) && value > 0 && value <= 65535)
      .filter((value, index, values) => values.indexOf(value) === index);
    if (parsed.length === DEFAULT_BRIDGE_PORTS.length) {
      return parsed.join(",");
    }
    if (!this.warnedBridgePortLimit) {
      this.warnedBridgePortLimit = true;
      this.output.appendLine(
        `[renium] config: exactly ${DEFAULT_BRIDGE_PORTS.length} bridge ports are required; using ${DEFAULT_BRIDGE_PORTS.join(",")}.`,
      );
    }
    return DEFAULT_BRIDGE_PORTS.join(",");
  }

  private configOrigin(config: vscode.WorkspaceConfiguration, key: string): string {
    const inspected = config.inspect(key);
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
    return inspected?.defaultValue !== undefined ? "default" : "unset";
  }

  private configuredValue(config: vscode.WorkspaceConfiguration, key: string): unknown {
    const inspected = config.inspect(key);
    return inspected?.workspaceFolderValue
      ?? inspected?.workspaceValue
      ?? inspected?.globalValue
      ?? sharedConfigValue(this.sharedConfig, key)
      ?? inspected?.defaultValue;
  }
}
