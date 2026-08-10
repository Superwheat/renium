import * as crypto from "crypto";
import * as fs from "fs";
import * as vscode from "vscode";

import { iconAssetNameForClass, loadAssetIconNames, logPackageDragDebug } from "./fileExplorer";
import { isScriptClass, robloxScriptFileName } from "./utils";

const RENIUM_PACKAGE_DRAG_MIME = "application/vnd.renium.package";
const RENIUM_PACKAGE_TEXT_PREFIX = "renium-package:";

type CliLinkStatusMirror = {
  path?: string;
  canonical?: string;
  drift?: boolean;
  exists?: boolean;
};

export type CliLinkStatusTarget = {
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

export type PackagePreviewNode = {
  settingsId?: string;
  name?: string;
  className?: string;
  parentId?: string;
  childCount?: number;
  pathSegments?: string[];
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
};

export type PackagePreviewData = {
  id: string;
  name: string;
  source?: string;
  sourcePath: string;
  rootClass?: string | null;
  rootName?: string | null;
  nodes: PackagePreviewNode[];
  rootIds: string[];
};

export type LinkedPackageScriptPreviewRequest = {
  service?: string;
  pathSegments?: string[];
  className?: string;
  name?: string;
};

export type OpenPackageScriptTab = {
  linkId: string;
  nodeKey: string;
};

interface PackagesController {
  getLinkFileIndex(force?: boolean): Promise<Map<string, LinkFileInfo>>;
  normalizeLinkPathKey(filePath: string): string;
  getLinkPackages(force?: boolean): Promise<CliLinkStatusLink[]>;
  loadPackagePreview(link: CliLinkStatusLink): Promise<PackagePreviewData>;
  getLinkStatus(force?: boolean): Promise<{ targets?: CliLinkStatusTarget[] } | undefined>;
}
export class LinkDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri[] | undefined>();
  public readonly onDidChangeFileDecorations = this.emitter.event;
  private index = new Map<string, LinkFileInfo>();
  private refreshGeneration = 0;

  public constructor(private readonly controller: PackagesController) {}

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

export type PackageTreeElement = PackageLinkElement | PackageNodeElement;

type PackagePreviewTree = {
  generation: number;
  preview: PackagePreviewData;
  nodesByKey: Map<string, PackagePreviewNode>;
  elementsByKey: Map<string, PackageNodeElement>;
  childrenByParent: Map<string, PackageNodeElement[]>;
  roots: PackageNodeElement[];
};

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

export function packageScriptUriInfo(uri: vscode.Uri): OpenPackageScriptTab | undefined {
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

export class PackageScriptContentProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
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
      path: `/${encodeURIComponent(packageName)}/${encodeURIComponent(robloxScriptFileName(node.name, node.className))}`,
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

export class PackageScriptDecorationProvider implements vscode.FileDecorationProvider {
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

export class PackagesTreeProvider implements vscode.TreeDataProvider<PackageTreeElement>, vscode.TreeDragAndDropController<PackageTreeElement>, vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<PackageTreeElement | undefined | null | void>();
  public readonly onDidChangeTreeData = this.changeEmitter.event;
  public readonly dragMimeTypes = [RENIUM_PACKAGE_DRAG_MIME, "text/plain"];
  public readonly dropMimeTypes: string[] = [];
  private readonly iconNames: ReadonlySet<string>;
  private readonly previewCache = new Map<string, Promise<PackagePreviewTree>>();
  private readonly expandedLinkIds = new Set<string>();
  private readonly expandedNodeKeys = new Map<string, Set<string>>();
  private clearDragTimer: NodeJS.Timeout | undefined;
  private selectionGeneration = 0;
  private suppressExpansionTracking = false;
  private propertiesUpdateChain: Promise<void> = Promise.resolve();

  public constructor(
    private readonly controller: PackagesController,
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
    item.id = `package:${link.id ?? name}:${this.selectionGeneration}`;
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
    item.iconPath = fs.existsSync(iconUri.fsPath) ? iconUri : new vscode.ThemeIcon(isScriptClass(className) ? "symbol-method" : "symbol-class");
    item.contextValue = isScriptClass(className) ? "reniumPackageNode.script" : "reniumPackageNode";
    item.id = `package-node:${element.link.id}:${element.nodeKey}:${this.selectionGeneration}`;
    item.command = {
      command: "renium.packages.openItem",
      title: isScriptClass(className) ? "Open Package Script" : "Show Package Properties",
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
    return { kind: "link", link, selectionVersion: this.selectionGeneration };
  }

  private currentPackageElement(element: PackageTreeElement): PackageTreeElement {
    if (element.kind === "link") {
      return this.linkElement(element.link);
    }
    return { ...element, selectionVersion: this.selectionGeneration };
  }

  private async elementForLinkId(linkId: string): Promise<PackageLinkElement | undefined> {
    const links = await this.controller.getLinkPackages(false);
    const link = links.find((candidate) => String(candidate.id ?? "").trim() === linkId);
    return link ? this.linkElement(link) : undefined;
  }

  private async elementForNodeKey(linkId: string, nodeKey: string): Promise<PackageNodeElement | undefined> {
    const tree = await this.previewTreeForLinkId(linkId);
    return tree?.elementsByKey.get(nodeKey);
  }

  private async previewTreeForLinkId(linkId: string): Promise<PackagePreviewTree | undefined> {
    const linkElement = await this.elementForLinkId(linkId);
    if (!linkElement) {
      return undefined;
    }
    const tree = await this.previewTree(linkElement.link);
    return tree.generation === this.selectionGeneration ? tree : undefined;
  }

  public async packageScriptSourceFor(linkId: string, nodeKey: string): Promise<string | undefined> {
    const tree = await this.previewTreeForLinkId(linkId);
    if (!tree) {
      return undefined;
    }
    const node = tree.nodesByKey.get(nodeKey);
    if (!node || !isScriptClass(node.className)) {
      return undefined;
    }
    return packageNodeSource(node);
  }

  public async openPackageScriptByKey(
    linkId: string,
    nodeKey: string,
    options: { preview?: boolean; preserveFocus?: boolean } = {},
  ): Promise<boolean> {
    const tree = await this.previewTreeForLinkId(linkId);
    if (!tree) {
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
          selectionVersion: this.selectionGeneration,
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
    const selected = await this.currentPackageNode(element);
    if (!selected) {
      return;
    }
    await this.showPackageProperties(selected.preview, selected.node, selected.generation);
    if (selected.openScript && selected.node) {
      await this.openPackageScript(selected.preview, selected.node, {}, selected.generation);
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
    if (!isScriptClass(node.className)) {
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
    return relativePath.length === 0 || !request.name || !node.name || request.name === node.name;
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
    const selected = await this.currentPackageNode(element);
    if (!selected) {
      return;
    }
    await this.showPackageProperties(selected.preview, selected.node, selected.generation);
  }

  private async currentPackageNode(element: PackageTreeElement | undefined): Promise<{
    preview: PackagePreviewData;
    node: PackagePreviewNode | undefined;
    generation: number;
    openScript: boolean;
  } | undefined> {
    if (!this.isCurrentElement(element)) {
      return undefined;
    }
    if (element.kind === "link") {
      const tree = await this.previewTree(element.link);
      if (!this.isCurrentElement(element) || tree.generation !== this.selectionGeneration) {
        return undefined;
      }
      return {
        preview: tree.preview,
        node: tree.roots[0]?.node,
        generation: this.selectionGeneration,
        openScript: tree.roots.length === 1,
      };
    }
    return {
      preview: element.preview,
      node: element.node,
      generation: this.selectionGeneration,
      openScript: true,
    };
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
    if (!isScriptClass(node.className)) {
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
