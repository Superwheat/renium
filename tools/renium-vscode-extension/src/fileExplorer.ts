import * as crypto from "crypto";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import { type GitViewActions } from "./gitView";
import { ROBLOX_CLASS_NAMES } from "./robloxClasses";
import { invalidateProjectSourceGraph, loadProjectSourceGraph } from "./sharedConfig";
import {
  SETTINGS_FILE_NAME,
  ensureModelFileExtension,
  isReniumSettingsFileName,
  isScriptClass,
  robloxModelFormatFromPath,
  tabInputUris,
  type RobloxModelFormat,
} from "./utils";
import { FileExplorerModel } from "./fileExplorerModel";
import { FileExplorerViewProvider } from "./fileExplorerView";
import { FilePropertiesViewProvider } from "./filePropertiesView";
import {
  type ExplorerConfig,
  type FileExplorerNode,
  type PackagePropertiesPayload,
  type ReadonlyInstanceInfo,
  type ViewVisibilityHandler,
  ReadonlyExplorerScriptContentProvider,
  configureExplorerExtensionRoot,
  getExplorerConfig,
  linkPathKey,
  logPackageDragDebug,
  nodeLinkPathKey,
  nodeTargetPath,
  normalizeFilesystemPathKey,
  normalizeId,
  normalizeRobloxModelPaths,
  safeModelFileName,
} from "./fileExplorerCore";

export {
  iconAssetNameForClass,
  loadAssetIconNames,
  logPackageDragDebug,
} from "./fileExplorerCore";
export type { ExplorerConfig, FileExplorerNode } from "./fileExplorerCore";

const RENIUM_ACTIVITY_VISIBLE_STATE_KEY = "renium.activityVisible";

export class FileExplorerController implements vscode.Disposable {
  private readonly model = new FileExplorerModel();
  private readonly propertiesProvider: FilePropertiesViewProvider;
  private readonly explorerProvider: FileExplorerViewProvider;
  private readonly readonlyScriptProvider = new ReadonlyExplorerScriptContentProvider();
  private readonly explorerSelectionEmitter = new vscode.EventEmitter<void>();
  public readonly onDidSelectExplorerNode = this.explorerSelectionEmitter.event;
  private readonly disposables: vscode.Disposable[] = [];
  private settingsWatchers: vscode.Disposable[] = [];
  private graphWatchers: vscode.Disposable[] = [];
  private readonly pendingSettingsRefreshes = new Set<string>();
  private readonly propertyOnlySettingsRefreshUntil = new Map<string, number>();
  private readonly visibleViewTypes = new Set<string>();
  private linkState: Record<string, string> = {};
  private settingsRefreshTimer: NodeJS.Timeout | undefined;
  private graphRefreshTimer: NodeJS.Timeout | undefined;
  private graphRefreshPromise: Promise<void> = Promise.resolve();
  private projectGraphFingerprint = "";
  private startupRestoreTimer: NodeJS.Timeout | undefined;
  private settingsRefreshInFlightGeneration: number | undefined;
  private settingsRefreshAgainGeneration: number | undefined;
  private projectGeneration = 0;
  private projectSwitchPrepared = false;
  private mutationAdmissionOpen = true;
  private readonly activeMutationCommands = new Set<Promise<void>>();
  private reniumActivityVisible = false;
  private observedVisibleViewThisSession = false;

  public showSettingsStorePropertiesReadonly(node: ReadonlyInstanceInfo): void {
    this.propertiesProvider.showReadonlyInstance(node);
  }

  public constructor(private readonly context: vscode.ExtensionContext, private readonly git?: GitViewActions) {
    configureExplorerExtensionRoot(context.extensionPath);
    this.reniumActivityVisible = context.workspaceState.get<boolean>(RENIUM_ACTIVITY_VISIBLE_STATE_KEY, false) === true;
    const onVisibilityChanged: ViewVisibilityHandler = (viewType, visible) => this.onViewVisibilityChanged(viewType, visible);
    this.propertiesProvider = new FilePropertiesViewProvider(this.model, context.extensionUri, onVisibilityChanged);
    this.explorerProvider = new FileExplorerViewProvider(this.model, context.extensionUri, this.propertiesProvider, {
      openScript: (node) => this.openScript(node),
      addInstance: (node) => this.addInstance(node),
      deleteInstance: (node) => this.deleteInstance(node),
      desyncPackageLink: (node) => this.desyncPackageLink(node),
      copyPath: (node) => this.copyPath(node),
      importModel: (node, modelPaths) => this.importModel(node, modelPaths),
      exportModel: (node) => this.exportModel(node),
      createLink: (node) => this.createLink(node),
      resaveLink: (node) => this.updateLink(node, "resave"),
      relinkLink: (node) => this.updateLink(node, "relink"),
      breakLink: (node) => this.breakLinkNode(node),
      insertPackage: (node, link) => this.insertPackage(node, link),
      onSelectNode: () => this.explorerSelectionEmitter.fire(),
      git: this.git,
    }, onVisibilityChanged);
    this.propertiesProvider.setReferenceRevealHandler((nodeId) => this.explorerProvider.revealReference(nodeId));
    this.disposables.push(
      this.readonlyScriptProvider,
      vscode.workspace.registerTextDocumentContentProvider("renium-readonly-script", this.readonlyScriptProvider),
      this.explorerProvider,
      vscode.window.registerWebviewViewProvider(FileExplorerViewProvider.viewType, this.explorerProvider, {
        webviewOptions: { retainContextWhenHidden: false },
      }),
      vscode.window.registerWebviewViewProvider(FilePropertiesViewProvider.viewType, this.propertiesProvider, {
        webviewOptions: { retainContextWhenHidden: false },
      }),
      vscode.commands.registerCommand("renium.fileExplorer.refresh", () => this.explorerProvider.refresh()),
      vscode.commands.registerCommand("renium.fileExplorer.prepareProjectSwitch", () => this.prepareProjectSwitch()),
      vscode.commands.registerCommand("renium.fileExplorer.cancelProjectSwitch", () => this.cancelProjectSwitch()),
      vscode.commands.registerCommand("renium.fileExplorer.switchProject", () => this.switchProject()),
      vscode.commands.registerCommand("renium.fileExplorer.refreshProjectGraph", () => this.refreshProjectGraph()),
      vscode.commands.registerCommand("renium.fileExplorer.showGit", () => this.explorerProvider.showGit()),
      vscode.commands.registerCommand("renium.fileExplorer.refreshGit", (options?: { fetch?: boolean }) =>
        this.explorerProvider.refreshGit(options ?? {}),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.openScript", (node?: FileExplorerNode) => this.openScript(node)),
      vscode.commands.registerCommand(
        "renium.fileExplorer.revealStudioScript",
        (action?: { service?: string; settingsId?: string; pathSegments?: string[]; pathOrdinals?: number[] }) =>
          this.revealStudioScript(action),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.addInstance", (node?: FileExplorerNode) =>
        this.runMutationCommand(() => this.addInstance(node))),
      vscode.commands.registerCommand("renium.fileExplorer.deleteInstance", (node?: FileExplorerNode) =>
        this.runMutationCommand(() => this.deleteInstance(node))),
      vscode.commands.registerCommand("renium.fileExplorer.desyncPackageLink", (node?: FileExplorerNode) =>
        this.runMutationCommand(() => this.desyncPackageLink(node))),
      vscode.commands.registerCommand("renium.fileExplorer.copyPath", (node?: FileExplorerNode) => this.copyPath(node)),
      vscode.commands.registerCommand("renium.fileExplorer.importModel", (node?: FileExplorerNode) =>
        this.runMutationCommand(() => this.importModel(node))),
      vscode.commands.registerCommand("renium.fileExplorer.exportModel", (node?: FileExplorerNode) => this.exportModel(node)),
      vscode.commands.registerCommand("renium.fileExplorer.refreshServices", (services?: string[]) =>
        this.explorerProvider.refreshServices(Array.isArray(services) ? services : []),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.refreshPropertyChanges", (settingsFiles?: string[]) =>
        this.refreshPropertyChanges(Array.isArray(settingsFiles) ? settingsFiles : []),
      ),
      vscode.commands.registerCommand(
        "renium.fileExplorer.lookupPropertyValues",
        (requests?: Array<{ service?: string; settingsId?: string; scope?: string; property?: string }>) =>
          this.model.lookupPropertyValues(Array.isArray(requests) ? requests : []),
      ),
      vscode.commands.registerCommand("renium.properties.showPackageNode", (payload?: PackagePropertiesPayload) =>
        this.propertiesProvider.showReadonlyPackage(payload),
      ),
      vscode.commands.registerCommand("renium.properties.clearPackageNode", () =>
        this.propertiesProvider.clearReadonlyPackage(),
      ),
      vscode.commands.registerCommand("renium.fileExplorer.setLinkState", (keys?: Record<string, string>) => {
        this.linkState = keys ?? {};
        this.explorerProvider.setLinkState(this.linkState);
      }),
    );
    this.startSettingsWatcher();
    this.startProjectGraphWatcher();
    this.scheduleStartupActivityRestore();
  }

  public dispose(): void {
    if (this.settingsRefreshTimer) {
      clearTimeout(this.settingsRefreshTimer);
      this.settingsRefreshTimer = undefined;
    }
    if (this.startupRestoreTimer) {
      clearTimeout(this.startupRestoreTimer);
      this.startupRestoreTimer = undefined;
    }
    if (this.graphRefreshTimer) {
      clearTimeout(this.graphRefreshTimer);
      this.graphRefreshTimer = undefined;
    }
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    for (const watcher of this.settingsWatchers) {
      watcher.dispose();
    }
    for (const watcher of this.graphWatchers) {
      watcher.dispose();
    }
    this.explorerSelectionEmitter.dispose();
  }

  public async prepareShutdown(): Promise<void> {
    await this.prepareProjectSwitch();
  }

  public setExternalPackageDrag(link?: { id: string; name?: string; mode?: string }): void {
    this.explorerProvider.setExternalPackageDrag(link);
  }

  public clearExplorerSelection(): void {
    this.explorerProvider.clearSelection();
  }

  private normalizedFileKey(filePath: string): string {
    return normalizeFilesystemPathKey(filePath);
  }

  private async closeSourceTabs(sourcePaths: string[] | undefined): Promise<boolean> {
    const pathKeys = new Set(
      (sourcePaths ?? [])
        .map((sourcePath) => String(sourcePath || "").trim())
        .filter((sourcePath) => sourcePath.length > 0)
        .map((sourcePath) => this.normalizedFileKey(sourcePath)),
    );
    if (pathKeys.size === 0) {
      return true;
    }
    const tabs: vscode.Tab[] = [];
    for (const group of vscode.window.tabGroups.all) {
      for (const tab of group.tabs) {
        if (tabInputUris(tab.input, "file").some((uri) => pathKeys.has(this.normalizedFileKey(uri.fsPath)))) {
          tabs.push(tab);
        }
      }
    }
    if (tabs.length > 0) {
      return vscode.window.tabGroups.close(tabs, true);
    }
    return true;
  }

  private scheduleStartupActivityRestore(): void {
    if (!this.reniumActivityVisible) {
      return;
    }
    this.startupRestoreTimer = setTimeout(() => {
      this.startupRestoreTimer = undefined;
      if (!this.reniumActivityVisible) {
        return;
      }
      void vscode.commands.executeCommand("workbench.view.extension.reniumContainer");
    }, 100);
  }

  private onViewVisibilityChanged(viewType: string, visible: boolean): void {
    if (visible) {
      this.visibleViewTypes.add(viewType);
      this.observedVisibleViewThisSession = true;
    } else {
      this.visibleViewTypes.delete(viewType);
      if (!this.observedVisibleViewThisSession && this.reniumActivityVisible) {
        return;
      }
    }
    const nextVisible = this.visibleViewTypes.size > 0;
    if (nextVisible === this.reniumActivityVisible) {
      return;
    }
    this.reniumActivityVisible = nextVisible;
    void this.context.workspaceState.update(RENIUM_ACTIVITY_VISIBLE_STATE_KEY, nextVisible);
  }

  private startSettingsWatcher(): void {
    for (const watcher of this.settingsWatchers) {
      watcher.dispose();
    }
    this.settingsWatchers = [];
    const generation = this.projectGeneration;
    let config: ExplorerConfig;
    try {
      config = getExplorerConfig();
    } catch (error) {
      vscode.window.showErrorMessage(
        `Could not watch project files. ${error instanceof Error ? error.message : String(error)}`,
      );
      return;
    }
    const graph = loadProjectSourceGraph(config.projectRoot);
    const directories = Array.from(new Set(graph.directories.map(path.normalize)));
    const watcherPatterns = directories.flatMap((root) =>
      [SETTINGS_FILE_NAME, `**/${SETTINGS_FILE_NAME}`].map((pattern) => ({ root, pattern })));
    for (const filePath of graph.files) {
      if (isReniumSettingsFileName(path.basename(filePath))) {
        watcherPatterns.push({ root: path.dirname(filePath), pattern: path.basename(filePath) });
      }
    }
    const watcherKeys = new Set<string>();
    const watchers = watcherPatterns
      .filter(({ root, pattern }) => {
        const key = `${normalizeFilesystemPathKey(root)}\0${pattern.toLowerCase()}`;
        if (watcherKeys.has(key)) {
          return false;
        }
        watcherKeys.add(key);
        return true;
      })
      .map(({ root, pattern }) =>
        vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(root, pattern)));
    const queue = (uri: vscode.Uri): void => {
      if (generation === this.projectGeneration && uri.scheme === "file") {
        this.queueSettingsRefresh(uri.fsPath);
      }
    };
    for (const watcher of watchers) {
      watcher.onDidCreate(queue);
      watcher.onDidChange(queue);
      watcher.onDidDelete(queue);
    }
    this.settingsWatchers = watchers;
  }

  private graphFingerprint(projectRoot: string): string {
    const graph = loadProjectSourceGraph(projectRoot);
    const manifests = Array.from(new Set([
      ...graph.manifests,
      path.join(projectRoot, "renium.project.jsonc"),
      path.join(projectRoot, "renium.project.json"),
    ])).map(path.normalize).sort();
    return manifests.map((manifest) => {
      try {
        const content = fs.readFileSync(manifest);
        return `${manifest}\0${content.length}\0${crypto.createHash("sha256").update(content).digest("hex")}`;
      } catch {
        return `${manifest}\0-`;
      }
    }).join("\n");
  }

  private startProjectGraphWatcher(): void {
    for (const watcher of this.graphWatchers) {
      watcher.dispose();
    }
    this.graphWatchers = [];
    let config: ExplorerConfig;
    try {
      config = getExplorerConfig();
    } catch {
      this.projectGraphFingerprint = "";
      return;
    }
    const graph = loadProjectSourceGraph(config.projectRoot);
    const manifests = Array.from(new Set([
      ...graph.manifests,
      path.join(config.projectRoot, "renium.project.jsonc"),
      path.join(config.projectRoot, "renium.project.json"),
    ].map(path.normalize)));
    const generation = this.projectGeneration;
    const queue = (): void => {
      if (generation !== this.projectGeneration) {
        return;
      }
      if (this.graphRefreshTimer) {
        clearTimeout(this.graphRefreshTimer);
      }
      this.graphRefreshTimer = setTimeout(() => {
        this.graphRefreshTimer = undefined;
        void this.refreshProjectGraph();
      }, 100);
    };
    this.graphWatchers = manifests.map((manifest) => {
      const watcher = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(path.dirname(manifest), path.basename(manifest)),
      );
      watcher.onDidCreate(queue);
      watcher.onDidChange(queue);
      watcher.onDidDelete(queue);
      return watcher;
    });
    this.projectGraphFingerprint = this.graphFingerprint(config.projectRoot);
  }

  private refreshProjectGraph(): Promise<void> {
    const run = this.graphRefreshPromise.then(async () => {
      const config = getExplorerConfig();
      invalidateProjectSourceGraph(config.projectRoot);
      const fingerprint = this.graphFingerprint(config.projectRoot);
      if (fingerprint === this.projectGraphFingerprint) {
        return;
      }
      await this.switchProject();
      await vscode.commands.executeCommand("renium.projectGraphChanged", config.projectRoot);
    });
    this.graphRefreshPromise = run.catch(() => undefined);
    return run;
  }

  private async runMutationCommand(task: () => Promise<void>): Promise<void> {
    if (!this.mutationAdmissionOpen) {
      return;
    }
    const generation = this.projectGeneration;
    const run = (async () => {
      if (generation !== this.projectGeneration || !this.mutationAdmissionOpen) {
        return;
      }
      await task();
    })();
    this.activeMutationCommands.add(run);
    try {
      await run;
    } finally {
      this.activeMutationCommands.delete(run);
    }
  }

  private async prepareProjectSwitch(): Promise<void> {
    if (this.projectSwitchPrepared) {
      return;
    }
    this.mutationAdmissionOpen = false;
    await Promise.allSettled(Array.from(this.activeMutationCommands));
    await this.explorerProvider.prepareProjectSwitch();
    this.projectSwitchPrepared = true;
  }

  private cancelProjectSwitch(): void {
    this.projectSwitchPrepared = false;
    this.mutationAdmissionOpen = true;
    this.explorerProvider.cancelProjectSwitch();
  }

  private async switchProject(): Promise<void> {
    await this.prepareProjectSwitch();
    try {
      const generation = ++this.projectGeneration;
      if (this.settingsRefreshTimer) {
        clearTimeout(this.settingsRefreshTimer);
        this.settingsRefreshTimer = undefined;
      }
      this.pendingSettingsRefreshes.clear();
      this.propertyOnlySettingsRefreshUntil.clear();
      this.settingsRefreshAgainGeneration = undefined;
      this.model.resetProjectState();
      this.propertiesProvider.resetProjectState();
      this.linkState = {};
      this.explorerProvider.setExternalPackageDrag(undefined);
      this.explorerProvider.setLinkState({});
      this.startSettingsWatcher();
      this.startProjectGraphWatcher();
      const explorerSwitch = this.explorerProvider.switchProject();
      await this.model.refresh();
      if (generation !== this.projectGeneration) {
        await explorerSwitch.catch(() => undefined);
        return;
      }
      await explorerSwitch;
    } finally {
      this.projectSwitchPrepared = false;
      this.mutationAdmissionOpen = true;
    }
  }

  private queueSettingsRefresh(settingsFile: string): void {
    const generation = this.projectGeneration;
    const normalizedSettingsFile = this.normalizeSettingsRefreshPath(settingsFile);
    this.clearExpiredPropertyOnlyRefreshes();
    const propertyOnlyUntil = this.propertyOnlySettingsRefreshUntil.get(normalizedSettingsFile);
    this.pendingSettingsRefreshes.add(normalizedSettingsFile);
    if (propertyOnlyUntil !== undefined && propertyOnlyUntil > Date.now()) {
      this.scheduleSettingsRefresh(generation, propertyOnlyUntil - Date.now() + 25);
      return;
    }
    this.scheduleSettingsRefresh(generation, 600);
  }

  private scheduleSettingsRefresh(generation: number, delayMs: number): void {
    if (this.settingsRefreshTimer) {
      clearTimeout(this.settingsRefreshTimer);
    }
    this.settingsRefreshTimer = setTimeout(() => {
      this.settingsRefreshTimer = undefined;
      void this.flushSettingsRefreshes(generation);
    }, Math.max(1, delayMs));
  }

  private async flushSettingsRefreshes(generation = this.projectGeneration): Promise<void> {
    if (generation !== this.projectGeneration) {
      return;
    }
    if (this.settingsRefreshInFlightGeneration === generation) {
      this.settingsRefreshAgainGeneration = generation;
      return;
    }
    this.settingsRefreshInFlightGeneration = generation;
    try {
      do {
        if (generation !== this.projectGeneration) {
          return;
        }
        this.settingsRefreshAgainGeneration = undefined;
        this.clearExpiredPropertyOnlyRefreshes();
        const settingsFiles = Array.from(this.pendingSettingsRefreshes).filter((settingsFile) => {
          const propertyOnlyUntil = this.propertyOnlySettingsRefreshUntil.get(settingsFile);
          return propertyOnlyUntil === undefined || propertyOnlyUntil <= Date.now();
        });
        for (const settingsFile of settingsFiles) {
          this.pendingSettingsRefreshes.delete(settingsFile);
        }
        if (settingsFiles.length > 0) {
          await this.explorerProvider.refreshSettingsFiles(settingsFiles);
          if (generation !== this.projectGeneration) {
            return;
          }
        }
      } while (this.settingsRefreshAgainGeneration === generation);
    } finally {
      if (this.settingsRefreshInFlightGeneration === generation) {
        this.settingsRefreshInFlightGeneration = undefined;
      }
    }
    if (generation !== this.projectGeneration) {
      return;
    }
    if (this.pendingSettingsRefreshes.size > 0) {
      const now = Date.now();
      const earliest = Math.min(...Array.from(this.pendingSettingsRefreshes, (settingsFile) =>
        this.propertyOnlySettingsRefreshUntil.get(settingsFile) ?? now));
      this.scheduleSettingsRefresh(generation, earliest - now + 25);
    }
  }

  private async refreshPropertyChanges(settingsFiles: string[]): Promise<void> {
    const normalizedSettingsFiles = Array.from(new Set(settingsFiles.map((settingsFile) => this.normalizeSettingsRefreshPath(settingsFile))));
    if (normalizedSettingsFiles.length === 0) {
      return;
    }
    const suppressUntil = Date.now() + 2500;
    for (const settingsFile of normalizedSettingsFiles) {
      this.pendingSettingsRefreshes.delete(settingsFile);
      this.propertyOnlySettingsRefreshUntil.set(settingsFile, suppressUntil);
    }
    await this.explorerProvider.refreshPropertyChanges(normalizedSettingsFiles);
  }

  private normalizeSettingsRefreshPath(settingsFile: string): string {
    const config = getExplorerConfig();
    const absolutePath = path.isAbsolute(settingsFile) ? settingsFile : path.join(config.projectRoot, settingsFile);
    return normalizeFilesystemPathKey(absolutePath);
  }

  private clearExpiredPropertyOnlyRefreshes(): void {
    const now = Date.now();
    for (const [settingsFile, until] of this.propertyOnlySettingsRefreshUntil) {
      if (until <= now) {
        this.propertyOnlySettingsRefreshUntil.delete(settingsFile);
      }
    }
  }

  private inheritedLinkState(node: FileExplorerNode): string | undefined {
    let current: FileExplorerNode | undefined = node;
    while (current && current.kind !== "service") {
      const key = nodeLinkPathKey(current);
      const state = key ? this.linkState[key] : undefined;
      if (state) {
        return state;
      }
      current = current.parentTreeId ? this.model.getNode(current.parentTreeId) : undefined;
    }
    return undefined;
  }

  private isPackageControlledNode(node: FileExplorerNode): boolean {
    let current: FileExplorerNode | undefined = node;
    while (current && current.kind !== "service") {
      if (current.hasPackageLink === true || current.className === "PackageLink") {
        return true;
      }
      current = current.parentTreeId ? this.model.getNode(current.parentTreeId) : undefined;
    }
    return false;
  }

  private async showReadonlyScriptDocument(node: FileExplorerNode, sourcePath: string, content: string): Promise<void> {
    const uri = this.readonlyScriptProvider.uriFor(node, sourcePath, content);
    const document = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(document, { preview: false });
    try {
      await vscode.languages.setTextDocumentLanguage(document, "luau");
    } catch {
    }
  }

  private async openScript(node?: FileExplorerNode): Promise<void> {
    if (!node || !isScriptClass(node.className)) {
      return;
    }
    const loaded = await this.model.loadDetails(await this.model.ensureLoaded(node));
    const sourcePath = loaded.sourcePath || `${loaded.name}.luau`;
    const inlineSource = typeof loaded.properties.Source === "string" ? loaded.properties.Source : undefined;
    const controlledByRenium = this.isPackageControlledNode(loaded) || this.inheritedLinkState(loaded) === "linked";
    if (controlledByRenium) {
      try {
        const opened = await vscode.commands.executeCommand<boolean>("renium.packages.openLinkedScriptPreview", {
          service: loaded.service,
          pathSegments: loaded.pathSegments.length > 0 ? loaded.pathSegments.slice() : [loaded.service, loaded.name],
          className: loaded.className,
          name: loaded.name,
        });
        if (opened === true) {
          return;
        }
      } catch {
      }
      if (inlineSource !== undefined) {
        await this.showReadonlyScriptDocument(loaded, sourcePath, inlineSource);
        return;
      }
      if (loaded.sourcePath && fs.existsSync(loaded.sourcePath)) {
        await this.showReadonlyScriptDocument(loaded, loaded.sourcePath, fs.readFileSync(loaded.sourcePath, "utf8"));
        return;
      }
      vscode.window.showWarningMessage(`Source preview not found for ${loaded.name}.`);
      return;
    }
    if (!loaded.sourcePath || !fs.existsSync(loaded.sourcePath)) {
      vscode.window.showWarningMessage(`Source file not found for ${loaded.name}.`);
      return;
    }
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(loaded.sourcePath));
    await vscode.window.showTextDocument(document, { preview: false });
  }

  private async revealStudioScript(action?: {
    service?: string;
    settingsId?: string;
    pathSegments?: string[];
    pathOrdinals?: number[];
  }): Promise<void> {
    const service = String(action?.service ?? "").trim();
    const settingsId = String(action?.settingsId ?? "").trim();
    const pathSegments = Array.isArray(action?.pathSegments) ? action.pathSegments.map(String) : [];
    const pathOrdinals = Array.isArray(action?.pathOrdinals)
      ? action.pathOrdinals.map((value) => Math.max(1, Math.floor(Number(value) || 1)))
      : [];
    if (!service) {
      return;
    }
    await this.model.loadAllServices();
    const node = settingsId
      ? this.model.getNode(normalizeId(service, settingsId))
      : this.model.findNodeByPath(service, pathSegments, pathOrdinals);
    if (!node || !isScriptClass(node.className)) {
      vscode.window.showWarningMessage("The selected Studio script is not present in the current Renium project.");
      return;
    }
    await this.explorerProvider.revealReference(node.treeId);
    await this.openScript(node);
  }

  private async addInstance(node?: FileExplorerNode): Promise<void> {
    if (!node) {
      return;
    }
    const item = await vscode.window.showQuickPick(
      ROBLOX_CLASS_NAMES.map((className) => ({ label: className })),
      { title: "Add Roblox Instance", placeHolder: "ClassName" },
    );
    if (!item) {
      return;
    }
    const name = await vscode.window.showInputBox({
      title: "Instance Name",
      value: item.label,
      prompt: "Name for the new instance",
    });
    if (!name) {
      return;
    }
    try {
      const created = await this.model.addInstance(node, item.label, name);
      if (created) {
        void this.propertiesProvider.show(created);
      }
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to add instance. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async deleteInstance(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      return;
    }
    try {
      const loaded = await this.model.ensureLoaded(node);
      const sourcePaths = await this.model.sourcePathsForSubtree(loaded);
      if (!await this.closeSourceTabs(sourcePaths)) {
        return;
      }
      const manifestTarget = this.reniumManifestTarget(loaded);
      if (manifestTarget && this.hasSiblingPathCollision(manifestTarget.node, manifestTarget.pathSegments)) {
        vscode.window.showWarningMessage(
          `Cannot safely delete ${loaded.name} because another sibling has the same linked target path (${manifestTarget.pathSegments.join(".")}). Rename one of them first.`,
        );
        return;
      }
      if (manifestTarget) {
        try {
          await vscode.commands.executeCommand("renium.link.breakInstance", {
            service: loaded.service,
            pathSegments: manifestTarget.pathSegments,
            silent: true,
            refreshExplorer: false,
          });
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          if (!/is not a renium-link target; nothing to break/i.test(message)) {
            throw error;
          }
        }
      }
      await this.model.removeInstance(loaded);
      void vscode.commands.executeCommand("renium.packages.refresh").then(
        () => undefined,
        () => undefined,
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (/no matched instance|no matching instance|instance not found/i.test(message)) {
        return;
      }
      vscode.window.showErrorMessage(`Failed to delete instance. ${message}`);
    }
  }

  private async desyncPackageLink(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      return;
    }
    const label = node.className === "PackageLink"
      ? node.pathSegments.join(".")
      : `${node.pathSegments.join(".") || node.name}'s PackageLink`;
    const picked = await vscode.window.showWarningMessage(
      `Remove ${label} and convert this package copy to a normal instance?`,
      { modal: true },
      "Desync Package",
    );
    if (picked !== "Desync Package") {
      return;
    }
    try {
      const result = await this.model.desyncPackageLink(node);
      const removed = Array.isArray(result.removedPackageLinks) ? result.removedPackageLinks.length : 0;
      vscode.window.showInformationMessage(`Removed ${removed} PackageLink${removed === 1 ? "" : "s"}.`);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to desync package. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async copyPath(node?: FileExplorerNode): Promise<void> {
    if (!node) {
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    await vscode.env.clipboard.writeText(loaded.pathSegments.join("."));
  }

  private async importModel(node?: FileExplorerNode, providedModelPaths?: string[]): Promise<void> {
    if (!node) {
      vscode.window.showWarningMessage("Select an Explorer node to import into.");
      return;
    }
    const target = await this.model.ensureLoaded(node);
    let modelPaths = normalizeRobloxModelPaths(providedModelPaths);
    if (modelPaths.length === 0) {
      const picked = await vscode.window.showOpenDialog({
        title: `Import Roblox Model into ${target.name}`,
        openLabel: "Import Model",
        canSelectFiles: true,
        canSelectFolders: false,
        canSelectMany: true,
        defaultUri: vscode.Uri.file(getExplorerConfig().projectRoot),
        filters: {
          "Roblox Model": ["rbxm", "rbxmx"],
        },
      });
      if (!picked || picked.length === 0) {
        return;
      }
      modelPaths = normalizeRobloxModelPaths(picked.map((uri) => uri.fsPath));
    }
    if (modelPaths.length === 0) {
      vscode.window.showWarningMessage("Choose one or more .rbxm or .rbxmx files to import.");
      return;
    }
    try {
      let lastCreated: FileExplorerNode | undefined;
      for (const modelPath of modelPaths) {
        const created = await this.model.importModel(target, modelPath);
        if (created) {
          lastCreated = created;
        }
      }
      await this.explorerProvider.refreshServices([target.service]);
      if (lastCreated) {
        void this.propertiesProvider.show(lastCreated);
      }
      const summary = modelPaths.length === 1
        ? path.basename(modelPaths[0])
        : `${modelPaths.length} model files`;
      vscode.window.showInformationMessage(`Imported ${summary} into ${target.name}.`);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to import model. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async exportModel(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage("Select an Explorer instance to export.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    const pickedFormat = await vscode.window.showQuickPick([
      {
        label: "rbxm",
        description: "Binary Roblox model",
        format: "rbxm" as RobloxModelFormat,
      },
      {
        label: "rbxmx",
        description: "XML Roblox model",
        format: "rbxmx" as RobloxModelFormat,
      },
    ], {
      title: `Export ${loaded.name}`,
      placeHolder: "Roblox model format",
    });
    if (!pickedFormat) {
      return;
    }
    const saveUri = await vscode.window.showSaveDialog({
      title: `Export ${loaded.name}`,
      saveLabel: "Export Model",
      defaultUri: vscode.Uri.file(path.join(getExplorerConfig().projectRoot, safeModelFileName(loaded.name, pickedFormat.format))),
      filters: pickedFormat.format === "rbxm"
        ? {
          "Roblox Binary Model": ["rbxm"],
        }
        : {
          "Roblox XML Model": ["rbxmx"],
        },
    });
    if (!saveUri) {
      return;
    }
    const format = robloxModelFormatFromPath(saveUri.fsPath) ?? pickedFormat.format;
    const outputPath = ensureModelFileExtension(saveUri.fsPath, format);
    try {
      const result = await this.model.exportModel(loaded, outputPath, format);
      const finalOutputPath = typeof result.output === "string" && result.output.trim().length > 0
        ? result.output
        : outputPath;
      vscode.window.showInformationMessage(`Exported ${loaded.name} to ${finalOutputPath}.`);
    } catch (error) {
      vscode.window.showErrorMessage(`Failed to export model. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private reniumManifestTarget(node: FileExplorerNode): { node: FileExplorerNode; pathSegments: string[] } | undefined {
    let current: FileExplorerNode | undefined = node;
    while (current && current.kind !== "service") {
      const key = nodeLinkPathKey(current);
      if (key && this.linkState[key]) {
        return { node: current, pathSegments: nodeTargetPath(current) };
      }
      current = current.parentTreeId ? this.model.getNode(current.parentTreeId) : undefined;
    }
    return undefined;
  }

  private hasSiblingPathCollision(targetNode: FileExplorerNode, pathSegments: string[]): boolean {
    const parent = targetNode.parentTreeId ? this.model.getNode(targetNode.parentTreeId) : undefined;
    const targetKey = linkPathKey(targetNode.service, pathSegments);
    if (!parent || !targetKey) {
      return false;
    }
    return this.model.getChildren(parent).some((child) =>
      child.treeId !== targetNode.treeId &&
      linkPathKey(child.service, nodeTargetPath(child)) === targetKey
    );
  }

  private async createLink(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage("Select an Explorer instance to link.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    await vscode.commands.executeCommand("renium.link.packInstance", {
      service: loaded.service,
      pathSegments: nodeTargetPath(loaded),
    });
  }

  private async updateLink(node: FileExplorerNode | undefined, action: "resave" | "relink"): Promise<void> {
    const warning = action === "resave"
      ? "Select a linked package root to resave."
      : "Select a broken package root to relink.";
    if (!node || node.kind === "service") {
      vscode.window.showWarningMessage(warning);
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    const target = this.reniumManifestTarget(loaded);
    if (!target) {
      vscode.window.showWarningMessage(warning);
      return;
    }
    const command = action === "resave" ? "renium.link.resavePackage" : "renium.link.relinkPackage";
    await vscode.commands.executeCommand(command, {
      service: loaded.service,
      pathSegments: target.pathSegments,
    });
  }

  private async insertPackage(node: FileExplorerNode | undefined, link: { id: string; name?: string }): Promise<void> {
    const linkId = String(link.id || "").trim();
    logPackageDragDebug(`explorer.controller.insertPackage: link=${linkId} node=${node ? `${node.service}.${node.pathSegments.join(".")}` : "none"}`);
    if (!linkId) {
      return;
    }
    if (!node) {
      vscode.window.showWarningMessage("Drop the package onto an Explorer service or instance.");
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    if (loaded.hasChildren && loaded.children.length === 0) {
      await this.model.loadChildren(loaded).catch(() => undefined);
    }
    const parentPath = loaded.kind === "service"
      ? [loaded.service]
      : nodeTargetPath(loaded);
    const leafName = this.model.uniqueChildName(loaded, String(link.name || linkId));
    const targetPath = parentPath.concat(leafName);
    logPackageDragDebug(`explorer.controller.insertPackage: target=${loaded.service}.${targetPath.join(".")}`);
    try {
      await vscode.commands.executeCommand("renium.packages.insertAtPath", {
        linkId,
        service: loaded.service,
        pathSegments: targetPath,
      });
      logPackageDragDebug(`explorer.controller.insertPackage: complete link=${linkId} target=${loaded.service}.${targetPath.join(".")}`);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      logPackageDragDebug(`explorer.controller.insertPackage: failed link=${linkId} target=${loaded.service}.${targetPath.join(".")} error=${message}`);
      vscode.window.showErrorMessage(`Failed to insert package. ${message}`);
    }
  }

  private async breakLinkNode(node?: FileExplorerNode): Promise<void> {
    if (!node || node.kind === "service") {
      return;
    }
    const loaded = await this.model.ensureLoaded(node);
    await vscode.commands.executeCommand("renium.link.breakInstance", {
      service: loaded.service,
      pathSegments: nodeTargetPath(loaded),
    });
  }
}
