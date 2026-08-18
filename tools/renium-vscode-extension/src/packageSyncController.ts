import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import type { CommandRunResult } from "./automationClient";
import { logPackageDragDebug } from "./fileExplorer";
import { renderCommandArgs } from "./gitSync";
import {
  type CliLinkStatusLink,
  type CliLinkStatusTarget,
  type LinkFileInfo,
  type PackagePreviewData,
  type PackagePreviewNode,
} from "./packagesView";
import {
  compactCommandOutput,
  ensureFileExists,
  filesystemPathKey,
  isPathInside,
  samePathSegments,
  tabInputUris,
} from "./utils";
import { inferProjectScriptIdentity } from "./sharedConfig";
import { parseCliJsonObject } from "./studioSyncProtocol";

export type WallySyncConfig = {
  wallyPath: string;
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

export type LinkSyncConfig = {
  manifest: string;
  folder: string;
  cacheDir: string;
  gitPath: string;
  wallyPath: string;
  offline: boolean;
  autoApply: boolean;
  applyToStudio: "ask" | "always" | "never";
};

type PackageSyncControllerConfig = {
  cliPath: string;
  projectRoot: string;
  srcDir: string;
  services: string[];
  progressHeartbeatSeconds: number;
  wallySync: WallySyncConfig;
  linkSync: LinkSyncConfig;
};

export type PendingPackageSource = {
  projectRoot: string;
  generation: number;
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
  processedTargets?: number;
  differenceCount?: number;
  changedPaths?: string[];
  changedSettingsIds?: string[];
  warnings?: string[];
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

type PackagePushOptions = {
  force?: boolean;
  skipChangeFilter?: boolean;
  taskName?: string;
  targetSettingsIds?: string[];
};

type PackageDeleteRequest = {
  force?: boolean;
  service?: string;
  settingsId?: string;
  className?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
};

type PackageSyncControllerHost<TConfig extends PackageSyncControllerConfig> = {
  output: vscode.OutputChannel;
  getConfig: () => TConfig;
  tryGetConfig: () => TConfig | undefined;
  enqueue: (taskName: string, task: () => Promise<void>) => Promise<void>;
  experienceChanging: () => boolean;
  experienceGeneration: () => number;
  logResolvedConfig: (config: TConfig) => void;
  runCommand: (
    command: string,
    args: string[],
    cwd: string,
    label: string,
    progressHeartbeatSeconds: number,
    options?: { quietLog?: boolean; timeoutMs?: number },
  ) => Promise<CommandRunResult>;
  canUseStudioPushPipeline: () => boolean;
  noteStudioPushSkipped: (reason: string) => void;
  pushEditorPathsNow: (paths: string[], options?: PackagePushOptions) => Promise<boolean>;
  pushEditorDeleteNow: (request: PackageDeleteRequest) => Promise<unknown>;
  noteProgrammaticEditorWrite: (request: {
    paths?: string[] | string;
    durationMs?: number;
  }) => void;
  isEditorLiveSyncActive: () => boolean;
  executeCommandBestEffort: (command: string, ...args: unknown[]) => Promise<void>;
};

export class PackageSyncController<TConfig extends PackageSyncControllerConfig> {
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
  private readonly pendingLinkPackageSourcePaths = new Map<string, PendingPackageSource>();
  private linkPackageSourceWatchers: vscode.Disposable[] = [];
  private readonly activeLinkPackageSourceKeys = new Set<string>();
  private readonly linkChangeEmitter = new vscode.EventEmitter<void>();
  public readonly onLinksChanged = this.linkChangeEmitter.event;

  public constructor(private readonly host: PackageSyncControllerHost<TConfig>) {}

  public dispose(): void {
    this.pausePendingSources();
    for (const watcher of this.linkPackageSourceWatchers) {
      watcher.dispose();
    }
    this.linkPackageSourceWatchers = [];
    this.activeLinkPackageSourceKeys.clear();
    this.linkChangeEmitter.dispose();
  }

  public pendingSourceEntries(): Array<[string, PendingPackageSource]> {
    return [...this.pendingLinkPackageSourcePaths]
      .map(([filePath, pending]): [string, PendingPackageSource] => [filePath, { ...pending }]);
  }

  public pausePendingSources(): void {
    if (this.linkPackageSourceApplyTimer) {
      clearTimeout(this.linkPackageSourceApplyTimer);
      this.linkPackageSourceApplyTimer = undefined;
    }
  }

  public restorePendingSources(
    entries: Array<[string, PendingPackageSource]>,
    projectRoot: string | undefined,
    generation: number,
  ): void {
    this.pendingLinkPackageSourcePaths.clear();
    for (const [filePath, pending] of entries) {
      this.pendingLinkPackageSourcePaths.set(filePath, {
        projectRoot: projectRoot ?? pending.projectRoot,
        generation,
      });
    }
  }

  public resetStatusCache(): void {
    this.linkStatusCache = undefined;
    this.linkStatusInflight = undefined;
  }

  public notifyLinksChanged(): void {
    this.linkChangeEmitter.fire();
  }

  public resumePendingSources(projectRoot: string, generation: number, delayMs = 500): boolean {
    let pendingSource = false;
    for (const pending of this.pendingLinkPackageSourcePaths.values()) {
      if (filesystemPathKey(pending.projectRoot) === filesystemPathKey(projectRoot)) {
        pending.generation = generation;
        pendingSource = true;
      }
    }
    if (pendingSource) {
      this.scheduleLinkPackageSourceFlush(projectRoot, generation, delayMs);
    }
    return pendingSource;
  }

  private sourceRoot(config: TConfig): string {
    return path.join(config.projectRoot, config.srcDir);
  }

  private async refreshFileExplorerSafe(): Promise<void> {
    await this.host.executeCommandBestEffort("renium.fileExplorer.refresh");
  }

  private async refreshFileExplorerServicesSafe(services: string[]): Promise<void> {
    try {
      await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", services);
    } catch {
      await this.refreshFileExplorerSafe();
    }
  }

  public async syncWallyPackages(): Promise<void> {
    this.host.output.appendLine(`[renium] Wally packages: requested at ${new Date().toISOString()}`);
    const requestedConfig = this.host.getConfig();
    const requestedRoot = filesystemPathKey(requestedConfig.projectRoot);
    const requestedGeneration = this.host.experienceGeneration();
    await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: "Syncing Wally packages",
        cancellable: false,
      },
      async (progress) => {
        progress.report({ message: "Waiting for Renium task queue..." });
        await this.host.enqueue("Sync Wally packages", async () => {
          const runCfg = this.host.getConfig();
          if (
            this.host.experienceGeneration() !== requestedGeneration
            || filesystemPathKey(runCfg.projectRoot) !== requestedRoot
          ) {
            throw new Error("The active Renium place changed. Run Wally package sync again.");
          }
          if (!(await this.ensureWallyManifest(runCfg))) {
            return;
          }
          const command = runCfg.cliPath;
          ensureFileExists(command);
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
            "--details",
          ];
          if (!runCfg.wallySync.runInstall) {
            args.push("--skip-install");
          }

          progress.report({ message: "Running wally install and bytecode import..." });
          this.host.output.show(false);
          this.host.logResolvedConfig(runCfg);
          this.host.output.appendLine(`[renium] Wally packages command: ${command} ${renderCommandArgs(args)}`);
          const result = await this.host.runCommand(command, args, runCfg.projectRoot, "wally-packages", runCfg.progressHeartbeatSeconds);
          if (result.code !== 0) {
            throw new Error(this.wallySyncFailureMessage(result));
          }

          const parsed = parseCliJsonObject<CliSyncWallyPackagesResult>(result.output);
          if (!parsed || parsed.ok === false) {
            throw new Error("Wally package sync didn't finish. Check the Renium output panel for details.");
          }
          if (
            typeof parsed.projectRoot !== "string"
            || filesystemPathKey(parsed.projectRoot) !== requestedRoot
            || this.host.experienceGeneration() !== requestedGeneration
            || filesystemPathKey(this.host.getConfig().projectRoot) !== requestedRoot
          ) {
            throw new Error("Wally package sync returned results for a different Renium place.");
          }
          const importedIds = Array.isArray(parsed.targetSettingsIds) ? parsed.targetSettingsIds : parsed.settingsIds;
          const importedCount = Array.isArray(importedIds) ? importedIds.length : 0;
          progress.report({ message: `Imported ${importedCount} package instance(s).` });
          this.host.output.appendLine(
            `[renium] Wally packages: imported ${importedCount} instance(s) into ${parsed.service ?? runCfg.wallySync.targetService}.${parsed.targetName ?? runCfg.wallySync.targetName}`,
          );
          await this.applyWallyPackagesToStudio(parsed, runCfg);
        });
      },
    );
  }

  private wallySyncFailureMessage(result: CommandRunResult): string {
    const detail = compactCommandOutput(result.output, 10, 1200);
    const hint = this.wallySyncFailureHint(result.output);
    const suffix = detail.length > 0 ? ` Details: ${detail}` : " Open the Renium output panel for details.";
    return `Couldn't sync Wally packages.${hint ? ` ${hint}` : ""}${suffix}`;
  }

  private wallySyncFailureHint(output: string): string {
    const lower = output.toLowerCase();
    if (lower.includes("failed to launch wally") || lower.includes("could not find command wally") || lower.includes("program not found")) {
      return "Wally was not found. Install Wally or set renium.wallySync.wallyPath.";
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

  private async ensureWallyManifest(cfg: TConfig): Promise<boolean> {
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
      this.host.output.appendLine(`[renium] Wally packages: created ${manifestPath}`);
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

  private async applyWallyPackagesToStudio(result: CliSyncWallyPackagesResult, cfg: TConfig): Promise<void> {
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
    if (!this.host.canUseStudioPushPipeline()) {
      this.host.noteStudioPushSkipped("serve/live sync is not active");
      vscode.window.showInformationMessage(`Synced Wally packages locally (${summaryTarget}). Start Serve or live sync before applying to Studio.`);
      return;
    }

    try {
      for (const removed of removedTargets) {
        await this.host.pushEditorDeleteNow({
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
        const pushed = await this.host.pushEditorPathsNow(changedPaths, {
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
      this.host.output.appendLine(`[renium] Wally packages Studio apply failed: ${message}`);
      this.host.output.show(true);
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

  private linkManifestPath(cfg: TConfig): string {
    return path.isAbsolute(cfg.linkSync.manifest)
      ? cfg.linkSync.manifest
      : path.join(cfg.projectRoot, cfg.linkSync.manifest);
  }

  private linkCommandArgs(cfg: TConfig, command: string, includeSource = true): string[] {
    const args = [command, "-r", cfg.projectRoot];
    if (includeSource) {
      args.push("-d", cfg.srcDir);
    }
    args.push("--manifest", cfg.linkSync.manifest);
    return args;
  }

  public activeLinkManifest(): {
    filePath: string;
    autoApply: boolean;
    projectRoot: string;
    generation: number;
  } {
    const config = this.host.getConfig();
    return {
      filePath: path.normalize(this.linkManifestPath(config)),
      autoApply: config.linkSync.autoApply,
      projectRoot: config.projectRoot,
      generation: this.host.experienceGeneration(),
    };
  }

  public async linkApply(options: { silent?: boolean; refreshExplorer?: boolean; forceStudio?: boolean; forceTargets?: boolean; forceTargetPaths?: string[][]; taskName?: string; linkId?: string; skipStudio?: boolean; expectedProjectRoot?: string; expectedGeneration?: number } = {}): Promise<CliLinkApplyResult | undefined> {
    let result: CliLinkApplyResult | undefined;
    let executed = false;
    await this.host.enqueue("Apply packages", async () => {
      const cfg = this.host.getConfig();
      if (
        (options.expectedGeneration !== undefined && options.expectedGeneration !== this.host.experienceGeneration())
        || (
          options.expectedProjectRoot
          && filesystemPathKey(options.expectedProjectRoot) !== filesystemPathKey(cfg.projectRoot)
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
      const command = cfg.cliPath;
      ensureFileExists(command);
      const args = this.linkCommandArgs(cfg, "link-apply");
      args.push(
        "--git-path",
        cfg.linkSync.gitPath,
        "--wally-path",
        cfg.linkSync.wallyPath,
      );
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
          args.push("--force-target", JSON.stringify({
            service: targetPath[0],
            path: targetPath,
            ords: [],
          }));
        }
      }
      const run = await this.host.runCommand(command, args, cfg.projectRoot, "link-apply", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't apply packages. Check the Renium output panel for details.");
      }
      const parsed = parseCliJsonObject<CliLinkApplyResult>(run.output);
      if (!parsed || parsed.ok === false) {
        throw new Error("Applying packages didn't finish. Check the Renium output panel for details.");
      }
      result = parsed;
      for (const warning of Array.isArray(parsed.warnings) ? parsed.warnings : []) {
        this.host.output.appendLine(`[renium] link warning: ${warning}`);
      }
      const processed = parsed.processedTargets ?? 0;
      if (!options.silent) {
        const warnCount = Array.isArray(parsed.warnings) ? parsed.warnings.length : 0;
        vscode.window.showInformationMessage(
          `Synced ${processed} link target(s)${warnCount > 0 ? `, ${warnCount} warning(s)` : ""}.`,
        );
      }
    });
    if ((options.expectedProjectRoot || options.expectedGeneration !== undefined) && !executed) {
      throw new Error("Package apply was cancelled before it started.");
    }
    this.invalidateLinkStatusCache();
    const forceStudioAllowed = options.forceStudio === true && this.host.canUseStudioPushPipeline();
    if (options.forceStudio === true && !forceStudioAllowed) {
      this.host.noteStudioPushSkipped("serve/live sync is not active");
    }
    if (result && options.skipStudio !== true && (forceStudioAllowed || (this.host.isEditorLiveSyncActive() && this.host.getConfig().linkSync.applyToStudio !== "never"))) {
      const changedPaths = (Array.isArray(result.changedPaths) ? result.changedPaths : [])
        .filter((filePath): filePath is string => typeof filePath === "string" && filePath.length > 0);
      if (changedPaths.length > 0) {
        this.host.noteProgrammaticEditorWrite({ paths: changedPaths, durationMs: 5000 });
        if (forceStudioAllowed) {
          const targetSettingsIds = (Array.isArray(result.changedSettingsIds) ? result.changedSettingsIds : [])
            .map((value) => String(value).trim())
            .filter((value) => value.length > 0);
          await this.host.pushEditorPathsNow(changedPaths, {
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
    const mode = this.host.getConfig().linkSync.applyToStudio;
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
      const targetSettingsIds = (Array.isArray(result.changedSettingsIds) ? result.changedSettingsIds : [])
        .map((value) => String(value).trim())
        .filter((value) => value.length > 0);
      await this.host.pushEditorPathsNow(changedPaths, {
        force: true,
        skipChangeFilter: true,
        taskName: "Link -> Studio",
        targetSettingsIds,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.host.output.appendLine(`[renium] link Studio push failed: ${message}`);
      if (!options.silent) {
        vscode.window.showWarningMessage(`Link applied to the project files, but the Studio push failed. ${message}`);
      }
    }
  }

  public async breakLink(service: string, pathSegments: string[], options: { silent?: boolean; refreshExplorer?: boolean } = {}): Promise<void> {
    await this.host.enqueue("Break link", async () => {
      const cfg = this.host.getConfig();
      const command = cfg.cliPath;
      ensureFileExists(command);
      const args = this.linkCommandArgs(cfg, "link-break");
      args.push(
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      );
      if (cfg.linkSync.cacheDir.length > 0) {
        args.push("--cache-dir", cfg.linkSync.cacheDir);
      }
      const run = await this.host.runCommand(command, args, cfg.projectRoot, "link-break", cfg.progressHeartbeatSeconds, { quietLog: true });
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
    const cfg = this.host.getConfig();
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
      source = isPathInside(abs, cfg.projectRoot)
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

    await this.host.enqueue("Add link", async () => {
      await this.runLinkAdd({
        sourceType: sourceType.value,
        source,
        sourceRef,
        subpath,
        service,
        pathSegments,
        failure: "Couldn't add the link. Check the Renium output panel for details.",
      });
    });
    const syncNow = await vscode.window.showInformationMessage("Link added. Apply it now?", "Sync now", "Later");
    if (syncNow === "Sync now") {
      await this.linkApply();
    } else {
      this.invalidateLinkStatusCache();
      await this.refreshFileExplorerSafe();
    }
  }

  private async runLinkAdd(request: {
    id?: string;
    sourceType?: string;
    source?: string;
    sourceRef?: string;
    subpath?: string;
    service: string;
    pathSegments: string[];
    failure: string;
  }): Promise<void> {
    const cfg = this.host.getConfig();
    const args = this.linkCommandArgs(cfg, "link-add", false);
    const add = (flag: string, value: string | undefined): void => {
      if (value) {
        args.push(flag, value);
      }
    };
    add("--id", request.id);
    add("--source-type", request.sourceType);
    add("--source", request.source);
    add("--ref", request.sourceRef);
    add("--subpath", request.subpath);
    args.push("--service", request.service, "--path", JSON.stringify(request.pathSegments));
    ensureFileExists(cfg.cliPath);
    const run = await this.host.runCommand(
      cfg.cliPath,
      args,
      cfg.projectRoot,
      "link-add",
      cfg.progressHeartbeatSeconds,
      { quietLog: true },
    );
    if (run.code !== 0) {
      throw new Error(request.failure);
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
    await this.host.executeCommandBestEffort("renium.fileExplorer.setLinkState", keys);
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

  private linkPackageFolderPath(cfg: TConfig): string {
    const folder = cfg.linkSync.folder || "links";
    return path.isAbsolute(folder)
      ? path.normalize(folder)
      : path.normalize(path.join(cfg.projectRoot, folder));
  }

  private isManagedPackagePath(cfg: TConfig, candidate: string): boolean {
    return isPathInside(candidate, this.linkPackageFolderPath(cfg))
      || isPathInside(candidate, this.globalPackagesDir());
  }

  private absoluteLinkSourcePath(cfg: TConfig, sourcePath: string | undefined): string | undefined {
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
    if (this.host.experienceChanging() || uri.scheme !== "file") {
      return;
    }
    const cfg = this.host.getConfig();
    const ext = path.extname(uri.fsPath).toLowerCase();
    if (ext !== ".renium" || !this.activeLinkPackageSourceKeys.has(filesystemPathKey(uri.fsPath))) {
      return;
    }
    const generation = this.host.experienceGeneration();
    this.pendingLinkPackageSourcePaths.set(path.normalize(uri.fsPath), {
      projectRoot: cfg.projectRoot,
      generation,
    });
    this.scheduleLinkPackageSourceFlush(cfg.projectRoot, generation);
  }

  public async refreshLinkPackageSourceWatchers(): Promise<void> {
    const cfg = this.host.getConfig();
    const generation = this.host.experienceGeneration();
    const resolution = await this.resolveLinkStatus(cfg);
    if (resolution.kind === "failed") {
      throw new Error(resolution.error);
    }
    if (generation !== this.host.experienceGeneration()) {
      return;
    }
    const links = resolution.kind === "success" ? resolution.value.links ?? [] : [];
    const sources = Array.from(new Set(links
      .map((link) => this.absoluteLinkSourcePath(cfg, link.sourcePath))
      .filter((sourcePath): sourcePath is string =>
        typeof sourcePath === "string" && sourcePath.toLowerCase().endsWith(".renium"))
      .map(path.normalize)));
    for (const watcher of this.linkPackageSourceWatchers) {
      watcher.dispose();
    }
    this.linkPackageSourceWatchers = [];
    this.activeLinkPackageSourceKeys.clear();
    for (const sourcePath of sources) {
      this.activeLinkPackageSourceKeys.add(filesystemPathKey(sourcePath));
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
        this.host.output.appendLine(`[renium] package source auto-apply failed: ${error instanceof Error ? error.message : String(error)}`);
      });
    }, delayMs);
  }

  private async flushLinkPackageSourceChanges(projectRoot: string, generation: number): Promise<void> {
    if (generation !== this.host.experienceGeneration() || this.host.experienceChanging()) {
      return;
    }
    const cfg = this.host.getConfig();
    if (filesystemPathKey(projectRoot) !== filesystemPathKey(cfg.projectRoot)) {
      return;
    }
    const changedPaths = [...this.pendingLinkPackageSourcePaths]
      .filter(([, pending]) =>
        pending.generation === generation
        && filesystemPathKey(pending.projectRoot) === filesystemPathKey(projectRoot))
      .map(([filePath]) => filePath);
    if (changedPaths.length === 0) {
      return;
    }
    const changedKeys = new Set(changedPaths.map((filePath) => filesystemPathKey(filePath)));
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
      if (!id || !sourcePath || !changedKeys.has(filesystemPathKey(sourcePath))) {
        continue;
      }
      if (link.isPackage !== true && !sourcePath.toLowerCase().endsWith(".renium")) {
        continue;
      }
      if (Number(link.activeTargetCount ?? link.targetCount ?? 0) <= 0) {
        continue;
      }
      const key = filesystemPathKey(sourcePath);
      const ids = linkIdsByPath.get(key) ?? new Set<string>();
      ids.add(id);
      linkIdsByPath.set(key, ids);
    }
    let appliedAny = false;
    let failed = false;
    const linkIds = new Set(Array.from(linkIdsByPath.values()).flatMap((ids) => [...ids]));
    this.host.output.appendLine(`[renium] package source changed: applying ${linkIds.size} active link package(s).`);
    for (const changedPath of changedPaths) {
      const ids = linkIdsByPath.get(filesystemPathKey(changedPath)) ?? new Set<string>();
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
        this.host.output.appendLine(
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
        this.host.output.appendLine(`[renium] startup link refresh failed: ${error instanceof Error ? error.message : String(error)}`);
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
    const cfg = this.host.getConfig();
    const srcRoot = this.sourceRoot(cfg);
    if (!isPathInside(uri.fsPath, srcRoot)) {
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
    const cfg = this.host.getConfig();
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
    await this.host.enqueue("Create package", async () => {
      const cfg = this.host.getConfig();
      const command = cfg.cliPath;
      ensureFileExists(command);
      const args = this.linkCommandArgs(cfg, "link-pack");
      args.push(
        "--service",
        service,
        "--path",
        JSON.stringify(pathSegments),
      );
      if (cfg.linkSync.folder) {
        args.push("--link-folder", cfg.linkSync.folder);
      }
      if (requestedLinkId.length > 0) {
        args.push("--id", requestedLinkId);
      }
      const run = await this.host.runCommand(command, args, cfg.projectRoot, "link-pack", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't save the package. Check the Renium output panel for details.");
      }
      packed = parseCliJsonObject<{ id?: string; source?: string }>(run.output) ?? undefined;
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
    const status = await this.getLinkStatus(true);
    const target = this.findLinkTarget(status?.targets, service, pathSegments, true);
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
    const cfg = this.host.getConfig();
    const sourcePath = this.absoluteLinkSourcePath(cfg, link.sourcePath);
    if (!sourcePath || !this.isManagedPackagePath(cfg, sourcePath) || !sourcePath.toLowerCase().endsWith(".renium")) {
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
    const status = await this.getLinkStatus(true);
    const target = this.findLinkTarget(status?.targets, service, pathSegments, false);
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
    await this.host.enqueue("Relink package", async () => {
      await this.runLinkAdd({
        id: linkId,
        service,
        pathSegments,
        failure: "Couldn't relink the package. Check the Renium output panel for details.",
      });
    });
    this.invalidateLinkStatusCache();
    await this.linkApply({ silent: true, linkId, skipStudio: true });
    await this.refreshFileExplorerSafe();
    vscode.window.showInformationMessage(`Relinked ${this.linkTargetDisplay(service, pathSegments)}.`);
  }

  private async addPackageMirror(linkId: string, source: string): Promise<void> {
    const cfg = this.host.getConfig();
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
    await this.host.enqueue("Add link mirror", async () => {
      await this.runLinkAdd({
        id: linkId,
        sourceType: "local",
        source,
        service,
        pathSegments,
        failure: "Couldn't add the mirror. Check the Renium output panel for details.",
      });
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
    this.host.output.appendLine(
      `[renium] link-status: ${status.linkCount ?? 0} link(s), ${targets.length} target(s), ${status.driftedTargets ?? 0} drifted, ${status.brokenTargets ?? 0} broken.`,
    );
    for (const target of targets) {
      const flags = [
        target.broken ? "broken" : target.readOnly ? "read-only" : "writable",
        target.drift ? "drifted" : undefined,
        target.resolved === false ? `unresolved(${target.reason ?? "?"})` : undefined,
      ].filter(Boolean).join(", ");
      this.host.output.appendLine(`  ${target.service}.${(target.path ?? []).join(".")} [${target.linkId}] ${flags}`);
    }
    this.host.output.show(true);
    vscode.window.showInformationMessage(
      `Renium links: ${status.linkCount ?? 0} link(s), ${targets.length} target(s), ${status.driftedTargets ?? 0} drifted, ${status.brokenTargets ?? 0} broken.`,
    );
  }

  public async getLinkPackages(force = false): Promise<CliLinkStatusLink[]> {
    const status = await this.getLinkStatus(force);
    const links = (Array.isArray(status?.links) ? status!.links! : [])
      .filter((link) => link.isPackage === true || String(link.sourcePath ?? "").toLowerCase().endsWith(".renium"));
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

  private findLinkTarget(
    targets: CliLinkStatusTarget[] | undefined,
    service: string,
    pathSegments: string[],
    requireResolved: boolean,
  ): CliLinkStatusTarget | undefined {
    const targetPath = this.normalizeLinkTargetSegments(service, pathSegments);
    return (targets ?? []).find((candidate) =>
      (!requireResolved || (candidate.missing !== true && candidate.resolved !== false))
      && String(candidate.service ?? "") === service
      && samePathSegments(
        this.normalizeLinkTargetSegments(
          service,
          Array.isArray(candidate.path) ? candidate.path : [],
        ),
        targetPath,
      ));
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
      if (targetPath.length === 0 || !samePathSegments(targetPath.slice(0, -1), parent)) {
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
    await this.host.enqueue("Insert link", async () => {
      await this.runLinkAdd({
        id: linkId,
        service,
        pathSegments,
        failure: "Couldn't insert the package. Check the Renium output panel for details.",
      });
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
      `packages.insertAtPath: link-apply ok changed=${Array.isArray(applyResult?.changedPaths) ? applyResult!.changedPaths!.length : 0} targets=${Array.isArray(applyResult?.changedSettingsIds) ? applyResult!.changedSettingsIds!.length : 0}`,
    );
    void this.pushLinkStateToExplorer().catch(() => undefined);
    await this.refreshFileExplorerServicesSafe([service]);
    logPackageDragDebug(`packages.insertAtPath: complete link=${linkId} target=${targetLabel}`);
    const normalizedTarget = this.normalizeLinkTargetSegments(service, pathSegments);
    const leaf = normalizedTarget.length > 0 ? normalizedTarget[normalizedTarget.length - 1] : "";
    vscode.window.showInformationMessage(`Inserted "${name}" at ${service}.${leaf}.`);
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
        if (tabInputUris(tab.input, "file").some((uri) => pathKeys.has(this.normalizeLinkPathKey(uri.fsPath)))) {
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
    await this.host.enqueue("Delete link package", async () => {
      const cfg = this.host.getConfig();
      const command = cfg.cliPath;
      ensureFileExists(command);
      const args = this.linkCommandArgs(cfg, "link-delete-package");
      args.push(
        "--id",
        fresh.id ?? "",
        "--action",
        action,
      );
      const run = await this.host.runCommand(command, args, cfg.projectRoot, "link-delete-package", cfg.progressHeartbeatSeconds, { quietLog: true });
      if (run.code !== 0) {
        throw new Error("Couldn't delete the package. Check the Renium output panel for details.");
      }
      const parsed = parseCliJsonObject<CliLinkDeletePackageResult>(run.output);
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
    if (changedPaths.length > 0 && this.host.isEditorLiveSyncActive()) {
      this.host.noteProgrammaticEditorWrite({ paths: changedPaths, durationMs: 5000 });
      await this.host.pushEditorPathsNow(changedPaths, {
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

  public async loadPackagePreview(link: CliLinkStatusLink): Promise<PackagePreviewData> {
    if (!link.id || !link.sourcePath) {
      throw new Error("Package link is missing an id or source path.");
    }
    const cfg = this.host.getConfig();
    const command = cfg.cliPath;
    ensureFileExists(command);
    const args = [
      "bytecode-explorer-batch",
      "-f",
      link.sourcePath,
      "-j",
      JSON.stringify([{ type: "service", fields: "brief,parentId,tree,properties,attributes" }]),
      "-o",
      "full",
    ];
    const run = await this.host.runCommand(command, args, cfg.projectRoot, "package-preview", cfg.progressHeartbeatSeconds, { quietLog: true });
    if (run.code !== 0) {
      throw new Error("Couldn't preview the package. Check the Renium output panel for details.");
    }
    const parsed = parseCliJsonObject<{
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

  private async resolveLinkStatus(cfg: TConfig): Promise<LinkStatusResolution> {
    const manifestPath = this.linkManifestPath(cfg);
    if (!fs.existsSync(manifestPath)) {
      return { kind: "missing" };
    }
    try {
      const command = cfg.cliPath;
      if (!fs.existsSync(command)) {
        return { kind: "failed", error: `Renium CLI not found: ${command}` };
      }
      const args = this.linkCommandArgs(cfg, "link-status");
      if (cfg.linkSync.cacheDir.length > 0) {
        args.push("--cache-dir", cfg.linkSync.cacheDir);
      }
      const run = await this.host.runCommand(
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
      const value = parseCliJsonObject<CliLinkStatusResult>(run.output);
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
    const cfg = this.host.getConfig();
    const projectRoot = filesystemPathKey(cfg.projectRoot);
    const generation = this.host.experienceGeneration();
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
    cfg: TConfig,
    projectRoot: string,
    generation: number,
    token: number,
  ): Promise<CliLinkStatusResult | undefined> {
    const resolution = await this.resolveLinkStatus(cfg);
    const value = resolution.kind === "success" ? resolution.value : undefined;
    if (resolution.kind === "failed") {
      this.host.output.appendLine(`[renium] link-status failed: ${resolution.error}`);
    }
    const currentConfig = this.host.tryGetConfig();
    if (
      this.linkStatusToken === token
      && this.host.experienceGeneration() === generation
      && currentConfig !== undefined
      && filesystemPathKey(currentConfig.projectRoot) === projectRoot
    ) {
      this.linkStatusCache = { at: now, projectRoot, generation, token, value };
    }
    return value;
  }

}
