import * as childProcess from "child_process";
import * as fs from "fs";
import * as vscode from "vscode";
import {
  spawnTrackedProcess,
  terminateProcess,
} from "./processSupervisor";

type ExplorerBackendConfig = {
  projectRoot: string;
  srcDir: string;
  cliPath: string;
  services: string[];
};

export type ExplorerViewMode = "normal" | "search";

export type ExplorerRowSummary = {
  id: string;
  settingsId?: string;
  settingsFile?: string;
  index?: number;
  kind: "service" | "instance";
  service: string;
  name: string;
  className: string;
  parentId?: string | null;
  depth: number;
  hasChildren: boolean;
  childCount?: number;
  hasPackageLink?: boolean;
  expanded?: boolean;
  matched?: boolean;
  iconName?: string;
  isScript?: boolean;
  disabled?: boolean;
  locked?: boolean;
  canRename?: boolean;
  canMove?: boolean;
  canDelete?: boolean;
  sourcePath?: string;
  pathSegments?: string[];
  pathOrdinals?: number[];
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
};

export type ExplorerBackendResponse = {
  type?: string;
  requestId?: number;
  snapshotVersion?: number;
  viewVersion?: number;
  mode?: ExplorerViewMode;
  start?: number;
  totalRows?: number;
  rows?: ExplorerRowSummary[];
  matchIds?: string[];
  details?: {
    id?: string;
    treeId?: string;
    kind?: "service" | "instance";
    service?: string;
    name?: string;
    className?: string;
    parentId?: string | null;
    parentTreeId?: string | null;
    children?: string[];
    loaded?: boolean;
    detailsLoaded?: boolean;
    hasChildren?: boolean;
    hasPackageLink?: boolean;
    settingsId?: string;
    projectionSettingsId?: string;
    index?: number;
    settingsFile?: string;
    sourcePath?: string;
    pathSegments?: string[];
    pathOrdinals?: number[];
    properties?: Record<string, unknown>;
    attributes?: Record<string, unknown>;
    childCount?: number;
    matched?: boolean;
  };
  searchId?: number;
  state?: string;
  loaded?: number;
  total?: number;
  matchCount?: number;
  rowIndex?: number;
  scrollToSelected?: boolean;
  code?: string;
  message?: string;
  stale?: boolean;
};

export type ExplorerRowRequest = {
  start: number;
  count: number;
  mode: ExplorerViewMode;
  scrollToSelected: boolean;
  includeMatchIds: boolean;
  revision?: number;
  generation: number;
};

type PendingRequest = {
  resolve: (response: ExplorerBackendResponse) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
};

export class ExplorerBackendClient implements vscode.Disposable {
  private process: childProcess.ChildProcessWithoutNullStreams | undefined;
  private buffer = "";
  private requestId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private disposed = false;
  private starting: Promise<void> | undefined;
  private stopping: Promise<void> = Promise.resolve();
  private initialized = false;
  private processInitialized = false;

  public constructor(
    private readonly config: () => ExplorerBackendConfig,
    private readonly onEvent: (response: ExplorerBackendResponse) => void,
  ) {}

  public dispose(): void {
    this.disposed = true;
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(new Error(`Explorer backend request ${id} cancelled.`));
    }
    this.pending.clear();
    this.stopping = this.stopCurrentProcess();
  }

  public async initialize(): Promise<ExplorerBackendResponse> {
    await this.ensureStarted();
    const response = await this.request("initialize", {});
    this.initialized = true;
    this.processInitialized = true;
    return response;
  }

  public async ensureInitialized(): Promise<void> {
    if (this.initialized && this.processInitialized && this.process && !this.process.killed) {
      return;
    }
    await this.initialize();
  }

  public hasInitialized(): boolean {
    return this.initialized;
  }

  public hasPendingRequests(): boolean {
    return this.pending.size > 0;
  }

  public restart(): void {
    this.processInitialized = false;
    this.failAll(new Error("Explorer backend restarted."));
    this.stopping = this.stopCurrentProcess();
  }

  public getRows(
    start: number,
    count: number,
    mode: ExplorerViewMode,
    includeMatchIds = false,
  ): Promise<ExplorerBackendResponse> {
    return this.request(mode === "search" ? "searchRows" : "getRows", {
      start,
      count,
      mode,
      includeMatchIds,
    });
  }

  public expand(nodeId: string, mode: ExplorerViewMode): Promise<ExplorerBackendResponse> {
    return this.request("expand", { nodeId, mode });
  }

  public collapse(nodeId: string, mode: ExplorerViewMode): Promise<ExplorerBackendResponse> {
    return this.request("collapse", { nodeId, mode });
  }

  public selectDetails(nodeId: string): Promise<ExplorerBackendResponse> {
    return this.request("selectDetails", { nodeId });
  }

  public searchStart(query: string, searchId: number): Promise<ExplorerBackendResponse> {
    return this.request("searchStart", { query, searchId });
  }

  public clearSearch(): Promise<ExplorerBackendResponse> {
    return this.request("clearSearch", {});
  }

  public reloadServices(services: string[]): Promise<ExplorerBackendResponse> {
    return this.request("reloadServices", { services });
  }

  public revealNode(nodeId: string): Promise<ExplorerBackendResponse> {
    return this.request("revealNode", { nodeId });
  }

  private async request(
    type: string,
    payload: Record<string, unknown>,
  ): Promise<ExplorerBackendResponse> {
    let lastError: unknown;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      try {
        return await this.requestOnce(type, payload);
      } catch (error) {
        lastError = error;
        if (this.disposed || !this.isRestartableError(error)) {
          throw error;
        }
        this.process = undefined;
        this.processInitialized = false;
      }
    }
    throw lastError instanceof Error ? lastError : new Error(String(lastError));
  }

  private async requestOnce(
    type: string,
    payload: Record<string, unknown>,
  ): Promise<ExplorerBackendResponse> {
    await this.ensureStarted();
    if (type !== "initialize" && this.initialized && !this.processInitialized) {
      await this.requestOnce("initialize", {});
      this.processInitialized = true;
    }
    const child = this.process;
    if (!child || child.killed) {
      throw new Error("Explorer backend is not running.");
    }
    const requestId = this.requestId++;
    const message = this.encodeRequest(type, requestId, payload);
    return new Promise((resolve, reject) => {
      const timeoutMs = type === "searchStart" || type === "searchRows" ? 10_000 : 30_000;
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        this.processInitialized = false;
        this.stopping = this.stopCurrentProcess();
        void this.stopping.finally(() => {
          reject(new Error(`Explorer backend request timed out: ${type}`));
        });
      }, timeoutMs);
      this.pending.set(requestId, { resolve, reject, timer });
      child.stdin.write(`${JSON.stringify(message)}\n`, (error) => {
        if (!error) {
          return;
        }
        clearTimeout(timer);
        this.pending.delete(requestId);
        reject(error);
      });
    });
  }

  private encodeRequest(
    type: string,
    requestId: number,
    payload: Record<string, unknown>,
  ): Record<string, unknown> {
    const typeMap: Record<string, string> = {
      initialize: "init",
      getRows: "rows",
      expand: "exp",
      collapse: "col",
      selectDetails: "det",
      searchStart: "ss",
      searchRows: "sr",
      clearSearch: "cs",
      reloadServices: "rl",
      revealNode: "rv",
    };
    const keyMap: Record<string, string> = {
      nodeId: "n",
      mode: "m",
      start: "a",
      count: "c",
      includeMatchIds: "ids",
      query: "q",
      searchId: "sid",
      services: "s",
    };
    const message: Record<string, unknown> = { t: typeMap[type] ?? type, id: requestId };
    for (const [key, value] of Object.entries(payload)) {
      message[keyMap[key] ?? key] = value;
    }
    return message;
  }

  private isRestartableError(error: unknown): boolean {
    const message = error instanceof Error ? error.message : String(error);
    return message.includes("Explorer backend exited")
      || message.includes("Explorer backend request timed out")
      || message.includes("not running");
  }

  private async ensureStarted(): Promise<void> {
    await this.stopping;
    if (this.process && !this.process.killed) {
      return;
    }
    if (this.starting) {
      return this.starting;
    }
    this.starting = this.start();
    try {
      await this.starting;
    } finally {
      this.starting = undefined;
    }
  }

  private async start(): Promise<void> {
    if (this.disposed) {
      throw new Error("Explorer backend is disposed.");
    }
    const config = this.config();
    if (!fs.existsSync(config.cliPath)) {
      throw new Error(`Renium was not found at ${config.cliPath}. Build it or set renium.cliPath.`);
    }
    const { child } = spawnTrackedProcess(config.cliPath, [
      "ed",
      "-r",
      config.projectRoot,
      "-d",
      config.srcDir,
      "-s",
      config.services.join(","),
      "--parent-pid",
      String(process.pid),
    ], config.projectRoot);
    this.process = child;
    this.buffer = "";
    child.stdout.on("data", (data: Buffer | string) => {
      if (this.process === child) {
        this.handleStdout(data.toString());
      }
    });
    child.stderr.on("data", (data: Buffer | string) => {
      if (this.process !== child) {
        return;
      }
      const text = data.toString().trim();
      if (text) {
        console.error(text);
      }
    });
    child.on("error", (error) => {
      if (this.process === child) {
        this.failAll(error);
      }
    });
    child.on("close", (code) => {
      if (this.process !== child) {
        return;
      }
      this.process = undefined;
      this.processInitialized = false;
      if (code === 0 && this.pending.size === 0) {
        return;
      }
      if (!this.disposed) {
        this.failAll(new Error(`Explorer backend exited with code ${code ?? 130}.`));
      }
    });
  }

  private async stopCurrentProcess(): Promise<void> {
    const child = this.process;
    this.process = undefined;
    this.processInitialized = false;
    if (!child || child.exitCode !== null || child.signalCode !== null) {
      return;
    }
    try {
      child.stdin.write(`${JSON.stringify({ t: "quit", id: this.requestId++ })}\n`);
    } catch {
    }
    await terminateProcess(child);
  }

  private handleStdout(chunk: string): void {
    this.buffer += chunk;
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) {
        return;
      }
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      if (!line) {
        continue;
      }
      let response: ExplorerBackendResponse;
      try {
        response = JSON.parse(line) as ExplorerBackendResponse;
      } catch {
        continue;
      }
      const requestId = response.requestId;
      if (typeof requestId === "number") {
        const pending = this.pending.get(requestId);
        if (pending) {
          clearTimeout(pending.timer);
          this.pending.delete(requestId);
          if (response.type === "error" && !response.stale) {
            pending.reject(new Error(response.message ?? response.code ?? "Explorer backend failed."));
          } else {
            pending.resolve(response);
          }
        }
      }
      this.onEvent(response);
    }
  }

  private failAll(error: Error): void {
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(error);
      this.pending.delete(id);
    }
  }
}
