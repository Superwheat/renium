import type { ChildProcessWithoutNullStreams } from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import {
  ExplorerBackendClient,
  type ExplorerBackendResponse,
  type ExplorerRowRequest,
  type ExplorerRowSummary,
  type ExplorerViewMode,
} from "./explorerBackendClient";
import { fileExplorerWebviewHtml } from "./fileExplorerWebview";
import { emptyGitViewState, type GitViewActions, type GitViewState } from "./gitView";
import { ROBLOX_CLASS_NAMES } from "./robloxClasses";
import {
  MAX_STORE_DROPPED_BYTES,
  decodeSettingsStoreBytes,
  decodeSettingsStoreToTree,
  type DecodeResult,
} from "./settingsStoreDecode";
import { safeObject } from "./utils";
import { FileExplorerModel } from "./fileExplorerModel";
import { FilePropertiesViewProvider } from "./filePropertiesView";
import { spawnTrackedProcess, terminateProcess } from "./processSupervisor";
import {
  type EditorHistoryManifest,
  type ExplorerConfig,
  type ExplorerHistoryEntry,
  type ExplorerHistoryGroup,
  type ExplorerHistoryTarget,
  type FileExplorerNode,
  type FileExplorerNodeKind,
  type ReadonlyInstanceInfo,
  type ViewVisibilityHandler,
  canonicalExplorerServices,
  editorHistoryRoot,
  escapeHtml,
  getExplorerConfig,
  iconAssetNameForClass,
  isNoMatchingInstanceError,
  linkPathKey,
  loadAssetIconNames,
  logPackageDragDebug,
  nodeLinkPathKey,
  nodeTargetPath,
  normalizeFilesystemPathKey,
  pathInsideRoot,
  projectGraphOwnsPath,
  resolveExplorerCliPath,
  runJsonCli,
  settingsFileForService,
  workspaceRoot,
} from "./fileExplorerCore";

const MAX_STORE_DROPPED_BASE64_CHARS = 4 * Math.ceil(MAX_STORE_DROPPED_BYTES / 3);
const MUTATION_MESSAGE_TYPES = new Set([
  "restoreHistory", "restoreHistoryGroup", "addInstance", "createInstance", "renameInstance", "moveInstance",
  "deleteInstance", "desyncPackageLink", "pasteInstance", "duplicateInstance", "importModel", "createLink",
  "resaveLink", "relinkLink", "insertPackage", "breakLink",
]);

export class FileExplorerViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "renium.fileExplorer";
  private webviewView: vscode.WebviewView | undefined;
  private selectedId: string | undefined;
  private webviewReady = false;
  private lastErrorMessage: string | undefined;
  private readonly backend = new ExplorerBackendClient(getExplorerConfig, (response) => this.onBackendEvent(response));
  private readonly searchBackend = new ExplorerBackendClient(getExplorerConfig, (response) => this.onBackendEvent(response));
  private clipboardNodeId: string | undefined;
  private currentMode: ExplorerViewMode = "normal";
  private rowWindow = { start: 0, count: 80 };
  private rowRequestSerial = 0;
  private rowRequestInFlight = false;
  private queuedRowRequest: ExplorerRowRequest | undefined;
  private mutationQueue: Promise<void> = Promise.resolve();
  private readonly activeMessageTasks = new Set<Promise<void>>();
  private mutationAdmissionOpen = true;
  private searchGeneration = 0;
  private projectGeneration = 0;
  private readonly propertyOnlyStaleServices = new Set<string>();
  private referencePreviewId: string | undefined;
  private referencePreviewScrollPending = false;
  private readonly availableIconNames: ReadonlySet<string>;
  private gitState: GitViewState | undefined;
  private gitLoading = false;
  private revealGitOnReady = false;
  private externalPackageDrag: {
    id: string;
    name?: string;
    mode?: string;
    projectRoot: string;
    generation: number;
  } | undefined;
  private packageCursorProcess: ChildProcessWithoutNullStreams | undefined;
  private packageCursorBuffer = "";
  private packageCursorLastPost = 0;
  private packageCursorLastTrace = 0;
  private packageCursorSampleCount = 0;
  private packageCursorSawButtonDown = false;
  private packageCursorReleaseTimer: NodeJS.Timeout | undefined;
  private linkState: Record<string, string> = {};

  public setLinkState(keys: Record<string, string>): void {
    this.linkState = keys;
    if (this.webviewReady) {
      this.webviewView?.webview.postMessage({ type: "linkState", keys: this.linkState });
    }
  }

  public setExternalPackageDrag(link?: { id: string; name?: string; mode?: string }): void {
    this.externalPackageDrag = link
      ? {
        ...link,
        projectRoot: getExplorerConfig().projectRoot,
        generation: this.projectGeneration,
      }
      : undefined;
    logPackageDragDebug(
      `explorer.host.setExternalPackageDrag: ${link ? `armed ${link.id} name=${link.name ?? ""} mode=${link.mode ?? ""}` : "cleared"} webviewReady=${this.webviewReady}`,
    );
    if (link) {
      this.startPackageCursorPolling();
    } else {
      this.stopPackageCursorPolling();
    }
    if (this.webviewReady) {
      this.webviewView?.webview.postMessage({ type: "packageDrag", link: this.externalPackageDrag ?? null });
    }
  }

  private startPackageCursorPolling(): void {
    if (process.platform !== "win32" || this.packageCursorProcess) {
      return;
    }
    const root = workspaceRoot();
    const config = getExplorerConfig();
    const cursorPollCliPath = resolveExplorerCliPath(root, config.cliPath);
    const { child } = spawnTrackedProcess(
      cursorPollCliPath,
      ["cursor-poll", "--interval-ms", "16"],
      config.projectRoot,
    );
    this.packageCursorProcess = child;
    this.packageCursorBuffer = "";
    this.packageCursorLastTrace = 0;
    this.packageCursorSampleCount = 0;
    this.packageCursorSawButtonDown = false;
    logPackageDragDebug(`explorer.host.cursorPoll: started cli=${cursorPollCliPath}`);
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      this.onPackageCursorChunk(chunk);
    });
    child.stderr?.setEncoding("utf8");
    child.stderr?.on("data", (chunk: string) => {
      const text = chunk.trim();
      logPackageDragDebug(`explorer.host.cursorPoll.stderr: ${text.slice(0, 600)}`);
      if (this.packageCursorProcess === child) {
        this.stopPackageCursorPolling("stderr");
      } else {
        void terminateProcess(child);
      }
    });
    child.on("error", (error) => {
      logPackageDragDebug(`explorer.host.cursorPoll.error: ${error instanceof Error ? error.message : String(error)}`);
      if (this.packageCursorProcess === child) {
        this.packageCursorProcess = undefined;
      }
    });
    child.on("exit", (code, signal) => {
      logPackageDragDebug(`explorer.host.cursorPoll.exit: code=${code ?? ""} signal=${signal ?? ""}`);
      if (this.packageCursorProcess === child) {
        this.packageCursorProcess = undefined;
      }
    });
  }

  private onPackageCursorChunk(chunk: string): void {
    this.packageCursorBuffer += chunk;
    let newline = this.packageCursorBuffer.indexOf("\n");
    while (newline >= 0) {
      const line = this.packageCursorBuffer.slice(0, newline).trim();
      this.packageCursorBuffer = this.packageCursorBuffer.slice(newline + 1);
      const match = /^(-?\d+),(-?\d+),(-?\d+),(-?\d+),(-?\d+),(-?\d+)(?:,([01]))?$/.exec(line);
      if (match && this.externalPackageDrag && this.webviewReady) {
        const now = Date.now();
        const leftButtonDown = match[7] === "1";
        this.packageCursorSampleCount += 1;
        if (this.packageCursorSampleCount === 1 || now - this.packageCursorLastTrace >= 500) {
          this.packageCursorLastTrace = now;
          logPackageDragDebug(
            `explorer.host.cursorPoll.sample: count=${this.packageCursorSampleCount} cursor=${match[1]},${match[2]} hwnd=${match[3]},${match[4]},${match[5]},${match[6]} button=${leftButtonDown ? 1 : 0} webviewReady=${this.webviewReady}`,
          );
        }
        if (this.externalPackageDrag.mode === "drag") {
          if (leftButtonDown) {
            this.packageCursorSawButtonDown = true;
            if (this.packageCursorReleaseTimer) {
              clearTimeout(this.packageCursorReleaseTimer);
              this.packageCursorReleaseTimer = undefined;
            }
          } else if (this.packageCursorSawButtonDown && !this.packageCursorReleaseTimer) {
            const dragId = this.externalPackageDrag.id;
            this.packageCursorReleaseTimer = setTimeout(() => {
              this.packageCursorReleaseTimer = undefined;
              if (this.externalPackageDrag?.id === dragId) {
                logPackageDragDebug(`explorer.host.cursorPoll.releaseClear: ${dragId}`);
                this.setExternalPackageDrag(undefined);
              }
            }, 250);
          }
        }
        if (now - this.packageCursorLastPost >= 16) {
          this.packageCursorLastPost = now;
          this.webviewView?.webview.postMessage({
            type: "packageDragCursor",
            screenX: Number(match[1]),
            screenY: Number(match[2]),
            windowLeft: Number(match[3]),
            windowTop: Number(match[4]),
            windowRight: Number(match[5]),
            windowBottom: Number(match[6]),
            leftButtonDown,
          });
        }
      }
      newline = this.packageCursorBuffer.indexOf("\n");
    }
  }

  private stopPackageCursorPolling(reason = "stopped"): void {
    const child = this.packageCursorProcess;
    if (this.packageCursorReleaseTimer) {
      clearTimeout(this.packageCursorReleaseTimer);
      this.packageCursorReleaseTimer = undefined;
    }
    this.packageCursorSawButtonDown = false;
    if (!child) {
      return;
    }
    this.packageCursorProcess = undefined;
    this.packageCursorBuffer = "";
    logPackageDragDebug(`explorer.host.cursorPoll: ${reason}`);
    void terminateProcess(child);
  }

  private directReniumLinkTargetPath(node: FileExplorerNode): string[] | undefined {
    const key = nodeLinkPathKey(node);
    return key && this.linkState[key] === "linked" ? nodeTargetPath(node) : undefined;
  }

  private childPathUnder(parent: FileExplorerNode, childName: string): string[] {
    const parentPath = parent.kind === "service" ? [parent.service] : nodeTargetPath(parent);
    return parentPath.concat(childName);
  }

  private siblingNamed(parent: FileExplorerNode | undefined, name: string, exceptTreeId?: string): FileExplorerNode | undefined {
    if (!parent) {
      return undefined;
    }
    return this.model.getChildren(parent).find((child) => child.treeId !== exceptTreeId && child.name === name);
  }

  private renamedPathForNode(node: FileExplorerNode, newName: string): string[] {
    if (node.kind === "service") {
      return [node.service];
    }
    const parent = node.parentTreeId ? this.model.getNode(node.parentTreeId) : undefined;
    if (parent) {
      return this.childPathUnder(parent, newName);
    }
    const currentPath = nodeTargetPath(node);
    if (currentPath.length > 0) {
      return currentPath.slice(0, -1).concat(newName);
    }
    return [node.service, newName];
  }

  private hasLinkedTargetCollision(service: string, oldPathSegments: string[], newPathSegments: string[]): boolean {
    const oldKey = linkPathKey(service, oldPathSegments);
    const newKey = linkPathKey(service, newPathSegments);
    return !!newKey && newKey !== oldKey && this.linkState[newKey] === "linked";
  }

  private async moveReniumLinkTarget(
    oldService: string,
    oldPathSegments: string[],
    newService: string,
    newPathSegments: string[],
  ): Promise<void> {
    const config = getExplorerConfig();
    const cfg = vscode.workspace.getConfiguration("renium");
    const manifest = cfg.get<string>("link.manifest", "renium-link.json");
    await runJsonCli(config, [
      "link-move-target",
      "-r",
      config.projectRoot,
      "--manifest",
      manifest,
      "--old-service",
      oldService,
      "--old-path",
      JSON.stringify(oldPathSegments),
      "--new-service",
      newService,
      "--new-path",
      JSON.stringify(newPathSegments),
    ]);
    const oldKey = linkPathKey(oldService, oldPathSegments);
    const newKey = linkPathKey(newService, newPathSegments);
    if (oldKey && newKey && this.linkState[oldKey]) {
      this.linkState[newKey] = this.linkState[oldKey];
      delete this.linkState[oldKey];
      this.setLinkState(this.linkState);
    }
  }

  public constructor(
    private readonly model: FileExplorerModel,
    private readonly extensionUri: vscode.Uri,
    private readonly propertiesProvider: FilePropertiesViewProvider,
    private readonly actions: {
      openScript: (node?: FileExplorerNode) => Promise<void>;
      addInstance: (node?: FileExplorerNode) => Promise<void>;
      deleteInstance: (node?: FileExplorerNode) => Promise<void>;
      desyncPackageLink: (node?: FileExplorerNode) => Promise<void>;
      copyPath: (node?: FileExplorerNode) => Promise<void>;
      importModel: (node?: FileExplorerNode, modelPaths?: string[]) => Promise<void>;
      exportModel: (node?: FileExplorerNode) => Promise<void>;
      createLink: (node?: FileExplorerNode) => Promise<void>;
      resaveLink: (node?: FileExplorerNode) => Promise<void>;
      relinkLink: (node?: FileExplorerNode) => Promise<void>;
      breakLink: (node?: FileExplorerNode) => Promise<void>;
      insertPackage: (node: FileExplorerNode | undefined, link: { id: string; name?: string }) => Promise<void>;
      onSelectNode?: () => void;
      git?: GitViewActions;
    },
    private readonly onVisibilityChanged?: ViewVisibilityHandler,
  ) {
    this.availableIconNames = new Set(loadAssetIconNames(this.extensionUri));
  }

  public dispose(): void {
    this.stopPackageCursorPolling();
    this.backend.dispose();
    this.searchBackend.dispose();
  }

  public clearSelection(): void {
    if (!this.selectedId && !this.referencePreviewId) {
      return;
    }
    this.selectedId = undefined;
    this.referencePreviewId = undefined;
    if (this.webviewReady) {
      this.webviewView?.webview.postMessage({ type: "clearSelection" });
    }
  }

  public resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.webviewView = webviewView;
    this.lastErrorMessage = undefined;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "assets")],
    };
    webviewView.webview.onDidReceiveMessage((message) => {
      const task = this.onMessage(message).catch((error) => {
        const text = error instanceof Error ? error.message : String(error);
        this.pushError(text);
        vscode.window.showErrorMessage(`Explorer action failed. ${text}`);
      });
      this.activeMessageTasks.add(task);
      void task.finally(() => this.activeMessageTasks.delete(task));
    });
    this.onVisibilityChanged?.(FileExplorerViewProvider.viewType, webviewView.visible);
    webviewView.onDidChangeVisibility(() => {
      this.onVisibilityChanged?.(FileExplorerViewProvider.viewType, webviewView.visible);
      if (!webviewView.visible) {
        return;
      }
      if (!this.webviewReady) {
        this.resetWebviewHtml(webviewView);
      } else {
        this.requestRows();
      }
    });
    this.resetWebviewHtml(webviewView);
    void this.refresh();
  }

  private resetWebviewHtml(webviewView: vscode.WebviewView): void {
    this.webviewReady = false;
    const assetBase = webviewView.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, "assets")).toString();
    webviewView.webview.html = this.html(assetBase);
    setTimeout(() => {
      if (!this.webviewView || this.webviewView !== webviewView || this.webviewReady) {
        return;
      }
      this.webviewReady = true;
      this.webviewView?.webview.postMessage({ type: "packageDrag", link: this.externalPackageDrag ?? null });
      if (this.lastErrorMessage) {
        this.pushError(this.lastErrorMessage);
      } else {
        this.requestRows();
      }
    }, 500);
  }

  public reveal(): void {
    this.webviewView?.show?.(true);
  }

  public async revealReference(nodeId: string): Promise<void> {
    const generation = this.projectGeneration;
    this.referencePreviewId = nodeId;
    this.referencePreviewScrollPending = true;
    this.currentMode = "normal";
    this.queuedRowRequest = undefined;
    this.rowRequestSerial += 1;
    this.reveal();
    this.webviewView?.webview.postMessage({ type: "prepareReferencePreview" });
    await this.backend.ensureInitialized();
    if (generation !== this.projectGeneration) {
      return;
    }
    const count = Math.max(this.rowWindow.count, 120);
    let start = this.rowWindow.start;
    const reveal = await this.backend.revealNode(nodeId);
    if (generation !== this.projectGeneration) {
      return;
    }
    if (typeof reveal.rowIndex === "number") {
      start = Math.max(0, reveal.rowIndex - Math.floor(count / 2));
    }
    this.rowWindow = { start, count };
    await this.requestRows(start, count, "normal");
  }

  public async refresh(): Promise<void> {
    const generation = this.projectGeneration;
    try {
      await this.backend.initialize();
      if (generation !== this.projectGeneration) {
        return;
      }
      await this.requestRows();
    } catch (error) {
      if (generation !== this.projectGeneration) {
        return;
      }
      this.pushError(error instanceof Error ? error.message : String(error));
      vscode.window.showErrorMessage(`Failed to refresh Explorer. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  public async switchProject(): Promise<void> {
    const refreshGit = this.gitState !== undefined || this.gitLoading || this.revealGitOnReady;
    this.projectGeneration += 1;
    this.searchGeneration += 1;
    this.mutationAdmissionOpen = true;
    this.mutationQueue = Promise.resolve();
    this.clipboardNodeId = undefined;
    this.propertyOnlyStaleServices.clear();
    this.propertiesProvider.resetProjectState();
    this.backend.restart();
    this.searchBackend.restart();
    this.selectedId = undefined;
    this.currentMode = "normal";
    this.referencePreviewId = undefined;
    this.gitState = undefined;
    this.gitLoading = false;
    this.postGitState();
    this.queuedRowRequest = undefined;
    this.rowRequestSerial += 1;
    await this.refresh();
    if (refreshGit) {
      await this.refreshGit();
    }
  }

  public async prepareProjectSwitch(): Promise<void> {
    this.mutationAdmissionOpen = false;
    const properties = this.propertiesProvider.prepareProjectSwitch();
    await Promise.allSettled(Array.from(this.activeMessageTasks));
    await this.mutationQueue.catch(() => undefined);
    await properties;
    await this.model.prepareProjectSwitch();
  }

  public cancelProjectSwitch(): void {
    this.mutationAdmissionOpen = true;
    this.propertiesProvider.cancelProjectSwitch();
  }

  public async showGit(): Promise<void> {
    this.revealGitOnReady = true;
    this.reveal();
    if (this.webviewView && this.webviewReady) {
      this.webviewView.webview.postMessage({ type: "setTab", tab: "git" });
      this.revealGitOnReady = false;
      await this.refreshGit();
    }
  }

  public async refreshGit(options: { fetch?: boolean } = {}): Promise<void> {
    const git = this.actions.git;
    if (!git || !this.webviewView || !this.webviewReady) {
      return;
    }
    const generation = this.projectGeneration;
    const projectRoot = path.resolve(getExplorerConfig().projectRoot);
    this.gitLoading = true;
    this.postGitState();
    try {
      const state = await git.refresh({ ...options, projectRoot });
      if (
        !this.isCurrentGitContext(projectRoot, generation)
        || !state.projectRoot
        || !this.isCurrentGitContext(state.projectRoot, generation)
      ) {
        return;
      }
      this.gitState = state;
    } catch (error) {
      if (!this.isCurrentGitContext(projectRoot, generation)) {
        return;
      }
      this.gitState = emptyGitViewState(
        projectRoot,
        vscode.workspace.isTrusted,
        error instanceof Error ? error.message : String(error),
      );
    } finally {
      if (this.isCurrentGitContext(projectRoot, generation)) {
        this.gitLoading = false;
        this.postGitState();
      }
    }
  }

  private postGitState(): void {
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    let projectRoot = "";
    try {
      projectRoot = path.resolve(getExplorerConfig().projectRoot);
    } catch {
    }
    this.webviewView.webview.postMessage({
      type: "gitState",
      state: this.gitState,
      loading: this.gitLoading,
      projectRoot,
      generation: this.projectGeneration,
    });
  }

  private isCurrentGitContext(projectRoot: string | undefined, generation: number | undefined): boolean {
    if (!projectRoot || generation !== this.projectGeneration) {
      return false;
    }
    try {
      const currentRoot = path.resolve(getExplorerConfig().projectRoot);
      const requestedRoot = path.resolve(projectRoot);
      return process.platform === "win32"
        ? currentRoot.toLowerCase() === requestedRoot.toLowerCase()
        : currentRoot === requestedRoot;
    } catch {
      return false;
    }
  }

  public async refreshServices(services: string[]): Promise<void> {
    const generation = this.projectGeneration;
    try {
      const canonicalServices = canonicalExplorerServices(getExplorerConfig(), services);
      const affected = new Set(canonicalServices);
      this.model.invalidateServices(canonicalServices);
      if (this.selectedId && affected.has(this.serviceFromNodeId(this.selectedId) ?? "")) {
        this.selectedId = undefined;
        this.referencePreviewId = undefined;
        this.propertiesProvider.resetProjectState();
      }
      if (this.clipboardNodeId && affected.has(this.serviceFromNodeId(this.clipboardNodeId) ?? "")) {
        this.clipboardNodeId = undefined;
      }
      await this.backend.reloadServices(canonicalServices);
      if (generation !== this.projectGeneration) {
        return;
      }
      if (this.searchBackend.hasInitialized()) {
        await this.searchBackend.reloadServices(canonicalServices).catch(() => undefined);
        if (generation !== this.projectGeneration) {
          return;
        }
      }
      for (const service of canonicalServices) {
        this.propertyOnlyStaleServices.delete(service);
      }
      try {
        await this.propertiesProvider.refreshCurrentForServices(canonicalServices);
        if (generation !== this.projectGeneration) {
          return;
        }
      } catch (error) {
        if (!isNoMatchingInstanceError(error)) {
          throw error;
        }
      }
      await this.requestRows();
    } catch (error) {
      if (generation !== this.projectGeneration) {
        return;
      }
      if (isNoMatchingInstanceError(error)) {
        await this.requestRows().catch(() => undefined);
        return;
      }
      this.pushError(error instanceof Error ? error.message : String(error));
      vscode.window.showErrorMessage(`Failed to refresh Explorer changes. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async enqueueMutation(task: () => Promise<void>): Promise<void> {
    if (!this.mutationAdmissionOpen) {
      return;
    }
    const generation = this.projectGeneration;
    const runTask = async (): Promise<void> => {
      if (generation === this.projectGeneration) {
        await task();
      }
    };
    const run = this.mutationQueue.then(runTask, runTask);
    this.mutationQueue = run.catch(() => undefined);
    await run;
  }

  public async refreshSettingsFiles(settingsFiles: string[]): Promise<void> {
    const services = this.model.servicesFromSettingsFiles(settingsFiles);
    if (services.length > 0) {
      await this.refreshServices(services);
    }
  }

  public async refreshPropertyChanges(settingsFiles: string[]): Promise<void> {
    const services = this.model.servicesFromSettingsFiles(settingsFiles);
    for (const service of services) {
      this.propertyOnlyStaleServices.add(service);
    }
    await this.propertiesProvider.refreshCurrentForSettingsFiles(settingsFiles);
  }

  private async requestRows(start = this.rowWindow.start, count = this.rowWindow.count, mode = this.currentMode, scrollToSelected = false, includeMatchIds = false, revision?: number): Promise<void> {
    if (!this.webviewView) {
      return;
    }
    if (!this.webviewReady) {
      return;
    }
    const maxCount = mode === "search" ? 2600 : 1400;
    this.queuedRowRequest = {
      start: Math.max(0, start),
      count: Math.max(1, Math.min(maxCount, count)),
      mode,
      scrollToSelected,
      includeMatchIds,
      revision,
      generation: this.projectGeneration,
    };
    if (this.rowRequestInFlight) {
      return;
    }
    this.rowRequestInFlight = true;
    try {
      while (this.queuedRowRequest) {
        const request: ExplorerRowRequest = this.queuedRowRequest;
        this.queuedRowRequest = undefined;
        if (request.generation !== this.projectGeneration) {
          continue;
        }
        this.rowWindow = { start: request.start, count: request.count };
        const serial = ++this.rowRequestSerial;
        try {
          const backend = request.mode === "search" ? this.searchBackend : this.backend;
          if (request.mode === "search") {
            await backend.ensureInitialized();
          }
          const response = await backend.getRows(request.start, request.count, request.mode, request.includeMatchIds);
          if (
            request.generation !== this.projectGeneration
            || this.queuedRowRequest
            || serial !== this.rowRequestSerial
            || request.mode !== this.currentMode
          ) {
            if (request.mode === this.currentMode) {
              this.postRowsPrefetch(response, request.revision);
            }
            continue;
          }
          this.postRowsWindow(response, request.scrollToSelected, request.revision);
        } catch (error) {
          if (
            request.generation === this.projectGeneration
            && !this.queuedRowRequest
            && request.mode === this.currentMode
          ) {
            const message = error instanceof Error ? error.message : String(error);
            if (message.includes("Explorer backend exited with code 0")) {
              this.queuedRowRequest = request;
              continue;
            }
            this.pushError(message);
          }
        }
      }
    } finally {
      this.rowRequestInFlight = false;
      if (this.queuedRowRequest) {
        void this.requestRows();
      }
    }
  }

  private postRowsWindow(response: ExplorerBackendResponse, scrollToSelected = false, revision?: number): void {
    if (!this.webviewView || !this.webviewReady || response.type !== "rowsWindow") {
      return;
    }
    for (const row of response.rows ?? []) {
      this.model.rememberNode(this.nodeFromBackend(row));
    }
    this.lastErrorMessage = undefined;
    this.webviewView.webview.postMessage({
      ...response,
      selectedId: this.selectedId,
      referencePreviewId: this.referencePreviewId ?? null,
      scrollToReferencePreview: this.referencePreviewScrollPending,
      hasClipboardInstance: this.hasClipboardInstance(),
      scrollToSelected,
      revision,
    });
    this.referencePreviewScrollPending = false;
  }

  private postRowsPrefetch(response: ExplorerBackendResponse, revision?: number): void {
    if (!this.webviewView || !this.webviewReady || response.type !== "rowsWindow") {
      return;
    }
    for (const row of response.rows ?? []) {
      this.model.rememberNode(this.nodeFromBackend(row));
    }
    this.webviewView.webview.postMessage({
      ...response,
      type: "rowsPrefetch",
      hasClipboardInstance: this.hasClipboardInstance(),
      revision,
    });
  }

  private hasClipboardInstance(): boolean {
    return !!this.clipboardNodeId && !!this.model.getNode(this.clipboardNodeId);
  }

  private postClipboardState(): void {
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    this.webviewView.webview.postMessage({
      type: "clipboardState",
      hasClipboardInstance: this.hasClipboardInstance(),
    });
  }

  private postOptimisticDelete(id: string): void {
    if (!this.webviewView || !this.webviewReady || !id) {
      return;
    }
    this.webviewView.webview.postMessage({ type: "optimisticDelete", id });
  }

  private async prefetchRows(start: number, count: number, mode: ExplorerViewMode, revision?: number): Promise<void> {
    if (!this.webviewView || !this.webviewReady || mode !== this.currentMode) {
      return;
    }
    const generation = this.projectGeneration;
    try {
      const backend = mode === "search" ? this.searchBackend : this.backend;
      if (mode === "search") {
        await backend.ensureInitialized();
      }
      const response = await backend.getRows(Math.max(0, start), Math.max(1, Math.min(2400, count)), mode);
      if (
        generation !== this.projectGeneration
        || !this.webviewView
        || !this.webviewReady
        || mode !== this.currentMode
        || response.type !== "rowsWindow"
      ) {
        return;
      }
      for (const row of response.rows ?? []) {
        this.model.rememberNode(this.nodeFromBackend(row));
      }
      this.webviewView.webview.postMessage({
        ...response,
        type: "rowsPrefetch",
        revision,
      });
    } catch {
      if (generation === this.projectGeneration) {
        this.webviewView?.webview.postMessage({ type: "rowsPrefetchDone", mode, revision });
      }
    }
  }

  private nodeFromBackend(row: ExplorerRowSummary | ExplorerBackendResponse["details"]): FileExplorerNode {
    const id = String(row?.id ?? "");
    const service = String(row?.service ?? this.serviceFromNodeId(id) ?? "");
    const kind = (row?.kind === "service" ? "service" : "instance") as FileExplorerNodeKind;
    const settingsId = typeof row?.settingsId === "string" ? row.settingsId : this.settingsIdFromNodeId(id);
    const config = getExplorerConfig();
    const hasBackendSettingsFile = !!row && Object.prototype.hasOwnProperty.call(row, "settingsFile");
    return {
      id,
      treeId: id,
      kind,
      service,
      name: String(row?.name ?? service),
      className: String(row?.className ?? service),
      settingsId,
      projectionSettingsId: this.settingsIdFromNodeId(id),
      index: typeof row?.index === "number" ? row.index : undefined,
      parentTreeId: typeof row?.parentId === "string" ? row.parentId : null,
      children: [],
      loaded: false,
      detailsLoaded: !!row?.properties || !!row?.attributes,
      hasChildren: row?.hasChildren === true || Number(row?.childCount ?? 0) > 0,
      hasPackageLink: row?.hasPackageLink === true,
      settingsFile: hasBackendSettingsFile
        ? typeof row?.settingsFile === "string" ? path.normalize(row.settingsFile) : ""
        : settingsFileForService(config, service),
      sourcePath: typeof row?.sourcePath === "string" ? row.sourcePath : undefined,
      pathSegments: Array.isArray(row?.pathSegments) ? row.pathSegments : [service],
      pathOrdinals: Array.isArray(row?.pathOrdinals) ? row.pathOrdinals : [1],
      properties: safeObject(row?.properties),
      attributes: safeObject(row?.attributes),
      searchMatched: row?.matched === true,
    };
  }

  private serviceFromNodeId(nodeId: string): string | undefined {
    if (nodeId.startsWith("service:")) {
      return nodeId.slice("service:".length);
    }
    return nodeId.split(":")[0] || undefined;
  }

  private settingsIdFromNodeId(nodeId: string): string | undefined {
    if (nodeId.startsWith("service:")) {
      return undefined;
    }
    const index = nodeId.indexOf(":");
    return index >= 0 ? nodeId.slice(index + 1) : undefined;
  }

  private pushError(message: string): void {
    this.lastErrorMessage = message;
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    this.webviewView.webview.postMessage({ type: "error", message });
  }

  private onBackendEvent(response: ExplorerBackendResponse): void {
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    if (response.type === "searchStatus") {
      this.webviewView.webview.postMessage({
        type: "searchStatus",
        loading: response.state !== "complete",
        loaded: response.loaded,
        total: response.total,
        matchCount: response.matchCount,
      });
    }
  }

  private async handleSettingsStoreDecode(name: string | undefined, base64: string | undefined): Promise<void> {
    const webview = this.webviewView?.webview;
    if (!webview || typeof base64 !== "string") {
      return;
    }
    const displayName = name && name.trim().length > 0 ? name.trim().slice(0, 255) : "dropped.renium";
    if (base64.length > MAX_STORE_DROPPED_BASE64_CHARS || base64.length % 4 === 1 || !/^[A-Za-z0-9+/]*={0,2}$/.test(base64)) {
      this.postSettingsStoreResult(webview, displayName, {
        ok: false,
        error: `Dropped files must be valid base64 and no larger than ${Math.floor(MAX_STORE_DROPPED_BYTES / (1024 * 1024))} MiB.`,
      });
      return;
    }
    const bytes = Buffer.from(base64, "base64");
    if (bytes.length > MAX_STORE_DROPPED_BYTES) {
      this.postSettingsStoreResult(webview, displayName, {
        ok: false,
        error: `Dropped files are limited to ${Math.floor(MAX_STORE_DROPPED_BYTES / (1024 * 1024))} MiB. Use the file picker for a larger store.`,
      });
      return;
    }
    const config = getExplorerConfig();
    this.postSettingsStoreResult(
      webview,
      displayName,
      await decodeSettingsStoreBytes(config.cliPath, config.projectRoot, bytes),
    );
  }

  private async handleSettingsStoreDecodePath(raw: string | undefined): Promise<void> {
    const webview = this.webviewView?.webview;
    if (!webview || typeof raw !== "string" || !raw.trim()) {
      return;
    }
    let fsPath = raw.trim().split(/\r?\n/)[0].trim();
    try {
      if (/^[a-z][a-z0-9+.-]*:\/\//i.test(fsPath)) {
        fsPath = vscode.Uri.parse(fsPath).fsPath;
      }
    } catch {

    }
    const config = getExplorerConfig();
    this.postSettingsStoreResult(
      webview,
      path.basename(fsPath) || "store.renium",
      await decodeSettingsStoreToTree(config.cliPath, config.projectRoot, fsPath),
    );
  }

  private async handleSettingsStoreBrowse(): Promise<void> {
    const picked = await vscode.window.showOpenDialog({
      canSelectMany: false,
      openLabel: "Inspect",
      filters: { "Renium store": ["renium"], "All files": ["*"] },
    });
    if (picked && picked[0]) {
      await this.handleSettingsStoreDecodePath(picked[0].fsPath);
    }
  }

  private postSettingsStoreResult(webview: vscode.Webview, displayName: string, result: DecodeResult): void {
    if (!result.ok) {
      void webview.postMessage({ type: "storeTree", name: displayName, error: result.error });
      return;
    }
    void webview.postMessage({ type: "storeTree", name: displayName, result: result.tree });
  }

  private async setNodeExpanded(
    nodeId: string,
    expanded: boolean,
    mode?: ExplorerViewMode,
    start?: number,
    count?: number,
  ): Promise<void> {
    this.currentMode = mode ?? this.currentMode;
    try {
      const backend = this.currentMode === "search" ? this.searchBackend : this.backend;
      if (this.currentMode === "search") {
        await backend.ensureInitialized();
      }
      if (expanded) {
        await backend.expand(nodeId, this.currentMode);
      } else {
        await backend.collapse(nodeId, this.currentMode);
      }
      await this.requestRows(
        start ?? this.rowWindow.start,
        count ?? this.rowWindow.count,
        this.currentMode,
      );
    } catch (error) {
      if (expanded) {
        this.webviewView?.webview.postMessage({ type: "loadComplete", nodeId, ok: false });
      }
      const action = expanded ? "expand" : "collapse";
      vscode.window.showErrorMessage(
        `Failed to ${action} instance. ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  private async onMessage(message: {
    type?: string;
    nodeId?: string;
    targetId?: string;
    linkId?: string;
    className?: string;
    name?: string;
    newName?: string;
    query?: string;
    start?: number;
    count?: number;
    mode?: ExplorerViewMode;
    revision?: number;
    expanded?: boolean;
    command?: string;
    historyId?: string;
    historyIds?: string[];
    historyGroupId?: string;
    modelPaths?: string[];
    fetch?: boolean;
    action?: string;
    path?: string;
    projectRoot?: string;
    generation?: number;
    message?: string;
    base64?: string;
    node?: ReadonlyInstanceInfo;
  }): Promise<void> {
    if (!this.mutationAdmissionOpen && message.type && MUTATION_MESSAGE_TYPES.has(message.type)) {
      return;
    }
    const node = message.nodeId ? this.model.getNode(message.nodeId) : undefined;
    switch (message.type) {
      case "storeDecode":
        await this.handleSettingsStoreDecode(message.name, message.base64);
        break;
      case "storeDecodePath":
        await this.handleSettingsStoreDecodePath(message.path);
        break;
      case "storeBrowse":
        await this.handleSettingsStoreBrowse();
        break;
      case "storeSelect":
        this.propertiesProvider.showReadonlyInstance(message.node ?? {});
        break;
      case "ready":
        this.webviewReady = true;
        this.webviewView?.webview.postMessage({ type: "linkState", keys: this.linkState });
        this.webviewView?.webview.postMessage({ type: "packageDrag", link: this.externalPackageDrag ?? null });
        if (this.revealGitOnReady) {
          this.webviewView?.webview.postMessage({ type: "setTab", tab: "git" });
          this.revealGitOnReady = false;
          await this.refreshGit();
        }
        if (this.lastErrorMessage) {
          this.pushError(this.lastErrorMessage);
        } else {
          await this.requestRows(0, 120, this.currentMode);
        }
        return;
      case "getRows":
        this.currentMode = message.mode ?? this.currentMode;
        await this.requestRows(message.start ?? 0, message.count ?? 120, this.currentMode, false, false, message.revision);
        return;
      case "prefetchRows":
        void this.prefetchRows(message.start ?? 0, message.count ?? 700, message.mode ?? this.currentMode, message.revision);
        return;
      case "refresh":
        await this.refresh();
        return;
      case "gitReady":
        await this.refreshGit();
        return;
      case "packageDragDebug":
        logPackageDragDebug(`explorer.webview: ${message.message ?? ""}`);
        return;
      case "cancelPackageDrag":
        this.setExternalPackageDrag(undefined);
        return;
      case "gitRefresh":
        if (this.isCurrentGitContext(message.projectRoot, message.generation)) {
          await this.refreshGit({ fetch: message.fetch === true });
        }
        return;
      case "gitAction": {
        const projectRoot = message.projectRoot;
        if (
          this.actions.git
          && message.action
          && projectRoot
          && this.isCurrentGitContext(projectRoot, message.generation)
        ) {
          const generation = this.projectGeneration;
          this.gitLoading = true;
          this.postGitState();
          try {
            await this.actions.git.runAction(String(message.action), { projectRoot });
          } finally {
            if (this.isCurrentGitContext(projectRoot, generation)) {
              await this.refreshGit();
            }
          }
        }
        return;
      }
      case "gitOpenOutput":
        this.actions.git?.openOutput();
        return;
      case "gitDiff": {
        const projectRoot = message.projectRoot;
        if (
          this.actions.git
          && message.path
          && projectRoot
          && this.isCurrentGitContext(projectRoot, message.generation)
        ) {
          await this.actions.git.openDiff(String(message.path), { projectRoot });
        }
        return;
      }
      case "loadHistory":
        await this.postHistoryEntries();
        return;
      case "openHistoryBackup":
        try {
          await this.openHistoryBackup(message.historyId);
        } catch (error) {
          vscode.window.showErrorMessage(`Failed to open history backup. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "compareHistoryBackup":
        try {
          await this.compareHistoryBackup(message.historyId);
        } catch (error) {
          vscode.window.showErrorMessage(`Failed to compare history backup. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "restoreHistory":
        try {
          await this.restoreHistoryEntry(message.historyId);
        } catch (error) {
          this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", id: message.historyId });
          vscode.window.showErrorMessage(`Failed to restore history. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "restoreHistoryGroup":
        try {
          await this.restoreHistoryGroup(message.historyIds, message.historyGroupId);
        } catch (error) {
          this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", groupId: message.historyGroupId });
          vscode.window.showErrorMessage(`Failed to restore history group. ${error instanceof Error ? error.message : String(error)}`);
        }
        return;
      case "searchLoad":
        await this.loadSearchCorpus(message.query ?? "", message.revision, message.count);
        return;
      case "clearSearch":
        this.searchGeneration += 1;
        this.currentMode = "normal";
        await this.backend.clearSearch();
        if (this.searchBackend.hasInitialized()) {
          await this.searchBackend.clearSearch().catch(() => undefined);
        }
        const count = message.count ?? 120;
        let start = 0;
        if (this.selectedId) {
          const reveal = await this.backend.revealNode(this.selectedId);
          if (typeof reveal.rowIndex === "number") {
            start = Math.max(0, reveal.rowIndex - Math.floor(count / 2));
          }
        }
        await this.requestRows(start, count, "normal", !!this.selectedId);
        return;
      case "expandNode":
        if (message.nodeId) {
          await this.setNodeExpanded(message.nodeId, true, message.mode, message.start, message.count);
        }
        return;
      case "collapseNode":
        if (message.nodeId) {
          await this.setNodeExpanded(message.nodeId, false, message.mode, message.start, message.count);
        }
        return;
      case "selectNode":
        if (message.nodeId) {
          this.referencePreviewId = undefined;
          const previousSelectedId = this.selectedId;
          const service = this.serviceFromNodeId(message.nodeId);
          try {
            let loadedNode: FileExplorerNode;
            if (service && this.propertyOnlyStaleServices.has(service)) {
              await this.backend.reloadServices([service]);
              const details = await this.backend.selectDetails(message.nodeId);
              loadedNode = this.model.rememberNode(
                this.nodeFromBackend(details.details ?? { id: message.nodeId }),
                true,
              );
              this.propertyOnlyStaleServices.delete(service);
            } else {
              const details = await this.backend.selectDetails(message.nodeId);
              loadedNode = this.model.rememberNode(
                this.nodeFromBackend(details.details ?? { id: message.nodeId }),
                true,
              );
            }
            this.selectedId = message.nodeId;
            await this.propertiesProvider.show(loadedNode);
            this.actions.onSelectNode?.();
          } catch (error) {
            this.selectedId = previousSelectedId;
            throw error;
          }
        }
        return;
      case "openScript":
        await this.actions.openScript(node);
        return;
      case "addInstance":
        await this.actions.addInstance(node);
        return;
      case "createInstance":
        if (node && message.className) {
          await this.createInstance(node, message.className, message.name ?? message.className);
        }
        return;
      case "renameInstance":
        if (node && message.newName !== undefined) {
          await this.renameInstance(node, message.newName);
        }
        return;
      case "moveInstance":
        if (node && message.targetId) {
          const target = this.model.getNode(message.targetId);
          if (target) {
            await this.moveInstance(node, target);
          }
        }
        return;
      case "deleteInstance":
        if (message.nodeId) {
          await this.enqueueMutation(async () => {
            const current = this.model.getNode(message.nodeId ?? "");
            if (!current || current.kind === "service") {
              return;
            }
            await this.actions.deleteInstance(current);
            this.postOptimisticDelete(current.treeId);
            void this.refreshServices([current.service]);
          });
        }
        return;
      case "desyncPackageLink":
        if (node) {
          await this.actions.desyncPackageLink(node);
          await this.backend.reloadServices([node.service]);
          await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
        }
        return;
      case "copyInstance":
        this.copyInstance(node);
        return;
      case "pasteInstance":
        await this.pasteInstance(node);
        return;
      case "duplicateInstance":
        await this.duplicateInstance(node);
        return;
      case "importModel":
        await this.actions.importModel(node, message.modelPaths);
        return;
      case "exportModel":
        await this.actions.exportModel(node);
        return;
      case "createLink":
        if (node) {
          await this.actions.createLink(node);
        }
        return;
      case "resaveLink":
        if (node) {
          await this.actions.resaveLink(node);
        }
        return;
      case "relinkLink":
        if (node) {
          await this.actions.relinkLink(node);
        }
        return;
      case "insertPackage":
        if (message.linkId) {
          const drag = this.externalPackageDrag;
          const currentRoot = getExplorerConfig().projectRoot;
          if (
            !drag
            || drag.id !== String(message.linkId)
            || drag.generation !== this.projectGeneration
            || normalizeFilesystemPathKey(drag.projectRoot) !== normalizeFilesystemPathKey(currentRoot)
          ) {
            this.setExternalPackageDrag(undefined);
            throw new Error("The package selection belongs to a different Renium project.");
          }
          logPackageDragDebug(`explorer.host.onMessage insertPackage: link=${message.linkId} node=${message.nodeId ?? ""} name=${message.name ?? ""}`);
          this.setExternalPackageDrag(undefined);
          await this.actions.insertPackage(node, {
            id: String(message.linkId),
            name: typeof message.name === "string" ? message.name : undefined,
          });
          if (typeof message.nodeId === "string" && message.nodeId) {
            this.webviewView?.webview.postMessage({ type: "expandInserted", nodeId: message.nodeId });
          }
        }
        return;
      case "breakLink":
        if (node) {
          await this.actions.breakLink(node);
        }
        return;
      case "copyPath":
        await this.actions.copyPath(node);
        return;
      default:
        return;
    }
  }

  private async loadSearchCorpus(query: string, revision?: number, count?: number): Promise<void> {
    const projectGeneration = this.projectGeneration;
    const generation = ++this.searchGeneration;
    const searchId = generation;
    const trimmedQuery = query.trim();
    this.currentMode = trimmedQuery ? "search" : "normal";
    this.queuedRowRequest = undefined;
    this.rowRequestSerial += 1;
    try {
      if (!trimmedQuery) {
        await this.backend.clearSearch();
        if (projectGeneration !== this.projectGeneration) {
          return;
        }
        await this.requestRows(0, this.rowWindow.count, "normal");
        return;
      }
      this.webviewView?.webview.postMessage({ type: "searchStatus", loading: true, loaded: 0, total: 0, matchCount: 0 });
      if (this.searchBackend.hasPendingRequests()) {
        this.searchBackend.restart();
      }
      let lastError: unknown;
      for (let attempt = 0; attempt < 2; attempt += 1) {
        try {
          await this.searchBackend.ensureInitialized();
          const searchStatus = await this.searchBackend.searchStart(trimmedQuery, searchId);
          if (projectGeneration === this.projectGeneration && generation === this.searchGeneration) {
            const firstCount = Math.max(700, Math.min(1800, count ?? this.rowWindow.count));
            const searchRows = await this.searchBackend.getRows(0, firstCount, "search");
            if (
              projectGeneration === this.projectGeneration
              && generation === this.searchGeneration
              && this.currentMode === "search"
            ) {
              this.postRowsWindow(searchRows, false, revision);
              this.webviewView?.webview.postMessage({
                type: "searchStatus",
                loading: false,
                loaded: searchStatus.loaded,
                total: searchStatus.total,
                matchCount: searchStatus.matchCount,
              });
            }
          }
          return;
        } catch (error) {
          lastError = error;
          if (projectGeneration !== this.projectGeneration || generation !== this.searchGeneration) {
            return;
          }
          const message = error instanceof Error ? error.message : String(error);
          if (attempt === 0 && /Explorer backend exited|timed out|not running|restarted/i.test(message)) {
            this.searchBackend.restart();
            continue;
          }
          throw error;
        }
      }
      throw lastError instanceof Error ? lastError : new Error(String(lastError));
    } catch (error) {
      if (projectGeneration === this.projectGeneration && generation === this.searchGeneration) {
        this.webviewView?.webview.postMessage({ type: "searchStatus", loading: false });
        vscode.window.showErrorMessage(`Failed to load search results. ${error instanceof Error ? error.message : String(error)}`);
      }
    }
  }

  private async createInstance(parent: FileExplorerNode, className: string, name: string): Promise<void> {
    try {
      const created = await this.model.addInstance(parent, className, name);
      if (created) {
        this.referencePreviewId = undefined;
        this.selectedId = created.treeId;
        await this.propertiesProvider.show(created);
      }
      await this.backend.reloadServices([parent.service]);
      await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to add instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async renameInstance(node: FileExplorerNode, newName: string): Promise<void> {
    try {
      const loaded = await this.model.ensureLoaded(node);
      const oldLinkTargetPath = this.directReniumLinkTargetPath(loaded);
      const newLinkTargetPath = oldLinkTargetPath ? this.renamedPathForNode(loaded, newName) : undefined;
      const parent = loaded.parentTreeId ? this.model.getNode(loaded.parentTreeId) : undefined;
      if (oldLinkTargetPath && this.siblingNamed(parent, newName, loaded.treeId)) {
        vscode.window.showWarningMessage(
          `Linked package targets need unique sibling names. ${newName} already exists under ${parent?.name ?? loaded.service}.`,
        );
        return;
      }
      if (oldLinkTargetPath && newLinkTargetPath && this.hasLinkedTargetCollision(loaded.service, oldLinkTargetPath, newLinkTargetPath)) {
        vscode.window.showWarningMessage(
          `${newName} is already a linked package target under this parent. Rename one of them to a unique name before linking or deleting.`,
        );
        return;
      }
      const renamed = await this.model.renameInstance(loaded, newName);
      if (oldLinkTargetPath && newLinkTargetPath) {
        try {
          await this.moveReniumLinkTarget(loaded.service, oldLinkTargetPath, loaded.service, newLinkTargetPath);
        } catch (error) {
          if (renamed) {
            await this.model.renameInstance(renamed, loaded.name);
          }
          throw error;
        }
        void vscode.commands.executeCommand("renium.packages.refresh").then(
          () => undefined,
          () => undefined,
        );
      }
      if (renamed) {
        this.referencePreviewId = undefined;
        this.selectedId = renamed.treeId;
        await this.propertiesProvider.show(renamed);
      }
      await this.backend.reloadServices([loaded.service]);
      await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to rename instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async moveInstance(
    node: FileExplorerNode,
    target: FileExplorerNode,
  ): Promise<FileExplorerNode | undefined> {
    try {
      const loaded = await this.model.ensureLoaded(node);
      const loadedTarget = await this.model.ensureLoaded(target);
      const oldLinkTargetPath = this.directReniumLinkTargetPath(loaded);
      if (oldLinkTargetPath && this.siblingNamed(loadedTarget, loaded.name, loaded.treeId)) {
        vscode.window.showWarningMessage(
          `Linked package targets need unique sibling names. ${loaded.name} already exists under ${loadedTarget.name}.`,
        );
        return;
      }
      const newLinkTargetPath = oldLinkTargetPath ? this.childPathUnder(loadedTarget, loaded.name) : undefined;
      const oldParent = loaded.parentTreeId ? this.model.getNode(loaded.parentTreeId) : undefined;
      const moved = await this.model.moveInstance(loaded, loadedTarget);
      if (oldLinkTargetPath && newLinkTargetPath) {
        try {
          await this.moveReniumLinkTarget(loaded.service, oldLinkTargetPath, loadedTarget.service, newLinkTargetPath);
        } catch (error) {
          if (moved && oldParent) {
            await this.model.moveInstance(moved, oldParent);
          }
          throw error;
        }
        void vscode.commands.executeCommand("renium.packages.refresh").then(
          () => undefined,
          () => undefined,
        );
      }
      if (moved) {
        this.referencePreviewId = undefined;
        this.selectedId = moved.treeId;
        await this.propertiesProvider.show(moved);
      }
      await this.backend.reloadServices(Array.from(new Set([loaded.service, loadedTarget.service])));
      await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
      return moved;
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to move instance. ${error instanceof Error ? error.message : String(error)}`);
      return undefined;
    }
  }

  private copyInstance(node?: FileExplorerNode): void {
    if (!node || node.kind === "service") {
      return;
    }
    this.clipboardNodeId = node.treeId;
    this.postClipboardState();
  }

  private async pasteInstance(parent?: FileExplorerNode): Promise<void> {
    if (!parent || !this.clipboardNodeId) {
      return;
    }
    const source = this.model.getNode(this.clipboardNodeId);
    if (!source) {
      this.clipboardNodeId = undefined;
      this.postClipboardState();
      vscode.window.showWarningMessage("Copied instance no longer exists.");
      return;
    }
    try {
      const created = await this.model.cloneInstance(source, parent);
      if (created) {
        this.referencePreviewId = undefined;
        this.selectedId = created.treeId;
        await this.propertiesProvider.show(created);
      }
      await this.backend.reloadServices([parent.service]);
      await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to paste instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async duplicateInstance(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service" || !node.parentTreeId) {
      return;
    }
    const parent = this.model.getNode(node.parentTreeId);
    if (!parent) {
      return;
    }
    try {
      const created = await this.model.cloneInstance(node, parent);
      if (created) {
        this.referencePreviewId = undefined;
        this.selectedId = created.treeId;
        await this.propertiesProvider.show(created);
      }
      await this.backend.reloadServices([node.service]);
      await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to duplicate instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private readHistoryEntries(limit = 5000): ExplorerHistoryEntry[] {
    const config = getExplorerConfig();
    const historyRoot = editorHistoryRoot(config);
    if (!fs.existsSync(historyRoot)) {
      return [];
    }

    const entries: ExplorerHistoryEntry[] = [];
    const dirents = fs.readdirSync(historyRoot, { withFileTypes: true })
      .filter((dirent) => dirent.isDirectory())
      .sort((a, b) => b.name.localeCompare(a.name))
      .slice(0, limit);
    for (const dirent of dirents) {
      if (!dirent.isDirectory()) {
        continue;
      }
      const entryDir = path.join(historyRoot, dirent.name);
      const manifestPath = path.join(entryDir, "manifest.json");
      if (!pathInsideRoot(historyRoot, manifestPath) || !fs.existsSync(manifestPath)) {
        continue;
      }
      try {
        const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8")) as EditorHistoryManifest;
        const service = String(manifest.service ?? "").trim();
        if (!service) {
          continue;
        }
        const createdUnixMs = Number(manifest.createdUnixMs ?? 0);
        const pathSegments = Array.isArray(manifest.pathSegments)
          ? manifest.pathSegments.map((segment) => String(segment))
          : [];
        const sourcePath = typeof manifest.sourcePath === "string" ? manifest.sourcePath : undefined;
        const settingsFile = typeof manifest.settingsFile === "string" ? manifest.settingsFile : undefined;
        const settingsId = typeof manifest.settingsId === "string" ? manifest.settingsId : undefined;
        const className = typeof manifest.className === "string" ? manifest.className : "Instance";
        const propertyName = typeof manifest.propertyName === "string" ? manifest.propertyName : undefined;
        const propertyLabel = typeof manifest.propertyLabel === "string" ? manifest.propertyLabel : propertyName;
        const baseTargetLabel = sourcePath || pathSegments.join(".") || settingsId || service;
        const targetLabel = propertyLabel ? `${baseTargetLabel} · ${propertyLabel}` : baseTargetLabel;
        const sourceBackupPath = typeof manifest.sourceBackup === "string" ? path.join(entryDir, manifest.sourceBackup) : undefined;
        const settingsBackupPath = typeof manifest.settingsBackup === "string" ? path.join(entryDir, manifest.settingsBackup) : undefined;
        entries.push({
          id: dirent.name,
          service,
          className,
          settingsId,
          sourcePath,
          settingsFile,
          pathSegments,
          propertyName,
          propertyLabel,
          createdUnixMs,
          createdLabel: Number.isFinite(createdUnixMs) && createdUnixMs > 0
            ? new Date(createdUnixMs).toLocaleString()
            : dirent.name,
          targetLabel,
          hasSourceBackup: sourceBackupPath !== undefined && pathInsideRoot(historyRoot, sourceBackupPath) && fs.existsSync(sourceBackupPath),
          hasSettingsBackup: settingsBackupPath !== undefined && pathInsideRoot(historyRoot, settingsBackupPath) && fs.existsSync(settingsBackupPath),
        });
      } catch {
        continue;
      }
    }

    return entries
      .sort((a, b) => b.createdUnixMs - a.createdUnixMs)
      .slice(0, limit);
  }

  private readHistoryGroups(groupLimit = 120): ExplorerHistoryGroup[] {
    const entries = this.readHistoryEntries();
    const groups: ExplorerHistoryGroup[] = [];
    let current: ExplorerHistoryEntry[] = [];

    const flush = (): void => {
      if (current.length === 0 || groups.length >= groupLimit) {
        current = [];
        return;
      }
      groups.push(this.historyGroupFromEntries(current));
      current = [];
    };

    for (const entry of entries) {
      const previous = current[current.length - 1];
      if (previous && previous.createdUnixMs - entry.createdUnixMs > 60_000) {
        flush();
      }
      if (groups.length >= groupLimit) {
        break;
      }
      current.push(entry);
    }
    flush();

    return groups;
  }

  private historyGroupFromEntries(entries: ExplorerHistoryEntry[]): ExplorerHistoryGroup {
    const newest = entries[0];
    const oldest = entries[entries.length - 1];
    const targets = new Map<string, ExplorerHistoryTarget>();

    for (const entry of entries) {
      const key = this.historyTargetKey(entry);
      const existing = targets.get(key);
      if (!existing) {
        targets.set(key, {
          ...entry,
          openId: entry.id,
          restoreId: entry.id,
          editCount: 1,
          firstCreatedUnixMs: entry.createdUnixMs,
          lastCreatedUnixMs: entry.createdUnixMs,
          timeLabel: this.historyTimeLabel(entry.createdUnixMs, entry.createdUnixMs),
        });
        continue;
      }
      existing.editCount += 1;
      existing.firstCreatedUnixMs = Math.min(existing.firstCreatedUnixMs, entry.createdUnixMs);
      existing.lastCreatedUnixMs = Math.max(existing.lastCreatedUnixMs, entry.createdUnixMs);
      existing.restoreId = entry.createdUnixMs < existing.firstCreatedUnixMs ? entry.id : existing.restoreId;
      if (entry.createdUnixMs <= existing.firstCreatedUnixMs) {
        existing.restoreId = entry.id;
      }
      if (!existing.hasSourceBackup && entry.hasSourceBackup) {
        existing.openId = entry.id;
      }
      existing.hasSourceBackup = existing.hasSourceBackup || entry.hasSourceBackup;
      existing.hasSettingsBackup = existing.hasSettingsBackup || entry.hasSettingsBackup;
      existing.timeLabel = this.historyTimeLabel(existing.firstCreatedUnixMs, existing.lastCreatedUnixMs);
    }

    const items = Array.from(targets.values()).sort((a, b) => b.lastCreatedUnixMs - a.lastCreatedUnixMs);
    const services = Array.from(new Set(items.map((item) => item.service))).sort();
    const title = items.length === 1
      ? items[0].targetLabel
      : `${entries.length} edits across ${items.length} items`;
    const subtitleParts = [
      this.historyTimeLabel(oldest.createdUnixMs, newest.createdUnixMs),
      services.join(", "),
    ].filter((part) => part.length > 0);

    return {
      id: `${newest.createdUnixMs}-${oldest.createdUnixMs}-${entries.length}-${items.length}`,
      title,
      subtitle: subtitleParts.join(" · "),
      createdUnixMs: newest.createdUnixMs,
      firstCreatedUnixMs: oldest.createdUnixMs,
      lastCreatedUnixMs: newest.createdUnixMs,
      entryCount: entries.length,
      targetCount: items.length,
      services,
      items,
    };
  }

  private historyTargetKey(entry: ExplorerHistoryEntry): string {
    const target = entry.sourcePath ?? entry.settingsId ?? entry.pathSegments.join(".") ?? entry.id;
    return entry.propertyName ? `${target}#${entry.propertyName}` : target;
  }

  private historyTimeLabel(startUnixMs: number, endUnixMs: number): string {
    if (!Number.isFinite(startUnixMs) || startUnixMs <= 0) {
      return "";
    }
    const start = new Date(startUnixMs);
    const end = new Date(endUnixMs);
    const date = end.toLocaleDateString();
    const startTime = start.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
    const endTime = end.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
    return startUnixMs === endUnixMs ? `${date} ${endTime}` : `${date} ${startTime}-${endTime}`;
  }

  private readHistoryManifest(id: string | undefined): { manifest: EditorHistoryManifest; entryDir: string } | undefined {
    const safeId = String(id ?? "").trim();
    if (!safeId || safeId.includes("/") || safeId.includes("\\")) {
      return undefined;
    }
    const config = getExplorerConfig();
    const historyRoot = editorHistoryRoot(config);
    const entryDir = path.join(historyRoot, safeId);
    const manifestPath = path.join(entryDir, "manifest.json");
    if (!pathInsideRoot(historyRoot, manifestPath) || !fs.existsSync(manifestPath)) {
      return undefined;
    }
    return {
      manifest: JSON.parse(fs.readFileSync(manifestPath, "utf8")) as EditorHistoryManifest,
      entryDir,
    };
  }

  private async postHistoryEntries(): Promise<void> {
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    try {
      this.webviewView.webview.postMessage({
        type: "historyEntries",
        groups: this.readHistoryGroups(),
      });
    } catch (error) {
      this.webviewView.webview.postMessage({
        type: "historyError",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }

  private async openHistoryBackup(id: string | undefined): Promise<void> {
    const data = this.readHistoryManifest(id);
    const sourceBackup = data?.manifest.sourceBackup;
    if (!data || typeof sourceBackup !== "string") {
      return;
    }
    const sourcePath = path.join(data.entryDir, sourceBackup);
    if (!pathInsideRoot(editorHistoryRoot(getExplorerConfig()), sourcePath) || !fs.existsSync(sourcePath)) {
      vscode.window.showWarningMessage("History source backup was not found.");
      return;
    }
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(sourcePath));
    await vscode.window.showTextDocument(document, { preview: true });
  }

  private async compareHistoryBackup(id: string | undefined): Promise<void> {
    const data = this.readHistoryManifest(id);
    if (!data) {
      return;
    }
    const manifest = data.manifest;
    const sourceBackup = manifest.sourceBackup;
    const sourcePath = typeof manifest.sourcePath === "string" ? manifest.sourcePath : undefined;
    if (typeof sourceBackup !== "string" || !sourcePath) {
      return;
    }
    const config = getExplorerConfig();
    const backupPath = path.join(data.entryDir, sourceBackup);
    const currentPath = path.isAbsolute(sourcePath)
      ? path.normalize(sourcePath)
      : path.normalize(path.join(config.projectRoot, sourcePath));
    if (!pathInsideRoot(editorHistoryRoot(config), backupPath) || !fs.existsSync(backupPath)) {
      vscode.window.showWarningMessage("History source backup was not found.");
      return;
    }
    if (!projectGraphOwnsPath(config, currentPath) || !fs.existsSync(currentPath)) {
      await this.openHistoryBackup(id);
      return;
    }
    const label = Array.isArray(manifest.pathSegments) && manifest.pathSegments.length > 0
      ? manifest.pathSegments.join(".")
      : path.basename(currentPath);
    await vscode.commands.executeCommand(
      "vscode.diff",
      vscode.Uri.file(backupPath),
      vscode.Uri.file(currentPath),
      `History: ${label}`,
    );
  }

  private async restoreHistoryEntry(id: string | undefined): Promise<void> {
    const data = this.readHistoryManifest(id);
    if (!data) {
      vscode.window.showWarningMessage("History entry was not found.");
      return;
    }
    const manifest = data.manifest;
    const service = String(manifest.service ?? "").trim();
    const sourcePath = typeof manifest.sourcePath === "string" ? manifest.sourcePath : undefined;
    const settingsId = typeof manifest.settingsId === "string" ? manifest.settingsId : undefined;
    if (!service || (!sourcePath && !settingsId)) {
      vscode.window.showWarningMessage("This history entry can't be restored — it no longer points at a file or instance.");
      return;
    }
    const targetLabel = sourcePath || (Array.isArray(manifest.pathSegments) ? manifest.pathSegments.join(".") : settingsId) || service;
    const picked = await vscode.window.showWarningMessage(
      `Restore editor history for ${targetLabel}?`,
      { modal: true },
      "Restore",
    );
    if (picked !== "Restore") {
      this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", id });
      return;
    }

    await this.restoreHistoryIds([String(id ?? "")], id);
  }

  private async restoreHistoryGroup(ids: string[] | undefined, groupId: string | undefined): Promise<void> {
    const restoreIds = Array.from(new Set((Array.isArray(ids) ? ids : [])
      .map((id) => String(id).trim())
      .filter((id) => id.length > 0)));
    if (restoreIds.length === 0) {
      vscode.window.showWarningMessage("History group has no restore targets.");
      return;
    }
    const picked = await vscode.window.showWarningMessage(
      `Restore ${restoreIds.length} history item${restoreIds.length === 1 ? "" : "s"} from this edit session?`,
      { modal: true },
      "Restore",
    );
    if (picked !== "Restore") {
      this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", groupId });
      return;
    }

    await this.restoreHistoryIds(restoreIds, groupId, true);
  }

  private restoreHistoryFile(
    config: ExplorerConfig,
    historyRoot: string,
    entryDir: string,
    backupPath: string,
    destination: string,
  ): string | undefined {
    const source = path.join(entryDir, backupPath);
    if (!pathInsideRoot(historyRoot, source) || !fs.existsSync(source)) {
      return undefined;
    }
    if (!projectGraphOwnsPath(config, destination)) {
      throw new Error(`Refusing to restore history outside project sources: ${destination}`);
    }
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.copyFileSync(source, destination);
    return destination;
  }

  private async restoreHistoryIds(ids: string[], completionId?: string, isGroup = false): Promise<void> {
    const config = getExplorerConfig();
    const historyRoot = editorHistoryRoot(config);
    const changedPaths: string[] = [];
    const changedServices = new Set<string>();

    for (const id of ids) {
      const data = this.readHistoryManifest(id);
      if (!data) {
        continue;
      }
      const manifest = data.manifest;
      const service = String(manifest.service ?? "").trim();
      const sourcePath = typeof manifest.sourcePath === "string" ? manifest.sourcePath : undefined;
      const settingsId = typeof manifest.settingsId === "string" ? manifest.settingsId : undefined;
      if (!service || (!sourcePath && !settingsId)) {
        continue;
      }
      changedServices.add(service);

      if (typeof manifest.settingsBackup === "string") {
        const configuredSettingsFile = typeof manifest.settingsFile === "string"
          ? manifest.settingsFile
          : settingsFileForService(config, service);
        const destination = path.isAbsolute(configuredSettingsFile)
          ? path.normalize(configuredSettingsFile)
          : path.normalize(path.join(config.projectRoot, configuredSettingsFile));
        const restored = this.restoreHistoryFile(
          config,
          historyRoot,
          data.entryDir,
          manifest.settingsBackup,
          destination,
        );
        if (restored) {
          changedPaths.push(restored);
        }
      }

      if (typeof manifest.sourceBackup === "string" && sourcePath) {
        const destination = path.isAbsolute(sourcePath)
          ? path.normalize(sourcePath)
          : path.normalize(path.join(config.projectRoot, sourcePath));
        const restored = this.restoreHistoryFile(
          config,
          historyRoot,
          data.entryDir,
          manifest.sourceBackup,
          destination,
        );
        if (restored) {
          changedPaths.push(restored);
        }
      }
    }

    for (const service of this.model.servicesFromSettingsFiles(changedPaths)) {
      changedServices.add(service);
    }
    const services = Array.from(changedServices);
    if (services.length > 0) {
      await this.backend.reloadServices(services);
      if (this.searchBackend.hasInitialized()) {
        await this.searchBackend.reloadServices(services).catch(() => undefined);
      }
      await this.propertiesProvider.refreshCurrentForServices(services);
      await this.requestRows(this.rowWindow.start, this.rowWindow.count, this.currentMode);
    }
    this.webviewView?.webview.postMessage({ type: "historyRestoreComplete", id: completionId, groupId: isGroup ? completionId : undefined });
    vscode.window.showInformationMessage(isGroup ? "History session restored locally." : "History entry restored locally.");

    if (changedPaths.length > 0) {
      const uniqueChangedPaths = Array.from(new Set(changedPaths));
      void vscode.commands.executeCommand("renium.pushEditorPathsNow", uniqueChangedPaths, {
        projectRoot: config.projectRoot,
        pendingServices: [...changedServices],
        skipChangeFilter: true,
        taskName: "History restore -> Studio sync",
      }).then(
        undefined,
        (error) => vscode.window.showErrorMessage(`Failed to push restored history. ${error instanceof Error ? error.message : String(error)}`),
      );
    }
  }

  private html(assetBase: string): string {
    return fileExplorerWebviewHtml({
      assetBase,
      classNames: ROBLOX_CLASS_NAMES,
      availableIconNames: this.availableIconNames,
      initialRows: this.staticRootRows(assetBase),
      maxStoreDroppedBytes: MAX_STORE_DROPPED_BYTES,
    });
  }

  private staticRootRows(assetBase: string): string {
    const roots = this.model.sort(this.model.getRoots());
    if (roots.length === 0) {
      return "";
    }
    return roots
      .map((node) => {
        const selected = this.selectedId === node.treeId ? " selected" : "";
        const twisty = node.hasChildren || node.children.length > 0 ? "" : " leaf";
        const iconName = iconAssetNameForClass(node.className, this.availableIconNames);
        return [
          `<div class="row${selected}" data-id="${escapeHtml(node.treeId)}" draggable="false" style="padding-left:0px">`,
          `<span class="twisty${twisty}"></span>`,
          `<img class="icon" src="${escapeHtml(assetBase)}/${escapeHtml(iconName)}.png">`,
          `<span class="name">${escapeHtml(node.name)}</span>`,
          "</div>",
        ].join("");
      })
      .join("");
  }
}
