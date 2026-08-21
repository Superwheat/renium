import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import { compareExplorerNodes } from "./serviceDefaults";
import {
  appendRecordAssignments,
  isProtectedStarterPlayerContainer,
  refPathKeyFromSegments,
  refRecordFromValue,
  refTargetFromObject,
  usesDisabledProperty,
} from "./explorerProperties";
import { delay, safeObject, type RobloxModelFormat } from "./utils";
import {
  type CliBatchOp,
  type CliChildrenDump,
  type CliCloneInstanceResult,
  type CliDesyncPackageLinkResult,
  type CliExplorerCounts,
  type CliExportModelResult,
  type CliImportModelResult,
  type CliRemoveInstanceResult,
  type CliServiceDump,
  type CliServiceNode,
  type ExplorerConfig,
  type FileExplorerNode,
  canonicalExplorerServices,
  explorerServiceForPath,
  getExplorerConfig,
  isSettingsLockTimeout,
  normalizeFilesystemPathKey,
  normalizeId,
  parseJsonValue,
  runBytecodeBatchOne,
  runJsonCli,
  serviceFromSettingsFile,
  serviceTreeId,
  settingsFileForService,
  srcRoot,
} from "./fileExplorerCore";

export class FileExplorerModel {
  private readonly nodesById = new Map<string, FileExplorerNode>();
  private readonly loadedServices = new Set<string>();
  private rootIds: string[] = [];
  private projectGeneration = 0;

  public resetProjectState(): void {
    this.projectGeneration += 1;
    this.nodesById.clear();
    this.loadedServices.clear();
    this.rootIds = [];
  }

  public isProjectGenerationCurrent(generation: number): boolean {
    return generation === this.projectGeneration;
  }

  public prepareProjectSwitch(): Promise<void> {
    return Promise.resolve();
  }

  private requireProjectGeneration(generation: number): void {
    if (!this.isProjectGenerationCurrent(generation)) {
      throw new Error("The active Renium project changed while the operation was running.");
    }
  }

  private async withPausedProjectWrite<T>(
    write: () => Promise<T>,
    finish: (result: T) => Promise<string[]>,
  ): Promise<T> {
    await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
      fileWrites: "pause",
    });
    try {
      const result = await write();
      const settledPaths = await finish(result);
      await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
        paths: settledPaths,
        fileWrites: "resume",
      });
      return result;
    } catch (error) {
      try {
        await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
          fileWrites: "resume",
        });
      } catch (resumeError) {
        const original = error instanceof Error ? error.message : String(error);
        const resume = resumeError instanceof Error ? resumeError.message : String(resumeError);
        throw new Error(`${original}; live sync also failed to resume: ${resume}`);
      }
      throw error;
    }
  }

  public getNode(treeId: string): FileExplorerNode | undefined {
    return this.nodesById.get(treeId);
  }

  public getKnownNodes(): FileExplorerNode[] {
    return Array.from(this.nodesById.values());
  }

  public findNodeByPath(service: string, pathSegments: string[], pathOrdinals: number[]): FileExplorerNode | undefined {
    return this.getKnownNodes().find((candidate) => {
      if (candidate.service !== service || candidate.pathSegments.length !== pathSegments.length) {
        return false;
      }
      for (let index = 0; index < pathSegments.length; index += 1) {
        if (
          candidate.pathSegments[index] !== pathSegments[index]
          || (candidate.pathOrdinals[index] ?? 1) !== (pathOrdinals[index] ?? 1)
        ) {
          return false;
        }
      }
      return true;
    });
  }

  public findNodeForReference(value: unknown, service?: string): FileExplorerNode | undefined {
    const record = refRecordFromValue(value);
    if (!record) {
      return undefined;
    }
    const target = refTargetFromObject(record);
    const serviceFilter = service && service.length > 0 ? service : undefined;
    for (const candidate of this.nodesById.values()) {
      if (serviceFilter && candidate.service !== serviceFilter) {
        continue;
      }
      if (target.settingsId !== undefined && candidate.settingsId === target.settingsId) {
        return candidate;
      }
      if (target.index !== undefined && candidate.index === target.index) {
        return candidate;
      }
      if (target.pathKey !== undefined && refPathKeyFromSegments(candidate.pathSegments) === target.pathKey) {
        return candidate;
      }
    }
    return undefined;
  }

  public lookupPropertyValues(
    requests: Array<{ service?: string; settingsId?: string; scope?: string; property?: string }>,
  ): Array<unknown> {
    const byServiceSettingsId = new Map<string, FileExplorerNode>();
    for (const node of this.nodesById.values()) {
      if (node.settingsId) {
        byServiceSettingsId.set(`${node.service}\0${node.settingsId}`, node);
      }
    }
    return requests.map((request) => {
      if (!request || !request.settingsId || !request.property) {
        return undefined;
      }
      const node = byServiceSettingsId.get(`${request.service ?? ""}\0${request.settingsId}`);
      if (!node) {
        return undefined;
      }
      if (request.scope === "metadata") {
        return request.property === "Name" ? node.name : undefined;
      }
      if (!node.detailsLoaded) {
        return undefined;
      }
      if (request.scope === "attribute") {
        return node.attributes[request.property];
      }
      return node.properties[request.property];
    });
  }

  public rememberNode(node: FileExplorerNode, authoritative = false): FileExplorerNode {
    const existing = this.nodesById.get(node.treeId);
    if (existing) {
      existing.id = node.id;
      existing.kind = node.kind;
      existing.service = node.service;
      existing.name = node.name;
      existing.className = node.className;
      existing.settingsId = node.settingsId;
      existing.projectionSettingsId = node.projectionSettingsId;
      existing.index = node.index;
      existing.parentTreeId = node.parentTreeId;
      existing.hasChildren = node.hasChildren;
      existing.hasPackageLink = node.hasPackageLink;
      existing.settingsFile = node.settingsFile;
      existing.sourcePath = authoritative ? node.sourcePath : node.sourcePath ?? existing.sourcePath;
      existing.pathSegments = node.pathSegments.length > 0 ? node.pathSegments : existing.pathSegments;
      existing.pathOrdinals = node.pathOrdinals.length > 0 ? node.pathOrdinals : existing.pathOrdinals;
      existing.detailsLoaded = authoritative ? node.detailsLoaded : existing.detailsLoaded || node.detailsLoaded;
      if (authoritative || Object.keys(node.properties).length > 0) {
        existing.properties = node.properties;
      }
      if (authoritative || Object.keys(node.attributes).length > 0) {
        existing.attributes = node.attributes;
      }
      return existing;
    }
    this.nodesById.set(node.treeId, node);
    if (node.kind === "service" && !this.rootIds.includes(node.treeId)) {
      this.rootIds.push(node.treeId);
      this.rootIds = this.sort(this.getRoots()).map((root) => root.treeId);
    }
    return node;
  }

  public getRoots(): FileExplorerNode[] {
    return this.rootIds
      .map((id) => this.nodesById.get(id))
      .filter((node): node is FileExplorerNode => node !== undefined);
  }

  public invalidateServices(services: string[]): void {
    const targets = new Set(services);
    for (const [treeId, node] of this.nodesById) {
      if (!targets.has(node.service)) {
        continue;
      }
      if (node.kind === "service") {
        node.children = [];
        node.loaded = false;
        node.detailsLoaded = false;
        node.properties = {};
        node.attributes = {};
        continue;
      }
      this.nodesById.delete(treeId);
    }
    for (const service of targets) {
      this.loadedServices.delete(service);
    }
  }

  public getChildren(node: FileExplorerNode): FileExplorerNode[] {
    return node.children
      .map((id) => this.nodesById.get(id))
      .filter((child): child is FileExplorerNode => child !== undefined);
  }

  public async sourcePathsForSubtree(node: FileExplorerNode): Promise<string[]> {
    const generation = this.projectGeneration;
    const root = await this.ensureLoaded(node);
    if (!this.isProjectGenerationCurrent(generation)) {
      return [];
    }
    const paths = new Set<string>();
    const visited = new Set<string>();
    const visit = async (current: FileExplorerNode): Promise<void> => {
      if (!this.isProjectGenerationCurrent(generation)) {
        return;
      }
      if (!visited.add(current.treeId)) {
        return;
      }
      if (current.sourcePath) {
        paths.add(current.sourcePath);
      }
      if (current.hasChildren || current.children.length > 0) {
        await this.loadChildren(current);
        if (!this.isProjectGenerationCurrent(generation)) {
          return;
        }
        for (const child of this.getChildren(current)) {
          await visit(child);
        }
      }
    };
    await visit(root);
    return [...paths];
  }

  public sort(nodes: FileExplorerNode[]): FileExplorerNode[] {
    return nodes.sort(compareExplorerNodes);
  }

  public async refresh(): Promise<void> {
    const config = getExplorerConfig();
    const root = srcRoot(config);
    const serviceNames = new Map<string, string>();
    const addService = (service: string): void => {
      const canonical = explorerServiceForPath(config, service);
      if (canonical) {
        serviceNames.set(canonical.toLowerCase(), canonical);
      }
    };
    this.loadedServices.clear();
    if (fs.existsSync(root)) {
      for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
        if (entry.isDirectory()) {
          addService(entry.name);
        }
      }
    }
    for (const service of config.services) {
      addService(service);
    }

    const serviceList = Array.from(serviceNames.values());

    this.nodesById.clear();
    this.rootIds = serviceList
      .map((service) => {
        const settingsFile = settingsFileForService(config, service);
        const node: FileExplorerNode = {
          id: serviceTreeId(service),
          treeId: serviceTreeId(service),
          kind: "service",
          service,
          name: service,
          className: service,
          parentTreeId: null,
          children: [],
          loaded: false,
          detailsLoaded: false,
          hasChildren: true,
          settingsFile,
          pathSegments: [service],
          pathOrdinals: [1],
          properties: {},
          attributes: {},
        };
        this.nodesById.set(node.treeId, node);
        return node.treeId;
      });
    this.rootIds = this.sort(this.getRoots()).map((node) => node.treeId);
  }

  public async refreshServices(services: string[]): Promise<void> {
    const generation = this.projectGeneration;
    const config = getExplorerConfig();
    const uniqueServices = canonicalExplorerServices(config, services);
    for (const service of uniqueServices) {
      await this.refreshService(config, service);
      if (!this.isProjectGenerationCurrent(generation)) {
        return;
      }
    }
  }

  public servicesFromSettingsFiles(settingsFiles: string[]): string[] {
    const config = getExplorerConfig();
    const services: string[] = [];
    for (const settingsFile of settingsFiles) {
      const key = normalizeFilesystemPathKey(settingsFile);
      const owner = Array.from(this.nodesById.values())
        .find((node) => normalizeFilesystemPathKey(node.settingsFile) === key);
      const service = owner?.service ?? serviceFromSettingsFile(config, settingsFile);
      if (service) {
        services.push(service);
      }
    }
    return Array.from(new Set(services));
  }

  private async refreshService(config: ExplorerConfig, service: string): Promise<void> {
    const generation = this.projectGeneration;
    const serviceNode = this.nodesById.get(serviceTreeId(service));
    const settingsFile = settingsFileForService(config, service);
    if (!serviceNode) {
      await this.refresh();
      return;
    }

    if (this.loadedServices.has(service)) {
      await this.loadServiceDump(config, service);
      return;
    }

    const loadedNodeIds = Array.from(this.nodesById.values())
      .filter((node) => node.service === service && node.loaded)
      .sort((a, b) => a.pathSegments.length - b.pathSegments.length)
      .map((node) => node.treeId);

    if (loadedNodeIds.length === 0) {
      try {
        const counts = await runBytecodeBatchOne<CliExplorerCounts>(config, settingsFile, service, { type: "counts" });
        if (!this.isProjectGenerationCurrent(generation)) {
          return;
        }
        serviceNode.hasChildren = (counts.rootChildren ?? 0) > 0;
        serviceNode.settingsFile = counts.settingsFile;
        return;
      } catch {
        return;
      }
    }

    for (const treeId of loadedNodeIds) {
      const node = this.nodesById.get(treeId);
      if (node) {
        await this.loadChildren(node);
        if (!this.isProjectGenerationCurrent(generation)) {
          return;
        }
      }
    }
  }

  public async ensureLoaded(node: FileExplorerNode): Promise<FileExplorerNode> {
    const current = this.nodesById.get(node.treeId);
    if (!current) {
      throw new Error("The selected instance no longer belongs to the active Renium project.");
    }
    if (current.loaded) {
      return current;
    }
    await this.loadChildren(current);
    const reloaded = this.nodesById.get(current.treeId);
    if (!reloaded) {
      throw new Error("The selected instance disappeared while it was loading.");
    }
    return reloaded;
  }

  public async loadDetails(node: FileExplorerNode): Promise<FileExplorerNode> {
    const generation = this.projectGeneration;
    const loaded = this.nodesById.get(node.treeId);
    if (!loaded) {
      throw new Error("The selected instance no longer belongs to the active Renium project.");
    }
    if (loaded.detailsLoaded) {
      return loaded;
    }
    if (!loaded.settingsId) {
      if (loaded.kind === "service") {
        await this.loadChildren(loaded);
        return this.nodesById.get(loaded.treeId) ?? loaded;
      }
      return loaded;
    }
    const config = getExplorerConfig();
    const raw = await runBytecodeBatchOne<CliServiceNode>(config, loaded.settingsFile, loaded.service, {
      type: "instance",
      output: "full",
      id: loaded.projectionSettingsId ?? loaded.settingsId,
    });
    if (!this.isProjectGenerationCurrent(generation)) {
      return this.nodesById.get(node.treeId) ?? node;
    }
    loaded.name = raw.name;
    loaded.className = raw.className;
    loaded.index = raw.index;
    loaded.sourcePath = raw.sourcePath;
    loaded.pathSegments = raw.pathSegments ?? loaded.pathSegments;
    loaded.pathOrdinals = raw.pathOrdinals ?? loaded.pathOrdinals;
    loaded.properties = safeObject(raw.properties);
    loaded.attributes = safeObject(raw.attributes);
    loaded.hasChildren = (raw.childCount ?? raw.children?.length ?? loaded.children.length) > 0;
    loaded.hasPackageLink = raw.hasPackageLink === true;
    loaded.detailsLoaded = true;
    return loaded;
  }

  private async ensureDetails(node: FileExplorerNode): Promise<FileExplorerNode> {
    const current = this.nodesById.get(node.treeId) ?? node;
    return current.detailsLoaded ? current : this.loadDetails(current);
  }

  public async loadService(service: string): Promise<void> {
    const serviceNode = this.nodesById.get(serviceTreeId(service));
    if (serviceNode) {
      await this.loadChildren(serviceNode);
    }
  }

  public async loadAllServices(): Promise<void> {
    const config = getExplorerConfig();
    const roots = this.getRoots().filter((root) => !this.loadedServices.has(root.service));
    const generation = this.projectGeneration;
    let cursor = 0;
    const workers = Array.from({ length: Math.min(4, roots.length) }, async () => {
      for (;;) {
        const root = roots[cursor];
        cursor += 1;
        if (!root) {
          return;
        }
        await this.loadServiceDump(config, root.service);
        if (!this.isProjectGenerationCurrent(generation)) {
          return;
        }
      }
    });
    await Promise.all(workers);
  }

  private applyRawNode(
    node: FileExplorerNode,
    raw: CliServiceNode,
    settingsFile: string,
    fallbackPathSegments: string[],
    fallbackPathOrdinals: number[],
  ): void {
    node.settingsId = raw.canonicalSettingsId ?? raw.settingsId;
    node.projectionSettingsId = raw.settingsId;
    node.settingsFile = raw.settingsFile ?? settingsFile;
    node.index = raw.index;
    node.name = raw.name;
    node.className = raw.className;
    node.pathSegments = raw.pathSegments ?? fallbackPathSegments;
    node.pathOrdinals = raw.pathOrdinals ?? fallbackPathOrdinals;
    node.sourcePath = raw.sourcePath;
    node.properties = safeObject(raw.properties);
    node.attributes = safeObject(raw.attributes);
    node.hasPackageLink = raw.hasPackageLink === true;
  }

  private instanceNodeFromRaw(
    raw: CliServiceNode,
    service: string,
    settingsFile: string,
    parentTreeId: string,
    options: {
      children: string[];
      loaded: boolean;
      detailsLoaded: boolean;
    },
  ): FileExplorerNode {
    const treeId = normalizeId(service, raw.settingsId);
    return {
      id: treeId,
      treeId,
      kind: "instance",
      service,
      settingsId: raw.canonicalSettingsId ?? raw.settingsId,
      projectionSettingsId: raw.settingsId,
      index: raw.index,
      name: raw.name,
      className: raw.className,
      parentTreeId,
      children: options.children,
      loaded: options.loaded,
      detailsLoaded: options.detailsLoaded,
      hasChildren: (raw.childCount ?? raw.children?.length ?? 0) > 0,
      hasPackageLink: raw.hasPackageLink === true,
      settingsFile: raw.settingsFile ?? settingsFile,
      sourcePath: raw.sourcePath,
      pathSegments: raw.pathSegments ?? [],
      pathOrdinals: raw.pathOrdinals ?? [],
      properties: safeObject(raw.properties),
      attributes: safeObject(raw.attributes),
    };
  }

  private async loadServiceDump(config: ExplorerConfig, service: string): Promise<void> {
    const generation = this.projectGeneration;
    const settingsFile = settingsFileForService(config, service);
    const serviceNode = this.nodesById.get(serviceTreeId(service));
    if (!serviceNode) {
      this.loadedServices.add(service);
      return;
    }
    const dump = await runBytecodeBatchOne<CliServiceDump>(config, settingsFile, service, { type: "service" });
    if (!this.isProjectGenerationCurrent(generation)) {
      return;
    }
    const rootId = dump.rootIds[0];
    const rootRaw = dump.nodes.find((raw) => raw.settingsId === rootId) ?? dump.nodes.find((raw) => !raw.parentId);
    if (!rootRaw) {
      serviceNode.children = [];
      serviceNode.loaded = true;
      serviceNode.detailsLoaded = true;
      serviceNode.hasChildren = false;
      this.loadedServices.add(service);
      return;
    }

    this.removeKnownChildren(serviceNode);
    this.applyRawNode(serviceNode, rootRaw, dump.settingsFile, [service], [1]);
    serviceNode.children = (rootRaw.children ?? []).map((childId) => normalizeId(service, childId));
    serviceNode.hasChildren = (rootRaw.childCount ?? serviceNode.children.length) > 0;
    serviceNode.loaded = true;
    serviceNode.detailsLoaded = true;

    for (const raw of dump.nodes) {
      if (raw.settingsId === rootRaw.settingsId) {
        continue;
      }
      const treeId = normalizeId(service, raw.settingsId);
      const parentTreeId = raw.parentId === rootRaw.settingsId
        ? serviceNode.treeId
        : raw.parentId
          ? normalizeId(service, raw.parentId)
          : serviceNode.treeId;
      const childNode = this.instanceNodeFromRaw(raw, service, dump.settingsFile, parentTreeId, {
        children: (raw.children ?? []).map((childId) => normalizeId(service, childId)),
        loaded: true,
        detailsLoaded: true,
      });
      this.nodesById.set(treeId, childNode);
    }
    this.loadedServices.add(service);
  }

  public async loadChildren(node: FileExplorerNode): Promise<void> {
    const generation = this.projectGeneration;
    const config = getExplorerConfig();
    const settingsFile = settingsFileForService(config, node.service);

    const op: CliBatchOp = { type: "children" };
    if (node.kind !== "service") {
      if (node.projectionSettingsId ?? node.settingsId) {
        op.id = node.projectionSettingsId ?? node.settingsId;
      } else if (node.index !== undefined) {
        op.x = node.index;
      } else {
        return;
      }
    }

    const dump = await runBytecodeBatchOne<CliChildrenDump>(config, settingsFile, node.service, op);
    if (!this.isProjectGenerationCurrent(generation)) {
      return;
    }
    const parentRaw = dump.parent;
    this.applyRawNode(node, parentRaw, dump.settingsFile, node.pathSegments, node.pathOrdinals);
    node.hasChildren = (parentRaw.childCount ?? dump.children.length) > 0;
    node.loaded = true;
    node.detailsLoaded = true;

    this.removeKnownChildren(node);
    node.children = dump.children.map((raw) => normalizeId(node.service, raw.settingsId));
    for (const raw of dump.children) {
      const treeId = normalizeId(node.service, raw.settingsId);
      const parentTreeId = raw.parentId === parentRaw.settingsId
        ? node.treeId
        : raw.parentId
          ? normalizeId(dump.service, raw.parentId)
          : serviceTreeId(dump.service);
      const childNode = this.instanceNodeFromRaw(raw, dump.service, dump.settingsFile, parentTreeId, {
        children: [],
        loaded: false,
        detailsLoaded: true,
      });
      this.nodesById.set(treeId, childNode);
    }
  }

  private removeKnownChildren(node: FileExplorerNode): void {
    for (const childId of node.children) {
      this.removeSubtree(childId);
    }
    node.children = [];
  }

  private removeSubtree(treeId: string): void {
    const node = this.nodesById.get(treeId);
    if (!node || node.kind === "service") {
      return;
    }
    for (const childId of node.children) {
      this.removeSubtree(childId);
    }
    this.nodesById.delete(treeId);
  }

  public async setValue(node: FileExplorerNode, scope: "metadata" | "property" | "attribute", name: string, valueText: string, options: { skipStudioPush?: boolean } = {}): Promise<string[]> {
    const generation = this.projectGeneration;
    const loaded = await this.ensureDetails(node);
    if (generation !== this.projectGeneration) {
      throw new Error("The active Renium project changed before the edit could be applied.");
    }
    if (loaded.index === undefined && !loaded.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    if (scope === "metadata" && name !== "Name" && loaded.kind === "service") {
      throw new Error("Service parent and class are read-only.");
    }
    if (scope === "metadata" && name !== "Name" && isProtectedStarterPlayerContainer(loaded)) {
      throw new Error(`${loaded.name} metadata is read-only.`);
    }
    const config = getExplorerConfig();
    let propertyName = name;
    let parsedValue = scope === "metadata" && name === "Parent"
      ? this.resolveParentValue(loaded, parseJsonValue(valueText))
      : parseJsonValue(valueText);
    const oldParentTreeId = loaded.parentTreeId;
    const newParentSettingsId =
      scope === "metadata" && name === "Parent" && typeof parsedValue === "string" ? parsedValue : undefined;
    if (scope === "property" && usesDisabledProperty(loaded.className) && name === "Enabled") {
      propertyName = "Disabled";
      parsedValue = !(parsedValue === true);
    }
    let fileWritesPaused = false;
    const resumeFileWrites = async (paths: string[]): Promise<void> => {
      if (!fileWritesPaused) {
        return;
      }
      await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
        paths,
        fileWrites: "resume",
      });
      fileWritesPaused = false;
    };
    const fail = async (error: unknown): Promise<never> => {
      try {
        if (fileWritesPaused) {
          await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
            fileWrites: "resume",
          });
          fileWritesPaused = false;
        }
      } catch (resumeError) {
        const original = error instanceof Error ? error.message : String(error);
        const resume = resumeError instanceof Error ? resumeError.message : String(resumeError);
        throw new Error(`${original}; live sync also failed to resume: ${resume}`);
      }
      throw error;
    };
    try {
      await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
        fileWrites: "pause",
      });
      fileWritesPaused = true;
    } catch (error) {
      return fail(error);
    }
    const batchDir = path.join(config.projectRoot, ".renium", "editor-property-batches");
    fs.mkdirSync(batchDir, { recursive: true });
    const batchPath = path.join(
      batchDir,
      `explorer-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.json`,
    );
    fs.writeFileSync(batchPath, JSON.stringify([{
      service: loaded.service,
      settingsId: loaded.settingsId,
      className: loaded.className,
      pathSegments: loaded.pathSegments,
      pathOrdinals: loaded.pathOrdinals,
      scope,
      property: propertyName,
      value: parsedValue,
    }]), "utf8");
    let batchResult: {
      applied?: number;
      filtered?: number;
      changedPaths?: string[];
      sourcePaths?: Array<{ path?: string }>;
    };
    try {
      batchResult = await runJsonCli(config, [
        "bytecode-apply-property-batch",
        "--project-root",
        config.projectRoot,
        "--input",
        batchPath,
        "--direction",
        "files-to-studio",
      ]);
    } catch (error) {
      return fail(error);
    } finally {
      try {
        fs.unlinkSync(batchPath);
      } catch {
      }
    }
    if (batchResult.applied !== 1) {
      return fail(new Error(batchResult.filtered === 1
        ? "This field is excluded by the project filters."
        : "The project did not apply this property edit."));
    }
    const changedPaths = Array.from(new Set([
      ...(batchResult.changedPaths ?? []),
      ...(batchResult.sourcePaths ?? [])
        .map((entry) => entry.path)
        .filter((value): value is string => typeof value === "string" && value.length > 0),
    ]));
    try {
      if (generation !== this.projectGeneration) {
        throw new Error("The active Renium project changed while the edit was being applied.");
      }
      if (scope === "metadata") {
        if (name === "Parent") {
          const oldParent = oldParentTreeId ? this.nodesById.get(oldParentTreeId) : undefined;
          if (oldParent) {
            await this.loadChildren(oldParent);
          }
          const newParent = newParentSettingsId
            ? this.nodesById.get(normalizeId(loaded.service, newParentSettingsId))
            : undefined;
          if (newParent) {
            await this.loadChildren(newParent);
          } else {
            await this.loadService(loaded.service);
          }
        } else {
          await this.reloadParentChildren(loaded);
        }
      } else {
        if (scope === "property") {
          loaded.properties[propertyName] = parsedValue;
        } else {
          loaded.attributes[propertyName] = parsedValue;
        }
        loaded.detailsLoaded = true;
      }
      if (!options.skipStudioPush) {
        await vscode.commands.executeCommand("renium.pushEditorPathsNow", changedPaths, {
          projectRoot: config.projectRoot,
          taskName: "Explorer -> Studio sync",
          targetSettingsId: loaded.settingsId,
          targetProperty: propertyName,
        });
      }
      await resumeFileWrites(options.skipStudioPush ? changedPaths : []);
    } catch (error) {
      return fail(error);
    }
    return changedPaths;
  }

  private async reloadParentChildren(node: FileExplorerNode): Promise<void> {
    const parent = node.parentTreeId ? this.nodesById.get(node.parentTreeId) : undefined;
    if (parent) {
      await this.loadChildren(parent);
      return;
    }
    await this.loadService(node.service);
  }

  private resolveParentValue(node: FileExplorerNode, value: unknown): unknown {
    if (node.kind === "service") {
      throw new Error("Service parent is read-only.");
    }
    if (value === null || typeof value === "number") {
      return value;
    }
    if (typeof value !== "string") {
      return value;
    }
    const text = value.trim();
    if (text.length === 0) {
      return value;
    }
    if (text === "game") {
      const serviceRoot = this.nodesById.get(serviceTreeId(node.service));
      if (serviceRoot?.settingsId) {
        return serviceRoot.settingsId;
      }
    }
    const candidates = Array.from(this.nodesById.values()).filter((candidate) => (
      candidate.service === node.service &&
      candidate.treeId !== node.treeId &&
      Boolean(candidate.settingsId)
    ));
    const idMatch = candidates.find((candidate) => (
      candidate.settingsId === text || candidate.treeId === text
    ));
    if (idMatch?.settingsId) {
      return idMatch.settingsId;
    }
    const qualifiedPath = (candidate: FileExplorerNode): string => candidate.pathSegments
      .map((segment, index) => {
        const ordinal = candidate.pathOrdinals[index] ?? 1;
        return ordinal > 1 ? `${segment}[${ordinal}]` : segment;
      })
      .join(".");
    const exactPathMatches = candidates.filter((candidate) => qualifiedPath(candidate) === text);
    if (exactPathMatches.length === 1) {
      return exactPathMatches[0].settingsId;
    }
    const looseMatches = candidates.filter((candidate) => (
      candidate.name === text || candidate.pathSegments.join(".") === text
    ));
    const matches = exactPathMatches.length > 0 ? exactPathMatches : looseMatches;
    if (matches.length === 1) {
      return matches[0].settingsId;
    }
    if (matches.length > 1) {
      const choices = matches
        .slice(0, 6)
        .map((candidate) => `${qualifiedPath(candidate)} (${candidate.settingsId})`)
        .join(", ");
      throw new Error(`Parent ${JSON.stringify(text)} is ambiguous. Use a bytecode id or ordinal-qualified path: ${choices}`);
    }
    return value;
  }

  public async renameInstance(node: FileExplorerNode, name: string): Promise<FileExplorerNode | undefined> {
    await this.setValue(node, "metadata", "Name", name);
    if (node.kind === "service") {
      return this.nodesById.get(serviceTreeId(node.service));
    }
    return node.settingsId ? this.nodesById.get(normalizeId(node.service, node.settingsId)) : this.nodesById.get(node.treeId);
  }

  public async moveInstance(node: FileExplorerNode, parent: FileExplorerNode): Promise<FileExplorerNode | undefined> {
    const loaded = await this.ensureLoaded(node);
    const loadedParent = await this.ensureLoaded(parent);
    if (loaded.kind === "service") {
      throw new Error("Service roots cannot be moved.");
    }
    if (isProtectedStarterPlayerContainer(loaded)) {
      throw new Error(`${loaded.name} is a fixed StarterPlayer container.`);
    }
    if (!loaded.settingsId || !loadedParent.settingsId) {
      throw new Error("Move requires bytecode ids on both instances.");
    }
    if (loaded.treeId === loadedParent.treeId || this.isDescendantOf(loadedParent, loaded.treeId)) {
      throw new Error("Cannot move an instance into itself or one of its descendants.");
    }
    if (loaded.service !== loadedParent.service) {
      throw new Error("Cross-service moves are not available because Studio cannot apply them atomically.");
    }
    await this.setValue(loaded, "metadata", "Parent", loadedParent.settingsId);
    return this.nodesById.get(normalizeId(loaded.service, loaded.settingsId));
  }

  public async addInstance(
    parent: FileExplorerNode,
    className: string,
    name: string,
    properties: Record<string, unknown> = {},
    attributes: Record<string, unknown> = {},
    pushToStudio = true,
  ): Promise<FileExplorerNode | undefined> {
    const generation = this.projectGeneration;
    const loadedParent = await this.ensureLoaded(parent);
    this.requireProjectGeneration(generation);
    if (!loadedParent.settingsId) {
      throw new Error("Parent instance has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "create",
      loadedParent.service,
      "-r",
      config.projectRoot,
      "-I",
      loadedParent.settingsId ?? "",
      "-c",
      className,
      "-n",
      name,
    ];
    appendRecordAssignments(args, "-p", properties);
    appendRecordAssignments(args, "-a", attributes);
    const result = await this.withPausedProjectWrite(
      () => runJsonCli<{ settingsId?: string; changedPaths?: string[]; sourceWrites?: string[] }>(config, args),
      async (written) => {
        this.requireProjectGeneration(generation);
        await this.loadChildren(loadedParent);
        this.requireProjectGeneration(generation);
        const changedPaths = written.changedPaths ?? written.sourceWrites ?? [];
        if (pushToStudio) {
          await this.pushChangedPathsToStudio(
            changedPaths,
            written.settingsId ? [written.settingsId] : undefined,
          );
          return [];
        }
        return changedPaths;
      },
    );
    if (pushToStudio) {
      this.requireProjectGeneration(generation);
    }
    return result.settingsId ? this.nodesById.get(normalizeId(loadedParent.service, result.settingsId)) : undefined;
  }

  public async cloneInstance(source: FileExplorerNode, parent: FileExplorerNode): Promise<FileExplorerNode | undefined> {
    const generation = this.projectGeneration;
    if (source.kind === "service") {
      throw new Error("Service roots cannot be copied.");
    }
    const loadedSource = await this.ensureDetails(await this.ensureLoaded(source));
    const loadedParent = await this.ensureLoaded(parent);
    this.requireProjectGeneration(generation);
    if (loadedSource.service !== loadedParent.service) {
      throw new Error("Cross-service copies need a subtree copy path before they can be full fidelity.");
    }
    if (!loadedSource.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    if (!loadedParent.settingsId) {
      throw new Error("Target parent has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "clone",
      loadedParent.service,
      "-r",
      config.projectRoot,
      "-i",
      loadedSource.settingsId,
      "-I",
      loadedParent.settingsId ?? "",
    ];
    const result = await this.withPausedProjectWrite(
      () => runJsonCli<CliCloneInstanceResult>(config, args),
      async (written) => {
        this.requireProjectGeneration(generation);
        await this.loadChildren(loadedParent);
        this.requireProjectGeneration(generation);
        await this.pushChangedPathsToStudio(written.changedPaths ?? [], written.settingsIds);
        return [];
      },
    );
    const current = result.rootSettingsId
      ? this.nodesById.get(normalizeId(loadedParent.service, result.rootSettingsId))
      : undefined;
    this.requireProjectGeneration(generation);
    return current;
  }

  public async exportModel(node: FileExplorerNode, outputPath: string, format: RobloxModelFormat): Promise<CliExportModelResult> {
    const generation = this.projectGeneration;
    const loaded = await this.ensureLoaded(node);
    this.requireProjectGeneration(generation);
    if (!loaded.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "export-model",
      loaded.service,
      "-r",
      config.projectRoot,
      "-o",
      outputPath,
      "--format",
      format,
    ];
    args.push("-i", loaded.settingsId ?? "");
    return runJsonCli<CliExportModelResult>(config, args);
  }

  public async importModel(parent: FileExplorerNode, modelPath: string): Promise<FileExplorerNode | undefined> {
    const generation = this.projectGeneration;
    const loadedParent = await this.ensureLoaded(parent);
    this.requireProjectGeneration(generation);
    if (!loadedParent.settingsId) {
      throw new Error("Target parent has no bytecode id.");
    }
    const config = getExplorerConfig();
    const args = [
      "import-model",
      loadedParent.service,
      "-r",
      config.projectRoot,
      "-m",
      modelPath,
      "-I",
      loadedParent.settingsId ?? "",
    ];
    const result = await this.withPausedProjectWrite(
      () => runJsonCli<CliImportModelResult>(config, args),
      async (written) => {
        this.requireProjectGeneration(generation);
        await this.loadChildren(loadedParent);
        this.requireProjectGeneration(generation);
        await this.pushChangedPathsToStudio(
          written.changedPaths ?? (written.sourceWrites ?? [])
            .map((write) => write.path)
            .filter((writePath): writePath is string => typeof writePath === "string" && writePath.length > 0),
          written.settingsIds,
        );
        return [];
      },
    );
    const created = result.rootSettingsIds?.[0]
      ? this.nodesById.get(normalizeId(loadedParent.service, result.rootSettingsIds[0]))
      : undefined;
    this.requireProjectGeneration(generation);
    return created;
  }

  public uniqueChildName(parent: FileExplorerNode, requested: string): string {
    const existing = new Set(this.getChildren(parent).map((child) => child.name));
    const base = requested.trim() || "Instance";
    if (!existing.has(base)) {
      return base;
    }
    let index = 2;
    let candidate = `${base} Copy`;
    while (existing.has(candidate)) {
      candidate = `${base} Copy ${index}`;
      index += 1;
    }
    return candidate;
  }

  public async removeInstance(node: FileExplorerNode): Promise<CliRemoveInstanceResult> {
    const generation = this.projectGeneration;
    const loaded = await this.ensureLoaded(node);
    this.requireProjectGeneration(generation);
    if (loaded.kind === "service") {
      throw new Error("Refusing to remove a service root.");
    }
    if (isProtectedStarterPlayerContainer(loaded)) {
      throw new Error(`${loaded.name} is a fixed StarterPlayer container.`);
    }
    if (!loaded.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    const config = getExplorerConfig();
    const parent = loaded.parentTreeId ? this.nodesById.get(loaded.parentTreeId) : undefined;
    const studioDeleteTarget: FileExplorerNode = {
      ...loaded,
      children: loaded.children.slice(),
      pathSegments: loaded.pathSegments.slice(),
      pathOrdinals: loaded.pathOrdinals.slice(),
      properties: { ...loaded.properties },
      attributes: { ...loaded.attributes },
    };
    const result = await this.withPausedProjectWrite(async () => {
      for (let attempt = 0; ; attempt++) {
        try {
          const removed = await runJsonCli<CliRemoveInstanceResult>(config, [
            "remove",
            loaded.service,
            "-r",
            config.projectRoot,
            "-i",
            loaded.settingsId ?? "",
          ]);
          this.requireProjectGeneration(generation);
          return removed;
        } catch (error) {
          if (attempt >= 2 || !isSettingsLockTimeout(error)) {
            throw error;
          }
          await delay(150 * (attempt + 1));
        }
      }
    }, async (removed) => {
      this.removeSubtree(loaded.treeId);
      if (parent) {
        parent.children = parent.children.filter((childId) => childId !== loaded.treeId);
        parent.hasChildren = parent.children.length > 0;
      }
      try {
        await this.pushDeleteToStudio(studioDeleteTarget, removed.changedPaths ?? []);
      } catch {
        await this.pushChangedPathsToStudio(removed.changedPaths ?? []);
      }
      return [];
    });
    return result;
  }

  public async desyncPackageLink(node: FileExplorerNode): Promise<CliDesyncPackageLinkResult> {
    const generation = this.projectGeneration;
    const loaded = await this.ensureLoaded(node);
    this.requireProjectGeneration(generation);
    if (loaded.kind === "service") {
      throw new Error("Select a package root or PackageLink instance.");
    }
    if (!loaded.settingsId) {
      throw new Error("Selected instance has no bytecode id.");
    }
    const config = getExplorerConfig();
    const result = await this.withPausedProjectWrite(
      () => runJsonCli<CliDesyncPackageLinkResult>(config, [
        "desync-package-link",
        loaded.service,
        "-r",
        config.projectRoot,
        "-i",
        loaded.settingsId ?? "",
      ]),
      async (written) => {
    this.requireProjectGeneration(generation);
    const removedLinks = Array.isArray(written.removedPackageLinks) ? written.removedPackageLinks : [];
    const studioDeleteTargets = removedLinks.map((link, offset) => {
      const treeId = link.settingsId ? normalizeId(loaded.service, link.settingsId) : `${loaded.treeId}:PackageLink:${offset}`;
      const existing = this.nodesById.get(treeId);
      if (existing) {
        return {
          ...existing,
          children: existing.children.slice(),
          pathSegments: existing.pathSegments.slice(),
          pathOrdinals: existing.pathOrdinals.slice(),
          properties: { ...existing.properties },
          attributes: { ...existing.attributes },
        };
      }
      return {
        id: treeId,
        treeId,
        kind: "instance" as const,
        service: loaded.service,
        name: link.name ?? "PackageLink",
        className: link.className ?? "PackageLink",
        settingsId: link.settingsId,
        parentTreeId: loaded.treeId,
        children: [],
        loaded: false,
        detailsLoaded: false,
        hasChildren: false,
        settingsFile: loaded.settingsFile,
        sourcePath: undefined,
        pathSegments: Array.isArray(link.pathSegments) && link.pathSegments.length > 0
          ? link.pathSegments
          : loaded.pathSegments.concat("PackageLink"),
        pathOrdinals: [],
        properties: {},
        attributes: {},
      };
    });
    const removedTreeIds = new Set(studioDeleteTargets.map((target) => target.treeId));
    const parent = loaded.parentTreeId ? this.nodesById.get(loaded.parentTreeId) : undefined;
    for (const treeId of removedTreeIds) {
      this.removeSubtree(treeId);
    }
    if (loaded.className === "PackageLink") {
      this.removeSubtree(loaded.treeId);
      if (parent) {
        parent.children = parent.children.filter((childId) => childId !== loaded.treeId);
        parent.hasChildren = parent.children.length > 0;
        parent.hasPackageLink = false;
      }
    } else {
      loaded.children = loaded.children.filter((childId) => !removedTreeIds.has(childId));
      loaded.hasChildren = loaded.children.length > 0;
      loaded.hasPackageLink = false;
    }
    try {
      for (const target of studioDeleteTargets) {
        await this.pushDeleteToStudio(target, [written.settingsFile ?? loaded.settingsFile]);
        this.requireProjectGeneration(generation);
      }
    } catch {
      this.requireProjectGeneration(generation);
      await this.pushSettingsToStudio(written.settingsFile ?? loaded.settingsFile);
      this.requireProjectGeneration(generation);
    }
    if (loaded.className === "PackageLink") {
      if (parent) {
        await this.loadChildren(parent);
      } else {
        await this.loadService(loaded.service);
      }
    } else {
      await this.loadChildren(loaded);
    }
    return [];
      },
    );
    return result;
  }

  private isDescendantOf(node: FileExplorerNode, ancestorTreeId: string): boolean {
    let current: FileExplorerNode | undefined = node;
    while (current?.parentTreeId) {
      if (current.parentTreeId === ancestorTreeId) {
        return true;
      }
      current = this.nodesById.get(current.parentTreeId);
    }
    return false;
  }

  public async pushSettingsToStudio(settingsFile: string, targetSettingsIds?: string[], extraPaths: string[] = []): Promise<void> {
    const paths = [settingsFile];
    const seen = new Set(paths);
    for (const extraPath of extraPaths) {
      if (extraPath && !seen.has(extraPath)) {
        seen.add(extraPath);
        paths.push(extraPath);
      }
    }
    await vscode.commands.executeCommand("renium.pushEditorPathsNow", paths, {
      projectRoot: getExplorerConfig().projectRoot,
      taskName: "Explorer -> Studio sync",
      targetSettingsIds,
    });
  }

  private async pushChangedPathsToStudio(paths: string[], targetSettingsIds?: string[]): Promise<void> {
    const changedPaths = Array.from(new Set(paths.filter((value) => typeof value === "string" && value.length > 0)));
    if (changedPaths.length === 0) {
      throw new Error("The project mutation returned no changed source paths.");
    }
    await vscode.commands.executeCommand("renium.pushEditorPathsNow", changedPaths, {
      projectRoot: getExplorerConfig().projectRoot,
      taskName: "Explorer -> Studio sync",
      targetSettingsIds,
    });
  }

  public async pushPropertyToStudio(
    node: FileExplorerNode,
    scope: "metadata" | "property" | "attribute",
    propertyName: string,
    value: unknown,
    changedPaths: string[] = [node.settingsFile],
  ): Promise<"applied" | "skipped"> {
    const targetProperty = String(propertyName).trim();
    if (targetProperty.length === 0) {
      return "skipped";
    }
    try {
      const outcome = await vscode.commands.executeCommand<"applied" | "skipped">(
        "renium.pushEditorPropertyNow",
        {
          ...this.editorMutationTarget(node, changedPaths),
          scope,
          property: targetProperty,
          value,
        },
      );
      if (outcome !== "applied") {
        await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
          paths: changedPaths,
          fileWrites: "queue",
        });
        return "skipped";
      }
      return "applied";
    } catch (error) {
      await vscode.commands.executeCommand("renium.noteProgrammaticEditorWrite", {
        paths: changedPaths,
        fileWrites: "queue",
      });
      throw error;
    }
  }

  private editorMutationTarget(node: FileExplorerNode, changedPaths: string[]): Record<string, unknown> {
    return {
      projectRoot: getExplorerConfig().projectRoot,
      settingsFile: node.settingsFile,
      service: node.service,
      settingsId: node.settingsId,
      className: node.className,
      pathSegments: node.pathSegments.length > 0
        ? node.pathSegments.slice()
        : [node.service, node.name].filter((segment) => segment.length > 0),
      pathOrdinals: node.pathOrdinals.slice(),
      changedPaths,
    };
  }

  public async pushDeleteToStudio(node: FileExplorerNode, changedPaths: string[] = [node.settingsFile]): Promise<void> {
    await vscode.commands.executeCommand(
      "renium.pushEditorDeleteNow",
      this.editorMutationTarget(node, changedPaths),
    );
  }

}
