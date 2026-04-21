import * as childProcess from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

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
  adaptiveThrottle: boolean;
  noUpdateEditorIcons: boolean;
  autoSyncOnSave: boolean;
  autoSyncDebounceMs: number;
  runImport: boolean;
  importMode: "direct" | "snapshot";
  watchConfigPath: string;
  wsWaitSeconds: number;
  progressHeartbeatSeconds: number;
};

const DEFAULT_SERVICES = [
  "Workspace",
  "Players",
  "Lighting",
  "MaterialService",
  "ReplicatedFirst",
  "ReplicatedStorage",
  "ServerScriptService",
  "ServerStorage",
  "StarterGui",
  "StarterPack",
  "StarterPlayer",
];

const DEFAULT_BRIDGE_PORTS = [8781, 8782, 8783, 8784];
const LEGACY_BRIDGE_PORTS = [8781, 8782, 8783, 8784, 8785, 8786, 8787, 8788];

class RobloxSyncController {
  private readonly output = vscode.window.createOutputChannel("Roblox MCP Sync");
  private readonly statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 200);
  private queue: Promise<void> = Promise.resolve();
  private liveSyncProcess: childProcess.ChildProcessWithoutNullStreams | undefined;
  private liveSyncStopping = false;
  private autoSyncTimer: NodeJS.Timeout | undefined;
  private pendingAutoServices = new Set<string>();
  private activeTaskName: string | undefined;
  private activeTaskStartedAt = 0;
  private activeTaskTicker: NodeJS.Timeout | undefined;
  private warnedLegacyStartupWaitSeconds = false;
  private warnedLegacyBridgePorts = false;
  private warnedBridgePortLimit = false;

  public constructor(private readonly context: vscode.ExtensionContext) {
    this.statusItem.command = "robloxSync.openMenu";
    this.statusItem.show();
    this.updateStatusBar();
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

    if (this.liveSyncProcess && !this.liveSyncProcess.killed) {
      this.liveSyncProcess.kill();
      this.liveSyncProcess = undefined;
    }

    this.statusItem.dispose();
    this.output.dispose();
  }

  public async openMenu(): Promise<void> {
    const cfg = this.getConfig();
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
        label: "$(file-code) Sync Active Service Now",
        description: "Fast service-targeted sync",
        action: "activeService",
      },
      {
        label: "$(arrow-down) Import Snapshots Into src",
        description: "Uses native Rust importer",
        action: "importOnly",
      },
      {
        label: "$(circle-slash) Live Sync (Unavailable)",
        description: "Rust-only build currently supports Studio -> src sync only",
        action: this.liveSyncProcess ? "stopLive" : "startLive",
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
    ];

    const picked = await vscode.window.showQuickPick(items, {
      title: "Roblox MCP Sync",
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
      case "importOnly":
        await this.importSnapshotsOnly();
        return;
      case "startLive":
        await this.startLiveSync();
        return;
      case "stopLive":
        await this.stopLiveSync();
        return;
      case "activeService":
        await this.syncActiveService();
        return;
      case "toggleAuto":
        await this.toggleAutoSyncOnSave();
        return;
      case "showOutput":
        this.output.show(true);
        return;
      default:
        return;
    }
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

  public async importSnapshotsOnly(): Promise<void> {
    await this.enqueue("Import snapshots", async () => {
      const cfg = this.getConfig();
      const snapshotPath = this.resolveSnapshotPath(cfg);
      await this.runRustImport(cfg, snapshotPath, cfg.services);

      vscode.window.showInformationMessage("Roblox Sync: snapshot import finished.");
    });
  }

  public async syncActiveService(): Promise<void> {
    await this.enqueue("Sync active service", async () => {
      const cfg = this.getConfig();
      const activePath = vscode.window.activeTextEditor?.document.uri.fsPath;

      let service = activePath ? this.detectServiceForPath(activePath, cfg.projectRoot, cfg.services) : undefined;

      if (!service) {
        service = await vscode.window.showQuickPick(cfg.services, {
          title: "Roblox MCP Sync",
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

  public async startLiveSync(): Promise<void> {
    vscode.window.showWarningMessage(
      "Roblox Sync: live sync is disabled in the rust-only build. Use Full Sync/Active Service sync.",
    );
  }

  public async stopLiveSync(): Promise<void> {
    if (!this.liveSyncProcess) {
      vscode.window.showInformationMessage("Roblox Sync: live sync is not running.");
      return;
    }

    this.liveSyncStopping = true;
    const proc = this.liveSyncProcess;
    proc.kill();
    vscode.window.showInformationMessage("Roblox Sync: stopping live sync...");
  }

  public async onDocumentSaved(doc: vscode.TextDocument): Promise<void> {
    if (doc.isUntitled || doc.uri.scheme !== "file") {
      return;
    }

    const cfg = this.getConfig();
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

  public onConfigurationChanged(): void {
    this.updateStatusBar();
  }

  private async toggleAutoSyncOnSave(): Promise<void> {
    const cfg = vscode.workspace.getConfiguration("robloxSync");
    const enabled = cfg.get<boolean>("autoSyncOnSave", false);
    await cfg.update("autoSyncOnSave", !enabled, vscode.ConfigurationTarget.Workspace);

    this.updateStatusBar();
    vscode.window.showInformationMessage(`Roblox Sync: auto sync on save ${!enabled ? "enabled" : "disabled"}.`);
  }

  private async enqueue(taskName: string, task: () => Promise<void>): Promise<void> {
    const run = async (): Promise<void> => {
      try {
        this.setActiveTask(taskName);
        this.output.appendLine(`[roblox-sync] task start: ${taskName}`);
        await task();
        this.output.appendLine(`[roblox-sync] task done: ${taskName}`);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[roblox-sync] task failed: ${taskName}: ${message}`);
        vscode.window.showErrorMessage(`Roblox Sync: ${taskName} failed. ${message}`);
        throw err;
      } finally {
        this.setActiveTask(undefined);
      }
    };

    this.queue = this.queue.then(run, run);
    await this.queue;
  }

  private async runExport(options: {
    services: string[];
    runImport: boolean;
    notifyOnSuccess: boolean;
    reason: string;
  }): Promise<void> {
    const cfg = this.getConfig();
    const selectedServices = this.normalizeServices(options.services, cfg.services);
    const useRustImportInExporter = options.runImport;
    const { command, args } = this.resolveExportCommand(
      cfg,
      selectedServices,
      options.runImport,
      useRustImportInExporter,
    );

    this.output.show(true);
    this.output.appendLine(`[roblox-sync] export command: ${command} ${this.renderArgs(args)}`);

    const code = await this.runCommand(command, args, cfg.projectRoot, "export", cfg.progressHeartbeatSeconds);
    if (code !== 0) {
      throw new Error(`Export exited with code ${code}`);
    }

    if (options.notifyOnSuccess && options.reason) {
      vscode.window.showInformationMessage(`Roblox Sync: ${options.reason}.`);
    }
  }

  private resolveExportCommand(
    cfg: SyncConfig,
    selectedServices: string[],
    requestedRunImport: boolean,
    useRustImportInExporter: boolean,
  ): { command: string; args: string[] } {
    const runImportFlag = requestedRunImport ? "--run-import" : "--no-run-import";
    const extraImportArgs: string[] = [];
    if (useRustImportInExporter) {
      this.ensureFileExists(cfg.rustCliPath);
      extraImportArgs.push("--import-cli", cfg.rustCliPath);
    }
    this.ensureFileExists(cfg.exportCliPath);
    return {
      command: cfg.exportCliPath,
      args: [
        "export-snapshots",
        "--project-root",
        cfg.projectRoot,
        "--snapshot-dir",
        cfg.snapshotDir,
        "--transport",
        cfg.transport,
        "--services",
        selectedServices.join(","),
        "--source-workers",
        String(Math.max(0, cfg.sourceWorkers)),
        "--instance-workers",
        String(Math.max(0, cfg.instanceWorkers)),
        "--import-workers",
        String(Math.max(0, cfg.importWorkers)),
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
        "--import-mode",
        cfg.importMode,
        runImportFlag,
        cfg.adaptiveThrottle ? "--adaptive-throttle" : "--no-adaptive-throttle",
        cfg.noUpdateEditorIcons ? "--no-update-editor-icons" : "",
        ...extraImportArgs,
      ].filter((x) => x.length > 0),
    };
  }

  private async runRustImport(cfg: SyncConfig, snapshotPath: string, services: string[]): Promise<void> {
    this.ensureFileExists(cfg.rustCliPath);
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
    this.output.appendLine(`[roblox-sync] rust import command: ${cfg.rustCliPath} ${this.renderArgs(args)}`);
    const code = await this.runCommand(
      cfg.rustCliPath,
      args,
      cfg.projectRoot,
      "rust-import",
      cfg.progressHeartbeatSeconds,
    );
    if (code !== 0) {
      throw new Error(`Rust import exited with code ${code}`);
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
  ): Promise<number> {
    return await new Promise<number>((resolve, reject) => {
      const launchedAt = Date.now();
      let lastOutputAt = launchedAt;
      let sawOutput = false;
      let lastOutputSummary = "process started";
      const child = childProcess.spawn(command, args, {
        cwd,
        env: process.env,
        shell: false,
        stdio: "pipe",
      });
      this.output.appendLine(
        `[roblox-sync] ${label}: spawned pid=${child.pid ?? "unknown"} at ${new Date(launchedAt).toISOString()}`,
      );

      const heartbeatMs = Math.max(2, Math.round(progressHeartbeatSeconds)) * 1000;
      const heartbeatTimer = setInterval(() => {
        const now = Date.now();
        const elapsedSec = ((now - launchedAt) / 1000).toFixed(1);
        const idleSec = ((now - lastOutputAt) / 1000).toFixed(1);
        if (!sawOutput) {
          this.output.appendLine(`[roblox-sync] ${label}: waiting for first output (${elapsedSec}s elapsed)`);
        } else {
          this.output.appendLine(
            `[roblox-sync] ${label}: still running (${elapsedSec}s elapsed, idle ${idleSec}s, last activity: ${lastOutputSummary})`,
          );
        }
      }, heartbeatMs);

      this.bindProcessOutput(child, command, (summary) => {
        sawOutput = true;
        lastOutputAt = Date.now();
        if (summary) {
          lastOutputSummary = summary;
        }
      });

      child.on("error", (err) => {
        clearInterval(heartbeatTimer);
        reject(err);
      });
      child.on("exit", (code) => {
        clearInterval(heartbeatTimer);
        const elapsedSec = ((Date.now() - launchedAt) / 1000).toFixed(1);
        this.output.appendLine(`[roblox-sync] ${label}: exited code=${code ?? 0} after ${elapsedSec}s`);
        resolve(code ?? 0);
      });
    });
  }

  private bindProcessOutput(
    child: childProcess.ChildProcess,
    prefix: string,
    onActivity?: (summary?: string) => void,
  ): void {
    child.stdout?.on("data", (data: Buffer | string) => {
      onActivity?.(this.summarizeProcessChunk(data));
      this.output.append(this.prefixOutput(prefix, data));
    });

    child.stderr?.on("data", (data: Buffer | string) => {
      onActivity?.(this.summarizeProcessChunk(data));
      this.output.append(this.prefixOutput(`${prefix}:err`, data));
    });
  }

  private summarizeProcessChunk(data: Buffer | string): string | undefined {
    const lines = data
      .toString()
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
    if (lines.length === 0) {
      return undefined;
    }
    const lastLine = lines[lines.length - 1]!;
    return lastLine.length > 180 ? `${lastLine.slice(0, 177)}...` : lastLine;
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

  private getConfig(): SyncConfig {
    const root = this.getWorkspaceRoot();
    const cfg = vscode.workspace.getConfiguration("robloxSync");

    const projectRoot = this.resolveConfigPath(cfg.get<string>("projectRoot", "${workspaceFolder}"), root);
    const configTomlPath = this.resolveConfigPath(
      cfg.get<string>("configTomlPath", "${userHome}/.codex/config.toml"),
      root,
    );
    const watchConfigPath = this.resolveConfigPath(
      cfg.get<string>("watchConfigPath", "${workspaceFolder}/tools/editor_to_studio_sync.json"),
      root,
    );
    const exportCliPath = this.resolveConfigPath(
      cfg.get<string>("exportCliPath", "${workspaceFolder}/tools/roblox-sync-rs/target/release/roblox-sync-rs.exe"),
      root,
    );
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
    const wsWaitSeconds = this.getWsWaitSeconds(cfg);
    const rustCliPath = this.resolveConfigPath(
      cfg.get<string>("rustCliPath", "${workspaceFolder}/tools/roblox-sync-rs/target/release/roblox-sync-rs.exe"),
      root,
    );

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
      sourceWorkers: Number(cfg.get<number>("sourceWorkers", 0) ?? 0),
      instanceWorkers: Number(cfg.get<number>("instanceWorkers", 0) ?? 0),
      importWorkers: Number(cfg.get<number>("importWorkers", 0) ?? 0),
      chunkSize: Number(cfg.get<number>("chunkSize", 262144) ?? 262144),
      snapshotInstanceChunkSize: Number(cfg.get<number>("snapshotInstanceChunkSize", 5000) ?? 5000),
      bridgeWaitSeconds: Number(cfg.get<number>("bridgeWaitSeconds", 8) ?? 8),
      bridgePorts: this.normalizeBridgePorts(
        String(
          cfg.get<string>(
            "bridgePorts",
            DEFAULT_BRIDGE_PORTS.join(","),
          ) ?? DEFAULT_BRIDGE_PORTS.join(","),
        ),
      ),
      adaptiveThrottle: cfg.get<boolean>("adaptiveThrottle", true),
      noUpdateEditorIcons: cfg.get<boolean>("noUpdateEditorIcons", true),
      autoSyncOnSave: cfg.get<boolean>("autoSyncOnSave", false),
      autoSyncDebounceMs: Number(cfg.get<number>("autoSyncDebounceMs", 800) ?? 800),
      runImport: cfg.get<boolean>("runImport", true),
      importMode,
      watchConfigPath,
      wsWaitSeconds,
      progressHeartbeatSeconds: Number(cfg.get<number>("progressHeartbeatSeconds", 2) ?? 2),
    };
  }

  private normalizeBridgePorts(raw: string): string {
    const parsed = raw
      .split(",")
      .map((token) => Number.parseInt(token.trim(), 10))
      .filter((value) => Number.isInteger(value) && value > 0 && value <= 65535)
      .filter((value, index, all) => all.indexOf(value) === index);

    let normalized = parsed;
    const matchesLegacyDefault =
      normalized.length === LEGACY_BRIDGE_PORTS.length &&
      normalized.every((value, index) => value === LEGACY_BRIDGE_PORTS[index]);
    if (matchesLegacyDefault) {
      if (!this.warnedLegacyBridgePorts) {
        this.warnedLegacyBridgePorts = true;
        this.output.appendLine(
          `[roblox-sync] config: migrating legacy 8-port bridge default to ${DEFAULT_BRIDGE_PORTS.join(",")}.`,
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
          `[roblox-sync] config: only 4 bridge ports are supported; using ${normalized
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
      return configuredWsWaitSeconds;
    }

    const legacyStartupWaitSeconds = this.getConfiguredNumber(cfg, "startupWaitSeconds");
    if (legacyStartupWaitSeconds !== undefined) {
      if (!this.warnedLegacyStartupWaitSeconds) {
        this.warnedLegacyStartupWaitSeconds = true;
        this.output.appendLine(
          "[roblox-sync] config: using legacy robloxSync.startupWaitSeconds as robloxSync.wsWaitSeconds; update your settings to robloxSync.wsWaitSeconds.",
        );
      }
      return legacyStartupWaitSeconds;
    }

    return Number(cfg.get<number>("wsWaitSeconds", 20) ?? 20);
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
    return configuredValue === undefined ? undefined : Number(configuredValue);
  }

  private resolveConfigPath(raw: string, workspaceRoot: string): string {
    const replaced = raw
      .replaceAll("${workspaceFolder}", workspaceRoot)
      .replaceAll("${userHome}", os.homedir());
    return path.isAbsolute(replaced) ? path.normalize(replaced) : path.normalize(path.join(workspaceRoot, replaced));
  }

  private getWorkspaceRoot(): string {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) {
      throw new Error("Open a workspace folder before using Roblox Sync.");
    }
    return folder.uri.fsPath;
  }

  private detectServiceForPath(filePath: string, projectRoot: string, services: string[]): string | undefined {
    const srcRoot = path.join(projectRoot, "src");
    const normalizedFilePath = path.normalize(filePath).toLowerCase();
    const normalizedSrcRoot = path.normalize(srcRoot).toLowerCase();

    if (!normalizedFilePath.startsWith(normalizedSrcRoot)) {
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
      this.statusItem.text = `$(sync~spin) Roblox Sync ${elapsedSeconds}s`;
      this.statusItem.tooltip = `${this.activeTaskName} in progress`;
      return;
    }

    const autoEnabled = vscode.workspace.getConfiguration("robloxSync").get<boolean>("autoSyncOnSave", false);

    if (this.liveSyncProcess) {
      this.statusItem.text = "$(sync~spin) Roblox Sync Live";
      this.statusItem.tooltip = "Live sync running (Editor -> Studio)";
      return;
    }

    if (autoEnabled) {
      this.statusItem.text = "$(sync) Roblox Sync Auto";
      this.statusItem.tooltip = "Auto sync on save is enabled";
      return;
    }

    this.statusItem.text = "$(sync) Roblox Sync";
    this.statusItem.tooltip = "Open Roblox MCP Sync menu";
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
}

export function activate(context: vscode.ExtensionContext): void {
  const controller = new RobloxSyncController(context);

  context.subscriptions.push(
    controller,
    vscode.commands.registerCommand("robloxSync.openMenu", () => controller.openMenu()),
    vscode.commands.registerCommand("robloxSync.fullSync", () => controller.fullSync()),
    vscode.commands.registerCommand("robloxSync.exportSnapshots", () => controller.exportSnapshotsOnly()),
    vscode.commands.registerCommand("robloxSync.importSnapshots", () => controller.importSnapshotsOnly()),
    vscode.commands.registerCommand("robloxSync.startLiveSync", () => controller.startLiveSync()),
    vscode.commands.registerCommand("robloxSync.stopLiveSync", () => controller.stopLiveSync()),
    vscode.commands.registerCommand("robloxSync.syncActiveService", () => controller.syncActiveService()),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      void controller.onDocumentSaved(doc);
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration("robloxSync")) {
        controller.onConfigurationChanged();
      }
    }),
  );
}

export function deactivate(): void {
  // Resources are disposed via extension context subscriptions.
}
