import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import {
  bytecodeValueFromVerde,
  defaultAttributeValue,
  isMetadataPropertyName,
  isModelPivotCFrameProperty,
  isModelPivotClass,
  modelPivotValue,
  propertyRowsForNode,
  searchTagsFromNode,
  usesDisabledProperty,
  verdePropertyRowsForNode,
  verdeTypeForValue,
} from "./explorerProperties";
import { getPropertiesHtml } from "./propertiesHtml";
import { safeFileComponent } from "./utils";
import { FileExplorerModel } from "./fileExplorerModel";
import {
  type EditorHistoryManifest,
  type FileExplorerNode,
  type PackagePropertiesPayload,
  type PropertiesUpdateMessage,
  type PropertyEditHistoryItem,
  type ReadonlyInstanceInfo,
  type ViewVisibilityHandler,
  cleanPropertyText,
  cloneHistoryValue,
  editorHistoryRoot,
  getExplorerConfig,
  jsonValuesEqual,
  normalizeId,
  pathInsideRoot,
  sanitizePathSegments,
  serviceTreeId,
  settingsFileForService,
} from "./fileExplorerCore";

const MUTATION_MESSAGE_TYPES = new Set([
  "setProperty", "undo", "redo", "addTag", "removeTag", "addAttribute", "setAttribute", "removeAttribute",
  "renameAttribute",
]);

export class FilePropertiesViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "renium.properties";
  private webviewView: vscode.WebviewView | undefined;
  private currentNode: FileExplorerNode | undefined;
  private currentPackageMessage: PropertiesUpdateMessage | undefined;
  private webviewReady = false;
  private readonly pendingPropertyFinalSets = new Map<string, {
    node: FileExplorerNode;
    name: string;
    value: unknown;
    generation: number;
  }>();
  private readonly pendingPropertyHistory = new Map<string, PropertyEditHistoryItem>();
  private readonly propertyUndoStack: PropertyEditHistoryItem[] = [];
  private readonly propertyRedoStack: PropertyEditHistoryItem[] = [];
  private propertyHistorySequence = 0;
  private propertyFinalSetGeneration: number | undefined;
  private readonly pendingLiveStudioPushes = new Map<string, {
    node: FileExplorerNode;
    scope: "metadata" | "property";
    propertyName: string;
    value: unknown;
    generation: number;
  }>();
  private liveStudioPushTimer: NodeJS.Timeout | undefined;
  private liveStudioFlushChain: Promise<void> = Promise.resolve();
  private studioPropertyPushChain: Promise<void> = Promise.resolve();
  private readonly activeMessageTasks = new Set<Promise<void>>();
  private mutationAdmissionOpen = true;
  private referenceRevealHandler: ((nodeId: string) => Promise<void> | void) | undefined;
  private projectGeneration = 0;

  public constructor(
    private readonly model: FileExplorerModel,
    private readonly extensionUri: vscode.Uri,
    private readonly onVisibilityChanged?: ViewVisibilityHandler,
  ) {}

  public setReferenceRevealHandler(handler: (nodeId: string) => Promise<void> | void): void {
    this.referenceRevealHandler = handler;
  }

  public resetProjectState(): void {
    this.projectGeneration += 1;
    this.mutationAdmissionOpen = true;
    if (this.liveStudioPushTimer) {
      clearTimeout(this.liveStudioPushTimer);
      this.liveStudioPushTimer = undefined;
    }
    this.currentNode = undefined;
    this.currentPackageMessage = undefined;
    this.pendingPropertyFinalSets.clear();
    this.pendingPropertyHistory.clear();
    this.pendingLiveStudioPushes.clear();
    this.propertyUndoStack.length = 0;
    this.propertyRedoStack.length = 0;
    this.propertyFinalSetGeneration = undefined;
    this.liveStudioFlushChain = Promise.resolve();
    this.studioPropertyPushChain = Promise.resolve();
    this.webviewView?.webview.postMessage({ type: "clear" });
  }

  public resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.webviewView = webviewView;
    this.webviewReady = false;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.extensionUri, "assets"),
        vscode.Uri.joinPath(this.extensionUri, "resources"),
      ],
    };
    webviewView.webview.onDidReceiveMessage((message) => {
      const task = this.onMessage(message).catch((error) => {
        const text = error instanceof Error ? error.message : String(error);
        this.webviewView?.webview.postMessage({ type: "error", message: text });
        vscode.window.showErrorMessage(`Property action failed. ${text}`);
      });
      this.activeMessageTasks.add(task);
      void task.finally(() => this.activeMessageTasks.delete(task));
    });
    this.onVisibilityChanged?.(FilePropertiesViewProvider.viewType, webviewView.visible);
    webviewView.onDidChangeVisibility(() => {
      this.onVisibilityChanged?.(FilePropertiesViewProvider.viewType, webviewView.visible);
    });
    webviewView.webview.html = getPropertiesHtml(this.extensionUri, { showFilterInput: true });
  }

  public async prepareProjectSwitch(): Promise<void> {
    this.mutationAdmissionOpen = false;
    await Promise.allSettled(Array.from(this.activeMessageTasks));
    await this.finalizePendingPropertyHistory();
    await this.flushLiveStudioPushes().catch(() => undefined);
    await this.studioPropertyPushChain.catch(() => undefined);
    await this.liveStudioFlushChain.catch(() => undefined);
  }

  public cancelProjectSwitch(): void {
    this.mutationAdmissionOpen = true;
  }

  public async show(node: FileExplorerNode): Promise<void> {
    const generation = this.projectGeneration;
    this.currentPackageMessage = undefined;
    const loaded = await this.model.loadDetails(node);
    if (generation !== this.projectGeneration) {
      return;
    }
    this.currentNode = loaded;
    this.pushCurrent();
  }

  public showReadonlyPackage(payload: PackagePropertiesPayload | undefined): void {
    if (!payload) {
      return;
    }
    const packageName = cleanPropertyText(payload.packageName) || cleanPropertyText(payload.packageId) || "Package";
    const rawNode = payload.node;
    const nodeName = cleanPropertyText(rawNode?.name) || cleanPropertyText(payload.rootName) || packageName;
    const className = cleanPropertyText(rawNode?.className) || cleanPropertyText(payload.rootClass) || "Package";
    const pathSegments = sanitizePathSegments(rawNode?.pathSegments);
    const displayPath = pathSegments.length > 0 ? pathSegments.join(".") : nodeName;
    const properties = { ...(rawNode?.properties ?? {}) };
    const sourceLabel = cleanPropertyText(payload.source ?? payload.sourcePath);
    if (sourceLabel) {
      properties.PackageSource = sourceLabel;
    }
    properties.Package = packageName;
    properties.PackagePath = displayPath;
    const node: FileExplorerNode = {
      id: `package:${cleanPropertyText(payload.packageId) || packageName}:${displayPath}`,
      treeId: `package:${cleanPropertyText(payload.packageId) || packageName}:${displayPath}`,
      kind: "instance",
      service: "Package",
      name: nodeName,
      className,
      settingsId: cleanPropertyText(rawNode?.settingsId) || undefined,
      parentTreeId: null,
      children: [],
      loaded: true,
      detailsLoaded: true,
      hasChildren: false,
      settingsFile: "",
      pathSegments,
      pathOrdinals: [],
      properties,
      attributes: { ...(rawNode?.attributes ?? {}) },
    };
    const data = verdePropertyRowsForNode(node, packageName, getExplorerConfig());
    for (const property of data.properties) {
      property.isReadOnly = true;
    }
    this.currentNode = undefined;
    this.currentPackageMessage = {
      type: "updateProperties",
      title: `PROPERTIES - ${packageName} - ${displayPath}`,
      properties: data,
      nodeName,
      nodeClassName: className,
      readOnly: true,
    };
    this.pushCurrent();
  }

  public clearReadonlyPackage(): void {
    if (!this.currentPackageMessage) {
      return;
    }
    this.currentPackageMessage = undefined;
    this.webviewView?.webview.postMessage({ type: "clear" });
  }

  public showReadonlyInstance(info: ReadonlyInstanceInfo): void {
    const nodeName = cleanPropertyText(info.name) || "Instance";
    const className = cleanPropertyText(info.className) || "Instance";
    const pathSegments = sanitizePathSegments(info.pathSegments);
    const displayPath = pathSegments.length > 0 ? pathSegments.join(".") : nodeName;
    const node: FileExplorerNode = {
      id: `store:${info.settingsId ?? displayPath}`,
      treeId: `store:${info.settingsId ?? displayPath}`,
      kind: "instance",
      service: "Inspector",
      name: nodeName,
      className,
      settingsId: cleanPropertyText(info.settingsId) || undefined,
      parentTreeId: null,
      children: [],
      loaded: true,
      detailsLoaded: true,
      hasChildren: false,
      settingsFile: "",
      pathSegments,
      pathOrdinals: [],
      properties: { ...(info.properties ?? {}) },
      attributes: { ...(info.attributes ?? {}) },
    };
    const data = verdePropertyRowsForNode(node, "", getExplorerConfig());
    for (const property of data.properties) {
      property.isReadOnly = true;
    }
    this.currentNode = undefined;
    this.currentPackageMessage = {
      type: "updateProperties",
      title: `PROPERTIES - ${displayPath}`,
      properties: data,
      nodeName,
      nodeClassName: className,
      readOnly: true,
    };
    this.pushCurrent();
  }

  public async refreshCurrentForServices(services: string[]): Promise<void> {
    if (!this.currentNode || !services.includes(this.currentNode.service)) {
      return;
    }
    await this.refreshCurrent();
  }

  public async refreshCurrentForSettingsFiles(settingsFiles: string[]): Promise<void> {
    if (!this.currentNode || settingsFiles.length === 0) {
      return;
    }
    const currentSettingsFile = path.normalize(this.currentNode.settingsFile || settingsFileForService(getExplorerConfig(), this.currentNode.service));
    const changedSettingsFiles = new Set(settingsFiles.map((settingsFile) => path.normalize(settingsFile)));
    if (!changedSettingsFiles.has(currentSettingsFile)) {
      return;
    }
    await this.refreshCurrent();
  }

  private async refreshCurrent(): Promise<void> {
    if (!this.currentNode) {
      return;
    }
    const generation = this.projectGeneration;
    const updated = (this.currentNode.settingsId
      ? this.model.getNode(normalizeId(this.currentNode.service, this.currentNode.settingsId))
      : this.model.getNode(serviceTreeId(this.currentNode.service))) ?? this.currentNode;
    updated.detailsLoaded = false;
    const loaded = await this.model.loadDetails(updated);
    if (generation !== this.projectGeneration) {
      return;
    }
    this.currentNode = loaded;
    this.pushCurrent();
  }

  private pushCurrent(): void {
    if (!this.webviewView || !this.webviewReady) {
      return;
    }
    if (this.currentPackageMessage) {
      this.webviewView.webview.postMessage(this.currentPackageMessage);
      return;
    }
    if (!this.currentNode) {
      return;
    }
    this.webviewView.webview.postMessage({
      type: "updateProperties",
      properties: verdePropertyRowsForNode(
        this.currentNode,
        this.parentLabel(this.currentNode),
        getExplorerConfig(),
        (value) => this.currentNode ? this.model.findNodeForReference(value, this.currentNode.service) : undefined,
      ),
      nodeName: this.currentNode.name,
      nodeClassName: this.currentNode.className,
      nodeTreeId: this.currentNode.treeId,
      readOnly: false,
    });
  }

  private parentLabel(node: FileExplorerNode): string {
    if (node.kind === "service") {
      return "game";
    }
    if (!node.parentTreeId) {
      return "game";
    }
    return this.model.getNode(node.parentTreeId)?.name ?? node.parentTreeId;
  }

  private async onMessage(message: {
    type?: string;
    propertyName?: string;
    propertyValue?: unknown;
    tagName?: string;
    attributeName?: string;
    attributeValue?: unknown;
    attributeType?: string;
    oldName?: string;
    newName?: string;
    instanceId?: string;
    nodeTreeId?: string;
    live?: boolean;
  }): Promise<void> {
    if (message.type === "ready") {
      this.webviewReady = true;
      this.pushCurrent();
      return;
    }
    if (message.type === "navigateToInstance") {
      const instanceId = typeof message.instanceId === "string" ? message.instanceId : undefined;
      if (instanceId && this.referenceRevealHandler) {
        try {
          await this.referenceRevealHandler(instanceId);
        } catch (error) {
          vscode.window.showErrorMessage(`Failed to reveal referenced instance. ${error instanceof Error ? error.message : String(error)}`);
        }
      }
      return;
    }
    if (!this.currentNode || !message.type) {
      return;
    }
    if (!this.mutationAdmissionOpen && MUTATION_MESSAGE_TYPES.has(message.type)) {
      return;
    }
    const messageNode = typeof message.nodeTreeId === "string" && message.nodeTreeId.length > 0
      ? this.model.getNode(message.nodeTreeId)
      : this.currentNode;
    if (MUTATION_MESSAGE_TYPES.has(message.type)) {
      if (!messageNode) {
        return;
      }
      if (message.type !== "setProperty" && messageNode.treeId !== this.currentNode.treeId) {
        return;
      }
    }
    try {
      let shouldPush = true;
      switch (message.type) {
        case "setProperty":
          await this.queuePropertyFromWebview(messageNode, message.propertyName, message.propertyValue, message.live === true);
          if (message.live !== true) {
            this.webviewView?.webview.postMessage({ type: "propertyCommitDone", propertyName: message.propertyName });
          }
          shouldPush = false;
          break;
        case "undo":
          await this.undoPropertyEdit(message.propertyName);
          this.webviewView?.webview.postMessage({ type: "propertyCommitDone", propertyName: message.propertyName });
          shouldPush = false;
          break;
        case "redo":
          await this.redoPropertyEdit(message.propertyName);
          this.webviewView?.webview.postMessage({ type: "propertyCommitDone", propertyName: message.propertyName });
          shouldPush = false;
          break;
        case "addTag":
          await this.addTag(message.tagName);
          break;
        case "removeTag":
          await this.removeTag(message.tagName);
          break;
        case "addAttribute":
          await this.addAttribute(message.attributeName, message.attributeType);
          break;
        case "setAttribute":
          await this.setAttribute(message.attributeName, message.attributeValue);
          break;
        case "removeAttribute":
          await this.removeAttribute(message.attributeName);
          break;
        case "renameAttribute":
          await this.renameAttribute(message.oldName, message.newName);
          break;
        default:
          return;
      }
      if (shouldPush) {
        this.pushCurrent();
      }
    } catch (error) {
      if (
        (message.type === "setProperty" && message.live !== true)
        || message.type === "undo"
        || message.type === "redo"
      ) {
        this.webviewView?.webview.postMessage({ type: "propertyCommitDone", propertyName: message.propertyName });
      }
      vscode.window.showErrorMessage(`Failed to update property. ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async queuePropertyFromWebview(node: FileExplorerNode | undefined, name: string | undefined, value: unknown, live: boolean): Promise<void> {
    if (!node || !name) {
      return;
    }
    const generation = this.projectGeneration;
    if (live) {
      await this.setPropertyFromWebview(node, name, value, true);
      return;
    }
    this.pendingPropertyFinalSets.set(`${node.treeId}:${name}`, { node, name, value, generation });
    if (this.propertyFinalSetGeneration === generation) {
      return;
    }
    this.propertyFinalSetGeneration = generation;
    try {
      while (generation === this.projectGeneration && this.pendingPropertyFinalSets.size > 0) {
        const next = this.pendingPropertyFinalSets.values().next().value;
        if (!next) {
          break;
        }
        this.pendingPropertyFinalSets.delete(`${next.node.treeId}:${next.name}`);
        if (next.generation !== generation) {
          continue;
        }
        await this.setPropertyFromWebview(next.node, next.name, next.value, false);
      }
    } finally {
      if (this.propertyFinalSetGeneration === generation) {
        this.propertyFinalSetGeneration = undefined;
      }
    }
  }

  private async setPropertyFromWebview(node: FileExplorerNode, name: string, value: unknown, live = false): Promise<void> {
    const scope = isMetadataPropertyName(name) ? "metadata" : "property";
    if (scope === "metadata" && name !== "Name") {
      return;
    }
    const row = scope === "property"
      ? propertyRowsForNode(node, getExplorerConfig()).find((candidate) => candidate.name === name)
      : undefined;
    const currentValue = scope === "property"
      ? isModelPivotCFrameProperty(node, name)
        ? modelPivotValue(node)
        : name === "Enabled" && usesDisabledProperty(node.className)
        ? !(node.properties.Disabled === true)
        : node.properties[name]
      : node.name;
    const writePropertyName = scope === "property" && (name === "WorldPivotData" || name === "Origin") && isModelPivotClass(node.className)
      ? "WorldPivot"
      : name;
    const studioPropertyName = scope === "property"
      ? name === "Enabled" && usesDisabledProperty(node.className)
        ? "Disabled"
        : name === "Origin" && isModelPivotClass(node.className)
        ? "Origin"
        : writePropertyName
      : writePropertyName;
    const rawValue = bytecodeValueFromVerde(value, row?.dataType, currentValue);
    const studioRawValue = studioPropertyName === "Disabled" && name === "Enabled"
      ? !(rawValue === true)
      : rawValue;
    const pushTarget = this.snapshotStudioPushNode(node);
    const propertyKey = `${node.treeId}:${scope}:${name}`;
    const pendingHistory = this.pendingPropertyHistory.get(propertyKey);
    const historyItem: PropertyEditHistoryItem = pendingHistory ?? {
      service: node.service,
      settingsId: node.settingsId,
      treeId: node.treeId,
      className: node.className,
      sourcePath: node.sourcePath,
      pathSegments: node.pathSegments.length > 0
        ? node.pathSegments.slice()
        : [node.service, node.name].filter((segment) => segment.length > 0),
      pathOrdinals: node.pathOrdinals.slice(),
      propertyKey,
      scope,
      name,
      propertyLabel: row?.displayName ?? name,
      writePropertyName,
      studioPropertyName,
      beforeRawValue: cloneHistoryValue(currentValue),
      afterRawValue: cloneHistoryValue(rawValue),
      settingsFile: node.settingsFile,
    };
    if (live) {
      historyItem.afterRawValue = cloneHistoryValue(rawValue);
      this.pendingPropertyHistory.set(propertyKey, historyItem);
      this.applyLocalPropertyValue(node, scope, name, rawValue);
      this.queueLiveStudioPush(pushTarget, scope, studioPropertyName, studioRawValue);
      return;
    }
    historyItem.afterRawValue = cloneHistoryValue(rawValue);
    this.pendingPropertyHistory.delete(propertyKey);
    const changedFromEditStart = !jsonValuesEqual(historyItem.beforeRawValue, rawValue);
    if (!changedFromEditStart) {
      if (pendingHistory) {
        this.applyLocalPropertyValue(node, scope, name, rawValue);
        this.pushCurrent();
        this.queueFinalStudioPropertyPush(pushTarget, scope, studioPropertyName, studioRawValue);
      }
      return;
    }
    const settingsBackup = await this.capturePropertyHistoryBackup(historyItem);
    this.applyLocalPropertyValue(node, scope, name, rawValue);
    const changedPaths = await this.model.setValue(
      node,
      scope,
      writePropertyName,
      JSON.stringify(rawValue),
      { skipStudioPush: true },
    );
    await this.rememberPropertyHistory(historyItem, settingsBackup);
    this.pushCurrent();
    this.queueFinalStudioPropertyPush(pushTarget, scope, studioPropertyName, studioRawValue, changedPaths);
  }

  private async finalizePendingPropertyHistory(): Promise<void> {
    for (const item of Array.from(this.pendingPropertyHistory.values())) {
      const node = this.propertyHistoryNode(item);
      if (!node || node.service !== item.service) {
        throw new Error(`Could not finish the pending ${item.propertyLabel} edit before changing projects.`);
      }
      await this.setPropertyFromWebview(node, item.name, cloneHistoryValue(item.afterRawValue), false);
    }
  }

  private async capturePropertyHistoryBackup(item: PropertyEditHistoryItem): Promise<Buffer | undefined> {
    const settingsFile = path.normalize(item.settingsFile);
    if (!settingsFile || !fs.existsSync(settingsFile)) {
      return undefined;
    }
    try {
      return await fs.promises.readFile(settingsFile);
    } catch {
      return undefined;
    }
  }

  private async rememberPropertyHistory(item: PropertyEditHistoryItem, settingsBackup: Buffer | undefined): Promise<void> {
    const historyId = await this.writePropertyHistory(item, settingsBackup);
    const stored: PropertyEditHistoryItem = {
      ...item,
      pathSegments: item.pathSegments.slice(),
      pathOrdinals: item.pathOrdinals.slice(),
      beforeRawValue: cloneHistoryValue(item.beforeRawValue),
      afterRawValue: cloneHistoryValue(item.afterRawValue),
      historyId,
    };
    this.propertyUndoStack.push(stored);
    this.propertyRedoStack.length = 0;
    if (this.propertyUndoStack.length > 100) {
      this.propertyUndoStack.splice(0, this.propertyUndoStack.length - 100);
    }
  }

  private async writePropertyHistory(item: PropertyEditHistoryItem, settingsBackup: Buffer | undefined): Promise<string | undefined> {
    if (!settingsBackup || settingsBackup.length === 0) {
      return undefined;
    }
    const config = getExplorerConfig();
    const historyRoot = editorHistoryRoot(config);
    const createdUnixMs = Date.now();
    const sequence = ++this.propertyHistorySequence;
    const safeService = safeFileComponent(item.service);
    const safeProperty = safeFileComponent(item.propertyLabel ?? item.name);
    const id = `${createdUnixMs}-${sequence}-${safeService}-${safeProperty}`;
    const entryDir = path.join(historyRoot, id);
    const stagedEntryDir = path.join(historyRoot, `.${id}.tmp`);
    const manifestPath = path.join(entryDir, "manifest.json");
    if (!pathInsideRoot(historyRoot, manifestPath) || !pathInsideRoot(historyRoot, stagedEntryDir)) {
      return undefined;
    }
    try {
      await fs.promises.mkdir(historyRoot, { recursive: true });
      await fs.promises.mkdir(stagedEntryDir);
      await fs.promises.writeFile(path.join(stagedEntryDir, "settings.renium"), settingsBackup);
      const manifest: EditorHistoryManifest = {
        version: 1,
        createdUnixMs,
        service: item.service,
        sourcePath: item.sourcePath,
        settingsId: item.settingsId,
        pathSegments: item.pathSegments.slice(),
        className: item.className,
        propertyName: item.name,
        propertyLabel: item.propertyLabel ?? item.name,
        settingsFile: item.settingsFile,
        settingsBackup: "settings.renium",
      };
      await fs.promises.writeFile(
        path.join(stagedEntryDir, "manifest.json"),
        `${JSON.stringify(manifest, null, 2)}\n`,
        "utf8",
      );
      await fs.promises.rename(stagedEntryDir, entryDir);
      const entries = (await fs.promises.readdir(historyRoot, { withFileTypes: true }))
        .filter((entry) => entry.isDirectory() && !entry.name.startsWith("."))
        .map((entry) => entry.name)
        .sort();
      await Promise.all(entries.slice(0, Math.max(0, entries.length - 100)).map((entry) =>
        fs.promises.rm(path.join(historyRoot, entry), { recursive: true, force: true })
      ));
      return id;
    } catch {
      await fs.promises.rm(stagedEntryDir, { recursive: true, force: true }).catch(() => undefined);
      return undefined;
    }
  }

  private takePropertyHistoryItem(stack: PropertyEditHistoryItem[], propertyName?: string): PropertyEditHistoryItem | undefined {
    const focusedName = String(propertyName ?? "").trim();
    if (focusedName.length > 0 && this.currentNode) {
      for (let index = stack.length - 1; index >= 0; index -= 1) {
        const item = stack[index];
        if (item.treeId !== this.currentNode.treeId) {
          continue;
        }
        if (item.name === focusedName || item.propertyKey.endsWith(`:${item.scope}:${focusedName}`) || focusedName.startsWith(`${item.name}.`)) {
          return stack.splice(index, 1)[0];
        }
      }
    }
    return stack.pop();
  }

  private async undoPropertyEdit(propertyName?: string): Promise<void> {
    const item = this.takePropertyHistoryItem(this.propertyUndoStack, propertyName);
    if (!item) {
      return;
    }
    await this.applyPropertyHistoryValue(item, item.beforeRawValue);
    this.propertyRedoStack.push(item);
  }

  private async redoPropertyEdit(propertyName?: string): Promise<void> {
    const item = this.takePropertyHistoryItem(this.propertyRedoStack, propertyName);
    if (!item) {
      return;
    }
    await this.applyPropertyHistoryValue(item, item.afterRawValue);
    this.propertyUndoStack.push(item);
  }

  private propertyHistoryNode(item: PropertyEditHistoryItem): FileExplorerNode | undefined {
    const currentMatches = !!this.currentNode && (
      this.currentNode.treeId === item.treeId
      || (!!item.settingsId && this.currentNode.settingsId === item.settingsId)
    );
    return this.model.getNode(item.treeId)
      ?? (item.settingsId ? this.model.getNode(normalizeId(item.service, item.settingsId)) : undefined)
      ?? (currentMatches ? this.currentNode : undefined);
  }

  private async applyPropertyHistoryValue(item: PropertyEditHistoryItem, rawValue: unknown): Promise<void> {
    const node = this.propertyHistoryNode(item);
    if (!node || node.service !== item.service) {
      return;
    }
    const pushTarget = this.snapshotStudioPushNode(node);
    const clonedValue = cloneHistoryValue(rawValue);
    this.applyLocalPropertyValue(node, item.scope, item.name, clonedValue);
    const changedPaths = await this.model.setValue(
      node,
      item.scope,
      item.writePropertyName,
      JSON.stringify(clonedValue),
      { skipStudioPush: true },
    );
    if (this.currentNode && (this.currentNode.treeId === node.treeId || (item.settingsId && this.currentNode.settingsId === item.settingsId))) {
      this.applyLocalPropertyValue(this.currentNode, item.scope, item.name, clonedValue);
      this.pushCurrent();
    }
    this.queueFinalStudioPropertyPush(
      pushTarget,
      item.scope,
      item.studioPropertyName,
      this.studioRawValueForHistory(item, clonedValue),
      changedPaths,
    );
  }

  private studioRawValueForHistory(item: PropertyEditHistoryItem, rawValue: unknown): unknown {
    return item.studioPropertyName === "Disabled" && item.name === "Enabled"
      ? !(rawValue === true)
      : rawValue;
  }

  private snapshotStudioPushNode(node: FileExplorerNode): FileExplorerNode {
    return {
      ...node,
      children: node.children.slice(),
      pathSegments: node.pathSegments.slice(),
      pathOrdinals: node.pathOrdinals.slice(),
      properties: { ...node.properties },
      attributes: { ...node.attributes },
    };
  }

  private applyLocalPropertyValue(node: FileExplorerNode, scope: "metadata" | "property", name: string, rawValue: unknown): void {
    if (scope === "property") {
      if (name === "Enabled" && usesDisabledProperty(node.className)) {
        node.properties.Disabled = !(rawValue === true);
      } else if ((name === "WorldPivot" || name === "WorldPivotData" || name === "Origin") && isModelPivotClass(node.className)) {
        node.properties.WorldPivot = rawValue;
        delete node.properties.WorldPivotData;
        node.properties.Origin = rawValue;
      } else {
        node.properties[name] = rawValue;
      }
    } else if (name === "Name" && typeof rawValue === "string") {
      node.name = rawValue;
    }
  }

  private queueLiveStudioPush(node: FileExplorerNode, scope: "metadata" | "property", propertyName: string, value: unknown): void {
    if (!node.settingsFile) {
      return;
    }
    this.pendingLiveStudioPushes.set(`${node.settingsFile}:${node.settingsId ?? node.treeId}:${scope}:${propertyName}`, {
      node,
      scope,
      propertyName,
      value,
      generation: this.projectGeneration,
    });
    if (this.liveStudioPushTimer) {
      clearTimeout(this.liveStudioPushTimer);
    }
    this.liveStudioPushTimer = setTimeout(() => {
      this.liveStudioPushTimer = undefined;
      void this.flushLiveStudioPushes();
    }, 16);
  }

  private async flushLiveStudioPushes(): Promise<void> {
    const run = this.liveStudioFlushChain
      .catch(() => undefined)
      .then(async () => {
        const generation = this.projectGeneration;
        if (this.liveStudioPushTimer) {
          clearTimeout(this.liveStudioPushTimer);
          this.liveStudioPushTimer = undefined;
        }
        const pushes = Array.from(this.pendingLiveStudioPushes.values())
          .filter((push) => push.generation === generation);
        this.pendingLiveStudioPushes.clear();
        await Promise.all(pushes.map((push) =>
          this.model.pushPropertyToStudio(push.node, push.scope, push.propertyName, push.value),
        ));
      });
    this.liveStudioFlushChain = run.catch(() => undefined);
    await run;
  }

  private queueFinalStudioPropertyPush(
    node: FileExplorerNode,
    scope: "metadata" | "property",
    propertyName: string,
    value: unknown,
    changedPaths: string[] = [node.settingsFile],
  ): void {
    const generation = this.projectGeneration;
    this.studioPropertyPushChain = this.studioPropertyPushChain
      .catch(() => undefined)
      .then(async () => {
        if (generation !== this.projectGeneration) {
          return;
        }
        await this.flushLiveStudioPushes();
        if (generation !== this.projectGeneration) {
          return;
        }
        await this.model.pushPropertyToStudio(node, scope, propertyName, value, changedPaths);
        if (generation !== this.projectGeneration) {
          return;
        }
        await this.reloadCurrent();
      })
      .catch((error) => {
        if (generation !== this.projectGeneration) {
          return;
        }
        vscode.window.showErrorMessage(
          `Failed to update property in Studio. ${error instanceof Error ? error.message : String(error)}`,
        );
      });
  }

  private async setAttribute(name: string | undefined, value: unknown): Promise<void> {
    if (!this.currentNode || !name) {
      return;
    }
    const currentValue = this.currentNode.attributes[name];
    const type = verdeTypeForValue(currentValue);
    const rawValue = bytecodeValueFromVerde(value, type, currentValue);
    await this.model.setValue(this.currentNode, "attribute", name, JSON.stringify(rawValue));
    await this.reloadCurrent();
  }

  private async addAttribute(name: string | undefined, type: string | undefined): Promise<void> {
    if (!this.currentNode || !name) {
      return;
    }
    await this.model.setValue(this.currentNode, "attribute", name, JSON.stringify(defaultAttributeValue(type ?? "string")));
    await this.reloadCurrent();
  }

  private async removeAttribute(name: string | undefined): Promise<void> {
    if (!this.currentNode || !name) {
      return;
    }
    await this.model.setValue(this.currentNode, "attribute", name, "null");
    await this.reloadCurrent();
  }

  private async renameAttribute(oldName: string | undefined, newName: string | undefined): Promise<void> {
    if (!this.currentNode || !oldName || !newName || oldName === newName) {
      return;
    }
    const currentValue = this.currentNode.attributes[oldName];
    await this.model.setValue(this.currentNode, "attribute", newName, JSON.stringify(currentValue ?? ""));
    await this.model.setValue(this.currentNode, "attribute", oldName, "null");
    await this.reloadCurrent();
  }

  private async addTag(tagName: string | undefined): Promise<void> {
    if (!this.currentNode || !tagName) {
      return;
    }
    const tags = searchTagsFromNode(this.currentNode);
    if (!tags.includes(tagName)) {
      tags.push(tagName);
      await this.model.setValue(this.currentNode, "property", "Tags", JSON.stringify(tags));
      await this.reloadCurrent();
    }
  }

  private async removeTag(tagName: string | undefined): Promise<void> {
    if (!this.currentNode || !tagName) {
      return;
    }
    const tags = searchTagsFromNode(this.currentNode).filter((tag) => tag !== tagName);
    await this.model.setValue(this.currentNode, "property", "Tags", JSON.stringify(tags));
    await this.reloadCurrent();
  }

  private async reloadCurrent(): Promise<void> {
    if (!this.currentNode) {
      return;
    }
    const generation = this.projectGeneration;
    const updated = this.currentNode.settingsId
      ? this.model.getNode(normalizeId(this.currentNode.service, this.currentNode.settingsId))
      : this.model.getNode(serviceTreeId(this.currentNode.service));
    if (updated) {
      const loaded = await this.model.loadDetails(updated);
      if (generation !== this.projectGeneration) {
        return;
      }
      this.currentNode = loaded;
    }
  }

}
