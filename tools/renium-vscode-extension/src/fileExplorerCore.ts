import * as crypto from "crypto";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import { reniumBinaryName, resolveReniumCliPath } from "./cliResolution";
import { resolveActiveExperiencePlace } from "./experience";
import type { VerdePropertiesData } from "./explorerProperties";
import { spawnTrackedProcess, terminateProcess } from "./processSupervisor";
import { canonicalExplorerServiceName, DEFAULT_SYNC_SERVICES } from "./serviceDefaults";
import {
  type SharedConfig,
  loadProjectSourceLocations,
  loadProjectSourceRoot,
  loadSharedConfig,
  sharedConfigValue,
} from "./sharedConfig";
import {
  SETTINGS_FILE_NAME,
  filesystemPathKey,
  isReniumSettingsFileName,
  pickWorkspaceRoot,
  resolveConfigPath,
  robloxModelFormatFromPath,
  robloxScriptFileName,
  safeFileComponent,
  type RobloxModelFormat,
} from "./utils";

const CLI_BINARY = reniumBinaryName();
let explorerExtensionRoot = "";
let packageDragDebugOutput: vscode.OutputChannel | undefined;

export function logPackageDragDebug(message: string): void {
  const level = vscode.workspace.getConfiguration("renium").get<string>("logLevel", "info");
  if (level !== "debug" && level !== "trace") {
    return;
  }
  if (!packageDragDebugOutput) {
    packageDragDebugOutput = vscode.window.createOutputChannel("Renium Package Drag");
  }
  packageDragDebugOutput.appendLine(`[${new Date().toISOString()}] ${message}`);
}

export function configureExplorerExtensionRoot(extensionRoot: string): void {
  explorerExtensionRoot = extensionRoot;
}

export type ExplorerConfig = {
  projectRoot: string;
  srcDir: string;
  cliPath: string;
  services: string[];
};

export type ViewVisibilityHandler = (viewType: string, visible: boolean) => void;

export type CliServiceNode = {
  id?: string;
  index: number;
  settingsId: string;
  canonicalSettingsId?: string;
  settingsFile?: string;
  name: string;
  className: string;
  parentId?: string | null;
  parentIndex?: number | null;
  children?: string[];
  childCount?: number;
  hasPackageLink?: boolean;
  pathSegments?: string[];
  pathOrdinals?: number[];
  sourcePath?: string;
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
};

export type CliServiceDump = {
  service: string;
  settingsFile: string;
  rootIds: string[];
  nodes: CliServiceNode[];
};

export type CliExplorerCounts = {
  service: string;
  settingsFile: string;
  rootChildren: number;
  descendants: number;
  instances: number;
};

export type CliChildrenDump = {
  service: string;
  settingsFile: string;
  parent: CliServiceNode;
  children: CliServiceNode[];
};

type CliBatchDump<T extends object> = {
  service: string;
  settingsFile: string;
  results: T[];
};

export type CliBatchOp = Record<string, unknown>;

export type CliCloneInstanceResult = {
  rootSettingsId?: string;
  settingsIds?: string[];
  sourceCopies?: Array<{ from?: string; to?: string }>;
  changedPaths?: string[];
};

export type CliExportModelResult = {
  ok?: boolean;
  output?: string;
  format?: string;
  rootSettingsIds?: string[];
  instances?: number;
};

export type CliImportModelResult = {
  ok?: boolean;
  rootSettingsIds?: string[];
  settingsIds?: string[];
  sourceWrites?: Array<{ settingsId?: string; path?: string }>;
  changedPaths?: string[];
};

export type CliRemoveInstanceResult = {
  ok?: boolean;
  removedIndexes?: number[];
  removedSourcePaths?: string[];
  changedPaths?: string[];
};

export type CliDesyncPackageLinkResult = {
  ok?: boolean;
  settingsFile?: string;
  removedPackageLinks?: Array<{
    settingsId?: string;
    name?: string;
    className?: string;
    pathSegments?: string[];
  }>;
};

export type FileExplorerNodeKind = "service" | "instance";

export type FileExplorerNode = {
  id: string;
  treeId: string;
  kind: FileExplorerNodeKind;
  service: string;
  name: string;
  className: string;
  settingsId?: string;
  projectionSettingsId?: string;
  index?: number;
  parentTreeId: string | null;
  children: string[];
  loaded: boolean;
  detailsLoaded: boolean;
  hasChildren: boolean;
  hasPackageLink?: boolean;
  settingsFile: string;
  sourcePath?: string;
  pathSegments: string[];
  pathOrdinals: number[];
  properties: Record<string, unknown>;
  attributes: Record<string, unknown>;
  searchMatched?: boolean;
};

export type ReadonlyInstanceInfo = {
  name?: string;
  className?: string;
  settingsId?: string;
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
  pathSegments?: string[];
};

export type CommandRunResult = {
  code: number;
  stdout: string;
  stderr: string;
};

export class ReadonlyExplorerScriptContentProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly contents = new Map<string, string>();
  private readonly changeEmitter = new vscode.EventEmitter<vscode.Uri>();
  private readonly closeSubscription = vscode.workspace.onDidCloseTextDocument((document) => {
    if (document.uri.scheme === "renium-readonly-script") {
      this.contents.delete(document.uri.toString());
    }
  });
  public readonly onDidChange = this.changeEmitter.event;

  public provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.toString()) ?? "";
  }

  public uriFor(node: FileExplorerNode, sourcePath: string, content: string): vscode.Uri {
    const fileName = robloxScriptFileName(node.name, node.className);
    const packageName = node.service.replace(/[<>:"/\\|?*\x00-\x1f]/g, "_") || "Renium";
    const uri = vscode.Uri.from({
      scheme: "renium-readonly-script",
      authority: "preview",
      path: `/${encodeURIComponent(packageName)}/${encodeURIComponent(fileName)}`,
      query: `id=${crypto.createHash("sha256").update(`${node.treeId}\0${sourcePath}`).digest("hex").slice(0, 24)}`,
    });
    this.contents.set(uri.toString(), content);
    this.changeEmitter.fire(uri);
    return uri;
  }

  public dispose(): void {
    this.contents.clear();
    this.closeSubscription.dispose();
    this.changeEmitter.dispose();
  }
}

type PackagePropertiesPayloadNode = {
  settingsId?: string;
  name?: string;
  className?: string;
  parentId?: string;
  pathSegments?: string[];
  properties?: Record<string, unknown>;
  attributes?: Record<string, unknown>;
};

export type PackagePropertiesPayload = {
  packageId?: string;
  packageName?: string;
  source?: string;
  sourcePath?: string;
  rootClass?: string | null;
  rootName?: string | null;
  node?: PackagePropertiesPayloadNode;
};

export type PropertiesUpdateMessage = {
  type: "updateProperties";
  properties: VerdePropertiesData;
  nodeName?: string;
  nodeClassName?: string;
  title?: string;
  readOnly?: boolean;
};

export function cleanPropertyText(value: unknown): string {
  if (value === null || value === undefined) {
    return "";
  }
  return String(value).trim();
}

export function sanitizePathSegments(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.map((segment) => cleanPropertyText(segment)).filter((segment) => segment.length > 0);
}

export type EditorHistoryManifest = {
  version?: number;
  createdUnixMs?: number;
  service?: string;
  sourcePath?: string;
  settingsId?: string;
  pathSegments?: string[];
  className?: string;
  propertyName?: string;
  propertyLabel?: string;
  settingsFile?: string;
  settingsBackup?: string;
  sourceBackup?: string;
};

export type ExplorerHistoryEntry = {
  id: string;
  service: string;
  className: string;
  settingsId?: string;
  sourcePath?: string;
  settingsFile?: string;
  pathSegments: string[];
  propertyName?: string;
  propertyLabel?: string;
  createdUnixMs: number;
  createdLabel: string;
  targetLabel: string;
  hasSourceBackup: boolean;
  hasSettingsBackup: boolean;
};

export type ExplorerHistoryTarget = ExplorerHistoryEntry & {
  openId: string;
  restoreId: string;
  editCount: number;
  firstCreatedUnixMs: number;
  lastCreatedUnixMs: number;
  timeLabel: string;
};

export type ExplorerHistoryGroup = {
  id: string;
  title: string;
  subtitle: string;
  createdUnixMs: number;
  firstCreatedUnixMs: number;
  lastCreatedUnixMs: number;
  entryCount: number;
  targetCount: number;
  services: string[];
  items: ExplorerHistoryTarget[];
};

export type PropertyEditHistoryItem = {
  service: string;
  settingsId?: string;
  treeId: string;
  className: string;
  sourcePath?: string;
  pathSegments: string[];
  pathOrdinals: number[];
  propertyKey: string;
  scope: "metadata" | "property";
  name: string;
  propertyLabel?: string;
  writePropertyName: string;
  studioPropertyName: string;
  beforeRawValue: unknown;
  afterRawValue: unknown;
  settingsFile: string;
  historyId?: string;
};

const valueIconFallbackClasses = new Set([
  "BinaryStringValue",
  "Color3Value",
  "DoubleConstrainedValue",
  "IntConstrainedValue",
  "IntValue",
  "NumberValue",
  "ObjectValue",
  "StringValue",
  "Vector3Value",
]);

function preferredIconAssetNameForClass(className: string): string {
  return valueIconFallbackClasses.has(className) ? "Value" : className;
}

function fallbackIconAssetNameForClass(className: string): string {
  if (valueIconFallbackClasses.has(className)) {
    return "Value";
  }
  return className.endsWith("Service") ? "Service" : "Class";
}

export function iconAssetNameForClass(className: string, availableIconNames?: ReadonlySet<string>): string {
  const preferred = preferredIconAssetNameForClass(className);
  if (!availableIconNames || availableIconNames.has(preferred)) {
    return preferred;
  }
  const fallback = fallbackIconAssetNameForClass(className);
  return availableIconNames.has(fallback) ? fallback : preferred;
}

export function loadAssetIconNames(extensionUri: vscode.Uri): string[] {
  const assetsPath = vscode.Uri.joinPath(extensionUri, "assets").fsPath;
  try {
    return fs.readdirSync(assetsPath)
      .filter((fileName) => fileName.toLowerCase().endsWith(".png"))
      .map((fileName) => path.basename(fileName, ".png"))
      .sort((a, b) => a.localeCompare(b));
  } catch {
    return [];
  }
}

export function workspaceRoot(): string {
  const root = pickWorkspaceRoot();
  if (!root) {
    throw new Error("Open a workspace folder before using Renium.");
  }
  return root;
}

export function isNoMatchingInstanceError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return /no matched instance|no matching instance|instance not found/i.test(message);
}

export function isSettingsLockTimeout(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return /timed out waiting for settings file lock/i.test(message);
}

export function resolveExplorerCliPath(root: string, configuredPath: string): string {
  return resolveReniumCliPath({
    configuredPath,
    extensionRoot: explorerExtensionRoot,
    roots: [root],
  });
}

export function getExplorerConfig(): ExplorerConfig {
  const root = workspaceRoot();
  const cfg = vscode.workspace.getConfiguration("renium", vscode.Uri.file(root));
  const read = <T>(shared: SharedConfig, key: string, defaultValue: T): T => {
    const inspected = cfg.inspect<T>(key);
    return inspected?.workspaceFolderValue
      ?? inspected?.workspaceValue
      ?? inspected?.globalValue
      ?? sharedConfigValue<T>(shared, key)
      ?? defaultValue;
  };
  const preliminaryShared = loadSharedConfig(root, root);
  const experienceRoot = resolveConfigPath(read(preliminaryShared, "projectRoot", "${workspaceFolder}"), root);
  const projectRoot = resolveActiveExperiencePlace(experienceRoot)?.projectRoot ?? experienceRoot;
  const shared = loadSharedConfig(root, projectRoot);
  const srcDir = loadProjectSourceRoot(projectRoot);
  const configuredCliPathRaw = read(shared, "cliPath", "").trim();
  const configuredCliPath = configuredCliPathRaw.length > 0
    ? resolveConfigPath(configuredCliPathRaw, root)
    : "";
  const cliPath = resolveExplorerCliPath(root, configuredCliPath);
  const servicesRaw = read<string[]>(shared, "services", [...DEFAULT_SYNC_SERVICES]);
  const services = Array.isArray(servicesRaw)
    ? distinctExplorerServices(servicesRaw.map((value) => canonicalExplorerServiceName(String(value))))
    : [...DEFAULT_SYNC_SERVICES];
  return { projectRoot, srcDir, cliPath, services };
}

export function srcRoot(config: ExplorerConfig): string {
  return path.join(config.projectRoot, config.srcDir);
}

function distinctExplorerServices(services: readonly string[]): string[] {
  const seen = new Set<string>();
  const result: string[] = [];
  for (const service of services) {
    const canonical = canonicalExplorerServiceName(service);
    const key = canonical.toLowerCase();
    if (!canonical || seen.has(key)) {
      continue;
    }
    seen.add(key);
    result.push(canonical);
  }
  return result;
}

export function explorerServiceForPath(config: ExplorerConfig, service: string): string {
  const trimmed = service.trim();
  const known = canonicalExplorerServiceName(trimmed);
  if (!trimmed || known !== trimmed) {
    return known;
  }
  const root = srcRoot(config);
  if (fs.existsSync(root)) {
    const diskEntry = fs.readdirSync(root, { withFileTypes: true })
      .find((entry) => entry.isDirectory() && entry.name.toLowerCase() === trimmed.toLowerCase());
    if (diskEntry) {
      return canonicalExplorerServiceName(diskEntry.name);
    }
  }
  const configured = config.services.find((candidate) => candidate.toLowerCase() === trimmed.toLowerCase());
  return configured ?? known;
}

export function canonicalExplorerServices(config: ExplorerConfig, services: readonly string[]): string[] {
  return distinctExplorerServices(services.map((service) => explorerServiceForPath(config, String(service))));
}

export function settingsFileForService(config: ExplorerConfig, service: string): string {
  return path.join(srcRoot(config), service, SETTINGS_FILE_NAME);
}

export function editorHistoryRoot(config: ExplorerConfig): string {
  return path.join(config.projectRoot, ".renium", "editor-history");
}

export function safeModelFileName(name: string, format: RobloxModelFormat): string {
  return `${safeFileComponent(name || "Model")}.${format}`;
}

function isRobloxModelFilePath(filePath: string): boolean {
  return robloxModelFormatFromPath(filePath) !== undefined;
}

function normalizeRobloxModelPath(raw: string): string | undefined {
  const trimmed = String(raw ?? "").trim();
  if (!trimmed) {
    return undefined;
  }
  let filePath = trimmed;
  if (/^file:/i.test(filePath)) {
    try {
      filePath = vscode.Uri.parse(filePath).fsPath;
    } catch {
      return undefined;
    }
  }
  const normalized = path.normalize(filePath);
  return isRobloxModelFilePath(normalized) ? normalized : undefined;
}

export function normalizeFilesystemPathKey(filePath: string): string {
  return filesystemPathKey(filePath).replace(/\\/g, "/");
}

export function nodeTargetPath(node: FileExplorerNode): string[] {
  return node.pathSegments.length > 0 ? node.pathSegments.slice() : [node.service, node.name];
}

export function nodeLinkPathKey(node: FileExplorerNode): string | undefined {
  const pathSegments = nodeTargetPath(node);
  return pathSegments.length < 2
    ? undefined
    : `${pathSegments[0]}\u0001${pathSegments.slice(1).join("/")}`;
}

export function linkPathKey(service: string, pathSegments: string[]): string | undefined {
  const segments = pathSegments.length > 0 ? pathSegments : [service];
  const normalized = segments[0] === service ? segments : [service, ...segments];
  return normalized.length < 2
    ? undefined
    : `${normalized[0]}\u0001${normalized.slice(1).join("/")}`;
}

export function normalizeRobloxModelPaths(rawPaths: string[] | undefined): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of rawPaths ?? []) {
    const normalized = normalizeRobloxModelPath(raw);
    if (!normalized) {
      continue;
    }
    const key = normalizeFilesystemPathKey(normalized);
    if (seen.has(key)) {
      continue;
    }
    seen.add(key);
    out.push(normalized);
  }
  return out;
}

export function cloneHistoryValue(value: unknown): unknown {
  try {
    return JSON.parse(JSON.stringify(value));
  } catch {
    return value;
  }
}

export function serviceFromSettingsFile(config: ExplorerConfig, filePath: string): string | undefined {
  if (!isReniumSettingsFileName(path.basename(filePath))) {
    return undefined;
  }
  const relativePath = path.relative(srcRoot(config), filePath);
  if (!relativePath || relativePath.startsWith("..") || path.isAbsolute(relativePath)) {
    return undefined;
  }
  const [service] = relativePath.split(/[\\/]/);
  return service ? explorerServiceForPath(config, service) : undefined;
}

export function normalizeId(service: string, settingsId: string): string {
  return `${service}:${settingsId}`;
}

export function serviceTreeId(service: string): string {
  return `service:${service}`;
}

export function escapeHtml(value: unknown): string {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll("\"", "&quot;");
}

export function parseJsonValue(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

function jsonValueText(value: unknown): string {
  try {
    return JSON.stringify(value) ?? "undefined";
  } catch {
    return String(value);
  }
}

export function jsonValuesEqual(a: unknown, b: unknown): boolean {
  return Object.is(a, b) || jsonValueText(a) === jsonValueText(b);
}

export function pathInsideRoot(root: string, candidate: string): boolean {
  const relativePath = path.relative(normalizeFilesystemPathKey(root), normalizeFilesystemPathKey(candidate));
  return relativePath === "" || (!relativePath.startsWith("..") && !path.isAbsolute(relativePath));
}

export function projectGraphOwnsPath(config: ExplorerConfig, candidate: string): boolean {
  if (pathInsideRoot(config.projectRoot, candidate)) {
    return true;
  }
  return loadProjectSourceLocations(config.projectRoot)
    .some((location) => pathInsideRoot(location, candidate) || pathInsideRoot(candidate, location));
}

function runCli(command: string, args: string[], cwd: string): Promise<CommandRunResult> {
  return new Promise((resolve, reject) => {
    const { child } = spawnTrackedProcess(command, args, cwd);
    let stdout = "";
    let stderr = "";
    let settled = false;
    const appendBounded = (current: string, chunk: string): string => {
      const combined = current + chunk;
      return combined.length > 8_000_000 ? combined.slice(-8_000_000) : combined;
    };
    const timer = setTimeout(() => {
      void terminateProcess(child).finally(() => {
        if (!settled) {
          settled = true;
          reject(new Error("Renium Explorer command timed out."));
        }
      });
    }, 60_000);
    child.stdout.on("data", (data: Buffer | string) => {
      stdout = appendBounded(stdout, data.toString());
    });
    child.stderr.on("data", (data: Buffer | string) => {
      stderr = appendBounded(stderr, data.toString());
    });
    child.on("error", (error) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        reject(error);
      }
    });
    child.on("close", (code) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve({ code: code ?? 130, stdout, stderr });
      }
    });
  });
}

export async function runJsonCli<T>(config: ExplorerConfig, args: string[]): Promise<T> {
  if (!fs.existsSync(config.cliPath)) {
    throw new Error(`${CLI_BINARY} was not found at ${config.cliPath}. Build the CLI or point the "renium.cliPath" setting at it.`);
  }
  const result = await runCli(config.cliPath, args, config.projectRoot);
  if (result.code !== 0) {
    throw new Error((result.stderr || result.stdout || `Renium reported an error (code ${result.code}).`).trim());
  }
  return JSON.parse(result.stdout.trim()) as T;
}

export async function runBytecodeBatchOne<T extends object>(
  config: ExplorerConfig,
  settingsFile: string,
  service: string,
  op: CliBatchOp,
): Promise<T & { service: string; settingsFile: string }> {
  const inputArgs = fs.existsSync(path.join(config.projectRoot, "renium.project.jsonc"))
    || fs.existsSync(path.join(config.projectRoot, "renium.project.json"))
    ? ["--project-root", config.projectRoot]
    : ["-f", settingsFile];
  const dump = await runJsonCli<CliBatchDump<T>>(config, [
    "bytecode-explorer-batch",
    ...inputArgs,
    "-s",
    service,
    "-o",
    "detail",
    "-j",
    JSON.stringify({ ops: [op] }),
  ]);
  const result = dump.results[0];
  if (!result) {
    throw new Error("Bytecode batch returned no results.");
  }
  return {
    service: dump.service,
    settingsFile: dump.settingsFile,
    ...result,
  };
}
